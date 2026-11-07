//! Rust の公開シンボルの契約が名指す名前の束縛を旧新で比べる (Issue #283)。
//!
//! 公開関数のシグネチャや公開構造体のフィールドの型が `use … as Text` の別名や非公開の
//! `type Text = …;` を名指すと、別名の指す先を変えてもシグネチャの字面は変わらない。宣言ごとに、
//! 契約 (関数の見出しと囲む impl の見出し、`pub` のフィールドの型、enum の variant の型、定数の型、
//! 型別名の右辺、trait の境界と関連型) が名指す名前 (ジェネリクスの引数と `Self` を除く) を、宣言を
//! 囲む module のスコープで解き、旧新で比べる。
//!
//! - 束縛は同じファイルの中で追う: 囲む inline module、`self::`・`super::`・`crate::`、明示の
//!   `use` (`as` の別名と module の別名を含む)、型別名と定数 (右辺の名前を宣言したスコープで解いて
//!   置き換えた並びで識別し、連鎖をたどる。右辺の型の名前は値の式の中でも型として解く)、同じ
//!   ファイルの中の module への glob、item 位置のマクロの呼び出し (呼び出しとファイルのマクロ定義の
//!   字面を識別に入れる)。cfg の付いた束縛 (`use`・module の別名・glob) は条件ごとの枝に分けて解き、
//!   枝の識別に条件を添える
//! - 同じスコープの構造体・enum などの宣言は「そこで宣言した」ことを識別にし、中身は入れない
//!   (型自身の変更は型の api.mod として出る)
//! - ファイルの外へ出るパス (ほかのファイルの module・外部 crate・prelude) は字面で比べる。ほかの
//!   ファイルの module は crate root からの経路にする
//! - 識別は束縛の先だけで作り、書き方 (明示の use か glob か、`self::` の有無、`r#`、見える範囲の
//!   指定、コメント、並び順) では変えない
//! - 旧新の両方に現れる名前の字面ごとに、解いた束縛を比べる。片方にしか無い名前は、契約の字面の
//!   変更そのものが説明する (透過的な別名の導入など) ので比べない
//! - 束縛が変わっても、同じ定義を移しただけ (名前・種類・中身が同じで、差分の中で元の場所から消して
//!   移した先に足した。移した先はこのファイルか、差分の中の同じ crate のファイル) なら変化にしない。
//!   外の型の取り込みへの差し替えや、元からあった別の定義への付け替えは変化

use super::*;
use std::collections::HashMap;

use crate::engine::rust_use_tree::{RustPubUseEdge, UsePathOrigin, expand_rust_use_tree_edges_ast};

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
}

/// 識別に右辺を使う宣言 (型別名・定数・static)。右辺はソースの範囲で持ち、解くときに読む。
#[derive(Debug)]
struct Definition {
    conditions: Vec<String>,
    /// 宣言のノードのバイトの範囲。
    range: (usize, usize),
}

/// 宣言した項目 (種類と条件)。中身は識別に入れない。
#[derive(Debug)]
struct ItemDeclaration {
    kind: &'static str,
    conditions: Vec<String>,
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
    /// 子の module (中身がこのファイルにあれば true)。
    modules: HashMap<String, bool>,
    uses: HashMap<String, Vec<UseTarget>>,
    globs: Vec<UseTarget>,
    /// item 位置のマクロの呼び出しと、展開できない `use` のトークン (どの名前も生成し得る)。
    macros: Vec<String>,
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
        for (name, inline) in other.modules {
            *self.modules.entry(name).or_default() |= inline;
        }
        for (name, uses) in other.uses {
            self.uses.entry(name).or_default().extend(uses);
        }
        self.globs.extend(other.globs);
        self.macros.extend(other.macros);
    }
}

/// 束縛の先。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    /// このファイルの module (中身がこのファイルにある)。
    Module(Vec<String>),
    /// 識別の字面。
    Identity(String),
}

/// 束縛の候補。条件 (cfg) の付いた束縛は、条件ごとの候補に分かれる。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Candidate {
    /// 候補の条件 (cfg のトークン)。外側の条件から順に並ぶ。
    conditions: Vec<String>,
    target: Target,
}

impl Candidate {
    fn new(conditions: &[String], target: Target) -> Self {
        Self {
            conditions: conditions.to_vec(),
            target,
        }
    }

    /// 条件を前置きした識別の字面。
    fn identity(&self) -> String {
        let identity = match &self.target {
            Target::Module(path) => format!("mod:{}", path.join("::")),
            Target::Identity(identity) => identity.clone(),
        };
        conditioned(&self.conditions, identity)
    }
}

/// 束縛を解いた結果 (候補の並び)。
#[derive(Debug)]
struct Resolved(Vec<Candidate>);

impl Resolved {
    /// 条件の無い 1 つの先。
    fn single(target: Target) -> Self {
        Self(vec![Candidate::new(&[], target)])
    }

    /// 各候補の条件の前に、外側の条件 `conditions` を足した候補。
    fn under(self, conditions: &[String]) -> Vec<Candidate> {
        self.0
            .into_iter()
            .map(|mut candidate| {
                let mut all = conditions.to_vec();
                all.append(&mut candidate.conditions);
                candidate.conditions = all;
                candidate
            })
            .collect()
    }
}

/// 候補の識別を並べ替えて `|` でつないだ字面。
fn joined_identity(candidates: &[Candidate]) -> String {
    let mut identities: Vec<String> = candidates.iter().map(Candidate::identity).collect();
    identities.sort();
    identities.dedup();
    identities.join("|")
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
    /// ファイルの module の crate の中の経路 (`src` 相対の位置から)。分からなければ None。
    file_module: Option<Vec<String>>,
    /// ファイルのマクロ定義 (`macro_rules!`) のトークン。マクロの呼び出しの識別に添える。
    macro_definitions: Vec<String>,
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
fn item_namespaces(kind: &str) -> &'static [Namespace] {
    match kind {
        // 単位・タプルの構造体は値の名前空間にも入る。
        "struct_item" => &[Namespace::Type, Namespace::Value],
        "enum_item" | "union_item" | "trait_item" => &[Namespace::Type],
        _ => &[Namespace::Value],
    }
}

/// 識別子の字面 (`r#` を外す)。
fn unraw(text: &str) -> &str {
    text.strip_prefix("r#").unwrap_or(text)
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

impl DependencyIndex {
    /// Rust のソース `source` (ファイル `file`、`dir` 相対) の索引。構文エラーなら None。
    pub(crate) fn new(source: &[u8], file: &str) -> Option<Self> {
        let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
        if tree.root_node().has_error() {
            return None;
        }
        let mut index = Self {
            scopes: HashMap::new(),
            file_module: source_module_path(file).map(|(_, module)| module),
            macro_definitions: Vec::new(),
            tree,
            source: source.to_vec(),
        };
        let root = index.tree.root_node();
        let mut scopes = Vec::new();
        let mut definitions = Vec::new();
        // 深い入れ子の inline module でもスタックを溢れさせないよう、明示のスタックで下りる。
        let mut pending = vec![(root, Vec::new(), Vec::new())];
        while let Some((scope, path, inherited)) = pending.pop() {
            pending.extend(collect_scope(
                scope,
                &index.source,
                path,
                &inherited,
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

    /// `start` (行, 列) に始まる宣言の契約が名指す名前の、字面ごとの束縛 (解けなければ None の
    /// 要素)。宣言を引けなければ None。
    fn declaration_dependencies(
        &self,
        start: (usize, usize),
        inline_module: &str,
    ) -> Option<HashMap<String, Vec<Option<String>>>> {
        let declaration = declaration_at(self.tree.root_node(), start)?;
        let scope: Vec<String> = if inline_module.is_empty() {
            Vec::new()
        } else {
            inline_module.split("::").map(str::to_owned).collect()
        };
        let mut found: HashMap<String, Vec<Option<String>>> = HashMap::new();
        for reference in contract_references(declaration, &self.source) {
            let identity = self.resolve_path(&scope, &reference, 0);
            let entry = found.entry(reference.text()).or_default();
            if !entry.contains(&identity) {
                entry.push(identity);
            }
        }
        for identities in found.values_mut() {
            identities.sort();
        }
        Some(found)
    }

    /// スコープ `scope` で、パス `reference` を解いた識別。解けなければ None。
    fn resolve_path(
        &self,
        scope: &[String],
        reference: &Reference,
        depth: usize,
    ) -> Option<String> {
        let (name, prefix) = reference.segments.split_last()?;
        if name.starts_with("<macro ") {
            return Some(format!(
                "{name}{SEPARATOR}{}",
                self.macro_definitions.join(" ")
            ));
        }
        let mut candidates = Vec::new();
        for branch in self.resolve_module_prefix(scope, prefix, depth)? {
            match branch.target {
                Target::Module(module) => candidates.extend(
                    self.resolve_name(&module, name, reference.namespace, depth)?
                        .under(&branch.conditions),
                ),
                Target::Identity(outside) => candidates.push(Candidate::new(
                    &branch.conditions,
                    Target::Identity(outside_path(&outside, std::slice::from_ref(name))),
                )),
            }
        }
        (candidates.len() <= MAX_CANDIDATES).then(|| joined_identity(&candidates))
    }

    /// スコープ `scope` からパスの module の部分 `segments` (`crate`・`self`・`super`・`::` で
    /// 始まり得る) を解いた枝 (このファイルの module か、外のパスの識別)。空ならそのスコープ。
    fn resolve_module_prefix(
        &self,
        scope: &[String],
        segments: &[String],
        depth: usize,
    ) -> Option<Vec<Candidate>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let outside =
            |identity: String| Some(vec![Candidate::new(&[], Target::Identity(identity))]);
        let Some((first, rest)) = segments.split_first() else {
            return Some(vec![Candidate::new(&[], Target::Module(scope.to_vec()))]);
        };
        match first.as_str() {
            "::" => outside(outside_path("global:", rest)),
            "crate" => self.walk_absolute(rest.to_vec(), depth),
            "self" => self.walk_modules(scope.to_vec(), rest, depth),
            "super" => {
                let supers = segments
                    .iter()
                    .take_while(|segment| segment.as_str() == "super")
                    .count();
                let rest = &segments[supers..];
                if supers <= scope.len() {
                    return self.walk_modules(scope[..scope.len() - supers].to_vec(), rest, depth);
                }
                // ファイルの外の祖先の module は、crate root からの経路にする。
                let up = supers - scope.len();
                match &self.file_module {
                    Some(file_module) if up <= file_module.len() => {
                        let mut absolute = file_module[..file_module.len() - up].to_vec();
                        absolute.extend(rest.iter().cloned());
                        self.walk_absolute(absolute, depth)
                    }
                    _ => outside(format!("outside:{}", segments.join("::"))),
                }
            }
            _ => self.walk_modules(scope.to_vec(), segments, depth),
        }
    }

    /// crate root からの module の経路 `absolute` をたどる。このファイルの中なら束縛でたどり、
    /// 外なら `crate:` の識別にする。
    fn walk_absolute(&self, absolute: Vec<String>, depth: usize) -> Option<Vec<Candidate>> {
        match &self.file_module {
            Some(file_module) if absolute.starts_with(file_module) => {
                self.walk_modules(Vec::new(), &absolute[file_module.len()..], depth)
            }
            _ => Some(vec![Candidate::new(
                &[],
                Target::Identity(outside_path("crate:", &absolute)),
            )]),
        }
    }

    /// module `module` から、module の名前の並び `segments` をたどった枝。別名 (`use a as m`) も
    /// 解き、条件付きの別名は条件ごとの枝に分ける。外へ出た枝は、残りの名前を足した識別にする。
    fn walk_modules(
        &self,
        module: Vec<String>,
        segments: &[String],
        depth: usize,
    ) -> Option<Vec<Candidate>> {
        let mut walking = vec![Candidate::new(&[], Target::Module(module))];
        let mut done = Vec::new();
        for (index, segment) in segments.iter().enumerate() {
            let mut next = Vec::new();
            for branch in walking {
                let Target::Module(module) = &branch.target else {
                    continue;
                };
                for candidate in self
                    .resolve_name_in(module, segment, Namespace::Type, depth + 1, true)?
                    .under(&branch.conditions)
                {
                    match candidate.target {
                        Target::Module(_) => next.push(candidate),
                        Target::Identity(identity) => done.push(Candidate {
                            conditions: candidate.conditions,
                            target: Target::Identity(outside_path(
                                &identity,
                                &segments[index + 1..],
                            )),
                        }),
                    }
                }
            }
            walking = next;
            if walking.len() + done.len() > MAX_CANDIDATES {
                return None;
            }
        }
        done.extend(walking);
        Some(done)
    }

    /// スコープ `scope` で名前 `name` を解く。明示の束縛が無ければ、同じファイルの中の glob、
    /// マクロ、外の glob、prelude の候補をまとめる。型の名前空間で見つからなければ、値の名前空間も
    /// 引く (const ジェネリクスの引数)。
    fn resolve_name(
        &self,
        scope: &[String],
        name: &str,
        namespace: Namespace,
        depth: usize,
    ) -> Option<Resolved> {
        self.resolve_name_in(scope, name, namespace, depth, false)
    }

    /// [`Self::resolve_name`] の本体。`module_segment` はパスの module の区切り (`std::path::X` の
    /// `std`) で、明示の束縛に無ければ外の crate の名前として識別する。module の区切りは glob から
    /// 引かない (glob の経路そのものを解くときに同じ glob を引き直して循環せず、外の glob やマクロの
    /// 変化を外のパスへ広げない。glob が module の名前を持ち込むことはまれ)。
    fn resolve_name_in(
        &self,
        scope: &[String],
        name: &str,
        namespace: Namespace,
        depth: usize,
        module_segment: bool,
    ) -> Option<Resolved> {
        if depth > MAX_DEPTH {
            return None;
        }
        let namespaces: &[Namespace] = match namespace {
            Namespace::Type => &[Namespace::Type, Namespace::Value],
            Namespace::Value => &[Namespace::Value],
        };
        for &namespace in namespaces {
            if let Some(found) = self.lookup(scope, name, namespace, depth)? {
                return Some(found);
            }
        }
        if module_segment {
            return Some(Resolved::single(Target::Identity(format!("extern:{name}"))));
        }
        let Some(bindings) = self.scopes.get(scope) else {
            return Some(Resolved::single(Target::Identity(format!(
                "outside:{}::{name}",
                scope.join("::")
            ))));
        };
        let mut certain = Vec::new();
        let mut uncertain = Vec::new();
        for glob in &bindings.globs {
            for branch in self.resolve_use_module(scope, glob, depth)? {
                let mut conditions = glob.conditions.clone();
                conditions.extend(branch.conditions);
                match branch.target {
                    Target::Module(module) => {
                        for &namespace in namespaces {
                            if let Some(found) = self.lookup(&module, name, namespace, depth + 1)? {
                                certain.extend(found.under(&conditions));
                                break;
                            }
                        }
                    }
                    Target::Identity(target) => uncertain.push(Candidate::new(
                        &conditions,
                        Target::Identity(format!("glob:{target}")),
                    )),
                }
            }
        }
        if !bindings.macros.is_empty() {
            uncertain.push(Candidate::new(
                &[],
                Target::Identity(format!(
                    "macros:{}{SEPARATOR}{}",
                    bindings.macros.join(" "),
                    self.macro_definitions.join(" ")
                )),
            ));
        }
        // 同じファイルの中の glob で見つかり、外の glob とマクロが無ければ、その先だけにする。
        if certain.is_empty() || !uncertain.is_empty() {
            certain.extend(uncertain);
            certain.push(Candidate::new(
                &[],
                Target::Identity(format!("prelude:{name}")),
            ));
        }
        (certain.len() <= MAX_CANDIDATES).then_some(Resolved(certain))
    }

    /// `use` の先の module の部分を解いた枝。
    fn resolve_use_module(
        &self,
        scope: &[String],
        target: &UseTarget,
        depth: usize,
    ) -> Option<Vec<Candidate>> {
        match target.origin {
            // crate root からの経路 (`crate`・`self`・`super` を解いた後)。
            UsePathOrigin::Anchored => self.walk_absolute(target.module.clone(), depth),
            // 2018 以降の識別子で始まる経路は、同じスコープの束縛 (子の module・別名) から読む。
            UsePathOrigin::Bare => self.walk_modules(scope.to_vec(), &target.module, depth),
            UsePathOrigin::Global => Some(vec![Candidate::new(
                &[],
                Target::Identity(outside_path("global:", &target.module)),
            )]),
        }
    }

    /// スコープ `scope` の明示の束縛 (宣言・型別名・定数・module・`use`) で名前 `name` を引く。
    /// 無ければ Some(None)、解けなければ None。
    fn lookup(
        &self,
        scope: &[String],
        name: &str,
        namespace: Namespace,
        depth: usize,
    ) -> Option<Option<Resolved>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let Some(bindings) = self.scopes.get(scope) else {
            return Some(None);
        };
        let place = scope.join("::");
        let mut found: Vec<Candidate> = Vec::new();
        if namespace == Namespace::Type {
            if let Some(&inline) = bindings.modules.get(name) {
                let mut path = scope.to_vec();
                path.push(name.to_owned());
                if inline && self.scopes.contains_key(&path) {
                    return Some(Some(Resolved::single(Target::Module(path))));
                }
                found.push(Candidate::new(
                    &[],
                    Target::Identity(format!("mod-file:{}", path.join("::"))),
                ));
            }
            for alias in bindings.aliases.get(name).into_iter().flatten() {
                let rendered = self.render_definition(scope, alias, depth + 1)?;
                found.push(Candidate::new(
                    &alias.conditions,
                    Target::Identity(format!("alias:{place}#{name}:{rendered}")),
                ));
            }
        } else {
            for constant in bindings.constants.get(name).into_iter().flatten() {
                let rendered = self.render_definition(scope, constant, depth + 1)?;
                found.push(Candidate::new(
                    &constant.conditions,
                    Target::Identity(format!("const:{place}#{name}:{rendered}")),
                ));
            }
        }
        for item in bindings.items.get(name).into_iter().flatten() {
            if item_namespaces(item.kind).contains(&namespace) {
                found.push(Candidate::new(
                    &item.conditions,
                    Target::Identity(format!("item:{place}#{name}:{}", item.kind)),
                ));
            }
        }
        for target in bindings.uses.get(name).into_iter().flatten() {
            let item = target.item.as_deref()?;
            // `use name;` は同じスコープの子の module か外部 crate (自分の束縛を引き直さない)。
            if target.origin == UsePathOrigin::Bare && target.module.is_empty() {
                let mut path = scope.to_vec();
                path.push(item.to_owned());
                let resolved = if self.scopes.contains_key(&path) {
                    Target::Module(path)
                } else {
                    Target::Identity(format!("extern:{item}"))
                };
                found.push(Candidate::new(&target.conditions, resolved));
                continue;
            }
            for branch in self.resolve_use_module(scope, target, depth + 1)? {
                let mut conditions = target.conditions.clone();
                conditions.extend(branch.conditions);
                match branch.target {
                    Target::Module(module) => {
                        // 同じスコープの同じ名前を指す use (宣言の再公開) は、宣言で表す。
                        if module == scope && item == name {
                            continue;
                        }
                        found.extend(
                            self.resolve_name(&module, item, namespace, depth + 1)?
                                .under(&conditions),
                        );
                    }
                    Target::Identity(prefix) => found.push(Candidate {
                        conditions,
                        target: Target::Identity(outside_path(&prefix, &[item.to_owned()])),
                    }),
                }
            }
        }
        if found.is_empty() {
            return Some(None);
        }
        (found.len() <= MAX_CANDIDATES).then_some(Some(Resolved(found)))
    }

    /// 型別名・定数の宣言 (スコープ `scope`) を、右辺の名前を解いた識別に置き換えた並びにする。
    /// 宣言の名前・見える範囲の指定・コメントは入れない。
    fn render_definition(
        &self,
        scope: &[String],
        definition: &Definition,
        depth: usize,
    ) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        let node = self
            .tree
            .root_node()
            .descendant_for_byte_range(definition.range.0, definition.range.1)?;
        let generics = generic_parameter_names(node, &self.source);
        let mut tokens = Vec::new();
        for (field, namespace) in [
            ("type_parameters", Namespace::Type),
            ("type", Namespace::Type),
            ("value", Namespace::Value),
        ] {
            if let Some(child) = node.child_by_field_name(field) {
                tokens.push(format!("<{field}>"));
                self.render(child, scope, namespace, &generics, depth, &mut tokens)?;
            }
        }
        Some(tokens.join(SEPARATOR))
    }

    /// `node` の字面を、名前を解いた識別に置き換えて `tokens` に足す。深い入れ子でもスタックを
    /// 溢れさせないよう、明示のスタックで前順にたどる。
    fn render(
        &self,
        node: tree_sitter::Node<'_>,
        scope: &[String],
        namespace: Namespace,
        generics: &[String],
        depth: usize,
        tokens: &mut Vec<String>,
    ) -> Option<()> {
        enum Work<'tree> {
            Node(tree_sitter::Node<'tree>, Namespace),
            Marker(&'static str),
        }
        let mut stack = vec![Work::Node(node, namespace)];
        while let Some(work) = stack.pop() {
            let (node, namespace) = match work {
                Work::Node(node, namespace) => (node, namespace),
                Work::Marker(marker) => {
                    tokens.push(marker.to_owned());
                    continue;
                }
            };
            // 型の名前は値の式の中 (`size_of::<T>()`) でも型の名前空間で解き、値の名前は値の
            // 位置でだけ解く。
            let reference_namespace = match node.kind() {
                "type_identifier" | "scoped_type_identifier" => Some(Namespace::Type),
                "identifier" | "scoped_identifier" if namespace == Namespace::Value => {
                    Some(Namespace::Value)
                }
                _ => None,
            };
            if let Some(reference_namespace) = reference_namespace
                && let Some(segments) = path_segments(node, &self.source)
            {
                if let Some(index) = generics.iter().position(|g| Some(g) == segments.first()) {
                    tokens.push(format!("<generic {index}>"));
                    if segments.len() > 1 {
                        tokens.push(segments[1..].join("::"));
                    }
                } else {
                    tokens.push(self.resolve_path(
                        scope,
                        &Reference {
                            segments,
                            namespace: reference_namespace,
                        },
                        depth + 1,
                    )?);
                }
                continue;
            }
            let children_namespace = match node.kind() {
                "line_comment" | "block_comment" => continue,
                "array_type" => {
                    if let Some(length) = node.child_by_field_name("length") {
                        stack.push(Work::Node(length, Namespace::Value));
                        stack.push(Work::Marker("<length>"));
                    }
                    if let Some(element) = node.child_by_field_name("element") {
                        stack.push(Work::Node(element, Namespace::Type));
                    }
                    continue;
                }
                "block" if namespace == Namespace::Type => Namespace::Value,
                _ => namespace,
            };
            if node.child_count() == 0 {
                push_tokens(node, &self.source, tokens);
                continue;
            }
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            stack.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|child| Work::Node(child, children_namespace)),
            );
        }
        Some(())
    }

    /// 旧い版 `self` の宣言 (`old`) と新しい版 `new_index` の宣言 (`new`) の契約が名指す名前の束縛を
    /// 比べる。旧新の両方に現れる名前だけを比べる。定義の移動は、差分の中のファイル `files` で
    /// 確かめる。
    pub(crate) fn compare(
        &self,
        old: &[RustDeclarationContext],
        new_index: &DependencyIndex,
        new: &[RustDeclarationContext],
        files: &DiffModules<'_>,
    ) -> DependencyComparison {
        let collect = |index: &DependencyIndex, declarations: &[RustDeclarationContext]| {
            let mut merged: HashMap<String, Vec<Option<String>>> = HashMap::new();
            for declaration in declarations {
                let found = index
                    .declaration_dependencies(declaration.start, &declaration.inline_module)?;
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
        let (Some(old_dependencies), Some(new_dependencies)) =
            (collect(self, old), collect(new_index, new))
        else {
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
                && !self.moved_definition(old_identities, new_index, new_identities, files)
            {
                return DependencyComparison::Changed;
            }
        }
        result
    }
}

/// 子の inline module の中身と、その経路と条件。
type ChildScope<'tree> = (tree_sitter::Node<'tree>, Vec<String>, Vec<String>);

impl DependencyIndex {
    /// スコープ `scope` に名前 `name` の定義 (宣言・型別名・定数) があるか。
    fn defines(&self, scope: &[String], name: &str) -> bool {
        self.scopes.get(scope).is_some_and(|bindings| {
            bindings.items.contains_key(name)
                || bindings.aliases.contains_key(name)
                || bindings.constants.contains_key(name)
        })
    }

    /// スコープ `scope` の名前 `name` の、条件の無いただ 1 つの定義 (宣言・型別名・定数) の識別
    /// ([`Self::lookup`] と同じ字面)。無いか、条件付きか、複数あれば None。
    fn sole_definition(&self, scope: &[String], name: &str) -> Option<String> {
        let bindings = self.scopes.get(scope)?;
        let place = scope.join("::");
        let mut found = Vec::new();
        for alias in bindings.aliases.get(name).into_iter().flatten() {
            let rendered = self.render_definition(scope, alias, 1)?;
            found.push((
                &alias.conditions,
                format!("alias:{place}#{name}:{rendered}"),
            ));
        }
        for constant in bindings.constants.get(name).into_iter().flatten() {
            let rendered = self.render_definition(scope, constant, 1)?;
            found.push((
                &constant.conditions,
                format!("const:{place}#{name}:{rendered}"),
            ));
        }
        for item in bindings.items.get(name).into_iter().flatten() {
            found.push((
                &item.conditions,
                format!("item:{place}#{name}:{}", item.kind),
            ));
        }
        match found.as_slice() {
            [(conditions, identity)] if conditions.is_empty() => Some(identity.clone()),
            _ => None,
        }
    }

    /// 識別 `identity` (この版で解いたもの) がただ 1 つの定義を指すなら、その場所と中身。差分の中の
    /// ほかのファイルの定義は、`files` の同じ版 (`old`) の索引で引く。
    fn located_definition(
        &self,
        identity: &str,
        files: &DiffModules<'_>,
        old: bool,
    ) -> Option<LocatedDefinition> {
        if let Some((place, name)) = local_definition(identity) {
            let scope = split_place(place);
            // 右辺が `|` を含み得るので、字面を分けずに引き直して 1 つの定義を指すことを確かめる。
            if self.sole_definition(&scope, name)? != identity {
                return None;
            }
            return Some(LocatedDefinition {
                place: DefinitionPlace::Local(scope),
                name: name.to_owned(),
                content: definition_content(identity)?,
            });
        }
        let (module, name) = self.crate_location(identity)?;
        let (index, scope) = files.module_index(&module, old)?;
        let definition = index.sole_definition(&scope, &name)?;
        Some(LocatedDefinition {
            place: DefinitionPlace::Crate(module),
            content: definition_content(&definition)?,
            name,
        })
    }

    /// 識別 `identity` が crate の中のほかのファイルの名前 (`crate:`・`mod-file:`) なら、その
    /// module の crate root からの経路と名前。
    fn crate_location(&self, identity: &str) -> Option<(Vec<String>, String)> {
        let mut path = if let Some(path) = identity.strip_prefix("crate:") {
            path_names(path)?
        } else {
            let mut path = self.file_module.clone()?;
            path.extend(path_names(identity.strip_prefix("mod-file:")?)?);
            path
        };
        let name = path.pop()?;
        Some((path, name))
    }

    /// 旧い束縛 `old` から新しい束縛 `new` への変化が、同じ定義を移しただけか。旧新がそれぞれ
    /// ただ 1 つの定義 (このファイルか、差分の中の同じ crate のファイル `files` のもの) を指し、
    /// 名前・種類・中身 (型別名と定数は右辺を解いた並び) が同じで、差分の中で旧い定義の場所から
    /// 消して新しい定義の場所に足したこと。外の型の取り込みへの差し替えや、元からあった別の定義への
    /// 付け替えは移動でない。移した定義そのものの変更は、その定義の変更として出る。
    fn moved_definition(
        &self,
        old: &[Option<String>],
        new_index: &DependencyIndex,
        new: &[Option<String>],
        files: &DiffModules<'_>,
    ) -> bool {
        let ([Some(old)], [Some(new)]) = (old, new) else {
            return false;
        };
        let (Some(from), Some(to)) = (
            self.located_definition(old, files, true),
            new_index.located_definition(new, files, false),
        ) else {
            return false;
        };
        from.name == to.name
            && from.content == to.content
            && from.place != to.place
            && !defined_at(&from, new_index, files, false)
            && !defined_at(&to, self, files, true)
    }
}

/// 定義の場所。
#[derive(Debug, PartialEq, Eq)]
enum DefinitionPlace {
    /// このファイルの module のスコープ。
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

/// 定義 `definition` の場所に、版 (`old`) で同じ名前の定義があるか。このファイルの場所は `index`
/// (その版のこのファイルの索引) で、ほかのファイルの場所は `files` で引く。その版の差分にその場所の
/// ファイルが無ければ (消した・移した) 無いとする。
fn defined_at(
    definition: &LocatedDefinition,
    index: &DependencyIndex,
    files: &DiffModules<'_>,
    old: bool,
) -> bool {
    match &definition.place {
        DefinitionPlace::Local(scope) => index.defines(scope, &definition.name),
        DefinitionPlace::Crate(module) => files
            .module_index(module, old)
            .is_some_and(|(other, scope)| other.defines(&scope, &definition.name)),
    }
}

/// このファイルの定義の識別 (`item:` / `alias:` / `const:`) の、場所と名前。
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
    index: std::cell::OnceCell<Option<DependencyIndex>>,
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
    fn module_index(
        &self,
        module: &[String],
        old: bool,
    ) -> Option<(&DependencyIndex, Vec<String>)> {
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
            .get_or_init(|| DependencyIndex::new(&read(&file.path)?, &file.path))
            .as_ref()?;
        Some((index, module[file.module.len()..].to_vec()))
    }
}

/// module のスコープ `scope` (ファイルの `source_file` か inline module の `declaration_list`) の
/// 束縛を集め、子の inline module を返す。`inherited` は囲む module の条件。
#[allow(clippy::too_many_arguments)]
fn collect_scope<'tree>(
    scope: tree_sitter::Node<'tree>,
    source: &[u8],
    path: Vec<String>,
    inherited: &[String],
    file_module: Option<&[String]>,
    scopes: &mut Vec<(Vec<String>, ScopeBindings)>,
    definitions: &mut Vec<String>,
) -> Vec<ChildScope<'tree>> {
    let mut bindings = ScopeBindings::default();
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
                .map(|name| unraw(name).to_owned())
        };
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
                let unraw_all =
                    |module: Vec<String>| module.iter().map(|s| unraw(s).to_owned()).collect();
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
                            .entry(unraw(&exported_name).to_owned())
                            .or_default()
                            .push(UseTarget {
                                module: unraw_all(target_module),
                                item: Some(unraw(&target_item).to_owned()),
                                origin,
                                conditions: conditions.clone(),
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
                        }),
                    }
                }
            }
            "extern_crate_declaration" => {
                let crate_name = item
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                    .map(|name| unraw(name).to_owned());
                let bound = item
                    .child_by_field_name("alias")
                    .and_then(|alias| alias.utf8_text(source).ok())
                    .map(|alias| unraw(alias).to_owned())
                    .or_else(|| crate_name.clone());
                if let (Some(crate_name), Some(bound)) = (crate_name, bound) {
                    bindings.uses.entry(bound).or_default().push(UseTarget {
                        module: Vec::new(),
                        item: Some(crate_name),
                        origin: UsePathOrigin::Global,
                        conditions: conditions(),
                    });
                }
            }
            "type_item" => {
                if let Some(name) = name() {
                    bindings.aliases.entry(name).or_default().push(Definition {
                        conditions: conditions(),
                        range: (item.start_byte(), item.end_byte()),
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
                            range: (item.start_byte(), item.end_byte()),
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
                    bindings
                        .items
                        .entry(name)
                        .or_default()
                        .push(ItemDeclaration {
                            kind,
                            conditions: conditions(),
                        });
                }
            }
            "mod_item" => {
                let Some(name) = name() else {
                    continue;
                };
                let body = item.child_by_field_name("body");
                *bindings.modules.entry(name.clone()).or_default() |= body.is_some();
                if let Some(body) = body {
                    let mut child = path.clone();
                    child.push(name);
                    let mut child_conditions = conditions();
                    child_conditions.extend(inner_conditions(body, source));
                    children.push((body, child, child_conditions));
                }
            }
            "macro_invocation" | "expression_statement" => {
                bindings.macros.extend(tokens_of(item, source));
            }
            "macro_definition" => definitions.extend(tokens_of(item, source)),
            _ => {}
        }
    }
    scopes.push((path, bindings));
    children
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
    let push = |node: Option<tree_sitter::Node<'_>>, references: &mut Vec<Reference>| {
        if let Some(node) = node {
            collect_references(node, source, Namespace::Type, references);
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
            names.push(unraw(text).to_owned());
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
                segments.push(unraw(current.utf8_text(source).ok()?).to_owned());
                break;
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let name = current
                    .child_by_field_name("name")?
                    .utf8_text(source)
                    .ok()?;
                segments.push(unraw(name).to_owned());
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

/// `node` の中の、型 (と配列の長さ・const ジェネリクスの引数の値) として名指す名前を集める。
fn collect_references(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    namespace: Namespace,
    references: &mut Vec<Reference>,
) {
    // 深い入れ子の型 (`Option<Option<…>>`) でもスタックを溢れさせないよう、明示のスタックでたどる。
    let mut stack = vec![(node, namespace)];
    while let Some((node, namespace)) = stack.pop() {
        match node.kind() {
            // 型の名前は値の式の中 (`{ size_of::<T>() }`) でも型の名前空間で引く。
            "type_identifier" | "scoped_type_identifier" => {
                if let Some(segments) = path_segments(node, source) {
                    references.push(Reference {
                        segments,
                        namespace: Namespace::Type,
                    });
                } else if let Some(path) = node.child_by_field_name("path") {
                    // `<() as Selected>::Item`・`Vec<T>::Assoc` は、末尾の関連型の名前でなく、経路の
                    // 中の名前を集める。
                    stack.push((path, Namespace::Type));
                }
                continue;
            }
            "identifier" | "scoped_identifier" if namespace == Namespace::Value => {
                if let Some(segments) = path_segments(node, source) {
                    references.push(Reference {
                        segments,
                        namespace,
                    });
                }
                continue;
            }
            "macro_invocation" => {
                // 型の位置のマクロは、呼び出しとファイルのマクロ定義の字面で識別する。
                references.push(Reference {
                    segments: vec![format!("<macro {}>", tokens_of(node, source).join(" "))],
                    namespace,
                });
                continue;
            }
            "lifetime" | "line_comment" | "block_comment" => continue,
            "array_type" => {
                if let Some(element) = node.child_by_field_name("element") {
                    stack.push((element, Namespace::Type));
                }
                if let Some(length) = node.child_by_field_name("length") {
                    stack.push((length, Namespace::Value));
                }
                continue;
            }
            "type_binding" => {
                // `Item = Text` の `Item` は関連型の名前で、束縛ではない。
                if let Some(ty) = node.child_by_field_name("type") {
                    stack.push((ty, namespace));
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
                    stack.push((*child, namespace));
                }
                continue;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(
            node.named_children(&mut cursor)
                .map(|child| (child, namespace)),
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
                            module.push(unraw(text).to_owned());
                        }
                        ancestor = parent.parent();
                    }
                    module.reverse();
                    return RustDeclarationContext {
                        inline_module: module.join("::"),
                        owner: Vec::new(),
                        conditions: Vec::new(),
                        start: (start.row, start.column),
                    };
                }
                let mut cursor = node.walk();
                let children: Vec<_> = node.named_children(&mut cursor).collect();
                stack.extend(children.into_iter().rev());
            }
            panic!("declaration {name}");
        };
        let files = DiffModules::new("src/lib.rs", &[], Box::new(|_| None), Box::new(|_| None));
        index(old).compare(
            &[declaration(old)],
            &index(new),
            &[declaration(new)],
            &files,
        )
    }

    use DependencyComparison::{Changed, Same};

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
}
