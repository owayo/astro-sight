//! 設定ごとの名前解決 (Issue #306・#318)。
//!
//! cfg の原子の真偽の組み合わせ (設定) 1 つについて、比べる名前の依存の範囲の (module, 名前) の束縛を、
//! 名前空間ごとに「未確定・無い・解けた (先と見える範囲)・曖昧・決められない」で求める (rustc の import の
//! 解決と同じく、未確定の named の取り込みがある名前空間では glob の結果を確定させない)。
//!
//! - 宣言と named の取り込み (explicit) が名前を作れば、それが束縛になる (glob を隠す)
//! - explicit が無ければ、glob の先の束縛のうち取り込む module から見えるものを持ち込む。同じ先は見える
//!   範囲を合わせ、違う先は曖昧にする。外部 crate・決められない先の glob、名前を列挙できない項目があれば
//!   決められない
//! - 決まるものを繰り返し決め、進まなくなったら、glob だけで回る循環を最小不動点で閉じる (先の決まった
//!   束縛を、循環の中で見える範囲を狭めながら持ち込む)。named の取り込みを含む循環は決められない
//! - module に公開の経路で届くかは、その module を型の名前空間で `pub` に束縛する (module, 名前) から、
//!   正に伝わる最小不動点で求める (確かに届く・届き得る)。名前を列挙できない項目 (マクロ) がその module の
//!   名前を字面に持てば、届き得るとする
//! - 名前を列挙できない項目や場所の分からない module のために決められない名前は、取り込みの作る名前と分けて
//!   [`State::Opaque`] にする (宣言の変化は宣言の報告に任せる)

use super::super::rust_cfg_predicate::Predicate;
use super::super::rust_dependencies::exports::{ProviderKind, narrower};
use super::index::{Entry, Index, Key, NAMESPACES, Place, visible};
use crate::engine::rust_edition::RustEdition;
use crate::engine::rust_use_tree::UsePathOrigin;
use std::collections::{BTreeMap, HashMap};

/// 束縛の先。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Target {
    /// module (crate root からの経路)。
    Module(Vec<String>),
    /// module でない宣言 (宣言した module, 名前, 名前を作るもの)。
    Item(Vec<String>, String, Entry),
    /// 外部 crate の項目 (経路)。
    External(Vec<String>),
}

/// 名前を作ったもの。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Via {
    /// 宣言か named の取り込み・`extern crate`。
    Entry(Entry),
    /// glob (スコープの添字, glob の添字)。
    Glob(usize, usize),
}

/// 解けた束縛。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Binding {
    pub(super) target: Target,
    /// 見える範囲 (`pub` は None)。
    pub(super) scope: Option<Vec<String>>,
    pub(super) via: Via,
    /// 取り込み (named・glob) で作った名前か (宣言なら偽)。
    pub(super) imported: bool,
}

/// 名前空間の束縛の状態。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum State {
    Pending,
    Absent,
    Resolved(Binding),
    Ambiguous,
    Unknown,
    /// 束縛を決められないが、`pub` ではない (非公開の取り込みの、外部 crate・決められない先)。
    Hidden,
    /// 名前を列挙できない項目 (マクロ) か、場所の分からない module のために決められない (取り込みではない)。
    Opaque,
    /// 外部 crate の項目 (経路) を、その項目がこの名前空間に有れば束縛する (見える範囲)。旧新で同じなら同じ。
    Foreign(Vec<String>, Option<Vec<String>>),
}

/// module の公開の到達性を決めるもの。
#[derive(Clone, Default)]
pub(super) struct Binders {
    /// module → それを型の名前空間で束縛し得る (module, 名前)。
    pub(super) keys: HashMap<Vec<String>, Vec<Key>>,
    /// module → それを公開し得る、名前を列挙できない項目 (module, スコープの添字, 添字)。
    pub(super) opaque: HashMap<Vec<String>, Vec<Place>>,
}

/// 設定ごとに評価する、比べる範囲の条件 (旧新で原子を共有する)。
#[derive(Default)]
pub(super) struct Conditions {
    /// (module, スコープの添字) → 置き場に着く条件 (OR。読めない節は None)。
    pub(super) placed: HashMap<(Vec<String>, usize), Vec<Option<Predicate>>>,
    /// (module, スコープの添字, 種類 (0: 名前を作るもの, 1: glob, 2: 列挙できない項目, 3: `#[macro_export]`),
    /// 添字) → 自身の有る条件。
    pub(super) own: HashMap<(Vec<String>, usize, u8, usize), Option<Predicate>>,
    /// module → 中身を決められない条件 (場所の分からない宣言、読めない置き場。OR)。
    pub(super) uncertain: HashMap<Vec<String>, Vec<Option<Predicate>>>,
}

/// 3 値の AND。
fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

/// 3 値の OR (節の並び)。
fn any(clauses: &[Option<Predicate>], config: u64) -> Option<bool> {
    let mut result = Some(false);
    for clause in clauses {
        match clause {
            Some(predicate) if predicate.holds(config) => return Some(true),
            Some(_) => {}
            None => result = None,
        }
    }
    result
}

/// module に公開の経路で届くか (確かに届く, 届き得る)。
type Reach = (bool, bool);

/// 経路の module の部分を解いた結果。
enum ModuleOf {
    Module(Vec<String>),
    /// 外部 crate の module (別名を解いた経路。同じ字面の別名でも先が違えば違う)。
    External(Vec<String>),
    Pending,
    Unknown,
}

/// named の取り込みの先。
enum Import {
    Bound(Target, Option<Vec<String>>),
    /// 外部 crate の項目 (経路) の、名前空間の分からない束縛。
    Foreign(Vec<String>),
    /// この名前空間には名前を作らない。
    Missing,
    Pending,
    Unknown,
}

/// 循環を閉じるときに足す持ち込み ((名前, 名前空間), 先, 見える範囲, glob)。
type CandidateUpdate = ((Key, usize), Target, Option<Vec<String>>, Via);

/// 循環を閉じるときに足す外部の項目 ((名前, 名前空間), 経路, 見える範囲)。
type ForeignUpdate = ((Key, usize), Vec<String>, Option<Vec<String>>);

/// glob だけで回る循環を閉じるための、未確定の名前空間の中身。
struct Soft {
    candidates: BTreeMap<Target, (Option<Vec<String>>, Via)>,
    /// 名前空間の分からない外部の項目 (経路 → 見える範囲)。
    foreign: BTreeMap<Vec<String>, Option<Vec<String>>>,
    unknown: bool,
    /// 決められない持ち込みがあるが、どれも `pub` にならない。
    hidden: bool,
    /// 名前を列挙できない項目がこの名前を作り得る。
    opaque: bool,
    ambiguous: bool,
    /// 未確定の持ち込み元 (先の (module, 名前), glob の見える範囲, glob)。
    sources: Vec<(Key, Option<Vec<String>>, Via)>,
}

/// 1 つの版の、1 つの設定の名前解決。
pub(super) struct Resolver<'a, 'p> {
    index: &'a Index<'p>,
    conditions: &'a Conditions,
    keys: &'a [Key],
    binders: &'a Binders,
    config: u64,
    states: HashMap<Key, [State; 3]>,
    reach: HashMap<Vec<String>, Reach>,
}

impl<'a, 'p> Resolver<'a, 'p> {
    /// 版 `index` の設定 `config` で、`keys` (依存の先から並べたもの) を解き、`binders` の module の公開の
    /// 到達性を求める。
    pub(super) fn run(
        index: &'a Index<'p>,
        conditions: &'a Conditions,
        keys: &'a [Key],
        binders: &'a Binders,
        config: u64,
    ) -> Self {
        let pending = || [State::Pending, State::Pending, State::Pending];
        let mut resolver = Self {
            index,
            conditions,
            keys,
            binders,
            config,
            states: keys.iter().map(|key| (key.clone(), pending())).collect(),
            reach: HashMap::new(),
        };
        loop {
            let mut progress = false;
            for key in resolver.keys {
                for namespace in 0..NAMESPACES.len() {
                    if resolver.states[key][namespace] != State::Pending {
                        continue;
                    }
                    let next = resolver.compute(key, namespace);
                    if next != State::Pending {
                        if let Some(states) = resolver.states.get_mut(key) {
                            states[namespace] = next;
                        }
                        progress = true;
                    }
                }
            }
            if !progress && !resolver.close_cycles() {
                break;
            }
        }
        for states in resolver.states.values_mut() {
            for state in states.iter_mut() {
                if *state == State::Pending {
                    *state = State::Unknown;
                }
            }
        }
        resolver.compute_reach();
        resolver
    }

    /// (module, 名前) の名前空間 `namespace` (添字) の状態。比べる範囲に無ければ決められない。
    pub(super) fn state(&self, key: &Key, namespace: usize) -> &State {
        const UNKNOWN: State = State::Unknown;
        self.states
            .get(key)
            .map_or(&UNKNOWN, |states| &states[namespace])
    }

    /// module `module` に公開の経路で届くか。
    pub(super) fn reach(&self, module: &[String]) -> Reach {
        if module.is_empty() {
            return (true, true);
        }
        self.reach.get(module).copied().unwrap_or((false, false))
    }

    /// 名前 `key` が名前空間 `namespace` で crate の外から使えるか (決められなければ None)。
    pub(super) fn presence(&self, key: &Key, namespace: usize) -> Option<bool> {
        let (definite, possible) = self.reach(&key.0);
        match self.state(key, namespace) {
            State::Resolved(binding) => {
                if binding.scope.is_some() {
                    Some(false)
                } else if definite {
                    Some(true)
                } else if possible {
                    None
                } else {
                    Some(false)
                }
            }
            State::Absent | State::Hidden => Some(false),
            State::Foreign(_, scope) if scope.is_some() => Some(false),
            State::Pending
            | State::Ambiguous
            | State::Unknown
            | State::Opaque
            | State::Foreign(..) => (!possible).then_some(false),
        }
    }

    /// module `module` の名前を列挙できない項目 (スコープの添字, 添字) が有るか。
    pub(super) fn opaque_present(
        &self,
        module: &[String],
        scope_index: usize,
        position: usize,
    ) -> Option<bool> {
        self.own_present(module, scope_index, 2, position)
    }

    /// module `module` の glob (スコープの添字, 添字) が有るか。
    pub(super) fn glob_present(
        &self,
        module: &[String],
        scope_index: usize,
        position: usize,
    ) -> Option<bool> {
        self.own_present(module, scope_index, 1, position)
    }

    /// module `module` の glob の経路 `path` (起点 `origin`) の先。crate の module なら `Some(Some(module))`、
    /// 外部 crate なら `Some(None)`、決められなければ None。
    pub(super) fn glob_module(
        &self,
        module: &[String],
        path: &[String],
        origin: UsePathOrigin,
    ) -> Option<Option<Vec<String>>> {
        match self.module_of(module, path, origin) {
            ModuleOf::Module(target) => Some(Some(target)),
            ModuleOf::External(_) => Some(None),
            ModuleOf::Pending | ModuleOf::Unknown => None,
        }
    }

    /// 経路 `path` が、この設定で enum (variant の名前を持つ擬似的な module) か (その名前を束縛する宣言で決める。
    /// cfg で分けて module にもなる名前を混ぜない)。
    pub(super) fn is_enum(&self, path: &[String]) -> bool {
        self.enum_entry(path).is_some()
    }

    /// この設定で経路 `path` の名前を束縛する enum の宣言 (enum でなければ None)。
    pub(super) fn enum_entry(&self, path: &[String]) -> Option<Entry> {
        let (last, parent) = path.split_last()?;
        match self.state(&(parent.to_vec(), last.clone()), 0) {
            State::Resolved(Binding {
                via: Via::Entry(entry),
                ..
            }) if matches!(
                self.index.provider(parent, *entry).1.kind,
                ProviderKind::Declared("enum")
            ) =>
            {
                Some(*entry)
            }
            _ => None,
        }
    }

    /// 版の索引。
    pub(super) fn index(&self) -> &'a Index<'p> {
        self.index
    }

    /// 名前を作るもの・glob・列挙できない項目 (`kind`) が有るか。
    fn own_present(
        &self,
        module: &[String],
        scope_index: usize,
        kind: u8,
        position: usize,
    ) -> Option<bool> {
        let placed = self
            .conditions
            .placed
            .get(&(module.to_vec(), scope_index))
            .and_then(|clauses| any(clauses, self.config));
        let own = self
            .conditions
            .own
            .get(&(module.to_vec(), scope_index, kind, position))
            .and_then(|predicate| predicate.as_ref().map(|p| p.holds(self.config)));
        and(placed, own)
    }

    /// (module, 名前) の名前を作るもの `entry` が有るか。
    fn entry_present(&self, module: &[String], entry: Entry) -> Option<bool> {
        match entry {
            Entry::Provider(scope_index, position) => {
                self.own_present(module, scope_index, 0, position)
            }
            Entry::Exported(module_index, scope_index, position) => {
                let placed = self.index.exported_scope(module_index);
                self.own_present(placed, scope_index, 3, position)
            }
        }
    }

    /// module `module` の中身が、この設定で決められないか (Some(false) なら決められる)。
    pub(super) fn uncertain(&self, module: &[String]) -> Option<bool> {
        match self.conditions.uncertain.get(module) {
            None => Some(false),
            Some(clauses) => any(clauses, self.config),
        }
    }

    fn compute(&self, key: &Key, namespace: usize) -> State {
        let (module, name) = key;
        if self.uncertain(module) != Some(false) {
            return State::Opaque;
        }
        let bit = NAMESPACES[namespace];
        let mut explicit: Vec<Binding> = Vec::new();
        // 決められない explicit (有るか読めない、先を解けない) は、`pub` なら決められず、`pub` でなければ
        // 非公開の束縛か、無い (glob に任せる) かのどちらか。
        let (mut pending, mut unknown, mut hidden) = (false, false, false);
        let mut foreign: Vec<(Vec<String>, Option<Vec<String>>)> = Vec::new();
        for &entry in self.index.entries(key) {
            let (_, provider) = self.index.provider(module, entry);
            if provider.namespaces & bit == 0 {
                continue;
            }
            let mut undecided = || {
                if provider.scope.is_none() {
                    unknown = true;
                } else {
                    hidden = true;
                }
            };
            match self.entry_present(module, entry) {
                Some(false) => continue,
                None => {
                    undecided();
                    continue;
                }
                Some(true) => {}
            }
            match &provider.kind {
                ProviderKind::Declared(kind) => {
                    // enum は variant の名前を持つ擬似的な module としても経路に使う。
                    let target = if matches!(*kind, "module" | "enum") {
                        let mut child = module.clone();
                        child.push(name.clone());
                        Target::Module(child)
                    } else {
                        Target::Item(module.clone(), name.clone(), entry)
                    };
                    explicit.push(Binding {
                        target,
                        scope: provider.scope.clone(),
                        via: Via::Entry(entry),
                        imported: false,
                    });
                }
                ProviderKind::ExternCrate(crate_name) => explicit.push(Binding {
                    target: if crate_name == "self" {
                        Target::Module(Vec::new())
                    } else {
                        Target::External(vec![crate_name.clone()])
                    },
                    scope: provider.scope.clone(),
                    via: Via::Entry(entry),
                    imported: true,
                }),
                ProviderKind::Use {
                    module: path,
                    item,
                    origin,
                } => match self.import(module, path, item, *origin, namespace) {
                    Import::Bound(target, target_scope) => explicit.push(Binding {
                        target,
                        scope: narrower(provider.scope.clone(), target_scope),
                        via: Via::Entry(entry),
                        imported: true,
                    }),
                    Import::Foreign(path) => foreign.push((path, provider.scope.clone())),
                    Import::Missing => {}
                    Import::Pending => pending = true,
                    Import::Unknown => undecided(),
                },
            }
        }
        if !explicit.is_empty() {
            // 決められない `pub` の explicit (`cfg_attr` で export するマクロなど) が同じ項目を公開し得る
            // なら、`pub` でない束縛は決められない。
            return match merge_explicit(explicit) {
                State::Resolved(binding) if unknown && binding.scope.is_some() => State::Unknown,
                merged => merged,
            };
        }
        if pending {
            return State::Pending;
        }
        if unknown {
            return State::Unknown;
        }
        let globbed = match self.soft(key, namespace) {
            None => return State::Pending,
            Some(soft) if !soft.sources.is_empty() => return State::Pending,
            Some(soft) => soft.finish(),
        };
        let not_public = |state: &State| match state {
            State::Absent | State::Hidden => true,
            State::Resolved(binding) => binding.scope.is_some(),
            _ => false,
        };
        // 外部の項目がこの名前空間に有ればそれが名前を作り (glob を隠す)、無ければ glob の結果。
        if let Some((path, scope)) = foreign.first().cloned() {
            let same = foreign
                .iter()
                .all(|other| other.0 == path && other.1 == scope);
            return match globbed {
                State::Absent if same && !hidden => State::Foreign(path, scope),
                // glob が同じ外部の項目を持ち込むなら、項目が無い設定では glob も何も持ち込まない。
                State::Foreign(ref globbed_path, _) if same && !hidden && *globbed_path == path => {
                    State::Foreign(path, scope)
                }
                ref other
                    if scope.is_some()
                        && not_public(other)
                        && foreign.iter().all(|f| f.1.is_some()) =>
                {
                    State::Hidden
                }
                _ => State::Unknown,
            };
        }
        if !hidden {
            return globbed;
        }
        // 非公開の explicit が名前を作るなら非公開、作らないなら glob の結果。glob の結果も `pub` でなければ
        // どちらでも `pub` ではない。
        if not_public(&globbed) {
            State::Hidden
        } else {
            State::Unknown
        }
    }

    /// glob で持ち込むもの (explicit の無い名前空間)。glob の先の module が未確定なら None。
    fn soft(&self, key: &Key, namespace: usize) -> Option<Soft> {
        let (module, name) = key;
        let mut soft = Soft {
            candidates: BTreeMap::new(),
            foreign: BTreeMap::new(),
            unknown: false,
            hidden: false,
            opaque: false,
            ambiguous: false,
            sources: Vec::new(),
        };
        for (scope_index, position, glob) in self.index.globs(module) {
            // glob は自分の経路の先頭の名前を型の名前空間に持ち込まない (持ち込めば自分の解決に依る。Rust では
            // そうならない)。経路は型の名前空間で解くので、値・マクロの名前空間には持ち込み得る
            // (`mod foo { pub fn foo() {} } pub use foo::*;` の関数 `foo`)。
            let head_here = match glob.origin {
                UsePathOrigin::Bare => {
                    self.bare_scope(module).as_deref() == Some(module.as_slice())
                }
                UsePathOrigin::Global => {
                    self.index.edition == Some(RustEdition::E2015) && module.is_empty()
                }
                UsePathOrigin::Anchored => module.is_empty(),
            };
            if namespace == 0 && head_here && glob.module.first() == Some(name) {
                continue;
            }
            // `pub` でない glob の決められない持ち込みは、`pub` の名前を作らない。
            let public = glob.scope.is_none();
            let undecided = |soft: &mut Soft| {
                if public {
                    soft.unknown = true;
                } else {
                    soft.hidden = true;
                }
            };
            let via = Via::Glob(scope_index, position);
            // 先の名前がこの名前空間に無ければ、glob の有無を問わず何も持ち込まない (glob の条件は、名前を
            // 持ち込み得るときだけ組んである)。
            let found = self.module_of(module, &glob.module, glob.origin);
            if let ModuleOf::Module(target) = &found
                && *self.state(&(target.clone(), name.clone()), namespace) == State::Absent
            {
                continue;
            }
            match self.own_present(module, scope_index, 1, position) {
                Some(false) => continue,
                None => {
                    undecided(&mut soft);
                    continue;
                }
                Some(true) => {}
            }
            match found {
                ModuleOf::Module(target) => {
                    let source = (target, name.clone());
                    match self.state(&source, namespace) {
                        State::Resolved(binding) => {
                            if visible(&binding.scope, module) {
                                soft.add(
                                    binding.target.clone(),
                                    narrower(binding.scope.clone(), glob.scope.clone()),
                                    via,
                                );
                            }
                        }
                        // 名前空間の分からない外部の項目は、そのまま持ち込む (項目が無ければ何も持ち込まない)。
                        State::Foreign(path, scope) => {
                            if visible(scope, module) {
                                soft.add_foreign(
                                    path.clone(),
                                    narrower(scope.clone(), glob.scope.clone()),
                                );
                            }
                        }
                        State::Absent => {}
                        State::Pending => soft.sources.push((source, glob.scope.clone(), via)),
                        State::Ambiguous => soft.ambiguous = true,
                        State::Unknown | State::Opaque => undecided(&mut soft),
                        State::Hidden => soft.hidden = true,
                    }
                }
                ModuleOf::External(_) | ModuleOf::Unknown => undecided(&mut soft),
                ModuleOf::Pending => return None,
            }
        }
        // 項目の位置のマクロは、字面に名前があれば、その名前を作り得る (glob を隠す名前も)。
        if self
            .index
            .opaque_affecting(module, name)
            .into_iter()
            .any(|(scope_index, position)| {
                self.own_present(module, scope_index, 2, position) != Some(false)
            })
        {
            soft.opaque = true;
        }
        Some(soft)
    }

    /// 進まなくなった未確定の名前空間のうち、glob だけで回る循環を最小不動点で閉じる。閉じたものがあれば真。
    fn close_cycles(&mut self) -> bool {
        let mut soft: BTreeMap<(Key, usize), Soft> = BTreeMap::new();
        for key in self.keys {
            for namespace in 0..NAMESPACES.len() {
                if self.states[key][namespace] != State::Pending {
                    continue;
                }
                // explicit が未確定なら閉じない (glob を隠すかが決まらない)。
                if !self.explicit_settled(key, namespace) {
                    continue;
                }
                if let Some(found) = self.soft(key, namespace) {
                    soft.insert((key.clone(), namespace), found);
                }
            }
        }
        // 閉じられない未確定のものから持ち込むものは、閉じない。
        loop {
            let blocked: Vec<(Key, usize)> = soft
                .iter()
                .filter(|((_, namespace), found)| {
                    found
                        .sources
                        .iter()
                        .any(|(source, _, _)| !soft.contains_key(&(source.clone(), *namespace)))
                })
                .map(|(key, _)| key.clone())
                .collect();
            if blocked.is_empty() {
                break;
            }
            for key in blocked {
                soft.remove(&key);
            }
        }
        if soft.is_empty() {
            return false;
        }
        loop {
            let mut candidates: Vec<CandidateUpdate> = Vec::new();
            let mut foreign: Vec<ForeignUpdate> = Vec::new();
            let mut flags: Vec<((Key, usize), bool, bool, bool)> = Vec::new();
            for ((key, namespace), found) in &soft {
                for (source, glob_scope, via) in &found.sources {
                    let Some(from) = soft.get(&(source.clone(), *namespace)) else {
                        continue;
                    };
                    for (target, (scope, _)) in &from.candidates {
                        if visible(scope, &key.0) {
                            candidates.push((
                                (key.clone(), *namespace),
                                target.clone(),
                                narrower(scope.clone(), glob_scope.clone()),
                                *via,
                            ));
                        }
                    }
                    for (path, scope) in &from.foreign {
                        if visible(scope, &key.0) {
                            foreign.push((
                                (key.clone(), *namespace),
                                path.clone(),
                                narrower(scope.clone(), glob_scope.clone()),
                            ));
                        }
                    }
                    if from.unknown || from.ambiguous || from.hidden || from.opaque {
                        // `pub` でない glob の先の決められないものは、`pub` にならない。
                        let public = glob_scope.is_none();
                        let undecided = from.unknown || from.opaque;
                        flags.push((
                            (key.clone(), *namespace),
                            undecided && public,
                            from.ambiguous,
                            from.hidden || (undecided && !public),
                        ));
                    }
                }
            }
            let mut changed = false;
            for (slot, unknown, ambiguous, hidden) in flags {
                if let Some(found) = soft.get_mut(&slot) {
                    changed |= (unknown && !found.unknown)
                        || (ambiguous && !found.ambiguous)
                        || (hidden && !found.hidden);
                    found.unknown |= unknown;
                    found.ambiguous |= ambiguous;
                    found.hidden |= hidden;
                }
            }
            for (slot, target, scope, via) in candidates {
                if let Some(found) = soft.get_mut(&slot) {
                    changed |= found.add(target, scope, via);
                }
            }
            for (slot, path, scope) in foreign {
                if let Some(found) = soft.get_mut(&slot) {
                    changed |= found.add_foreign(path, scope);
                }
            }
            if !changed {
                break;
            }
        }
        for ((key, namespace), found) in soft {
            if let Some(states) = self.states.get_mut(&key) {
                states[namespace] = found.finish();
            }
        }
        true
    }

    /// (module, 名前) の名前空間の explicit (宣言・named の取り込み) が決まっていて、名前を作らないか。
    fn explicit_settled(&self, key: &Key, namespace: usize) -> bool {
        let (module, _) = key;
        let bit = NAMESPACES[namespace];
        for &entry in self.index.entries(key) {
            let (_, provider) = self.index.provider(module, entry);
            if provider.namespaces & bit == 0 || self.entry_present(module, entry) == Some(false) {
                continue;
            }
            match &provider.kind {
                ProviderKind::Use {
                    module: path,
                    item,
                    origin,
                } => {
                    if !matches!(
                        self.import(module, path, item, *origin, namespace),
                        Import::Missing
                    ) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }

    /// module `module` で書いた経路 `path` (起点 `origin`) の module。
    fn module_of(&self, module: &[String], path: &[String], origin: UsePathOrigin) -> ModuleOf {
        match origin {
            UsePathOrigin::Anchored => self.walk(Vec::new(), path),
            UsePathOrigin::Global => match self.index.edition {
                Some(RustEdition::E2015) => self.walk(Vec::new(), path),
                Some(RustEdition::E2018OrLater) => ModuleOf::External(path.to_vec()),
                None => ModuleOf::Unknown,
            },
            UsePathOrigin::Bare => {
                let Some(scope) = self.bare_scope(module) else {
                    return ModuleOf::Unknown;
                };
                let Some((head, rest)) = path.split_first() else {
                    return ModuleOf::Module(scope);
                };
                match self.state(&(scope, head.clone()), 0) {
                    State::Resolved(binding) => match &binding.target {
                        Target::Module(target) => self.walk(target.clone(), rest),
                        Target::External(external) => {
                            ModuleOf::External(external.iter().chain(rest).cloned().collect())
                        }
                        Target::Item(..) => ModuleOf::Unknown,
                    },
                    // スコープに無い先頭の名前は、2018 以降は外部 crate (extern prelude)。
                    State::Absent => {
                        if self.index.edition == Some(RustEdition::E2015) {
                            ModuleOf::Unknown
                        } else {
                            ModuleOf::External(path.to_vec())
                        }
                    }
                    State::Pending => ModuleOf::Pending,
                    State::Ambiguous
                    | State::Unknown
                    | State::Hidden
                    | State::Opaque
                    | State::Foreign(..) => ModuleOf::Unknown,
                }
            }
        }
    }

    /// module `module` で、先頭が識別子の経路を読むスコープ (2015 は crate root、2018 以降は今の module。
    /// crate root の外で edition が分からなければ None)。
    fn bare_scope(&self, module: &[String]) -> Option<Vec<String>> {
        match self.index.edition {
            _ if module.is_empty() => Some(Vec::new()),
            Some(RustEdition::E2015) => Some(Vec::new()),
            Some(RustEdition::E2018OrLater) => Some(module.to_vec()),
            None => None,
        }
    }

    /// module `start` から、名前 `rest` を型の名前空間の module として順にたどる。
    fn walk(&self, start: Vec<String>, rest: &[String]) -> ModuleOf {
        let mut current = start;
        for (position, segment) in rest.iter().enumerate() {
            match self.state(&(current.clone(), segment.clone()), 0) {
                State::Resolved(binding) => match &binding.target {
                    Target::Module(next) => current = next.clone(),
                    Target::External(external) => {
                        return ModuleOf::External(
                            external
                                .iter()
                                .chain(&rest[position + 1..])
                                .cloned()
                                .collect(),
                        );
                    }
                    Target::Item(..) => return ModuleOf::Unknown,
                },
                State::Pending => return ModuleOf::Pending,
                State::Absent
                | State::Ambiguous
                | State::Unknown
                | State::Hidden
                | State::Opaque
                | State::Foreign(..) => {
                    return ModuleOf::Unknown;
                }
            }
        }
        ModuleOf::Module(current)
    }

    /// module `module` の named の取り込み `path::item` (起点 `origin`) が名前空間 `namespace` に作る束縛。
    fn import(
        &self,
        module: &[String],
        path: &[String],
        item: &str,
        origin: UsePathOrigin,
        namespace: usize,
    ) -> Import {
        // 先の名前の無い取り込み (`use crate as x;`) は、経路の module そのもの。
        if item.is_empty() {
            return match self.module_of(module, path, origin) {
                ModuleOf::Module(target) if namespace == 0 => {
                    Import::Bound(Target::Module(target), None)
                }
                ModuleOf::External(external) if namespace == 0 => {
                    Import::Bound(Target::External(external), None)
                }
                ModuleOf::Module(_) | ModuleOf::External(_) => Import::Missing,
                ModuleOf::Pending => Import::Pending,
                ModuleOf::Unknown => Import::Unknown,
            };
        }
        // 一要素の取り込み (`use item as x;`) は、スコープの名前か外部 crate。
        if path.is_empty() && origin == UsePathOrigin::Bare {
            let Some(scope) = self.bare_scope(module) else {
                return Import::Unknown;
            };
            let key = (scope, item.to_owned());
            if !self.absent_everywhere(&key) {
                return self.lookup(&key, namespace);
            }
            // スコープに無い名前は、2018 以降は外部 crate (extern prelude)。
            return if self.index.edition == Some(RustEdition::E2015) {
                Import::Missing
            } else if namespace == 0 {
                Import::Bound(Target::External(vec![item.to_owned()]), None)
            } else {
                Import::Missing
            };
        }
        match self.module_of(module, path, origin) {
            ModuleOf::Module(target) => self.lookup(&(target, item.to_owned()), namespace),
            // 外部 crate の項目の名前空間は分からない (型には有るとみなす)。項目は別名を解いた経路で示す。
            ModuleOf::External(mut full) => {
                full.push(item.to_owned());
                if namespace == 0 {
                    Import::Bound(Target::External(full), None)
                } else {
                    Import::Foreign(full)
                }
            }
            ModuleOf::Pending => Import::Pending,
            ModuleOf::Unknown => Import::Unknown,
        }
    }

    /// crate の module の (module, 名前) の名前空間の束縛を取り込みの先として引く。crate の module の名前は
    /// 索引で全て分かる (分からない module は決められない状態になる) ので、無ければ取り込みは名前を作らない
    /// (この設定ではビルドできない)。
    fn lookup(&self, key: &Key, namespace: usize) -> Import {
        match self.state(key, namespace) {
            State::Resolved(binding) => {
                Import::Bound(binding.target.clone(), binding.scope.clone())
            }
            State::Pending => Import::Pending,
            State::Foreign(path, _) => Import::Foreign(path.clone()),
            State::Ambiguous | State::Unknown | State::Hidden | State::Opaque => Import::Unknown,
            State::Absent => {
                if (0..NAMESPACES.len()).any(|other| self.state(key, other) == &State::Pending) {
                    Import::Pending
                } else {
                    Import::Missing
                }
            }
        }
    }

    /// (module, 名前) がどの名前空間にも無いか。
    fn absent_everywhere(&self, key: &Key) -> bool {
        (0..NAMESPACES.len()).all(|namespace| self.state(key, namespace) == &State::Absent)
    }

    /// 束縛の決まった後で、比べる範囲の module の公開の到達性を求める。
    fn compute_reach(&mut self) {
        let mut modules: Vec<Vec<String>> = self.binders.keys.keys().cloned().collect();
        modules.extend(
            self.binders
                .opaque
                .keys()
                .filter(|module| !self.binders.keys.contains_key(*module))
                .cloned(),
        );
        for module in &modules {
            self.reach.insert(module.clone(), (false, false));
        }
        loop {
            let mut changed = false;
            for module in &modules {
                let mut next = self.reach(module);
                // マクロが字面に名前を持てば、その module を公開し得る。
                for (placed, scope_index, position) in
                    self.binders.opaque.get(module).into_iter().flatten()
                {
                    if self.own_present(placed, *scope_index, 2, *position) != Some(false) {
                        next.1 |= self.reach(placed).1;
                    }
                }
                for binder in self.binders.keys.get(module).into_iter().flatten() {
                    let (definite, possible) = self.reach(&binder.0);
                    match self.state(binder, 0) {
                        State::Resolved(binding) => {
                            if binding.target == Target::Module(module.clone())
                                && binding.scope.is_none()
                            {
                                next.0 |= definite;
                                next.1 |= possible;
                            }
                        }
                        State::Absent => {}
                        _ => next.1 |= possible,
                    }
                }
                if next != self.reach(module) {
                    self.reach.insert(module.clone(), next);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
}

impl Soft {
    /// 先 `target` を見える範囲 `scope` で足す (同じ先は広いほう)。変わったら真。
    fn add(&mut self, target: Target, scope: Option<Vec<String>>, via: Via) -> bool {
        match self.candidates.get_mut(&target) {
            None => {
                self.candidates.insert(target, (scope, via));
                true
            }
            Some((known, _)) => {
                let wider = wider(known.clone(), scope);
                if *known != wider {
                    *known = wider;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// 名前空間の分からない外部の項目 `path` を見える範囲 `scope` で足す (同じ項目は広いほう)。変わったら真。
    fn add_foreign(&mut self, path: Vec<String>, scope: Option<Vec<String>>) -> bool {
        match self.foreign.get_mut(&path) {
            None => {
                self.foreign.insert(path, scope);
                true
            }
            Some(known) => {
                let wider = wider(known.clone(), scope);
                if *known != wider {
                    *known = wider;
                    true
                } else {
                    false
                }
            }
        }
    }

    fn finish(self) -> State {
        if self.ambiguous || self.candidates.len() > 1 {
            return State::Ambiguous;
        }
        if self.unknown {
            return State::Unknown;
        }
        if self.opaque {
            // 先の決まった持ち込みと重なれば、マクロが隠すかもしれない (取り込みの名前)。
            return if self.candidates.is_empty() && self.foreign.is_empty() && !self.hidden {
                State::Opaque
            } else {
                State::Unknown
            };
        }
        if self.hidden {
            // 先の決まった持ち込みと重なれば曖昧かもしれないが、どれも `pub` でなければ `pub` ではない。
            return if self.candidates.values().all(|(scope, _)| scope.is_some())
                && self.foreign.values().all(Option::is_some)
            {
                State::Hidden
            } else {
                State::Unknown
            };
        }
        if !self.foreign.is_empty() {
            // 外部の項目がこの名前空間に有るかは分からないので、ほかの先と重なれば決められない。
            return match (self.candidates.is_empty(), self.foreign.len()) {
                (true, 1) => match self.foreign.into_iter().next() {
                    Some((path, scope)) => State::Foreign(path, scope),
                    None => State::Unknown,
                },
                _ => State::Unknown,
            };
        }
        match self.candidates.into_iter().next() {
            Some((target, (scope, via))) => State::Resolved(Binding {
                target,
                scope,
                via,
                imported: true,
            }),
            None => State::Absent,
        }
    }
}

/// 2 つの見える範囲の広いほう (共通の祖先)。
fn wider(a: Option<Vec<String>>, b: Option<Vec<String>>) -> Option<Vec<String>> {
    match (a, b) {
        (None, _) | (_, None) => None,
        (Some(a), Some(b)) => Some(
            a.iter()
                .zip(&b)
                .take_while(|(x, y)| x == y)
                .map(|(x, _)| x.clone())
                .collect(),
        ),
    }
}

/// explicit な束縛をまとめる (同じ先は広い見える範囲、違う先は曖昧)。
fn merge_explicit(bindings: Vec<Binding>) -> State {
    let mut merged: Option<Binding> = None;
    for binding in bindings {
        match &mut merged {
            None => merged = Some(binding),
            Some(known) if known.target == binding.target => {
                known.scope = wider(known.scope.clone(), binding.scope);
                known.imported &= binding.imported;
            }
            Some(_) => return State::Ambiguous,
        }
    }
    merged.map_or(State::Absent, State::Resolved)
}
