//! 版の lib crate の、module ごとの名前を作るものと、名前から引く索引 (Issue #306・#318)。
//!
//! 名前を作るもの ([`exports`]) を、置き場 (#305) の module の経路ごとにまとめ、(module, 名前) から引ける
//! ようにする。設定によらずに指し得るもの (条件を問わない経路の解き方、glob で持ち込み得る名前、逆引き) は、
//! 比べる名前の依存の範囲を集めるのに使う。設定ごとの名前解決は [`super::resolve`]。

use super::super::rust_dependencies::exports::{
    self, Glob, Opaque, Provider, ProviderKind, ScopeExports,
};
use super::super::rust_dependencies::placements::{Clause, Placements};
use crate::engine::rust_edition::RustEdition;
use crate::engine::rust_use_tree::UsePathOrigin;
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

/// (module の経路, 名前)。
pub(super) type Key = (Vec<String>, String);

/// スコープの中の場所 (module, スコープの添字, 添字)。
pub(super) type Place = (Vec<String>, usize, usize);

/// 呼び出しの組 (呼ぶマクロの名前の組, 呼び出しの経路の名前, 引数の名前 (呼ぶ crate のマクロが引数をマクロの
/// 名前か経路に使い得るときだけ), 引数の名前 (crate に定義の無いマクロを呼ぶときだけ。マクロの名前に使い
/// 得る))。
pub(super) type CallKey = (Vec<String>, Vec<String>, Vec<String>, Vec<String>);

/// glob が持ち込み得る名前と、列挙できない持ち込みがあるかのメモ。
type GlobMemo = HashMap<Place, Rc<(BTreeSet<String>, bool)>>;

/// module のスコープ 1 つ (置き場のファイルと、その中の inline module)。
pub(super) struct Scope {
    pub(super) file: String,
    /// ファイルの中の inline module の経路。
    pub(super) inline: Vec<String>,
    /// ファイルの置き場に着く条件 (OR)。
    pub(super) clauses: Vec<Clause>,
    pub(super) exports: Rc<ScopeExports>,
}

/// 名前を作るものの位置 (module のスコープの添字, スコープの中の添字)。crate root の `#[macro_export]` の
/// マクロは、置いたファイルのスコープを指す ([`Entry`])。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Entry {
    /// スコープの名前を作るもの。
    Provider(usize, usize),
    /// `#[macro_export]` のマクロ (置いた module, スコープの添字, 添字)。crate root の名前。
    Exported(usize, usize, usize),
}

/// 経路に名前を使うもの。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum User {
    /// named の取り込み (module, 名前)。
    Named(Key),
    /// glob (module, スコープの添字, glob の添字)。
    Glob(Vec<String>, usize, usize),
}

/// 版の lib crate の索引。
pub(super) struct Index<'p> {
    pub(super) placements: &'p Placements,
    /// lib target の edition (分からなければ None)。
    pub(super) edition: Option<RustEdition>,
    pub(super) modules: BTreeMap<Vec<String>, Vec<Scope>>,
    /// `#[macro_export]` のマクロを置いた (module, スコープの添字) (crate root の名前)。
    exported_macro_scopes: Vec<(Vec<String>, usize)>,
    /// (module, 名前) → 名前を作るもの。
    providers: HashMap<Key, Vec<Entry>>,
    /// ファイル → (module, スコープの添字)。
    pub(super) by_file: HashMap<String, Vec<(Vec<String>, usize)>>,
    /// 取り込む先の名前 → それを named で取り込む (module, 名前)。
    importers: HashMap<String, Vec<Key>>,
    /// 経路の名前 (全てのセグメント) → それを経路に使うもの。
    path_users: HashMap<String, Vec<User>>,
    /// glob の先になり得る module → glob を持つ module。
    glob_importers: HashMap<Vec<String>, BTreeSet<Vec<String>>>,
    /// module → 名前 → 型の名前空間で指し得る crate の module (どこかの設定で)。
    module_names: ModuleNames,
    /// 名前 → それが字面 (マクロの呼び出しと、呼び出す crate の `macro_rules!` の本体) に現れる、名前を
    /// 列挙できない項目 (module, スコープの添字, 添字)。
    opaque_by_name: HashMap<String, Vec<Place>>,
    /// マクロの名前 → 同じ名前の `macro_rules!` の定義。
    macro_definitions: HashMap<String, MacroDefinition>,
    /// 取り込みの別名 → 先の名前 (`use inner as alias;` の `alias` → `inner`。マクロを指し得るものだけ)。
    macro_aliases: HashMap<String, BTreeSet<String>>,
    /// マクロの呼び出しが作り得る名前の数の残り (超えれば `macro_over`)。
    macro_budget: std::cell::Cell<usize>,
    /// マクロの呼び出しが作り得る名前が多過ぎて、求め切れなかったか。
    macro_over: std::cell::Cell<bool>,
    /// module の名前の表が上限を超えて、求め切れなかったか。
    table_over: std::cell::Cell<bool>,
    /// マクロの名前 → 本体の呼び出しで閉じた、作り得る名前 (求めたものを覚える)。
    macro_closures: RefCell<HashMap<String, Rc<BTreeSet<String>>>>,
    /// (呼ぶマクロの名前の組, 呼び出しの経路の名前) → 呼び出しの見る定義を決める入力の指紋 (求めたものを
    /// 覚える)。
    bodies_memo: RefCell<HashMap<CallKey, String>>,
    /// 呼び出しの見る定義が crate の並びに依るとき (同じ名前の定義が 2 つ以上か、マクロを定義するマクロか
    /// `#[macro_use]` がある)、crate の全てのスコープのマクロの並び (置き場も) の指紋。
    textual_order: Option<String>,
    /// module → スコープごとの置き場 (ファイル・inline module・着く条件) の短い指紋 (長い条件を項目ごとに
    /// 写さない)。
    placed: HashMap<Vec<String>, Vec<String>>,
    /// (module, 名前) がマクロの名前空間に入り得るか (求めたものを覚える)。
    macro_key_memo: RefCell<HashMap<Key, bool>>,
    /// module → その module の glob の先になり得る module。
    glob_sources: HashMap<Vec<String>, BTreeSet<Vec<String>>>,
    /// 先を決め切れない glob (外部 crate など) を持つ module (どの名前のマクロも持ち込み得る)。
    open_globs: HashSet<Vec<String>>,
    /// 呼ぶと、引数をマクロの名前か経路に使い得るマクロの名前 (本体の呼び出しと別名をたどる。1 度だけ求める)。
    dynamic_names: OnceCell<HashSet<String>>,
    /// 別名をたどって crate のマクロの定義に着き得る名前 (1 度だけ求める)。
    defined_names: OnceCell<HashSet<String>>,
    /// 呼び出しの見る定義を決める入力のグラフ (1 度だけ作る)。
    input_graph: OnceCell<InputGraph>,
    /// glob (module, スコープの添字, glob の添字) が名前を持ち込み得るかのメモ (先と、列挙できない持ち込み)。
    glob_memo: RefCell<GlobMemo>,
    /// module → ほかの取り込みが、その module から引く名前 (named の先、経路の途中)。
    used_from: OnceCell<HashMap<Vec<String>, BTreeSet<String>>>,
}

/// 同じ名前の `macro_rules!` の定義をまとめたもの。
#[derive(Default)]
struct MacroDefinition {
    /// 同じ名前の定義の数。
    count: usize,
    /// 本体 (規則の右辺) に現れる名前。
    mentions: BTreeSet<String>,
    /// 本体で呼ぶマクロの名前。
    calls: BTreeSet<String>,
    /// 本体のどれかが、引数をマクロの名前か経路に使い得るか。
    dynamic: bool,
    /// 本体が引数を渡して呼ぶマクロの名前。
    forwards: BTreeSet<String>,
}

/// 呼び出しの入力のグラフの節の役割。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Role {
    /// マクロの名前 (呼ぶ名前・別名の先・定義の中の呼び出し)。マクロの名前空間に入り得るものだけを見る。
    Macro,
    /// 経路の名前 (呼び出し・取り込み・glob の経路と、module の別名の先)。型の名前空間に入り得るものだけを
    /// 見る。
    Path,
    /// 全ての glob (経路で引けるマクロを持ち込み得る)。
    Globs,
}

/// 呼び出しの見る定義を決める入力のグラフ。節は (役割, 名前) で、入力 (置き場ごとの字面のハッシュ) と、
/// たどる先の節を持つ。指紋は、SCC ごとに、中の節の入力と外のたどる先の SCC の指紋から求める (閉包を写さず、
/// 共有する部分は 1 度だけ求める)。
#[derive(Default)]
struct InputGraph {
    ids: HashMap<(Role, String), usize>,
    names: Vec<(Role, String)>,
    local: Vec<Vec<u64>>,
    edges: Vec<Vec<usize>>,
    digests: Vec<u64>,
}

/// 値のハッシュ (同じ版の比較の中だけで使う)。
fn hash_of<T: std::hash::Hash + ?Sized>(value: &T) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// 節 (役割, 名前) の、自身の入力のハッシュ。
fn member_hash(role: Role, name: &str, local: &[u64]) -> u64 {
    let mut sorted = local.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    hash_of(&(role, name, sorted))
}

impl InputGraph {
    /// 節 (役割, 名前) の添字 (無ければ作る)。
    fn id(&mut self, role: Role, name: &str) -> usize {
        if let Some(&id) = self.ids.get(&(role, name.to_owned())) {
            return id;
        }
        let id = self.names.len();
        self.ids.insert((role, name.to_owned()), id);
        self.names.push((role, name.to_owned()));
        self.local.push(Vec::new());
        self.edges.push(Vec::new());
        id
    }

    /// 節 `from` から (役割, 名前) へたどる。
    fn edge(&mut self, from: usize, role: Role, name: &str) {
        let to = self.id(role, name);
        self.edges[from].push(to);
    }

    /// 節の指紋を SCC ごとに求める (Tarjan。たどる先の SCC は先に閉じる)。
    fn finish(&mut self) {
        let count = self.names.len();
        let mut order = vec![usize::MAX; count];
        let mut low = vec![0usize; count];
        let mut on_stack = vec![false; count];
        let mut component = vec![usize::MAX; count];
        let mut stack: Vec<usize> = Vec::new();
        let mut components: Vec<Vec<usize>> = Vec::new();
        let mut next = 0usize;
        for root in 0..count {
            if order[root] != usize::MAX {
                continue;
            }
            let mut work: Vec<(usize, usize)> = vec![(root, 0)];
            order[root] = next;
            low[root] = next;
            next += 1;
            stack.push(root);
            on_stack[root] = true;
            while let Some(&(node, at)) = work.last() {
                if let Some(&to) = self.edges[node].get(at) {
                    if let Some(top) = work.last_mut() {
                        top.1 += 1;
                    }
                    if order[to] == usize::MAX {
                        order[to] = next;
                        low[to] = next;
                        next += 1;
                        stack.push(to);
                        on_stack[to] = true;
                        work.push((to, 0));
                    } else if on_stack[to] {
                        low[node] = low[node].min(order[to]);
                    }
                    continue;
                }
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    low[parent] = low[parent].min(low[node]);
                }
                if low[node] == order[node] {
                    let mut members = Vec::new();
                    while let Some(member) = stack.pop() {
                        on_stack[member] = false;
                        component[member] = components.len();
                        members.push(member);
                        if member == node {
                            break;
                        }
                    }
                    components.push(members);
                }
            }
        }
        let mut digests: Vec<u64> = vec![0; components.len()];
        for (index, members) in components.iter().enumerate() {
            let mut own: Vec<u64> = members
                .iter()
                .map(|&member| {
                    let (role, name) = &self.names[member];
                    member_hash(*role, name, &self.local[member])
                })
                .collect();
            own.sort_unstable();
            let mut successors: Vec<u64> = members
                .iter()
                .flat_map(|&member| self.edges[member].iter())
                .filter(|&&to| component[to] != index)
                .map(|&to| digests[component[to]])
                .collect();
            successors.sort_unstable();
            successors.dedup();
            digests[index] = hash_of(&(own, successors));
        }
        self.digests = (0..count).map(|node| digests[component[node]]).collect();
    }

    /// 節 (役割, 名前) の指紋 (無い節は、入力もたどる先も無い節と同じ)。
    fn digest(&self, role: Role, name: &str) -> u64 {
        match self.ids.get(&(role, name.to_owned())) {
            Some(&id) => self.digests[id],
            None => hash_of(&(vec![member_hash(role, name, &[])], Vec::<u64>::new())),
        }
    }
}

/// 名前を列挙できない持ち込み (glob の名前を列挙し切れない理由の場所)。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Hole {
    /// 外部 crate か決められない先の glob (module, スコープの添字, glob の添字)。
    Glob(Vec<String>, usize, usize),
    /// 名前を列挙できない項目 (module, スコープの添字, 添字)。
    Opaque(Vec<String>, usize, usize),
    /// 場所か中身を決められない module。
    Uncertain(Vec<String>),
}

/// module → 名前 → 型の名前空間で指し得る crate の module と、それ以外 (外部 crate・module でない項目) かも
/// しれないか。
type ModuleNames = HashMap<Vec<String>, BTreeMap<String, (BTreeSet<Vec<String>>, bool)>>;

/// 経路をたどるときに (module, 名前) を引く表 (先の module と、外部 crate・module でない項目かもしれないか)。
type Lookup<'l, 't> = dyn Fn(&[String], &str) -> Option<(&'t BTreeSet<Vec<String>>, bool)> + 'l;

/// マクロの呼び出しが作り得る名前 ((項目, 名前) の組と、覚えるマクロの閉包の名前) の数の上限。超えれば
/// 再公開した名前を探し切れなかったとする。
const MAX_MACRO_NAMES: usize = 2_000_000;

/// module の名前の表の大きさ (名前の項目と、名前の指し得る module の経路の数) の上限。超えれば再公開した名前を
/// 探し切れなかったとする。
pub(super) const MAX_MODULE_PATHS: usize = 1_000_000;

/// module の名前の表を広げる作業 (写す経路と、印を伝える名前の数) の上限。
const MAX_MODULE_WORK: usize = 20_000_000;

/// 字面の指紋 (16 進の短い字面)。
fn text_digest(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// スコープのマクロの定義・呼び出しと `mod` の宣言の並びの指紋。
fn order_digest(order: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    order.hash(&mut hasher);
    hasher.finish()
}

/// module → 名前 → 型の名前空間で指し得る crate の module (表を求める途中の集合)。
type ModuleSets = HashMap<Vec<String>, BTreeMap<String, BTreeSet<Vec<String>>>>;

/// 名前を作るものの、ファイルの索引ごとの抽出結果の共有 (旧新で同じ索引を引くファイルは 1 度だけ)。
pub(super) type ExportsCache =
    HashMap<(usize, Vec<String>), Rc<BTreeMap<Vec<String>, Rc<ScopeExports>>>>;

impl<'p> Index<'p> {
    pub(super) fn new(
        placements: &'p Placements,
        edition: Option<RustEdition>,
        cache: &mut ExportsCache,
    ) -> Self {
        let mut modules: BTreeMap<Vec<String>, Vec<Scope>> = BTreeMap::new();
        for (module, files) in &placements.files {
            for placement in files {
                if placement.clauses.is_empty() {
                    continue;
                }
                let Some(index) = &placement.index else {
                    continue;
                };
                let key = (Rc::as_ptr(index) as usize, module.clone());
                let scopes = cache
                    .entry(key)
                    .or_insert_with(|| {
                        Rc::new(
                            exports::exports(index, module)
                                .into_iter()
                                .map(|(scope, found)| (scope, Rc::new(found)))
                                .collect(),
                        )
                    })
                    .clone();
                for (inline, found) in scopes.iter() {
                    let mut full = module.clone();
                    full.extend(inline.iter().cloned());
                    modules.entry(full).or_default().push(Scope {
                        file: placement.file.clone(),
                        inline: inline.clone(),
                        clauses: placement.clauses.clone(),
                        exports: found.clone(),
                    });
                }
            }
        }
        let mut index = Self {
            placements,
            edition,
            modules,
            exported_macro_scopes: Vec::new(),
            providers: HashMap::new(),
            by_file: HashMap::new(),
            importers: HashMap::new(),
            path_users: HashMap::new(),
            glob_importers: HashMap::new(),
            module_names: HashMap::new(),
            opaque_by_name: HashMap::new(),
            macro_definitions: HashMap::new(),
            macro_aliases: HashMap::new(),
            macro_budget: std::cell::Cell::new(MAX_MACRO_NAMES),
            macro_over: std::cell::Cell::new(false),
            table_over: std::cell::Cell::new(false),
            macro_closures: RefCell::new(HashMap::new()),
            bodies_memo: RefCell::new(HashMap::new()),
            textual_order: None,
            placed: HashMap::new(),
            macro_key_memo: RefCell::new(HashMap::new()),
            glob_sources: HashMap::new(),
            open_globs: HashSet::new(),
            dynamic_names: OnceCell::new(),
            defined_names: OnceCell::new(),
            input_graph: OnceCell::new(),
            glob_memo: RefCell::new(HashMap::new()),
            used_from: OnceCell::new(),
        };
        let mut providers: HashMap<Key, Vec<Entry>> = HashMap::new();
        let mut by_file: HashMap<String, Vec<(Vec<String>, usize)>> = HashMap::new();
        let mut importers: HashMap<String, Vec<Key>> = HashMap::new();
        let mut path_users: HashMap<String, Vec<User>> = HashMap::new();
        let mut macro_aliases: HashMap<String, BTreeSet<String>> = HashMap::new();
        let mut exported_macro_scopes = Vec::new();
        for (module, scopes) in &index.modules {
            for (scope_index, scope) in scopes.iter().enumerate() {
                by_file
                    .entry(scope.file.clone())
                    .or_default()
                    .push((module.clone(), scope_index));
                for (position, provider) in scope.exports.providers.iter().enumerate() {
                    let key = (module.clone(), provider.name.clone());
                    providers
                        .entry(key.clone())
                        .or_default()
                        .push(Entry::Provider(scope_index, position));
                    if let ProviderKind::Use {
                        module: path, item, ..
                    } = &provider.kind
                    {
                        if !item.is_empty() {
                            importers.entry(item.clone()).or_default().push(key.clone());
                            if *item != provider.name {
                                macro_aliases
                                    .entry(provider.name.clone())
                                    .or_default()
                                    .insert(item.clone());
                            }
                        }
                        for segment in path {
                            path_users
                                .entry(segment.clone())
                                .or_default()
                                .push(User::Named(key.clone()));
                        }
                    }
                }
                for (position, glob) in scope.exports.globs.iter().enumerate() {
                    for segment in &glob.module {
                        path_users
                            .entry(segment.clone())
                            .or_default()
                            .push(User::Glob(module.clone(), scope_index, position));
                    }
                }
                if !scope.exports.exported_macros.is_empty() {
                    exported_macro_scopes.push((module.clone(), scope_index));
                }
            }
        }
        for (module_index, (module, scope_index)) in exported_macro_scopes.iter().enumerate() {
            let scope = &index.modules[module][*scope_index];
            for (position, provider) in scope.exports.exported_macros.iter().enumerate() {
                providers
                    .entry((Vec::new(), provider.name.clone()))
                    .or_default()
                    .push(Entry::Exported(module_index, *scope_index, position));
            }
        }
        index.providers = providers;
        index.by_file = by_file;
        index.importers = importers;
        index.path_users = path_users;
        index.exported_macro_scopes = exported_macro_scopes;
        // スコープの置き場 (ファイル・inline module・着く条件) の短い指紋。
        index.placed = index
            .modules
            .iter()
            .map(|(module, scopes)| {
                let placed = scopes
                    .iter()
                    .map(|scope| {
                        text_digest(&format!(
                            "{} {:?} {:?}",
                            scope.file,
                            scope.inline,
                            scope
                                .clauses
                                .iter()
                                .map(super::super::rust_dependencies::placements::Clause::identity)
                                .collect::<Vec<_>>()
                        ))
                    })
                    .collect();
                (module.clone(), placed)
            })
            .collect();
        let mut macro_definitions: HashMap<String, MacroDefinition> = HashMap::new();
        // crate の全てのスコープのマクロの並び (module の経路の順。置き場も)。
        let mut orders: Vec<String> = Vec::new();
        let mut macro_use = false;
        for (module, scopes) in &index.modules {
            for (scope_index, scope) in scopes.iter().enumerate() {
                let placed = &index.placed[module][scope_index];
                let order = order_digest(&scope.exports.macro_order);
                if !scope.exports.macro_order.is_empty() {
                    orders.push(format!("{module:?} {} {placed} {order:016x}", scope.file));
                }
                macro_use |= scope
                    .exports
                    .macro_order
                    .iter()
                    .any(|entry| entry.starts_with("use-"));
                for body in &scope.exports.macro_bodies {
                    let definition = macro_definitions.entry(body.name.clone()).or_default();
                    definition.count += 1;
                    definition.mentions.extend(body.mentions.iter().cloned());
                    definition.calls.extend(body.calls.iter().cloned());
                    definition.dynamic |= body.dynamic;
                    definition.forwards.extend(body.forwards.iter().cloned());
                }
            }
        }
        // 同じ名前のマクロを 2 度以上定義するか、マクロを定義するマクロか `#[macro_use]` があれば、呼び出しの
        // 見る定義は crate の並びに依る (textual scope。`#[macro_use]` の module の定義は、後の module で取り込み
        // より先に見る)。そのときは全ての呼び出しの指紋に crate の並びを入れる。
        if macro_use
            || macro_definitions.values().any(|definition| {
                definition.count >= 2 || definition.mentions.contains("macro_rules")
            })
        {
            orders.sort();
            index.textual_order = Some(text_digest(&orders.join("\n")));
        }
        index.macro_definitions = macro_definitions;
        // マクロの別名の候補は、先が (別名をたどって) マクロの定義の名前に着き得るもの (型や値の別名の長い
        // 連なりはたどらない)。
        let mut reachable: HashMap<&str, Vec<&str>> = HashMap::new();
        for (alias, targets) in &macro_aliases {
            for target in targets {
                reachable
                    .entry(target.as_str())
                    .or_default()
                    .push(alias.as_str());
            }
        }
        let mut kept: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = index.macro_definitions.keys().map(String::as_str).collect();
        while let Some(name) = stack.pop() {
            for &alias in reachable.get(name).into_iter().flatten() {
                if kept.insert(alias) {
                    stack.push(alias);
                }
            }
        }
        index.macro_aliases = macro_aliases
            .iter()
            .filter(|(alias, _)| kept.contains(alias.as_str()))
            .map(|(alias, targets)| (alias.clone(), targets.clone()))
            .collect();
        // マクロが作り得る名前は、module の名前の表では中身の分からない名前 (マクロが module を作り得る)。
        // 別名を絞る前の候補で、module ごとの和だけを求める。
        let generated: Vec<Key> = index.generated_names();
        // module の名前の表は、名前を作るものを引けるようになってから求め、glob の先はその表で解く。
        index.module_names = index.module_name_table(&generated);
        let mut glob_importers: HashMap<Vec<String>, BTreeSet<Vec<String>>> = HashMap::new();
        let mut glob_sources: HashMap<Vec<String>, BTreeSet<Vec<String>>> = HashMap::new();
        let mut open_globs: HashSet<Vec<String>> = HashSet::new();
        for (module, scopes) in &index.modules {
            for scope in scopes {
                for glob in &scope.exports.globs {
                    let (targets, open) = index.modules_of(module, &glob.module, glob.origin);
                    if open {
                        open_globs.insert(module.clone());
                    }
                    for target in targets {
                        glob_sources
                            .entry(module.clone())
                            .or_default()
                            .insert(target.clone());
                        glob_importers
                            .entry(target)
                            .or_default()
                            .insert(module.clone());
                    }
                }
            }
        }
        index.glob_importers = glob_importers;
        index.glob_sources = glob_sources;
        index.open_globs = open_globs;
        // マクロの別名は、取り込みの名前がマクロの名前空間に入り得るもの (型の別名と同じ名前のマクロの本体を
        // 混ぜない) だけにして、作り得る名前を求め直す。
        index.macro_aliases = index.macro_import_aliases();
        // 事前の計算で上限を超えていなければ、絞った別名で求め直す (超えていれば表が足りないので戻さない)。
        if !index.macro_over.get() {
            index.macro_closures.borrow_mut().clear();
            index.macro_budget.set(MAX_MACRO_NAMES);
            index.opaque_by_name = index.opaque_names();
        }
        index
    }

    /// module ごとの、名前を列挙できない項目が作り得る名前 (module の名前の表の、中身の分からない名前)。
    fn generated_names(&self) -> Vec<Key> {
        let mut generated: Vec<Key> = Vec::new();
        for (module, scopes) in &self.modules {
            // 同じマクロを何度呼んでも、閉包は 1 度だけ足す。
            let mut names: BTreeSet<String> = BTreeSet::new();
            let mut called: BTreeSet<&str> = BTreeSet::new();
            for scope in scopes {
                for opaque in &scope.exports.opaque {
                    if !self.spend_macro_names(1 + opaque.mentions.len()) {
                        return generated;
                    }
                    names.extend(opaque.mentions.iter().cloned());
                    for name in opaque.macro_name.iter().chain(&opaque.calls) {
                        called.extend(self.macro_targets(name));
                    }
                }
            }
            for target in called {
                names.extend(self.macro_closure(target).iter().cloned());
                if self.macro_over.get() {
                    return generated;
                }
            }
            if !self.spend_macro_names(names.len()) {
                return generated;
            }
            generated.extend(names.into_iter().map(|name| (module.clone(), name)));
        }
        generated
    }

    /// マクロの呼び出しが作り得る名前が多過ぎて、求め切れなかったか。
    pub(super) fn table_over(&self) -> bool {
        self.table_over.get()
    }

    /// マクロの呼び出しが作り得る名前が多過ぎて、求め切れなかったか。
    pub(super) fn macro_over(&self) -> bool {
        self.macro_over.get()
    }

    /// マクロの呼び出しが作り得る名前を `count` 個使う。残りが足りなければ求め切れなかったとし、偽。
    fn spend_macro_names(&self, count: usize) -> bool {
        match self.macro_budget.get().checked_sub(count) {
            Some(rest) => {
                self.macro_budget.set(rest);
                true
            }
            None => {
                self.macro_over.set(true);
                false
            }
        }
    }

    /// マクロの名前 `name` が指し得るマクロの名前 (自身と、取り込みの別名の先。別名の別名もたどる)。
    fn macro_targets<'s>(&'s self, name: &'s str) -> BTreeSet<&'s str> {
        let mut found: BTreeSet<&str> = BTreeSet::new();
        let mut stack = vec![name];
        while let Some(current) = stack.pop() {
            if found.insert(current) {
                stack.extend(
                    self.macro_aliases
                        .get(current)
                        .into_iter()
                        .flatten()
                        .map(String::as_str),
                );
            }
        }
        found
    }

    /// (module, 名前) `key` がマクロの名前空間に入り得るか: `macro_rules!` か `#[macro_export]` のマクロの名前か、
    /// それを (named の取り込みか glob で) 取り込むか、先を解けない named の取り込みか。`key` からたどれる
    /// (module, 名前) の中で最小の不動点を求めて覚える (全ての組を前もって数え上げない)。
    fn is_macro_key(&self, key: &Key) -> bool {
        if let Some(&known) = self.macro_key_memo.borrow().get(key) {
            return known;
        }
        let mut ids: HashMap<Key, usize> = HashMap::from([(key.clone(), 0)]);
        let mut keys: Vec<Key> = vec![key.clone()];
        let mut value: Vec<bool> = Vec::new();
        let mut depends: Vec<Vec<usize>> = Vec::new();
        let mut at = 0;
        while at < keys.len() {
            let (module, name) = keys[at].clone();
            let memo = self.macro_key_memo.borrow().get(&keys[at]).copied();
            let mut direct = memo.unwrap_or(false);
            let mut next: Vec<Key> = Vec::new();
            // たどる組の数もマクロの名前の予算から払う (超えれば入り得るとし、覚えない)。
            if memo.is_none() && !self.spend_macro_names(1) {
                return true;
            }
            if memo.is_none() {
                // 先を決め切れない glob は、どの名前のマクロも持ち込み得る。
                direct |= self.open_globs.contains(&module);
                for &entry in self.entries(&keys[at]) {
                    let (_, provider) = self.provider(&module, entry);
                    match &provider.kind {
                        _ if matches!(entry, Entry::Exported(..)) => direct = true,
                        ProviderKind::Declared("macro") => direct = true,
                        ProviderKind::Use {
                            module: path,
                            item,
                            origin,
                        } if !item.is_empty() && provider.namespaces & exports::MACRO != 0 => {
                            let (targets, open) = self.modules_of(&module, path, *origin);
                            direct |= open;
                            next.extend(targets.into_iter().map(|target| (target, item.clone())));
                        }
                        _ => {}
                    }
                }
                for source in self.glob_sources.get(&module).into_iter().flatten() {
                    next.push((source.clone(), name.clone()));
                }
            }
            let mut deps = Vec::new();
            for next_key in next {
                let id = match ids.get(&next_key) {
                    Some(&id) => id,
                    None => {
                        ids.insert(next_key.clone(), keys.len());
                        keys.push(next_key);
                        keys.len() - 1
                    }
                };
                deps.push(id);
            }
            value.push(direct);
            depends.push(deps);
            at += 1;
        }
        // 最小の不動点 (依存のどれかが入り得れば入り得る)。
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); keys.len()];
        for (from, deps) in depends.iter().enumerate() {
            for &to in deps {
                dependents[to].push(from);
            }
        }
        let mut queue: Vec<usize> = (0..keys.len()).filter(|&index| value[index]).collect();
        while let Some(index) = queue.pop() {
            for &from in &dependents[index] {
                if !value[from] {
                    value[from] = true;
                    queue.push(from);
                }
            }
        }
        let found = value[0];
        let mut memo = self.macro_key_memo.borrow_mut();
        for (index, key) in keys.into_iter().enumerate() {
            memo.insert(key, value[index]);
        }
        found
    }

    /// provider `provider` (module `module` の) がマクロの名前空間に入り得るか: マクロの定義か、先がマクロの
    /// 名前空間に入り得る named の取り込みか、先を解けない named の取り込み。
    fn provider_is_macro(&self, module: &[String], provider: &Provider) -> bool {
        match &provider.kind {
            ProviderKind::Declared(_) => provider.namespaces & exports::MACRO != 0,
            ProviderKind::Use {
                module: path,
                item,
                origin,
            } => {
                !item.is_empty() && provider.namespaces & exports::MACRO != 0 && {
                    let (targets, open) = self.modules_of(module, path, *origin);
                    open || targets
                        .into_iter()
                        .any(|target| self.is_macro_key(&(target, item.clone())))
                }
            }
            ProviderKind::ExternCrate(_) => false,
        }
    }

    /// 取り込みの別名のうち、マクロを指し得るもの (別名 → 先の名前)。候補 (今の `macro_aliases`) のうち、
    /// 取り込みの先の (module, 名前) がマクロの名前空間に入り得るか、先を解けないものだけを残す。
    fn macro_import_aliases(&self) -> HashMap<String, BTreeSet<String>> {
        let mut aliases: HashMap<String, BTreeSet<String>> = HashMap::new();
        for (module, scopes) in &self.modules {
            for scope in scopes {
                for provider in &scope.exports.providers {
                    let ProviderKind::Use {
                        module: path,
                        item,
                        origin,
                    } = &provider.kind
                    else {
                        continue;
                    };
                    let candidate = provider.namespaces & exports::MACRO != 0
                        && self
                            .macro_aliases
                            .get(&provider.name)
                            .is_some_and(|targets| targets.contains(item));
                    if !candidate {
                        continue;
                    }
                    // 取り込みの先がマクロの名前空間に入り得るか (取り込む側の名前は、同じ名前の
                    // `macro_rules!` があればマクロになるので見ない)。
                    let (targets, open) = self.modules_of(module, path, *origin);
                    let found = targets
                        .into_iter()
                        .any(|target| self.is_macro_key(&(target, item.clone())));
                    if open || found {
                        aliases
                            .entry(provider.name.clone())
                            .or_default()
                            .insert(item.clone());
                    }
                }
            }
        }
        aliases
    }

    /// マクロ `name` の本体と、本体が (別名もたどって) 呼ぶマクロの本体に現れる名前。求めたものを覚える (呼ば
    /// れるマクロの分だけ求める)。
    fn macro_closure(&self, name: &str) -> Rc<BTreeSet<String>> {
        if let Some(found) = self.macro_closures.borrow().get(name) {
            return found.clone();
        }
        if self.macro_over.get() {
            return Rc::new(BTreeSet::new());
        }
        let mut names: BTreeSet<String> = BTreeSet::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = vec![name];
        while let Some(current) = stack.pop() {
            if !seen.insert(current) {
                continue;
            }
            // たどる定義の数も上限に数える (名前の無い長い呼び出しの連なり)。
            if !self.spend_macro_names(1) {
                return Rc::new(BTreeSet::new());
            }
            if let Some(definition) = self.macro_definitions.get(current) {
                names.extend(definition.mentions.iter().cloned());
                for call in &definition.calls {
                    stack.extend(self.macro_targets(call));
                }
            }
        }
        // 覚える名前の数にも上限を設ける (呼ばれるマクロが多く、それぞれの閉包が長いとき)。
        if !self.spend_macro_names(names.len()) {
            names.clear();
        }
        let found = Rc::new(names);
        self.macro_closures
            .borrow_mut()
            .insert(name.to_owned(), found.clone());
        found
    }

    /// 名前を列挙できない項目を、作り得る名前から引けるようにする。
    fn opaque_names(&self) -> HashMap<String, Vec<Place>> {
        let mut by_name: HashMap<String, Vec<Place>> = HashMap::new();
        for (module, scopes) in &self.modules {
            for (scope_index, scope) in scopes.iter().enumerate() {
                for position in 0..scope.exports.opaque.len() {
                    let names = self.opaque_names_of(module, scope_index, position);
                    if self.macro_over.get() || !self.spend_macro_names(names.len()) {
                        return by_name;
                    }
                    for name in names {
                        by_name.entry(name).or_default().push((
                            module.clone(),
                            scope_index,
                            position,
                        ));
                    }
                }
            }
        }
        by_name
    }

    /// 名前を列挙できない項目 (module, スコープの添字, 添字) の字面と、呼び出す crate のマクロの本体に現れる
    /// 名前 (その項目が作り得る名前。本体が呼び出すマクロも、別名の呼び出しは先のマクロ)。
    pub(super) fn opaque_names_of(
        &self,
        module: &[String],
        scope_index: usize,
        position: usize,
    ) -> BTreeSet<String> {
        let Some(opaque) = self
            .scopes(module)
            .get(scope_index)
            .and_then(|scope| scope.exports.opaque.get(position))
        else {
            return BTreeSet::new();
        };
        let mut names: BTreeSet<String> = opaque.mentions.iter().cloned().collect();
        for called in opaque.macro_name.iter().chain(&opaque.calls) {
            for target in self.macro_targets(called) {
                names.extend(self.macro_closure(target).iter().cloned());
            }
        }
        names
    }

    /// 名前を列挙できない項目 (module, スコープの添字, 添字) の、呼び出しの見る定義を決める入力の指紋
    /// ([`Index::call_inputs`])。マクロを呼ばない項目は空。
    pub(super) fn opaque_bodies_of(
        &self,
        module: &[String],
        scope_index: usize,
        position: usize,
    ) -> String {
        let Some(opaque) = self
            .scopes(module)
            .get(scope_index)
            .and_then(|scope| scope.exports.opaque.get(position))
        else {
            return String::new();
        };
        self.call_key(opaque)
            .map(|key| self.call_inputs(&key))
            .unwrap_or_default()
    }

    /// 呼び出し `opaque` の組 ([`CallKey`])。マクロを呼ばない項目は None。
    pub(super) fn call_key(&self, opaque: &Opaque) -> Option<CallKey> {
        let called: Vec<String> = opaque
            .macro_name
            .iter()
            .chain(&opaque.calls)
            .cloned()
            .collect();
        if called.is_empty() {
            return None;
        }
        let arguments = if self.passes_arguments(&called) {
            opaque.mentions.clone()
        } else {
            Vec::new()
        };
        // crate に定義の無いマクロ (外部 crate のものなど。引数の中で呼ぶものも) は、本体が分からないので、引数を
        // マクロの名前に使い得る。作る名前は引数だけなので、効くのは crate のマクロの定義に着き得る引数の名前だけ。
        let macro_arguments =
            if arguments.is_empty() && called.iter().any(|name| !self.is_defined(name)) {
                opaque
                    .mentions
                    .iter()
                    .filter(|name| self.is_defined(name))
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };
        Some((called, opaque.paths.clone(), arguments, macro_arguments))
    }

    /// 名前 `name` が、別名をたどって crate のマクロの定義に着き得るか。
    fn is_defined(&self, name: &str) -> bool {
        self.defined_names
            .get_or_init(|| {
                self.reach_back(self.macro_definitions.keys().map(String::as_str), false)
            })
            .contains(name)
    }

    /// 名前 `called` のマクロ (別名の先と、本体が呼ぶマクロも) のどれかが、引数をマクロの名前か経路に使い得るか。
    fn passes_arguments(&self, called: &[String]) -> bool {
        let dynamic = self.dynamic_names.get_or_init(|| self.find_dynamic_names());
        called.iter().any(|name| dynamic.contains(name.as_str()))
    }

    /// 呼ぶと、引数をマクロの名前か経路に使い得るマクロの名前: 本体がそうする定義 (引数をマクロの名前か経路に
    /// 使うか、crate に定義の無いマクロへ引数を渡す) の名前から、別名と本体の呼び出しの辺を逆にたどる。
    fn find_dynamic_names(&self) -> HashSet<String> {
        let starts: Vec<&str> = self
            .macro_definitions
            .iter()
            .filter(|(_, definition)| {
                definition.dynamic
                    || definition
                        .forwards
                        .iter()
                        .any(|callee| !self.is_defined(callee))
            })
            .map(|(name, _)| name.as_str())
            .collect();
        self.reach_back(starts.into_iter(), true)
    }

    /// 名前 `starts` から、別名の辺 (先 → 別名) を逆にたどって着く名前 (`calls` なら、本体の呼び出しの辺
    /// (呼ばれる名前 → 呼ぶ定義の名前) も)。辺は名前のまま持ち、別名の閉包を展開しない。
    fn reach_back<'s>(
        &'s self,
        starts: impl Iterator<Item = &'s str>,
        calls: bool,
    ) -> HashSet<String> {
        let mut back: HashMap<&str, Vec<&str>> = HashMap::new();
        for (alias, targets) in &self.macro_aliases {
            for target in targets {
                back.entry(target.as_str())
                    .or_default()
                    .push(alias.as_str());
            }
        }
        if calls {
            for (name, definition) in &self.macro_definitions {
                for call in &definition.calls {
                    back.entry(call.as_str()).or_default().push(name.as_str());
                }
            }
        }
        let mut found: HashSet<String> = HashSet::new();
        let mut stack: Vec<&str> = starts.collect();
        while let Some(name) = stack.pop() {
            if !found.insert(name.to_owned()) {
                continue;
            }
            stack.extend(back.get(name).into_iter().flatten().copied());
        }
        found
    }

    /// 呼び出しのマクロの名前に届き得る名前: 呼び出しの呼ぶ名前と引数の名前 (引数を使い得るとき)、定義の名前と
    /// 本体の呼び出しの名前 (呼ぶマクロが引数を使い得るなら本体の名前も) から、その名前の named の取り込みの
    /// 先の名前をたどる (これ以外の取り込みは、マクロの名前空間に入り得るかを問わない)。
    fn macro_relevant_names(&self) -> HashSet<String> {
        let mut stack: Vec<String> = Vec::new();
        let mut items: HashMap<&str, Vec<&str>> = HashMap::new();
        for scopes in self.modules.values() {
            for scope in scopes {
                for opaque in &scope.exports.opaque {
                    if let Some((called, _, arguments, macro_arguments)) = self.call_key(opaque) {
                        stack.extend(called);
                        stack.extend(arguments);
                        stack.extend(macro_arguments);
                    }
                }
                for body in &scope.exports.macro_bodies {
                    stack.push(body.name.clone());
                    stack.extend(body.calls.iter().cloned());
                    if self.passes_arguments(&body.calls) {
                        stack.extend(body.mentions.iter().cloned());
                    } else if body.calls.iter().any(|call| !self.is_defined(call)) {
                        stack.extend(
                            body.mentions
                                .iter()
                                .filter(|name| self.is_defined(name))
                                .cloned(),
                        );
                    }
                }
                for provider in &scope.exports.providers {
                    if let ProviderKind::Use { item, .. } = &provider.kind
                        && !item.is_empty()
                    {
                        items
                            .entry(provider.name.as_str())
                            .or_default()
                            .push(item.as_str());
                    }
                }
            }
        }
        let mut relevant: HashSet<String> = HashSet::new();
        while let Some(name) = stack.pop() {
            if relevant.contains(&name) {
                continue;
            }
            stack.extend(
                items
                    .get(name.as_str())
                    .into_iter()
                    .flatten()
                    .map(|item| (*item).to_owned()),
            );
            relevant.insert(name);
        }
        relevant
    }

    /// 呼び出しの組 `key` の、見る定義を決める入力の指紋: 入力のグラフ ([`InputGraph`]) の、呼ぶ名前のマクロの
    /// 節と、経路の名前の節と、引数の名前 (呼ぶマクロが引数をマクロの名前か経路に使い得るとき) のマクロと
    /// 経路の節の指紋と、crate の並び (textual scope) と edition。同じ組は覚える。
    pub(super) fn call_inputs(&self, key: &CallKey) -> String {
        if let Some(found) = self.bodies_memo.borrow().get(key) {
            return found.clone();
        }
        let (called, paths, arguments, macro_arguments) = key;
        let graph = self.input_graph.get_or_init(|| self.build_input_graph());
        let digests: Vec<u64> = called
            .iter()
            .map(|name| graph.digest(Role::Macro, name))
            .chain(paths.iter().map(|name| graph.digest(Role::Path, name)))
            .chain(arguments.iter().flat_map(|name| {
                [
                    graph.digest(Role::Macro, name),
                    graph.digest(Role::Path, name),
                ]
            }))
            .chain(
                macro_arguments
                    .iter()
                    .map(|name| graph.digest(Role::Macro, name)),
            )
            .collect();
        let found = format!(
            "{:016x}",
            hash_of(&(digests, &self.textual_order, format!("{:?}", self.edition)))
        );
        self.bodies_memo
            .borrow_mut()
            .insert(key.clone(), found.clone());
        found
    }

    /// 呼び出しの見る定義を決める入力のグラフを作る:
    /// - マクロの名前の節: その名前の `macro_rules!` の定義 (字面・条件・置き場・並び。定義の中の呼び出しの
    ///   名前と経路へたどる)、マクロの名前空間に入り得る取り込み (置き場ごと。経路の名前と先のマクロの名前へ
    ///   たどる) と `#[macro_export]`。経路で引ける (取り込み・`#[macro_export]`) なら、全ての glob へたどる
    /// - 経路の名前の節: 型の名前空間に入り得る provider (module・型・取り込み・`extern crate`。置き場ごと。
    ///   取り込みの経路の名前と先の名前へたどる)
    /// - glob の節: 全ての glob (置き場ごと。経路の名前へたどる)
    fn build_input_graph(&self) -> InputGraph {
        let mut graph = InputGraph::default();
        let globs = graph.id(Role::Globs, "");
        let relevant = self.macro_relevant_names();
        for (module, scopes) in &self.modules {
            for (scope_index, scope) in scopes.iter().enumerate() {
                let placed = &self.placed[module][scope_index];
                let order = order_digest(&scope.exports.macro_order);
                for provider in &scope.exports.providers {
                    let entry = hash_of(&format!(
                        "{module:?} {placed} {}",
                        exports::provider_entry(provider)
                    ));
                    let use_path = match &provider.kind {
                        ProviderKind::Use {
                            module: path, item, ..
                        } => Some((path, item)),
                        _ => None,
                    };
                    if relevant.contains(&provider.name) && self.provider_is_macro(module, provider)
                    {
                        let id = graph.id(Role::Macro, &provider.name);
                        graph.local[id].push(entry);
                        if let Some((path, item)) = use_path {
                            for segment in path {
                                graph.edge(id, Role::Path, segment);
                            }
                            if !item.is_empty() {
                                graph.edge(id, Role::Macro, item);
                            }
                            graph.edges[id].push(globs);
                        }
                    }
                    if provider.namespaces & exports::TYPE != 0 {
                        let id = graph.id(Role::Path, &provider.name);
                        graph.local[id].push(entry);
                        if let Some((path, item)) = use_path {
                            for segment in path {
                                graph.edge(id, Role::Path, segment);
                            }
                            if !item.is_empty() {
                                graph.edge(id, Role::Path, item);
                            }
                        }
                    }
                }
                for provider in &scope.exports.exported_macros {
                    let id = graph.id(Role::Macro, &provider.name);
                    graph.local[id].push(hash_of(&format!(
                        "export {module:?} {placed} {}",
                        exports::provider_entry(provider)
                    )));
                    graph.edges[id].push(globs);
                }
                for glob in &scope.exports.globs {
                    graph.local[globs].push(hash_of(&format!(
                        "glob {module:?} {placed} {}",
                        exports::glob_entry(glob)
                    )));
                    for segment in &glob.module {
                        graph.edge(globs, Role::Path, segment);
                    }
                }
                for body in &scope.exports.macro_bodies {
                    let id = graph.id(Role::Macro, &body.name);
                    graph.local[id]
                        .push(hash_of(&format!("{} @ {placed} {order:016x}", body.text)));
                    for call in &body.calls {
                        graph.edge(id, Role::Macro, call);
                    }
                    for segment in &body.paths {
                        graph.edge(id, Role::Path, segment);
                    }
                    // 本体が呼ぶマクロが引数をマクロの名前か経路に使い得るなら、本体が渡す名前もたどる (crate に
                    // 定義の無いマクロを呼ぶなら、crate のマクロの定義に着き得る名前を、マクロの名前として)。
                    if self.passes_arguments(&body.calls) {
                        for name in &body.mentions {
                            graph.edge(id, Role::Macro, name);
                            graph.edge(id, Role::Path, name);
                        }
                    } else if body.calls.iter().any(|call| !self.is_defined(call)) {
                        for name in body.mentions.iter().filter(|name| self.is_defined(name)) {
                            graph.edge(id, Role::Macro, name);
                        }
                    }
                }
            }
        }
        graph.finish();
        graph
    }

    /// 最後の名前が `name` の module。
    pub(super) fn modules_named<'s>(
        &'s self,
        name: &'s str,
    ) -> impl Iterator<Item = &'s Vec<String>> + 's {
        self.modules
            .keys()
            .filter(move |module| module.last().map(String::as_str) == Some(name))
    }

    /// module `module` の glob (スコープの添字, glob の添字) が、名前 `name` を持ち込み得るか (どこかの設定で。
    /// 先を決め切れないか、列挙できない持ち込みがあれば持ち込み得る)。
    pub(super) fn glob_may_bring(
        &self,
        module: &[String],
        scope_index: usize,
        position: usize,
        name: &str,
    ) -> bool {
        let key = (module.to_vec(), scope_index, position);
        let known = self.glob_memo.borrow().get(&key).cloned();
        let found = match known {
            Some(found) => found,
            None => {
                let Some(glob) = self
                    .scopes(module)
                    .get(scope_index)
                    .and_then(|scope| scope.exports.globs.get(position))
                else {
                    return false;
                };
                let (targets, mut open) = self.modules_of(module, &glob.module, glob.origin);
                let mut names = BTreeSet::new();
                for target in targets {
                    let (found, holes) = self.glob_names(&target, module);
                    names.extend(found);
                    open |= !holes.is_empty();
                }
                let found = Rc::new((names, open));
                self.glob_memo.borrow_mut().insert(key, found.clone());
                found
            }
        };
        found.1 || found.0.contains(name)
    }

    /// module `module` の名前を列挙できない項目のうち、名前 `name` を作り得るもの (スコープの添字, 添字)。
    pub(super) fn opaque_affecting(&self, module: &[String], name: &str) -> Vec<(usize, usize)> {
        self.opaque_by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter(|(placed, _, _)| placed == module)
            .map(|(_, scope_index, position)| (*scope_index, *position))
            .collect()
    }

    /// 名前 `name` を字面に持つ、名前を列挙できない項目 (module, スコープの添字, 添字)。その module を
    /// 公開し得る (`pub use crate::hidden::api;` をマクロが作る)。
    pub(super) fn opaque_mentioning(&self, name: &str) -> &[Place] {
        self.opaque_by_name.get(name).map_or(&[], Vec::as_slice)
    }

    /// (module, 名前) `importer` の named の取り込みのうち、先の名前 `item` を module `module` から取り込み得る
    /// ものがあるか (経路の解き方は条件を問わない)。
    pub(super) fn imports_from(&self, importer: &Key, item: &str, module: &[String]) -> bool {
        self.entries(importer).iter().any(|&entry| {
            let (_, provider) = self.provider(&importer.0, entry);
            match &provider.kind {
                ProviderKind::Use {
                    module: path,
                    item: target,
                    origin,
                } if target == item => self
                    .modules_of(&importer.0, path, *origin)
                    .0
                    .iter()
                    .any(|found| found.as_slice() == module),
                _ => false,
            }
        })
    }

    /// (module, 名前) が型の名前空間で指し得る crate の module を、子の module の宣言・named の取り込み・
    /// glob で持ち込む名前から求める (どこかの設定で指し得るもの)。先の module の集合は、増えた (module,
    /// 名前) を読む規則だけを解き直す最小不動点 (別名の長い連なりも 1 度ずつ)。外部 crate・module でない
    /// 項目かもしれない印は、決まった集合の上で、印の付いた (module, 名前) を読む規則だけをたどる最小
    /// 不動点。マクロが作り得る (module, 名前) `generated` は、中身の分からない名前 (印の起点) にする。
    fn module_name_table(&self, generated: &[Key]) -> ModuleNames {
        /// 名前を作る規則。
        enum Rule<'s> {
            /// named の取り込み (module, 名前, 経路 (先の名前まで), 起点)。
            Use(&'s Vec<String>, &'s str, Vec<String>, UsePathOrigin),
            /// glob (module, 経路, 起点)。
            Glob(&'s Vec<String>, &'s [String], UsePathOrigin),
        }
        /// 表の大きさ (項目と、名前の指し得る module の経路の数) と、写す作業の量。上限を超えれば表を作るのを
        /// やめる。
        #[derive(Default)]
        struct Budget {
            size: usize,
            work: usize,
            over: bool,
        }
        impl Budget {
            /// 大きさ `size` と作業 `work` を使う。上限を超えれば偽。
            fn spend(&mut self, size: usize, work: usize) -> bool {
                self.size += size;
                self.work += work;
                if self.size > MAX_MODULE_PATHS || self.work > MAX_MODULE_WORK {
                    self.over = true;
                }
                !self.over
            }
        }
        // (module, 名前) の集合を `modules` で広げる (名前が無ければ作る)。作ったか広げたら真。項目・経路・作業を
        // `budget` に数え、上限を超えたら途中でやめる。
        fn grow(
            sets: &mut ModuleSets,
            module: &[String],
            name: &str,
            modules: &BTreeSet<Vec<String>>,
            budget: &mut Budget,
        ) -> bool {
            if budget.over {
                return false;
            }
            let names = sets.entry(module.to_vec()).or_default();
            let created = !names.contains_key(name);
            let entry = names.entry(name.to_owned()).or_default();
            if created && !budget.spend(1, 1) {
                return false;
            }
            let mut grown = false;
            for target in modules {
                let added = entry.insert(target.clone());
                grown |= added;
                if !budget.spend(usize::from(added), 1) {
                    break;
                }
            }
            created || grown
        }
        let mut budget = Budget::default();
        let mut sets: ModuleSets = HashMap::new();
        // 外部 crate・module でない項目の名前 (印の起点)。
        let mut fixed_open: Vec<Key> = Vec::new();
        for (module, name) in generated {
            grow(&mut sets, module, name, &BTreeSet::new(), &mut budget);
            fixed_open.push((module.clone(), name.clone()));
        }
        let mut rules: Vec<Rule<'_>> = Vec::new();
        for (module, scopes) in &self.modules {
            sets.entry(module.clone()).or_default();
            for scope in scopes {
                for provider in &scope.exports.providers {
                    if provider.namespaces & exports::TYPE == 0 {
                        continue;
                    }
                    let name = provider.name.as_str();
                    match &provider.kind {
                        ProviderKind::Declared("module" | "enum") => {
                            let mut child = module.clone();
                            child.push(provider.name.clone());
                            grow(
                                &mut sets,
                                module,
                                name,
                                &BTreeSet::from([child]),
                                &mut budget,
                            );
                        }
                        ProviderKind::ExternCrate(crate_name) if crate_name == "self" => {
                            grow(
                                &mut sets,
                                module,
                                name,
                                &BTreeSet::from([Vec::new()]),
                                &mut budget,
                            );
                        }
                        ProviderKind::Declared(_) | ProviderKind::ExternCrate(_) => {
                            grow(&mut sets, module, name, &BTreeSet::new(), &mut budget);
                            fixed_open.push((module.clone(), provider.name.clone()));
                        }
                        ProviderKind::Use {
                            module: path,
                            item,
                            origin,
                        } => {
                            grow(&mut sets, module, name, &BTreeSet::new(), &mut budget);
                            let mut full = path.clone();
                            if !item.is_empty() {
                                full.push(item.clone());
                            }
                            rules.push(Rule::Use(module, name, full, *origin));
                        }
                    }
                }
                for glob in &scope.exports.globs {
                    rules.push(Rule::Glob(module, &glob.module, glob.origin));
                }
            }
        }
        // 1. 先の module の集合。規則の読んだ (module, 名前) → 規則、glob の先の module → glob の規則。
        let mut readers: HashMap<Key, Vec<usize>> = HashMap::new();
        let mut registered: HashSet<(usize, Key)> = HashSet::new();
        let mut glob_targets: Vec<BTreeSet<Vec<String>>> = vec![BTreeSet::new(); rules.len()];
        let mut by_target: HashMap<Vec<String>, Vec<usize>> = HashMap::new();
        let mut queued: Vec<bool> = vec![true; rules.len()];
        let mut rule_queue: std::collections::VecDeque<usize> = (0..rules.len()).collect();
        let mut key_queue: Vec<Key> = Vec::new();
        loop {
            // 名前の指し得る module が多過ぎれば、求め切れなかったとする (表を作るのをやめる)。
            if budget.over {
                self.table_over.set(true);
                return HashMap::new();
            }
            if let Some(key) = key_queue.pop() {
                for &reader in readers.get(&key).into_iter().flatten() {
                    if !std::mem::replace(&mut queued[reader], true) {
                        rule_queue.push_back(reader);
                    }
                }
                // glob で取り込む module へ、その名前を伝える。
                let (module, name) = &key;
                let modules = sets
                    .get(module)
                    .and_then(|names| names.get(name))
                    .cloned()
                    .unwrap_or_default();
                if !budget.spend(0, modules.len()) {
                    continue;
                }
                for &rule in by_target.get(module).into_iter().flatten() {
                    if let Rule::Glob(into, _, _) = &rules[rule]
                        && grow(&mut sets, into, name, &modules, &mut budget)
                    {
                        key_queue.push(((*into).clone(), name.clone()));
                    }
                }
                continue;
            }
            let Some(rule) = rule_queue.pop_front() else {
                break;
            };
            queued[rule] = false;
            let mut reads: Vec<Key> = Vec::new();
            match &rules[rule] {
                Rule::Use(module, name, path, origin) => {
                    let targets = self.walk_sets(&sets, module, path, *origin, &mut reads);
                    if grow(&mut sets, module, name, &targets, &mut budget) {
                        key_queue.push(((*module).clone(), (*name).to_owned()));
                    }
                }
                Rule::Glob(module, path, origin) => {
                    for target in self.walk_sets(&sets, module, path, *origin, &mut reads) {
                        if target == **module || !glob_targets[rule].insert(target.clone()) {
                            continue;
                        }
                        by_target.entry(target.clone()).or_default().push(rule);
                        let names: Vec<(String, BTreeSet<Vec<String>>)> = sets
                            .get(&target)
                            .map(|names| {
                                names
                                    .iter()
                                    .map(|(name, modules)| (name.clone(), modules.clone()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        if !budget
                            .spend(0, names.iter().map(|(_, modules)| modules.len() + 1).sum())
                        {
                            break;
                        }
                        for (name, modules) in names {
                            if grow(&mut sets, module, &name, &modules, &mut budget) {
                                key_queue.push(((*module).clone(), name));
                            }
                        }
                    }
                }
            }
            for key in reads {
                if registered.insert((rule, key.clone())) {
                    readers.entry(key).or_default().push(rule);
                }
            }
        }
        if budget.over {
            self.table_over.set(true);
            return HashMap::new();
        }
        // 2. 外部 crate・module でない項目かもしれない印 (決まった集合で、読んだ名前に印があるか、無い名前を
        // 読んだ named の取り込み。glob は先の名前の印を持ち込む)。
        let mut open: HashSet<Key> = HashSet::new();
        let mut readers: HashMap<Key, Vec<usize>> = HashMap::new();
        let mut open_queue: Vec<Key> = Vec::new();
        for key in fixed_open {
            if open.insert(key.clone()) {
                open_queue.push(key);
            }
        }
        let use_open = |rule: usize, open: &HashSet<Key>, reads: &mut Vec<Key>| -> Option<Key> {
            let Rule::Use(module, name, path, origin) = &rules[rule] else {
                return None;
            };
            let lookup = |scope: &[String], segment: &str| {
                sets.get(scope)
                    .and_then(|names| names.get(segment))
                    .map(|modules| {
                        (
                            modules,
                            open.contains(&(scope.to_vec(), segment.to_owned())),
                        )
                    })
            };
            let (_, flagged) = self.walk_with(&lookup, module, path, *origin, Some(reads));
            flagged.then(|| ((*module).clone(), (*name).to_owned()))
        };
        for rule in 0..rules.len() {
            let mut reads = Vec::new();
            if let Some(key) = use_open(rule, &open, &mut reads)
                && open.insert(key.clone())
            {
                open_queue.push(key);
            }
            for key in reads {
                readers.entry(key).or_default().push(rule);
            }
        }
        while let Some(key) = open_queue.pop() {
            if !budget.spend(0, 1) {
                self.table_over.set(true);
                return HashMap::new();
            }
            for &rule in readers.get(&key).into_iter().flatten() {
                if let Some(next) = use_open(rule, &open, &mut Vec::new())
                    && open.insert(next.clone())
                {
                    open_queue.push(next);
                }
            }
            let (module, name) = &key;
            for &rule in by_target.get(module).into_iter().flatten() {
                if let Rule::Glob(into, _, _) = &rules[rule] {
                    let next = ((*into).clone(), name.clone());
                    if open.insert(next.clone()) {
                        open_queue.push(next);
                    }
                }
            }
        }
        sets.into_iter()
            .map(|(module, names)| {
                let names = names
                    .into_iter()
                    .map(|(name, modules)| {
                        let flagged = open.contains(&(module.clone(), name.clone()));
                        (name, (modules, flagged))
                    })
                    .collect();
                (module, names)
            })
            .collect()
    }

    /// 先の module の集合 `sets` で、module `module` で書いた経路 `path` (起点 `origin`) が指し得る crate の
    /// module。読んだ (module, 名前) を `reads` に足す。
    fn walk_sets(
        &self,
        sets: &ModuleSets,
        module: &[String],
        path: &[String],
        origin: UsePathOrigin,
        reads: &mut Vec<Key>,
    ) -> BTreeSet<Vec<String>> {
        let lookup = |scope: &[String], segment: &str| {
            sets.get(scope)
                .and_then(|names| names.get(segment))
                .map(|modules| (modules, false))
        };
        self.walk_with(&lookup, module, path, origin, Some(reads)).0
    }

    /// 名前の表 `table` で、module `module` で書いた経路 `path` (起点 `origin`) が指し得る crate の module と、
    /// 外部 crate・module でない項目かもしれないか。
    fn walk(
        &self,
        table: &ModuleNames,
        module: &[String],
        path: &[String],
        origin: UsePathOrigin,
    ) -> (BTreeSet<Vec<String>>, bool) {
        let lookup = |scope: &[String], segment: &str| {
            table
                .get(scope)
                .and_then(|names| names.get(segment))
                .map(|(modules, open)| (modules, *open))
        };
        self.walk_with(&lookup, module, path, origin, None)
    }

    /// module `module` で書いた経路 `path` (起点 `origin`) が指し得る crate の module と、外部 crate・module で
    /// ない項目かもしれないか。名前は `lookup` で引き (先の module と印)、引いた (module, 名前) を `reads` に
    /// 足す。
    fn walk_with<'t>(
        &self,
        lookup: &Lookup<'_, 't>,
        module: &[String],
        path: &[String],
        origin: UsePathOrigin,
        mut reads: Option<&mut Vec<Key>>,
    ) -> (BTreeSet<Vec<String>>, bool) {
        let mut open = false;
        let starts: Vec<Vec<String>> = match origin {
            UsePathOrigin::Anchored => vec![Vec::new()],
            UsePathOrigin::Global => match self.edition {
                Some(RustEdition::E2015) => vec![Vec::new()],
                Some(RustEdition::E2018OrLater) => return (BTreeSet::new(), true),
                None => {
                    open = true;
                    vec![Vec::new()]
                }
            },
            UsePathOrigin::Bare => {
                let scopes = self.bare_scopes(module);
                if path.is_empty() {
                    return (scopes.into_iter().collect(), false);
                }
                scopes
            }
        };
        let mut current: BTreeSet<Vec<String>> = starts.into_iter().collect();
        for segment in path {
            let mut next = BTreeSet::new();
            for scope in &current {
                if let Some(reads) = reads.as_deref_mut() {
                    reads.push((scope.clone(), segment.clone()));
                }
                match lookup(scope, segment) {
                    Some((modules, segment_open)) => {
                        next.extend(modules.iter().cloned());
                        open |= segment_open;
                    }
                    // スコープに無い名前は、外部 crate (先頭) か、解けない経路。
                    None => open = true,
                }
            }
            current = next;
        }
        (current, open)
    }

    /// (module, 名前) を作るもの。
    pub(super) fn entries(&self, key: &Key) -> &[Entry] {
        self.providers.get(key).map_or(&[], Vec::as_slice)
    }

    /// 名前を作るもの `entry` (module `module` の) と、そのスコープ。
    pub(super) fn provider(&self, module: &[String], entry: Entry) -> (&Scope, &Provider) {
        match entry {
            Entry::Provider(scope_index, position) => {
                let scope = &self.modules[module][scope_index];
                (scope, &scope.exports.providers[position])
            }
            Entry::Exported(module_index, scope_index, position) => {
                let (placed, _) = &self.exported_macro_scopes[module_index];
                let scope = &self.modules[placed][scope_index];
                (scope, &scope.exports.exported_macros[position])
            }
        }
    }

    /// module `module` のスコープ。
    pub(super) fn scopes(&self, module: &[String]) -> &[Scope] {
        self.modules.get(module).map_or(&[], Vec::as_slice)
    }

    /// module `module` の glob (スコープの添字, glob の添字, glob)。
    pub(super) fn globs<'s>(
        &'s self,
        module: &[String],
    ) -> impl Iterator<Item = (usize, usize, &'s Glob)> + 's {
        self.scopes(module)
            .iter()
            .enumerate()
            .flat_map(|(scope_index, scope)| {
                scope
                    .exports
                    .globs
                    .iter()
                    .enumerate()
                    .map(move |(position, glob)| (scope_index, position, glob))
            })
    }

    /// module `module` の名前を列挙できない項目 (スコープの添字, 添字, 項目)。
    pub(super) fn opaque<'s>(
        &'s self,
        module: &[String],
    ) -> impl Iterator<Item = (usize, usize, &'s Opaque)> + 's {
        self.scopes(module)
            .iter()
            .enumerate()
            .flat_map(|(scope_index, scope)| {
                scope
                    .exports
                    .opaque
                    .iter()
                    .enumerate()
                    .map(move |(position, opaque)| (scope_index, position, opaque))
            })
    }

    /// `#[macro_export]` のマクロを置いた module ([`Entry::Exported`] の 1 つ目の添字)。
    pub(super) fn exported_scope(&self, module_index: usize) -> &[String] {
        &self.exported_macro_scopes[module_index].0
    }

    /// module `scope` の名前 `name` が、どこかの設定で型の名前空間に指し得る crate の module。
    pub(super) fn segment_modules(&self, scope: &[String], name: &str) -> BTreeSet<Vec<String>> {
        self.module_names
            .get(scope)
            .and_then(|names| names.get(name))
            .map(|(modules, _)| modules.clone())
            .unwrap_or_default()
    }

    /// 取り込む先の名前 `item` を named で取り込む (module, 名前)。
    pub(super) fn importers(&self, item: &str) -> &[Key] {
        self.importers.get(item).map_or(&[], Vec::as_slice)
    }

    /// 経路に名前 `name` を使うもの。
    pub(super) fn path_users(&self, name: &str) -> &[User] {
        self.path_users.get(name).map_or(&[], Vec::as_slice)
    }

    /// module `module` を glob で取り込み得る module。
    pub(super) fn glob_importers(&self, module: &[String]) -> impl Iterator<Item = &Vec<String>> {
        self.glob_importers.get(module).into_iter().flatten()
    }

    /// module `module` で、先頭が識別子の経路を読むスコープ (2015 は crate root、2018 以降は今の module。
    /// edition が分からず crate root の外なら両方)。
    pub(super) fn bare_scopes(&self, module: &[String]) -> Vec<Vec<String>> {
        match self.edition {
            _ if module.is_empty() => vec![Vec::new()],
            Some(RustEdition::E2015) => vec![Vec::new()],
            Some(RustEdition::E2018OrLater) => vec![module.to_vec()],
            None => vec![module.to_vec(), Vec::new()],
        }
    }

    /// module `module` で書いた経路 `path` (起点 `origin`) が、どこかの設定で指し得る crate の module と、
    /// 外部 crate・module でない項目かもしれないか。経路の名前は、子の module・named の取り込み・glob で
    /// 持ち込む名前として順にたどる。
    pub(super) fn modules_of(
        &self,
        module: &[String],
        path: &[String],
        origin: UsePathOrigin,
    ) -> (BTreeSet<Vec<String>>, bool) {
        self.walk(&self.module_names, module, path, origin)
    }

    /// module `module` で経路 `path` (起点 `origin`) をたどるときに引く (module, 名前)。
    pub(super) fn path_keys(
        &self,
        module: &[String],
        path: &[String],
        origin: UsePathOrigin,
    ) -> Vec<Key> {
        let starts: Vec<Vec<String>> = match origin {
            UsePathOrigin::Anchored => vec![Vec::new()],
            UsePathOrigin::Global => match self.edition {
                Some(RustEdition::E2018OrLater) => return Vec::new(),
                _ => vec![Vec::new()],
            },
            UsePathOrigin::Bare => {
                if path.is_empty() {
                    return Vec::new();
                }
                self.bare_scopes(module)
            }
        };
        let mut keys = Vec::new();
        let mut current: BTreeSet<Vec<String>> = starts.into_iter().collect();
        for segment in path {
            let mut next = BTreeSet::new();
            for scope in &current {
                keys.push((scope.clone(), segment.clone()));
                next.extend(self.segment_modules(scope, segment));
            }
            current = next;
        }
        keys
    }

    /// ほかの取り込みが module `module` から引き得る名前 (named の取り込みの先の名前と、経路の途中の名前)。
    /// 置き場の変わった module の `pub` でない名前も、子の module の `use super::x::y;` などが使えば比べる。
    pub(super) fn names_used_from(&self, module: &[String]) -> Option<&BTreeSet<String>> {
        self.used_from
            .get_or_init(|| {
                let mut used: HashMap<Vec<String>, BTreeSet<String>> = HashMap::new();
                for (importer, scopes) in &self.modules {
                    for scope in scopes {
                        for provider in &scope.exports.providers {
                            let ProviderKind::Use {
                                module: path,
                                item,
                                origin,
                            } = &provider.kind
                            else {
                                continue;
                            };
                            for (from, segment) in self.path_keys(importer, path, *origin) {
                                used.entry(from).or_default().insert(segment);
                            }
                            if item.is_empty() {
                                continue;
                            }
                            for from in self.modules_of(importer, path, *origin).0 {
                                used.entry(from).or_default().insert(item.clone());
                            }
                        }
                        for glob in &scope.exports.globs {
                            for (from, segment) in
                                self.path_keys(importer, &glob.module, glob.origin)
                            {
                                used.entry(from).or_default().insert(segment);
                            }
                        }
                    }
                }
                used
            })
            .get(module)
    }

    /// module `target` から glob で module `viewer` に持ち込み得る名前 (どこかの設定で) と、列挙できない
    /// 持ち込み (外部 crate・決められない先の glob、名前を列挙できない項目) があるか。`viewer` から見えない
    /// ものは数えない。
    pub(super) fn glob_names(
        &self,
        target: &[String],
        viewer: &[String],
    ) -> (BTreeSet<String>, Vec<Hole>) {
        let mut names = BTreeSet::new();
        let mut holes: Vec<Hole> = Vec::new();
        let mut visited: HashSet<Vec<String>> = HashSet::new();
        let mut pending = vec![target.to_vec()];
        while let Some(module) = pending.pop() {
            if !visited.insert(module.clone()) {
                continue;
            }
            if self.module_uncertain(&module) {
                holes.push(Hole::Uncertain(module.clone()));
            }
            if module.is_empty() {
                for (placed, scope_index) in &self.exported_macro_scopes {
                    for provider in &self.modules[placed][*scope_index].exports.exported_macros {
                        names.insert(provider.name.clone());
                    }
                }
            }
            for (scope_index, scope) in self.scopes(&module).iter().enumerate() {
                for provider in &scope.exports.providers {
                    if visible(&provider.scope, viewer) {
                        names.insert(provider.name.clone());
                    }
                }
                for position in 0..scope.exports.opaque.len() {
                    holes.push(Hole::Opaque(module.clone(), scope_index, position));
                }
                for (position, glob) in scope.exports.globs.iter().enumerate() {
                    if !visible(&glob.scope, viewer) {
                        continue;
                    }
                    let (targets, open) = self.modules_of(&module, &glob.module, glob.origin);
                    if open {
                        holes.push(Hole::Glob(module.clone(), scope_index, position));
                    }
                    pending.extend(targets);
                }
            }
        }
        (names, holes)
    }

    /// module `module` (か祖先) の場所を決められないか、中身を読めない置き場があるか。
    pub(super) fn module_uncertain(&self, module: &[String]) -> bool {
        (0..=module.len()).any(|depth| {
            let prefix = &module[..depth];
            self.placements.unknown.contains_key(prefix)
                || self
                    .placements
                    .files
                    .get(prefix)
                    .into_iter()
                    .flatten()
                    .any(|placement| placement.index.is_none() && !placement.clauses.is_empty())
        })
    }
}

/// 見える範囲 `scope` (`pub` は None) が module `viewer` を含むか。
pub(super) fn visible(scope: &Option<Vec<String>>, viewer: &[String]) -> bool {
    scope.as_ref().is_none_or(|scope| viewer.starts_with(scope))
}

/// 名前空間のビットの並び。
pub(super) const NAMESPACES: [u8; 3] = [exports::TYPE, exports::VALUE, exports::MACRO];
