//! 条件付きの子 module と、glob に隠され得る extern prelude 名の限定的な証明。

use std::collections::{BTreeSet, HashMap, HashSet};

use super::*;

/// 各 cfg 属性を独立した必要条件として持つ。式同士の同値変形は推測しない。
#[derive(Clone, Debug, Default)]
pub(crate) struct Activation(Option<BTreeSet<Vec<String>>>);

impl Activation {
    fn plain() -> Self {
        Self(Some(BTreeSet::new()))
    }

    pub(crate) fn item(item: tree_sitter::Node<'_>, source: &[u8]) -> Self {
        let mut result = Self::plain();
        let mut previous = item.prev_named_sibling();
        while let Some(node) = previous {
            match node.kind() {
                "line_comment" | "block_comment" => {}
                "attribute_item" => result.add_attribute(node, source),
                _ => break,
            }
            previous = node.prev_named_sibling();
        }
        result
    }

    fn inner(scope: tree_sitter::Node<'_>, source: &[u8]) -> Self {
        let mut result = Self::plain();
        let mut cursor = scope.walk();
        for child in scope.named_children(&mut cursor) {
            if child.kind() == "inner_attribute_item" {
                result.add_attribute(child, source);
            }
        }
        result
    }

    fn add_attribute(&mut self, node: tree_sitter::Node<'_>, source: &[u8]) {
        let Some(attribute) = node.named_child(0) else {
            self.0 = None;
            return;
        };
        let name = attribute
            .named_child(0)
            .and_then(|n| n.utf8_text(source).ok());
        if name == Some("cfg") {
            let Some(arguments) = attribute.child_by_field_name("arguments") else {
                self.0 = None;
                return;
            };
            let mut tokens = Vec::new();
            let mut stack = vec![arguments];
            while let Some(node) = stack.pop() {
                if matches!(node.kind(), "line_comment" | "block_comment") {
                    continue;
                }
                if node.child_count() == 0
                    || matches!(node.kind(), "string_literal" | "raw_string_literal")
                {
                    let Ok(text) = node.utf8_text(source) else {
                        self.0 = None;
                        return;
                    };
                    tokens.push(text.to_string());
                } else {
                    let mut cursor = node.walk();
                    let start = stack.len();
                    stack.extend(node.children(&mut cursor));
                    stack[start..].reverse();
                }
            }
            if let Some(conditions) = &mut self.0 {
                conditions.insert(tokens);
            }
        } else if !attribute_is_plain(node, source)
            || matches!(name, Some("derive" | "no_implicit_prelude"))
        {
            self.0 = None;
        }
    }

    fn and(&mut self, other: &Self) {
        match (&mut self.0, &other.0) {
            (Some(left), Some(right)) => left.extend(right.iter().cloned()),
            _ => self.0 = None,
        }
    }

    fn implies(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(active), Some(required)) => required.is_subset(active),
            _ => false,
        }
    }
}

#[derive(Default)]
struct Namespace {
    inner: Activation,
    children: HashMap<String, Vec<Activation>>,
    names: HashSet<String>,
    child_competitors: HashSet<String>,
    globs: Vec<RustPubUseEdge>,
    opaque: bool,
}

#[derive(Default)]
pub(crate) struct BindingProof {
    scopes: HashMap<Vec<String>, Namespace>,
}

impl BindingProof {
    pub(crate) fn collect(
        &mut self,
        root: tree_sitter::Node<'_>,
        source: &[u8],
        module: &[String],
    ) {
        let mut stack = vec![(root, module.to_vec())];
        while let Some((scope, module)) = stack.pop() {
            let bindings = ScopeBindings::collect(scope, source, &module);
            let mut namespace = Namespace {
                inner: Activation::inner(scope, source),
                names: bindings.child_competitors.clone(),
                child_competitors: bindings.child_competitors,
                ..Namespace::default()
            };
            let mut cursor = scope.walk();
            for item in scope.named_children(&mut cursor) {
                if item.is_error() || item.is_missing() {
                    namespace.opaque = true;
                }
                if !matches!(
                    item.kind(),
                    "attribute_item" | "inner_attribute_item" | "line_comment" | "block_comment"
                ) && Activation::item(item, source).0.is_none()
                {
                    namespace.opaque = true;
                }
                match item.kind() {
                    "mod_item" => {
                        let Some(name) = item_name(item, source) else {
                            namespace.opaque = true;
                            continue;
                        };
                        namespace
                            .children
                            .entry(name.clone())
                            .or_default()
                            .push(Activation::item(item, source));
                        if let Some(body) = item.child_by_field_name("body") {
                            let mut child = module.clone();
                            child.push(name);
                            stack.push((body, child));
                        }
                    }
                    "use_declaration" => {
                        let mut edges = Vec::new();
                        let has_wildcard =
                            item.child_by_field_name("argument")
                                .is_some_and(|argument| {
                                    crate::engine::tree_walk::any_node(
                                        argument,
                                        crate::engine::tree_walk::Children::All,
                                        |node| node.kind() == "use_wildcard",
                                    )
                                });
                        let expanded = item.child_by_field_name("argument").and_then(|argument| {
                            expand_rust_use_tree_edges_ast(
                                argument,
                                source,
                                &[],
                                &module,
                                &module,
                                None,
                                &mut edges,
                            )
                        });
                        if expanded.is_none() {
                            namespace.opaque = true;
                        }
                        if has_wildcard
                            && !edges
                                .iter()
                                .any(|edge| matches!(edge, RustPubUseEdge::Wildcard { .. }))
                        {
                            namespace.opaque = true;
                        }
                        for edge in edges {
                            match edge {
                                RustPubUseEdge::Named { exported_name, .. } => {
                                    // 同名の子 mod と共存できる import でも、名前の不在は証明しない。
                                    namespace.names.insert(
                                        normalize_raw_identifier(&exported_name).to_string(),
                                    );
                                }
                                RustPubUseEdge::Wildcard { .. } => namespace.globs.push(edge),
                            }
                        }
                    }
                    "macro_invocation" | "expression_statement" | "foreign_mod_item" => {
                        namespace.opaque = true
                    }
                    _ => {}
                }
            }
            // 同じ module を複数のファイルや inline 宣言が作る場合も、片方だけを採らない。
            match self.scopes.entry(module) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(namespace);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().opaque = true;
                }
            }
        }
    }

    fn activation(&self, module: &[String]) -> Option<Activation> {
        let mut active = Activation::plain();
        for depth in 0..=module.len() {
            let scope = self.scopes.get(&module[..depth])?;
            if scope.opaque {
                return None;
            }
            active.and(&scope.inner);
            if depth > 0 {
                let parent = self.scopes.get(&module[..depth - 1])?;
                if parent.child_competitors.contains(&module[depth - 1]) {
                    return None;
                }
                let [declaration] = parent.children.get(&module[depth - 1])?.as_slice() else {
                    return None;
                };
                active.and(declaration);
            }
        }
        active.0.as_ref()?;
        Some(active)
    }

    pub(crate) fn refine(
        &self,
        edges: Vec<CollectedPubUseEdge>,
        edition: Option<crate::engine::rust_edition::RustEdition>,
    ) -> Vec<CollectedPubUseEdge> {
        if edition != Some(crate::engine::rust_edition::RustEdition::E2018OrLater) {
            return edges;
        }
        let mut result = Vec::with_capacity(edges.len());
        for mut collected in edges {
            if matches!(
                collected.bare_leading,
                Some(BareLeadingBinding::Unresolved | BareLeadingBinding::ChildModule)
            ) {
                let (RustPubUseEdge::Named { source_module, .. }
                | RustPubUseEdge::Wildcard { source_module, .. }) = &collected.edge;
                if let (Some(scope), Some(mut active), Some(head)) = (
                    self.scopes.get(source_module),
                    self.activation(source_module),
                    bare_leading_name(&collected.edge),
                ) {
                    let raw_head = head;
                    let head = normalize_raw_identifier(head);
                    active.and(&collected.activation);
                    let mut child = source_module.clone();
                    child.push(head.to_string());
                    if head == raw_head
                        && !scope.child_competitors.contains(head)
                        && scope.children.contains_key(head)
                        && self
                            .activation(&child)
                            .is_some_and(|required| active.implies(&required))
                    {
                        collected.bare_leading = Some(BareLeadingBinding::GuardedChildModule);
                    } else if active.0.is_some()
                        && !scope.children.contains_key(head)
                        && !scope.names.contains(head)
                        && self
                            .scopes
                            .get(&[][..])
                            .is_some_and(|root| !root.names.contains(head))
                        && !scope.globs.is_empty()
                        && self.globs_lack_name(scope, head)
                    {
                        // この edge は extern prelude へ解決した。ローカルの公開経路には混ぜない。
                        continue;
                    }
                }
            }
            result.push(collected);
        }
        result
    }

    fn globs_lack_name(&self, scope: &Namespace, name: &str) -> bool {
        let mut pending: Vec<_> = scope.globs.iter().collect();
        let mut seen = HashSet::new();
        while let Some(edge) = pending.pop() {
            let RustPubUseEdge::Wildcard {
                source_module,
                target_module,
                origin,
            } = edge
            else {
                return false;
            };
            let target = match origin {
                UsePathOrigin::Anchored => target_module.clone(),
                UsePathOrigin::Bare => {
                    // Bare な glob は、無条件の子 module を経由するときだけ解決する。
                    let mut path = source_module.clone();
                    path.extend(target_module.iter().cloned());
                    if self
                        .activation(&path)
                        .is_none_or(|a| a.0.is_none_or(|conditions| !conditions.is_empty()))
                    {
                        return false;
                    }
                    path
                }
                UsePathOrigin::Global => return false,
            };
            if !seen.insert(target.clone()) {
                continue;
            }
            let Some(namespace) = self.scopes.get(&target) else {
                return false;
            };
            if self.activation(&target).is_none()
                || namespace.names.contains(name)
                || namespace.children.contains_key(name)
            {
                return false;
            }
            pending.extend(namespace.globs.iter());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_matches_tokens_without_merging_literal_contents() {
        fn guard(text: &str) -> Activation {
            let tree =
                parser::parse_source(text.as_bytes(), crate::language::LangId::Rust).unwrap();
            assert!(!tree.root_node().has_error());
            Activation::item(tree.root_node().named_child(1).unwrap(), text.as_bytes())
        }
        let spaced = guard("#[cfg(feature = \"f a\")] mod child;");
        let same = guard("#[cfg( feature/* comment */=\"f a\" )] mod child;");
        let different = guard("#[cfg(feature=\"fa\")] mod child;");
        assert!(same.implies(&spaced));
        assert!(!different.implies(&spaced));
        assert!(!guard("#[cfg_attr(unix, cfg(feature=\"f a\"))] mod child;").implies(&spaced));
    }

    #[test]
    fn extern_glob_proof_preserves_self_crate_aliases_and_unknown_ancestors() {
        for (prefix, external) in [
            ("", true),
            ("extern crate self as core;", false),
            ("#![no_implicit_prelude]", false),
        ] {
            let source = format!(
                "{prefix}\npub mod backend {{}} pub mod prelude {{ pub use crate::backend::*; pub use core::fmt::Debug; }}"
            );
            let tree =
                parser::parse_source(source.as_bytes(), crate::language::LangId::Rust).unwrap();
            assert!(!tree.root_node().has_error());
            let mut proof = BindingProof::default();
            proof.collect(tree.root_node(), source.as_bytes(), &[]);
            let mut edges = Vec::new();
            collect_pub_use_edges(tree.root_node(), source.as_bytes(), &[], &mut edges).unwrap();
            let refined = proof.refine(
                edges,
                Some(crate::engine::rust_edition::RustEdition::E2018OrLater),
            );
            assert_eq!(refined.len(), if external { 1 } else { 2 }, "{prefix}");
            if !external {
                assert_eq!(
                    refined[1].bare_leading,
                    Some(BareLeadingBinding::Unresolved)
                );
            }
        }
    }
}
