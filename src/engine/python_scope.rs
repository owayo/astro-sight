//! impact 用の Python 関数ローカル束縛の証明。refs の出現一覧は変更しない。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::lexical_binding::LexicalBinding;

pub(crate) mod namespace;

const OPAQUE_SCOPES: &[&str] = &[
    "lambda",
    "list_comprehension",
    "set_comprehension",
    "dictionary_comprehension",
    "generator_expression",
];

pub(super) fn is_comprehension(kind: &str) -> bool {
    kind != "lambda" && OPAQUE_SCOPES.contains(&kind)
}
const TARGET_WRAPPERS: &[&str] = &[
    "pattern_list",
    "tuple_pattern",
    "list_pattern",
    "tuple",
    "list",
    "parenthesized_expression",
    "list_splat_pattern",
    "dictionary_splat_pattern",
    "as_pattern_target",
];

#[derive(Default)]
struct Bindings {
    local: HashSet<String>,
    blocked: HashSet<String>,
    namespace_values: HashSet<String>,
    ambiguous: bool,
}

pub(crate) struct PythonScopeIndex {
    scopes: HashMap<usize, Bindings>,
    parameter_positions: HashSet<usize>,
}

impl PythonScopeIndex {
    /// 部分木で呼ばれても、global/nonlocal を見落とさないよう木全体から構築する。
    pub(crate) fn build(mut root: Node<'_>, source: &[u8]) -> Option<Self> {
        while let Some(parent) = root.parent() {
            root = parent;
        }
        if root.has_error() {
            return None;
        }
        let mut functions = Vec::new();
        let mut has_namespace_access = false;
        // 前順の走査で開いている class の終わりの位置。関数が class の中かを、祖先をたどらずに知る。
        let mut classes: Vec<usize> = Vec::new();
        let mut cursor = root.walk();
        'scan: loop {
            let node = cursor.node();
            // Python は識別子を NFKC で正規化するので (`global ｔｏｔａｌ` は `global total`)、
            // 綴りで束縛を照合できない識別子があればファイル全体を証明しない。
            if node.kind() == "identifier" {
                let name = node.utf8_text(source).ok()?;
                if !name.is_ascii() && !unicode_normalization::is_nfkc(name) {
                    return None;
                }
                if !has_namespace_access {
                    has_namespace_access =
                        namespace::dynamic_name(name) || namespace::reflective_name(name);
                }
            }
            // walrus は内包表記から外側へ束縛が漏れる。型パラメータも独自の scope を持つ。
            // これらの名前解決を証明できるまではファイル全体で従来の判定を保つ。
            let is_type_parameter_declaration = node.kind() == "type_parameter"
                && node.parent().is_some_and(|parent| {
                    matches!(parent.kind(), "function_definition" | "class_definition")
                        && parent
                            .child_by_field_name("type_parameters")
                            .is_some_and(|parameters| parameters.id() == node.id())
                });
            if node.is_missing()
                || matches!(node.kind(), "named_expression" | "type_alias_statement")
                || is_type_parameter_declaration
            {
                return None;
            }
            while classes.last().is_some_and(|&end| end <= node.start_byte()) {
                classes.pop();
            }
            if node.kind() == "class_definition" {
                classes.push(node.end_byte());
            }
            if node.kind() == "function_definition" {
                functions.push((node, !classes.is_empty()));
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    break 'scan;
                }
            }
        }
        let namespace_targets = if has_namespace_access && !functions.is_empty() {
            namespace::binding_targets(root, source)
        } else {
            HashSet::new()
        };
        let mut scopes = HashMap::new();
        let mut parameter_positions = HashSet::new();
        for (function, in_class) in functions {
            let bindings = collect_bindings(
                function,
                source,
                &mut parameter_positions,
                in_class,
                &namespace_targets,
            );
            scopes.insert(function.id(), bindings);
        }
        Some(Self {
            scopes,
            parameter_positions,
        })
    }

    pub(crate) fn resolve(&self, node: Node<'_>, source: &[u8]) -> LexicalBinding {
        if node.kind() != "identifier" {
            return LexicalBinding::Unknown;
        }
        // 型付き/default/splat 引数の宣言名も、外側で評価される既定値・型とは区別する。
        if self.parameter_positions.contains(&node.id()) {
            return LexicalBinding::FunctionLocal;
        }
        let Ok(name) = node.utf8_text(source) else {
            return LexicalBinding::Unknown;
        };
        let Some(parent) = node.parent() else {
            return LexicalBinding::Unknown;
        };
        if (parent.kind() == "attribute" && field_contains(parent, "attribute", node))
            || (parent.kind() == "keyword_argument" && field_contains(parent, "name", node))
        {
            return LexicalBinding::Unknown;
        }
        let mut child = node;
        let mut crossed_function_body = false;
        let mut within_target = true;
        let mut writes_local = false;
        while let Some(parent) = child.parent() {
            if within_target && !TARGET_WRAPPERS.contains(&parent.kind()) {
                within_target = false;
                let field = match parent.kind() {
                    "assignment" | "for_statement" => Some("left"),
                    "as_pattern" => Some("alias"),
                    _ => None,
                };
                writes_local = field
                    .and_then(|f| parent.child_by_field_name(f))
                    .is_some_and(|n| n.id() == child.id());
            }
            match parent.kind() {
                "type" | "dotted_name" | "aliased_import" | "global_statement"
                | "nonlocal_statement" | "case_pattern" => return LexicalBinding::Unknown,
                // 独自の評価 scope は証明しない。外側へ束縛を漏らさない。
                kind if OPAQUE_SCOPES.contains(&kind) => {
                    return LexicalBinding::Unknown;
                }
                "class_definition" if field_contains(parent, "body", node) => {
                    // メソッドの自由変数は class の名前空間を見ない。class の中の `__x` は
                    // `_Class__x` に名前修飾され、class の外の `__x` の束縛とは別の名前になる。
                    if !crossed_function_body || is_mangled_name(name) {
                        return LexicalBinding::Unknown;
                    }
                }
                "function_definition" if field_contains(parent, "body", node) => {
                    crossed_function_body = true;
                    let Some(bindings) = self.scopes.get(&parent.id()) else {
                        return LexicalBinding::Unknown;
                    };
                    if bindings.ambiguous || bindings.blocked.contains(name) {
                        return LexicalBinding::Unknown;
                    }
                    if bindings.local.contains(name) {
                        if !writes_local && bindings.namespace_values.contains(name) {
                            return LexicalBinding::Unknown;
                        }
                        return LexicalBinding::FunctionLocal;
                    }
                }
                _ => {}
            }
            child = parent;
        }
        LexicalBinding::Unknown
    }
}

fn field_contains(parent: Node<'_>, field: &str, node: Node<'_>) -> bool {
    parent.child_by_field_name(field).is_some_and(|field| {
        field.start_byte() <= node.start_byte() && node.end_byte() <= field.end_byte()
    })
}

/// `in_class` は関数が class の中にあるか (名前修飾の対象の名前を照合できない)。
fn collect_bindings(
    function: Node<'_>,
    source: &[u8],
    parameter_positions: &mut HashSet<usize>,
    in_class: bool,
    namespace_targets: &HashSet<usize>,
) -> Bindings {
    let mut bindings = Bindings::default();
    let bind = |target: Node<'_>, bindings: &mut Bindings| {
        if namespace_targets.contains(&target.id()) {
            collect_targets(target, source, &mut bindings.namespace_values);
        }
        collect_targets(target, source, &mut bindings.local);
    };
    if let Some(parameters) = function.child_by_field_name("parameters") {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if let Some(target) = parameter_target(parameter) {
                bind(target, &mut bindings);
                parameter_positions.extend(target_identifiers(target).into_iter().map(|n| n.id()));
            }
        }
    }
    let mut pending: Vec<_> = function.child_by_field_name("body").into_iter().collect();
    while let Some(node) = pending.pop() {
        match node.kind() {
            "function_definition" | "class_definition" => {
                // 宣言名の実体も追わない。import を閉じ込める未知の同名束縛は保持する。
                if let Some(name) = node.child_by_field_name("name") {
                    collect_targets(name, source, &mut bindings.blocked);
                }
                continue;
            }
            kind if OPAQUE_SCOPES.contains(&kind) => continue,
            "global_statement" | "nonlocal_statement" => {
                let mut cursor = node.walk();
                for name in node.named_children(&mut cursor) {
                    // class の中の `global __x` は `global _Class__x` を意味し、綴りの違う
                    // `_Class__x` の束縛を隠す。名前修飾までは解かずに関数ごと保留する。
                    if in_class && name.utf8_text(source).is_ok_and(is_mangled_name) {
                        bindings.ambiguous = true;
                    }
                    collect_targets(name, source, &mut bindings.blocked);
                }
            }
            // import の対象を完全に解決しない限り、関数内 import は保守的に残す。
            "import_statement" | "import_from_statement" | "match_statement" => {
                bindings.ambiguous = true;
            }
            "assignment" | "augmented_assignment" | "for_statement" => {
                // 右辺の無い注釈は裸の名前だけを束縛する (`x: int` は束縛し、`(x): int` は
                // 注釈を評価するだけで束縛しない。PEP 526)。
                let annotation_only =
                    node.kind() == "assignment" && node.child_by_field_name("right").is_none();
                if let Some(target) = node.child_by_field_name("left")
                    && (!annotation_only || target.kind() == "identifier")
                {
                    bind(target, &mut bindings);
                }
            }
            "as_pattern" => {
                if let Some(target) = node.child_by_field_name("alias") {
                    bind(target, &mut bindings);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    // class の中の関数は `__x` を `_Class__x` として束縛する。`global _Class__x` の宣言で
    // 修飾後の名前がモジュールへ届くことがあるので、修飾される名前はローカルと証明しない。
    if in_class {
        let mangled: Vec<String> = bindings
            .local
            .iter()
            .filter(|name| is_mangled_name(name))
            .cloned()
            .collect();
        for name in mangled {
            bindings.local.remove(&name);
            bindings.blocked.insert(name);
        }
    }
    bindings
}

/// class の中で名前修飾される名前 (`__x`。終わりが `__` のものは除く)。
fn is_mangled_name(name: &str) -> bool {
    name.starts_with("__") && !name.ends_with("__")
}

pub(super) fn parameter_target(parameter: Node<'_>) -> Option<Node<'_>> {
    match parameter.kind() {
        "default_parameter" | "typed_default_parameter" => parameter.child_by_field_name("name"),
        "typed_parameter" => parameter.named_child(0),
        _ => Some(parameter),
    }
}

pub(super) fn collect_targets(node: Node<'_>, source: &[u8], names: &mut HashSet<String>) {
    for node in target_identifiers(node) {
        if let Ok(name) = node.utf8_text(source) {
            names.insert(name.to_owned());
        }
    }
}

fn target_identifiers(node: Node<'_>) -> Vec<Node<'_>> {
    let mut pending = vec![node];
    let mut out = Vec::new();
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier" => {
                out.push(node);
            }
            kind if TARGET_WRAPPERS.contains(&kind) => {
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::parser, language::LangId};

    fn resolve_last(source: &str) -> LexicalBinding {
        let tree = parser::parse_source(source.as_bytes(), LangId::Python).unwrap();
        let start = source.rfind("collect_items").unwrap();
        let node = tree
            .root_node()
            .descendant_for_byte_range(start, start + 13)
            .unwrap();
        PythonScopeIndex::build(node, source.as_bytes())
            .map(|index| index.resolve(node, source.as_bytes()))
            .unwrap_or_default()
    }

    #[test]
    fn function_bindings_cover_parameters_patterns_and_closures() {
        for source in [
            "def main(collect_items):\n    return collect_items\n",
            "def main(collect_items: list):\n    return collect_items\n",
            "def main(collect_items=[]):\n    return collect_items\n",
            "def main(collect_items: list=[]):\n    return collect_items\n",
            "def main(*collect_items):\n    return collect_items\n",
            "def main(**collect_items):\n    return collect_items\n",
            "def main():\n    collect_items: list\n    return collect_items\n",
            "def main():\n    collect_items: list[int] = []\n    return collect_items\n",
            "def helper(value: dict[str, int]):\n    pass\ndef main():\n    collect_items = []\n    return collect_items\n",
            "def main():\n    a = collect_items = []\n    return collect_items\n",
            "def main():\n    a, *collect_items = [1, 2]\n    return collect_items\n",
            "def main():\n    if flag:\n        collect_items = []\n    return collect_items\n",
            "def main():\n    collect_items += 1\n    return collect_items\n",
            "def main():\n    for collect_items in items:\n        print(collect_items)\n",
            "async def main():\n    async for collect_items in items:\n        print(collect_items)\n",
            "def main():\n    with resource() as collect_items:\n        print(collect_items)\n",
            "def main():\n    try:\n        pass\n    except Error as collect_items:\n        print(collect_items)\n",
            "def outer(collect_items):\n    def inner():\n        return collect_items\n",
            "def outer(collect_items):\n    class C:\n        def inner(self):\n            return collect_items\n",
            "def outer(collect_items):\n    def inner(value=collect_items):\n        pass\n",
            "def main():\n    collect_items = collect_items(3)\n",
            // 右辺のある括弧付きの注釈は代入なので束縛する。
            "def main():\n    (collect_items): list = []\n    return collect_items\n",
        ] {
            assert_eq!(
                resolve_last(source),
                LexicalBinding::FunctionLocal,
                "{source}"
            );
        }
    }

    #[test]
    fn namespace_values_propagate_through_bindings_and_closures() {
        for source in [
            "read = globals\npool = read()\ndef run():\n    x = collect_items = pool['key']\n    return collect_items(1)\n",
            "def names():\n    return globals()\ndef run():\n    collect_items = names().get(key)\n    return collect_items(1)\n",
            "def run():\n    collect_items = (lambda: globals()['collect_items'])()\n    return collect_items(1)\n",
            "read = lambda: globals()['collect_items']\ndef run():\n    collect_items = read()\n    return collect_items(1)\n",
            "def setup(cache):\n    cache.read = globals\ndef run(cache):\n    collect_items = cache.read()['key']\n    return collect_items(1)\n",
            "def outer():\n    collect_items, other = (getattr(module, key), None)\n    def inner():\n        return collect_items(1)\n",
            "def run(collect_items=getattr(module, key)):\n    return collect_items(1)\n",
            "def run():\n    for collect_items in vars(module).values():\n        collect_items(1)\n",
            "def run():\n    with open_resource(getattr(module, key)) as collect_items:\n        collect_items(1)\n",
        ] {
            assert_eq!(resolve_last(source), LexicalBinding::Unknown, "{source}");
        }
        for source in [
            "def other():\n    return globals()\ndef run(collect_items):\n    return collect_items(1)\n",
            "def other():\n    box = globals()\n    return box\ndef run():\n    box = {'key': standalone}\n    collect_items = box['key']\n    return collect_items(1)\n",
            "def run(key):\n    ns = globals()\n    collect_items = {'key': standalone}[key]\n    return collect_items(1)\n",
            "def run():\n    ns = globals()\n    collect_items = lambda: ns\n    return collect_items()\n",
            "def run():\n    collect_items = (lambda unused: globals()['collect_items'])\n    return collect_items(1)\n",
        ] {
            assert_eq!(
                resolve_last(source),
                LexicalBinding::FunctionLocal,
                "{source}"
            );
        }
    }

    #[test]
    fn imported_and_unproven_positions_are_preserved() {
        for source in [
            "from producer import collect_items\ndef main():\n    return collect_items(3)\n",
            "def main():\n    from producer import collect_items\n    return collect_items(3)\n",
            "def main():\n    global collect_items\n    collect_items = []\n    return collect_items\n",
            "def outer(collect_items):\n    def inner():\n        nonlocal collect_items\n        return collect_items\n",
            "def main(collect_items):\n    return producer.collect_items(3)\n",
            "def main(collect_items):\n    return other(collect_items=1)\n",
            "def main(collect_items=collect_items(3)):\n    pass\n",
            "def main(collect_items):\n    value: collect_items\n",
            "class C:\n    collect_items = []\n    def main(self):\n        return collect_items(3)\n",
            "def main():\n    class C:\n        collect_items = collect_items(3)\n",
            "def main():\n    [0 for collect_items in []]\n    return collect_items(3)\n",
            "def main():\n    def inner():\n        collect_items = []\n    return collect_items(3)\n",
            "def main(collect_items):\n    return [collect_items for collect_items in items]\n",
            "def main(collect_items):\n    return lambda collect_items: collect_items\n",
            "def main():\n    if (collect_items := []):\n        return collect_items\n",
            "def main[T]():\n    collect_items = []\n    return collect_items\n",
            "type Alias[V] = list[V]\ndef main():\n    collect_items = []\n    return collect_items\n",
            "def main(collect_items):\n    broken = (\n    return collect_items\n",
            // 右辺の無い括弧付きの注釈は名前を束縛しない (PEP 526)。
            "def main():\n    (collect_items): int\n    return collect_items(3)\n",
            // NFKC で同じ名前になる宣言は、綴りで照合できない。
            "def main(collect_items):\n    def inner():\n        global ｃｏｌｌｅｃｔ_ｉｔｅｍｓ\n        return collect_items(3)\n    return inner()\n",
            // クラスの中では `global __x` が `global _C__x` を意味する (名前修飾)。
            "class C:\n    def run(self):\n        global __collect_items\n        _C__collect_items = None\n        return _C__collect_items()\n",
            // 逆に `global _C__x` の後の `__x = None` はモジュールの `_C__x` へ代入する。
            "class C:\n    def run(self, override):\n        global _C__collect_items\n        if override:\n            __collect_items = None\n        return __collect_items()\n",
            // class の中の `__x` は `_C__x` を指し、class の外の関数の `__x` の束縛とは別の名前。
            "def outer(__collect_items):\n    class C:\n        def run(self):\n            return __collect_items()\n    return C().run()\n",
            // 修飾の綴りがそろう束縛も、上の宣言と区別できないので証明しない。
            "class C:\n    def run(self):\n        __collect_items = 1\n        return __collect_items\n",
        ] {
            assert_eq!(resolve_last(source), LexicalBinding::Unknown, "{source}");
        }
    }

    #[test]
    fn scope_node_tables_exist_in_python_grammar() {
        let language = LangId::Python.ts_language();
        for kind in OPAQUE_SCOPES.iter().chain(TARGET_WRAPPERS).chain(
            [
                "function_definition",
                "class_definition",
                "type",
                "dotted_name",
                "aliased_import",
                "global_statement",
                "nonlocal_statement",
                "case_pattern",
                "named_expression",
                "type_parameter",
                "type_alias_statement",
                "default_parameter",
                "typed_default_parameter",
                "typed_parameter",
                "import_statement",
                "import_from_statement",
                "match_statement",
                "assignment",
                "augmented_assignment",
                "for_statement",
                "as_pattern",
                "attribute",
                "keyword_argument",
            ]
            .iter(),
        ) {
            tree_sitter::Query::new(&language, &format!("({kind}) @node"))
                .unwrap_or_else(|e| panic!("{kind}: {e}"));
        }
    }
}
