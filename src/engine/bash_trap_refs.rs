//! bash の `trap '<handler>' SIG...` builtin の handler 文字列内に書かれた
//! コマンド呼び出しを「シンボル参照」として抽出する。
//!
//! tree-sitter-bash は trap の第 1 引数を `raw_string` / `string` / `word` の
//! いずれかとして parse するだけで、handler 文字列内のコマンド呼び出しは AST 化
//! しない。そのため `trap 'cleanup_signal 130' INT` のような構文では
//! `cleanup_signal` 関数定義への参照が AST 上に現れず、refs / dead-code が誤って
//! 「参照ゼロ」と判定してしまう。
//!
//! 本モジュールは `command` ノードを受け取り、`command_name` が `trap` の場合に
//! 限り、handler 文字列を tree-sitter-bash で再 parse して内側 AST の
//! `command_name` を再帰的に拾い、ファイル全体での `(name, row, col)` を返す。
//! これにより `time func`, `( func )`, `{ func; }`, `if ...; then func; fi` の
//! ような構文も自然に拾える。
//!
//! 引用なし `trap func SIG` (POSIX 推奨外) は tree-sitter-bash で `word` ノード
//! として parse され、通常の identifier 走査で既に拾われるため、本モジュールでは
//! 二重カウント防止のため `raw_string` / `string` のみを処理対象とする。

use tree_sitter::Node;

use crate::engine::parser;
use crate::language::LangId;

/// `command` ノードを受け取り、trap handler 内の参照候補を抽出して返す。
///
/// 戻り値の `(name, row, col)` はファイル全体での 0-indexed 位置 (`row` は行番号、
/// `col` はバイトカラム)。Bash 以外 / trap 以外 / handler 文字列が空・reset・ignore
/// の場合は空 Vec を返す。
pub fn bash_trap_handler_ref_segments(
    node: Node<'_>,
    source: &[u8],
    lang_id: LangId,
) -> Vec<(String, usize, usize)> {
    if lang_id != LangId::Bash {
        return Vec::new();
    }
    if node.kind() != "command" {
        return Vec::new();
    }

    // command_name の子テキストが "trap" か確認
    let Some(name_node) = node.child_by_field_name("name") else {
        return Vec::new();
    };
    let name_text = match name_node.utf8_text(source) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    if name_text != "trap" {
        return Vec::new();
    }

    // argument 子を順に走査し、最初の handler 候補 (raw_string / string) を取得。
    // `trap -p`, `trap -l`, `trap --` のようなオプションのみの呼び出しは skip。
    // `trap -- 'handler' INT` (区切り `--`) は raw_string を handler として採用。
    let mut cursor = node.walk();
    let mut handler: Option<HandlerArg<'_>> = None;
    for child in node.children_by_field_name("argument", &mut cursor) {
        match child.kind() {
            "raw_string" => {
                handler = Some(HandlerArg::RawString(child));
                break;
            }
            "string" => {
                handler = Some(HandlerArg::DoubleString(child));
                break;
            }
            // 引用なし `word` (`trap func SIG`) はここで handler 候補としては
            // 採用しない。通常の identifier 走査で拾われるため、二重カウント防止。
            // `--` や `-p` `-l` 等のオプションは skip。
            _ => continue,
        }
    }

    let Some(handler) = handler else {
        return Vec::new();
    };

    // クォート位置と内側バイト列を取り出す。
    // outer_row / outer_col は handler 文字列「内側 1 バイト目」のファイル内位置。
    let outer_node = match handler {
        HandlerArg::RawString(n) => n,
        HandlerArg::DoubleString(n) => n,
    };
    let outer_start = outer_node.start_position();
    let outer_range = outer_node.byte_range();
    let outer_bytes = &source[outer_range.start..outer_range.end];

    // クォートを剥がした内側 byte slice を取得。
    // raw_string: `'...'` → 先頭・末尾 1 バイト除去
    // string: `"..."` → 先頭・末尾 1 バイト除去 (start_position が `"` の位置のため
    // 内側オフセットも +1)
    if outer_bytes.len() < 2 {
        return Vec::new();
    }
    let inner = &outer_bytes[1..outer_bytes.len() - 1];

    // reset / ignore は参照なし
    if inner.is_empty() || inner == b"-" {
        return Vec::new();
    }

    // handler 文字列を Bash として再 parse
    let Ok(inner_tree) = parser::parse_source(inner, LangId::Bash) else {
        return Vec::new();
    };

    let inner_root = inner_tree.root_node();
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    collect_command_names(
        inner_root,
        inner,
        &mut out,
        outer_start.row,
        outer_start.column,
    );

    out
}

enum HandlerArg<'a> {
    /// `'...'` シングルクォート文字列 (raw_string)
    RawString(Node<'a>),
    /// `"..."` ダブルクォート文字列 (string)。`${var}` 等の展開を含む場合あり。
    DoubleString(Node<'a>),
}

/// 内側 AST を再帰走査し、`command_name` を再帰的に拾う。
///
/// `command_name` の子が `word` 等で静的に名前が取れる場合のみ採用する。
/// `${var}` のような動的コマンド名は skip (utf8_text が取れても変数展開ノードを
/// 含むため、ここでは「子が word 単一」のみを採用)。
fn collect_command_names(
    node: Node<'_>,
    inner_bytes: &[u8],
    out: &mut Vec<(String, usize, usize)>,
    outer_row: usize,
    outer_col: usize,
) {
    if node.kind() == "command_name" {
        // command_name の子を確認: 静的な word が単独で存在する場合のみ採用
        let mut child_cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut child_cursor).collect();
        if children.len() == 1
            && children[0].kind() == "word"
            && let Ok(name_text) = children[0].utf8_text(inner_bytes)
        {
            let inner_pos = children[0].start_position();
            let (row, col) =
                translate_position(inner_pos.row, inner_pos.column, outer_row, outer_col);
            out.push((name_text.to_string(), row, col));
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_command_names(child, inner_bytes, out, outer_row, outer_col);
    }
}

/// 内側 AST 内の (row, col) を、ファイル全体での (row, col) に変換する。
///
/// 外側 quote 開始位置 (outer_row, outer_col) はクォート文字 `'` / `"` 自体の位置。
/// 内側 1 バイト目はファイル上で `(outer_row, outer_col + 1)` に対応する。
/// 内側に改行がある場合、行頭からの列は内側オフセットそのままになる。
fn translate_position(
    inner_row: usize,
    inner_col: usize,
    outer_row: usize,
    outer_col: usize,
) -> (usize, usize) {
    if inner_row == 0 {
        // 同一行: outer_col + 1 (opening quote) + inner_col
        (outer_row, outer_col + 1 + inner_col)
    } else {
        // 改行を跨いだ後: row を相対加算、col は内側オフセットのまま
        (outer_row + inner_row, inner_col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_bash(src: &str) -> (tree_sitter::Tree, Vec<u8>) {
        let bytes = src.as_bytes().to_vec();
        let tree = parser::parse_source(&bytes, LangId::Bash).expect("parse bash");
        (tree, bytes)
    }

    fn find_first_trap_command<'a>(node: Node<'a>, src: &[u8]) -> Option<Node<'a>> {
        if node.kind() == "command"
            && let Some(name) = node.child_by_field_name("name")
            && let Ok(t) = name.utf8_text(src)
            && t == "trap"
        {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = find_first_trap_command(child, src) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn single_quoted_trap_resolves_handler_function() {
        let src = "trap 'cleanup_signal 130' INT\n";
        let (tree, bytes) = parse_bash(src);
        let trap = find_first_trap_command(tree.root_node(), &bytes).expect("trap node");
        let segs = bash_trap_handler_ref_segments(trap, &bytes, LangId::Bash);
        assert_eq!(segs.len(), 1, "{segs:?}");
        assert_eq!(segs[0].0, "cleanup_signal");
        // 'cleanup_signal' は trap の 5+1=6 列目から
        assert_eq!(segs[0].1, 0);
        assert_eq!(segs[0].2, 6);
    }

    #[test]
    fn double_quoted_trap_resolves_handler_function() {
        let src = "trap \"cleanup_signal 130\" INT\n";
        let (tree, bytes) = parse_bash(src);
        let trap = find_first_trap_command(tree.root_node(), &bytes).expect("trap node");
        let segs = bash_trap_handler_ref_segments(trap, &bytes, LangId::Bash);
        assert_eq!(segs.len(), 1, "{segs:?}");
        assert_eq!(segs[0].0, "cleanup_signal");
    }

    #[test]
    fn trap_with_multiple_commands_separated_by_semicolon() {
        let src = "trap 'first_cleanup; second_cleanup' EXIT\n";
        let (tree, bytes) = parse_bash(src);
        let trap = find_first_trap_command(tree.root_node(), &bytes).expect("trap node");
        let segs = bash_trap_handler_ref_segments(trap, &bytes, LangId::Bash);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"first_cleanup"), "{names:?}");
        assert!(names.contains(&"second_cleanup"), "{names:?}");
    }

    #[test]
    fn trap_with_dash_argument_is_reset_no_refs() {
        let src = "trap - INT\n";
        let (tree, bytes) = parse_bash(src);
        let trap = find_first_trap_command(tree.root_node(), &bytes).expect("trap node");
        let segs = bash_trap_handler_ref_segments(trap, &bytes, LangId::Bash);
        // `-` は word として argument に来るが、handler 候補は raw_string/string のみなので空
        assert!(segs.is_empty(), "{segs:?}");
    }

    #[test]
    fn trap_with_empty_single_quotes_is_ignore_no_refs() {
        let src = "trap '' INT\n";
        let (tree, bytes) = parse_bash(src);
        let trap = find_first_trap_command(tree.root_node(), &bytes).expect("trap node");
        let segs = bash_trap_handler_ref_segments(trap, &bytes, LangId::Bash);
        assert!(segs.is_empty(), "{segs:?}");
    }

    #[test]
    fn trap_with_empty_double_quotes_is_ignore_no_refs() {
        let src = "trap \"\" INT\n";
        let (tree, bytes) = parse_bash(src);
        let trap = find_first_trap_command(tree.root_node(), &bytes).expect("trap node");
        let segs = bash_trap_handler_ref_segments(trap, &bytes, LangId::Bash);
        assert!(segs.is_empty(), "{segs:?}");
    }

    #[test]
    fn trap_with_unquoted_word_is_not_processed_here() {
        // POSIX 推奨外。`word` は通常 identifier 走査で拾われるため二重カウント防止で
        // ここでは何も返さない。
        let src = "trap cleanup_signal INT\n";
        let (tree, bytes) = parse_bash(src);
        let trap = find_first_trap_command(tree.root_node(), &bytes).expect("trap node");
        let segs = bash_trap_handler_ref_segments(trap, &bytes, LangId::Bash);
        assert!(segs.is_empty(), "{segs:?}");
    }

    #[test]
    fn non_bash_language_returns_empty() {
        // Bash 以外の言語では何も返さない (この helper を Rust ファイルで呼んでも no-op)
        let src = "trap 'cleanup_signal 130' INT\n";
        let (tree, bytes) = parse_bash(src);
        let trap = find_first_trap_command(tree.root_node(), &bytes).expect("trap node");
        let segs = bash_trap_handler_ref_segments(trap, &bytes, LangId::Rust);
        assert!(segs.is_empty(), "{segs:?}");
    }

    #[test]
    fn non_command_node_returns_empty() {
        let src = "trap 'cleanup_signal' INT\n";
        let (tree, bytes) = parse_bash(src);
        // root (program) ノードを渡しても何も返さない
        let segs = bash_trap_handler_ref_segments(tree.root_node(), &bytes, LangId::Bash);
        assert!(segs.is_empty(), "{segs:?}");
    }

    #[test]
    fn trap_with_compound_statement_handler_resolves_inner_call() {
        // `trap '{ cleanup_signal 130; }' INT` のような複合構文
        let src = "trap '{ cleanup_signal 130; }' INT\n";
        let (tree, bytes) = parse_bash(src);
        let trap = find_first_trap_command(tree.root_node(), &bytes).expect("trap node");
        let segs = bash_trap_handler_ref_segments(trap, &bytes, LangId::Bash);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"cleanup_signal"), "{names:?}");
    }
}
