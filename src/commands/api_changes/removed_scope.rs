//! 削除した自由関数と無関係な、座標ごとの定義・値束縛を証明する。

use std::collections::HashSet;
use tree_sitter::Node;

use crate::engine::lexical_binding::LexicalBinding;
use crate::engine::python_scope::PythonScopeIndex;

use crate::engine::refs::is_rust_struct_field_non_callable;

pub(super) type Positions = HashSet<(usize, usize)>;

fn position(node: Node<'_>) -> (usize, usize) {
    let p = node.start_position();
    (p.row, p.column)
}

/// 再帰せず、同一解析木を一度だけ列挙する。
fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    loop {
        out.push(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return out;
            }
        }
    }
}

/// Python は一意な通常のモジュール直下 def と、確定した読み取り位置だけを証明する。
/// 未知の束縛、動的名前解決、構文エラーがあればファイル単位で証明を諦める。
pub(super) fn python_independent_positions(root: Node<'_>, source: &[u8], name: &str) -> Positions {
    if root.has_error() {
        return Positions::new();
    }
    let all = nodes(root);
    if !python_names_are_static(&all, source, name) {
        return Positions::new();
    }
    let definitions: Vec<_> = all
        .iter()
        .copied()
        .filter(|n| {
            n.kind() == "function_definition"
                && n.parent().is_some_and(|p| p.id() == root.id())
                && n.child(0).is_none_or(|c| c.kind() != "async")
                && n.child_by_field_name("name")
                    .and_then(|c| c.utf8_text(source).ok())
                    == Some(name)
        })
        .collect();
    if definitions.len() != 1 {
        return Positions::new();
    }
    let Some(def) = definitions[0].child_by_field_name("name") else {
        return Positions::new();
    };
    let mut out = Positions::new();
    for node in all
        .into_iter()
        .filter(|n| n.kind() == "identifier" && n.utf8_text(source).ok() == Some(name))
    {
        if node.id() == def.id() {
            out.insert(position(node));
            continue;
        }
        let Some(parent) = node.parent() else {
            return Positions::new();
        };
        if parent.kind() == "attribute"
            && parent
                .child_by_field_name("attribute")
                .is_some_and(|n| n.id() == node.id())
        {
            continue;
        }
        if !python_is_load(node) {
            return Positions::new();
        }
        out.insert(position(node));
    }
    out
}

/// Python の関数の中で、`global`・`nonlocal` の宣言なしに束縛した名前の出現 (関数のローカル
/// 束縛に解決する位置)。関数の中の代入・引数・`for` の対象・`with ... as`・`except ... as` で
/// 束縛した名前は、その関数 (と、名前を束縛し直さない内側の関数) の中ではモジュールの関数を
/// 指さない。束縛を確かめられない構文 (lambda・内包表記・関数の中の import・`match` など) は
/// refs と同じ [`PythonScopeIndex`] が証明しない。
///
/// 名前でコードや属性を引く経路 (`globals()["x"]`・`keep.__globals__["x"]`・
/// `operator.methodcaller("x")` など) と、名前を値に含む文字列 (`typing.get_type_hints` が
/// モジュールで評価する文字列の注釈、`__annotations__` に入れた文字列を含む) は、ローカルの束縛を
/// 通さずにモジュールの関数へ届くので、あればファイルごと証明しない。
pub(super) fn python_local_positions(root: Node<'_>, source: &[u8], name: &str) -> Positions {
    if root.has_error() {
        return Positions::new();
    }
    let all = nodes(root);
    if !python_names_are_static(&all, source, name)
        || python_mentions_reflective_name(&all, source)
        || python_strings_mention(&all, source, name)
    {
        return Positions::new();
    }
    let Some(index) = PythonScopeIndex::build(root, source) else {
        return Positions::new();
    };
    all.into_iter()
        .filter(|n| n.kind() == "identifier" && n.utf8_text(source).ok() == Some(name))
        .filter(|n| index.resolve(*n, source) == LexicalBinding::FunctionLocal)
        .map(position)
        .collect()
}

/// 削除前の Python のファイルで、decorator の付いた `def` の名前 (入れ子の `def` を含む)。
/// decorator で登録した関数 (pytest の fixture など) は、引数の名前で要求されうる
/// (`def test_x(total)`)。構文エラーなら `None` (どの関数も decorator 付きとみなす)。
pub(super) fn python_decorated_functions(source: &[u8]) -> Option<HashSet<String>> {
    let tree = crate::engine::parser::parse_source(source, crate::language::LangId::Python).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    Some(
        nodes(root)
            .into_iter()
            .filter(|n| n.kind() == "decorated_definition")
            .filter_map(|n| n.child_by_field_name("definition"))
            .filter(|n| n.kind() == "function_definition")
            .filter_map(|n| {
                n.child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()
                    .map(str::to_owned)
            })
            .collect(),
    )
}

/// 名前空間の辞書や名前で引く属性に届く名前 ([`PYTHON_REFLECTIVE_NAMES`]) が識別子
/// (属性名を含む) として現れるか。
fn python_mentions_reflective_name(all: &[Node<'_>], source: &[u8]) -> bool {
    all.iter().any(|n| {
        n.kind() == "identifier"
            && n.utf8_text(source)
                .is_ok_and(|s| PYTHON_REFLECTIVE_NAMES.contains(&s))
    })
}

/// 文字列リテラル (隣り合う文字列の暗黙の連結は連結した値) が、`name` を識別子として含むか。
/// 値はエスケープを解き、Python が識別子に施す NFKC で正規化してから比べる (`"\x74otal"`・
/// `"to" "tal(int)"`・全角の綴り)。解けないエスケープ (`\N{...}`) は含むとみなす。f-string の
/// 補間は区切りとして扱う (補間の中のコードは識別子として別に数えられる)。
fn python_strings_mention(all: &[Node<'_>], source: &[u8], name: &str) -> bool {
    all.iter().any(|n| match n.kind() {
        "string" | "concatenated_string" => {
            python_string_value(*n, source).is_none_or(|value| mentions_identifier(&value, name))
        }
        _ => false,
    })
}

/// `string` / `concatenated_string` の値。エスケープを解けないときは `None`。
fn python_string_value(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut value = String::new();
    let mut cursor = node.walk();
    if node.kind() == "concatenated_string" {
        for part in node.named_children(&mut cursor) {
            if part.kind() == "string" {
                value.push_str(&python_string_value(part, source)?);
            }
        }
        return Some(value);
    }
    let mut raw = false;
    for child in node.children(&mut cursor) {
        match child.kind() {
            "string_start" => {
                raw = child
                    .utf8_text(source)
                    .ok()?
                    .chars()
                    .any(|c| matches!(c, 'r' | 'R'));
            }
            "string_content" => {
                value.push_str(&python_unescape(child.utf8_text(source).ok()?, raw)?);
            }
            // 補間の値は分からない。名前の一部としてつながらないよう区切る。
            "interpolation" => value.push('\0'),
            _ => {}
        }
    }
    Some(value)
}

/// Python の文字列リテラルのエスケープを解く。`\N{...}` (文字の名前) は解かずに `None`。
/// bytes リテラルの `\u` も解く (値を広く取るほど、名前を含むとみなす側に倒れる)。
fn python_unescape(text: &str, raw: bool) -> Option<String> {
    if raw || !text.contains('\\') {
        return Some(text.to_owned());
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(escape) = chars.next() else {
            out.push('\\');
            break;
        };
        let mut code = |digits: usize, radix: u32, first: Option<char>| -> Option<char> {
            let mut text: String = first.into_iter().collect();
            while text.len() < digits {
                match chars.peek() {
                    Some(c) if c.is_digit(radix) => text.push(chars.next()?),
                    _ if radix == 8 && !text.is_empty() => break,
                    _ => return None,
                }
            }
            char::from_u32(u32::from_str_radix(&text, radix).ok()?)
        };
        match escape {
            '\n' => {}
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            '\\' | '\'' | '"' => out.push(escape),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\x0b'),
            '0'..='7' => out.push(code(3, 8, Some(escape))?),
            'x' => out.push(code(2, 16, None)?),
            'u' => out.push(code(4, 16, None)?),
            'U' => out.push(code(8, 16, None)?),
            'N' => return None,
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Some(out)
}

/// NFKC で正規化した `value` に、`name` が識別子の区切り (英数字と `_` 以外) で囲まれて現れるか。
fn mentions_identifier(value: &str, name: &str) -> bool {
    use unicode_normalization::UnicodeNormalization;
    let value: String = value.nfkc().collect();
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    value.match_indices(name).any(|(start, matched)| {
        value[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident(c))
            && value[start + matched.len()..]
                .chars()
                .next()
                .is_none_or(|c| !is_ident(c))
    })
}

/// 未知の束縛を作る・名前でコードや属性を引く経路が無く、識別子を綴りで照合できるか
/// (`all` はファイルの全ノード)。`getattr` は、文字列リテラルで `name` 以外の名前を渡す呼び出し
/// だけを許す。
fn python_names_are_static(all: &[Node<'_>], source: &[u8], name: &str) -> bool {
    // 文字列リテラルの名前で属性を読む getattr の呼び出しの、関数名の識別子。識別子から親を
    // 根から探し直さないよう、call の側から function の子を見る。
    let literal_getattr: HashSet<usize> = all
        .iter()
        .filter(|n| n.kind() == "call")
        .filter_map(|call| {
            let function = call.child_by_field_name("function")?;
            (function.kind() == "identifier"
                && function.utf8_text(source).ok() == Some("getattr")
                && python_getattr_reads_literal(*call, source, name))
            .then(|| function.id())
        })
        .collect();
    for n in all {
        if n.is_missing() || n.kind() == "wildcard_import" {
            return false;
        }
        if n.kind() != "identifier" {
            continue;
        }
        let Ok(s) = n.utf8_text(source) else {
            continue;
        };
        // Python は識別子を NFKC 正規化するので、`ｅｘｅｃ` は `exec` と同じ名前になる。
        if !s.is_ascii() && !unicode_normalization::is_nfkc(s) {
            return false;
        }
        if PYTHON_DYNAMIC_NAMES.contains(&s)
            && !(s == "getattr" && literal_getattr.contains(&n.id()))
        {
            return false;
        }
    }
    // getattr を許したファイルでは、名前空間の辞書や名前で引く属性に届く経路がほかに無いことも
    // 確かめる。`getattr(locals(), "update")` のように、取り出した値で束縛を書き換えられるため。
    literal_getattr.is_empty()
        || !all.iter().any(|n| {
            n.kind() == "identifier"
                && n.utf8_text(source)
                    .is_ok_and(|s| PYTHON_REFLECTIVE_NAMES.contains(&s))
        })
}

/// 未知の束縛を作る・名前でコードや属性を引く経路の名前。識別子として現れたら、どのファイルでも
/// 証明をやめる。`getattr` だけは、文字列リテラルで名前を渡す呼び出しを別に判定する。
const PYTHON_DYNAMIC_NAMES: &[&str] = &[
    "globals",
    "vars",
    "exec",
    "eval",
    "__import__",
    "modules",
    "getattr",
    "setattr",
    "__builtins__",
    "__dict__",
    "import_module",
];

/// 名前空間の辞書 (モジュール直下の `locals()`・関数やフレームの globals)、名前で引く属性、
/// オブジェクトの参照関係に届く名前。`getattr` を許すファイルでだけ、識別子でも証明をやめる。
/// 通常のコードにも現れる名前 (関数の中の `locals()` など) を含むため、ほかのファイルには広げない。
const PYTHON_REFLECTIVE_NAMES: &[&str] = &[
    "locals",
    "delattr",
    "__globals__",
    "f_globals",
    "f_locals",
    "f_builtins",
    "__getattribute__",
    "__getattr__",
    "__setattr__",
    "__delattr__",
    "attrgetter",
    "methodcaller",
    "getattr_static",
    "getmembers",
    "getmembers_static",
    "get_objects",
    "get_referrers",
    "get_referents",
];

/// 裸の `getattr(obj, "name")` / `getattr(obj, "name", default)` の呼び出し (`call`) で、名前が
/// 上の一覧にも削除した関数名にも当たらないもの。同じ名前の属性アクセス `obj.name` と同じく読むだけと
/// 言える。属性アクセスなら属性名の出現が参照として数えられるが、文字列の名前は数えられないので、
/// 削除した関数名と同じ名前は許さない。
fn python_getattr_reads_literal(call: Node<'_>, source: &[u8], removed: &str) -> bool {
    let Some(arguments) = call
        .child_by_field_name("arguments")
        .filter(|a| a.kind() == "argument_list")
    else {
        return false;
    };
    // comment や行継続 (`\` の改行) などの extra は引数の間にも入りうるので、引数の位置から外す。
    let mut cursor = arguments.walk();
    let args: Vec<_> = arguments
        .named_children(&mut cursor)
        .filter(|n| !n.is_extra())
        .take(4)
        .collect();
    if !(2..=3).contains(&args.len()) || args.iter().any(|n| python_splat_or_keyword(*n)) {
        return false;
    }
    python_plain_string_name(args[1], source).is_some_and(|attr| {
        attr != removed
            && !PYTHON_DYNAMIC_NAMES.contains(&attr)
            && !PYTHON_REFLECTIVE_NAMES.contains(&attr)
    })
}

/// 引数の数や位置を変えうる splat とキーワード引数。括弧付きの splat `(*xs)` は
/// parenthesized_expression の別名で現れるので、括弧を外して中身を見る。
fn python_splat_or_keyword(mut node: Node<'_>) -> bool {
    loop {
        match node.kind() {
            "list_splat" | "dictionary_splat" | "keyword_argument" => return true,
            "parenthesized_expression" => {
                let mut cursor = node.walk();
                let Some(inner) = node.named_children(&mut cursor).find(|n| !n.is_extra()) else {
                    return false;
                };
                node = inner;
            }
            _ => return false,
        }
    }
}

/// 接頭辞が無いか `r` / `u` で、補間もエスケープも無い文字列リテラルのうち、中身が ASCII の
/// 識別子のもの。値は中身の字面と一致する。
fn python_plain_string_name<'s>(node: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    let parts: Vec<_> = node.named_children(&mut cursor).collect();
    let [start, content, end] = parts.as_slice() else {
        return None;
    };
    if start.kind() != "string_start"
        || content.kind() != "string_content"
        || content.named_child_count() != 0
        || end.kind() != "string_end"
        || !matches!(
            start.utf8_text(source).ok()?.trim_end_matches(['"', '\'']),
            "" | "r" | "R" | "u" | "U"
        )
    {
        return None;
    }
    let text = content.utf8_text(source).ok()?;
    let mut chars = text.chars();
    (chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric()))
    .then_some(text)
}

/// tree-sitter に Load フラグは無いため、読み取りと確定できる親だけを許す。
pub(super) fn python_is_load(mut node: Node<'_>) -> bool {
    // 括弧・tuple の入れ子は入力次第で深くなるため、再帰せずループで親方向へ辿る。
    loop {
        let Some(parent) = node.parent() else {
            return false;
        };
        let in_field = |field| {
            parent
                .child_by_field_name(field)
                .is_some_and(|n| n.id() == node.id())
        };
        match parent.kind() {
            "call" => return in_field("function"),
            "attribute" => return in_field("object"),
            "assignment" | "augmented_assignment" => return in_field("right"),
            "named_expression" | "keyword_argument" => return in_field("value"),
            "argument_list"
            | "return_statement"
            | "expression_statement"
            | "binary_operator"
            | "unary_operator"
            | "boolean_operator"
            | "comparison_operator"
            | "not_operator"
            | "conditional_expression" => return true,
            // tuple/list は代入先にも現れるため、親の読み取り位置まで辿る。
            "parenthesized_expression" | "tuple" | "list" | "set" => node = parent,
            _ => return false,
        }
    }
}

fn has_attributes(node: Node<'_>) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(n) = previous {
        match n.kind() {
            "attribute_item" => return true,
            "line_comment" | "block_comment" => previous = n.prev_named_sibling(),
            _ => break,
        }
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|n| n.kind() == "attribute_item")
}

/// Rust の top-level fn 定義と、通常の let / 引数の有効範囲を別々に返す。
/// refs の出現自体は変更せず、削除候補の帰属判定だけで用いる。
pub(super) fn rust_scope_positions(
    root: Node<'_>,
    source: &[u8],
    name: &str,
) -> (Positions, Positions) {
    if root.has_error() {
        return (Positions::new(), Positions::new());
    }
    let all = nodes(root);
    if all.iter().any(|n| n.is_missing()) {
        return (Positions::new(), Positions::new());
    }
    let named = |n: Node<'_>| {
        n.child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            == Some(name)
    };
    let defs: Vec<_> = all
        .iter()
        .copied()
        .filter(|n| n.kind() == "function_item" && named(*n))
        .collect();
    let mut independent = Positions::new();
    if defs.len() == 1
        && defs[0].parent().is_some_and(|p| p.id() == root.id())
        && !has_attributes(defs[0])
        && let Some(n) = defs[0].child_by_field_name("name")
    {
        independent.insert(position(n));
    }
    // Item macros may introduce declarations not present in this tree. Expression
    // macros cannot introduce bindings into the surrounding function's block.
    let opaque_item_macro = all.iter().any(|node| {
        node.kind() == "macro_invocation"
            && node.parent().is_some_and(|parent| {
                matches!(parent.kind(), "source_file" | "declaration_list")
                    || (parent.kind() == "expression_statement"
                        && parent.parent().is_some_and(|p| {
                            matches!(p.kind(), "source_file" | "declaration_list")
                        }))
            })
    });
    let scope = crate::engine::rust_scope::RustScopeIndex::build(root, source);
    let mut local = Positions::new();
    for node in all {
        if node.utf8_text(source).ok() != Some(name) {
            continue;
        }
        if is_rust_struct_field_non_callable(node) {
            local.insert(position(node));
            continue;
        }
        // Attribute macros can rewrite the entire function, including bindings.
        let mut parent = node.parent();
        let mut attributed_function = false;
        while let Some(ancestor) = parent {
            if ancestor.kind().ends_with("_item") && has_attributes(ancestor) {
                attributed_function = true;
                break;
            }
            parent = ancestor.parent();
        }
        if !opaque_item_macro
            && !attributed_function
            && scope.as_ref().is_some_and(|index| {
                index.resolve(node, source)
                    == crate::engine::lexical_binding::LexicalBinding::RustValueBinding
            })
        {
            local.insert(position(node));
        }
    }
    (independent, local)
}

/// ファイル直下に同名の自由関数を持つ Rust ファイルで、そのファイルのモジュールから修飾なしで
/// 名前を引く位置。こうした出現はその関数か、より内側の束縛に解決し、ほかのファイルの宣言を
/// 指さない。inline の `mod` の中、パスの一部、use、extern ブロック、マクロと属性の中、属性の
/// 付いた item の中は含めず、標準の式マクロ以外のマクロや `macro_rules!` の定義を含むファイルでは
/// 何も返さない。
pub(super) fn rust_file_scope_positions(root: Node<'_>, source: &[u8], name: &str) -> Positions {
    if !crate::engine::impact::rust_cargo_reachability::invokes_only_standard_macros(root, source) {
        return Positions::new();
    }
    let all = nodes(root);
    let mut positions = Positions::new();
    for node in all {
        if node.kind() != "identifier" || node.utf8_text(source).ok() != Some(name) {
            continue;
        }
        let mut ancestor = node.parent();
        let mut module_scope = true;
        while let Some(parent) = ancestor {
            // 属性マクロは item の本体ごと書き換えうる。
            if matches!(
                parent.kind(),
                "scoped_identifier"
                    | "scoped_type_identifier"
                    | "use_declaration"
                    | "mod_item"
                    | "foreign_mod_item"
                    | "token_tree"
                    | "attribute_item"
                    | "inner_attribute_item"
            ) || (parent.kind().ends_with("_item") && has_attributes(parent))
            {
                module_scope = false;
                break;
            }
            ancestor = parent.parent();
        }
        if module_scope {
            positions.insert(position(node));
        }
    }
    positions
}

/// 拡張子の違いと package/module の代替配置を同一のモジュールとして扱う。
pub(super) fn module_key(path: &str) -> String {
    let path = std::path::Path::new(path);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(stem, "__init__" | "mod") {
        path.parent()
            .unwrap_or_else(|| std::path::Path::new(""))
            .to_string_lossy()
            .into_owned()
    } else {
        path.with_extension("").to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn python_is_load_recursive(node: Node<'_>) -> bool {
        let Some(parent) = node.parent() else {
            return false;
        };
        let in_field = |field| {
            parent
                .child_by_field_name(field)
                .is_some_and(|n| n.id() == node.id())
        };
        match parent.kind() {
            "call" => in_field("function"),
            "attribute" => in_field("object"),
            "assignment" | "augmented_assignment" => in_field("right"),
            "named_expression" | "keyword_argument" => in_field("value"),
            "argument_list"
            | "return_statement"
            | "expression_statement"
            | "binary_operator"
            | "unary_operator"
            | "boolean_operator"
            | "comparison_operator"
            | "not_operator"
            | "conditional_expression" => true,
            "parenthesized_expression" | "tuple" | "list" | "set" => {
                python_is_load_recursive(parent)
            }
            _ => false,
        }
    }

    /// `abort` の定義と呼び出しのうち、証明できた座標。構文エラーで空になった結果を取り違えないよう、
    /// 解析木にエラーが無いことも確かめる。
    fn abort_positions(src: &str) -> Positions {
        let tree =
            crate::engine::parser::parse_source(src.as_bytes(), crate::language::LangId::Python)
                .unwrap();
        assert!(!tree.root_node().has_error(), "{src}");
        python_independent_positions(tree.root_node(), src.as_bytes(), "abort")
    }

    /// 名前を文字列リテラルで渡す getattr は、同名の定義と呼び出しの証明を止めない。
    /// getattr の無いファイルの、関数の中の `locals()` と NFKC で変わらない識別子も今のまま証明する。
    #[test]
    fn python_literal_getattr_keeps_independent_positions() {
        for body in [
            "uid = getattr(os, 'getuid', None)",
            "sep = getattr(os, \"sep\")",
            "sep = getattr(\n        os,  # 区切り文字\n        r'sep',\n    )",
            "sep = getattr(os, \\\n        'sep', '/')",
            "sep = getattr(os, u'sep', '/')",
            "sep = getattr(os, '''sep''')",
            "return render(message, locals())",
            "計算 = message",
        ] {
            let src = format!("import os\ndef abort(message):\n    {body}\nabort('failed')\n");
            assert_eq!(abort_positions(&src).len(), 2, "{src}");
        }
    }

    /// 名前が決まらない getattr、一覧の名前や削除した関数名に届く getattr、値として使う getattr、
    /// getattr を許したファイルでの名前空間への経路、NFKC で綴りが変わる識別子では、証明をやめる。
    #[test]
    fn python_dynamic_access_drops_independent_positions() {
        let mut statements: Vec<String> = [
            "getattr(os, NAME, None)",
            "getattr(os, 'ex' 'ec')",
            "getattr(os, '\\x65xec')",
            "getattr(os, f'sep')",
            "getattr(os, b'sep')",
            "getattr(os, ('sep'))",
            "getattr(os, '')",
            "getattr(os, 'os.sep')",
            "getattr(os)",
            "getattr(os, 'sep', None, None)",
            "getattr(os, *NAMES)",
            "getattr(*ARGS)",
            "getattr(os, 'sep', *DEFAULTS)",
            "getattr(os, 'sep', (*DEFAULTS))",
            "getattr(os, 'sep', **OPTIONS)",
            "getattr(os, 'sep', default=None)",
            "getattr(os, name='sep')",
            // 行継続をはさんでも、2 番目の引数を名前として判定する (受け手の 'sep' と取り違えない)。
            "getattr(\\\n    'sep', NAME)",
            "getattr(name for name in NAMES)",
            "getattr(os, 'abort')",
            "os.getattr(os, 'sep')",
            "(getattr)(os, 'sep')",
            "g = getattr",
            "seps = list(map(getattr, [os], ['sep']))",
            "sep = getattr(os, 'sep')\ng = getattr",
            "getattr(locals(), 'update')({'abort': replacement})",
            "ａｂｏｒｔ = replacement",
            "ｅｘｅｃ('from tools.util import abort')",
            "ｇｅｔａｔｔｒ = replacement\ngetattr(os, 'sep')",
        ]
        .map(String::from)
        .to_vec();
        for name in PYTHON_DYNAMIC_NAMES.iter().chain(PYTHON_REFLECTIVE_NAMES) {
            statements.push(format!("getattr(os, '{name}')"));
            statements.push(format!("sep = getattr(os, 'sep')\nvalue = os.{name}"));
        }
        for statement in &statements {
            let src = format!(
                "import os\ndef abort(message):\n    print(message)\n{statement}\nabort('failed')\n"
            );
            assert!(abort_positions(&src).is_empty(), "{src}");
        }
    }

    /// 文字列の値に名前が識別子として現れるか。エスケープ・暗黙の連結・NFKC を解いて比べ、
    /// 計算して作る名前 (`'to' + 'tal'`) と補間をはさむ綴りは今回の範囲の外。
    #[test]
    fn python_strings_mention_reads_literal_values() {
        let mentions = |body: &str| {
            let src = format!("def keep(total):\n    {body}\n");
            let tree = crate::engine::parser::parse_source(
                src.as_bytes(),
                crate::language::LangId::Python,
            )
            .unwrap();
            assert!(!tree.root_node().has_error(), "{src}");
            python_strings_mention(&nodes(tree.root_node()), src.as_bytes(), "total")
        };
        for body in [
            "return 'total'",
            "return \"total(1)\"",
            "return 'to' 'tal(1)'",
            "return '\\x74otal'",
            "return '\\164otal'",
            "return '\\u0074otal'",
            "return '\\N{LATIN SMALL LETTER T}otal'",
            "return 'ｔｏｔａｌ'",
            "return 'tools.util.total'",
            "return f'{x}total'",
            "return f'{\"total\"}'",
            "return b'total'",
        ] {
            assert!(mentions(body), "{body}");
        }
        for body in [
            "return 'subtotal'",
            "return 'totally'",
            "return r'\\x74otal'",
            "return 'tot' 'al_x'",
            "return 'to' + 'tal'",
            "return f'to{x}tal'",
        ] {
            assert!(!mentions(body), "{body}");
        }
    }

    /// decorator の付いた `def` は、入れ子・メソッドも含めて名前を返す。構文エラーなら返さない。
    #[test]
    fn python_decorated_functions_lists_registered_defs() {
        let src = "import pytest\n\n@pytest.fixture\ndef total():\n    return 1\n\ndef keep():\n    @cache\n    def inner():\n        pass\n    return inner\n\nclass C:\n    @staticmethod\n    def method():\n        pass\n";
        let names = python_decorated_functions(src.as_bytes()).unwrap();
        assert_eq!(
            names,
            HashSet::from(["total", "inner", "method"].map(String::from))
        );
        assert!(python_decorated_functions(b"def broken(:\n").is_none());
    }

    /// ループ版が、浅い入力のすべてのノードで再帰版と同じ判定を返す。
    #[test]
    fn python_is_load_matches_recursive_version() {
        let src = "a, (b, [c]) = ((d), e)\nf((g, [h]))\nx = {i, (j)}\nk.l = (m)\nn += (o, p)\n\
                   return_value = q if (r) else s\nfor t in (u, v): pass\ndel (w)\nprint(not (y))\n\
                   z = [aa for bb in (cc)]\n";
        let tree =
            crate::engine::parser::parse_source(src.as_bytes(), crate::language::LangId::Python)
                .unwrap();
        let mut loads = 0;
        let mut stores = 0;
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            tree.root_node(),
            crate::engine::tree_walk::Children::All,
            |node, _| {
                let expected = python_is_load_recursive(node);
                assert_eq!(python_is_load(node), expected, "{}", node.to_sexp());
                if node.kind() == "identifier" {
                    if expected {
                        loads += 1;
                    } else {
                        stores += 1;
                    }
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        assert!(
            loads >= 5 && stores >= 5,
            "読み取りと書き込みの両方を比べる"
        );
    }
}
