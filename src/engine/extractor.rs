use crate::models::ast_node::{AstEdge, AstNode};
use crate::models::location::Range;
use tree_sitter::{Node, Point};

/// Extract AST nodes at or around the given position.
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

/// Extract AST nodes within a given range.
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
        // Fallback: return deepest node at start
        if let Some(n) = find_deepest_node_at(root, start) {
            results.push(node_to_ast(n, source, depth));
        }
    }

    results
}

/// Extract the full AST (root level) with limited depth.
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

    // Skip if entirely outside range
    if node_end.row < start.row || (node_end.row == start.row && node_end.column < start.column) {
        return;
    }
    if node_start.row > end.row || (node_start.row == end.row && node_start.column > end.column) {
        return;
    }

    // If node is fully within range, include it
    if node.is_named() && is_within(node_start, node_end, start, end) {
        results.push(node_to_ast(node, source, depth));
        return;
    }

    // Otherwise recurse into children
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

/// Remove common leading whitespace from multi-line text.
fn dedent(s: &str) -> String {
    if !s.contains('\n') {
        return s.replace('\t', " ");
    }
    let lines: Vec<&str> = s.lines().collect();
    let min_indent = lines
        .iter()
        .skip(1) // first line has no leading indent (starts at node column)
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
        node.children(&mut cursor)
            .filter(|c| c.is_named())
            .map(|child| {
                let field = node
                    .field_name_for_named_child(child.id() as u32)
                    .map(|s| s.to_string());
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
