//! Rust 関数シグネチャの非契約差分を正規化・証明する。
//!
//! 通常引数の binding-side `mut` 除去と、root-level の公開 type alias の導入・展開だけで
//! 実体型が変わらないことの old/new AST 証明、関数に `const` を足しただけの証明を担う。

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, any_node, walk_preorder};
use crate::language::LangId;

mod const_qualifier;
mod parameter_patterns;
pub(crate) use const_qualifier::{RustConstQualifier, RustFileItems, rust_const_qualifier};
pub(crate) use parameter_patterns::{
    rust_parameter_import_environment, rust_parameter_pattern_contract,
};

/// old/new それぞれの同一ファイル内 `pub type` を使い、関数シグネチャの型表現が
/// 透過的な alias の導入 (新側で使う) か展開 (旧側で使っていた alias を展開した形に戻す) だけで
/// 変わったかを証明するコンテキスト。
///
/// 対象は非 generic・一意・unrestricted `pub` の alias に限定する。import alias、private / restricted
/// alias、generic、重複、循環、parse 不能はすべて「同値と証明できない」側へ倒す。
pub(crate) struct RustTypeAliasEquivalence {
    old: TypeAliasIndex,
    new: TypeAliasIndex,
}

/// working tree 側を先に parse した alias 同値証明候補。
///
/// impact では Pass 1 の既存 tree から構築して再 parse を避ける。旧側を読んだ後、どちらの
/// 側にも利用可能な root `pub type` が無ければ証明しない ([`Self::with_old_source`])。
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

    /// working tree ソースを先に parse する。解析できなければ None。
    pub(crate) fn prepare_new_source(new_source: &[u8]) -> Option<PreparedRustTypeAliases> {
        let tree = parser::parse_source(new_source, LangId::Rust).ok()?;
        Self::prepare_new_tree(tree.root_node(), new_source)
    }

    /// 呼び出し側が既に parse した working tree の root を再利用する。
    pub(crate) fn prepare_new_tree(
        new_root: Node<'_>,
        new_source: &[u8],
    ) -> Option<PreparedRustTypeAliases> {
        Some(PreparedRustTypeAliases {
            new: TypeAliasIndex::from_root(new_root, new_source)?,
        })
    }

    /// old/new の完全な root 関数ヘッダを比較する。diff の変更行や抽出済みの
    /// 1 行シグネチャは名前の候補にだけ使い、同値性の証拠にはしない。
    ///
    /// 旧新のどちらかで alias を展開し、展開後のトークン列が一致することを求める。新側で
    /// alias を使う (導入) だけでなく、旧側で使っていた alias を展開した形に戻す変更も同じ型を
    /// 表す。移動と同じ差分では旧新が別のファイルなので、alias が片側にしか無いことも多い。
    pub(crate) fn prove_function(&self, name: &str) -> bool {
        self.proven_pair(name).is_some()
    }

    /// 移動 (旧新が別のファイル) の関数ヘッダの同値証明。[`Self::prove_function`] に加え、
    /// ヘッダと展開した alias の右辺に現れる名前が、どちらのファイルでも同じものを指すことを
    /// 求める ([`resolves_alike_across_files`])。同じファイルの旧新と違い、字面が同じ root の
    /// 宣言や `use crate::...` でも別のものを指し得る。
    pub(crate) fn prove_moved_function(&self, name: &str) -> bool {
        let Some((old, new)) = self.proven_pair(name) else {
            return false;
        };
        let names = old.names.union(&new.names).cloned().collect();
        let roots = PathRoots::union(&old.path_roots, &new.path_roots);
        resolves_alike_across_files(&self.old, &self.new, &names, &roots, &|used| {
            self.expanded_alias_itself(&old, &new, used)
        })
    }

    /// 同値と証明できた旧新の正規化シグネチャ。
    fn proven_pair(&self, name: &str) -> Option<(CanonicalSignature, CanonicalSignature)> {
        let name = rust_name_key(name);
        // import の意味が同じと証明できない状態で、alias RHS 内の同名型を字面だけで同一視しない。
        if self.old.imports != self.new.imports {
            return None;
        }
        // マクロ呼び出し・`extern crate`・inner attribute などで root のスコープが曖昧な側は、
        // 名前を何が束縛しているか分からない (展開しない側でも、生成された型が std の名前を
        // 隠し得る)。
        if self.old.ambiguous_scope || self.new.ambiguous_scope {
            return None;
        }
        if self.old.function_counts.get(name) != Some(&1)
            || self.new.function_counts.get(name) != Some(&1)
        {
            return None;
        }
        let old_header = self.old.root_function_headers.get(name)?;
        let new_header = self.new.root_function_headers.get(name)?;
        let old = canonicalize_signature(old_header, &self.old)?;
        let new = canonicalize_signature(new_header, &self.new)?;
        if old.function_name != name || new.function_name != name {
            return None;
        }
        if !(old.expanded_alias || new.expanded_alias) || old.tokens != new.tokens {
            return None;
        }
        // 型・配列長などに現れる名前の root 束縛が変化した場合は、字面の
        // 一致だけで実体型が同じとは言えない。片側で展開した alias 自身だけを除外する。
        old.names
            .union(&new.names)
            .all(|used| {
                self.expanded_alias_itself(&old, &new, used)
                    || self.old.root_bindings.get(used) == self.new.root_bindings.get(used)
            })
            .then_some((old, new))
    }

    /// `used` が片側で展開した alias 自身で、反対側に同じ名前の root 束縛が無い (新側で導入した
    /// alias・旧側で展開をやめた alias)。右辺に現れる名前は含めない。
    fn expanded_alias_itself(
        &self,
        old: &CanonicalSignature,
        new: &CanonicalSignature,
        used: &str,
    ) -> bool {
        (new.expanded_alias_names.contains(used) && !self.old.root_bindings.contains_key(used))
            || (old.expanded_alias_names.contains(used)
                && !self.new.root_bindings.contains_key(used))
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
    /// どちらの側にも証明に使える alias が無ければ None。
    pub(crate) fn with_old_source(self, old_source: &[u8]) -> Option<RustTypeAliasEquivalence> {
        let old = TypeAliasIndex::from_source(old_source)?;
        (!old.aliases.is_empty() || !self.new.aliases.is_empty())
            .then_some(RustTypeAliasEquivalence { old, new: self.new })
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
    /// root のマクロ呼び出し・`extern crate`・inner attribute・属性付き `use` があり、root の
    /// 名前を何が束縛しているか確定できない。
    ambiguous_scope: bool,
    /// root の `use` が束縛する名前と、その束縛 (glob は含めない)。
    use_bindings: HashMap<String, Vec<UseBinding>>,
    /// root に glob の `use` がある (std からの glob でも、どの名前を持ち込むかは std の版や
    /// モジュールで変わる)。
    has_glob: bool,
    /// root の項目に、名前を生成し得る属性 (未知の属性マクロ・std 以外の derive) がある。
    unknown_attribute: bool,
}

/// root の `use` による名前 1 つの束縛。
#[derive(PartialEq, Eq)]
struct UseBinding {
    /// 束縛先のパス (`std::collections::HashMap`)。
    path: String,
    /// パスの先頭の字面が std / core / alloc。先頭名の未束縛は別に確かめる。
    std_rooted: bool,
}

/// どのファイル・クレートでも同じものを指すパスの先頭。
const STD_ROOTS: [&str; 3] = ["std", "core", "alloc"];

/// 索引用の名前。表示と比較に使うトークンは元の綴りを保つ。
fn rust_name_key(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

impl TypeAliasIndex {
    /// root で `name` を束縛する宣言か `use` がある。
    fn binds(&self, name: &str) -> bool {
        let name = rust_name_key(name);
        self.root_bindings.contains_key(name) || self.use_bindings.contains_key(name)
    }
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
                if crate::engine::impact::rust_std_origin::safe_attribute(child, source).is_none() {
                    index.unknown_attribute = true;
                }
                continue;
            }
            if child.kind() == "inner_attribute_item" {
                ambiguous_root_scope = true;
                continue;
            }
            if matches!(child.kind(), "line_comment" | "block_comment") {
                continue;
            }
            // root のマクロ呼び出しは `foo! { .. }` なら `macro_invocation`、`foo!(..);` なら
            // それを包む `expression_statement` になる (どちらも項目を生成し得る)。
            if matches!(
                child.kind(),
                "macro_invocation" | "expression_statement" | "extern_crate_declaration"
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
                if index.collect_use_bindings(child, source).is_none() {
                    ambiguous_root_scope = true;
                }
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
        index.ambiguous_scope = ambiguous_root_scope;
        index.imports.sort();
        Some(index)
    }

    /// root の `use` 1 つが束縛する名前を `use_bindings` に積む。読めない形は None。
    fn collect_use_bindings(&mut self, item: Node<'_>, source: &[u8]) -> Option<()> {
        let mut bindings = Vec::new();
        collect_use_tree(item.child_by_field_name("argument")?, source, &mut bindings)?;
        for (name, path) in bindings {
            let path: Vec<_> = path
                .iter()
                .map(|name| rust_name_key(name).to_owned())
                .collect();
            match name {
                Some(name) => self
                    .use_bindings
                    .entry(rust_name_key(&name).to_owned())
                    .or_default()
                    .push(UseBinding {
                        std_rooted: path
                            .first()
                            .is_some_and(|root| STD_ROOTS.contains(&root.as_str())),
                        path: path.join("::"),
                    }),
                None => self.has_glob = true,
            }
        }
        Some(())
    }

    fn collect_alias(&mut self, item: Node<'_>, source: &[u8], attributed: bool) {
        let Some(name_node) = item.child_by_field_name("name") else {
            return;
        };
        let Ok(name) = name_node.utf8_text(source) else {
            return;
        };
        let name = rust_name_key(name).to_string();
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
        let name = rust_name_key(name);
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
        let name = rust_name_key(name.utf8_text(source).ok()?).to_string();
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
                    let name = rust_name_key(name);
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

/// use tree が束縛する名前と、束縛先のパスの区切りごとの名前を `out` に積む。glob は名前を
/// None にする (パスの下のどの名前も束縛し得る)。`crate` / `self` / `super` もパスの区切りとして
/// 積む。読めない形は None。入れ子の深さだけ再帰しないよう明示スタックで辿る。
fn collect_use_tree(
    argument: Node<'_>,
    source: &[u8],
    out: &mut Vec<(Option<String>, Vec<String>)>,
) -> Option<()> {
    let mut pending: Vec<(Node<'_>, std::rc::Rc<[String]>)> =
        vec![(argument, std::rc::Rc::from(Vec::new()))];
    while let Some((node, prefix)) = pending.pop() {
        match node.kind() {
            "identifier" | "scoped_identifier" => {
                let mut path = prefix.to_vec();
                path.extend(path_segments(node, source)?);
                out.push((Some(path.last()?.clone()), path));
            }
            // `a::{self}` は `a` を束縛する
            "self" => out.push((Some(prefix.last()?.clone()), prefix.to_vec())),
            "use_as_clause" => {
                let mut path = prefix.to_vec();
                path.extend(path_segments(node.child_by_field_name("path")?, source)?);
                let alias = node.child_by_field_name("alias")?.utf8_text(source).ok()?;
                // `use Trait as _;` は名前を束縛しない
                if alias != "_" {
                    out.push((Some(alias.to_owned()), path));
                }
            }
            "scoped_use_list" => {
                let mut nested = prefix.to_vec();
                if let Some(path) = node.child_by_field_name("path") {
                    nested.extend(path_segments(path, source)?);
                }
                pending.push((node.child_by_field_name("list")?, std::rc::Rc::from(nested)));
            }
            "use_list" => {
                let mut cursor = node.walk();
                let items: Vec<_> = node
                    .named_children(&mut cursor)
                    .filter(|item| !matches!(item.kind(), "line_comment" | "block_comment"))
                    .collect();
                pending.extend(
                    items
                        .into_iter()
                        .rev()
                        .map(|item| (item, std::rc::Rc::clone(&prefix))),
                );
            }
            "use_wildcard" => {
                let mut path = prefix.to_vec();
                let mut cursor = node.walk();
                if let Some(inner) = node
                    .named_children(&mut cursor)
                    .find(|child| !matches!(child.kind(), "line_comment" | "block_comment"))
                {
                    path.extend(path_segments(inner, source)?);
                }
                out.push((None, path));
            }
            _ => return None,
        }
    }
    Some(())
}

/// パス (`std::collections::HashMap`・`crate::a`・`::std::fmt`) の区切りごとの名前。先頭の `::` は
/// 落とす。`crate` / `self` / `super` はその字面。`<T as Tr>::` などの形は None。
fn path_segments(mut node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    loop {
        match node.kind() {
            "scoped_identifier" | "scoped_type_identifier" => {
                segments.push(
                    node.child_by_field_name("name")?
                        .utf8_text(source)
                        .ok()?
                        .to_owned(),
                );
                match node.child_by_field_name("path") {
                    Some(path) => node = path,
                    None => break,
                }
            }
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                segments.push(node.utf8_text(source).ok()?.to_owned());
                break;
            }
            _ => return None,
        }
    }
    segments.reverse();
    Some(segments)
}

/// 契約部分に現れるパスの先頭の名前。
#[derive(Default)]
struct PathRoots {
    names: HashSet<String>,
    /// 先頭を名前で表せないパス (`<T as Tr>::X`) やマクロの型がある。
    unsupported: bool,
}

impl PathRoots {
    fn union(old: &Self, new: &Self) -> Self {
        Self {
            names: old.names.union(&new.names).cloned().collect(),
            unsupported: old.unsupported || new.unsupported,
        }
    }
}

/// 移動 (旧新が別のファイル) の契約部分に現れる名前が、旧新のどちらのファイルでも同じものを
/// 指すか。同じファイルの旧新と違い、字面が同じでも別のものを指し得る (別ファイルの同名の
/// root 宣言、`use crate::...`・`use super::...`、子モジュール名で始まるパス、glob の `use`、
/// マクロ)。名前の束縛を積極的に確かめられたときだけ同じとみなす:
/// - root のスコープがどちらでも曖昧でない ([`TypeAliasIndex::ambiguous_scope`])
/// - パスの先頭が std / core / alloc で、その名前をどちらでも root で束縛し直しておらず、glob の
///   `use` も無い
/// - 素の名前が、どちらでも std / core / alloc を先頭にする同じパスの `use` で束縛されているか、
///   どちらでも束縛されていない (prelude の型・引数名など)。後者は、どちらにも glob の `use` と
///   名前を生成し得る属性が無いときだけ (生成された項目や glob は prelude の名前を隠し得る)。
///   root の宣言で束縛される名前は、字面が同じでも別のファイルの別のものなので同じとみなさない
///
/// `checked` は呼び出し側が別に確かめた名前 (片側で展開した alias 自身)。
fn resolves_alike_across_files(
    old: &TypeAliasIndex,
    new: &TypeAliasIndex,
    names: &HashSet<String>,
    roots: &PathRoots,
    checked: &dyn Fn(&str) -> bool,
) -> bool {
    let sides = [old, new];
    // Rust は識別子を NFC で比べる。字面で作った索引から同名の束縛を取りこぼさない。
    if names.iter().any(|name| !name.is_ascii())
        || sides.iter().any(|side| {
            side.root_bindings
                .keys()
                .chain(side.use_bindings.keys())
                .any(|name| !name.is_ascii())
        })
    {
        return false;
    }
    if sides.iter().any(|side| side.ambiguous_scope) || roots.unsupported {
        return false;
    }
    if !roots.names.is_empty()
        && !roots.names.iter().all(|root| {
            STD_ROOTS.contains(&root.as_str())
                && sides.iter().all(|side| !side.binds(root) && !side.has_glob)
        })
    {
        return false;
    }
    names.iter().all(|name| {
        if checked(name) {
            return true;
        }
        if sides
            .iter()
            .any(|side| side.root_bindings.contains_key(name.as_str()))
        {
            return false;
        }
        match (
            old.use_bindings.get(name.as_str()),
            new.use_bindings.get(name.as_str()),
        ) {
            (Some(old), Some(new)) => {
                old == new
                    && old.iter().all(|binding| {
                        binding.std_rooted
                            && binding.path.split("::").next().is_some_and(|root| {
                                sides.iter().all(|side| !side.binds(root) && !side.has_glob)
                            })
                    })
            }
            (None, None) => sides
                .iter()
                .all(|side| !side.has_glob && !side.unknown_attribute),
            _ => false,
        }
    })
}

/// 移動 (旧新が別のファイル) の root の定数 `name` の型に現れる名前が、旧新のどちらのファイルでも
/// 同じものを指すか ([`resolves_alike_across_files`])。値だけの変更 (`const_value`) は型を字面で
/// 比べるので、別ファイルでは名前が同じものを指すことを別に確かめる。値の評価が要る配列長や
/// const generic 引数、マクロの型は証明しない。`old` / `new` は解析済みのファイル全体。
pub(crate) fn moved_constant_type_resolves_alike(
    old: (Node<'_>, &[u8]),
    new: (Node<'_>, &[u8]),
    name: &str,
) -> bool {
    let contract =
        |(root, source): (Node<'_>, &[u8])| -> Option<(TypeAliasIndex, CanonicalSignature)> {
            let index = TypeAliasIndex::from_root(root, source)?;
            let mut cursor = root.walk();
            let mut items = root.named_children(&mut cursor).filter(|item| {
                matches!(item.kind(), "const_item" | "static_item")
                    && item
                        .child_by_field_name("name")
                        .and_then(|item_name| item_name.utf8_text(source).ok())
                        == Some(name)
            });
            let (Some(item), None) = (items.next(), items.next()) else {
                return None;
            };
            let ty = item.child_by_field_name("type")?;
            let mut canonical = CanonicalSignature::default();
            canonicalize_node(
                ty,
                source,
                ty.end_byte(),
                &TypeAliasIndex::default(),
                &HashSet::new(),
                &mut Vec::new(),
                &mut canonical,
            )?;
            Some((index, canonical))
        };
    let (Some((old_index, old)), Some((new_index, new))) = (contract(old), contract(new)) else {
        return false;
    };
    resolves_alike_across_files(
        &old_index,
        &new_index,
        &old.names.union(&new.names).cloned().collect(),
        &PathRoots::union(&old.path_roots, &new.path_roots),
        &|_| false,
    )
}

/// root 自由関数の移動と const 追加を、両ファイルの宣言と型の所有元で証明する。
pub(crate) struct MovedRustConstAdditions {
    old: MovedConstFile,
    new: MovedConstFile,
}

struct MovedConstFile {
    index: TypeAliasIndex,
    items: RustFileItems,
    functions: HashMap<String, (RustConstQualifier, CanonicalSignature)>,
}

impl MovedConstFile {
    fn new(root: Node<'_>, source: &[u8]) -> Option<Self> {
        let mut functions = HashMap::new();
        let mut cursor = root.walk();
        for function in root.named_children(&mut cursor) {
            if function.kind() != "function_item"
                || function
                    .child_by_field_name("return_type")
                    .is_some_and(|ty| {
                        any_node(ty, Children::Named, |node| node.kind() == "abstract_type")
                    })
            {
                continue;
            }
            let Some(qualifier) = rust_const_qualifier(function, source) else {
                continue;
            };
            let body = function.child_by_field_name("body")?;
            let header =
                std::str::from_utf8(source.get(function.start_byte()..body.start_byte())?).ok()?;
            let Some(contract) = canonicalize_signature(header, &TypeAliasIndex::default()) else {
                continue;
            };
            functions.insert(contract.function_name.clone(), (qualifier, contract));
        }
        Some(Self {
            index: TypeAliasIndex::from_root(root, source)?,
            items: RustFileItems::new(root, source)?,
            functions,
        })
    }
}

impl MovedRustConstAdditions {
    pub(crate) fn new(old: (Node<'_>, &[u8]), new: (Node<'_>, &[u8])) -> Option<Self> {
        Some(Self {
            old: MovedConstFile::new(old.0, old.1)?,
            new: MovedConstFile::new(new.0, new.1)?,
        })
    }

    pub(crate) fn proves(&self, name: &str) -> bool {
        let name = rust_name_key(name);
        if self.old.index.function_counts.get(name) != Some(&1)
            || self.new.index.function_counts.get(name) != Some(&1)
            || self.old.index.imports != self.new.index.imports
        {
            return false;
        }
        let (Some((old_qualifier, old)), Some((new_qualifier, new))) =
            (self.old.functions.get(name), self.new.functions.get(name))
        else {
            return false;
        };
        if !new_qualifier.only_adds_const_to(&self.new.items, old_qualifier, &self.old.items) {
            return false;
        }
        let names = old.names.union(&new.names).cloned().collect();
        resolves_alike_across_files(
            &self.old.index,
            &self.new.index,
            &names,
            &PathRoots::union(&old.path_roots, &new.path_roots),
            &|_| false,
        )
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
                .entry(rust_name_key(name.utf8_text(source).ok()?).to_string())
                .or_default() += 1;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    Some(())
}

#[derive(Default)]
struct CanonicalSignature {
    tokens: String,
    expanded_alias: bool,
    expanded_alias_names: HashSet<String>,
    function_name: String,
    names: HashSet<String>,
    /// 移動の組の検査 ([`resolves_alike_across_files`]) だけが使う。
    path_roots: PathRoots,
}

fn canonicalize_signature(signature: &str, aliases: &TypeAliasIndex) -> Option<CanonicalSignature> {
    let (source, original_len) = parseable_signature_source(signature);
    let tree = parser::parse_source(&source, LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let function = find_function_node(tree.root_node())?;
    let function_name = rust_name_key(
        function
            .child_by_field_name("name")?
            .utf8_text(&source)
            .ok()?,
    )
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
        function_name,
        ..CanonicalSignature::default()
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
    // primitive_type も同名の type や use で隠せる。
    if matches!(
        node.kind(),
        "identifier" | "type_identifier" | "primitive_type"
    ) {
        canonical
            .names
            .insert(rust_name_key(node.utf8_text(source).ok()?).to_string());
    }
    // パスの先頭は、鎖の外側の 1 か所 (親が修飾パスでない位置) でだけ数える。
    if matches!(node.kind(), "scoped_identifier" | "scoped_type_identifier")
        && !parent.is_some_and(|parent| {
            matches!(
                parent.kind(),
                "scoped_identifier" | "scoped_type_identifier"
            )
        })
    {
        match path_segments(node, source).and_then(|segments| segments.into_iter().next()) {
            Some(root) => {
                canonical.path_roots.names.insert(root);
            }
            None => canonical.path_roots.unsupported = true,
        }
    }
    if node.kind() == "macro_invocation" {
        canonical.path_roots.unsupported = true;
    }
    if node.kind() == "type_identifier" {
        let name = rust_name_key(node.utf8_text(source).ok()?);
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
    let mut canonical = CanonicalSignature::default();
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
            names.insert(rust_name_key(name).to_string());
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

    /// 旧新のどちらにも alias が無ければ証明の文脈を作らない。片側にあれば作る。
    #[test]
    fn sources_without_candidate_alias_skip_the_proof() {
        let plain: &[u8] = b"pub fn collect() -> Vec<(u64, String)> { vec![] }";
        let aliased: &[u8] =
            b"pub type Entry = (u64, String);\npub fn collect() -> Vec<Entry> { vec![] }";
        let context = |old: &[u8], new: &[u8]| {
            RustTypeAliasEquivalence::prepare_new_source(new)
                .and_then(|prepared| prepared.with_old_source(old))
        };
        assert!(context(plain, plain).is_none());
        for (old, new) in [(plain, aliased), (aliased, plain)] {
            assert!(
                context(old, new).is_some_and(|context| context.prove_function("collect")),
                "{} -> {}",
                String::from_utf8_lossy(old),
                String::from_utf8_lossy(new)
            );
        }
    }

    /// 旧側で使っていた alias を展開した形に戻す変更も、導入と同じく同じ型を表す。
    #[test]
    fn alias_expansion_on_the_old_side_is_equivalent() {
        let old = b"pub type Entry = (u64, String);\npub fn collect() -> Vec<Entry> { vec![] }";
        for new in [
            "pub type Entry = (u64, String);\npub fn collect() -> Vec<(u64, String)> { vec![] }",
            "pub fn collect() -> Vec<(u64, String)> { vec![] }",
            "pub type Item = (u64, String);\npub fn collect() -> Vec<Item> { vec![] }",
        ] {
            let context =
                RustTypeAliasEquivalence::new(old, new.as_bytes()).expect("valid Rust sources");
            assert!(context.prove_function("collect"), "{new}");
        }
    }

    /// 旧側の alias の右辺が別の型を指すことになる変更は、展開でも証明しない。
    #[test]
    fn alias_expansion_on_the_old_side_fails_closed() {
        for (label, old, new) in [
            (
                "展開した形の型が違う",
                "pub type Entry = (u64, String);\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub fn collect() -> Vec<(u32, String)> { vec![] }",
            ),
            (
                "右辺の名前の root 束縛が変わる",
                "pub struct Id;\npub type Entry = (Id, String);\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub struct Id(u8);\npub fn collect() -> Vec<(Id, String)> { vec![] }",
            ),
            (
                "右辺の名前の import が変わる",
                "use a::Id;\npub type Entry = (Id, String);\npub fn collect() -> Vec<Entry> { vec![] }",
                "use b::Id;\npub fn collect() -> Vec<(Id, String)> { vec![] }",
            ),
            (
                "generic な alias",
                "pub type Entry<T> = (T, String);\npub fn collect() -> Vec<Entry<u64>> { vec![] }",
                "pub fn collect() -> Vec<(u64, String)> { vec![] }",
            ),
            (
                "非公開の alias",
                "type Entry = (u64, String);\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub fn collect() -> Vec<(u64, String)> { vec![] }",
            ),
        ] {
            let context = RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes())
                .expect("valid Rust sources");
            assert!(!context.prove_function("collect"), "{label}");
        }
    }

    /// 移動 (旧新が別のファイル) の別名の証明は、ヘッダと別名の右辺の名前がどちらのファイルでも
    /// 同じものを指すことも求める。同じファイルの旧新なら同じものを指す字面 (root の宣言・
    /// `crate::` の import・glob) は、別のファイルでは別のものを指し得る。
    #[test]
    fn moved_alias_proof_requires_names_that_resolve_alike() {
        let context = |old: &str, new: &str| {
            RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes())
                .expect("valid Rust sources")
        };
        let expanded = "use std::collections::HashMap;\npub fn count(m: &HashMap<String, String>) -> usize { 0 }";
        let aliased = "use std::collections::HashMap;\npub type Tags = HashMap<String, String>;\npub fn count(m: &Tags) -> usize { 0 }";
        for (old, new) in [
            (expanded, aliased),
            (aliased, expanded),
            (
                "pub fn count(m: &::std::collections::HashMap<u8, u8>) -> usize { 0 }",
                "pub type Tags = ::std::collections::HashMap<u8, u8>;\npub fn count(m: &Tags) -> usize { 0 }",
            ),
            (
                "pub fn count(x: (u8, u16)) -> usize { 0 }",
                "pub type Pair = (u8, u16);\npub fn count(x: Pair) -> usize { 0 }",
            ),
        ] {
            assert!(
                context(old, new).prove_moved_function("count"),
                "{old} -> {new}"
            );
        }
        // 同じファイルの旧新なら証明できるが、別のファイルでは名前が別のものを指し得る。
        for (label, old, new) in [
            (
                "字面が同じ root の型",
                "pub struct Marks;\npub fn count(_: &Marks) -> usize { 0 }",
                "pub struct Marks;\npub type Tags = Marks;\npub fn count(_: &Tags) -> usize { 0 }",
            ),
            (
                "crate 起点の use",
                "use crate::types::Map;\npub fn count(_: &Map) -> usize { 0 }",
                "use crate::types::Map;\npub type Tags = Map;\npub fn count(_: &Tags) -> usize { 0 }",
            ),
            (
                "super 起点のパス",
                "pub fn count(_: &super::Map) -> usize { 0 }",
                "pub type Tags = super::Map;\npub fn count(_: &Tags) -> usize { 0 }",
            ),
            (
                "子モジュール名で始まるパス",
                "pub fn count(_: &types::Map) -> usize { 0 }",
                "pub type Tags = types::Map;\npub fn count(_: &Tags) -> usize { 0 }",
            ),
            (
                "std からの glob",
                "use std::fmt::*;\npub fn count(_: Option<Error>) -> usize { 0 }",
                "use std::fmt::*;\npub type Failure = Option<Error>;\npub fn count(_: Failure) -> usize { 0 }",
            ),
            (
                "std を root で束縛し直す",
                "pub mod std {}\npub fn count(_: &::std::string::String) -> usize { 0 }",
                "pub mod std {}\npub type Text = ::std::string::String;\npub fn count(_: &Text) -> usize { 0 }",
            ),
            (
                "名前を生成し得る属性",
                "#[my_macro]\npub struct Other;\npub fn count(_: String) -> usize { 0 }",
                "#[my_macro]\npub struct Other;\npub type Text = String;\npub fn count(_: Text) -> usize { 0 }",
            ),
            (
                "std 以外の derive",
                "#[derive(Serialize)]\npub struct Other;\npub fn count(_: String) -> usize { 0 }",
                "#[derive(Serialize)]\npub struct Other;\npub type Text = String;\npub fn count(_: Text) -> usize { 0 }",
            ),
            (
                "型の中のマクロ",
                "pub fn count(_: u8, _: ty!()) -> usize { 0 }",
                "pub type Byte = u8;\npub fn count(_: Byte, _: ty!()) -> usize { 0 }",
            ),
        ] {
            let context = context(old, new);
            assert!(
                context.prove_function("count"),
                "{label}: 移さない変更なら証明できる"
            );
            assert!(!context.prove_moved_function("count"), "{label}");
        }
    }

    #[test]
    fn moved_alias_proof_rejects_names_with_unresolved_nfc_bindings() {
        // Kelvin sign は Rust では ASCII の K と同じ名前になる。
        let old = "pub struct K(u8);\npub fn count(_: K) -> usize { 0 }";
        let new = "pub struct K(u16);\npub type Key = K;\npub fn count(_: Key) -> usize { 0 }";
        let context = RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes()).unwrap();
        assert!(!context.prove_moved_function("count"));
    }

    #[test]
    fn moved_alias_proof_rejects_shadowed_standard_import_roots() {
        for root in STD_ROOTS {
            let module = format!("pub mod {root} {{ pub mod types {{ pub struct Id; }} }}\n");
            for binding in [format!("mod {root}"), format!("mod r#{root}")] {
                let module = module.replace(&format!("mod {root}"), &binding);
                let old = format!("{module}use {root}::types::Id;\npub fn take(_: Id) {{}}");
                let new = format!(
                    "{module}use {root}::types::Id;\npub type Value = Id;\npub fn take(_: Value) {{}}"
                );
                for (old, new) in [(&old, &new), (&new, &old)] {
                    let context =
                        RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes()).unwrap();
                    assert!(
                        !context.prove_moved_function("take"),
                        "{binding}: {old} -> {new}"
                    );
                }
            }
        }
    }

    #[test]
    fn moved_constant_proof_rejects_shadowed_standard_import_roots() {
        for root in STD_ROOTS {
            for binding in [format!("mod {root}"), format!("mod r#{root}")] {
                let prefix = format!(
                    "pub {binding} {{ pub mod types {{ pub struct Id; }} }}\nuse {root}::types::Id;\n"
                );
                let old = format!("{prefix}pub const WEEK: Option<Id> = None;");
                let new = format!("{prefix}pub const WEEK: Option<Id> = Some(Id);");
                let old_tree = parser::parse_source(old.as_bytes(), LangId::Rust).unwrap();
                let new_tree = parser::parse_source(new.as_bytes(), LangId::Rust).unwrap();
                assert!(
                    !moved_constant_type_resolves_alike(
                        (old_tree.root_node(), old.as_bytes()),
                        (new_tree.root_node(), new.as_bytes()),
                        "WEEK",
                    ),
                    "{binding}"
                );
            }
        }
    }

    #[test]
    fn moved_proof_rejects_shadowed_primitives() {
        for declaration in ["pub type u8 = u16;", "pub type r#u8 = u16;"] {
            let old = "pub const WEEK: u8 = 1;";
            let new = format!("{declaration}\npub const WEEK: u8 = 1 + 0;");
            let old_tree = parser::parse_source(old.as_bytes(), LangId::Rust).unwrap();
            let new_tree = parser::parse_source(new.as_bytes(), LangId::Rust).unwrap();
            assert!(!moved_constant_type_resolves_alike(
                (old_tree.root_node(), old.as_bytes()),
                (new_tree.root_node(), new.as_bytes()),
                "WEEK",
            ));
            let old = "pub fn take(_: u8) {}";
            let new = format!("{declaration}\npub type Value = u8;\npub fn take(_: Value) {{}}");
            let context = RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes()).unwrap();
            assert!(!context.prove_moved_function("take"), "{declaration}");
        }
    }

    #[test]
    fn moved_proof_normalizes_raw_binding_names() {
        for prefix in [
            "pub struct Day;",
            "pub struct r#Day;",
            "use crate::types::Day as r#Day;",
            "use crate::types::Day;",
        ] {
            for name in ["Day", "r#Day"] {
                let old = format!("{prefix}\npub const WEEK: Option<{name}> = None;");
                let new = format!("{prefix}\npub const WEEK: Option<{name}> = None::<{name}>;");
                let old_tree = parser::parse_source(old.as_bytes(), LangId::Rust).unwrap();
                let new_tree = parser::parse_source(new.as_bytes(), LangId::Rust).unwrap();
                assert!(
                    !moved_constant_type_resolves_alike(
                        (old_tree.root_node(), old.as_bytes()),
                        (new_tree.root_node(), new.as_bytes()),
                        "WEEK",
                    ),
                    "{prefix}: {name}"
                );
                let old = format!("{prefix}\npub fn take(_: Option<{name}>) {{}}");
                let new = format!(
                    "{prefix}\npub type Value = Option<{name}>;\npub fn take(_: Value) {{}}"
                );
                let context =
                    RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes()).unwrap();
                assert!(!context.prove_moved_function("take"), "{prefix}: {name}");
            }
        }
    }

    #[test]
    fn moved_proof_preserves_transparent_raw_aliases() {
        for declaration in ["Value", "r#Value"] {
            for usage in ["Value", "r#Value"] {
                let prefix = "use std::num::NonZeroU8 as r#Number;\n";
                let old = format!("{prefix}pub fn take(_: Option<Number>) {{}}");
                let new = format!(
                    "{prefix}pub type {declaration} = Option<Number>;\npub fn take(_: {usage}) {{}}"
                );
                let context =
                    RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes()).unwrap();
                assert!(context.prove_function("take"), "{declaration}: {usage}");
                assert!(
                    context.prove_moved_function("take"),
                    "{declaration}: {usage}"
                );
            }
        }
    }

    fn moved_const_addition_is_proven(old: &str, new: &str) -> bool {
        let old_tree = parser::parse_source(old.as_bytes(), LangId::Rust).unwrap();
        let new_tree = parser::parse_source(new.as_bytes(), LangId::Rust).unwrap();
        MovedRustConstAdditions::new(
            (old_tree.root_node(), old.as_bytes()),
            (new_tree.root_node(), new.as_bytes()),
        )
        .is_some_and(|context| context.proves("f"))
    }

    #[test]
    fn moved_const_addition_proves_explicit_contracts() {
        for (old, new) in [
            (
                "pub fn f(x: u8) -> u8 { x }",
                "pub const fn f(x: u8) -> u8 { x + 1 }",
            ),
            (
                "#[inline]\npub fn f(x: u8) -> u8 { x }",
                "#[inline]\npub const fn f(x: u8) -> u8 { x }",
            ),
            (
                "pub fn f<T: Copy>(x: T) -> T { x }",
                "pub const fn f<T: Copy>(x: T) -> T { x }",
            ),
            (
                "pub fn f(x: [u8; 4]) -> [u8; 4] { x }",
                "pub const fn f(x: [u8; 4]) -> [u8; 4] { x }",
            ),
            (
                "use core::num::NonZeroU8;\npub fn f(_: Option<NonZeroU8>) {}",
                "use core::num::NonZeroU8;\npub const fn f(_: Option<NonZeroU8>) {}",
            ),
            (
                "type Other = u8;\npub fn f() -> u8 { 0 }",
                "type Other = u16;\npub const fn f() -> u8 { 1 }",
            ),
            (
                "pub fn f(x: u8) -> u8 { assert!(x > 0); x }",
                "pub const fn f(x: u8) -> u8 { assert!(x > 0); x }",
            ),
        ] {
            assert!(moved_const_addition_is_proven(old, new), "{old} -> {new}");
        }
    }

    #[test]
    fn moved_const_addition_requires_file_and_owner_proofs() {
        for (old, new) in [
            (
                "pub const fn f(x: u8) -> u8 { x }",
                "pub fn f(x: u8) -> u8 { x }",
            ),
            ("pub fn f(x: u8) {}", "pub const fn f(x: u16) {}"),
            (
                "pub fn f() -> u8 { 0 }",
                "pub const unsafe fn f() -> u8 { 0 }",
            ),
            (
                "pub fn f() -> u8 { 0 }",
                "#[inline]\npub const fn f() -> u8 { 0 }",
            ),
            (
                "pub struct Id;\npub fn f(_: Id) {}",
                "pub struct Id;\npub const fn f(_: Id) {}",
            ),
            (
                "pub struct r#Id;\npub fn f(_: Id) {}",
                "pub struct r#Id;\npub const fn f(_: Id) {}",
            ),
            (
                "use crate::types::Id as r#Arg;\npub fn f(_: Arg) {}",
                "use crate::types::Id as r#Arg;\npub const fn f(_: Arg) {}",
            ),
            (
                "pub type u8 = u16;\npub fn f(_: u8) {}",
                "pub type u8 = u16;\npub const fn f(_: u8) {}",
            ),
            (
                "pub mod r#std { pub mod fmt { pub struct Error; } }\nuse std::fmt::Error;\npub fn f(_: Error) {}",
                "pub mod r#std { pub mod fmt { pub struct Error; } }\nuse std::fmt::Error;\npub const fn f(_: Error) {}",
            ),
            (
                "use crate::other::*;\npub fn f(_: u8) {}",
                "use crate::other::*;\npub const fn f(_: u8) {}",
            ),
            (
                "pub struct Other;\npub fn f() -> u8 { 0 }",
                "pub struct Other;\nimpl Other {}\npub const fn f() -> u8 { 0 }",
            ),
            (
                "pub fn f() -> impl Copy { 0u8 }",
                "pub const fn f() -> impl Copy { 0u8 }",
            ),
            (
                "pub fn f() {}",
                "pub const fn f() {}\nmod other { pub fn f() {} }",
            ),
        ] {
            assert!(!moved_const_addition_is_proven(old, new), "{old} -> {new}");
        }
    }

    /// root のマクロ呼び出しがある側は、展開する側でもしない側でも証明しない。旧側だけにある
    /// 変更 (今の新側で展開する向き) も含む。
    #[test]
    fn ambiguous_root_scope_on_either_side_blocks_alias_proof() {
        for (old, new) in [
            (
                "local_string!();\npub fn count(_: &String) -> usize { 0 }",
                "pub type Tags = String;\npub fn count(_: &Tags) -> usize { 0 }",
            ),
            (
                "pub type Tags = String;\npub fn count(_: &Tags) -> usize { 0 }",
                "local_string!();\npub fn count(_: &String) -> usize { 0 }",
            ),
        ] {
            let context = RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes())
                .expect("valid Rust sources");
            assert!(!context.prove_function("count"), "{old} -> {new}");
            assert!(!context.prove_moved_function("count"), "{old} -> {new}");
        }
    }

    /// 移動する定数の型に現れる名前も、どちらのファイルでも同じものを指すことを求める。
    #[test]
    fn moved_constant_type_requires_names_that_resolve_alike() {
        let resolves = |old: &str, new: &str| {
            let old_tree = parser::parse_source(old.as_bytes(), LangId::Rust).expect("parse");
            let new_tree = parser::parse_source(new.as_bytes(), LangId::Rust).expect("parse");
            moved_constant_type_resolves_alike(
                (old_tree.root_node(), old.as_bytes()),
                (new_tree.root_node(), new.as_bytes()),
                "WEEK",
            )
        };
        for (old, new) in [
            (
                "pub const WEEK: [(&str, u8); 2] = [(\"Mon\", 1), (\"Tue\", 2)];",
                "const NAMES: [&str; 2] = [\"Mon\", \"Tue\"];\npub const WEEK: [(&str, u8); 2] = [(NAMES[0], 1), (NAMES[1], 2)];",
            ),
            (
                "use std::num::NonZeroU8;\npub const WEEK: Option<NonZeroU8> = None;",
                "use std::num::NonZeroU8;\n#[derive(Debug)]\npub struct Other;\npub const WEEK: Option<NonZeroU8> = NonZeroU8::new(1);",
            ),
            (
                "pub static WEEK: &str = \"a\";",
                "pub struct Other;\npub static WEEK: &str = \"b\";",
            ),
        ] {
            assert!(resolves(old, new), "{old} -> {new}");
        }
        for (label, old, new) in [
            (
                "primitive を隠し得る glob と属性",
                "pub static WEEK: &str = \"a\";",
                "use crate::other::*;\n#[my_macro]\npub struct Other;\npub static WEEK: &str = \"b\";",
            ),
            (
                "字面が同じ root の型",
                "pub struct Day;\npub const WEEK: Day = Day;",
                "pub struct Day;\npub const WEEK: Day = Day;",
            ),
            (
                "std からの別の glob で持ち込む名前",
                "use std::fmt::*;\npub const WEEK: Option<Error> = None;",
                "use std::io::*;\npub const WEEK: Option<Error> = None;",
            ),
            (
                "同じ名前で import した std の別の型",
                "use std::fmt::Error;\npub const WEEK: Option<Error> = None;",
                "use std::io::Error;\npub const WEEK: Option<Error> = None;",
            ),
            (
                "crate 起点の use",
                "use crate::day::Day;\npub const WEEK: Option<Day> = None;",
                "use crate::day::Day;\npub const WEEK: Option<Day> = None;",
            ),
            (
                "値の評価が要る配列長",
                "const fn width() -> usize { 1 }\npub const WEEK: [u8; width()] = [1; width()];",
                "const fn width() -> usize { 2 }\npub const WEEK: [u8; width()] = [2; width()];",
            ),
            (
                "定数の配列長",
                "const N: usize = 2;\npub const WEEK: [u8; N] = [1, 2];",
                "const N: usize = 2;\npub const WEEK: [u8; N] = [2, 1];",
            ),
            (
                "型の中のマクロ",
                "pub const WEEK: ty!() = 1;",
                "pub const WEEK: ty!() = 2;",
            ),
            (
                "root のマクロ呼び出し",
                "pub const WEEK: Option<u8> = None;",
                "local_option!();\npub const WEEK: Option<u8> = Some(1);",
            ),
            (
                "名前を生成し得る属性",
                "pub const WEEK: Option<u8> = None;",
                "#[my_macro]\npub struct Other;\npub const WEEK: Option<u8> = Some(1);",
            ),
            (
                "同じ名前の定数が 2 件",
                "pub const WEEK: u8 = 1;",
                "#[cfg(unix)]\npub const WEEK: u8 = 2;\n#[cfg(not(unix))]\npub const WEEK: u8 = 3;",
            ),
        ] {
            assert!(!resolves(old, new), "{label}");
        }
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
        if matches!(
            node.kind(),
            "identifier" | "type_identifier" | "primitive_type"
        ) {
            canonical
                .names
                .insert(rust_name_key(node.utf8_text(source).ok()?).to_string());
        }
        if node.kind() == "type_identifier" {
            let name = rust_name_key(node.utf8_text(source).ok()?);
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
            names.insert(rust_name_key(node.utf8_text(source).ok()?).to_string());
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
        CanonicalSignature::default()
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
