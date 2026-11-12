//! Rust の型の宣言 (構造体・enum) の公開の形で共有する構文処理 (Issue #270・#276)。
//!
//! 字面のトークン (`Tokens`)、形に効く属性 (`Attr`)、型の引数と `where` (`Generics`) と推論される
//! 寿命の境界、型の引数の改名 (`Renaming`)、フィールドの読み取り、型の引数の比較
//! (`compare_generics`) を持つ。比べ方の規則は [`super::rust_struct_api`] と
//! [`super::rust_enum_api`] の説明を参照。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};

/// 識別子の `r#` を外した字面 (`r#type` は `type`)。生文字列 (`r#"..."#`) はそのまま。
pub(super) fn unraw(text: &str) -> &str {
    match text.strip_prefix("r#") {
        Some(rest) if rest.starts_with(|c: char| c.is_alphabetic() || c == '_') => rest,
        _ => text,
    }
}

pub(super) fn is_comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment")
}

/// ドキュメントコメント (`///`・`//!`・`/** */`・`/*! */`) か。マクロの入力では `#[doc]` の
/// トークンになる。
pub(super) fn is_doc_comment(node: Node<'_>) -> bool {
    is_comment(node)
        && (node.child_by_field_name("outer").is_some()
            || node.child_by_field_name("inner").is_some())
}

/// マクロの引数 (`token_tree`) の中か。
pub(super) fn in_token_tree(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "token_tree" {
            return true;
        }
        current = parent.parent();
    }
    false
}

/// 葉の名前の働き。型の引数の改名で、どの名前空間の引数に置き換えてよいかを決める。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    /// 型の名前空間の名前 (型の位置の名前と、path の先頭)。
    TypeName,
    /// 値の名前空間の名前 (配列の長さなどの式の中)。
    ValueName,
    Lifetime,
    /// マクロの引数の中。マクロは字面で照合するので、`r#` もドキュメントコメントも区別する。
    Opaque,
    Other,
}

/// コメントを飛ばした次の名前付きの兄弟。
pub(super) fn next_named_sibling_skipping_comments(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node.next_named_sibling();
    while let Some(sibling) = current {
        if !is_comment(sibling) {
            return Some(sibling);
        }
        current = sibling.next_named_sibling();
    }
    None
}

/// `node` が関連型の境界の名前の位置 (`type_arguments` の中で、後に境界が続く) か。
pub(super) fn is_associated_bound_name(node: Node<'_>) -> bool {
    node.parent()
        .is_some_and(|parent| parent.kind() == "type_arguments")
        && next_named_sibling_skipping_comments(node)
            .is_some_and(|next| next.kind() == "trait_bounds")
}

/// マクロの呼び出しの名前の中 (`a::m!()` の `a::m`) か。引数の中は [`in_token_tree`] で先に
/// 分かれる。
pub(super) fn in_macro_invocation(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "macro_invocation" {
            return true;
        }
        current = parent.parent();
    }
    false
}

/// 葉 `leaf` の働き。path の続き (`a::B` の `B`)、関連型の名前 (`Item = u8`・`Item: Copy`・
/// `Item<'a>: Copy` の `Item`)、マクロの名前 (`A!()`・`A::m!()`) は型の引数を指さない。
pub(super) fn role_of(leaf: Node<'_>) -> Role {
    if in_token_tree(leaf) {
        return Role::Opaque;
    }
    if in_macro_invocation(leaf) {
        return Role::Other;
    }
    let parent = leaf.parent();
    let parent_kind = parent.map(|parent| parent.kind());
    let is_field =
        |name: &str| parent.and_then(|parent| parent.child_by_field_name(name)) == Some(leaf);
    match leaf.kind() {
        "lifetime" => Role::Lifetime,
        "type_identifier" => match parent_kind {
            Some(
                "scoped_type_identifier"
                | "scoped_type_identifier_in_expression_position"
                | "type_binding",
            ) if is_field("name") => Role::Other,
            // 関連型の境界 (`Values<Item: Copy>`・`Values<Item<'a>: Copy>`)。
            _ if is_associated_bound_name(leaf) => Role::Other,
            Some("generic_type")
                if is_field("type") && parent.is_some_and(is_associated_bound_name) =>
            {
                Role::Other
            }
            _ => Role::TypeName,
        },
        "identifier" => match parent_kind {
            Some(
                "scoped_identifier"
                | "scoped_type_identifier"
                | "scoped_type_identifier_in_expression_position",
            ) if is_field("path") => Role::TypeName,
            Some("scoped_identifier") if is_field("name") => Role::Other,
            _ => Role::ValueName,
        },
        _ => Role::Other,
    }
}

/// 葉の構文 1 つ。
#[derive(Clone, Debug)]
pub(super) struct Token {
    pub(super) text: String,
    /// ソースで直前の葉との間に空白かコメントがあったか (表示にだけ使う)。
    pub(super) spaced: bool,
    pub(super) role: Role,
}

impl Token {
    /// 比べる字面 (マクロの引数の外では `r#` を外す)。
    pub(super) fn compared(&self) -> &str {
        if self.role == Role::Opaque {
            &self.text
        } else {
            unraw(&self.text)
        }
    }
}

/// 葉の構文の並び。比べるときは字面だけを見る (空白とコメントは見ない。マクロの引数の中の
/// ドキュメントコメントは葉に数える)。表示はソースで空白かコメントのあった切れ目にだけ空白を
/// 1 つ入れるので、葉の切れ目を保つ (`pick!(& &u8)` と `pick!(&&u8)` は別の表示)。
#[derive(Clone, Debug, Default)]
pub(super) struct Tokens(pub(super) Vec<Token>);

impl Tokens {
    /// 並んだ兄弟 `nodes` の葉。文字列・文字のリテラルとライフタイムは 1 つの葉にする。
    /// 構文エラーを含めば None。
    pub(super) fn of_nodes(nodes: &[Node<'_>], source: &[u8]) -> Option<Self> {
        Self::collect(nodes, source, false)
    }

    pub(super) fn of(node: Node<'_>, source: &[u8]) -> Option<Self> {
        Self::of_nodes(&[node], source)
    }

    /// 構文エラーの中の葉とコメントも含めた葉 (形を読めない宣言の signature に使う)。
    pub(super) fn lenient(nodes: &[Node<'_>], source: &[u8]) -> Option<Self> {
        Self::collect(nodes, source, true)
    }

    pub(super) fn collect(nodes: &[Node<'_>], source: &[u8], lenient: bool) -> Option<Self> {
        let mut tokens = Vec::new();
        let mut previous_end: Option<usize> = None;
        for node in nodes {
            let flow = walk_preorder(*node, Children::All, |child, _| {
                if child.is_missing() || (child.is_error() && !lenient) {
                    return if lenient {
                        ControlFlow::Continue(Walk::Skip)
                    } else {
                        ControlFlow::Break(())
                    };
                }
                let keep_comment = lenient || (is_doc_comment(child) && in_token_tree(child));
                if is_comment(child) && !keep_comment {
                    return ControlFlow::Continue(Walk::Skip);
                }
                let atomic = is_comment(child)
                    || matches!(
                        child.kind(),
                        "string_literal" | "raw_string_literal" | "char_literal" | "lifetime"
                    );
                if !atomic && child.child_count() > 0 {
                    return ControlFlow::Continue(Walk::Descend);
                }
                let Ok(text) = child.utf8_text(source) else {
                    return ControlFlow::Break(());
                };
                // 行コメントの範囲に入る改行は、次の葉との間の空白に数える。改行の前の空白は
                // ドキュメントの文字列に入るので残す。
                let text = text.trim_end_matches(['\r', '\n']);
                if !text.is_empty() {
                    tokens.push(Token {
                        text: text.to_owned(),
                        spaced: previous_end.is_some_and(|end| child.start_byte() > end),
                        role: role_of(child),
                    });
                    previous_end = Some(child.start_byte() + text.len());
                }
                ControlFlow::Continue(Walk::Skip)
            });
            if flow.is_break() {
                return None;
            }
        }
        Some(Self(tokens))
    }

    /// 比べる鍵。
    pub(super) fn key(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|token| token.compared().to_owned())
            .collect()
    }

    pub(super) fn shown(&self) -> String {
        let mut out = String::new();
        for (index, token) in self.0.iter().enumerate() {
            if index > 0 && token.spaced {
                out.push(' ');
            }
            out.push_str(&token.text);
        }
        out
    }
}

/// 形に効く属性 1 つ (`#[..]` の中身。`cfg_attr` は効く中身だけを残す)。
#[derive(Clone, Debug)]
pub(super) struct Attr {
    /// 比べる鍵。
    pub(super) key: Vec<String>,
    /// 表示 (`#[` と `]` を除く)。
    pub(super) shown: String,
}

impl Attr {
    pub(super) fn from_tokens(tokens: &Tokens) -> Self {
        Self {
            key: tokens.key(),
            shown: tokens.shown(),
        }
    }

    pub(super) fn is_conditional(&self) -> bool {
        self.key.first().is_some_and(|name| name == "cfg_attr")
    }
}

/// 1 区切りの path の属性の名前 (`r#` を外す)。`a::b` のような path は None。
pub(super) fn simple_name<'a>(path: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    if path.kind() != "identifier" {
        return None;
    }
    path.utf8_text(source).ok().map(unraw)
}

/// `attribute_item` のうち名前が `wanted` の属性と、それを含む `cfg_attr` (効く中身だけに
/// 絞る)。効かない属性は `Some(None)`、読めなければ None。
pub(super) fn project_attribute(
    item: Node<'_>,
    source: &[u8],
    wanted: &[&str],
) -> Option<Option<Attr>> {
    let attribute = item
        .named_children(&mut item.walk())
        .find(|child| child.kind() == "attribute")?;
    let Some(name) = simple_name(attribute.named_child(0)?, source) else {
        return Some(None);
    };
    if wanted.contains(&name) {
        return Some(Some(Attr::from_tokens(&Tokens::of(attribute, source)?)));
    }
    if name != "cfg_attr" {
        return Some(None);
    }
    project_cfg_attr(attribute.child_by_field_name("arguments")?, source, wanted)
}

/// `cfg_attr(述語, 属性, ..)` の引数の `token_tree` を、名前が `wanted` の属性 (入れ子の
/// `cfg_attr` の中も) だけに絞る。
pub(super) fn project_cfg_attr(
    arguments: Node<'_>,
    source: &[u8],
    wanted: &[&str],
) -> Option<Option<Attr>> {
    if arguments.kind() != "token_tree" {
        return None;
    }
    let children: Vec<Node<'_>> = arguments.children(&mut arguments.walk()).collect();
    // 外側の括弧を除き、直下の `,` で区切る。
    let inner = children.get(1..children.len().checked_sub(1)?)?;
    let mut groups: Vec<Vec<Node<'_>>> = vec![Vec::new()];
    for child in inner {
        if child.is_error() || child.is_missing() {
            return None;
        }
        if is_comment(*child) {
            continue;
        }
        if child.kind() == "," {
            groups.push(Vec::new());
        } else {
            groups.last_mut()?.push(*child);
        }
    }
    if groups.len() > 1 && groups.last().is_some_and(Vec::is_empty) {
        groups.pop();
    }
    let (predicate, attributes) = groups.split_first()?;
    if predicate.is_empty() {
        return None;
    }
    let mut kept = Vec::new();
    for group in attributes {
        let first = *group.first()?;
        let Some(name) = simple_name(first, source) else {
            continue;
        };
        if group.get(1).is_some_and(|next| next.kind() == "::") {
            continue;
        }
        if wanted.contains(&name) {
            kept.push(Attr::from_tokens(&Tokens::of_nodes(group, source)?));
        } else if name == "cfg_attr" {
            let [_, nested] = group.as_slice() else {
                return None;
            };
            if let Some(attr) = project_cfg_attr(*nested, source, wanted)? {
                kept.push(attr);
            }
        }
    }
    if kept.is_empty() {
        return Some(None);
    }
    let predicate = Tokens::of_nodes(predicate, source)?;
    let mut key = vec!["cfg_attr".to_owned(), "(".to_owned()];
    key.extend(predicate.key());
    for attr in &kept {
        key.push(",".to_owned());
        key.extend(attr.key.iter().cloned());
    }
    key.push(")".to_owned());
    let shown = format!(
        "cfg_attr({}, {})",
        predicate.shown(),
        kept.iter()
            .map(|attr| attr.shown.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Some(Some(Attr { key, shown }))
}

/// `node` の直前にある属性 (`attribute_item`。間のコメントは飛ばす) を、宣言の順に。
pub(super) fn preceding_attributes(node: Node<'_>) -> Vec<Node<'_>> {
    let mut attributes = Vec::new();
    let mut current = node.prev_named_sibling();
    while let Some(sibling) = current {
        match sibling.kind() {
            "attribute_item" => attributes.push(sibling),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        current = sibling.prev_named_sibling();
    }
    attributes.reverse();
    attributes
}

/// `visibility_modifier` が制限の無い `pub` か。
pub(super) fn is_unrestricted_pub(visibility: Node<'_>, source: &[u8]) -> bool {
    visibility
        .utf8_text(source)
        .is_ok_and(|text| text.trim() == "pub")
}

/// 型の引数の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ParamKind {
    Lifetime,
    Type,
    Const,
}

/// 型の引数 1 つ。
#[derive(Clone, Debug)]
pub(super) struct Param {
    pub(super) kind: ParamKind,
    /// 名前 (`r#` を外す。ライフタイムは `'a`)。
    pub(super) name: String,
    /// 引数に付けた境界と、左辺がこの引数の `where` の境界 (`?Sized` と標準の `Sized` を除く)。
    pub(super) bounds: Vec<Tokens>,
    /// `?Sized` が付いているか。
    pub(super) relaxed: bool,
    /// 標準の `Sized` を明示したか (`?Sized` と一緒なら `Sized` を求める)。
    pub(super) sized_written: bool,
    pub(super) default: Option<Tokens>,
    /// 型の引数の既定値が `Sized` で旧い利用のどれでも正しい型と言える条件。
    pub(super) default_sizedness: Sizedness,
    /// 既定値が構造体の引数を含まない (旧い利用で寿命の境界をいつも満たす)。
    pub(super) default_concrete: bool,
    pub(super) const_type: Option<Tokens>,
}

impl Param {
    pub(super) fn new(kind: ParamKind, name: String) -> Self {
        Self {
            kind,
            name,
            bounds: Vec::new(),
            relaxed: false,
            sized_written: false,
            default: None,
            default_sizedness: Sizedness::Unknown,
            default_concrete: false,
            const_type: None,
        }
    }

    /// 大きさの決まらない型を受け付けるか (`?Sized` で、`Sized` を明示していない)。
    pub(super) fn unsized_allowed(&self) -> bool {
        self.relaxed && !self.sized_written
    }
}

/// 型が構文から `Sized` で、旧い利用のどれでも正しい型と言えるか。
#[derive(Clone, Debug)]
pub(super) enum Sizedness {
    /// 前の型の引数そのもの。
    Param(String),
    /// 挙げた型の引数 (名前) が `Sized` なら言える (空ならいつも)。
    IfParams(Vec<String>),
    /// 言えない (名前の付いた型・関連型・`str`・スライス・`dyn`・マクロなど)。
    Unknown,
}

/// `node` の中に、構造体の型の引数かライフタイムの引数の名前があるか。マクロは展開した先で
/// 引数を使えるので、あるとみなす。
pub(super) fn mentions_params(node: Node<'_>, source: &[u8], params: &[String]) -> bool {
    walk_preorder(node, Children::Named, |part, _| {
        let named = matches!(
            part.kind(),
            "type_identifier" | "identifier" | "lifetime" | "primitive_type"
        ) && part
            .utf8_text(source)
            .ok()
            .map(unraw)
            .is_some_and(|name| params.iter().any(|param| param == name));
        if named || part.kind() == "macro_invocation" {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(Walk::Descend)
        }
    })
    .is_break()
}

/// 型 `node` が `Sized` で、旧い利用のどれでも正しい型と言える条件。`!`・`str` 以外の基本型・
/// `()`、構造体の引数を含まない参照・ポインタ・関数ポインタ (参照は参照先に寿命の境界を求める
/// ので、引数を含めば言えない)、それらのタプルと配列、型の引数そのもの (その引数が `Sized`
/// なら) だけを数える。名前の付いた型は、大きさの決まらない独自の型や関連型の射影かもしれない
/// ので数えない。基本型と同じ名前の型の引数 (`u8`) は引数として扱う。
pub(super) fn sizedness(
    node: Node<'_>,
    source: &[u8],
    type_params: &[String],
    params: &[String],
) -> Sizedness {
    let param_name = |node: Node<'_>| {
        matches!(node.kind(), "type_identifier" | "primitive_type")
            .then(|| node.utf8_text(source).ok().map(unraw))
            .flatten()
            .filter(|name| type_params.iter().any(|param| param == name))
    };
    if let Some(name) = param_name(node) {
        return Sizedness::Param(name.to_owned());
    }
    let mut names = Vec::new();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if let Some(name) = param_name(node) {
            names.push(name.to_owned());
            continue;
        }
        match node.kind() {
            "never_type" | "unit_type" => {}
            "reference_type" | "pointer_type" => {
                if node
                    .child_by_field_name("type")
                    .is_none_or(|referent| mentions_params(referent, source, params))
                {
                    return Sizedness::Unknown;
                }
            }
            "function_type" => {
                if mentions_params(node, source, params) {
                    return Sizedness::Unknown;
                }
            }
            "primitive_type" => {
                if node.utf8_text(source).ok() == Some("str") {
                    return Sizedness::Unknown;
                }
            }
            "tuple_type" => stack.extend(
                node.named_children(&mut node.walk())
                    .filter(|child| !is_comment(*child)),
            ),
            "array_type" if node.child_by_field_name("length").is_some() => {
                match node.child_by_field_name("element") {
                    Some(element) => stack.push(element),
                    None => return Sizedness::Unknown,
                }
            }
            _ => return Sizedness::Unknown,
        }
    }
    Sizedness::IfParams(names)
}

/// 型の引数と `where`。
#[derive(Clone, Debug, Default)]
pub(super) struct Generics {
    pub(super) params: Vec<Param>,
    /// 左辺が型の引数そのものでない `where` の述語 (左辺, 境界 1 つ)。
    pub(super) predicates: Vec<(Tokens, Tokens)>,
    /// どの設定でもあるフィールドの型から推論される寿命の境界 (成分の名前, ライフタイム)。
    pub(super) inferred_outlives: BTreeSet<(String, String)>,
    /// cfg の付いたフィールドの型から推論される寿命の境界 ((条件の鍵, 成分の名前, ライフタイム)
    /// から条件の表示)。どの設定でもある境界と同じものは除く。
    pub(super) conditional_outlives: BTreeMap<(Vec<Vec<String>>, String, String), String>,
}

pub(super) fn bound_nodes(bounds: Node<'_>) -> Vec<Node<'_>> {
    bounds
        .named_children(&mut bounds.walk())
        .filter(|child| !is_comment(*child))
        .collect()
}

/// 推論で中を見ない型 (関数ポインタ・`dyn`・`impl`・射影・マクロ)。rustc はこれらの中からも
/// 推論することがあるが、足りない分は寿命の境界の削除を壊す扱いにするだけ (拒みすぎ側)。
pub(super) fn opaque_for_outlives(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_type"
            | "dynamic_type"
            | "abstract_type"
            | "macro_invocation"
            | "token_tree"
            | "attribute_item"
            | "bracketed_type"
            | "qualified_type"
            | "scoped_type_identifier"
    )
}

/// 式の中 (配列の長さ・ブロック) か。式の中で宣言した名前は構造体の引数ではない。
pub(super) fn is_expression_part(node: Node<'_>) -> bool {
    node.kind() == "block"
        || node.parent().is_some_and(|parent| {
            parent.kind() == "array_type" && parent.child_by_field_name("length") == Some(node)
        })
}

/// フィールドの型と、その条件 (`#[cfg]` と、`cfg` を含む `#[cfg_attr]`)。
pub(super) fn field_types<'tree>(
    body: Node<'tree>,
    source: &[u8],
) -> Option<Vec<(Vec<Attr>, Node<'tree>)>> {
    let mut types = Vec::new();
    let mut attributes = Vec::new();
    for child in body.named_children(&mut body.walk()) {
        match child.kind() {
            "attribute_item" => attributes.push(child),
            "line_comment" | "block_comment" | "visibility_modifier" => {}
            kind => {
                let ty = if kind == "field_declaration" {
                    child.child_by_field_name("type")?
                } else {
                    child
                };
                types.push((field_conditions(&attributes, source)?, ty));
                attributes.clear();
            }
        }
    }
    Some(types)
}

/// 推論される寿命の境界 (どの設定でもあるフィールドからのもの, cfg の付いたフィールドからのもの)。
pub(super) type InferredOutlives = (
    BTreeSet<(String, String)>,
    BTreeMap<(Vec<Vec<String>>, String, String), String>,
);

/// フィールドの型の参照 (`&'a T`) から推論される寿命の境界。rustc は構造体のフィールドの型から
/// 寿命の境界を推論する (RFC 2093) ので、`T: 'a` を書くかどうかは、型の引数に `dyn` を渡せない
/// 限り利用側に見える違いにならない。cfg の付いたフィールドからの境界は、その設定でだけ効くので
/// 条件ごとに分ける。
pub(super) fn inferred_outlives(
    types: &[(Vec<Attr>, Node<'_>)],
    source: &[u8],
    generics: &Generics,
) -> InferredOutlives {
    let is_param = |name: &str, kind: ParamKind| {
        generics
            .params
            .iter()
            .any(|param| param.name == name && param.kind == kind)
    };
    let mut unconditional = BTreeSet::new();
    let mut conditional = BTreeMap::new();
    for (conditions, ty) in types {
        let mut found = BTreeSet::new();
        inferred_from(*ty, source, &is_param, &mut found);
        if conditions.is_empty() {
            unconditional.extend(found);
        } else {
            for (component, lifetime) in found {
                conditional.insert(
                    (condition_keys(conditions), component, lifetime),
                    condition_prefix(conditions),
                );
            }
        }
    }
    conditional.retain(|(_, component, lifetime), _| {
        !unconditional.contains(&(component.clone(), lifetime.clone()))
    });
    (unconditional, conditional)
}

/// 型 `ty` の中の参照 `&'a X` ごとに、`X` の成分 (型の引数とライフタイムの引数) が `'a` を
/// 生きることを記録する。成分は組み込みの型 (参照・ポインタ・タプル・配列・スライス) を通して
/// だけたどる。名前の付いた型 (型の別名を含む) は関連型の射影かもしれず、射影の寿命の境界は
/// その引数の境界にならないので、中を見ない (足りない分は拒みすぎ側)。`'a: 'a` は数えない。
pub(super) fn inferred_from(
    ty: Node<'_>,
    source: &[u8],
    is_param: &dyn Fn(&str, ParamKind) -> bool,
    inferred: &mut BTreeSet<(String, String)>,
) {
    let lifetime_of = |reference: Node<'_>| {
        reference
            .named_children(&mut reference.walk())
            .find(|child| child.kind() == "lifetime")
            .and_then(|lifetime| lifetime.utf8_text(source).ok())
    };
    let _ = walk_preorder(ty, Children::Named, |node, _| {
        if opaque_for_outlives(node) || is_expression_part(node) {
            return ControlFlow::<(), Walk>::Continue(Walk::Skip);
        }
        if node.kind() != "reference_type" {
            return ControlFlow::Continue(Walk::Descend);
        }
        let (Some(lifetime), Some(referent)) =
            (lifetime_of(node), node.child_by_field_name("type"))
        else {
            return ControlFlow::Continue(Walk::Descend);
        };
        if !is_param(lifetime, ParamKind::Lifetime) {
            return ControlFlow::Continue(Walk::Descend);
        }
        let mut stack = vec![referent];
        while let Some(part) = stack.pop() {
            if is_expression_part(part) {
                continue;
            }
            match part.kind() {
                "type_identifier" if role_of(part) == Role::TypeName => {
                    if let Some(name) = part.utf8_text(source).ok().map(unraw)
                        && is_param(name, ParamKind::Type)
                    {
                        inferred.insert((name.to_owned(), lifetime.to_owned()));
                    }
                }
                "reference_type" => {
                    if let Some(inner) = lifetime_of(part)
                        && inner != lifetime
                        && is_param(inner, ParamKind::Lifetime)
                    {
                        inferred.insert((inner.to_owned(), lifetime.to_owned()));
                    }
                    stack.extend(part.child_by_field_name("type"));
                }
                "pointer_type" => stack.extend(part.child_by_field_name("type")),
                "array_type" => stack.extend(part.child_by_field_name("element")),
                "tuple_type" => stack.extend(
                    part.named_children(&mut part.walk())
                        .filter(|child| !is_comment(*child)),
                ),
                _ => {}
            }
        }
        ControlFlow::Continue(Walk::Descend)
    });
}

impl Generics {
    /// `type_parameters` と `where_clause` を読む。引数の属性やマクロの変数があれば None。
    pub(super) fn parse(
        parameters: Option<Node<'_>>,
        where_clause: Option<Node<'_>>,
        source: &[u8],
    ) -> Option<Self> {
        let mut generics = Self::default();
        if let Some(parameters) = parameters {
            for child in parameters.named_children(&mut parameters.walk()) {
                if is_comment(child) {
                    continue;
                }
                let name = child.child_by_field_name("name")?.utf8_text(source).ok()?;
                let optional = |field: &str| -> Option<Option<Tokens>> {
                    match child.child_by_field_name(field) {
                        Some(node) => Some(Some(Tokens::of(node, source)?)),
                        None => Some(None),
                    }
                };
                let param = match child.kind() {
                    "lifetime_parameter" => Param::new(ParamKind::Lifetime, name.to_owned()),
                    "type_parameter" => {
                        // 既定値が指せるのは前の引数だけ。
                        let earlier: Vec<String> = generics
                            .params
                            .iter()
                            .filter(|param| param.kind == ParamKind::Type)
                            .map(|param| param.name.clone())
                            .collect();
                        let earlier_all: Vec<String> = generics
                            .params
                            .iter()
                            .map(|param| param.name.clone())
                            .collect();
                        let default_node = child.child_by_field_name("default_type");
                        let mut param = Param::new(ParamKind::Type, unraw(name).to_owned());
                        param.default = optional("default_type")?;
                        if let Some(default) = default_node {
                            param.default_sizedness =
                                sizedness(default, source, &earlier, &earlier_all);
                            param.default_concrete =
                                !mentions_params(default, source, &earlier_all);
                        }
                        param
                    }
                    "const_parameter" => {
                        let mut param = Param::new(ParamKind::Const, unraw(name).to_owned());
                        param.default = optional("value")?;
                        param.const_type =
                            Some(Tokens::of(child.child_by_field_name("type")?, source)?);
                        param
                    }
                    _ => return None,
                };
                generics.params.push(param);
                if let Some(bounds) = child.child_by_field_name("bounds") {
                    generics.add_bounds(generics.params.len() - 1, bounds, source)?;
                }
            }
        }
        let Some(clause) = where_clause else {
            return Some(generics);
        };
        for predicate in clause.named_children(&mut clause.walk()) {
            if is_comment(predicate) {
                continue;
            }
            if predicate.kind() != "where_predicate" {
                return None;
            }
            let left = predicate.child_by_field_name("left")?;
            let bounds = predicate.child_by_field_name("bounds")?;
            let lifetime = left.kind() == "lifetime";
            let target = if lifetime || left.kind() == "type_identifier" {
                let text = unraw(left.utf8_text(source).ok()?);
                generics.params.iter().position(|param| {
                    param.name == text
                        && (param.kind == ParamKind::Lifetime) == lifetime
                        && param.kind != ParamKind::Const
                })
            } else {
                None
            };
            match target {
                Some(index) => generics.add_bounds(index, bounds, source)?,
                None => {
                    let left = Tokens::of(left, source)?;
                    for bound in bound_nodes(bounds) {
                        generics
                            .predicates
                            .push((left.clone(), Tokens::of(bound, source)?));
                    }
                }
            }
        }
        Some(generics)
    }

    pub(super) fn add_bounds(
        &mut self,
        index: usize,
        bounds: Node<'_>,
        source: &[u8],
    ) -> Option<()> {
        for bound in bound_nodes(bounds) {
            let tokens = Tokens::of(bound, source)?;
            let param = self.params.get_mut(index)?;
            let key = tokens.key();
            if param.kind == ParamKind::Type && key.first().is_some_and(|first| first == "?") {
                if !is_sized_path(&key[1..], true) {
                    // `?Sized` 以外の緩める境界は読めない。
                    return None;
                }
                param.relaxed = true;
            } else if param.kind == ParamKind::Type && is_sized_path(&key, false) {
                // 標準の `Sized` を明示した境界は、`?Sized` が無ければ暗黙の境界と同じ。
                param.sized_written = true;
            } else {
                param.bounds.push(tokens);
            }
        }
        Some(())
    }

    /// フィールドの型 `types` (条件と型) から推論される寿命の境界を持つ。引数に書いた寿命の
    /// 境界と同じものは、どの設定でもあるので条件付きから除く。
    pub(super) fn infer_outlives(&mut self, types: &[(Vec<Attr>, Node<'_>)], source: &[u8]) {
        (self.inferred_outlives, self.conditional_outlives) =
            inferred_outlives(types, source, self);
        let written: BTreeSet<(String, String)> = self
            .params
            .iter()
            .flat_map(|param| {
                param
                    .bounds
                    .iter()
                    .filter_map(|bound| match bound.key().as_slice() {
                        [lifetime] if lifetime.starts_with('\'') => {
                            Some((param.name.clone(), lifetime.clone()))
                        }
                        _ => None,
                    })
            })
            .collect();
        self.conditional_outlives
            .retain(|(_, component, lifetime), _| {
                !written.contains(&(component.clone(), lifetime.clone()))
            });
    }

    /// ライフタイムと、それ以外 (型と定数) の引数。
    pub(super) fn split(&self) -> (Vec<&Param>, Vec<&Param>) {
        self.params
            .iter()
            .partition(|param| param.kind == ParamKind::Lifetime)
    }
}

/// 標準の `Sized` を指す path か。`relaxed` (`?` の付く境界) なら、`Sized` にしか付けられない
/// ので `Sized` と相対の path も数える。`?` の無い境界では、`Sized` と相対の `std::marker::Sized`
/// は同じ名前の独自のトレイトやモジュールかもしれないので、crate の根からの path だけを数える。
pub(super) fn is_sized_path(key: &[String], relaxed: bool) -> bool {
    let parts: Vec<&str> = key.iter().map(String::as_str).collect();
    match parts.as_slice() {
        ["Sized"] | ["std" | "core", "::", "marker", "::", "Sized"] => relaxed,
        ["::", "std" | "core", "::", "marker", "::", "Sized"] => true,
        _ => false,
    }
}

/// 型の引数の名前から位置の名前への置き換え (引数の改名を変更としない)。名前空間ごとに分け、
/// ライフタイムはライフタイムの並びの位置、型と定数はそれ以外の並びの位置で名付ける。
pub(super) struct Renaming {
    pub(super) types: HashMap<String, String>,
    pub(super) values: HashMap<String, String>,
    pub(super) lifetimes: HashMap<String, String>,
}

impl Renaming {
    pub(super) fn new(generics: &Generics) -> Self {
        let mut renaming = Self {
            types: HashMap::new(),
            values: HashMap::new(),
            lifetimes: HashMap::new(),
        };
        let (mut lifetimes, mut others) = (0usize, 0usize);
        for param in &generics.params {
            match param.kind {
                ParamKind::Lifetime => {
                    renaming
                        .lifetimes
                        .insert(param.name.clone(), format!("'#{lifetimes}"));
                    lifetimes += 1;
                }
                ParamKind::Type => {
                    renaming
                        .types
                        .insert(param.name.clone(), format!("#{others}"));
                    others += 1;
                }
                ParamKind::Const => {
                    renaming
                        .values
                        .insert(param.name.clone(), format!("#{others}"));
                    others += 1;
                }
            }
        }
        renaming
    }

    /// 型の引数かライフタイムの引数の名前 1 つを、位置の名前にしたもの (引数でなければそのまま)。
    pub(super) fn rename_name(&self, name: &str) -> String {
        self.lifetimes
            .get(name)
            .or_else(|| self.types.get(name))
            .cloned()
            .unwrap_or_else(|| name.to_owned())
    }

    /// `tokens` の鍵の、型の引数を指す名前を置き換えたもの。型の引数は型の名前空間の名前、
    /// 定数の引数は式の中の名前だけを置き換える (`Foo<N>` の `N` は型の名前かもしれないので
    /// 置き換えない)。`for<'a>` が束縛するライフタイムも、束縛の順の名前にする。
    pub(super) fn apply(&self, tokens: &Tokens) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(tokens.0.len());
        let mut binders: HashMap<&str, String> = HashMap::new();
        let mut binding = false;
        let mut bound = 0usize;
        for (index, token) in tokens.0.iter().enumerate() {
            let text = token.compared();
            let renamed = match token.role {
                Role::Other => {
                    if text == "<"
                        && index > 0
                        && tokens.0[index - 1].role == Role::Other
                        && tokens.0[index - 1].text == "for"
                    {
                        binding = true;
                    } else if text == ">" {
                        binding = false;
                    }
                    None
                }
                Role::Lifetime if binding => {
                    let placeholder = format!("'^{bound}");
                    bound += 1;
                    binders.insert(text, placeholder.clone());
                    Some(placeholder)
                }
                Role::Lifetime => binders
                    .get(text)
                    .or_else(|| self.lifetimes.get(text))
                    .cloned(),
                Role::TypeName => self.types.get(text).cloned(),
                Role::ValueName => self.values.get(text).cloned(),
                Role::Opaque => None,
            };
            out.push(renamed.unwrap_or_else(|| text.to_owned()));
        }
        out
    }
}

/// 位置の名前を既定値に置き換える (既定値がほかの新しい引数を指せば、それも置き換える)。
pub(super) fn substitute_defaults(
    tokens: Vec<String>,
    defaults: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let mut current = tokens;
    for _ in 0..=defaults.len() {
        if !current.iter().any(|token| defaults.contains_key(token)) {
            break;
        }
        current = current
            .into_iter()
            .flat_map(|token| defaults.get(&token).cloned().unwrap_or_else(|| vec![token]))
            .collect();
    }
    current
}

/// 構造体の形の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Form {
    Unit,
    Named,
    Tuple,
}

/// フィールドの鍵 (名前付きは `r#` を外した名前、タプルは位置)。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum FieldKey {
    Name(String),
    Index(usize),
}

/// 1 つのフィールド。
#[derive(Clone, Debug)]
pub(super) struct Field {
    pub(super) key: FieldKey,
    /// 書いたとおりの名前 (名前付きだけ。表示に使う)。
    pub(super) shown_name: String,
    /// 制限の無い `pub` か。`pub(crate)` などは外から見えない。
    pub(super) public: bool,
    /// `#[cfg(..)]` と、`cfg` を含む `#[cfg_attr(..)]`。
    pub(super) conditions: Vec<Attr>,
    pub(super) ty: Tokens,
}

/// 条件の鍵 (並べ替えて比べる)。
pub(super) fn condition_keys(conditions: &[Attr]) -> Vec<Vec<String>> {
    let mut keys: Vec<Vec<String>> = conditions.iter().map(|attr| attr.key.clone()).collect();
    keys.sort();
    keys
}

/// 条件の表示 (`#[cfg(..)] ` を鍵の順に)。
pub(super) fn condition_prefix(conditions: &[Attr]) -> String {
    let mut sorted: Vec<&Attr> = conditions.iter().collect();
    sorted.sort_by(|a, b| a.key.cmp(&b.key));
    sorted.iter().fold(String::new(), |mut out, attr| {
        let _ = write!(out, "#[{}] ", attr.shown);
        out
    })
}

/// 公開でないフィールドの表示。無条件のものがあれば `..`、cfg の付いたものだけなら条件ごとに
/// `#[cfg(..)] ..`。
pub(super) fn private_summary<'a>(fields: impl Iterator<Item = &'a Field>) -> Vec<String> {
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

/// フィールドの前に付いた属性のうち、`cfg` と `cfg` を含む `cfg_attr`。
pub(super) fn field_conditions(attributes: &[Node<'_>], source: &[u8]) -> Option<Vec<Attr>> {
    let mut conditions = Vec::new();
    for attribute in attributes {
        if let Some(attr) = project_attribute(*attribute, source, &["cfg"])? {
            conditions.push(attr);
        }
    }
    Some(conditions)
}

/// 名前付きのフィールド。`inherited` は enum の variant のフィールド (可視性を書けず、enum と
/// 同じだけ見える)。
pub(super) fn named_fields(body: Node<'_>, source: &[u8], inherited: bool) -> Option<Vec<Field>> {
    let mut fields = Vec::new();
    let mut attributes = Vec::new();
    for child in body.named_children(&mut body.walk()) {
        match child.kind() {
            "attribute_item" => attributes.push(child),
            "line_comment" | "block_comment" => {}
            "field_declaration" => {
                let visibility = child
                    .named_children(&mut child.walk())
                    .find(|part| part.kind() == "visibility_modifier");
                let name = child.child_by_field_name("name")?.utf8_text(source).ok()?;
                fields.push(Field {
                    key: FieldKey::Name(unraw(name).to_owned()),
                    shown_name: name.to_owned(),
                    public: inherited || visibility.is_some_and(|v| is_unrestricted_pub(v, source)),
                    conditions: field_conditions(&attributes, source)?,
                    ty: Tokens::of(child.child_by_field_name("type")?, source)?,
                });
                attributes.clear();
            }
            _ => return None,
        }
    }
    attributes.is_empty().then_some(fields)
}

/// タプルのフィールド。`inherited` は [`named_fields`] と同じ。
pub(super) fn tuple_fields(body: Node<'_>, source: &[u8], inherited: bool) -> Option<Vec<Field>> {
    let mut fields = Vec::new();
    let mut attributes = Vec::new();
    let mut visibility = None;
    for child in body.named_children(&mut body.walk()) {
        match child.kind() {
            "attribute_item" => attributes.push(child),
            "line_comment" | "block_comment" => {}
            "visibility_modifier" => visibility = Some(child),
            _ => {
                // 残りは型 (`type` の field)。
                fields.push(Field {
                    key: FieldKey::Index(fields.len()),
                    shown_name: String::new(),
                    public: inherited || visibility.is_some_and(|v| is_unrestricted_pub(v, source)),
                    conditions: field_conditions(&attributes, source)?,
                    ty: Tokens::of(child, source)?,
                });
                attributes.clear();
                visibility = None;
            }
        }
    }
    (attributes.is_empty() && visibility.is_none()).then_some(fields)
}

/// ファイルの中の、名前が `name` の `item_kind` (`struct_item`・`enum_item`・`union_item`) の項目
/// (入れ子の module の中も、書いた順)。
pub(crate) fn rust_items_named<'tree>(
    root: Node<'tree>,
    source: &[u8],
    item_kind: &str,
    name: &str,
) -> Vec<Node<'tree>> {
    let mut found = Vec::new();
    let _ = walk_preorder(root, Children::Named, |node, _| {
        if node.kind() == item_kind
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

/// 旧新の公開の形を比べた結果 (重い順に並べる)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ShapeChange {
    /// 外の利用者から見て同じ (整形・型の引数の改名・外から見えない変更)。
    Unchanged,
    /// 外の利用者を壊さずに広げるか、制限を緩めるだけ。
    Compatible,
    /// 外の利用者を壊し得る。
    Breaking,
}

/// 境界 1 つが寿命の境界 (`'a`) か。
pub(super) fn is_outlives(bound: &[String]) -> bool {
    matches!(bound, [single] if single.starts_with('\''))
}

/// トレイトの境界を比べる。新しい境界が旧い境界に含まれる (緩めるだけ) なら互換。`?Sized` を
/// 外すのは強めること。寿命の境界は [`outlives_set`] でまとめて比べる。
pub(super) fn compare_bounds(
    old: &Param,
    new: &Param,
    old_names: &Renaming,
    new_names: &Renaming,
) -> ShapeChange {
    let traits = |param: &Param, names: &Renaming| -> BTreeSet<Vec<String>> {
        param
            .bounds
            .iter()
            .map(|bound| names.apply(bound))
            .filter(|bound| !is_outlives(bound))
            .collect()
    };
    let (old_traits, new_traits) = (traits(old, old_names), traits(new, new_names));
    let (old_unsized, new_unsized) = (old.unsized_allowed(), new.unsized_allowed());
    // `?Sized` の引数には `dyn Trait` を渡せ、その既定の寿命は引数に書いた寿命の境界で決まる
    // (推論される境界は効かない)。書いた寿命の境界が変わると `S<'a, dyn Trait>` の意味が変わる。
    let written_outlives = |param: &Param, names: &Renaming| -> BTreeSet<Vec<String>> {
        param
            .bounds
            .iter()
            .map(|bound| names.apply(bound))
            .filter(|bound| is_outlives(bound))
            .collect()
    };
    let object_default_changed = old.kind == ParamKind::Type
        && old_unsized
        && new_unsized
        && written_outlives(old, old_names) != written_outlives(new, new_names);
    if !new_traits.is_subset(&old_traits) || (old_unsized && !new_unsized) || object_default_changed
    {
        ShapeChange::Breaking
    } else if new_traits != old_traits || old_unsized != new_unsized {
        ShapeChange::Compatible
    } else {
        ShapeChange::Unchanged
    }
}

/// 寿命の境界の集合 (左辺, ライフタイム)。引数の境界・`where` の述語・フィールドの型から推論
/// される境界を合わせる。型を使う側に暗黙の境界 (implied bounds) として伝わるので、旧新で同じで
/// あることを求める (書いても推論されるだけの境界の付け外しは変わらない)。`skip` の位置の名前
/// (旧い利用では引数を含まない既定値に決まる足した引数) を左辺とする境界は、旧い利用でいつも
/// 満たされるので除く。
pub(super) fn outlives_set(
    generics: &Generics,
    names: &Renaming,
    skip: &BTreeSet<String>,
) -> BTreeSet<(Vec<String>, String)> {
    let mut set = BTreeSet::new();
    for param in &generics.params {
        for bound in &param.bounds {
            if let [lifetime] = names.apply(bound).as_slice()
                && lifetime.starts_with('\'')
            {
                set.insert((vec![names.rename_name(&param.name)], lifetime.clone()));
            }
        }
    }
    for (left, bound) in &generics.predicates {
        if let [lifetime] = names.apply(bound).as_slice()
            && lifetime.starts_with('\'')
        {
            set.insert((names.apply(left), lifetime.clone()));
        }
    }
    for (component, lifetime) in &generics.inferred_outlives {
        set.insert((
            vec![names.rename_name(component)],
            names.rename_name(lifetime),
        ));
    }
    // `skip` の引数を左辺とする境界と、自明な `'a: 'a` を除く。
    set.retain(|(left, lifetime)| {
        !matches!(left.as_slice(), [only] if skip.contains(only) || only == lifetime)
    });
    set
}

/// cfg の付いたフィールドから推論される寿命の境界 (条件の鍵, 成分, ライフタイム)。その設定でだけ
/// 効く暗黙の境界なので、条件ごとに旧新で同じであることを求める。
pub(super) fn conditional_outlives_set(
    generics: &Generics,
    names: &Renaming,
    skip: &BTreeSet<String>,
) -> BTreeSet<(Vec<Vec<String>>, String, String)> {
    generics
        .conditional_outlives
        .keys()
        .map(|(conditions, component, lifetime)| {
            (
                conditions.clone(),
                names.rename_name(component),
                names.rename_name(lifetime),
            )
        })
        .filter(|(_, component, _)| !skip.contains(component))
        .collect()
}

/// 型の引数と `where` を比べる。引数は位置で対応させる (改名は変更にしない)。
pub(super) fn compare_generics(
    old: &Generics,
    new: &Generics,
    old_names: &Renaming,
    new_names: &Renaming,
) -> ShapeChange {
    use ShapeChange::{Breaking, Compatible, Unchanged};
    let (old_lifetimes, old_others) = old.split();
    let (new_lifetimes, new_others) = new.split();
    if old_lifetimes.len() != new_lifetimes.len() || new_others.len() < old_others.len() {
        return Breaking;
    }
    let mut change = Unchanged;
    for (old_param, new_param) in old_lifetimes.iter().zip(&new_lifetimes) {
        change = change.max(compare_bounds(old_param, new_param, old_names, new_names));
    }
    let renamed =
        |names: &Renaming, tokens: &Option<Tokens>| tokens.as_ref().map(|t| names.apply(t));
    for (index, new_param) in new_others.iter().enumerate() {
        let Some(old_param) = old_others.get(index) else {
            // 既定値の付いた引数の追加だけが互換。既定値が境界を満たすとは構文から言えないので、
            // 境界の付いた引数と、既定値が `Sized` と言えない引数は壊す扱い。
            if new_param.default.is_none() || !new_param.bounds.is_empty() {
                return Breaking;
            }
            if new_param.kind == ParamKind::Type {
                // 旧い利用で `Sized` と言える引数: 旧い位置の引数は旧い宣言で `?Sized` でなく、
                // 足した引数は `?Sized` でない (その既定値は前に確かめた)。
                let sized_for_old_uses = |name: &String| {
                    let position = new_others
                        .iter()
                        .position(|param| param.kind == ParamKind::Type && &param.name == name);
                    match position {
                        Some(position) => match old_others.get(position) {
                            Some(old_param) => !old_param.unsized_allowed(),
                            None => !new_others[position].unsized_allowed(),
                        },
                        None => false,
                    }
                };
                // `?Sized` の引数でも、既定値は旧い利用のどれでも正しい型でなければならない。
                // `Sized` を求めなくてよいのは、既定値が前の引数そのもののときだけ。
                let valid = match &new_param.default_sizedness {
                    Sizedness::Param(name) => {
                        new_param.unsized_allowed() || sized_for_old_uses(name)
                    }
                    Sizedness::IfParams(names) => names.iter().all(sized_for_old_uses),
                    Sizedness::Unknown => false,
                };
                if !valid {
                    return Breaking;
                }
            }
            change = change.max(Compatible);
            continue;
        };
        if old_param.kind != new_param.kind
            || renamed(old_names, &old_param.const_type)
                != renamed(new_names, &new_param.const_type)
        {
            return Breaking;
        }
        match (
            renamed(old_names, &old_param.default),
            renamed(new_names, &new_param.default),
        ) {
            (None, Some(_)) => change = change.max(Compatible),
            (old_default, new_default) if old_default != new_default => return Breaking,
            _ => {}
        }
        change = change.max(compare_bounds(old_param, new_param, old_names, new_names));
    }
    let concrete_additions: BTreeSet<String> = new_others
        .iter()
        .enumerate()
        .skip(old_others.len())
        .filter(|(_, param)| param.default_concrete)
        .map(|(index, _)| format!("#{index}"))
        .collect();
    let none = BTreeSet::new();
    if outlives_set(old, old_names, &none) != outlives_set(new, new_names, &concrete_additions)
        || conditional_outlives_set(old, old_names, &none)
            != conditional_outlives_set(new, new_names, &concrete_additions)
    {
        return Breaking;
    }
    let predicates =
        |generics: &Generics, names: &Renaming| -> BTreeSet<(Vec<String>, Vec<String>)> {
            generics
                .predicates
                .iter()
                .map(|(left, bound)| (names.apply(left), names.apply(bound)))
                .filter(|(_, bound)| !is_outlives(bound))
                .collect()
        };
    let (old_predicates, new_predicates) = (predicates(old, old_names), predicates(new, new_names));
    if !new_predicates.is_subset(&old_predicates) {
        return Breaking;
    }
    if new_predicates != old_predicates {
        change = change.max(Compatible);
    }
    change
}

/// 整数のリテラル (`8`・`0x8`・`1_0`。接尾辞は無い) の値。
fn unsuffixed_integer(literal: &str) -> Option<u128> {
    let digits: String = literal.chars().filter(|c| *c != '_').collect();
    let (radix, digits) = match digits.get(..2) {
        Some("0x") => (16, &digits[2..]),
        Some("0o") => (8, &digits[2..]),
        Some("0b") => (2, &digits[2..]),
        _ => (10, digits.as_str()),
    };
    u128::from_str_radix(digits, radix).ok()
}

/// `#[repr]` の項目 1 つを、意味の同じ書き方にそろえる (`packed(1)` と `packed(1,)` は `packed`、
/// `align(0x8)` は `align(8)`。Rust の参照の alignment modifiers)。
pub(super) fn normalize_repr_item(item: Vec<String>) -> Vec<String> {
    // 属性の引数の中の識別子は `r#` を保つので外す (`r#packed` は `packed`)。
    let item: Vec<String> = item.iter().map(|token| unraw(token).to_owned()).collect();
    if let [name, open, value, rest @ ..] = item.as_slice()
        && matches!(name.as_str(), "packed" | "align")
        && open == "("
        && (rest == [")"] || rest == [",", ")"])
        && let Some(value) = unsuffixed_integer(value)
    {
        if name == "packed" && value == 1 {
            return vec![name.clone()];
        }
        return vec![
            name.clone(),
            open.clone(),
            value.to_string(),
            ")".to_owned(),
        ];
    }
    item
}

/// 無条件の `#[repr(..)]` の項目 (`C`・`u8`・`align(8)` など) の集合。属性を分けて書いても、
/// 並べ替えても、末尾に `,` があっても、`packed` を `packed(1)` と書いても同じ。
pub(super) fn repr_items(layout: &[Attr]) -> BTreeSet<Vec<String>> {
    let mut items: BTreeSet<Vec<String>> = layout
        .iter()
        .filter(|attr| !attr.is_conditional())
        .flat_map(|attr| {
            // `repr` `(` 項目 `,` .. `)`
            split_repr_items(
                attr.key
                    .get(2..attr.key.len().saturating_sub(1))
                    .unwrap_or_default(),
            )
        })
        .collect();
    keep_widest_align(&mut items);
    items
}

/// `align(N)` が複数あれば、効く最大の値のものだけを残す (Rust の参照の alignment modifiers)。
pub(super) fn keep_widest_align(items: &mut BTreeSet<Vec<String>>) {
    let align = |item: &Vec<String>| match item.as_slice() {
        [name, open, value, close] if name == "align" && open == "(" && close == ")" => {
            value.parse::<u128>().ok()
        }
        _ => None,
    };
    let Some(widest) = items.iter().filter_map(align).max() else {
        return;
    };
    items.retain(|item| align(item).is_none_or(|value| value == widest));
}

/// `repr(..)` の括弧の中 `inner` の項目 (深さ 0 の `,` で分け、意味の同じ書き方にそろえる)。
pub(super) fn split_repr_items(inner: &[String]) -> Vec<Vec<String>> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut item = Vec::new();
    for token in inner {
        match token.as_str() {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            "," if depth == 0 => {
                if !item.is_empty() {
                    items.push(normalize_repr_item(std::mem::take(&mut item)));
                }
                continue;
            }
            _ => {}
        }
        item.push(token.clone());
    }
    if !item.is_empty() {
        items.push(normalize_repr_item(item));
    }
    items
}

/// `tokens[start..]` の、深さ 0 の閉じ括弧の位置 (無ければ長さ)。
pub(super) fn closing_paren(tokens: &[String], start: usize) -> usize {
    let mut depth = 0usize;
    let mut position = start;
    while let Some(token) = tokens.get(position) {
        match token.as_str() {
            "(" => depth += 1,
            ")" if depth == 0 => return position,
            ")" => depth -= 1,
            _ => {}
        }
        position += 1;
    }
    tokens.len()
}

/// `#[repr]` を含む `#[cfg_attr]` の鍵 (`repr(..)` の項目の書き方と識別子の `r#` をそろえる)。
pub(super) fn conditional_repr(layout: &[Attr]) -> BTreeSet<Vec<String>> {
    layout
        .iter()
        .filter(|attr| attr.is_conditional())
        .map(|attr| normalize_layout_key(&attr.key))
        .collect()
}

/// `#[cfg_attr]` の鍵の `repr(..)` を、項目を意味の同じ書き方にそろえて並べたもの (属性の引数の
/// 中では識別子の `r#` が残るので `r#repr` も読む)。条件の字面は変えない (`r#true` は名前の付いた
/// cfg で、定数の `true` ではない)。
fn normalize_layout_key(key: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(key.len());
    let mut position = 0;
    while let Some(token) = key.get(position) {
        if unraw(token) == "repr" && key.get(position + 1).is_some_and(|next| next == "(") {
            let end = closing_paren(key, position + 2);
            let mut items: BTreeSet<Vec<String>> = split_repr_items(&key[position + 2..end])
                .into_iter()
                .collect();
            keep_widest_align(&mut items);
            out.extend(["repr".to_owned(), "(".to_owned()]);
            for (index, item) in items.into_iter().enumerate() {
                if index > 0 {
                    out.push(",".to_owned());
                }
                out.extend(item);
            }
            out.push(")".to_owned());
            position = end + 1;
            continue;
        }
        out.push(token.clone());
        position += 1;
    }
    out
}
