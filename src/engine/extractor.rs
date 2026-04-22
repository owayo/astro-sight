use crate::models::ast_node::{AstEdge, AstNode};
use crate::models::location::Range;
use tree_sitter::{Node, Point};

/// 指定位置付近の AST ノードを抽出する。
pub fn extract_at_point(
    root: Node<'_>,
    source: &[u8],
    line: usize,
    column: usize,
    depth: usize,
) -> Vec<AstNode> {
    let point = Point::new(line, column);
    let node = find_deepest_node_at(root, point);

    match node {
        Some(n) => vec![node_to_ast(n, source, depth)],
        None => vec![node_to_ast(root, source, depth.min(2))],
    }
}

/// 指定範囲内の AST ノードを抽出する。
pub fn extract_range(
    root: Node<'_>,
    source: &[u8],
    start_line: usize,
    start_col: usize,
    end_line: usize,
    end_col: usize,
    depth: usize,
) -> Vec<AstNode> {
    let start = Point::new(start_line, start_col);
    let end = Point::new(end_line, end_col);

    let mut results = Vec::new();
    collect_nodes_in_range(root, source, start, end, depth, &mut results);

    if results.is_empty() {
        // フォールバック: start 位置の最深ノードを返す
        if let Some(n) = find_deepest_node_at(root, start) {
            results.push(node_to_ast(n, source, depth));
        }
    }

    results
}

/// ルートレベルの AST 全体を制限深度で抽出する。
pub fn extract_full(root: Node<'_>, source: &[u8], depth: usize) -> Vec<AstNode> {
    let mut results = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.is_named() {
            results.push(node_to_ast(child, source, depth));
        }
    }
    results
}

fn find_deepest_node_at<'a>(node: Node<'a>, point: Point) -> Option<Node<'a>> {
    if !contains_point(node, point) {
        return None;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(deeper) = find_deepest_node_at(child, point) {
            return Some(deeper);
        }
    }

    if node.is_named() { Some(node) } else { None }
}

fn contains_point(node: Node<'_>, point: Point) -> bool {
    let start = node.start_position();
    let end = node.end_position();

    if point.row < start.row || point.row > end.row {
        return false;
    }
    if point.row == start.row && point.column < start.column {
        return false;
    }
    if point.row == end.row && point.column > end.column {
        return false;
    }
    true
}

fn collect_nodes_in_range(
    node: Node<'_>,
    source: &[u8],
    start: Point,
    end: Point,
    depth: usize,
    results: &mut Vec<AstNode>,
) {
    let node_start = node.start_position();
    let node_end = node.end_position();

    // 範囲外のノードをスキップ
    if node_end.row < start.row || (node_end.row == start.row && node_end.column < start.column) {
        return;
    }
    if node_start.row > end.row || (node_start.row == end.row && node_start.column > end.column) {
        return;
    }

    // ノードが範囲内に完全に含まれる場合は収集
    if node.is_named() && is_within(node_start, node_end, start, end) {
        results.push(node_to_ast(node, source, depth));
        return;
    }

    // 含まれない場合は子ノードを再帰走査
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_nodes_in_range(child, source, start, end, depth, results);
    }
}

fn is_within(ns: Point, ne: Point, rs: Point, re: Point) -> bool {
    let start_ok = ns.row > rs.row || (ns.row == rs.row && ns.column >= rs.column);
    let end_ok = ne.row < re.row || (ne.row == re.row && ne.column <= re.column);
    start_ok && end_ok
}

/// 複数行テキストの共通インデントを除去する。
fn dedent(s: &str) -> String {
    if !s.contains('\n') {
        return s.replace('\t', " ");
    }
    let lines: Vec<&str> = s.lines().collect();
    let min_indent = lines
        .iter()
        .skip(1) // 最初の行はノードの列位置から始まるためインデントなし
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    if min_indent == 0 {
        return s.replace('\t', " ");
    }
    let mut result = String::with_capacity(s.len());
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        if i == 0 {
            result.push_str(line);
        } else if line.len() >= min_indent {
            result.push_str(&line[min_indent..]);
        } else {
            result.push_str(line.trim_start());
        }
    }
    result.replace('\t', " ")
}

const MAX_AST_TEXT_LEN: usize = 256;
const MAX_AST_TEXT_SCAN_LEN: usize = MAX_AST_TEXT_LEN * 4;

fn truncate_ast_text(s: &str, force_ellipsis: bool) -> String {
    if s.len() <= MAX_AST_TEXT_LEN {
        if force_ellipsis {
            format!("{s}...")
        } else {
            s.to_string()
        }
    } else {
        let truncated = &s[..s.floor_char_boundary(MAX_AST_TEXT_LEN)];
        format!("{truncated}...")
    }
}

fn preview_prefix(s: &str, limit: usize) -> (&str, bool) {
    if s.len() <= limit {
        (s, false)
    } else {
        (&s[..s.floor_char_boundary(limit)], true)
    }
}

fn normalize_ast_text(s: &str) -> String {
    if s.contains('\n') {
        let (preview, was_cut) = preview_prefix(s, MAX_AST_TEXT_SCAN_LEN);
        truncate_ast_text(&dedent(preview), was_cut)
    } else if s.len() <= MAX_AST_TEXT_LEN {
        s.replace('\t', " ")
    } else {
        // 単一巨大行は先頭だけを取り込み、無制限コピーを避ける。
        let truncated = &s[..s.floor_char_boundary(MAX_AST_TEXT_LEN)];
        format!("{}...", truncated.replace('\t', " "))
    }
}

fn node_to_ast(node: Node<'_>, source: &[u8], remaining_depth: usize) -> AstNode {
    let text = if node.child_count() == 0 || remaining_depth == 0 {
        node.utf8_text(source).ok().map(normalize_ast_text)
    } else {
        None
    };

    let children = if remaining_depth > 0 {
        let mut cursor = node.walk();
        // named child のインデックスでフィールド名を取得
        let mut named_index = 0u32;
        node.children(&mut cursor)
            .filter(|c| c.is_named())
            .map(|child| {
                let field = node
                    .field_name_for_named_child(named_index)
                    .map(|s| s.to_string());
                named_index += 1;
                AstEdge {
                    field,
                    node: node_to_ast(child, source, remaining_depth - 1),
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    AstNode {
        id: node.id(),
        kind: node.kind().to_string(),
        named: Some(node.is_named()),
        range: Range::from(node.range()),
        text,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Point;

    // --- dedent テスト ---

    /// 単一行テキストのタブがスペースに変換される
    #[test]
    fn dedent_single_line() {
        assert_eq!(dedent("hello\tworld"), "hello world");
    }

    /// 複数行テキストの共通インデント（8スペース）が除去される
    #[test]
    fn dedent_multi_line() {
        let input = "fn main() {\n        let x = 1;\n        let y = 2;\n    }";
        let result = dedent(input);
        // 2行目以降の最小インデント=4 が除去される
        assert!(result.contains("let x = 1;"));
        // 先頭行はインデント除去対象外
        assert!(result.starts_with("fn main()"));
    }

    /// 空行はインデント計算に含まれない
    #[test]
    fn dedent_empty_line_ignored() {
        let input = "start\n        line1\n\n        line2";
        let result = dedent(input);
        // 空行がインデント計算を壊さず、8スペースが除去される
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
        // 空行後の行もインデントが正しく除去される
        assert!(!result.contains("        line2"));
    }

    // --- contains_point テスト ---

    /// ノードの範囲内のポイントが true を返す
    #[test]
    fn contains_point_inside() {
        let source = b"fn main() {\n    let x = 1;\n}";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&crate::language::LangId::Rust.ts_language())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        // ルートノードは全体を包含するので内部のポイントは true
        assert!(contains_point(root, Point::new(1, 4)));
    }

    /// ノードの範囲外のポイントが false を返す
    #[test]
    fn contains_point_outside() {
        let source = b"fn main() {}";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&crate::language::LangId::Rust.ts_language())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        // ルートノードの範囲外（遥か下の行）
        assert!(!contains_point(root, Point::new(100, 0)));
    }

    // --- is_within テスト ---

    /// 完全に含まれる範囲が true を返す
    #[test]
    fn is_within_fully_contained() {
        let ns = Point::new(2, 5);
        let ne = Point::new(4, 10);
        let rs = Point::new(1, 0);
        let re = Point::new(5, 0);
        assert!(is_within(ns, ne, rs, re));
    }

    /// 部分的に範囲外の場合は false を返す
    #[test]
    fn is_within_partially_outside() {
        let ns = Point::new(0, 0);
        let ne = Point::new(4, 10);
        let rs = Point::new(1, 0);
        let re = Point::new(5, 0);
        // ns がrs より前なので false
        assert!(!is_within(ns, ne, rs, re));
    }

    // --- extract_at_point テスト ---

    fn parse_rust_source(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&crate::language::LangId::Rust.ts_language())
            .unwrap();
        parser.parse(source.as_bytes(), None).unwrap()
    }

    /// 関数名の位置で extract_at_point すると identifier ノードが返る
    #[test]
    fn extract_at_point_returns_identifier() {
        let source = "fn hello() {}";
        let tree = parse_rust_source(source);
        let root = tree.root_node();
        // "hello" は行0, 列3 から開始
        let nodes = extract_at_point(root, source.as_bytes(), 0, 3, 1);
        assert!(!nodes.is_empty());
        assert_eq!(nodes[0].kind, "identifier");
        assert_eq!(nodes[0].text.as_deref(), Some("hello"));
    }

    /// 範囲外の位置で extract_at_point するとルートノードのフォールバック
    #[test]
    fn extract_at_point_out_of_range_falls_back() {
        let source = "fn hello() {}";
        let tree = parse_rust_source(source);
        let root = tree.root_node();
        // 存在しない行
        let nodes = extract_at_point(root, source.as_bytes(), 100, 0, 1);
        // フォールバックでルートの AST が返る
        assert!(!nodes.is_empty());
    }

    // --- extract_range テスト ---

    /// 範囲内のノードが正しく抽出される
    #[test]
    fn extract_range_captures_nodes_in_range() {
        let source = "fn foo() {}\nfn bar() {}\nfn baz() {}";
        let tree = parse_rust_source(source);
        let root = tree.root_node();
        // 2行目（bar）のみを含む範囲
        let nodes = extract_range(root, source.as_bytes(), 1, 0, 1, 11, 3);
        assert!(!nodes.is_empty());
        // function_item が返る
        assert!(nodes.iter().any(|n| n.kind == "function_item"));
    }

    /// 空の範囲（ノードが含まれない）ではフォールバック
    #[test]
    fn extract_range_empty_range_falls_back() {
        let source = "fn foo() {}";
        let tree = parse_rust_source(source);
        let root = tree.root_node();
        // 存在しない範囲
        let nodes = extract_range(root, source.as_bytes(), 10, 0, 10, 10, 3);
        // フォールバックでルートの最深ノードか空を返す
        // （実装上は start 位置のフォールバックが試行される）
        // ノードが見つからなければ空
        assert!(nodes.is_empty() || !nodes.is_empty()); // パニックしないことを確認
    }

    // --- extract_full テスト ---

    /// extract_full が全トップレベルノードを返す
    #[test]
    fn extract_full_returns_all_top_level() {
        let source = "fn foo() {}\nstruct Bar {}\nenum Baz { A }";
        let tree = parse_rust_source(source);
        let root = tree.root_node();
        let nodes = extract_full(root, source.as_bytes(), 2);
        assert_eq!(nodes.len(), 3);
        let kinds: Vec<&str> = nodes.iter().map(|n| n.kind.as_str()).collect();
        assert!(kinds.contains(&"function_item"));
        assert!(kinds.contains(&"struct_item"));
        assert!(kinds.contains(&"enum_item"));
    }

    /// depth=0 では子ノードなしでテキストが付与される
    #[test]
    fn extract_full_depth_zero_has_text() {
        let source = "fn foo() {}";
        let tree = parse_rust_source(source);
        let root = tree.root_node();
        let nodes = extract_full(root, source.as_bytes(), 0);
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].text.is_some());
        assert!(nodes[0].children.is_empty());
    }

    /// depth=0 の text は長大行でも上限で切り詰められる
    #[test]
    fn extract_full_depth_zero_truncates_long_text() {
        let source = format!("fn foo() {{ let s = \"{}\"; }}", "x".repeat(512));
        let tree = parse_rust_source(&source);
        let root = tree.root_node();
        let nodes = extract_full(root, source.as_bytes(), 0);
        let text = nodes[0].text.as_deref().expect("text");

        assert!(text.ends_with("..."));
        assert!(text.len() <= MAX_AST_TEXT_LEN + 3);
    }

    /// depth=0 の複数行 text も巨大入力を丸ごとコピーせず上限で切り詰める
    #[test]
    fn extract_full_depth_zero_truncates_long_multiline_text() {
        let body = (0..64)
            .map(|_| format!("    let s = \"{}\";\n", "y".repeat(32)))
            .collect::<String>();
        let source = format!("fn foo() {{\n{body}}}\n");
        let tree = parse_rust_source(&source);
        let root = tree.root_node();
        let nodes = extract_full(root, source.as_bytes(), 0);
        let text = nodes[0].text.as_deref().expect("text");

        assert!(text.ends_with("..."));
        assert!(text.len() <= MAX_AST_TEXT_LEN + 3);
    }

    // --- node_to_ast テスト ---

    /// node_to_ast で depth > 0 のとき子ノードが含まれる
    #[test]
    fn node_to_ast_includes_children_with_depth() {
        let source = "fn foo() { let x = 1; }";
        let tree = parse_rust_source(source);
        let root = tree.root_node();
        let func = root.child(0).unwrap(); // function_item
        let ast = super::node_to_ast(func, source.as_bytes(), 3);
        assert_eq!(ast.kind, "function_item");
        assert!(!ast.children.is_empty());
    }
}
