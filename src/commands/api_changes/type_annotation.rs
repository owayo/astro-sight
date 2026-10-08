//! 宣言子直下の型注釈だけの変更を、互換性未確認のポリシーとして扱う。

use tree_sitter::Node;

use super::js_const::unique_top_level_const;
use super::signature_tokens::{SigTokens, node_signature_tokens, signature_tokens_in_range};
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::language::LangId;

const MAX_NODES: usize = 4096;
const MAX_DEPTH: usize = 128;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TypeAnnotationDetection {
    NotApplicable,
    Unchanged,
    AnnotationOnly,
}

#[derive(Debug, PartialEq, Eq)]
struct AstStructureNode {
    kind: String,
    field: Option<String>,
    named: bool,
    children: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct AstContract {
    tokens: SigTokens,
    structure: Vec<AstStructureNode>,
}

#[derive(Debug, PartialEq, Eq)]
struct AnnotationContract {
    prefix: SigTokens,
    binding: SigTokens,
    suffix: SigTokens,
    scaffold: Vec<AstStructureNode>,
    initializer: AstContract,
    annotation: Option<AstContract>,
    annotation_source: Option<String>,
}

pub(super) fn classify_type_annotation_change(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> TypeAnnotationDetection {
    if site.kind != "variable" || (!site.old_sig.contains(':') && !site.new_sig.contains(':')) {
        return TypeAnnotationDetection::NotApplicable;
    }
    let Some(lang) = site.lang_in(&[LangId::Typescript, LangId::Tsx]) else {
        return TypeAnnotationDetection::NotApplicable;
    };
    let Some(source) = sources.get(site) else {
        return TypeAnnotationDetection::NotApplicable;
    };
    let Some((old_tree, new_tree)) = source.parse_pair(lang) else {
        return TypeAnnotationDetection::NotApplicable;
    };
    match (
        annotation_contract(old_tree.root_node(), &source.old, site.name),
        annotation_contract(new_tree.root_node(), &source.new, site.name),
    ) {
        (Some(old), Some(new)) => classify_contracts(old, new),
        _ => TypeAnnotationDetection::NotApplicable,
    }
}

fn classify_contracts(old: AnnotationContract, new: AnnotationContract) -> TypeAnnotationDetection {
    if old.annotation_source == new.annotation_source {
        return TypeAnnotationDetection::NotApplicable;
    }
    if old.prefix != new.prefix
        || old.binding != new.binding
        || old.suffix != new.suffix
        || old.scaffold != new.scaffold
        || old.initializer != new.initializer
    {
        return TypeAnnotationDetection::NotApplicable;
    }
    if old.annotation == new.annotation {
        TypeAnnotationDetection::Unchanged
    } else {
        TypeAnnotationDetection::AnnotationOnly
    }
}

fn annotation_contract(root: Node<'_>, source: &[u8], name: &str) -> Option<AnnotationContract> {
    let (declarator, statement) = unique_top_level_const(root, source, name)?;
    let name = declarator.child_by_field_name("name")?;
    let value = declarator.child_by_field_name("value")?;
    let annotation = declarator.child_by_field_name("type");
    if annotation.is_some_and(|node| node.kind() != "type_annotation") {
        return None;
    }
    let mut previous = statement.prev_sibling();
    let mut count = 0;
    while let Some(comment) = previous.filter(|node| node.kind() == "comment") {
        count += 1;
        if count > MAX_NODES || tagged_jsdoc(comment, source)? {
            return None;
        }
        previous = comment.prev_sibling();
    }
    // 省略する部分も含めて上限と JSDoc を確認する。
    structure(statement, source, None, None)?;
    let binding = if let Some(annotation) = annotation {
        let mut tokens =
            signature_tokens_in_range(statement, source, name.end_byte(), annotation.start_byte())?;
        tokens.0.extend(
            signature_tokens_in_range(
                statement,
                source,
                annotation.end_byte(),
                value.start_byte(),
            )?
            .0,
        );
        tokens
    } else {
        signature_tokens_in_range(statement, source, name.end_byte(), value.start_byte())?
    };
    let annotation_contract = match annotation {
        Some(node) => Some(ast_contract(node, source)?),
        None => None,
    };
    let annotation_source = match annotation {
        Some(node) => Some(
            std::str::from_utf8(source.get(name.end_byte()..node.end_byte())?)
                .ok()?
                .to_string(),
        ),
        None => None,
    };
    Some(AnnotationContract {
        prefix: signature_tokens_in_range(
            statement,
            source,
            statement.start_byte(),
            name.end_byte(),
        )?,
        binding,
        suffix: signature_tokens_in_range(
            statement,
            source,
            value.end_byte(),
            statement.end_byte(),
        )?,
        scaffold: structure(statement, source, annotation, Some(value))?,
        initializer: ast_contract(value, source)?,
        annotation: annotation_contract,
        annotation_source,
    })
}

fn ast_contract(node: Node<'_>, source: &[u8]) -> Option<AstContract> {
    Some(AstContract {
        structure: structure(node, source, None, None)?,
        tokens: node_signature_tokens(node, source)?,
    })
}

fn tagged_jsdoc(node: Node<'_>, source: &[u8]) -> Option<bool> {
    if node.kind() != "comment" {
        return Some(false);
    }
    let text = node.utf8_text(source).ok()?;
    Some(text.starts_with("/**") && text.contains('@'))
}

fn structure(
    root: Node<'_>,
    source: &[u8],
    annotation: Option<Node<'_>>,
    initializer: Option<Node<'_>>,
) -> Option<Vec<AstStructureNode>> {
    let mut result = Vec::new();
    let mut stack = vec![(root, None, 0)];
    while let Some((node, field, depth)) = stack.pop() {
        if result.len() >= MAX_NODES
            || depth > MAX_DEPTH
            || node.child_count() as usize > MAX_NODES
            || node.is_error()
            || node.is_missing()
            || tagged_jsdoc(node, source)?
        {
            return None;
        }
        if initializer.is_some_and(|value| value.id() == node.id()) {
            result.push(AstStructureNode {
                kind: "<initializer>".to_string(),
                field,
                named: true,
                children: 0,
            });
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node
            .children(&mut cursor)
            .enumerate()
            .filter(|(_, child)| {
                !annotation.is_some_and(|annotation| annotation.id() == child.id())
            })
            .map(|(index, child)| {
                (
                    child,
                    node.field_name_for_child(index as u32).map(str::to_string),
                    depth + 1,
                )
            })
            .collect();
        result.push(AstStructureNode {
            kind: node.kind().to_string(),
            field,
            named: node.is_named(),
            children: children.len(),
        });
        stack.extend(children.into_iter().rev());
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;

    fn proof(source: &str, lang: LangId) -> Option<AnnotationContract> {
        let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
        annotation_contract(tree.root_node(), source.as_bytes(), "VALUE")
    }

    fn detection(old: &str, new: &str, lang: LangId) -> TypeAnnotationDetection {
        match (proof(old, lang), proof(new, lang)) {
            (Some(old), Some(new)) => classify_contracts(old, new),
            _ => TypeAnnotationDetection::NotApplicable,
        }
    }

    #[test]
    fn annotations_only_preserve_initializer_tokens_and_structure() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (old, new) in [
                (
                    "export const VALUE = { width: 100 * 2 };",
                    "export const VALUE: Dim = { width: 100 * 2 };",
                ),
                (
                    "export const VALUE: Dim = { width: 100 * 2 };",
                    "export const VALUE = { width: 100 * 2 };",
                ),
                (
                    "export const VALUE: Dim = getDim();",
                    "export const VALUE: Readonly<Dim> = getDim();",
                ),
                (
                    "export const VALUE: Fn = (x: number): number => x;",
                    "export const VALUE: OtherFn = (x: number): number => x;",
                ),
                (
                    "export const VALUE: 'a b' = input;",
                    "export const VALUE: 'a  b' = input;",
                ),
                (
                    "export const VALUE: `a b${string}` = input;",
                    "export const VALUE: `a  b${string}` = input;",
                ),
            ] {
                assert_eq!(
                    detection(old, new, lang),
                    TypeAnnotationDetection::AnnotationOnly,
                    "{lang:?}: {old} -> {new}"
                );
            }
            assert_eq!(
                detection(
                    "export const VALUE: Map<string, number> = getDim();",
                    "export const VALUE : Map< string ,\n number > = getDim();",
                    lang
                ),
                TypeAnnotationDetection::Unchanged
            );
            assert_eq!(
                detection(
                    "export const VALUE: number = 1;",
                    "export const VALUE : number = 1;",
                    lang
                ),
                TypeAnnotationDetection::Unchanged
            );
            assert_eq!(
                detection("export const VALUE = 1;", "export const VALUE = 1;", lang),
                TypeAnnotationDetection::NotApplicable
            );
        }
    }

    #[test]
    fn annotations_do_not_hide_initializer_literal_type_or_asi_changes() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (old, new) in [
                ("{ width: 100 * 2 }", "{ width: 100 * 3 }"),
                ("'a b'", "'a  b'"),
                ("`a b${input}`", "`a  b${input}`"),
                ("/a b/g", "/a  b/g"),
                ("input as Shape", "input as OtherShape"),
                ("input satisfies Shape", "input satisfies OtherShape"),
                ("(x: number): number => x", "(x: string): number => x"),
                ("(x: number): number => x", "(x: number): unknown => x"),
                (
                    "() => { return\n{ value: 1 }; }",
                    "() => { return { value: 1 }; }",
                ),
                ("input", "(input)"),
            ] {
                assert_eq!(
                    detection(
                        &format!("export const VALUE: Before = {old};"),
                        &format!("export const VALUE: After = {new};"),
                        lang
                    ),
                    TypeAnnotationDetection::NotApplicable,
                    "{lang:?}: {old} -> {new}"
                );
            }
        }
        assert_eq!(
            detection(
                "export const VALUE: Before = <span>a b</span>;",
                "export const VALUE: After = <span>a  b</span>;",
                LangId::Tsx
            ),
            TypeAnnotationDetection::NotApplicable
        );
        let old = "export const VALUE: Before = () => { return\n{ value: 1 }; };";
        let new = "export const VALUE: After = () => { return { value: 1 }; };";
        let old = proof(old, LangId::Typescript).unwrap();
        let new = proof(new, LangId::Typescript).unwrap();
        assert_eq!(old.initializer.tokens, new.initializer.tokens);
        assert_ne!(old.initializer.structure, new.initializer.structure);
    }

    #[test]
    fn annotation_proof_rejects_uncertain_bindings_and_other_header_changes() {
        let old = "export const VALUE: Dim = getDim();";
        for new in [
            "export let VALUE: Other = getDim();",
            "const VALUE: Other = getDim();",
            "export const VALUE: Other = getDim(), extra = 1;",
            "export const { VALUE }: Other = getDim();",
            "export declare const VALUE: Other;",
            "export const VALUE: Other = getDim(); const VALUE = 1;",
            "export const VALUE: Other = getDim(); type VALUE = number;",
            "export const VALUE: Other = getDim(); function broken( {",
            "/** @type {Other} */ export const VALUE: Other = getDim();",
            "export const VALUE: Other = /** @type {Other} */ getDim();",
        ] {
            assert_eq!(
                detection(old, new, LangId::Typescript),
                TypeAnnotationDetection::NotApplicable,
                "{new}"
            );
        }
        let source = format!("export const VALUE: Dim = [{}];", "1,".repeat(MAX_NODES));
        assert!(proof(&source, LangId::Typescript).is_none());
    }

    #[test]
    fn annotation_node_contract_matches_the_ts_and_tsx_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            tree_sitter::Query::new(
                &lang.ts_language(),
                "(variable_declarator type: (type_annotation))",
            )
            .unwrap();
        }
        let source = include_str!("../../../tests/fixtures/type_annotation_contract.tsx");
        let tree = parser::parse_source(source.as_bytes(), LangId::Tsx).unwrap();
        for name in ["AREA", "LITERALS", "HANDLER", "ASI"] {
            assert!(
                annotation_contract(tree.root_node(), source.as_bytes(), name).is_some(),
                "{name}"
            );
        }
    }
}
