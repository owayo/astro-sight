use anyhow::Result;
use std::collections::HashSet;
use tracing::info;

use crate::engine::parser;
use crate::error::{AstroError, ErrorCode};
use crate::models::dead_code::DeadCodeResult;
use crate::models::review::DeadSymbol;

#[cfg(test)]
use super::api_changes::extract_exported_symbols_from_file_inner;
use super::api_changes::{
    bare_name, extract_exported_symbols_from_file_inner_with_lang, extract_symbol_lines,
};
use super::common::{MAX_INPUT_SIZE, read_file_to_string_limited, serialize_output};
use super::dead_code_member_liveness::{JsTsMemberLiveness, MemberStatus, PhpMemberLiveness};
use super::git_input::{GitDiffInput, resolve_git_diff};

/// dead-code 検出本体。候補収集 → 名前インデックス構築 → 参照カウント →
/// アセット参照収集 → 分類の段階パイプラインで (dead_symbols, test_only_symbols) を返す。
pub(crate) fn detect_dead_symbols_from_files(
    dir: &str,
    files: &[std::path::PathBuf],
) -> (Vec<DeadSymbol>, Vec<DeadSymbol>) {
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(d) => d,
        Err(_) => return (Vec::new(), Vec::new()),
    };

    // case-insensitive 言語 (Xojo 等) のみで構成された files では dead-code 検出を
    // skip する。
    //
    // v26.5 まで: CI 言語 (Xojo) は tree-sitter parse が OOM する問題で diff 全体を skip。
    // v26.6 以降: tree-sitter-xojo を削除し lexer-only に移行。dead-code は lexer 経由で
    // 動作するため CI skip 機構は不要。`ASTRO_SIGHT_FORCE_CI_LANG_DEAD_CODE` は deprecate
    // (no-op、警告も出さない)。

    let candidates = collect_dead_code_candidates(dir, &canonical_dir, files);
    if candidates.all_syms.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let index = build_dead_code_name_index(&candidates, &canonical_dir, files);

    // production / test 別に refs カウント。test/ 配下のみで参照されるシンボルは
    // dead_symbols ではなく test_only_symbols として分離する (F5)。
    let counts = match crate::engine::refs::count_non_definition_refs_split_with_extra_files(
        &index.unique_names,
        &canonical_dir,
        None,
        files,
        is_test_path,
    ) {
        Ok(v) => v,
        Err(_) => return (Vec::new(), Vec::new()),
    };

    let asset_refs = collect_framework_asset_refs(&canonical_dir);

    classify_dead_symbols(&candidates, &index, &counts, &asset_refs)
}

/// 候補収集の中間状態。dead-code 候補シンボルと C/C++ liveness 補助情報を保持する。
struct DeadCodeCandidates {
    /// 全ファイルのエクスポートシンボル (original_name, kind, file, lang_id)。
    /// case-insensitive 言語では lang_id でシンボル名を正規化した比較を行うため lang も保持する。
    all_syms: Vec<(String, String, String, crate::language::LangId)>,
    /// C/C++ の追加 liveness 情報 (file, シンボル名, 追加名リスト, lang)。
    /// enum→列挙子名 / typedef tag→alias 名。後段で正規化して liveness_aliases に変換する。
    liveness_raw: Vec<(String, String, Vec<String>, crate::language::LangId)>,
}

/// 走査対象ファイルからエクスポートシンボル（trait impl メソッドは除外）と
/// C/C++ liveness 補助情報を収集する。
fn collect_dead_code_candidates(
    dir: &str,
    canonical_dir: &std::path::Path,
    files: &[std::path::PathBuf],
) -> DeadCodeCandidates {
    // .gitattributes の linguist-generated 指定ファイルは dead-code 検出から除外する
    let gitattrs = crate::engine::gitattributes::GitAttributes::load(canonical_dir);

    let mut all_syms: Vec<(String, String, String, crate::language::LangId)> = Vec::new();
    let mut liveness_raw: Vec<(String, String, Vec<String>, crate::language::LangId)> = Vec::new();
    for path in files {
        // canonicalize で削除済みファイルをスキップ、dir 外のパスも除外
        let canonical_path = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let rel = match canonical_path.strip_prefix(canonical_dir) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue, // dir 外のパスは除外（セキュリティ境界）
        };
        if gitattrs.is_generated(&rel) {
            continue;
        }
        // ファイル先頭の「自動生成」マーカーコメントでも除外する (.gitattributes が
        // 無いリポジトリでも tree-sitter の parser.c / protoc の *.pb.go 等を無視できる)
        if crate::engine::generated::is_auto_generated(&canonical_path) {
            continue;
        }
        if let Some((lang, syms)) =
            extract_exported_symbols_from_file_inner_with_lang(dir, &rel, true, true)
        {
            for (name, kind, _sig) in syms {
                all_syms.push((name, kind, rel.clone(), lang));
            }
            // C/C++ では enum の列挙子・typedef alias を liveness 補助名として集める。
            // enum 型名が直接使われなくても列挙子が使われていれば live、body あり typedef tag が
            // alias 名でのみ使われていても live と判定するために使う (Issue #11/#12)。
            if matches!(
                lang,
                crate::language::LangId::C | crate::language::LangId::Cpp
            ) {
                for (sym, extras) in collect_cpp_liveness_for_file(dir, &rel, lang) {
                    liveness_raw.push((rel.clone(), sym, extras, lang));
                }
            }
        }
    }

    DeadCodeCandidates {
        all_syms,
        liveness_raw,
    }
}

/// 検索インデックスの中間状態。正規化済み bare name の同名カウント / liveness alias / 検索対象名。
struct DeadCodeNameIndex {
    /// 正規化 bare name → 出現数。同名 export が複数あるシンボルの保守的スキップに使う。
    name_counts: std::collections::HashMap<String, usize>,
    /// C/C++ の (file, 正規化シンボル名) → 追加 liveness 名 (正規化済み)。
    liveness_aliases: std::collections::HashMap<(String, String), Vec<String>>,
    /// refs 検索対象の正規化済みシンボル名 (重複除去済み、liveness 補助名を含む)。
    unique_names: Vec<String>,
    /// TS/JS の duplicate な同名 class member について owner 一意推定で
    /// dead/test_only/live を再判定するためのインデックス。`name_counts > 1` で
    /// スキップ対象の候補のうち、本インデックスで `Live` / `TestOnly` / `Dead` が
    /// 返るものは旧スキップを緩和して通常の dead 判定経路に乗せる。
    member_liveness: JsTsMemberLiveness,
    /// PHP の duplicate な同名 class member について `Owner::member()` などの
    /// 確定参照だけを使って dead/test_only/live を再判定するためのインデックス。
    php_member_liveness: PhpMemberLiveness,
}

/// 言語別に正規化した bare name を返す。
///
/// refs 検索は AST 上の identifier ノードに対してマッチするため、
/// `Container.method` 形式の qualname ではマッチせず常に 0 件となってしまう。
/// そのため検索キーは末尾セグメント（bare name）に統一する。
fn normalized_bare_name(lang: crate::language::LangId, name: &str) -> String {
    crate::language::normalize_identifier(lang, bare_name(name)).into_owned()
}

/// 候補シンボルから同名カウント / liveness alias / refs 検索対象名のインデックスを構築する。
/// `candidate_files` は diff 由来の候補ファイル (hidden 配下含む)。member liveness の
/// 走査集合を count 経路 (`count_non_definition_refs_split_with_extra_files`) と一致させる。
fn build_dead_code_name_index(
    candidates: &DeadCodeCandidates,
    canonical_dir: &std::path::Path,
    candidate_files: &[std::path::PathBuf],
) -> DeadCodeNameIndex {
    // 同名 export が複数ファイル/複数コンテナに存在する場合は保守的にスキップ（誤判定防止）。
    // キーは bare name を言語別に正規化したもの (Xojo では `Foo` と `FOO` を同一視)。
    let mut name_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (name, _, _, lang) in &candidates.all_syms {
        *name_counts
            .entry(normalized_bare_name(*lang, name))
            .or_default() += 1;
    }

    // C/C++ の (file, 正規化シンボル名) → 追加 liveness 名 (正規化済み) を構築。
    // enum 候補は列挙子名、typedef tag 候補は alias 名を介した参照でも live と判定する。
    let mut liveness_aliases: std::collections::HashMap<(String, String), Vec<String>> =
        std::collections::HashMap::new();
    for (file, sym, extras, lang) in &candidates.liveness_raw {
        let key = normalized_bare_name(*lang, sym);
        let extra_keys: Vec<String> = extras
            .iter()
            .map(|e| normalized_bare_name(*lang, e))
            .collect();
        liveness_aliases
            .entry((file.clone(), key))
            .or_default()
            .extend(extra_keys);
    }

    // 全シンボル名の非 Definition 参照件数をカウント（SymbolReference を確保しない）。
    // 入力も正規化済みキーで渡し、refs 側の HashMap キーと lookup を一致させる。
    // liveness 補助名 (列挙子 / alias) も検索対象に含め、enum/tag の生存判定に使う。
    let unique_names: Vec<String> = {
        let mut seen = HashSet::new();
        let mut names = Vec::new();
        for (name, _, _, lang) in &candidates.all_syms {
            let k = normalized_bare_name(*lang, name);
            if seen.insert(k.clone()) {
                names.push(k);
            }
        }
        for extras in liveness_aliases.values() {
            for ek in extras {
                if seen.insert(ek.clone()) {
                    names.push(ek.clone());
                }
            }
        }
        names
    };

    // TS/JS の duplicate な同名 class member について owner 一意推定で liveness を再判定する。
    // `name_counts > 1` で従来スキップされていた候補のうち、安全に owner を一意推定できる
    // ケースだけ通常の dead/test_only/live 判定経路に乗せる。
    let member_liveness = JsTsMemberLiveness::build(
        &candidates.all_syms,
        canonical_dir,
        candidate_files,
        is_test_path,
    );
    let php_member_liveness = PhpMemberLiveness::build(
        &candidates.all_syms,
        canonical_dir,
        candidate_files,
        is_test_path,
    );

    DeadCodeNameIndex {
        name_counts,
        liveness_aliases,
        unique_names,
        member_liveness,
        php_member_liveness,
    }
}

/// AST の cross-file refs では追跡できないフレームワークアセット由来の参照名集合。
struct FrameworkAssetRefs {
    /// AndroidManifest.xml / layout XML から参照されるシンボル名。
    xml: HashSet<String>,
    /// Angular テンプレート (`*.component.html` / inline template) から参照される名前。
    template: HashSet<String>,
}

impl FrameworkAssetRefs {
    /// bare name / qualname のいずれかでフレームワークアセットから参照されているか判定する。
    fn contains_symbol(&self, name: &str) -> bool {
        // bare name と qualname (Container.method) の両方を突き合わせる。
        // layout XML の `android:onClick="handler"` や Angular template の
        // `(event)="handler()"` は単純名でしか書けないため bare で検索し、
        // `android:name=".Foo"` 等で Container 側をカバーするケースは qualname でも検査する。
        let bare = bare_name(name);
        self.xml.contains(bare)
            || self.xml.contains(name)
            || self.template.contains(bare)
            || self.template.contains(name)
    }
}

/// Android XML / Angular テンプレートからの参照集合を収集する。
fn collect_framework_asset_refs(canonical_dir: &std::path::Path) -> FrameworkAssetRefs {
    // Android プロジェクトでは `AndroidManifest.xml` / layout XML から
    // シンボルが参照されうる（`<activity android:name=".MainActivity"/>` 等）。
    // Kotlin/Java AST のみでは追跡できない Android framework 経由の生存判定を補うため、
    // XML 参照集合に含まれるシンボルは dead から除外する。
    // AndroidManifest.xml が存在しないプロジェクトでは空集合が返り副作用なし。
    let xml = crate::engine::xml_refs::collect_xml_symbol_references(canonical_dir);

    // Angular プロジェクトでは `*.component.html` テンプレートや
    // `@Component({ template: \`...\` })` の inline template 内の binding 式から
    // component method/プロパティが参照される。TypeScript AST のみでは追跡できない
    // ため、テンプレート参照集合に含まれるシンボルは dead から除外する。
    // angular.json / *.component.ts のどちらも見つからないプロジェクトでは空集合が
    // 返り副作用なし。
    let template =
        crate::engine::angular_template_refs::collect_angular_template_refs(canonical_dir);

    FrameworkAssetRefs { xml, template }
}

/// シンボルの production / test 参照数を返す。
///
/// C/C++ では enum 列挙子 / typedef alias (liveness alias) 経由の参照も合算する。
/// enum 型名が直接使われなくても列挙子が使われていれば live、body あり typedef tag が
/// alias 名でのみ使われていても live と判定する (Issue #11/#12)。
fn ref_counts_with_liveness(
    key: &str,
    file: &str,
    index: &DeadCodeNameIndex,
    counts: &std::collections::HashMap<String, (usize, usize)>,
) -> (usize, usize) {
    let (mut prod_cnt, mut test_cnt) = counts.get(key).copied().unwrap_or((0, 0));
    if let Some(extra_keys) = index
        .liveness_aliases
        .get(&(file.to_string(), key.to_string()))
    {
        for ek in extra_keys {
            if let Some((p, t)) = counts.get(ek) {
                prod_cnt += p;
                test_cnt += t;
            }
        }
    }
    (prod_cnt, test_cnt)
}

/// qualname `Container.member` を owner/bare に分割し、言語別 member liveness を
/// 呼び出す。対象外言語、または owner を一意特定できない名前 (bare のみ / 多段
/// qualname) の場合は `None` を返し、呼び出し側は従来の保守的スキップを維持する。
fn member_status_for_candidate(
    name: &str,
    file: &str,
    lang: crate::language::LangId,
    js_ts_liveness: &JsTsMemberLiveness,
    php_liveness: &PhpMemberLiveness,
) -> Option<MemberStatus> {
    let (owner, bare) = name.rsplit_once('.')?;
    if owner.contains('.') {
        // 多段 qualname は class.member とみなさない。
        return None;
    }
    match lang {
        crate::language::LangId::Typescript
        | crate::language::LangId::Javascript
        | crate::language::LangId::Tsx => js_ts_liveness.status_for(owner, bare, file),
        crate::language::LangId::Php => php_liveness.status_for(owner, bare, file),
        _ => None,
    }
}

/// 参照カウントと各除外規約から dead / test-only シンボルを分類する。
fn classify_dead_symbols(
    candidates: &DeadCodeCandidates,
    index: &DeadCodeNameIndex,
    counts: &std::collections::HashMap<String, (usize, usize)>,
    asset_refs: &FrameworkAssetRefs,
) -> (Vec<DeadSymbol>, Vec<DeadSymbol>) {
    // production 0 / test 0 → dead_symbols
    // production 0 / test > 0 → test_only_symbols (F5)
    // production > 0 → 生存とみなしどちらにも報告しない
    let mut dead = Vec::new();
    let mut test_only = Vec::new();
    for (name, kind, file, lang) in &candidates.all_syms {
        let key = normalized_bare_name(*lang, name);
        // 同名シンボルが複数存在する場合は bare name では区別できないためスキップする。
        // ただし TS/JS の class member については `JsTsMemberLiveness` で owner 一意推定
        // が成立した場合のみ、duplicate スキップを緩和して通常判定に乗せる。
        if index.name_counts.get(&key).copied().unwrap_or(0) > 1 {
            match member_status_for_candidate(
                name,
                file,
                *lang,
                &index.member_liveness,
                &index.php_member_liveness,
            ) {
                Some(MemberStatus::Live) => continue,
                Some(MemberStatus::TestOnly) => {
                    // Angular template / Android XML から参照されるシンボルは live。
                    if asset_refs.contains_symbol(name) {
                        continue;
                    }
                    test_only.push(DeadSymbol {
                        name: name.clone(),
                        kind: kind.clone(),
                        file: file.clone(),
                    });
                    continue;
                }
                Some(MemberStatus::Dead) => {
                    // Angular template / Android XML から参照されるシンボルは live。
                    if asset_refs.contains_symbol(name) {
                        continue;
                    }
                    dead.push(DeadSymbol {
                        name: name.clone(),
                        kind: kind.clone(),
                        file: file.clone(),
                    });
                    continue;
                }
                Some(MemberStatus::Ambiguous) | None => continue,
            }
        }

        let (prod_cnt, test_cnt) = ref_counts_with_liveness(&key, file, index, counts);
        if prod_cnt > 0 {
            continue;
        }

        // Android XML / Angular テンプレート経由で参照されるシンボルは live。
        if asset_refs.contains_symbol(name) {
            continue;
        }

        let sym = DeadSymbol {
            name: name.clone(),
            kind: kind.clone(),
            file: file.clone(),
        };
        if test_cnt > 0 {
            // PHPUnit テストクラス内のヘルパーメソッドは test_only からも除外する。
            if has_phpunit_test_container(name, *lang) {
                continue;
            }
            test_only.push(sym);
        } else {
            dead.push(sym);
        }
    }

    (dead, test_only)
}

/// C/C++ ファイルをパースし、dead-code liveness 補助情報 (enum→列挙子 / typedef tag→alias) を返す。
/// `detect_dead_symbols_from_files` で enum / typedef tag の生存判定を補強するために使う。
pub(crate) fn collect_cpp_liveness_for_file(
    dir: &str,
    rel: &str,
    lang: crate::language::LangId,
) -> Vec<(String, Vec<String>)> {
    let full = std::path::Path::new(dir).join(rel);
    let Some(full_str) = full.to_str() else {
        return Vec::new();
    };
    let utf8 = camino::Utf8Path::new(full_str);
    let Ok(source) = parser::read_file(utf8) else {
        return Vec::new();
    };
    let Ok(tree) = parser::parse_source(&source, lang) else {
        return Vec::new();
    };
    crate::engine::symbols::collect_cpp_dead_liveness_aliases(tree.root_node(), &source, lang)
}

/// テストディレクトリとみなすセグメント名一覧。
///
/// - 言語共通: `tests`, `Tests`, `__tests__`, `spec`, `testdata`
/// - JVM/Gradle 標準: `test` (`src/test/`), `androidTest`, `sharedTest`, `integrationTest`
///
/// `is_test_path` (API 差分検出) と `DEFAULT_DEAD_CODE_EXCLUDES_TESTS` (dead-code 既定除外)
/// の両側で同じ判定を行うため一元化する。`is_test_path` が `test` 単数形を含む一方で
/// `DEFAULT_DEAD_CODE_EXCLUDES_TESTS` には含まれない、という履歴的なねじれ
/// (2026-05-21 の JUnit Kotlin dead 誤検出として顕在化) を解消する。
pub(crate) const TEST_DIRECTORY_SEGMENTS: &[&str] = &[
    "tests",
    "test",
    "Tests",
    "__tests__",
    "spec",
    "testdata",
    "androidTest",
    "sharedTest",
    "integrationTest",
];

/// refs カウントを production / test に振り分けるための判定関数。
///
/// - ファイル名規約 (`*_test.go`, `*Test.php`, `*_spec.rb` 等) は既存の
///   `is_test_file_path` に委譲する。
/// - ディレクトリセグメント規約は `TEST_DIRECTORY_SEGMENTS` に一元化。
pub(crate) fn is_test_path(path: &std::path::Path) -> bool {
    if let Some(s) = path.to_str() {
        if crate::engine::impact::test_context::is_test_file_path(s) {
            return true;
        }
        if s.split('/')
            .any(|seg| TEST_DIRECTORY_SEGMENTS.contains(&seg))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
pub(crate) fn extract_dead_code_candidates_from_file(
    dir: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
    // dead-code 走査では既定でテストディレクトリ (tests/, Tests/, __tests__/, spec/,
    // testdata/) が collect 段階で除外される。`--include-tests` で opt-in したときは
    // テストファイルも走査対象に含めるため、ここでは test_path 除外を行わない
    // (API 検出側 extract_exported_symbols_from_file は test path 除外を行う)。
    //
    // dead-code 判定では Typer / Click / FastAPI / Flask / pytest 等のフレームワーク
    // 登録デコレータが付いた関数を除外する。デコレータ経由でフレームワーク内部
    // レジストリに登録されるため、識別子レベルの cross-file refs では caller を
    // 追跡できず偽陽性源になる。
    extract_exported_symbols_from_file_inner(dir, file_path, true, true)
}

pub(crate) fn filter_dead_by_touched_symbols(
    dir: &str,
    dead: Vec<crate::models::review::DeadSymbol>,
    diff_input: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> Vec<crate::models::review::DeadSymbol> {
    use std::collections::{HashMap, HashSet};

    // changed file 集合 (削除ファイルは含めない)。
    let mut changed_files: HashSet<&str> = HashSet::new();
    for df in diff_files {
        if df.new_path != "/dev/null" {
            changed_files.insert(df.new_path.as_str());
        }
    }

    // 「ファイル → 追加行 set (0-indexed)」「ファイル → シンボル名→宣言行」を per-file キャッシュ。
    let mut changed_lines_cache: HashMap<String, HashSet<usize>> = HashMap::new();
    let mut sym_lines_cache: HashMap<String, HashMap<String, usize>> = HashMap::new();

    dead.into_iter()
        .filter(|ds| {
            if !changed_files.contains(ds.file.as_str()) {
                // diff に含まれないファイル: touched ではないので除外。
                return false;
            }
            let changed_lines = changed_lines_cache
                .entry(ds.file.clone())
                .or_insert_with(|| {
                    crate::engine::diff::extract_changed_new_lines(diff_input, &ds.file)
                });
            let line_map = sym_lines_cache
                .entry(ds.file.clone())
                .or_insert_with(|| extract_symbol_lines(dir, &ds.file).unwrap_or_default());
            // DeadSymbol.name は class member だと qualname (`Container.method`) だが、
            // `extract_symbol_lines` のキーは bare name。qualname で引けない場合は
            // 末尾セグメントで再 lookup しないと member が常に「宣言行不明 → touched 扱い」
            // に落ち、touched-symbols フィルタが no-op になる。
            let line_lookup = line_map
                .get(&ds.name)
                .or_else(|| line_map.get(bare_name(&ds.name)));
            let Some(&line) = line_lookup else {
                // 宣言行が引けない (lexer-only で取り漏れ等) は保守的に touched 扱いで残す。
                return true;
            };
            changed_lines.contains(&line)
        })
        .collect()
}

/// 同一 diff で新規 export された (= `api_changes.added` に列挙される) シンボルを
/// dead 警告から除外する。`review --hook` の既定挙動として「多段実装中に純粋ヘルパーを
/// 先に追加し、consumer 結線が後続コミットになる」WIP パターンのノイズを抑止するために
/// 使う。`--include-wip-dead` 指定時は呼び出し側でこの関数を skip し、新規 export も
/// dead として全件返す。
///
/// 突合せキーは `(file, name)` (DeadSymbol と ApiSymbol の共通フィールド)。kind は
/// 同一シンボルで言語別表記が一致しないケースを避けるため照合対象から外す
/// (例: `class` vs `struct` の混在は file+name の一致で十分判別できる)。
pub(crate) fn filter_dead_by_wip_added(
    dead: Vec<crate::models::review::DeadSymbol>,
    added: &[crate::models::review::ApiSymbol],
) -> Vec<crate::models::review::DeadSymbol> {
    if added.is_empty() {
        return dead;
    }
    let added_set: std::collections::HashSet<(&str, &str)> = added
        .iter()
        .map(|a| (a.file.as_str(), a.name.as_str()))
        .collect();
    dead.into_iter()
        .filter(|ds| !added_set.contains(&(ds.file.as_str(), ds.name.as_str())))
        .collect()
}

/// `sym` の range を内包する最も内側の container (class/struct/trait/interface/enum) を返す。
/// `sym` 自身は除外する。
pub(crate) fn enclosing_container<'a>(
    sym: &crate::models::symbol::Symbol,
    containers: &'a [&'a crate::models::symbol::Symbol],
) -> Option<&'a crate::models::symbol::Symbol> {
    containers
        .iter()
        .copied()
        .filter(|c| symbol_range_contains(c, sym) && !symbol_range_equal(c, sym))
        .min_by_key(|c| {
            (
                c.range.end.line.saturating_sub(c.range.start.line),
                c.range.end.column.saturating_sub(c.range.start.column),
            )
        })
}

fn symbol_range_contains(
    outer: &crate::models::symbol::Symbol,
    inner: &crate::models::symbol::Symbol,
) -> bool {
    let starts_before_or_at = outer.range.start.line < inner.range.start.line
        || (outer.range.start.line == inner.range.start.line
            && outer.range.start.column <= inner.range.start.column);
    let ends_after_or_at = outer.range.end.line > inner.range.end.line
        || (outer.range.end.line == inner.range.end.line
            && outer.range.end.column >= inner.range.end.column);
    starts_before_or_at && ends_after_or_at
}

fn symbol_range_equal(
    a: &crate::models::symbol::Symbol,
    b: &crate::models::symbol::Symbol,
) -> bool {
    a.range.start.line == b.range.start.line
        && a.range.start.column == b.range.start.column
        && a.range.end.line == b.range.end.line
        && a.range.end.column == b.range.end.column
}

// ---------------------------------------------------------------------------
// Dead-code コマンド: diff 関連 or プロジェクト全体のデッドコード検出
// ---------------------------------------------------------------------------

/// `dead-code` の既定除外ディレクトリ名。
///
/// 大規模リポでは `vendor/`, `node_modules/`, `tests/` 等が `dead-code` 候補の 88%+ を占め、
/// 実運用のノイズになる。ディレクトリ名と完全一致するセグメントをパスに含むファイルを
/// 走査対象から落とす。`--include-vendor` / `--include-tests` / `--include-build` で
/// 個別に再取込できる。
///
/// グループ化の意図:
/// - `vendor`: Composer, Ruby Bundler, Go modules vendor
/// - `node_modules`, `bower_components`: Node パッケージ
/// - `tests`, `Tests`, `__tests__`, `spec`, `testdata`,
///   `test`, `androidTest`, `sharedTest`, `integrationTest`: 言語共通 + JVM/Gradle のテストディレクトリ
///   (実体は `TEST_DIRECTORY_SEGMENTS` 定数で `is_test_path` と共有)
/// - `target`, `dist`, `build`, `out`, `_build`, `cmake-build-debug`, `cmake-build-release`: ビルド成果物
/// - `.venv`, `venv`, `.tox`: Python 仮想環境
pub(crate) const DEFAULT_DEAD_CODE_EXCLUDES_VENDOR: &[&str] = &[
    "vendor",
    "node_modules",
    "bower_components",
    ".venv",
    "venv",
    ".tox",
];
/// dead-code 既定除外のテストディレクトリ。`is_test_path` と同じセグメント集合
/// (`TEST_DIRECTORY_SEGMENTS`) を使い、API 検出側と dead-code 側のテスト判定を統一する。
pub(crate) const DEFAULT_DEAD_CODE_EXCLUDES_TESTS: &[&str] = TEST_DIRECTORY_SEGMENTS;
pub(crate) const DEFAULT_DEAD_CODE_EXCLUDES_BUILD: &[&str] = &[
    "target",
    "dist",
    "build",
    "out",
    "_build",
    "cmake-build-debug",
    "cmake-build-release",
];

/// 現在のフラグ設定から除外ディレクトリリストを組み立てる。
pub(crate) fn resolve_dead_code_excludes(
    include_vendor: bool,
    include_tests: bool,
    include_build: bool,
) -> Vec<&'static str> {
    let mut excludes: Vec<&'static str> = Vec::new();
    if !include_vendor {
        excludes.extend(DEFAULT_DEAD_CODE_EXCLUDES_VENDOR);
    }
    if !include_tests {
        excludes.extend(DEFAULT_DEAD_CODE_EXCLUDES_TESTS);
    }
    if !include_build {
        excludes.extend(DEFAULT_DEAD_CODE_EXCLUDES_BUILD);
    }
    excludes
}

/// Laravel 規約プリセット。フレームワークが自動で呼び出す規約的エントリポイントを除外する。
///
/// - `database/migrations/**`: Artisan `migrate` から `up()` / `down()` を呼ぶ
/// - `database/seeds/**` / `database/seeders/**` / `database/factories/**`: Artisan `db:seed` が `run()` を呼ぶ
/// - `database/views/**`: DB view 定義 (Artisan 駆動)
/// - `app/Console/Commands/**`: `handle()` が Artisan から呼ばれる
/// - `app/Http/Controllers/**`: Route 定義 (`routes/web.php` 等) から文字列経由で呼ばれる
/// - `app/Http/Middleware/**`: `handle()` が Route/Kernel 経由で呼ばれる
/// - `app/Http/Requests/**`: `authorize()` / `rules()` が Form Request 解決時に自動呼出し
/// - `app/Http/Resources/**`: `toArray()` が Response serialization で呼ばれる
/// - `app/GraphQL/**`: GraphQL schema ファイルから文字列経由で解決される
/// - `app/Listeners/**`, `app/Providers/**`: Service Container / Event Bus 経由
/// - `_ide_helper*.php`, `.phpstorm.meta.php`: IDE 補助の自動生成ファイル
///
/// `**/` 接頭辞でサブディレクトリに埋め込まれた Laravel アプリ（モノレポ内の複数 Laravel
/// 等）にも対応する。
pub(crate) const LARAVEL_PRESET_EXCLUDE_GLOBS: &[&str] = &[
    // 標準マイグレーション経路 (Artisan 駆動)
    "**/database/migrations/**",
    // Multi-DB / 複数コネクション構成で派生する migrations_foo, migrations-foo
    // (Laravel 公式 ドキュメントの `--path` 指定パターン) も同様に Artisan 駆動
    "**/database/migrations_*/**",
    "**/database/migrations-*/**",
    // シーダー / ファクトリ / ビュー定義 / テーブル定義スナップショット
    "**/database/seeds/**",
    "**/database/seeders/**",
    "**/database/factories/**",
    "**/database/views/**",
    "**/database/TableDefinitions/**",
    // Artisan / Route / GraphQL 経由で呼ばれるエントリポイント
    "**/app/Console/Commands/**",
    "**/app/Http/Controllers/**",
    "**/app/Http/Middleware/**",
    "**/app/Http/Requests/**",
    "**/app/Http/Resources/**",
    "**/app/GraphQL/**",
    "**/app/Listeners/**",
    "**/app/Providers/**",
    // bootstrap/app.php で ExceptionHandler 規約で登録されるハンドラ
    "**/app/Exceptions/**",
    // Service Container / Observer / Cast / Policy / Event / Queue / Mail / Notification /
    // Broadcast channel / FormRequest validation Rule — いずれも Laravel のフレームワーク側が
    // reflection / 文字列 FQN / 自動ディスパッチで呼び出す規約的エントリポイント群
    "**/app/Casts/**",
    "**/app/Observers/**",
    "**/app/Policies/**",
    "**/app/Events/**",
    "**/app/Jobs/**",
    "**/app/Notifications/**",
    "**/app/Mail/**",
    "**/app/Rules/**",
    "**/app/Broadcasting/**",
    // IDE 補助の自動生成ファイル
    "**/_ide_helper.php",
    "**/_ide_helper_models.php",
    "**/.phpstorm.meta.php",
];

/// Next.js (App Router / Pages Router) のフレームワーク entrypoint プリセット。
///
/// Next.js のファイルシステムルーティングでは、特定のファイル名 (`page` / `layout` /
/// `route` 等) の default export が Next.js ランタイム経由で呼ばれる。AST 上の
/// cross-file refs では caller を追跡できないため、astro-sight 単独では
/// `dead-code` の偽陽性源になる。`--framework nextjs` でこれらを除外する。
///
/// - **App Router** (Next.js 13+): `app/**/page.*`, `layout.*`, `loading.*`, `error.*`,
///   `not-found.*`, `template.*`, `default.*`, `global-error.*`, `route.*`
/// - **Pages Router** (legacy): `pages/**/*.{js,jsx,ts,tsx}` (含む `pages/api/**`)
/// - **Root entrypoints**: `middleware.{js,ts}`, `instrumentation.{js,ts}`
///
/// `src/app/**` のような src layout もそのまま `**/app/**` のグロブでカバーされる。
pub(crate) const NEXTJS_PRESET_EXCLUDE_GLOBS: &[&str] = &[
    // App Router 規約ファイル
    "**/app/**/page.{js,jsx,ts,tsx}",
    "**/app/**/layout.{js,jsx,ts,tsx}",
    "**/app/**/loading.{js,jsx,ts,tsx}",
    "**/app/**/error.{js,jsx,ts,tsx}",
    "**/app/**/not-found.{js,jsx,ts,tsx}",
    "**/app/**/template.{js,jsx,ts,tsx}",
    "**/app/**/default.{js,jsx,ts,tsx}",
    "**/app/**/global-error.{js,jsx,ts,tsx}",
    "**/app/**/route.{js,jsx,ts,tsx}",
    // Pages Router (legacy)
    "**/pages/**/*.{js,jsx,ts,tsx}",
    // Root entrypoints
    "**/middleware.{js,ts}",
    "**/instrumentation.{js,ts}",
];

/// `resolve_framework_globs` の auto-detect 対応版。
///
/// 呼び出し側で `framework` が明示指定されていれば従来通り `resolve_framework_globs` に
/// 委譲する。未指定の場合は `dir` 直下の `package.json` を読んで `next` 依存を検出し、
/// 見つかれば `"nextjs"` プリセットを適用する。明示指定が auto detect より常に優先される。
///
/// 自動検出に失敗した場合 (package.json なし、JSON パース失敗、依存不一致) は空 Vec を
/// 返す。debug ログを出さない (副作用最小化のため、検出結果は呼び出し側の review JSON 等で
/// 表現する余地を残す)。
pub(crate) fn resolve_framework_globs_with_auto_detect(
    framework: Option<&str>,
    dir: &str,
) -> Result<Vec<String>> {
    if framework.is_some() {
        return resolve_framework_globs(framework);
    }
    match auto_detect_framework(dir) {
        Some(name) => resolve_framework_globs(Some(name)),
        None => Ok(Vec::new()),
    }
}

/// `dir/package.json` を読んで Next.js プロジェクトかを判定する。
///
/// 判定: `package.json` の `dependencies` または `devDependencies` に `next` キーが存在
/// すること。`peerDependencies` / `optionalDependencies` は Next.js ライブラリやテスト
/// fixture で誤爆しやすいため対象外。
///
/// 失敗時 (ファイル無し / JSON パース失敗 / next 依存なし) は `None` を返す。
///
/// モノレポでの workspace 走査は将来対応 (初期実装は root `package.json` のみ)。
pub(crate) fn auto_detect_framework(dir: &str) -> Option<&'static str> {
    let pkg_path = std::path::Path::new(dir).join("package.json");
    let text = std::fs::read_to_string(&pkg_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let has_next = ["dependencies", "devDependencies"].iter().any(|field| {
        value
            .get(field)
            .and_then(|v| v.as_object())
            .is_some_and(|deps| deps.contains_key("next"))
    });
    if has_next { Some("nextjs") } else { None }
}

/// フレームワーク名から対応する除外 glob プリセットを返す。
/// 未知のフレームワーク名はエラー。
///
/// `**/app/X/**` / `**/database/X/**` のような app-prefix 付きパターンには、
/// `**/X/**` という prefix 省略版も自動で追加する。これにより以下が同時にカバーされる:
/// - `--dir <project>/app` のように `app/` 直下を指した場合の fallback
/// - `app/` を別名 (例: `core/`) にリネームしている独自レイアウト
/// - Laravel 配下に複数 module を抱えるモノレポ (`<root>/<sub>/Http/Controllers/...`)
///
/// 過剰除外の懸念: `**/Http/**` の類は Laravel 規約以外でも使われ得るが、
/// 既定除外に `vendor/` / `node_modules/` 等のサードパーティ配下が入っており、
/// なおかつ `--framework laravel` を指定しているのは Laravel プロジェクトのみという
/// 前提なので、実用上の誤マッチはほぼ発生しない。
pub(crate) fn resolve_framework_globs(framework: Option<&str>) -> Result<Vec<String>> {
    match framework {
        None => Ok(Vec::new()),
        Some(name) => match name.to_ascii_lowercase().as_str() {
            "laravel" => {
                let mut globs: Vec<String> =
                    Vec::with_capacity(LARAVEL_PRESET_EXCLUDE_GLOBS.len() * 2);
                for pat in LARAVEL_PRESET_EXCLUDE_GLOBS {
                    globs.push((*pat).to_string());
                    // app/database prefix の省略版を並列で登録 (--dir が app/ 直下の場合の fallback、
                    // および Laravel 標準外レイアウトへの自動対応)
                    if let Some(rest) = pat
                        .strip_prefix("**/app/")
                        .or_else(|| pat.strip_prefix("**/database/"))
                    {
                        globs.push(format!("**/{rest}"));
                    }
                }
                Ok(globs)
            }
            "nextjs" | "next" => {
                // Next.js は `app/` と `pages/` が予約ディレクトリ名で、`src/app/`
                // / `src/pages/` レイアウトも `**/app/**` / `**/pages/**` グロブで
                // そのままカバーされるため prefix 省略形は不要。
                // むしろ `**/pages/**/*.{js,jsx,ts,tsx}` の省略形は
                // `**/*.{js,jsx,ts,tsx}` となり全 TS/JS ファイルを誤除外するので
                // Laravel と異なり省略形を生成しない。
                Ok(NEXTJS_PRESET_EXCLUDE_GLOBS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect())
            }
            other => Err(AstroError::new(
                ErrorCode::InvalidRequest,
                format!("Unknown framework preset: {other} (supported: laravel, nextjs)"),
            )
            .into()),
        },
    }
}

/// 指定パスが既定除外対象のディレクトリセグメントを含むかを判定する。
pub(crate) fn path_is_default_excluded(path: &str, excludes: &[&str]) -> bool {
    if excludes.is_empty() {
        return false;
    }
    path.split('/').any(|seg| excludes.contains(&seg))
}

/// `diff_files` を dead-code 検出対象に絞り込む共通ヘルパー。
/// `cmd_dead_code` と `cmd_review` の両者から呼び、除外ロジックを一元化する。
///
/// - `excludes`: 既定除外ディレクトリ名 (vendor / tests / build 等、呼び出し側で合成済み)
/// - `combined_exclude_globs`: framework プリセット + ユーザ指定 `--exclude-glob` を合成したパターン列
/// - `glob`: positive glob フィルタ。指定時は whitelist されたもののみ残す。
pub(crate) fn filter_diff_files_for_dead_code(
    canonical_dir: &std::path::Path,
    diff_files: &[crate::models::impact::DiffFile],
    excludes: &[&str],
    combined_exclude_globs: &[&str],
    glob: Option<&str>,
) -> Result<Vec<std::path::PathBuf>> {
    // 除外判定は workspace 相対の new_path で行う。canonical_dir に `test` 等の
    // 親セグメントが含まれているケース (例: `/private/tmp/test/myrepo`) でも、
    // リポ内の `src/foo.rs` のような non-test ファイルを誤って除外しないようにするため。
    let mut files: Vec<std::path::PathBuf> = diff_files
        .iter()
        .filter(|f| f.new_path != "/dev/null")
        // diff の new_path は信頼境界外。絶対パスやトラバーサル成分を含むパスは
        // canonical_dir.join() で workspace 外を指してしまうため、ここで弾く。
        .filter(|f| crate::engine::impact::is_safe_diff_path(&f.new_path))
        .filter(|f| !path_is_default_excluded(&f.new_path, excludes))
        .map(|f| canonical_dir.join(&f.new_path))
        .filter(|p| {
            crate::language::LangId::from_path(camino::Utf8Path::new(p.to_str().unwrap_or("")))
                .is_ok()
        })
        .collect();

    if glob.is_some() || !combined_exclude_globs.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(canonical_dir);
        if let Some(pattern) = glob {
            ob.add(pattern)?;
        } else {
            ob.add("**/*")?;
        }
        for pat in combined_exclude_globs {
            let negated = if pat.starts_with('!') {
                (*pat).to_string()
            } else {
                format!("!{pat}")
            };
            ob.add(&negated)?;
        }
        let overrides = ob.build()?;
        files.retain(|p| !overrides.matched(p, false).is_ignore());
        // glob が指定されているときは「whitelist に明示マッチ」だけを残す。
        // `Match::None` (どのパターンにもマッチしない) を許可すると、
        // `--glob '**/*.py'` のような絞り込みでも Rust ファイル等が残ってしまう。
        if glob.is_some() {
            files.retain(|p| overrides.matched(p, false).is_whitelist());
        }
    }
    Ok(files)
}

/// PHPUnit 命名規約に合致するシンボルかどうかを判定する。
///
/// PHP プロジェクト (Laravel を含む) ではテストメソッドは `public function testXxx` や
/// `setUp` / `tearDown` / `setUpBeforeClass` / `tearDownAfterClass` 等、PHPUnit が自動で
/// 呼び出す規約的メソッドが大半。識別子レベルの cross-file ref は生じないが dead ではない。
///
/// 同じ規約は JUnit / NUnit / MSTest でも使われるが誤判定を避けるため、本判定は PHP
/// ファイルに限定する。
pub(crate) fn is_phpunit_test_symbol(
    name: &str,
    kind: crate::models::symbol::SymbolKind,
    lang_id: crate::language::LangId,
) -> bool {
    use crate::language::LangId;
    use crate::models::symbol::SymbolKind;
    if lang_id != LangId::Php {
        return false;
    }
    // qualname (`Foo.testBar`) の末尾要素を取る
    let short = name.rsplit_once('.').map(|(_, t)| t).unwrap_or(name);
    match kind {
        SymbolKind::Class => is_phpunit_test_class_name(short),
        SymbolKind::Method | SymbolKind::Function => {
            matches!(
                short,
                "setUp" | "tearDown" | "setUpBeforeClass" | "tearDownAfterClass"
            ) || is_phpunit_test_method_name(short)
        }
        _ => false,
    }
}

/// PHPUnit テストクラス名規約 (`*Test` / `*TestCase` / `*IntegrationTest` / `*FeatureTest`)
/// に合致するか判定する。test_only_symbols 振り分け時に container 名と突き合わせる。
pub(crate) fn is_phpunit_test_class_name(name: &str) -> bool {
    name.ends_with("Test")
        || name.ends_with("TestCase")
        || name.ends_with("IntegrationTest")
        || name.ends_with("FeatureTest")
}

/// PHP シンボルの qualname container が PHPUnit テストクラス規約に合致するか判定する。
///
/// 該当メソッドは同一クラス内の self::/static::/$this-> ヘルパー、または @dataProvider /
/// @depends / #[DataProvider] 経由で reflection 呼び出しされる helper である可能性が高く、
/// test_only_symbols としてレポートしても「テストランナーが内部で使うだけのノイズ」になる
/// ため、test_only からも除外するために使う。
fn has_phpunit_test_container(name: &str, lang: crate::language::LangId) -> bool {
    if lang != crate::language::LangId::Php {
        return false;
    }
    let Some((container, _)) = name.rsplit_once('.') else {
        return false;
    };
    let container_short = container
        .rsplit_once('.')
        .map(|(_, t)| t)
        .unwrap_or(container);
    is_phpunit_test_class_name(container_short)
}

/// `^test[A-Z_]` で始まるメソッド名かどうか (PHPUnit の testXxx 規約)。
pub(crate) fn is_phpunit_test_method_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() <= 4 {
        return false;
    }
    if &bytes[..4] != b"test" {
        return false;
    }
    let c = bytes[4];
    c.is_ascii_uppercase() || c == b'_'
}

/// `unittest.TestCase` / `unittest.IsolatedAsyncioTestCase` を直接示す base 名集合。
pub(crate) const PYTHON_UNITTEST_ROOT_BASES: &[&str] = &[
    "TestCase",
    "unittest.TestCase",
    "IsolatedAsyncioTestCase",
    "unittest.IsolatedAsyncioTestCase",
];

/// 同一ファイル内の Python クラスについて、`unittest.TestCase` 系を直接/間接継承する
/// クラス名集合を fixed-point で解決して返す。クロスファイル継承は対象外。
///
/// 例: `class Base(unittest.TestCase): ...` と `class Child(Base): ...` の両方を拾う。
pub(crate) fn collect_python_unittest_classes(
    syms: &[crate::models::symbol::Symbol],
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lang_id: crate::language::LangId,
) -> std::collections::HashSet<String> {
    use crate::models::symbol::SymbolKind;
    let mut unittest_classes: std::collections::HashSet<String> = std::collections::HashSet::new();
    if lang_id != crate::language::LangId::Python {
        return unittest_classes;
    }

    // (クラス名, 解決待ち base 名のリスト) のペアを集める。
    let mut class_bases: Vec<(String, Vec<String>)> = Vec::new();
    for sym in syms {
        if !matches!(sym.kind, SymbolKind::Class) {
            continue;
        }
        let bases = crate::engine::symbols::python_class_base_names(root, source, &sym.range);
        // 直接 root base を継承していれば即座に確定。
        if bases
            .iter()
            .any(|b| PYTHON_UNITTEST_ROOT_BASES.contains(&b.as_str()))
        {
            unittest_classes.insert(sym.name.clone());
            continue;
        }
        // それ以外は候補として保留し、後段で fixed-point 解決する。
        class_bases.push((sym.name.clone(), bases));
    }

    // 同一ファイル内の Base → Child チェーンを fixed-point で広げる。
    loop {
        let mut changed = false;
        let mut idx = 0;
        while idx < class_bases.len() {
            let inherited = class_bases[idx]
                .1
                .iter()
                .any(|b| unittest_classes.contains(b.as_str()));
            if inherited {
                let (name, _) = class_bases.swap_remove(idx);
                unittest_classes.insert(name);
                changed = true;
            } else {
                idx += 1;
            }
        }
        if !changed {
            break;
        }
    }

    unittest_classes
}

/// ファイル名が pytest のモジュール命名規約 (`test_*.py` または `*_test.py`) に
/// 一致するかを判定する。`conftest.py` は別関数で判定する。
pub(crate) fn file_name_is_pytest_module(file_path: Option<&str>) -> bool {
    let Some(path) = file_path else {
        return false;
    };
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !file_name.ends_with(".py") {
        return false;
    }
    // conftest.py は別ハンドリング。
    if file_name == "conftest.py" {
        return false;
    }
    if file_name.starts_with("test_") {
        return true;
    }
    // `*_test.py` 規約 (ファイル名が `_test.py` で終わる)。
    let stem = file_name.trim_end_matches(".py");
    stem.ends_with("_test") && stem.len() > "_test".len()
}

/// ファイル名が pytest の `conftest.py` かどうかを判定する。
pub(crate) fn file_name_is_python_conftest(file_path: Option<&str>) -> bool {
    let Some(path) = file_path else {
        return false;
    };
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        == Some("conftest.py")
}

/// `unittest` / pytest のテスト規約に該当する Python シンボルかを判定する。
///
/// 対象:
/// - `unittest.TestCase` 直接/間接継承クラス (同一ファイル内のチェーン)
/// - そのクラス配下の `test_*` メソッドおよび `setUp` / `tearDown` /
///   `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup`
/// - `test_*.py` / `*_test.py` のトップレベル `test_*` 関数 (pytest 規約)
/// - `conftest.py` 内のすべての関数 (pytest フィクスチャ規約)
pub(crate) fn is_python_test_symbol(
    name: &str,
    kind: crate::models::symbol::SymbolKind,
    lang_id: crate::language::LangId,
    file_path: Option<&str>,
    container: Option<&str>,
    unittest_classes: &std::collections::HashSet<String>,
) -> bool {
    use crate::language::LangId;
    use crate::models::symbol::SymbolKind;
    if lang_id != LangId::Python {
        return false;
    }

    // qualname (`Foo.test_bar`) の場合は末尾要素を取り出して container を補正する。
    let (short, qual_container) = match name.rsplit_once('.') {
        Some((head, tail)) => (tail, Some(head)),
        None => (name, None),
    };
    let effective_container = container.or(qual_container);

    if matches!(kind, SymbolKind::Class) {
        return unittest_classes.contains(short);
    }

    if !matches!(kind, SymbolKind::Function | SymbolKind::Method) {
        return false;
    }

    // conftest.py 内の関数はすべて pytest 規約で参照されうる。
    if file_name_is_python_conftest(file_path) && effective_container.is_none() {
        return true;
    }

    // `test_*.py` / `*_test.py` のトップレベル `test_*` 関数は pytest が discover する。
    if file_name_is_pytest_module(file_path)
        && effective_container.is_none()
        && short.starts_with("test_")
    {
        return true;
    }

    // unittest.TestCase 派生クラス配下のメソッド。
    if let Some(class_name) = effective_container
        && unittest_classes.contains(class_name)
    {
        return short.starts_with("test_")
            || matches!(
                short,
                "setUp"
                    | "tearDown"
                    | "setUpClass"
                    | "tearDownClass"
                    | "asyncSetUp"
                    | "asyncTearDown"
                    | "addCleanup"
                    | "addClassCleanup"
                    | "addAsyncCleanup"
            );
    }

    false
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_dead_code(
    dir: &str,
    glob: Option<&str>,
    diff: Option<&str>,
    diff_file: Option<&str>,
    git: bool,
    base: &str,
    staged: bool,
    include_vendor: bool,
    include_tests: bool,
    include_build: bool,
    framework: Option<&str>,
    extra_exclude_dirs: &[String],
    extra_exclude_globs: &[String],
    pretty: bool,
    dead_scope: crate::cli::DeadScope,
) -> Result<()> {
    let canonical_dir = std::fs::canonicalize(dir)?;
    if !canonical_dir.is_dir() {
        return Err(
            AstroError::new(ErrorCode::InvalidRequest, format!("Not a directory: {dir}")).into(),
        );
    }

    let default_excludes = resolve_dead_code_excludes(include_vendor, include_tests, include_build);
    let mut excludes: Vec<&str> = default_excludes.to_vec();
    for name in extra_exclude_dirs {
        excludes.push(name.as_str());
    }

    // glob 除外: フレームワークプリセット + ユーザ指定
    // 未指定時は package.json から next 依存を検出して nextjs プリセットを自動適用する。
    let framework_globs = resolve_framework_globs_with_auto_detect(framework, dir)?;
    let mut combined_globs: Vec<&str> = framework_globs.iter().map(String::as_str).collect();
    for pat in extra_exclude_globs {
        combined_globs.push(pat.as_str());
    }

    // diff 指定があれば diff 関連ファイルのみ、なければプロジェクト全体
    let has_diff = diff.is_some() || diff_file.is_some() || git;
    // diff_input / diff_files は touched-symbols filter でも使うため、ここで一度だけ
    // 取得・parse して再利用する (旧実装は run_git_diff + parse_unified_diff を 2 回呼んでおり、
    // --staged 実行中の git add で 2 つの diff が乖離する競合状態があった)。
    let (diff_input, diff_files): (Option<String>, Option<Vec<crate::models::impact::DiffFile>>) =
        if has_diff {
            let input = if let Some(d) = diff {
                d.to_string()
            } else if let Some(df) = diff_file {
                read_file_to_string_limited(df, MAX_INPUT_SIZE)?
            } else {
                // git 経路 (diff/diff_file なし + has_diff): 管理外なら
                // 空の dead_symbols + skipped で exit 0。
                match resolve_git_diff(dir, base, staged)? {
                    GitDiffInput::Diff(s) => s,
                    GitDiffInput::Skipped(skip) => {
                        let result = DeadCodeResult {
                            dir: canonical_dir.to_string_lossy().to_string(),
                            scanned_files: 0,
                            dead_symbols: Vec::new(),
                            test_only_symbols: Vec::new(),
                            skipped: Some(skip),
                        };
                        let output = serialize_output(&result, pretty)?;
                        println!("{output}");
                        return Ok(());
                    }
                }
            };

            if input.trim().is_empty() {
                let result = DeadCodeResult {
                    dir: canonical_dir.to_string_lossy().to_string(),
                    scanned_files: 0,
                    dead_symbols: Vec::new(),
                    test_only_symbols: Vec::new(),
                    skipped: None,
                };
                let output = serialize_output(&result, pretty)?;
                println!("{output}");
                return Ok(());
            }

            let parsed = crate::engine::diff::parse_unified_diff(&input);
            (Some(input), Some(parsed))
        } else {
            (None, None)
        };

    let files: Vec<std::path::PathBuf> = if let Some(diff_files) = diff_files.as_ref() {
        filter_diff_files_for_dead_code(
            &canonical_dir,
            diff_files,
            &excludes,
            &combined_globs,
            glob,
        )?
    } else {
        crate::engine::refs::collect_files_with_excludes(
            &canonical_dir,
            glob,
            &excludes,
            &combined_globs,
        )?
    };

    let scanned_files = files.len();
    let (dead_symbols, test_only_symbols) = detect_dead_symbols_from_files(dir, &files);

    // dead-scope=touched-symbols: --git/--diff 指定時のみ意味を持つ。
    // diff の追加行情報が必要なので、has_diff のときだけ適用する。
    let dead_symbols = if matches!(dead_scope, crate::cli::DeadScope::TouchedSymbols)
        && let (Some(diff_input), Some(diff_files)) = (diff_input.as_deref(), diff_files.as_ref())
    {
        filter_dead_by_touched_symbols(dir, dead_symbols, diff_input, diff_files)
    } else {
        dead_symbols
    };

    let result = DeadCodeResult {
        dir: canonical_dir.to_string_lossy().to_string(),
        scanned_files,
        dead_symbols,
        test_only_symbols,
        skipped: None,
    };

    let output = serialize_output(&result, pretty)?;
    info!(
        command = "dead-code",
        dir = dir,
        scanned_files = scanned_files,
        dead_count = result.dead_symbols.len(),
        "command completed"
    );
    println!("{output}");
    Ok(())
}
