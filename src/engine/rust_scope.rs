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
    functions: HashSet<String>,
    named_imports: HashSet<String>,
    barrier: bool,
    non_expression_macro_barrier: bool,
    // 呼び出し位置で新しい let を生成し得る文。既に終わった文は後の let を隠さない。
    dynamic_statements: Vec<usize>,
}

struct LetBinding {
    start: usize,
    declaration: usize,
    proven: bool,
}

pub(crate) struct RustScopeIndex {
    blocks: HashMap<usize, BlockFacts>,
    parameters: HashMap<usize, HashSet<String>>,
    assignment_names: HashMap<usize, HashSet<String>>,
    for_pattern_names: HashMap<usize, HashSet<String>>,
    scoped_patterns: HashMap<usize, HashSet<String>>,
    closure_patterns: HashSet<usize>,
    pattern_positions: HashSet<usize>,
    unknown_positions: HashSet<usize>,
}

impl RustScopeIndex {
    /// 可視性の変更時に、非修飾名の由来になり得るローカルな宣言があるか。
    /// パターンは定数名とも競合するので、import の到達性は呼び出し側で別に確認する。
    pub(crate) fn has_lexical_name(&self, name: &str, site: Node<'_>, scopes: &[Node<'_>]) -> bool {
        let name = binding_name(name);
        if self.pattern_positions.contains(&site.id())
            || self.unknown_positions.contains(&site.id())
        {
            return true;
        }
        for &scope in scopes.iter().rev() {
            let names = match scope.kind() {
                "function_item" => {
                    return self
                        .parameters
                        .get(&scope.id())
                        .is_some_and(|p| p.contains(name));
                }
                "block" => {
                    if self
                        .blocks
                        .get(&scope.id())
                        .and_then(|b| b.lets.get(name))
                        .and_then(|b| b.first())
                        .is_some_and(|binding| binding.start <= site.start_byte())
                    {
                        return true;
                    }
                    continue;
                }
                "for_expression"
                    if field_contains(scope, "pattern", site)
                        || field_contains(scope, "body", site) =>
                {
                    self.for_pattern_names.get(&scope.id())
                }
                "closure_expression" => self.scoped_patterns.get(&scope.id()),
                "let_condition" if field_contains(scope, "pattern", site) => {
                    self.scoped_patterns.get(&scope.id())
                }
                "if_expression" | "while_expression"
                    if field_contains(scope, "consequence", site)
                        || field_contains(scope, "body", site) =>
                {
                    scope
                        .child_by_field_name("condition")
                        .and_then(|n| self.scoped_patterns.get(&n.id()))
                }
                kind if ITEM_BOUNDARIES.contains(&kind) => return false,
                _ => continue,
            };
            if names.is_some_and(|names| names.contains(name)) {
                return true;
            }
        }
        false
    }

    pub(crate) fn blocks_without_imports(&self, path: &[Node<'_>]) -> bool {
        path.iter()
            .filter_map(|node| self.blocks.get(&node.id()))
            .all(|block| !block.barrier && block.named_imports.is_empty())
    }
    /// walk の途中から呼ばれても、解析エラーと巻き上げ宣言を木全体で確認する。
    pub(crate) fn build(root: Node<'_>, source: &[u8]) -> Option<Self> {
        Self::with_data_initializers(root, source, &HashSet::new())
    }

    pub(crate) fn with_data_initializers(
        mut root: Node<'_>,
        source: &[u8],
        data: &HashSet<usize>,
    ) -> Option<Self> {
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
            for_pattern_names: HashMap::new(),
            scoped_patterns: HashMap::new(),
            closure_patterns: HashSet::new(),
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
                "closure_expression" => {
                    if let Some(params) = node.child_by_field_name("parameters") {
                        let mut names = HashSet::new();
                        let mut cursor = params.walk();
                        let mut known = true;
                        for param in params.named_children(&mut cursor) {
                            let pattern = param.child_by_field_name("pattern").unwrap_or(param);
                            if matches!(param.kind(), "attribute_item" | "macro_invocation")
                                || subtree_names(pattern, source).1
                            {
                                known = false;
                                break;
                            }
                            for target in pattern_targets(pattern) {
                                if let Ok(name) = target.utf8_text(source) {
                                    names.insert(binding_name(name).to_owned());
                                    index.closure_patterns.insert(target.id());
                                }
                            }
                        }
                        if known {
                            index.scoped_patterns.insert(node.id(), names);
                        }
                    }
                }
                "match_arm" | "let_condition" => {
                    if let Some(pattern) = node.child_by_field_name("pattern") {
                        let (names, opaque) = subtree_names(pattern, source);
                        if !opaque && names.iter().all(|name| name.is_ascii()) {
                            index.scoped_patterns.insert(node.id(), names);
                        }
                    }
                }
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
                    let facts = index.block_facts(node, source, data);
                    index.blocks.insert(node.id(), facts);
                }
                "for_expression" => {
                    if let Some(pattern) = node.child_by_field_name("pattern") {
                        let (names, has_macro) = subtree_names(pattern, source);
                        if !has_macro && names.iter().all(|name| name.is_ascii()) {
                            index.for_pattern_names.insert(node.id(), names);
                        }
                    }
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

    fn block_facts(&mut self, block: Node<'_>, source: &[u8], data: &HashSet<usize>) -> BlockFacts {
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
            let invocation = if statement.kind() == "macro_invocation" {
                Some(statement)
            } else if statement.kind() == "expression_statement" {
                statement
                    .named_child(0)
                    .filter(|child| child.kind() == "macro_invocation")
            } else {
                None
            };
            if !attributed
                && statement.kind() == "function_item"
                && let Some(name) = statement
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
            {
                facts.functions.insert(binding_name(name).to_owned());
            }
            if attributed || invocation.is_some() {
                facts.dynamic_statements.push(statement.start_byte());
            }
            if !attributed
                && statement.kind() == "use_declaration"
                && let Some(path) = statement.child_by_field_name("argument")
                && let Some(name) = match path.kind() {
                    "identifier" => Some(path),
                    "scoped_identifier" => path.child_by_field_name("name"),
                    "use_as_clause" => path.child_by_field_name("alias"),
                    _ => None,
                }
                && let Ok(name) = name.utf8_text(source)
            {
                facts.named_imports.insert(
                    crate::language::normalize_identifier(crate::language::LangId::Rust, name)
                        .into_owned(),
                );
                continue;
            }
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
                if attributed
                    || !invocation
                        .is_some_and(|node| is_value_expression_macro_invocation(node, source))
                {
                    facts.non_expression_macro_barrier = true;
                }
            }
            if statement.kind() == "let_declaration"
                && let Some(pattern) = statement.child_by_field_name("pattern")
            {
                let (_, pattern_has_macro) = subtree_names(pattern, source);
                if attributed || pattern_has_macro {
                    facts.barrier = true;
                    facts.non_expression_macro_barrier = true;
                }
                // 複数の束縛があっても初期化子は一度だけ走査する。
                let rhs_names = statement
                    .child_by_field_name("value")
                    .filter(|value| !data.contains(&value.id()))
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
                                declaration: statement.id(),
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

    /// レシーバーの値を導入した let または関数引数の宣言を、走査中の祖先列から引く。
    pub(crate) fn receiver_binding(
        &self,
        name: &str,
        site: Node<'_>,
        ancestors: &[Node<'_>],
    ) -> Option<usize> {
        let name = binding_name(name);
        for &scope in ancestors.iter().rev() {
            match scope.kind() {
                "block" => {
                    let facts = self.blocks.get(&scope.id())?;
                    if facts.named_imports.contains(name) || facts.functions.contains(name) {
                        return None;
                    }
                    let binding = facts.lets.get(name).and_then(|lets| {
                        let end =
                            lets.partition_point(|binding| binding.start <= site.start_byte());
                        lets[..end].last()
                    });
                    let n = facts
                        .dynamic_statements
                        .partition_point(|&start| start < site.start_byte());
                    if n > 0 && binding.is_none_or(|b| facts.dynamic_statements[n - 1] >= b.start) {
                        return None;
                    }
                    if let Some(binding) = binding {
                        return Some(binding.declaration);
                    }
                    if facts.non_expression_macro_barrier {
                        return None;
                    }
                }
                "function_item" => {
                    return self
                        .parameters
                        .get(&scope.id())?
                        .contains(name)
                        .then_some(scope.id());
                }
                "for_expression" if !self.for_pattern_names.get(&scope.id())?.contains(name) => {}
                "closure_expression" | "match_arm" | "let_condition"
                    if !self.scoped_patterns.get(&scope.id())?.contains(name) => {}
                "if_expression" | "while_expression"
                    if scope.child_by_field_name("condition").is_some_and(|cond| {
                        matches!(cond.kind(), "let_condition" | "let_chain")
                    }) =>
                {
                    return None;
                }
                "let_declaration" if field_contains(scope, "alternative", site) => return None,
                kind if ITEM_BOUNDARIES.contains(&kind)
                    || matches!(
                        kind,
                        "for_expression"
                            | "closure_expression"
                            | "match_arm"
                            | "let_condition"
                            | "match_guard"
                            | "let_chain"
                    ) =>
                {
                    return None;
                }
                _ => {}
            }
        }
        None
    }

    pub(crate) fn resolve(&self, node: Node<'_>, source: &[u8]) -> LexicalBinding {
        self.resolve_inner(node, source, false)
    }

    pub(crate) fn impact_binding(&self, node: Node<'_>, source: &[u8]) -> LexicalBinding {
        let binding = self.resolve(node, source);
        if binding != LexicalBinding::RustValueBinding
            && self.resolve_inner(node, source, true) == LexicalBinding::RustValueBinding
        {
            return LexicalBinding::RustScopedValue;
        }
        binding
    }

    fn resolve_inner(&self, node: Node<'_>, source: &[u8], scoped: bool) -> LexicalBinding {
        if self.unknown_positions.contains(&node.id()) {
            return LexicalBinding::Unknown;
        }
        let pattern_position = self.pattern_positions.contains(&node.id())
            || (scoped && self.closure_patterns.contains(&node.id()));
        if !pattern_position && !is_rust_shadowable_value_identifier(node) {
            return LexicalBinding::Unknown;
        }
        let typed_let_pattern = pattern_position
            && node.parent().is_some_and(|statement| {
                statement.kind() == "let_declaration"
                    && statement.child_by_field_name("type").is_some()
                    && statement.child_by_field_name("pattern") == Some(node)
            });
        let Ok(name) = node.utf8_text(source) else {
            return LexicalBinding::Unknown;
        };
        let name = binding_name(name);
        let parent = node.parent();
        let direct_call =
            scoped && parent.is_some_and(|p| p.child_by_field_name("function") == Some(node));
        let closure_data = parent.is_some_and(|parent| match parent.kind() {
            "field_expression" => {
                parent.child_by_field_name("value") == Some(node)
                    && parent.parent().is_some_and(|call| {
                        call.kind() == "call_expression"
                            && call.child_by_field_name("function") == Some(parent)
                    })
            }
            "binary_expression" => true,
            _ => false,
        });
        let unbound = |scope: Node<'_>| {
            scoped
                && name.is_ascii()
                && self
                    .scoped_patterns
                    .get(&scope.id())
                    .is_some_and(|names| !names.contains(name))
        };
        let macro_argument = is_lone_expression_macro_argument(node, source);
        let mut requires_macro_validation = macro_argument;
        let mut proven = pattern_position;
        let mut child = node;
        while let Some(parent) = child.parent() {
            // match の値には arm の束縛が届かない。for の本体は同名の束縛が無い場合だけ通す。
            let opaque_scope = OPAQUE_SCOPES.contains(&parent.kind())
                && !(parent.kind() == "match_expression" && field_contains(parent, "value", node))
                && !(parent.kind() == "match_expression" && scoped)
                && !(parent.kind() == "match_arm"
                    && unbound(parent)
                    && field_contains(parent, "value", node))
                && !(parent.kind() == "let_condition"
                    && scoped
                    && field_contains(parent, "value", node))
                && !(parent.kind() == "for_expression"
                    && name.is_ascii()
                    && field_contains(parent, "body", node)
                    && self
                        .for_pattern_names
                        .get(&parent.id())
                        .is_some_and(|names| !names.contains(name)));
            // 内側のブロックで確定した let は、外側のパターン束縛に影響されない。
            if (!proven && opaque_scope)
                || ITEM_BOUNDARIES.contains(&parent.kind())
                || matches!(
                    parent.kind(),
                    "attribute_item"
                        | "type_arguments"
                        | "type_binding"
                        | "scoped_identifier"
                        | "scoped_type_identifier"
                )
                || field_contains(parent, "type", node)
                || field_contains(parent, "return_type", node)
                || (parent.kind() == "token_tree"
                    && !(macro_argument
                        && node
                            .parent()
                            .is_some_and(|direct| direct.id() == parent.id())))
            {
                return LexicalBinding::Unknown;
            }
            match parent.kind() {
                "if_expression" | "while_expression"
                    if !proven
                        && parent
                            .child_by_field_name("condition")
                            .is_some_and(|condition| {
                                matches!(condition.kind(), "let_condition" | "let_chain")
                                    && !unbound(condition)
                            }) =>
                {
                    return LexicalBinding::Unknown;
                }
                "let_declaration" if field_contains(parent, "alternative", node) => {
                    return LexicalBinding::Unknown;
                }
                "closure_expression" => {
                    if scoped && let Some(names) = self.scoped_patterns.get(&parent.id()) {
                        if names.contains(name) && field_contains(parent, "body", node) {
                            if !proven && !closure_data {
                                return LexicalBinding::Unknown;
                            }
                            proven = true;
                        } else if !proven && !field_contains(parent, "body", node) {
                            return LexicalBinding::Unknown;
                        }
                    } else if parent
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
                    // 型付き let の宣言位置は、別の文の巻き上げやマクロに左右されない。
                    let latest = facts.lets.get(name).and_then(|bindings| {
                        let end =
                            bindings.partition_point(|binding| binding.start <= node.start_byte());
                        end.checked_sub(1).map(|i| &bindings[i])
                    });
                    if scoped
                        && !proven
                        && !requires_macro_validation
                        && latest.is_none()
                        && facts.functions.contains(name)
                        && facts
                            .dynamic_statements
                            .first()
                            .is_none_or(|&pos| pos >= node.start_byte())
                    {
                        return LexicalBinding::RustValueBinding;
                    }
                    // 束縛と利用の間の文マクロ・属性は、隠れた再束縛を生成できる。
                    let after_binding = latest.is_some_and(|binding| {
                        let end = facts
                            .dynamic_statements
                            .partition_point(|&start| start < node.start_byte());
                        end == 0 || facts.dynamic_statements[end - 1] < binding.start
                    });
                    // 名前を明示した use が巻き上げるのは、その import が束縛する名前だけ。
                    let import_barrier = !facts.named_imports.is_empty()
                        && (!scoped || !name.is_ascii() || facts.named_imports.contains(name));
                    if (facts.barrier || import_barrier)
                        && !typed_let_pattern
                        && !(scoped && (proven || after_binding))
                    {
                        if facts.non_expression_macro_barrier || import_barrier {
                            return LexicalBinding::Unknown;
                        }
                        requires_macro_validation = true;
                    }
                    if let Some(binding) = latest
                        && !(scoped && proven)
                    {
                        if !binding.proven {
                            return LexicalBinding::Unknown;
                        }
                        proven = true;
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
                    // 新たにたどったスコープで let の値が関数として使われるときは、
                    // コールバックの実体を推定しない。関数の明示的な引数は本体全体で束縛する。
                    if direct_call && proven {
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
                    return if proven && requires_macro_validation {
                        LexicalBinding::RustMacroValueBinding
                    } else if proven {
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

pub(crate) fn is_value_expression_macro(name: &str) -> bool {
    matches!(
        name,
        "print"
            | "println"
            | "eprint"
            | "eprintln"
            | "format"
            | "format_args"
            | "write"
            | "writeln"
            | "panic"
            | "unreachable"
            | "todo"
            | "unimplemented"
            | "assert"
            | "assert_eq"
            | "assert_ne"
            | "debug_assert"
            | "debug_assert_eq"
            | "debug_assert_ne"
            | "dbg"
            | "vec"
    )
}

fn is_value_expression_macro_invocation(node: Node<'_>, source: &[u8]) -> bool {
    node.child_by_field_name("macro").is_some_and(|name| {
        name.kind() == "identifier" && name.utf8_text(source).is_ok_and(is_value_expression_macro)
    })
}

/// 全引数が単独の識別子かリテラルの場合だけ扱う。型引数・パターン・名前付き引数を
/// token tree のカンマだけで式と誤認しない。入れ子や呼び出しは従来の候補へ残す。
fn is_lone_expression_macro_argument(node: Node<'_>, source: &[u8]) -> bool {
    if node.kind() != "identifier" {
        return false;
    }
    let Some(tokens) = node.parent().filter(|parent| parent.kind() == "token_tree") else {
        return false;
    };
    let Some(invocation) = tokens
        .parent()
        .filter(|parent| parent.kind() == "macro_invocation")
    else {
        return false;
    };
    if !is_value_expression_macro_invocation(invocation, source) {
        return false;
    }
    let is_vec = invocation
        .child_by_field_name("macro")
        .and_then(|name| name.utf8_text(source).ok())
        == Some("vec");
    let mut expect_value = true;
    let mut found = false;
    let mut cursor = tokens.walk();
    for child in tokens.children(&mut cursor) {
        if matches!(child.kind(), "line_comment" | "block_comment")
            || child.start_byte() == tokens.start_byte()
            || child.end_byte() == tokens.end_byte()
        {
            continue;
        }
        if expect_value {
            if child.kind() != "identifier"
                && !matches!(
                    child.kind(),
                    "string_literal"
                        | "raw_string_literal"
                        | "integer_literal"
                        | "float_literal"
                        | "boolean_literal"
                        | "char_literal"
                )
            {
                return false;
            }
            found |= child.id() == node.id();
        } else if child.kind() != "," && !(is_vec && child.kind() == ";") {
            return false;
        }
        expect_value = !expect_value;
    }
    found
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
    fn rust_scope_resolves_match_values_without_entering_arm_bindings() {
        for source in [
            "fn f(mut comments: impl FnMut() -> u8) { match comments() { 0 => {}, _ => {} } }",
            "fn f() { let comments = || 1; match comments() { 0 => {}, _ => {} } }",
        ] {
            assert_eq!(
                resolve_last(source),
                LexicalBinding::RustValueBinding,
                "{source}"
            );
        }
        for source in [
            "fn f() { match comments() { 0 => {}, _ => {} } }",
            "fn f(comments: fn()) { match value { Some(comments) => comments(), _ => {} } }",
            "fn f(comments: fn()) { match value { Some(other) => comments(), _ => {} } }",
        ] {
            assert_eq!(resolve_last(source), LexicalBinding::Unknown, "{source}");
        }
    }

    #[test]
    fn rust_scope_proves_inner_let_inside_pattern_control_flow() {
        for source in [
            "fn f() { while let Some(offset) = next() { let comments = offset + 1; comments; } }",
            "fn f() { if let Some(offset) = next() { let comments = offset + 1; comments; } }",
            "fn f() { for offset in values { let comments = offset + 1; comments; } }",
            "fn f() { match value { Some(offset) => { let comments = offset + 1; comments; }, _ => {} } }",
        ] {
            assert_eq!(
                resolve_last(source),
                LexicalBinding::RustValueBinding,
                "{source}"
            );
        }

        let source = "fn f(values: &[u32]) -> Option<usize> { let mut cursor = 0; while let Some(offset) = values[cursor..].iter().position(|v| *v == 0) { let comments = cursor + offset; if comments > 2 { return Some(comments); } cursor = comments + 1; } None }";
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        assert!(!tree.root_node().has_error());
        let index = RustScopeIndex::build(tree.root_node(), source.as_bytes()).unwrap();
        let positions: Vec<_> = source.match_indices("comments").collect();
        assert_eq!(positions.len(), 4);
        for (start, _) in positions {
            let node = tree
                .root_node()
                .descendant_for_byte_range(start, start + "comments".len())
                .unwrap();
            assert_eq!(
                index.resolve(node, source.as_bytes()),
                LexicalBinding::RustValueBinding,
                "position {start}: {source}"
            );
        }
    }

    #[test]
    fn rust_scope_proves_outer_binding_in_unrelated_for_pattern_body() {
        for source in [
            "fn f() { let comments = 1; for value in values { comments(); } }",
            "fn f() { let comments = 1; for (index, value) in values { consume(comments); } }",
            "fn f(comments: String) { for value in values { consume(&comments); } }",
            "fn f() { let comments = 1; for value in values { for next in rest { consume(comments); } } }",
            "fn f() { let comments = 1; for Some(value) in values { consume(comments); } }",
        ] {
            assert_eq!(
                resolve_last(source),
                LexicalBinding::RustValueBinding,
                "{source}"
            );
        }
        for source in [
            "fn f() { let comments = 1; for comments in values { comments(); } }",
            "fn f() { let comments = 1; for (value, comments) in values { comments(); } }",
            "fn f() { let comments = 1; for (value, r#comments) in values { comments(); } }",
            "fn f() { let comments = 1; for bind!() in values { comments(); } }",
            "fn f() { let comments = 1; for K in values { comments(); } }",
            "fn f() { let comments = 1; for value in comments() { consume(value); } }",
            "fn f() { let comments = 1; for value in values { let capture = |arg| comments(); } }",
            "fn f() { let comments = 1; for value in values { unknown!(); comments(); } }",
            "fn f() { let comments = 1; for value in values { use crate::api::comments; comments(); } }",
        ] {
            assert_eq!(resolve_last(source), LexicalBinding::Unknown, "{source}");
        }
    }

    #[test]
    fn rust_scope_inner_let_keeps_opaque_bindings_and_barriers_unknown() {
        for source in [
            "fn f() { while let Some(comments) = next() { comments(); } }",
            "fn f() { if let Some(comments) = next() { comments(); } }",
            "fn f() { for comments in values { comments(); } }",
            "fn f() { match value { Some(comments) => comments(), _ => () } }",
            "fn f() { while let Some(offset) = next() { let comments = comments(); } }",
            "fn f() { while let Some(offset) = next() { let comments = 1; let comments = crate::api::comments; comments(); } }",
            "fn f() { while let Some(offset) = next() { let mut comments = || 1; comments = crate::api::comments; comments(); } }",
            "fn f() { while let Some(offset) = next() { let comments = 1; create_binding!(); comments(); } }",
            "fn f() { while let Some(offset) = next() { #[cfg(feature=\"x\")] let comments = 1; comments(); } }",
            "fn f() { while let Some(offset) = next() { let comments = 1; fn inner() { comments(); } } }",
            "fn comments(n: u8) {} #[cfg(test)] mod tests { use super::*; #[test] fn invokes() { comments(1); } }",
        ] {
            assert_eq!(resolve_last(source), LexicalBinding::Unknown, "{source}");
        }
        assert_eq!(
            resolve_last(
                "fn f() { while let Some(offset) = next() { let comments = offset; dbg!(comments); } }"
            ),
            LexicalBinding::RustMacroValueBinding
        );
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
            "fn f() { let comments = 1; crate::api::comments(); }",
            "fn f() { let comments = 1; self::comments(); }",
            "fn f() { let comments = 1; object.comments(); }",
            "fn f() { let comments = 1; let value: comments = input; }",
            "fn f() { let comments = 1; let value: [u8; comments] = input; }",
            "fn f(comments: u8) -> [u8; comments] { value }",
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
    fn rust_scope_macro_arguments_require_project_validation_and_simple_values() {
        for source in [
            "fn f() { let comments = 1; println!(\"{}\", comments); }",
            "fn f(comments: u8) { dbg!(comments); }",
            "fn f() { let comments = 1; let s = format!(\"{}\", comments); }",
            "fn f() { let comments = 1; vec![comments; 3]; }",
        ] {
            assert_eq!(
                resolve_last(source),
                LexicalBinding::RustMacroValueBinding,
                "{source}"
            );
        }
        for source in [
            "fn f() { let comments = 1; println!(\"{}\", (comments)); }",
            "fn f() { let comments = 1; println!(\"{}\", comments()); }",
            "fn f() { let comments = 1; println!(\"{}\", object.comments); }",
            "fn f() { let comments = 1; println!(\"{}\", crate::api::comments); }",
            "fn f() { let comments = 1; println!(\"{}\", f::<A, comments, B>()); }",
            "fn f() { let comments = 1; custom!(comments); }",
            "fn f() { let comments = 1; std::println!(\"{}\", comments); }",
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
                "string_literal",
                "raw_string_literal",
                "integer_literal",
                "float_literal",
                "boolean_literal",
                "char_literal",
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
