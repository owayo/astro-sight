//! pytest が既定の規約で集めるテストと、名前で呼ぶ準備・後始末の判定。
//!
//! pytest は `test_*.py` / `*_test.py` (`python_files` の既定) のモジュールから、名前が `test` で
//! 始まる関数 (`python_functions`) と `Test` で始まるクラス (`python_classes`) を集める。集めた
//! クラスでは、自身と基底 (`__mro__`) の名前空間から `test` で始まるメソッドと入れ子の `Test*`
//! クラスを集める。xunit 形式の準備と後始末 (`setup_module` / `setup_method` など) と
//! `pytest_generate_tests` も名前で呼ぶ。呼び出し側に名前が現れないので、参照を数えるだけでは
//! dead と誤報する。
//!
//! dead と断定しない側に倒す。クラスを集めない (pytest が `__init__` / `__new__` を持つクラスを
//! 飛ばす) と言うのは、`object` 以外の基底・metaclass・デコレータを持たず、本体の直下に
//! デコレータの無い `def __init__` / `def __new__` を書いたクラスだけにする。基底の
//! `__init_subclass__` や metaclass は `__init__` を差し替えうるので、それ以外のクラスは集めると
//! みなす。基底は同じファイルのクラス定義と単純な別名 (`Alias = Mixin`) を名前で引く。
//! 見るのは既定の規約だけで、設定ファイル (`python_classes` など)・実行引数・独自の収集
//! プラグイン・`__test__ = False`・nose 形式の `setup` / `teardown`・別ファイルの基底・
//! 実行時の書き換えは見ない。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::models::location::Range;

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

/// 集めるクラス (と、その基底) に書くと pytest が名前で呼ぶメソッド。
const CLASS_HOOKS: &[&str] = &[
    "setup_method",
    "teardown_method",
    "setup_class",
    "teardown_class",
    "pytest_generate_tests",
];

/// 1 ファイルの中で、pytest が集めるか名前で呼ぶ定義。
#[derive(Debug, Default)]
pub(crate) struct PytestEntrypoints {
    /// 定義ノード (`class_definition` / `function_definition` / lambda を束縛する `assignment`) の
    /// 開始位置 (行, 列)。Python のシンボルの range は定義ノード全体なので、その開始位置で引く。
    starts: HashSet<(usize, usize)>,
}

impl PytestEntrypoints {
    /// `root` は pytest のテストモジュール (ファイル名で判定済み) の構文木。
    pub(crate) fn collect(root: Node<'_>, source: &[u8]) -> Self {
        let module = ModuleFacts::collect(root, source);
        let base_candidates: Vec<Vec<usize>> = module
            .classes
            .iter()
            .map(|class| {
                class
                    .bases
                    .iter()
                    .flat_map(|base| module.candidates(base))
                    .collect()
            })
            .collect();
        let markers = module.test_markers(&base_candidates);

        let mut starts: HashSet<(usize, usize)> = module.functions.iter().copied().collect();
        let count = module.classes.len();
        let mut examined = vec![false; count];
        let mut walked = vec![false; count];
        // 名前と印で集めるか調べるクラス (module 直下と、歩いたクラスの入れ子)。
        let mut to_examine: Vec<usize> = (0..count)
            .filter(|&index| module.classes[index].owner.is_none())
            .collect();
        // `TestAlias = Mixin` のように `Test*` の名前へ束縛したクラス。pytest はその名前で集める。
        let mut aliased: Vec<usize> = module.test_alias_targets(&module.aliases);
        // 名前空間を歩くクラス (集めたクラスと、その基底の候補)。
        let mut to_walk: Vec<usize> = Vec::new();
        loop {
            if let Some(index) = to_walk.pop() {
                if std::mem::replace(&mut walked[index], true) {
                    continue;
                }
                let class = &module.classes[index];
                starts.extend(class.members.iter().copied());
                to_examine.extend(class.nested.iter().copied());
                aliased.extend(module.test_alias_targets(&class.aliases));
                to_walk.extend(base_candidates[index].iter().copied());
            } else if let Some(index) = aliased.pop() {
                if !module.skipped_by_constructor(index) {
                    starts.insert(module.classes[index].start);
                    to_walk.push(index);
                }
            } else if let Some(index) = to_examine.pop() {
                if std::mem::replace(&mut examined[index], true) {
                    continue;
                }
                let class = &module.classes[index];
                if (class.name.starts_with("Test") || markers[index])
                    && !module.skipped_by_constructor(index)
                {
                    starts.insert(class.start);
                    to_walk.push(index);
                }
            } else {
                break;
            }
        }
        Self { starts }
    }

    /// `range` で始まる定義を、pytest が集めるか名前で呼ぶか。
    pub(crate) fn contains(&self, range: &Range) -> bool {
        self.starts
            .contains(&(range.start.line, range.start.column))
    }
}

/// クラス自身の名前空間での `__init__` / `__new__` の束縛。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OwnBinding {
    /// 名前空間に現れない。
    Absent,
    /// 本体の直下にあるデコレータの無い `def` の名前としてだけ現れる。
    Def,
    /// それ以外の形 (代入・条件付きの def・デコレータ付きの def・import・del など) で現れる。
    Unknown,
}

impl OwnBinding {
    fn observe(&mut self, plain_def: bool) {
        *self = match (*self, plain_def) {
            (Self::Absent | Self::Def, true) => Self::Def,
            _ => Self::Unknown,
        };
    }
}

/// クラスの基底や別名の右辺の式。
#[derive(Clone, Debug)]
enum Base {
    /// 識別子 (`Base`)。
    Name(String),
    /// 属性 (`helpers.Base`) の末尾、subscript (`Base[int]`) の値の名前。
    /// 同じファイルの同じ名前のクラスを、生存の根拠を広げる側にだけ使う。
    Qualified(String),
    /// 呼び出しや可変長引数など。
    Other,
}

impl Base {
    fn from_expression(node: Node<'_>, source: &[u8]) -> Self {
        // `Base[int][str]` の入れ子は値の側に深くなるので、再帰せずにたどる。
        let mut node = node;
        let mut subscripted = false;
        while node.kind() == "subscript" {
            let Some(value) = node.child_by_field_name("value") else {
                return Self::Other;
            };
            node = value;
            subscripted = true;
        }
        match node.kind() {
            "identifier" if subscripted => Self::Qualified(node_text(node, source).to_string()),
            "identifier" => Self::Name(node_text(node, source).to_string()),
            "attribute" => node
                .child_by_field_name("attribute")
                .map_or(Self::Other, |tail| {
                    Self::Qualified(node_text(tail, source).to_string())
                }),
            _ => Self::Other,
        }
    }

    fn name(&self) -> Option<&str> {
        match self {
            Self::Name(name) | Self::Qualified(name) => Some(name),
            Self::Other => None,
        }
    }
}

#[derive(Debug)]
struct ClassFacts {
    name: String,
    start: (usize, usize),
    /// 本体の中で定義したクラスの添字。module 直下なら `None`。
    owner: Option<usize>,
    decorated: bool,
    /// クラス文にキーワード引数 (`metaclass=...` など) がある。
    keyword_arguments: bool,
    bases: Vec<Base>,
    init: OwnBinding,
    new: OwnBinding,
    /// 名前空間に `__test__ = True` がある。
    test_marker: bool,
    /// 名前空間の `test*` と、pytest が名前で呼ぶメソッドの開始位置。
    members: Vec<(usize, usize)>,
    /// 名前空間の直下で定義したクラスの添字。
    nested: Vec<usize>,
    /// 名前空間の単純な別名 (`Name = Expression`)。
    aliases: Vec<(String, Base)>,
}

impl ClassFacts {
    fn observe_constructor(&mut self, name: &str, plain_def: bool) {
        match name {
            "__init__" => self.init.observe(plain_def),
            "__new__" => self.new.observe(plain_def),
            _ => {}
        }
    }
}

/// 1 モジュール分の事実。
#[derive(Debug, Default)]
struct ModuleFacts {
    /// 関数・lambda の本体の外にあるクラス定義。
    classes: Vec<ClassFacts>,
    /// 名前 → その名前のクラス定義の添字 (関数の中を除くすべてのスコープ)。
    classes_by_name: HashMap<String, Vec<usize>>,
    /// module 直下の単純な別名 (`Name = Expression`)。
    aliases: Vec<(String, Base)>,
    /// 名前 → その名前への単純な別名の右辺 (すべてのスコープ)。
    aliases_by_name: HashMap<String, Vec<Base>>,
    /// クラスの外から `__init__` / `__new__` に触れうる (module 直下の出現か、
    /// クラスの名前空間の `X.__init__`)。どのクラスについても飛ばすとは言わない。
    constructor_rewritten: bool,
    /// module 直下で `object` が束縛されうる (識別子の出現、同じ名前のクラス、star import)。
    object_rebound: bool,
    /// module 直下の `test*` と、pytest が名前で呼ぶ関数の開始位置。
    functions: Vec<(usize, usize)>,
}

/// 名前空間の走査で訪れるノード。
#[derive(Clone, Copy)]
struct Visit<'t> {
    node: Node<'t>,
    /// クラスの本体の直下の文か。
    in_body: bool,
    /// `decorated_definition` の子か。
    decorated: bool,
}

impl<'t> Visit<'t> {
    fn plain(node: Node<'t>) -> Self {
        Self {
            node,
            in_body: false,
            decorated: false,
        }
    }
}

impl ModuleFacts {
    /// 構文木を明示スタックで辿る。関数と lambda の本体には入らない (そこで定義したものは
    /// モジュールやクラスの属性にならず、pytest も集めない)。
    fn collect(root: Node<'_>, source: &[u8]) -> Self {
        let mut facts = Self::default();
        // (名前空間を持つクラスの添字 (module は None), 名前空間の文を子に持つノード)
        let mut scopes: Vec<(Option<usize>, Node<'_>)> = vec![(None, root)];
        while let Some((scope, statements)) = scopes.pop() {
            let mut stack: Vec<Visit<'_>> = Vec::new();
            push_children(&mut stack, statements, scope.is_some(), false);
            while let Some(visit) = stack.pop() {
                let node = visit.node;
                match node.kind() {
                    "class_definition" => {
                        let index = facts.add_class(node, source, scope, visit.decorated);
                        if let Some(body) = node.child_by_field_name("body") {
                            scopes.push((Some(index), body));
                        }
                    }
                    "function_definition" => {
                        if let Some(name) = definition_name(node, source) {
                            facts.add_function(
                                scope,
                                name,
                                start_of(node),
                                visit.in_body && !visit.decorated,
                            );
                        }
                    }
                    "decorated_definition" => {
                        push_children(&mut stack, node, visit.in_body, true);
                    }
                    // lambda の本体は式なので、定義も束縛も無い。
                    "lambda" => {}
                    "assignment" => {
                        facts.observe_assignment(scope, node, source);
                        push_children(&mut stack, node, false, false);
                    }
                    "attribute" => {
                        // `X.__init__` は、クラスの外からコンストラクタを書き換えうる。
                        if node
                            .child_by_field_name("attribute")
                            .is_some_and(|tail| is_constructor_name(node_text(tail, source)))
                        {
                            facts.constructor_rewritten = true;
                        }
                        if let Some(object) = node.child_by_field_name("object") {
                            stack.push(Visit::plain(object));
                        }
                    }
                    "identifier" => facts.observe_identifier(scope, node_text(node, source)),
                    "wildcard_import" if scope.is_none() => facts.object_rebound = true,
                    _ => push_children(&mut stack, node, false, false),
                }
            }
        }
        facts
    }

    fn add_class(
        &mut self,
        node: Node<'_>,
        source: &[u8],
        owner: Option<usize>,
        decorated: bool,
    ) -> usize {
        let index = self.classes.len();
        let name = definition_name(node, source)
            .unwrap_or_default()
            .to_string();
        let mut bases = Vec::new();
        let mut keyword_arguments = false;
        if let Some(arguments) = node.child_by_field_name("superclasses") {
            let mut cursor = arguments.walk();
            for argument in arguments.named_children(&mut cursor) {
                match argument.kind() {
                    "keyword_argument" => keyword_arguments = true,
                    "comment" => {}
                    _ => bases.push(Base::from_expression(argument, source)),
                }
            }
        }
        match owner {
            None => self.observe_identifier(None, &name),
            Some(owner) => {
                let class = &mut self.classes[owner];
                class.nested.push(index);
                class.observe_constructor(&name, false);
            }
        }
        self.classes_by_name
            .entry(name.clone())
            .or_default()
            .push(index);
        self.classes.push(ClassFacts {
            name,
            start: start_of(node),
            owner,
            decorated,
            keyword_arguments,
            bases,
            init: OwnBinding::Absent,
            new: OwnBinding::Absent,
            test_marker: false,
            members: Vec::new(),
            nested: Vec::new(),
            aliases: Vec::new(),
        });
        index
    }

    /// `def` か lambda の束縛を 1 つ記録する。`plain_def` は、クラスの本体の直下にある
    /// デコレータの無い `def` か。
    fn add_function(
        &mut self,
        scope: Option<usize>,
        name: &str,
        start: (usize, usize),
        plain_def: bool,
    ) {
        match scope {
            None => {
                self.observe_identifier(None, name);
                if name.starts_with("test") || MODULE_HOOKS.contains(&name) {
                    self.functions.push(start);
                }
            }
            Some(owner) => {
                let class = &mut self.classes[owner];
                class.observe_constructor(name, plain_def);
                if name.starts_with("test") || CLASS_HOOKS.contains(&name) {
                    class.members.push(start);
                }
            }
        }
    }

    fn observe_assignment(&mut self, scope: Option<usize>, node: Node<'_>, source: &[u8]) {
        let Some(left) = node
            .child_by_field_name("left")
            .filter(|left| left.kind() == "identifier")
        else {
            return;
        };
        let name = node_text(left, source);
        let Some(right) = node.child_by_field_name("right") else {
            return;
        };
        match right.kind() {
            // シンボルの抽出と同じく、識別子に lambda を束縛した代入を関数として扱う。
            "lambda" => self.add_function(scope, name, start_of(node), false),
            "true" if name == "__test__" => {
                if let Some(owner) = scope {
                    self.classes[owner].test_marker = true;
                }
            }
            _ => {
                let target = Base::from_expression(right, source);
                if target.name().is_none() {
                    return;
                }
                let alias = (name.to_string(), target.clone());
                match scope {
                    None => self.aliases.push(alias),
                    Some(owner) => self.classes[owner].aliases.push(alias),
                }
                self.aliases_by_name
                    .entry(name.to_string())
                    .or_default()
                    .push(target);
            }
        }
    }

    fn observe_identifier(&mut self, scope: Option<usize>, name: &str) {
        match scope {
            None => {
                if is_constructor_name(name) {
                    self.constructor_rewritten = true;
                }
                if name == "object" {
                    self.object_rebound = true;
                }
            }
            Some(owner) => self.classes[owner].observe_constructor(name, false),
        }
    }

    /// 基底や別名の右辺の名前から、生存の根拠を広げる側に使うクラスの候補 (同じ名前の
    /// すべてのクラスと、単純な別名をたどった先)。
    fn candidates(&self, base: &Base) -> Vec<usize> {
        let mut found = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut names: Vec<&str> = base.name().into_iter().collect();
        while let Some(name) = names.pop() {
            if !seen.insert(name) {
                continue;
            }
            if let Some(classes) = self.classes_by_name.get(name) {
                found.extend(classes.iter().copied());
            }
            if let Some(targets) = self.aliases_by_name.get(name) {
                names.extend(targets.iter().filter_map(Base::name));
            }
        }
        found
    }

    /// `Test*` の名前に束縛した別名の先のクラスの候補。
    fn test_alias_targets(&self, aliases: &[(String, Base)]) -> Vec<usize> {
        aliases
            .iter()
            .filter(|(name, _)| name.starts_with("Test"))
            .flat_map(|(_, target)| self.candidates(target))
            .collect()
    }

    /// pytest がクラスを飛ばす (`__init__` / `__new__` が `object` のものと違う) と構文から言えるか。
    fn skipped_by_constructor(&self, index: usize) -> bool {
        let class = &self.classes[index];
        let object_only = class.bases.is_empty()
            || (!self.object_rebound
                && class
                    .bases
                    .iter()
                    .all(|base| matches!(base, Base::Name(name) if name == "object")));
        !self.constructor_rewritten
            && !class.decorated
            && !class.keyword_arguments
            && object_only
            && (class.init == OwnBinding::Def || class.new == OwnBinding::Def)
    }

    /// 各クラスが `__test__ = True` を (自身か基底の候補で) 持つか。
    fn test_markers(&self, base_candidates: &[Vec<usize>]) -> Vec<bool> {
        let mut markers: Vec<bool> = self.classes.iter().map(|class| class.test_marker).collect();
        loop {
            let mut changed = false;
            for (index, candidates) in base_candidates.iter().enumerate() {
                if !markers[index] && candidates.iter().any(|&base| markers[base]) {
                    markers[index] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        markers
    }
}

/// `node` の名前付きの子を、走査のスタックに積む。集合を作るだけなので訪れる順は問わない。
fn push_children<'t>(stack: &mut Vec<Visit<'t>>, node: Node<'t>, in_body: bool, decorated: bool) {
    let mut cursor = node.walk();
    stack.extend(node.named_children(&mut cursor).map(|child| Visit {
        node: child,
        in_body,
        decorated,
    }));
}

fn is_constructor_name(name: &str) -> bool {
    matches!(name, "__init__" | "__new__")
}

fn definition_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name("name")
        .map(|name| node_text(name, source))
}

fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or_default()
}

fn start_of(node: Node<'_>) -> (usize, usize) {
    let start = node.start_position();
    (start.row, start.column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::symbols::extract_symbols;
    use crate::language::LangId;

    /// `source` の Python のシンボルのうち、定義の位置が集合にあるものの名前
    /// (メソッドは `クラス.名前`) を並べて返す。
    fn live(source: &str) -> Vec<String> {
        let language = LangId::Python.ts_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        let symbols = extract_symbols(root, source.as_bytes(), LangId::Python).unwrap();
        let entrypoints = PytestEntrypoints::collect(root, source.as_bytes());
        let mut names: Vec<String> = symbols
            .iter()
            .filter(|symbol| entrypoints.contains(&symbol.range))
            .map(|symbol| match &symbol.container {
                Some(container) => format!("{container}.{}", symbol.name),
                None => symbol.name.clone(),
            })
            .collect();
        names.sort();
        names
    }

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        names.sort();
        names
    }

    /// pytest の既定の規約で集めるクラス・メソッド・関数と、名前で呼ぶ準備・後始末。
    /// `Test` で始まらないクラス、集めないクラスの中の入れ子、`test` で始まらないメソッド、
    /// pytest 8 で外れた nose 形式の `setup` / `teardown` は集めない。
    #[test]
    fn default_conventions_collect_tests_and_hooks() {
        let source = r#"import sys


class TestAdd:
    def test_small(self):
        pass

    @staticmethod
    def test_static():
        pass

    @classmethod
    def test_class(cls):
        pass

    async def test_async(self):
        pass

    def testnounderscore(self):
        pass

    test_lambda = lambda self: None

    def setup_method(self, method):
        pass

    def teardown_method(self, method):
        pass

    @classmethod
    def setup_class(cls):
        pass

    @classmethod
    def teardown_class(cls):
        pass

    def pytest_generate_tests(self, metafunc):
        pass

    def helper(self):
        pass

    def setup(self):
        pass

    class TestNested:
        def test_nested(self):
            pass

    class NestedHelper:
        def test_in_helper(self):
            pass


class Test:
    def test_bare(self):
        pass


class Testing:
    def test_ing(self):
        pass


if sys.platform:
    class TestConditional:
        def test_conditional(self):
            pass


class Helper:
    def test_not_collected(self):
        pass


def test():
    pass


def testplain():
    pass


test_top_lambda = lambda: None


def setup_module(module):
    pass


def teardown_module(module):
    pass


def setUpModule():
    pass


def tearDownModule():
    pass


def setup_function(function):
    pass


def teardown_function(function):
    pass


def pytest_generate_tests(metafunc):
    pass


def setup():
    pass


def helper():
    pass
"#;
        assert_eq!(
            live(source),
            sorted(&[
                "TestAdd",
                "TestAdd.test_small",
                "TestAdd.test_static",
                "TestAdd.test_class",
                "TestAdd.test_async",
                "TestAdd.testnounderscore",
                "TestAdd.test_lambda",
                "TestAdd.setup_method",
                "TestAdd.teardown_method",
                "TestAdd.setup_class",
                "TestAdd.teardown_class",
                "TestAdd.pytest_generate_tests",
                "TestNested",
                "TestNested.test_nested",
                "Test",
                "Test.test_bare",
                "Testing",
                "Testing.test_ing",
                "TestConditional",
                "TestConditional.test_conditional",
                "test",
                "testplain",
                "test_top_lambda",
                "setup_module",
                "teardown_module",
                "setUpModule",
                "tearDownModule",
                "setup_function",
                "teardown_function",
                "pytest_generate_tests",
            ])
        );
    }

    /// pytest がクラスを飛ばすと言うのは、`object` 以外の基底・metaclass・デコレータを持たず、
    /// 本体の直下にデコレータの無い `def __init__` / `def __new__` を書いたときだけ。
    /// 基底は `__init_subclass__` で、metaclass やデコレータは生成時に `__init__` を差し替え
    /// うるので、集めるとみなす (同じファイルの基底から `__init__` を継承する場合も含む)。
    #[test]
    fn only_plain_constructor_definitions_skip_a_class() {
        let source = r#"class TestOwnInit:
    def __init__(self):
        pass

    def test_a(self):
        pass


class TestOwnNew:
    def __new__(cls):
        return super().__new__(cls)

    def test_b(self):
        pass


class TestObjectInit(object):
    def __init__(self):
        pass

    def test_c(self):
        pass


class TestOuterInit:
    def __init__(self):
        pass

    class TestInsideInit:
        def test_d(self):
            pass


class BaseWithInit:
    def __init__(self):
        pass


class TestInheritsInit(BaseWithInit):
    def test_e(self):
        pass


class Reset:
    def __init_subclass__(cls):
        cls.__init__ = object.__init__


class TestResetBySubclassHook(Reset):
    def __init__(self):
        pass

    def test_f(self):
        pass


class TestConditionalInit:
    if True:
        def __init__(self):
            pass

    def test_g(self):
        pass


class TestDecoratedInit:
    @some_decorator
    def __init__(self):
        pass

    def test_h(self):
        pass


@decorate
class TestDecoratedClass:
    def __init__(self):
        pass

    def test_i(self):
        pass


class TestMetaclass(metaclass=Meta):
    def __init__(self):
        pass

    def test_j(self):
        pass


class TestImportedBase(ImportedBase):
    def test_k(self):
        pass
"#;
        assert_eq!(
            live(source),
            sorted(&[
                "TestInheritsInit",
                "TestInheritsInit.test_e",
                "TestResetBySubclassHook",
                "TestResetBySubclassHook.test_f",
                "TestConditionalInit",
                "TestConditionalInit.test_g",
                "TestDecoratedInit",
                "TestDecoratedInit.test_h",
                "TestDecoratedClass",
                "TestDecoratedClass.test_i",
                "TestMetaclass",
                "TestMetaclass.test_j",
                "TestImportedBase",
                "TestImportedBase.test_k",
            ])
        );
    }

    /// クラスの外から `X.__init__` に触れる形があれば、どのクラスも飛ばすと言わない。
    /// `object` が束縛し直されうるなら、`(object)` の基底も素の基底とみなさない。
    #[test]
    fn rewritten_constructors_and_rebound_object_keep_classes_collected() {
        let rewritten = "class TestRewritten:\n    def __init__(self):\n        pass\n\n    def test_a(self):\n        pass\n\n\nTestRewritten.__init__ = object.__init__\n";
        assert_eq!(
            live(rewritten),
            sorted(&["TestRewritten", "TestRewritten.test_a"])
        );
        let assigned = "class Base:\n    def __init__(self):\n        pass\n\n\nclass TestReset(Base):\n    __init__ = object.__init__\n\n    def test_b(self):\n        pass\n";
        assert_eq!(live(assigned), sorted(&["TestReset", "TestReset.test_b"]));
        let rebound = "from compat import *\n\n\nclass TestObject(object):\n    def __init__(self):\n        pass\n\n    def test_c(self):\n        pass\n";
        assert_eq!(live(rebound), sorted(&["TestObject", "TestObject.test_c"]));
    }

    /// 集めたクラスの基底 (同じファイルのクラスと単純な別名) の `test*`・準備と後始末・
    /// 入れ子の `Test*` クラスも集める。`__test__ = True` のクラス (継承も含む) と、`Test*` の
    /// 名前へ束縛したクラスは、クラス名が `Test` で始まらなくても集める。集めないクラスの中の
    /// クラスは、集めるクラスと同じ名前でも集めない。
    #[test]
    fn bases_aliases_and_markers_are_collected() {
        let source = r#"class Mixin:
    def test_inherited(self):
        pass

    def setup_method(self, method):
        pass

    def helper(self):
        pass

    class TestFromMixin:
        def test_nested_in_mixin(self):
            pass


class TestConcrete(Mixin):
    pass


class AliasedMixin:
    def test_via_alias(self):
        pass


Alias = AliasedMixin


class TestViaAlias(Alias):
    pass


class Plain:
    def test_bound(self):
        pass


TestBound = Plain


class Marked:
    __test__ = True

    def test_marked(self):
        pass


class MarkedChild(Marked):
    def test_child(self):
        pass


class TestSame:
    def test_live(self):
        pass


class Holder:
    class TestSame:
        def test_never(self):
            pass


class Unused:
    def test_unused(self):
        pass
"#;
        assert_eq!(
            live(source),
            sorted(&[
                "Mixin.test_inherited",
                "Mixin.setup_method",
                "TestFromMixin",
                "TestFromMixin.test_nested_in_mixin",
                "TestConcrete",
                "AliasedMixin.test_via_alias",
                "TestViaAlias",
                "Plain",
                "Plain.test_bound",
                "Marked",
                "Marked.test_marked",
                "MarkedChild",
                "MarkedChild.test_child",
                "TestSame",
                "TestSame.test_live",
            ])
        );
    }

    /// 位置はシンボルの抽出と同じ構文木の座標 (行, バイト列) で引く。BOM・CRLF・タブ・
    /// 1 行に並べた lambda の代入・デコレータ付きのクラスでも、定義ごとに引ける。
    #[test]
    fn positions_match_symbols_across_line_endings_and_indentation() {
        let source = "\u{feff}import pytest\r\n\r\n\r\n@pytest.mark.slow\r\nclass TestTabs:\r\n\tdef test_tab(self):\r\n\t\tpass\r\n\r\n\ttest_a = lambda self: 1; test_b = lambda self: 2\r\n\r\n\tdef helper(self):\r\n\t\tpass\r\n\r\n\r\ndef test_crlf():\r\n\tpass\r\n";
        assert_eq!(
            live(source),
            sorted(&[
                "TestTabs",
                "TestTabs.test_tab",
                "TestTabs.test_a",
                "TestTabs.test_b",
                "test_crlf",
            ])
        );
    }
}
