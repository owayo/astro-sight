//! 同じファイルの private inherent メソッドと結び付かないドット呼び出し。

mod project;

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

pub(super) use project::Project;
use tree_sitter::{Node, Point};

use super::declaration::PrivateMethodOwner;
use super::rust_macro_site::OpaqueAttributeScopes;
use super::rust_owner_alias::{Names, items_with_attributes};
use super::rust_path_owner::{PrivateModule, RustPathOwnerIndex};
use super::rust_std_origin::RustStdOriginIndex;
use crate::engine::rust_scope::RustScopeIndex;
use crate::engine::rust_use_tree::UsePathOrigin;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

#[derive(Clone, Copy)]
enum Receiver<'a> {
    Slice,
    Collection,
    Struct(&'a str),
    Factory {
        module: &'a str,
        ty: &'a str,
        function: &'a str,
    },
}

struct Imports<'a> {
    project: &'a Project<'a>,
    module: Arc<PrivateModule>,
    names: Names,
    concrete: HashSet<String>,
    old: bool,
}

/// edition 2021 以降の無条件の宣言で、裸の具象型として使われる名前。trait の
/// 境界や dyn/impl Trait の内側、型引数が名前を隠す宣言は証拠にしない。
fn concrete_types(root: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut result = HashSet::new();
    let mut cursor = root.walk();
    for (item, attributed) in items_with_attributes(root.named_children(&mut cursor)) {
        if attributed || has_type_bindings(item) {
            continue;
        }
        let mut pending = Vec::new();
        let fields = match item.kind() {
            "struct_item" => item.child_by_field_name("body"),
            "function_item" => {
                pending.extend(item.child_by_field_name("return_type"));
                item.child_by_field_name("parameters")
            }
            "impl_item" => {
                pending.extend(item.child_by_field_name("type"));
                None
            }
            _ => None,
        };
        if let Some(fields) = fields {
            for (field, attributed) in
                items_with_attributes(fields.named_children(&mut fields.walk()))
            {
                if !attributed {
                    pending.extend(field.child_by_field_name("type"));
                }
            }
        }
        while let Some(ty) = pending.pop() {
            match ty.kind() {
                "type_identifier" => {
                    if let Some(name) = text(ty, source) {
                        result.insert(name.to_owned());
                    }
                }
                "reference_type" | "array_type" => pending.extend(
                    ty.child_by_field_name("type")
                        .or_else(|| ty.child_by_field_name("element")),
                ),
                "generic_type" => {
                    pending.extend(ty.child_by_field_name("type"));
                    pending.extend(ty.child_by_field_name("type_arguments"));
                }
                "tuple_type" | "type_arguments" => {
                    pending.extend(ty.named_children(&mut ty.walk()))
                }
                _ => {}
            }
        }
    }
    result
}

impl<'a> Imports<'a> {
    fn new(
        project: &'a Project<'a>,
        root: Node<'_>,
        source: &[u8],
        file: &str,
        old: bool,
    ) -> Option<Self> {
        let file = Path::new(file);
        let module = project.layout.ordinary_private_module(file)?;
        if !module.library_only
            || !project.layout.unrelocated(file)
            || old && !project.previous_layout_known()
        {
            return None;
        }
        project.module(&module, &module.path, old)?;
        let names = Names::collect(root, source, &module.path)?;
        if !names.globs.is_empty() {
            return None;
        }
        let package = project
            .layout
            .alias_cache
            .manifest(&project.layout.root, module.src.parent()?)?;
        if !matches!(
            package.get("package")?.get("edition")?.as_str()?,
            "2021" | "2024"
        ) {
            return None;
        }
        let concrete = concrete_types(root, source);
        Some(Self {
            project,
            module,
            names,
            concrete,
            old,
        })
    }

    fn module_path(&self, origin: UsePathOrigin, path: &[String]) -> Option<Vec<String>> {
        if origin == UsePathOrigin::Anchored {
            return Some(path.to_vec());
        }
        if origin != UsePathOrigin::Bare
            || !path
                .first()
                .is_some_and(|name| self.names.items.contains(name))
        {
            return None;
        }
        let mut full = self.module.path.clone();
        full.extend_from_slice(path);
        Some(full)
    }

    fn no_extension_get(&self, root: Node<'_>) -> bool {
        if ["Option", "Some"]
            .iter()
            .any(|&n| self.names.items.contains(n) || self.names.imports.contains_key(n))
            || root
                .named_children(&mut root.walk())
                .any(|n| n.kind() == "trait_item")
        {
            return false;
        }
        self.names.imports.iter().all(|(name, binding)| {
            if self.concrete.contains(name) {
                return true;
            }
            let Some((origin, path)) = binding else {
                return false;
            };

            let Some(full) = self.module_path(*origin, path) else {
                return false;
            };
            let Some((item, module)) = full.split_last() else {
                return false;
            };
            self.project
                .module(&self.module, module, self.old)
                .is_some_and(|m| m.nontraits.contains(item))
        })
    }

    fn factory_method(&self, module: &str, ty: &str, factory: &str, method: &str) -> bool {
        let Some((origin, path)) = self.names.imports.get(module).and_then(Option::as_ref) else {
            return false;
        };
        self.module_path(*origin, path)
            .and_then(|p| self.project.module(&self.module, &p, self.old))
            .is_some_and(|m| {
                m.nominals.contains(ty)
                    && m.factories.contains(&(ty.to_owned(), factory.to_owned()))
                    && m.methods.contains(&(ty.to_owned(), method.to_owned()))
            })
    }
}

struct Types<'a> {
    names: RustStdOriginIndex,
    root: usize,
    fields: HashMap<&'a str, HashMap<&'a str, Receiver<'a>>>,
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source)
        .ok()
        .filter(|s| s.is_ascii())
        .map(|s| s.strip_prefix("r#").unwrap_or(s))
}

impl<'a> Types<'a> {
    fn collect(root: Node<'_>, source: &'a [u8]) -> Option<Self> {
        let mut types = Self {
            names: RustStdOriginIndex::from_source(root, source)?,
            root: root.id(),
            fields: HashMap::new(),
        };
        let mut cursor = root.walk();
        for (item, attributed) in items_with_attributes(root.named_children(&mut cursor)) {
            if attributed || item.kind() != "struct_item" || has_type_bindings(item) {
                continue;
            }
            let name = text(item.child_by_field_name("name")?, source)?;
            if !types.names.file_type(root.id(), root.id(), name) {
                continue;
            }
            let mut fields = HashMap::new();
            if let Some(body) = item.child_by_field_name("body") {
                let mut cursor = body.walk();
                for (field, attributed) in items_with_attributes(body.named_children(&mut cursor)) {
                    if !attributed
                        && let Some(name) = field
                            .child_by_field_name("name")
                            .and_then(|n| text(n, source))
                        && let Some(ty) = field
                            .child_by_field_name("type")
                            .and_then(|ty| types.declared_type(ty, root.id(), source))
                    {
                        fields.insert(name, ty);
                    }
                }
            }
            types.fields.insert(name, fields);
        }
        Some(types)
    }

    fn projected_type(&self, receiver: Receiver<'a>, field: Option<&str>) -> Option<Receiver<'a>> {
        match (receiver, field) {
            (Receiver::Struct(name), Some(field)) => self.fields.get(name)?.get(field).copied(),
            (receiver, None) => Some(receiver),
            _ => None,
        }
    }

    fn declared_type(
        &self,
        mut ty: Node<'_>,
        scope: usize,
        source: &'a [u8],
    ) -> Option<Receiver<'a>> {
        while ty.kind() == "reference_type" {
            ty = ty.child_by_field_name("type")?;
        }
        if ty.kind() == "array_type" {
            return Some(Receiver::Slice);
        }
        if ty.kind() == "generic_type" {
            ty = ty.child_by_field_name("type")?;
        }
        if self.names.collection_receiver(scope, ty, source) {
            return Some(Receiver::Collection);
        }
        let name = text(ty, source)?;
        (ty.kind() == "type_identifier" && self.names.file_type(scope, self.root, name))
            .then_some(Receiver::Struct(name))
    }
}

pub(super) fn independent_calls<'p>(
    root: Node<'_>,
    source: &[u8],
    owners: &HashMap<String, PrivateMethodOwner>,
    file: &str,
    old: bool,
    project: &impl Fn() -> Option<&'p Project<'p>>,
) -> HashSet<Point> {
    let mut calls = HashSet::new();
    let Some(types) = Types::collect(root, source) else {
        return calls;
    };
    let Some(locals) = RustScopeIndex::build(root, source) else {
        return calls;
    };
    let imports = OnceCell::new();
    let imports = || {
        imports
            .get_or_init(|| Imports::new(project()?, root, source, file, old))
            .as_ref()
    };
    let no_extension = OnceCell::new();
    let std = OnceCell::new();
    let standard = || *std.get_or_init(|| project().is_some_and(|p| p.stock_std(file, old)));
    let get_result = owners
        .get("get")
        .and_then(|owner| get_result_type(root, source, owner, &types));
    let standard_test = memchr::memmem::find(source, b"#[test]").is_some()
        && imports().is_some_and(|imports| {
            super::rust_visibility_scope::parent_glob_import_names(source)
                .is_some_and(|names| !names.contains("test"))
                && !imports.names.items.contains("test")
                && standard()
        });
    let opaque = OpaqueAttributeScopes::with_test_attribute(root, source, standard_test);
    let mut path = Vec::new();
    let mut scopes = Vec::new();
    let mut values = HashMap::new();
    let macro_names = OnceCell::new();
    let stock_macro = |call: Node<'_>, name: &str| {
        let Some(imports) = imports() else {
            return false;
        };
        let p = imports.project.layout;
        !imports.names.items.contains(name)
            && !imports.names.imports.contains_key(name)
            && (!old
                || imports
                    .project
                    .old_macro_names()
                    .is_some_and(|names| !names.contains(name)))
            && macro_names
                .get_or_init(|| RustPathOwnerIndex::build(root, source))
                .standard_macro_call(call, name, source, &p.root, file, p)
    };
    let mut factory_scopes = HashMap::new();
    let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
        path.truncate(depth);
        scopes.truncate(depth);
        let scope = types
            .names
            .scope_at(node, scopes.last().copied().unwrap_or(root.id()));
        if opaque.contains(node) || matches!(node.kind(), "macro_invocation" | "token_tree") {
            return ControlFlow::Continue(Walk::Skip);
        }
        match node.kind() {
            "function_item" => {
                if let Some(params) = node.child_by_field_name("parameters") {
                    let mut cursor = params.walk();
                    for (param, attributed) in
                        items_with_attributes(params.named_children(&mut cursor))
                    {
                        if !attributed {
                            insert_type(&mut values, node.id(), param, scope, source, &types);
                        }
                    }
                }
            }
            "let_declaration" => {
                insert_type(&mut values, node.id(), node, scope, source, &types);
                if let Some(pattern) = node.child_by_field_name("pattern")
                    && let Some(value) = node.child_by_field_name("value")
                    && value.kind() == "call_expression"
                    && let Some(callee) = value.child_by_field_name("function")
                    && node.child_by_field_name("type").is_none()
                {
                    if let Some((binding, base)) = some_get(pattern, callee, source)
                        && let Some((owner, result)) = get_result
                        && let Some(decl) = locals.receiver_binding(base, value, &path)
                        && matches!(values.get(&(decl, base)), Some(Receiver::Struct(actual)) if *actual == owner)
                        && !path.iter().any(|&n| {
                            matches!(n.kind(), "mod_item" | "trait_item" | "impl_item")
                                || has_type_bindings(n)
                        })
                        && locals.blocks_without_imports(&path)
                        && *no_extension
                            .get_or_init(|| imports().is_some_and(|i| i.no_extension_get(root)))
                    {
                        values.insert((node.id(), binding), Receiver::Struct(result));
                    } else if pattern.kind() == "identifier"
                        && let Some(name) = text(pattern, source)
                        && let Some(Receiver::Factory {
                            module,
                            ty,
                            function,
                        }) = factory_path(callee, source)
                        && !path
                            .iter()
                            .any(|n| n.child_by_field_name("type_parameters").is_some())
                        && path
                            .iter()
                            .filter(|n| matches!(n.kind(), "block" | "declaration_list"))
                            .all(|n| {
                                *factory_scopes.entry((n.id(), module)).or_insert_with(|| {
                                    factory_scope(*n, source, module, &opaque, stock_macro)
                                })
                            })
                    {
                        values.insert(
                            (node.id(), name),
                            Receiver::Factory {
                                module,
                                ty,
                                function,
                            },
                        );
                    }
                }
            }
            "call_expression" => {
                if let Some(callee) = node.child_by_field_name("function")
                    && callee.kind() == "field_expression"
                    && let Some(member) = callee.child_by_field_name("field")
                    && let Some(name) = text(member, source)
                    && owners.get(name).is_some_and(|owner| owner.unconditional)
                    && let Some(value) = callee.child_by_field_name("value")
                    && let Some((base, field)) = receiver_parts(value, source)
                    && let Some(key) = locals.receiver_binding(base, value, &path)
                    && let Some(receiver) = values.get(&(key, base))
                    && let Some(receiver) = types.projected_type(*receiver, field)
                    && match receiver {
                        Receiver::Slice => true,
                        Receiver::Collection => standard(),
                        // 型引数を省いた impl のメソッドは、別の具体化では Deref 先を指し得る。
                        Receiver::Struct(ty) => {
                            types.fields.contains_key(ty)
                                && owners[name]
                                    .peers
                                    .as_ref()
                                    .is_some_and(|other| other.contains(ty))
                        }
                        Receiver::Factory {
                            module,
                            ty,
                            function,
                        } => {
                            imports().is_some_and(|i| i.factory_method(module, ty, function, name))
                        }
                    }
                {
                    calls.insert(member.start_position());
                }
            }
            _ => {}
        }
        path.push(node);
        scopes.push(scope);
        ControlFlow::Continue(Walk::Descend)
    });
    calls
}

fn factory_scope(
    scope: Node<'_>,
    source: &[u8],
    module: &str,
    opaque: &OpaqueAttributeScopes,
    stock_macro: impl Fn(Node<'_>, &str) -> bool,
) -> bool {
    let mut glob = false;
    let mut cursor = scope.walk();
    for (item, attributed) in items_with_attributes(scope.named_children(&mut cursor)) {
        if attributed && (scope.kind() == "block" || opaque.contains(item)) {
            return false;
        }
        let invocation = match item.kind() {
            "macro_invocation" => Some(item),
            "expression_statement" => item
                .named_child(0)
                .filter(|n| n.kind() == "macro_invocation"),
            _ => None,
        };
        if let Some(call) = invocation {
            let Some(name) = call
                .child_by_field_name("macro")
                .and_then(|n| text(n, source))
            else {
                return false;
            };
            if !matches!(
                name,
                "assert"
                    | "assert_eq"
                    | "assert_ne"
                    | "debug_assert"
                    | "debug_assert_eq"
                    | "debug_assert_ne"
            ) || !stock_macro(call, name)
            {
                return false;
            }
        } else if item.kind() == "use_declaration" {
            let parent = item
                .child_by_field_name("argument")
                .is_some_and(|a| a.utf8_text(source) == Ok("super::*"));
            if !parent || attributed || scope.kind() != "declaration_list" {
                return false;
            }
            glob = true;
        } else if matches!(
            item.kind(),
            "macro_invocation" | "macro_definition" | "extern_crate_declaration"
        ) || item
            .child_by_field_name("name")
            .and_then(|n| text(n, source))
            == Some(module)
        {
            return false;
        }
    }
    scope.kind() != "declaration_list" || glob
}

fn has_type_bindings(item: Node<'_>) -> bool {
    item.child_by_field_name("type_parameters")
        .is_some_and(|params| {
            params
                .named_children(&mut params.walk())
                .any(|n| n.kind() != "lifetime_parameter")
        })
}

/// `let Some(binding) = base.get(..)`。get は標準 prelude の trait メソッドではない。
fn some_get<'a>(
    pattern: Node<'_>,
    callee: Node<'_>,
    source: &'a [u8],
) -> Option<(&'a str, &'a str)> {
    if pattern.kind() != "tuple_struct_pattern"
        || pattern.named_child_count() != 2
        || pattern
            .child_by_field_name("type")?
            .utf8_text(source)
            .ok()?
            != "Some"
        || callee.kind() != "field_expression"
        || text(callee.child_by_field_name("field")?, source)? != "get"
    {
        return None;
    }
    let binding = pattern
        .named_child(1)
        .filter(|n| n.kind() == "identifier")?;
    let base = callee
        .child_by_field_name("value")
        .filter(|n| n.kind() == "identifier")?;
    Some((text(binding, source)?, text(base, source)?))
}

fn factory_path<'a>(callee: Node<'_>, source: &'a [u8]) -> Option<Receiver<'a>> {
    if callee.kind() != "scoped_identifier" {
        return None;
    }
    let owner = callee
        .child_by_field_name("path")
        .filter(|n| n.kind() == "scoped_identifier")?;
    let module = owner
        .child_by_field_name("path")
        .filter(|n| n.kind() == "identifier")?;
    Some(Receiver::Factory {
        module: text(module, source)?,
        ty: text(owner.child_by_field_name("name")?, source)?,
        function: text(callee.child_by_field_name("name")?, source)?,
    })
}

fn get_result_type<'a>(
    root: Node<'_>,
    source: &'a [u8],
    target: &PrivateMethodOwner,
    types: &Types<'_>,
) -> Option<(&'a str, &'a str)> {
    if !target.unconditional {
        return None;
    }
    let [target_name] = target.names.as_slice() else {
        return None;
    };
    let mut cursor = root.walk();
    for item in root
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "impl_item" && n.child_by_field_name("trait").is_none())
    {
        let Some(owner) = item
            .child_by_field_name("type")
            .and_then(|n| text(n, source))
        else {
            continue;
        };
        let Some(body) = item
            .child_by_field_name("body")
            .filter(|_| owner == target_name)
        else {
            continue;
        };
        for method in body.named_children(&mut body.walk()) {
            let Some(name) = method
                .child_by_field_name("name")
                .and_then(|n| text(n, source))
            else {
                continue;
            };
            if name != "get"
                || has_type_bindings(method)
                || !method
                    .child_by_field_name("parameters")
                    .and_then(|p| p.named_child(0))
                    .is_some_and(|p| p.kind() == "self_parameter")
            {
                continue;
            }
            if let Some(ret) = method
                .child_by_field_name("return_type")
                .filter(|n| n.kind() == "generic_type")
                && ret
                    .child_by_field_name("type")
                    .is_some_and(|ty| ty.utf8_text(source) == Ok("Option"))
                && let Some(args) = ret
                    .child_by_field_name("type_arguments")
                    .filter(|n| n.named_child_count() == 1)
                && let Some(ty) = args
                    .named_child(0)
                    .filter(|n| n.kind() == "reference_type")
                    .and_then(|n| n.child_by_field_name("type"))
                    .and_then(|n| text(n, source))
                && types.fields.contains_key(ty)
            {
                return Some((owner, ty));
            }
        }
    }
    None
}

fn insert_type<'a>(
    values: &mut HashMap<(usize, &'a str), Receiver<'a>>,
    id: usize,
    binding: Node<'_>,
    scope: usize,
    source: &'a [u8],
    types: &Types<'a>,
) {
    if let Some(name) = binding
        .child_by_field_name("pattern")
        .filter(|p| p.kind() == "identifier")
        .and_then(|p| text(p, source))
        && let Some(ty) = binding
            .child_by_field_name("type")
            .and_then(|ty| types.declared_type(ty, scope, source))
    {
        values.insert((id, name), ty);
    }
}

fn receiver_parts<'a>(value: Node<'_>, source: &'a [u8]) -> Option<(&'a str, Option<&'a str>)> {
    match value.kind() {
        "identifier" => Some((text(value, source)?, None)),
        "field_expression" => {
            let base = value.child_by_field_name("value")?;
            (base.kind() == "identifier").then_some((
                text(base, source)?,
                Some(text(value.child_by_field_name("field")?, source)?),
            ))
        }
        _ => None,
    }
}
