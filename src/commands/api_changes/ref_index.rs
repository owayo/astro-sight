//! API 差分判定用の cross-file 参照インデックス (`ApiRefIndex`) と、
//! 参照状況から api.add / api.rm / api.mod の扱いを決める判定ヘルパー。

use std::collections::{HashMap, HashSet};

use crate::engine::parser;
use crate::service::AppService;

use super::super::git_input::{git_show_blob, validate_git_revision};
use super::{PreparedDiffFile, bare_name};

/// Old-side references used only to distinguish a rename from an already unused removal.
/// All changed sources are parsed once, and references inside declarations removed by the
/// same diff are excluded: a recursive method or a deleted caller cannot prove a rename.
pub(crate) struct OldRefIndex {
    references: HashMap<String, Vec<crate::models::reference::SymbolReference>>,
    removed_ranges: HashMap<String, Vec<crate::models::location::Range>>,
}

impl OldRefIndex {
    pub(crate) fn build(
        names: &HashSet<String>,
        diff_files: &[crate::models::impact::DiffFile],
        prepared: &[PreparedDiffFile],
        base_blobs: &crate::commands::git_input::GitBlobBatch,
    ) -> Self {
        let mut sorted: Vec<String> = names.iter().cloned().collect();
        sorted.sort_unstable();
        let mut index = Self {
            references: HashMap::new(),
            removed_ranges: HashMap::new(),
        };
        let Ok(batch) = crate::engine::refs::SourceReferenceBatch::new(&sorted) else {
            return index;
        };
        for (df, prep) in diff_files.iter().zip(prepared) {
            if df.old_path == "/dev/null" || df.new_path == "/dev/null" {
                continue;
            }
            let PreparedDiffFile::Modified {
                old_syms: Some(old_syms),
                new_syms: Some(new_syms),
                ..
            } = prep
            else {
                continue;
            };
            let Some(source) = base_blobs.read(&df.old_path) else {
                continue;
            };
            let Ok(scan) = batch.scan(camino::Utf8Path::new(&df.old_path), &source) else {
                continue;
            };
            let surviving: HashSet<&str> =
                new_syms.iter().map(|(name, _, _)| name.as_str()).collect();
            let removed: HashSet<&str> = old_syms
                .iter()
                .map(|(name, _, _)| name.as_str())
                .filter(|name| !surviving.contains(name))
                .collect();
            let matched: Vec<_> = scan
                .symbols
                .iter()
                .filter_map(|symbol| {
                    let qualified = symbol.container.as_ref().map_or_else(
                        || symbol.name.clone(),
                        |container| format!("{container}.{}", symbol.name),
                    );
                    removed
                        .contains(qualified.as_str())
                        .then_some((qualified, symbol.range))
                })
                .collect();
            // Missing declaration ranges make attribution uncertain. Keep the removal visible.
            if removed
                .iter()
                .any(|name| !matched.iter().any(|(matched_name, _)| matched_name == name))
            {
                continue;
            }
            let ranges = matched.into_iter().map(|(_, range)| range).collect();
            index.removed_ranges.insert(df.old_path.clone(), ranges);
            for (name, refs) in sorted.iter().zip(scan.references) {
                index
                    .references
                    .entry(name.clone())
                    .or_default()
                    .extend(refs);
            }
        }
        index
    }

    pub(crate) fn has_surviving_old_reference(&self, name: &str) -> bool {
        use crate::models::reference::RefKind;
        self.references.get(bare_name(name)).is_some_and(|refs| {
            refs.iter().any(|reference| {
                if reference.kind == Some(RefKind::Definition) {
                    return false;
                }
                let position = (reference.line, reference.column);
                !self
                    .removed_ranges
                    .get(&reference.path)
                    .is_some_and(|ranges| {
                        ranges.iter().any(|range| {
                            position >= (range.start.line, range.start.column)
                                && position < (range.end.line, range.end.column)
                        })
                    })
            })
        })
    }
}

/// API 差分判定用の cross-file 参照インデックス。
///
/// `detect_api_changes` の各判定ヘルパーは候補シンボルごとに `find_references` で
/// 全リポジトリを走査していた (O(候補数 × 全ファイル))。判定対象になりうる name を
/// 前段で収集し、`find_references_batch` を chunk 単位で呼んで 1 つのインデックスに
/// 集約することで、走査回数を O(ceil(候補数 / chunk)) に抑える
/// (`partition_removed_dead_candidates` と同じ手法)。
///
/// 検索に失敗した chunk の name と未収集の name は `refs_for` が `None` を返し、
/// 各判定ヘルパーが per-symbol 時代の検索失敗時と同じ保守側ポリシーに倒す
/// (false negative を起こさない)。
pub(crate) struct ApiRefIndex {
    /// 検索キー (exact name / bare name) → HEAD ツリー全体の参照 (`dir` 相対パス)。
    /// batch 検索済みの name は参照 0 件でもエントリを持つ (「未検索」と区別するため)。
    refs: std::collections::HashMap<String, Vec<crate::models::reference::SymbolReference>>,
    /// batch 検索に失敗した chunk の name 集合 (保守側判定の対象)。
    failed: HashSet<String>,
}

impl ApiRefIndex {
    /// `names` の参照を batch 検索で収集する。`find_references_batch` が内部で名前を
    /// chunk 分割 (既定 64、`ASTRO_SIGHT_REFS_BATCH_CHUNK` で調整、AC trie のメモリ上限)
    /// しつつディレクトリ走査を 1 回に集約するため、ここでは全名を 1 回で渡す。
    pub(crate) fn build(dir: &str, names: &HashSet<String>) -> Self {
        let mut sorted: Vec<String> = names.iter().filter(|n| !n.is_empty()).cloned().collect();
        sorted.sort_unstable();
        let mut index = Self {
            refs: std::collections::HashMap::new(),
            failed: HashSet::new(),
        };
        if sorted.is_empty() {
            return index;
        }
        // 検索に失敗した場合は全 name を failed に倒し、各判定ヘルパーが per-symbol 時代の
        // 検索失敗時と同じ保守側ポリシー (cross_file/blocking → true) に倒れる
        // (false negative を起こさない)。
        let service = AppService::new();
        match service.find_symbol_references_batch(&sorted, dir, None) {
            Ok(results) => {
                for r in results {
                    index.refs.insert(r.symbol, r.references);
                }
            }
            Err(_) => index.failed.extend(sorted),
        }
        index
    }

    /// `name` の参照リスト。検索失敗 / 未収集の name は `None` (呼び出し側で保守側に倒す)。
    pub(crate) fn refs_for(
        &self,
        name: &str,
    ) -> Option<&[crate::models::reference::SymbolReference]> {
        if self.failed.contains(name) {
            return None;
        }
        self.refs.get(name).map(|v| v.as_slice())
    }
}

/// `name` が `file_path` 以外のファイルから参照されているかを判定する。
/// 参照の取得に失敗した場合 (index 構築失敗 / 未収集) は保守的に true（＝外部参照あり
/// とみなす）を返し、modified の除外を抑止する（false positive を恐れて false negative
/// を起こさない方針）。
///
/// 検索は bare 名で行う。qualname (`Container.method`) は identifier ノードに一致せず
/// 恒久的に 0 件となり、「自ファイル内で呼ばれているだけ」で除外が成立してしまう。
/// bare 名の同名他クラスヒットは「参照あり = 除外しない」方向にしか作用しない (保守的)。
///
/// `file_path` は `dir` からの相対パスを想定する。index の参照パスも `dir` 相対なので
/// `Path` 単位で比較する。
pub(crate) fn has_cross_file_refs(index: &ApiRefIndex, file_path: &str, name: &str) -> bool {
    use std::path::Path;

    let Some(refs) = index.refs_for(bare_name(name)) else {
        return true;
    };
    let self_path = Path::new(file_path);
    refs.iter().any(|r| Path::new(r.path.as_str()) != self_path)
}

/// `name` シンボルが値として利用 (`X(...)` / `new X` / `typeof X` / `X.foo` / `X[...]`)
/// されている参照があるかを判定する。JSX タグ利用・import/re-export・定義のみなら false
/// (= 降格可)。解析失敗・判定不能な参照があれば true (= blocking 維持、false negative 回避)。
///
/// 判定は参照ファイルを parse した参照位置のノードの役割で行う。行テキスト (`ctx`) で判定すると、
/// import 文と同じ行に書いた値利用 (`import { X } from "./x"; X();`) を import 行として捨て、
/// 256 バイトで切り詰めた行では後半の直接呼び出し (`<X />…{X()}`) を見落としていた。
/// 別名で import / 再 export された名前 (`import { X as Y }`) の利用は名前索引に現れないので、
/// 1 件でもあれば値利用の可能性ありとする。
pub(crate) fn has_blocking_value_usage(
    index: &ApiRefIndex,
    dir: &str,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    use crate::models::reference::RefKind;
    let bare = bare_name(name);
    let Some(refs) = index.refs_for(bare) else {
        return true;
    };
    for r in refs {
        if r.kind == Some(RefKind::Definition) {
            continue;
        }
        let Some(parsed) = parsed_ref_file(dir, &r.path, caches) else {
            return true;
        };
        let Some(node) = ref_name_node(parsed, bare, r.line, r.column) else {
            return true;
        };
        if !js_reference_is_non_value(node, &parsed.source, bare) {
            return true;
        }
    }
    false
}

/// JS/TS の参照位置が値利用でないか (import 指定子、別名なしの export 指定子、JSX の要素名、
/// 宣言名)。それ以外 (呼び出し・メンバー・`typeof`・`new`・値としての受け渡し) は false。
fn js_reference_is_non_value(node: tree_sitter::Node<'_>, source: &[u8], bare: &str) -> bool {
    match import_role_at(node, source, bare) {
        RefImportRole::Aliased => return false,
        RefImportRole::Plain => return true,
        RefImportRole::Outside => {}
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() == "export_specifier"
        && parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
    {
        // 別名付きの再 export は `import_role_at` が Aliased にしている。
        return true;
    }
    if jsx_name_opening_tag(node, source).is_some() {
        return true;
    }
    matches!(
        parent.kind(),
        "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "abstract_class_declaration"
            | "function_expression"
            | "function"
            | "generator_function"
            | "class"
            | "variable_declarator"
    ) && parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
}

/// 参照の位置が import / use 文の中か、その中で別名を付けているか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefImportRole {
    /// import / use 文と別名付き再 export 指定子の外。
    Outside,
    /// import / use 文の中で、同じ名前で束縛する (`import { run }` / `from m import run`)。
    Plain,
    /// 別名で束縛・公開する (`run as r` / `export { run as execute }`)。別名の利用は名前索引に
    /// 現れないので、名前で集めた参照は全利用者を表さない。
    Aliased,
}

/// import / use 文のノード種別 (`engine::imports` の import 行集合と同じ範囲)。
const IMPORT_STATEMENT_KINDS: &[&str] = &[
    "use_declaration",
    "import_statement",
    "import_from_statement",
    "future_import_statement",
    "import_declaration",
    "namespace_use_declaration",
    "using_directive",
    "import_header",
    "preproc_include",
];

/// `node` (参照の名前) が `ancestor` の部分木に入るか。
fn node_within(node: tree_sitter::Node<'_>, ancestor: tree_sitter::Node<'_>) -> bool {
    ancestor.start_byte() <= node.start_byte() && node.end_byte() <= ancestor.end_byte()
}

/// 参照の名前ノードから祖先を辿り、import / use 文と別名の有無を判定する。
fn import_role_at(node: tree_sitter::Node<'_>, source: &[u8], bare: &str) -> RefImportRole {
    let alias_differs = |alias: tree_sitter::Node<'_>| alias.utf8_text(source).ok() != Some(bare);
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            // JS/TS: `import { run as r }` / `export { run as r }`、Python: `run as r`
            "import_specifier" | "export_specifier" | "aliased_import" => {
                if let Some(alias) = parent.child_by_field_name("alias")
                    && alias_differs(alias)
                    && parent
                        .child_by_field_name("name")
                        .is_some_and(|name| node_within(node, name))
                {
                    return RefImportRole::Aliased;
                }
            }
            // Rust: `use m::run as r;`
            "use_as_clause" => {
                if let Some(alias) = parent.child_by_field_name("alias")
                    && alias_differs(alias)
                    && parent
                        .child_by_field_name("path")
                        .is_some_and(|path| node_within(node, path))
                {
                    return RefImportRole::Aliased;
                }
            }
            // PHP: `use function A\run as r;`
            "namespace_use_clause" => {
                if let Some(alias) = parent.child_by_field_name("alias")
                    && alias_differs(alias)
                    && !node_within(node, alias)
                {
                    return RefImportRole::Aliased;
                }
            }
            // Kotlin: `import a.run as r`
            "import_header" => {
                let mut cursor = parent.walk();
                let aliased = parent.named_children(&mut cursor).any(|child| {
                    child.kind() == "import_alias"
                        && !node_within(node, child)
                        && child.named_child(0).is_none_or(&alias_differs)
                });
                return if aliased {
                    RefImportRole::Aliased
                } else {
                    RefImportRole::Plain
                };
            }
            kind if IMPORT_STATEMENT_KINDS.contains(&kind) => return RefImportRole::Plain,
            _ => {}
        }
        current = parent;
    }
    RefImportRole::Outside
}

/// 参照位置 (0 始まりの行・byte 列) から、テキストが名前と一致する最小のノードを返す。
/// 位置と名前が食い違う (ファイルが変わった・解析できない) 場合は None。
fn ref_name_node<'t>(
    parsed: &'t ParsedRefFile,
    bare: &str,
    line: usize,
    column: usize,
) -> Option<tree_sitter::Node<'t>> {
    let point = tree_sitter::Point { row: line, column };
    let mut node = parsed
        .tree
        .root_node()
        .descendant_for_point_range(point, point)?;
    let case_insensitive = parsed.lang_id == crate::language::LangId::Php;
    loop {
        if node.start_position() == point
            && node.utf8_text(&parsed.source).ok().is_some_and(|text| {
                text == bare || (case_insensitive && text.eq_ignore_ascii_case(bare))
            })
        {
            return Some(node);
        }
        let parent = node.parent()?;
        if parent.start_position() != point {
            return None;
        }
        node = parent;
    }
}

/// 参照ファイルを parse して参照位置の import 上の役割を返す。parse・位置の特定に失敗したら None
/// (呼び出し側は除外も証明もせず、blocking 側に倒す)。
pub(crate) fn ref_import_role(
    dir: &str,
    r: &crate::models::reference::SymbolReference,
    bare: &str,
    caches: &mut ApiClosureCaches,
) -> Option<RefImportRole> {
    let parsed = parsed_ref_file(dir, &r.path, caches)?;
    let node = ref_name_node(parsed, bare, r.line, r.column)?;
    Some(import_role_at(node, &parsed.source, bare))
}

/// JS / TS を埋め込む、refs が解析できないソース。ここにある利用は名前索引に現れない。
const EMBEDDED_JS_SOURCE_EXTENSIONS: &[&str] = &["vue", "svelte", "astro", "mdx"];
/// 埋め込みソース 1 ファイルの読み込み上限。超えるファイルは「どの名前も使い得る」とみなす。
const EMBEDDED_JS_SOURCE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// リポジトリ内の JS / TS を埋め込む解析できないソースの内容。`None` の要素は読めなかった
/// (上限超過・UTF-8 でない) ファイルで、どの名前も使い得るとみなす。
pub(crate) struct EmbeddedJsSources {
    unknown: bool,
    texts: Vec<Option<String>>,
}

impl EmbeddedJsSources {
    fn load(dir: &str) -> Self {
        let root = std::path::Path::new(dir);
        match crate::engine::refs::collect_files_scan(
            root,
            None,
            crate::engine::refs::FileScanOptions::default(),
        ) {
            Ok(collection) => Self::from_collection(&collection),
            Err(_) => Self {
                unknown: true,
                texts: Vec::new(),
            },
        }
    }

    /// 走査済みのファイル集合から、JS / TS を埋め込む解析できないソースを読む。
    pub(crate) fn from_collection(collection: &crate::engine::refs::FileCollection) -> Self {
        let texts = collection
            .unanalyzable_source_paths()
            .iter()
            .filter(|path| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        EMBEDDED_JS_SOURCE_EXTENSIONS
                            .iter()
                            .any(|known| ext.eq_ignore_ascii_case(known))
                    })
            })
            .map(|path| {
                crate::engine::bounded_read::read_utf8_file_limited(
                    path,
                    EMBEDDED_JS_SOURCE_MAX_BYTES,
                )
            })
            .collect();
        Self {
            unknown: false,
            texts,
        }
    }

    /// どれかの埋め込みソースに、いずれかの名前が識別子として現れ得るか。
    pub(crate) fn may_mention(&self, names: &[&str]) -> bool {
        self.unknown
            || self.texts.iter().any(|text| {
                text.as_ref()
                    .is_none_or(|text| names.iter().any(|name| contains_js_word(text, name)))
            })
    }
}

/// `text` に `name` が JS の識別子の単語として現れるか (前後が識別子の文字でない出現)。
pub(crate) fn contains_js_word(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    text.match_indices(name).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + name.len()..].chars().next();
        before.is_none_or(|c| !is_ident(c)) && after.is_none_or(|c| !is_ident(c))
    })
}

/// 解析できない埋め込みソース (`.vue` 等) に `names` のどれかが現れ得るか。1 回の検出で
/// リポジトリの走査と読み込みは 1 回だけ行う。
pub(crate) fn embedded_js_sources_may_mention(
    dir: &str,
    names: &[&str],
    caches: &mut ApiClosureCaches,
) -> bool {
    caches
        .embedded_js_sources
        .get_or_insert_with(|| std::rc::Rc::new(EmbeddedJsSources::load(dir)))
        .may_mention(names)
}

/// 無変更の呼び出し式を閉じる追加証拠。TS は共有 const の必須プロパティ追加、
/// Python はローカル変数のクラスと新しい Protocol の構造的な適合を証明する。
pub(crate) enum ClosureArgEvidence<'a> {
    None,
    TsAddedRequiredProps(&'a super::ts_signature::AddedRequiredObjectProps),
    PythonProtocolParams(&'a super::python_protocol_arg::ProtocolArgumentChange),
}

pub(crate) struct ModifiedClosureInput<'a> {
    pub(crate) index: &'a ApiRefIndex,
    pub(crate) dir: &'a str,
    pub(crate) name: &'a str,
    pub(crate) kind: &'a str,
    pub(crate) base: &'a str,
    pub(crate) target_new_path: &'a str,
    pub(crate) diff_files: &'a [crate::models::impact::DiffFile],
    pub(crate) arg_evidence: ClosureArgEvidence<'a>,
    /// 対象が宣言名と違う公開名 (default export / 別名 export) で公開されている (JS/TS)。
    /// その利用者は宣言名の参照に現れないので、見えた呼び出しが全部更新済みでも閉じない。
    pub(crate) public_name_differs: bool,
}

/// modified シンボルの全 cross-file 参照が同一 diff 内の変更 hunk で追随済みかを判定する。
///
/// 全ての非定義参照が diff_files の変更 hunk (new 範囲) に収まれば、呼び出し側が同一
/// コミットで更新済みとみなし closed-in-diff (informational)。refs 解析失敗 /
/// diff 外 or hunk 外の未証明の参照が 1 つでもあれば false を返し、保守的に blocking 側
/// (通常の api.mod) へ倒す。無変更の呼び出しを閉じる追加証拠は ClosureArgEvidence に渡す
/// (TS: Issue 2026-08-05-api-mod-callers-updated-indirectly、Python: #38)。
///
/// 同名定義が複数ある場合: 変更対象ファイル (`target_new_path`) 内の定義が 1 つに
/// 特定できなければ従来どおり false。他ファイルの同名定義は、JS/TS/TSX のトップレベル
/// 関数 (bare 名) に限り `js_ts_shadow::resolve_reference_binding` で参照単位に解決し、
/// 「同一ファイルの function_declaration に束縛されるローカル呼び出し」だけを判定対象
/// から除外する (Issue 2026-07-12-api-mod-same-diff-informational: export 関数と
/// 別ファイルのローカル同名関数の併存で closed 判定が全滅していた)。Python の method
/// qualname は、他ファイルの同名定義がすべて追加行にあると証明できる場合だけ通す。
/// それ以外の言語・種類は従来どおり blocking に残す。
pub(crate) fn is_modified_closed_in_diff(
    input: ModifiedClosureInput<'_>,
    caches: &mut ApiClosureCaches,
) -> bool {
    let mut diff_files = DiffFileLookup::new(input.diff_files, input.dir);
    let Some(target) = prepare_closure_target(&input, &mut diff_files, caches) else {
        return false;
    };
    // import / use 宣言の参照は signature 変更に追随する必要がない (名前だけの import で、
    // 通常は変更されず context 行に残る) ため除外し、実際の呼び出し参照のみを対象とする。
    // 呼び出し参照が 1 件も無ければ (import のみ / 呼び出しが diff 外にある可能性) closed
    // 扱いにせず blocking。
    // 行頭テキスト判定 (ref_is_import_line) は複数行 grouped use ブロックの継続行
    // (`    a, b, cmd_cochange, ...` のように `use ` で始まらない行) を拾えないため、
    // AST ベースの import 行集合でも除外する。import/use 文内の参照は signature 変更に
    // 追随する必要がない (api.mod 誤検出 2026-05-31: grouped use 継続行を未更新 caller と
    // 誤判定して blocking していた問題への対応)。
    // 2 つのキャッシュは detect_api_changes スコープで生成して全 modified シンボルで共有する。
    // per-symbol の git diff サブプロセス起動と tree-sitter parse を unique file 単位に削減する。
    let call_refs = collect_call_refs(target.refs, input.dir, target.bare, caches);
    if call_refs.is_empty() {
        return false;
    }
    // 全ての呼び出し参照が、diff 内ファイルかつ実際の追加/変更行 (context 行ではない) にあるか。
    // shadow 除外で全参照が消えた場合は「対象 API の caller を 1 件も確認できていない」
    // ため closed にしない (除外前の空チェックだけだと fail-open になる)。空集合に対する
    // `all` は true なので、確認できた参照の数を数えて判定する。
    let mut effective_call_refs = 0usize;
    for r in &call_refs {
        match classify_reference_owner(&input, &target, r, caches) {
            ReferenceOwner::Target => {}
            ReferenceOwner::Other => continue,
            ReferenceOwner::Unknown => return false,
        }
        effective_call_refs += 1;
        if !prove_reference_updated(&input, &target, r, &mut diff_files, caches) {
            return false;
        }
    }
    effective_call_refs > 0
}

/// 閉包判定の対象。変更対象ファイル内の定義が 1 つに特定でき、他ファイルの同名定義の
/// 扱いも決まったときだけ作る。
struct ClosureTarget<'a> {
    bare: &'a str,
    target_lang: Option<crate::language::LangId>,
    /// Python のクラス直下のメソッド (`Class.method`)。
    python_method: bool,
    refs: &'a [crate::models::reference::SymbolReference],
    /// 変更対象ファイル以外にも同名の定義がある。
    has_foreign_defs: bool,
    /// (Python のメソッドで) 他ファイルの同名定義がすべてこの diff の追加行にある。
    python_foreign_defs_updated: bool,
}

/// 呼び出し参照 1 件の帰属。
enum ReferenceOwner {
    /// 変更対象の定義への参照。追随を証明する対象として数える。
    Target,
    /// 別の定義への参照。判定から外す。
    Other,
    /// 帰属を決められない。閉じない。
    Unknown,
}

/// 参照パス → 一致する diff ファイル (diff の並びで最初に一致したもの) のメモ。
/// `diff_path_matches_ref` は一致しない組で canonicalize を 2 回呼ぶため、参照ごとに
/// diff ファイルを先頭から照合し直さず、参照パスごとに 1 回だけ照合する。
struct DiffFileLookup<'a> {
    diff_files: &'a [crate::models::impact::DiffFile],
    dir: &'a str,
    memo: std::collections::HashMap<String, Option<usize>>,
}

impl<'a> DiffFileLookup<'a> {
    fn new(diff_files: &'a [crate::models::impact::DiffFile], dir: &'a str) -> Self {
        Self {
            diff_files,
            dir,
            memo: std::collections::HashMap::new(),
        }
    }

    fn find(&mut self, ref_path: &str) -> Option<&'a crate::models::impact::DiffFile> {
        let index = match self.memo.get(ref_path) {
            Some(index) => *index,
            None => {
                let index = self.diff_files.iter().position(|df| {
                    df.new_path != "/dev/null"
                        && diff_path_matches_ref(&df.new_path, ref_path, self.dir)
                });
                self.memo.insert(ref_path.to_string(), index);
                index
            }
        };
        index.map(|index| &self.diff_files[index])
    }
}

/// 全体の前提を確かめて閉包判定の対象を作る。どれか 1 つでも満たさなければ None (閉じない)。
///
/// 同名定義が複数ある場合: 変更対象ファイル (`target_new_path`) 内の定義が 1 つに
/// 特定できなければ None。他ファイルの同名定義は、JS/TS/TSX のトップレベル関数 (bare 名)
/// なら参照ごとの shadow 解決 ([`classify_reference_owner`]) へ進む。Python の method
/// qualname は、他ファイルの同名定義がすべて追加行にあると証明できる場合だけ通す。
fn prepare_closure_target<'a>(
    input: &ModifiedClosureInput<'a>,
    diff_files: &mut DiffFileLookup<'a>,
    caches: &mut ApiClosureCaches,
) -> Option<ClosureTarget<'a>> {
    use crate::models::reference::RefKind;
    let &ModifiedClosureInput {
        index,
        dir,
        name,
        kind,
        base,
        target_new_path,
        ref arg_evidence,
        public_name_differs,
        ..
    } = input;
    let bare = bare_name(name);
    let target_lang =
        crate::language::LangId::from_path(camino::Utf8Path::new(target_new_path)).ok();
    let python_method = target_lang == Some(crate::language::LangId::Python)
        && kind == "function"
        && name.contains('.');
    if public_name_differs {
        return None;
    }
    let refs = index.refs_for(bare)?;
    // 別名で import / use / 再 export された名前 (`run as r`) の呼び出しは名前索引に現れず、
    // 見えた呼び出しが全部更新済みでも未更新の利用者が残り得る。言語を問わず閉じない。
    if refs.iter().any(|r| {
        r.kind != Some(crate::models::reference::RefKind::Definition)
            && ref_import_role(dir, r, bare, caches) == Some(RefImportRole::Aliased)
    }) {
        return None;
    }
    // `.vue` などの解析できない埋め込みソースにある利用は refs に現れない。名前が現れ得る
    // ソースがあれば、見えた呼び出しだけで追随済みとは言えない。
    if matches!(
        target_lang,
        Some(
            crate::language::LangId::Javascript
                | crate::language::LangId::Typescript
                | crate::language::LangId::Tsx
        )
    ) && embedded_js_sources_may_mention(dir, &[bare], caches)
    {
        return None;
    }
    if matches!(arg_evidence, ClosureArgEvidence::PythonProtocolParams(_))
        && !super::python_protocol_arg::imports_are_unaliased(dir, refs, bare, caches)
    {
        return None;
    }
    // 変更対象ファイル内の定義が 1 つに特定できなければ曖昧なので保守的に blocking。
    let defs: Vec<&crate::models::reference::SymbolReference> = refs
        .iter()
        .filter(|r| r.kind == Some(RefKind::Definition))
        .collect();
    let target_defs: Vec<_> = defs
        .iter()
        .filter(|r| diff_path_matches_ref(target_new_path, &r.path, dir))
        .filter(|r| {
            !python_method
                || super::python_call_arity::definition_belongs_to_method(dir, name, r, caches)
        })
        .copied()
        .collect();
    let target_def_count = target_defs.len();
    if target_def_count != 1 {
        return None;
    }
    // 他ファイルの同名定義がある場合、Python メソッドでは全定義の名前位置がこの
    // diff の追加行にあることを証明できれば、呼び出し側の追随判定へ進む。
    // 定義の帰属は曖昧でも、全候補が更新済みならどの定義へ束縛されてもよい。
    // JS/TS/TSX のトップレベル関数は従来の shadow resolver を使う。
    let has_foreign_defs = defs.len() > target_def_count;
    let python_foreign_defs_updated = has_foreign_defs
        && name.contains('.')
        && target_lang == Some(crate::language::LangId::Python)
        && defs
            .iter()
            .filter(|r| {
                !target_defs.iter().any(|target| {
                    target.path == r.path && target.line == r.line && target.column == r.column
                })
            })
            .all(|r| {
                let Some(parsed) = parsed_ref_file(dir, &r.path, caches) else {
                    return false;
                };
                if parsed.lang_id != crate::language::LangId::Python
                    || parsed.tree.root_node().has_error()
                {
                    return false;
                }
                let Some(df) = diff_files.find(&r.path) else {
                    return false;
                };
                caches
                    .changed_new_lines
                    .entry(df.new_path.clone())
                    .or_insert_with(|| {
                        std::rc::Rc::new(changed_new_lines_for_diff_file(dir, base, df))
                    })
                    .contains(&r.line)
            });
    if has_foreign_defs
        && !python_foreign_defs_updated
        && !python_method
        && (name.contains('.')
            || kind != "function"
            || !matches!(
                target_lang,
                Some(
                    crate::language::LangId::Javascript
                        | crate::language::LangId::Typescript
                        | crate::language::LangId::Tsx
                )
            ))
    {
        return None;
    }
    Some(ClosureTarget {
        bare,
        target_lang,
        python_method,
        refs,
        has_foreign_defs,
        python_foreign_defs_updated,
    })
}

/// 呼び出し参照 1 件が変更対象の定義への参照か、別の定義への参照かを決める。
fn classify_reference_owner(
    input: &ModifiedClosureInput<'_>,
    target: &ClosureTarget<'_>,
    r: &crate::models::reference::SymbolReference,
    caches: &mut ApiClosureCaches,
) -> ReferenceOwner {
    let &ModifiedClosureInput {
        dir,
        name,
        target_new_path,
        ..
    } = input;
    // 他ファイルの同名定義がある場合: 変更対象ファイル以外の参照で、lexical scope が
    // 同一ファイルの function_declaration に束縛されるものは対象 API と無関係な
    // ローカル呼び出しなので除外する。解決失敗・property 位置・function 以外の
    // binding は OtherOrAmbiguous = 除外せず diff の実変更行の確認に掛かる。
    if target.has_foreign_defs
        && !target.python_foreign_defs_updated
        && !target.python_method
        && !diff_path_matches_ref(target_new_path, &r.path, dir)
        && resolve_ref_shadow_binding(dir, r, target.bare, caches)
            == crate::commands::api_changes::js_ts_shadow::LocalResolution::SameFileFunction
    {
        return ReferenceOwner::Other;
    }
    if target.python_method {
        use super::python_call_arity::MethodCallMatch;
        if target.has_foreign_defs && !target.python_foreign_defs_updated {
            match super::python_call_arity::method_reference_owner(
                dir,
                target_new_path,
                name,
                r,
                caches,
            ) {
                MethodCallMatch::Target => {}
                MethodCallMatch::Other => return ReferenceOwner::Other,
                MethodCallMatch::Unknown => return ReferenceOwner::Unknown,
            }
        }
        match super::python_call_arity::changed_method_call_matches_signature(
            dir,
            target_new_path,
            name,
            r,
            caches,
        ) {
            MethodCallMatch::Target => {}
            MethodCallMatch::Other => return ReferenceOwner::Other,
            MethodCallMatch::Unknown => return ReferenceOwner::Unknown,
        }
    }
    ReferenceOwner::Target
}

/// 変更対象への呼び出し参照 1 件が、同一 diff で追随済みだと証明できるか。
///
/// 証拠は次の順に見る。順序を変えると結果が変わる。
/// 1. 引数が変更後の signature に合わなければ、変更行にあっても追随済みではない (JS/TS・Python)。
/// 2. Python の Protocol 引数の証拠は diff 外の参照も閉じる (diff ファイルを探すより先)。
/// 3. 参照のあるファイルが diff に無ければ未更新の可能性がある。
/// 4. 参照行、または参照を callee とする複数行の呼び出しが実変更行にあれば追随済み。
/// 5. 呼び出し式が無変更でも、引数の共有 const の定義側が同一 diff で更新されていれば追随済み。
fn prove_reference_updated(
    input: &ModifiedClosureInput<'_>,
    target: &ClosureTarget<'_>,
    r: &crate::models::reference::SymbolReference,
    diff_files: &mut DiffFileLookup<'_>,
    caches: &mut ApiClosureCaches,
) -> bool {
    let &ModifiedClosureInput {
        dir,
        name,
        kind,
        base,
        target_new_path,
        ref arg_evidence,
        ..
    } = input;
    let bare = target.bare;
    if matches!(
        target.target_lang,
        Some(
            crate::language::LangId::Javascript
                | crate::language::LangId::Typescript
                | crate::language::LangId::Tsx
        )
    ) && kind == "function"
        && !name.contains('.')
        && !super::js_ts_call_arity::changed_call_matches_signature(
            dir,
            target_new_path,
            bare,
            r,
            caches,
        )
    {
        return false;
    }
    if target.target_lang == Some(crate::language::LangId::Python)
        && kind == "function"
        && !name.contains('.')
        && !super::python_call_arity::changed_top_level_call_matches_signature(
            dir,
            target_new_path,
            bare,
            r,
            caches,
        )
    {
        return false;
    }
    if let ClosureArgEvidence::PythonProtocolParams(proof) = arg_evidence
        && proof.closes_reference(dir, r, caches)
    {
        return true;
    }
    let Some(df) = diff_files.find(&r.path) else {
        return false; // diff 外ファイルの参照 → 未更新 caller の可能性
    };
    // HunkInfo の new 範囲は context 行を含むため、git diff から実 `+` 行集合を取得して照合する
    // (codex 指摘: context 行に古い呼び出しが入ると未更新 caller を誤って closed 判定してしまう)。
    let changed = std::rc::Rc::clone(
        caches
            .changed_new_lines
            .entry(df.new_path.clone())
            .or_insert_with(|| std::rc::Rc::new(changed_new_lines_for_diff_file(dir, base, df))),
    );
    if changed.contains(&r.line) {
        return true;
    }
    // 複数行呼び出し (`startRecording({\n  fps,\n  cursor,\n})`) では識別子行は
    // 未変更のまま実引数のプロパティ行だけが変わる。JS/TS/TSX で参照が call の
    // callee を成す場合に限り、enclosing call_expression の行範囲まで広げて
    // 実変更行との交差を確認する (交差しなければ従来どおり未更新 caller 扱い)。
    if enclosing_call_line_range(dir, r, bare, caches)
        .is_some_and(|(start, end)| (start..=end).any(|line| changed.contains(&line)))
    {
        return true;
    }
    // 呼び出し式が完全に無変更でも、引数に渡している共有 const の定義側に
    // 同一 diff で必須プロパティが追加されていれば追随済み
    // (`buildX(SHARED_DEPS)` のまま SHARED_DEPS 側だけ更新されるパターン)。
    // そうでなければ context 行 (未変更) の参照 → 未更新 caller の可能性。
    closed_via_local_const_argument(
        dir,
        r,
        bare,
        match arg_evidence {
            ClosureArgEvidence::TsAddedRequiredProps(props) => Some(*props),
            _ => None,
        },
        &changed,
        caches,
    )
}

/// 参照ファイルを parse (キャッシュ) して shadow binding を解決する。
/// read / parse / 言語判定に失敗した場合は `OtherOrAmbiguous` (除外しない)。
fn resolve_ref_shadow_binding(
    dir: &str,
    r: &crate::models::reference::SymbolReference,
    bare: &str,
    caches: &mut ApiClosureCaches,
) -> crate::commands::api_changes::js_ts_shadow::LocalResolution {
    use crate::commands::api_changes::js_ts_shadow::LocalResolution;
    let Some(parsed) = parsed_ref_file(dir, &r.path, caches) else {
        return LocalResolution::OtherOrAmbiguous;
    };
    crate::commands::api_changes::js_ts_shadow::resolve_reference_binding(
        &parsed.tree,
        &parsed.source,
        parsed.lang_id,
        bare,
        r.line,
        r.column,
    )
}

pub(super) struct ParsedRefFile {
    pub(super) source: crate::engine::parser::SourceBuf,
    pub(super) tree: tree_sitter::Tree,
    pub(super) lang_id: crate::language::LangId,
}

/// 参照ファイルの parse 結果 (キャッシュ付き) を返す。読み込みは `parser::read_file`
/// (100MB 上限 + TOCTOU 対策 + mmap ゼロコピー) を通し、`std::fs::read` の無制限読み込みを
/// 迂回経路にしない。
pub(super) fn parsed_ref_file<'a>(
    dir: &str,
    ref_path: &str,
    caches: &'a mut ApiClosureCaches,
) -> Option<&'a ParsedRefFile> {
    caches
        .parsed_refs
        .entry(ref_path.to_string())
        .or_insert_with(|| parse_ref_file(dir, ref_path).map(std::rc::Rc::new))
        .as_deref()
}

/// `parsed_ref_file` と同じキャッシュから、共有の parse 結果を返す。参照を返す版と違い
/// `caches` の借用を残さないので、1 つのファイルの解析木を見ながら別のファイルの解析や
/// 判定結果のキャッシュ (`&mut caches`) を使える。
pub(super) fn parsed_ref_file_shared(
    dir: &str,
    ref_path: &str,
    caches: &mut ApiClosureCaches,
) -> Option<std::rc::Rc<ParsedRefFile>> {
    caches
        .parsed_refs
        .entry(ref_path.to_string())
        .or_insert_with(|| parse_ref_file(dir, ref_path).map(std::rc::Rc::new))
        .clone()
}

fn parse_ref_file(dir: &str, ref_path: &str) -> Option<ParsedRefFile> {
    let abs = if std::path::Path::new(ref_path).is_absolute() {
        std::path::PathBuf::from(ref_path)
    } else {
        std::path::Path::new(dir).join(ref_path)
    };
    // 言語は refs と同じく拡張子 → shebang の順で決める (拡張子なしのスクリプトも参照元になる)。
    let lang = crate::engine::refs::detect_source_lang(&abs)?;
    let abs_utf8 = camino::Utf8PathBuf::from_path_buf(abs).ok()?;
    let source = crate::engine::parser::read_file(&abs_utf8).ok()?;
    let tree = crate::engine::parser::parse_source(&source, lang).ok()?;
    Some(ParsedRefFile {
        source,
        tree,
        lang_id: lang,
    })
}

/// JS/TS/TSX の参照が call の callee (直接または `obj.name(...)` の member 経由) を成す
/// 場合、その call_expression / new_expression 全体の行範囲 (0-indexed、両端含む) を返す。
/// JSX の要素名なら、子要素を含まない開始タグの範囲を返す。
/// callee でない参照・他言語・parse 失敗は `None` (従来の単一行判定に留める)。
fn enclosing_call_line_range(
    dir: &str,
    r: &crate::models::reference::SymbolReference,
    bare: &str,
    caches: &mut ApiClosureCaches,
) -> Option<(usize, usize)> {
    let parsed = parsed_ref_file(dir, &r.path, caches)?;
    if !matches!(
        parsed.lang_id,
        crate::language::LangId::Javascript
            | crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
    ) {
        return None;
    }
    callee_call_line_range(&parsed.tree, &parsed.source, bare, r.line, r.column)
}

/// `(line, column)` の identifier が call の callee を成すなら、その call_expression /
/// new_expression または JSX 開始タグの行範囲を返す純粋ロジック。ファイル I/O を分離してユニットテスト
/// 可能にした部分。callee でない参照は `None`。
fn callee_call_line_range(
    tree: &tree_sitter::Tree,
    source: &[u8],
    bare: &str,
    line: usize,
    column: usize,
) -> Option<(usize, usize)> {
    let point = tree_sitter::Point { row: line, column };
    let node = tree
        .root_node()
        .descendant_for_point_range(point, point)
        .filter(|n| {
            matches!(n.kind(), "identifier" | "property_identifier")
                && n.utf8_text(source).ok() == Some(bare)
        })?;
    if let Some(tag) = jsx_name_opening_tag(node, source) {
        // 新しい JSX 証明は、ファイル全体に解析エラーがあれば使わない。
        return (!tree.root_node().has_error())
            .then(|| (tag.start_position().row, tag.end_position().row));
    }
    // property_identifier の受け入れは JSX のメンバー名だけに限定する。
    if node.kind() != "identifier" {
        return None;
    }
    // callee 位置まで透過して登る。member_expression は `ns.startRecording({...})` の
    // receiver、parenthesized_expression は冗長括弧付き callee `(foo)(...)` を通すため
    // (括弧を挟むと call_expression の function フィールドは最外の括弧ノードになるので、
    // cur を透過で登らせておけば id が一致する)。
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            "member_expression" | "parenthesized_expression" => cur = parent,
            "call_expression" | "new_expression" => {
                let is_callee = parent
                    .child_by_field_name("function")
                    .or_else(|| parent.child_by_field_name("constructor"))
                    .is_some_and(|f| f.id() == cur.id());
                return is_callee.then(|| (parent.start_position().row, parent.end_position().row));
            }
            _ => return None,
        }
    }
    None
}

pub(super) use crate::engine::jsx::jsx_name_opening_tag;

/// 素通し再 export の名前位置なら true。同じ行の実利用は除外しない。
/// 改名された公開名の利用は名前索引で追跡できないため、改名 specifier は残す。
fn pass_through_reexport_at(
    tree: &tree_sitter::Tree,
    source: &[u8],
    bare: &str,
    line: usize,
    column: usize,
) -> bool {
    let check = || -> Option<()> {
        let root = tree.root_node();
        if bare.is_empty() || root.has_error() {
            return None;
        }
        let start = tree_sitter::Point { row: line, column };
        let end = tree_sitter::Point {
            row: line,
            column: column.checked_add(bare.len())?,
        };
        // 終点は識別子内部に置き、隣の句読点を含む範囲照会を避ける。
        let inside = tree_sitter::Point {
            row: line,
            column: end.column.checked_sub(1)?,
        };
        let node = root.descendant_for_point_range(start, inside)?;
        if node.start_position() != start
            || node.end_position() != end
            || node.utf8_text(source).ok()? != bare
        {
            return None;
        }
        let specifier = node.parent()?;
        let clause = specifier.parent()?;
        let statement = clause.parent()?;
        if specifier.kind() != "export_specifier"
            || clause.kind() != "export_clause"
            || statement.kind() != "export_statement"
            || statement.child_by_field_name("source")?.kind() != "string"
            || statement.child_by_field_name("declaration").is_some()
            || statement.child_by_field_name("value").is_some()
        {
            return None;
        }
        let name = specifier.child_by_field_name("name")?;
        let alias = specifier.child_by_field_name("alias");
        if name.kind() == "string"
            || alias.is_some_and(|a| a.kind() == "string")
            || (node.id() != name.id() && !alias.is_some_and(|a| node.id() == a.id()))
        {
            return None;
        }
        if let Some(alias) = alias {
            let name = name.utf8_text(source).ok()?;
            if name != "default" && name != alias.utf8_text(source).ok()? {
                return None;
            }
        }
        Some(())
    };
    check().is_some()
}

fn ref_is_pass_through_reexport(
    dir: &str,
    r: &crate::models::reference::SymbolReference,
    bare: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    // 他言語・lexer-only ファイルには新たな parse を行わない。
    if !matches!(
        crate::language::LangId::from_path(camino::Utf8Path::new(&r.path)),
        Ok(crate::language::LangId::Javascript
            | crate::language::LangId::Typescript
            | crate::language::LangId::Tsx)
    ) {
        return false;
    }
    let Some(parsed) = parsed_ref_file(dir, &r.path, caches) else {
        return false;
    };
    pass_through_reexport_at(&parsed.tree, &parsed.source, bare, r.line, r.column)
}

/// 定義・import/use 文の中の参照・素通し再 export を除いた「実際の呼び出し参照」だけを抜き出す。
///
/// import / use 文の中かどうかは参照位置のノードの祖先で判定する (複数行 grouped use ブロックの
/// 継続行も含む。api.mod 誤検出 2026-05-31)。行単位で判定すると、import 文と同じ行に書いた
/// 呼び出し (`import { foo } from "./a"; foo(1);` / Python の `from m import run; run(1)`) や、
/// 行頭が `use ` で始まる Python の代入 (`use = run(1)`) まで捨て、未更新の呼び出しを見落とす。
/// parse・位置の特定に失敗した参照は除外しない (blocking 側)。
fn collect_call_refs<'a>(
    refs: &'a [crate::models::reference::SymbolReference],
    dir: &str,
    bare: &str,
    caches: &mut ApiClosureCaches,
) -> Vec<&'a crate::models::reference::SymbolReference> {
    use crate::models::reference::RefKind;
    refs.iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .filter(|r| {
            !matches!(
                ref_import_role(dir, r, bare, caches),
                Some(RefImportRole::Plain | RefImportRole::Aliased)
            ) && !ref_is_pass_through_reexport(dir, r, bare, caches)
        })
        .collect()
}

/// リポジトリ内に解決できた呼び出し参照が 1 件も無いか
/// (定義・import/use 行・素通し再 export を除く)。
///
/// `api.mod` に残ったシンボルへ添えるトリアージ用フラグの算出。参照集合を引けない
/// (index 未収集 / batch 失敗) 場合は `false` = 「参照ありかもしれない」に倒す
/// (「呼び出し側が無い」と断定して読み手を誤らせない保守側)。
///
/// **分類や blocking 判定には使わない** — 参照 0 件は「未使用」ではなく「静的に解決できた
/// 内部参照が 0 件」でしかない (外部リポジトリ / 動的呼び出し / 文字列参照を区別できない)。
pub(crate) fn has_no_resolved_internal_callers(
    index: &ApiRefIndex,
    dir: &str,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    let Some(refs) = index.refs_for(bare_name(name)) else {
        return false;
    };
    collect_call_refs(refs, dir, bare_name(name), caches).is_empty()
}

/// 参照 (call の callee) が「引数に渡している共有 `const` の定義側が同一 diff 内で更新済み」
/// によって追随しているかを判定する。
///
/// 呼び出し式そのものが無変更 (`buildX(SHARED_DEPS)`) で、実際の追随が `SHARED_DEPS` の定義側で
/// 行われているケースを未更新 caller と誤判定しないための追加証拠。ガードは
/// `ts_const_arg` 側に集約している (TS/TSX 限定 / 裸 identifier 引数 / 同一ファイル内の一意な
/// `const` object literal / 追加必須プロパティが実変更行にあること)。
/// `added` が `None` (= 必須プロパティ追加以外の signature 変更) なら常に false。
fn closed_via_local_const_argument(
    dir: &str,
    r: &crate::models::reference::SymbolReference,
    bare: &str,
    added: Option<&crate::commands::api_changes::ts_signature::AddedRequiredObjectProps>,
    changed_lines: &std::collections::HashSet<usize>,
    caches: &mut ApiClosureCaches,
) -> bool {
    let Some(added) = added else {
        return false;
    };
    let Some(parsed) = parsed_ref_file(dir, &r.path, caches) else {
        return false;
    };
    let point = tree_sitter::Point {
        row: r.line,
        column: r.column,
    };
    let Some(node) = parsed
        .tree
        .root_node()
        .descendant_for_point_range(point, point)
        .filter(|n| n.kind() == "identifier" && n.utf8_text(&parsed.source).ok() == Some(bare))
    else {
        return false;
    };
    // 直接の callee (`foo(...)`) のみを対象にする。member 経由 (`ns.foo(...)`) は receiver の
    // 解決が別問題になるため対象外。
    let Some(call_node) = node
        .parent()
        .filter(|p| p.kind() == "call_expression")
        .filter(|p| {
            p.child_by_field_name("function")
                .is_some_and(|f| f.id() == node.id())
        })
    else {
        return false;
    };
    crate::commands::api_changes::ts_const_arg::closed_via_local_const_argument(
        parsed.lang_id,
        parsed.tree.root_node(),
        &parsed.source,
        call_node,
        added,
        changed_lines,
    )
}

/// `is_modified_closed_in_diff` で `detect_api_changes` 呼び出し 1 回の間だけ生かす per-file
/// キャッシュをまとめた構造体。
///
/// modified シンボル 1 件毎に再生成すると、refs が走る `unique_file` 数分の `git diff` 起動と
/// tree-sitter parse が M×F 回走る。`detect_api_changes` で 1 度だけ確保して使い回すことで
/// unique file 単位の F 回に圧縮する。
#[derive(Default)]
pub(crate) struct ApiClosureCaches {
    pub(crate) optional_removal: super::ts_signature::OptionalRemovalCache,
    /// `import_statement_lines_for_ref` の (ref path → import 行集合) キャッシュ。
    pub(crate) import_lines: std::collections::HashMap<String, std::collections::HashSet<usize>>,
    /// `changed_new_lines_for_file` の (new_path → 実際に変更/追加された new 行集合) キャッシュ。
    /// 呼び出し参照の証明の途中で他のキャッシュと同時に使うため、集合は共有で持つ。
    pub(crate) changed_new_lines:
        std::collections::HashMap<String, std::rc::Rc<std::collections::HashSet<usize>>>,
    /// shadow binding 解決用の (ref path → parse 結果) キャッシュ。`None` = read/parse
    /// 失敗 (毎回再試行しない)。同名定義が複数ある modified シンボルが多数あっても、
    /// 参照ファイル単位で 1 回だけ parse する。
    parsed_refs: std::collections::HashMap<String, Option<std::rc::Rc<ParsedRefFile>>>,
    /// Python のクラス・束縛の解決結果。参照ごとに定義ファイルや呼び出し側の解析木全体を
    /// 走査し直さないよう、(ファイル, 名前) 単位で 1 回だけ計算する。
    pub(super) python_classes: super::python_call_arity::PythonClassCache,
    /// JS / TS を埋め込む解析できないソース (`.vue` 等) の内容。1 回の検出で 1 回だけ読む。
    embedded_js_sources: Option<std::rc::Rc<EmbeddedJsSources>>,
    /// (旧パス, 新パス) → 宣言名と違う公開名で公開されている JS/TS の宣言名。
    pub(super) js_published_names: std::collections::HashMap<
        (String, String),
        std::rc::Rc<super::js_public_name::PublishedNames>,
    >,
    /// Python の変更後関数契約。参照ごとの定義ファイル再走査を避ける。
    pub(super) python_call_contracts:
        std::collections::HashMap<(String, String), Option<super::python_call_arity::CallContract>>,
    pub(super) python_method_contracts: std::collections::HashMap<
        (String, String),
        Option<super::python_call_arity::MethodContract>,
    >,
    pub(super) python_module_paths: std::collections::HashMap<String, Option<String>>,
    /// JS / TS / TSX の変更後関数契約。参照ごとの定義ファイル再走査を避ける。
    pub(super) js_call_contracts:
        std::collections::HashMap<(String, String), Option<super::js_ts_call_arity::CallContract>>,
}

/// 参照行が named export clause (`export { X }` / `export type { X }`) かを判定する。
/// re-export は公開エクスポート経路であり実利用ではないため、内部参照数から除外する。
///
/// `export type Foo = Bar` のような型エイリアス定義は除外しない (`Bar` は実利用)。
/// 判定は `export` の後に (`type` を挟んで) `{` が来るかで行い、波括弧の有無で
/// re-export と定義を切り分ける。
fn ref_is_export_clause_line(r: &crate::models::reference::SymbolReference) -> bool {
    r.context
        .as_deref()
        .map(|c| {
            let Some(rest) = c.trim_start().strip_prefix("export ") else {
                return false;
            };
            let rest = rest.trim_start();
            let rest = rest
                .strip_prefix("type ")
                .map_or(rest, |after_type| after_type.trim_start());
            rest.starts_with('{')
        })
        .unwrap_or(false)
}

/// `name` の同一ファイル (`file`) 内の実利用参照数を数える。
///
/// 定義行と import / re-export 行は実利用ではないため除外する
/// (`is_used_in_diff_paths` の他ファイル判定と同じ除外規約)。api.add の抽出条件は
/// 「同一ファイル内の**呼び出し**参照が無い」+「同一 diff の他ファイルからの実利用参照が
/// 無い」の合成で、TS の型注釈のように呼び出しではない参照は条件に現れない。内部参照数を
/// 出力に添えることで「同一ファイル内では使われているが他ファイルからは参照されていない」
/// と「完全に未参照」を利用者が区別でき、`refs` の再実行が不要になる
/// (Issue 2026-08-04-review-add-scope-naming)。
///
/// 検索失敗 / 未収集の name は 0 を返す (出力から省略され「情報なし」と同義になる)。
/// 複数行 export clause の継続行は行テキストから re-export と判定できず実利用として
/// 数えるが、過大側 (= 内部参照あり = export を残す判断) に倒れるため安全側。
pub(crate) fn count_internal_refs(
    index: &ApiRefIndex,
    dir: &str,
    file: &str,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> usize {
    use crate::models::reference::RefKind;
    let Some(refs) = index.refs_for(bare_name(name)) else {
        return 0;
    };
    let self_path = std::path::Path::new(file);
    let mut count = 0usize;
    for r in refs {
        if matches!(r.kind, Some(RefKind::Definition | RefKind::Unknown)) {
            continue;
        }
        if std::path::Path::new(r.path.as_str()) != self_path {
            continue;
        }
        if ref_is_import_line(r) || ref_is_export_clause_line(r) {
            continue;
        }
        let import_lines = caches
            .import_lines
            .entry(r.path.clone())
            .or_insert_with(|| import_statement_lines_for_ref(dir, &r.path));
        if import_lines.contains(&r.line) {
            continue;
        }
        count += 1;
    }
    count
}

/// 参照行が import / use 宣言 (signature 変更に追随不要) かを行テキストで簡易判定する。
pub(crate) fn ref_is_import_line(r: &crate::models::reference::SymbolReference) -> bool {
    r.context
        .as_deref()
        .map(|c| {
            let t = c.trim_start();
            t.starts_with("use ")
                || t.starts_with("pub use ")
                || t.starts_with("import ")
                || t.starts_with("from ")
        })
        .unwrap_or(false)
}

/// 参照ファイルの import/use 文が占める行集合 (0-indexed、`SymbolReference.line` と同基準)
/// を tree-sitter AST から取得する。複数行 grouped use ブロックの継続行も含む。
/// parse 不能 (lexer-only 言語 / 拡張子未対応 / 読み込み失敗) の場合は空集合を返し、
/// 既存の除外挙動を変えない。
pub(crate) fn import_statement_lines_for_ref(
    dir: &str,
    ref_path: &str,
) -> std::collections::HashSet<usize> {
    use std::collections::HashSet;
    let abs = if std::path::Path::new(ref_path).is_absolute() {
        std::path::PathBuf::from(ref_path)
    } else {
        std::path::Path::new(dir).join(ref_path)
    };
    let Some(utf8) = camino::Utf8Path::from_path(&abs) else {
        return HashSet::new();
    };
    let Ok(lang) = crate::language::LangId::from_path(utf8) else {
        return HashSet::new();
    };
    // lexer-only 言語 (Xojo) は ts_language() が panic するため parse しない。
    if lang.is_lexer_only() {
        return HashSet::new();
    }
    let Ok(source) = parser::read_file(utf8) else {
        return HashSet::new();
    };
    let Ok(tree) = parser::parse_source(&source, lang) else {
        return HashSet::new();
    };
    crate::engine::imports::import_statement_lines(tree.root_node())
}

/// diff ファイル 1 件について、new 側で実際に追加/変更された 0-indexed 行集合を返す。
///
/// `--git` が diff に合成した未追跡ファイルは `git diff <base>` に現れないため、git から
/// 取り直すと常に空集合になり、追随済みの呼び出し側まで未更新と誤判定していた
/// (`git add -N` しただけで結果が変わる)。
///
/// - 新規ファイル (`old_path == /dev/null`) は new 側の全行が追加行なので、hunk の new 範囲を
///   そのまま使う。context 行を含まない hunk (`old_count == 0`) だけを使うので、`+` 行以外を
///   追加行と数えることはない。
/// - 既存ファイルの hunk は context 行を含み、`HunkInfo` からは `+` 行を区別できないため
///   従来どおり git diff から実 `+` 行を取る。未追跡ファイルへの rename (削除 + 未追跡の
///   合成) だけは git diff に new 側が現れないので `untracked_rename_added_lines` で補う。
fn changed_new_lines_for_diff_file(
    dir: &str,
    base: &str,
    df: &crate::models::impact::DiffFile,
) -> std::collections::HashSet<usize> {
    if df.old_path == "/dev/null" {
        return df
            .hunks
            .iter()
            .filter(|h| h.old_count == 0)
            .flat_map(|h| {
                let start = h.new_start.saturating_sub(1);
                start..start + h.new_count
            })
            .collect();
    }
    let changed = changed_new_lines_for_file(dir, base, &df.old_path, &df.new_path);
    if !changed.is_empty()
        || df.hunks.is_empty()
        || df.old_path == df.new_path
        || !is_untracked_path(dir, &df.new_path)
    {
        return changed;
    }
    untracked_rename_added_lines(dir, base, &df.old_path, &df.new_path).unwrap_or(changed)
}

/// `path` (dir 相対) が git の index に無い (= 未追跡) か。判定できなければ false
/// (呼び出し側は git diff の結果をそのまま使い、blocking 側に倒れる)。
fn is_untracked_path(dir: &str, path: &str) -> bool {
    if validate_git_revision(path, "diff file path").is_err() {
        return false;
    }
    // パスに glob 文字 (`[id].tsx` 等) が含まれても別ファイルに一致させない。
    let output = crate::git_support::git_command_for_dir(dir)
        .args(["--literal-pathspecs", "ls-files", "-z", "--", path])
        .current_dir(dir)
        .output();
    match output {
        Ok(output) => output.status.success() && output.stdout.is_empty(),
        Err(_) => false,
    }
}

/// 未追跡ファイルへの rename (`old_path` は base に存在) で、new 側の各行のうち旧ファイルの
/// どの行とも一致しない行の 0-indexed 行集合を返す。旧側・新側が読めなければ None。
///
/// 合成 diff の hunk は共通の先頭・末尾以外をすべて `+` にするため、その範囲には
/// 書き換えていない行 (未更新の呼び出し) も入りうる。旧ファイルに同じ行がある行を
/// 変更行に数えなければ、どの行対応 (git の diff を含む) で見ても追加された行だけが残る
/// (未更新の呼び出しを追随済みと誤判定する向きには倒れない)。
fn untracked_rename_added_lines(
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
) -> Option<std::collections::HashSet<usize>> {
    let old = git_show_blob(dir, base, old_path)?;
    let full = std::path::Path::new(dir).join(new_path);
    let new = parser::read_file(camino::Utf8Path::new(full.to_str()?)).ok()?;
    let new = std::str::from_utf8(&new).ok()?;
    let old = String::from_utf8_lossy(&old);
    let old_lines: HashSet<&str> = old.lines().collect();
    Some(
        new.lines()
            .enumerate()
            .filter(|(_, line)| !old_lines.contains(line))
            .map(|(i, _)| i)
            .collect(),
    )
}

/// `git diff <base> -M -- <old_path> <new_path>` を解析し、new 側で実際に追加/変更された
/// 0-indexed 行集合を返す。取得・解析に失敗した場合は空集合 (= どの参照も追随済みと見なさず
/// blocking 維持) を返す。
pub(crate) fn changed_new_lines_for_file(
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
) -> std::collections::HashSet<usize> {
    use std::collections::HashSet;
    if validate_git_revision(base, "--base").is_err()
        || validate_git_revision(new_path, "diff file path").is_err()
    {
        return HashSet::new();
    }
    // rename 検出を有効化 (-M)。rename された caller を new_path だけの pathspec で diff すると
    // Git は「新規ファイル全行追加」として返し、未更新の古い呼び出しまで changed に見えてしまう
    // (codex 指摘)。old_path も pathspec に含めて rename-aware な diff にする。
    // core.quotepath=off: 非 ASCII 名の `+++ "b/..."` クォートで extract_changed_new_lines の
    // パス照合が外れ、追随済み参照まで blocking 維持になるのを防ぐ。
    // --relative: `old_path` / `new_path` は `dir` 相対 (pathspec も cwd 基準) なので、
    // 出力ヘッダも `dir` 相対に揃えないと `extract_changed_new_lines` の new_path 照合が
    // サブディレクトリ実行で外れる。`--dir` = リポジトリルートでは無変化。
    // GIT_DIFF_PARSEABLE_OUTPUT_ARGS: 利用者の git 設定 (`diff.mnemonicPrefix` / `color.diff` /
    // `diff.external` 等) で出力形式が変わると new_path 照合が外れ、追随済みの参照まで
    // blocking に残る (`GIT_DIFF_PARSEABLE_OUTPUT_ARGS` の doc 参照)。
    let mut args: Vec<&str> = vec!["-c", "core.quotepath=off", "diff"];
    args.extend(crate::git_support::GIT_DIFF_PARSEABLE_OUTPUT_ARGS);
    args.extend(["--relative", base]);
    let snapshot_tree = crate::git_support::index_snapshot_tree(dir);
    if let Some(tree) = snapshot_tree.as_deref() {
        args.push(tree);
    }
    args.extend(["-M", "--"]);
    if old_path != "/dev/null" && old_path != new_path {
        if validate_git_revision(old_path, "diff file path").is_err() {
            return HashSet::new();
        }
        args.push(old_path);
    }
    args.push(new_path);
    let output = crate::git_support::git_command_for_dir(dir)
        .args(&args)
        .current_dir(dir)
        .output();
    let Ok(output) = output else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    let diff = String::from_utf8_lossy(&output.stdout);
    crate::engine::diff::extract_changed_new_lines(&diff, new_path)
}

/// diff の new_path (dir 相対) と参照 path (dir 相対 or 絶対) が同一ファイルを指すか判定する。
pub(crate) fn diff_path_matches_ref(diff_path: &str, ref_path: &str, dir: &str) -> bool {
    if diff_path == ref_path {
        return true;
    }
    let abs_diff = std::path::Path::new(dir).join(diff_path);
    let abs_ref = if std::path::Path::new(ref_path).is_absolute() {
        std::path::PathBuf::from(ref_path)
    } else {
        std::path::Path::new(dir).join(ref_path)
    };
    match (
        std::fs::canonicalize(&abs_diff),
        std::fs::canonicalize(&abs_ref),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 新規追加シンボル `name` が、同一 diff 内の別ファイル（`diff_new_paths`）から
/// 参照されているかを判定する。参照があれば「コミット内で完結して使用されている」
/// として api.add から除外する（pub struct の import や型参照が典型例）。
///
/// **`pub use` / `use` などの import/re-export 文中の参照は internal-use と数えない**
/// (Issue 2026-06-05-rust-api-add-private-module-reexport-edge-graph 対応)。
/// `pub use crate::wifi::found;` は内部利用ではなく公開エクスポートで、true で抑制すると
/// re-export が internal-use と誤認されて api.add から脱落する。実利用 (関数本体での
/// 呼び出し / 型注釈 / 値参照) があれば別の参照として出るためそちらで判定する。
///
/// `defining_file` / `diff_new_paths` は `dir` からの相対パスを想定する。
/// 参照検索に失敗した場合は false を返し、保守的に api.add に残す。
pub(crate) fn is_used_in_diff_paths(
    index: &ApiRefIndex,
    dir: &str,
    name: &str,
    defining_file: &str,
    diff_new_paths: &HashSet<String>,
) -> bool {
    use crate::models::reference::RefKind;
    // qualname (`Container.method`) の場合は bare name で参照検索する
    let search_name = bare_name(name);
    if search_name.is_empty() {
        return false;
    }
    let Some(refs) = index.refs_for(search_name) else {
        return false;
    };
    let defining_path = std::path::Path::new(defining_file);
    // import/use 文が占める行集合を参照ファイルごとに 1 度だけ計算してキャッシュする。
    let mut import_lines_cache: std::collections::HashMap<
        String,
        std::collections::HashSet<usize>,
    > = std::collections::HashMap::new();
    refs.iter().any(|r| {
        if r.kind == Some(RefKind::Definition) {
            return false;
        }
        let ref_path = r.path.as_str();
        if std::path::Path::new(ref_path) == defining_path || !diff_new_paths.contains(ref_path) {
            return false;
        }
        // import/use 行は internal-use ではなく公開エクスポート経路なので除外する。
        if ref_is_import_line(r) {
            return false;
        }
        let import_lines = import_lines_cache
            .entry(ref_path.to_string())
            .or_insert_with(|| import_statement_lines_for_ref(dir, ref_path));
        if import_lines.contains(&r.line) {
            return false;
        }
        true
    })
}

/// 削除されたシンボル `name` が、変更後のツリー全体のどこからも参照されていないかを判定する。
/// 参照が 0 件であれば同一 diff 内で全 caller が追随済みと判断し、`api.rm` から除外する。
/// 参照検索に失敗した場合は保守的に false（外部参照ありとみなす）を返し、
/// レビュー対象として残す（false negative を起こさない方針）。
///
/// qualname (`Container.method`) は refs 検索の identifier マッチでは常に 0 件になるため、
/// `bare_name` で正規化して検索する。同名定義が HEAD ツリーに 2 件以上残存する場合は
/// 「部分削除」「同名複数 export」の可能性があるため保守的に false を返す
/// (codex 指摘: detect_api_changes の早期 continue 経路でも qualname 対応が必要)。
pub(crate) fn is_removed_symbol_unreferenced(index: &ApiRefIndex, name: &str) -> bool {
    use crate::models::reference::RefKind;
    let bare = bare_name(name);
    let Some(refs) = index.refs_for(bare) else {
        return false;
    };
    let mut def_count = 0usize;
    let mut ref_count = 0usize;
    for r in refs {
        if r.kind == Some(RefKind::Definition) {
            def_count += 1;
        } else {
            ref_count += 1;
        }
    }
    if def_count > 1 {
        return false;
    }
    ref_count == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    #[test]
    fn reexport_exclusion_is_exact_and_preserves_renamed_and_value_uses() {
        for (marked, expected) in [
            ("export {@Card} from './module';", true),
            ("export {\n @Card,\n} from './module';", true),
            ("export { @Card as Card } from './module';", true),
            ("export { Card as @Card } from './module';", true),
            ("export { default as @Card } from './module';", true),
            ("export { @Card, Other as Legacy } from './module';", true),
            ("/* カード */ export { @Card } from './module';", true),
            ("export { @Card as Legacy } from './module';", false),
            ("export { Other as @Card } from './module';", false),
            ("export { @Card as default } from './module';", false),
            ("export { 'Card' as @Card } from './module';", false),
            ("export { @Card as 'x-y' } from './module';", false),
            ("export { @Card };", false),
            ("export default @Card;", false),
            ("export const value = @Card();", false),
            ("export function value() { return @Card(); }", false),
            ("export * as @Card from './module';", false),
            ("export { Card } from './module'; @Card();", false),
            ("export { @Card } from './module';\nconst broken = ;", false),
        ] {
            let offset = marked.find('@').unwrap();
            let source = marked.replace('@', "");
            let prefix = &source[..offset];
            let line = prefix.bytes().filter(|b| *b == b'\n').count();
            let column = offset - prefix.rfind('\n').map_or(0, |i| i + 1);
            for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert_eq!(
                    pass_through_reexport_at(&tree, source.as_bytes(), "Card", line, column),
                    expected,
                    "{lang:?}: {marked}"
                );
                assert!(!pass_through_reexport_at(
                    &tree,
                    source.as_bytes(),
                    "Card",
                    line,
                    column + 1
                ));
            }
        }
        for source in [
            "export type { Card } from './module';",
            "export { type Card } from './module';",
        ] {
            let column = source.find("Card").unwrap();
            for lang in [LangId::Typescript, LangId::Tsx] {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(pass_through_reexport_at(
                    &tree,
                    source.as_bytes(),
                    "Card",
                    0,
                    column
                ));
            }
        }
    }

    /// `@` の位置にある名前の import 上の役割。
    fn role_at(marked: &str, lang: LangId, bare: &str) -> Option<RefImportRole> {
        let offset = marked.find('@').unwrap();
        let source = marked.replace('@', "");
        let prefix = &source[..offset];
        let line = prefix.bytes().filter(|b| *b == b'\n').count();
        let column = offset - prefix.rfind('\n').map_or(0, |i| i + 1);
        let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
        let parsed = ParsedRefFile {
            source: crate::engine::parser::SourceBuf::Vec(source.clone().into_bytes()),
            tree,
            lang_id: lang,
        };
        let node = ref_name_node(&parsed, bare, line, column)?;
        Some(import_role_at(node, &parsed.source, bare))
    }

    /// import / use 文の中かどうかは参照位置のノードで決める。同じ行の別の文にある呼び出しや、
    /// 行頭が `use ` の Python の代入を import 扱いしない。別名を付ける位置は Aliased。
    #[test]
    fn import_role_is_judged_by_reference_position() {
        use RefImportRole::{Aliased, Outside, Plain};
        for (marked, lang, bare, expected) in [
            (
                "import { @foo } from \"./a\"; foo(1);",
                LangId::Typescript,
                "foo",
                Plain,
            ),
            (
                "import { foo } from \"./a\"; @foo(1);",
                LangId::Typescript,
                "foo",
                Outside,
            ),
            (
                "import { @foo as f } from \"./a\";",
                LangId::Typescript,
                "foo",
                Aliased,
            ),
            (
                "import { x as @foo } from \"./a\";",
                LangId::Typescript,
                "foo",
                Plain,
            ),
            (
                "export { @foo as execute } from \"./a\";",
                LangId::Typescript,
                "foo",
                Aliased,
            ),
            (
                "export { @foo as default };",
                LangId::Javascript,
                "foo",
                Aliased,
            ),
            ("export { @foo };", LangId::Typescript, "foo", Outside),
            (
                "from pkg.a import @run; VALUE = run(1)\n",
                LangId::Python,
                "run",
                Plain,
            ),
            (
                "from pkg.a import run; VALUE = @run(1)\n",
                LangId::Python,
                "run",
                Outside,
            ),
            ("use = @run(1)\n", LangId::Python, "run", Outside),
            (
                "from pkg.a import @run as r\n",
                LangId::Python,
                "run",
                Aliased,
            ),
            ("use crate::m::@run;\n", LangId::Rust, "run", Plain),
            ("use crate::m::@run as r;\n", LangId::Rust, "run", Aliased),
            (
                "use crate::m::{@run as r, other};\n",
                LangId::Rust,
                "run",
                Aliased,
            ),
            ("import a.b.@run as r\n", LangId::Kotlin, "run", Aliased),
            ("import a.b.@run\n", LangId::Kotlin, "run", Plain),
            (
                "<?php\nuse function A\\B\\@run as r;\n",
                LangId::Php,
                "run",
                Aliased,
            ),
            (
                "<?php\nuse function A\\B\\@run;\n",
                LangId::Php,
                "run",
                Plain,
            ),
        ] {
            assert_eq!(
                role_at(marked, lang, bare),
                Some(expected),
                "{lang:?}: {marked}"
            );
        }
        // 位置と名前が食い違えば判定しない (呼び出し側は除外しない)
        assert_eq!(
            role_at("@import { foo } from \"./a\";", LangId::Typescript, "foo"),
            None
        );
    }

    /// `@` の位置の参照が値利用でないか (JSX の要素名・import 指定子・export 指定子・宣言名)。
    fn non_value_at(marked: &str) -> bool {
        let offset = marked.find('@').unwrap();
        let source = marked.replace('@', "");
        let prefix = &source[..offset];
        let line = prefix.bytes().filter(|b| *b == b'\n').count();
        let column = offset - prefix.rfind('\n').map_or(0, |i| i + 1);
        let tree = parser::parse_source(source.as_bytes(), LangId::Tsx).unwrap();
        let parsed = ParsedRefFile {
            source: crate::engine::parser::SourceBuf::Vec(source.clone().into_bytes()),
            tree,
            lang_id: LangId::Tsx,
        };
        let node = ref_name_node(&parsed, "ScheduleItem", line, column).expect("node");
        js_reference_is_non_value(node, &parsed.source, "ScheduleItem")
    }

    /// 値利用かどうかは参照位置のノードで判定する。行テキストで判定すると、同じ行の別の出現を
    /// 混ぜ、256 バイトで切り詰めた行の後半を見落とす。
    #[test]
    fn js_value_usage_is_judged_by_reference_position() {
        let long = "t".repeat(300);
        for (marked, non_value) in [
            ("const v = <@ScheduleItem foo={1} />;", true),
            ("const v = <ScheduleItem>x</@ScheduleItem>;", true),
            (
                "import { @ScheduleItem } from \"./s\"; ScheduleItem();",
                true,
            ),
            (
                "import { ScheduleItem } from \"./s\"; @ScheduleItem();",
                false,
            ),
            ("import { @ScheduleItem as S } from \"./s\";", false),
            ("export { @ScheduleItem } from \"./s\";", true),
            ("export { @ScheduleItem as S };", false),
            (
                "export const @ScheduleItem = memo(function ScheduleItem() { return null; });",
                true,
            ),
            (
                "export const ScheduleItem = memo(function @ScheduleItem() { return null; });",
                true,
            ),
            ("const x = @ScheduleItem({});", false),
            ("type P = typeof @ScheduleItem;", false),
            ("@ScheduleItem.displayName = 'x';", false),
            ("const Alias = @ScheduleItem;", false),
            ("export default @ScheduleItem;", false),
            (
                &format!(
                    "const v = <section><@ScheduleItem /><p title=\"{long}\"></p>{{ScheduleItem()}}</section>;"
                ),
                true,
            ),
            (
                &format!(
                    "const v = <section><ScheduleItem /><p title=\"{long}\"></p>{{@ScheduleItem()}}</section>;"
                ),
                false,
            ),
        ] {
            assert_eq!(non_value_at(marked), non_value, "{marked}");
        }
    }

    #[test]
    fn contains_js_word_respects_identifier_boundaries() {
        assert!(contains_js_word("const x = format(1);", "format"));
        assert!(contains_js_word("config.b + 1", "b"));
        assert!(!contains_js_word("const x = formatted(1);", "format"));
        assert!(!contains_js_word("const $format = 1;", "format"));
        assert!(!contains_js_word("const bb = 1; ab;", "b"));
        assert!(!contains_js_word("anything", ""));
    }

    fn call_range_at(src: &str, bare: &str, line: usize, column: usize) -> Option<(usize, usize)> {
        let tree = parser::parse_source(src.as_bytes(), LangId::Typescript).unwrap();
        callee_call_line_range(&tree, src.as_bytes(), bare, line, column)
    }

    #[test]
    fn jsx_call_range_excludes_children_attributes_and_receivers() {
        for lang in [LangId::Javascript, LangId::Tsx] {
            let src = "const view = <ns.Card\n value={Card}\n>\n {Card}\n</ns.Card>;\n";
            let tree = parser::parse_source(src.as_bytes(), lang).unwrap();
            for (line, column, expected) in [
                (0, 17, Some((0, 2))),
                (4, 5, Some((0, 2))),
                (1, 8, None),
                (3, 2, None),
            ] {
                assert_eq!(
                    callee_call_line_range(&tree, src.as_bytes(), "Card", line, column),
                    expected,
                    "{lang:?}: {line}:{column}",
                );
            }
            assert_eq!(
                callee_call_line_range(&tree, src.as_bytes(), "ns", 0, 14),
                None
            );
        }
    }

    /// 括弧なしの複数行呼び出しは従来どおり call_expression 全体の行範囲を返す。
    #[test]
    fn plain_multiline_callee_returns_call_range() {
        // `foo` は line0 col10
        let src = "const a = foo(\n  changedArg,\n);\n";
        assert_eq!(call_range_at(src, "foo", 0, 10), Some((0, 2)));
    }

    /// 冗長括弧付き callee `(foo)(...)` でも call_expression 全体の行範囲を返す (B-3)。
    /// 括弧を透過しないと function フィールドの括弧ノードと id 照合できず None になっていた。
    #[test]
    fn parenthesized_callee_returns_call_range() {
        // `foo` は line0 col11
        let src = "const a = (foo)(\n  changedArg,\n);\n";
        assert_eq!(call_range_at(src, "foo", 0, 11), Some((0, 2)));
    }

    /// 二重括弧 `((foo))(...)` も透過して行範囲を返す。
    #[test]
    fn double_parenthesized_callee_returns_call_range() {
        // `foo` は line0 col12
        let src = "const a = ((foo))(\n  changedArg,\n);\n";
        assert_eq!(call_range_at(src, "foo", 0, 12), Some((0, 2)));
    }

    /// 括弧付きでも callee でない (単なる `(foo)` 値参照) なら None のまま = 過検出しない。
    #[test]
    fn parenthesized_non_callee_is_none() {
        // `foo` は line0 col11
        let src = "const a = (foo) + 1;\n";
        assert_eq!(call_range_at(src, "foo", 0, 11), None);
    }
}
