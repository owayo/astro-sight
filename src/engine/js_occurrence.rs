//! シグネチャ変更の影響を受けない JS/TS の出現位置だけを証明する。
//! 束縛の由来や値の別名を推測せず、用途を許可リストで限定する。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::ControlFlow;

use tree_sitter::Node;

use super::js_scope::{Namespace, ScopeBinding, pattern_binds_name, resolve_binding_scope};
use super::tree_walk::{Children, Walk, walk_preorder};

const MAX_NODES: usize = 100_000;
const MAX_PARENTS: usize = 128;
const COMMON_IDENTIFIER_KINDS: &[&str] = &[
    "identifier",
    "private_property_identifier",
    "property_identifier",
    "shorthand_property_identifier",
    "shorthand_property_identifier_pattern",
    "statement_identifier",
];
const TS_IDENTIFIER_KINDS: &[&str] = &[
    "nested_identifier",
    "nested_type_identifier",
    "type_identifier",
];

fn identifier_kind(kind: &str) -> bool {
    COMMON_IDENTIFIER_KINDS.contains(&kind) || TS_IDENTIFIER_KINDS.contains(&kind)
}

const FUNCTION_KINDS: &[&str] = &[
    "function_declaration",
    "function_expression",
    "generator_function_declaration",
    "generator_function",
    "arrow_function",
    "method_definition",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsSignatureSite {
    NonReference,
    InsensitiveParamUse,
    Unknown,
}

/// 解析木の所有者と同じファイル単位で保持する。Node 自体は保存しない。
/// 関数内の全用途検査は (関数, 名前) ごとに一度だけ行う。
pub(crate) struct JsOccurrenceIndex {
    invalid: bool,
    strict_file: bool,
    parameter_uses: RefCell<HashMap<(usize, String), bool>>,
}

impl JsOccurrenceIndex {
    pub(crate) fn new(root: Node<'_>, source: &[u8]) -> Self {
        let mut count = 0;
        let invalid = root.has_error()
            || walk_preorder(root, Children::Named, |node, _| {
                count += 1;
                if count > MAX_NODES
                    || node.is_missing()
                    || node.kind() == "with_statement"
                    || (node.kind() == "identifier" && node.utf8_text(source).ok() == Some("eval"))
                    || (identifier_kind(node.kind())
                        && node
                            .utf8_text(source)
                            .ok()
                            .is_some_and(|text| text.contains('\\')))
                {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(Walk::Descend)
                }
            })
            .is_break();
        Self {
            invalid,
            strict_file: !invalid && file_is_strict(root, source),
            parameter_uses: RefCell::new(HashMap::new()),
        }
    }

    pub(crate) fn classify(&self, node: Node<'_>, source: &[u8], name: &str) -> JsSignatureSite {
        use JsSignatureSite::{InsensitiveParamUse, NonReference, Unknown};
        if self.invalid || node.utf8_text(source).ok() != Some(name) {
            return Unknown;
        }
        let Some(function) = enclosing_function_without_ambient(node) else {
            return if primitive_property(node, source) {
                NonReference
            } else {
                Unknown
            };
        };
        if primitive_property(node, source) {
            return NonReference;
        }
        if !self.strict_file
            || !matches!(
                node.kind(),
                "identifier" | "shorthand_property_identifier_pattern"
            )
        {
            return Unknown;
        }
        let key = (function.id(), name.to_owned());
        let cached = self.parameter_uses.borrow().get(&key).copied();
        let insensitive = cached.unwrap_or_else(|| {
            let proof = matches!(
                resolve_binding_scope(node, source, name, Namespace::Value),
                Some((scope, ScopeBinding::Other)) if scope.id() == function.id()
            ) && insensitive_parameter(function, source, name);
            self.parameter_uses.borrow_mut().insert(key, proof);
            proof
        });
        if insensitive {
            InsensitiveParamUse
        } else {
            Unknown
        }
    }
}

/// ファイル全体の strict mode を証明する。動的 import や関数内の directive は使わない。
fn file_is_strict(root: Node<'_>, source: &[u8]) -> bool {
    let mut prologue = true;
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if matches!(statement.kind(), "import_statement" | "export_statement") {
            return true;
        }
        if matches!(statement.kind(), "comment" | "hash_bang_line") || !prologue {
            continue;
        }
        if statement.kind() != "expression_statement" {
            prologue = false;
            continue;
        }
        let mut expressions = statement.walk();
        let mut expressions = statement
            .named_children(&mut expressions)
            .filter(|node| node.kind() != "comment");
        let Some(literal) = expressions.next().filter(|node| node.kind() == "string") else {
            prologue = false;
            continue;
        };
        if expressions.next().is_some() {
            prologue = false;
            continue;
        }
        if matches!(
            literal.utf8_text(source).ok(),
            Some("'use strict'" | "\"use strict\"")
        ) {
            return true;
        }
    }
    false
}

/// 最寄りの関数を見つけても、ambient の祖先があれば証明しない。
fn enclosing_function_without_ambient(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node;
    let mut nearest = None;
    for _ in 0..MAX_PARENTS {
        if matches!(
            current.kind(),
            "ambient_declaration" | "internal_module" | "module"
        ) {
            return None;
        }
        if nearest.is_none() && FUNCTION_KINDS.contains(&current.kind()) {
            nearest = Some(current);
        }
        let Some(parent) = current.parent() else {
            return nearest;
        };
        current = parent;
    }
    None
}

fn primitive_property(node: Node<'_>, source: &[u8]) -> bool {
    let Some(property) = node.parent().filter(|p| p.kind() == "property_signature") else {
        return false;
    };
    if property.child_by_field_name("name").map(|n| n.id()) != Some(node.id())
        || node.kind() != "property_identifier"
    {
        return false;
    }
    // 宣言ファイルや declare module の契約へこの証拠を適用しない。
    let mut current = property;
    for _ in 0..MAX_PARENTS {
        if matches!(
            current.kind(),
            "ambient_declaration" | "internal_module" | "module"
        ) {
            return false;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    if current.parent().is_some() {
        return false;
    }
    let Some(annotation) = property.child_by_field_name("type") else {
        return false;
    };
    let mut stack = vec![annotation];
    let mut count = 0;
    while let Some(ty) = stack.pop() {
        count += 1;
        if count > MAX_PARENTS {
            return false;
        }
        match ty.kind() {
            "type_annotation" | "union_type" => {
                let mut cursor = ty.walk();
                stack.extend(
                    ty.named_children(&mut cursor)
                        .filter(|c| c.kind() != "comment"),
                );
            }
            "predefined_type" => {
                if !matches!(
                    ty.utf8_text(source).ok(),
                    Some(
                        "boolean" | "string" | "number" | "symbol" | "undefined" | "never" | "void"
                    )
                ) {
                    return false;
                }
            }
            "literal_type" => {
                let Some(literal) = ty.named_child(0) else {
                    return false;
                };
                if !matches!(
                    literal.kind(),
                    "true"
                        | "false"
                        | "null"
                        | "undefined"
                        | "number"
                        | "string"
                        | "unary_expression"
                ) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn insensitive_parameter(function: Node<'_>, source: &[u8], name: &str) -> bool {
    let Some(binding) = plain_parameter_binding(function, source, name) else {
        return false;
    };
    // 内側の closure も検査する。同名の宣言・shadow・書き換えも許可外の用途になる。
    !walk_preorder(function, Children::Named, |node, _| {
        // 名前による用途が真偽判定だけでも、arguments は引数への別経路を持つ。
        if matches!(
            node.kind(),
            "identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
        ) && node.utf8_text(source).ok() == Some("arguments")
        {
            return ControlFlow::Break(());
        }
        if matches!(
            node.kind(),
            "identifier"
                | "type_identifier"
                | "property_identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
        ) && node.utf8_text(source).ok() == Some(name)
            && node.id() != binding.id()
            && !truthiness_use(node)
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(Walk::Descend)
        }
    })
    .is_break()
}

fn plain_parameter_binding<'t>(function: Node<'t>, source: &[u8], name: &str) -> Option<Node<'t>> {
    if function.kind() == "method_definition"
        && function
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            == Some("constructor")
    {
        return None;
    }
    let mut parameters = Vec::new();
    if let Some(list) = function.child_by_field_name("parameters") {
        let mut cursor = list.walk();
        parameters.extend(
            list.named_children(&mut cursor)
                .filter(|p| p.kind() != "comment"),
        );
    } else if let Some(single) = function.child_by_field_name("parameter") {
        parameters.push(single);
    }
    let mut found = None;
    for parameter in parameters {
        let pattern = if matches!(
            parameter.kind(),
            "required_parameter" | "optional_parameter"
        ) {
            // 型注釈と default 値を束縛 visitor へ渡さない。
            let mut cursor = parameter.walk();
            if parameter.child_by_field_name("value").is_some()
                || parameter.children(&mut cursor).any(|c| {
                    matches!(
                        c.kind(),
                        "accessibility_modifier" | "readonly" | "override_modifier" | "decorator"
                    )
                })
            {
                return None;
            }
            parameter.child_by_field_name("pattern")?
        } else {
            parameter
        };
        if !pattern_binds_name(pattern, source, name) {
            continue;
        }
        let binding = match pattern.kind() {
            "identifier" if name != "this" => pattern,
            "object_pattern" => {
                let mut cursor = pattern.walk();
                let mut matched = None;
                for child in pattern
                    .named_children(&mut cursor)
                    .filter(|p| p.kind() != "comment")
                {
                    if child.kind() != "shorthand_property_identifier_pattern" {
                        return None;
                    }
                    if child.utf8_text(source).ok() == Some(name) {
                        if matched.is_some() {
                            return None;
                        }
                        matched = Some(child);
                    }
                }
                matched?
            }
            _ => return None,
        };
        if found.is_some() {
            return None;
        }
        found = Some(binding);
    }
    found
}

/// 新旧ともこの証拠を満たすときだけ、シグネチャに依存しない用途を除外できる。
/// 値から関数への種類変更や、見えている再束縛を関数の変更と取り違えない。
pub(crate) fn unique_direct_function(root: Node<'_>, source: &[u8], name: &str) -> bool {
    if JsOccurrenceIndex::new(root, source).invalid {
        return false;
    }
    let mut function = None;
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        let declaration = if statement.kind() == "export_statement" {
            statement
                .child_by_field_name("declaration")
                .unwrap_or(statement)
        } else {
            statement
        };
        if matches!(
            declaration.kind(),
            "function_declaration" | "generator_function_declaration"
        ) && declaration
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            == Some(name)
        {
            if function.is_some() || declaration.child_by_field_name("body").is_none() {
                return false;
            }
            function = Some(declaration.id());
        }
    }
    let Some(function) = function else {
        return false;
    };
    !walk_preorder(root, Children::Named, |node, _| {
        let ambiguous = match node.kind() {
            "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "function_signature"
            | "class_declaration"
            | "abstract_class_declaration"
            | "enum_declaration"
            | "internal_module"
            | "module"
            | "interface_declaration"
            | "type_alias_declaration" => {
                node.id() != function
                    && node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        == Some(name)
            }
            "variable_declarator" => node
                .child_by_field_name("name")
                .is_some_and(|p| pattern_binds_name(p, source, name)),
            "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => {
                node.child_by_field_name("left")
                    .is_some_and(|p| pattern_binds_name(p, source, name))
            }
            "update_expression" => node
                .child_by_field_name("argument")
                .is_some_and(|p| pattern_binds_name(p, source, name)),
            "unary_expression"
                if node
                    .child_by_field_name("operator")
                    .is_some_and(|op| op.kind() == "delete") =>
            {
                node.child_by_field_name("argument")
                    .is_some_and(|p| pattern_binds_name(p, source, name))
            }
            "import_statement" => super::js_scope::import_binds_name(node, source, name),
            _ => false,
        };
        if ambiguous {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(Walk::Descend)
        }
    })
    .is_break()
}

fn truthiness_use(mut node: Node<'_>) -> bool {
    for _ in 0..MAX_PARENTS {
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind() == "parenthesized_expression" {
            node = parent;
            continue;
        }
        return match parent.kind() {
            "ternary_expression" | "if_statement" | "while_statement" | "do_statement"
            | "for_statement" => {
                parent.child_by_field_name("condition").map(|c| c.id()) == Some(node.id())
            }
            "unary_expression" => {
                parent.child_by_field_name("argument").map(|c| c.id()) == Some(node.id())
                    && parent
                        .child_by_field_name("operator")
                        .is_some_and(|op| op.kind() == "!")
            }
            _ => false,
        };
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    fn site(marked: &str, lang: LangId) -> JsSignatureSite {
        let offset = marked.find('@').unwrap();
        let source = marked.replace('@', "");
        let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
        let node = tree
            .root_node()
            .descendant_for_byte_range(offset, offset)
            .unwrap();
        JsOccurrenceIndex::new(tree.root_node(), source.as_bytes()).classify(
            node,
            source.as_bytes(),
            "isOpen",
        )
    }

    fn module_site(marked: &str, lang: LangId) -> JsSignatureSite {
        site(&format!("export {{}};\n{marked}"), lang)
    }

    #[test]
    fn only_proven_file_strictness_enables_parameter_proofs() {
        let body = "function badge(isOpen) { return !!@isOpen; }";
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for prefix in [
                "export {};\n",
                "import './days';\n",
                "import {dayLabel} from './days';\n",
                "'use strict';\n",
                "\"use strict\";\n",
                "'other'; /* comment */ 'use strict';\n",
                "#!/usr/bin/env node\n'use strict';\n",
            ] {
                assert_eq!(
                    site(&format!("{prefix}{body}"), lang),
                    JsSignatureSite::InsensitiveParamUse,
                    "{lang:?}: {prefix}"
                );
            }
            for prefix in [
                "",
                "import('./days');\n",
                "; 'use strict';\n",
                "void 0; 'use strict';\n",
                "('use strict');\n",
                "'use strict' + '';\n",
                "`use strict`;\n",
                "'use\\x20strict';\n",
                "{'use strict';}\n",
            ] {
                assert_eq!(
                    site(&format!("{prefix}{body}"), lang),
                    JsSignatureSite::Unknown,
                    "{lang:?}: {prefix}"
                );
            }
            assert_eq!(
                site(
                    "function badge(isOpen) { 'use strict'; return !!@isOpen; }",
                    lang
                ),
                JsSignatureSite::Unknown
            );
            assert_eq!(
                site(&format!("{body} 'use strict';"), lang),
                JsSignatureSite::Unknown
            );
        }
    }

    #[test]
    fn truthiness_uses_and_plain_parameter_bindings() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for source in [
                "function badge(isOpen) { return @isOpen ? 1 : 0; }",
                "function badge(isOpen) { if ((@isOpen)) return 1; }",
                "function badge(isOpen) { while (@isOpen) break; }",
                "function badge(isOpen) { do {} while (@isOpen); }",
                "function badge(isOpen) { for (; @isOpen;) break; }",
                "function badge(isOpen) { return !(@isOpen); }",
                "const badge = isOpen => !@isOpen;",
                "const badge = function(isOpen) { return !@isOpen; };",
                "function* badge(isOpen) { yield !@isOpen; }",
                "const badge = function*(isOpen) { yield !@isOpen; };",
                "const obj = { badge(isOpen) { return !@isOpen; } };",
                "function badge({@isOpen}) { return isOpen ? 1 : 0; }",
                "function badge({isOpen}) { return @isOpen ? 1 : 0; }",
            ] {
                assert_eq!(
                    module_site(source, lang),
                    JsSignatureSite::InsensitiveParamUse,
                    "{lang:?}: {source}"
                );
            }
        }
        for lang in [LangId::Typescript, LangId::Tsx] {
            for source in [
                "function badge(@isOpen?: boolean) { return !!isOpen; }",
                "function badge({@isOpen}: Props) { return !!isOpen; }",
                "function badge(isOpen: boolean) { const text = '日本語'; return @isOpen ? text : ''; }",
            ] {
                assert_eq!(
                    module_site(source, lang),
                    JsSignatureSite::InsensitiveParamUse,
                    "{source}"
                );
            }
        }
    }

    #[test]
    fn any_sensitive_use_rejects_every_occurrence_of_the_parameter() {
        for source in [
            "function badge(@isOpen) { if (isOpen) isOpen(); }",
            "function badge(@isOpen) { return () => isOpen(); }",
            "function badge(@isOpen) { return isOpen; }",
            "function badge(@isOpen) { return isOpen.member; }",
            "function badge(@isOpen) { return isOpen?.(); }",
            "function badge(@isOpen) { return new isOpen(); }",
            "function badge(@isOpen) { return isOpen`tag`; }",
            "function badge(@isOpen) { consume(isOpen); }",
            "function badge(@isOpen) { return {isOpen}; }",
            "function badge(@isOpen) { return typeof isOpen; }",
            "function badge(@isOpen) { return isOpen as boolean; }",
            "function badge(@isOpen) { return isOpen! ? 1 : 0; }",
            "function badge(@isOpen) { return isOpen || false; }",
            "function badge(@isOpen) { isOpen = false; return !isOpen; }",
            "function badge(@isOpen) { let isOpen = false; return !isOpen; }",
            "function badge(@isOpen) { function inner(isOpen) { return !isOpen; } return !isOpen; }",
            "function badge(@isOpen) { eval('isOpen()'); return !isOpen; }",
            "function badge(@isOpen) { \\u0065val('isOpen()'); return !isOpen; }",
            "function badge(@isOpen) { with (object) { } return !isOpen; }",
            "function badge(@isOpen) { return !isOpen; } const broken = ;",
            "function badge(@isOpen = fallback) { return !isOpen; }",
            "function badge(...@isOpen) { return !isOpen; }",
            "function badge({isOpen: @isOpen}) { return !isOpen; }",
            "function badge({@isOpen = fallback}) { return !isOpen; }",
            "class Badge { constructor(readonly @isOpen: boolean) {} }",
            "class Badge { 'constructor'(readonly @isOpen: boolean) { return !isOpen; } }",
            "class Badge { constructor(private @isOpen: boolean) {} }",
            "declare namespace N { function badge(@isOpen: boolean): void; }",
            "function badge(other: typeof @isOpen) { return !other; }",
        ] {
            assert_eq!(
                module_site(source, LangId::Typescript),
                JsSignatureSite::Unknown,
                "{source}"
            );
        }
    }

    #[test]
    fn arguments_can_alias_a_truthiness_parameter() {
        for source in [
            "function badge(@isOpen) { arguments[0]('mon'); return !!isOpen; }",
            "function badge({@isOpen}) { arguments[0]['isOpen']('mon'); return !!isOpen; }",
            "function badge({@isOpen}) { const inner = () => arguments[0]['isOpen']('mon'); return !!isOpen; }",
            "function badge({@isOpen}) { const alias = {arguments}; alias.arguments[0]['isOpen']('mon'); return !!isOpen; }",
        ] {
            assert_eq!(
                module_site(source, LangId::Typescript),
                JsSignatureSite::Unknown,
                "{source}"
            );
        }
    }

    #[test]
    fn reflective_arguments_access_can_alias_a_truthiness_parameter() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            let mut missed = Vec::new();
            for access in [
                ".arguments",
                "['arguments']",
                "[\"arguments\"]",
                "[`arguments`]",
                "[('arguments')]",
            ] {
                let source = format!(
                    "function badge({{@isOpen}}) {{ badge{access}[0]['isOpen']('mon'); return !!isOpen; }}"
                );
                if site(&source, lang) != JsSignatureSite::Unknown {
                    missed.push(access);
                }
            }
            assert!(missed.is_empty(), "{lang:?}: missed {missed:?}");
        }
    }

    #[test]
    fn only_primitive_noncomputed_property_signatures_are_nonreferences() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for ty in [
                "boolean",
                "string",
                "number",
                "symbol",
                "undefined",
                "never",
                "void",
                "true",
                "false",
                "null",
                "'open'",
                "1",
                "-1",
                "boolean | null | 'open'",
            ] {
                let source = format!("interface Props {{ @isOpen?: {ty}; }}");
                assert_eq!(
                    site(&source, lang),
                    JsSignatureSite::NonReference,
                    "{source}"
                );
            }
            for source in [
                "interface Props { @isOpen: any; }",
                "interface Props { @isOpen: bigint; }",
                "interface Props { @isOpen: unknown; }",
                "interface Props { @isOpen: object; }",
                "interface Props { @isOpen: Alias; }",
                "interface Props { @isOpen: typeof other; }",
                "interface Props { @isOpen: () => boolean; }",
                "interface Props { @isOpen(day: string): boolean; }",
                "interface Props { [@isOpen]: boolean; }",
                "const mock = { @isOpen: true };",
                "class Props { @isOpen: boolean; }",
                "declare module './days' { interface Props { @isOpen?: boolean; } }",
                "declare namespace N { interface Props { @isOpen?: boolean; } }",
            ] {
                assert_eq!(site(source, lang), JsSignatureSite::Unknown, "{source}");
            }
        }
    }

    #[test]
    fn escaped_shorthand_cannot_hide_parameter_escape_or_rebinding() {
        let source = "export function badge(@isOpen) { if (isOpen) {} const o = {is\\u004fpen}; return Object.values(o)[0]('mon'); }";
        assert_eq!(site(source, LangId::Javascript), JsSignatureSite::Unknown);
        let source =
            "export function isOpen(day) { return day; } ({is\\u004fpen} = {isOpen: false});";
        let tree = parser::parse_source(source.as_bytes(), LangId::Javascript).unwrap();
        assert!(!unique_direct_function(
            tree.root_node(),
            source.as_bytes(),
            "isOpen"
        ));
    }

    #[test]
    fn direct_function_proof_rejects_kind_changes_and_ambiguous_bindings() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            let prove = |source: &str| {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                unique_direct_function(tree.root_node(), source.as_bytes(), "isOpen")
            };
            assert!(prove("export function isOpen(day) { return day; }"));
            assert!(prove("function isOpen(day) { return day; }"));
            for source in [
                "export const isOpen = false;",
                "export const isOpen = day => day;",
                "export function other(day) { return day; }",
                "export function isOpen(day) { return day; } function isOpen() {}",
                "export function isOpen(day) { function isOpen() {} return day; }",
                "export function isOpen(day) { return day; } const isOpen = false;",
                "export function isOpen(day) { return day; } isOpen = false;",
                "export function isOpen(day) { return day; } ({isOpen} = values);",
                "export function isOpen(day) { return day; } isOpen++;",
                "export function isOpen(day) { return day; } eval('isOpen = false');",
                "export function isOpen(day) { return day; } const broken = ;",
            ] {
                assert!(!prove(source), "{lang:?}: {source}");
            }
            // forward と reverse のいずれも、片側が値なら証明できない。
            let function = "export function isOpen(day) { return day; }";
            let value = "export const isOpen = false;";
            assert!(!(prove(function) && prove(value)));
            assert!(!(prove(value) && prove(function)));
        }
        for source in [
            "export function isOpen(day: string): boolean; export function isOpen(day: string) { return true; }",
            "declare function isOpen(day: string): boolean;",
            "export function isOpen(day) { return day; } interface isOpen {}",
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Typescript).unwrap();
            assert!(
                !unique_direct_function(tree.root_node(), source.as_bytes(), "isOpen"),
                "{source}"
            );
        }
    }

    #[test]
    fn identifier_alias_metadata() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            let grammar = lang.ts_language();
            let kinds: std::collections::BTreeSet<_> = (0..grammar.node_kind_count())
                .filter_map(|id| {
                    grammar
                        .node_kind_is_named(id as u16)
                        .then(|| grammar.node_kind_for_id(id as u16))
                        .flatten()
                })
                .filter(|kind| kind.contains("identifier"))
                .collect();
            let expected: std::collections::BTreeSet<_> = COMMON_IDENTIFIER_KINDS
                .iter()
                .copied()
                .chain(
                    if lang == LangId::Javascript {
                        &[][..]
                    } else {
                        TS_IDENTIFIER_KINDS
                    }
                    .iter()
                    .copied(),
                )
                .collect();
            assert_eq!(kinds, expected, "{lang:?}");
            assert!(kinds.into_iter().all(identifier_kind));
        }
    }

    #[test]
    fn occurrence_node_kinds_exist_in_applicable_grammars() {
        let common = [
            "identifier",
            "property_identifier",
            "shorthand_property_identifier",
            "shorthand_property_identifier_pattern",
            "object_pattern",
            "formal_parameters",
            "comment",
            "hash_bang_line",
            "expression_statement",
            "string",
            "parenthesized_expression",
            "ternary_expression",
            "if_statement",
            "while_statement",
            "do_statement",
            "for_statement",
            "unary_expression",
            "with_statement",
            "export_statement",
            "class_declaration",
            "variable_declarator",
            "assignment_expression",
            "augmented_assignment_expression",
            "for_in_statement",
            "update_expression",
            "import_statement",
        ];
        let typescript = [
            "type_identifier",
            "property_signature",
            "type_annotation",
            "union_type",
            "predefined_type",
            "literal_type",
            "required_parameter",
            "optional_parameter",
            "accessibility_modifier",
            "override_modifier",
            "decorator",
            "ambient_declaration",
            "internal_module",
            "module",
            "function_signature",
            "abstract_class_declaration",
            "enum_declaration",
            "interface_declaration",
            "type_alias_declaration",
        ];
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for kind in common
                .iter()
                .chain(FUNCTION_KINDS)
                .chain(COMMON_IDENTIFIER_KINDS)
                .chain(if lang == LangId::Javascript {
                    &[][..]
                } else {
                    TS_IDENTIFIER_KINDS
                })
                .chain(if lang == LangId::Javascript {
                    &[][..]
                } else {
                    &typescript[..]
                })
            {
                tree_sitter::Query::new(&lang.ts_language(), &format!("({kind})"))
                    .unwrap_or_else(|error| panic!("{lang:?}: {kind}: {error}"));
            }
        }
    }
}
