//! 同じファイルの型宣言の変更を、公開ヘッダの依存先へ伝える。

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use crate::models::change_evidence::{ChangeEvidence, ChangedPart};
use tree_sitter::Node;

use super::super::signature_tokens::{SigTokens, signature_tokens_in_range};
use super::type_dependencies::{
    DependencyEnvironment, Member, Reference, collect_references_in_scope, local_type_values,
    references_unchanged,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DependencyStatus {
    Unchanged,
    Changed,
    #[default]
    Unknown,
}

#[derive(Default)]
pub(crate) struct DependencyChanges {
    statuses: BTreeMap<String, DependencyStatus>,
    positions: BTreeMap<String, BTreeSet<ChangedPart>>,
    old_references: BTreeMap<String, Option<Vec<Reference>>>,
    new_references: BTreeMap<String, Option<Vec<Reference>>>,
}

impl DependencyChanges {
    pub(crate) fn annotate(&self, name: &str, evidence: &mut ChangeEvidence) {
        if let Some(parts) = self.positions.get(name) {
            evidence.add_type_parts(parts.iter().copied());
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&DependencyStatus> {
        self.statuses.get(name)
    }

    fn references(&self, name: &str, reason: &str) -> Option<Vec<Reference>> {
        let references = if reason == "trailing_optional_params_removed" {
            &self.new_references
        } else {
            &self.old_references
        };
        references.get(name)?.clone()
    }
}

#[derive(Clone, Default)]
struct References {
    names: BTreeSet<Reference>,
    positions: BTreeSet<(ChangedPart, Reference)>,
    unknown: bool,
}

impl References {
    fn add(&mut self, node: Node<'_>, source: &[u8], bindings: &[String]) {
        let mut references = Vec::new();
        self.unknown |=
            collect_references_in_scope(node, source, &mut references, bindings).is_none();
        self.names.extend(references);
    }

    fn extend(&mut self, other: Self) {
        self.unknown |= other.unknown;
        self.names.extend(other.names);
        self.positions.extend(other.positions);
    }
}

struct LocalType {
    kind: String,
    tokens: Vec<SigTokens>,
    references: References,
}

impl Reference {
    fn whole(name: String) -> Self {
        Self { name, member: None }
    }

    fn graph_key(&self) -> Cow<'_, Self> {
        if matches!(self.member, Some(Member::Qualified(_))) {
            Cow::Owned(Self::whole(self.name.clone()))
        } else {
            Cow::Borrowed(self)
        }
    }
}

#[derive(Default)]
pub(crate) struct DependencySnapshot {
    types: BTreeMap<Reference, Option<LocalType>>,
    declarations: BTreeMap<String, References>,
    exported: BTreeSet<String>,
    unknown: bool,
}

fn plain(node: Node<'_>, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?;
    (!text.is_empty() && !text.contains('\\')).then(|| text.to_owned())
}

fn bindings(node: Node<'_>, source: &[u8], outer: &[String]) -> Vec<String> {
    let mut names = outer.to_vec();
    if let Some(parameters) = node.child_by_field_name("type_parameters") {
        let mut cursor = parameters.walk();
        names.extend(
            parameters
                .named_children(&mut cursor)
                .filter_map(|parameter| plain(parameter.child_by_field_name("name")?, source)),
        );
    }
    names
}

fn type_tokens(node: Node<'_>, source: &[u8], end: usize) -> Option<SigTokens> {
    let mut tokens = signature_tokens_in_range(node, source, node.start_byte(), end)?;
    tokens.0.retain(|(kind, _)| kind != "comment");
    Some(tokens)
}

fn header_references(node: Node<'_>, source: &[u8], outer: &[String]) -> References {
    let names = bindings(node, source, outer);
    let mut references = References::default();
    for field in [
        "type_parameters",
        "parameters",
        "return_type",
        "type",
        "index_type",
    ] {
        if let Some(root) = node.child_by_field_name(field) {
            let mut section = References::default();
            section.add(root, source, &names);
            let part = match field {
                "parameters" => Some(ChangedPart::ParamAnnotation),
                "return_type" => Some(ChangedPart::ReturnAnnotation),
                _ => None,
            };
            if let Some(part) = part {
                references.positions.extend(
                    section
                        .names
                        .iter()
                        .cloned()
                        .map(|reference| (part, reference)),
                );
            }
            references.extend(section);
        }
    }
    references
}

fn indexed_properties<'tree>(
    body: Node<'tree>,
    source: &[u8],
) -> Option<HashMap<String, Node<'tree>>> {
    if !matches!(body.kind(), "object_type" | "interface_body") {
        return None;
    }
    let mut properties = HashMap::new();
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor).filter(|n| !n.is_extra()) {
        if member.kind() != "property_signature" {
            return None;
        }
        let name = super::object_key_text(member.child_by_field_name("name")?, source)?;
        if name.contains('\\') || properties.insert(name, member).is_some() {
            return None;
        }
    }
    Some(properties)
}

impl DependencySnapshot {
    fn references(&self, name: &str) -> Option<Vec<Reference>> {
        let references = self.declarations.get(name)?;
        (!self.unknown && !references.unknown).then(|| references.names.iter().cloned().collect())
    }
    pub(crate) fn collect(root: Node<'_>, source: &[u8]) -> Self {
        let mut snapshot = Self {
            unknown: root.has_error(),
            ..Self::default()
        };
        let mut cursor = root.walk();
        for statement in root.named_children(&mut cursor) {
            let declaration = if statement.kind() == "export_statement" {
                let Some(declaration) = statement.child_by_field_name("declaration") else {
                    if statement.child_by_field_name("source").is_none() {
                        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
                            statement,
                            crate::engine::tree_walk::Children::Named,
                            |node, _| {
                                if node.kind() == "export_specifier"
                                    && let Some(name) = node
                                        .child_by_field_name("name")
                                        .and_then(|name| plain(name, source))
                                {
                                    snapshot.exported.insert(name);
                                }
                                std::ops::ControlFlow::Continue(
                                    crate::engine::tree_walk::Walk::Descend,
                                )
                            },
                        );
                    }
                    continue;
                };
                if matches!(
                    declaration.kind(),
                    "type_alias_declaration"
                        | "interface_declaration"
                        | "enum_declaration"
                        | "class_declaration"
                        | "abstract_class_declaration"
                ) && let Some(name) = declaration
                    .child_by_field_name("name")
                    .and_then(|name| plain(name, source))
                {
                    snapshot.exported.insert(name);
                }
                declaration
            } else {
                statement
            };
            let name = declaration
                .child_by_field_name("name")
                .and_then(|name| plain(name, source));
            match declaration.kind() {
                "type_alias_declaration" | "interface_declaration" | "enum_declaration" => {
                    let Some(name) = name else {
                        snapshot.unknown = true;
                        continue;
                    };
                    let mut references = References::default();
                    if declaration.kind() != "enum_declaration" {
                        references.add(declaration, source, &[]);
                    } else if let Some(body) = declaration.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        references.unknown |= body.named_children(&mut cursor).any(|member| {
                            member.child_by_field_name("value").is_some_and(|value| {
                                !matches!(value.kind(), "string" | "number")
                                    && !(value.kind() == "unary_expression"
                                        && value
                                            .named_child(0)
                                            .is_some_and(|operand| operand.kind() == "number"))
                            })
                        });
                    }
                    snapshot.insert_declaration(name.clone(), references.clone());
                    let fact =
                        type_tokens(declaration, source, declaration.end_byte()).map(|tokens| {
                            LocalType {
                                kind: declaration.kind().to_owned(),
                                tokens: vec![tokens],
                                references,
                            }
                        });
                    match snapshot.types.entry(Reference::whole(name)) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(fact);
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            match (entry.get_mut().as_mut(), fact) {
                                (Some(previous), Some(next))
                                    if previous.kind == "interface_declaration"
                                        && previous.kind == next.kind =>
                                {
                                    previous.tokens.extend(next.tokens);
                                    previous.references.extend(next.references);
                                }
                                _ => {
                                    entry.insert(None);
                                }
                            }
                        }
                    }
                }
                "function_declaration"
                | "generator_function_declaration"
                | "function_signature" => {
                    if let Some(name) = name {
                        snapshot
                            .insert_declaration(name, header_references(declaration, source, &[]));
                    }
                }
                "class_declaration" | "abstract_class_declaration" => {
                    if let Some(name) = name {
                        snapshot.collect_class(declaration, source, &name);
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    let mut cursor = declaration.walk();
                    for variable in declaration
                        .named_children(&mut cursor)
                        .filter(|n| n.kind() == "variable_declarator")
                    {
                        let Some(name) = variable
                            .child_by_field_name("name")
                            .filter(|n| n.kind() == "identifier")
                            .and_then(|n| plain(n, source))
                        else {
                            continue;
                        };
                        let references = if let Some(annotation) =
                            variable.child_by_field_name("type")
                        {
                            let mut references = References::default();
                            references.add(annotation, source, &[]);
                            references
                        } else if let Some(function) = variable
                            .child_by_field_name("value")
                            .and_then(super::callable::unwrap_parentheses)
                            .and_then(|value| {
                                if matches!(value.kind(), "arrow_function" | "function_expression")
                                {
                                    Some(value)
                                } else {
                                    // `react` の `memo()` の公開型は、包んだ関数の型から作る
                                    // (Issue #266)。
                                    super::react_memo::react_memo_function(root, source, value)
                                }
                            })
                        {
                            header_references(function, source, &[])
                        } else {
                            References {
                                unknown: true,
                                ..References::default()
                            }
                        };
                        snapshot.insert_declaration(name, references);
                    }
                }
                "ambient_declaration" | "internal_module" | "module" | "import_alias" => {
                    snapshot.unknown = true
                }
                _ => {}
            }
        }
        snapshot.collect_indexed_members(root, source);
        snapshot
    }

    fn insert_declaration(&mut self, name: String, references: References) {
        match self.declarations.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(references);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().unknown = true;
                entry.get_mut().extend(references);
            }
        }
    }

    /// 同じファイルの単純な `T['key']` は、選んだプロパティの宣言とその依存だけをたどる。
    fn collect_indexed_members(&mut self, root: Node<'_>, source: &[u8]) {
        let indexed = |r: &&Reference| matches!(r.member, Some(Member::Indexed(_)));
        let mut pending: BTreeSet<_> = self
            .declarations
            .values()
            .flat_map(|refs| refs.names.iter().filter(indexed).cloned())
            .collect();
        if pending.is_empty() {
            return;
        }
        let locals = local_type_values(root, source, true).unwrap_or_default();
        let mut owners = HashMap::new();
        while let Some(reference) = pending.pop_first() {
            if self.types.contains_key(&reference) {
                continue;
            }
            let Some(Member::Indexed(key)) = &reference.member else {
                continue;
            };
            let properties = owners
                .entry(reference.name.clone())
                .or_insert_with(|| indexed_properties(*locals.get(&reference.name)?, source));
            let selected = properties
                .as_ref()
                .and_then(|properties| properties.get(key));
            let mut references = References::default();
            let mut tokens = Vec::new();
            if let Some(&member) = selected {
                references.add(member, source, &[]);
                if let Some(header) = type_tokens(member, source, member.end_byte()) {
                    tokens.push(header);
                } else {
                    references.unknown = true;
                }
            }
            // this 型は選択元の他のメンバーも見る。解けない添字も元の型全体に依存させる。
            if selected.is_none()
                || references.unknown
                || selected.is_some_and(|member| {
                    crate::engine::tree_walk::any_node(
                        *member,
                        crate::engine::tree_walk::Children::Named,
                        |node| node.kind() == "this_type",
                    )
                })
            {
                references
                    .names
                    .insert(Reference::whole(reference.name.clone()));
            }
            pending.extend(references.names.iter().filter(indexed).cloned());
            self.types.insert(
                reference,
                Some(LocalType {
                    kind: "lookup_type".to_owned(),
                    tokens,
                    references,
                }),
            );
        }
    }

    fn collect_class(&mut self, class: Node<'_>, source: &[u8], name: &str) {
        let names = bindings(class, source, &[]);
        let mut references = header_references(class, source, &[]);
        let mut cursor = class.walk();
        for child in class
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "class_heritage")
        {
            let mut cursor = child.walk();
            for clause in child.named_children(&mut cursor) {
                if clause.kind() == "implements_clause" {
                    let mut cursor = clause.walk();
                    for ty in clause.named_children(&mut cursor) {
                        references.add(ty, source, &names);
                    }
                } else if clause.kind() == "extends_clause" {
                    if let Some(base) = clause
                        .child_by_field_name("value")
                        .filter(|base| base.kind() == "identifier")
                        .and_then(|base| plain(base, source))
                    {
                        references.names.insert(Reference::whole(base));
                    } else {
                        references.unknown = true;
                    }
                    let mut cursor = clause.walk();
                    for arguments in clause
                        .named_children(&mut cursor)
                        .filter(|node| node.kind() == "type_arguments")
                    {
                        references.add(arguments, source, &names);
                    }
                } else {
                    references.unknown = true;
                }
            }
        }
        let mut tokens = Vec::new();
        let mut tokens_known = true;
        if let Some(body) = class.child_by_field_name("body") {
            if let Some(header) = type_tokens(class, source, body.start_byte()) {
                tokens.push(header);
            } else {
                tokens_known = false;
            }
            let mut cursor = body.walk();
            for member in body.named_children(&mut cursor).filter(|n| !n.is_extra()) {
                let end = member
                    .child_by_field_name("body")
                    .map(|body| body.start_byte())
                    .or_else(|| member.child_by_field_name("type").map(|ty| ty.end_byte()))
                    .unwrap_or_else(|| member.end_byte());
                if let Some(header) = type_tokens(member, source, end) {
                    tokens.push(header);
                } else {
                    tokens_known = false;
                }
                let member_refs = header_references(member, source, &names);
                if member.kind() == "public_field_definition"
                    && member.child_by_field_name("type").is_none()
                {
                    references.unknown = true;
                }
                if member.kind() == "method_definition"
                    && member.child_by_field_name("return_type").is_none()
                    && member
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                        != Some("constructor")
                {
                    references.unknown = true;
                }
                if member.kind() != "index_signature"
                    && let Some(member_name) = member
                        .child_by_field_name("name")
                        .and_then(|n| plain(n, source))
                {
                    let mut independent = member_refs.clone();
                    independent.unknown |= !matches!(
                        member.kind(),
                        "method_definition"
                            | "abstract_method_signature"
                            | "public_field_definition"
                    );
                    self.insert_declaration(format!("{name}.{member_name}"), independent);
                }
                references.extend(member_refs);
            }
        }
        let fact = tokens_known.then(|| LocalType {
            kind: class.kind().to_owned(),
            tokens,
            references: references.clone(),
        });
        match self.types.entry(Reference::whole(name.to_owned())) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(fact);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
        self.insert_declaration(name.to_owned(), references);
    }

    pub(crate) fn compare(self, new: Self) -> DependencyChanges {
        let mut changed = BTreeSet::new();
        let mut unknown = BTreeSet::new();
        let mut reverse: BTreeMap<Reference, BTreeSet<Reference>> = BTreeMap::new();
        let keys: BTreeSet<_> = self.types.keys().chain(new.types.keys()).cloned().collect();
        for name in &keys {
            match (self.types.get(name), new.types.get(name)) {
                (Some(Some(old)), Some(Some(next))) => {
                    if (old.kind != next.kind || old.tokens != next.tokens)
                        && !(self.exported.contains(&name.name)
                            && new.exported.contains(&name.name))
                    {
                        changed.insert(name.clone());
                    }
                    for references in [&old.references, &next.references] {
                        if references.unknown {
                            unknown.insert(name.clone());
                        }
                        for reference in &references.names {
                            let key = reference.graph_key();
                            if matches!(reference.member, Some(Member::Qualified(_)))
                                || !keys.contains(key.as_ref())
                            {
                                unknown.insert(name.clone());
                            }
                            reverse
                                .entry(key.into_owned())
                                .or_default()
                                .insert(name.clone());
                        }
                    }
                }
                (Some(Some(_)), None) | (None, Some(Some(_)))
                    if !(self.exported.contains(&name.name)
                        && new.exported.contains(&name.name)) =>
                {
                    changed.insert(name.clone());
                }
                _ => {
                    unknown.insert(name.clone());
                }
            }
        }
        for seeds in [&mut changed, &mut unknown] {
            let mut queue: VecDeque<_> = seeds.iter().cloned().collect();
            while let Some(name) = queue.pop_front() {
                for dependent in reverse.get(&name).into_iter().flatten() {
                    if seeds.insert(dependent.clone()) {
                        queue.push_back(dependent.clone());
                    }
                }
            }
        }
        let mut positions = BTreeMap::new();
        let statuses = new
            .declarations
            .iter()
            .map(|(name, references)| {
                let previous = self.declarations.get(name);
                let refs = previous
                    .into_iter()
                    .flat_map(|r| r.names.intersection(&references.names));
                let mut status = if self.unknown
                    || new.unknown
                    || references.unknown
                    || previous.is_none_or(|r| r.unknown)
                {
                    DependencyStatus::Unknown
                } else {
                    DependencyStatus::Unchanged
                };
                for reference in refs {
                    let key = reference.graph_key();
                    if changed.contains(key.as_ref()) {
                        status = DependencyStatus::Changed;
                        let mut parts = BTreeSet::from([ChangedPart::ReferencedType]);
                        if let Some(previous) = previous {
                            parts.extend(
                                previous
                                    .positions
                                    .intersection(&references.positions)
                                    .filter(|(_, reference)| {
                                        changed.contains(reference.graph_key().as_ref())
                                    })
                                    .map(|(part, _)| *part),
                            );
                        }
                        positions.insert(name.clone(), parts);
                        break;
                    }
                    if matches!(reference.member, Some(Member::Qualified(_)))
                        || unknown.contains(key.as_ref())
                        || !keys.contains(key.as_ref())
                    {
                        status = DependencyStatus::Unknown;
                    }
                }
                (name.clone(), status)
            })
            .collect();
        let proof_references = |snapshot: Self| {
            snapshot
                .declarations
                .into_iter()
                .map(|(name, references)| {
                    let references = (!snapshot.unknown && !references.unknown)
                        .then(|| references.names.into_iter().collect());
                    (name, references)
                })
                .collect()
        };
        DependencyChanges {
            statuses,
            positions,
            old_references: proof_references(self),
            new_references: proof_references(new),
        }
    }
}

pub(crate) fn compatible_dependencies_unchanged(
    site: &super::CompatibleModSite<'_>,
    sources: &mut super::SignatureSourceCache<'_>,
    cache: &mut super::OptionalRemovalCache,
    diff_files: &[crate::models::impact::DiffFile],
    reason: &str,
    dependencies: Option<&DependencyChanges>,
) -> bool {
    let Some(lang) = site.lang_in(super::TS_ONLY_LANGS) else {
        return true;
    };
    let Some(pair) = sources.get(site) else {
        return false;
    };
    let Some((old, new)) = pair.parse_pair(lang) else {
        return false;
    };
    let references = if let Some(dependencies) = dependencies {
        dependencies.references(site.name, reason)
    } else {
        let selected = if reason == "trailing_optional_params_removed" {
            DependencySnapshot::collect(new.root_node(), &pair.new)
        } else {
            DependencySnapshot::collect(old.root_node(), &pair.old)
        };
        selected.references(site.name)
    };
    let Some(references) = references else {
        return false;
    };
    if references.is_empty() {
        return true;
    }
    let Some(evidence) = cache.evidence(site.dir) else {
        return false;
    };
    let Some(packages) = evidence.relaxation() else {
        return false;
    };
    let changed_paths = diff_files
        .iter()
        .flat_map(|file| [&file.old_path, &file.new_path])
        .filter(|path| *path != "/dev/null")
        .map(|path| crate::git_support::normalize_workspace_separators(path))
        .collect();
    let mut local_imports = BTreeSet::new();
    references_unchanged(
        &DependencyEnvironment {
            target_path: site.new_path,
            changed_paths: &changed_paths,
            registry_dependency: &|specifier| packages.is_registry_dependency(specifier),
            declared_globally: &|name| evidence.declares_global(name),
        },
        old.root_node(),
        &pair.old,
        new.root_node(),
        &pair.new,
        references,
        (reason == "trailing_optional_params" && site.old_path == site.new_path)
            .then_some(&mut local_imports),
    ) && local_imports
        .into_iter()
        .all(|(specifier, name)| evidence.stable_type_import(site, sources, specifier, &name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::parser, language::LangId};

    #[test]
    fn signature_dependency_graph_tracks_local_types_and_generic_bindings() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (before, after, name, expected) in [
                (
                    "type Label = string; export function format(v: Label): string { return ''; }",
                    "type Label = number; export function format(v: Label): string { return ''; }",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "type Inner = string; type Label = Inner; export function format(): Label { return '' as any; }",
                    "type Inner = number; type Label = Inner; export function format(): Label { return '' as any; }",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "interface Label { a: string } interface Label { b: boolean } export function format(v: Label): void {}",
                    "interface Label { a: number } interface Label { b: boolean } export function format(v: Label): void {}",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "enum Mode { Open = 'open' } export function format(v: Mode): void {}",
                    "enum Mode { Open = 'closed' } export function format(v: Mode): void {}",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "type Label = string; export function format<Label>(v: Label): Label { return v; }",
                    "type Label = number; export function format<Label>(v: Label): Label { return v; }",
                    "format",
                    DependencyStatus::Unchanged,
                ),
                (
                    "type Label = string; export function format<T extends Label>(v: T): T { return v; }",
                    "type Label = number; export function format<T extends Label>(v: T): T { return v; }",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "interface A { next: B } interface B { next: A; text: string } export function format(v: A): void {}",
                    "interface A { next: B } interface B { next: A; text: number } export function format(v: A): void {}",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "interface A { next: B } interface B { next: A } export function format(v: A): void {}",
                    "interface A { next: B } interface B { next: A } export function format(v: A): void {}",
                    "format",
                    DependencyStatus::Unchanged,
                ),
                (
                    "type Label = string; export class View { text: Label; render(v: Label): Label { return v; } }",
                    "type Label = number; export class View { text: Label; render(v: Label): Label { return v; } }",
                    "View",
                    DependencyStatus::Changed,
                ),
                (
                    "type Label = string; export class View<Label> { render(v: Label): Label { return v; } }",
                    "type Label = number; export class View<Label> { render(v: Label): Label { return v; } }",
                    "View.render",
                    DependencyStatus::Unchanged,
                ),
                (
                    "type Label = string; export const format: (v: Label) => string = v => String(v);",
                    "type Label = number; export const format: (v: Label) => string = v => String(v);",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "type Label = string; export const format = (v: Label): string => String(v);",
                    "type Label = number; export const format = (v: Label): string => String(v);",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "type Label = string; export const format = (((v: Label): string => String(v)));",
                    "type Label = number; export const format = (((v: Label): string => String(v)));",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "import { External } from 'pkg'; type Unused = string; export function format(v: External): void {}",
                    "import { External } from 'pkg'; type Unused = number; export function format(v: External): void {}",
                    "format",
                    DependencyStatus::Unknown,
                ),
                (
                    "type Label = string; export function format(v: string): void { let local: Label; }",
                    "type Label = number; export function format(v: string): void { let local: Label; }",
                    "format",
                    DependencyStatus::Unchanged,
                ),
                (
                    "type Label = string; export function format(v: Label): void {}",
                    "type Label = /* same */ string ; export function format(v: Label): void {}",
                    "format",
                    DependencyStatus::Unchanged,
                ),
                (
                    "type Label = string; class Hidden { value!: Label } export function format(v: Hidden): void {}",
                    "type Label = number; class Hidden { value!: Label } export function format(v: Hidden): void {}",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "type Label = string; class Base { value!: Label } class Hidden extends Base {} export function format(v: Hidden): void {}",
                    "type Label = number; class Base { value!: Label } class Hidden extends Base {} export function format(v: Hidden): void {}",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "type Label = string; class Base<T> { value!: T } class Hidden extends Base<Label> {} export function format(v: Hidden): void {}",
                    "type Label = number; class Base<T> { value!: T } class Hidden extends Base<Label> {} export function format(v: Hidden): void {}",
                    "format",
                    DependencyStatus::Changed,
                ),
                (
                    "type Label = string; class Hidden { render(): string { let local: Label; return ''; } } export function format(v: Hidden): void {}",
                    "type Label = number; class Hidden { render(): string { let local: Label; return ''; } } export function format(v: Hidden): void {}",
                    "format",
                    DependencyStatus::Unchanged,
                ),
                (
                    "export interface Row { value: string } export function format(v: Row): void {}",
                    "export interface Row { value: number } export function format(v: Row): void {}",
                    "format",
                    DependencyStatus::Unchanged,
                ),
                (
                    "type Row = string; export type { Row as Public }; export function format(v: Row): void {}",
                    "type Row = number; export type { Row as Public }; export function format(v: Row): void {}",
                    "format",
                    DependencyStatus::Unchanged,
                ),
            ] {
                let old_tree = parser::parse_source(before.as_bytes(), lang).unwrap();
                let new_tree = parser::parse_source(after.as_bytes(), lang).unwrap();
                assert!(!old_tree.root_node().has_error() && !new_tree.root_node().has_error());
                let old = DependencySnapshot::collect(old_tree.root_node(), before.as_bytes());
                let new = DependencySnapshot::collect(new_tree.root_node(), after.as_bytes());
                assert_eq!(
                    old.compare(new).get(name),
                    Some(&expected),
                    "{lang:?}: {after}"
                );
            }
        }
    }

    #[test]
    fn class_index_signature_bindings_do_not_name_class_members() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (result, expected) in [
                ("string", DependencyStatus::Unchanged),
                ("Value", DependencyStatus::Changed),
            ] {
                let before = format!(
                    "type Value = string; export class Bag {{ [read: string]: Value | (() => {result}); read(): {result} {{ return '' as any; }} }}"
                );
                let after = before.replace("Value = string", "Value = number");
                let snapshot = |source: &str| {
                    let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                    assert!(!tree.root_node().has_error());
                    DependencySnapshot::collect(tree.root_node(), source.as_bytes())
                };
                let changes = snapshot(&before).compare(snapshot(&after));
                assert_eq!(changes.get("Bag"), Some(&DependencyStatus::Changed));
                assert_eq!(
                    changes.get("Bag.read"),
                    Some(&expected),
                    "{lang:?}: {after}"
                );
            }
        }
    }

    #[test]
    fn class_index_signature_selected_members_follow_their_own_dependencies() {
        let source = concat!(
            "type Key = `data-${string}`; type Other = number; ",
            "interface Names { keep: Key; unused: Other } ",
            "type Selected = Names['keep']; class Bag { [read: Selected | symbol]: number } ",
            "export function pick(): Bag { return new Bag(); }"
        );
        for lang in [LangId::Typescript, LangId::Tsx] {
            let snapshot = |source: &str| {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error());
                DependencySnapshot::collect(tree.root_node(), source.as_bytes())
            };
            for (after, expected) in [
                (source.replace("data-", "attr-"), DependencyStatus::Changed),
                (
                    source.replace("Other = number", "Other = string"),
                    DependencyStatus::Unchanged,
                ),
            ] {
                assert_eq!(
                    snapshot(source).compare(snapshot(&after)).get("pick"),
                    Some(&expected),
                    "{lang:?}: {after}"
                );
            }
        }
    }

    #[test]
    fn signature_dependency_node_contracts_exist() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for query in [
                "(type_alias_declaration name: (type_identifier))",
                "(interface_declaration name: (type_identifier))",
                "(enum_declaration name: (identifier))",
                "(function_declaration parameters: (formal_parameters))",
                "(generator_function_declaration)",
                "(function_signature)",
                "(class_declaration body: (class_body))",
                "(abstract_class_declaration)",
                "(class_heritage (extends_clause value: (identifier) (type_arguments)))",
                "(implements_clause)",
                "(method_definition return_type: (type_annotation))",
                "(abstract_method_signature)",
                "(public_field_definition type: (type_annotation))",
                "(index_signature name: (identifier) index_type: (lookup_type (type_identifier) (literal_type (string))) type: (type_annotation))",
                "(lexical_declaration (variable_declarator name: (identifier)))",
                "(variable_declaration)",
                "(arrow_function)",
                "(parenthesized_expression (arrow_function))",
                "(function_expression)",
                "(ambient_declaration)",
                "(internal_module)",
                "(module)",
                "(import_alias)",
                "(export_statement declaration: (type_alias_declaration))",
                "(export_specifier name: (identifier))",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), query)
                    .unwrap_or_else(|error| panic!("{lang:?}: {query}: {error}"));
            }
        }
    }
}
