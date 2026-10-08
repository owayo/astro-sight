//! Python の Protocol 引数変更に、実装クラス側の構造的な適合証拠を付ける。
//! 名前だけの duck typing はせず、解決できない型・束縛・契約は blocking に残す。

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use tree_sitter::Node;

use super::ref_index::{ApiClosureCaches, parsed_ref_file};
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::engine::python_signature::normalize_signature_range;
use crate::language::LangId;
use crate::models::reference::SymbolReference;

type Contract = BTreeMap<String, String>;

pub(crate) struct ProtocolArgumentChange {
    name: String,
    target_path: String,
    parameter_count: usize,
    required_count: usize,
    changes: Vec<ParameterChange>,
    class_results: RefCell<HashMap<(String, String, usize), bool>>,
    caller_facts: RefCell<HashMap<String, CallerFacts>>,
}

#[derive(Default)]
struct CallerFacts {
    static_checks: HashMap<usize, bool>,
    definitions: HashMap<(String, &'static str), Option<String>>,
    locals: HashMap<(usize, String), Option<LocalArgumentType>>,
}

#[derive(Clone)]
struct LocalArgumentType {
    class: String,
    optional: bool,
    declaration_end: usize,
}

impl CallerFacts {
    fn definition_file(
        &mut self,
        dir: &str,
        path: &str,
        root: Node<'_>,
        source: &[u8],
        name: &str,
        kind: &'static str,
    ) -> Option<String> {
        self.definitions
            .entry((name.to_owned(), kind))
            .or_insert_with(|| resolve_definition_file(dir, path, root, source, name, kind))
            .clone()
    }
}

struct ParameterChange {
    index: usize,
    optional: bool,
    new: Contract,
}

/// 引数名・順序・既定値・戻り型を保った、ローカル Protocol 注釈の置換だけを抽出する。
pub(super) fn prepare_protocol_argument_change(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<ProtocolArgumentChange> {
    if site.lang_id != Some(LangId::Python) || site.kind != "function" || site.name.contains('.') {
        return None;
    }
    let target_path = workspace_path(site.dir, site.new_path)?;
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(LangId::Python)?;
    let old_root = old_tree.root_node();
    let new_root = new_tree.root_node();
    if old_root.has_error() || new_root.has_error() {
        return None;
    }
    let old_fn = local_definition(old_root, &src.old, site.name, "function_definition")?;
    let new_fn = local_definition(new_root, &src.new, site.name, "function_definition")?;
    if old_fn.child_by_field_name("type_parameters").is_some()
        || new_fn.child_by_field_name("type_parameters").is_some()
    {
        return None;
    }
    let old_params = old_fn.child_by_field_name("parameters")?;
    let new_params = new_fn.child_by_field_name("parameters")?;
    for (old_range, new_range) in [
        (
            old_fn.start_byte()..old_params.start_byte(),
            new_fn.start_byte()..new_params.start_byte(),
        ),
        (
            old_params.end_byte()..old_fn.child_by_field_name("body")?.start_byte(),
            new_params.end_byte()..new_fn.child_by_field_name("body")?.start_byte(),
        ),
    ] {
        if normalize_signature_range(old_fn, &src.old, old_range)?
            != normalize_signature_range(new_fn, &src.new, new_range)?
        {
            return None;
        }
    }
    let old = parameters(old_params)?;
    let new = parameters(new_params)?;
    if old.len() != new.len() || old.is_empty() {
        return None;
    }
    let mut parameter_names = BTreeSet::new();
    if new.iter().any(|param| {
        parameter_name(*param)
            .and_then(|name| text(name, &src.new))
            .is_none_or(|name| !parameter_names.insert(name))
    }) {
        return None;
    }
    let mut changes = Vec::new();
    let mut required_count = 0;
    for (index, (left, right)) in old.iter().zip(&new).enumerate() {
        if parameter_shape(*left, &src.old)? != parameter_shape(*right, &src.new)? {
            return None;
        }
        if right.child_by_field_name("value").is_none() {
            required_count = index + 1;
        }
        let (Some(old_type), Some(new_type)) = (
            left.child_by_field_name("type"),
            right.child_by_field_name("type"),
        ) else {
            if normalize(*left, &src.old)? != normalize(*right, &src.new)? {
                return None;
            }
            continue;
        };
        if normalize(old_type, &src.old)? == normalize(new_type, &src.new)? {
            continue;
        }
        let (new_name, optional) = named_optional_type(new_type, &src.new)?;
        let new_contract = protocol_contract(new_root, &src.new, &new_name, &mut BTreeSet::new())?;
        if new_contract.is_empty() || !module_is_static(new_root, &src.new, &new_contract)? {
            return None;
        }
        changes.push(ParameterChange {
            index,
            optional,
            new: new_contract,
        });
    }
    (!changes.is_empty()).then_some(ProtocolArgumentChange {
        name: site.name.to_owned(),
        target_path,
        parameter_count: new.len(),
        required_count,
        changes,
        class_results: RefCell::default(),
        caller_facts: RefCell::default(),
    })
}

impl ProtocolArgumentChange {
    pub(super) fn closes_reference(
        &self,
        dir: &str,
        reference: &SymbolReference,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        self.prove_reference(dir, reference, caches).is_some()
    }

    fn prove_reference(
        &self,
        dir: &str,
        reference: &SymbolReference,
        caches: &mut ApiClosureCaches,
    ) -> Option<()> {
        let path = workspace_path(dir, &reference.path)?;
        if LangId::from_path(camino::Utf8Path::new(&path)).ok()? != LangId::Python {
            return None;
        }
        let actuals = {
            let caller = parsed_ref_file(dir, &path, caches)?;
            if caller.tree.root_node().has_error() {
                return None;
            }
            let root = caller.tree.root_node();
            let source = &caller.source;
            let point = tree_sitter::Point {
                row: reference.line,
                column: reference.column,
            };
            let callee = root.descendant_for_point_range(point, point)?;
            if callee.kind() != "identifier" {
                return None;
            }
            let call = callee.parent().filter(|n| n.kind() == "call")?;
            if call.child_by_field_name("function")?.id() != callee.id() {
                return None;
            }
            let callee_name = text(callee, source)?;
            if callee_name != self.name {
                return None;
            }
            let mut caller_facts = self.caller_facts.borrow_mut();
            let facts = caller_facts.entry(path.clone()).or_default();
            if facts.definition_file(
                dir,
                &path,
                root,
                source,
                callee_name,
                "function_definition",
            )? != self.target_path
            {
                return None;
            }
            let arguments = call.child_by_field_name("arguments")?;
            if arguments.kind() != "argument_list" {
                return None;
            }
            let args = named_children(arguments)
                .into_iter()
                .filter(|node| node.kind() != "comment")
                .collect::<Vec<_>>();
            if args.len() < self.required_count
                || args.len() > self.parameter_count
                || args.iter().any(|n| {
                    matches!(
                        n.kind(),
                        "keyword_argument" | "list_splat" | "dictionary_splat"
                    )
                })
            {
                return None;
            }
            let mut actuals = Vec::new();
            for change in &self.changes {
                // 木は解析中に固定される。参照数だけ全体走査を繰り返さない。
                if !*facts
                    .static_checks
                    .entry(change.index)
                    .or_insert_with(|| module_is_static(root, source, &change.new).unwrap_or(false))
                {
                    return None;
                }
                let arg = *args.get(change.index)?;
                if arg.kind() != "identifier" {
                    return None;
                }
                let function = caller_function(call)?;
                let argument_name = text(arg, source)?;
                let local = facts
                    .locals
                    .entry((function.start_byte(), argument_name.to_owned()))
                    .or_insert_with(|| {
                        local_argument_type(function, argument_name, source, callee_name)
                    })
                    .as_ref()?;
                if local.declaration_end > call.start_byte() || local.optional && !change.optional {
                    return None;
                }
                let class = local.class.clone();
                let class_path =
                    facts.definition_file(dir, &path, root, source, &class, "class_definition")?;
                actuals.push((class_path, class));
            }
            actuals
        };
        for (change, (class_path, class)) in self.changes.iter().zip(actuals) {
            let key = (class_path.clone(), class.clone(), change.index);
            if let Some(valid) = self.class_results.borrow().get(&key) {
                if !valid {
                    return None;
                }
                continue;
            }
            let valid = (|| {
                let current = parsed_ref_file(dir, &class_path, caches)?;
                if current.lang_id != LangId::Python
                    || current.tree.root_node().has_error()
                    || !module_is_static(current.tree.root_node(), &current.source, &change.new)?
                {
                    return None;
                }
                let methods = concrete_methods(
                    current.tree.root_node(),
                    &current.source,
                    &class,
                    &change.new,
                )?;
                satisfies(&methods, &change.new).then_some(())
            })()
            .is_some();
            self.class_results.borrow_mut().insert(key, valid);
            if !valid {
                return None;
            }
        }
        Some(())
    }
}

struct Method {
    signature: Option<String>,
}

fn satisfies(methods: &BTreeMap<String, Method>, contract: &Contract) -> bool {
    contract.iter().all(|(name, signature)| {
        methods.get(name).and_then(|m| m.signature.as_ref()) == Some(signature)
    })
}

fn concrete_methods(
    root: Node<'_>,
    source: &[u8],
    name: &str,
    required: &Contract,
) -> Option<BTreeMap<String, Method>> {
    let class = local_definition(root, source, name, "class_definition")?;
    if class.child_by_field_name("superclasses").is_some()
        || class.child_by_field_name("type_parameters").is_some()
    {
        return None;
    }
    let mut methods = BTreeMap::new();
    for node in named_children(class.child_by_field_name("body")?) {
        if is_class_trivia(node) {
            continue;
        }
        if node.kind() != "function_definition" {
            return None;
        }
        if is_builtin_name(text(node.child_by_field_name("name")?, source)?) {
            return None;
        }
        let name = text(node.child_by_field_name("name")?, source)?.to_owned();
        if matches!(
            name.as_str(),
            "__getattribute__" | "__getattr__" | "__setattr__" | "__new__"
        ) || has_member_write(node, source, required)?
        {
            return None;
        }
        let signature = required
            .contains_key(&name)
            .then(|| method_signature(root, node, source))
            .flatten();
        let method = Method { signature };
        if methods.insert(name, method).is_some() || methods.len() > 256 {
            return None;
        }
    }
    Some(methods)
}

fn protocol_contract(
    root: Node<'_>,
    source: &[u8],
    name: &str,
    visiting: &mut BTreeSet<String>,
) -> Option<Contract> {
    if visiting.len() >= 32 || !visiting.insert(name.to_owned()) {
        return None;
    }
    let class = local_definition(root, source, name, "class_definition")?;
    if class.child_by_field_name("type_parameters").is_some() {
        return None;
    }
    let mut contract = BTreeMap::new();
    let mut has_protocol = false;
    for base in named_children(class.child_by_field_name("superclasses")?) {
        if base.kind() != "identifier" {
            return None;
        }
        let base_name = text(base, source)?;
        if base_name == "Protocol" {
            if !matches!(module_binding(root, source, base_name)?, Binding::Import(module) if matches!(module.as_str(), "typing" | "typing_extensions"))
            {
                return None;
            }
            if has_protocol {
                return None;
            }
            has_protocol = true;
        } else {
            for (member, signature) in protocol_contract(root, source, base_name, visiting)? {
                if contract.contains_key(&member) {
                    return None;
                }
                contract.insert(member, signature);
            }
        }
    }
    if !has_protocol {
        return None;
    }
    let mut own = BTreeSet::new();
    for node in named_children(class.child_by_field_name("body")?) {
        if is_class_trivia(node) {
            continue;
        }
        if node.kind() != "function_definition" {
            return None;
        }
        if is_builtin_name(text(node.child_by_field_name("name")?, source)?) {
            return None;
        }
        let member = text(node.child_by_field_name("name")?, source)?.to_owned();
        if !own.insert(member.clone()) {
            return None;
        }
        let signature = method_signature(root, node, source)?;
        if contract.contains_key(&member) {
            return None;
        }
        contract.insert(member, signature);
        if contract.len() > 256 {
            return None;
        }
    }
    visiting.remove(name);
    Some(contract)
}

/// 自己引数を除く契約の厳密一致だけを証明する。未知の型や variance は評価しない。
fn method_signature(root: Node<'_>, function: Node<'_>, source: &[u8]) -> Option<String> {
    let params = function.child_by_field_name("parameters")?;
    let parts = parameters(params)?;
    let first = *parts.first()?;
    if first.kind() != "identifier" || function.child_by_field_name("type_parameters").is_some() {
        return None;
    }
    let return_type = function.child_by_field_name("return_type")?;
    if !builtin_type(root, return_type, source, 0) {
        return None;
    }
    let mut names = BTreeSet::from([text(first, source)?.to_owned()]);
    for param in parts.iter().skip(1) {
        if !names.insert(text(parameter_name(*param)?, source)?.to_owned())
            || !builtin_type(root, param.child_by_field_name("type")?, source, 0)
        {
            return None;
        }
        if let Some(default) = param.child_by_field_name("value")
            && !matches!(
                default.kind(),
                "none" | "true" | "false" | "integer" | "float" | "string"
            )
        {
            return None;
        }
    }
    let mut pending = vec![function];
    let mut visited = 0;
    while let Some(node) = pending.pop() {
        visited += 1;
        if visited > 100_000 || node.kind() == "yield" {
            return None;
        }
        if node.kind() == "comment"
            && text(node, source)?
                .strip_prefix('#')?
                .trim_start()
                .starts_with("type:")
        {
            return None;
        }
        pending.extend(named_children(node));
    }
    let head =
        normalize_signature_range(function, source, function.start_byte()..params.start_byte())?;
    let name = text(function.child_by_field_name("name")?, source)?;
    if head != format!("def {name}") && head != format!("async def {name}") {
        return None;
    }
    Some(format!(
        "{}{} -> {}",
        if head.starts_with("async ") {
            "async "
        } else {
            ""
        },
        normalize_signature_range(params, source, first.end_byte()..params.end_byte())?,
        normalize(return_type, source)?
    ))
}

fn builtin_type(root: Node<'_>, node: Node<'_>, source: &[u8], depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    match node.kind() {
        "type" | "type_parameter" => {
            let children = named_children(node);
            !children.is_empty()
                && children
                    .iter()
                    .all(|child| builtin_type(root, *child, source, depth + 1))
        }
        "none" => true,
        "binary_operator" => {
            node.child_by_field_name("operator")
                .and_then(|operator| text(operator, source))
                == Some("|")
                && node
                    .child_by_field_name("left")
                    .is_some_and(|left| builtin_type(root, left, source, depth + 1))
                && node
                    .child_by_field_name("right")
                    .is_some_and(|right| builtin_type(root, right, source, depth + 1))
        }
        "identifier" => text(node, source).is_some_and(|name| {
            matches!(
                name,
                "str" | "int" | "float" | "bool" | "bytes" | "complex" | "object"
            ) && matches!(module_binding(root, source, name), Some(Binding::Absent))
        }),
        "generic_type" => {
            let children = named_children(node);
            let [base, args] = children.as_slice() else {
                return false;
            };
            let Some(name) = text(*base, source) else {
                return false;
            };
            if base.kind() != "identifier"
                || !matches!(module_binding(root, source, name), Some(Binding::Absent))
                || args.kind() != "type_parameter"
            {
                return false;
            }
            let args = named_children(*args);
            let valid_arity = match name {
                "list" | "set" | "frozenset" => args.len() == 1,
                "dict" => args.len() == 2,
                "tuple" => !args.is_empty(),
                _ => false,
            };
            valid_arity
                && args.iter().enumerate().all(|(index, arg)| {
                    let inner = if arg.kind() == "type" {
                        arg.named_child(0).unwrap_or(*arg)
                    } else {
                        *arg
                    };
                    if inner.kind() == "ellipsis" {
                        name == "tuple" && args.len() == 2 && index == 1
                    } else {
                        builtin_type(root, *arg, source, depth + 1)
                    }
                })
        }
        _ => false,
    }
}

fn caller_function(call: Node<'_>) -> Option<Node<'_>> {
    let mut parent = call.parent()?;
    while parent.kind() != "function_definition" {
        if matches!(parent.kind(), "class_definition" | "lambda" | "module") {
            return None;
        }
        parent = parent.parent()?;
    }
    if parent.parent()?.kind() != "module"
        || parent.child_by_field_name("type_parameters").is_some()
    {
        return None;
    }
    Some(parent)
}

fn local_argument_type(
    function: Node<'_>,
    argument_name: &str,
    source: &[u8],
    callee: &str,
) -> Option<LocalArgumentType> {
    if !scope_is_simple(function, source)? {
        return None;
    }
    let mut declaration = None;
    let mut assignments = Vec::new();
    for statement in named_children(function.child_by_field_name("body")?) {
        match statement.kind() {
            "comment" | "pass_statement" => continue,
            "return_statement" => {
                if statement.named_child_count() != 1 || statement.named_child(0)?.kind() != "call"
                {
                    return None;
                }
                continue;
            }
            "expression_statement" => {
                let children = named_children(statement);
                let [expression] = children.as_slice() else {
                    return None;
                };
                if expression.kind() == "assignment" {
                    // 連鎖代入の内側にも束縛があるため、外側の名前だけでは証明しない。
                    if expression
                        .child_by_field_name("right")
                        .is_some_and(|right| right.kind() == "assignment")
                    {
                        return None;
                    }
                    let left = expression.child_by_field_name("left")?;
                    if left.kind() != "identifier" {
                        return None;
                    }
                    let name = text(left, source)?;
                    if name == argument_name && declaration.replace(*expression).is_some() {
                        return None;
                    }
                    assignments.push(name);
                } else if !matches!(expression.kind(), "call" | "string") {
                    return None;
                }
            }
            _ => return None,
        }
    }
    let declaration = declaration?;
    let (class, optional) = named_optional_type(declaration.child_by_field_name("type")?, source)?;
    if assignments
        .iter()
        .any(|name| *name == class || *name == callee)
    {
        return None;
    }
    for param in parameters(function.child_by_field_name("parameters")?)? {
        let name = text(parameter_name(param)?, source)?;
        if name == argument_name || name == class || name == callee {
            return None;
        }
    }
    let value = declaration.child_by_field_name("right")?;
    if !(value.kind() == "none" && optional)
        && (value.kind() != "call"
            || text(value.child_by_field_name("function")?, source)? != class)
    {
        return None;
    }
    Some(LocalArgumentType {
        class,
        optional,
        declaration_end: declaration.end_byte(),
    })
}

enum Binding<'a> {
    Absent,
    Local(Node<'a>),
    Import(String),
}

/// モジュール直下の一意な宣言・別名なし from import に限定する。条件付き束縛は保留する。
fn module_binding<'a>(root: Node<'a>, source: &[u8], name: &str) -> Option<Binding<'a>> {
    if root.named_child_count() > 10_000 {
        return None;
    }
    let mut result = Binding::Absent;
    for node in named_children(root) {
        let found = match node.kind() {
            "comment" | "pass_statement" => None,
            "function_definition" | "class_definition" => {
                (text(node.child_by_field_name("name")?, source)? == name)
                    .then_some(Binding::Local(node))
            }
            "import_from_statement" => {
                let module_node = node.child_by_field_name("module_name")?;
                if module_node.kind() != "dotted_name" {
                    return None;
                }
                let module = text(module_node, source)?;
                let mut found = None;
                for child in named_children(node) {
                    if child.id() == module_node.id() || child.kind() == "comment" {
                        continue;
                    }
                    if child.kind() != "dotted_name" {
                        return None;
                    }
                    if text(child, source)? == name {
                        if found.is_some() {
                            return None;
                        }
                        found = Some(Binding::Import(module.to_owned()));
                    }
                }
                found
            }
            "import_statement" => {
                for child in named_children(node) {
                    if child.kind() != "dotted_name" {
                        return None;
                    }
                    if text(child, source)?.split('.').next()? == name {
                        return None;
                    }
                }
                None
            }
            "expression_statement" if is_docstring(node) => None,
            _ => return None,
        };
        if let Some(found) = found {
            if !matches!(result, Binding::Absent) {
                return None;
            }
            result = found;
        }
    }
    Some(result)
}

fn local_definition<'a>(root: Node<'a>, source: &[u8], name: &str, kind: &str) -> Option<Node<'a>> {
    match module_binding(root, source, name)? {
        Binding::Local(node) if node.kind() == kind => Some(node),
        _ => None,
    }
}

fn resolve_definition_file(
    dir: &str,
    current: &str,
    root: Node<'_>,
    source: &[u8],
    name: &str,
    kind: &str,
) -> Option<String> {
    match module_binding(root, source, name)? {
        Binding::Local(node) if node.kind() == kind => Some(current.to_owned()),
        Binding::Import(module) => {
            if !module.split('.').all(|part| {
                !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            }) {
                return None;
            }
            let relative = module.replace('.', "/");
            let candidates = [
                format!("{relative}.py"),
                format!("{relative}/__init__.py"),
                format!("{relative}.pyi"),
                format!("{relative}/__init__.pyi"),
            ];
            let found = candidates
                .iter()
                .filter(|path| {
                    std::path::Path::new(dir)
                        .join(path)
                        .try_exists()
                        .unwrap_or(true)
                })
                .collect::<Vec<_>>();
            let [path] = found.as_slice() else {
                return None;
            };
            if !path.ends_with(".py") {
                return None;
            }
            workspace_path(dir, path)
        }
        _ => None,
    }
}

fn workspace_path(dir: &str, path: &str) -> Option<String> {
    let root = std::fs::canonicalize(dir).ok()?;
    let abs = std::fs::canonicalize(root.join(path)).ok()?;
    if abs.with_extension("pyi").try_exists().unwrap_or(true) {
        return None;
    }
    let relative =
        crate::git_support::normalize_workspace_separators(abs.strip_prefix(root).ok()?.to_str()?);
    crate::engine::impact::is_safe_diff_path(&relative).then_some(relative)
}

fn has_member_write(function: Node<'_>, source: &[u8], required: &Contract) -> Option<bool> {
    let mut pending = vec![function];
    let mut visited = 0;
    while let Some(node) = pending.pop() {
        visited += 1;
        if visited > 100_000 {
            return None;
        }
        let targets = if node.kind() == "delete_statement" {
            Some(named_children(node))
        } else if matches!(
            node.kind(),
            "assignment" | "augmented_assignment" | "named_expression"
        ) {
            let left = node
                .child_by_field_name("left")
                .or_else(|| node.child_by_field_name("name"))?;
            Some(vec![left])
        } else {
            None
        };
        if let Some(mut targets) = targets {
            while let Some(target) = targets.pop() {
                if target.kind() == "attribute"
                    && required
                        .contains_key(text(target.child_by_field_name("attribute")?, source)?)
                {
                    return Some(true);
                }
                targets.extend(named_children(target));
            }
        }
        pending.extend(named_children(node));
    }
    Some(false)
}

fn parameters(node: Node<'_>) -> Option<Vec<Node<'_>>> {
    let parts = named_children(node)
        .into_iter()
        .filter(|n| n.kind() != "comment")
        .collect::<Vec<_>>();
    (parts.len() <= 64
        && parts.iter().all(|n| {
            matches!(
                n.kind(),
                "identifier" | "typed_parameter" | "default_parameter" | "typed_default_parameter"
            )
        }))
    .then_some(parts)
}

fn parameter_name(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "identifier" {
        return Some(node);
    }
    node.child_by_field_name("name")
        .or_else(|| node.named_child(0))
        .filter(|n| n.kind() == "identifier")
}

fn parameter_shape(node: Node<'_>, source: &[u8]) -> Option<(String, Option<String>, bool)> {
    Some((
        text(parameter_name(node)?, source)?.to_owned(),
        match node.child_by_field_name("value") {
            Some(n) => Some(normalize(n, source)?),
            None => None,
        },
        node.child_by_field_name("type").is_some(),
    ))
}

fn named_optional_type(node: Node<'_>, source: &[u8]) -> Option<(String, bool)> {
    let mut node = node;
    if node.kind() == "type" {
        node = node.named_child(0)?;
    }
    if node.kind() == "identifier" {
        return Some((text(node, source)?.to_owned(), false));
    }
    if node.kind() != "binary_operator"
        || text(node.child_by_field_name("operator")?, source)? != "|"
    {
        return None;
    }
    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    let name = if left.kind() == "identifier" && right.kind() == "none" {
        left
    } else if right.kind() == "identifier" && left.kind() == "none" {
        right
    } else {
        return None;
    };
    Some((text(name, source)?.to_owned(), true))
}

fn is_docstring(node: Node<'_>) -> bool {
    node.kind() == "expression_statement"
        && node.named_child_count() == 1
        && node.named_child(0).is_some_and(|n| n.kind() == "string")
}
fn is_class_trivia(node: Node<'_>) -> bool {
    matches!(node.kind(), "comment" | "pass_statement") || is_docstring(node)
}
fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}
fn normalize(node: Node<'_>, source: &[u8]) -> Option<String> {
    normalize_signature_range(node, source, node.byte_range())
}

fn is_builtin_name(name: &str) -> bool {
    matches!(
        name,
        "str"
            | "int"
            | "float"
            | "bool"
            | "bytes"
            | "complex"
            | "object"
            | "list"
            | "dict"
            | "set"
            | "frozenset"
            | "tuple"
    )
}

fn module_is_static(root: Node<'_>, source: &[u8], required: &Contract) -> Option<bool> {
    if has_member_write(root, source, required)? {
        return Some(false);
    }
    let mut pending = vec![root];
    let mut count = 0;
    while let Some(node) = pending.pop() {
        count += 1;
        if count > 100_000 {
            return None;
        }
        if node.kind() == "call" {
            let function = node.child_by_field_name("function")?;
            let name = if function.kind() == "attribute" {
                function.child_by_field_name("attribute")?
            } else {
                function
            };
            if text(name, source).is_some_and(|name| {
                matches!(
                    name,
                    "setattr" | "delattr" | "globals" | "locals" | "exec" | "eval"
                )
            }) {
                return Some(false);
            }
        }
        pending.extend(named_children(node));
    }
    Some(true)
}

fn scope_is_simple(root: Node<'_>, source: &[u8]) -> Option<bool> {
    let mut pending = vec![root];
    let mut count = 0;
    while let Some(node) = pending.pop() {
        count += 1;
        if count > 100_000 {
            return None;
        }
        if matches!(
            node.kind(),
            "named_expression"
                | "lambda"
                | "list_comprehension"
                | "set_comprehension"
                | "dictionary_comprehension"
                | "generator_expression"
                | "delete_statement"
                | "augmented_assignment"
        ) {
            return Some(false);
        }
        if node.kind() == "call"
            && text(node.child_by_field_name("function")?, source)
                .is_some_and(|name| matches!(name, "exec" | "eval" | "locals" | "globals"))
        {
            return Some(false);
        }
        pending.extend(named_children(node));
    }
    Some(true)
}

/// 別名 import の呼び出しは名前一致の refs に現れないため、生の import 参照も確認する。
pub(super) fn imports_are_unaliased(
    dir: &str,
    refs: &[SymbolReference],
    bare: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    let mut files = HashSet::new();
    for reference in refs {
        if LangId::from_path(camino::Utf8Path::new(&reference.path)).ok() != Some(LangId::Python)
            || !files.insert(reference.path.clone())
        {
            continue;
        }
        let Some(path) = workspace_path(dir, &reference.path) else {
            return false;
        };
        let Some(parsed) = parsed_ref_file(dir, &path, caches) else {
            return false;
        };
        if parsed.tree.root_node().has_error() {
            return false;
        }
        let mut pending = vec![parsed.tree.root_node()];
        let mut count = 0;
        while let Some(node) = pending.pop() {
            count += 1;
            if count > 100_000 {
                return false;
            }
            if node.kind() == "aliased_import"
                && node
                    .child_by_field_name("name")
                    .and_then(|name| text(name, &parsed.source))
                    == Some(bare)
            {
                return false;
            }
            pending.extend(named_children(node));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;

    fn contract(source: &str, name: &str) -> Option<Contract> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Python).unwrap();
        if tree.root_node().has_error() {
            return None;
        }
        protocol_contract(
            tree.root_node(),
            source.as_bytes(),
            name,
            &mut BTreeSet::new(),
        )
    }

    #[test]
    fn protocol_contract_requires_direct_protocol_base_and_unambiguous_members() {
        let valid = "from typing import Protocol\nclass Poster(Protocol):\n    def post(self, text: str) -> None: ...\nclass Reader(Protocol):\n    def read(self) -> list[str]: ...\nclass Port(Poster, Reader, Protocol):\n    pass\n";
        assert_eq!(contract(valid, "Port").unwrap().len(), 2);
        for invalid in [
            valid.replace("Port(Poster, Reader, Protocol)", "Port(Poster, Reader)"),
            valid.replace("from typing import Protocol", "from other import Protocol"),
            valid.replace("class Reader(Protocol)", "class Reader(Port, Protocol)"),
            valid.replace(
                "def read(self) -> list[str]",
                "def post(self, text: str) -> None",
            ),
            valid.replace("    pass\n", "    value: int\n"),
            valid.replace(
                "def read(self) -> list[str]",
                "def read(self) -> list[str, str]",
            ),
            valid.replace("def read(self) -> list[str]", "def read(self) -> Unknown"),
        ] {
            assert!(contract(&invalid, "Port").is_none(), "{invalid}");
        }
    }

    #[test]
    fn protocol_method_contract_normalizes_formatting_and_keeps_callable_shape() {
        let simple = "from typing import Protocol\nclass Port(Protocol):\n    def post(self, text: str) -> None: ...\n";
        let multiline = "from typing import Protocol\nclass Port(Protocol):\n    def post(\n        self,\n        text : str, # note\n    ) -> None: ...\n";
        assert_eq!(contract(simple, "Port"), contract(multiline, "Port"));
        for changed in [
            simple.replace("text: str", "text: bytes"),
            simple.replace("text: str", "message: str"),
            simple.replace("text: str", "text: str = 'default'"),
            simple.replace("def post", "async def post"),
            simple.replace("-> None", "-> str"),
        ] {
            assert_ne!(
                contract(simple, "Port"),
                contract(&changed, "Port"),
                "{changed}"
            );
        }
    }

    #[test]
    fn protocol_proof_node_kinds_exist_in_python_grammar() {
        for kind in [
            "function_definition",
            "class_definition",
            "import_from_statement",
            "import_statement",
            "dotted_name",
            "aliased_import",
            "assignment",
            "augmented_assignment",
            "named_expression",
            "type",
            "type_parameter",
            "generic_type",
            "binary_operator",
            "identifier",
            "none",
            "ellipsis",
            "call",
            "argument_list",
            "typed_parameter",
            "typed_default_parameter",
            "default_parameter",
            "keyword_argument",
            "list_splat",
            "dictionary_splat",
            "attribute",
            "yield",
            "comment",
            "return_statement",
            "pass_statement",
            "expression_statement",
            "string",
            "lambda",
            "list_comprehension",
            "set_comprehension",
            "dictionary_comprehension",
            "generator_expression",
            "delete_statement",
            "true",
            "false",
            "integer",
            "float",
            "module",
        ] {
            tree_sitter::Query::new(&LangId::Python.ts_language(), &format!("({kind})"))
                .unwrap_or_else(|e| panic!("{kind}: {e}"));
        }
    }
}
