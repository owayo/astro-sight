//! 追加 / 削除 / 変更されたファイルごとの API 差分分類。

use super::*;

/// 内部用: reconcile のために signature を保持する一時構造。
#[derive(Debug, Clone)]
pub(crate) struct ApiSymbolCandidate {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) file: String,
    pub(crate) signature: String,
}

impl ApiSymbolCandidate {
    pub(crate) fn into_api_symbol(self) -> ApiSymbol {
        ApiSymbol {
            name: self.name,
            kind: self.kind,
            file: self.file,
            refs_internal: 0,
        }
    }

    /// 同一ファイル内参照数を添えて `ApiSymbol` にする (api.add 専用)。
    pub(crate) fn into_added_api_symbol(self, refs_internal: usize) -> ApiSymbol {
        ApiSymbol {
            refs_internal,
            ..self.into_api_symbol()
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
pub(crate) fn process_modified_file(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
) {
    let maps = SymbolMaps::build(facts);
    // 追加 → 削除 → 変更 の順で振り分ける。削除判定は追加判定が集めた
    // 「同ファイル内の新規シンボル名」を rename 判定に使うため、この順序に依存する。
    let new_symbols_in_current_file = collect_added_symbols(inputs, state, df, facts, &maps);
    collect_removed_symbols(
        inputs,
        state,
        df,
        facts,
        &maps,
        &new_symbols_in_current_file,
    );
    collect_modified_symbols(inputs, state, df, facts, &maps);
}

/// 変更ファイルの新旧 exported シンボルから作る突き合わせ用インデックス。
///
/// 同名シンボルが旧/新いずれかに複数存在する場合、`HashMap<name, sig>` は最後の 1 件しか
/// 保持できず、別のオーバーロードや誤パースされた定義同士を突き合わせて api.mod を
/// 誤検出する。出現回数も併せて持ち、複数あるシンボルは曖昧として modified 判定から除外する
/// (Issue #13: C++ overload / マクロ誤パースの api.mod 誤検出対策)。
struct SymbolMaps<'a> {
    old_map: std::collections::HashMap<&'a str, &'a str>,
    new_map: std::collections::HashMap<&'a str, (&'a str, &'a str)>,
    old_name_counts: std::collections::HashMap<&'a str, usize>,
    new_name_counts: std::collections::HashMap<&'a str, usize>,
}

impl<'a> SymbolMaps<'a> {
    fn build(facts: &ModifiedFileFacts<'a>) -> Self {
        let mut old_name_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for (name, _, _) in facts.old_syms {
            *old_name_counts.entry(name.as_str()).or_default() += 1;
        }
        let mut new_name_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for (name, _, _) in facts.new_syms {
            *new_name_counts.entry(name.as_str()).or_default() += 1;
        }
        Self {
            old_map: facts
                .old_syms
                .iter()
                .map(|(name, _kind, sig)| (name.as_str(), sig.as_str()))
                .collect(),
            new_map: facts
                .new_syms
                .iter()
                .map(|(name, kind, sig)| (name.as_str(), (kind.as_str(), sig.as_str())))
                .collect(),
            old_name_counts,
            new_name_counts,
        }
    }
}

/// 旧ツリーに存在しない新規シンボルを `all_new_candidates` / `added` に振り分ける。
///
/// 返り値は同ファイル内の新規シンボル名集合。削除判定が「rename + 実装置換」の
/// 除外条件として使うため、追加判定の副産物として返す。
fn collect_added_symbols(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    maps: &SymbolMaps<'_>,
) -> std::collections::HashSet<String> {
    let &DetectionInputs {
        dir,
        diff_new_paths,
        ref_index,
        ..
    } = inputs;
    let &ModifiedFileFacts {
        new_syms,
        in_file_callees,
        ..
    } = facts;
    let SymbolMaps { old_map, .. } = maps;

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
            state.buckets.all_new_candidates.push(candidate.clone());
            if is_binary_rust_crate {
                continue;
            }
            if is_rust_new_symbol_outside_public_api_surface(
                dir,
                &df.new_path,
                name,
                state.worktree_reexports,
            ) {
                continue;
            }
            if is_internally_connected(in_file_callees, name) {
                continue;
            }
            if is_used_in_diff_paths(ref_index, dir, name, &df.new_path, diff_new_paths) {
                continue;
            }
            state.buckets.added.push(candidate);
        }
    }
    new_symbols_in_current_file
}

/// 新ツリーに存在しない削除シンボルを `removed` / `property_to_field` に振り分ける。
fn collect_removed_symbols(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    maps: &SymbolMaps<'_>,
    new_symbols_in_current_file: &std::collections::HashSet<String>,
) {
    let &DetectionInputs {
        dir,
        base,
        diff_new_paths,
        ref_index,
        ..
    } = inputs;
    let &ModifiedFileFacts {
        old_syms,
        new_export_surface_names,
        ..
    } = facts;
    let SymbolMaps { new_map, .. } = maps;

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
                state.base_reexports,
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
                state.buckets.property_to_field.push(PropertyToFieldChange {
                    name: name.clone(),
                    file: target_file,
                });
                continue;
            }
            state.buckets.removed.push(ApiSymbolCandidate {
                name: name.clone(),
                kind: kind.clone(),
                file: df.old_path.clone(),
                signature: sig.clone(),
            });
        }
    }
}

/// 新旧どちらにも存在しシグネチャが変わったシンボルを分類する。
fn collect_modified_symbols(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    maps: &SymbolMaps<'_>,
) {
    let &DetectionInputs {
        dir,
        base,
        ref_index,
        ..
    } = inputs;
    let &ModifiedFileFacts {
        new_syms,
        in_file_callees,
        ..
    } = facts;
    let SymbolMaps {
        old_map,
        old_name_counts,
        new_name_counts,
        ..
    } = maps;

    // Rust bin-only crate 判定 (api.mod 抑制用)。lib → bin / bin → lib どちらかが bin-only なら
    // 外部 API 面の変更ではないとみなす。
    let is_binary_rust_old_crate_for_mod =
        state
            .base_reexports
            .is_binary_only_at_base(dir, base, &df.old_path);
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
                state.base_reexports,
            ) {
                continue;
            }
            // 同名が旧/新いずれかに複数あるシンボルは曖昧として modified から除外
            if old_name_counts.get(name.as_str()).copied().unwrap_or(0) > 1
                || new_name_counts.get(name.as_str()).copied().unwrap_or(0) > 1
            {
                continue;
            }
            // closed-in-diff: 同一ファイル内でしか呼ばれていない関数のシグネチャ変更は除外。
            //
            // ただし Python の TypedDict `total=` 変更候補だけはここで落とさない。
            // 落とすと契約変更の 3 値判定に到達できず、「キーが必須化する破壊的変更」が
            // api.mod にすら出なくなる。判定が `NotApplicable` なら
            // `classify_signature_change` 側で従来どおり捨てる。
            let internally_closed = is_internally_connected(in_file_callees, name)
                && !has_cross_file_refs(ref_index, &df.new_path, name);
            if internally_closed
                && !may_be_python_typed_dict_total_change(kind, old_sig, new_sig, lang_id_for_file)
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
                // blocking な api.mod に確定した時点でのみ算出する (classify_signature_change)。
                no_resolved_internal_callers: false,
                // 型契約変更として種別が確定した時点で埋める (classify_signature_change)。
                contract_change: None,
            };
            classify_signature_change(
                inputs,
                state,
                df,
                SignatureChangeSite {
                    change,
                    kind,
                    old_sig,
                    new_sig,
                    lang_id: lang_id_for_file,
                    internally_closed,
                },
            );
        }
    }
}

/// シグネチャ変更を const_value / compatible_modified (React HOC / Object member 削除) /
/// modified_closed_in_diff / modified のいずれかに分類する。
pub(crate) fn classify_signature_change(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    site: SignatureChangeSite<'_>,
) {
    let &DetectionInputs {
        dir,
        base,
        diff_files,
        ref_index,
        ..
    } = inputs;
    let SignatureChangeSite {
        change,
        kind,
        old_sig,
        new_sig,
        lang_id: lang_id_for_file,
        internally_closed,
    } = site;
    let name = change.name.clone();
    // const / 非 mut static / export const の値 (initializer) のみ変更は const_value_changes へ
    if lang_id_for_file.is_some_and(|lid| is_const_value_only_change(old_sig, new_sig, kind, lid)) {
        state.buckets.const_value_changes.push(change);
        return;
    }
    // 以降の互換判定器はすべて同じ現場情報を見る。old/new ソースは最初に必要になった
    // 判定器で 1 度だけ取得して使い回す (旧実装は判定器ごとに git show を起動していた)。
    let site = CompatibleModSite {
        dir,
        base,
        old_path: &df.old_path,
        new_path: &df.new_path,
        name: &name,
        kind,
        old_sig,
        new_sig,
        lang_id: lang_id_for_file,
    };
    let sources = &mut SignatureSourceCache::default();
    // Python の TypedDict で `total=` が絡む変更は blocking な api.mod に確定させる。
    // 互換判定器や closed-in-diff 判定より**前**に置くのが要点で、「リポジトリ内の参照が
    // 同一 diff で更新済み」でも降格させないため (外部リポジトリ / 動的生成された dict は
    // 静的に追えない。Issue 2026-08-18-python-typeddict-contract-change-classification)。
    //
    // **種別を確定できなくても降格させない**。分類できないことは「破壊的でない」証明では
    // ないため (`NotRequired` 同居で分類を諦めたケースでも bare フィールドの requiredness は
    // 実際に反転する)。ラベル (`contract_change`) は確定したときだけ添える。
    match detect_python_typed_dict_total_change(&site, sources) {
        // TypedDict の `total=` 変更だと**証明できなかった**ので、同一ファイル内でしか
        // 使われていないシンボルは従来どおり api.mod に出さない (上の早期 continue の続き)。
        // 証明できないケースまで出すと、`class X(Base, total=False)` のような無関係な
        // Python class を新たに blocking にしてしまう。この早期除外そのものの是非は
        // 言語横断の既存ルールなので本 Issue では触らない。
        PythonContractDetection::NotApplicable => {
            if internally_closed {
                return;
            }
        }
        PythonContractDetection::PotentialBreakingChange if internally_closed => {
            return;
        }
        detection => {
            let mut change = change;
            if let PythonContractDetection::Classified(contract) = detection {
                change.contract_change = Some(contract);
            }
            change.no_resolved_internal_callers =
                has_no_resolved_internal_callers(ref_index, dir, &name, state.closure_caches);
            state.buckets.modified.push(change);
            return;
        }
    }
    // React component を memo / forwardRef 等の HOC でラップしただけは compatible_modified
    if let Some(compat) = detect_react_wrapper_compatible_mod(ref_index, &site, sources) {
        state.buckets.compatible_modified.push(compat);
        return;
    }
    // exported object の未参照プロパティ削除も compatible_modified
    if let Some(compat) = detect_object_members_compatible_mod(&site, sources) {
        state.buckets.compatible_modified.push(compat);
        return;
    }
    // TS/TSX の関数末尾へ optional/default 引数を追加しただけなら、既存呼び出しの required
    // arity は変わらないため compatible_modified として扱う。
    if let Some(compat) = detect_trailing_optional_params_compatible_mod(&site, sources) {
        state.buckets.compatible_modified.push(compat);
        return;
    }
    // TS/TSX の引数 object type literal へ optional プロパティを追加しただけなら、
    // 既存呼び出しが渡す object はそのまま受理されるため compatible_modified として扱う。
    if let Some(compat) = detect_optional_object_props_compatible_mod(&site, sources) {
        state.buckets.compatible_modified.push(compat);
        return;
    }
    // React Server Component の async 化 (async キーワード追加のみ + 全参照が JSX タグ利用)
    // は呼び出し側の書き換えが不要なため compatible_modified として扱う。
    if let Some(compat) = detect_async_jsx_component_compatible_mod(ref_index, &site, sources) {
        state.buckets.compatible_modified.push(compat);
        return;
    }
    // Python のトップレベル関数 / モジュール直下クラスのメソッドへ末尾 optional/default
    // 引数 (`*` 後の kwonly+default 含む) を追加しただけなら、既存呼び出しが壊れないため
    // compatible_modified として扱う。
    if let Some(compat) = detect_python_trailing_optional_params_compatible_mod(&site, sources) {
        state.buckets.compatible_modified.push(compat);
        return;
    }
    // object type literal 引数への必須プロパティ追加のみの変更なら、閉包判定へ追加証拠として
    // 渡す (呼び出し式が無変更でも共有 const の定義側が同一 diff で更新されていれば追随済み)。
    // 互換変更ではないので compatible_modified には入れず、あくまで closed 判定の入力。
    let added_required_props = detect_added_required_object_props(&site, sources);
    // 全 cross-file 参照が同一 diff 内で追随済みなら informational
    if is_modified_closed_in_diff(
        ModifiedClosureInput {
            index: ref_index,
            dir,
            name: &name,
            kind,
            base,
            target_new_path: &df.new_path,
            diff_files,
            added_required_props: added_required_props.as_ref(),
        },
        state.closure_caches,
    ) {
        state.buckets.modified_closed_in_diff.push(change);
    } else {
        // blocking な api.mod にだけ「解決できた呼び出し参照ゼロ」フラグを添える。
        // 分類も blocking 判定も変えず、トリアージが「呼び出し側を探す」段階を省けるようにする。
        let mut change = change;
        change.no_resolved_internal_callers =
            has_no_resolved_internal_callers(ref_index, dir, &name, state.closure_caches);
        state.buckets.modified.push(change);
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
pub(crate) fn is_python_root_script_local_file(dir: &str, base: &str, old_path: &str) -> bool {
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
pub(crate) fn python_module_referenced_in_tree(dir: &str, stem: &str) -> bool {
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
pub(crate) fn base_pyproject_marks_module_as_api(dir: &str, base: &str, stem: &str) -> bool {
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

/// `git show <base>:<rel>` でファイル内容を UTF-8 文字列として取得する。失敗時は None。
pub(crate) fn git_show_base_file(dir: &str, base: &str, rel: &str) -> Option<String> {
    String::from_utf8(git_show_blob(dir, base, rel)?).ok()
}

/// 削除ファイル (`new_path == "/dev/null"`) 由来の exported シンボルを `removed` /
/// `property_to_field` に分類する。
///
/// Rust private module / bin-only crate / Bash の未 export 関数 / Python `@property` →
/// dataclass field 置き換え / Python root-level スクリプトの helper は `removed` から除外する。
pub(crate) fn process_deleted_file(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
) {
    let &DetectionInputs {
        dir,
        base,
        diff_new_paths,
        ..
    } = inputs;
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
            state.base_reexports,
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
            state.buckets.property_to_field.push(PropertyToFieldChange {
                name: name.clone(),
                file: target_file,
            });
            continue;
        }
        state.buckets.removed.push(ApiSymbolCandidate {
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
pub(crate) fn process_added_file(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &AddedFileFacts<'_>,
) {
    let &DetectionInputs {
        dir,
        diff_new_paths,
        ref_index,
        ..
    } = inputs;
    let &AddedFileFacts {
        new_syms,
        in_file_callees,
    } = facts;
    let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path);
    for (name, kind, sig) in new_syms {
        let candidate = ApiSymbolCandidate {
            name: name.clone(),
            kind: kind.clone(),
            file: df.new_path.clone(),
            signature: sig.clone(),
        };
        state.buckets.all_new_candidates.push(candidate.clone());
        if is_binary_rust_crate {
            continue;
        }
        if is_rust_new_symbol_outside_public_api_surface(
            dir,
            &df.new_path,
            name,
            state.worktree_reexports,
        ) {
            continue;
        }
        if is_internally_connected(in_file_callees, name) {
            continue;
        }
        if is_used_in_diff_paths(ref_index, dir, name, &df.new_path, diff_new_paths) {
            continue;
        }
        state.buckets.added.push(candidate);
    }
}
