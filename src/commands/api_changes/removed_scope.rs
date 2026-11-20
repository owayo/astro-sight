//! 削除した自由関数と無関係な、座標ごとの定義・値束縛を証明する。

use std::collections::HashSet;
use tree_sitter::Node;

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
    for n in &all {
        if n.is_missing() || n.kind() == "wildcard_import" {
            return Positions::new();
        }
        if n.kind() != "identifier" {
            continue;
        }
        let Ok(s) = n.utf8_text(source) else {
            continue;
        };
        // Python は識別子を NFKC 正規化するので、`ｅｘｅｃ` は `exec` と同じ名前になる。
        if !s.is_ascii() && !unicode_normalization::is_nfkc(s) {
            return Positions::new();
        }
        if PYTHON_DYNAMIC_NAMES.contains(&s)
            && !(s == "getattr" && literal_getattr.contains(&n.id()))
        {
            return Positions::new();
        }
    }
    // getattr を許したファイルでは、名前空間の辞書や名前で引く属性に届く経路がほかに無いことも
    // 確かめる。`getattr(locals(), "update")` のように、取り出した値で束縛を書き換えられるため。
    if !literal_getattr.is_empty()
        && all.iter().any(|n| {
            n.kind() == "identifier"
                && n.utf8_text(source)
                    .is_ok_and(|s| PYTHON_REFLECTIVE_NAMES.contains(&s))
        })
    {
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
