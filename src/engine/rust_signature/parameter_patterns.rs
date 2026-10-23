//! Rust の通常引数パターンだけを消した、呼び出し契約の比較用トークン。
//!
//! 表示用 signature は変更しない。引数名を外部契約に使い得る属性・マクロや、
//! 解析できない構文があれば同値と証明しない。

use super::{
    Children, ControlFlow, HashSet, Node, Walk, is_rust_binding_mut, is_rust_list_trailing_comma,
    walk_preorder,
};

/// 通常引数の型・位置は保持し、関数本体でだけ使うパターンを同じ印に置き換える。
/// receiver、ジェネリクス、ABI、可視性、外側の宣言と属性のトークンは保持する。
pub(crate) fn rust_parameter_pattern_contract(
    function: Node<'_>,
    source: &[u8],
) -> Option<Vec<String>> {
    if !matches!(function.kind(), "function_item" | "function_signature_item") {
        return None;
    }
    let parameters = function.child_by_field_name("parameters")?;
    let mut patterns = HashSet::new();
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.is_extra() || parameter.kind() == "self_parameter" {
            continue;
        }
        if parameter.kind() != "parameter" {
            return None;
        }
        let pattern = parameter.child_by_field_name("pattern")?;
        parameter.child_by_field_name("type")?;
        let mut pc = parameter.walk();
        if !parameter.children(&mut pc).any(|child| child.kind() == ":") {
            // 2015 edition の匿名 trait 引数は名前と型を区別できない。
            return None;
        }
        if pattern.kind() == "self" {
            continue;
        }
        if !matches!(
            pattern.kind(),
            "identifier"
                | "_"
                | "tuple_pattern"
                | "struct_pattern"
                | "tuple_struct_pattern"
                | "reference_pattern"
                | "slice_pattern"
                | "captured_pattern"
                | "mut_pattern"
                | "ref_pattern"
        ) {
            return None;
        }
        let unsafe_pattern = walk_preorder(pattern, Children::Named, |node, _| {
            if matches!(
                node.kind(),
                "macro_invocation"
                    | "or_pattern"
                    | "range_pattern"
                    | "integer_literal"
                    | "float_literal"
                    | "char_literal"
                    | "string_literal"
                    | "boolean_literal"
            ) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(Walk::Descend)
            }
        })
        .is_break();
        if unsafe_pattern {
            return None;
        }
        patterns.insert(pattern.id());
    }
    // 引数が無い宣言も、generic list の末尾commaなどヘッダの整形比較に使う。
    let mut tokens = Vec::new();
    append_header(function, source, &patterns, None, &mut tokens)?;
    let mut current = function;
    // Node::parent の深さ依存コストを制限し、深い未対応の配置では証明を保留する。
    for _ in 0..64 {
        append_outer_attributes(current, source, &mut tokens)?;
        if let Some(body) = current.child_by_field_name("body") {
            append_inner_attributes(body, source, &mut tokens)?;
        }
        let parent = current.parent()?;
        match parent.kind() {
            "source_file" => {
                if parent.has_error() {
                    return None;
                }
                append_inner_attributes(parent, source, &mut tokens)?;
                return Some(tokens);
            }
            "declaration_list" => current = parent,
            "impl_item" | "trait_item" | "mod_item" | "foreign_mod_item" => {
                tokens.push("<enclosing-declaration>".to_string());
                append_header(parent, source, &HashSet::new(), None, &mut tokens)?;
                current = parent;
            }
            _ => return None,
        }
    }
    None
}

/// 同じ型名の import を同時に差し替えた変更には、パターンだけの同値証拠を使わない。
/// 関数本体内の use は呼び出し契約の型束縛に関与しないため対象外。
pub(crate) fn rust_parameter_import_environment(
    root: Node<'_>,
    source: &[u8],
) -> Option<Vec<String>> {
    if root.has_error() {
        return None;
    }
    let mut tokens = Vec::new();
    let result = walk_preorder(root, Children::Named, |node, depth| {
        if matches!(
            node.kind(),
            "function_item" | "function_signature_item" | "block"
        ) {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.kind() == "mod_item" {
            tokens.push(format!("<module-depth:{depth}>"));
            if append_header(node, source, &HashSet::new(), None, &mut tokens).is_none()
                || append_outer_attributes(node, source, &mut tokens).is_none()
            {
                return ControlFlow::Break(());
            }
        }
        if matches!(node.kind(), "use_declaration" | "extern_crate_declaration") {
            tokens.push(format!("<import-depth:{depth}>"));
            if append_tokens(node, source, &mut tokens).is_none()
                || append_outer_attributes(node, source, &mut tokens).is_none()
            {
                return ControlFlow::Break(());
            }
            tokens.push("<import-end>".to_string());
            return ControlFlow::Continue(Walk::Skip);
        }
        ControlFlow::Continue(Walk::Descend)
    });
    result.is_continue().then_some(tokens)
}

/// 本体を除く宣言のトークンを足す。`patterns` の部分木は同じ印に置き換え、`omitted` の
/// トークン (関数自身の `const` など) は比べない。
pub(super) fn append_header(
    declaration: Node<'_>,
    source: &[u8],
    patterns: &HashSet<usize>,
    omitted: Option<usize>,
    tokens: &mut Vec<String>,
) -> Option<()> {
    let body = declaration
        .child_by_field_name("body")
        .map(|node| node.id());
    let result = walk_preorder(declaration, Children::All, |node, _| {
        if node.is_error()
            || node.is_missing()
            || matches!(
                node.kind(),
                "attribute_item" | "inner_attribute_item" | "macro_invocation"
            )
        {
            return ControlFlow::Break(());
        }
        if node.is_extra()
            || Some(node.id()) == body
            || Some(node.id()) == omitted
            || is_rust_binding_mut(node)
            || is_rust_list_trailing_comma(node)
        {
            return ControlFlow::Continue(Walk::Skip);
        }
        if patterns.contains(&node.id()) {
            tokens.push("<parameter-pattern>".to_string());
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.child_count() == 0 {
            let Ok(text) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            tokens.push(text.to_owned());
        }
        ControlFlow::Continue(Walk::Descend)
    });
    result.is_continue().then_some(())
}

pub(super) fn append_outer_attributes(
    declaration: Node<'_>,
    source: &[u8],
    tokens: &mut Vec<String>,
) -> Option<()> {
    let mut previous = declaration.prev_named_sibling();
    while let Some(node) = previous {
        if node.is_extra() {
            previous = node.prev_named_sibling();
            continue;
        }
        if node.kind() != "attribute_item" {
            break;
        }
        append_safe_attribute(node, source, tokens)?;
        previous = node.prev_named_sibling();
    }
    Some(())
}

pub(super) fn append_inner_attributes(
    container: Node<'_>,
    source: &[u8],
    tokens: &mut Vec<String>,
) -> Option<()> {
    let mut cursor = container.walk();
    for node in container.named_children(&mut cursor) {
        if node.is_extra() {
            continue;
        }
        if node.kind() != "inner_attribute_item" {
            break;
        }
        append_safe_attribute(node, source, tokens)?;
    }
    Some(())
}

fn append_safe_attribute(item: Node<'_>, source: &[u8], tokens: &mut Vec<String>) -> Option<()> {
    let attribute = item.named_child(0)?;
    let name = attribute.named_child(0)?;
    if name.kind() != "identifier"
        || !matches!(
            name.utf8_text(source).ok()?,
            "doc"
                | "inline"
                | "cold"
                | "must_use"
                | "track_caller"
                | "deprecated"
                | "allow"
                | "warn"
                | "deny"
                | "forbid"
                | "expect"
                | "cfg"
                | "no_mangle"
                | "export_name"
                | "link_name"
                | "link"
                | "target_feature"
        )
    {
        // cfg_attr と unsafe(...) も中の属性を証明しない限り許さない。
        return None;
    }
    append_tokens(item, source, tokens)
}

pub(super) fn append_tokens(node: Node<'_>, source: &[u8], tokens: &mut Vec<String>) -> Option<()> {
    let result = walk_preorder(node, Children::All, |node, _| {
        if node.is_error() || node.is_missing() {
            return ControlFlow::Break(());
        }
        if node.is_extra() {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.child_count() == 0 {
            let Ok(text) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            tokens.push(text.to_owned());
        }
        ControlFlow::Continue(Walk::Descend)
    });
    result.is_continue().then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::parser, language::LangId};

    fn contract(source: &str) -> Option<Vec<String>> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let function = match walk_preorder(tree.root_node(), Children::Named, |node, _| {
            if matches!(node.kind(), "function_item" | "function_signature_item") {
                ControlFlow::Break(node)
            } else {
                ControlFlow::Continue(Walk::Descend)
            }
        }) {
            ControlFlow::Break(node) => node,
            _ => panic!("missing function: {source}"),
        };
        rust_parameter_pattern_contract(function, source.as_bytes())
    }

    #[test]
    fn patterns_change_without_changing_the_call_contract() {
        for (old, new) in [
            ("pub fn f(_n: i32) {}", "pub fn f(n: i32) { use_it(n); }"),
            ("pub fn f((a,b): (u8,u8)) {}", "pub fn f(pair: (u8,u8)) {}"),
            ("fn f(ref n: i32) {}", "fn f(mut n: i32) {}"),
            ("fn f(&n: &u8) {}", "fn f(n: &u8) {}"),
            (
                "trait T { fn f(&self, _n: u8); }",
                "trait T { fn f(&self, n: u8); }",
            ),
            (
                "impl T { pub fn f(self: Box<Self>, _n: u8) {} }",
                "impl T { pub fn f(self: Box<Self>, n: u8) {} }",
            ),
            ("#[inline] fn f(_n: u8) {}", "#[inline] fn f(n: u8) {}"),
        ] {
            let a = contract(old).expect(old);
            let b = contract(new).expect(new);
            assert_eq!(a, b, "{old} -> {new}");
        }
    }

    #[test]
    fn parameter_contract_preserves_types_receivers_and_qualifiers() {
        for (old, new) in [
            ("fn f(_n: u8) {}", "fn f(n: u16) {}"),
            ("fn f(_n: &mut u8) {}", "fn f(n: &u8) {}"),
            ("fn f(_a: u8, b: u16) {}", "fn f(a: u16, b: u8) {}"),
            ("fn f(_n: u8) {}", "fn f(n: u8, m: u8) {}"),
            ("fn f(_n: u8) -> u8 { 0 }", "fn f(n: u8) -> u16 { 0 }"),
            (
                "impl T { fn f(&self, _n: u8) {} }",
                "impl T { fn f(&mut self, n: u8) {} }",
            ),
            (
                "impl T { fn f(self: Box<Self>, _n: u8) {} }",
                "impl T { fn f(self: &Self, n: u8) {} }",
            ),
            ("fn f<T: Copy>(_n: T) {}", "fn f<T: Clone>(n: T) {}"),
            (
                "fn f<'a>(_n: &'a u8) -> &'a u8 { todo!() }",
                "fn f<'a>(n: &u8) -> &'a u8 { todo!() }",
            ),
            (
                "extern \"C\" fn f(_n: u8) {}",
                "extern \"C-unwind\" fn f(n: u8) {}",
            ),
            (
                "extern \"C\" { fn f(_n: u8); }",
                "extern \"system\" { fn f(n: u8); }",
            ),
            ("fn f(_n: u8) {}", "unsafe fn f(n: u8) {}"),
            ("fn f(_n: u8) {}", "async fn f(n: u8) {}"),
            ("fn f(_n: u8) {}", "const fn f(n: u8) {}"),
            ("pub fn f(_n: u8) {}", "pub(crate) fn f(n: u8) {}"),
            (
                "impl<T: Copy> S<T> { fn f(_n: u8) {} }",
                "impl<T: Clone> S<T> { fn f(n: u8) {} }",
            ),
            (
                "#[export_name = \"a  b\"] fn f(_n: u8) {}",
                "#[export_name = \"a b\"] fn f(n: u8) {}",
            ),
        ] {
            assert_ne!(
                contract(old).expect(old),
                contract(new).expect(new),
                "{old} -> {new}"
            );
        }
    }

    #[test]
    fn macro_attributes_and_unproven_parameters_have_no_contract() {
        for source in [
            "#[pyfunction] fn f(n: u8) {}",
            "#[tauri::command] fn f(n: u8) {}",
            "#[cfg_attr(feature = \"python\", pyfunction)] fn f(n: u8) {}",
            "#[unsafe(no_mangle)] fn f(n: u8) {}",
            "#[pymethods] impl S { fn f(n: u8) {} }",
            "#[bridge] mod ffi { fn f(n: u8) {} }",
            "#![custom] fn f(n: u8) {}",
            "fn f(n: u8) { #![custom] }",
            "fn f(#[custom] n: u8) {}",
            "trait T { fn f(u8); }",
            "fn f(bind!(): u8) {}",
            "fn f(n: u8) { let = ; }",
        ] {
            assert!(contract(source).is_none(), "{source}");
        }
    }

    #[test]
    fn grammar_supports_the_parameter_and_scope_nodes_used_by_the_proof() {
        let language = LangId::Rust.ts_language();
        tree_sitter::Query::new(&language, "(parameter pattern: (_) type: (_)) (function_item parameters: (parameters)) (function_signature_item parameters: (parameters)) (self_parameter) (impl_item body: (declaration_list)) (trait_item body: (declaration_list)) (mod_item body: (declaration_list)) (foreign_mod_item body: (declaration_list)) (inner_attribute_item) (attribute_item)").unwrap();
    }
}

#[cfg(test)]
mod environment_tests {
    use super::*;
    use crate::{engine::parser, language::LangId};

    fn environment(source: &str) -> Option<Vec<String>> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        rust_parameter_import_environment(tree.root_node(), source.as_bytes())
    }

    #[test]
    fn import_scope_and_configuration_are_part_of_the_environment() {
        let outside = "mod a { mod b {} use crate::One as T; fn f(n: T) {} }";
        let inside = "mod a { mod b { use crate::One as T; } fn f(n: T) {} }";
        assert_ne!(environment(outside).unwrap(), environment(inside).unwrap());
        assert_ne!(
            environment("#[cfg(unix)] use a::T;").unwrap(),
            environment("#[cfg(windows)] use a::T;").unwrap()
        );
        assert_eq!(
            environment("use a::T; fn f(n: T) {}"),
            environment("use a::T; fn f(n: T) { use b::T; }")
        );
    }
}
