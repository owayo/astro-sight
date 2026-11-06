//! プロジェクトの Python のモジュールをまたいで、基底と束縛の名前を解決する。
//!
//! 相対 import は importer のディレクトリから、絶対 import は確かめられる root (importer の
//! basedir = pytest が既定で `sys.path` の先頭に入れる `__init__.py` の無い最初の祖先、
//! `dir`、`dir/src`) から引く。そこで見つからず、経路の末尾だけがモジュール名に一致する
//! ファイルは推測の解決先とし、実際の import 先はプロジェクトの外かもしれないので不完全にする。
//! 同じ名前の束縛は上書きせず和集合にする。静的な束縛も `import *` も無い名前は組み込み
//! (か未定義) とみなす。import の先で束縛を見つけられない名前 (第三者のモジュールなど) は、
//! プロジェクトのすべてのモジュールで同じ名前を引いた結果 (推測) を足す。module の
//! `__getattr__`・中身を見られないモジュール・プロジェクトの関数やインスタンスの呼び出し・
//! プロジェクトの入れ物やインスタンスの属性と要素は、プロジェクトのどのクラスも指しうるとする。
//! クラスやプロジェクトの外のものの呼び出しは、引数のクラスを基底にしたクラスやプロジェクトの
//! 外のものを返しうる (推測)。

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};

use super::facts::{ClassFacts, Import, ModuleRef, OwnBinding};
use super::{
    Base, ClassId, Collected, MAX_BINDING_DEPTH, PythonModuleFacts, Resolution,
    UNITTEST_ROOT_NAMES, Universe, lookup_scopes,
};

/// 判定に使う 1 モジュール。
pub(crate) struct ProjectModule {
    /// `dir` 相対のパス (`/` 区切り)。
    pub(crate) path: String,
    pub(crate) facts: PythonModuleFacts,
    /// ファイル名が pytest のテストモジュールか。
    pub(crate) pytest_module: bool,
}

/// プロジェクトの Python のモジュールの集まり。
pub(crate) struct ProjectTests {
    modules: Vec<ProjectModule>,
    /// 各モジュールの basedir の接頭辞 (`tests/`、`dir` 直下は空)。
    basedirs: Vec<String>,
    /// `dir` 相対のパス → モジュール。
    by_path: HashMap<String, usize>,
    /// モジュールの経路の末尾 (`pkg.helpers` / `helpers`) → そのモジュール (.py と `__init__.py`)。
    by_suffix: HashMap<String, Vec<usize>>,
    /// 名前 → その名前の束縛 (クラス・別名・import・関数) をどこかの名前空間に持つモジュール。
    by_name: HashMap<String, Vec<usize>>,
    /// 中身を見られなかったモジュールのパスと、経路の末尾。
    unobserved_paths: HashSet<String>,
    unobserved_suffixes: HashSet<String>,
    /// 中身を見られなかったモジュールがある / そのうち pytest のテストモジュールがある。
    unobserved_flags: (bool, bool),
    /// 同じ名前のフォールバックの結果 (引く側のモジュールに依らないので名前ごとに覚える)。
    fallbacks: RefCell<HashMap<String, Resolution>>,
}

/// import の対象のモジュールの解決先。
#[derive(Default)]
struct Targets {
    modules: Vec<usize>,
    /// 相対 import か、確かめられる root で見つけた。偽なら経路の末尾だけが一致した推測。
    confirmed: bool,
    /// 中身を見られなかったモジュールが解決先になりうる。
    unobserved: bool,
}

impl Targets {
    /// モジュールが (中身を見られないものを含めて) あるか。
    fn exists(&self) -> bool {
        !self.modules.is_empty() || self.unobserved
    }
}

/// 引く名前 (モジュール, 名前空間, 名前, ここまでの経路を確かめられたか)。
type BindingKey = (usize, Option<usize>, String, bool);

/// クラスに対する操作のうち、自身か基底の定義によって、ふつうと違うものを返しうるもの。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Probe {
    /// `X[...]` (独自の `__class_getitem__` か metaclass の `__getitem__`)。
    Subscript,
    /// `X(...)` (独自の `__new__` か metaclass の `__call__`)。
    Call,
    /// `X | Y` などの演算 (metaclass の `__or__` など)。
    Operator,
}

impl Probe {
    /// クラス自身の定義が、ふつうと違うものを返しうるか。
    fn custom(self, class: &ClassFacts) -> bool {
        class.metaclass
            || match self {
                Self::Subscript => class.class_getitem,
                Self::Call => class.new != OwnBinding::Absent,
                Self::Operator => false,
            }
    }
}

/// 基底がプロジェクトの外にあっても、ふつうの振る舞いだと分かっているモジュール
/// (`typing.Generic`・`typing.Protocol`・`collections.abc.Mapping`・`abc.ABC` など)。
const STANDARD_BASE_MODULES: &[&[&str]] = &[
    &["typing"],
    &["typing_extensions"],
    &["collections", "abc"],
    &["abc"],
];

/// 引数を基底にしないクラスを作る、標準ライブラリの関数とクラス ((モジュール, 名前))。
const STANDARD_FACTORIES: &[(&[&str], &str)] = &[
    (&["collections"], "namedtuple"),
    (&["typing"], "NamedTuple"),
    (&["typing"], "TypedDict"),
    (&["typing"], "NewType"),
    (&["typing_extensions"], "NamedTuple"),
    (&["typing_extensions"], "TypedDict"),
    (&["typing_extensions"], "NewType"),
    (&["enum"], "Enum"),
    (&["enum"], "IntEnum"),
    (&["enum"], "StrEnum"),
    (&["enum"], "Flag"),
    (&["enum"], "IntFlag"),
];

/// 1 回の解決の途中の状態。
struct Walk {
    /// いま引いているものの結果。
    out: Resolution,
    /// 引いた名前 → 引いた結果 (束縛があったか, 結果)。同じ名前は 1 回の解決で 1 度だけ引く
    /// (別の経路で引いた結果も落とさない)。引いている途中なら `None` で、循環はそこで止める。
    memo: HashMap<BindingKey, Option<(bool, Resolution)>>,
    /// たどっている途中の属性の (モジュール, 名前空間, 要素の並び)。属性の循環で止める。
    attributes: HashSet<(usize, Option<usize>, Vec<String>)>,
    /// (クラス, 操作) → ふつうと違うものを返しうるか (調べている途中なら `None`)。
    probes: HashMap<(ClassId, Probe), Option<bool>>,
    /// 名前の束縛と継承をたどる深さ ([`MAX_BINDING_DEPTH`] を超えたら止める)。
    depth: usize,
    /// 解決しきれないときに、同じ名前のフォールバックを足すか (フォールバックの中では足さない)。
    fallback: bool,
    /// ここまでの経路を確かめられたか。
    confirmed: bool,
}

impl Walk {
    fn new(fallback: bool, confirmed: bool) -> Self {
        Self {
            out: Resolution::default(),
            memo: HashMap::new(),
            attributes: HashSet::new(),
            probes: HashMap::new(),
            depth: 0,
            fallback,
            confirmed,
        }
    }
}

/// モジュールのパス (`a/b/c.py` → `a.b.c`、`a/b/__init__.py` → `a.b`) のすべての末尾。
fn module_suffixes(path: &str) -> Vec<String> {
    let Some(stem) = path.strip_suffix(".py") else {
        return Vec::new();
    };
    let mut parts: Vec<&str> = stem.split('/').collect();
    if parts.last() == Some(&"__init__") {
        parts.pop();
    }
    (0..parts.len())
        .map(|first| parts[first..].join("."))
        .collect()
}

/// `parts` (ディレクトリの要素) をパスの接頭辞 (`a/b/`) にする。
fn prefix_of(parts: &[&str]) -> String {
    parts.iter().map(|part| format!("{part}/")).collect()
}

impl ProjectTests {
    /// `unobserved` は中身を見られなかったモジュールの (パス, pytest のテストモジュールか)。
    pub(crate) fn new(modules: Vec<ProjectModule>, unobserved: Vec<(String, bool)>) -> Self {
        let mut by_path: HashMap<String, usize> = HashMap::new();
        let mut by_suffix: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, module) in modules.iter().enumerate() {
            by_path.insert(module.path.clone(), index);
            for suffix in module_suffixes(&module.path) {
                by_suffix.entry(suffix).or_default().push(index);
            }
            let mut names: Vec<&str> = module.facts.bound_names().collect();
            names.sort_unstable();
            names.dedup();
            for name in names {
                by_name.entry(name.to_string()).or_default().push(index);
            }
        }
        let unobserved_flags = (
            !unobserved.is_empty(),
            unobserved.iter().any(|(_, pytest_module)| *pytest_module),
        );
        let unobserved_suffixes = unobserved
            .iter()
            .flat_map(|(path, _)| module_suffixes(path))
            .collect();
        let unobserved_paths: HashSet<String> =
            unobserved.into_iter().map(|(path, _)| path).collect();
        let has_init = |prefix: &str| {
            let init = format!("{prefix}__init__.py");
            by_path.contains_key(&init) || unobserved_paths.contains(&init)
        };
        let basedirs = modules
            .iter()
            .map(|module| {
                let mut parts: Vec<&str> = module.path.split('/').collect();
                parts.pop();
                while !parts.is_empty() && has_init(&prefix_of(&parts)) {
                    parts.pop();
                }
                prefix_of(&parts)
            })
            .collect();
        Self {
            modules,
            basedirs,
            by_path,
            by_suffix,
            by_name,
            unobserved_paths,
            unobserved_suffixes,
            unobserved_flags,
            fallbacks: RefCell::default(),
        }
    }

    pub(crate) fn modules(&self) -> &[ProjectModule] {
        &self.modules
    }

    pub(crate) fn collect(&self) -> Collected {
        super::collect(self)
    }

    /// 接頭辞 `prefix` の下の `relative` (`a/b`、空なら接頭辞のパッケージ自身) のモジュール。
    fn lookup_module(&self, prefix: &str, relative: &str, targets: &mut Targets) {
        let paths = if relative.is_empty() {
            vec![format!("{prefix}__init__.py")]
        } else {
            vec![
                format!("{prefix}{relative}.py"),
                format!("{prefix}{relative}/__init__.py"),
            ]
        };
        for path in &paths {
            if let Some(&module) = self.by_path.get(path) {
                targets.modules.push(module);
            }
            targets.unobserved |= self.unobserved_paths.contains(path);
        }
    }

    /// `importer` の文脈で、import の対象のモジュールのファイル。
    fn resolve_module(&self, importer: usize, target: &ModuleRef) -> Targets {
        let relative = target.path.join("/");
        let mut targets = Targets {
            confirmed: true,
            ..Targets::default()
        };
        if target.level > 0 {
            // 相対 import: importer のディレクトリから level - 1 段上がる。
            let mut parts: Vec<&str> = self.modules[importer].path.split('/').collect();
            parts.pop();
            for _ in 1..target.level {
                if parts.pop().is_none() {
                    return Targets::default();
                }
            }
            self.lookup_module(&prefix_of(&parts), &relative, &mut targets);
            return targets;
        }
        if relative.is_empty() {
            return Targets::default();
        }
        let mut roots: Vec<&str> = vec![self.basedirs[importer].as_str()];
        for root in ["", "src/"] {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        for root in roots {
            self.lookup_module(root, &relative, &mut targets);
        }
        targets.modules.sort_unstable();
        targets.modules.dedup();
        if targets.exists() {
            return targets;
        }
        // 推測: プロジェクトの .py のうち、経路の末尾がモジュール名に一致するもの。
        let key = target.path.join(".");
        Targets {
            modules: self.by_suffix.get(&key).cloned().unwrap_or_default(),
            confirmed: false,
            unobserved: self.unobserved_suffixes.contains(&key),
        }
    }

    fn mark_root(walk: &mut Walk) {
        if walk.confirmed {
            walk.out.root = true;
        } else {
            walk.out.guessed_root = true;
        }
    }

    /// `f` が引いたものを、いまの結果とは別の結果に集める (引いた名前の結果は共有する)。
    fn collect_into(
        &self,
        walk: &mut Walk,
        confirmed: bool,
        f: impl FnOnce(&Self, &mut Walk),
    ) -> Resolution {
        let outer = std::mem::take(&mut walk.out);
        let saved = walk.confirmed;
        walk.confirmed &= confirmed;
        f(self, walk);
        walk.confirmed = saved;
        std::mem::replace(&mut walk.out, outer)
    }

    /// `base` を、`module` の名前空間 `scope` で書いた式として別の結果に解決する。
    fn resolve_apart(
        &self,
        module: usize,
        scope: Option<usize>,
        base: &Base,
        walk: &mut Walk,
    ) -> Resolution {
        self.collect_into(walk, true, |this, inner| {
            this.resolve_expr(module, scope, base, inner);
        })
    }

    fn resolve_expr(&self, module: usize, scope: Option<usize>, base: &Base, walk: &mut Walk) {
        let before = walk.out.classes.len();
        let chain = lookup_scopes(scope);
        match base {
            Base::Name(name) => {
                let mut bound = false;
                for &scope in &chain {
                    bound |= self.find(module, scope, name, walk);
                }
                // 静的な束縛が無い名前は組み込みか未定義 (`import *` の先が分からないときは
                // `find` が不完全にしている)。
                walk.out.builtin_type |= !bound && name == "type";
            }
            Base::Qualified(path) => self.find_attribute(module, &chain, path, walk),
            Base::Subscript(value) => {
                let found = self.resolve_apart(module, scope, value, walk);
                walk.out.root |= found.root;
                walk.out.guessed_root |= found.guessed_root;
                walk.out.incomplete |= found.incomplete;
                // 入れ物やインスタンスの要素 (`registry["x"]`) は実行時に決まる。
                walk.out.wildcard |= found.wildcard || found.object;
                for &(id, confirmed) in &found.classes {
                    if self.probe(id, Probe::Subscript, walk) {
                        // 独自の `__class_getitem__` や metaclass の `__getitem__` は何でも
                        // 返しうる (値のクラスは推測として残す)。
                        walk.out.wildcard = true;
                        walk.out.incomplete = true;
                        walk.out.classes.push((id, false));
                    } else {
                        // 標準の generic は、基底を値のクラスにする。
                        walk.out.classes.push((id, confirmed));
                    }
                }
            }
            Base::Call { callee, arguments } => {
                self.resolve_call(module, scope, callee, arguments, walk);
            }
            Base::Union(alternatives) => {
                for alternative in alternatives {
                    self.resolve_expr(module, scope, alternative, walk);
                }
            }
            // 入れ物は、あとで要素を足しうる (数や文字列だけの表示は除く)。
            Base::Container { .. } | Base::Mapping { .. } => {
                walk.out.object |= !base.is_scalar();
            }
            Base::Function => walk.out.object = true,
            Base::Item(value) | Base::Values(value) => {
                self.resolve_item(module, scope, value, walk);
            }
            Base::Operator(operands) => self.resolve_operator(module, scope, operands, walk),
            Base::NotClass => {}
            // 何に束縛するかが式から決まらない。
            Base::Other => walk.out.wildcard = true,
        }
        // 名前をプロジェクトのクラスに確かめられなかったときは、書き方で TestCase とみなす
        // (`from django.test import TestCase` のような第三者の TestCase の派生)。
        if base.is_unittest_root_text()
            && !walk.out.classes[before..]
                .iter()
                .any(|&(_, confirmed)| confirmed)
        {
            Self::mark_root(walk);
        }
    }

    /// 呼び出し `callee(arguments)` の結果。
    fn resolve_call(
        &self,
        module: usize,
        scope: Option<usize>,
        callee: &Base,
        arguments: &[Base],
        walk: &mut Walk,
    ) {
        // 標準ライブラリのクラスの作り方 (`namedtuple(...)`・`Enum(...)` など) は、引数を
        // 基底にしないプロジェクトの外のクラスを返す。
        if self.standard_factory(module, scope, callee) {
            walk.out.incomplete = true;
            return;
        }
        let target = self.resolve_apart(module, scope, callee, walk);
        // プロジェクトの関数・インスタンス・何でもありうるものの呼び出しは、何を返すかも
        // 分からない。
        if target.wildcard || target.object {
            walk.out.wildcard = true;
        }
        // プロジェクトのクラスの呼び出しはインスタンス。独自の `__new__`・metaclass・`type` や
        // プロジェクトの外の基底を持つクラス (metaclass でありうる) は何でも返しうる。
        let mut custom = false;
        for &(id, _) in &target.classes {
            custom |= self.probe(id, Probe::Call, walk);
        }
        if custom {
            walk.out.wildcard = true;
        } else if !target.classes.is_empty() {
            walk.out.object = true;
        }
        // ふつうのプロジェクトのクラスだけの呼び出しは、そのインスタンスで、引数に依らない。
        if !custom
            && !target.classes.is_empty()
            && !target.wildcard
            && !target.object
            && !target.incomplete
            && !target.root
            && !target.guessed_root
        {
            return;
        }
        // クラスやプロジェクトの外のもの (`type(...)`・`namedtuple(...)`・metaclass) の呼び出しは、
        // 引数のクラスを基底にしたクラスや、プロジェクトの外のものを返しうる。
        walk.out.incomplete = true;
        for argument in arguments {
            // 表示 (`(A, B)`・`{"k": A}`) は要素 (辞書はキーと値) を引数のクラスとして見る。
            let elements: Vec<&Base> = match argument {
                Base::Container {
                    elements: Some(elements),
                    ..
                } => elements.iter().collect(),
                Base::Mapping { keys, values } => keys.iter().chain(values).collect(),
                other => vec![other],
            };
            for element in elements {
                let found = self.resolve_apart(module, scope, element, walk);
                // プロジェクトの値 (入れ物・関数・インスタンス) を渡すと、その中身から何でも
                // 作りうる。
                walk.out.wildcard |= found.wildcard || found.object;
                walk.out.guessed_root |= found.root || found.guessed_root;
                walk.out
                    .classes
                    .extend(found.classes.iter().map(|&(id, _)| (id, false)));
            }
        }
    }

    /// 値 `value` の要素 (アンパック・反復・`*` / `**` の展開)。
    fn resolve_item(&self, module: usize, scope: Option<usize>, value: &Base, walk: &mut Walk) {
        let found = self.resolve_apart(module, scope, value, walk);
        // プロジェクトの入れ物・インスタンス・クラスの要素は実行時に決まる。呼び出しの結果の
        // 要素は、呼び出しの結果と同じく推測する (引数のクラスは推測のまま)。
        let call = matches!(value, Base::Call { .. });
        if found.wildcard || found.object || (!call && !found.classes.is_empty()) {
            walk.out.wildcard = true;
        }
        walk.out.incomplete = true;
        walk.out.guessed_root |= found.root || found.guessed_root;
        walk.out
            .classes
            .extend(found.classes.iter().map(|&(id, _)| (id, false)));
    }

    /// 演算 (`a | b` など) の結果。被演算子がプロジェクトの値なら、多重定義した演算子は何でも
    /// 返しうる。クラスどうしの演算 (`A | None`) は、metaclass が演算子を持ちうるときだけ。
    fn resolve_operator(
        &self,
        module: usize,
        scope: Option<usize>,
        operands: &[Base],
        walk: &mut Walk,
    ) {
        for operand in operands {
            let found = self.resolve_apart(module, scope, operand, walk);
            walk.out.incomplete |= found.incomplete;
            walk.out.wildcard |= found.wildcard || found.object;
            for &(id, _) in &found.classes {
                if self.probe(id, Probe::Operator, walk) {
                    walk.out.wildcard = true;
                }
            }
        }
    }

    /// クラス `id` に対する操作 `probe` が、ふつうと違うものを返しうるか。自身か基底 (たどれる
    /// 範囲) が独自の定義を持つか metaclass を指定するか、基底が何でもありうるか、ふつうと
    /// 分かっていないプロジェクトの外の基底を持つとき。調べている途中のクラス (継承の循環) と、
    /// 深すぎる継承は、返しうるとする。
    fn probe(&self, id: ClassId, probe: Probe, walk: &mut Walk) -> bool {
        match walk.probes.get(&(id, probe)) {
            Some(Some(custom)) => return *custom,
            Some(None) => return true,
            None => {}
        }
        if walk.depth >= MAX_BINDING_DEPTH {
            return true;
        }
        walk.probes.insert((id, probe), None);
        walk.depth += 1;
        let (module, class) = id;
        let class = &self.modules[module].facts.classes[class];
        let mut custom = probe.custom(class);
        for base in &class.bases {
            if custom {
                break;
            }
            let found = self.resolve_apart(module, class.owner, base, walk);
            // `type` を継承するクラス (metaclass) の呼び出しは、クラスを作る。
            let builtin_type = probe == Probe::Call && found.builtin_type;
            // プロジェクトの外の基底は、呼び出し (metaclass でありうる) ではいつも、subscript と
            // 演算ではふつうと分かっていないときに、独自の定義を持ちうる。
            let external = found.incomplete
                && (probe == Probe::Call
                    || !self.standard_external_base(module, class.owner, base));
            custom = builtin_type
                || external
                || found.wildcard
                || found.object
                || found
                    .classes
                    .iter()
                    .any(|&(base, _)| self.probe(base, probe, walk));
        }
        walk.depth -= 1;
        walk.probes.insert((id, probe), Some(custom));
        custom
    }

    /// `scope` から引く `name` の束縛が import だけならその import。クラス・別名・関数の束縛か、
    /// module 直下で引くのに `import *` (どの名前も上書きしうる) があれば `None`。
    fn import_only_bindings(
        &self,
        module: usize,
        scope: Option<usize>,
        name: &str,
    ) -> Option<Vec<&Import>> {
        let facts = &self.modules[module].facts;
        if !facts.star_imports.is_empty() {
            return None;
        }
        let mut imports = Vec::new();
        for scope in lookup_scopes(scope) {
            let bindings = facts.bindings(scope, name);
            if !bindings.classes.is_empty() || !bindings.aliases.is_empty() || bindings.function {
                return None;
            }
            imports.extend(bindings.imports);
        }
        (!imports.is_empty()).then_some(imports)
    }

    /// 呼ぶもの `callee` が、標準ライブラリのクラスの作り方 ([`STANDARD_FACTORIES`]) を指す
    /// import だけに束縛されているか (`from collections import namedtuple` の `namedtuple`、
    /// `import enum` の `enum.Enum`)。
    fn standard_factory(&self, module: usize, scope: Option<usize>, callee: &Base) -> bool {
        let (head, middle, tail): (&str, Option<&[String]>, Option<&str>) = match callee {
            Base::Name(name) => (name, None, None),
            Base::Qualified(path) => (
                &path[0],
                Some(&path[1..path.len() - 1]),
                Some(&path[path.len() - 1]),
            ),
            _ => return false,
        };
        let Some(imports) = self.import_only_bindings(module, scope, head) else {
            return false;
        };
        let known = |path: &[String], name: &str| {
            STANDARD_FACTORIES.iter().any(|(module, factory)| {
                *factory == name && module.iter().copied().eq(path.iter().map(String::as_str))
            })
        };
        !imports.is_empty()
            && imports.iter().all(|import| match (import, middle, tail) {
                (Import::From { module, name }, None, None) => {
                    module.level == 0 && known(&module.path, name)
                }
                (import, Some(middle), Some(tail)) => {
                    let module = import.as_module();
                    let mut path = module.path;
                    path.extend(middle.iter().cloned());
                    module.level == 0 && known(&path, tail)
                }
                _ => false,
            })
    }

    /// 基底の式 `base` (subscript を除いた先頭) が、ふつうの振る舞いと分かっているプロジェクトの
    /// 外のモジュール ([`STANDARD_BASE_MODULES`]) からの import だけに束縛されているか
    /// (`from typing import Generic` の `Generic[T]`、`import typing` の `typing.Protocol`)。
    fn standard_external_base(&self, module: usize, scope: Option<usize>, base: &Base) -> bool {
        let mut base = base;
        while let Base::Subscript(value) = base {
            base = value;
        }
        // (先頭の名前, 先頭をモジュールとして読むときの間の要素)。識別子だけなら `None`。
        let (head, middle): (&str, Option<&[String]>) = match base {
            Base::Name(name) => (name, None),
            Base::Qualified(path) => (&path[0], Some(&path[1..path.len() - 1])),
            _ => return false,
        };
        let Some(imports) = self.import_only_bindings(module, scope, head) else {
            return false;
        };
        let standard = |path: &[String]| {
            STANDARD_BASE_MODULES
                .iter()
                .any(|standard| standard.iter().copied().eq(path.iter().map(String::as_str)))
        };
        !imports.is_empty()
            && imports.iter().all(|import| {
                // 先頭の名前が指すモジュール (`from typing import Generic` の `Generic` は
                // `typing` の属性なので `typing`、`from collections import abc` の `abc.X` は
                // `collections.abc`)。
                let path = match (import, middle) {
                    (Import::From { module, .. }, None) => {
                        (module.level == 0).then(|| module.path.clone())
                    }
                    (import, Some(middle)) => {
                        let module = import.as_module();
                        (module.level == 0).then(|| {
                            let mut path = module.path;
                            path.extend(middle.iter().cloned());
                            path
                        })
                    }
                    (Import::Module(_), None) => None,
                };
                path.is_some_and(|path| standard(&path))
            })
    }

    /// `module` の名前空間 `scope` の `name` の束縛をたどり、束縛があったかを返す。
    /// 無くても不完全にはしない (呼び出し側が決める。`import *` の先には無いのが普通)。
    fn find(&self, module: usize, scope: Option<usize>, name: &str, walk: &mut Walk) -> bool {
        let key = (module, scope, name.to_string(), walk.confirmed);
        match walk.memo.get(&key) {
            Some(Some((found, resolution))) => {
                let found = *found;
                let resolution = resolution.clone();
                walk.out.absorb(&resolution);
                return found;
            }
            // 引いている途中 (循環)。
            Some(None) => return true,
            None => {}
        }
        if walk.depth >= MAX_BINDING_DEPTH {
            walk.out.wildcard = true;
            return true;
        }
        walk.memo.insert(key.clone(), None);
        walk.depth += 1;
        let outer = std::mem::take(&mut walk.out);
        let found = self.find_bindings(module, scope, name, walk);
        let resolution = std::mem::replace(&mut walk.out, outer);
        walk.depth -= 1;
        walk.out.absorb(&resolution);
        walk.memo.insert(key, Some((found, resolution)));
        found
    }

    /// [`Self::find`] の本体 (結果を `walk.out` に集める)。
    fn find_bindings(
        &self,
        module: usize,
        scope: Option<usize>,
        name: &str,
        walk: &mut Walk,
    ) -> bool {
        let facts = &self.modules[module].facts;
        let bindings = facts.bindings(scope, name);
        let mut found = !bindings.is_empty();
        // 構文エラーのあるファイルは、束縛を取りこぼしうる。
        walk.out.incomplete |= facts.has_error;
        let confirmed = walk.confirmed;
        walk.out.classes.extend(
            bindings
                .classes
                .iter()
                .map(|&class| ((module, class), confirmed)),
        );
        walk.out.object |= bindings.function;
        for alias in bindings.aliases {
            self.resolve_expr(module, scope, alias, walk);
        }
        for import in bindings.imports {
            if import.is_unittest_root() {
                // 標準ライブラリの TestCase。中身は分かっているので、ほかを引かない。
                Self::mark_root(walk);
                continue;
            }
            // モジュールとしても指しうる (`import a` はモジュール、`from p import a` の `a` は
            // サブモジュールかもしれない)。属性はそのモジュールで引く。
            if import.is_unittest_module() {
                walk.out.unittest_module = true;
            } else {
                walk.out.modules.push((module, import.as_module()));
            }
            if let Import::From {
                module: target,
                name: imported,
            } = import
            {
                self.find_from(module, target, imported, walk);
            }
        }
        if scope.is_none() {
            for star in &facts.star_imports {
                let targets = self.resolve_module(module, star);
                if targets.unobserved {
                    walk.out.wildcard = true;
                    found = true;
                }
                if targets.modules.is_empty() {
                    // プロジェクトの外の `import *` は、どの名前を入れるか分からない。
                    walk.out.incomplete |= !targets.unobserved;
                    continue;
                }
                walk.out.incomplete |= !targets.confirmed;
                let saved = walk.confirmed;
                walk.confirmed &= targets.confirmed;
                for &target in &targets.modules {
                    let target_facts = &self.modules[target].facts;
                    if !target_facts.star_exports(name) {
                        continue;
                    }
                    let bound = self.find(target, None, name, walk);
                    // `__all__` にだけある名前は、module の `__getattr__` が返しうる。
                    if !bound && target_facts.star_may_call_getattr(name) {
                        walk.out.wildcard = true;
                    }
                    found |= bound;
                }
                walk.confirmed = saved;
            }
        }
        found
    }

    /// `importer` の `from target import name`。
    fn find_from(&self, importer: usize, target: &ModuleRef, name: &str, walk: &mut Walk) {
        let targets = self.resolve_module(importer, target);
        if targets.unobserved {
            walk.out.wildcard = true;
        }
        if !targets.modules.is_empty() && self.find_in_modules(&targets, name, walk) {
            return;
        }
        // サブモジュール (`from pkg import helpers`) はモジュールの束縛で、クラスではない。
        if targets.unobserved || self.resolve_module(importer, &target.child(name)).exists() {
            return;
        }
        // 第三者・標準ライブラリのモジュールか、静的に見えない束縛。
        self.give_up(name, walk);
    }

    /// 解決したモジュールの module 直下で `name` を引き、束縛があったかを返す。推測の解決先は
    /// 不完全にし、束縛が無く module の `__getattr__` があれば、どのクラスでもありうるとする。
    fn find_in_modules(&self, targets: &Targets, name: &str, walk: &mut Walk) -> bool {
        walk.out.incomplete |= !targets.confirmed;
        let saved = walk.confirmed;
        walk.confirmed &= targets.confirmed;
        let mut found = false;
        for &module in &targets.modules {
            let bound = self.find(module, None, name, walk);
            if !bound && self.modules[module].facts.module_getattr {
                walk.out.wildcard = true;
                found = true;
            }
            found |= bound;
        }
        walk.confirmed = saved;
        found
    }

    /// `a.b.X` の属性を、要素ごとに置き場をたどって引く。置き場は、モジュール (import の束縛と
    /// 代入した別名、そのサブモジュール。module 直下の束縛を引く)、クラス (本体の束縛を引く。
    /// 基底の名前空間は見ない)、クラスでないプロジェクトの値 (属性は何でもありうる)。`unittest`
    /// のモジュールの `TestCase` は root。末尾がどこにも見つからなければ、末尾の名前の
    /// フォールバックを推測として足す (末尾の名前で、同じ名前の別のクラスを確かめたことには
    /// しない)。
    fn find_attribute(
        &self,
        module: usize,
        chain: &[Option<usize>],
        path: &[String],
        walk: &mut Walk,
    ) {
        let (head, rest) = (&path[0], &path[1..]);
        let Some(tail) = rest.last() else {
            return;
        };
        let key = (module, chain[0], path.to_vec());
        if !walk.attributes.insert(key.clone()) {
            return;
        }
        let mut found = false;
        // 先頭の名前が指しうるクラス・値・モジュール。
        let head_found = self.collect_into(walk, true, |this, inner| {
            for &scope in chain {
                this.find(module, scope, head, inner);
            }
        });
        walk.out.incomplete |= head_found.incomplete;
        walk.out.wildcard |= head_found.wildcard;
        let mut classes = head_found.classes;
        let mut object = head_found.object;
        let mut modules = head_found.modules;
        let mut unittest_module = head_found.unittest_module;
        let mut builtin_type = false;
        for (position, segment) in rest.iter().enumerate() {
            let last = position + 1 == rest.len();
            // `unittest` のモジュールの TestCase (`unittest.TestCase`・`unittest.case.TestCase`)。
            if unittest_module && UNITTEST_ROOT_NAMES.contains(&tail.as_str()) {
                Self::mark_root(walk);
                found = true;
            }
            modules.sort_by(|(left, a), (right, b)| {
                (left, a.level, &a.path).cmp(&(right, b.level, &b.path))
            });
            modules.dedup();
            let mut step = Resolution::default();
            let mut bound = false;
            let mut next_modules: Vec<(usize, ModuleRef)> = Vec::new();
            // クラスでないプロジェクトの値の属性は、何でもありうる。
            if object {
                step.wildcard = true;
                bound = true;
            }
            for (importer, target) in &modules {
                let targets = self.resolve_module(*importer, target);
                if targets.unobserved {
                    step.wildcard = true;
                    bound = true;
                }
                if !targets.modules.is_empty() {
                    let members = self.collect_into(walk, true, |this, inner| {
                        bound |= this.find_in_modules(&targets, segment, inner);
                    });
                    step.absorb(&members);
                }
                next_modules.push((*importer, target.child(segment)));
            }
            for &((class_module, class), confirmed) in &classes {
                let members = self.collect_into(walk, confirmed, |this, inner| {
                    bound |= this.find(class_module, Some(class), segment, inner);
                });
                step.absorb(&members);
            }
            // その名前空間で import した名前は、次の要素のモジュールにもなる。
            next_modules.append(&mut step.modules);
            walk.out.incomplete |= step.incomplete;
            walk.out.wildcard |= step.wildcard;
            if last {
                walk.out.root |= step.root;
                walk.out.guessed_root |= step.guessed_root;
                found |= bound;
            } else if step.root || step.guessed_root {
                // TestCase の属性はプロジェクトの外のもの。
                walk.out.incomplete = true;
            }
            classes = step.classes;
            object = step.object;
            modules = next_modules;
            unittest_module = step.unittest_module;
            builtin_type = step.builtin_type;
        }
        // 末尾がサブモジュールなら、モジュール (クラスではない)。
        if !found
            && modules
                .iter()
                .any(|(importer, target)| self.resolve_module(*importer, target).exists())
        {
            found = true;
        }
        walk.out.classes.extend(classes);
        walk.out.object |= object;
        walk.out.modules.extend(modules);
        walk.out.unittest_module |= unittest_module;
        walk.out.builtin_type |= builtin_type;
        if !found {
            self.give_up(tail, walk);
        }
        walk.attributes.remove(&key);
    }

    /// 解決しきれなかった。不完全にし、同じ名前のフォールバックを推測として足す。
    fn give_up(&self, name: &str, walk: &mut Walk) {
        walk.out.incomplete = true;
        if !walk.fallback {
            return;
        }
        let cached = self.fallbacks.borrow().get(name).cloned();
        let fallback = cached.unwrap_or_else(|| {
            let mut inner = Walk::new(false, false);
            for &module in self.by_name.get(name).map_or(&[][..], Vec::as_slice) {
                let facts = &self.modules[module].facts;
                self.find(module, None, name, &mut inner);
                for class in 0..facts.classes.len() {
                    if !facts.bindings(Some(class), name).is_empty() {
                        self.find(module, Some(class), name, &mut inner);
                    }
                }
            }
            self.fallbacks
                .borrow_mut()
                .insert(name.to_string(), inner.out.clone());
            inner.out
        });
        walk.out
            .classes
            .extend(fallback.classes.iter().map(|&(id, _)| (id, false)));
        walk.out.guessed_root |= fallback.root || fallback.guessed_root;
        walk.out.wildcard |= fallback.wildcard;
        walk.out.object |= fallback.object;
        walk.out.modules.extend(fallback.modules.iter().cloned());
        walk.out.unittest_module |= fallback.unittest_module;
    }
}

impl Universe for ProjectTests {
    fn module_count(&self) -> usize {
        self.modules.len()
    }

    fn facts(&self, module: usize) -> &PythonModuleFacts {
        &self.modules[module].facts
    }

    fn is_pytest_module(&self, module: usize) -> bool {
        self.modules[module].pytest_module
    }

    fn is_test_module(&self, module: usize) -> bool {
        let module = &self.modules[module];
        module.pytest_module
            || module
                .path
                .rsplit('/')
                .next()
                .is_some_and(|name| name.starts_with("test") && name.ends_with(".py"))
    }

    fn resolve(&self, module: usize, scope: Option<usize>, base: &Base) -> Resolution {
        let mut walk = Walk::new(true, true);
        self.resolve_expr(module, scope, base, &mut walk);
        let mut out = walk.out;
        out.classes.sort_unstable();
        out.classes.dedup();
        out
    }

    fn imported_bindings(&self, module: usize) -> Vec<(String, Resolution)> {
        let facts = &self.modules[module].facts;
        let mut names: BTreeSet<String> = facts
            .imports
            .iter()
            .filter(|(_, import)| matches!(import, Import::From { .. }))
            .map(|(name, _)| name.clone())
            .collect();
        // `from M import *` で入る名前 (M の `import *` で入る名前も含む)。
        let mut wildcard = false;
        let mut pending: Vec<usize> = Vec::new();
        for star in &facts.star_imports {
            let targets = self.resolve_module(module, star);
            wildcard |= targets.unobserved;
            pending.extend(targets.modules);
        }
        let mut seen: HashSet<usize> = HashSet::new();
        while let Some(target) = pending.pop() {
            if !seen.insert(target) {
                continue;
            }
            let target_facts = &self.modules[target].facts;
            names.extend(target_facts.star_exported_names().map(str::to_string));
            wildcard |= target_facts.star_names_are_dynamic();
            for star in &target_facts.star_imports {
                let targets = self.resolve_module(target, star);
                wildcard |= targets.unobserved;
                pending.extend(targets.modules);
            }
        }
        let mut bindings: Vec<(String, Resolution)> = names
            .into_iter()
            .map(|name| {
                let resolution = self.resolve(module, None, &Base::Name(name.clone()));
                (name, resolution)
            })
            .collect();
        if wildcard {
            // 中身を見られないモジュールの `import *` は、どの名前を入れるか分からない。
            bindings.push((
                String::new(),
                Resolution {
                    wildcard: true,
                    ..Resolution::default()
                },
            ));
        }
        bindings
    }

    fn unobserved(&self) -> (bool, bool) {
        self.unobserved_flags
    }
}
