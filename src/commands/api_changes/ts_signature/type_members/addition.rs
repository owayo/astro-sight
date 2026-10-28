//! interface / object 型への必須プロパティ追加を分類する。

use super::*;
use crate::commands::api_changes::ts_required_members::{
    MemberChangeMode, RequiredMemberChange, RequiredMemberDelta, RequiredProperty,
};

/// interface / object 型の type alias へ必須メンバーを足した変更を分類する (Issue #89)。
///
/// 宣言ヘッダ・フッタが不変で、既存メンバーがトークン一致ですべて残り、追加メンバーに必須の
/// ものがあるときだけ対象にする (それ以外は `NotApplicable` で従来の判定へ)。証明に対応する形
/// (generic でも `extends` でもない宣言へ、名前が識別子で型が単純な必須の property_signature を
/// 足し、宣言が自分自身を参照しない) なら `Supported`、それ以外の必須メンバー追加は
/// `Unsupported`。追加された optional メンバーは利用側を壊さないので無視する。
pub(crate) fn classify_required_member_addition(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> RequiredMemberChange {
    let Some(lang) = site.lang_in(TS_ONLY_LANGS) else {
        return RequiredMemberChange::NotApplicable;
    };
    if !matches!(site.kind, "type" | "trait" | "interface") {
        return RequiredMemberChange::NotApplicable;
    }
    let Some(src) = sources.get(site) else {
        return RequiredMemberChange::NotApplicable;
    };
    let Some((old_tree, new_tree)) = src.parse_pair(lang) else {
        return RequiredMemberChange::NotApplicable;
    };
    if old_tree.root_node().has_error() || new_tree.root_node().has_error() {
        return RequiredMemberChange::NotApplicable;
    }
    let (Some((old_decl, old_outer)), Some((new_decl, new_outer))) = (
        unique_type_declaration(old_tree.root_node(), site.name, &src.old),
        unique_type_declaration(new_tree.root_node(), site.name, &src.new),
    ) else {
        return RequiredMemberChange::NotApplicable;
    };
    if old_decl.kind() != new_decl.kind() {
        return RequiredMemberChange::NotApplicable;
    }
    let (Some(old_body), Some(new_body)) = (type_member_body(old_decl), type_member_body(new_decl))
    else {
        return RequiredMemberChange::NotApplicable;
    };
    let header_same = normalized_prefix(old_outer, old_body, &src.old)
        .zip(normalized_prefix(new_outer, new_body, &src.new))
        .is_some_and(|(old, new)| old == new);
    let footer_same = normalized_suffix(old_outer, old_body, &src.old)
        .zip(normalized_suffix(new_outer, new_body, &src.new))
        .is_some_and(|(old, new)| old == new);
    if !header_same || !footer_same {
        return RequiredMemberChange::NotApplicable;
    }
    let (Some(old_members), Some(new_members)) = (
        normalized_members(old_body, &src.old),
        normalized_members(new_body, &src.new),
    ) else {
        return RequiredMemberChange::NotApplicable;
    };
    let Some(required) = added_required_members(&old_members, new_members) else {
        return RequiredMemberChange::NotApplicable;
    };
    let old_names: HashSet<_> = old_members
        .iter()
        .filter_map(|(_, member)| member_name(*member, &src.old).flatten())
        .collect();
    // 型引数・継承元は追加メンバーとの衝突や型引数ごとの値を確かめられない。
    let has_extends = {
        let mut cursor = new_decl.walk();
        new_decl
            .children(&mut cursor)
            .any(|child| child.kind() == "extends_type_clause")
    };
    if new_decl.child_by_field_name("type_parameters").is_some() || has_extends {
        return RequiredMemberChange::Unsupported;
    }
    // index signature は追加プロパティの型も制約する (`[key: string]: number` に `note: string`
    // を足すと宣言自体が型エラー)。キーの範囲と型の適合を確かめないので扱わない。
    let has_index_signature = {
        let mut cursor = new_body.walk();
        new_body
            .named_children(&mut cursor)
            .any(|member| member.kind() == "index_signature")
    };
    if has_index_signature {
        return RequiredMemberChange::Unsupported;
    }
    if !unique_member_names(new_body, &src.new) {
        return RequiredMemberChange::Unsupported;
    }
    // 自分自身を参照する型は、値の中の入れ子の値も同じ型になり、最上位のキーだけでは証明できない。
    let self_referential = crate::engine::tree_walk::any_node(
        new_body,
        crate::engine::tree_walk::Children::Named,
        |node| node.kind() == "type_identifier" && node.utf8_text(&src.new).ok() == Some(site.name),
    );
    if self_referential {
        return RequiredMemberChange::Unsupported;
    }
    let mut properties = Vec::with_capacity(required.len());
    for member in required {
        if member.kind() != "property_signature" {
            return RequiredMemberChange::Unsupported;
        }
        let Some(Some(name)) = member_name(member, &src.new) else {
            return RequiredMemberChange::Unsupported;
        };
        let Some(ty) = member
            .child_by_field_name("type")
            .and_then(|annotation| simple_annotation_type(annotation, &src.new))
        else {
            return RequiredMemberChange::Unsupported;
        };
        if old_names.contains(&name) {
            return RequiredMemberChange::Unsupported;
        }
        properties.push(RequiredProperty { name, ty });
    }
    RequiredMemberChange::Supported(RequiredMemberDelta {
        added: properties,
        mode: MemberChangeMode::Addition,
    })
}

/// 既存メンバーを多重集合で照合し、追加された必須メンバーをソース順に返す。
fn added_required_members<'a>(
    old_members: &[(String, Node<'_>)],
    new_members: Vec<(String, Node<'a>)>,
) -> Option<Vec<Node<'a>>> {
    let mut remaining: HashMap<String, Vec<Node<'_>>> = HashMap::new();
    for (signature, member) in new_members {
        remaining.entry(signature).or_default().push(member);
    }
    for (signature, _) in old_members {
        match remaining.get_mut(signature) {
            Some(nodes) if !nodes.is_empty() => {
                nodes.pop();
            }
            // 既存メンバーの削除・変更を含む変更は対象外。
            _ => return None,
        }
    }
    // 出力を決定的にするため、追加メンバーはソース上の出現順に並べる。
    let mut added: Vec<Node<'_>> = remaining.into_values().flatten().collect();
    added.sort_by_key(Node::start_byte);
    let required: Vec<Node<'_>> = added
        .into_iter()
        .filter(|member| {
            !(matches!(member.kind(), "property_signature" | "method_signature")
                && ts_property_signature_is_optional(*member))
        })
        .collect();
    (!required.is_empty()).then_some(required)
}

/// optional メンバーを含めて、プロパティの重複とメソッドとの名前衝突を検査する。
fn unique_member_names(body: Node<'_>, source: &[u8]) -> bool {
    // 新しい宣言のメンバー名が衝突しないこと。同時に足した optional メンバーとの重複も含め、
    // プロパティどうしの重複とプロパティ・メソッドの同名は宣言自体が型エラーになる。
    let mut property_names = HashSet::new();
    let mut method_names = HashSet::new();
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        let key = || {
            let name = member.child_by_field_name("name")?;
            match name.kind() {
                // 名前を表記のまま比べるので、表記が揺れる名前 (数値の `1` と `0x1`、escape を含む
                // 識別子・文字列) は比べられない。
                "property_identifier" => name
                    .utf8_text(source)
                    .ok()
                    .filter(|text| !text.contains('\\'))
                    .map(str::to_string),
                "string" => super::super::static_js_string_text(name, source)
                    .filter(|text| !text.contains('\\'))
                    .map(str::to_string),
                _ => None,
            }
        };
        match member.kind() {
            "property_signature" => match key() {
                Some(name) if property_names.insert(name.clone()) => {}
                _ => return false,
            },
            "method_signature" => match key() {
                Some(name) => {
                    method_names.insert(name);
                }
                None => return false,
            },
            _ => {}
        }
    }
    if property_names
        .iter()
        .any(|name| method_names.contains(name))
    {
        return false;
    }
    true
}
