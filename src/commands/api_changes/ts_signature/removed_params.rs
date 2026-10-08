//! 末尾の省略可能引数削除と、リポジトリ内の直接呼び出しの証拠。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use tree_sitter::Node;

use super::*;
use crate::commands::api_changes::ts_const_arg::collect_bindings_named;
use crate::commands::dead_code_default_liveness::{resolve_relative_specifier, static_specifier};
use crate::engine::parser::SourceBuf;
use crate::engine::refs::{FileScanOptions, collect_files_scan};
use crate::git_support::normalize_workspace_separators;
use crate::language::LangId;

const MAX_FILES: usize = 512;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODES: usize = 100_000;
const MAX_DEPTH: usize = 128;

struct RemovalFile {
    source: SourceBuf,
    tree: tree_sitter::Tree,
    lang: LangId,
}

struct RemovalInventory {
    files: BTreeMap<String, RemovalFile>,
    paths: HashMap<String, Vec<String>>,
}

/// 一回の API 検出に閉じる。取得失敗も記録し、証明を再試行しない。
#[derive(Default)]
pub(crate) struct OptionalRemovalCache {
    inventory: Option<Option<RemovalInventory>>,
}

impl OptionalRemovalCache {
    fn get(&mut self, dir: &str) -> Option<&RemovalInventory> {
        self.inventory
            .get_or_insert_with(|| load_inventory(dir))
            .as_ref()
    }
}

pub(crate) fn detect_trailing_optional_params_removed_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut OptionalRemovalCache,
) -> Option<CompatibleApiModification> {
    if site.kind != "function" || site.name.contains('.') {
        return None;
    }
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    let new_arity = removal_arity(
        old_tree.root_node(),
        &src.old,
        new_tree.root_node(),
        &src.new,
        site.name,
    )?;
    let refs = index.refs_for(site.name)?;
    if refs.is_empty() {
        return None;
    }
    let inventory = cache.get(site.dir)?;
    if inventory.files.get(site.new_path)?.source.as_ref() != src.new.as_ref() {
        return None;
    }
    let mut proven_positions = HashSet::new();
    let mut calls = 0;
    for (path, file) in &inventory.files {
        calls += prove_file(
            file,
            path,
            site.new_path,
            site.name,
            new_arity,
            &inventory.paths,
            &mut proven_positions,
        )?;
    }
    if calls == 0
        || refs.iter().any(|reference| {
            !proven_positions.contains(&(
                normalize_workspace_separators(&reference.path),
                reference.line,
                reference.column,
            ))
        })
    {
        return None;
    }
    Some(site.compatible("trailing_optional_params_removed"))
}

fn checked_nodes(root: Node<'_>) -> Option<Vec<Node<'_>>> {
    if root.has_error() {
        return None;
    }
    let mut result = Vec::new();
    let mut stack = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth > MAX_DEPTH || result.len() >= MAX_NODES || node.is_missing() || node.is_error() {
            return None;
        }
        result.push(node);
        if node.child_count() as usize > MAX_NODES {
            return None;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor).map(|child| (child, depth + 1)));
    }
    Some(result)
}

fn unique_function<'a>(root: Node<'a>, source: &[u8], name: &str) -> Option<(Node<'a>, Node<'a>)> {
    if checked_nodes(root)?
        .into_iter()
        .any(|node| escaped_identifier(node, source))
    {
        return None;
    }
    let bindings = collect_bindings_named(root, source, name);
    let [function] = bindings.as_slice() else {
        return None;
    };
    let function = *function;
    if function.kind() != "function_declaration"
        || function.child_by_field_name("name")?.kind() != "identifier"
        || function.child_by_field_name("body")?.kind() != "statement_block"
        || function.child_by_field_name("return_type")?.kind() != "type_annotation"
        || function.child_by_field_name("type_parameters").is_some()
    {
        return None;
    }
    let parent = function.parent()?;
    let statement = match parent.kind() {
        "program" => function,
        "export_statement"
            if parent.parent()?.kind() == "program"
                && parent.child_by_field_name("declaration")?.id() == function.id() =>
        {
            parent
        }
        _ => return None,
    };
    let mut cursor = statement.walk();
    if statement
        .children(&mut cursor)
        .any(|child| child.kind() == "default")
    {
        return None;
    }
    let params = function.child_by_field_name("parameters")?;
    for node in checked_nodes(params)? {
        if node.kind() == "rest_pattern"
            || (matches!(node.kind(), "required_parameter" | "optional_parameter")
                && node.child_by_field_name("pattern").is_some_and(|pattern| {
                    pattern.kind() == "this" || pattern.utf8_text(source).ok() == Some("this")
                }))
        {
            return None;
        }
    }
    Some((function, statement))
}

fn removal_arity(
    old: Node<'_>,
    old_source: &[u8],
    new: Node<'_>,
    new_source: &[u8],
    name: &str,
) -> Option<usize> {
    let (old_fn, old_statement) = unique_function(old, old_source, name)?;
    let (new_fn, new_statement) = unique_function(new, new_source, name)?;
    let old_parts = ts_function_signature_parts(old_fn, old_source)?;
    let new_parts = ts_function_signature_parts(new_fn, new_source)?;
    let header = |statement: Node<'_>, function: Node<'_>, source: &[u8]| {
        signature_tokens_in_range(
            statement,
            source,
            statement.start_byte(),
            function.child_by_field_name("parameters")?.start_byte(),
        )
    };
    if header(old_statement, old_fn, old_source)? != header(new_statement, new_fn, new_source)?
        || old_parts.head != new_parts.head
        || old_parts.tail != new_parts.tail
        || old_parts.tail.0.is_empty()
        || !ts_params_prefix_same_with_optional_tail(&new_parts.params, &old_parts.params)
    {
        return None;
    }
    Some(new_parts.params.len())
}

fn load_inventory(dir: &str) -> Option<RemovalInventory> {
    let root = Path::new(dir).canonicalize().ok()?;
    let collection = collect_files_scan(
        &root,
        None,
        FileScanOptions {
            include_generated: true,
        },
    )
    .ok()?;
    if collection.files.len() > MAX_FILES || !collection.unanalyzable_truncations(&root).is_empty()
    {
        return None;
    }
    let mut files = BTreeMap::new();
    let mut bytes = 0usize;
    let mut nodes = 0usize;
    for path in collection.files {
        let path = path.canonicalize().ok()?;
        let relative =
            normalize_workspace_separators(&path.strip_prefix(&root).ok()?.to_string_lossy());
        let utf8 = camino::Utf8Path::from_path(&path)?;
        let source = parser::read_file(utf8).ok()?;
        bytes = bytes.checked_add(source.len())?;
        if bytes > MAX_BYTES {
            return None;
        }
        let (tree, lang) = parser::parse_file(utf8, &source).ok()?;
        // 他言語の依存や埋め込み構文は、この閉包証明では解決しない。
        if !matches!(lang, LangId::Typescript | LangId::Tsx | LangId::Javascript) {
            return None;
        }
        nodes = nodes.checked_add(checked_nodes(tree.root_node())?.len())?;
        if nodes > MAX_NODES {
            return None;
        }
        files.insert(relative, RemovalFile { source, tree, lang });
    }
    let paths = files
        .keys()
        .map(|path| (path.clone(), Vec::new()))
        .collect();
    Some(RemovalInventory { files, paths })
}

fn dependency_target(
    node: Node<'_>,
    source: &[u8],
    path: &str,
    paths: &HashMap<String, Vec<String>>,
) -> Option<String> {
    if !matches!(node.kind(), "string" | "template_string") {
        return None;
    }
    let specifier = static_specifier(node, source)?;
    let candidates = resolve_relative_specifier(path, specifier, paths);
    let [target] = candidates.as_slice() else {
        return None;
    };
    Some(target.clone())
}

fn direct_call_arity(identifier: Node<'_>) -> Option<usize> {
    let call = identifier.parent()?;
    if call.kind() != "call_expression"
        || call.child_by_field_name("function")?.id() != identifier.id()
        || call.child_by_field_name("type_arguments").is_some()
    {
        return None;
    }
    let mut cursor = call.walk();
    if call.children(&mut cursor).any(|child| child.kind() == "?.") {
        return None;
    }
    let arguments = call.child_by_field_name("arguments")?;
    if arguments.kind() != "arguments" {
        return None;
    }
    let mut cursor = arguments.walk();
    let mut count = 0;
    for argument in arguments.named_children(&mut cursor) {
        match argument.kind() {
            "comment" => {}
            "spread_element" => return None,
            _ => count += 1,
        }
    }
    Some(count)
}

fn prove_file(
    file: &RemovalFile,
    path: &str,
    target: &str,
    name: &str,
    arity: usize,
    paths: &HashMap<String, Vec<String>>,
    positions: &mut HashSet<(String, usize, usize)>,
) -> Option<usize> {
    let root = file.tree.root_node();
    let source = &file.source;
    let nodes = checked_nodes(root)?;
    let mut import_names = HashSet::new();
    let mut import_statements = HashSet::new();
    for node in &nodes {
        if escaped_identifier(*node, source) || dynamic_execution_entry(*node, source) {
            return None;
        }
        // 動的なモジュール列挙と require の別名化は依存を閉じられない。
        if node.kind() == "meta_property" {
            let mut cursor = node.walk();
            if node
                .children(&mut cursor)
                .any(|child| child.kind() == "import")
            {
                return None;
            }
        }
        if node.kind() == "identifier" && node.utf8_text(source).ok()? == "require" {
            let parent = node.parent()?;
            if parent.kind() != "call_expression"
                || parent.child_by_field_name("function")?.id() != node.id()
            {
                return None;
            }
        }
        match node.kind() {
            "import_statement" | "export_statement" => {
                if let Some(specifier) = node.child_by_field_name("source")
                    && dependency_target(specifier, source, path, paths)? == target
                {
                    if node.kind() != "import_statement" {
                        return None;
                    }
                    let mut cursor = node.walk();
                    if node
                        .children(&mut cursor)
                        .any(|child| child.kind() == "type")
                    {
                        return None;
                    }
                    let clause = node
                        .named_children(&mut cursor)
                        .find(|child| child.kind() == "import_clause")?;
                    let mut cursor = clause.walk();
                    let children: Vec<_> = clause
                        .named_children(&mut cursor)
                        .filter(|child| child.kind() != "comment")
                        .collect();
                    let [named] = children.as_slice() else {
                        return None;
                    };
                    if named.kind() != "named_imports" {
                        return None;
                    }
                    let mut cursor = named.walk();
                    for spec in named.named_children(&mut cursor) {
                        if spec.kind() == "comment" {
                            continue;
                        }
                        if spec.kind() != "import_specifier"
                            || spec.child_by_field_name("alias").is_some()
                        {
                            return None;
                        }
                        let mut cursor = spec.walk();
                        if spec
                            .children(&mut cursor)
                            .any(|child| child.kind() == "type")
                        {
                            return None;
                        }
                        let imported = spec.child_by_field_name("name")?;
                        if imported.kind() != "identifier" {
                            return None;
                        }
                        if imported.utf8_text(source).ok()? == name {
                            import_names.insert(imported.id());
                            import_statements.insert(node.id());
                        }
                    }
                }
            }
            "import_alias" | "import_require_clause" => return None,
            "internal_module" | "module" => {
                if let Some(specifier) = node.child_by_field_name("name")
                    && specifier.kind() == "string"
                    && dependency_target(specifier, source, path, paths)? == target
                {
                    return None;
                }
            }
            "call_expression" => {
                let function = node.child_by_field_name("function")?;
                if function.kind() == "import"
                    || (function.kind() == "identifier"
                        && function.utf8_text(source).ok()? == "require")
                {
                    let arguments = node.child_by_field_name("arguments")?;
                    let mut cursor = arguments.walk();
                    let first = arguments
                        .named_children(&mut cursor)
                        .find(|child| child.kind() != "comment")?;
                    if dependency_target(first, source, path, paths)? == target {
                        return None;
                    }
                }
            }
            _ => {}
        }
    }
    let bindings = collect_bindings_named(root, source, name);
    let declaration_name = if path == target {
        unique_function(root, source, name)?
            .0
            .child_by_field_name("name")
            .map(|node| node.id())
    } else {
        if !bindings.is_empty()
            && (import_names.len() != 1
                || bindings.iter().any(|binding| {
                    !import_statements.contains(&binding.id())
                        && !(binding.kind() == "import_specifier"
                            && binding
                                .child_by_field_name("name")
                                .is_some_and(|node| import_names.contains(&node.id())))
                }))
        {
            return None;
        }
        None
    };
    let mut calls = 0;
    for node in nodes {
        if !matches!(
            node.kind(),
            "identifier"
                | "type_identifier"
                | "property_identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
                | "statement_identifier"
        ) || node.utf8_text(source).ok()? != name
        {
            continue;
        }
        if !matches!(file.lang, LangId::Typescript | LangId::Tsx) {
            return None;
        }
        let position = node.start_position();
        positions.insert((path.to_string(), position.row, position.column));
        if declaration_name == Some(node.id()) || import_names.contains(&node.id()) {
            continue;
        }
        if path != target && import_names.is_empty() {
            return None;
        }
        if node.kind() != "identifier" || direct_call_arity(node)? > arity {
            return None;
        }
        calls += 1;
    }
    Some(calls)
}

fn dynamic_execution_entry(node: Node<'_>, source: &[u8]) -> bool {
    if matches!(node.kind(), "identifier" | "property_identifier")
        && matches!(node.utf8_text(source).ok(), Some("eval" | "Function"))
    {
        return true;
    }
    // グローバルの添字アクセスは、既知の実行入口の別名になり得る。
    node.kind() == "subscript_expression"
        && node.child_by_field_name("object").is_some_and(|object| {
            object.kind() == "identifier"
                && matches!(
                    object.utf8_text(source).ok(),
                    Some("globalThis" | "window" | "self" | "global")
                )
        })
}

fn escaped_identifier(node: Node<'_>, source: &[u8]) -> bool {
    matches!(
        node.kind(),
        "identifier"
            | "type_identifier"
            | "property_identifier"
            | "private_property_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
            | "statement_identifier"
    ) && node.utf8_text(source).is_ok_and(|text| text.contains('\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_removal_requires_unique_explicit_declaration_contract() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            let old = "export function spacing(size: number, scale = 1): number { return size; }";
            let old_tree = parser::parse_source(old.as_bytes(), lang).unwrap();
            for (new, accepted) in [
                (
                    "export function spacing(size: number): number { return size; }",
                    true,
                ),
                (
                    "export function spacing(size: string): number { return 1; }",
                    false,
                ),
                (
                    "export function spacing(size: number): string { return ''; }",
                    false,
                ),
                (
                    "export function spacing(size: number) { return size; }",
                    false,
                ),
                (
                    "export default function spacing(size: number): number { return size; }",
                    false,
                ),
                (
                    "export function spacing(size: number): number { return size; } type spacing = number;",
                    false,
                ),
                (
                    "export function spacing(size: number): number; export function spacing(size: number): number { return size; }",
                    false,
                ),
                (
                    "export function spacing(size: number): number { return size; } const broken = ;",
                    false,
                ),
            ] {
                let tree = parser::parse_source(new.as_bytes(), lang).unwrap();
                assert_eq!(
                    removal_arity(
                        old_tree.root_node(),
                        old.as_bytes(),
                        tree.root_node(),
                        new.as_bytes(),
                        "spacing"
                    )
                    .is_some(),
                    accepted,
                    "{lang:?}: {new}"
                );
            }
        }
    }

    #[test]
    fn optional_removal_counts_only_exact_direct_call_arguments() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (source, expected) in [
                ("spacing();", Some(0)),
                ("spacing(/* before */ 1, /* tail */);", Some(1)),
                ("spacing(undefined);", Some(1)),
                ("spacing(1, undefined);", Some(2)),
                ("spacing(...values);", None),
                ("(spacing)();", None),
                ("new spacing();", None),
                ("consume(spacing);", None),
                ("spacing?.();", None),
                ("spacing<string>();", None),
                ("spacing.call(null);", None),
            ] {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                let identifier = checked_nodes(tree.root_node())
                    .unwrap()
                    .into_iter()
                    .find(|node| {
                        node.kind() == "identifier"
                            && node.utf8_text(source.as_bytes()).unwrap() == "spacing"
                    })
                    .unwrap();
                assert_eq!(
                    direct_call_arity(identifier),
                    expected,
                    "{lang:?}: {source}"
                );
            }
        }
    }

    #[test]
    fn optional_removal_module_and_call_node_contracts_exist() {
        let fixture = include_str!("../../../../tests/fixtures/optional_removal_contract.tsx");
        let tree = parser::parse_source(fixture.as_bytes(), LangId::Tsx).unwrap();
        assert!(!tree.root_node().has_error());
        for lang in [LangId::Typescript, LangId::Tsx] {
            let grammar = lang.ts_language();
            for query in [
                "(function_declaration name: (identifier) parameters: (formal_parameters) return_type: (type_annotation) body: (statement_block))",
                "(required_parameter)",
                "(optional_parameter)",
                "(rest_pattern)",
                "(import_statement source: (string))",
                "(import_specifier name: (identifier))",
                "(import_specifier alias: (identifier))",
                "(namespace_import)",
                "(import_require_clause)",
                "(import_alias)",
                "(internal_module)",
                "(module name: (string))",
                "(meta_property \"import\")",
                "(call_expression function: (identifier) arguments: (arguments))",
                "(call_expression type_arguments: (type_arguments))",
                "(call_expression \"?.\")",
                "(spread_element)",
                "(new_expression)",
                "(identifier)",
                "(type_identifier)",
                "(property_identifier)",
                "(private_property_identifier)",
                "(shorthand_property_identifier)",
                "(shorthand_property_identifier_pattern)",
                "(statement_identifier)",
                "(subscript_expression object: (identifier) index: (string))",
                "(export_statement source: (string))",
            ] {
                tree_sitter::Query::new(&grammar, query)
                    .unwrap_or_else(|error| panic!("{lang:?}: {query}: {error}"));
            }
        }
    }
}
