//! 静的オブジェクトの変更パスと、直接読む未変更メンバーの証明。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::engine::{diff, parser, symbols};
use crate::language::LangId;
use crate::models::impact::AffectedSymbol;
use crate::models::symbol::Symbol;

#[derive(PartialEq)]
enum StaticValue {
    Literal(String, String),
    Object(Vec<(String, StaticValue)>),
}

pub(super) struct ObjectMemberChange {
    old: StaticValue,
    new: StaticValue,
    changed: Vec<String>,
}

impl ObjectMemberChange {
    pub(super) fn read_is_unchanged(&self, path: &str) -> bool {
        !path.is_empty()
            && has_own_path(&self.old, path)
            && has_own_path(&self.new, path)
            && !self
                .changed
                .iter()
                .any(|changed| paths_overlap(path, changed))
    }
}

fn has_own_path(value: &StaticValue, path: &str) -> bool {
    own_value(value, path).is_some()
}

fn own_value<'a>(value: &'a StaticValue, path: &str) -> Option<&'a StaticValue> {
    let mut current = value;
    for part in path.split('.') {
        let StaticValue::Object(members) = current else {
            return None;
        };
        let (_, next) = members.iter().find(|(key, _)| key == part)?;
        current = next;
    }
    Some(current)
}

fn paths_overlap(left: &str, right: &str) -> bool {
    fn prefix(path: &str, ancestor: &str) -> bool {
        ancestor.is_empty()
            || path == ancestor
            || path
                .strip_prefix(ancestor)
                .is_some_and(|rest| rest.starts_with('.'))
    }
    prefix(left, right) || prefix(right, left)
}

/// '.' 連結が一意になるキーだけを扱う。escape の解釈は推測しない。
fn static_key(node: Node<'_>, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?;
    let key = match node.kind() {
        "property_identifier" => text,
        "string" if text.starts_with(['\'', '"']) && text.len() >= 2 => &text[1..text.len() - 1],
        _ => return None,
    };
    if key.is_empty() || key.contains(['.', '\\']) || key == "__proto__" {
        return None;
    }
    Some(key.to_string())
}

fn static_value(node: Node<'_>, source: &[u8], depth: usize) -> Option<StaticValue> {
    // 病的な入れ子で Rust のスタックを使い切らない。
    if depth > 64 || node.has_error() || node.is_missing() {
        return None;
    }
    match node.kind() {
        "object" => {
            let mut keys = HashSet::new();
            let mut members = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.is_extra() {
                    continue;
                }
                if child.kind() != "pair" {
                    return None;
                }
                let key = static_key(child.child_by_field_name("key")?, source)?;
                if !keys.insert(key.clone()) {
                    return None;
                }
                let value = static_value(child.child_by_field_name("value")?, source, depth + 1)?;
                members.push((key, value));
            }
            Some(StaticValue::Object(members))
        }
        "number" | "string" | "true" | "false" | "null" => Some(StaticValue::Literal(
            node.kind().to_string(),
            node.utf8_text(source).ok()?.to_string(),
        )),
        _ => None,
    }
}

fn child_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        format!("{parent}.{key}")
    }
}

fn changed_paths(old: &StaticValue, new: &StaticValue, path: &str, changed: &mut Vec<String>) {
    if old == new {
        return;
    }
    let (StaticValue::Object(old_members), StaticValue::Object(new_members)) = (old, new) else {
        changed.push(path.to_string());
        return;
    };
    // 共通キーの順序変更も、そのオブジェクトを読む側から観測できる。
    let old_keys: HashSet<&str> = old_members.iter().map(|(key, _)| key.as_str()).collect();
    let new_keys: HashSet<&str> = new_members.iter().map(|(key, _)| key.as_str()).collect();
    let old_common: Vec<&str> = old_members
        .iter()
        .filter(|(key, _)| new_keys.contains(key.as_str()))
        .map(|(key, _)| key.as_str())
        .collect();
    let new_common: Vec<&str> = new_members
        .iter()
        .filter(|(key, _)| old_keys.contains(key.as_str()))
        .map(|(key, _)| key.as_str())
        .collect();
    if old_common != new_common {
        changed.push(path.to_string());
        return;
    }
    let new_by_key: HashMap<&str, &StaticValue> = new_members
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    for (key, old_value) in old_members {
        let member_path = child_path(path, key);
        if let Some(new_value) = new_by_key.get(key.as_str()) {
            changed_paths(old_value, new_value, &member_path, changed);
        } else {
            changed.push(member_path);
        }
    }
    for (key, _) in new_members {
        if !old_keys.contains(key.as_str()) {
            changed.push(child_path(path, key));
        }
    }
}

fn header_tokens(node: Node<'_>, omitted_id: usize, source: &[u8]) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut cursor = node.walk();
    loop {
        let current = cursor.node();
        let skip = current.id() == omitted_id || current.is_extra();
        if !skip && current.child_count() == 0 {
            tokens.push(current.utf8_text(source).ok()?.to_string());
        }
        if !skip && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() || cursor.node().id() == node.id() {
                return Some(tokens);
            }
        }
    }
}

fn object_declaration(
    root: Node<'_>,
    source: &[u8],
    symbol: &Symbol,
) -> Option<(Vec<String>, StaticValue)> {
    let declaration = super::descendant_for_range(root, &symbol.range)?;
    if declaration.kind() != "variable_declarator"
        || declaration.child_by_field_name("name")?.kind() != "identifier"
        || declaration.child_by_field_name("type").is_some()
    {
        return None;
    }
    let lexical = declaration.parent()?;
    if lexical.kind() != "lexical_declaration"
        || lexical
            .child_by_field_name("kind")?
            .utf8_text(source)
            .ok()?
            != "const"
    {
        return None;
    }
    let value = declaration.child_by_field_name("value")?;
    if value.kind() != "object" {
        return None;
    }
    let header = lexical
        .parent()
        .filter(|parent| parent.kind() == "export_statement")
        .unwrap_or(lexical);
    Some((
        header_tokens(header, value.id(), source)?,
        static_value(value, source, 0)?,
    ))
}

/// 復元・parse はファイルごとに一度だけ。失敗時は証明を一切残さない。
pub(super) fn collect_object_member_changes(
    file_diff: &str,
    path: &str,
    root: Node<'_>,
    source: &[u8],
    lang: LangId,
    syms: &[Symbol],
    affected: &[AffectedSymbol],
) -> HashMap<String, ObjectMemberChange> {
    let empty = HashMap::new();
    if !matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx) || root.has_error() {
        return empty;
    }
    let candidates: Vec<_> = affected
        .iter()
        .filter(|affected| affected.change_type == "modified")
        .filter_map(|affected| {
            let mut matches = syms.iter().filter(|symbol| symbol.name == affected.name);
            let symbol = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            let (header, value) = object_declaration(root, source, symbol)?;
            Some((symbol, header, value))
        })
        .collect();
    if candidates.is_empty() {
        return empty;
    }
    let Some(old_source) = diff::reconstruct_old_source(file_diff, path, source) else {
        return empty;
    };
    let Ok(old_tree) = parser::parse_source(&old_source.source, lang) else {
        return empty;
    };
    if old_tree.root_node().has_error() {
        return empty;
    }
    let Ok(old_symbols) = symbols::extract_symbols(old_tree.root_node(), &old_source.source, lang)
    else {
        return empty;
    };
    let mut changes = HashMap::new();
    let mut new_declarations = HashMap::new();
    let mut old_declarations = HashMap::new();
    for (symbol, header, new) in candidates {
        let mut matches = old_symbols.iter().filter(|old| old.name == symbol.name);
        let Some(old_symbol) = matches.next() else {
            continue;
        };
        if matches.next().is_some()
            || old_symbol.kind != symbol.kind
            || old_symbol.container != symbol.container
        {
            continue;
        }
        let Some((old_header, old)) =
            object_declaration(old_tree.root_node(), &old_source.source, old_symbol)
        else {
            continue;
        };
        if old_header != header {
            continue;
        }
        let declaration_name_id = |root, symbol: &Symbol| {
            super::descendant_for_range(root, &symbol.range)?
                .child_by_field_name("name")
                .map(|name| name.id())
        };
        let Some(new_name_id) = declaration_name_id(root, symbol) else {
            continue;
        };
        let Some(old_name_id) = declaration_name_id(old_tree.root_node(), old_symbol) else {
            continue;
        };
        let mut changed = Vec::new();
        changed_paths(&old, &new, "", &mut changed);
        new_declarations.insert(symbol.name.as_str(), new_name_id);
        old_declarations.insert(symbol.name.as_str(), old_name_id);
        changes.insert(
            symbol.name.clone(),
            ObjectMemberChange { old, new, changed },
        );
    }
    let mut escaped = source_escapes(root, source, &changes, &new_declarations, false);
    escaped.extend(source_escapes(
        old_tree.root_node(),
        &old_source.source,
        &changes,
        &old_declarations,
        true,
    ));
    changes.retain(|name, _| !escaped.contains(name));
    changes
}

/// 宣言元で変更・流出し得るオブジェクトは、初期化子だけで未変更と証明しない。
/// 候補名の索引を使い、各ソースを一度だけ走査する。
fn source_escapes(
    root: Node<'_>,
    source: &[u8],
    changes: &HashMap<String, ObjectMemberChange>,
    declarations: &HashMap<&str, usize>,
    old_side: bool,
) -> HashSet<String> {
    let mut escaped = HashSet::new();
    if changes.is_empty() {
        return escaped;
    }
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        // 型タグの参照先は追跡せず、JSDoc があるファイル全体を保守側に戻す。
        if node.kind() == "comment"
            && node
                .utf8_text(source)
                .is_ok_and(|text| text.starts_with("/**") && text.contains('@'))
        {
            return changes.keys().cloned().collect();
        }
        // eval や escape 識別子は、名前一致の参照一覧で安全を証明しない。
        if matches!(
            node.kind(),
            "identifier" | "property_identifier" | "shorthand_property_identifier"
        ) && node
            .utf8_text(source)
            .is_ok_and(|name| name == "eval" || name.contains('\\'))
        {
            return changes.keys().cloned().collect();
        }
        if matches!(node.kind(), "identifier" | "shorthand_property_identifier")
            && let Ok(name) = node.utf8_text(source)
            && let Some(change) = changes.get(name)
            && !escaped.contains(name)
            && declarations.get(name) != Some(&node.id())
        {
            let named_export = node.parent().is_some_and(|parent| {
                parent.kind() == "export_specifier"
                    && parent.child_by_field_name("alias").is_none()
                    && parent
                        .child_by_field_name("name")
                        .is_some_and(|exported| exported.id() == node.id())
            });
            let value = if old_side { &change.old } else { &change.new };
            let literal_read = direct_member_read(node, source).is_some_and(|path| {
                matches!(own_value(value, &path), Some(StaticValue::Literal(..)))
            });
            if !named_export && !literal_read {
                escaped.insert(name.to_string());
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return escaped;
            }
        }
    }
}

/// bare identifier を起点とする dot chain の読み出しだけを返す。
pub(super) fn direct_member_read(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "identifier" || node.utf8_text(source).ok()?.contains('\\') {
        return None;
    }
    let mut current = node;
    let mut path = String::new();
    while let Some(parent) = current.parent() {
        if parent.kind() != "member_expression" {
            break;
        }
        if parent.child_by_field_name("object")?.id() != current.id()
            || parent.child_by_field_name("optional_chain").is_some()
            || parent.has_error()
        {
            return None;
        }
        let property = parent.child_by_field_name("property")?;
        if property.kind() != "property_identifier" {
            return None;
        }
        let key = static_key(property, source)?;
        path = child_path(&path, &key);
        current = parent;
    }
    if path.is_empty() {
        return None;
    }
    // 許可する親を列挙し、未知のラッパー・型位置・書き込みは従来判定へ戻す。
    let parent = current.parent()?;
    match parent.kind() {
        "return_statement"
        | "expression_statement"
        | "arguments"
        | "array"
        | "binary_expression"
        | "ternary_expression"
        | "template_substitution" => Some(path),
        "variable_declarator" | "pair"
            if parent.child_by_field_name("value")?.id() == current.id() =>
        {
            Some(path)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANGS: [LangId; 3] = [LangId::Javascript, LangId::Typescript, LangId::Tsx];

    fn evidence(before: &str, after: &str) -> HashMap<String, ObjectMemberChange> {
        let tree = parser::parse_source(after.as_bytes(), LangId::Typescript).unwrap();
        let syms = symbols::extract_symbols(tree.root_node(), after.as_bytes(), LangId::Typescript)
            .unwrap();
        let diff = format!(
            "diff --git a/layout.ts b/layout.ts\n--- a/layout.ts\n+++ b/layout.ts\n@@ -1 +1 @@\n-{}\n+{}\n",
            before.trim_end(),
            after.trim_end()
        );
        collect_object_member_changes(
            &diff,
            "layout.ts",
            tree.root_node(),
            after.as_bytes(),
            LangId::Typescript,
            &syms,
            &[AffectedSymbol {
                name: "layout".to_string(),
                kind: "variable".to_string(),
                change_type: "modified".to_string(),
            }],
        )
    }

    #[test]
    fn paths_overlap_only_on_member_boundaries() {
        for (left, right, overlap) in [
            ("options.level", "options", true),
            ("options", "options.level", true),
            ("key", "", true),
            ("key", "key", true),
            ("key", "keys", false),
            ("options.stable", "options.level", false),
        ] {
            assert_eq!(paths_overlap(left, right), overlap, "{left} / {right}");
        }
    }

    #[test]
    fn object_member_changes_require_known_paths_and_preserve_key_order() {
        let before = "export const layout = { key: 1, options: { first: 10, second: 20 } };";
        let unchanged = evidence(
            before,
            "export const layout = { key: 1, options: { first: 11, second: 20 } };",
        );
        let change = &unchanged["layout"];
        assert!(change.read_is_unchanged("key"));
        assert!(change.read_is_unchanged("options.second"));
        for path in [
            "",
            "options",
            "options.first",
            "missing",
            "key.deep",
            "toString",
        ] {
            assert!(!change.read_is_unchanged(path), "{path}");
        }
        let reordered = evidence(
            before,
            "export const layout = { key: 1, options: { second: 20, first: 10 } };",
        );
        assert!(reordered["layout"].read_is_unchanged("key"));
        assert!(!reordered["layout"].read_is_unchanged("options.second"));
        let root_reordered = evidence(
            before,
            "export const layout = { options: { first: 10, second: 20 }, key: 1 };",
        );
        assert!(!root_reordered["layout"].read_is_unchanged("key"));
        let added = evidence(
            before,
            "export const layout = { key: 1, options: { first: 10, added: true, second: 20 } };",
        );
        assert!(added["layout"].read_is_unchanged("key"));
        assert!(added["layout"].read_is_unchanged("options.second"));
        assert!(!added["layout"].read_is_unchanged("options"));
    }

    #[test]
    fn object_member_changes_reject_parse_errors_duplicates_and_headers() {
        let before = "export const layout = { key: 1, level: 10 };";
        for after in [
            "export const layout = { key: 1, level: 20;",
            "export const layout = { key: 1, level: };",
            "export const layout = { key: 1, level: 20 }; const layout = { key: 1 };",
            "export const layout = { key: 1, level: 20 }; function f() { const layout = 0; }",
            "export let layout = { key: 1, level: 20 };",
            "export const layout: { key: number, level: number } = { key: 1, level: 20 };",
            "export const layout = { key: 1, level: 20 } satisfies object;",
            "export const layout = { key: 1, level: [20] };",
            "export const layout = { key: 1, level };",
            "export const layout = { key: 1, '\\u006bey': 1, level: 20 };",
            "export const layout = { key: 1, 'key': 2, level: 20 };",
            "export const layout = { key: 1, '__proto__': {}, level: 20 };",
        ] {
            assert!(evidence(before, after).is_empty(), "{after}");
            assert!(evidence(after, before).is_empty(), "old: {after}");
        }
    }

    #[test]
    fn object_member_changes_validate_source_reads_and_named_exports() {
        let before = "const layout = { key: 1, options: { level: 10 } }; export { layout }; function read() { return layout.key; }";
        let after = "const layout = { key: 1, options: { level: 20 } }; export { layout }; function read() { return layout.key; }";
        assert!(evidence(before, after)["layout"].read_is_unchanged("key"));
        for escape in [
            "layout.key = layout.options.level;",
            "[layout.key] = [2];",
            "({ k: layout.key } = source);",
            "for (layout.key of values) {}",
            "const alias = layout;",
            "const holder = { layout };",
            "const options = layout.options;",
            "export default layout;",
            "export { layout as alias };",
            "export { layout as default };",
            "eval('layout.key = 2');",
            "globalThis.eval('layout.key = 2');",
            "lay\\u006fut.key = 9;",
            "ev\\u0061l('layout.key = 2');",
        ] {
            let escaped_before = format!("{before} {escape}");
            let escaped_after = format!("{after} {escape}");
            assert!(evidence(&escaped_before, after).is_empty(), "old: {escape}");
            assert!(evidence(before, &escaped_after).is_empty(), "new: {escape}");
        }
    }

    #[test]
    fn object_member_changes_reject_type_annotations_and_jsdoc_tags() {
        for prefix in [
            "type Layout = { key: number, level: number }; export const layout: Layout",
            "export const layout: { key: number, level: number }",
            "/** @type {Layout} */ export const layout",
            "/** @satisfies {Layout} */ export const layout",
            "/** @enum {number} */ export const layout",
            "/** @template T */ export const layout",
        ] {
            let before = format!("{prefix} = {{ key: 1, level: 10 }};");
            let after = format!("{prefix} = {{ key: 1, level: 20 }};");
            assert!(evidence(&before, &after).is_empty(), "{prefix}");
        }
        let before = "/** Layout documentation. */ export const layout = { key: 1, level: 10 };";
        let after = "/** Layout documentation. */ export const layout = { key: 1, level: 20 };";
        assert!(evidence(before, after)["layout"].read_is_unchanged("key"));
        for tag in [
            "@type {Layout}",
            "@satisfies {Layout}",
            "@typedef {Object} Layout",
        ] {
            let old_tag = format!("{before} /** {tag} */");
            let new_tag = format!("{after} /** {tag} */");
            assert!(evidence(&old_tag, after).is_empty(), "old: {tag}");
            assert!(evidence(before, &new_tag).is_empty(), "new: {tag}");
        }
    }

    #[test]
    fn object_member_node_kinds_exist_in_each_supported_grammar() {
        for lang in LANGS {
            for kind in [
                "object",
                "pair",
                "property_identifier",
                "string",
                "number",
                "true",
                "false",
                "null",
                "variable_declarator",
                "identifier",
                "lexical_declaration",
                "export_statement",
                "member_expression",
                "return_statement",
                "expression_statement",
                "arguments",
                "array",
                "binary_expression",
                "ternary_expression",
                "template_substitution",
                "shorthand_property_identifier",
                "export_specifier",
                "import_specifier",
                "comment",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), &format!("({kind}) @node"))
                    .unwrap_or_else(|error| panic!("{lang:?}/{kind}: {error}"));
            }
        }
    }

    #[test]
    fn direct_member_reads_agree_for_javascript_typescript_and_tsx() {
        for lang in LANGS {
            for (source, expected) in [
                ("layout.options.key;", Some("options.key")),
                ("const value = layout.key;", Some("key")),
                ("consume(layout.key);", Some("key")),
                ("layout?.key;", None),
                ("layout.key();", None),
                ("layout.key = 2;", None),
                ("[layout.key] = [2];", None),
                ("({ k: layout.key } = source);", None),
                ("for (layout.key of values) {}", None),
                ("layout.key++;", None),
                ("delete layout.key;", None),
                ("layout.key[index];", None),
                ("other.layout.key;", None),
                ("(layout.key);", None),
            ] {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{lang:?}: {source}");
                let start = source.find("layout").unwrap();
                let node = tree
                    .root_node()
                    .descendant_for_byte_range(start, start + 6)
                    .unwrap();
                assert_eq!(
                    direct_member_read(node, source.as_bytes()).as_deref(),
                    expected,
                    "{lang:?}: {source}"
                );
            }
        }
    }
}
