//! 省略可能な入力を既定値で埋める factory の、必須メンバー追加への追随を確認する。

use super::*;

fn partial_target<'t>(ty: Node<'t>, source: &[u8]) -> Option<Node<'t>> {
    if ty.kind() != "generic_type"
        || ty.child_by_field_name("name")?.utf8_text(source).ok()? != "Partial"
    {
        return None;
    }
    let target = only_named_child(ty.child_by_field_name("type_arguments")?)?;
    (target.kind() == "type_identifier").then_some(target)
}

pub(super) fn empty_default(parameter: Node<'_>) -> bool {
    parameter
        .child_by_field_name("value")
        .and_then(unwrap_parens)
        .is_some_and(|value| {
            value.kind() == "object"
                && value
                    .named_children(&mut value.walk())
                    .all(|child| child.kind() == "comment")
        })
}

fn optional_properties(ty: Node<'_>, source: &[u8]) -> Option<BTreeMap<String, Vec<TypeAtom>>> {
    if ty.kind() != "object_type" {
        return None;
    }
    let mut properties = BTreeMap::new();
    let mut cursor = ty.walk();
    for member in ty.named_children(&mut cursor) {
        if member.kind() == "comment" {
            continue;
        }
        if member.kind() != "property_signature" || !has_token(member, "?") {
            return None;
        }
        let key = member.child_by_field_name("name")?;
        if !matches!(key.kind(), "property_identifier" | "string")
            || key.utf8_text(source).ok()?.contains('\\')
        {
            return None;
        }
        let name = super::super::object_key_text(key, source)?;
        let atoms = simple_type(
            only_named_child(member.child_by_field_name("type")?)?,
            source,
        )?;
        if atoms
            .iter()
            .any(|atom| matches!(atom, TypeAtom::Any | TypeAtom::Unknown))
            || properties.insert(name, atoms).is_some()
        {
            return None;
        }
    }
    Some(properties)
}

/// 新しい証明は呼び出し・書き込み・入力の持ち出しを含まない factory に限定する。
pub(super) fn parameter_reads_are_plain(body: Node<'_>, source: &[u8], name: &str) -> bool {
    !any_node(body, Children::Named, |node| {
        if matches!(
            node.kind(),
            "call_expression"
                | "new_expression"
                | "assignment_expression"
                | "augmented_assignment_expression"
                | "update_expression"
                | "with_statement"
                | "await_expression"
                | "yield_expression"
        ) {
            return true;
        }
        if !matches!(node.kind(), "identifier" | "shorthand_property_identifier")
            || node.utf8_text(source).ok() != Some(name)
            || is_binding_name(node)
        {
            return false;
        }
        let Some(member) = node.parent().filter(|parent| {
            parent.kind() == "member_expression"
                && parent
                    .child_by_field_name("object")
                    .is_some_and(|object| object.id() == node.id())
        }) else {
            return true;
        };
        let Some(coalesce) = member.parent() else {
            return true;
        };
        coalesce.kind() != "binary_expression"
            || !has_token(coalesce, "??")
            || coalesce
                .child_by_field_name("left")
                .is_none_or(|left| left.id() != member.id())
    })
}

fn has_added_default_read(
    function: Node<'_>,
    parameter: Node<'_>,
    source: &[u8],
    added: &[RequiredProperty],
) -> bool {
    let Some(name) = parameter
        .child_by_field_name("pattern")
        .filter(|p| p.kind() == "identifier")
        .and_then(|p| p.utf8_text(source).ok())
    else {
        return false;
    };
    let Some(body) = function.child_by_field_name("body") else {
        return false;
    };
    function_returns(body).into_iter().any(|ret| {
        let Some(object) = only_named_child(ret)
            .and_then(unwrap_parens)
            .filter(|value| value.kind() == "object")
        else {
            return false;
        };
        object.named_children(&mut object.walk()).any(|member| {
            if member.kind() != "pair"
                || !member
                    .child_by_field_name("key")
                    .and_then(|key| super::super::object_key_text(key, source))
                    .is_some_and(|key| added.iter().any(|p| p.name == key))
            {
                return false;
            }
            member
                .child_by_field_name("value")
                .and_then(unwrap_parens)
                .filter(|value| value.kind() == "binary_expression" && has_token(*value, "??"))
                .and_then(|value| value.child_by_field_name("left"))
                .filter(|left| left.kind() == "member_expression")
                .and_then(|left| left.child_by_field_name("object"))
                .is_some_and(|object| {
                    object.kind() == "identifier" && object.utf8_text(source).ok() == Some(name)
                })
        })
    })
}

impl Prover<'_, '_> {
    fn partial_annotation_is_target(
        &self,
        ty: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        if !matches!(self.delta.mode, MemberChangeMode::Addition) {
            return false;
        }
        let source: &[u8] = &parsed.source;
        let Some(target) = partial_target(ty, source) else {
            return false;
        };
        let Some(name) = ty.child_by_field_name("name") else {
            return false;
        };
        target.utf8_text(source).ok() == Some(self.scope.type_name)
            && self.binds_target_type(target, parsed, path, caches)
            && resolve_binding_scope(name, source, "Partial", Namespace::Type).is_none()
            && *self.partial_builtin.get_or_init(|| {
                let names = BTreeSet::from(["Partial"]);
                ensure_name_refs(self.scope.dir, &names, caches);
                self.builtin_types_are_plain(&names, caches)
            })
    }

    pub(super) fn partial_parameter_reference(
        &self,
        node: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let Some(ty) = node
            .parent()
            .filter(|p| p.kind() == "type_arguments")
            .and_then(|p| p.parent())
        else {
            return false;
        };
        let Some(annotation) = ty.parent().filter(|p| p.kind() == "type_annotation") else {
            return false;
        };
        let Some(parameter) = annotation.parent().filter(|p| {
            p.kind() == "required_parameter"
                && p.child_by_field_name("type")
                    .is_some_and(|a| a.id() == annotation.id())
        }) else {
            return false;
        };
        let Some(function) = parameter
            .parent()
            .filter(|p| p.kind() == "formal_parameters")
            .and_then(|p| p.parent())
        else {
            return false;
        };
        function.kind() == "function_declaration"
            && is_top_level_declaration(function)
            && empty_default(parameter)
            && function
                .child_by_field_name("return_type")
                .is_some_and(|a| self.annotation_is_target(a, parsed, path, caches))
            && self.partial_annotation_is_target(ty, parsed, path, caches)
    }

    pub(super) fn register_factory_parameters(
        &mut self,
        function: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        if !matches!(self.delta.mode, MemberChangeMode::Addition) {
            return true;
        }
        let Some(parameters) = function.child_by_field_name("parameters") else {
            return true;
        };
        let source: &[u8] = &parsed.source;
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            let Some(ty) = parameter
                .child_by_field_name("type")
                .and_then(only_named_child)
            else {
                continue;
            };
            let properties = if partial_target(ty, source)
                .is_some_and(|target| target.utf8_text(source).ok() == Some(self.scope.type_name))
            {
                if !self.partial_annotation_is_target(ty, parsed, path, caches) {
                    return false;
                }
                self.delta
                    .added
                    .iter()
                    .map(|p| (p.name.clone(), p.ty.clone()))
                    .collect()
            } else if ty.kind() == "object_type"
                && has_added_default_read(function, parameter, source, &self.delta.added)
                && any_node(ty, Children::Named, |member| {
                    member.kind() == "property_signature"
                        && member
                            .child_by_field_name("name")
                            .and_then(|key| super::super::object_key_text(key, source))
                            .is_some_and(|name| self.delta.added.iter().any(|p| p.name == name))
                })
            {
                let Some(mut properties) = optional_properties(ty, source) else {
                    return false;
                };
                properties.retain(|name, _| self.delta.added.iter().any(|p| &p.name == name));
                properties
            } else {
                continue;
            };
            if function.child_by_field_name("type_parameters").is_some()
                || function
                    .child_by_field_name("body")
                    .is_none_or(|body| !return_producers::definitely_exits(body, 0))
                || !self.record_consumer(parameter, parsed, path, Some(properties))
            {
                return false;
            }
        }
        true
    }

    pub(super) fn factory_default_type(
        &self,
        expression: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
    ) -> Option<Vec<TypeAtom>> {
        if !matches!(self.delta.mode, MemberChangeMode::Addition) {
            return None;
        }
        let value = unwrap_parens(expression)?;
        if value.kind() != "binary_expression" || !has_token(value, "??") {
            return None;
        }
        let left = value.child_by_field_name("left")?;
        if left.kind() != "member_expression" || has_token(left, "?.") {
            return None;
        }
        let object = left.child_by_field_name("object")?;
        let key = left.child_by_field_name("property")?;
        if object.kind() != "identifier" || key.kind() != "property_identifier" {
            return None;
        }
        let source: &[u8] = &parsed.source;
        let name = object.utf8_text(source).ok()?;
        let property = key.utf8_text(source).ok()?;
        let (function, ScopeBinding::Other) =
            resolve_binding_scope(object, source, name, Namespace::Value)?
        else {
            return None;
        };
        let parameter = parameter_named(function, source, name)?;
        let parameters = function.child_by_field_name("parameters")?;
        let index = parameters
            .named_children(&mut parameters.walk())
            .filter(|p| p.kind() != "comment")
            .position(|p| p.id() == parameter.id())?;
        // 引数の型・既定値・本体の読み方は登録時に確認済み。同じ束縛の証拠だけを再利用する。
        let mut atoms = self
            .consumers
            .iter()
            .find(|consumer| {
                consumer.path == path
                    && consumer.function_start == function.start_byte()
                    && consumer.function_end == function.end_byte()
                    && consumer.index == index
            })?
            .optional_members
            .as_ref()?
            .get(property)?
            .clone();
        if atoms
            .iter()
            .any(|atom| matches!(atom, TypeAtom::Any | TypeAtom::Unknown))
        {
            return None;
        }
        atoms.retain(|atom| !matches!(atom, TypeAtom::Undefined | TypeAtom::Null));
        atoms.extend(self.value_type(value.child_by_field_name("right")?, parsed, 0)?);
        atoms.sort();
        atoms.dedup();
        Some(atoms)
    }

    pub(super) fn prove_optional_factory_argument(
        &self,
        argument: Node<'_>,
        properties: &BTreeMap<String, Vec<TypeAtom>>,
        parsed: &ParsedRefFile,
    ) -> bool {
        let Some(value) = unwrap_parens(argument) else {
            return false;
        };
        if value.kind() != "object" {
            return false;
        }
        let source: &[u8] = &parsed.source;
        let mut seen = BTreeSet::new();
        let mut cursor = value.walk();
        for member in value.named_children(&mut cursor) {
            if member.kind() == "comment" {
                continue;
            }
            if member.kind() != "pair" {
                return false;
            }
            let Some(key) = member.child_by_field_name("key") else {
                return false;
            };
            if key.kind() == "computed_property_name"
                || key.utf8_text(source).is_ok_and(|key| key.contains('\\'))
            {
                return false;
            }
            let Some(name) = super::super::object_key_text(key, source) else {
                return false;
            };
            if !seen.insert(name.clone()) {
                return false;
            }
            if let Some(expected) = properties.get(&name)
                && !member
                    .child_by_field_name("value")
                    .and_then(|v| self.value_type(v, parsed, 0))
                    .is_some_and(|actual| assignable(&actual, expected))
            {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_default_reads_use_the_registered_function_identity() {
        for extension in ["ts", "tsx"] {
            let dir = tempfile::tempdir().unwrap();
            let path = format!("factory.{extension}");
            let source = "interface Info { labels: string[]; } function first(overrides: { labels?: string[] } = {}): Info { return { labels: overrides.labels ?? [] }; } function second(overrides: { labels?: number[] } = {}): Info { return { labels: overrides.labels ?? [] }; }";
            std::fs::write(dir.path().join(&path), source).unwrap();
            let mut caches = ApiClosureCaches::default();
            let parsed =
                parsed_ref_file_shared(dir.path().to_str().unwrap(), &path, &mut caches).unwrap();
            let root = parsed.tree.root_node();
            let functions: Vec<_> = root
                .named_children(&mut root.walk())
                .filter(|node| node.kind() == "function_declaration")
                .collect();
            assert_eq!(functions.len(), 2);
            assert_eq!(
                functions[0].start_position().row,
                functions[1].start_position().row
            );
            fn value(function: Node<'_>) -> Node<'_> {
                function
                    .child_by_field_name("body")
                    .unwrap()
                    .named_child(0)
                    .unwrap()
                    .named_child(0)
                    .unwrap()
                    .named_child(0)
                    .unwrap()
                    .child_by_field_name("value")
                    .unwrap()
            }
            let scope = ClosureScope {
                dir: dir.path().to_str().unwrap(),
                base: "HEAD",
                type_name: "Info",
                target_path: &path,
            };
            let delta = RequiredMemberDelta {
                added: vec![RequiredProperty {
                    name: "labels".into(),
                    ty: vec![TypeAtom::Array(vec![TypeAtom::String])],
                }],
                mode: MemberChangeMode::Addition,
            };
            let mut prover = Prover {
                scope: &scope,
                delta: &delta,
                consumers: Vec::new(),
                typeof_names: BTreeMap::new(),
                narrowed_paths: BTreeSet::new(),
                invariant_files: RefCell::new(HashMap::new()),
                invariant_builtins: OnceCell::new(),
                partial_builtin: OnceCell::new(),
                wrapped_producers: Vec::new(),
                sites: 0,
            };
            assert!(prover.register_factory_parameters(functions[0], &parsed, &path, &mut caches));
            assert_eq!(
                prover.factory_default_type(value(functions[0]), &parsed, &path),
                Some(vec![
                    TypeAtom::Array(vec![]),
                    TypeAtom::Array(vec![TypeAtom::String])
                ])
            );
            assert_eq!(
                prover.factory_default_type(value(functions[1]), &parsed, &path),
                None,
                "同じ行でも別の関数の証拠を使わない"
            );
            assert!(prover.register_factory_parameters(functions[1], &parsed, &path, &mut caches));
            assert_eq!(
                prover.factory_default_type(value(functions[1]), &parsed, &path),
                Some(vec![
                    TypeAtom::Array(vec![]),
                    TypeAtom::Array(vec![TypeAtom::Number])
                ])
            );
        }
    }
}
