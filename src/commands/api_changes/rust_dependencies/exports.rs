//! module のスコープが作る名前 (宣言と `use`) と、その有る条件・名前空間・見える範囲 (Issue #306・#318)。
//!
//! 再公開した名前を解くために、ファイルのスコープ (ファイルの module と inline module) ごとに、名前を作る
//! もの (宣言した項目、`use` の named の取り込み、`extern crate`、glob) と、名前を列挙できない項目 (項目の
//! 位置のマクロの呼び出し、展開できない `use`) を集める。名前空間は型・値・マクロを分ける (関数・定数・
//! static は値、構造体は型と、タプル・ユニット構造体ならコンストラクタの値も、ほかの項目と module は型、
//! `macro_rules!` はマクロ)。group の中の `self` の取り込み (`use a::b::{self}`) は型だけにする。見える
//! 範囲は crate root からの module の経路 (`pub` は None) で、タプル構造体のコンストラクタはフィールドの
//! 見える範囲でも狭める。有る条件は項目と同じく、宣言自身・囲む inline module・ファイルの先頭の `cfg` と
//! `cfg` に絞った `cfg_attr`。

use super::*;
use crate::engine::rust_use_tree::expand_rust_use_tree_edges_marking_self;
use std::collections::BTreeMap;

/// 型の名前空間。
pub(crate) const TYPE: u8 = 1;
/// 値の名前空間。
pub(crate) const VALUE: u8 = 2;
/// マクロの名前空間。
pub(crate) const MACRO: u8 = 4;
/// `use` が取り込む名前空間 (全部)。
pub(crate) const ALL: u8 = TYPE | VALUE | MACRO;

/// 名前を作るものの種類。
#[derive(Clone, Debug)]
pub(crate) enum ProviderKind {
    /// 宣言した項目 (種類の表示)。
    Declared(&'static str),
    /// `use` の named の取り込み (先の module の経路 (書いた形)、先の名前、経路の起点)。
    Use {
        module: Vec<String>,
        item: String,
        origin: UsePathOrigin,
    },
    /// `extern crate` (crate の名前)。
    ExternCrate(String),
}

/// スコープの名前を作るもの 1 つ。
#[derive(Clone, Debug)]
pub(crate) struct Provider {
    pub(crate) name: String,
    /// 名前空間 ([`TYPE`]・[`VALUE`]・[`MACRO`] のビット)。`use` は先の名前空間に従う。
    pub(crate) namespaces: u8,
    /// 見える範囲 (この module の経路から見える module。`pub` (制限なし) は None)。
    pub(crate) scope: Option<Vec<String>>,
    /// 有る条件 (読めない属性があれば None)。
    pub(crate) availability: Option<Vec<AvailabilityAttr>>,
    /// 有る条件の生のトークン。
    pub(crate) availability_raw: Vec<String>,
    pub(crate) kind: ProviderKind,
    /// `use`・`extern crate` の字面 (報告に添える)。宣言は空。
    pub(crate) text: String,
}

/// スコープの glob の取り込み。
#[derive(Clone, Debug)]
pub(crate) struct Glob {
    /// 見える範囲 (`pub` は None)。
    pub(crate) scope: Option<Vec<String>>,
    pub(crate) availability: Option<Vec<AvailabilityAttr>>,
    pub(crate) availability_raw: Vec<String>,
    /// 先の module の経路 (書いた形) と経路の起点。
    pub(crate) module: Vec<String>,
    pub(crate) origin: UsePathOrigin,
    pub(crate) text: String,
}

/// 名前を列挙できない項目 (項目の位置のマクロの呼び出し、展開できない `use`)。
#[derive(Clone, Debug)]
pub(crate) struct Opaque {
    pub(crate) availability: Option<Vec<AvailabilityAttr>>,
    pub(crate) availability_raw: Vec<String>,
    pub(crate) text: String,
    /// 引数の字面に現れる名前 (マクロが作り得る名前はこの中か、呼び出すマクロの本体にあるとみなす)。
    pub(crate) mentions: Vec<String>,
    /// 呼び出すマクロの名前 (`use` なら None)。
    pub(crate) macro_name: Option<String>,
    /// 引数の中で呼び出すマクロの名前 (`inner!(..)`)。
    pub(crate) calls: Vec<String>,
    /// 呼び出しの経路の名前 (`crate::selected::m!` の `crate`・`selected`。引数の中の呼び出しの経路も)。
    pub(crate) paths: Vec<String>,
}

/// `macro_rules!` の本体。
#[derive(Clone, Debug)]
pub(crate) struct MacroBody {
    pub(crate) name: String,
    /// 本体に現れる名前。
    pub(crate) mentions: Vec<String>,
    /// 本体の中で呼び出すマクロの名前。
    pub(crate) calls: Vec<String>,
    /// 本体の中の呼び出しの経路の名前 (`a::b!` の `a`)。
    pub(crate) paths: Vec<String>,
    /// 本体が、引数 (メタ変数) をマクロの名前か経路に使い得るか (`$callee!(..)`・`$path::m!(..)`)。
    pub(crate) dynamic: bool,
    /// 本体が、引数 (メタ変数) を渡して呼ぶマクロの名前 (その先が crate に定義の無いマクロなら、引数をマクロの
    /// 名前か経路に使い得る)。
    pub(crate) forwards: Vec<String>,
    /// 定義の字面と有る条件 (旧新の比較に使う。同じ名前の定義は後のものが前のものを隠すので、条件で呼ぶ
    /// 本体が変わる)。
    pub(crate) text: String,
}

/// スコープの名前を作るもの。
#[derive(Clone, Debug, Default)]
pub(crate) struct ScopeExports {
    pub(crate) providers: Vec<Provider>,
    pub(crate) globs: Vec<Glob>,
    pub(crate) opaque: Vec<Opaque>,
    /// `#[macro_export]` の `macro_rules!` (crate root の名前を作る)。
    pub(crate) exported_macros: Vec<Provider>,
    /// `macro_rules!` の本体。
    pub(crate) macro_bodies: Vec<MacroBody>,
    /// マクロの定義 (`def 名前`)・呼び出し (`call 名前`) と `mod` の宣言 (`mod 名前`) の並び (定義か呼び出しの
    /// あるスコープだけ)。後の定義が前の定義を隠し、呼び出しと子の module はそれより前の定義を見る (呼び出しも
    /// マクロを定義し得る) ので、並びが変われば呼ぶ本体が変わり得る。字面と条件の指紋を付ける (同じ名前の定義の
    /// 本体や呼び出しの引数、cfg で分けた module の入れ替えでも変わる)。
    pub(crate) macro_order: Vec<String>,
}

/// スコープの項目の並び `order` ((種類, 名前, 字面と条件の指紋))。マクロの定義か呼び出しか、`#[macro_use]` の
/// module のあるスコープだけ (定義と子の module の前後で、子の呼び出しの見る定義が変わり得る。無ければ空)。
fn macro_order(order: &[(&'static str, String, String)]) -> Vec<String> {
    if order.iter().all(|(kind, _, _)| *kind == "mod") {
        return Vec::new();
    }
    order
        .iter()
        .map(|(kind, name, detail)| format!("{kind} {name} {detail}"))
        .collect()
}

/// 字面の指紋 (並びの比較に使う短い字面)。
fn text_digest(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 見える範囲を、見える module の経路 (`pub` は None) にする。
fn scope_of(visibility: Visibility) -> Option<Vec<String>> {
    match visibility {
        Visibility::Public => None,
        Visibility::Within(module) => Some(module),
    }
}

/// 2 つの見える範囲の狭いほう (どちらも同じ module から見えるので、経路の長いほう)。
pub(crate) fn narrower(a: Option<Vec<String>>, b: Option<Vec<String>>) -> Option<Vec<String>> {
    match (a, b) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => Some(if a.len() >= b.len() { a } else { b }),
    }
}

/// タプル構造体・ユニット構造体 `item` (module `current` の中、見える範囲 `scope`) のコンストラクタ (値の
/// 名前空間) の見える範囲: 構造体と全てのフィールドの見える範囲の狭いほう (`#[non_exhaustive]` なら
/// `pub(crate)` まで)。条件の付いた制限 (`cfg_attr` の `non_exhaustive`、`cfg` の付いたフィールド) は、
/// 設定によって効くかが変わるので、効く場合の狭い範囲と、効かない場合の広い範囲を返す (同じなら None)。
/// フィールドに名前のある構造体は値の名前を作らない (None)。
#[allow(clippy::type_complexity)]
fn constructor_scope(
    item: tree_sitter::Node<'_>,
    source: &[u8],
    current: &[String],
    scope: &Option<Vec<String>>,
) -> Option<(Option<Vec<String>>, Option<Option<Vec<String>>>)> {
    let mut certain = scope.clone();
    let mut conditional = scope.clone();
    let mut uncertain = false;
    match attribute_named(item, source, "non_exhaustive") {
        Some(None) => certain = narrower(certain, Some(Vec::new())),
        Some(Some(_)) => {
            uncertain = true;
            conditional = narrower(conditional, Some(Vec::new()));
        }
        None => {}
    }
    if let Some(body) = item.child_by_field_name("body") {
        match body.kind() {
            "field_declaration_list" => return None,
            "ordered_field_declaration_list" => {
                let mut modifier: Option<Option<Vec<String>>> = None;
                let mut attributed = false;
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    match child.kind() {
                        "visibility_modifier" => {
                            modifier = Some(scope_of(modifier_visibility(child, source, current)));
                        }
                        "attribute_item" => {
                            let text = child.utf8_text(source).unwrap_or_default();
                            attributed |= text.contains("cfg");
                        }
                        "line_comment" | "block_comment" => {}
                        _ => {
                            let field = modifier.take().unwrap_or_else(|| Some(current.to_vec()));
                            if std::mem::take(&mut attributed) {
                                uncertain = true;
                                conditional = narrower(conditional, field);
                            } else {
                                certain = narrower(certain, field);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let narrow = narrower(certain.clone(), conditional);
    Some(if uncertain && narrow != certain {
        (narrow, Some(certain))
    } else {
        (certain, None)
    })
}

/// 項目 `item` の前の属性のうち、`name` (`macro_export`・`non_exhaustive`・`macro_use`) を付けるもの。直接
/// 付けるなら `Some(None)`、`cfg_attr` で条件付きに付けるなら `Some(Some(属性の字面))`、付けなければ None。
fn attribute_named(
    item: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
) -> Option<Option<String>> {
    let mut found: Option<Option<String>> = None;
    let mut previous = item.prev_named_sibling();
    while let Some(node) = previous {
        match node.kind() {
            "attribute_item" => match attribute_applies(node, source, name) {
                Some(false) => return Some(None),
                Some(true) => found = Some(Some(attribute_text(node, source))),
                None => {}
            },
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = node.prev_named_sibling();
    }
    found
}

/// 項目 `item` の前の属性のうち、`name` を (直接か `cfg_attr` で) 付けるものの字面 (書いた順)。
fn attribute_texts(item: tree_sitter::Node<'_>, source: &[u8], name: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut previous = item.prev_named_sibling();
    while let Some(node) = previous {
        match node.kind() {
            "attribute_item" => {
                if attribute_applies(node, source, name).is_some() {
                    found.push(attribute_text(node, source));
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = node.prev_named_sibling();
    }
    found.reverse();
    found
}

/// 属性の節 `node` (`attribute_item`・`inner_attribute_item`) が属性 `name` を付けるか。直接なら `Some(false)`、
/// `cfg_attr` で条件付きなら `Some(true)` (入れ子の `cfg_attr` も)、付けなければ None。字句は tree-sitter の
/// 読みのまま比べる (文字列・raw 文字列・文字・lifetime・コメントを字面から読み直さない)。読めない (構文の
/// 誤りを含む) 節は、`name` の字句を持てば条件付きとする。
fn attribute_applies(node: tree_sitter::Node<'_>, source: &[u8], name: &str) -> Option<bool> {
    let mut cursor = node.walk();
    let attribute = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "attribute");
    let Some(attribute) = attribute.filter(|_| !node.has_error()) else {
        return has_token(node, source, name).then_some(true);
    };
    let mut cursor = attribute.walk();
    let path = attribute
        .named_children(&mut cursor)
        .find(|child| !matches!(child.kind(), "line_comment" | "block_comment"))?;
    let path = unraw(path.utf8_text(source).ok()?);
    if path == name {
        return Some(false);
    }
    (path == "cfg_attr"
        && attribute
            .child_by_field_name("arguments")
            .is_some_and(|arguments| cfg_attr_applies(arguments, source, name)))
    .then_some(true)
}

/// `cfg_attr` の引数の字句の木 `arguments` (`(条件, 属性, ..)`) の、条件の後ろの属性に `name` があるか
/// (入れ子の `cfg_attr` も)。属性は、最上位の `,` の字句で分ける。入れ子の `cfg_attr` は明示的な
/// スタックで辿る (深い入れ子でコールスタックを溢れさせない)。辿る順は再帰と同じく、書いた順に
/// 属性を調べ、入れ子の `cfg_attr` は見つけた位置で先に調べて、最初に付けると分かった時点で終える。
fn cfg_attr_applies(arguments: tree_sitter::Node<'_>, source: &[u8], name: &str) -> bool {
    cfg_attr_applies_visiting(arguments, source, name, |_| {})
}

/// [`cfg_attr_applies`] の本体。`visit` は、調べる `cfg_attr` の引数の字句の木を調べる順に受け取る
/// (テストで辿る順と短絡を確かめる)。
fn cfg_attr_applies_visiting<'tree>(
    arguments: tree_sitter::Node<'tree>,
    source: &[u8],
    name: &str,
    mut visit: impl FnMut(tree_sitter::Node<'tree>),
) -> bool {
    let text = |node: &tree_sitter::Node<'_>| unraw(node.utf8_text(source).unwrap_or_default());
    /// `cfg_attr` の引数の、条件の後ろの属性 (書いた順の逆に積み、末尾から取り出す)。
    fn attributes(arguments: tree_sitter::Node<'_>) -> Vec<Vec<tree_sitter::Node<'_>>> {
        let mut cursor = arguments.walk();
        let tokens: Vec<tree_sitter::Node<'_>> = arguments
            .children(&mut cursor)
            .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"))
            .collect();
        // 最初と最後の字句は括弧。
        let inner = tokens
            .get(1..tokens.len().saturating_sub(1))
            .unwrap_or_default();
        let mut attributes: Vec<Vec<tree_sitter::Node<'_>>> = inner
            .split(|token| token.kind() == ",")
            .skip(1)
            .map(<[tree_sitter::Node<'_>]>::to_vec)
            .collect();
        attributes.reverse();
        attributes
    }
    visit(arguments);
    let mut pending = vec![attributes(arguments)];
    while let Some(frame) = pending.last_mut() {
        let Some(attribute) = frame.pop() else {
            pending.pop();
            continue;
        };
        match attribute.as_slice() {
            [path, rest @ ..] if path.kind() == "identifier" && text(path) == name => {
                if rest
                    .first()
                    .is_none_or(|next| matches!(next.kind(), "token_tree" | "="))
                {
                    return true;
                }
            }
            [path, nested, ..]
                if path.kind() == "identifier"
                    && text(path) == "cfg_attr"
                    && nested.kind() == "token_tree" =>
            {
                visit(*nested);
                pending.push(attributes(*nested));
            }
            _ => {}
        }
    }
    false
}

/// 節 `node` の中に、字面が `name` の字句 (葉) があるか。
fn has_token(node: tree_sitter::Node<'_>, source: &[u8], name: &str) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.child_count() == 0
            && current.utf8_text(source).ok().map(unraw).as_deref() == Some(name)
        {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.children(&mut cursor));
    }
    false
}

/// 中身 `body` (ファイルか inline module の中身) の先頭の内側の属性が `#![macro_use]` を持つか (`cfg_attr` も)。
fn inner_macro_use(body: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = body.walk();
    let found: Vec<String> = body
        .named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment" | "shebang"))
        .take_while(|child| child.kind() == "inner_attribute_item")
        .filter(|child| attribute_applies(*child, source, "macro_use").is_some())
        .map(|child| attribute_text(child, source))
        .collect();
    (!found.is_empty()).then(|| found.join(" "))
}

/// 属性の節 `node` の字面。コメントを除いて字句を詰め、続く 2 つの字句の境が語の文字 (英数字・`_`) どうし
/// なら空白 1 つを挟む (`docs!(en r"x")` の `en` と `r"x"` を 1 つにしない)。字句は tree-sitter の読みのまま
/// (文字列・raw 文字列・文字は 1 つの字句)。
fn attribute_text(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    use crate::engine::tree_walk::{Children, Walk, walk_preorder};
    use std::ops::ControlFlow;
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::new();
    let _ = walk_preorder(node, Children::All, |current, _| {
        let (token, action) = match current.kind() {
            "line_comment" | "block_comment" => (None, Walk::Skip),
            "string_literal" | "raw_string_literal" | "char_literal" => (Some(current), Walk::Skip),
            _ if current.child_count() == 0 => (Some(current), Walk::Skip),
            _ => (None, Walk::Descend),
        };
        if let Some(token) = token {
            let text = token.utf8_text(source).unwrap_or_default();
            if out.chars().next_back().is_some_and(word) && text.chars().next().is_some_and(word) {
                out.push(' ');
            }
            out.push_str(text);
        }
        ControlFlow::<(), Walk>::Continue(action)
    });
    out
}

/// use tree の引数 `argument` (module `current` の中) のうち、`crate`・`self`・`super` だけの経路を別名で
/// 取り込むもの (`crate as name`・`super as name`・`super::super as name`、crate root に着く
/// `crate::{self as name}`) と、その先の module (crate root からの経路)。展開の edge には現れない (crate root の
/// 外の group の `self` (`super::{self as name}`) は edge になる)。
fn keyword_aliases(
    argument: tree_sitter::Node<'_>,
    source: &[u8],
    current: &[String],
) -> Vec<(String, Vec<String>)> {
    let text = |node: tree_sitter::Node<'_>| {
        node.utf8_text(source)
            .ok()
            .map(|text| unraw(text.trim()).into_owned())
    };
    // 経路の字面が `crate`・`self`・`super` だけでできていれば、その先の module。
    let target = |path: tree_sitter::Node<'_>| -> Option<Vec<String>> {
        let written = text(path)?;
        let mut depth = current.len();
        for (position, segment) in written.split("::").map(str::trim).enumerate() {
            match segment {
                "crate" if position == 0 => depth = 0,
                "self" if position == 0 => {}
                "super" => depth = depth.checked_sub(1)?,
                _ => return None,
            }
        }
        Some(current[..depth].to_vec())
    };
    let mut found = Vec::new();
    // (節, 経路の付いた group の中か)。group の中の経路は group の経路に続く。
    let mut pending = vec![(argument, false)];
    while let Some((node, grouped)) = pending.pop() {
        match node.kind() {
            "use_as_clause" if !grouped => {
                if let (Some(path), Some(alias)) = (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("alias").and_then(text),
                ) && matches!(
                    path.kind(),
                    "crate" | "self" | "super" | "scoped_identifier"
                ) && let Some(module) = target(path)
                {
                    found.push((alias, module));
                }
            }
            "scoped_use_list" => {
                if let (Some(path), Some(list)) = (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("list"),
                ) && target(path).is_some_and(|module| module.is_empty())
                {
                    let mut cursor = list.walk();
                    for item in list.named_children(&mut cursor) {
                        if item.kind() == "use_as_clause"
                            && item.child_by_field_name("path").and_then(text).as_deref()
                                == Some("self")
                            && let Some(alias) = item.child_by_field_name("alias").and_then(text)
                        {
                            found.push((alias, Vec::new()));
                        }
                    }
                }
            }
            _ => {}
        }
        let inside = grouped || node.kind() == "scoped_use_list";
        let mut cursor = node.walk();
        pending.extend(
            node.named_children(&mut cursor)
                .map(|child| (child, inside)),
        );
    }
    found.retain(|(alias, _)| alias != "_");
    found
}

/// トークンの字面が、引数 (メタ変数。`$crate` は除く) をマクロの名前か経路に使い得るか: `!` か `::` の直前が
/// メタ変数か、メタ変数を含む繰り返しの終わり (`$($callee)+ !`・`$($segment)::+ !`)。
fn passes_arguments(tokens: &[String]) -> bool {
    let groups = Groups::new(tokens);
    (1..tokens.len()).any(|index| {
        if tokens[index] != "!" && tokens[index] != "::" {
            return false;
        }
        let before = index - 1;
        if is_argument(&tokens[before]) {
            return true;
        }
        // 繰り返しの演算子 (`+`・`*`・`?`) と、その前の区切りの字句を戻って `)` に着けば繰り返しの終わり。
        if !matches!(tokens[before].as_str(), "+" | "*" | "?") || before == 0 {
            return false;
        }
        let mut at = before - 1;
        if tokens[at] != ")" {
            let Some(previous) = at.checked_sub(1) else {
                return false;
            };
            at = previous;
        }
        tokens[at] == ")"
            && groups.open_of(at).is_some_and(|open| {
                open > 0 && tokens[open - 1] == "$" && groups.has_argument(open, at)
            })
    })
}

/// 字句が引数 (メタ変数。定義した crate を指す `$crate` は除く) か。
fn is_argument(token: &str) -> bool {
    token
        .strip_prefix("metavariable:")
        .is_some_and(|name| name != "$crate")
}

/// 字句の並びの括弧の対応と、引数の字句の数の累積 (区間に引数があるかを 1 度のなめで引けるようにする)。
struct Groups {
    /// 閉じ括弧の位置 → 対応する開き括弧の位置。
    opens: Vec<Option<usize>>,
    /// 位置 `i` より前の引数の字句の数。
    counts: Vec<usize>,
}

impl Groups {
    fn new(tokens: &[String]) -> Self {
        let mut opens = vec![None; tokens.len()];
        let mut counts = Vec::with_capacity(tokens.len() + 1);
        counts.push(0);
        let mut stack: Vec<usize> = Vec::new();
        for (index, token) in tokens.iter().enumerate() {
            match token.as_str() {
                "(" | "[" | "{" => stack.push(index),
                ")" | "]" | "}" => opens[index] = stack.pop(),
                _ => {}
            }
            counts.push(counts[index] + usize::from(is_argument(token)));
        }
        Self { opens, counts }
    }

    /// 閉じ括弧 `close` に対応する開き括弧の位置。
    fn open_of(&self, close: usize) -> Option<usize> {
        self.opens.get(close).copied().flatten()
    }

    /// 位置 `open` から `close` まで (両端を含む) に引数の字句があるか。
    fn has_argument(&self, open: usize, close: usize) -> bool {
        self.counts[close + 1] > self.counts[open]
    }
}

/// トークンの字面のうち、引数 (メタ変数。`$crate` は除く) を渡すマクロの呼び出しの名前 (`name!(.. $x ..)`。
/// 入れ子の呼び出しの中の引数も外の呼び出しに渡る)。1 度のなめで求める。
fn forwarded_calls(tokens: &[String]) -> Vec<String> {
    let groups = Groups::new(tokens);
    let mut found: Vec<String> = Vec::new();
    for (close, token) in tokens.iter().enumerate() {
        if !matches!(token.as_str(), ")" | "]" | "}") {
            continue;
        }
        let Some(open) = groups.open_of(close) else {
            continue;
        };
        if open < 2 || tokens[open - 1] != "!" || !groups.has_argument(open, close) {
            continue;
        }
        if let Some(name) = tokens[open - 2]
            .split_once(':')
            .filter(|(kind, _)| kind.ends_with("identifier"))
            .map(|(_, text)| unraw(text).into_owned())
        {
            found.push(name);
        }
    }
    found.sort();
    found.dedup();
    found
}

/// トークンの字面のうち、マクロの呼び出しの名前 (`name` の直後が `!`)。
fn calls_in(tokens: &[String]) -> Vec<String> {
    let mut calls: Vec<String> = tokens
        .windows(2)
        .filter_map(|pair| {
            let (kind, text) = pair[0].split_once(':')?;
            (kind.ends_with("identifier") && pair[1] == "!").then(|| unraw(text).into_owned())
        })
        .collect();
    calls.sort();
    calls.dedup();
    calls
}

/// トークンの字面のうち、マクロの呼び出し (`a::b!`) の経路の名前 (`a`。呼ぶ名前の `b` は含めない)。
fn call_paths_in(tokens: &[String]) -> Vec<String> {
    let name = |token: &String| {
        token
            .split_once(':')
            .filter(|(kind, _)| !kind.is_empty())
            .map(|(_, text)| unraw(text).into_owned())
    };
    let mut paths: Vec<String> = Vec::new();
    for (index, pair) in tokens.windows(2).enumerate() {
        if pair[1] != "!" || name(&pair[0]).is_none() {
            continue;
        }
        let mut at = index;
        while at >= 2 && tokens[at - 1] == "::" {
            let Some(segment) = name(&tokens[at - 2]) else {
                break;
            };
            paths.push(segment);
            at -= 2;
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// トークンの字面 (`identifier:name` など) のうち、名前のもの。
fn names_in(tokens: &[String]) -> Vec<String> {
    let mut names: Vec<String> = tokens
        .iter()
        .filter_map(|token| {
            let (kind, text) = token.split_once(':')?;
            kind.ends_with("identifier")
                .then(|| unraw(text).into_owned())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// ファイルの索引 `index` (crate root からの経路 `module` の module) のスコープ (inline module の経路) ごと
/// の、名前を作るもの。
pub(crate) fn exports(
    index: &DependencyIndex,
    module: &[String],
) -> BTreeMap<Vec<String>, ScopeExports> {
    let source = &index.source;
    let mut result: BTreeMap<Vec<String>, ScopeExports> = BTreeMap::new();
    let mut pending: Vec<(tree_sitter::Node<'_>, Vec<String>)> =
        vec![(index.tree.root_node(), Vec::new())];
    // enum の variant を名前として持つ、enum の名前の擬似的なスコープ (`E::V` を module の経路と同じく解く)。
    let mut enum_scopes: Vec<(Vec<String>, Vec<Provider>)> = Vec::new();
    while let Some((scope_node, scope)) = pending.pop() {
        let mut current = module.to_vec();
        current.extend(scope.iter().cloned());
        let exports = result.entry(scope.clone()).or_default();
        // マクロの定義・呼び出しと `mod` の宣言の並び。中身の内側の `#![macro_use]` (この module のマクロを
        // 親の後の textual scope へ出す) も。
        let mut order: Vec<(&'static str, String, String)> = Vec::new();
        // このスコープで `#[macro_export]` するマクロの名前。同じスコープの `use m as x;` は、textual scope の
        // マクロ (crate root の `m` と同じもの。見える範囲もそれ) を取り込む。
        let exported_here: std::collections::HashSet<String> = {
            let mut cursor = scope_node.walk();
            scope_node
                .named_children(&mut cursor)
                .filter(|child| {
                    child.kind() == "macro_definition"
                        && attribute_named(*child, source, "macro_export") == Some(None)
                })
                .filter_map(|child| {
                    child
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                        .map(|name| unraw(name).into_owned())
                })
                .collect()
        };
        if let Some(attribute) = inner_macro_use(scope_node, source) {
            order.push(("use-scope", String::new(), text_digest(&attribute)));
        }
        let mut cursor = scope_node.walk();
        for item in scope_node.named_children(&mut cursor) {
            let item_scope = scope_of(visibility(item, source, &current));
            let named = |field: &str| {
                item.child_by_field_name(field)
                    .and_then(|name| name.utf8_text(source).ok())
                    .map(|name| unraw(name).into_owned())
            };
            let declared = |kind: &'static str, namespaces: u8, scope: Option<Vec<String>>| {
                let (availability, availability_raw) = item_availability(item, source);
                named("name").map(|name| Provider {
                    name,
                    namespaces,
                    scope,
                    availability,
                    availability_raw,
                    kind: ProviderKind::Declared(kind),
                    text: String::new(),
                })
            };
            match item.kind() {
                "function_item" | "function_signature_item" => {
                    exports
                        .providers
                        .extend(declared("function", VALUE, item_scope));
                }
                "const_item" => {
                    exports
                        .providers
                        .extend(declared("constant", VALUE, item_scope));
                }
                "static_item" => {
                    exports
                        .providers
                        .extend(declared("static", VALUE, item_scope));
                }
                "struct_item" => {
                    let constructor = constructor_scope(item, source, &current, &item_scope);
                    exports
                        .providers
                        .extend(declared("struct", TYPE, item_scope));
                    if let Some((narrow, wide)) = constructor {
                        exports.providers.extend(declared("struct", VALUE, narrow));
                        // 条件付きの制限が効かない設定の広い範囲は、有るかを決められないものにする。
                        if let Some(wide) = wide
                            && let Some(mut provider) = declared("struct", VALUE, wide)
                        {
                            provider.availability = None;
                            provider
                                .availability_raw
                                .push("conditional constructor".to_owned());
                            exports.providers.push(provider);
                        }
                    }
                }
                "enum_item" => {
                    exports
                        .providers
                        .extend(declared("enum", TYPE, item_scope.clone()));
                    if let (Some(name), Some(body)) =
                        (named("name"), item.child_by_field_name("body"))
                    {
                        let (enum_availability, enum_raw) = item_availability(item, source);
                        let mut variants: Vec<Provider> = Vec::new();
                        let mut variant_cursor = body.walk();
                        for variant in body.named_children(&mut variant_cursor) {
                            if variant.kind() != "enum_variant" {
                                continue;
                            }
                            let Some(variant_name) = variant
                                .child_by_field_name("name")
                                .and_then(|name| name.utf8_text(source).ok())
                                .map(|name| unraw(name).into_owned())
                            else {
                                continue;
                            };
                            let (own, own_raw) = item_availability(variant, source);
                            let availability = enum_availability
                                .clone()
                                .zip(own)
                                .map(|(outer, inner)| outer.into_iter().chain(inner).collect());
                            let mut availability_raw = enum_raw.clone();
                            availability_raw.extend(own_raw);
                            let provider = |namespaces: u8, scope: Option<Vec<String>>| Provider {
                                name: variant_name.clone(),
                                namespaces,
                                scope,
                                availability: availability.clone(),
                                availability_raw: availability_raw.clone(),
                                kind: ProviderKind::Declared("variant"),
                                text: String::new(),
                            };
                            // 名前の付いたフィールドの variant は型だけ、ほかは型と値 (コンストラクタ)。
                            // `#[non_exhaustive]` の variant のコンストラクタは crate の外から使えない
                            // (`cfg_attr` で付くなら、効く設定の狭い範囲と、有るかを決めない広い範囲)。
                            if variant
                                .child_by_field_name("body")
                                .is_some_and(|body| body.kind() == "field_declaration_list")
                            {
                                variants.push(provider(TYPE, item_scope.clone()));
                                continue;
                            }
                            let restricted = attribute_named(variant, source, "non_exhaustive");
                            let narrow = narrower(item_scope.clone(), Some(Vec::new()));
                            match restricted {
                                Some(conditional) if narrow != item_scope => {
                                    variants.push(provider(TYPE, item_scope.clone()));
                                    variants.push(provider(VALUE, narrow));
                                    if let Some(attribute) = conditional {
                                        let mut wide = provider(VALUE, item_scope.clone());
                                        wide.availability = None;
                                        wide.availability_raw.push(attribute);
                                        variants.push(wide);
                                    }
                                }
                                _ => variants.push(provider(TYPE | VALUE, item_scope.clone())),
                            }
                        }
                        let mut pseudo = scope.clone();
                        pseudo.push(name);
                        enum_scopes.push((pseudo, variants));
                    }
                }
                "union_item" => exports
                    .providers
                    .extend(declared("union", TYPE, item_scope)),
                "trait_item" => exports
                    .providers
                    .extend(declared("trait", TYPE, item_scope)),
                "type_item" => exports.providers.extend(declared("type", TYPE, item_scope)),
                "mod_item" => {
                    exports
                        .providers
                        .extend(declared("module", TYPE, item_scope));
                    if let Some(name) = named("name") {
                        // cfg で分けた同じ名前の module を区別し (条件の字面の指紋)、`#[macro_use]` (中の
                        // マクロを後の textual scope へ出す) は、それだけで並びを持つ理由にする。
                        let (_, availability_raw) = item_availability(item, source);
                        let macro_use = attribute_named(item, source, "macro_use").or_else(|| {
                            item.child_by_field_name("body")
                                .and_then(|body| inner_macro_use(body, source))
                                .map(Some)
                        });
                        let detail =
                            text_digest(&format!("{} | {macro_use:?}", availability_raw.join(" ")));
                        order.push((
                            if macro_use.is_some() {
                                "use-mod"
                            } else {
                                "mod"
                            },
                            name,
                            detail,
                        ));
                    }
                    if let (Some(body), Some(name)) =
                        (item.child_by_field_name("body"), named("name"))
                    {
                        let mut child = scope.clone();
                        child.push(name);
                        pending.push((body, child));
                    }
                }
                "macro_definition" => {
                    // `#[macro_export]` の `macro_rules!` は crate root の公開の名前を作る。ほかは今の
                    // スコープだけ (`pub(crate) use m;` で取り込める)。`cfg_attr` で条件付きに export
                    // するものは、crate root の名前の有る条件を決められない (読めない属性とする)。
                    match attribute_named(item, source, "macro_export") {
                        Some(None) => exports
                            .exported_macros
                            .extend(declared("macro", MACRO, None)),
                        Some(Some(attribute)) => {
                            exports.providers.extend(declared(
                                "macro",
                                MACRO,
                                Some(current.clone()),
                            ));
                            if let Some(mut exported) = declared("macro", MACRO, None) {
                                exported.availability = None;
                                exported.availability_raw.push(attribute);
                                exports.exported_macros.push(exported);
                            }
                        }
                        None => exports.providers.extend(declared(
                            "macro",
                            MACRO,
                            Some(current.clone()),
                        )),
                    }
                    if let Some(name) = named("name") {
                        // 作り得る名前と呼び出すマクロは、規則の右辺 (展開する字句) から集める (見出しの
                        // マクロ自身の名前や、左辺の断片の指定は数えない)。
                        let mut expansion: Vec<String> = Vec::new();
                        let mut rule_cursor = item.walk();
                        for rule in item.named_children(&mut rule_cursor) {
                            if rule.kind() == "macro_rule"
                                && let Some(right) = rule.child_by_field_name("right")
                            {
                                expansion.extend(tokens_of(right, source));
                            }
                        }
                        let (_, availability_raw) = item_availability(item, source);
                        let text = format!(
                            "{} | {}",
                            tokens_of(item, source).join(" "),
                            availability_raw.join(" ")
                        );
                        order.push(("def", name.clone(), text_digest(&text)));
                        exports.macro_bodies.push(MacroBody {
                            name,
                            mentions: names_in(&expansion),
                            calls: calls_in(&expansion),
                            paths: call_paths_in(&expansion),
                            dynamic: passes_arguments(&expansion),
                            forwards: forwarded_calls(&expansion),
                            text,
                        });
                    }
                }
                "extern_crate_declaration" => {
                    let crate_name = named("name");
                    // `#[macro_use]` の `extern crate` は、その crate のマクロを macro_use prelude に入れる (取り込み
                    // の無い名前の呼び出しが見る)。並びに入れて、crate の並びに依らせる (`as _` でも)。
                    // 取り込むマクロのリスト (`#[macro_use(m)]`) も字面に入れる。
                    let macro_use = attribute_texts(item, source, "macro_use");
                    if !macro_use.is_empty() {
                        let (_, availability_raw) = item_availability(item, source);
                        order.push((
                            "use-crate",
                            crate_name.clone().unwrap_or_default(),
                            text_digest(&format!(
                                "{} | {} | {}",
                                availability_raw.join(" "),
                                macro_use.join(" "),
                                tokens_of(item, source).join(" ")
                            )),
                        ));
                    }
                    let bound = named("alias").or_else(|| crate_name.clone());
                    if let (Some(crate_name), Some(bound)) = (crate_name, bound)
                        && bound != "_"
                    {
                        let (availability, availability_raw) = item_availability(item, source);
                        exports.providers.push(Provider {
                            name: bound,
                            namespaces: TYPE,
                            scope: item_scope,
                            availability,
                            availability_raw,
                            kind: ProviderKind::ExternCrate(crate_name),
                            text: tokens_of(item, source).join(" "),
                        });
                    }
                }
                "use_declaration" => {
                    let (availability, availability_raw) = item_availability(item, source);
                    let text = tokens_of(item, source).join(" ");
                    let Some(argument) = item.child_by_field_name("argument") else {
                        continue;
                    };
                    let mut edges = Vec::new();
                    let mut self_edges = Vec::new();
                    if expand_rust_use_tree_edges_marking_self(
                        argument,
                        source,
                        &[],
                        &current,
                        &current,
                        None,
                        &mut edges,
                        &mut self_edges,
                    )
                    .is_none()
                    {
                        let tokens = tokens_of(item, source);
                        exports.opaque.push(Opaque {
                            availability,
                            availability_raw,
                            text,
                            mentions: names_in(&tokens),
                            macro_name: None,
                            calls: Vec::new(),
                            paths: Vec::new(),
                        });
                        continue;
                    }
                    let unraw_all = |module: Vec<String>| -> Vec<String> {
                        module.iter().map(|s| unraw(s).into_owned()).collect()
                    };
                    // `crate`・`self`・`super` だけの経路の別名は、先の module の取り込みにする (crate
                    // root は先の名前の無い取り込み)。
                    for (alias, target) in keyword_aliases(argument, source, &current) {
                        let (module, item) = match target.split_last() {
                            Some((last, parent)) => (parent.to_vec(), last.clone()),
                            None => (Vec::new(), String::new()),
                        };
                        exports.providers.push(Provider {
                            name: alias,
                            namespaces: TYPE,
                            scope: item_scope.clone(),
                            availability: availability.clone(),
                            availability_raw: availability_raw.clone(),
                            kind: ProviderKind::Use {
                                module,
                                item,
                                origin: UsePathOrigin::Anchored,
                            },
                            text: text.clone(),
                        });
                    }
                    for (position, edge) in edges.into_iter().enumerate() {
                        match edge {
                            RustPubUseEdge::Named {
                                exported_name,
                                target_module,
                                target_item,
                                origin,
                                ..
                            } => {
                                let name = unraw(&exported_name).into_owned();
                                if name == "_" {
                                    continue;
                                }
                                let module = unraw_all(target_module);
                                let item = unraw(&target_item).into_owned();
                                let namespaces = if self_edges.contains(&position) {
                                    TYPE
                                } else {
                                    ALL
                                };
                                let provider =
                                    |namespaces: u8, module: Vec<String>, origin: UsePathOrigin| {
                                        Provider {
                                            name: name.clone(),
                                            namespaces,
                                            scope: item_scope.clone(),
                                            availability: availability.clone(),
                                            availability_raw: availability_raw.clone(),
                                            kind: ProviderKind::Use {
                                                module,
                                                item: item.clone(),
                                                origin,
                                            },
                                            text: text.clone(),
                                        }
                                    };
                                // 同じスコープで export したマクロを 1 語で取り込む `use m as x;` は、マクロの
                                // 名前空間では crate root の `m` を取り込む (型・値の名前空間は書いたとおり)。
                                if origin == UsePathOrigin::Bare
                                    && module.is_empty()
                                    && namespaces & MACRO != 0
                                    && exported_here.contains(&item)
                                {
                                    exports.providers.push(provider(
                                        namespaces & !MACRO,
                                        module,
                                        origin,
                                    ));
                                    exports.providers.push(provider(
                                        MACRO,
                                        Vec::new(),
                                        UsePathOrigin::Anchored,
                                    ));
                                } else {
                                    exports.providers.push(provider(namespaces, module, origin));
                                }
                            }
                            RustPubUseEdge::Wildcard {
                                target_module,
                                origin,
                                ..
                            } => exports.globs.push(Glob {
                                scope: item_scope.clone(),
                                availability: availability.clone(),
                                availability_raw: availability_raw.clone(),
                                module: unraw_all(target_module),
                                origin,
                                text: text.clone(),
                            }),
                        }
                    }
                }
                "foreign_mod_item" => {
                    // `extern "C" { pub fn f(); pub static S: u8; }` の中の項目も module の名前を作る。
                    if let Some(body) = item.child_by_field_name("body") {
                        let mut inner_cursor = body.walk();
                        for inner in body.named_children(&mut inner_cursor) {
                            let namespaces = match inner.kind() {
                                "function_signature_item" | "static_item" => VALUE,
                                "type_item" => TYPE,
                                _ => continue,
                            };
                            let kind = match inner.kind() {
                                "function_signature_item" => "function",
                                "static_item" => "static",
                                _ => "type",
                            };
                            let Some(name) = inner
                                .child_by_field_name("name")
                                .and_then(|name| name.utf8_text(source).ok())
                                .map(|name| unraw(name).into_owned())
                            else {
                                continue;
                            };
                            let (availability, availability_raw) = item_availability(inner, source);
                            exports.providers.push(Provider {
                                name,
                                namespaces,
                                scope: scope_of(visibility(inner, source, &current)),
                                availability,
                                availability_raw,
                                kind: ProviderKind::Declared(kind),
                                text: String::new(),
                            });
                        }
                    }
                }
                "macro_invocation" | "expression_statement" => {
                    let (availability, availability_raw) = item_availability(item, source);
                    let tokens = tokens_of(item, source);
                    // 呼び出すマクロの名前 (`a::b!` なら `b`)。
                    let invocation = if item.kind() == "macro_invocation" {
                        Some(item)
                    } else {
                        let mut inner_cursor = item.walk();
                        item.named_children(&mut inner_cursor)
                            .find(|child| child.kind() == "macro_invocation")
                    };
                    // 経路の名前は構文の字句から取る (コメントを挟んでも名前を変えない)。
                    let macro_path: Vec<String> = invocation
                        .and_then(|invocation| invocation.child_by_field_name("macro"))
                        .map(|path| {
                            tokens_of(path, source)
                                .iter()
                                .filter_map(|token| {
                                    token
                                        .split_once(':')
                                        .filter(|(kind, _)| !kind.is_empty())
                                        .map(|(_, text)| unraw(text).into_owned())
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let macro_name = macro_path.last().cloned();
                    // 呼び出すマクロの名前は作る名前に数えない (引数の中の名前だけ)。
                    let arguments: Vec<String> = invocation
                        .and_then(|invocation| {
                            let mut inner_cursor = invocation.walk();
                            invocation
                                .named_children(&mut inner_cursor)
                                .find(|child| child.kind() == "token_tree")
                        })
                        .map(|tree| tokens_of(tree, source))
                        .unwrap_or_else(|| tokens.clone());
                    if let Some(name) = &macro_name {
                        order.push((
                            "call",
                            name.clone(),
                            text_digest(&format!(
                                "{} | {}",
                                tokens.join(" "),
                                availability_raw.join(" ")
                            )),
                        ));
                    }
                    // 呼び出しの経路の名前 (呼ぶ名前を除く) と、引数の中の呼び出しの経路の名前。
                    let mut paths = call_paths_in(&arguments);
                    paths.extend(
                        macro_path
                            .iter()
                            .take(macro_path.len().saturating_sub(1))
                            .cloned(),
                    );
                    paths.sort();
                    paths.dedup();
                    exports.opaque.push(Opaque {
                        availability,
                        availability_raw,
                        text: tokens.join(" "),
                        mentions: names_in(&arguments),
                        macro_name,
                        calls: calls_in(&arguments),
                        paths,
                    });
                }
                _ => {}
            }
        }
        // cfg で分けた同じ経路の inline module は、別々の並びをつなぐ (上書きしない)。
        let found = macro_order(&order);
        if !found.is_empty() {
            if !exports.macro_order.is_empty() {
                exports.macro_order.push("--".to_owned());
            }
            exports.macro_order.extend(found);
        }
    }
    for (pseudo, variants) in enum_scopes {
        result.entry(pseudo).or_default().providers.extend(variants);
    }
    result
}

/// 名前を作るものの、旧新の比較に使う字面 (名前、種類と先、名前空間、見える範囲、有る条件)。
pub(crate) fn provider_entry(provider: &Provider) -> String {
    format!(
        "{} {:?} {} {:?} | {}",
        provider.name,
        provider.kind,
        provider.namespaces,
        provider.scope,
        provider.availability_raw.join(" ")
    )
}

/// glob の、旧新の比較に使う字面。
pub(crate) fn glob_entry(glob: &Glob) -> String {
    format!(
        "* {:?} {:?} {:?} | {}",
        glob.module,
        glob.origin,
        glob.scope,
        glob.availability_raw.join(" ")
    )
}

/// `macro_rules!` の本体の、旧新の比較に使う字面。
pub(crate) fn macro_entry(body: &MacroBody) -> String {
    format!("macro {} {}", body.name, body.text)
}

/// スコープのマクロの定義・呼び出しと `mod` の宣言の並びの、旧新の比較に使う字面 (同じ名前が 1 度ずつなら
/// 無い)。
pub(crate) fn macro_order_entry(exports: &ScopeExports) -> Option<String> {
    (!exports.macro_order.is_empty()).then(|| format!("order {:?}", exports.macro_order))
}

/// 名前を列挙できない項目の、旧新の比較に使う字面。
pub(crate) fn opaque_entry(opaque: &Opaque) -> String {
    format!("? {} | {}", opaque.text, opaque.availability_raw.join(" "))
}

/// ファイルの旧新の索引の、再公開した名前に効く変化 (Issue #306・#318)。
pub(crate) enum ReexportChange {
    /// 名前を作る入力が変わらない。
    None,
    /// 再公開した名前が変わり得る。
    Relevant,
    /// 宣言と非公開の取り込みだけが変わった: 非公開の取り込み (module, 名前) は、それを経路に使う取り込みか、
    /// 字面に持つ module の位置のマクロがあれば変わり得る。宣言の名前は、extern prelude の crate の名前なら
    /// 経路の先頭の crate を隠し得る (`pub use std::fmt as api;` の `std` を宣言する)。
    Mentioned {
        imports: Vec<(Vec<String>, String)>,
        declarations: Vec<String>,
    },
}

/// ファイルの旧新の索引 (`old`・`new`。無いファイルは None) の、再公開した名前に効く変化。見える取り込み・
/// glob・名前を列挙できない項目・`macro_rules!`・`#[macro_export]` の変化、宣言の見える範囲の変化、glob か名前を
/// 列挙できない項目のあるスコープの名前の変化 (glob の持ち込む名前を隠し得る) は効く。非公開の取り込みの
/// 変化は名前を使うものがあるとき、ほかの宣言の変化は extern prelude の crate を隠し得るときだけ効く (宣言
/// そのものの変化は宣言の報告に任せる)。項目の本体やシグネチャだけの変更は効かない。
pub(crate) fn reexport_change(
    old: Option<&DependencyIndex>,
    new: Option<&DependencyIndex>,
) -> ReexportChange {
    let module_of = |index: &DependencyIndex| -> Vec<String> {
        index
            .file_module()
            .map(<[String]>::to_vec)
            .unwrap_or_default()
    };
    let old_scopes = old
        .map(|index| exports(index, &module_of(index)))
        .unwrap_or_default();
    let new_scopes = new
        .map(|index| exports(index, &module_of(index)))
        .unwrap_or_default();
    let module = new.or(old).map(module_of).unwrap_or_default();
    let empty = ScopeExports::default();
    let mut imports: Vec<(Vec<String>, String)> = Vec::new();
    let mut declarations: Vec<String> = Vec::new();
    let scopes: std::collections::BTreeSet<&Vec<String>> =
        old_scopes.keys().chain(new_scopes.keys()).collect();
    for scope in scopes {
        let before = old_scopes.get(scope).unwrap_or(&empty);
        let after = new_scopes.get(scope).unwrap_or(&empty);
        let entries = |exports: &ScopeExports| -> std::collections::BTreeSet<String> {
            exports
                .globs
                .iter()
                .map(glob_entry)
                .chain(exports.opaque.iter().map(opaque_entry))
                .chain(exports.macro_bodies.iter().map(macro_entry))
                .chain(
                    exports
                        .exported_macros
                        .iter()
                        .map(|provider| format!("export {}", provider_entry(provider))),
                )
                .chain(macro_order_entry(exports))
                .collect()
        };
        if entries(before) != entries(after) {
            return ReexportChange::Relevant;
        }
        let shadowing = [before, after]
            .iter()
            .any(|exports| !exports.globs.is_empty() || !exports.opaque.is_empty());
        let mut current = module.clone();
        current.extend(scope.iter().cloned());
        let names = |exports: &ScopeExports| -> std::collections::BTreeSet<String> {
            exports.providers.iter().map(provider_entry).collect()
        };
        let (before_names, after_names) = (names(before), names(after));
        for (provider, other, unchanged) in before
            .providers
            .iter()
            .map(|provider| (provider, after, &after_names))
            .chain(
                after
                    .providers
                    .iter()
                    .map(|provider| (provider, before, &before_names)),
            )
        {
            if unchanged.contains(&provider_entry(provider)) {
                continue;
            }
            match &provider.kind {
                ProviderKind::Declared(kind) => {
                    // 見える範囲だけが変わった宣言は、glob で再公開した名前を出し入れする。
                    let visibility_changed = other.providers.iter().any(|candidate| {
                        candidate.name == provider.name
                            && matches!(candidate.kind, ProviderKind::Declared(other) if other == *kind)
                            && candidate.scope != provider.scope
                    });
                    if visibility_changed || shadowing {
                        return ReexportChange::Relevant;
                    }
                    // 経路の先頭の crate を隠し得る (`pub use std::fmt as api;` の `std` を宣言する)。
                    declarations.push(provider.name.clone());
                }
                ProviderKind::Use { .. } | ProviderKind::ExternCrate(_) => {
                    if shadowing || provider.scope.as_deref() != Some(current.as_slice()) {
                        return ReexportChange::Relevant;
                    }
                    imports.push((current.clone(), provider.name.clone()));
                }
            }
        }
    }
    if imports.is_empty() && declarations.is_empty() {
        ReexportChange::None
    } else {
        ReexportChange::Mentioned {
            imports,
            declarations,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{attribute_applies, attribute_text, cfg_attr_applies_visiting};
    use crate::engine::tree_walk::{Children, Walk, walk_preorder};
    use crate::language::LangId;
    use std::ops::ControlFlow;

    /// `source` を Rust として読み、最初の属性の節 (`attribute_item`) を `check` に渡す。
    fn with_first_attribute(source: &[u8], check: impl FnOnce(tree_sitter::Node<'_>)) {
        let tree = crate::engine::parser::parse_source(source, LangId::Rust).expect("parse");
        let found = walk_preorder(tree.root_node(), Children::Named, |node, _| {
            if node.kind() == "attribute_item" {
                ControlFlow::Break(node)
            } else {
                ControlFlow::Continue(Walk::Descend)
            }
        });
        let ControlFlow::Break(attribute) = found else {
            panic!("attribute_item が無い");
        };
        check(attribute);
    }

    /// 浅い入れ子では、`cfg_attr` の判定と属性の字面は従来どおり。字面はコメントを除いて字句を詰め、
    /// 語の文字どうしの境にだけ空白を挟む。
    #[test]
    fn attribute_helpers_keep_results_for_shallow_attributes() {
        let nested = b"#[cfg_attr(feature = \"x\", /* c */ cfg_attr(unix, macro_use))]\nmod m;\n";
        with_first_attribute(nested, |attribute| {
            assert_eq!(
                attribute_applies(attribute, nested, "macro_use"),
                Some(true)
            );
            assert_eq!(attribute_applies(attribute, nested, "macro_export"), None);
            assert_eq!(
                attribute_text(attribute, nested),
                "#[cfg_attr(feature=\"x\",cfg_attr(unix,macro_use))]"
            );
        });
        let direct = b"#[macro_use]\nmod m;\n";
        with_first_attribute(direct, |attribute| {
            assert_eq!(
                attribute_applies(attribute, direct, "macro_use"),
                Some(false)
            );
            assert_eq!(attribute_text(attribute, direct), "#[macro_use]");
        });
        let words = b"#[doc(en r\"x\")]\nfn f() {}\n";
        with_first_attribute(words, |attribute| {
            assert_eq!(attribute_text(attribute, words), "#[doc(en r\"x\")]");
        });
        // 兄弟の入れ子の `cfg_attr` は書いた順に深さ優先で調べ、付けると分かった時点で終える
        // (後ろの兄弟は調べない)。どの兄弟の中にある属性も見つける。
        let siblings = b"#[cfg_attr(all(),\n    cfg_attr(all(), macro_use),\n    cfg_attr(all(), cfg_attr(all(), allow(dead_code)), cfg_attr(all(), allow(dead_code)))\n)]\nmod m {}\n";
        with_first_attribute(siblings, |attribute| {
            assert_eq!(
                attribute_applies(attribute, siblings, "macro_use"),
                Some(true)
            );
            assert_eq!(attribute_applies(attribute, siblings, "allow"), Some(true));
            assert_eq!(attribute_applies(attribute, siblings, "macro_export"), None);
        });
    }

    /// 入れ子の `cfg_attr` は書いた順の深さ優先で調べ、付けると分かった時点で終える。`macro_use` は
    /// 外側と最初の入れ子だけを調べ、後ろの兄弟とその入れ子へは進まない (後ろの兄弟から先に辿ると、
    /// 兄弟の入れ子の数に比例して走査が増える)。後ろの入れ子の中の名前は、書いた順に辿って見つける。
    #[test]
    fn cfg_attr_search_is_depth_first_in_written_order_and_stops_early() {
        let source = b"#[cfg_attr(all(),\n    cfg_attr(all(), macro_use),\n    cfg_attr(all(), cfg_attr(all(), allow(dead_code)), cfg_attr(all(), allow(dead_code)))\n)]\nmod m {}\n";
        with_first_attribute(source, |attribute| {
            let mut cursor = attribute.walk();
            let arguments = attribute
                .named_children(&mut cursor)
                .find(|child| child.kind() == "attribute")
                .and_then(|attribute| attribute.child_by_field_name("arguments"))
                .expect("cfg_attr の引数");
            let position = |node: tree_sitter::Node<'_>| {
                let point = node.start_position();
                (point.row, point.column)
            };
            let mut visited = Vec::new();
            assert!(cfg_attr_applies_visiting(
                arguments,
                source,
                "macro_use",
                |node| visited.push(position(node))
            ));
            assert_eq!(visited, vec![(0, 10), (1, 12)]);
            let mut visited = Vec::new();
            assert!(cfg_attr_applies_visiting(
                arguments,
                source,
                "allow",
                |node| visited.push(position(node))
            ));
            assert_eq!(visited, vec![(0, 10), (1, 12), (2, 12), (2, 28)]);
        });
    }

    /// 深く入れ子にした `cfg_attr` (字句の木) でも、`cfg_attr` の判定と属性の字面がコールスタックを
    /// 溢れさせない (小さいスタックの子プロセスで確かめる)。
    #[test]
    fn attribute_helpers_handle_deeply_nested_attributes() {
        crate::engine::tree_walk::run_deep_test_isolated(
            module_path!(),
            "attribute_helpers_handle_deeply_nested_attributes",
            || {
                const DEPTH: usize = 5_000;
                let source = format!(
                    "#[{}macro_use{}]\nmod m;\n",
                    "cfg_attr(a, ".repeat(DEPTH),
                    ")".repeat(DEPTH)
                );
                with_first_attribute(source.as_bytes(), |attribute| {
                    assert_eq!(
                        attribute_applies(attribute, source.as_bytes(), "macro_use"),
                        Some(true)
                    );
                    assert_eq!(
                        attribute_text(attribute, source.as_bytes()),
                        format!(
                            "#[{}macro_use{}]",
                            "cfg_attr(a,".repeat(DEPTH),
                            ")".repeat(DEPTH)
                        )
                    );
                });
            },
        );
    }
}
