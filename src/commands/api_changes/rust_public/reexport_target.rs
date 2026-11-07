//! `pub use` で再公開した名前の解決先の宣言 (Issue #281)。
//!
//! 項目を別のファイルへ移し、元のファイルから `pub use` で再公開した差分で、旧い宣言と比べる新しい
//! 宣言を、同じ名前の候補から推測せずに再公開の経路から求める。次をすべて確かめたときだけ解決する。
//!
//! - 元のファイルが crate の module の木でその module の中身のファイルそのもので (inline にして
//!   残ったファイルは読まれない)、その module が `pub mod` の鎖で公開され、祖先の宣言に条件 (`cfg`
//!   など) が無い (非公開にしたか条件があれば、旧いパスが消え得る)
//! - その module で、その名前を公開する named な `pub use` が 1 つだけで、条件が無い。glob (`*`) と、
//!   `as` で別の名前を公開するもの (宣言の名前が変わる) は扱わない
//! - 経路の起点 (`crate`・`self`・`super`、先頭が識別子なら edition と同じスコープの束縛) から、
//!   crate の中の module に解決される (外部 crate・判定できない束縛は解決しない)
//! - その module と祖先の宣言に条件が無く、`#[path]` が無く、1 回だけ宣言され、ファイルが読めて
//!   構文エラーが無い
//! - その module のスコープに、名前の宣言 (関数・定数・static・構造体・enum・union・trait・
//!   型別名) が 1 つだけあり、条件が無く、同じ名前を束縛し得るもの (use・`extern crate`・同じ名前の
//!   `mod`・item 位置のマクロ・extern ブロック) が無い。そこからさらに再公開するもの (多段) は扱わない
//!
//! 先の宣言が同じ差分で足したものかは、base のその module のスコープで決める。module のファイルが
//! base に無ければ (rename は辿る)、base にある祖先の module のファイルから辿り、途中の module が
//! 無く名前を生成し得るマクロも無ければ、足したものとする。

use super::*;

/// 再公開した名前が指す宣言。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReexportTarget {
    /// 宣言のあるファイル (`dir` 相対、`/` 区切り)。
    pub(crate) file: String,
    /// 宣言の種類 (`function`・`constant`・`struct`・`enum`・`union`・`trait`・`type`)。
    pub(crate) kind: &'static str,
    /// 宣言のある module の、ファイルの中の inline module の名前の並び (ファイルの module なら空)。
    pub(crate) inline: Vec<String>,
    /// 宣言の始まりの位置 (行, 列。0 始まり)。
    pub(crate) start: (usize, usize),
    /// base のその module に、その名前の宣言が無かった (同じ差分で足した)。
    pub(crate) added_in_diff: bool,
}

/// 作業ツリーのパスの、base での中身 (rename は旧いパスを読む)。
pub(crate) enum BaseFile {
    /// base には無い (同じ差分で足したファイル)。
    Missing,
    Content(Vec<u8>),
    /// 確かめられない。
    Unknown,
}

/// 宣言のノードの種類から、シンボルの種類。
fn declaration_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "function_item" => "function",
        "const_item" | "static_item" => "constant",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "union_item" => "union",
        "trait_item" => "trait",
        "type_item" => "type",
        _ => return None,
    })
}

/// `r#` を外した識別子。
fn unraw(text: &str) -> &str {
    text.strip_prefix("r#").unwrap_or(text)
}

/// 字面 `text` が名前 `name` を単語として (`r#` を外して) 含むか。
fn mentions(text: &str, name: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '#'))
        .any(|word| unraw(word) == name)
}

/// `scope` (ファイルの `source_file` か inline module の `declaration_list`) の直下の、名前が
/// `name` の宣言。1 つだけなら Some(Some(..))、無く、ほかに名前を束縛し得るものも無ければ
/// Some(None)。同じ名前を束縛し得るものがほかにあるか、条件が付くか、2 つ以上なら None。
fn scope_declaration<'tree>(
    scope: tree_sitter::Node<'tree>,
    source: &[u8],
    name: &str,
) -> Option<Option<(&'static str, tree_sitter::Node<'tree>)>> {
    let mut found = None;
    let mut cursor = scope.walk();
    for item in scope.named_children(&mut cursor) {
        let text = || item.utf8_text(source).unwrap_or_default();
        let declared_name = || {
            item.child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .map(unraw)
        };
        match item.kind() {
            "attribute_item"
            | "inner_attribute_item"
            | "line_comment"
            | "block_comment"
            | "macro_definition" => {}
            // 名前を持ち込み得るもの。glob はどの名前も持ち込む。
            "use_declaration" | "extern_crate_declaration" | "foreign_mod_item" => {
                if mentions(text(), name) || text().contains('*') {
                    return None;
                }
            }
            // item 位置のマクロ (`make!();` は式の文に包まれる) は、どの名前の宣言も作り得る。
            "macro_invocation" | "expression_statement" => return None,
            "mod_item" => {
                if declared_name() == Some(name) {
                    return None;
                }
            }
            kind => {
                let Some(declared) = declaration_kind(kind) else {
                    continue;
                };
                if declared_name() != Some(name) {
                    continue;
                }
                if found.is_some() || !item_attributes_are_plain(item, source) {
                    return None;
                }
                found = Some((declared, item));
            }
        }
    }
    Some(found)
}

/// `scope` の直下に item 位置のマクロ (`make!();` は式の文に包まれる) があるか。どの名前の宣言も
/// module も作り得る。
fn has_item_macro(scope: tree_sitter::Node<'_>) -> bool {
    let mut cursor = scope.walk();
    scope
        .named_children(&mut cursor)
        .any(|item| matches!(item.kind(), "macro_invocation" | "expression_statement"))
}

/// `root` から inline module の名前の並び `inline` を辿った先の module のスコープ。並びの module が
/// 無く、そこに名前を生成し得るマクロも無ければ Some(None)。同じ名前の module が 2 つ以上あるか、
/// 外部ファイルの module (`mod name;`) か、マクロがあれば None。
fn inline_scope<'tree>(
    root: tree_sitter::Node<'tree>,
    source: &[u8],
    inline: &[String],
) -> Option<Option<tree_sitter::Node<'tree>>> {
    let mut scope = root;
    for name in inline {
        let mut cursor = scope.walk();
        let modules: Vec<tree_sitter::Node<'tree>> = scope
            .named_children(&mut cursor)
            .filter(|item| {
                item.kind() == "mod_item"
                    && item
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        .map(unraw)
                        == Some(name.as_str())
            })
            .collect();
        match modules.as_slice() {
            [] if has_item_macro(scope) => return None,
            [] => return Some(None),
            [module] => scope = module.child_by_field_name("body")?,
            _ => return None,
        }
    }
    Some(Some(scope))
}

/// Rust のソース `source` の、inline module の並び `inline` の module のスコープにある名前 `name` の
/// 宣言の種類。無ければ Some(None) (module ごと無いときも)、構文エラー・同じ名前を束縛し得るもの・
/// 条件付き・2 つ以上なら None。
pub(crate) fn declaration_in_scope(
    source: &[u8],
    inline: &[String],
    name: &str,
) -> Option<Option<&'static str>> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let Some(scope) = inline_scope(tree.root_node(), source, inline)? else {
        return Some(None);
    };
    Some(scope_declaration(scope, source, name)?.map(|(kind, _)| kind))
}

/// Rust のソース `source` の直下にある名前 `name` の唯一の宣言の始まりの位置 (行, 列) と種類
/// ([`declaration_in_scope`] と同じ条件)。
pub(crate) fn root_declaration(
    source: &[u8],
    name: &str,
) -> Option<((usize, usize), &'static str)> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let (kind, node) = scope_declaration(tree.root_node(), source, name)??;
    let start = node.start_position();
    Some(((start.row, start.column), kind))
}

/// ファイル `source` の module のスコープ (直下と inline module の中) にある、名前が `name` の宣言の
/// 数。構文エラーなら None。
pub(crate) fn module_scope_declarations(source: &[u8], name: &str) -> Option<usize> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let mut count = 0;
    let mut scopes = vec![tree.root_node()];
    while let Some(scope) = scopes.pop() {
        let mut cursor = scope.walk();
        for item in scope.named_children(&mut cursor) {
            if item.kind() == "mod_item" {
                scopes.extend(item.child_by_field_name("body"));
                continue;
            }
            if declaration_kind(item.kind()).is_some()
                && item
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(unraw)
                    == Some(name)
            {
                count += 1;
            }
        }
    }
    Some(count)
}

/// 項目に可視性 (`pub`・`pub(crate)` など) が付いているか。
fn has_visibility(item: tree_sitter::Node<'_>) -> bool {
    let mut cursor = item.walk();
    item.named_children(&mut cursor)
        .any(|child| child.kind() == "visibility_modifier")
}

/// 項目の字面を比べるためのトークン (構文木の葉) の並び。名前付きの葉は種類を添える
/// (`identifier:x`)。リテラルは前置きと引用符ごとノード全体を 1 つにし (生の文字列の `r#"` は
/// 構文木の葉に現れない)、中身の空白も保つ (`"a b"` と `"a  b"` は別)。トークンの間の空白とコメント
/// の違いは無視するが、マクロの入力 (トークン木) の中の doc コメントは `#[doc]` 属性として渡るので
/// 足す。連結しないので、区切りの違う並びが同じになることもない。
pub(crate) type Tokens = Vec<String>;

/// ノード全体を 1 つのトークンにするリテラルの種類。
const LITERAL_KINDS: [&str; 6] = [
    "string_literal",
    "raw_string_literal",
    "char_literal",
    "integer_literal",
    "float_literal",
    "boolean_literal",
];

/// `node` のトークンを `tokens` に足す ([`Tokens`])。
pub(crate) fn push_tokens(node: tree_sitter::Node<'_>, source: &[u8], tokens: &mut Tokens) {
    // 深い入れ子 (`Option<Option<…>>`) でスタックを溢れさせないよう、明示のスタックで前順に
    // たどる。`in_macro` はマクロの入力 (トークン木) の中か。
    let mut stack = vec![(node, false)];
    while let Some((node, in_macro)) = stack.pop() {
        let kind = node.kind();
        let text = node.utf8_text(source).unwrap_or_default();
        if matches!(kind, "line_comment" | "block_comment") {
            if in_macro && is_doc_comment(node) {
                tokens.push(format!("{kind}:{text}"));
            }
            continue;
        }
        if node.child_count() == 0 || LITERAL_KINDS.contains(&kind) {
            if !text.is_empty() {
                tokens.push(if node.is_named() {
                    format!("{kind}:{text}")
                } else {
                    text.to_owned()
                });
            }
            continue;
        }
        let in_macro = in_macro || kind == "token_tree";
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev().map(|child| (child, in_macro)));
    }
}

/// コメントが doc コメント (`///`・`//!`・`/** */`・`/*! */`) か。
fn is_doc_comment(node: tree_sitter::Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            "outer_doc_comment_marker" | "inner_doc_comment_marker" | "doc_comment"
        )
    })
}

/// ファイルの中の、型の名前空間の名前の束縛 ([`type_namespace_bindings`])。
pub(crate) struct TypeBindings {
    /// module のスコープ (inline module の見出しのトークンの並び。直下は空) ごとの、名前を束縛し
    /// 得るもののトークン (並べ替え済み)。束縛し得るものの無いスコープは入らない。
    scopes: std::collections::HashMap<Vec<Tokens>, Vec<Tokens>>,
    /// どこかのスコープに item 位置のマクロの呼び出しがある。展開が何を束縛するかはマクロの定義
    /// 次第で、呼び出しの字面からは決まらない。
    item_macros: bool,
}

/// ファイル `source` の module のスコープ (直下と inline module の中) ごとに、型の名前空間に名前
/// `name` を束縛し得るもの (その名前の構造体・enum・union・trait・型別名の宣言、その名前を挙げるか
/// glob の可視性付きの `use`、item 位置のマクロの呼び出し) を集める。項目は外側の属性を含めた字面、
/// inline module は外側と内側の属性を含めた見出しで区別する (`cfg` を入れ替えたものを同じとみなさ
/// ない)。字面はトークンの並び ([`Tokens`]) で比べる。構文エラーなら None。
pub(crate) fn type_namespace_bindings(source: &[u8], name: &str) -> Option<TypeBindings> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let mut bindings = TypeBindings {
        scopes: std::collections::HashMap::new(),
        item_macros: false,
    };
    let mut scopes = vec![(tree.root_node(), Vec::new())];
    while let Some((scope, path)) = scopes.pop() {
        let mut cursor = scope.walk();
        // 次の項目に付く外側の属性のトークン。
        let mut outer: Tokens = Vec::new();
        for item in scope.named_children(&mut cursor) {
            let text = item.utf8_text(source).ok()?;
            match item.kind() {
                "attribute_item" => {
                    push_tokens(item, source, &mut outer);
                    continue;
                }
                "line_comment" | "block_comment" | "inner_attribute_item" => continue,
                _ => {}
            }
            let mut tokens = std::mem::take(&mut outer);
            let declared_name = item
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .map(unraw);
            let binds = match item.kind() {
                "mod_item" => {
                    if let Some(body) = item.child_by_field_name("body") {
                        // 見出し (中身の手前まで) と、中身の内側の属性。
                        let mut header_cursor = item.walk();
                        for child in item.children(&mut header_cursor) {
                            if child.id() != body.id() {
                                push_tokens(child, source, &mut tokens);
                            }
                        }
                        let mut body_cursor = body.walk();
                        for child in body.named_children(&mut body_cursor) {
                            if child.kind() == "inner_attribute_item" {
                                push_tokens(child, source, &mut tokens);
                            }
                        }
                        let mut nested: Vec<Tokens> = path.clone();
                        nested.push(std::mem::take(&mut tokens));
                        scopes.push((body, nested));
                    }
                    false
                }
                "struct_item" | "enum_item" | "union_item" | "trait_item" | "type_item" => {
                    declared_name == Some(name)
                }
                "use_declaration" => {
                    has_visibility(item) && (mentions(text, name) || text.contains('*'))
                }
                "macro_invocation" | "expression_statement" => {
                    bindings.item_macros = true;
                    true
                }
                _ => false,
            };
            if binds {
                push_tokens(item, source, &mut tokens);
                bindings
                    .scopes
                    .entry(path.clone())
                    .or_default()
                    .push(tokens);
            }
        }
    }
    for entries in bindings.scopes.values_mut() {
        entries.sort();
    }
    Some(bindings)
}

/// 先のファイルの `base` と `new` で、型の名前空間の名前 `name` の束縛 ([`type_namespace_bindings`])
/// が変わらないか。base に束縛し得るもののあった module のスコープごとに、`new` の同じスコープに同じ
/// ものが同じ字面で同じ数だけあり、足したものも無いこと (足した明示の束縛は glob の束縛を覆う)。
/// 字面のまま残った束縛は指す先が変わらない。束縛の無かったスコープに足したものは新しいパスで、
/// 別の型を指していたパスではない。base に item 位置のマクロの呼び出しがあれば、定義が変わった
/// (展開が別の名前を束縛する) かを確かめられないので false。読めないか構文エラーなら false。
pub(crate) fn type_namespace_bindings_kept(base: &[u8], new: Option<&[u8]>, name: &str) -> bool {
    let (Some(base), Some(new)) = (
        type_namespace_bindings(base, name),
        new.and_then(|new| type_namespace_bindings(new, name)),
    ) else {
        return false;
    };
    !base.item_macros
        && base
            .scopes
            .iter()
            .all(|(path, entries)| new.scopes.get(path) == Some(entries))
}

/// `file` (`dir` 相対、`/` 区切り) の、`src` 相対の位置から決まる crate の root (`src` の親の
/// `dir` 相対のパス) と module の経路。`src` の下でなければ None。
pub(crate) fn source_module_path(file: &str) -> Option<(String, Vec<String>)> {
    let (package, relative) = match file.rfind("/src/") {
        Some(at) => (&file[..at], &file[at + 5..]),
        None => ("", file.strip_prefix("src/")?),
    };
    if !relative.ends_with(".rs") {
        return None;
    }
    Some((
        package.to_owned(),
        module_path_segments(std::path::Path::new(relative)),
    ))
}

/// module が root から `pub mod` の鎖で公開されているか。
fn module_is_public(module_tree: &ModuleTree, module: &[String]) -> bool {
    (0..module.len()).all(|depth| {
        module_tree
            .pub_children
            .get(&module[..depth])
            .is_some_and(|children| children.contains(&module[depth]))
    })
}

/// base で、module `module` のスコープに名前 `name` の宣言があったか ([`declaration_in_scope`] と
/// 同じ値)。module のファイルが base に無ければ、base にある祖先の module のファイルから辿る。
fn base_declaration(
    module_tree: &ModuleTree,
    info: &RustPrivateModuleInfo,
    module: &[String],
    name: &str,
    base: &dyn Fn(&str) -> BaseFile,
) -> Option<Option<&'static str>> {
    for depth in (0..=module.len()).rev() {
        let location = module_tree.location(&module[..depth])?;
        let file = info.src_root_rel.join(&location.file);
        let file = crate::git_support::normalize_workspace_separators(file.to_str()?);
        match base(&file) {
            BaseFile::Content(source) => {
                let mut inline = location.inline.clone();
                inline.extend(module[depth..].iter().cloned());
                return declaration_in_scope(&source, &inline, name);
            }
            BaseFile::Missing => {}
            BaseFile::Unknown => return None,
        }
    }
    None
}

/// `file` (`dir` 相対) を含む crate の情報 (`src/lib.rs` のある library crate だけ)。module の
/// 経路は `src` 相対のファイルの位置から求める (crate root は空)。
fn crate_module_info(dir: &str, file: &str) -> Option<RustPrivateModuleInfo> {
    use std::path::Path;
    let canonical_dir = std::fs::canonicalize(dir).ok()?;
    let absolute = canonical_dir.join(Path::new(file));
    let mut ancestor = absolute.parent();
    let mut crate_root = None;
    while let Some(directory) = ancestor {
        if directory.join("Cargo.toml").is_file() {
            crate_root = Some(directory.to_path_buf());
            break;
        }
        if directory == canonical_dir {
            break;
        }
        ancestor = directory.parent();
    }
    let crate_root = crate_root?;
    let src_dir = crate_root.join("src");
    if !src_dir.join("lib.rs").is_file() {
        return None;
    }
    let rel_to_src = absolute.strip_prefix(&src_dir).ok()?;
    let crate_root_rel = crate_root.strip_prefix(&canonical_dir).ok()?.to_path_buf();
    Some(RustPrivateModuleInfo {
        src_root_rel: crate_root_rel.join("src"),
        crate_root_rel,
        module_segments: module_path_segments(rel_to_src),
    })
}

/// 作業ツリーの `file` (`dir` 相対、`/` 区切り) の module が名前 `name` で公開する `pub use` の
/// 解決先の宣言。`base` は作業ツリーのパスの base での中身。解決できないか、先の宣言が base に
/// あったかを決められなければ None (呼び出し側は対応を確かめられないものとして扱う)。
pub(crate) fn resolve_reexport_target(
    dir: &str,
    file: &str,
    name: &str,
    base: &dyn Fn(&str) -> BaseFile,
) -> Option<ReexportTarget> {
    use crate::engine::rust_edition::RustEdition;
    use std::path::Path;
    let source = RustSourceTree::Worktree;
    let info = crate_module_info(dir, file)?;
    let module_source = read_rs_blob(source, dir, Path::new(file))?;
    let tree = parser::parse_source(&module_source, crate::language::LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let mut collected = Vec::new();
    collect_pub_use_edges(
        tree.root_node(),
        &module_source,
        &info.module_segments,
        &mut collected,
    )?;
    let mut matching = collected.into_iter().filter(|collected| {
        matches!(
            &collected.edge,
            RustPubUseEdge::Named { source_module, exported_name, .. }
                if *source_module == info.module_segments && exported_name == name
        )
    });
    let (Some(edge), None) = (matching.next(), matching.next()) else {
        return None;
    };
    if !edge.unconditional {
        return None;
    }
    let edition = crate::engine::rust_edition::library_edition(&info.crate_root_rel, |manifest| {
        read_rust_manifest(source, dir, manifest)
    });
    // 2018 以降の、スコープに束縛の無い識別子は extern prelude (依存の crate) を指す。
    if edge.bare_leading == Some(BareLeadingBinding::Unbound) && edition != Some(RustEdition::E2015)
    {
        return None;
    }
    let module_tree = collect_all_modules(source, dir, &info)?;
    // 再公開する module 自身も、`pub mod` の鎖で公開され、条件が無い (旧いパスが残る)。読んだ
    // ファイルは、その module の中身のファイルそのもの (inline にして残ったファイルは読まれない)。
    if !module_tree.pub_children.contains_key(&info.module_segments)
        || !module_is_public(&module_tree, &info.module_segments)
        || !module_tree.is_unconditional_path(&info.module_segments)
    {
        return None;
    }
    let own = module_tree.location(&info.module_segments)?;
    if !own.inline.is_empty()
        || crate::git_support::normalize_workspace_separators(
            info.src_root_rel.join(&own.file).to_str()?,
        ) != file
    {
        return None;
    }
    let resolved = resolve_global_pub_use_edges(vec![edge], edition, || {
        read_rust_module_source(source, dir, &info.crate_root_rel, Path::new("lib.rs"))
    })?;
    let resolved = resolve_bare_pub_use_edges(resolved, &module_tree, || edition)?;
    let [resolved] = resolved.as_slice() else {
        return None;
    };
    let RustPubUseEdge::Named {
        target_module,
        target_item,
        origin,
        ..
    } = &resolved.edge
    else {
        return None;
    };
    if *origin == UsePathOrigin::Global
        || target_item != name
        || !module_tree.pub_children.contains_key(target_module)
        || !module_tree.is_unconditional_path(target_module)
    {
        return None;
    }
    let location = module_tree.location(target_module)?;
    let target_rel = info.src_root_rel.join(&location.file);
    let target_source = read_rs_blob(source, dir, &target_rel)?;
    let target_tree = parser::parse_source(&target_source, crate::language::LangId::Rust).ok()?;
    if target_tree.root_node().has_error() {
        return None;
    }
    let scope = inline_scope(target_tree.root_node(), &target_source, &location.inline)??;
    let (kind, item) = scope_declaration(scope, &target_source, name)??;
    let start = item.start_position();
    let added_in_diff = base_declaration(&module_tree, &info, target_module, name, base)?.is_none();
    Some(ReexportTarget {
        file: crate::git_support::normalize_workspace_separators(target_rel.to_str()?),
        kind,
        inline: location.inline.clone(),
        start: (start.row, start.column),
        added_in_diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
    }

    const CARGO: &str = "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

    /// `from` (既定は `src/lib.rs`) から `name` の再公開を解く。
    fn resolve_from(
        files: &[(&str, &str)],
        from: &str,
        name: &str,
    ) -> Option<(String, &'static str, Vec<String>)> {
        let dir = tempfile::tempdir().unwrap();
        let mut all = vec![("Cargo.toml", CARGO)];
        all.extend_from_slice(files);
        write(dir.path(), &all);
        // base は空の crate root だけ (先の宣言はどれも同じ差分で足したもの)。
        resolve_reexport_target(dir.path().to_str().unwrap(), from, name, &|path| {
            if path == "src/lib.rs" {
                BaseFile::Content(Vec::new())
            } else {
                BaseFile::Missing
            }
        })
        .map(|target| (target.file, target.kind, target.inline))
    }

    fn resolve(files: &[(&str, &str)], name: &str) -> Option<(String, &'static str, Vec<String>)> {
        resolve_from(files, "src/lib.rs", name)
    }

    #[test]
    fn resolves_named_reexports_to_unique_declarations() {
        let detail = ("src/detail.rs", "pub fn greet() {}\npub struct Note;\n");
        for lib in [
            "mod detail;\npub use detail::greet;\n",
            "mod detail;\npub use self::detail::greet;\n",
            "mod detail;\npub use crate::detail::{greet, Note};\n",
            "mod detail;\npub use detail::{self as d, greet};\n",
        ] {
            assert_eq!(
                resolve(&[("src/lib.rs", lib), detail], "greet"),
                Some(("src/detail.rs".to_owned(), "function", Vec::new())),
                "{lib}"
            );
        }
        // `detail/mod.rs` の module と、inline module の中の宣言 (別の inline module の同じ名前は
        // 数えない)
        assert_eq!(
            resolve(
                &[
                    ("src/lib.rs", "mod detail;\npub use detail::Note;\n"),
                    ("src/detail/mod.rs", "pub struct Note;\n"),
                ],
                "Note"
            ),
            Some(("src/detail/mod.rs".to_owned(), "struct", Vec::new()))
        );
        assert_eq!(
            resolve(
                &[(
                    "src/lib.rs",
                    "mod outer {\n    pub mod inner {\n        pub const LIMIT: u8 = 1;\n    }\n    mod other {\n        pub const LIMIT: u8 = 2;\n    }\n}\npub use outer::inner::LIMIT;\n"
                )],
                "LIMIT"
            ),
            Some((
                "src/lib.rs".to_owned(),
                "constant",
                vec!["outer".to_owned(), "inner".to_owned()]
            ))
        );
        // 子の module のファイルからの再公開 (2018 以降の子 module と `super::`)
        assert_eq!(
            resolve_from(
                &[
                    ("src/lib.rs", "pub mod api;\nmod detail;\n"),
                    (
                        "src/api.rs",
                        "mod inner;\npub use inner::greet;\npub use super::detail::Note;\n"
                    ),
                    ("src/api/inner.rs", "pub fn greet() {}\n"),
                    ("src/detail.rs", "pub struct Note;\n"),
                ],
                "src/api.rs",
                "greet"
            ),
            Some(("src/api/inner.rs".to_owned(), "function", Vec::new()))
        );
    }

    #[test]
    fn unprovable_reexports_are_not_resolved() {
        let detail = ("src/detail.rs", "pub fn greet() {}\n");
        for files in [
            // 外部 crate・glob・別名・条件付きの use と module・多段の再公開
            vec![("src/lib.rs", "pub use serde::greet;\n")],
            vec![("src/lib.rs", "mod detail;\npub use detail::*;\n"), detail],
            vec![
                (
                    "src/lib.rs",
                    "mod detail;\npub use detail::other as greet;\n",
                ),
                ("src/detail.rs", "pub fn other() {}\n"),
            ],
            vec![
                (
                    "src/lib.rs",
                    "mod detail;\n#[cfg(feature = \"x\")]\npub use detail::greet;\n",
                ),
                detail,
            ],
            vec![
                (
                    "src/lib.rs",
                    "#[cfg(feature = \"x\")]\nmod detail;\npub use detail::greet;\n",
                ),
                detail,
            ],
            vec![
                ("src/lib.rs", "mod detail;\npub use detail::greet;\n"),
                ("src/detail.rs", "mod inner;\npub use inner::greet;\n"),
                ("src/detail/inner.rs", "pub fn greet() {}\n"),
            ],
            // 宣言が 2 つ・条件付き・マクロ・#[path]
            vec![
                ("src/lib.rs", "mod detail;\npub use detail::greet;\n"),
                (
                    "src/detail.rs",
                    "pub fn greet() {}\n#[allow(non_snake_case)]\npub struct greet;\n",
                ),
            ],
            vec![
                ("src/lib.rs", "mod detail;\npub use detail::greet;\n"),
                ("src/detail.rs", "#[cfg(unix)]\npub fn greet() {}\n"),
            ],
            vec![
                ("src/lib.rs", "mod detail;\npub use detail::greet;\n"),
                ("src/detail.rs", "make!();\npub fn greet() {}\n"),
            ],
            vec![
                (
                    "src/lib.rs",
                    "#[path = \"other.rs\"]\nmod detail;\npub use detail::greet;\n",
                ),
                ("src/other.rs", "pub fn greet() {}\n"),
            ],
        ] {
            assert_eq!(resolve(&files, "greet"), None, "{files:?}");
        }
        // 再公開する module 自身が条件付き (`cfg` の宣言か、ファイルの内部属性)
        for (lib, api) in [
            (
                "#[cfg(feature = \"x\")]\npub mod api;\nmod detail;\n",
                "pub use crate::detail::greet;\n",
            ),
            (
                "pub mod api;\nmod detail;\n",
                "#![cfg(feature = \"x\")]\npub use crate::detail::greet;\n",
            ),
            // 再公開する module が非公開
            ("mod api;\nmod detail;\n", "pub use crate::detail::greet;\n"),
        ] {
            assert_eq!(
                resolve_from(
                    &[("src/lib.rs", lib), ("src/api.rs", api), detail],
                    "src/api.rs",
                    "greet"
                ),
                None,
                "{lib}{api}"
            );
        }
    }

    /// 再公開したファイルの module の経路は、rename した新しいパスからも求める (旧いパスと比べる)。
    #[test]
    fn source_module_paths_follow_the_file_layout() {
        let path = |segments: &[&str]| segments.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            source_module_path("src/lib.rs"),
            Some((String::new(), Vec::new()))
        );
        assert_eq!(
            source_module_path("src/api/mod.rs"),
            Some((String::new(), path(&["api"])))
        );
        assert_eq!(
            source_module_path("crates/notes/src/api/inner.rs"),
            Some(("crates/notes".to_owned(), path(&["api", "inner"])))
        );
        assert_ne!(
            source_module_path("src/api.rs"),
            source_module_path("src/renamed.rs")
        );
        assert_eq!(source_module_path("README.md"), None);
    }

    #[test]
    fn declarations_in_scopes_distinguish_absent_and_unknown() {
        let source = b"pub fn greet() {}\nmod inner {\n    pub struct Note;\n}\n";
        assert_eq!(
            declaration_in_scope(source, &[], "greet"),
            Some(Some("function"))
        );
        assert_eq!(declaration_in_scope(source, &[], "Note"), Some(None));
        assert_eq!(
            declaration_in_scope(source, &["inner".to_owned()], "Note"),
            Some(Some("struct"))
        );
        // module ごと無い
        assert_eq!(
            declaration_in_scope(source, &["missing".to_owned()], "Note"),
            Some(None)
        );
        // 名前を持ち込み得る use・マクロ
        assert_eq!(declaration_in_scope(b"use a::greet;\n", &[], "greet"), None);
        assert_eq!(declaration_in_scope(b"make!();\n", &[], "greet"), None);
        // 無い module のスコープにマクロがあるか、外部ファイルの module なら決められない
        assert_eq!(
            declaration_in_scope(b"make_detail!();\n", &["detail".to_owned()], "Note"),
            None
        );
        assert_eq!(
            declaration_in_scope(b"mod detail;\n", &["detail".to_owned()], "Note"),
            None
        );
        // 直下と inline module の中の同じ名前の宣言を数える
        assert_eq!(
            module_scope_declarations(b"pub fn f() {}\npub mod api {\n    pub fn f() {}\n}\n", "f"),
            Some(2)
        );
        // 型の名前空間の束縛を、スコープごとに、属性を含めたトークンの並びで集める (関数と可視性の
        // 無い use は数えない)
        let bindings = type_namespace_bindings(
            b"use crate::other::Note;\npub fn Note() {}\n#[cfg(feature = \"x\")]\npub mod nested {\n    #![allow(dead_code)]\n    pub use crate::other::*;\n    #[cfg(unix)]\n    pub type   Note = u8;\n}\n",
            "Note",
        )
        .expect("bindings");
        assert!(!bindings.item_macros);
        let [(path, entries)] = bindings.scopes.iter().collect::<Vec<_>>()[..] else {
            panic!("{:?}", bindings.scopes);
        };
        assert_eq!(path.len(), 1, "{path:?}");
        let has = |tokens: &Tokens, token: &str| tokens.iter().any(|t| t == token);
        assert!(
            has(&path[0], "identifier:cfg") && has(&path[0], "identifier:dead_code"),
            "{path:?}"
        );
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(
            entries.iter().any(|entry| has(entry, "identifier:unix")),
            "{entries:?}"
        );
        // base に束縛のあった module に同じものが同じ字面で同じ数だけあり、足したものも無ければ
        // 保たれる。束縛の無かった module に足したものは数えない
        let kept = |base: &str, new: &str| {
            type_namespace_bindings_kept(base.as_bytes(), Some(new.as_bytes()), "Note")
        };
        let base = "pub mod nested {\n    pub type Note = u8;\n}\n";
        assert!(kept(
            base,
            "pub struct Note;\npub mod nested {\n    pub type Note = u8;\n}\n"
        ));
        assert!(!kept(
            base,
            "pub struct Note;\npub mod nested {\n    pub type Note = super::Note;\n}\n"
        ));
        assert!(!kept(
            base,
            "pub struct Note;\npub mod other {\n    pub type Note = u8;\n}\n"
        ));
        // 足した明示の束縛は glob の束縛を覆う
        assert!(!kept(
            "pub mod nested {\n    pub use crate::other::*;\n}\n",
            "pub struct Note;\npub mod nested {\n    pub use crate::other::*;\n    pub use super::Note;\n}\n"
        ));
        // トークンの間の空白とコメントの違いは無視し、文字列リテラルの中の空白は区別する
        assert!(kept(
            "pub mod nested {\n    pub type   Note = /* alias */ u8; // old\n}\n",
            "pub struct Note;\npub mod nested {\n    pub type Note=u8;\n}\n"
        ));
        assert!(!kept(
            "pub mod nested {\n    #[cfg(mode = \"a b\")]\n    pub use super::Note;\n    #[cfg(mode = \"a  b\")]\n    pub type Note = u8;\n}\n",
            "pub struct Note;\npub mod nested {\n    #[cfg(mode = \"a  b\")]\n    pub use super::Note;\n    #[cfg(mode = \"a b\")]\n    pub type Note = u8;\n}\n"
        ));
        // 生の文字列は前置きごと 1 つのトークンで、同じ字面の識別子と区別する。マクロの入力の中の
        // doc コメントは `#[doc]` として渡るので数え、普通のコメントは数えない
        let alias = |ty: &str| format!("pub mod nested {{\n    pub type Note = {ty};\n}}\n");
        let moved = |ty: &str| format!("pub struct Note;\n{}", alias(ty));
        for (before, after) in [
            ("m!(r\"x\")", "m!(x)"),
            ("m!(r\"x\")", "m!(br\"x\")"),
            ("m!(r\"x\")", "m!(r#\"x\"#)"),
            ("m!(/** x */)", "m!()"),
            ("m!(/// x\n)", "m!()"),
        ] {
            assert!(!kept(&alias(before), &moved(after)), "{before} -> {after}");
        }
        assert!(kept(&alias("m!(/* x */ y)"), &moved("m!(y)")));
        // cfg を入れ替えると別の束縛
        assert!(!kept(
            "pub mod nested {\n    #[cfg(any())]\n    pub use super::Note;\n    #[cfg(not(any()))]\n    pub struct Note;\n}\n",
            "pub struct Note;\npub mod nested {\n    #[cfg(not(any()))]\n    pub use super::Note;\n    #[cfg(any())]\n    pub struct Note;\n}\n"
        ));
        // base のマクロの呼び出しは、字面が同じでも展開が変わり得る。足したマクロは数えない
        assert!(!kept(
            "pub mod nested {\n    make!();\n}\n",
            "pub struct Note;\npub mod nested {\n    make!();\n}\n"
        ));
        assert!(kept(
            "pub mod nested {}\n",
            "pub struct Note;\npub mod nested {\n    make!();\n}\n"
        ));
    }
}
