//! tracing 0.1 の debug! で、自由関数を指さないログ項目名とメンバー名。

use super::*;
use crate::engine::rust_use_tree::{RustPubUseEdge, UsePathOrigin, expand_rust_use_tree_edges_ast};
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use std::ops::ControlFlow;
use std::ops::Range;
use std::rc::Rc;

#[derive(Clone, Default)]
struct Names {
    aliases: HashSet<String>,
    attribute_bindings: HashSet<String>,
}

impl Names {
    fn invocation(&self, node: Node<'_>, source: &[u8]) -> bool {
        node.child_by_field_name("macro")
            .and_then(|n| n.utf8_text(source).ok())
            .is_some_and(|name| {
                matches!(name, "tracing::debug" | "::tracing::debug")
                    || self.aliases.contains(
                        crate::language::normalize_identifier(LangId::Rust, name).as_ref(),
                    )
            })
    }

    fn scope(
        &self,
        scope: Node<'_>,
        source: &[u8],
        ancestors: &format_owner::Environment,
    ) -> Option<Self> {
        let mut names = self.clone();
        let mut hidden = HashSet::new();
        let mut attributes = Vec::new();
        let mut statements = Vec::new();
        let mut cursor = scope.walk();
        for item in scope.named_children(&mut cursor) {
            match item.kind() {
                "use_declaration" => {
                    let mut edges = Vec::new();
                    expand_rust_use_tree_edges_ast(
                        item.child_by_field_name("argument")?,
                        source,
                        &[],
                        &[],
                        &[],
                        None,
                        &mut edges,
                    )?;
                    for edge in edges {
                        let RustPubUseEdge::Named {
                            exported_name,
                            target_module,
                            target_item,
                            origin,
                            ..
                        } = edge
                        else {
                            return None;
                        };
                        let exported_name =
                            crate::language::normalize_identifier(LangId::Rust, &exported_name)
                                .into_owned();
                        if exported_name == "tracing" {
                            return None;
                        }
                        if matches!(origin, UsePathOrigin::Bare | UsePathOrigin::Global)
                            && target_module == ["tracing"]
                            && target_item == "debug"
                            && !has_attribute_prefix(item)
                            && ancestors.plain(&exported_name)
                        {
                            names.aliases.insert(exported_name);
                        } else {
                            hidden.insert(exported_name);
                        }
                    }
                }
                "macro_definition" => {
                    let name = item.child_by_field_name("name")?.utf8_text(source).ok()?;
                    hidden.insert(
                        crate::language::normalize_identifier(LangId::Rust, name).into_owned(),
                    );
                }
                "mod_item" | "type_item"
                    if item
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        .is_some_and(|n| n.trim_start_matches("r#") == "tracing") =>
                {
                    return None;
                }
                "attribute_item" | "inner_attribute_item" => {
                    attributes.extend(super::super::rust_std_origin::safe_attribute(item, source)?);
                }
                "extern_crate_declaration" => return None,
                "macro_invocation" => statements.push(item),
                "expression_statement" => {
                    if let Some(call) = item
                        .named_child(0)
                        .filter(|n| n.kind() == "macro_invocation")
                    {
                        statements.push(call);
                    }
                }
                _ => {}
            }
        }
        names.aliases.retain(|name| !hidden.contains(name));
        names.attribute_bindings.extend(
            hidden
                .iter()
                .filter(|n| {
                    matches!(n.as_str(), "derive" | "test")
                        || super::super::rust_std_origin::builtin_derive(n)
                })
                .cloned(),
        );
        if ancestors.shadows_any(&attributes)
            || attributes
                .iter()
                .any(|a| names.attribute_bindings.contains(a) || names.aliases.contains(a))
        {
            return None;
        }
        // 未知の文マクロは、後の文の debug! を隠す macro_rules! を生成し得る。
        if statements.iter().any(|n| !names.invocation(*n, source)) {
            return None;
        }
        Some(names)
    }
}

struct Call {
    range: Range<usize>,
    fields: Vec<usize>,
}

fn field_tokens(parts: &[Node<'_>], source: &[u8], fields: &mut Vec<usize>) -> Option<()> {
    if let [message] = parts
        && matches!(message.kind(), "string_literal" | "raw_string_literal")
    {
        return (!message.utf8_text(source).ok()?.contains(['{', '}', '\\'])).then_some(());
    }
    if let [key, colon, value] = parts
        && colon.kind() == ":"
        && matches!(key.utf8_text(source).ok()?, "target" | "parent")
    {
        return matches!(
            value.kind(),
            "identifier" | "string_literal" | "raw_string_literal"
        )
        .then_some(());
    }
    let equal = parts.iter().position(|n| n.kind() == "=")?;
    let (key, rest) = parts.split_at(equal);
    if !matches!(key, [literal] if matches!(literal.kind(), "string_literal" | "raw_string_literal"))
    {
        if key.is_empty()
            || key.len().is_multiple_of(2)
            || !key.iter().enumerate().all(|(i, n)| {
                n.kind()
                    == if i.is_multiple_of(2) {
                        "identifier"
                    } else {
                        "."
                    }
            })
        {
            return None;
        }
        fields.extend(key.iter().step_by(2).map(Node::id));
    }
    let mut value = &rest[1..];
    if value.first().is_some_and(|n| matches!(n.kind(), "?" | "%")) {
        value = &value[1..];
    }
    // tracing は右辺を expr として取る。コピー内の Rust AST で直下の .field を照合し、
    // 未展開のマクロへ渡る token_tree の識別子は候補にしない。
    const PREFIX: &[u8] = b"fn f(){let _=";
    let start = value.first()?.start_byte();
    let rhs = &source[start..value.last()?.end_byte()];
    let copy = [PREFIX, rhs, b"\n;}"].concat();
    let tree = parser::parse_source(&copy, LangId::Rust).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let expr = root
        .named_child(0)?
        .child_by_field_name("body")?
        .named_child(0)?
        .child_by_field_name("value")?;
    if expr.byte_range() != (PREFIX.len()..PREFIX.len() + rhs.len()) {
        return None;
    }
    let direct: HashMap<_, _> = value
        .windows(2)
        .filter(|p| p[0].kind() == "." && p[1].kind() == "identifier")
        .map(|p| (PREFIX.len() + (p[1].start_byte() - start), p[1].id()))
        .collect();
    if !direct.is_empty() {
        let _: ControlFlow<()> = walk_preorder(expr, Children::Named, |node, _| {
            if node.kind() == "token_tree" {
                return ControlFlow::Continue(Walk::Skip);
            }
            if node.kind() == "field_identifier"
                && let Some(&id) = direct.get(&node.start_byte())
            {
                fields.push(id);
            }
            ControlFlow::Continue(Walk::Descend)
        });
    }
    Some(())
}

fn arguments(call: Node<'_>, source: &[u8]) -> Option<Vec<usize>> {
    let body = call.named_child(1).filter(|n| n.kind() == "token_tree")?;
    let mut cursor = body.walk();
    let parts: Vec<_> = body
        .children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect();
    let [open, args @ .., close] = parts.as_slice() else {
        return None;
    };
    if open.kind() != "(" || close.kind() != ")" {
        return None;
    }
    let mut fields = Vec::new();
    for arg in args.split(|n| n.kind() == ",").filter(|s| !s.is_empty()) {
        field_tokens(arg, source, &mut fields)?;
    }
    Some(fields)
}

fn calls(
    root: Node<'_>,
    source: &[u8],
    ancestors: &format_owner::Environment,
) -> Option<Vec<Call>> {
    if root.has_error() {
        return None;
    }
    let base = Rc::new(Names::default().scope(root, source, ancestors)?);
    let opaque = super::super::rust_macro_site::OpaqueAttributeScopes::build(root, source);
    let mut stack = vec![(root, base)];
    let mut calls = Vec::new();
    while let Some((node, names)) = stack.pop() {
        if matches!(
            node.kind(),
            "macro_definition" | "mod_item" | "attribute_item" | "token_tree"
        ) {
            continue;
        }
        if node.kind() == "macro_invocation" {
            if !opaque.contains(node) && names.invocation(node, source) {
                calls.push(Call {
                    range: node.byte_range(),
                    fields: arguments(node, source).unwrap_or_default(),
                });
            }
            continue;
        }
        let names = if node.kind() == "block" {
            let Some(inner) = names.scope(node, source, ancestors) else {
                continue;
            };
            Rc::new(inner)
        } else {
            names
        };
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor).map(|n| (n, names.clone())));
    }
    calls.sort_unstable_by_key(|c| c.range.start);
    Some(calls)
}

impl RustPathOwnerIndex {
    pub(in crate::engine::impact) fn tracing_field<'a>(
        &self,
        node: Node<'_>,
        source: &[u8],
        file: &Path,
        view: impl FnOnce() -> Option<&'a RustPathOwnerProjectProof>,
    ) -> bool {
        self.tracing_fields
            .get_or_init(|| {
                memchr::memmem::find(source, b"tracing")?;
                let project = view()?;
                if !registry_macros::stock_dependency(project, file, "tracing") {
                    return None;
                }
                let mut ancestors = format_owner::Environment::default();
                for parent in enum_owner::ancestor_files(&project.root, file.to_str()?)? {
                    if parent == file || parent == Path::new("src/main.rs") {
                        continue;
                    }
                    if project.reexport_old_sources.contains_key(&parent) {
                        return None;
                    }
                    let data = read_input_file(&project.root.join(parent))?;
                    let tree = parser::parse_source(&data, LangId::Rust).ok()?;
                    ancestors.merge(format_owner::environment(
                        tree.root_node(),
                        &data,
                        None,
                        false,
                    )?);
                }
                let mut root = node;
                while let Some(p) = root.parent() {
                    root = p;
                }
                let current = calls(root, source, &ancestors)?;
                if let Some(previous) = project.reexport_old_sources.get(file) {
                    let old = previous.as_deref()?;
                    let tree = parser::parse_source(old, LangId::Rust).ok()?;
                    let before = calls(tree.root_node(), old, &ancestors)?;
                    // 引数の追随以外に旧側のスコープやマクロ束縛が変わった場合は証明しない。
                    if before.len() != current.len() {
                        return None;
                    }
                    let mut fields = HashSet::new();
                    let (mut a, mut b) = (0, 0);
                    for (prev, next) in before.iter().zip(&current) {
                        if old[a..prev.range.start] != source[b..next.range.start] {
                            return None;
                        }
                        if old[prev.range.clone()] == source[next.range.clone()] {
                            fields.extend(&next.fields)
                        }
                        a = prev.range.end;
                        b = next.range.end;
                    }
                    return (old[a..] == source[b..]).then_some(fields);
                }
                Some(current.into_iter().flat_map(|c| c.fields).collect())
            })
            .as_ref()
            .is_some_and(|ids| ids.contains(&node.id()))
    }
}
