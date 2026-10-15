//! Optional inline props の変更が、各 JSX caller に適用されるかを証明する。

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::engine::{diff, jsx, parser, symbols};
use crate::language::LangId;
use crate::models::impact::AffectedSymbol;
use crate::models::symbol::Symbol;

pub(super) struct OptionalPropChange {
    changed: HashSet<String>,
}

pub(super) struct JsxShape {
    supplied: HashSet<String>,
    children: bool,
}

impl OptionalPropChange {
    pub(super) fn is_omitted_by(&self, shape: &JsxShape) -> bool {
        self.changed.is_disjoint(&shape.supplied)
            && !(self.changed.contains("children") && shape.children)
    }
}

fn name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?;
    (!text.is_empty() && !text.contains('\\')).then(|| text.to_owned())
}

fn tokens(node: Node<'_>, source: &[u8], omitted: &[usize]) -> Option<Vec<(String, String)>> {
    let mut result = Vec::new();
    walk_preorder(node, Children::All, |node, _| {
        if node.is_extra() || omitted.contains(&node.id()) {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.child_count() == 0 {
            let Ok(text) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            result.push((node.kind().to_owned(), text.to_owned()));
        }
        ControlFlow::Continue(Walk::Descend)
    })
    .is_continue()
    .then_some(result)
}

fn property<'tree>(node: Node<'tree>, source: &[u8]) -> Option<(String, bool, Node<'tree>)> {
    if node.kind() != "property_signature" {
        return None;
    }
    let key = node.child_by_field_name("name")?;
    if key.kind() != "property_identifier" {
        return None;
    }
    let annotation = node.child_by_field_name("type")?;
    let mut cursor = annotation.walk();
    let mut types = annotation
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra());
    let ty = types.next()?;
    if types.next().is_some() {
        return None;
    }
    let mut cursor = node.walk();
    if node
        .named_children(&mut cursor)
        .any(|child| !child.is_extra() && child.id() != key.id() && child.id() != annotation.id())
    {
        return None;
    }
    let mut cursor = node.walk();
    let optional = node.children(&mut cursor).any(|child| child.kind() == "?");
    Some((name(key, source)?, optional, ty))
}

/// 外部名を参照しない、最小の型だけを反復で検査する。
fn closed_type(ty: Node<'_>, source: &[u8]) -> bool {
    let mut pending = vec![ty];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "predefined_type" => {
                if !node.utf8_text(source).is_ok_and(|text| {
                    matches!(
                        text,
                        "string"
                            | "number"
                            | "boolean"
                            | "bigint"
                            | "symbol"
                            | "object"
                            | "any"
                            | "unknown"
                            | "never"
                            | "void"
                    )
                }) {
                    return false;
                }
            }
            "literal_type" => {
                if node.named_child(0).is_none_or(|value| {
                    !matches!(
                        value.kind(),
                        "string" | "number" | "true" | "false" | "null" | "undefined"
                    )
                }) {
                    return false;
                }
            }
            "object_type" => {
                let mut names = HashSet::new();
                let mut cursor = node.walk();
                for member in node
                    .named_children(&mut cursor)
                    .filter(|node| !node.is_extra())
                {
                    let Some((key, _, ty)) = property(member, source) else {
                        return false;
                    };
                    if !names.insert(key) {
                        return false;
                    }
                    pending.push(ty);
                }
            }
            _ => return false,
        }
    }
    true
}

fn plain_pattern(pattern: Node<'_>, source: &[u8]) -> bool {
    if pattern.kind() == "identifier" {
        return name(pattern, source).is_some();
    }
    if pattern.kind() != "object_pattern" {
        return false;
    }
    let mut keys = HashSet::new();
    let mut bindings = HashSet::new();
    let mut cursor = pattern.walk();
    for entry in pattern
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra())
    {
        let pair = match entry.kind() {
            "shorthand_property_identifier_pattern" => Some((entry, entry)),
            "pair_pattern" => entry
                .child_by_field_name("key")
                .zip(entry.child_by_field_name("value"))
                .filter(|(key, value)| {
                    key.kind() == "property_identifier" && value.kind() == "identifier"
                }),
            _ => None,
        };
        let Some((key, binding)) = pair else {
            return false;
        };
        let (Some(key), Some(binding)) = (name(key, source), name(binding, source)) else {
            return false;
        };
        if !keys.insert(key) || !bindings.insert(binding) {
            return false;
        }
    }
    true
}

struct Prop {
    optional: bool,
    tokens: Vec<(String, String)>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum ReturnCategory {
    Jsx,
    String,
    Number,
    Boolean,
    Null,
}

fn has_jsx_pragma(root: Node<'_>, source: &[u8]) -> bool {
    walk_preorder(root, Children::Named, |node, _| {
        if node.kind() == "comment"
            && node
                .utf8_text(source)
                .is_ok_and(|text| text.contains("@jsx"))
        {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(Walk::Descend)
    })
    .is_break()
}

fn return_category(mut value: Node<'_>, source: &[u8]) -> Option<ReturnCategory> {
    while value.kind() == "parenthesized_expression" {
        let mut cursor = value.walk();
        let mut children = value
            .named_children(&mut cursor)
            .filter(|child| !child.is_extra());
        value = children.next()?;
        if children.next().is_some() {
            return None;
        }
    }
    match value.kind() {
        "jsx_element" | "jsx_self_closing_element" => Some(ReturnCategory::Jsx),
        "string" => Some(ReturnCategory::String),
        "number" if !value.utf8_text(source).ok()?.ends_with('n') => Some(ReturnCategory::Number),
        "true" | "false" => Some(ReturnCategory::Boolean),
        "null" => Some(ReturnCategory::Null),
        _ => None,
    }
}

fn inferred_returns(body: Node<'_>, source: &[u8]) -> Option<HashSet<ReturnCategory>> {
    let mut cursor = body.walk();
    if body
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra())
        .last()?
        .kind()
        != "return_statement"
    {
        return None;
    }
    let mut result = HashSet::new();
    walk_preorder(body, Children::Named, |node, _| {
        if matches!(
            node.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "function_expression"
                | "generator_function"
                | "arrow_function"
                | "method_definition"
                | "class_declaration"
                | "abstract_class_declaration"
                | "class"
        ) {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.kind() == "return_statement" {
            let mut cursor = node.walk();
            let mut values = node
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra());
            let Some(category) = values
                .next()
                .and_then(|value| return_category(value, source))
            else {
                return ControlFlow::Break(());
            };
            if values.next().is_some() {
                return ControlFlow::Break(());
            }
            result.insert(category);
            return ControlFlow::Continue(Walk::Skip);
        }
        ControlFlow::Continue(Walk::Descend)
    })
    .is_continue()
    .then_some(result)
}

struct Contract {
    header: Vec<(String, String)>,
    props: HashMap<String, Prop>,
    inferred_returns: Option<HashSet<ReturnCategory>>,
}

fn contract(function: Node<'_>, source: &[u8], jsx_pragma: bool) -> Option<Contract> {
    if function.kind() != "function_declaration"
        || function.has_error()
        || function.child(0)?.kind() != "function"
        || function.child_by_field_name("type_parameters").is_some()
    {
        return None;
    }
    let params = function.child_by_field_name("parameters")?;
    let mut cursor = params.walk();
    let mut params = params
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra());
    let param = params.next()?;
    if params.next().is_some()
        || !matches!(param.kind(), "required_parameter" | "optional_parameter")
        || param.child_by_field_name("value").is_some()
        || !plain_pattern(
            param
                .child_by_field_name("pattern")
                .or_else(|| param.child_by_field_name("name"))?,
            source,
        )
    {
        return None;
    }
    let annotation = param.child_by_field_name("type")?;
    let ty = annotation.named_child(0)?;
    if ty.kind() != "object_type" || !closed_type(ty, source) {
        return None;
    }
    let body = function.child_by_field_name("body")?;
    let inferred_returns = if let Some(return_type) = function.child_by_field_name("return_type") {
        if return_type.kind() != "type_annotation"
            || !closed_type(return_type.named_child(0)?, source)
        {
            return None;
        }
        None
    } else {
        if jsx_pragma {
            return None;
        }
        Some(inferred_returns(body, source)?)
    };
    let mut props = HashMap::new();
    let mut cursor = ty.walk();
    for member in ty
        .named_children(&mut cursor)
        .filter(|node| !node.is_extra())
    {
        let (key, optional, _) = property(member, source)?;
        if props
            .insert(
                key,
                Prop {
                    optional,
                    tokens: tokens(member, source, &[])?,
                },
            )
            .is_some()
        {
            return None;
        }
    }
    let header = function
        .parent()
        .filter(|parent| parent.kind() == "export_statement")
        .unwrap_or(function);
    Some(Contract {
        header: tokens(header, source, &[annotation.id(), body.id()])?,
        props,
        inferred_returns,
    })
}

fn root_functions<'tree>(root: Node<'tree>, source: &[u8]) -> HashMap<String, Vec<Node<'tree>>> {
    let mut result: HashMap<String, Vec<Node<'_>>> = HashMap::new();
    let mut cursor = root.walk();
    for mut node in root.named_children(&mut cursor) {
        if node.kind() == "export_statement" {
            let Some(declaration) = node.child_by_field_name("declaration") else {
                continue;
            };
            node = declaration;
        }
        if matches!(node.kind(), "function_declaration" | "function_signature")
            && let Some(key) = node
                .child_by_field_name("name")
                .and_then(|node| name(node, source))
        {
            result.entry(key).or_default().push(node);
        }
    }
    result
}

/// 新側 AST を共有し、旧側の復元・parse は候補ファイルにつき一度だけ行う。
pub(super) fn collect(
    file_diff: &str,
    path: &str,
    root: Node<'_>,
    source: &[u8],
    lang: LangId,
    syms: &[Symbol],
    affected: &[AffectedSymbol],
) -> HashMap<String, OptionalPropChange> {
    let mut result = HashMap::new();
    if !matches!(lang, LangId::Typescript | LangId::Tsx) || root.has_error() {
        return result;
    }
    let new_functions = root_functions(root, source);
    let new_jsx_pragma = has_jsx_pragma(root, source);
    let candidates: Vec<_> = affected
        .iter()
        .filter(|symbol| symbol.change_type == "modified")
        .filter_map(|symbol| {
            let nodes = new_functions.get(&symbol.name)?;
            if nodes.len() != 1
                || syms
                    .iter()
                    .filter(|candidate| candidate.name == symbol.name)
                    .count()
                    != 1
            {
                return None;
            }
            Some((
                symbol.name.clone(),
                contract(nodes[0], source, new_jsx_pragma)?,
            ))
        })
        .collect();
    if candidates.is_empty() {
        return result;
    }
    let Some(old) = diff::reconstruct_old_source(file_diff, path, source) else {
        return result;
    };
    let Ok(tree) = parser::parse_source(&old.source, lang) else {
        return result;
    };
    let old_root = tree.root_node();
    if old_root.has_error() {
        return result;
    }
    let Ok(old_syms) = symbols::extract_symbols(old_root, &old.source, lang) else {
        return result;
    };
    let old_functions = root_functions(old_root, &old.source);
    let old_jsx_pragma = has_jsx_pragma(old_root, &old.source);
    for (name, new) in candidates {
        let Some(nodes) = old_functions.get(&name) else {
            continue;
        };
        if nodes.len() != 1 || old_syms.iter().filter(|symbol| symbol.name == name).count() != 1 {
            continue;
        }
        let Some(old) = contract(nodes[0], &old.source, old_jsx_pragma) else {
            continue;
        };
        if old.header != new.header || old.inferred_returns != new.inferred_returns {
            continue;
        }
        let mut changed = HashSet::new();
        let mut valid = true;
        for key in old.props.keys().chain(new.props.keys()) {
            let before = old.props.get(key);
            let after = new.props.get(key);
            if before
                .zip(after)
                .is_some_and(|(before, after)| before.tokens == after.tokens)
            {
                continue;
            }
            if before.is_none_or(|prop| !prop.optional) || after.is_none_or(|prop| !prop.optional) {
                valid = false;
                break;
            }
            changed.insert(key.clone());
        }
        if valid && !changed.is_empty() {
            result.insert(name, OptionalPropChange { changed });
        }
    }
    result
}

/// AST の寿命を越えて保持するのは属性名と children の有無だけ。
pub(super) fn parse_is_clean(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        node = parent;
    }
    !node.has_error()
}

pub(super) fn caller_shape(node: Node<'_>, source: &[u8], parse_clean: bool) -> Option<JsxShape> {
    if !parse_clean {
        return None;
    }
    let open = jsx::jsx_name_opening_tag(node, source)?;
    let tag = open.child_by_field_name("name")?;
    if tag.kind() != "identifier" || open.child_by_field_name("type_arguments").is_some() {
        return None;
    }
    name(tag, source)?;
    let mut supplied = HashSet::new();
    let mut cursor = open.walk();
    for attribute in open
        .named_children(&mut cursor)
        .filter(|child| child.id() != tag.id() && !child.is_extra())
    {
        if attribute.kind() != "jsx_attribute" {
            return None;
        }
        let key = attribute.named_child(0)?;
        if key.kind() != "property_identifier" || !supplied.insert(name(key, source)?) {
            return None;
        }
    }
    let children = if open.kind() == "jsx_opening_element" {
        let element = open.parent()?;
        if element.kind() != "jsx_element" {
            return None;
        }
        let close = element.child_by_field_name("close_tag")?;
        let close_name = close.child_by_field_name("name")?;
        if close.kind() != "jsx_closing_element"
            || close_name.kind() != "identifier"
            || close_name.utf8_text(source).ok()? != tag.utf8_text(source).ok()?
        {
            return None;
        }
        open.end_byte() < close.start_byte()
    } else {
        false
    };
    Some(JsxShape { supplied, children })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(before: &str, after: &str, lang: LangId) -> Option<OptionalPropChange> {
        let tree = parser::parse_source(after.as_bytes(), lang).unwrap();
        let syms = symbols::extract_symbols(tree.root_node(), after.as_bytes(), lang).unwrap();
        let file_diff = format!(
            "diff --git a/panel.tsx b/panel.tsx\n--- a/panel.tsx\n+++ b/panel.tsx\n@@ -1,{} +1,{} @@\n{}{}",
            before.lines().count(),
            after.lines().count(),
            before
                .lines()
                .map(|line| format!("-{line}\n"))
                .collect::<String>(),
            after
                .lines()
                .map(|line| format!("+{line}\n"))
                .collect::<String>()
        );
        collect(
            &file_diff,
            "panel.tsx",
            tree.root_node(),
            after.as_bytes(),
            lang,
            &syms,
            &[AffectedSymbol {
                name: "ControlPanel".into(),
                kind: "function".into(),
                change_type: "modified".into(),
            }],
        )
        .remove("ControlPanel")
    }

    #[test]
    fn jsx_props_source_proof_requires_closed_default_free_root_contracts() {
        let old = "{ options }: { options?: { key: string } }";
        let new = "{ options }: { options?: { key: string; category: string } }";
        let function = |param: &str, tail: &str| {
            format!("export function ControlPanel({param}){tail} {{ return ''; }}\n")
        };
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (before, after, expected) in [
                (function(old, ""), function(new, ""), true),
                (
                    function("props: { options?: { key: string } }", ": string"),
                    function(
                        "props: { options?: { key: string; category: string } }",
                        ": string",
                    ),
                    true,
                ),
                (
                    function(&old.replace("options?:", "label: string; options?:"), ""),
                    function(&new.replace("options?:", "label: string; options?:"), ""),
                    true,
                ),
                (
                    function(
                        &old.replace("options?:", "config: { id: string }; options?:"),
                        "",
                    ),
                    function(
                        &new.replace("options?:", "config: { id: string }; options?:"),
                        "",
                    ),
                    true,
                ),
                (function(old, ": string"), function(new, ": string"), true),
                (
                    function(old, ""),
                    function(&new.replace("options?:", "options:"), ""),
                    false,
                ),
                (
                    function(
                        &old.replace("options?:", "config: { id: string }; options?:"),
                        "",
                    ),
                    function(
                        &new.replace(
                            "options?:",
                            "config: { id: string; extra: string }; options?:",
                        ),
                        "",
                    ),
                    false,
                ),
                (
                    function(&old.replace("options?:", "config: Other; options?:"), ""),
                    function(&new.replace("options?:", "config: Other; options?:"), ""),
                    false,
                ),
                (
                    function(old, ": ReactNode"),
                    function(new, ": ReactNode"),
                    false,
                ),
                (
                    function(old, ": JSX.Element"),
                    function(new, ": JSX.Element"),
                    false,
                ),
                (
                    function(old, ": typeof value"),
                    function(new, ": typeof value"),
                    false,
                ),
                (
                    function(
                        &old.replace("{ options }", "{ options = { key: 'a' } }"),
                        "",
                    ),
                    function(
                        &new.replace("{ options }", "{ options = { key: 'a' } }"),
                        "",
                    ),
                    false,
                ),
                (
                    function(
                        "props: { options?: { key: string } } = { options: { key: 'a' } }",
                        "",
                    ),
                    function(
                        "props: { options?: { key: string; category: string } } = { options: { key: 'a' } }",
                        "",
                    ),
                    false,
                ),
                (
                    function(old, ""),
                    function(&new.replace("{ options }", "{ options: opts }"), ""),
                    false,
                ),
                (
                    function(old, ""),
                    function(&format!("{new}, extra: string"), ""),
                    false,
                ),
                (
                    function(old, ""),
                    function(&new.replace("options?:", "required: string; options?:"), ""),
                    false,
                ),
                (function(old, ""), function("{}: {}", ""), false),
                (
                    function(old, ""),
                    function(&new.replace("options?:", "options?: number; options?:"), ""),
                    false,
                ),
                (
                    function(old, ""),
                    function(
                        &new.replace("options?:", "[key: string]: string; options?:"),
                        "",
                    ),
                    false,
                ),
                (
                    function(old, ""),
                    function(&new.replace("options?:", "method(): void; options?:"), ""),
                    false,
                ),
                (
                    function(old, ""),
                    function(&new.replace("options?:", "[key]?: string; options?:"), ""),
                    false,
                ),
                (
                    function(old, "").replace("ControlPanel(", "ControlPanel<T>("),
                    function(new, "").replace("ControlPanel(", "ControlPanel<T>("),
                    false,
                ),
                (
                    format!("namespace N {{ {} }}\n", function(old, "")),
                    format!("namespace N {{ {} }}\n", function(new, "")),
                    false,
                ),
                (
                    format!(
                        "{}function nested() {{ function ControlPanel() {{}} }}\n",
                        function(old, "")
                    ),
                    format!(
                        "{}function nested() {{ function ControlPanel() {{}} }}\n",
                        function(new, "")
                    ),
                    false,
                ),
            ] {
                assert_eq!(
                    evidence(&before, &after, lang).is_some(),
                    expected,
                    "{lang:?}/{before} -> {after}"
                );
            }
        }
    }

    #[test]
    fn jsx_props_overloads_keep_the_source_contract_unknown() {
        let overload = "export function ControlPanel(props: {}): string;\n";
        let before = format!(
            "{overload}export function ControlPanel({{ options }}: {{ options?: {{ key: string }} }}) {{ return ''; }}\n"
        );
        let after = before.replace("key: string", "key: string; category: string");
        for lang in [LangId::Typescript, LangId::Tsx] {
            for source in [&before, &after] {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{lang:?}/{source}");
            }
            assert!(evidence(&before, &after, lang).is_none(), "{lang:?}");
        }
    }

    fn return_evidence(before_body: &str, after_body: &str, lang: LangId) -> bool {
        let before = format!(
            "export function ControlPanel({{ options }}: {{ options?: {{ key: string }} }}) {{ {before_body}\n}}\n"
        );
        let after = format!(
            "export function ControlPanel({{ options }}: {{ options?: {{ key: string; category: string }} }}) {{ {after_body}\n}}\n"
        );
        for source in [&before, &after] {
            let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
            assert!(!tree.root_node().has_error(), "{lang:?}/{source}");
        }
        evidence(&before, &after, lang).is_some()
    }

    #[test]
    fn jsx_props_inferred_return_categories_are_closed_and_stable() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (before, after, expected) in [
                ("return '';", "return 'changed';", true),
                ("return 0;", "return (42);", true),
                ("return true;", "return ((false));", true),
                ("return null;", "return (null);", true),
                ("return 'a'; // end", "return ('b'); // end", true),
                (
                    "if (options) return null; return '';",
                    "if (options) return null; return '';",
                    true,
                ),
                (
                    "throw options; return '';",
                    "throw options; return '';",
                    true,
                ),
                ("return '';", "return 1;", false),
                ("return '';", "if (options) return null; return '';", false),
                ("if (options) return '';", "if (options) return '';", false),
                (
                    "return ''; function later() {}",
                    "return ''; function later() {}",
                    false,
                ),
                ("", "", false),
            ] {
                assert_eq!(
                    return_evidence(before, after, lang),
                    expected,
                    "{lang:?}/{before} -> {after}"
                );
            }
            for body in [
                "return options;",
                "return factory();",
                "return options ? '' : null;",
                "return options ?? null;",
                "return `literal`;",
                "return -1;",
                "return 1n;",
                "return -1n;",
                "return undefined;",
                "return;",
                "return '' as string;",
                "return '' satisfies string;",
                "return options!;",
                "return await factory();",
                "return ('', null);",
            ] {
                assert!(!return_evidence(body, body, lang), "{lang:?}/{body}");
            }
        }
        for (before, after) in [
            ("return <span />;", "return ((<span />));"),
            ("return <span />;", "return (<><span /></>);"),
            (
                "if (!options) return null; return <span />;",
                "if (!options) return null; return (<><span /></>);",
            ),
            (
                "return <span />;",
                "useEffect(() => { return () => cleanup(options); }); return <span />;",
            ),
        ] {
            assert!(
                return_evidence(before, after, LangId::Tsx),
                "{before} -> {after}"
            );
        }
    }

    #[test]
    fn jsx_props_inferred_returns_skip_every_nested_function_and_class_kind() {
        for nested in [
            "function nested() { return null; }",
            "function* nested() { return null; }",
            "const nested = function() { return null; };",
            "const nested = function*() { return null; };",
            "const nested = () => { return null; };",
            "const nested = { method() { return null; } };",
            "class Nested { method() { return null; } }",
            "abstract class Nested { method() { return null; } }",
            "const Nested = class { method() { return null; } };",
        ] {
            let before = format!("{nested} return <span />;");
            let after = format!("{nested} if (!options) return null; return <span />;");
            assert!(!return_evidence(&before, &after, LangId::Tsx), "{nested}");
            assert!(
                return_evidence("return <span />;", &before, LangId::Tsx),
                "{nested}"
            );
        }
    }

    #[test]
    fn jsx_props_inferred_returns_reject_pragmas_and_invalid_roots_only_on_inferred_path() {
        let before = "export function ControlPanel({ options }: { options?: { key: string } }) { return ''; }\n";
        let after = before.replace("key: string", "key: string; category: string");
        for lang in [LangId::Typescript, LangId::Tsx] {
            for pragma in [
                "/** @jsx factory */\n",
                "/** @jsxImportSource other */\n",
                "// @jsxFrag Fragment\n",
            ] {
                for (old_prefix, new_prefix) in [(pragma, ""), ("", pragma), (pragma, pragma)] {
                    assert!(
                        evidence(
                            &format!("{old_prefix}{before}"),
                            &format!("{new_prefix}{after}"),
                            lang
                        )
                        .is_none(),
                        "{lang:?}/{pragma}"
                    );
                }
                assert!(
                    evidence(
                        &format!("{pragma}{}", before.replace(") {", "): string {")),
                        &format!("{pragma}{}", after.replace(") {", "): string {")),
                        lang
                    )
                    .is_some(),
                    "explicit/{lang:?}/{pragma}"
                );
            }
            assert!(return_evidence(
                "'@jsxImportSource text'; return '';",
                "'@jsxImportSource text'; return '';",
                lang
            ));
            for (before, after) in [
                (
                    before.replace("function ControlPanel", "async function ControlPanel"),
                    after.replace("function ControlPanel", "async function ControlPanel"),
                ),
                (
                    before.replace("function ControlPanel", "function* ControlPanel"),
                    after.replace("function ControlPanel", "function* ControlPanel"),
                ),
                (before.replace("return '';", "return ( ;"), after.clone()),
                (
                    before.to_string(),
                    after.replace("return '';", "return ( ;"),
                ),
                (
                    before
                        .replace("function ControlPanel", "const ControlPanel =")
                        .replace(") {", ") => {"),
                    after
                        .replace("function ControlPanel", "const ControlPanel =")
                        .replace(") {", ") => {"),
                ),
            ] {
                assert!(
                    evidence(&before, &after, lang).is_none(),
                    "{lang:?}/{before} -> {after}"
                );
            }
        }
    }

    fn shapes(source: &str) -> Vec<Option<JsxShape>> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Tsx).unwrap();
        let mut shapes = Vec::new();
        let _: ControlFlow<()> = walk_preorder(tree.root_node(), Children::Named, |node, _| {
            if matches!(node.kind(), "identifier" | "property_identifier")
                && node.utf8_text(source.as_bytes()).ok() == Some("ControlPanel")
            {
                shapes.push(caller_shape(
                    node,
                    source.as_bytes(),
                    !tree.root_node().has_error(),
                ));
            }
            ControlFlow::Continue(Walk::Descend)
        });
        shapes
    }

    #[test]
    fn jsx_props_caller_proof_checks_each_open_close_pair_and_attribute() {
        let change = OptionalPropChange {
            changed: HashSet::from(["options".into()]),
        };
        for (jsx, omitted, known) in [
            ("<ControlPanel />", true, true),
            ("<ControlPanel label='Name' />", true, true),
            ("<ControlPanel></ControlPanel>", true, true),
            ("<ControlPanel>text</ControlPanel>", true, true),
            ("<ControlPanel options />", false, true),
            (
                "<ControlPanel options={undefined}></ControlPanel>",
                false,
                true,
            ),
            ("<ControlPanel options={{key:'a'}} />", false, true),
            ("<ControlPanel {...props} />", false, false),
            ("<ControlPanel<number> />", false, false),
            ("<NS.ControlPanel />", false, false),
            ("<NS:ControlPanel />", false, false),
            ("<ControlPanel xml:lang='x' />", false, false),
            ("<ControlPanel label='a' label='b' />", false, false),
            ("<ControlPanel></Other>", false, false),
        ] {
            let shapes = shapes(&format!("const view = {jsx};"));
            assert!(!shapes.is_empty(), "{jsx}");
            for shape in shapes {
                assert_eq!(shape.is_some(), known, "{jsx}");
                assert_eq!(
                    shape.is_some_and(|shape| change.is_omitted_by(&shape)),
                    omitted,
                    "{jsx}"
                );
            }
        }
        let change = OptionalPropChange {
            changed: HashSet::from(["children".into(), "options".into()]),
        };
        let plain = shapes("const view = <ControlPanel />;")
            .pop()
            .unwrap()
            .unwrap();
        let children = shapes("const view = <ControlPanel>text</ControlPanel>;")
            .pop()
            .unwrap()
            .unwrap();
        assert!(change.is_omitted_by(&plain));
        assert!(!change.is_omitted_by(&children));
        assert!(
            shapes("const view = <ControlPanel />; const broken = ;")
                .into_iter()
                .all(|shape| shape.is_none())
        );
    }

    #[test]
    fn jsx_props_whitespace_between_tags_supplies_children() {
        let change = OptionalPropChange {
            changed: HashSet::from(["children".into()]),
        };
        for content in ["", " ", "\t", "\n"] {
            let source = format!("const view = <ControlPanel>{content}</ControlPanel>;");
            let refs = shapes(&source);
            assert_eq!(refs.len(), 2, "{source:?}");
            for shape in refs {
                let shape = shape.unwrap();
                assert_eq!(shape.children, !content.is_empty(), "{source:?}");
                assert_eq!(
                    change.is_omitted_by(&shape),
                    content.is_empty(),
                    "{source:?}"
                );
            }
        }
    }

    #[test]
    fn jsx_props_node_kinds_exist_in_supported_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for kind in [
                "function_declaration",
                "function_signature",
                "generator_function_declaration",
                "function_expression",
                "generator_function",
                "arrow_function",
                "method_definition",
                "class_declaration",
                "abstract_class_declaration",
                "class",
                "return_statement",
                "parenthesized_expression",
                "comment",
                "program",
                "export_statement",
                "required_parameter",
                "optional_parameter",
                "object_pattern",
                "identifier",
                "shorthand_property_identifier_pattern",
                "pair_pattern",
                "property_identifier",
                "object_type",
                "property_signature",
                "type_annotation",
                "predefined_type",
                "literal_type",
                "string",
                "number",
                "true",
                "false",
                "null",
                "undefined",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), &format!("({kind}) @node"))
                    .unwrap_or_else(|error| panic!("{lang:?}/{kind}: {error}"));
            }
        }
        for kind in [
            "jsx_opening_element",
            "jsx_self_closing_element",
            "jsx_closing_element",
            "jsx_element",
            "jsx_attribute",
            "jsx_expression",
            "jsx_text",
            "jsx_namespace_name",
            "member_expression",
            "type_arguments",
        ] {
            tree_sitter::Query::new(&LangId::Tsx.ts_language(), &format!("({kind}) @node"))
                .unwrap();
        }
    }
}
