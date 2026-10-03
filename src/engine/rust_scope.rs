//! impact 用の Rust 値束縛の証明。関数以外への名前解決は断定しない。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::lexical_binding::LexicalBinding;
use super::refs::is_rust_shadowable_value_identifier;

const PATTERN_WRAPPERS: &[&str] = &[
    "tuple_pattern",
    "slice_pattern",
    "reference_pattern",
    "ref_pattern",
    "mut_pattern",
    "captured_pattern",
    "or_pattern",
];
const ITEM_BOUNDARIES: &[&str] = &[
    "impl_item",
    "trait_item",
    "mod_item",
    "const_item",
    "static_item",
    "const_block",
];
const OPAQUE_SCOPES: &[&str] = &[
    "for_expression",
    "match_expression",
    "match_arm",
    "let_condition",
    "let_chain",
];

#[derive(Default)]
struct BlockFacts {
    /// 宣言順の開始位置。参照ごとに前方の文を走査しない。
    lets: HashMap<String, Vec<LetBinding>>,
    barrier: bool,
}

struct LetBinding {
    start: usize,
    proven: bool,
}

pub(crate) struct RustScopeIndex {
    blocks: HashMap<usize, BlockFacts>,
    parameters: HashMap<usize, HashSet<String>>,
    assignment_names: HashMap<usize, HashSet<String>>,
    pattern_positions: HashSet<usize>,
    unknown_positions: HashSet<usize>,
}

impl RustScopeIndex {
    /// walk の途中から呼ばれても、解析エラーと巻き上げ宣言を木全体で確認する。
    pub(crate) fn build(mut root: Node<'_>, source: &[u8]) -> Option<Self> {
        while let Some(parent) = root.parent() {
            root = parent;
        }
        if root.has_error() {
            return None;
        }
        let mut index = Self {
            blocks: HashMap::new(),
            parameters: HashMap::new(),
            assignment_names: HashMap::new(),
            pattern_positions: HashSet::new(),
            unknown_positions: HashSet::new(),
        };
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            if node.is_missing() {
                return None;
            }
            match node.kind() {
                "assignment_expression" => {
                    if let (Some(left), Some(right)) = (
                        node.child_by_field_name("left"),
                        node.child_by_field_name("right"),
                    ) {
                        // tuple/field を含む代入も、同名依存があれば束縛を証明しない。
                        let left_names = subtree_names(left, source).0;
                        let right_names = subtree_names(right, source).0;
                        let names: HashSet<_> = left_names
                            .into_iter()
                            .filter(|name| right_names.contains(name))
                            .collect();
                        if !names.is_empty() {
                            let mut enclosing = node.parent();
                            while let Some(parent) = enclosing {
                                if parent.kind() == "function_item" {
                                    index
                                        .assignment_names
                                        .entry(parent.id())
                                        .or_default()
                                        .extend(names);
                                    break;
                                }
                                if ITEM_BOUNDARIES.contains(&parent.kind()) {
                                    break;
                                }
                                enclosing = parent.parent();
                            }
                        }
                    }
                }
                "block" => {
                    let facts = index.block_facts(node, source);
                    index.blocks.insert(node.id(), facts);
                }
                "function_item" => {
                    let mut names = HashSet::new();
                    if let Some(parameters) = node.child_by_field_name("parameters") {
                        let mut cursor = parameters.walk();
                        let mut attributed = false;
                        for parameter in parameters.named_children(&mut cursor) {
                            match parameter.kind() {
                                "attribute_item" => attributed = true,
                                "line_comment" | "block_comment" => {}
                                _ => {
                                    if !attributed
                                        && parameter.kind() == "parameter"
                                        && let Some(pattern) =
                                            parameter.child_by_field_name("pattern")
                                    {
                                        for target in pattern_targets(pattern) {
                                            if let Ok(name) = target.utf8_text(source) {
                                                names.insert(binding_name(name).to_owned());
                                                index.pattern_positions.insert(target.id());
                                            }
                                        }
                                    }
                                    attributed = false;
                                }
                            }
                        }
                    }
                    index.parameters.insert(node.id(), names);
                }
                _ => {}
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return Some(index);
                }
            }
        }
    }

    fn block_facts(&mut self, block: Node<'_>, source: &[u8]) -> BlockFacts {
        let mut facts = BlockFacts::default();
        let mut cursor = block.walk();
        let mut attributed = false;
        for statement in block.named_children(&mut cursor) {
            if statement.kind() == "attribute_item" {
                attributed = true;
                continue;
            }
            if matches!(statement.kind(), "line_comment" | "block_comment") {
                continue;
            }
            // item/use は block 全体へ巻き上がる。文マクロも未知の再束縛を生成し得る。
            if statement.kind().ends_with("_item")
                || matches!(
                    statement.kind(),
                    "use_declaration" | "macro_definition" | "macro_invocation"
                )
                || (statement.kind() == "expression_statement"
                    && statement
                        .named_child(0)
                        .is_some_and(|child| child.kind() == "macro_invocation"))
            {
                facts.barrier = true;
            }
            if statement.kind() == "let_declaration"
                && let Some(pattern) = statement.child_by_field_name("pattern")
            {
                let (_, pattern_has_macro) = subtree_names(pattern, source);
                if attributed || pattern_has_macro {
                    facts.barrier = true;
                }
                // 複数の束縛があっても初期化子は一度だけ走査する。
                let rhs_names = statement
                    .child_by_field_name("value")
                    .map(|value| subtree_names(value, source).0)
                    .unwrap_or_default();
                for target in pattern_targets(pattern) {
                    if let Ok(name) = target.utf8_text(source) {
                        let name = binding_name(name);
                        let proven = !attributed && !rhs_names.contains(name);
                        facts
                            .lets
                            .entry(name.to_owned())
                            .or_default()
                            .push(LetBinding {
                                start: statement.end_byte(),
                                proven,
                            });
                        if proven {
                            self.pattern_positions.insert(target.id());
                        } else {
                            self.unknown_positions.insert(target.id());
                        }
                    }
                }
            }
            attributed = false;
        }
        facts
    }

    pub(crate) fn resolve(&self, node: Node<'_>, source: &[u8]) -> LexicalBinding {
        if self.unknown_positions.contains(&node.id()) {
            return LexicalBinding::Unknown;
        }
        let pattern_position = self.pattern_positions.contains(&node.id());
        if !pattern_position && !is_rust_shadowable_value_identifier(node) {
            return LexicalBinding::Unknown;
        }
        let Ok(name) = node.utf8_text(source) else {
            return LexicalBinding::Unknown;
        };
        let name = binding_name(name);
        let mut proven = pattern_position;
        let mut child = node;
        while let Some(parent) = child.parent() {
            if OPAQUE_SCOPES.contains(&parent.kind())
                || ITEM_BOUNDARIES.contains(&parent.kind())
                || matches!(
                    parent.kind(),
                    "token_tree"
                        | "attribute_item"
                        | "type_arguments"
                        | "type_binding"
                        | "scoped_identifier"
                        | "scoped_type_identifier"
                )
                || field_contains(parent, "type", node)
                || field_contains(parent, "return_type", node)
            {
                return LexicalBinding::Unknown;
            }
            match parent.kind() {
                "if_expression" | "while_expression"
                    if parent
                        .child_by_field_name("condition")
                        .is_some_and(|condition| {
                            matches!(condition.kind(), "let_condition" | "let_chain")
                        }) =>
                {
                    return LexicalBinding::Unknown;
                }
                "let_declaration" if field_contains(parent, "alternative", node) => {
                    return LexicalBinding::Unknown;
                }
                "closure_expression" => {
                    // 引数を持つ closure は初版の証明対象外。空引数の capture だけを通す。
                    if parent
                        .child_by_field_name("parameters")
                        .is_some_and(|parameters| parameters.named_child_count() != 0)
                    {
                        return LexicalBinding::Unknown;
                    }
                }
                "block" => {
                    let Some(facts) = self.blocks.get(&parent.id()) else {
                        return LexicalBinding::Unknown;
                    };
                    if facts.barrier {
                        return LexicalBinding::Unknown;
                    }
                    if let Some(bindings) = facts.lets.get(name) {
                        let latest =
                            bindings.partition_point(|binding| binding.start <= node.start_byte());
                        if latest != 0 {
                            if !bindings[latest - 1].proven {
                                return LexicalBinding::Unknown;
                            }
                            proven = true;
                        }
                    }
                }
                "function_item" => {
                    // ネストした item から外側のローカルへは進まない。
                    if self
                        .assignment_names
                        .get(&parent.id())
                        .is_some_and(|names| names.contains(name))
                    {
                        return LexicalBinding::Unknown;
                    }
                    if field_contains(parent, "body", node)
                        && self
                            .parameters
                            .get(&parent.id())
                            .is_some_and(|names| names.contains(name))
                    {
                        proven = true;
                    }
                    return if proven {
                        LexicalBinding::RustValueBinding
                    } else {
                        LexicalBinding::Unknown
                    };
                }
                _ => {}
            }
            child = parent;
        }
        LexicalBinding::Unknown
    }
}

fn field_contains(parent: Node<'_>, field: &str, node: Node<'_>) -> bool {
    parent.child_by_field_name(field).is_some_and(|field| {
        field.start_byte() <= node.start_byte() && node.end_byte() <= field.end_byte()
    })
}

fn binding_name(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

/// 修飾名の末尾や token_tree 内も含め、見えている同名依存は保守的に残す。
fn subtree_names(root: Node<'_>, source: &[u8]) -> (HashSet<String>, bool) {
    let mut names = HashSet::new();
    let mut has_macro = false;
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        has_macro |= matches!(node.kind(), "macro_invocation" | "token_tree");
        if node.kind().contains("identifier")
            && let Ok(name) = node.utf8_text(source)
        {
            names.insert(binding_name(name).to_owned());
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return (names, has_macro);
            }
        }
    }
}

fn pattern_targets(node: Node<'_>) -> Vec<Node<'_>> {
    let mut pending = vec![node];
    let mut out = Vec::new();
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier" | "shorthand_field_identifier" => out.push(node),
            "tuple_struct_pattern" | "struct_pattern" => {
                let type_node = node.child_by_field_name("type");
                let mut cursor = node.walk();
                pending.extend(
                    node.named_children(&mut cursor)
                        .filter(|child| type_node.is_none_or(|ty| ty.id() != child.id())),
                );
            }
            "field_pattern" => {
                if let Some(pattern) = node.child_by_field_name("pattern") {
                    pending.push(pattern);
                } else if let Some(name) = node.child_by_field_name("name")
                    && name.kind() == "shorthand_field_identifier"
                {
                    out.push(name);
                }
            }
            kind if PATTERN_WRAPPERS.contains(&kind) => {
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::parser, language::LangId};

    fn resolve_last(source: &str) -> LexicalBinding {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        assert!(!tree.root_node().has_error(), "{source}");
        let start = source.rfind("comments").unwrap();
        let node = tree
            .root_node()
            .descendant_for_byte_range(start, start + "comments".len())
            .unwrap();
        RustScopeIndex::build(node, source.as_bytes())
            .unwrap()
            .resolve(node, source.as_bytes())
    }

    #[test]
    fn rust_scope_proves_parameters_patterns_and_captured_values() {
        for prefix in ["", "use crate::api::comments;", "use crate::api::*;"] {
            for source in [
                "fn f() { let comments = vec![1]; comments.len(); }",
                "fn f() { let mut comments = 1; comments = value; comments; }",
                "fn other() { let comments; comments = crate::api::comments; } fn f() { let comments = 1; comments; }",
                "fn f(comments: Vec<u8>) { comments.len(); }",
                "fn f((comments, _): (u8,u8)) { comments; }",
                "fn f() { let (comments, _) = pair; comments; }",
                "fn f() { let [comments, ..] = pair; comments; }",
                "fn f() { let mut comments = 1; comments; }",
                "fn f() { let &comments = value; comments; }",
                "fn f() { let Some(comments) = value else { return; }; comments; }",
                "fn f() { let Data { comments, .. } = value; comments; }",
                "fn f() { let Data { x: comments } = value; comments; }",
                "fn f() { let comments = 1; { { comments; } } }",
                "fn f() { let comments = 1; if ready { comments; } }",
                "fn f() { let comments = 1; let capture = || comments; }",
                "fn f(comments: u8) { let capture = || comments; }",
                "fn f() { let comments = 1; let capture = async move { comments }; }",
                "fn f() { let comments = 1; Data { comments }; }",
                "fn outer() { let value = 1; fn inner() { let comments = vec![1]; comments.len(); } }",
                "fn f() { let comments = 1; }",
                "fn f(comments: u8) {}",
                "fn f() { let Data { comments } = value; }",
            ] {
                let source = format!("{prefix}\n{source}");
                assert_eq!(
                    resolve_last(&source),
                    LexicalBinding::RustValueBinding,
                    "{source}"
                );
            }
        }
    }

    #[test]
    fn rust_scope_keeps_order_boundaries_attributes_and_unknown_scopes() {
        for source in [
            "fn f() { let comments = comments(); }",
            "fn f() { let comments; comments = crate::api::comments; comments(); }",
            "fn f() { let mut comments = || 1; comments = crate::api::comments; comments(); }",
            "fn f(mut comments: fn()) { comments = crate::api::comments; comments(); }",
            "fn f() { let comments; (comments,) = (crate::api::comments,); comments(); }",
            "fn f() { let comments; comments = crate::api::r#comments; comments(); }",
            "fn f() { let r#comments; r#comments = crate::api::comments; comments(); }",
            "fn f() { let comments; comments = identity!(crate::api::comments); comments(); }",
            "fn f() { let comments; let capture = || { comments = crate::api::comments; }; comments(); }",
            "fn f() { let comments = 1; object.comments = crate::api::comments; comments(); }",
            "fn f() { let comments = || comments(); }",
            "fn f() { comments(); let value = 1; }",
            "fn f() { { let comments = 1; } comments(); }",
            "fn f() { let Some(comments) = value else { comments(); return; }; }",
            "fn f() { let comments = 1; fn inner() { comments(); } }",
            "fn f() { let comments = 1; let value = const { comments() }; }",
            "fn f() { let comments = 1; const VALUE: u8 = comments(); }",
            "fn f() { let comments = 1; static VALUE: u8 = comments(); }",
            "fn f() { let comments = 1; mod inner { fn f() { comments(); } } }",
            "fn f() { let comments = 1; impl Data { fn f() { comments(); } } }",
            "fn f() { let comments = 1; { use crate::api::comments; comments(); } }",
            "fn f() { let comments = 1; { use crate::api::*; comments(); } }",
            "fn f() { let comments = 1; comments(); fn comments() {} }",
            "fn f() { let comments = 1; { comments(); fn unrelated() {} } }",
            "fn f() { #[cfg(feature=\"x\")] let comments = 1; comments(); }",
            "fn f(#[cfg(feature=\"x\")] comments: u8) { comments(); }",
            "fn f(#[cfg(feature=\"x\")] /* note */ comments: u8) { comments(); }",
            "fn f() { let comments = 1; create_binding!(); comments(); }",
            "fn f() { let comments = 1; comments!(); }",
            "fn f() { let comments = 1; dbg!(comments); }",
            "fn f() { let comments = 1; crate::api::comments(); }",
            "fn f() { let comments = 1; self::comments(); }",
            "fn f() { let comments = 1; object.comments(); }",
            "fn f() { let comments = 1; let value: comments = input; }",
            "fn f() { let comments = 1; let value: [u8; comments] = input; }",
            "fn f(comments: u8) -> [u8; comments] { value }",
            "fn f() { let comments = 1; for value in values { comments(); } }",
            "fn f() { let comments = 1; match value { _ => comments() }; }",
            "fn f() { let comments = 1; if let Some(x) = value { comments(); } }",
            "fn f() { let comments = 1; while let Some(x) = value { comments(); } }",
            "fn f() { let comments = 1; if ready && let Some(x) = value { comments(); } }",
            "fn f() { let comments = 1; let capture = |x| comments(); }",
            "fn f() { let comments = 1; let Data { comments: other } = value; }",
            "fn f() { let comments = 1; let comments(_) = value; }",
            "fn f() { let comments = crate::api::comments; comments(); }",
            "fn f() { let comments = 1; let comments = crate::api::comments; comments(); }",
            "fn f(comments: usize) { let comments = crate::api::comments; comments(); }",
            "fn f() { let comments = 1; let comments = comments; comments; }",
            "fn f() { let comments = || crate::api::comments(); comments(); }",
            "fn f() { let comments = identity!(crate::api::comments); comments(); }",
            "fn f() { let comments = 1; #[cfg(feature=\"x\")] let comments = crate::api::comments; comments(); }",
            "fn f() { let comments = 1; let pattern!() = value; comments(); }",
            "fn f() { let comments = crate::api::r#comments; comments(); }",
            "fn f() { let r#comments = crate::api::comments; comments(); }",
        ] {
            assert_eq!(resolve_last(source), LexicalBinding::Unknown, "{source}");
        }
    }

    #[test]
    fn rust_scope_rejects_errors_and_does_not_reuse_another_walk() {
        for source in [
            "fn f() { let comments = 1; comments( }",
            "fn f() { let comments = 1; comments; } fn broken( {}",
            "fn f() { let comments = 1 comments; }",
            "fn f() { let closure = |#[cfg(feature=\"x\")] comments: u8| comments; }",
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let start = source.find("comments").unwrap();
            let node = tree
                .root_node()
                .descendant_for_byte_range(start, start + 8)
                .unwrap();
            assert!(
                RustScopeIndex::build(node, source.as_bytes()).is_none(),
                "{source}"
            );
        }
        let mut buffer = Vec::with_capacity(256);
        for source in [
            "fn f() { let comments = 1; comments; }",
            "fn f() { let comments = 1; bind!(); comments; }",
            "fn f() { let comments = 1; comments; }",
        ] {
            buffer.clear();
            buffer.extend_from_slice(source.as_bytes());
            buffer.resize(128, b' ');
            let tree = parser::parse_source(&buffer, LangId::Rust).unwrap();
            let start = source.rfind("comments").unwrap();
            let node = tree
                .root_node()
                .descendant_for_byte_range(start, start + 8)
                .unwrap();
            let resolved = RustScopeIndex::build(node, &buffer)
                .unwrap()
                .resolve(node, &buffer);
            assert_eq!(
                resolved == LexicalBinding::RustValueBinding,
                !source.contains("bind!")
            );
        }
    }

    #[test]
    fn rust_scope_node_tables_and_fields_exist_in_the_grammar() {
        let language = LangId::Rust.ts_language();
        for kind in PATTERN_WRAPPERS
            .iter()
            .chain(ITEM_BOUNDARIES)
            .chain(OPAQUE_SCOPES)
            .copied()
            .chain([
                "identifier",
                "shorthand_field_identifier",
                "tuple_struct_pattern",
                "struct_pattern",
                "field_pattern",
                "block",
                "function_item",
                "parameter",
                "attribute_item",
                "line_comment",
                "block_comment",
                "use_declaration",
                "macro_definition",
                "macro_invocation",
                "expression_statement",
                "let_declaration",
                "assignment_expression",
                "token_tree",
                "type_arguments",
                "type_binding",
                "scoped_identifier",
                "scoped_type_identifier",
                "if_expression",
                "while_expression",
                "closure_expression",
            ])
        {
            tree_sitter::Query::new(&language, &format!("({kind}) @node"))
                .unwrap_or_else(|error| panic!("{kind}: {error}"));
        }
        for query in [
            "(let_declaration pattern: (_) value: (_) alternative: (block))",
            "(assignment_expression left: (_) right: (_))",
            "(function_item parameters: (_) return_type: (_) body: (_))",
            "(parameter pattern: (_) type: (_))",
            "(closure_expression parameters: (_) body: (_))",
            "(if_expression condition: (_))",
            "(while_expression condition: (_))",
            "(struct_pattern type: (_))",
            "(tuple_struct_pattern type: (_))",
            "(field_pattern name: (_) pattern: (_))",
        ] {
            tree_sitter::Query::new(&language, query)
                .unwrap_or_else(|error| panic!("{query}: {error}"));
        }
    }
}
