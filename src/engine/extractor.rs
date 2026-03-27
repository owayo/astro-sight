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

fn node_to_ast(node: Node<'_>, source: &[u8], remaining_depth: usize) -> AstNode {
    let text = if node.child_count() == 0 || remaining_depth == 0 {
        node.utf8_text(source).ok().map(dedent)
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
}
