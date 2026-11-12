//! Python のテストランナー (pytest と unittest) が既定の規約で集めるテストと、名前で呼ぶ
//! 準備・後始末の判定。
//!
//! pytest は `test_*.py` / `*_test.py` (`python_files` の既定) のモジュールの名前空間から、
//! `test` で始まる名前の関数 (`python_functions`) と、`Test` で始まる名前 (`python_classes`) か
//! `__test__ = True` のクラスを集める。名前は定義の名前ではなく名前空間への束縛の名前なので、
//! 別名 (`TestAlias = Mixin`) や import した `Test*` のクラスも集める。集めたクラスでは、自身と
//! 基底 (`__mro__`) の名前空間から `test` で始まるメソッドと入れ子のクラスを同じ規則で集め、
//! xunit 形式の準備と後始末 (`setup_method` など) と `pytest_generate_tests` を名前で呼ぶ。
//! unittest (と pytest の unittest 対応) は `unittest.TestCase` の派生クラスを集め、自身と基底の
//! `test` で始まるメソッドを集めて、`setUp` などを MRO で呼ぶ。pytest の unittest 対応
//! (`UnitTestCase`) は、テストモジュールの名前空間 (と集めたクラスの本体) に束縛した TestCase の
//! 派生を名前に依らず集め、xunit 形式の準備と後始末 (`setup_method` など 4 つ) も呼ぶ。呼び出し側に
//! 名前が現れないので、参照を数えるだけでは dead と誤報する。
//!
//! 事実はファイルごとに [`PythonModuleFacts`] へ集め、判定は [`collect`] の 1 つの実装で行う。
//! 基底や束縛の名前をクラスへ解決する [`Universe`] だけを差し替える。ファイル単位の判定
//! ([`PythonTestEntrypoints`]) は同じファイルの中だけで解決し、dead-code の段階
//! ([`ProjectTests`]) は import をたどってプロジェクト全体で解決する。
//! 同じ事実と名前の解決を io の ABC の契約メソッドの判定にも使う ([`io_protocol`])。
//!
//! dead と断定しない側に倒す。クラスを集めない (pytest が `__init__` / `__new__` を持つクラスを
//! 飛ばす) と言うのは、`object` 以外の基底・metaclass・デコレータを持たず、本体の直下に
//! デコレータの無い `def __init__` / `def __new__` を書いたクラスだけにする。基底の
//! `__init_subclass__` や metaclass は `__init__` を差し替えうるので、それ以外のクラスは集めると
//! みなす。名前の解決を確かめられない (推測した解決先を通る、第三者のモジュールの基底を持つ、
//! module の `__getattr__` やプロジェクトの関数の呼び出しを通るなど) ものは「集める」と言わず
//! 「不確か」として返す。静的な束縛も `import *` も無い名前は組み込み (か未定義) とみなす。
//! 祖先は和集合でたどり、C3 の順序や上書き (`test_x = None`) は見ない。見るのは既定の規約と静的な
//! 束縛だけで、設定ファイル (`python_classes` など)・実行引数・独自の収集プラグイン・
//! `__test__ = False`・nose 形式の `setup` / `teardown`・実行時の名前空間の書き換え
//! (`globals()` や関数の中の `global`)・文字列で引く属性 (`getattr(m, "Name")`)・識別子の NFKC
//! 正規化は見ない。

mod facts;
mod io_protocol;
mod project;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use crate::models::location::Range;

use facts::Base;
pub(crate) use facts::PythonModuleFacts;
pub(crate) use io_protocol::is_io_protocol_name;
pub(crate) use project::{ProjectModule, ProjectTests};

/// module 直下に書くと pytest が名前で呼ぶ関数 (`setUpModule` は unittest の名前)。
const MODULE_HOOKS: &[&str] = &[
    "setup_module",
    "teardown_module",
    "setUpModule",
    "tearDownModule",
    "setup_function",
    "teardown_function",
    "pytest_generate_tests",
];

/// pytest が集めるクラス (と、その基底) に書くと名前で呼ぶメソッド。
const PYTEST_CLASS_HOOKS: &[&str] = &[
    "setup_method",
    "teardown_method",
    "setup_class",
    "teardown_class",
    "pytest_generate_tests",
];

/// `unittest.TestCase` の派生クラス (と、その基底) に書くと、標準のランナーと `TestCase` の
/// メソッドが暗黙に呼ぶメソッド。`runTest` はテストメソッドが無いときの既定の名前。
/// `addCleanup` / `addClassCleanup` / `addAsyncCleanup` は `enterContext` /
/// `enterClassContext` / `enterAsyncContext` が呼ぶ。
const UNITTEST_CLASS_HOOKS: &[&str] = &[
    "setUp",
    "tearDown",
    "setUpClass",
    "tearDownClass",
    "asyncSetUp",
    "asyncTearDown",
    "runTest",
    "doCleanups",
    "doClassCleanups",
    "addCleanup",
    "addClassCleanup",
    "addAsyncCleanup",
];

/// pytest の unittest 対応 (`UnitTestCase`) が、`TestCase` の派生クラス (と、その基底) に書くと
/// unittest の準備・後始末に加えて名前で呼ぶ xunit 形式のメソッド。`pytest_generate_tests` は
/// 呼ばない (`UnitTestCase` は通常の pytest のクラスの収集を通らない)。
const PYTEST_UNITTEST_HOOKS: &[&str] = &[
    "setup_method",
    "teardown_method",
    "setup_class",
    "teardown_class",
];

/// 名前がプロジェクトのクラスに解決できないときに、unittest の TestCase とみなす基底の書き方
/// (`from django.test import TestCase` のような第三者の TestCase の派生を含む)。
const UNITTEST_ROOT_BASES: &[&str] = &[
    "TestCase",
    "unittest.TestCase",
    "IsolatedAsyncioTestCase",
    "unittest.IsolatedAsyncioTestCase",
];

/// `unittest` (とその下のモジュール) から import すると TestCase を指す名前。
const UNITTEST_ROOT_NAMES: &[&str] = &["TestCase", "IsolatedAsyncioTestCase"];

/// テストランナー。集めたクラスと基底のどのメンバーを名前で呼ぶかが違う。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Runner {
    Pytest,
    Unittest,
    /// pytest が、テストモジュールの名前空間 (と集めたクラスの本体) に束縛された `TestCase` の
    /// 派生を、名前と印に依らず `UnitTestCase` として集めたもの。unittest の規則で歩き、xunit 形式の
    /// 準備・後始末も呼ぶ。入れ子のクラスや別名は調べない (`UnitTestCase` はメソッドの名前だけを
    /// 見る)。
    PytestUnittest,
}

impl Runner {
    /// 集めたクラスか、その基底にある `name` のメソッドを、テストとして集めるか名前で呼ぶか。
    fn calls(self, name: &str) -> bool {
        name.starts_with("test")
            || match self {
                Self::Pytest => PYTEST_CLASS_HOOKS.contains(&name),
                Self::Unittest => UNITTEST_CLASS_HOOKS.contains(&name),
                Self::PytestUnittest => {
                    UNITTEST_CLASS_HOOKS.contains(&name) || PYTEST_UNITTEST_HOOKS.contains(&name)
                }
            }
    }
}

/// どちらかのテストランナーが、集めたクラスか基底の `name` のメソッドを集めるか名前で呼ぶか。
pub(crate) fn is_runner_member_name(name: &str) -> bool {
    Runner::Pytest.calls(name) || Runner::Unittest.calls(name)
}

/// どちらかのテストランナーが、`name` の関数 (module 直下) かメソッドを集めるか名前で呼びうるか。
pub(crate) fn is_runner_name(name: &str) -> bool {
    is_runner_member_name(name) || MODULE_HOOKS.contains(&name)
}

/// 1 ファイルの中だけで、テストランナーが集めるか名前で呼ぶと言える定義。
#[derive(Debug, Default)]
pub(crate) struct PythonTestEntrypoints {
    /// 定義ノード (`class_definition` / `function_definition` / lambda を束縛する `assignment`) の
    /// 開始位置 (行, 列)。Python のシンボルの range は定義ノード全体なので、その開始位置で引く。
    starts: HashSet<(usize, usize)>,
}

impl PythonTestEntrypoints {
    /// `pytest_module` は、ファイル名が pytest のテストモジュール (`test_*.py` など) か。
    /// 同じファイルで解決できない基底や import の先は見ない (プロジェクト単位の判定に任せる)。
    pub(crate) fn collect(facts: &PythonModuleFacts, pytest_module: bool) -> Self {
        let collected = collect(&SingleModule {
            facts,
            pytest_module,
        });
        Self {
            starts: collected.live.into_iter().map(|(_, start)| start).collect(),
        }
    }

    /// `range` で始まる定義を、テストランナーが集めるか名前で呼ぶか。
    pub(crate) fn contains(&self, range: &Range) -> bool {
        self.starts
            .contains(&(range.start.line, range.start.column))
    }
}

/// (モジュールの添字, そのモジュールのクラスの添字)。
type ClassId = (usize, usize);

/// 基底の式や束縛の名前が指しうるもの。
#[derive(Clone, Debug, Default)]
struct Resolution {
    /// 到達できたクラスと、その経路を確かめられたか (推測した解決先を通っていないか)。
    classes: Vec<(ClassId, bool)>,
    /// 確かめられた経路で unittest の TestCase に届く。
    root: bool,
    /// 推測した経路でだけ unittest の TestCase に届きうる。
    guessed_root: bool,
    /// 標準ライブラリの io の ABC が持つ契約 (ビット集合)。
    io_bases: u8,
    /// 静的に解決しきれず、プロジェクトの外のもの (第三者のモジュールのクラスなど) も指しうる。
    incomplete: bool,
    /// プロジェクトのどのクラスも指しうる (module の `__getattr__`、読めないモジュール、
    /// プロジェクトの関数の呼び出しなど)。
    wildcard: bool,
    /// クラスでないプロジェクトの値 (関数・プロジェクトのクラスのインスタンス・入れ物) も
    /// 指しうる。その属性・要素・呼び出しの結果は静的に分からない。
    object: bool,
    /// モジュールとしても指しうるものの (import したモジュール, 経路)。属性はそのモジュールで
    /// 引く (`import helpers` や `ns = helpers` の `ns.X`)。
    modules: Vec<(usize, facts::ModuleRef)>,
    /// `unittest` (とその下) のモジュールも指しうる (その `TestCase` 属性は root)。
    unittest_module: bool,
    /// 組み込みの `type` も指しうる (継承したクラスは metaclass で、呼び出すとクラスを作る)。
    builtin_type: bool,
}

impl Resolution {
    /// `other` の結果を足す。
    fn absorb(&mut self, other: &Self) {
        self.classes.extend(other.classes.iter().copied());
        self.modules.extend(other.modules.iter().cloned());
        self.root |= other.root;
        self.guessed_root |= other.guessed_root;
        self.io_bases |= other.io_bases;
        self.incomplete |= other.incomplete;
        self.wildcard |= other.wildcard;
        self.object |= other.object;
        self.unittest_module |= other.unittest_module;
        self.builtin_type |= other.builtin_type;
    }
}

/// 名前の束縛をたどる深さの上限 (別名や import の長い連鎖でスタックを使い切らないため)。
/// 超えたら、どのクラスでもありうるとして止める。
const MAX_BINDING_DEPTH: usize = 64;

/// 判定に使うモジュールの集まりと、名前の解決。
trait Universe {
    fn module_count(&self) -> usize;
    fn facts(&self, module: usize) -> &PythonModuleFacts;
    /// ファイル名が pytest のテストモジュールか。
    fn is_pytest_module(&self, module: usize) -> bool;
    /// ファイル名が、テストランナーがテストを探すモジュールの名前か (pytest の既定と、
    /// unittest の既定の探索 `test*.py`)。
    fn is_test_module(&self, module: usize) -> bool;
    /// `module` の名前空間 `scope` (`None` は module、`Some(c)` はクラス `c` の本体) で書いた
    /// 基底や別名の右辺が指しうるもの。
    fn resolve(&self, module: usize, scope: Option<usize>, base: &Base) -> Resolution;
    /// pytest のテストモジュール `module` の名前空間へ import で入る (束縛の名前, 指しうるもの)。
    fn imported_bindings(&self, module: usize) -> Vec<(String, Resolution)>;
    /// 中身を見られないモジュールがあり、(どのクラスを継承するテストクラスも、どのクラスを
    /// 束縛するテストモジュールも) ありうるか。
    fn unobserved(&self) -> (bool, bool);
}

/// 判定の結果。定義は (モジュールの添字, 定義の開始位置) で持つ。
#[derive(Debug, Default)]
pub(crate) struct Collected {
    /// テストランナーが集めるか名前で呼ぶ定義。
    pub(crate) live: HashSet<(usize, (usize, usize))>,
    /// 集めるかどうか分からない定義 (`live` を除く)。
    pub(crate) uncertain: HashSet<(usize, (usize, usize))>,
    /// どのクラスの、テストランナーが名前で呼ぶメンバーも集めるかどうか分からない
    /// (テストの入口から見えない基底をたどった、中身を見られないモジュールがあるなど)。
    pub(crate) any_member_uncertain: bool,
    /// どのクラスも、集めるかどうか分からない。
    pub(crate) any_class_uncertain: bool,
}

/// クラスごとの基底の解決。
struct ClassBases {
    /// (基底の候補, 確かめられた経路か)。
    edges: Vec<(usize, bool)>,
    root: bool,
    guessed_root: bool,
    incomplete: bool,
    wildcard: bool,
}

/// 入口から名前空間を歩くときの確かさ。順序は強さ (弱い順)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Certainty {
    /// 入口が分からない (TestCase の派生か、`__test__ = True` を持つか分からない) か、推測した
    /// 解決先を通る。
    Uncertain,
    /// 確かめられた経路の入口。
    Certain,
}

impl Certainty {
    /// 辺を 1 つたどった後の確かさ (確かめられない辺なら、確かではなくなる)。
    fn through(self, confirmed: bool) -> Self {
        if confirmed { self } else { Self::Uncertain }
    }
}

/// `flags` を基底から派生へ伝える (固定点)。`follow` が真の辺だけをたどり、`allowed` が偽の
/// クラスには伝えない。
fn propagate(
    bases: &[ClassBases],
    mut flags: Vec<bool>,
    follow: impl Fn(bool) -> bool,
    allowed: impl Fn(usize) -> bool,
) -> Vec<bool> {
    loop {
        let mut changed = false;
        for (index, class) in bases.iter().enumerate() {
            if !flags[index]
                && allowed(index)
                && class
                    .edges
                    .iter()
                    .any(|&(base, confirmed)| follow(confirmed) && flags[base])
            {
                flags[index] = true;
                changed = true;
            }
        }
        if !changed {
            return flags;
        }
    }
}

/// テストランナーが集めるか名前で呼ぶ定義と、集めるかどうか分からない定義を求める。
fn collect(universe: &impl Universe) -> Collected {
    let module_count = universe.module_count();
    // クラスに通し番号を振る。
    let mut ids: Vec<ClassId> = Vec::new();
    let mut offsets: Vec<usize> = Vec::with_capacity(module_count);
    for module in 0..module_count {
        offsets.push(ids.len());
        ids.extend((0..universe.facts(module).classes.len()).map(|class| (module, class)));
    }
    let global = |(module, class): ClassId| offsets[module] + class;
    let class_facts = |index: usize| {
        let (module, class) = ids[index];
        &universe.facts(module).classes[class]
    };

    // 各クラスの基底を、クラス文を書いた名前空間 (所属するクラスの本体と module) で解決する。
    let bases: Vec<ClassBases> = ids
        .iter()
        .map(|&(module, class)| {
            let facts = universe.facts(module);
            let class = &facts.classes[class];
            let mut bases = ClassBases {
                edges: Vec::new(),
                root: false,
                guessed_root: false,
                incomplete: false,
                wildcard: false,
            };
            for base in &class.bases {
                let resolution = universe.resolve(module, class.owner, base);
                bases.edges.extend(
                    resolution
                        .classes
                        .iter()
                        .map(|&(id, confirmed)| (global(id), confirmed)),
                );
                bases.root |= resolution.root;
                bases.guessed_root |= resolution.guessed_root;
                bases.incomplete |= resolution.incomplete;
                // クラスでない値を基底に書けるのは `__mro_entries__` を持つときで、それは
                // 何でも基底にしうる。
                bases.wildcard |= resolution.wildcard || resolution.object;
            }
            // 構文エラーのあるファイルのクラスは、基底を取りこぼしうる。
            bases.incomplete |= facts.has_error;
            bases.edges.sort_unstable();
            bases.edges.dedup();
            bases
        })
        .collect();
    let count = ids.len();
    // TestCase の派生 (確かめられた経路で root に届く)。
    let testcase = propagate(
        &bases,
        bases.iter().map(|class| class.root).collect(),
        |confirmed| confirmed,
        |_| true,
    );
    // TestCase の派生か分からない (確かめられない経路や、解決しきれない基底がある)。
    let unknown = propagate(
        &bases,
        (0..count)
            .map(|index| {
                let class = &bases[index];
                !testcase[index]
                    && (class.guessed_root
                        || class.incomplete
                        || class.wildcard
                        || class
                            .edges
                            .iter()
                            .any(|&(base, confirmed)| !confirmed && testcase[base]))
            })
            .collect(),
        |_| true,
        |index| !testcase[index],
    );
    // `__test__ = True` (継承を含む) と、それが分からないもの。
    let markers = propagate(
        &bases,
        (0..count)
            .map(|index| class_facts(index).test_marker)
            .collect(),
        |confirmed| confirmed,
        |_| true,
    );
    let unknown_markers = propagate(
        &bases,
        (0..count)
            .map(|index| {
                let class = &bases[index];
                !markers[index]
                    && (class.incomplete
                        || class.wildcard
                        || class
                            .edges
                            .iter()
                            .any(|&(base, confirmed)| !confirmed && markers[base]))
            })
            .collect(),
        |_| true,
        |index| !markers[index],
    );
    // 基底を `TestCase` などと書いた (か、そう書いたクラスを継承する) クラス。TestCase の派生か
    // 分からなくても、テストを継承しうる。
    let written_testcase = propagate(
        &bases,
        (0..count)
            .map(|index| {
                class_facts(index)
                    .bases
                    .iter()
                    .any(Base::is_unittest_root_text)
            })
            .collect(),
        |_| true,
        |_| true,
    );
    let mut walker = Walker {
        universe,
        ids: &ids,
        global: &global,
        bases: &bases,
        markers: &markers,
        unknown_markers: &unknown_markers,
        testcase: &testcase,
        unknown: &unknown,
        collected: Collected::default(),
        walked: HashMap::new(),
        examined: HashMap::new(),
        to_walk: Vec::new(),
        to_examine: Vec::new(),
        aliased: Vec::new(),
        markers_held: false,
        tests_held: false,
        testcases_held: false,
    };
    let (members, classes) = universe.unobserved();
    walker.collected.any_member_uncertain |= members;
    walker.collected.any_class_uncertain |= classes;

    for index in 0..count {
        if testcase[index] {
            walker.define(index, Certainty::Certain);
            walker
                .to_walk
                .push((index, Runner::Unittest, Certainty::Certain));
        } else if unknown[index] {
            // TestCase の派生か分からないクラスの定義は、テストを持ちうるとき (テストを探す
            // モジュールにあるか、自身か基底にテストの名前の束縛がある) か、基底を `TestCase`
            // などと書いたとき (見えない基底からテストを継承しうる) だけ不確かにする (第三者の
            // 基底を持つ普通のクラスまで判定を控えない)。
            if walker.may_hold_tests(index) || written_testcase[index] {
                walker.define(index, Certainty::Uncertain);
            }
            walker
                .to_walk
                .push((index, Runner::Unittest, Certainty::Uncertain));
        }
    }
    for module in 0..module_count {
        if !universe.is_pytest_module(module) {
            continue;
        }
        let facts = universe.facts(module);
        walker
            .collected
            .live
            .extend(facts.functions.iter().map(|&start| (module, start)));
        for (class, facts) in facts.classes.iter().enumerate() {
            if facts.owner.is_none() {
                walker
                    .to_examine
                    .push((global((module, class)), Certainty::Certain));
            }
        }
        let bindings: Vec<(String, Resolution)> = facts
            .aliases
            .iter()
            .map(|(name, target)| (name.clone(), universe.resolve(module, None, target)))
            .chain(universe.imported_bindings(module))
            .collect();
        for (name, resolution) in bindings {
            walker.bind(&name, &resolution, Certainty::Certain);
        }
    }
    walker.run();

    let mut collected = walker.collected;
    let Collected {
        live, uncertain, ..
    } = &mut collected;
    uncertain.retain(|definition| !live.contains(definition));
    collected
}

/// 入口から名前空間を歩く途中の状態。
struct Walker<'w, U: Universe, G: Fn(ClassId) -> usize> {
    universe: &'w U,
    ids: &'w [ClassId],
    global: &'w G,
    bases: &'w [ClassBases],
    markers: &'w [bool],
    unknown_markers: &'w [bool],
    /// TestCase の派生 (確かめられた経路で root に届く)。
    testcase: &'w [bool],
    /// TestCase の派生か分からない。
    unknown: &'w [bool],
    collected: Collected,
    /// (クラス, テストランナー) → 歩いたときの一番強い確かさ。
    walked: HashMap<(usize, Runner), Certainty>,
    /// クラス → 調べたときの一番強い確かさ。
    examined: HashMap<usize, Certainty>,
    /// 名前空間を歩くクラス (集めたクラスと、その基底の候補)。
    to_walk: Vec<(usize, Runner, Certainty)>,
    /// pytest が名前と印で集めるか調べるクラス (module 直下と、歩いたクラスの入れ子)。
    to_examine: Vec<(usize, Certainty)>,
    /// pytest が `Test*` の名前 (か印) で束縛された名前空間から集めるクラス。
    aliased: Vec<(usize, Certainty)>,
    /// `__test__ = True` のクラスを、どれも集めうるとした。
    markers_held: bool,
    /// テストを持ちうるクラスを、どれも集めうるとした。
    tests_held: bool,
    /// TestCase の派生 (か、そうかもしれないクラス) を、どれも `UnitTestCase` として集めうるとした。
    testcases_held: bool,
}

impl<'w, U: Universe, G: Fn(ClassId) -> usize> Walker<'w, U, G> {
    fn class(&self, index: usize) -> (usize, &'w facts::ClassFacts) {
        let universe: &'w U = self.universe;
        let (module, class) = self.ids[index];
        (module, &universe.facts(module).classes[class])
    }

    /// 定義を、集める (確か) か、集めるかどうか分からないものとして記録する。
    fn define(&mut self, index: usize, certainty: Certainty) {
        let (module, class) = self.class(index);
        self.record((module, class.start), certainty);
    }

    fn record(&mut self, definition: (usize, (usize, usize)), certainty: Certainty) {
        if certainty == Certainty::Certain {
            self.collected.live.insert(definition);
        } else {
            self.collected.uncertain.insert(definition);
        }
    }

    /// 集めるかどうか分からないクラスの定義を、不確かにするか。テストを探すモジュールにあるか、
    /// 自身か (どの経路でも) 基底の名前空間に `test*` / `Test*` の束縛 (メソッド・別名・import・
    /// 入れ子のクラス) があるときだけにする (第三者の基底を持つ普通のクラスまで判定を控えない)。
    fn may_hold_tests(&self, index: usize) -> bool {
        let (module, _) = self.ids[index];
        if self.universe.is_test_module(module) {
            return true;
        }
        let mut pending = vec![index];
        let mut seen: HashSet<usize> = HashSet::new();
        while let Some(current) = pending.pop() {
            if !seen.insert(current) {
                continue;
            }
            let (module, class) = self.ids[current];
            if self.universe.facts(module).binds_test_names(class) {
                return true;
            }
            pending.extend(self.bases[current].edges.iter().map(|&(base, _)| base));
        }
        false
    }

    /// pytest が集める名前空間 (テストモジュールか、集めたクラスの本体) で `name` に束縛された
    /// ものを、集めるか調べる。`name` が空なら、中身を見られない `import *` で入る名前。
    fn bind(&mut self, name: &str, resolution: &Resolution, certainty: Certainty) {
        if resolution.wildcard {
            // どのクラスが束縛されるか分からない。
            if name.is_empty() {
                // 中身を見られない `import *` は、名前を書かずにどのクラスでも入れうる。
                self.collected.any_class_uncertain = true;
                self.collected.any_member_uncertain = true;
            } else if name.starts_with("Test") {
                // どのクラスも束縛されうる (デコレータ・metaclass・`__init_subclass__`・
                // `__set_name__`・`__subclasses__()` などで、名前を書かずにクラスを取り出せる)。
                // メンバーはどれも、クラスの定義はテストを持ちうるものを集めうるとする。
                self.collected.any_member_uncertain = true;
                self.hold_tests();
            } else {
                // `Test*` でない名前なら、`__test__ = True` と分かっているクラスと、名前に依らず
                // 集める TestCase の派生 (か、そうかもしれないクラス) だけを集めうる。
                self.hold_markers();
                self.hold_testcases();
            }
        }
        for &(id, confirmed) in &resolution.classes {
            let index = (self.global)(id);
            if self.testcase[index] {
                // TestCase の派生は、名前と印に依らず `UnitTestCase` として集める。
                self.collect_unittest(index, certainty.through(confirmed));
                continue;
            }
            if name.starts_with("Test") || self.markers[index] {
                self.aliased.push((index, certainty.through(confirmed)));
            } else if self.unknown_markers[index] {
                self.aliased.push((index, Certainty::Uncertain));
            }
            if self.unknown[index] {
                self.collect_unittest(index, Certainty::Uncertain);
            }
        }
    }

    /// pytest が `UnitTestCase` として集めるクラス (名前空間に束縛された TestCase の派生) の定義を
    /// 記録し、unittest の規則と xunit 形式の準備・後始末で歩く。`UnitTestCase` は通常の pytest の
    /// クラスの `__init__` / `__new__` による除外を通らない。
    fn collect_unittest(&mut self, index: usize, certainty: Certainty) {
        self.define(index, certainty);
        self.to_walk
            .push((index, Runner::PytestUnittest, certainty));
    }

    /// TestCase の派生 (か、そうかもしれないクラス) を、どれも `UnitTestCase` として集めうるとする。
    fn hold_testcases(&mut self) {
        if std::mem::replace(&mut self.testcases_held, true) {
            return;
        }
        for index in 0..self.ids.len() {
            if self.testcase[index] || self.unknown[index] {
                self.collect_unittest(index, Certainty::Uncertain);
            }
        }
    }

    /// `__test__ = True` と分かっているクラスを、どれも集めうるとする。
    fn hold_markers(&mut self) {
        if std::mem::replace(&mut self.markers_held, true) {
            return;
        }
        for index in 0..self.ids.len() {
            if self.markers[index] {
                self.aliased.push((index, Certainty::Uncertain));
            }
        }
    }

    /// テストを持ちうるクラスを、どれも集めうるとする。
    fn hold_tests(&mut self) {
        if std::mem::replace(&mut self.tests_held, true) {
            return;
        }
        for index in 0..self.ids.len() {
            if self.may_hold_tests(index) {
                self.aliased.push((index, Certainty::Uncertain));
            }
        }
    }

    fn run(&mut self) {
        loop {
            if let Some((index, runner, certainty)) = self.to_walk.pop() {
                if self
                    .walked
                    .get(&(index, runner))
                    .is_some_and(|&walked| walked >= certainty)
                {
                    continue;
                }
                self.walked.insert((index, runner), certainty);
                self.walk(index, runner, certainty);
            } else if let Some((index, certainty)) = self.aliased.pop() {
                let (module, class) = self.ids[index];
                if !self.universe.facts(module).skipped_by_constructor(class) {
                    self.define(index, certainty);
                    self.to_walk.push((index, Runner::Pytest, certainty));
                }
            } else if let Some((index, certainty)) = self.to_examine.pop() {
                if self
                    .examined
                    .get(&index)
                    .is_some_and(|&examined| examined >= certainty)
                {
                    continue;
                }
                self.examined.insert(index, certainty);
                if self.testcase[index] {
                    // TestCase の派生は、名前と印に依らず `UnitTestCase` として集める。
                    self.collect_unittest(index, certainty);
                    continue;
                }
                if self.unknown[index] {
                    self.collect_unittest(index, Certainty::Uncertain);
                }
                let (module, class) = self.ids[index];
                let facts = self.universe.facts(module);
                if facts.skipped_by_constructor(class) {
                    continue;
                }
                if facts.classes[class].name.starts_with("Test") || self.markers[index] {
                    self.define(index, certainty);
                    self.to_walk.push((index, Runner::Pytest, certainty));
                } else if self.unknown_markers[index] {
                    self.define(index, Certainty::Uncertain);
                    self.to_walk
                        .push((index, Runner::Pytest, Certainty::Uncertain));
                }
            } else {
                return;
            }
        }
    }

    fn walk(&mut self, index: usize, runner: Runner, certainty: Certainty) {
        let (module, class) = self.class(index);
        let (_, class_index) = self.ids[index];
        for (name, start) in &class.members {
            if runner.calls(name) {
                self.record((module, *start), certainty);
            }
        }
        // 見えない基底 (プロジェクトの関数の呼び出し、module の `__getattr__`、読めない
        // モジュールなど) は、プロジェクトのどのクラスでもありうる。
        if self.bases[index].wildcard {
            self.collected.any_member_uncertain = true;
        }
        // unittest は入れ子のクラスや名前空間の別名を集めない (`TestLoader` はメソッドの名前だけを
        // 見る)。pytest は名前空間の束縛 (入れ子・別名・import) を同じ規則で調べる。
        if runner == Runner::Pytest {
            let nested: Vec<usize> = class
                .nested
                .iter()
                .map(|&nested| (self.global)((module, nested)))
                .collect();
            self.to_examine
                .extend(nested.into_iter().map(|nested| (nested, certainty)));
            let mut bindings: Vec<(String, Resolution)> = class
                .aliases
                .iter()
                .map(|(name, target)| {
                    (
                        name.clone(),
                        self.universe.resolve(module, Some(class_index), target),
                    )
                })
                .collect();
            let mut imported: Vec<&str> = class
                .imports
                .iter()
                .filter(|(_, import)| matches!(import, facts::Import::From { .. }))
                .map(|(name, _)| name.as_str())
                .collect();
            imported.sort_unstable();
            imported.dedup();
            for name in imported {
                let resolution =
                    self.universe
                        .resolve(module, Some(class_index), &Base::Name(name.to_string()));
                bindings.push((name.to_string(), resolution));
            }
            for (name, resolution) in bindings {
                self.bind(&name, &resolution, certainty);
            }
        }
        for &(base, confirmed) in &self.bases[index].edges {
            self.to_walk
                .push((base, runner, certainty.through(confirmed)));
        }
    }
}

/// `scope` に書いた式の名前を引く名前空間の並び。クラスの本体ならその本体と module で、
/// 間にある外側のクラスの本体は見えない (Python の名前解決と同じ)。
fn lookup_scopes(scope: Option<usize>) -> Vec<Option<usize>> {
    match scope {
        Some(class) => vec![Some(class), None],
        None => vec![None],
    }
}

/// ファイル単位の判定: 1 つのモジュールの中だけで名前を引く。import の先、`import *` で入る
/// 名前、書き方だけで TestCase とみなす規則 (`from x import TestCase` の先はプロジェクトの
/// クラスかもしれない)、呼び出しの結果は、ファイル単位では確かめられないので使わない (不完全と
/// し、プロジェクト単位の判定に任せる)。
struct SingleModule<'f> {
    facts: &'f PythonModuleFacts,
    pytest_module: bool,
}

/// [`SingleModule`] の 1 回の解決の途中の状態。
#[derive(Default)]
struct LocalWalk {
    /// (名前空間, 名前) → 引いた結果 (引いている途中なら `None`。循環はそこで止める)。
    memo: HashMap<(Option<usize>, String), Option<Resolution>>,
    /// たどっている途中の属性 (循環で止める)。
    attributes: HashSet<(Option<usize>, Vec<String>)>,
    /// 名前の束縛をたどる深さ ([`MAX_BINDING_DEPTH`] を超えたら止める)。
    depth: usize,
}

impl SingleModule<'_> {
    /// `scopes` の名前空間の `name` の束縛をたどる。`unittest` から import した TestCase は
    /// root とする。
    fn find(
        &self,
        scopes: &[Option<usize>],
        name: &str,
        resolution: &mut Resolution,
        walk: &mut LocalWalk,
    ) {
        for &scope in scopes {
            let key = (scope, name.to_string());
            match walk.memo.get(&key) {
                Some(Some(found)) => {
                    let found = found.clone();
                    resolution.absorb(&found);
                    continue;
                }
                Some(None) => continue,
                None => {}
            }
            if walk.depth >= MAX_BINDING_DEPTH {
                resolution.incomplete = true;
                continue;
            }
            walk.memo.insert(key.clone(), None);
            walk.depth += 1;
            let mut found = Resolution::default();
            let bindings = self.facts.bindings(scope, name);
            found
                .classes
                .extend(bindings.classes.iter().map(|&class| ((0, class), true)));
            found.object |= bindings.function;
            for alias in bindings.aliases {
                self.resolve_into(scope, alias, &mut found, walk);
            }
            for import in bindings.imports {
                if import.is_unittest_root() {
                    found.root = true;
                } else {
                    found.incomplete = true;
                }
            }
            if scope.is_none() && !self.facts.star_imports.is_empty() {
                found.incomplete = true;
            }
            walk.depth -= 1;
            resolution.absorb(&found);
            walk.memo.insert(key, Some(found));
        }
    }

    fn resolve_into(
        &self,
        scope: Option<usize>,
        base: &Base,
        resolution: &mut Resolution,
        walk: &mut LocalWalk,
    ) {
        let chain = lookup_scopes(scope);
        match base {
            Base::Name(name) => self.find(&chain, name, resolution, walk),
            Base::Qualified(path) => {
                // 属性の循環 (`Box.Base` の先がまた `Box.Base`) は、ここで止める。
                if !walk.attributes.insert((scope, path.clone())) {
                    return;
                }
                let tail = &path[path.len() - 1];
                // 先頭が `unittest` のモジュールなら TestCase。
                let unittest_module = chain.iter().any(|&scope| {
                    self.facts
                        .bindings(scope, &path[0])
                        .imports
                        .iter()
                        .any(facts::Import::is_unittest_module)
                });
                if unittest_module && UNITTEST_ROOT_NAMES.contains(&tail.as_str()) {
                    resolution.root = true;
                } else {
                    // 先頭が同じファイルのクラスなら、属性をその本体の束縛でたどる (引いた名前の
                    // 結果は共有するので、別の経路で引いた先頭も落とさない)。クラスでない値の
                    // 属性は分からない。
                    let mut head = Resolution::default();
                    self.find(&chain, &path[0], &mut head, walk);
                    resolution.incomplete |= head.incomplete || head.object || head.wildcard;
                    let mut current: Vec<usize> =
                        head.classes.iter().map(|&((_, class), _)| class).collect();
                    for segment in &path[1..] {
                        let mut next = Resolution::default();
                        for &class in &current {
                            self.find(&[Some(class)], segment, &mut next, walk);
                        }
                        resolution.incomplete |= next.incomplete || next.wildcard || next.object;
                        resolution.root |= next.root;
                        current = next.classes.iter().map(|&((_, class), _)| class).collect();
                    }
                    if current.is_empty() {
                        resolution.incomplete = true;
                    }
                    resolution
                        .classes
                        .extend(current.into_iter().map(|class| ((0, class), true)));
                }
                walk.attributes.remove(&(scope, path.clone()));
            }
            Base::Subscript(value) => {
                // 標準の generic と言えるのは、基底も metaclass も独自の `__class_getitem__` も
                // 持たないクラスだけにする (継承した `__class_getitem__` や metaclass の
                // `__getitem__` はファイル単位では確かめきれない)。入れ物の要素は分からない。
                let mut found = Resolution::default();
                self.resolve_into(scope, value, &mut found, walk);
                resolution.root |= found.root;
                resolution.incomplete |= found.incomplete || found.object || found.wildcard;
                for &((_, class), confirmed) in &found.classes {
                    let facts = &self.facts.classes[class];
                    if facts.bases.is_empty() && !facts.keyword_arguments && !facts.class_getitem {
                        resolution.classes.push(((0, class), confirmed));
                    } else {
                        resolution.incomplete = true;
                    }
                }
            }
            Base::Union(alternatives) => {
                for alternative in alternatives {
                    self.resolve_into(scope, alternative, resolution, walk);
                }
            }
            Base::Container { .. } | Base::Mapping { .. } => {
                resolution.object |= !base.is_scalar();
            }
            Base::Function => resolution.object = true,
            Base::NotClass => {}
            Base::Call { .. }
            | Base::Item(_)
            | Base::Values(_)
            | Base::Operator(_)
            | Base::Other => resolution.incomplete = true,
        }
    }
}

impl Universe for SingleModule<'_> {
    fn module_count(&self) -> usize {
        1
    }

    fn facts(&self, _module: usize) -> &PythonModuleFacts {
        self.facts
    }

    fn is_pytest_module(&self, _module: usize) -> bool {
        self.pytest_module
    }

    fn is_test_module(&self, _module: usize) -> bool {
        self.pytest_module
    }

    fn resolve(&self, _module: usize, scope: Option<usize>, base: &Base) -> Resolution {
        let mut resolution = Resolution::default();
        self.resolve_into(scope, base, &mut resolution, &mut LocalWalk::default());
        resolution
    }

    fn imported_bindings(&self, _module: usize) -> Vec<(String, Resolution)> {
        Vec::new()
    }

    fn unobserved(&self) -> (bool, bool) {
        (false, false)
    }
}
