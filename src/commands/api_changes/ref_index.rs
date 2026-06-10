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
    /// `names` の参照を chunk 単位の batch 検索で収集する。
    /// chunk サイズは CLI `refs --names` と同じ (既定 64、`ASTRO_SIGHT_REFS_BATCH_CHUNK`
    /// で調整可)。AC trie はパターン数に対して非線形にメモリを使うため一括にはしない。
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
        let service = AppService::new();
        for chunk in sorted.chunks(super::super::refs_batch_chunk_size()) {
            match service.find_references_batch(chunk, dir, None) {
                Ok(results) => {
                    for r in results {
                        index.refs.insert(r.symbol, r.references);
                    }
                }
                Err(_) => index.failed.extend(chunk.iter().cloned()),
            }
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
/// `file_path` は `dir` からの相対パスを想定する。index の参照パスも `dir` 相対なので
/// `Path` 単位で比較する。
pub(crate) fn has_cross_file_refs(index: &ApiRefIndex, file_path: &str, name: &str) -> bool {
    use std::path::Path;

    let Some(refs) = index.refs_for(name) else {
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
/// コミットで更新済みとみなし closed-in-diff (informational)。同名定義が複数 (別型の同名
/// メソッド等) / refs 解析失敗 / diff 外 or hunk 外の参照が 1 つでもあれば false を返し、
/// 保守的に blocking 側 (通常の api.mod) へ倒す。
pub(crate) fn is_modified_closed_in_diff(
    index: &ApiRefIndex,
    dir: &str,
    name: &str,
    base: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> bool {
    use crate::models::reference::RefKind;
    use std::collections::{HashMap, HashSet};
    let bare = bare_name(name);
    let Some(refs) = index.refs_for(bare) else {
        return false;
    };
    // 同名定義が 1 つでなければ曖昧 (別型の同名メソッド等) なので保守的に blocking。
    let def_count = refs
        .iter()
        .filter(|r| r.kind == Some(RefKind::Definition))
        .count();
    if def_count != 1 {
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
    let mut import_lines_cache: HashMap<String, HashSet<usize>> = HashMap::new();
    let call_refs: Vec<&crate::models::reference::SymbolReference> = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition) && !ref_is_import_line(r))
        .filter(|r| {
            let import_lines = import_lines_cache
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
    let mut changed_cache: HashMap<String, HashSet<usize>> = HashMap::new();
    for r in &call_refs {
        let Some(df) = diff_files.iter().find(|df| {
            df.new_path != "/dev/null" && diff_path_matches_ref(&df.new_path, &r.path, dir)
        }) else {
            return false; // diff 外ファイルの参照 → 未更新 caller の可能性
        };
        let changed = changed_cache
            .entry(df.new_path.clone())
            .or_insert_with(|| changed_new_lines_for_file(dir, base, &df.old_path, &df.new_path));
        if !changed.contains(&r.line) {
            return false; // context 行 (未変更) の参照 → 未更新 caller の可能性
        }
    }
    true
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
    let mut args: Vec<&str> = vec!["diff", base, "-M", "--"];
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
