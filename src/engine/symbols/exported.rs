use tree_sitter::Node;

use crate::language::LangId;
use crate::models::location::Range;

use super::cpp::is_exported_cpp;
use super::{is_js_function_body, node_for_symbol_range};

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
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
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
pub(super) fn is_private_class_member_js_ts(node: Node, source: &[u8]) -> bool {
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

/// TS/JS の named export clause specifier 1 件分。
/// `export { local as exported }` (from 句なし) / `export { name } from "..."` (forwarding)
/// の両形式を表す。
struct JsTsExportClauseSpecifier<'a> {
    /// export 元のローカル名 (`export { A as B }` の A)。
    local_name: &'a str,
    /// 利用者から見える公開名 (`export { A as B }` の B、alias なしなら A)。
    exported_name: &'a str,
    /// from 句 (`export { X } from "..."`) の有無。
    has_source: bool,
}

/// root 直下の export_statement 内の export specifier を列挙して visit に渡す。
/// `has_named_export` / `collect_js_ts_named_export_surface_names` の共通 AST walk。
fn visit_js_ts_export_clause_specifiers<'a>(
    root: Node,
    source: &'a [u8],
    mut visit: impl FnMut(JsTsExportClauseSpecifier<'a>),
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "export_statement" {
            continue;
        }
        let has_source = child.child_by_field_name("source").is_some();
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
                let Some(local_name) = spec
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                else {
                    continue;
                };
                let exported_name = spec
                    .child_by_field_name("alias")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or(local_name);
                visit(JsTsExportClauseSpecifier {
                    local_name,
                    exported_name,
                    has_source,
                });
            }
        }
    }
}

/// トップレベルの export { ... } 文から一致する名前を検索する。
/// `export { foo } from "./b"` は別モジュールの forwarding (re-export) であり、
/// このファイルでの foo のローカル定義 export ではないため対象外。
fn has_named_export(root: Node, source: &[u8], target_name: &str) -> bool {
    let mut found = false;
    visit_js_ts_export_clause_specifiers(root, source, |spec| {
        if !spec.has_source && spec.local_name == target_name {
            found = true;
        }
    });
    found
}

/// TS/JS の named export clause (`export { foo }` / `export { foo as bar }` /
/// `export { foo } from "..."`) が **このファイルから提供する公開 export 名** の集合を
/// 返す (alias があれば alias 名)。from 句の有無は問わない — `import ...; export { X };`
/// (from 句なし re-export) も `export { X } from "..."` (forwarding) も、利用者から見た
/// export 面では同じく X を公開し続ける。`export * from "..."` (wildcard) は名前が
/// 静的に不明なため対象外。
///
/// api.rm 抑制に使う: あるシンボルがローカル定義から re-export (from 句の有無を問わず)
/// に置き換わっても、利用者から見た export 面 (import path から取れる名前) は維持されて
/// いるため「削除」ではない。ローカル定義付き `export { X }` の X も集合に入るが、その
/// 場合 X は exported シンボルとして別途抽出され api.rm 候補に上がらないため無害。
pub(crate) fn collect_js_ts_named_export_surface_names(
    root: Node,
    source: &[u8],
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    visit_js_ts_export_clause_specifiers(root, source, |spec| {
        names.insert(spec.exported_name.to_string());
    });
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
