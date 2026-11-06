//! 初期値だけを変えた非公開の Rust の const / static の参照のうち、実行時に値を読むだけのもの
//! (Issue #257)。
//!
//! 初期値だけの変更 (`declaration::rust_value_only_constants`) は #145 の公開定数と同じく値変更の
//! ポリシー分類で、参照側の互換の証明ではない。値をコンパイル時に使う参照は壊れうる。パターンは
//! 網羅性と到達性の検査に値を使い (`true` → `false` で `match` の網羅が崩れる)、配列の長さや型引数は
//! 値で型が決まる (`[u8; NAME.len()]`)。`const` / `static` の初期化式・列挙子の判別子・`const`
//! ブロック・`const fn` の本体も値をコンパイル時に評価する。const を場所として使う参照 (借用・
//! メソッド呼び出しの自動借用・添字・集約の要素・`match` の対象など) は、値が `'static` へ昇格
//! できるかで参照の寿命が変わる。これらの位置の参照は blocking に残し、`const` の付かない関数の
//! 本体で値をそのまま使う参照 (static は場所としての利用も) だけを情報提供に回す。
//!
//! 構文で分からない依存は扱わない: 既定で deny の lint (`NAME + 1` の `arithmetic_overflow`、
//! `1 / NAME` の `unconditional_panic`。ローカル変数を経由した定数の伝播も含む)、別のファイルの
//! `macro_rules!` や `#[macro_use]` による標準マクロの差し替え。

use std::collections::HashSet;
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};

/// 祖先をたどる上限 (1 件の参照の判定の合計)。`Node::parent` は深さに比例するので、深い入れ子では
/// 証明しない。
const MAX_ANCESTORS: usize = 256;

/// マクロの引数として読む token tree の子の数の上限。
const MAX_MACRO_TOKENS: u32 = 4096;

/// 参照の位置での値の使われ方。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum RustValueUse {
    /// 実行時に値をそのまま使うと証明できない (パターン・配列長・定数の文脈・未知のマクロなど)。
    #[default]
    Unproven,
    /// `const` の付かない関数の本体で値をそのまま使う (コピー・ムーブ・フィールドの読み出し・比較・
    /// 演算・引数・戻り値など)。const の値の昇格に依らない。
    Read,
    /// `const` の付かない関数の本体で場所として使う (借用・メソッド呼び出しの自動借用・添字・
    /// 集約の要素・`match` の対象など)。static は値によらず `'static` の場所だが、const は値が
    /// 昇格できるかで参照の寿命が変わる (`None` → `Some(AtomicU8::new(0))` で E0515)。
    Place,
}

/// 参照の名前 `node` の値の使われ方を、参照ファイルの構文から判定する。
/// `local_macros` は参照ファイルが標準マクロの名前を差し替えうるか (`LocalMacros::collect`)。
pub(super) fn classify(node: Node<'_>, source: &[u8], local_macros: &LocalMacros) -> RustValueUse {
    let mut budget = MAX_ANCESTORS;
    let Some(parent) = node.parent() else {
        return RustValueUse::Unproven;
    };
    let (expression, value_use) = if parent.kind() == "token_tree" {
        // マクロの引数は構文木にならないので、引数の字面を `&? パス (.名前)*` に限る。
        // 入れ子の token tree (`vec![[0u8; N]]` の配列長、`matches!` のパターン) は通さない。
        let Some(invocation) = parent
            .parent()
            .filter(|invocation| invocation.kind() == "macro_invocation")
        else {
            return RustValueUse::Unproven;
        };
        let Some(macro_name) = invocation
            .child_by_field_name("macro")
            .filter(|name| name.kind() == "identifier")
            .and_then(|name| name.utf8_text(source).ok())
            .filter(|name| local_macros.is_standard_value_macro(name))
        else {
            return RustValueUse::Unproven;
        };
        let Some(borrow) = simple_macro_argument(parent, node) else {
            return RustValueUse::Unproven;
        };
        // `format_args!` は引数の参照を値として返す (`let a = format_args!("{}", NAME);`)。
        let value_use = if borrow || macro_name.trim_start_matches("r#") == "format_args" {
            RustValueUse::Place
        } else {
            RustValueUse::Read
        };
        (invocation, value_use)
    } else {
        let Some(path) = path_expression(node, &mut budget) else {
            return RustValueUse::Unproven;
        };
        let Some((expression, value_use)) = value_position(path, source, &mut budget) else {
            return RustValueUse::Unproven;
        };
        (expression, value_use)
    };
    if !in_runtime_function_body(expression, budget) {
        return RustValueUse::Unproven;
    }
    value_use
}

/// 参照の名前を含む式としてのパス。`a::b::NAME` なら一番外の `scoped_identifier`。名前がパスの
/// 途中 (`NAME::x`) なら定数の値ではないので `None`。
fn path_expression<'a>(node: Node<'a>, budget: &mut usize) -> Option<Node<'a>> {
    if node.kind() != "identifier" {
        return None;
    }
    let mut expression = node;
    while let Some(parent) = expression
        .parent()
        .filter(|parent| parent.kind() == "scoped_identifier")
    {
        *budget = budget.checked_sub(1)?;
        if !is_field(parent, "name", expression) {
            return None;
        }
        expression = parent;
    }
    Some(expression)
}

/// パスの値の使われ方と、それを含む式。
///
/// 値の全体や一部をそのまま外へ渡す式は外側へたどる: フィールドの読み出し (`NAME.text`)・括弧・
/// タプル・配列の要素・構造体の初期化 (`W { x: NAME }`・`W { ..NAME }`)・範囲・タプル構造体や
/// 列挙子のコンストラクタの引数 (`Some(NAME)`。名前が大文字で始まる呼び出し)・入れ子のブロックの
/// 末尾。const はたどった式ごと借用されると、値が `'static` へ昇格できるかで参照の寿命が変わる
/// (`&Some(NAME)`)。たどった先で値を受け取って消費する位置 (関数やメソッドの呼び出しの引数・
/// 戻り値・関数やクロージャの本体の末尾・`ref` の無い変数の初期化・代入の右辺・二項演算・`-` /
/// `!`・条件・キャスト・`?`・`.await`・関数ポインタの呼び出し・式文) に着けば `Read`。借用・
/// メソッド呼び出しの受け手や `*` (自動借用)・添字・`match` の対象・`ref` の束縛など、それ以外は
/// `Place`。
fn value_position<'a>(
    path: Node<'a>,
    source: &[u8],
    budget: &mut usize,
) -> Option<(Node<'a>, RustValueUse)> {
    let mut expression = path;
    loop {
        *budget = budget.checked_sub(1)?;
        let parent = expression.parent()?;
        let by_value = match parent.kind() {
            "field_expression" if is_field(parent, "value", expression) => {
                if parent.parent().is_some_and(|call| {
                    call.kind() == "call_expression" && is_field(call, "function", parent)
                }) {
                    return Some((parent, RustValueUse::Place));
                }
                expression = parent;
                continue;
            }
            "parenthesized_expression" | "tuple_expression" | "range_expression" => {
                expression = parent;
                continue;
            }
            // 配列の要素。繰り返しの長さ (`[0; NAME]`) は `in_runtime_function_body` が拒む。
            "array_expression" if !is_field(parent, "length", expression) => {
                expression = parent;
                continue;
            }
            "field_initializer" | "base_field_initializer" => {
                if parent.kind() == "field_initializer" && !is_field(parent, "value", expression) {
                    return Some((parent, RustValueUse::Place));
                }
                // field_initializer_list の親の struct_expression。
                expression = parent.parent()?.parent()?;
                continue;
            }
            "arguments" => {
                let call = parent.parent()?;
                if call.kind() == "call_expression"
                    && call
                        .child_by_field_name("function")
                        .is_some_and(|callee| may_construct(callee, source, budget))
                {
                    expression = call;
                    continue;
                }
                true
            }
            "block" => {
                let owner = parent.parent()?;
                if matches!(owner.kind(), "function_item" | "closure_expression")
                    && is_field(owner, "body", parent)
                {
                    true
                } else {
                    expression = parent;
                    continue;
                }
            }
            "let_declaration" => {
                is_field(parent, "value", expression)
                    && parent
                        .child_by_field_name("pattern")
                        .is_some_and(|pattern| matches!(pattern.kind(), "identifier" | "_"))
            }
            "unary_expression" => parent
                .child(0)
                .is_some_and(|operator| matches!(operator.kind(), "-" | "!")),
            "assignment_expression" | "compound_assignment_expr" => {
                is_field(parent, "right", expression)
            }
            "if_expression" | "while_expression" => is_field(parent, "condition", expression),
            "type_cast_expression" => is_field(parent, "value", expression),
            // 関数ポインタの定数の呼び出し (`NAME()`)。メソッド呼び出しは上で場所にした。
            "call_expression" => is_field(parent, "function", expression),
            "closure_expression" => is_field(parent, "body", expression),
            "return_expression"
            | "binary_expression"
            | "try_expression"
            | "await_expression"
            | "expression_statement" => true,
            _ => false,
        };
        return Some((
            expression,
            if by_value {
                RustValueUse::Read
            } else {
                RustValueUse::Place
            },
        ));
    }
}

/// 呼び出し先がタプル構造体や列挙子のコンストラクタでありうるか (先頭の `_` を除いて名前が大文字で
/// 始まる。`rust_signature::is_type_like_name`)。コンストラクタの呼び出しは集約と同じく値ごと
/// 昇格しうる。小文字で始まる関数とメソッドの呼び出しは値を消費する。名前を読めない・非 ASCII で
/// 始まるもの、パスでもメソッドでもない呼び出し先はコンストラクタとみなす (保守側)。
fn may_construct(callee: Node<'_>, source: &[u8], budget: &mut usize) -> bool {
    let mut callee = callee;
    loop {
        let Some(rest) = budget.checked_sub(1) else {
            return true;
        };
        *budget = rest;
        let name = match callee.kind() {
            "identifier" | "type_identifier" => callee,
            "scoped_identifier" | "scoped_type_identifier" => {
                let Some(name) = callee.child_by_field_name("name") else {
                    return true;
                };
                name
            }
            "generic_function" => {
                let Some(inner) = callee.child_by_field_name("function") else {
                    return true;
                };
                callee = inner;
                continue;
            }
            // `(Some)(NAME)` の括弧は外して見る。
            "parenthesized_expression" => {
                let Some(inner) = callee.named_child(0) else {
                    return true;
                };
                callee = inner;
                continue;
            }
            // メソッド (`x.push(NAME)`) と、構造体のフィールドの関数ポインタの呼び出し。
            "field_expression" => return false,
            // そのほか (クロージャ・呼び出しの結果など) は保守側に倒す。
            _ => return true,
        };
        // `_Wrap(NAME)` の先頭の `_` は除き、プリミティブ型の名前もコンストラクタ側に倒す。
        return name
            .utf8_text(source)
            .ok()
            .is_none_or(crate::engine::rust_signature::is_type_like_name);
    }
}

/// マクロの引数のうち `name` を含むもの (トップレベルの `,` と `;` で区切る) が `&? パス (.名前)*`
/// で、`name` がパスの最後の名前か。`&` があれば `Some(true)`。参照ごとに引数の並びを読むので、
/// 生成された巨大な表 (`MAX_MACRO_TOKENS` を超える token tree) は証明しない。
fn simple_macro_argument(tokens: Node<'_>, name: Node<'_>) -> Option<bool> {
    if tokens.child_count() > MAX_MACRO_TOKENS {
        return None;
    }
    let mut cursor = tokens.walk();
    let children: Vec<Node<'_>> = tokens
        .children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect();
    // 先頭と末尾は区切りの括弧。
    let inner = children.get(1..children.len().checked_sub(1)?)?;
    let position = inner.iter().position(|child| child.id() == name.id())?;
    let start = inner[..position]
        .iter()
        .rposition(|child| matches!(child.kind(), "," | ";"))
        .map_or(0, |ix| ix + 1);
    let end = inner[position..]
        .iter()
        .position(|child| matches!(child.kind(), "," | ";"))
        .map_or(inner.len(), |ix| position + ix);
    let segment = &inner[start..end];
    let name_ix = position - start;
    let borrow = segment.first().is_some_and(|token| token.kind() == "&");
    let path_start = usize::from(borrow);
    // パス: 名前・`self`・`crate`・`super` を `::` でつなぐ。
    let mut ix = path_start;
    loop {
        if !matches!(
            segment.get(ix)?.kind(),
            "identifier" | "self" | "crate" | "super"
        ) {
            return None;
        }
        if segment
            .get(ix + 1)
            .is_some_and(|token| token.kind() == "::")
        {
            ix += 2;
        } else {
            break;
        }
    }
    if ix != name_ix {
        return None;
    }
    // 続くのはフィールドの読み出し (`.名前`) だけ。
    let mut rest = segment[ix + 1..].chunks(2);
    rest.all(
        |pair| matches!(pair, [dot, field] if dot.kind() == "." && field.kind() == "identifier"),
    )
    .then_some(borrow)
}

/// `expression` が `const` の付かない関数の本体の中にあり、値をコンパイル時に使う位置
/// (パターン・型・定数の文脈・属性) に無いか。祖先は `budget` 段までたどる。
fn in_runtime_function_body(expression: Node<'_>, budget: usize) -> bool {
    let mut child = expression;
    for _ in 0..budget {
        let Some(parent) = child.parent() else {
            return false;
        };
        let kind = parent.kind();
        // パターン。match arm のガード (`match_pattern` の `condition`) は実行時の式。
        if kind == "match_pattern" {
            if !is_field(parent, "condition", child) {
                return false;
            }
        } else if kind.ends_with("_pattern") {
            return false;
        }
        match kind {
            "let_declaration" | "let_condition" | "for_expression" | "parameter"
                if is_field(parent, "pattern", child) =>
            {
                return false;
            }
            "array_expression" if is_field(parent, "length", child) => return false,
            "closure_parameters"
            | "array_type"
            | "type_arguments"
            | "type_parameters"
            | "const_parameter"
            | "const_block"
            | "const_item"
            | "static_item"
            | "enum_variant"
            | "attribute_item"
            | "inner_attribute_item"
            | "macro_definition"
            // マクロの引数の中の式は `classify` が確かめる。それより外側のマクロは無い
            // (引数は構文木にならない) ので、現れれば証明しない。
            | "macro_invocation"
            | "token_tree"
            | "source_file"
            | "declaration_list" => return false,
            "function_item" => return is_field(parent, "body", child) && !is_const_fn(parent),
            _ => {}
        }
        child = parent;
    }
    false
}

fn is_field(parent: Node<'_>, field: &str, child: Node<'_>) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|node| node.id() == child.id())
}

/// `const fn` か (`function_modifiers` に `const` がある)。
fn is_const_fn(function: Node<'_>) -> bool {
    let mut cursor = function.walk();
    function
        .children(&mut cursor)
        .filter(|child| child.kind() == "function_modifiers")
        .any(|modifiers| {
            let mut cursor = modifiers.walk();
            modifiers
                .children(&mut cursor)
                .any(|modifier| modifier.kind() == "const")
        })
}

/// 参照ファイルが標準マクロの名前を差し替えうるか。`macro_rules!` の定義と `use` が束縛する
/// 名前 (`use make as vec;`) を集め、`#[macro_use]` があればすべての名前を差し替えうるとみなす。
#[derive(Debug, Default)]
pub(super) struct LocalMacros {
    names: HashSet<String>,
    macro_use: bool,
}

impl LocalMacros {
    /// `node` を含むファイル全体から集める。
    pub(super) fn collect(node: Node<'_>, source: &[u8]) -> Self {
        let mut root = node;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        let mut local = Self::default();
        let _ = walk_preorder(root, Children::Named, |node, _| -> ControlFlow<(), Walk> {
            match node.kind() {
                "macro_definition" => match node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                {
                    Some(name) => local.insert(name),
                    None => local.macro_use = true,
                },
                "use_declaration" => {
                    let _ =
                        walk_preorder(node, Children::Named, |name, _| -> ControlFlow<(), Walk> {
                            if name.kind() == "identifier"
                                && let Ok(text) = name.utf8_text(source)
                            {
                                local.insert(text);
                            }
                            ControlFlow::Continue(Walk::Descend)
                        });
                    return ControlFlow::Continue(Walk::Skip);
                }
                "attribute_item" | "inner_attribute_item"
                    if node
                        .utf8_text(source)
                        .is_ok_and(|text| text.contains("macro_use")) =>
                {
                    local.macro_use = true;
                }
                _ => {}
            }
            ControlFlow::Continue(Walk::Descend)
        });
        local
    }

    fn insert(&mut self, name: &str) {
        self.names
            .insert(name.strip_prefix("r#").unwrap_or(name).to_owned());
    }

    /// 引数を値の式として展開する標準のマクロ (`rust_scope::is_value_expression_macro`) で、
    /// 参照ファイルが同じ名前を差し替えていないか。`matches!` はパターンを取るので含まない。
    fn is_standard_value_macro(&self, name: &str) -> bool {
        let name = name.strip_prefix("r#").unwrap_or(name);
        !self.macro_use
            && crate::engine::rust_scope::is_value_expression_macro(name)
            && !self.names.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    /// `source` の中の名前 `NAME` の出現ごとの判定 (出現順)。
    fn uses(source: &str) -> Vec<RustValueUse> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let root = tree.root_node();
        assert!(!root.has_error(), "{source}");
        let local = LocalMacros::collect(root, source.as_bytes());
        let mut found = Vec::new();
        let _ = walk_preorder(root, Children::Named, |node, _| {
            if matches!(node.kind(), "identifier" | "type_identifier")
                && node.utf8_text(source.as_bytes()) == Ok("NAME")
            {
                found.push(classify(node, source.as_bytes(), &local));
            }
            ControlFlow::<(), Walk>::Continue(Walk::Descend)
        });
        assert!(!found.is_empty(), "{source}");
        found
    }

    #[test]
    fn plain_reads_in_function_bodies_are_runtime_uses() {
        for source in [
            "fn f() -> u32 { greeting::NAME }",
            "fn f() -> u32 { crate::notes::NAME }",
            "fn f() -> u32 { let value = NAME; value }",
            "fn f() -> &'static str { NAME.text }",
            "fn f(v: u32) -> bool { match v { x if x == NAME => true, _ => false } }",
            "fn f() { if NAME > 0 { g(NAME as usize); } }",
            "fn f() -> Note { Note { text: NAME, ..NAME } }",
            "fn f() -> Option<u32> { Some(NAME) }",
            "fn f() { g(Some((NAME, [NAME]))); }",
            "fn f() -> Note { let note = Note { text: NAME }; note }",
            "fn f() -> i32 { -NAME }",
            "fn f() -> bool { !NAME }",
            "fn f() -> u32 { let value = { g(); NAME }; value }",
            "fn f() -> Vec<u32> { vec![NAME; 2] }",
            "fn f() -> Vec<u8> { vec![0; NAME] }",
            "fn f() { println!(\"{}\", greeting::NAME); }",
            "fn f() { assert_eq!(NAME.name, \"greeting\"); }",
            "fn f() -> impl Fn() -> u32 { || NAME }",
            "impl S { fn f(&self) -> u32 { NAME } }",
            "fn outer() { fn inner() -> u32 { NAME } }",
            "async fn f() -> u32 { NAME }",
        ] {
            let found = uses(source);
            assert!(
                found.iter().all(|use_| *use_ == RustValueUse::Read),
                "{source}: {found:?}"
            );
        }
        // 場所としての利用は、const では値が 'static へ昇格できるかで参照の寿命が変わる
        // (static では値によらない)。
        for source in [
            "fn f() -> Vec<&'static crate::Note> { vec![&greeting::NAME] }",
            "fn f() -> &'static str { &NAME.text }",
            "fn f() -> &'static u32 { &(NAME) }",
            "fn f() -> Option<&'static AtomicU8> { NAME.as_ref() }",
            "fn f() -> usize { NAME.text.len() }",
            "fn f() -> &'static AtomicU8 { &NAME[0] }",
            "fn f() -> &'static (u8,) { &(NAME,) }",
            "fn f() -> &'static [u8; 1] { &[NAME] }",
            "fn f() -> &'static u8 { &{ NAME } }",
            "fn f() -> &'static u8 { match NAME { ref value => value } }",
            "fn f() { let args = format_args!(\"{}\", NAME); }",
            // 集約やコンストラクタの引数ごと借用すると、値ごと昇格しうる。
            "fn f() -> &'static Option<Option<AtomicU8>> { &Some(NAME) }",
            "fn f() -> &'static Option<Option<AtomicU8>> { &Option::Some(NAME) }",
            "fn f() -> &'static impl Sized { &(Some)(NAME) }",
            "fn f() -> &'static _Wrap { &_Wrap(NAME) }",
            "fn f() -> &'static W { &W { x: NAME } }",
            "fn f() -> &'static W { &W { ..NAME } }",
            "fn f() -> &'static std::ops::Range<u8> { &(NAME..9) }",
            // `ref` の束縛と、`*` の Deref (自動借用)。
            "fn f() -> &'static Option<AtomicU8> { let ref x = NAME; x }",
            "fn f() -> &'static Option<AtomicU8> { &*NAME }",
        ] {
            assert_eq!(uses(source), [RustValueUse::Place], "{source}");
        }
    }

    #[test]
    fn compile_time_and_unknown_positions_are_not_runtime_uses() {
        for source in [
            // パターン (網羅性と到達性の検査に値を使う)。
            "fn f(v: bool) { match v { crate::constants::NAME => (), false => () } }",
            "fn f(v: u8) { match v { NAME..=9 => (), _ => () } }",
            "fn f(v: u8) { if let NAME = v {} }",
            "fn f(v: Option<u8>) { if let Some(NAME) = v {} }",
            "fn f(v: u8) { while let NAME = v {} }",
            // 型・コンパイル時の評価。
            "fn f() -> [u8; NAME] { [0; 1] }",
            "fn f() { let a = [0u8; NAME]; }",
            "fn f() { let a: [u8; NAME] = [0; 1]; }",
            "fn f() { g::<NAME>(); }",
            "fn f() { let a = const { NAME }; }",
            "fn f() { const OTHER: u32 = NAME; }",
            "fn f() { static OTHER: u32 = NAME; }",
            "const fn f() -> u32 { NAME }",
            "const OTHER: u32 = NAME + 1;",
            "static OTHER: &u32 = &NAME;",
            "enum E { A = NAME }",
            "struct S<const N: usize = NAME>;",
            "#[doc = NAME] fn f() {}",
            "impl S { const OTHER: u32 = NAME; }",
            "struct S { a: [u8; NAME] }",
            // マクロの中で構文が見えないもの・標準の値のマクロでないもの。
            "fn f() -> Vec<[u8; 1]> { vec![[0u8; crate::constants::NAME]] }",
            "fn f(v: u8) -> bool { matches!(v, crate::constants::NAME..=1) }",
            "fn f() { assert!(NAME > 0); }",
            "fn f() { custom!(NAME); }",
            "fn f() { std::println!(\"{}\", NAME); }",
            "fn f() { vec![NAME::other]; }",
            "fn f() { vec![g(NAME)]; }",
            // 定数の値ではない位置。
            "fn f() { NAME::run(); }",
        ] {
            assert!(
                uses(source)
                    .iter()
                    .all(|use_| *use_ == RustValueUse::Unproven),
                "{source}: {:?}",
                uses(source)
            );
        }
    }

    #[test]
    fn redefined_or_imported_standard_macros_are_not_trusted() {
        for source in [
            "macro_rules! vec { ($n:expr) => { let _: [(); $n] = [(); 1]; }; }\nfn f() { vec!(NAME); }",
            "use make as vec;\nfn f() { vec!(NAME); }",
            "#[macro_use] extern crate other;\nfn f() { vec!(NAME); }",
            "mod tests { macro_rules! println { () => {} } }\nfn f() { println!(\"{}\", NAME); }",
        ] {
            assert!(
                uses(source)
                    .iter()
                    .all(|use_| *use_ == RustValueUse::Unproven),
                "{source}"
            );
        }
        // 対照: 別の名前のマクロの定義と取り込みは標準マクロを差し替えない。
        assert_eq!(
            uses("macro_rules! other { () => {} }\nuse std::fmt::Write;\nfn f() { vec![NAME]; }"),
            [RustValueUse::Read]
        );
    }

    /// 判定に使うノード名・フィールド名が Rust の文法に実在すること。
    #[test]
    fn value_use_node_kinds_exist_in_the_rust_grammar() {
        let language = LangId::Rust.ts_language();
        for kind in [
            "token_tree",
            "macro_invocation",
            "scoped_identifier",
            "identifier",
            "field_expression",
            "parenthesized_expression",
            "reference_expression",
            "match_pattern",
            "range_pattern",
            "tuple_struct_pattern",
            "let_declaration",
            "let_condition",
            "for_expression",
            "parameter",
            "array_expression",
            "closure_parameters",
            "array_type",
            "type_arguments",
            "type_parameters",
            "const_parameter",
            "const_block",
            "const_item",
            "static_item",
            "enum_variant",
            "attribute_item",
            "inner_attribute_item",
            "macro_definition",
            "source_file",
            "declaration_list",
            "function_item",
            "function_modifiers",
            "use_declaration",
            "self",
            "crate",
            "super",
        ] {
            assert_ne!(language.id_for_node_kind(kind, true), 0, "{kind}");
        }
        for kind in ["&", "::", ".", ",", ";", "const"] {
            assert_ne!(language.id_for_node_kind(kind, false), 0, "{kind}");
        }
        for field in [
            "macro",
            "name",
            "value",
            "condition",
            "pattern",
            "length",
            "body",
        ] {
            assert!(language.field_id_for_name(field).is_some(), "{field}");
        }
    }
}
