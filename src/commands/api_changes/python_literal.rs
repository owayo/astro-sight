//! Python の公開 `Literal` 型エイリアスに対する値集合変更の検出。
//!
//! 通常の symbol 集合へ型エイリアスを混ぜると `symbols` / `refs` / `dead-code` の契約まで
//! 変わるため、API 差分専用の疑似シンボルとして扱う。確実に証明できない構文は報告しない。

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use crate::engine::symbols::python_module_export_policy;
use crate::models::review::{ApiContractChange, ApiContractChangeKind, ApiContractSide};

use super::normalize_signature_whitespace;
use super::python_contract::{
    BindingCounts, TypingName, attribute_path, collect_binding_counts, collect_typing_names,
    node_text, subscript_parts, unwrap_type_node,
};

const LITERAL_QUALIFIER: &str = "Literal";
const TYPE_ALIAS_QUALIFIER: &str = "TypeAlias";

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiteralAliasFact {
    signature: String,
    values: BTreeSet<LiteralValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum LiteralValue {
    String(String),
    Integer(i128),
    Boolean(bool),
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiteralAliasChange {
    pub(crate) name: String,
    pub(crate) old_signature: String,
    pub(crate) new_signature: String,
    pub(crate) contract: Option<ApiContractChange>,
}

/// old/new の双方で直下の `Literal[...]` と証明できる公開型エイリアスだけを比較する。
pub(crate) fn detect_python_literal_alias_changes(
    old_root: Node<'_>,
    old_source: &[u8],
    new_root: Node<'_>,
    new_source: &[u8],
) -> Vec<LiteralAliasChange> {
    let (Some(old_aliases), Some(new_aliases)) = (
        collect_literal_aliases(old_root, old_source),
        collect_literal_aliases(new_root, new_source),
    ) else {
        return Vec::new();
    };

    let mut changes = Vec::new();
    for (name, old) in old_aliases {
        let Some(new) = new_aliases.get(&name) else {
            // 片側だけが Literal / PEP 613 の形なら、移行か削除かをこの検出器では決めない。
            continue;
        };
        if old.values == new.values {
            continue;
        }

        let contract = if new.values.is_subset(&old.values) {
            Some(ApiContractChange {
                kind: ApiContractChangeKind::LiteralValuesNarrowed,
                breaks: ApiContractSide::Producer,
            })
        } else if old.values.is_subset(&new.values) {
            Some(ApiContractChange {
                kind: ApiContractChangeKind::LiteralValuesWidened,
                breaks: ApiContractSide::Consumer,
            })
        } else {
            // 値の追加と削除が同時にあるため、壊れる側を一方向に決められない。
            None
        };
        changes.push(LiteralAliasChange {
            name,
            old_signature: old.signature,
            new_signature: new.signature.clone(),
            contract,
        });
    }
    changes
}

/// module 直下の、静的に証明できる `Literal` 型エイリアスを名前順に集める。
///
/// `None` はファイル全体の名前解決または公開面が確定不能、`Some(empty)` は対象なし。
fn collect_literal_aliases(
    root: Node<'_>,
    source: &[u8],
) -> Option<BTreeMap<String, LiteralAliasFact>> {
    let literal_names = collect_typing_names(root, source, LITERAL_QUALIFIER)?;
    if literal_names.is_empty() {
        return Some(BTreeMap::new());
    }
    let type_alias_names = collect_typing_names(root, source, TYPE_ALIAS_QUALIFIER)?;
    let bindings = collect_binding_counts(root, source);
    if bindings.has_dynamic_namespace_operation() {
        return None;
    }
    // `__all__` の解析はファイル全体の走査を含む。候補ごとに繰り返すと、定数表のような
    // module 直下代入が多いファイルで O(N²) になるため 1 回だけ行う。
    let export_policy = python_module_export_policy(root, source);

    let mut aliases = BTreeMap::new();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if statement.kind() != "expression_statement" {
            continue;
        }
        let Some(assignment) = statement
            .named_child(0)
            .filter(|n| n.kind() == "assignment")
        else {
            continue;
        };
        let Some(left) = assignment
            .child_by_field_name("left")
            .filter(|n| n.kind() == "identifier")
        else {
            continue;
        };
        let name = node_text(left, source)?;
        if bindings.count(&name) != 1 {
            continue;
        }
        if let Some(annotation) = assignment.child_by_field_name("type") {
            let annotation = unwrap_type_node(annotation);
            let Some(path) = attribute_path(annotation, source) else {
                continue;
            };
            if !typing_name_matches(&type_alias_names, &bindings, TYPE_ALIAS_QUALIFIER, &path) {
                continue;
            }
        }

        let Some(right) = assignment.child_by_field_name("right") else {
            continue;
        };
        let Some((path, arguments)) = subscript_parts(right, source) else {
            continue;
        };
        if !typing_name_matches(&literal_names, &bindings, LITERAL_QUALIFIER, &path)
            || arguments.is_empty()
        {
            continue;
        }

        let mut values = BTreeSet::new();
        let mut complete = true;
        for argument in arguments {
            let Some(value) = literal_value(argument, source) else {
                complete = false;
                break;
            };
            values.insert(value);
        }
        if !complete || values.is_empty() {
            continue;
        }
        match export_policy.name_is_exported(&name) {
            Some(true) => {}
            Some(false) => continue,
            None => return None,
        }
        let signature = normalize_signature_whitespace(
            source
                .get(right.start_byte()..right.end_byte())
                .unwrap_or_default(),
        );
        aliases.insert(name, LiteralAliasFact { signature, values });
    }
    Some(aliases)
}

fn typing_name_matches(
    names: &std::collections::HashSet<TypingName>,
    bindings: &BindingCounts,
    target: &str,
    path: &str,
) -> bool {
    names
        .iter()
        .any(|name| name.base_text(target) == path && name.is_provable(bindings, target))
}

fn literal_value(node: Node<'_>, source: &[u8]) -> Option<LiteralValue> {
    match node.kind() {
        "string" => python_plain_string_literal(node, source).map(LiteralValue::String),
        "integer" => {
            let raw = node_text(node, source)?;
            if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
                return None;
            }
            raw.replace('_', "")
                .parse::<i128>()
                .ok()
                .map(LiteralValue::Integer)
        }
        "true" => Some(LiteralValue::Boolean(true)),
        "false" => Some(LiteralValue::Boolean(false)),
        "none" => Some(LiteralValue::None),
        // 負数・enum member・式・入れ子 Literal は初期スコープ外。部分集合だけを比べない。
        _ => None,
    }
}

/// quote の種類だけが違う文字列を同じ値として扱う。escape / prefix / 補間は評価しない。
fn python_plain_string_literal(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut content: Option<String> = None;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "string_start" => {
                let text = child.utf8_text(source).ok()?;
                if !matches!(text, "\"" | "'" | "\"\"\"" | "'''") {
                    return None;
                }
            }
            "string_end" => {}
            "string_content" => {
                if content.is_some() {
                    return None;
                }
                // escape_sequence は string_content の子に入る。デコードせず生テキストを
                // 比較すると、quote 正規化だけで別値と誤判定するため alias ごと諦める。
                if child.named_child_count() != 0 {
                    return None;
                }
                content = Some(child.utf8_text(source).ok()?.to_string());
            }
            _ => return None,
        }
    }
    Some(content.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    fn changes(old: &str, new: &str) -> Vec<LiteralAliasChange> {
        let old_tree = parser::parse_source(old.as_bytes(), LangId::Python).expect("old parse");
        let new_tree = parser::parse_source(new.as_bytes(), LangId::Python).expect("new parse");
        detect_python_literal_alias_changes(
            old_tree.root_node(),
            old.as_bytes(),
            new_tree.root_node(),
            new.as_bytes(),
        )
    }

    #[test]
    fn classifies_literal_set_direction_and_keeps_replacement_unclassified() {
        let old = r#"
from typing import Literal

Narrow = Literal["a", "b"]
Widen = Literal["a"]
Replace = Literal["a", "b"]
Same = Literal["a", "b", "a"]
BoolVsInt = Literal[True]
"#;
        let new = r#"
from typing import Literal

Narrow = Literal["a"]
Widen = Literal["a", "b"]
Replace = Literal["a", "c"]
Same = Literal['b', 'a']
BoolVsInt = Literal[1]
"#;
        let found = changes(old, new);
        assert_eq!(
            found
                .iter()
                .map(|change| change.name.as_str())
                .collect::<Vec<_>>(),
            ["BoolVsInt", "Narrow", "Replace", "Widen"]
        );
        assert_eq!(found[0].contract, None, "True と 1 は別の Literal 値");
        assert_eq!(
            found[1].contract,
            Some(ApiContractChange {
                kind: ApiContractChangeKind::LiteralValuesNarrowed,
                breaks: ApiContractSide::Producer,
            })
        );
        assert_eq!(found[2].contract, None, "非包含変更に方向を付けない");
        assert_eq!(
            found[3].contract,
            Some(ApiContractChange {
                kind: ApiContractChangeKind::LiteralValuesWidened,
                breaks: ApiContractSide::Consumer,
            })
        );
    }

    #[test]
    fn supports_direct_qualified_and_pep613_aliases() {
        for (label, old, new) in [
            (
                "direct alias",
                "from typing import Literal as L\nMode = L['a', 'b']\n",
                "from typing import Literal as L\nMode = L['a']\n",
            ),
            (
                "qualified",
                "import typing as t\nMode = t.Literal['a', 'b']\n",
                "import typing as t\nMode = t.Literal['a']\n",
            ),
            (
                "PEP 613",
                "from typing import Literal, TypeAlias as TA\nMode: TA = Literal['a', 'b']\n",
                "from typing import Literal, TypeAlias as TA\nMode: TA = Literal['a']\n",
            ),
        ] {
            let found = changes(old, new);
            assert_eq!(found.len(), 1, "{label}: {found:?}");
            assert_eq!(found[0].name, "Mode", "{label}");
            assert_eq!(
                found[0].contract.map(|contract| contract.kind),
                Some(ApiContractChangeKind::LiteralValuesNarrowed),
                "{label}"
            );
        }
    }

    #[test]
    fn respects_static_dunder_all_and_private_names() {
        let old = r#"
from typing import Literal
__all__ = ["Mode"]
Mode = Literal["a", "b"]
Hidden = Literal["a", "b"]
_Private = Literal["a", "b"]
"#;
        let new = r#"
from typing import Literal
__all__ = ["Mode"]
Mode = Literal["a"]
Hidden = Literal["a"]
_Private = Literal["a"]
"#;
        let found = changes(old, new);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "Mode");
    }

    #[test]
    fn indeterminate_exports_and_dynamic_namespace_are_not_reported() {
        let old = r#"
from typing import Literal
__all__ = ["Mode"]
__all__ += EXTRA
Mode = Literal["a", "b"]
"#;
        let new = old.replace("Literal[\"a\", \"b\"]", "Literal[\"a\"]");
        assert!(changes(old, &new).is_empty());

        let old =
            "from typing import Literal\nMode = Literal['a', 'b']\nsetattr(object(), 'x', 1)\n";
        let new = old.replace("'a', 'b'", "'a'");
        assert!(changes(old, &new).is_empty());
    }

    #[test]
    fn shadowed_or_unknown_literal_names_are_not_reported() {
        for (old, new) in [
            (
                "from other import Literal\nMode = Literal['a', 'b']\n",
                "from other import Literal\nMode = Literal['a']\n",
            ),
            (
                "from typing import Literal\nLiteral = list\nMode = Literal['a', 'b']\n",
                "from typing import Literal\nLiteral = list\nMode = Literal['a']\n",
            ),
            (
                "from typing import *\nMode = Literal['a', 'b']\n",
                "from typing import *\nMode = Literal['a']\n",
            ),
        ] {
            assert!(changes(old, new).is_empty(), "old={old}");
        }
    }

    #[test]
    fn non_direct_or_non_static_literal_shapes_are_not_partially_compared() {
        for (old, new) in [
            (
                "from typing import Literal\nMode = Literal['a', 'b']\n",
                "from typing import Literal\nMode = Literal['a'] | None\n",
            ),
            (
                "from typing import Literal\nVALUE = 'b'\nMode = Literal['a', VALUE]\n",
                "from typing import Literal\nVALUE = 'b'\nMode = Literal['a']\n",
            ),
            (
                "from typing import Literal\nMode = Literal['a', 'b']\n",
                "from typing import Literal\ntype Mode = Literal['a']\n",
            ),
            (
                "from typing import Literal\nMode = Literal['a', 'b']\n",
                "from typing import Literal\nMode = Literal[f'{1}']\n",
            ),
        ] {
            assert!(changes(old, new).is_empty(), "new={new}");
        }

        let escaped_old = r#"from typing import Literal
Mode = Literal['it\'s', "b"]
"#;
        let escaped_new = r#"from typing import Literal
Mode = Literal["it's", "b"]
"#;
        assert!(
            changes(escaped_old, escaped_new).is_empty(),
            "escape を解釈しない以上、quote 正規化を値変更と誤判定してはならない"
        );
    }
}
