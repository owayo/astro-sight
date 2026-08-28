//! React component 固有の互換性判定。

use super::*;

/// exported component を `memo` / `forwardRef` でラップしただけなら互換とする。
pub(crate) fn detect_react_wrapper_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(TS_JS_LANGS)?;
    if !new_sig_has_react_wrapper(site.new_sig) {
        return None;
    }
    if new_sig_has_react_wrapper(site.old_sig) {
        return None;
    }
    let src = sources.get(site)?;
    let old_props = extract_component_props_type(&src.old, lang, site.name)?;
    let new_props = extract_component_props_type(&src.new, lang, site.name)?;
    if old_props != new_props {
        return None;
    }
    if has_blocking_value_usage(index, site.name) {
        return None;
    }
    Some(site.compatible("react_component_wrapper"))
}
