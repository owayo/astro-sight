//! 非 scalar の残存プロパティは、旧新の値と周辺の束縛が不変な場合だけ証明する。

use super::*;
use crate::commands::api_changes::signature_tokens::{
    SigTokens, node_signature_tokens, signature_tokens_in_range,
};

const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_NODES: usize = 100_000;

pub(super) type ObjectRange = (usize, usize);

pub(crate) fn property_builtin_names(
    annotation: Node<'_>,
    source: &[u8],
) -> BTreeSet<&'static str> {
    let mut names = BTreeSet::new();
    any_node(annotation, Children::Named, |node| {
        if node.kind() == "generic_type" {
            match node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
            {
                Some("Array") => {
                    names.insert("Array");
                }
                Some("ReadonlyArray") => {
                    names.insert("ReadonlyArray");
                }
                Some("Record") => {
                    names.insert("Record");
                }
                _ => {}
            }
        }
        false
    });
    names
}

pub(crate) fn supported_property_type(annotation: Node<'_>, source: &[u8]) -> bool {
    only_named_child(annotation).is_some_and(|ty| supported_type(ty, source, 0))
}

fn supported_type(ty: Node<'_>, source: &[u8], depth: usize) -> bool {
    if depth >= MAX_ARRAY_DEPTH {
        return false;
    }
    if let Some(atoms) = simple_type(ty, source) {
        return plain_atoms(&atoms);
    }
    match ty.kind() {
        "readonly_type" => only_named_child(ty)
            .filter(|inner| inner.kind() == "array_type")
            .is_some_and(|inner| supported_type(inner, source, depth + 1)),
        "generic_type" => {
            let Some(name) = ty
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
            else {
                return false;
            };
            let Some(arguments) = ty.child_by_field_name("type_arguments") else {
                return false;
            };
            let mut cursor = arguments.walk();
            let args = arguments
                .named_children(&mut cursor)
                .filter(|n| n.kind() != "comment")
                .collect::<Vec<_>>();
            match (name, args.as_slice()) {
                ("Array" | "ReadonlyArray", [item]) => {
                    simple_type(*item, source).is_some_and(|atoms| plain_atoms(&atoms))
                }
                ("Record", [key, value]) => {
                    key.kind() == "predefined_type"
                        && key.utf8_text(source).ok() == Some("string")
                        && simple_type(*value, source).is_some_and(|atoms| {
                            atoms.iter().all(|atom| !matches!(atom, TypeAtom::Array(_)))
                                && plain_atoms(&atoms)
                        })
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn plain_atoms(atoms: &[TypeAtom]) -> bool {
    atoms.iter().all(|atom| match atom {
        TypeAtom::Array(items) => plain_atoms(items),
        TypeAtom::Any | TypeAtom::Unknown | TypeAtom::StrLit(None) => false,
        _ => true,
    })
}

impl Prover<'_, '_> {
    pub(super) fn prove_invariant_object(
        &self,
        object: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        properties: &[RetainedProperty],
        caches: &mut ApiClosureCaches,
    ) -> bool {
        if !*self
            .invariant_builtins
            .get_or_init(|| self.invariant_builtins_are_plain(caches))
        {
            return false;
        }
        if !self.invariant_files.borrow().contains_key(path) {
            let objects = self.invariant_file_objects(parsed, path, properties);
            self.invariant_files
                .borrow_mut()
                .insert(path.to_string(), objects);
        }
        self.invariant_files
            .borrow()
            .get(path)
            .and_then(Option::as_ref)
            .is_some_and(|objects| objects.contains(&(object.start_byte(), object.end_byte())))
    }

    fn invariant_builtins_are_plain(&self, caches: &mut ApiClosureCaches) -> bool {
        let MemberChangeMode::Removal(removed) = &self.delta.mode else {
            return false;
        };
        ensure_name_refs(self.scope.dir, &removed.builtin_names, caches);
        for name in &removed.builtin_names {
            let Some(refs) = name_refs(caches, name) else {
                return false;
            };
            for r in refs.iter() {
                if r.kind == Some(RefKind::Definition)
                    || !matches!(
                        ref_import_role(self.scope.dir, r, name, caches),
                        Some(RefImportRole::Outside)
                    )
                {
                    return false;
                }
                let Some(file) = parsed_ref_file_shared(self.scope.dir, &r.path, caches) else {
                    return false;
                };
                let Some(node) = ref_name_node(&file, name, r.line, r.column) else {
                    return false;
                };
                if file.tree.root_node().has_error()
                    || !matches!(node.kind(), "type_identifier" | "identifier")
                    || resolve_binding_scope(node, &file.source, name, Namespace::Type).is_some()
                    || node
                        .parent()
                        .is_some_and(|parent| parent.kind() == "export_specifier")
                    || (node.kind() == "identifier"
                        && (is_binding_name(node)
                            || binds_value_to_another_name(node, &file.source, false)
                            || builtin_value_is_written(node)))
                {
                    return false;
                }
            }
        }
        true
    }

    fn invariant_file_objects(
        &self,
        parsed: &ParsedRefFile,
        path: &str,
        properties: &[RetainedProperty],
    ) -> Option<Vec<ObjectRange>> {
        if !matches!(parsed.lang_id, LangId::Typescript | LangId::Tsx)
            || parsed.source.len() > MAX_FILE_BYTES
            || !crate::engine::impact::is_safe_diff_path(path)
        {
            return None;
        }
        let new = file_signature(
            parsed.tree.root_node(),
            &parsed.source,
            self.scope,
            path,
            properties,
            self.delta.removed_names(),
            false,
        )?;
        let old =
            super::super::super::git_input::git_show_blob(self.scope.dir, self.scope.base, path)
                .filter(|source| source.len() <= MAX_FILE_BYTES)?;
        let tree = crate::engine::parser::parse_source(&old, parsed.lang_id).ok()?;
        let old = file_signature(
            tree.root_node(),
            &old,
            self.scope,
            path,
            properties,
            self.delta.removed_names(),
            true,
        )?;
        (old.tokens == new.tokens).then_some(new.objects)
    }
}

fn builtin_value_is_written(node: Node<'_>) -> bool {
    let mut current = node;
    for _ in 0..MAX_UNWRAP {
        let Some(parent) = current.parent() else {
            return false;
        };
        let target = match parent.kind() {
            "assignment_expression" | "augmented_assignment_expression" => {
                parent.child_by_field_name("left")
            }
            "update_expression" => parent.child_by_field_name("argument"),
            "unary_expression" if has_token(parent, "delete") => {
                parent.child_by_field_name("argument")
            }
            "for_in_statement" => parent.child_by_field_name("left"),
            _ => None,
        };
        if target.is_some_and(|target| {
            target.start_byte() <= node.start_byte() && node.end_byte() <= target.end_byte()
        }) {
            return true;
        }
        if matches!(
            parent.kind(),
            "member_expression" | "subscript_expression" | "parenthesized_expression"
        ) {
            current = parent;
        } else {
            return false;
        }
    }
    true
}

struct FileSignature {
    tokens: SigTokens,
    objects: Vec<ObjectRange>,
}

struct Replacement<'t> {
    node: Node<'t>,
    tokens: SigTokens,
}

fn file_signature(
    root: Node<'_>,
    source: &[u8],
    scope: &ClosureScope<'_>,
    path: &str,
    properties: &[RetainedProperty],
    removed: &[String],
    old: bool,
) -> Option<FileSignature> {
    let mut count = 0;
    if root.has_error()
        || any_node(root, Children::All, |n| {
            count += 1;
            count > MAX_NODES || n.is_missing()
        })
    {
        return None;
    }
    let mut replacements = Vec::new();
    let mut objects = Vec::new();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        let declaration = if statement.kind() == "export_statement" {
            statement.child_by_field_name("declaration")
        } else {
            Some(statement)
        };
        let Some(declaration) = declaration else {
            continue;
        };
        if scope.target_path == path
            && matches!(
                declaration.kind(),
                "interface_declaration" | "type_alias_declaration"
            )
            && super::super::node_field_name_eq(declaration, scope.type_name, source)
        {
            replacements.push(Replacement {
                node: declaration,
                tokens: SigTokens(Vec::new()),
            });
            continue;
        }
        if declaration.kind() == "function_declaration"
            && declaration
                .child_by_field_name("return_type")
                .is_some_and(|annotation| annotation_is_name(annotation, scope.type_name, source))
        {
            let name = declaration
                .child_by_field_name("name")?
                .utf8_text(source)
                .ok()?;
            if super::super::ts_const_arg::collect_bindings_named(root, source, name).len() != 1
                || declaration.child_by_field_name("type_parameters").is_some()
                || has_token(declaration, "async")
                || has_token(declaration, "*")
            {
                return None;
            }
            for ret in function_returns(declaration.child_by_field_name("body")?) {
                let Some(object) = only_named_child(ret)
                    .and_then(unwrap_parens)
                    .filter(|n| n.kind() == "object")
                else {
                    continue;
                };
                replacements.push(Replacement {
                    node: object,
                    tokens: object_signature(object, source, properties, removed, old, 0)?,
                });
                objects.push((object.start_byte(), object.end_byte()));
            }
        } else if declaration.kind() == "lexical_declaration" && has_token(declaration, "const") {
            let mut cursor = declaration.walk();
            for declarator in declaration
                .named_children(&mut cursor)
                .filter(|n| n.kind() == "variable_declarator")
            {
                if !declarator
                    .child_by_field_name("type")
                    .is_some_and(|ty| annotation_is_name(ty, scope.type_name, source))
                {
                    continue;
                }
                let name = declarator.child_by_field_name("name")?;
                if name.kind() != "identifier"
                    || super::super::ts_const_arg::collect_bindings_named(
                        root,
                        source,
                        name.utf8_text(source).ok()?,
                    )
                    .len()
                        != 1
                {
                    return None;
                }
                let Some(object) = declarator
                    .child_by_field_name("value")
                    .and_then(unwrap_parens)
                    .filter(|n| n.kind() == "object")
                else {
                    continue;
                };
                replacements.push(Replacement {
                    node: object,
                    tokens: object_signature(object, source, properties, removed, old, 0)?,
                });
                objects.push((object.start_byte(), object.end_byte()));
            }
        }
    }
    replacements.sort_by_key(|replacement| replacement.node.start_byte());
    let mut tokens = Vec::new();
    let mut end = root.start_byte();
    for replacement in replacements {
        tokens
            .extend(signature_tokens_in_range(root, source, end, replacement.node.start_byte())?.0);
        tokens.extend(replacement.tokens.0);
        end = replacement.node.end_byte();
    }
    tokens.extend(signature_tokens_in_range(root, source, end, root.end_byte())?.0);
    tokens.retain(|(kind, _)| kind != "comment");
    Some(FileSignature {
        tokens: SigTokens(tokens),
        objects,
    })
}

fn annotation_is_name(annotation: Node<'_>, name: &str, source: &[u8]) -> bool {
    only_named_child(annotation)
        .is_some_and(|ty| ty.kind() == "type_identifier" && ty.utf8_text(source).ok() == Some(name))
}

fn object_signature(
    object: Node<'_>,
    source: &[u8],
    properties: &[RetainedProperty],
    removed: &[String],
    old: bool,
    depth: usize,
) -> Option<SigTokens> {
    if depth >= MAX_ARRAY_DEPTH {
        return None;
    }
    let mut seen = HashSet::new();
    let mut tokens = vec![("{".into(), "{".into())];
    let mut cursor = object.walk();
    for member in object.named_children(&mut cursor) {
        if member.kind() == "comment" {
            continue;
        }
        let (key, value) = match member.kind() {
            "pair" => (
                member.child_by_field_name("key")?,
                member.child_by_field_name("value")?,
            ),
            "shorthand_property_identifier" => (member, member),
            _ => return None,
        };
        if key.kind() == "computed_property_name" || key.utf8_text(source).ok()?.contains('\\') {
            return None;
        }
        let name = super::super::object_key_text(key, source)?;
        if !seen.insert(name.clone()) {
            return None;
        }
        if removed.contains(&name) {
            if old {
                continue;
            }
            return None;
        }
        if !properties.iter().any(|property| property.name == name)
            || !plain_value(value, source, depth + 1)
        {
            return None;
        }
        tokens.extend(node_signature_tokens(member, source)?.0);
        tokens.push((",".into(), ",".into()));
    }
    if properties
        .iter()
        .any(|property| property.required && !seen.contains(&property.name))
    {
        return None;
    }
    tokens.push(("}".into(), "}".into()));
    Some(SigTokens(tokens))
}

fn plain_value(value: Node<'_>, source: &[u8], depth: usize) -> bool {
    if depth >= MAX_ARRAY_DEPTH {
        return false;
    }
    match value.kind() {
        "string" | "number" | "true" | "false" | "null" => value
            .utf8_text(source)
            .is_ok_and(|text| !text.contains('\\')),
        "array" => {
            let mut cursor = value.walk();
            value
                .named_children(&mut cursor)
                .filter(|n| n.kind() != "comment")
                .all(|child| plain_value(child, source, depth + 1))
        }
        "object" => {
            let mut seen = HashSet::new();
            let mut cursor = value.walk();
            value
                .named_children(&mut cursor)
                .filter(|n| n.kind() != "comment")
                .all(|member| {
                    let Some(key) = member.child_by_field_name("key") else {
                        return false;
                    };
                    member.kind() == "pair"
                        && key.kind() != "computed_property_name"
                        && key.utf8_text(source).is_ok_and(|text| !text.contains('\\'))
                        && super::super::object_key_text(key, source)
                            .is_some_and(|key| seen.insert(key))
                        && member
                            .child_by_field_name("value")
                            .is_some_and(|value| plain_value(value, source, depth + 1))
                })
        }
        "identifier" | "shorthand_property_identifier" => {
            let Ok(name) = value.utf8_text(source) else {
                return false;
            };
            let Some((function, ScopeBinding::Other)) =
                resolve_binding_scope(value, source, name, Namespace::Value)
            else {
                return false;
            };
            if function.kind() != "function_declaration" || !is_top_level_declaration(function) {
                return false;
            }
            let Some(parameter) = parameter_named(function, source, name) else {
                return false;
            };
            parameter.kind() == "required_parameter"
                && parameter.child_by_field_name("value").is_none()
                && parameter.child_by_field_name("type").is_some_and(|ty| {
                    supported_property_type(ty, source)
                        && property_builtin_names(ty, source).is_empty()
                })
                && function
                    .child_by_field_name("body")
                    .is_some_and(|body| !return_producers::callback_rebinds(body, source, name))
        }
        "parenthesized_expression" => {
            only_named_child(value).is_some_and(|inner| plain_value(inner, source, depth + 1))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_removal_invariant_nodes_and_shapes_match_both_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for query in [
                "(readonly_type (array_type))",
                "(generic_type name: (type_identifier) type_arguments: (type_arguments))",
                "(array)",
                "(object (pair key: (_) value: (_)))",
                "(shorthand_property_identifier)",
                "(function_declaration parameters: (formal_parameters) return_type: (type_annotation) body: (statement_block))",
                "(required_parameter pattern: (identifier) type: (type_annotation))",
                "(variable_declarator name: (identifier) type: (type_annotation) value: (_))",
                "(export_specifier)",
                "(assignment_expression left: (_) right: (_))",
                "(augmented_assignment_expression)",
                "(update_expression)",
                "(unary_expression)",
                "(for_in_statement)",
                "(member_expression)",
                "(subscript_expression)",
                "(parenthesized_expression)",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), query).unwrap();
            }
            for (annotation, expected) in [
                ("string[]", true),
                ("readonly string[]", true),
                ("Array<string>", true),
                ("ReadonlyArray<string>", true),
                ("Record<string, number>", true),
                ("Tag[]", false),
                ("Record<string, Tag>", false),
                ("any[]", false),
                ("unknown[]", false),
                ("() => string", false),
            ] {
                let source = format!("export interface Row {{ data: {annotation} }}");
                let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{source}");
                let mut actual = None;
                any_node(tree.root_node(), Children::Named, |node| {
                    if node.kind() == "property_signature" {
                        actual = node.child_by_field_name("type");
                        return true;
                    }
                    false
                });
                assert_eq!(
                    supported_property_type(actual.unwrap(), source.as_bytes()),
                    expected,
                    "{source}"
                );
                let builtin = match annotation {
                    "Array<string>" => BTreeSet::from(["Array"]),
                    "ReadonlyArray<string>" => BTreeSet::from(["ReadonlyArray"]),
                    "Record<string, number>" | "Record<string, Tag>" => BTreeSet::from(["Record"]),
                    _ => BTreeSet::new(),
                };
                assert_eq!(
                    property_builtin_names(actual.unwrap(), source.as_bytes()),
                    builtin
                );
            }
        }
    }

    #[test]
    fn property_removal_invariant_file_tokens_preserve_context_and_values() {
        let scope = ClosureScope {
            dir: ".",
            base: "HEAD",
            type_name: "Row",
            target_path: "row.ts",
        };
        let properties = vec![
            RetainedProperty {
                name: "id".into(),
                required: true,
            },
            RetainedProperty {
                name: "data".into(),
                required: true,
            },
        ];
        let removed = vec!["extra".into()];
        let old = "export interface Row { id: string; data: string[]; extra: number; }\nfunction helper() { return 1; }\nexport function toRow(id: string): Row { return { id, data: [], extra: 0 }; }\n";
        let new = old.replace(" extra: number;", "").replace(", extra: 0", "");
        for lang in [LangId::Typescript, LangId::Tsx] {
            let signature = |source: &str, old| {
                let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
                file_signature(
                    tree.root_node(),
                    source.as_bytes(),
                    &scope,
                    "row.ts",
                    &properties,
                    &removed,
                    old,
                )
                .map(|file| file.tokens)
            };
            let previous = signature(old, true).expect("old file should be supported");
            assert_eq!(signature(&new, false), Some(previous.clone()));
            for after in [
                new.replace("data: []", "data: ['changed']"),
                new.replace("id: string): Row", "id: number): Row"),
                new.replace("return {", "const id = 1; return {"),
                new.replace("data: []", "data: external"),
                new.replace("data: []", "data: makeData()"),
                new.replace("data: []", "data: [] as string[]"),
                new.replace("data: []", "data: [], extra: 0"),
                format!("{new}export const row: Row = {{ id: 'new', data: [] }};\n"),
            ] {
                assert_ne!(signature(&after, false), Some(previous.clone()), "{after}");
            }
        }
    }

    #[test]
    fn property_removal_parameter_types_do_not_rely_on_unaudited_builtin_names() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (annotation, expected) in [
                ("string[]", true),
                ("readonly string[]", true),
                ("Array<string>", false),
                ("ReadonlyArray<string>", false),
                ("Record<string, number>", false),
            ] {
                let source = format!(
                    "import type {{ Array }} from './types';\nexport function toRow(data: {annotation}): Row {{ return {{ data }}; }}\n"
                );
                let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{source}");
                let mut value = None;
                any_node(tree.root_node(), Children::Named, |node| {
                    if node.kind() == "shorthand_property_identifier" {
                        value = Some(node);
                        return true;
                    }
                    false
                });
                assert_eq!(
                    plain_value(value.unwrap(), source.as_bytes(), 0),
                    expected,
                    "{source}"
                );
            }
        }
    }
}
