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

/// `return` が属する関数の種類。
const FUNCTION_KINDS: &[&str] = &[
    "function_declaration",
    "function_expression",
    "generator_function_declaration",
    "generator_function",
    "arrow_function",
    "method_definition",
];

/// 関数 `function` が `object` をそのまま返すか (return の式か、矢印関数の式の本体。括弧は外す)。
/// 本体を走査し直さず、`object` から祖先へ辿って確かめる。
fn returned_by(function: Node<'_>, object: Node<'_>) -> bool {
    let mut value = object;
    while let Some(parent) = value
        .parent()
        .filter(|parent| parent.kind() == "parenthesized_expression")
    {
        value = parent;
    }
    let Some(parent) = value.parent() else {
        return false;
    };
    if parent.id() == function.id() {
        return function
            .child_by_field_name("body")
            .is_some_and(|body| body.id() == value.id());
    }
    if parent.kind() != "return_statement"
        || only_named_child(parent).is_none_or(|returned| returned.id() != value.id())
    {
        return false;
    }
    let mut current = parent.parent();
    while let Some(ancestor) = current {
        if FUNCTION_KINDS.contains(&ancestor.kind()) {
            return ancestor.id() == function.id();
        }
        current = ancestor.parent();
    }
    false
}

/// 関数がそのまま返す値 (return の式と、矢印関数の式の本体)。
fn returned_values(function: Node<'_>) -> Vec<Node<'_>> {
    match function.child_by_field_name("body") {
        Some(body) if body.kind() == "statement_block" => function_returns(body)
            .into_iter()
            .filter_map(only_named_child)
            .collect(),
        Some(body) => vec![body],
        None => Vec::new(),
    }
}

/// 省略可能な入力 `parameter` のほかの引数が、本体より先に入力を書き換えうるコードを持たないか。
/// 引数の既定値と分割代入は本体より先に評価され、メンバーの読み出しも getter を動かしうるので、
/// ほかの引数は単純な名前に限り、既定値は副作用の無い literal (primitive と空の `{}` / `[]`)
/// だけを許す。
pub(super) fn other_parameters_are_plain(
    formal: Node<'_>,
    parameter: Node<'_>,
    source: &[u8],
    name: &str,
) -> bool {
    let mut cursor = formal.walk();
    formal
        .named_children(&mut cursor)
        .filter(|other| other.kind() != "comment" && other.id() != parameter.id())
        .all(|other| {
            other.child_by_field_name("decorator").is_none()
                && other.child_by_field_name("pattern").is_some_and(|pattern| {
                    pattern.kind() == "identifier" && pattern.utf8_text(source).ok() != Some(name)
                })
                && other.child_by_field_name("value").is_none_or(|value| {
                    unwrap_parens(value).is_some_and(|value| {
                        is_primitive_literal(value)
                            || (matches!(value.kind(), "object" | "array")
                                && value
                                    .named_children(&mut value.walk())
                                    .all(|child| child.kind() == "comment"))
                    })
                })
        })
}

/// 新しい証明は呼び出し・書き込み・入力の持ち出しを含まない factory に限定する。入力は
/// `name.member ?? 既定値` で読むか、関数がそのまま返す object literal の直下で `...name` と
/// 広げる (宣言どおりの型で上書きするだけ) ものに限る。
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
        // `arguments[0]` は引数の名前を使わずに入力を指す (`for (arguments[0].tags of …)` で書き換えうる)。
        if node.kind() == "identifier" && node.utf8_text(source).ok() == Some("arguments") {
            return true;
        }
        if !matches!(node.kind(), "identifier" | "shorthand_property_identifier")
            || node.utf8_text(source).ok() != Some(name)
            || is_binding_name(node)
        {
            return false;
        }
        if node.kind() == "identifier"
            && let Some(object) = node
                .parent()
                .filter(|parent| parent.kind() == "spread_element")
                .and_then(|spread| spread.parent())
                .filter(|parent| parent.kind() == "object")
            && body
                .parent()
                .is_some_and(|function| returned_by(function, object))
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
    returned_values(function).into_iter().any(|value| {
        let Some(object) = unwrap_parens(value).filter(|value| value.kind() == "object") else {
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
        (function.kind() == "function_declaration" && is_top_level_declaration(function)
            || top_level_const_arrow(function).is_some())
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
            // 矢印関数の式の本体は、その値を必ず返す。
            if function.child_by_field_name("type_parameters").is_some()
                || function.child_by_field_name("body").is_none_or(|body| {
                    body.kind() == "statement_block" && !return_producers::definitely_exits(body, 0)
                })
                || !self.record_consumer(parameter, parsed, path, Some(properties))
            {
                return false;
            }
        }
        true
    }

    /// object literal のただ 1 つの spread が、この object literal をそのまま返す factory の
    /// 組み込み `Partial<Target>` の引数 (呼び出し側の確認を登録済み) を広げるなら、その spread。
    /// 部分の spread は追加したプロパティを宣言どおりの型 (`Target[K]`) でしか上書きしないので、
    /// 追加したプロパティを明示の値で持てば、値が spread の前でも後ろでも証明は壊れない。
    pub(super) fn partial_spread<'t>(
        &self,
        object: Node<'t>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> Option<Node<'t>> {
        if !matches!(self.delta.mode, MemberChangeMode::Addition) {
            return None;
        }
        let source: &[u8] = &parsed.source;
        let mut cursor = object.walk();
        let spreads: Vec<Node<'t>> = object
            .named_children(&mut cursor)
            .filter(|member| member.kind() == "spread_element")
            .collect();
        let [spread] = spreads.as_slice() else {
            return None;
        };
        let value = only_named_child(*spread).filter(|value| value.kind() == "identifier")?;
        let name = value.utf8_text(source).ok()?;
        let (function, ScopeBinding::Other) =
            resolve_binding_scope(value, source, name, Namespace::Value)?
        else {
            return None;
        };
        if !returned_by(function, object) {
            return None;
        }
        let parameter = parameter_named(function, source, name)?;
        let ty = parameter
            .child_by_field_name("type")
            .and_then(only_named_child)?;
        if !self.partial_annotation_is_target(ty, parsed, path, caches) {
            return None;
        }
        let parameters = function.child_by_field_name("parameters")?;
        let mut cursor = parameters.walk();
        let index = parameters
            .named_children(&mut cursor)
            .filter(|p| p.kind() != "comment")
            .position(|p| p.id() == parameter.id())?;
        self.consumers
            .iter()
            .any(|consumer| {
                consumer.path == path
                    && consumer.function_start == function.start_byte()
                    && consumer.function_end == function.end_byte()
                    && consumer.index == index
                    && consumer.optional_members.is_some()
            })
            .then_some(*spread)
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
            // `__proto__: 値` は入力のプロトタイプを決め、`overrides.tags ?? …` に継承した値を
            // 読ませうる。
            if !seen.insert(name.clone()) || name == "__proto__" {
                return false;
            }
            // 値の出どころまで追い、型注釈や `any` で隠した値 (`undefined` を含む) を通さない。
            // 値の中の名前は表記のまま解決するので、escape で綴った束縛の影も見落とさない。
            if let Some(expected) = properties.get(&name)
                && !member.child_by_field_name("value").is_some_and(|value| {
                    !(parsed.has_escaped_identifier()
                        && any_node(value, Children::Named, |node| {
                            matches!(node.kind(), "identifier" | "shorthand_property_identifier")
                        }))
                        && self
                            .value_type(value, parsed, 0)
                            .is_some_and(|actual| assignable(&actual, expected))
                        && self.narrowed_value_sources(value, parsed, 0, &mut HashMap::new())
                })
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
                builtin_names: BTreeSet::new(),
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
                array_literal_builtins: OnceCell::new(),
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
