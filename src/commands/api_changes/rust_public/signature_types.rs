//! 公開シグネチャから届く型を、module と名前の組で伝播する。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::*;

type PathQuery = (Vec<String>, Vec<String>, bool);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct TypeKey {
    pub(super) module: Vec<String>,
    pub(super) name: String,
}

struct Entry {
    owner: TypeKey,
    nominal: bool,
    seed: bool,
    dependencies: Option<Vec<Vec<String>>>,
    receiver: Option<Vec<String>>,
    fallback: Option<Vec<String>>,
}

#[derive(Default)]
pub(super) struct SignatureTypes {
    declarations: HashSet<TypeKey>,
    definite_declarations: HashSet<TypeKey>,
    imports: Vec<RustPubUseEdge>,
    definite_imports: HashSet<usize>,
    entries: Vec<Entry>,
    opaque: bool,
    shadowable_attributes: HashSet<String>,
    macro_bindings: HashSet<String>,
}

impl SignatureTypes {
    fn observe_expansion(&mut self, item: Node<'_>, source: &[u8]) -> bool {
        match item.kind() {
            "attribute_item" | "inner_attribute_item" => {
                match crate::engine::impact::rust_std_origin::safe_attribute(item, source) {
                    Some(names) => self.shadowable_attributes.extend(names),
                    None => self.opaque = true,
                }
            }
            "macro_definition" => {
                if let Some(name) = item
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                {
                    self.macro_bindings.insert(clean(name).to_owned());
                } else {
                    self.opaque = true;
                }
            }
            "macro_invocation" => self.opaque = true,
            "expression_statement"
                if item
                    .named_child(0)
                    .is_some_and(|node| node.kind() == "macro_invocation") =>
            {
                self.opaque = true
            }
            _ => return false,
        }
        true
    }

    pub(super) fn collect(
        &mut self,
        root: Node<'_>,
        source: &[u8],
        module: &[String],
    ) -> Option<()> {
        let mut pending = vec![(root, module.to_vec())];
        while let Some((scope, module)) = pending.pop() {
            let mut cursor = scope.walk();
            for item in scope.named_children(&mut cursor) {
                if self.observe_expansion(item, source) {
                    continue;
                }
                let name = item
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok());
                let public = rust_node_has_unrestricted_pub_visibility(item, source);
                let generics = generic_names(item, source);
                match item.kind() {
                    "mod_item" => {
                        if let Some(body) = item.child_by_field_name("body") {
                            let mut child = module.clone();
                            child.push(clean(name?).to_owned());
                            pending.push((body, child));
                        }
                    }
                    "use_declaration" => {
                        let first = self.imports.len();
                        expand_rust_use_tree_edges_ast(
                            item.child_by_field_name("argument")?,
                            source,
                            &[],
                            &module,
                            &module,
                            None,
                            &mut self.imports,
                        )?;
                        if item_attributes_are_plain(item, source) {
                            self.definite_imports.extend(first..self.imports.len());
                        }
                    }
                    "foreign_mod_item" => {
                        if let Some(body) = item.child_by_field_name("body") {
                            pending.push((body, module.clone()));
                        }
                    }
                    "struct_item" | "enum_item" | "union_item" | "trait_item" | "type_item" => {
                        let owner = TypeKey {
                            module: module.clone(),
                            name: clean(name?).to_owned(),
                        };
                        self.declarations.insert(owner.clone());
                        if mod_declaration_is_unconditional(item, source) {
                            self.definite_declarations.insert(owner.clone());
                        }
                        let mut parts = header_parts(item);
                        match item.kind() {
                            "struct_item" | "union_item" => {
                                if let Some(body) = item.child_by_field_name("body") {
                                    parts.extend(public_fields(body, source));
                                }
                            }
                            "enum_item" => parts.extend(item.child_by_field_name("body")),
                            "trait_item" => {
                                if let Some(body) = item.child_by_field_name("body") {
                                    let mut cursor = body.walk();
                                    for member in body.named_children(&mut cursor) {
                                        if self.observe_expansion(member, source) {
                                            continue;
                                        }
                                        parts.extend(header_parts(member));
                                    }
                                }
                            }
                            "type_item" => parts.extend(item.child_by_field_name("type")),
                            _ => unreachable!(),
                        }
                        self.entries.push(Entry {
                            owner,
                            nominal: true,
                            seed: public,
                            dependencies: type_paths(parts, source, &generics),
                            receiver: None,
                            fallback: None,
                        });
                    }
                    "function_item" | "function_signature_item" if public => {
                        self.entries.push(Entry {
                            owner: TypeKey {
                                module: module.clone(),
                                name: clean(name?).to_owned(),
                            },
                            seed: true,
                            nominal: false,
                            dependencies: type_paths(header_parts(item), source, &generics),
                            receiver: None,
                            fallback: None,
                        });
                    }
                    "const_item" | "static_item" if public => {
                        self.entries.push(Entry {
                            owner: TypeKey {
                                module: module.clone(),
                                name: clean(name?).to_owned(),
                            },
                            seed: true,
                            nominal: false,
                            dependencies: type_paths(
                                item.child_by_field_name("type").into_iter().collect(),
                                source,
                                &generics,
                            ),
                            receiver: None,
                            fallback: None,
                        });
                    }
                    "impl_item" => {
                        let receiver = type_path(item.child_by_field_name("type")?, source);
                        let trait_path = item
                            .child_by_field_name("trait")
                            .and_then(|node| type_path(node, source));
                        let trait_impl = item.child_by_field_name("trait").is_some();
                        if receiver.is_none() || (trait_impl && trait_path.is_none()) {
                            self.opaque = true;
                            continue;
                        }
                        if let Some(trait_node) = item.child_by_field_name("trait") {
                            self.entries.push(Entry {
                                owner: TypeKey {
                                    module: module.clone(),
                                    name: String::new(),
                                },
                                seed: false,
                                nominal: true,
                                dependencies: type_paths(vec![trait_node], source, &generics),
                                receiver: receiver.clone(),
                                fallback: trait_path.clone(),
                            });
                        }
                        if let Some(body) = item.child_by_field_name("body") {
                            let mut cursor = body.walk();
                            for member in body.named_children(&mut cursor) {
                                if self.observe_expansion(member, source) {
                                    continue;
                                }
                                if !matches!(
                                    member.kind(),
                                    "function_item" | "type_item" | "const_item"
                                ) || !(trait_impl
                                    || rust_node_has_unrestricted_pub_visibility(member, source))
                                {
                                    continue;
                                }
                                let mut names = generics.clone();
                                names.extend(generic_names(member, source));
                                // 関連定数は型が外から見える (値の式は見えない。Issue #315)。
                                let mut parts = if member.kind() == "const_item" {
                                    member.child_by_field_name("type").into_iter().collect()
                                } else {
                                    header_parts(member)
                                };
                                // impl の対象の型の引数も、`Self` を通して届く (`impl Public<types::S>` の
                                // `fn new() -> Self`。Issue #315)。
                                parts.extend(item.child_by_field_name("type"));
                                self.entries.push(Entry {
                                    owner: TypeKey {
                                        module: module.clone(),
                                        name: String::new(),
                                    },
                                    seed: false,
                                    nominal: true,
                                    dependencies: type_paths(parts, source, &names),
                                    receiver: receiver.clone(),
                                    fallback: trait_path.clone(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Some(())
    }

    pub(super) fn reachable(
        self,
        modules: &HashMap<Vec<String>, Vec<String>>,
        definite_modules: &HashMap<Vec<String>, bool>,
        edition: Option<crate::engine::rust_edition::RustEdition>,
        aliases: Option<HashSet<String>>,
        public: impl Fn(&[String], &str) -> bool,
    ) -> (HashSet<TypeKey>, bool) {
        let resolver = Resolver {
            declarations: &self.declarations,
            definite_declarations: &self.definite_declarations,
            imports: &self.imports,
            definite_imports: &self.definite_imports,
            modules,
            definite_modules,
            edition,
            aliases,
        };
        let mut dependencies: HashMap<TypeKey, Vec<Option<Vec<TypeKey>>>> = HashMap::new();
        let mut reachable = HashSet::new();
        let mut unknown = self.opaque
            || self
                .macro_bindings
                .iter()
                .any(|name| self.shadowable_attributes.contains(name));
        if !self.shadowable_attributes.is_empty() {
            for import in &self.imports {
                match import {
                    RustPubUseEdge::Named { exported_name, .. } => {
                        unknown |= self.shadowable_attributes.contains(clean(exported_name));
                    }
                    RustPubUseEdge::Wildcard {
                        source_module,
                        target_module,
                        origin,
                    } => {
                        let mut target: Vec<_> = target_module
                            .iter()
                            .map(|part| clean(part).to_owned())
                            .collect();
                        let local = match origin {
                            UsePathOrigin::Anchored => true,
                            UsePathOrigin::Bare => match edition {
                                Some(crate::engine::rust_edition::RustEdition::E2015) => true,
                                Some(crate::engine::rust_edition::RustEdition::E2018OrLater) => {
                                    target.splice(0..0, source_module.iter().cloned());
                                    true
                                }
                                None => false,
                            },
                            UsePathOrigin::Global => match edition {
                                Some(crate::engine::rust_edition::RustEdition::E2015) => true,
                                Some(crate::engine::rust_edition::RustEdition::E2018OrLater)
                                    if resolver.aliases.as_ref().is_some_and(|aliases| {
                                        target.first().is_some_and(|name| aliases.contains(name))
                                    }) =>
                                {
                                    target.remove(0);
                                    true
                                }
                                _ => false,
                            },
                        };
                        unknown |= !local || !modules.contains_key(&target);
                    }
                }
            }
        }
        for entry in &self.entries {
            let owners = match &entry.receiver {
                Some(path) => match resolver.resolve(&entry.owner.module, path) {
                    Ok(owners) if owners.is_empty() => {
                        if let Some(fallback) = &entry.fallback {
                            match resolver.resolve(&entry.owner.module, fallback) {
                                Ok(owners) => owners,
                                Err(()) => {
                                    unknown = true;
                                    continue;
                                }
                            }
                        } else {
                            owners
                        }
                    }
                    Ok(owners) => owners,
                    Err(()) => {
                        unknown = true;
                        continue;
                    }
                },
                None => vec![entry.owner.clone()],
            };
            for owner in owners {
                let mut resolved = Vec::new();
                let known = entry.dependencies.as_ref().is_some_and(|paths| {
                    paths.iter().all(|path| {
                        if path.as_slice() == ["Self"] {
                            resolved.push(owner.clone());
                            true
                        } else {
                            match resolver.resolve(&entry.owner.module, path) {
                                Ok(types) => {
                                    resolved.extend(types);
                                    true
                                }
                                Err(()) => false,
                            }
                        }
                    })
                });
                if !entry.nominal {
                    if entry.seed && public(&owner.module, &owner.name) {
                        if known {
                            reachable.extend(resolved);
                        } else {
                            unknown = true;
                        }
                    }
                    continue;
                }
                dependencies
                    .entry(owner.clone())
                    .or_default()
                    .push(known.then_some(resolved));
                if entry.seed && public(&owner.module, &owner.name) {
                    reachable.insert(owner);
                }
            }
        }
        let mut pending: Vec<_> = reachable.iter().cloned().collect();
        while let Some(owner) = pending.pop() {
            for paths in dependencies.get(&owner).into_iter().flatten() {
                let Some(paths) = paths else {
                    unknown = true;
                    continue;
                };
                for target in paths {
                    if reachable.insert(target.clone()) {
                        pending.push(target.clone());
                    }
                }
            }
        }
        (reachable, unknown)
    }
}

struct Resolver<'a> {
    declarations: &'a HashSet<TypeKey>,
    definite_declarations: &'a HashSet<TypeKey>,
    imports: &'a [RustPubUseEdge],
    definite_imports: &'a HashSet<usize>,
    modules: &'a HashMap<Vec<String>, Vec<String>>,
    definite_modules: &'a HashMap<Vec<String>, bool>,
    edition: Option<crate::engine::rust_edition::RustEdition>,
    aliases: Option<HashSet<String>>,
}

impl Resolver<'_> {
    fn resolve(&self, scope: &[String], path: &[String]) -> Result<Vec<TypeKey>, ()> {
        let Some(initial) = self.anchored(scope, path)? else {
            return Ok(Vec::new());
        };
        let mut pending = vec![initial];
        let mut visited = HashSet::new();
        let mut imports_used = HashSet::new();
        let mut result = HashSet::new();
        while let Some((scope, path, required)) = pending.pop() {
            if !visited.insert((scope.clone(), path.clone(), required)) {
                return Err(());
            }
            let (first, tail) = path.split_first().ok_or(())?;
            let key = TypeKey {
                module: scope.clone(),
                name: first.clone(),
            };
            let mut bound = false;
            let mut definite = self.definite_declarations.contains(&key);
            if self.declarations.contains(&key) {
                if !tail.is_empty() {
                    return Err(());
                }
                result.insert(key);
                bound = true;
            }
            let mut child = scope.clone();
            child.push(first.clone());
            if self.modules.contains_key(&child) {
                definite |= self.definite_modules.get(&child) == Some(&true);
                if tail.is_empty() {
                    return Err(());
                }
                pending.push((child, tail.to_vec(), required));
                bound = true;
            }
            definite |= self.imports.iter().enumerate().any(|(index, import)| {
                self.definite_imports.contains(&index) && matches!(import, RustPubUseEdge::Named { source_module, exported_name, .. } if source_module == &scope && clean(exported_name) == first)
            });
            for (index, import) in self.imports.iter().enumerate() {
                if definite && matches!(import, RustPubUseEdge::Wildcard { .. }) {
                    continue;
                }
                let (owner, target, item) = match import {
                    RustPubUseEdge::Named {
                        source_module,
                        exported_name,
                        target_module,
                        target_item,
                        ..
                    } if clean(exported_name) == first => {
                        (source_module, target_module, Some(target_item))
                    }
                    RustPubUseEdge::Wildcard {
                        source_module,
                        target_module,
                        ..
                    } => (source_module, target_module, None),
                    _ => continue,
                };
                if owner != &scope {
                    continue;
                }
                if !imports_used.insert(index) {
                    return Err(());
                }
                let mut target: Vec<_> = target.iter().map(|part| clean(part).to_owned()).collect();
                if let Some(item) = item {
                    target.push(clean(item).to_owned());
                    target.extend_from_slice(tail);
                } else {
                    target.extend_from_slice(&path);
                }
                let anchored = match import.origin() {
                    UsePathOrigin::Anchored => Some((Vec::new(), target, true)),
                    UsePathOrigin::Global => {
                        target.insert(0, "::".to_owned());
                        self.anchored(owner, &target)?
                    }
                    UsePathOrigin::Bare => {
                        use crate::engine::rust_edition::RustEdition;
                        let module = match self.edition {
                            Some(RustEdition::E2015) => Vec::new(),
                            Some(RustEdition::E2018OrLater) => owner.clone(),
                            None => return Err(()),
                        };
                        Some((module, target, false))
                    }
                };
                if let Some(target) = anchored {
                    pending.push(target);
                }
                bound = true;
            }
            if !bound && required {
                return Err(());
            }
        }
        Ok(result.into_iter().collect())
    }

    fn anchored(&self, scope: &[String], path: &[String]) -> Result<Option<PathQuery>, ()> {
        let mut scope = scope.to_vec();
        let mut path = path.to_vec();
        let mut required = false;
        match path.first().map(String::as_str) {
            Some("crate") => {
                scope.clear();
                path.remove(0);
                required = true;
            }
            Some("self") => {
                path.remove(0);
                required = true;
            }
            Some("super") => {
                while path.first().is_some_and(|part| part == "super") {
                    scope.pop().ok_or(())?;
                    path.remove(0);
                }
                required = true;
            }
            Some("::") => {
                use crate::engine::rust_edition::RustEdition;
                scope.clear();
                path.remove(0);
                match self.edition {
                    Some(RustEdition::E2015) => {}
                    Some(RustEdition::E2018OrLater) => {
                        if !self
                            .aliases
                            .as_ref()
                            .ok_or(())?
                            .contains(path.first().ok_or(())?)
                        {
                            return Ok(None);
                        }
                        path.remove(0);
                    }
                    None => return Err(()),
                }
                required = true;
            }
            _ => {}
        }
        if path.is_empty() {
            return Err(());
        }
        Ok(Some((scope, path, required)))
    }
}

fn clean(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

fn generic_names(item: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut names = HashSet::new();
    if let Some(parameters) = item.child_by_field_name("type_parameters") {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if let Some(name) = parameter
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
            {
                names.insert(clean(name).to_owned());
            }
        }
    }
    names
}

fn header_parts(item: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = item.walk();
    item.named_children(&mut cursor)
        .filter(|child| {
            !matches!(
                child.kind(),
                "block"
                    | "declaration_list"
                    | "field_declaration_list"
                    | "ordered_field_declaration_list"
                    | "enum_variant_list"
                    | "visibility_modifier"
            ) && item
                .child_by_field_name("name")
                .is_none_or(|name| name.id() != child.id())
        })
        .collect()
}

fn public_fields<'a>(body: Node<'a>, source: &[u8]) -> Vec<Node<'a>> {
    let mut cursor = body.walk();
    let mut fields = Vec::new();
    let mut public = false;
    for child in body.named_children(&mut cursor) {
        if child.kind() == "field_declaration" {
            if rust_node_has_unrestricted_pub_visibility(child, source) {
                fields.extend(child.child_by_field_name("type"));
            }
        } else if child.kind() == "visibility_modifier" {
            public = child.utf8_text(source).ok() == Some("pub");
        } else if !matches!(
            child.kind(),
            "attribute_item" | "line_comment" | "block_comment"
        ) {
            if public {
                fields.push(child);
            }
            public = false;
        }
    }
    fields
}

fn type_paths(
    parts: Vec<Node<'_>>,
    source: &[u8],
    generics: &HashSet<String>,
) -> Option<Vec<Vec<String>>> {
    let mut pending = parts;
    let mut paths = Vec::new();
    while let Some(node) = pending.pop() {
        match node.kind() {
            "scoped_type_identifier" | "type_identifier" => {
                let path = type_path(node, source)?;
                if !(path.len() == 1 && generics.contains(&path[0])) {
                    paths.push(path);
                }
            }
            "macro_invocation" | "qualified_type" => return None,
            "block" => {}
            _ => {
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
        }
    }
    Some(paths)
}

fn type_path(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    let mut pending = vec![node];
    let mut path = Vec::new();
    while let Some(node) = pending.pop() {
        match node.kind() {
            "generic_type" => pending.push(node.child_by_field_name("type")?),
            "reference_type" => pending.push(node.child_by_field_name("type")?),
            "scoped_type_identifier" | "scoped_identifier" => {
                let name = node.child_by_field_name("name")?;
                pending.push(name);
                if let Some(prefix) = node.child_by_field_name("path") {
                    pending.push(prefix);
                } else {
                    path.push("::".to_owned());
                }
            }
            "type_identifier" | "identifier" | "primitive_type" | "crate" | "self" | "super" => {
                path.push(clean(node.utf8_text(source).ok()?).to_owned())
            }
            _ => return None,
        }
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_signature_types_are_conservative_only_when_reached() {
        for (source, unknown) in [
            (
                "type Hidden = unknown!(); pub fn current() -> u32 { 1 }",
                false,
            ),
            (
                "type Hidden = unknown!(); pub fn current() -> Hidden { loop {} }",
                true,
            ),
            (
                "type First = Second; type Second = First; pub fn current() -> First { loop {} }",
                false,
            ),
            (
                "pub fn current() -> <Missing as Trait>::Item { loop {} }",
                true,
            ),
            (
                "#[derive(Debug)] struct Hidden; pub fn current() -> u32 { 1 }",
                false,
            ),
            (
                "#[derive(Custom)] struct Hidden; pub fn current() -> u32 { 1 }",
                true,
            ),
            (
                "use custom::Debug; #[derive(Debug)] struct Hidden; pub fn current() -> u32 { 1 }",
                true,
            ),
            (
                "pub struct Root; impl Root { #[custom] pub fn value() -> u32 { 1 } } pub fn current() -> Root { Root }",
                true,
            ),
            (
                "pub struct Root; impl Root { #[inline] pub fn value() -> u32 { 1 } } pub fn current() -> Root { Root }",
                false,
            ),
        ] {
            let tree =
                parser::parse_source(source.as_bytes(), crate::language::LangId::Rust).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            let mut types = SignatureTypes::default();
            types
                .collect(tree.root_node(), source.as_bytes(), &[])
                .unwrap();
            let modules = HashMap::from([(Vec::new(), Vec::new())]);
            let definite_modules = HashMap::from([(Vec::new(), true)]);
            let (_, actual) = types.reachable(
                &modules,
                &definite_modules,
                Some(crate::engine::rust_edition::RustEdition::E2018OrLater),
                Some(HashSet::new()),
                |module, name| module.is_empty() && name == "current",
            );
            assert_eq!(actual, unknown, "{source}");
        }
    }
}
