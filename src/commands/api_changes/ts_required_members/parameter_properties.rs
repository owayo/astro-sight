//! 型注釈付きの必須引数のプロパティの読み出し (`source.tags`) を、宣言された型で型付けする。

use super::*;

/// 読み出すと `Object.prototype` から継承した値を返しうる名前。宣言の型だけでは値を決められない。
pub(in crate::commands::api_changes) const INHERITED_NAMES: &[&str] = &[
    "constructor",
    "toString",
    "toLocaleString",
    "valueOf",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "__proto__",
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
];

impl Prover<'_, '_> {
    /// `source.tags` の宣言された型。`source` は型注釈付きの必須引数で、注釈はインラインの
    /// object 型か、同じファイルか相対 import の先のファイル直下にある interface・object 型の
    /// 別名に限る。optional のメンバーには `undefined` を足す。
    pub(super) fn parameter_property_type(
        &self,
        member: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> Option<Vec<TypeAtom>> {
        let source: &[u8] = &parsed.source;
        // 束縛の解決は名前を表記のまま比べるので、escape で綴った束縛の影を見落とす。
        if parsed.has_escaped_identifier() {
            return None;
        }
        let member = unwrap_parens(member)?;
        if member.kind() != "member_expression"
            || has_token(member, "?.")
            || member.child_by_field_name("optional_chain").is_some()
        {
            return None;
        }
        let object = member
            .child_by_field_name("object")
            .filter(|node| node.kind() == "identifier")?;
        let key = member
            .child_by_field_name("property")
            .filter(|node| node.kind() == "property_identifier")?
            .utf8_text(source)
            .ok()?;
        if key.contains('\\') || INHERITED_NAMES.contains(&key) {
            return None;
        }
        let name = object.utf8_text(source).ok()?;
        let (scope, ScopeBinding::Other) =
            resolve_binding_scope(object, source, name, Namespace::Value)?
        else {
            return None;
        };
        if ancestor_rebinds(object, Some(scope), source, name, Namespace::Value) {
            return None;
        }
        let parameter = parameter_named(scope, source, name)?;
        if parameter.kind() != "required_parameter"
            || parameter.child_by_field_name("value").is_some()
            || parameter
                .child_by_field_name("pattern")
                .is_none_or(|pattern| pattern.kind() != "identifier")
        {
            return None;
        }
        let ty = only_named_child(parameter.child_by_field_name("type")?)?;
        match ty.kind() {
            "object_type" => declared_member_type(ty, source, key),
            "type_identifier" => {
                let type_name = ty.utf8_text(source).ok()?;
                // 注釈の位置から型の名前を解決する。型引数・ローカルの型に当たったら外側へ戻らない。
                if ancestor_rebinds(ty, None, source, type_name, Namespace::Type) {
                    return None;
                }
                match resolve_binding_scope(ty, source, type_name, Namespace::Type)? {
                    (scope, ScopeBinding::TypeDeclaration) if scope.kind() == "program" => {
                        let members = root_object_members(parsed, type_name, false)?;
                        declared_member_type(members, source, key)
                    }
                    (_, ScopeBinding::Import(statement)) => {
                        let target = self.import_target(
                            statement,
                            source,
                            path,
                            type_name,
                            ImportUse::Type,
                            caches,
                        )?;
                        let declared = parsed_ref_file_shared(self.scope.dir, &target, caches)?;
                        if declared.has_escaped_identifier() {
                            return None;
                        }
                        let members = root_object_members(&declared, type_name, true)?;
                        declared_member_type(members, &declared.source, key)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// `node` から祖先へ `until` の手前まで辿り、`name` を中で束縛し直す宣言があるか。
/// `resolve_binding_scope` はクラスの型引数と、クラス式・関数式の自分の名前を見ないので、外側の
/// 宣言への取り違えをここで止める。型の名前空間では型引数とクラスの名前を、値の名前空間では
/// クラスと関数式の名前を見る。
fn ancestor_rebinds(
    node: Node<'_>,
    until: Option<Node<'_>>,
    source: &[u8],
    name: &str,
    namespace: Namespace,
) -> bool {
    let declares = |declared: Option<Node<'_>>| {
        declared.and_then(|declared| declared.utf8_text(source).ok()) == Some(name)
    };
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if until.is_some_and(|until| until.id() == ancestor.id()) {
            return false;
        }
        let self_named = match ancestor.kind() {
            "class" | "class_declaration" | "abstract_class_declaration" => {
                declares(ancestor.child_by_field_name("name"))
            }
            "function_expression" | "generator_function" => {
                namespace == Namespace::Value && declares(ancestor.child_by_field_name("name"))
            }
            _ => false,
        };
        if self_named {
            return true;
        }
        if namespace == Namespace::Type
            && let Some(parameters) = ancestor.child_by_field_name("type_parameters")
        {
            let mut cursor = parameters.walk();
            if parameters
                .named_children(&mut cursor)
                .any(|parameter| declares(parameter.child_by_field_name("name")))
            {
                return true;
            }
        }
        current = ancestor.parent();
    }
    false
}

/// `export { … }`・`export * as …` が `name` を書き出すか。引用符の名前
/// (`export { X as "Source" }`) は中身で比べ、escape を含めば同じ名前とみなす。
fn exports_name(statement: Node<'_>, source: &[u8], name: &str) -> bool {
    let text = |node: Node<'_>| match node.kind() {
        "string" => super::super::static_js_string_text(node, source)
            .is_none_or(|text| text == name || text.contains('\\')),
        _ => node.utf8_text(source).ok() == Some(name),
    };
    let mut cursor = statement.walk();
    statement
        .named_children(&mut cursor)
        .any(|child| match child.kind() {
            "export_clause" => {
                let mut cursor = child.walk();
                child.named_children(&mut cursor).any(|specifier| {
                    specifier
                        .child_by_field_name("alias")
                        .or_else(|| specifier.child_by_field_name("name"))
                        .is_some_and(text)
                })
            }
            "namespace_export" => {
                let mut cursor = child.walk();
                child.named_children(&mut cursor).any(text)
            }
            _ => false,
        })
}

/// ファイル直下でただ 1 つの、型引数も `extends` も持たない interface か、右辺が object 型の
/// 型の別名の、メンバーの並び。同名の宣言 (宣言のマージ・class・enum・namespace) や、
/// `declare global` / `declare module` があるファイルは扱わない。`exported` なら直接の
/// named export に限り、同じ名前の再 export や `export =` があるファイルは扱わない。
fn root_object_members<'t>(
    parsed: &'t ParsedRefFile,
    name: &str,
    exported: bool,
) -> Option<Node<'t>> {
    let source: &[u8] = &parsed.source;
    let root = parsed.tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut found = None;
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        let (declaration, is_exported) = if statement.kind() == "export_statement" {
            match statement.child_by_field_name("declaration") {
                // `export default interface Source` もファイルの中の宣言で、named export ではない。
                Some(declaration) => (declaration, !has_token(statement, "default")),
                None => {
                    if exported
                        && (has_token(statement, "=") || exports_name(statement, source, name))
                    {
                        return None;
                    }
                    continue;
                }
            }
        } else {
            (statement, false)
        };
        if declaration.kind() == "ambient_declaration" {
            return None;
        }
        let declares = matches!(
            declaration.kind(),
            "interface_declaration"
                | "type_alias_declaration"
                | "class_declaration"
                | "abstract_class_declaration"
                | "enum_declaration"
                | "internal_module"
                | "module"
        ) && declaration
            .child_by_field_name("name")
            .and_then(|declared| declared.utf8_text(source).ok())
            == Some(name);
        if !declares {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some((declaration, is_exported));
    }
    let (declaration, is_exported) = found?;
    if (exported && !is_exported) || declaration.child_by_field_name("type_parameters").is_some() {
        return None;
    }
    match declaration.kind() {
        "interface_declaration" => {
            let mut cursor = declaration.walk();
            if declaration
                .named_children(&mut cursor)
                .any(|child| child.kind() == "extends_type_clause")
            {
                return None;
            }
            declaration
                .child_by_field_name("body")
                .filter(|body| body.kind() == "interface_body")
        }
        "type_alias_declaration" => declaration
            .child_by_field_name("value")
            .filter(|value| value.kind() == "object_type"),
        _ => None,
    }
}

/// object 型・interface の本体 `members` の中で、キーが `key` のプロパティの型。メンバーが
/// プロパティだけで、キーが重複せず、型が `simple_type` で読める場合に限る。
fn declared_member_type(members: Node<'_>, source: &[u8], key: &str) -> Option<Vec<TypeAtom>> {
    let mut found = None;
    let mut cursor = members.walk();
    for member in members.named_children(&mut cursor) {
        if member.kind() == "comment" {
            continue;
        }
        // method・index・call・construct の signature や getter / setter は、読み出しの型を
        // プロパティの注釈だけで決められない。
        if member.kind() != "property_signature" {
            return None;
        }
        let name_node = member.child_by_field_name("name")?;
        if name_node.utf8_text(source).ok()?.contains('\\') {
            return None;
        }
        let name = super::super::object_key_text(name_node, source)?;
        if name != key {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(member);
    }
    let member = found?;
    let mut ty = simple_type(
        only_named_child(member.child_by_field_name("type")?)?,
        source,
    )?;
    // 配列の要素の `any` / `unknown` も、どの値でも入りうる。
    if !super::narrowed_uses::known_type(&ty) {
        return None;
    }
    if has_token(member, "?") {
        ty.push(TypeAtom::Undefined);
        ty.sort();
        ty.dedup();
    }
    Some(ty)
}
