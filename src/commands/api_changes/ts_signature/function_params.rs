//! TypeScript 関数シグネチャ固有の互換性判定。

use super::*;

/// 末尾へ optional/default 引数を追加しただけなら既存呼び出しと互換とする。
pub(crate) fn detect_trailing_optional_params_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
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
    sources: &mut SignatureSourceCache<'_>,
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
    sources: &mut SignatureSourceCache<'_>,
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

/// 引数なし関数へ、default 値も `?` も無い destructured 引数 (型の全メンバーが optional) を
/// 足しただけの関数コンポーネントを JSX 呼び出し互換とする。
///
/// TS では default 値も `?` も無い引数は省略できない (`f()` は TS2554) ので、型が全 optional
/// でも直接呼び出しは壊れる。JSX (`<F />`) は常に props object を渡すため、参照がすべて
/// JSX タグ利用 (と import / 定義) の場合だけ降格する。default 値 / `?` 付きの引数追加は
/// 直接呼び出しでも省略できるので `trailing_optional_params` が扱う。
///
/// 戻り値型・`async`・型パラメータなど引数以外の signature が変わった場合は不成立
/// (引数の省略可否だけを見て変更ごと捨てていた旧判定は、`getLabel(): string` →
/// `getLabel({ loud } = {}): number` の戻り値型変更まで互換扱いしていた)。
pub(crate) fn detect_optional_props_jsx_component_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<CompatibleApiModification> {
    if site.kind != "function" || site.name.contains('.') {
        return None;
    }
    with_resolved_ts_fn_pair(site, sources, |old_fn, old_source, new_fn, new_source| {
        let old_parts = ts_function_signature_parts(old_fn, old_source)?;
        let new_parts = ts_function_signature_parts(new_fn, new_source)?;
        if old_parts.head != new_parts.head || old_parts.tail != new_parts.tail {
            return None;
        }
        if !old_parts.params.is_empty() || new_parts.params.iter().any(|p| p.omittable) {
            return None;
        }
        let params = new_fn.child_by_field_name("parameters")?;
        is_optionally_omittable_single_destructured_param(params, module_root(new_fn), new_source)
            .then_some(())
    })?;
    if has_blocking_value_usage(index, site.name) {
        return None;
    }
    Some(site.compatible("optional_props_jsx_component"))
}
