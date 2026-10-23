//! API 判定専用の Rust 自由関数と関連メンバーの証拠。

use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

use crate::{engine::parser, language::LangId, models::symbol::SymbolKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MemberSite {
    AssociatedDefinition,
    DirectMethodCall,
    UnprovenFieldValue,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ModuleFunctionProof(());

pub(super) struct ModuleFunctions {
    signatures: HashMap<String, Option<String>>,
    /// `extern` の ABI (`extern "C" fn` など) を持つ関数の名前。
    foreign_abi: HashSet<String>,
}

impl ModuleFunctions {
    pub(super) fn parse(source: &[u8]) -> Option<Self> {
        let tree = parser::parse_source(source, LangId::Rust).ok()?;
        Self::from_root(tree.root_node(), source)
    }

    pub(super) fn from_root(root: Node<'_>, source: &[u8]) -> Option<Self> {
        if !clean_tree(root) {
            return None;
        }
        let symbols = crate::engine::symbols::extract_symbols(root, source, LangId::Rust).ok()?;
        let lines: Vec<_> = std::str::from_utf8(source).ok()?.lines().collect();
        let mut signatures = HashMap::new();
        let mut foreign_abi = HashSet::new();
        for symbol in symbols {
            if matches!(
                symbol.kind,
                SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Constant | SymbolKind::Variable
            ) && let Some(node) = root.descendant_for_point_range(
                tree_sitter::Point::new(symbol.range.start.line, symbol.range.start.column),
                tree_sitter::Point::new(symbol.range.end.line, symbol.range.end.column),
            ) && module_child(node)
            {
                signatures.insert(symbol.name.clone(), None);
            }
            if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
                continue;
            }
            let name = super::bare_name(&symbol.name).to_owned();
            let node = root.descendant_for_point_range(
                tree_sitter::Point::new(symbol.range.start.line, symbol.range.start.column),
                tree_sitter::Point::new(symbol.range.end.line, symbol.range.end.column),
            )?;
            if node.child_by_field_name("name").is_some_and(|name_node| {
                classify_member_site(name_node, source) == Some(MemberSite::AssociatedDefinition)
            }) {
                continue;
            }
            let signature = (node.kind() == "function_item"
                && module_child(node)
                && !opaque_ancestor(node))
            .then(|| {
                super::signature::extract_api_signature(&symbol, root, source, &lines, LangId::Rust)
            });
            if has_extern_modifier(node) {
                foreign_abi.insert(name.clone());
            }
            signatures
                .entry(name)
                .and_modify(|entry| *entry = None)
                .or_insert(signature);
        }
        Some(Self {
            signatures,
            foreign_abi,
        })
    }

    pub(super) fn prove(&self, name: &str, signature: &str) -> Option<ModuleFunctionProof> {
        (self.signatures.get(name)?.as_deref()? == signature).then_some(ModuleFunctionProof(()))
    }

    /// `prove` に加えて `extern` の ABI を持たない関数か。C などの ABI で公開した関数は、Cargo の
    /// 依存の外 (リンクやほかの言語) から同じ名前で使われうる。
    pub(super) fn proves_package_local(&self, name: &str, signature: &str) -> bool {
        self.prove(name, signature).is_some() && !self.foreign_abi.contains(name)
    }
}

fn has_extern_modifier(function: Node<'_>) -> bool {
    let mut cursor = function.walk();
    function
        .children(&mut cursor)
        .filter(|child| child.kind() == "function_modifiers")
        .any(|modifiers| {
            let mut cursor = modifiers.walk();
            modifiers
                .children(&mut cursor)
                .any(|modifier| modifier.kind() == "extern_modifier")
        })
}

fn module_child(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "source_file"
            || (parent.kind() == "declaration_list"
                && parent
                    .parent()
                    .is_some_and(|owner| owner.kind() == "mod_item"))
    })
}

pub(super) fn clean_tree(root: Node<'_>) -> bool {
    if root.has_error() {
        return false;
    }
    let mut clean = true;
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        root,
        crate::engine::tree_walk::Children::All,
        |node, _| {
            if node.is_missing() {
                clean = false;
                return std::ops::ControlFlow::Break(());
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    clean
}

fn opaque_ancestor(mut node: Node<'_>) -> bool {
    loop {
        if matches!(node.kind(), "attribute_item" | "token_tree") {
            return true;
        }
        let inner_scope = match node.kind() {
            "source_file" | "declaration_list" | "block" => Some(node),
            "function_item" => node.child_by_field_name("body"),
            _ => None,
        };
        if let Some(scope) = inner_scope {
            let mut cursor = scope.walk();
            if scope
                .named_children(&mut cursor)
                .any(|child| child.kind() == "inner_attribute_item")
            {
                return true;
            }
        }
        let mut previous = node.prev_named_sibling();
        while let Some(sibling) = previous {
            match sibling.kind() {
                "attribute_item" => return true,
                "line_comment" | "block_comment" => previous = sibling.prev_named_sibling(),
                _ => break,
            }
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}

pub(super) fn classify_member_site(node: Node<'_>, _source: &[u8]) -> Option<MemberSite> {
    let parent = node.parent()?;
    if matches!(parent.kind(), "function_item" | "function_signature_item")
        && parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
        && parent.parent().is_some_and(|body| {
            body.kind() == "declaration_list"
                && body
                    .parent()
                    .is_some_and(|owner| matches!(owner.kind(), "impl_item" | "trait_item"))
        })
    {
        return (!opaque_ancestor(node)).then_some(MemberSite::AssociatedDefinition);
    }
    if parent.kind() != "field_expression"
        || !parent
            .child_by_field_name("field")
            .is_some_and(|field| field.id() == node.id())
    {
        return None;
    }
    if !opaque_ancestor(node)
        && parent.parent().is_some_and(|call| {
            call.kind() == "call_expression"
                && call
                    .child_by_field_name("function")
                    .is_some_and(|callee| callee.id() == parent.id())
        })
    {
        return Some(MemberSite::DirectMethodCall);
    }
    // 末尾式・入れ物・未知の式も関数値を運び得る。用途から型を推定しない。
    Some(MemberSite::UnprovenFieldValue)
}

/// 同一モジュールの直接 struct と引数型だけで証明できる primitive field。
/// import・型別名・generic・マクロ・属性や本体内の束縛がある場合は使わない。
/// ファイルと関数は一度ずつ調べ、各 field の検査で全ソースを走査し直さない。
#[derive(Default)]
struct PrimitiveFields {
    structs: HashMap<(usize, String), bool>,
    parameters: HashMap<usize, HashMap<String, String>>,
}

impl PrimitiveFields {
    fn collect(root: Node<'_>, source: &[u8], field_name: &str) -> Self {
        let mut facts = Self::default();
        let mut functions = Vec::new();
        let mut opaque = false;
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            root,
            crate::engine::tree_walk::Children::Named,
            |node, _| {
                if matches!(
                    node.kind(),
                    "use_declaration"
                        | "extern_crate_declaration"
                        | "macro_invocation"
                        | "macro_definition"
                        | "attribute_item"
                        | "inner_attribute_item"
                        | "type_item"
                        | "type_parameters"
                ) || node.child_by_field_name("name").is_some_and(|name| {
                    name.utf8_text(source).is_ok_and(|name| {
                        is_primitive_name(name.strip_prefix("r#").unwrap_or(name))
                    })
                }) {
                    opaque = true;
                    return std::ops::ControlFlow::Break(());
                }
                // 深い式の各ノードで parent() を呼ぶと二次走査になる。
                // 宣言名のないノードは、親を調べる前に除く。
                if let Some(name) = node.child_by_field_name("name")
                    && module_child(node)
                    && let Ok(name) = name.utf8_text(source)
                {
                    let primitive = node.kind() == "struct_item"
                        && node.child_by_field_name("body").is_some_and(|body| {
                            let mut cursor = body.walk();
                            let fields: Vec<_> = body
                                .named_children(&mut cursor)
                                .filter(|field| {
                                    field.child_by_field_name("name").is_some_and(|name| {
                                        name.utf8_text(source).ok() == Some(field_name)
                                    })
                                })
                                .collect();
                            fields.len() == 1
                                && fields[0].child_by_field_name("type").is_some_and(|ty| {
                                    ty.kind() == "primitive_type"
                                        && ty.utf8_text(source).is_ok_and(is_primitive_name)
                                })
                        });
                    facts
                        .structs
                        .entry((node.parent().unwrap().id(), name.to_owned()))
                        .and_modify(|entry| *entry = false)
                        .or_insert(primitive);
                    if node.kind() == "function_item" {
                        functions.push(node);
                    }
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        if opaque {
            return Self::default();
        }
        for function in functions {
            let Some(body) = function.child_by_field_name("body") else {
                continue;
            };
            let mut shadowable = false;
            let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
                body,
                crate::engine::tree_walk::Children::Named,
                |node, _| {
                    if node.kind().ends_with("_item")
                        || matches!(
                            node.kind(),
                            "let_declaration"
                                | "let_condition"
                                | "for_expression"
                                | "match_arm"
                                | "closure_expression"
                        )
                    {
                        shadowable = true;
                        return std::ops::ControlFlow::Break(());
                    }
                    std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
                },
            );
            if shadowable {
                continue;
            }
            let Some(parameters) = function.child_by_field_name("parameters") else {
                continue;
            };
            let mut bindings = HashMap::new();
            let mut unsupported_pattern = false;
            let mut cursor = parameters.walk();
            for parameter in parameters.named_children(&mut cursor) {
                let Some(pattern) = parameter.child_by_field_name("pattern") else {
                    continue;
                };
                if pattern.kind() != "identifier" {
                    unsupported_pattern = true;
                    break;
                }
                let Some(mut ty) = parameter.child_by_field_name("type") else {
                    continue;
                };
                while ty.kind() == "reference_type" {
                    let Some(inner) = ty.child_by_field_name("type") else {
                        break;
                    };
                    ty = inner;
                }
                if ty.kind() == "type_identifier"
                    && let (Ok(name), Ok(ty)) = (pattern.utf8_text(source), ty.utf8_text(source))
                {
                    bindings
                        .entry(name.to_owned())
                        .and_modify(|ty: &mut String| ty.clear())
                        .or_insert_with(|| ty.to_owned());
                }
            }
            if !unsupported_pattern {
                facts.parameters.insert(function.id(), bindings);
            }
        }
        facts
    }

    fn proves(&self, field: Node<'_>, source: &[u8]) -> bool {
        let Some(expression) = field.parent() else {
            return false;
        };
        let Some(receiver) = expression.child_by_field_name("value") else {
            return false;
        };
        if receiver.kind() != "identifier" {
            return false;
        }
        let Ok(receiver) = receiver.utf8_text(source) else {
            return false;
        };
        let mut ancestor = expression.parent();
        while let Some(node) = ancestor {
            if node.kind() == "function_item" {
                return self
                    .parameters
                    .get(&node.id())
                    .and_then(|bindings| bindings.get(receiver))
                    .is_some_and(|ty| {
                        node.parent().is_some_and(|module| {
                            self.structs.get(&(module.id(), ty.clone())) == Some(&true)
                        })
                    });
            }
            ancestor = node.parent();
        }
        false
    }
}

fn is_primitive_name(name: &str) -> bool {
    matches!(
        name,
        "bool"
            | "char"
            | "str"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f32"
            | "f64"
    )
}

pub(super) fn member_positions(
    root: Node<'_>,
    source: &[u8],
    name: &str,
) -> HashMap<(usize, usize), MemberSite> {
    let mut positions = HashMap::new();
    if !clean_tree(root) {
        return positions;
    }
    let primitive_fields = PrimitiveFields::collect(root, source, name);
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        root,
        crate::engine::tree_walk::Children::Named,
        |node, _| {
            if matches!(node.kind(), "identifier" | "field_identifier")
                && node.utf8_text(source).ok() == Some(name)
                && let Some(site) = classify_member_site(node, source)
                && !(site == MemberSite::UnprovenFieldValue
                    && primitive_fields.proves(node, source))
            {
                let point = node.start_position();
                positions.insert((point.row, point.column), site);
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    positions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_reference_sites_classification_requires_exact_ast_roles() {
        for (source, expected) in [
            (
                "impl C { fn put(&self) {} }",
                Some(MemberSite::AssociatedDefinition),
            ),
            (
                "trait C { fn put(&self); }",
                Some(MemberSite::AssociatedDefinition),
            ),
            (
                "trait C { fn put(&self) {} }",
                Some(MemberSite::AssociatedDefinition),
            ),
            (
                "fn f(c: &C) { c.put(3); }",
                Some(MemberSite::DirectMethodCall),
            ),
            (
                "fn f(c: &C) { (c.put)(3); }",
                Some(MemberSite::UnprovenFieldValue),
            ),
            (
                "fn f(c: &C) { let f = c.put; }",
                Some(MemberSite::UnprovenFieldValue),
            ),
            (
                "fn f(c: &C) { consume(c.put); }",
                Some(MemberSite::UnprovenFieldValue),
            ),
            (
                "fn f(c: &C) { c.put }",
                Some(MemberSite::UnprovenFieldValue),
            ),
            (
                "fn f(c: &C) { (c.put,) }",
                Some(MemberSite::UnprovenFieldValue),
            ),
            (
                "fn f(c: &C) { [c.put] }",
                Some(MemberSite::UnprovenFieldValue),
            ),
            (
                "#[rewrite] fn f(c: &C) { c.put }",
                Some(MemberSite::UnprovenFieldValue),
            ),
            (
                "mod m { #![rewrite] fn f(c: &C) { c.put(3); } }",
                Some(MemberSite::UnprovenFieldValue),
            ),
            (
                "fn f(c: &C) { #![rewrite] c.put(3); }",
                Some(MemberSite::UnprovenFieldValue),
            ),
            ("mod m { #![rewrite] impl C { fn put(&self) {} } }", None),
            ("impl C { fn put(&self) { #![rewrite] } }", None),
            ("fn put(n: u32) {}", None),
            ("impl C { fn f() { fn put(n: u32) {} } }", None),
            ("fn f() { crate::api::put(3); }", None),
            ("fn f() { C::put(3); }", None),
            (
                "fn f(c: &C) { c.put::<u32>(3); }",
                Some(MemberSite::UnprovenFieldValue),
            ),
            ("fn f(c: &C) { m!(c.put(3)); }", None),
            ("#[rewrite] impl C { fn put(&self) {} }", None),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            let start = source.rfind("put").unwrap();
            let node = tree
                .root_node()
                .descendant_for_byte_range(start, start + 3)
                .unwrap();
            assert_eq!(
                classify_member_site(node, source.as_bytes()),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn rust_reference_sites_primitive_field_proof_requires_the_receiver_type() {
        for source in [
            "struct C { put: u32 } fn f(c: C) -> u32 { c.put }",
            "struct C { put: u32 } fn f(c: &C) -> u32 { c.put }",
            "struct C { put: u32 } fn f(c: &mut C) -> (u32,) { (c.put,) }",
            "mod m { struct C { put: u32 } fn f(c: C) -> [u32; 1] { [c.put] } }",
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let start = source.rfind("put").unwrap();
            let position = tree
                .root_node()
                .descendant_for_byte_range(start, start + 3)
                .unwrap()
                .start_position();
            assert!(
                !member_positions(tree.root_node(), source.as_bytes(), "put")
                    .contains_key(&(position.row, position.column)),
                "{source}"
            );
        }
        for source in [
            "struct C { put: fn() } fn f(c: C) -> fn() { c.put }",
            "struct C { put: u32 } struct E { put: fn() } fn f(c: E) -> fn() { c.put }",
            "struct C { put: u32 } struct E { put: fn() } fn f(c: C) -> fn() { let c = E { put: fallback }; c.put }",
            "struct C { put: u32 } fn f(c: C) { let get = |c| c.put; }",
            "struct C { put: u32 } fn f(c: C) { if let Some(c) = other { c.put } }",
            "struct C { put: u32 } fn f<C>(c: C) { c.put }",
            "type u32 = fn(); struct C { put: u32 } fn f(c: C) { c.put }",
            "struct r#u32 { call: fn() } struct C { put: u32 } fn f(c: C) { c.put }",
            "use other::u32; struct C { put: u32 } fn f(c: C) { c.put }",
            "struct C { put: u32 } #[rewrite] fn f(c: C) { c.put }",
            "#[rewrite] struct C { put: u32 } fn f(c: C) { c.put }",
            "struct C { put: u32 } fn f(c: C) { m!(); c.put }",
            "struct C { put: u32 } fn f(c: C) { fn g(c: E) { c.put } }",
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            let start = source.rfind("put").unwrap();
            let position = tree
                .root_node()
                .descendant_for_byte_range(start, start + 3)
                .unwrap()
                .start_position();
            assert_eq!(
                member_positions(tree.root_node(), source.as_bytes(), "put")
                    .get(&(position.row, position.column)),
                Some(&MemberSite::UnprovenFieldValue),
                "{source}"
            );
        }
    }

    #[test]
    fn rust_reference_sites_module_proof_checks_uniqueness_signature_and_old_shape() {
        for source in [
            "pub fn put(n: u32) -> u32 { n }",
            "pub fn put(n: u32) -> u32 { n } impl C { fn put(&self) {} }",
        ] {
            let facts = ModuleFunctions::parse(source.as_bytes()).unwrap();
            assert!(
                facts.prove("put", "pub fn put(n: u32) -> u32").is_some(),
                "{source}"
            );
            assert!(facts.prove("put", "pub fn put(n: u64) -> u64").is_none());
        }
        for source in [
            "impl C { pub fn put(n: u32) -> u32 { n } }",
            "pub fn put(n: u32) -> u32 { n } fn other() { fn put() {} }",
            "pub fn put(n: u32) -> u32 { n } pub fn put(n: u64) -> u64 { n }",
            "pub fn put(n: u32) -> u32 { n } pub const put: u8 = 1;",
            "#[rewrite] pub fn put(n: u32) -> u32 { n }",
            "#![rewrite] pub fn put(n: u32) -> u32 { n }",
            "pub fn put(n: u32) -> u32 { #![rewrite] n }",
        ] {
            let facts = ModuleFunctions::parse(source.as_bytes()).unwrap();
            assert!(
                facts.prove("put", "pub fn put(n: u32) -> u32").is_none(),
                "{source}"
            );
        }
        let malformed = b"pub fn put(n: u32) -> u32 { n";
        assert!(ModuleFunctions::parse(malformed).is_none());
        let tree = parser::parse_source(malformed, LangId::Rust).unwrap();
        assert!(member_positions(tree.root_node(), malformed, "put").is_empty());
    }
}
