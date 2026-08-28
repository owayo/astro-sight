//! TypeScript literal union alias の意味的同値判定。

use super::*;

/// 同一ファイル内で有限 literal 集合として証明でき、型パラメータ列も不変なら互換とする。
pub(crate) fn detect_equivalent_literal_union_alias_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    if site.kind != "type" {
        return None;
    }
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    if old_tree.root_node().has_error() || new_tree.root_node().has_error() {
        return None;
    }
    let old_params = type_alias_type_parameter_tokens(old_tree.root_node(), &src.old, site.name)?;
    let new_params = type_alias_type_parameter_tokens(new_tree.root_node(), &src.new, site.name)?;
    if old_params != new_params {
        return None;
    }
    let old_values = eval_named_literal_union(old_tree.root_node(), &src.old, site.name)?;
    let new_values = eval_named_literal_union(new_tree.root_node(), &src.new, site.name)?;
    (old_values == new_values).then(|| site.compatible("equivalent_literal_union_alias"))
}
