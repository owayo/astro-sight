//! TypeScript 関数シグネチャ固有の互換性判定。

use super::*;
use tree_sitter::Node;

/// 末尾へ optional/default 引数を追加しただけなら既存呼び出しと互換とする。
pub(crate) fn detect_trailing_optional_params_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<CompatibleApiModification> {
    if site.kind == "class" {
        constructor_params::trailing_optional_constructor_parameters(site, sources)?;
        return Some(site.compatible("trailing_optional_params"));
    }
    with_resolved_ts_callable_pair(site, sources, trailing_optional_prefix_len)?;
    Some(site.compatible("trailing_optional_params"))
}

/// inline object type の第一階層へ optional property を追加しただけなら互換とする。
/// `react` の `memo()` で包んだ関数部品は、包んだ関数に委ねる (Issue #266)。
pub(crate) fn detect_optional_object_props_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    caches: &mut ApiClosureCaches,
    diff_files: &[crate::models::impact::DiffFile],
    public_name_differs: bool,
) -> Option<Result<CompatibleApiModification, ()>> {
    let mut check =
        |old_fn: Node<'_>, old_source: &[u8], new_fn: Node<'_>, new_source: &[u8], memo| {
            let extension = optional_object_props_extension(
                old_fn,
                old_source,
                new_fn,
                new_source,
                Some(site),
                None,
            )?;
            let safe =
                added_props::preserves_return(old_fn, old_source, new_fn, new_source, &extension)
                    && (extension.parameters.is_empty()
                        || (!public_name_differs
                            && added_props::repository_accepts(
                                index,
                                site,
                                &mut caches.optional_removal,
                                new_fn,
                                new_source,
                                extension.parameters,
                                memo,
                            )));
            Some(
                safe.then(|| site.compatible("optional_object_props"))
                    .ok_or(()),
            )
        };
    with_resolved_ts_callable_pair(site, sources, |old, old_source, new, new_source| {
        check(old, old_source, new, new_source, false)
    })
    .or_else(|| {
        super::react_memo::with_resolved_react_memo_pair(
            site,
            sources,
            &mut caches.react_memo,
            diff_files,
            |old, old_source, new, new_source| check(old, old_source, new, new_source, true),
        )
    })
}

/// 型依存の変更を、宣言とこの引数だけに現れる非公開の props 型まで展開して確かめる。
pub(crate) fn prove_local_named_property_addition(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut OptionalRemovalCache,
    diff_files: &[crate::models::impact::DiffFile],
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    if site.old_path != site.new_path {
        return None;
    }
    let src = sources.get(site)?;
    if src.original_old() != src.old || src.original_new() != src.new.as_ref() {
        return None;
    }
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    let (old_root, new_root) = (old_tree.root_node(), new_tree.root_node());
    let resolve = |root, source: &[u8]| {
        let (function, statement) =
            removed_params::unique_function_declaration(root, source, site.name)?;
        let param = removed_props::only_parameter(function)?;
        if statement.kind() != "export_statement"
            || param.child_by_field_name("pattern")?.kind() != "object_pattern"
        {
            return None;
        }
        let ty = super::super::ts_required_members::only_named_child(
            param.child_by_field_name("type")?,
        )?;
        if ty.kind() != "type_identifier" {
            return None;
        }
        let object = removed_props::exclusive_local_object(root, source, ty, true)?;
        Some((function, ty, object))
    };
    let (old_fn, old_type, old_object) = resolve(old_root, &src.old)?;
    let (new_fn, new_type, new_object) = resolve(new_root, &src.new)?;
    if old_type.utf8_text(&src.old).ok()? != new_type.utf8_text(&src.new).ok()? {
        return None;
    }
    let extension = optional_object_props_extension(
        old_fn,
        &src.old,
        new_fn,
        &src.new,
        Some(site),
        Some((old_object, new_object)),
    )?;
    if extension.bindings.is_empty()
        || !added_props::preserves_return(old_fn, &src.old, new_fn, &src.new, &extension)
    {
        return None;
    }
    // 型依存の変更を例外扱いするので、推論した戻り値も字面の一致だけでは決めない。
    let declared_return = new_fn.child_by_field_name("return_type");
    if declared_return.is_none()
        && !(added_props::returns_jsx(old_fn.child_by_field_name("body")?, &src.old)
            && added_props::returns_jsx(new_fn.child_by_field_name("body")?, &src.new))
    {
        return None;
    }
    let mut type_roots = vec![new_object];
    type_roots.extend(declared_return);
    if !type_dependencies::property_dependencies_unchanged(
        site,
        src,
        old_root,
        new_root,
        cache.evidence(site.dir)?,
        diff_files,
        &type_roots,
    ) || !added_props::repository_accepts(
        index,
        site,
        cache,
        new_fn,
        &src.new,
        extension.parameters,
        false,
    ) {
        return None;
    }
    Some(site.compatible("optional_object_props"))
}

pub(super) struct OptionalPropsExtension<'tree> {
    pub(super) parameters: Vec<added_props::ParameterAddition<'tree>>,
    pub(super) bindings: Vec<pattern_extension::AddedBinding>,
}

pub(super) fn optional_object_props_extension<'new>(
    old_fn: tree_sitter::Node<'_>,
    old_source: &[u8],
    new_fn: tree_sitter::Node<'new>,
    new_source: &[u8],
    site: Option<&CompatibleModSite<'_>>,
    resolved_object: Option<(Node<'_>, Node<'new>)>,
) -> Option<OptionalPropsExtension<'new>> {
    let old_parts = ts_function_signature_parts(old_fn, old_source)?;
    let new_parts = ts_function_signature_parts(new_fn, new_source)?;
    if old_parts.head != new_parts.head || old_parts.tail != new_parts.tail {
        return None;
    }

    let old_params_node = old_fn.child_by_field_name("parameters")?;
    let new_params_node = new_fn.child_by_field_name("parameters")?;
    let mut old_cursor = old_params_node.walk();
    let old_children: Vec<tree_sitter::Node> = old_params_node
        .named_children(&mut old_cursor)
        .filter(|n| !n.is_extra())
        .collect();
    let mut new_cursor = new_params_node.walk();
    let new_children: Vec<tree_sitter::Node> = new_params_node
        .named_children(&mut new_cursor)
        .filter(|n| !n.is_extra())
        .collect();
    if old_children.len() != new_children.len() {
        return None;
    }

    let mut any_extension = false;
    let mut added_bindings = Vec::new();
    let mut additions = Vec::new();
    for (position, (old_param, new_param)) in
        old_children.iter().zip(new_children.iter()).enumerate()
    {
        let old_text = node_signature_tokens(*old_param, old_source)?;
        let new_text = node_signature_tokens(*new_param, new_source)?;
        if old_text == new_text && resolved_object.is_none() {
            continue;
        }
        let types = match resolved_object {
            Some(types) => types,
            None => (
                old_param.child_by_field_name("type")?.named_child(0)?,
                new_param.child_by_field_name("type")?.named_child(0)?,
            ),
        };
        let members = ts_param_pair_optional_object_extension(
            *old_param,
            old_source,
            *new_param,
            new_source,
            types,
            site,
            &mut added_bindings,
        )?;
        if !members.is_empty() {
            additions.push(added_props::ParameterAddition {
                position,
                parameter: *new_param,
                members,
            });
        }
        any_extension = true;
    }
    (any_extension
        && pattern_extension::signature_preserves_bindings(new_fn, new_source, &added_bindings))
    .then_some(OptionalPropsExtension {
        parameters: additions,
        bindings: added_bindings,
    })
}

/// server component 関数への `async` 追加だけなら JSX 呼び出し互換とする。
pub(crate) fn detect_async_jsx_component_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    caches: &mut ApiClosureCaches,
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
    if has_blocking_value_usage(index, site.dir, site.name, caches) {
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
    caches: &mut ApiClosureCaches,
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
    if has_blocking_value_usage(index, site.dir, site.name, caches) {
        return None;
    }
    Some(site.compatible("optional_props_jsx_component"))
}
