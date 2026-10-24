//! 配列・Promise の戻り値を持つ producer の限定的な閉包証明 (Issue #138)。

use super::*;

struct WrappedReturn<'t> {
    target: Node<'t>,
    array: bool,
    promise: bool,
    builtins: Vec<Node<'t>>,
}

/// 裸の型以外を許すのは関数宣言の戻り値だけ。変数・引数・callback の型引数は広げない。
pub(super) fn wrapped_return_owner(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_ARRAY_DEPTH {
        let parent = node.parent()?;
        match parent.kind() {
            "array_type" | "readonly_type" | "generic_type" | "type_arguments" => node = parent,
            "type_annotation" => {
                let function = parent.parent()?;
                return (function.kind() == "function_declaration"
                    && function.child_by_field_name("return_type")?.id() == parent.id())
                .then_some(function);
            }
            _ => return None,
        }
    }
    None
}

fn wrapped_return<'t>(function: Node<'t>, source: &[u8], name: &str) -> Option<WrappedReturn<'t>> {
    let mut ty = only_named_child(function.child_by_field_name("return_type")?)?;
    let mut builtins = Vec::new();
    let promise = generic_argument(ty, source, "Promise").is_some();
    if promise {
        builtins.push(ty.child_by_field_name("name")?);
        ty = generic_argument(ty, source, "Promise")?;
    }
    if ty.kind() == "readonly_type" {
        ty = only_named_child(ty)?;
        if ty.kind() != "array_type" {
            return None;
        }
    }
    let array = match ty.kind() {
        "array_type" => {
            ty = only_named_child(ty)?;
            true
        }
        "generic_type" => {
            builtins.push(ty.child_by_field_name("name")?);
            ty = generic_argument(ty, source, "Array")?;
            true
        }
        _ => false,
    };
    (ty.kind() == "type_identifier"
        && ty.utf8_text(source).ok() == Some(name)
        && (array || promise))
        .then_some(WrappedReturn {
            target: ty,
            array,
            promise,
            builtins,
        })
}

fn generic_argument<'t>(ty: Node<'t>, source: &[u8], name: &str) -> Option<Node<'t>> {
    (ty.kind() == "generic_type"
        && ty.child_by_field_name("name")?.kind() == "type_identifier"
        && ty.child_by_field_name("name")?.utf8_text(source).ok() == Some(name))
    .then_some(())?;
    only_named_child(ty.child_by_field_name("type_arguments")?)
}

impl Prover<'_, '_> {
    pub(super) fn prove_wrapped_return(
        &mut self,
        function: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let source: &[u8] = &parsed.source;
        let Some(shape) = wrapped_return(function, source, self.scope.type_name) else {
            return false;
        };
        if !is_top_level_declaration(function)
            || has_token(function, "*")
            || has_token(function, "async") != shape.promise
            || function.child_by_field_name("type_parameters").is_some()
            || !self.binds_target_type(shape.target, parsed, path, caches)
            || shape.builtins.iter().any(|node| {
                resolve_binding_scope(
                    *node,
                    source,
                    node.utf8_text(source).unwrap_or_default(),
                    Namespace::Type,
                )
                .is_some()
            })
        {
            return false;
        }
        // 宣言の追加・global augmentation があれば、組み込み Array / Promise とみなさない。
        let mut names = BTreeSet::new();
        if shape.array {
            names.extend(["Array", "ReadonlyArray"]);
        }
        if shape.promise {
            names.insert("Promise");
        }
        ensure_name_refs(self.scope.dir, &names, caches);
        if !self.builtin_types_are_plain(&names, caches) {
            return false;
        }
        let Some(body) = function.child_by_field_name("body") else {
            return false;
        };
        if !definitely_exits(body, 0) {
            return false;
        }
        let returns = function_returns(body);
        if returns.is_empty()
            || !returns.into_iter().all(|ret| {
                only_named_child(ret).is_some_and(|value| {
                    if shape.array {
                        self.prove_array_map(value, parsed, path, diff_files, caches)
                    } else {
                        self.prove_value(value, parsed, path, diff_files, caches)
                    }
                })
            })
            || !self.register_producer(function, parsed)
        {
            return false;
        }
        self.sites += 1;
        true
    }

    fn builtin_types_are_plain(
        &self,
        names: &BTreeSet<&str>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        for name in names {
            let Some(refs) = name_refs(caches, name) else {
                return false;
            };
            for r in refs.iter() {
                if r.kind == Some(RefKind::Definition) {
                    return false;
                }
                let Some(parsed) = parsed_ref_file_shared(self.scope.dir, &r.path, caches) else {
                    return false;
                };
                if parsed.tree.root_node().has_error() {
                    return false;
                }
                let Some(node) = ref_name_node(&parsed, name, r.line, r.column) else {
                    return false;
                };
                if node.kind() == "type_identifier" {
                    continue;
                }
                // prototype の変更や別名への持ち出しを許さない。通常の static call だけ許す。
                let Some(member) = node.parent().filter(|p| p.kind() == "member_expression") else {
                    return false;
                };
                if member.child_by_field_name("object").map(|n| n.id()) != Some(node.id())
                    || member
                        .child_by_field_name("property")
                        .and_then(|n| n.utf8_text(&parsed.source).ok())
                        == Some("prototype")
                    || member.parent().is_none_or(|call| {
                        call.kind() != "call_expression"
                            || call.child_by_field_name("function").map(|n| n.id())
                                != Some(member.id())
                    })
                {
                    return false;
                }
            }
        }
        true
    }

    fn prove_array_map(
        &self,
        value: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let Some(call) = unwrap_parens(value) else {
            return false;
        };
        let Some((receiver, callback)) = map_parts(call, &parsed.source) else {
            return false;
        };
        if self.map_receiver_element(receiver, parsed).is_none() {
            return false;
        }
        let Some(body) = callback.child_by_field_name("body") else {
            return false;
        };
        if body.kind() != "statement_block" {
            return self.prove_value(body, parsed, path, diff_files, caches);
        }
        let returns = function_returns(body);
        definitely_exits(body, 0)
            && !returns.is_empty()
            && returns.into_iter().all(|ret| {
                only_named_child(ret)
                    .is_some_and(|expr| self.prove_value(expr, parsed, path, diff_files, caches))
            })
    }

    /// 注釈のある配列引数か配列 literal だけ。任意オブジェクトの同名 map を証拠にしない。
    fn map_receiver_element(
        &self,
        receiver: Node<'_>,
        parsed: &ParsedRefFile,
    ) -> Option<Vec<TypeAtom>> {
        let source: &[u8] = &parsed.source;
        let receiver = unwrap_parens(receiver)?;
        if receiver.kind() == "array" {
            let ty = self.array_type(receiver, parsed, 0)?;
            let [TypeAtom::Array(elements)] = ty.as_slice() else {
                return None;
            };
            return Some(elements.iter().map(widen_element).collect());
        }
        if receiver.kind() != "identifier" {
            return None;
        }
        let name = receiver.utf8_text(source).ok()?;
        let (scope, ScopeBinding::Other) =
            resolve_binding_scope(receiver, source, name, Namespace::Value)?
        else {
            return None;
        };
        let param = parameter_named(scope, source, name)?;
        if param.kind() != "required_parameter" || param.child_by_field_name("value").is_some() {
            return None;
        }
        let mut ty = only_named_child(param.child_by_field_name("type")?)?;
        if ty.kind() == "readonly_type" {
            ty = only_named_child(ty)?;
        }
        let element = if ty.kind() == "array_type" {
            only_named_child(ty)?
        } else {
            let argument = generic_argument(ty, source, "Array")?;
            let builtin = ty.child_by_field_name("name")?;
            if resolve_binding_scope(builtin, source, "Array", Namespace::Type).is_some() {
                return None;
            }
            argument
        };
        let elements = simple_type(element, source)?;
        if elements
            .iter()
            .any(|ty| matches!(ty, TypeAtom::Any | TypeAtom::Unknown))
            || !array_parameter_uses_are_plain(scope, param, name, source)
        {
            return None;
        }
        Some(elements)
    }

    pub(super) fn map_parameter_type(
        &self,
        scope: Node<'_>,
        name: &str,
        parsed: &ParsedRefFile,
    ) -> Option<Vec<TypeAtom>> {
        if scope.kind() != "arrow_function" {
            return None;
        }
        let arguments = scope.parent().filter(|n| n.kind() == "arguments")?;
        let call = arguments.parent()?;
        let (receiver, callback) = map_parts(call, &parsed.source)?;
        if callback.id() != scope.id()
            || callback_parameter(callback)?
                .utf8_text(&parsed.source)
                .ok()?
                != name
            || callback_rebinds(callback.child_by_field_name("body")?, &parsed.source, name)
        {
            return None;
        }
        self.map_receiver_element(receiver, parsed)
    }
}

/// 配列 literal の文脈で primitive literal は widening する。`["x"]` の引数は string。
fn widen_element(atom: &TypeAtom) -> TypeAtom {
    match atom {
        TypeAtom::StrLit(_) => TypeAtom::String,
        TypeAtom::NumLit(_) => TypeAtom::Number,
        TypeAtom::BigIntLit(_) => TypeAtom::BigInt,
        TypeAtom::True | TypeAtom::False => TypeAtom::Boolean,
        TypeAtom::Array(items) => TypeAtom::Array(items.iter().map(widen_element).collect()),
        _ => atom.clone(),
    }
}

fn callback_rebinds(body: Node<'_>, source: &[u8], name: &str) -> bool {
    any_node(body, Children::Named, |node| match node.kind() {
        "assignment_expression" | "augmented_assignment_expression" => node
            .child_by_field_name("left")
            .is_some_and(|target| assignment_target_binds(target, source, name)),
        "update_expression" => node
            .child_by_field_name("argument")
            .is_some_and(|target| assignment_target_binds(target, source, name)),
        "variable_declarator" => {
            node.parent()
                .is_some_and(|p| p.kind() == "variable_declaration")
                && node
                    .child_by_field_name("name")
                    .is_some_and(|target| pattern_binds_name(target, source, name))
        }
        "for_in_statement" => {
            node.child_by_field_name("kind")
                .is_none_or(|kind| kind.kind() == "var")
                && node
                    .child_by_field_name("left")
                    .is_some_and(|target| assignment_target_binds(target, source, name))
        }
        _ => false,
    })
}

fn map_parts<'t>(call: Node<'t>, source: &[u8]) -> Option<(Node<'t>, Node<'t>)> {
    if call.kind() != "call_expression"
        || call.child_by_field_name("optional_chain").is_some()
        || call.child_by_field_name("type_arguments").is_some()
    {
        return None;
    }
    let member = call.child_by_field_name("function")?;
    if member.kind() != "member_expression"
        || member.child_by_field_name("optional_chain").is_some()
        || member.child_by_field_name("property")?.kind() != "property_identifier"
        || member
            .child_by_field_name("property")?
            .utf8_text(source)
            .ok()
            != Some("map")
    {
        return None;
    }
    let callback = only_named_child(call.child_by_field_name("arguments")?)?;
    if callback.kind() != "arrow_function"
        || has_token(callback, "async")
        || callback.child_by_field_name("type_parameters").is_some()
        || callback.child_by_field_name("return_type").is_some()
    {
        return None;
    }
    callback_parameter(callback)?;
    Some((member.child_by_field_name("object")?, callback))
}

fn callback_parameter(callback: Node<'_>) -> Option<Node<'_>> {
    if let Some(param) = callback.child_by_field_name("parameter") {
        return (param.kind() == "identifier").then_some(param);
    }
    let param = only_named_child(callback.child_by_field_name("parameters")?)?;
    (param.kind() == "required_parameter"
        && param.child_by_field_name("value").is_none()
        && param.child_by_field_name("type").is_none())
    .then_some(())?;
    param
        .child_by_field_name("pattern")
        .filter(|n| n.kind() == "identifier")
}

/// 配列引数を別名・呼び出しへ渡したり、map 自体を書き換える使い方を証明に混ぜない。
fn array_parameter_uses_are_plain(
    scope: Node<'_>,
    param: Node<'_>,
    name: &str,
    source: &[u8],
) -> bool {
    !any_node(scope, Children::Named, |node| {
        if node.kind() != "identifier" || node.utf8_text(source).ok() != Some(name) {
            return false;
        }
        if node.start_byte() >= param.start_byte() && node.end_byte() <= param.end_byte() {
            return false;
        }
        let Some(member) = node.parent().filter(|p| p.kind() == "member_expression") else {
            return true;
        };
        if member.child_by_field_name("object").map(|n| n.id()) != Some(node.id()) {
            return true;
        }
        match member
            .child_by_field_name("property")
            .and_then(|p| p.utf8_text(source).ok())
        {
            Some("map") => member.parent().is_none_or(|call| {
                call.kind() != "call_expression"
                    || call.child_by_field_name("function").map(|n| n.id()) != Some(member.id())
            }),
            Some("length") => member.parent().is_some_and(|parent| {
                matches!(
                    parent.kind(),
                    "assignment_expression"
                        | "augmented_assignment_expression"
                        | "update_expression"
                        | "unary_expression"
                )
            }),
            _ => true,
        }
    })
}

/// 対応する終了形を限定し、分岐の片側だけに return がある本体は閉じない。
fn definitely_exits(node: Node<'_>, depth: usize) -> bool {
    if depth >= MAX_UNWRAP {
        return false;
    }
    match node.kind() {
        "return_statement" | "throw_statement" => true,
        "statement_block" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|child| child.kind() != "comment")
                .last()
                .is_some_and(|last| definitely_exits(last, depth + 1))
        }
        "if_statement" => {
            node.child_by_field_name("consequence")
                .is_some_and(|n| definitely_exits(n, depth + 1))
                && node.child_by_field_name("alternative").is_some_and(|n| {
                    only_named_child(n).is_some_and(|n| definitely_exits(n, depth + 1))
                })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_node_kinds_exist_in_typescript_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for kind in [
                "readonly_type",
                "array_type",
                "generic_type",
                "type_arguments",
                "arrow_function",
                "member_expression",
                "if_statement",
                "else_clause",
            ] {
                assert!(
                    tree_sitter::Query::new(&lang.ts_language(), &format!("({kind}) @node"))
                        .is_ok(),
                    "{lang}: {kind}"
                );
            }
        }
    }
}
