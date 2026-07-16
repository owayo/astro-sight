use anyhow::Result;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use crate::language::LangId;
use crate::models::location::Range;
use crate::models::symbol::{Symbol, SymbolKind};

mod complexity;
mod cpp;
mod exported;
mod framework;
mod overrides;
mod scope;

pub use complexity::calculate_complexity;
pub use exported::is_symbol_exported;
pub use framework::{
    has_framework_entrypoint_decorator_python, is_java_flyway_migration_class,
    is_js_ts_angular_lifecycle_hook, is_js_ts_angular_provider_option_callback,
    is_js_ts_angular_runtime_entrypoint, is_js_ts_framework_dsl_callback,
    is_php_laravel_runtime_entrypoint, is_php_pseudo_enum_method, php_doc_has_runtime_annotation,
    python_class_base_names,
};
pub use overrides::is_override_method;
pub use scope::is_local_scope_symbol;

pub(crate) use cpp::{
    collect_cpp_dead_liveness_aliases, is_cpp_forward_declaration, is_cpp_nested_function,
    is_trait_impl_method_rust,
};
pub(crate) use exported::{
    collect_js_ts_named_export_surface_names, collect_rust_reexported_names,
};

use cpp::cpp_enclosing_function_definition;

#[cfg(test)]
use exported::is_private_class_member_js_ts;
#[cfg(test)]
use overrides::contains_keyword;

/// `symbol_range` を tree-sitter の Point 範囲へ変換し、対応する最小の子孫ノードを返す。
/// symbols.rs 全域の symbol 判定 (スコープ/エクスポート/フレームワーク/言語別ヘルパー) が
/// 共通で使う汎用入口処理。
fn node_for_symbol_range<'a>(root: Node<'a>, symbol_range: &Range) -> Option<Node<'a>> {
    let start = tree_sitter::Point {
        row: symbol_range.start.line,
        column: symbol_range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol_range.end.line,
        column: symbol_range.end.column,
    };
    root.descendant_for_point_range(start, end)
}

/// `node` 自身から祖先方向へ走査し、`kinds` のいずれかの kind を持つ最初のノードを返す。
/// method_definition や class_declaration など「囲む特定種別ノード」を辿る用途。
fn enclosing_of_kind<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if kinds.contains(&n.kind()) {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

/// `method_definition` ノードの name フィールドを UTF-8 文字列として取り出す。
fn js_ts_method_name<'a>(method_node: Node, source: &'a [u8]) -> Option<&'a str> {
    method_node
        .child_by_field_name("name")?
        .utf8_text(source)
        .ok()
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

/// パース済み AST からシンボルを抽出する。
pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {
    let query_src = symbol_query(lang_id);
    if query_src.is_empty() {
        return Ok(fallback_symbols(root, source));
    }
    run_symbol_query(root, source, lang_id, query_src)
}

/// カスタム tree-sitter クエリでシンボルを抽出する (`symbols --query`)。
/// built-in クエリを置換する。capture 名は built-in と同じ語彙
/// (`function.name` / `class.name` 等 = `capture_name_to_kind` が解決できる名前) に
/// 限定し、不正なクエリ・未知 capture・有効 capture なしは `INVALID_REQUEST` を返す
/// (従来の silent no-op を廃止)。
pub fn extract_symbols_with_custom_query(
    root: Node<'_>,
    source: &[u8],
    lang_id: LangId,
    custom_query: &str,
) -> Result<Vec<Symbol>> {
    use crate::error::{AstroError, ErrorCode};

    let language = lang_id.ts_language();
    let query = Query::new(&language, custom_query).map_err(|e| {
        AstroError::new(
            ErrorCode::InvalidRequest,
            format!("invalid --query for {lang_id}: {e}"),
        )
    })?;
    let unknown: Vec<&str> = query
        .capture_names()
        .iter()
        .filter(|name| capture_name_to_kind(name).is_none())
        .copied()
        .collect();
    if !unknown.is_empty() {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "--query uses unsupported capture(s) {:?}; supported captures map to symbol kinds like function.name / class.name / method.name",
                unknown
            ),
        )
        .into());
    }
    if query.capture_names().is_empty() {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            "--query has no captures; add e.g. @function.name to select symbols",
        )
        .into());
    }
    run_symbol_query(root, source, lang_id, custom_query)
}

/// symbol クエリ本体の実行 (built-in / custom 共通)。
fn run_symbol_query(
    root: Node<'_>,
    source: &[u8],
    lang_id: LangId,
    query_src: &str,
) -> Result<Vec<Symbol>> {
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
                let name = node.utf8_text(source).unwrap_or("");
                if !name.is_empty() {
                    let mut parent_node = node.parent().unwrap_or(node);
                    // C/C++ は関数名を `function_declarator` 配下でキャプチャするため、
                    // 本体を持つ function_definition まで繰り上げる。pointer_declarator /
                    // reference_declarator / qualified_identifier を経由しても辿れる。
                    // 繰り上げないと range が宣言子（シグネチャ行）だけに潰れ、複雑度が
                    // 常に 1 になり、impact 分析が関数本体のみの変更を取りこぼす。
                    // function_definition に到達しない宣言（プロトタイプ・関数ポインタ）は
                    // 本体が無いためシンボルとして採用しない。
                    let mut promoted_to_definition = false;
                    if matches!(lang_id, LangId::C | LangId::Cpp)
                        && matches!(kind, SymbolKind::Function | SymbolKind::Method)
                    {
                        match cpp_enclosing_function_definition(node) {
                            Some(def) => {
                                parent_node = def;
                                promoted_to_definition = true;
                            }
                            None => continue,
                        }
                    }
                    // Ruby の `class A::B` / `module A::B` は name capture が scope_resolution
                    // 内の末尾 constant で、parent が scope_resolution 止まりだと range が
                    // 名前部分に潰れ、配下メソッドの container 帰属・impact の class 帰属が
                    // 壊れる。class / module ノードまで昇格して本体全体を range にする。
                    if lang_id == LangId::Ruby
                        && parent_node.kind() == "scope_resolution"
                        && let Some(scoped_decl) = parent_node.parent()
                        && matches!(scoped_decl.kind(), "class" | "module")
                    {
                        parent_node = scoped_decl;
                        promoted_to_definition = true;
                    }
                    // C/C++ の関数は name の親が function_declarator で、その prev sibling は
                    // 戻り型になり doc comment に到達しない (Ruby の scoped class も同様に
                    // scope_resolution の prev sibling は comment でない)。昇格した定義ノード
                    // 自身の直前コメントを doc として拾う。
                    let doc = if promoted_to_definition {
                        collect_preceding_comments(parent_node, source)
                    } else {
                        extract_doc_comment(node, source)
                    };
                    // 関数/メソッドの場合のみ循環的複雑度を算出
                    let complexity = if matches!(kind, SymbolKind::Function | SymbolKind::Method) {
                        Some(calculate_complexity(parent_node, lang_id))
                    } else {
                        None
                    };
                    symbols.push(Symbol {
                        name: name.to_string(),
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
    collect_preceding_comments(parent, source)
}

/// node 自身の直前に連続する comment ノードを集めて doc として返す。
fn collect_preceding_comments(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut prev = node.prev_named_sibling();

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
mod tests;
