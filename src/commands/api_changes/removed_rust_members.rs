//! 同じファイルの明示的な型注釈から、残存する inherent メソッドへの帰属を証明する。

use std::collections::HashMap;

use tree_sitter::Node;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};

#[derive(Clone)]
pub(super) struct RemovedOwnerProof {
    pub(super) owner: String,
}

struct LocalTypes<'tree> {
    types: HashMap<String, Option<Node<'tree>>>,
    methods: HashMap<(String, String), Option<Node<'tree>>>,
    fields: HashMap<(String, String), Option<String>>,
}

type Bindings = HashMap<String, Option<(String, usize)>>;

fn text_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name = node.utf8_text(source).ok()?;
    let name = name.strip_prefix("r#").unwrap_or(name);
    name.is_ascii().then(|| name.to_owned())
}

fn nominal_type(mut node: Node<'_>, source: &[u8]) -> Option<String> {
    while node.kind() == "reference_type" {
        node = node.child_by_field_name("type")?;
    }
    (node.kind() == "type_identifier")
        .then(|| text_name(node, source))
        .flatten()
}

fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let _: std::ops::ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
        out.push(node);
        std::ops::ControlFlow::Continue(Walk::Descend)
    });
    out
}

fn plain_attribute(node: Node<'_>, source: &[u8]) -> bool {
    let Ok(text) = node.utf8_text(source) else {
        return false;
    };
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    matches!(
        compact.as_str(),
        "#[derive(Clone,Copy)]" | "#[derive(Copy,Clone)]" | "#[derive(Clone)]" | "#[derive(Copy)]"
    )
}

impl<'tree> LocalTypes<'tree> {
    fn collect(root: Node<'tree>, source: &[u8]) -> Option<Self> {
        if !super::rust_ref_sites::clean_tree(root) {
            return None;
        }
        let all = nodes(root);
        // 別名、条件付きの宣言、マクロによる名前解決は、この局所的な証明には含めない。
        if all.iter().any(|node| {
            matches!(
                node.kind(),
                "use_declaration"
                    | "extern_crate_declaration"
                    | "type_item"
                    | "type_parameters"
                    | "where_clause"
                    | "macro_invocation"
                    | "macro_definition"
                    | "inner_attribute_item"
            ) || (node.kind() == "attribute_item" && !plain_attribute(*node, source))
                || (node.kind() == "impl_item" && node.child_by_field_name("trait").is_some())
        }) {
            return None;
        }
        let mut facts = Self {
            types: HashMap::new(),
            methods: HashMap::new(),
            fields: HashMap::new(),
        };
        for node in &all {
            if matches!(node.kind(), "struct_item" | "enum_item")
                && node.parent().is_some_and(|parent| parent.id() == root.id())
                && let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| text_name(name, source))
            {
                facts
                    .types
                    .entry(name)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(*node));
            }
        }
        for node in &all {
            if node.kind() != "impl_item"
                || node.child_by_field_name("trait").is_some()
                || !node.parent().is_some_and(|parent| parent.id() == root.id())
            {
                continue;
            }
            let owner = node.child_by_field_name("type")?;
            let Some(owner) = nominal_type(owner, source).filter(|owner| facts.has_type(owner))
            else {
                continue;
            };
            let body = node.child_by_field_name("body")?;
            let mut cursor = body.walk();
            for method in body.named_children(&mut cursor) {
                if method.kind() != "function_item" {
                    continue;
                }
                let name = text_name(method.child_by_field_name("name")?, source)?;
                facts
                    .methods
                    .entry((owner.clone(), name))
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(method));
            }
        }
        for (owner, definition) in &facts.types {
            let Some(body) = definition.and_then(|node| node.child_by_field_name("body")) else {
                continue;
            };
            if body.kind() != "field_declaration_list" {
                continue;
            }
            let mut cursor = body.walk();
            for field in body.named_children(&mut cursor) {
                if field.kind() != "field_declaration" {
                    continue;
                }
                let name = text_name(field.child_by_field_name("name")?, source)?;
                let ty = field
                    .child_by_field_name("type")
                    .and_then(|ty| nominal_type(ty, source))
                    .filter(|ty| facts.has_type(ty));
                facts
                    .fields
                    .entry((owner.clone(), name))
                    .and_modify(|entry| *entry = None)
                    .or_insert(ty);
            }
        }
        Some(facts)
    }

    fn has_type(&self, name: &str) -> bool {
        self.types.get(name).is_some_and(Option::is_some)
    }

    fn method_owner(&self, method: Node<'_>, source: &[u8]) -> Option<String> {
        let parent = method.parent()?.parent()?;
        if parent.kind() != "impl_item" || parent.child_by_field_name("trait").is_some() {
            return None;
        }
        let owner = nominal_type(parent.child_by_field_name("type")?, source)?;
        let name = text_name(method.child_by_field_name("name")?, source)?;
        self.methods
            .get(&(owner.clone(), name))
            .and_then(|node| *node)
            .filter(|node| node.id() == method.id())
            .map(|_| owner)
    }

    fn instance_method(&self, owner: &str, member: &str, source: &[u8]) -> bool {
        let Some(method) = self
            .methods
            .get(&(owner.to_owned(), member.to_owned()))
            .and_then(|node| *node)
        else {
            return false;
        };
        let Some(parameters) = method.child_by_field_name("parameters") else {
            return false;
        };
        let mut cursor = parameters.walk();
        parameters.named_children(&mut cursor).any(|parameter| {
            parameter.kind() == "self_parameter"
                && parameter
                    .utf8_text(source)
                    .is_ok_and(|text| matches!(text.trim(), "self" | "mut self" | "&self"))
        })
    }
}

pub(super) struct RemovedOwners {
    owners: HashMap<String, String>,
}

impl RemovedOwners {
    pub(super) fn parse(source: &[u8]) -> Option<Self> {
        let tree =
            crate::engine::parser::parse_source(source, crate::language::LangId::Rust).ok()?;
        let facts = LocalTypes::collect(tree.root_node(), source)?;
        Some(Self {
            owners: facts
                .methods
                .into_iter()
                .filter_map(|((owner, member), method)| {
                    method.map(|_| (format!("{owner}.{member}"), owner))
                })
                .collect(),
        })
    }

    /// 旧版にも、別名ではない同じファイル直下の型の inherent メソッドがあったこと。
    pub(super) fn prove(&self, name: &str) -> Option<RemovedOwnerProof> {
        self.owners.get(name).map(|owner| RemovedOwnerProof {
            owner: owner.clone(),
        })
    }
}

fn receiver_type(
    mut receiver: Node<'_>,
    source: &[u8],
    bindings: &Bindings,
    facts: &LocalTypes<'_>,
) -> Option<String> {
    let mut fields = Vec::new();
    while receiver.kind() == "field_expression" {
        fields.push(text_name(receiver.child_by_field_name("field")?, source)?);
        receiver = receiver.child_by_field_name("value")?;
    }
    if !matches!(receiver.kind(), "identifier" | "self") {
        return None;
    }
    let name = text_name(receiver, source)?;
    let (mut ty, start) = bindings.get(&name)?.clone()?;
    if receiver.start_byte() < start {
        return None;
    }
    for field in fields.into_iter().rev() {
        ty = facts.fields.get(&(ty, field))?.clone()?;
    }
    Some(ty)
}

fn function_bindings(
    function: Node<'_>,
    source: &[u8],
    facts: &LocalTypes<'_>,
) -> Option<Bindings> {
    let body = function.child_by_field_name("body")?;
    let parameters = function.child_by_field_name("parameters")?;
    let mut bindings = HashMap::new();
    let mut add = |name, value| {
        bindings
            .entry(name)
            .and_modify(|entry| *entry = None)
            .or_insert(value);
    };
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.kind() == "self_parameter" {
            add(
                "self".to_owned(),
                facts.method_owner(function, source).map(|owner| (owner, 0)),
            );
        } else if parameter.kind() == "parameter" {
            let pattern = parameter.child_by_field_name("pattern")?;
            if pattern.kind() != "identifier" {
                return None;
            }
            let name = text_name(pattern, source)?;
            let ty = parameter
                .child_by_field_name("type")
                .and_then(|ty| nominal_type(ty, source))
                .filter(|ty| facts.has_type(ty));
            add(name, ty.map(|ty| (ty, 0)));
        }
    }
    for node in nodes(body) {
        if node.kind().ends_with("_item")
            || matches!(
                node.kind(),
                "closure_expression" | "for_expression" | "match_arm" | "let_condition"
            )
        {
            return None;
        }
        if node.kind() != "let_declaration" {
            continue;
        }
        if !node.parent().is_some_and(|parent| parent.id() == body.id()) {
            return None;
        }
        let pattern = node.child_by_field_name("pattern")?;
        if pattern.kind() != "identifier" {
            return None;
        }
        let name = text_name(pattern, source)?;
        let ty = node
            .child_by_field_name("type")
            .and_then(|ty| nominal_type(ty, source))
            .filter(|ty| facts.has_type(ty));
        add(name, ty.map(|ty| (ty, node.end_byte())));
    }
    Some(bindings)
}

/// 座標ごとの型名。削除候補との照合は旧版の型の証明と同じファイル内だけで行う。
pub(super) fn positions(
    root: Node<'_>,
    source: &[u8],
    member: &str,
) -> HashMap<(usize, usize), String> {
    let Some(facts) = LocalTypes::collect(root, source) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for ((owner, name), method) in &facts.methods {
        if name == member
            && let Some(name) = method.and_then(|method| method.child_by_field_name("name"))
        {
            let start = name.start_position();
            out.insert((start.row, start.column), owner.clone());
        }
    }
    for function in nodes(root)
        .into_iter()
        .filter(|node| node.kind() == "function_item")
    {
        if !function
            .parent()
            .is_some_and(|parent| parent.id() == root.id())
            && facts.method_owner(function, source).is_none()
        {
            continue;
        }
        let Some(bindings) = function_bindings(function, source, &facts) else {
            continue;
        };
        let Some(body) = function.child_by_field_name("body") else {
            continue;
        };
        for call in nodes(body) {
            if call.kind() != "call_expression" {
                continue;
            }
            let Some(field) = call
                .child_by_field_name("function")
                .filter(|node| node.kind() == "field_expression")
            else {
                continue;
            };
            let Some(name) = field
                .child_by_field_name("field")
                .filter(|name| text_name(*name, source).as_deref() == Some(member))
            else {
                continue;
            };
            let Some(owner) = field
                .child_by_field_name("value")
                .and_then(|receiver| receiver_type(receiver, source, &bindings, &facts))
            else {
                continue;
            };
            if facts.instance_method(&owner, member, source) {
                let start = name.start_position();
                out.insert((start.row, start.column), owner);
            }
        }
    }
    out
}
