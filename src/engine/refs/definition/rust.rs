//! Rust の参照解析。
//!
//! 関数名と struct フィールド名が衝突したときの誤マッチ除外と、identifier ノードと
//! しては現れない serde 系属性文字列 (`#[serde(serialize_with = "path::fn")]`) の
//! 参照源抽出を持つ。

use tree_sitter::Node;

use crate::language::LangId;

/// Rust の構造体フィールド系ノードを参照として扱うべきでないかを判定する。
///
/// `pub fn redact()` のような関数名と struct field 名 (`pub redact: bool`) が衝突した場合、
/// `cfg.output.redact` のフィールドアクセスや struct 宣言/初期化が関数の呼び出し位置として
/// 誤マッチして impact 分析にノイズを生む (Issue: 2026-05-21-redact-impact-triage)。
///
/// 関数ではないことが構造的に明らかな以下のケースを除外する:
/// - `field_declaration` の field_identifier (struct のフィールド宣言)
/// - `field_initializer` の field 側 field_identifier (`Config { redact: ... }`)
/// - `field_pattern` の field_identifier (destructuring `let Cfg { redact: v } = ...`)
/// - `shorthand_field_initializer` 配下の identifier (`Config { redact }`)
/// - `field_expression` の field_identifier で、祖先 `call_expression.function` でないもの
///   (純粋なフィールドアクセス `obj.redact`)
///
/// 一方、メソッド呼び出し (`obj.method()`) の `method` 部は `field_identifier` ノードだが
/// 親 `field_expression` がさらに親 `call_expression` の `function` フィールドに位置するため
/// 関数参照として残す。
pub(crate) fn is_rust_struct_field_non_callable(node: Node<'_>) -> bool {
    match node.kind() {
        "field_identifier" => {
            let Some(parent) = node.parent() else {
                return false;
            };
            match parent.kind() {
                // `pub redact: bool` の name
                "field_declaration" => true,
                // `Config { redact: ... }` の field 部
                "field_initializer" => parent
                    .child_by_field_name("field")
                    .is_some_and(|n| n.id() == node.id()),
                // `let Cfg { redact: v } = ...` の destructuring 中の field name 部
                // (`field_pattern` の field_identifier は常に name 役割で、pattern 部は別ノード)
                "field_pattern" => true,
                // `obj.redact` または `obj.redact()` の field 部
                "field_expression" => {
                    let Some(grand) = parent.parent() else {
                        // 祖先なし → 純粋なフィールドアクセスとして除外
                        return true;
                    };
                    // method call (`obj.method()`) の `method` 部は関数参照として残す
                    let is_method_call = grand.kind() == "call_expression"
                        && grand
                            .child_by_field_name("function")
                            .is_some_and(|n| n.id() == parent.id());
                    !is_method_call
                }
                _ => false,
            }
        }
        // shorthand: `Config { redact }` の `redact`
        "identifier" => node
            .parent()
            .is_some_and(|p| p.kind() == "shorthand_field_initializer"),
        _ => false,
    }
}

/// Rust の属性引数で文字列値を識別子/パス参照として解釈すべきキー。
/// serde 系の `#[serde(serialize_with = "path::to::fn")]` 形式を想定する。
const RUST_ATTR_STRING_REF_KEYS: &[&str] = &[
    "serialize_with",
    "deserialize_with",
    "with",
    "skip_serializing_if",
    "try_from",
    "from",
    "into",
];

/// `string_content` ノードが Rust の serde 系属性値として現れるかを判定する。
/// 構造: `attribute > token_tree > identifier "=" string_literal > string_content`
fn is_rust_attribute_ref_string(node: Node<'_>, source: &[u8]) -> bool {
    let Some(string_literal) = node.parent() else {
        return false;
    };
    if string_literal.kind() != "string_literal" {
        return false;
    }
    let Some(token_tree) = string_literal.parent() else {
        return false;
    };
    if token_tree.kind() != "token_tree" {
        return false;
    }

    // token_tree の直下兄弟で `identifier "=" string_literal` の並びを検出する。
    let mut cursor = token_tree.walk();
    let mut prev_prev: Option<Node> = None;
    let mut prev: Option<Node> = None;
    for child in token_tree.children(&mut cursor) {
        if child.id() == string_literal.id() {
            let Some(eq) = prev else {
                return false;
            };
            if eq.kind() != "=" {
                return false;
            }
            let Some(key) = prev_prev else {
                return false;
            };
            if key.kind() != "identifier" {
                return false;
            }
            let Ok(key_text) = key.utf8_text(source) else {
                return false;
            };
            return RUST_ATTR_STRING_REF_KEYS.contains(&key_text);
        }
        prev_prev = prev;
        prev = Some(child);
    }
    false
}

/// "Option::is_none" を [("Option", 0), ("is_none", 8)] のように (segment, byte offset) で分割する。
pub(crate) fn split_path_segments(text: &str) -> Vec<(&str, usize)> {
    let mut results = Vec::new();
    let mut offset = 0usize;
    for seg in text.split("::") {
        if !seg.is_empty() {
            results.push((seg, offset));
        }
        offset += seg.len() + 2; // "::"
    }
    results
}

/// Rust 属性の string_content から (segment, row, col) を列挙する。
/// 非 Rust やパターンに合わない場合は空 Vec を返す。
pub(crate) fn rust_attr_string_ref_segments<'a>(
    node: Node<'_>,
    source: &'a [u8],
    lang_id: LangId,
) -> Vec<(&'a str, usize, usize)> {
    if lang_id != LangId::Rust || node.kind() != "string_content" {
        return Vec::new();
    }
    if !is_rust_attribute_ref_string(node, source) {
        return Vec::new();
    }
    let Ok(text) = node.utf8_text(source) else {
        return Vec::new();
    };
    let base = node.start_position();
    split_path_segments(text)
        .into_iter()
        .map(|(seg, off)| (seg, base.row, base.column + off))
        .collect()
}
