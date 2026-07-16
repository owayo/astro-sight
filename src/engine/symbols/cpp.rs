use tree_sitter::Node;

use crate::language::LangId;
use crate::models::location::Range;

use super::node_for_symbol_range;

/// Rust のシンボルが trait impl ブロックに属しているかを判定する。
/// trait impl メソッドは trait dispatch 経由で呼ばれるため、cross-file refs
/// 検索では caller を追跡できず、dead-code 判定でスキップする必要がある。
pub(crate) fn is_trait_impl_method_rust(root: Node, symbol_range: &Range) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };

    let mut parent = node.parent();
    while let Some(p) = parent {
        if p.kind() == "impl_item" {
            return p.child_by_field_name("trait").is_some();
        }
        parent = p.parent();
    }
    false
}

/// C/C++ で、実関数の本体ブロック (compound_statement) の内側にネストして出現する
/// function_definition かどうかを判定する。
///
/// tree-sitter-cpp はマクロ呼び出し `BOOST_FOREACH(...) { ... }` 等を function_definition
/// として誤パースすることがあり、それらは実関数 body の内側に偽の関数定義として現れる。
/// トップレベル / namespace / class・struct 直下の本物の定義は、祖先が translation_unit /
/// declaration_list / field_declaration_list であって compound_statement ではないため
/// false を返す。GNU C の nested function を見逃す程度の false negative は許容する。
pub(crate) fn is_cpp_nested_function(root: Node, symbol_range: &Range) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };
    let mut parent = node.parent();
    while let Some(p) = parent {
        if p.kind() == "compound_statement" {
            return true;
        }
        parent = p.parent();
    }
    false
}

/// C/C++ の struct/class/union/enum が本体 (フィールド/列挙子リスト) を持たない前方宣言かを
/// 判定する。
///
/// `struct st_mysql;` や `typedef struct st_mysql MYSQL;` の `st_mysql`、`enum E : int;` の
/// ような opaque タグ / 前方宣言は「定義」ではなく宣言であり、dead-code (未使用定義の検出) や
/// API 変更の対象にすべきではない。本体 (field_declaration_list / enumerator_list) を持つ
/// 定義のみを残すために使う (Issue #11: typedef underlying tag の dead 誤検出対策)。
pub(crate) fn is_cpp_forward_declaration(root: Node, symbol_range: &Range) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };
    // symbol_range は specifier (struct_specifier 等) を指すこともあれば、name identifier を
    // 指すこともある。specifier ノードを特定する。
    let specifier = if is_cpp_type_specifier_kind(node.kind()) {
        Some(node)
    } else {
        node.parent()
            .filter(|p| is_cpp_type_specifier_kind(p.kind()))
    };
    match specifier {
        Some(s) => !cpp_specifier_has_body(s),
        None => false,
    }
}

fn is_cpp_type_specifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "struct_specifier" | "class_specifier" | "union_specifier" | "enum_specifier"
    )
}

/// C/C++ の型 specifier が本体 (field_declaration_list / enumerator_list) を持つか判定する。
fn cpp_specifier_has_body(specifier: Node) -> bool {
    let mut cursor = specifier.walk();
    for child in specifier.children(&mut cursor) {
        if matches!(child.kind(), "field_declaration_list" | "enumerator_list") {
            return true;
        }
    }
    false
}

/// C/C++ の dead-code liveness 補助情報を集める。
///
/// `(シンボル名, 追加 liveness 名のリスト)` を返す。dead 候補の生存判定で、元のシンボル名に
/// 加えてこれらの追加名のいずれかが参照されていれば live とみなすために使う (Issue #11/#12)。
///
/// - enum 型: enum 名 → 列挙子名のリスト (列挙子のいずれかが参照されていれば enum は live)
/// - typedef struct/union/enum: underlying tag 名 → alias 名 (alias が参照されていれば tag は live)
pub(crate) fn collect_cpp_dead_liveness_aliases(
    root: Node<'_>,
    source: &[u8],
    lang_id: LangId,
) -> Vec<(String, Vec<String>)> {
    if !matches!(lang_id, LangId::C | LangId::Cpp) {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "enum_specifier" => {
                if let Some(name_node) = node.child_by_field_name("name")
                    && let Ok(enum_name) = name_node.utf8_text(source)
                {
                    let enumerators = collect_cpp_enumerator_names(node, source);
                    if !enumerators.is_empty() {
                        result.push((enum_name.to_string(), enumerators));
                    }
                }
            }
            "type_definition" => {
                if let Some(pair) = extract_cpp_typedef_tag_aliases(node, source) {
                    result.push(pair);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    result
}

/// enum_specifier の body から列挙子名を集める。
fn collect_cpp_enumerator_names(enum_specifier: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let Some(body) = enum_specifier.child_by_field_name("body") else {
        return names;
    };
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "enumerator"
            && let Some(name_node) = child.child_by_field_name("name")
            && let Ok(n) = name_node.utf8_text(source)
        {
            names.push(n.to_string());
        }
    }
    names
}

/// type_definition から `(underlying tag 名, alias 名リスト)` を抽出する。
/// type フィールドが struct/union/enum specifier で name (tag) を持つ場合のみ Some。
fn extract_cpp_typedef_tag_aliases(
    type_definition: Node<'_>,
    source: &[u8],
) -> Option<(String, Vec<String>)> {
    let type_node = type_definition.child_by_field_name("type")?;
    if !matches!(
        type_node.kind(),
        "struct_specifier" | "union_specifier" | "enum_specifier"
    ) {
        return None;
    }
    let tag_name = type_node
        .child_by_field_name("name")?
        .utf8_text(source)
        .ok()?
        .to_string();
    // 複数 declarator (`typedef struct S {} A, *B;`) の全 alias 名を集める (codex 指摘 2)。
    let mut aliases = Vec::new();
    let mut cursor = type_definition.walk();
    for decl in type_definition.children_by_field_name("declarator", &mut cursor) {
        if let Some(alias) = cpp_declarator_name(decl, source) {
            aliases.push(alias);
        }
    }
    if aliases.is_empty() {
        return None;
    }
    Some((tag_name, aliases))
}

/// typedef の declarator から alias 名を取り出す (pointer/array declarator を剥がす)。
fn cpp_declarator_name(decl: Node<'_>, source: &[u8]) -> Option<String> {
    match decl.kind() {
        "type_identifier" | "identifier" => decl.utf8_text(source).ok().map(|s| s.to_string()),
        _ => decl
            .child_by_field_name("declarator")
            .and_then(|d| cpp_declarator_name(d, source)),
    }
}

/// C/C++ の関数名ノードから、本体を持つ `function_definition` まで繰り上げる。
/// pointer_declarator / reference_declarator / qualified_identifier を経由する。
/// 宣言 (`declaration` / `field_declaration` / `parameter_declaration`) に先に
/// 到達した場合は本体を持たない宣言なので `None` を返す (シンボル非採用)。
pub(super) fn cpp_enclosing_function_definition(name_node: Node<'_>) -> Option<Node<'_>> {
    let mut n = name_node;
    while let Some(p) = n.parent() {
        match p.kind() {
            "function_definition" => return Some(p),
            "declaration" | "field_declaration" | "parameter_declaration" => return None,
            _ => {}
        }
        n = p;
    }
    None
}

/// C++: メソッドの可視性を `access_specifier` から判定する。
/// クラス/構造体の外 (namespace / global スコープの自由関数・クラス外定義) は公開扱い。
/// `class` のデフォルトは private、`struct` のデフォルトは public。
/// 直近の `public:` / `protected:` 配下は公開、`private:` 配下は非公開。
pub(super) fn is_exported_cpp(node: Node<'_>, source: &[u8]) -> bool {
    // メソッド定義 (function_definition) を起点にする
    let mut method = node;
    while method.kind() != "function_definition" {
        match method.parent() {
            Some(p) => method = p,
            None => return true, // 関数定義が見つからない → 保守的に公開
        }
    }
    // 直近の囲い specifier (class/struct) を探す。access_specifier は
    // field_declaration_list の直接子なので、走査中に「parent が
    // field_declaration_list である祖先」(直接メンバなら method 自身、template
    // メンバなら template_declaration) を member_anchor として保持する。
    // method 自身から prev_sibling を辿ると template メンバで access_specifier に
    // 届かず class デフォルト private に誤判定する。
    let mut cur = method;
    let mut member_anchor = method;
    let default_public = loop {
        match cur.parent() {
            Some(p) => match p.kind() {
                "class_specifier" => break false, // class: default private
                "struct_specifier" => break true, // struct: default public
                _ => {
                    if p.kind() == "field_declaration_list" {
                        member_anchor = cur;
                    }
                    cur = p;
                }
            },
            None => return true, // クラス外 (自由関数・クラス外定義) は公開
        }
    };
    // member_anchor の直前の兄弟を遡って直近の access_specifier を探す
    let mut sibling = member_anchor.prev_sibling();
    while let Some(s) = sibling {
        if s.kind() == "access_specifier" {
            let txt = s.utf8_text(source).unwrap_or("");
            return txt.starts_with("public") || txt.starts_with("protected");
        }
        sibling = s.prev_sibling();
    }
    default_public
}
