use anyhow::Result;
use rayon::prelude::*;
use std::path::Path;
use tree_sitter::Node;

use crate::engine::parser;
use crate::language::{LangId, normalize_identifier};
use crate::models::reference::{RefKind, SymbolReference};

/// `find_references` / `find_references_batch` 用の最大並列ワーカー数。
///
/// 数万ファイル級の大規模リポジトリでは rayon fold バケットがワーカー数に比例して
/// `Vec<SymbolReference>` を抱えるため、物理コア数をそのまま使うと RSS が線形に膨張し
/// OOM を招く。`ASTRO_SIGHT_BATCH_WORKERS` で上書き可能。
fn bounded_worker_count() -> usize {
    std::env::var("ASTRO_SIGHT_BATCH_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4)
}

/// 指定シンボルへの参照をディレクトリ内のファイルから検索する。
/// glob パターン（例: "**/*.rs"）によるフィルタも可能。
pub fn find_references(
    symbol_name: &str,
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<Vec<SymbolReference>> {
    let files = collect_files(dir, glob_pattern)?;

    let worker_limit = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(bounded_worker_count());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_limit)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build rayon pool: {e}"))?;

    // per-file Vec を全ファイル分保持せず、worker local の Vec へ直接統合する。
    let mut all_refs: Vec<SymbolReference> = pool.install(|| {
        files
            .into_par_iter()
            .fold(Vec::new, |mut local, path| {
                if let Some(path_str) = path.to_str() {
                    let utf8_path = camino::Utf8Path::new(path_str);
                    if let Ok(mut refs) = find_refs_in_file(symbol_name, utf8_path) {
                        local.append(&mut refs);
                    }
                }
                local
            })
            .reduce(Vec::new, |mut acc, mut local| {
                acc.append(&mut local);
                acc
            })
    });

    sort_references(&mut all_refs);

    Ok(all_refs)
}

fn sort_references(refs: &mut [SymbolReference]) {
    // ソート: 定義を先頭に、その後パス/行番号順
    refs.sort_by(|a, b| {
        let def_order = |k: &Option<RefKind>| match k {
            Some(RefKind::Definition) => 0,
            _ => 1,
        };
        def_order(&a.kind)
            .cmp(&def_order(&b.kind))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
}

/// ignore クレートでファイルを収集する（.gitignore 対応）。
pub fn collect_files(dir: &Path, glob_pattern: Option<&str>) -> Result<Vec<std::path::PathBuf>> {
    collect_files_with_excludes(dir, glob_pattern, &[], &[])
}

/// ignore クレートでファイルを収集し、ディレクトリ名またはネガティブ glob で除外する。
///
/// - `excluded_dir_names`: 完全一致するパスセグメント (例: `vendor`, `node_modules`) を
///   含むファイルを除外。軽量な判定用。
/// - `excluded_globs`: `database/migrations/**` のような glob パターン (ワークスペース相対)。
///   内部で `!<pattern>` として `ignore::overrides` に追加し、パッケージパス内の特定サブ
///   ディレクトリだけをピンポイント除外する。
///
/// 両方が空であれば `collect_files(dir, glob)` と同じ挙動。`.gitignore` は常に尊重する。
pub fn collect_files_with_excludes(
    dir: &Path,
    glob_pattern: Option<&str>,
    excluded_dir_names: &[&str],
    excluded_globs: &[&str],
) -> Result<Vec<std::path::PathBuf>> {
    use ignore::WalkBuilder;

    let mut builder = WalkBuilder::new(dir);
    builder.hidden(true).git_ignore(true).git_global(true);

    // glob フィルタと除外 glob を同じ OverrideBuilder にまとめる。
    // ignore::overrides は「ポジティブパターンがある → その中だけ許可 / ネガティブ (`!`
    // 接頭辞) → 除外」なので、glob_pattern が None のときも `**/*` を足してから
    // `!excluded_globs...` を重ねることで「全体許可 + 指定分だけ除外」を実現する。
    if glob_pattern.is_some() || !excluded_globs.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(dir);
        if let Some(pattern) = glob_pattern {
            ob.add(pattern)?;
        } else if !excluded_globs.is_empty() {
            ob.add("**/*")?;
        }
        for pat in excluded_globs {
            let negated = if pat.starts_with('!') {
                pat.to_string()
            } else {
                format!("!{pat}")
            };
            ob.add(&negated)?;
        }
        builder.overrides(ob.build()?);
    }

    let mut files = Vec::new();
    for entry in builder.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if !excluded_dir_names.is_empty() && path_has_excluded_segment(&path, excluded_dir_names) {
            continue;
        }
        // パース可能なファイルのみ対象
        if LangId::from_path(camino::Utf8Path::new(path.to_str().unwrap_or(""))).is_ok() {
            files.push(path);
        }
    }

    Ok(files)
}

/// パスのいずれかの中間ディレクトリ名が除外対象と完全一致するかを判定する。
fn path_has_excluded_segment(path: &Path, excluded: &[&str]) -> bool {
    path.components().any(|c| match c.as_os_str().to_str() {
        Some(name) => excluded.contains(&name),
        None => false,
    })
}

/// 単一ファイル内でシンボル参照を検索する。
fn find_refs_in_file(symbol_name: &str, path: &camino::Utf8Path) -> Result<Vec<SymbolReference>> {
    let source = parser::read_file(path)?;

    // ファイル言語を拡張子から先読みし、CI 言語ではバイト事前フィルタを skip
    // (memchr は case-sensitive のため Xojo の `MyVar`/`myvar` 一致を取りこぼす)。
    let ext_lang = LangId::from_path(path).ok();
    let is_ci = ext_lang.is_some_and(|l| l.is_case_insensitive());
    if !is_ci && memchr::memmem::find(&source, symbol_name.as_bytes()).is_none() {
        return Ok(Vec::new());
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();

    let mut refs = Vec::new();
    let definition_kinds = definition_node_kinds(lang_id);
    let target = normalize_identifier(lang_id, symbol_name);
    collect_identifier_refs(
        root,
        &source,
        target.as_ref(),
        path.as_str(),
        definition_kinds,
        lang_id,
        &mut refs,
    );

    Ok(refs)
}

/// AST を再帰走査し、指定シンボル名に一致する identifier ノードを収集する。
/// `symbol_name` は言語に応じて正規化済みであることが前提。
fn collect_identifier_refs(
    node: Node<'_>,
    source: &[u8],
    symbol_name: &str,
    path: &str,
    definition_kinds: &[&str],
    lang_id: LangId,
    refs: &mut Vec<SymbolReference>,
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && normalize_identifier(lang_id, text).as_ref() == symbol_name
    {
        let is_def = is_definition_context(node, definition_kinds, lang_id);
        let context = extract_line_context(source, node.start_position().row);

        refs.push(SymbolReference {
            path: path.to_string(),
            line: node.start_position().row,
            column: node.start_position().column,
            context: Some(context),
            kind: Some(if is_def {
                RefKind::Definition
            } else {
                RefKind::Reference
            }),
        });
    }

    // Rust の serde 属性文字列値を識別子参照として扱う。
    for (seg, row, col) in rust_attr_string_ref_segments(node, source, lang_id) {
        if normalize_identifier(lang_id, seg).as_ref() == symbol_name {
            refs.push(SymbolReference {
                path: path.to_string(),
                line: row,
                column: col,
                context: Some(extract_line_context(source, row)),
                kind: Some(RefKind::Reference),
            });
        }
    }

    // PHP の callable array `[Foo::class, 'method']` の string literal を ref として扱う (N3)。
    if let Some((method, row, col)) = php_callable_array_method_segment(node, source, lang_id)
        && normalize_identifier(lang_id, method).as_ref() == symbol_name
    {
        refs.push(SymbolReference {
            path: path.to_string(),
            line: row,
            column: col,
            context: Some(extract_line_context(source, row)),
            kind: Some(RefKind::Reference),
        });
    }

    // PHP の文字列 callable `'Class@method'` / `Class::class . '@method'` を ref として扱う (N4)。
    if let Some((method, row, col)) = php_string_callable_method_segment(node, source, lang_id)
        && normalize_identifier(lang_id, method).as_ref() == symbol_name
    {
        refs.push(SymbolReference {
            path: path.to_string(),
            line: row,
            column: col,
            context: Some(extract_line_context(source, row)),
            kind: Some(RefKind::Reference),
        });
    }

    // 子ノードを再帰走査
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_refs(
            child,
            source,
            symbol_name,
            path,
            definition_kinds,
            lang_id,
            refs,
        );
    }
}

/// この identifier ノードが定義コンテキストにあるかを判定する。
fn is_definition_context(node: Node<'_>, definition_kinds: &[&str], lang_id: LangId) -> bool {
    if lang_id == LangId::Ruby {
        return is_ruby_definition_context(node);
    }
    if lang_id == LangId::Php {
        return is_php_definition_context(node);
    }

    if let Some(parent) = node.parent() {
        // 親ノードが定義ノードかチェック
        if definition_kinds.contains(&parent.kind()) {
            return true;
        }
        // 祖父ノードもチェック（例: function_declarator > identifier）
        if let Some(grandparent) = parent.parent()
            && definition_kinds.contains(&grandparent.kind())
        {
            return true;
        }
    }
    false
}

/// PHP: 識別子が「宣言の `name` フィールド」であるときだけ `Definition` とみなす。
///
/// 単純な parent/grandparent 走査では `class Derived extends AbstractBase` の
/// `AbstractBase` や `implements InterfaceX` の `InterfaceX` が grandparent
/// `class_declaration` にぶら下がって def と誤判定され、継承ツリーを経由した
/// 参照がすべて 0 件になる (dead-code が基底 class / interface を大量に FP とする根因)。
/// field_name が "name" のものだけを定義と数え、`base_clause` / `class_interface_clause` /
/// `use_declaration` 等の中の識別子は ref として分類する。
fn is_php_definition_context(node: Node<'_>) -> bool {
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

fn is_ruby_definition_context(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };

    match parent.kind() {
        "method" | "singleton_method" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "assignment" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        "class" | "module" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "scope_resolution" => {
            let is_name = parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id());
            if !is_name {
                return false;
            }

            if let Some(grandparent) = parent.parent() {
                match grandparent.kind() {
                    "assignment" => grandparent
                        .child_by_field_name("left")
                        .is_some_and(|left| left.id() == parent.id()),
                    "class" | "module" => grandparent
                        .child_by_field_name("name")
                        .is_some_and(|name| name.id() == parent.id()),
                    _ => false,
                }
            } else {
                false
            }
        }
        _ => false,
    }
}

/// 言語ごとの定義ノード種別を返す。
/// 静的スライスを返すことで毎回の Vec アロケーションを回避する。
fn definition_node_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &[
            "function_item",
            "function_signature_item", // trait メソッド宣言（ボディなし）
            "struct_item",
            "enum_item",
            "trait_item",
            "impl_item",
            "const_item",
            "static_item",
            "type_item",
            "mod_item",
        ],
        LangId::C => &["function_definition", "struct_specifier", "enum_specifier"],
        LangId::Cpp => &[
            "function_definition",
            "struct_specifier",
            "class_specifier",
            "enum_specifier",
            "namespace_definition",
        ],
        LangId::Python => &["function_definition", "class_definition"],
        LangId::Javascript => &[
            "function_declaration",
            "class_declaration",
            "method_definition",
            "variable_declarator",
        ],
        LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "class_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            "variable_declarator",
        ],
        LangId::Go => &[
            "package_clause",
            "function_declaration",
            "method_declaration",
            "type_spec",
        ],
        LangId::Php => &[
            "function_definition",
            "class_declaration",
            "method_declaration",
            "interface_declaration",
            "enum_declaration",
            "trait_declaration",
        ],
        LangId::Java => &[
            "method_declaration",
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        LangId::Kotlin => &[
            "function_declaration",
            "class_declaration",
            "object_declaration",
        ],
        LangId::Swift => &[
            "function_declaration",
            "class_declaration",
            "protocol_declaration",
        ],
        LangId::CSharp => &[
            "namespace_declaration",
            "method_declaration",
            "class_declaration",
            "struct_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        LangId::Bash => &["function_definition"],
        LangId::Ruby => &[
            "method",
            "singleton_method",
            "class",
            "module",
            "assignment",
        ],
        LangId::Zig => &[
            "function_declaration",
            "variable_declaration",
            "test_declaration",
            "struct_declaration",
            "enum_declaration",
            "union_declaration",
        ],
        LangId::Xojo => &[
            "class_declaration",
            "module_declaration",
            "interface_declaration",
            "structure_declaration",
            "enum_declaration",
            "sub_declaration",
            "function_declaration",
            "constructor_declaration",
            "destructor_declaration",
            "event_declaration",
            "delegate_declaration",
            "simple_property_declaration",
            "computed_property_declaration",
            "const_declaration",
            "field_declaration",
            "declare_declaration",
        ],
    }
}

/// identifier ノードかどうかを判定する。
fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "simple_identifier"
            | "namespace_identifier"
            | "package_identifier"
            | "name"
            | "qualified_name"
            | "word"
            | "constant"
    )
}

/// Rust の属性引数で文字列値を識別子/パス参照として解釈すべきキー。
/// serde 系の `#[serde(serialize_with = "path::to::fn")]` 形式を想定する。
const RUST_ATTR_STRING_REF_KEYS: &[&str] = &[
    "serialize_with",
    "deserialize_with",
    "with",
    "skip_serializing_if",
    "try_from",
    "from",
    "into",
];

/// `string_content` ノードが Rust の serde 系属性値として現れるかを判定する。
/// 構造: `attribute > token_tree > identifier "=" string_literal > string_content`
fn is_rust_attribute_ref_string(node: Node<'_>, source: &[u8]) -> bool {
    let Some(string_literal) = node.parent() else {
        return false;
    };
    if string_literal.kind() != "string_literal" {
        return false;
    }
    let Some(token_tree) = string_literal.parent() else {
        return false;
    };
    if token_tree.kind() != "token_tree" {
        return false;
    }

    // token_tree の直下兄弟で `identifier "=" string_literal` の並びを検出する。
    let mut cursor = token_tree.walk();
    let mut prev_prev: Option<Node> = None;
    let mut prev: Option<Node> = None;
    for child in token_tree.children(&mut cursor) {
        if child.id() == string_literal.id() {
            let Some(eq) = prev else {
                return false;
            };
            if eq.kind() != "=" {
                return false;
            }
            let Some(key) = prev_prev else {
                return false;
            };
            if key.kind() != "identifier" {
                return false;
            }
            let Ok(key_text) = key.utf8_text(source) else {
                return false;
            };
            return RUST_ATTR_STRING_REF_KEYS.contains(&key_text);
        }
        prev_prev = prev;
        prev = Some(child);
    }
    false
}

/// "Option::is_none" を [("Option", 0), ("is_none", 8)] のように (segment, byte offset) で分割する。
fn split_path_segments(text: &str) -> Vec<(&str, usize)> {
    let mut results = Vec::new();
    let mut offset = 0usize;
    for seg in text.split("::") {
        if !seg.is_empty() {
            results.push((seg, offset));
        }
        offset += seg.len() + 2; // "::"
    }
    results
}

/// Rust 属性の string_content から (segment, row, col) を列挙する。
/// 非 Rust やパターンに合わない場合は空 Vec を返す。
fn rust_attr_string_ref_segments<'a>(
    node: Node<'_>,
    source: &'a [u8],
    lang_id: LangId,
) -> Vec<(&'a str, usize, usize)> {
    if lang_id != LangId::Rust || node.kind() != "string_content" {
        return Vec::new();
    }
    if !is_rust_attribute_ref_string(node, source) {
        return Vec::new();
    }
    let Ok(text) = node.utf8_text(source) else {
        return Vec::new();
    };
    let base = node.start_position();
    split_path_segments(text)
        .into_iter()
        .map(|(seg, off)| (seg, base.row, base.column + off))
        .collect()
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
fn php_callable_array_method_segment<'a>(
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
fn php_string_callable_method_segment<'a>(
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

/// 指定行のソース行をコンテキストとして抽出する。
/// minified/生成コードの巨大行によるメモリ爆発を防ぐため 256B で切り詰める。
/// `memchr` で該当行の範囲のみ走査し、ソース全体の UTF-8 検証は行わない。
/// これにより 1 ファイル内で N 識別子を処理するとき O(N × filesize) → O(N × row + filesize) に削減する。
fn extract_line_context(source: &[u8], row: usize) -> String {
    const MAX_CTX: usize = 256;
    // row 行目の開始位置を memchr で高速に特定する
    let mut line_start = 0usize;
    for _ in 0..row {
        match memchr::memchr(b'\n', &source[line_start..]) {
            Some(nl) => line_start += nl + 1,
            None => return String::new(),
        }
    }
    let line_end = memchr::memchr(b'\n', &source[line_start..])
        .map(|n| line_start + n)
        .unwrap_or(source.len());

    // 必要な範囲のみ UTF-8 変換する（失敗時は空コンテキストを返す）
    let line = std::str::from_utf8(&source[line_start..line_end])
        .unwrap_or("")
        .trim();
    if line.len() <= MAX_CTX {
        line.to_string()
    } else {
        // UTF-8 境界で安全に切り詰める
        let truncated = &line[..line.floor_char_boundary(MAX_CTX)];
        format!("{truncated}...")
    }
}

// ---------------------------------------------------------------------------
// バッチ参照検索: O(S × N) ではなく O(N + S) で処理する
// ---------------------------------------------------------------------------

/// 全シンボル名の参照を1回のディレクトリウォークで検索する。
/// シンボル名→参照リストのマップを返す。
/// Aho-Corasick オートマトンによる効率的なマルチパターン事前フィルタを使用。
///
/// fold/reduce でワーカー局所バケットに直接統合し、
/// per_file Vec + merged HashMap の二重保持を回避する。
pub fn find_references_batch(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<std::collections::HashMap<String, Vec<SymbolReference>>> {
    use std::collections::HashMap;

    if symbol_names.is_empty() {
        return Ok(HashMap::new());
    }

    // AC は ASCII CI で構築: CI 言語 (Xojo) で case 違いを事前フィルタで取りこぼさないため。
    // 非 CI 言語では多少の false positive (大文字小文字違い) が発生するが、AST 比較で弾く。
    let ac = aho_corasick::AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(symbol_names)
        .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))?;

    let files = collect_files(dir, glob_pattern)?;

    // rayon のワーカー数を上限付きにする。ワーカー毎に `Vec<Vec<SymbolReference>>` の
    // fold バケットが生成されるため、大規模リポジトリではワーカー数 × 参照件数に比例して
    // ピーク RSS が線形増大する。
    // 物理コア数と上限のうち小さい方を採用し、バケット総量を押さえつつ並列性を維持する。
    let worker_limit = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(bounded_worker_count());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_limit)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build rayon pool: {e}"))?;

    // fold/reduce: ワーカーごとに Vec<Vec<SymbolReference>> を持ち、直接統合
    let mut buckets: Vec<Vec<SymbolReference>> = pool.install(|| {
        files
            .into_par_iter()
            .fold(
                || vec![Vec::new(); symbol_names.len()],
                |mut local, path| {
                    let Some(path_str) = path.to_str() else {
                        return local;
                    };
                    let utf8_path = camino::Utf8Path::new(path_str);
                    if let Ok(per_file) =
                        find_refs_batch_in_file_indexed(symbol_names, &ac, utf8_path)
                    {
                        for (ix, mut refs) in per_file.into_iter().enumerate() {
                            local[ix].append(&mut refs);
                        }
                    }
                    local
                },
            )
            .reduce(
                || vec![Vec::new(); symbol_names.len()],
                |mut acc, mut local| {
                    for (acc_refs, local_refs) in acc.iter_mut().zip(local.iter_mut()) {
                        acc_refs.append(local_refs);
                    }
                    acc
                },
            )
    });

    let mut merged = HashMap::with_capacity(symbol_names.len());
    for (i, name) in symbol_names.iter().enumerate() {
        let mut refs = std::mem::take(&mut buckets[i]);
        sort_references(&mut refs);
        if !refs.is_empty() {
            merged.insert(name.clone(), refs);
        }
    }

    Ok(merged)
}

/// impact analyze 用: symbol_names を AC 事前フィルタで 1 回構築して返す。
/// streaming Pass から per-file 呼び出しのためのユーティリティ。
pub(crate) fn build_ac_case_insensitive(
    symbol_names: &[String],
) -> Result<aho_corasick::AhoCorasick> {
    aho_corasick::AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(symbol_names)
        .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))
}

/// dead-code 判定用 (test-only 分類版): 各シンボルの非 Definition 参照件数を
/// production と test それぞれ別カウントで返す。
///
/// `is_test` predicate は呼び出し側から渡す (例: `is_test_path` / ディレクトリセグメント判定)。
/// 戻り値は `HashMap<symbol_name, (production_count, test_count)>`。
pub fn count_non_definition_refs_split<F>(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
    is_test: F,
) -> Result<std::collections::HashMap<String, (usize, usize)>>
where
    F: Fn(&Path) -> bool + Sync,
{
    use std::collections::HashMap;

    if symbol_names.is_empty() {
        return Ok(HashMap::new());
    }

    let ac = aho_corasick::AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(symbol_names)
        .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))?;

    let files = collect_files(dir, glob_pattern)?;

    let n = symbol_names.len();
    let (prod_counts, test_counts): (Vec<usize>, Vec<usize>) = files
        .into_par_iter()
        .fold(
            || (vec![0usize; n], vec![0usize; n]),
            |(mut prod, mut test), path| {
                let Some(path_str) = path.to_str() else {
                    return (prod, test);
                };
                let utf8_path = camino::Utf8Path::new(path_str);
                if let Ok(per_file) = count_refs_in_file(symbol_names, &ac, utf8_path) {
                    let bucket = if is_test(&path) { &mut test } else { &mut prod };
                    for (ix, cnt) in per_file.into_iter().enumerate() {
                        bucket[ix] += cnt;
                    }
                }
                (prod, test)
            },
        )
        .reduce(
            || (vec![0usize; n], vec![0usize; n]),
            |(mut acc_p, mut acc_t), (lp, lt)| {
                for (a, b) in acc_p.iter_mut().zip(lp) {
                    *a += b;
                }
                for (a, b) in acc_t.iter_mut().zip(lt) {
                    *a += b;
                }
                (acc_p, acc_t)
            },
        );

    let mut out = HashMap::with_capacity(n);
    for (i, name) in symbol_names.iter().enumerate() {
        out.insert(name.clone(), (prod_counts[i], test_counts[i]));
    }
    Ok(out)
}

/// visitor callback 版の per-file ref 走査。
///
/// `SymbolReference` を 1 件も生成せず、identifier にヒットした瞬間に `visitor.on_ref`
/// を直接呼ぶため、per-file の `Vec<Vec<SymbolReference>>` に起因する heap 確保を完全に
/// 廃止できる。呼び出し側（impact streaming Pass）で filter + intern まで一気に処理する。
pub(crate) fn visit_refs_and_defs_in_file_cb<V: RefVisitor>(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
    visitor: &mut V,
) -> Result<()> {
    use std::collections::HashSet;

    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    let mut present_indices: HashSet<usize> = HashSet::new();
    for mat in ac.find_overlapping_iter(source.as_bytes()) {
        present_indices.insert(mat.pattern().as_usize());
        if present_indices.len() == num {
            break;
        }
    }
    if present_indices.is_empty() {
        return Ok(());
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    let mut name_to_ix: std::collections::HashMap<std::borrow::Cow<'_, str>, Vec<usize>> =
        std::collections::HashMap::with_capacity(present_indices.len());
    for &i in &present_indices {
        let key = normalize_identifier(lang_id, symbol_names[i].as_str());
        name_to_ix.entry(key).or_default().push(i);
    }

    collect_refs_and_defs_indexed_cb(
        root,
        &source,
        &name_to_ix,
        definition_kinds,
        lang_id,
        visitor,
    );
    Ok(())
}

/// `visit_refs_and_defs_in_file_cb` が内部で呼び出す訪問者 trait。
/// Xojo の case-insensitive 多重 index (`name_to_ix[key]` が `Vec<usize>`) や
/// Rust attribute 文字列内参照の場合も、ヒットしたすべての sym_ix について
/// 1 回ずつ `on_ref` が呼ばれる。
pub(crate) trait RefVisitor {
    fn on_ref(&mut self, sym_ix: u32, line: usize, column: usize, context: &str, is_def: bool);
}

/// `visit_refs_and_defs_in_file_cb` 用の AST 再帰走査。Identifier と Rust attribute 文字列
/// 参照を発見したら `visitor.on_ref` を直接呼び、`Vec<SymbolReference>` を一切生成しない。
fn collect_refs_and_defs_indexed_cb<V: RefVisitor>(
    node: Node<'_>,
    source: &[u8],
    name_to_ix: &std::collections::HashMap<std::borrow::Cow<'_, str>, Vec<usize>>,
    definition_kinds: &[&str],
    lang_id: LangId,
    visitor: &mut V,
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && let Some(indices) = name_to_ix.get(&normalize_identifier(lang_id, text))
    {
        let is_def = is_definition_context(node, definition_kinds, lang_id);
        let context = extract_line_context(source, node.start_position().row);
        let line = node.start_position().row;
        let column = node.start_position().column;
        for &ix in indices {
            visitor.on_ref(ix as u32, line, column, &context, is_def);
        }
    }

    for (seg, row, col) in rust_attr_string_ref_segments(node, source, lang_id) {
        if let Some(indices) = name_to_ix.get(&normalize_identifier(lang_id, seg)) {
            let context = extract_line_context(source, row);
            for &ix in indices {
                visitor.on_ref(ix as u32, row, col, &context, false);
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_refs_and_defs_indexed_cb(
            child,
            source,
            name_to_ix,
            definition_kinds,
            lang_id,
            visitor,
        );
    }
}

/// 単一ファイル内で複数シンボルの参照を index ベースの Vec に格納する。
/// find_references_batch の fold/reduce および impact analyze の streaming Pass から呼ばれる。
pub(crate) fn find_refs_batch_in_file_indexed(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
) -> Result<Vec<Vec<SymbolReference>>> {
    use std::collections::HashSet;

    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    // マルチパターン事前フィルタ (AC は ASCII CI で構築済、超集合フィルタ)
    let mut present_indices: HashSet<usize> = HashSet::new();
    for mat in ac.find_overlapping_iter(source.as_bytes()) {
        present_indices.insert(mat.pattern().as_usize());
        if present_indices.len() == num {
            break;
        }
    }

    if present_indices.is_empty() {
        return Ok(vec![Vec::new(); num]);
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    // 言語別に正規化済みキーで name_to_ix を再構築する (Xojo では case 違いを吸収)。
    // CI 言語では `Foo` と `foo` のように正規化後キーが衝突し得るため、
    // 単一 index ではなく Vec<usize> を値として保持し、全シンボルに参照を配る。
    let mut name_to_ix: std::collections::HashMap<std::borrow::Cow<'_, str>, Vec<usize>> =
        std::collections::HashMap::with_capacity(present_indices.len());
    for &i in &present_indices {
        let key = normalize_identifier(lang_id, symbol_names[i].as_str());
        name_to_ix.entry(key).or_default().push(i);
    }

    let mut result = vec![Vec::new(); num];
    collect_identifier_refs_indexed(
        root,
        &source,
        &name_to_ix,
        path.as_str(),
        definition_kinds,
        lang_id,
        &mut result,
    );

    Ok(result)
}

/// AST を再帰走査し、シンボル index ベースの Vec に参照を格納する。
/// CI 言語（Xojo）で正規化後キーが衝突する場合でも全 index に参照を配るため、
/// 値は `Vec<usize>` を受け取る。
fn collect_identifier_refs_indexed(
    node: Node<'_>,
    source: &[u8],
    name_to_ix: &std::collections::HashMap<std::borrow::Cow<'_, str>, Vec<usize>>,
    path: &str,
    definition_kinds: &[&str],
    lang_id: LangId,
    refs: &mut [Vec<SymbolReference>],
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && let Some(indices) = name_to_ix.get(&normalize_identifier(lang_id, text))
    {
        let is_def = is_definition_context(node, definition_kinds, lang_id);
        let context = extract_line_context(source, node.start_position().row);
        let line = node.start_position().row;
        let column = node.start_position().column;
        let kind = if is_def {
            RefKind::Definition
        } else {
            RefKind::Reference
        };

        for &ix in indices {
            refs[ix].push(SymbolReference {
                path: path.to_string(),
                line,
                column,
                context: Some(context.clone()),
                kind: Some(kind),
            });
        }
    }

    // Rust の serde 属性文字列値を識別子参照として扱う。
    for (seg, row, col) in rust_attr_string_ref_segments(node, source, lang_id) {
        if let Some(indices) = name_to_ix.get(&normalize_identifier(lang_id, seg)) {
            let context = extract_line_context(source, row);
            for &ix in indices {
                refs[ix].push(SymbolReference {
                    path: path.to_string(),
                    line: row,
                    column: col,
                    context: Some(context.clone()),
                    kind: Some(RefKind::Reference),
                });
            }
        }
    }

    // PHP の callable array `[Foo::class, 'method']` の string literal を ref として扱う (N3)。
    if let Some((method, row, col)) = php_callable_array_method_segment(node, source, lang_id)
        && let Some(indices) = name_to_ix.get(&normalize_identifier(lang_id, method))
    {
        let context = extract_line_context(source, row);
        for &ix in indices {
            refs[ix].push(SymbolReference {
                path: path.to_string(),
                line: row,
                column: col,
                context: Some(context.clone()),
                kind: Some(RefKind::Reference),
            });
        }
    }

    // PHP の文字列 callable `'Class@method'` / `Class::class . '@method'` を ref として扱う (N4)。
    if let Some((method, row, col)) = php_string_callable_method_segment(node, source, lang_id)
        && let Some(indices) = name_to_ix.get(&normalize_identifier(lang_id, method))
    {
        let context = extract_line_context(source, row);
        for &ix in indices {
            refs[ix].push(SymbolReference {
                path: path.to_string(),
                line: row,
                column: col,
                context: Some(context.clone()),
                kind: Some(RefKind::Reference),
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_refs_indexed(
            child,
            source,
            name_to_ix,
            path,
            definition_kinds,
            lang_id,
            refs,
        );
    }
}

/// 単一ファイル内の非 Definition 参照件数をカウントする（SymbolReference を確保しない）。
fn count_refs_in_file(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
) -> Result<Vec<usize>> {
    use std::collections::HashSet;

    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    let mut present_indices: HashSet<usize> = HashSet::new();
    for mat in ac.find_overlapping_iter(source.as_bytes()) {
        present_indices.insert(mat.pattern().as_usize());
        if present_indices.len() == num {
            break;
        }
    }

    if present_indices.is_empty() {
        return Ok(vec![0; num]);
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    // 言語別に正規化キーで name_to_ix を構築
    let name_to_ix: std::collections::HashMap<std::borrow::Cow<'_, str>, usize> = present_indices
        .iter()
        .map(|&i| (normalize_identifier(lang_id, symbol_names[i].as_str()), i))
        .collect();

    let mut counts = vec![0usize; num];
    count_identifier_refs(
        root,
        &source,
        &name_to_ix,
        definition_kinds,
        lang_id,
        &mut counts,
    );

    Ok(counts)
}

/// AST を再帰走査し、非 Definition 参照の件数のみカウントする。
fn count_identifier_refs(
    node: Node<'_>,
    source: &[u8],
    name_to_ix: &std::collections::HashMap<std::borrow::Cow<'_, str>, usize>,
    definition_kinds: &[&str],
    lang_id: LangId,
    counts: &mut [usize],
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && let Some(&ix) = name_to_ix.get(&normalize_identifier(lang_id, text))
        && !is_definition_context(node, definition_kinds, lang_id)
    {
        counts[ix] += 1;
    }

    // Rust の serde 属性文字列値を非 Definition 参照としてカウントする。
    for (seg, _row, _col) in rust_attr_string_ref_segments(node, source, lang_id) {
        if let Some(&ix) = name_to_ix.get(&normalize_identifier(lang_id, seg)) {
            counts[ix] += 1;
        }
    }

    // PHP の callable array `[Foo::class, 'method']` の string literal を ref とする (N3)。
    if let Some((method, _row, _col)) = php_callable_array_method_segment(node, source, lang_id)
        && let Some(&ix) = name_to_ix.get(&normalize_identifier(lang_id, method))
    {
        counts[ix] += 1;
    }

    // PHP の文字列 callable `'Class@method'` / `Class::class . '@method'` を ref とする (N4)。
    if let Some((method, _row, _col)) = php_string_callable_method_segment(node, source, lang_id)
        && let Some(&ix) = name_to_ix.get(&normalize_identifier(lang_id, method))
    {
        counts[ix] += 1;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count_identifier_refs(child, source, name_to_ix, definition_kinds, lang_id, counts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PHP の callable array `[Class::class, 'method']` で string ノードの中身が
    /// method ref として返されることを検証 (N3 unit-level)。
    #[test]
    fn php_callable_array_method_segment_extracts_method_string() {
        let source = b"<?php\nclass C {\n    public function h() { $x = [C::class, 'foo']; return $x; }\n}\n";
        let path = camino::Utf8Path::new("dummy.php");
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        let lang_id = LangId::Php;
        let _ = path; // silence unused warning
        // 再帰で array_creation_expression を探す
        fn find_array<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "array_creation_expression" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_array(child) {
                    return Some(found);
                }
            }
            None
        }
        let arr = find_array(tree.root_node()).expect("array_creation_expression must exist");
        let seg = php_callable_array_method_segment(arr, source, lang_id);
        assert!(
            seg.is_some(),
            "[C::class, 'foo'] should yield a method segment, got None"
        );
        let (m, _row, _col) = seg.unwrap();
        assert_eq!(m, "foo");
    }

    /// 第1要素が `Class::class` でない場合は ref として認めない (誤検出防止)
    #[test]
    fn php_callable_array_method_segment_rejects_non_class_const() {
        // [1, 'foo'] や ['foo', 'bar'] は callable array ではない
        let source = b"<?php\nfunction f() { $x = [1, 'foo']; return $x; }\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        let lang_id = LangId::Php;
        fn find_array<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "array_creation_expression" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_array(child) {
                    return Some(found);
                }
            }
            None
        }
        let arr = find_array(tree.root_node()).expect("array_creation_expression must exist");
        assert!(php_callable_array_method_segment(arr, source, lang_id).is_none());
    }

    /// PHP 文字列 callable `'Cls@method'` 形式で method 部分が抽出されることを検証 (N4)。
    #[test]
    fn php_string_callable_method_segment_extracts_pure_string() {
        let source = b"<?php\nfunction f() { return 'Controller@handle'; }\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        fn find_string<'t>(
            n: tree_sitter::Node<'t>,
            target: &str,
            source: &[u8],
        ) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "string" && n.utf8_text(source).ok() == Some(target) {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_string(child, target, source) {
                    return Some(found);
                }
            }
            None
        }
        let s = find_string(tree.root_node(), "'Controller@handle'", source)
            .expect("string must exist");
        let seg = php_string_callable_method_segment(s, source, LangId::Php);
        assert!(
            seg.is_some(),
            "'Controller@handle' should yield method segment"
        );
        let (m, _r, _c) = seg.unwrap();
        assert_eq!(m, "handle");
    }

    /// PHP `Cls::class . '@method'` concat 右辺 string から method 部分が抽出されることを検証 (N4)。
    #[test]
    fn php_string_callable_method_segment_extracts_concat_segment() {
        let source = b"<?php\nclass C {}\n$x = C::class . '@handler';\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        fn find_string<'t>(
            n: tree_sitter::Node<'t>,
            target: &str,
            source: &[u8],
        ) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "string" && n.utf8_text(source).ok() == Some(target) {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_string(child, target, source) {
                    return Some(found);
                }
            }
            None
        }
        let s = find_string(tree.root_node(), "'@handler'", source).expect("string must exist");
        let seg = php_string_callable_method_segment(s, source, LangId::Php);
        assert!(seg.is_some(), "Cls::class . '@handler' should match");
        let (m, _r, _c) = seg.unwrap();
        assert_eq!(m, "handler");
    }

    /// メール風文字列は method ref として抽出しない (誤検出防止)
    #[test]
    fn php_string_callable_method_segment_rejects_email_like() {
        let source = b"<?php\n$x = 'user@example.com';\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "string" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_string(child) {
                    return Some(found);
                }
            }
            None
        }
        let s = find_string(tree.root_node()).expect("string must exist");
        assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
    }

    /// `P@ssw0rd` のようなパスワード風文字列は class 部分が 1 文字で reject される
    #[test]
    fn php_string_callable_method_segment_rejects_short_class_part() {
        let source = b"<?php\n$x = 'P@ssw0rd';\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "string" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_string(child) {
                    return Some(found);
                }
            }
            None
        }
        let s = find_string(tree.root_node()).expect("string must exist");
        assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
    }

    /// 引数単独の `'@method'` (concat 親ではない) は reject
    #[test]
    fn php_string_callable_method_segment_rejects_bare_at_method() {
        let source = b"<?php\nfunction f($x) {} f('@handler');\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "string" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_string(child) {
                    return Some(found);
                }
            }
            None
        }
        let s = find_string(tree.root_node()).expect("string must exist");
        assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
    }

    /// 既知の identifier ノード種別が true を返すことを検証
    #[test]
    fn is_identifier_kind_matches() {
        assert!(is_identifier_kind("identifier"));
        assert!(is_identifier_kind("type_identifier"));
        assert!(is_identifier_kind("field_identifier"));
        assert!(is_identifier_kind("property_identifier"));
        assert!(is_identifier_kind("constant"));
        assert!(is_identifier_kind("name"));
        assert!(is_identifier_kind("word"));
    }

    /// 非 identifier ノード種別が false を返すことを検証
    #[test]
    fn is_identifier_kind_rejects_non_identifier() {
        assert!(!is_identifier_kind("function_definition"));
        assert!(!is_identifier_kind("block"));
        assert!(!is_identifier_kind("string"));
        assert!(!is_identifier_kind("comment"));
    }

    /// 単一 refs 検索が複数ファイルを横断し、定義を先頭に返すことを検証
    #[test]
    fn find_references_single_search_sorts_definition_first() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        std::fs::write(&a, "pub fn greet() {}\nfn main() { greet(); }\n").unwrap();
        std::fs::write(&b, "fn other() { crate::greet(); }\n").unwrap();

        let refs = find_references("greet", dir.path(), Some("**/*.rs")).unwrap();

        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].kind, Some(RefKind::Definition));
        assert_eq!(refs[0].line, 0);
        assert!(
            refs[1..]
                .iter()
                .all(|r| r.kind != Some(RefKind::Definition))
        );
    }

    /// 指定行のソースが正しく抽出され、前後の空白が除去されることを検証
    #[test]
    fn extract_line_context_basic() {
        let source = b"line0\n  line1  \nline2";
        let ctx = extract_line_context(source, 1);
        assert_eq!(ctx, "line1");
    }

    /// 範囲外の行に対して空文字を返すことを検証
    #[test]
    fn extract_line_context_out_of_range() {
        let source = b"only one line";
        let ctx = extract_line_context(source, 5);
        assert_eq!(ctx, "");
    }

    /// 改行なしで終わる最終行も正しく抽出できることを検証（memchr 版で新規テスト）
    #[test]
    fn extract_line_context_final_line_without_newline() {
        let source = b"first\nsecond";
        let ctx = extract_line_context(source, 1);
        assert_eq!(ctx, "second");
    }

    /// 巨大行を 256 バイト境界で切り詰めることを検証（minified コード防御）
    #[test]
    fn extract_line_context_truncates_long_line() {
        let long = "a".repeat(500);
        let source = format!("line0\n{long}");
        let ctx = extract_line_context(source.as_bytes(), 1);
        assert!(ctx.ends_with("..."), "256 バイト超は省略記号で終わるべき");
        assert!(ctx.len() <= 256 + 3, "256 バイト + '...' 以内に収まるべき");
    }

    /// UTF-8 境界で安全に切り詰められることを検証（マルチバイト文字の分割禁止）
    #[test]
    fn extract_line_context_utf8_boundary_safe() {
        // 「あ」は UTF-8 で 3 バイト。256B 境界を跨ぐ位置に配置する
        let mut long = "a".repeat(254);
        long.push_str("あいうえお");
        let source = format!("x\n{long}");
        let ctx = extract_line_context(source.as_bytes(), 1);
        // UTF-8 境界違反でパニックしないこと
        assert!(ctx.ends_with("..."));
        assert!(std::str::from_utf8(ctx.as_bytes()).is_ok());
    }

    /// Rust の定義ノード種別に function_item と struct_item が含まれることを検証
    #[test]
    fn definition_node_kinds_rust() {
        let kinds = definition_node_kinds(LangId::Rust);
        assert!(kinds.contains(&"function_item"));
        assert!(kinds.contains(&"struct_item"));
        assert!(kinds.contains(&"enum_item"));
        assert!(kinds.contains(&"trait_item"));
    }

    /// Python の定義ノード種別に function_definition と class_definition が含まれることを検証
    #[test]
    fn definition_node_kinds_python() {
        let kinds = definition_node_kinds(LangId::Python);
        assert!(kinds.contains(&"function_definition"));
        assert!(kinds.contains(&"class_definition"));
    }

    /// `split_path_segments` が "::" 区切りの各セグメントとバイトオフセットを返すことを検証
    #[test]
    fn split_path_segments_basic() {
        assert_eq!(split_path_segments("foo"), vec![("foo", 0)]);
        assert_eq!(
            split_path_segments("Option::is_none"),
            vec![("Option", 0), ("is_none", 8)]
        );
        assert_eq!(
            split_path_segments("a::b::c"),
            vec![("a", 0), ("b", 3), ("c", 6)]
        );
        assert!(split_path_segments("").is_empty());
    }

    /// ヘルパー: Rust ソースを tree-sitter でパースしてツリーを返す
    fn parse_rust(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust language");
        parser.parse(source, None).expect("parse rust source")
    }

    /// serde の serialize_with = "..." 内の関数名が参照として収集されることを検証
    #[test]
    fn rust_attr_string_ref_detected_for_serialize_with() {
        let source = r#"
fn serialize_jst() {}
struct Foo;
impl Foo {
    fn placeholder() {}
}
#[derive(Serialize)]
struct Bar {
    #[serde(serialize_with = "serialize_jst")]
    time: i64,
}
"#;
        let tree = parse_rust(source);
        let defs = definition_node_kinds(LangId::Rust);
        let mut refs = Vec::new();
        collect_identifier_refs(
            tree.root_node(),
            source.as_bytes(),
            "serialize_jst",
            "test.rs",
            defs,
            LangId::Rust,
            &mut refs,
        );

        // 定義 1 件 + 属性文字列内参照 1 件
        let def_cnt = refs
            .iter()
            .filter(|r| matches!(r.kind, Some(RefKind::Definition)))
            .count();
        let ref_cnt = refs
            .iter()
            .filter(|r| matches!(r.kind, Some(RefKind::Reference)))
            .count();
        assert_eq!(def_cnt, 1, "definition should be captured");
        assert_eq!(ref_cnt, 1, "serde attribute string ref should be captured");
    }

    /// 属性文字列参照が非 Definition としてカウントされ、dead-code 判定に反映されることを検証
    #[test]
    fn rust_attr_string_ref_counted_as_non_definition() {
        use std::borrow::Cow;
        use std::collections::HashMap;

        let source = r#"
fn serialize_jst() {}
#[derive(Serialize)]
struct Bar {
    #[serde(serialize_with = "serialize_jst")]
    time: i64,
}
"#;
        let tree = parse_rust(source);
        let defs = definition_node_kinds(LangId::Rust);
        let mut name_to_ix: HashMap<Cow<'_, str>, usize> = HashMap::new();
        name_to_ix.insert(Cow::Borrowed("serialize_jst"), 0);
        let mut counts = vec![0usize];
        count_identifier_refs(
            tree.root_node(),
            source.as_bytes(),
            &name_to_ix,
            defs,
            LangId::Rust,
            &mut counts,
        );
        assert_eq!(counts[0], 1, "attribute string ref must lift dead-code");
    }

    /// `Option::is_none` のようなパス文字列では最終セグメントもカウントされることを検証
    #[test]
    fn rust_attr_string_ref_path_segments() {
        let source = r#"
#[derive(Serialize)]
struct Bar {
    #[serde(skip_serializing_if = "Option::is_none")]
    inner: Option<i64>,
}
"#;
        let tree = parse_rust(source);
        let defs = definition_node_kinds(LangId::Rust);
        let mut refs = Vec::new();
        collect_identifier_refs(
            tree.root_node(),
            source.as_bytes(),
            "is_none",
            "test.rs",
            defs,
            LangId::Rust,
            &mut refs,
        );
        assert_eq!(
            refs.len(),
            1,
            "path tail segment should be matched as reference"
        );
    }

    /// 対象外キー (例: rename) の文字列値は参照として扱わないことを検証
    #[test]
    fn rust_attr_string_ref_ignores_non_ref_keys() {
        let source = r#"
#[derive(Serialize)]
struct Bar {
    #[serde(rename = "created_at")]
    time: i64,
}
"#;
        let tree = parse_rust(source);
        let defs = definition_node_kinds(LangId::Rust);
        let mut refs = Vec::new();
        collect_identifier_refs(
            tree.root_node(),
            source.as_bytes(),
            "created_at",
            "test.rs",
            defs,
            LangId::Rust,
            &mut refs,
        );
        assert!(
            refs.is_empty(),
            "rename is not a reference key and must not match"
        );
    }

    /// 非 Rust 言語では属性文字列ヒューリスティックが動作しないことを検証
    #[test]
    fn rust_attr_helper_is_noop_for_other_languages() {
        // Python AST 上に string_content が登場しても反応しない
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("load python language");
        let source = "x = \"serialize_jst\"\n";
        let tree = parser.parse(source, None).unwrap();
        let segs = collect_all_attr_segments(tree.root_node(), source.as_bytes(), LangId::Python);
        assert!(segs.is_empty());
    }

    /// ヘルパー: 木全体で rust_attr_string_ref_segments が拾うセグメントを再帰収集
    fn collect_all_attr_segments<'a>(
        node: Node<'a>,
        source: &'a [u8],
        lang_id: LangId,
    ) -> Vec<(String, usize, usize)> {
        let mut out: Vec<(String, usize, usize)> =
            rust_attr_string_ref_segments(node, source, lang_id)
                .into_iter()
                .map(|(s, r, c)| (s.to_string(), r, c))
                .collect();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            out.extend(collect_all_attr_segments(child, source, lang_id));
        }
        out
    }
}
