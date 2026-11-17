//! 限定した data class の末尾既定値追加を、解析範囲内の Kotlin ソースで証明する。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use tree_sitter::{Node, Tree};

use super::ref_index::ApiRefIndex;
use super::signature_tokens::{SigTokens, signature_tokens_in_range};
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::engine::{parser, refs};
use crate::git_support::normalize_workspace_separators;
use crate::language::LangId;
use crate::models::{impact::DiffFile, review::CompatibleApiModification};

const MAX_FILES: usize = 512;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE: usize = 2 * 1024 * 1024;
const MAX_NODES: usize = 100_000;
const MAX_DEPTH: usize = 128;
const BUILD_FILE_NAMES: &[&str] = &[
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "pom.xml",
    "build.xml",
    "libs.versions.toml",
    "gradle.properties",
    "gradle.lockfile",
    "Makefile",
    "makefile",
    "CMakeLists.txt",
    "WORKSPACE",
    "WORKSPACE.bazel",
    "MODULE.bazel",
    "BUILD.bazel",
];

pub(super) enum Detection {
    Compatible(CompatibleApiModification),
    Unverified,
}

pub(super) fn detect(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    diff_files: &[DiffFile],
    index: &ApiRefIndex,
) -> Option<Detection> {
    if site.lang_id != Some(LangId::Kotlin) || site.kind != "class" {
        return None;
    }
    let data = sources.get(site).and_then(|pair| {
        if pair.old.len() > MAX_SOURCE || pair.new.len() > MAX_SOURCE {
            return None;
        }
        let (old, _) = pair.parse_pair(LangId::Kotlin)?;
        let class = class(old.root_node(), &pair.old, site.name)?;
        let mut cursor = class.walk();
        Some(class.named_children(&mut cursor).any(|child| {
            child.kind() == "modifiers"
                && text(child, &pair.old)
                    .is_some_and(|text| text.split_whitespace().any(|modifier| modifier == "data"))
        }))
    });
    if data == Some(false) {
        return None;
    }
    Some(match prove(site, sources, diff_files, index) {
        Some(compatible) => Detection::Compatible(compatible),
        None => Detection::Unverified,
    })
}

fn prove(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    diff_files: &[DiffFile],
    index: &ApiRefIndex,
) -> Option<CompatibleApiModification> {
    if !identifier(site.name)
        || site.old_path != site.new_path
        || !matches!(diff_files, [file] if file.old_path == site.old_path && file.new_path == site.new_path)
    {
        return None;
    }
    let pair = sources.get(site)?;
    if pair.old.len() > MAX_SOURCE || pair.new.len() > MAX_SOURCE {
        return None;
    }
    let (old, new) = pair.parse_pair(LangId::Kotlin)?;
    let old_class = class(old.root_node(), &pair.old, site.name)?;
    let new_class = class(new.root_node(), &pair.new, site.name)?;
    let contract = addition(old_class, &pair.old, new_class, &pair.new)?;
    // 型や既定値の束縛を変える別の変更を、この証明で覆わない。
    if pair.old[..old_class.start_byte()] != pair.new[..new_class.start_byte()]
        || pair.old[old_class.end_byte()..] != pair.new[new_class.end_byte()..]
    {
        return None;
    }
    let inventory = Inventory::load(site.dir)?;
    inventory.proves(site, &pair.new, &contract, index)?;
    Some(site.compatible("kotlin_trailing_default_constructor_params"))
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

fn identifier(name: &str) -> bool {
    let mut chars = name.bytes();
    chars
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && chars.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && name.bytes().any(|byte| byte != b'_')
}

struct Site<'a> {
    node: Node<'a>,
    parent: Option<Node<'a>>,
    grandparent: Option<Node<'a>>,
}

fn nodes(root: Node<'_>) -> Option<Vec<Site<'_>>> {
    if root.has_error() {
        return None;
    }
    let mut result = Vec::new();
    let mut pending = vec![(root, None, None, 0)];
    while let Some((node, parent, grandparent, depth)) = pending.pop() {
        if depth > MAX_DEPTH || result.len() >= MAX_NODES || node.is_error() || node.is_missing() {
            return None;
        }
        if node.child_count() as usize > MAX_NODES {
            return None;
        }
        let mut cursor = node.walk();
        pending.extend(
            node.children(&mut cursor)
                .map(|child| (child, Some(node), parent, depth + 1)),
        );
        result.push(Site {
            node,
            parent,
            grandparent,
        });
    }
    Some(result)
}

fn class<'a>(root: Node<'a>, source: &[u8], name: &str) -> Option<Node<'a>> {
    let mut found = None;
    for site in nodes(root)? {
        if site.node.kind() != "class_declaration" {
            continue;
        }
        let mut cursor = site.node.walk();
        let named = site
            .node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "type_identifier")?;
        if text(named, source)? == name
            && (site.parent?.id() != root.id() || found.replace(site.node).is_some())
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
    defaulted: bool,
}

struct Contract {
    added: BTreeSet<String>,
    scalar_names: BTreeSet<String>,
    old_parameters: Vec<(String, bool)>,
}

fn addition(
    old: Node<'_>,
    old_source: &[u8],
    new: Node<'_>,
    new_source: &[u8],
) -> Option<Contract> {
    let (old_constructor, old_params, mut scalar_names) = parameters(old, old_source)?;
    let (new_constructor, new_params, new_scalars) = parameters(new, new_source)?;
    if old_params.is_empty()
        || new_params.len() <= old_params.len()
        || old_params != new_params[..old_params.len()]
        || new_params[old_params.len()..]
            .iter()
            .any(|param| !param.defaulted)
        || signature_tokens_in_range(
            old,
            old_source,
            old.start_byte(),
            old_constructor.start_byte(),
        )? != signature_tokens_in_range(
            new,
            new_source,
            new.start_byte(),
            new_constructor.start_byte(),
        )?
    {
        return None;
    }
    scalar_names.extend(new_scalars);
    Some(Contract {
        added: new_params[old_params.len()..]
            .iter()
            .map(|param| param.name.clone())
            .collect(),
        scalar_names,
        old_parameters: old_params
            .into_iter()
            .map(|parameter| (parameter.name, parameter.defaulted))
            .collect(),
    })
}

fn parameters<'a>(
    class: Node<'a>,
    source: &[u8],
) -> Option<(Node<'a>, Vec<Parameter>, BTreeSet<String>)> {
    let mut constructor = None;
    let mut data = false;
    let mut cursor = class.walk();
    for child in class.named_children(&mut cursor) {
        match child.kind() {
            "modifiers" => {
                let modifiers: Vec<_> = text(child, source)?.split_whitespace().collect();
                if modifiers
                    .iter()
                    .any(|modifier| !matches!(*modifier, "public" | "final" | "data"))
                {
                    return None;
                }
                data = modifiers.contains(&"data");
            }
            "type_identifier" | "line_comment" | "multiline_comment" => {}
            "primary_constructor" if constructor.is_none() => constructor = Some(child),
            _ => return None,
        }
    }
    if !data {
        return None;
    }
    let constructor = constructor?;
    let mut parameters = Vec::new();
    let mut names = HashSet::new();
    let mut scalars = BTreeSet::new();
    let mut cursor = constructor.walk();
    for node in constructor.named_children(&mut cursor) {
        if matches!(node.kind(), "line_comment" | "multiline_comment") {
            continue;
        }
        if node.kind() != "class_parameter" {
            return None;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        let (name, ty, default) = match children.as_slice() {
            [binding, name, colon, ty]
                if matches!(binding.kind(), "val" | "var") && colon.kind() == ":" =>
            {
                (*name, *ty, None)
            }
            [binding, name, colon, ty, equals, default]
                if matches!(binding.kind(), "val" | "var")
                    && colon.kind() == ":"
                    && equals.kind() == "=" =>
            {
                (*name, *ty, Some(*default))
            }
            _ => return None,
        };
        if name.kind() != "simple_identifier" || !matches!(ty.kind(), "user_type" | "nullable_type")
        {
            return None;
        }
        let name = text(name, source)?;
        if !identifier(name) || !names.insert(name.to_owned()) {
            return None;
        }
        let ty = text(ty, source)?;
        let (scalar, nullable) = scalar(ty)?;
        if default.is_some_and(|default| !literal(default, source, scalar, nullable)) {
            return None;
        }
        scalars.insert(scalar.to_owned());
        parameters.push(Parameter {
            name: name.to_owned(),
            // parameter 全体の原文を保ち、文字・文字列リテラルの内容も比較する。
            tokens: SigTokens(vec![(
                node.kind().to_owned(),
                text(node, source)?.to_owned(),
            )]),
            defaulted: default.is_some(),
        });
    }
    Some((constructor, parameters, scalars))
}

fn scalar(ty: &str) -> Option<(&str, bool)> {
    let nullable = ty.ends_with('?');
    let ty = ty.strip_suffix('?').unwrap_or(ty);
    let ty = ty.strip_prefix("kotlin.").unwrap_or(ty);
    matches!(
        ty,
        "Byte" | "Short" | "Int" | "Long" | "Float" | "Double" | "Boolean" | "Char" | "String"
    )
    .then_some((ty, nullable))
}

fn literal(node: Node<'_>, source: &[u8], scalar: &str, nullable: bool) -> bool {
    let Some(value) = text(node, source) else {
        return false;
    };
    if nullable && value == "null" {
        return true;
    }
    match scalar {
        "Boolean" => matches!(value, "true" | "false"),
        "String" => {
            node.kind() == "string_literal"
                && value.starts_with('"')
                && value.ends_with('"')
                && !value.contains(['$', '\\', '\n', '\r'])
        }
        "Char" => {
            value.starts_with('\'')
                && value.ends_with('\'')
                && value.chars().count() == 3
                && !value.contains('\\')
                && value
                    .chars()
                    .nth(1)
                    .is_some_and(|character| u32::from(character) <= 0xffff)
        }
        "Byte" | "Short" | "Int" | "Long" => {
            let value = if scalar == "Long" {
                value.strip_suffix('L').unwrap_or(value)
            } else {
                value
            };
            if value.is_empty()
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'-')
            {
                return false;
            }
            let Ok(value) = value.parse::<i64>() else {
                return false;
            };
            match scalar {
                "Byte" => i8::try_from(value).is_ok(),
                "Short" => i16::try_from(value).is_ok(),
                "Int" => i32::try_from(value).is_ok(),
                _ => true,
            }
        }
        "Float" | "Double" => {
            let value = if scalar == "Float" {
                let Some(value) = value.strip_suffix(['f', 'F']) else {
                    return false;
                };
                value
            } else {
                value
            };
            let finite = if scalar == "Float" {
                value.parse::<f32>().is_ok_and(f32::is_finite)
            } else {
                value.parse::<f64>().is_ok_and(f64::is_finite)
            };
            value.contains('.')
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'-'))
                && finite
        }
        _ => false,
    }
}

struct File {
    source: parser::SourceBuf,
    tree: Tree,
}

struct Inventory(BTreeMap<String, File>);

fn has_build_definition(dir: &Path) -> Option<bool> {
    for name in BUILD_FILE_NAMES {
        match std::fs::symlink_metadata(dir.join(name)) {
            Ok(_) => return Some(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(false)
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
        // ignore された build 定義も依存を変えるので、ソースの選別とは別に確かめる。
        if has_build_definition(&root)? {
            return None;
        }
        for (count, entry) in ignore::WalkBuilder::new(&root).build().enumerate() {
            let entry = entry.ok()?;
            if count >= MAX_NODES || entry.error().is_some() || linked(entry.path())? {
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
        let mut count = 0;
        let mut checked_dirs = HashSet::from([root.clone()]);
        for path in collection.files {
            let path = path.canonicalize().ok()?;
            let relative = normalize_workspace_separators(path.strip_prefix(&root).ok()?.to_str()?);
            for (depth, parent) in path
                .ancestors()
                .skip(1)
                .take_while(|parent| parent.starts_with(&root))
                .enumerate()
            {
                if depth > MAX_DEPTH {
                    return None;
                }
                if checked_dirs.insert(parent.to_owned()) && has_build_definition(parent)? {
                    return None;
                }
            }
            let path = camino::Utf8Path::from_path(&path)?;
            let source = parser::read_file(path).ok()?;
            bytes += source.len();
            if source.len() > MAX_SOURCE || bytes > MAX_BYTES {
                return None;
            }
            let (tree, lang) = parser::parse_file(path, &source).ok()?;
            if lang != LangId::Kotlin {
                return None;
            }
            count += nodes(tree.root_node())?.len();
            if count > MAX_NODES {
                return None;
            }
            files.insert(relative, File { source, tree });
        }
        Some(Self(files))
    }

    fn proves(
        &self,
        site: &CompatibleModSite<'_>,
        source: &[u8],
        contract: &Contract,
        index: &ApiRefIndex,
    ) -> Option<()> {
        let target = self.0.get(site.new_path)?;
        if target.source.as_ref() != source {
            return None;
        }
        let target_class = class(target.tree.root_node(), source, site.name)?;
        let package = package(target.tree.root_node(), source)?;
        let import = if package.is_empty() {
            site.name.to_owned()
        } else {
            format!("{package}.{}", site.name)
        };
        let mut positions = HashSet::new();
        for (path, file) in &self.0 {
            let local_package = package_name(file)?;
            let mut imported = false;
            for current in nodes(file.tree.root_node())? {
                if current.node.kind() == "import_header" {
                    if text(current.node, &file.source)?
                        .strip_prefix("import")?
                        .trim()
                        .trim_end_matches(';')
                        != import
                    {
                        return None;
                    }
                    imported = true;
                }
            }
            for current in nodes(file.tree.root_node())? {
                let node = current.node;
                let kind = node.kind();
                // 別名・拡張・注釈・参照の arity は、この限定した証明では追わない。
                if matches!(
                    kind,
                    "type_alias"
                        | "annotation"
                        | "file_annotation"
                        | "::"
                        | "multi_variable_declaration"
                ) || extension(node)
                {
                    return None;
                }
                if !matches!(kind, "simple_identifier" | "type_identifier") {
                    continue;
                }
                let name = text(node, &file.source)?;
                if name == "copy"
                    || name.strip_prefix("component").is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                    })
                {
                    return None;
                }
                if contract.scalar_names.contains(name) && !scalar_reference(&current, &file.source)
                {
                    return None;
                }
                let in_target = path == site.new_path
                    && node.start_byte() >= target_class.start_byte()
                    && node.end_byte() <= target_class.end_byte();
                if contract.added.contains(name) && !in_target {
                    return None;
                }
                if name != site.name {
                    continue;
                }
                let point = node.start_position();
                positions.insert((path.clone(), point.row, point.column));
                let parent = current.parent?;
                if in_target && parent.id() == target_class.id() && kind == "type_identifier" {
                    continue;
                }
                if local_package != package && !imported {
                    return None;
                }
                if kind == "type_identifier"
                    && parent.kind() == "user_type"
                    && text(parent, &file.source)? == site.name
                {
                    continue;
                }
                if current
                    .grandparent
                    .is_some_and(|parent| parent.kind() == "import_header")
                {
                    continue;
                }
                if kind != "simple_identifier"
                    || !constructor_call(&current, &file.source, contract)
                {
                    return None;
                }
            }
        }
        if index.complete_refs_for(site.name)?.iter().any(|reference| {
            !positions.contains(&(
                normalize_workspace_separators(&reference.path),
                reference.line,
                reference.column,
            ))
        }) {
            return None;
        }
        Some(())
    }
}

fn package(root: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    let mut package = None;
    for child in root
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "package_header")
    {
        if package.is_some() {
            return None;
        }
        let value = text(child, source)?
            .strip_prefix("package")?
            .trim()
            .trim_end_matches(';');
        if !value.split('.').all(identifier) {
            return None;
        }
        package = Some(value.to_owned());
    }
    Some(package.unwrap_or_default())
}

fn package_name(file: &File) -> Option<String> {
    package(file.tree.root_node(), &file.source)
}

fn extension(node: Node<'_>) -> bool {
    let stop = match node.kind() {
        "function_declaration" => "simple_identifier",
        "property_declaration" => "variable_declaration",
        _ => return false,
    };
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == stop {
            return false;
        }
        if child.kind() != "modifiers" {
            return true;
        }
    }
    true
}

fn scalar_reference(site: &Site<'_>, source: &[u8]) -> bool {
    site.parent.is_some_and(|parent| {
        parent.kind() == "user_type" && scalar(text(parent, source).unwrap_or("")).is_some()
    })
}

fn constructor_call(site: &Site<'_>, source: &[u8], contract: &Contract) -> bool {
    let Some(call) = site.parent else {
        return false;
    };
    if call.kind() != "call_expression"
        || call
            .named_child(0)
            .is_none_or(|callee| callee.id() != site.node.id())
        || site
            .grandparent
            .is_some_and(|parent| parent.kind() == "call_expression")
    {
        return false;
    }
    let mut cursor = call.walk();
    let suffixes: Vec<_> = call.named_children(&mut cursor).skip(1).collect();
    let [suffix] = suffixes.as_slice() else {
        return false;
    };
    if suffix.kind() != "call_suffix"
        || suffix.named_child_count() != 1
        || !text(*suffix, source).is_some_and(|text| text.starts_with('(') && text.ends_with(')'))
    {
        return false;
    }
    let Some(arguments) = suffix
        .named_child(0)
        .filter(|arguments| arguments.kind() == "value_arguments")
    else {
        return false;
    };
    arguments_fit(arguments, source, &contract.old_parameters).is_some()
}

fn arguments_fit(arguments: Node<'_>, source: &[u8], parameters: &[(String, bool)]) -> Option<()> {
    let mut supplied = vec![false; parameters.len()];
    let mut positional = 0;
    let mut named = false;
    let mut cursor = arguments.walk();
    for argument in arguments.named_children(&mut cursor) {
        if matches!(argument.kind(), "line_comment" | "multiline_comment") {
            continue;
        }
        if argument.kind() != "value_argument" {
            return None;
        }
        if text(argument, source)?.trim_start().starts_with('*') {
            return None;
        }
        let mut cursor = argument.walk();
        let parts: Vec<_> = argument.children(&mut cursor).collect();
        let index = match parts.as_slice() {
            [value] if value.kind() != "lambda_literal" && !named => {
                let index = positional;
                positional += 1;
                index
            }
            [name, equals, _] if name.kind() == "simple_identifier" && equals.kind() == "=" => {
                named = true;
                parameters
                    .iter()
                    .position(|(parameter, _)| text(*name, source) == Some(parameter))?
            }
            _ => return None,
        };
        if *supplied.get(index)? {
            return None;
        }
        supplied[index] = true;
    }
    parameters
        .iter()
        .zip(supplied)
        .all(|((_, optional), supplied)| *optional || supplied)
        .then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_nodes_exist_in_kotlin_grammar() {
        let language = LangId::Kotlin.ts_language();
        for kind in [
            "class_declaration",
            "modifiers",
            "primary_constructor",
            "class_parameter",
            "type_identifier",
            "simple_identifier",
            "user_type",
            "nullable_type",
            "string_literal",
            "line_comment",
            "multiline_comment",
            "package_header",
            "import_header",
            "type_alias",
            "annotation",
            "file_annotation",
            "multi_variable_declaration",
            "function_declaration",
            "property_declaration",
            "variable_declaration",
            "call_expression",
            "call_suffix",
            "value_arguments",
            "value_argument",
            "lambda_literal",
        ] {
            assert!(
                tree_sitter::Query::new(&language, &format!("({kind}) @probe")).is_ok(),
                "{kind}"
            );
        }
        for token in ["::", "val", "var", ":", "="] {
            assert!(
                tree_sitter::Query::new(&language, &format!("\"{token}\" @probe")).is_ok(),
                "{token}"
            );
        }
    }

    #[test]
    fn appended_defaults_require_valid_scalar_literals_and_names() {
        let old_source = b"data class Gauge(val count: Int)";
        let old_tree = parser::parse_source(old_source, LangId::Kotlin).unwrap();
        let old = class(old_tree.root_node(), old_source, "Gauge").unwrap();
        for (tail, compatible) in [
            ("val extra: Int = -3", true),
            ("var extra: kotlin.Long = 3L", true),
            ("val extra: Boolean = true", true),
            ("val extra: String = \"a b\"", true),
            ("val extra: Char = 'x'", true),
            ("val extra: Double? = null", true),
            ("val extra: Float = 1.0f", true),
            ("val extra: Byte = 128", false),
            ("val extra: Int = 2147483648", false),
            (
                "val extra: Float = 9999999999999999999999999999999999999999.0f",
                false,
            ),
            ("val extra: Char = '😀'", false),
            ("val extra: String = \"$count\"", false),
            ("val _: Int = 0", false),
            ("val __: Int = 0", false),
            ("val count: Int = 0", false),
        ] {
            let source = format!("data class Gauge(val count: Int, {tail})");
            let tree = parser::parse_source(source.as_bytes(), LangId::Kotlin).unwrap();
            let actual = class(tree.root_node(), source.as_bytes(), "Gauge")
                .and_then(|new| addition(old, old_source, new, source.as_bytes()))
                .is_some();
            assert_eq!(actual, compatible, "{source}");
        }
    }
}
