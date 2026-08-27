//! dead-code の言語別 member liveness 判定の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

#[test]
fn dead_code_php_string_callable_prevents_false_positive() {
    // N4 の影響: Gate::define / routing 等で string callable 経由で呼ばれるだけのメソッドが
    // dead_symbols に入らないこと。実際のユースケースは Laravel の Policy/Ability だが、
    // テストではその構造を抽象化した最小再現を使う。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
class Ability {\n\
    public function allow() { return true; }\n\
}\n\
class Bootstrapper {\n\
    public function register() {\n\
        $this->gate('check', Ability::class . '@allow');\n\
        $this->route('/x', 'Ability@allow');\n\
    }\n\
    public function gate($k, $v) { return [$k, $v]; }\n\
    public function route($p, $v) { return [$p, $v]; }\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead.iter().any(|n| n.contains("allow")),
        "string callable で呼ばれる Ability::allow は dead にならないこと: {dead:?}"
    );
}

#[test]
fn dead_code_php_abstract_methods_and_interface_decls_are_not_dead() {
    // PHP の `abstract public function ...` は子クラスでの実装が必須、
    // `interface X { public function y(); }` は implementer が必ず提供するため、
    // 宣言そのものを dead として報告するのは誤検出。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();

    std::fs::write(
        root.join("src/abstract_interface_sample.php"),
        "<?php\n\
abstract class AbstractCommand {\n\
    abstract public function mustImplement(): void;\n\
    public function concreteHelper(): int { return 0; }\n\
}\n\
interface BoundaryContract {\n\
    public function boundaryEntry(): void;\n\
    public function boundaryExit(): void;\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for banned in ["mustImplement", "boundaryEntry", "boundaryExit"] {
        assert!(
            !names.iter().any(|n| n.contains(banned)),
            "{banned} は abstract/interface 宣言のため dead 対象から外れるべき: {names:?}"
        );
    }
    // abstract class の通常 (concrete) method は従来どおり dead 判定される
    // (子クラスからの呼び出しが refs で拾えるかは別問題)
    assert!(
        names.iter().any(|n| n.contains("concreteHelper")),
        "abstract class 内の concrete method は従来どおり dead として報告される: {names:?}"
    );
}

#[test]
fn dead_code_php_abstract_base_and_trait_are_reachable_via_extends_and_use() {
    // PHP の `class Derived extends AbstractBase` と `use TraitX;` は tree-sitter で
    // 一見 class_declaration の子孫として現れるため parent/grandparent 走査だけだと
    // 基底クラス名・使用 trait 名が `Definition` に誤分類され、実際の参照にも
    // 関わらず dead-code 判定される。field_name == "name" の識別子だけを def と
    // 数えることで、継承 / trait 経由の参照が正しくカウントされる。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();

    std::fs::write(
        root.join("src/base_contract.php"),
        "<?php\nabstract class BaseContract {\n    public function contractHook(): void {}\n}\ninterface SignerContract {\n    public function sign(): void;\n}\ntrait SharedBehavior {\n    public function shared(): void {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/concrete.php"),
        "<?php\nclass Concrete extends BaseContract implements SignerContract {\n    use SharedBehavior;\n    public function sign(): void {}\n    public function publicButUnused(): void {}\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    // extends / implements / use で参照されているクラス・インターフェイス・trait は
    // dead 扱いされない
    for reachable in ["BaseContract", "SignerContract", "SharedBehavior"] {
        assert!(
            !names.iter().any(|n| n == reachable),
            "{reachable} は extends/implements/use 経由で参照されているため dead 対象から外れるべき: {names:?}"
        );
    }
    // 実際に未参照の public メソッドは従来どおり dead
    assert!(
        names.iter().any(|n| n.contains("publicButUnused")),
        "真の未参照 public メソッドは dead として報告される: {names:?}"
    );
}

#[test]
fn dead_code_php_protected_and_private_methods_are_not_dead() {
    // PHP の `protected` / `private` メソッドは公開 API ではないため、
    // cross-file の識別子参照が無くても dead-code 対象にしない。
    // 対照として `public` メソッド (参照なし) は dead として報告される。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();

    // クラス内の各 visibility / トップレベル関数 / trait 内メソッド を網羅
    std::fs::write(
        root.join("src/visibility_sample.php"),
        "<?php\n\
class VisibilitySampleHolder {\n\
    public function publicUnreferenced() {}\n\
    protected function protectedHelper() {}\n\
    private function privateHelper() {}\n\
    public static function publicStaticUnreferenced() {}\n\
    protected static function protectedStatic() {}\n\
}\n\
trait VisibilitySampleTrait {\n\
    protected function traitProtectedHelper() {}\n\
    private function traitPrivateHelper() {}\n\
}\n\
function free_unreferenced_helper() {}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    // public な未参照シンボル 3 つは dead として報告される
    assert!(
        names.iter().any(|n| n.contains("publicUnreferenced")),
        "public method は dead として報告される: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("publicStaticUnreferenced")),
        "public static method も dead として報告される: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "free_unreferenced_helper"),
        "トップレベル function (暗黙 public) は dead として報告される: {names:?}"
    );

    // protected / private は visibility で除外
    for banned in [
        "protectedHelper",
        "privateHelper",
        "protectedStatic",
        "traitProtectedHelper",
        "traitPrivateHelper",
    ] {
        assert!(
            !names.iter().any(|n| n.contains(banned)),
            "{banned} は protected/private なので dead 判定から除外されるべき: {names:?}"
        );
    }
}

#[test]
fn dead_code_php_phpunit_conventions_excluded() {
    // PHPUnit の規約的メソッド / クラスは識別子レベルの cross-file ref がないが
    // PHPUnit ランナーから自動呼出しされるため dead-code から除外する。
    // - メソッド名が `^test[A-Z_]` で始まる (testBar, test_case_one, testAccess_ok)
    // - `setUp`, `tearDown`, `setUpBeforeClass`, `tearDownAfterClass`
    // - クラス名末尾が `Test` / `TestCase` / `IntegrationTest` / `FeatureTest`
    //
    // 意図的に `--include-tests` を付けて tests/ ディレクトリ除外を無効化し、
    // 命名規約ベースの除外だけを効かせる。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("tests/SampleCaseTest.php"),
        "<?php\nclass SampleCaseTest {\n    public function setUp(): void {}\n    public function tearDown(): void {}\n    public function testBar(): void {}\n    public function regular_helper(): void {}\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    // PHPUnit 規約シンボルは除外される
    for banned in [
        "SampleCaseTest",
        "SampleCaseTest.setUp",
        "SampleCaseTest.tearDown",
        "SampleCaseTest.testBar",
    ] {
        assert!(
            !names.iter().any(|n| n == banned),
            "{banned} は PHPUnit 規約として dead-code から除外されるべき: {names:?}"
        );
    }
    // 規約外の通常 public メソッドは従来通り dead 判定
    assert!(
        names.iter().any(|n| n == "SampleCaseTest.regular_helper"),
        "PHPUnit 規約外のメソッドは dead として報告される: {names:?}"
    );
}

#[test]
fn dead_code_phpunit_class_helpers_excluded_from_test_only() {
    // PHPUnit テストクラス内の helper メソッドが、同一クラス内の self::/static::/
    // $this-> 呼び出しでのみ参照されている場合、test_only_symbols ではなく
    // "ランナー内部用ヘルパー" として完全に除外される。
    // 現実には @dataProvider / #[DataProvider] / @depends 経由で reflection 呼び出し
    // されるが識別子レベルの cross-file refs では追跡できないため、test_only に
    // 大量のノイズが出るのを防ぐ目的。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("test/Models")).unwrap();
    std::fs::write(
        root.join("test/Models/SampleLogEntityTest.php"),
        r#"<?php
class SampleLogEntityTest {
    public function testCreatesEntity(): void {
        $vo = self::voEventTime();
        $msg = self::voMessage();
        // assert ...
    }

    public static function voEventTime(): string {
        return '2025-01-01T00:00:00Z';
    }

    public static function voMessage(): string {
        return 'sample';
    }
}
"#,
    )
    .unwrap();

    // production コードからは voEventTime / voMessage を参照しない。
    // self::voEventTime() / self::voMessage() の呼び出しはテストファイル内なので
    // test refs > 0 になり、従来は test_only_symbols に積まれていた。
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");

    // test_only_symbols は空 Vec の場合 serde で省略されるため、フィールド不在は空扱い
    let test_only_names: Vec<String> = json["test_only_symbols"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // voEventTime / voMessage は PHPUnit テストクラスの内部 helper として
    // test_only_symbols から除外される (test refs > 0 でもノイズなので捨てる)
    for banned in [
        "SampleLogEntityTest.voEventTime",
        "SampleLogEntityTest.voMessage",
    ] {
        assert!(
            !test_only_names.iter().any(|n| n == banned),
            "{banned} は PHPUnit container 内 helper として test_only_symbols から除外されるべき: {test_only_names:?}"
        );
    }
}

/// PHP の `ClassName::method()` 形式の cross-file 静的呼び出し
/// (`scoped_call_expression`) と同一クラス内の `self::method()` / `static::method()` を
/// dead-code 検出が usage として認識し、PHPUnit テストクラス内の helper が `dead_symbols`
/// にも `test_only_symbols` にも漏れないことを確認する。
///
/// Laravel 風の `test/.../FixtureTest.php` 内 `vo*` helper が
/// `FixtureControllerTest.php` から `FixtureTest::voXxx()` で呼ばれる構造を最小再現する。
#[test]
fn dead_code_php_cross_file_scoped_static_call_keeps_phpunit_helper_alive() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("test/Acme/Models/Fixture/Controllers")).unwrap();
    // 定義側: PHPUnit テストクラス内に public static helper を並べる。
    // 同一クラス内では self:: / static:: で互いを参照する。
    std::fs::write(
        root.join("test/Acme/Models/Fixture/FixtureTest.php"),
        r#"<?php
namespace Acme\Tests\Models\Fixture;

class FixtureTest {
    public static function voPhoneNumberPrefix(): string {
        return self::voPhoneNumberPrefix2();
    }

    public static function voPhoneNumberPrefix2(): string {
        return static::voRecording();
    }

    public static function voRecording(): string {
        return 'rec';
    }

    public static function voAgc(): string {
        return 'agc';
    }
}
"#,
    )
    .unwrap();
    // 参照側: 別ディレクトリの別 PHPUnit テストクラスから cross-file で
    // `FixtureTest::voXxx()` を呼ぶ (scoped_call_expression)。
    std::fs::write(
        root.join("test/Acme/Models/Fixture/Controllers/FixtureControllerTest.php"),
        r#"<?php
namespace Acme\Tests\Models\Fixture\Controllers;

use Acme\Tests\Models\Fixture\FixtureTest;

class FixtureControllerTest {
    public function testRouting(): void {
        $phone = FixtureTest::voPhoneNumberPrefix();
        $rec = FixtureTest::voRecording();
        $agc = FixtureTest::voAgc();
    }
}
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");

    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let test_only_names: Vec<String> = json["test_only_symbols"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    for sym in [
        "FixtureTest.voPhoneNumberPrefix",
        "FixtureTest.voPhoneNumberPrefix2",
        "FixtureTest.voRecording",
        "FixtureTest.voAgc",
    ] {
        assert!(
            !dead_names.iter().any(|n| n == sym),
            "{sym} は cross-file `FixtureTest::method()` と self::/static:: 参照があるので dead_symbols に出ないこと: dead={dead_names:?}"
        );
        assert!(
            !test_only_names.iter().any(|n| n == sym),
            "{sym} は PHPUnit container 内 helper として test_only_symbols からも除外されること: test_only={test_only_names:?}"
        );
    }
}

#[test]
fn dead_code_php_trait_methods_called_via_using_classes_are_live() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("QueryA.php"),
        "<?php\ntrait QueryA { public static function findByAccount(): void {} }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("QueryB.php"),
        "<?php\ntrait QueryB { public static function findByAccount(): void {} }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("RepositoryA.php"),
        "<?php\nclass RepositoryA { use QueryA; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("RepositoryB.php"),
        "<?php\nclass RepositoryB { use QueryB; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Caller.php"),
        "<?php\nRepositoryA::findByAccount();\nRepositoryB::findByAccount();\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect();

    assert!(
        !names.contains(&"QueryA.findByAccount") && !names.contains(&"QueryB.findByAccount"),
        "trait use先クラス経由の静的呼び出しはtraitメソッドをliveにするべき: {names:?}"
    );
}

#[test]
fn dead_code_php_trait_method_adaptation_remains_ambiguous() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("Traits.php"),
        "<?php\ntrait QueryA { public static function work(): void {} }\n\
trait QueryB { public static function work(): void {} }\n\
class Repository {\n\
    use QueryA, QueryB { QueryA::work insteadof QueryB; }\n\
}\n\
Repository::work();\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect();

    assert!(
        !names.contains(&"QueryA.work") && !names.contains(&"QueryB.work"),
        "adaptation付き複数trait dispatchは安全に一意化せずdead判定をskipするべき: {names:?}"
    );
}

#[test]
fn dead_code_php_nested_trait_static_dispatch_is_live() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("Nested.php"),
        "<?php\ntrait QueryA { public static function work(): void {} }\n\
trait QueryB { public static function work(): void {} }\n\
trait RepositoryBehaviorA { use QueryA; }\n\
trait RepositoryBehaviorB { use QueryB; }\n\
class RepositoryA { use RepositoryBehaviorA; }\n\
class RepositoryB { use RepositoryBehaviorB; }\n\
RepositoryA::work();\n\
RepositoryB::work();\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect();

    assert!(
        !names.contains(&"QueryA.work") && !names.contains(&"QueryB.work"),
        "nested trait use経由の一意な静的dispatchもliveにするべき: {names:?}"
    );
}

#[test]
fn dead_code_python_unittest_conventions_excluded() {
    // Python unittest の規約 (`unittest.TestCase` 派生クラスとそのテストメソッド、
    // setUp/tearDown 等の lifecycle hook) は dead-code 判定から除外される。
    // テストランナーがリフレクションで動的 discover するため、識別子レベルの
    // cross-file refs では caller を追跡できない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("scripts/test_corpus_test.py"),
        "import unittest\n\
         \n\
         class CorpusTestScriptTests(unittest.TestCase):\n    \
             def test_is_separator(self):\n        \
                 self.assertEqual(1, 1)\n\n    \
             def test_extract_tests(self):\n        \
                 self.assertTrue(True)\n\n    \
             def setUp(self):\n        \
                 pass\n\n    \
             def tearDown(self):\n        \
                 pass\n\n    \
             def regular_helper(self):\n        \
                 return 42\n\n\n\
         class DerivedTests(CorpusTestScriptTests):\n    \
             def test_inherited(self):\n        \
                 self.assertTrue(True)\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for banned in [
        "CorpusTestScriptTests",
        "CorpusTestScriptTests.test_is_separator",
        "CorpusTestScriptTests.test_extract_tests",
        "CorpusTestScriptTests.setUp",
        "CorpusTestScriptTests.tearDown",
        // 同一ファイル内の間接継承 (CorpusTestScriptTests → DerivedTests) も解決される
        "DerivedTests",
        "DerivedTests.test_inherited",
    ] {
        assert!(
            !names.iter().any(|n| n == banned),
            "{banned} は unittest 規約として dead-code から除外されるべき: {names:?}"
        );
    }
}

#[test]
fn dead_code_python_pytest_top_level_test_functions_excluded() {
    // pytest 規約のファイル名 (`test_*.py` / `*_test.py`) のトップレベル `test_*`
    // 関数と `conftest.py` 内の関数は dead-code から除外する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("scripts/test_module.py"),
        "def test_addition():\n    assert 1 + 1 == 2\n\n\ndef regular_helper():\n    return 1\n",
    )
    .unwrap();
    std::fs::write(
        root.join("scripts/feature_test.py"),
        "def test_feature():\n    assert True\n",
    )
    .unwrap();
    std::fs::write(
        root.join("scripts/conftest.py"),
        "def my_fixture():\n    return {}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for banned in ["test_addition", "test_feature", "my_fixture"] {
        assert!(
            !names.iter().any(|n| n == banned),
            "{banned} は pytest 規約として dead-code から除外されるべき: {names:?}"
        );
    }
    // pytest 規約外のトップレベル関数は dead と判定される
    assert!(
        names.iter().any(|n| n == "regular_helper"),
        "pytest 規約外のトップレベル関数は dead として報告される: {names:?}"
    );
}

#[test]
fn dead_code_python_instance_method_is_live() {
    // Python の `obj.method()` 形式の呼び出しが参照として認識され、
    // class 内メソッドが偽陽性で dead 判定されないことを確認。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("sample.py"),
        "class GitLabClient:\n    def post_comment(self, body):\n        print(body)\n\n\ndef main():\n    client = GitLabClient()\n    client.post_comment(\"hi\")\n\n\nif __name__ == \"__main__\":\n    main()\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.contains("post_comment")),
        "obj.method() 参照が live として検出されるべき: {names:?}"
    );
}

#[test]
fn dead_code_python_classmethod_and_property_are_live() {
    // @classmethod (`Class.method()`) と @property (`obj.attr`) を参照として認識する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("sample.py"),
        "class ReviewConfig:\n    @classmethod\n    def from_env(cls):\n        return cls()\n\n    @property\n    def project_name(self):\n        return \"demo\"\n\n\ndef main():\n    config = ReviewConfig.from_env()\n    print(config.project_name)\n\n\nmain()\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.contains("from_env")),
        "@classmethod 呼び出しは live: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("project_name")),
        "@property アクセスは live: {names:?}"
    );
}

/// Python で `self.method()` / `self.attr` のクラス内自己参照も live として認識される。
/// `@property` 経由の self 属性アクセスと、他メソッドから呼ばれる self.method 呼び出しの
/// 両方が dead に載らないことを確認。
/// (レポート 2026-04-20-python-dead-code-attribute-resolution.md の再現)
#[test]
fn dead_code_python_self_method_and_property_is_live() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("sample.py"),
        r#"class GitLabClient:
    def __init__(self, cfg):
        self.cfg = cfg

    @property
    def project(self):
        return self.cfg

    def post_comment(self, body):
        _ = self.project
        print(body)

    def post_comment_as(self, body, user):
        self.post_comment(body)


def main():
    client = GitLabClient({"project_name": "x"})
    client.post_comment("hi")
    client.post_comment_as("hi", "u")


main()
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.contains("project")),
        "@property への self アクセスは live: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("post_comment")),
        "self.method() 自己呼び出しは live: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("post_comment_as")),
        "外部 caller からの obj.method() は live: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_owner_aware_unused_side_is_dead() {
    // GitLab Issue #19 の再現: TS で同名 getter が別クラスにあり、片方だけが使われている場合に
    // 未使用側の getter が dead として検出されること。
    // - VoiceLogSettingModel.isOmnis: 参照 0 件 → dead に出るべき
    // - VoiceLogModel.isOmnis: 別ファイルで使用中 → live (報告されない)
    // - VoiceLogSettingModel.isOther: 同名 export が他になく従来通り dead
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("setting/models")).unwrap();
    std::fs::create_dir_all(root.join("log/models")).unwrap();
    std::fs::create_dir_all(root.join("log/components")).unwrap();

    std::fs::write(
        root.join("setting/models/setting.model.ts"),
        "export class VoiceLogSettingModel {\n\
        \x20   voice_log_type: number = 1;\n\
        \x20   get isAmi(): boolean { return this.voice_log_type === 1; }\n\
        \x20   get isOmnis(): boolean { return this.voice_log_type === 2; }\n\
        \x20   get isOther(): boolean { return this.voice_log_type === 3; }\n\
        }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("log/models/log.model.ts"),
        "export class VoiceLogModel {\n\
        \x20   type: number = 1;\n\
        \x20   isOmnis(): boolean { return this.type === 2; }\n\
        }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("log/components/log.parts.component.ts"),
        "import { VoiceLogModel } from \"../models/log.model\";\n\
        const voiceLogs: VoiceLogModel[] = [];\n\
        console.log(voiceLogs[0].isOmnis());\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        names.contains(&"VoiceLogSettingModel.isOmnis"),
        "owner 一意推定 (import only VoiceLogModel) で未使用側の isOmnis が dead に出るべき: {names:?}"
    );
    assert!(
        !names.contains(&"VoiceLogModel.isOmnis"),
        "別ファイルで使用中の VoiceLogModel.isOmnis は dead に出るべきでない: {names:?}"
    );
    assert!(
        names.contains(&"VoiceLogSettingModel.isOther"),
        "従来通り duplicate でない isOther は dead に出るべき: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_ambiguous_when_both_owners_imported() {
    // duplicate owner を両方 import しているファイルで `.member` が使われている場合は
    // どちらの owner のメソッドへの参照か owner 一意推定できないため、ambiguous として
    // 旧スキップを維持する (どちらも dead に出さない)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();

    std::fs::write(
        root.join("models/foo.model.ts"),
        "export class FooModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("models/bar.model.ts"),
        "export class BarModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/app.ts"),
        "import { FooModel } from \"../models/foo.model\";\n\
        import { BarModel } from \"../models/bar.model\";\n\
        function pick(x: FooModel | BarModel): boolean { return x.isReady(); }\n\
        console.log(pick(new FooModel()));\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with(".isReady")),
        "両 owner を同ファイルで import している ambiguous ケースでは旧スキップを維持: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_ambiguous_when_no_import() {
    // duplicate owner のいずれも import していないファイルで `.member` が使われている場合は
    // owner 推定できないため ambiguous (旧スキップ維持)。
    // ローカルクラスや any 型経由の呼び出しを safe に保守する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::create_dir_all(root.join("util")).unwrap();

    std::fs::write(
        root.join("models/foo.model.ts"),
        "export class FooModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("models/bar.model.ts"),
        "export class BarModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("util/helper.ts"),
        "export function checkReady(x: any): boolean {\n    return x.isReady();\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with(".isReady")),
        "owner を import していないファイルで .member が出る ambiguous ケースは旧スキップ維持: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_string_literal_marks_ambiguous() {
    // bare member 名が文字列リテラルとして出現する場合は computed access の可能性があるため
    // 全 duplicate candidate を ambiguous へ倒し、旧スキップを維持する (safe-by-default)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();

    std::fs::write(
        root.join("models/foo.model.ts"),
        "export class FooModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("models/bar.model.ts"),
        "export class BarModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/app.ts"),
        "import { FooModel } from \"../models/foo.model\";\n\
        const key = \"isReady\";\n\
        const foo = new FooModel();\n\
        console.log((foo as any)[key]);\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with(".isReady")),
        "string literal で member 名が出る場合は ambiguous で旧スキップ維持: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_self_ref_in_owner_file_is_not_misattributed() {
    // codex pre-commit review 指摘の FP 修正: duplicate owner X の定義ファイルが unrelated
    // owner Y を型用途で import している場合、ファイル内の `this.member` を Y 側に誤帰属
    // させてはならない。receiver-aware 帰属では `this.isReady()` が enclosing class
    // (FooModel) へ確定票として入り、FooModel の dead 誤検出を防ぐ。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();

    std::fs::write(
        root.join("models/bar.model.ts"),
        "export class BarModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("models/foo.model.ts"),
        "import { BarModel } from \"./bar.model\";\n\
        export class FooModel {\n\
        \x20   other?: BarModel;\n\
        \x20   isReady(): boolean { return true; }\n\
        \x20   check(): boolean { return this.isReady(); }\n\
        }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/app.ts"),
        "import { FooModel } from \"../models/foo.model\";\n\
        console.log(new FooModel().check());\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    // FooModel.isReady は this.isReady() 経由で内部参照されており、外部から FooModel.check() を
    // 呼び出すコードもあるため dead に出してはならない。
    assert!(
        !names.contains(&"FooModel.isReady"),
        "owner 定義ファイル内の this.member を unrelated import 側に誤帰属して FooModel.isReady を dead と判定してはならない: {names:?}"
    );
    // receiver-aware 帰属では `this.isReady()` が FooModel へ確定解決されるため、
    // set は Ambiguous に倒れず、真に未参照の BarModel.isReady は正確に dead と出る。
    assert!(
        names.contains(&"BarModel.isReady"),
        "未参照の BarModel.isReady は receiver-aware 帰属で dead に出るべき: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_namespace_import_marks_ambiguous() {
    // `import * as ns from ...` (namespace import) の場合は ns 経由で任意の owner が
    // アクセスされうるため owner を一意推定できず ambiguous へ倒す (旧スキップ維持)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();

    std::fs::write(
        root.join("models/foo.model.ts"),
        "export class FooModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("models/bar.model.ts"),
        "export class BarModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/app.ts"),
        "import * as foo from \"../models/foo.model\";\n\
        const inst = new foo.FooModel();\n\
        console.log(inst.isReady());\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with(".isReady")),
        "namespace import (`import * as ...`) は owner 推定不能で ambiguous 維持: {names:?}"
    );
}

/// GitLab #9 回帰テスト: Xojo dead-code 検出で refs ベースの参照判定が機能することを検証。
///
/// 旧実装では `count_refs_in_file` (refs.rs) が tree-sitter 経路だけを呼んでおり、
/// Xojo は parse_file が UNSUPPORTED_LANGUAGE を返してファイル単位の count が 0 になり、
/// refs が見つかるシンボルでも dead 判定されていた。lexer-only dispatch を追加して修正。
///
/// production-only fixture で検証する (UnitTests/ 配下のテストファイルは PR2 で
/// 別 fixture に切り出し、Window Event handler / TestGroup *Test 等の framework
/// entrypoint 認識テストで使う想定)。
#[test]
fn dead_code_xojo_excludes_symbols_with_refs() {
    use std::path::Path;

    let fixture = Path::new("tests/fixtures/xojo_dead_code");
    assert!(fixture.exists(), "fixture missing: {fixture:?}");

    // dead-code は `tests/` を含むパスを test ディレクトリと判定し、
    // 全ファイルを test 扱いにする。production 参照判定を正しく検証するため
    // fixture を tempdir に複製してから走らせる。
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest_root = tmp.path().join("project");
    std::fs::create_dir_all(&dest_root).expect("create dest_root");
    let cp_status = Command::new("cp")
        .args([
            "-R",
            &format!("{}/.", fixture.display()),
            dest_root.to_str().unwrap(),
        ])
        .status()
        .expect("cp -R");
    assert!(cp_status.success(), "failed to copy fixture to tempdir");

    let output = cargo_bin()
        .args(["dead-code", "--dir", dest_root.to_str().unwrap()])
        .output()
        .expect("failed to run dead-code");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols 配列")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    let test_only_names: Vec<&str> = json["test_only_symbols"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["name"].as_str()).collect())
        .unwrap_or_default();

    // 中核回帰: refs で参照が見つかるシンボルは `dead_symbols` にも `test_only_symbols`
    // にも含めない。
    // - Greeter は Caller.Run() と Main.Open() から参照
    // - Greet は Caller.Run() から呼び出し
    // - Caller は Main.Open() で `New Caller` として参照
    // - Run は Caller インスタンスから呼び出し
    for name in ["Greeter", "Greet", "Caller", "Run"] {
        assert!(
            !dead_names.contains(&name),
            "{name} は refs で参照されているため dead 判定すべきでない: dead_names={dead_names:?}"
        );
        assert!(
            !test_only_names.contains(&name),
            "{name} は production 参照があるため test_only にも入れるべきでない: \
             test_only_names={test_only_names:?}"
        );
    }

    // 退行検出: 修正が「Xojo を一律 dead から除外」ではなく
    // 「参照ベースで正しく判定」していることを保証するため、誰からも呼ばれない
    // Orphan クラスは dead に残る必要がある。
    assert!(
        dead_names.contains(&"Orphan"),
        "未参照クラス Orphan は dead に残るべき: dead_names={dead_names:?}"
    );
}

/// GitLab #15 回帰テスト: Xojo の `#tag Event` 配下のイベントハンドラ (Sub/Function) は
/// ランタイムがイベント駆動で呼ぶ entrypoint のため dead に出ない。`#tag Events <control>`
/// 配下のハンドラが対象。`#tag Event` で囲まれない通常メソッドは従来どおり dead 判定する。
#[test]
fn dead_code_xojo_excludes_event_handlers() {
    use std::path::Path;
    use std::process::Command;

    let fixture = Path::new("tests/fixtures/xojo_dead_code");
    assert!(fixture.exists(), "fixture missing: {fixture:?}");

    let tmp = tempfile::tempdir().expect("tempdir");
    let dest_root = tmp.path().join("project");
    std::fs::create_dir_all(&dest_root).expect("create dest_root");
    let cp_status = Command::new("cp")
        .args([
            "-R",
            &format!("{}/.", fixture.display()),
            dest_root.to_str().unwrap(),
        ])
        .status()
        .expect("cp -R");
    assert!(cp_status.success(), "failed to copy fixture to tempdir");

    let output = cargo_bin()
        .args(["dead-code", "--dir", dest_root.to_str().unwrap()])
        .output()
        .expect("failed to run dead-code");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols 配列")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();

    // CellAction は Main.xojo_window の `#tag Events Listbox1` > `#tag Event` 配下の
    // イベントハンドラ。Xojo ランタイムが呼ぶ entrypoint のため dead に出ない。
    assert!(
        !dead_names.contains(&"CellAction"),
        "CellAction (#tag Event 配下のイベントハンドラ) は dead に出すべきでない: \
         dead_names={dead_names:?}"
    );

    // 退行検出: entrypoint でない未参照クラス Orphan は従来どおり dead に残る
    // (「一律除外」ではなく構造ベースで判定していることを保証)。
    assert!(
        dead_names.contains(&"Orphan"),
        "未参照クラス Orphan は dead に残るべき: dead_names={dead_names:?}"
    );
}

/// GitLab #16 回帰テスト: `Inherits TestGroup` クラスの引数なし `*Test` / `Setup` /
/// `TearDown` メソッドは XojoUnit が Introspection で実行する entrypoint のため dead に
/// 出ない。`#tag Event` 配下の `InitializeTestGroups` も entrypoint。Test で終わらない
/// 通常メソッド (orphanHelper) は参照0なら dead に残る。
#[test]
fn dead_code_xojo_excludes_testgroup_test_methods() {
    use std::path::Path;
    use std::process::Command;

    let fixture = Path::new("tests/fixtures/xojo_testgroup");
    assert!(fixture.exists(), "fixture missing: {fixture:?}");

    let tmp = tempfile::tempdir().expect("tempdir");
    let dest_root = tmp.path().join("project");
    std::fs::create_dir_all(&dest_root).expect("create dest_root");
    let cp_status = Command::new("cp")
        .args([
            "-R",
            &format!("{}/.", fixture.display()),
            dest_root.to_str().unwrap(),
        ])
        .status()
        .expect("cp -R");
    assert!(cp_status.success(), "failed to copy fixture to tempdir");

    let output = cargo_bin()
        .args(["dead-code", "--dir", dest_root.to_str().unwrap()])
        .output()
        .expect("failed to run dead-code");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols 配列")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    let test_only_names: Vec<&str> = json["test_only_symbols"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["name"].as_str()).collect())
        .unwrap_or_default();

    // *Test / Setup / TearDown / `#tag Event` 配下の InitializeTestGroups は entrypoint。
    // dead にも test_only にも出ない。
    for name in [
        "enableControlTest",
        "validateValueTest",
        "Setup",
        "secondScenarioTest",
        "InitializeTestGroups",
    ] {
        assert!(
            !dead_names.contains(&name),
            "{name} は XojoUnit entrypoint のため dead に出すべきでない: dead_names={dead_names:?}"
        );
        assert!(
            !test_only_names.contains(&name),
            "{name} は entrypoint のため test_only にも出すべきでない: \
             test_only_names={test_only_names:?}"
        );
    }

    // 退行検出: Test で終わらない通常メソッド orphanHelper は参照0なら dead に残る。
    assert!(
        dead_names.contains(&"orphanHelper"),
        "orphanHelper (通常メソッド, 参照0) は dead に残るべき: dead_names={dead_names:?}"
    );
}

#[test]
fn dead_code_ts_factory_receiver_marks_set_ambiguous() {
    // Issue 2026-07-10-jsts-member-liveness-factory-attribution:
    // duplicate member (Alpha.fmt / Beta.fmt) で consumer が import { Alpha } +
    // factory 経由 (`const beta = getBeta(); beta.fmt();`) の場合、receiver を
    // 静的に辿れない access があるため set 全体を Ambiguous に倒し、
    // import 有無ベースの誤帰属 (Beta.fmt の dead 誤検出) をしない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("alpha.ts"),
        "export class Alpha {\n  fmt() { return \"alpha\"; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("beta.ts"),
        "export class Beta {\n  fmt() { return \"beta\"; }\n}\nexport function getBeta(): Beta { return new Beta(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("consumer.ts"),
        "import { Alpha } from \"./alpha\";\nimport { getBeta } from \"./beta\";\nexport function run() {\n  const beta = getBeta();\n  beta.fmt();\n  return new Alpha();\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Beta.fmt"),
        "factory 経由で実際に呼ばれる Beta.fmt を dead にしない: {names:?}"
    );
    assert!(
        !names.contains(&"Alpha.fmt"),
        "unresolved access がある set は Ambiguous (旧スキップ) 維持: {names:?}"
    );
}

#[test]
fn dead_code_ts_for_of_loop_variable_shadow_marks_set_ambiguous() {
    // for-of の loop 変数が owner クラス名と同名の場合、`Alpha.fmt()` は loop 変数
    // (中身は Beta インスタンス) への access であり、import 由来の class static
    // access と誤認して Alpha へ票を計上しない。binding 収集が for-of/for-in の
    // left (bare pattern) を見落とすと Beta.fmt が票を失い dead 誤検出 (fail-open)
    // になる回帰テスト。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("alpha.ts"),
        "export class Alpha {\n  fmt() { return \"alpha\"; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("beta.ts"),
        "export class Beta {\n  fmt() { return \"beta\"; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("consumer.ts"),
        "import { Alpha } from \"./alpha\";\nimport { Beta } from \"./beta\";\nexport function run(items: Beta[]) {\n  for (const Alpha of items) {\n    Alpha.fmt();\n  }\n  return new Alpha();\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Beta.fmt"),
        "loop 変数経由で実際に呼ばれる Beta.fmt を dead にしない: {names:?}"
    );
    assert!(
        !names.contains(&"Alpha.fmt"),
        "loop 変数 shadow がある set は Ambiguous (旧スキップ) 維持: {names:?}"
    );
}

#[test]
fn dead_code_ts_owner_named_variable_is_not_class_access() {
    // codex レビュー指摘 (警告4): owner と同名のローカル変数 (`const Alpha = getBeta();`)
    // を class static access と誤認して確定票を入れない。binding が Unresolvable なので
    // set 全体が Ambiguous に倒れる。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("alpha.ts"),
        "export class Alpha {\n  fmt() { return \"alpha\"; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("beta.ts"),
        "export class Beta {\n  fmt() { return \"beta\"; }\n}\nexport function getBeta(): Beta { return new Beta(); }\n",
    )
    .unwrap();
    // Alpha は import せず、同名変数に factory 戻り値を束縛して .fmt() を呼ぶ
    std::fs::write(
        root.join("consumer.ts"),
        "import { getBeta } from \"./beta\";\nexport function run() {\n  const Alpha = getBeta();\n  Alpha.fmt();\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Beta.fmt"),
        "同名変数経由で実際に呼ばれる Beta.fmt を dead にしない (Ambiguous 維持): {names:?}"
    );
    assert!(
        !names.contains(&"Alpha.fmt"),
        "owner 同名変数を class access と誤認して Alpha へ票を入れない: {names:?}"
    );
}

#[test]
fn dead_code_ts_unrelated_namespace_import_does_not_suppress_set() {
    // codex レビュー指摘 (警告5): 無関係な `import * as ns` があるだけで
    // duplicate set 全体を Ambiguous にしない。対象 member への access が
    // 静的に解決できる別ファイルの票は生きる。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("alpha.ts"),
        // 無関係の namespace import を持つ owner 定義ファイル
        "import * as path from \"path\";\nexport class Alpha {\n  fmt() { return path.sep; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("beta.ts"),
        "export class Beta {\n  fmt() { return \"beta\"; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("consumer.ts"),
        "import { Alpha } from \"./alpha\";\nexport function run() {\n  new Alpha().fmt();\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Alpha.fmt"),
        "new Alpha().fmt() の確定票で Alpha.fmt は live: {names:?}"
    );
    assert!(
        names.contains(&"Beta.fmt"),
        "無関係 namespace import で set を Ambiguous 化せず、未参照の Beta.fmt は dead: {names:?}"
    );
}

/// Python: 型注釈位置でしか使われないクラス (基底クラス / 戻り値型) を dead にしない。
///
/// 汎用の parent/grandparent 走査が `class Derived(Base)` と `def f() -> Base` の
/// `Base` を定義と誤分類していたため、参照として数えられず dead と報告されていた。
/// 真に未参照な `Derived` は引き続き dead になること (対照) も同時に固定する。
#[test]
fn dead_code_python_type_annotation_only_classes_are_live() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.py"),
        "class OnlyBase:\n\
    pass\n\
\n\
\n\
class OnlyReturn:\n\
    pass\n\
\n\
\n\
class Derived(OnlyBase):\n\
    pass\n\
\n\
\n\
def build() -> OnlyReturn:\n\
    return None\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    assert!(
        !dead.iter().any(|n| n == "OnlyBase"),
        "基底クラスとして参照されている OnlyBase は dead にしない: {dead:?}"
    );
    assert!(
        !dead.iter().any(|n| n == "OnlyReturn"),
        "戻り値型として参照されている OnlyReturn は dead にしない: {dead:?}"
    );
    assert!(
        dead.iter().any(|n| n == "Derived"),
        "どこからも参照されない Derived は引き続き dead (対照): {dead:?}"
    );
}

/// Python: 関数内のネスト定義は公開シンボルではないため dead 候補に出さない。
///
/// デコレータでフレームワークへ登録されるハンドラ・クロージャとして返される関数は
/// ソース中に直接の呼び出し式を持たないのが設計どおりで、字句スコープを見ない
/// repo-wide の bare-name 参照検索では生死を判定できない。モジュール直下の未参照
/// 関数は引き続き dead になること (対照) も固定する。
#[test]
fn dead_code_python_nested_definitions_are_not_reported() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.py"),
        "def make_app(registry):\n\
    @registry.handler(name=\"ping\")\n\
    async def ping() -> str:\n\
        return \"pong\"\n\
\n\
    def plain_nested() -> str:\n\
        return \"local\"\n\
\n\
    class Inner:\n\
        pass\n\
\n\
    return (registry, plain_nested, Inner)\n\
\n\
\n\
def module_level_unused():\n\
    return None\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for nested in ["ping", "plain_nested", "Inner"] {
        assert!(
            !dead.iter().any(|n| n == nested),
            "ネスト定義 {nested} は公開シンボルではないので dead に出さない: {dead:?}"
        );
    }
    assert!(
        dead.iter().any(|n| n == "module_level_unused"),
        "モジュール直下の未参照関数は引き続き dead (対照): {dead:?}"
    );
}

/// 型注釈位置 (基底クラス・戻り値型) でのみ使われる型は dead ではない。
///
/// `is_definition_context` の汎用 grandparent 走査が `class_declaration > superclass`
/// や `method_declaration > type:` の識別子を def と分類していたため、参照として
/// 数えられず dead と誤報されていた (Java / C# で実測)。同名重複によるスキップと
/// 区別できるよう型名は言語ごとに分け、真に未参照の型が dead に出ること (対照) も固定する。
#[test]
fn dead_code_type_only_usage_is_live_in_java_and_csharp() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("J.java"),
        "public class J {\n\
    static class JOnlyBase {}\n\
    static class JOnlyReturn {}\n\
    static class JNeverUsed {}\n\
    static class JDerived extends JOnlyBase {}\n\
    JOnlyReturn make() { return null; }\n\
    void use() { make(); new JDerived(); }\n\
}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("C.cs"),
        "class CsOnlyBase { }\n\
class CsOnlyReturn { }\n\
class CsNeverUsed { }\n\
class CsDerived : CsOnlyBase { }\n\
class CsApp {\n\
    CsOnlyReturn Make() { return null; }\n\
    void Use() { Make(); new CsDerived(); }\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for live in ["JOnlyBase", "JOnlyReturn", "CsOnlyBase", "CsOnlyReturn"] {
        assert!(
            !dead.iter().any(|n| n == live),
            "型注釈位置で使われている {live} は dead ではない: {dead:?}"
        );
    }
    for unused in ["JNeverUsed", "CsNeverUsed"] {
        assert!(
            dead.iter().any(|n| n == unused),
            "真に未参照の {unused} は引き続き dead (対照): {dead:?}"
        );
    }
}

/// C++ の `union` メンバにも可視性判定 (`access_specifier`) を効かせる。
///
/// `is_exported_cpp` が `class_specifier` / `struct_specifier` しか列挙していなかったため、
/// union のメンバは祖先走査が translation_unit まで抜けて「クラス外 = 公開」に落ち、
/// access_specifier を 1 度も見ずに `private:` 配下まで公開扱いになっていた。
///
/// **対照ケースを内蔵する**: union は struct と同じく default public なので、
/// 修飾子なしと `public:` 配下は引き続き公開 API 面に残る。ここまで固定しないと
/// 「union を丸ごと非公開にする」逆方向の修正でもテストが通ってしまう。
#[test]
fn dead_code_cpp_union_respects_access_specifier() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("u.cpp"),
        "union Tagged {\n\
         public:\n\
         \x20 void unionPublic() {}\n\
         private:\n\
         \x20 void unionPrivate() {}\n\
         };\n\
         union Bare {\n\
         \x20 void bareDefault() {}\n\
         };\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    assert!(
        !dead.iter().any(|n| n == "unionPrivate"),
        "`private:` 配下の union メンバは公開 API 面ではない: {dead:?}"
    );
    // 対照: union は default public なので、公開メンバは従来どおり報告される
    for public_member in ["unionPublic", "bareDefault"] {
        assert!(
            dead.iter().any(|n| n == public_member),
            "union の公開メンバ {public_member} は引き続き dead 候補 (対照): {dead:?}"
        );
    }
}

/// Rust の制限付き可視性 (`pub(crate)` 等) は AST の `visibility_modifier` で判定する。
///
/// 旧実装は宣言行に `"pub("` が含まれるかの**部分一致**だったため、
/// (a) フィールドだけが制限付きの公開型 (`pub struct S { pub(crate) a: u32 }`)、
/// (b) 名前がたまたま `pub(` を含む関数 (`pub fn to_epub()`) が公開 API 面から消え、
/// api.add / api.rm / api.mod / dead-code の 4 経路が同時に沈黙していた。
///
/// **対照ケースを内蔵する**: 本当に `pub(crate)` / `pub(super)` な宣言は引き続き除外する。
/// これが無いと「Rust の可視性チェックを丸ごと外す」修正でもテストが通ってしまう。
#[test]
fn dead_code_rust_restricted_visibility_uses_ast_not_line_substring() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("lib.rs"),
        "pub struct PlainPublic { pub a: u32 }\n\
         pub struct FieldRestricted { pub(crate) a: u32 }\n\
         pub struct TupleRestricted(pub(super) u32);\n\
         pub fn to_epub() {}\n\
         pub(crate) fn internal_fn() {}\n\
         pub(super) struct InternalStruct { pub a: u32 }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    // 型・関数自体は無修飾 pub なので公開 API 面に残る
    for public in [
        "PlainPublic",
        "FieldRestricted",
        "TupleRestricted",
        "to_epub",
    ] {
        assert!(
            dead.iter().any(|n| n == public),
            "無修飾 pub の {public} は公開 API 面に残るべき: {dead:?}"
        );
    }
    // 対照: 本当に制限付きの宣言は従来どおり crate 内部として除外される
    for restricted in ["internal_fn", "InternalStruct"] {
        assert!(
            !dead.iter().any(|n| n == restricted),
            "制限付き可視性の {restricted} は公開 API 面ではない (対照): {dead:?}"
        );
    }
}
