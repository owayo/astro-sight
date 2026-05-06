use anyhow::Result;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use crate::language::LangId;
use crate::models::location::Range;
use crate::models::symbol::{Symbol, SymbolKind};

/// シンボルが関数/メソッド本体内のローカルスコープ定義かどうかを判定。
///
/// 関数内の `const`/`let`/`var` 等はファイル外への影響を持たないため、
/// impact 分析の cross-file 起点から除外できる。
/// 未対応言語では保守的に `false`（ローカルではない＝除外しない）を返す。
pub fn is_local_scope_symbol(
    root: Node,
    _source: &[u8],
    lang_id: LangId,
    symbol_range: &Range,
) -> bool {
    let start = tree_sitter::Point {
        row: symbol_range.start.line,
        column: symbol_range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol_range.end.line,
        column: symbol_range.end.column,
    };

    let Some(node) = root.descendant_for_point_range(start, end) else {
        return false; // 保守的: ノード未検出はローカルと判定しない
    };

    match lang_id {
        LangId::Typescript | LangId::Tsx | LangId::Javascript => {
            has_enclosing_function_body_js(node)
        }
        LangId::Rust => has_enclosing_function_body_rust(node),
        LangId::Python => has_enclosing_function_body_python(node),
        LangId::Go => has_enclosing_function_body_go(node),
        LangId::Java | LangId::Kotlin => has_enclosing_function_body_jvm(node),
        _ => false, // 未対応言語は保守的にローカルと判定しない
    }
}

/// JS/TS: 祖先に関数本体 (statement_block) があるかチェック。
fn has_enclosing_function_body_js(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if is_js_function_body(n) {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Rust: 祖先に function_item の block があるかチェック。
fn has_enclosing_function_body_rust(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "block" && n.parent().is_some_and(|p| p.kind() == "function_item") {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Python: 祖先に function_definition の block があるかチェック。
fn has_enclosing_function_body_python(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "block"
            && n.parent()
                .is_some_and(|p| p.kind() == "function_definition")
        {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Go: 祖先に function/method の block があるかチェック。
fn has_enclosing_function_body_go(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "block"
            && n.parent().is_some_and(|p| {
                p.kind() == "function_declaration" || p.kind() == "method_declaration"
            })
        {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Java/Kotlin: 祖先に method/constructor の block があるかチェック。
fn has_enclosing_function_body_jvm(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "block"
            && n.parent().is_some_and(|p| {
                matches!(
                    p.kind(),
                    "method_declaration" | "constructor_declaration" | "function_declaration"
                )
            })
        {
            return true;
        }
        current = n.parent();
    }
    false
}

/// 指定範囲のシンボルがエクスポートされているか（ファイル外から参照可能か）を判定する。
///
/// エクスポートのセマンティクスが明確でない言語（Java、Python、C 等）では、
/// 偽陰性を避けるため保守的に `true` を返す。
pub fn is_symbol_exported(
    root: Node,
    source: &[u8],
    lang_id: LangId,
    symbol_range: &Range,
) -> bool {
    let start = tree_sitter::Point {
        row: symbol_range.start.line,
        column: symbol_range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol_range.end.line,
        column: symbol_range.end.column,
    };

    let Some(node) = root.descendant_for_point_range(start, end) else {
        return true; // 保守的: ノード未検出時はエクスポートと判定
    };

    match lang_id {
        LangId::Typescript | LangId::Tsx | LangId::Javascript => {
            is_exported_js_ts(node, source, root)
        }
        LangId::Rust => is_exported_rust(node),
        LangId::Go => is_exported_go(node, source),
        LangId::Java | LangId::Kotlin => is_exported_jvm(node, source),
        LangId::Zig => is_exported_zig(node, source),
        LangId::Python => is_exported_python(node, source, root),
        LangId::Php => is_exported_php(node, source),
        _ => true, // 未対応言語は保守的にエクスポートと判定
    }
}

/// PHP: `method_declaration` の `visibility_modifier` が `protected` / `private` なら非公開。
///
/// さらに以下のケースも「公開 API として dead-code 判定する対象ではない」ため非公開扱いする:
/// - `abstract public function foo();` — 子クラスでの実装が必須。abstract 宣言自体を
///   dead として報告してもユーザは削除できない
/// - `interface X { public function foo(); }` 内の method 宣言 — implementer 側が
///   必ず提供するので宣言そのものは削除対象にならない
///
/// その他:
/// - トップレベル `function_definition` は PHP では常に `public` 扱い (visibility キーワード
///   自体を書けない)
/// - `class_declaration` / `interface_declaration` / `trait_declaration` / `enum_declaration`
///   は名前空間スコープで常に public (PHP には "package-private" に相当する概念がない)
fn is_exported_php(node: Node, source: &[u8]) -> bool {
    let Some(decl) = find_enclosing_declaration(node) else {
        return true;
    };
    if decl.kind() != "method_declaration" {
        return true;
    }

    // interface 配下の method 宣言は implementer 依存のため dead 対象外。
    // method_declaration -> declaration_list -> interface_declaration
    if let Some(parent) = decl.parent()
        && let Some(grand) = parent.parent()
        && grand.kind() == "interface_declaration"
    {
        return false;
    }

    // method の modifier をまとめて確認 (abstract は visibility と同列の存在で、
    // どちらか先でも後でも構わない)
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        match child.kind() {
            "abstract_modifier" => return false,
            "visibility_modifier" => {
                if let Ok(text) = child.utf8_text(source) {
                    let vis = text.trim();
                    if vis == "private" || vis == "protected" {
                        return false;
                    }
                }
            }
            _ => {}
        }
    }
    true
}

/// Java/Kotlin: `private` 修飾子があれば非公開と判定。
/// デフォルト（修飾子なし）は公開扱い（Java の package-private も cross-file 参照可能）。
fn is_exported_jvm(node: Node, source: &[u8]) -> bool {
    let decl = find_enclosing_declaration(node);
    let Some(decl) = decl else {
        return true;
    };

    // modifiers 子ノードのテキストに "private" が含まれるかチェック
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if child.kind() == "modifiers"
            && let Ok(text) = child.utf8_text(source)
            && text.contains("private")
        {
            return false;
        }
    }
    true
}

/// シンボル名ノードから囲んでいる宣言ノードを探す。
fn find_enclosing_declaration(node: Node) -> Option<Node> {
    let declaration_kinds = [
        "function_declaration",
        // PHP ではトップレベル関数は `function_definition`
        "function_definition",
        "method_declaration",
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "object_declaration",
        // PHP trait
        "trait_declaration",
    ];
    let mut current = Some(node);
    while let Some(n) = current {
        if declaration_kinds.contains(&n.kind()) {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

/// JS/TS: 祖先の export_statement または named export { name } をチェック。
fn is_exported_js_ts(node: Node, source: &[u8], root: Node) -> bool {
    // 祖先に export_statement があるかチェック（関数スコープ境界で停止）。
    // この境界チェックがないと、export された関数内のローカル変数
    // （例: `export function foo()` 内の `const result`）が export_statement の
    // 子孫であるため誤ってエクスポートと判定される。
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "export_statement" {
            return true;
        }
        // 関数本体の境界で停止 — 内部のシンボルはローカル
        if is_js_function_body(n) {
            break;
        }
        current = n.parent();
    }

    // named export のチェック: export { name }
    if let Some(name_node) = node.child_by_field_name("name")
        && let Ok(name) = name_node.utf8_text(source)
    {
        return has_named_export(root, source, name);
    }

    false
}

/// トップレベルの export { ... } 文から一致する名前を検索する。
fn has_named_export(root: Node, source: &[u8], target_name: &str) -> bool {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "export_statement" {
            continue;
        }
        let mut inner = child.walk();
        for grandchild in child.children(&mut inner) {
            if grandchild.kind() != "export_clause" {
                continue;
            }
            let mut spec_cursor = grandchild.walk();
            for spec in grandchild.children(&mut spec_cursor) {
                if spec.kind() != "export_specifier" {
                    continue;
                }
                let local_name = spec
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok());
                if local_name == Some(target_name) {
                    return true;
                }
            }
        }
    }
    false
}

/// Rust: visibility_modifier (pub) または impl ブロック所属をチェック。
///
/// - `pub fn` → エクスポート
/// - trait impl のメソッド（明示的な `pub` 不要）→ エクスポート
/// - 固有 impl の `pub` なしメソッド → モジュール内限定、非エクスポート
fn is_exported_rust(node: Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return true;
        }
    }

    // 囲んでいる impl ブロックをチェック
    let mut parent = node.parent();
    while let Some(p) = parent {
        if p.kind() == "impl_item" {
            // trait impl: メソッドは trait の可視性を継承（常に公開）
            // 固有 impl: pub なしメソッド → モジュール内限定
            return p.child_by_field_name("trait").is_some();
        }
        parent = p.parent();
    }

    false
}

/// メソッドが親 interface/class のメンバーを override しているかを判定する。
///
/// Kotlin/Swift/TS/C# は `override` 修飾子、Java は `@Override` アノテーションで判別する。
/// override メソッドは親型のディスパッチ経由（Android framework の listener callback など）
/// で呼ばれるため cross-file refs では caller を追跡できず、dead-code / API 変更判定の両方で
/// 誤検出源になるため明示的に除外する。
pub fn is_override_method(
    root: Node,
    source: &[u8],
    lang_id: LangId,
    symbol_range: &Range,
) -> bool {
    let start = tree_sitter::Point {
        row: symbol_range.start.line,
        column: symbol_range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol_range.end.line,
        column: symbol_range.end.column,
    };
    let Some(node) = root.descendant_for_point_range(start, end) else {
        return false;
    };
    let Some(decl) = find_method_declaration_for_override(node) else {
        return false;
    };

    match lang_id {
        LangId::Java => declaration_has_java_override_annotation(decl, source),
        LangId::Kotlin | LangId::Swift | LangId::CSharp => {
            declaration_has_override_modifier(decl, source)
        }
        LangId::Typescript | LangId::Tsx => declaration_has_override_modifier(decl, source),
        _ => false,
    }
}

/// 関数/メソッド宣言ノード（override 判定対象）を探す。
/// `find_enclosing_declaration` と異なり TS の `method_definition` も対象に含める。
fn find_method_declaration_for_override(node: Node) -> Option<Node> {
    let decl_kinds = [
        "function_declaration",
        "method_declaration",
        "method_definition",
    ];
    let mut current = Some(node);
    while let Some(n) = current {
        if decl_kinds.contains(&n.kind()) {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

/// `modifiers` 子ノードのテキストに `override` キーワードが含まれるかをチェックする。
/// 単純な部分文字列マッチだと `overrider` のような名前にも反応するため、トークン境界を確認する。
fn declaration_has_override_modifier(decl: Node, source: &[u8]) -> bool {
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let Ok(text) = child.utf8_text(source) else {
            continue;
        };
        if contains_keyword(text, "override") {
            return true;
        }
    }
    // Kotlin の `override` は `modifiers` 経由以外に、TS では個別 `override` キーワード
    // 子ノードとして現れる場合もあるため、全子ノードの kind もチェックする。
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if child.kind() == "override" {
            return true;
        }
    }
    false
}

/// Java の `@Override` マーカーアノテーションが宣言の modifiers に含まれるかをチェックする。
fn declaration_has_java_override_annotation(decl: Node, source: &[u8]) -> bool {
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        // marker_annotation / annotation 子ノードを走査して @Override を探す。
        let mut inner = child.walk();
        for annot in child.children(&mut inner) {
            if !matches!(annot.kind(), "marker_annotation" | "annotation") {
                continue;
            }
            let Ok(text) = annot.utf8_text(source) else {
                continue;
            };
            let stripped = text.trim_start_matches('@').trim();
            if stripped == "Override" || stripped.ends_with(".Override") {
                return true;
            }
        }
    }
    false
}

/// 指定キーワードがトークン境界（英数字・アンダースコア以外）で囲まれて現れるかを判定する。
fn contains_keyword(text: &str, keyword: &str) -> bool {
    let bytes = text.as_bytes();
    let kw = keyword.as_bytes();
    if bytes.len() < kw.len() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i + kw.len() <= bytes.len() {
        if &bytes[i..i + kw.len()] == kw {
            let before_ok = i == 0 || !is_word(bytes[i - 1]);
            let after_ok = i + kw.len() == bytes.len() || !is_word(bytes[i + kw.len()]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Python の関数 / メソッド / クラスがフレームワーク登録デコレータ
/// (Typer / Click / FastAPI / Flask / pytest 等) で装飾されているかを判定する。
///
/// `@app.command(...)` のようなデコレータはフレームワーク内部レジストリに関数を
/// 登録するため、識別子レベルの cross-file refs では caller を追跡できず
/// dead-code 判定で誤陽性になる。本判定で装飾されたシンボルは保守的に
/// 「フレームワーク到達可能」と見なし、dead 候補から除外できる。
///
/// 末尾セグメントマッチ (`<obj>.<method>` 形式):
/// - Typer / Click / Discord.py / aiogram: `command`, `callback`, `group`, `event`
/// - FastAPI / Flask HTTP: `get`, `post`, `put`, `delete`, `patch`, `options`,
///   `head`, `route`, `websocket`
/// - FastAPI / Flask lifecycle: `middleware`, `on_event`, `exception_handler`,
///   `before_request`, `after_request`, `errorhandler`, `teardown_request`,
///   `teardown_appcontext`, `context_processor`, `shell_context_processor`,
///   `before_first_request`
/// - Celery / RQ: `task`
/// - pytest: `fixture`, `parametrize`, `usefixtures`
///
/// 単体名マッチ:
/// - Django: `receiver`, `login_required`, `permission_required`,
///   `csrf_exempt`, `cache_page`, `require_GET`, `require_POST`,
///   `require_http_methods`, `require_safe`, `staff_member_required`,
///   `user_passes_test`
///
/// その他: `pytest.mark.<anything>` プレフィックスを pytest test marker として認識。
pub fn has_framework_entrypoint_decorator_python(
    root: Node,
    source: &[u8],
    symbol_range: &Range,
) -> bool {
    let start = tree_sitter::Point {
        row: symbol_range.start.line,
        column: symbol_range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol_range.end.line,
        column: symbol_range.end.column,
    };
    let Some(node) = root.descendant_for_point_range(start, end) else {
        return false;
    };

    // 関数 / メソッド / クラス定義の祖先を探す
    let mut current = Some(node);
    let mut def_node = None;
    while let Some(n) = current {
        if matches!(n.kind(), "function_definition" | "class_definition") {
            def_node = Some(n);
            break;
        }
        current = n.parent();
    }
    let Some(def) = def_node else {
        return false;
    };

    // 親が decorated_definition でなければデコレータ無し
    let Some(parent) = def.parent() else {
        return false;
    };
    if parent.kind() != "decorated_definition" {
        return false;
    }

    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.kind() != "decorator" {
            continue;
        }
        if decorator_matches_framework_pattern_python(child, source) {
            return true;
        }
    }
    false
}

/// Python decorator ノードのテキストから先頭式 (call 引数を除く) を取り出し、
/// 既知のフレームワーク登録パターンに一致するかを判定する。
fn decorator_matches_framework_pattern_python(decorator: Node, source: &[u8]) -> bool {
    let Ok(text) = decorator.utf8_text(source) else {
        return false;
    };
    let stripped = text.trim_start_matches('@').trim();
    let head = stripped.split('(').next().unwrap_or(stripped).trim();
    if head.is_empty() {
        return false;
    }

    // pytest.mark.<anything> は pytest test marker として扱う
    if head.starts_with("pytest.mark.") {
        return true;
    }

    const BARE_DECORATORS: &[&str] = &[
        "receiver",
        "login_required",
        "permission_required",
        "csrf_exempt",
        "cache_page",
        "require_GET",
        "require_POST",
        "require_http_methods",
        "require_safe",
        "staff_member_required",
        "user_passes_test",
    ];
    if BARE_DECORATORS.contains(&head) {
        return true;
    }

    let last = head.rsplit('.').next().unwrap_or(head);
    const TAIL_DECORATORS: &[&str] = &[
        // Typer / Click / Discord.py / aiogram
        "command",
        "callback",
        "group",
        "event",
        // FastAPI / Flask HTTP methods
        "get",
        "post",
        "put",
        "delete",
        "patch",
        "options",
        "head",
        "route",
        "websocket",
        // FastAPI / Flask lifecycle
        "middleware",
        "on_event",
        "exception_handler",
        "before_request",
        "after_request",
        "errorhandler",
        "teardown_request",
        "teardown_appcontext",
        "context_processor",
        "shell_context_processor",
        "before_first_request",
        // Celery / RQ
        "task",
        // pytest
        "fixture",
        "parametrize",
        "usefixtures",
    ];
    TAIL_DECORATORS.contains(&last)
}

/// Python の `class_definition` が直接持つ base class 名のリストを返す。
/// 例: `class Foo(Bar, baz.Qux):` → `["Bar", "baz.Qux"]`
///
/// dead-code 判定で同一ファイル内の継承チェーンを fixed-point で解決するための低レベル
/// helper。クロスファイル継承解析や引数 (`metaclass=...`) の解釈は行わない。
pub fn python_class_base_names(root: Node, source: &[u8], symbol_range: &Range) -> Vec<String> {
    let start = tree_sitter::Point {
        row: symbol_range.start.line,
        column: symbol_range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol_range.end.line,
        column: symbol_range.end.column,
    };
    let Some(node) = root.descendant_for_point_range(start, end) else {
        return Vec::new();
    };

    // class_definition の祖先を探す
    let mut current = Some(node);
    let mut class_node = None;
    while let Some(n) = current {
        if n.kind() == "class_definition" {
            class_node = Some(n);
            break;
        }
        current = n.parent();
    }
    let Some(class_def) = class_node else {
        return Vec::new();
    };

    // superclasses field は argument_list ノードを含む
    let Some(args) = class_def.child_by_field_name("superclasses") else {
        return Vec::new();
    };

    let mut bases = Vec::new();
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        // identifier (`Bar`) / attribute (`unittest.TestCase`) のみを拾う。
        // keyword_argument (`metaclass=ABCMeta`) や generator 等は無視する。
        if matches!(child.kind(), "identifier" | "attribute")
            && let Ok(text) = child.utf8_text(source)
        {
            bases.push(text.to_string());
        }
    }
    bases
}

/// Rust のシンボルが trait impl ブロックに属しているかを判定する。
/// trait impl メソッドは trait dispatch 経由で呼ばれるため、cross-file refs
/// 検索では caller を追跡できず、dead-code 判定でスキップする必要がある。
pub(crate) fn is_trait_impl_method_rust(root: Node, symbol_range: &Range) -> bool {
    let start = tree_sitter::Point {
        row: symbol_range.start.line,
        column: symbol_range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol_range.end.line,
        column: symbol_range.end.column,
    };
    let Some(node) = root.descendant_for_point_range(start, end) else {
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

/// JS/TS: ノードが関数本体（親が関数系ノードの statement_block）かどうかを判定する。
fn is_js_function_body(node: Node) -> bool {
    if node.kind() != "statement_block" {
        return false;
    }
    node.parent().is_some_and(|p| {
        matches!(
            p.kind(),
            "function_declaration"
                | "function"
                | "function_expression"
                | "arrow_function"
                | "method_definition"
                | "generator_function_declaration"
                | "generator_function"
        )
    })
}

/// Go: 大文字で始まる識別子はエクスポート。
fn is_exported_go(node: Node, source: &[u8]) -> bool {
    let name = node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok());
    match name {
        Some(n) => n.starts_with(char::is_uppercase),
        None => true, // 保守的
    }
}

/// Zig: 宣言行に `pub` キーワードがあればエクスポート。
fn is_exported_zig(node: Node, source: &[u8]) -> bool {
    // variable_declaration / function_declaration の先頭行に "pub " があるかチェック
    let decl = find_enclosing_declaration_zig(node);
    let Some(decl) = decl else {
        return true; // 保守的
    };
    let start = decl.start_byte();
    // "pub " は宣言の先頭に付くため最初の20バイト程度をチェック
    let end = (start + 20).min(source.len());
    let prefix = &source[start..end];
    prefix.starts_with(b"pub ")
}

/// Zig: 囲んでいる宣言ノードを探す。
fn find_enclosing_declaration_zig(node: Node) -> Option<Node> {
    let zig_decl_kinds = [
        "function_declaration",
        "variable_declaration",
        "test_declaration",
    ];
    let mut current = Some(node);
    while let Some(n) = current {
        if zig_decl_kinds.contains(&n.kind()) {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

/// Python: PEP8 の `_` プレフィックスを private 慣習として扱い、
/// モジュール先頭に `__all__` があればそのリストを優先して判定する。
fn is_exported_python(node: Node, source: &[u8], root: Node) -> bool {
    let name = node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .or_else(|| {
            // フォールバック: ノードが識別子そのものの場合
            if node.kind() == "identifier" {
                node.utf8_text(source).ok()
            } else {
                None
            }
        });
    let Some(name) = name else {
        return true; // 保守的
    };

    // `__all__` が定義されていればその集合のみを public とみなす
    if let Some(dunder_all) = parse_python_dunder_all(root, source) {
        return dunder_all.iter().any(|s| s == name);
    }

    // デフォルト: `_` プレフィックスは private
    !name.starts_with('_')
}

/// Python モジュールのトップレベル `__all__` 定義を解析し、収録された
/// シンボル名の一覧を返す。定義がなければ None。
///
/// 対応する形式:
///   - `__all__ = ["foo", 'bar']`
///   - `__all__ = ("foo", "bar")`
///   - `__all__: list[str] = ["foo"]`
///
/// 複雑な演算（`__all__ += [...]` 等）は未対応。
fn parse_python_dunder_all(root: Node, source: &[u8]) -> Option<Vec<String>> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "expression_statement" {
            continue;
        }
        let Some(assignment) = child.child(0) else {
            continue;
        };
        if assignment.kind() != "assignment" {
            continue;
        }
        let Some(left) = assignment.child_by_field_name("left") else {
            continue;
        };
        if left.utf8_text(source).ok() != Some("__all__") {
            continue;
        }
        let Some(right) = assignment.child_by_field_name("right") else {
            continue;
        };
        if right.kind() != "list" && right.kind() != "tuple" {
            // 現状は単純な list / tuple リテラルのみ対応
            return None;
        }

        let mut names = Vec::new();
        let mut rc = right.walk();
        for element in right.children(&mut rc) {
            if element.kind() != "string" {
                continue;
            }
            if let Ok(text) = element.utf8_text(source) {
                let stripped = text.trim_matches(|c: char| c == '"' || c == '\'');
                if !stripped.is_empty() {
                    names.push(stripped.to_string());
                }
            }
        }
        return Some(names);
    }
    None
}

/// 関数/メソッドノードの循環的複雑度を算出する（ベース1 + 分岐ノード数）。
/// ネストした関数/クロージャの分岐は含めない。
pub fn calculate_complexity(node: Node, lang_id: LangId) -> usize {
    let branch_kinds = branch_node_kinds(lang_id);
    let func_kinds = function_boundary_kinds(lang_id);
    let mut count = 1; // ベース複雑度
    count_branch_nodes(node, branch_kinds, func_kinds, true, &mut count);
    count
}

/// 再帰的に分岐ノードをカウントする。
/// ネストした関数境界（クロージャ・内部関数）で走査を停止する。
fn count_branch_nodes(
    node: Node,
    branch_kinds: &'static [&'static str],
    func_kinds: &[&str],
    is_root: bool,
    count: &mut usize,
) {
    let kind = node.kind();
    // ルート以外の関数境界で停止（ネスト関数の分岐を除外）
    if !is_root && func_kinds.contains(&kind) {
        return;
    }
    if branch_kinds.contains(&kind) {
        *count += 1;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count_branch_nodes(child, branch_kinds, func_kinds, false, count);
    }
}

/// 関数境界を示すノード種別を返す（ネスト関数検出用）。
/// 言語別の関数境界ノード種別を返す。
/// 静的スライスを返すことで毎回の Vec アロケーションを回避する。
fn function_boundary_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &["function_item", "closure_expression"],
        LangId::Javascript | LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "function_expression",
            "arrow_function",
            "method_definition",
            "generator_function_declaration",
        ],
        LangId::Python => &["function_definition", "lambda"],
        LangId::Go => &["function_declaration", "method_declaration", "func_literal"],
        LangId::Java => &["method_declaration", "lambda_expression"],
        LangId::Kotlin => &["function_declaration", "lambda_literal"],
        LangId::Swift => &["function_declaration", "lambda_literal"],
        LangId::CSharp => &["method_declaration", "lambda_expression"],
        LangId::Php => &[
            "function_definition",
            "method_declaration",
            "anonymous_function_creation_expression",
        ],
        LangId::Ruby => &["method", "singleton_method", "lambda", "block"],
        _ => &[],
    }
}

/// 言語別の分岐ノード種別を返す。
/// 静的スライスを返すことで毎回の Vec アロケーションを回避する。
fn branch_node_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &[
            "if_expression",
            "match_expression",
            "for_expression",
            "while_expression",
            "loop_expression",
            "else_clause",
            "match_arm",
        ],
        LangId::Javascript | LangId::Typescript | LangId::Tsx => &[
            "if_statement",
            "switch_case",
            "for_statement",
            "for_in_statement",
            "while_statement",
            "do_statement",
            "ternary_expression",
            "catch_clause",
        ],
        LangId::Python => &[
            "if_statement",
            "elif_clause",
            "for_statement",
            "while_statement",
            "except_clause",
            "conditional_expression",
        ],
        LangId::Go => &[
            "if_statement",
            "for_statement",
            "select_statement",
            "type_switch_statement",
            "case_clause",
        ],
        LangId::Java | LangId::Kotlin => &[
            "if_statement",
            "switch_expression",
            "for_statement",
            "enhanced_for_statement",
            "while_statement",
            "do_statement",
            "catch_clause",
        ],
        LangId::Ruby => &[
            "if", "elsif", "unless", "case", "when", "for", "while", "until", "rescue",
        ],
        LangId::Php => &[
            "if_statement",
            "switch_statement",
            "case_statement",
            "for_statement",
            "foreach_statement",
            "while_statement",
            "do_statement",
            "catch_clause",
        ],
        LangId::CSharp => &[
            "if_statement",
            "switch_section",
            "for_statement",
            "for_each_statement",
            "while_statement",
            "do_statement",
            "catch_clause",
        ],
        LangId::Zig => &[
            "if_expression",
            "if_statement",
            "for_expression",
            "for_statement",
            "while_expression",
            "while_statement",
            "switch_expression",
            "switch_case",
            "catch_expression",
            "else_clause",
        ],
        // 汎用パターン（C, C++, Swift, Bash 等）
        _ => &[
            "if_statement",
            "if_expression",
            "for_statement",
            "for_expression",
            "while_statement",
            "while_expression",
            "switch_statement",
            "case_statement",
            "catch_clause",
        ],
    }
}

/// パース済み AST からシンボルを抽出する。
pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {
    let query_src = symbol_query(lang_id);
    if query_src.is_empty() {
        return Ok(fallback_symbols(root, source));
    }

    let language = lang_id.ts_language();
    let query = Query::new(&language, query_src)?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    let mut symbols = Vec::new();
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let node = capture.node;
            let capture_name = &query.capture_names()[capture.index as usize];
            let kind = capture_name_to_kind(capture_name);

            if let Some(kind) = kind {
                let name = node.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    let doc = extract_doc_comment(node, source);
                    let parent_node = node.parent().unwrap_or(node);
                    // 関数/メソッドの場合のみ循環的複雑度を算出
                    let complexity = if matches!(kind, SymbolKind::Function | SymbolKind::Method) {
                        Some(calculate_complexity(parent_node, lang_id))
                    } else {
                        None
                    };
                    symbols.push(Symbol {
                        name,
                        kind,
                        range: Range::from(parent_node.range()),
                        doc,
                        complexity,
                        container: None,
                        children: Vec::new(),
                    });
                }
            }
        }
    }

    assign_enclosing_containers(&mut symbols);
    Ok(symbols)
}

/// 同一ファイル内の symbols について、各 method/function に enclosing container 名を付与する。
///
/// container 候補は class / struct / trait / interface / enum / type (Rust の impl 対象型を含む)。
/// method の range が container の range に内包される場合、最も内側 (range が小さい) container 名を
/// `Symbol::container` に設定する。同名の method が複数の impl ブロックに存在しても、container 名で
/// 見分けが付くようになる (例: `impl Default for A` の `default` には container=A)。
fn assign_enclosing_containers(symbols: &mut [Symbol]) {
    use crate::models::location::Range as Rng;
    let containers: Vec<(usize, Rng)> = symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            matches!(
                s.kind,
                SymbolKind::Class
                    | SymbolKind::Struct
                    | SymbolKind::Trait
                    | SymbolKind::Interface
                    | SymbolKind::Enum
                    | SymbolKind::Type
            )
        })
        .map(|(i, s)| (i, s.range))
        .collect();

    for i in 0..symbols.len() {
        let s = &symbols[i];
        if !matches!(s.kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }
        let target = s.range;
        let mut best: Option<(usize, usize)> = None;
        for (ci, crange) in &containers {
            if *ci == i {
                continue;
            }
            if range_contains(crange, &target) {
                let size = crange.end.line.saturating_sub(crange.start.line);
                match best {
                    None => best = Some((*ci, size)),
                    Some((_, best_size)) if size < best_size => best = Some((*ci, size)),
                    _ => {}
                }
            }
        }
        if let Some((ci, _)) = best {
            symbols[i].container = Some(symbols[ci].name.clone());
        }
    }
}

fn range_contains(
    outer: &crate::models::location::Range,
    inner: &crate::models::location::Range,
) -> bool {
    if outer.start.line > inner.start.line || outer.end.line < inner.end.line {
        return false;
    }
    if outer.start.line == inner.start.line && outer.start.column > inner.start.column {
        return false;
    }
    if outer.end.line == inner.end.line && outer.end.column < inner.end.column {
        return false;
    }
    true
}

fn capture_name_to_kind(name: &str) -> Option<SymbolKind> {
    match name {
        "function.name" => Some(SymbolKind::Function),
        "method.name" => Some(SymbolKind::Method),
        "class.name" => Some(SymbolKind::Class),
        "struct.name" => Some(SymbolKind::Struct),
        "enum.name" => Some(SymbolKind::Enum),
        "interface.name" | "trait.name" => Some(SymbolKind::Trait),
        "constant.name" => Some(SymbolKind::Constant),
        "variable.name" => Some(SymbolKind::Variable),
        "type.name" => Some(SymbolKind::Type),
        "module.name" => Some(SymbolKind::Module),
        "import.name" => Some(SymbolKind::Import),
        "field.name" => Some(SymbolKind::Field),
        _ => None,
    }
}

fn extract_doc_comment(node: Node<'_>, source: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    let mut prev = parent.prev_named_sibling();

    let mut comments = Vec::new();
    while let Some(p) = prev {
        if p.kind().contains("comment") {
            let text = p.utf8_text(source).ok()?;
            comments.push(text.to_string());
            prev = p.prev_named_sibling();
        } else {
            break;
        }
    }

    if comments.is_empty() {
        None
    } else {
        comments.reverse();
        Some(comments.join("\n"))
    }
}

/// フォールバック: トップレベルの named ノードをシンボルとして抽出する。
fn fallback_symbols(root: Node<'_>, source: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut cursor = root.walk();

    for child in root.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        let kind = node_kind_to_symbol_kind(child.kind());
        let name = find_name_child(child, source).unwrap_or_else(|| child.kind().to_string());

        symbols.push(Symbol {
            name,
            kind,
            range: Range::from(child.range()),
            doc: None,
            complexity: None,
            container: None,
            children: Vec::new(),
        });
    }

    symbols
}

fn find_name_child(node: Node<'_>, source: &[u8]) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node.utf8_text(source).ok().map(|s| s.to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier"
            || child.kind() == "type_identifier"
            || child.kind() == "name"
        {
            return child.utf8_text(source).ok().map(|s| s.to_string());
        }
    }
    None
}

fn node_kind_to_symbol_kind(kind: &str) -> SymbolKind {
    match kind {
        "function_item" | "function_definition" | "function_declaration" | "method_declaration" => {
            SymbolKind::Function
        }
        "struct_item" | "struct_declaration" => SymbolKind::Struct,
        "enum_item" | "enum_declaration" => SymbolKind::Enum,
        "class_declaration" | "class_definition" => SymbolKind::Class,
        "trait_item" | "interface_declaration" => SymbolKind::Trait,
        "const_item" | "const_declaration" => SymbolKind::Constant,
        "type_alias" | "type_declaration" => SymbolKind::Type,
        "impl_item" | "impl_block" => SymbolKind::Type,
        "mod_item" | "module" => SymbolKind::Module,
        "use_declaration" | "import_statement" | "import_declaration" => SymbolKind::Import,
        _ => SymbolKind::Variable,
    }
}

fn symbol_query(lang_id: LangId) -> &'static str {
    match lang_id {
        LangId::Rust => {
            r#"
            (function_item name: (identifier) @function.name)
            (struct_item name: (type_identifier) @struct.name)
            (enum_item name: (type_identifier) @enum.name)
            (trait_item name: (type_identifier) @trait.name)
            (impl_item type: (type_identifier) @type.name)
            (const_item name: (identifier) @constant.name)
            (static_item name: (identifier) @constant.name)
            (type_item name: (type_identifier) @type.name)
            (mod_item name: (identifier) @module.name)
            "#
        }
        LangId::C => {
            r#"
            (function_definition declarator: (function_declarator declarator: (identifier) @function.name))
            (struct_specifier name: (type_identifier) @struct.name)
            (enum_specifier name: (type_identifier) @enum.name)
            "#
        }
        LangId::Cpp => {
            r#"
            (function_definition declarator: (function_declarator declarator: (identifier) @function.name))
            (class_specifier name: (type_identifier) @class.name)
            (struct_specifier name: (type_identifier) @struct.name)
            (enum_specifier name: (type_identifier) @enum.name)
            (namespace_definition name: (namespace_identifier) @module.name)
            "#
        }
        LangId::Python => {
            r#"
            (function_definition name: (identifier) @function.name)
            (class_definition name: (identifier) @class.name)
            "#
        }
        LangId::Javascript => {
            r#"
            (function_declaration name: (identifier) @function.name)
            (class_declaration name: (identifier) @class.name)
            (method_definition name: (property_identifier) @method.name)
            (lexical_declaration (variable_declarator name: (identifier) @variable.name))
            "#
        }
        LangId::Typescript | LangId::Tsx => {
            r#"
            (function_declaration name: (identifier) @function.name)
            (class_declaration name: (type_identifier) @class.name)
            (method_definition name: (property_identifier) @method.name)
            (interface_declaration name: (type_identifier) @interface.name)
            (type_alias_declaration name: (type_identifier) @type.name)
            (enum_declaration name: (identifier) @enum.name)
            (lexical_declaration (variable_declarator name: (identifier) @variable.name))
            "#
        }
        LangId::Go => {
            r#"
            (package_clause (package_identifier) @module.name)
            (function_declaration name: (identifier) @function.name)
            (method_declaration name: (field_identifier) @method.name)
            (type_declaration (type_spec name: (type_identifier) @type.name))
            "#
        }
        LangId::Php => {
            r#"
            (function_definition name: (name) @function.name)
            (class_declaration name: (name) @class.name)
            (method_declaration name: (name) @method.name)
            (interface_declaration name: (name) @interface.name)
            (enum_declaration name: (name) @enum.name)
            (trait_declaration name: (name) @trait.name)
            "#
        }
        LangId::Java => {
            r#"
            (method_declaration name: (identifier) @function.name)
            (class_declaration name: (identifier) @class.name)
            (interface_declaration name: (identifier) @interface.name)
            (enum_declaration name: (identifier) @enum.name)
            "#
        }
        LangId::Kotlin => {
            r#"
            (function_declaration (simple_identifier) @function.name)
            (class_declaration (type_identifier) @class.name)
            (object_declaration (type_identifier) @class.name)
            "#
        }
        LangId::Swift => {
            // tree-sitter-swift は struct/class/enum に class_declaration を使用
            r#"
            (function_declaration name: (simple_identifier) @function.name)
            (class_declaration name: (type_identifier) @class.name)
            (protocol_declaration name: (type_identifier) @interface.name)
            "#
        }
        LangId::CSharp => {
            r#"
            (namespace_declaration name: (_) @module.name)
            (method_declaration name: (identifier) @function.name)
            (class_declaration name: (identifier) @class.name)
            (struct_declaration name: (identifier) @struct.name)
            (interface_declaration name: (identifier) @interface.name)
            (enum_declaration name: (identifier) @enum.name)
            "#
        }
        LangId::Bash => {
            r#"
            (function_definition name: (word) @function.name)
            "#
        }
        LangId::Ruby => {
            r#"
            (method name: (_) @function.name)
            (singleton_method name: (_) @function.name)
            (class name: (constant) @class.name)
            (class name: (scope_resolution name: (_) @class.name))
            (module name: (constant) @module.name)
            (module name: (scope_resolution name: (_) @module.name))
            "#
        }
        LangId::Zig => {
            // Zig: 型は const X = struct/enum/union {} で定義されるため variable_declaration 経由
            r#"
            (function_declaration name: (identifier) @function.name)
            (variable_declaration (identifier) @variable.name)
            (test_declaration (identifier) @function.name)
            (test_declaration (string) @function.name)
            "#
        }
        LangId::Xojo => {
            r#"
            (class_declaration name: (identifier) @class.name)
            (module_declaration name: (identifier) @module.name)
            (interface_declaration name: (identifier) @interface.name)
            (structure_declaration name: (identifier) @struct.name)
            (enum_declaration name: (identifier) @enum.name)
            (sub_declaration name: (identifier) @method.name)
            (function_declaration name: (identifier) @method.name)
            (event_declaration name: (identifier) @method.name)
            (delegate_declaration name: (identifier) @method.name)
            (simple_property_declaration name: (identifier) @field.name)
            (computed_property_declaration name: (identifier) @field.name)
            (const_declaration name: (identifier) @constant.name)
            "#
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_exported(source: &str, lang_id: LangId, symbol_name: &str) -> bool {
        let language = lang_id.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, source.as_bytes(), lang_id).unwrap();
        let sym = syms
            .iter()
            .find(|s| s.name == symbol_name)
            .unwrap_or_else(|| {
                panic!(
                    "symbol '{}' not found in {:?}",
                    symbol_name,
                    syms.iter().map(|s| &s.name).collect::<Vec<_>>()
                )
            });
        is_symbol_exported(root, source.as_bytes(), lang_id, &sym.range)
    }

    #[test]
    fn ts_export_function_is_exported() {
        assert!(check_exported(
            "export function foo() {}",
            LangId::Typescript,
            "foo"
        ));
    }

    #[test]
    fn ts_non_export_function_is_not_exported() {
        assert!(!check_exported(
            "function foo() {}",
            LangId::Typescript,
            "foo"
        ));
    }

    #[test]
    fn ts_named_export_is_exported() {
        assert!(check_exported(
            "function foo() {}\nexport { foo }",
            LangId::Typescript,
            "foo"
        ));
    }

    #[test]
    fn rust_pub_fn_is_exported() {
        assert!(check_exported("pub fn foo() {}", LangId::Rust, "foo"));
    }

    #[test]
    fn rust_private_fn_is_not_exported() {
        assert!(!check_exported("fn foo() {}", LangId::Rust, "foo"));
    }

    #[test]
    fn go_uppercase_is_exported() {
        assert!(check_exported(
            "package main\nfunc Foo() {}",
            LangId::Go,
            "Foo"
        ));
    }

    #[test]
    fn go_lowercase_is_not_exported() {
        assert!(!check_exported(
            "package main\nfunc foo() {}",
            LangId::Go,
            "foo"
        ));
    }

    #[test]
    fn ts_local_var_inside_exported_fn_is_not_exported() {
        assert!(!check_exported(
            "export function foo() { const result = 1; }",
            LangId::Typescript,
            "result"
        ));
    }

    #[test]
    fn ts_top_level_exported_const_is_exported() {
        assert!(check_exported(
            "export const bar = 42;",
            LangId::Typescript,
            "bar"
        ));
    }

    #[test]
    fn python_public_function_is_exported() {
        assert!(check_exported(
            "def foo():\n    pass\n",
            LangId::Python,
            "foo"
        ));
    }

    #[test]
    fn python_underscore_function_is_not_exported() {
        assert!(!check_exported(
            "def _helper():\n    pass\n",
            LangId::Python,
            "_helper"
        ));
    }

    #[test]
    fn python_dunder_function_is_not_exported() {
        // `__dunder__` も `_` プレフィックスなので private 扱い
        assert!(!check_exported(
            "def __internal__():\n    pass\n",
            LangId::Python,
            "__internal__"
        ));
    }

    #[test]
    fn python_underscore_method_is_not_exported() {
        assert!(!check_exported(
            "class C:\n    def _helper(self):\n        pass\n",
            LangId::Python,
            "_helper"
        ));
    }

    #[test]
    fn python_underscore_class_is_not_exported() {
        assert!(!check_exported(
            "class _Internal:\n    pass\n",
            LangId::Python,
            "_Internal"
        ));
    }

    #[test]
    fn python_dunder_all_limits_exports_to_list() {
        let src = r#"
__all__ = ["public_api"]

def public_api():
    pass

def also_public_without_underscore():
    pass
"#;
        assert!(check_exported(src, LangId::Python, "public_api"));
        // `also_public_without_underscore` は `_` プレフィックスを持たないが
        // `__all__` に含まれていないため非 public と判定される
        assert!(!check_exported(
            src,
            LangId::Python,
            "also_public_without_underscore"
        ));
    }

    #[test]
    fn python_dunder_all_tuple_form_supported() {
        let src = r#"
__all__ = ("foo", 'bar')

def foo():
    pass

def bar():
    pass

def baz():
    pass
"#;
        assert!(check_exported(src, LangId::Python, "foo"));
        assert!(check_exported(src, LangId::Python, "bar"));
        assert!(!check_exported(src, LangId::Python, "baz"));
    }

    #[test]
    fn python_typed_dunder_all_supported() {
        let src = r#"
__all__: list[str] = ["typed_api"]

def typed_api():
    pass

def other():
    pass
"#;
        assert!(check_exported(src, LangId::Python, "typed_api"));
        assert!(!check_exported(src, LangId::Python, "other"));
    }

    // --- is_local_scope_symbol テスト ---

    fn check_local_scope(source: &str, lang_id: LangId, symbol_name: &str) -> bool {
        let language = lang_id.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, source.as_bytes(), lang_id).unwrap();
        let sym = syms
            .iter()
            .find(|s| s.name == symbol_name)
            .unwrap_or_else(|| {
                panic!(
                    "symbol '{}' not found in {:?}",
                    symbol_name,
                    syms.iter().map(|s| &s.name).collect::<Vec<_>>()
                )
            });
        is_local_scope_symbol(root, source.as_bytes(), lang_id, &sym.range)
    }

    #[test]
    fn ts_local_var_is_local_scope() {
        assert!(check_local_scope(
            "export function foo() { const result = 1; }",
            LangId::Typescript,
            "result"
        ));
    }

    #[test]
    fn ts_top_level_var_is_not_local_scope() {
        assert!(!check_local_scope(
            "export const bar = 42;",
            LangId::Typescript,
            "bar"
        ));
    }

    #[test]
    fn ts_arrow_fn_local_is_local_scope() {
        assert!(check_local_scope(
            "export const foo = () => { const x = 1; }",
            LangId::Typescript,
            "x"
        ));
    }

    #[test]
    fn rust_fn_def_is_not_local_scope() {
        // Rust のクエリは関数内ローカル変数をキャプチャしないが、関数定義自体はローカルスコープではない
        assert!(!check_local_scope(
            "pub fn foo() { let x = 1; }",
            LangId::Rust,
            "foo"
        ));
    }

    #[test]
    fn ts_non_export_top_level_var_is_not_local_scope() {
        assert!(!check_local_scope(
            "const bar = 42;",
            LangId::Typescript,
            "bar"
        ));
    }

    // --- calculate_complexity テスト ---

    fn get_complexity(source: &str, lang_id: LangId, symbol_name: &str) -> Option<usize> {
        let language = lang_id.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, source.as_bytes(), lang_id).unwrap();
        let sym = syms
            .iter()
            .find(|s| s.name == symbol_name)
            .unwrap_or_else(|| {
                panic!(
                    "symbol '{}' not found in {:?}",
                    symbol_name,
                    syms.iter().map(|s| &s.name).collect::<Vec<_>>()
                )
            });
        sym.complexity
    }

    #[test]
    fn rust_empty_fn_complexity_1() {
        // 空の関数はベース複雑度 1
        assert_eq!(get_complexity("fn foo() {}", LangId::Rust, "foo"), Some(1));
    }

    #[test]
    fn rust_if_else_match_complexity() {
        // if(+1) + else(+1) + match(+1) + 2 match_arm(+2) = base 1 + 5 = 6
        // ただし match_arm は各アーム全てカウント
        let src = r#"
fn foo() {
    if x {
    } else {
        match y {
            1 => {},
            _ => {},
        }
    }
}
"#;
        // if_expression=1, else_clause=1, match_expression=1, match_arm=2 → 1+5=6
        assert_eq!(get_complexity(src, LangId::Rust, "foo"), Some(6));
    }

    #[test]
    fn rust_for_while_loop_complexity() {
        let src = r#"
fn bar() {
    for i in 0..10 {
        while x > 0 {
            loop {
                break;
            }
        }
    }
}
"#;
        // for_expression=1, while_expression=1, loop_expression=1 → 1+3=4
        assert_eq!(get_complexity(src, LangId::Rust, "bar"), Some(4));
    }

    #[test]
    fn python_complexity() {
        let src = r#"
def foo():
    if x:
        pass
    elif y:
        pass
    for i in range(10):
        pass
"#;
        // if_statement=1, elif_clause=1, for_statement=1 → 1+3=4
        assert_eq!(get_complexity(src, LangId::Python, "foo"), Some(4));
    }

    #[test]
    fn ts_complexity() {
        let src = r#"
function foo() {
    if (x) {
        for (let i = 0; i < 10; i++) {}
    }
    const y = x ? 1 : 2;
}
"#;
        // if_statement=1, for_statement=1, ternary_expression=1 → 1+3=4
        assert_eq!(get_complexity(src, LangId::Typescript, "foo"), Some(4));
    }

    #[test]
    fn struct_has_no_complexity() {
        // struct にはcomplexity が付かない
        assert_eq!(get_complexity("struct Foo {}", LangId::Rust, "Foo"), None);
    }

    // --- is_override_method テスト ---

    fn check_override(source: &str, lang_id: LangId, symbol_name: &str) -> bool {
        let language = lang_id.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, source.as_bytes(), lang_id).unwrap();
        let sym = syms
            .iter()
            .find(|s| s.name == symbol_name)
            .unwrap_or_else(|| {
                panic!(
                    "symbol '{}' not found in {:?}",
                    symbol_name,
                    syms.iter().map(|s| &s.name).collect::<Vec<_>>()
                )
            });
        is_override_method(root, source.as_bytes(), lang_id, &sym.range)
    }

    #[test]
    fn kotlin_override_is_detected() {
        let src = "class A : B() { override fun foo() {} }";
        assert!(check_override(src, LangId::Kotlin, "foo"));
    }

    #[test]
    fn kotlin_plain_function_is_not_override() {
        let src = "class A { fun foo() {} }";
        assert!(!check_override(src, LangId::Kotlin, "foo"));
    }

    #[test]
    fn kotlin_override_in_object_expression_is_detected() {
        // 匿名 object 内の override も検出できること
        let src = r#"class A {
    fun setup() {
        val w = object : TextWatcher {
            override fun afterTextChanged(s: Editable?) {}
        }
    }
}"#;
        assert!(check_override(src, LangId::Kotlin, "afterTextChanged"));
    }

    #[test]
    fn java_override_annotation_is_detected() {
        let src = r#"class A extends B {
    @Override
    public void foo() {}
}"#;
        assert!(check_override(src, LangId::Java, "foo"));
    }

    #[test]
    fn java_plain_method_is_not_override() {
        let src = "class A { public void foo() {} }";
        assert!(!check_override(src, LangId::Java, "foo"));
    }

    #[test]
    fn contains_keyword_respects_word_boundaries() {
        // `overrider` は `override` キーワードに誤マッチしないこと
        assert!(!super::contains_keyword("public overrider", "override"));
        assert!(super::contains_keyword("public override fun", "override"));
        assert!(super::contains_keyword("override", "override"));
    }

    // --- has_framework_entrypoint_decorator_python テスト ---

    fn check_python_framework_decorator(source: &str, symbol_name: &str) -> bool {
        let language = LangId::Python.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, source.as_bytes(), LangId::Python).unwrap();
        let sym = syms
            .iter()
            .find(|s| s.name == symbol_name)
            .unwrap_or_else(|| {
                panic!(
                    "symbol '{}' not found in {:?}",
                    symbol_name,
                    syms.iter().map(|s| &s.name).collect::<Vec<_>>()
                )
            });
        has_framework_entrypoint_decorator_python(root, source.as_bytes(), &sym.range)
    }

    #[test]
    fn python_typer_command_decorator_detected() {
        // Issue 報告ケース: Typer の @app.command(...) で装飾された関数
        let src = r#"
import typer
app = typer.Typer()

@app.command("list")
def list_tokens():
    pass
"#;
        assert!(check_python_framework_decorator(src, "list_tokens"));
    }

    #[test]
    fn python_typer_callback_decorator_detected() {
        let src = r#"
@app.callback()
def main():
    pass
"#;
        assert!(check_python_framework_decorator(src, "main"));
    }

    #[test]
    fn python_click_command_decorator_detected() {
        let src = r#"
import click

@click.command()
def hello():
    pass
"#;
        assert!(check_python_framework_decorator(src, "hello"));
    }

    #[test]
    fn python_click_group_subcommand_detected() {
        let src = r#"
@cli.command()
def sync():
    pass
"#;
        assert!(check_python_framework_decorator(src, "sync"));
    }

    #[test]
    fn python_fastapi_route_detected() {
        let src = r#"
@app.get("/items/{item_id}")
def read_item(item_id: int):
    return {"item_id": item_id}
"#;
        assert!(check_python_framework_decorator(src, "read_item"));
    }

    #[test]
    fn python_fastapi_router_post_detected() {
        let src = r#"
@router.post("/users/")
def create_user(user: User):
    return user
"#;
        assert!(check_python_framework_decorator(src, "create_user"));
    }

    #[test]
    fn python_flask_route_detected() {
        let src = r#"
@app.route("/")
def index():
    return "Hello"
"#;
        assert!(check_python_framework_decorator(src, "index"));
    }

    #[test]
    fn python_flask_blueprint_route_detected() {
        let src = r#"
@bp.route("/foo")
def foo():
    return "foo"
"#;
        assert!(check_python_framework_decorator(src, "foo"));
    }

    #[test]
    fn python_pytest_fixture_detected() {
        let src = r#"
import pytest

@pytest.fixture
def db_session():
    return None
"#;
        assert!(check_python_framework_decorator(src, "db_session"));
    }

    #[test]
    fn python_pytest_mark_parametrize_detected() {
        let src = r#"
import pytest

@pytest.mark.parametrize("x", [1, 2])
def test_x(x):
    assert x > 0
"#;
        assert!(check_python_framework_decorator(src, "test_x"));
    }

    #[test]
    fn python_celery_task_detected() {
        let src = r#"
@app.task
def send_email(to):
    pass
"#;
        assert!(check_python_framework_decorator(src, "send_email"));
    }

    #[test]
    fn python_django_receiver_detected() {
        let src = r#"
from django.dispatch import receiver

@receiver(post_save, sender=User)
def on_user_save(sender, instance, **kwargs):
    pass
"#;
        assert!(check_python_framework_decorator(src, "on_user_save"));
    }

    #[test]
    fn python_django_login_required_detected() {
        let src = r#"
from django.contrib.auth.decorators import login_required

@login_required
def my_view(request):
    return None
"#;
        assert!(check_python_framework_decorator(src, "my_view"));
    }

    #[test]
    fn python_dataclass_decorator_not_detected() {
        // @dataclass はフレームワーク登録ではないので除外しない
        let src = r#"
from dataclasses import dataclass

@dataclass
class Point:
    x: int
    y: int
"#;
        assert!(!check_python_framework_decorator(src, "Point"));
    }

    #[test]
    fn python_property_decorator_not_detected() {
        let src = r#"
class Foo:
    @property
    def name(self):
        return self._name
"#;
        assert!(!check_python_framework_decorator(src, "name"));
    }

    #[test]
    fn python_classmethod_decorator_not_detected() {
        let src = r#"
class Foo:
    @classmethod
    def create(cls):
        return cls()
"#;
        assert!(!check_python_framework_decorator(src, "create"));
    }

    #[test]
    fn python_undecorated_function_not_detected() {
        let src = r#"
def helper():
    return None
"#;
        assert!(!check_python_framework_decorator(src, "helper"));
    }

    #[test]
    fn python_decorated_method_in_class_detected() {
        // クラス内のメソッドでも @app.command が認識できること
        let src = r#"
class Cli:
    @app.command("show")
    def show(self):
        pass
"#;
        assert!(check_python_framework_decorator(src, "show"));
    }

    /// `python_class_base_names` がクラスの直接 base を identifier / attribute 形式で
    /// 抽出できることを検証する (`unittest.TestCase` 等)。
    #[test]
    fn python_class_base_names_returns_identifier_and_attribute() {
        let src = "import unittest\nclass Foo(unittest.TestCase, BaseMixin):\n    pass\n";
        let language = LangId::Python.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, src.as_bytes(), LangId::Python).unwrap();
        let foo = syms.iter().find(|s| s.name == "Foo").expect("class Foo");
        let bases = python_class_base_names(root, src.as_bytes(), &foo.range);
        assert_eq!(
            bases,
            vec!["unittest.TestCase".to_string(), "BaseMixin".to_string()]
        );
    }

    /// クラスが base を持たない場合は空リストを返すことを検証する。
    #[test]
    fn python_class_base_names_empty_when_no_base() {
        let src = "class Foo:\n    pass\n";
        let language = LangId::Python.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, src.as_bytes(), LangId::Python).unwrap();
        let foo = syms.iter().find(|s| s.name == "Foo").expect("class Foo");
        let bases = python_class_base_names(root, src.as_bytes(), &foo.range);
        assert!(bases.is_empty(), "no base => empty: {bases:?}");
    }

    /// `metaclass=...` のようなキーワード引数は base に含まれないことを検証する。
    #[test]
    fn python_class_base_names_skips_keyword_arguments() {
        let src = "class Foo(Bar, metaclass=ABCMeta):\n    pass\n";
        let language = LangId::Python.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, src.as_bytes(), LangId::Python).unwrap();
        let foo = syms.iter().find(|s| s.name == "Foo").expect("class Foo");
        let bases = python_class_base_names(root, src.as_bytes(), &foo.range);
        assert_eq!(bases, vec!["Bar".to_string()]);
    }
}
