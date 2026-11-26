//! 移動先のモジュールに元からある参照が、新しく入る同名の宣言を読むか確かめる。

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;

use tree_sitter::Node;

use super::moved_members::MemberFileFacts;
use super::prepare::DetectionInputs;
use super::removed_attribution::{normalize_relative_specifier, relative_import_candidates};
use super::ts_alias_resolution::{BaseTree, ConfigFs};
use crate::engine::js_binding_pattern::visit_pattern_bindings;
use crate::engine::js_scope::{Namespace, is_scope_node};
use crate::engine::js_string::decoded;
use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;
use crate::models::review::{ApiSymbolChange, MovedSymbol};

fn applies(kind: &str, space: Namespace) -> bool {
    match kind {
        "function" | "constant" | "variable" => space == Namespace::Value,
        "type" | "interface" | "trait" => space == Namespace::Type,
        "class" | "enum" | "module" => true,
        _ => false,
    }
}

type Name = (String, Namespace);

#[derive(Default)]
struct Scope {
    outer: usize,
    bound: HashSet<Name>,
}

struct ModuleUses {
    declared: HashSet<Name>,
    exposed: HashSet<Name>,
}

fn uses(root: Node<'_>, source: &[u8], names: &HashSet<String>) -> ModuleUses {
    let mut scopes = vec![Scope::default()];
    let mut active = vec![(0, 0)];
    let mut ancestors: Vec<Node<'_>> = Vec::new();
    let mut bindings = HashSet::new();
    let mut sites = HashSet::new();
    let _ = walk_preorder(root, Children::Named, |node, depth| {
        ancestors.truncate(depth);
        while active.len() > 1 && active.last().is_some_and(|&(d, _)| d >= depth) {
            active.pop();
        }
        let parent = ancestors.last().copied();
        ancestors.push(node);
        let kind = node.kind();
        if kind == "import_statement"
            || (kind == "export_statement" && node.child_by_field_name("source").is_some())
        {
            return ControlFlow::Continue(Walk::Skip);
        }
        let mut outer = active.last().unwrap().1;
        // メソッドの計算名と decorator の式は、そのメソッドの引数を束縛しない。
        if kind == "computed_property_name"
            && parent.is_some_and(|p| p.kind() == "method_definition")
        {
            outer = scopes[outer].outer;
            active.push((depth, outer));
        } else if kind == "decorator" {
            outer = 0;
            active.push((depth, outer));
        }
        let scoped = depth > 0
            && (is_scope_node(kind)
                || node.child_by_field_name("type_parameters").is_some()
                || matches!(
                    kind,
                    "class"
                        | "class_declaration"
                        | "abstract_class_declaration"
                        | "function_signature"
                        | "method_signature"
                        | "abstract_method_signature"
                        | "function_type"
                        | "constructor_type"
                        | "construct_signature"
                        | "call_signature"
                        | "index_signature"
                        | "internal_module"
                        | "module"
                ));
        let inner = if scoped {
            let id = scopes.len();
            scopes.push(Scope {
                outer,
                ..Scope::default()
            });
            active.push((depth, id));
            id
        } else {
            outer
        };
        let mut bind = |id: usize, slot: Node<'_>, space| {
            if let Ok(raw) = slot.utf8_text(source)
                && let Some(name) = decoded(raw)
                && names.contains(name.as_ref())
            {
                bindings.insert(slot.id());
                scopes[id].bound.insert((name.into_owned(), space));
            }
        };
        if let Some(slot) = node.child_by_field_name("name") {
            match kind {
                "function_declaration"
                | "generator_function_declaration"
                | "function_signature" => bind(outer, slot, Namespace::Value),
                "function_expression" | "generator_function" => bind(inner, slot, Namespace::Value),
                "interface_declaration" | "type_alias_declaration" => {
                    bind(outer, slot, Namespace::Type)
                }
                "class_declaration"
                | "abstract_class_declaration"
                | "enum_declaration"
                | "internal_module"
                | "module" => {
                    bind(outer, slot, Namespace::Value);
                    bind(outer, slot, Namespace::Type);
                }
                "class" => {
                    bind(inner, slot, Namespace::Value);
                    bind(inner, slot, Namespace::Type);
                }
                "type_parameter" => bind(inner, slot, Namespace::Type),
                "index_signature" => bind(inner, slot, Namespace::Value),
                _ => {}
            }
        }
        let pattern = match kind {
            "variable_declarator" => node.child_by_field_name("name"),
            "required_parameter" | "optional_parameter" => node.child_by_field_name("pattern"),
            "arrow_function" | "catch_clause" => node.child_by_field_name("parameter"),
            "for_in_statement" if node.child_by_field_name("kind").is_some() => {
                node.child_by_field_name("left")
            }
            _ => None,
        };
        if let Some(pattern) = pattern {
            let _ = visit_pattern_bindings(pattern, &mut |n| {
                bind(inner, n, Namespace::Value);
                ControlFlow::Continue(())
            });
        }
        let space = match kind {
            "type_identifier" => Namespace::Type,
            "undefined" if parent.is_some_and(|p| p.kind() != "literal_type") => Namespace::Value,
            "identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern" => Namespace::Value,
            _ => return ControlFlow::Continue(Walk::Descend),
        };
        if !bindings.contains(&node.id())
            && let Ok(raw) = node.utf8_text(source)
            && let Some(name) = decoded(raw)
            && names.contains(name.as_ref())
            && let Some(parent) = parent
        {
            let field_is = |field| {
                parent
                    .child_by_field_name(field)
                    .is_some_and(|n| n.id() == node.id())
            };
            let property = (parent.kind() == "nested_type_identifier" && field_is("name"))
                || (parent.kind() == "export_specifier" && field_is("alias"))
                || (matches!(
                    parent.kind(),
                    "jsx_opening_element" | "jsx_closing_element" | "jsx_self_closing_element"
                ) && field_is("name")
                    && name.starts_with(|c: char| c.is_ascii_lowercase()));
            if !property {
                sites.insert((inner, (name.into_owned(), space)));
            }
        }
        ControlFlow::<(), Walk>::Continue(Walk::Descend)
    });
    let mut exposed = HashSet::new();
    let mut cache = HashMap::<Name, HashMap<usize, bool>>::new();
    for (mut scope, name) in sites {
        if exposed.contains(&name) {
            continue;
        }
        let visible = cache.entry(name.clone()).or_default();
        let mut traversed = Vec::new();
        let reaches_module = loop {
            if scope == 0 {
                break true;
            }
            if let Some(&known) = visible.get(&scope) {
                break known;
            }
            traversed.push(scope);
            if scopes[scope].bound.contains(&name) {
                break false;
            }
            scope = scopes[scope].outer;
        };
        visible.extend(traversed.into_iter().map(|id| (id, reaches_module)));
        if reaches_module {
            exposed.insert(name);
        }
    }
    ModuleUses {
        declared: std::mem::take(&mut scopes[0].bound),
        exposed,
    }
}

fn import_target(root: &Path, fs: &ConfigFs, from: &str, module: &str) -> Option<String> {
    let base = normalize_relative_specifier(from, module)?;
    if module.contains(['\\', ':', '?', '#', '%'])
        || !crate::engine::impact::is_safe_diff_path(&base)
        || base
            .split('/')
            .any(|part| part.eq_ignore_ascii_case("node_modules"))
        || fs.is_regular_file(&root.join(base).join("package.json"))?
    {
        return None;
    }
    let mut found = None;
    for path in relative_import_candidates(from, module)? {
        if fs.is_regular_file(&root.join(&path))? && found.replace(path).is_some() {
            return None;
        }
    }
    found
}

fn parsed<T>(path: &str, source: &[u8], inspect: impl FnOnce(Node<'_>) -> T) -> Option<T> {
    let (tree, _) = parser::parse_file(camino::Utf8Path::new(path), source).ok()?;
    let root = tree.root_node();
    (!root.has_error()).then(|| inspect(root))
}

pub(super) fn verify(
    inputs: &DetectionInputs<'_>,
    files: &MemberFileFacts<'_>,
    moved: Vec<MovedSymbol>,
    modified: &mut Vec<ApiSymbolChange>,
) -> Vec<MovedSymbol> {
    let mut destinations = HashMap::<&str, Vec<usize>>::new();
    for (i, symbol) in moved.iter().enumerate() {
        if symbol.from != symbol.to
            && !symbol.name.contains('.')
            && (applies(&symbol.kind, Namespace::Type) || applies(&symbol.kind, Namespace::Value))
            && matches!(
                LangId::from_path(camino::Utf8Path::new(&symbol.to)),
                Ok(LangId::Typescript | LangId::Tsx)
            )
            && files.old_path_of.contains_key(symbol.to.as_str())
        {
            destinations.entry(&symbol.to).or_default().push(i);
        }
    }
    let mut blocked = HashSet::new();
    let old_files = std::cell::OnceCell::new();
    for (path, indices) in destinations {
        let old_path = files.old_path_of[path];
        let names = indices
            .iter()
            .filter_map(|&i| decoded(&moved[i].name).map(|n| n.into_owned()))
            .collect();
        let old = inputs
            .base_blobs
            .read_limited(old_path, parser::MAX_FILE_SIZE as usize)
            .and_then(|source| {
                parsed(old_path, &source, |root| {
                    (
                        uses(root, &source, &names),
                        super::ts_export_bindings::same_name_imports(root, &source),
                    )
                })
            });
        let Some((before, imports)) = old else {
            blocked.extend(indices);
            continue;
        };
        if before.exposed.is_empty() {
            continue;
        }
        let Some(after) = super::load_new_source(inputs.dir, path)
            .and_then(|source| parsed(path, &source, |root| uses(root, &source, &names)))
        else {
            blocked.extend(indices);
            continue;
        };
        let mut resolved = HashMap::new();
        for i in indices {
            let symbol = &moved[i];
            let Some(name) = decoded(&symbol.name) else {
                blocked.insert(i);
                continue;
            };
            for space in [Namespace::Value, Namespace::Type] {
                let key = (name.to_string(), space);
                if !applies(&symbol.kind, space)
                    || !before.exposed.contains(&key)
                    || !after.exposed.contains(&key)
                    || !after.declared.contains(&key)
                {
                    continue;
                }
                let same = imports
                    .get(name.as_ref())
                    .is_some_and(|(module, type_only)| {
                        if *type_only && space == Namespace::Value {
                            return false;
                        }
                        resolved
                            .entry(module)
                            .or_insert_with(|| {
                                let (root, fs) = old_files
                                    .get_or_init(|| {
                                        BaseTree::for_relative_imports(inputs.checkout, inputs.base)
                                    })
                                    .as_ref()?;
                                import_target(root, fs, old_path, module)
                            })
                            .as_deref()
                            == Some(&symbol.from)
                    });
                if !same {
                    blocked.insert(i);
                    break;
                }
            }
        }
    }
    moved
        .into_iter()
        .enumerate()
        .filter_map(|(i, symbol)| {
            if !blocked.contains(&i) {
                return Some(symbol);
            }
            let signature = files
                .old
                .get(symbol.from.as_str())
                .zip(files.new.get(symbol.to.as_str()))
                .and_then(|(old, (new, _))| {
                    old.iter().find(|entry| {
                        entry.0 == symbol.name && entry.1 == symbol.kind && new.contains(entry)
                    })
                })
                .map(|(_, _, signature)| signature.clone());
            modified.push(ApiSymbolChange::new(
                symbol.name,
                symbol.kind,
                symbol.to,
                signature.clone(),
                signature,
            ));
            None
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_capture_respects_lexical_namespaces_and_bindings() {
        let cases = [
            (
                "function f(x: Date) { return globalThis.Date; }",
                false,
                true,
            ),
            ("function f() { return {Date}; }", true, false),
            (r"function f() { return D\u0061te; }", true, false),
            ("function f() { return () => new Date(); }", true, false),
            ("function f() { return {Date: 1}.Date; }", false, false),
            (
                "function f({Date}: {Date: number}) { return () => Date; }",
                false,
                false,
            ),
            (
                "function f() { { const Date = 1; } return Date; }",
                true,
                false,
            ),
            (
                "function f(x = Date) { const Date = 1; return x; }",
                true,
                false,
            ),
            (
                "function f() { let call: (Date: number) => number; return Date; }",
                true,
                false,
            ),
            (
                "class C { [Date.name](Date: number) { return Date; } }",
                true,
                false,
            ),
            ("type X = typeof Date;", true, false),
            ("type X = NS.Date;", false, false),
            (
                "function f<Date>(p: Date): Date { return p; }",
                false,
                false,
            ),
        ];
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (source, value, ty) in cases {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{source}");
                let names = HashSet::from(["Date".to_owned()]);
                let result = uses(tree.root_node(), source.as_bytes(), &names);
                assert_eq!(
                    result.exposed.contains(&("Date".into(), Namespace::Value)),
                    value,
                    "{lang:?}: {source}"
                );
                assert_eq!(
                    result.exposed.contains(&("Date".into(), Namespace::Type)),
                    ty,
                    "{lang:?}: {source}"
                );
            }
        }
    }
}
