//! PHP の参照解析。
//!
//! 定義コンテキスト判定に加え、PHP 固有の case-insensitivity 規則 (関数/メソッド/
//! クラス系の名前だけを大小無視で照合する) と、identifier ノードとして現れない
//! 文字列由来の参照源 (`[Foo::class, 'method']` / `'Class@method'`) の抽出を持つ。

use tree_sitter::Node;

use crate::language::LangId;

/// PHP: 識別子が「宣言の `name` フィールド」であるときだけ `Definition` とみなす。
///
/// 単純な parent/grandparent 走査では `class Derived extends AbstractBase` の
/// `AbstractBase` や `implements InterfaceX` の `InterfaceX` が grandparent
/// `class_declaration` にぶら下がって def と誤判定され、継承ツリーを経由した
/// 参照がすべて 0 件になる (dead-code が基底 class / interface を大量に FP とする根因)。
/// field_name が "name" のものだけを定義と数え、`base_clause` / `class_interface_clause` /
/// `use_declaration` 等の中の識別子は ref として分類する。
pub(crate) fn is_php_definition_context(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "function_definition"
        | "class_declaration"
        | "method_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "trait_declaration" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        _ => false,
    }
}

/// PHP の `name` ノードが case-insensitive 照合すべき文脈にあるか判定する。
///
/// PHP は関数名・メソッド名・クラス/interface/trait/enum 名が case-insensitive だが、
/// 変数・プロパティ・定数は case-sensitive。誤って定数・プロパティ・変数を case-fold
/// しないよう、ホワイトリスト方式で「関数/メソッド/クラス系の名前」だけ true を返す。
pub(crate) fn php_name_is_case_insensitive(node: Node<'_>) -> bool {
    // 定義側 (method/function/class/interface/trait/enum の name フィールド)
    if is_php_definition_context(node) {
        return true;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    // qualified_name (`\App\Foo` 等) の末尾 name は、namespace prefix (namespace_name 配下)
    // を除き qualified_name 全体を実効ノードとして、その親文脈で判定する。
    // namespace_name 配下の name (App / Repository) は namespace セグメントなので _ に落ちて exact。
    if parent.kind() == "qualified_name" {
        return match parent.parent() {
            Some(grand) => php_ref_context_is_case_insensitive(grand, parent),
            None => false,
        };
    }
    php_ref_context_is_case_insensitive(parent, node)
}

/// 参照ノード `effective` (name または qualified_name) が親 `parent` の文脈で
/// case-insensitive 照合対象 (関数/メソッド/クラス系の名前) かを判定する。
fn php_ref_context_is_case_insensitive(parent: Node<'_>, effective: Node<'_>) -> bool {
    let is_field = |field: &str| {
        parent
            .child_by_field_name(field)
            .is_some_and(|n| n.id() == effective.id())
    };
    // scope 位置判定: tree-sitter-php は scoped_call_expression には field "scope" を付けるが、
    // class_constant_access_expression / scoped_property_access_expression は positional
    // (field 名なし) のため、field を優先しつつ named_child(0) に fallback する。
    let is_scope = || {
        parent.child_by_field_name("scope").map_or_else(
            || {
                parent
                    .named_child(0)
                    .is_some_and(|s| s.id() == effective.id())
            },
            |s| s.id() == effective.id(),
        )
    };
    match parent.kind() {
        // $x->method() のメソッド名。member_access_expression ($x->prop) は _ に落ちて exact。
        "member_call_expression" => is_field("name"),
        // func() のグローバル/名前空間関数名
        "function_call_expression" => is_field("function"),
        // Foo::method() — scope(クラス名) も name(メソッド名) も case-fold
        // (self/static/parent は relative_scope ノードのため scope は名前ノードにならない)
        "scoped_call_expression" => is_scope() || is_field("name"),
        // Foo::CONST — scope(クラス名)は case-fold、定数 name は exact。
        // ただし trait adaptation (`A::foo insteadof B` / `A::foo as bar`) 配下の name は
        // trait メソッド名なので case-fold する。
        "class_constant_access_expression" => {
            if is_scope() {
                true
            } else {
                matches!(
                    parent.parent().map(|g| g.kind()),
                    Some("use_instead_of_clause") | Some("use_as_clause")
                )
            }
        }
        // Foo::$prop — scope(クラス名)のみ case-fold。静的プロパティ ($prop) は variable_name で別ノード。
        "scoped_property_access_expression" => is_scope(),
        // new Foo() / extends Foo / implements Iface / trait use の直接子はクラス系名。
        // 引数等の name は parent が arguments 等になるため誤巻き込みしない。
        "object_creation_expression"
        | "base_clause"
        | "class_interface_clause"
        | "use_declaration" => true,
        // 名前空間 use: クラス / 関数 import は case-insensitive、定数 import (`use const`) は
        // PHP 定数が case-sensitive なため exact のままにする。
        "namespace_use_clause" => !php_namespace_use_is_const(parent),
        // 型ヒント `Foo $x` の named_type 内 (variable_name 内の name は _ に落ちて exact)
        "named_type" => true,
        // trait adaptation の trait 名 / 別名 (`use A, B { B::foo insteadof A; A::bar as baz; }`)
        "use_instead_of_clause" | "use_as_clause" => true,
        _ => false,
    }
}

/// PHP の名前空間 use 文が `use const ...` (定数 import) かを判定する。
///
/// 定数は case-sensitive のため case-fold しない。`use` (クラス import) / `use function`
/// (関数 import) は case-insensitive。group use (`use App\{const FOO, Bar, function baz}`) の
/// 個別修飾子と、`use const App\{...}` のグループ全体修飾子の両方に対応する。
/// `const` / `function` は anonymous keyword node として現れる。
fn php_namespace_use_is_const(clause: Node<'_>) -> bool {
    // group use の各 clause は自身の先頭に const / function キーワードを持つ場合がある。
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        match child.kind() {
            "const" => return true,
            "function" => return false,
            _ => {}
        }
    }
    // 単純 use / group 全体の修飾子は namespace_use_declaration 直下にある。
    let mut current = clause.parent();
    while let Some(n) = current {
        if n.kind() == "namespace_use_declaration" {
            let mut decl_cursor = n.walk();
            for child in n.children(&mut decl_cursor) {
                match child.kind() {
                    "const" => return true,
                    "function" => return false,
                    // clause / group 本体に到達したら宣言レベルの修飾子はない
                    "namespace_use_clause" | "namespace_use_group" => break,
                    _ => {}
                }
            }
            return false;
        }
        current = n.parent();
    }
    false
}

/// PHP の callable array `[<Class>::class, '<method>']` パターンから
/// `<method>` の文字列を method reference として返す (N3)。
///
/// Laravel 7+ 推奨の Route 記法 `Route::get('/path', [Foo::class, 'bar'])` や
/// `[Foo::class, 'method']` で `'method'` 部分が string literal となるため、
/// tree-sitter の identifier ノードでは捕捉できない。誤検出を避けるため、
/// 第1要素が `Foo::class` (= `class_constant_access_expression` の右辺が
/// `class` キーワード) であり、第2要素が単独の string literal で
/// 中身が PHP 識別子文法に合致する場合のみ ref として認める。
pub(crate) fn php_callable_array_method_segment<'a>(
    node: Node<'_>,
    source: &'a [u8],
    lang_id: LangId,
) -> Option<(&'a str, usize, usize)> {
    if lang_id != LangId::Php || node.kind() != "array_creation_expression" {
        return None;
    }
    let mut cursor = node.walk();
    let elements: Vec<Node> = node
        .children(&mut cursor)
        .filter(|c| c.kind() == "array_element_initializer")
        .collect();
    if elements.len() != 2 {
        return None;
    }

    // 第1要素: class_constant_access_expression で右辺が `class` キーワード
    let first = elements[0];
    let mut fc = first.walk();
    let first_inner = first.children(&mut fc).next()?;
    if first_inner.kind() != "class_constant_access_expression" {
        return None;
    }
    let mut cc = first_inner.walk();
    let has_class_kw = first_inner
        .children(&mut cc)
        .any(|c| c.kind() == "name" && c.utf8_text(source) == Ok("class"));
    if !has_class_kw {
        return None;
    }

    // 第2要素: string / encapsed_string literal
    let second = elements[1];
    let mut sc = second.walk();
    let str_node = second
        .children(&mut sc)
        .find(|c| c.kind() == "string" || c.kind() == "encapsed_string")?;
    let raw = str_node.utf8_text(source).ok()?;
    let trimmed = raw.trim_matches(|c: char| c == '\'' || c == '"');
    if !is_php_identifier(trimmed) {
        return None;
    }
    let pos = str_node.start_position();
    // 引用符の次の文字を method 名の開始位置として登録する
    Some((trimmed, pos.row, pos.column.saturating_add(1)))
}

/// PHP の識別子文法 `[A-Za-z_][A-Za-z0-9_]*` に合致するかを判定する。
fn is_php_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// PHP string literal が Laravel 互換の callable 表記 `Class@method` / `@method`
/// (concat 連結の右辺) を含んでいれば、`method` 部分を ref として返す (N4)。
///
/// 対象構文:
/// 1. 純粋文字列 `'ClassName@handler'` / `'\\Fully\\Qualified\\Name@handler'`
/// 2. 連結 `ClassName::class . '@handler'` の右辺 string (class_part が空)
///
/// 誤検出対策:
/// - method 部分は PHP 識別子 (2 文字以上、英小文字または `_` で始まる)
/// - class 部分が非空の場合、名前空間 `\\` 区切りで各セグメントが 2 文字以上 + 先頭大文字
///   (英小文字始まりの場合はメール/単語の可能性があるため reject)
/// - class 部分が空の場合、親が `binary_expression` (`.` 演算子) で左辺が `X::class`
///   (`class_constant_access_expression`) の場合のみ認める
/// - double-quoted (encapsed_string) は補間で構造が崩れるため対象外
pub(crate) fn php_string_callable_method_segment<'a>(
    node: Node<'_>,
    source: &'a [u8],
    lang_id: LangId,
) -> Option<(&'a str, usize, usize)> {
    if lang_id != LangId::Php || node.kind() != "string" {
        return None;
    }
    let raw = node.utf8_text(source).ok()?;
    if raw.len() < 2 {
        return None;
    }
    let bytes = raw.as_bytes();
    let first = bytes[0];
    let last = bytes[raw.len() - 1];
    if (first != b'\'' && first != b'"') || first != last {
        return None;
    }
    let body = &raw[1..raw.len() - 1];

    let at_pos = body.find('@')?;
    let class_part = &body[..at_pos];
    let method_part = &body[at_pos + 1..];

    if !is_php_method_name(method_part) {
        return None;
    }
    let class_ok = if class_part.is_empty() {
        is_parent_class_const_concat(node, source)
    } else {
        is_php_class_path_strict(class_part)
    };
    if !class_ok {
        return None;
    }

    let start = node.start_position();
    // quote 1 byte + class_part bytes + '@' 1 byte。column は tree-sitter の仕様上
    // byte offset 相当なので、method 先頭の byte 位置として足し合わせる。
    let byte_offset = 1 + class_part.len() + 1;
    Some((
        method_part,
        start.row,
        start.column.saturating_add(byte_offset),
    ))
}

/// N4 method 部分用: PHP 識別子 かつ 英小文字/`_` で始まる、かつ 2 文字以上。
/// `'P@ssw0rd'` (class_part='P', method_part='ssw0rd') を弾くため method 側は厳しめにしない
/// 代わりに class_part 側で 1 文字を reject する。ここは英識別子であれば広めに許容する。
fn is_php_method_name(s: &str) -> bool {
    if s.len() < 2 {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// N4 class 部分用: 名前空間 `\\` 区切りで各セグメントが先頭大文字 + 2 文字以上 + 識別子。
/// 先頭 `\\` の absolute namespace プレフィクスも許容する。
fn is_php_class_path_strict(s: &str) -> bool {
    let s = s.strip_prefix('\\').unwrap_or(s);
    if s.is_empty() {
        return false;
    }
    for part in s.split('\\') {
        if part.len() < 2 {
            return false;
        }
        let mut chars = part.chars();
        let first = chars.next().unwrap();
        if !first.is_ascii_uppercase() {
            return false;
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    true
}

/// N4 parent check: `node` が `X::class . node` 形式の concat 右辺であれば true。
fn is_parent_class_const_concat(node: Node<'_>, source: &[u8]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "binary_expression" {
        return false;
    }
    // operator field: tree-sitter-php では binary_expression の `operator` は子ノード
    // として現れる。field 名で取れなくても子 token で `.` を探す。
    let mut cursor = parent.walk();
    let op_is_dot = parent.children(&mut cursor).any(|c| {
        // operator トークンは kind = "." になる (tree-sitter-php)
        c.kind() == "." && c.utf8_text(source) == Ok(".")
    });
    if !op_is_dot {
        return false;
    }
    // node が parent の右側にいるか確認: 親の children で node より前に
    // class_constant_access_expression が存在することを検証する。
    let mut cur2 = parent.walk();
    let mut seen_class_const = false;
    let mut node_is_right = false;
    for c in parent.children(&mut cur2) {
        if c.id() == node.id() {
            node_is_right = seen_class_const;
            break;
        }
        if c.kind() == "class_constant_access_expression" && is_class_class_expr(c, source) {
            seen_class_const = true;
        }
    }
    node_is_right
}

/// `X::class` 形式の class_constant_access_expression かを判定。
fn is_class_class_expr(node: Node<'_>, source: &[u8]) -> bool {
    if node.kind() != "class_constant_access_expression" {
        return false;
    }
    let mut c = node.walk();
    node.children(&mut c)
        .any(|child| child.kind() == "name" && child.utf8_text(source) == Ok("class"))
}
