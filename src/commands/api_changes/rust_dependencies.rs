//! Rust の公開シンボルの契約が名指す名前の束縛を旧新で比べる (Issue #283・#300)。
//!
//! 公開関数のシグネチャや公開構造体のフィールドの型が `use … as Text` の別名や非公開の
//! `type Text = …;` を名指すと、別名の指す先を変えてもシグネチャの字面は変わらない。宣言ごとに、
//! 契約 (関数の見出しと囲む impl の見出し、`pub` のフィールドの型、enum の variant の型、定数の型、
//! 型別名の右辺、trait の境界と関連型) が名指す名前 (ジェネリクスの引数と `Self` を除く) を、宣言を
//! 囲む module のスコープで解き、旧新で比べる。
//!
//! - 束縛は、囲む inline module、`self::`・`super::`・`crate::`、明示の `use` (`as` の別名と module
//!   の別名を含む)、型別名と定数 (右辺の名前を宣言したスコープで解いて置き換えた並びで識別し、連鎖を
//!   たどる。右辺の型の名前は値の式の中でも型として解く)、module への glob、item 位置のマクロの
//!   呼び出し (呼び出しとファイルのマクロ定義の字面を識別に入れる) で追う。cfg の付いた束縛 (`use`・
//!   module の別名・glob・module の宣言) は条件ごとの枝に分けて解き、枝の識別に条件を添える
//! - 同じスコープの構造体・enum などの宣言は「そこで宣言した」ことを識別にし、中身は入れない
//!   (型自身の変更は型の api.mod として出る)
//! - ほかのファイルの module (Issue #300): 差分の中の同じ package の Rust のファイルで、resolver が
//!   読む束縛 ([`DependencyIndex::fingerprint`]) が旧新で変わったときだけ、版ごとに lib の crate
//!   root から `mod` 宣言をたどってほかのファイルを開き、その束縛で解き続ける ([`crates`])。変わって
//!   いなければ、ファイルの外へ出るパスの束縛は旧新で同じなので、今までどおり字面 (crate root からの
//!   経路) で比べる。外部 crate・prelude は字面で比べる
//! - 識別は束縛の先だけで作り、書き方 (明示の use か glob か、`self::` の有無、`r#`、見える範囲の
//!   指定、コメント、並び順) では変えない。比べるファイルの中の定義はファイル相対の場所、ほかの
//!   ファイルの定義は crate root からの場所 (`crate::a::b`) で識別する
//! - 旧新の両方に現れる名前の字面ごとに、解いた束縛を比べる。片方にしか無い名前は、契約の字面の
//!   変更そのものが説明する (透過的な別名の導入など) ので比べない
//! - 束縛が変わっても、同じ定義を移しただけ (名前・種類・中身が同じで、差分の中で元の場所から消して
//!   移した先に足した。移した先はこのファイルか、差分の中の同じ crate のファイル) なら変化にしない。
//!   外の型の取り込みへの差し替えや、元からあった別の定義への付け替えは変化
//! - 名指す側のファイルが差分に無くても、変わった束縛を名指し得るファイルを探して比べ直す
//!   ([`dependents`])

use super::*;
use std::collections::HashMap;

use crate::engine::rust_use_tree::{RustPubUseEdge, UsePathOrigin, expand_rust_use_tree_edges_ast};

pub(crate) mod crates;
pub(crate) mod dependents;
pub(crate) mod exports;
pub(crate) mod placements;
mod resolver;
pub(crate) mod use_scan;

pub(crate) use crates::RustCrates;
pub(crate) use dependents::{ChangeSet, MoveMap};
pub(crate) use resolver::{
    Candidate, CrateFiles, File, Opened, Resolver, Target, candidates_cover, conditions_never_hold,
};

/// inherent impl の対象の型のトークン `owner` (`crate::S<T>` など。名前は `type_identifier:S` のように
/// 種類付き) の、型の引数の前までの経路の区切り (`crate`・`S`)。経路で書かれていない型 (`dyn Trait`・
/// 参照・タプル・配列・基本型など) や、外の crate からの経路 (`::a::S`) は None。
fn owner_path_segments(owner: &[String]) -> Option<Vec<String>> {
    let end = owner
        .iter()
        .position(|token| token == "<")
        .unwrap_or(owner.len());
    let mut segments = Vec::new();
    for (index, token) in owner[..end].iter().enumerate() {
        if index % 2 == 1 {
            if token != "::" {
                return None;
            }
            continue;
        }
        let (kind, text) = token.split_once(':')?;
        let name = match kind {
            "identifier" | "type_identifier" => unraw(text),
            "crate" | "self" | "super" if index == 0 || kind == "super" => {
                std::borrow::Cow::Borrowed(text)
            }
            _ => return None,
        };
        if name == "Self" {
            return None;
        }
        segments.push(name.into_owned());
    }
    (end % 2 == 1).then_some(segments)
}

/// 版の crate `krate` で、ファイル `path` の inline module の経路 `scope` (直下なら空) の中の inherent impl の
/// 対象の型 `owner` (トークン) を、そのスコープの名前解決で解いた候補と、ファイル `path` を置く条件の枝
/// (crate root からたどれなければ条件の無い枝 1 つ。Issue #315)。経路で書かれていない型や、ファイルを 1 つの
/// module に決められない・経路を解けないときは None。
pub(crate) fn impl_owner_candidates(
    krate: &crates::CrateVersion<'_>,
    path: &str,
    scope: &[String],
    owner: &[String],
    own: Option<&[AvailabilityAttr]>,
) -> Option<(Vec<Candidate>, Vec<Vec<String>>)> {
    let segments = owner_path_segments(owner)?;
    let Opened::File(file) = krate.open_file(path) else {
        return None;
    };
    let resolver = Resolver::new(Some(krate as &dyn CrateFiles), file.clone(), None, None);
    let candidates = resolver.resolve_type_path(&file, scope, &segments)?;
    let placements = resolver
        .placement_branches(&file)
        .unwrap_or_else(|| vec![Vec::new()]);
    let candidates = resolver::candidates_in_context(own, &placements, candidates);
    let direct_root = match segments.as_slice() {
        [anchor, name] if anchor == "crate" => {
            let prelude = format!("prelude:{name}");
            candidates.iter().all(|candidate| match &candidate.definition {
                Some(definition) => {
                    !definition.opaque
                        && definition.path == krate.root_path()
                        && definition.scope.is_empty()
                        && definition.name == *name
                        && matches!(definition.kind, "struct_item" | "enum_item" | "union_item")
                }
                None => matches!(&candidate.target, Target::Identity(identity) if *identity == prelude),
            })
        }
        _ => false,
    };
    let known: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| {
            candidate.definition.as_ref().is_some_and(|definition| {
                !definition.opaque
                    && matches!(definition.kind, "struct_item" | "enum_item" | "union_item")
            })
        })
        .cloned()
        .collect();
    // `crate::S` が root に直接宣言した型で、全設定を覆うと証明できたときだけ prelude を除く。
    // glob・別名・マクロの不明な候補は、この証明に混ぜない。`derive` は既存の型を消さない。
    let candidates =
        if direct_root && !known.is_empty() && candidates_cover(own, &placements, &known) {
            known
        } else {
            candidates
        };
    Some((candidates, placements))
}

/// 束縛を解く深さの上限 (別名の連鎖・module の経路・glob をたどる回数)。超えれば解けない。
const MAX_DEPTH: usize = 32;

/// 束縛の候補の数の上限 (条件付きの別名の枝の組み合わせ)。超えれば解けない。
const MAX_CANDIDATES: usize = 64;

/// 識別の区切り。
const SEPARATOR: &str = "\u{1f}";

/// 名指す名前の名前空間。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Namespace {
    Type,
    Value,
}

/// 契約が名指す名前 (パス)。
#[derive(Clone, Debug)]
struct Reference {
    /// パスの区切り (`a::b::Text` なら `a`・`b`・`Text`。先頭の `::` は `::`)。
    segments: Vec<String>,
    namespace: Namespace,
    /// 局所の束縛を作り得る式 ([`local_scope`]) の中の参照か。名前は外の module のスコープで解くが、
    /// 実際は局所の `use`・項目・束縛が隠し得るので、外の定義で覆いを証明しない (Issue #312)。
    local: bool,
}

/// 構文木の種類 `kind` の式が、中に局所の束縛 (`use`・項目・`let`・パターン・引数) を作り得るか。
fn local_scope(kind: &str) -> bool {
    matches!(
        kind,
        "block" | "closure_expression" | "match_expression" | "let_condition" | "let_chain"
    )
}

impl Reference {
    /// 旧新で対応させる字面。
    fn text(&self) -> String {
        self.segments.join("::")
    }
}

/// `use` で持ち込んだ束縛の先 (`r#` を外した経路)。
#[derive(Clone, Debug)]
struct UseTarget {
    module: Vec<String>,
    /// glob なら None。
    item: Option<String>,
    origin: UsePathOrigin,
    conditions: Vec<String>,
    /// 見える範囲 (ほかの module の glob に持ち込まれるか)。
    visibility: Visibility,
}

impl UseTarget {
    /// 束縛の指紋の字面。
    fn fingerprint(&self) -> String {
        format!(
            "use {:?} {} {:?} {} {:?}",
            self.origin,
            self.module.join("::"),
            self.item,
            self.conditions.join(" "),
            self.visibility
        )
    }
}

/// 宣言の見える範囲 (Issue #300)。glob が取り込むかを、取り込む側の module で決める。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Visibility {
    /// `pub` (どこからも見える)。
    Public,
    /// この module (crate root からの経路) とその子孫からだけ見える (`pub(crate)`・`pub(super)`・
    /// `pub(in …)`・`pub(self)`・指定なし)。
    Within(Vec<String>),
}

impl Visibility {
    /// module `importer` (crate root からの経路) から見えるか。
    fn visible_from(&self, importer: &[String]) -> bool {
        match self {
            Visibility::Public => true,
            Visibility::Within(module) => importer.starts_with(module),
        }
    }
}

/// module `current` (crate root からの経路) の中の項目 `item` の見える範囲。読めない `pub(in …)` は
/// どこからも見えるとする (glob が余分に取り込むだけで、見逃しにはならない)。
fn visibility(item: tree_sitter::Node<'_>, source: &[u8], current: &[String]) -> Visibility {
    let mut cursor = item.walk();
    let Some(modifier) = item
        .named_children(&mut cursor)
        .find(|child| child.kind() == "visibility_modifier")
    else {
        return Visibility::Within(current.to_vec());
    };
    modifier_visibility(modifier, source, current)
}

/// module `current` の中の見える範囲の指定 `modifier` (`visibility_modifier`) が表す範囲 (タプル構造体の
/// フィールドにも使う)。
fn modifier_visibility(
    modifier: tree_sitter::Node<'_>,
    source: &[u8],
    current: &[String],
) -> Visibility {
    let text: String = modifier
        .utf8_text(source)
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    let parent = || current[..current.len().saturating_sub(1)].to_vec();
    match text.as_str() {
        "pub" => Visibility::Public,
        "pub(crate)" => Visibility::Within(Vec::new()),
        "pub(self)" => Visibility::Within(current.to_vec()),
        "pub(super)" => Visibility::Within(parent()),
        _ => {
            let Some(path) = text
                .strip_prefix("pub(in")
                .and_then(|rest| rest.strip_suffix(')'))
            else {
                return Visibility::Public;
            };
            let segments: Vec<&str> = path.split("::").collect();
            let mut module = match segments.first() {
                Some(&"crate") => Vec::new(),
                Some(&"self") => current.to_vec(),
                Some(&"super") => parent(),
                _ => return Visibility::Public,
            };
            for segment in &segments[1..] {
                match *segment {
                    "super" => {
                        module.pop();
                    }
                    "self" => {}
                    segment => module.push(unraw(segment).into_owned()),
                }
            }
            Visibility::Within(module)
        }
    }
}

/// 識別に右辺を使う宣言 (型別名・定数・static)。右辺はソースの範囲で持ち、解くときに読む。
#[derive(Debug)]
struct Definition {
    conditions: Vec<String>,
    /// 自身か囲む inline module に、項目を置き換え得る属性 (属性マクロなど。
    /// [`opaque_preceding_attributes`]) が付いているか (Issue #312)。同じ名前の inline module の束縛は
    /// 1 つのスコープにまとめるので、宣言を囲まない (属性マクロで消える) 本体の定義もあり得る。
    opaque_attributes: bool,
    /// 自身に付いた、項目を置き換え得る属性 (属性マクロ・`derive` など) ごとの、適用される条件の属性の
    /// トークン (常に適用されるなら空。Issue #314)。
    replaceable: Vec<Vec<String>>,
    /// 宣言のノードのバイトの範囲。
    range: (usize, usize),
    /// 見える範囲 (ほかの module の glob に持ち込まれるか)。
    visibility: Visibility,
}

/// 宣言した項目 (種類と条件)。中身は識別に入れない。
#[derive(Debug)]
struct ItemDeclaration {
    kind: &'static str,
    conditions: Vec<String>,
    /// 自身か囲む inline module に、項目を置き換え得る属性 (属性マクロなど。
    /// [`opaque_preceding_attributes`]) が付いているか (Issue #312)。同じ名前の inline module の束縛は
    /// 1 つのスコープにまとめるので、宣言を囲まない (属性マクロで消える) 本体の定義もあり得る。
    opaque_attributes: bool,
    /// 自身に付いた、項目を置き換え得る属性 (属性マクロ・`derive` など) ごとの、適用される条件の属性の
    /// トークン (常に適用されるなら空。Issue #314)。
    replaceable: Vec<Vec<String>>,
    /// 名前付きのフィールドの構造体か (値の名前空間に名前を作らない)。
    named_fields: bool,
    /// 見える範囲 (ほかの module の glob に持ち込まれるか)。
    visibility: Visibility,
    /// 自身か囲む inline module に、項目そのもの (種類・名前・公開範囲) を置き換え得る属性 (属性マクロなど。
    /// 項目を足すだけの `derive` は数えない。[`item_replacing_preceding_attributes`]) が付いているか (Issue #315)。
    replacing_attributes: bool,
}

/// `mod` の宣言 (Issue #300)。
#[derive(Debug)]
pub(crate) struct ModuleDeclaration {
    /// 中身がこのファイルにある (inline)。
    inline: bool,
    /// 中身のファイルの指定 (`#[path]`)。
    path: ModuleSource,
    /// 宣言の条件 (囲む module の条件と、宣言自身の条件)。
    conditions: Vec<String>,
    /// 宣言の属性のトークン (指紋に入れる)。
    attributes: Vec<String>,
    /// 見える範囲 (ほかの module の glob に持ち込まれるか)。
    visibility: Visibility,
    /// 宣言の有る条件 (Issue #305。宣言自身、囲む inline module、ファイルの先頭の、`cfg` と `cfg` に
    /// 絞った `cfg_attr`。外側から)。読めない属性があれば None。
    availability: Option<Vec<AvailabilityAttr>>,
    /// `availability` に入れる属性の生のトークン。
    availability_raw: Vec<String>,
    /// 宣言自身の属性のうち、中身の場所の決め方に効き得るもの (`path`・`cfg_attr`) のトークン。
    location_attributes: Vec<String>,
    /// 宣言を囲む inline module の属性のうち、中身の場所の決め方に効き得るもののトークン (外側から)。
    enclosing_attributes: Vec<String>,
    /// 宣言を囲む inline module が全て `pub` (制限なし) か。
    enclosing_public: bool,
    /// 宣言自身に付いた、項目を置き換え得る属性 (属性マクロなど) ごとの、適用される条件の属性のトークン
    /// (常に適用されるなら空。展開で中身の名前が出入りし得る。囲む inline module のものはスコープの印で
    /// 持つ。Issue #314)。
    opaque: Vec<Vec<String>>,
}

impl ModuleDeclaration {
    /// 中身がこのファイルにある (inline) か。
    pub(crate) fn inline(&self) -> bool {
        self.inline
    }

    /// `pub` (制限なし) か。
    pub(crate) fn public(&self) -> bool {
        self.visibility == Visibility::Public
    }

    /// 宣言自身と、それを囲む inline module が全て `pub` (制限なし) か (crate の外から module の経路で
    /// 届くか)。
    pub(crate) fn reachable(&self) -> bool {
        self.public() && self.enclosing_public
    }

    /// 宣言の有る条件 (読めない属性があれば None)。
    pub(crate) fn availability(&self) -> Option<&[AvailabilityAttr]> {
        self.availability.as_deref()
    }

    /// 宣言の有る条件の生のトークン。
    pub(crate) fn availability_raw(&self) -> &[String] {
        &self.availability_raw
    }

    /// 中身のファイルの場所を決める入力の識別 (宣言と囲む inline module の、場所の決め方に効き得る
    /// 属性 (`path`・`cfg_attr`) のトークン)。場所を決められない宣言の旧新を比べるのに使う。
    pub(crate) fn resolution_identity(&self) -> String {
        format!(
            "{:?} {} | {}",
            self.path,
            self.location_attributes.join(" "),
            self.enclosing_attributes.join(" ")
        )
    }

    /// 宣言に `#[cfg(test)]` が付いているか (テストの build にだけある module)。
    pub(crate) fn only_in_tests(&self) -> bool {
        self.conditions
            .windows(4)
            .any(|window| window == ["identifier:cfg", "(", "identifier:test", ")"])
    }
}

/// `mod name;` の中身のファイルの指定。
#[derive(Clone, Debug, PartialEq, Eq)]
enum ModuleSource {
    /// 既定の場所 (`name.rs`・`name/mod.rs`)。
    Default,
    /// `#[path = "…"]` の値。
    Path(String),
    /// 読めない指定 (`cfg_attr` の中の `path`、重なる `path`、文字列でない値)。
    Unknown,
}

/// module のスコープの束縛。
#[derive(Debug, Default)]
struct ScopeBindings {
    /// 型・関数などの宣言 (型別名・定数・static・module は別に持つ)。
    items: HashMap<String, Vec<ItemDeclaration>>,
    /// 型別名。
    aliases: HashMap<String, Vec<Definition>>,
    /// 定数・static。
    constants: HashMap<String, Vec<Definition>>,
    /// 子の module の宣言。
    modules: HashMap<String, Vec<ModuleDeclaration>>,
    uses: HashMap<String, Vec<UseTarget>>,
    globs: Vec<UseTarget>,
    /// item 位置のマクロの呼び出しと、展開できない `use` のトークン (どの名前も生成し得る)。
    macros: Vec<String>,
    /// inline module に `#[path]` が付いている (中の `mod x;` の場所を決められない)。
    path_attribute: bool,
    /// item 位置でマクロを呼び出す (項目や module を生成し得る。Issue #305)。
    invokes_macros: bool,
    /// スコープの inline module か、それを囲む inline module のうち、項目を置き換え得る属性 (属性マクロなど)
    /// が付いているものの条件 (展開で名前が出入りし得る。同じ経路の cfg で分けた inline module ごと。
    /// Issue #314)。
    opaque: Vec<Vec<String>>,
    /// スコープの直下の、項目を置き換え得る属性 (属性マクロ・`derive` など) の付いた項目ごとの条件 (展開は
    /// 兄弟の名前も作り得る。Issue #314)。
    attributed: Vec<Vec<String>>,
    /// スコープの直下の item 位置のマクロの呼び出しごとの条件 (展開は兄弟の名前も作り得る。Issue #314)。
    macro_conditions: Vec<Vec<String>>,
    /// `impl`・trait・`extern` ブロックの中の、マクロの呼び出しごとの条件 (展開は関連項目を作り得るが、
    /// スコープの名前は作らない。Issue #314)。
    body_macro_conditions: Vec<Vec<String>>,
    /// `impl`・trait・`extern` ブロックの中の、項目を置き換え得る属性の付いた関連項目ごとの条件 (適用の
    /// 条件も。展開は関連項目を作り得る。Issue #314)。
    body_attributed: Vec<Vec<String>>,
}

impl ScopeBindings {
    /// 同じ経路のほかのスコープ (cfg で分けた同じ名前の inline module) の束縛を足す。
    fn merge(&mut self, other: ScopeBindings) {
        for (name, items) in other.items {
            self.items.entry(name).or_default().extend(items);
        }
        for (name, aliases) in other.aliases {
            self.aliases.entry(name).or_default().extend(aliases);
        }
        for (name, constants) in other.constants {
            self.constants.entry(name).or_default().extend(constants);
        }
        for (name, modules) in other.modules {
            self.modules.entry(name).or_default().extend(modules);
        }
        for (name, uses) in other.uses {
            self.uses.entry(name).or_default().extend(uses);
        }
        self.globs.extend(other.globs);
        self.macros.extend(other.macros);
        self.path_attribute |= other.path_attribute;
        self.invokes_macros |= other.invokes_macros;
        self.opaque.extend(other.opaque);
        self.attributed.extend(other.attributed);
        self.macro_conditions.extend(other.macro_conditions);
        self.body_macro_conditions
            .extend(other.body_macro_conditions);
        self.body_attributed.extend(other.body_attributed);
    }

    /// スコープが束縛する名前 (宣言・型別名・定数・module・`use`)。
    fn names(&self) -> std::collections::BTreeSet<&str> {
        self.items
            .keys()
            .chain(self.aliases.keys())
            .chain(self.constants.keys())
            .chain(self.modules.keys())
            .chain(self.uses.keys())
            .map(String::as_str)
            .collect()
    }
}

/// 外のパスの識別 `prefix` (`crate:a`・経路の無い `crate:`) に、続く名前の並び `rest` を足す。
fn outside_path(prefix: &str, rest: &[String]) -> String {
    let mut path = prefix.to_owned();
    for segment in rest {
        if !path.ends_with(':') {
            path.push_str("::");
        }
        path.push_str(segment);
    }
    path
}

/// Rust のファイルの、module のスコープごとの束縛の索引。
pub(crate) struct DependencyIndex {
    scopes: HashMap<Vec<String>, ScopeBindings>,
    /// ファイルの module の crate の中の経路 (`src` 相対の位置か、crate root から `mod` 宣言を
    /// たどった経路)。分からなければ None。
    file_module: Option<Vec<String>>,
    /// ファイルのパス (`dir` 相対)。
    path: String,
    /// 中の `mod x;` のファイルを、このファイルのディレクトリから探す (crate root・`mod.rs`・
    /// `#[path]` で開いたファイル)。ほかは、ファイルの名前のディレクトリから探す。
    mod_rs: bool,
    /// ファイルのマクロ定義 (`macro_rules!`) のトークン。マクロの呼び出しの識別に添える。
    macro_definitions: Vec<String>,
    /// ファイルの先頭の内側の `cfg` と `cfg` に絞った `cfg_attr` (読めなければ None。Issue #305)。
    file_availability: Option<Vec<AvailabilityAttr>>,
    /// ファイルの先頭の内側の条件の属性のトークン (Issue #314)。
    file_conditions: Vec<String>,
    tree: tree_sitter::Tree,
    source: Vec<u8>,
}

/// 旧新の比較の結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DependencyComparison {
    /// 旧新の両方に現れる名前の束縛がどれも同じ。
    Same,
    /// 束縛の変わった名前がある。
    Changed,
    /// 束縛を解けず、同じと言えない名前がある。
    Unknown,
}

impl DependencyComparison {
    /// blocking な api.mod に添える理由 (同じなら None)。
    pub(crate) fn unverified_reason(
        self,
    ) -> Option<crate::models::review::ApiModificationUnverifiedReason> {
        use crate::models::review::ApiModificationUnverifiedReason as Reason;
        match self {
            DependencyComparison::Same => None,
            DependencyComparison::Changed => Some(Reason::RustSignatureDependencyChanged),
            DependencyComparison::Unknown => Some(Reason::RustSignatureDependencyUnresolved),
        }
    }
}

/// 項目の種類が束縛する名前空間。
fn item_namespaces(item: &ItemDeclaration) -> &'static [Namespace] {
    match item.kind {
        // 単位・タプルの構造体は値も束縛する。属性で書き換わり得る項目は形を確定しない。
        "struct_item" if !item.named_fields || item.opaque_attributes => {
            &[Namespace::Type, Namespace::Value]
        }
        "struct_item" | "enum_item" | "union_item" | "trait_item" => &[Namespace::Type],
        _ => &[Namespace::Value],
    }
}

/// 識別子の字面 (`r#` を外す)。
fn unraw(text: &str) -> std::borrow::Cow<'_, str> {
    crate::language::normalize_identifier(crate::language::LangId::Rust, text)
}

/// `node` のトークンの並び ([`push_tokens`])。
fn tokens_of(node: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let mut tokens = Vec::new();
    push_tokens(node, source, &mut tokens);
    tokens
}

/// 条件 (`cfg`・`cfg_attr` の属性のトークンを続けた並び) を識別の前置きにする (無ければ空)。条件は
/// 属性の論理積なので、属性ごとに分けて並べ替え、重なりを除く (同じ条件を 2 回経た束縛と 1 回だけ
/// 経た束縛、属性の順序だけが違う束縛を同じにする)。
fn conditioned(conditions: &[String], identity: String) -> String {
    if conditions.is_empty() {
        return identity;
    }
    // 属性は括弧の外の `#` で始まる (`#[…]`・`#![…]`)。`cfg_attr` の中の属性の引数の `#` では
    // 分けない (区切りの記号は名前の無い葉のトークンで、字面そのもの)。
    let mut attributes: Vec<String> = Vec::new();
    let mut depth = 0usize;
    for token in conditions {
        match token.as_str() {
            "#" if depth == 0 => {
                attributes.push(token.clone());
                continue;
            }
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth = depth.saturating_sub(1),
            _ => {}
        }
        match attributes.last_mut() {
            Some(attribute) => {
                attribute.push(' ');
                attribute.push_str(token);
            }
            None => attributes.push(token.clone()),
        }
    }
    attributes.sort();
    attributes.dedup();
    format!("cfg({}){identity}", attributes.join(" "))
}

/// module のスコープの束縛の指紋 (Issue #300)。resolver が読む中身だけを持つ。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ScopeFingerprint {
    /// 名前ごとの束縛 (宣言の種類と条件、型別名・定数の宣言の全トークン、`mod` の宣言、`use` の先)。
    pub(crate) names: std::collections::BTreeMap<String, Vec<String>>,
    /// glob の先。
    pub(crate) globs: Vec<String>,
    /// item 位置のマクロの呼び出し (と、呼び出しがあればファイルのマクロ定義)。
    pub(crate) macros: Vec<String>,
    /// `mod` の宣言のある名前。
    pub(crate) modules: std::collections::BTreeSet<String>,
}

impl DependencyIndex {
    /// Rust のソース `source` (ファイル `file`、`dir` 相対) の索引。module の経路はパスの規約
    /// (`src` 相対の位置) から決める。構文エラーなら None。
    pub(crate) fn new(source: &[u8], file: &str) -> Option<Self> {
        let module = source_module_path(file).map(|(_, module)| module);
        let mod_rs = std::path::Path::new(file)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "lib.rs" | "main.rs" | "mod.rs"));
        Self::new_in_module(source, file, module, mod_rs)
    }

    /// module の経路 `file_module` を指定した索引 (`#[path]` で開いたファイルなど)。構文エラーなら None。
    pub(crate) fn new_in_module(
        source: &[u8],
        file: &str,
        file_module: Option<Vec<String>>,
        mod_rs: bool,
    ) -> Option<Self> {
        let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
        if tree.root_node().has_error() {
            return None;
        }
        let mut index = Self {
            scopes: HashMap::new(),
            file_module,
            path: file.to_owned(),
            mod_rs,
            macro_definitions: Vec::new(),
            file_availability: None,
            file_conditions: Vec::new(),
            tree,
            source: source.to_vec(),
        };
        let root = index.tree.root_node();
        index.file_availability = file_availability(root, &index.source);
        index.file_conditions = inner_conditions(root, &index.source);
        let mut scopes = Vec::new();
        let mut definitions = Vec::new();
        // 深い入れ子の inline module でもスタックを溢れさせないよう、明示のスタックで下りる。
        let mut pending = vec![ChildScope {
            body: root,
            path: Vec::new(),
            conditions: Vec::new(),
            path_attribute: false,
            opaque: Vec::new(),
        }];
        while let Some(scope) = pending.pop() {
            pending.extend(collect_scope(
                scope,
                &index.source,
                index.file_module.as_deref(),
                &mut scopes,
                &mut definitions,
            ));
        }
        // cfg で分けた同じ名前の inline module は、同じ経路のスコープとしてまとめる (束縛には
        // module の条件を引き継いでいる)。
        for (path, bindings) in scopes {
            index.scopes.entry(path).or_default().merge(bindings);
        }
        index.macro_definitions = definitions;
        Some(index)
    }

    /// ファイルのパス (`dir` 相対)。
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    /// ファイルの module の crate の中の経路。
    pub(crate) fn file_module(&self) -> Option<&[String]> {
        self.file_module.as_deref()
    }

    /// スコープ `scope` に名前 `name` の定義 (宣言・型別名・定数) があるか。
    fn defines(&self, scope: &[String], name: &str) -> bool {
        self.scopes.get(scope).is_some_and(|bindings| {
            bindings.items.contains_key(name)
                || bindings.aliases.contains_key(name)
                || bindings.constants.contains_key(name)
        })
    }

    /// スコープ `scope` の名前 `name` の、inline でない `mod` の宣言。
    pub(crate) fn module_declarations(
        &self,
        scope: &[String],
        name: &str,
    ) -> Vec<&ModuleDeclaration> {
        self.scopes
            .get(scope)
            .and_then(|bindings| bindings.modules.get(name))
            .into_iter()
            .flatten()
            .collect()
    }

    /// スコープ `scope` に inline の module `name` があるか。
    pub(crate) fn has_inline_module(&self, scope: &[String], name: &str) -> bool {
        let mut path = scope.to_vec();
        path.push(name.to_owned());
        self.scopes.contains_key(&path)
            && self
                .scopes
                .get(scope)
                .and_then(|bindings| bindings.modules.get(name))
                .is_some_and(|declarations| declarations.iter().any(|d| d.inline))
    }

    /// スコープ `scope` (inline module の経路) に `#[path]` が付いているか。
    pub(crate) fn scope_has_path_attribute(&self, scope: &[String]) -> bool {
        (1..=scope.len()).any(|depth| {
            self.scopes
                .get(&scope[..depth])
                .is_some_and(|bindings| bindings.path_attribute)
        })
    }

    /// スコープの経路 (inline module の経路) の並び。
    pub(crate) fn scope_paths(&self) -> impl Iterator<Item = &Vec<String>> {
        self.scopes.keys()
    }

    /// スコープ `scope` の名前 `name` の束縛が名指すパス (`use` の先、型別名・定数の右辺の名前、
    /// `mod` の名前)。
    pub(crate) fn binding_paths(&self, scope: &[String], name: &str) -> Vec<Vec<String>> {
        let Some(bindings) = self.scopes.get(scope) else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        for target in bindings.uses.get(name).into_iter().flatten() {
            let mut path = target.module.clone();
            path.extend(target.item.iter().cloned());
            paths.push(path);
        }
        let definitions = bindings
            .aliases
            .get(name)
            .into_iter()
            .flatten()
            .chain(bindings.constants.get(name).into_iter().flatten());
        for definition in definitions {
            if let Some(node) = self
                .tree
                .root_node()
                .descendant_for_byte_range(definition.range.0, definition.range.1)
            {
                // 型と型引数は型、定数の右辺は値として名前を集める ([`Resolver`] の右辺の解き方と同じ)。
                let mut references = Vec::new();
                for (field, namespace) in [
                    ("type_parameters", Namespace::Type),
                    ("type", Namespace::Type),
                    ("value", Namespace::Value),
                ] {
                    if let Some(child) = node.child_by_field_name(field) {
                        collect_references(child, &self.source, namespace, false, &mut references);
                    }
                }
                paths.extend(references.into_iter().map(|reference| reference.segments));
            }
        }
        if bindings.modules.contains_key(name) {
            paths.push(vec![name.to_owned()]);
        }
        paths
    }

    /// スコープ `scope` の glob の経路。
    pub(crate) fn glob_paths(&self, scope: &[String]) -> Vec<Vec<String>> {
        self.scopes
            .get(scope)
            .map(|bindings| {
                bindings
                    .globs
                    .iter()
                    .map(|glob| glob.module.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 中身のファイルを `#[path]` で指定した (か、指定を読めない) `mod` の宣言があるか。
    pub(crate) fn declares_path_modules(&self) -> bool {
        self.scopes.values().any(|bindings| {
            bindings.path_attribute
                || bindings
                    .modules
                    .values()
                    .flatten()
                    .any(|declaration| declaration.path != ModuleSource::Default)
        })
    }

    /// スコープ `scope` が束縛する名前。
    pub(crate) fn scope_names(&self, scope: &[String]) -> Vec<String> {
        self.scopes
            .get(scope)
            .map(|bindings| bindings.names().into_iter().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// module の置き場の条件に効く入力の指紋 (Issue #305)。スコープごとの、中身が別のファイルの `mod` の
    /// 宣言の名前・中身の場所の指定・公開か・場所の決め方に効き得る属性・有る条件の生のトークン・囲む
    /// inline module の場所の決め方に効き得る属性。ファイルの先頭の `#![cfg]` は宣言の有る条件に入る。
    pub(crate) fn module_condition_fingerprint(&self) -> Vec<String> {
        let mut entries = Vec::new();
        for (scope, bindings) in &self.scopes {
            for (name, declarations) in &bindings.modules {
                // inline module の宣言は、中のファイルの module の宣言の有る条件と場所の入力 (囲む inline
                // module の属性を含む) に入るので、それ自体は数えない (`#[cfg(test)] mod tests {}` を
                // 足しても門を開かない)。
                for declaration in declarations
                    .iter()
                    .filter(|declaration| !declaration.inline)
                {
                    entries.push(format!(
                        "{scope:?} {name} {} {} {} | {}",
                        declaration.inline,
                        declaration.reachable(),
                        declaration.resolution_identity(),
                        declaration.availability_raw.join(" ")
                    ));
                }
            }
        }
        entries.sort();
        entries
    }

    /// ファイルの `pub` (制限なし) の `use` が持ち込む・公開する名前 (束縛の名前と先の名前)。公開の glob・
    /// 展開できない `use`・項目の位置のマクロがあれば、どの名前も公開し得る (None)。`reachable` が偽の
    /// スコープ (どの設定でも無い inline module) は見ない。ファイルの非公開の inline module の項目を
    /// 再公開し得るかに使う (Issue #305)。
    pub(crate) fn public_mentions(
        &self,
        reachable: impl Fn(&[String]) -> bool,
    ) -> Option<std::collections::HashSet<String>> {
        let mut names = std::collections::HashSet::new();
        for (scope, bindings) in &self.scopes {
            // どの設定でも無い inline module の中は、何も公開しない。
            if !reachable(scope) {
                continue;
            }
            if !bindings.macros.is_empty()
                || bindings
                    .globs
                    .iter()
                    .any(|glob| glob.visibility == Visibility::Public)
            {
                return None;
            }
            for (bound, targets) in &bindings.uses {
                for target in targets {
                    if target.visibility == Visibility::Public {
                        names.insert(bound.clone());
                        names.extend(target.item.iter().cloned());
                    }
                }
            }
        }
        Some(names)
    }

    /// ファイルのソースの大きさ。
    pub(crate) fn source_len(&self) -> usize {
        self.source.len()
    }

    /// ファイルの先頭の内側の `cfg` と `cfg` に絞った `cfg_attr` (読めなければ None)。
    pub(crate) fn file_availability(&self) -> Option<&[AvailabilityAttr]> {
        self.file_availability.as_deref()
    }

    /// 始まりが `start` の型別名・構造体・enum・union の宣言全体のトークンと、trait の見出し (型引数・上位
    /// trait・`where`) と関連型のトークン (signature に入らない型別名の右辺、フィールド・variant、関連型の
    /// 制約を含む。コメントは除く。trait のメソッドと関連定数は別の項目として比べる)。ほかの宣言 (signature
    /// と束縛で比べる関数など) と引けない宣言は None (Issue #305)。
    pub(crate) fn declaration_contract(&self, start: (usize, usize)) -> Option<Vec<String>> {
        let node = declaration_at(self.tree.root_node(), start)?;
        match node.kind() {
            "type_item" | "struct_item" | "enum_item" | "union_item" => {
                Some(tokens_of(node, &self.source))
            }
            "trait_item" => {
                let body = node.child_by_field_name("body");
                let mut tokens = Vec::new();
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if Some(child) != body {
                        tokens.extend(tokens_of(child, &self.source));
                        continue;
                    }
                    let mut members = child.walk();
                    for member in child.named_children(&mut members) {
                        if member.kind() == "associated_type" {
                            tokens.extend(tokens_of(member, &self.source));
                        }
                    }
                }
                Some(tokens)
            }
            _ => None,
        }
    }

    /// スコープ `scope` (inline module の経路) の直下の item 位置のマクロの呼び出しごとの条件 (Issue #314)。
    pub(crate) fn scope_macro_conditions(&self, scope: &[String]) -> &[Vec<String>] {
        self.scopes
            .get(scope)
            .map_or(&[], |bindings| bindings.macro_conditions.as_slice())
    }

    /// スコープ `scope` (inline module の経路) の `impl`・trait・`extern` ブロックの中の、マクロの呼び出し
    /// ごとの条件 (Issue #314)。
    pub(crate) fn scope_body_macro_conditions(&self, scope: &[String]) -> &[Vec<String>] {
        self.scopes
            .get(scope)
            .map_or(&[], |bindings| bindings.body_macro_conditions.as_slice())
    }

    /// ファイルの先頭の内側の条件の属性のトークン (Issue #314)。
    pub(crate) fn file_conditions(&self) -> &[String] {
        &self.file_conditions
    }

    /// スコープ `scope` (inline module の経路) の inline module か、それを囲む inline module に付いた、項目を
    /// 置き換え得る属性 (属性マクロなど) ごとの適用の条件 (Issue #314)。
    pub(crate) fn scope_opaque(&self, scope: &[String]) -> &[Vec<String>] {
        self.scopes
            .get(scope)
            .map_or(&[], |bindings| bindings.opaque.as_slice())
    }

    /// スコープ `scope` (inline module の経路) の `impl`・trait・`extern` ブロックの中の、項目を置き換え得る
    /// 属性の付いた関連項目ごとの条件 (Issue #314)。
    pub(crate) fn scope_body_attributed(&self, scope: &[String]) -> &[Vec<String>] {
        self.scopes
            .get(scope)
            .map_or(&[], |bindings| bindings.body_attributed.as_slice())
    }

    /// スコープ `scope` (inline module の経路) の直下の、項目を置き換え得る属性 (属性マクロ・`derive`
    /// など) の付いた項目ごとの条件 (Issue #314)。
    pub(crate) fn scope_attributed(&self, scope: &[String]) -> &[Vec<String>] {
        self.scopes
            .get(scope)
            .map_or(&[], |bindings| bindings.attributed.as_slice())
    }

    /// resolver が読む束縛の指紋 (Issue #300)。スコープ (inline module の経路) ごと。関数の本体・
    /// impl・構造体の中身・コメントは入れない。
    pub(crate) fn fingerprint(&self) -> std::collections::BTreeMap<Vec<String>, ScopeFingerprint> {
        let node_tokens = |definition: &Definition| {
            self.tree
                .root_node()
                .descendant_for_byte_range(definition.range.0, definition.range.1)
                .map(|node| tokens_of(node, &self.source).join(" "))
                .unwrap_or_default()
        };
        let mut result = std::collections::BTreeMap::new();
        for (path, bindings) in &self.scopes {
            let mut fingerprint = ScopeFingerprint::default();
            let mut push = |name: &str, entry: String| {
                fingerprint
                    .names
                    .entry(name.to_owned())
                    .or_insert_with(Vec::new)
                    .push(entry);
            };
            for (name, items) in &bindings.items {
                for item in items {
                    push(
                        name,
                        format!(
                            "item {} {:?} {:?} {}",
                            item.kind,
                            item.visibility,
                            item_namespaces(item),
                            item.conditions.join(" ")
                        ),
                    );
                }
            }
            for (name, aliases) in &bindings.aliases {
                for alias in aliases {
                    push(
                        name,
                        format!(
                            "alias {} {}",
                            alias.conditions.join(" "),
                            node_tokens(alias)
                        ),
                    );
                }
            }
            for (name, constants) in &bindings.constants {
                for constant in constants {
                    push(
                        name,
                        format!(
                            "const {} {}",
                            constant.conditions.join(" "),
                            node_tokens(constant)
                        ),
                    );
                }
            }
            for (name, modules) in &bindings.modules {
                for module in modules {
                    push(
                        name,
                        format!(
                            "mod {} {:?} {:?} {} {}",
                            module.inline,
                            module.path,
                            module.visibility,
                            module.conditions.join(" "),
                            module.attributes.join(" ")
                        ),
                    );
                }
                fingerprint.modules.insert(name.clone());
            }
            for (name, uses) in &bindings.uses {
                for target in uses {
                    push(name, target.fingerprint());
                }
            }
            for entries in fingerprint.names.values_mut() {
                entries.sort();
            }
            fingerprint.globs = bindings.globs.iter().map(UseTarget::fingerprint).collect();
            fingerprint.globs.sort();
            fingerprint.macros = bindings.macros.clone();
            if !bindings.macros.is_empty() {
                fingerprint.macros.push(SEPARATOR.to_owned());
                fingerprint
                    .macros
                    .extend(self.macro_definitions.iter().cloned());
            }
            fingerprint
                .globs
                .push(format!("path_attribute {}", bindings.path_attribute));
            result.insert(path.clone(), fingerprint);
        }
        result
    }
}

/// 束縛を集める module のスコープ (ファイルの `source_file` か inline module の中身)。
struct ChildScope<'tree> {
    body: tree_sitter::Node<'tree>,
    /// ファイルの中の経路。
    path: Vec<String>,
    /// 囲む module の条件。
    conditions: Vec<String>,
    /// スコープの inline module に `#[path]` が付いているか。
    path_attribute: bool,
    /// スコープの inline module か、それを囲む inline module のうち、中身を置き換え得る属性 (属性マクロ
    /// など。[`opaque_preceding_attributes`]) が付いているものの条件 (Issue #312・#314)。
    opaque: Vec<Vec<String>>,
}

/// 旧い版の宣言 (`old_declarations`、旧い版の文脈 `old`) と新しい版の宣言 (`new_declarations`、
/// 新しい版の文脈 `new`) の契約が名指す名前の束縛を比べる。旧新の両方に現れる名前だけを比べる。
/// 定義の移動は、差分の中のファイル `files` で確かめる。
pub(crate) fn compare_dependencies(
    old: &Resolver<'_>,
    old_declarations: &[RustDeclarationContext],
    new: &Resolver<'_>,
    new_declarations: &[RustDeclarationContext],
    files: &DiffModules<'_>,
) -> DependencyComparison {
    // 枝を絞るのは、宣言 (同じ鍵の宣言をまとめて) が、原子を独立に扱った全ての設定で旧新に同じく
    // 有るときだけ (Issue #312)。有る設定が変わったら、その増減の報告はほかの比較 (項目の有る設定) の
    // 読み方に頼ることになるので、絞らずに全ての枝を比べる。宣言自身か囲む項目に属性マクロがあれば、
    // 宣言の有る条件と名前を解くスコープが展開で変わり得るので、やはり絞らない。
    let old_refs: Vec<&RustDeclarationContext> = old_declarations.iter().collect();
    let new_refs: Vec<&RustDeclarationContext> = new_declarations.iter().collect();
    let narrowing = !old_declarations
        .iter()
        .chain(new_declarations)
        .any(|declaration| declaration.macro_attributes)
        && super::rust_availability::same_in_every_config(&old_refs, &new_refs);
    let collect = |resolver: &Resolver<'_>, declarations: &[RustDeclarationContext]| {
        let mut merged: HashMap<String, Vec<Option<String>>> = HashMap::new();
        for declaration in declarations {
            // 宣言自身の有る条件で絞る (親の module の条件を含めなくても、実際に有る設定を含む)。
            let filter = narrowing
                .then(|| resolver::AvailabilityFilter::new(declaration.availability.as_deref()))
                .flatten()
                .map(std::rc::Rc::new);
            let found = resolver.declaration_dependencies(
                declaration.start,
                &declaration.inline_module,
                filter,
            )?;
            for (text, identities) in found {
                let entry = merged.entry(text).or_default();
                for identity in identities {
                    if !entry.contains(&identity) {
                        entry.push(identity);
                    }
                }
            }
        }
        for identities in merged.values_mut() {
            identities.sort();
        }
        Some(merged)
    };
    let (Some(old_dependencies), Some(new_dependencies)) = (
        collect(old, old_declarations),
        collect(new, new_declarations),
    ) else {
        return DependencyComparison::Unknown;
    };
    let mut result = DependencyComparison::Same;
    for (text, old_identities) in &old_dependencies {
        let Some(new_identities) = new_dependencies.get(text) else {
            continue;
        };
        if old_identities.contains(&None) || new_identities.contains(&None) {
            result = DependencyComparison::Unknown;
        } else if old_identities != new_identities
            && !moved_definition(old, old_identities, new, new_identities, files)
        {
            return DependencyComparison::Changed;
        }
    }
    result
}

/// 定義の場所。
#[derive(Debug, PartialEq, Eq)]
enum DefinitionPlace {
    /// 比べるファイルの module のスコープ。
    Local(Vec<String>),
    /// 差分の中のほかのファイルの module (crate root からの経路)。
    Crate(Vec<String>),
}

/// 識別が指す定義。
#[derive(Debug)]
struct LocatedDefinition {
    place: DefinitionPlace,
    name: String,
    /// 場所と名前を除いた識別 (種類と、型別名・定数なら右辺を解いた並び)。
    content: String,
}

/// 識別 `identity` (版の文脈 `resolver` で解いたもの) がただ 1 つの定義を指すなら、その場所と中身。
/// 差分の中のほかのファイルの定義は、`files` の同じ版 (`old`) の索引で引く。
fn located_definition(
    resolver: &Resolver<'_>,
    identity: &str,
    files: &DiffModules<'_>,
    old: bool,
) -> Option<LocatedDefinition> {
    if let Some((place, name)) = local_definition(identity) {
        let (file, scope, location) = match place_module(place) {
            // ほかのファイルの定義 (crate root からの場所)。
            Some(module) => {
                let (file, scope) = files.module_index(&module, old)?;
                (file, scope, DefinitionPlace::Crate(module))
            }
            None => {
                let scope = split_place(place);
                (
                    resolver.current.clone(),
                    scope.clone(),
                    DefinitionPlace::Local(scope),
                )
            }
        };
        // 右辺が `|` を含み得るので、字面を分けずに引き直して 1 つの定義を指すことを確かめる。
        if resolver.sole_definition(&file, &scope, name)? != identity {
            return None;
        }
        return Some(LocatedDefinition {
            place: location,
            name: name.to_owned(),
            content: definition_content(identity)?,
        });
    }
    let (module, name) = crate_location(resolver, identity)?;
    let (file, scope) = files.module_index(&module, old)?;
    let definition = resolver.sole_definition(&file, &scope, &name)?;
    Some(LocatedDefinition {
        place: DefinitionPlace::Crate(module),
        content: definition_content(&definition)?,
        name,
    })
}

/// 字面の識別 `identity` が crate の中のほかのファイルの名前 (`crate:`・`mod-file:`) なら、その
/// module の crate root からの経路と名前。
fn crate_location(resolver: &Resolver<'_>, identity: &str) -> Option<(Vec<String>, String)> {
    let mut path = if let Some(path) = identity.strip_prefix("crate:") {
        path_names(path)?
    } else {
        let mut path = resolver.current.file_module.clone()?;
        path.extend(path_names(identity.strip_prefix("mod-file:")?)?);
        path
    };
    let name = path.pop()?;
    Some((path, name))
}

/// 旧い束縛 `old_identities` から新しい束縛 `new_identities` への変化が、同じ定義を移しただけか。
/// 旧新がそれぞれただ 1 つの定義 (比べるファイルか、差分の中の同じ crate のファイル `files` のもの) を
/// 指し、名前・種類・中身 (型別名と定数は右辺を解いた並び) が同じで、差分の中で旧い定義の場所から
/// 消して新しい定義の場所に足したこと。外の型の取り込みへの差し替えや、元からあった別の定義への
/// 付け替えは移動でない。移した定義そのものの変更は、その定義の変更として出る。
fn moved_definition(
    old: &Resolver<'_>,
    old_identities: &[Option<String>],
    new: &Resolver<'_>,
    new_identities: &[Option<String>],
    files: &DiffModules<'_>,
) -> bool {
    let ([Some(old_identity)], [Some(new_identity)]) = (old_identities, new_identities) else {
        return false;
    };
    let (Some(from), Some(to)) = (
        located_definition(old, old_identity, files, true),
        located_definition(new, new_identity, files, false),
    ) else {
        return false;
    };
    from.name == to.name
        && from.content == to.content
        && from.place != to.place
        && !defined_at(&from, &new.current, files, false)
        && !defined_at(&to, &old.current, files, true)
}

/// 定義 `definition` の場所に、版 (`old`) で同じ名前の定義があるか。比べるファイルの場所は `current`
/// (その版の比べるファイルの索引) で、ほかのファイルの場所は `files` で引く。その版の差分にその場所の
/// ファイルが無ければ (消した・移した) 無いとする。
fn defined_at(
    definition: &LocatedDefinition,
    current: &DependencyIndex,
    files: &DiffModules<'_>,
    old: bool,
) -> bool {
    match &definition.place {
        DefinitionPlace::Local(scope) => current.defines(scope, &definition.name),
        DefinitionPlace::Crate(module) => files
            .module_index(module, old)
            .is_some_and(|(other, scope)| other.defines(&scope, &definition.name)),
    }
}

/// 定義の識別 (`item:` / `alias:` / `const:`) の、場所と名前。
fn local_definition(identity: &str) -> Option<(&str, &str)> {
    let rest = ["item:", "alias:", "const:"]
        .iter()
        .find_map(|prefix| identity.strip_prefix(prefix))?;
    let (place, rest) = rest.split_once('#')?;
    let (name, _) = rest.split_once(':')?;
    Some((place, name))
}

/// 定義の識別の、場所と名前を除いた部分 (`item:struct_item`・`alias:<type>…`)。
fn definition_content(identity: &str) -> Option<String> {
    let (prefix, rest) = ["item:", "alias:", "const:"]
        .iter()
        .find_map(|prefix| Some((*prefix, identity.strip_prefix(prefix)?)))?;
    let (_, rest) = rest.split_once('#')?;
    let (_, content) = rest.split_once(':')?;
    Some(format!("{prefix}{content}"))
}

/// crate root からの場所 (`crate`・`crate::a::b`) なら、その module の経路。
fn place_module(place: &str) -> Option<Vec<String>> {
    let rest = place.strip_prefix("crate")?;
    if rest.is_empty() {
        return Some(Vec::new());
    }
    Some(split_place(rest.strip_prefix("::")?))
}

/// `a::b` の形の場所のスコープ。
fn split_place(place: &str) -> Vec<String> {
    if place.is_empty() {
        Vec::new()
    } else {
        place.split("::").map(str::to_owned).collect()
    }
}

/// 外のパスの経路 (`a::b::Name`) の名前。識別子でない区切り (`|` でつないだ候補など) があれば None。
fn path_names(path: &str) -> Option<Vec<String>> {
    let names: Vec<String> = path
        .split("::")
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    names
        .iter()
        .all(|name| name.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .then_some(names)
}

/// 差分の中のファイルの、ある版のソースを読む関数 (読めなければ None)。
pub(crate) type SourceReader<'a> = Box<dyn Fn(&str) -> Option<Vec<u8>> + 'a>;

/// 差分の中の、同じ package の Rust のファイルの旧新の索引 (定義の移動を確かめる)。索引は引く
/// ときに作る。
pub(crate) struct DiffModules<'a> {
    old: Vec<ModuleFile>,
    new: Vec<ModuleFile>,
    read_old: SourceReader<'a>,
    read_new: SourceReader<'a>,
}

/// 差分の中のファイルと、その module の crate root からの経路。
struct ModuleFile {
    module: Vec<String>,
    path: String,
    index: std::cell::OnceCell<Option<File>>,
}

impl<'a> DiffModules<'a> {
    /// ファイル `file` と同じ package の、差分 `diff_files` の中の Rust のファイル。旧い版は
    /// `read_old`、新しい版は `read_new` で読む。
    pub(crate) fn new(
        file: &str,
        diff_files: &[crate::models::impact::DiffFile],
        read_old: SourceReader<'a>,
        read_new: SourceReader<'a>,
    ) -> Self {
        let package = source_module_path(file).map(|(package, _)| package);
        let module_file = |path: &str| {
            if path == "/dev/null" {
                return None;
            }
            let (file_package, module) = source_module_path(path)?;
            (package.as_ref() == Some(&file_package)).then(|| ModuleFile {
                module,
                path: path.to_owned(),
                index: std::cell::OnceCell::new(),
            })
        };
        Self {
            old: diff_files
                .iter()
                .filter_map(|df| module_file(&df.old_path))
                .collect(),
            new: diff_files
                .iter()
                .filter_map(|df| module_file(&df.new_path))
                .collect(),
            read_old,
            read_new,
        }
    }

    /// crate の module `module` (crate root からの経路) の中身を持つ、版 (`old`) の差分の中の
    /// ファイルの索引と、ファイルの中の inline module の経路。祖先の module のファイルの inline
    /// module も見る。差分に無ければ None。
    fn module_index(&self, module: &[String], old: bool) -> Option<(File, Vec<String>)> {
        let (files, read) = if old {
            (&self.old, &self.read_old)
        } else {
            (&self.new, &self.read_new)
        };
        let file = files
            .iter()
            .filter(|file| module.starts_with(&file.module))
            .max_by_key(|file| file.module.len())?;
        let index = file
            .index
            .get_or_init(|| {
                DependencyIndex::new(&read(&file.path)?, &file.path).map(std::rc::Rc::new)
            })
            .clone()?;
        Some((index, module[file.module.len()..].to_vec()))
    }
}

/// module のスコープ `scope` (ファイルの `source_file` か inline module の `declaration_list`) の
/// 束縛を集め、子の inline module を返す。
fn collect_scope<'tree>(
    scope: ChildScope<'tree>,
    source: &[u8],
    file_module: Option<&[String]>,
    scopes: &mut Vec<(Vec<String>, ScopeBindings)>,
    definitions: &mut Vec<String>,
) -> Vec<ChildScope<'tree>> {
    let ChildScope {
        body: scope,
        path,
        conditions: inherited,
        path_attribute,
        opaque,
    } = scope;
    let inherited = inherited.as_slice();
    // 定義に、項目を置き換え得る属性 (自身か囲む inline module のもの) が付いているか (Issue #312)。
    let opaque_attributes = |item: tree_sitter::Node<'_>| {
        !opaque.is_empty() || opaque_preceding_attributes(item, source)
    };
    let mut bindings = ScopeBindings {
        path_attribute,
        opaque: opaque.clone(),
        ..ScopeBindings::default()
    };
    let mut current_module: Vec<String> = file_module.map(<[String]>::to_vec).unwrap_or_default();
    current_module.extend(path.iter().cloned());
    let mut children = Vec::new();
    let mut cursor = scope.walk();
    for item in scope.named_children(&mut cursor) {
        // 束縛の条件は、囲む module の条件と項目自身の条件。
        let conditions = || {
            let mut conditions = inherited.to_vec();
            conditions.extend(preceding_conditions(item, source));
            conditions
        };
        let name = || {
            item.child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                .map(|name| unraw(name).into_owned())
        };
        // 項目自身に効く条件 (外側の属性と、inline module・関数・trait・impl・extern ブロックの中身の先頭の
        // 内側の属性。Issue #312・#314)。
        let own_conditions = || {
            let mut own = conditions();
            if matches!(
                item.kind(),
                "mod_item" | "function_item" | "trait_item" | "impl_item" | "foreign_mod_item"
            ) && let Some(body) = item.child_by_field_name("body")
            {
                own.extend(inner_conditions(body, source));
            }
            own
        };
        // 項目を置き換え得る属性の付いた項目 (展開は兄弟の名前も作り得る)。属性ごとに、項目自身の条件と
        // 適用の条件 (`cfg_attr` の述語) を合わせて数える (Issue #314)。
        if !matches!(
            item.kind(),
            "attribute_item" | "inner_attribute_item" | "line_comment" | "block_comment"
        ) {
            for applied in replaceable_attribute_conditions(item, source) {
                let mut all = own_conditions();
                all.extend(applied);
                bindings.attributed.push(all);
            }
        }
        // `impl`・trait・`extern` ブロックの中のマクロの呼び出しと属性の付いた関連項目は、ブロックの条件と
        // 項目ごとの条件を合わせて数える。ブロックに付いた属性マクロは中の `#[cfg]` を解かずに受け取るので、
        // 中に呼び出しがあれば、属性の適用の条件でも数える (Issue #305・#314)。
        if matches!(item.kind(), "impl_item" | "trait_item" | "foreign_mod_item")
            && let Some(body) = item.child_by_field_name("body")
        {
            let block = own_conditions();
            let (mut calls, attributed) = body_conditions(body, source);
            bindings.invokes_macros |= !calls.is_empty();
            if !calls.is_empty() {
                calls.extend(replaceable_attribute_conditions(item, source));
            }
            for (out, found) in [
                (&mut bindings.body_macro_conditions, calls),
                (&mut bindings.body_attributed, attributed),
            ] {
                for own in found {
                    let mut all = block.clone();
                    all.extend(own);
                    out.push(all);
                }
            }
        }
        match item.kind() {
            "use_declaration" => {
                let Some(argument) = item.child_by_field_name("argument") else {
                    continue;
                };
                let mut edges = Vec::new();
                if expand_rust_use_tree_edges_ast(
                    argument,
                    source,
                    &[],
                    &current_module,
                    &current_module,
                    None,
                    &mut edges,
                )
                .is_none()
                {
                    // 展開できない use は、どの名前も持ち込み得るものとして扱う。
                    bindings.macros.extend(tokens_of(item, source));
                    continue;
                }
                let conditions = conditions();
                let visibility = visibility(item, source, &current_module);
                let unraw_all =
                    |module: Vec<String>| module.iter().map(|s| unraw(s).into_owned()).collect();
                for edge in edges {
                    match edge {
                        RustPubUseEdge::Named {
                            exported_name,
                            target_module,
                            target_item,
                            origin,
                            ..
                        } => bindings
                            .uses
                            .entry(unraw(&exported_name).into_owned())
                            .or_default()
                            .push(UseTarget {
                                module: unraw_all(target_module),
                                item: Some(unraw(&target_item).into_owned()),
                                origin,
                                conditions: conditions.clone(),
                                visibility: visibility.clone(),
                            }),
                        RustPubUseEdge::Wildcard {
                            target_module,
                            origin,
                            ..
                        } => bindings.globs.push(UseTarget {
                            module: unraw_all(target_module),
                            item: None,
                            origin,
                            conditions: conditions.clone(),
                            visibility: visibility.clone(),
                        }),
                    }
                }
            }
            "extern_crate_declaration" => {
                let crate_name = item
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                    .map(|name| unraw(name).into_owned());
                let bound = item
                    .child_by_field_name("alias")
                    .and_then(|alias| alias.utf8_text(source).ok())
                    .map(|alias| unraw(alias).into_owned())
                    .or_else(|| crate_name.clone());
                if let (Some(crate_name), Some(bound)) = (crate_name, bound) {
                    bindings.uses.entry(bound).or_default().push(UseTarget {
                        module: Vec::new(),
                        item: Some(crate_name),
                        origin: UsePathOrigin::Global,
                        conditions: conditions(),
                        visibility: visibility(item, source, &current_module),
                    });
                }
            }
            "type_item" => {
                if let Some(name) = name() {
                    bindings.aliases.entry(name).or_default().push(Definition {
                        conditions: conditions(),
                        opaque_attributes: opaque_attributes(item),
                        replaceable: replaceable_attribute_conditions(item, source),
                        range: (item.start_byte(), item.end_byte()),
                        visibility: visibility(item, source, &current_module),
                    });
                }
            }
            "const_item" | "static_item" => {
                if let Some(name) = name() {
                    bindings
                        .constants
                        .entry(name)
                        .or_default()
                        .push(Definition {
                            conditions: conditions(),
                            opaque_attributes: opaque_attributes(item),
                            replaceable: replaceable_attribute_conditions(item, source),
                            range: (item.start_byte(), item.end_byte()),
                            visibility: visibility(item, source, &current_module),
                        });
                }
            }
            "struct_item"
            | "enum_item"
            | "union_item"
            | "trait_item"
            | "function_item"
            | "function_signature_item" => {
                if let Some(name) = name() {
                    let kind: &'static str = match item.kind() {
                        "struct_item" => "struct_item",
                        "enum_item" => "enum_item",
                        "union_item" => "union_item",
                        "trait_item" => "trait_item",
                        _ => "function_item",
                    };
                    // 関数・trait の本体の先頭の `#![cfg]` は、その項目自身に効く (Issue #312)。
                    let mut item_conditions = conditions();
                    if matches!(item.kind(), "function_item" | "trait_item")
                        && let Some(body) = item.child_by_field_name("body")
                    {
                        item_conditions.extend(inner_conditions(body, source));
                    }
                    bindings
                        .items
                        .entry(name)
                        .or_default()
                        .push(ItemDeclaration {
                            kind,
                            conditions: item_conditions,
                            opaque_attributes: opaque_attributes(item),
                            replaceable: replaceable_attribute_conditions(item, source),
                            named_fields: item.kind() == "struct_item"
                                && item
                                    .child_by_field_name("body")
                                    .is_some_and(|body| body.kind() == "field_declaration_list"),
                            visibility: visibility(item, source, &current_module),
                            replacing_attributes: !opaque.is_empty()
                                || item_replacing_preceding_attributes(item, source),
                        });
                }
            }
            "mod_item" => {
                let Some(name) = name() else {
                    continue;
                };
                let body = item.child_by_field_name("body");
                let attributes = preceding_attribute_tokens(item, source);
                let module_source = module_source(item, source);
                let path_attribute = module_source != ModuleSource::Default;
                let (availability, availability_raw) = item_availability(item, source);
                bindings
                    .modules
                    .entry(name.clone())
                    .or_default()
                    .push(ModuleDeclaration {
                        inline: body.is_some(),
                        path: module_source,
                        conditions: conditions(),
                        attributes,
                        visibility: visibility(item, source, &current_module),
                        availability,
                        availability_raw,
                        location_attributes: location_attribute_tokens(item, source),
                        enclosing_attributes: enclosing_module_attributes(item, source),
                        enclosing_public: enclosing_modules_public(item, source),
                        // inline module の中身の先頭の内側の属性も、宣言の属性の適用の条件に入れる。
                        opaque: replaceable_attribute_conditions(item, source)
                            .into_iter()
                            .map(|applied| {
                                let mut all = body
                                    .map(|body| inner_conditions(body, source))
                                    .unwrap_or_default();
                                all.extend(applied);
                                all
                            })
                            .collect(),
                    });
                if let Some(body) = body {
                    let mut child = path.clone();
                    child.push(name);
                    let mut child_conditions = conditions();
                    child_conditions.extend(inner_conditions(body, source));
                    let mut child_opaque = opaque.clone();
                    for applied in replaceable_attribute_conditions(item, source) {
                        let mut all = child_conditions.clone();
                        all.extend(applied);
                        child_opaque.push(all);
                    }
                    children.push(ChildScope {
                        body,
                        path: child,
                        conditions: child_conditions,
                        path_attribute,
                        opaque: child_opaque,
                    });
                }
            }
            "macro_invocation" | "expression_statement" => {
                bindings.macros.extend(tokens_of(item, source));
                bindings.invokes_macros = true;
                bindings.macro_conditions.push(conditions());
            }
            "macro_definition" => definitions.extend(tokens_of(item, source)),
            _ => {}
        }
    }
    scopes.push((path, bindings));
    children
}

/// 項目 `item` を囲む inline module の属性のトークン (外側から。各 module の外側の属性と、中身の
/// 内側の属性)。
fn enclosing_module_attributes(item: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut node = item;
    while let Some(body) = node.parent() {
        if body.kind() != "declaration_list" {
            break;
        }
        let Some(container) = body.parent() else {
            break;
        };
        let mut tokens = location_attribute_tokens(container, source);
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.kind() == "inner_attribute_item"
                && let Some(found) = location_tokens(child, source)
            {
                tokens.push("#![".to_owned());
                tokens.extend(found);
                tokens.push("]".to_owned());
            }
        }
        groups.push(tokens);
        node = container;
    }
    groups.reverse();
    groups.into_iter().flatten().collect()
}

/// `impl`・trait・`extern` ブロックの中身 `body` の直下の、マクロの呼び出しごとの条件 (呼び出し自身の
/// `cfg`) と、項目を置き換え得る属性の付いた関連項目の属性ごとの条件 (項目自身の `cfg` と属性の適用の
/// 条件)。関連関数 (trait のデフォルトの実装も) 自身の条件には、本体の先頭の内側の `#![cfg]` も入れる。
/// 展開は関連項目を作り得る (Issue #305・#314)。
fn body_conditions(
    body: tree_sitter::Node<'_>,
    source: &[u8],
) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    let (mut calls, mut attributed) = (Vec::new(), Vec::new());
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "macro_invocation" | "expression_statement" => {
                calls.push(preceding_conditions(child, source));
            }
            "attribute_item" | "inner_attribute_item" | "line_comment" | "block_comment" => {}
            _ => {
                let mut own = preceding_conditions(child, source);
                if child.kind() == "function_item"
                    && let Some(block) = child.child_by_field_name("body")
                {
                    own.extend(inner_conditions(block, source));
                }
                for applied in replaceable_attribute_conditions(child, source) {
                    let mut all = own.clone();
                    all.extend(applied);
                    attributed.push(all);
                }
            }
        }
    }
    (calls, attributed)
}

/// 属性 `attribute` の、module の中身の場所の決め方に効く部分のトークン (`path` と、`cfg_attr` の
/// `path` に絞った鍵)。効かなければ None、読めなければ属性全体。
fn location_tokens(attribute: tree_sitter::Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    match super::rust_type_api::project_attribute(attribute, source, &["path"]) {
        Some(Some(projected)) => Some(projected.key),
        Some(None) => None,
        None => Some(tokens_of(attribute, source)),
    }
}

/// 項目 `item` の直前に並ぶ外側の属性のうち、module の中身の場所の決め方に効く部分のトークン。
fn location_attribute_tokens(item: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut previous = item.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "attribute_item" => attributes.push(sibling),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    let mut tokens = Vec::new();
    for attribute in attributes.into_iter().rev() {
        if let Some(found) = location_tokens(attribute, source) {
            tokens.push("#[".to_owned());
            tokens.extend(found);
            tokens.push("]".to_owned());
        }
    }
    tokens
}

/// 項目 `item` を囲む inline module が全て `pub` (制限なし) か。
fn enclosing_modules_public(item: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut node = item;
    while let Some(body) = node.parent() {
        if body.kind() != "declaration_list" {
            break;
        }
        let Some(container) = body.parent() else {
            break;
        };
        if container.kind() == "mod_item"
            && !crate::engine::symbols::rust_node_has_unrestricted_pub_visibility(container, source)
        {
            return false;
        }
        node = container;
    }
    true
}

/// 項目 `item` の直前に並ぶ外側の属性のトークン (コメントは飛ばす)。
fn preceding_attribute_tokens(item: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut previous = item.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "attribute_item" => attributes.push(sibling),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    let mut tokens = Vec::new();
    for attribute in attributes.into_iter().rev() {
        push_tokens(attribute, source, &mut tokens);
    }
    tokens
}

/// `mod` の宣言 `item` の中身のファイルの指定 (`#[path = "…"]`)。`cfg_attr` の中の `path`、2 つ以上の
/// `path`、文字列で読めない値は読めない指定にする。
fn module_source(item: tree_sitter::Node<'_>, source: &[u8]) -> ModuleSource {
    let mut found = ModuleSource::Default;
    let mut previous = item.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "attribute_item" => {
                let mut cursor = sibling.walk();
                for attribute in sibling.named_children(&mut cursor) {
                    if attribute.kind() != "attribute" {
                        continue;
                    }
                    let name = attribute
                        .named_child(0)
                        .and_then(|name| super::rust_type_api::simple_name(name, source));
                    match name {
                        Some("path") => {
                            let value = attribute
                                .child_by_field_name("value")
                                .filter(|value| {
                                    matches!(value.kind(), "string_literal" | "raw_string_literal")
                                })
                                .and_then(|value| value.utf8_text(source).ok())
                                .and_then(super::rust_cfg_predicate::string_value);
                            found = match (found, value) {
                                (ModuleSource::Default, Some(value)) => ModuleSource::Path(value),
                                _ => ModuleSource::Unknown,
                            };
                        }
                        // `cfg_attr(…, path = "…")` は条件で場所が変わる。
                        Some("cfg_attr")
                            if super::rust_type_api::project_attribute(
                                sibling,
                                source,
                                &["path"],
                            )
                            .is_none_or(|projected| projected.is_some()) =>
                        {
                            found = ModuleSource::Unknown;
                        }
                        _ => {}
                    }
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    found
}

/// `start` (行, 列) に始まる宣言の項目のノード。
fn declaration_at(
    root: tree_sitter::Node<'_>,
    start: (usize, usize),
) -> Option<tree_sitter::Node<'_>> {
    let point = tree_sitter::Point {
        row: start.0,
        column: start.1,
    };
    let mut node = root.descendant_for_point_range(point, point)?;
    loop {
        if node.start_position() == point
            && matches!(
                node.kind(),
                "function_item"
                    | "function_signature_item"
                    | "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "trait_item"
                    | "type_item"
                    | "const_item"
                    | "static_item"
                    | "macro_definition"
                    | "mod_item"
                    | "impl_item"
                    | "associated_type"
            )
        {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// 宣言の契約が名指す名前 (宣言と、囲む impl・trait の generics の引数と `Self` を除く)。
fn contract_references(declaration: tree_sitter::Node<'_>, source: &[u8]) -> Vec<Reference> {
    let mut generics = generic_parameter_names(declaration, source);
    let mut references = Vec::new();
    // 宣言自身がブロックなど局所の束縛を作り得る式の中にあれば (`const _: () = { use a::LEN; .. };`
    // の中の impl など)、名前は局所の `use`・項目が隠し得る。
    let local = std::iter::successors(declaration.parent(), |node| node.parent())
        .any(|node| local_scope(node.kind()));
    let push = |node: Option<tree_sitter::Node<'_>>, references: &mut Vec<Reference>| {
        if let Some(node) = node {
            collect_references(node, source, Namespace::Type, local, references);
        }
    };
    let mut ancestor = declaration.parent();
    while let Some(node) = ancestor {
        match node.kind() {
            // メソッドを呼べる条件は、囲む impl の見出し (対象の型・trait・境界) にも書く。
            "impl_item" => {
                generics.extend(generic_parameter_names(node, source));
                for field in ["type_parameters", "trait", "type"] {
                    push(node.child_by_field_name(field), &mut references);
                }
                push(where_clause(node), &mut references);
            }
            "trait_item" => generics.extend(generic_parameter_names(node, source)),
            _ => {}
        }
        ancestor = node.parent();
    }
    match declaration.kind() {
        "function_item" | "function_signature_item" => {
            push(
                declaration.child_by_field_name("type_parameters"),
                &mut references,
            );
            if let Some(parameters) = declaration.child_by_field_name("parameters") {
                let mut cursor = parameters.walk();
                for parameter in parameters.named_children(&mut cursor) {
                    push(parameter.child_by_field_name("type"), &mut references);
                }
            }
            push(
                declaration.child_by_field_name("return_type"),
                &mut references,
            );
            push(where_clause(declaration), &mut references);
        }
        "struct_item" | "union_item" => {
            push(
                declaration.child_by_field_name("type_parameters"),
                &mut references,
            );
            push(where_clause(declaration), &mut references);
            if let Some(body) = declaration.child_by_field_name("body") {
                for ty in public_field_types(body, source) {
                    push(Some(ty), &mut references);
                }
            }
        }
        "enum_item" => {
            push(
                declaration.child_by_field_name("type_parameters"),
                &mut references,
            );
            push(where_clause(declaration), &mut references);
            push(declaration.child_by_field_name("body"), &mut references);
        }
        "type_item" => {
            push(
                declaration.child_by_field_name("type_parameters"),
                &mut references,
            );
            push(declaration.child_by_field_name("type"), &mut references);
        }
        "const_item" | "static_item" => {
            push(declaration.child_by_field_name("type"), &mut references);
        }
        "trait_item" => {
            push(
                declaration.child_by_field_name("type_parameters"),
                &mut references,
            );
            push(declaration.child_by_field_name("bounds"), &mut references);
            push(where_clause(declaration), &mut references);
            // 関連型の境界と既定の型、関連定数の型も trait の契約。
            if let Some(body) = declaration.child_by_field_name("body") {
                let mut cursor = body.walk();
                for item in body.named_children(&mut cursor) {
                    match item.kind() {
                        "associated_type" => {
                            for field in ["type_parameters", "bounds", "default_type"] {
                                push(item.child_by_field_name(field), &mut references);
                            }
                            push(where_clause(item), &mut references);
                        }
                        "const_item" => push(item.child_by_field_name("type"), &mut references),
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
    references.retain(|reference| {
        !names_generic(reference, &generics)
            && reference
                .segments
                .first()
                .is_none_or(|first| first != "Self")
    });
    references
}

/// 宣言の `where` 節。
fn where_clause(declaration: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = declaration.walk();
    declaration
        .named_children(&mut cursor)
        .find(|child| child.kind() == "where_clause")
}

/// 構造体・union の中身の、`pub` のフィールドの型。
fn public_field_types<'tree>(
    body: tree_sitter::Node<'tree>,
    source: &[u8],
) -> Vec<tree_sitter::Node<'tree>> {
    let is_public = |node: tree_sitter::Node<'_>| {
        node.utf8_text(source)
            .is_ok_and(|text| text.split_whitespace().collect::<String>() == "pub")
    };
    let mut types = Vec::new();
    match body.kind() {
        "field_declaration_list" => {
            let mut cursor = body.walk();
            for field in body.named_children(&mut cursor) {
                if field.kind() != "field_declaration" {
                    continue;
                }
                let mut field_cursor = field.walk();
                let public = field
                    .named_children(&mut field_cursor)
                    .any(|child| child.kind() == "visibility_modifier" && is_public(child));
                if public && let Some(ty) = field.child_by_field_name("type") {
                    types.push(ty);
                }
            }
        }
        "ordered_field_declaration_list" => {
            let mut public = false;
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                match child.kind() {
                    "visibility_modifier" => public = is_public(child),
                    "attribute_item" | "line_comment" | "block_comment" => {}
                    _ => {
                        if public {
                            types.push(child);
                        }
                        public = false;
                    }
                }
            }
        }
        _ => {}
    }
    types
}

/// 宣言の generics の引数の名前 (型・const・寿命)。
fn generic_parameter_names(declaration: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let Some(parameters) = declaration.child_by_field_name("type_parameters") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        let name = match parameter.kind() {
            "type_identifier" | "identifier" => Some(parameter),
            _ => parameter
                .child_by_field_name("name")
                .or_else(|| parameter.child_by_field_name("left")),
        };
        if let Some(text) = name.and_then(|name| name.utf8_text(source).ok()) {
            names.push(unraw(text).into_owned());
        }
    }
    names
}

/// 名前 `reference` の先頭が generics の引数か。
fn names_generic(reference: &Reference, generics: &[String]) -> bool {
    reference
        .segments
        .first()
        .is_some_and(|first| generics.iter().any(|generic| generic == first))
}

/// パスのノード (`identifier`・`type_identifier`・`scoped_identifier`・`scoped_type_identifier`・
/// `crate`・`self`・`super`) の区切り (`r#` を外す)。解けない形 (`<T as Trait>::X`・
/// `Vec<T>::X`) なら None。
fn path_segments(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    // 深い経路 (`m::m::…::X`) でもスタックを溢れさせないよう、末尾から繰り返しでたどる。
    let mut segments = Vec::new();
    let mut current = node;
    loop {
        match current.kind() {
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                segments.push(unraw(current.utf8_text(source).ok()?).into_owned());
                break;
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let name = current
                    .child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()?;
                segments.push(unraw(name).into_owned());
                match current.child_by_field_name("path") {
                    Some(path) => current = path,
                    // `::a` (先頭の `::`)。
                    None => {
                        segments.push("::".to_owned());
                        break;
                    }
                }
            }
            _ => return None,
        }
    }
    segments.reverse();
    Some(segments)
}

/// `node` の中の、型 (と配列の長さ・const ジェネリクスの引数の値) として名指す名前を集める。`local` は
/// `node` が局所の束縛を作り得る式の中にあるか。
fn collect_references(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    namespace: Namespace,
    local: bool,
    references: &mut Vec<Reference>,
) {
    // 深い入れ子の型 (`Option<Option<…>>`) でもスタックを溢れさせないよう、明示のスタックでたどる。
    // 局所の束縛を作り得る式の中かも持つ。
    let mut stack = vec![(node, namespace, local)];
    while let Some((node, namespace, local)) = stack.pop() {
        match node.kind() {
            // 型の名前は値の式の中 (`{ size_of::<T>() }`) でも型の名前空間で引く。
            "type_identifier" | "scoped_type_identifier" => {
                if let Some(segments) = path_segments(node, source) {
                    references.push(Reference {
                        segments,
                        namespace: Namespace::Type,
                        local,
                    });
                } else if let Some(path) = node.child_by_field_name("path") {
                    // `<() as Selected>::Item`・`Vec<T>::Assoc` は、末尾の関連型の名前でなく、経路の
                    // 中の名前を集める。
                    stack.push((path, Namespace::Type, local));
                }
                continue;
            }
            "identifier" | "scoped_identifier" if namespace == Namespace::Value => {
                if let Some(segments) = path_segments(node, source) {
                    references.push(Reference {
                        segments,
                        namespace,
                        local,
                    });
                }
                continue;
            }
            "macro_invocation" => {
                // 型の位置のマクロは、呼び出しとファイルのマクロ定義の字面で識別する。
                references.push(Reference {
                    segments: vec![format!("<macro {}>", tokens_of(node, source).join(" "))],
                    namespace,
                    local,
                });
                continue;
            }
            "lifetime" | "line_comment" | "block_comment" => continue,
            "array_type" => {
                if let Some(element) = node.child_by_field_name("element") {
                    stack.push((element, Namespace::Type, local));
                }
                if let Some(length) = node.child_by_field_name("length") {
                    stack.push((length, Namespace::Value, local));
                }
                continue;
            }
            "type_binding" => {
                // `Item = Text` の `Item` は関連型の名前で、束縛ではない。
                if let Some(ty) = node.child_by_field_name("type") {
                    stack.push((ty, namespace, local));
                }
                continue;
            }
            "type_arguments" => {
                // `Item: Copy` (関連型の境界) は `Item` と境界が並ぶ。`Item` は関連型の名前。const
                // ジェネリクスの引数の `{ … }` は値。
                let mut cursor = node.walk();
                let children: Vec<_> = node.named_children(&mut cursor).collect();
                for (index, child) in children.iter().enumerate() {
                    let bounded = child.kind() == "type_identifier"
                        && children
                            .get(index + 1)
                            .is_some_and(|next| next.kind() == "trait_bounds");
                    if bounded {
                        continue;
                    }
                    let namespace = if child.kind() == "block" {
                        Namespace::Value
                    } else {
                        Namespace::Type
                    };
                    stack.push((*child, namespace, local));
                }
                continue;
            }
            _ => {}
        }
        let local = local || local_scope(node.kind());
        let mut cursor = node.walk();
        stack.extend(
            node.named_children(&mut cursor)
                .map(|child| (child, namespace, local)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `old` と `new` の、`name` の宣言の契約の束縛の比較。宣言は前順で最初の同じ名前のもの。
    fn compare(old: &str, new: &str, name: &str) -> DependencyComparison {
        let index =
            |source: &str| DependencyIndex::new(source.as_bytes(), "src/lib.rs").expect("index");
        let declaration = |source: &str| {
            let tree =
                parser::parse_source(source.as_bytes(), crate::language::LangId::Rust).unwrap();
            let mut stack = vec![tree.root_node()];
            while let Some(node) = stack.pop() {
                if node.kind() != "mod_item"
                    && node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                        == Some(name)
                {
                    let start = node.start_position();
                    let mut module = Vec::new();
                    let mut ancestor = node.parent();
                    while let Some(parent) = ancestor {
                        if parent.kind() == "mod_item" {
                            let text = parent
                                .child_by_field_name("name")
                                .unwrap()
                                .utf8_text(source.as_bytes())
                                .unwrap();
                            module.push(unraw(text).into_owned());
                        }
                        ancestor = parent.parent();
                    }
                    module.reverse();
                    return RustDeclarationContext {
                        inline_module: module.join("::"),
                        owner: Vec::new(),
                        conditions: Vec::new(),
                        start: (start.row, start.column),
                        container_header: Vec::new(),
                        availability: item_availability(node, source.as_bytes()).0,
                        availability_raw: Vec::new(),
                        inline_public: true,
                        macro_attributes: false,
                        container_macro_attributes: false,
                    };
                }
                let mut cursor = node.walk();
                let children: Vec<_> = node.named_children(&mut cursor).collect();
                stack.extend(children.into_iter().rev());
            }
            panic!("declaration {name}");
        };
        let files = DiffModules::new("src/lib.rs", &[], Box::new(|_| None), Box::new(|_| None));
        let resolver = |source: &str| Resolver::textual(std::rc::Rc::new(index(source)));
        super::compare_dependencies(
            &resolver(old),
            &[declaration(old)],
            &resolver(new),
            &[declaration(new)],
            &files,
        )
    }

    use DependencyComparison::{Changed, Same};

    #[test]
    fn raw_path_attributes_use_the_same_module_source() {
        for (attributes, expected, has_path) in [
            (
                "#[path = \"actual.rs\"]",
                ModuleSource::Path("actual.rs".into()),
                true,
            ),
            (
                "#[r#path = \"actual.rs\"]",
                ModuleSource::Path("actual.rs".into()),
                true,
            ),
            (
                "#[r#path = r\"actual.rs\"]",
                ModuleSource::Path("actual.rs".into()),
                true,
            ),
            (
                "#[cfg_attr(unix, path = \"actual.rs\")]",
                ModuleSource::Unknown,
                true,
            ),
            (
                "#[cfg_attr(unix, r#path = \"actual.rs\")]",
                ModuleSource::Unknown,
                true,
            ),
            (
                "#[r#cfg_attr(unix, r#cfg_attr(windows, r#path = \"actual.rs\"))]",
                ModuleSource::Unknown,
                true,
            ),
            (
                "#[path = \"first.rs\"]\n#[r#path = \"actual.rs\"]",
                ModuleSource::Unknown,
                true,
            ),
            ("#[r#path = 1]", ModuleSource::Unknown, true),
            (
                "#[r#path = \"actual.rs\"]\n// comment",
                ModuleSource::Path("actual.rs".into()),
                true,
            ),
            ("#[doc = \"path\"]", ModuleSource::Default, false),
            (
                "#[cfg_attr(unix, doc = \"path\")]",
                ModuleSource::Default,
                false,
            ),
            (
                "#[tool::path = \"actual.rs\"]",
                ModuleSource::Default,
                false,
            ),
        ] {
            let source = format!("{attributes}\nmod selected;\n");
            let tree = parser::parse_source(source.as_bytes(), crate::language::LangId::Rust)
                .expect("parse");
            assert!(!tree.root_node().has_error(), "{attributes}");
            let module = tree
                .root_node()
                .named_children(&mut tree.root_node().walk())
                .find(|node| node.kind() == "mod_item")
                .expect("module");
            assert_eq!(
                module_source(module, source.as_bytes()),
                expected,
                "{attributes}"
            );
            assert_eq!(
                rust_mod_item_has_path_attribute(module, source.as_bytes()),
                has_path,
                "{attributes}"
            );
        }
    }

    #[test]
    fn alias_targets_are_compared() {
        // `use … as` の別名と非公開の型別名の指す先の変化
        assert_eq!(
            compare(
                "use std::path::PathBuf as Text;\npub fn label(t: Text) -> usize { 0 }\n",
                "use std::string::String as Text;\npub fn label(t: Text) -> usize { 0 }\n",
                "label"
            ),
            Changed
        );
        assert_eq!(
            compare(
                "type Text = std::path::PathBuf;\npub fn label(t: Text) {}\n",
                "type Text = String;\npub fn label(t: Text) {}\n",
                "label"
            ),
            Changed
        );
        // 別名の連鎖
        assert_eq!(
            compare(
                "type Inner = std::path::PathBuf;\ntype Text = Inner;\npub fn label(t: Text) {}\n",
                "type Inner = String;\ntype Text = Inner;\npub fn label(t: Text) {}\n",
                "label"
            ),
            Changed
        );
        // 変えなければ同じ。本体や引数名、無関係な use の変更も同じ
        assert_eq!(
            compare(
                "use std::path::PathBuf as Text;\nuse std::fmt;\npub fn label(t: Text) -> usize { 0 }\n",
                "use std::path::PathBuf as Text;\npub fn label(text: Text) -> usize { 1 }\n",
                "label"
            ),
            Same
        );
    }

    #[test]
    fn value_name_resolution_follows_struct_shape() {
        for (declaration, value_binding) in [
            ("struct LEN;", true),
            ("struct LEN();", true),
            ("struct LEN(u8);", true),
            ("struct LEN {}", false),
            ("struct LEN { hidden: () }", false),
            ("#[rewrite]\nstruct LEN { hidden: () }", true),
            ("#[derive(Create)]\nstruct LEN { hidden: () }", true),
            ("union LEN { hidden: u8 }", false),
            ("enum LEN { Value }", false),
            ("trait LEN {}", false),
            ("fn LEN() {}", true),
        ] {
            let index = std::rc::Rc::new(
                DependencyIndex::new(declaration.as_bytes(), "src/lib.rs").expect("index"),
            );
            let resolver = Resolver::textual(index.clone());
            let module = resolver::ModuleRef {
                file: index,
                scope: Vec::new(),
            };
            let resolved = resolver
                .resolve_name(&module, "LEN", Namespace::Value, 0)
                .expect("resolve");
            assert_eq!(
                resolved
                    .0
                    .iter()
                    .any(|candidate| candidate.definition.is_some()),
                value_binding,
                "{declaration}"
            );
        }
    }

    #[test]
    fn namespace_shape_is_in_dependency_fingerprint() {
        let fingerprint = |source: &str| {
            DependencyIndex::new(source.as_bytes(), "src/lib.rs")
                .expect("index")
                .fingerprint()
        };
        assert_ne!(fingerprint("struct LEN;"), fingerprint("struct LEN {}"));
        assert_ne!(fingerprint("struct LEN(u8);"), fingerprint("struct LEN {}"));
        for attribute in ["#[rewrite]", "#[derive(Create)]"] {
            assert_ne!(
                fingerprint("struct LEN {}"),
                fingerprint(&format!("{attribute}\nstruct LEN {{}}"))
            );
        }
        assert_eq!(
            fingerprint("struct LEN { hidden: u8 }"),
            fingerprint("struct LEN { hidden: u16 }")
        );
    }

    #[test]
    fn scopes_generics_and_globs_are_followed() {
        // ジェネリクスの引数は別名と関係ない
        assert_eq!(
            compare(
                "use std::path::PathBuf as Text;\npub fn label<Text>(t: Text) {}\n",
                "use std::string::String as Text;\npub fn label<Text>(t: Text) {}\n",
                "label"
            ),
            Same
        );
        // 別の inline module の同じ名前の別名は関係ない
        assert_eq!(
            compare(
                "pub mod a {\n    pub type Text = u8;\n}\npub mod b {\n    type Text = u16;\n    pub fn label(t: Text) {}\n}\n",
                "pub mod a {\n    pub type Text = u32;\n}\npub mod b {\n    type Text = u16;\n    pub fn label(t: Text) {}\n}\n",
                "label"
            ),
            Same
        );
        // `use super::*` で引いた親の別名の変化
        assert_eq!(
            compare(
                "type Text = std::path::PathBuf;\npub mod api {\n    use super::*;\n    pub fn label(t: Text) {}\n}\n",
                "type Text = String;\npub mod api {\n    use super::*;\n    pub fn label(t: Text) {}\n}\n",
                "label"
            ),
            Changed
        );
        // 修飾したパスの先の別名の変化
        assert_eq!(
            compare(
                "pub mod m {\n    pub type Text = u8;\n}\npub fn label(t: m::Text) {}\n",
                "pub mod m {\n    pub type Text = u16;\n}\npub fn label(t: m::Text) {}\n",
                "label"
            ),
            Changed
        );
        // マクロの定義だけを変える
        assert_eq!(
            compare(
                "macro_rules! define_text {\n    () => { type Text = u8; };\n}\ndefine_text!();\npub fn label(t: Text) {}\n",
                "macro_rules! define_text {\n    () => { type Text = u16; };\n}\ndefine_text!();\npub fn label(t: Text) {}\n",
                "label"
            ),
            Changed
        );
    }

    #[test]
    fn public_fields_and_constants_are_compared() {
        assert_eq!(
            compare(
                "type Text = u8;\npub struct Note {\n    pub text: Text,\n}\n",
                "type Text = u16;\npub struct Note {\n    pub text: Text,\n}\n",
                "Note"
            ),
            Changed
        );
        // 非公開のフィールドの型は契約に入らない
        assert_eq!(
            compare(
                "type Text = u8;\npub struct Note {\n    text: Text,\n}\n",
                "type Text = u16;\npub struct Note {\n    text: Text,\n}\n",
                "Note"
            ),
            Same
        );
        assert_eq!(
            compare(
                "type Text = u8;\npub const LIMIT: Text = 1;\n",
                "type Text = u16;\npub const LIMIT: Text = 1;\n",
                "LIMIT"
            ),
            Changed
        );
        // 同じ型の構造体の中身の変化は、その型の変更として出るので関数には伝えない
        assert_eq!(
            compare(
                "pub struct Text {\n    pub a: u8,\n}\npub fn label(t: Text) {}\n",
                "pub struct Text {\n    pub a: u16,\n}\npub fn label(t: Text) {}\n",
                "label"
            ),
            Same
        );
    }

    /// 前回のレビューの見逃しの反例。
    #[test]
    fn module_aliases_conditions_and_qualified_paths_are_followed() {
        let types = "pub mod a {\n    pub struct Text;\n}\npub mod b {\n    pub struct Text;\n}\n";
        // module の別名を経由した use
        assert_eq!(
            compare(
                &format!("{types}use a as chosen;\nuse chosen::Text;\npub fn f(_: Text) {{}}\n"),
                &format!("{types}use b as chosen;\nuse chosen::Text;\npub fn f(_: Text) {{}}\n"),
                "f"
            ),
            Changed
        );
        // glob の cfg 条件の入れ替え
        assert_eq!(
            compare(
                &format!(
                    "{types}#[cfg(unix)]\nuse a::*;\n#[cfg(not(unix))]\nuse b::*;\npub fn f(_: Text) {{}}\n"
                ),
                &format!(
                    "{types}#[cfg(not(unix))]\nuse a::*;\n#[cfg(unix)]\nuse b::*;\npub fn f(_: Text) {{}}\n"
                ),
                "f"
            ),
            Changed
        );
        // 修飾された関連型の経路の中の名前
        let traits = "pub trait A {\n    type Item;\n}\npub trait B {\n    type Item;\n}\n";
        assert_eq!(
            compare(
                &format!(
                    "{traits}use self::A as Selected;\npub fn f(_: <() as Selected>::Item) {{}}\n"
                ),
                &format!(
                    "{traits}use self::B as Selected;\npub fn f(_: <() as Selected>::Item) {{}}\n"
                ),
                "f"
            ),
            Changed
        );
        // `r#` の付いた inline module の中の別名
        assert_eq!(
            compare(
                "pub mod r#api {\n    type Text = u8;\n    pub fn f(_: Text) {}\n}\n",
                "pub mod r#api {\n    type Text = u16;\n    pub fn f(_: Text) {}\n}\n",
                "f"
            ),
            Changed
        );
    }

    #[test]
    fn constant_values_and_enclosing_bounds_are_followed() {
        // 配列の長さの定数の値
        assert_eq!(
            compare(
                "const N: usize = 2;\npub fn f(_: [u8; N]) {}\n",
                "const N: usize = 3;\npub fn f(_: [u8; N]) {}\n",
                "f"
            ),
            Changed
        );
        // const ジェネリクスの引数の定数の import
        let consts = "pub mod a {\n    pub const N: usize = 1;\n}\npub mod b {\n    pub const N: usize = 2;\n}\npub struct Buf<const K: usize>(pub [u8; K]);\n";
        assert_eq!(
            compare(
                &format!("{consts}use a::N;\npub fn f(_: Buf<N>) {{}}\n"),
                &format!("{consts}use b::N;\npub fn f(_: Buf<N>) {{}}\n"),
                "f"
            ),
            Changed
        );
        // 囲む impl の境界
        let bounds = "pub trait A {}\npub trait B {}\npub struct Wrap<T>(pub T);\n";
        assert_eq!(
            compare(
                &format!(
                    "{bounds}use self::A as Bound;\nimpl<T: Bound> Wrap<T> {{\n    pub fn f(&self) {{}}\n}}\n"
                ),
                &format!(
                    "{bounds}use self::B as Bound;\nimpl<T: Bound> Wrap<T> {{\n    pub fn f(&self) {{}}\n}}\n"
                ),
                "f"
            ),
            Changed
        );
        // trait の関連型の境界
        assert_eq!(
            compare(
                "pub trait A {}\npub trait B {}\nuse self::A as Bound;\npub trait Api {\n    type Item: Bound;\n}\n",
                "pub trait A {}\npub trait B {}\nuse self::B as Bound;\npub trait Api {\n    type Item: Bound;\n}\n",
                "Api"
            ),
            Changed
        );
    }

    /// 前回のレビューの誤って blocking にする反例: 書き方だけの違いは同じ束縛。
    #[test]
    fn spelling_of_bindings_does_not_matter() {
        // 型別名の見える範囲・`self::`・`r#`
        assert_eq!(
            compare(
                "pub mod inner {\n    pub struct X;\n}\ntype Text = inner::X;\npub fn f(_: Text) {}\n",
                "pub mod inner {\n    pub struct X;\n}\npub(crate) type r#Text = self::inner::X;\npub fn f(_: Text) {}\n",
                "f"
            ),
            Same
        );
        // 明示の use と glob
        assert_eq!(
            compare(
                "mod a {\n    pub struct Text;\n}\nuse a::Text;\npub fn f(_: Text) {}\n",
                "mod a {\n    pub struct Text;\n}\nuse a::*;\npub fn f(_: Text) {}\n",
                "f"
            ),
            Same
        );
        // glob の並び順
        assert_eq!(
            compare(
                "use std::cmp::*;\nuse std::path::*;\npub fn f(_: String) {}\n",
                "use std::path::*;\nuse std::cmp::*;\npub fn f(_: String) {}\n",
                "f"
            ),
            Same
        );
        // use の module 部分の `r#`
        assert_eq!(
            compare(
                "mod inner {\n    pub struct Text;\n}\nuse inner::Text;\npub fn f(_: Text) {}\n",
                "mod inner {\n    pub struct Text;\n}\nuse r#inner::Text;\npub fn f(_: Text) {}\n",
                "f"
            ),
            Same
        );
        // `use … as` の中のコメント
        assert_eq!(
            compare(
                "use std::string::String as Text;\npub fn f(_: Text) {}\n",
                "use std::string::String as /* text */ Text;\npub fn f(_: Text) {}\n",
                "f"
            ),
            Same
        );
        // 関連型の境界の名前は束縛でない
        assert_eq!(
            compare(
                "use std::path::PathBuf as Item;\npub fn f(_: impl Iterator<Item: Copy>) {}\n",
                "use std::string::String as Item;\npub fn f(_: impl Iterator<Item: Copy>) {}\n",
                "f"
            ),
            Same
        );
    }

    /// 2 回目のレビューの反例: 型の差し替え・定数の右辺の型引数・条件付きの module の別名。
    #[test]
    fn replacements_type_arguments_and_conditional_module_aliases() {
        // 型別名を外の型の取り込みに差し替えるのは、定義の移動でない
        assert_eq!(
            compare(
                "type Duration = u8;\npub fn f(_: Duration) {}\n",
                "use std::time::Duration;\npub fn f(_: Duration) {}\n",
                "f"
            ),
            Changed
        );
        // 元からあった別の型への付け替えも移動でない
        let shapes =
            "pub mod a {\n    pub struct Shape;\n}\npub mod b {\n    pub struct Shape;\n}\n";
        assert_eq!(
            compare(
                &format!("{shapes}use a::Shape;\npub fn f(_: Shape) {{}}\n"),
                &format!("{shapes}use b::Shape;\npub fn f(_: Shape) {{}}\n"),
                "f"
            ),
            Changed
        );
        // 同じファイルの別の module へ型別名を移すのは同じ。移すときに右辺を変えれば変化
        let moved = |text: &str| {
            format!(
                "pub mod a {{}}\npub mod b {{\n    pub type Text = {text};\n}}\nuse b::Text;\npub fn f(_: Text) {{}}\n"
            )
        };
        let old = "pub mod a {\n    pub type Text = u8;\n}\npub mod b {}\nuse a::Text;\npub fn f(_: Text) {}\n";
        assert_eq!(compare(old, &moved("u8"), "f"), Same);
        assert_eq!(compare(old, &moved("u16"), "f"), Changed);
        // 定数の右辺の型引数
        assert_eq!(
            compare(
                "type T = u8;\nconst N: usize = std::mem::size_of::<T>();\npub fn f(_: [u8; N]) {}\n",
                "type T = u16;\nconst N: usize = std::mem::size_of::<T>();\npub fn f(_: [u8; N]) {}\n",
                "f"
            ),
            Changed
        );
        // 条件付きの module の別名の先の型別名
        let aliased = |text: &str| {
            format!(
                "pub mod a {{\n    pub type Text = {text};\n}}\n#[cfg(unix)]\nuse a as chosen;\npub fn f(_: chosen::Text) {{}}\n"
            )
        };
        assert_eq!(compare(&aliased("u8"), &aliased("u16"), "f"), Changed);
        assert_eq!(compare(&aliased("u8"), &aliased("u8"), "f"), Same);
        // 条件付きの module の別名を経由した use
        let imported = |text: &str| {
            format!(
                "pub mod a {{\n    pub type Text = {text};\n}}\n#[cfg(unix)]\nuse a as chosen;\n#[cfg(unix)]\nuse chosen::Text;\npub fn f(_: Text) {{}}\n"
            )
        };
        assert_eq!(compare(&imported("u8"), &imported("u16"), "f"), Changed);
    }

    /// 3 回目のレビューの反例: 条件は属性の論理積なので、同じ条件の重なりと属性の順序は識別に入れない。
    #[test]
    fn repeated_and_reordered_conditions_are_the_same() {
        let module = "pub mod a {\n    pub type Text = u8;\n}\n";
        assert_eq!(
            compare(
                &format!(
                    "{module}#[cfg(unix)]\nuse a as chosen;\n#[cfg(unix)]\nuse chosen::Text;\n#[cfg(unix)]\npub fn f(_: Text) {{}}\n"
                ),
                &format!(
                    "{module}#[cfg(unix)]\nuse a::Text;\n#[cfg(unix)]\npub fn f(_: Text) {{}}\n"
                ),
                "f"
            ),
            Same
        );
        assert_eq!(
            compare(
                &format!(
                    "{module}#[cfg(unix)]\n#[cfg(feature = \"x\")]\nuse a::Text;\npub fn f(_: Text) {{}}\n"
                ),
                &format!(
                    "{module}#[cfg(feature = \"x\")]\n#[cfg(unix)]\nuse a::Text;\npub fn f(_: Text) {{}}\n"
                ),
                "f"
            ),
            Same
        );
        // 条件そのものを変えるのは変化
        assert_eq!(
            compare(
                &format!("{module}#[cfg(unix)]\nuse a::Text;\npub fn f(_: Text) {{}}\n"),
                &format!("{module}#[cfg(windows)]\nuse a::Text;\npub fn f(_: Text) {{}}\n"),
                "f"
            ),
            Changed
        );
        // `cfg_attr` の中の属性の引数の `#` では属性を分けない (引数の順序の変化は条件の変化)
        assert_eq!(
            compare(
                &format!(
                    "{module}#[cfg_attr(unix, pick(#a #b #end))]\nuse a::Text;\npub fn f(_: Text) {{}}\n"
                ),
                &format!(
                    "{module}#[cfg_attr(unix, pick(#b #a #end))]\nuse a::Text;\npub fn f(_: Text) {{}}\n"
                ),
                "f"
            ),
            Changed
        );
    }

    /// `use` と glob で、版の crate を開かずに字面で識別したファイル (`mod-file:`) を通る候補は、通る
    /// module の条件が欠け得るので、覆いの証明に使わない (#312 の 5 回目のレビュー)。
    #[test]
    fn candidates_through_unopened_files_do_not_prove_coverage() {
        let source = |base: &str| {
            format!(
                "type Base = {base};\n\nmod bridge {{\n    mod defaults {{\n        pub mod selected {{\n            pub type Text = crate::Base;\n        }}\n    }}\n    use self::defaults::*;\n    mod selected;\n    pub use self::selected::*;\n}}\n\n#[cfg(feature = \"x\")]\ntype Text = Base;\n#[cfg(not(feature = \"x\"))]\nuse self::bridge::Text;\n\n#[cfg(not(feature = \"x\"))]\npub fn run(_: Text) {{}}\n"
            )
        };
        assert_eq!(compare(&source("u8"), &source("u16"), "run"), Changed);
        // Issue #312 の再現 (この crate の中の定義だけで覆う) は絞る
        let issue = |wide: &str| {
            format!(
                "#[cfg(feature = \"wide\")]\ntype Text = {wide};\n#[cfg(not(feature = \"wide\"))]\ntype Text = u8;\n\n#[cfg(not(feature = \"wide\"))]\npub fn run(_: Text) {{}}\n"
            )
        };
        assert_eq!(compare(&issue("u32"), &issue("u64"), "run"), Same);
    }
}
