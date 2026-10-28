//! 型の値を、既知の引数・配列 callback・辞書の添字の間だけで追う。

use super::*;
use slots::{Binding, Consumer, Plan, Primitive};

struct Proof<'a> {
    inventory: &'a inventory::Inventory,
    plan: &'a Plan,
    removal: &'a shape::Removal,
    callbacks: Vec<Binding>,
}

pub(super) fn audit(
    inventory: &inventory::Inventory,
    plan: &Plan,
    removal: &shape::Removal,
) -> Option<()> {
    let mut proof = Proof {
        inventory,
        plan,
        removal,
        callbacks: Vec::new(),
    };
    let mut used = false;
    let array_names = plan
        .arrays
        .iter()
        .map(|site| site.name.clone())
        .collect::<BTreeSet<_>>();
    for (path, file) in inventory.relevant(&array_names) {
        for node in slots::nodes(inventory.root(path)?) {
            if !crate::engine::js_occurrence::identifier_kind(node.kind()) {
                continue;
            }
            let name = text(node, &file.source)?;
            if !array_names.contains(name) {
                continue;
            }
            let owner = slots::value_owner(inventory, path, node, name)?;
            if !plan.arrays.contains(&owner) {
                continue;
            }
            if !neutral(node, &owner) {
                proof.array_chain(path, node)?;
                used = true;
            }
        }
    }
    let value_names = plan
        .records
        .iter()
        .map(|record| record.site.name.clone())
        .chain(
            plan.consumers
                .iter()
                .map(|consumer| consumer.site.name.clone()),
        )
        .collect::<BTreeSet<_>>();
    for (path, file) in inventory.relevant(&value_names) {
        for node in slots::nodes(inventory.root(path)?) {
            if !crate::engine::js_occurrence::identifier_kind(node.kind()) {
                continue;
            }
            let name = text(node, &file.source)?;
            if !value_names.contains(name) {
                continue;
            }
            let owner = slots::value_owner(inventory, path, node, name)?;
            if let Some(record) = plan.records.iter().find(|record| record.site == owner) {
                if !neutral(node, &owner) {
                    let subscript = node.parent().filter(|parent| {
                        parent.kind() == "subscript_expression"
                            && parent.child_by_field_name("object").map(span) == Some(span(node))
                    })?;
                    used = true;
                    if proof.record_read(path, subscript) != Some(record.primitive) {
                        return None;
                    }
                }
            } else if plan.consumers.iter().any(|consumer| consumer.site == owner)
                && !neutral(node, &owner)
            {
                let call = node.parent().filter(|call| {
                    call.kind() == "call_expression"
                        && call.child_by_field_name("function").map(span) == Some(span(node))
                })?;
                proof.consumer_call(path, call)?;
                used = true;
            }
        }
    }
    for binding in plan.bindings.iter().chain(proof.callbacks.iter()) {
        let file = inventory.files.get(&binding.path)?;
        let scope = inventory
            .root(&binding.path)?
            .descendant_for_byte_range(binding.scope.0, binding.scope.1)?;
        if span(scope) != binding.scope {
            return None;
        }
        for node in slots::nodes(scope) {
            if node.kind() == "for_in_statement"
                && node
                    .child_by_field_name("kind")
                    .is_none_or(|kind| kind.kind() == "var")
                && node
                    .child_by_field_name("left")
                    .is_some_and(|left| pattern_contains(left, &file.source, &binding.name))
            {
                // bare for 左辺を新束縛とみなす resolver を迂回して外側を調べる。
                let (owner, _) = resolve_binding_scope(
                    node.parent()?,
                    file.source.as_bytes(),
                    &binding.name,
                    Namespace::Value,
                )?;
                if span(owner) == binding.scope {
                    return None;
                }
            }
            if ((node.kind() == "variable_declarator"
                && node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "variable_declaration"))
                || matches!(
                    node.kind(),
                    "function_declaration" | "generator_function_declaration"
                ))
                && node
                    .child_by_field_name("name")
                    .is_some_and(|name| pattern_contains(name, &file.source, &binding.name))
            {
                let mut parent = node.parent();
                while let Some(ancestor) = parent {
                    if matches!(
                        ancestor.kind(),
                        "function_declaration"
                            | "function_expression"
                            | "arrow_function"
                            | "generator_function_declaration"
                            | "generator_function"
                            | "method_definition"
                    ) {
                        if span(ancestor) == binding.scope {
                            return None;
                        }
                        break;
                    }
                    parent = ancestor.parent();
                }
            }
            if !matches!(
                node.kind(),
                "identifier"
                    | "shorthand_property_identifier"
                    | "shorthand_property_identifier_pattern"
            ) || text(node, &file.source)? != binding.name
            {
                continue;
            }
            if !proof.binding_owns(binding, &binding.path, node)? {
                continue;
            }
            if span(node) != binding.declaration {
                proof.binding_use(&binding.path, node)?;
                used = true;
            }
        }
    }
    used.then_some(())?;
    builtin_and_transport_audit(inventory, plan, &proof.callbacks)
}

fn neutral(node: Node<'_>, site: &Site) -> bool {
    if node.parent().is_some_and(|parent| {
        span(parent) == site.span
            && parent.child_by_field_name("name").map(span) == Some(span(node))
    }) {
        return true;
    }
    if slots::enclosing_import(node).is_some() {
        return true;
    }
    node.parent()
        .filter(|parent| {
            parent.kind() == "export_specifier" && parent.child_by_field_name("alias").is_none()
        })
        .and_then(|specifier| specifier.parent())
        .and_then(|clause| clause.parent())
        .is_some_and(|statement| {
            statement.kind() == "export_statement"
                && statement.child_by_field_name("source").is_none()
                && !token(statement, "type")
        })
}

impl Proof<'_> {
    fn source(&self, path: &str) -> Option<&str> {
        self.inventory.source(path)
    }
    fn binding_owns(&self, binding: &Binding, path: &str, node: Node<'_>) -> Option<bool> {
        if binding.path != path {
            return Some(false);
        }
        let source = self.source(path)?;
        let (scope, kind) =
            resolve_binding_scope(node, source.as_bytes(), &binding.name, Namespace::Value)?;
        Some(matches!(kind, ScopeBinding::Other) && span(scope) == binding.scope)
    }
    fn known_mode(&self, path: &str, node: Node<'_>) -> bool {
        let Some(node) = unparen(node) else {
            return false;
        };
        let Some(source) = self.source(path) else {
            return false;
        };
        if node.kind() == "string" {
            return decode::string(node, source)
                .is_some_and(|value| self.removal.kept.contains(&value));
        }
        node.kind() == "identifier"
            && self
                .plan
                .bindings
                .iter()
                .chain(self.callbacks.iter())
                .any(|binding| {
                    text(node, source) == Some(binding.name.as_str())
                        && self.binding_owns(binding, path, node) == Some(true)
                })
    }
    fn consumer<'a>(&'a self, path: &str, call: Node<'_>) -> Option<&'a Consumer> {
        if !ordinary_call(call) {
            return None;
        }
        let source = self.source(path)?;
        let callee = call
            .child_by_field_name("function")
            .filter(|node| node.kind() == "identifier")?;
        let owner = slots::value_owner(self.inventory, path, callee, text(callee, source)?)?;
        self.plan
            .consumers
            .iter()
            .find(|consumer| consumer.site == owner)
    }
    fn consumer_call(&self, path: &str, call: Node<'_>) -> Option<Primitive> {
        let consumer = self.consumer(path, call)?;
        let arguments = children(call.child_by_field_name("arguments")?);
        if arguments.len() != consumer.arity
            || consumer
                .positions
                .iter()
                .any(|index| !self.known_mode(path, arguments[*index]))
        {
            return None;
        }
        Some(consumer.result)
    }
    fn record_read(&self, path: &str, node: Node<'_>) -> Option<Primitive> {
        if node.kind() != "subscript_expression"
            || token(node, "?.")
            || node.child_by_field_name("optional_chain").is_some()
            || !read_only(node)
        {
            return None;
        }
        let source = self.source(path)?;
        let object = node
            .child_by_field_name("object")
            .filter(|object| object.kind() == "identifier")?;
        let owner = slots::value_owner(self.inventory, path, object, text(object, source)?)?;
        let record = self
            .plan
            .records
            .iter()
            .find(|record| record.site == owner)?;
        self.known_mode(path, node.child_by_field_name("index")?)
            .then_some(record.primitive)
    }
    fn comparison(&self, path: &str, node: Node<'_>) -> bool {
        if node.kind() != "binary_expression"
            || !node
                .child_by_field_name("operator")
                .is_some_and(|operator| matches!(operator.kind(), "==" | "!=" | "===" | "!=="))
        {
            return false;
        }
        let (Some(left), Some(right)) = (
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
        ) else {
            return false;
        };
        let Some(source) = self.source(path) else {
            return false;
        };
        let literal = |node: Node<'_>| {
            node.kind() == "string"
                && decode::string(node, source)
                    .is_some_and(|value| self.removal.kept.contains(&value))
        };
        (self.known_mode(path, left) && literal(right))
            || (literal(left) && self.known_mode(path, right))
    }
    fn binding_use(&self, path: &str, node: Node<'_>) -> Option<()> {
        let mut current = node;
        for _ in 0..MAX_DEPTH {
            let parent = current.parent()?;
            if parent.kind() == "parenthesized_expression" {
                current = parent;
                continue;
            }
            return match parent.kind() {
                "binary_expression" => self.comparison(path, parent).then_some(()),
                "arguments" => {
                    let call = parent.parent()?;
                    let consumer = self.consumer(path, call)?;
                    let arguments = children(parent);
                    let index = arguments
                        .iter()
                        .position(|argument| span(*argument) == span(current))?;
                    (consumer.positions.contains(&index)
                        && self.consumer_call(path, call).is_some())
                    .then_some(())
                }
                "subscript_expression"
                    if parent.child_by_field_name("index").map(span) == Some(span(current)) =>
                {
                    self.record_read(path, parent).map(|_| ())
                }
                "switch_statement"
                    if parent.child_by_field_name("value").map(span) == Some(span(current)) =>
                {
                    let source = self.source(path)?;
                    let body = parent.child_by_field_name("body")?;
                    children(body)
                        .iter()
                        .all(|case| {
                            case.kind() == "switch_default"
                                || (case.kind() == "switch_case"
                                    && case.child_by_field_name("value").is_some_and(|value| {
                                        value.kind() == "string"
                                            && decode::string(value, source).is_some_and(|value| {
                                                self.removal.kept.contains(&value)
                                            })
                                    }))
                        })
                        .then_some(())
                }
                _ => None,
            };
        }
        None
    }
    fn array_chain(&mut self, path: &str, mut current: Node<'_>) -> Option<()> {
        if current.kind() != "identifier" {
            return None;
        }
        for _ in 0..32 {
            let member = current.parent().filter(|member| {
                member.kind() == "member_expression"
                    && member.child_by_field_name("object").map(span) == Some(span(current))
            })?;
            let source = self.source(path)?;
            if token(member, "?.") || member.child_by_field_name("optional_chain").is_some() {
                return None;
            }
            let method = text(member.child_by_field_name("property")?, source)?.to_owned();
            if !matches!(method.as_str(), "filter" | "map") {
                return None;
            }
            let call = member.parent().filter(|call| {
                ordinary_call(*call)
                    && call.child_by_field_name("function").map(span) == Some(span(member))
            })?;
            let callback = only(call.child_by_field_name("arguments")?)?;
            let (name, declaration, body) = callback_parts(callback, source)?;
            let binding = Binding {
                path: path.to_owned(),
                name,
                scope: span(callback),
                declaration,
            };
            if !self
                .callbacks
                .iter()
                .any(|previous| previous.path == binding.path && previous.scope == binding.scope)
            {
                self.callbacks.push(binding);
            }
            let primitive = if self.comparison(path, body) {
                Some(Primitive::Boolean)
            } else if body.kind() == "call_expression" {
                self.consumer_call(path, body)
            } else if body.kind() == "subscript_expression" {
                self.record_read(path, body)
            } else {
                None
            };
            if method == "filter" {
                if primitive != Some(Primitive::Boolean) {
                    return None;
                }
                current = call;
            } else {
                primitive?;
                return Some(());
            }
        }
        None
    }
}

fn callback_parts<'t>(callback: Node<'t>, source: &str) -> Option<(String, Span, Node<'t>)> {
    if callback.kind() != "arrow_function"
        || token(callback, "async")
        || callback.child_by_field_name("type_parameters").is_some()
        || callback.child_by_field_name("return_type").is_some()
    {
        return None;
    }
    let parameter = if let Some(parameter) = callback.child_by_field_name("parameter") {
        parameter
    } else {
        let parameter = only(callback.child_by_field_name("parameters")?)?;
        if parameter.kind() != "required_parameter"
            || parameter.child_by_field_name("type").is_some()
            || parameter.child_by_field_name("value").is_some()
        {
            return None;
        }
        parameter.child_by_field_name("pattern")?
    };
    if parameter.kind() != "identifier" || !plain_name(text(parameter, source)?) {
        return None;
    }
    let body = unparen(callback.child_by_field_name("body")?)?;
    if body.kind() == "statement_block" {
        return None;
    }
    Some((text(parameter, source)?.to_owned(), span(parameter), body))
}
fn pattern_contains(pattern: Node<'_>, source: &str, name: &str) -> bool {
    any_node(pattern, Children::Named, |node| {
        matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) && text(node, source) == Some(name)
    })
}
fn read_only(mut node: Node<'_>) -> bool {
    for _ in 0..MAX_DEPTH {
        let Some(parent) = node.parent() else {
            return true;
        };
        let writes = match parent.kind() {
            "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => {
                parent.child_by_field_name("left").is_some_and(|left| {
                    left.start_byte() <= node.start_byte() && node.end_byte() <= left.end_byte()
                })
            }
            "update_expression" | "type_query" | "lookup_type" => true,
            "unary_expression" => parent
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == "delete"),
            _ => false,
        };
        if writes {
            return false;
        }
        if matches!(
            parent.kind(),
            "program"
                | "function_declaration"
                | "function_expression"
                | "arrow_function"
                | "method_definition"
        ) {
            return true;
        }
        node = parent;
    }
    false
}

/// 名前が既知の loader API だけを止め、任意の別名や runtime 合成は追跡しない。
fn known_loader_call(node: Node<'_>, source: &str) -> bool {
    if node.kind() != "call_expression" {
        return false;
    }
    let Some(function) = node.child_by_field_name("function").and_then(unparen) else {
        return false;
    };
    let (object, property) = match function.kind() {
        "member_expression" => (
            function.child_by_field_name("object"),
            function
                .child_by_field_name("property")
                .and_then(|node| text(node, source))
                .and_then(decode::identifier),
        ),
        "subscript_expression" => (
            function.child_by_field_name("object"),
            function
                .child_by_field_name("index")
                .and_then(|node| decode::string(node, source)),
        ),
        _ => return false,
    };
    let (Some(object), Some(property)) = (object, property) else {
        return false;
    };
    property == "require"
        || (property == "context"
            && object.kind() == "identifier"
            && text(object, source).and_then(decode::identifier).as_deref() == Some("require"))
        || (matches!(property.as_str(), "glob" | "globEager")
            && object.kind() == "meta_property"
            && token(object, "import"))
}

fn builtin_and_transport_audit(
    inventory: &inventory::Inventory,
    plan: &Plan,
    callbacks: &[Binding],
) -> Option<()> {
    let mut names = BTreeSet::from([plan.target.name.clone()]);
    names.extend(plan.records.iter().map(|record| record.site.name.clone()));
    names.extend(plan.arrays.iter().map(|site| site.name.clone()));
    names.extend(
        plan.consumers
            .iter()
            .map(|consumer| consumer.site.name.clone()),
    );
    let mut escaped_names = names.clone();
    escaped_names.extend(["eval", "Function", "createRequire", "require"].map(str::to_owned));
    escaped_names.extend(
        plan.bindings
            .iter()
            .chain(callbacks.iter())
            .map(|binding| binding.name.clone()),
    );
    let mut builtins = BTreeSet::new();
    if !plan.records.is_empty() {
        builtins.insert("Record");
    }
    if !plan.arrays.is_empty() {
        builtins.extend(["Array", "ReadonlyArray"]);
    }
    let owned = std::iter::once(plan.target.path.as_str())
        .chain(plan.records.iter().map(|record| record.site.path.as_str()))
        .chain(plan.arrays.iter().map(|site| site.path.as_str()))
        .chain(
            plan.consumers
                .iter()
                .map(|consumer| consumer.site.path.as_str()),
        )
        .collect::<BTreeSet<_>>();
    for (path, file) in &inventory.files {
        if matches!(file.kind, inventory::Kind::Embedded) {
            // 未解析の module 輸送は #alias などで所属名を隠せるので、原文だけでは除外しない。
            if file.source.contains('\\')
                || names
                    .iter()
                    .any(|name| super::super::ref_index::contains_js_word(&file.source, name))
                || builtins
                    .iter()
                    .any(|name| super::super::ref_index::contains_js_word(&file.source, name))
                || [
                    "import",
                    "export",
                    "require",
                    "createRequire",
                    "eval",
                    "Function",
                    "with",
                ]
                .iter()
                .any(|name| super::super::ref_index::contains_js_word(&file.source, name))
            {
                return None;
            }
            continue;
        }
        let inventory::Kind::Source(lang) = file.kind else {
            continue;
        };
        if !file.source.contains('\\')
            && ![
                "import",
                "export",
                "require",
                "createRequire",
                "eval",
                "Function",
                "with",
            ]
            .iter()
            .any(|word| file.source.contains(word))
            && !names.iter().any(|name| file.source.contains(name))
            && !builtins.iter().any(|name| file.source.contains(name))
        {
            continue;
        }
        for node in slots::nodes(inventory.root(path)?) {
            let raw = text(node, &file.source)?;
            if node.kind() == "comment"
                && lang == LangId::Javascript
                && raw.starts_with("/**")
                && (raw.contains('\\')
                    || names
                        .iter()
                        .any(|name| super::super::ref_index::contains_js_word(raw, name)))
            {
                return None;
            }
            if crate::engine::js_occurrence::identifier_kind(node.kind()) {
                if raw.contains('\\')
                    && decode::identifier(raw).is_none_or(|name| {
                        escaped_names.contains(&name) || builtins.contains(name.as_str())
                    })
                {
                    return None;
                }
                if matches!(raw, "eval" | "Function" | "createRequire") {
                    return None;
                }
                if builtins.contains(raw) {
                    if node.kind() == "type_identifier" {
                        if resolve_binding_scope(node, file.source.as_bytes(), raw, Namespace::Type)
                            .is_some()
                        {
                            return None;
                        }
                    } else if node.kind() == "identifier" {
                        if resolve_binding_scope(node, file.source.as_bytes(), raw, Namespace::Type)
                            .is_some()
                            || resolve_binding_scope(
                                node,
                                file.source.as_bytes(),
                                raw,
                                Namespace::Value,
                            )
                            .is_some()
                        {
                            return None;
                        }
                        let member = node.parent().filter(|parent| {
                            parent.kind() == "member_expression"
                                && parent.child_by_field_name("object").map(span)
                                    == Some(span(node))
                        })?;
                        if member
                            .child_by_field_name("property")
                            .and_then(|node| text(node, &file.source))
                            == Some("prototype")
                        {
                            return None;
                        }
                        let call = member.parent().filter(|call| {
                            ordinary_call(*call)
                                && call.child_by_field_name("function").map(span)
                                    == Some(span(member))
                        })?;
                        let _ = call;
                    } else {
                        return None;
                    }
                }
            }
            if node.kind() == "with_statement" || known_loader_call(node, &file.source) {
                return None;
            }
            let source = if node.kind() == "import_require_clause" {
                Some(node.child_by_field_name("source")?)
            } else if node.kind() == "import_statement" {
                let clause = children(node)
                    .into_iter()
                    .find(|child| child.kind() == "import_clause");
                let opaque = clause.is_some_and(|clause| {
                    children(clause).iter().any(|node| {
                        node.kind() != "named_imports"
                            || slots::nodes(*node).into_iter().any(|specifier| {
                                specifier.kind() == "import_specifier"
                                    && specifier
                                        .child_by_field_name("name")
                                        .is_some_and(|name| name.kind() != "identifier")
                            })
                    })
                });
                if opaque {
                    node.child_by_field_name("source")
                } else {
                    None
                }
            } else if node.kind() == "export_statement" {
                node.child_by_field_name("source")
            } else if node.kind() == "call_expression"
                && node
                    .child_by_field_name("function")
                    .is_some_and(|function| {
                        function.kind() == "import"
                            || (function.kind() == "identifier"
                                && text(function, &file.source) == Some("require"))
                    })
            {
                Some(only(node.child_by_field_name("arguments")?)?)
            } else {
                None
            };
            if let Some(source) = source {
                let spec = decode::string(source, &file.source)?;
                if spec.starts_with('.') {
                    let target = slots::module(inventory, path, &spec)?;
                    if owned.contains(target.as_str()) {
                        return None;
                    }
                } else {
                    // bare specifier の package exports/imports・外部注入は解決しない。
                    return None;
                }
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const MODE: &str = "export type Mode = 'all' | 'recent';\nexport const LABELS: Record<Mode, string> = { all: 'All', recent: 'Recent' };\nexport function matches(mode: Mode, age: number): boolean { switch(mode) { case 'all': return true; case 'recent': return age < 7; } }\n";
    fn try_plan(
        inventory: &inventory::Inventory,
        extension: &str,
        removal: &shape::Removal,
    ) -> Option<Plan> {
        let path = format!("src/mode.{extension}");
        let site = CompatibleModSite {
            dir: ".",
            base: "HEAD",
            old_path: &path,
            new_path: &path,
            name: "Mode",
            kind: "type",
            old_sig: "",
            new_sig: "",
            lang_id: Some(if extension == "ts" {
                LangId::Typescript
            } else {
                LangId::Tsx
            }),
        };
        slots::collect(inventory, &site, removal)
    }
    fn plan(inventory: &inventory::Inventory, extension: &str, removal: &shape::Removal) -> Plan {
        try_plan(inventory, extension, removal).expect("supported slots")
    }
    #[test]
    fn literal_union_flow_nodes_and_minimal_chains_match_both_grammars() {
        let removal = shape::Removal {
            kept: BTreeSet::from(["all".into(), "recent".into()]),
            removed: BTreeSet::from(["legacy".into()]),
        };
        for (extension, lang) in [("ts", LangId::Typescript), ("tsx", LangId::Tsx)] {
            for query in [
                "(type_alias_declaration name: (type_identifier) value: (_))",
                "(union_type)",
                "(literal_type (string))",
                "(parenthesized_type)",
                "(generic_type name: (type_identifier) type_arguments: (type_arguments))",
                "(readonly_type (array_type))",
                "(variable_declarator name: (identifier) type: (type_annotation) value: (_))",
                "(pair key: (_) value: (_))",
                "(switch_statement value: (_) body: (switch_body))",
                "(switch_case value: (_))",
                "(switch_default)",
                "(binary_expression left: (_) operator: _ right: (_))",
                "(call_expression \"?.\")",
                "(subscript_expression object: (_) index: (_))",
                "(member_expression object: (_) property: (_))",
                "(arrow_function body: (_))",
                "(required_parameter pattern: (identifier))",
                "(import_specifier name: (_))",
                "(import_require_clause source: (string))",
                "(export_statement declaration: (import_alias))",
                "(shorthand_property_identifier_pattern)",
                "(object_pattern)",
                "(for_in_statement left: (_) right: (_))",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), query).unwrap();
            }
            for (chain, expected) in [
                (
                    "MODES.filter(m => matches(m, age)).map(m => LABELS[m])",
                    true,
                ),
                ("MODES.map(m => LABELS[m])", true),
                (
                    "MODES.filter(m => m !== 'recent').filter(m => matches(m, age)).map(m => LABELS[m])",
                    true,
                ),
                ("MODES.filter(matches).map(m => LABELS[m])", false),
                ("MODES.filter(m => matches(m, age))", false),
                ("MODES.map(m => m)", false),
                ("MODES.map(m => unknown(m))", false),
                ("MODES.map?.(m => LABELS[m])", false),
                (
                    "MODES.filter<string>(m => matches(m, age)).map(m => LABELS[m])",
                    false,
                ),
                ("MODES.map((m) => LABELS[m], context)", false),
            ] {
                let mode_path = format!("src/mode.{extension}");
                let view_path = format!("src/view.{extension}");
                let view = format!(
                    "import {{ type Mode, LABELS, matches }} from './mode';\nconst MODES: readonly Mode[] = ['all', 'recent'];\nexport function labels(age: number): string[] {{ return {chain}; }}\n"
                );
                let (_directory, inventory) =
                    inventory::fixture(&[(&mode_path, MODE), (&view_path, &view)]);
                let plan = plan(&inventory, extension, &removal);
                assert_eq!(
                    audit(&inventory, &plan, &removal).is_some(),
                    expected,
                    "{extension}/{chain}"
                );
            }
        }
    }
    #[test]
    fn literal_union_flow_rejects_writes_through_patterns_and_ambiguous_bindings() {
        let removal = shape::Removal {
            kept: BTreeSet::from(["all".into(), "recent".into()]),
            removed: BTreeSet::from(["legacy".into()]),
        };
        for extension in ["ts", "tsx"] {
            for statement in [
                "",
                "var mode;",
                "function mode() {}",
                r"m\u006fde = external;",
                r"{ let m\u006fde = external; return LABELS[mode] !== ''; }",
                "({ mode } = external);",
                "for ({ mode } of values) {}",
                "for (mode of values) {}",
                "for (var mode of values) {}",
                "for (LABELS[mode] of values) {}",
                "({ x: LABELS[mode] } = external);",
                "[LABELS[mode]] = values;",
                "delete LABELS[mode];",
                "LABELS[mode] += 'x';",
            ] {
                let source = format!(
                    "export type Mode = 'all' | 'recent'; export const LABELS: Record<Mode,string> = {{all:'All',recent:'Recent'}}; export function matches(mode: Mode): boolean {{ {statement} return mode === 'all'; }} matches('all');"
                );
                let path = format!("src/mode.{extension}");
                let (_directory, inventory) = inventory::fixture(&[(&path, &source)]);
                let proved = try_plan(&inventory, extension, &removal)
                    .is_some_and(|plan| audit(&inventory, &plan, &removal).is_some());
                assert_eq!(proved, statement.is_empty(), "{extension}/{statement}");
            }
        }
    }
    #[test]
    fn literal_union_flow_distinguishes_independent_type_and_rejects_unknown_values() {
        let removal = shape::Removal {
            kept: BTreeSet::from(["all".into(), "recent".into()]),
            removed: BTreeSet::from(["legacy".into()]),
        };
        let view = "import { type Mode, LABELS, matches } from './mode'; const MODES: Mode[] = ['all']; export const labels = MODES.filter(m => matches(m, 1)).map(m => LABELS[m]);";
        for (extra, expected) in [
            (
                "export {}; type Mode = 'other'; const value: Mode = 'other';",
                true,
            ),
            ("import { matches } from './mode'; matches('all', 1);", true),
            (
                "import { matches } from './mode'; matches(external, 1);",
                false,
            ),
            (
                "import { matches } from './mode'; type Query = typeof matches;",
                false,
            ),
            ("import * as namespace from './mode';", false),
            ("import * as namespace from '#mode';", false),
            ("const namespace = require('#mode');", false),
        ] {
            let (_directory, inventory) = inventory::fixture(&[
                ("src/mode.ts", MODE),
                ("src/view.ts", view),
                ("src/extra.ts", extra),
            ]);
            let plan = plan(&inventory, "ts", &removal);
            assert_eq!(
                audit(&inventory, &plan, &removal).is_some(),
                expected,
                "{extra}"
            );
        }
    }
    #[test]
    fn literal_union_flow_checks_embedded_transports_and_import_require_in_both_grammars() {
        let removal = shape::Removal {
            kept: BTreeSet::from(["all".into(), "recent".into()]),
            removed: BTreeSet::from(["legacy".into()]),
        };
        for (extension, lang) in [("ts", LangId::Typescript), ("tsx", LangId::Tsx)] {
            let mode_path = format!("src/mode.{extension}");
            let view_path = format!("src/view.{extension}");
            let view = "import { type Mode, LABELS, matches } from './mode'; const MODES: Mode[] = ['all']; export const labels = MODES.filter(m => matches(m, 1)).map(m => LABELS[m]);";
            for (extra_path, extra, expected) in [
                (
                    "src/picker.vue",
                    "<template><p>unrelated</p></template>",
                    true,
                ),
                (
                    "src/picker.vue",
                    "<script>matches(external, 1)</script>",
                    false,
                ),
                (
                    "src/picker.vue",
                    "<script>import * as opaque from '#alias'; opaque['matches'](external, 1)</script>",
                    false,
                ),
                (
                    "src/picker.vue",
                    "<script>type Local = Mode;</script>",
                    false,
                ),
                (
                    "src/picker.vue",
                    r"<script>m\u0061tches(external, 1)</script>",
                    false,
                ),
                (
                    "src/extra.ts",
                    "import opaque = require('./mode'); opaque['matches'](external,1);",
                    false,
                ),
                ("src/extra.ts", "import opaque = require('#alias');", false),
                ("src/extra.ts", "import opaque = require('./other');", true),
                (
                    "src/extra.ts",
                    "export import opaque = require('./mode');",
                    false,
                ),
            ] {
                let (_directory, inventory) = inventory::fixture(&[
                    (&mode_path, MODE),
                    (&view_path, view),
                    (extra_path, extra),
                    ("src/other.ts", "export const other = 1;"),
                ]);
                let plan = plan(&inventory, extension, &removal);
                assert_eq!(
                    audit(&inventory, &plan, &removal).is_some(),
                    expected,
                    "{extension}/{extra}"
                );
            }
            let source = "import opaque = require('./mode');";
            let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
            assert!(!tree.root_node().has_error());
            let clause = nodes_for_test(tree.root_node(), "import_require_clause");
            assert_eq!(clause.len(), 1);
            assert_eq!(
                text(clause[0].child_by_field_name("source").unwrap(), source),
                Some("'./mode'")
            );
            let source = "export import opaque = require('./mode');";
            let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
            assert!(tree.root_node().has_error());
            // この文法が export import require を未対応の間も、輸送を不在扱いしない。
            assert!(!nodes_for_test(tree.root_node(), "import_alias").is_empty());
        }
    }
    fn nodes_for_test<'t>(root: Node<'t>, kind: &str) -> Vec<Node<'t>> {
        slots::nodes(root)
            .into_iter()
            .filter(|node| node.kind() == kind)
            .collect()
    }
    #[test]
    fn literal_union_flow_rejects_escaped_callback_shadowing() {
        let removal = shape::Removal {
            kept: BTreeSet::from(["all".into(), "recent".into()]),
            removed: BTreeSet::from(["legacy".into()]),
        };
        for extension in ["ts", "tsx"] {
            let mode_path = format!("src/mode.{extension}");
            let view_path = format!("src/view.{extension}");
            for (body, expected) in [
                ("LABELS[m]", true),
                (r"LABELS[\u006d]", false),
                (r"matches(\u006d = external, 1)", false),
            ] {
                let view = format!(
                    "import {{type Mode, LABELS, matches}} from './mode'; const MODES: Mode[]=['all']; const labels=MODES.map(m => {body});"
                );
                let (_directory, inventory) =
                    inventory::fixture(&[(&mode_path, MODE), (&view_path, &view)]);
                let plan = plan(&inventory, extension, &removal);
                assert_eq!(
                    audit(&inventory, &plan, &removal).is_some(),
                    expected,
                    "{extension}/{body}"
                );
            }
        }
    }
    #[test]
    fn literal_union_flow_rejects_known_loader_apis_and_escaped_control_names() {
        let removal = shape::Removal {
            kept: BTreeSet::from(["all".into(), "recent".into()]),
            removed: BTreeSet::from(["legacy".into()]),
        };
        for (extension, lang) in [("ts", LangId::Typescript), ("tsx", LangId::Tsx)] {
            for query in [
                "(call_expression function: (member_expression object: (_) property: (property_identifier)))",
                "(member_expression object: (meta_property))",
                "(subscript_expression object: (_) index: (string))",
                "(import_specifier name: (identifier) alias: (identifier))",
                r#"(meta_property "import")"#,
            ] {
                tree_sitter::Query::new(&lang.ts_language(), query).unwrap();
            }
            for (source, expected) in [
                ("module.require('./mode');", true),
                ("module['require']('./mode');", true),
                ("require.context('./', true, /mode/);", true),
                ("require['context']('./');", true),
                ("import.meta.glob('./*.ts');", true),
                ("import.meta.globEager('./*.ts');", true),
                ("import.meta['glob']('./*.ts');", true),
                ("object.context('./');", false),
                ("object.glob('./');", false),
            ] {
                let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{extension}/{source}");
                assert_eq!(
                    slots::nodes(tree.root_node())
                        .into_iter()
                        .any(|node| known_loader_call(node, source)),
                    expected,
                    "{extension}/{source}"
                );
            }
            let mode_path = format!("src/mode.{extension}");
            let extra_path = format!("src/extra.{extension}");
            for extra in [
                "module.require('./mode');",
                "require.context('./', true, /mode/);",
                "import.meta.glob('./*.ts');",
                "import.meta.globEager('./*.ts');",
                "import {createRequire as loader} from 'node:module';",
                "createRequire(import.meta.url);",
                r"import {createRequ\u0069re as loader} from 'node:module';",
                r"\u0065val('unknown');",
                r"new Funct\u0069on('unknown');",
            ] {
                let (_directory, inventory) =
                    inventory::fixture(&[(&mode_path, MODE), (&extra_path, extra)]);
                let plan = plan(&inventory, extension, &removal);
                assert!(
                    audit(&inventory, &plan, &removal).is_none(),
                    "{extension}/{extra}"
                );
            }
            let (_directory, inventory) = inventory::fixture(&[
                (&mode_path, MODE),
                (&extra_path, "export const text = 'createRequire';"),
            ]);
            let plan = plan(&inventory, extension, &removal);
            assert!(audit(&inventory, &plan, &removal).is_some());
        }
    }
    #[test]
    fn literal_union_flow_audits_string_named_imports_in_both_grammars() {
        let removal = shape::Removal {
            kept: BTreeSet::from(["all".into(), "recent".into()]),
            removed: BTreeSet::from(["legacy".into()]),
        };
        for (extension, lang) in [("ts", LangId::Typescript), ("tsx", LangId::Tsx)] {
            tree_sitter::Query::new(
                &lang.ts_language(),
                "(import_specifier name: (string) alias: (identifier))",
            )
            .unwrap();
            let source =
                r#"import { "matches" as check } from './mode'; check(external as never, 1);"#;
            let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
            assert!(!tree.root_node().has_error());
            let specifiers = nodes_for_test(tree.root_node(), "import_specifier");
            assert_eq!(specifiers.len(), 1);
            assert_eq!(
                specifiers[0].child_by_field_name("name").unwrap().kind(),
                "string"
            );
            let mode_path = format!("src/mode.{extension}");
            let extra_path = format!("src/extra.{extension}");
            for (extra, expected) in [
                (source, false),
                (
                    r#"import { "LABELS" as lookup } from './mode'; lookup[external];"#,
                    false,
                ),
                (
                    r#"import { "other" as independent } from './other'; independent();"#,
                    true,
                ),
            ] {
                let (_directory, inventory) = inventory::fixture(&[
                    (&mode_path, MODE),
                    (&extra_path, extra),
                    ("src/other.ts", "export function other(): void {}"),
                ]);
                let plan = plan(&inventory, extension, &removal);
                assert_eq!(
                    audit(&inventory, &plan, &removal).is_some(),
                    expected,
                    "{extension}/{extra}"
                );
            }
        }
    }
}
