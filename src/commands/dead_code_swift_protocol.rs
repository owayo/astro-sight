//! Swift の外部 protocol が呼ぶメソッドを dead 候補と区別する。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use crate::config::{SwiftProtocolMethod, SwiftProtocolMethods, compact_type};
use crate::engine::{bounded_read::read_input_file, parser};
use crate::git_support::normalize_workspace_separators;
use crate::language::LangId;

#[derive(Default)]
struct Environment {
    imports: HashSet<String>,
    bindings: HashSet<String>,
}

struct Owner {
    name: String,
    nominal: bool,
    protocols: Vec<String>,
}

struct Method {
    owner: String,
    exported_name: String,
    line: usize,
    name: String,
    signature: String,
    labels: Vec<String>,
    types: Vec<String>,
    returns: String,
    is_static: bool,
}

struct FileFacts {
    path: String,
    module: String,
    environment: Environment,
    owners: Vec<Owner>,
    methods: Vec<Method>,
    blocked: bool,
}

/// `(候補ファイル, API と同じ宣言ヘッダ)` の一意な witness。
#[derive(Default)]
pub(super) struct SwiftProtocolLiveness {
    witnesses: HashSet<(String, String, String)>,
    lines: HashMap<(String, String, String), Option<usize>>,
}

impl SwiftProtocolLiveness {
    pub(super) fn build(root: &Path, files: &[PathBuf], custom: &SwiftProtocolMethods) -> Self {
        let mut requirements = builtin_methods();
        requirements.extend(custom.iter().cloned());
        let mut facts = Vec::new();
        let mut manifests = HashMap::new();
        for path in files {
            if crate::engine::refs::detect_source_lang(path) != Some(LangId::Swift) {
                continue;
            }
            let Some(source) = read_input_file(path) else {
                return Self::default();
            };
            let Ok(tree) = parser::parse_source(&source, LangId::Swift) else {
                return Self::default();
            };
            let Ok(relative) = path.strip_prefix(root) else {
                return Self::default();
            };
            let path_text = normalize_workspace_separators(&relative.to_string_lossy());
            let module = module_key(root, path, &path_text, &mut manifests);
            facts.push(file_facts(path_text, module, tree.root_node(), &source));
        }
        let mut bindings: HashMap<&str, HashSet<String>> = HashMap::new();
        let mut nominal: HashMap<(&str, &str), usize> = HashMap::new();
        let mut blocked = HashSet::new();
        for file in &facts {
            bindings
                .entry(&file.module)
                .or_default()
                .extend(file.environment.bindings.iter().cloned());
            if file.blocked {
                blocked.insert(file.module.as_str());
            }
            for owner in &file.owners {
                if owner.nominal {
                    *nominal.entry((&file.module, &owner.name)).or_default() += 1;
                }
            }
        }
        let mut conformances: HashMap<(&str, &str), HashSet<&str>> = HashMap::new();
        for file in &facts {
            if blocked.contains(file.module.as_str()) {
                continue;
            }
            for owner in &file.owners {
                if nominal.get(&(file.module.as_str(), owner.name.as_str())) != Some(&1) {
                    continue;
                }
                for requirement in &requirements {
                    if owner.protocols.iter().any(|protocol| {
                        name_matches(
                            protocol,
                            &requirement.protocol,
                            &file.environment,
                            &bindings[&file.module.as_str()],
                        )
                    }) {
                        conformances
                            .entry((&file.module, &owner.name))
                            .or_default()
                            .insert(&requirement.protocol);
                    }
                }
            }
        }
        let mut witnesses = HashSet::new();
        let mut lines = HashMap::new();
        for file in &facts {
            if blocked.contains(file.module.as_str()) {
                continue;
            }
            for method in &file.methods {
                let key = (
                    file.path.clone(),
                    method.exported_name.clone(),
                    method.signature.clone(),
                );
                lines
                    .entry(key.clone())
                    .and_modify(|line| *line = None)
                    .or_insert(Some(method.line));
                let Some(protocols) =
                    conformances.get(&(file.module.as_str(), method.owner.as_str()))
                else {
                    continue;
                };
                if requirements.iter().any(|requirement| {
                    protocols.contains(requirement.protocol.as_str())
                        && method_matches(
                            method,
                            requirement,
                            &file.environment,
                            &bindings[&file.module.as_str()],
                        )
                }) {
                    witnesses.insert(key);
                }
            }
        }
        witnesses.retain(|key| lines.get(key).is_some_and(Option::is_some));
        Self { witnesses, lines }
    }

    pub(super) fn contains(&self, file: &str, name: &str, signature: &str) -> bool {
        self.witnesses
            .contains(&(file.to_owned(), name.to_owned(), signature.to_owned()))
    }

    pub(super) fn line(&self, file: &str, name: &str, signature: &str) -> Option<usize> {
        self.lines
            .get(&(file.to_owned(), name.to_owned(), signature.to_owned()))
            .copied()
            .flatten()
    }
}

fn builtin_methods() -> Vec<SwiftProtocolMethod> {
    [
        (
            "AppKit.NSApplicationDelegate",
            "applicationShouldTerminateAfterLastWindowClosed",
            vec!["_"],
            vec!["AppKit.NSApplication"],
            "Swift.Bool",
        ),
        (
            "AppKit.NSMenuDelegate",
            "menuNeedsUpdate",
            vec!["_"],
            vec!["AppKit.NSMenu"],
            "Swift.Void",
        ),
        (
            "Swift.Hashable",
            "hash",
            vec!["into"],
            vec!["inout Swift.Hasher"],
            "Swift.Void",
        ),
        (
            "SwiftUI.NSViewRepresentable",
            "makeNSView",
            vec!["context"],
            vec!["Context"],
            "AppKit.NSView",
        ),
        (
            "SwiftUI.NSViewRepresentable",
            "updateNSView",
            vec!["_", "context"],
            vec!["AppKit.NSView", "Context"],
            "Swift.Void",
        ),
    ]
    .into_iter()
    .map(
        |(protocol, method, labels, types, returns)| SwiftProtocolMethod {
            protocol: protocol.to_owned(),
            method: method.to_owned(),
            labels: labels.into_iter().map(str::to_owned).collect(),
            parameter_types: types.into_iter().map(str::to_owned).collect(),
            return_type: returns.to_owned(),
            is_static: false,
        },
    )
    .collect()
}

fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut cursor = root.walk();
    loop {
        result.push(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return result;
            }
        }
    }
}

fn file_facts(path: String, module: String, root: Node<'_>, source: &[u8]) -> FileFacts {
    let all = nodes(root);
    let mut facts = FileFacts {
        path,
        module,
        environment: Environment::default(),
        owners: Vec::new(),
        methods: Vec::new(),
        blocked: root.has_error()
            || all.iter().any(|node| {
                node.is_missing()
                    || matches!(
                        node.kind(),
                        "directive" | "macro_invocation" | "macro_declaration"
                    )
                    || (node.kind() == "attribute" && !known_attribute(*node, source))
            }),
    };
    for node in all {
        if node.kind() == "import_declaration" {
            let mut cursor = node.walk();
            for identifier in node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "identifier")
            {
                if let Ok(name) = identifier.utf8_text(source) {
                    facts
                        .environment
                        .imports
                        .insert(name.split('.').next().unwrap_or(name).to_owned());
                }
            }
        }
        if matches!(
            node.kind(),
            "class_declaration" | "protocol_declaration" | "typealias_declaration"
        ) && let Some(name) = node
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
        {
            facts.environment.bindings.insert(name.to_owned());
        }
        if node.kind() != "class_declaration" {
            continue;
        }
        let Some(name) = owner_name(node, source) else {
            continue;
        };
        let Some(kind) = node
            .child_by_field_name("declaration_kind")
            .and_then(|kind| kind.utf8_text(source).ok())
        else {
            continue;
        };
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        if children
            .iter()
            .any(|child| matches!(child.kind(), "type_parameters" | "type_constraints"))
            || !attributes_supported(node, source)
        {
            continue;
        }
        let protocols = children
            .iter()
            .filter(|child| child.kind() == "inheritance_specifier")
            .filter_map(|child| child.child_by_field_name("inherits_from"))
            .filter(|inherited| {
                inherited.kind() == "user_type"
                    && !nodes(*inherited)
                        .iter()
                        .any(|child| child.kind() == "type_arguments")
            })
            .filter_map(|inherited| inherited.utf8_text(source).ok().map(compact_type))
            .collect();
        facts.owners.push(Owner {
            name: name.clone(),
            nominal: kind != "extension",
            protocols,
        });
        let Some(body) = node.child_by_field_name("body") else {
            continue;
        };
        let mut cursor = body.walk();
        for method in body
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "function_declaration")
        {
            if let Some(method) = method_facts(method, &name, source) {
                facts.methods.push(method);
            }
        }
    }
    facts
}

fn owner_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    if !matches!(name.kind(), "type_identifier" | "user_type")
        || nodes(name)
            .iter()
            .any(|child| child.kind() == "type_arguments")
    {
        return None;
    }
    let mut name = compact_type(name.utf8_text(source).ok()?);
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        if ancestor.kind() == "class_declaration" {
            name = format!(
                "{}.{}",
                ancestor
                    .child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()?,
                name
            );
        }
        if matches!(ancestor.kind(), "function_declaration" | "lambda_literal") {
            return None;
        }
        parent = ancestor.parent();
    }
    Some(name)
}

fn attributes_supported(node: Node<'_>, source: &[u8]) -> bool {
    let body_start = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |body| body.start_byte());
    nodes(node)
        .into_iter()
        .filter(|child| child.start_byte() < body_start && child.kind() == "attribute")
        .all(|attribute| known_attribute(attribute, source))
}

fn known_attribute(attribute: Node<'_>, source: &[u8]) -> bool {
    attribute.utf8_text(source).ok().is_some_and(|text| {
        matches!(
            text,
            "@MainActor" | "@objc" | "@preconcurrency" | "@testable"
        ) || text.starts_with("@available(")
    })
}

fn method_facts(node: Node<'_>, owner: &str, source: &[u8]) -> Option<Method> {
    let name = node
        .child_by_field_name("name")?
        .utf8_text(source)
        .ok()?
        .to_owned();
    let body = node.child_by_field_name("body")?;
    let header_nodes: Vec<_> = nodes(node)
        .into_iter()
        .filter(|child| child.start_byte() < body.start_byte())
        .collect();
    if !attributes_supported(node, source)
        || header_nodes.iter().any(|child| {
            matches!(
                child.kind(),
                "type_parameters"
                    | "type_constraints"
                    | "throws"
                    | "throws_clause"
                    | "async"
                    | "..."
                    | "default_value"
            ) || (child.child_count() == 0
                && matches!(
                    child.utf8_text(source),
                    Ok("async" | "throws" | "rethrows" | "class" | "mutating")
                ))
        })
        || node.child_by_field_name("default_value").is_some()
    {
        return None;
    }
    let is_static = header_nodes
        .iter()
        .any(|child| child.child_count() == 0 && child.utf8_text(source).ok() == Some("static"));
    let mut cursor = node.walk();
    let parameters: Vec<_> = node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "parameter")
        .collect();
    let mut labels = Vec::new();
    let mut types = Vec::new();
    for parameter in parameters {
        let label = parameter
            .child_by_field_name("external_name")
            .or_else(|| parameter.child_by_field_name("name"))?;
        labels.push(label.utf8_text(source).ok()?.to_owned());
        let parameter_text = parameter.utf8_text(source).ok()?;
        let (_, value_type) = parameter_text.split_once(':')?;
        types.push(compact_type(value_type));
    }
    let returns = node
        .child_by_field_name("return_type")
        .and_then(|value| value.utf8_text(source).ok())
        .map(compact_type)
        .unwrap_or_else(|| "Swift.Void".to_owned());
    let signature = String::from_utf8_lossy(&source[node.start_byte()..body.start_byte()])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let container = node.parent()?.parent()?;
    let is_extension = container
        .child_by_field_name("declaration_kind")?
        .utf8_text(source)
        .ok()?
        == "extension";
    let exported_name = if is_extension {
        name.clone()
    } else {
        format!("{}.{}", owner.rsplit('.').next()?, name)
    };
    Some(Method {
        owner: owner.to_owned(),
        exported_name,
        line: node.start_position().row,
        name,
        signature,
        labels,
        types,
        returns,
        is_static,
    })
}

fn method_matches(
    method: &Method,
    requirement: &SwiftProtocolMethod,
    environment: &Environment,
    bindings: &HashSet<String>,
) -> bool {
    method.name == requirement.method
        && method.labels == requirement.labels
        && method.is_static == requirement.is_static
        && method.types.len() == requirement.parameter_types.len()
        && method
            .types
            .iter()
            .zip(&requirement.parameter_types)
            .all(|(actual, expected)| type_matches(actual, expected, environment, bindings))
        && type_matches(
            &method.returns,
            &requirement.return_type,
            environment,
            bindings,
        )
}

fn type_matches(
    actual: &str,
    expected: &str,
    environment: &Environment,
    bindings: &HashSet<String>,
) -> bool {
    if matches!(actual, "()" | "Void" | "Swift.Void")
        && matches!(expected, "()" | "Void" | "Swift.Void")
    {
        return (actual == "()" || name_matches(actual, "Swift.Void", environment, bindings))
            && (expected == "()" || name_matches(expected, "Swift.Void", environment, bindings));
    }
    let tokens = |value: &str| {
        let mut tokens = Vec::new();
        let mut start = 0;
        for (offset, character) in value.char_indices() {
            if !character.is_alphanumeric() && !matches!(character, '_' | '.' | '`') {
                if start < offset {
                    tokens.push(value[start..offset].to_owned());
                }
                tokens.push(character.to_string());
                start = offset + character.len_utf8();
            }
        }
        if start < value.len() {
            tokens.push(value[start..].to_owned());
        }
        tokens
    };
    let actual = tokens(actual);
    let expected = tokens(expected);
    actual.len() == expected.len()
        && actual.iter().zip(&expected).all(|(actual, expected)| {
            if expected
                .chars()
                .next()
                .is_some_and(|character| character.is_alphabetic() || character == '_')
            {
                matches!(expected.as_str(), "inout" | "some" | "any" | "Self") && actual == expected
                    || name_matches(actual, expected, environment, bindings)
            } else {
                actual == expected
            }
        })
}

fn name_matches(
    actual: &str,
    expected: &str,
    environment: &Environment,
    bindings: &HashSet<String>,
) -> bool {
    let Some((module, tail)) = expected.split_once('.') else {
        return actual == expected
            && !bindings.contains(expected)
            && (environment.imports.len() <= 1
                || (expected == "Context"
                    && environment.imports.iter().all(|import| {
                        matches!(
                            import.as_str(),
                            "Swift" | "Foundation" | "AppKit" | "SwiftUI" | "Testing"
                        )
                    })));
    };
    if bindings.contains(module) || (module != "Swift" && !environment.imports.contains(module)) {
        return false;
    }
    if actual == expected {
        return true;
    }
    // The built-in table also fixes the owners of these SDK names. For custom
    // names, another import can export the same short name, so require spelling
    // out its module in that case.
    let known_sdk_name = matches!(
        expected,
        "Swift.Hashable"
            | "Swift.Hasher"
            | "Swift.Bool"
            | "Swift.Void"
            | "Swift.String"
            | "Swift.Int"
            | "AppKit.NSApplicationDelegate"
            | "AppKit.NSApplication"
            | "AppKit.NSMenuDelegate"
            | "AppKit.NSMenu"
            | "AppKit.NSView"
            | "SwiftUI.NSViewRepresentable"
    );
    actual == tail
        && !bindings.contains(tail)
        && environment.imports.iter().all(|import| {
            import == module
                || (known_sdk_name
                    && matches!(
                        import.as_str(),
                        "Swift" | "Foundation" | "AppKit" | "SwiftUI" | "Testing"
                    ))
        })
}

fn module_key(
    root: &Path,
    path: &Path,
    fallback: &str,
    manifests: &mut HashMap<PathBuf, Option<HashSet<String>>>,
) -> String {
    let mut parent = path.parent();
    while let Some(directory) = parent {
        if !directory.starts_with(root) {
            break;
        }
        let manifest = directory.join("Package.swift");
        if manifest.is_file()
            && let Ok(relative) = path.strip_prefix(directory)
        {
            let components: Vec<_> = relative.components().collect();
            let targets = manifests
                .entry(manifest.clone())
                .or_insert_with(|| manifest_targets(&manifest));
            if components.len() >= 3
                && matches!(
                    components[0].as_os_str().to_str(),
                    Some("Sources" | "Tests")
                )
                && targets.as_ref().is_some_and(|targets| {
                    targets.contains(&format!(
                        "{}/{}",
                        components[0].as_os_str().to_string_lossy(),
                        components[1].as_os_str().to_string_lossy()
                    ))
                })
            {
                return format!(
                    "{}:{}",
                    directory.display(),
                    components[1].as_os_str().to_string_lossy()
                );
            }
            break;
        }
        parent = directory.parent();
    }
    format!("file:{fallback}")
}

/// Only the usual literal SPM target array establishes a cross-file module.
/// A dynamic manifest or a custom source layout keeps file-local evidence.
fn manifest_targets(path: &Path) -> Option<HashSet<String>> {
    let source = read_input_file(path)?;
    let tree = parser::parse_source(&source, LangId::Swift).ok()?;
    let root = tree.root_node();
    let all = nodes(root);
    if root.has_error()
        || all.iter().any(|node| {
            node.is_missing()
                || node.kind() == "directive"
                || (node.kind() == "simple_identifier"
                    && matches!(
                        node.utf8_text(&source),
                        Ok("path" | "sources" | "exclude" | "unsafeFlags")
                    ))
        })
    {
        return None;
    }
    let mut cursor = root.walk();
    let mut package = None;
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "comment" | "multiline_comment" => {}
            "import_declaration"
                if child.utf8_text(&source).ok() == Some("import PackageDescription") => {}
            "property_declaration" if package.is_none() => {
                if child.child_by_field_name("name")?.utf8_text(&source).ok()? != "package" {
                    return None;
                }
                package = child.child_by_field_name("value");
            }
            _ => return None,
        }
    }
    let package = package?;
    if package.kind() != "call_expression"
        || package.named_child(0)?.utf8_text(&source).ok()? != "Package"
    {
        return None;
    }
    let arguments = call_arguments(package)?;
    let array = arguments
        .into_iter()
        .find(|argument| {
            argument
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(&source).ok())
                == Some("targets")
        })?
        .child_by_field_name("value")?;
    if array.kind() != "array_literal" {
        return None;
    }
    let mut targets = HashSet::new();
    let mut cursor = array.walk();
    for target in array.named_children(&mut cursor) {
        let factory = target.named_child(0)?.utf8_text(&source).ok()?;
        if target.kind() != "call_expression"
            || !matches!(factory, ".target" | ".executableTarget" | ".testTarget")
        {
            return None;
        }
        let arguments = call_arguments(target)?;
        let value = arguments
            .into_iter()
            .find(|argument| {
                argument
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(&source).ok())
                    == Some("name")
            })?
            .child_by_field_name("value")?;
        if value.kind() != "line_string_literal"
            || value.named_child_count() != 1
            || value.named_child(0)?.kind() != "line_str_text"
        {
            return None;
        }
        let name = value.named_child(0)?.utf8_text(&source).ok()?;
        let prefix = if factory == ".testTarget" {
            "Tests"
        } else {
            "Sources"
        };
        if name.is_empty()
            || !name.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
            || !targets.insert(format!("{prefix}/{name}"))
        {
            return None;
        }
    }
    Some(targets)
}

fn call_arguments(node: Node<'_>) -> Option<Vec<Node<'_>>> {
    let suffix = node.named_child(1)?;
    let arguments = suffix.named_child(0)?;
    if suffix.kind() != "call_suffix" || arguments.kind() != "value_arguments" {
        return None;
    }
    let mut cursor = arguments.walk();
    Some(arguments.named_children(&mut cursor).collect())
}
