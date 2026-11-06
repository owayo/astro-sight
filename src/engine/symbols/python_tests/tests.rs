use std::collections::HashSet;

use super::*;
use crate::engine::symbols::extract_symbols;
use crate::language::LangId;

fn parse(source: &str) -> tree_sitter::Tree {
    let language = LangId::Python.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    parser.parse(source, None).unwrap()
}

/// `source` の Python のシンボルのうち、ファイル単位の判定で集める (呼ぶ) ものの名前
/// (メソッドは `クラス.名前`) を並べて返す。
fn live_in(source: &str, pytest_module: bool) -> Vec<String> {
    let tree = parse(source);
    let root = tree.root_node();
    let symbols = extract_symbols(root, source.as_bytes(), LangId::Python).unwrap();
    let facts = PythonModuleFacts::collect(root, source.as_bytes());
    let entrypoints = PythonTestEntrypoints::collect(&facts, pytest_module);
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

/// pytest のテストモジュールでの [`live_in`]。
fn live(source: &str) -> Vec<String> {
    live_in(source, true)
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

/// Issue #259: unittest は `TestCase` 派生クラスの MRO にある基底の `test*` メソッドも集め、
/// `setUp` などを MRO で呼ぶ。同じファイルの mixin と基底の連鎖、`import unittest as ut` と
/// `from unittest import TestCase as Base` の別名、派生の派生も、ファイル名がテストモジュール
/// でなくても集める (`addCleanup` は `enterContext` が呼ぶ)。TestCase の派生に継承されない
/// mixin、`test` で始まらず暗黙にも呼ばないメソッド、unittest は呼ばない pytest の準備・
/// 後始末、unittest が集めない入れ子のクラス、基底のクラスそのものは集めない。
#[test]
fn unittest_bases_in_the_same_file_are_collected() {
    let source = r#"import unittest
import unittest as ut
from unittest import TestCase as Base


class SharedMixin:
    def test_shared(self):
        pass

    def setUp(self):
        pass

    def tearDown(self):
        pass

    @classmethod
    def setUpClass(cls):
        pass

    def runTest(self):
        pass

    def doCleanups(self):
        return True

    def addCleanup(self, function):
        pass

    def helper(self):
        pass

    def setup_method(self, method):
        pass

    class TestNestedInMixin:
        def test_nested(self):
            pass


class CalcTests(SharedMixin, unittest.TestCase):
    def test_own(self):
        pass


class Deeper:
    def test_deeper(self):
        pass


class Middle(Deeper):
    def test_middle(self):
        pass


class ChainTests(Middle, ut.TestCase):
    pass


class AliasTests(Base):
    def testCamel(self):
        pass


class Derived(CalcTests):
    def test_derived(self):
        pass


class Unrelated:
    def test_unrelated(self):
        pass


class NotATest(Unrelated):
    pass
"#;
    let expected = sorted(&[
        "SharedMixin.test_shared",
        "SharedMixin.setUp",
        "SharedMixin.tearDown",
        "SharedMixin.setUpClass",
        "SharedMixin.runTest",
        "SharedMixin.doCleanups",
        "SharedMixin.addCleanup",
        "CalcTests",
        "CalcTests.test_own",
        "Deeper.test_deeper",
        "Middle.test_middle",
        "ChainTests",
        "AliasTests",
        "AliasTests.testCamel",
        "Derived",
        "Derived.test_derived",
    ]);
    assert_eq!(live_in(source, false), expected);
    // pytest のテストモジュールでも同じ (どのクラスも `Test` で始まらない)。
    assert_eq!(live_in(source, true), expected);
}

/// ファイル単位の判定は、import した名前の先や、先頭が同じファイルのクラスでない属性の
/// 末尾を推測しない。`helpers.Box.Base` を同じファイルの `Base` と取り違えると、実行されない
/// `Base.test_unused` を生存にしてしまう (別ファイルの解決はプロジェクト単位の判定が行う)。
/// 先頭が同じファイルのクラスなら、属性をその本体の束縛でたどる。
#[test]
fn single_module_does_not_guess_attribute_tails() {
    let source = r#"from helpers import Box


class Base:
    def test_unused(self):
        pass


class TestCross(Box.Base):
    pass


class Local:
    class Inner:
        def test_inner(self):
            pass

    Alias = Base


class TestNested(Local.Inner):
    pass
"#;
    assert_eq!(
        live(source),
        sorted(&["TestCross", "TestNested", "Inner.test_inner"])
    );
}

/// `files` (パス, ソース) をプロジェクトのモジュールにし、判定の (入口, 不確か) を
/// `パス:qualname` の並びで返す。ファイル名が `test_` で始まるものを pytest のテスト
/// モジュールにする。`unobserved` は中身を見られなかったモジュールの (パス, pytest のテスト
/// モジュールか)。
fn project_with(
    files: &[(&str, &str)],
    unobserved: &[(&str, bool)],
) -> (Vec<String>, Vec<String>, bool, bool) {
    let modules = files
        .iter()
        .map(|(path, source)| {
            let tree = parse(source);
            let file_name = path.rsplit('/').next().unwrap_or(path);
            ProjectModule {
                path: path.to_string(),
                facts: PythonModuleFacts::collect(tree.root_node(), source.as_bytes()),
                pytest_module: file_name.starts_with("test_"),
            }
        })
        .collect();
    let unobserved = unobserved
        .iter()
        .map(|(path, pytest_module)| (path.to_string(), *pytest_module))
        .collect();
    let project = ProjectTests::new(modules, unobserved);
    let collected = project.collect();
    let names = |definitions: &HashSet<(usize, (usize, usize))>| {
        let mut names: Vec<String> = definitions
            .iter()
            .filter_map(|&(module, start)| {
                let module = &project.modules()[module];
                module
                    .facts
                    .definition_name(start)
                    .map(|name| format!("{}:{name}", module.path))
            })
            .collect();
        names.sort();
        names
    };
    (
        names(&collected.live),
        names(&collected.uncertain),
        collected.any_member_uncertain,
        collected.any_class_uncertain,
    )
}

fn project(files: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
    let (live, uncertain, any_member, any_class) = project_with(files, &[]);
    assert!(!any_member && !any_class, "{live:?} {uncertain:?}");
    (live, uncertain)
}

/// Issue #261: 別ファイルの基底、テストモジュールへ import した `Test*` のクラス (別名と
/// `import *` を含む)、再 export の別名、相対 import、属性の基底、別ファイルの TestCase の
/// 連鎖と mixin、`tests/` (pytest が basedir を `sys.path` に入れる) の補助モジュールをたどる。
/// `Test*` でない名前に束縛したクラス、モジュールの別名、`__init__` を持つクラス、
/// 別のモジュールの同じ名前のクラス、どのテストクラスも継承しないクラス、`__all__` で
/// 外した名前、`_` で始まる名前は集めない。
#[test]
fn project_follows_bases_and_bindings_across_files() {
    let (live, uncertain) = project(&[
        (
            "shared_checks.py",
            "class SharedChecks:\n    def test_from_base(self):\n        pass\n\n    def helper(self):\n        pass\n\n\nclass TestExported:\n    def test_exported(self):\n        pass\n\n\nclass Renamed:\n    def test_renamed(self):\n        pass\n\n\nclass TestHidden:\n    def test_hidden(self):\n        pass\n\n\nclass TestWithInit:\n    def __init__(self):\n        pass\n\n    def test_init(self):\n        pass\n",
        ),
        (
            "other/shared_checks.py",
            "class SharedChecks:\n    def test_other(self):\n        pass\n",
        ),
        (
            "base_tests.py",
            "import unittest\n\n\nclass BaseTests(unittest.TestCase):\n    def test_base(self):\n        pass\n",
        ),
        (
            "mixins.py",
            "class CrossMixin:\n    def test_mixin(self):\n        pass\n\n    def setUp(self):\n        pass\n\n    def setup_method(self, method):\n        pass\n",
        ),
        (
            "library.py",
            "class Library:\n    def test_library(self):\n        pass\n",
        ),
        (
            "star_source.py",
            "class TestStarred:\n    def test_starred(self):\n        pass\n\n\nclass Starred:\n    def test_not_test_name(self):\n        pass\n\n\nclass _TestPrivate:\n    def test_private(self):\n        pass\n",
        ),
        (
            "facade.py",
            "from checks_all import TestUnexported, TestExportedByAll\n\n__all__ = [\"TestExportedByAll\"]\n",
        ),
        (
            "checks_all.py",
            "class TestUnexported:\n    def test_unexported(self):\n        pass\n\n\nclass TestExportedByAll:\n    def test_exported_by_all(self):\n        pass\n",
        ),
        (
            "test_star.py",
            "from star_source import *\nfrom facade import *\n",
        ),
        ("pkg/__init__.py", "from .impl import Checks as Shared\n"),
        (
            "pkg/impl.py",
            "class Checks:\n    def test_reexported(self):\n        pass\n",
        ),
        (
            "pkg/test_relative.py",
            "from . import Shared\nfrom .impl import Checks as TestAgain\n\n\nclass TestRelative(Shared):\n    pass\n",
        ),
        (
            "tests/helper_mod.py",
            "class InTestsDir:\n    def test_in_tests_dir(self):\n        pass\n",
        ),
        (
            "tests/test_tests_dir.py",
            "from helper_mod import InTestsDir\n\n\nclass TestUsesDir(InTestsDir):\n    pass\n",
        ),
        (
            "test_cross.py",
            "import unittest\n\nimport library as TestLibrary\nimport shared_checks\nfrom base_tests import BaseTests\nfrom mixins import CrossMixin\nfrom shared_checks import SharedChecks, TestExported, TestWithInit\nfrom shared_checks import Renamed as TestRenamed\nfrom shared_checks import TestHidden as Hidden\n\n\nclass TestCross(SharedChecks):\n    pass\n\n\nclass TestAttribute(shared_checks.SharedChecks):\n    pass\n\n\nclass MyTests(BaseTests):\n    def test_mine(self):\n        pass\n\n\nclass MixedTests(CrossMixin, unittest.TestCase):\n    pass\n",
        ),
    ]);
    assert_eq!(
        live,
        sorted(&[
            "base_tests.py:BaseTests",
            "base_tests.py:BaseTests.test_base",
            "checks_all.py:TestExportedByAll",
            "checks_all.py:TestExportedByAll.test_exported_by_all",
            "mixins.py:CrossMixin.setUp",
            "mixins.py:CrossMixin.test_mixin",
            "pkg/impl.py:Checks",
            "pkg/impl.py:Checks.test_reexported",
            "pkg/test_relative.py:TestRelative",
            "shared_checks.py:Renamed",
            "shared_checks.py:Renamed.test_renamed",
            "shared_checks.py:SharedChecks.test_from_base",
            "shared_checks.py:TestExported",
            "shared_checks.py:TestExported.test_exported",
            "star_source.py:TestStarred",
            "star_source.py:TestStarred.test_starred",
            "test_cross.py:MixedTests",
            "test_cross.py:MyTests",
            "test_cross.py:MyTests.test_mine",
            "test_cross.py:TestAttribute",
            "test_cross.py:TestCross",
            "tests/helper_mod.py:InTestsDir.test_in_tests_dir",
            "tests/test_tests_dir.py:TestUsesDir",
        ])
    );
    assert_eq!(uncertain, Vec::<String>::new());
}

/// TestCase の派生か分からない (第三者のモジュールの基底を持つ) クラスは、自身と基底の
/// `test*` と unittest が暗黙に呼ぶメソッドを不確かにする。クラスの定義は、テストを探す
/// モジュールにあるか `test*` を持つときだけ不確かにする。確かめられた経路で TestCase の root
/// に届く基底があれば、ほかの基底が分からなくても TestCase とする。
#[test]
fn project_unknown_bases_make_test_members_uncertain() {
    let (live, uncertain) = project(&[
        (
            "shared_checks.py",
            "class SharedChecks:\n    def test_from_external_chain(self):\n        pass\n\n    def helper(self):\n        pass\n",
        ),
        (
            "test_api.py",
            "import unittest\n\nfrom rest_framework.test import APITestCase\nfrom shared_checks import SharedChecks\n\n\nclass UserAPITests(APITestCase):\n    def test_list_users(self):\n        pass\n\n    def setUp(self):\n        pass\n\n\nclass Checks(SharedChecks, APITestCase):\n    pass\n\n\nclass InheritsOnly(APITestCase):\n    pass\n\n\nclass Known(APITestCase, unittest.TestCase):\n    def test_known(self):\n        pass\n",
        ),
        (
            "models.py",
            "from pydantic import BaseModel\n\n\nclass Model(BaseModel):\n    def save(self):\n        pass\n",
        ),
    ]);
    assert_eq!(
        live,
        sorted(&["test_api.py:Known", "test_api.py:Known.test_known"])
    );
    assert_eq!(
        uncertain,
        sorted(&[
            "shared_checks.py:SharedChecks.test_from_external_chain",
            "test_api.py:Checks",
            "test_api.py:InheritsOnly",
            "test_api.py:UserAPITests",
            "test_api.py:UserAPITests.setUp",
            "test_api.py:UserAPITests.test_list_users",
        ])
    );
}

/// 推測でしか解決できない基底は「集める」と言わない。クラスの属性 (`Box.Base`) は本体の束縛で
/// たどり、同じ名前の別のクラスと取り違えない。絶対 import の先がプロジェクトで経路の末尾
/// だけが一致するファイルなら、実際の import 先は外かもしれないので不確かにする。外から継承する
/// `__test__ = True` も分からないものとして、pytest の準備・後始末を不確かにする。
#[test]
fn project_guessed_resolution_is_uncertain() {
    let (live, uncertain) = project(&[
        (
            "helpers.py",
            "class RealBase:\n    def test_real(self):\n        pass\n\n\nclass Box:\n    Base = RealBase\n",
        ),
        (
            "test_box.py",
            "from helpers import Box\n\n\nclass Base:\n    def test_unused(self):\n        pass\n\n\nclass TestBox(Box.Base):\n    pass\n",
        ),
        ("unrelated_pkg/vendor_case.py", "class Base:\n    pass\n"),
        (
            "test_vendor.py",
            "from vendor_case import Base\nfrom vendor_marker import Marked\n\n\nclass Checks(Base):\n    def test_local(self):\n        pass\n\n\nclass MarkedChecks(Marked):\n    def setup_method(self, method):\n        pass\n\n    def test_marked_local(self):\n        pass\n",
        ),
    ]);
    assert_eq!(
        live,
        sorted(&["helpers.py:RealBase.test_real", "test_box.py:TestBox"])
    );
    assert_eq!(
        uncertain,
        sorted(&[
            "test_vendor.py:Checks",
            "test_vendor.py:Checks.test_local",
            "test_vendor.py:MarkedChecks",
            "test_vendor.py:MarkedChecks.setup_method",
            "test_vendor.py:MarkedChecks.test_marked_local",
        ])
    );
}

/// 静的に見えない束縛 (module の `__getattr__`、呼び出しの式の基底、中身を見られない
/// モジュール) をテストの入口から通るときは、どのクラスのメンバーも集めうるとする。
/// テストモジュールの中身を見られないときは、どのクラスも集めうるとする。
#[test]
fn project_dynamic_bindings_hold_every_candidate() {
    let getattr = [
        (
            "checks.py",
            "class RealChecks:\n    def test_hidden(self):\n        pass\n",
        ),
        (
            "facade.py",
            "from checks import RealChecks\n\n\ndef __getattr__(name):\n    return RealChecks\n",
        ),
        (
            "test_facade.py",
            "from facade import Base\n\n\nclass TestFacade(Base):\n    pass\n",
        ),
    ];
    let (live, _, any_member, any_class) = project_with(&getattr, &[]);
    assert_eq!(live, sorted(&["test_facade.py:TestFacade"]));
    assert!(any_member && !any_class);

    let call = [(
        "test_call.py",
        "def make_base():\n    return object\n\n\nclass TestCall(make_base()):\n    pass\n",
    )];
    let (_, _, any_member, any_class) = project_with(&call, &[]);
    assert!(any_member && !any_class);

    let bridge = [
        (
            "checks.py",
            "class Checks:\n    __test__ = True\n\n    def test_active(self):\n        pass\n",
        ),
        ("test_exports.py", "from bridge import *\n"),
    ];
    let (_, _, any_member, any_class) = project_with(&bridge, &[("bridge.py", false)]);
    assert!(any_member && any_class);

    let (_, _, any_member, any_class) = project_with(&[], &[("test_big.py", true)]);
    assert!(any_member && any_class);
    let (_, _, any_member, any_class) = project_with(&[], &[("big.py", false)]);
    assert!(any_member && !any_class);
}

/// 互いに import し合う再 export の循環や、自身を基底に書いた別名でも止まる。
#[test]
fn project_resolution_terminates_on_cycles() {
    let (live, _) = project(&[
        ("a.py", "from b import Shared\n"),
        ("b.py", "from a import Shared\n"),
        (
            "test_cycle.py",
            "from a import Shared\n\nLoop = Loop\nBox = Box.Inner\n\n\nclass TestCycle(Shared, Loop, Box.Inner):\n    pass\n",
        ),
    ]);
    assert_eq!(live, sorted(&["test_cycle.py:TestCycle"]));
}

/// 呼び出しの結果を代入した別名 (`Alias = choose()`) と、`__class_getitem__` を持つクラスの
/// subscript (`Selector[int]`) は、どのクラスでもありうる。テストの入口から通ると、どの
/// クラスのメンバーも集めうるとする。`__class_getitem__` が無ければ subscript は値のクラスを
/// 継承する (`Generic[T]` と同じ)。
#[test]
fn project_call_results_and_custom_subscripts_hold_members() {
    let alias = [(
        "test_alias.py",
        "class Actual:\n    def test_active(self):\n        pass\n\n\ndef choose():\n    return Actual\n\n\nAlias = choose()\n\n\nclass TestSelected(Alias):\n    pass\n",
    )];
    let (live, _, any_member, any_class) = project_with(&alias, &[]);
    assert_eq!(live, sorted(&["test_alias.py:TestSelected"]));
    assert!(any_member && !any_class);

    let custom = [(
        "test_index.py",
        "class Actual:\n    def test_active(self):\n        pass\n\n\nclass Selector:\n    def __class_getitem__(cls, item):\n        return Actual\n\n\nclass TestSelected(Selector[int]):\n    pass\n",
    )];
    let (live, _, any_member, any_class) = project_with(&custom, &[]);
    assert_eq!(live, sorted(&["test_index.py:TestSelected"]));
    assert!(any_member && !any_class);

    let plain = [(
        "test_generic.py",
        "from typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\n\n\nclass Checks(Generic[T]):\n    def test_generic(self):\n        pass\n\n\nclass TestTyped(Checks[int]):\n    pass\n",
    )];
    let (live, _) = project(&plain);
    assert_eq!(
        live,
        sorted(&[
            "test_generic.py:Checks.test_generic",
            "test_generic.py:TestTyped"
        ])
    );
}

/// pytest は集めたクラスの名前空間の束縛 (本体の import と、`Test*` でない名前の
/// `__test__ = True` のクラス) も同じ規則で集める。
#[test]
fn project_class_namespace_imports_and_marked_aliases_are_collected() {
    let (live, uncertain) = project(&[
        (
            "helpers.py",
            "class Checks:\n    def test_imported(self):\n        pass\n\n\nclass Marked:\n    __test__ = True\n\n    def test_marked(self):\n        pass\n\n\nclass Plain:\n    def test_plain_never(self):\n        pass\n",
        ),
        (
            "test_outer.py",
            "import helpers\n\n\nclass TestOuter:\n    from helpers import Checks as TestInner\n\n    Alias = helpers.Marked\n    Other = helpers.Plain\n",
        ),
    ]);
    assert_eq!(
        live,
        sorted(&[
            "helpers.py:Checks",
            "helpers.py:Checks.test_imported",
            "helpers.py:Marked",
            "helpers.py:Marked.test_marked",
            "test_outer.py:TestOuter",
        ])
    );
    assert_eq!(uncertain, Vec::<String>::new());
}

/// `from facade import *` は facade の `__all__` の名前を `getattr` で引くので、静的に束縛して
/// いない `__all__` の名前は module の `__getattr__` が返しうる (`Test*` の名前なら、どのクラスの
/// メンバーも集めうる。クラスの定義は `__getattr__` の中で名前を書いて参照するので控えない)。
/// `__all__` が無ければ `import *` は名前空間の名前だけを入れ、`__getattr__` を呼ばない。
#[test]
fn project_star_exports_through_all_may_come_from_getattr() {
    let checks = (
        "checks.py",
        "class Checks:\n    def test_active(self):\n        pass\n",
    );
    let published = [
        checks,
        (
            "facade.py",
            "from checks import Checks\n\n__all__ = [\"TestPublished\"]\n\n\ndef __getattr__(name):\n    return Checks\n",
        ),
        ("test_exports.py", "from facade import *\n"),
    ];
    let (_, _, any_member, any_class) = project_with(&published, &[]);
    assert!(any_member && !any_class);

    let without_all = [
        checks,
        (
            "facade.py",
            "from checks import Checks\n\n\ndef __getattr__(name):\n    return Checks\n",
        ),
        ("test_exports.py", "from facade import *\n"),
    ];
    let (live, uncertain) = project(&without_all);
    assert_eq!(live, Vec::<String>::new());
    assert_eq!(uncertain, Vec::<String>::new());
}

/// 同じ名前に unittest の TestCase と第三者のクラスの両方を束縛すると、TestCase の派生と
/// 言える一方で、`__test__ = True` を継承するかは分からない (pytest の準備・後始末を不確かに
/// する)。`TestCase` という名前で import したプロジェクトのクラスは、書き方だけで TestCase と
/// みなさない (ファイル単位の判定も同じで、プロジェクト単位で訂正できない除外をしない)。
#[test]
fn project_unittest_root_bindings_keep_other_bindings_incomplete() {
    let (live, uncertain) = project(&[
        (
            "test_checks.py",
            "from unittest import TestCase as Parent\nfrom external_lib import Parent\n\n\nclass Checks(Parent):\n    def setup_method(self, method):\n        pass\n\n    def test_active(self):\n        pass\n",
        ),
        ("helper.py", "class Base:\n    pass\n"),
        (
            "ordinary.py",
            "from helper import Base as TestCase\n\n\nclass Consumer(TestCase):\n    def test_unused(self):\n        pass\n",
        ),
    ]);
    assert_eq!(
        live,
        sorted(&["test_checks.py:Checks", "test_checks.py:Checks.test_active"])
    );
    assert_eq!(uncertain, sorted(&["test_checks.py:Checks.setup_method"]));

    let ordinary = "from helper import Base as TestCase\n\n\nclass Consumer(TestCase):\n    def test_unused(self):\n        pass\n";
    assert_eq!(live_in(ordinary, false), Vec::<String>::new());
}

/// 同じ名前を条件で 2 つのクラスの属性に束縛しても、どちらも確かめられた基底とする (属性の
/// 先頭を、別の経路で引いた訪問済みで落とさない)。
#[test]
fn project_attribute_heads_are_resolved_on_every_path() {
    let source = "class Box:\n    class Active:\n        def test_active(self):\n            pass\n\n    class Decoy:\n        def test_decoy(self):\n            pass\n\n\nSelected = Box.Active\nif False:\n    Selected = Box.Decoy\n\n\nclass TestSelected(Selected):\n    pass\n";
    let (live, uncertain) = project(&[("test_selected.py", source)]);
    assert_eq!(
        live,
        sorted(&[
            "test_selected.py:Active.test_active",
            "test_selected.py:Decoy.test_decoy",
            "test_selected.py:TestSelected",
        ])
    );
    assert_eq!(uncertain, Vec::<String>::new());
    assert_eq!(
        live_in(source, true),
        sorted(&["Active.test_active", "Decoy.test_decoy", "TestSelected"])
    );
}

/// 静的な束縛も `import *` も無い名前 (`object` や `Exception` などの組み込み) は、解決できない
/// 基底にしない (テストを探すモジュールのふつうのクラスを不確かにしない)。アンパックの代入の
/// 名前は右辺の表示の要素に束縛する。`with ... as` で束縛した名前は何でもありうるので、それを
/// 継承するクラスから、どのクラスのメンバーも集めうるとする。
#[test]
fn project_builtins_resolve_and_dynamic_bindings_hold() {
    let (live, uncertain) = project(&[(
        "test_plain.py",
        "class Plain(object):\n    def helper(self):\n        pass\n\n\nclass Failure(ValueError):\n    pass\n\n\nA, B = object, object\n\n\nclass Unpacked(A):\n    def test_unpacked(self):\n        pass\n",
    )]);
    assert_eq!(live, Vec::<String>::new());
    assert_eq!(uncertain, Vec::<String>::new());

    let (live, uncertain, any_member, any_class) = project_with(
        &[(
            "test_with.py",
            "with open(\"data\") as Opened:\n    pass\n\n\nclass FromWith(Opened):\n    def test_with(self):\n        pass\n",
        )],
        &[],
    );
    assert_eq!(live, Vec::<String>::new());
    assert_eq!(
        uncertain,
        sorted(&["test_with.py:FromWith", "test_with.py:FromWith.test_with"])
    );
    assert!(any_member && !any_class);
}

/// プロジェクトの外の関数の呼び出しの結果 (`declarative_base()`) は、プロジェクトの外のもの
/// (か引数のクラスを基底にしたもの) とし、どのクラスでもありうるとはしない。TestCase の派生か
/// 分からないクラスは不確かにし、確かな入口から継承すれば集める。
#[test]
fn project_external_call_results_are_incomplete() {
    let models = (
        "models.py",
        "from sqlalchemy.orm import declarative_base\n\nBase = declarative_base()\n\n\nclass User(Base):\n    def test_like_name(self):\n        pass\n",
    );
    let mixin = (
        "mixins.py",
        "class Mixin:\n    def test_mixin_never(self):\n        pass\n",
    );
    let (_, uncertain, any_member, any_class) = project_with(&[models, mixin], &[]);
    assert_eq!(
        uncertain,
        sorted(&["models.py:User", "models.py:User.test_like_name"])
    );
    assert!(!any_member && !any_class);

    let entry = (
        "test_user.py",
        "from models import User\n\n\nclass TestUser(User):\n    pass\n",
    );
    let (live, _) = project(&[models, mixin, entry]);
    assert_eq!(
        live,
        sorted(&["models.py:User.test_like_name", "test_user.py:TestUser"])
    );
}

/// アンパックの代入・連鎖した代入・表示を反復する `for`・代入式・条件式・`or` は、右辺の
/// どれかに束縛する (どれも集める)。どこにも束縛しないクラスと、数のリテラルを束縛した `Test*`
/// の名前は集めない。
#[test]
fn project_binding_forms_resolve_to_their_values() {
    let source = "class First:\n    def test_first(self):\n        pass\n\n\nclass Second:\n    def test_second(self):\n        pass\n\n\nclass Third:\n    def test_third(self):\n        pass\n\n\nclass Fourth:\n    def test_fourth(self):\n        pass\n\n\nclass Fifth:\n    def test_fifth(self):\n        pass\n\n\nclass Sixth:\n    def test_sixth(self):\n        pass\n\n\nclass Never:\n    def test_never(self):\n        pass\n\n\nA, (B, _) = First, (Second, 1)\nC = D = Third\nfor E in [Fourth]:\n    pass\nif (F := Fifth):\n    pass\nG = None or (Sixth if F else None)\nTestCount = 1\n\n\nclass TestA(A):\n    pass\n\n\nclass TestB(B):\n    pass\n\n\nclass TestD(D):\n    pass\n\n\nclass TestE(E):\n    pass\n\n\nclass TestF(F):\n    pass\n\n\nclass TestG(G):\n    pass\n";
    let (live, uncertain) = project(&[("test_forms.py", source)]);
    assert_eq!(
        live,
        sorted(&[
            "test_forms.py:Fifth.test_fifth",
            "test_forms.py:First.test_first",
            "test_forms.py:Fourth.test_fourth",
            "test_forms.py:Second.test_second",
            "test_forms.py:Sixth.test_sixth",
            "test_forms.py:TestA",
            "test_forms.py:TestB",
            "test_forms.py:TestD",
            "test_forms.py:TestE",
            "test_forms.py:TestF",
            "test_forms.py:TestG",
            "test_forms.py:Third.test_third",
        ])
    );
    assert_eq!(uncertain, Vec::<String>::new());
}

/// codex の指摘から: クラスでないプロジェクトの値 (入れ物・モジュールの変数) の要素・属性・
/// 呼び出しの結果を `Test*` の名前に束縛すると、どのクラスのメンバーも集めうる。クラスは
/// デコレータ・metaclass・`__init_subclass__`・`__set_name__`・`__subclasses__()` などで名前を
/// 書かずに取り出せるので、テストを持ちうるクラス (テストを探すモジュールにあるか、`test*` を
/// 持つ) の定義はどれも集めうるとする。テストを持たない、テストを探すモジュールの外のクラスは
/// 控えない。
#[test]
fn project_registries_hold_classes_that_may_hold_tests() {
    let local = "registry = []\n\n\ndef register(cls):\n    registry.append(cls)\n    return cls\n\n\n@register\nclass Checks:\n    def test_registry(self):\n        pass\n\n\nclass Plain:\n    def test_plain(self):\n        pass\n\n\nTestPicked = registry.pop()\n";
    let (live, uncertain, any_member, any_class) = project_with(&[("test_local.py", local)], &[]);
    assert_eq!(live, Vec::<String>::new());
    assert_eq!(
        uncertain,
        sorted(&[
            "test_local.py:Checks",
            "test_local.py:Checks.test_registry",
            "test_local.py:Plain",
            "test_local.py:Plain.test_plain",
        ])
    );
    assert!(any_member && !any_class);

    let helpers = "registry = {}\n\n\ndef register(name):\n    def wrap(cls):\n        registry[name] = cls\n        return cls\n\n    return wrap\n\n\nclass Base:\n    def __init_subclass__(cls):\n        registry[cls.__name__] = cls\n\n\n@register(\"x\")\nclass Registered:\n    def test_registered(self):\n        pass\n\n\nclass Child(Base):\n    def test_child(self):\n        pass\n";
    for entry in [
        "import helpers\n\nTestPicked = helpers.registry[\"x\"]\n",
        "from helpers import registry\n\nTestPicked = registry.get(\"x\")\n",
    ] {
        let (live, uncertain, any_member, any_class) =
            project_with(&[("helpers.py", helpers), ("test_entry.py", entry)], &[]);
        assert_eq!(live, Vec::<String>::new());
        assert_eq!(
            uncertain,
            sorted(&[
                "helpers.py:Child",
                "helpers.py:Child.test_child",
                "helpers.py:Registered",
                "helpers.py:Registered.test_registered",
            ]),
            "{entry}"
        );
        assert!(any_member && !any_class);
    }
}

/// codex の指摘から: 静的に見えない値を返しうる形は、どれも何でもありうるとする。独自の
/// `__new__` を持つクラスの呼び出し、クラスでない値の基底 (`__mro_entries__`)、属性の途中で
/// import したモジュール (の `__getattr__`)、演算子の多重定義、`type` 文の値の属性、
/// ふつうと分かっていないプロジェクトの外の基底を持つクラスの subscript。
#[test]
fn project_hidden_values_hold_members() {
    let checks = "class Checks:\n    def test_registry(self):\n        pass\n\n\n";
    let cases = [
        "class Factory:\n    def __new__(cls):\n        return Checks\n\n\nTestPicked = Factory()\n",
        "class Adapter:\n    def __mro_entries__(self, bases):\n        return (Checks,)\n\n\nclass TestPicked(Adapter()):\n    pass\n",
        "class Select:\n    def __or__(self, other):\n        return Checks\n\n\nTestPicked = Select() | 0\n",
        "type Alias = Checks\nTestPicked = Alias.__value__\n",
        "from ext import Parent\n\n\nclass Selector(Parent):\n    target = Checks\n\n\nTestPicked = Selector[0]\n",
    ];
    for case in cases {
        let source = format!("{checks}{case}");
        let (live, _, any_member, any_class) =
            project_with(&[("test_hidden.py", source.as_str())], &[]);
        assert!(
            !live.contains(&"test_hidden.py:Checks.test_registry".to_string()),
            "{case}"
        );
        assert!(any_member && !any_class, "{case}");
    }

    let attribute = [
        (
            "shared_checks.py",
            "class Checks:\n    def test_active(self):\n        pass\n\n\ndef __getattr__(name):\n    return Checks\n",
        ),
        ("library.py", "import shared_checks as ns\n"),
        (
            "test_example.py",
            "import library\n\n\nclass TestPicked(library.ns.Dynamic):\n    pass\n",
        ),
    ];
    let (live, _, any_member, any_class) = project_with(&attribute, &[]);
    assert_eq!(live, sorted(&["test_example.py:TestPicked"]));
    assert!(any_member && !any_class);

    // 属性の途中の import は、その先のモジュールで引く。
    let reexported = [
        (
            "shared_checks.py",
            "class Checks:\n    def test_active(self):\n        pass\n",
        ),
        ("library.py", "import shared_checks as ns\n"),
        (
            "test_example.py",
            "import library\n\n\nclass TestPicked(library.ns.Checks):\n    pass\n",
        ),
    ];
    let (live, _) = project(&reexported);
    assert_eq!(
        live,
        sorted(&[
            "shared_checks.py:Checks.test_active",
            "test_example.py:TestPicked"
        ])
    );
}

/// codex の指摘から: 辞書の反復とアンパックはキーを、`*` の展開を含む表示のアンパックは
/// 要素のどれかを束縛する。数や文字列だけのタプルは、引数に渡してもクラスを作らない
/// (`namedtuple("Point", fields)`)。リストは、空でなくても、あとでクラスを足すレジストリで
/// ありうる。
#[test]
fn project_mappings_starred_displays_and_scalar_containers() {
    let checks = "class Checks:\n    def test_registry(self):\n        pass\n\n\n";
    for case in [
        "TestPicked, other = {Checks: None, 1: None}\n",
        "TestPicked, a, b = [*[], Checks, *[0, 1]]\n",
    ] {
        let source = format!("{checks}{case}");
        let (live, _) = project(&[("test_unpack.py", source.as_str())]);
        assert!(
            live.contains(&"test_unpack.py:Checks.test_registry".to_string()),
            "{case}: {live:?}"
        );
    }

    // 標準ライブラリのクラスの作り方は、引数 (リストのフィールド名を含む) を基底にしない。
    for points in [
        "from collections import namedtuple\n\nfields = (\"x\", \"y\")\nPoint = namedtuple(\"Point\", fields)\n\n\nclass Position(Point):\n    pass\n",
        "from collections import namedtuple\n\nfields = [\"x\", \"y\"]\nPoint = namedtuple(\"Point\", fields)\n\n\nclass Position(Point):\n    pass\n",
        "import enum\n\nnames = [\"RED\"]\nColor = enum.Enum(\"Color\", names)\n\n\nclass Shade(Color):\n    pass\n",
    ] {
        let (_, _, any_member, any_class) = project_with(&[("points.py", points)], &[]);
        assert!(!any_member && !any_class, "{points}");
    }
    // ほかのプロジェクトの外の関数にリストを渡すと、その要素から何でも作りうる。
    let built = "import factories\n\nfields = [\"x\", \"y\"]\nPoint = factories.build(\"Point\", fields)\n\n\nclass Position(Point):\n    pass\n";
    let (_, _, any_member, _) = project_with(&[("points.py", built)], &[]);
    assert!(any_member);

    for registry in [
        "import random\n\nregistry = []\nTestPicked = random.choice(registry)\n",
        "registry = [\"reserved\"]\n\n\ndef register(cls):\n    registry.append(cls)\n    return cls\n\n\n@register\nclass Checks:\n    def test_registry(self):\n        pass\n\n\nTestPicked = registry.pop()\n",
    ] {
        let (_, uncertain, any_member, _) = project_with(&[("test_choice.py", registry)], &[]);
        assert!(any_member, "{registry}");
        if registry.contains("class Checks") {
            assert!(
                uncertain.contains(&"test_choice.py:Checks".to_string()),
                "{uncertain:?}"
            );
        }
    }
}

/// codex の指摘から: 代入で作ったモジュールの別名 (`ns = helpers`) の属性はそのモジュールで
/// 引き、`type` を継承するクラス (metaclass) の呼び出しはクラスを作りうる。ふつうのクラスの
/// 呼び出しはインスタンスで引数に依らず、`is` / `in` の比較は bool なので、どちらも何でも
/// ありうるとはしない。`test*` の別名や `Test*` の入れ子のクラスを持つクラスも、テストを
/// 持ちうる。
#[test]
fn project_module_aliases_constructors_and_test_bindings() {
    let helpers = (
        "helpers.py",
        "class Checks:\n    def test_dynamic(self):\n        pass\n\n\ndef __getattr__(name):\n    return Checks\n",
    );
    let entry = (
        "test_entry.py",
        "import helpers\n\nns = helpers\nTestPicked = ns.Dynamic\n",
    );
    let (_, _, any_member, any_class) = project_with(&[helpers, entry], &[]);
    assert!(any_member && !any_class);

    for made in [
        "class Mixin:\n    def test_mixin(self):\n        pass\n\n\nclass Meta(type):\n    pass\n\n\nTestMade = Meta(\"TestMade\", (Mixin,), {})\n",
        "class Mixin:\n    def test_mixin(self):\n        pass\n\n\nType = type\n\n\nclass Meta(Type):\n    pass\n\n\nTestMade = Meta(\"TestMade\", (Mixin,), {})\n",
    ] {
        let (_, uncertain, any_member, any_class) = project_with(&[("test_made.py", made)], &[]);
        assert!(any_member && !any_class, "{made}");
        assert!(
            uncertain.contains(&"test_made.py:Mixin.test_mixin".to_string()),
            "{made}: {uncertain:?}"
        );
    }

    // TestCase の派生か分からないクラスは、テストメソッドが無くても unittest が実行する
    // `runTest` を持てばテストを持ちうる。
    let (_, uncertain, _, _) = project_with(
        &[
            (
                "base_case.py",
                "from external_cases import ExternalCase\n\n\nclass TestCase(ExternalCase):\n    pass\n",
            ),
            (
                "checks.py",
                "from base_case import TestCase\n\n\nclass SmokeCase(TestCase):\n    def runTest(self):\n        pass\n",
            ),
        ],
        &[],
    );
    assert!(
        uncertain.contains(&"checks.py:SmokeCase".to_string())
            && uncertain.contains(&"checks.py:SmokeCase.runTest".to_string()),
        "{uncertain:?}"
    );

    let plain = "class Box:\n    def __init__(self, value):\n        self.value = value\n\n\nclass Unused:\n    def test_unused(self):\n        pass\n\n\nitems = []\nTestValue = Box(items)\nTestFlag = Box(None) is None\nTestMember = items in [1]\n";
    let (live, uncertain) = project(&[("test_data.py", plain)]);
    assert_eq!(live, Vec::<String>::new());
    assert_eq!(uncertain, Vec::<String>::new());

    let registered = (
        "helpers.py",
        "registry = []\n\n\ndef register(cls):\n    registry.append(cls)\n    return cls\n\n\ndef check():\n    pass\n\n\n@register\nclass Checks:\n    test_alias = staticmethod(check)\n\n\n@register\nclass Outer:\n    class TestNested:\n        def test_inner(self):\n            pass\n\n\nclass Plain:\n    value = 1\n",
    );
    let entry = (
        "test_entry.py",
        "from helpers import registry\n\nTestPicked = registry.pop()\n",
    );
    let (_, uncertain, any_member, _) = project_with(&[registered, entry], &[]);
    assert!(any_member);
    assert_eq!(
        uncertain,
        sorted(&[
            "helpers.py:Checks",
            "helpers.py:Outer",
            "helpers.py:TestNested",
            "helpers.py:TestNested.test_inner",
        ])
    );
}

/// codex の指摘から: 基底を `TestCase` と書いたクラス (と、その派生) は、TestCase の派生か
/// 分からなくても、見えない基底からテストを継承しうるので定義を保留する。`import *` は標準の
/// 名前 (`namedtuple`) を上書きしうるので、標準のクラスの作り方とみなさない。クラスの属性を
/// 通した `type` の別名を継承したクラスも metaclass。
#[test]
fn project_written_testcase_star_overrides_and_type_attributes() {
    let (_, uncertain, _, _) = project_with(
        &[
            (
                "base_case.py",
                "from external_cases import ExternalCase\n\n\nclass TestCase(ExternalCase):\n    pass\n",
            ),
            (
                "checks.py",
                "from base_case import TestCase\n\n\nclass SmokeCase(TestCase):\n    def setUp(self):\n        pass\n\n\nclass Deeper(SmokeCase):\n    pass\n",
            ),
            (
                "models.py",
                "from external_models import Model\n\n\nclass User(Model):\n    def save(self):\n        pass\n",
            ),
        ],
        &[],
    );
    assert_eq!(
        uncertain,
        sorted(&[
            "checks.py:Deeper",
            "checks.py:SmokeCase",
            "checks.py:SmokeCase.setUp",
        ])
    );

    let overridden = [
        (
            "factory.py",
            "class SharedMixin:\n    def test_factory(self):\n        pass\n\n\ndef namedtuple(name, fields):\n    return SharedMixin\n",
        ),
        (
            "checks.py",
            "import unittest\nfrom collections import namedtuple\nfrom factory import *\n\n\nclass FactoryCase(namedtuple(\"Base\", []), unittest.TestCase):\n    pass\n",
        ),
    ];
    let (_, _, any_member, any_class) = project_with(&overridden, &[]);
    assert!(any_member && !any_class);

    let attribute = "from unittest import TestCase\n\n\nclass Mixin:\n    def test_meta(self):\n        pass\n\n\nclass Box:\n    Type = type\n\n\nclass Meta(Box.Type):\n    pass\n\n\nTestMade = Meta(\"TestMade\", (Mixin, TestCase), {})\n";
    let (_, uncertain, any_member, _) = project_with(&[("test_made.py", attribute)], &[]);
    assert!(any_member);
    assert!(
        uncertain.contains(&"test_made.py:Mixin.test_meta".to_string()),
        "{uncertain:?}"
    );
}

/// 構文エラーのあるファイルでも、エラー回復でクラスの本体の文を入れた ERROR の子のメソッドを
/// クラスのメンバーにする (シンボルの抽出と同じ)。
#[test]
fn class_bodies_inside_error_recovery_are_members() {
    let source = "import unittest\n\n\nclass Positions(unittest.TestCase):\n    def test_before(self):\n        pass\n\n    def test_weird(self):\n        def f():\n            (bar.\n        baz)\n            (bar.\n        baz(\n        ))\n            files().setdefault(\n                0\n            ).setdefault(\n                0\n            )\n        for line, end_line, column, end_column in f.__code__.co_positions():\n            pass\n\n    def test_after(self):\n        pass\n";
    let tree = parse(source);
    assert!(tree.root_node().has_error());
    assert_eq!(
        live_in(source, false),
        sorted(&["Positions", "Positions.test_before", "Positions.test_weird"])
    );
}

/// codex の指摘から: TestCase の派生か分からないクラス (プロジェクトの関数が返す基底) から見えない
/// 基底をたどると、どのクラスのメンバーも集めうるとする (返したクラスが TestCase と mixin を
/// 継承していれば、unittest は mixin の `test*` を実行する)。`match` の capture は何でも
/// ありうる。
#[test]
fn project_unknown_entries_through_hidden_bases_hold_members() {
    let factory = "import unittest\n\n\nclass Mixin:\n    def test_mixin(self):\n        pass\n\n\ndef make_base():\n    class Generated(unittest.TestCase, Mixin):\n        pass\n\n    return Generated\n\n\nclass Suite(make_base()):\n    pass\n";
    let (_, _, any_member, any_class) = project_with(&[("suites.py", factory)], &[]);
    assert!(any_member && !any_class);

    let captured = "class Checks:\n    def test_match(self):\n        pass\n\n\nmatch [Checks]:\n    case [Parent]:\n        pass\n\n\nclass TestPicked(Parent):\n    pass\n";
    let (live, _, any_member, any_class) = project_with(&[("test_match.py", captured)], &[]);
    assert_eq!(live, sorted(&["test_match.py:TestPicked"]));
    assert!(any_member && !any_class);
}

/// codex の指摘から: subscript の値のクラスが、独自の `__class_getitem__` (自作の
/// `GenericAlias` を含む)・metaclass・それを持つ基底を持てば、subscript は何でも返しうる。
/// `types` の `GenericAlias` は標準の generic と同じ。継承の循環でも止まる。ファイル単位では、
/// 基底も metaclass も持たないクラスの subscript だけを値のクラスとする。
#[test]
fn project_custom_subscripts_through_metaclasses_and_bases() {
    let custom = [
        "class Actual:\n    def test_generic(self):\n        pass\n\n\ndef GenericAlias(cls, item):\n    return Actual\n\n\nclass Selector:\n    __class_getitem__ = classmethod(GenericAlias)\n\n\nclass TestPicked(Selector[int]):\n    pass\n",
        "class Actual:\n    def test_meta(self):\n        pass\n\n\nclass Meta(type):\n    def __getitem__(cls, item):\n        return Actual\n\n\nclass Selector(metaclass=Meta):\n    pass\n\n\nclass TestPicked(Selector[int]):\n    pass\n",
        "class Actual:\n    def test_actual(self):\n        pass\n\n\nclass Root:\n    def __class_getitem__(cls, item):\n        return Actual\n\n\nclass Selector(Root):\n    def test_never(self):\n        pass\n\n\nclass TestPicked(Selector[int]):\n    pass\n",
    ];
    for source in custom {
        let (live, _, any_member, any_class) = project_with(&[("test_index.py", source)], &[]);
        assert_eq!(live, sorted(&["test_index.py:TestPicked"]), "{source}");
        assert!(any_member && !any_class, "{source}");
        assert_eq!(live_in(source, true), sorted(&["TestPicked"]), "{source}");
    }

    let standard = "from types import GenericAlias\n\n\nclass Selector:\n    __class_getitem__ = classmethod(GenericAlias)\n\n    def test_selected(self):\n        pass\n\n\nclass TestPicked(Selector[int]):\n    pass\n";
    let (live, _) = project(&[("test_standard.py", standard)]);
    assert_eq!(
        live,
        sorted(&[
            "test_standard.py:Selector.test_selected",
            "test_standard.py:TestPicked"
        ])
    );
    assert_eq!(
        live_in(standard, true),
        sorted(&["Selector.test_selected", "TestPicked"])
    );

    let cycle = "from types import GenericAlias\n\n\nclass A:\n    __class_getitem__ = classmethod(GenericAlias)\n\n\nclass B(A[int]):\n    pass\n\n\nclass A(B):\n    pass\n\n\nclass TestPicked(A[int]):\n    pass\n";
    let (live, _, _, _) = project_with(&[("test_cycle.py", cycle)], &[]);
    assert_eq!(live, sorted(&["test_cycle.py:TestPicked"]));
}

/// 別名・import・属性の長い連鎖でも、名前の解決の深さに上限を設けてスタックを使い切らない
/// (上限を超えた先は、どのクラスでもありうるとして保留する)。上限の内側の連鎖は解決する。
#[test]
fn long_binding_chains_stop_at_the_depth_limit() {
    let chain = |length: usize| {
        let mut source =
            String::from("class Base:\n    def test_base(self):\n        pass\n\n\nA0 = Base\n");
        for index in 1..length {
            source.push_str(&format!("A{index} = A{}\n", index - 1));
        }
        source.push_str(&format!("\n\nclass TestDeep(A{}):\n    pass\n", length - 1));
        source
    };
    let short = chain(8);
    let (live, _) = project(&[("test_short.py", short.as_str())]);
    assert_eq!(
        live,
        sorted(&["test_short.py:Base.test_base", "test_short.py:TestDeep"])
    );
    assert!(live_in(&short, true).contains(&"Base.test_base".to_string()));

    let long = chain(5_000);
    let (live, _, any_member, _) = project_with(&[("test_long.py", long.as_str())], &[]);
    assert_eq!(live, sorted(&["test_long.py:TestDeep"]));
    assert!(any_member);
    assert_eq!(live_in(&long, true), sorted(&["TestDeep"]));

    // 属性の連鎖 (`Box.Next` の先がまた属性) と、別のモジュールをまたぐ import の連鎖。
    let mut files: Vec<(String, String)> = (0..2_000)
        .map(|index| {
            let body = if index == 0 {
                "class Leaf:\n    def test_leaf(self):\n        pass\n".to_string()
            } else {
                format!("from m{} import Leaf\n", index - 1)
            };
            (format!("m{index}.py"), body)
        })
        .collect();
    files.push((
        "test_imports.py".to_string(),
        "from m1999 import Leaf\n\n\nclass TestImports(Leaf):\n    pass\n".to_string(),
    ));
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect();
    let (live, _, any_member, _) = project_with(&files, &[]);
    assert_eq!(live, sorted(&["test_imports.py:TestImports"]));
    assert!(any_member);
}

/// テストモジュールで、どのクラスでもありうる名前 (module の `__getattr__` が返す) を `Test*`
/// でない名前に束縛しても、`__test__ = True` と分かっているクラスだけが集められうる
/// (`__test__` が分からないだけのクラスまで広げない)。
#[test]
fn project_wildcard_bindings_only_reach_marked_classes() {
    let (live, uncertain, any_member, any_class) = project_with(
        &[
            (
                "lazy.py",
                "from marked import Marked\n\n\ndef __getattr__(name):\n    return Marked\n",
            ),
            (
                "marked.py",
                "class Marked:\n    __test__ = True\n\n    def test_marked(self):\n        pass\n",
            ),
            (
                "external_based.py",
                "from somewhere import External\n\n\nclass Unknown(External):\n    def setup_method(self, method):\n        pass\n",
            ),
            ("test_lazy.py", "from lazy import thing\n"),
        ],
        &[],
    );
    assert_eq!(live, Vec::<String>::new());
    assert_eq!(
        uncertain,
        sorted(&["marked.py:Marked", "marked.py:Marked.test_marked"])
    );
    assert!(!any_member && !any_class);
}
