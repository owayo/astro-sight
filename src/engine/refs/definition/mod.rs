//! identifier が定義コンテキストにあるかの言語別判定。
//!
//! 言語ごとの分岐は本モジュールの [`is_definition_context`] /
//! [`definition_node_kinds`] に集約する。文法差が大きく判定が長くなる C/C++・PHP・
//! Ruby・Rust は言語別サブモジュールへ分け、そこに当該言語の参照解析ヘルパー
//! (case 折りたたみ規則・文字列由来の synthetic 参照源) も同居させる。
//!
//! [`definition_node_kinds`] は `LangId` を全列挙し `_` catch-all を置かない。
//! 汎用スライスの流用や grammar の改称に追随できていない場合、エラーにならず
//! 「1 つもマッチせず参照が 0 件」という静かな壊れ方をするため。

pub(crate) mod cpp;
pub(crate) mod php;
pub(crate) mod ruby;
pub(crate) mod rust;

use tree_sitter::Node;

use crate::language::LangId;

use cpp::{is_cpp_definition_context, is_cpp_standalone_forward_declaration_tag_name};
use php::is_php_definition_context;
use ruby::is_ruby_definition_context;

/// この identifier ノードが定義コンテキストにあるかを判定する。
///
/// `LangId` を全列挙し `_` catch-all を置かない。言語追加時にコンパイルエラーで
/// 「どの判定に載せるか」を必ず決めさせるため (`definition_node_kinds` と同じ方針)。
pub(crate) fn is_definition_context(
    node: Node<'_>,
    definition_kinds: &[&str],
    lang_id: LangId,
) -> bool {
    match lang_id {
        LangId::Ruby => is_ruby_definition_context(node),
        LangId::Php => is_php_definition_context(node),
        LangId::Typescript | LangId::Tsx | LangId::Javascript => {
            is_name_field_definition_context(node, definition_kinds)
        }
        LangId::Python => is_python_definition_context(node, definition_kinds),
        LangId::Zig => is_zig_definition_context(node, definition_kinds),
        LangId::C | LangId::Cpp => is_cpp_definition_context(node),
        LangId::Go => is_go_definition_context(node, definition_kinds),
        LangId::Java | LangId::CSharp => is_name_field_definition_context(node, definition_kinds),
        LangId::Swift => is_swift_definition_context(node, definition_kinds),
        LangId::Kotlin => is_kotlin_definition_context(node, definition_kinds),
        // 未移行: 汎用の parent/grandparent 走査。型注釈位置の識別子を def と誤判定する
        // 既知の欠陥を持つが、言語ごとに実ノード名とフィールド構成を確認するまで倒さない。
        LangId::Rust | LangId::Bash | LangId::Xojo => {
            is_ancestor_kind_definition_context(node, definition_kinds)
        }
    }
}

/// 未移行言語向けの汎用判定 (親または祖父が定義ノードなら def)。
///
/// 型注釈位置 (戻り値型・基底クラス) の識別子を祖父ノード経由で def と誤判定するため、
/// 言語ごとの実ノード名を確認しながら順次 name 一致方式へ置き換えていく。
fn is_ancestor_kind_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if definition_kinds.contains(&parent.kind()) {
        return true;
    }
    parent
        .parent()
        .is_some_and(|grandparent| definition_kinds.contains(&grandparent.kind()))
}

pub(crate) fn is_ignored_identifier_context(node: Node<'_>, lang_id: LangId) -> bool {
    matches!(lang_id, LangId::C | LangId::Cpp)
        && is_cpp_standalone_forward_declaration_tag_name(node)
}

/// 定義ノードが `name` フィールドを持つ文法で、識別子が「宣言の `name` フィールド」
/// であるときだけ `Definition` とみなす。JS/TS/TSX・Go・Java・C# が使う
/// (Swift は `name` フィールドが宣言名と戻り値型で重複するため専用の
/// [`is_swift_definition_context`])。
///
/// 単純な parent/grandparent 走査では `function parseExcel(): ExcelParseResult {}` の
/// `ExcelParseResult` (戻り値型) や `class A extends B {}` の `B` が grandparent
/// `function_declaration` / `class_declaration` 等にぶら下がって def と誤判定される。
/// これにより dead-code 判定で「型が ref されているのに def しか見つからない」状況が
/// 発生する (例: `excel-service.ts` で戻り値型 `ExcelParseResult` が dead 扱いになる)。
/// PHP と同じく `name` フィールドの一致を要求し、return_type / extends_clause 等の中の
/// 識別子は ref として分類する。
///
/// 各言語で戻り値型・基底クラスがぶら下がる位置 (`astro-sight ast` で確認済み):
/// - Go: `function_declaration` / `method_declaration` の `result:` が直接子
/// - Java: `method_declaration` の `type:` が直接子、`class_declaration` の
///   `superclass:` は `superclass` ノードを挟む
/// - C#: `method_declaration` の `returns:` が直接子、`class_declaration` の基底型は
///   `base_list` を挟む
fn is_name_field_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };

    // parent が定義ノード: name フィールド一致を要求
    if definition_kinds.contains(&parent.kind()) {
        if let Some(name_node) = parent.child_by_field_name("name")
            && name_node.id() == node.id()
        {
            return true;
        }
        return false;
    }

    // grandparent が定義ノード: parent 経由で name フィールドに到達するときのみ def 扱い
    // (例: `variable_declarator > identifier` の identifier は def、
    //      `function_declaration > return_type > type_identifier` は ref)
    if let Some(grandparent) = parent.parent()
        && definition_kinds.contains(&grandparent.kind())
        && let Some(name_node) = grandparent.child_by_field_name("name")
        && (name_node.id() == node.id() || name_node.id() == parent.id())
    {
        return true;
    }
    false
}

/// `parent` の最初の identifier 系の子が `node` かどうかを返す。
///
/// `name` フィールドを持たない宣言ノード (Kotlin の各宣言、Go の `package_clause`) で
/// 宣言名の位置を特定する。素朴な `name` 一致判定だけだと、これらの宣言名そのものが
/// def から漏れて「定義が 1 つも無い」という逆向きの誤りになる。
fn first_identifier_child_matches(parent: Node<'_>, node: Node<'_>) -> bool {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if is_identifier_kind(child.kind()) {
            return child.id() == node.id();
        }
    }
    false
}

/// Go: 宣言の `name` フィールド一致を要求する。
///
/// `package_clause` (`package main` の `main`) だけは `name` フィールドを持たず
/// `package_identifier` が直接ぶら下がるため、最初の identifier 子を名前位置とする
/// (Zig の `variable_declaration` と同型の例外)。
fn is_go_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
    if let Some(parent) = node.parent()
        && parent.kind() == "package_clause"
    {
        return first_identifier_child_matches(parent, node);
    }
    is_name_field_definition_context(node, definition_kinds)
}

/// Swift: 宣言名は「`name` フィールドのうち identifier 系のノード」で判定する。
///
/// tree-sitter-swift の `function_declaration` は関数名と戻り値型の**双方**を `name`
/// フィールドで返す:
/// `(function_declaration name: (simple_identifier) … name: (user_type (type_identifier)))`
/// `child_by_field_name` は最初の一致を返すため結果的に関数名へ当たるが、その順序依存に
/// 頼らず kind まで確認する (戻り値型は `user_type` で identifier 系ではない)。
fn is_swift_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !definition_kinds.contains(&parent.kind()) {
        return false;
    }
    let mut cursor = parent.walk();
    parent
        .children_by_field_name("name", &mut cursor)
        .find(|child| is_identifier_kind(child.kind()))
        .is_some_and(|name| name.id() == node.id())
}

/// Kotlin: tree-sitter-kotlin の宣言ノードは `name` フィールドを持たず、最初の
/// identifier 系の子が宣言名になる (`class_declaration (modifiers) (type_identifier)` /
/// `function_declaration (simple_identifier) …`)。
///
/// 戻り値型は `function_declaration` 直下の `user_type` を挟むため parent が定義ノードに
/// ならず ref に落ちる。継承 (`class Impl : Base()`) も `delegation_specifier >
/// constructor_invocation > user_type` と深いため従来から ref で、本判定でも変わらない。
fn is_kotlin_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !definition_kinds.contains(&parent.kind()) {
        return false;
    }
    first_identifier_child_matches(parent, node)
}

/// Python: 宣言の `name` フィールドと、`parameters` 直下の bare パラメータ名だけを
/// `Definition` とみなす。
///
/// 汎用の parent/grandparent 走査では、型注釈位置の識別子が祖父ノード経由で def と
/// 誤判定される:
/// - `def f() -> Base:` → `function_definition > type > identifier` (祖父が定義ノード)
/// - `class Derived(Base):` → `class_definition > argument_list > identifier` (同上)
///
/// これらが def になると参照として数えられないため、dead-code で「基底クラス」
/// 「戻り値型でしか使われないクラス」が dead と報告され、`api.add` の `refs_internal`
/// も過小になる。JS/TS・Zig と同じく name フィールド一致を要求し、型注釈位置は ref
/// として分類する。
///
/// パラメータ名 (`def f(x)` の `x`) は宣言なので def のまま維持する。型付き
/// パラメータ (`def f(x: int)` の `x`) は `typed_parameter` を挟むため従来から def に
/// ならず、この非対称は本修正の対象外 (揃えるなら Python の binding 分類を
/// まとめて再設計する必要がある)。
///
/// `global x` / `nonlocal x` / `except E as e` / walrus / match capture /
/// lambda 引数 / PEP 695 の型パラメータは、いずれも parent・grandparent が定義ノードに
/// ならないため従来どおり ref のまま (本修正で分類は変わらない)。
fn is_python_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };

    // `def foo` / `class Foo` の名前位置のみ def。return_type や superclasses は ref。
    if definition_kinds.contains(&parent.kind()) {
        return parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id());
    }

    // `def f(x)` の直接パラメータ。`parameters` フィールド経由であることまで確認する。
    if parent.kind() == "parameters"
        && let Some(function) = parent.parent()
        && function.kind() == "function_definition"
        && function
            .child_by_field_name("parameters")
            .is_some_and(|params| params.id() == parent.id())
    {
        return true;
    }

    false
}

/// Zig: 宣言の「名前位置」にある identifier だけを `Definition` とみなす。
///
/// tree-sitter-zig の AST では:
/// - `variable_declaration` は `name` フィールドが無く、最初の子 identifier が変数名
/// - `function_declaration` は `name`/`type`/`body` フィールドあり (戻り値型は `type`)
/// - `test_declaration` は最初の identifier/string が テスト名
/// - `struct_declaration` / `enum_declaration` 等は `name` フィールドあり
///
/// 単純な parent/grandparent 走査では `const Foo = bar()` の `bar` (右辺) や
/// `fn foo() ReturnType { ... }` の `ReturnType` (戻り値型) が def 誤判定される。
/// 各定義種別ごとに「名前位置」を厳密に判定し、それ以外の identifier は ref として返す。
fn is_zig_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !definition_kinds.contains(&parent.kind()) {
        return false;
    }

    // 1. name フィールドが定義されている種別 (function_declaration, struct_declaration,
    //    enum_declaration, union_declaration) は name 一致を要求
    if let Some(name_node) = parent.child_by_field_name("name") {
        return name_node.id() == node.id();
    }

    // 2. variable_declaration / test_declaration は最初の identifier (or string) 子が
    //    名前位置。それ以降の identifier は ref として扱う。
    if matches!(parent.kind(), "variable_declaration" | "test_declaration") {
        let mut cursor = parent.walk();
        for child in parent.children(&mut cursor) {
            if matches!(child.kind(), "identifier" | "string") {
                return child.id() == node.id();
            }
        }
    }

    false
}

/// 言語ごとの定義ノード種別を返す。
/// 静的スライスを返すことで毎回の Vec アロケーションを回避する。
pub(crate) fn definition_node_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &[
            "function_item",
            "function_signature_item", // trait メソッド宣言（ボディなし）
            "struct_item",
            "enum_item",
            "trait_item",
            "impl_item",
            "const_item",
            "static_item",
            "type_item",
            "mod_item",
        ],
        LangId::C => &["function_definition", "struct_specifier", "enum_specifier"],
        LangId::Cpp => &[
            "function_definition",
            "struct_specifier",
            "class_specifier",
            "enum_specifier",
            "namespace_definition",
        ],
        LangId::Python => &["function_definition", "class_definition"],
        LangId::Javascript => &[
            "function_declaration",
            "class_declaration",
            "method_definition",
            "variable_declarator",
        ],
        LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "class_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            "variable_declarator",
        ],
        LangId::Go => &[
            "package_clause",
            "function_declaration",
            "method_declaration",
            "type_spec",
        ],
        LangId::Php => &[
            "function_definition",
            "class_declaration",
            "method_declaration",
            "interface_declaration",
            "enum_declaration",
            "trait_declaration",
        ],
        LangId::Java => &[
            "method_declaration",
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        LangId::Kotlin => &[
            "function_declaration",
            "class_declaration",
            "object_declaration",
        ],
        LangId::Swift => &[
            "function_declaration",
            "class_declaration",
            "protocol_declaration",
        ],
        LangId::CSharp => &[
            "namespace_declaration",
            "method_declaration",
            "class_declaration",
            "struct_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        LangId::Bash => &["function_definition"],
        LangId::Ruby => &[
            "method",
            "singleton_method",
            "class",
            "module",
            "assignment",
        ],
        LangId::Zig => &[
            "function_declaration",
            "variable_declaration",
            "test_declaration",
            "struct_declaration",
            "enum_declaration",
            "union_declaration",
        ],
        LangId::Xojo => &[
            "class_declaration",
            "module_declaration",
            "interface_declaration",
            "structure_declaration",
            "enum_declaration",
            "sub_declaration",
            "function_declaration",
            "constructor_declaration",
            "destructor_declaration",
            "event_declaration",
            "delegate_declaration",
            "simple_property_declaration",
            "computed_property_declaration",
            "const_declaration",
            "field_declaration",
            "declare_declaration",
        ],
    }
}

/// identifier ノードかどうかを判定する。
pub(crate) fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "simple_identifier"
            | "namespace_identifier"
            | "package_identifier"
            | "name"
            | "qualified_name"
            | "word"
            | "constant"
    )
}
