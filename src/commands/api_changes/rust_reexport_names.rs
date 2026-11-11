//! 再公開した名前の有る設定を比べる (Issue #306・#318)。
//!
//! `pub use detail::make;` が作る公開の名前 (`crate::make`) は公開シンボルとして抽出しないので、`pub use`
//! に `#[cfg]` を足す・消す・公開範囲を狭めても `api_changes` に出なかった。名前を作る入力 (宣言・`use`・
//! glob の名前・名前空間・見える範囲・有る条件、項目の位置のマクロの字面、module の置き場、edition) が差分で
//! 変わった lib crate で、旧新の版の置き場 (#305) から名前を作るものの索引 ([`index`]) を作り、変わった名前と、
//! それを取り込んで作る名前 (named の取り込み、glob、経路の名前) を集める。依存の範囲が旧新で同じ名前は
//! 比べない。
//!
//! 比べる名前は、依存の範囲に現れる cfg の原子の真偽の組み合わせ (設定) ごとに、旧新の版で名前解決し
//! ([`resolve`])、名前空間ごとに「crate の外から使えるか」(module に公開の経路で届き、名前の束縛が `pub`)
//! を比べる。
//!
//! - 取り込み (named・glob) が作る名前だけを報告する (宣言そのものの変化は宣言の報告に任せる)
//! - 消える設定は削除と同じ区分、増える設定は追加 (#291 と同じ)。新版でどの設定にも無ければ
//!   `cfg_availability` の無いふつうの削除、旧版でどの設定にも無ければふつうの追加。条件の表示は原子の
//!   積の和 ([`display`])
//! - 決められない設定がある (外部 crate・決められない先の glob、項目の位置のマクロ、場所の分からない
//!   module、曖昧な glob、解けない循環、読めない属性) か、module の名前の指す module が変わったら、
//!   `rust_cfg_availability_unverified` の blocking な変更にする
//! - 列挙できない持ち込みの名前が、変化によって外へ出入りし得るとき、名前・依存の範囲が上限を超えたときは、
//!   探し切れなかったこと (`rust_reexports_incomplete`) にする

mod display;
mod index;
mod resolve;

use super::rust_cfg_predicate::{Atoms, Predicate};
use super::rust_dependencies::exports::{
    ProviderKind, glob_entry, macro_entry, macro_order_entry, opaque_entry, provider_entry,
};
use super::rust_dependencies::placements::Clause;
use super::*;
use crate::engine::rust_use_tree::UsePathOrigin;
use crate::models::review::CfgAvailability;
use index::{CallKey, Entry, ExportsCache, Hole, Index, Key, NAMESPACES, Place, User};
use resolve::{Binders, Binding, Conditions, Resolver, State, Target, Via};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// 比べる名前の上限 (再公開をたどって増やす)。超えれば探し切れなかったとする。
const MAX_NAMES: usize = 10_000;
/// 依存の範囲の (module, 名前) の上限。
const MAX_KEYS: usize = 200_000;
/// 設定ごとに解く量 (設定の数 × 依存の範囲の名前の数) の上限。超えれば名前ごとに分けて解く。
const MAX_WORK: usize = 4_000_000;

/// 宣言 (ファイル・inline module (`a::b`。ファイルの module の直下なら None)・名前・種類 (API の候補の種類の
/// 字面。`fn E` と `enum E` のように名前空間の違う同じ名前の宣言を区別する))。
pub(crate) type DeclarationId = (String, Option<String>, String, &'static str);

/// 宣言の種類 (`Declared` の字面) を、API の候補の種類の字面にする (`static` は `constant`。候補にならない
/// マクロはそのまま)。
fn candidate_kind(kind: &'static str) -> &'static str {
    match kind {
        "static" => "constant",
        other => other,
    }
}

/// 比べ直した結果の 1 つ。
pub(crate) enum NameOutcome {
    /// 有る設定の変化 (旧新のどちらにも有る設定がある)。
    Availability {
        candidate: ApiSymbolCandidate,
        lost: bool,
        gained: bool,
        availability: CfgAvailability,
    },
    /// 新版のどの設定にも無くなった (ふつうの削除)。旧版で名前の指していた宣言を添える。
    Removed(ApiSymbolCandidate, Vec<DeclarationId>),
    /// 旧版のどの設定にも無かった (ふつうの追加)。新版で名前の指す宣言を添える。
    Added(ApiSymbolCandidate, Vec<DeclarationId>),
    /// 比べられない。
    Unverified {
        candidate: ApiSymbolCandidate,
        old_signature: String,
    },
}

/// 再公開した名前を比べ直した結果。
#[derive(Default)]
pub(crate) struct ReexportNames {
    outcomes: Vec<NameOutcome>,
    pub(crate) truncations: Vec<crate::models::truncation::TruncationInfo>,
    names: Vec<String>,
    /// 比べ切った crate root (上限や読めない置き場で止めていない。Issue #314)。
    completed: HashSet<String>,
    /// (crate root, 名前) のうち、どこかの設定で型の名前空間の束縛が取り込みで、比べたもの (報告したか、
    /// 公開の有無と先が旧新で同じ。Issue #314)。
    rebound: HashSet<(String, Key)>,
    /// (crate root, 名前) のうち、上限で比べられなかったもの (Issue #314)。
    unresolved: HashSet<(String, Key)>,
}

/// 探し切れなかったことの申告。
fn incomplete(root: &str, reason: &str) -> crate::models::truncation::TruncationInfo {
    crate::models::truncation::TruncationInfo::new(
        Some(root.to_owned()),
        crate::models::truncation::TruncationReason::RustReexportsIncomplete,
        format!("names re-exported by changed Rust items were not all compared: {reason}"),
    )
}

/// 列挙できない glob の変化の申告の理由。
const UNLISTED: &str = "a change may move names that could not be listed across the crate boundary";

/// 版の、ファイル・inline module ごとの名前を作るもの・glob・列挙できない項目・マクロの本体の字面。
type ScopeEntries = HashMap<(String, Vec<String>), HashSet<String>>;

/// 版 `version` の、ファイル `paths` のスコープごとの字面。
fn scope_entries(version: &Index<'_>, paths: &HashSet<&str>) -> ScopeEntries {
    let mut result: ScopeEntries = HashMap::new();
    for path in paths {
        for (module, scope_index) in version.by_file.get(*path).into_iter().flatten() {
            let scope = &version.scopes(module)[*scope_index];
            let entries = result
                .entry(((*path).to_owned(), scope.inline.clone()))
                .or_default();
            entries.extend(scope.exports.providers.iter().map(provider_entry));
            entries.extend(
                scope
                    .exports
                    .exported_macros
                    .iter()
                    .map(|provider| format!("export {}", provider_entry(provider))),
            );
            entries.extend(scope.exports.globs.iter().map(glob_entry));
            entries.extend(scope.exports.opaque.iter().map(opaque_entry));
            entries.extend(scope.exports.macro_bodies.iter().map(macro_entry));
            entries.extend(macro_order_entry(&scope.exports));
        }
    }
    result
}

/// 列挙できない持ち込み (版の添字, 場所)。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Gap {
    side: usize,
    hole: Hole,
}

impl Gap {
    /// 持ち込む module。
    fn module(&self) -> &Vec<String> {
        hole_module(&self.hole)
    }
}

/// 列挙できない持ち込みの module。
fn hole_module(hole: &Hole) -> &Vec<String> {
    match hole {
        Hole::Glob(module, _, _) | Hole::Opaque(module, _, _) | Hole::Uncertain(module) => module,
    }
}

/// 版によらない、列挙できない持ち込みの識別 (module, 種類, 字面)。
type HoleId = (Vec<String>, u8, String);

/// 版 `version` の列挙できない持ち込み `hole` の識別。
fn hole_id(version: &Index<'_>, hole: &Hole) -> HoleId {
    match hole {
        Hole::Glob(module, scope_index, position) => (
            module.clone(),
            0,
            glob_entry(&version.scopes(module)[*scope_index].exports.globs[*position]),
        ),
        Hole::Opaque(module, scope_index, position) => (
            module.clone(),
            1,
            format!(
                "{} {:?}",
                opaque_entry(&version.scopes(module)[*scope_index].exports.opaque[*position]),
                version.opaque_names_of(module, *scope_index, *position)
            ),
        ),
        Hole::Uncertain(module) => (module.clone(), 2, placement_identity(version, module)),
    }
}

/// 識別 `id` の列挙できない持ち込みを、版 `version` で探す。
fn locate_hole(version: &Index<'_>, id: &HoleId) -> Option<Hole> {
    let (module, kind, _) = id;
    match kind {
        0 => version
            .globs(module)
            .map(|(scope_index, position, _)| Hole::Glob(module.clone(), scope_index, position))
            .find(|hole| hole_id(version, hole) == *id),
        1 => version
            .opaque(module)
            .map(|(scope_index, position, _)| Hole::Opaque(module.clone(), scope_index, position))
            .find(|hole| hole_id(version, hole) == *id),
        _ => version
            .module_uncertain(module)
            .then(|| Hole::Uncertain(module.clone()))
            .filter(|hole| hole_id(version, hole) == *id),
    }
}

/// 旧新の版の、同じ識別の列挙できない持ち込み。
type Holes = Vec<(HoleId, [Option<Hole>; 2])>;

/// 版の、比べる名前の依存の範囲。
struct Cone<'a, 'p> {
    index: &'a Index<'p>,
    /// 依存の先から並べた (module, 名前)。
    order: Vec<Key>,
    seen: HashSet<Key>,
    /// (module, 名前) → 解くのに引く (module, 名前)。
    deps: HashMap<Key, Vec<Key>>,
    /// module の公開の到達性を決めるもの。
    binders: Binders,
    /// 列挙できない持ち込みのある module → それを glob で取り込み得る module。
    forwarding: HashMap<Vec<String>, BTreeSet<Vec<String>>>,
    over: bool,
}

impl<'a, 'p> Cone<'a, 'p> {
    fn new(index: &'a Index<'p>) -> Self {
        Self {
            index,
            order: Vec::new(),
            seen: HashSet::new(),
            deps: HashMap::new(),
            binders: Binders::default(),
            forwarding: HashMap::new(),
            over: false,
        }
    }

    /// module `module` で経路 `path` (起点 `origin`) をたどるときに引く (module, 名前)。
    fn path_keys(&self, module: &[String], path: &[String], origin: UsePathOrigin) -> Vec<Key> {
        self.index.path_keys(module, path, origin)
    }

    /// (module, 名前) を解くのに引く (module, 名前)。
    fn direct_deps(&self, key: &Key) -> Vec<Key> {
        let (module, name) = key;
        let mut deps = Vec::new();
        for &entry in self.index.entries(key) {
            let (_, provider) = self.index.provider(module, entry);
            if let ProviderKind::Use {
                module: path,
                item,
                origin,
            } = &provider.kind
            {
                deps.extend(self.path_keys(module, path, *origin));
                let (targets, _) = self.index.modules_of(module, path, *origin);
                if !item.is_empty() {
                    deps.extend(targets.into_iter().map(|target| (target, item.clone())));
                }
                if path.is_empty() && *origin == UsePathOrigin::Bare {
                    deps.extend(
                        self.index
                            .bare_scopes(module)
                            .into_iter()
                            .map(|scope| (scope, item.clone())),
                    );
                }
            }
        }
        for (_, _, glob) in self.index.globs(module) {
            deps.extend(self.path_keys(module, &glob.module, glob.origin));
            let (targets, _) = self.index.modules_of(module, &glob.module, glob.origin);
            deps.extend(targets.into_iter().map(|target| (target, name.clone())));
        }
        deps
    }

    /// (module, 名前) とその依存を、依存の先から並べて足す。
    fn visit_key(&mut self, root: &Key) {
        if self.seen.contains(root) || self.over {
            return;
        }
        self.seen.insert(root.clone());
        let mut stack: Vec<(Key, Vec<Key>, usize)> =
            vec![(root.clone(), self.direct_deps(root), 0)];
        while let Some(top) = stack.last_mut() {
            if top.2 < top.1.len() {
                let next = top.1[top.2].clone();
                top.2 += 1;
                if self.seen.insert(next.clone()) {
                    if self.seen.len() > MAX_KEYS {
                        self.over = true;
                        return;
                    }
                    let deps = self.direct_deps(&next);
                    stack.push((next, deps, 0));
                }
            } else if let Some((key, deps, _)) = stack.pop() {
                self.deps.insert(key.clone(), deps);
                self.order.push(key);
            }
        }
    }

    /// module `module` (と、それを束縛し得る (module, 名前) の module) の公開の到達性に引くものを足す。
    fn visit_reach(&mut self, module: &[String]) {
        let mut pending = vec![module.to_vec()];
        while let Some(current) = pending.pop() {
            if current.is_empty() || self.binders.keys.contains_key(&current) || self.over {
                continue;
            }
            let parent = current[..current.len() - 1].to_vec();
            let last = current[current.len() - 1].clone();
            let mut found: Vec<Key> = Vec::new();
            let mut seen: HashSet<Key> = HashSet::new();
            let mut queue = vec![(parent, last.clone())];
            while let Some(binder) = queue.pop() {
                if !seen.insert(binder.clone()) {
                    continue;
                }
                if seen.len() > MAX_KEYS {
                    self.over = true;
                    return;
                }
                // named の取り込みは、経路がこの module を指し得るものだけ。
                queue.extend(
                    self.index
                        .importers(&binder.1)
                        .iter()
                        .filter(|importer| self.index.imports_from(importer, &binder.1, &binder.0))
                        .cloned(),
                );
                queue.extend(
                    self.index
                        .glob_importers(&binder.0)
                        .map(|importer| (importer.clone(), binder.1.clone())),
                );
                found.push(binder);
            }
            for binder in &found {
                self.visit_key(binder);
                pending.push(binder.0.clone());
            }
            // 名前を列挙できない項目 (マクロ) が字面に module の名前を持てば、公開し得る。
            let opaque: Vec<Place> = self.index.opaque_mentioning(&last).to_vec();
            for (placed, _, _) in &opaque {
                pending.push(placed.clone());
            }
            self.binders.opaque.insert(current.clone(), opaque);
            self.binders.keys.insert(current, found);
        }
    }

    /// 列挙できない持ち込みのある module `module` から、名前を glob で外へ出し得る module を足す。
    fn visit_forwarding(&mut self, module: &[String]) {
        if self.forwarding.contains_key(module) {
            return;
        }
        let mut found: BTreeSet<Vec<String>> = BTreeSet::new();
        let mut queue = vec![module.to_vec()];
        while let Some(current) = queue.pop() {
            for importer in self.index.glob_importers(&current) {
                if found.insert(importer.clone()) {
                    queue.push(importer.clone());
                }
            }
        }
        let mut modules = found.clone();
        modules.insert(module.to_vec());
        for current in &modules {
            let globs: Vec<(Vec<String>, UsePathOrigin)> = self
                .index
                .globs(current)
                .map(|(_, _, glob)| (glob.module.clone(), glob.origin))
                .collect();
            for (path, origin) in globs {
                for key in self.path_keys(current, &path, origin) {
                    self.visit_key(&key);
                }
            }
            self.visit_reach(current);
        }
        self.forwarding.insert(module.to_vec(), found);
    }

    /// 名前 `roots` と module `reach` から引くもの全て ((module, 名前) と、到達性を引く module)。
    fn closure(
        &self,
        roots: &[Key],
        reach: &[Vec<String>],
    ) -> (BTreeSet<Key>, BTreeSet<Vec<String>>) {
        let mut keys: BTreeSet<Key> = BTreeSet::new();
        let mut modules: BTreeSet<Vec<String>> = BTreeSet::new();
        let mut key_queue: Vec<Key> = roots.to_vec();
        let mut module_queue: Vec<Vec<String>> = reach.to_vec();
        loop {
            if let Some(key) = key_queue.pop() {
                if keys.insert(key.clone()) {
                    key_queue.extend(self.deps.get(&key).into_iter().flatten().cloned());
                }
                continue;
            }
            if let Some(module) = module_queue.pop() {
                if modules.insert(module.clone()) {
                    for binder in self.binders.keys.get(&module).into_iter().flatten() {
                        key_queue.push(binder.clone());
                        module_queue.push(binder.0.clone());
                    }
                    for (placed, _, _) in self.binders.opaque.get(&module).into_iter().flatten() {
                        module_queue.push(placed.clone());
                    }
                }
                continue;
            }
            break;
        }
        (keys, modules)
    }
}

/// 字面の並びのハッシュ。
fn digest<'s>(parts: impl IntoIterator<Item = &'s str>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut sorted: Vec<&str> = parts.into_iter().collect();
    sorted.sort_unstable();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sorted.hash(&mut hasher);
    hasher.finish()
}

/// 版 `index` の module `module` の、名前を解くのに使う入力 (glob・名前を列挙できない項目・置き場) の
/// ハッシュ。
fn module_digest(index: &Index<'_>, module: &[String]) -> u64 {
    let mut parts: Vec<String> = Vec::new();
    for (_, _, glob) in index.globs(module) {
        parts.push(glob_entry(glob));
    }
    for (scope_index, position, opaque) in index.opaque(module) {
        parts.push(format!(
            "{} {:?} {}",
            opaque_entry(opaque),
            index.opaque_names_of(module, scope_index, position),
            index.opaque_bodies_of(module, scope_index, position)
        ));
    }
    // 同じ名前のマクロの定義・呼び出しの並び (呼ぶ本体が変わり得る)。
    for scope in index.scopes(module) {
        parts.extend(macro_order_entry(&scope.exports));
    }
    parts.push(placement_identity(index, module));
    digest(parts.iter().map(String::as_str))
}

/// 版 `index` の (module, 名前) `key` の、名前を作るものの入力のハッシュ。`#[macro_export]` のマクロは、
/// 置いた module の置き場 (着く条件) でも有るかが変わる。
fn key_digest(index: &Index<'_>, key: &Key) -> u64 {
    let parts: Vec<String> = index
        .entries(key)
        .iter()
        .map(|&entry| {
            let text = provider_entry(index.provider(&key.0, entry).1);
            match entry {
                Entry::Exported(module_index, _, _) => format!(
                    "{text} @ {}",
                    placement_identity(index, index.exported_scope(module_index))
                ),
                Entry::Provider(..) => text,
            }
        })
        .collect();
    digest(parts.iter().map(String::as_str))
}

/// 旧新で解いた結果が変わり得る (module, 名前) と、公開の経路が変わり得る module と、glob・名前を
/// 列挙できない項目・置き場が変わった module。
struct Dirty {
    keys: HashSet<Key>,
    reach: HashSet<Vec<String>>,
    modules: HashSet<Vec<String>>,
}

/// 旧新で入力 (名前を作るもの・module の glob・名前を列挙できない項目・置き場・依存・公開の経路を決める
/// もの) が違う (module, 名前) と module から、それに依るものへ逆にたどる (依存の範囲が同じで入力も同じなら、
/// 解いても同じ)。
fn dirtiness(versions: &[&Index<'_>; 2], cones: &[Cone<'_, '_>; 2]) -> Dirty {
    let mut module_digests: [HashMap<Vec<String>, u64>; 2] = [HashMap::new(), HashMap::new()];
    let mut module_of = |side: usize, module: &Vec<String>| -> u64 {
        *module_digests[side]
            .entry(module.clone())
            .or_insert_with(|| module_digest(versions[side], module))
    };
    let mut dirty_keys: HashSet<Key> = HashSet::new();
    let all_keys: BTreeSet<&Key> = cones[0].seen.iter().chain(&cones[1].seen).collect();
    for key in &all_keys {
        let in_both = cones[0].seen.contains(*key) && cones[1].seen.contains(*key);
        let same = in_both
            && key_digest(versions[0], key) == key_digest(versions[1], key)
            && module_of(0, &key.0) == module_of(1, &key.0)
            && cones[0]
                .deps
                .get(*key)
                .map(|deps| deps.iter().collect::<BTreeSet<_>>())
                == cones[1]
                    .deps
                    .get(*key)
                    .map(|deps| deps.iter().collect::<BTreeSet<_>>());
        if !same {
            dirty_keys.insert((*key).clone());
        }
    }
    let mut dirty_modules: HashSet<Vec<String>> = HashSet::new();
    let all_modules: BTreeSet<&Vec<String>> = cones[0]
        .binders
        .keys
        .keys()
        .chain(cones[1].binders.keys.keys())
        .chain(cones[0].binders.opaque.keys())
        .chain(cones[1].binders.opaque.keys())
        .collect();
    for module in &all_modules {
        let binders = |side: usize| {
            cones[side]
                .binders
                .keys
                .get(*module)
                .map(|found| found.iter().collect::<BTreeSet<_>>())
        };
        let opaque = |side: usize| {
            cones[side]
                .binders
                .opaque
                .get(*module)
                .map(|found| found.iter().collect::<BTreeSet<_>>())
        };
        let placed_changed = cones[0]
            .binders
            .opaque
            .get(*module)
            .into_iter()
            .chain(cones[1].binders.opaque.get(*module))
            .flatten()
            .any(|(placed, _, _)| module_of(0, placed) != module_of(1, placed));
        if binders(0) != binders(1) || opaque(0) != opaque(1) || placed_changed {
            dirty_modules.insert((*module).clone());
        }
    }
    // 逆の依存: (module, 名前) → それに依る (module, 名前)、(module, 名前) → それを束縛に使う module、
    // module → その到達性に依る module。
    let mut reverse: HashMap<&Key, Vec<&Key>> = HashMap::new();
    let mut binds: HashMap<&Key, Vec<&Vec<String>>> = HashMap::new();
    let mut reaches: HashMap<&Vec<String>, Vec<&Vec<String>>> = HashMap::new();
    for cone in cones {
        for (key, deps) in &cone.deps {
            for dep in deps {
                reverse.entry(dep).or_default().push(key);
            }
        }
        for (module, found) in &cone.binders.keys {
            for binder in found {
                binds.entry(binder).or_default().push(module);
                reaches.entry(&binder.0).or_default().push(module);
            }
        }
        for (module, found) in &cone.binders.opaque {
            for (placed, _, _) in found {
                reaches.entry(placed).or_default().push(module);
            }
        }
    }
    let mut key_queue: Vec<Key> = dirty_keys.iter().cloned().collect();
    let mut module_queue: Vec<Vec<String>> = dirty_modules.iter().cloned().collect();
    loop {
        if let Some(key) = key_queue.pop() {
            for dependent in reverse.get(&key).into_iter().flatten() {
                if dirty_keys.insert((*dependent).clone()) {
                    key_queue.push((*dependent).clone());
                }
            }
            for module in binds.get(&key).into_iter().flatten() {
                if dirty_modules.insert((*module).clone()) {
                    module_queue.push((*module).clone());
                }
            }
            continue;
        }
        if let Some(module) = module_queue.pop() {
            for dependent in reaches.get(&module).into_iter().flatten() {
                if dirty_modules.insert((*dependent).clone()) {
                    module_queue.push((*dependent).clone());
                }
            }
            continue;
        }
        break;
    }
    // glob・名前を列挙できない項目・置き場が変わった module (列挙できない持ち込みの外への出方に効く)。
    let mut modules: HashSet<Vec<String>> = HashSet::new();
    let forwarding: BTreeSet<&Vec<String>> = cones
        .iter()
        .flat_map(|cone| {
            cone.forwarding
                .iter()
                .flat_map(|(module, importers)| std::iter::once(module).chain(importers))
        })
        .collect();
    for module in forwarding {
        if module_of(0, module) != module_of(1, module) {
            modules.insert(module.clone());
        }
    }
    Dirty {
        keys: dirty_keys,
        reach: dirty_modules,
        modules,
    }
}

/// 名前 `names` のうち、旧新で解いた結果が変わり得るもの。
fn changed_names(dirty: &Dirty, names: BTreeSet<Key>) -> Vec<Key> {
    names
        .into_iter()
        .filter(|name| dirty.keys.contains(name) || dirty.reach.contains(&name.0))
        .collect()
}

/// 列挙できない持ち込み `holes` のうち、外への出方が旧新で変わり得るもの: 片方の版にしか無いか、それを持つ
/// module か、それを glob で取り込み得る module の、glob・置き場・公開の経路・glob の経路が変わり得る。
fn changed_holes(dirty: &Dirty, cones: &[Cone<'_, '_>; 2], holes: Holes) -> Holes {
    holes
        .into_iter()
        .filter(|(_, found)| {
            if found[0].is_none() || found[1].is_none() {
                return true;
            }
            found.iter().enumerate().any(|(side, hole)| {
                let Some(hole) = hole else {
                    return false;
                };
                let module = hole_module(hole);
                let cone = &cones[side];
                std::iter::once(module)
                    .chain(cone.forwarding.get(module).into_iter().flatten())
                    .any(|module| {
                        dirty.modules.contains(module)
                            || dirty.reach.contains(module)
                            || cone.index.globs(module).any(|(_, _, glob)| {
                                cone.path_keys(module, &glob.module, glob.origin)
                                    .iter()
                                    .any(|key| dirty.keys.contains(key))
                            })
                    })
            })
        })
        .collect()
}

/// 名前 `name` が、どちらかの版で取り込み (named・glob・名前を列挙できない項目・場所の分からない module) で
/// 作られ得るか (宣言だけが作る名前は、宣言の報告に任せる)。
fn may_be_imported(versions: &[&Index<'_>; 2], name: &Key) -> bool {
    versions.iter().any(|version| {
        version.entries(name).iter().any(|&entry| {
            !matches!(
                version.provider(&name.0, entry).1.kind,
                ProviderKind::Declared(_)
            )
        }) || version.globs(&name.0).any(|(scope_index, position, _)| {
            version.glob_may_bring(&name.0, scope_index, position, &name.1)
        }) || !version.opaque_affecting(&name.0, &name.1).is_empty()
            || version.module_uncertain(&name.0)
    })
}

/// module `module` の置き場の字面 (ファイル・inline module・着く条件、場所の分からない宣言)。
fn placement_identity(index: &Index<'_>, module: &[String]) -> String {
    let mut places: Vec<String> = index
        .scopes(module)
        .iter()
        .map(|scope| {
            format!(
                "{} {:?} {:?}",
                scope.file,
                scope.inline,
                scope
                    .clauses
                    .iter()
                    .map(Clause::identity)
                    .collect::<Vec<_>>()
            )
        })
        .collect();
    places.sort();
    let unknown: Vec<String> = (0..=module.len())
        .flat_map(|depth| {
            index
                .placements
                .unknown
                .get(&module[..depth])
                .into_iter()
                .flatten()
                .map(|unknown| unknown.identity.clone())
        })
        .collect();
    format!(
        "{module:?} place {places:?} {unknown:?} {}",
        index.module_uncertain(module)
    )
}

/// 属性の並び (読めなければ None) の有る条件。
fn predicate(attributes: Option<&[AvailabilityAttr]>, atoms: &mut Atoms) -> Option<Predicate> {
    attributes.map(|attributes| {
        Predicate::All(
            attributes
                .iter()
                .map(|attribute| atoms.presence(&attribute.key))
                .collect(),
        )
    })
}

/// 版 `index` の (module, 名前) `keys` を解くのに使う条件を、原子 `atoms` で組む。名前が空の鍵の module は、
/// 全ての glob の条件を組む (列挙できない持ち込みの転送を確かめる)。glob の条件は、その glob が鍵の名前を
/// 持ち込み得るときだけ組む (関係の無い glob の原子で組み合わせを増やさない。解くときは先の名前が無ければ
/// glob の有無を見ない)。
fn conditions(
    index: &Index<'_>,
    keys: &BTreeSet<Key>,
    opaque: &[Place],
    atoms: &mut Atoms,
) -> Conditions {
    let mut result = Conditions::default();
    let mut names: BTreeMap<Vec<String>, BTreeSet<&str>> = BTreeMap::new();
    for key in keys {
        names.entry(key.0.clone()).or_default().insert(&key.1);
        for &entry in index.entries(key) {
            match entry {
                Entry::Provider(scope_index, position) => {
                    let (_, provider) = index.provider(&key.0, entry);
                    result.own.insert(
                        (key.0.clone(), scope_index, 0, position),
                        predicate(provider.availability.as_deref(), atoms),
                    );
                }
                Entry::Exported(module_index, scope_index, position) => {
                    let placed = index.exported_scope(module_index).to_vec();
                    let (_, provider) = index.provider(&key.0, entry);
                    result.own.insert(
                        (placed.clone(), scope_index, 3, position),
                        predicate(provider.availability.as_deref(), atoms),
                    );
                    names.entry(placed).or_default();
                }
            }
        }
    }
    for (module, scope_index, _) in opaque {
        names.entry(module.clone()).or_default();
        let _ = scope_index;
    }
    for (module, wanted) in &names {
        for (scope_index, scope) in index.scopes(module).iter().enumerate() {
            result.placed.insert(
                (module.clone(), scope_index),
                scope
                    .clauses
                    .iter()
                    .map(|clause| predicate(clause.attributes.as_deref(), atoms))
                    .collect(),
            );
            for (position, glob) in scope.exports.globs.iter().enumerate() {
                let relevant = wanted.contains("")
                    || wanted
                        .iter()
                        .any(|name| index.glob_may_bring(module, scope_index, position, name));
                if relevant {
                    result.own.insert(
                        (module.clone(), scope_index, 1, position),
                        predicate(glob.availability.as_deref(), atoms),
                    );
                }
            }
            for (position, item) in scope.exports.opaque.iter().enumerate() {
                let names_of = index.opaque_names_of(module, scope_index, position);
                let relevant = opaque
                    .iter()
                    .any(|(placed, placed_scope, placed_position)| {
                        placed == module
                            && *placed_scope == scope_index
                            && *placed_position == position
                    })
                    || wanted
                        .iter()
                        .any(|name| name.is_empty() || names_of.contains(*name));
                if relevant {
                    result.own.insert(
                        (module.clone(), scope_index, 2, position),
                        predicate(item.availability.as_deref(), atoms),
                    );
                }
            }
        }
        if index.module_uncertain(module) {
            let mut clauses = Vec::new();
            for depth in 0..=module.len() {
                let prefix = &module[..depth];
                for unknown in index.placements.unknown.get(prefix).into_iter().flatten() {
                    clauses.extend(
                        unknown
                            .clauses
                            .iter()
                            .map(|clause| predicate(clause.attributes.as_deref(), atoms)),
                    );
                }
                for placement in index.placements.files.get(prefix).into_iter().flatten() {
                    if placement.index.is_none() {
                        clauses.extend(
                            placement
                                .clauses
                                .iter()
                                .map(|clause| predicate(clause.attributes.as_deref(), atoms)),
                        );
                    }
                }
            }
            result.uncertain.insert(module.clone(), clauses);
        }
    }
    result
}

/// 名前 1 つの、設定ごとの比べた結果の集計。
#[derive(Default)]
struct Tally {
    imported: bool,
    /// 型の名前空間 (module の名前) の束縛が、どこかの設定で取り込みか (Issue #314)。
    imported_type: bool,
    undecided: bool,
    retarget: bool,
    lost: [bool; 3],
    gained: [bool; 3],
    /// 名前空間ごとの、設定ごとの有る・無い (旧, 新)。
    old_present: [Vec<bool>; 3],
    new_present: [Vec<bool>; 3],
    /// 報告の置き場 (取り込みで作ったものを優先)。
    old_site: Option<(Via, Target, bool)>,
    new_site: Option<(Via, Target, bool)>,
    old_targets: BTreeSet<Target>,
    new_targets: BTreeSet<Target>,
    /// 名前の有る設定で、enum の名前を束縛した宣言 (enum の経路, 名前を作るもの)。
    old_enums: BTreeSet<(Vec<String>, Entry)>,
    new_enums: BTreeSet<(Vec<String>, Entry)>,
}

impl Tally {
    fn observe(
        &mut self,
        old: &Resolver<'_, '_>,
        new: &Resolver<'_, '_>,
        key: &Key,
        enums: &mut HashMap<(Vec<String>, Vec<String>), bool>,
    ) {
        for namespace in 0..NAMESPACES.len() {
            let (was, is) = (old.presence(key, namespace), new.presence(key, namespace));
            let (old_state, new_state) = (old.state(key, namespace), new.state(key, namespace));
            for state in [old_state, new_state] {
                let imported = match state {
                    State::Resolved(binding) => binding.imported,
                    State::Ambiguous | State::Unknown | State::Foreign(..) => true,
                    State::Absent | State::Pending | State::Hidden | State::Opaque => false,
                };
                self.imported |= imported;
                self.imported_type |= namespace == 0 && imported;
            }
            // 同じ外部の項目の同じ束縛なら、名前空間が分からなくても旧新で同じ。
            let same_foreign = matches!(old_state, State::Foreign(..)) && old_state == new_state;
            match (was, is) {
                (Some(was), Some(is)) => {
                    self.lost[namespace] |= was && !is;
                    self.gained[namespace] |= !was && is;
                }
                _ if same_foreign => {}
                _ => self.undecided = true,
            }
            self.old_present[namespace].push(was == Some(true));
            self.new_present[namespace].push(is == Some(true));
            // module の名前の指す先が変われば、その下の名前を比べ切れない (外部 crate の項目は module かも
            // しれない)。enum は variant の名前を持つ擬似的な module として解く。enum から enum への切り替えは、
            // この設定で公開の variant (名前空間ごと) が同じなら下の名前も同じ (移動や名前の変更)。
            if namespace == 0
                && let (State::Resolved(old_binding), State::Resolved(new_binding)) =
                    (old_state, new_state)
                && old_binding.target != new_binding.target
                && (was != Some(false) || is != Some(false))
            {
                let enum_path = |resolver: &Resolver<'_, '_>, target: &Target| match target {
                    Target::Module(path) if resolver.is_enum(path) => Some(path.clone()),
                    _ => None,
                };
                let module_like = |resolver: &Resolver<'_, '_>, target: &Target| match target {
                    Target::Module(path) => !resolver.is_enum(path),
                    Target::External(_) => true,
                    Target::Item(..) => false,
                };
                self.retarget |= match (
                    enum_path(old, &old_binding.target),
                    enum_path(new, &new_binding.target),
                ) {
                    (Some(old_path), Some(new_path)) => *enums
                        .entry((old_path.clone(), new_path.clone()))
                        .or_insert_with(|| {
                            variants_differ(old.index(), new.index(), &old_path, &new_path)
                        }),
                    // enum と enum でないものの間の切り替えは、variant の名前が出入りする。
                    (Some(_), None) | (None, Some(_)) => true,
                    (None, None) => {
                        module_like(old, &old_binding.target)
                            || module_like(new, &new_binding.target)
                    }
                };
            }
            for (resolver, state, bound, present) in [
                (old, old_state, &mut self.old_enums, was),
                (new, new_state, &mut self.new_enums, is),
            ] {
                if present == Some(true)
                    && let State::Resolved(Binding {
                        target: Target::Module(path),
                        ..
                    }) = state
                    && let Some(entry) = resolver.enum_entry(path)
                {
                    bound.insert((path.clone(), entry));
                }
            }
            for (state, site, targets, present) in [
                (old_state, &mut self.old_site, &mut self.old_targets, was),
                (new_state, &mut self.new_site, &mut self.new_targets, is),
            ] {
                if let State::Resolved(binding) = state {
                    let better = match site {
                        None => true,
                        Some((_, _, imported)) => !*imported && binding.imported,
                    };
                    if better {
                        *site = Some((binding.via, binding.target.clone(), binding.imported));
                    }
                    if present == Some(true) {
                        targets.insert(binding.target.clone());
                    }
                }
            }
        }
    }
}

impl ReexportNames {
    /// 差分の中の package の lib crate のうち、名前を作る入力が変わったものの再公開した名前を比べる。
    pub(crate) fn new(
        dir: &str,
        rust_crates: &RustCrates<'_>,
        diff_files: &[crate::models::impact::DiffFile],
        renames: &[crate::models::impact::DiffFile],
    ) -> Self {
        let mut this = Self::default();
        for (root, reason) in rust_crates.reexport_gaps() {
            this.truncations.push(incomplete(root, reason));
        }
        let pairs: Vec<(String, String)> = diff_files
            .iter()
            .chain(renames)
            .map(|df| (df.old_path.clone(), df.new_path.clone()))
            .collect();
        for (root, pair) in rust_crates.reexport_pairs() {
            this.compare_crate(dir, &root, pair, &pairs);
        }
        this
    }

    /// 比べ直した結果。
    pub(crate) fn outcomes(&self) -> &[NameOutcome] {
        &self.outcomes
    }

    /// 追加の候補の名前 (ApiRefIndex で引く)。
    pub(crate) fn index_names(&self) -> impl Iterator<Item = &String> {
        self.names.iter()
    }

    /// crate root `root` の module `module` の名前 `name` を、型の名前空間の取り込みの束縛として比べたか
    /// (Issue #314)。比べた (報告したか、公開の有無と先が旧新で同じ) なら真、取り込みが作らない (比べる名前に
    /// 入らない・どの設定でも型の名前空間に取り込みの束縛が無い) なら偽。crate を比べ切っていない・その名前を
    /// 比べられなかったなら None。
    pub(crate) fn rebinds(&self, root: &str, module: &[String], name: &str) -> Option<bool> {
        let key = (root.to_owned(), (module.to_vec(), name.to_owned()));
        if self.rebound.contains(&key) {
            return Some(true);
        }
        (self.completed.contains(root) && !self.unresolved.contains(&key)).then_some(false)
    }

    fn compare_crate(
        &mut self,
        dir: &str,
        root: &str,
        pair: &super::rust_dependencies::crates::CratePair<'_>,
        pairs: &[(String, String)],
    ) {
        use crate::commands::common::timed;
        let (old_placements, new_placements) = timed("reexport_names.placements", || {
            (pair.old.placements(), pair.new.placements())
        });
        for placements in [old_placements, new_placements] {
            if let Some(reason) = &placements.incomplete {
                self.truncations.push(incomplete(root, reason));
                return;
            }
        }
        let mut cache = ExportsCache::new();
        let (old, new) = timed("reexport_names.index", || {
            (
                Index::new(old_placements, pair.old.edition(&pair.package), &mut cache),
                Index::new(new_placements, pair.new.edition(&pair.package), &mut cache),
            )
        });
        let versions = [&old, &new];
        if versions.iter().any(|version| version.table_over()) {
            self.truncations.push(incomplete(
                root,
                &format!(
                    "names may point to more than {} modules",
                    index::MAX_MODULE_PATHS
                ),
            ));
            return;
        }
        if versions.iter().any(|version| version.macro_over()) {
            self.truncations.push(incomplete(
                root,
                "macro calls may make too many names to compare",
            ));
            return;
        }
        let (seeds, mut gaps) = timed("reexport_names.seeds", || {
            seeds(dir, pair, &versions, pairs)
        });
        // 呼び出しの入力の閉包も、マクロの名前の予算から払う。
        if versions.iter().any(|version| version.macro_over()) {
            self.truncations.push(incomplete(
                root,
                "macro calls may make too many names to compare",
            ));
            return;
        }
        let Some(names) = timed("reexport_names.closure", || {
            reexporting_names(&versions, seeds, &mut gaps)
        }) else {
            self.truncations.push(incomplete(
                root,
                &format!("more than {MAX_NAMES} names are re-exported"),
            ));
            return;
        };
        // 取り込みで作られ得ない名前 (宣言だけが作る名前) は、宣言の報告に任せて比べない。
        let names: BTreeSet<Key> = names
            .into_iter()
            .filter(|name| may_be_imported(&versions, name))
            .collect();
        let mut cones = [Cone::new(&old), Cone::new(&new)];
        timed("reexport_names.cones", || {
            for name in &names {
                for cone in &mut cones {
                    cone.visit_key(name);
                    cone.visit_reach(&name.0);
                }
            }
        });
        // 列挙できない持ち込みは、旧新の両方の版で外へ出るかを比べる (同じ識別のものを探す)。
        let ids: BTreeSet<HoleId> = gaps
            .iter()
            .map(|gap| hole_id(versions[gap.side], &gap.hole))
            .collect();
        let holes: Holes = ids
            .into_iter()
            .map(|id| {
                let found = [locate_hole(&old, &id), locate_hole(&new, &id)];
                (id, found)
            })
            .collect();
        timed("reexport_names.forwarding", || {
            for (_, found) in &holes {
                for (side, hole) in found.iter().enumerate() {
                    if let Some(hole) = hole {
                        cones[side].visit_forwarding(hole_module(hole));
                    }
                }
            }
        });
        if cones.iter().any(|cone| cone.over) {
            self.truncations.push(incomplete(
                root,
                &format!("more than {MAX_KEYS} names are needed to resolve the re-exports"),
            ));
            return;
        }
        // 依存の範囲の入力が旧新で同じ名前・列挙できない持ち込みは、解いても同じなので比べない。
        let (changed, holes) = timed("reexport_names.fingerprints", || {
            let dirty = dirtiness(&versions, &cones);
            let changed: Vec<Key> = changed_names(&dirty, names);
            (changed, changed_holes(&dirty, &cones, holes))
        });
        if versions.iter().any(|version| version.macro_over()) {
            self.truncations.push(incomplete(
                root,
                "macro calls may make too many names to compare",
            ));
            return;
        }
        if changed.is_empty() && holes.is_empty() {
            self.completed.insert(root.to_owned());
            return;
        }
        crate::commands::common::log_phase(
            &format!(
                "reexport_names.sizes names={} changed={} gaps={} holes={} keys={}+{}",
                cones[0].seen.len().max(cones[1].seen.len()),
                changed.len(),
                gaps.len(),
                holes.len(),
                cones[0].order.len(),
                cones[1].order.len()
            ),
            "info",
            0,
        );
        if !timed("reexport_names.group", || {
            self.compare_group(root, &versions, &cones, &changed, &holes)
        }) {
            // 原子か量が多ければ、名前・列挙できない持ち込みごとに分けて解く。
            for hole in &holes {
                if !self.compare_group(root, &versions, &cones, &[], std::slice::from_ref(hole)) {
                    self.truncations.push(incomplete(root, UNLISTED));
                    break;
                }
            }
            for name in &changed {
                if !self.compare_group(root, &versions, &cones, std::slice::from_ref(name), &[]) {
                    let key = (root.to_owned(), name.clone());
                    if may_be_public_import(&versions, name) && self.unverified(&versions, name) {
                        self.rebound.insert(key);
                    } else {
                        self.unresolved.insert(key);
                    }
                }
            }
        }
        self.completed.insert(root.to_owned());
    }

    /// 名前 `names` と列挙できない持ち込み `gaps` を、まとめて設定ごとに解いて比べる。原子か量が上限を
    /// 超えれば何もせず偽。
    fn compare_group(
        &mut self,
        root: &str,
        versions: &[&Index<'_>; 2],
        cones: &[Cone<'_, '_>; 2],
        names: &[Key],
        holes: &[(HoleId, [Option<Hole>; 2])],
    ) -> bool {
        let mut atoms = Atoms::default();
        let mut keys: [Vec<Key>; 2] = [Vec::new(), Vec::new()];
        let mut binders: [Binders; 2] = [Binders::default(), Binders::default()];
        let mut conditions_of: Vec<Conditions> = Vec::new();
        for side in 0..2 {
            let cone = &cones[side];
            let mut reach_roots: Vec<Vec<String>> =
                names.iter().map(|name| name.0.clone()).collect();
            let mut key_roots: Vec<Key> = names.to_vec();
            let mut glob_modules: BTreeSet<Vec<String>> = BTreeSet::new();
            let mut places: Vec<Place> = Vec::new();
            for hole in holes.iter().filter_map(|(_, found)| found[side].as_ref()) {
                let module = hole_module(hole);
                glob_modules.insert(module.clone());
                glob_modules.extend(cone.forwarding.get(module).into_iter().flatten().cloned());
                if let Hole::Opaque(module, scope_index, position) = hole {
                    places.push((module.clone(), *scope_index, *position));
                }
            }
            for module in &glob_modules {
                reach_roots.push(module.clone());
                for (_, _, glob) in versions[side].globs(module) {
                    key_roots.extend(cone.path_keys(module, &glob.module, glob.origin));
                }
            }
            let (closure_keys, closure_modules) = cone.closure(&key_roots, &reach_roots);
            keys[side] = cone
                .order
                .iter()
                .filter(|key| closure_keys.contains(*key))
                .cloned()
                .collect();
            for module in &closure_modules {
                if let Some(found) = cone.binders.keys.get(module) {
                    binders[side].keys.insert(module.clone(), found.clone());
                }
                if let Some(found) = cone.binders.opaque.get(module) {
                    binders[side].opaque.insert(module.clone(), found.clone());
                    places.extend(found.iter().cloned());
                }
            }
            let mut needed = closure_keys;
            needed.extend(
                glob_modules
                    .into_iter()
                    .map(|module| (module, String::new())),
            );
            conditions_of.push(conditions(versions[side], &needed, &places, &mut atoms));
        }
        let Some(configs) = atoms.configs() else {
            return false;
        };
        let configs: Vec<u64> = configs.collect();
        if configs.len().saturating_mul(keys[0].len() + keys[1].len()) > MAX_WORK {
            return false;
        }
        let mut tallies: BTreeMap<Key, Tally> = names
            .iter()
            .map(|name| (name.clone(), Tally::default()))
            .collect();
        let mut outward = false;
        // enum の組ごとの variant の比べた結果 (設定によらない)。
        let mut enums: HashMap<(Vec<String>, Vec<String>), bool> = HashMap::new();
        for &config in &configs {
            let old = Resolver::run(
                versions[0],
                &conditions_of[0],
                &keys[0],
                &binders[0],
                config,
            );
            let new = Resolver::run(
                versions[1],
                &conditions_of[1],
                &keys[1],
                &binders[1],
                config,
            );
            for (name, tally) in &mut tallies {
                tally.observe(&old, &new, name, &mut enums);
            }
            // 列挙できない名前を外へ出す module が旧新で違う設定があれば、名前を比べ切れない (別の module から
            // 出ることでは相殺しない)。
            for (_, found) in holes {
                let exposed = |side: usize, resolver: &Resolver<'_, '_>| {
                    found[side]
                        .as_ref()
                        .map(|hole| {
                            let gap = Gap {
                                side,
                                hole: hole.clone(),
                            };
                            forwards(versions[side], &cones[side], resolver, &gap)
                        })
                        .unwrap_or_default()
                };
                outward |= exposed(0, &old) != exposed(1, &new);
            }
        }
        if outward {
            self.truncations.push(incomplete(root, UNLISTED));
        }
        for (name, tally) in tallies {
            // module の名前は型の名前空間なので、その束縛が取り込みのときだけ任せられる (同じ名前の値の
            // 取り込みは module の配下の項目を残さない)。
            let imported_type = tally.imported_type;
            if self.report(versions, &atoms, &configs, &name, tally) && imported_type {
                self.rebound.insert((root.to_owned(), name));
            }
        }
        true
    }

    /// 名前 `name` を比べられないとする。報告できたか (報告の置き場を決められなければ偽)。
    fn unverified(&mut self, versions: &[&Index<'_>; 2], name: &Key) -> bool {
        let Some(candidate) =
            fallback_candidate(versions[1], name).or_else(|| fallback_candidate(versions[0], name))
        else {
            return false;
        };
        let old_signature = candidate.signature.clone();
        self.outcomes.push(NameOutcome::Unverified {
            candidate,
            old_signature,
        });
        true
    }

    /// 名前 `name` の集計 `tally` を報告にする。取り込みの束縛として扱ったか (どこかの設定で取り込みの
    /// 束縛があり、報告したか、公開の有無と先が旧新で同じ。Issue #314) を返す。
    fn report(
        &mut self,
        versions: &[&Index<'_>; 2],
        atoms: &Atoms,
        configs: &[u64],
        name: &Key,
        tally: Tally,
    ) -> bool {
        if !tally.imported {
            return false;
        }
        let lost = tally.lost.iter().any(|lost| *lost);
        let gained = tally.gained.iter().any(|gained| *gained);
        let site = |side: usize, found: &Option<(Via, Target, bool)>| {
            found
                .as_ref()
                .and_then(|(via, target, _)| site_candidate(versions[side], name, *via, target))
        };
        let old_candidate = site(0, &tally.old_site);
        let new_candidate = site(1, &tally.new_site);
        if tally.undecided || tally.retarget {
            let candidate = new_candidate
                .clone()
                .or_else(|| old_candidate.clone())
                .or_else(|| fallback_candidate(versions[1], name))
                .or_else(|| fallback_candidate(versions[0], name));
            let Some(candidate) = candidate else {
                return false;
            };
            let old_signature =
                old_candidate.map_or_else(|| candidate.signature.clone(), |old| old.signature);
            self.outcomes.push(NameOutcome::Unverified {
                candidate,
                old_signature,
            });
            return true;
        }
        if !lost && !gained {
            return true;
        }
        let never =
            |present: &[Vec<bool>; 3]| present.iter().all(|configs| configs.iter().all(|p| !p));
        let (old_never, new_never) = (never(&tally.old_present), never(&tally.new_present));
        let declarations = |side: usize,
                            targets: &BTreeSet<Target>,
                            enums: &BTreeSet<(Vec<String>, Entry)>|
         -> Vec<DeclarationId> {
            let version = versions[side];
            let declared = |module: &[String], item: &String, entry: Entry| {
                let (scope, provider) = version.provider(module, entry);
                (
                    scope.file.clone(),
                    (!scope.inline.is_empty()).then(|| scope.inline.join("::")),
                    item.clone(),
                    match provider.kind {
                        ProviderKind::Declared(kind) => candidate_kind(kind),
                        _ => "",
                    },
                )
            };
            let items = targets.iter().filter_map(|target| match target {
                Target::Item(module, item, entry) => Some(declared(module, item, *entry)),
                Target::Module(_) | Target::External(_) => None,
            });
            // enum は擬似的な module として解くので、名前の有った設定で束縛した enum の宣言を使う。
            let enum_declarations = enums.iter().filter_map(|(path, entry)| {
                path.split_last()
                    .map(|(item, module)| declared(module, item, *entry))
            });
            items.chain(enum_declarations).collect()
        };
        if new_never {
            let Some(candidate) = old_candidate else {
                return false;
            };
            self.outcomes.push(NameOutcome::Removed(
                candidate,
                declarations(0, &tally.old_targets, &tally.old_enums),
            ));
            return true;
        }
        if old_never {
            let Some(candidate) = new_candidate else {
                return false;
            };
            self.names.push(name.1.clone());
            self.outcomes.push(NameOutcome::Added(
                candidate,
                declarations(1, &tally.new_targets, &tally.new_enums),
            ));
            return true;
        }
        // 表示は、消える設定のある名前空間を優先する。
        let namespace = (0..NAMESPACES.len())
            .find(|&namespace| tally.lost[namespace])
            .or_else(|| (0..NAMESPACES.len()).find(|&namespace| tally.gained[namespace]))
            .unwrap_or(0);
        let availability = CfgAvailability {
            old: display::shown(atoms, configs, &tally.old_present[namespace]),
            new: display::shown(atoms, configs, &tally.new_present[namespace]),
        };
        let Some(candidate) = new_candidate.or(old_candidate) else {
            return false;
        };
        if gained {
            self.names.push(name.1.clone());
        }
        self.outcomes.push(NameOutcome::Availability {
            candidate,
            lost,
            gained,
            availability,
        });
        true
    }
}

/// 旧の enum `old_path` と新の enum `new_path` の公開の variant (名前空間ごと) が、どこかの設定で違うか。
/// variant 自身と置き場の条件の原子だけで設定を作る (決められないか、量が上限を超えれば違うとする)。
fn variants_differ(
    old: &Index<'_>,
    new: &Index<'_>,
    old_path: &[String],
    new_path: &[String],
) -> bool {
    /// (名前, 名前空間) → 有る条件 (置き場の節の OR と自身の AND) の並び。
    type Present = BTreeMap<(String, usize), Vec<Predicate>>;
    fn collect(index: &Index<'_>, path: &[String], atoms: &mut Atoms) -> Option<Present> {
        let mut found: Present = BTreeMap::new();
        for scope in index.scopes(path) {
            let placed = scope
                .clauses
                .iter()
                .map(|clause| predicate(clause.attributes.as_deref(), atoms))
                .collect::<Option<Vec<_>>>()?;
            for provider in scope.exports.providers.iter().filter(|p| p.scope.is_none()) {
                let own = predicate(provider.availability.as_deref(), atoms)?;
                // 原子が上限を超えれば設定を作れない (それ以上は登録しない)。
                if atoms.len() > super::rust_cfg_predicate::MAX_ATOMS {
                    return None;
                }
                for (namespace, bit) in NAMESPACES.iter().enumerate() {
                    if provider.namespaces & bit != 0 {
                        found
                            .entry((provider.name.clone(), namespace))
                            .or_default()
                            .push(Predicate::All(vec![
                                Predicate::Any(placed.clone()),
                                own.clone(),
                            ]));
                    }
                }
            }
        }
        Some(found)
    }
    let mut atoms = Atoms::default();
    let (Some(before), Some(after)) = (
        collect(old, old_path, &mut atoms),
        collect(new, new_path, &mut atoms),
    ) else {
        return true;
    };
    let Some(configs) = atoms.configs() else {
        return true;
    };
    let configs: Vec<u64> = configs.collect();
    let names: BTreeSet<&(String, usize)> = before.keys().chain(after.keys()).collect();
    if configs.len().saturating_mul(names.len()) > MAX_WORK {
        return true;
    }
    let present = |found: &Present, name: &(String, usize), config: u64| {
        found
            .get(name)
            .is_some_and(|list| list.iter().any(|predicate| predicate.holds(config)))
    };
    configs.iter().any(|&config| {
        names
            .iter()
            .any(|name| present(&before, name, config) != present(&after, name, config))
    })
}

/// 名前 `name` が、どちらかの版で crate の外へ取り込みで出し得るか (`pub` の取り込みか、名前を持ち込み得る
/// `pub` の glob がある)。
fn may_be_public_import(versions: &[&Index<'_>; 2], name: &Key) -> bool {
    versions.iter().any(|version| {
        version.entries(name).iter().any(|&entry| {
            let (_, provider) = version.provider(&name.0, entry);
            provider.scope.is_none() && !matches!(provider.kind, ProviderKind::Declared(_))
        }) || version.globs(&name.0).any(|(scope_index, position, glob)| {
            glob.scope.is_none() && version.glob_may_bring(&name.0, scope_index, position, &name.1)
        })
    })
}

/// 差分のファイルの、変わった名前を作るもの・glob・列挙できない項目・マクロの本体と、置き場・edition の
/// 変化から、比べる名前の種と、列挙できない持ち込みを集める。
fn seeds(
    dir: &str,
    pair: &super::rust_dependencies::crates::CratePair<'_>,
    versions: &[&Index<'_>; 2],
    pairs: &[(String, String)],
) -> (BTreeSet<Key>, BTreeSet<Gap>) {
    let mut seeds: BTreeSet<Key> = BTreeSet::new();
    let mut gaps: BTreeSet<Gap> = BTreeSet::new();
    let in_package = |path: &str| {
        super::rust_dependencies::crates::package_of(dir, path).as_deref()
            == Some(pair.package.as_str())
    };
    let pairs: Vec<&(String, String)> = pairs
        .iter()
        .filter(|(old_path, new_path)| in_package(old_path) || in_package(new_path))
        .collect();
    let paths: [HashSet<&str>; 2] = [
        pairs
            .iter()
            .map(|(old_path, _)| old_path.as_str())
            .collect(),
        pairs
            .iter()
            .map(|(_, new_path)| new_path.as_str())
            .collect(),
    ];
    let entries = [
        scope_entries(versions[0], &paths[0]),
        scope_entries(versions[1], &paths[1]),
    ];
    for (old_path, new_path) in &pairs {
        for (side, path, other_side, other_path) in
            [(0, old_path, 1, new_path), (1, new_path, 0, old_path)]
        {
            if path == "/dev/null" {
                continue;
            }
            let version = versions[side];
            for (module, scope_index) in version.by_file.get(path.as_str()).into_iter().flatten() {
                let scope = &version.scopes(module)[*scope_index];
                let empty = HashSet::new();
                let other = entries[other_side]
                    .get(&(other_path.to_string(), scope.inline.clone()))
                    .unwrap_or(&empty);
                for provider in &scope.exports.providers {
                    if !other.contains(&provider_entry(provider)) {
                        seeds.insert((module.clone(), provider.name.clone()));
                    }
                }
                for provider in &scope.exports.exported_macros {
                    if !other.contains(&format!("export {}", provider_entry(provider))) {
                        seeds.insert((Vec::new(), provider.name.clone()));
                    }
                }
                for (position, glob) in scope.exports.globs.iter().enumerate() {
                    if other.contains(&glob_entry(glob)) {
                        continue;
                    }
                    glob_seeds(
                        version,
                        side,
                        module,
                        *scope_index,
                        position,
                        &mut seeds,
                        &mut gaps,
                    );
                }
                for (position, opaque) in scope.exports.opaque.iter().enumerate() {
                    if other.contains(&opaque_entry(opaque)) {
                        continue;
                    }
                    opaque_seeds(version, module, *scope_index, position, &mut seeds);
                }
                // 定義・呼び出しの並びが変われば、このスコープの呼び出しの見る定義が変わり得る (定義の並びは
                // 定義の指紋に入るので、それを呼ぶほかの呼び出しは、下の入力の比較で拾う)。
                if let Some(order) = macro_order_entry(&scope.exports)
                    && !other.contains(&order)
                {
                    for position in 0..scope.exports.opaque.len() {
                        opaque_seeds(version, module, *scope_index, position, &mut seeds);
                    }
                }
            }
        }
    }
    // 呼び出しの見る定義を決める入力 (定義・取り込み・宣言・置き場・glob・並び・edition。呼ぶ名前と経路の名前
    // から閉じる) の指紋が旧新で違う呼び出しは、作り得る名前を比べる。同じ呼ぶ名前と経路の組の呼び出しは同じ
    // 指紋。
    let mut calls: BTreeMap<CallKey, [Vec<Place>; 2]> = BTreeMap::new();
    for (side, version) in versions.iter().enumerate() {
        for (module, scopes) in &version.modules {
            for (scope_index, scope) in scopes.iter().enumerate() {
                for (position, opaque) in scope.exports.opaque.iter().enumerate() {
                    let Some(key) = version.call_key(opaque) else {
                        continue;
                    };
                    calls.entry(key).or_default()[side].push((
                        module.clone(),
                        scope_index,
                        position,
                    ));
                }
            }
        }
    }
    for (key, places) in &calls {
        if versions[0].call_inputs(key) == versions[1].call_inputs(key) {
            continue;
        }
        for (side, version) in versions.iter().enumerate() {
            for (module, scope_index, position) in &places[side] {
                opaque_seeds(version, module, *scope_index, *position, &mut seeds);
            }
        }
    }
    let modules: BTreeSet<&Vec<String>> = versions
        .iter()
        .flat_map(|version| version.modules.keys())
        .collect();
    let moved: Vec<&Vec<String>> = modules
        .into_iter()
        .filter(|module| {
            placement_identity(versions[0], module) != placement_identity(versions[1], module)
        })
        .collect();
    // 置き場が変わった module の名前 (その module の名前を取り込む名前は閉包で足す)。
    for module in moved {
        for (side, version) in versions.iter().enumerate() {
            module_seeds(version, side, module, &mut seeds, &mut gaps);
        }
    }
    // edition が変われば、名前で始まる経路と `::` で始まる経路の読み方が変わる。
    if versions[0].edition != versions[1].edition {
        for (side, version) in versions.iter().enumerate() {
            for (module, scopes) in &version.modules {
                for (scope_index, scope) in scopes.iter().enumerate() {
                    for provider in &scope.exports.providers {
                        if let ProviderKind::Use { origin, .. } = provider.kind
                            && origin != UsePathOrigin::Anchored
                        {
                            seeds.insert((module.clone(), provider.name.clone()));
                        }
                    }
                    for (position, glob) in scope.exports.globs.iter().enumerate() {
                        if glob.origin != UsePathOrigin::Anchored {
                            glob_seeds(
                                version,
                                side,
                                module,
                                scope_index,
                                position,
                                &mut seeds,
                                &mut gaps,
                            );
                        }
                    }
                }
            }
        }
    }
    (seeds, gaps)
}

/// glob (版 `side` の module `module` の、スコープの添字・glob の添字) が持ち込み得る名前を種に足し、列挙
/// できない持ち込みを `gaps` に足す。
fn glob_seeds(
    version: &Index<'_>,
    side: usize,
    module: &[String],
    scope_index: usize,
    position: usize,
    seeds: &mut BTreeSet<Key>,
    gaps: &mut BTreeSet<Gap>,
) {
    let glob = &version.scopes(module)[scope_index].exports.globs[position];
    let (targets, open) = version.modules_of(module, &glob.module, glob.origin);
    if open {
        gaps.insert(Gap {
            side,
            hole: Hole::Glob(module.to_vec(), scope_index, position),
        });
    }
    for target in targets {
        let (names, holes) = version.glob_names(&target, module);
        seeds.extend(names.into_iter().map(|name| (module.to_vec(), name)));
        gaps.extend(holes.into_iter().map(|hole| Gap { side, hole }));
    }
}

/// 名前を列挙できない項目 (module `module` の、スコープの添字・添字) が作り得る名前と、それが字面に持つ
/// module の名前を種に足す。
fn opaque_seeds(
    version: &Index<'_>,
    module: &[String],
    scope_index: usize,
    position: usize,
    seeds: &mut BTreeSet<Key>,
) {
    for name in version.opaque_names_of(module, scope_index, position) {
        seeds.insert((module.to_vec(), name.clone()));
        // マクロが module を公開し得るので、その module の名前の公開も変わり得る。
        for named in version.modules_named(&name) {
            for scope in version.scopes(named) {
                for provider in &scope.exports.providers {
                    seeds.insert((named.clone(), provider.name.clone()));
                }
            }
        }
    }
}

/// 置き場の変わった module `module` の、外の名前に効き得る名前を種に足す: `pub` の名前を作るもの・`pub` の
/// glob が持ち込み得るもの、ほかの取り込みがこの module から引く名前 (子の module の `use super::x::y;` の
/// `x` は `pub` でなくてもよい)、`#[macro_export]` のマクロ (crate root の名前)。glob で取り込む module が
/// あれば、`pub` でない名前も (子の module の glob は見える)。どれにも当たらない名前は、置き場が変わっても
/// 外の名前を作らない。
fn module_seeds(
    version: &Index<'_>,
    side: usize,
    module: &[String],
    seeds: &mut BTreeSet<Key>,
    gaps: &mut BTreeSet<Gap>,
) {
    let shared = version.glob_importers(module).next().is_some();
    let used = version.names_used_from(module);
    for (scope_index, scope) in version.scopes(module).iter().enumerate() {
        for provider in &scope.exports.providers {
            if provider.scope.is_none()
                || shared
                || used.is_some_and(|used| used.contains(&provider.name))
            {
                seeds.insert((module.to_vec(), provider.name.clone()));
            }
        }
        for provider in &scope.exports.exported_macros {
            seeds.insert((Vec::new(), provider.name.clone()));
        }
        for (position, glob) in scope.exports.globs.iter().enumerate() {
            if glob.scope.is_none() || shared {
                glob_seeds(version, side, module, scope_index, position, seeds, gaps);
            }
        }
        for position in 0..scope.exports.opaque.len() {
            opaque_seeds(version, module, scope_index, position, seeds);
        }
    }
    // `pub` でない glob の持ち込む名前も、ほかの取り込みが引けば。
    for name in used.into_iter().flatten() {
        seeds.insert((module.to_vec(), name.clone()));
    }
}

/// 種 `seeds` から、それを取り込んで作る名前 (named の取り込み、glob、経路に名前を使うもの) をたどって
/// 足す。名前が上限を超えれば None。
fn reexporting_names(
    versions: &[&Index<'_>; 2],
    seeds: BTreeSet<Key>,
    gaps: &mut BTreeSet<Gap>,
) -> Option<BTreeSet<Key>> {
    let mut names = seeds.clone();
    let mut queue: Vec<Key> = seeds.into_iter().collect();
    let mut globs_seen: HashSet<(usize, Vec<String>, usize, usize)> = HashSet::new();
    while let Some(key) = queue.pop() {
        if names.len() > MAX_NAMES {
            return None;
        }
        let mut found: BTreeSet<Key> = BTreeSet::new();
        for (side, version) in versions.iter().enumerate() {
            // named の取り込みは、経路がこの module を指し得るものだけ。
            found.extend(
                version
                    .importers(&key.1)
                    .iter()
                    .filter(|importer| version.imports_from(importer, &key.1, &key.0))
                    .cloned(),
            );
            found.extend(
                version
                    .glob_importers(&key.0)
                    .map(|importer| (importer.clone(), key.1.clone())),
            );
            for user in version.path_users(&key.1) {
                match user {
                    User::Named(named) => {
                        found.insert(named.clone());
                    }
                    User::Glob(module, scope_index, position) => {
                        if globs_seen.insert((side, module.clone(), *scope_index, *position)) {
                            glob_seeds(
                                version,
                                side,
                                module,
                                *scope_index,
                                *position,
                                &mut found,
                                gaps,
                            );
                        }
                    }
                }
            }
        }
        for next in found {
            if names.insert(next.clone()) {
                queue.push(next);
            }
        }
    }
    Some(names)
}

/// 列挙できない持ち込み `gap` の名前を、この設定で crate の外へ出し得る module (持ち込みのある module と、
/// `pub` の glob で取り込む module の連なりのうち、公開の経路で届き得るもの) と、確かに出すか。
fn forwards(
    version: &Index<'_>,
    cone: &Cone<'_, '_>,
    resolver: &Resolver<'_, '_>,
    gap: &Gap,
) -> BTreeSet<(Vec<String>, bool)> {
    let definite = match &gap.hole {
        Hole::Glob(module, scope_index, position) => {
            let glob = &version.scopes(module)[*scope_index].exports.globs[*position];
            let present = resolver.glob_present(module, *scope_index, *position);
            if glob.scope.is_some() || present == Some(false) {
                return BTreeSet::new();
            }
            match resolver.glob_module(module, &glob.module, glob.origin) {
                // この設定で crate の module を指すなら、その名前は列挙してある。
                Some(Some(_)) => return BTreeSet::new(),
                Some(None) => present == Some(true),
                None => false,
            }
        }
        Hole::Opaque(module, scope_index, position) => {
            match resolver.opaque_present(module, *scope_index, *position) {
                Some(false) => return BTreeSet::new(),
                present => present == Some(true),
            }
        }
        Hole::Uncertain(module) => match resolver.uncertain(module) {
            Some(false) => return BTreeSet::new(),
            uncertain => uncertain == Some(true),
        },
    };
    let start = gap.module();
    // module → 確かに持ち込むか。
    let mut forwarded: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    forwarded.insert(start.clone(), definite);
    let importers: Vec<&Vec<String>> = cone.forwarding.get(start).into_iter().flatten().collect();
    loop {
        let mut changed = false;
        for importer in &importers {
            let mut reached: Option<bool> = None;
            for (scope_index, position, glob) in version.globs(importer) {
                let present = resolver.glob_present(importer, scope_index, position);
                if glob.scope.is_some() || present == Some(false) {
                    continue;
                }
                let sure = match resolver.glob_module(importer, &glob.module, glob.origin) {
                    Some(Some(target)) => match forwarded.get(&target) {
                        Some(sure) => *sure && present == Some(true),
                        None => continue,
                    },
                    Some(None) => continue,
                    None => false,
                };
                reached = Some(reached.unwrap_or(false) || sure);
            }
            let Some(sure) = reached else {
                continue;
            };
            let known = forwarded.get(*importer).copied();
            if known.is_none() || (sure && known == Some(false)) {
                forwarded.insert((*importer).clone(), sure);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    forwarded
        .into_iter()
        .filter_map(|(module, sure)| {
            let (definite, possible) = resolver.reach(&module);
            possible.then_some((module, sure && definite))
        })
        .collect()
}

/// 版 `version` の名前 `name` を作った `via` (先 `target`) の報告の置き場。
fn site_candidate(
    version: &Index<'_>,
    name: &Key,
    via: Via,
    target: &Target,
) -> Option<ApiSymbolCandidate> {
    let kind = match target {
        Target::Item(module, _, entry) => match &version.provider(module, *entry).1.kind {
            ProviderKind::Declared(kind) => (*kind).to_owned(),
            _ => "reexport".to_owned(),
        },
        // enum は擬似的な module として解くので、宣言の種類で示す。
        Target::Module(path) => path
            .split_last()
            .and_then(|(last, parent)| {
                version
                    .entries(&(parent.to_vec(), last.clone()))
                    .iter()
                    .find_map(|&entry| match version.provider(parent, entry).1.kind {
                        ProviderKind::Declared(kind) => Some(kind.to_owned()),
                        _ => None,
                    })
            })
            .unwrap_or_else(|| "module".to_owned()),
        Target::External(_) => "reexport".to_owned(),
    };
    let (scope, text) = match via {
        Via::Entry(entry) => {
            let (scope, provider) = version.provider(&name.0, entry);
            (scope, provider.text.clone())
        }
        Via::Glob(scope_index, position) => {
            let scope = version.scopes(&name.0).get(scope_index)?;
            (scope, scope.exports.globs.get(position)?.text.clone())
        }
    };
    Some(ApiSymbolCandidate {
        inline_module: (!scope.inline.is_empty()).then(|| scope.inline.join("::")),
        name: name.1.clone(),
        kind,
        file: scope.file.clone(),
        signature: text,
    })
}

/// 版 `version` の名前 `name` の、解けなかったときの報告の置き場 (名前を作るものか、module の glob)。
fn fallback_candidate(version: &Index<'_>, name: &Key) -> Option<ApiSymbolCandidate> {
    let (scope, text) = if let Some(&entry) = version.entries(name).first() {
        let (scope, provider) = version.provider(&name.0, entry);
        (scope, provider.text.clone())
    } else if let Some((scope_index, _, glob)) = version.globs(&name.0).next() {
        (&version.scopes(&name.0)[scope_index], glob.text.clone())
    } else {
        return None;
    };
    Some(ApiSymbolCandidate {
        inline_module: (!scope.inline.is_empty()).then(|| scope.inline.join("::")),
        name: name.1.clone(),
        kind: "reexport".to_owned(),
        file: scope.file.clone(),
        signature: text,
    })
}
