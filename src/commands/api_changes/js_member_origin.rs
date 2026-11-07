//! A narrow, position-specific proof for calls on a directly imported class.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use tree_sitter::Node;

use crate::engine::parser;
use crate::language::LangId;

use super::removed_attribution::relative_import_candidates;

#[derive(Clone)]
pub(crate) struct JsMemberEvidence {
    pub class_files: Rc<HashSet<String>>,
    pub dependency_targets: Rc<HashSet<String>>,
}

pub(super) struct DependencyScan {
    pub targets: HashSet<String>,
    pub opaque: bool,
}

struct ImportedClass {
    local_name: String,
    exported_name: String,
    files: HashSet<String>,
    binding_start: usize,
}

pub(super) fn collect_member_evidence(
    dir: &str,
    ref_path: &str,
    source: &[u8],
    root: Node<'_>,
    member_name: &str,
    ref_dependencies: &HashSet<String>,
) -> HashMap<(usize, usize), Rc<JsMemberEvidence>> {
    if root.has_error() || !is_normalized_repo_path(ref_path) {
        return HashMap::new();
    }
    let mut imports = Vec::new();
    let mut cursor = root.walk();
    for stmt in root.named_children(&mut cursor) {
        if stmt.kind() != "import_statement" {
            continue;
        }
        if has_token(stmt, "type") {
            continue;
        }
        let Some(spec) = stmt
            .child_by_field_name("source")
            .and_then(|n| super::static_js_string_text(n, source))
        else {
            continue;
        };
        let Some(files) = relative_import_candidates(ref_path, spec) else {
            continue;
        };
        // ディレクトリの package.json が解決先を変えると、候補で網羅できない。
        if super::removed_attribution::relative_directory_has_manifest(dir, ref_path, spec) {
            continue;
        }
        let Some(clause) = named_child(stmt, "import_clause") else {
            continue;
        };
        let Some(named) = named_child(clause, "named_imports") else {
            continue;
        };
        let mut entries = named.walk();
        for entry in named.named_children(&mut entries) {
            if entry.kind() != "import_specifier" {
                continue;
            }
            if has_token(entry, "type") {
                continue;
            }
            let Some(name) = entry.child_by_field_name("name") else {
                continue;
            };
            let Some(exported_name) = name.utf8_text(source).ok() else {
                continue;
            };
            let binding = entry.child_by_field_name("alias").unwrap_or(name);
            let Some(local_name) = binding.utf8_text(source).ok() else {
                continue;
            };
            imports.push(ImportedClass {
                local_name: local_name.to_string(),
                exported_name: exported_name.to_string(),
                files: files.clone(),
                binding_start: binding.start_byte(),
            });
        }
    }

    let mut result = HashMap::new();
    let mut nodes = vec![root];
    let mut members = Vec::new();
    while let Some(node) = nodes.pop() {
        if node.kind() == "member_expression"
            && let Some(property) = node.child_by_field_name("property")
            && property.utf8_text(source).ok() == Some(member_name)
            && let Some(object) = node.child_by_field_name("object")
            && object.kind() == "new_expression"
            && let Some(constructor) = object.child_by_field_name("constructor")
            && constructor.kind() == "identifier"
            && let Ok(class_name) = constructor.utf8_text(source)
        {
            members.push((property.start_position(), class_name.to_string()));
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    if members.is_empty() {
        return result;
    }

    let class_names: HashSet<&str> = members.iter().map(|(_, name)| name.as_str()).collect();
    let mut all_identifiers: HashMap<String, Vec<(usize, bool)>> = HashMap::new();
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if (node.kind().ends_with("identifier")
            || node.kind() == "shorthand_property_identifier_pattern")
            && let Ok(name) = node.utf8_text(source)
            && class_names.contains(name)
        {
            let constructor = node.parent().is_some_and(|parent| {
                parent.kind() == "new_expression"
                    && parent
                        .child_by_field_name("constructor")
                        .is_some_and(|n| n.id() == node.id())
            });
            all_identifiers
                .entry(name.to_string())
                .or_default()
                .push((node.start_byte(), constructor));
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }

    let mut verified_classes: HashMap<String, Option<Rc<JsMemberEvidence>>> = HashMap::new();
    for (position, class_name) in members {
        let evidence = verified_classes
            .entry(class_name.clone())
            .or_insert_with(|| {
                verify_class(
                    dir,
                    &class_name,
                    member_name,
                    &imports,
                    &all_identifiers,
                    ref_dependencies,
                )
            });
        if let Some(evidence) = evidence {
            result.insert((position.row, position.column), Rc::clone(evidence));
        }
    }
    result
}

pub(super) fn is_normalized_repo_path(path: &str) -> bool {
    path.is_ascii()
        && !path.contains('\\')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

pub(super) fn paths_match(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn verify_class(
    dir: &str,
    class_name: &str,
    member_name: &str,
    imports: &[ImportedClass],
    all_identifiers: &HashMap<String, Vec<(usize, bool)>>,
    ref_dependencies: &HashSet<String>,
) -> Option<Rc<JsMemberEvidence>> {
    let mut matched = imports.iter().filter(|i| i.local_name == class_name);
    let imported = matched.next()?;
    if matched.next().is_some() {
        return None;
    }
    // Any other use of this spelling may introduce a local binding or reassignment.
    // Accept only the import binding and direct constructor positions.
    if all_identifiers.get(class_name).is_none_or(|uses| {
        uses.iter()
            .any(|(start, constructor)| *start != imported.binding_start && !constructor)
    }) {
        return None;
    }
    let mut dependencies = ref_dependencies.clone();
    let mut found = false;
    for file in &imported.files {
        let path = std::path::Path::new(dir).join(file);
        match path.try_exists() {
            Ok(false) => continue,
            Ok(true) => {}
            Err(_) => return None,
        }
        let (module_source, module_tree) = load_module(&path)?;
        if !defines_exported_instance_method(
            module_tree.root_node(),
            &module_source,
            &imported.exported_name,
            member_name,
            super::ts_module_paths::is_declaration_path(file),
        ) {
            return None;
        }
        let scan = dependency_targets(dir, file, &module_source, module_tree.root_node());
        if scan.opaque {
            return None;
        }
        dependencies.extend(scan.targets);
        found = true;
    }
    found.then(|| {
        Rc::new(JsMemberEvidence {
            class_files: Rc::new(imported.files.clone()),
            dependency_targets: Rc::new(dependencies),
        })
    })
}

fn named_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|n| n.kind() == kind)
}

fn has_token(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

fn load_module(path: &std::path::Path) -> Option<(Vec<u8>, tree_sitter::Tree)> {
    let utf8 = camino::Utf8Path::from_path(path)?;
    let lang = LangId::from_path(utf8).ok()?;
    if !matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx) {
        return None;
    }
    let source = parser::read_file(utf8).ok()?;
    let tree = parser::parse_source(&source, lang).ok()?;
    (!tree.root_node().has_error()).then_some((source.to_vec(), tree))
}

fn defines_exported_instance_method(
    root: Node<'_>,
    source: &[u8],
    class_name: &str,
    member_name: &str,
    declaration_file: bool,
) -> bool {
    if contains_kind(root, "decorator") {
        return false;
    }
    let mut matches = 0usize;
    let mut valid = false;
    let mut cursor = root.walk();
    for stmt in root.named_children(&mut cursor) {
        if stmt.kind() != "export_statement" {
            continue;
        }
        let Some(mut class) = stmt.child_by_field_name("declaration") else {
            continue;
        };
        // 宣言ファイルでは `export declare class Name { method(): T; }` が普通の書き方。
        if declaration_file && class.kind() == "ambient_declaration" {
            let mut inner = class.walk();
            if let Some(declared) = class
                .named_children(&mut inner)
                .find(|child| child.kind() == "class_declaration")
            {
                class = declared;
            }
        }
        if class.kind() != "class_declaration"
            || class
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                != Some(class_name)
        {
            continue;
        }
        matches += 1;
        let Some(name) = class.child_by_field_name("name") else {
            continue;
        };
        if !class_name_is_unique(root, source, class_name, name.start_byte())
            || named_child(class, "class_heritage").is_some()
        {
            continue;
        }
        let Some(body) = class.child_by_field_name("body") else {
            continue;
        };
        let mut methods = body.walk();
        let members: Vec<_> = body.named_children(&mut methods).collect();
        if members.iter().any(|method| {
            method.kind() == "method_definition"
                && method
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    == Some("constructor")
                && contains_kind(*method, "return_statement")
        }) {
            continue;
        }
        // 宣言ファイルのメソッドは本体の無いシグネチャ (`method(): T;`)。
        valid = members.into_iter().any(|method| {
            let defined = (method.kind() == "method_definition"
                && method.child_by_field_name("body").is_some())
                || (declaration_file && method.kind() == "method_signature");
            defined
                && method
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    == Some(member_name)
                && !has_token(method, "static")
                && !has_token(method, "get")
                && !has_token(method, "set")
        });
    }
    matches == 1 && valid
}

fn contains_kind(root: Node<'_>, kind: &str) -> bool {
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if node.kind() == kind {
            return true;
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    false
}

fn class_name_is_unique(root: Node<'_>, source: &[u8], name: &str, definition: usize) -> bool {
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if (node.kind().ends_with("identifier")
            || node.kind() == "shorthand_property_identifier_pattern")
            && node.utf8_text(source).ok() == Some(name)
            && node.start_byte() != definition
        {
            return false;
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    true
}

pub(super) fn dependency_targets(
    dir: &str,
    path: &str,
    source: &[u8],
    root: Node<'_>,
) -> DependencyScan {
    let mut targets = HashSet::new();
    let mut opaque = false;
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if matches!(node.kind(), "string" | "template_string") {
            if node.kind() == "template_string" && contains_kind(node, "template_substitution") {
                opaque = true;
            } else if let Some(spec) = super::static_js_string_text(node, source) {
                if (spec.starts_with('.') || spec.starts_with('\\')) && spec.contains('\\') {
                    opaque = true;
                } else if let Some(paths) = relative_import_candidates(path, spec) {
                    // 宣言ファイル (`.d.ts` など) も候補に入る。ディレクトリの package.json が
                    // 解決先を変えると網羅できない。
                    if super::removed_attribution::relative_directory_has_manifest(dir, path, spec)
                    {
                        opaque = true;
                    }
                    targets.extend(paths);
                }
            }
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    DependencyScan { targets, opaque }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_proof_rejects_noncanonical_paths() {
        assert!(is_normalized_repo_path("src/main.ts"));
        for path in [
            "/src/main.ts",
            "./src/main.ts",
            "src//main.ts",
            "src/../main.ts",
        ] {
            assert!(!is_normalized_repo_path(path), "{path}");
        }
        assert!(paths_match("src/util.js", "src/Util.js"));
    }

    #[test]
    fn dependency_scan_reads_static_templates_and_marks_dynamic_ones_opaque() {
        let source = b"import(`../dist/util.js`); import(`../dist/${name}.js`);";
        let tree = parser::parse_source(source, LangId::Typescript).unwrap();
        let scan = dependency_targets(".", "src/main.ts", source, tree.root_node());
        assert!(scan.targets.contains("dist/util.js"));
        assert!(scan.opaque);
    }
}
