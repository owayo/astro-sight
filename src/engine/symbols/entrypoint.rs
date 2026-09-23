//! 言語規約のプログラムエントリポイント (`main`) 判定。
//!
//! エントリポイントはランタイム / ローダ / リンカが規約の名前と形で呼び出すため、
//! リポジトリ内に呼び出し元が 1 つも無いのが正常。dead-code (実行時入口を除外する経路) が
//! これを「未参照」と報告しないために使う。
//!
//! 判定は名前・スコープ・所有関係を優先し、引数の型は保守的に見る。入口を見落とすと
//! 生きているシンボルを dead と報告する最悪方向の誤りになるため、形が明確に不適合な
//! ものだけを外し、別名や解釈できない構文は入口の可能性を残す。
//!
//! API 差分には持ち込まない。形に依存する判定を旧版 / 新版の双方へ当てると、形の変更だけで
//! 片側だけが除外され、誤った api.add / api.rm を生むため (Laravel relation と同じ境界)。

use tree_sitter::Node;

use crate::language::LangId;
use crate::models::location::Range;
use crate::models::symbol::SymbolKind;

use super::node_for_symbol_range;

/// `symbol_range` のシンボルが、その言語のプログラムエントリポイント、または
/// エントリポイントを (ネストした型の中も含めて) 宣言する型かを判定する。
///
/// 型も対象にするのは、入口が生きているのに所有型だけを dead と報告すると論理的に
/// 矛盾するため (Java の `App` / C# の `Program` はビルド設定からしか参照されない)。
/// ネストした型が入口を持つ場合は外側の型も対象にする (外側を消すと入口も消える)。
/// 兄弟の型や、入口以外のメンバーへは広げない。
pub fn is_program_entrypoint(
    root: Node,
    source: &[u8],
    lang_id: LangId,
    kind: SymbolKind,
    symbol_range: &Range,
) -> bool {
    let (declaration_kinds, is_function) = match kind {
        SymbolKind::Function | SymbolKind::Method => (function_kinds(lang_id), true),
        SymbolKind::Class
        | SymbolKind::Struct
        | SymbolKind::Enum
        | SymbolKind::Interface
        | SymbolKind::Trait => (owner_type_kinds(lang_id), false),
        _ => return false,
    };
    if declaration_kinds.is_empty() {
        return false;
    }
    let Some(declaration) = declaration_at(root, symbol_range, declaration_kinds) else {
        return false;
    };
    if is_function {
        is_entrypoint_function(declaration, source, lang_id)
    } else {
        type_declares_entrypoint(declaration, source, lang_id)
    }
}

/// シンボル自身の宣言ノードを返す。
///
/// 祖先へ無制限に遡ると、判定対象外の宣言 (C# の型の中の `enum` など) が外側の型の
/// 判定結果を借りてしまうため、シンボルと同じ位置から始まるノードだけを候補にする。
fn declaration_at<'tree>(
    root: Node<'tree>,
    symbol_range: &Range,
    kinds: &[&str],
) -> Option<Node<'tree>> {
    let node = node_for_symbol_range(root, symbol_range)?;
    let start = node.start_position();
    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.start_position() != start {
            return None;
        }
        if kinds.contains(&candidate.kind()) {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

/// エントリポイントになりうる関数の宣言ノード。
fn function_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::C | LangId::Cpp => &["function_definition"],
        LangId::Java | LangId::CSharp => &["method_declaration"],
        LangId::Kotlin => &["function_declaration"],
        // Go の `func main` は非 export なので dead-code の候補にならない。
        // Python / JS / TS / PHP / Ruby / Bash は言語規約のエントリポイント関数を持たない
        // (`if __name__ == "__main__":` 等はトップレベルの文で、関数シンボルではない)。
        LangId::Go
        | LangId::Python
        | LangId::Javascript
        | LangId::Typescript
        | LangId::Tsx
        | LangId::Php
        | LangId::Ruby
        | LangId::Bash => &[],
        // Swift の `@main` 型と Zig / Rust の `main` は未対応 (Zig / Rust はどのファイルが
        // root かを単一ファイルから決められず、判定の近似を別途決める必要がある)。
        LangId::Swift | LangId::Zig | LangId::Rust => &[],
        // lexer-only 言語は AST を持たず、この判定へ到達しない。
        LangId::Xojo => &[],
    }
}

/// エントリポイントを宣言しうる型の宣言ノード。
fn owner_type_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Java => JAVA_TYPE_KINDS,
        LangId::CSharp => CSHARP_TYPE_KINDS,
        LangId::Kotlin => KOTLIN_TYPE_KINDS,
        // 入口が自由関数で型に属さない言語と、入口の判定を持たない言語。
        LangId::C
        | LangId::Cpp
        | LangId::Swift
        | LangId::Zig
        | LangId::Rust
        | LangId::Go
        | LangId::Python
        | LangId::Javascript
        | LangId::Typescript
        | LangId::Tsx
        | LangId::Php
        | LangId::Ruby
        | LangId::Bash => &[],
        LangId::Xojo => &[],
    }
}

const JAVA_TYPE_KINDS: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "record_declaration",
];
const CSHARP_TYPE_KINDS: &[&str] = &[
    "class_declaration",
    "struct_declaration",
    "record_declaration",
    "interface_declaration",
];
/// Kotlin の `object` 宣言と `companion object` は `@JvmStatic fun main` の置き場所になる。
const KOTLIN_TYPE_KINDS: &[&str] = &["class_declaration", "object_declaration"];
const KOTLIN_OBJECT_KINDS: &[&str] = &["object_declaration", "companion_object"];
const KOTLIN_BODY_KINDS: &[&str] = &["class_body", "enum_class_body"];

/// C の関数宣言子を包むノード。内側へ辿って関数名の識別子を得る。
const C_DECLARATOR_WRAPPERS: &[&str] = &[
    "function_declarator",
    "pointer_declarator",
    "parenthesized_declarator",
    "attributed_declarator",
];
/// C++ は参照返り (`T& f()`) の `reference_declarator` が加わる。
const CPP_DECLARATOR_WRAPPERS: &[&str] = &[
    "function_declarator",
    "pointer_declarator",
    "parenthesized_declarator",
    "attributed_declarator",
    "reference_declarator",
];
/// グローバルスコープでないことを示す祖先。入口の `main` はグローバルスコープの自由関数に限る
/// (C++ では名前空間やクラスの中の `main` は普通の関数)。
const C_NON_GLOBAL_SCOPE_KINDS: &[&str] =
    &["struct_specifier", "union_specifier", "compound_statement"];
const CPP_NON_GLOBAL_SCOPE_KINDS: &[&str] = &[
    "namespace_definition",
    "class_specifier",
    "struct_specifier",
    "union_specifier",
    "compound_statement",
    "template_declaration",
];

fn is_entrypoint_function(function: Node, source: &[u8], lang_id: LangId) -> bool {
    match lang_id {
        LangId::C => is_c_family_main(
            function,
            source,
            C_DECLARATOR_WRAPPERS,
            C_NON_GLOBAL_SCOPE_KINDS,
        ),
        LangId::Cpp => is_c_family_main(
            function,
            source,
            CPP_DECLARATOR_WRAPPERS,
            CPP_NON_GLOBAL_SCOPE_KINDS,
        ),
        LangId::Java => is_java_main(function, source),
        LangId::CSharp => is_csharp_main(function, source),
        LangId::Kotlin => is_kotlin_main(function, source),
        LangId::Swift
        | LangId::Zig
        | LangId::Rust
        | LangId::Go
        | LangId::Python
        | LangId::Javascript
        | LangId::Typescript
        | LangId::Tsx
        | LangId::Php
        | LangId::Ruby
        | LangId::Bash
        | LangId::Xojo => false,
    }
}

fn type_declares_entrypoint(type_node: Node, source: &[u8], lang_id: LangId) -> bool {
    match lang_id {
        LangId::Java => java_type_declares_main(type_node, source),
        LangId::CSharp => csharp_type_declares_main(type_node, source),
        LangId::Kotlin => kotlin_type_declares_main(type_node, source),
        LangId::C
        | LangId::Cpp
        | LangId::Swift
        | LangId::Zig
        | LangId::Rust
        | LangId::Go
        | LangId::Python
        | LangId::Javascript
        | LangId::Typescript
        | LangId::Tsx
        | LangId::Php
        | LangId::Ruby
        | LangId::Bash
        | LangId::Xojo => false,
    }
}

// --- C / C++ ---

/// グローバルスコープの自由関数 `main` か。引数の形は問わない
/// (`int main(void)` / `int main(int, char**)` / 環境変数付きなど処理系で幅がある)。
fn is_c_family_main(
    function: Node,
    source: &[u8],
    declarator_wrappers: &[&str],
    non_global_scope_kinds: &[&str],
) -> bool {
    let Some(mut declarator) = function.child_by_field_name("declarator") else {
        return false;
    };
    // `int *main()` / `int (main)()` のように宣言子が包まれていても関数名まで辿る。
    // `reference_declarator` などは `declarator` フィールドを持たないので先頭の子を見る。
    while declarator_wrappers.contains(&declarator.kind()) {
        let Some(inner) = declarator
            .child_by_field_name("declarator")
            .or_else(|| declarator.named_child(0))
        else {
            return false;
        };
        declarator = inner;
    }
    // クラス内のメソッドは field_identifier、クラス外定義は qualified_identifier になる。
    declarator.kind() == "identifier"
        && node_text(declarator, source) == "main"
        && !has_ancestor(function, non_global_scope_kinds)
}

// --- Java ---

/// Java の起動プロトコルが選ぶ `main`。
///
/// 名前 `main`・戻り値 `void`・非 private で、引数は無し (Java 25 の JEP 512) か
/// `String[]` 相当 1 個。static / instance の両方を入口とみなす (JEP 512 で instance main も
/// 起動対象になった)。Java には型の別名が無いので、引数の型は字面で厳密に判定できる。
fn is_java_main(method: Node, source: &[u8]) -> bool {
    if field_text(method, "name", source) != Some("main")
        || method.child_by_field_name("type").map(|t| t.kind()) != Some("void_type")
        || java_has_modifier(method, "private")
    {
        return false;
    }
    let Some(parameters) = method.child_by_field_name("parameters") else {
        return false;
    };
    let mut cursor = parameters.walk();
    let params: Vec<Node> = parameters
        .named_children(&mut cursor)
        .filter(|p| matches!(p.kind(), "formal_parameter" | "spread_parameter"))
        .collect();
    match params.as_slice() {
        [] => true,
        [param] => java_param_is_string_array(*param, source),
        _ => false,
    }
}

fn java_has_modifier(declaration: Node, keyword: &str) -> bool {
    let mut cursor = declaration.walk();
    declaration
        .children(&mut cursor)
        .filter(|child| child.kind() == "modifiers")
        .any(|modifiers| {
            let mut inner = modifiers.walk();
            modifiers
                .children(&mut inner)
                .any(|token| token.kind() == keyword)
        })
}

/// `String[] args` / `String args[]` / `String... args` / `java.lang.String[] args` か。
fn java_param_is_string_array(param: Node, source: &[u8]) -> bool {
    let (element, dimensions) = if param.kind() == "spread_parameter" {
        // 可変長引数は型を 1 次元の配列として受け取る。型ノードはフィールド名を持たない。
        let mut cursor = param.walk();
        let Some(ty) = param
            .named_children(&mut cursor)
            .find(|n| !matches!(n.kind(), "modifiers" | "variable_declarator"))
        else {
            return false;
        };
        let (element, dimensions) = java_split_array_type(ty, source);
        (element, dimensions + 1)
    } else {
        let Some(ty) = param.child_by_field_name("type") else {
            return false;
        };
        let (element, dimensions) = java_split_array_type(ty, source);
        // C 形式の `String args[]` は次元が引数名側に付く。
        let c_style = param
            .child_by_field_name("dimensions")
            .map_or(0, |d| bracket_count(d, source));
        (element, dimensions + c_style)
    };
    dimensions == 1 && java_is_string_type(element, source)
}

fn java_split_array_type<'tree>(ty: Node<'tree>, source: &[u8]) -> (Node<'tree>, usize) {
    if ty.kind() != "array_type" {
        return (ty, 0);
    }
    let dimensions = ty
        .child_by_field_name("dimensions")
        .map_or(0, |d| bracket_count(d, source));
    (ty.child_by_field_name("element").unwrap_or(ty), dimensions)
}

fn java_is_string_type(ty: Node, source: &[u8]) -> bool {
    match ty.kind() {
        "type_identifier" | "scoped_type_identifier" => {
            let text: String = node_text(ty, source)
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            text == "String" || text == "java.lang.String"
        }
        // 型注釈付き (`@NonNull String`) は形を確定できないので入口の可能性を残す。
        "annotated_type" => true,
        _ => false,
    }
}

/// 型本体 (ネストした型を含む) に入口の `main` があるか。メソッド本体・フィールド初期化子
/// (匿名クラス) の中は辿らない。
fn java_type_declares_main(type_node: Node, source: &[u8]) -> bool {
    type_node
        .child_by_field_name("body")
        .is_some_and(|body| java_body_declares_main(body, source))
}

fn java_body_declares_main(body: Node, source: &[u8]) -> bool {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .any(|member| match member.kind() {
            "method_declaration" => is_java_main(member, source),
            // enum 定数の後ろのメンバー宣言は enum_body_declarations にまとまる。
            "enum_body_declarations" => java_body_declares_main(member, source),
            kind if JAVA_TYPE_KINDS.contains(&kind) => java_type_declares_main(member, source),
            _ => false,
        })
}

// --- C# ---

/// C# のエントリポイント候補 `static Main`。引数は無しか `string[]` 相当 1 個。
/// 戻り値 (`void` / `int` / `Task` / `Task<int>`) は見ない。
fn is_csharp_main(method: Node, source: &[u8]) -> bool {
    field_text(method, "name", source) == Some("Main")
        && csharp_has_modifier(method, "static", source)
        && csharp_parameters_fit_main(method, source)
}

fn csharp_has_modifier(declaration: Node, keyword: &str, source: &[u8]) -> bool {
    let mut cursor = declaration.walk();
    declaration
        .children(&mut cursor)
        .any(|child| child.kind() == "modifier" && node_text(child, source) == keyword)
}

fn csharp_parameters_fit_main(method: Node, source: &[u8]) -> bool {
    let Some(list) = method.child_by_field_name("parameters") else {
        return false;
    };
    let mut count = 0;
    let mut cursor = list.walk();
    for child in list.children(&mut cursor) {
        match child.kind() {
            "parameter" => {
                count += 1;
                if child
                    .child_by_field_name("type")
                    .is_some_and(|ty| !csharp_type_may_be_string_array(ty, source))
                {
                    return false;
                }
            }
            // `params string[] args` は parameter に包まれず、キーワードと型が
            // parameter_list の直下に並ぶ。
            "params" => count += 1,
            "array_type" if !csharp_type_may_be_string_array(child, source) => return false,
            _ => {}
        }
    }
    count <= 1
}

/// 引数の型が `string[]` でありうるか。組み込み型の非配列 (`int x`) や、要素が `string` 以外の
/// 組み込み型・多次元・ジャグ配列は明確に不適合。`String[]` / `System.String[]` と、
/// 別名 (`using Args = string[];`) でありうる名前付き型は入口の可能性を残す。
fn csharp_type_may_be_string_array(ty: Node, source: &[u8]) -> bool {
    match ty.kind() {
        "predefined_type" => false,
        "array_type" => {
            let one_dimensional = ty
                .child_by_field_name("rank")
                .is_some_and(|rank| !node_text(rank, source).contains(','));
            let element_may_be_string =
                ty.child_by_field_name("type")
                    .is_some_and(|element| match element.kind() {
                        "predefined_type" => node_text(element, source) == "string",
                        "array_type" => false,
                        _ => true,
                    });
            one_dimensional && element_may_be_string
        }
        _ => true,
    }
}

fn csharp_type_declares_main(type_node: Node, source: &[u8]) -> bool {
    let Some(body) = type_node.child_by_field_name("body") else {
        return false;
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .any(|member| match member.kind() {
            "method_declaration" => is_csharp_main(member, source),
            kind if CSHARP_TYPE_KINDS.contains(&kind) => csharp_type_declares_main(member, source),
            _ => false,
        })
}

// --- Kotlin ---

/// Kotlin の入口: トップレベルの `fun main`、または `object` / `companion object` 直下の
/// `@JvmStatic fun main` (JVM の static main になる)。引数は 0〜1 個 (`Array<String>` /
/// `vararg args: String`。型の別名がありうるので型そのものは見ない)。
fn is_kotlin_main(function: Node, source: &[u8]) -> bool {
    if kotlin_function_name(function, source) != Some("main")
        || kotlin_parameter_count(function) > 1
    {
        return false;
    }
    if is_top_level(function) {
        return true;
    }
    let in_object = function
        .parent()
        .filter(|body| KOTLIN_BODY_KINDS.contains(&body.kind()))
        .and_then(|body| body.parent())
        .is_some_and(|owner| KOTLIN_OBJECT_KINDS.contains(&owner.kind()));
    in_object && kotlin_has_annotation(function, "JvmStatic", source)
}

fn kotlin_function_name<'a>(function: Node, source: &'a [u8]) -> Option<&'a str> {
    let mut cursor = function.walk();
    function
        .named_children(&mut cursor)
        .find(|child| child.kind() == "simple_identifier")
        .map(|name| node_text(name, source))
}

fn kotlin_parameter_count(function: Node) -> usize {
    let mut cursor = function.walk();
    let Some(parameters) = function
        .named_children(&mut cursor)
        .find(|child| child.kind() == "function_value_parameters")
    else {
        return 0;
    };
    let mut inner = parameters.walk();
    parameters
        .named_children(&mut inner)
        .filter(|child| child.kind() == "parameter")
        .count()
}

/// `@JvmStatic` / `@kotlin.jvm.JvmStatic` のような annotation を持つか。
fn kotlin_has_annotation(declaration: Node, name: &str, source: &[u8]) -> bool {
    let mut cursor = declaration.walk();
    declaration
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "modifiers")
        .any(|modifiers| {
            let mut inner = modifiers.walk();
            modifiers
                .named_children(&mut inner)
                .filter(|child| child.kind() == "annotation")
                .any(|annotation| {
                    let text = node_text(annotation, source).trim_start_matches('@').trim();
                    text.rsplit('.').next() == Some(name)
                })
        })
}

fn kotlin_type_declares_main(type_node: Node, source: &[u8]) -> bool {
    let mut cursor = type_node.walk();
    let Some(body) = type_node
        .named_children(&mut cursor)
        .find(|child| KOTLIN_BODY_KINDS.contains(&child.kind()))
    else {
        return false;
    };
    let mut inner = body.walk();
    body.named_children(&mut inner)
        .any(|member| match member.kind() {
            "function_declaration" => is_kotlin_main(member, source),
            "companion_object" => kotlin_type_declares_main(member, source),
            kind if KOTLIN_TYPE_KINDS.contains(&kind) => kotlin_type_declares_main(member, source),
            _ => false,
        })
}

// --- 共通ヘルパー ---

/// ファイル直下 (親がルートノード) の宣言か。
fn is_top_level(node: Node) -> bool {
    node.parent()
        .is_some_and(|parent| parent.parent().is_none())
}

fn has_ancestor(node: Node, kinds: &[&str]) -> bool {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if kinds.contains(&ancestor.kind()) {
            return true;
        }
        current = ancestor.parent();
    }
    false
}

fn field_text<'a>(node: Node, field: &str, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name(field)
        .map(|child| node_text(child, source))
}

fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

fn bracket_count(node: Node, source: &[u8]) -> usize {
    node_text(node, source).matches('[').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 各言語の判定が依存するノード名。`function_kinds` / `owner_type_kinds` と同じく
    /// `LangId` を catch-all なしで全列挙し、文法に実在することを検証する
    /// (改称や他言語からの流用は「1 つもマッチしない」形で静かに壊れるため)。
    fn structural_kinds(lang_id: LangId) -> &'static [&'static str] {
        match lang_id {
            LangId::C => &[
                "function_definition",
                "function_declarator",
                "pointer_declarator",
                "parenthesized_declarator",
                "attributed_declarator",
                "identifier",
                "struct_specifier",
                "union_specifier",
                "compound_statement",
            ],
            LangId::Cpp => &[
                "function_definition",
                "function_declarator",
                "pointer_declarator",
                "parenthesized_declarator",
                "attributed_declarator",
                "reference_declarator",
                "identifier",
                "namespace_definition",
                "class_specifier",
                "struct_specifier",
                "union_specifier",
                "compound_statement",
                "template_declaration",
            ],
            LangId::Java => &[
                "method_declaration",
                "void_type",
                "modifiers",
                "formal_parameter",
                "spread_parameter",
                "variable_declarator",
                "array_type",
                "type_identifier",
                "scoped_type_identifier",
                "annotated_type",
                "enum_body_declarations",
            ],
            LangId::CSharp => &[
                "method_declaration",
                "modifier",
                "parameter",
                "array_type",
                "predefined_type",
            ],
            LangId::Kotlin => &[
                "function_declaration",
                "simple_identifier",
                "function_value_parameters",
                "parameter",
                "modifiers",
                "annotation",
                "companion_object",
                "class_body",
                "enum_class_body",
            ],
            LangId::Swift
            | LangId::Zig
            | LangId::Rust
            | LangId::Go
            | LangId::Python
            | LangId::Javascript
            | LangId::Typescript
            | LangId::Tsx
            | LangId::Php
            | LangId::Ruby
            | LangId::Bash
            | LangId::Xojo => &[],
        }
    }

    fn assert_node_kinds_exist(lang_id: LangId, table: &str, kinds: &[&str]) {
        let language = lang_id.ts_language();
        for kind in kinds {
            let pattern = format!("({kind}) @probe");
            assert!(
                tree_sitter::Query::new(&language, &pattern).is_ok(),
                "{lang_id} の {table} に実在しないノード名 \"{kind}\" がある"
            );
        }
    }

    #[test]
    fn entrypoint_node_kinds_exist_in_every_grammar() {
        for &lang_id in LangId::ALL_TREE_SITTER {
            assert_node_kinds_exist(lang_id, "function_kinds", function_kinds(lang_id));
            assert_node_kinds_exist(lang_id, "owner_type_kinds", owner_type_kinds(lang_id));
            assert_node_kinds_exist(lang_id, "structural_kinds", structural_kinds(lang_id));
        }
        // 所有型の探索で使う Kotlin の object 系ノードも実在すること。
        assert_node_kinds_exist(LangId::Kotlin, "KOTLIN_OBJECT_KINDS", KOTLIN_OBJECT_KINDS);
        // 匿名トークンはクエリの文字列パターンで確かめる。
        for (lang_id, token) in [(LangId::Java, "private"), (LangId::CSharp, "params")] {
            let language = lang_id.ts_language();
            let pattern = format!("\"{token}\" @probe");
            assert!(
                tree_sitter::Query::new(&language, &pattern).is_ok(),
                "{lang_id} に匿名トークン \"{token}\" が無い"
            );
        }
    }

    /// `source` のシンボルのうち入口と判定されたものを `(名前, 宣言の開始行)` で返す。
    /// qualname の container 付与規則に依存しないよう、ネストの区別は行番号で行う。
    fn entrypoints(lang_id: LangId, source: &str) -> Vec<(String, usize)> {
        let tree = crate::engine::parser::parse_source(source.as_bytes(), lang_id).unwrap();
        let root = tree.root_node();
        let syms = super::super::extract_symbols(root, source.as_bytes(), lang_id).unwrap();
        syms.iter()
            .filter(|sym| {
                is_program_entrypoint(root, source.as_bytes(), lang_id, sym.kind, &sym.range)
            })
            .map(|sym| (sym.name.clone(), sym.range.start.line))
            .collect()
    }

    /// 入口判定の期待値を (名前, 行) の集合で比較する (順序は問わない)。
    fn assert_entrypoints(lang_id: LangId, source: &str, expected: &[(&str, usize)]) {
        let mut found = entrypoints(lang_id, source);
        found.sort();
        let mut expected: Vec<(String, usize)> = expected
            .iter()
            .map(|(name, line)| ((*name).to_string(), *line))
            .collect();
        expected.sort();
        assert_eq!(found, expected, "{lang_id}");
    }

    #[test]
    fn c_and_cpp_main_is_global_free_function_only() {
        assert_entrypoints(
            LangId::C,
            "#include <stdio.h>\nint main(int argc, char **argv) { return 0; }\n",
            &[("main", 1)],
        );
        // 対照: 名前空間・クラス内・クラス外定義の `main` は普通の関数。
        // プリプロセッサ条件の中でもグローバルスコープなら入口。
        let cpp = r#"
namespace app {
int main() { return 1; }
}
class Foo {
public:
    int main() { return 2; }
};
int Bar::main() { return 3; }
#ifdef TEST
int main(int argc, char **argv) { return 0; }
#endif
"#;
        assert_entrypoints(LangId::Cpp, cpp, &[("main", 10)]);
    }

    #[test]
    fn java_main_and_owner_types() {
        let src = r#"
public class App {
    public static void main(String[] args) {}
}
class Varargs { public static void main(final String... args) {} }
class CStyle { static void main(String args[]) {} }
class Qualified { public static void main(java.lang.String[] args) {} }
class Instance { void main() {} }
class Outer { static class Inner { public static void main(String[] a) {} } }
enum Mode { A; public static void main(String[] args) {} }
class Holder { enum Kind { A } interface Marker {} public static void main(String[] a) {} }
class Priv { private static void main(String[] args) {} }
class WrongParam { public static void main(int x) {} }
class WrongReturn { public static int main(String[] args) { return 0; } }
class TwoDim { public static void main(String[][] args) {} }
class TwoParams { public static void main(String[] a, String b) {} }
class Plain { public static void run(String[] args) {} }
class Sibling {}
"#;
        // 対照 (出てはならないもの): private / 引数や戻り値の形が違う / 名前が違う main、
        // それらの所有型、入口を持たない兄弟の型、入口を持つ型の中の別の型宣言。
        assert_entrypoints(
            LangId::Java,
            src,
            &[
                ("App", 1),
                ("main", 2),
                ("Varargs", 4),
                ("main", 4),
                ("CStyle", 5),
                ("main", 5),
                ("Qualified", 6),
                ("main", 6),
                ("Instance", 7),
                ("main", 7),
                ("Outer", 8),
                ("Inner", 8),
                ("main", 8),
                ("Mode", 9),
                ("main", 9),
                ("Holder", 10),
                ("main", 10),
            ],
        );
    }

    #[test]
    fn csharp_static_main_and_owner_types() {
        let src = r#"
class Program { static void Main(string[] args) {} }
class AsyncProgram { public static async Task<int> Main() { return 0; } }
class ParamsProgram { static void Main(params string[] args) {} }
class Qualified { static void Main(System.String[] args) {} }
struct StructProgram { static void Main() {} }
class Outer { class Inner { static void Main() {} } }
class Instance { void Main() {} }
class WrongParam { static void Main(int x) {} }
class TwoParams { static void Main(string[] a, string b) {} }
class Jagged { static void Main(string[][] a) {} }
class Plain { static void Run(string[] args) {} }
class Host {
    enum Color { Red }
    static void Main() {}
}
"#;
        // 対照: instance / 引数の形が違う / 名前が違う Main と、その所有型は出ない。
        // 入口を持つ型の中の enum (所有型の候補にならない宣言) も外側の判定を借りない。
        assert_entrypoints(
            LangId::CSharp,
            src,
            &[
                ("Program", 1),
                ("Main", 1),
                ("AsyncProgram", 2),
                ("Main", 2),
                ("ParamsProgram", 3),
                ("Main", 3),
                ("Qualified", 4),
                ("Main", 4),
                ("StructProgram", 5),
                ("Main", 5),
                ("Outer", 6),
                ("Inner", 6),
                ("Main", 6),
                ("Host", 12),
                ("Main", 14),
            ],
        );
    }

    #[test]
    fn kotlin_top_level_and_jvm_static_main() {
        let src = r#"
fun main(args: Array<String>) {}
object App {
    @JvmStatic
    fun main(args: Array<String>) {}
}
class Host {
    companion object {
        @kotlin.jvm.JvmStatic fun main() {}
    }
}
object NoJvmStatic {
    fun main(args: Array<String>) {}
}
class NotObject {
    @JvmStatic fun main() {}
}
fun main(a: Int, b: Int) {}
"#;
        // 対照: @JvmStatic の無い object メンバー、object 以外の @JvmStatic、
        // 引数 2 個のトップレベル main は入口ではない。
        assert_entrypoints(
            LangId::Kotlin,
            src,
            &[
                ("main", 1),
                ("App", 2),
                ("main", 3),
                ("Host", 6),
                ("main", 8),
            ],
        );
    }

    #[test]
    fn languages_without_entrypoint_functions_report_nothing() {
        assert_entrypoints(LangId::Go, "package main\n\nfunc Main() {}\n", &[]);
        assert_entrypoints(LangId::Python, "def main():\n    pass\n", &[]);
        assert_entrypoints(LangId::Typescript, "export function main() {}\n", &[]);
    }
}
