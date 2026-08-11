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

/// closure のパラメータが束縛している名前の識別子か (束縛位置・closure 本体の使用の両方)。
///
/// `refs` は名前一致だけで参照を数えるため、`|(_, tail)| tail` のようにローカル束縛が
/// 同名の関数をシャドーイングしていると、本番参照ゼロの関数が「参照あり」に見える。
/// これは dead-code の fail-open (死蔵コードを live と誤認) になる。
/// 実例では 7 件の参照のうち本番参照が 0 件だった。
///
/// スコープを closure に限定するのは、`let` shadowing / match arm / `for` ループまで
/// 広げると宣言順と scope boundary の追跡が必要になり、判定の確度が落ちるため。
/// closure は「パラメータが body 全体をシャドーイングする」ことが構文だけで確定する。
pub(crate) fn is_rust_closure_bound_identifier(node: Node<'_>, name: &str, source: &[u8]) -> bool {
    // 修飾参照・メソッド名・型位置はローカル束縛にシャドーイングされない。
    // 先に弾くことで、祖先走査とパターン再走査のコストも大半の識別子で回避する。
    if !is_rust_shadowable_value_identifier(node) {
        return false;
    }
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "closure_expression"
            && let Some(params) = parent.child_by_field_name("parameters")
            && rust_pattern_binds_name(params, name, source)
        {
            return true;
        }
        current = parent;
    }
    false
}

/// closure 束縛によるシャドーイングの対象になり得る識別子か。
///
/// 対象外にするもの (いずれも同名のローカル束縛があっても外側シンボルを指し続ける。
/// 除外すると live symbol を dead と誤判定する逆向きの事故になる):
/// - `crate::tail()` / `Type::tail()` の `scoped_identifier` 構成要素
/// - `obj.tail()` のメソッド名 (`field_identifier` — kind で弾かれる)
/// - 型位置の識別子 (`type_identifier` / ジェネリック引数 — kind と親で弾かれる)
/// - `tail!()` のマクロ名 — マクロは値とは別の名前空間で、値束縛にシャドーイングされない
fn is_rust_shadowable_value_identifier(node: Node<'_>) -> bool {
    if node.kind() != "identifier" {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    !matches!(
        parent.kind(),
        "scoped_identifier"
            | "scoped_type_identifier"
            | "scoped_use_list"
            | "generic_type"
            | "type_arguments"
            | "type_binding"
            | "macro_invocation"
            | "macro_definition"
            | "attribute"
    )
}

/// Rust のパターンノードが `name` を束縛しているか。
///
/// binding だと構文上確定できる位置だけを辿る。`tuple_struct_pattern` / `struct_pattern`
/// の `type:` と `field_pattern` の `name:` は型名・フィールド名であって束縛ではないので
/// 辿らない。未知のパターン種別は辿らず `false` を返す (= 参照として残す安全側。
/// 束縛の取りこぼしは従来どおりの過大計上に戻るだけだが、逆に非束縛を束縛と誤れば
/// 本物の参照を消してしまう)。
fn rust_pattern_binds_name(node: Node<'_>, name: &str, source: &[u8]) -> bool {
    let matches_name = |n: Node<'_>| n.utf8_text(source).is_ok_and(|text| text == name);
    match node.kind() {
        // `|tail|` / `|(_, tail)|` の束縛位置。
        //
        // パターン中の bare identifier は必ずしも束縛ではない — 単位構造体 (`|Unit| ()`)
        // や定数 (`|MAX| ()`) との照合も同じノードになり、こちらは束縛ではなく参照。
        "identifier" => matches_name(node) && rust_pattern_name_is_binding(node, name, source),
        // `Foo { tail }` の shorthand。フィールド名は常に束縛なので命名規約の制約は不要
        "shorthand_field_identifier" => matches_name(node),
        // `Foo(tail)` / `Foo { .. }` の `type:` は型参照なので飛ばす
        "tuple_struct_pattern" | "struct_pattern" => {
            let type_node = node.child_by_field_name("type");
            let mut cursor = node.walk();
            node.children(&mut cursor).any(|child| {
                type_node.is_none_or(|t| t.id() != child.id())
                    && rust_pattern_binds_name(child, name, source)
            })
        }
        // `Foo { key: tail }` は `pattern:` 側だけが束縛。shorthand なら `name:` が束縛
        "field_pattern" => match node.child_by_field_name("pattern") {
            Some(pattern) => rust_pattern_binds_name(pattern, name, source),
            None => node
                .child_by_field_name("name")
                .is_some_and(|n| n.kind() == "shorthand_field_identifier" && matches_name(n)),
        },
        // `|tail: u8|` は `parameter pattern: (identifier) type: (primitive_type)`
        "parameter" => node
            .child_by_field_name("pattern")
            .is_some_and(|pattern| rust_pattern_binds_name(pattern, name, source)),
        // 束縛を包むだけのパターン。子をそのまま辿る
        "closure_parameters" | "tuple_pattern" | "slice_pattern" | "reference_pattern"
        | "ref_pattern" | "mut_pattern" | "captured_pattern" | "or_pattern" => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .any(|child| rust_pattern_binds_name(child, name, source))
        }
        _ => false,
    }
}

/// パターン中の bare identifier が「束縛」か「定数・単位構造体パターン」かを判定する。
///
/// Rust の単一 identifier パターンは名前解決後に const / static / 単位構造体 /
/// enum variant を指し得る (irrefutable path pattern)。`non_snake_case` /
/// `non_upper_case_globals` は lint であって強制ではないため、命名規約だけでは
/// 証明できない — `#![allow(non_upper_case_globals)] pub const tail: () = ();` に対する
/// `|tail: ()| ()` は束縛ではなく定数パターンで、束縛と誤れば本物の参照を消す。
///
/// 束縛と確定できない次のケースは束縛と判定しない (= 参照として残す安全側):
/// - 先頭の実文字が大文字 (慣習上ほぼ確実に定数・単位構造体・enum variant)
/// - 同一ファイル内に同名の const / static / 単位構造体 / use import がある
/// - ファイルルートを辿れない (判定不能)
fn rust_pattern_name_is_binding(node: Node<'_>, name: &str, source: &[u8]) -> bool {
    if !rust_pattern_name_is_binding_style(node, source) {
        return false;
    }
    let Some(root) = rust_file_root(node) else {
        return false;
    };
    !rust_file_declares_non_binding_name(root, name, source)
}

/// パターン中の識別子が「束縛の命名規約」に従っているか (先頭の実文字が大文字でない)。
fn rust_pattern_name_is_binding_style(node: Node<'_>, source: &[u8]) -> bool {
    node.utf8_text(source).is_ok_and(|text| {
        // 先頭の `_` は「未使用」を示す接頭辞で名前空間を変えないため読み飛ばす。
        // `_Unit` のような leading underscore 付きの単位構造体パターンを束縛と誤れば
        // 本物の参照を消す (先頭 1 文字だけを見ると `_` が小文字扱いになる)。
        // すべて `_` の名前は実文字が無いので束縛と判定しない (安全側)。
        text.chars()
            .find(|c| *c != '_')
            .is_some_and(|first| !first.is_uppercase())
    })
}

/// `node` が属するファイルのルート (`source_file`) を返す。
fn rust_file_root(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        current = parent;
    }
    (current.kind() == "source_file").then_some(current)
}

/// 同一ファイル内に `name` を「パターン位置で束縛ではなくする」宣言があるか。
///
/// const / static / 単位構造体 (body を持たない `struct X;`) はパターン位置で
/// 定数パターンになる。`use` で持ち込まれた名前も同様 (外部の const / unit struct /
/// enum variant かもしれず、ファイル内では確定できない)。
/// いずれかに該当したら束縛判定を諦める (fail-closed)。
fn rust_file_declares_non_binding_name(root: Node<'_>, name: &str, source: &[u8]) -> bool {
    fn walk(node: Node<'_>, name: &str, source: &[u8]) -> bool {
        match node.kind() {
            "const_item" | "static_item" => {
                if node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    == Some(name)
                {
                    return true;
                }
            }
            "struct_item" => {
                // 単位構造体 (`struct X;`) だけがパターン位置で定数パターンになる。
                // フィールドを持つ struct はパターンに `X { .. }` / `X(..)` が要る。
                let is_unit = node.child_by_field_name("body").is_none();
                if is_unit
                    && node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        == Some(name)
                {
                    return true;
                }
            }
            // `use foo::tail;` で持ち込まれた名前は外部の const / unit struct / variant
            // かもしれず、ファイル内では確定できない。
            "use_declaration"
                if node.utf8_text(source).is_ok_and(|text| {
                    text.split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|seg| seg == name)
                }) =>
            {
                return true;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .any(|child| walk(child, name, source))
    }
    walk(root, name, source)
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
