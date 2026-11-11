//! 旧新 AST の宣言を対応付け、意味や互換性を判定せず構文の差だけを記録する。

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::language::{LangId, normalize_identifier};
use crate::models::change_evidence::{
    ChangeAnalysis, ChangeEvidence, ChangeUnknownReason, ChangedPart,
};
use crate::models::symbol::{Symbol, SymbolKind};

use super::tree_walk::{Children, Walk, walk_preorder};
use super::{parser, symbols};

type Parts = BTreeMap<ChangedPart, Vec<(String, String)>>;
const MAX_EVIDENCE_SOURCE_BYTES: usize = 1024 * 1024;

pub(crate) struct ChangeEvidenceIndex {
    entries: BTreeMap<(String, String), ChangeEvidence>,
    fallback: ChangeUnknownReason,
}

impl ChangeEvidenceIndex {
    pub(crate) fn unavailable() -> Self {
        Self {
            entries: BTreeMap::new(),
            fallback: ChangeUnknownReason::SourceUnavailable,
        }
    }

    pub(crate) fn from_diff(
        file_diff: &str,
        path: &str,
        new_root: Node<'_>,
        new: &[u8],
        lang: LangId,
    ) -> Self {
        if !supported(lang) {
            return Self {
                fallback: ChangeUnknownReason::UnsupportedLanguage,
                ..Self::unavailable()
            };
        }
        if new.len() > MAX_EVIDENCE_SOURCE_BYTES {
            return Self {
                fallback: ChangeUnknownReason::ResourceLimit,
                ..Self::unavailable()
            };
        }
        super::diff::reconstruct_old_source(file_diff, path, new)
            .map(|old| Self::with_new_tree(&old.source, new_root, new, lang))
            .unwrap_or_else(Self::unavailable)
    }

    pub(crate) fn from_sources(old: &[u8], new: &[u8], lang: LangId) -> Self {
        if old.len() > MAX_EVIDENCE_SOURCE_BYTES || new.len() > MAX_EVIDENCE_SOURCE_BYTES {
            return Self {
                fallback: ChangeUnknownReason::ResourceLimit,
                ..Self::unavailable()
            };
        }
        if !supported(lang) {
            return Self {
                fallback: ChangeUnknownReason::UnsupportedLanguage,
                ..Self::unavailable()
            };
        }
        let Ok(new_tree) = parser::parse_source(new, lang) else {
            return Self {
                fallback: ChangeUnknownReason::ParseError,
                ..Self::unavailable()
            };
        };
        Self::with_new_tree(old, new_tree.root_node(), new, lang)
    }

    pub(crate) fn with_new_tree(old: &[u8], new_root: Node<'_>, new: &[u8], lang: LangId) -> Self {
        if old.len() > MAX_EVIDENCE_SOURCE_BYTES || new.len() > MAX_EVIDENCE_SOURCE_BYTES {
            return Self {
                fallback: ChangeUnknownReason::ResourceLimit,
                ..Self::unavailable()
            };
        }
        if !supported(lang) {
            return Self {
                fallback: ChangeUnknownReason::UnsupportedLanguage,
                ..Self::unavailable()
            };
        }
        let Ok(old_tree) = parser::parse_source(old, lang) else {
            return Self {
                fallback: ChangeUnknownReason::ParseError,
                ..Self::unavailable()
            };
        };
        let old_root = old_tree.root_node();
        if old_root.has_error()
            || new_root.has_error()
            || std::str::from_utf8(old).is_err()
            || std::str::from_utf8(new).is_err()
        {
            return Self {
                fallback: ChangeUnknownReason::ParseError,
                ..Self::unavailable()
            };
        }
        let old_parts = declarations(old_root, old, lang);
        let new_parts = declarations(new_root, new, lang);
        let mut entries = BTreeMap::new();
        for (key, current) in new_parts {
            let evidence = match (old_parts.get(&key), current.as_slice()) {
                (Some(previous), [Some(new)]) if previous.len() == 1 => match &previous[0] {
                    Some(old) => compare(old, new),
                    None => ChangeEvidence::unknown(ChangeUnknownReason::UnsupportedSyntax),
                },
                (Some(previous), [_]) if previous.len() == 1 => {
                    ChangeEvidence::unknown(ChangeUnknownReason::UnsupportedSyntax)
                }
                _ => ChangeEvidence::unknown(ChangeUnknownReason::DeclarationPairingAmbiguous),
            };
            entries.insert(key, evidence);
        }
        Self {
            entries,
            fallback: ChangeUnknownReason::DeclarationPairingAmbiguous,
        }
    }

    pub(crate) fn get(&self, name: &str, kind: &str, lang: LangId) -> ChangeEvidence {
        let name = normalize_identifier(lang, name).into_owned();
        self.entries
            .get(&(name.clone(), kind.to_owned()))
            // API 抽出は JS/TS の const を constant、symbols は variable と呼ぶ。
            .or_else(|| {
                if kind == "constant" {
                    self.entries.get(&(name, "variable".to_owned()))
                } else {
                    None
                }
            })
            .cloned()
            .unwrap_or_else(|| ChangeEvidence::unknown(self.fallback))
    }
}

fn supported(lang: LangId) -> bool {
    match lang {
        LangId::Python | LangId::Javascript | LangId::Typescript | LangId::Tsx | LangId::Rust => {
            true
        }
        LangId::C
        | LangId::Cpp
        | LangId::Go
        | LangId::Php
        | LangId::Java
        | LangId::Kotlin
        | LangId::Swift
        | LangId::CSharp
        | LangId::Bash
        | LangId::Ruby
        | LangId::Zig
        | LangId::Xojo => false,
    }
}

fn declarations(
    root: Node<'_>,
    source: &[u8],
    lang: LangId,
) -> BTreeMap<(String, String), Vec<Option<Parts>>> {
    let mut result: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for symbol in
        symbols::extract_symbols_without_complexity(root, source, lang).unwrap_or_default()
    {
        let Some(kind) = kind_name(symbol.kind) else {
            continue;
        };
        let parts = partition(root, source, &symbol, lang);
        let name = normalize_identifier(lang, &symbol.name).into_owned();
        result
            .entry((name.clone(), kind.to_owned()))
            .or_default()
            .push(parts.clone());
        if let Some(container) = &symbol.container {
            result
                .entry((format!("{container}.{name}"), kind.to_owned()))
                .or_default()
                .push(parts);
        }
    }
    result
}

fn kind_name(kind: SymbolKind) -> Option<&'static str> {
    match kind {
        SymbolKind::Function => Some("function"),
        SymbolKind::Method => Some("method"),
        SymbolKind::Variable => Some("variable"),
        SymbolKind::Constant => Some("constant"),
        SymbolKind::Field => Some("field"),
        _ => None,
    }
}

fn declaration_kinds(lang: LangId) -> &'static [&'static str] {
    match lang {
        LangId::Python => &["function_definition", "assignment"],
        LangId::Javascript | LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "generator_function_declaration",
            "variable_declarator",
        ],
        LangId::Rust => &[
            "function_item",
            "function_signature_item",
            "const_item",
            "static_item",
        ],
        LangId::C
        | LangId::Cpp
        | LangId::Go
        | LangId::Php
        | LangId::Java
        | LangId::Kotlin
        | LangId::Swift
        | LangId::CSharp
        | LangId::Bash
        | LangId::Ruby
        | LangId::Zig
        | LangId::Xojo => &[],
    }
}

fn partition(root: Node<'_>, source: &[u8], symbol: &Symbol, lang: LangId) -> Option<Parts> {
    let mut declaration = root.descendant_for_point_range(
        tree_sitter::Point {
            row: symbol.range.start.line,
            column: symbol.range.start.column,
        },
        tree_sitter::Point {
            row: symbol.range.end.line,
            column: symbol.range.end.column,
        },
    )?;
    if declaration.kind() == "decorated_definition" {
        declaration = declaration.child_by_field_name("definition")?;
    }
    if !declaration_kinds(lang).contains(&declaration.kind()) {
        return None;
    }
    let mut span = declaration;
    if let Some(parent) = declaration.parent() {
        if matches!(
            parent.kind(),
            "decorated_definition" | "export_statement" | "expression_statement"
        ) {
            span = parent;
        } else if matches!(
            parent.kind(),
            "lexical_declaration" | "variable_declaration"
        ) {
            let mut cursor = parent.walk();
            if parent
                .named_children(&mut cursor)
                .filter(|node| node.kind() == "variable_declarator")
                .count()
                != 1
            {
                return None;
            }
            span = parent;
            if let Some(export) = parent
                .parent()
                .filter(|node| node.kind() == "export_statement")
            {
                span = export;
            }
        }
    }
    if span.parent().is_none_or(|parent| parent.id() != root.id()) {
        return None;
    }
    let is_function = matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method);
    let function = if is_function && declaration.kind() == "variable_declarator" {
        let value = declaration.child_by_field_name("value")?;
        if !matches!(
            value.kind(),
            "arrow_function" | "function_expression" | "generator_function"
        ) {
            return None;
        }
        value
    } else {
        declaration
    };
    let mut ranges = Vec::new();
    let mut parameter_slots = Vec::new();
    if is_function {
        if let Some(body) = function.child_by_field_name("body") {
            ranges.push((body.start_byte(), body.end_byte(), ChangedPart::Body));
        }
        let parameters = function
            .child_by_field_name("parameters")
            .or_else(|| function.child_by_field_name("parameter"))?;
        ranges.push((
            parameters.start_byte(),
            parameters.end_byte(),
            ChangedPart::Params,
        ));
        let mut cursor = parameters.walk();
        for (slot, parameter) in parameters
            .named_children(&mut cursor)
            .filter(|node| !node.is_extra())
            .enumerate()
        {
            if let Some(ty) = parameter.child_by_field_name("type") {
                ranges.push(annotation_range(ty, ChangedPart::ParamAnnotation));
                parameter_slots.push((ChangedPart::ParamAnnotation, slot));
            }
            if let Some(value) = parameter.child_by_field_name("value") {
                ranges.push(annotation_range(value, ChangedPart::DefaultValue));
                parameter_slots.push((ChangedPart::DefaultValue, slot));
            }
        }
        if let Some(ty) = function.child_by_field_name("return_type") {
            ranges.push(annotation_range(ty, ChangedPart::ReturnAnnotation));
        }
    } else {
        if let Some(value) = declaration
            .child_by_field_name("value")
            .or_else(|| declaration.child_by_field_name("right"))
        {
            ranges.push((
                value.start_byte(),
                value.end_byte(),
                ChangedPart::Initializer,
            ));
        }
        if let Some(ty) = declaration.child_by_field_name("type") {
            ranges.push(annotation_range(ty, ChangedPart::ReferencedType));
        }
    }
    let mut cursor = span.walk();
    for child in span.named_children(&mut cursor) {
        if child.kind() == "decorator" {
            ranges.push((child.start_byte(), child.end_byte(), ChangedPart::Decorator));
        }
    }
    let mut cursor = declaration.walk();
    for child in declaration.named_children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            ranges.push((
                child.start_byte(),
                child.end_byte(),
                ChangedPart::Visibility,
            ));
        }
    }
    let mut parts = Parts::new();
    for (part, slot) in parameter_slots {
        parts
            .entry(part)
            .or_default()
            .push(("parameter_slot".to_owned(), slot.to_string()));
    }
    append_tokens(span, source, &ranges, &mut parts);
    // Rust の属性は宣言の兄弟。直前の属性も比較し、注釈だけという誤った証明を防ぐ。
    if lang == LangId::Rust {
        let mut previous = declaration.prev_named_sibling();
        while let Some(node) = previous {
            if node.kind() == "attribute_item" {
                let mut tokens = Parts::new();
                append_tokens(
                    node,
                    source,
                    &[(node.start_byte(), node.end_byte(), ChangedPart::Decorator)],
                    &mut tokens,
                );
                let mut attributes: Vec<_> = tokens.into_values().flatten().collect();
                attributes.append(parts.entry(ChangedPart::Decorator).or_default());
                parts.insert(ChangedPart::Decorator, attributes);
            } else if !node.is_extra() {
                break;
            }
            previous = node.prev_named_sibling();
        }
    }
    Some(parts)
}

fn annotation_range(node: Node<'_>, part: ChangedPart) -> (usize, usize, ChangedPart) {
    let mut previous = node.prev_sibling();
    while previous.is_some_and(|node| node.is_extra()) {
        previous = previous.and_then(|node| node.prev_sibling());
    }
    let start = previous
        .filter(|previous| matches!(previous.kind(), ":" | "->" | "="))
        .map_or(node.start_byte(), |previous| previous.start_byte());
    (start, node.end_byte(), part)
}

fn append_tokens(
    root: Node<'_>,
    source: &[u8],
    ranges: &[(usize, usize, ChangedPart)],
    parts: &mut Parts,
) {
    let mut ordered_ranges = ranges.to_vec();
    ordered_ranges
        .sort_unstable_by_key(|&(start, end, part)| (start, std::cmp::Reverse(end), part));
    let mut next_range = 0;
    let mut active_ranges = Vec::new();
    let _: ControlFlow<()> = walk_preorder(root, Children::All, |node, _| {
        if node.is_extra() {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.child_count() == 0 {
            while let Some(&(start, end, part)) = ordered_ranges.get(next_range)
                && start <= node.start_byte()
            {
                active_ranges.push((start, end, part));
                next_range += 1;
            }
            active_ranges.retain(|&(_, end, _)| end >= node.end_byte());
            let part = active_ranges
                .iter()
                .filter(|(start, end, _)| *start <= node.start_byte() && node.end_byte() <= *end)
                .min_by_key(|(start, end, _)| end - start)
                .map_or_else(
                    || match node.kind() {
                        "export"
                        | "default"
                        | "visibility_modifier"
                        | "public"
                        | "private"
                        | "protected" => ChangedPart::Visibility,
                        _ => ChangedPart::OtherDeclaration,
                    },
                    |(_, _, part)| *part,
                );
            if let Ok(text) = node.utf8_text(source) {
                parts
                    .entry(part)
                    .or_default()
                    .push((node.kind().to_owned(), text.to_owned()));
            }
        }
        ControlFlow::Continue(Walk::Descend)
    });
}

fn compare(old: &Parts, new: &Parts) -> ChangeEvidence {
    let changed_parts: Vec<_> = old
        .keys()
        .chain(new.keys())
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|part| old.get(part) != new.get(part))
        .collect();
    let annotation_only = !changed_parts.is_empty()
        && changed_parts.iter().all(|part| {
            matches!(
                part,
                ChangedPart::ReturnAnnotation
                    | ChangedPart::ParamAnnotation
                    | ChangedPart::ReferencedType
            )
        });
    let call_shape_unchanged = if !old.contains_key(&ChangedPart::Params)
        || !new.contains_key(&ChangedPart::Params)
        || old.contains_key(&ChangedPart::Decorator)
        || new.contains_key(&ChangedPart::Decorator)
        || changed_parts.contains(&ChangedPart::OtherDeclaration)
    {
        None
    } else {
        Some(
            !changed_parts
                .iter()
                .any(|part| matches!(part, ChangedPart::Params | ChangedPart::DefaultValue)),
        )
    };
    ChangeEvidence {
        analysis: ChangeAnalysis::Known,
        changed_parts,
        annotation_only: Some(annotation_only),
        call_shape_unchanged,
        unknown_reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(lang: LangId, old: &str, new: &str, name: &str, kind: &str) -> ChangeEvidence {
        ChangeEvidenceIndex::from_sources(old.as_bytes(), new.as_bytes(), lang)
            .get(name, kind, lang)
    }

    #[test]
    fn evidence_python_return_annotation_and_mixed_body() {
        let old = "def enabled(value):\n    return bool(value)\n";
        let annotation = "def enabled(value) -> bool:\n    return bool(value)\n";
        let proof = evidence(LangId::Python, old, annotation, "enabled", "function");
        assert_eq!(proof.changed_parts, [ChangedPart::ReturnAnnotation]);
        assert_eq!(proof.annotation_only, Some(true));
        assert_eq!(proof.call_shape_unchanged, Some(true));
        let mixed = annotation.replace("bool(value)", "not value");
        let proof = evidence(LangId::Python, old, &mixed, "enabled", "function");
        assert_eq!(
            proof.changed_parts,
            [ChangedPart::ReturnAnnotation, ChangedPart::Body]
        );
        assert_eq!(proof.annotation_only, Some(false));
    }

    #[test]
    fn evidence_python_params_defaults_and_decorator_are_distinct() {
        let old = "def enabled(value):\n    return bool(value)\n";
        let typed = "def enabled(value: int):\n    return bool(value)\n";
        let proof = evidence(LangId::Python, old, typed, "enabled", "function");
        assert_eq!(proof.changed_parts, [ChangedPart::ParamAnnotation]);
        assert_eq!(proof.annotation_only, Some(true));
        let defaulted = "def enabled(value: int = 1):\n    return bool(value)\n";
        let proof = evidence(LangId::Python, typed, defaulted, "enabled", "function");
        assert_eq!(proof.changed_parts, [ChangedPart::DefaultValue]);
        assert_eq!(proof.call_shape_unchanged, Some(false));
        let decorated = format!("@wrapper\n{typed}");
        let proof = evidence(LangId::Python, old, &decorated, "enabled", "function");
        assert_eq!(
            proof.changed_parts,
            [ChangedPart::ParamAnnotation, ChangedPart::Decorator]
        );
        assert_eq!(proof.annotation_only, Some(false));
        assert_eq!(proof.call_shape_unchanged, None);
        let renamed = typed.replace("value", "input");
        assert!(
            evidence(LangId::Python, typed, &renamed, "enabled", "function")
                .changed_parts
                .contains(&ChangedPart::Params)
        );
        let proof = evidence(
            LangId::Python,
            "def enabled(x: int, y):\n    return x\n",
            "def enabled(x, y: int):\n    return x\n",
            "enabled",
            "function",
        );
        assert_eq!(proof.changed_parts, [ChangedPart::ParamAnnotation]);
        assert_eq!(proof.annotation_only, Some(true));
    }

    #[test]
    fn evidence_body_initializer_and_visibility() {
        let rust = "pub fn enabled(value: u32) -> bool { value > 0 }\n";
        let proof = evidence(
            LangId::Rust,
            rust,
            &rust.replace("u32", "u64"),
            "enabled",
            "function",
        );
        assert_eq!(proof.changed_parts, [ChangedPart::ParamAnnotation]);
        assert_eq!(proof.annotation_only, Some(true));
        assert_eq!(proof.call_shape_unchanged, Some(true));
        let old = "export function enabled(value: number): boolean { return value > 0; }\n";
        let new = old.replace("value > 0", "value >= 0");
        let proof = evidence(LangId::Typescript, old, &new, "enabled", "function");
        assert_eq!(proof.changed_parts, [ChangedPart::Body]);
        assert_eq!(proof.annotation_only, Some(false));
        assert_eq!(proof.call_shape_unchanged, Some(true));
        let old = "pub const FLAG: bool = true;\n";
        let new = "const FLAG: bool = false;\n";
        let proof = evidence(LangId::Rust, old, new, "FLAG", "constant");
        assert_eq!(
            proof.changed_parts,
            [ChangedPart::Visibility, ChangedPart::Initializer]
        );
        assert_eq!(proof.call_shape_unchanged, None);
        let old = "export const FLAG: boolean = true;\n";
        let proof = evidence(
            LangId::Typescript,
            old,
            &old.replace("true", "false"),
            "FLAG",
            "constant",
        );
        assert_eq!(proof.changed_parts, [ChangedPart::Initializer]);
        let old = "export function enabled(value) { return value > 0; }\n";
        let proof = evidence(
            LangId::Javascript,
            old,
            &old.replace("> 0", ">= 0"),
            "enabled",
            "function",
        );
        assert_eq!(proof.changed_parts, [ChangedPart::Body]);
    }

    #[test]
    fn evidence_unknown_is_explicit_and_attributes_do_not_prove_call_shape() {
        let old = "def enabled(value):\n    return value\n";
        let broken = "def enabled(value -> bool:\n    return value\n";
        let proof = evidence(LangId::Python, old, broken, "enabled", "function");
        assert_eq!(proof.analysis, ChangeAnalysis::Unknown);
        assert_eq!(proof.unknown_reason, Some(ChangeUnknownReason::ParseError));
        let json = serde_json::to_value(proof).unwrap();
        assert!(json.as_object().unwrap().contains_key("annotation_only"));
        assert!(
            json.as_object()
                .unwrap()
                .contains_key("call_shape_unchanged")
        );
        assert_eq!(json["annotation_only"], serde_json::Value::Null);
        assert_eq!(json["call_shape_unchanged"], serde_json::Value::Null);
        let rust = "#[inline]\npub fn enabled(value: u32) -> bool { value > 0 }\n";
        let proof = evidence(
            LangId::Rust,
            rust,
            &rust.replace("bool", "u8"),
            "enabled",
            "function",
        );
        assert_eq!(proof.changed_parts, [ChangedPart::ReturnAnnotation]);
        assert_eq!(proof.call_shape_unchanged, None);
        let duplicate = format!("{old}\n{old}");
        let proof = evidence(
            LangId::Python,
            &duplicate,
            &duplicate,
            "enabled",
            "function",
        );
        assert_eq!(
            proof.unknown_reason,
            Some(ChangeUnknownReason::DeclarationPairingAmbiguous)
        );
        let proof = evidence(
            LangId::Go,
            "package x\nfunc enabled() {}",
            "package x\nfunc enabled() { return }",
            "enabled",
            "function",
        );
        assert_eq!(
            proof.unknown_reason,
            Some(ChangeUnknownReason::UnsupportedLanguage)
        );
    }

    #[test]
    fn evidence_node_kinds_exist_in_supported_grammars() {
        for &lang in LangId::ALL_TREE_SITTER {
            let grammar = lang.ts_language();
            for &kind in declaration_kinds(lang) {
                assert!(
                    (0..grammar.node_kind_count())
                        .any(|index| grammar.node_kind_for_id(index as u16) == Some(kind)),
                    "{lang}: {kind}"
                );
            }
        }
    }

    #[test]
    fn evidence_limits_and_literal_contents_are_preserved() {
        let oversized = vec![b' '; MAX_EVIDENCE_SOURCE_BYTES + 1];
        let proof = ChangeEvidenceIndex::from_sources(&oversized, b"", LangId::Python).get(
            "enabled",
            "function",
            LangId::Python,
        );
        assert_eq!(
            proof.unknown_reason,
            Some(ChangeUnknownReason::ResourceLimit)
        );
        assert_eq!(proof.annotation_only, None);
        let old = "def enabled(value):\n    return 'a b'\n";
        let formatted = "def enabled( value ):\n    # comment\n    return 'a b'\n";
        let proof = evidence(LangId::Python, old, formatted, "enabled", "function");
        assert!(proof.changed_parts.is_empty(), "{proof:?}");
        assert_eq!(proof.annotation_only, Some(false));
        let proof = evidence(
            LangId::Python,
            old,
            &old.replace("a b", "a  b"),
            "enabled",
            "function",
        );
        assert_eq!(proof.changed_parts, [ChangedPart::Body]);
    }
}
