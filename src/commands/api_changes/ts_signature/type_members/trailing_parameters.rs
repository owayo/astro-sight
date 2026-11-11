//! 型引数の末尾への既定値付き追加を、既存メンバーだけを読む利用の範囲で証明する。

use super::super::removed_params::{
    InventoryView, OptionalRemovalCache, checked_nodes, dependency_target, dynamic_execution_entry,
    escaped_identifier, imports_only_unrelated_names,
};
use super::super::repository_audit::PackageBoundary;
use super::*;
use crate::commands::api_changes::ts_const_arg::collect_bindings_named;
use crate::commands::api_changes::ts_required_members::{
    TypeAtom, body_rebinds_or_queries, simple_type,
};
use crate::commands::dead_code_default_liveness::static_specifier;
use crate::engine::js_scope::{Namespace, resolve_binding_scope};
use crate::git_support::normalize_workspace_separators;

struct Delta {
    old_arity: usize,
    required: usize,
    retained: HashSet<String>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Consumer {
    path: String,
    name: String,
    line: usize,
    column: usize,
}

pub(crate) fn detect_trailing_default_type_params_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut OptionalRemovalCache,
    diff_files: &[crate::models::impact::DiffFile],
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    if !matches!(site.kind, "type" | "trait" | "interface") || site.old_path != site.new_path {
        return None;
    }
    let src = sources.get(site)?;
    // 公開用の投影と在庫を同じ宣言で比較できない場合は証明しない。
    if src.original_old() != src.old.as_slice() || src.original_new() != src.new.as_ref() {
        return None;
    }
    let (old, new) = src.parse_pair(lang)?;
    let delta = classify(
        old.root_node(),
        &src.old,
        new.root_node(),
        &src.new,
        site.name,
    )?;
    let expected = src.original_new().to_vec();
    if !super::super::compatible_dependencies_unchanged(
        site,
        sources,
        cache,
        diff_files,
        "trailing_default_type_params",
        None,
    ) {
        return None;
    }
    let refs = index.complete_refs_for(site.name)?;
    let (inventory, evidence) = cache.inventory(site.dir, site.name, site.new_path)?;
    if !inventory.has_source(site.new_path, &expected)
        || !evidence.repository.as_ref()?.jsx_standard()
    {
        return None;
    }
    let consumers = prove_uses(refs, &inventory, evidence.relaxation(), site, &delta)?;
    for consumer in consumers {
        let (inventory, evidence) = cache.inventory(site.dir, &consumer.name, &consumer.path)?;
        prove_consumer_unused(&consumer, &inventory, evidence.relaxation())?;
    }
    Some(site.compatible("trailing_default_type_params"))
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect()
}

fn parameters(decl: Node<'_>) -> Option<Vec<Node<'_>>> {
    let Some(parameters) = decl.child_by_field_name("type_parameters") else {
        return Some(Vec::new());
    };
    let nodes = children(parameters);
    nodes
        .iter()
        .all(|node| node.kind() == "type_parameter")
        .then_some(nodes)
}

fn text<'source>(node: Node<'_>, source: &'source [u8]) -> Option<&'source str> {
    let text = node.utf8_text(source).ok()?;
    (!text.contains('\\')).then_some(text)
}

fn classify(
    old_root: Node<'_>,
    old_source: &[u8],
    new_root: Node<'_>,
    new_source: &[u8],
    name: &str,
) -> Option<Delta> {
    if old_root.has_error() || new_root.has_error() {
        return None;
    }
    let (old, old_outer) = unique_type_declaration(old_root, name, old_source)?;
    let (new, new_outer) = unique_type_declaration(new_root, name, new_source)?;
    if old.kind() != new.kind() || new_outer.kind() != "export_statement" {
        return None;
    }
    let old_name = old.child_by_field_name("name")?;
    // 再帰的なメンバーの型も変わる場合は、字面の一致だけで旧メンバーを証明しない。
    if checked_nodes(old)?.iter().any(|node| {
        node.id() != old_name.id()
            && node.kind() == "type_identifier"
            && text(*node, old_source) == Some(name)
    }) {
        return None;
    }
    let (old_body, new_body) = (type_member_body(old)?, type_member_body(new)?);
    let (old_params, new_params) = (parameters(old)?, parameters(new)?);
    if new_params.len() <= old_params.len() {
        return None;
    }
    for (old, new) in old_params.iter().zip(&new_params) {
        if normalized_node(*old, old_source)? != normalized_node(*new, new_source)? {
            return None;
        }
    }
    let old_header = old.child_by_field_name("type_parameters");
    let new_header = new.child_by_field_name("type_parameters")?;
    if normalized_range(
        old_outer,
        old_source,
        old_outer.start_byte(),
        old_header.map_or(old_body.start_byte(), |params| params.start_byte()),
    )? != normalized_range(
        new_outer,
        new_source,
        new_outer.start_byte(),
        new_header.start_byte(),
    )? || normalized_range(
        old_outer,
        old_source,
        old_header.map_or(old_body.start_byte(), |params| params.end_byte()),
        old_body.start_byte(),
    )? != normalized_range(
        new_outer,
        new_source,
        new_header.end_byte(),
        new_body.start_byte(),
    )? || normalized_suffix(old_outer, old_body, old_source)?
        != normalized_suffix(new_outer, new_body, new_source)?
        || children(new)
            .iter()
            .any(|node| node.kind() == "extends_type_clause")
    {
        return None;
    }
    let mut names = HashSet::new();
    for parameter in &new_params {
        if !names.insert(text(parameter.child_by_field_name("name")?, new_source)?.to_owned()) {
            return None;
        }
    }
    let mut defaults = HashMap::new();
    for parameter in &new_params[old_params.len()..] {
        let name = text(parameter.child_by_field_name("name")?, new_source)?.to_owned();
        let parts = children(*parameter);
        if parts.len() != 2 || parameter.child_by_field_name("constraint").is_some() {
            return None;
        }
        let mut cursor = parameter.walk();
        if parameter
            .children(&mut cursor)
            .any(|child| matches!(child.kind(), "in" | "out" | "const"))
        {
            return None;
        }
        let default = parameter.child_by_field_name("value")?;
        let values = children(default);
        let [value] = values.as_slice() else {
            return None;
        };
        let ty = simple_type(*value, new_source)?;
        // 追加した名前が従来の型・制約を隠す場合は、字面が同じでも契約が変わる。
        if checked_nodes(old)?.iter().any(|node| {
            node.kind() == "type_identifier" && text(*node, old_source) == Some(name.as_str())
        }) {
            return None;
        }
        defaults.insert(name, ty);
    }
    let (old_members, new_members) = (
        normalized_members(old_body, old_source)?,
        normalized_members(new_body, new_source)?,
    );
    if !optional_superset(
        old_members.clone(),
        new_members.clone(),
        old_source,
        new_source,
    ) {
        return None;
    }
    let mut retained = HashSet::new();
    for (_, member) in &old_members {
        let name = member_name(*member, old_source).flatten()?;
        if member.kind() != "property_signature" || !retained.insert(name) {
            return None;
        }
    }
    for (signature, member) in &new_members {
        if old_members.iter().any(|(old, _)| old == signature) {
            continue;
        }
        if member.kind() != "property_signature" || !ts_property_signature_is_optional(*member) {
            return None;
        }
        let annotation = member.child_by_field_name("type")?;
        let annotation_types = children(annotation);
        let [ty] = annotation_types.as_slice() else {
            return None;
        };
        let ty = if ty.kind() == "type_identifier" {
            defaults.get(text(*ty, new_source)?)?.clone()
        } else {
            simple_type(*ty, new_source)?
        };
        // 外部から渡す既存の構造的な値にも、新しい optional property の制約を増やさない。
        if !ty
            .iter()
            .any(|atom| matches!(atom, TypeAtom::Unknown | TypeAtom::Any))
        {
            return None;
        }
    }
    let required = old_params
        .iter()
        .filter(|parameter| parameter.child_by_field_name("value").is_none())
        .count();
    Some(Delta {
        old_arity: old_params.len(),
        required,
        retained,
    })
}

fn prove_uses(
    refs: &[crate::models::reference::SymbolReference],
    inventory: &InventoryView<'_>,
    packages: Option<&PackageBoundary>,
    site: &CompatibleModSite<'_>,
    delta: &Delta,
) -> Option<Vec<Consumer>> {
    let paths = inventory
        .parsed()
        .map(|(path, _, _, _)| (path.to_owned(), Vec::new()))
        .collect();
    let mut positions = HashSet::new();
    let mut uses = 0;
    let mut consumers = Vec::new();
    for (path, source, root, lang) in inventory.parsed() {
        uses += prove_file(
            path,
            source,
            root,
            lang,
            &paths,
            packages,
            site,
            delta,
            &mut positions,
            &mut consumers,
        )?;
    }
    if uses == 0
        || refs.iter().any(|reference| {
            !positions.contains(&(
                normalize_workspace_separators(&reference.path),
                reference.line,
                reference.column,
            ))
        })
    {
        return None;
    }
    consumers.sort();
    consumers.dedup();
    Some(consumers)
}

#[allow(clippy::too_many_arguments)]
fn prove_file(
    path: &str,
    source: &[u8],
    root: Node<'_>,
    lang: crate::language::LangId,
    paths: &HashMap<String, Vec<String>>,
    packages: Option<&PackageBoundary>,
    site: &CompatibleModSite<'_>,
    delta: &Delta,
    positions: &mut HashSet<(String, usize, usize)>,
    consumers: &mut Vec<Consumer>,
) -> Option<usize> {
    let nodes = checked_nodes(root)?;
    let mut imported = HashSet::new();
    let mut import_statements = HashSet::new();
    for &node in &nodes {
        if escaped_identifier(node, source) || dynamic_execution_entry(node, source) {
            return None;
        }
        match node.kind() {
            "ambient_declaration"
            | "internal_module"
            | "module"
            | "import_alias"
            | "import_require_clause"
            | "meta_property" => return None,
            "comment"
                if node.utf8_text(source).is_ok_and(|comment| {
                    comment.contains(site.name) || comment.contains("\\u")
                }) =>
            {
                return None;
            }
            "string" | "template_string"
                if static_specifier(node, source)
                    .is_some_and(|value| value == site.name || value.contains('\\')) =>
            {
                return None;
            }
            "call_expression"
                if node
                    .child_by_field_name("function")
                    .is_some_and(|function| {
                        function.kind() == "import" || text(function, source) == Some("require")
                    }) =>
            {
                return None;
            }
            "import_statement" | "export_statement" => {
                let Some(specifier) = node.child_by_field_name("source") else {
                    continue;
                };
                let target = dependency_target(specifier, source, path, paths);
                if target.is_none()
                    && !(node.kind() == "import_statement"
                        && imports_only_unrelated_names(node, source, site.name)
                        && static_specifier(specifier, source).is_some_and(|name| {
                            packages.is_some_and(|packages| packages.is_registry_dependency(name))
                        }))
                {
                    return None;
                }
                if target.as_deref() != Some(site.new_path) {
                    continue;
                }
                if node.kind() != "import_statement" {
                    return None;
                }
                let clause = children(node)
                    .into_iter()
                    .find(|child| child.kind() == "import_clause")?;
                let clause_parts = children(clause);
                let [named] = clause_parts.as_slice() else {
                    return None;
                };
                if named.kind() != "named_imports" {
                    return None;
                }
                for spec in children(*named) {
                    if spec.kind() != "import_specifier"
                        || spec.child_by_field_name("alias").is_some()
                    {
                        return None;
                    }
                    let imported_name = spec.child_by_field_name("name")?;
                    if text(imported_name, source)? == site.name {
                        imported.insert(imported_name.id());
                        import_statements.insert(node.id());
                        import_statements.insert(spec.id());
                    }
                }
            }
            _ => {}
        }
    }
    let declaration = if path == site.new_path {
        Some(unique_type_declaration(root, site.name, source)?.0)
    } else {
        None
    };
    let bindings: Vec<_> = collect_bindings_named(root, source, site.name)
        .into_iter()
        .filter(|binding| binding.kind() != "generic_type")
        .collect();
    if let Some(declaration) = declaration {
        if bindings
            .iter()
            .any(|binding| binding.id() != declaration.id())
        {
            return None;
        }
    } else if bindings
        .iter()
        .any(|binding| !import_statements.contains(&binding.id()))
    {
        return None;
    }
    let mut uses = 0;
    for &node in &nodes {
        if !matches!(
            node.kind(),
            "identifier"
                | "type_identifier"
                | "property_identifier"
                | "shorthand_property_identifier"
        ) || text(node, source) != Some(site.name)
        {
            continue;
        }
        let point = node.start_position();
        positions.insert((path.to_owned(), point.row, point.column));
        if declaration.is_some_and(|declaration| {
            node.start_byte() >= declaration.start_byte()
                && node.end_byte() <= declaration.end_byte()
        }) || imported.contains(&node.id())
        {
            continue;
        }
        if !TS_ONLY_LANGS.contains(&lang)
            || (path != site.new_path && imported.is_empty())
            || node.kind() != "type_identifier"
        {
            return None;
        }
        let mut ty = node;
        let mut arity = 0;
        if let Some(parent) = node
            .parent()
            .filter(|parent| parent.kind() == "generic_type")
        {
            if parent.child_by_field_name("name")?.id() != node.id() {
                return None;
            }
            let args = children(parent.child_by_field_name("type_arguments")?);
            for &argument in &args {
                simple_type(argument, source)?;
            }
            arity = args.len();
            ty = parent;
        }
        if arity < delta.required || arity > delta.old_arity {
            return None;
        }
        let annotation = ty.parent()?;
        if annotation.kind() != "type_annotation" || children(annotation).len() != 1 {
            return None;
        }
        let parameter = annotation.parent()?;
        if parameter.kind() != "required_parameter"
            || parameter.child_by_field_name("type")?.id() != annotation.id()
            || parameter.child_by_field_name("value").is_some()
        {
            return None;
        }
        let (name, point) = prove_consumer(parameter, root, source, delta)?;
        consumers.push(Consumer {
            path: path.to_owned(),
            name,
            line: point.row,
            column: point.column,
        });
        uses += 1;
    }
    Some(uses)
}

fn prove_consumer(
    parameter: Node<'_>,
    root: Node<'_>,
    source: &[u8],
    delta: &Delta,
) -> Option<(String, tree_sitter::Point)> {
    let pattern = parameter.child_by_field_name("pattern")?;
    if pattern.kind() != "identifier" {
        return None;
    }
    let name = text(pattern, source)?;
    let function = parameter.parent()?.parent()?;
    if function.kind() != "function_declaration"
        || function.child_by_field_name("type_parameters").is_some()
    {
        return None;
    }
    let parent = function.parent()?;
    if parent.id() != root.id()
        && !(parent.kind() == "export_statement" && parent.parent()?.id() == root.id())
    {
        return None;
    }
    let function_name_node = function.child_by_field_name("name")?;
    let function_name = text(function_name_node, source)?;
    if collect_bindings_named(root, source, function_name).len() != 1 {
        return None;
    }
    let return_type = function.child_by_field_name("return_type")?;
    let return_types = children(return_type);
    let [return_type] = return_types.as_slice() else {
        return None;
    };
    simple_type(*return_type, source)?;
    let body = function.child_by_field_name("body")?;
    if body_rebinds_or_queries(body, source, name, None) {
        return None;
    }
    let mut reads = 0;
    for node in checked_nodes(body)? {
        if !matches!(node.kind(), "identifier" | "shorthand_property_identifier")
            || text(node, source) != Some(name)
        {
            continue;
        }
        // 同名の局所束縛や閉包は、この初期の証明では追わない。
        let (scope, _) = resolve_binding_scope(node, source, name, Namespace::Value)?;
        if scope.id() != function.id() {
            return None;
        }
        let member = node.parent()?;
        if member.kind() != "member_expression"
            || member.child_by_field_name("object")?.id() != node.id()
        {
            return None;
        }
        let property = text(member.child_by_field_name("property")?, source)?;
        if !delta.retained.contains(property) {
            return None;
        }
        let parent = member.parent()?;
        if matches!(
            parent.kind(),
            "assignment_expression"
                | "augmented_assignment_expression"
                | "update_expression"
                | "unary_expression"
                | "call_expression"
        ) {
            return None;
        }
        reads += 1;
    }
    (reads > 0).then(|| {
        (
            function_name.to_owned(),
            function_name_node.start_position(),
        )
    })
}

fn prove_consumer_unused(
    consumer: &Consumer,
    inventory: &InventoryView<'_>,
    packages: Option<&PackageBoundary>,
) -> Option<()> {
    let paths = inventory
        .parsed()
        .map(|(path, _, _, _)| (path.to_owned(), Vec::new()))
        .collect();
    for (path, source, root, _) in inventory.parsed() {
        for node in checked_nodes(root)? {
            if escaped_identifier(node, source) || dynamic_execution_entry(node, source) {
                return None;
            }
            match node.kind() {
                "ambient_declaration"
                | "internal_module"
                | "module"
                | "import_alias"
                | "import_require_clause"
                | "meta_property" => return None,
                "comment"
                    if node.utf8_text(source).is_ok_and(|comment| {
                        comment.contains(&consumer.name) || comment.contains("\\u")
                    }) =>
                {
                    return None;
                }
                "string" | "template_string"
                    if static_specifier(node, source)
                        .is_some_and(|value| value == consumer.name || value.contains('\\')) =>
                {
                    return None;
                }
                "call_expression"
                    if node
                        .child_by_field_name("function")
                        .is_some_and(|function| {
                            function.kind() == "import" || text(function, source) == Some("require")
                        }) =>
                {
                    return None;
                }
                "import_statement" | "export_statement" => {
                    if let Some(specifier) = node.child_by_field_name("source") {
                        let target = dependency_target(specifier, source, path, &paths);
                        if target.as_deref() == Some(consumer.path.as_str())
                            || (target.is_none()
                                && !static_specifier(specifier, source).is_some_and(|name| {
                                    packages.is_some_and(|packages| {
                                        packages.is_registry_dependency(name)
                                    })
                                }))
                        {
                            return None;
                        }
                    }
                }
                _ => {}
            }
            if matches!(
                node.kind(),
                "identifier"
                    | "type_identifier"
                    | "property_identifier"
                    | "shorthand_property_identifier"
            ) && text(node, source) == Some(consumer.name.as_str())
            {
                let point = node.start_position();
                if path != consumer.path
                    || point.row != consumer.line
                    || point.column != consumer.column
                {
                    return None;
                }
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn trailing_default_type_params_node_kinds_exist_in_both_grammars() {
        for lang in [
            crate::language::LangId::Typescript,
            crate::language::LangId::Tsx,
        ] {
            for kind in [
                "type_parameters",
                "type_parameter",
                "default_type",
                "constraint",
                "generic_type",
                "type_arguments",
                "type_annotation",
                "required_parameter",
                "interface_declaration",
                "interface_body",
                "property_signature",
                "export_statement",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), &format!("({kind}) @node"))
                    .unwrap_or_else(|error| panic!("{lang:?}/{kind}: {error}"));
            }
        }
    }
}
