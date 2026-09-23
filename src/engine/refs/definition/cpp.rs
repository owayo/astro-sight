//! C/C++ の定義コンテキスト判定と、マクロ本体由来の参照源。
//!
//! `struct X` / `enum X` の型使用が `*_specifier` 配下に現れるため、汎用の
//! parent/grandparent 判定ではパラメータ型やローカル変数型まで Definition に
//! 化ける。本体付き tag 定義・関数本体付き定義の名前だけを Definition とする。

use tree_sitter::Node;

use crate::engine::refs::{LineIndex, absolute_position};
use crate::language::LangId;

/// C/C++ は `struct X` / `enum X` の型使用が `*_specifier` 配下に現れるため、
/// 汎用 parent/grandparent 判定だとパラメータ型やローカル変数型まで Definition になる。
/// body 付き tag 定義・関数本体付き定義の名前だけを Definition として扱う。
pub(crate) fn is_cpp_definition_context(node: Node<'_>) -> bool {
    if let Some(is_def) = cpp_typedef_enum_definition_context(node) {
        return is_def;
    }
    if let Some(is_def) = cpp_tag_specifier_definition_context(node) {
        return is_def;
    }
    if let Some(is_def) = cpp_function_definition_context(node) {
        return is_def;
    }
    if let Some(parent) = node.parent()
        && parent.kind() == "namespace_definition"
    {
        return parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id());
    }
    false
}

/// `struct X;` / `class X;` のような単独 forward declaration は定義でも参照でもない。
/// 型使用 (`struct X *p`) は declarator を持つ declaration なのでここでは除外しない。
pub(crate) fn is_cpp_standalone_forward_declaration_tag_name(node: Node<'_>) -> bool {
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            "struct_specifier" | "class_specifier" | "union_specifier" | "enum_specifier" => {
                let is_name = parent
                    .child_by_field_name("name")
                    .is_some_and(|name| name.id() == node.id());
                if !is_name || cpp_tag_specifier_has_body(parent) {
                    return false;
                }
                return cpp_tag_specifier_is_standalone_forward_declaration(parent);
            }
            "type_definition"
            | "function_definition"
            | "parameter_declaration"
            | "compound_statement" => return false,
            _ => {}
        }
        cur = parent;
    }
    false
}

fn cpp_tag_specifier_is_standalone_forward_declaration(spec: Node<'_>) -> bool {
    let Some(parent) = spec.parent() else {
        return false;
    };
    match parent.kind() {
        "translation_unit" | "namespace_definition" | "type_definition" => true,
        "declaration" | "field_declaration" => !cpp_declaration_has_declarator(parent),
        "declaration_list" => parent.parent().is_some_and(|p| {
            matches!(
                p.kind(),
                "translation_unit"
                    | "namespace_definition"
                    | "struct_specifier"
                    | "class_specifier"
                    | "union_specifier"
            )
        }),
        _ => false,
    }
}

fn cpp_declaration_has_declarator(decl: Node<'_>) -> bool {
    if decl.child_by_field_name("declarator").is_some() {
        return true;
    }
    let mut cursor = decl.walk();
    decl.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            "init_declarator"
                | "pointer_declarator"
                | "reference_declarator"
                | "array_declarator"
                | "function_declarator"
                | "parenthesized_declarator"
                | "identifier"
                | "field_identifier"
        )
    })
}

/// `struct X {}` / `class X {}` / `enum X {}` の tag 名だけを Definition とする。
/// `struct X *p` / `sizeof(struct X)` は型参照。forward declaration `struct X;` は別途 skip。
fn cpp_tag_specifier_definition_context(node: Node<'_>) -> Option<bool> {
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            "struct_specifier" | "class_specifier" | "union_specifier" | "enum_specifier" => {
                let is_name = parent
                    .child_by_field_name("name")
                    .is_some_and(|name| name.id() == node.id());
                return Some(is_name && cpp_tag_specifier_has_body(parent));
            }
            "type_definition"
            | "function_definition"
            | "declaration"
            | "field_declaration"
            | "parameter_declaration"
            | "compound_statement"
            | "translation_unit" => return None,
            _ => {}
        }
        cur = parent;
    }
    None
}

fn cpp_tag_specifier_has_body(spec: Node<'_>) -> bool {
    let mut cursor = spec.walk();
    spec.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            "field_declaration_list" | "enumerator_list" | "declaration_list"
        )
    })
}

/// 本体を持つ C/C++ function_definition の宣言子名だけを Definition とする。
/// parameter / return type / local declaration の型名は Reference に倒す。
fn cpp_function_definition_context(node: Node<'_>) -> Option<bool> {
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            "function_definition" => {
                let is_name = parent
                    .child_by_field_name("declarator")
                    .and_then(cpp_declarator_name_node)
                    .is_some_and(|name| name.id() == node.id());
                return Some(is_name);
            }
            "declaration" | "field_declaration" | "parameter_declaration" => return Some(false),
            _ => {}
        }
        cur = parent;
    }
    None
}

fn cpp_declarator_name_node(decl: Node<'_>) -> Option<Node<'_>> {
    match decl.kind() {
        "identifier" | "field_identifier" => Some(decl),
        "qualified_identifier" => decl
            .child_by_field_name("name")
            .and_then(cpp_declarator_name_node),
        "function_declarator"
        | "pointer_declarator"
        | "reference_declarator"
        | "parenthesized_declarator"
        | "array_declarator" => decl
            .child_by_field_name("declarator")
            .or_else(|| {
                let mut cursor = decl.walk();
                decl.children(&mut cursor).find(|child| {
                    matches!(
                        child.kind(),
                        "function_declarator"
                            | "pointer_declarator"
                            | "reference_declarator"
                            | "parenthesized_declarator"
                            | "identifier"
                            | "field_identifier"
                            | "qualified_identifier"
                    )
                })
            })
            .and_then(cpp_declarator_name_node),
        _ => None,
    }
}

/// C/C++ の type_definition / enumerator に属する識別子の Definition 判定。
///
/// - `type_definition` の `declarator` フィールド (typedef alias 名) → Definition
/// - `enumerator` の `name` フィールド (列挙子名) → Definition
/// - 上記以外 (typedef の元型、enumerator の value 式内識別子) → 参照 (Some(false))
/// - type_definition / enumerator のいずれにも属さない → None (汎用判定へ委譲)
///
/// enumerator / typedef alias の宣言行を参照と二重計上せず、かつ宣言名 (列挙子名 / alias 名)
/// 経由の参照を liveness 判定に使えるようにするために使う (Issue #11/#12)。
fn cpp_typedef_enum_definition_context(node: Node<'_>) -> Option<bool> {
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            "type_definition" => {
                // 全 declarator の alias 名 leaf を集め、node がそのいずれかと一致すれば
                // Definition。`typedef int (*H)(MYSQL);` の MYSQL (元型参照) や複数 declarator
                // (`typedef S A, *B;`) も正しく扱い、declarator 配下の型参照は Reference に倒す
                // (codex 指摘 1/2)。
                let is_alias = typedef_alias_name_nodes(parent)
                    .iter()
                    .any(|leaf| leaf.id() == node.id());
                return Some(is_alias);
            }
            "enumerator" => {
                let is_name = parent
                    .child_by_field_name("name")
                    .is_some_and(|n| n.id() == node.id());
                return Some(is_name);
            }
            // 型/関数の境界に達したら type_definition / enumerator の外。
            "function_definition"
            | "struct_specifier"
            | "class_specifier"
            | "union_specifier"
            | "enum_specifier"
            | "field_declaration_list"
            | "compound_statement"
            | "translation_unit"
            | "preproc_def" => {
                return None;
            }
            _ => {}
        }
        cur = parent;
    }
    None
}

/// type_definition の全 declarator フィールドから alias 名の leaf ノードを集める。
/// `typedef struct S {} A, *B;` のように複数 declarator がある場合は全 alias 名を返す。
fn typedef_alias_name_nodes<'a>(type_definition: Node<'a>) -> Vec<Node<'a>> {
    let mut leaves = Vec::new();
    let mut cursor = type_definition.walk();
    for decl in type_definition.children_by_field_name("declarator", &mut cursor) {
        if let Some(leaf) = typedef_declarator_leaf(decl) {
            leaves.push(leaf);
        }
    }
    leaves
}

/// declarator (type_identifier / pointer_declarator / function_declarator 等) から
/// alias 名の leaf identifier を取り出す。pointer/array/function declarator を剥がして
/// 最終的な名前ノードを返す。
fn typedef_declarator_leaf(decl: Node<'_>) -> Option<Node<'_>> {
    match decl.kind() {
        "type_identifier" | "identifier" => Some(decl),
        _ => decl
            .child_by_field_name("declarator")
            .and_then(typedef_declarator_leaf),
    }
}

/// C/C++ のマクロ定義本体 (`#define NAME body` / `#define F(x) body` の `preproc_arg`) を
/// 識別子トークンへ分割し、参照セグメント `(name, row, col)` として返す。
///
/// tree-sitter-c/cpp は `preproc_arg` を不透明なテキストとして返すため、
/// `#define CLAMP(v) clamp_impl((v), 0, 255)` 経由でしか使われない `clamp_impl` が
/// 参照 0 件で dead に出ていた (`refs` も定義行しか返さなかった)。
///
/// 文字列・文字リテラルとコメント (`preproc_arg` は行末の `// ...` を含み得る) の中は
/// 数えず、数値リテラル (`0x1Fu` / `1e10` / `1'000`) の断片も識別子にしない。
/// マクロ仮引数 (`v`) や `##` で連結される断片も参照として数えられるが、
/// 「参照を過大に数える = dead と断定しない」保守側として許容する。
/// `#pragma` / `#error` 等 (`preproc_call`) の引数は散文を含み得るので対象外。
pub(crate) fn cpp_macro_body_ref_segments<'a>(
    node: Node<'_>,
    source: &'a [u8],
    lang_id: LangId,
) -> Vec<(&'a str, usize, usize)> {
    if !matches!(lang_id, LangId::C | LangId::Cpp) || node.kind() != "preproc_arg" {
        return Vec::new();
    }
    let is_macro_body = node.parent().is_some_and(|parent| {
        matches!(parent.kind(), "preproc_def" | "preproc_function_def")
            && parent
                .child_by_field_name("value")
                .is_some_and(|value| value.id() == node.id())
    });
    if !is_macro_body {
        return Vec::new();
    }
    let Some(body) = source.get(node.byte_range()) else {
        return Vec::new();
    };
    let tokens = macro_body_identifier_tokens(body);
    if tokens.is_empty() {
        return Vec::new();
    }
    // 継続行 (`\` 改行) を含む本体では 2 行目以降の位置がずれるため、本体内の相対位置を
    // 行索引で求めてからファイル上の位置へ変換する。
    let lines = LineIndex::new(body);
    let base = node.start_position();
    tokens
        .into_iter()
        .filter_map(|(start, end)| {
            let text = std::str::from_utf8(&body[start..end]).ok()?;
            let rel = lines.row_col(body.len(), start);
            let (row, col) = absolute_position((base.row, base.column), rel);
            Some((text, row, col))
        })
        .collect()
}

/// マクロ本体テキストから識別子トークンの byte 範囲 `(start, end)` を列挙する。
///
/// 非 ASCII バイトは識別子の構成文字として扱う (トークン境界が必ず ASCII 上に来るので、
/// 切り出した範囲は UTF-8 として正しいまま)。
fn macro_body_identifier_tokens(body: &[u8]) -> Vec<(usize, usize)> {
    let is_ident_start = |b: u8| b == b'_' || b.is_ascii_alphabetic() || b >= 0x80;
    let is_ident_continue = |b: u8| is_ident_start(b) || b.is_ascii_digit();
    let n = body.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let b = body[i];
        let next = body.get(i + 1).copied();
        if b == b'/' && next == Some(b'/') {
            // 行コメント: 行末まで。
            i = body[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(n, |p| i + p);
        } else if b == b'/' && next == Some(b'*') {
            i = body[i + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(n, |p| i + 2 + p + 2);
        } else if b == b'"' || b == b'\'' {
            // 文字列・文字リテラル。閉じ引用符まで (エスケープは 1 文字読み飛ばす)。
            let mut j = i + 1;
            while j < n && body[j] != b {
                j += if body[j] == b'\\' { 2 } else { 1 };
            }
            i = (j + 1).min(n);
        } else if b.is_ascii_digit() || (b == b'.' && next.is_some_and(|c| c.is_ascii_digit())) {
            // pp-number (`0x1Fu` / `1e+10` / `1'000'000`)。数値の途中の英字を識別子にしない。
            let mut j = i + 1;
            while j < n {
                let c = body[j];
                // 指数部の符号 (`1e+10` / `0x1p-3`) も数値の一部。
                let exponent_sign =
                    matches!(c, b'+' | b'-') && matches!(body[j - 1], b'e' | b'E' | b'p' | b'P');
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || exponent_sign {
                    j += 1;
                } else if c == b'\'' && body.get(j + 1).is_some_and(u8::is_ascii_alphanumeric) {
                    j += 2;
                } else {
                    break;
                }
            }
            i = j.min(n);
        } else if is_ident_start(b) {
            let mut j = i + 1;
            while j < n && is_ident_continue(body[j]) {
                j += 1;
            }
            // 文字列の接頭辞 (`L"..."` / `u8"..."` / `R"(...)"`) は識別子ではない。
            let is_literal_prefix = matches!(body.get(j), Some(b'"' | b'\''))
                && matches!(
                    &body[i..j],
                    b"L" | b"u" | b"U" | b"u8" | b"R" | b"LR" | b"uR" | b"UR" | b"u8R"
                );
            if !is_literal_prefix {
                out.push((i, j));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}
