//! `impl Trait` と async の戻り値に漏れる auto trait を、本体と宣言の依存から保守的に比べる。
//!
//! 変更された library package の宣言を名前でつなぐ過大近似であり、型や局所束縛の同一性は証明しない。
//! 本体が参照し得る宣言が変われば、同名の候補を含めて未検証の契約変更として残す。公開面と、
//! 両立しない cfg の判定は既存の証明を使う。ファイル数・読み込み量・parse の不足は申告する。
//!
//! glob の import は束縛名を決められない。可視性の修飾の無い glob が変える束縛は、置いた module M と
//! 子孫の module の中だけなので、M の範囲の印 (`@glob:<M の鍵>`) を束縛名に入れ、M と子孫の全ての宣言を変化の
//! 起点にする (Issue #381)。宣言は自分の module の範囲の印だけを参照し、範囲の印の変化は子の module の範囲の
//! 印へ伝える (宣言ごとに祖先の全ての印を持たない)。可視性の修飾のある glob と位置の分からない glob は、
//! package 全体の印 (`@unknown-imports`) にする。宣言を経ずに外へ効く構成 (trait の impl・マクロの呼び出し・
//! 組み込みでない属性・extern ブロック) は名前の伝播に載らないので、範囲の印から伝播した変化がそこへ届くか
//! (展開の見えないマクロの呼び出しと属性は、範囲の glob で意味の変わる宣言があれば届くとする)、旧新のどちらかの
//! 版で範囲を確かめ切れない (置き場をたどり切れない・lib crate のファイルを全て読めていない・`include!` がある・
//! glob のファイルを文字列のパスで読み得る・範囲と祖先の `mod` に属性マクロが付く) なら、package 全体に戻す。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::ControlFlow;

use super::rust_dependencies::crates::{CrateVersion, LibraryVersions};
use super::rust_dependencies::placements::Placements;
use super::rust_dependencies::test_scopes::TestScopes;
use super::*;
use crate::engine::rust_use_tree::{RustPubUseEdge, expand_rust_use_tree_edges_ast};
use crate::engine::tree_walk::{Children, Walk, any_node, walk_preorder};
use crate::models::review::{ApiModificationUnverified, ApiModificationUnverifiedReason};
use crate::models::truncation::{TruncationInfo, TruncationReason};

const MAX_FILES: usize = 10_000;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const UNKNOWN_IMPORTS: &str = "@unknown-imports";
/// module の範囲の印の接頭辞 (`@glob:<module の鍵>`。鍵は crate root からの経路を親から増分で作った指紋)。
/// Issue #381。
const GLOB_SCOPE: &str = "@glob:";
/// 版の lib crate の置き場をたどれない宣言が参照する印。どの module の範囲の glob の変化でも起点に入る
/// (Issue #381)。
const ANY_GLOB_SCOPE: &str = "@glob-any";

type Key = (String, [u8; 32], String, String);
type ChangedNames = BTreeMap<String, Vec<Vec<AvailabilityAttr>>>;

struct Scope {
    depth: usize,
    name: String,
    key: blake3::Hash,
    /// impl・trait の見出し (本体を除く) の識別子。囲む関数の依存に入れる (Issue #381)。
    header: BTreeSet<String>,
    /// 中の宣言を置く module の範囲の鍵 (ファイルを置いた module ごと。impl・trait は囲む module のもの。版の
    /// lib crate の木をたどれなければ None。Issue #381)。
    modules: Option<Vec<blake3::Hash>>,
}

struct PublicFunction {
    name: String,
    kind: String,
    signature: String,
    context: RustDeclarationContext,
    body: blake3::Hash,
    references: BTreeSet<String>,
}

struct Declaration {
    fingerprint: blake3::Hash,
    name: String,
    bound_names: BTreeSet<String>,
    references: BTreeSet<String>,
    conditions: Vec<AvailabilityAttr>,
    public: Option<PublicFunction>,
    /// 可視性の修飾の無い glob を含む use 宣言の、置いた module の経路 (位置が分かるとき。Issue #381)。
    glob_scope: Option<Vec<String>>,
    /// `mod` の宣言か (範囲の glob で意味の変わる項目には数えない。Issue #381)。
    module: bool,
}

/// 宣言を経ずに外へ効き得る構成 (Issue #381)。trait の impl (対象の型の auto trait と trait の解決)、
/// extern ブロック (中の関数を宣言として持たない)、関数の本体などの中の impl (中の項目を宣言として持たない) は、
/// 参照する名前を構成のトークンから読める。マクロの呼び出し (展開が式の中のブロックにも impl や項目を作り得る)
/// と組み込みでない属性 (属性マクロ・derive が付いた型以外にも項目を作り得る) は、展開が見えない。
/// 参照する名前と置いた位置の印のどれかが範囲の glob の変化に入れば、その変化が届き得る。展開の見えない構成は、
/// 範囲の glob で意味の変わる宣言があれば、名前を書いていなくても展開がそれを名指し得るので届き得る。
struct Transmitter {
    references: BTreeSet<String>,
    /// 有る条件 (属性は付いた項目の条件)。
    conditions: Vec<AvailabilityAttr>,
    /// 展開が見えないか。
    opaque: bool,
}

#[derive(Default)]
struct Snapshot {
    declarations: BTreeMap<Key, Vec<Declaration>>,
    /// lib crate に置いたファイル (か置き場の分からないファイル) の、宣言を経ずに外へ効き得る構成 (Issue #381)。
    /// どの module にも使われていないファイルの構成は、`include!` で取り込まれてもその呼び出しが展開の見えない
    /// 構成として数えるので持たない。
    transmitters: Vec<Transmitter>,
    /// 宣言の `mod` か中身に組み込みでない属性の付いた module の経路。属性マクロが配下ごと書き換え得る
    /// (Issue #381)。
    attributed: BTreeSet<Vec<String>>,
    /// 中身を全て読んだか、テストの module として比べないファイル (Issue #381)。lib crate のファイルを全て
    /// 読めたかを確かめる。
    added: BTreeSet<String>,
    /// マクロの呼び出しと属性の文字列のパスで読み得るファイル (小文字にそろえたパス。Issue #381)。中の glob は、
    /// 展開した先の module の束縛も変え得るので範囲に絞れない。
    included: BTreeSet<String>,
    /// どの範囲の glob も確かめ切れない構成がある (Issue #381): 取り込むファイルを決められない構成 (`include!`、
    /// 解けない文字列のパス。どのファイルも取り込み得る) か、付く項目の無い外側の属性 (展開を確かめられない)。
    unconfined: bool,
    /// module の範囲の印 → 子の module の範囲の印 (Issue #381)。範囲の印の変化を子孫の範囲へ伝える。
    scope_children: BTreeMap<String, BTreeSet<String>>,
}

/// 版での、ファイルの置き場とテストの module の判定。
struct FilePlacement<'t, 'v, 'a> {
    /// ファイルを置いた module の全て (crate root からの経路)。版の lib crate の木をたどれなければ None、
    /// どの module にも使われていなければ空。
    modules: Option<Vec<Vec<String>>>,
    /// テストの module の判定 (Issue #382)。公開の契約はテストでない build のものなので、旧新の両方で
    /// テストの module のスコープにある宣言は、依存の変化の起点にも伝播の中継にも入れない
    /// (`rust_dependencies/dependents.rs` と同じ約束)。置いた module が 1 つに決まるときだけ使う。
    tests: Option<&'t TestScopes<'v, 'a>>,
}

impl<'t, 'v, 'a> FilePlacement<'t, 'v, 'a> {
    /// 版 `version` でのファイル `path` の置き場。`#[path]` もたどった木で引く。
    fn of(tests: Option<&'t TestScopes<'v, 'a>>, version: &CrateVersion<'_>, path: &str) -> Self {
        Self {
            modules: version.tree_modules(path),
            tests,
        }
    }

    /// lib crate に置いたファイルか、置き場の分からないファイルか (どの module にも使われていないファイルで
    /// ない)。
    fn in_library(&self) -> bool {
        self.modules
            .as_ref()
            .is_none_or(|modules| !modules.is_empty())
    }

    /// 置き場の分からない (版の lib crate を開けない) ファイル。
    fn unknown() -> Self {
        Self {
            modules: None,
            tests: None,
        }
    }

    /// ファイルを置いた module (1 つに決まるとき)。
    fn module(&self) -> Option<&[String]> {
        match self.modules.as_deref() {
            Some([module]) => Some(module),
            _ => None,
        }
    }

    /// ファイルの中の inline module の経路 `inline` のスコープが、テストの module の中か。
    fn excludes(&self, inline: &[String]) -> bool {
        self.tests
            .zip(self.scope_module(inline))
            .is_some_and(|(tests, module)| tests.contains(&module))
    }

    /// ファイルの中の inline module の経路 `inline` のスコープの、crate root からの経路 (置いた module が
    /// 1 つに決まるとき)。
    fn scope_module(&self, inline: &[String]) -> Option<Vec<String>> {
        let mut module = self.module()?.to_vec();
        module.extend_from_slice(inline);
        Some(module)
    }

    /// ファイルを置いた全ての module での、inline module の経路 `inline` のスコープの経路。
    fn placed_scopes<'s>(&'s self, inline: &'s [String]) -> impl Iterator<Item = Vec<String>> + 's {
        self.modules.iter().flatten().map(move |module| {
            let mut scope = module.clone();
            scope.extend_from_slice(inline);
            scope
        })
    }
}

/// 鍵 `key` の module の範囲の印。
fn glob_scope_marker(key: &blake3::Hash) -> String {
    format!("{GLOB_SCOPE}{}", key.to_hex())
}

/// crate root の範囲の鍵。
fn root_scope_key() -> blake3::Hash {
    blake3::hash(b"crate")
}

/// 鍵 `parent` の module の子 `name` の範囲の鍵 (経路を複製せず、親から増分で作る)。
fn child_scope_key(parent: &blake3::Hash, name: &str) -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    hash.update(parent.as_bytes());
    hash.update(&(name.len() as u64).to_le_bytes());
    hash.update(name.as_bytes());
    hash.finalize()
}

/// 宣言の位置の印: 置いた module (ファイルを置いた module ごと) の範囲の印。祖先の範囲の変化は、範囲の印の
/// 子へのつながり ([`Snapshot::scope_children`]) で届く。版の lib crate の木をたどれなければ (`keys` が None)、
/// どの範囲の glob の変化でも起点に入る印。どの module にも使われていなければ印は無い (lib crate に入るのは
/// `include!` だけで、その呼び出しは外へ効き得る構成)。
fn position_markers(keys: Option<&[blake3::Hash]>) -> BTreeSet<String> {
    match keys {
        Some(keys) => keys.iter().map(glob_scope_marker).collect(),
        None => BTreeSet::from([ANY_GLOB_SCOPE.to_owned()]),
    }
}

/// スコープの並びのうち、inline module の名前 (ファイルの中の経路)。
fn inline_modules(scopes: &[Scope]) -> Vec<String> {
    scopes
        .iter()
        .filter_map(|scope| scope.name.strip_prefix("mod_item:").map(str::to_owned))
        .collect()
}

fn public_functions(declarations: &[Declaration]) -> Vec<&PublicFunction> {
    declarations
        .iter()
        .filter_map(|item| item.public.as_ref())
        .collect()
}

fn bodies_changed(before: &[&PublicFunction], after: &[&PublicFunction]) -> bool {
    let mut groups: BTreeMap<_, (Vec<_>, Vec<_>)> = BTreeMap::new();
    for function in before {
        groups
            .entry(*function.body.as_bytes())
            .or_default()
            .0
            .push(&function.context);
    }
    for function in after {
        groups
            .entry(*function.body.as_bytes())
            .or_default()
            .1
            .push(&function.context);
    }
    groups.values().any(|(old, new)| {
        super::rust_availability::compare(old, new)
            != super::rust_availability::AvailabilityChange::Same
    })
}

/// use 宣言 `node` の束縛名と、module の範囲の印にした glob の module。glob は、可視性の修飾が無く置いた
/// module が分かれば、その範囲の印と位置の分からない参照側の印にする (Issue #381)。そうでなければ package
/// 全体の印にする。`keys` は置いた位置の範囲の鍵 ([`Scope::modules`])。
fn import_bound_names(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    inline: &[String],
    file: &FilePlacement<'_, '_, '_>,
    keys: Option<&[blake3::Hash]>,
) -> (BTreeSet<String>, Option<Vec<String>>) {
    let mut names = BTreeSet::new();
    let mut edges = Vec::new();
    let scope = file.scope_module(inline);
    let expanded = (|| {
        // 置き場の決まらないファイルは、パスの規約の module で展開する。
        let module = match &scope {
            Some(module) => module.clone(),
            None => {
                let (_, mut module) = super::rust_public::source_module_path(path)?;
                module.extend_from_slice(inline);
                module
            }
        };
        let argument = node.child_by_field_name("argument")?;
        expand_rust_use_tree_edges_ast(argument, source, &[], &module, &module, None, &mut edges)?;
        // `_` の trait import は束縛名を作らず、メソッド解決を変え得る。
        (!any_node(argument, Children::Named, |node| {
            node.kind() == "use_as_clause"
                && node
                    .child_by_field_name("alias")
                    .is_some_and(|alias| alias.utf8_text(source) == Ok("_"))
        }))
        .then_some(())
    })();
    if expanded.is_none() || edges.is_empty() {
        names.insert(UNKNOWN_IMPORTS.to_owned());
    }
    // 可視性の修飾の無い (`pub(self)`・`pub(in self)` と同じ) glob の束縛は、置いた module と子孫からしか
    // 見えない。
    let private = {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|child| child.kind() == "visibility_modifier")
            .is_none_or(|visibility| {
                let text: String = visibility
                    .utf8_text(source)
                    .unwrap_or_default()
                    .split_whitespace()
                    .collect();
                matches!(text.as_str(), "pub(self)" | "pub(inself)")
            })
    };
    let mut glob_scope = None;
    for edge in edges {
        match edge {
            RustPubUseEdge::Named { exported_name, .. } => {
                names.insert(
                    crate::language::normalize_identifier(
                        crate::language::LangId::Rust,
                        &exported_name,
                    )
                    .into_owned(),
                );
            }
            RustPubUseEdge::Wildcard { .. } => {
                match scope
                    .as_ref()
                    .filter(|_| private)
                    .zip(keys.and_then(|keys| match keys {
                        [key] => Some(key),
                        _ => None,
                    })) {
                    Some((module, key)) => {
                        names.insert(glob_scope_marker(key));
                        names.insert(ANY_GLOB_SCOPE.to_owned());
                        glob_scope = Some(module.clone());
                    }
                    // glob の候補名を狭く証明できない場合は package の opaque 戻り値に残す。
                    None => {
                        names.insert(UNKNOWN_IMPORTS.to_owned());
                    }
                }
            }
        }
    }
    (names, glob_scope)
}

fn matching_move(moved: &MovedSymbol, path: &str, function: &PublicFunction) -> bool {
    moved.to == path
        && moved.name == function.name
        && moved.kind == function.kind
        && moved.to_inline_module == super::rust_declarations::inline_module_of(&function.context)
}

fn previous_declarations<'a>(
    old: &'a Snapshot,
    key: &Key,
    function: &PublicFunction,
    moved: &[MovedSymbol],
) -> Option<(&'a str, &'a [Declaration])> {
    if let Some((key, declarations)) = old.declarations.get_key_value(key) {
        return Some((key.0.as_str(), declarations.as_slice()));
    }
    let mut candidates = old.declarations.iter().filter(|(old_key, declarations)| {
        moved.iter().any(|movement| {
            matching_move(movement, &key.0, function)
                && movement.from == old_key.0
                && declarations.iter().any(|declaration| {
                    declaration.public.as_ref().is_some_and(|previous| {
                        previous.name == movement.name
                            && previous.kind == movement.kind
                            && super::rust_declarations::inline_module_of(&previous.context)
                                == movement.from_inline_module
                    })
                })
        })
    });
    let (key, declarations) = candidates.next()?;
    candidates
        .next()
        .is_none()
        .then_some((key.0.as_str(), declarations.as_slice()))
}

fn identifiers(node: tree_sitter::Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let body = matches!(node.kind(), "mod_item" | "trait_item")
        .then(|| node.child_by_field_name("body"))
        .flatten();
    let _: ControlFlow<()> = walk_preorder(node, Children::Named, |node, _| {
        if Some(node) == body {
            return ControlFlow::Continue(Walk::Skip);
        }
        if matches!(
            node.kind(),
            "identifier" | "type_identifier" | "field_identifier"
        ) && let Ok(name) = node.utf8_text(source)
        {
            names.insert(
                crate::language::normalize_identifier(crate::language::LangId::Rust, name)
                    .into_owned(),
            );
        }
        ControlFlow::Continue(Walk::Descend)
    });
    names
}

/// 関数のシグネチャの型の識別子 (引数の型・型引数・戻り値の型・where 節。Issue #381)。async の future と
/// 不透明な戻り値は引数を取り込むので、本体に書かれていない引数の型の auto trait にも依存する。引数の
/// 名前 (パターン) と関数自身の名前は入れない。
fn signature_identifiers(function: tree_sitter::Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let skipped = [
        function.child_by_field_name("name"),
        function.child_by_field_name("body"),
    ];
    let mut names = BTreeSet::new();
    let mut cursor = function.walk();
    for child in function.named_children(&mut cursor) {
        if skipped.contains(&Some(child))
            || matches!(child.kind(), "visibility_modifier" | "function_modifiers")
        {
            continue;
        }
        if child.kind() == "parameters" {
            let mut parameters = child.walk();
            for parameter in child.named_children(&mut parameters) {
                if let Some(ty) = parameter.child_by_field_name("type") {
                    names.extend(identifiers(ty, source));
                }
            }
            continue;
        }
        names.extend(identifiers(child, source));
    }
    names
}

/// impl・trait の見出し (本体を除く) の識別子 (Issue #381)。メソッドの receiver は impl の対象の型。
fn header_identifiers(node: tree_sitter::Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let body = node.child_by_field_name("body");
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| Some(*child) != body && child.kind() != "visibility_modifier")
        .flat_map(|child| identifiers(child, source))
        .collect()
}

/// ノードの中の全ての識別子 (module・trait の本体も)。
fn all_identifiers(node: tree_sitter::Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let _: ControlFlow<()> = walk_preorder(node, Children::Named, |node, _| {
        if matches!(
            node.kind(),
            "identifier" | "type_identifier" | "field_identifier"
        ) && let Ok(name) = node.utf8_text(source)
        {
            names.insert(
                crate::language::normalize_identifier(crate::language::LangId::Rust, name)
                    .into_owned(),
            );
        }
        ControlFlow::Continue(Walk::Descend)
    });
    names
}

/// 前順の走査 (名前付きのノード) で、ノードの直前に並ぶ外側の属性 (間のコメントを飛ばす。書いた順) を、
/// 兄弟をたどり直さずに渡す (Issue #381)。`prev_named_sibling`・`next_named_sibling` は親を根から探し直すので、
/// 深い入れ子の中でノードごとに呼ぶと二乗になる。
#[derive(Default)]
struct AttributeRun<'n> {
    /// 深さごとの、まだ項目の来ていない外側の属性。
    levels: Vec<Vec<tree_sitter::Node<'n>>>,
}

impl<'n> AttributeRun<'n> {
    /// 深さ `depth` のノード `node` を訪れる。`node` の直前の外側の属性と、項目の来ないまま並びの終わった属性
    /// (終えた部分木の分) を返す。外側の属性とコメントは次の項目の分として覚え、自分の分は空。
    fn visit(
        &mut self,
        node: tree_sitter::Node<'n>,
        depth: usize,
    ) -> (Vec<tree_sitter::Node<'n>>, Vec<tree_sitter::Node<'n>>) {
        let mut trailing = Vec::new();
        while self.levels.len() > depth + 1 {
            trailing.extend(self.levels.pop().into_iter().flatten());
        }
        self.levels.resize_with(depth + 1, Vec::new);
        let preceding = match node.kind() {
            "attribute_item" => {
                self.levels[depth].push(node);
                Vec::new()
            }
            "line_comment" | "block_comment" => Vec::new(),
            _ => std::mem::take(&mut self.levels[depth]),
        };
        (preceding, trailing)
    }

    /// 走査の終わりに、項目の来ないまま残った属性。
    fn finish(self) -> Vec<tree_sitter::Node<'n>> {
        self.levels.into_iter().flatten().collect()
    }
}

/// ノードの、宣言を経ずに外へ効き得る構成としての見方 (Issue #381)。名前は記録するときにだけ集める。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Construct {
    /// 外へ効き得る構成。展開が見えないか、見出しの名前だけを参照するか (ほかは全体の名前。属性は付いた項目の
    /// 名前 ([`attributed_item_identifiers`]) も)。
    Transmitter { opaque: bool, header: bool },
    /// 展開しない属性 (中を見ない)。
    Inert,
    /// ほか。
    Other,
}

/// 中身を持つ項目 (module・trait・impl・関数)。付いた属性の構成は見出しの名前だけを参照する (中身の宣言と
/// 構成は、それぞれ名前と位置を持つ。入れ子の属性ごとに中身を集め直さない)。
fn header_only(item: tree_sitter::Node<'_>) -> bool {
    matches!(
        item.kind(),
        "mod_item" | "trait_item" | "impl_item" | "function_item"
    )
}

/// 属性の付いた項目 `item` のうち、属性の構成が参照する名前 ([`header_only`] の項目は見出しだけ)。
fn attributed_item_identifiers(item: tree_sitter::Node<'_>, source: &[u8]) -> BTreeSet<String> {
    if header_only(item) {
        header_identifiers(item, source)
    } else {
        all_identifiers(item, source)
    }
}

/// マクロの呼び出しだけの文 (`name!(..);`)。有る条件の属性は文の前に付くので、文ごと構成として扱う。
fn macro_statement(node: tree_sitter::Node<'_>) -> bool {
    if node.kind() != "expression_statement" {
        return false;
    }
    let mut cursor = node.walk();
    let mut children = node
        .named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"));
    matches!(
        (children.next(), children.next()),
        (Some(child), None) if child.kind() == "macro_invocation"
    )
}

/// ノード `node` の構成。trait の impl は見出しの名前 (中の項目は宣言として持つ)、`nested` (関数の本体などの
/// 中) の impl は中の項目を宣言として持たないので全体の名前、extern ブロックは全体の名前を参照する。マクロの
/// 呼び出し (それだけの文も) と、展開しない属性 ([`super::rust_declarations::transparent_attribute`]) でない
/// 属性は展開が見えない。
fn construct(node: tree_sitter::Node<'_>, source: &[u8], nested: bool) -> Construct {
    if macro_statement(node) {
        return Construct::Transmitter {
            opaque: true,
            header: false,
        };
    }
    match node.kind() {
        "impl_item" if nested => Construct::Transmitter {
            opaque: false,
            header: false,
        },
        "impl_item" if node.child_by_field_name("trait").is_some() => Construct::Transmitter {
            opaque: false,
            header: true,
        },
        "foreign_mod_item" => Construct::Transmitter {
            opaque: false,
            header: false,
        },
        "macro_invocation" => Construct::Transmitter {
            opaque: true,
            header: false,
        },
        "attribute_item" | "inner_attribute_item"
            if super::rust_declarations::transparent_attribute(node, source) =>
        {
            Construct::Inert
        }
        "attribute_item" | "inner_attribute_item" => Construct::Transmitter {
            opaque: true,
            header: false,
        },
        _ => Construct::Other,
    }
}

/// 識別子 `node` が `include` か (`r#include` も)。
fn include_identifier(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    node.kind() == "identifier"
        && node.utf8_text(source).is_ok_and(|text| {
            crate::language::normalize_identifier(crate::language::LangId::Rust, text) == "include"
        })
}

/// エスケープ `text` (`\n`・`\x2e`・`\u{2e}` など) の文字。行の継続 (`\` と改行) は空。解けなければ None。
fn unescape(text: &str) -> Option<String> {
    let rest = text.strip_prefix('\\')?;
    let mut chars = rest.chars();
    let head = chars.next()?;
    let tail = chars.as_str();
    let decoded = match head {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        '0' => '\0',
        '\\' | '\'' | '"' => head,
        '\n' | '\r' => return Some(String::new()),
        'x' => char::from(u8::from_str_radix(tail, 16).ok()?),
        'u' => {
            let digits = tail
                .strip_prefix('{')
                .and_then(|digits| digits.strip_suffix('}'))
                .unwrap_or(tail)
                .replace('_', "");
            char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?
        }
        _ => return None,
    };
    Some(decoded.to_string())
}

/// 文字列リテラル `node` (`string_literal`・`raw_string_literal`) の値 (エスケープを解く。行の継続の後の空白を
/// 飛ばす)。解けなければ None。
fn string_value(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut value = String::new();
    let mut continued = false;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let text = child.utf8_text(source).ok()?;
        match child.kind() {
            "string_content" if continued => value.push_str(text.trim_start()),
            "string_content" => value.push_str(text),
            "escape_sequence" => {
                let decoded = unescape(text)?;
                continued = decoded.is_empty();
                value.push_str(&decoded);
                continue;
            }
            _ => return None,
        }
        continued = false;
    }
    Some(value)
}

/// ファイル `path` の中のノード `node` (マクロの呼び出し・属性・`macro_rules!` の定義・use 宣言) が取り込み得る
/// Rust のファイルを `included` に足す (Issue #381)。値 (エスケープを解いた文字列リテラル 1 つ) が `.rs` で終わる
/// (大文字小文字を問わない) 文字列は、そのファイルのディレクトリからの相対パスとし、小文字にそろえて覚える
/// (展開がそのファイルを読み得る。大文字小文字を区別しないファイルシステムでは別の綴りでも同じファイル)。
/// 取り込むファイルを決められなければ false を返す: `include` の名前 (`include!` の呼び出し、別名・展開・取り込みの
/// 中の名前。`r#include` も) がある、値を解けない文字列か、解けない `.rs` のパス (絶対パスなど) がある。
/// `include!` の取り込み先は文字列でも確かめ切れない (生成物 (`OUT_DIR`) が追跡中のファイルを取り込む、
/// シンボリックリンクや大文字小文字の違う綴りで同じファイルを指す)。
fn record_includes(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    included: &mut BTreeSet<String>,
) -> bool {
    use super::rust_dependencies::crates::{join_path, parent_dir};
    let mut known = true;
    let _: ControlFlow<()> = walk_preorder(node, Children::Named, |inner, _| {
        if matches!(inner.kind(), "string_literal" | "raw_string_literal") {
            match string_value(inner, source) {
                Some(value) if value.to_ascii_lowercase().ends_with(".rs") => {
                    match join_path(parent_dir(path), &value) {
                        Some(target) => {
                            included.insert(target.to_lowercase());
                        }
                        None => known = false,
                    }
                }
                Some(_) => {}
                None => known = false,
            }
            return ControlFlow::Continue(Walk::Skip);
        }
        if include_identifier(inner, source) {
            known = false;
        }
        ControlFlow::Continue(Walk::Descend)
    });
    known
}

/// 宣言の中 (関数の本体など) の構成 (Issue #381)。
#[derive(Default)]
struct Nested {
    /// 外へ効き得る構成 (参照する名前と、展開が見えないか)。
    transmitters: Vec<(BTreeSet<String>, bool)>,
    /// 文字列のパスで取り込み得るファイル。
    included: BTreeSet<String>,
    /// 取り込むファイルを決められない構成がある。
    unconfined: bool,
}

impl Nested {
    /// 1 つの項目 `item` (付く項目が無ければ None) に付く外側の属性 `attributes` のうち、展開の見えないものの
    /// 取り込み得るファイルを確かめ、`named` なら 1 つの構成として参照する名前とともに覚える (項目の名前を属性ごとに
    /// 集め直さない)。展開の見えない属性があったかを返す。
    fn attributes(
        &mut self,
        attributes: &[tree_sitter::Node<'_>],
        item: Option<tree_sitter::Node<'_>>,
        source: &[u8],
        path: &str,
        named: bool,
    ) -> bool {
        let opaque = opaque_attributes(attributes, source);
        if opaque.is_empty() {
            return false;
        }
        for attribute in &opaque {
            self.unconfined |= !record_includes(*attribute, source, path, &mut self.included);
        }
        if named {
            self.transmitters
                .push((attribute_identifiers(&opaque, item, source), true));
        }
        true
    }
}

/// 外側の属性 `attributes` のうち、展開の見えないもの ([`construct`] が [`Construct::Inert`] でない属性)。
fn opaque_attributes<'n>(
    attributes: &[tree_sitter::Node<'n>],
    source: &[u8],
) -> Vec<tree_sitter::Node<'n>> {
    attributes
        .iter()
        .copied()
        .filter(|attribute| construct(*attribute, source, true) != Construct::Inert)
        .collect()
}

/// 展開の見えない属性 `opaque` とその付いた項目 `item` の、1 つの構成として参照する名前。
fn attribute_identifiers(
    opaque: &[tree_sitter::Node<'_>],
    item: Option<tree_sitter::Node<'_>>,
    source: &[u8],
) -> BTreeSet<String> {
    let mut names = item
        .map(|item| attributed_item_identifiers(item, source))
        .unwrap_or_default();
    for attribute in opaque {
        names.extend(all_identifiers(*attribute, source));
    }
    names
}

/// 宣言 `node` (ファイル `path` の中) の中 (関数の本体など) の構成 (Issue #381)。どの設定にも無い `#[cfg]` の
/// 付いた文・式・項目の中は、付いた属性ごと見ない。中の impl と extern ブロックの中の構成も見る。外の構成が
/// 全体の名前を持つ部分木では名前を集め直さない: 展開の見えない属性の付いた項目 ([`header_only`] の項目を除く)
/// の中の構成は、その属性の構成に含まれるので覚えない。impl の中の impl は、外の impl に含まれるので覚えない。
/// 取り込み得るファイルは、どこでも確かめる。兄弟と親はたどり直さない (深い入れ子で二乗にしない)。
fn nested_constructs(node: tree_sitter::Node<'_>, source: &[u8], path: &str) -> Nested {
    let mut nested = Nested::default();
    // 外の構成が全体の名前を持つ部分木の深さ (展開の見えない構成のものと、impl のもの)。
    let mut opaque_cover: Option<usize> = None;
    let mut impl_cover: Option<usize> = None;
    let mut attributes = AttributeRun::default();
    let _: ControlFlow<()> = walk_preorder(node, Children::Named, |inner, depth| {
        if depth == 0 {
            return ControlFlow::Continue(Walk::Descend);
        }
        for cover in [&mut opaque_cover, &mut impl_cover] {
            if cover.is_some_and(|level| depth <= level) {
                *cover = None;
            }
        }
        let (preceding, trailing) = attributes.visit(inner, depth);
        nested.attributes(&trailing, None, source, path, true);
        let kind = inner.kind();
        if matches!(kind, "attribute_item" | "line_comment" | "block_comment") {
            return ControlFlow::Continue(Walk::Skip);
        }
        if !preceding.is_empty()
            && super::rust_declarations::availability_after(inner, &preceding, source)
                .is_some_and(|conditions| super::rust_availability::unsatisfiable(&conditions))
        {
            return ControlFlow::Continue(Walk::Skip);
        }
        let attributed = nested.attributes(
            &preceding,
            Some(inner),
            source,
            path,
            opaque_cover.is_none(),
        );
        if attributed && opaque_cover.is_none() && !header_only(inner) {
            opaque_cover = Some(depth);
        }
        if matches!(kind, "macro_definition" | "use_declaration") {
            nested.unconfined |= !record_includes(inner, source, path, &mut nested.included);
            return ControlFlow::Continue(Walk::Skip);
        }
        match construct(inner, source, true) {
            Construct::Transmitter { opaque, header } => {
                if opaque {
                    nested.unconfined |=
                        !record_includes(inner, source, path, &mut nested.included);
                }
                if opaque_cover.is_none() && !(kind == "impl_item" && impl_cover.is_some()) {
                    let names = if header {
                        header_identifiers(inner, source)
                    } else {
                        all_identifiers(inner, source)
                    };
                    nested.transmitters.push((names, opaque));
                    if kind == "impl_item" {
                        impl_cover = Some(depth);
                    }
                }
                // impl と extern ブロックは中の構成も見る。
                ControlFlow::Continue(if opaque { Walk::Skip } else { Walk::Descend })
            }
            Construct::Inert => ControlFlow::Continue(Walk::Skip),
            Construct::Other => ControlFlow::Continue(Walk::Descend),
        }
    });
    nested.attributes(&attributes.finish(), None, source, path, true);
    nested
}

fn fingerprint(node: tree_sitter::Node<'_>, source: &[u8]) -> blake3::Hash {
    let mut tokens = Vec::new();
    if matches!(node.kind(), "mod_item" | "trait_item") {
        let body = node.child_by_field_name("body");
        let mut cursor = node.walk();
        for child in node
            .children(&mut cursor)
            .filter(|child| Some(*child) != body)
        {
            push_tokens(child, source, &mut tokens);
        }
    } else {
        push_tokens(node, source, &mut tokens);
    }
    let mut previous = node.prev_named_sibling();
    while let Some(attribute) = previous {
        match attribute.kind() {
            "attribute_item" => push_tokens(attribute, source, &mut tokens),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = attribute.prev_named_sibling();
    }
    let mut hash = blake3::Hasher::new();
    for token in tokens {
        let token = match token.split_once(':') {
            Some((kind @ ("identifier" | "type_identifier" | "field_identifier"), name)) => {
                format!(
                    "{kind}:{}",
                    crate::language::normalize_identifier(crate::language::LangId::Rust, name)
                )
            }
            _ => token,
        };
        hash.update(&(token.len() as u64).to_le_bytes());
        hash.update(token.as_bytes());
    }
    hash.finalize()
}

fn opaque_function(node: tree_sitter::Node<'_>) -> bool {
    node.kind() == "function_item"
        && (node
            .child_by_field_name("return_type")
            .is_some_and(|ty| any_node(ty, Children::Named, |node| node.kind() == "abstract_type"))
            || {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|child| child.kind() == "function_modifiers")
                    .is_some_and(|modifiers| {
                        any_node(modifiers, Children::All, |node| node.kind() == "async")
                    })
            })
}

impl Snapshot {
    /// ファイル `path` の宣言を足す。置いた module が分かれば、テストの module のスコープにある宣言を
    /// 足さない。
    fn add(
        &mut self,
        path: &str,
        source: &[u8],
        strip: &ts_strip_internal::StripInternal,
        file: &FilePlacement<'_, '_, '_>,
    ) -> Option<()> {
        // ファイルを置いた module がテストの module なら、中の宣言を比べない。
        if file.excludes(&[]) {
            self.added.insert(path.to_owned());
            return Some(());
        }
        let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
        let root = tree.root_node();
        if root.has_error() {
            return None;
        }
        let (symbols, contexts) = extract_deleted_exported_facts(path, source, strip)?;
        let mut exported: BTreeMap<_, _> = symbols
            .into_iter()
            .zip(contexts?)
            .map(|(symbol, context)| (context.start, (symbol, context)))
            .collect();
        // 深さとともにスコープを戻す。各ノードから祖先をたどり直さない。
        let mut scopes: Vec<Scope> = Vec::new();
        let file_conditions = super::rust_declarations::file_availability(root, source);
        let mut condition_scopes: Vec<(usize, Option<Vec<AvailabilityAttr>>)> = Vec::new();
        // ファイルを置いた module の範囲の鍵 (置いた module ごと。Issue #381)。
        let file_keys: Option<Vec<blake3::Hash>> = file.modules.as_ref().map(|modules| {
            modules
                .iter()
                .map(|module| self.scope_chain(module))
                .collect()
        });
        // 外側の属性は、付く項目を訪れたときに項目とまとめて読む (属性ごとに兄弟をたどり直さない。Issue #381)。
        let mut attributes = AttributeRun::default();
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
            while scopes.last().is_some_and(|scope| scope.depth >= depth) {
                scopes.pop();
            }
            while condition_scopes
                .last()
                .is_some_and(|(level, _)| *level >= depth)
            {
                condition_scopes.pop();
            }
            let (preceding, trailing) = attributes.visit(node, depth);
            // 付く項目の無い外側の属性 (rustc は受理しない) は、展開を確かめられないので範囲に絞らない。
            if !opaque_attributes(&trailing, source).is_empty() {
                self.unconfined = true;
            }
            let kind = node.kind();
            if matches!(kind, "attribute_item" | "line_comment" | "block_comment") {
                return ControlFlow::Continue(Walk::Skip);
            }
            // 置いた位置の範囲の鍵 (`mod` の宣言も、名前を束縛する親の module のもの)。
            let keys = scopes
                .last()
                .map_or_else(|| file_keys.clone(), |scope| scope.modules.clone());
            if matches!(kind, "mod_item" | "impl_item" | "trait_item") {
                let field = if kind == "impl_item" { "type" } else { "name" };
                if let Some(name_node) = node.child_by_field_name(field)
                    && let Ok(name) = name_node.utf8_text(source)
                {
                    let normalized =
                        crate::language::normalize_identifier(crate::language::LangId::Rust, name)
                            .into_owned();
                    let modules = if kind == "mod_item" {
                        keys.as_ref().map(|keys| {
                            keys.iter()
                                .map(|key| self.scope_child(key, &normalized))
                                .collect()
                        })
                    } else {
                        keys.clone()
                    };
                    let name = if kind == "impl_item" {
                        fingerprint(name_node, source).to_hex().to_string()
                    } else {
                        normalized
                    };
                    let name = format!("{kind}:{name}");
                    let mut hash = blake3::Hasher::new();
                    if let Some(parent) = scopes.last() {
                        hash.update(parent.key.as_bytes());
                    }
                    hash.update(&(name.len() as u64).to_le_bytes());
                    hash.update(name.as_bytes());
                    let header = if kind == "mod_item" {
                        BTreeSet::new()
                    } else {
                        header_identifiers(node, source)
                    };
                    scopes.push(Scope {
                        depth,
                        name,
                        key: hash.finalize(),
                        header,
                        modules,
                    });
                }
            }
            // テストの module は、宣言ごと中身も比べない (付いた属性も数えない)。確かめるのは `test` を書いた属性の
            // 付く宣言だけ (囲むテストの module は、その宣言で飛ばしている。片方の版だけ確かめても、比べる宣言が
            // 増えて変化が多く出るだけで、見落としにはならない)。
            if kind == "mod_item"
                && file.tests.is_some()
                && super::rust_declarations::conditions_after(&preceding, source)
                    .iter()
                    .any(|token| matches!(token.as_str(), "identifier:test" | "identifier:r#test"))
                && file.excludes(&inline_modules(&scopes))
            {
                return ControlFlow::Continue(Walk::Skip);
            }
            let macro_attributes = opaque_attributes(&preceding, source);
            let shape = construct(node, source, false);
            if shape == Construct::Inert {
                return ControlFlow::Continue(Walk::Skip);
            }
            let declaration = matches!(
                kind,
                "function_item"
                    | "type_item"
                    | "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "const_item"
                    | "static_item"
                    | "use_declaration"
                    | "macro_definition"
                    | "trait_item"
                    | "mod_item"
            );
            let nested_body = node
                .child_by_field_name("body")
                .filter(|body| body.kind() == "declaration_list");
            if macro_attributes.is_empty()
                && shape == Construct::Other
                && !declaration
                && nested_body.is_none()
            {
                return ControlFlow::Continue(Walk::Descend);
            }
            // 項目の有る条件 (囲む項目とファイルの条件、直前の外側の属性、inline module の中身の先頭の属性)。
            let enclosing = condition_scopes
                .last()
                .map_or(&file_conditions, |(_, conditions)| conditions);
            let conditions = enclosing
                .as_ref()
                .zip(super::rust_declarations::availability_after(
                    node, &preceding, source,
                ))
                .map(|(enclosing, own)| enclosing.iter().cloned().chain(own).collect::<Vec<_>>());
            let available = conditions.clone().unwrap_or_default();
            let satisfiable = !super::rust_availability::unsatisfiable(&available);
            // 展開の見えない属性は、付いた項目とまとめて 1 つの構成として、参照する名前と置いた位置の印と有る条件
            // とともに覚える (Issue #381)。属性マクロの付いた `mod` の宣言は、中身ごと書き換え得る (`scopes` は
            // 宣言した module を含む)。どの設定にも無い項目の属性は数えない。
            if !macro_attributes.is_empty() && satisfiable {
                for attribute in &macro_attributes {
                    if !record_includes(*attribute, source, path, &mut self.included) {
                        self.unconfined = true;
                    }
                }
                if file.in_library() {
                    let mut references =
                        attribute_identifiers(&macro_attributes, Some(node), source);
                    references.extend(position_markers(keys.as_deref()));
                    self.transmitters.push(Transmitter {
                        references,
                        conditions: available.clone(),
                        opaque: true,
                    });
                }
                if kind == "mod_item" {
                    self.attributed
                        .extend(file.placed_scopes(&inline_modules(&scopes)));
                }
            }
            // 宣言を経ずに外へ効き得る構成を、参照する名前と置いた位置の印と有る条件とともに覚える (Issue #381)。
            // 中の項目を宣言として持つ trait の impl と extern ブロック (static を宣言として持つ) は中へ降りる。
            // どの module にも使われていないファイルの構成は持たない (取り込み得るファイルだけは覚える)。
            if let Construct::Transmitter { opaque, header } = shape {
                if satisfiable {
                    if opaque && !record_includes(node, source, path, &mut self.included) {
                        self.unconfined = true;
                    }
                    if file.in_library() {
                        let mut references = if header {
                            header_identifiers(node, source)
                        } else {
                            all_identifiers(node, source)
                        };
                        references.extend(position_markers(keys.as_deref()));
                        self.transmitters.push(Transmitter {
                            references,
                            conditions: available.clone(),
                            opaque,
                        });
                        // 中身の属性 (`#![..]`) は、その module の配下ごと書き換え得る。
                        if kind == "inner_attribute_item" {
                            self.attributed
                                .extend(file.placed_scopes(&inline_modules(&scopes)));
                        }
                    }
                }
                if !matches!(kind, "impl_item" | "foreign_mod_item") {
                    return ControlFlow::Continue(Walk::Skip);
                }
            }
            if !declaration && nested_body.is_none() {
                return ControlFlow::Continue(Walk::Descend);
            }
            if let Some(body) = nested_body {
                let mut nested = conditions;
                if kind != "mod_item" {
                    nested = nested
                        .zip(super::rust_declarations::file_availability(body, source))
                        .map(|(mut conditions, inner)| {
                            conditions.extend(inner);
                            conditions
                        });
                }
                condition_scopes.push((depth, nested));
            }
            if !declaration {
                return ControlFlow::Continue(Walk::Descend);
            }
            // 内部キーには祖先の全名を複製せず、親から増分で作った固定長の指紋を使う。
            let scope = scopes
                .last()
                .map_or_else(|| blake3::hash(&[]), |scope| scope.key);
            // `mod` の宣言は中の取り込みの印も参照する (中の取り込みの変化を、マクロが作る見えない項目の代わりに
            // module の名前で伝える)。
            let imports = format!("@imports:{path}:{}", scope.to_hex());
            if !satisfiable {
                return ControlFlow::Continue(Walk::Skip);
            }
            // 宣言を置いた module (`mod` の宣言は、名前を束縛する親の module) の範囲の印。
            let markers = position_markers(keys.as_deref());
            // `macro_rules!` の定義と use 宣言は、`include!` を呼び得る名前と取り込み得るファイルを確かめる。
            if matches!(kind, "macro_definition" | "use_declaration")
                && !record_includes(node, source, path, &mut self.included)
            {
                self.unconfined = true;
            }
            // 中身を飛ばす宣言は、中 (関数の本体など) の外へ効き得る構成をここで覚える (`macro_rules!` の定義と
            // use 宣言の中は構成を持たない。取り込みは上で確かめた)。
            if !matches!(
                kind,
                "mod_item" | "trait_item" | "macro_definition" | "use_declaration"
            ) {
                let nested = nested_constructs(node, source, path);
                if file.in_library() {
                    for (mut references, opaque) in nested.transmitters {
                        references.extend(markers.iter().cloned());
                        self.transmitters.push(Transmitter {
                            references,
                            conditions: available.clone(),
                            opaque,
                        });
                    }
                }
                self.included.extend(nested.included);
                self.unconfined |= nested.unconfined;
            }
            let name = if kind == "use_declaration" {
                imports.clone()
            } else {
                let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                else {
                    return ControlFlow::Continue(Walk::Skip);
                };
                crate::language::normalize_identifier(crate::language::LangId::Rust, name)
                    .into_owned()
            };
            // 宣言の名前の依存と、囲む impl・trait の見出し (中の項目は見出しの型と trait に依存する) と、
            // 置いた位置の印。
            let mut references = identifiers(node, source);
            for scope in &scopes {
                references.extend(scope.header.iter().cloned());
            }
            references.insert(imports.clone());
            references.extend(markers.iter().cloned());
            let (bound_names, glob_scope) = if kind == "use_declaration" {
                import_bound_names(
                    node,
                    source,
                    path,
                    &inline_modules(&scopes),
                    file,
                    keys.as_deref(),
                )
            } else {
                (BTreeSet::new(), None)
            };
            let start = node.start_position();
            let public = if opaque_function(node) {
                exported.remove(&(start.row, start.column)).and_then(
                    |((name, kind, signature), context)| {
                        let body = node.child_by_field_name("body")?;
                        // 本体と、取り込む引数の型・囲む impl と trait の見出し (receiver の型) に依存する。
                        let mut function_references = identifiers(body, source);
                        function_references.extend(signature_identifiers(node, source));
                        for scope in &scopes {
                            function_references.extend(scope.header.iter().cloned());
                        }
                        function_references.insert(imports.clone());
                        function_references.insert(UNKNOWN_IMPORTS.to_owned());
                        function_references.extend(markers.iter().cloned());
                        Some(PublicFunction {
                            name,
                            kind,
                            signature,
                            context,
                            body: fingerprint(body, source),
                            references: function_references,
                        })
                    },
                )
            } else {
                None
            };
            self.declarations
                .entry((
                    path.to_owned(),
                    *scope.as_bytes(),
                    kind.to_owned(),
                    name.clone(),
                ))
                .or_default()
                .push(Declaration {
                    fingerprint: fingerprint(node, source),
                    name,
                    bound_names,
                    references,
                    conditions: available,
                    public,
                    glob_scope,
                    module: kind == "mod_item",
                });
            ControlFlow::Continue(if matches!(kind, "mod_item" | "trait_item") {
                Walk::Descend
            } else {
                Walk::Skip
            })
        });
        // 項目の来ないまま残った外側の属性 (rustc は受理しない) も、展開を確かめられない。
        if !opaque_attributes(&attributes.finish(), source).is_empty() {
            self.unconfined = true;
        }
        self.added.insert(path.to_owned());
        Some(())
    }

    /// module の範囲の印にした glob の、ファイルと module。
    fn glob_scopes(&self) -> impl Iterator<Item = (&str, &Vec<String>)> {
        self.declarations
            .iter()
            .flat_map(|((path, ..), declarations)| {
                declarations.iter().filter_map(move |declaration| {
                    Some((path.as_str(), declaration.glob_scope.as_ref()?))
                })
            })
    }

    /// 範囲の glob の変化 `glob` (範囲の印から伝播した変化) が、宣言の変化を経ずに外へ効き得る構成に届くか
    /// (Issue #381)。構成が変化した名前 (module の名前も) か位置の印を参照すれば届く。展開の見えない構成は、
    /// 範囲の glob で意味の変わった項目 `items` (`mod` でない宣言の名前) があれば、名前を書いていなくても展開が
    /// それを名指し得るので届く。範囲の外のコードは範囲の private な glob の束縛を名指せないので、意味の変わった
    /// 項目が無ければ観測できるものが無い (展開が crate のソースファイルを読んで範囲の中身を別の場所に作ることは、
    /// `include!` の名前と、1 つの文字列リテラルで書いたパス ([`record_includes`]) でだけ数える)。
    fn transmits(&self, glob: &ChangedNames, items: &ChangedNames) -> bool {
        self.transmitters.iter().any(|transmitter| {
            affected(&transmitter.references, &transmitter.conditions, glob)
                || (transmitter.opaque
                    && items.values().any(|branches| {
                        branches.iter().any(|branch| {
                            let combined: Vec<_> = transmitter
                                .conditions
                                .iter()
                                .chain(branch)
                                .cloned()
                                .collect();
                            !super::rust_availability::unsatisfiable(&combined)
                        })
                    }))
        })
    }

    /// この版で、範囲の glob の変化を範囲の宣言と外へ効き得る構成で確かめる前提がそろうか (Issue #381。版ごとに
    /// 1 度): 置き場をたどり切れている (場所を決められない `mod` の宣言が無い)、lib crate に置いたファイルを全て
    /// 読んだ (テストの module として比べないものを除く)、どの範囲も確かめ切れない構成 ([`Self::unconfined`]) が
    /// 無い。`placements` はこの版の lib crate の置き場。
    fn confinable(&self, placements: Option<&Placements>) -> bool {
        let Some(placements) = placements else {
            return false;
        };
        placements.incomplete.is_none()
            && placements.unknown.is_empty()
            && placements
                .files
                .values()
                .flatten()
                .all(|placement| placement.index.is_some() && self.added.contains(&placement.file))
            && !self.unconfined
    }

    /// この版で、ファイル `path` の module `scope` の範囲の glob を確かめ切れるか ([`Self::confinable`] の版で):
    /// glob のファイルを文字列のパスで読み得ず (展開した先の module の束縛も変える)、範囲と祖先の `mod` の宣言と
    /// 中身に属性マクロが付いていない。
    fn confines(&self, path: &str, scope: &[String]) -> bool {
        !self.included.contains(&path.to_lowercase())
            && !(0..=scope.len()).any(|depth| self.attributed.contains(&scope[..depth]))
    }

    /// crate root から module `module` までの範囲の鍵を作り、範囲の印の子へのつながりを覚える (Issue #381)。
    fn scope_chain(&mut self, module: &[String]) -> blake3::Hash {
        module
            .iter()
            .fold(root_scope_key(), |key, name| self.scope_child(&key, name))
    }

    /// 鍵 `parent` の module の子 `name` の範囲の鍵を作り、範囲の印の子へのつながりを覚える (Issue #381)。
    fn scope_child(&mut self, parent: &blake3::Hash, name: &str) -> blake3::Hash {
        let child = child_scope_key(parent, name);
        self.scope_children
            .entry(glob_scope_marker(parent))
            .or_default()
            .insert(glob_scope_marker(&child));
        child
    }
}

/// module の範囲の印にした glob のうち、旧新のどちらかの版で範囲を確かめ切れないものを package 全体の印に
/// 戻す (Issue #381)。全てのファイルを足した後に呼ぶ。`library` は旧新の lib crate (置き場は範囲の印の glob が
/// あるときだけ引く)。
fn confine(old: &mut Snapshot, new: &mut Snapshot, library: Option<&LibraryVersions<'_>>) {
    let globs: BTreeSet<(String, Vec<String>)> = [&*old, &*new]
        .into_iter()
        .flat_map(|snapshot| snapshot.glob_scopes())
        .map(|(path, scope)| (path.to_owned(), scope.clone()))
        .collect();
    if globs.is_empty() {
        return;
    }
    let placements = [
        library.map(|library| library.old.placements()),
        library.map(|library| library.new.placements()),
    ];
    let confinable = old.confinable(placements[0]) && new.confinable(placements[1]);
    let unconfined: BTreeSet<(String, Vec<String>)> = globs
        .into_iter()
        .filter(|(path, scope)| {
            !(confinable && old.confines(path, scope) && new.confines(path, scope))
        })
        .collect();
    for snapshot in [old, new] {
        for ((path, ..), declarations) in &mut snapshot.declarations {
            for declaration in declarations {
                if declaration
                    .glob_scope
                    .as_ref()
                    .is_some_and(|scope| unconfined.contains(&(path.clone(), scope.clone())))
                {
                    declaration.bound_names.insert(UNKNOWN_IMPORTS.to_owned());
                }
            }
        }
    }
}

fn remember(changes: &mut ChangedNames, name: &str, conditions: &[AvailabilityAttr]) -> bool {
    let branches = changes.entry(name.to_owned()).or_default();
    if branches.iter().any(|known| known == conditions) {
        return false;
    }
    branches.push(conditions.to_vec());
    true
}

fn remember_declaration(
    changes: &mut ChangedNames,
    pending: &mut VecDeque<(String, Vec<AvailabilityAttr>)>,
    declaration: &Declaration,
) {
    for name in std::iter::once(&declaration.name).chain(&declaration.bound_names) {
        if remember(changes, name, &declaration.conditions) {
            pending.push_back((name.clone(), declaration.conditions.clone()));
        }
    }
}

fn affected(
    references: &BTreeSet<String>,
    conditions: &[AvailabilityAttr],
    changes: &ChangedNames,
) -> bool {
    references.iter().any(|name| {
        changes.get(name).is_some_and(|branches| {
            branches.iter().any(|branch| {
                let combined: Vec<_> = conditions.iter().chain(branch).cloned().collect();
                !super::rust_availability::unsatisfiable(&combined)
            })
        })
    })
}

/// 変化を伝える先 (旧新の両方)。
struct Dependents<'s> {
    /// 名前 → それを参照する宣言。
    declarations: BTreeMap<&'s str, Vec<&'s Declaration>>,
    /// module の範囲の印 → 子の module の範囲の印 (Issue #381)。宣言は自分の module の範囲の印だけを参照するので、
    /// 範囲の印の変化を子孫の範囲の印へ伝える。
    scope_children: BTreeMap<&'s str, BTreeSet<&'s str>>,
}

fn dependents<'s>(old: &'s Snapshot, new: &'s Snapshot) -> Dependents<'s> {
    let mut declarations: BTreeMap<&'s str, Vec<&'s Declaration>> = BTreeMap::new();
    for declaration in old
        .declarations
        .values()
        .chain(new.declarations.values())
        .flatten()
    {
        for name in &declaration.references {
            declarations.entry(name).or_default().push(declaration);
        }
    }
    let mut scope_children: BTreeMap<&'s str, BTreeSet<&'s str>> = BTreeMap::new();
    for (parent, children) in old.scope_children.iter().chain(&new.scope_children) {
        scope_children
            .entry(parent)
            .or_default()
            .extend(children.iter().map(String::as_str));
    }
    Dependents {
        declarations,
        scope_children,
    }
}

/// 変更名から逆向きに伝播し、逆順の長い依存連鎖でも全宣言を繰り返し走査しない。変化に入れた宣言ごとに
/// `visit` を呼ぶ。
fn propagate(
    dependents: &Dependents<'_>,
    changes: &mut ChangedNames,
    pending: &mut VecDeque<(String, Vec<AvailabilityAttr>)>,
    mut visit: impl FnMut(&Declaration),
) {
    while let Some((name, branch)) = pending.pop_front() {
        // module の範囲の印の変化は、子の module の範囲にも及ぶ (Issue #381)。
        for child in dependents
            .scope_children
            .get(name.as_str())
            .into_iter()
            .flatten()
        {
            if remember(changes, child, &branch) {
                pending.push_back(((*child).to_owned(), branch.clone()));
            }
        }
        if let Some(declarations) = dependents.declarations.get(name.as_str()) {
            for declaration in declarations {
                let combined: Vec<_> = declaration
                    .conditions
                    .iter()
                    .chain(&branch)
                    .cloned()
                    .collect();
                if !super::rust_availability::unsatisfiable(&combined) {
                    visit(declaration);
                    remember_declaration(changes, pending, declaration);
                }
            }
        }
    }
}

/// 範囲の glob の変化 (変化 `changes` の範囲の印) だけから伝播した変化と、そのうち意味の変わった項目 (`mod` で
/// ない宣言の、印でない名前) (Issue #381)。範囲の glob と関係の無い変更が外へ効き得る構成に届いても、package
/// 全体には戻さない。
fn glob_changes(
    dependents: &Dependents<'_>,
    changes: &ChangedNames,
) -> (ChangedNames, ChangedNames) {
    let mut glob = ChangedNames::new();
    let mut items = ChangedNames::new();
    let mut pending = VecDeque::new();
    for (name, branches) in changes
        .iter()
        .filter(|(name, _)| name.starts_with(GLOB_SCOPE) || name.as_str() == ANY_GLOB_SCOPE)
    {
        for branch in branches {
            if remember(&mut glob, name, branch) {
                pending.push_back((name.clone(), branch.clone()));
            }
        }
    }
    propagate(dependents, &mut glob, &mut pending, |declaration| {
        if declaration.module {
            return;
        }
        for name in std::iter::once(&declaration.name).chain(&declaration.bound_names) {
            if !name.starts_with('@') {
                remember(&mut items, name, &declaration.conditions);
            }
        }
    });
    (glob, items)
}

#[cfg(test)]
fn changed_names(old: &Snapshot, new: &Snapshot) -> ChangedNames {
    changed_names_with(old, new, &dependents(old, new))
}

fn changed_names_with(old: &Snapshot, new: &Snapshot, dependents: &Dependents<'_>) -> ChangedNames {
    let mut changes = ChangedNames::new();
    let mut pending = VecDeque::new();
    let keys: BTreeSet<_> = old
        .declarations
        .keys()
        .chain(new.declarations.keys())
        .collect();
    for key in keys {
        let fingerprint = |snapshot: &Snapshot| {
            let mut hashes: Vec<_> = snapshot
                .declarations
                .get(key)
                .into_iter()
                .flatten()
                .map(|declaration| *declaration.fingerprint.as_bytes())
                .collect();
            hashes.sort();
            hashes
        };
        let before = fingerprint(old);
        let after = fingerprint(new);
        if before != after {
            for declaration in old
                .declarations
                .get(key)
                .into_iter()
                .flatten()
                .chain(new.declarations.get(key).into_iter().flatten())
            {
                let hash = declaration.fingerprint.as_bytes();
                if before.iter().filter(|known| *known == hash).count()
                    != after.iter().filter(|known| *known == hash).count()
                {
                    remember_declaration(&mut changes, &mut pending, declaration);
                }
            }
        }
    }
    propagate(dependents, &mut changes, &mut pending, |_| {});
    changes
}

fn incomplete(state: &mut DetectionState<'_>, path: Option<&str>, reason: &str) {
    state.buckets.parse_truncations.push(TruncationInfo::new(path.map(str::to_owned), TruncationReason::RustDependentsIncomplete,
        format!("Rust opaque return bodies and their declaration dependencies were not all compared: {reason}")));
}

/// 隠れた型の変化は、呼び出しが差分内で追随していても外部の auto trait 契約の変更として残す。
pub(super) fn record(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    moved: &[MovedSymbol],
) {
    let packages: BTreeSet<_> = inputs
        .diff_files
        .iter()
        .zip(inputs.prepared)
        .filter(|(_, prepared)| !matches!(prepared, PreparedDiffFile::Skip))
        .map(|(diff, _)| diff)
        .filter(|df| df.old_path.ends_with(".rs") || df.new_path.ends_with(".rs"))
        .flat_map(|df| [&df.old_path, &df.new_path])
        .filter(|path| path.ends_with(".rs") && crate::engine::impact::is_safe_diff_path(path))
        .filter(|path| {
            !is_binary_only_rust_crate(inputs.dir, path)
                || !is_binary_only_rust_crate_at_base(inputs.base_blobs, path)
        })
        .filter_map(|path| super::rust_dependencies::crates::crate_package(inputs.dir, path))
        .collect();
    if packages.is_empty() {
        return;
    }
    let Some(mut paths) = super::prepare::tracked_files(inputs.dir) else {
        incomplete(state, None, "the tracked source paths could not be listed");
        return;
    };
    paths.extend(
        inputs
            .diff_files
            .iter()
            .flat_map(|df| [df.old_path.clone(), df.new_path.clone()]),
    );
    paths.sort();
    paths.dedup();
    let canonical = std::fs::canonicalize(inputs.dir).ok();
    let resolver = canonical
        .as_deref()
        .map(crate::engine::gitattributes::AttributeResolver::open);
    let mut lookup = resolver.as_ref().map(|resolver| resolver.lookup());
    let mut undetermined = Vec::new();
    let mut snapshots: BTreeMap<String, (Snapshot, Snapshot)> = BTreeMap::new();
    // package ごとの旧新の lib crate (開ける package だけ)。
    let mut libraries: BTreeMap<String, &LibraryVersions<'_>> = BTreeMap::new();
    // package ごとのテストの module の判定 (旧新の lib crate を開ける package だけ。Issue #382)。
    let mut test_scopes: BTreeMap<String, TestScopes<'_, '_>> = BTreeMap::new();
    let mut files = 0usize;
    let mut bytes = 0usize;
    for path in paths {
        if !path.ends_with(".rs") || !crate::engine::impact::is_safe_diff_path(&path) {
            continue;
        }
        let Some(package) = super::rust_dependencies::crates::crate_package(inputs.dir, &path)
            .filter(|package| packages.contains(package))
        else {
            continue;
        };
        // 旧新の版でファイルを置いた module (glob の範囲と宣言の位置。Issue #381)。一意に決まれば、
        // テストの module の宣言を比べない (Issue #382)。
        let library = inputs.rust_crates.library_versions(&path);
        if let Some(library) = library {
            libraries.entry(package.clone()).or_insert(library);
        }
        let scopes = library.map(|library| {
            &*test_scopes
                .entry(package.clone())
                .or_insert_with(|| TestScopes::new(&library.old, &library.new))
        });
        let contexts = match library {
            Some(library) => [
                FilePlacement::of(scopes, &library.old, &path),
                FilePlacement::of(scopes, &library.new, &path),
            ],
            None => [FilePlacement::unknown(), FilePlacement::unknown()],
        };
        let pair = snapshots.entry(package.clone()).or_default();
        // テストの module に置いたファイルは、テストのパスや生成物で飛ばしても読んだことにする (lib crate の
        // ファイルを全て読めたかの確かめ。Issue #381)。テストのパスのファイルは比べない。
        for (snapshot, file) in [&mut pair.0, &mut pair.1].into_iter().zip(&contexts) {
            if file.excludes(&[]) {
                snapshot.added.insert(path.clone());
            }
        }
        if is_test_path(std::path::Path::new(&path)) {
            continue;
        }
        let exists = std::path::Path::new(inputs.dir).join(&path).is_file();
        let df = crate::models::impact::DiffFile {
            old_path: path.clone(),
            new_path: if exists {
                path.clone()
            } else {
                "/dev/null".to_owned()
            },
            hunks: Vec::new(),
            deleted_old_source: None,
        };
        if should_skip_diff_file(
            &df,
            lookup.as_mut(),
            canonical.as_deref(),
            &mut undetermined,
        ) {
            continue;
        }
        files += 1;
        if files > MAX_FILES {
            incomplete(state, Some(&package), "more than 10000 source files");
            break;
        }
        let old = inputs.base_blobs.read(&path);
        let new = exists.then(|| load_new_source(inputs.dir, &path)).flatten();
        bytes += old.as_ref().map_or(0, Vec::len) + new.as_ref().map_or(0, |source| source.len());
        if bytes > MAX_BYTES {
            incomplete(state, Some(&package), "more than 64 MiB of source text");
            break;
        }
        for ((snapshot, source), file) in
            [(&mut pair.0, old.as_deref()), (&mut pair.1, new.as_deref())]
                .into_iter()
                .zip(&contexts)
        {
            let Some(source) = source else {
                continue;
            };
            if source.len() > MAX_FILE_BYTES
                || snapshot
                    .add(&path, source, inputs.strip_internal, file)
                    .is_none()
            {
                incomplete(
                    state,
                    Some(&path),
                    "a source exceeds 2 MiB or could not be parsed",
                );
            }
        }
        if exists && new.is_none() {
            incomplete(state, Some(&path), "a current source could not be read");
        }
    }
    if let Some(root) = canonical.as_deref() {
        state.buckets.parse_truncations.extend(
            crate::engine::refs::generated_undetermined_truncations(root, &undetermined),
        );
    }
    // module の範囲の glob は、旧新の版の置き場で範囲を確かめ切れなければ package 全体に戻す (Issue #381)。
    for (package, (old, new)) in &mut snapshots {
        confine(old, new, libraries.get(package).copied());
    }
    for (old, new) in snapshots.values() {
        let dependents = dependents(old, new);
        let mut changes = changed_names_with(old, new, &dependents);
        // module の範囲の glob の変化が、宣言を経ずに外へ効き得る構成に届けば、package 全体の変化にする
        // (Issue #381)。届くかは範囲の印から伝播した変化だけで決める。範囲の glob の変化が無ければ、今までどおり
        // 宣言の名前だけで比べる。
        if changes.keys().any(|name| name.starts_with(GLOB_SCOPE)) {
            let (glob, items) = glob_changes(&dependents, &changes);
            if old.transmits(&glob, &items) || new.transmits(&glob, &items) {
                remember(&mut changes, UNKNOWN_IMPORTS, &[]);
            }
        }
        for (key, current) in &new.declarations {
            let after = public_functions(current);
            let Some(&new_function) = after.first() else {
                continue;
            };
            let previous = previous_declarations(old, key, new_function, moved);
            if previous.is_none()
                && !moved
                    .iter()
                    .any(|movement| matching_move(movement, &key.0, new_function))
            {
                continue;
            }
            let before =
                previous.map_or_else(Vec::new, |(_, declarations)| public_functions(declarations));
            let old_function = before.first().copied();
            let body_changed = before.is_empty() || bodies_changed(&before, &after);
            let dependency_changed = current
                .iter()
                .chain(
                    previous
                        .into_iter()
                        .flat_map(|(_, declarations)| declarations),
                )
                .any(|declaration| {
                    declaration.public.as_ref().is_some_and(|function| {
                        affected(&function.references, &declaration.conditions, &changes)
                    })
                });
            if !body_changed && !dependency_changed {
                continue;
            }
            let (path, _, _, _) = key;
            if rust_declaration_outside_public_api_surface(
                inputs.rust_crates,
                state.rust_public,
                inputs.dir,
                inputs.base_blobs,
                false,
                path,
                &new_function.name,
                Some(&new_function.context),
            ) && old_function.is_some_and(|old_function| {
                rust_declaration_outside_public_api_surface(
                    inputs.rust_crates,
                    state.rust_public,
                    inputs.dir,
                    inputs.base_blobs,
                    true,
                    previous.map_or(path.as_str(), |(path, _)| path),
                    &old_function.name,
                    Some(&old_function.context),
                )
            }) {
                continue;
            }
            let inline_module = super::rust_declarations::inline_module_of(&new_function.context);
            let same = |change: &ApiSymbolChange| {
                change.file == *path
                    && change.name == new_function.name
                    && change.inline_module == inline_module
            };
            if state.buckets.modified.iter().any(same) {
                continue;
            }
            state
                .buckets
                .modified_closed_in_diff
                .retain(|change| !same(change));
            state.buckets.compatible_modified.retain(|change| {
                !(change.file == *path
                    && change.name == new_function.name
                    && change.inline_module == inline_module)
            });
            state.buckets.modified.push(ApiSymbolChange {
                inline_module,
                unverified: Some(ApiModificationUnverified {
                    reason: ApiModificationUnverifiedReason::RustSignatureDependencyUnresolved,
                    first_unproven: None,
                }),
                ..ApiSymbolChange::new(
                    new_function.name.clone(),
                    new_function.kind.clone(),
                    path.clone(),
                    old_function.map(|function| function.signature.clone()),
                    Some(new_function.signature.clone()),
                )
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::tests::common::*;

    const CARGO: &str = "[package]\nname = \"opaque_demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[features]\nwide = []\n";

    fn changes(old: &[(&str, &str)], new: &[(&str, &str)]) -> ApiChanges {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let files: Vec<_> = std::iter::once(("Cargo.toml", CARGO))
            .chain(old.iter().copied())
            .collect();
        git_commit_files(repo, &files, "base");
        for (path, source) in new {
            let path = repo.join(path);
            // 新しいファイルは、無いディレクトリにも置く (`src/other/tests.rs` など)
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        detect_api_changes_from_worktree(repo)
    }

    fn modified(api: &ApiChanges, name: &str) -> bool {
        api.modified.iter().any(|change| change.name == name)
    }

    #[test]
    fn opaque_returns_report_direct_body_and_private_alias_changes() {
        let before = "type Hidden = ();\npub fn indirect() -> impl Copy { Hidden::default() }\npub fn direct() -> impl Copy { 0u8 }\npub fn ordinary() -> u8 { 0 }\n";
        let after = "type Hidden = core::marker::PhantomData<std::rc::Rc<()>>;\npub fn indirect() -> impl Copy { Hidden::default() }\npub fn direct() -> impl Copy { core::marker::PhantomData::<std::rc::Rc<()>> }\npub fn ordinary() -> u8 { 1 }\n";
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", after)]);
        assert!(modified(&api, "indirect"), "{api:?}");
        assert!(modified(&api, "direct"), "{api:?}");
        assert!(!modified(&api, "ordinary"), "{api:?}");
        assert!(
            api.modified_closed_in_diff
                .iter()
                .all(|change| !matches!(change.name.as_str(), "indirect" | "direct")),
            "{api:?}"
        );
    }

    #[test]
    fn opaque_returns_follow_other_file_helpers_and_async_bodies() {
        let root = "mod hidden;\npub fn produce() -> impl Copy { hidden::make() }\npub async fn future() { hidden::wait().await; }\npub fn ordinary() -> u8 { hidden::plain() }\n";
        let before = "type Hidden = ();\npub fn make() -> Hidden { Hidden::default() }\npub async fn wait() { let held = (); std::future::ready(()).await; drop(held); }\npub fn plain() -> u8 { 0 }\n";
        let after = "type Hidden = core::marker::PhantomData<std::rc::Rc<()>>;\npub fn make() -> Hidden { Hidden::default() }\npub async fn wait() { let held = std::rc::Rc::new(()); std::future::ready(()).await; drop(held); }\npub fn plain() -> u8 { 1 }\n";
        let api = changes(
            &[("src/lib.rs", root), ("src/hidden.rs", before)],
            &[("src/hidden.rs", after)],
        );
        assert!(modified(&api, "produce"), "{api:?}");
        assert!(modified(&api, "future"), "{api:?}");
        assert!(!modified(&api, "ordinary"), "{api:?}");
        assert!(!modified(&api, "make"), "{api:?}");
        for (imported, name) in [
            ("Hidden", "crate::Hidden"),
            ("Hidden as Selected", "crate::Selected"),
            ("*", "Hidden"),
        ] {
            let root = format!(
                "mod a {{ pub type Hidden = (); }}\nmod b {{ pub type Hidden = core::marker::PhantomData<std::rc::Rc<()>>; }}\nuse a::{imported};\npub mod api;\n"
            );
            let api_source = format!(
                "{}pub fn produce() -> impl Copy {{ {name}::default() }}\n",
                if imported == "*" {
                    "use super::*;\n"
                } else {
                    ""
                },
            );
            let changed = root.replace("use a::", "use b::");
            let api = changes(
                &[("src/lib.rs", &root), ("src/api.rs", &api_source)],
                &[("src/lib.rs", &changed)],
            );
            assert!(modified(&api, "produce"), "{imported}: {api:?}");
        }
        let root = "mod a { pub type Other = (); }\nmod b { pub type Other = u8; }\nuse a::Other;\npub mod api;\n";
        let changed = root.replace("use a::", "use b::");
        let api = changes(
            &[
                ("src/lib.rs", root),
                (
                    "src/api.rs",
                    "type Hidden = ();\npub fn produce() -> impl Copy { Hidden::default() }\n",
                ),
            ],
            &[("src/lib.rs", &changed)],
        );
        assert!(!modified(&api, "produce"), "{api:?}");
    }

    #[test]
    fn opaque_returns_propagate_reverse_order_dependency_chains() {
        let mut source = String::new();
        for index in 0..512 {
            source.push_str(&format!("type A{index:04} = A{:04};\n", index + 1));
        }
        source.push_str("type A0512 = ();\ntype Unrelated = ();\npub fn produce() -> impl Copy { A0000::default() }\n");
        let after = source.replace("type A0512 = ();", "type A0512 = u8;");
        let strip = ts_strip_internal::StripInternal::disabled();
        let mut old = Snapshot::default();
        let mut new = Snapshot::default();
        old.add(
            "src/lib.rs",
            source.as_bytes(),
            &strip,
            &FilePlacement::unknown(),
        )
        .unwrap();
        new.add(
            "src/lib.rs",
            after.as_bytes(),
            &strip,
            &FilePlacement::unknown(),
        )
        .unwrap();
        let changed = changed_names(&old, &new);
        assert!(changed.contains_key("A0000"));
        assert!(changed.contains_key("produce"));
        assert!(!changed.contains_key("Unrelated"));
        assert!(changed_names(&old, &old).is_empty());
    }

    /// Issue #381: 宣言は自分の module の範囲の印だけを参照し (入れ子の深い module で、祖先の全ての印を宣言ごとに
    /// 持たない)、外側の module の範囲の glob の変化は、範囲の印の子へのつながりで内側の宣言に届く。
    #[test]
    fn opaque_returns_mark_only_their_own_module_scope() {
        let depth = 64;
        let mut source = String::from("use crate::a::*;\n");
        for index in 0..depth {
            source.push_str(&format!(
                "pub mod m{index:04} {{\npub fn f{index:04}() -> impl Copy {{ 0u8 }}\n"
            ));
        }
        source.push_str(&"}\n".repeat(depth));
        let after = source.replace("use crate::a::*;", "use crate::b::*;");
        let strip = ts_strip_internal::StripInternal::disabled();
        let placement = FilePlacement {
            modules: Some(vec![Vec::new()]),
            tests: None,
        };
        let mut old = Snapshot::default();
        let mut new = Snapshot::default();
        old.add("src/lib.rs", source.as_bytes(), &strip, &placement)
            .unwrap();
        new.add("src/lib.rs", after.as_bytes(), &strip, &placement)
            .unwrap();
        for declaration in new.declarations.values().flatten() {
            let markers = declaration
                .references
                .iter()
                .filter(|name| name.starts_with(GLOB_SCOPE))
                .count();
            assert_eq!(markers, 1, "{}", declaration.name);
        }
        let changed = changed_names(&old, &new);
        let innermost = format!("f{:04}", depth - 1);
        let function = new
            .declarations
            .values()
            .flatten()
            .find(|declaration| declaration.name == innermost)
            .and_then(|declaration| declaration.public.as_ref())
            .unwrap();
        assert!(affected(&function.references, &[], &changed));
        assert!(changed_names(&old, &old).is_empty());
    }

    #[test]
    fn opaque_returns_keep_deep_private_module_dependencies_local() {
        let modules: Vec<_> = (0..512).map(|index| format!("m{index:04}")).collect();
        let mut source = String::new();
        for name in &modules {
            source.push_str(&format!("mod {name} {{\n"));
        }
        source.push_str("type Hidden = ();\npub fn produce() -> impl Copy { Hidden::default() }\n");
        source.push_str(&"}\n".repeat(modules.len()));
        let after = source.replace("type Hidden = ();", "type Hidden = u8;");
        let strip = ts_strip_internal::StripInternal::disabled();
        let mut old = Snapshot::default();
        let mut new = Snapshot::default();
        old.add(
            "src/lib.rs",
            source.as_bytes(),
            &strip,
            &FilePlacement::unknown(),
        )
        .unwrap();
        new.add(
            "src/lib.rs",
            after.as_bytes(),
            &strip,
            &FilePlacement::unknown(),
        )
        .unwrap();
        let changed = changed_names(&old, &new);
        assert!(changed.contains_key("produce"));
        assert!(!changed.contains_key("m0000"));
        let function = new
            .declarations
            .values()
            .flatten()
            .find_map(|declaration| declaration.public.as_ref())
            .unwrap();
        assert!(affected(&function.references, &[], &changed));
        assert_eq!(
            super::super::rust_declarations::inline_module_of(&function.context).as_deref(),
            Some(modules.join("::").as_str()),
        );
        assert!(changed_names(&old, &old).is_empty());
    }

    #[test]
    fn opaque_returns_keep_format_comments_unrelated_and_inactive_changes_outside() {
        let before = "type Hidden = ();\nfn helper() -> Hidden { Hidden::default() }\nfn unrelated() -> u8 { 0 }\npub fn produce() -> impl Copy { helper() }\npub fn ordinary() -> u8 { 0 }\n";
        let after = "type Hidden = /* same */ ( );\nfn helper() -> Hidden { /* same */ Hidden :: default ( ) }\nfn unrelated() -> u8 { 1 }\npub fn produce() -> impl Copy {\n helper() // same\n}\npub fn ordinary() -> u8 { 1 }\n";
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", after)]);
        assert!(api.modified.is_empty(), "{api:?}");
        let before = "#[cfg(feature = \"wide\")]\ntype Hidden = ();\n#[cfg(not(feature = \"wide\"))]\ntype Hidden = u8;\n#[cfg(feature = \"wide\")]\npub fn produce() -> impl Copy { Hidden::default() }\n";
        let after = before.replace("type Hidden = u8;", "type Hidden = u16;");
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", &after)]);
        assert!(!modified(&api, "produce"), "{api:?}");
        for header in [
            "#[cfg(feature = \"wide\")]\npub mod nested {\n",
            "pub mod nested {\n#![cfg(feature = \"wide\")]\n",
        ] {
            let before = format!(
                "{header}#[cfg(feature = \"wide\")]\ntype Hidden = ();\n#[cfg(not(feature = \"wide\"))]\ntype Hidden = u8;\npub fn produce() -> impl Copy {{ Hidden::default() }}\n}}\n"
            );
            let after = before.replace("type Hidden = u8;", "type Hidden = u16;");
            let api = changes(&[("src/lib.rs", &before)], &[("src/lib.rs", &after)]);
            assert!(!modified(&api, "produce"), "{header}: {api:?}");
            let after = before.replace("type Hidden = ();", "type Hidden = u32;");
            let api = changes(&[("src/lib.rs", &before)], &[("src/lib.rs", &after)]);
            assert!(modified(&api, "produce"), "{header}: {api:?}");
        }
        let before = "#[cfg(feature = \"wide\")]\npub fn produce() -> impl Copy { 0u8 }\n";
        let after = before.replace("cfg(feature = \"wide\")", "cfg(all(feature = \"wide\"))");
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", &after)]);
        assert!(!modified(&api, "produce"), "{api:?}");
    }

    /// Issue #382 のテストの root。不透明な戻り値の公開の関数 ([`TEST_MODULE_OPAQUE`]) と、テストの module
    /// を置く `other`。
    const TEST_MODULE_ROOT: &str = "pub mod other;\npub fn opaque() -> impl Copy { 0u8 }\npub async fn future() {}\npub fn items(values: &[u8]) -> impl Iterator<Item = &u8> { values.iter() }\npub struct Client;\nimpl Client {\n    pub async fn get(&self) -> u8 { 0 }\n}\n";
    const TEST_MODULE_OTHER: &str = "pub fn helper() -> u8 { 1 }\n";
    /// テストの helper (テストでない build にもある) とテストの関数を持つ、glob で取り込む module のファイル。
    const GLOB_TESTS: &str = "use super::*;\n\npub(crate) fn shown() -> String {\n    format!(\"{}\", helper())\n}\n\n#[test]\nfn helper_is_one() {\n    assert_eq!(helper(), 1);\n}\n";
    const TEST_MODULE_OPAQUE: [&str; 4] = ["opaque", "future", "items", "Client.get"];

    /// `src/other.rs` の末尾に `extra` を足したソース。
    fn other_with(extra: &str) -> String {
        format!("{TEST_MODULE_OTHER}{extra}")
    }

    /// `old` をコミットし、`new` に書き換えたとき、[`TEST_MODULE_OPAQUE`] のうち modified に出たものと、
    /// 解析の打ち切り。
    fn opaque_reported(
        old: &[(&str, &str)],
        new: &[(&str, String)],
    ) -> (Vec<&'static str>, Vec<TruncationInfo>) {
        let new: Vec<_> = new
            .iter()
            .map(|(path, source)| (*path, source.as_str()))
            .collect();
        let api = changes(old, &new);
        let reported = TEST_MODULE_OPAQUE
            .into_iter()
            .filter(|name| modified(&api, name))
            .collect();
        (reported, api.parse_truncations)
    }

    /// テストでない build の `Hidden = outside` を返す `produce` と、テストの module の中の同じ名前の型
    /// `Hidden = inside` を持つ root。
    fn hidden_root(outside: &str, inside: &str) -> String {
        format!(
            "{TEST_MODULE_ROOT}type Hidden = {outside};\npub fn produce() -> impl Copy {{ Hidden::default() }}\n#[cfg(test)]\nmod tests {{\n    type Hidden = {inside};\n}}\n"
        )
    }

    /// Issue #382: テストの module (旧新の両方で `#[cfg(test)]` の付いた宣言の module か、その中) の変化は、
    /// テストでない build の公開の契約に関わらないので、依存の変化の起点にも伝播の中継にもしない
    /// (`rust_dependencies/dependents.rs` と同じ約束)。
    #[test]
    fn opaque_returns_ignore_test_module_changes() {
        let base = [
            ("src/lib.rs", TEST_MODULE_ROOT),
            ("src/other.rs", TEST_MODULE_OTHER),
        ];
        let existing = [
            ("src/lib.rs", TEST_MODULE_ROOT.to_owned()),
            ("src/other.rs", other_with("#[cfg(test)]\nmod tests;\n")),
            (
                "src/other/tests.rs",
                GLOB_TESTS.replace("use super::*;", "use super::helper;"),
            ),
        ];
        let existing: Vec<_> = existing
            .iter()
            .map(|(path, source)| (*path, source.as_str()))
            .collect();
        for (case, old, new) in [
            // 誤検出の再現: 別ファイルのテストの module を、glob の import ごと足す
            (
                "test module file with a glob",
                &base[..],
                vec![
                    ("src/other.rs", other_with("#[cfg(test)]\nmod tests;\n")),
                    ("src/other/tests.rs", GLOB_TESTS.to_owned()),
                ],
            ),
            // インラインのテストの module
            (
                "inline test module with a glob",
                &base[..],
                vec![(
                    "src/other.rs",
                    other_with(
                        "#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn t() {\n        assert_eq!(helper(), 1);\n    }\n}\n",
                    ),
                )],
            ),
            // 既にあるテストの module の import を glob に変える
            (
                "existing test module gains a glob",
                &existing[..],
                vec![("src/other/tests.rs", GLOB_TESTS.to_owned())],
            ),
            // `#[path]` で置いたテストの module (ファイルの名前によらない)
            (
                "test module placed with #[path]",
                &base[..],
                vec![
                    (
                        "src/lib.rs",
                        format!(
                            "{TEST_MODULE_ROOT}#[cfg(test)]\n#[path = \"testing/support.rs\"]\nmod tests;\n"
                        ),
                    ),
                    ("src/testing/support.rs", "use super::*;\n".to_owned()),
                ],
            ),
        ] {
            let (reported, truncations) = opaque_reported(old, &new);
            assert!(reported.is_empty(), "{case}: {reported:?}");
            assert!(truncations.is_empty(), "{case}: {truncations:?}");
        }
        // テストの module の中の同じ名前の型の変化を、テストでない build の宣言へ伝えない
        let api = changes(
            &[
                ("src/lib.rs", &hidden_root("()", "()")),
                ("src/other.rs", TEST_MODULE_OTHER),
            ],
            &[("src/lib.rs", &hidden_root("()", "std::rc::Rc<()>"))],
        );
        assert!(!modified(&api, "produce"), "{api:?}");
    }

    /// Issue #382 の対照: テストでない module と、置き場を一意に決められないファイルは、今までどおり比べる。
    #[test]
    fn opaque_returns_keep_comparing_non_test_and_ambiguous_modules() {
        let base = [
            ("src/lib.rs", TEST_MODULE_ROOT),
            ("src/other.rs", TEST_MODULE_OTHER),
        ];
        for (case, new) in [
            // `#[cfg(test)]` の無い module は、テストの module ではない
            (
                "module without cfg(test)",
                vec![
                    ("src/other.rs", other_with("mod tests;\n")),
                    ("src/other/tests.rs", GLOB_TESTS.to_owned()),
                ],
            ),
            // `cfg_attr` の中の `cfg(test)` は、テストでない build にも module を残し得る (Issue #380)
            (
                "cfg(test) inside cfg_attr",
                vec![
                    (
                        "src/other.rs",
                        other_with("#[cfg_attr(feature = \"wide\", cfg(test))]\nmod tests;\n"),
                    ),
                    ("src/other/tests.rs", GLOB_TESTS.to_owned()),
                ],
            ),
            // 同じファイルをテストの module とテストでない module の両方に置く
            (
                "file placed in a test module and another module",
                vec![
                    (
                        "src/lib.rs",
                        format!(
                            "{TEST_MODULE_ROOT}#[cfg(test)]\nmod tests;\n#[path = \"tests.rs\"]\nmod also;\n"
                        ),
                    ),
                    ("src/tests.rs", "use super::*;\n".to_owned()),
                ],
            ),
        ] {
            let (reported, _) = opaque_reported(&base, &new);
            assert_eq!(reported, TEST_MODULE_OPAQUE, "{case}");
        }
        // テストでない build の同じ名前の型を変えれば、今までどおり変化
        let api = changes(
            &[
                ("src/lib.rs", &hidden_root("()", "()")),
                ("src/other.rs", TEST_MODULE_OTHER),
            ],
            &[("src/lib.rs", &hidden_root("std::rc::Rc<()>", "()"))],
        );
        assert!(modified(&api, "produce"), "{api:?}");
    }

    /// Issue #381: 可視性の修飾の無い glob の変化は、置いた module と子孫の宣言だけを起点にする。範囲に名前を
    /// 経ずに外へ効く構成が無ければ、範囲の宣言を名前で使わない不透明な戻り値の公開の関数を modified にしない。
    #[test]
    fn opaque_returns_scope_private_globs_to_their_modules() {
        let base = [
            ("src/lib.rs", TEST_MODULE_ROOT),
            ("src/other.rs", TEST_MODULE_OTHER),
        ];
        let helper = "mod inner {\n    use super::helper;\n    pub(crate) fn twice() -> u8 {\n        helper() + helper()\n    }\n}\n";
        let existing = [
            ("src/lib.rs", TEST_MODULE_ROOT.to_owned()),
            ("src/other.rs", other_with(helper)),
        ];
        let existing: Vec<_> = existing
            .iter()
            .map(|(path, source)| (*path, source.as_str()))
            .collect();
        for (case, old, new) in [
            // Issue の再現: inline の module に glob だけを足す
            (
                "inline module with only a glob",
                &base[..],
                vec![(
                    "src/other.rs",
                    other_with("mod inner {\n    #[allow(unused_imports)]\n    use super::*;\n}\n"),
                )],
            ),
            // glob だけの子のファイルを足す
            (
                "child file with only a glob",
                &base[..],
                vec![
                    ("src/other.rs", other_with("pub mod extra;\n")),
                    ("src/other/extra.rs", "use super::*;\n".to_owned()),
                ],
            ),
            // 範囲の中の helper の import を glob に変える (helper を使う公開の関数は無い)
            (
                "existing import becomes a glob",
                &existing[..],
                vec![(
                    "src/other.rs",
                    other_with(&helper.replace("use super::helper;", "use super::*;")),
                )],
            ),
            // `pub(self)` は修飾の無い glob と同じ範囲
            (
                "pub(self) glob",
                &base[..],
                vec![(
                    "src/other.rs",
                    other_with(
                        "mod inner {\n    #[allow(unused_imports)]\n    pub(self) use super::*;\n}\n",
                    ),
                )],
            ),
        ] {
            let (reported, truncations) = opaque_reported(old, &new);
            assert!(reported.is_empty(), "{case}: {reported:?}");
            assert!(truncations.is_empty(), "{case}: {truncations:?}");
        }
        // 範囲の中のテストの module (Issue #382) の impl・derive・マクロは、外へ効き得る構成に数えない
        let tests = "#[cfg(test)]\nmod tests {\n    use super::*;\n    #[derive(Clone)]\n    struct Local;\n    impl Local {}\n    #[test]\n    fn t() {\n        assert_eq!(helper(), 1);\n    }\n}\n";
        let old = [
            ("src/lib.rs", TEST_MODULE_ROOT.to_owned()),
            ("src/other.rs", other_with(tests)),
        ];
        let old: Vec<_> = old
            .iter()
            .map(|(path, source)| (*path, source.as_str()))
            .collect();
        let (reported, truncations) = opaque_reported(
            &old,
            &[(
                "src/other.rs",
                format!(
                    "#[allow(unused_imports)]\nuse super::*;\n{}",
                    other_with(tests)
                ),
            )],
        );
        assert!(reported.is_empty(), "{reported:?}");
        assert!(truncations.is_empty(), "{truncations:?}");
        let inner = other_with("mod inner {\n    #[allow(unused_imports)]\n    use super::*;\n}\n");
        // 範囲の glob で意味の変わる宣言が無ければ、展開の見えない構成 (crate の外の derive・マクロ) にも届かない
        let root = format!(
            "{TEST_MODULE_ROOT}#[derive(serde::Serialize)]\npub struct Wire;\npub fn wire() -> u8 {{\n    helper::value!()\n}}\n"
        );
        let (reported, truncations) = opaque_reported(
            &[("src/lib.rs", &root), ("src/other.rs", TEST_MODULE_OTHER)],
            &[("src/other.rs", inner.clone())],
        );
        assert!(reported.is_empty(), "{reported:?}");
        assert!(truncations.is_empty(), "{truncations:?}");
        // 範囲の glob と関係の無い変更が trait の impl に届いても、package 全体に戻さない
        let root = format!(
            "{TEST_MODULE_ROOT}pub struct Other(pub u8);\npub trait Marker {{}}\nimpl Marker for Other {{}}\npub fn other() -> impl Copy {{\n    Other(0).0\n}}\n"
        );
        let api = changes(
            &[("src/lib.rs", &root), ("src/other.rs", TEST_MODULE_OTHER)],
            &[
                (
                    "src/lib.rs",
                    &root.replace("Other(pub u8)", "Other(pub u16)"),
                ),
                ("src/other.rs", &inner),
            ],
        );
        assert!(modified(&api, "other"), "{api:?}");
        let reported: Vec<_> = TEST_MODULE_OPAQUE
            .into_iter()
            .filter(|name| modified(&api, name))
            .collect();
        assert!(reported.is_empty(), "{reported:?}");
        // どの設定にも無い `mod` に付いた属性マクロは、同じ名前の有効な module の範囲を広げない
        let twins = |glob: &str| {
            format!(
                "{TEST_MODULE_ROOT}#[cfg(any())]\n#[unavailable::attr]\nmod twin {{}}\nmod twin {{\n{glob}}}\n"
            )
        };
        let (reported, truncations) = opaque_reported(
            &[
                ("src/lib.rs", &twins("")),
                ("src/other.rs", TEST_MODULE_OTHER),
            ],
            &[(
                "src/lib.rs",
                twins("    #[allow(unused_imports)]\n    use super::*;\n"),
            )],
        );
        assert!(reported.is_empty(), "{reported:?}");
        assert!(truncations.is_empty(), "{truncations:?}");
        // 生成物のテストの module のファイルも、テストの module として読んだことにする
        let root =
            format!("{TEST_MODULE_ROOT}#[cfg(test)]\n#[path = \"support.rs\"]\nmod tests;\n");
        let (reported, truncations) = opaque_reported(
            &[
                ("src/lib.rs", &root),
                ("src/other.rs", TEST_MODULE_OTHER),
                ("src/support.rs", "use super::*;\n#[test]\nfn t() {}\n"),
                (".gitattributes", "src/support.rs linguist-generated\n"),
            ],
            &[("src/other.rs", inner)],
        );
        assert!(reported.is_empty(), "{reported:?}");
        assert!(truncations.is_empty(), "{truncations:?}");
    }

    /// Issue #381 の対照の root。`m` の glob の先 (`a`・`b`) と、`m` の宣言を範囲の外で使う公開の関数
    /// (子の module の helper・impl の receiver・引数の型) と、使わない公開の関数 (`unrelated`)。
    const SCOPE_ROOT: &str = "pub mod a {\n    pub type Hidden = ();\n}\npub mod b {\n    pub type Hidden = core::marker::PhantomData<std::rc::Rc<()>>;\n}\npub mod m;\npub fn outer() -> impl Copy {\n    m::child::make()\n}\nimpl m::Client {\n    pub async fn get(&self) {}\n}\npub async fn take(client: m::Client) {\n    drop(client);\n}\npub fn unrelated() -> impl Copy {\n    0u8\n}\n";
    /// `m` の中身。glob で取り込む `Hidden` を、フィールドと子の module の helper に使う。
    const SCOPE_MODULE: &str = "use crate::a::*;\npub struct Client {\n    pub inner: Hidden,\n}\npub mod child {\n    pub fn make() -> super::Hidden {\n        Default::default()\n    }\n}\n";
    const SCOPE_OPAQUE: [&str; 4] = ["outer", "get", "take", "unrelated"];

    /// `src/lib.rs` を `root`、`src/m.rs` を `module` (と `extra`) でコミットし、`m.rs` の glob の先を `a` から
    /// `b` に変えたとき、[`SCOPE_OPAQUE`] のうち modified に出たもの。
    fn scope_reported(root: &str, module: &str, extra: &[(&str, &str)]) -> Vec<&'static str> {
        let old: Vec<_> = [("src/lib.rs", root), ("src/m.rs", module)]
            .into_iter()
            .chain(extra.iter().copied())
            .collect();
        let changed = module.replace("crate::a::*", "crate::b::*");
        assert_ne!(changed, module);
        let api = changes(&old, &[("src/m.rs", &changed)]);
        SCOPE_OPAQUE
            .into_iter()
            .filter(|name| modified(&api, name))
            .collect()
    }

    /// Issue #381 の対照: 範囲の中の宣言 (子の module の helper・フィールドの型) の変化は、名前で範囲の外へ
    /// 伝わる (impl の receiver・引数の型も依存に入る)。位置の分からないファイルの宣言は、どの範囲の glob の
    /// 変化でも起点に入る。
    #[test]
    fn opaque_returns_follow_private_glob_scopes_outward() {
        let precise = ["outer", "get", "take"];
        assert_eq!(scope_reported(SCOPE_ROOT, SCOPE_MODULE, &[]), precise);
        // 子の module のファイルを、範囲の外の module にも置く
        let root = format!("{SCOPE_ROOT}#[path = \"shared.rs\"]\npub mod elsewhere;\n");
        let module = SCOPE_MODULE.replace(
            "pub mod child {\n    pub fn make() -> super::Hidden {\n        Default::default()\n    }\n}\n",
            "#[path = \"shared.rs\"]\npub mod child;\n",
        );
        let shared = "pub fn make() -> super::Hidden {\n    Default::default()\n}\n";
        assert_eq!(
            scope_reported(&root, &module, &[("src/shared.rs", shared)]),
            precise
        );
        // 範囲の中の inherent impl の項目は宣言として範囲に入る (見出しの外の型の trait は変えない)
        let module = format!(
            "{SCOPE_MODULE}impl Client {{\n    pub fn new() -> Self {{\n        Client {{ inner: Default::default() }}\n    }}\n}}\n"
        );
        assert_eq!(scope_reported(SCOPE_ROOT, &module, &[]), precise);
        // 範囲の宣言の名前を参照しない trait の impl、組み込みの不活性な属性、関数の中のどの設定にも無い構成は
        // 範囲を広げない
        let root = format!(
            "{SCOPE_ROOT}pub struct Other;\nimpl Clone for Other {{\n    fn clone(&self) -> Self {{\n        Other\n    }}\n}}\n#[inline]\n#[allow(dead_code)]\nfn formatted() {{}}\nfn disabled() {{\n    #[cfg(any())]\n    {{\n        unavailable::install!();\n    }}\n}}\n"
        );
        assert_eq!(scope_reported(&root, SCOPE_MODULE, &[]), precise);
        // どの設定にも無い構成と、テストの module に付いた属性は数えない
        let module = format!(
            "{SCOPE_MODULE}#[cfg(any())]\nimpl Clone for Client {{\n    fn clone(&self) -> Self {{\n        Client {{ inner: Default::default() }}\n    }}\n}}\n#[cfg(test)]\n#[some_attribute]\nmod tests {{}}\n"
        );
        assert_eq!(scope_reported(SCOPE_ROOT, &module, &[]), precise);
        // glob を消す (旧い版の範囲の宣言が起点に入る)
        let explicit = SCOPE_MODULE.replace("use crate::a::*;", "use crate::a::Hidden;");
        let api = changes(
            &[("src/lib.rs", SCOPE_ROOT), ("src/m.rs", SCOPE_MODULE)],
            &[("src/m.rs", &explicit)],
        );
        let reported: Vec<_> = SCOPE_OPAQUE
            .into_iter()
            .filter(|name| modified(&api, name))
            .collect();
        assert_eq!(reported, precise);
    }

    /// Issue #381 の対照: 範囲の glob の変化が宣言を経ずに外へ効き得る構成 (範囲の中か、範囲の宣言の名前を
    /// 参照する範囲の外の trait の impl・マクロ・属性) に届くか、範囲を確かめ切れない glob は、今までどおり
    /// package 全体の変化にする。
    #[test]
    fn opaque_returns_keep_private_globs_package_wide_when_unconfined() {
        let nested = format!(
            "pub mod inner {{\n    use crate::a::*;\n}}\n{}",
            SCOPE_MODULE.replace("use crate::a::*;\n", "use crate::a::Hidden;\n")
        );
        let outside = |extra: &str| format!("{SCOPE_ROOT}{extra}");
        for (case, root, module, extra) in [
            (
                "trait impl in the scope",
                SCOPE_ROOT.to_owned(),
                format!(
                    "{SCOPE_MODULE}impl Clone for Client {{\n    fn clone(&self) -> Self {{\n        Client {{ inner: Default::default() }}\n    }}\n}}\n"
                ),
                vec![],
            ),
            (
                "trait impl outside the scope",
                outside(
                    "pub struct State<T>(core::marker::PhantomData<T>);\nunsafe impl Send for State<m::Client> {}\n",
                ),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "empty trait impl outside the scope",
                outside(
                    "pub trait Make<T> {\n    fn build(self) -> impl Copy\n    where\n        Self: Sized,\n    {\n        core::marker::PhantomData::<T>\n    }\n}\nimpl Make<m::Client> for () {}\n",
                ),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "item macro outside the scope",
                outside("macro_rules! install {\n    ($ty:ty) => {};\n}\ninstall!(m::Client);\n"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "derive outside the scope",
                outside("#[derive(Clone)]\npub struct Wrapper(m::Client);\n"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "expression macro outside the scope",
                outside("fn helper() -> usize {\n    stringify!(m::Client).len()\n}\n"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            // 展開の見えない構成 (マクロの呼び出しと derive を含む属性) は、範囲の glob で意味の変わる宣言を
            // 名指さなくても届き得る
            (
                "external macro outside the scope",
                outside("pub fn helper() -> u8 {\n    external::install!()\n}\n"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "standard macro outside the scope",
                outside("fn printed() {\n    println!(\"{}\", 1);\n}\n"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "standard derive outside the scope",
                outside("#[derive(Clone, Debug)]\npub struct Unrelated;\n"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "macro imported through the crate root",
                outside("extern crate helper;\nuse crate::helper::format;\nformat!();\n"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "local macro expanding an include",
                outside(
                    "macro_rules! load {\n    () => {\n        include!(\"state.rs\");\n    };\n}\nload!();\n",
                ),
                SCOPE_MODULE.to_owned(),
                vec![(
                    "src/state.rs",
                    "unsafe impl Send for State<crate::m::Client> {}\n",
                )],
            ),
            (
                "external macro inside an impl in a function",
                outside(
                    "pub struct Anchor;\nfn setup() {\n    impl Anchor {\n        fn unused() {\n            helper::format!();\n        }\n    }\n}\n",
                ),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            // `test` は同じ名前の取り込みに隠れ得るので、テストの関数の属性と中身も数える
            (
                "macro in a test function in the scope",
                SCOPE_ROOT.to_owned(),
                format!("{SCOPE_MODULE}#[test]\nfn check() {{\n    helper::verify!(Client);\n}}\n"),
                vec![],
            ),
            (
                "derive renamed to test outside the scope",
                outside("use core::prelude::v1::derive as test;\n#[test(Clone)]\nstruct Anchor;\n"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "external derive outside the scope",
                outside("#[derive(serde::Serialize)]\npub struct Wire;\n"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            // ツールの属性の名前は、同じ名前の取り込み (`use helper as rustfmt;`) で属性マクロを指し得る
            (
                "tool attribute outside the scope",
                outside(
                    "use core::prelude::v1 as clippy;\n#[clippy::derive(Clone)]\nstruct Marker;\n",
                ),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "tool attribute on the module",
                SCOPE_ROOT.replace("pub mod m;", "#[rustfmt::skip]\npub mod m;"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "include of an unparsed file outside the scope",
                outside("include!(\"send.inc\");\n"),
                SCOPE_MODULE.to_owned(),
                vec![("src/send.inc", "unsafe impl Send for State<m::Client> {}\n")],
            ),
            (
                "include of a parsed file outside the scope",
                outside(
                    "pub struct State<T>(core::marker::PhantomData<T>);\ninclude!(\"send.rs\");\n",
                ),
                SCOPE_MODULE.to_owned(),
                vec![("src/send.rs", "unsafe impl Send for State<m::Client> {}\n")],
            ),
            (
                "item macro in the scope",
                SCOPE_ROOT.to_owned(),
                format!("{SCOPE_MODULE}macro_rules! local {{\n    () => {{}};\n}}\nlocal!();\n"),
                vec![],
            ),
            (
                "expression macro in the scope",
                SCOPE_ROOT.to_owned(),
                format!("{SCOPE_MODULE}fn count() -> usize {{\n    stringify!(x).len()\n}}\n"),
                vec![],
            ),
            (
                "derive in the scope",
                SCOPE_ROOT.to_owned(),
                SCOPE_MODULE.replace(
                    "pub struct Client",
                    "#[derive(Clone, Copy)]\npub struct Client",
                ),
                vec![],
            ),
            (
                "attribute macro in the scope",
                SCOPE_ROOT.to_owned(),
                format!("{SCOPE_MODULE}#[some_attribute]\nfn local() {{}}\n"),
                vec![],
            ),
            (
                "extern block in the scope",
                SCOPE_ROOT.to_owned(),
                format!("{SCOPE_MODULE}extern \"C\" {{\n    fn ext() -> u8;\n}}\n"),
                vec![],
            ),
            (
                "attribute macro on the module",
                SCOPE_ROOT.replace("pub mod m;", "#[some_attribute]\npub mod m;"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "attribute macro on an ancestor",
                SCOPE_ROOT.replace("pub mod m;", "#[some_attribute]\npub mod m;"),
                nested.clone(),
                vec![],
            ),
            (
                "glob with a visibility",
                SCOPE_ROOT.to_owned(),
                SCOPE_MODULE.replace("use crate::a::*;", "pub(crate) use crate::a::*;"),
                vec![],
            ),
            (
                "glob placed in two modules",
                format!("{SCOPE_ROOT}#[path = \"m.rs\"]\nmod again;\n"),
                SCOPE_MODULE.to_owned(),
                vec![],
            ),
            (
                "unparsed file in the scope",
                SCOPE_ROOT.to_owned(),
                format!("{SCOPE_MODULE}pub mod broken;\n"),
                vec![("src/m/broken.rs", "pub fn (\n")],
            ),
        ] {
            assert_eq!(
                scope_reported(&root, &module, &extra),
                SCOPE_OPAQUE,
                "{case}"
            );
        }
    }

    /// Issue #381: 公開の関数は、本体に書かれていない引数の型と、囲む impl の見出し (receiver の型) にも依存
    /// する (async の future と不透明な戻り値が取り込む)。
    #[test]
    fn opaque_returns_follow_parameter_and_receiver_types() {
        let module = "pub type Hidden = ();\npub struct Client {\n    pub inner: Hidden,\n}\n";
        let root = "pub mod m;\nimpl m::Client {\n    pub async fn get(&self) {}\n}\npub async fn take(client: m::Client) {\n    drop(client);\n}\npub fn items(clients: &[m::Client]) -> impl Iterator<Item = &m::Client> {\n    clients.iter()\n}\npub async fn unrelated() {}\npub fn plain(client: &m::Client) -> u8 {\n    0\n}\n";
        let changed = module.replace(
            "pub type Hidden = ();",
            "pub type Hidden = core::marker::PhantomData<std::rc::Rc<()>>;",
        );
        let api = changes(
            &[("src/lib.rs", root), ("src/m.rs", module)],
            &[("src/m.rs", &changed)],
        );
        for name in ["get", "take", "items"] {
            assert!(modified(&api, name), "{name}: {api:?}");
        }
        for name in ["unrelated", "plain"] {
            assert!(!modified(&api, name), "{name}: {api:?}");
        }
    }

    /// Issue #381: 文字列のパスで取り込み得るファイルの glob は、取り込んだ先の module の束縛も変えるので範囲に
    /// 絞らない。`include!` (別名・raw 識別子・`macro_rules!` の中も) は取り込むファイルを確かめ切れないので、
    /// あればどの glob も絞らない (ビルドの生成物 (`OUT_DIR`) も、追跡中のファイルを取り込み得る)。解けない
    /// 絶対パスの文字列も同じ。マクロの引数に範囲の module の名前があれば、そのマクロにも届く。どの設定にも無い
    /// `include!` は妨げない。
    #[test]
    fn opaque_returns_keep_globs_in_included_files_package_wide() {
        let head = "pub mod a {\n    pub type Hidden = ();\n}\npub mod b {\n    pub type Hidden = core::marker::PhantomData<std::rc::Rc<()>>;\n}\n";
        let tail = "pub fn produce() -> impl Copy {\n    Hidden::default()\n}\npub fn unrelated() -> impl Copy {\n    0u8\n}\n";
        let module = "use crate::a::*;\n";
        for (case, reuse) in [
            (
                "the same file as a module and an include",
                "mod m;\ninclude!(\"m.rs\");\n",
            ),
            (
                "include in a macro definition",
                "mod m;\nmacro_rules! load {\n    () => {\n        include!(concat!(\"m\", \".rs\"));\n    };\n}\nload!();\n",
            ),
            (
                "renamed include",
                "mod m;\nuse core::include as load;\nload!(concat!(\"m\", \".rs\"));\n",
            ),
            (
                "raw include",
                "mod m;\nr#include!(concat!(\"m\", \".rs\"));\n",
            ),
            (
                "absolute include",
                "mod m;\ninclude!(\"/demo/src/m.rs\");\n",
            ),
            (
                "include out of the build output",
                "mod m;\ninclude!(concat!(env!(\"OUT_DIR\"), \"/../../../../../src/m.rs\"));\n",
            ),
            // ほかの crate が別の名前で公開する `include!` は名前で分からないので、文字列の値 (エスケープを解く)
            // でファイルを読み得るかを確かめる
            (
                "macro reading the module file",
                "mod m;\nhelper::embed!(\"m.rs\");\n",
            ),
            (
                "include renamed in another crate with an escaped path",
                "mod m;\nuse helper::load;\nload!(\"m\\u{2e}rs\");\n",
            ),
            (
                "macro reading the module file through a raw string",
                "mod m;\nhelper::embed!(r\"m.rs\");\n",
            ),
        ] {
            let root = format!("{head}{reuse}{tail}");
            let api = changes(
                &[("src/lib.rs", &root), ("src/m.rs", module)],
                &[("src/m.rs", &module.replace("crate::a::*", "crate::b::*"))],
            );
            assert!(modified(&api, "produce"), "{case}: {api:?}");
            assert!(modified(&api, "unrelated"), "{case}: {api:?}");
        }
        // マクロの引数に範囲の module の名前があれば、展開がその中身を使い得る
        let root = format!("{head}mod m {{\n    use crate::a::*;\n}}\nhelper::expose!(m);\n{tail}");
        let api = changes(
            &[("src/lib.rs", &root)],
            &[("src/lib.rs", &root.replace("crate::a::*", "crate::b::*"))],
        );
        assert!(modified(&api, "produce"), "{api:?}");
        let inner = other_with("mod inner {\n    #[allow(unused_imports)]\n    use super::*;\n}\n");
        for (case, include, expected) in [
            (
                "include of an unknown file",
                "include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/extra.rs\"));\n",
                &TEST_MODULE_OPAQUE[..],
            ),
            (
                "include of a generated file",
                "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n",
                &TEST_MODULE_OPAQUE[..],
            ),
            (
                "macro reading a file spelled in another case",
                "helper::embed!(\"Other.Rs\");\n",
                &TEST_MODULE_OPAQUE[..],
            ),
            (
                "include of an unknown file in no configuration",
                "#[cfg(any())]\ninclude!(concat!(env!(\"UNAVAILABLE\"), \"/unused.rs\"));\n",
                &[][..],
            ),
        ] {
            let root = format!("{TEST_MODULE_ROOT}{include}");
            let (reported, _) = opaque_reported(
                &[("src/lib.rs", &root), ("src/other.rs", TEST_MODULE_OTHER)],
                &[("src/other.rs", inner.clone())],
            );
            assert_eq!(reported, expected, "{case}");
        }
    }

    /// `mod` の中の取り込みの変化は、マクロが作る見えない項目の代わりに module の名前で伝える (Issue #381 の
    /// レビューで、`mod` の宣言を親の取り込みに結ぶと見落とすと分かった)。
    #[test]
    fn opaque_returns_follow_module_names_through_imports_inside_modules() {
        let before = "pub mod a {\n    pub type Hidden = ();\n}\npub mod b {\n    pub type Hidden = core::marker::PhantomData<std::rc::Rc<()>>;\n}\nmacro_rules! install {\n    () => {\n        pub fn made() -> impl Copy {\n            Hidden::default()\n        }\n    };\n}\nmod m {\n    use crate::a::Hidden;\n    install!();\n}\npub fn produce() -> impl Copy {\n    m::made()\n}\npub fn unrelated() -> impl Copy {\n    0u8\n}\n";
        let after = before.replace("use crate::a::Hidden;", "use crate::b::Hidden;");
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", &after)]);
        assert!(modified(&api, "produce"), "{api:?}");
        assert!(!modified(&api, "unrelated"), "{api:?}");
    }

    /// extern ブロックの中の static は宣言として持ち、型の変化を参照する関数へ伝える (Issue #381 の
    /// レビューで、extern ブロックを外へ効き得る構成として飛ばすと見落とすと分かった)。
    #[test]
    fn opaque_returns_follow_extern_statics() {
        let before = "type Hidden = ();\nextern \"C\" {\n    static VALUE: Hidden;\n}\npub fn produce() -> impl Copy {\n    unsafe { VALUE }\n}\npub fn ordinary() -> impl Copy {\n    0u8\n}\n";
        let after = before.replace(
            "type Hidden = ();",
            "type Hidden = core::marker::PhantomData<std::rc::Rc<()>>;",
        );
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", &after)]);
        assert!(modified(&api, "produce"), "{api:?}");
        assert!(!modified(&api, "ordinary"), "{api:?}");
    }

    #[test]
    fn opaque_return_moves_compare_the_verified_source_body() {
        let root = "pub mod a;\npub mod b;\npub use a::produce;\n";
        let destination_root = root.replace("a::produce", "b::produce");
        let before = "pub fn produce() -> impl Copy { 0u8 }\n";
        for (body, changed) in [
            ("0u8", false),
            ("core::marker::PhantomData::<std::rc::Rc<()>>", true),
        ] {
            let after =
                format!("pub fn anchor() {{}}\npub fn produce() -> impl Copy {{ {body} }}\n");
            let api = changes(
                &[
                    ("src/lib.rs", root),
                    ("src/a.rs", before),
                    ("src/b.rs", "pub fn anchor() {}\n"),
                ],
                &[
                    ("src/lib.rs", &destination_root),
                    ("src/a.rs", "// moved\n"),
                    ("src/b.rs", &after),
                ],
            );
            assert!(
                api.moved.iter().any(|movement| movement.name == "produce"),
                "{api:?}"
            );
            assert_eq!(modified(&api, "produce"), changed, "{body}: {api:?}");
        }
    }

    #[test]
    fn opaque_returns_keep_cfg_conditions_paired_with_each_body() {
        let before = "#[cfg(feature = \"wide\")]\npub fn produce() -> impl Copy { 0u8 }\n#[cfg(not(feature = \"wide\"))]\npub fn produce() -> impl Copy { core::marker::PhantomData::<std::rc::Rc<()>> }\n";
        let after = "#[cfg(feature = \"wide\")]\npub fn produce() -> impl Copy { core::marker::PhantomData::<std::rc::Rc<()>> }\n#[cfg(not(feature = \"wide\"))]\npub fn produce() -> impl Copy { 0u8 }\n";
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", after)]);
        assert!(modified(&api, "produce"), "{api:?}");
    }

    #[test]
    fn opaque_returns_follow_trait_headers_and_module_path_attributes() {
        let before = "trait Marker: Send {}\ntype Hidden = core::marker::PhantomData<Box<dyn Marker>>;\npub fn produce() -> impl Copy { Hidden::default() }\n";
        let after = before.replace("Marker: Send", "Marker");
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", &after)]);
        assert!(modified(&api, "produce"), "{api:?}");
        // 中身を変えない trait のコメント・整形は、指紋を変えない。
        let after = before.replace("Marker: Send", "Marker : /* same */ Send");
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", &after)]);
        assert!(!modified(&api, "produce"), "{api:?}");
        let root = "#[path = \"first.rs\"]\nmod selected;\npub fn produce() -> impl Copy { selected::make() }\n";
        let after = root.replace("first.rs", "second.rs");
        let api = changes(
            &[
                ("src/lib.rs", root),
                ("src/first.rs", "pub fn make() -> impl Copy { () }\n"),
                (
                    "src/second.rs",
                    "pub fn make() -> impl Copy { core::marker::PhantomData::<std::rc::Rc<()>> }\n",
                ),
            ],
            &[("src/lib.rs", &after)],
        );
        assert!(modified(&api, "produce"), "{api:?}");
    }
}
