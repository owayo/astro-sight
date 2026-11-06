//! Rust の const / 非 mut static の宣言のうち、初期値の外を旧新で比べる比較用のトークン。
//!
//! 初期値 (`value`) を除いた宣言 (可視性・`const` / `static`・名前・型) と、外側の属性・囲む
//! `mod`・ファイルと `mod` の本体の内側の属性、型に現れる名前を束縛しうる同じスコープの宣言が
//! 旧新で同じなら、宣言の変更は初期値だけと扱う。初期値の変更で参照側が壊れるか (パターンや
//! 配列長などのコンパイル時の利用) は参照の位置で別に判定する (`engine::impact::rust_const_value`)。
//!
//! 次は証明しない (`None`): `static mut`、型か初期値の無い宣言、型の中のマクロ・解析エラー、
//! 許可リストの外の属性、`mod` 以外の囲む宣言 (impl / trait / extern block / 関数)、item や
//! マクロを含む初期値 (初期化式の中の `impl` は外の型の関連型を変えうる)、item 位置のマクロ
//! 呼び出しがあるスコープ (作る名前を字面で特定できない)、関連型への射影 (`<S as Tr>::A` の意味は
//! クレートのどこの `impl` でも変わる)、囲む `mod` があるときに `super` / `crate` から始まる
//! パス (型と、たどった型別名・`use`。同じファイルのほかのスコープの宣言で決まる)。

use std::collections::{BTreeSet, HashMap};

use super::parameter_patterns::{
    append_header, append_inner_attributes, append_outer_attributes, append_tokens,
};
use super::{Children, ControlFlow, HashSet, Node, Walk, any_node, walk_preorder};

/// 囲む `mod` をたどる上限 (`rust_parameter_pattern_contract` と同じ)。`Node::parent` は深さに
/// 比例するので、深い入れ子では証明しない。
const MAX_ENCLOSING_DEPTH: usize = 64;

/// 宣言や `use` が束縛し直せない、型に現れる名前 (パスの起点)。
const UNBINDABLE_NAMES: &[&str] = &["crate", "self", "super", "Self"];

/// 初期値を除いた宣言の比較用のトークン列と、型の名前を束縛しうる名前。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RustConstValueContract {
    tokens: Vec<String>,
    /// 型に現れる名前と、同じスコープの型別名・定数・`use` からたどった名前 (`use a::Note as
    /// Alias` なら `Note` も)。差分のほかのファイルで同じ名前の宣言が変われば、型の意味が
    /// 変わりうる (呼び出し側が証明を取り消す)。
    pub(crate) type_names: BTreeSet<String>,
}

/// 型の名前の束縛をたどるときに集める事実。
#[derive(Default)]
struct BindingFacts {
    names: BTreeSet<String>,
    /// `crate` / `super` から始まるパスがある。
    rooted_outside: bool,
    /// 関連型への射影 (`qualified_type`・`Self`) がある。
    projected: bool,
}

/// `item` (`const_item` / `static_item`) から初期値だけを除いた比較用のトークン列を作る。
/// 旧新で一致すれば初期値だけの変更と扱う。証明できない形なら `None`。
pub(crate) fn rust_const_value_contract(
    item: Node<'_>,
    source: &[u8],
) -> Option<RustConstValueContract> {
    if !matches!(item.kind(), "const_item" | "static_item") || item.has_error() {
        return None;
    }
    let value = item.child_by_field_name("value")?;
    let ty = item.child_by_field_name("type")?;
    if any_node(value, Children::All, |node| {
        node.kind().ends_with("_item")
            || matches!(
                node.kind(),
                "macro_invocation"
                    | "macro_definition"
                    | "use_declaration"
                    | "extern_crate_declaration"
            )
    }) {
        return None;
    }
    let mut tokens = Vec::new();
    let mut cursor = item.walk();
    for child in item.children(&mut cursor) {
        if child.kind() == "mutable_specifier" {
            return None;
        }
        if child.id() == value.id() {
            tokens.push("<value>".to_owned());
        } else if !child.is_extra() {
            // 型の中のマクロ・解析エラーは `append_header` が拒む。
            append_header(child, source, &HashSet::new(), None, &mut tokens)?;
        }
    }
    let nested = append_enclosing_modules(item, source, &mut tokens)?;
    let mut facts = BindingFacts::default();
    collect_bindable_names(ty, source, &mut facts)?;
    tokens.push("<bindings>".to_owned());
    append_scope_bindings(item.parent()?, item, source, &mut facts, &mut tokens)?;
    if facts.projected || (nested && facts.rooted_outside) {
        return None;
    }
    Some(RustConstValueContract {
        tokens,
        type_names: facts.names,
    })
}

/// 外側の属性と、囲む `mod` のヘッダ・属性、ファイルと `mod` の本体の内側の属性を足す。
/// 囲む `mod` があれば true。`mod` 以外に囲まれていれば `None`。
fn append_enclosing_modules(
    item: Node<'_>,
    source: &[u8],
    tokens: &mut Vec<String>,
) -> Option<bool> {
    let mut nested = false;
    let mut current = item;
    for _ in 0..MAX_ENCLOSING_DEPTH {
        append_outer_attributes(current, source, tokens)?;
        if let Some(body) = current.child_by_field_name("body") {
            append_inner_attributes(body, source, tokens)?;
        }
        let parent = current.parent()?;
        match parent.kind() {
            "source_file" => {
                if parent.has_error() {
                    return None;
                }
                append_inner_attributes(parent, source, tokens)?;
                return Some(nested);
            }
            "declaration_list" if parent.parent()?.kind() == "mod_item" => current = parent,
            "mod_item" => {
                nested = true;
                tokens.push("<enclosing-module>".to_owned());
                append_header(parent, source, &HashSet::new(), None, tokens)?;
                current = parent;
            }
            _ => return None,
        }
    }
    None
}

/// `node` の部分木の名前のうち、宣言や `use` が束縛しうるものを `facts` に足す。lifetime の名前は
/// 除く。`crate` / `super` から始まるパスと、関連型・関連定数へのパス (`<S as Tr>::A`・`Self`・
/// 大文字で始まる名前を途中に持つ `Sizes::N`。どこの `impl` でも意味が変わる) も記録する。
/// `use` のパスは `use` 自体のトークンを比べるので、関連のパスとして扱わない
/// (`use Kind::*` の列挙子の取り込み)。
fn collect_bindable_names(node: Node<'_>, source: &[u8], facts: &mut BindingFacts) -> Option<()> {
    let check_associated = !matches!(node.kind(), "use_declaration" | "extern_crate_declaration");
    let result = walk_preorder(node, Children::All, |node, _| {
        match node.kind() {
            "lifetime" => return ControlFlow::Continue(Walk::Skip),
            "crate" | "super" => facts.rooted_outside = true,
            "qualified_type" => facts.projected = true,
            "scoped_identifier" | "scoped_type_identifier"
                if check_associated
                    && node
                        .child_by_field_name("path")
                        .is_some_and(|path| path_ends_with_type_like_name(path, source)) =>
            {
                facts.projected = true;
            }
            "identifier" | "type_identifier" | "primitive_type" => {
                let Ok(text) = node.utf8_text(source) else {
                    return ControlFlow::Break(());
                };
                let name = text.strip_prefix("r#").unwrap_or(text);
                if name == "Self" {
                    facts.projected = true;
                } else if !UNBINDABLE_NAMES.contains(&name) {
                    facts.names.insert(name.to_owned());
                }
            }
            _ => {}
        }
        ControlFlow::Continue(Walk::Descend)
    });
    result.is_continue().then_some(())
}

/// パスの最後の名前が型らしいか。`Sizes::N` の `Sizes`・`_Sizes`、`u8::MAX` の `u8`、
/// `<Sizes>::N` の `<Sizes>`、`Vec::<u8>::new` の `Vec::<u8>`。名前は先頭の `_` を除いて大文字で
/// 始まるか、プリミティブ型の名前のもの (読めない・非 ASCII で始まるものも含める)。
fn path_ends_with_type_like_name(path: Node<'_>, source: &[u8]) -> bool {
    let mut node = path;
    // パスの入れ子は浅いが、念のため上限を置く (超えれば型らしいとみなす)。
    for _ in 0..MAX_ENCLOSING_DEPTH {
        node = match node.kind() {
            "identifier" | "type_identifier" => {
                return node.utf8_text(source).ok().is_none_or(is_type_like_name);
            }
            "bracketed_type" | "primitive_type" => return true,
            "scoped_identifier" | "scoped_type_identifier" => {
                let Some(name) = node.child_by_field_name("name") else {
                    return true;
                };
                name
            }
            "generic_type" => {
                let Some(inner) = node.child_by_field_name("type") else {
                    return true;
                };
                inner
            }
            // `crate` / `self` / `super` などパスの起点。
            _ => return false,
        };
    }
    true
}

/// 型 (やタプル構造体・列挙子) の名前らしいか。先頭の `_` を除いて大文字か非 ASCII で始まる名前、
/// 空の名前、プリミティブ型の名前。命名規約 (型は UpperCamelCase、モジュールと関数は snake_case)
/// に従うコードを前提にする。
pub(crate) fn is_type_like_name(name: &str) -> bool {
    let name = name.strip_prefix("r#").unwrap_or(name);
    if matches!(
        name,
        "u8" | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f16"
            | "f32"
            | "f64"
            | "f128"
            | "bool"
            | "char"
            | "str"
    ) {
        return true;
    }
    name.trim_start_matches('_')
        .chars()
        .next()
        .is_none_or(|first| !first.is_ascii() || first.is_ascii_uppercase())
}

/// 宣言を直接含むスコープ `scope` (ファイルか `mod` の本体) の直下の宣言のうち、型の名前
/// (`facts.names`) の束縛に関わりうるものを足す。
///
/// - グロブの `use` と `extern crate` はすべて (取り込む名前を字面で特定できない)。
/// - ほかの `use` は名前を含むもの、名前付きの宣言は名前が一致するもの。比べる宣言に現れる名前も
///   たどる (`use a::Note as Alias` の `Note`、型別名の先、配列長の定数、構造体のフィールドの型、
///   配列長に使う `const fn` の本体が呼ぶ関数)。たどった宣言の `crate` / `super` から始まるパスと
///   関連型への射影も `facts` に残す。
///
/// 直前の属性も宣言の一部として比べる。item 位置のマクロの呼び出しがあれば証明しない。子の `mod`
/// の本体や関数の本体の中の宣言は、このスコープの名前の束縛を変えないので比べない (例えば
/// `mod tests` の中の `use` の追加)。
fn append_scope_bindings(
    scope: Node<'_>,
    item: Node<'_>,
    source: &[u8],
    facts: &mut BindingFacts,
    tokens: &mut Vec<String>,
) -> Option<()> {
    // (宣言, 直前の属性)。
    let mut declarations: Vec<(Node<'_>, Vec<Node<'_>>)> = Vec::new();
    let mut attributes = Vec::new();
    let mut cursor = scope.walk();
    for child in scope.named_children(&mut cursor) {
        match child.kind() {
            _ if child.is_extra() => {}
            "inner_attribute_item" => {}
            "attribute_item" => attributes.push(child),
            "macro_invocation" | "expression_statement" => return None,
            _ if child.id() == item.id() => attributes.clear(),
            _ => declarations.push((child, std::mem::take(&mut attributes))),
        }
    }
    // 名前 → その名前で引く宣言 (名前付きの宣言は名前、`use` は現れる名前すべて)。
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    let mut included = vec![false; declarations.len()];
    let mut pending = Vec::new();
    for (ix, (declaration, _)) in declarations.iter().enumerate() {
        match declaration.kind() {
            "use_declaration" => {
                if any_node(*declaration, Children::Named, |node| {
                    node.kind() == "use_wildcard"
                }) {
                    pending.push(ix);
                    continue;
                }
                let mut mentioned = BindingFacts::default();
                collect_bindable_names(*declaration, source, &mut mentioned)?;
                for name in mentioned.names {
                    by_name.entry(name).or_default().push(ix);
                }
            }
            "extern_crate_declaration" => pending.push(ix),
            _ => {
                if let Some(name) = declaration
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                {
                    by_name
                        .entry(name.strip_prefix("r#").unwrap_or(name).to_owned())
                        .or_default()
                        .push(ix);
                }
            }
        }
    }
    let mut names: Vec<String> = facts.names.iter().cloned().collect();
    loop {
        while let Some(name) = names.pop() {
            pending.extend(by_name.remove(&name).into_iter().flatten());
        }
        let Some(ix) = pending.pop() else {
            break;
        };
        if std::mem::replace(&mut included[ix], true) {
            continue;
        }
        let mut found = BindingFacts::default();
        collect_bindable_names(declarations[ix].0, source, &mut found)?;
        facts.rooted_outside |= found.rooted_outside;
        facts.projected |= found.projected;
        for name in found.names {
            if facts.names.insert(name.clone()) {
                names.push(name);
            }
        }
    }
    for ((declaration, attributes), included) in declarations.iter().zip(included) {
        if !included {
            continue;
        }
        tokens.push("<binding>".to_owned());
        for attribute in attributes {
            append_tokens(*attribute, source, tokens)?;
        }
        append_tokens(*declaration, source, tokens)?;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::parser, language::LangId};

    fn contract(source: &str, name: &str) -> Option<RustConstValueContract> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let item = match walk_preorder(tree.root_node(), Children::Named, |node, _| {
            if matches!(node.kind(), "const_item" | "static_item")
                && node
                    .child_by_field_name("name")
                    .and_then(|node| node.utf8_text(source.as_bytes()).ok())
                    == Some(name)
            {
                ControlFlow::Break(node)
            } else {
                ControlFlow::Continue(Walk::Descend)
            }
        }) {
            ControlFlow::Break(node) => node,
            _ => panic!("missing {name}: {source}"),
        };
        rust_const_value_contract(item, source.as_bytes())
    }

    fn same(old: &str, new: &str, name: &str) -> bool {
        let old_contract = contract(old, name);
        old_contract.is_some() && old_contract == contract(new, name)
    }

    #[test]
    fn initializer_only_changes_keep_the_contract() {
        for (old, new) in [
            (
                "use crate::Note;\npub(super) static GREETING: Note = Note {\n    name: \"g\",\n    text: \"Hello.\",\n};\n",
                "use crate::Note;\npub(super) static GREETING: Note = Note {\n    name: \"g\",\n    text: \"Hello, world.\",\n};\n",
            ),
            (
                "pub(crate) const LIMIT: u32 = 10;\n",
                "pub(crate) const LIMIT: u32 = 10 * 4; // comment\n",
            ),
            ("const LIMIT: u32 = 1;\n", "const   LIMIT :u32=2;\n"),
            (
                "#[allow(dead_code)]\n/// doc\npub(crate) const LIMIT: u32 = 1;\n",
                "#[allow(dead_code)]\n/// changed doc\npub(crate) const LIMIT: u32 = 2;\n",
            ),
            (
                "mod inner { pub(super) const LIMIT: u8 = 1; }\n",
                "mod inner { pub(super) const LIMIT: u8 = 2; }\n",
            ),
            (
                "const N: usize = 3;\nconst LIMIT: [u8; N] = [0; N];\n",
                "const N: usize = 3;\nconst LIMIT: [u8; N] = [1; N];\n",
            ),
            // 子の mod の本体の中の宣言は、このスコープの型の名前の束縛を変えない。
            (
                "use crate::Note;\nstatic GREETING: Note = Note;\n#[cfg(test)]\nmod tests { use super::*; }\n",
                "use crate::Note;\nstatic GREETING: Note = Note;\n#[cfg(test)]\nmod tests { use super::*; use std::collections::HashSet; fn t() { assert!(true); } }\n",
            ),
            // 型の名前に関わらない `use` と宣言の変更。
            (
                "use crate::Note;\nconst LIMIT: u32 = 1;\nfn other() {}\n",
                "use crate::Note;\nuse std::fmt::Write;\nconst LIMIT: u32 = 2;\nfn other() { let _ = 1; }\n",
            ),
            // `crate` から始まる型はファイル直下なら比べられる。
            (
                "static GREETING: crate::Note = crate::Note;\n",
                "static GREETING: crate::Note = crate::Note::new();\n",
            ),
            // 列挙子のグロブの取り込みは `use` のトークンごと比べる (関連のパスとして拒まない)。
            (
                "enum Kind { A }\nuse Kind::*;\nconst LIMIT: u32 = 1;\n",
                "enum Kind { A }\nuse Kind::*;\nconst LIMIT: u32 = 2;\n",
            ),
        ] {
            let name = if old.contains("GREETING") {
                "GREETING"
            } else {
                "LIMIT"
            };
            assert!(same(old, new, name), "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn declaration_and_binding_changes_are_not_initializer_only() {
        for (old, new) in [
            // 型・可視性・種類・名前の束縛の変更。
            (
                "pub(crate) const LIMIT: u32 = 1;\n",
                "pub(crate) const LIMIT: u64 = 2;\n",
            ),
            (
                "pub(crate) const LIMIT: u32 = 1;\n",
                "const LIMIT: u32 = 2;\n",
            ),
            (
                "pub(crate) const LIMIT: u32 = 1;\n",
                "pub(crate) static LIMIT: u32 = 2;\n",
            ),
            (
                "#[cfg(feature = \"a\")]\nconst LIMIT: u32 = 1;\n",
                "#[cfg(feature = \"b\")]\nconst LIMIT: u32 = 2;\n",
            ),
            (
                "const LIMIT: u32 = 1;\n",
                "#![allow(unused)]\nconst LIMIT: u32 = 2;\n",
            ),
            (
                "mod inner { pub(super) const LIMIT: u8 = 1; }\n",
                "pub(crate) mod inner { pub(super) const LIMIT: u8 = 2; }\n",
            ),
            (
                "use crate::a::Note;\nstatic LIMIT: Note = Note;\n",
                "use crate::b::Note;\nstatic LIMIT: Note = Note;\n",
            ),
            (
                "type Limit = u8;\nconst LIMIT: Limit = 1;\n",
                "type Limit = u16;\nconst LIMIT: Limit = 2;\n",
            ),
            (
                "type Limit = Inner;\ntype Inner = u8;\nconst LIMIT: Limit = 1;\n",
                "type Limit = Inner;\ntype Inner = u16;\nconst LIMIT: Limit = 2;\n",
            ),
            (
                "const N: usize = 3;\nconst LIMIT: [u8; N] = [0; 3];\n",
                "const N: usize = 4;\nconst LIMIT: [u8; N] = [0; 4];\n",
            ),
            (
                "use crate::*;\nstatic LIMIT: Note = Note;\n",
                "use crate::other::*;\nstatic LIMIT: Note = Note;\n",
            ),
            (
                "struct Note { text: u8 }\nstatic LIMIT: Note = Note { text: 1 };\n",
                "struct Note { body: u8 }\nstatic LIMIT: Note = Note { body: 1 };\n",
            ),
            // 比べる宣言の中に現れる名前もたどる (構造体のフィールドの型、const fn が呼ぶ関数)。
            (
                "type Field = u8;\npub(crate) struct Note { pub(crate) text: Field }\npub(crate) const LIMIT: Note = Note { text: 1 };\n",
                "type Field = u16;\npub(crate) struct Note { pub(crate) text: Field }\npub(crate) const LIMIT: Note = Note { text: 2 };\n",
            ),
            (
                "const fn size() -> usize { 1 }\nconst fn len() -> usize { size() }\npub(crate) const LIMIT: [u8; len()] = [0; len()];\n",
                "const fn size() -> usize { 2 }\nconst fn len() -> usize { size() }\npub(crate) const LIMIT: [u8; len()] = [1; len()];\n",
            ),
        ] {
            assert!(!same(old, new, "LIMIT"), "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn unprovable_declarations_have_no_contract() {
        for source in [
            "static mut LIMIT: u32 = 1;\n",
            "const LIMIT: [u8; line!() as usize] = [0; line!() as usize];\n",
            "#[used]\nstatic LIMIT: u32 = 1;\n",
            "#[cfg_attr(test, allow(dead_code))]\nconst LIMIT: u32 = 1;\n",
            "impl S { const LIMIT: u32 = 1; }\n",
            "trait T { const LIMIT: u32 = 1; }\n",
            "trait T { const LIMIT: u32; }\n",
            "fn f() { const LIMIT: u32 = 1; }\n",
            "extern \"C\" { static LIMIT: u32; }\n",
            // 初期化式の中の item とマクロは外の型を変えうる。
            "const LIMIT: <S as T>::A = { impl T for S { type A = u8; } 1 };\n",
            "const LIMIT: &str = concat!(\"a\", \"b\");\n",
            // item 位置のマクロの呼び出しは同じスコープの名前を作りうる。
            "define_t!();\nconst LIMIT: T = 1;\n",
            "mod inner { define_t!(); pub(super) const LIMIT: T = 1; }\n",
            // 囲む mod があるとき、super / crate から始まる型は別のスコープの宣言で決まる。
            "type T = u8;\nmod inner { pub(super) const LIMIT: super::T = 1; }\n",
            "mod inner { pub(super) const LIMIT: crate::T = 1; }\n",
            // たどった型別名と `use` の先も同じ (`Inner` は同じファイルの別の mod の宣言)。
            "mod model { pub(crate) type Inner = u8; }\nmod config {\n    type Alias = super::model::Inner;\n    pub(crate) const LIMIT: Alias = 1;\n}\n",
            "mod config {\n    use crate::model::Inner;\n    pub(crate) const LIMIT: Inner = 1;\n}\n",
            // 関連型・関連定数の意味は、クレートのどこの impl でも変わる。
            "struct S;\ntrait Tr { type A; }\nimpl Tr for S { type A = u8; }\npub(crate) const LIMIT: <S as Tr>::A = 1;\n",
            "type Alias = <S as Tr>::A;\npub(crate) const LIMIT: Alias = 1;\n",
            "struct Sizes;\nimpl Sizes { const N: usize = 1; }\npub(crate) const LIMIT: [u8; Sizes::N] = [0; Sizes::N];\n",
            "struct Sizes;\nimpl Sizes { const N: usize = 1; }\npub(crate) const LIMIT: [u8; <Sizes>::N] = [0; 1];\n",
            "trait Sizes { const N: usize; }\nimpl Sizes for u8 { const N: usize = 1; }\npub(crate) const LIMIT: [u8; u8::N] = [0; 1];\n",
            "struct _Sizes;\nimpl _Sizes { const N: usize = 1; }\npub(crate) const LIMIT: [u8; _Sizes::N] = [0; 1];\n",
            "const COUNT: usize = Kind::B as usize;\nenum Kind { A, B }\npub(crate) const LIMIT: [u8; COUNT] = [0; 1];\n",
        ] {
            assert!(contract(source, "LIMIT").is_none(), "{source}");
        }
    }

    #[test]
    fn type_names_follow_scope_aliases_and_array_lengths() {
        let contract = contract(
            "use crate::Note;\ntype Notes = [Note; COUNT];\nconst COUNT: usize = SIZE;\nconst SIZE: usize = 2;\nstatic LIMIT: &'static Notes = &[];\n",
            "LIMIT",
        )
        .expect("contract");
        for name in ["Note", "Notes", "COUNT", "SIZE"] {
            assert!(contract.type_names.contains(name), "{name}: {contract:?}");
        }
        assert!(!contract.type_names.contains("static"), "{contract:?}");
        // `use a::Note as Alias` の取り込み元の名前も、ほかのファイルの変更と突き合わせる。
        let aliased = super::tests::contract(
            "use crate::model::Note as Alias;\npub(crate) static LIMIT: Alias = Alias { text: 1 };\n",
            "LIMIT",
        )
        .expect("contract");
        for name in ["Alias", "Note", "model"] {
            assert!(aliased.type_names.contains(name), "{name}: {aliased:?}");
        }
    }

    /// 宣言の数に比例する手間で、逆順に並んだ型別名の鎖もたどれること (不動点の繰り返しにしない)。
    #[test]
    fn long_reverse_alias_chains_are_followed_once() {
        let count = 2_000;
        let mut source = String::new();
        for ix in (1..count).rev() {
            source.push_str(&format!("type A{ix} = A{};\n", ix - 1));
        }
        source.push_str("type A0 = u8;\n");
        source.push_str(&format!("const LIMIT: A{} = 1;\n", count - 1));
        let started = std::time::Instant::now();
        let contract = contract(&source, "LIMIT").expect("contract");
        assert!(contract.type_names.contains("A0"), "{contract:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    /// 判定に使うノード名・フィールド名が Rust の文法に実在すること。
    #[test]
    fn const_value_node_kinds_exist_in_the_rust_grammar() {
        let language = LangId::Rust.ts_language();
        for kind in [
            "const_item",
            "static_item",
            "mutable_specifier",
            "macro_invocation",
            "macro_definition",
            "use_declaration",
            "extern_crate_declaration",
            "source_file",
            "declaration_list",
            "mod_item",
            "inner_attribute_item",
            "attribute_item",
            "expression_statement",
            "use_wildcard",
            "type_item",
            "lifetime",
            "crate",
            "super",
            "qualified_type",
            "identifier",
            "type_identifier",
            "primitive_type",
        ] {
            assert_ne!(language.id_for_node_kind(kind, true), 0, "{kind}");
        }
        for field in ["value", "type", "body", "name"] {
            assert!(language.field_id_for_name(field).is_some(), "{field}");
        }
    }
}
