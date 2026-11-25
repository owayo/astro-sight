//! 識別子としては現れない Python の名前参照を、モジュール関数の削除判定に足す。

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use tree_sitter::Node;
use unicode_normalization::UnicodeNormalization;

use super::ApiSymbolCandidate;
use super::python_contract::{collect_binding_counts, collect_typing_names, subscript_parts};
use super::removed_scope::python_string_value;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::engine::{bounded_read, parser, refs, symbols::python_module_export_policy};
use crate::language::LangId;
use crate::models::truncation::TruncationInfo;

const MAX_SOURCE: u64 = 2 * 1024 * 1024;
const MAX_BYTES: u64 = 64 * 1024 * 1024;

fn module_function(candidate: &ApiSymbolCandidate) -> bool {
    candidate.kind == "function"
        && !candidate.name.contains('.')
        && super::removed_attribution::path_lang(&candidate.file) == Some(LangId::Python)
}

pub(super) fn retain_referenced(
    dir: &str,
    gitlinks: &[PathBuf],
    removed: &mut Vec<ApiSymbolCandidate>,
    dead: &mut Vec<ApiSymbolCandidate>,
    notices: &mut Vec<TruncationInfo>,
) {
    let mut uses = Uses::default();
    for c in dead.iter().filter(|c| module_function(c)) {
        let file = Path::new(&c.file).with_extension("");
        let path = crate::git_support::normalize_workspace_separators(&file.to_string_lossy());
        let module = path
            .strip_suffix("/__init__")
            .unwrap_or(&path)
            .replace('/', ".");
        uses.wanted
            .entry(c.name.nfkc().collect())
            .or_default()
            .push((c.file.clone(), module));
    }
    if uses.wanted.is_empty() {
        return;
    }
    if let Err(notice) = uses.scan(Path::new(dir), gitlinks, dead) {
        notices.push(notice);
        uses.any.unknown = true;
    }
    let (live, unused) = std::mem::take(dead)
        .into_iter()
        .partition(|c| module_function(c) && uses.may_refer(c));
    removed.extend(live);
    *dead = unused;
}

#[derive(Default)]
struct Names {
    known: HashSet<String>,
    unknown: bool,
}

impl Names {
    fn includes(&self, name: &str) -> bool {
        self.unknown || self.known.contains(name)
    }
}

#[derive(Default)]
struct Uses {
    // 関数の名前ごとの削除元ファイルと、パスから得た完全なモジュール名。
    wanted: HashMap<String, Vec<(String, String)>>,
    any: Names,
    local: HashMap<String, Names>,
    module_reads: HashMap<String, HashSet<String>>,
    declarations: HashMap<String, HashSet<String>>,
}

impl Uses {
    fn scan(
        &mut self,
        root: &Path,
        gitlinks: &[PathBuf],
        dead: &[ApiSymbolCandidate],
    ) -> Result<(), TruncationInfo> {
        let failed = |path: &str, reason: &str| {
            TruncationInfo::source_analysis_failed(
                path,
                format!(
                    "Python string references for removed functions could not be verified: {reason}"
                ),
            )
        };
        let root = root
            .canonicalize()
            .map_err(|_| failed(".", "workspace unavailable"))?;
        let inventory =
            refs::collect_files_scan(&root, None, refs::FileScanOptions::DEAD_CODE_REFERENCES)
                .map_err(|_| failed(".", "file inventory unavailable"))?;
        if inventory.unreadable_candidates != 0 {
            return Err(failed(
                ".",
                "could not determine the language of an unreadable file",
            ));
        }
        let mut files = inventory.files;
        // 隠しディレクトリ等にある、diff で明示された削除元ファイルも調べる。
        let mut extra = dead
            .iter()
            .filter(|c| module_function(c))
            .map(|c| PathBuf::from(&c.file))
            .collect::<Vec<_>>();
        extra.sort_unstable();
        extra.dedup();
        refs::merge_extra_files(&mut files, &root, &extra);
        files.sort_unstable();
        let mut remaining = MAX_BYTES;
        for path in files {
            let relative = path
                .strip_prefix(&root)
                .map_err(|_| failed(".", "file outside workspace"))?;
            if gitlinks.iter().any(|link| relative.starts_with(link)) {
                continue;
            }
            let language = refs::detect_source_lang(&path)
                .ok_or_else(|| failed(".", "file language unavailable"))?;
            if language != LangId::Python {
                continue;
            }
            let file = crate::git_support::normalize_workspace_separators(
                relative
                    .to_str()
                    .ok_or_else(|| failed(".", "non-UTF-8 path"))?,
            );
            let source = bounded_read::read_utf8_file_limited(&path, MAX_SOURCE.min(remaining))
                .ok_or_else(|| {
                    failed(
                        &file,
                        "source unreadable or exceeds the 2 MiB file or 64 MiB scan limit",
                    )
                })?;
            remaining -= source.len() as u64;
            let tree = parser::parse_source(source.as_bytes(), LangId::Python)
                .map_err(|_| failed(&file, "parse failed"))?;
            if tree.root_node().has_error() {
                return Err(failed(&file, "parse errors"));
            }
            self.scan_module(&file, tree.root_node(), source.as_bytes())
                .map_err(|reason| failed(&file, reason))?;
            if self.any.unknown {
                return Ok(());
            }
        }
        // 別モジュールが同名関数を直接定義し、import 先を確定できた読み取りだけを帰属させる。
        for (module, names) in std::mem::take(&mut self.module_reads) {
            let owner = root
                .to_str()
                .and_then(|dir| super::python_module_path::resolve(dir, &module));
            for name in names {
                let proven = owner.as_deref().filter(|file| {
                    self.declarations
                        .get(*file)
                        .is_some_and(|defs| defs.contains(&name))
                });
                self.names(proven).known.insert(name);
            }
        }
        Ok(())
    }

    fn may_refer(&self, candidate: &ApiSymbolCandidate) -> bool {
        let name = candidate.name.nfkc().collect::<String>();
        self.any.includes(&name)
            || self
                .local
                .get(&candidate.file)
                .is_some_and(|n| n.includes(&name))
    }

    fn scan_module(
        &mut self,
        file: &str,
        root: Node<'_>,
        source: &[u8],
    ) -> Result<(), &'static str> {
        let mut defs = HashSet::new();
        let mut cursor = root.walk();
        let plain_module = root
            .named_children(&mut cursor)
            .all(|stmt| match stmt.kind() {
                "function_definition" => {
                    if let Some(name) = field_text(stmt, "name", source)
                        && self.wanted.contains_key(name)
                    {
                        defs.insert(name.to_owned());
                    }
                    true
                }
                "pass_statement" | "comment" => true,
                _ => false,
            });
        if plain_module && !defs.is_empty() {
            self.declarations.insert(file.to_owned(), defs);
        }
        let mut aliases: HashMap<&str, HashSet<Operation>> = HashMap::new();
        let mut wildcard_import = false;
        let mut check_typing_bindings = memchr::memmem::find(source, b"typing").is_some();
        let checked = walk_preorder(root, Children::Named, |node, depth| {
            // 束縛数の補助解析は左辺や呼び出し先を再走査する。深く重なるときだけ
            // Literal / Annotated の除外を見送り、文字列参照の走査は最後まで続ける。
            if check_typing_bindings && depth > 192 && node.named_child_count() > 0 {
                let repeated_subtree = match node.kind() {
                    "case_pattern" => Some(node),
                    "call" => node.child_by_field_name("function"),
                    _ => node.child_by_field_name("left"),
                };
                if repeated_subtree.is_some_and(|n| n.named_child_count() > 0) {
                    check_typing_bindings = false;
                }
            }
            if node.kind() == "identifier"
                && let Ok(name) = node.utf8_text(source)
                && !name.is_ascii()
                && !unicode_normalization::is_nfkc(name)
            {
                return ControlFlow::Break("identifier requires normalization for name lookup");
            }
            wildcard_import |= node.kind() == "wildcard_import";
            if node.kind() == "aliased_import"
                && let Some(name) = field_text(node, "name", source)
                && let Some(op) = Operation::named(name.rsplit('.').next().unwrap_or(name))
                && let Some(alias) = field_text(node, "alias", source)
            {
                aliases.entry(alias).or_default().insert(op);
            }
            ControlFlow::Continue(Walk::Descend)
        });
        if let ControlFlow::Break(reason) = checked {
            return Err(reason);
        }
        let imports = stable_module_imports(root, source);
        let metadata = if check_typing_bindings {
            typing_metadata(root, source)
        } else {
            HashMap::new()
        };
        let local = (!wildcard_import).then_some(file);
        let exports = python_module_export_policy(root, source);
        if exports.defines_all() {
            if exports.name_is_exported("").is_none() {
                self.names(local).unknown = true;
            } else {
                for name in exports.static_names() {
                    self.record(local, name);
                }
            }
        }
        let mut annotation_end = 0;
        let mut metadata_args = HashSet::new();
        let mut metadata_end = 0;
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
            if metadata_args.remove(&node.id()) {
                metadata_end = metadata_end.max(node.end_byte());
            }
            if !metadata.is_empty()
                && node.start_byte() < annotation_end
                && node.start_byte() >= metadata_end
                && matches!(node.kind(), "generic_type" | "subscript")
                && let Some((base, args)) = subscript_parts(node, source)
                && let Some(&skip) = metadata.get(&base)
            {
                metadata_args.extend(args.into_iter().skip(skip).map(|n| n.id()));
            }
            match node.kind() {
                "type" => annotation_end = annotation_end.max(node.end_byte()),
                "string" | "concatenated_string"
                    if node.start_byte() < annotation_end && node.start_byte() >= metadata_end =>
                {
                    self.observe(node, source, None, Form::Annotation);
                    return ControlFlow::Continue(Walk::Skip);
                }
                "call" => {
                    if let Some(function) = node.child_by_field_name("function")
                        && let Some(name) = leaf_name(function, source)
                        && let Some(args) = node.child_by_field_name("arguments")
                    {
                        for op in operations(name, &aliases) {
                            self.call(op, function, args, source, &imports);
                        }
                        if name == "object"
                            && function
                                .child_by_field_name("object")
                                .and_then(|n| leaf_name(n, source))
                                .is_some_and(|n| {
                                    operations(n, &aliases).any(|op| op == Operation::Patch)
                                })
                        {
                            self.call(Operation::Attribute, function, args, source, &imports);
                        }
                    }
                }
                "subscript" => {
                    if let (Some(value), Some(key)) = (
                        node.child_by_field_name("value"),
                        node.child_by_field_name("subscript"),
                    ) {
                        let value = ungroup(value);
                        if value.kind() == "attribute"
                            && field_text(value, "attribute", source) == Some("__dict__")
                        {
                            let module = imported_receiver(
                                value.child_by_field_name("object"),
                                source,
                                &imports,
                            );
                            self.observe(key, source, None, Form::Member(module));
                        } else if value.kind() == "call"
                            && let Some(name) = value
                                .child_by_field_name("function")
                                .and_then(|n| leaf_name(n, source))
                        {
                            for op in operations(name, &aliases) {
                                match op {
                                    Operation::Globals => {
                                        self.observe(key, source, local, Form::Name)
                                    }
                                    Operation::Vars => {
                                        let receiver = value
                                            .child_by_field_name("arguments")
                                            .and_then(first_argument);
                                        self.observe(
                                            key,
                                            source,
                                            None,
                                            Form::Member(imported_receiver(
                                                receiver, source, &imports,
                                            )),
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            ControlFlow::Continue(Walk::Descend)
        });
        Ok(())
    }

    fn names(&mut self, local: Option<&str>) -> &mut Names {
        match local {
            Some(file) => self.local.entry(file.to_owned()).or_default(),
            None => &mut self.any,
        }
    }

    fn record(&mut self, local: Option<&str>, name: &str) {
        if self.wanted.contains_key(name) {
            self.names(local).known.insert(name.to_owned());
        }
    }

    fn observe(&mut self, expr: Node<'_>, source: &[u8], local: Option<&str>, form: Form<'_>) {
        let expr = ungroup(expr);
        let value = matches!(expr.kind(), "string" | "concatenated_string")
            .then(|| python_string_value(expr, source))
            .flatten();
        let Some(value) = value.filter(|s| !s.contains('\0')) else {
            self.names(local).unknown = true;
            return;
        };
        match form {
            Form::Member(Some(module)) => {
                if self.wanted.contains_key(&value) {
                    self.module_reads
                        .entry(module.to_owned())
                        .or_default()
                        .insert(value);
                }
            }
            Form::Name | Form::Member(None) => self.record(local, &value),
            Form::Path => {
                for (pos, _) in value.match_indices('.') {
                    let qualifier = &value[..pos];
                    let name = value[pos + 1..].split('.').next().unwrap_or_default();
                    if let Some(targets) = self.wanted.get(name) {
                        for (path, module) in targets {
                            if module_matches(module, qualifier) {
                                self.local
                                    .entry(path.clone())
                                    .or_default()
                                    .known
                                    .insert(name.to_owned());
                            }
                        }
                    }
                }
            }
            Form::Attributes => {
                for name in value.split('.') {
                    self.record(None, name);
                }
            }
            Form::Annotation => {
                let normalized = value.nfkc().collect::<String>();
                for name in normalized.split(|c: char| {
                    c.is_whitespace() || (c.is_ascii() && !c.is_ascii_alphanumeric() && c != '_')
                }) {
                    self.record(None, name);
                }
            }
        }
    }

    fn call(
        &mut self,
        op: Operation,
        function: Node<'_>,
        args: Node<'_>,
        source: &[u8],
        imports: &HashMap<&str, &str>,
    ) {
        if matches!(op, Operation::Globals | Operation::Vars) {
            return;
        }
        let mut cursor = args.walk();
        let receiver = imported_receiver(first_argument(args), source, imports);
        let mut position = 0;
        let mut dotted_target = false;
        for arg in args.named_children(&mut cursor).filter(|n| !n.is_extra()) {
            let keyword = (arg.kind() == "keyword_argument")
                .then(|| field_text(arg, "name", source))
                .flatten();
            let value = arg
                .child_by_field_name("value")
                .filter(|_| keyword.is_some())
                .unwrap_or(arg);
            let positional = keyword.is_none();
            let form = match op {
                Operation::Patch if (positional && position == 0) || keyword == Some("target") => {
                    Some(Form::Path)
                }
                Operation::SetDelete
                    if function.kind() == "attribute"
                        && position == 0
                        && (positional || keyword == Some("target"))
                        && matches!(ungroup(value).kind(), "string" | "concatenated_string") =>
                {
                    dotted_target = true;
                    Some(Form::Path)
                }
                Operation::Attribute | Operation::SetDelete
                    if !dotted_target
                        && ((positional && position == 1)
                            || keyword == Some("name")
                            || (keyword == Some("attribute")
                                && leaf_name(function, source) == Some("object"))
                            || arg.kind() == "list_splat"
                            || arg.kind() == "dictionary_splat") =>
                {
                    Some(Form::Member(receiver))
                }
                Operation::MethodCaller if positional && position == 0 => Some(Form::Name),
                Operation::AttrGetter if positional => Some(Form::Attributes),
                _ => None,
            };
            if let Some(form) = form {
                self.observe(value, source, None, form);
            }
            position += usize::from(positional);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Operation {
    Globals,
    Vars,
    Attribute,
    SetDelete,
    MethodCaller,
    AttrGetter,
    Patch,
}

impl Operation {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "globals" => Self::Globals,
            "vars" => Self::Vars,
            "getattr" | "hasattr" => Self::Attribute,
            "setattr" | "delattr" => Self::SetDelete,
            "methodcaller" => Self::MethodCaller,
            "attrgetter" => Self::AttrGetter,
            "patch" => Self::Patch,
            _ => return None,
        })
    }
}

fn operations<'a>(
    name: &str,
    aliases: &'a HashMap<&str, HashSet<Operation>>,
) -> impl Iterator<Item = Operation> + 'a {
    Operation::named(name)
        .into_iter()
        .chain(aliases.get(name).into_iter().flatten().copied())
}

enum Form<'a> {
    Name,
    Member(Option<&'a str>),
    Path,
    Attributes,
    Annotation,
}

// sys.path による別 root と祖先パッケージからの再公開を含め、完全なセグメントだけを比べる。
fn module_matches(owner: &str, qualifier: &str) -> bool {
    if qualifier.is_empty() {
        return false;
    }
    if owner == "__init__" {
        return true;
    }
    let mut starts = std::iter::once(0).chain(owner.match_indices('.').map(|(pos, _)| pos + 1));
    if starts.any(|pos| {
        owner[pos..]
            .strip_prefix(qualifier)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    }) {
        return true;
    }
    // --dir 自体がパッケージ内なら、完全修飾名の先頭は相対パスに現れない。
    let mut ends = owner
        .match_indices('.')
        .map(|(pos, _)| pos)
        .chain(std::iter::once(owner.len()));
    ends.any(|pos| {
        qualifier
            .strip_suffix(&owner[..pos])
            .is_some_and(|rest| rest.ends_with('.'))
    })
}

fn first_argument(args: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = args.walk();
    args.named_children(&mut cursor).find(|n| !n.is_extra())
}

fn imported_receiver<'a>(
    receiver: Option<Node<'_>>,
    source: &[u8],
    imports: &HashMap<&str, &'a str>,
) -> Option<&'a str> {
    let receiver = ungroup(receiver?);
    if receiver.kind() != "identifier" {
        return None;
    }
    imports.get(receiver.utf8_text(source).ok()?).copied()
}

// module 直下で import し、出現がその宣言・引数の読み取り・属性の受け手に限られる名前。
fn stable_module_imports<'a>(root: Node<'_>, source: &'a [u8]) -> HashMap<&'a str, &'a str> {
    let mut names = HashMap::new();
    let mut declarations = HashSet::new();
    let mut cursor = root.walk();
    for stmt in root
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "import_statement")
    {
        let mut cursor = stmt.walk();
        for import in stmt.children_by_field_name("name", &mut cursor) {
            if let Some(path) = import.child_by_field_name("name") {
                let mut cursor = path.walk();
                declarations.extend(path.named_children(&mut cursor).map(|n| n.id()));
            }
            let (name, module) = match import.kind() {
                "aliased_import" => (
                    import.child_by_field_name("alias"),
                    field_text(import, "name", source),
                ),
                "dotted_name" if import.named_child_count() == 1 => {
                    (import.named_child(0), import.utf8_text(source).ok())
                }
                _ => continue,
            };
            if let (Some(binding), Some(module)) = (name, module)
                && let Ok(name) = binding.utf8_text(source)
            {
                if !names.contains_key(name) {
                    declarations.insert(binding.id());
                }
                names.insert(name, module);
            }
        }
    }
    if names.is_empty() {
        return names;
    }
    let mut ancestors: Vec<Node<'_>> = Vec::new();
    let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
        ancestors.truncate(depth);
        if node.kind() == "wildcard_import"
            || (node.kind() == "identifier"
                && matches!(
                    node.utf8_text(source),
                    Ok("exec" | "eval" | "globals" | "locals")
                ))
        {
            names.clear();
            return ControlFlow::Break(());
        }
        if node.kind() == "identifier"
            && !declarations.contains(&node.id())
            && let Ok(name) = node.utf8_text(source)
            && names.contains_key(name)
            && !ancestors.last().is_some_and(|parent| {
                parent.kind() == "argument_list"
                    || (parent.kind() == "attribute"
                        && parent
                            .child_by_field_name("object")
                            .is_some_and(|object| object.id() == node.id()))
            })
        {
            names.remove(name);
        }
        ancestors.push(node);
        ControlFlow::Continue(Walk::Descend)
    });
    names
}

fn typing_metadata(root: Node<'_>, source: &[u8]) -> HashMap<String, usize> {
    let mut result = HashMap::new();
    let bindings = std::cell::LazyCell::new(|| collect_binding_counts(root, source));
    for (name, type_args) in [("Literal", 0), ("Annotated", 1)] {
        if let Some(names) = collect_typing_names(root, source, name) {
            result.extend(
                names
                    .iter()
                    .filter(|n| n.is_provable(&bindings, name))
                    .map(|n| (n.base_text(name), type_args)),
            );
        }
    }
    result
}

fn field_text<'a>(node: Node<'_>, field: &str, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name(field)?.utf8_text(source).ok()
}

fn leaf_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let node = ungroup(node);
    match node.kind() {
        "identifier" => node.utf8_text(source).ok(),
        "attribute" => field_text(node, "attribute", source),
        _ => None,
    }
}

fn ungroup(mut node: Node<'_>) -> Node<'_> {
    while node.kind() == "parenthesized_expression" {
        let mut cursor = node.walk();
        let Some(inner) = node.named_children(&mut cursor).find(|n| !n.is_extra()) else {
            break;
        };
        node = inner;
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(dir: &Path) -> (bool, Vec<TruncationInfo>) {
        let mut dead = vec![ApiSymbolCandidate {
            file: "pkg/api.py".into(),
            name: "total".into(),
            kind: "function".into(),
            inline_module: None,
            signature: String::new(),
        }];
        let mut live = vec![];
        let mut notices = vec![];
        retain_referenced(
            dir.to_str().unwrap(),
            &[],
            &mut live,
            &mut dead,
            &mut notices,
        );
        (!live.is_empty(), notices)
    }

    #[test]
    fn dynamic_name_positions_distinguish_names_from_arguments_and_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("caller.py");
        for (source, expected) in [
            ("hasattr(module, 'total')", true),
            ("setattr(module, 'other', 'total')", false),
            ("delattr(module, 'total')", true),
            (
                "from operator import attrgetter as read\nread('child.total')",
                true,
            ),
            (
                "from operator import methodcaller as call\ncall('other', 'total')",
                false,
            ),
            (
                "from unittest.mock import patch as edit\nedit.object(module, attribute='total')",
                true,
            ),
            ("monkeypatch.setattr(module, 'total', replacement)", true),
            ("getattr(module, 't' r'ota' '\\x6c')", true),
            (
                "print(globals()['other'], 'total', records['total'])",
                false,
            ),
            ("globals()['total']", false),
            ("from pkg.api import *\nglobals()['total']", true),
            ("getattr(unknown, dynamic_name)", true),
            (
                "from unittest.mock import patch\npatch('elsewhere.api.total')",
                false,
            ),
            (
                "from unittest.mock import patch\npatch('pkg.api.total.__doc__')",
                true,
            ),
        ] {
            std::fs::write(&path, source).unwrap();
            assert_eq!(classify(tmp.path()), (expected, vec![]), "{source}");
        }
    }

    #[test]
    fn unreadable_or_incomplete_python_sources_keep_the_removal_and_report_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("caller.py");
        std::fs::write(&path, "print('total')").unwrap();
        assert_eq!(classify(tmp.path()), (false, vec![]));
        for source in [b"def unfinished(".as_slice(), &[0xff]] {
            std::fs::write(&path, source).unwrap();
            let (live, notices) = classify(tmp.path());
            assert!(live);
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0].path.as_deref(), Some("caller.py"));
        }
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_SOURCE + 1)
            .unwrap();
        let (live, notices) = classify(tmp.path());
        assert!(live && notices[0].message.contains("2 MiB"));
    }

    #[test]
    fn deep_expressions_preserve_later_string_name_lookups() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("pkg")).unwrap();
        let path = tmp.path().join("pkg/api.py");
        let grouped = format!("{}1{}", "(".repeat(50_000), ")".repeat(50_000));
        let sum = format!("{}1", "1 + ".repeat(2_000));
        for (expression, label) in [(grouped, "total"), (sum, "label")] {
            for (key, expected) in [("subtotal", false), ("total", true)] {
                let source = format!(
                    "from typing import Literal\nv = {expression}\nkind: Literal['{label}']\nresult = globals()['{key}']\n"
                );
                std::fs::write(&path, source).unwrap();
                assert_eq!(classify(tmp.path()), (expected, vec![]), "{key}, {label}");
            }
        }
    }
}
