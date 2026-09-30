//! Go のテストランナーとドキュメントが規約で発見する自由関数の判定。

use tree_sitter::Node;
use unicode_general_category::{GeneralCategory, get_general_category};

use crate::models::location::Range;

use super::node_for_symbol_range;

pub(crate) fn is_go_test_entrypoint(
    root: Node<'_>,
    source: &[u8],
    range: &Range,
    file_path: Option<&str>,
) -> bool {
    if !file_path.is_some_and(|p| p.ends_with("_test.go")) {
        return false;
    }
    let Some(function) = node_for_symbol_range(root, range) else {
        return false;
    };
    if function.kind() != "function_declaration"
        || function.parent().map(|p| p.id()) != Some(root.id())
        || ["name", "parameters", "result"]
            .into_iter()
            .filter_map(|field| function.child_by_field_name(field))
            .any(|node| node.has_error())
        || function.child_by_field_name("type_parameters").is_some()
    {
        return false;
    }
    // 空の戻り値リスト `()` は戻り値なしと同じ。
    if function
        .child_by_field_name("result")
        .is_some_and(|r| r.kind() != "parameter_list" || non_comment_children(r).next().is_some())
    {
        return false;
    }
    let Some(name) = field_text(function, "name", source) else {
        return false;
    };
    let Some(parameters) = function.child_by_field_name("parameters") else {
        return false;
    };
    let params: Vec<_> = non_comment_children(parameters).collect();
    // Output コメントの無い Example も公開ドキュメントの入口なので保持する。
    if has_test_prefix(name, "Example") {
        return params.is_empty() && function.child_by_field_name("body").is_some();
    }
    let expected = if name == "TestMain" {
        &["M", "T"][..]
    } else if has_test_prefix(name, "Test") {
        &["T"][..]
    } else if has_test_prefix(name, "Benchmark") {
        &["B"][..]
    } else if has_test_prefix(name, "Fuzz") {
        &["F"][..]
    } else {
        return false;
    };
    let [param] = params.as_slice() else {
        return false;
    };
    if param.kind() != "parameter_declaration" {
        return false;
    }
    let mut cursor = param.walk();
    if param.children_by_field_name("name", &mut cursor).count() > 1 {
        return false;
    }
    let Some(pointer) = param.child_by_field_name("type") else {
        return false;
    };
    if pointer.kind() != "pointer_type" {
        return false;
    }
    let Some(ty) = non_comment_children(pointer).next() else {
        return false;
    };
    // Go のランナーは型名だけで入口を選ぶ。型エイリアス経由も有効なので import 元で絞らない。
    let type_name = match ty.kind() {
        "qualified_type" => field_text(ty, "name", source),
        "type_identifier" => ty.utf8_text(source).ok(),
        _ => return false,
    };
    type_name.is_some_and(|name| expected.contains(&name))
}

fn has_test_prefix(name: &str, prefix: &str) -> bool {
    // Go の unicode.IsLower は Ll カテゴリ。Rust の is_lowercase は Other_Lowercase も含む。
    name.strip_prefix(prefix).is_some_and(|suffix| {
        suffix
            .chars()
            .next()
            .is_none_or(|c| get_general_category(c) != GeneralCategory::LowercaseLetter)
    })
}

fn non_comment_children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.named_child_count())
        .filter_map(move |i| node.named_child(i as u32))
        .filter(|child| child.kind() != "comment")
}

fn field_text<'a>(node: Node<'_>, field: &str, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name(field)?.utf8_text(source).ok()
}
