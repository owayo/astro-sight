//! TypeScript 関数シグネチャ固有の互換性判定。

use super::*;

/// 末尾へ optional/default 引数を追加しただけなら既存呼び出しと互換とする。
pub(crate) fn detect_trailing_optional_params_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    with_resolved_ts_fn_pair(site, sources, |old_fn, old_source, new_fn, new_source| {
        let old_parts = ts_function_signature_parts(old_fn, old_source)?;
        let new_parts = ts_function_signature_parts(new_fn, new_source)?;
        if old_parts.head != new_parts.head || old_parts.tail != new_parts.tail {
            return None;
        }
        if !ts_params_prefix_same_with_optional_tail(&old_parts.params, &new_parts.params) {
            return None;
        }
        Some(())
    })?;
    Some(site.compatible("trailing_optional_params"))
}

/// inline object type の第一階層へ optional property を追加しただけなら互換とする。
pub(crate) fn detect_optional_object_props_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    with_resolved_ts_fn_pair(site, sources, |old_fn, old_source, new_fn, new_source| {
        let old_parts = ts_function_signature_parts(old_fn, old_source)?;
        let new_parts = ts_function_signature_parts(new_fn, new_source)?;
        if old_parts.head != new_parts.head || old_parts.tail != new_parts.tail {
            return None;
        }

        let old_params_node = old_fn.child_by_field_name("parameters")?;
        let new_params_node = new_fn.child_by_field_name("parameters")?;
        let mut old_cursor = old_params_node.walk();
        let old_children: Vec<tree_sitter::Node> =
            old_params_node.named_children(&mut old_cursor).collect();
        let mut new_cursor = new_params_node.walk();
        let new_children: Vec<tree_sitter::Node> =
            new_params_node.named_children(&mut new_cursor).collect();
        if old_children.len() != new_children.len() {
            return None;
        }

        let mut any_extension = false;
        for (old_param, new_param) in old_children.iter().zip(new_children.iter()) {
            let old_text = node_normalized_text(*old_param, old_source)?;
            let new_text = node_normalized_text(*new_param, new_source)?;
            if old_text == new_text {
                continue;
            }
            if !ts_param_pair_is_optional_object_extension(
                *old_param, old_source, *new_param, new_source,
            ) {
                return None;
            }
            any_extension = true;
        }
        any_extension.then_some(())
    })?;
    Some(site.compatible("optional_object_props"))
}

/// server component 関数への `async` 追加だけなら JSX 呼び出し互換とする。
pub(crate) fn detect_async_jsx_component_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    if site.kind != "function" || site.name.contains('.') {
        return None;
    }
    with_resolved_ts_fn_pair(site, sources, |old_fn, old_source, new_fn, new_source| {
        let old_parts = ts_function_signature_parts(old_fn, old_source)?;
        let new_parts = ts_function_signature_parts(new_fn, new_source)?;
        if old_parts.params != new_parts.params || old_parts.tail != new_parts.tail {
            return None;
        }
        if !head_is_async_addition(&old_parts.head, &new_parts.head) {
            return None;
        }
        if ts_module_has_use_client_directive(module_root(new_fn), new_source) {
            return None;
        }
        Some(())
    })?;
    if has_blocking_value_usage(index, site.name) {
        return None;
    }
    Some(site.compatible("async_jsx_component"))
}

/// 引数なし関数へ省略可能な単一 destructured 引数を追加した変更かを判定する。
pub(crate) fn is_ts_no_arg_to_optional_destructured_compatible(
    old_sig: &str,
    new_sig: &str,
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
    fn_name: &str,
) -> bool {
    let full_new_path = std::path::Path::new(dir).join(new_path);
    let Some(utf8_str) = full_new_path.to_str() else {
        return false;
    };
    let utf8_new_path = camino::Utf8Path::new(utf8_str);
    let Ok(lang_id) = crate::language::LangId::from_path(utf8_new_path) else {
        return false;
    };
    if !matches!(
        lang_id,
        crate::language::LangId::Typescript | crate::language::LangId::Tsx
    ) {
        return false;
    }
    if !signature_has_destructured_params_for(new_sig, fn_name)
        || !signature_has_empty_parens_for(old_sig, fn_name)
    {
        return false;
    }
    if !old_top_level_function_has_empty_parameters(dir, base, old_path, lang_id, fn_name) {
        return false;
    }

    let Ok(source) = parser::read_file(utf8_new_path) else {
        return false;
    };
    let Ok(tree) = parser::parse_source(&source, lang_id) else {
        return false;
    };
    let root = tree.root_node();
    let Some(fn_node) = find_top_level_function_by_name(root, &source, fn_name) else {
        return false;
    };
    let Some(params) = fn_node.child_by_field_name("parameters") else {
        return false;
    };
    is_optionally_omittable_single_destructured_param(params, root, &source)
}
