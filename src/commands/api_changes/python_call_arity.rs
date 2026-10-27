//! Python の変更済み呼び出しが、変更後の関数の引数契約を満たすか確認する。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use tree_sitter::Node;

use super::ref_index::{ApiClosureCaches, parsed_ref_file, parsed_ref_file_shared};
use crate::models::reference::SymbolReference;

/// 1 回の検出の中で答えが変わらない、Python のクラス・束縛の解決結果のキャッシュ。
///
/// メソッドの閉包判定は参照 1 件ごとに受け手のクラスを解決する。キャッシュが無いと、
/// import したクラスの定義ファイルを参照ごとに読み直して parse し、呼び出し側の解析木全体も
/// 参照ごとに走査し直すため、参照数 × ファイルの大きさ (同じファイル内では参照数の 2 乗) に
/// 比例して時間がかかっていた。キーのファイルは `parsed_ref_file` のキーと同じパス文字列で、
/// node id はそのファイルのキャッシュ済み解析木に固有なのでファイルをまたいで混ざらない。
#[derive(Default)]
pub(crate) struct PythonClassCache {
    /// (ファイル, 名前) → その名前を束縛するノードの id (解析木全体)
    bindings: HashMap<(String, String), Rc<Vec<usize>>>,
    /// (ファイル, 関数ノード id, 名前) → 関数の中に引数以外の束縛があるか
    scope_bindings: HashMap<(String, usize, String), bool>,
    /// (ファイル, クラス名) → `local_class`
    local_classes: HashMap<(String, String), bool>,
    /// (ファイル, クラス名, メソッド名) → `class_has_direct_method`
    direct_methods: HashMap<(String, String, String), bool>,
    /// (呼び出し側ファイル, クラス名, メソッド名) → `imported_class`
    imported_classes: HashMap<(String, String, Option<String>), bool>,
    /// ファイル → `module_lookup_is_dynamic`
    dynamic_lookups: HashMap<String, bool>,
    /// (ファイル, クラス名) → `class_import_module`
    import_modules: HashMap<(String, String), Option<String>>,
    /// キャッシュが効いていることを確かめるための計算回数 (テスト用)。
    #[cfg(test)]
    pub(super) stats: PythonClassCacheStats,
}

#[cfg(test)]
#[derive(Default, Debug)]
pub(super) struct PythonClassCacheStats {
    pub(super) binding_scans: usize,
    pub(super) local_class_scans: usize,
    pub(super) imported_class_scans: usize,
}

/// 解析中の Python ファイル。`path` はキャッシュのキー (`parsed_ref_file` と同じ)。
#[derive(Clone, Copy)]
struct PyFile<'a> {
    path: &'a str,
    root: Node<'a>,
    source: &'a [u8],
}

#[derive(Clone)]
struct Parameter {
    name: String,
    required: bool,
    positional_only: bool,
    keyword_only: bool,
}

#[derive(Clone)]
pub(super) struct CallContract {
    parameters: Vec<Parameter>,
    varargs: bool,
    kwargs: bool,
    bound_receiver: Option<String>,
}

#[derive(Clone, Copy)]
enum MethodBinding {
    Instance,
    Class,
    Static,
}

#[derive(Clone)]
pub(super) struct MethodContract {
    full: CallContract,
    bound: Option<CallContract>,
    binding: MethodBinding,
    class_name: String,
}

fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    std::str::from_utf8(source.get(node.byte_range())?).ok()
}

fn parameter_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let name = if node.kind() == "identifier" {
        node
    } else {
        node.child_by_field_name("name")
            .or_else(|| node.named_child(0))?
    };
    (name.kind() == "identifier")
        .then(|| node_text(name, source))
        .flatten()
}

fn contains_identifier(node: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "identifier" && node_text(current, source) == Some(name) {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    false
}

fn import_binds_name(node: Node<'_>, source: &[u8], name: &str) -> bool {
    let from_import = node.kind() == "import_from_statement";
    let module = node
        .child_by_field_name("module_name")
        .map(|child| child.id());
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| Some(child.id()) != module)
        .any(|child| match child.kind() {
            "wildcard_import" => true,
            "aliased_import" => child
                .child_by_field_name("alias")
                .is_some_and(|alias| node_text(alias, source) == Some(name)),
            "dotted_name" if from_import => contains_identifier(child, source, name),
            "dotted_name" => child
                .named_child(0)
                .is_some_and(|first| node_text(first, source) == Some(name)),
            _ => false,
        })
}

/// `node` 自身が `name` を束縛するか (引数は含まない)。
fn binds_name(node: Node<'_>, source: &[u8], name: &str) -> bool {
    match node.kind() {
        "function_definition" | "class_definition" => node
            .child_by_field_name("name")
            .is_some_and(|child| contains_identifier(child, source, name)),
        "assignment" | "augmented_assignment" | "for_statement" | "type_alias_statement" => node
            .child_by_field_name("left")
            .is_some_and(|child| contains_identifier(child, source, name)),
        "named_expression" => node
            .child_by_field_name("name")
            .is_some_and(|child| contains_identifier(child, source, name)),
        "as_pattern" => u32::try_from(node.named_child_count().saturating_sub(1))
            .ok()
            .and_then(|index| node.named_child(index))
            .is_some_and(|child| contains_identifier(child, source, name)),
        "import_statement" | "import_from_statement" => import_binds_name(node, source, name),
        "global_statement" => contains_identifier(node, source, name),
        "delete_statement" | "match_statement" => contains_identifier(node, source, name),
        _ => false,
    }
}

fn has_other_binding(root: Node<'_>, function: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.id() != function.id() && binds_name(node, source, name) {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

/// ファイル全体で `name` を束縛するノードの id。(ファイル, 名前) ごとに 1 回だけ走査する。
fn root_binding_ids(file: PyFile<'_>, name: &str, caches: &mut ApiClosureCaches) -> Rc<Vec<usize>> {
    let cache = &mut caches.python_classes;
    let key = (file.path.to_owned(), name.to_owned());
    if let Some(ids) = cache.bindings.get(&key) {
        return Rc::clone(ids);
    }
    #[cfg(test)]
    {
        cache.stats.binding_scans += 1;
    }
    let mut ids = Vec::new();
    let mut stack = vec![file.root];
    while let Some(node) = stack.pop() {
        if binds_name(node, file.source, name) {
            ids.push(node.id());
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    let ids = Rc::new(ids);
    cache.bindings.insert(key, Rc::clone(&ids));
    ids
}

/// `has_other_binding(file.root, except, ..)` と同じ結果を、ファイル単位の束縛一覧から返す。
fn has_other_root_binding(
    file: PyFile<'_>,
    except: Node<'_>,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    root_binding_ids(file, name, caches)
        .iter()
        .any(|id| *id != except.id())
}

fn is_identifier_attribute_chain(mut node: Node<'_>) -> bool {
    // 属性の連鎖 (`a.b.c…`) は入力次第で長くなるため、再帰せずループで受け手側へ辿る。
    loop {
        match node.kind() {
            "identifier" => return true,
            "attribute" => {
                if !node
                    .child_by_field_name("attribute")
                    .is_some_and(|member| member.kind() == "identifier")
                {
                    return false;
                }
                match node.child_by_field_name("object") {
                    Some(object) => node = object,
                    None => return false,
                }
            }
            _ => return false,
        }
    }
}

impl CallContract {
    pub(super) fn from_function(function: Node<'_>, source: &[u8]) -> Option<Self> {
        // 任意の decorator は関数の呼出し契約を変更できる。
        if function
            .parent()
            .is_some_and(|parent| parent.kind() == "decorated_definition")
        {
            return None;
        }
        Self::from_function_undecorated(function, source)
    }

    fn from_function_undecorated(function: Node<'_>, source: &[u8]) -> Option<Self> {
        let list = function.child_by_field_name("parameters")?;
        let mut parameters: Vec<Parameter> = Vec::new();
        let mut names = HashSet::new();
        let mut positional_only = false;
        let mut keyword_only = false;
        let mut varargs = false;
        let mut kwargs = false;
        let mut seen_default_positional = false;
        let mut cursor = list.walk();
        for child in list.named_children(&mut cursor) {
            match child.kind() {
                "comment" => {}
                "positional_separator" if !positional_only && !keyword_only => {
                    positional_only = true;
                    for param in &mut parameters {
                        param.positional_only = true;
                    }
                }
                "keyword_separator" if !keyword_only => keyword_only = true,
                "list_splat_pattern" if !varargs && !kwargs => {
                    varargs = true;
                    keyword_only = true;
                }
                "dictionary_splat_pattern" if !kwargs => kwargs = true,
                "typed_parameter"
                    if child
                        .named_child(0)
                        .is_some_and(|pattern| pattern.kind() == "list_splat_pattern")
                        && !varargs
                        && !kwargs =>
                {
                    varargs = true;
                    keyword_only = true;
                }
                "typed_parameter"
                    if child
                        .named_child(0)
                        .is_some_and(|pattern| pattern.kind() == "dictionary_splat_pattern")
                        && !kwargs =>
                {
                    kwargs = true;
                }
                "identifier"
                | "typed_parameter"
                | "default_parameter"
                | "typed_default_parameter"
                    if !kwargs =>
                {
                    let name = parameter_name(child, source)?.to_owned();
                    if !names.insert(name.clone()) || parameters.len() >= 64 {
                        return None;
                    }
                    let required = !matches!(
                        child.kind(),
                        "default_parameter" | "typed_default_parameter"
                    );
                    if !keyword_only && required && seen_default_positional {
                        return None;
                    }
                    if !keyword_only && !required {
                        seen_default_positional = true;
                    }
                    parameters.push(Parameter {
                        name,
                        required,
                        positional_only: false,
                        keyword_only,
                    });
                }
                _ => return None,
            }
        }
        Some(Self {
            parameters,
            varargs,
            kwargs,
            bound_receiver: None,
        })
    }

    pub(super) fn accepts_call(&self, call: Node<'_>, source: &[u8]) -> bool {
        let Some(arguments) = call.child_by_field_name("arguments") else {
            return false;
        };
        if arguments.kind() != "argument_list" {
            return false;
        }
        let mut supplied = HashSet::new();
        let mut keyword_names = HashSet::new();
        let mut positional = 0usize;
        let mut seen_keyword = false;
        let mut cursor = arguments.walk();
        for argument in arguments.named_children(&mut cursor) {
            match argument.kind() {
                "comment" => {}
                // 展開される実引数は静的に確定できない。
                "list_splat" | "dictionary_splat" => return false,
                "keyword_argument" => {
                    seen_keyword = true;
                    let Some(name) = argument
                        .child_by_field_name("name")
                        .or_else(|| argument.named_child(0))
                        .and_then(|node| node_text(node, source))
                    else {
                        return false;
                    };
                    if !keyword_names.insert(name) {
                        return false;
                    }
                    if self.bound_receiver.as_deref() == Some(name) {
                        return false;
                    }
                    if let Some(index) = self.parameters.iter().position(|p| p.name == name) {
                        let param = &self.parameters[index];
                        if param.positional_only {
                            if !self.kwargs {
                                return false;
                            }
                        } else if !supplied.insert(index) {
                            return false;
                        }
                    } else if !self.kwargs {
                        return false;
                    }
                }
                _ => {
                    if seen_keyword {
                        return false;
                    }
                    let target = self
                        .parameters
                        .iter()
                        .enumerate()
                        .filter(|(_, param)| !param.keyword_only)
                        .nth(positional)
                        .map(|(index, _)| index);
                    if let Some(index) = target {
                        if !supplied.insert(index) {
                            return false;
                        }
                    } else if !self.varargs {
                        return false;
                    }
                    positional += 1;
                }
            }
        }
        self.parameters
            .iter()
            .enumerate()
            .all(|(index, param)| !param.required || supplied.contains(&index))
    }
}

fn simple_class_base(root: Node<'_>, class: Node<'_>, source: &[u8]) -> bool {
    let Some(bases) = class.child_by_field_name("superclasses") else {
        return true;
    };
    match node_text(bases, source) {
        Some("(object)") => !has_other_binding(root, class, source, "object"),
        Some("(Protocol)") => {
            let mut cursor = root.walk();
            root.named_children(&mut cursor).any(|node| {
                node.kind() == "import_from_statement"
                    && node_text(node, source) == Some("from typing import Protocol")
                    && !has_other_binding(root, node, source, "Protocol")
            })
        }
        _ => false,
    }
}

fn class_has_dynamic_hooks(class: Node<'_>, source: &[u8]) -> bool {
    let Some(body) = class.child_by_field_name("body") else {
        return true;
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor).any(|child| {
        let function = if child.kind() == "decorated_definition" {
            child.child_by_field_name("definition")
        } else {
            Some(child)
        };
        function.is_some_and(|node| {
            node.kind() == "function_definition"
                && node.child_by_field_name("name").is_some_and(|n| {
                    matches!(node_text(n, source), Some("__new__" | "__getattribute__"))
                })
        })
    })
}

fn method_contract_for_target(
    dir: &str,
    target_path: &str,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> Option<MethodContract> {
    let (class_name, method_name) = name.split_once('.')?;
    if class_name.is_empty()
        || method_name.is_empty()
        || method_name.contains('.')
        || method_name.starts_with("__")
    {
        return None;
    }
    let target = parsed_ref_file(dir, target_path, caches)?;
    let root = target.tree.root_node();
    if target.lang_id != crate::language::LangId::Python || root.has_error() {
        return None;
    }
    let method = super::python_signature::find_python_function_by_name(root, &target.source, name)?;
    let body = method
        .parent()
        .filter(|parent| parent.kind() == "block")
        .or_else(|| {
            method
                .parent()
                .filter(|parent| parent.kind() == "decorated_definition")?
                .parent()
        })?;
    let class = body
        .parent()
        .filter(|parent| parent.kind() == "class_definition")?;
    if class
        .parent()
        .is_none_or(|parent| parent.kind() != "module")
        || has_other_binding(root, class, &target.source, class_name)
    {
        return None;
    }
    // 外部基底クラスのフックや属性解決のカスタマイズは、明示的な call だけでは追えない。
    if !simple_class_base(root, class, &target.source)
        || class_has_dynamic_hooks(class, &target.source)
    {
        return None;
    }
    if has_other_binding(body, method, &target.source, method_name) {
        return None;
    }
    let binding = if let Some(decorated) = method
        .parent()
        .filter(|parent| parent.kind() == "decorated_definition")
    {
        let mut cursor = decorated.walk();
        let decorators: Vec<_> = decorated
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "decorator")
            .collect();
        if decorators.len() != 1 {
            return None;
        }
        let decorator = decorators[0].named_child(0)?;
        let name = node_text(decorator, &target.source)?;
        if has_other_binding(root, method, &target.source, name) {
            return None;
        }
        match name {
            "classmethod" => MethodBinding::Class,
            "staticmethod" => MethodBinding::Static,
            _ => return None,
        }
    } else {
        MethodBinding::Instance
    };
    let full = CallContract::from_function_undecorated(method, &target.source)?;
    let bound = if matches!(binding, MethodBinding::Static) {
        None
    } else {
        if full
            .parameters
            .first()
            .is_none_or(|p| !p.required || p.keyword_only)
        {
            return None;
        }
        let mut bound = full.clone();
        let receiver = bound.parameters.remove(0);
        if !receiver.positional_only {
            bound.bound_receiver = Some(receiver.name);
        }
        Some(bound)
    };
    Some(MethodContract {
        full,
        bound,
        binding,
        class_name: class_name.to_owned(),
    })
}

#[derive(Clone, Copy)]
enum ReceiverKind {
    Instance,
    Class,
}

struct ResolvedReceiver {
    kind: ReceiverKind,
    owner: String,
    exact_class: bool,
}

fn local_class(file: PyFile<'_>, name: &str, caches: &mut ApiClosureCaches) -> bool {
    let key = (file.path.to_owned(), name.to_owned());
    if let Some(found) = caches.python_classes.local_classes.get(&key) {
        return *found;
    }
    #[cfg(test)]
    {
        caches.python_classes.stats.local_class_scans += 1;
    }
    let found = local_class_uncached(file, name, caches);
    caches.python_classes.local_classes.insert(key, found);
    found
}

fn local_class_uncached(file: PyFile<'_>, name: &str, caches: &mut ApiClosureCaches) -> bool {
    let PyFile { root, source, .. } = file;
    let mut cursor = root.walk();
    let mut matches = root.named_children(&mut cursor).filter(|node| {
        node.kind() == "class_definition"
            && node
                .child_by_field_name("name")
                .is_some_and(|n| node_text(n, source) == Some(name))
    });
    let Some(class) = matches.next() else {
        return false;
    };
    matches.next().is_none()
        && simple_class_base(root, class, source)
        && !has_other_root_binding(file, class, name, caches)
        && !class_has_dynamic_hooks(class, source)
}

fn class_has_direct_method(
    file: PyFile<'_>,
    class_name: &str,
    method_name: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    let key = (
        file.path.to_owned(),
        class_name.to_owned(),
        method_name.to_owned(),
    );
    if let Some(found) = caches.python_classes.direct_methods.get(&key) {
        return *found;
    }
    let PyFile { root, source, .. } = file;
    let qualified = format!("{class_name}.{method_name}");
    let found = super::python_signature::find_python_function_by_name(root, source, &qualified)
        .is_some_and(|method| {
            method
                .parent()
                .filter(|parent| parent.kind() == "block")
                .is_some_and(|body| {
                    body.parent()
                        .is_some_and(|parent| parent.kind() == "class_definition")
                        && !has_other_binding(body, method, source, method_name)
                })
        });
    caches.python_classes.direct_methods.insert(key, found);
    found
}

fn imported_class(
    file: PyFile<'_>,
    dir: &str,
    name: &str,
    method: Option<&str>,
    caches: &mut ApiClosureCaches,
) -> bool {
    let key = (
        file.path.to_owned(),
        name.to_owned(),
        method.map(str::to_owned),
    );
    if let Some(found) = caches.python_classes.imported_classes.get(&key) {
        return *found;
    }
    #[cfg(test)]
    {
        caches.python_classes.stats.imported_class_scans += 1;
    }
    let found = imported_class_uncached(file, dir, name, method, caches);
    caches.python_classes.imported_classes.insert(key, found);
    found
}

fn imported_class_uncached(
    file: PyFile<'_>,
    dir: &str,
    name: &str,
    method: Option<&str>,
    caches: &mut ApiClosureCaches,
) -> bool {
    let PyFile { root, source, .. } = file;
    let mut cursor = root.walk();
    let statements: Vec<_> = root.named_children(&mut cursor).collect();
    statements.into_iter().any(|node| {
        if node.kind() != "import_from_statement" {
            return false;
        }
        let Some(module_node) = node.child_by_field_name("module_name") else {
            return false;
        };
        if module_node.kind() != "dotted_name" {
            return false;
        }
        let Some(module) = node_text(module_node, source) else {
            return false;
        };
        let mut imports = node.walk();
        if !node.named_children(&mut imports).any(|child| {
            child.id() != module_node.id()
                && child.kind() == "dotted_name"
                && node_text(child, source) == Some(name)
        }) || module.is_empty()
            || !module.split('.').all(|part| {
                !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        {
            return false;
        }
        if has_other_root_binding(file, node, name, caches) {
            return false;
        }
        let relative = module.replace('.', "/");
        let root_path = std::path::Path::new(dir);
        let candidates = [
            root_path.join(format!("{relative}.py")),
            root_path.join(format!("{relative}/__init__.py")),
            root_path.join(format!("src/{relative}.py")),
            root_path.join(format!("src/{relative}/__init__.py")),
        ];
        let mut existing = candidates
            .iter()
            .filter(|path| path.try_exists().unwrap_or(true));
        let Some(path) = existing.next() else {
            return false;
        };
        if existing.next().is_some() {
            return false;
        }
        let Ok(canonical_root) = std::fs::canonicalize(root_path) else {
            return false;
        };
        let Ok(canonical_path) = std::fs::canonicalize(path) else {
            return false;
        };
        if !canonical_path.starts_with(&canonical_root)
            || canonical_path
                .with_extension("pyi")
                .try_exists()
                .unwrap_or(true)
        {
            return false;
        }
        // 定義ファイルは他の経路と同じ上限付きの読み込み (`parser::read_file`) とキャッシュを通す。
        let Ok(relative_path) = canonical_path.strip_prefix(&canonical_root) else {
            return false;
        };
        let module_path =
            crate::git_support::normalize_workspace_separators(&relative_path.to_string_lossy());
        let Some(module_file) = parsed_ref_file_shared(dir, &module_path, caches) else {
            return false;
        };
        if module_file.source.len() > 1_000_000
            || module_file.lang_id != crate::language::LangId::Python
            || module_file.tree.root_node().has_error()
        {
            return false;
        }
        let module = PyFile {
            path: &module_path,
            root: module_file.tree.root_node(),
            source: &module_file.source,
        };
        local_class(module, name, caches)
            && method.is_none_or(|m| class_has_direct_method(module, name, m, caches))
    })
}

fn known_class(file: PyFile<'_>, dir: &str, name: &str, caches: &mut ApiClosureCaches) -> bool {
    local_class(file, name, caches) || imported_class(file, dir, name, None, caches)
}

fn known_other_method(
    file: PyFile<'_>,
    dir: &str,
    class_name: &str,
    method: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    (local_class(file, class_name, caches)
        && class_has_direct_method(file, class_name, method, caches))
        || imported_class(file, dir, class_name, Some(method), caches)
}

fn enclosing_function(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "function_definition" {
            return Some(parent);
        }
        current = parent.parent();
    }
    None
}

fn parameter_named(function: Node<'_>, source: &[u8], name: &str) -> bool {
    let Some(parameters) = function.child_by_field_name("parameters") else {
        return false;
    };
    let mut cursor = parameters.walk();
    parameters
        .named_children(&mut cursor)
        .any(|parameter| parameter_name(parameter, source) == Some(name))
}

fn parameter_in_enclosing_scopes(node: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "function_definition" && parameter_named(parent, source, name) {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn local_constructor_binding(
    receiver: Node<'_>,
    file: PyFile<'_>,
    dir: &str,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> Option<String> {
    let source = file.source;
    let function = enclosing_function(receiver)?;
    if parameter_named(function, source, name) {
        return None;
    }
    let body = function.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let statements: Vec<_> = body.named_children(&mut cursor).collect();
    statements.into_iter().find_map(|statement| {
        let assignment = if statement.kind() == "expression_statement" {
            statement.named_child(0)
        } else {
            Some(statement)
        };
        let assignment = assignment?;
        if assignment.kind() != "assignment"
            || assignment.end_byte() > receiver.start_byte()
            || assignment.child_by_field_name("left").is_none_or(|left| {
                left.kind() != "identifier" || node_text(left, source) != Some(name)
            })
        {
            return None;
        }
        let value = assignment.child_by_field_name("right")?;
        let constructor = value.child_by_field_name("function")?;
        if value.kind() != "call"
            || constructor.kind() != "identifier"
            || has_other_root_binding(file, assignment, name, caches)
        {
            return None;
        }
        let class = node_text(constructor, source)?;
        known_class(file, dir, class, caches).then(|| class.to_owned())
    })
}

/// `has_other_binding(function, function, ..)` (関数の中の引数以外の束縛) を関数ごとに 1 回だけ計算する。
fn function_rebinds_name(
    file: PyFile<'_>,
    function: Node<'_>,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    let key = (file.path.to_owned(), function.id(), name.to_owned());
    if let Some(found) = caches.python_classes.scope_bindings.get(&key) {
        return *found;
    }
    let found = has_other_binding(function, function, file.source, name);
    caches.python_classes.scope_bindings.insert(key, found);
    found
}

fn parameter_receiver_kind(
    receiver: Node<'_>,
    file: PyFile<'_>,
    dir: &str,
    caches: &mut ApiClosureCaches,
) -> Option<ResolvedReceiver> {
    let source = file.source;
    let name = node_text(receiver, source)?;
    let function = enclosing_function(receiver)?;
    if function_rebinds_name(file, function, name, caches) {
        return None;
    }
    let parameters = function.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let params: Vec<_> = parameters
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect();
    for (index, parameter) in params.iter().enumerate() {
        if parameter_name(*parameter, source) != Some(name) {
            continue;
        }
        if index == 0
            && function.parent().is_some_and(|p| p.kind() == "block")
            && function
                .parent()
                .and_then(|p| p.parent())
                .is_some_and(|p| p.kind() == "class_definition")
        {
            let class = function.parent()?.parent()?;
            let owner = class
                .child_by_field_name("name")
                .and_then(|n| node_text(n, source))?;
            return Some(ResolvedReceiver {
                kind: ReceiverKind::Instance,
                owner: owner.to_owned(),
                exact_class: false,
            });
        }
        if parameter.kind() == "typed_parameter" {
            let annotation = parameter.child_by_field_name("type")?.named_child(0)?;
            if annotation.kind() == "identifier"
                && known_class(file, dir, node_text(annotation, source)?, caches)
            {
                return Some(ResolvedReceiver {
                    kind: ReceiverKind::Instance,
                    owner: node_text(annotation, source)?.to_owned(),
                    exact_class: false,
                });
            }
        }
    }
    None
}

fn receiver_kind(
    receiver: Node<'_>,
    file: PyFile<'_>,
    dir: &str,
    caches: &mut ApiClosureCaches,
) -> Option<ResolvedReceiver> {
    let source = file.source;
    let mut current = receiver.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "lambda"
                | "list_comprehension"
                | "set_comprehension"
                | "dictionary_comprehension"
                | "generator_expression"
        ) {
            return None;
        }
        current = parent.parent();
    }
    match receiver.kind() {
        "identifier" => {
            let name = node_text(receiver, source)?;
            if known_class(file, dir, name, caches)
                && !parameter_in_enclosing_scopes(receiver, source, name)
            {
                return Some(ResolvedReceiver {
                    kind: ReceiverKind::Class,
                    owner: name.to_owned(),
                    exact_class: true,
                });
            }
            if let Some(resolved) = parameter_receiver_kind(receiver, file, dir, caches) {
                return Some(resolved);
            }
            local_constructor_binding(receiver, file, dir, name, caches).map(|owner| {
                ResolvedReceiver {
                    kind: ReceiverKind::Instance,
                    owner,
                    exact_class: true,
                }
            })
        }
        "call" => {
            let function = receiver.child_by_field_name("function")?;
            if function.kind() == "identifier"
                && known_class(file, dir, node_text(function, source)?, caches)
            {
                Some(ResolvedReceiver {
                    kind: ReceiverKind::Instance,
                    owner: node_text(function, source)?.to_owned(),
                    exact_class: true,
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// メソッド呼び出しの実引数と暗黙の receiver を変更後契約に照合する。
/// 束縛形を証明できない参照は blocking 側に残す。
pub(super) enum MethodCallMatch {
    Target,
    Other,
    Unknown,
}

pub(super) fn definition_belongs_to_method(
    dir: &str,
    name: &str,
    reference: &SymbolReference,
    caches: &mut ApiClosureCaches,
) -> bool {
    let Some(file) = parsed_ref_file(dir, &reference.path, caches) else {
        return false;
    };
    if file.lang_id != crate::language::LangId::Python || file.tree.root_node().has_error() {
        return false;
    }
    let Some(function) = super::python_signature::find_python_function_by_name(
        file.tree.root_node(),
        &file.source,
        name,
    ) else {
        return false;
    };
    function.child_by_field_name("name").is_some_and(|node| {
        node.start_position()
            == tree_sitter::Point {
                row: reference.line,
                column: reference.column,
            }
    })
}

fn module_lookup_is_dynamic(file: PyFile<'_>, caches: &mut ApiClosureCaches) -> bool {
    if let Some(found) = caches.python_classes.dynamic_lookups.get(file.path) {
        return *found;
    }
    let found = module_lookup_is_dynamic_uncached(file.root, file.source);
    caches
        .python_classes
        .dynamic_lookups
        .insert(file.path.to_owned(), found);
    found
}

fn module_lookup_is_dynamic_uncached(root: Node<'_>, source: &[u8]) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if (node.kind() == "function_definition"
            && node
                .child_by_field_name("name")
                .and_then(|n| node_text(n, source))
                == Some("__getattr__"))
            || (node.kind() == "attribute"
                && node
                    .child_by_field_name("attribute")
                    .and_then(|n| node_text(n, source))
                    .is_some_and(|n| matches!(n, "path" | "modules"))
                && node
                    .child_by_field_name("object")
                    .and_then(|n| node_text(n, source))
                    == Some("sys"))
        {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn class_import_module(
    file: PyFile<'_>,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> Option<String> {
    let key = (file.path.to_owned(), name.to_owned());
    if let Some(found) = caches.python_classes.import_modules.get(&key) {
        return found.clone();
    }
    let found = class_import_module_uncached(file, name, caches);
    caches
        .python_classes
        .import_modules
        .insert(key, found.clone());
    found
}

fn class_import_module_uncached(
    file: PyFile<'_>,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> Option<String> {
    let PyFile { root, source, .. } = file;
    let mut found = None;
    let mut cursor = root.walk();
    let statements: Vec<_> = root.named_children(&mut cursor).collect();
    for node in statements {
        if node.kind() != "import_from_statement" {
            continue;
        }
        let module = node.child_by_field_name("module_name")?;
        if module.kind() != "dotted_name" {
            continue;
        }
        let mut children = node.walk();
        if node.named_children(&mut children).any(|n| {
            n.id() != module.id() && n.kind() == "dotted_name" && node_text(n, source) == Some(name)
        }) {
            if found.is_some() || has_other_root_binding(file, node, name, caches) {
                return None;
            }
            found = Some(node_text(module, source)?.to_owned());
        }
    }
    found
}

/// 他 owner の無変更定義を除外する追加証拠。曖昧な receiver は除外しない。
pub(super) fn method_reference_owner(
    dir: &str,
    target_path: &str,
    name: &str,
    reference: &SymbolReference,
    caches: &mut ApiClosureCaches,
) -> MethodCallMatch {
    let Some((target_class, method_name)) = name.split_once('.') else {
        return MethodCallMatch::Unknown;
    };
    let Some(caller) = parsed_ref_file_shared(dir, &reference.path, caches) else {
        return MethodCallMatch::Unknown;
    };
    let root = caller.tree.root_node();
    let caller_file = PyFile {
        path: &reference.path,
        root,
        source: &caller.source,
    };
    if caller.lang_id != crate::language::LangId::Python
        || root.has_error()
        || module_lookup_is_dynamic(caller_file, caches)
    {
        return MethodCallMatch::Unknown;
    }
    let point = tree_sitter::Point {
        row: reference.line,
        column: reference.column,
    };
    let Some(member) = root.descendant_for_point_range(point, point) else {
        return MethodCallMatch::Unknown;
    };
    if member.kind() != "identifier" || node_text(member, &caller.source) != Some(method_name) {
        return MethodCallMatch::Unknown;
    }
    let Some(attribute) = member.parent().filter(|n| {
        n.kind() == "attribute"
            && n.child_by_field_name("attribute")
                .is_some_and(|a| a.id() == member.id())
    }) else {
        return MethodCallMatch::Unknown;
    };
    if attribute.parent().is_none_or(|n| {
        n.kind() != "call"
            || n.child_by_field_name("function")
                .is_none_or(|f| f.id() != attribute.id())
    }) {
        return MethodCallMatch::Unknown;
    }
    let Some(receiver) = attribute
        .child_by_field_name("object")
        .and_then(|r| receiver_kind(r, caller_file, dir, caches))
    else {
        return MethodCallMatch::Unknown;
    };
    if !receiver.exact_class {
        return MethodCallMatch::Unknown;
    }
    let owner = receiver.owner;
    let local = local_class(caller_file, &owner, caches);
    let module = if local {
        None
    } else {
        class_import_module(caller_file, &owner, caches)
    };
    let origin = if local {
        Some(reference.path.clone())
    } else {
        let Some(module) = module else {
            return MethodCallMatch::Unknown;
        };
        caches
            .python_module_paths
            .entry(module.clone())
            .or_insert_with(|| super::python_module_path::resolve(dir, &module))
            .clone()
    };
    let Some(origin) = origin else {
        return MethodCallMatch::Unknown;
    };
    let canonical = |path: &str| {
        let path = std::path::Path::new(path);
        std::fs::canonicalize(if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::path::Path::new(dir).join(path)
        })
        .ok()
    };
    let (Some(origin_path), Some(target)) = (canonical(&origin), canonical(target_path)) else {
        return MethodCallMatch::Unknown;
    };
    let Some(file) = parsed_ref_file_shared(dir, &origin, caches) else {
        return MethodCallMatch::Unknown;
    };
    let root = file.tree.root_node();
    let origin_file = PyFile {
        path: &origin,
        root,
        source: &file.source,
    };
    if root.has_error()
        || module_lookup_is_dynamic(origin_file, caches)
        || !local_class(origin_file, &owner, caches)
    {
        return MethodCallMatch::Unknown;
    }
    if origin_path == target && owner == target_class {
        return MethodCallMatch::Target;
    }
    let qualified = format!("{owner}.{method_name}");
    let ordinary =
        super::python_signature::find_python_function_by_name(root, &file.source, &qualified)
            .and_then(|m| m.parent())
            .filter(|n| n.kind() == "block")
            .and_then(|n| n.parent())
            .is_some_and(|class| {
                class
                    .child_by_field_name("superclasses")
                    .is_none_or(|n| node_text(n, &file.source) == Some("(object)"))
            });
    if ordinary && class_has_direct_method(origin_file, &owner, method_name, caches) {
        MethodCallMatch::Other
    } else {
        MethodCallMatch::Unknown
    }
}

pub(super) fn changed_method_call_matches_signature(
    dir: &str,
    target_path: &str,
    name: &str,
    reference: &SymbolReference,
    caches: &mut ApiClosureCaches,
) -> MethodCallMatch {
    let key = (target_path.to_owned(), name.to_owned());
    let contract = if let Some(cached) = caches.python_method_contracts.get(&key) {
        cached.clone()
    } else {
        let loaded = method_contract_for_target(dir, target_path, name, caches);
        caches.python_method_contracts.insert(key, loaded.clone());
        loaded
    };
    let Some(contract) = contract else {
        return MethodCallMatch::Unknown;
    };
    let Some(caller) = parsed_ref_file_shared(dir, &reference.path, caches) else {
        return MethodCallMatch::Unknown;
    };
    let root = caller.tree.root_node();
    let caller_file = PyFile {
        path: &reference.path,
        root,
        source: &caller.source,
    };
    if caller.lang_id != crate::language::LangId::Python || root.has_error() {
        return MethodCallMatch::Unknown;
    }
    let point = tree_sitter::Point {
        row: reference.line,
        column: reference.column,
    };
    let Some(member) = root.descendant_for_point_range(point, point) else {
        return MethodCallMatch::Unknown;
    };
    if member.kind() != "identifier"
        || node_text(member, &caller.source) != name.split_once('.').map(|(_, method)| method)
    {
        return MethodCallMatch::Unknown;
    }
    let Some(attribute) = member
        .parent()
        .filter(|parent| parent.kind() == "attribute")
    else {
        return MethodCallMatch::Unknown;
    };
    if attribute
        .child_by_field_name("attribute")
        .map(|node| node.id())
        != Some(member.id())
    {
        return MethodCallMatch::Unknown;
    }
    let Some(call) = attribute.parent().filter(|parent| parent.kind() == "call") else {
        return MethodCallMatch::Unknown;
    };
    if call.child_by_field_name("function").map(|node| node.id()) != Some(attribute.id()) {
        return MethodCallMatch::Unknown;
    }
    let receiver = attribute.child_by_field_name("object");
    let resolved = receiver.and_then(|node| receiver_kind(node, caller_file, dir, caches));
    let method_name = name
        .split_once('.')
        .map(|(_, method)| method)
        .unwrap_or(name);
    if let Some(other) = &resolved
        && other.owner != contract.class_name
        && other.exact_class
        && known_other_method(caller_file, dir, &other.owner, method_name, caches)
    {
        return MethodCallMatch::Other;
    }
    let selected = match contract.binding {
        MethodBinding::Static => &contract.full,
        MethodBinding::Class => {
            let Some(bound) = contract.bound.as_ref() else {
                return MethodCallMatch::Unknown;
            };
            bound
        }
        MethodBinding::Instance => match resolved {
            Some(ResolvedReceiver {
                kind: ReceiverKind::Instance,
                ..
            }) => {
                let Some(bound) = contract.bound.as_ref() else {
                    return MethodCallMatch::Unknown;
                };
                bound
            }
            Some(ResolvedReceiver {
                kind: ReceiverKind::Class,
                owner,
                ..
            }) if owner == contract.class_name => &contract.full,
            _ => return MethodCallMatch::Unknown,
        },
    };
    if selected.accepts_call(call, &caller.source) {
        MethodCallMatch::Target
    } else {
        MethodCallMatch::Unknown
    }
}

fn contract_for_target(
    dir: &str,
    target_path: &str,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> Option<CallContract> {
    let target = parsed_ref_file(dir, target_path, caches)?;
    if target.lang_id != crate::language::LangId::Python || target.tree.root_node().has_error() {
        return None;
    }
    let function = super::python_signature::find_python_function_by_name(
        target.tree.root_node(),
        &target.source,
        name,
    )?;
    if has_other_binding(target.tree.root_node(), function, &target.source, name) {
        return None;
    }
    CallContract::from_function(function, &target.source)
}

/// direct call のみ対象とする。値参照・decorator・解析エラー・展開引数は証明しない。
pub(super) fn changed_top_level_call_matches_signature(
    dir: &str,
    target_path: &str,
    name: &str,
    reference: &SymbolReference,
    caches: &mut ApiClosureCaches,
) -> bool {
    let key = (target_path.to_owned(), name.to_owned());
    let contract = if let Some(cached) = caches.python_call_contracts.get(&key) {
        cached.clone()
    } else {
        let loaded = contract_for_target(dir, target_path, name, caches);
        caches.python_call_contracts.insert(key, loaded.clone());
        loaded
    };
    let Some(contract) = contract else {
        return false;
    };
    let Some(caller) = parsed_ref_file(dir, &reference.path, caches) else {
        return false;
    };
    if caller.lang_id != crate::language::LangId::Python || caller.tree.root_node().has_error() {
        return false;
    }
    let point = tree_sitter::Point {
        row: reference.line,
        column: reference.column,
    };
    let Some(callee) = caller
        .tree
        .root_node()
        .descendant_for_point_range(point, point)
    else {
        return false;
    };
    if callee.kind() != "identifier" || node_text(callee, &caller.source) != Some(name) {
        return false;
    }
    let function = match callee.parent() {
        Some(call) if call.kind() == "call" => callee,
        Some(attribute)
            if attribute.kind() == "attribute"
                && attribute
                    .child_by_field_name("attribute")
                    .is_some_and(|field| field.id() == callee.id())
                && attribute
                    .child_by_field_name("object")
                    .is_some_and(is_identifier_attribute_chain) =>
        {
            attribute
        }
        _ => return false,
    };
    let Some(call) = function.parent().filter(|parent| parent.kind() == "call") else {
        return false;
    };
    if call.child_by_field_name("function").map(|node| node.id()) != Some(function.id()) {
        return false;
    }
    contract.accepts_call(call, &caller.source)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// import したクラスのメソッド呼び出しが多数あっても、クラスの定義ファイルの解析と
    /// 呼び出し側の解析木全体の走査は (ファイル, 名前) ごとに 1 回だけ行う。参照ごとに定義ファイルを
    /// 読み直していたため、参照数 × モジュールの大きさに比例して Stop hook の時間制限を超えていた。
    /// 判定結果は従来どおり (別クラスの同名メソッドは Other、対象クラスは Target)。
    #[test]
    fn imported_class_resolution_is_computed_once_per_file_and_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("pkg")).expect("mkdir");
        std::fs::write(root.join("pkg/__init__.py"), "").expect("write");
        std::fs::write(
            root.join("pkg/store.py"),
            "class Store:\n    def save(self, a, b):\n        return a + b\n",
        )
        .expect("write");
        let mut models = String::from("class Cache:\n    def save(self, a):\n        return a\n");
        for i in 0..200 {
            models.push_str(&format!("\ndef helper_{i}(x):\n    return x + {i}\n"));
        }
        std::fs::write(root.join("models.py"), models).expect("write");
        let calls = 40;
        let mut caller = String::from(
            "from models import Cache\nfrom pkg.store import Store\n\ndef main():\n    Store().save(1, 2)\n",
        );
        for j in 0..calls {
            caller.push_str(&format!("    Cache().save({j})\n"));
        }
        std::fs::write(root.join("use.py"), &caller).expect("write");
        let dir_str = root.to_str().expect("utf-8");
        let mut caches = ApiClosureCaches::default();
        let reference = |line: usize, column: usize| SymbolReference {
            path: "use.py".to_string(),
            line,
            column,
            context: None,
            kind: Some(crate::models::reference::RefKind::Reference),
            confidence: None,
        };
        // `Store().save(1, 2)` (4 行目) は対象メソッドの呼び出し
        assert!(matches!(
            changed_method_call_matches_signature(
                dir_str,
                "pkg/store.py",
                "Store.save",
                &reference(4, 12),
                &mut caches,
            ),
            MethodCallMatch::Target
        ));
        // `Cache().save(j)` は import した別クラスの同名メソッド
        for j in 0..calls {
            assert!(
                matches!(
                    changed_method_call_matches_signature(
                        dir_str,
                        "pkg/store.py",
                        "Store.save",
                        &reference(5 + j, 12),
                        &mut caches,
                    ),
                    MethodCallMatch::Other
                ),
                "line {}",
                5 + j
            );
        }
        let stats = &caches.python_classes.stats;
        assert!(
            stats.imported_class_scans <= 3
                && stats.local_class_scans <= 4
                && stats.binding_scans <= 6,
            "参照数 ({calls}) に比例して解析し直してはならない: {stats:?}"
        );
    }

    #[test]
    fn python_top_level_binding_scan_distinguishes_import_sources_and_aliases() {
        for (label, suffix, expected) in [
            ("no later binding", "", false),
            ("value use is not a binding", "handler = render\n", false),
            ("assignment", "render = legacy\n", true),
            (
                "named expression",
                "if (render := legacy):\n    pass\n",
                true,
            ),
            (
                "exception alias",
                "try:\n    pass\nexcept Error as render:\n    pass\n",
                true,
            ),
            ("type alias", "type render = str\n", true),
            (
                "import module named like function",
                "from .render import helpers\n",
                false,
            ),
            ("import qualified module", "import pkg.render\n", false),
            (
                "renamed import does not bind original",
                "from x import render as other\n",
                false,
            ),
            ("import alias", "from x import fast as render\n", true),
            (
                "qualified import alias",
                "import pkg.fast as render\n",
                true,
            ),
            ("wildcard import", "from x import *\n", true),
        ] {
            let source = format!("def render(name, color):\n    return name + color\n{suffix}");
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_python::LANGUAGE.into())
                .expect("Python grammar");
            let tree = parser.parse(&source, None).expect("Python parse");
            assert!(!tree.root_node().has_error(), "{label}: {source}");
            let function = super::super::python_signature::find_python_function_by_name(
                tree.root_node(),
                source.as_bytes(),
                "render",
            )
            .expect("function");
            assert_eq!(
                has_other_binding(tree.root_node(), function, source.as_bytes(), "render"),
                expected,
                "{label}: {source}"
            );
        }
    }

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn is_identifier_attribute_chain_recursive(node: Node<'_>) -> bool {
        match node.kind() {
            "identifier" => true,
            "attribute" => {
                node.child_by_field_name("attribute")
                    .is_some_and(|member| member.kind() == "identifier")
                    && node
                        .child_by_field_name("object")
                        .is_some_and(is_identifier_attribute_chain_recursive)
            }
            _ => false,
        }
    }

    /// ループ版が、浅い入力のすべての部分木で再帰版と同じ判定を返す。
    #[test]
    fn is_identifier_attribute_chain_matches_recursive_version() {
        let source = "a.b.c(1)\nf()(x).y.z\nd[0].e\n(g).h\npkg.mod.Class().method\nx = q.r\n";
        let tree =
            crate::engine::parser::parse_source(source.as_bytes(), crate::language::LangId::Python)
                .unwrap();
        let mut checked = 0;
        let mut accepted = 0;
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            tree.root_node(),
            crate::engine::tree_walk::Children::All,
            |node, _| {
                let expected = is_identifier_attribute_chain_recursive(node);
                assert_eq!(
                    is_identifier_attribute_chain(node),
                    expected,
                    "{}",
                    node.to_sexp()
                );
                checked += 1;
                accepted += usize::from(expected && node.kind() == "attribute");
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        assert!(checked > 30);
        assert!(accepted >= 3, "属性の連鎖を受理する場合も比べる");
    }
}
