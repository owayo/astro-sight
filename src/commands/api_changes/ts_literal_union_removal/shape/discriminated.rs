//! 判別値が一意な object union の、残る要素を変えない削除だけを扱う。

use super::*;

struct Variant {
    fields: BTreeMap<String, ObjectField>,
    signature: SigTokens,
}

fn property_name(node: Node<'_>, source: &str) -> Option<String> {
    let raw = text(node, source)?;
    if raw.contains('\\') {
        return None;
    }
    let name = match node.kind() {
        "property_identifier" if plain_name(raw) => raw.to_owned(),
        "string" => decode::string(node, source)?,
        _ => return None,
    };
    (name != "__proto__").then_some(name)
}

fn field_type(node: Node<'_>, source: &str) -> Option<ObjectField> {
    if let Some(primitive) = slots::Primitive::read(node, source) {
        return (primitive != slots::Primitive::Void).then_some(ObjectField::Primitive(primitive));
    }
    let literal = (node.kind() == "literal_type")
        .then(|| only(node))
        .flatten()?;
    let raw = text(literal, source)?;
    if raw.contains('\\') || !matches!(literal.kind(), "string" | "number" | "true" | "false") {
        return None;
    }
    Some(ObjectField::Literal(raw.to_owned()))
}

fn variants(root: Node<'_>, source: &str) -> Option<Vec<Variant>> {
    let mut pending = vec![root];
    let mut variants = Vec::new();
    while let Some(node) = pending.pop() {
        match node.kind() {
            "union_type" | "parenthesized_type" => pending.extend(children(node).into_iter().rev()),
            "object_type" => {
                let mut fields = BTreeMap::new();
                for property in children(node) {
                    if property.kind() != "property_signature" || token(property, "?") {
                        return None;
                    }
                    let name = property_name(property.child_by_field_name("name")?, source)?;
                    let ty = field_type(only(property.child_by_field_name("type")?)?, source)?;
                    if fields.insert(name, ty).is_some() {
                        return None;
                    }
                }
                let mut signature = signature_tokens_in_range(
                    node,
                    source.as_bytes(),
                    node.start_byte(),
                    node.end_byte(),
                )?;
                signature.0.retain(|(kind, _)| kind != "comment");
                variants.push(Variant { fields, signature });
            }
            _ => return None,
        }
        if variants.len() + pending.len() > 256 {
            return None;
        }
    }
    (!variants.is_empty()).then_some(variants)
}

fn string_tag(field: &ObjectField) -> Option<String> {
    let ObjectField::Literal(raw) = field else {
        return None;
    };
    if !raw.starts_with(['\'', '"']) {
        return None;
    }
    String::from_utf16(&decode::units(raw.get(1..raw.len().checked_sub(1)?)?)?).ok()
}

fn tagged<'a>(variants: &'a [Variant], tag: &str) -> Option<Vec<(String, &'a Variant)>> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for variant in variants {
        let name = string_tag(variant.fields.get(tag)?)?;
        if !seen.insert(name.clone()) {
            return None;
        }
        result.push((name, variant));
    }
    Some(result)
}

pub(super) fn classify(
    site: &CompatibleModSite<'_>,
    pair: &OldNewSources,
    old_root: Node<'_>,
    old_source: &str,
    old_candidate: Node<'_>,
) -> Change {
    let Some(old_variants) = old_candidate
        .child_by_field_name("value")
        .and_then(|value| variants(value, old_source))
        .filter(|variants| variants.len() > 1)
    else {
        return Change::NotApplicable;
    };
    let tags = old_variants[0]
        .fields
        .keys()
        .filter(|tag| tagged(&old_variants, tag).is_some())
        .collect::<Vec<_>>();
    let [tag] = tags.as_slice() else {
        return Change::NotApplicable;
    };
    let tag = (*tag).clone();
    let result = (|| {
        if !checked(old_root) || pair.new.len() > inventory::MAX_SOURCE {
            return None;
        }
        let lang = site.lang_id?;
        let new = crate::engine::parser::parse_source(&pair.new, lang).ok()?;
        let new_source = std::str::from_utf8(&pair.new).ok()?;
        if !checked(new.root_node()) {
            return None;
        }
        let old_decl = declaration(old_root, old_source, site.name)?;
        let new_decl = declaration(new.root_node(), new_source, site.name)?;
        let old_value = old_decl.child_by_field_name("value")?;
        let new_value = new_decl.child_by_field_name("value")?;
        let new_variants = variants(new_value, new_source)?;
        let old_tagged = tagged(&old_variants, &tag)?;
        let new_tagged = tagged(&new_variants, &tag)?;
        let old_set = old_tagged
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>();
        let new_set = new_tagged
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>();
        if old_set.is_subset(&new_set) {
            return Some(Change::NotApplicable);
        }
        if !new_set.is_subset(&old_set)
            || header(old_decl, old_value, old_source)? != header(new_decl, new_value, new_source)?
            || old_tagged
                .iter()
                .filter(|(name, _)| new_set.contains(name))
                .map(|(name, variant)| (name, &variant.signature))
                .collect::<Vec<_>>()
                != new_tagged
                    .iter()
                    .map(|(name, variant)| (name, &variant.signature))
                    .collect::<Vec<_>>()
        {
            return None;
        }
        let objects = ObjectShape {
            tag,
            variants: new_tagged
                .iter()
                .map(|(name, variant)| (name.clone(), variant.fields.clone()))
                .collect(),
        };
        Some(Change::ObjectRemoval(
            Removal {
                removed: old_set.difference(&new_set).cloned().collect(),
                kept: new_set,
            },
            objects,
        ))
    })();
    result.unwrap_or(Change::Unsupported)
}

pub(super) fn accepts(shape: &ObjectShape, node: Node<'_>, source: &str) -> Option<()> {
    if node.kind() != "object" {
        return None;
    }
    let mut fields = BTreeMap::new();
    for member in children(node) {
        if member.kind() != "pair" {
            return None;
        }
        let name = property_name(member.child_by_field_name("key")?, source)?;
        if fields
            .insert(name, unparen(member.child_by_field_name("value")?)?)
            .is_some()
        {
            return None;
        }
    }
    let tag = decode::string(*fields.get(&shape.tag)?, source)?;
    let expected = shape.variants.get(&tag)?;
    if fields.len() != expected.len() {
        return None;
    }
    for (name, ty) in expected {
        let value = *fields.get(name)?;
        let accepted = match ty {
            ObjectField::Primitive(primitive) => primitive.accepts(value, source),
            ObjectField::Literal(raw) if raw.starts_with(['\'', '"']) => {
                decode::string(value, source) == string_tag(ty)
            }
            ObjectField::Literal(raw) => {
                text(value, source) == Some(raw.as_str())
                    && matches!(value.kind(), "number" | "true" | "false")
            }
        };
        if !accepted {
            return None;
        }
    }
    Some(())
}
