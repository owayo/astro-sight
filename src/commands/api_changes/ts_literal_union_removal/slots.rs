//! 対象型の参照を、値を受ける三種の宣言だけへ分類する。

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Primitive {
    String,
    Number,
    Boolean,
    BigInt,
    Void,
}
impl Primitive {
    pub(super) fn read(node: Node<'_>, source: &str) -> Option<Self> {
        if node.kind() != "predefined_type" {
            return None;
        }
        Some(match text(node, source)? {
            "string" => Self::String,
            "number" => Self::Number,
            "boolean" => Self::Boolean,
            "bigint" => Self::BigInt,
            "void" => Self::Void,
            _ => return None,
        })
    }
    pub(super) fn accepts(self, node: Node<'_>, source: &str) -> bool {
        match (self, node.kind()) {
            (Self::String, "string") => decode::string(node, source).is_some(),
            (Self::Boolean, "true" | "false") => true,
            (Self::Number, "number") => {
                text(node, source).is_some_and(|number| !number.ends_with('n'))
            }
            (Self::BigInt, "number") => {
                text(node, source).is_some_and(|number| number.ends_with('n'))
            }
            (Self::Number | Self::BigInt, "unary_expression") => {
                node.child_by_field_name("operator")
                    .is_some_and(|operator| operator.kind() == "-")
                    && node
                        .child_by_field_name("argument")
                        .is_some_and(|argument| self.accepts(argument, source))
            }
            _ => false,
        }
    }
}
#[derive(Clone)]
pub(super) struct Binding {
    pub(super) path: String,
    pub(super) name: String,
    pub(super) scope: Span,
    pub(super) declaration: Span,
}
pub(super) struct Record {
    pub(super) site: Site,
    pub(super) primitive: Primitive,
}
pub(super) struct Consumer {
    pub(super) site: Site,
    pub(super) positions: BTreeSet<usize>,
    pub(super) arity: usize,
    pub(super) result: Primitive,
}
pub(super) struct Plan {
    pub(super) target: Site,
    pub(super) records: Vec<Record>,
    pub(super) arrays: Vec<Site>,
    pub(super) consumers: Vec<Consumer>,
    pub(super) bindings: Vec<Binding>,
}

pub(super) fn collect(
    inventory: &inventory::Inventory,
    site: &CompatibleModSite<'_>,
    removal: &shape::Removal,
) -> Option<Plan> {
    let source = inventory.source(site.new_path)?;
    let declaration = shape::declaration(inventory.root(site.new_path)?, source, site.name)?;
    let mut plan = Plan {
        target: Site {
            path: site.new_path.to_owned(),
            name: site.name.to_owned(),
            span: span(declaration),
        },
        records: Vec::new(),
        arrays: Vec::new(),
        consumers: Vec::new(),
        bindings: Vec::new(),
    };
    let names = BTreeSet::from([site.name.to_owned()]);
    for (path, file) in inventory.relevant(&names) {
        let root = inventory.root(path)?;
        for node in nodes(root) {
            if !crate::engine::js_occurrence::identifier_kind(node.kind()) {
                continue;
            }
            let name = text(node, &file.source)?;
            if name.contains('\\') {
                if decode::identifier(name).as_deref() == Some(site.name) {
                    return None;
                }
                continue;
            }
            if name != site.name {
                continue;
            }
            let owner = type_owner(inventory, path, node, site.name)?;
            if owner != plan.target {
                continue;
            }
            if span(node) == declaration.child_by_field_name("name").map(span)?
                && path == site.new_path
            {
                continue;
            }
            if enclosing_import(node).is_some() {
                continue;
            }
            let inventory::Kind::Source(LangId::Typescript | LangId::Tsx) = file.kind else {
                return None;
            };
            if node.kind() != "type_identifier" {
                return None;
            }
            if let Some(declarator) = carrier_declaration(node) {
                let record = record_slot(inventory, path, declarator, node, removal)?;
                if !plan
                    .records
                    .iter()
                    .any(|previous| previous.site == record.site)
                {
                    plan.records.push(record);
                }
            } else if let Some(declarator) = array_declaration(node) {
                let array = array_slot(inventory, path, declarator, removal)?;
                if !plan.arrays.contains(&array) {
                    plan.arrays.push(array);
                }
            } else {
                let parameter = node
                    .parent()
                    .filter(|parent| parent.kind() == "type_annotation")
                    .and_then(|annotation| annotation.parent())
                    .filter(|node| node.kind() == "required_parameter")?;
                consumer_slot(inventory, path, parameter, &mut plan)?;
            }
        }
    }
    (!plan.records.is_empty() || !plan.arrays.is_empty() || !plan.consumers.is_empty())
        .then_some(plan)
}

pub(super) fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let _ = crate::engine::tree_walk::walk_preorder(root, Children::Named, |node, _| {
        result.push(node);
        std::ops::ControlFlow::<(), _>::Continue(crate::engine::tree_walk::Walk::Descend)
    });
    result
}

pub(super) fn enclosing_import(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_DEPTH {
        if node.kind() == "import_statement" {
            return Some(node);
        }
        node = node.parent()?;
    }
    None
}

pub(super) fn module(inventory: &inventory::Inventory, from: &str, spec: &str) -> Option<String> {
    inventory.caller_resolution_is_plain(from)?;
    let normalized = super::super::removed_attribution::normalize_relative_specifier(from, spec)?;
    let candidates = super::super::removed_attribution::relative_import_candidates(from, spec)?;
    let existing = candidates
        .into_iter()
        .filter(|candidate| inventory.dir.join(candidate).is_file())
        .collect::<Vec<_>>();
    let [target] = existing.as_slice() else {
        return None;
    };
    let mut alternatives = preceding_alternatives(&normalized);
    alternatives.retain(|alternative| alternative != target);
    inventory.alternative_inputs_are_absent(&alternatives)?;
    let target_path = std::path::Path::new(target);
    if target_path.parent() == Some(std::path::Path::new(&normalized)) {
        inventory.directory_index_is_plain(target)?;
    }
    inventory
        .files
        .get(target)
        .filter(|file| matches!(file.kind, inventory::Kind::Source(_)))?;
    Some(target.clone())
}

/// TS の解決順で共有候補より先に試され得る入力を、所有元の肯定前に監査する。
fn preceding_alternatives(normalized: &str) -> Vec<String> {
    for (suffix, implementations) in [
        (".d.ts", &["ts", "tsx"][..]),
        (".d.mts", &["mts"][..]),
        (".d.cts", &["cts"][..]),
    ] {
        if let Some(stem) = normalized.strip_suffix(suffix) {
            return implementations
                .iter()
                .map(|extension| format!("{stem}.{extension}"))
                .collect();
        }
    }
    let extension = std::path::Path::new(normalized)
        .extension()
        .and_then(|extension| extension.to_str());
    // 明示の実装 source は exact path が第一。存在しなければ共有候補も成功しない。
    if matches!(extension, Some("ts" | "tsx" | "mts" | "cts")) {
        return Vec::new();
    }
    let javascript = matches!(extension, Some("js" | "jsx" | "mjs" | "cjs"));
    let stem = if javascript {
        normalized
            .rsplit_once('.')
            .map(|(stem, _)| stem)
            .unwrap_or(normalized)
    } else {
        normalized
    };
    const SOURCE_SUFFIXES: [&str; 7] = ["ts", "tsx", "mts", "cts", "d.ts", "d.mts", "d.cts"];
    let index = if normalized.is_empty() {
        "index".to_owned()
    } else {
        format!("{normalized}/index")
    };
    let package = if normalized.is_empty() {
        "package.json".to_owned()
    } else {
        format!("{normalized}/package.json")
    };
    let mut alternatives = SOURCE_SUFFIXES
        .map(|extension| format!("{stem}.{extension}"))
        .to_vec();
    if javascript {
        // node10/classic は拡張子を残した TS source を、JS source より先に調べる。
        alternatives.extend(SOURCE_SUFFIXES.map(|extension| format!("{normalized}.{extension}")));
    } else if let Some((stem, extension)) = normalized.rsplit_once('.')
        && !extension.contains('/')
    {
        alternatives.push(format!("{stem}.d.{extension}.ts"));
    }
    alternatives.extend(SOURCE_SUFFIXES.map(|extension| format!("{index}.{extension}")));
    alternatives.push(package);
    alternatives.sort();
    alternatives.dedup();
    alternatives
}

fn imported(
    inventory: &inventory::Inventory,
    path: &str,
    statement: Node<'_>,
    name: &str,
    value: bool,
) -> Option<String> {
    let source = inventory.source(path)?;
    if statement.parent()?.kind() != "program" || (value && token(statement, "type")) {
        return None;
    }
    let specifiers = nodes(statement)
        .into_iter()
        .filter(|node| {
            node.kind() == "import_specifier"
                && node
                    .child_by_field_name("name")
                    .and_then(|node| text(node, source))
                    == Some(name)
        })
        .collect::<Vec<_>>();
    let [specifier] = specifiers.as_slice() else {
        return None;
    };
    if specifier.child_by_field_name("alias").is_some() || (value && token(*specifier, "type")) {
        return None;
    }
    let literal = statement.child_by_field_name("source")?;
    let raw = text(literal, source)?;
    if raw.contains('\\') {
        return None;
    }
    module(inventory, path, &decode::string(literal, source)?)
}

pub(super) fn type_owner(
    inventory: &inventory::Inventory,
    path: &str,
    node: Node<'_>,
    name: &str,
) -> Option<Site> {
    let source = inventory.source(path)?;
    if let Some(statement) = enclosing_import(node) {
        let target = imported(inventory, path, statement, name, false)?;
        return root_site(inventory, &target, name, true, true);
    }
    match resolve_binding_scope(node, source.as_bytes(), name, Namespace::Type)? {
        (scope, ScopeBinding::TypeDeclaration) => {
            if scope.kind() == "program" {
                root_site(inventory, path, name, true, false)
            } else {
                Some(Site {
                    path: path.to_owned(),
                    name: name.to_owned(),
                    span: span(scope),
                })
            }
        }
        (_, ScopeBinding::Import(statement)) => {
            let target = imported(inventory, path, statement, name, false)?;
            root_site(inventory, &target, name, true, true)
        }
        _ => None,
    }
}

pub(super) fn value_owner(
    inventory: &inventory::Inventory,
    path: &str,
    node: Node<'_>,
    name: &str,
) -> Option<Site> {
    let source = inventory.source(path)?;
    if let Some(statement) = enclosing_import(node) {
        let target = imported(inventory, path, statement, name, true)?;
        return root_site(inventory, &target, name, false, true);
    }
    match resolve_binding_scope(node, source.as_bytes(), name, Namespace::Value)? {
        (scope, ScopeBinding::FunctionDeclaration) if scope.kind() == "program" => {
            root_site(inventory, path, name, false, false)
        }
        (_, ScopeBinding::Variable(declaration)) => Some(Site {
            path: path.to_owned(),
            name: name.to_owned(),
            span: span(declaration),
        }),
        (_, ScopeBinding::Import(statement)) => {
            let target = imported(inventory, path, statement, name, true)?;
            root_site(inventory, &target, name, false, true)
        }
        (scope, _) => Some(Site {
            path: path.to_owned(),
            name: name.to_owned(),
            span: span(scope),
        }),
    }
}

fn root_site(
    inventory: &inventory::Inventory,
    path: &str,
    name: &str,
    type_only: bool,
    require_export: bool,
) -> Option<Site> {
    let source = inventory.source(path)?;
    let root = inventory.root(path)?;
    let bindings =
        super::super::ts_const_arg::collect_bindings_named(root, source.as_bytes(), name);
    let [binding] = bindings.as_slice() else {
        return None;
    };
    let expected = if type_only {
        matches!(
            binding.kind(),
            "type_alias_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "class_declaration"
        )
    } else {
        matches!(
            binding.kind(),
            "function_declaration" | "variable_declarator" | "class_declaration"
        )
    };
    if !expected || outer_declaration(*binding).is_none() || (require_export && !exported(*binding))
    {
        return None;
    }
    Some(Site {
        path: path.to_owned(),
        name: name.to_owned(),
        span: span(*binding),
    })
}

fn declared_site(
    inventory: &inventory::Inventory,
    path: &str,
    declaration: Node<'_>,
) -> Option<Site> {
    outer_declaration(declaration)?;
    let source = inventory.source(path)?;
    let name = text(declaration.child_by_field_name("name")?, source)?;
    if !plain_name(name) {
        return None;
    }
    let expected = root_site(inventory, path, name, false, false)?;
    (expected.span == span(declaration)).then_some(expected)
}

fn carrier_declaration(node: Node<'_>) -> Option<Node<'_>> {
    let arguments = node
        .parent()
        .filter(|parent| parent.kind() == "type_arguments")?;
    let generic = arguments
        .parent()
        .filter(|parent| parent.kind() == "generic_type")?;
    let annotation = generic
        .parent()
        .filter(|parent| parent.kind() == "type_annotation")?;
    let declaration = annotation
        .parent()
        .filter(|parent| parent.kind() == "variable_declarator")?;
    (declaration.child_by_field_name("type")?.id() == annotation.id()).then_some(declaration)
}
fn array_declaration(mut node: Node<'_>) -> Option<Node<'_>> {
    node = node
        .parent()
        .filter(|parent| parent.kind() == "array_type")?;
    if node.parent()?.kind() == "readonly_type" {
        node = node.parent()?;
    }
    let annotation = node
        .parent()
        .filter(|parent| parent.kind() == "type_annotation")?;
    annotation
        .parent()
        .filter(|parent| parent.kind() == "variable_declarator")
}
fn is_const(declaration: Node<'_>) -> bool {
    declaration
        .parent()
        .is_some_and(|parent| parent.kind() == "lexical_declaration" && token(parent, "const"))
}

fn record_slot(
    inventory: &inventory::Inventory,
    path: &str,
    declaration: Node<'_>,
    mode: Node<'_>,
    removal: &shape::Removal,
) -> Option<Record> {
    if !is_const(declaration) {
        return None;
    }
    let source = inventory.source(path)?;
    let generic = only(declaration.child_by_field_name("type")?)?;
    if generic.kind() != "generic_type"
        || generic
            .child_by_field_name("name")
            .and_then(|node| text(node, source))
            != Some("Record")
    {
        return None;
    }
    let arguments = children(generic.child_by_field_name("type_arguments")?);
    let [key, value] = arguments.as_slice() else {
        return None;
    };
    if span(*key) != span(mode) {
        return None;
    }
    let primitive = Primitive::read(*value, source)?;
    if primitive == Primitive::Void {
        return None;
    }
    let object = declaration
        .child_by_field_name("value")
        .filter(|value| value.kind() == "object")?;
    let mut keys = BTreeSet::new();
    for member in children(object) {
        if member.kind() != "pair" {
            return None;
        }
        let key = member.child_by_field_name("key")?;
        let name = match key.kind() {
            "property_identifier" => text(key, source)?.to_owned(),
            "string" => decode::string(key, source)?,
            _ => return None,
        };
        if text(key, source)?.contains('\\')
            || !keys.insert(name)
            || !primitive.accepts(member.child_by_field_name("value")?, source)
        {
            return None;
        }
    }
    (keys == removal.kept)
        .then(|| declared_site(inventory, path, declaration).map(|site| Record { site, primitive }))
        .flatten()
}
fn array_slot(
    inventory: &inventory::Inventory,
    path: &str,
    declaration: Node<'_>,
    removal: &shape::Removal,
) -> Option<Site> {
    if !is_const(declaration) {
        return None;
    }
    let source = inventory.source(path)?;
    let array = declaration.child_by_field_name("value")?;
    if array.kind() != "array" {
        return None;
    }
    let mut expect = true;
    let mut cursor = array.walk();
    for node in array.children(&mut cursor) {
        match node.kind() {
            "[" | "]" | "comment" => {}
            "," => {
                if expect {
                    return None;
                }
                expect = true;
            }
            "string" => {
                if !removal.kept.contains(&decode::string(node, source)?) {
                    return None;
                }
                expect = false;
            }
            _ => return None,
        }
    }
    declared_site(inventory, path, declaration)
}
fn consumer_slot(
    inventory: &inventory::Inventory,
    path: &str,
    parameter: Node<'_>,
    plan: &mut Plan,
) -> Option<()> {
    let function = parameter
        .parent()
        .filter(|parent| parent.kind() == "formal_parameters")?
        .parent()?;
    if function.kind() != "function_declaration"
        || function.child_by_field_name("type_parameters").is_some()
        || token(function, "async")
        || token(function, "*")
        || function.child_by_field_name("body").is_none()
        || function
            .parent()
            .is_some_and(|parent| parent.kind() == "export_statement" && token(parent, "default"))
    {
        return None;
    }
    let source = inventory.source(path)?;
    let site = declared_site(inventory, path, function)?;
    let result = Primitive::read(only(function.child_by_field_name("return_type")?)?, source)?;
    let parameters = children(function.child_by_field_name("parameters")?);
    let mut names = BTreeSet::new();
    let mut positions = BTreeSet::new();
    for (index, param) in parameters.iter().enumerate() {
        let name = param.child_by_field_name("pattern")?;
        if param.kind() != "required_parameter"
            || name.kind() != "identifier"
            || param.child_by_field_name("value").is_some()
            || !plain_name(text(name, source)?)
            || !names.insert(text(name, source)?)
        {
            return None;
        }
        let ty = only(param.child_by_field_name("type")?)?;
        if ty.kind() == "type_identifier" && text(ty, source)? == plan.target.name {
            if type_owner(inventory, path, ty, &plan.target.name)? != plan.target {
                return None;
            }
            positions.insert(index);
            let binding = Binding {
                path: path.to_owned(),
                name: text(name, source)?.to_owned(),
                scope: span(function),
                declaration: span(name),
            };
            if !plan.bindings.iter().any(|previous| {
                previous.path == binding.path && previous.declaration == binding.declaration
            }) {
                plan.bindings.push(binding);
            }
        } else if Primitive::read(ty, source).is_none_or(|primitive| primitive == Primitive::Void) {
            return None;
        }
    }
    if positions.is_empty() {
        return None;
    }
    if !plan.consumers.iter().any(|consumer| consumer.site == site) {
        plan.consumers.push(Consumer {
            site,
            positions,
            arity: parameters.len(),
            result,
        });
    }
    Some(())
}

#[cfg(test)]
mod alternative_tests {
    use super::*;
    #[test]
    fn literal_union_preceding_inputs_keep_root_relative_paths() {
        let alternatives = preceding_alternatives("");
        assert!(alternatives.contains(&"index.d.ts".to_owned()));
        assert!(alternatives.contains(&"package.json".to_owned()));
        assert!(
            alternatives
                .iter()
                .all(|alternative| !std::path::Path::new(alternative).is_absolute())
        );
    }
}
