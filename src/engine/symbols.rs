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
        LangId::Swift => is_exported_swift(node, source),
        // C++ はクラスメソッドの可視性 (public/protected/private) を判定する。
        // 非公開メソッドの変更を API 差分の偽陽性にしないため。自由関数・クラス外
        // 定義は public 扱い。
        LangId::Cpp => is_exported_cpp(node, source),
        _ => true, // 未対応言語 (C 等) は保守的にエクスポートと判定
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

/// Swift: 外部公開 API は `public` / `open` 明示宣言のみ。デフォルト (internal) や
/// `private` / `fileprivate` は同一モジュール内のみ可視で外部 API ではないため非公開扱い。
/// これにより sidecar / executable に同梱される internal 型 (例: `enum DetectionError`) を
/// api 差分に出さない (Issue 2026-05-29-swift-sidecar-api-mod パターンD)。
fn is_exported_swift(node: Node, source: &[u8]) -> bool {
    let Some(decl) = find_enclosing_declaration(node) else {
        return true; // 保守的: 宣言未検出はエクスポート扱い
    };
    match swift_explicit_visibility(decl, source) {
        Some(is_pub) => is_pub,
        // 明示修飾子なし (デフォルト internal)。ただし public extension のメンバは
        // デフォルト public なので、コンテナが public extension のときだけ公開扱いにする。
        None => swift_default_member_is_public(decl, source),
    }
}

/// Swift 宣言ノードの明示 visibility_modifier を返す。public/open → Some(true)、
/// private/fileprivate/internal → Some(false)、明示なし → None。
fn swift_explicit_visibility(decl: Node, source: &[u8]) -> Option<bool> {
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mc = child.walk();
            for m in child.children(&mut mc) {
                if m.kind() == "visibility_modifier"
                    && let Ok(text) = m.utf8_text(source)
                {
                    let vis = text.trim();
                    return Some(vis == "public" || vis == "open");
                }
            }
        }
    }
    None
}

/// 明示修飾子のない Swift メンバが public 既定となるコンテナ (public/open extension) 配下かを
/// 判定する。public extension のメンバはデフォルト public。struct/class/enum 配下や top-level は
/// internal 既定なので false。protocol requirement は find_enclosing_declaration が
/// protocol_declaration を直接返すため、本関数ではなく明示 visibility 経路で判定される。
fn swift_default_member_is_public(decl: Node, source: &[u8]) -> bool {
    let mut current = decl.parent();
    while let Some(n) = current {
        match n.kind() {
            // extension は class_declaration として現れる ("extension" キーワードで判別)。
            "class_declaration" if swift_is_extension(n) => {
                return swift_explicit_visibility(n, source) == Some(true);
            }
            // 通常の struct/class/enum はメンバにデフォルト可視性を継承しない (internal)。
            "class_declaration" | "enum_declaration" => return false,
            _ => {}
        }
        current = n.parent();
    }
    false
}

/// class_declaration が extension (`extension Foo {}`) かを判定する。
fn swift_is_extension(class_decl: Node) -> bool {
    let mut cursor = class_decl.walk();
    class_decl
        .children(&mut cursor)
        .any(|c| c.kind() == "extension")
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
        // Swift protocol
        "protocol_declaration",
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
///
/// TypeScript の class member は accessibility_modifier (`private` / `protected`) を
/// 持てる。`private` 修飾されたメソッド / フィールド、および ECMAScript `#private`
/// メンバ (名前が `#` で始まる private_property_identifier) は exported ではない。
/// (Issue: 2026-05-22-temperature-api-triage の TS private クラスメンバ誤検出)
/// `protected` は当面保守的に true を返す (TODO: protected を別カテゴリで扱う)。
fn is_exported_js_ts(node: Node, source: &[u8], root: Node) -> bool {
    // TypeScript class の private member は外部公開 API ではない (Issue B)。
    if is_private_class_member_js_ts(node, source) {
        return false;
    }

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

/// TypeScript / JS の class member ノードが `private` であるかを判定する。
///
/// 判定対象:
/// - `accessibility_modifier` 子が "private" の `method_definition` / `public_field_definition`
/// - 名前が `private_property_identifier` (= ECMAScript `#private`) のメンバー
///
/// 関数本体の境界では停止し、内部関数の中のシンボルは判定対象外とする。
/// `protected` は意図的に対象外 (保守的に exported 扱い)。
fn is_private_class_member_js_ts(node: Node, source: &[u8]) -> bool {
    let mut current = Some(node);
    while let Some(n) = current {
        // 関数本体の境界で停止 — 内部関数のローカルは class member ではない
        if is_js_function_body(n) {
            return false;
        }
        if matches!(n.kind(), "method_definition" | "public_field_definition") {
            // accessibility_modifier "private" の有無を確認
            let mut cursor = n.walk();
            for child in n.children(&mut cursor) {
                if child.kind() == "accessibility_modifier"
                    && let Ok(text) = child.utf8_text(source)
                    && text.trim() == "private"
                {
                    return true;
                }
            }
            // ECMAScript `#private`: name が private_property_identifier
            if let Some(name) = n.child_by_field_name("name")
                && name.kind() == "private_property_identifier"
            {
                return true;
            }
            // class member 直下で確定 (上位にはこれ以上関連情報はない)
            return false;
        }
        current = n.parent();
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
        // `export { foo } from "./b"` は別モジュールの forwarding (re-export) であり、
        // このファイルでの foo のローカル定義 export ではない。from 句 (source) があれば
        // ローカル export 判定の対象外とする。
        if child.child_by_field_name("source").is_some() {
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

/// TS/JS の named re-export (`export { foo } from "..."` /
/// `export { foo as bar } from "..."`) で **このファイルから提供される export 名** の
/// 集合を返す (alias があれば alias 名)。`export * from "..."` (wildcard) は名前が静的に
/// 不明なため対象外。
///
/// api.rm 抑制に使う: あるシンボルがローカル定義から re-export に置き換わっても、
/// 利用者から見た export 面 (import path から取れる名前) は維持されているため
/// 「削除」ではない。
pub(crate) fn collect_reexported_names(
    root: Node,
    source: &[u8],
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "export_statement" {
            continue;
        }
        // from 句 (source) がある export のみ re-export。
        if child.child_by_field_name("source").is_none() {
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
                // exported 名: alias (`as bar`) があれば alias、なければ name。
                let exported = spec
                    .child_by_field_name("alias")
                    .or_else(|| spec.child_by_field_name("name"));
                if let Some(n) = exported
                    && let Ok(text) = n.utf8_text(source)
                {
                    names.insert(text.to_string());
                }
            }
        }
    }
    names
}

/// Rust の `pub use` 再エクスポート (`pub use sub::name;` / `pub use sub::{A, B};` /
/// `pub use sub::name as alias;`) で **このファイルから提供される export 名** の集合を返す
/// (alias があれば alias 名)。`pub use sub::*;` (wildcard) は名前が静的に不明なため対象外。
/// `pub(crate)` / `pub(super)` 等の制限付き公開は外部利用者から見えないため対象外。
///
/// api.rm 抑制に使う: TS/JS の named re-export と同じ思想で、定義を子モジュールへ移動して
/// 親モジュールで `pub use sub::name;` を残しているケースは、利用者から見た公開 API
/// (`crate::parent::name`) が維持されているため「削除」ではない。
pub(crate) fn collect_rust_reexported_names(
    root: Node,
    source: &[u8],
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "use_declaration" {
            continue;
        }
        // pub のみ (pub(crate) / pub(super) 等は外部公開ではない)
        if !rust_use_is_public(child, source) {
            continue;
        }
        let Some(argument) = child.child_by_field_name("argument") else {
            continue;
        };
        collect_rust_use_tree_names(argument, source, &mut names);
    }
    names
}

/// `pub use ...` (修飾子なしの `pub`) かを判定する。`pub(crate)` 等は内部公開で対象外。
fn rust_use_is_public(use_decl: Node, source: &[u8]) -> bool {
    let mut cursor = use_decl.walk();
    for child in use_decl.children(&mut cursor) {
        if child.kind() == "visibility_modifier"
            && let Ok(text) = child.utf8_text(source)
        {
            return text.trim() == "pub";
        }
    }
    false
}

/// use_tree の各 variant を歩いて、最終的に外部に露出される名前 (末端 identifier または
/// `as alias` の alias 名) を集める。
fn collect_rust_use_tree_names(
    node: Node,
    source: &[u8],
    names: &mut std::collections::HashSet<String>,
) {
    match node.kind() {
        // `pub use foo::bar;` → bar が公開される
        "scoped_identifier" => {
            if let Some(name) = node.child_by_field_name("name")
                && let Ok(text) = name.utf8_text(source)
            {
                names.insert(text.to_string());
            }
        }
        // `pub use foo;` → foo が公開される
        "identifier" => {
            if let Ok(text) = node.utf8_text(source) {
                names.insert(text.to_string());
            }
        }
        // `pub use foo::bar as baz;` → baz (alias) が公開される
        "use_as_clause" => {
            if let Some(alias) = node.child_by_field_name("alias")
                && let Ok(text) = alias.utf8_text(source)
            {
                names.insert(text.to_string());
            }
        }
        // `pub use foo::{A, B as C};` — list 配下の各 item を再帰的に走査
        "scoped_use_list" => {
            if let Some(list) = node.child_by_field_name("list") {
                let mut cursor = list.walk();
                for child in list.children(&mut cursor) {
                    collect_rust_use_tree_names(child, source, names);
                }
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_rust_use_tree_names(child, source, names);
            }
        }
        // `pub use foo::*;` — 名前が静的に解決できないため fail-open を避けて対象外
        "use_wildcard" => {}
        _ => {}
    }
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

/// Java の Flyway Java マイグレーションクラスを判定する。
///
/// 判定基準: クラスの **直接の親型 (extends 句の 1 つ目 / implements 句のそれぞれ)** が
/// 以下のどちらかに一致するとき true を返す:
///
/// - 単純名: `BaseJavaMigration` / `JavaMigration` (import 経由のローカル参照)
/// - 完全修飾名: `org.flywaydb.core.api.migration.BaseJavaMigration` /
///   `org.flywaydb.core.api.migration.JavaMigration` (import なしの直接参照)
///
/// `extends com.example.BaseJavaMigration` のような別 package の同名クラスは
/// 完全修飾名が一致しないため false (codex 指摘)。`implements Wrapper<JavaMigration>` の
/// ような generic 型引数も「直接の親型」ではないため false (型引数を再帰走査しない)。
///
/// Flyway はクラスパス走査 + リフレクションで migration を発見・実行するため、
/// アプリコード上の直接参照は存在せず、dead-code / API 変更検出の両方で false positive
/// 源になる。`db/migration/V*__*.java` 命名規約は補助的シグナルだが、ここでは継承関係を
/// 主な判定基準にする (命名だけで除外すると同名の業務クラスを巻き込みやすい)。
/// GitLab issue #24 対応。
pub fn is_java_flyway_migration_class(root: Node, source: &[u8], symbol_range: &Range) -> bool {
    const FLYWAY_SIMPLE: &[&str] = &["BaseJavaMigration", "JavaMigration"];
    const FLYWAY_FQN: &[&str] = &[
        "org.flywaydb.core.api.migration.BaseJavaMigration",
        "org.flywaydb.core.api.migration.JavaMigration",
    ];

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

    // class_declaration ノードを探す (symbol_range 内 or 祖先)。
    let mut cur = Some(node);
    let class_node = loop {
        match cur {
            Some(n) if n.kind() == "class_declaration" => break n,
            Some(n) => cur = n.parent(),
            None => return false,
        }
    };

    // superclass (`extends X`) と super_interfaces (`implements X, Y`) を確認。
    // 直接の親型ノードだけ (generic_type の base type、型引数は見ない) を集めて判定する。
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        match child.kind() {
            "superclass" => {
                if java_direct_supertype_matches(child, source, FLYWAY_SIMPLE, FLYWAY_FQN) {
                    return true;
                }
            }
            "super_interfaces" => {
                // tree-sitter-java の `super_interfaces` は `implements` + `type_list` で構成
                // される (`interface_type_list` ではない)。`implements A, B, C` は `type_list`
                // の各子要素が直接の親型。
                let mut inner = child.walk();
                for grand in child.children(&mut inner) {
                    if grand.kind() == "type_list"
                        && java_direct_supertype_matches(grand, source, FLYWAY_SIMPLE, FLYWAY_FQN)
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// 親型節 (`superclass` / `interface_type_list`) の **直接の子要素** から「直接の親型」を
/// 取り出し、Flyway の simple / FQN いずれかに一致するか判定する。型引数 (`generic_type`
/// の `type_arguments`) は親型ではないため見ない。
///
/// 直接の親型ノードは次のいずれか:
///   - `type_identifier`: 単純名 (例: `BaseJavaMigration`)
///   - `scoped_type_identifier`: 完全修飾名 (例: `org.flywaydb.core.api.migration.BaseJavaMigration`)
///   - `generic_type`: ジェネリクス付き親型。base type 部分のみ評価。
fn java_direct_supertype_matches(
    container: Node,
    source: &[u8],
    simple_names: &[&str],
    fqn_names: &[&str],
) -> bool {
    let mut cursor = container.walk();
    for child in container.children(&mut cursor) {
        if java_type_node_matches(child, source, simple_names, fqn_names) {
            return true;
        }
    }
    false
}

/// 「型を表すノード」が指定の simple / FQN にマッチするか判定する。
/// `generic_type` は最初の子 (= base type) を辿って評価する。
/// `scoped_type_identifier` は AST 上の識別子セグメントを `.` で連結したものを比較
/// (raw text ではなく構造ベース) — qualified name の途中に改行・コメント・空白が
/// 挟まれた valid Java でも検出漏れしないようにする。
fn java_type_node_matches(
    type_node: Node,
    source: &[u8],
    simple_names: &[&str],
    fqn_names: &[&str],
) -> bool {
    match type_node.kind() {
        "type_identifier" => type_node
            .utf8_text(source)
            .ok()
            .is_some_and(|text| simple_names.contains(&text)),
        "scoped_type_identifier" => {
            // 再帰的に scoped_type_identifier / type_identifier を辿り、識別子セグメントを
            // `.` で連結する。これにより `org.flywaydb...\n BaseJavaMigration` のような
            // 空白入りの valid Java も `org.flywaydb...BaseJavaMigration` として比較できる。
            let mut segments: Vec<&str> = Vec::new();
            if java_collect_scoped_type_segments(type_node, source, &mut segments) {
                let joined = segments.join(".");
                fqn_names.iter().any(|fqn| joined == *fqn)
            } else {
                false
            }
        }
        "generic_type" => {
            // base type は通常先頭の named child。`A<B>` の `A` 部分。
            let mut cursor = type_node.walk();
            for child in type_node.children(&mut cursor) {
                if matches!(child.kind(), "type_identifier" | "scoped_type_identifier") {
                    return java_type_node_matches(child, source, simple_names, fqn_names);
                }
            }
            false
        }
        _ => false,
    }
}

/// `scoped_type_identifier` を再帰的に辿り、各識別子セグメントを `segments` に push する。
/// scoped_type_identifier の構造: `[scoped_type_identifier|scope] "." type_identifier`。
/// 識別子以外の text (`.` トークンや空白) を比較対象から除くために用いる。
fn java_collect_scoped_type_segments<'a>(
    node: Node<'a>,
    source: &'a [u8],
    segments: &mut Vec<&'a str>,
) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "scoped_type_identifier" | "scope" => {
                if !java_collect_scoped_type_segments(child, source, segments) {
                    return false;
                }
            }
            "type_identifier" | "identifier" => match child.utf8_text(source) {
                Ok(text) => segments.push(text),
                Err(_) => return false,
            },
            _ => {}
        }
    }
    !segments.is_empty()
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
/// JS/TS で symbol が「フレームワーク DSL 関数の引数オブジェクトのプロパティ
/// (method shorthand または key:function pair)」かを判定する。該当なら
/// dead-code から除外する (Issue 2026-05-14-wxt-defineContentScript-main)。
///
/// 対象 DSL (allowlist):
///   - WXT: `defineContentScript`, `defineBackground`
///   - Vue: `defineComponent`
///   - Vite/Vitest: `defineConfig`
///   - Nuxt: `defineNuxtConfig`
///
/// 検出パターン (method shorthand):
///   ```text
///   call_expression
///     function: identifier "defineContentScript"
///     arguments: arguments
///       object: object
///         method_definition (= main() { ... })  ← symbol
///   ```
///
/// 検出パターン (pair value):
///   ```text
///   call_expression
///     function: identifier "defineComponent"
///     arguments: arguments
///       object: object
///         pair
///           key: property_identifier "setup"
///           value: arrow_function | function_expression  ← symbol
///   ```
///
/// 直接の親チェーンで判定するため、DSL の callback 内部で定義された関数
/// (本来 dead 判定したい helper) は誤って除外しない。
pub fn is_js_ts_framework_dsl_callback(root: Node, source: &[u8], symbol_range: &Range) -> bool {
    const FRAMEWORK_DSL_CALLEES: &[&str] = &[
        "defineContentScript",
        "defineBackground",
        "defineComponent",
        "defineConfig",
        "defineNuxtConfig",
    ];

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

    // symbol を含む最も近い function-like ノードを探す。
    let mut cur = node;
    let outer = loop {
        match cur.kind() {
            "method_definition"
            | "arrow_function"
            | "function_expression"
            | "function_declaration" => break cur,
            _ => {}
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    };

    // outer の親チェーンを辿って `object → arguments → call_expression` を確認する。
    // method shorthand: method_definition の親が object
    // pair value:        arrow/function_expression の親が pair → object
    let object = match outer.parent() {
        Some(p) if p.kind() == "object" => p,
        Some(p) if p.kind() == "pair" => match p.parent() {
            Some(o) if o.kind() == "object" => o,
            _ => return false,
        },
        _ => return false,
    };
    let Some(arguments) = object.parent().filter(|p| p.kind() == "arguments") else {
        return false;
    };
    let Some(call_expression) = arguments.parent().filter(|p| p.kind() == "call_expression") else {
        return false;
    };
    let Some(callee) = call_expression.child_by_field_name("function") else {
        return false;
    };
    let Ok(text) = callee.utf8_text(source) else {
        return false;
    };
    FRAMEWORK_DSL_CALLEES.contains(&text)
}

/// Angular の class method lifecycle hook 名集合 (Angular v17+ 公式 docs 準拠)。
///
/// これらは Angular ランタイムが change detection サイクル等で自動呼出するため、
/// ユーザコード側に直接の caller が静的解析で見つからないのが正常。
/// `implements OnInit` 等の interface 実装は Angular の呼出規約では不要なため
/// 判定材料にしない (Angular はメソッド名 + class decorator で hook を解決する)。
///
/// `afterNextRender` / `afterEveryRender` は standalone callback API で
/// クラスメソッドの lifecycle hook ではないため対象外。
const ANGULAR_LIFECYCLE_HOOKS: &[&str] = &[
    "ngOnChanges",
    "ngOnInit",
    "ngDoCheck",
    "ngAfterContentInit",
    "ngAfterContentChecked",
    "ngAfterViewInit",
    "ngAfterViewChecked",
    "ngOnDestroy",
];

/// `symbol_range` の method が Angular `@Component` / `@Directive` 装飾クラスの
/// lifecycle hook かを判定する。
///
/// 判定:
/// 1. メソッド名が [`ANGULAR_LIFECYCLE_HOOKS`] のいずれかに一致
/// 2. enclosing `class_declaration` に `@Component` または `@Directive` decorator が付与されている
///
/// dead-code 検出側で `exclude_framework_entrypoints == true` のとき除外対象に使う想定。
pub fn is_js_ts_angular_lifecycle_hook(root: Node, source: &[u8], symbol_range: &Range) -> bool {
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

    // method_definition を探す
    let mut cur = node;
    let method_node = loop {
        if cur.kind() == "method_definition" {
            break cur;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    };

    // メソッド名チェック
    let Some(name_node) = method_node.child_by_field_name("name") else {
        return false;
    };
    let Ok(name) = name_node.utf8_text(source) else {
        return false;
    };
    if !ANGULAR_LIFECYCLE_HOOKS.contains(&name) {
        return false;
    }

    // enclosing class_declaration を探し、@Component / @Directive decorator を確認
    let mut cur = method_node;
    while let Some(parent) = cur.parent() {
        if matches!(
            parent.kind(),
            "class_declaration" | "abstract_class_declaration"
        ) {
            return class_has_component_or_directive_decorator(parent, source);
        }
        cur = parent;
    }
    false
}

/// `class_declaration` ノードに `@Component` / `@Directive` decorator が付与されているかを判定する。
///
/// tree-sitter-typescript の AST では decorator は class_declaration の sibling として
/// **直前** に並ぶ (export 文の中では export_statement の子)。class_declaration の親を
/// 走査して周辺の decorator ノードを確認する。
fn class_has_component_or_directive_decorator(class_node: Node, source: &[u8]) -> bool {
    const ANGULAR_DECORATORS: &[&str] = &["Component", "Directive"];

    // class_declaration の前方兄弟と export_statement 経由の decorator の両方を見る
    let containers: [Node; 2] = match class_node.parent() {
        Some(parent) => [parent, class_node],
        None => [class_node, class_node],
    };
    for container in &containers {
        let mut cursor = container.walk();
        for child in container.children(&mut cursor) {
            if child.kind() != "decorator" {
                continue;
            }
            // decorator の中身: `@Foo(...)` の `Foo` 部分が identifier として現れる
            let mut dcursor = child.walk();
            for dchild in child.children(&mut dcursor) {
                // call_expression / identifier いずれにも対応
                match dchild.kind() {
                    "identifier" => {
                        if let Ok(name) = dchild.utf8_text(source)
                            && ANGULAR_DECORATORS.contains(&name)
                        {
                            return true;
                        }
                    }
                    "call_expression" => {
                        if let Some(callee) = dchild.child_by_field_name("function")
                            && let Ok(name) = callee.utf8_text(source)
                            && ANGULAR_DECORATORS.contains(&name)
                        {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    false
}

/// Angular の `ControlValueAccessor` 規約メソッド名。`@Component` / `@Directive` で装飾
/// された CVA 実装クラスに含まれるとき、Angular Forms ランタイムが NG_VALUE_ACCESSOR
/// provider 経由で `ngModel` / `formControl` バインド時に呼び出すため、静的 caller が
/// 0 件でも dead ではない (GitLab #20)。`writeValue` / `registerOnChange` /
/// `registerOnTouched` / `setDisabledState` の 4 メソッドを対象とする。
const ANGULAR_CVA_METHODS: &[&str] = &[
    "writeValue",
    "registerOnChange",
    "registerOnTouched",
    "setDisabledState",
];

/// メンバー単位の Angular runtime entrypoint デコレータ名。これらが property / accessor /
/// method に付与されているとき、Angular ランタイムが change detection / event binding /
/// query 解決経由で呼ぶ・読む・書くため、静的 caller が 0 件でも dead ではない (GitLab #23)。
///
/// 対象:
/// - イベント / バインディング: `@HostListener`, `@HostBinding`
/// - input / output: `@Input`, `@Output`
/// - view / content query: `@ViewChild`, `@ViewChildren`, `@ContentChild`, `@ContentChildren`
const ANGULAR_MEMBER_RUNTIME_DECORATORS: &[&str] = &[
    "HostListener",
    "HostBinding",
    "Input",
    "Output",
    "ViewChild",
    "ViewChildren",
    "ContentChild",
    "ContentChildren",
];

/// `symbol_range` が Angular ランタイムから呼び出される member かを判定する。
/// `is_js_ts_angular_lifecycle_hook` の上位互換で、`@Component` / `@Directive` 装飾クラスの
/// 以下の member を `true` とする (静的 caller 0 件でも dead 候補から除外する想定):
///
/// 1. 既存: lifecycle hook 名 (`ngOnInit` 等)
/// 2. (GitLab #20) `ControlValueAccessor` 規約メソッド (`writeValue` / `registerOnChange` /
///    `registerOnTouched` / `setDisabledState`)。CVA を `implements ControlValueAccessor` で
///    宣言しているか、または同じ意味の `NG_VALUE_ACCESSOR` provider を decorator metadata に
///    持つクラスのみ対象。
/// 3. (GitLab #23) member 単位の Angular decorator (`@HostListener` / `@HostBinding` /
///    `@Input` / `@Output` / `@ViewChild` / `@ViewChildren` / `@ContentChild` /
///    `@ContentChildren`) が付与された property / accessor / method。
pub fn is_js_ts_angular_runtime_entrypoint(
    root: Node,
    source: &[u8],
    symbol_range: &Range,
) -> bool {
    if is_js_ts_angular_lifecycle_hook(root, source, symbol_range) {
        return true;
    }
    if is_js_ts_angular_member_decorator_target(root, source, symbol_range) {
        return true;
    }
    if is_js_ts_angular_cva_contract_method(root, source, symbol_range) {
        return true;
    }
    false
}

/// `symbol_range` の member (property / accessor / method) に Angular の member 単位
/// runtime decorator (`@HostListener` 等) が付与されているか判定する。
/// `@Component` / `@Directive` 装飾クラス配下のメンバーのみ対象とする (誤検出回避)。
fn is_js_ts_angular_member_decorator_target(
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

    // member ノード (method_definition / public_field_definition 等) を探す。
    // tree-sitter-typescript では @Input() x: string などは `public_field_definition`、
    // @HostListener(...) f() {} などは `method_definition` として表現される。
    let member_kinds = [
        "method_definition",
        "public_field_definition",
        "field_definition",
        "property_signature",
    ];
    let mut cur = Some(node);
    let member_node = loop {
        match cur {
            Some(n) if member_kinds.contains(&n.kind()) => break n,
            Some(n) => cur = n.parent(),
            None => return false,
        }
    };

    // 親が `class_body` で、その先祖 `class_declaration` / `abstract_class_declaration` が
    // Angular 装飾されているか確認 (export abstract class も abstract_class_declaration になる)。
    let mut class_cur = member_node;
    let class_node = loop {
        match class_cur.parent() {
            Some(p) if matches!(p.kind(), "class_declaration" | "abstract_class_declaration") => {
                break p;
            }
            Some(p) => class_cur = p,
            None => return false,
        }
    };
    if !class_has_component_or_directive_decorator(class_node, source) {
        return false;
    }

    // member 自身に decorator が付いているか確認。
    // tree-sitter-typescript では member の直前の child として decorator が並ぶ。
    let parent = member_node.parent().unwrap_or(member_node);
    let mut cursor = parent.walk();
    let mut last_decorator_names: Vec<String> = Vec::new();
    for child in parent.children(&mut cursor) {
        if child.kind() == "decorator" {
            if let Some(name) = decorator_call_name(child, source) {
                last_decorator_names.push(name);
            }
        } else if child.id() == member_node.id() {
            if last_decorator_names
                .iter()
                .any(|n| ANGULAR_MEMBER_RUNTIME_DECORATORS.contains(&n.as_str()))
            {
                return true;
            }
            last_decorator_names.clear();
        } else {
            last_decorator_names.clear();
        }
    }
    false
}

/// decorator ノードから `@Foo(...)` の `Foo` 部分の識別子名を取り出す。
fn decorator_call_name(decorator: Node, source: &[u8]) -> Option<String> {
    let mut cursor = decorator.walk();
    for child in decorator.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                if let Ok(t) = child.utf8_text(source) {
                    return Some(t.to_string());
                }
            }
            "call_expression" => {
                if let Some(callee) = child.child_by_field_name("function")
                    && let Ok(t) = callee.utf8_text(source)
                {
                    return Some(t.to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// `symbol_range` の method が Angular CVA 規約メソッド (`writeValue` 等) かつ enclosing
/// クラスが ControlValueAccessor を実装しているか判定する。
///
/// 「実装」の判定:
/// - `implements ControlValueAccessor` (TS interface 実装節)、または
/// - `@Component({providers: [{provide: NG_VALUE_ACCESSOR, ...}]})` のような provider 登録
///   (decorator メタデータ内に `NG_VALUE_ACCESSOR` 識別子が含まれる)
///
/// どちらも構文上の手がかりを使う (cross-file 解析や型推論は行わない)。GitLab #20 対応。
fn is_js_ts_angular_cva_contract_method(root: Node, source: &[u8], symbol_range: &Range) -> bool {
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

    // method_definition を探す。
    let mut cur = node;
    let method_node = loop {
        if cur.kind() == "method_definition" {
            break cur;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    };
    let Some(name_node) = method_node.child_by_field_name("name") else {
        return false;
    };
    let Ok(name) = name_node.utf8_text(source) else {
        return false;
    };
    if !ANGULAR_CVA_METHODS.contains(&name) {
        return false;
    }

    // enclosing class を探し、`implements ControlValueAccessor` または NG_VALUE_ACCESSOR
    // provider 登録があるか確認。
    let mut cur = method_node;
    while let Some(parent) = cur.parent() {
        if matches!(
            parent.kind(),
            "class_declaration" | "abstract_class_declaration"
        ) {
            return class_implements_control_value_accessor(parent, source)
                || class_has_ng_value_accessor_provider(parent, source);
        }
        cur = parent;
    }
    false
}

/// `class X implements ControlValueAccessor [...]` の implements 節を判定する。
/// `implements` リストに `ControlValueAccessor` 識別子が含まれていれば true。
fn class_implements_control_value_accessor(class_node: Node, source: &[u8]) -> bool {
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() != "class_heritage" {
            continue;
        }
        let mut inner = child.walk();
        for grand in child.children(&mut inner) {
            if grand.kind() != "implements_clause" {
                continue;
            }
            // implements_clause の中の type 識別子を順次見る。
            let mut g = grand.walk();
            for type_node in grand.children(&mut g) {
                if let Ok(t) = type_node.utf8_text(source)
                    && t.trim() == "ControlValueAccessor"
                {
                    return true;
                }
                // generic_type / scoped 等の場合は子の識別子も走査
                let mut tcur = type_node.walk();
                for tchild in type_node.children(&mut tcur) {
                    if let Ok(t) = tchild.utf8_text(source)
                        && t.trim() == "ControlValueAccessor"
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// `@Component({ providers: [{provide: NG_VALUE_ACCESSOR, ...}] })` のように decorator
/// メタデータ内に `NG_VALUE_ACCESSOR` 識別子が含まれているか判定する。tree-sitter で
/// 識別子トークン単位の出現を見るだけのざっくり判定 (string literal 内には現れないため
/// 誤検出は起こりにくい)。
fn class_has_ng_value_accessor_provider(class_node: Node, source: &[u8]) -> bool {
    let containers: [Node; 2] = match class_node.parent() {
        Some(parent) => [parent, class_node],
        None => [class_node, class_node],
    };
    for container in &containers {
        let mut cursor = container.walk();
        for child in container.children(&mut cursor) {
            if child.kind() != "decorator" {
                continue;
            }
            if node_contains_identifier(child, source, "NG_VALUE_ACCESSOR") {
                return true;
            }
        }
    }
    false
}

/// `node` 配下の任意の identifier ノードが `target` と一致するかを再帰的に探す。
fn node_contains_identifier(node: Node, source: &[u8], target: &str) -> bool {
    if node.kind() == "identifier"
        && let Ok(t) = node.utf8_text(source)
        && t == target
    {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if node_contains_identifier(child, source, target) {
            return true;
        }
    }
    false
}

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

/// PHP の擬似 enum (Java enum 風 static factory) パターンを判定する。
///
/// 擬似 enum とは次の形のメソッドを指す。Laravel / DDD 系プロジェクトで
/// `AbstractValueObjectString` を継承した値オブジェクトに大量に存在し、識別子レベルの
/// cross-file refs では caller が追跡できない (migration の文字列リテラル / DB 列値 /
/// reflection annotation 経由で利用される) ため dead 判定すると 100+ 件単位の
/// false positive が出る。
///
/// ```php
/// public static function FOO(): self {
///     return new self('FOO');
/// }
/// ```
///
/// 判定条件 (すべて満たす場合のみ true):
/// - PHP のメソッド宣言ノード
/// - `static` 修飾子付き
/// - 戻り値型注釈が `self` または `static`
/// - 本体に `return new self(<string literal>)` または `return new static(<string literal>)`
/// - 文字列引数のテキストがメソッド名と完全一致 (case-sensitive)
///
/// 第 4 引数 `enclosing_extends_value_object` は本関数の責務外の判定で、
/// 呼び出し側が「`extends AbstractValueObject*` のクラス所属」を確認した場合に
/// `true` を渡すと、より厳格な判定としてフィルタを通す (現状は判定本体には使わない
/// が、将来的により厳しい条件を追加する余地として API シグネチャに残す)。
pub fn is_php_pseudo_enum_method(
    root: Node,
    source: &[u8],
    symbol_range: &Range,
    method_name: &str,
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

    // method_declaration 祖先を取る
    let mut decl_node = None;
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "method_declaration" {
            decl_node = Some(n);
            break;
        }
        current = n.parent();
    }
    let Some(decl) = decl_node else {
        return false;
    };

    // static 修飾子チェック
    if !php_method_has_static_modifier(decl, source) {
        return false;
    }

    // 戻り値型注釈が self または static
    if !php_return_type_is_self_or_static(decl, source) {
        return false;
    }

    // 本体内で `return new self(method_name)` 相当を検出
    let Some(body) = decl.child_by_field_name("body") else {
        return false;
    };
    php_body_returns_new_self_with_name(body, source, method_name)
}

fn php_method_has_static_modifier(decl: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        // tree-sitter-php では static 修飾子は `static_modifier` として直接 child に出る。
        if child.kind() == "static_modifier" {
            return true;
        }
        // フォールバック: テキスト走査で `static` キーワードを含むかチェック (古いノード名対応)
        if child.kind() == "visibility_modifier" || child.kind() == "abstract_modifier" {
            continue;
        }
        if let Ok(text) = child.utf8_text(source)
            && text.split_whitespace().any(|tok| tok == "static")
        {
            return true;
        }
    }
    false
}

fn php_return_type_is_self_or_static(decl: Node<'_>, source: &[u8]) -> bool {
    // tree-sitter-php では戻り値型は `return_type` フィールドにある (named_type / primitive_type)
    let Some(rt) = decl.child_by_field_name("return_type") else {
        return false;
    };
    let Ok(text) = rt.utf8_text(source) else {
        return false;
    };
    let trimmed = text.trim();
    matches!(trimmed, "self" | "static")
}

/// 本体に `return new self('NAME')` または `return new static('NAME')` があり、
/// 引数の文字列リテラル内容が `method_name` と一致するかを判定する。
fn php_body_returns_new_self_with_name(body: Node<'_>, source: &[u8], method_name: &str) -> bool {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if php_node_returns_new_self_with_name(child, source, method_name) {
            return true;
        }
    }
    false
}

fn php_node_returns_new_self_with_name(node: Node<'_>, source: &[u8], method_name: &str) -> bool {
    // return_statement → object_creation_expression を探す
    if node.kind() == "return_statement" {
        // return の expression を取得
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "object_creation_expression"
                && php_object_creation_matches_name(child, source, method_name)
            {
                return true;
            }
        }
    }
    // 再帰: ネストした control flow (try/match/if) の中も探す
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if php_node_returns_new_self_with_name(child, source, method_name) {
            return true;
        }
    }
    false
}

fn php_object_creation_matches_name(node: Node<'_>, source: &[u8], method_name: &str) -> bool {
    // `new self('NAME')` の type 部分が self / static
    let type_node = node.child_by_field_name("type").or_else(|| {
        // tree-sitter-php のバージョンによっては type フィールドが付かないので
        // 子ノードから直接探す
        let mut cursor = node.walk();
        node.children(&mut cursor).find(|c| {
            matches!(
                c.kind(),
                "name" | "scoped_call_expression" | "qualified_name"
            )
        })
    });
    let type_ok = type_node
        .and_then(|n| n.utf8_text(source).ok())
        .is_some_and(|t| matches!(t.trim(), "self" | "static"));
    if !type_ok {
        return false;
    }

    // 引数リスト (arguments) を取得
    let args = node.child_by_field_name("arguments").or_else(|| {
        let mut cursor = node.walk();
        node.children(&mut cursor).find(|c| c.kind() == "arguments")
    });
    let Some(args) = args else {
        return false;
    };

    // 1 つ目の引数が string literal で、その中身が method_name と一致するかをチェック
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        // argument ノードを skip して中身の string literal を探す
        let actual = if child.kind() == "argument" {
            child.named_child(0)
        } else {
            Some(child)
        };
        let Some(actual) = actual else { continue };
        if matches!(actual.kind(), "string" | "encapsed_string") {
            // string ノードの中身は string_value / string_content / encapsed_string で囲まれる
            let Ok(raw) = actual.utf8_text(source) else {
                continue;
            };
            // クォートを剥がす ('FOO' / "FOO" のいずれも対応)
            let trimmed = raw.trim();
            let stripped = trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
                .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')));
            if let Some(literal) = stripped
                && literal == method_name
            {
                return true;
            }
            // 1 つ目の引数で結論が出たので break
            return false;
        }
    }
    false
}

/// PHP メソッドの docstring (`/** ... */`) に runtime annotation が含まれるかを判定する。
///
/// `@TypeItem`, `@dataProvider`, `@DataProvider`, `@Route`, `@Listen` 等、reflection
/// 経由でフレームワークから動的に呼ばれることを示すアノテーションが付いていれば true。
/// `Symbol.doc` から渡される文字列をチェックするため AST 走査は不要。
pub fn php_doc_has_runtime_annotation(doc: &str) -> bool {
    // `@\App\Annotations\TypeItem(...)` のような fully qualified 形式と
    // `@TypeItem(...)` のような short 形式の両方に対応するため、`@<Identifier>` を
    // 走査する。識別子の頭文字が大文字 (PHP の attribute / annotation 慣習) を要件にする。
    for token in doc.split('@').skip(1) {
        // token の先頭から識別子部分だけ取り出す
        let name: String = token
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '\\')
            .collect();
        let last = name.rsplit('\\').next().unwrap_or(name.as_str());
        // 大文字始まりの annotation (PSR / Doctrine 慣習) を検出
        if last.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            // `@TypeItem`, `@DataProvider`, `@Route`, `@Listen`, `@Bean` 等
            return true;
        }
        // 小文字始まりでも reflection 経由でよく使われるものは別途列挙
        const RUNTIME_LOWER_ANNOTATIONS: &[&str] = &[
            "dataProvider",
            "depends",
            "test",
            "before",
            "after",
            "beforeClass",
            "afterClass",
            "covers",
            "uses",
            "group",
            "ticket",
            "preserveGlobalState",
            "runInSeparateProcess",
        ];
        if RUNTIME_LOWER_ANNOTATIONS.contains(&last) {
            return true;
        }
    }
    false
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

/// C/C++ で、実関数の本体ブロック (compound_statement) の内側にネストして出現する
/// function_definition かどうかを判定する。
///
/// tree-sitter-cpp はマクロ呼び出し `BOOST_FOREACH(...) { ... }` 等を function_definition
/// として誤パースすることがあり、それらは実関数 body の内側に偽の関数定義として現れる。
/// トップレベル / namespace / class・struct 直下の本物の定義は、祖先が translation_unit /
/// declaration_list / field_declaration_list であって compound_statement ではないため
/// false を返す。GNU C の nested function を見逃す程度の false negative は許容する。
pub(crate) fn is_cpp_nested_function(root: Node, symbol_range: &Range) -> bool {
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
fn cpp_enclosing_function_definition(name_node: Node<'_>) -> Option<Node<'_>> {
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
fn is_exported_cpp(node: Node<'_>, source: &[u8]) -> bool {
    // メソッド定義 (function_definition) を起点にする
    let mut method = node;
    while method.kind() != "function_definition" {
        match method.parent() {
            Some(p) => method = p,
            None => return true, // 関数定義が見つからない → 保守的に公開
        }
    }
    // 直近の囲い specifier (class/struct) を探す
    let mut cur = method;
    let default_public = loop {
        match cur.parent() {
            Some(p) => match p.kind() {
                "class_specifier" => break false, // class: default private
                "struct_specifier" => break true, // struct: default public
                _ => cur = p,
            },
            None => return true, // クラス外 (自由関数・クラス外定義) は公開
        }
    };
    // method の直前の兄弟を遡って直近の access_specifier を探す
    let mut sibling = method.prev_sibling();
    while let Some(s) = sibling {
        if s.kind() == "access_specifier" {
            let txt = s.utf8_text(source).unwrap_or("");
            return txt.starts_with("public") || txt.starts_with("protected");
        }
        sibling = s.prev_sibling();
    }
    default_public
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
    // named ノードのみ計上する。tree-sitter-ruby では `if` 文ノードと
    // キーワードトークン `if` が同じ kind 名を持つため、named 制約が無いと
    // 分岐が二重計上される（他言語の分岐ノードは全て named なので無影響）。
    if node.is_named() && branch_kinds.contains(&kind) {
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
        LangId::Kotlin => &[
            "function_declaration",
            "lambda_literal",
            "anonymous_function",
        ],
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
        LangId::Java => &[
            "if_statement",
            "switch_expression",
            "for_statement",
            "enhanced_for_statement",
            "while_statement",
            "do_statement",
            "catch_clause",
        ],
        // Kotlin の分岐ノードは tree-sitter-kotlin 固有名 (`if_expression` / `when_expression` /
        // `when_entry` / `do_while_statement` / `catch_block` / `elvis_expression`)。
        // Java と同じスライスを共用すると一切マッチせず複雑度がベース 1 のまま返る。
        LangId::Kotlin => &[
            "if_expression",
            "when_expression",
            "when_entry",
            "for_statement",
            "while_statement",
            "do_while_statement",
            "catch_block",
            "elvis_expression",
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
                    let mut parent_node = node.parent().unwrap_or(node);
                    // C/C++ は関数名を `function_declarator` 配下でキャプチャするため、
                    // 本体を持つ function_definition まで繰り上げる。pointer_declarator /
                    // reference_declarator / qualified_identifier を経由しても辿れる。
                    // 繰り上げないと range が宣言子（シグネチャ行）だけに潰れ、複雑度が
                    // 常に 1 になり、impact 分析が関数本体のみの変更を取りこぼす。
                    // function_definition に到達しない宣言（プロトタイプ・関数ポインタ）は
                    // 本体が無いためシンボルとして採用しない。
                    if matches!(lang_id, LangId::C | LangId::Cpp)
                        && matches!(kind, SymbolKind::Function | SymbolKind::Method)
                    {
                        match cpp_enclosing_function_definition(node) {
                            Some(def) => parent_node = def,
                            None => continue,
                        }
                    }
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
            // function_declarator の名前を直接キャプチャし、本体を持つ
            // function_definition のみ採用する (extract_symbols の climb で判定)。
            // これにより `Type *foo()` のようなポインタ返り関数も拾える
            // (declarator が pointer_declarator に包まれ旧クエリではマッチしなかった)。
            r#"
            (function_declarator declarator: (identifier) @function.name)
            (struct_specifier name: (type_identifier) @struct.name)
            (enum_specifier name: (type_identifier) @enum.name)
            "#
        }
        LangId::Cpp => {
            // C と同様、function_declarator の名前を直接キャプチャする。
            // identifier=自由関数、field_identifier=クラス内メソッド、
            // qualified_identifier=クラス外定義 (Foo::bar)。pointer/reference 返りも
            // climb で function_definition まで辿るため拾える。
            r#"
            (function_declarator declarator: (identifier) @function.name)
            (function_declarator declarator: (field_identifier) @method.name)
            (function_declarator declarator: (qualified_identifier name: (identifier) @method.name))
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
            (protocol_function_declaration name: (simple_identifier) @function.name)
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

    /// テスト用: ソースからシンボルを抽出する。
    fn syms_of(source: &str, lang_id: LangId) -> Vec<Symbol> {
        let language = lang_id.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_symbols(tree.root_node(), source.as_bytes(), lang_id).unwrap()
    }

    /// テスト用: 指定シンボルの循環的複雑度を取得する。
    fn cx_of(source: &str, lang_id: LangId, name: &str) -> usize {
        let syms = syms_of(source, lang_id);
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "symbol '{name}' not found in {:?}",
                    syms.iter().map(|s| &s.name).collect::<Vec<_>>()
                )
            })
            .complexity
            .unwrap_or_else(|| panic!("symbol '{name}' has no complexity"))
    }

    // --- C/C++: ポインタ返り関数・クラスメソッド抽出 (回帰) ---

    #[test]
    fn c_pointer_returning_function_is_extracted() {
        // `Type *foo()` は declarator が pointer_declarator に包まれ、旧クエリ
        // (function_definition > function_declarator 直結) ではマッチしなかった。
        let syms = syms_of("int *make(int n) { return 0; }", LangId::C);
        assert!(
            syms.iter()
                .any(|s| s.name == "make" && matches!(s.kind, SymbolKind::Function)),
            "ポインタ返り関数 make が抽出される: {:?}",
            syms.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn c_function_prototype_is_not_extracted() {
        // 本体のない宣言 (プロトタイプ) はシンボルにしない。
        let syms = syms_of("int foo(void);\nint foo(void) { return 0; }", LangId::C);
        let count = syms.iter().filter(|s| s.name == "foo").count();
        assert_eq!(
            count,
            1,
            "定義のみ採用しプロトタイプは除外: {:?}",
            syms.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cpp_class_method_is_extracted_with_container() {
        let src = "class P {\npublic:\n  void add(int x) {}\n  int get() const { return 0; }\n};";
        let syms = syms_of(src, LangId::Cpp);
        let add = syms
            .iter()
            .find(|s| s.name == "add")
            .expect("メソッド add が抽出される");
        assert!(matches!(add.kind, SymbolKind::Method));
        assert_eq!(add.container.as_deref(), Some("P"));
        assert!(syms.iter().any(|s| s.name == "get"), "get も抽出される");
    }

    #[test]
    fn cpp_reference_returning_method_is_extracted() {
        let src =
            "class P {\npublic:\n  const int& at(int i) const { static int z=0; return z; }\n};";
        let syms = syms_of(src, LangId::Cpp);
        assert!(
            syms.iter().any(|s| s.name == "at"),
            "参照返りメソッド at が抽出される: {:?}",
            syms.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    // --- C++: メソッド可視性 (is_exported_cpp) ---

    #[test]
    fn cpp_public_method_is_exported() {
        assert!(check_exported(
            "class P {\npublic:\n  void pub_m() {}\n};",
            LangId::Cpp,
            "pub_m"
        ));
    }

    #[test]
    fn cpp_private_method_is_not_exported() {
        // class のデフォルトは private。private: 配下も非公開。
        let src = "class P {\n  void hidden() {}\nprivate:\n  void also() {}\n};";
        assert!(
            !check_exported(src, LangId::Cpp, "hidden"),
            "デフォルト private"
        );
        assert!(!check_exported(src, LangId::Cpp, "also"), "private: 配下");
    }

    #[test]
    fn cpp_protected_method_is_exported() {
        assert!(
            check_exported(
                "class P {\nprotected:\n  void prot_m() {}\n};",
                LangId::Cpp,
                "prot_m"
            ),
            "protected は継承 API として公開扱い"
        );
    }

    #[test]
    fn cpp_struct_method_default_is_exported() {
        // struct のデフォルトは public。
        assert!(check_exported(
            "struct S {\n  void m() {}\n};",
            LangId::Cpp,
            "m"
        ));
    }

    #[test]
    fn cpp_free_function_is_exported() {
        assert!(check_exported(
            "int freefn() { return 0; }",
            LangId::Cpp,
            "freefn"
        ));
    }

    // --- Ruby: 循環的複雑度の二重計上回帰 ---

    #[test]
    fn ruby_if_else_complexity_no_double_count() {
        // tree-sitter-ruby は `if` 文ノードとキーワードトークン `if` が同名 kind。
        // named ガードが無いと二重計上され cx=3。正しくは 2 (ベース1 + if 1)。
        assert_eq!(
            cx_of("def f(x)\n  if x then 1 else 2 end\nend", LangId::Ruby, "f"),
            2,
            "if/else は分岐 1 つ"
        );
    }

    #[test]
    fn ruby_case_when_complexity_no_double_count() {
        // case + when×2 = 3 分岐 → cx 4 (二重計上なら 7)。
        let src = "def f(x)\n  case x\n  when 1 then 1\n  when 2 then 2\n  else 3\n  end\nend";
        assert_eq!(cx_of(src, LangId::Ruby, "f"), 4);
    }

    #[test]
    fn ruby_while_complexity_no_double_count() {
        assert_eq!(
            cx_of(
                "def f(x)\n  while x > 0\n    x -= 1\n  end\nend",
                LangId::Ruby,
                "f"
            ),
            2,
            "while は分岐 1 つ"
        );
    }

    // --- Kotlin: 循環的複雑度 (旧実装は Java の分岐ノード名を共用しており常に 1 だった) ---

    #[test]
    fn kotlin_when_expression_complexity_counts_entries() {
        // `when` は tree-sitter-kotlin で `when_expression`、ブランチは `when_entry`。
        // 旧実装は Java の `switch_expression` を流用しており when を取りこぼし cx=1 だった。
        let src = "fun classify(x: Int): String {\n  return when (x) {\n    0 -> \"zero\"\n    1 -> \"one\"\n    2 -> \"two\"\n    else -> \"other\"\n  }\n}";
        // ベース 1 + when_expression 1 + when_entry 4 (0/1/2/else) = 6
        assert_eq!(cx_of(src, LangId::Kotlin, "classify"), 6);
    }

    #[test]
    fn kotlin_if_else_complexity_is_counted() {
        // `if` は tree-sitter-kotlin で `if_expression`。旧実装では cx=1 のままだった。
        let src = "fun pickIf(a: Int, b: Int): Int {\n  if (a > 0) {\n    if (b > 0) {\n      return a + b\n    } else {\n      return a - b\n    }\n  }\n  return 0\n}";
        // ベース 1 + if_expression 2 (外側 + 内側) = 3
        assert_eq!(cx_of(src, LangId::Kotlin, "pickIf"), 3);
    }

    #[test]
    fn kotlin_loop_complexity_is_counted() {
        let src = "fun loopSum(items: List<Int>): Int {\n  var sum = 0\n  for (it in items) {\n    while (sum < 100) {\n      sum += it\n    }\n  }\n  return sum\n}";
        // ベース 1 + for_statement 1 + while_statement 1 = 3
        assert_eq!(cx_of(src, LangId::Kotlin, "loopSum"), 3);
    }

    #[test]
    fn kotlin_try_catch_complexity_is_counted() {
        // `catch` は tree-sitter-kotlin で `catch_block`。
        let src = "fun tryIt(): Int {\n  try {\n    return 1\n  } catch (e: Exception) {\n    return -1\n  }\n}";
        // ベース 1 + catch_block 1 = 2
        assert_eq!(cx_of(src, LangId::Kotlin, "tryIt"), 2);
    }

    /// re-export 名抽出テスト用ヘルパー。
    fn collect_reexports(source: &str, lang_id: LangId) -> std::collections::HashSet<String> {
        let language = lang_id.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        super::collect_reexported_names(tree.root_node(), source.as_bytes())
    }

    #[test]
    fn ts_reexport_from_clause_is_not_local_export() {
        // `export { foo } from "./b"` は re-export (別モジュールの forwarding) であり、
        // ローカル定義 foo があっても from 句付き export はローカル export 判定に使わない。
        assert!(!check_exported(
            "function foo() {}\nexport { foo } from \"./b\"",
            LangId::Typescript,
            "foo"
        ));
    }

    #[test]
    fn collect_reexported_names_named_and_alias() {
        let names = collect_reexports(
            "export { foo, bar as baz } from \"./b\";",
            LangId::Typescript,
        );
        assert!(names.contains("foo"), "foo: {names:?}");
        assert!(names.contains("baz"), "alias 後の baz: {names:?}");
        assert!(
            !names.contains("bar"),
            "alias 前の bar は含まない: {names:?}"
        );
    }

    #[test]
    fn collect_reexported_names_ignores_local_export() {
        // from 句のない `export { foo }` は re-export ではない (ローカル export)。
        let names = collect_reexports("function foo() {}\nexport { foo };", LangId::Typescript);
        assert!(
            names.is_empty(),
            "ローカル export は re-export でない: {names:?}"
        );
    }

    /// Rust の `pub use` 再エクスポート抽出ヘルパー。
    fn collect_rust_reexports(source: &str) -> std::collections::HashSet<String> {
        let language = LangId::Rust.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        super::collect_rust_reexported_names(tree.root_node(), source.as_bytes())
    }

    #[test]
    fn collect_rust_reexports_single_scoped_identifier() {
        // `pub use sub::name;` → name が公開される
        let names = collect_rust_reexports("pub use sub::name;");
        assert!(names.contains("name"), "got: {names:?}");
        assert_eq!(names.len(), 1, "他の名前は含まない: {names:?}");
    }

    #[test]
    fn collect_rust_reexports_group_use_list() {
        // `pub use sub::{A, B};` → A, B が公開される
        let names = collect_rust_reexports("pub use sub::{A, B};");
        assert!(names.contains("A"), "got: {names:?}");
        assert!(names.contains("B"), "got: {names:?}");
        assert_eq!(names.len(), 2, "他の名前は含まない: {names:?}");
    }

    #[test]
    fn collect_rust_reexports_use_as_alias() {
        // `pub use sub::name as alias;` → alias が公開される (name は含まれない)
        let names = collect_rust_reexports("pub use sub::name as alias;");
        assert!(names.contains("alias"), "alias を公開: {names:?}");
        assert!(!names.contains("name"), "元の name は対象外: {names:?}");
    }

    #[test]
    fn collect_rust_reexports_group_with_alias() {
        // `pub use sub::{A, B as C};` → A, C が公開される
        let names = collect_rust_reexports("pub use sub::{A, B as C};");
        assert!(names.contains("A"), "got: {names:?}");
        assert!(names.contains("C"), "alias 後の C: {names:?}");
        assert!(!names.contains("B"), "alias 前の B は対象外: {names:?}");
    }

    #[test]
    fn collect_rust_reexports_wildcard_is_ignored() {
        // `pub use sub::*;` は名前が静的に解決できないため対象外 (fail-open 回避)
        let names = collect_rust_reexports("pub use sub::*;");
        assert!(names.is_empty(), "wildcard は対象外: {names:?}");
    }

    #[test]
    fn collect_rust_reexports_pub_crate_is_not_public() {
        // `pub(crate) use` は内部公開で外部利用者から見えないため対象外
        let names = collect_rust_reexports("pub(crate) use sub::name;");
        assert!(names.is_empty(), "pub(crate) は対象外: {names:?}");
    }

    #[test]
    fn collect_rust_reexports_private_use_is_ignored() {
        // `use sub::name;` (private) は import であり re-export ではない
        let names = collect_rust_reexports("use sub::name;");
        assert!(names.is_empty(), "private use は対象外: {names:?}");
    }

    #[test]
    fn collect_rust_reexports_multiple_declarations() {
        // 複数の pub use 宣言をまとめて収集する
        let source = "mod common;\n\
                      pub use common::{MAX_INPUT_SIZE, classify_error};\n\
                      pub use common::serialize_output;\n\
                      pub(crate) use common::internal;\n";
        let names = collect_rust_reexports(source);
        assert!(names.contains("MAX_INPUT_SIZE"), "got: {names:?}");
        assert!(names.contains("classify_error"), "got: {names:?}");
        assert!(names.contains("serialize_output"), "got: {names:?}");
        assert!(
            !names.contains("internal"),
            "pub(crate) は対象外: {names:?}"
        );
        assert_eq!(names.len(), 3, "got: {names:?}");
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
    fn swift_public_struct_is_exported() {
        assert!(check_exported(
            "public struct Detector {}",
            LangId::Swift,
            "Detector"
        ));
    }

    #[test]
    fn swift_internal_enum_is_not_exported() {
        assert!(!check_exported(
            "enum DetectionError { case failed }",
            LangId::Swift,
            "DetectionError"
        ));
    }

    #[test]
    fn swift_open_class_is_exported() {
        assert!(check_exported(
            "open class Service {}",
            LangId::Swift,
            "Service"
        ));
    }

    #[test]
    fn swift_private_func_is_not_exported() {
        assert!(!check_exported(
            "private func helper() {}",
            LangId::Swift,
            "helper"
        ));
    }

    #[test]
    fn swift_internal_method_in_public_struct_is_not_exported() {
        assert!(!check_exported(
            "public struct S {\n    func internalMethod() {}\n}\n",
            LangId::Swift,
            "internalMethod"
        ));
    }

    #[test]
    fn swift_public_method_is_exported() {
        assert!(check_exported(
            "public struct S {\n    public func run() {}\n}\n",
            LangId::Swift,
            "run"
        ));
    }

    #[test]
    fn swift_public_extension_method_is_exported() {
        // public extension のメンバは明示修飾子なしでもデフォルト public
        assert!(check_exported(
            "public extension Foo {\n    func bar() -> Int { 0 }\n}\n",
            LangId::Swift,
            "bar"
        ));
    }

    #[test]
    fn swift_public_protocol_requirement_is_exported() {
        // public protocol の requirement (func handle) は外部公開 API なので exported。
        assert!(check_exported(
            "public protocol Service {\n    func handle() -> Int\n}\n",
            LangId::Swift,
            "handle"
        ));
    }

    #[test]
    fn swift_internal_protocol_requirement_is_not_exported() {
        // internal (修飾子なし) protocol の requirement は外部公開 API ではない。
        assert!(!check_exported(
            "protocol Service {\n    func handle() -> Int\n}\n",
            LangId::Swift,
            "handle"
        ));
    }

    #[test]
    fn swift_plain_extension_method_is_not_exported() {
        // 修飾子なし extension のメンバは internal (外部公開 API ではない)
        assert!(!check_exported(
            "extension Foo {\n    func bar() -> Int { 0 }\n}\n",
            LangId::Swift,
            "bar"
        ));
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

    /// TypeScript class の `private` メソッドは公開 API ではないことを検証
    /// (Issue: 2026-05-22-temperature-api-triage の private isLegacyGpt5Model 誤検出)
    #[test]
    fn ts_private_class_method_is_not_exported() {
        let src = r#"export class C {
    private internal(): boolean { return false; }
    public callable(): void { this.internal(); }
}
"#;
        assert!(!check_exported(src, LangId::Typescript, "internal"));
    }

    /// TypeScript class の `public` メソッドは引き続き公開 API として扱われることを検証
    #[test]
    fn ts_public_class_method_is_exported() {
        let src = r#"export class C {
    public callable(): void {}
}
"#;
        assert!(check_exported(src, LangId::Typescript, "callable"));
    }

    /// TypeScript class の accessibility なし (=public 相当) メソッドも exported のまま
    #[test]
    fn ts_default_class_method_is_exported() {
        let src = r#"export class C {
    callable(): void {}
}
"#;
        assert!(check_exported(src, LangId::Typescript, "callable"));
    }

    /// ECMAScript `#private` メンバを private_class_member 判定で捕捉できることを検証。
    /// (extract_symbols は現状 `#`-prefixed メンバを symbols として返さないが、
    /// 仮に抽出された場合に exported 判定を fail-closed にするための保険)
    #[test]
    fn ts_hash_private_member_helper_returns_true() {
        let src = r#"export class C {
    #secret: number = 0;
    #helper(): void {}
}
"#;
        let language = LangId::Typescript.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        // private_property_identifier ノードを再帰探索し、is_private_class_member_js_ts が true を返すこと
        fn find_private_property_identifier<'t>(
            n: tree_sitter::Node<'t>,
        ) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "private_property_identifier" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_private_property_identifier(child) {
                    return Some(found);
                }
            }
            None
        }
        let priv_node =
            find_private_property_identifier(root).expect("private_property_identifier exists");
        assert!(
            is_private_class_member_js_ts(priv_node, src.as_bytes()),
            "#private メンバ ({:?}) は private_class_member として判定されるべき",
            priv_node.utf8_text(src.as_bytes())
        );
    }

    /// `protected` は当面保守的に exported のまま (TODO 扱い)
    #[test]
    fn ts_protected_class_method_is_exported() {
        let src = r#"export class C {
    protected helper(): void {}
}
"#;
        assert!(check_exported(src, LangId::Typescript, "helper"));
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

    /// PHP 擬似 enum パターンの判定 (Laravel/DDD 系の AbstractValueObject 派生)
    #[test]
    fn is_php_pseudo_enum_method_detects_value_object_factory() {
        let src = r#"<?php
class MenuName {
    public static function MENU_HOME(): self {
        return new self('MENU_HOME');
    }
    public static function MENU_NEW_FEATURE(): static {
        return new static('MENU_NEW_FEATURE');
    }
    public static function notPseudo(): self {
        return new self('different_name');
    }
    public function instanceMethod(): self {
        return new self('instanceMethod');
    }
}
"#;
        let language = LangId::Php.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, src.as_bytes(), LangId::Php).unwrap();

        let menu_home = syms
            .iter()
            .find(|s| s.name == "MENU_HOME")
            .expect("MENU_HOME");
        assert!(
            is_php_pseudo_enum_method(root, src.as_bytes(), &menu_home.range, "MENU_HOME"),
            "self ベースの擬似 enum を検出すべき"
        );

        let menu_new = syms
            .iter()
            .find(|s| s.name == "MENU_NEW_FEATURE")
            .expect("MENU_NEW_FEATURE");
        assert!(
            is_php_pseudo_enum_method(root, src.as_bytes(), &menu_new.range, "MENU_NEW_FEATURE"),
            "static ベースの擬似 enum も検出すべき"
        );

        let not_pseudo = syms
            .iter()
            .find(|s| s.name == "notPseudo")
            .expect("notPseudo");
        assert!(
            !is_php_pseudo_enum_method(root, src.as_bytes(), &not_pseudo.range, "notPseudo"),
            "メソッド名と new self('...') の文字列が不一致なら擬似 enum ではない"
        );

        let instance_method = syms
            .iter()
            .find(|s| s.name == "instanceMethod")
            .expect("instanceMethod");
        assert!(
            !is_php_pseudo_enum_method(
                root,
                src.as_bytes(),
                &instance_method.range,
                "instanceMethod"
            ),
            "static でないメソッドは擬似 enum ではない"
        );
    }

    /// 戻り値型が self/static でないと擬似 enum とみなさない
    #[test]
    fn is_php_pseudo_enum_method_requires_self_return_type() {
        let src = r#"<?php
class Foo {
    public static function BAR(): Foo {
        return new self('BAR');
    }
}
"#;
        let language = LangId::Php.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, src.as_bytes(), LangId::Php).unwrap();
        let bar = syms.iter().find(|s| s.name == "BAR").expect("BAR");
        assert!(
            !is_php_pseudo_enum_method(root, src.as_bytes(), &bar.range, "BAR"),
            "戻り値型が self/static でないと擬似 enum とは判定しない"
        );
    }

    /// PHP runtime annotation の検出 (`@TypeItem`, `@Route`, `@dataProvider` 等)
    #[test]
    fn php_doc_has_runtime_annotation_detects_uppercase_annotations() {
        // @TypeItem fully-qualified
        assert!(php_doc_has_runtime_annotation(
            "/**\n * @\\App\\Annotations\\TypeItem(id=1, name=\"X\")\n */"
        ));
        // @TypeItem short
        assert!(php_doc_has_runtime_annotation("/** @TypeItem(...) */"));
        // @Route
        assert!(php_doc_has_runtime_annotation("/** @Route(...) */"));
        // @dataProvider (lowercase 慣用)
        assert!(php_doc_has_runtime_annotation(
            "/** @dataProvider voEvent */"
        ));
        // @DataProvider (PHPUnit 11 形式)
        assert!(php_doc_has_runtime_annotation(
            "/** @DataProvider('foo') */"
        ));
    }

    #[test]
    fn php_doc_has_runtime_annotation_skips_plain_docstring() {
        // 通常コメント
        assert!(!php_doc_has_runtime_annotation("/** メニュー: ホーム */"));
        // @param / @return / @var (低リスクの常識的タグ)
        assert!(!php_doc_has_runtime_annotation("/** @param int $x */"));
        assert!(!php_doc_has_runtime_annotation("/** @return self */"));
        assert!(!php_doc_has_runtime_annotation("/** @var string */"));
    }

    // --- is_js_ts_framework_dsl_callback テスト ---

    fn check_js_ts_dsl_callback(source: &str, lang: LangId, symbol_name: &str) -> bool {
        let language = lang.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, source.as_bytes(), lang).unwrap();
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
        is_js_ts_framework_dsl_callback(root, source.as_bytes(), &sym.range)
    }

    #[test]
    fn js_ts_wxt_define_content_script_main_method_detected() {
        // Issue 報告ケース: WXT defineContentScript の引数 object に method shorthand
        let src = r#"
import { defineContentScript } from 'wxt/sandbox';

export default defineContentScript({
    matches: ['*://example.com/*'],
    main() {
        console.log('hello');
    },
});
"#;
        assert!(check_js_ts_dsl_callback(src, LangId::Typescript, "main"));
    }

    #[test]
    fn js_ts_wxt_define_background_main_method_detected() {
        let src = r#"
export default defineBackground({
    main() {
        console.log('background');
    },
});
"#;
        assert!(check_js_ts_dsl_callback(src, LangId::Typescript, "main"));
    }

    #[test]
    fn js_ts_vue_define_component_setup_method_detected() {
        // Vue 推奨の method shorthand 形式
        let src = r#"
export default defineComponent({
    setup() {
        return {};
    },
});
"#;
        assert!(check_js_ts_dsl_callback(src, LangId::Typescript, "setup"));
    }

    #[test]
    fn js_ts_vite_define_config_method_detected() {
        let src = r#"
export default defineConfig({
    name: "x",
});
"#;
        // この source には method/function symbol が無いため別 case で確認:
        let src2 = r#"
export default defineConfig({
    onPluginInit() {
        return null;
    },
});
"#;
        let _ = src;
        assert!(check_js_ts_dsl_callback(
            src2,
            LangId::Typescript,
            "onPluginInit"
        ));
    }

    #[test]
    fn js_ts_plain_object_method_not_excluded() {
        // 通常のオブジェクトリテラルの method shorthand は dead 候補のまま (false)
        let src = r#"
export const obj = {
    main() {
        return 1;
    },
};
"#;
        assert!(!check_js_ts_dsl_callback(src, LangId::Typescript, "main"));
    }

    #[test]
    fn js_ts_unknown_dsl_caller_not_excluded() {
        // allowlist に含まれない関数の引数 object メソッドは dead 候補のまま
        let src = r#"
export default someUnknownFn({
    main() {
        return 1;
    },
});
"#;
        assert!(!check_js_ts_dsl_callback(src, LangId::Typescript, "main"));
    }

    #[test]
    fn js_ts_inner_helper_inside_dsl_callback_not_excluded() {
        // DSL callback (main) 内部で定義された helper は dead 候補のまま
        let src = r#"
export default defineContentScript({
    main() {
        function helper() {
            return 1;
        }
        return helper();
    },
});
"#;
        // main は除外対象
        assert!(check_js_ts_dsl_callback(src, LangId::Typescript, "main"));
        // helper は対象外 (内部 function は dead 判定の余地を残す)
        assert!(!check_js_ts_dsl_callback(src, LangId::Typescript, "helper"));
    }

    // --- is_js_ts_angular_lifecycle_hook テスト ---

    fn check_angular_lifecycle_hook(source: &str, symbol_name: &str) -> bool {
        let language = LangId::Typescript.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();

        let syms = extract_symbols(root, source.as_bytes(), LangId::Typescript).unwrap();
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
        is_js_ts_angular_lifecycle_hook(root, source.as_bytes(), &sym.range)
    }

    /// GitLab issue #8 再現: `@Component` 装飾クラスの `ngAfterViewChecked` は除外対象。
    #[test]
    fn angular_component_lifecycle_hook_is_detected() {
        let src = r#"
import { Component } from '@angular/core';

@Component({
    template: '<div>example</div>',
})
export class MinimalComponent {
    public ngAfterViewChecked(): void {
    }
}
"#;
        assert!(check_angular_lifecycle_hook(src, "ngAfterViewChecked"));
    }

    /// `@Directive` 装飾クラスの lifecycle hook も除外対象。
    #[test]
    fn angular_directive_lifecycle_hook_is_detected() {
        let src = r#"
import { Directive } from '@angular/core';

@Directive({ selector: '[appFoo]' })
export class FooDirective {
    public ngOnInit(): void {}
    public ngOnDestroy(): void {}
}
"#;
        assert!(check_angular_lifecycle_hook(src, "ngOnInit"));
        assert!(check_angular_lifecycle_hook(src, "ngOnDestroy"));
    }

    /// `implements AfterViewChecked` 等の interface 実装が省略されていても、Angular の
    /// 呼出規約は decorator + メソッド名で成立する。
    #[test]
    fn angular_lifecycle_hook_detected_without_interface_implementation() {
        let src = r#"
@Component({ template: '' })
class WithoutInterface {
    ngAfterViewChecked() {}
}
"#;
        assert!(check_angular_lifecycle_hook(src, "ngAfterViewChecked"));
    }

    /// `@Component` / `@Directive` のいずれも持たないクラスのメソッドは Angular hook 扱いしない。
    #[test]
    fn non_angular_class_with_same_method_name_not_detected() {
        let src = r#"
class PlainClass {
    ngAfterViewChecked(): void {}
}
"#;
        assert!(!check_angular_lifecycle_hook(src, "ngAfterViewChecked"));
    }

    /// `@Injectable` 等の他の Angular decorator は対象外 (lifecycle hook を持たないため)。
    #[test]
    fn angular_injectable_class_method_not_detected() {
        let src = r#"
@Injectable({ providedIn: 'root' })
class FooService {
    ngOnInit(): void {}
}
"#;
        assert!(!check_angular_lifecycle_hook(src, "ngOnInit"));
    }

    /// Angular lifecycle hook 名以外のメソッド (custom method) は除外対象外。
    #[test]
    fn angular_component_non_lifecycle_method_not_detected() {
        let src = r#"
@Component({ template: '' })
class Foo {
    public regularMethod(): void {}
}
"#;
        assert!(!check_angular_lifecycle_hook(src, "regularMethod"));
    }

    // --- is_js_ts_angular_runtime_entrypoint テスト ---

    fn check_angular_runtime_entrypoint(source: &str, symbol_name: &str) -> bool {
        let language = LangId::Typescript.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        let syms = extract_symbols(root, source.as_bytes(), LangId::Typescript).unwrap();
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
        is_js_ts_angular_runtime_entrypoint(root, source.as_bytes(), &sym.range)
    }

    /// GitLab issue #20: `implements ControlValueAccessor` のクラスの 4 規約メソッドは除外。
    #[test]
    fn angular_cva_methods_via_implements_are_detected() {
        let src = r#"
@Directive()
export abstract class AbstractBaseControl implements ControlValueAccessor {
    writeValue(obj: any) {}
    registerOnChange(fn: any) {}
    registerOnTouched(fn: any) {}
    setDisabledState(isDisabled: boolean) {}
}
"#;
        assert!(check_angular_runtime_entrypoint(src, "writeValue"));
        assert!(check_angular_runtime_entrypoint(src, "registerOnChange"));
        assert!(check_angular_runtime_entrypoint(src, "registerOnTouched"));
        assert!(check_angular_runtime_entrypoint(src, "setDisabledState"));
    }

    /// GitLab issue #20: `NG_VALUE_ACCESSOR` provider 登録のあるクラス (implements 句なし) でも
    /// CVA 規約メソッドは除外する。
    #[test]
    fn angular_cva_methods_via_ng_value_accessor_provider_are_detected() {
        let src = r#"
@Component({
    selector: 'bz-input',
    providers: [{ provide: NG_VALUE_ACCESSOR, useExisting: InputControlComponent, multi: true }],
})
export class InputControlComponent {
    writeValue(obj: any) {}
    registerOnChange(fn: any) {}
    registerOnTouched(fn: any) {}
}
"#;
        assert!(check_angular_runtime_entrypoint(src, "writeValue"));
        assert!(check_angular_runtime_entrypoint(src, "registerOnChange"));
        assert!(check_angular_runtime_entrypoint(src, "registerOnTouched"));
    }

    /// CVA 規約メソッド名でも `implements ControlValueAccessor` も NG_VALUE_ACCESSOR provider
    /// もない通常クラスでは除外対象外。
    #[test]
    fn angular_cva_method_name_in_non_cva_class_not_detected() {
        let src = r#"
@Component({ template: '' })
export class PlainComponent {
    writeValue(obj: any) {}
}
"#;
        assert!(!check_angular_runtime_entrypoint(src, "writeValue"));
    }

    /// GitLab issue #23: `@HostListener` 付きメソッドは除外対象。
    #[test]
    fn angular_host_listener_method_is_detected() {
        let src = r#"
@Component({ template: '' })
export class ChatComponent {
    @HostListener('window:beforeunload', ['$event'])
    beforeUnloadHandler() {}
}
"#;
        assert!(check_angular_runtime_entrypoint(src, "beforeUnloadHandler"));
    }

    /// `@HostListener` が付かないメソッドは除外対象外。
    #[test]
    fn angular_method_without_host_listener_not_detected() {
        let src = r#"
@Component({ template: '' })
export class ChatComponent {
    notAHandler() {}
}
"#;
        assert!(!check_angular_runtime_entrypoint(src, "notAHandler"));
    }

    /// `@HostBinding` 付き method も Angular 経路扱いで除外する (member 単位 decorator
    /// allowlist の網羅確認)。
    #[test]
    fn angular_host_binding_method_is_detected() {
        let src = r#"
@Directive({ selector: '[appFoo]' })
export class FooDirective {
    @HostBinding('class.active')
    isActive() { return true; }
}
"#;
        assert!(check_angular_runtime_entrypoint(src, "isActive"));
    }

    /// `@Component` / `@Directive` を持たないクラスのメンバーは Angular 経路扱いしない
    /// (member decorator が付いていても class 装飾が無ければ Angular とみなさない)。
    #[test]
    fn angular_member_decorator_in_non_angular_class_not_detected() {
        let src = r#"
class Plain {
    @HostListener('click')
    onClick() {}
}
"#;
        assert!(!check_angular_runtime_entrypoint(src, "onClick"));
    }

    // --- is_java_flyway_migration_class テスト ---

    fn check_java_flyway_class(source: &str, symbol_name: &str) -> bool {
        let language = LangId::Java.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        let syms = extract_symbols(root, source.as_bytes(), LangId::Java).unwrap();
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
        is_java_flyway_migration_class(root, source.as_bytes(), &sym.range)
    }

    /// GitLab issue #24 再現: `extends BaseJavaMigration` の class は除外対象。
    #[test]
    fn java_flyway_migration_extends_base_class_is_detected() {
        let src = r#"
package db.migration;

import org.flywaydb.core.api.migration.BaseJavaMigration;
import org.flywaydb.core.api.migration.Context;

public class V2021_01_02__Zipcode extends BaseJavaMigration {
    public void migrate(Context context) throws Exception {
    }
}
"#;
        assert!(check_java_flyway_class(src, "V2021_01_02__Zipcode"));
    }

    /// `implements JavaMigration` のクラスも除外対象。
    #[test]
    fn java_flyway_migration_implements_interface_is_detected() {
        let src = r#"
package db.migration;

import org.flywaydb.core.api.migration.JavaMigration;
import org.flywaydb.core.api.migration.MigrationVersion;

public class V1__Manual implements JavaMigration {
    public MigrationVersion getVersion() { return null; }
    public String getDescription() { return ""; }
    public Integer getChecksum() { return null; }
    public boolean isUndo() { return false; }
    public boolean canExecuteInTransaction() { return true; }
    public void migrate(org.flywaydb.core.api.migration.Context context) throws Exception {}
}
"#;
        assert!(check_java_flyway_class(src, "V1__Manual"));
    }

    /// 完全修飾名 (`extends org.flywaydb.core.api.migration.BaseJavaMigration`) でも検出する。
    #[test]
    fn java_flyway_migration_fully_qualified_super_is_detected() {
        let src = r#"
package db.migration;

public class V2__Fqcn extends org.flywaydb.core.api.migration.BaseJavaMigration {
    public void migrate(org.flywaydb.core.api.migration.Context context) throws Exception {}
}
"#;
        assert!(check_java_flyway_class(src, "V2__Fqcn"));
    }

    /// Flyway を継承していない通常のクラスは除外対象外。
    #[test]
    fn java_non_flyway_class_not_detected() {
        let src = r#"
package app.example;

public class RegularService {
    public void doWork() {}
}
"#;
        assert!(!check_java_flyway_class(src, "RegularService"));
    }

    /// 同名でも別 framework (`extends BaseTask` 等) の継承は Flyway と誤判定しない。
    #[test]
    fn java_unrelated_super_class_not_detected() {
        let src = r#"
package app.batch;

public class V2021_01_02__BatchJob extends app.batch.BaseTask {
    public void run() {}
}
"#;
        assert!(!check_java_flyway_class(src, "V2021_01_02__BatchJob"));
    }

    /// Flyway migration class 配下の method (例: `migrate(Context)`) も同じ Flyway 判定で
    /// true を返す。symbol_range から class_declaration 祖先まで遡る設計のため、Class と
    /// Method を同じヘルパーで処理できる (dead-code 経路は class 単体 + 配下メソッドの
    /// 両方を Flyway runtime が反射経由で呼ぶため除外したい)。
    #[test]
    fn java_flyway_migration_member_method_is_detected_via_class() {
        let src = r#"
package db.migration;
import org.flywaydb.core.api.migration.BaseJavaMigration;
import org.flywaydb.core.api.migration.Context;
public class V1__X extends BaseJavaMigration {
    public void migrate(Context context) throws Exception {}
}
"#;
        assert!(check_java_flyway_class(src, "migrate"));
    }

    /// `implements MigrationContainer<JavaMigration>` のような generic 型引数として現れる
    /// `JavaMigration` は「直接の親型」ではないため除外対象にしない (codex 指摘の再帰走査
    /// 過剰マッチ修正)。
    #[test]
    fn java_generic_type_argument_with_flyway_name_not_detected() {
        let src = r#"
package app;
interface MigrationContainer<T> {}
class JavaMigration {}
public class Wrapper implements MigrationContainer<JavaMigration> {
    public void run() {}
}
"#;
        assert!(!check_java_flyway_class(src, "Wrapper"));
    }

    /// 別 package の `com.example.BaseJavaMigration` は完全修飾名が Flyway のものと
    /// 一致しないため除外対象にしない (codex 指摘の FQN 末尾名マッチ修正)。
    #[test]
    fn java_unrelated_fqcn_base_with_flyway_simple_name_not_detected() {
        let src = r#"
package app;
public class Custom extends com.example.BaseJavaMigration {
    public void run() {}
}
"#;
        assert!(!check_java_flyway_class(src, "Custom"));
    }

    /// FQN が改行 (valid Java) で分割されていても、AST 識別子セグメントの連結で比較する
    /// ため検出漏れしない (codex 指摘の raw text 比較弱点に対する対処)。
    #[test]
    fn java_flyway_migration_split_fqn_is_detected() {
        let src = "package db.migration;\n\
                   public class V2__SplitFqn extends org.flywaydb.core.api.migration.\n\
                       BaseJavaMigration {\n\
                       public void migrate(org.flywaydb.core.api.migration.Context context) throws Exception {}\n\
                   }\n";
        assert!(check_java_flyway_class(src, "V2__SplitFqn"));
    }
}
