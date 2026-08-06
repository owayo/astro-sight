//! 参照 identifier の「使われ方」と確信度の分類。
//!
//! Rust は AST 上の使われ方を [`RefUsageRole`] に、PHP は receiver の型推論可否を
//! `RefConfidence` に落とす。いずれも impact 分析で blocking / informational を
//! 振り分ける材料になる。

use tree_sitter::Node;

use crate::language::LangId;
use crate::models::reference::RefConfidence;

/// 参照 identifier の AST 上の使われ方 (Rust のみ分類、他言語は `Other`)。
///
/// シグネチャのみ変更された関数への `FunctionValue` 参照 (高階 API への値渡し) は
/// トレイト境界 (Bevy `IntoSystem` 等) に吸収されコンパイルが通ることが多く、
/// blocking impact ではなく informational へ格下げする材料になる。
/// `fn(...)` 型へ固定される `TypeConstrainedValue` はシグネチャ変更で壊れるため
/// blocking 維持。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefUsageRole {
    /// `foo(...)` の callee 位置。
    CallCallee,
    /// arguments / tuple / array 要素・メソッドレシーバとしての関数値渡し。
    FunctionValue,
    /// `let x: fn(..) = foo;` / `foo as fn(..)` のような fn ポインタ型固定の値。
    TypeConstrainedValue,
    /// それ以外 (型位置・通常の変数参照・分類対象外言語)。
    Other,
}

pub(crate) fn is_rust_macro_invocation_callee(node: Node<'_>, lang_id: LangId) -> bool {
    if lang_id != LangId::Rust {
        return false;
    }
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            // macro 引数は token_tree 配下に入るため callee ではない。
            "token_tree" => return false,
            "macro_invocation" => return true,
            "macro_definition" | "function_item" | "struct_item" | "enum_item" | "source_file" => {
                return false;
            }
            _ => {}
        }
        cur = parent;
    }
    false
}

/// Rust の参照 identifier の使われ方を分類する (他言語は `Other`)。
/// `scoped_identifier` (`path::name`) / `generic_function` (`f::<T>`) のラッパーと
/// 冗長括弧 (`(my_system).after(x)`) は透過してから直上の親で判定する。
pub(crate) fn classify_ref_usage_role(node: Node<'_>, lang_id: LangId) -> RefUsageRole {
    if lang_id != LangId::Rust {
        return RefUsageRole::Other;
    }
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            // 冗長括弧も透過し、field_expression の value 等の実文脈で判定する
            // (`(my_system).after(x)` の receiver を Other に誤格上げしない)。
            "scoped_identifier" | "generic_function" | "parenthesized_expression" => cur = parent,
            _ => break,
        }
    }
    let Some(parent) = cur.parent() else {
        return RefUsageRole::Other;
    };
    match parent.kind() {
        "call_expression" => {
            if parent
                .child_by_field_name("function")
                .is_some_and(|f| f.id() == cur.id())
            {
                RefUsageRole::CallCallee
            } else {
                RefUsageRole::Other
            }
        }
        // 関数値としての受け渡し位置のうち、タプル/配列の**要素**のみ格下げ対象。
        // 直接引数 (`arguments` 直下、`accept(my_system)`) は呼び出し先が
        // `fn(fn(u32))` のような具体 fn ポインタ引数を取る場合に型変更で壊れるが、
        // AST だけでは渡し先シグネチャを解決できないため blocking (Other) を維持する。
        // タプル/配列要素は fn ポインタを直接取る API がほぼ存在せず (型パラメータ
        // 経由のトレイト境界になる)、Bevy `add_systems(Update, (a, b, c))` パターンを
        // 安全に informational 化できる。ただし `let h: (fn(u32),) = (my_system,);` の
        // ように外側へ明示型注釈が付く場合は fn ポインタ型固定でシグネチャ変更が
        // コンパイルエラーになるため blocking を維持する。
        "tuple_expression" | "array_expression" => {
            if tuple_or_array_has_explicit_type(parent) {
                RefUsageRole::Other
            } else {
                RefUsageRole::FunctionValue
            }
        }
        "field_expression" => {
            if parent
                .child_by_field_name("value")
                .is_some_and(|v| v.id() == cur.id())
            {
                RefUsageRole::FunctionValue
            } else {
                RefUsageRole::Other
            }
        }
        // `let x: fn(..) = foo;` / `static X: fn(..) = foo;` / `foo as fn(..)` は
        // fn ポインタ型に固定されるためシグネチャ変更で壊れる。いずれも value/type の
        // フィールド名が共通なので同一アームで判定する。
        "let_declaration" | "const_item" | "static_item" | "type_cast_expression" => {
            let is_value = parent
                .child_by_field_name("value")
                .is_some_and(|v| v.id() == cur.id());
            let is_fn_type = parent
                .child_by_field_name("type")
                .is_some_and(|t| t.kind() == "function_type");
            if is_value && is_fn_type {
                RefUsageRole::TypeConstrainedValue
            } else {
                RefUsageRole::Other
            }
        }
        _ => RefUsageRole::Other,
    }
}

/// タプル/配列リテラルが明示型注釈付きの束縛・cast に直接使われているかを返す。
/// ネストしたタプル/配列は透過して外側を確認する。`let h: (fn(u32),) = (my_system,);`
/// のような外側明示型は要素の fn 参照を型固定するため、informational 格下げ対象から外す。
fn tuple_or_array_has_explicit_type(tuple_or_array: Node<'_>) -> bool {
    let mut cur = tuple_or_array;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            // 冗長括弧 (`((my_system,))`) も透過して外側の束縛を確認する。
            "tuple_expression" | "array_expression" | "parenthesized_expression" => cur = parent,
            "let_declaration" | "const_item" | "static_item" | "type_cast_expression" => {
                let is_value = parent
                    .child_by_field_name("value")
                    .is_some_and(|v| v.id() == cur.id());
                return is_value && parent.child_by_field_name("type").is_some();
            }
            _ => return false,
        }
    }
    false
}

/// receiver-aware な method ref 確信度判定 (Phase 3 で PHP に拡張予定)。
/// - 定義ノード: ExactOwner (定義そのもの)
/// - PHP の `member_call_expression` 直下の name node:
///   - `Foo::bar()` → ExactOwner (`scoped_call_expression`)
///   - `[Foo::class, 'bar']` → ExactOwner (callable array、別経路で処理)
///   - `$x->bar()` で `@var Foo $x` 解析できれば InferredOwner、無ければ BareNameOnly
/// - PHP 以外: 既存挙動維持のため ExactOwner を返す
pub(crate) fn classify_method_ref_confidence(
    node: Node<'_>,
    source: &[u8],
    lang_id: LangId,
    is_def: bool,
) -> RefConfidence {
    if is_def {
        return RefConfidence::ExactOwner;
    }
    if !matches!(lang_id, LangId::Php) {
        return RefConfidence::ExactOwner;
    }
    classify_php_method_ref(node, source)
}

/// PHP の identifier ノードから method ref の確信度を判定する。
///
/// - `scoped_call_expression` (`Foo::bar()`) の name 子 → `ExactOwner`
/// - `member_call_expression` (`$x->bar()`) の name 子 → `InferredOwner` または `BareNameOnly`
///   - 同関数本体内で `@var Foo $x` または `Foo $x` のパラメータ型注釈があれば InferredOwner
///   - それ以外は BareNameOnly
/// - `function_call_expression` (`bar()`) など receiver なし呼び出しは ExactOwner 扱い
///   (グローバル関数 / namespace 関数として全箇所が caller になりうる)
/// - 上記以外 (定義 / クラス名等) は ExactOwner
fn classify_php_method_ref(node: Node<'_>, source: &[u8]) -> RefConfidence {
    let Some(parent) = node.parent() else {
        return RefConfidence::ExactOwner;
    };
    match parent.kind() {
        // Foo::bar() — class scope が明示されているので ExactOwner
        "scoped_call_expression" | "scoped_property_access_expression" => RefConfidence::ExactOwner,
        // $x->bar() — receiver の型を最低限調査
        "member_call_expression" | "member_access_expression" => {
            php_member_call_inferred_or_bare(parent, source)
        }
        _ => RefConfidence::ExactOwner,
    }
}

/// `$x->bar()` の receiver `$x` の型を簡易判定する。
///
/// 同一関数本体内で以下のいずれかが見つかれば InferredOwner、なければ BareNameOnly:
/// - `Foo $x` パラメータ型注釈 (parameter declaration with type)
/// - `@var Foo $x` PHPDoc コメント (簡易テキスト検索)
/// - `$x = new Foo(...)` 代入
///
/// 詳細な型推論は行わず、見つかったら InferredOwner にする保守的判定。
/// `$this->bar()` は InferredOwner (enclosing class が判明している)。
fn php_member_call_inferred_or_bare(call_expr: Node<'_>, source: &[u8]) -> RefConfidence {
    // receiver ノードを取得 (member_call_expression の object フィールド)
    let receiver = call_expr.child_by_field_name("object");
    let Some(receiver) = receiver else {
        return RefConfidence::BareNameOnly;
    };

    // $this->bar(): enclosing class が型として推論可能
    if let Ok(rcv_text) = receiver.utf8_text(source)
        && rcv_text == "$this"
    {
        return RefConfidence::InferredOwner;
    }

    // 変数名を抽出 ($x → "x")
    let var_name = match receiver.utf8_text(source) {
        Ok(t) if t.starts_with('$') => &t[1..],
        _ => return RefConfidence::BareNameOnly,
    };

    // 同一関数スコープを上方向に探索
    let Some(func_body) = enclosing_function_body(call_expr) else {
        return RefConfidence::BareNameOnly;
    };

    // (1) パラメータ型注釈 / (2) `@var` コメント / (3) `new ClassName()` 代入を探す
    if php_scope_has_inferable_var_type(func_body, source, var_name) {
        RefConfidence::InferredOwner
    } else {
        RefConfidence::BareNameOnly
    }
}

/// 与えられたノードを内包する関数 / メソッド本体ノードを返す。
fn enclosing_function_body<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            // `anonymous_function` は tree-sitter-php 0.24 での
            // `anonymous_function_creation_expression` からの改称後の名前。
            // 旧名だけだと closure 内の `$obj->m()` が InferredOwner に上がらず
            // BareNameOnly に落ちる。移行期の互換のため旧名も残す。
            "method_declaration"
            | "function_definition"
            | "function_static_declaration"
            | "anonymous_function"
            | "anonymous_function_creation_expression"
            | "arrow_function" => {
                return n.child_by_field_name("body");
            }
            _ => current = n.parent(),
        }
    }
    None
}

/// 関数本体内に変数 `$var_name` の型推論材料があるかを判定する。
fn php_scope_has_inferable_var_type(body: Node<'_>, source: &[u8], var_name: &str) -> bool {
    let var_marker = format!("${var_name}");
    php_scope_has_inferable_var_type_recursive(body, source, &var_marker)
}

fn php_scope_has_inferable_var_type_recursive(
    node: Node<'_>,
    source: &[u8],
    var_marker: &str,
) -> bool {
    match node.kind() {
        // simple_parameter / parameter は子に type と name を持つ
        "simple_parameter" | "property_promotion_parameter" => {
            let has_type = node.child_by_field_name("type").is_some();
            let name_match = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .is_some_and(|t| t == var_marker);
            if has_type && name_match {
                return true;
            }
        }
        // $x = new Foo(...) の代入式
        "assignment_expression" => {
            let lhs_match = node
                .child_by_field_name("left")
                .and_then(|n| n.utf8_text(source).ok())
                .is_some_and(|t| t == var_marker);
            let rhs_is_object_creation = node
                .child_by_field_name("right")
                .is_some_and(|n| n.kind() == "object_creation_expression");
            if lhs_match && rhs_is_object_creation {
                return true;
            }
        }
        // PHPDoc `@var Foo $x` を含む block-level コメント
        "comment" => {
            if let Ok(text) = node.utf8_text(source)
                && text.contains("@var")
                && text.contains(var_marker)
            {
                // 雑だが「@var Foo $x」が同じ comment 内にあれば InferredOwner と判定
                return true;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if php_scope_has_inferable_var_type_recursive(child, source, var_marker) {
            return true;
        }
    }
    false
}
