//! 旧インスタンスを外へ出さず、旧位置から消えた非公開クラスの削除証拠。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;

use tree_sitter::{Node, Tree};

use crate::commands::git_input::GitBlobBatch;
use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;

use super::removed_attribution::RemovedCandidateRef;

const MAX_FILES: usize = 512;
const MAX_SOURCE: usize = 1_000_000;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODES: usize = 500_000;

struct Source {
    bytes: Vec<u8>,
    tree: Tree,
}

#[derive(Default)]
struct Facts {
    identifiers: HashSet<String>,
    classes: HashSet<String>,
    methods: HashSet<String>,
    annotations: HashSet<String>,
    uncertain_annotation: bool,
    has_all: bool,
    class_members: Members,
    missing_refs: HashSet<String>,
}

struct Project {
    old: HashMap<String, Source>,
    old_facts: HashMap<String, Facts>,
    current: HashMap<String, Facts>,
}

#[derive(Default)]
struct Members {
    methods: HashMap<String, HashSet<String>>,
    fields: HashSet<String>,
    bases: HashMap<String, Option<String>>,
    instance_fields: HashMap<String, HashSet<String>>,
}

impl Members {
    fn new(
        file: &Source,
        classes: &HashMap<&str, Node<'_>>,
        family: &HashSet<String>,
    ) -> Option<Self> {
        let mut index = Self {
            methods: HashMap::new(),
            fields: HashSet::new(),
            bases: HashMap::new(),
            instance_fields: HashMap::new(),
        };
        for name in family {
            let class = *classes.get(name.as_str())?;
            index.bases.insert(
                name.clone(),
                single_base(class, &file.bytes)?.map(str::to_owned),
            );
            let mut methods = HashSet::new();
            for method in children(class.child_by_field_name("body")?) {
                if method.kind() == "function_definition" {
                    let method_name = text(method.child_by_field_name("name")?, &file.bytes)?;
                    if !methods.insert(method_name.to_owned()) {
                        return None;
                    }
                }
            }
            index.methods.insert(name.clone(), methods);
            for node in nodes(class) {
                if node.kind() != "assignment" {
                    continue;
                }
                let Some(left) = node.child_by_field_name("left") else {
                    continue;
                };
                for left in nodes(left).into_iter().filter(|left| {
                    left.kind() == "attribute"
                        && left
                            .child_by_field_name("object")
                            .and_then(|object| text(object, &file.bytes))
                            == Some("self")
                }) {
                    let field =
                        text(left.child_by_field_name("attribute")?, &file.bytes)?.to_owned();
                    index.fields.insert(field.clone());
                    index
                        .instance_fields
                        .entry(name.clone())
                        .or_default()
                        .insert(field);
                }
            }
        }
        Some(index)
    }

    fn has_method<'a>(&'a self, mut owner: &'a str, member: &str) -> bool {
        if self.fields.contains(member) {
            return false;
        }
        let mut found = false;
        for _ in 0..=self.methods.len() {
            if self
                .instance_fields
                .get(owner)
                .is_some_and(|fields| fields.contains(member))
            {
                return false;
            }
            if self
                .methods
                .get(owner)
                .is_some_and(|methods| methods.contains(member))
            {
                found = true;
            }
            let Some(Some(base)) = self.bases.get(owner) else {
                return found;
            };
            owner = base;
        }
        false
    }
}

#[derive(Default)]
pub(super) struct PrivateOwnerRemovals {
    project: Option<Option<Project>>,
    proven: HashMap<(String, String, String), bool>,
}

impl PrivateOwnerRemovals {
    pub(super) fn proves_closed_removal(
        &mut self,
        dir: &str,
        base: &GitBlobBatch,
        candidate: &RemovedCandidateRef<'_>,
        old_paths: &HashSet<String>,
    ) -> bool {
        if !matches!(candidate.kind, "method" | "function") || !candidate.old_path.ends_with(".py")
        {
            return false;
        }
        let Some((owner, member)) = candidate.name.split_once('.') else {
            return false;
        };
        if !owner.starts_with('_') || !owner.is_ascii() || member.contains('.') {
            return false;
        }
        let project = self
            .project
            .get_or_insert_with(|| Project::load(dir, base, old_paths));
        let Some(project) = project else {
            return false;
        };
        // 同名のメソッドが残る移設に限定する。定義も無い未知 caller は従来どおり保留。
        if !project
            .current
            .values()
            .any(|facts| facts.methods.contains(member))
        {
            return false;
        }
        *self
            .proven
            .entry((
                candidate.old_path.to_owned(),
                owner.to_owned(),
                member.to_owned(),
            ))
            .or_insert_with(|| project.closed_family(candidate.old_path, owner, member))
    }
}

impl Project {
    fn load(dir: &str, base: &GitBlobBatch, old_paths: &HashSet<String>) -> Option<Self> {
        let root = std::fs::canonicalize(dir).ok()?;
        let git_root = git_output(dir, &["rev-parse", "--show-toplevel"])?;
        let git_root = std::str::from_utf8(&git_root).ok()?.trim_end_matches('\n');
        if std::fs::canonicalize(git_root).ok()? != root
            || !git_output(dir, &["rev-parse", "--show-superproject-working-tree"])?
                .iter()
                .all(|b| b.is_ascii_whitespace())
        {
            return None;
        }
        if !current_paths_are_normal(dir, &root) {
            return None;
        }
        let listing = git_output(dir, &["ls-tree", "-r", "-z", base.rev()])?;
        let mut project = Self {
            old: HashMap::new(),
            old_facts: HashMap::new(),
            current: HashMap::new(),
        };
        let (mut total, mut node_total, mut entries) = (0usize, 0usize, 0usize);
        for entry in listing.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
            entries += 1;
            if entries > 10_000 {
                return None;
            }
            let (metadata, path) = std::str::from_utf8(entry).ok()?.split_once('\t')?;
            let fields: Vec<_> = metadata.split_whitespace().collect();
            if fields.len() != 3
                || !matches!(fields[0], "100644" | "100755")
                || fields[1] != "blob"
                || !normal_extension(path)
            {
                return None;
            }
            if Path::new(path).extension().is_some() && !path.ends_with(".py") {
                continue;
            }
            let bytes = base.read_limited(path, MAX_SOURCE)?;
            if !path.ends_with(".py") && python_shebang(&bytes) != Some(LangId::Python) {
                continue;
            }
            total = total.checked_add(bytes.len())?;
            if total > MAX_BYTES || project.old_facts.len() >= MAX_FILES {
                return None;
            }
            let (source, facts, count) = Source::parse(bytes)?;
            node_total += count;
            if node_total > MAX_NODES {
                return None;
            }
            project.old_facts.insert(path.to_owned(), facts);
            if old_paths.contains(path) {
                project.old.insert(path.to_owned(), source);
            }
        }
        let files = crate::engine::refs::collect_files(&root, None).ok()?;
        total = 0;
        node_total = 0;
        for path in files {
            let relative = path.strip_prefix(&root).ok()?.to_str()?;
            if !normal_extension(relative) {
                return None;
            }
            if crate::engine::refs::detect_source_lang(&path) != Some(LangId::Python) {
                continue;
            }
            if std::fs::canonicalize(&path).ok()? != path {
                return None;
            }
            let bytes = crate::engine::bounded_read::read_file_limited(&path, MAX_SOURCE as u64)?;
            total = total.checked_add(bytes.len())?;
            if total > MAX_BYTES || project.current.len() >= MAX_FILES {
                return None;
            }
            let (_, facts, count) = Source::parse(bytes)?;
            node_total += count;
            if node_total > MAX_NODES {
                return None;
            }
            project.current.insert(
                crate::git_support::normalize_workspace_separators(relative),
                facts,
            );
        }
        for path in old_paths {
            if root.join(path).try_exists().ok()? && !project.current.contains_key(path) {
                return None;
            }
        }
        Some(project)
    }

    fn closed_family(&self, path: &str, owner: &str, member: &str) -> bool {
        let Some(file) = self.old.get(path) else {
            return false;
        };
        let Some(classes) = root_classes(file) else {
            return false;
        };
        if !classes.contains_key(owner) {
            return false;
        }
        let mut family = HashSet::from([owner.to_owned()]);
        loop {
            let old_len = family.len();
            for (name, class) in &classes {
                if let Some(base) = single_base(*class, &file.bytes).flatten()
                    && (family.contains(*name) || family.contains(base))
                    && base != "object"
                {
                    family.insert((*name).to_owned());
                    family.insert(base.to_owned());
                }
            }
            if family.len() == old_len {
                break;
            }
        }
        if family.len() > 64
            || family.iter().any(|name| !name.starts_with('_'))
            || self.old_facts.get(path).is_none_or(|facts| facts.has_all)
        {
            return false;
        }
        for (file_path, facts) in &self.old_facts {
            if facts.uncertain_annotation
                || family.iter().any(|name| facts.annotations.contains(name))
                || (file_path != path && family.iter().any(|name| facts.identifiers.contains(name)))
            {
                return false;
            }
        }
        // 定義だけでなく旧名の import・代入・未解決の生成も残っていてはならない。
        for (file_path, facts) in &self.current {
            if (file_path == path && facts.has_all)
                || facts.uncertain_annotation
                || family.iter().any(|name| facts.annotations.contains(name))
            {
                return false;
            }
            if facts.missing_refs.contains(member) {
                return false;
            }
            for name in &family {
                if facts.identifiers.contains(name)
                    && (file_path == path || !facts.classes.contains(name))
                {
                    return false;
                }
                if facts.classes.contains(name) && !facts.class_members.has_method(name, member) {
                    return false;
                }
            }
        }
        let Some(members) = Members::new(file, &classes, &family) else {
            return false;
        };
        family.iter().all(|name| {
            let Some(class) = classes.get(name.as_str()) else {
                return false;
            };
            closed_class(file, *class, &members)
        }) && closed_uses(file, &members, &family)
    }
}

impl Source {
    fn parse(bytes: Vec<u8>) -> Option<(Self, Facts, usize)> {
        if bytes.starts_with(b"version https://git-lfs.github.com/spec/") {
            return None;
        }
        let tree = parser::parse_source(&bytes, LangId::Python).ok()?;
        let root = tree.root_node();
        if root.has_error() {
            return None;
        }
        let all = nodes(root);
        if all.len() > 100_000 {
            return None;
        }
        let mut facts = Facts::default();
        let mut bindings: HashMap<String, usize> = HashMap::new();
        let mut annotation_identifiers = HashSet::new();
        let annotation_classes: HashSet<&str> = all
            .iter()
            .filter_map(|node| {
                (node.kind() == "class_definition"
                    && node.parent().is_some_and(|parent| parent.id() == root.id()))
                .then(|| {
                    node.child_by_field_name("name")
                        .and_then(|name| text(name, &bytes))
                })
                .flatten()
            })
            .collect();
        for node in &all {
            if node.is_missing()
                || matches!(
                    node.kind(),
                    "wildcard_import"
                        | "named_expression"
                        | "global_statement"
                        | "nonlocal_statement"
                        | "type_alias_statement"
                        | "match_statement"
                )
                || node.child_by_field_name("type_parameters").is_some()
            {
                return None;
            }
            if node.kind() == "identifier" {
                let name = text(*node, &bytes)?;
                if matches!(
                    name,
                    "globals"
                        | "locals"
                        | "vars"
                        | "exec"
                        | "eval"
                        | "__import__"
                        | "getattr"
                        | "setattr"
                        | "delattr"
                        | "hasattr"
                        | "__dict__"
                        | "__class__"
                        | "__bases__"
                        | "__mro__"
                        | "__subclasses__"
                        | "__base__"
                        | "__getattr__"
                        | "__getattribute__"
                        | "__setattr__"
                        | "__new__"
                        | "type"
                        | "attrgetter"
                        | "import_module"
                        | "new_class"
                        | "modules"
                        | "__annotations__"
                        | "get_type_hints"
                        | "__builtins__"
                        | "resolve_name"
                        | "getmembers"
                        | "getmembers_static"
                        | "locate"
                        | "get_objects"
                        | "get_referrers"
                        | "__reduce_ex__"
                ) {
                    return None;
                }
                facts.has_all |= name == "__all__";
                facts.identifiers.insert(name.to_owned());
                if in_annotation(*node) {
                    annotation_identifiers.insert(name.to_owned());
                }
                if in_annotation(*node)
                    && !annotation_classes.contains(name)
                    && !matches!(
                        name,
                        "str"
                            | "int"
                            | "float"
                            | "bool"
                            | "bytes"
                            | "list"
                            | "dict"
                            | "tuple"
                            | "set"
                            | "frozenset"
                            | "object"
                            | "complex"
                    )
                {
                    facts.uncertain_annotation = true;
                }
            }
            if matches!(node.kind(), "concatenated_string" | "binary_operator")
                && in_annotation(*node)
            {
                facts.uncertain_annotation = true;
            }
            if node.kind() == "string" && in_annotation(*node) {
                let annotation = text(*node, &bytes)?;
                facts.uncertain_annotation |= annotation.contains('\\');
                facts.uncertain_annotation |= children(*node)
                    .first()
                    .and_then(|start| text(*start, &bytes))
                    .is_none_or(|start| !matches!(start, "'" | "\"" | "'''" | "\"\"\""));
                facts.annotations.extend(
                    annotation
                        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned),
                );
            }
            let pattern = match node.kind() {
                "class_definition" | "function_definition" => node.child_by_field_name("name"),
                "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                    node.child_by_field_name("left")
                }
                "typed_parameter" | "typed_default_parameter" | "default_parameter" => node
                    .child_by_field_name("name")
                    .or_else(|| node.named_child(0)),
                "identifier"
                    if node.parent().is_some_and(|parent| {
                        matches!(parent.kind(), "parameters" | "lambda_parameters")
                    }) =>
                {
                    Some(*node)
                }
                "as_pattern" => children(*node).last().copied(),
                _ => None,
            };
            if let Some(pattern) = pattern {
                record_bindings(pattern, &bytes, &mut bindings);
            }
            if matches!(node.kind(), "import_statement" | "import_from_statement") {
                for child in children(*node) {
                    if node
                        .child_by_field_name("module_name")
                        .is_some_and(|module| module.id() == child.id())
                    {
                        continue;
                    }
                    let binding = if child.kind() == "aliased_import" {
                        child.child_by_field_name("alias")
                    } else if child.kind() == "dotted_name" {
                        child.named_child(0)
                    } else {
                        None
                    };
                    if let Some(binding) = binding {
                        record_bindings(binding, &bytes, &mut bindings);
                    }
                }
            }
        }
        facts.uncertain_annotation |= annotation_identifiers
            .iter()
            .any(|name| !annotation_classes.contains(name.as_str()) && bindings.contains_key(name));
        for node in &all {
            if node.kind() == "class_definition" {
                if node.parent()?.id() != root.id() {
                    return None;
                }
                let name = text(node.child_by_field_name("name")?, &bytes)?;
                if bindings.get(name) != Some(&1) {
                    return None;
                }
                facts.classes.insert(name.to_owned());
            }
            if node.kind() == "function_definition"
                && node
                    .parent()
                    .and_then(|body| body.parent())
                    .is_some_and(|class| class.kind() == "class_definition")
            {
                facts
                    .methods
                    .insert(text(node.child_by_field_name("name")?, &bytes)?.to_owned());
            }
        }
        let invalid_uses: HashSet<String> = all
            .iter()
            .filter_map(|node| {
                let name = (node.kind() == "identifier")
                    .then(|| text(*node, &bytes))
                    .flatten()?;
                (facts.classes.contains(name) && !independent_class_site(*node))
                    .then(|| name.to_owned())
            })
            .collect();
        facts.classes.retain(|name| !invalid_uses.contains(name));
        let count = all.len();
        let file = Self { bytes, tree };
        let classes = root_classes(&file)?;
        let mut checked = HashSet::new();
        for class in classes.values() {
            if children(class.child_by_field_name("body")?)
                .iter()
                .any(|item| {
                    !matches!(
                        item.kind(),
                        "function_definition" | "comment" | "pass_statement"
                    ) && !(item.kind() == "expression_statement"
                        && item.named_child_count() == 1
                        && item
                            .named_child(0)
                            .is_some_and(|value| value.kind() == "string"))
                })
            {
                return None;
            }
            if let Some(base) = single_base(*class, &file.bytes)? {
                if base == "object" {
                    if bindings.contains_key("object") {
                        return None;
                    }
                } else if !classes.contains_key(base) {
                    return None;
                }
            }
            let mut visiting = HashSet::new();
            let mut next = Some(*class);
            while let Some(class) = next {
                if checked.contains(&class.id()) {
                    break;
                }
                if !visiting.insert(class.id()) {
                    return None;
                }
                next = single_base(class, &file.bytes)?.and_then(|base| classes.get(base).copied());
            }
            checked.extend(visiting);
        }
        let class_names = classes.keys().map(|name| (*name).to_owned()).collect();
        let members = Members::new(&file, &classes, &class_names)?;
        for node in nodes(file.tree.root_node()) {
            let target = match node.kind() {
                "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                    node.child_by_field_name("left")
                }
                "as_pattern" => node.child_by_field_name("alias"),
                "delete_statement" => Some(node),
                _ => None,
            };
            if let Some(target) = target {
                for attribute in nodes(target)
                    .into_iter()
                    .filter(|node| node.kind() == "attribute")
                {
                    let member = text(attribute.child_by_field_name("attribute")?, &file.bytes)?;
                    facts.missing_refs.insert(member.to_owned());
                }
            }
        }
        for attribute in nodes(file.tree.root_node()) {
            if attribute.kind() != "attribute" {
                continue;
            }
            if attribute.parent().is_some_and(|assignment| {
                assignment.kind() == "assignment"
                    && assignment
                        .child_by_field_name("left")
                        .is_some_and(|left| left.id() == attribute.id())
            }) {
                continue;
            }
            let Some(member) = attribute
                .child_by_field_name("attribute")
                .and_then(|name| text(name, &file.bytes))
            else {
                continue;
            };
            let Some(receiver) = attribute.child_by_field_name("object") else {
                continue;
            };
            let mut owner = None;
            let receiver_name = text(receiver, &file.bytes);
            let explicit = receiver.kind() == "call"
                || (receiver.kind() == "identifier"
                    && receiver_name.is_some_and(|name| {
                        matches!(name, "self" | "cls") || classes.contains_key(name)
                    }));
            if receiver.kind() == "identifier" && receiver_name == Some("self") {
                let mut parent = receiver.parent();
                while let Some(node) = parent {
                    if node.kind() == "class_definition" {
                        owner = node
                            .child_by_field_name("name")
                            .and_then(|name| text(name, &file.bytes));
                        break;
                    }
                    parent = node.parent();
                }
            } else if receiver.kind() == "call" {
                owner = receiver
                    .child_by_field_name("function")
                    .filter(|callee| callee.kind() == "identifier")
                    .and_then(|name| text(name, &file.bytes))
                    .filter(|name| classes.contains_key(name));
            } else if receiver.kind() == "identifier" {
                owner = receiver_name.filter(|name| classes.contains_key(name));
            }
            if explicit && owner.is_none_or(|owner| !members.has_method(owner, member)) {
                facts.missing_refs.insert(member.to_owned());
            }
        }
        facts.class_members = members;
        Some((file, facts, count))
    }
}

fn closed_class(file: &Source, class: Node<'_>, members: &Members) -> bool {
    if members
        .methods
        .values()
        .any(|methods| methods.iter().any(|member| members.fields.contains(member)))
    {
        return false;
    }
    let Some(owner) = class
        .child_by_field_name("name")
        .and_then(|name| text(name, &file.bytes))
    else {
        return false;
    };
    let Some(body) = class.child_by_field_name("body") else {
        return false;
    };
    let mut names = HashSet::new();
    for method in children(body) {
        if method.kind() == "comment" || method.kind() == "pass_statement" {
            continue;
        }
        if method.kind() != "function_definition" {
            return false;
        }
        if method.child(0).is_some_and(|child| child.kind() == "async") {
            return false;
        }
        let Some(name) = method
            .child_by_field_name("name")
            .and_then(|name| text(name, &file.bytes))
        else {
            return false;
        };
        if !names.insert(name) || (name.starts_with("__") && name != "__init__") {
            return false;
        }
        let Some(parameters) = method.child_by_field_name("parameters") else {
            return false;
        };
        let Some(receiver) = children(parameters).first().copied() else {
            return false;
        };
        if text(receiver, &file.bytes) != Some("self") {
            return false;
        }
        for node in nodes(method) {
            if node.id() != method.id() && node.kind() == "function_definition"
                || matches!(node.kind(), "lambda" | "yield" | "await" | "async")
            {
                return false;
            }
            if node.kind() == "generator_expression" && !consumed_join(node, &file.bytes) {
                return false;
            }
            if node.kind() != "identifier" {
                continue;
            }
            let Some(value) = text(node, &file.bytes) else {
                return false;
            };
            if matches!(value, "cls" | "super") {
                return false;
            }
            if value != "self" || node.id() == receiver.id() {
                continue;
            }
            let Some(attribute) = node.parent().filter(|parent| parent.kind() == "attribute")
            else {
                return false;
            };
            if attribute
                .child_by_field_name("object")
                .is_none_or(|object| object.id() != node.id())
            {
                return false;
            }
            let Some(member) = attribute
                .child_by_field_name("attribute")
                .and_then(|name| text(name, &file.bytes))
            else {
                return false;
            };
            if member.starts_with("__") {
                return false;
            }
            let is_method = members.has_method(owner, member);
            let direct_call = attribute.parent().is_some_and(|call| {
                call.kind() == "call"
                    && call
                        .child_by_field_name("function")
                        .is_some_and(|function| function.id() == attribute.id())
            });
            if direct_call != is_method {
                return false;
            }
            if !is_method && !members.fields.contains(member) {
                return false;
            }
        }
    }
    true
}

fn closed_uses(file: &Source, members: &Members, family: &HashSet<String>) -> bool {
    for node in nodes(file.tree.root_node()) {
        if node.kind() != "identifier"
            || !text(node, &file.bytes).is_some_and(|name| family.contains(name))
        {
            continue;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind() == "class_definition"
            && parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id())
        {
            continue;
        }
        if parent.kind() == "argument_list"
            && parent.parent().is_some_and(|class| {
                class.kind() == "class_definition"
                    && class
                        .child_by_field_name("name")
                        .and_then(|name| text(name, &file.bytes))
                        .is_some_and(|name| family.contains(name))
            })
        {
            continue;
        }
        if parent.kind() != "call"
            || parent
                .child_by_field_name("function")
                .is_none_or(|callee| callee.id() != node.id())
        {
            return false;
        }
        let owner = text(node, &file.bytes).unwrap_or_default();
        let mut scope = parent.parent();
        let mut functions = 0;
        while let Some(ancestor) = scope {
            if matches!(ancestor.kind(), "lambda" | "generator_expression") {
                return false;
            }
            functions += usize::from(ancestor.kind() == "function_definition");
            scope = ancestor.parent();
        }
        if functions != 1 {
            return false;
        }
        if !instance_receiver(file, parent, owner, members) {
            return false;
        }
    }
    true
}

fn instance_receiver(file: &Source, instance: Node<'_>, owner: &str, members: &Members) -> bool {
    let Some(parent) = instance.parent() else {
        return false;
    };
    if parent.kind() == "attribute" {
        return direct_instance_call(file, instance, owner, members);
    }
    // 局所変数は関数直下の単純代入だけ。全出現がその後の receiver であることを要求する。
    if parent.kind() != "assignment"
        || parent
            .child_by_field_name("right")
            .is_none_or(|right| right.id() != instance.id())
    {
        return false;
    }
    let Some(left) = parent
        .child_by_field_name("left")
        .filter(|left| left.kind() == "identifier")
    else {
        return false;
    };
    let Some(statement) = parent
        .parent()
        .filter(|statement| statement.kind() == "expression_statement")
    else {
        return false;
    };
    let Some(body) = statement.parent().filter(|body| body.kind() == "block") else {
        return false;
    };
    let Some(function) = body
        .parent()
        .filter(|function| function.kind() == "function_definition")
    else {
        return false;
    };
    let name = text(left, &file.bytes);
    nodes(function).iter().all(|node| {
        if node.kind() != "identifier" || text(*node, &file.bytes) != name || node.id() == left.id()
        {
            return true;
        }
        let mut scope = node.parent();
        while let Some(ancestor) = scope {
            if ancestor.id() == function.id() {
                break;
            }
            if matches!(
                ancestor.kind(),
                "function_definition"
                    | "lambda"
                    | "generator_expression"
                    | "list_comprehension"
                    | "set_comprehension"
                    | "dictionary_comprehension"
            ) {
                return false;
            }
            scope = ancestor.parent();
        }
        node.start_byte() > parent.end_byte() && direct_instance_call(file, *node, owner, members)
    })
}

fn direct_instance_call(file: &Source, instance: Node<'_>, owner: &str, members: &Members) -> bool {
    let Some(attribute) = instance
        .parent()
        .filter(|parent| parent.kind() == "attribute")
    else {
        return false;
    };
    if attribute
        .child_by_field_name("object")
        .is_none_or(|object| object.id() != instance.id())
    {
        return false;
    }
    let Some(member) = attribute
        .child_by_field_name("attribute")
        .and_then(|name| text(name, &file.bytes))
    else {
        return false;
    };
    attribute.parent().is_some_and(|call| {
        call.kind() == "call"
            && call
                .child_by_field_name("function")
                .is_some_and(|function| function.id() == attribute.id())
    }) && members.has_method(owner, member)
}

fn consumed_join(generator: Node<'_>, source: &[u8]) -> bool {
    let parts = children(generator);
    if parts.len() != 2
        || parts[1].kind() != "for_in_clause"
        || parts[1]
            .child_by_field_name("left")
            .is_none_or(|left| left.kind() != "identifier")
        || parts[1]
            .child(0)
            .is_some_and(|child| child.kind() == "async")
    {
        return false;
    }
    let Some(call) = generator.parent().filter(|parent| parent.kind() == "call") else {
        return false;
    };
    if call
        .child_by_field_name("arguments")
        .is_none_or(|args| args.id() != generator.id())
    {
        return false;
    }
    let Some(function) = call
        .child_by_field_name("function")
        .filter(|function| function.kind() == "attribute")
    else {
        return false;
    };
    let Some(receiver) = function
        .child_by_field_name("object")
        .filter(|receiver| receiver.kind() == "string")
    else {
        return false;
    };
    function
        .child_by_field_name("attribute")
        .and_then(|name| text(name, source))
        == Some("join")
        && children(receiver)
            .first()
            .and_then(|start| text(*start, source))
            .is_some_and(|start| {
                start == "'" || start == "\"" || start == "'''" || start == "\"\"\""
            })
        && !nodes(generator)
            .iter()
            .any(|node| matches!(node.kind(), "lambda" | "await" | "named_expression"))
}

fn independent_class_site(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    (parent.kind() == "class_definition"
        && parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()))
        || (parent.kind() == "call"
            && parent
                .child_by_field_name("function")
                .is_some_and(|callee| callee.id() == node.id()))
        || (parent.kind() == "argument_list"
            && parent
                .parent()
                .is_some_and(|class| class.kind() == "class_definition"))
}

fn root_classes(file: &Source) -> Option<HashMap<&str, Node<'_>>> {
    let mut classes = HashMap::new();
    for node in children(file.tree.root_node()) {
        if node.kind() == "class_definition" {
            let name = text(node.child_by_field_name("name")?, &file.bytes)?;
            if classes.insert(name, node).is_some() {
                return None;
            }
        }
    }
    Some(classes)
}

fn single_base<'a>(class: Node<'_>, source: &'a [u8]) -> Option<Option<&'a str>> {
    let Some(bases) = class.child_by_field_name("superclasses") else {
        return Some(None);
    };
    let bases = children(bases);
    if bases.is_empty() {
        return Some(None);
    }
    if bases.len() != 1 || bases[0].kind() != "identifier" {
        return None;
    }
    Some(Some(text(bases[0], source)?))
}

fn record_bindings(pattern: Node<'_>, source: &[u8], bindings: &mut HashMap<String, usize>) {
    let _: std::ops::ControlFlow<()> = walk_preorder(pattern, Children::Named, |node, _| {
        if matches!(node.kind(), "attribute" | "subscript" | "type") {
            return std::ops::ControlFlow::Continue(Walk::Skip);
        }
        if node.kind() == "identifier"
            && let Some(name) = text(node, source)
        {
            *bindings.entry(name.to_owned()).or_default() += 1;
        }
        std::ops::ControlFlow::Continue(Walk::Descend)
    });
}

fn in_annotation(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "type" {
            return true;
        }
        if matches!(parent.kind(), "block" | "expression_statement") {
            return false;
        }
        node = parent;
    }
    false
}

fn normal_extension(path: &str) -> bool {
    Path::new(path).extension().is_none_or(|extension| {
        extension.to_str().is_some_and(|extension| {
            extension.is_ascii()
                && !extension.eq_ignore_ascii_case("pyi")
                && (!extension.eq_ignore_ascii_case("py") || extension == "py")
        })
    })
}

fn current_paths_are_normal(dir: &str, root: &Path) -> bool {
    let Some(tracked) = git_output(dir, &["ls-files", "--stage", "-z"]) else {
        return false;
    };
    let Some(untracked) = git_output(dir, &["ls-files", "--others", "--exclude-standard", "-z"])
    else {
        return false;
    };
    let mut count = 0;
    for entry in tracked.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        count += 1;
        let Some((metadata, path)) = std::str::from_utf8(entry)
            .ok()
            .and_then(|entry| entry.split_once('\t'))
        else {
            return false;
        };
        let fields: Vec<_> = metadata.split_whitespace().collect();
        if fields.len() != 3
            || !matches!(fields[0], "100644" | "100755")
            || fields[2] != "0"
            || !normal_current_path(root, path)
        {
            return false;
        }
    }
    for path in untracked.split(|b| *b == 0).filter(|path| !path.is_empty()) {
        count += 1;
        let Ok(path) = std::str::from_utf8(path) else {
            return false;
        };
        if !normal_current_path(root, path) {
            return false;
        }
    }
    count <= 10_000
}

fn normal_current_path(root: &Path, path: &str) -> bool {
    if !normal_extension(path) {
        return false;
    }
    let full = root.join(path);
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) => {
            metadata.is_file() && std::fs::canonicalize(&full).ok().as_ref() == Some(&full)
        }
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

fn python_shebang(source: &[u8]) -> Option<LangId> {
    LangId::from_shebang(std::str::from_utf8(source.split(|b| *b == b'\n').next()?).ok()?)
}

fn git_output(dir: &str, args: &[&str]) -> Option<Vec<u8>> {
    let mut child = crate::git_support::git_command_for_dir(dir)
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let output = crate::commands::common::read_bytes_limited_and_drain(
        child.stdout.take()?,
        2 * 1024 * 1024,
        "Python owner metadata",
    );
    if !child.wait().ok()?.success() {
        return None;
    }
    output.ok()
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let _: std::ops::ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
        out.push(node);
        std::ops::ControlFlow::Continue(Walk::Descend)
    });
    out
}
