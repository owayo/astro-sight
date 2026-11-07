//! Rust の公開構造体の公開の形 (Issue #270)。
//!
//! `pub struct` の API の signature を、宣言の先頭行から、外の利用者に見える形 (公開フィールドの
//! 名前か位置と型、フィールドの `#[cfg]`、外から構築できるか、`#[repr]`・`#[non_exhaustive]`、型の
//! 引数と `where`) の表示に広げる。旧新の形は Cargo の SemVer の手引き
//! (<https://doc.rust-lang.org/cargo/reference/semver.html>) に沿って比べる。
//!
//! - 壊す (blocking): 公開フィールドの削除・改名・型の変更・公開でなくすこと・cfg の変更、外から
//!   構築できる構造体への公開フィールドの追加、外から構築できなくすること、タプルの公開フィールドの
//!   位置を変えること (前のフィールドの cfg による移動を含む)、形と可視性の変更、型の引数の削除・
//!   既定値の無い追加・境界の付いた追加・既定値の変更と削除・トレイトの境界を強めること・寿命の
//!   境界の付け外し (利用側の暗黙の境界になる。フィールドから推論される境界は書かなくてもある
//!   ものとして比べる)、`#[repr]` を外すことと `repr(packed)`・`repr(align)` の付け外し、
//!   `#[repr]` の構造体の公開フィールドの並べ替えと、旧い公開フィールドより前への公開フィールドの
//!   追加
//! - 互換 (mod_compat): 構築の制限を緩めること、外から構築できない構造体への公開フィールドの追加、
//!   トレイトの境界を緩めること、型の引数に既定値を付けること、境界の無い既定値の付いた型の引数を
//!   足すこと (既定値で置き換えて公開フィールドの型が同じで、既定値が見た目から `Sized` で今の
//!   どの利用でも正しい型と言えるとき)、既定の表現に `repr(C)`・`repr(transparent)` を付けること、
//!   公開フィールドの無い構築できない構造体のタプルと名前付きの間の変更
//! - 変更なし: 型の引数の改名 (`for<'a>` の束縛も)、整形とコメント、`r#` の付け外し、`#[repr]` の
//!   書き方 (項目の集合で比べる)、`#[repr]` の無い構造体の名前付きのフィールドの並べ替え、外から
//!   構築できない構造体の公開でないフィールドの変更 (`repr(C)` でも)
//!
//! 寿命の境界は、フィールドの参照から推論されるもの (公開でないフィールドからも。cfg の付いた
//! フィールドからのものは条件付き) を合わせて比べ、推論されるものは signature に
//! `/* implied: .. */` として示す。推論は組み込みの型 (参照・ポインタ・タプル・配列・スライス) だけを
//! たどり、名前の付いた型・関数ポインタ・トレイトオブジェクト・マクロを通る暗黙の境界は追わない
//! (型の検査が要る)。`?Sized` の型の引数に書いた寿命の境界は、渡す `dyn Trait` の既定の寿命も
//! 決めるので、書いたまま比べる。
//!
//! マクロの引数は字面で照合されるので、`r#` もドキュメントコメントも区別して比べる。自動トレイト
//! (`Send`・`Sync` など) と型の引数の分散は、公開でないフィールドやほかの型の中まで含むフィールドの
//! 型から決まるので、構文だけでは追えない (型の検査が要る)。derive と構造体自体の `#[cfg]` は形に
//! 含めない (移動の照合の signature を変えないため)。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};

use super::rust_type_api::*;

/// 公開でないフィールドの表示。無条件のものがあれば `..`、cfg の付いたものだけなら条件ごとに
/// `#[cfg(..)] ..`。
fn private_summary<'a>(fields: impl Iterator<Item = &'a Field>) -> Vec<String> {
    let mut conditional = BTreeMap::new();
    for field in fields.filter(|field| !field.public) {
        if field.conditions.is_empty() {
            return vec!["..".to_owned()];
        }
        conditional
            .entry(condition_keys(&field.conditions))
            .or_insert_with(|| format!("{}..", condition_prefix(&field.conditions)));
    }
    conditional.into_values().collect()
}

/// 構造体 1 つの公開の形。
#[derive(Clone, Debug)]
pub(crate) struct RustStructShape {
    /// 表示する属性 (`#[repr]`・`#[non_exhaustive]` と、それを含む `#[cfg_attr]`)。
    attributes: Vec<String>,
    /// `#[repr]` と、それを含む `#[cfg_attr]`。
    layout: Vec<Attr>,
    /// 無条件の `#[non_exhaustive]`。
    non_exhaustive: bool,
    /// `#[non_exhaustive]` を含む `#[cfg_attr]`。
    conditional_non_exhaustive: Vec<Attr>,
    visibility: Option<Tokens>,
    name: String,
    type_parameters: Option<Tokens>,
    where_clause: Option<Tokens>,
    generics: Generics,
    form: Form,
    /// 宣言の順のフィールド。
    fields: Vec<Field>,
}

/// tree-sitter-rust 0.24 の文法はユニット構造体の `where`
/// (`struct S<const N: usize> where [(); N]: Sized;`) を受け付けず、`where` 節を ERROR に包む。
/// 構文エラーがそれだけなら、その `where` 節。
fn unit_where_clause_in_error(item: Node<'_>) -> Option<Node<'_>> {
    if item.child_by_field_name("body").is_some() {
        return None;
    }
    let children: Vec<Node<'_>> = item.children(&mut item.walk()).collect();
    let mut errors = children.iter().filter(|child| child.is_error());
    let error = *errors.next()?;
    if errors.next().is_some()
        || children
            .iter()
            .any(|child| *child != error && (child.has_error() || child.is_missing()))
    {
        return None;
    }
    let inner: Vec<Node<'_>> = error
        .children(&mut error.walk())
        .filter(|child| !is_comment(*child))
        .collect();
    let [clause] = inner.as_slice() else {
        return None;
    };
    let clause = *clause;
    if clause.kind() != "where_clause" || clause.has_error() {
        return None;
    }
    (children.last()?.kind() == ";").then_some(clause)
}

impl RustStructShape {
    /// `struct_item` の形。読めない構文があれば None。
    pub(crate) fn from_node(item: Node<'_>, source: &[u8]) -> Option<Self> {
        if item.kind() != "struct_item" {
            return None;
        }
        let where_node = if item.has_error() {
            Some(unit_where_clause_in_error(item)?)
        } else {
            item.named_children(&mut item.walk())
                .find(|child| child.kind() == "where_clause")
        };
        let mut attributes = Vec::new();
        let mut layout = Vec::new();
        let mut non_exhaustive = false;
        let mut conditional_non_exhaustive = Vec::new();
        for attribute in preceding_attributes(item) {
            if let Some(attr) = project_attribute(attribute, source, &["repr", "non_exhaustive"])? {
                attributes.push(attr.shown);
            }
            if let Some(attr) = project_attribute(attribute, source, &["repr"])? {
                layout.push(attr);
            }
            if let Some(attr) = project_attribute(attribute, source, &["non_exhaustive"])? {
                if attr.is_conditional() {
                    conditional_non_exhaustive.push(attr);
                } else {
                    non_exhaustive = true;
                }
            }
        }
        let visibility = match item
            .named_children(&mut item.walk())
            .find(|child| child.kind() == "visibility_modifier")
        {
            Some(visibility) => Some(Tokens::of(visibility, source)?),
            None => None,
        };
        let parameters = item.child_by_field_name("type_parameters");
        let type_parameters = match parameters {
            Some(parameters) => Some(Tokens::of(parameters, source)?),
            None => None,
        };
        let where_clause = match where_node {
            Some(clause) => Some(Tokens::of(clause, source)?),
            None => None,
        };
        let body = item.child_by_field_name("body");
        let (form, fields) = match body {
            None => (Form::Unit, Vec::new()),
            Some(body) if body.kind() == "field_declaration_list" => {
                (Form::Named, named_fields(body, source, false)?)
            }
            Some(body) if body.kind() == "ordered_field_declaration_list" => {
                (Form::Tuple, tuple_fields(body, source, false)?)
            }
            Some(_) => return None,
        };
        let mut generics = Generics::parse(parameters, where_node, source)?;
        if let Some(body) = body {
            generics.infer_outlives(&field_types(body, source)?, source);
        }
        Some(Self {
            attributes,
            layout,
            non_exhaustive,
            conditional_non_exhaustive,
            visibility,
            name: item
                .child_by_field_name("name")?
                .utf8_text(source)
                .ok()?
                .to_owned(),
            type_parameters,
            where_clause,
            generics,
            form,
            fields,
        })
    }

    /// 外から構築できない (構造体式を書けない) か。無条件の公開でないフィールドがあるか、
    /// 無条件の `#[non_exhaustive]` が付いている。
    fn unconditionally_restricted(&self) -> bool {
        self.non_exhaustive
            || self
                .fields
                .iter()
                .any(|field| !field.public && field.conditions.is_empty())
    }

    /// 設定によって外から構築できなくする条件 (cfg の付いた公開でないフィールドと、
    /// `cfg_attr` の中の `non_exhaustive`)。
    fn conditional_restrictions(&self) -> BTreeSet<Vec<Vec<String>>> {
        let mut conditions: BTreeSet<Vec<Vec<String>>> = self
            .fields
            .iter()
            .filter(|field| !field.public && !field.conditions.is_empty())
            .map(|field| condition_keys(&field.conditions))
            .collect();
        for attr in &self.conditional_non_exhaustive {
            conditions.insert(vec![attr.key.clone()]);
        }
        conditions
    }

    /// 公開フィールドが無く、外から構築できない。
    fn has_only_private_fields(&self) -> bool {
        !self.fields.is_empty()
            && self.fields.iter().all(|field| !field.public)
            && self.unconditionally_restricted()
    }

    /// 無条件の `#[repr(..)]` の項目 (`C`・`align(8)` など) の集合。属性を分けて書いても、
    /// 並べ替えても、末尾に `,` があっても同じ。
    fn layout_items(&self) -> BTreeSet<Vec<String>> {
        repr_items(&self.layout)
    }

    /// `#[repr]` を含む `#[cfg_attr]` の鍵。
    fn conditional_layout(&self) -> BTreeSet<Vec<String>> {
        conditional_repr(&self.layout)
    }

    /// API の signature として出す表示。名前付きの公開フィールドは名前の順 (`#[repr]` の構造体は
    /// 宣言の順)、タプルは最後の公開フィールドまでの位置 (公開でないものは `_`) を示し、残りの
    /// 公開でないフィールドは `..` で示す。
    pub(crate) fn signature(&self) -> String {
        let mut out = String::new();
        for attribute in &self.attributes {
            let _ = write!(out, "#[{attribute}] ");
        }
        if let Some(visibility) = &self.visibility {
            out.push_str(&visibility.shown());
            out.push(' ');
        }
        out.push_str("struct ");
        out.push_str(&self.name);
        if let Some(parameters) = &self.type_parameters {
            out.push_str(&parameters.shown());
        }
        let mut where_text = self
            .where_clause
            .as_ref()
            .map(|clause| format!(" {}", clause.shown()))
            .unwrap_or_default();
        // フィールドから推論される寿命の境界も、利用側に暗黙の境界として伝わる契約なので示す
        // (公開でないフィールドの変更でも変わる)。cfg の付いたフィールドからのものは条件付きで。
        let implied: Vec<String> = self
            .generics
            .inferred_outlives
            .iter()
            .map(|(component, lifetime)| format!("{component}: {lifetime}"))
            .chain(self.generics.conditional_outlives.iter().map(
                |((_, component, lifetime), condition)| {
                    format!("{condition}{component}: {lifetime}")
                },
            ))
            .collect();
        if !implied.is_empty() {
            let _ = write!(where_text, " /* implied: {} */", implied.join(", "));
        }
        match self.form {
            Form::Unit => {
                out.push_str(&where_text);
                out.push(';');
            }
            Form::Named => {
                out.push_str(&where_text);
                let mut public: Vec<&Field> = self.fields.iter().filter(|f| f.public).collect();
                if self.layout.is_empty() {
                    public.sort_by(|a, b| a.key.cmp(&b.key));
                }
                let mut parts: Vec<String> = public
                    .iter()
                    .map(|field| {
                        format!(
                            "{}pub {}: {}",
                            condition_prefix(&field.conditions),
                            field.shown_name,
                            field.ty.shown()
                        )
                    })
                    .collect();
                parts.extend(private_summary(self.fields.iter()));
                if parts.is_empty() {
                    out.push_str(" {}");
                } else {
                    let _ = write!(out, " {{ {} }}", parts.join(", "));
                }
            }
            Form::Tuple => {
                let shown = self
                    .fields
                    .iter()
                    .rposition(|field| field.public)
                    .map_or(0, |last| last + 1);
                let (positions, rest) = self.fields.split_at(shown);
                let mut parts: Vec<String> = positions
                    .iter()
                    .map(|field| {
                        let value = if field.public {
                            format!("pub {}", field.ty.shown())
                        } else {
                            "_".to_owned()
                        };
                        format!("{}{value}", condition_prefix(&field.conditions))
                    })
                    .collect();
                parts.extend(private_summary(rest.iter()));
                let _ = write!(out, "({}){where_text};", parts.join(", "));
            }
        }
        out
    }
}

/// 旧新の公開の形を比べる。
pub(crate) fn classify(old: &RustStructShape, new: &RustStructShape) -> ShapeChange {
    use ShapeChange::{Breaking, Compatible};
    if old.visibility.as_ref().map(Tokens::key) != new.visibility.as_ref().map(Tokens::key) {
        return Breaking;
    }
    let (old_names, new_names) = (Renaming::new(&old.generics), Renaming::new(&new.generics));
    let mut change = compare_generics(&old.generics, &new.generics, &old_names, &new_names);
    if change == Breaking {
        return Breaking;
    }
    // 足した型の引数は、旧い利用者から見て既定値と同じ。
    let old_count = old.generics.split().1.len();
    let defaults: HashMap<String, Vec<String>> = new
        .generics
        .split()
        .1
        .iter()
        .enumerate()
        .skip(old_count)
        .filter_map(|(index, param)| {
            Some((
                format!("#{index}"),
                new_names.apply(param.default.as_ref()?),
            ))
        })
        .collect();
    let same_field = |old_field: &Field, new_field: &Field| {
        condition_keys(&old_field.conditions) == condition_keys(&new_field.conditions)
            && old_names.apply(&old_field.ty)
                == substitute_defaults(new_names.apply(&new_field.ty), &defaults)
    };

    // 既定の表現に `repr(C)`・`repr(transparent)` を付けるだけが互換。
    if old.conditional_layout() != new.conditional_layout() {
        return Breaking;
    }
    let (old_items, new_items) = (old.layout_items(), new.layout_items());
    if old_items != new_items {
        let defined = |item: &Vec<String>| matches!(item.as_slice(), [only] if only == "C" || only == "transparent");
        if !old_items.is_empty() || !new_items.iter().all(defined) {
            return Breaking;
        }
        change = change.max(Compatible);
    }
    let (old_layout, new_layout) = (!old.layout.is_empty(), !new.layout.is_empty());
    if old.form != new.form {
        let private_only =
            |shape: &RustStructShape| shape.form != Form::Unit && shape.has_only_private_fields();
        if !private_only(old) || !private_only(new) {
            return Breaking;
        }
        change = change.max(Compatible);
    }

    let old_constructible = !old.unconditionally_restricted();
    let mut added = false;
    match (old.form, new.form) {
        (Form::Named, Form::Named) => {
            let public = |shape: &'_ RustStructShape| -> BTreeMap<FieldKey, usize> {
                shape
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| field.public)
                    .map(|(index, field)| (field.key.clone(), index))
                    .collect()
            };
            let (old_public, new_public) = (public(old), public(new));
            for (key, &old_index) in &old_public {
                let Some(&new_index) = new_public.get(key) else {
                    return Breaking;
                };
                if !same_field(&old.fields[old_index], &new.fields[new_index]) {
                    return Breaking;
                }
            }
            added = new_public.keys().any(|key| !old_public.contains_key(key));
            // `#[repr]` の構造体は、公開フィールドの順もレイアウトの約束 (公開でないフィールドの
            // 変更は Cargo の手引きでも互換)。足す公開フィールドは、ほかのフィールドのレイアウトを
            // 変えないよう、旧い公開フィールドの後に置く必要がある (`repr-c-private-change`)。
            if old_layout && new_layout {
                let order = |shape: &RustStructShape| -> Vec<FieldKey> {
                    shape
                        .fields
                        .iter()
                        .filter(|field| field.public && old_public.contains_key(&field.key))
                        .map(|field| field.key.clone())
                        .collect()
                };
                let last_kept = new
                    .fields
                    .iter()
                    .rposition(|field| field.public && old_public.contains_key(&field.key));
                let inserted_before = new.fields.iter().enumerate().any(|(index, field)| {
                    field.public
                        && !old_public.contains_key(&field.key)
                        && last_kept.is_some_and(|last| index < last)
                });
                if order(old) != order(new) || inserted_before {
                    return Breaking;
                }
            }
        }
        (Form::Tuple, Form::Tuple) => {
            for (index, old_field) in old.fields.iter().enumerate() {
                if !old_field.public {
                    continue;
                }
                let Some(new_field) = new.fields.get(index) else {
                    return Breaking;
                };
                // 前のフィールドの cfg が変わると、公開フィールドの実際の位置が変わる。
                let positions_kept = old.fields[..index]
                    .iter()
                    .zip(&new.fields[..index])
                    .all(|(a, b)| condition_keys(&a.conditions) == condition_keys(&b.conditions));
                if !new_field.public || !positions_kept || !same_field(old_field, new_field) {
                    return Breaking;
                }
            }
            added = new.fields.iter().enumerate().any(|(index, field)| {
                field.public
                    && !old
                        .fields
                        .get(index)
                        .is_some_and(|old_field| old_field.public)
            });
        }
        _ => {}
    }
    if added {
        // 外から構築できた構造体では、外の構造体式が足りなくなる。
        if old_constructible {
            return Breaking;
        }
        change = change.max(Compatible);
    }

    let (old_restricted, new_restricted) = (
        old.unconditionally_restricted(),
        new.unconditionally_restricted(),
    );
    if new_restricted {
        if !old_restricted {
            return Breaking;
        }
    } else if old_restricted {
        change = change.max(Compatible);
    } else {
        let (old_conditions, new_conditions) = (
            old.conditional_restrictions(),
            new.conditional_restrictions(),
        );
        if !new_conditions.is_subset(&old_conditions) {
            return Breaking;
        }
        if new_conditions != old_conditions {
            change = change.max(Compatible);
        }
    }
    change
}

/// ファイルの中の、名前が `name` の `struct_item` (入れ子の module の中も)。
pub(crate) fn struct_items_named<'tree>(
    root: Node<'tree>,
    source: &[u8],
    name: &str,
) -> Vec<Node<'tree>> {
    let mut found = Vec::new();
    let _ = walk_preorder(root, Children::Named, |node, _| {
        if node.kind() == "struct_item"
            && node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                == Some(name)
        {
            found.push(node);
        }
        ControlFlow::<(), Walk>::Continue(Walk::Descend)
    });
    found
}

/// シンボル `sym` の構造体の公開の形の signature。形を読めない構造体は、前に付いた属性と宣言全体の
/// 空白を詰めた字面にする (どの変更も signature に現れ、判定で blocking に残る)。構造体を特定
/// できなければ None (呼び出し側は従来の先頭行に戻る)。
pub(crate) fn struct_signature(
    sym: &crate::models::symbol::Symbol,
    root: Node<'_>,
    source: &[u8],
) -> Option<String> {
    let start = tree_sitter::Point::new(sym.range.start.line, sym.range.start.column);
    let end = tree_sitter::Point::new(sym.range.end.line, sym.range.end.column);
    let mut node = root.descendant_for_point_range(start, end)?;
    while node.kind() != "struct_item" {
        node = node.parent()?;
    }
    if let Some(shape) = RustStructShape::from_node(node, source) {
        return Some(shape.signature());
    }
    // 文字列の中の空白とコメントも字面のまま残す (どの変更も signature に現れるように)。
    let mut parts = preceding_attributes(node);
    parts.push(node);
    Some(Tokens::lenient(&parts, source)?.shown())
}

/// ソースの中で名前が `name` の構造体が 1 つだけなら、その公開の形。
pub(crate) fn unique_struct_shape(source: &[u8], name: &str) -> Option<RustStructShape> {
    let tree = crate::engine::parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    let items = struct_items_named(tree.root_node(), source, name);
    let [item] = items.as_slice() else {
        return None;
    };
    RustStructShape::from_node(*item, source)
}
