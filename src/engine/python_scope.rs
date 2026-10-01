//! impact 用の Python 関数ローカル束縛の証明。refs の出現一覧は変更しない。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

const OPAQUE_SCOPES: &[&str] = &[
    "lambda",
    "list_comprehension",
    "set_comprehension",
    "dictionary_comprehension",
    "generator_expression",
];
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LexicalBinding {
    #[default]
    Unknown,
    FunctionLocal,
}

#[derive(Default)]
struct Bindings {
    local: HashSet<String>,
    blocked: HashSet<String>,
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
        let mut scopes = HashMap::new();
        let mut parameter_positions = HashSet::new();
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
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
            if node.kind() == "function_definition" {
                scopes.insert(
                    node.id(),
                    collect_bindings(node, source, &mut parameter_positions),
                );
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return Some(Self {
                        scopes,
                        parameter_positions,
                    });
                }
            }
        }
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
        while let Some(parent) = child.parent() {
            match parent.kind() {
                "type" | "dotted_name" | "aliased_import" | "global_statement"
                | "nonlocal_statement" | "case_pattern" => return LexicalBinding::Unknown,
                // 独自の評価 scope は証明しない。外側へ束縛を漏らさない。
                kind if OPAQUE_SCOPES.contains(&kind) => {
                    return LexicalBinding::Unknown;
                }
                "class_definition" if field_contains(parent, "body", node) => {
                    // メソッドの自由変数は class の名前空間を見ない。
                    if !crossed_function_body {
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

fn collect_bindings(
    function: Node<'_>,
    source: &[u8],
    parameter_positions: &mut HashSet<usize>,
) -> Bindings {
    let mut bindings = Bindings::default();
    if let Some(parameters) = function.child_by_field_name("parameters") {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            let target = match parameter.kind() {
                "default_parameter" | "typed_default_parameter" => {
                    parameter.child_by_field_name("name")
                }
                "typed_parameter" => parameter.named_child(0),
                _ => Some(parameter),
            };
            if let Some(target) = target {
                collect_targets(target, source, &mut bindings.local);
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
                    collect_targets(name, source, &mut bindings.blocked);
                }
            }
            // import の対象を完全に解決しない限り、関数内 import は保守的に残す。
            "import_statement" | "import_from_statement" | "match_statement" => {
                bindings.ambiguous = true;
            }
            "assignment" | "augmented_assignment" | "for_statement" => {
                if let Some(target) = node.child_by_field_name("left") {
                    collect_targets(target, source, &mut bindings.local);
                }
            }
            "as_pattern" => {
                if let Some(target) = node.child_by_field_name("alias") {
                    collect_targets(target, source, &mut bindings.local);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    bindings
}

fn collect_targets(node: Node<'_>, source: &[u8], names: &mut HashSet<String>) {
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
