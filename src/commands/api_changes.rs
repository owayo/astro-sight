use anyhow::Result;
use std::collections::HashSet;

use crate::engine::parser;
use crate::models::cochange::CoChangeOptions;
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MissingCochange,
    MovedSymbol, PropertyToFieldChange,
};
use crate::service::AppService;

use super::dead_code::{
    collect_python_unittest_classes, enclosing_container, is_phpunit_test_symbol,
    is_python_test_symbol, is_test_path,
};
use super::git_input::validate_git_revision;

pub(crate) type ExportedSymbols = Vec<(String, String, String)>;
type ExportedSymbolsWithLang = (crate::language::LangId, ExportedSymbols);

/// 依存マニフェストとロックファイルの既知ペア。
/// これらは `cargo update` や `npm install` など片側のみが変更される正規操作が頻繁に発生するため、
/// missing_cochange 警告から除外する。同一ディレクトリに属するペアのみ除外対象とする（monorepo 配慮）。
pub(crate) const DEPENDENCY_MANIFEST_LOCK_PAIRS: &[(&str, &str)] = &[
    ("Cargo.toml", "Cargo.lock"),
    ("package.json", "package-lock.json"),
    ("package.json", "pnpm-lock.yaml"),
    ("package.json", "yarn.lock"),
    ("pyproject.toml", "uv.lock"),
    ("pyproject.toml", "poetry.lock"),
    ("pyproject.toml", "pdm.lock"),
    ("Gemfile", "Gemfile.lock"),
    ("composer.json", "composer.lock"),
    ("go.mod", "go.sum"),
    ("mix.exs", "mix.lock"),
];

/// 2 つのパスが既知の依存マニフェスト/ロックペアであれば true を返す。
/// monorepo 誤判定を避けるため、親ディレクトリが一致する場合のみ真。
pub(crate) fn is_dependency_manifest_pair(file_a: &str, file_b: &str) -> bool {
    let path_a = std::path::Path::new(file_a);
    let path_b = std::path::Path::new(file_b);
    let (Some(base_a), Some(base_b)) = (
        path_a.file_name().and_then(|s| s.to_str()),
        path_b.file_name().and_then(|s| s.to_str()),
    ) else {
        return false;
    };
    if path_a.parent() != path_b.parent() {
        return false;
    }
    DEPENDENCY_MANIFEST_LOCK_PAIRS
        .iter()
        .any(|(a, b)| (base_a == *a && base_b == *b) || (base_a == *b && base_b == *a))
}

pub(crate) fn detect_missing_cochanges(
    service: &AppService,
    dir: &str,
    changed_files: &HashSet<String>,
    min_confidence: f64,
    base: Option<&str>,
) -> Result<Vec<MissingCochange>> {
    // review では blame モードで cochange を解析する。
    // 起点ファイル = 差分に登場したファイル。
    // ただし起点が無い (差分が空) ときは何もせず空を返す。
    let source_files: Vec<String> = changed_files.iter().cloned().collect();
    if source_files.is_empty() {
        return Ok(Vec::new());
    }
    // review の差分取得で使った base を blame 解析にも渡し、複数コミット範囲の
    // review でも同じ変更範囲を対象にする。base 解決失敗や git 不在は engine 側で
    // 空集合を返すので最終的に Vec::new() に落ちる。
    let opts = CoChangeOptions {
        source_files,
        base: base.map(str::to_string),
        min_confidence,
        ..CoChangeOptions::default()
    };
    let cochange_result = match service.analyze_cochange(dir, &opts) {
        Ok(r) => r,
        Err(err) => {
            // 入力検証エラー (min_confidence の NaN / 範囲外等) はユーザーへ伝播する。
            // git 不在 / base 解決失敗は engine 側で empty 結果を返すため、ここまで
            // Err が来ない。InvalidRequest だけ早期失敗させて silent な誤動作を防ぐ。
            if let Some(astro_err) = err.downcast_ref::<crate::error::AstroError>()
                && astro_err.code == crate::error::ErrorCode::InvalidRequest
            {
                return Err(err);
            }
            return Ok(Vec::new());
        }
    };

    // 各 missing file につき最も confidence が高いペアのみ残す
    let mut best: std::collections::HashMap<String, MissingCochange> =
        std::collections::HashMap::new();
    for entry in &cochange_result.entries {
        // 依存マニフェスト/ロックペアは片側変更が正規操作として頻発するためスキップ
        if is_dependency_manifest_pair(&entry.file_a, &entry.file_b) {
            continue;
        }

        let a_in_diff = changed_files.contains(&entry.file_a);
        let b_in_diff = changed_files.contains(&entry.file_b);

        let candidate = if a_in_diff && !b_in_diff {
            Some(MissingCochange {
                file: entry.file_b.clone(),
                expected_with: entry.file_a.clone(),
                confidence: entry.confidence,
            })
        } else if b_in_diff && !a_in_diff {
            Some(MissingCochange {
                file: entry.file_a.clone(),
                expected_with: entry.file_b.clone(),
                confidence: entry.confidence,
            })
        } else {
            None
        };

        if let Some(c) = candidate {
            best.entry(c.file.clone())
                .and_modify(|existing| {
                    if c.confidence > existing.confidence {
                        *existing = c.clone();
                    }
                })
                .or_insert(c);
        }
    }

    // confidence 降順でソートし最大10件に制限
    let mut missing: Vec<MissingCochange> = best.into_values().collect();
    missing.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    missing.truncate(10);
    Ok(missing)
}

/// 内部用: reconcile のために signature を保持する一時構造。
#[derive(Debug, Clone)]
pub(crate) struct ApiSymbolCandidate {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) file: String,
    pub(crate) signature: String,
}

impl ApiSymbolCandidate {
    fn into_api_symbol(self) -> ApiSymbol {
        ApiSymbol {
            name: self.name,
            kind: self.kind,
            file: self.file,
        }
    }
}

/// 通常の modified ファイル (old_path / new_path がどちらも `/dev/null` でない) で、
/// added (新規シンボル) / removed (削除シンボル) / modified (シグネチャ変更) を分類する。
///
/// 全ての pub/exported シンボルの組み合わせを評価し、Rust private module 抑制 /
/// bin-only crate 抑制 / 内部参照 / 同一 diff 内 closed-in-diff / TS optional destructure 等
/// の各種抑制ルールを適用する。最も複雑な処理パス。
///
/// `old_syms` / `new_syms` / `in_file_callees` は `detect_api_changes` の Phase 0 で
/// 抽出済みのものを受け取る (rename 差分では base 側に新パスが存在しないため、旧版は
/// old_path 由来)。cross-file 参照判定は事前構築済みの `ref_index` を参照する。
#[allow(clippy::too_many_arguments)]
fn process_modified_file(
    df: &crate::models::impact::DiffFile,
    dir: &str,
    base: &str,
    diff_files: &[crate::models::impact::DiffFile],
    diff_new_paths: &HashSet<String>,
    old_syms: &[(String, String, String)],
    new_syms: &[(String, String, String)],
    in_file_callees: &std::collections::HashSet<String>,
    new_export_surface_names: &std::collections::HashSet<String>,
    ref_index: &ApiRefIndex,
    rust_reexport_cache: &mut RustBaseReexportCache,
    rust_new_reexport_cache: &mut RustWorktreeReexportCache,
    closure_caches: &mut crate::commands::api_changes::ref_index::ApiClosureCaches,
    buckets: &mut ApiChangeBuckets,
) {
    let old_map: std::collections::HashMap<&str, &str> = old_syms
        .iter()
        .map(|(name, _kind, sig)| (name.as_str(), sig.as_str()))
        .collect();
    let new_map: std::collections::HashMap<&str, (&str, &str)> = new_syms
        .iter()
        .map(|(name, kind, sig)| (name.as_str(), (kind.as_str(), sig.as_str())))
        .collect();

    // 同名シンボルが旧/新いずれかに複数存在する場合、HashMap<name, sig> は最後の 1 件しか
    // 保持できず、別のオーバーロードや誤パースされた定義同士を突き合わせて api.mod を
    // 誤検出する。出現回数を数え、複数あるシンボルは曖昧として modified 判定から除外する
    // (Issue #13: C++ overload / マクロ誤パースの api.mod 誤検出対策)。
    let mut old_name_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for (name, _, _) in old_syms {
        *old_name_counts.entry(name.as_str()).or_default() += 1;
    }
    let mut new_name_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for (name, _, _) in new_syms {
        *new_name_counts.entry(name.as_str()).or_default() += 1;
    }

    let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path);

    // rename 検出用: 同ファイル内に新規追加された全シンボル名を追跡する。
    let mut new_symbols_in_current_file: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for (name, kind, sig) in new_syms {
        if !old_map.contains_key(name.as_str()) {
            new_symbols_in_current_file.insert(name.clone());
            let candidate = ApiSymbolCandidate {
                name: name.clone(),
                kind: kind.clone(),
                file: df.new_path.clone(),
                signature: sig.clone(),
            };
            buckets.all_new_candidates.push(candidate.clone());
            if is_binary_rust_crate {
                continue;
            }
            if is_rust_new_symbol_outside_public_api_surface(
                dir,
                &df.new_path,
                name,
                rust_new_reexport_cache,
            ) {
                continue;
            }
            if is_internally_connected(in_file_callees, name) {
                continue;
            }
            if is_used_in_diff_paths(ref_index, dir, name, &df.new_path, diff_new_paths) {
                continue;
            }
            buckets.added.push(candidate);
        }
    }

    // Bash スクリプトでは関数定義は `export -f` (または `declare -fx`/`declare -xf`) で
    // 明示しない限りサブプロセスへ波及しない。
    let is_bash_old_file = is_bash_script_path(&df.old_path);
    // TS/JS: 新ツリーで export clause (`export { name } from "..."` / `import ...;
    // export { name };`) により name が公開され続けているシンボルは、利用者から見た
    // API 面が維持されているため api.rm から除外する。
    // `new_export_surface_names` は Phase 0 の単一 parse で先取り済み (perf #2)。
    for (name, kind, sig) in old_syms {
        if !new_map.contains_key(name.as_str()) {
            if is_rust_old_symbol_outside_public_api_surface(
                dir,
                base,
                &df.old_path,
                name,
                rust_reexport_cache,
            ) {
                continue;
            }
            if new_export_surface_names.contains(name.as_str()) {
                continue;
            }
            // closed-in-diff for api.rm: 同ファイルに新規追加されたシンボルがあり、削除された
            // シンボルが変更後ツリーで 0 件参照なら「rename + 実装置換」と判断して api.rm から
            // 除外する。
            let bash_pure_removal_skip = is_bash_old_file
                && new_symbols_in_current_file.is_empty()
                && !bash_function_is_exported_in_git(dir, base, &df.old_path, name);
            if (!new_symbols_in_current_file.is_empty() || bash_pure_removal_skip)
                && is_removed_symbol_unreferenced(ref_index, name)
            {
                continue;
            }
            // Python の @property → dataclass field 置き換えなら removed 扱いせず
            // property_to_field に振り替える。
            if let Some(target_file) =
                detect_python_property_to_field(dir, &df.old_path, name, diff_new_paths)
            {
                buckets.property_to_field.push(PropertyToFieldChange {
                    name: name.clone(),
                    file: target_file,
                });
                continue;
            }
            buckets.removed.push(ApiSymbolCandidate {
                name: name.clone(),
                kind: kind.clone(),
                file: df.old_path.clone(),
                signature: sig.clone(),
            });
        }
    }

    // Rust bin-only crate 判定 (api.mod 抑制用)。lib → bin / bin → lib どちらかが bin-only なら
    // 外部 API 面の変更ではないとみなす。
    let is_binary_rust_old_crate_for_mod =
        rust_reexport_cache.is_binary_only_at_base(dir, base, &df.old_path);
    let is_binary_rust_new_crate_for_mod = is_binary_only_rust_crate(dir, &df.new_path);
    let skip_mod_for_binary_crate =
        is_binary_rust_old_crate_for_mod || is_binary_rust_new_crate_for_mod;

    // 値バインディングの value-only 変更を const_value_changes へ振り分けるための言語判定。
    let lang_id_for_file =
        crate::language::LangId::from_path(camino::Utf8Path::new(df.new_path.as_str())).ok();

    // 同一 (file, qualname) の modified を重複排除するためのキーセット
    let mut seen_modified: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for (name, kind, new_sig) in new_syms {
        if let Some(old_sig) = old_map.get(name.as_str())
            && old_sig != &new_sig.as_str()
            && seen_modified.insert((df.new_path.clone(), name.clone()))
        {
            if skip_mod_for_binary_crate {
                continue;
            }
            // private module 抑制
            if is_rust_old_symbol_outside_public_api_surface(
                dir,
                base,
                &df.old_path,
                name,
                rust_reexport_cache,
            ) {
                continue;
            }
            // 同名が旧/新いずれかに複数あるシンボルは曖昧として modified から除外
            if old_name_counts.get(name.as_str()).copied().unwrap_or(0) > 1
                || new_name_counts.get(name.as_str()).copied().unwrap_or(0) > 1
            {
                continue;
            }
            // closed-in-diff: 同一ファイル内でしか呼ばれていない関数のシグネチャ変更は除外
            if is_internally_connected(in_file_callees, name)
                && !has_cross_file_refs(ref_index, &df.new_path, name)
            {
                continue;
            }

            // TS/TSX で「引数なし `()` → 省略可能 destructured 引数」追加は後方互換
            if is_ts_no_arg_to_optional_destructured_compatible(
                old_sig,
                new_sig,
                dir,
                base,
                &df.old_path,
                &df.new_path,
                name,
            ) {
                continue;
            }

            let change = ApiSymbolChange {
                name: name.clone(),
                kind: kind.clone(),
                file: df.new_path.clone(),
                old_signature: Some(old_sig.to_string()),
                new_signature: Some(new_sig.clone()),
            };
            classify_signature_change(
                change,
                kind,
                old_sig,
                new_sig,
                dir,
                base,
                df,
                diff_files,
                lang_id_for_file,
                ref_index,
                closure_caches,
                buckets,
            );
        }
    }
}

/// シグネチャ変更を const_value / compatible_modified (React HOC / Object member 削除) /
/// modified_closed_in_diff / modified のいずれかに分類する。
#[allow(clippy::too_many_arguments)]
fn classify_signature_change(
    change: ApiSymbolChange,
    kind: &str,
    old_sig: &str,
    new_sig: &str,
    dir: &str,
    base: &str,
    df: &crate::models::impact::DiffFile,
    diff_files: &[crate::models::impact::DiffFile],
    lang_id_for_file: Option<crate::language::LangId>,
    ref_index: &ApiRefIndex,
    closure_caches: &mut crate::commands::api_changes::ref_index::ApiClosureCaches,
    buckets: &mut ApiChangeBuckets,
) {
    let name = change.name.clone();
    // const / 非 mut static / export const の値 (initializer) のみ変更は const_value_changes へ
    if lang_id_for_file.is_some_and(|lid| is_const_value_only_change(old_sig, new_sig, kind, lid)) {
        buckets.const_value_changes.push(change);
        return;
    }
    // React component を memo / forwardRef 等の HOC でラップしただけは compatible_modified
    if let Some(compat) = detect_react_wrapper_compatible_mod(
        ref_index,
        dir,
        base,
        &df.old_path,
        &df.new_path,
        &name,
        kind,
        old_sig,
        new_sig,
        lang_id_for_file,
    ) {
        buckets.compatible_modified.push(compat);
        return;
    }
    // exported object の未参照プロパティ削除も compatible_modified
    if let Some(compat) = detect_object_members_compatible_mod(
        dir,
        base,
        &df.old_path,
        &df.new_path,
        &name,
        kind,
        old_sig,
        new_sig,
        lang_id_for_file,
    ) {
        buckets.compatible_modified.push(compat);
        return;
    }
    // TS/TSX の関数末尾へ optional/default 引数を追加しただけなら、既存呼び出しの required
    // arity は変わらないため compatible_modified として扱う。
    if let Some(compat) = detect_trailing_optional_params_compatible_mod(
        dir,
        base,
        &df.old_path,
        &df.new_path,
        &name,
        kind,
        old_sig,
        new_sig,
        lang_id_for_file,
    ) {
        buckets.compatible_modified.push(compat);
        return;
    }
    // Python のトップレベル関数 / モジュール直下クラスのメソッドへ末尾 optional/default
    // 引数 (`*` 後の kwonly+default 含む) を追加しただけなら、既存呼び出しが壊れないため
    // compatible_modified として扱う。
    if let Some(compat) = detect_python_trailing_optional_params_compatible_mod(
        dir,
        base,
        &df.old_path,
        &df.new_path,
        &name,
        kind,
        old_sig,
        new_sig,
        lang_id_for_file,
    ) {
        buckets.compatible_modified.push(compat);
        return;
    }
    // 全 cross-file 参照が同一 diff 内で追随済みなら informational
    if is_modified_closed_in_diff(
        ref_index,
        dir,
        &name,
        kind,
        base,
        &df.new_path,
        diff_files,
        closure_caches,
    ) {
        buckets.modified_closed_in_diff.push(change);
    } else {
        buckets.modified.push(change);
    }
}

/// Python の root-level スクリプト (package 外の単体スクリプト、例: `build_font.py`) の削除/変更
/// シンボルが公開 API 面外かを判定する (codex 設計合意の厳格版 A3、Issue
/// 2026-06-14-python-script-move-api-rm)。
///
/// 以下を全て満たすとき script-local = true:
/// - `old_path` が `.py`、path 区切りを含まない (リポジトリルート直下)、`__init__.py` でない
/// - モジュール名 (stem) が新ツリーの Python ファイルから参照 (import) されていない
/// - base の pyproject.toml の `[project.scripts]` / `[project.gui-scripts]` が当モジュールを
///   entrypoint に指定していない
///
/// 直接実行されるスクリプトの公開面は「ファイルを実行できること」であって内部 helper の
/// signature ではないため、これらの削除/シグネチャ変更を api.rm / api.mod にしない。package
/// module (サブディレクトリ配下) は対象外 = 従来どおり API 扱い (false negative 回避)。判定は
/// file 単位で 1 度行えば足りる (find_references の全走査は root-level .py 削除時のみ走る)。
fn is_python_root_script_local_file(dir: &str, base: &str, old_path: &str) -> bool {
    let path = std::path::Path::new(old_path);
    if path.extension().and_then(|e| e.to_str()) != Some("py") {
        return false;
    }
    // root-level のみ (package module は API 扱いで安全側に倒す)
    if old_path.contains('/') {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    if stem == "__init__" {
        return false;
    }
    // base の pyproject が当モジュールを script entrypoint にしている、または pyproject が
    // 解析不能なら公開面ありとして扱い script-local 判定を止める (keep, fail-closed)。
    if base_pyproject_marks_module_as_api(dir, base, stem) {
        return false;
    }
    // 新ツリーで stem が参照 (import) されていなければ script-local
    !python_module_referenced_in_tree(dir, stem)
}

/// 新ツリーの Python ファイルで `stem` (モジュール名) が identifier として参照されているか。
/// `import build_font` / `build_font.foo()` 等を 1 件でも見つければ true。判定不能 (検索失敗) は
/// true (= 参照あり扱いで api.rm を残す fail-closed)。
fn python_module_referenced_in_tree(dir: &str, stem: &str) -> bool {
    match crate::engine::refs::find_references(stem, std::path::Path::new(dir), Some("**/*.py")) {
        Ok(refs) => !refs.is_empty(),
        Err(_) => true,
    }
}

/// base の pyproject.toml が `stem` モジュールを script entrypoint に宣言しているか、または
/// pyproject が存在するが解析不能なら `true` (= 公開 API/CLI 面ありとして script-local 判定を
/// 止める fail-closed)。pyproject が存在しなければ `false` (script 宣言なし → 参照判定へ進む)。
///
/// 対応形式:
/// - PEP 621 `[project.scripts]` / `[project.gui-scripts]` (`name = "module:func"`)
/// - Poetry `[tool.poetry.scripts]` (string 形式 `name = "module:func"`)
///
/// fail-closed: pyproject 解析失敗、または script 値が string で取れない (Poetry 拡張テーブル
/// 形式など) 場合は `true` を返し、real な CLI/API 面を誤って隠さない。
fn base_pyproject_marks_module_as_api(dir: &str, base: &str, stem: &str) -> bool {
    let Some(content) = git_show_base_file(dir, base, "pyproject.toml") else {
        return false;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&content) else {
        return true; // 存在するが解析不能 → fail-closed (API 扱い)
    };
    let script_tables = [
        value.get("project").and_then(|p| p.get("scripts")),
        value.get("project").and_then(|p| p.get("gui-scripts")),
        value
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("scripts")),
    ];
    for table in script_tables.into_iter().flatten() {
        let Some(table) = table.as_table() else {
            // script セクションが存在するが table でない (schema 不正) → 解析不能なので
            // fail-closed (API 扱い)。codex 指摘: ここを continue にすると安全側から漏れる。
            return true;
        };
        for v in table.values() {
            let Some(target) = v.as_str() else {
                // string で取れない値 (Poetry 拡張テーブル形式など) は解析不能 → fail-closed。
                return true;
            };
            let module = target.split(':').next().unwrap_or("");
            if module == stem || module.split('.').next() == Some(stem) {
                return true;
            }
        }
    }
    false
}

/// `git show <base>:<rel>` でファイル内容を取得する。失敗時は None。
fn git_show_base_file(dir: &str, base: &str, rel: &str) -> Option<String> {
    validate_git_revision(base, "--base").ok()?;
    let output = std::process::Command::new("git")
        .args(["show", &format!("{base}:{rel}")])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// 削除ファイル (`new_path == "/dev/null"`) 由来の exported シンボルを `removed` /
/// `property_to_field` に分類する。
///
/// Rust private module / bin-only crate / Bash の未 export 関数 / Python `@property` →
/// dataclass field 置き換え / Python root-level スクリプトの helper は `removed` から除外する。
fn process_deleted_file(
    df: &crate::models::impact::DiffFile,
    dir: &str,
    base: &str,
    diff_new_paths: &HashSet<String>,
    rust_reexport_cache: &mut RustBaseReexportCache,
    buckets: &mut ApiChangeBuckets,
) {
    // base が source branch HEAD と同一の場合、`git show base:old_path` は削除済みで
    // 失敗し None になる。その場合は --diff-file が保持している旧ソース
    // (deleted_old_source) から AST を組み立てて exported シンボルを抽出する。
    let old_syms_opt = extract_exported_symbols_from_git(dir, base, &df.old_path).or_else(|| {
        df.deleted_old_source
            .as_deref()
            .and_then(|src| extract_exported_symbols_from_source(&df.old_path, src))
    });
    let Some(old_syms) = old_syms_opt else {
        return;
    };
    // Python の root-level スクリプト (package 外の単体スクリプト) の top-level helper は公開
    // API 面外なので api.rm にしない (A3、Issue 2026-06-14-python-script-move-api-rm)。file 単位の
    // 判定なので全シンボルをまとめて除外する。
    if is_python_root_script_local_file(dir, base, &df.old_path) {
        return;
    }
    let is_bash_old_file = is_bash_script_path(&df.old_path);
    for (name, kind, sig) in &old_syms {
        if is_rust_old_symbol_outside_public_api_surface(
            dir,
            base,
            &df.old_path,
            name,
            rust_reexport_cache,
        ) {
            continue;
        }
        if is_bash_old_file
            && !bash_function_is_exported_in_git(dir, base, &df.old_path, name)
            && is_removed_bash_symbol_unreferenced(dir, name)
        {
            continue;
        }
        // Python の @property → dataclass field 置き換えなら removed 扱いせず
        // property_to_field に振り替える。
        if let Some(target_file) =
            detect_python_property_to_field(dir, &df.old_path, name, diff_new_paths)
        {
            buckets.property_to_field.push(PropertyToFieldChange {
                name: name.clone(),
                file: target_file,
            });
            continue;
        }
        buckets.removed.push(ApiSymbolCandidate {
            name: name.clone(),
            kind: kind.clone(),
            file: df.old_path.clone(),
            signature: sig.clone(),
        });
    }
}

/// 新規ファイル (`old_path == "/dev/null"`) 由来の exported シンボルを `added` /
/// `all_new_candidates` に分類する。
///
/// bin-only crate (`src/lib.rs` なし) / private module / 内部のみ参照 / 同一 diff 内で
/// 完結したシンボルは `added` から除外する。
///
/// `new_syms` / `in_file_callees` は `detect_api_changes` の Phase 0 で抽出済みのものを
/// 受け取り、cross-file 参照判定は事前構築済みの `ref_index` を参照する。
#[allow(clippy::too_many_arguments)]
fn process_added_file(
    df: &crate::models::impact::DiffFile,
    dir: &str,
    diff_new_paths: &HashSet<String>,
    new_syms: &[(String, String, String)],
    in_file_callees: &std::collections::HashSet<String>,
    ref_index: &ApiRefIndex,
    rust_new_reexport_cache: &mut RustWorktreeReexportCache,
    buckets: &mut ApiChangeBuckets,
) {
    let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path);
    for (name, kind, sig) in new_syms {
        let candidate = ApiSymbolCandidate {
            name: name.clone(),
            kind: kind.clone(),
            file: df.new_path.clone(),
            signature: sig.clone(),
        };
        buckets.all_new_candidates.push(candidate.clone());
        if is_binary_rust_crate {
            continue;
        }
        if is_rust_new_symbol_outside_public_api_surface(
            dir,
            &df.new_path,
            name,
            rust_new_reexport_cache,
        ) {
            continue;
        }
        if is_internally_connected(in_file_callees, name) {
            continue;
        }
        if is_used_in_diff_paths(ref_index, dir, name, &df.new_path, diff_new_paths) {
            continue;
        }
        buckets.added.push(candidate);
    }
}

/// API 差分検出から除外すべき diff ファイルかを判定する。
///
/// - 信頼境界外のトラバーサルパスを `dir.join()` で読まないよう、絶対パスや `..` を
///   含むパスは拒否する。
/// - `.gitattributes` の `linguist-generated` 指定ファイルは検出対象外。
/// - ファイル先頭の自動生成マーカーコメントが付くファイルも対象外。
fn should_skip_diff_file(
    df: &crate::models::impact::DiffFile,
    gitattrs: &crate::engine::gitattributes::GitAttributes,
    canonical_dir: Option<&std::path::Path>,
) -> bool {
    if df.new_path != "/dev/null" && !crate::engine::impact::is_safe_diff_path(&df.new_path) {
        return true;
    }
    if df.old_path != "/dev/null" && !crate::engine::impact::is_safe_diff_path(&df.old_path) {
        return true;
    }
    if gitattrs.is_generated(&df.new_path) || gitattrs.is_generated(&df.old_path) {
        return true;
    }
    if let Some(root) = canonical_dir
        && df.new_path != "/dev/null"
    {
        let full = root.join(&df.new_path);
        // symlink 経由で workspace 外を指すファイルを on-disk read しないよう、
        // canonicalize して root 配下にあることを fail-closed で確認する。
        // is_safe_diff_path は文字列レベルの check のみで、`evil.rs ->
        // /etc/passwd` のような symlink を検出できないため、実ファイルを読む直前に
        // canonical 境界判定を入れる (canonicalize 失敗時も skip 側へ倒す)。
        match std::fs::canonicalize(&full) {
            Ok(canonical) if canonical.starts_with(root) => {}
            _ => return true,
        }
        if crate::engine::generated::is_auto_generated(&full) {
            return true;
        }
    }
    false
}

/// `detect_api_changes` の各バケット。`reconcile_with_moves` /
/// `partition_removed_dead_candidates` で最終分類する前の中間状態。
#[derive(Default)]
pub(crate) struct ApiChangeBuckets {
    pub(crate) added: Vec<ApiSymbolCandidate>,
    pub(crate) removed: Vec<ApiSymbolCandidate>,
    pub(crate) modified: Vec<ApiSymbolChange>,
    pub(crate) modified_closed_in_diff: Vec<ApiSymbolChange>,
    pub(crate) const_value_changes: Vec<ApiSymbolChange>,
    pub(crate) compatible_modified: Vec<CompatibleApiModification>,
    pub(crate) all_new_candidates: Vec<ApiSymbolCandidate>,
    pub(crate) property_to_field: Vec<PropertyToFieldChange>,
}

/// Phase 0 で抽出した diff ファイルごとの exported シンボル。
/// 抽出は git show / parse を伴うため 1 回だけ行い、name 収集と process_* で共有する。
enum PreparedDiffFile {
    /// `should_skip_diff_file` で除外されたファイル。
    Skip,
    /// 新規ファイル (`old_path == "/dev/null"`)。
    Added {
        new_syms: Option<Vec<(String, String, String)>>,
        in_file_callees: std::collections::HashSet<String>,
    },
    /// 削除ファイル (`new_path == "/dev/null"`)。cross-file 参照判定を行わないため
    /// 抽出は従来どおり `process_deleted_file` 内で行う。
    Deleted,
    /// 通常の modified ファイル。
    Modified {
        old_syms: Option<Vec<(String, String, String)>>,
        new_syms: Option<Vec<(String, String, String)>>,
        in_file_callees: std::collections::HashSet<String>,
        /// new_path の export clause / `pub use` が公開する名前集合 (TS/JS/Rust)。
        /// Phase 0 の単一 parse で先取りし、process_modified_file が再 read+parse
        /// せず使う (perf #2)。
        new_export_surface_names: std::collections::HashSet<String>,
    },
}

/// modified ファイルで cross-file 参照判定の対象になりうる name を `index_names` へ集める。
/// - 新規追加 (new のみ): `is_used_in_diff_paths` が bare name で検索
/// - 削除 (old のみ): `is_removed_symbol_unreferenced` が bare name で検索
/// - シグネチャ変更: `has_cross_file_refs` が exact name、`is_modified_closed_in_diff` /
///   `has_blocking_value_usage` が bare name で検索
///
/// per-symbol 実装の短絡で検索に到達しえない name は収集しない (過剰収集は AC trie の
/// パターンと事前フィルタのヒット (= parse 対象ファイル) を増やし、小規模 diff で
/// per-symbol より遅くなる):
/// - 新規追加: `is_internally_connected` が true なら `is_used_in_diff_paths` 未到達
/// - シグネチャ変更: exact name (`has_cross_file_refs`) は
///   `is_internally_connected && !has_cross_file_refs` の短絡により internally connected
///   のときのみ評価される
/// - 削除: `(!new_symbols_in_current_file.is_empty() || bash_pure_removal_skip)` が
///   成立しなければ `is_removed_symbol_unreferenced` 未到達 (bash の git show 判定は
///   再現せず `is_bash_old_file` で過剰側に倒す)
///
/// bin-only crate / private module / 同名複数等の残りの篩いは再現せず過剰側に倒す。
/// 過剰収集は検索コストが増えるだけで判定結果には影響しない (逆に収集漏れは
/// `refs_for` が `None` を返して保守側に倒れ、判定が変わりうる)。
fn collect_modified_file_index_names(
    old_syms: &[(String, String, String)],
    new_syms: &[(String, String, String)],
    in_file_callees: &std::collections::HashSet<String>,
    is_bash_old_file: bool,
    index_names: &mut HashSet<String>,
) {
    let old_map: std::collections::HashMap<&str, &str> = old_syms
        .iter()
        .map(|(name, _kind, sig)| (name.as_str(), sig.as_str()))
        .collect();
    let new_names: HashSet<&str> = new_syms.iter().map(|(name, _, _)| name.as_str()).collect();
    let mut has_new_only_symbol = false;
    for (name, _kind, sig) in new_syms {
        match old_map.get(name.as_str()) {
            None => {
                has_new_only_symbol = true;
                if !is_internally_connected(in_file_callees, name) {
                    index_names.insert(bare_name(name).to_string());
                }
            }
            Some(old_sig) if old_sig != &sig.as_str() => {
                // has_cross_file_refs も bare 名で照合するため exact 名 (qualname) の
                // 収集は不要 (identifier ノードに一致せず常に 0 件で AC trie を太らせるだけ)。
                index_names.insert(bare_name(name).to_string());
            }
            Some(_) => {}
        }
    }
    if !(has_new_only_symbol || is_bash_old_file) {
        return;
    }
    for (name, _, _) in old_syms {
        if !new_names.contains(name.as_str()) {
            index_names.insert(bare_name(name).to_string());
        }
    }
}

pub(crate) fn detect_api_changes(
    dir: &str,
    base: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> ApiChanges {
    let mut buckets = ApiChangeBuckets::default();
    // api.rm の Rust private module 抑制で base 側 re-export index を base+crate 単位に再利用する。
    let mut rust_reexport_cache = RustBaseReexportCache::default();
    // api.add 経路では new (working tree) 側 crate を 1 度走査して edge graph を構築する。
    let mut rust_new_reexport_cache = RustWorktreeReexportCache::default();

    // .gitattributes の linguist-generated 指定ファイルは API 変更検出から除外する
    let gitattrs = std::fs::canonicalize(dir)
        .map(|d| crate::engine::gitattributes::GitAttributes::load(&d))
        .unwrap_or_default();

    // 同一 diff 内で追加/変更されたファイルパスの集合。新規 pub シンボルが diff 内の
    // 別ファイルから参照されていれば「同一 diff 内で完結して使用されている」と判断し、
    // api.add から除外する（binary crate の pub struct が同 diff 内で use されるケース等）。
    let diff_new_paths: HashSet<String> = diff_files
        .iter()
        .filter(|f| f.new_path != "/dev/null")
        .map(|f| f.new_path.clone())
        .collect();

    let canonical_dir = std::fs::canonicalize(dir).ok();

    // Phase 0: added / modified ファイルの exported シンボルを抽出し、cross-file 参照
    // 判定の対象になりうる name を集めて ApiRefIndex を構築する。候補シンボルごとの
    // 全リポジトリ走査 (O(候補数 × 全ファイル)) を chunk 単位の batch 検索に集約する。
    let mut prepared: Vec<PreparedDiffFile> = Vec::with_capacity(diff_files.len());
    let mut index_names: HashSet<String> = HashSet::new();
    for df in diff_files {
        if should_skip_diff_file(df, &gitattrs, canonical_dir.as_deref()) {
            prepared.push(PreparedDiffFile::Skip);
            continue;
        }
        if df.old_path == "/dev/null" {
            // new_path を 1 回 read+parse して exported / callees を導出 (perf #2)。
            let facts = extract_new_file_facts(dir, &df.new_path);
            let new_syms = facts.exported;
            let in_file_callees = facts.callees;
            if let Some(syms) = &new_syms {
                for (name, _, _) in syms {
                    // per-symbol 実装と同じ短絡: ファイル内から呼ばれるシンボルは
                    // `is_used_in_diff_paths` に到達しないため検索対象に入れない。
                    if !is_internally_connected(&in_file_callees, name) {
                        index_names.insert(bare_name(name).to_string());
                    }
                }
            }
            prepared.push(PreparedDiffFile::Added {
                new_syms,
                in_file_callees,
            });
            continue;
        }
        if df.new_path == "/dev/null" {
            prepared.push(PreparedDiffFile::Deleted);
            continue;
        }
        // rename 差分では base 側に新パスが存在しないため、旧版は old_path から読む。
        let old_syms = extract_exported_symbols_from_git(dir, base, &df.old_path);
        // new_path を 1 回 read+parse して exported / callees / export surface を導出 (perf #2)。
        // export surface は process_modified_file が再 parse せず使えるよう
        // PreparedDiffFile に持たせる。
        let facts = extract_new_file_facts(dir, &df.new_path);
        let new_syms = facts.exported;
        let in_file_callees = facts.callees;
        let new_export_surface_names = facts.export_surface_names;
        if let (Some(old), Some(new)) = (&old_syms, &new_syms) {
            collect_modified_file_index_names(
                old,
                new,
                &in_file_callees,
                is_bash_script_path(&df.old_path),
                &mut index_names,
            );
        }
        prepared.push(PreparedDiffFile::Modified {
            old_syms,
            new_syms,
            in_file_callees,
            new_export_surface_names,
        });
    }
    let ref_index = ApiRefIndex::build(dir, &index_names);

    // process_modified_file → classify_signature_change → is_modified_closed_in_diff の per-file
    // キャッシュ (import 行集合 / 変更行集合) を detect_api_changes スコープで 1 度確保し、
    // 全 modified シンボル横断で共有する。per-symbol の git diff 起動 + tree-sitter parse を
    // unique file 単位に削減 (#perf N+1 改善)。
    let mut closure_caches = crate::commands::api_changes::ref_index::ApiClosureCaches::default();

    for (df, prep) in diff_files.iter().zip(&prepared) {
        match prep {
            PreparedDiffFile::Skip => {}
            PreparedDiffFile::Added {
                new_syms,
                in_file_callees,
            } => {
                if let Some(new_syms) = new_syms {
                    process_added_file(
                        df,
                        dir,
                        &diff_new_paths,
                        new_syms,
                        in_file_callees,
                        &ref_index,
                        &mut rust_new_reexport_cache,
                        &mut buckets,
                    );
                }
            }
            PreparedDiffFile::Deleted => {
                process_deleted_file(
                    df,
                    dir,
                    base,
                    &diff_new_paths,
                    &mut rust_reexport_cache,
                    &mut buckets,
                );
            }
            PreparedDiffFile::Modified {
                old_syms,
                new_syms,
                in_file_callees,
                new_export_surface_names,
            } => {
                if let (Some(old_syms), Some(new_syms)) = (old_syms, new_syms) {
                    process_modified_file(
                        df,
                        dir,
                        base,
                        diff_files,
                        &diff_new_paths,
                        old_syms,
                        new_syms,
                        in_file_callees,
                        new_export_surface_names,
                        &ref_index,
                        &mut rust_reexport_cache,
                        &mut rust_new_reexport_cache,
                        &mut closure_caches,
                        &mut buckets,
                    );
                }
            }
        }
    }

    // git の rename detection が効かない diff (外部供給 / 非 git 入力 / 設定で無効化された
    // 環境など) に対するフォールバックとして、同一 (name, kind, signature) の add/rm ペアを
    // rename または move として相殺し、`moved` カテゴリに移す。`all_new_candidates` には
    // `is_used_in_diff_paths` 等で `added` から外れた候補も含まれるため、module → package
    // 化のように新規ファイル側のシンボルが同 diff 内の `__init__.py` 等から参照されて
    // `added` に乗らないケースでも `removed` を相殺できる。
    let (added, removed, moved) =
        reconcile_with_moves(buckets.added, buckets.removed, buckets.all_new_candidates);

    // removed のうち HEAD ツリーで他ファイル参照 0 件のものを `removed_dead` に振り分け。
    // 「base 時点で dead だった symbol の整理」だけでなく「base alive → HEAD で関連
    // caller も削除」も同 diff 内で repo 内到達性 0 になるため同一カテゴリに含む。
    // 順序は moved > removed_dead (rename/move 相殺を先に行わないと移動が dead 誤分類
    // される)。codex 設計合意 (Issue
    // 2026-05-28-meet-virtual-you-gemini-multi-select 対応)。
    //
    // qualname (`Container.method`) は refs 検索が identifier ノードでマッチするため
    // 常に 0 件返却となり誤分類するため、bare name で検索する。同名 def が複数残って
    // いる場合は「部分的削除」or「同名複数定義」の可能性があるため保守的に removed
    // に残す (codex 指摘 1 対応)。
    //
    // 複数候補がある場合、`find_references_batch` で 1 度のリポジトリ走査に集約する
    // (codex 指摘 3 対応: 候補数 × リポ全体走査の回避)。
    let (removed_kept, removed_dead) = partition_removed_dead_candidates(dir, removed);

    ApiChanges {
        added: added.into_iter().map(|c| c.into_api_symbol()).collect(),
        removed: removed_kept
            .into_iter()
            .map(|c| c.into_api_symbol())
            .collect(),
        modified: buckets.modified,
        moved,
        property_to_field: buckets.property_to_field,
        removed_dead: removed_dead
            .into_iter()
            .map(|c| c.into_api_symbol())
            .collect(),
        modified_closed_in_diff: buckets.modified_closed_in_diff,
        const_value_changes: buckets.const_value_changes,
        compatible_modified: buckets.compatible_modified,
    }
}

/// src 相対パスを Rust モジュールセグメント列に変換する。
/// `meeting/macos.rs` → `[meeting, macos]`、`meeting/mod.rs` → `[meeting]`、
/// `lib.rs` / `main.rs` → `[]` (root モジュール)。
pub(crate) fn module_path_segments(rel: &std::path::Path) -> Vec<String> {
    let comps: Vec<_> = rel.components().collect();
    let mut segs: Vec<String> = Vec::new();
    let last = comps.len().saturating_sub(1);
    for (i, c) in comps.iter().enumerate() {
        let name = c.as_os_str().to_string_lossy();
        if i == last {
            let stem = std::path::Path::new(name.as_ref())
                .file_stem()
                .map(|s| s.to_string_lossy().to_string());
            match stem.as_deref() {
                // mod.rs / lib.rs / main.rs はそのディレクトリのモジュール自身を表す
                Some("mod") | Some("lib") | Some("main") => {}
                Some(s) => segs.push(s.to_string()),
                None => {}
            }
        } else {
            segs.push(name.to_string());
        }
    }
    segs
}

/// 親モジュールファイル直下の `mod <mod_name>` 宣言の可視性 (制限なし pub か) を返す。
///
/// source_file 直下の `mod_item` のみを見る。inline mod (`mod foo { mod bar; }`) 内の同名
/// 宣言は別モジュールスコープの宣言なので拾わない (codex 指摘: 再帰探索で別スコープの同名
/// mod を誤って拾うと可視性判定が壊れる)。
pub(crate) fn find_mod_decl_visibility(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    mod_name: &str,
) -> Option<bool> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "mod_item"
            && child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                == Some(mod_name)
        {
            // #[path = "..."] でファイル名と module 名がずれる場合、モジュール解決を諦めて
            // 「判定不能」(None) を返す。下流 (rust_private_module_info_at_base /
            // public_reachable_modules_at_base) は api.rm 抑制を諦め、削除を残す方向に倒す。
            if rust_mod_item_has_path_attribute(child, source) {
                return None;
            }
            let mut mc = child.walk();
            let is_pub = child.children(&mut mc).any(|c| {
                c.kind() == "visibility_modifier" && c.utf8_text(source).map(str::trim) == Ok("pub")
            });
            return Some(is_pub);
        }
    }
    None
}

/// 同名・同種別・同シグネチャの api.add / api.rm ペアを `moved` として相殺する。
///
/// `all_new_candidates` は `added` フィルタ適用前の新規側候補一覧（`added` の上位集合）。
/// `is_used_in_diff_paths` などで `added` から落ちた候補も `removed` との突き合わせに
/// 利用するため、別系統で渡す。
///
/// 戻り値:
/// - `kept_added`: `moved` で相殺されなかった追加シンボル
/// - `kept_removed`: `moved` で相殺されなかった削除シンボル
/// - `moved`: `from`/`to` のペアにまとめた移動シンボル
pub(crate) fn reconcile_with_moves(
    added: Vec<ApiSymbolCandidate>,
    removed: Vec<ApiSymbolCandidate>,
    all_new_candidates: Vec<ApiSymbolCandidate>,
) -> (
    Vec<ApiSymbolCandidate>,
    Vec<ApiSymbolCandidate>,
    Vec<MovedSymbol>,
) {
    use std::collections::HashMap;
    use std::collections::VecDeque;

    // 1) removed を (name, kind, signature) でバケット化。
    let mut removed_bucket: HashMap<(String, String, String), VecDeque<ApiSymbolCandidate>> =
        HashMap::new();
    for sym in removed {
        removed_bucket
            .entry((sym.name.clone(), sym.kind.clone(), sym.signature.clone()))
            .or_default()
            .push_back(sym);
    }

    // 2) 新規候補を順に走査して removed と突き合わせ、`moved` を組み立てる。
    //    同じ (name, kind, signature, file) の重複候補は最初の 1 件だけ扱う。
    //    (name, kind, signature) を共有する複数 add が同じ removed と組まないように、
    //    一度マッチした new 側は `matched_new_files` に記録しておき、後で `added` から
    //    除外する。
    let mut moved: Vec<MovedSymbol> = Vec::new();
    let mut seen_new_keys: std::collections::HashSet<(String, String, String, String)> =
        std::collections::HashSet::new();
    let mut matched_new_files: HashMap<
        (String, String, String),
        std::collections::HashSet<String>,
    > = HashMap::new();
    for new in &all_new_candidates {
        let dedup_key = (
            new.name.clone(),
            new.kind.clone(),
            new.signature.clone(),
            new.file.clone(),
        );
        if !seen_new_keys.insert(dedup_key) {
            continue;
        }
        let bucket_key = (new.name.clone(), new.kind.clone(), new.signature.clone());
        if let Some(bucket) = removed_bucket.get_mut(&bucket_key)
            && let Some(rm) = bucket.pop_front()
        {
            matched_new_files
                .entry(bucket_key)
                .or_default()
                .insert(new.file.clone());
            moved.push(MovedSymbol {
                name: rm.name,
                kind: rm.kind,
                from: rm.file,
                to: new.file.clone(),
            });
        }
    }

    // 3) `moved` で相殺された候補は `added` からも除外する。
    let kept_added: Vec<ApiSymbolCandidate> = added
        .into_iter()
        .filter(|a| {
            let key = (a.name.clone(), a.kind.clone(), a.signature.clone());
            !matched_new_files
                .get(&key)
                .map(|files| files.contains(&a.file))
                .unwrap_or(false)
        })
        .collect();

    // 4) ペア化されなかった `removed` を集める。
    let kept_removed: Vec<ApiSymbolCandidate> = removed_bucket
        .into_values()
        .flat_map(|bucket| bucket.into_iter())
        .collect();

    (kept_added, kept_removed, moved)
}

/// qualname (`Container.method`) から末尾セグメントのみを抜き出す。
/// `a.b.c` → `c`、`foo` → `foo`。
pub(crate) fn bare_name(qualname: &str) -> &str {
    qualname.rsplit('.').next().unwrap_or(qualname)
}

/// ファイルリストからエクスポートシンボルを収集し、参照ゼロのシンボルを返す。
/// dead-code コマンドと review コマンドの共通コアロジック。
/// count_non_definition_refs_split で production / test 別に件数のみカウントし、
/// SymbolReference を確保しない。
pub(crate) fn extract_exported_symbols_from_git(
    dir: &str,
    base: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
    // テストファイル配下のシンボルは API 差分検出の対象外。
    // (api.rm の base 側比較もテストファイルからは行わない)
    if is_test_path(std::path::Path::new(file_path)) {
        return Some(Vec::new());
    }
    // `base` と `file_path` はオプション誤認識を避けるため先頭が `-` のものを拒否する
    validate_git_revision(base, "--base").ok()?;
    validate_git_revision(file_path, "diff file path").ok()?;
    let output = std::process::Command::new("git")
        .args(["show", &format!("{base}:{file_path}")])
        .current_dir(dir)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    extract_exported_symbols_from_source(file_path, &output.stdout)
}

/// 与えられた旧側ソースから export シンボル一覧を抽出する。
///
/// `extract_exported_symbols_from_git` のフォールバックとして、`--diff-file` の削除 hunk から
/// 復元した旧ソースを直接渡す経路で使う。test path 判定とフィルタは git 経路と同一。
pub(crate) fn extract_exported_symbols_from_source(
    file_path: &str,
    source: &[u8],
) -> Option<ExportedSymbols> {
    if is_test_path(std::path::Path::new(file_path)) {
        return Some(Vec::new());
    }
    let utf8_path = camino::Utf8Path::new(file_path);
    let lang_id = parser::detect_lang(utf8_path, source).ok()?;
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();

    let syms = crate::engine::symbols::extract_symbols(root, source, lang_id).ok()?;
    // Rust の `impl Trait for Type` 配下のメソッドは trait の実装事実であり、独立した
    // 公開 API item ではない。module 移動など実体は維持したままの変更でも api.add / api.rm
    // に誤計上されるのを避けるため、API 変更検出でも trait impl メソッドを除外する。
    // 旧側を読む経路は API 変更検出 (api.rm 比較) のみで使われる。
    // dead-code は最新コミット側だけを見るため framework entrypoint の除外は不要。
    Some(filter_exported_symbols(
        &syms,
        root,
        source,
        lang_id,
        true,
        false,
        Some(file_path),
    ))
}

#[cfg(test)]
pub(crate) fn extract_exported_symbols_from_file_inner(
    dir: &str,
    file_path: &str,
    exclude_trait_impls: bool,
    exclude_framework_entrypoints: bool,
) -> Option<ExportedSymbols> {
    extract_exported_symbols_from_file_inner_with_lang(
        dir,
        file_path,
        exclude_trait_impls,
        exclude_framework_entrypoints,
    )
    .map(|(_, syms)| syms)
}

pub(crate) fn extract_exported_symbols_from_file_inner_with_lang(
    dir: &str,
    file_path: &str,
    exclude_trait_impls: bool,
    exclude_framework_entrypoints: bool,
) -> Option<ExportedSymbolsWithLang> {
    // diff から得た file_path は信頼境界外。`../etc/passwd` 等のトラバーサルや絶対パスを
    // 拒否し、workspace 外のファイルを誤って読まないようにする。
    if !crate::engine::impact::is_safe_diff_path(file_path) {
        return None;
    }
    let full_path = std::path::Path::new(dir).join(file_path);
    let utf8_path = camino::Utf8Path::new(full_path.to_str()?);
    let source = parser::read_file(utf8_path).ok()?;
    let lang_id = parser::detect_lang(utf8_path, &source).ok()?;

    // lexer-only 言語 (現状 Xojo) は tree-sitter を持たないため、lexer 経由で
    // export 相当のシンボルを抽出する。
    if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        return Some((
            lang_id,
            crate::engine::lexer::extract_exported_symbols(
                &source,
                lexer_lang,
                exclude_framework_entrypoints,
            ),
        ));
    }

    let tree = parser::parse_source(&source, lang_id).ok()?;
    let root = tree.root_node();

    let syms = crate::engine::symbols::extract_symbols(root, &source, lang_id).ok()?;
    Some((
        lang_id,
        filter_exported_symbols(
            &syms,
            root,
            &source,
            lang_id,
            exclude_trait_impls,
            exclude_framework_entrypoints,
            Some(file_path),
        ),
    ))
}

/// new_path 1 ファイルから API 差分検出に必要な 3 種の facts をまとめて抽出する。
/// 旧 exported / in_file_callees / export surface の各抽出が同一 new_path をそれぞれ
/// read+parse していた (TS/JS/Rust で 3 回、他言語で 2 回) のを **1 回の read+parse に
/// 集約**する (perf #2)。
/// 各抽出のガードは元関数と完全に一致させ behavior-preserving とする:
/// - `exported`: test path は `Some(空)` (parse せず) / 非 test は `is_safe_diff_path` 必須で
///   None と空を区別 / lexer-only は lexer 経由 / それ以外 tree-sitter parse+filter
/// - `callees`: test/safe ガードなし、read+parse 失敗時は空 (lexer-only は parse_source が Err→空)
/// - `export_surface_names`: `is_safe_diff_path` かつ TS/TSX/JS/Rust のみ、それ以外は空。
///   TS/JS は named export clause (from 句の有無を問わず)、Rust は `pub use` が公開する名前
pub(crate) struct NewFileFacts {
    pub(crate) exported: Option<Vec<(String, String, String)>>,
    pub(crate) callees: std::collections::HashSet<String>,
    pub(crate) export_surface_names: std::collections::HashSet<String>,
}

pub(crate) fn extract_new_file_facts(dir: &str, file_path: &str) -> NewFileFacts {
    let mut facts = NewFileFacts {
        exported: None,
        callees: std::collections::HashSet::new(),
        export_surface_names: std::collections::HashSet::new(),
    };
    // exported の test path 短絡: parse せず Some(空) を返す (extract_exported_symbols_from_file と一致)。
    let is_test = is_test_path(std::path::Path::new(file_path));
    if is_test {
        facts.exported = Some(Vec::new());
    }
    // exported (非 test) / reexports は is_safe_diff_path を要求する。callees は要求しない。
    let safe = crate::engine::impact::is_safe_diff_path(file_path);

    let full_path = std::path::Path::new(dir).join(file_path);
    let Some(utf8) = full_path.to_str() else {
        return facts;
    };
    let utf8_path = camino::Utf8Path::new(utf8);
    let Ok(source) = parser::read_file(utf8_path) else {
        return facts;
    };
    let Ok(lang_id) = parser::detect_lang(utf8_path, &source) else {
        return facts;
    };

    // lexer-only (Xojo): tree-sitter parse は呼ばない (parse_source は Err を返すため callees/
    // reexports は元実装でも空)。exported のみ lexer 経由 (非 test かつ safe のとき)。
    if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        if !is_test && safe {
            facts.exported = Some(crate::engine::lexer::extract_exported_symbols(
                &source, lexer_lang, false,
            ));
        }
        return facts;
    }

    let Ok(tree) = parser::parse_source(&source, lang_id) else {
        return facts;
    };
    let root = tree.root_node();

    // callees: test/safe ガードなし (extract_in_file_callees と一致)。
    facts.callees =
        crate::engine::calls::extract_all_callees(root, &source, lang_id).unwrap_or_default();

    // exported (非 test かつ safe): extract_symbols → filter_exported_symbols。
    // extract_symbols 失敗は None のまま (元 _inner の `?` と一致)。
    if !is_test
        && safe
        && let Ok(syms) = crate::engine::symbols::extract_symbols(root, &source, lang_id)
    {
        facts.exported = Some(filter_exported_symbols(
            &syms,
            root,
            &source,
            lang_id,
            true,
            false,
            Some(file_path),
        ));
    }

    // export surface: safe かつ TS/TSX/JS/Rust のみ。
    if safe
        && matches!(
            lang_id,
            crate::language::LangId::Typescript
                | crate::language::LangId::Tsx
                | crate::language::LangId::Javascript
                | crate::language::LangId::Rust
        )
    {
        facts.export_surface_names = match lang_id {
            crate::language::LangId::Rust => {
                crate::engine::symbols::collect_rust_reexported_names(root, &source)
            }
            _ => crate::engine::symbols::collect_js_ts_named_export_surface_names(root, &source),
        };
    }

    facts
}

/// dead_symbols のうち、宣言行が今回の diff の追加行 (`+` 行) と重なるもののみを残す。
///
/// `--dead-scope touched-symbols` の実装。`review --hook` のデフォルトとして使われ、
/// 「changed file 内に元からあった dead」がレビューノイズとして毎回出る UX 問題を
/// 解消する。
///
/// 注意: `HunkInfo` の `new_start` / `new_count` は context 行も含むため
/// hunk 範囲全体を「touched」と扱うと既存 dead まで残してしまう。ここでは
/// `extract_changed_new_lines` で **実際に追加された行** だけを set 化して照合する。
pub(crate) fn extract_symbol_lines(
    dir: &str,
    file_path: &str,
) -> Option<std::collections::HashMap<String, usize>> {
    use std::collections::HashMap;
    let full = std::path::Path::new(dir).join(file_path);
    let utf8 = camino::Utf8Path::new(full.to_str()?);
    let source = parser::read_file(utf8).ok()?;
    let lang_id = parser::detect_lang(utf8, &source).ok()?;

    let symbols = if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        crate::engine::lexer::extract_symbols(&source, lexer_lang)
    } else {
        let tree = parser::parse_source(&source, lang_id).ok()?;
        crate::engine::symbols::extract_symbols(tree.root_node(), &source, lang_id).ok()?
    };

    let mut map = HashMap::new();
    for s in symbols {
        // 同名シンボルが複数ある場合、最初に出現した行を保持する。
        map.entry(s.name).or_insert(s.range.start.line);
    }
    Some(map)
}

/// シンボルの種類に応じた API シグネチャを抽出する。
/// 関数/メソッド → 宣言行、struct/enum/trait/interface/class → 宣言行のみ。
///
/// クラス/型は宣言行（`class Foo(Bar):` や `struct Foo {` など）のみをシグネチャとする。
/// 本体（メソッド本体や private フィールド）の変更でクラス全体の API 変更として
/// 再検出されるのを避けるため、メンバーの集約はしない。
/// メンバー個々の変更は method シンボル単独で検出される。
///
/// function / method の場合は tree-sitter ノードで「宣言開始から body 直前まで」を
/// 抽出し、whitespace を正規化して signature とする。これにより `where` 句や複数行
/// generics で先頭行が同一でも引数列が変わったケース (Issue
/// 2026-05-14-rename-and-multiline-signature) を検出できる。
/// 関数/メソッドノードの body 開始 byte を返す。tree-sitter の "body" フィールドを優先し、
/// 取得できない grammar (tree-sitter-kotlin 0.3.5 の `function_declaration` は
/// `fields: []` でフィールド名を持たず、body は `function_body` 型の直接子) では直接の
/// named child から既知の body ノード kind を fallback で探す。body を持たない宣言
/// (Swift protocol requirement / Rust trait fn / Kotlin abstract fun) では None を返し、
/// 呼び出し側が `end_byte()` (= 宣言全体 = 署名のみ) に倒す。
/// これを入れないと body フィールドを持たない言語で「関数全体」が署名になり、
/// body のみ変更が api.mod に誤検出される (Kotlin body-only 変更の false positive 対策)。
fn function_body_start_byte(node: tree_sitter::Node<'_>) -> Option<usize> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body.start_byte());
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| {
            matches!(
                child.kind(),
                "function_body" | "block" | "statement_block" | "compound_statement"
            )
        })
        .map(|child| child.start_byte())
}

/// body が無い (interface method / abstract 等) や node 取得失敗時は先頭行を fallback。
pub(crate) fn extract_api_signature(
    sym: &crate::models::symbol::Symbol,
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lines: &[&str],
    lang_id: crate::language::LangId,
) -> String {
    use crate::models::symbol::SymbolKind;
    if matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) {
        let start = tree_sitter::Point {
            row: sym.range.start.line,
            column: sym.range.start.column,
        };
        let end = tree_sitter::Point {
            row: sym.range.end.line,
            column: sym.range.end.column,
        };
        if let Some(node) = root.descendant_for_point_range(start, end) {
            let mut cur = node;
            loop {
                match cur.kind() {
                    "function_item"
                    | "function_declaration"
                    | "function_definition"
                    | "method_declaration"
                    | "method_definition"
                    | "function_signature_item"
                    // Swift protocol requirement (body なしの宣言)。複数行 requirement でも
                    // 先頭行 fallback でなく AST から signature 全体を抽出する (codex 指摘)。
                    | "protocol_function_declaration" => {
                        let s = cur.start_byte();
                        let e =
                            function_body_start_byte(cur).unwrap_or_else(|| cur.end_byte());
                        // TS/TSX の関数 destructured params (`function foo({ a, b }: T)`) は
                        // `{ ... }` 内の variable 列が変わっても呼び出し側契約 (`: T` 型注釈)
                        // に影響しないため、signature 比較から除外する。React の Props
                        // 拡張 (optional prop 追加 + destructure 受け取り追加) で api.mod に
                        // 出る false positive を防ぐ (Issue
                        // 2026-05-28-api-mod-optional-props-additive 対応)。
                        if matches!(
                            lang_id,
                            crate::language::LangId::Typescript | crate::language::LangId::Tsx
                        ) {
                            return normalize_typescript_destructure_signature(cur, source, s, e);
                        }
                        // Tauri command (`#[tauri::command]` / `#[command]`) の自動注入型引数
                        // (AppHandle / State / Window 等) は実行時に Tauri が注入し JS 側 invoke()
                        // の引数には現れないため、signature 比較から除外する
                        // (Issue 2026-05-29-swift-sidecar-api-mod パターンB)。
                        if lang_id == crate::language::LangId::Rust
                            && let Some(sig) =
                                normalize_rust_tauri_command_signature(cur, source, s, e)
                        {
                            return sig;
                        }
                        if let Some(bytes) = source.get(s..e) {
                            return normalize_signature_whitespace(bytes);
                        }
                        break;
                    }
                    _ => {}
                }
                match cur.parent() {
                    Some(p) => cur = p,
                    None => break,
                }
            }
        }
    }

    // フォールバック: 先頭行のみ
    lines
        .get(sym.range.start.line)
        .unwrap_or(&"")
        .trim()
        .to_string()
}

/// 値バインディング (const / static / export const) の宣言から抽出した shape 情報。
/// initializer (= 右辺) を除いた宣言の骨格と、value-only 変更を安全に判定するための補助情報。
pub(crate) struct BindingShape {
    /// initializer を除いた正規化済み宣言テキスト (名前・型・visibility・binding kind を含む)。
    shape: String,
    /// 不変バインディング (Rust `const` / 非 mut `static`、TS/JS `const`) なら true。
    /// mutable (`static mut` / `let` / `var`) は false。
    is_const_binding: bool,
    /// 型注釈を持つなら true (TS の型注釈なし initializer の安全判定に使う)。
    has_type_annotation: bool,
    /// initializer が scalar literal (数値 / 文字列 / 真偽値 / null 等) なら true。
    /// 関数 / object / array / call 等の複雑な式は false。
    initializer_is_scalar: bool,
}

/// `node` を起点に、指定 kind のいずれかに最初に一致する子孫ノードを深さ優先で探す。
/// signature 文字列は単一宣言なので export_statement 等のラップを潜るために使う。
pub(crate) fn find_first_descendant_of_kinds<'a>(
    node: tree_sitter::Node<'a>,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    if kinds.contains(&node.kind()) {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_first_descendant_of_kinds(child, kinds) {
            return Some(found);
        }
    }
    None
}

/// value 手前で切った宣言テキストを正規化する。末尾に残る `=` と前後・連続空白を畳む。
pub(crate) fn normalize_binding_shape_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let trimmed = s.trim_end();
    // value の直前で切ると末尾に `= ` が残るため取り除く。
    let without_eq = trimmed
        .strip_suffix('=')
        .map(str::trim_end)
        .unwrap_or(trimmed);
    without_eq.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// signature 文字列を AST パースし、値バインディングなら initializer を除いた shape を返す。
/// 対象外 (関数 / 型 / バインディング以外) や抽出失敗時は None を返し、呼び出し側は保守的に
/// 従来どおり api.mod へ倒す (codex 設計合意: テキストの `=` 分割ではなく AST ベース)。
pub(crate) fn extract_binding_shape(
    sig: &str,
    lang_id: crate::language::LangId,
) -> Option<BindingShape> {
    // lexer-only 言語は tree-sitter を持たないため対象外。
    if lang_id.is_lexer_only() {
        return None;
    }
    let source = sig.as_bytes();
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();
    match lang_id {
        crate::language::LangId::Rust => {
            let decl = find_first_descendant_of_kinds(root, &["const_item", "static_item"])?;
            extract_rust_binding_shape(decl, source)
        }
        crate::language::LangId::Typescript
        | crate::language::LangId::Tsx
        | crate::language::LangId::Javascript => {
            let decl = find_first_descendant_of_kinds(
                root,
                &["lexical_declaration", "variable_declaration"],
            )?;
            extract_js_binding_shape(decl, source)
        }
        _ => None,
    }
}

/// Rust の const_item / static_item から shape を抽出する。
pub(crate) fn extract_rust_binding_shape(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<BindingShape> {
    // static mut は mutable_specifier を子に持つ。const は常に不変。
    let mut cursor = node.walk();
    let is_mut = node
        .children(&mut cursor)
        .any(|c| c.kind() == "mutable_specifier");
    let value = node.child_by_field_name("value");
    let has_type_annotation = node.child_by_field_name("type").is_some();
    let shape_end = value
        .map(|v| v.start_byte())
        .unwrap_or_else(|| node.end_byte());
    let shape_bytes = source.get(node.start_byte()..shape_end)?;
    let initializer_is_scalar = value.map(rust_value_is_scalar).unwrap_or(false);
    Some(BindingShape {
        shape: normalize_binding_shape_text(shape_bytes),
        is_const_binding: !is_mut,
        has_type_annotation,
        initializer_is_scalar,
    })
}

/// TS/JS の lexical_declaration / variable_declaration から shape を抽出する。
pub(crate) fn extract_js_binding_shape(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<BindingShape> {
    // binding kind (`const` / `let` / `var`) を最初の anonymous child から判定する。
    let mut decl_cursor = node.walk();
    let binding_kw = node
        .children(&mut decl_cursor)
        .find(|c| matches!(c.kind(), "const" | "let" | "var"))
        .map(|c| c.kind());
    let is_const_binding = binding_kw == Some("const");

    // 複数 declarator (`const a = 1, b = 2;`) は shape 抽出が壊れるため対象外。
    let mut declarators = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            declarators.push(child);
        }
    }
    if declarators.len() != 1 {
        return None;
    }
    let declarator = declarators[0];
    let value = declarator.child_by_field_name("value");
    let has_type_annotation = declarator.child_by_field_name("type").is_some();

    // visibility (export) を shape に含めるため、親が export_statement なら起点を遡る。
    let shape_start = match node.parent() {
        Some(p) if p.kind() == "export_statement" => p.start_byte(),
        _ => node.start_byte(),
    };
    let shape_end = value
        .map(|v| v.start_byte())
        .unwrap_or_else(|| declarator.end_byte());
    let shape_bytes = source.get(shape_start..shape_end)?;
    let initializer_is_scalar = value.map(js_value_is_scalar).unwrap_or(false);
    Some(BindingShape {
        shape: normalize_binding_shape_text(shape_bytes),
        is_const_binding,
        has_type_annotation,
        initializer_is_scalar,
    })
}

/// Rust の値ノードが scalar literal かを判定する (型注釈なし経路の安全弁、誤検出側に倒す)。
pub(crate) fn rust_value_is_scalar(value: tree_sitter::Node<'_>) -> bool {
    matches!(
        value.kind(),
        "integer_literal"
            | "float_literal"
            | "string_literal"
            | "raw_string_literal"
            | "char_literal"
            | "boolean_literal"
    )
}

/// JS/TS の値ノードが scalar literal かを判定する。関数 / object / array / call は false。
pub(crate) fn js_value_is_scalar(value: tree_sitter::Node<'_>) -> bool {
    matches!(
        value.kind(),
        "number" | "string" | "true" | "false" | "null" | "undefined"
    )
}

/// old/new signature が「const / 非 mut static / export const の値のみ変更 (shape 不変)」かを
/// 判定する。true なら api.mod ではなく const_value_changes (informational) に振り分ける。
///
/// gate: (1) kind が value binding (constant/variable)、(2) 言語が Rust/TS/TSX/JS、
/// (3) 両者が不変バインディング、(4) shape 一致、(5) TS で型注釈なしなら両者 scalar literal。
/// いずれか外れる / 抽出失敗時は false を返し、保守的に api.mod へ倒す。
pub(crate) fn is_const_value_only_change(
    old_sig: &str,
    new_sig: &str,
    kind: &str,
    lang_id: crate::language::LangId,
) -> bool {
    // 値バインディングの kind のみ (Rust const/static="constant"、TS/JS const="variable")。
    if !matches!(kind, "constant" | "variable") {
        return false;
    }
    if !matches!(
        lang_id,
        crate::language::LangId::Rust
            | crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
            | crate::language::LangId::Javascript
    ) {
        return false;
    }
    let (Some(old), Some(new)) = (
        extract_binding_shape(old_sig, lang_id),
        extract_binding_shape(new_sig, lang_id),
    ) else {
        return false;
    };
    // mutable バインディング (static mut / let / var) は demote しない。
    if !old.is_const_binding || !new.is_const_binding {
        return false;
    }
    // shape (名前・型・visibility・binding kind) が変われば破壊的変更の可能性 → api.mod。
    if old.shape != new.shape {
        return false;
    }
    // TS/JS で型注釈がない場合、関数 / object / array / call initializer は shape 推定が
    // 危険なため scalar literal 同士のときだけ demote する (codex 指摘)。
    if matches!(
        lang_id,
        crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
            | crate::language::LangId::Javascript
    ) {
        let both_typed = old.has_type_annotation && new.has_type_annotation;
        let both_scalar = old.initializer_is_scalar && new.initializer_is_scalar;
        if !both_typed && !both_scalar {
            return false;
        }
    }
    true
}

/// Tauri command の自動注入型 (実行時に Tauri が注入し JS-facing な invoke() 引数に現れない型)。
/// `Channel<T>` は JS 側から渡す引数なので含めない (signature 差分の対象に残す)。
pub(crate) const TAURI_INJECTED_TYPES: &[&str] = &[
    "AppHandle",
    "Window",
    "Webview",
    "WebviewWindow",
    "State",
    "Request",
    "CommandScope",
    "GlobalScope",
];

/// Rust の型ノードから base 名 (パス・参照・ジェネリクスを剥がした末尾型名) を取り出す。
pub(crate) fn rust_type_base_name(ty: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    match ty.kind() {
        "type_identifier" => ty.utf8_text(source).ok().map(str::to_string),
        // tauri::AppHandle → name 子 'AppHandle'
        "scoped_type_identifier" => ty
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .map(str::to_string),
        // State<'_, T> → base 'State'
        "generic_type" => ty
            .child_by_field_name("type")
            .and_then(|t| rust_type_base_name(t, source)),
        // &State<...> / &AppHandle → 内側の型
        "reference_type" => ty
            .child_by_field_name("type")
            .and_then(|t| rust_type_base_name(t, source)),
        _ => None,
    }
}

/// function_item が Tauri command 属性 (`#[tauri::command]` / `#[command]`) を持つか判定する。
/// Rust では属性は function_item の前方兄弟 (attribute_item) に並ぶ。
pub(crate) fn rust_fn_has_tauri_command_attr(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
) -> bool {
    let mut sib = fn_node.prev_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "attribute_item" => {
                if let Ok(text) = s.utf8_text(source) {
                    let inner = text
                        .trim_start_matches("#[")
                        .trim_start_matches("#![")
                        .trim_end_matches(']')
                        .trim();
                    if inner == "tauri::command"
                        || inner.starts_with("tauri::command(")
                        || inner == "command"
                        || inner.starts_with("command(")
                    {
                        return true;
                    }
                }
            }
            // 属性とコメントは読み飛ばし、それ以外に到達したら属性列の終端
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sib = s.prev_sibling();
    }
    false
}

/// Tauri command 関数の signature から自動注入型引数を除外して返す。
/// Tauri command でなければ None を返し、呼び出し側で通常の signature 抽出にフォールバックする。
pub(crate) fn normalize_rust_tauri_command_signature(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
    s: usize,
    e: usize,
) -> Option<String> {
    if !rust_fn_has_tauri_command_attr(fn_node, source) {
        return None;
    }
    let params = fn_node.child_by_field_name("parameters")?;
    let mut kept: Vec<String> = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "parameter" => {
                let injected = child
                    .child_by_field_name("type")
                    .and_then(|t| rust_type_base_name(t, source))
                    .is_some_and(|n| TAURI_INJECTED_TYPES.contains(&n.as_str()));
                if !injected && let Ok(t) = child.utf8_text(source) {
                    kept.push(t.to_string());
                }
            }
            "self_parameter" => {
                if let Ok(t) = child.utf8_text(source) {
                    kept.push(t.to_string());
                }
            }
            _ => {}
        }
    }
    let prefix = source.get(s..params.start_byte())?;
    let suffix = source.get(params.end_byte()..e)?;
    let rebuilt = format!(
        "{}({}){}",
        String::from_utf8_lossy(prefix),
        kept.join(", "),
        String::from_utf8_lossy(suffix)
    );
    Some(normalize_signature_whitespace(rebuilt.as_bytes()))
}

/// TS/TSX 関数の signature を抽出し、parameters 直下の `object_pattern`
/// (destructured params) を `{}` に正規化する。
///
/// `function foo({ a, b, c = 0 }: Props)` と `function foo({ a, b }: Props)` は
/// どちらも呼び出し側契約は `: Props` のみで、destructure 中身は内部 binding。
/// 正規化することで Props 拡張に伴う destructure 行の追加が api.mod に出ない。
///
/// 型注釈側の inline object type (`function foo({x}: {x: string, y: number})` の
/// `{x: string, y: number}`) は `type_annotation` 子なので置換対象外。
///
/// 「引数なし `()` から省略可能な destructured 引数追加」の互換性判定は、
/// signature 単独では行わない (型注釈変更だけ起きるケースを誤って互換扱いする
/// リスクがあるため)。両側 signature を見て判定するロジックは
/// [`is_ts_no_arg_to_optional_destructured_compatible`] が detect_api_changes
/// 経路で行う。
pub(crate) fn normalize_typescript_destructure_signature(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
    start_byte: usize,
    end_byte: usize,
) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    if let Some(params) = fn_node.child_by_field_name("parameters") {
        collect_parameter_object_pattern_ranges(params, &mut ranges);
    }
    if ranges.is_empty() {
        if let Some(bytes) = source.get(start_byte..end_byte) {
            return normalize_signature_whitespace(bytes);
        }
        return String::new();
    }
    ranges.sort_by_key(|r| r.0);

    let mut buf: Vec<u8> = Vec::with_capacity(end_byte - start_byte);
    let mut cursor = start_byte;
    for (op_start, op_end) in &ranges {
        if *op_start < cursor || *op_end > end_byte {
            continue;
        }
        if let Some(bytes) = source.get(cursor..*op_start) {
            buf.extend_from_slice(bytes);
        }
        buf.extend_from_slice(b"{}");
        cursor = *op_end;
    }
    if let Some(bytes) = source.get(cursor..end_byte) {
        buf.extend_from_slice(bytes);
    }
    normalize_signature_whitespace(&buf)
}

/// TS/TSX の formal_parameters 直下にある `object_pattern` のバイト範囲を集める。
///
/// パラメータの `type_annotation` (inline object type など) には踏み込まないため、
/// 型注釈側の object type は影響を受けない。required_parameter / optional_parameter の
/// `pattern` フィールドを直接見て object_pattern かを判定する。
pub(crate) fn collect_parameter_object_pattern_ranges(
    params: tree_sitter::Node<'_>,
    ranges: &mut Vec<(usize, usize)>,
) {
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        match child.kind() {
            "required_parameter" | "optional_parameter" => {
                if let Some(pattern) = child.child_by_field_name("pattern")
                    && pattern.kind() == "object_pattern"
                {
                    ranges.push((pattern.start_byte(), pattern.end_byte()));
                }
            }
            // 無型 JS スタイル: parameter ノードがなく object_pattern が直接子に来る
            // ケース。安全側に倒して同様に正規化する (TS/TSX に限定済み)。
            "object_pattern" => {
                ranges.push((child.start_byte(), child.end_byte()));
            }
            _ => {}
        }
    }
}

/// signature bytes を whitespace で分割して 1 つの space で結合し正規化する。
/// 改行・タブ・連続スペース・末尾の `{` 直前空白を一括で潰す。
pub(crate) fn normalize_signature_whitespace(bytes: &[u8]) -> String {
    std::str::from_utf8(bytes)
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn filter_exported_symbols(
    syms: &[crate::models::symbol::Symbol],
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lang_id: crate::language::LangId,
    exclude_trait_impls: bool,
    exclude_framework_entrypoints: bool,
    file_path: Option<&str>,
) -> Vec<(String, String, String)> {
    use crate::models::symbol::SymbolKind;
    let source_str = std::str::from_utf8(source).unwrap_or("");
    let lines: Vec<&str> = source_str.lines().collect();

    // 同名別メソッドを区別するための enclosing container (class/struct/trait/interface) を収集。
    // メソッド/関数の range が container の range に内包される場合、qualname として
    // `Container.method` を使う（最も内側の container を優先）。
    let containers: Vec<&crate::models::symbol::Symbol> = syms
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::Class
                    | SymbolKind::Struct
                    | SymbolKind::Trait
                    | SymbolKind::Interface
                    | SymbolKind::Enum
            )
        })
        .collect();

    // Python 限定: 同一ファイル内の `unittest.TestCase` 派生クラスを fixed-point で解決する。
    // dead-code 経路でのみ使う想定だが、`exclude_framework_entrypoints` が true の場合に
    // 集合を構築すれば十分。
    let unittest_classes =
        if exclude_framework_entrypoints && lang_id == crate::language::LangId::Python {
            collect_python_unittest_classes(syms, root, source, lang_id)
        } else {
            std::collections::HashSet::new()
        };

    let mut result = Vec::new();
    for sym in syms {
        // モジュール宣言 (`pub mod foo;`) はファイル構成の整理であり、
        // 公開 API 面としての意味は薄い。dead-code / api.add 両経路で除外する
        // (Rust `mod`, Python の module、他言語の同等表現)。
        if matches!(sym.kind, SymbolKind::Module) {
            continue;
        }
        if !crate::engine::symbols::is_symbol_exported(root, source, lang_id, &sym.range) {
            continue;
        }
        // pub(crate), pub(super) 等はクレート内部APIなので除外。
        // Rust 限定: 他言語では `pub` という名前の関数呼び出し (`export function pub(...)` 等) が
        // 宣言行に現れるだけで API 面から消えてしまう (Rust では `pub` は予約語のため識別子不可)。
        if lang_id == crate::language::LangId::Rust {
            let decl_line = lines.get(sym.range.start.line).unwrap_or(&"").trim();
            if decl_line.contains("pub(") {
                continue;
            }
        }
        // C/C++ で実関数 body 内にネストした function_definition は、tree-sitter-cpp が
        // マクロ呼び出し (BOOST_FOREACH 等) を関数定義と誤パースした結果であることが多い。
        // 本物のトップレベル関数 / クラスメソッドではないため dead-code / API 変更検出の
        // どちらでも exported シンボルから除外する
        // (Issue #13: api_changes.modified が差分外の BOOST_FOREACH を拾う誤検出対策)。
        if matches!(
            lang_id,
            crate::language::LangId::C | crate::language::LangId::Cpp
        ) && matches!(sym.kind, SymbolKind::Function | SymbolKind::Method)
            && crate::engine::symbols::is_cpp_nested_function(root, &sym.range)
        {
            continue;
        }
        // C/C++ の前方宣言・opaque tag (本体を持たない struct/class/enum) は「定義」ではなく
        // 宣言であり、dead-code (未使用定義検出) や API 変更の対象にすべきではない。
        // `typedef struct st_mysql MYSQL;` の st_mysql (外部ライブラリの不透明構造体タグ) を
        // dead 誤検出する問題への対応 (Issue #11)。
        if matches!(
            lang_id,
            crate::language::LangId::C | crate::language::LangId::Cpp
        ) && matches!(
            sym.kind,
            SymbolKind::Struct | SymbolKind::Class | SymbolKind::Enum
        ) && crate::engine::symbols::is_cpp_forward_declaration(root, &sym.range)
        {
            continue;
        }
        // Rust の `impl Trait for Type` 配下のメソッドは除外する。
        //   - dead-code 判定: trait dispatch 経由で呼ばれるため cross-file refs で caller を
        //     追跡できず、偽陽性になる。
        //   - API 変更検出: trait メソッドの実装は公開 item ではなく実装事実のため、個別の
        //     `on_ref` / `default` 等を api.add / api.rm にしない。必要であれば `impl Trait
        //     for Type` 単位で差分を扱うべきで、メソッド単位では扱わない。
        if exclude_trait_impls
            && lang_id == crate::language::LangId::Rust
            && crate::engine::symbols::is_trait_impl_method_rust(root, &sym.range)
        {
            continue;
        }
        // Kotlin/Java/Swift/TS/C# の `override` メソッドは親 interface/class の
        // メソッドを実装しているため、親型経由（Android の Listener callback 等）
        // で呼ばれる。cross-file refs では caller を追跡できず dead-code / api.add/rm
        // のいずれでも偽陽性になるため除外する。
        if exclude_trait_impls
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_override_method(root, source, lang_id, &sym.range)
        {
            continue;
        }
        // TS/JS の `constructor` メソッドは `new ClassName(...)` 構文で暗黙的に呼び出される。
        // 識別子レベルの cross-file refs では `constructor` 名を探しても見つからず、
        // クラスが利用されていても dead 判定される。クラス自体の dead 判定で十分なので、
        // constructor を独立した API/dead 候補から除外する。
        if matches!(sym.kind, SymbolKind::Method)
            && sym.name == "constructor"
            && matches!(
                lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
        {
            continue;
        }
        // PHPUnit 規約のテストメソッド / テストクラス。PHP 限定。
        // `public function testXxx`, `setUp`, `tearDown`, `setUpBeforeClass`,
        // `tearDownAfterClass`, および `*Test` / `*TestCase` / `*IntegrationTest` /
        // `*FeatureTest` クラスは PHPUnit のランナーから自動で呼ばれる規約的シンボルで、
        // 識別子レベルの cross-file ref は発生しないが dead でもない。
        if is_phpunit_test_symbol(&sym.name, sym.kind, lang_id) {
            continue;
        }
        // PHP 擬似 enum (Java enum 風 static factory) パターン。PHP 限定。
        // `public static function FOO(): self { return new self('FOO'); }` 形式は
        // Laravel / DDD 系の AbstractValueObject 系で大量に存在し、
        // migration の文字列リテラル / DB 列値 / annotation reflection 経由で
        // 利用されるが識別子レベルの cross-file refs では caller が追跡できない。
        // dead-code の framework_entrypoints 除外と同じ意味合いで除外する。
        if exclude_framework_entrypoints
            && lang_id == crate::language::LangId::Php
            && matches!(sym.kind, SymbolKind::Method)
            && crate::engine::symbols::is_php_pseudo_enum_method(
                root, source, &sym.range, &sym.name,
            )
        {
            continue;
        }
        // PHP の runtime annotation (`@TypeItem`, `@Route`, `@DataProvider`, `@dataProvider` 等) が
        // docstring に付いているメソッド / クラスは reflection 経由で動的に呼ばれるため
        // dead-code 候補から除外する。
        if exclude_framework_entrypoints
            && lang_id == crate::language::LangId::Php
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Class)
            && let Some(doc) = sym.doc.as_deref()
            && crate::engine::symbols::php_doc_has_runtime_annotation(doc)
        {
            continue;
        }
        // Python のフレームワーク登録デコレータ (Typer / Click / FastAPI / Flask /
        // pytest 等) で装飾された関数 / メソッド / クラスは、フレームワーク内部
        // レジストリ経由で呼び出されるため識別子レベルの cross-file refs では
        // caller を追跡できない。dead-code 判定では偽陽性源になるため除外する。
        if exclude_framework_entrypoints
            && lang_id == crate::language::LangId::Python
            && matches!(
                sym.kind,
                SymbolKind::Method | SymbolKind::Function | SymbolKind::Class
            )
            && crate::engine::symbols::has_framework_entrypoint_decorator_python(
                root, source, &sym.range,
            )
        {
            continue;
        }
        // JS/TS のフレームワーク DSL コールバック (WXT defineContentScript /
        // defineBackground、Vue defineComponent、Vite/Nuxt defineConfig 等) の
        // 引数オブジェクトメソッド (`main()`, `setup()` 等) は、フレームワーク内部
        // からビルド時連結で呼び出されるため識別子レベルの cross-file refs では
        // caller を追跡できない (Issue 2026-05-14-wxt-defineContentScript-main)。
        if exclude_framework_entrypoints
            && matches!(
                lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_js_ts_framework_dsl_callback(root, source, &sym.range)
        {
            continue;
        }
        // Angular DI provider option callback。例: RECAPTCHA_LOADER_OPTIONS の
        // `useValue: { onBeforeLoad() { ... } }` はライブラリ側から呼ばれるため、
        // TS 上の直接 caller が無くても dead ではない (GitLab #26)。
        if exclude_framework_entrypoints
            && matches!(
                lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_js_ts_angular_provider_option_callback(
                root, source, &sym.range,
            )
        {
            continue;
        }
        // Angular `@Component` / `@Directive` 装飾クラスの runtime entrypoint メンバー。
        // 以下の 3 系統を統合判定する (詳細は is_js_ts_angular_runtime_entrypoint):
        //   1. lifecycle hook メソッド (`ngOnInit` / `ngAfterViewChecked` 等、既存)
        //      Angular ランタイムが change detection サイクルで自動呼出するため静的 caller が無い。
        //      GitLab issue #8 対応。
        //   2. ControlValueAccessor 規約メソッド (`writeValue` / `registerOnChange` /
        //      `registerOnTouched` / `setDisabledState`)。`implements ControlValueAccessor` または
        //      decorator metadata 内の `NG_VALUE_ACCESSOR` provider をシグナルとして判定。
        //      Angular Forms が NG_VALUE_ACCESSOR provider 経由で ngModel/formControl バインド時に
        //      呼ぶ。GitLab issue #20 対応。
        //   3. member 単位の Angular decorator (`@HostListener` / `@HostBinding` / `@Input` /
        //      `@Output` / `@ViewChild` / `@ViewChildren` / `@ContentChild` / `@ContentChildren`)
        //      が付与された method/property。GitLab issue #23 対応。
        if exclude_framework_entrypoints
            && matches!(
                lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Field)
            && crate::engine::symbols::is_js_ts_angular_runtime_entrypoint(root, source, &sym.range)
        {
            continue;
        }
        // Laravel runtime entrypoint。PHP 限定。dead-code 経路のみ
        // (`exclude_framework_entrypoints=true`) で除外する。
        // 以下の 2 系統:
        //   1. Eloquent リレーション (`public function x(): BelongsTo` 等の戻り型)。`->with('x')`
        //      文字列リテラルや `$model->x` magic property 経由で Eloquent が呼ぶため、
        //      static caller 0 件でも dead ではない (GitLab issue #21)。
        //   2. Laravel framework が contract 経由で呼ぶ既知のメソッド名 (`getEmailForPasswordReset`
        //      / `sendPasswordResetNotification`)。enclosing class が `CanResetPassword(Contract)?`
        //      を `implements` する場合のみ対象 (GitLab issue #22)。
        // 文字列リテラル参照 (`with(['x'])`) / magic property 解決は静的解析の本質的限界のため
        // 別 issue としている (codex 設計判断)。
        //
        // API 差分経路 (`exclude_framework_entrypoints=false`) では除外しない。判定が戻り型
        // ベースのため、戻り型なし旧版 (`function x() {`) は残り戻り型付き新版 (`(): HasOne`)
        // だけ除外される非対称が起き、実在メソッドの返り型付与が api.rm に誤分類されていた
        // (GitLab issue #33)。公開メソッドである以上シグネチャ差分は api.mod として扱う。
        if exclude_framework_entrypoints
            && lang_id == crate::language::LangId::Php
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_php_laravel_runtime_entrypoint(root, source, &sym.range)
        {
            continue;
        }
        // Flyway の Java マイグレーションクラスとそのメンバ。Java 限定。
        // `extends BaseJavaMigration` / `implements JavaMigration` のクラスは Flyway が
        // クラスパス走査 + リフレクションで発見・実行するため、アプリコード上に直接参照が
        // 存在せず dead-code / API 変更検出の両方で false positive 源になる。クラス自体に
        // 加えて配下の `migrate(Context)` 等のメソッドも framework が反射で呼ぶため除外する
        // (sym.range から class_declaration 祖先まで遡って判定)。Java symbols 抽出では
        // クラスメソッドが `SymbolKind::Function` で返る (Method ではない) ため Function/Method
        // の両方を許容する。本判定は `exclude_framework_entrypoints` フラグに依存させず常に
        // 効かせる ― API 変更検出の new 側 (`extract_new_file_facts`) / old 側
        // (`extract_old_exported_symbols`) は flag=false で呼ばれるが、Flyway migration は
        // 公開 API 面ではない runtime entrypoint なので api.added / api.removed にも出さない
        // ため。GitLab issue #24 対応。
        if lang_id == crate::language::LangId::Java
            && matches!(
                sym.kind,
                SymbolKind::Class | SymbolKind::Method | SymbolKind::Function
            )
            && crate::engine::symbols::is_java_flyway_migration_class(root, source, &sym.range)
        {
            continue;
        }
        // unittest / pytest のテスト規約シンボル。Python 限定。
        // `class Foo(unittest.TestCase):` 派生クラスとそのメソッド (`test_*`,
        // `setUp` 等)、`test_*.py` / `*_test.py` のトップレベル `test_*` 関数、
        // `conftest.py` 内の関数はテストランナーから動的 discover されるため、
        // 識別子レベルの cross-file refs では caller を追跡できない。
        if exclude_framework_entrypoints
            && is_python_test_symbol(
                &sym.name,
                sym.kind,
                lang_id,
                file_path,
                sym.container.as_deref(),
                &unittest_classes,
            )
        {
            continue;
        }
        let sig = extract_api_signature(sym, root, source, &lines, lang_id);
        let qualname = if matches!(sym.kind, SymbolKind::Method | SymbolKind::Function) {
            enclosing_container(sym, &containers)
                .map(|c| format!("{}.{}", c.name, sym.name))
                .unwrap_or_else(|| sym.name.clone())
        } else {
            sym.name.clone()
        };
        // qualname ベースでも最終チェック (例: `Foo.testBar` を PHP で除外)
        if is_phpunit_test_symbol(&qualname, sym.kind, lang_id) {
            continue;
        }
        // qualname ベースでも Python unittest 規約をチェック (`Foo.test_bar` 等)
        if exclude_framework_entrypoints
            && is_python_test_symbol(
                &qualname,
                sym.kind,
                lang_id,
                file_path,
                sym.container.as_deref(),
                &unittest_classes,
            )
        {
            continue;
        }
        result.push((qualname, format!("{:?}", sym.kind).to_lowercase(), sig));
    }
    result
}

/// `qualname` (例: `Class.method` や bare name `foo`) が `callees` に含まれるかを判定する。
/// Python/Ruby など「obj.method()」形式で呼び出される言語では callee 側は bare name のみ
/// なので、qualname の末尾 (`.` 区切りの最後) でも判定する。
pub(crate) fn is_internally_connected(
    callees: &std::collections::HashSet<String>,
    qualname: &str,
) -> bool {
    if callees.contains(qualname) {
        return true;
    }
    if let Some(bare) = qualname.rsplit('.').next()
        && bare != qualname
        && callees.contains(bare)
    {
        return true;
    }
    false
}

mod js_ts_shadow;
mod python_signature;
mod ref_index;
mod rust_public;
mod ts_signature;

pub(crate) use python_signature::*;
pub(crate) use ref_index::*;
pub(crate) use rust_public::*;
pub(crate) use ts_signature::*;

/// 削除されたシンボル `name` が、変更後のツリー全体のどこからも参照されていないかを判定する。
/// 参照が 0 件であれば同一 diff 内で全 caller が追随済みと判断し、`api.rm` から除外する。
/// 参照検索に失敗した場合は保守的に false（外部参照ありとみなす）を返し、
/// レビュー対象として残す（false negative を起こさない方針）。
/// `removed` 候補のうち、HEAD ツリーで repo 内参照 0 件のものを `removed_dead` に
/// 振り分ける。残りは `removed` (破壊的削除) として返す。
///
/// 実装上の配慮:
/// - **qualname → bare name**: `Container.method` 形式は refs 検索の identifier
///   マッチでは常に 0 件になるため、`bare_name` で正規化して検索する
/// - **batch refs**: 候補ごとに `find_references` を呼ぶと「候補数 × リポ全体走査」と
///   なる。`find_references_batch` で 1 回の AC + ディレクトリ走査に集約
/// - **同名複数定義の保守扱い**: 削除後の HEAD で同名 def が 2 件以上残っていれば
///   「部分削除」「同名複数 export」など破壊的削除の可能性があるため、保守的に
///   `removed` に残す (false negative より false positive を優先)
/// - **検索失敗時の保守扱い**: batch refs が `Err` を返した場合、すべて `removed`
///   に残す (false negative 防止)
pub(crate) fn partition_removed_dead_candidates(
    dir: &str,
    candidates: Vec<ApiSymbolCandidate>,
) -> (Vec<ApiSymbolCandidate>, Vec<ApiSymbolCandidate>) {
    use crate::models::reference::RefKind;
    use std::collections::{HashMap, HashSet};

    if candidates.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // 候補から bare name を重複排除して集める
    let mut unique_bare: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for c in &candidates {
        let bare = bare_name(&c.name).to_string();
        if seen.insert(bare.clone()) {
            unique_bare.push(bare);
        }
    }

    let service = AppService::new();
    let batch_result = match service.find_references_batch(&unique_bare, dir, None) {
        Ok(r) => r,
        Err(_) => {
            // 検索失敗時は保守的にすべて removed に残す
            return (candidates, Vec::new());
        }
    };

    // 外部パッケージ (package.json deps) から import された同名 binding は、削除した
    // ローカルシンボルとは別物 (例: tailwindcss の `Config` 型) なので参照カウントから
    // 除外する。これがないと汎用名の削除が外部同名 import を拾って api.rm に誤分類される
    // (codex 設計合意。full TS resolver は入れず、証明できる外部 import binding のみ除外)。
    let external_pkgs = load_external_package_names(dir);
    // (path, symbol) -> (外部 import の local binding が symbol か, 外部 import 元名が symbol の行集合)
    let mut import_info_cache: HashMap<(String, String), (bool, HashSet<usize>)> = HashMap::new();

    // bare_name -> (def_count, ref_count)
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    for r in &batch_result {
        let mut def_count = 0usize;
        let mut ref_count = 0usize;
        for x in &r.references {
            if x.kind == Some(RefKind::Definition) {
                def_count += 1;
                continue;
            }
            let key = (x.path.clone(), r.symbol.clone());
            let (local_bound, source_name_lines) =
                import_info_cache.entry(key).or_insert_with(|| {
                    analyze_external_import_for_symbol(dir, &x.path, &r.symbol, &external_pkgs)
                });
            // 外部 import specifier の import 元名そのものの参照 (import 行) は別モジュールの
            // export 名なので数えない (`import { Config as X } from "pkg"` の `Config`)。
            if source_name_lines.contains(&x.line) {
                continue;
            }
            // 外部 import の local binding を持つファイルなら、その使用箇所も外部由来として
            // 数えない (`import { Config } from "pkg"` の local Config 利用)。
            if *local_bound {
                continue;
            }
            ref_count += 1;
        }
        counts.insert(r.symbol.clone(), (def_count, ref_count));
    }

    let mut removed_kept = Vec::new();
    let mut removed_dead = Vec::new();
    for c in candidates {
        let bare = bare_name(&c.name).to_string();
        let (def_count, ref_count) = counts.get(&bare).copied().unwrap_or((0, 0));
        // 同名定義が複数残っている → 保守的に removed に残す
        if def_count > 1 {
            removed_kept.push(c);
            continue;
        }
        if ref_count == 0 {
            removed_dead.push(c);
        } else {
            removed_kept.push(c);
        }
    }

    // 第 2 パス: `Owner.member` 形式の removed 候補は、同一 file の型 owner が新ツリーから
    // 完全に消滅している (定義 0 件 かつ 参照 0 件) 場合に限り member も追従して removed_dead
    // へ移す。owner 型がリポジトリ内のどこにも存在しない以上、その owner を import / 生成して
    // member へ到達する静的経路も存在しない。member の bare name カウントだけでは、同一 diff
    // 内で切替先の別クラスが持つ同名メソッド (`listEvents` 等) への参照を「削除メソッドへの
    // 残存参照」と誤認し、owner は informational (rm_dead) なのに member だけ blocking (rm) に
    // 残って hook を止めていた (Issue 2026-07-15-ts-add-refactor-delete-chain-api-rm-fp)。
    //
    // 条件は「owner が removed_dead に居る」だけでは不十分。第 1 パスは def_count <= 1 かつ
    // ref_count == 0 を removed_dead にするため、partial class / open class / extension で
    // 新ツリーに owner の別定義が 1 つ残る (def_count == 1) ケースも removed_dead に入りうる。
    // その場合 owner 型は生存しており member 削除は破壊的変更なので降格してはならない。
    // よって counts が厳密に (0, 0) の owner だけを対象にする。counts に owner が無い
    // (参照検索の欠落) 場合も fail-closed で除外する (codex レビュー指摘)。
    // owner は型 kind (class/struct/trait/interface/enum) に限定し、同名の削除済み関数など
    // による誤降格を防ぐ。
    let dead_type_owner_keys: HashSet<(String, String)> = removed_dead
        .iter()
        .filter(|c| {
            matches!(
                c.kind.as_str(),
                "class" | "struct" | "trait" | "interface" | "enum"
            ) && counts.get(bare_name(&c.name)).copied() == Some((0, 0))
        })
        .map(|c| (c.file.clone(), c.name.clone()))
        .collect();
    if !dead_type_owner_keys.is_empty() {
        let (follow, kept): (Vec<ApiSymbolCandidate>, Vec<ApiSymbolCandidate>) =
            removed_kept.into_iter().partition(|c| {
                matches!(c.kind.as_str(), "method" | "function")
                    && c.name.rsplit_once('.').is_some_and(|(owner, _member)| {
                        dead_type_owner_keys.contains(&(c.file.clone(), owner.to_string()))
                    })
            });
        removed_kept = kept;
        removed_dead.extend(follow);
    }

    (removed_kept, removed_dead)
}

/// `<dir>/package.json` の dependencies / devDependencies / peerDependencies /
/// optionalDependencies のキー (外部パッケージ名) を集める。package.json 不在 / パース
/// 失敗時は空集合 (= 何も除外しない、保守的)。
pub(crate) fn load_external_package_names(dir: &str) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    let path = std::path::Path::new(dir).join("package.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return HashSet::new();
    };
    let mut pkgs = HashSet::new();
    for key in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(obj) = json.get(key).and_then(|v| v.as_object()) {
            for name in obj.keys() {
                pkgs.insert(name.clone());
            }
        }
    }
    pkgs
}

/// import specifier から npm パッケージ名を取り出す。相対 (`./` `../` `/`) / alias
/// (`@/` `~/` `#`) は外部パッケージではないため None (保守的に内部扱い)。scoped は
/// `@scope/pkg`、bare は最初のセグメント。
pub(crate) fn import_specifier_package_name(spec: &str) -> Option<String> {
    if spec.is_empty()
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/')
        || spec.starts_with("@/")
        || spec.starts_with("~/")
        || spec.starts_with('#')
    {
        return None;
    }
    if let Some(scoped) = spec.strip_prefix('@') {
        // @scope/pkg[/sub]
        let mut parts = scoped.splitn(3, '/');
        let scope = parts.next()?;
        let pkg = parts.next()?;
        if scope.is_empty() || pkg.is_empty() {
            return None;
        }
        return Some(format!("@{scope}/{pkg}"));
    }
    spec.split('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `ref_path` の TS/JS ファイル内で、`symbol` が外部パッケージ (`external_pkgs`) からの
/// import で束縛されているか。束縛されていれば、そのファイルの `symbol` 参照は削除した
/// ローカルシンボルとは別物 (別モジュールの同名型) と判断できる。
/// 非 TS/JS / 読み込み・parse 失敗 / external_pkgs 空は false (除外しない、保守的)。
/// `ref_path` の TS/JS ファイルを解析し、`symbol` に関する外部パッケージ import 情報を返す。
/// 戻り値 `(local_bound, source_name_lines)`:
/// - `local_bound`: 外部パッケージ (`external_pkgs`) からの import で local binding が `symbol`
///   (`import { Config }` / `import { Foo as Config }` / `import Config` / `import * as Config`)。
///   この場合ファイル内の `symbol` 利用は外部由来 (使用箇所も除外対象)。
/// - `source_name_lines`: 外部 import specifier の **import 元名** が `symbol` の行 (0-indexed)。
///   `import { Config as X } from "pkg"` の `Config` は別モジュールの export 名なので、その
///   import 行の参照だけを除外する (使用箇所は local binding X で別物)。
///
/// 非 TS/JS / 読み込み・parse 失敗 / external_pkgs 空は `(false, 空集合)` (除外しない、保守的)。
pub(crate) fn analyze_external_import_for_symbol(
    dir: &str,
    ref_path: &str,
    symbol: &str,
    external_pkgs: &std::collections::HashSet<String>,
) -> (bool, std::collections::HashSet<usize>) {
    use crate::language::LangId;
    use std::collections::HashSet;
    let empty = (false, HashSet::new());
    if external_pkgs.is_empty() {
        return empty;
    }
    let abs = if std::path::Path::new(ref_path).is_absolute() {
        std::path::PathBuf::from(ref_path)
    } else {
        std::path::Path::new(dir).join(ref_path)
    };
    let Some(utf8) = camino::Utf8Path::from_path(&abs) else {
        return empty;
    };
    let Ok(lang) = LangId::from_path(utf8) else {
        return empty;
    };
    if !matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx) {
        return empty;
    }
    let Ok(source) = parser::read_file(utf8) else {
        return empty;
    };
    let Ok(tree) = parser::parse_source(&source, lang) else {
        return empty;
    };
    let root = tree.root_node();
    let mut local_bound = false;
    let mut source_name_lines = HashSet::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import_statement" {
            continue;
        }
        let Some(src_node) = child.child_by_field_name("source") else {
            continue;
        };
        let Some(spec) = static_js_string_text(src_node, &source) else {
            continue;
        };
        let Some(pkg) = import_specifier_package_name(spec) else {
            continue;
        };
        if !external_pkgs.contains(&pkg) {
            continue;
        }
        collect_external_import_bindings(
            child,
            &source,
            symbol,
            &mut local_bound,
            &mut source_name_lines,
        );
    }
    (local_bound, source_name_lines)
}

/// 外部パッケージ import 文 `import_stmt` を解析し、`symbol` の local binding 有無を
/// `local_bound` に、import 元名が `symbol` の出現行を `source_name_lines` に記録する。
pub(crate) fn collect_external_import_bindings(
    import_stmt: tree_sitter::Node,
    source: &[u8],
    symbol: &str,
    local_bound: &mut bool,
    source_name_lines: &mut std::collections::HashSet<usize>,
) {
    let mut cursor = import_stmt.walk();
    let Some(clause) = import_stmt
        .named_children(&mut cursor)
        .find(|c| c.kind() == "import_clause")
    else {
        return;
    };
    let mut clause_cursor = clause.walk();
    for child in clause.named_children(&mut clause_cursor) {
        match child.kind() {
            // default import: `import Config from "..."`
            "identifier" => {
                if child.utf8_text(source).ok() == Some(symbol) {
                    *local_bound = true;
                }
            }
            // namespace import: `import * as Config from "..."`
            "namespace_import" => {
                let mut ns = child.walk();
                if child
                    .named_children(&mut ns)
                    .any(|n| n.kind() == "identifier" && n.utf8_text(source).ok() == Some(symbol))
                {
                    *local_bound = true;
                }
            }
            // named imports: `import { Foo, Bar as Baz } from "..."`
            "named_imports" => {
                let mut ni = child.walk();
                for spec in child.named_children(&mut ni) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    let name_node = spec.child_by_field_name("name");
                    // import 元名が symbol → その出現行を記録 (別モジュールの export 名)
                    if let Some(name) = name_node
                        && name.utf8_text(source).ok() == Some(symbol)
                    {
                        source_name_lines.insert(name.start_position().row);
                    }
                    // local binding (alias があれば alias、無ければ name) が symbol → 利用も外部
                    let local = spec.child_by_field_name("alias").or(name_node);
                    if local.and_then(|n| n.utf8_text(source).ok()) == Some(symbol) {
                        *local_bound = true;
                    }
                }
            }
            _ => {}
        }
    }
}

/// 削除された bash 関数 `name` が、変更後ツリーの bash 系ファイル内のどこからも
/// 参照されていないかを判定する。CLI スクリプトを別言語に書き換えたときに、
/// 新言語側の同名定義/参照を「別物」として扱うため bash ファイル限定で検索する。
/// 参照検索に失敗した場合は保守的に false を返してレビュー対象として残す。
pub(crate) fn is_removed_bash_symbol_unreferenced(dir: &str, name: &str) -> bool {
    let service = AppService::new();
    let Ok(refs_result) = service.find_references(name, dir, None) else {
        return false;
    };
    refs_result
        .references
        .iter()
        .all(|r| !is_bash_script_path(r.path.as_str()))
}

/// 拡張子から bash 系シェルスクリプトファイル（.sh / .bash / .zsh）かを判定する。
pub(crate) fn is_bash_script_path(file_path: &str) -> bool {
    std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| matches!(ext, "sh" | "bash" | "zsh"))
}

/// `git show <base>:<file_path>` の内容から bash 関数 `name` が `export -f` 等で
/// 明示的にエクスポートされているか判定する。base 側の取得に失敗した場合は
/// 保守的に false（未 export 扱い）を返す。
pub(crate) fn bash_function_is_exported_in_git(
    dir: &str,
    base: &str,
    file_path: &str,
    name: &str,
) -> bool {
    if validate_git_revision(base, "--base").is_err()
        || validate_git_revision(file_path, "diff file path").is_err()
    {
        return false;
    }
    let Ok(output) = std::process::Command::new("git")
        .args(["show", &format!("{base}:{file_path}")])
        .current_dir(dir)
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(text) = std::str::from_utf8(&output.stdout) else {
        return false;
    };
    bash_has_export_f(text, name)
}

/// shell ソース文字列に `export -f <name>` / `declare -fx <name>` / `declare -xf <name>`
/// による関数エクスポート宣言が含まれているかを判定する。
///
/// 各行を `trim_start()` してから先頭一致を見るため、インデント付きの宣言にも対応する。
/// 同一行に複数名を列挙する形式 (`export -f foo bar`) もサポートする。
pub(crate) fn bash_has_export_f(source: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    const PREFIXES: &[&str] = &["export -f ", "declare -fx ", "declare -xf "];
    for line in source.lines() {
        let trimmed = line.trim_start();
        for prefix in PREFIXES {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                for token in rest.split_whitespace() {
                    if token == name {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Python のクラス内に存在するフィールド宣言 (`name: type` 形式) を集める。
///
/// `@property def x(self) -> T` から `@dataclass` フィールド `x: T` への置き換えを検出する
/// ために使う。tree-sitter で `class_definition` を走査し、`name` フィールドが `class_name`
/// と一致するクラスの body 直下にある `name: type` 宣言の左辺 identifier を返す。
pub(crate) fn extract_python_class_fields(
    dir: &str,
    file_path: &str,
    class_name: &str,
) -> std::collections::HashSet<String> {
    let mut fields = std::collections::HashSet::new();
    let full_path = std::path::Path::new(dir).join(file_path);
    let utf8_path = match camino::Utf8Path::from_path(&full_path) {
        Some(p) => p,
        None => return fields,
    };
    let lang_id = match crate::language::LangId::from_path(utf8_path) {
        Ok(l) => l,
        Err(_) => return fields,
    };
    if lang_id != crate::language::LangId::Python {
        return fields;
    }
    let source = match parser::read_file(utf8_path) {
        Ok(s) => s,
        Err(_) => return fields,
    };
    let tree = match parser::parse_source(&source, lang_id) {
        Ok(t) => t,
        Err(_) => return fields,
    };

    walk_python_class_for_fields(tree.root_node(), &source, class_name, &mut fields);
    fields
}

pub(crate) fn walk_python_class_for_fields(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    class_name: &str,
    out: &mut std::collections::HashSet<String>,
) {
    if node.kind() == "class_definition"
        && let Some(name_node) = node.child_by_field_name("name")
        && name_node.utf8_text(source).ok() == Some(class_name)
        && let Some(body) = node.child_by_field_name("body")
    {
        collect_python_dataclass_fields(body, source, out);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_python_class_for_fields(child, source, class_name, out);
    }
}

/// Python のクラス body 直下にある `name: type` 形式の宣言の左辺 identifier を集める。
///
/// tree-sitter-python では `name: type` (右辺なし) は `expression_statement > assignment`
/// に展開され、`assignment.left = identifier` / `assignment.type` が存在する。`name: type = default`
/// の形式も同じく `assignment` ノードで `right` が追加されるだけなので同じハンドラで取れる。
pub(crate) fn collect_python_dataclass_fields(
    body: tree_sitter::Node<'_>,
    source: &[u8],
    out: &mut std::collections::HashSet<String>,
) {
    let mut cursor = body.walk();
    for stmt in body.children(&mut cursor) {
        if stmt.kind() != "expression_statement" {
            continue;
        }
        let mut sub_cursor = stmt.walk();
        for sub in stmt.children(&mut sub_cursor) {
            if sub.kind() != "assignment" {
                continue;
            }
            let Some(left) = sub.child_by_field_name("left") else {
                continue;
            };
            if left.kind() != "identifier" {
                continue;
            }
            // `type` フィールドが存在するもの（typed annotation）のみ対象
            if sub.child_by_field_name("type").is_none() {
                continue;
            }
            if let Ok(name) = left.utf8_text(source) {
                out.insert(name.to_string());
            }
        }
    }
}

/// Python の `@property def member(self) -> T` を `@dataclass` フィールド `member: T` に
/// 置き換えた変更を検出する。
///
/// `qualname` は `Container.member` 形式の文字列。`diff_new_paths` 内のいずれかの新ファイルに
/// 同名 `Container` クラスが存在し、その中に `member: type` の typed annotation 宣言が
/// あれば、それが置き換え先のファイルパスであるとして返す。複数候補があれば最初のものを返す。
///
/// `old_path` は削除シンボルの元ファイル。Python 以外なら対象外 (他言語の `Container.member`
/// 削除が、diff 内 .py の偶然の同名 class+field で informational に降格するのを防ぐ)。
pub(crate) fn detect_python_property_to_field(
    dir: &str,
    old_path: &str,
    qualname: &str,
    diff_new_paths: &HashSet<String>,
) -> Option<String> {
    if !matches!(
        crate::language::LangId::from_path(camino::Utf8Path::new(old_path)),
        Ok(crate::language::LangId::Python)
    ) {
        return None;
    }
    let (container, member) = qualname.split_once('.')?;
    if container.is_empty() || member.is_empty() {
        return None;
    }
    // qualname がさらにネストしている場合 (`A.B.member`) は保守的に対象外とする。
    if member.contains('.') {
        return None;
    }
    for new_path in diff_new_paths {
        if !std::path::Path::new(new_path)
            .extension()
            .and_then(|s| s.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("py"))
            .unwrap_or(false)
        {
            continue;
        }
        let fields = extract_python_class_fields(dir, new_path, container);
        if fields.contains(member) {
            return Some(new_path.clone());
        }
    }
    None
}
