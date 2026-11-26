//! 宣言元の公開値と import の束縛を起点に、オブジェクトの書き込み・流出を調べる。

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::{StaticValue, project::Candidate};
use crate::engine::impact::{
    ts_barrels::WriteOrigins,
    ts_const_aliases::{Binding, module_imports, visit_bindings},
};
use crate::engine::js_scope::{Namespace, find_binding_in_scope};
use crate::engine::js_string::decoded;

#[derive(Clone)]
enum Value {
    Local(usize),
    Function(String),
    Import(String, Option<String>),
    Exports,
    Module,
    Loader,
    Builtin,
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub(super) struct Getter {
    file: String,
    name: String,
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub(super) struct GetterCall {
    caller: String,
    module: Option<String>,
    name: String,
}

impl GetterCall {
    fn may_invoke(&self, getter: &Getter, origins: &WriteOrigins<'_>) -> bool {
        match &self.module {
            Some(module) => {
                origins.may_call(&self.caller, module, &self.name, &getter.file, &getter.name)
            }
            None => self.caller == getter.file && self.name == getter.name,
        }
    }
}

pub(super) type GetterForwards = HashSet<(Getter, GetterCall)>;

#[derive(Default)]
pub(super) struct GetterUses {
    returns: HashSet<Getter>,
    calls: HashSet<GetterCall>,
}

impl GetterUses {
    // 値を返すだけの関数は書き込み元にならない。その結果を受け取った呼び出しまで照合する。
    pub(super) fn may_write(self, origins: &WriteOrigins<'_>, forwards: &GetterForwards) -> bool {
        if self.calls.is_empty() {
            return false;
        }
        let mut visited = self.returns;
        let mut pending: Vec<_> = visited.iter().cloned().collect();
        while let Some(getter) = pending.pop() {
            if self
                .calls
                .iter()
                .any(|call| call.may_invoke(&getter, origins))
            {
                return true;
            }
            for (wrapper, call) in forwards {
                if !visited.contains(wrapper) && call.may_invoke(&getter, origins) {
                    visited.insert(wrapper.clone());
                    pending.push(wrapper.clone());
                }
            }
        }
        false
    }
}

fn returned_by<'s>(access: &Access<'_, '_>, source: &'s [u8]) -> Option<&'s str> {
    let parent = access.outer.last()?;
    if parent.kind() != "return_statement"
        && !(parent.kind() == "arrow_function" && field(*parent, "body", access.end))
    {
        return None;
    }
    for (i, n) in access.outer.iter().enumerate().rev() {
        match n.kind() {
            "function_declaration" | "generator_function_declaration" => {
                return n.child_by_field_name("name")?.utf8_text(source).ok();
            }
            "arrow_function" | "function_expression" | "generator_function" => {
                let decl = access.outer.get(i.checked_sub(1)?)?;
                return (decl.kind() == "variable_declarator" && field(*decl, "value", *n))
                    .then(|| decl.child_by_field_name("name"))??
                    .utf8_text(source)
                    .ok();
            }
            "method_definition" => return None,
            _ => {}
        }
    }
    None
}

fn local_functions<'s>(
    root: Node<'_>,
    source: &'s [u8],
    bindings: &mut HashMap<Cow<'s, str>, Binding<Value>>,
) {
    let mut add = |decl: Node<'_>| {
        if let Some(name) = decl
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier")
            .and_then(|n| n.utf8_text(source).ok())
            .and_then(decoded)
        {
            bindings.entry(name.clone()).or_insert_with(|| Binding {
                target: Value::Function(name.into_owned()),
                type_only: false,
            });
        }
    };
    for node in root.named_children(&mut root.walk()) {
        let decl = node.child_by_field_name("declaration").unwrap_or(node);
        if matches!(
            decl.kind(),
            "function_declaration" | "generator_function_declaration"
        ) {
            add(decl);
        }
        if matches!(decl.kind(), "lexical_declaration" | "variable_declaration") {
            for var in decl.named_children(&mut decl.walk()).filter(|n| {
                n.child_by_field_name("value").is_some_and(|v| {
                    matches!(
                        v.kind(),
                        "arrow_function" | "function_expression" | "generator_function"
                    )
                })
            }) {
                add(var);
            }
        }
    }
}

fn key<'s>(node: Node<'_>, source: &'s [u8]) -> Option<Cow<'s, str>> {
    let raw = node.utf8_text(source).ok()?;
    match node.kind() {
        "property_identifier" => decoded(raw),
        "string" => decoded(crate::engine::js_string::quoted_contents(raw)?),
        _ => None,
    }
}

fn field(node: Node<'_>, name: &str, child: Node<'_>) -> bool {
    node.child_by_field_name(name)
        .is_some_and(|n| n.id() == child.id())
}

struct Access<'tree, 's> {
    end: Node<'tree>,
    outer: &'s [Node<'tree>],
    keys: Vec<Option<Cow<'s, str>>>,
}

fn access<'tree, 's>(
    mut end: Node<'tree>,
    mut outer: &'s [Node<'tree>],
    source: &'s [u8],
) -> Access<'tree, 's> {
    let mut keys = Vec::new();
    while let Some((&parent, rest)) = outer.split_last() {
        match parent.kind() {
            "member_expression" | "subscript_expression" if field(parent, "object", end) => {
                // 入れ子の member_expression の field が内側の property を返す文法にも合わせる。
                let last = parent
                    .named_children(&mut parent.walk())
                    .filter(|n| !n.is_extra())
                    .last();
                keys.push(last.and_then(|k| key(k, source)));
            }
            "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression"
                if parent.named_child(0) == Some(end) => {}
            _ => break,
        }
        end = parent;
        outer = rest;
    }
    Access { end, outer, keys }
}

fn assignment(access: &Access<'_, '_>, source: &[u8]) -> bool {
    let mut end = access.end;
    for &parent in access.outer.iter().rev() {
        match parent.kind() {
            "array_pattern" | "object_pattern" | "rest_pattern" | "assignment_pattern"
            | "pair_pattern" => end = parent,
            "assignment_expression" | "augmented_assignment_expression" | "for_in_statement" => {
                return field(parent, "left", end);
            }
            "update_expression" => return true,
            "unary_expression" => {
                return parent
                    .child_by_field_name("operator")
                    .is_some_and(|op| op.utf8_text(source).ok() == Some("delete"));
            }
            _ => return false,
        }
    }
    false
}

fn object_value<'a>(
    mut value: &'a StaticValue,
    keys: &[Option<Cow<'_, str>>],
) -> Option<&'a StaticValue> {
    for key in keys {
        let StaticValue::Object(entries) = value else {
            return Some(value);
        };
        let name = key.as_deref()?;
        value = &entries.iter().find(|(k, _)| k == name)?.1;
    }
    Some(value)
}

fn can_escape(access: &Access<'_, '_>, value: &StaticValue, keys: &[Option<Cow<'_, str>>]) -> bool {
    let Some(parent) = access.outer.last() else {
        return false;
    };
    if keys.is_empty() && field(*parent, "function", access.end) {
        return false;
    }
    if matches!(
        parent.kind(),
        "expression_statement" | "unary_expression" | "binary_expression" | "type_query"
    ) {
        return false;
    }
    let Some(value) = object_value(value, keys) else {
        return true;
    };
    let StaticValue::Object(entries) = value else {
        return false;
    };
    if parent.kind() == "spread_element"
        && access
            .outer
            .iter()
            .rev()
            .nth(1)
            .is_some_and(|n| n.kind() == "object")
    {
        return entries
            .iter()
            .any(|(_, v)| matches!(v, StaticValue::Object(_)));
    }
    true
}

fn excludes_export(
    access: &Access<'_, '_>,
    source: &[u8],
    name: &str,
    builtins: &HashSet<usize>,
) -> bool {
    let mut parents = access.outer.iter().rev();
    let Some(args) = parents.next().filter(|p| p.kind() == "arguments") else {
        return false;
    };
    let Some(callee) = parents
        .next()
        .and_then(|call| call.child_by_field_name("function"))
    else {
        return false;
    };
    if !callee
        .child_by_field_name("object")
        .is_some_and(|n| builtins.contains(&n.id()))
    {
        return false;
    }
    let mut cursor = args.walk();
    let mut args = args.named_children(&mut cursor).filter(|n| !n.is_extra());
    if args.next() != Some(access.end) {
        return false;
    }
    match callee.utf8_text(source).ok() {
        Some(
            "Object.defineProperty"
            | "Reflect.defineProperty"
            | "Reflect.set"
            | "Reflect.deleteProperty",
        ) => args
            .next()
            .and_then(|n| key(n, source))
            .is_some_and(|k| k != name),
        Some("Object.assign" | "Object.defineProperties") => args.all(|n| {
            n.kind() == "object"
                && n.named_children(&mut n.walk())
                    .filter(|n| !n.is_extra())
                    .all(|n| {
                        n.kind() == "pair"
                            && n.child_by_field_name("key")
                                .and_then(|n| key(n, source))
                                .is_some_and(|k| k != name)
                    })
        }),
        _ => false,
    }
}

fn single_child(n: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = n.walk();
    let mut children = n.named_children(&mut cursor).filter(|n| !n.is_extra());
    let child = children.next()?;
    children.next().is_none().then_some(child)
}

// import(...).then(({ X }) => X.a) はコールバックの引数から返すメンバーまでを照合する。
fn then_read<'s>(
    access: &Access<'_, '_>,
    source: &'s [u8],
) -> Option<(&'s str, Vec<Option<Cow<'s, str>>>)> {
    if access.keys.len() != 1 || access.keys[0].as_deref() != Some("then") {
        return None;
    }
    let call = access.outer.last()?;
    if !field(*call, "function", access.end) {
        return None;
    }
    let arrow = single_child(call.child_by_field_name("arguments")?)?;
    if arrow.kind() != "arrow_function" {
        return None;
    }
    let mut pattern = single_child(arrow.child_by_field_name("parameters")?)?;
    if pattern.kind() == "required_parameter" {
        pattern = single_child(pattern)?;
    }
    if pattern.kind() != "object_pattern" {
        return None;
    }
    let binding = single_child(pattern)?;
    let (key, local) = match binding.kind() {
        "shorthand_property_identifier_pattern" => (binding, binding),
        "pair_pattern" => (
            binding.child_by_field_name("key")?,
            binding.child_by_field_name("value")?,
        ),
        _ => return None,
    };
    let (base, path) =
        super::type_queries::member_path(arrow.child_by_field_name("body")?, source)?;
    if path.is_empty()
        || base.kind() != "identifier"
        || base.utf8_text(source).ok()? != local.utf8_text(source).ok()?
    {
        return None;
    }
    let name = key.utf8_text(source).ok()?;
    if name.contains('\\')
        || !matches!(
            key.kind(),
            "property_identifier" | "shorthand_property_identifier_pattern"
        )
    {
        return None;
    }
    Some((
        name,
        path.into_iter().map(|k| Some(Cow::Borrowed(k))).collect(),
    ))
}

fn loader_call<'s>(call: Node<'_>, source: &'s [u8]) -> Option<Cow<'s, str>> {
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let mut args = args.named_children(&mut cursor).filter(|n| !n.is_extra());
    let arg = args.next()?;
    if args.next().is_some() {
        return None;
    }
    if arg.kind() == "template_string"
        && !arg
            .named_children(&mut arg.walk())
            .any(|n| n.kind() == "template_substitution")
    {
        return decoded(
            arg.utf8_text(source)
                .ok()?
                .strip_prefix('`')?
                .strip_suffix('`')?,
        );
    }
    (arg.kind() == "string").then(|| key(arg, source)).flatten()
}

fn alias_value<'s>(
    mut value: Node<'_>,
    source: &'s [u8],
    bindings: &HashMap<Cow<'s, str>, Binding<Value>>,
) -> Option<Value> {
    let mut member = None;
    if matches!(value.kind(), "member_expression" | "subscript_expression") {
        let mut cursor = value.walk();
        let mut children = value.named_children(&mut cursor).filter(|n| !n.is_extra());
        value = children.next()?;
        member = Some(key(children.next()?, source)?.into_owned());
        if children.next().is_some() {
            return None;
        }
    }
    let target = if value.kind() == "identifier" {
        bindings
            .get(decoded(value.utf8_text(source).ok()?)?.as_ref())?
            .target
            .clone()
    } else {
        let awaited = value.kind() == "await_expression";
        if awaited {
            value = value.named_child(0)?;
        }
        let callee = value.child_by_field_name("function")?;
        if !awaited && callee.kind() == "import" {
            return None;
        }
        if !matches!(
            bindings
                .get(callee.utf8_text(source).ok()?)
                .map(|b| &b.target),
            Some(Value::Loader)
        ) {
            return None;
        }
        Value::Import(loader_call(value, source)?.into_owned(), None)
    };
    match (target, member) {
        (Value::Import(module, None), Some(name)) => Some(Value::Import(module, Some(name))),
        (value, None) => Some(value),
        _ => None,
    }
}

// モジュール直下の const 別名は実際の使用先を見る。公開・分解できない別名は流出として残す。
fn aliases<'s>(
    root: Node<'_>,
    source: &'s [u8],
    bindings: &mut HashMap<Cow<'s, str>, Binding<Value>>,
) -> HashSet<usize> {
    let mut exported = HashSet::new();
    crate::engine::symbols::visit_js_ts_export_clause_specifiers(root, source, |s| {
        if s.statement.child_by_field_name("source").is_none()
            && let Some(name) = decoded(s.local_name)
        {
            exported.insert(name);
        }
    });
    let mut inits = HashSet::new();
    for stmt in root.named_children(&mut root.walk()).filter(|n| {
        n.kind() == "lexical_declaration"
            && n.child_by_field_name("kind")
                .is_some_and(|k| k.kind() == "const")
    }) {
        for decl in stmt.named_children(&mut stmt.walk()) {
            let Some(value) = decl.child_by_field_name("value") else {
                continue;
            };
            let Some(pattern) = decl.child_by_field_name("name") else {
                continue;
            };
            let Some(target) = alias_value(value, source, bindings) else {
                continue;
            };
            let mut add = |pat: Node<'_>, target: Value| {
                let Some(local) = pat.utf8_text(source).ok().and_then(decoded) else {
                    return false;
                };
                if exported.contains(local.as_ref()) || bindings.contains_key(local.as_ref()) {
                    return false;
                }
                inits.insert(pat.id());
                bindings.insert(
                    local,
                    Binding {
                        target,
                        type_only: false,
                    },
                );
                true
            };
            let handled = match (pattern.kind(), target) {
                ("identifier", target) => add(pattern, target),
                ("object_pattern", Value::Import(module, None)) => pattern
                    .named_children(&mut pattern.walk())
                    .filter(|n| !n.is_extra())
                    .all(|n| {
                        let (key, local) = match n.kind() {
                            "shorthand_property_identifier_pattern" => (n, n),
                            "pair_pattern" => {
                                let Some(k) = n.child_by_field_name("key") else {
                                    return false;
                                };
                                let Some(l) = n
                                    .child_by_field_name("value")
                                    .filter(|l| l.kind() == "identifier")
                                else {
                                    return false;
                                };
                                (k, l)
                            }
                            _ => return false,
                        };
                        let Some(name) = key.utf8_text(source).ok().and_then(decoded) else {
                            return false;
                        };
                        add(
                            local,
                            Value::Import(module.clone(), Some(name.into_owned())),
                        )
                    }),
                _ => false,
            };
            if handled {
                inits.insert(value.id());
            }
        }
    }
    inits
}

pub(super) fn check_file(
    root: Node<'_>,
    source: &[u8],
    caller: &str,
    origins: &WriteOrigins<'_>,
    forwards: &mut GetterForwards,
    candidates: &mut [Candidate<'_>],
) {
    let mut bindings = HashMap::new();
    module_imports(root, source, |module, local, exported, type_only, _| {
        if !type_only {
            bindings.insert(
                local,
                Binding {
                    target: Value::Import(module.to_owned(), exported.map(Cow::into_owned)),
                    type_only: false,
                },
            );
        }
    });
    let owned: Vec<_> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| (c.path == caller).then_some(i))
        .collect();
    for &i in &owned {
        bindings.insert(
            Cow::Borrowed(candidates[i].name),
            Binding {
                target: Value::Local(i),
                type_only: false,
            },
        );
    }
    if !owned.is_empty() {
        for (name, value) in [
            ("exports", Value::Exports),
            ("module", Value::Module),
            ("Object", Value::Builtin),
            ("Reflect", Value::Builtin),
        ] {
            if memchr::memmem::find(source, name.as_bytes()).is_some()
                && find_binding_in_scope(root, source, name, Namespace::Value).is_none()
            {
                bindings.insert(
                    Cow::Borrowed(name),
                    Binding {
                        target: value,
                        type_only: false,
                    },
                );
            }
        }
    }
    for name in ["import", "require"] {
        if memchr::memmem::find(source, name.as_bytes()).is_some()
            && (name == "import"
                || find_binding_in_scope(root, source, name, Namespace::Value).is_none())
        {
            bindings.insert(
                Cow::Borrowed(name),
                Binding {
                    target: Value::Loader,
                    type_only: false,
                },
            );
        }
    }
    local_functions(root, source, &mut bindings);
    let inits = aliases(root, source, &mut bindings);
    let mut owners = HashMap::new();
    let mut builtins = HashSet::new();
    visit_bindings(
        root,
        source,
        &bindings,
        |mut node, mut parents, in_type_query, binding| {
            if parents
                .last()
                .is_some_and(|p| p.kind() == "variable_declarator" && field(*p, "name", node))
                || in_type_query
            {
                return;
            }
            if matches!(binding, Value::Builtin) {
                builtins.insert(node.id());
                return;
            }
            let loader;
            let mut promise = false;
            let binding = if matches!(binding, Value::Loader) {
                let Some((&call, above)) = parents.split_last() else {
                    return;
                };
                if call.kind() != "call_expression" || !field(call, "function", node) {
                    return;
                }
                let Some(module) = loader_call(call, source) else {
                    return;
                };
                loader = Value::Import(module.into_owned(), None);
                promise = node.kind() == "import";
                parents = above;
                node = call;
                if let Some((&p, rest)) = parents.split_last()
                    && p.kind() == "await_expression"
                {
                    node = p;
                    parents = rest;
                    promise = false;
                }
                &loader
            } else {
                binding
            };
            let reference = access(node, parents, source);
            if !promise
                && let Some((&call, outer)) = reference.outer.split_last()
                && field(call, "function", reference.end)
            {
                let function = match (binding, reference.keys.as_slice()) {
                    (Value::Import(module, Some(name)), []) => Some((Some(module), name.as_str())),
                    (Value::Import(module, None), [Some(name)]) => {
                        Some((Some(module), name.as_ref()))
                    }
                    (Value::Function(name), []) => Some((None, name.as_str())),
                    _ => None,
                };
                if let Some((module, name)) = function {
                    let value = access(call, outer, source);
                    let callee = GetterCall {
                        caller: caller.to_owned(),
                        module: module.cloned(),
                        name: name.to_owned(),
                    };
                    if let Some(wrapper) = returned_by(&value, source) {
                        forwards.insert((
                            Getter {
                                file: caller.to_owned(),
                                name: wrapper.to_owned(),
                            },
                            callee,
                        ));
                    } else {
                        let assigned = assignment(&value, source);
                        for c in &mut *candidates {
                            if assigned
                                || can_escape(&value, &c.change.old, &value.keys)
                                || can_escape(&value, &c.change.new, &value.keys)
                            {
                                c.getters.calls.insert(callee.clone());
                            }
                        }
                    }
                }
            }
            let access = reference;
            if inits.contains(&access.end.id()) {
                return;
            }

            let projection = promise.then(|| then_read(&access, source)).flatten();
            let mut keys = access.keys.as_slice();
            let write = assignment(&access, source);
            let mut container = false;
            let indices: &[usize] = match binding {
                Value::Local(i) => std::slice::from_ref(i),
                Value::Import(module, exported) => {
                    let name = if let Some(exported) = exported {
                        Some(exported.as_str())
                    } else if let Some((name, members)) = &projection {
                        keys = members;
                        Some(*name)
                    } else if promise {
                        keys = &[];
                        None
                    } else if let Some((first, rest)) = keys.split_first() {
                        keys = rest;
                        first.as_deref()
                    } else {
                        None
                    };
                    owners
                        .entry((module.clone(), name.map(str::to_owned)))
                        .or_insert_with(|| {
                            candidates
                                .iter()
                                .enumerate()
                                .filter_map(|(i, c)| {
                                    origins
                                        .may_target(caller, module, name, c.path, c.name)
                                        .then_some(i)
                                })
                                .collect::<Vec<_>>()
                        })
                }
                Value::Function(_) => return,
                Value::Loader | Value::Builtin => unreachable!(),
                Value::Exports | Value::Module => {
                    if matches!(binding, Value::Module) {
                        let Some((first, rest)) = keys.split_first() else {
                            return;
                        };
                        if first.as_deref().is_some_and(|s| s != "exports") {
                            return;
                        }
                        keys = rest;
                    } else if keys.is_empty() && write {
                        return;
                    }
                    container = true;
                    &owned
                }
            };
            for &i in indices {
                let c = &mut candidates[i];
                let mut members = keys;
                if container {
                    if let Some((first, rest)) = members.split_first() {
                        if first.as_deref().is_some_and(|n| n != c.name) {
                            continue;
                        }
                        members = rest;
                    } else if excludes_export(&access, source, c.name, &builtins) {
                        continue;
                    }
                }
                if write
                    || promise
                        && projection.is_none()
                        && access
                            .outer
                            .last()
                            .is_some_and(|p| field(*p, "function", access.end))
                {
                    c.change.written = true;
                } else if can_escape(&access, &c.change.old, members)
                    || can_escape(&access, &c.change.new, members)
                {
                    if let Some(getter) = returned_by(&access, source) {
                        c.getters.returns.insert(Getter {
                            file: caller.to_owned(),
                            name: getter.to_owned(),
                        });
                    } else {
                        c.change.written = true;
                    }
                }
            }
        },
    );
}
