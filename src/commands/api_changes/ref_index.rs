//! API 差分判定用の cross-file 参照インデックス (`ApiRefIndex`) と、
//! 参照状況から api.add / api.rm / api.mod の扱いを決める判定ヘルパー。

use std::collections::HashSet;

use crate::engine::parser;
use crate::service::AppService;

use super::super::git_input::validate_git_revision;
use super::{bare_name, ctx_usage_is_jsx_or_safe};

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
        match service.find_references_batch(&sorted, dir, None) {
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
pub(crate) fn has_blocking_value_usage(index: &ApiRefIndex, name: &str) -> bool {
    use crate::models::reference::RefKind;
    let bare = bare_name(name);
    let Some(refs) = index.refs_for(bare) else {
        return true;
    };
    for r in refs {
        if r.kind == Some(RefKind::Definition) {
            continue;
        }
        let Some(ctx) = r.context.as_deref() else {
            return true;
        };
        let trimmed = ctx.trim_start();
        // import / re-export specifier は値利用ではない
        if ref_is_import_line(r)
            || trimmed.starts_with("export {")
            || trimmed.starts_with("export type")
        {
            continue;
        }
        if !ctx_usage_is_jsx_or_safe(ctx, bare) {
            return true;
        }
    }
    false
}

/// modified シンボルの全 cross-file 参照が同一 diff 内の変更 hunk で追随済みかを判定する。
///
/// 全ての非定義参照が diff_files の変更 hunk (new 範囲) に収まれば、呼び出し側が同一
/// コミットで更新済みとみなし closed-in-diff (informational)。refs 解析失敗 /
/// diff 外 or hunk 外の参照が 1 つでもあれば false を返し、保守的に blocking 側
/// (通常の api.mod) へ倒す。
///
/// 同名定義が複数ある場合: 変更対象ファイル (`target_new_path`) 内の定義が 1 つに
/// 特定できなければ従来どおり false。他ファイルの同名定義は、JS/TS/TSX のトップレベル
/// 関数 (bare 名) に限り `js_ts_shadow::resolve_reference_binding` で参照単位に解決し、
/// 「同一ファイルの function_declaration に束縛されるローカル呼び出し」だけを判定対象
/// から除外する (Issue 2026-07-12-api-mod-same-diff-informational: export 関数と
/// 別ファイルのローカル同名関数の併存で closed 判定が全滅していた)。method qualname
/// (`Container.method`) と JS/TS 以外の言語は従来ガード (即 blocking) を維持する。
#[allow(clippy::too_many_arguments)]
pub(crate) fn is_modified_closed_in_diff(
    index: &ApiRefIndex,
    dir: &str,
    name: &str,
    kind: &str,
    base: &str,
    target_new_path: &str,
    diff_files: &[crate::models::impact::DiffFile],
    caches: &mut ApiClosureCaches,
) -> bool {
    use crate::models::reference::RefKind;
    let bare = bare_name(name);
    let Some(refs) = index.refs_for(bare) else {
        return false;
    };
    // 変更対象ファイル内の定義が 1 つに特定できなければ曖昧なので保守的に blocking。
    let defs: Vec<&crate::models::reference::SymbolReference> = refs
        .iter()
        .filter(|r| r.kind == Some(RefKind::Definition))
        .collect();
    let target_def_count = defs
        .iter()
        .filter(|r| diff_path_matches_ref(target_new_path, &r.path, dir))
        .count();
    if target_def_count != 1 {
        return false;
    }
    // 他ファイルの同名定義がある場合、JS/TS/TSX の**トップレベル関数** (bare 名かつ
    // kind=function) のみ shadow resolver での参照単位除外に進む。class / const /
    // type alias 等や method qualname、他言語は従来どおり blocking。
    let has_foreign_defs = defs.len() > target_def_count;
    let target_lang =
        crate::language::LangId::from_path(camino::Utf8Path::new(target_new_path)).ok();
    if has_foreign_defs
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
        return false;
    }
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
    let call_refs: Vec<&crate::models::reference::SymbolReference> = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition) && !ref_is_import_line(r))
        .filter(|r| {
            let import_lines = caches
                .import_lines
                .entry(r.path.clone())
                .or_insert_with(|| import_statement_lines_for_ref(dir, &r.path));
            !import_lines.contains(&r.line)
        })
        .collect();
    if call_refs.is_empty() {
        return false;
    }
    // 全ての呼び出し参照が、diff 内ファイルかつ実際の追加/変更行 (context 行ではない) にあるか。
    // HunkInfo の new 範囲は context 行を含むため、git diff から実 `+` 行集合を取得して照合する
    // (codex 指摘: context 行に古い呼び出しが入ると未更新 caller を誤って closed 判定してしまう)。
    // shadow 除外で全参照が消えた場合は「対象 API の caller を 1 件も確認できていない」
    // ため closed にしない (除外前の空チェックだけだと fail-open になる)。
    let mut effective_call_refs = 0usize;
    for r in &call_refs {
        // 他ファイルの同名定義がある場合: 変更対象ファイル以外の参照で、lexical scope が
        // 同一ファイルの function_declaration に束縛されるものは対象 API と無関係な
        // ローカル呼び出しなので除外する。解決失敗・property 位置・function 以外の
        // binding は OtherOrAmbiguous = 除外せず下の diff 実変更行チェックに掛かる。
        if has_foreign_defs
            && !diff_path_matches_ref(target_new_path, &r.path, dir)
            && resolve_ref_shadow_binding(dir, r, bare, caches)
                == crate::commands::api_changes::js_ts_shadow::LocalResolution::SameFileFunction
        {
            continue;
        }
        effective_call_refs += 1;
        let Some(df) = diff_files.iter().find(|df| {
            df.new_path != "/dev/null" && diff_path_matches_ref(&df.new_path, &r.path, dir)
        }) else {
            return false; // diff 外ファイルの参照 → 未更新 caller の可能性
        };
        let changed = caches
            .changed_new_lines
            .entry(df.new_path.clone())
            .or_insert_with(|| changed_new_lines_for_file(dir, base, &df.old_path, &df.new_path));
        if !changed.contains(&r.line) {
            // 複数行呼び出し (`startRecording({\n  fps,\n  cursor,\n})`) では識別子行は
            // 未変更のまま実引数のプロパティ行だけが変わる。JS/TS/TSX で参照が call の
            // callee を成す場合に限り、enclosing call_expression の行範囲まで広げて
            // 実変更行との交差を確認する (交差しなければ従来どおり未更新 caller 扱い)。
            let call_range = enclosing_call_line_range(dir, r, bare, caches);
            let intersects = call_range.is_some_and(|(start, end)| {
                let changed = caches.changed_new_lines.get(&df.new_path);
                changed.is_some_and(|c| (start..=end).any(|line| c.contains(&line)))
            });
            if !intersects {
                return false; // context 行 (未変更) の参照 → 未更新 caller の可能性
            }
        }
    }
    effective_call_refs > 0
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
    let Some((source, tree, lang)) = parsed_ref_file(dir, &r.path, caches) else {
        return LocalResolution::OtherOrAmbiguous;
    };
    crate::commands::api_changes::js_ts_shadow::resolve_reference_binding(
        tree, source, *lang, bare, r.line, r.column,
    )
}

/// 参照ファイルの parse 結果 (キャッシュ付き) を返す。読み込みは `parser::read_file`
/// (100MB 上限 + TOCTOU 対策 + mmap ゼロコピー) を通し、`std::fs::read` の無制限読み込みを
/// 迂回経路にしない。
fn parsed_ref_file<'a>(
    dir: &str,
    ref_path: &str,
    caches: &'a mut ApiClosureCaches,
) -> Option<&'a (
    crate::engine::parser::SourceBuf,
    tree_sitter::Tree,
    crate::language::LangId,
)> {
    caches
        .parsed_refs
        .entry(ref_path.to_string())
        .or_insert_with(|| {
            let abs = if std::path::Path::new(ref_path).is_absolute() {
                std::path::PathBuf::from(ref_path)
            } else {
                std::path::Path::new(dir).join(ref_path)
            };
            let abs_utf8 = camino::Utf8PathBuf::from_path_buf(abs).ok()?;
            let lang = crate::language::LangId::from_path(camino::Utf8Path::new(ref_path)).ok()?;
            let source = crate::engine::parser::read_file(&abs_utf8).ok()?;
            let tree = crate::engine::parser::parse_source(&source, lang).ok()?;
            Some((source, tree, lang))
        })
        .as_ref()
}

/// JS/TS/TSX の参照が call の callee (直接または `obj.name(...)` の member 経由) を成す
/// 場合、その call_expression / new_expression 全体の行範囲 (0-indexed、両端含む) を返す。
/// callee でない参照・他言語・parse 失敗は `None` (従来の単一行判定に留める)。
fn enclosing_call_line_range(
    dir: &str,
    r: &crate::models::reference::SymbolReference,
    bare: &str,
    caches: &mut ApiClosureCaches,
) -> Option<(usize, usize)> {
    let (source, tree, lang) = parsed_ref_file(dir, &r.path, caches)?;
    if !matches!(
        lang,
        crate::language::LangId::Javascript
            | crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
    ) {
        return None;
    }
    let point = tree_sitter::Point {
        row: r.line,
        column: r.column,
    };
    let node = tree
        .root_node()
        .descendant_for_point_range(point, point)
        .filter(|n| n.kind() == "identifier" && n.utf8_text(source).ok() == Some(bare))?;
    // callee 位置まで member_expression を透過して登る (`ns.startRecording({...})` 等)。
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            "member_expression" => cur = parent,
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

/// `is_modified_closed_in_diff` で `detect_api_changes` 呼び出し 1 回の間だけ生かす per-file
/// キャッシュをまとめた構造体。
///
/// modified シンボル 1 件毎に再生成すると、refs が走る `unique_file` 数分の `git diff` 起動と
/// tree-sitter parse が M×F 回走る。`detect_api_changes` で 1 度だけ確保して使い回すことで
/// unique file 単位の F 回に圧縮する。
#[derive(Default)]
pub(crate) struct ApiClosureCaches {
    /// `import_statement_lines_for_ref` の (ref path → import 行集合) キャッシュ。
    pub(crate) import_lines: std::collections::HashMap<String, std::collections::HashSet<usize>>,
    /// `changed_new_lines_for_file` の (new_path → 実際に変更/追加された new 行集合) キャッシュ。
    pub(crate) changed_new_lines:
        std::collections::HashMap<String, std::collections::HashSet<usize>>,
    /// shadow binding 解決用の (ref path → parse 結果) キャッシュ。`None` = read/parse
    /// 失敗 (毎回再試行しない)。同名定義が複数ある modified シンボルが多数あっても、
    /// 参照ファイル単位で 1 回だけ parse する。
    #[allow(clippy::type_complexity)]
    pub(crate) parsed_refs: std::collections::HashMap<
        String,
        Option<(
            crate::engine::parser::SourceBuf,
            tree_sitter::Tree,
            crate::language::LangId,
        )>,
    >,
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
    let mut args: Vec<&str> = vec!["-c", "core.quotepath=off", "diff", base, "-M", "--"];
    if old_path != "/dev/null" && old_path != new_path {
        if validate_git_revision(old_path, "diff file path").is_err() {
            return HashSet::new();
        }
        args.push(old_path);
    }
    args.push(new_path);
    let output = std::process::Command::new("git")
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
