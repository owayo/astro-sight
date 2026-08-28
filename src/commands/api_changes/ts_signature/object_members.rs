//! exported object の shape 変更に対する互換性判定。

use super::*;

pub(crate) fn detect_object_members_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(TS_JS_LANGS)?;
    let src = sources.get(site)?;
    let old_keys = extract_object_member_keys(&src.old, lang, site.name)?;
    let new_keys = extract_object_member_keys(&src.new, lang, site.name)?;
    if old_keys.record_keys.is_some() != new_keys.record_keys.is_some() {
        return None;
    }
    if old_keys.declarator_type != new_keys.declarator_type
        || old_keys.wrappers != new_keys.wrappers
    {
        return None;
    }
    for (key, old_wrappers) in &old_keys.entry_wrappers {
        if let Some(new_wrappers) = new_keys.entry_wrappers.get(key)
            && new_wrappers != old_wrappers
        {
            return None;
        }
    }
    let has_added_member = new_keys
        .member_keys
        .difference(&old_keys.member_keys)
        .next()
        .is_some();
    let has_added_record_entry = match (&old_keys.record_keys, &new_keys.record_keys) {
        (Some(old_record), Some(new_record)) => {
            if old_record.difference(new_record).next().is_some() {
                return None;
            }
            new_record.difference(old_record).next().is_some()
        }
        (None, None) => false,
        _ => return None,
    };
    let removed_members: Vec<&String> = old_keys
        .member_keys
        .difference(&new_keys.member_keys)
        .collect();
    if removed_members.is_empty() && !has_added_member && !has_added_record_entry {
        return None;
    }
    for (key, old_value) in &old_keys.entry_values {
        if let Some(new_value) = new_keys.entry_values.get(key)
            && new_value != old_value
        {
            return None;
        }
    }
    let removed_member_set: HashSet<&str> =
        removed_members.iter().map(|key| key.as_str()).collect();
    if member_access_keys_have_ref(site.dir, &removed_member_set) {
        return None;
    }
    Some(site.compatible("unused_object_members"))
}
