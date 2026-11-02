//! Proof for a Rust file-root item whose only edit widens private visibility.
//!
//! The byte comparison is deliberately strict: formatting, attributes, or
//! another edit in the file make the old binding uncertain.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use tree_sitter::Node;

use crate::engine::{diff, parser};
use crate::language::LangId;
use crate::models::impact::AffectedSymbol;

const DECLARATION_KINDS: &[&str] = &[
    "struct_item",
    "enum_item",
    "union_item",
    "trait_item",
    "type_item",
    "function_item",
    "const_item",
    "static_item",
];

/// 宣言自身の可視性。修飾子が無い宣言は宣言モジュールの中だけから見える (`pub(self)` と
/// 同じ範囲)。修飾子はあるが解釈できないものは、この型にせず証明をやめる。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Visibility {
    Private,
    Public,
    Restricted(Vec<String>),
}

impl Visibility {
    /// 宣言モジュール `module` を基準にした可視範囲。祖先でない path や root を越える
    /// `super` は解決できない (`None`)。
    fn boundary(&self, module: &[String]) -> Option<Boundary> {
        match self {
            Self::Private => Some(Boundary::Module(module.to_vec())),
            Self::Public => Some(Boundary::Public),
            Self::Restricted(path) => resolve_visibility_path(path, module).map(Boundary::Module),
        }
    }

    /// `pub(crate)` (`pub(in crate)`) か `pub`。private からこれらへの拡大は
    /// `private_widening_names` が扱う。
    fn is_crate_or_public(&self) -> bool {
        match self {
            Self::Public => true,
            Self::Restricted(path) => *path == ["crate"],
            Self::Private => false,
        }
    }
}

/// 宣言モジュールを基準に解決した可視範囲。`Module` はそのモジュールの部分木。
#[derive(Debug, PartialEq, Eq)]
enum Boundary {
    Public,
    Module(Vec<String>),
}

impl Boundary {
    fn contains(&self, module: &[String]) -> bool {
        match self {
            Self::Public => true,
            Self::Module(path) => module.starts_with(path),
        }
    }

    /// `other` の範囲をすべて含む (同じか広い)。
    fn covers(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Public, _) => true,
            (Self::Module(_), Self::Public) => false,
            (Self::Module(path), Self::Module(other)) => other.starts_with(path),
        }
    }
}

/// 1 つの宣言の可視性の書き換え。
#[derive(Clone, Debug)]
struct VisibilityEdit {
    old: Visibility,
    new: Visibility,
}

/// 可視性だけを広げた宣言から見た、参照モジュールの位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WideningScope {
    /// 旧範囲の中。変更前から宣言を参照できた。
    InsideOld,
    /// 新しい範囲の中で旧範囲の外。
    NewlyVisible,
    /// 新しい範囲の外。変更の前も後も宣言を参照できない。
    OutsideNew,
}

#[derive(Clone, Debug)]
pub(super) struct ExplicitWidening {
    edit: VisibilityEdit,
    /// 同じファイルで可視性を書き換えた宣言すべて (この宣言を含む)。
    file_edits: Arc<[VisibilityEdit]>,
}

impl ExplicitWidening {
    /// 宣言モジュール `module` から見て、参照モジュール `reference` がどの範囲に入るか。
    /// 両方が同じ crate に属すると確かめてから呼ぶ。
    ///
    /// ファイル内の書き換えのどれかの範囲を解決できないか縮小なら、またこの宣言の新しい
    /// 範囲が旧範囲より厳密に広くなければ (crate root での private と `pub(crate)` のように
    /// 範囲が同じなら) `None`。
    pub(super) fn classify(
        &self,
        module: &[String],
        reference: &[String],
    ) -> Option<WideningScope> {
        for edit in self.file_edits.iter() {
            let old = edit.old.boundary(module)?;
            if !edit.new.boundary(module)?.covers(&old) {
                return None;
            }
        }
        let old = self.edit.old.boundary(module)?;
        let new = self.edit.new.boundary(module)?;
        if !new.covers(&old) || old.covers(&new) {
            return None;
        }
        Some(if old.contains(reference) {
            WideningScope::InsideOld
        } else if new.contains(reference) {
            WideningScope::NewlyVisible
        } else {
            WideningScope::OutsideNew
        })
    }
}

fn resolve_visibility_path(path: &[String], module: &[String]) -> Option<Vec<String>> {
    let (first, rest) = path.split_first()?;
    let mut resolved = match first.as_str() {
        "crate" => Vec::new(),
        "self" => module.to_vec(),
        "super" => {
            let mut parent = module.to_vec();
            parent.pop()?;
            parent
        }
        _ => return None,
    };
    let mut leading_super = first == "super";
    for part in rest {
        match part.as_str() {
            "super" if leading_super => {
                resolved.pop()?;
            }
            "crate" | "self" | "super" => return None,
            _ => {
                leading_super = false;
                resolved.push(part.clone());
            }
        }
    }
    module.starts_with(&resolved).then_some(resolved)
}

fn visibility_path(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    match node.kind() {
        "identifier" => {
            let name = node.utf8_text(source).ok()?;
            let name = name.strip_prefix("r#").unwrap_or(name);
            (!matches!(name, "crate" | "self" | "super" | "Self")).then(|| vec![name.to_owned()])
        }
        "crate" | "self" | "super" => Some(vec![node.utf8_text(source).ok()?.to_owned()]),
        "scoped_identifier" => {
            let mut path = visibility_path(node.child_by_field_name("path")?, source)?;
            path.extend(visibility_path(node.child_by_field_name("name")?, source)?);
            Some(path)
        }
        _ => None,
    }
}

/// 宣言自身の可視性修飾子 (宣言の直接の子)。フィールドやメソッドの修飾子は含まない。
fn visibility_modifier(item: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = item.walk();
    item.named_children(&mut cursor)
        .find(|node| node.kind() == "visibility_modifier")
}

fn visibility<'a>(item: Node<'a>, source: &[u8]) -> Option<(Node<'a>, Visibility)> {
    let node = visibility_modifier(item)?;
    let mut cursor = node.walk();
    let mut children = node
        .named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"));
    let value = match children.next() {
        None => Visibility::Public,
        Some(path) if children.next().is_none() => {
            Visibility::Restricted(visibility_path(path, source)?)
        }
        _ => return None,
    };
    Some((node, value))
}

/// 宣言の可視性。修飾子なしは `Private`。修飾子はあるが解釈できない (`pub(in Self)` など)
/// ときは `None`。
fn declared_visibility(item: Node<'_>, source: &[u8]) -> Option<Visibility> {
    if visibility_modifier(item).is_none() {
        return Some(Visibility::Private);
    }
    visibility(item, source).map(|(_, value)| value)
}

/// 取り除く修飾子の範囲。旧新の片側にしか修飾子が無いときは、次のトークンとの間の空白も含める。
fn modifier_range(node: Node<'_>, source: &[u8], with_space: bool) -> Range<usize> {
    let mut end = node.end_byte();
    while with_space && end < source.len() && source[end].is_ascii_whitespace() {
        end += 1;
    }
    node.start_byte()..end
}

/// 昇順で重ならない `ranges` を取り除いた byte 列。
fn without_ranges(source: &[u8], ranges: &[Range<usize>]) -> Vec<u8> {
    let mut result = Vec::with_capacity(source.len());
    let mut start = 0;
    for range in ranges {
        result.extend_from_slice(&source[start..range.start]);
        start = range.end;
    }
    result.extend_from_slice(&source[start..]);
    result
}

/// 旧新の違いがファイル直下の宣言自身の可視性修飾子だけなら、書き換えた宣言の
/// (名前, kind, 旧新の可視性) をソースの順に返す。
///
/// 直下の項目をソースの順・kind・名前で対応付け、書き換えた修飾子を取り除いた残り
/// (本文・属性・コメント・宣言の順) が byte で一致することを求める。フィールド・メソッド・
/// inline module の中・`use` の修飾子は取り除かないので、それらが変わると一致しない。
/// 書き換えた修飾子を解釈できないときも `None` (証明しない)。
fn visibility_only_edits<'a>(
    old_root: Node<'_>,
    old_source: &[u8],
    new_root: Node<'a>,
    new_source: &'a [u8],
) -> Option<Vec<(&'a str, &'a str, VisibilityEdit)>> {
    let mut cursor = old_root.walk();
    let old_items = old_root.named_children(&mut cursor).collect::<Vec<_>>();
    let mut cursor = new_root.walk();
    let new_items = new_root.named_children(&mut cursor).collect::<Vec<_>>();
    if old_items.len() != new_items.len() {
        return None;
    }
    let mut old_removed = Vec::new();
    let mut new_removed = Vec::new();
    let mut edits = Vec::new();
    for (old_item, new_item) in old_items.into_iter().zip(new_items) {
        if old_item.kind() != new_item.kind() {
            return None;
        }
        if !DECLARATION_KINDS.contains(&new_item.kind()) {
            continue;
        }
        let name = new_item
            .child_by_field_name("name")?
            .utf8_text(new_source)
            .ok()?;
        if old_item
            .child_by_field_name("name")?
            .utf8_text(old_source)
            .ok()?
            != name
        {
            return None;
        }
        let old_modifier = visibility_modifier(old_item);
        let new_modifier = visibility_modifier(new_item);
        if old_modifier.map(|node| &old_source[node.byte_range()])
            == new_modifier.map(|node| &new_source[node.byte_range()])
        {
            continue;
        }
        let edit = VisibilityEdit {
            old: declared_visibility(old_item, old_source)?,
            new: declared_visibility(new_item, new_source)?,
        };
        let one_sided = old_modifier.is_none() || new_modifier.is_none();
        if let Some(node) = old_modifier {
            old_removed.push(modifier_range(node, old_source, one_sided));
        }
        if let Some(node) = new_modifier {
            new_removed.push(modifier_range(node, new_source, one_sided));
        }
        edits.push((name, new_item.kind(), edit));
    }
    (without_ranges(old_source, &old_removed) == without_ranges(new_source, &new_removed))
        .then_some(edits)
}

fn unique_declaration<'a>(
    root: Node<'a>,
    source: &[u8],
    name: &str,
    kind: &str,
) -> Option<Node<'a>> {
    let mut cursor = root.walk();
    let mut nodes = root.named_children(&mut cursor).filter(|node| {
        node.kind() == kind
            && node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                == Some(name)
    });
    let item = nodes.next()?;
    nodes.next().is_none().then_some(item)
}

/// ファイル直下の宣言の可視性だけを書き換えた差分から、名前ごとの拡大の証明を作る。
/// 旧側の修飾子なし (private) から `pub(super)` / `pub(in ...)` への書き換えと、明示の
/// `pub(...)` からの書き換えを扱う。範囲が実際に広がるかは、宣言モジュールが決まってから
/// `ExplicitWidening::classify` で確かめる。
pub(super) fn explicit_widenings(
    file_diff: &str,
    file_path: &str,
    source: &[u8],
    root: Node<'_>,
    affected: &[AffectedSymbol],
) -> HashMap<String, ExplicitWidening> {
    let mut result = HashMap::new();
    if root.has_error() {
        return result;
    }
    let Some(old) = diff::reconstruct_old_source(file_diff, file_path, source) else {
        return result;
    };
    let Ok(old_tree) = parser::parse_source(&old.source, LangId::Rust) else {
        return result;
    };
    let old_root = old_tree.root_node();
    if old_root.has_error() {
        return result;
    }
    let Some(edits) = visibility_only_edits(old_root, &old.source, root, source) else {
        return result;
    };
    let file_edits = edits
        .iter()
        .map(|(_, _, edit)| edit.clone())
        .collect::<Arc<[_]>>();
    for changed in affected
        .iter()
        .filter(|item| item.change_type == "modified")
    {
        // 同じ名前の宣言を 2 つ以上書き換えた (型と関数の同名など) ときは、参照がどちらの
        // 範囲で決まるか分からない。数えるのは書き換えた宣言で、証明できた宣言ではない。
        let mut named = edits.iter().filter(|(name, _, _)| *name == changed.name);
        let Some((_, kind, edit)) = named.next() else {
            continue;
        };
        if named.next().is_some()
            // 型と値で同じ名前を使えるので、一意性は宣言の kind ごとに確認する。
            || unique_declaration(root, source, &changed.name, kind).is_none()
            // private から `pub(crate)` / `pub` への拡大は `private_widening_names` が扱う。
            || (edit.old == Visibility::Private && edit.new.is_crate_or_public())
            || edit.new == Visibility::Private
        {
            continue;
        }
        result.insert(
            changed.name.clone(),
            ExplicitWidening {
                edit: edit.clone(),
                file_edits: Arc::clone(&file_edits),
            },
        );
    }
    result
}

pub(super) fn private_widening_names(
    file_diff: &str,
    file_path: &str,
    source: &[u8],
    root: Node<'_>,
    affected: &[AffectedSymbol],
) -> HashSet<String> {
    if root.has_error() {
        return HashSet::new();
    }
    let Some(old) = diff::reconstruct_old_source(file_diff, file_path, source) else {
        return HashSet::new();
    };
    let Ok(old_tree) = parser::parse_source(&old.source, LangId::Rust) else {
        return HashSet::new();
    };
    if old_tree.root_node().has_error() {
        return HashSet::new();
    }

    let mut result = HashSet::new();
    for changed in affected
        .iter()
        .filter(|item| item.change_type == "modified")
    {
        let mut cursor = root.walk();
        let mut matches = root.named_children(&mut cursor).filter(|node| {
            matches!(
                node.kind(),
                "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "trait_item"
                    | "type_item"
                    | "function_item"
                    | "const_item"
                    | "static_item"
            ) && node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                == Some(changed.name.as_str())
        });
        let Some(item) = matches.next() else { continue };
        if matches.next().is_some() {
            continue;
        }
        let mut children = item.walk();
        let Some(visibility) = item
            .named_children(&mut children)
            .find(|child| child.kind() == "visibility_modifier")
        else {
            continue;
        };
        if !matches!(visibility.utf8_text(source), Ok("pub(crate)" | "pub")) {
            continue;
        }
        let mut end = visibility.end_byte();
        while end < source.len() && source[end].is_ascii_whitespace() {
            end += 1;
        }
        let mut without_visibility =
            Vec::with_capacity(source.len() - (end - visibility.start_byte()));
        without_visibility.extend_from_slice(&source[..visibility.start_byte()]);
        without_visibility.extend_from_slice(&source[end..]);
        if without_visibility == old.source {
            result.insert(changed.name.clone());
        }
    }
    result
}

/// A newly visible item can enter scope through a glob import. In that case
/// even an unchanged reference to another item might become ambiguous.
pub(super) fn has_glob_import(source: &[u8]) -> bool {
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "use_declaration" && source[node.byte_range()].contains(&b'*') {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

/// 親以外からの glob は、公開範囲の拡大で新しい同名束縛を取り込み得る。
pub(super) fn has_non_parent_glob(source: &[u8]) -> bool {
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "use_wildcard" {
            let mut cursor = node.walk();
            let mut children = node
                .named_children(&mut cursor)
                .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"));
            if !node
                .parent()
                .is_some_and(|parent| parent.kind() == "use_declaration")
                || children.next().is_none_or(|child| child.kind() != "super")
                || children.next().is_some()
            {
                return true;
            }
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

#[cfg(test)]
mod explicit_tests {
    use super::*;

    #[test]
    fn explicit_visibility_grammar_nodes_and_fields_exist() {
        let language = LangId::Rust.ts_language();
        for kind in DECLARATION_KINDS {
            tree_sitter::Query::new(&language, &format!("({kind} name: (_) @name)")).unwrap();
        }
        for query in [
            "(visibility_modifier)",
            "(use_declaration argument: (use_wildcard (super)))",
            "(mod_item name: (identifier))",
            "(scoped_identifier path: (_) @path name: (_) @name)",
            "(identifier)",
            "(crate)",
            "(self)",
            "(super)",
        ] {
            tree_sitter::Query::new(&language, query).unwrap();
        }
    }

    #[test]
    fn private_widening_globs_require_a_direct_parent_path() {
        for source in [
            "use super::*;",
            "use super /* parent */ :: *;",
            "pub(crate) use super::*;",
            "use super::item;",
            "const TEXT: &str = \"use other::*;\"; // use other::*;",
        ] {
            assert!(!has_non_parent_glob(source.as_bytes()), "{source}");
        }
        for source in [
            "use other::*;",
            "use super::other::*;",
            "use crate::*;",
            "use super::{*};",
            "use super::{item, other::*};",
            "fn broken(",
        ] {
            assert!(has_non_parent_glob(source.as_bytes()), "{source}");
        }
    }

    #[test]
    fn explicit_visibility_all_root_declaration_kinds_use_the_same_byte_proof() {
        for declaration in [
            "struct Item { value: bool }",
            "enum Item {}",
            "union Item { value: u32 }",
            "trait Item {}",
            "type Item = u32;",
            "fn Item() {}",
            "const Item: u32 = 0;",
            "static Item: u32 = 0;",
        ] {
            let old = format!("pub(crate) {declaration}\n");
            let new = format!("pub {declaration}\n");
            let patch = format!("--- a/src/item.rs\n+++ b/src/item.rs\n@@ -1 +1 @@\n-{old}+{new}");
            let tree = parser::parse_source(new.as_bytes(), LangId::Rust).unwrap();
            let changed = [AffectedSymbol {
                name: "Item".into(),
                kind: "function".into(),
                change_type: "modified".into(),
            }];
            let proofs = explicit_widenings(
                &patch,
                "src/item.rs",
                new.as_bytes(),
                tree.root_node(),
                &changed,
            );
            assert_eq!(proofs.len(), 1, "{declaration}");
            assert_eq!(
                proofs["Item"].classify(&module(&["a"]), &module(&["b"])),
                Some(WideningScope::InsideOld),
                "{declaration}"
            );
        }
    }

    #[test]
    fn explicit_visibility_boundaries_reject_non_ancestors_and_illegal_paths() {
        let module = ["a".into(), "b".into()];
        for (path, expected) in [
            (vec!["crate", "a"], Some(vec!["a"])),
            (vec!["super", "super"], Some(vec![])),
            (vec!["super", "b"], Some(vec!["a", "b"])),
            (vec!["self"], Some(vec!["a", "b"])),
            (vec!["crate", "missing"], None),
            (vec!["crate", "super"], None),
            (vec!["self", "super"], None),
            (vec!["super", "b", "super"], None),
            (vec!["super", "super", "super"], None),
        ] {
            let parts = path
                .iter()
                .map(|part| (*part).to_owned())
                .collect::<Vec<_>>();
            let expected =
                expected.map(|parts| parts.into_iter().map(str::to_owned).collect::<Vec<_>>());
            assert_eq!(
                resolve_visibility_path(&parts, &module),
                expected,
                "{path:?}"
            );
        }
    }

    fn module(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    /// `old` を `new` へ書き換えた 1 ファイルの差分から、`names` の変更の証明を作る。
    fn widenings(old: &str, new: &str, names: &[&str]) -> HashMap<String, ExplicitWidening> {
        let mut patch = format!(
            "--- a/src/item.rs\n+++ b/src/item.rs\n@@ -1,{} +1,{} @@\n",
            old.lines().count(),
            new.lines().count()
        );
        for line in old.lines() {
            patch.push_str(&format!("-{line}\n"));
        }
        for line in new.lines() {
            patch.push_str(&format!("+{line}\n"));
        }
        let tree = parser::parse_source(new.as_bytes(), LangId::Rust).unwrap();
        let changed = names
            .iter()
            .map(|name| AffectedSymbol {
                name: (*name).into(),
                kind: "function".into(),
                change_type: "modified".into(),
            })
            .collect::<Vec<_>>();
        explicit_widenings(
            &patch,
            "src/item.rs",
            new.as_bytes(),
            tree.root_node(),
            &changed,
        )
    }

    /// 宣言モジュール `crate::numeric::ascii` の parse を、`outside` から見た分類。
    fn outside_scope(
        proofs: &HashMap<String, ExplicitWidening>,
        name: &str,
        outside: &[&str],
    ) -> Option<WideningScope> {
        proofs
            .get(name)?
            .classify(&module(&["numeric", "ascii"]), &module(outside))
    }

    #[test]
    fn explicit_visibility_private_old_side_is_the_declaring_module() {
        // 修飾子なしの旧側は宣言モジュールの部分木だけから見える (`pub(self)` と同じ)。
        for new in [
            "pub(super) fn parse() {}\n",
            "pub(in crate::numeric) fn parse() {}\n",
        ] {
            let proofs = widenings("fn parse() {}\n", new, &["parse"]);
            for (reference, expected) in [
                (&["numeric", "ascii"][..], WideningScope::InsideOld),
                (&["numeric", "ascii", "child"][..], WideningScope::InsideOld),
                (&["numeric"][..], WideningScope::NewlyVisible),
                (&["numeric", "sibling"][..], WideningScope::NewlyVisible),
                (&["unrelated"][..], WideningScope::OutsideNew),
                (&[][..], WideningScope::OutsideNew),
            ] {
                assert_eq!(
                    outside_scope(&proofs, "parse", reference),
                    Some(expected),
                    "{new} {reference:?}"
                );
            }
        }
    }

    #[test]
    fn explicit_visibility_needs_a_strictly_wider_scope() {
        // 範囲が同じ書き換えは拡大ではない。
        for new in [
            "pub(self) fn parse() {}\n",
            "pub(in self) fn parse() {}\n",
            "pub(in crate::numeric::ascii) fn parse() {}\n",
        ] {
            let proofs = widenings("fn parse() {}\n", new, &["parse"]);
            assert_eq!(
                outside_scope(&proofs, "parse", &["unrelated"]),
                None,
                "{new}"
            );
        }
        // crate root の宣言では、private (pub(self)) と pub(crate) の範囲は同じ。
        let proofs = widenings(
            "pub(self) fn parse() {}\n",
            "pub(crate) fn parse() {}\n",
            &["parse"],
        );
        assert_eq!(proofs["parse"].classify(&[], &module(&["other"])), None);
        assert_eq!(
            proofs["parse"].classify(&module(&["a"]), &module(&["other"])),
            Some(WideningScope::NewlyVisible)
        );
        // crate root から root を越える super は解決できない。
        let proofs = widenings("fn parse() {}\n", "pub(super) fn parse() {}\n", &["parse"]);
        assert_eq!(proofs["parse"].classify(&[], &module(&["other"])), None);
        // private から pub(crate) / pub への拡大は private_widening_names が扱う。
        for new in [
            "pub(crate) fn parse() {}\n",
            "pub(in crate) fn parse() {}\n",
            "pub fn parse() {}\n",
        ] {
            assert!(
                widenings("fn parse() {}\n", new, &["parse"]).is_empty(),
                "{new}"
            );
        }
        // 縮小は証明しない。
        assert!(widenings("pub(super) fn parse() {}\n", "fn parse() {}\n", &["parse"]).is_empty());
        let proofs = widenings(
            "pub(crate) fn parse() {}\n",
            "pub(super) fn parse() {}\n",
            &["parse"],
        );
        assert_eq!(outside_scope(&proofs, "parse", &["unrelated"]), None);
    }

    #[test]
    fn explicit_visibility_allows_only_root_visibility_edits_in_the_file() {
        let old = "struct Number;\nfn parse(_: &str) -> Number { Number }\nfn readings(_: &str, _: Number) -> usize { 1 }\n";
        let new = "pub(super) struct Number;\npub(super) fn parse(_: &str) -> Number { Number }\npub(super) fn readings(_: &str, _: Number) -> usize { 1 }\n";
        let names = ["Number", "parse", "readings"];
        let proofs = widenings(old, new, &names);
        for name in names {
            assert_eq!(
                outside_scope(&proofs, name, &["unrelated"]),
                Some(WideningScope::OutsideNew),
                "{name}"
            );
        }
        // 可視性以外の違いが 1 つでも混ざれば、ファイルのどの宣言も証明しない。
        for (label, changed) in [
            ("body", new.replace("{ 1 }", "{ 2 }")),
            (
                "parameter",
                new.replace("fn parse(_: &str)", "fn parse(_: &str, _: bool)"),
            ),
            (
                "attribute",
                new.replace("pub(super) fn parse", "#[inline]\npub(super) fn parse"),
            ),
            ("comment", format!("// note\n{new}")),
            ("use", format!("use std::fmt;\n{new}")),
            ("space", new.replace("Number;", "Number ;")),
        ] {
            assert!(widenings(old, &changed, &names).is_empty(), "{label}");
        }
        // 宣言の順が変われば対応付けない。
        assert!(
            widenings(
                "fn a() {}\nfn b() {}\n",
                "pub(super) fn b() {}\nfn a() {}\n",
                &["a", "b"]
            )
            .is_empty()
        );
        // 宣言自身以外の修飾子 (フィールド・メソッド・inline module の中・use) は取り除かない。
        for (label, old, new) in [
            (
                "field",
                "struct Number { value: u8 }\nfn parse() {}\n",
                "struct Number { pub(super) value: u8 }\npub(super) fn parse() {}\n",
            ),
            (
                "method",
                "struct Number;\nimpl Number { fn new() -> Self { Number } }\nfn parse() {}\n",
                "struct Number;\nimpl Number { pub(super) fn new() -> Self { Number } }\npub(super) fn parse() {}\n",
            ),
            (
                "inline-module",
                "mod inner { fn helper() {} }\nfn parse() {}\n",
                "mod inner { pub(super) fn helper() {} }\npub(super) fn parse() {}\n",
            ),
            (
                "use",
                "use std::fmt;\nfn parse() {}\n",
                "pub(super) use std::fmt;\npub(super) fn parse() {}\n",
            ),
        ] {
            assert!(widenings(old, new, &["parse"]).is_empty(), "{label}");
        }
    }

    #[test]
    fn explicit_visibility_rejects_same_name_edits_narrowing_and_unknown_modifiers() {
        // 型と関数の両方を書き換えると、名前 parse は証明しない (書き換えた宣言の数で数える)。
        let proofs = widenings(
            "struct parse {}\nfn parse() {}\nstruct Number;\n",
            "pub(super) struct parse {}\npub(super) fn parse() {}\npub(super) struct Number;\n",
            &["parse", "Number"],
        );
        assert!(!proofs.contains_key("parse"));
        assert_eq!(
            outside_scope(&proofs, "Number", &["unrelated"]),
            Some(WideningScope::OutsideNew)
        );
        // 書き換えていない同名の型は妨げない。
        let proofs = widenings(
            "struct parse {}\nfn parse() {}\n",
            "struct parse {}\npub(super) fn parse() {}\n",
            &["parse"],
        );
        assert_eq!(
            outside_scope(&proofs, "parse", &["unrelated"]),
            Some(WideningScope::OutsideNew)
        );
        // cfg で分けた同 kind の同名宣言は一意でない。
        assert!(
            widenings(
                "#[cfg(unix)]\nfn parse() {}\n#[cfg(not(unix))]\nfn parse() {}\n",
                "#[cfg(unix)]\npub(super) fn parse() {}\n#[cfg(not(unix))]\nfn parse() {}\n",
                &["parse"]
            )
            .is_empty()
        );
        // 縮小が 1 つでも混ざれば、ファイルのどの宣言も証明しない。
        let proofs = widenings(
            "pub(crate) struct Number;\nfn parse() {}\n",
            "pub(super) struct Number;\npub(super) fn parse() {}\n",
            &["parse", "Number"],
        );
        assert_eq!(outside_scope(&proofs, "parse", &["unrelated"]), None);
        assert_eq!(outside_scope(&proofs, "Number", &["unrelated"]), None);
        // 書き換えた修飾子を解釈できなければ証明しない。書き換えていない修飾子は妨げない。
        for new in [
            "pub(in Self) fn parse() {}\nfn other() {}\n",
            "pub(super) fn parse() {}\npub(in ::other) fn other() {}\n",
        ] {
            assert!(
                widenings("fn parse() {}\nfn other() {}\n", new, &["parse"]).is_empty(),
                "{new}"
            );
        }
        let proofs = widenings(
            "fn parse() {}\npub(in Self) fn other() {}\n",
            "pub(super) fn parse() {}\npub(in Self) fn other() {}\n",
            &["parse"],
        );
        assert_eq!(
            outside_scope(&proofs, "parse", &["unrelated"]),
            Some(WideningScope::OutsideNew)
        );
    }

    #[test]
    fn explicit_visibility_accepts_modifier_spellings() {
        for new in [
            "pub(super)fn parse() {}\n",
            "pub(/* parent */ super) fn parse() {}\n",
            "pub(in super) fn parse() {}\n",
            "pub(in crate::r#numeric) fn parse() {}\n",
        ] {
            let proofs = widenings("fn parse() {}\n", new, &["parse"]);
            assert_eq!(
                outside_scope(&proofs, "parse", &["unrelated"]),
                Some(WideningScope::OutsideNew),
                "{new}"
            );
        }
        // 旧新の両方に修飾子があるときは、修飾子の外の空白も含めて一致を求める。
        assert!(
            widenings(
                "pub(crate) fn parse() {}\n",
                "pub  fn parse() {}\n",
                &["parse"]
            )
            .is_empty()
        );
    }

    #[test]
    fn explicit_visibility_restricted_paths_resolve_from_the_declaring_module() {
        let declaring = module(&["a", "b"]);
        for (new, expected) in [
            ("pub(in crate::a)", Some(WideningScope::OutsideNew)),
            ("pub(in crate::r#a)", Some(WideningScope::OutsideNew)),
            ("pub(in super::super)", Some(WideningScope::NewlyVisible)),
            // 祖先でない path と root を越える super は解決できない。
            ("pub(in crate::c)", None),
            ("pub(in super::super::super)", None),
        ] {
            let proofs = widenings(
                "fn parse() {}\n",
                &format!("{new} fn parse() {{}}\n"),
                &["parse"],
            );
            assert_eq!(
                proofs
                    .get("parse")
                    .and_then(|proof| proof.classify(&declaring, &module(&["outside"]))),
                expected,
                "{new}"
            );
        }
    }

    #[test]
    fn explicit_visibility_public_scope_never_excludes() {
        let proofs = widenings(
            "pub(super) fn parse() {}\n",
            "pub fn parse() {}\n",
            &["parse"],
        );
        let declaring = module(&["a", "b"]);
        for (reference, expected) in [
            (&["a", "c"][..], WideningScope::InsideOld),
            (&["outside"][..], WideningScope::NewlyVisible),
            (&[][..], WideningScope::NewlyVisible),
        ] {
            assert_eq!(
                proofs["parse"].classify(&declaring, &module(reference)),
                Some(expected),
                "{reference:?}"
            );
        }
    }

    #[test]
    fn explicit_visibility_one_sided_modifier_allows_only_the_following_whitespace() {
        // 片側にしか修飾子が無いときは、修飾子と次のトークンの間の空白 (改行・タブを含む) を
        // 取り除いて比べる。
        let proofs = widenings(
            "fn parse() {}\n",
            "pub(super)\n\tfn parse() {}\n",
            &["parse"],
        );
        assert_eq!(
            outside_scope(&proofs, "parse", &["unrelated"]),
            Some(WideningScope::OutsideNew)
        );
        // コメント・inner attribute・item 位置のマクロの追加や変更が混ざれば証明しない。
        for (label, old, new) in [
            (
                "comment-after-modifier",
                "fn parse() {}\n",
                "pub(super) /* note */ fn parse() {}\n",
            ),
            (
                "comment-change",
                "// before\nfn parse() {}\n",
                "// after\npub(super) fn parse() {}\n",
            ),
            (
                "inner-attribute-added",
                "fn parse() {}\n",
                "#![allow(dead_code)]\npub(super) fn parse() {}\n",
            ),
            (
                "inner-attribute-change",
                "#![allow(dead_code)]\nfn parse() {}\n",
                "#![allow(unused)]\npub(super) fn parse() {}\n",
            ),
            (
                "item-macro-added",
                "fn parse() {}\n",
                "pub(super) fn parse() {}\nmake_items!();\n",
            ),
            (
                "item-macro-change",
                "make_items!(a);\nfn parse() {}\n",
                "make_items!(b);\npub(super) fn parse() {}\n",
            ),
        ] {
            assert!(widenings(old, new, &["parse"]).is_empty(), "{label}");
        }
    }

    #[test]
    fn explicit_visibility_explicit_and_private_old_sides_classify_alike() {
        // 明示の pub(self) からの拡大も、修飾子なしからの拡大と同じく 4 分類する。
        let explicit = widenings(
            "pub(self) fn parse() {}\n",
            "pub(super) fn parse() {}\n",
            &["parse"],
        );
        let private = widenings("fn parse() {}\n", "pub(super) fn parse() {}\n", &["parse"]);
        for proofs in [&explicit, &private] {
            for (reference, expected) in [
                (&["numeric", "ascii", "child"][..], WideningScope::InsideOld),
                (&["numeric", "sibling"][..], WideningScope::NewlyVisible),
                (&["unrelated"][..], WideningScope::OutsideNew),
            ] {
                assert_eq!(
                    outside_scope(proofs, "parse", reference),
                    Some(expected),
                    "{reference:?}"
                );
            }
        }
        // 宣言モジュールが ["a"] なら pub(super) は crate root まで届くので、crate root 直下の
        // 別のモジュールも新しい範囲の中 (NewlyVisible) で、範囲の外にはならない。
        let declaring = module(&["a"]);
        for (reference, expected) in [
            (&["a", "child"][..], WideningScope::InsideOld),
            (&["b"][..], WideningScope::NewlyVisible),
            (&[][..], WideningScope::NewlyVisible),
        ] {
            assert_eq!(
                private["parse"].classify(&declaring, &module(reference)),
                Some(expected),
                "{reference:?}"
            );
        }
    }
}
