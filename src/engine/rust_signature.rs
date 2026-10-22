//! Rust 関数シグネチャの非契約差分を正規化・証明する。
//!
//! 通常引数の binding-side `mut` 除去と、root-level の公開 type alias 導入だけで
//! 実体型が変わらないことの old/new AST 証明を担う。

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, any_node, walk_preorder};
use crate::language::LangId;

mod parameter_patterns;
pub(crate) use parameter_patterns::{
    rust_parameter_import_environment, rust_parameter_pattern_contract,
};

/// old/new それぞれの同一ファイル内 `pub type` を使い、関数シグネチャの型表現が
/// 透過的な alias 導入だけで変わったかを証明するコンテキスト。
///
/// 対象は非 generic・一意・unrestricted `pub` の alias に限定する。import alias、private / restricted
/// alias、generic、重複、循環、parse 不能はすべて「同値と証明できない」側へ倒す。
pub(crate) struct RustTypeAliasEquivalence {
    old: TypeAliasIndex,
    new: TypeAliasIndex,
}

/// working tree 側を先に parse した alias 同値証明候補。
///
/// 新側に利用可能な root `pub type` が無ければ構築せず、API 検出で base blob の
/// `git show` を起動しない。impact では Pass 1 の既存 tree から構築して再 parse を避ける。
pub(crate) struct PreparedRustTypeAliases {
    new: TypeAliasIndex,
}

impl RustTypeAliasEquivalence {
    #[cfg(test)]
    pub(crate) fn new(old_source: &[u8], new_source: &[u8]) -> Option<Self> {
        Some(Self {
            old: TypeAliasIndex::from_source(old_source)?,
            new: TypeAliasIndex::from_source(new_source)?,
        })
    }

    /// working tree ソースを先に parse し、新側に証明対象の alias がある場合だけ候補を返す。
    pub(crate) fn prepare_new_source(new_source: &[u8]) -> Option<PreparedRustTypeAliases> {
        let tree = parser::parse_source(new_source, LangId::Rust).ok()?;
        Self::prepare_new_tree(tree.root_node(), new_source)
    }

    /// 呼び出し側が既に parse した working tree の root を再利用する。
    pub(crate) fn prepare_new_tree(
        new_root: Node<'_>,
        new_source: &[u8],
    ) -> Option<PreparedRustTypeAliases> {
        let new = TypeAliasIndex::from_root(new_root, new_source)?;
        (!new.aliases.is_empty()).then_some(PreparedRustTypeAliases { new })
    }

    /// old/new の完全な root 関数ヘッダを比較する。diff の変更行や抽出済みの
    /// 1 行シグネチャは名前の候補にだけ使い、同値性の証拠にはしない。
    pub(crate) fn prove_function(&self, name: &str) -> bool {
        // import の意味が同じと証明できない状態で、alias RHS 内の同名型を字面だけで同一視しない。
        if self.old.imports != self.new.imports {
            return false;
        }
        if self.old.function_counts.get(name) != Some(&1)
            || self.new.function_counts.get(name) != Some(&1)
        {
            return false;
        }
        let Some(old_header) = self.old.root_function_headers.get(name) else {
            return false;
        };
        let Some(new_header) = self.new.root_function_headers.get(name) else {
            return false;
        };
        let Some(old) = canonicalize_signature(old_header, &self.old) else {
            return false;
        };
        let Some(new) = canonicalize_signature(new_header, &self.new) else {
            return false;
        };
        if old.function_name != name || new.function_name != name {
            return false;
        }
        if !new.expanded_alias || old.tokens != new.tokens {
            return false;
        }
        // 型・配列長などに現れる名前の root 束縛が変化した場合は、字面の
        // 一致だけで実体型が同じとは言えない。新規 alias 自身だけを除外する。
        old.names.union(&new.names).all(|used| {
            if new.expanded_alias_names.contains(used) && !self.old.root_bindings.contains_key(used)
            {
                return true;
            }
            self.old.root_bindings.get(used) == self.new.root_bindings.get(used)
        })
    }

    #[cfg(test)]
    pub(crate) fn signatures_equivalent(&self, old_signature: &str, new_signature: &str) -> bool {
        let Some(old) = canonicalize_signature(old_signature, &self.old) else {
            return false;
        };
        let Some(new) = canonicalize_signature(new_signature, &self.new) else {
            return false;
        };
        old.function_name == new.function_name && self.prove_function(&old.function_name)
    }
}

impl PreparedRustTypeAliases {
    /// 先に準備した new index と、後から取得した old フルソースを組み合わせる。
    pub(crate) fn with_old_source(self, old_source: &[u8]) -> Option<RustTypeAliasEquivalence> {
        Some(RustTypeAliasEquivalence {
            old: TypeAliasIndex::from_source(old_source)?,
            new: self.new,
        })
    }
}

#[derive(Default)]
struct TypeAliasIndex {
    aliases: HashMap<String, String>,
    blocked: HashSet<String>,
    imports: Vec<String>,
    root_function_headers: HashMap<String, String>,
    function_counts: HashMap<String, usize>,
    root_bindings: HashMap<String, Vec<String>>,
    blocked_root_functions: HashSet<String>,
}

impl TypeAliasIndex {
    fn from_source(source: &[u8]) -> Option<Self> {
        let tree = parser::parse_source(source, LangId::Rust).ok()?;
        Self::from_root(tree.root_node(), source)
    }

    fn from_root(root: Node<'_>, source: &[u8]) -> Option<Self> {
        if root.has_error() {
            return None;
        }
        let mut index = Self::default();
        let mut has_pending_attribute = false;
        let mut pending_attributes = String::new();
        let mut ambiguous_root_scope = false;
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() == "attribute_item" {
                has_pending_attribute = true;
                pending_attributes.push_str(child.utf8_text(source).ok()?);
                pending_attributes.push('\n');
                continue;
            }
            if child.kind() == "inner_attribute_item" {
                ambiguous_root_scope = true;
                continue;
            }
            if matches!(child.kind(), "line_comment" | "block_comment") {
                continue;
            }
            if matches!(
                child.kind(),
                "macro_invocation" | "extern_crate_declaration"
            ) {
                ambiguous_root_scope = true;
            }
            if child.kind() == "type_item" {
                index.collect_alias(child, source, has_pending_attribute);
            } else if child.kind() == "function_item" {
                index.collect_root_function(child, source, has_pending_attribute);
            } else {
                // スコープ解決は行わない。root 以外の同名 alias / associated type があれば、
                // root alias を誤適用しないよう名前単位で同値証明を諦める。
                index.block_nested_aliases(child, source);
            }

            if child.kind() == "use_declaration" {
                // 条件付き import の有効性は静的に確定できない。old/new の字面一致だけで
                // 名前解決が同じと証明せず、ファイル全体の alias 証明を無効にする。
                if has_pending_attribute {
                    ambiguous_root_scope = true;
                }
                index.imports.push(canonical_token_stream(child, source)?);
            }
            if child.kind() != "function_item" {
                index.collect_root_binding(child, source, &pending_attributes)?;
            }
            has_pending_attribute = false;
            pending_attributes.clear();
        }
        collect_function_counts(root, source, &mut index.function_counts)?;
        if ambiguous_root_scope {
            index.aliases.clear();
        }
        index.imports.sort();
        Some(index)
    }

    fn collect_alias(&mut self, item: Node<'_>, source: &[u8], attributed: bool) {
        let Some(name_node) = item.child_by_field_name("name") else {
            return;
        };
        let Ok(name) = name_node.utf8_text(source) else {
            return;
        };
        let name = name.to_string();
        let unrestricted_public = direct_named_child(item, "visibility_modifier")
            .and_then(|node| node.utf8_text(source).ok())
            .is_some_and(|visibility| !visibility.contains('('));
        let generic = direct_named_child(item, "type_parameters").is_some();
        let rhs = item
            .child_by_field_name("type")
            .and_then(|node| node.utf8_text(source).ok())
            .map(str::to_string);

        if attributed || !unrestricted_public || generic || self.aliases.contains_key(&name) {
            self.aliases.remove(&name);
            self.blocked.insert(name);
            return;
        }
        let Some(rhs) = rhs else {
            self.aliases.remove(&name);
            self.blocked.insert(name);
            return;
        };
        if !self.blocked.contains(&name) {
            self.aliases.insert(name, rhs);
        }
    }

    fn collect_root_function(&mut self, item: Node<'_>, source: &[u8], attributed: bool) {
        let Some(name) = item
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
        else {
            return;
        };
        let header = item
            .child_by_field_name("body")
            .and_then(|body| source.get(item.start_byte()..body.start_byte()))
            .and_then(|bytes| std::str::from_utf8(bytes).ok());
        let Some(header) = header else {
            self.root_function_headers.remove(name);
            self.blocked_root_functions.insert(name.to_string());
            return;
        };
        if attributed || self.blocked_root_functions.contains(name) {
            self.root_function_headers.remove(name);
            self.blocked_root_functions.insert(name.to_string());
            return;
        }
        if self
            .root_function_headers
            .insert(name.to_string(), header.to_string())
            .is_some()
        {
            self.root_function_headers.remove(name);
            self.blocked_root_functions.insert(name.to_string());
        }
    }

    fn collect_root_binding(
        &mut self,
        item: Node<'_>,
        source: &[u8],
        pending_attributes: &str,
    ) -> Option<()> {
        let Some(name) = item.child_by_field_name("name") else {
            return Some(());
        };
        let name = name.utf8_text(source).ok()?.to_string();
        let declaration = format!("{pending_attributes}{}", item.utf8_text(source).ok()?);
        self.root_bindings
            .entry(name)
            .or_default()
            .push(declaration);
        Some(())
    }

    fn block_nested_aliases(&mut self, node: Node<'_>, source: &[u8]) {
        let mut pending = vec![node];
        while let Some(node) = pending.pop() {
            if matches!(node.kind(), "type_item" | "associated_type") {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                {
                    self.aliases.remove(name);
                    self.blocked.insert(name.to_string());
                }
                continue;
            }
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            pending.extend(children.into_iter().rev());
        }
    }
}

fn collect_function_counts(
    root: Node<'_>,
    source: &[u8],
    counts: &mut HashMap<String, usize>,
) -> Option<()> {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "function_item" | "function_signature_item")
            && let Some(name) = node.child_by_field_name("name")
        {
            *counts
                .entry(name.utf8_text(source).ok()?.to_string())
                .or_default() += 1;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    Some(())
}

struct CanonicalSignature {
    tokens: String,
    expanded_alias: bool,
    expanded_alias_names: HashSet<String>,
    function_name: String,
    names: HashSet<String>,
}

fn canonicalize_signature(signature: &str, aliases: &TypeAliasIndex) -> Option<CanonicalSignature> {
    let (source, original_len) = parseable_signature_source(signature);
    let tree = parser::parse_source(&source, LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let function = find_function_node(tree.root_node())?;
    let function_name = function
        .child_by_field_name("name")?
        .utf8_text(&source)
        .ok()?
        .to_string();
    let mut shadowed = HashSet::new();
    if let Some(type_parameters) = direct_named_child(function, "type_parameters") {
        collect_type_identifiers(type_parameters, &source, &mut shadowed)?;
    }

    let signature_end = function
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or(original_len)
        .min(original_len);
    let mut canonical = CanonicalSignature {
        tokens: String::new(),
        expanded_alias: false,
        expanded_alias_names: HashSet::new(),
        function_name,
        names: HashSet::new(),
    };
    let mut stack = Vec::new();
    canonicalize_node(
        function,
        &source,
        signature_end,
        aliases,
        &shadowed,
        &mut stack,
        &mut canonical,
    )?;
    Some(canonical)
}

/// `node` の部分木を、子を順に辿った前順のトークン列へ正規化する。
///
/// 型の入れ子 (`Option<Option<…>>`) の深さだけ再帰しないよう、1 つの TreeCursor の
/// 前順走査で辿る。再帰するのは別名の展開 (`canonicalize_alias`、上限 32 段) だけ。
/// 親は `Node::parent` (根から辿り直すので深さに比例する) で引かず、前順の深さに
/// 合わせて保つ祖先の列から引く (深い入れ子で 2 乗の時間にしない)。
fn canonicalize_node(
    node: Node<'_>,
    source: &[u8],
    end: usize,
    aliases: &TypeAliasIndex,
    shadowed: &HashSet<String>,
    stack: &mut Vec<String>,
    canonical: &mut CanonicalSignature,
) -> Option<()> {
    let root_parent = node.parent();
    let mut ancestors = Vec::new();
    let flow = walk_preorder(node, Children::All, |node, depth| {
        if node.start_byte() >= end {
            return ControlFlow::Continue(Walk::Skip);
        }
        ancestors.truncate(depth);
        let parent = if depth == 0 {
            root_parent
        } else {
            ancestors.last().copied()
        };
        ancestors.push(node);
        canonicalize_visit(node, parent, source, aliases, shadowed, stack, canonical)
            .map_or(ControlFlow::Break(()), ControlFlow::Continue)
    });
    flow.is_continue().then_some(())
}

/// `canonicalize_node` の 1 ノード分。`None` は正規化の失敗 (同値と証明しない)。
fn canonicalize_visit(
    node: Node<'_>,
    parent: Option<Node<'_>>,
    source: &[u8],
    aliases: &TypeAliasIndex,
    shadowed: &HashSet<String>,
    stack: &mut Vec<String>,
    canonical: &mut CanonicalSignature,
) -> Option<Walk> {
    if is_rust_list_trailing_comma(node) {
        return Some(Walk::Skip);
    }
    // 引数の束縛に付く `mut` は呼び出し契約ではないので比べない (`is_rust_binding_mut`)。
    if node.kind() == "mutable_specifier" && parent.is_some_and(is_binding_mut_parent) {
        return Some(Walk::Skip);
    }
    // 配列長と const generic 引数は値の評価が要る。関数・関連定数・式が
    // 同じ字面でも別の値になり得るため、リテラル以外を同値の証拠にしない。
    if node.kind() == "array_type"
        && node
            .child_by_field_name("length")
            .is_some_and(|length| length.kind() != "integer_literal")
    {
        return None;
    }
    if node.kind() == "type_arguments" {
        let mut cursor = node.walk();
        if node.named_children(&mut cursor).any(|child| {
            matches!(child.kind(), "identifier" | "scoped_identifier" | "block")
                || child.kind().ends_with("_expression")
        }) {
            return None;
        }
    }
    if matches!(node.kind(), "identifier" | "type_identifier") {
        canonical
            .names
            .insert(node.utf8_text(source).ok()?.to_string());
    }
    if node.kind() == "type_identifier" {
        let name = node.utf8_text(source).ok()?;
        if aliases.blocked.contains(name) {
            return None;
        }
        let qualified = parent.is_some_and(|parent| {
            matches!(
                parent.kind(),
                "scoped_type_identifier" | "scoped_identifier"
            )
        });
        if aliases.aliases.contains_key(name) && !shadowed.contains(name) && !qualified {
            if !shadowed.is_empty() {
                return None;
            }
            canonical.expanded_alias = true;
            canonical.expanded_alias_names.insert(name.to_string());
            canonicalize_alias(name, aliases, shadowed, stack, canonical)?;
            return Some(Walk::Skip);
        }
    }

    if node.child_count() == 0 {
        if !matches!(node.kind(), "line_comment" | "block_comment") {
            push_token(canonical, node.kind(), node.utf8_text(source).ok()?);
        }
        return Some(Walk::Skip);
    }
    Some(Walk::Descend)
}

fn canonicalize_alias(
    name: &str,
    aliases: &TypeAliasIndex,
    shadowed: &HashSet<String>,
    stack: &mut Vec<String>,
    canonical: &mut CanonicalSignature,
) -> Option<()> {
    if stack.len() >= 32 || stack.iter().any(|active| active == name) {
        return None;
    }
    let rhs = aliases.aliases.get(name)?;
    stack.push(name.to_string());
    let wrapped = format!("type __AstroSightAlias = {rhs};");
    let tree = parser::parse_source(wrapped.as_bytes(), LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let type_item = direct_named_child(tree.root_node(), "type_item")?;
    let rhs_node = type_item.child_by_field_name("type")?;
    if contains_unsupported_alias_type(rhs_node) {
        return None;
    }
    canonicalize_node(
        rhs_node,
        wrapped.as_bytes(),
        wrapped.len(),
        aliases,
        shadowed,
        stack,
        canonical,
    )?;
    stack.pop();
    Some(())
}

fn contains_unsupported_alias_type(node: Node<'_>) -> bool {
    any_node(node, Children::Named, |node| {
        matches!(node.kind(), "dynamic_type" | "macro_invocation")
    })
}

fn canonical_token_stream(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut canonical = CanonicalSignature {
        tokens: String::new(),
        expanded_alias: false,
        expanded_alias_names: HashSet::new(),
        function_name: String::new(),
        names: HashSet::new(),
    };
    canonicalize_node(
        node,
        source,
        node.end_byte(),
        &TypeAliasIndex::default(),
        &HashSet::new(),
        &mut Vec::new(),
        &mut canonical,
    )?;
    Some(canonical.tokens)
}

fn collect_type_identifiers(
    node: Node<'_>,
    source: &[u8],
    names: &mut HashSet<String>,
) -> Option<()> {
    let flow = walk_preorder(node, Children::Named, |node, _| {
        if node.kind() == "type_identifier" {
            let Ok(name) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            names.insert(name.to_string());
        }
        ControlFlow::Continue(Walk::Descend)
    });
    flow.is_continue().then_some(())
}

fn push_token(canonical: &mut CanonicalSignature, kind: &str, text: &str) {
    let _ = write!(
        canonical.tokens,
        "{}:{kind}:{}:{text};",
        kind.len(),
        text.len()
    );
}

fn parseable_signature_source(signature: &str) -> (Vec<u8>, usize) {
    let original_len = signature.len();
    let trimmed = signature.trim_end();
    let mut parseable = signature.as_bytes().to_vec();
    if trimmed.ends_with('{') {
        parseable.extend_from_slice(b" }");
    } else if !trimmed.ends_with(';') && !trimmed.ends_with('}') {
        // ヘッダの末尾が行コメントでも、補う本体をコメントへ取り込まない。
        parseable.extend_from_slice(b"\n{}");
    }
    (parseable, original_len)
}

/// Rust の引数の束縛に付く `mut` か。
///
/// `mut x: T`・`mut self`・パターンの中の `(mut a, b)`・`S { mut v }`・`ref mut z` の `mut` は
/// 本体側の束縛の指定で、関数の型にも呼び出し方にも影響しない。`&mut self`・`&mut T`・
/// `*mut T`・参照パターン `&mut x` の `mut` は型の側なので含めない。
pub(crate) fn is_rust_binding_mut(node: Node<'_>) -> bool {
    node.kind() == "mutable_specifier" && node.parent().is_some_and(is_binding_mut_parent)
}

/// 引数・型パラメータ・型引数のリストに直接属する省略可能な末尾カンマ。
/// tuple / macro の token tree / 呼び出し引数は、そのカンマが値や型を変え得るので含めない。
pub(crate) fn is_rust_list_trailing_comma(node: Node<'_>) -> bool {
    if node.kind() != "," {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    let closing = match parent.kind() {
        "parameters" => ")",
        "type_parameters" | "type_arguments" => ">",
        _ => return false,
    };
    let mut sibling = node.next_sibling();
    while let Some(comment) =
        sibling.filter(|next| matches!(next.kind(), "line_comment" | "block_comment"))
    {
        sibling = comment.next_sibling();
    }
    sibling.is_some_and(|next| next.kind() == closing && next.next_sibling().is_none())
}

/// 表示用 signature を書き換えず、Rust のヘッダのトークンが同じかを比べる。
pub(crate) fn rust_signature_formatting_equivalent(old: &str, new: &str) -> bool {
    rust_signature_token_key(old)
        .zip(rust_signature_token_key(new))
        .is_some_and(|(old, new)| old == new)
}

fn rust_signature_token_key(signature: &str) -> Option<Vec<(String, String)>> {
    let (source, original_len) = parseable_signature_source(signature);
    let tree = parser::parse_source(&source, LangId::Rust).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let function = find_function_node(root)?;
    let mut children = root.walk();
    if root
        .named_children(&mut children)
        .any(|node| !node.is_extra() && node.id() != function.id())
    {
        return None;
    }
    let end = function
        .child_by_field_name("body")
        .map_or(original_len, |body| body.start_byte());
    // 行ベースの入力が本体まで含む場合は、既存の厳格な本体変更証明に任せる。
    if end < original_len && signature.get(end..)?.trim() != "{" {
        return None;
    }
    let mut tokens = Vec::new();
    let flow = walk_preorder(function, Children::All, |node, _| {
        if node.start_byte() >= end {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.is_error() || node.is_missing() {
            return ControlFlow::Break(());
        }
        if is_rust_binding_mut(node) || is_rust_list_trailing_comma(node) {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.child_count() == 0
            || matches!(
                node.kind(),
                "string_literal" | "raw_string_literal" | "char_literal" | "token_tree"
            )
        {
            let Ok(text) = node.utf8_text(&source) else {
                return ControlFlow::Break(());
            };
            tokens.push((node.kind().to_owned(), text.to_owned()));
            return ControlFlow::Continue(Walk::Skip);
        }
        ControlFlow::Continue(Walk::Descend)
    });
    flow.is_continue().then_some(tokens)
}

/// `mutable_specifier` の親から、その `mut` が束縛の指定かを決める。
fn is_binding_mut_parent(parent: Node<'_>) -> bool {
    match parent.kind() {
        "parameter" | "mut_pattern" | "field_pattern" => true,
        // `&mut self` / `&'a mut self` は `&` の後ろの `mut` で、参照の型の一部。
        "self_parameter" => {
            let mut cursor = parent.walk();
            !parent
                .children(&mut cursor)
                .any(|child| child.kind() == "&")
        }
        _ => false,
    }
}

/// Rust の引数の束縛に付く `mut` (`is_rust_binding_mut`) を除いたシグネチャを返す。
///
/// `fn run(mut callback: F)` の `mut` は関数本体で callback 変数を再束縛・可変借用するための
/// 指定で、呼び出し側の型契約ではない。一方、`callback: &mut F` の `mut` は型の一部なので
/// 除外しない。文字列置換では両者を区別できないため、AST 上の位置で束縛側の `mut` だけを除く。
/// 本体の無い宣言 (トレイトの必須メソッド、extern の関数) も同じ規則で扱う。
pub(crate) fn normalize_rust_parameter_binding_signature(
    function: Node<'_>,
    source: &[u8],
    start: usize,
    end: usize,
) -> Option<String> {
    if function.has_error() {
        return None;
    }
    let parameters = function
        .child_by_field_name("parameters")
        .or_else(|| direct_named_child(function, "parameters"))?;
    let mut omitted = Vec::new();
    let _: ControlFlow<()> = walk_preorder(parameters, Children::Named, |node, _| {
        if is_rust_binding_mut(node) {
            omitted.push((node.start_byte(), node.end_byte()));
        }
        ControlFlow::Continue(Walk::Descend)
    });
    let mut preserve_whitespace = false;
    let flow = walk_preorder(function, Children::All, |node, _| {
        if node.start_byte() >= end {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.is_missing() || node.is_error() {
            return ControlFlow::Break(());
        }
        if matches!(
            node.kind(),
            "string_literal"
                | "raw_string_literal"
                | "char_literal"
                | "token_tree"
                | "line_comment"
                | "block_comment"
        ) {
            preserve_whitespace = true;
            return ControlFlow::Continue(Walk::Skip);
        }
        ControlFlow::Continue(Walk::Descend)
    });
    if flow.is_break() {
        return None;
    }

    // リテラル・macro token tree の内部と行コメントの終端は空白にも意味がある。
    // これらがあるヘッダは表示の空白を保ち、比較用 AST トークン列に整形差を任せる。
    rebuild_without_ranges(source, start, end, &omitted, preserve_whitespace)
}

/// unified diff の 1 行シグネチャにも同じ AST 正規化を適用する。
/// parse や function node の特定に失敗した場合は `None` を返し、呼び出し側が元文字列で
/// 比較することで fail-closed に倒す。
pub(crate) fn normalize_rust_signature_text(signature: &str) -> Option<String> {
    let (parseable, original_len) = parseable_signature_source(signature);

    let tree = parser::parse_source(&parseable, LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let function = find_function_node(tree.root_node())?;
    normalize_rust_parameter_binding_signature(
        function,
        &parseable,
        function.start_byte(),
        original_len,
    )
}

fn direct_named_child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn find_function_node(node: Node<'_>) -> Option<Node<'_>> {
    match walk_preorder(node, Children::Named, |node, _| {
        if matches!(node.kind(), "function_item" | "function_signature_item") {
            ControlFlow::Break(node)
        } else {
            ControlFlow::Continue(Walk::Descend)
        }
    }) {
        ControlFlow::Break(found) => Some(found),
        ControlFlow::Continue(()) => None,
    }
}

fn rebuild_without_ranges(
    source: &[u8],
    start: usize,
    end: usize,
    omitted: &[(usize, usize)],
    preserve_whitespace: bool,
) -> Option<String> {
    let _ = source.get(start..end)?;
    if omitted.iter().any(|&(range_start, range_end)| {
        range_start < start || range_end > end || range_start >= range_end
    }) {
        return None;
    }
    let mut ranges = omitted.to_vec();
    ranges.sort_unstable();
    ranges.dedup();

    let mut rebuilt = Vec::with_capacity(end.saturating_sub(start));
    let mut cursor = start;
    for (range_start, mut range_end) in ranges {
        if range_start < cursor {
            return None;
        }
        rebuilt.extend_from_slice(source.get(cursor..range_start)?);
        while range_end < end && source[range_end].is_ascii_whitespace() {
            range_end += 1;
        }
        cursor = range_end;
    }
    rebuilt.extend_from_slice(source.get(cursor..end)?);
    if preserve_whitespace {
        String::from_utf8(rebuilt).ok()
    } else {
        Some(normalize_whitespace(&rebuilt))
    }
}

fn normalize_whitespace(bytes: &[u8]) -> String {
    super::normalize_whitespace(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_signature_formatting_keys_keep_contract_tokens() {
        for (old, new) in [
            ("pub fn f(x: u64) -> u64", "pub fn f(\n x: u64,\n) -> u64"),
            (
                "pub fn f<'a, T>(x: &'a Vec<Result<T, u8>>) -> Vec<T>",
                "pub fn f<'a, T,>(\n x: &'a Vec<Result<T, u8,>,>,\n) -> Vec<T,>",
            ),
            (
                "pub fn f<T /*same*/>(x: T /*same*/) -> T",
                "pub fn f<T, /*same*/>(x: T, /*same*/) -> T",
            ),
            (
                "pub fn f<T //same\n>(x: T //same\n) -> T",
                "pub fn f<T, //same\n>(x: T, //same\n) -> T",
            ),
        ] {
            assert!(
                rust_signature_formatting_equivalent(old, new),
                "{old} -> {new}"
            );
            assert!(
                rust_signature_formatting_equivalent(new, old),
                "{new} -> {old}"
            );
        }
        for (old, new) in [
            ("pub fn f(x: (String))", "pub fn f(x: (String,))"),
            ("pub fn f(x: u64)", "pub fn f(x: u64, y: u64)"),
            ("pub fn f(x: u64, y: String)", "pub fn f(x: String, y: u64)"),
            ("pub fn f(x: u64) -> u64", "pub fn f(x: u32) -> u64"),
            ("pub fn f(x: u64) -> u64", "pub fn f(x: u64) -> u32"),
            (
                "pub fn f(x: tagged!(\"a b\"))",
                "pub fn f(x: tagged!(\"a  b\"))",
            ),
            ("pub fn f(x: tagged!(u8,))", "pub fn f(x: tagged!(u8))"),
            (
                "pub fn f(x: Const<{ call(1,) }>)",
                "pub fn f(x: Const<{ call(1) }>)",
            ),
            ("pub fn f(x: )", "pub fn f(x: )"),
            ("pub fn f<T(x: T)", "pub fn f<T(x: T)"),
        ] {
            assert!(
                !rust_signature_formatting_equivalent(old, new),
                "{old} -> {new}"
            );
        }
        let literal = "pub fn f(x: tagged!(\"a  b\"))";
        assert!(
            normalize_rust_signature_text(literal)
                .unwrap()
                .contains("a  b")
        );
    }

    #[test]
    fn rust_signature_trailing_comma_node_contract() {
        let source = b"pub fn f<T,>(x: Vec<T,>, y: (u8,),) { g(1,); m!(u8,); }";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        assert!(!tree.root_node().has_error());
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        let _: ControlFlow<()> = walk_preorder(tree.root_node(), Children::All, |node, _| {
            if node.kind() == "," {
                let parent = node.parent().unwrap().kind();
                if is_rust_list_trailing_comma(node) {
                    accepted.push(parent);
                } else {
                    rejected.push(parent);
                }
            }
            ControlFlow::Continue(Walk::Descend)
        });
        assert_eq!(
            accepted,
            ["type_parameters", "type_arguments", "parameters"]
        );
        assert!(rejected.contains(&"tuple_type"));
        assert!(rejected.contains(&"arguments"));
        assert!(rejected.contains(&"token_tree"));
        let grammar = LangId::Rust.ts_language();
        for kind in [
            "parameters",
            "type_parameters",
            "type_arguments",
            "tuple_type",
            "arguments",
            "token_tree",
            "line_comment",
            "block_comment",
            "string_literal",
            "raw_string_literal",
            "char_literal",
        ] {
            assert_ne!(
                grammar.id_for_node_kind(kind, true),
                0,
                "missing grammar kind: {kind}"
            );
        }
    }

    #[test]
    fn rust_alias_proof_uses_the_same_trailing_comma_rule() {
        let old = b"pub fn f(x: Vec<(u64, String)>) -> Vec<(u64, String)> { x }";
        let new =
            b"pub type Entry = (u64, String);\npub fn f(x: Vec<Entry,>,) -> Vec<Entry,> { x }";
        assert!(
            RustTypeAliasEquivalence::new(old, new)
                .unwrap()
                .prove_function("f")
        );
        let new = b"pub type Entry = (u64, String,);\npub fn f(x: Vec<(Entry,)>,) -> Vec<Entry,> { vec![] }";
        assert!(
            !RustTypeAliasEquivalence::new(old, new)
                .unwrap()
                .prove_function("f")
        );
    }

    #[test]
    fn deep_nested_body_does_not_overflow_alias_index() {
        let source = format!(
            "pub type Alias = u64;\nmod inner {{ type Alias = u32; fn deep() {{ let _ = {}1{}; }} }}\npub fn outer(x: Alias) -> Alias {{ x }}\n",
            "(".repeat(12_000),
            ")".repeat(12_000)
        );
        let index = TypeAliasIndex::from_source(source.as_bytes()).unwrap();
        assert_eq!(index.function_counts.get("deep"), Some(&1));
        assert!(index.blocked.contains("Alias"));
        assert!(index.aliases.is_empty());
    }

    #[test]
    fn removes_parameter_binding_mut_but_keeps_type_mutability() {
        let with_binding_mut =
            normalize_rust_signature_text("pub fn run<F>(mut callback: F, value: &mut u32) {")
                .expect("Rust signature should parse");
        let without_binding_mut =
            normalize_rust_signature_text("pub fn run<F>(callback: F, value: &mut u32) {")
                .expect("Rust signature should parse");
        assert_eq!(with_binding_mut, without_binding_mut);

        let immutable_type =
            normalize_rust_signature_text("pub fn run<F>(callback: F, value: &u32) {")
                .expect("Rust signature should parse");
        assert_ne!(without_binding_mut, immutable_type);
    }

    /// 束縛の `mut` は位置を問わず除く (`mut self`、パターンの中、本体の無い宣言)。
    /// 型の `mut` (`&mut self`・`&mut T`・`*mut T`・参照パターン `&mut x`) は残す。
    #[test]
    fn binding_mut_is_removed_everywhere_but_type_mut_is_kept() {
        let normalize = |signature: &str| {
            normalize_rust_signature_text(signature)
                .unwrap_or_else(|| panic!("Rust signature should parse: {signature}"))
        };
        for (with_mut, without_mut) in [
            (
                "pub fn take(mut self) -> u32 {",
                "pub fn take(self) -> u32 {",
            ),
            (
                "pub fn take(mut self: Box<Self>) -> u32 {",
                "pub fn take(self: Box<Self>) -> u32 {",
            ),
            (
                "pub fn f((mut a, b): (u32, u32), S { mut v, ref mut z, .. }: S, ref mut q: u32) {",
                "pub fn f((a, b): (u32, u32), S { v, ref z, .. }: S, ref q: u32) {",
            ),
            ("fn g(mut self) -> u32;", "fn g(self) -> u32;"),
            (
                "pub fn ext(mut x: u32) -> u32;",
                "pub fn ext(x: u32) -> u32;",
            ),
        ] {
            assert_eq!(normalize(with_mut), normalize(without_mut), "{with_mut}");
        }
        for (shared, mutable) in [
            (
                "pub fn peek(&self) -> u32 {",
                "pub fn peek(&mut self) -> u32 {",
            ),
            (
                "pub fn peek<'a>(&'a self) -> u32 {",
                "pub fn peek<'a>(&'a mut self) -> u32 {",
            ),
            (
                "pub fn peek(self: &Self) -> u32 {",
                "pub fn peek(self: &mut Self) -> u32 {",
            ),
            (
                "pub fn add(x: &u32) -> u32 {",
                "pub fn add(x: &mut u32) -> u32 {",
            ),
            (
                "pub fn raw(p: *const u8) -> bool {",
                "pub fn raw(p: *mut u8) -> bool {",
            ),
            (
                "pub fn get(&t: &u32) -> u32 {",
                "pub fn get(&mut t: &mut u32) -> u32 {",
            ),
            ("fn h(&self);", "fn h(&mut self);"),
        ] {
            assert_ne!(normalize(shared), normalize(mutable), "{mutable}");
        }
    }

    /// 型別名の同値証明も、束縛の `mut` だけの差を同じ型とみなす。型の `mut` は違う型。
    #[test]
    fn alias_proof_ignores_binding_mut_but_not_type_mut() {
        let old = b"pub fn run(x: u64, y: &u64) -> u64 { x + *y }";
        let new = b"pub type N = u64;\npub fn run(mut x: N, y: &N) -> N { x += 0; x + *y }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid syntax trees");
        assert!(context.prove_function("run"));

        let new = b"pub type N = u64;\npub fn run(x: N, y: &mut N) -> N { x + *y }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid syntax trees");
        assert!(!context.prove_function("run"));
    }

    #[test]
    fn malformed_signature_fails_closed() {
        assert!(normalize_rust_signature_text("not a Rust function").is_none());
    }

    #[test]
    fn transparent_public_alias_introduction_is_equivalent() {
        let old = b"pub fn collect() -> Vec<(u64, (u8, u8), String)> { vec![] }";
        let new =
            b"pub type Entry = (u64, (u8, u8), String);\npub fn collect() -> Vec<Entry> { vec![] }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(context.signatures_equivalent(
            "pub fn collect() -> Vec<(u64, (u8, u8), String)>",
            "pub fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn new_source_without_candidate_alias_skips_preparation() {
        assert!(
            RustTypeAliasEquivalence::prepare_new_source(
                b"pub fn collect() -> Vec<(u64, String)> { vec![] }"
            )
            .is_none()
        );
    }

    #[test]
    fn changed_generic_imported_private_and_cyclic_aliases_fail_closed() {
        for (label, new_source, new_signature) in [
            (
                "changed rhs",
                "pub type Entry = (u32, String);\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub fn collect() -> Vec<Entry>",
            ),
            (
                "generic",
                "pub type Entry<T> = (T, String);\npub fn collect() -> Vec<Entry<u64>> { vec![] }",
                "pub fn collect() -> Vec<Entry<u64>>",
            ),
            (
                "imported",
                "use external::Entry;\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub fn collect() -> Vec<Entry>",
            ),
            (
                "private",
                "type Entry = (u64, String);\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub fn collect() -> Vec<Entry>",
            ),
            (
                "cycle",
                "pub type Entry = Other;\npub type Other = Entry;\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub fn collect() -> Vec<Entry>",
            ),
        ] {
            let old = b"pub fn collect() -> Vec<(u64, String)> { vec![] }";
            let context = RustTypeAliasEquivalence::new(old, new_source.as_bytes())
                .expect("sources should remain parseable");
            assert!(
                !context
                    .signatures_equivalent("pub fn collect() -> Vec<(u64, String)>", new_signature),
                "{label} は同値と証明せず blocking を維持する"
            );
        }
    }

    #[test]
    fn changed_import_binding_prevents_alias_equivalence() {
        let old = b"use old_crate::External;\npub fn collect() -> Vec<External> { vec![] }";
        let new = b"use new_crate::External;\npub type Entry = External;\npub fn collect() -> Vec<Entry> { vec![] }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(!context.signatures_equivalent(
            "pub fn collect() -> Vec<External>",
            "pub fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn nested_same_name_alias_blocks_root_alias_expansion() {
        let old = b"pub type Entry = (u64, String);\npub mod inner { pub fn collect() -> Vec<(u64, String)> { vec![] } }";
        let new = b"pub type Entry = (u64, String);\npub mod inner { pub type Entry = (u32, String); pub fn collect() -> Vec<Entry> { vec![] } }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(!context.signatures_equivalent(
            "pub fn collect() -> Vec<(u64, String)>",
            "pub fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn associated_type_with_same_name_blocks_root_alias_expansion() {
        let old = b"pub type Entry = (u64, String);\npub trait Source { type Entry; fn collect() -> Vec<(u64, String)>; }";
        let new = b"pub type Entry = (u64, String);\npub trait Source { type Entry; fn collect() -> Vec<Entry>; }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(!context.signatures_equivalent(
            "fn collect() -> Vec<(u64, String)>",
            "fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn nested_import_scope_cannot_reuse_root_alias_proof() {
        let old = b"pub struct ExtA; pub struct ExtB; use crate::ExtA as Ext; pub type Entry = (Ext, String); pub mod inner { use crate::ExtB as Ext; pub fn collect() -> Vec<(Ext, String)> { vec![] } }";
        let new = b"pub struct ExtA; pub struct ExtB; use crate::ExtA as Ext; pub type Entry = (Ext, String); pub mod inner { use crate::ExtB as Ext; use super::Entry; pub fn collect() -> Vec<Entry> { vec![] } }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(!context.signatures_equivalent(
            "pub fn collect() -> Vec<(Ext, String)>",
            "pub fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn attributed_alias_and_function_fail_closed() {
        let old = b"#[cfg(feature = \"x\")] pub fn collect() -> Vec<(u64, String)> { vec![] }";
        let new = b"#[cfg(feature = \"x\")] pub type Entry = (u64, String); #[cfg(feature = \"x\")] pub fn collect() -> Vec<Entry> { vec![] }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(!context.signatures_equivalent(
            "pub fn collect() -> Vec<(u64, String)>",
            "pub fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn complete_headers_and_name_bindings_are_required_for_alias_proof() {
        for (label, old, new) in [
            (
                "multiline return changed",
                "pub fn collect(x: (u64, String),\n) -> u32 { 1 }",
                "pub type Entry = (u64, String);\npub fn collect(x: Entry,\n) -> u64 { 1 }",
            ),
            (
                "same name in nested module",
                "pub fn collect() -> (u64, String) { unreachable!() }\nmod inner { fn collect() {} }",
                "pub type Entry = (u64, String);\npub fn collect() -> Entry { unreachable!() }\nmod inner { fn collect() {} }",
            ),
            (
                "prelude name rebound",
                "pub fn collect() -> (u64, String) { unreachable!() }",
                "pub struct String;\npub type Entry = (u64, String);\npub fn collect() -> Entry { unreachable!() }",
            ),
            (
                "root binding attribute changed",
                "pub struct String;\npub fn collect() -> (u64, String) { unreachable!() }",
                "#[cfg(feature = \"extra\")]\npub struct String;\npub type Entry = (u64, String);\npub fn collect() -> Entry { unreachable!() }",
            ),
            (
                "const in array length changed",
                "const N: usize = 4;\npub fn collect() -> [u8; N] { [0; N] }",
                "const N: usize = 8;\npub type Entry = [u8; N];\npub fn collect() -> Entry { [0; N] }",
            ),
            (
                "const function in array length changed",
                "pub const fn size() -> usize { 4 }\npub fn collect() -> [u8; size()] { [0; size()] }",
                "pub const fn size() -> usize { 8 }\npub type Entry = [u8; size()];\npub fn collect() -> Entry { [0; size()] }",
            ),
            (
                "associated const in array length changed",
                "pub struct Foo;\nimpl Foo { pub const N: usize = 4; }\npub fn collect() -> [u8; Foo::N] { [0; Foo::N] }",
                "pub struct Foo;\nimpl Foo { pub const N: usize = 8; }\npub type Entry = [u8; Foo::N];\npub fn collect() -> Entry { [0; Foo::N] }",
            ),
            (
                "const generic expression changed",
                "pub struct Bag<const N: usize>;\npub const fn size() -> usize { 4 }\npub fn collect() -> Bag<{ size() }> { unreachable!() }",
                "pub struct Bag<const N: usize>;\npub const fn size() -> usize { 8 }\npub type Entry = Bag<{ size() }>;\npub fn collect() -> Entry { unreachable!() }",
            ),
            (
                "function generic shadows root type",
                "pub struct T;\npub fn collect<T>() -> (T, u8) { unreachable!() }",
                "pub struct T;\npub type Entry = (T, u8);\npub fn collect<T>() -> Entry { unreachable!() }",
            ),
            (
                "trait object lifetime changes",
                "pub trait Tr {}\npub fn collect(x: &dyn Tr) {}",
                "pub trait Tr {}\npub type Obj = dyn Tr;\npub fn collect(x: &Obj) {}",
            ),
            (
                "attribute followed by comment",
                "pub fn collect() -> (u64, String) { unreachable!() }",
                "#[cfg(feature = \"extra\")]\n/// Documentation\npub type Entry = (u64, String);\npub fn collect() -> Entry { unreachable!() }",
            ),
        ] {
            let context = RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes())
                .expect("valid syntax trees");
            assert!(!context.prove_function("collect"), "{label}");
        }
    }

    #[test]
    fn unrelated_root_addition_does_not_block_alias_proof() {
        let old = b"pub fn collect() -> Vec<(u64, String)> { vec![] }";
        let new = b"pub struct Unrelated;\npub type Entry = (u64, String);\npub fn collect() -> Vec<Entry> { vec![] }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid syntax trees");
        assert!(context.prove_function("collect"));
    }

    #[test]
    fn slices_and_literal_length_arrays_allow_alias_proof() {
        for (old, new) in [
            (
                "pub fn collect(input: &[u8]) -> Vec<(u64, String)> { vec![] }",
                "pub type Entry = (u64, String);\npub fn collect(input: &[u8]) -> Vec<Entry> { vec![] }",
            ),
            (
                "pub fn collect() -> Vec<[u8; 4]> { vec![] }",
                "pub type Arr = [u8; 4];\npub fn collect() -> Vec<Arr> { vec![] }",
            ),
        ] {
            let context = RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes())
                .expect("valid syntax trees");
            assert!(context.prove_function("collect"), "{old} -> {new}");
        }
    }

    /// 修正前の再帰版 (浅い入力で反復版と比べるためだけに残す)。
    fn canonicalize_node_recursive(
        node: Node<'_>,
        source: &[u8],
        end: usize,
        aliases: &TypeAliasIndex,
        shadowed: &HashSet<String>,
        stack: &mut Vec<String>,
        canonical: &mut CanonicalSignature,
    ) -> Option<()> {
        if node.start_byte() >= end {
            return Some(());
        }
        if node.kind() == "array_type"
            && node
                .child_by_field_name("length")
                .is_some_and(|length| length.kind() != "integer_literal")
        {
            return None;
        }
        if node.kind() == "type_arguments" {
            let mut cursor = node.walk();
            if node.named_children(&mut cursor).any(|child| {
                matches!(child.kind(), "identifier" | "scoped_identifier" | "block")
                    || child.kind().ends_with("_expression")
            }) {
                return None;
            }
        }
        if matches!(node.kind(), "identifier" | "type_identifier") {
            canonical
                .names
                .insert(node.utf8_text(source).ok()?.to_string());
        }
        if node.kind() == "type_identifier" {
            let name = node.utf8_text(source).ok()?;
            if aliases.blocked.contains(name) {
                return None;
            }
            let qualified = node.parent().is_some_and(|parent| {
                matches!(
                    parent.kind(),
                    "scoped_type_identifier" | "scoped_identifier"
                )
            });
            if aliases.aliases.contains_key(name) && !shadowed.contains(name) && !qualified {
                if !shadowed.is_empty() {
                    return None;
                }
                canonical.expanded_alias = true;
                canonical.expanded_alias_names.insert(name.to_string());
                if stack.len() >= 32 || stack.iter().any(|active| active == name) {
                    return None;
                }
                let rhs = aliases.aliases.get(name)?;
                stack.push(name.to_string());
                let wrapped = format!("type __AstroSightAlias = {rhs};");
                let tree = parser::parse_source(wrapped.as_bytes(), LangId::Rust).ok()?;
                if tree.root_node().has_error() {
                    return None;
                }
                let type_item = direct_named_child(tree.root_node(), "type_item")?;
                let rhs_node = type_item.child_by_field_name("type")?;
                if contains_unsupported_alias_type_recursive(rhs_node) {
                    return None;
                }
                canonicalize_node_recursive(
                    rhs_node,
                    wrapped.as_bytes(),
                    wrapped.len(),
                    aliases,
                    shadowed,
                    stack,
                    canonical,
                )?;
                stack.pop();
                return Some(());
            }
        }
        if node.child_count() == 0 {
            if !matches!(node.kind(), "line_comment" | "block_comment") {
                push_token(canonical, node.kind(), node.utf8_text(source).ok()?);
            }
            return Some(());
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            canonicalize_node_recursive(child, source, end, aliases, shadowed, stack, canonical)?;
        }
        Some(())
    }

    fn contains_unsupported_alias_type_recursive(node: Node<'_>) -> bool {
        if matches!(node.kind(), "dynamic_type" | "macro_invocation") {
            return true;
        }
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .any(contains_unsupported_alias_type_recursive)
    }

    fn collect_type_identifiers_recursive(
        node: Node<'_>,
        source: &[u8],
        names: &mut HashSet<String>,
    ) -> Option<()> {
        if node.kind() == "type_identifier" {
            names.insert(node.utf8_text(source).ok()?.to_string());
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect_type_identifiers_recursive(child, source, names)?;
        }
        Some(())
    }

    fn find_function_node_recursive(node: Node<'_>) -> Option<Node<'_>> {
        if matches!(node.kind(), "function_item" | "function_signature_item") {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if let Some(found) = find_function_node_recursive(child) {
                return Some(found);
            }
        }
        None
    }

    type CanonicalSummary = (Option<()>, String, bool, Vec<String>, Vec<String>, usize);

    fn summarize(
        result: Option<()>,
        canonical: &CanonicalSignature,
        stack: &[String],
    ) -> CanonicalSummary {
        let mut expanded: Vec<_> = canonical.expanded_alias_names.iter().cloned().collect();
        expanded.sort();
        let mut names: Vec<_> = canonical.names.iter().cloned().collect();
        names.sort();
        (
            result,
            canonical.tokens.clone(),
            canonical.expanded_alias,
            expanded,
            names,
            stack.len(),
        )
    }

    fn empty_canonical() -> CanonicalSignature {
        CanonicalSignature {
            tokens: String::new(),
            expanded_alias: false,
            expanded_alias_names: HashSet::new(),
            function_name: String::new(),
            names: HashSet::new(),
        }
    }

    /// 反復に書き換えた正規化・別名の検査・型名の収集・関数の探索が、浅い入力で修正前の
    /// 再帰版と同じ結果 (失敗の有無、トークン列、名前、展開した別名、途中で失敗した時点の
    /// 状態まで) を返す。別名の展開・循環・未対応の別名・修飾パス・型引数の式・
    /// コメント・本体の手前で切る位置の各分岐を通す。
    #[test]
    fn iterative_canonicalization_matches_recursive_version() {
        let alias_source = b"pub type A = u32;\npub type B = Option<A>;\npub type Dyn = Box<dyn Fn()>;\npub type Mac = m!();\npub(crate) type R = u8;\npub type Cyc = Cyc2;\npub type Cyc2 = Cyc;\npub type Arr = [A; N];\n";
        let index = TypeAliasIndex::from_source(alias_source).unwrap();
        let signatures = [
            "pub fn f(x: A, y: B) -> Option<A> {",
            "pub fn f(x: crate::A, y: A::Assoc) -> u32 {",
            "pub fn f<T: Into<A>>(x: T, y: A) {",
            "pub fn f<T>(x: T) -> T {",
            "pub fn f(x: [u8; N]) {",
            "pub fn f(x: Foo<{ N }>, y: Bar<N>) {",
            "pub fn f(x: R) {",
            "pub fn f(x: Dyn) {",
            "pub fn f(x: Mac) {",
            "pub fn f(x: Cyc) {",
            "pub fn f(x: Arr) {",
            "pub fn f(x: /* c */ A, // d\n y: [u8; 4]) -> Vec<A> where A: Copy {",
            "fn g(x: A);",
            "pub fn f(x: A) -> A { let y: B = x; y }",
            "pub async unsafe fn f<'a>(x: &'a mut A, ...) -> impl Iterator<Item = B> + 'a {",
            "struct S;",
            "impl S { fn m(&self, a: A) -> B {} }",
        ];
        for signature in signatures {
            let (source, original_len) = parseable_signature_source(signature);
            let tree = parser::parse_source(&source, LangId::Rust).unwrap();
            let root = tree.root_node();
            assert_eq!(
                find_function_node(root).map(|node| node.id()),
                find_function_node_recursive(root).map(|node| node.id()),
                "{signature}"
            );
            let mut cursor = root.walk();
            let mut nodes = vec![root];
            let mut index_in_tree = 0;
            while index_in_tree < nodes.len() {
                let node = nodes[index_in_tree];
                nodes.extend(node.named_children(&mut cursor));
                index_in_tree += 1;
            }
            for node in nodes {
                assert_eq!(
                    contains_unsupported_alias_type(node),
                    contains_unsupported_alias_type_recursive(node),
                    "{signature} / {}",
                    node.kind()
                );
                let (mut iterative, mut recursive) = (HashSet::new(), HashSet::new());
                assert_eq!(
                    collect_type_identifiers(node, &source, &mut iterative),
                    collect_type_identifiers_recursive(node, &source, &mut recursive),
                    "{signature} / {}",
                    node.kind()
                );
                assert_eq!(iterative, recursive, "{signature} / {}", node.kind());
            }
            let Some(function) = find_function_node(root) else {
                continue;
            };
            let end = function
                .child_by_field_name("body")
                .map(|body| body.start_byte())
                .unwrap_or(original_len)
                .min(original_len);
            for shadowed in [HashSet::new(), HashSet::from(["T".to_string()])] {
                let (mut stack, mut canonical) = (Vec::new(), empty_canonical());
                let result = canonicalize_node(
                    function,
                    &source,
                    end,
                    &index,
                    &shadowed,
                    &mut stack,
                    &mut canonical,
                );
                let iterative = summarize(result, &canonical, &stack);
                let (mut stack, mut canonical) = (Vec::new(), empty_canonical());
                let result = canonicalize_node_recursive(
                    function,
                    &source,
                    end,
                    &index,
                    &shadowed,
                    &mut stack,
                    &mut canonical,
                );
                assert_eq!(
                    iterative,
                    summarize(result, &canonical, &stack),
                    "{signature} / shadowed {shadowed:?}"
                );
            }
        }
        // import 行の正規化 (`canonical_token_stream`) も同じ走査を使う。
        let imports = b"use a::{b, c::{d as e}, f::*};\nuse crate::g /* c */ ::h;\n";
        let tree = parser::parse_source(imports, LangId::Rust).unwrap();
        let mut cursor = tree.root_node().walk();
        for item in tree.root_node().named_children(&mut cursor) {
            let (mut stack, mut canonical) = (Vec::new(), empty_canonical());
            let result = canonicalize_node_recursive(
                item,
                imports,
                item.end_byte(),
                &TypeAliasIndex::default(),
                &HashSet::new(),
                &mut stack,
                &mut canonical,
            );
            assert_eq!(
                canonical_token_stream(item, imports),
                result.map(|()| canonical.tokens),
                "{}",
                item.utf8_text(imports).unwrap()
            );
        }
    }
}
