//! 省略可能引数の削除を、解析範囲内の変更後の直接呼び出しで閉じる。

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use tree_sitter::{Node, Tree};

use super::python_call_arity::CallContract;
use super::ref_index::ApiRefIndex;
use super::signature_tokens::{SigTokens, node_signature_tokens, signature_tokens_in_range};
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::engine::{parser, refs};
use crate::git_support::normalize_workspace_separators;
use crate::language::LangId;

const MAX_FILES: usize = 512;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE: usize = 2 * 1024 * 1024;
const MAX_NODES: usize = 100_000;
const MAX_DEPTH: usize = 128;

struct File {
    source: parser::SourceBuf,
    tree: Tree,
}

struct Inventory(BTreeMap<String, File>);

#[derive(Default)]
pub(super) struct RemovalCache {
    inventory: Option<Option<Inventory>>,
}

/// `None` は対象外。削除形を確認した後の証明失敗は、行だけの閉包へ戻さない。
pub(super) fn prove_removal(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut RemovalCache,
) -> Option<bool> {
    if site.lang_id != Some(LangId::Python)
        || site.kind != "function"
        || site.name.contains('.')
        || !site.name.is_ascii()
        || site.old_path != site.new_path
    {
        return None;
    }
    let pair = sources.get(site)?;
    let (old, new) = pair.parse_pair(LangId::Python)?;
    let old_fn = unique_function(old.root_node(), &pair.old, site.name)?;
    let new_fn = unique_function(new.root_node(), &pair.new, site.name)?;
    removal_contract(old_fn, &pair.old, new_fn, &pair.new)?;
    let inventory = cache
        .inventory
        .get_or_insert_with(|| Inventory::load(site.dir));
    Some(
        inventory
            .as_ref()
            .is_some_and(|inventory| inventory.proves(index, site, &pair.new).is_some()),
    )
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

fn checked_nodes(root: Node<'_>) -> Option<Vec<Node<'_>>> {
    if root.has_error() {
        return None;
    }
    let mut nodes = Vec::new();
    let mut pending = vec![(root, 0)];
    while let Some((node, depth)) = pending.pop() {
        if depth > MAX_DEPTH || nodes.len() >= MAX_NODES || node.is_missing() || node.is_error() {
            return None;
        }
        if node.child_count() as usize > MAX_NODES {
            return None;
        }
        nodes.push(node);
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor).map(|child| (child, depth + 1)));
    }
    Some(nodes)
}

fn unique_function<'a>(root: Node<'a>, source: &[u8], name: &str) -> Option<Node<'a>> {
    let mut found = None;
    for node in checked_nodes(root)? {
        if node.kind() != "identifier" || text(node, source)? != name {
            continue;
        }
        let parent = node.parent()?;
        if parent.kind() == "function_definition"
            && parent.child_by_field_name("name")?.id() == node.id()
            && parent.parent()?.kind() == "module"
            && parent.child_by_field_name("type_parameters").is_none()
        {
            if found.replace(parent).is_some() {
                return None;
            }
        } else if parent.kind() != "call"
            || parent.child_by_field_name("function")?.id() != node.id()
        {
            return None;
        }
    }
    found
}

#[derive(PartialEq, Eq)]
struct Parameter {
    name: String,
    tokens: SigTokens,
    optional: bool,
    keyword_only: bool,
}

fn parameters(function: Node<'_>, source: &[u8]) -> Option<Vec<Parameter>> {
    let list = function.child_by_field_name("parameters")?;
    let mut result = Vec::new();
    let mut keyword_only = false;
    let mut names = HashSet::new();
    let mut cursor = list.walk();
    for node in list.named_children(&mut cursor) {
        match node.kind() {
            "comment" => continue,
            "keyword_separator" if !keyword_only => {
                keyword_only = true;
                continue;
            }
            "identifier" | "typed_parameter" | "default_parameter" | "typed_default_parameter" => {}
            _ => return None,
        }
        let name = if node.kind() == "identifier" {
            node
        } else {
            node.child_by_field_name("name")
                .or_else(|| node.named_child(0))?
        };
        if name.kind() != "identifier" || result.len() >= 64 {
            return None;
        }
        let name = text(name, source)?.to_owned();
        if !names.insert(name.clone()) {
            return None;
        }
        result.push(Parameter {
            name,
            tokens: node_signature_tokens(node, source)?,
            optional: matches!(node.kind(), "default_parameter" | "typed_default_parameter"),
            keyword_only,
        });
    }
    Some(result)
}

fn removal_contract(
    old: Node<'_>,
    old_source: &[u8],
    new: Node<'_>,
    new_source: &[u8],
) -> Option<()> {
    let old_params = old.child_by_field_name("parameters")?;
    let new_params = new.child_by_field_name("parameters")?;
    for (old_range, new_range) in [
        (
            old.start_byte()..old_params.start_byte(),
            new.start_byte()..new_params.start_byte(),
        ),
        (
            old_params.end_byte()..old.child_by_field_name("body")?.start_byte(),
            new_params.end_byte()..new.child_by_field_name("body")?.start_byte(),
        ),
    ] {
        if signature_tokens_in_range(old, old_source, old_range.start, old_range.end)?
            != signature_tokens_in_range(new, new_source, new_range.start, new_range.end)?
        {
            return None;
        }
    }
    let old = parameters(old, old_source)?;
    let new = parameters(new, new_source)?;
    if old.len() <= new.len() {
        return None;
    }
    let mut kept = 0;
    let mut removed_positional = false;
    for param in &old {
        if new.get(kept).is_some_and(|next| next.name == param.name) {
            if new.get(kept)? != param || (removed_positional && !param.keyword_only) {
                return None;
            }
            kept += 1;
        } else {
            if !param.optional {
                return None;
            }
            removed_positional |= !param.keyword_only;
        }
    }
    (kept == new.len()).then_some(())
}

fn linked(path: &Path) -> Option<bool> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Some(metadata.file_attributes() & 0x400 != 0)
    }
    #[cfg(not(windows))]
    {
        Some(metadata.file_type().is_symlink())
    }
}

impl Inventory {
    fn load(dir: &str) -> Option<Self> {
        if linked(Path::new(dir))? {
            return None;
        }
        let root = Path::new(dir).canonicalize().ok()?;
        // refs と同じ ignore/hidden 範囲で、リンクと走査失敗も不在扱いにしない。
        for (count, entry) in ignore::WalkBuilder::new(&root).build().enumerate() {
            let entry = entry.ok()?;
            if count >= MAX_NODES || linked(entry.path())? {
                return None;
            }
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                std::fs::File::open(entry.path()).ok()?;
            }
        }
        let collection = refs::collect_files_scan(
            &root,
            None,
            refs::FileScanOptions {
                include_generated: true,
            },
        )
        .ok()?;
        if collection.files.len() > MAX_FILES
            || !collection.unanalyzable_truncations(&root).is_empty()
        {
            return None;
        }
        let mut files = BTreeMap::new();
        let mut bytes = 0;
        let mut nodes = 0;
        for path in collection.files {
            let path = path.canonicalize().ok()?;
            let relative = normalize_workspace_separators(path.strip_prefix(&root).ok()?.to_str()?);
            let utf8 = camino::Utf8Path::from_path(&path)?;
            let source = parser::read_file(utf8).ok()?;
            bytes += source.len();
            if source.len() > MAX_SOURCE || bytes > MAX_BYTES {
                return None;
            }
            let (tree, lang) = parser::parse_file(utf8, &source).ok()?;
            if lang != LangId::Python {
                return None;
            }
            nodes += checked_nodes(tree.root_node())?.len();
            if nodes > MAX_NODES {
                return None;
            }
            files.insert(relative, File { source, tree });
        }
        Some(Self(files))
    }

    fn proves(
        &self,
        index: &ApiRefIndex,
        site: &CompatibleModSite<'_>,
        source: &[u8],
    ) -> Option<()> {
        let target = self.0.get(site.new_path)?;
        if target.source.as_ref() != source {
            return None;
        }
        let function = unique_function(target.tree.root_node(), source, site.name)?;
        let contract = CallContract::from_function(function, source)?;
        let module = site
            .new_path
            .strip_prefix("src/")
            .unwrap_or(site.new_path)
            .strip_suffix(".py")?
            .replace('/', ".");
        if module.ends_with(".__init__") {
            return None;
        }
        let mut positions = HashSet::new();
        let mut calls = 0;
        for (path, file) in &self.0 {
            calls += prove_file(file, path, site, &module, &contract, &mut positions)?;
        }
        if calls == 0
            || index.complete_refs_for(site.name)?.iter().any(|reference| {
                !positions.contains(&(
                    normalize_workspace_separators(&reference.path),
                    reference.line,
                    reference.column,
                ))
            })
        {
            return None;
        }
        Some(())
    }
}

fn targets_module(module: &str, target: &str) -> bool {
    module == target
        || target
            .strip_prefix(module)
            .is_some_and(|rest| rest.starts_with('.'))
}

fn prove_file(
    file: &File,
    path: &str,
    site: &CompatibleModSite<'_>,
    module: &str,
    contract: &CallContract,
    positions: &mut HashSet<(String, usize, usize)>,
) -> Option<usize> {
    let source = &file.source;
    let nodes = checked_nodes(file.tree.root_node())?;
    let mut imports = HashSet::new();
    for &node in &nodes {
        if node.kind() == "identifier"
            && (!text(node, source)?.is_ascii()
                || matches!(
                    text(node, source)?,
                    "eval"
                        | "exec"
                        | "compile"
                        | "globals"
                        | "locals"
                        | "vars"
                        | "getattr"
                        | "setattr"
                        | "delattr"
                        | "__import__"
                        | "importlib"
                        | "sys"
                        | "__builtins__"
                        | "__dict__"
                        | "__getattribute__"
                        | "__getattr__"
                        | "__all__"
                ))
        {
            return None;
        }
        if !matches!(node.kind(), "import_statement" | "import_from_statement") {
            continue;
        }
        if node.parent()?.kind() != "module" {
            return None;
        }
        let from = node.child_by_field_name("module_name");
        let from_text = if let Some(from) = from {
            if from.kind() != "dotted_name" {
                return None;
            }
            Some(text(from, source)?)
        } else {
            None
        };
        let mut cursor = node.walk();
        for imported in node.named_children(&mut cursor).filter(|child| {
            child.kind() != "comment" && Some(child.id()) != from.map(|from| from.id())
        }) {
            let name_node = if imported.kind() == "aliased_import" {
                imported.child_by_field_name("name")?
            } else {
                imported
            };
            if name_node.kind() != "dotted_name" {
                return None;
            }
            let name = text(name_node, source)?;
            let Some(from) = from_text else {
                if targets_module(name, module) {
                    return None;
                }
                continue;
            };
            if targets_module(&format!("{from}.{name}"), module) {
                return None;
            }
            if name != site.name {
                continue;
            }
            if from != module
                || imported.kind() != "dotted_name"
                || name_node.named_child_count() != 1
                || super::python_module_path::resolve(site.dir, from).as_deref()
                    != Some(site.new_path)
            {
                return None;
            }
            imports.insert(name_node.named_child(0)?.id());
        }
    }
    let declaration = if path == site.new_path {
        Some(
            unique_function(file.tree.root_node(), source, site.name)?
                .child_by_field_name("name")?
                .id(),
        )
    } else {
        None
    };
    if imports.len() > 1 || (declaration.is_some() && !imports.is_empty()) {
        return None;
    }
    let mut calls = 0;
    for node in nodes {
        if node.kind() != "identifier" || text(node, source)? != site.name {
            continue;
        }
        let point = node.start_position();
        positions.insert((path.to_owned(), point.row, point.column));
        if declaration == Some(node.id()) || imports.contains(&node.id()) {
            continue;
        }
        if declaration.is_none() && imports.is_empty() {
            return None;
        }
        let call = node.parent()?;
        if call.kind() != "call"
            || call.child_by_field_name("function")?.id() != node.id()
            || !contract.accepts_call(call, source)
        {
            return None;
        }
        calls += 1;
    }
    Some(calls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_optional_removal_nodes_exist_and_match_real_syntax() {
        let language = LangId::Python.ts_language();
        for kind in [
            "identifier",
            "function_definition",
            "module",
            "call",
            "parameters",
            "comment",
            "keyword_separator",
            "typed_parameter",
            "default_parameter",
            "typed_default_parameter",
            "import_statement",
            "import_from_statement",
            "dotted_name",
            "aliased_import",
        ] {
            tree_sitter::Query::new(&language, &format!("({kind}) @node")).unwrap();
        }
        let source = b"from pkg.conv import convert\nimport other as module\ndef convert(data: bytes, *, limit: int = 10, hook=None) -> str:\n    return convert(data)\n";
        let tree = parser::parse_source(source, LangId::Python).unwrap();
        let function = unique_function(tree.root_node(), source, "missing");
        assert!(function.is_none());
        let mut cursor = tree.root_node().walk();
        let function = tree
            .root_node()
            .named_children(&mut cursor)
            .find(|node| node.kind() == "function_definition")
            .unwrap();
        let params = parameters(function, source).unwrap();
        assert_eq!(params.len(), 3);
        assert!(!params[0].keyword_only);
        assert!(params[1].keyword_only && params[1].optional);
        assert!(params[2].keyword_only && params[2].optional);
    }
}
