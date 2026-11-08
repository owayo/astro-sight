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
//!   `repr(C)` の構造体の公開フィールドの並べ替えと、旧い公開フィールドより前への公開フィールドの
//!   追加 (構造体が有り `repr(C)` が効く設定ごとに、そこに同時に有るフィールドどうしで比べる)
//! - 互換 (mod_compat): 構築の制限を緩めること、外から構築できない構造体への公開フィールドの追加、
//!   トレイトの境界を緩めること、型の引数に既定値を付けること、境界の無い既定値の付いた型の引数を
//!   足すこと (既定値で置き換えて公開フィールドの型が同じで、既定値が見た目から `Sized` で今の
//!   どの利用でも正しい型と言えるとき)、既定の表現に `repr(C)`・`repr(transparent)` を付けること、
//!   公開フィールドの無い構築できない構造体のタプルと名前付きの間の変更
//! - 変更なし: 型の引数の改名 (`for<'a>` の束縛も)、整形とコメント、`r#` の付け外し、`#[repr]` の
//!   書き方 (構造体の有る、`#[cfg]`・`#[cfg_attr]` の条件の設定ごとに、効く項目の集合で比べる)、
//!   `repr(C)` の効かない構造体 (`align`・`packed`・`transparent` だけや `repr(Rust)`) の名前付きの
//!   フィールドの並べ替え、外から構築できない構造体の公開でないフィールドの変更 (`repr(C)` でも)
//!
//! 名前付きの公開フィールドは、名前と `#[cfg]` の条件の組で対応させる (`#[cfg]` で分けた同じ名前の
//! フィールドは、それぞれを比べる。Issue #290)。同じ組の公開フィールドが 2 つあれば、対応を
//! 決められないので blocking に残す。
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

use super::rust_cfg_predicate::{Atoms, Predicate, effective_repr};
use super::rust_type_api::*;

/// 構造体 1 つの公開の形。
#[derive(Clone, Debug)]
pub(crate) struct RustStructShape {
    /// 構造体自身と囲む項目の `#[cfg]` と、`cfg` を含む `#[cfg_attr]` (比べる設定を、構造体の有る設定に
    /// 絞る。enum・union と同じく signature には出さない。Issue #293)。
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
        let mut conditions = Vec::new();
        let mut attributes = Vec::new();
        let mut layout = Vec::new();
        let mut non_exhaustive = false;
        let mut conditional_non_exhaustive = Vec::new();
        for attribute in preceding_attributes(item) {
            if let Some(attr) = project_attribute(attribute, source, &["cfg"])? {
                conditions.push(attr);
            }
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
            form,
            fields,
        })
    }

    /// 囲む項目 (inline module・ファイルの先頭など) の有る条件を足す。比べる設定を、構造体の有る
    /// 設定に絞る (Issue #291・#293)。
    pub(super) fn with_enclosing(mut self, enclosing: Vec<Attr>) -> Self {
        self.conditions.extend(enclosing);
        self
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

    /// 構造体の有る設定のどこかで、フィールドの順をレイアウトの約束にする表現 ([`orders_fields`]) が
    /// 効くか。signature で公開フィールドを宣言の順に示すかに使う。原子が多すぎれば true。
    fn has_ordered_layout(&self) -> bool {
        let mut atoms = Atoms::default();
        let present = atoms.present(&self.conditions);
        let reprs = atoms.reprs(&self.layout);
        let Some(mut configs) = atoms.configs() else {
            return true;
        };
        configs
            .any(|config| present.holds(config) && orders_fields(&effective_repr(&reprs, config)))
    }

    /// API の signature として出す表示。名前付きの公開フィールドは名前と `#[cfg]` の条件の順
    /// (順を約束する `repr(C)` が効く設定のある構造体は宣言の順)、タプルは最後の公開フィールドまでの位置
    /// (公開でないものは `_`) を示し、残りの公開でないフィールドは `..` で示す。
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
                if !self.has_ordered_layout() {
                    public.sort_by_key(|field| public_key(field));
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

/// 名前付きの公開フィールドを旧新で対応させる鍵 (名前と `#[cfg]` の条件)。
type PublicKey = (FieldKey, Vec<Vec<String>>);

fn public_key(field: &Field) -> PublicKey {
    (field.key.clone(), condition_keys(&field.conditions))
}

/// 名前付きの公開フィールドの鍵から、宣言の位置へ。同じ鍵の公開フィールドが 2 つあれば、どれと
/// どれが対応するかを決められないので None。
fn public_fields(shape: &RustStructShape) -> Option<BTreeMap<PublicKey, usize>> {
    let mut public = BTreeMap::new();
    for (index, field) in shape.fields.iter().enumerate() {
        if field.public && public.insert(public_key(field), index).is_some() {
            return None;
        }
    }
    Some(public)
}

/// 効く表現の項目 `items` が、フィールドの順をレイアウトの約束にするか。`repr(C)` は順を約束し、
/// `align`・`packed` (既定の表現の修飾) と `transparent` (唯一の大きさのあるフィールドと同じ
/// レイアウト) だけでは約束しない (Rust の参照)。ほかの読めない項目は約束するものとする (Issue #293)。
fn orders_fields(items: &BTreeSet<Vec<String>>) -> bool {
    items.iter().any(|item| {
        !matches!(
            item.first().map(String::as_str),
            Some("align" | "packed" | "transparent")
        )
    })
}

/// 構造体の公開フィールドの順を、`#[cfg]`・`#[cfg_attr]` の原子の真偽の組み合わせ (設定) ごとに比べる。
/// 比べるのは、旧新の構造体が有り、旧新とも順を約束する表現 ([`orders_fields`]) が効く設定だけで、
/// その設定に同時に有るフィールドどうしで比べる (`#[cfg]` で分けた同じ名前のフィールドは同じ設定に
/// 有らないので、その間の並べ替えはどの設定の順も変えない。Issue #290・#293)。旧い公開フィールド
/// (`old_public`) の並びが同じで、足した公開フィールドが旧い公開フィールドより前に無ければ true。
/// 同じ名前のフィールドが 2 つ有る設定はビルドできないので、旧版でそうなら比べず、新版だけでそう
/// なら壊すものとする。原子が多すぎれば false。
fn same_public_order(
    old: &RustStructShape,
    new: &RustStructShape,
    old_public: &BTreeMap<PublicKey, usize>,
) -> bool {
    // 順を約束する表現の効く設定が旧新のどちらかに無ければ、順は比べない (フィールドの条件の原子を
    // 列挙しない)。
    if !old.has_ordered_layout() || !new.has_ordered_layout() {
        return true;
    }
    let mut atoms = Atoms::default();
    let (old_present, new_present) = (
        atoms.present(&old.conditions),
        atoms.present(&new.conditions),
    );
    let (old_reprs, new_reprs) = (atoms.reprs(&old.layout), atoms.reprs(&new.layout));
    let mut presence = |shape: &RustStructShape| -> Vec<Predicate> {
        shape
            .fields
            .iter()
            .map(|field| atoms.present(&field.conditions))
            .collect()
    };
    let (old_presence, new_presence) = (presence(old), presence(new));
    let Some(configs) = atoms.configs() else {
        return false;
    };
    // 設定 `config` で有る公開フィールドの鍵 (宣言の順)。同じ名前のフィールドが 2 つ有れば None。
    let present = |shape: &RustStructShape, presence: &[Predicate], config: u64| {
        let mut names = BTreeSet::new();
        let mut keys = Vec::new();
        for (field, predicate) in shape.fields.iter().zip(presence) {
            if !predicate.holds(config) {
                continue;
            }
            if !names.insert(&field.key) {
                return None;
            }
            if field.public {
                keys.push(public_key(field));
            }
        }
        Some(keys)
    };
    for config in configs {
        if !old_present.holds(config)
            || !new_present.holds(config)
            || !orders_fields(&effective_repr(&old_reprs, config))
            || !orders_fields(&effective_repr(&new_reprs, config))
        {
            continue;
        }
        let Some(old_keys) = present(old, &old_presence, config) else {
            continue;
        };
        let Some(new_keys) = present(new, &new_presence, config) else {
            return false;
        };
        let kept = |key: &&PublicKey| old_public.contains_key(*key);
        if !old_keys.iter().eq(new_keys.iter().filter(kept)) {
            return false;
        }
        let last_kept = new_keys.iter().rposition(|key| kept(&key));
        let inserted_before = new_keys
            .iter()
            .enumerate()
            .any(|(index, key)| !kept(&key) && last_kept.is_some_and(|last| index < last));
        if inserted_before {
            return false;
        }
    }
    true
}

/// 旧新の公開の形を比べる。`availability_compared` は、旧新の有る設定の変化を別に比べる (同じ
/// ファイルの宣言の組。Issue #291) か。比べるなら、新しい構造体が無い設定はここでは比べない。
pub(crate) fn classify(
    old: &RustStructShape,
    new: &RustStructShape,
    availability_compared: bool,
) -> ShapeChange {
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

    // 表現は、`#[cfg]`・`#[cfg_attr]` の条件の原子の真偽の組み合わせ (設定) ごとに、効く項目の集合で
    // 比べる (enum・union と同じ)。既定の表現に `repr(C)`・`repr(transparent)` を付けるだけが互換。
    // 構造体の無い設定は比べない (Issue #293)。旧い構造体だけが有る設定は、有る設定の変化を別に
    // 比べるならそちらに任せ、そうでなければ壊す (enum・union と同じ)。
    let mut atoms = Atoms::default();
    let (old_present, new_present) = (
        atoms.present(&old.conditions),
        atoms.present(&new.conditions),
    );
    let (old_reprs, new_reprs) = (atoms.reprs(&old.layout), atoms.reprs(&new.layout));
    let Some(configs) = atoms.configs() else {
        return Breaking;
    };
    let defined = |item: &Vec<String>| matches!(item.as_slice(), [only] if only == "C" || only == "transparent");
    for config in configs {
        if !old_present.holds(config) {
            continue;
        }
        if !new_present.holds(config) {
            if availability_compared {
                continue;
            }
            return Breaking;
        }
        let (old_items, new_items) = (
            effective_repr(&old_reprs, config),
            effective_repr(&new_reprs, config),
        );
        if old_items != new_items {
            if !old_items.is_empty() || !new_items.iter().all(defined) {
                return Breaking;
            }
            change = change.max(Compatible);
        }
    }
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
            // 名前と `#[cfg]` の条件の組で対応させる (`#[cfg]` で分けた同じ名前のフィールドを、
            // 後のものが前のものを隠さないように。Issue #290)。
            let (Some(old_public), Some(new_public)) = (public_fields(old), public_fields(new))
            else {
                return Breaking;
            };
            for (key, &old_index) in &old_public {
                let Some(&new_index) = new_public.get(key) else {
                    return Breaking;
                };
                if !same_field(&old.fields[old_index], &new.fields[new_index]) {
                    return Breaking;
                }
            }
            added = new_public.keys().any(|key| !old_public.contains_key(key));
            // `repr(C)` の構造体は、公開フィールドの順もレイアウトの約束 (公開でないフィールドの
            // 変更は Cargo の手引きでも互換)。足す公開フィールドは、ほかのフィールドのレイアウトを
            // 変えないよう、旧い公開フィールドの後に置く必要がある (`repr-c-private-change`)。
            if !same_public_order(old, new, &old_public) {
                return Breaking;
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
        // 囲む項目の有る条件も、順を約束する表現が効く設定を決めるのに使う (読めなければ自身の条件だけ)。
        let shape = match super::rust_declarations::enclosing_conditions(node, source) {
            Some(enclosing) => shape.with_enclosing(enclosing),
            None => shape,
        };
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
    let enclosing = super::rust_declarations::enclosing_conditions(*item, source)?;
    Some(RustStructShape::from_node(*item, source)?.with_enclosing(enclosing))
}
