//! 固定された登録契約の範囲内で、実装が引数を受け取り始めたことの証拠。

use std::collections::BTreeMap;
use std::rc::Rc;
use tree_sitter::Node;

use super::*;
use crate::commands::api_changes::ref_index::{ParsedRefFile, parsed_ref_file_shared};
use crate::commands::api_changes::ts_const_arg::collect_bindings_named;
use crate::commands::dead_code_default_liveness::{resolve_relative_specifier, static_specifier};
use crate::engine::refs::{FileScanOptions, collect_files_scan};
use crate::git_support::normalize_workspace_separators;

type Inventory = BTreeMap<String, Rc<ParsedRefFile>>;

#[derive(Default)]
pub(crate) struct FixedCallbackCache {
    inventory: Option<Option<Rc<Inventory>>>,
}

fn inventory(dir: &str, caches: &mut ApiClosureCaches) -> Option<Rc<Inventory>> {
    if caches.fixed_callback.inventory.is_none() {
        let loaded = load_inventory(dir, caches).map(Rc::new);
        caches.fixed_callback.inventory = Some(loaded);
    }
    caches.fixed_callback.inventory.as_ref()?.clone()
}

fn load_inventory(dir: &str, caches: &mut ApiClosureCaches) -> Option<Inventory> {
    let root = std::path::Path::new(dir).canonicalize().ok()?;
    let collection = collect_files_scan(
        &root,
        None,
        FileScanOptions {
            include_generated: true,
        },
    )
    .ok()?;
    if collection.files.len() > 512 || !collection.unanalyzable_truncations(&root).is_empty() {
        return None;
    }
    let mut files = BTreeMap::new();
    let (mut bytes, mut count) = (0usize, 0usize);
    for path in collection.files {
        if path.symlink_metadata().ok()?.file_type().is_symlink() {
            return None;
        }
        let path = path.canonicalize().ok()?;
        if path.metadata().ok()?.len() > (8 * 1024 * 1024 - bytes) as u64 {
            return None;
        }
        let relative =
            normalize_workspace_separators(&path.strip_prefix(&root).ok()?.to_string_lossy());
        let file = parsed_ref_file_shared(dir, &relative, caches)?;
        if !TS_JS_LANGS.contains(&file.lang_id) {
            return None;
        }
        bytes = bytes.checked_add(file.source.len())?;
        if bytes > 8 * 1024 * 1024 {
            return None;
        }
        let nodes = checked_nodes(file.tree.root_node())?;
        count = count.checked_add(nodes.len())?;
        if count > 100_000
            || nodes
                .iter()
                .any(|&node| uncertain_access(node, &file.source))
        {
            return None;
        }
        files.insert(relative, file);
    }
    Some(files)
}

fn uncertain_access(node: Node<'_>, source: &[u8]) -> bool {
    if node.kind() == "call_expression"
        && node
            .child_by_field_name("function")
            .is_some_and(|function| function.kind() == "import")
    {
        return true;
    }
    matches!(
        node.kind(),
        "subscript_expression" | "computed_property_name" | "ambient_declaration"
    ) || matches!(
        node.kind(),
        "identifier"
            | "type_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
    ) && text(node, source).is_none_or(|text| {
        text.contains('\\') || matches!(text, "eval" | "Function" | "Reflect" | "Proxy" | "require")
    })
}

pub(crate) fn detect_fixed_callback_contract_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    caches: &mut ApiClosureCaches,
) -> Option<CompatibleApiModification> {
    if site.kind != "method" || site.name.contains('.') || site.old_path != site.new_path {
        return None;
    }
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    let old = registration(old_tree.root_node(), &src.old, site.name)?;
    let new = registration(new_tree.root_node(), &src.new, site.name)?;
    let old_params = old.method.child_by_field_name("parameters")?;
    let new_params = new.method.child_by_field_name("parameters")?;
    let old_body = old.method.child_by_field_name("body")?;
    let new_body = new.method.child_by_field_name("body")?;
    let old_names = implementation_parameters(old_params, &src.old)?;
    let new_names = implementation_parameters(new_params, &src.new)?;
    if !old_names.is_empty() || new_names.is_empty() || new_names.len() > new.arity {
        return None;
    }
    // 契約・登録関数・公開経路を含め、実装の引数と本体以外は変更されていない。
    if src.old.get(..old_params.start_byte())? != src.new.get(..new_params.start_byte())?
        || src.old.get(old_params.end_byte()..old_body.start_byte())?
            != src.new.get(new_params.end_byte()..new_body.start_byte())?
        || src.old.get(old_body.end_byte()..)? != src.new.get(new_body.end_byte()..)?
    {
        return None;
    }
    let refs = index.refs_for(site.name)?;
    let files = inventory(site.dir, caches)?;
    if files.get(site.new_path)?.source.as_ref() != src.new.as_ref() {
        return None;
    }
    if prove_default_imports(&files, site.new_path, site.name, new.arity)? == 0 {
        return None;
    }
    for reference in refs {
        let path = normalize_workspace_separators(&reference.path);
        let file = files.get(&path)?;
        let point = tree_sitter::Point::new(reference.line, reference.column);
        let node = file
            .tree
            .root_node()
            .descendant_for_point_range(point, point)?;
        if node.utf8_text(&file.source).ok()? != site.name {
            return None;
        }
        let same_file = path == site.new_path;
        if same_file
            && [new.method.child_by_field_name("name")?, new.contract_name]
                .iter()
                .any(|name| name.start_position() == point)
        {
            continue;
        }
        if member_call_arity(node)? != new.arity {
            return None;
        }
    }
    Some(site.compatible("fixed_callback_contract"))
}

fn prove_default_imports(
    files: &Inventory,
    target: &str,
    name: &str,
    arity: usize,
) -> Option<usize> {
    let mut calls = 0;
    let paths = files
        .keys()
        .map(|path| (path.clone(), Vec::new()))
        .collect();
    for (path, file) in files {
        let root = file.tree.root_node();
        let source = &file.source;
        let nodes = checked_nodes(root)?;
        // 文字列からメンバーを取得する経路は refs に現れない。
        if nodes.iter().any(|&node| {
            matches!(node.kind(), "string" | "template_string")
                && text(node, source)
                    .is_none_or(|value| value.contains(name) || value.contains('\\'))
        }) {
            return None;
        }
        for &statement in &nodes {
            if !matches!(statement.kind(), "import_statement" | "export_statement") {
                continue;
            }
            let Some(specifier) = statement.child_by_field_name("source") else {
                continue;
            };
            let specifier = static_specifier(specifier, source)?;
            // 設定に依存する alias や外部モジュールの再 export は証明しない。
            if !specifier.starts_with('.') {
                return None;
            }
            let candidates = resolve_relative_specifier(path, specifier, &paths);
            if candidates.len() != 1 {
                return None;
            }
            if !candidates.iter().any(|candidate| candidate == target) {
                continue;
            }
            if statement.kind() != "import_statement" {
                return None;
            }
            let parts = children(statement);
            let [clause, _] = parts.as_slice() else {
                return None;
            };
            let bindings = children(*clause);
            let [binding] = bindings.as_slice() else {
                return None;
            };
            if clause.kind() != "import_clause" || binding.kind() != "identifier" {
                return None;
            }
            let local = text(*binding, source)?;
            if collect_bindings_named(root, source, local).len() != 1 {
                return None;
            }
            for &node in &nodes {
                if matches!(node.kind(), "identifier" | "shorthand_property_identifier")
                    && text(node, source) == Some(local)
                    && node.id() != binding.id()
                {
                    let member = node.parent()?;
                    let property = member.child_by_field_name("property")?;
                    if member.kind() != "member_expression"
                        || member.child_by_field_name("object")?.id() != node.id()
                        || text(property, source)? != name
                        || member_call_arity(property)? != arity
                    {
                        return None;
                    }
                    calls += 1;
                }
            }
        }
    }
    Some(calls)
}

struct Registration<'tree> {
    method: Node<'tree>,
    contract_name: Node<'tree>,
    arity: usize,
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect()
}

fn checked_nodes(root: Node<'_>) -> Option<Vec<Node<'_>>> {
    if root.has_error() {
        return None;
    }
    let mut nodes = Vec::new();
    let mut stack = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        if nodes.len() >= 100_000 || depth > 128 || node.is_missing() || node.is_error() {
            return None;
        }
        nodes.push(node);
        stack.extend(children(node).into_iter().map(|child| (child, depth + 1)));
    }
    Some(nodes)
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

fn simple_type_name<'a>(annotation: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let parts = children(annotation);
    let [name] = parts.as_slice() else {
        return None;
    };
    (annotation.kind() == "type_annotation" && name.kind() == "type_identifier")
        .then(|| text(*name, source))?
}

fn implementation_parameters<'a>(params: Node<'_>, source: &'a [u8]) -> Option<Vec<&'a str>> {
    let mut names = Vec::new();
    for parameter in children(params) {
        let parts = children(parameter);
        let [pattern] = parts.as_slice() else {
            return None;
        };
        if parameter.kind() != "required_parameter" || pattern.kind() != "identifier" {
            return None;
        }
        let name = text(*pattern, source)?;
        if name == "this" || name.contains('\\') || names.contains(&name) {
            return None;
        }
        names.push(name);
    }
    Some(names)
}

fn registration<'tree>(
    root: Node<'tree>,
    source: &[u8],
    name: &str,
) -> Option<Registration<'tree>> {
    let nodes = checked_nodes(root)?;
    let methods: Vec<_> = nodes
        .iter()
        .copied()
        .filter(|node| {
            node.kind() == "method_definition"
                && node
                    .child_by_field_name("name")
                    .and_then(|name| text(name, source))
                    == Some(name)
        })
        .collect();
    let [method] = methods.as_slice() else {
        return None;
    };
    let method = *method;
    let method_name = method.child_by_field_name("name")?;
    let parts = children(method);
    if method_name.kind() != "property_identifier"
        || parts.len() != 3
        || parts[0].id() != method_name.id()
        || parts[1].kind() != "formal_parameters"
        || parts[2].kind() != "statement_block"
        || method.start_byte() != method_name.start_byte()
    {
        return None;
    }
    let object = method.parent()?;
    if object.kind() != "object" || children(object) != [method] {
        return None;
    }
    let args = object.parent()?;
    let call = args.parent()?;
    if args.kind() != "arguments"
        || children(args) != [object]
        || call.kind() != "call_expression"
        || call.child_by_field_name("type_arguments").is_some()
    {
        return None;
    }
    let mut cursor = call.walk();
    if call
        .children(&mut cursor)
        .any(|child| matches!(child.kind(), "?." | "optional_chain"))
    {
        return None;
    }
    let callee = call.child_by_field_name("function")?;
    let export = call.parent()?;
    let mut cursor = export.walk();
    if callee.kind() != "identifier"
        || export.kind() != "export_statement"
        || export.parent()?.kind() != "program"
        || export.child_by_field_name("value")?.id() != call.id()
        || !export
            .children(&mut cursor)
            .any(|child| child.kind() == "default")
    {
        return None;
    }
    let bindings = collect_bindings_named(root, source, text(callee, source)?);
    let [function] = bindings.as_slice() else {
        return None;
    };
    let function = *function;
    let function_name = function.child_by_field_name("name")?;
    if function.kind() != "function_declaration"
        || function.parent()?.kind() != "program"
        || function.child_by_field_name("type_parameters").is_some()
        || source.get(function.start_byte()..function_name.start_byte())? != b"function "
    {
        return None;
    }
    let parameters = children(function.child_by_field_name("parameters")?);
    let [parameter] = parameters.as_slice() else {
        return None;
    };
    let pattern = parameter.child_by_field_name("pattern")?;
    if parameter.kind() != "required_parameter"
        || pattern.kind() != "identifier"
        || parameter.child_by_field_name("value").is_some()
        || children(*parameter).len() != 2
    {
        return None;
    }
    let type_name = simple_type_name(parameter.child_by_field_name("type")?, source)?;
    if simple_type_name(function.child_by_field_name("return_type")?, source)? != type_name {
        return None;
    }
    let statements = children(function.child_by_field_name("body")?);
    let [statement] = statements.as_slice() else {
        return None;
    };
    let values = children(*statement);
    let [value] = values.as_slice() else {
        return None;
    };
    if statement.kind() != "return_statement"
        || value.kind() != "identifier"
        || text(*value, source)? != text(pattern, source)?
    {
        return None;
    }
    let bindings = collect_bindings_named(root, source, type_name);
    let [interface] = bindings.as_slice() else {
        return None;
    };
    if interface.kind() != "interface_declaration"
        || interface.parent()?.kind() != "program"
        || children(*interface).len() != 2
    {
        return None;
    }
    let members = children(interface.child_by_field_name("body")?);
    let [contract] = members.as_slice() else {
        return None;
    };
    let contract_name = contract.child_by_field_name("name")?;
    if contract.kind() != "method_signature"
        || contract_name.kind() != "property_identifier"
        || text(contract_name, source)? != name
        || children(*contract).len() != 3
        || contract.start_byte() != contract_name.start_byte()
        || text(contract.child_by_field_name("return_type")?, source)?.trim() != ": void"
    {
        return None;
    }
    let mut cursor = contract.walk();
    if contract
        .children(&mut cursor)
        .any(|child| child.kind() == "?")
    {
        return None;
    }
    let params = children(contract.child_by_field_name("parameters")?);
    if params.is_empty() {
        return None;
    }
    for parameter in &params {
        if parameter.kind() != "required_parameter"
            || children(*parameter).len() != 2
            || parameter.child_by_field_name("pattern")?.kind() != "identifier"
            || parameter.child_by_field_name("value").is_some()
        {
            return None;
        }
        if text(parameter.child_by_field_name("pattern")?, source)? == "this" {
            return None;
        }
        parameter.child_by_field_name("type")?;
    }
    // 同名の別宣言や実装の抜き出しを、固定されたメンバー契約に混ぜない。
    if collect_bindings_named(root, source, name)
        .iter()
        .any(|node| node.id() != method.id() && node.id() != contract.id())
    {
        return None;
    }
    Some(Registration {
        method,
        contract_name,
        arity: params.len(),
    })
}

fn member_call_arity(node: Node<'_>) -> Option<usize> {
    let member = node.parent()?;
    let call = member.parent()?;
    if member.kind() != "member_expression"
        || member.child_by_field_name("property")?.id() != node.id()
        || call.kind() != "call_expression"
        || call.child_by_field_name("function")?.id() != member.id()
        || call.child_by_field_name("type_arguments").is_some()
    {
        return None;
    }
    for expression in [member, call] {
        let mut cursor = expression.walk();
        if expression
            .children(&mut cursor)
            .any(|child| child.kind() == "optional_chain" || child.kind() == "?.")
        {
            return None;
        }
    }
    let args = call.child_by_field_name("arguments")?;
    if args.kind() != "arguments" {
        return None;
    }
    let args = children(args);
    (!args.iter().any(|arg| arg.kind() == "spread_element")).then_some(args.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_callback_inventory_limits_reject_partial_proofs() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        for index in 0..512 {
            std::fs::write(directory.path().join(format!("module-{index}.ts")), "").unwrap();
        }
        assert_eq!(
            load_inventory(dir, &mut ApiClosureCaches::default())
                .unwrap()
                .len(),
            512
        );
        std::fs::write(directory.path().join("overflow.ts"), "").unwrap();
        assert!(load_inventory(dir, &mut ApiClosureCaches::default()).is_none());

        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        let path = directory.path().join("large.ts");
        std::fs::write(&path, vec![b' '; 8 * 1024 * 1024 + 1]).unwrap();
        assert!(load_inventory(dir, &mut ApiClosureCaches::default()).is_none());
        std::fs::write(path, ";".repeat(100_001)).unwrap();
        assert!(load_inventory(dir, &mut ApiClosureCaches::default()).is_none());
    }

    #[test]
    fn fixed_callback_contract_nodes_and_fields_exist() {
        for &lang in TS_ONLY_LANGS {
            for query in [
                "(method_definition name: (property_identifier) parameters: (formal_parameters) body: (statement_block))",
                "(method_signature name: (property_identifier) parameters: (formal_parameters) return_type: (type_annotation))",
                "(required_parameter pattern: (identifier) type: (type_annotation))",
                "(function_declaration name: (identifier) return_type: (type_annotation))",
                "(export_statement value: (call_expression function: (identifier) arguments: (arguments (object))))",
                "(interface_declaration name: (type_identifier) body: (interface_body))",
                "(subscript_expression)",
                "(computed_property_name)",
                "(ambient_declaration)",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), query)
                    .unwrap_or_else(|error| panic!("{lang:?}: {query}: {error}"));
            }
        }
    }
}
