//! Rust の公開 enum の公開の形 (Issue #276)。
//!
//! `pub enum` の API の signature を、宣言の先頭行から、外の利用者に見える形 (variant の名前・形・
//! フィールドと型・`#[cfg]`・`#[non_exhaustive]`・書いた discriminant、enum の `#[repr]`・
//! `#[non_exhaustive]`、型の引数と `where`) の表示に広げる。旧新の形は Cargo の SemVer の手引き
//! (<https://doc.rust-lang.org/cargo/reference/semver.html>) と Rust の参照に沿って比べる。
//!
//! `#[cfg]` と `#[cfg_attr]` は述語 (`all`・`any`・`not` と原子) として読み、原子の真偽の組み合わせ
//! (設定) ごとに、その設定で有る variant・フィールド・`#[non_exhaustive]`・`#[repr]` を作って比べる。
//! 設定の中では、variant は名前、名前付きのフィールドは名前、タプルのフィールドは位置で対応させ、
//! どれかの設定で壊せば壊す変更とする。同じ設定に同じ名前の variant かフィールドが 2 つ有る組み合わせは、
//! 実際には無い設定として比べない。
//!
//! - 壊す (blocking): variant の削除・改名・形の変更、`#[non_exhaustive]` でない enum への variant の
//!   追加、`#[non_exhaustive]` でない variant へのフィールドの追加、フィールドの削除・改名・型の変更・
//!   タプルの位置の変更、enum と variant への `#[non_exhaustive]` の追加、外から観測できる discriminant
//!   の値の変更と、外から `as` で変換できなくすること、`#[repr]` を外すことと変えること、
//!   `repr(C)`・`repr(<int>)` の variant の名前付きのフィールドの並べ替え、可視性の変更、型の引数の
//!   壊す変更 (構造体と同じ規則)。どれも、ある設定でだけ起きるものを含む
//! - 互換 (mod_compat): `#[non_exhaustive]` の enum への variant の追加 (`#[cfg]` を外して、無かった
//!   設定に足すのも)、`#[non_exhaustive]` の variant へのフィールドの追加、フィールドの無い
//!   `#[non_exhaustive]` の variant の形の変更、enum と variant の `#[non_exhaustive]` を外すこと、
//!   既定の表現に `repr(C)`・`repr(<int>)`・`repr(C, <int>)`・`repr(transparent)` を付けること、
//!   型の引数の互換な変更
//! - 変更なし: 整形とコメント、型の引数の改名、`r#` の付け外し、値が外から観測できない enum の
//!   variant の並べ替え、`repr(C)`・`repr(<int>)` でない enum の名前付きのフィールドの並べ替え、
//!   値の同じ discriminant の書き方、どの設定でも同じになる `#[cfg]` の書き換え、derive・helper 属性
//!   (`#[serde]`・`#[default]`)・`#[doc(hidden)]` (公開の形ではない)
//!
//! discriminant の値は、`as` で変換できる設定 (有る variant にフィールドが無く、書いた discriminant は
//! unit の variant にだけあり、`#[non_exhaustive]` の variant が無い) と、`repr(C)`・`repr(<int>)` の
//! 設定で観測できる。値は整数のリテラル (符号・接尾辞・`_` を許す) と、前に有る variant からの暗黙の
//! `+1` で求め、リテラルでない式は字面で比べる (同じ式から同じ数だけ後ろなら同じ値。定数の中身の
//! 変更は追わない)。派生 (`Ord`・`Default` など) の振る舞いの変化は形の比較では見ない。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::ops::ControlFlow;

use tree_sitter::Node;

use super::rust_cfg_predicate::*;
use super::rust_type_api::*;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

/// 整数の表現 (`repr(u8)` など)。
const INTEGER_REPRS: [&str; 12] = [
    "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
];

/// variant 1 つ。
#[derive(Clone, Debug)]
struct Variant {
    /// `r#` を外した名前 (対応付けの鍵)。
    key: String,
    /// 書いたとおりの名前。
    shown_name: String,
    /// `#[cfg]` と、`cfg` を含む `#[cfg_attr]`。
    conditions: Vec<Attr>,
    /// 表示する属性 (`#[non_exhaustive]` と、それを含む `#[cfg_attr]`)。
    attributes: Vec<String>,
    /// 無条件の `#[non_exhaustive]`。
    non_exhaustive: bool,
    /// `#[non_exhaustive]` を含む `#[cfg_attr]`。
    conditional_non_exhaustive: Vec<Attr>,
    form: Form,
    fields: Vec<Field>,
    /// 書いた discriminant (`=` の後の式)。
    discriminant: Option<Tokens>,
    /// discriminant が整数のリテラルなら、その値。
    literal: Option<Discriminant>,
    /// フィールドごとの、型から推論される寿命の境界 (成分の名前, ライフタイム)。`fields` と同じ順。
    outlives: Vec<BTreeSet<(String, String)>>,
}

impl Variant {
    /// 並べるときの鍵 (名前と cfg の条件)。
    fn sort_key(&self) -> (String, Vec<Vec<String>>) {
        (self.key.clone(), condition_keys(&self.conditions))
    }
}

/// discriminant の値。`base` は最後に書いたリテラルでない式の字面 (無ければ 0 から)、`offset` は
/// そこからの差。同じ組は同じ値。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Discriminant {
    base: Option<Vec<String>>,
    offset: i128,
}

/// `2^127` 以上の値 (`repr(u128)`) の基点の印。値は `i128` に収まらないので、ここからの差で表す。
const HIGH_BASE: &str = "#2^127";

impl Discriminant {
    /// 符号と絶対値の整数。`i128` に収まらない正の値は [`HIGH_BASE`] からの差。
    fn of(negative: bool, magnitude: u128) -> Option<Self> {
        if negative {
            return Some(Self {
                base: None,
                offset: 0i128.checked_sub_unsigned(magnitude)?,
            });
        }
        Some(match i128::try_from(magnitude) {
            Ok(offset) => Self { base: None, offset },
            Err(_) => Self {
                base: Some(vec![HIGH_BASE.to_owned()]),
                offset: i128::try_from(magnitude - (1u128 << 127)).ok()?,
            },
        })
    }

    /// 次の variant の暗黙の値 (`+ 1`)。
    fn next(&self) -> Self {
        match self.offset.checked_add(1) {
            Some(offset) => Self {
                base: self.base.clone(),
                offset,
            },
            None if self.base.is_none() => Self {
                base: Some(vec![HIGH_BASE.to_owned()]),
                offset: 0,
            },
            // どの整数の表現にも収まらない (コンパイルできない) 値。
            None => Self {
                base: Some(vec!["#overflow".to_owned()]),
                offset: 0,
            },
        }
    }
}

/// enum 1 つの公開の形。
#[derive(Clone, Debug)]
pub(crate) struct RustEnumShape {
    /// enum 自身の `#[cfg]` と、`cfg` を含む `#[cfg_attr]` (比べる設定を、enum の有る設定に絞る。
    /// 構造体と同じく signature には出さない)。
    conditions: Vec<Attr>,
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
    /// 宣言の順の variant。
    variants: Vec<Variant>,
}

/// 整数のリテラル (`1`・`0x1F`・`-1`・`1_000u16`) の値。
fn integer_value(node: Node<'_>, source: &[u8]) -> Option<Discriminant> {
    let (negative, literal) = match node.kind() {
        "integer_literal" => (false, node),
        "unary_expression" | "negative_literal" => {
            let children: Vec<Node<'_>> = node
                .children(&mut node.walk())
                .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"))
                .collect();
            match children.as_slice() {
                [minus, literal] if minus.kind() == "-" && literal.kind() == "integer_literal" => {
                    (true, *literal)
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    let text: String = literal
        .utf8_text(source)
        .ok()?
        .chars()
        .filter(|c| *c != '_')
        .collect();
    let digits = INTEGER_REPRS
        .iter()
        .find_map(|suffix| {
            text.strip_suffix(suffix)
                .filter(|rest| !rest.is_empty() && !rest.ends_with(['x', 'o', 'b']))
        })
        .unwrap_or(&text);
    let (radix, digits) = match digits.get(..2) {
        Some("0x") => (16, &digits[2..]),
        Some("0o") => (8, &digits[2..]),
        Some("0b") => (2, &digits[2..]),
        _ => (10, digits),
    };
    Discriminant::of(negative, u128::from_str_radix(digits, radix).ok()?)
}

/// enum の本体の variant (宣言の順)。読めない構文があれば None。
fn variants(body: Node<'_>, source: &[u8]) -> Option<Vec<Variant>> {
    let mut variants = Vec::new();
    for child in body.named_children(&mut body.walk()) {
        match child.kind() {
            "attribute_item" | "line_comment" | "block_comment" => {}
            "enum_variant" => {
                if child
                    .named_children(&mut child.walk())
                    .any(|part| part.kind() == "visibility_modifier")
                {
                    return None;
                }
                let attributes = preceding_attributes(child);
                let mut shown = Vec::new();
                let mut non_exhaustive = false;
                let mut conditional_non_exhaustive = Vec::new();
                for attribute in &attributes {
                    if let Some(attr) = project_attribute(*attribute, source, &["non_exhaustive"])?
                    {
                        shown.push(attr.shown.clone());
                        if attr.is_conditional() {
                            conditional_non_exhaustive.push(attr);
                        } else {
                            non_exhaustive = true;
                        }
                    }
                }
                let name = child.child_by_field_name("name")?.utf8_text(source).ok()?;
                let (form, fields) = match child.child_by_field_name("body") {
                    None => (Form::Unit, Vec::new()),
                    Some(body) if body.kind() == "field_declaration_list" => {
                        (Form::Named, named_fields(body, source, true)?)
                    }
                    Some(body) if body.kind() == "ordered_field_declaration_list" => {
                        (Form::Tuple, tuple_fields(body, source, true)?)
                    }
                    Some(_) => return None,
                };
                let value = child.child_by_field_name("value");
                variants.push(Variant {
                    key: unraw(name).to_owned(),
                    shown_name: name.to_owned(),
                    conditions: field_conditions(&attributes, source)?,
                    attributes: shown,
                    non_exhaustive,
                    conditional_non_exhaustive,
                    form,
                    fields,
                    discriminant: match value {
                        Some(value) => Some(Tokens::of(value, source)?),
                        None => None,
                    },
                    literal: value.and_then(|value| integer_value(value, source)),
                    outlives: Vec::new(),
                });
            }
            _ => return None,
        }
    }
    Some(variants)
}

impl RustEnumShape {
    /// 囲む項目 (inline module・ファイルの先頭など) の有る条件を足す。比べる設定を、enum の有る
    /// 設定に絞る (Issue #291)。
    pub(super) fn with_enclosing(mut self, enclosing: Vec<Attr>) -> Self {
        self.conditions.extend(enclosing);
        self
    }

    /// `enum_item` の形。読めない構文があれば None。
    pub(crate) fn from_node(item: Node<'_>, source: &[u8]) -> Option<Self> {
        if item.kind() != "enum_item" || item.has_error() {
            return None;
        }
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
        let mut variants = variants(body, source)?;
        let mut generics = Generics::parse(parameters, where_node, source)?;
        // 推論される寿命の境界は、全ての variant のフィールドの型から (variant の cfg も条件に)。
        let mut types = Vec::new();
        for child in body.named_children(&mut body.walk()) {
            if child.kind() != "enum_variant" {
                continue;
            }
            let conditions = field_conditions(&preceding_attributes(child), source)?;
            if let Some(fields) = child.child_by_field_name("body") {
                for (field_conditions, ty) in field_types(fields, source)? {
                    let mut all = conditions.clone();
                    all.extend(field_conditions);
                    types.push((all, ty));
                }
            }
        }
        generics.infer_outlives(&types, source);
        // 設定ごとに比べるので、フィールドごとの推論される境界も持つ。
        let is_param = |name: &str, kind: ParamKind| {
            generics
                .params
                .iter()
                .any(|param| param.name == name && param.kind == kind)
        };
        let mut position = 0;
        for child in body.named_children(&mut body.walk()) {
            if child.kind() != "enum_variant" {
                continue;
            }
            let variant = variants.get_mut(position)?;
            position += 1;
            if let Some(fields) = child.child_by_field_name("body") {
                for (_, ty) in field_types(fields, source)? {
                    let mut found = BTreeSet::new();
                    inferred_from(ty, source, &is_param, &mut found);
                    variant.outlives.push(found);
                }
            }
            if variant.outlives.len() != variant.fields.len() {
                return None;
            }
        }
        let conditions = field_conditions(&preceding_attributes(item), source)?;
        Some(Self {
            conditions,
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
            variants,
        })
    }

    /// 無条件の `#[repr]` の項目 (`Rust` は既定と同じなので除く)。
    fn repr(&self) -> BTreeSet<Vec<String>> {
        let mut items = repr_items(&self.layout);
        items.remove(&vec!["Rust".to_owned()]);
        items
    }

    /// `repr(C)`・`repr(<int>)` になり得る (無条件にそう書いたか、`cfg_attr` の `repr` がある)。
    /// signature で名前付きのフィールドを宣言の順に示すかに使う。
    fn layout_possible(&self) -> bool {
        !conditional_repr(&self.layout).is_empty()
            || self.repr().iter().any(|item| match item.as_slice() {
                [only] => only == "C" || INTEGER_REPRS.contains(&only.as_str()),
                _ => false,
            })
    }

    /// 原子の表 `atoms` で読んだ、variant・フィールド・`#[non_exhaustive]`・`#[repr]` の条件。
    fn conditions(&self, atoms: &mut Atoms) -> Conditions {
        let restriction = |unconditional: bool, conditional: &[Attr], atoms: &mut Atoms| {
            let mut parts = Vec::new();
            if unconditional {
                parts.push(Predicate::always());
            }
            for attribute in conditional {
                for (predicate, payload) in atoms.payloads(&attribute.key) {
                    if payload
                        .first()
                        .is_some_and(|name| unraw(name) == "non_exhaustive")
                    {
                        parts.push(predicate);
                    }
                }
            }
            Predicate::Any(parts)
        };
        let mut conditions = Conditions {
            present: atoms.present(&self.conditions),
            variants: Vec::with_capacity(self.variants.len()),
            fields: Vec::with_capacity(self.variants.len()),
            restricted_variants: Vec::with_capacity(self.variants.len()),
            restricted: restriction(self.non_exhaustive, &self.conditional_non_exhaustive, atoms),
            repr: atoms.reprs(&self.layout),
        };
        for variant in &self.variants {
            conditions.variants.push(atoms.present(&variant.conditions));
            conditions.fields.push(
                variant
                    .fields
                    .iter()
                    .map(|field| atoms.present(&field.conditions))
                    .collect(),
            );
            conditions.restricted_variants.push(restriction(
                variant.non_exhaustive,
                &variant.conditional_non_exhaustive,
                atoms,
            ));
        }
        conditions
    }

    /// ある設定で discriminant の値を外から観測できるか (signature の並べ方に使う)。
    fn values_observable(&self) -> bool {
        let mut atoms = Atoms::default();
        let conditions = self.conditions(&mut atoms);
        let Some(mut configs) = atoms.configs() else {
            return true;
        };
        configs.any(|config| {
            effective(self, &conditions, config)
                .is_some_and(|shape| shape.present && (shape.castable() || shape.layout_defined()))
        })
    }

    /// API の signature として出す表示。variant は宣言の順 (値を観測できない enum は名前の順) で、
    /// 書いた discriminant を示す。名前付きのフィールドは名前の順 (`repr(C)`・`repr(<int>)` の
    /// enum は宣言の順) にする。
    pub(crate) fn signature(&self) -> String {
        let mut out = String::new();
        for attribute in &self.attributes {
            let _ = write!(out, "#[{attribute}] ");
        }
        if let Some(visibility) = &self.visibility {
            out.push_str(&visibility.shown());
            out.push(' ');
        }
        out.push_str("enum ");
        out.push_str(&self.name);
        if let Some(parameters) = &self.type_parameters {
            out.push_str(&parameters.shown());
        }
        if let Some(clause) = &self.where_clause {
            let _ = write!(out, " {}", clause.shown());
        }
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
        let ordered = self.layout_possible();
        let mut variants: Vec<&Variant> = self.variants.iter().collect();
        if !self.values_observable() {
            variants.sort_by_key(|variant| variant.sort_key());
        }
        let parts: Vec<String> = variants
            .iter()
            .map(|variant| {
                let mut part = condition_prefix(&variant.conditions);
                for attribute in &variant.attributes {
                    let _ = write!(part, "#[{attribute}] ");
                }
                part.push_str(&variant.shown_name);
                match variant.form {
                    Form::Unit => {}
                    Form::Named => {
                        let mut fields: Vec<&Field> = variant.fields.iter().collect();
                        if !ordered {
                            fields.sort_by(|a, b| a.key.cmp(&b.key));
                        }
                        let shown: Vec<String> = fields
                            .iter()
                            .map(|field| {
                                format!(
                                    "{}{}: {}",
                                    condition_prefix(&field.conditions),
                                    field.shown_name,
                                    field.ty.shown()
                                )
                            })
                            .collect();
                        if shown.is_empty() {
                            part.push_str(" {}");
                        } else {
                            let _ = write!(part, " {{ {} }}", shown.join(", "));
                        }
                    }
                    Form::Tuple => {
                        let shown: Vec<String> = variant
                            .fields
                            .iter()
                            .map(|field| {
                                format!(
                                    "{}{}",
                                    condition_prefix(&field.conditions),
                                    field.ty.shown()
                                )
                            })
                            .collect();
                        let _ = write!(part, "({})", shown.join(", "));
                    }
                }
                if let Some(discriminant) = &variant.discriminant {
                    let _ = write!(part, " = {}", discriminant.shown());
                }
                part
            })
            .collect();
        if parts.is_empty() {
            out.push_str(" {}");
        } else {
            let _ = write!(out, " {{ {} }}", parts.join(", "));
        }
        out
    }
}

/// 表現の項目の集合が、既定の表現に付けてよい定義した表現 (`C`・`<int>`・`C, <int>`・
/// `transparent`) か。
fn defined_repr(items: &BTreeSet<Vec<String>>) -> bool {
    let defined = |item: &Vec<String>| match item.as_slice() {
        [only] => only == "C" || only == "transparent" || INTEGER_REPRS.contains(&only.as_str()),
        _ => false,
    };
    let integers = items
        .iter()
        .filter(|item| matches!(item.as_slice(), [only] if INTEGER_REPRS.contains(&only.as_str())))
        .count();
    let transparent = items.contains(&vec!["transparent".to_owned()]);
    !items.is_empty()
        && items.iter().all(defined)
        && integers <= 1
        && !(transparent && items.len() > 1)
}

/// variant・フィールド・`#[non_exhaustive]`・`#[repr]` が効く条件。
struct Conditions {
    /// enum が有る条件。
    present: Predicate,
    /// variant ごとの、有る条件。
    variants: Vec<Predicate>,
    /// variant ごと・フィールドごとの、有る条件 (variant の条件は含まない)。
    fields: Vec<Vec<Predicate>>,
    /// variant ごとの、`#[non_exhaustive]` が効く条件。
    restricted_variants: Vec<Predicate>,
    /// enum の `#[non_exhaustive]` が効く条件。
    restricted: Predicate,
    /// `#[repr]` の項目と、それが効く条件 (無条件のものは真)。
    repr: Vec<(Predicate, BTreeSet<Vec<String>>)>,
}

/// 1 つの設定での variant。
struct EffectiveVariant<'a> {
    variant: &'a Variant,
    /// 有るフィールド (宣言の順)。
    fields: Vec<&'a Field>,
    /// 外で構築も網羅的な照合もできない (`#[non_exhaustive]` が効く)。
    restricted: bool,
}

/// 1 つの設定での enum。
struct EffectiveEnum<'a> {
    /// enum がこの設定に有るか。
    present: bool,
    /// 有る variant (宣言の順)。
    variants: Vec<EffectiveVariant<'a>>,
    restricted: bool,
    repr: BTreeSet<Vec<String>>,
    /// 有るフィールドの型から推論される寿命の境界。
    outlives: BTreeSet<(String, String)>,
}

/// 設定 `config` での enum。同じ名前の variant か名前付きのフィールドが 2 つ有れば、実際には無い
/// 設定なので None。
fn effective<'a>(
    shape: &'a RustEnumShape,
    conditions: &Conditions,
    config: u64,
) -> Option<EffectiveEnum<'a>> {
    let mut names = BTreeSet::new();
    let mut variants = Vec::new();
    let mut outlives = BTreeSet::new();
    for (index, variant) in shape.variants.iter().enumerate() {
        if !conditions.variants[index].holds(config) {
            continue;
        }
        if !names.insert(variant.key.as_str()) {
            return None;
        }
        let mut fields: Vec<&Field> = Vec::new();
        for ((field, presence), found) in variant
            .fields
            .iter()
            .zip(&conditions.fields[index])
            .zip(&variant.outlives)
        {
            if presence.holds(config) {
                fields.push(field);
                outlives.extend(found.iter().cloned());
            }
        }
        let mut field_names = BTreeSet::new();
        if !fields.iter().all(|field| field_names.insert(&field.key)) {
            return None;
        }
        variants.push(EffectiveVariant {
            variant,
            fields,
            restricted: conditions.restricted_variants[index].holds(config),
        });
    }
    Some(EffectiveEnum {
        present: conditions.present.holds(config),
        variants,
        restricted: conditions.restricted.holds(config),
        repr: effective_repr(&conditions.repr, config),
        outlives,
    })
}

impl EffectiveEnum<'_> {
    /// 外の crate から `as` で整数に変換できるか (有る variant にフィールドが無く、書いた
    /// discriminant は unit の variant にだけあり、`#[non_exhaustive]` の variant が無い)。
    fn castable(&self) -> bool {
        self.variants.iter().all(|entry| {
            entry.fields.is_empty()
                && (entry.variant.form == Form::Unit || entry.variant.discriminant.is_none())
                && !entry.restricted
        })
    }

    /// タグの値と variant のフィールドの並びがレイアウトの約束になる表現 (`repr(C)`・`repr(<int>)`)。
    fn layout_defined(&self) -> bool {
        self.repr.iter().any(|item| match item.as_slice() {
            [only] => only == "C" || INTEGER_REPRS.contains(&only.as_str()),
            _ => false,
        })
    }

    /// 有る variant ごとの discriminant の値 (前に有る variant から暗黙に `+1`)。
    fn values(&self) -> Vec<Discriminant> {
        let mut current = Discriminant {
            base: None,
            offset: -1,
        };
        self.variants
            .iter()
            .map(|entry| {
                current = match (&entry.variant.literal, &entry.variant.discriminant) {
                    (Some(value), _) => value.clone(),
                    (None, Some(expression)) => Discriminant {
                        base: Some(expression.key()),
                        offset: 0,
                    },
                    (None, None) => current.next(),
                };
                current.clone()
            })
            .collect()
    }

    fn position(&self, key: &str) -> Option<usize> {
        self.variants
            .iter()
            .position(|entry| entry.variant.key == key)
    }
}

/// 1 つの設定で、対応する variant を比べる。`same_type` はフィールドの型が同じか。
fn compare_variant(
    old: &EffectiveVariant<'_>,
    new: &EffectiveVariant<'_>,
    layout_defined: bool,
    same_type: &dyn Fn(&Field, &Field) -> bool,
) -> ShapeChange {
    use ShapeChange::{Breaking, Compatible, Unchanged};
    let mut change = Unchanged;
    // variant の `#[non_exhaustive]`: 付ければ外の構築と照合を壊し、外せば緩めるだけ。
    if new.restricted && !old.restricted {
        return Breaking;
    }
    if old.restricted && !new.restricted {
        change = Compatible;
    }
    let added = if old.variant.form != new.variant.form {
        // フィールドの無い制限された variant は、外で構築できず、照合は `V { .. }` だけなので、
        // フィールドを持たせても壊さない (Rust の参照の `non_exhaustive`)。
        if !(old.restricted && old.fields.is_empty()) {
            return Breaking;
        }
        change = change.max(Compatible);
        !new.fields.is_empty()
    } else {
        match old.variant.form {
            Form::Unit => false,
            Form::Named => {
                let index = |entry: &EffectiveVariant<'_>| -> BTreeMap<FieldKey, usize> {
                    entry
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(position, field)| (field.key.clone(), position))
                        .collect()
                };
                let (old_index, new_index) = (index(old), index(new));
                for (key, &old_position) in &old_index {
                    let Some(&new_position) = new_index.get(key) else {
                        return Breaking;
                    };
                    if !same_type(old.fields[old_position], new.fields[new_position]) {
                        return Breaking;
                    }
                }
                // `repr(C)`・`repr(<int>)` では、フィールドの並びもレイアウトの約束。足す
                // フィールドは旧いフィールドの後に置く。
                if layout_defined {
                    let kept = |field: &&Field| old_index.contains_key(&field.key);
                    let order = |fields: &[&Field]| -> Vec<FieldKey> {
                        fields
                            .iter()
                            .filter(|field| kept(field))
                            .map(|field| field.key.clone())
                            .collect()
                    };
                    let last_kept = new.fields.iter().rposition(kept);
                    let inserted_before = new.fields.iter().enumerate().any(|(position, field)| {
                        !kept(field) && last_kept.is_some_and(|last| position < last)
                    });
                    if order(&old.fields) != order(&new.fields) || inserted_before {
                        return Breaking;
                    }
                }
                new_index.keys().any(|key| !old_index.contains_key(key))
            }
            Form::Tuple => {
                for (position, old_field) in old.fields.iter().enumerate() {
                    let Some(new_field) = new.fields.get(position) else {
                        return Breaking;
                    };
                    if !same_type(old_field, new_field) {
                        return Breaking;
                    }
                }
                new.fields.len() > old.fields.len()
            }
        }
    };
    if added {
        // 外で構築できた variant では、外の構築とフィールドを全て書いた照合が壊れる。
        if !old.restricted {
            return Breaking;
        }
        change = change.max(Compatible);
    }
    change
}

/// 1 つの設定で旧新の enum を比べる。
fn compare_effective(
    old: &EffectiveEnum<'_>,
    new: &EffectiveEnum<'_>,
    same_type: &dyn Fn(&Field, &Field) -> bool,
) -> ShapeChange {
    use ShapeChange::{Breaking, Compatible, Unchanged};
    let mut change = Unchanged;
    // enum の `#[non_exhaustive]`: 付ければ外の網羅的な照合を壊し、外せば緩めるだけ。
    if new.restricted && !old.restricted {
        return Breaking;
    }
    if old.restricted && !new.restricted {
        change = Compatible;
    }
    // 表現: 同じか、既定の表現に定義した表現を付けるだけなら互換。
    if old.repr != new.repr {
        if !(old.repr.is_empty() && defined_repr(&new.repr)) {
            return Breaking;
        }
        change = change.max(Compatible);
    }
    let layout_defined = old.layout_defined();
    for old_entry in &old.variants {
        let Some(new_position) = new.position(&old_entry.variant.key) else {
            return Breaking;
        };
        change = change.max(compare_variant(
            old_entry,
            &new.variants[new_position],
            layout_defined,
            same_type,
        ));
        if change == Breaking {
            return Breaking;
        }
    }
    if new
        .variants
        .iter()
        .any(|entry| old.position(&entry.variant.key).is_none())
    {
        // 網羅的な照合が外にあり得る enum への追加は壊す。
        if !old.restricted {
            return Breaking;
        }
        change = change.max(Compatible);
    }
    // `as` で変換できた enum を変換できなくするのは壊す。値が観測できるなら、残した variant の
    // 値を保つ。
    if old.castable() && !new.castable() {
        return Breaking;
    }
    if old.castable() || layout_defined {
        let (old_values, new_values) = (old.values(), new.values());
        for (old_position, entry) in old.variants.iter().enumerate() {
            let Some(new_position) = new.position(&entry.variant.key) else {
                return Breaking;
            };
            if old_values[old_position] != new_values[new_position] {
                return Breaking;
            }
        }
    }
    change
}

/// 旧新の公開の形を比べる。`availability_compared` は、旧新の有る設定の変化を別に比べる
/// (同じファイルの宣言の組。Issue #291) か。比べるなら、新しい enum が無い設定はここでは比べない
/// (消える設定は削除と同じ区分で出す)。
pub(crate) fn classify(
    old: &RustEnumShape,
    new: &RustEnumShape,
    availability_compared: bool,
) -> ShapeChange {
    use ShapeChange::Breaking;
    if old.visibility.as_ref().map(Tokens::key) != new.visibility.as_ref().map(Tokens::key) {
        return Breaking;
    }
    let (old_names, new_names) = (Renaming::new(&old.generics), Renaming::new(&new.generics));
    let mut change = ShapeChange::Unchanged;
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
    let mut compared = false;
    for config in configs {
        // 旧い enum が無いか、同じ名前が 2 つ有ってビルドできない設定は比べない。
        let Some(old_enum) = effective(old, &old_conditions, config).filter(|shape| shape.present)
        else {
            continue;
        };
        let new_enum = match effective(new, &new_conditions, config) {
            Some(shape) if shape.present => shape,
            Some(_) if availability_compared => continue,
            // 旧くビルドできた設定で、新しい enum が無いかビルドできないのは壊す。
            _ => return Breaking,
        };
        compared = true;
        // 型の引数と寿命の境界 (推論されるものは、この設定で有るフィールドから)。
        let with_outlives = |generics: &Generics, shape: &EffectiveEnum<'_>| {
            let mut generics = generics.clone();
            generics.inferred_outlives = shape.outlives.clone();
            generics.conditional_outlives.clear();
            generics
        };
        change = change.max(compare_generics(
            &with_outlives(&old.generics, &old_enum),
            &with_outlives(&new.generics, &new_enum),
            &old_names,
            &new_names,
        ));
        if change == Breaking {
            return Breaking;
        }
        change = change.max(compare_effective(&old_enum, &new_enum, &same_type));
        if change == Breaking {
            return Breaking;
        }
    }
    if !compared && !availability_compared {
        return Breaking;
    }
    change
}

/// ファイルの中の、名前が `name` の `enum_item` (入れ子の module の中も)。
pub(crate) fn enum_items_named<'tree>(
    root: Node<'tree>,
    source: &[u8],
    name: &str,
) -> Vec<Node<'tree>> {
    let mut found = Vec::new();
    let _ = walk_preorder(root, Children::Named, |node, _| {
        if node.kind() == "enum_item"
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

/// シンボル `sym` の enum の公開の形の signature。形を読めない enum は、前に付いた属性と宣言全体の
/// 空白を詰めた字面にする (どの変更も signature に現れ、判定で blocking に残る)。enum を特定
/// できなければ None (呼び出し側は従来の先頭行に戻る)。
pub(crate) fn enum_signature(
    sym: &crate::models::symbol::Symbol,
    root: Node<'_>,
    source: &[u8],
) -> Option<String> {
    let start = tree_sitter::Point::new(sym.range.start.line, sym.range.start.column);
    let end = tree_sitter::Point::new(sym.range.end.line, sym.range.end.column);
    let mut node = root.descendant_for_point_range(start, end)?;
    while node.kind() != "enum_item" {
        node = node.parent()?;
    }
    if let Some(shape) = RustEnumShape::from_node(node, source) {
        return Some(shape.signature());
    }
    let mut parts = preceding_attributes(node);
    parts.push(node);
    Some(Tokens::lenient(&parts, source)?.shown())
}

/// ソースの中で名前が `name` の enum が 1 つだけなら、その公開の形。
pub(crate) fn unique_enum_shape(source: &[u8], name: &str) -> Option<RustEnumShape> {
    let tree = crate::engine::parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    let items = enum_items_named(tree.root_node(), source, name);
    let [item] = items.as_slice() else {
        return None;
    };
    let enclosing = super::rust_declarations::enclosing_conditions(*item, source)?;
    Some(RustEnumShape::from_node(*item, source)?.with_enclosing(enclosing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(source: &str) -> RustEnumShape {
        unique_enum_shape(source.as_bytes(), "Kind").expect("shape")
    }

    /// 設定ごとに `as` で変換できるか (どの設定でも、ある設定で)。
    fn castable(source: &str) -> (bool, bool) {
        let shape = shape(source);
        let mut atoms = Atoms::default();
        let conditions = shape.conditions(&mut atoms);
        let results: Vec<bool> = atoms
            .configs()
            .expect("configs")
            .filter_map(|config| effective(&shape, &conditions, config))
            .map(|shape| shape.castable())
            .collect();
        (
            results.iter().all(|castable| *castable),
            results.iter().any(|castable| *castable),
        )
    }

    #[test]
    fn integer_discriminants_are_evaluated() {
        let kind =
            shape("pub enum Kind { A = 0x10, B, C = -2, D = 1_000u16, E = 0b11, F = SOME + 1, G }");
        let mut atoms = Atoms::default();
        let conditions = kind.conditions(&mut atoms);
        let values: Vec<_> = effective(&kind, &conditions, 0)
            .expect("effective")
            .values()
            .into_iter()
            .map(|value| (value.base.is_some(), value.offset))
            .collect();
        assert_eq!(
            values,
            [
                (false, 16),
                (false, 17),
                (false, -2),
                (false, 1000),
                (false, 3),
                (true, 0),
                (true, 1)
            ]
        );
    }

    #[test]
    fn signatures_sort_variants_only_when_values_are_not_observable() {
        // 値を観測できない enum は、並べ替えても signature が同じ (移動の照合に効く)
        assert_eq!(
            shape("pub enum Kind { A(u8), B(u8) }").signature(),
            shape("pub enum Kind { B(u8), A(u8) }").signature()
        );
        // `as` で読める enum と `repr(<int>)` の enum は、宣言の順を示す
        assert_ne!(
            shape("pub enum Kind { A, B }").signature(),
            shape("pub enum Kind { B, A }").signature()
        );
        assert_ne!(
            shape("#[repr(u8)] pub enum Kind { A(u8), B(u8) }").signature(),
            shape("#[repr(u8)] pub enum Kind { B(u8), A(u8) }").signature()
        );
    }

    #[test]
    fn castability_follows_the_reference() {
        assert_eq!(castable("pub enum Kind { A, B = 3 }"), (true, true));
        assert_eq!(castable("pub enum Kind { A(), B {} }"), (true, true));
        // 書いた discriminant が unit の variant にだけあれば変換できる
        assert_eq!(castable("pub enum Kind { A(), B = 3 }"), (true, true));
        assert_eq!(castable("pub enum Kind { A() = 1, B }"), (false, false));
        assert_eq!(castable("pub enum Kind { A(u8), B }"), (false, false));
        assert_eq!(
            castable("pub enum Kind { #[non_exhaustive] A, B }"),
            (false, false)
        );
        assert_eq!(
            castable("#[non_exhaustive] pub enum Kind { A, B }"),
            (true, true)
        );
        // cfg で外れる variant・フィールドと、cfg_attr の non_exhaustive は、ある設定では変換できる
        assert_eq!(
            castable(
                "pub enum Kind { A, #[cfg(feature = \"x\")] B(u8), C(#[cfg(feature = \"y\")] u8), #[cfg_attr(feature = \"z\", non_exhaustive)] D }"
            ),
            (false, true)
        );
        assert_eq!(
            castable("pub enum Kind { A(u8), #[cfg(feature = \"x\")] B }"),
            (false, false)
        );
        // cfg と cfg(not) で分けたフィールドは、どの設定でも有る
        assert_eq!(
            castable(
                "pub enum Kind { A { #[cfg(feature = \"wide\")] value: u32, #[cfg(not(feature = \"wide\"))] value: u8 }, B }"
            ),
            (false, false)
        );
    }
}
