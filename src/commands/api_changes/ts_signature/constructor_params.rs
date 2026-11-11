//! クラスのほかの構造を変えない、コンストラクタ末尾の省略可能な引数追加。

use tree_sitter::Node;

use super::*;

fn class_statement(constructor: Node<'_>) -> Option<Node<'_>> {
    let body = constructor.parent()?;
    if body.kind() != "class_body" {
        return None;
    }
    let class = body.parent()?;
    if !matches!(
        class.kind(),
        "class_declaration" | "abstract_class_declaration"
    ) {
        return None;
    }
    Some(match class.parent() {
        Some(export) if export.kind() == "export_statement" => export,
        _ => class,
    })
}

fn parameter_is_property(parameter: Node<'_>) -> bool {
    let mut cursor = parameter.walk();
    parameter.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            "accessibility_modifier" | "readonly" | "override_modifier" | "decorator"
        )
    })
}

pub(super) fn trailing_optional_constructor_parameters(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<()> {
    if site.kind != "class" || site.name.is_empty() || site.name.contains('.') {
        return None;
    }
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    callable::ts_site_languages(site)?;
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    if old_tree.root_node().has_error() || new_tree.root_node().has_error() {
        return None;
    }
    let old = find_unique_top_level_class_method(
        old_tree.root_node(),
        &src.old,
        site.name,
        "constructor",
    )?;
    let new = find_unique_top_level_class_method(
        new_tree.root_node(),
        &src.new,
        site.name,
        "constructor",
    )?;
    old.child_by_field_name("body")?;
    new.child_by_field_name("body")?;
    let old_parts = ts_function_signature_parts(old, &src.old)?;
    let new_parts = ts_function_signature_parts(new, &src.new)?;
    if !ts_params_prefix_same_with_optional_tail(&old_parts.params, &new_parts.params) {
        return None;
    }
    let old_params = old.child_by_field_name("parameters")?;
    let new_params = new.child_by_field_name("parameters")?;
    let mut cursor = new_params.walk();
    if new_params
        .named_children(&mut cursor)
        .skip(old_parts.params.len())
        .any(parameter_is_property)
    {
        return None;
    }
    // 引数プロパティはフィールドも増やす。引数の外は本体を含めて同じと証明する。
    let old_statement = class_statement(old)?;
    let new_statement = class_statement(new)?;
    for (old_start, old_end, new_start, new_end) in [
        (
            old_statement.start_byte(),
            old_params.start_byte(),
            new_statement.start_byte(),
            new_params.start_byte(),
        ),
        (
            old_params.end_byte(),
            old_statement.end_byte(),
            new_params.end_byte(),
            new_statement.end_byte(),
        ),
    ] {
        if signature_tokens_in_range(old_statement, &src.old, old_start, old_end)?
            != signature_tokens_in_range(new_statement, &src.new, new_start, new_end)?
        {
            return None;
        }
    }
    Some(())
}
