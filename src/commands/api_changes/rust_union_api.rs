//! Rust の公開 union の公開の形 (Issue #277)。
//!
//! `pub union` の API の signature を、外の利用者に見える形 (公開フィールドの名前と型、フィールドの
//! `#[cfg]`、公開でないフィールドの有無、`#[repr]`、型の引数と `where`、推論される寿命の境界) の表示に
//! する。旧新の形は Cargo の SemVer の手引き (<https://doc.rust-lang.org/cargo/reference/semver.html>)
//! と Rust の参照の union の節 (<https://doc.rust-lang.org/reference/items/unions.html>) に沿って比べる。
//!
//! union の値は 1 つのフィールドで作り (`Value { number: 1 }`)、パターンも 1 つのフィールドを書く
//! (`Value { number }`)。そのため構造体と違い、公開フィールドを足しても、公開でないフィールドが有っても、
//! 外の構築とパターンは壊れない。`repr(C)` では全てのフィールドが先頭 (offset 0) に置かれ、既定の
//! 表現ではレイアウトが決まっていないので、どちらでもフィールドの順は外から頼れる形に効かない。
//!
//! `#[cfg]` と `#[cfg_attr]` は enum (Issue #276) と同じく述語として読み、原子の真偽の組み合わせ (設定)
//! ごとに、その設定で有るフィールドと `#[repr]` を作って比べる。フィールドは名前で対応させ、どれかの
//! 設定で壊せば壊す変更とする。フィールドが無いか同じ名前が 2 つ有る設定はビルドできないので、旧い側が
//! そうなら比べず、旧くビルドできた設定で新しい側がそうなら壊す。
//!
//! - 壊す (blocking): 公開フィールドの削除・改名・型の変更・公開でなくすこと (ある設定で無くすことも)、
//!   `#[repr]` を外すことと変えること (`packed`・`align` の付け外しを含む)、`#[repr]` の付いた公開
//!   フィールドだけの union へのフィールドの追加 (公開の型が決めていた大きさ・アラインメント・値渡しの
//!   ABI が変わり得る。旧いフィールドと同じ型のフィールドは除く)、外から作れる union への型・定数の
//!   引数の追加 (値は 1 つのフィールドで作り、式の推論に既定値は使われないので、`Value { number: 1 }`
//!   が E0282 になり得る。推論を保つことは構文から確かめられない)、可視性と型の引数の壊す変更
//!   (構造体と同じ規則)
//! - 互換 (mod_compat): 公開フィールドの追加と、公開でないフィールドを公開すること (`#[repr]` の付いた
//!   公開フィールドだけの union を除く)、既定の表現に `repr(C)` を付けること (`#[cfg_attr]` でも)、
//!   型の引数の互換な変更
//! - 変更なし: フィールドの並べ替え (`repr(C)` でも)、公開でないフィールドの変更 (`#[repr]` の付いた
//!   公開フィールドだけの union に足すのを除く)、整形とコメント、型の引数の改名、`r#` の付け外し、
//!   `#[repr]` の書き方 (`packed` と `packed(1)` も同じ)、どの設定でも同じになる `#[cfg]` の書き換え、
//!   derive とドキュメント
//!
//! `repr(transparent)` (union では不安定) は既定の表現に付けてよい表現として扱わない。自動トレイト
//! (`Send`・`Sync` と、`Copy` を導出できるか)、分散、`#[repr(align)]` の型を含むか (`repr(packed)` の
//! 型のフィールドにできるか) は、公開でないフィールドを含む全てのフィールドの型から決まり、型の検査が
//! 要るので追わない (構造体と同じ)。union 自身の `#[cfg]` は比べる設定を絞るのに使うが、構造体・enum と
//! 同じく signature には含めない (それだけの変更は Issue #291)。

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::ops::ControlFlow;

use tree_sitter::Node;

use super::rust_cfg_predicate::*;
use super::rust_type_api::*;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

/// union 1 つの公開の形。
#[derive(Clone, Debug)]
pub(crate) struct RustUnionShape {
    /// union 自身の `#[cfg]` と、`cfg` を含む `#[cfg_attr]` (比べる設定を、union の有る設定に絞る)。
    conditions: Vec<Attr>,
    /// 表示する属性 (`#[repr]` と、それを含む `#[cfg_attr]`)。
    attributes: Vec<String>,
    /// `#[repr]` と、それを含む `#[cfg_attr]`。
    layout: Vec<Attr>,
    visibility: Option<Tokens>,
    name: String,
    type_parameters: Option<Tokens>,
    where_clause: Option<Tokens>,
    generics: Generics,
    /// 宣言の順のフィールド。
    fields: Vec<Field>,
    /// フィールドごとの、型から推論される寿命の境界 (成分の名前, ライフタイム)。`fields` と同じ順。
    outlives: Vec<BTreeSet<(String, String)>>,
}

impl RustUnionShape {
    /// 囲む項目 (inline module・ファイルの先頭など) の有る条件を足す。比べる設定を、union の有る
    /// 設定に絞る (Issue #291)。
    pub(super) fn with_enclosing(mut self, enclosing: Vec<Attr>) -> Self {
        self.conditions.extend(enclosing);
        self
    }

    /// `union_item` の形。読めない構文があれば None。
    pub(crate) fn from_node(item: Node<'_>, source: &[u8]) -> Option<Self> {
        if item.kind() != "union_item" || item.has_error() {
            return None;
        }
        let mut attributes = Vec::new();
        let mut layout = Vec::new();
        for attribute in preceding_attributes(item) {
            if let Some(attr) = project_attribute(attribute, source, &["repr"])? {
                attributes.push(attr.shown.clone());
                layout.push(attr);
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
        let where_node = item
            .named_children(&mut item.walk())
            .find(|child| child.kind() == "where_clause");
        let type_parameters = match parameters {
            Some(parameters) => Some(Tokens::of(parameters, source)?),
            None => None,
        };
        let where_clause = match where_node {
            Some(clause) => Some(Tokens::of(clause, source)?),
            None => None,
        };
        let body = item.child_by_field_name("body")?;
        if body.kind() != "field_declaration_list" {
            return None;
        }
        let fields = named_fields(body, source, false)?;
        let types = field_types(body, source)?;
        let mut generics = Generics::parse(parameters, where_node, source)?;
        generics.infer_outlives(&types, source);
        // 設定ごとに比べるので、フィールドごとの推論される境界も持つ。
        let is_param = |name: &str, kind: ParamKind| {
            generics
                .params
                .iter()
                .any(|param| param.name == name && param.kind == kind)
        };
        let outlives: Vec<BTreeSet<(String, String)>> = types
            .iter()
            .map(|(_, ty)| {
                let mut found = BTreeSet::new();
                inferred_from(*ty, source, &is_param, &mut found);
                found
            })
            .collect();
        if outlives.len() != fields.len() {
            return None;
        }
        Some(Self {
            conditions: field_conditions(&preceding_attributes(item), source)?,
            attributes,
            layout,
            visibility,
            name: item
                .child_by_field_name("name")?
                .utf8_text(source)
                .ok()?
                .to_owned(),
            type_parameters,
            where_clause,
            generics,
            fields,
            outlives,
        })
    }

    /// 原子の表 `atoms` で読んだ、union・フィールド・`#[repr]` の条件。
    fn conditions(&self, atoms: &mut Atoms) -> Conditions {
        Conditions {
            present: atoms.present(&self.conditions),
            fields: self
                .fields
                .iter()
                .map(|field| atoms.present(&field.conditions))
                .collect(),
            repr: atoms.reprs(&self.layout),
        }
    }

    /// API の signature として出す表示。公開フィールドは名前の順 (順はレイアウトにも効かない) に示し、
    /// 公開でないフィールドは `..` (cfg の付いたものだけなら条件ごとに `#[cfg(..)] ..`) で示す。
    pub(crate) fn signature(&self) -> String {
        let mut out = String::new();
        for attribute in &self.attributes {
            let _ = write!(out, "#[{attribute}] ");
        }
        if let Some(visibility) = &self.visibility {
            out.push_str(&visibility.shown());
            out.push(' ');
        }
        out.push_str("union ");
        out.push_str(&self.name);
        if let Some(parameters) = &self.type_parameters {
            out.push_str(&parameters.shown());
        }
        if let Some(clause) = &self.where_clause {
            let _ = write!(out, " {}", clause.shown());
        }
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
            let _ = write!(out, " /* implied: {} */", implied.join(", "));
        }
        let mut public: Vec<&Field> = self.fields.iter().filter(|field| field.public).collect();
        public.sort_by_key(|field| (field.key.clone(), condition_keys(&field.conditions)));
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
        out
    }
}

/// union・フィールド・`#[repr]` が効く条件。
struct Conditions {
    /// union が有る条件。
    present: Predicate,
    /// フィールドごとの、有る条件。
    fields: Vec<Predicate>,
    /// `#[repr]` の項目と、それが効く条件 (無条件のものは真)。
    repr: Vec<(Predicate, BTreeSet<Vec<String>>)>,
}

/// 1 つの設定での union。
struct EffectiveUnion<'a> {
    /// union がこの設定に有るか。
    present: bool,
    /// 有るフィールド (宣言の順)。
    fields: Vec<&'a Field>,
    repr: BTreeSet<Vec<String>>,
    /// 有るフィールドの型から推論される寿命の境界。
    outlives: BTreeSet<(String, String)>,
}

/// 設定 `config` での union。フィールドが無いか、同じ名前のフィールドが 2 つ有れば、ビルドできない
/// 設定なので None。
fn effective<'a>(
    shape: &'a RustUnionShape,
    conditions: &Conditions,
    config: u64,
) -> Option<EffectiveUnion<'a>> {
    let mut names = BTreeSet::new();
    let mut fields = Vec::new();
    let mut outlives = BTreeSet::new();
    for ((field, presence), found) in shape
        .fields
        .iter()
        .zip(&conditions.fields)
        .zip(&shape.outlives)
    {
        if !presence.holds(config) {
            continue;
        }
        if !names.insert(&field.key) {
            return None;
        }
        fields.push(field);
        outlives.extend(found.iter().cloned());
    }
    if fields.is_empty() {
        return None;
    }
    Some(EffectiveUnion {
        present: conditions.present.holds(config),
        fields,
        repr: effective_repr(&conditions.repr, config),
        outlives,
    })
}

impl<'a> EffectiveUnion<'a> {
    fn field(&self, key: &FieldKey) -> Option<&'a Field> {
        self.fields.iter().find(|field| &field.key == key).copied()
    }
}

/// 1 つの設定で旧新の union を比べる。`same_type` はフィールドの型が同じか、`parameters_added` は
/// 型・定数の引数を足したか。
fn compare_effective(
    old: &EffectiveUnion<'_>,
    new: &EffectiveUnion<'_>,
    same_type: &dyn Fn(&Field, &Field) -> bool,
    parameters_added: bool,
) -> ShapeChange {
    use ShapeChange::{Breaking, Compatible, Unchanged};
    let mut change = Unchanged;
    // 表現: 同じか、既定の表現に `repr(C)` を付けるだけなら互換 (`repr(transparent)` は union では
    // 不安定なので含めない)。
    if old.repr != new.repr {
        if !(old.repr.is_empty() && new.repr == BTreeSet::from([vec!["C".to_owned()]])) {
            return Breaking;
        }
        change = Compatible;
    }
    // 旧い公開フィールドは、どれも同じ名前・型で公開のまま残る (外の構築・読み書き・パターン)。
    let mut constructible = false;
    for old_field in old.fields.iter().filter(|field| field.public) {
        constructible = true;
        let Some(new_field) = new.field(&old_field.key) else {
            return Breaking;
        };
        if !new_field.public || !same_type(old_field, new_field) {
            return Breaking;
        }
    }
    // union の値は 1 つのフィールドで作り、式の推論に既定値は使われないので、足した型・定数の引数は
    // その 1 つの初期化の式から推論できなければならない。`Value { number: ManuallyDrop::new(None) }`
    // のように、フィールドの型が引数を含んでも推論できないことがあり、構文からは確かめられない。
    if parameters_added && constructible {
        return Breaking;
    }
    // `#[repr]` の付いた公開フィールドだけの union は、公開の型が大きさ・アラインメント・値渡しの
    // ABI を決める。フィールドを足すと変わり得る (公開でないフィールドが有れば、公開の API は
    // レイアウトを決めない。Cargo の手引きの repr-c-private-change)。旧いフィールドと同じ型の
    // フィールドは、どれも変えない。
    let layout_fixed = !old.repr.is_empty() && old.fields.iter().all(|field| field.public);
    let layout_kept = |field: &Field| {
        old.fields
            .iter()
            .any(|old_field| same_type(old_field, field))
    };
    for new_field in &new.fields {
        match old.field(&new_field.key) {
            None if layout_fixed && !layout_kept(new_field) => return Breaking,
            None if new_field.public => change = change.max(Compatible),
            Some(old_field) if new_field.public && !old_field.public => {
                change = change.max(Compatible);
            }
            _ => {}
        }
    }
    change
}

/// 旧新の公開の形を比べる。
pub(crate) fn classify(
    old: &RustUnionShape,
    new: &RustUnionShape,
    availability_compared: bool,
) -> ShapeChange {
    use ShapeChange::Breaking;
    if old.visibility.as_ref().map(Tokens::key) != new.visibility.as_ref().map(Tokens::key) {
        return Breaking;
    }
    let (old_names, new_names) = (Renaming::new(&old.generics), Renaming::new(&new.generics));
    // 足した型の引数は、旧い利用者から見て既定値と同じ。
    let old_count = old.generics.split().1.len();
    let new_count = new.generics.split().1.len();
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
    let same_type = |old_field: &Field, new_field: &Field| {
        old_names.apply(&old_field.ty)
            == substitute_defaults(new_names.apply(&new_field.ty), &defaults)
    };
    // cfg の原子の真偽の組み合わせ (設定) ごとに比べる。
    let mut atoms = Atoms::default();
    let (old_conditions, new_conditions) = (old.conditions(&mut atoms), new.conditions(&mut atoms));
    let Some(configs) = atoms.configs() else {
        return Breaking;
    };
    let mut change = ShapeChange::Unchanged;
    let mut compared = false;
    for config in configs {
        // 旧い union が無いか、ビルドできない設定は比べない。
        let Some(old_union) = effective(old, &old_conditions, config).filter(|shape| shape.present)
        else {
            continue;
        };
        let new_union = match effective(new, &new_conditions, config) {
            Some(shape) if shape.present => shape,
            // 有る設定の変化を別に比べる (Issue #291) なら、新しい union が無い設定は比べない。
            Some(_) if availability_compared => continue,
            // 旧くビルドできた設定で、新しい union が無いかビルドできないのは壊す。
            _ => return Breaking,
        };
        compared = true;
        // 型の引数と寿命の境界 (推論されるものは、この設定で有るフィールドから)。
        let with_outlives = |generics: &Generics, shape: &EffectiveUnion<'_>| {
            let mut generics = generics.clone();
            generics.inferred_outlives = shape.outlives.clone();
            generics.conditional_outlives.clear();
            generics
        };
        change = change.max(compare_generics(
            &with_outlives(&old.generics, &old_union),
            &with_outlives(&new.generics, &new_union),
            &old_names,
            &new_names,
        ));
        if change == Breaking {
            return Breaking;
        }
        change = change.max(compare_effective(
            &old_union,
            &new_union,
            &same_type,
            new_count > old_count,
        ));
        if change == Breaking {
            return Breaking;
        }
    }
    if !compared && !availability_compared {
        return Breaking;
    }
    change
}

/// ファイルの中の、名前が `name` の `union_item` (入れ子の module の中も)。
pub(crate) fn union_items_named<'tree>(
    root: Node<'tree>,
    source: &[u8],
    name: &str,
) -> Vec<Node<'tree>> {
    let mut found = Vec::new();
    let _ = walk_preorder(root, Children::Named, |node, _| {
        if node.kind() == "union_item"
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

/// シンボル `sym` の union の公開の形の signature。形を読めない union は、前に付いた属性と宣言全体の
/// 空白を詰めた字面にする (どの変更も signature に現れ、判定で blocking に残る)。union を特定
/// できなければ None (呼び出し側は従来の先頭行に戻る)。
pub(crate) fn union_signature(
    sym: &crate::models::symbol::Symbol,
    root: Node<'_>,
    source: &[u8],
) -> Option<String> {
    let start = tree_sitter::Point::new(sym.range.start.line, sym.range.start.column);
    let end = tree_sitter::Point::new(sym.range.end.line, sym.range.end.column);
    let mut node = root.descendant_for_point_range(start, end)?;
    while node.kind() != "union_item" {
        node = node.parent()?;
    }
    if let Some(shape) = RustUnionShape::from_node(node, source) {
        return Some(shape.signature());
    }
    let mut parts = preceding_attributes(node);
    parts.push(node);
    Some(Tokens::lenient(&parts, source)?.shown())
}

/// ソースの中で名前が `name` の union が 1 つだけなら、その公開の形。
pub(crate) fn unique_union_shape(source: &[u8], name: &str) -> Option<RustUnionShape> {
    let tree = crate::engine::parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    let items = union_items_named(tree.root_node(), source, name);
    let [item] = items.as_slice() else {
        return None;
    };
    let enclosing = super::rust_declarations::enclosing_conditions(*item, source)?;
    Some(RustUnionShape::from_node(*item, source)?.with_enclosing(enclosing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(source: &str) -> RustUnionShape {
        unique_union_shape(source.as_bytes(), "Value").expect("shape")
    }

    fn change(old: &str, new: &str) -> ShapeChange {
        classify(&shape(old), &shape(new), false)
    }

    #[test]
    fn signatures_show_public_fields_by_name() {
        // フィールドの順はレイアウトにも効かないので、並べ替えても signature は同じ (移動の照合に効く)
        assert_eq!(
            shape("#[repr(C)] pub union Value { pub b: f32, pub a: u32 }").signature(),
            shape("#[repr(C)] pub union Value { pub a: u32, pub b: f32 }").signature()
        );
        assert_eq!(
            shape("pub union Value<'a, T> where T: Copy { pub a: &'a T, b: u8, #[cfg(x)] c: u16 }")
                .signature(),
            "pub union Value<'a, T> where T: Copy /* implied: T: 'a */ { pub a: &'a T, .. }"
        );
        assert_eq!(
            shape("pub union Value { pub a: u32, #[cfg(x)] b: u16 }").signature(),
            "pub union Value { pub a: u32, #[cfg(x)] .. }"
        );
    }

    #[test]
    fn added_parameters_break_constructible_unions() {
        use ShapeChange::{Breaking, Compatible};
        for (old, new) in [
            // `Value { number: 1 }` から `T` を推論できない (E0282)
            (
                "pub union Value { pub number: u32 }",
                "pub union Value<T = u32> { pub number: u32, pub extra: std::mem::ManuallyDrop<T> }",
            ),
            // フィールドの型が引数を含んでも、`ManuallyDrop::new(None)` からは推論できない
            (
                "pub union Value { pub number: std::mem::ManuallyDrop<Option<u32>> }",
                "pub union Value<T = u32> { pub number: std::mem::ManuallyDrop<Option<T>> }",
            ),
            // 型の別名が射影を隠すこともある (E0284)
            (
                "pub union Value { pub number: Alias<u32> }",
                "pub union Value<T = u32> { pub number: Alias<T> }",
            ),
            (
                "pub union Value { pub number: [u8; 4] }",
                "pub union Value<const N: usize = 4> { pub number: [u8; N] }",
            ),
        ] {
            assert_eq!(change(old, new), Breaking, "{old} => {new}");
        }
        // 外から作れない union (公開フィールドが無い) は、推論の経路が無い
        assert_eq!(
            change(
                "pub union Value { number: u32 }",
                "pub union Value<T = u32> { number: u32, extra: std::mem::ManuallyDrop<T> }"
            ),
            Compatible
        );
    }

    #[test]
    fn layout_of_repr_unions_with_only_public_fields() {
        use ShapeChange::{Breaking, Compatible, Unchanged};
        let old = "#[repr(C)] pub union Value { pub number: u32 }";
        // 旧いフィールドと同じ型のフィールドは、大きさ・アラインメント・値渡しの ABI を変えない
        assert_eq!(
            change(
                old,
                "#[repr(C)] pub union Value { pub number: u32, pub alias: u32 }"
            ),
            Compatible
        );
        assert_eq!(
            change(
                old,
                "#[repr(C)] pub union Value { pub number: u32, raw: u32 }"
            ),
            Unchanged
        );
        assert_eq!(
            change(
                old,
                "#[repr(C)] pub union Value { pub number: u32, pub wide: u64 }"
            ),
            Breaking
        );
        // `packed` と `packed(1)` は同じ表現
        assert_eq!(
            change(
                "#[repr(packed)] pub union Value { pub number: u32 }",
                "#[repr(packed(1))] pub union Value { pub number: u32 }"
            ),
            Unchanged
        );
        assert_eq!(
            change(
                "#[repr(C, align(0x8))] pub union Value { pub number: u32 }",
                "#[repr(C, align(8))] pub union Value { pub number: u32 }"
            ),
            Unchanged
        );
        assert_eq!(
            change(
                "#[repr(packed)] pub union Value { pub number: u32 }",
                "#[repr(packed(2))] pub union Value { pub number: u32 }"
            ),
            Breaking
        );
        // 末尾の `,` と、`cfg_attr` の中の `r#repr` (属性の引数の中では `r#` が残る)
        assert_eq!(
            change(
                "#[repr(C, packed)] pub union Value { pub number: u32 }",
                "#[repr(C, packed(1,))] pub union Value { pub number: u32 }"
            ),
            Unchanged
        );
        assert_eq!(
            change(
                "#[repr(C, align(8))] pub union Value { pub number: u32 }",
                "#[repr(C, align(0x8,))] pub union Value { pub number: u32 }"
            ),
            Unchanged
        );
        assert_eq!(
            change(
                "#[cfg_attr(all(), r#repr(C, packed))] pub union Value { pub number: u32 }",
                "#[cfg_attr(all(), r#repr(C))] pub union Value { pub number: u32 }"
            ),
            Breaking
        );
        // `cfg_attr` の条件の `r#true` は名前の付いた cfg (偽の設定では repr が付かない)
        assert_eq!(
            change(
                "#[cfg_attr(true, repr(C, packed))] pub union Value { pub number: u32 }",
                "#[cfg_attr(r#true, repr(C, packed))] pub union Value { pub number: u32 }"
            ),
            Breaking
        );
        // 複数の `align` は最大のものが効く
        assert_eq!(
            change(
                "#[repr(C, align(4), align(8))] pub union Value { pub number: u32 }",
                "#[repr(C, align(8))] pub union Value { pub number: u32 }"
            ),
            Unchanged
        );
        // `cfg(all(),)` の末尾の `,` は無視される
        assert_eq!(
            change(
                "pub union Value { #[cfg(all())] pub number: u32, pub spare: u8 }",
                "pub union Value { #[cfg(all(),)] pub number: u32, pub spare: u8 }"
            ),
            Unchanged
        );
    }
}
