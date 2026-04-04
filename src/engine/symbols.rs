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
        _ => true, // 未対応言語は保守的にエクスポートと判定
    }
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
        "method_declaration",
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "object_declaration",
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

/// 関数/メソッドノードの循環的複雑度を算出する（ベース1 + 分岐ノード数）。
pub fn calculate_complexity(node: Node, lang_id: LangId) -> usize {
    let branch_kinds = branch_node_kinds(lang_id);
    let mut count = 1; // ベース複雑度
    count_branch_nodes(node, &branch_kinds, &mut count);
    count
}

/// 再帰的に分岐ノードをカウントする。
fn count_branch_nodes(node: Node, branch_kinds: &'static [&'static str], count: &mut usize) {
    let kind = node.kind();
    if branch_kinds.contains(&kind) {
        *count += 1;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count_branch_nodes(child, branch_kinds, count);
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
                        children: Vec::new(),
                    });
                }
            }
        }
    }

    Ok(symbols)
}

fn capture_name_to_kind(name: &str) -> Option<SymbolKind> {
    match name {
        "function.name" | "method.name" => Some(SymbolKind::Function),
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
}
