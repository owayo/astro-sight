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
pub(crate) fn is_definition_context(
    node: Node<'_>,
    definition_kinds: &[&str],
    lang_id: LangId,
) -> bool {
    if lang_id == LangId::Ruby {
        return is_ruby_definition_context(node);
    }
    if lang_id == LangId::Php {
        return is_php_definition_context(node);
    }
    if matches!(
        lang_id,
        LangId::Typescript | LangId::Tsx | LangId::Javascript
    ) {
        return is_js_ts_definition_context(node, definition_kinds);
    }
    if lang_id == LangId::Python {
        return is_python_definition_context(node, definition_kinds);
    }
    if lang_id == LangId::Zig {
        return is_zig_definition_context(node, definition_kinds);
    }

    if matches!(lang_id, LangId::C | LangId::Cpp) {
        return is_cpp_definition_context(node);
    }

    if let Some(parent) = node.parent() {
        // 親ノードが定義ノードかチェック
        if definition_kinds.contains(&parent.kind()) {
            return true;
        }
        // 祖父ノードもチェック（例: function_declarator > identifier）
        if let Some(grandparent) = parent.parent()
            && definition_kinds.contains(&grandparent.kind())
        {
            return true;
        }
    }
    false
}

pub(crate) fn is_ignored_identifier_context(node: Node<'_>, lang_id: LangId) -> bool {
    matches!(lang_id, LangId::C | LangId::Cpp)
        && is_cpp_standalone_forward_declaration_tag_name(node)
}

/// JS/TS/TSX: 識別子が「宣言の `name` フィールド」であるときだけ `Definition` とみなす。
///
/// 単純な parent/grandparent 走査では `function parseExcel(): ExcelParseResult {}` の
/// `ExcelParseResult` (戻り値型) や `class A extends B {}` の `B` が grandparent
/// `function_declaration` / `class_declaration` 等にぶら下がって def と誤判定される。
/// これにより dead-code 判定で「型が ref されているのに def しか見つからない」状況が
/// 発生する (例: `excel-service.ts` で戻り値型 `ExcelParseResult` が dead 扱いになる)。
/// PHP と同じく `name` フィールドの一致を要求し、return_type / extends_clause 等の中の
/// 識別子は ref として分類する。
fn is_js_ts_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
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
