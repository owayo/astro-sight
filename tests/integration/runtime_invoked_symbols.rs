//! 呼び出し側に名前が識別子として現れない使われ方 (callable の配列・文字列、属性、
//! フレームワークの登録、言語の演算子) をしているシンボルを、dead-code が dead と
//! 報告しないことの統合テスト。各テストは「使われていない」対照のシンボルが dead のまま
//! であることも確かめる。

#[allow(unused_imports)]
use super::support::*;

fn dead_names_with(repo: &TestRepo, args: &[&str]) -> Vec<String> {
    let json = repo.run_json("dead-code", args);
    let mut names: Vec<String> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols")
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

fn dead_names(repo: &TestRepo) -> Vec<String> {
    dead_names_with(repo, &[])
}

/// PHP の callable (`[$this, 'm']` / `['Plugin', 'm']` / `[new Plugin(), 'm']` /
/// `'Plugin::m'` / `Closure::fromCallable([$this, 'm'])` / `is_callable([$this, 'm'])`)
/// でしか渡さないメソッドを dead にしない。
///
/// 旧実装は第1要素が `X::class` の配列と `Class@method` の文字列だけを参照に数えていた。
/// 同名のメソッドが別クラスにあると member liveness が文字列を見て報告しなくなるので、
/// 無関係なクラスの有無で結果が入れ替わっていた (その場合も報告しないことを確かめる)。
#[test]
fn php_callable_arrays_and_strings_keep_methods_alive() {
    let plugin = "<?php\nclass Plugin {\n    public function boot(array $a): void {\n        add_action('init', [$this, 'viaThis']);\n        usort($a, ['Plugin', 'viaClassString']);\n        call_user_func([new Plugin(), 'viaNew']);\n        call_user_func('Plugin::viaStaticString');\n        call_user_func([self::class, 'viaSelfClass']);\n        call_user_func([Plugin::class, 'viaClassConst']);\n        $g = Closure::fromCallable([$this, 'viaFromCallable']);\n        is_callable([$this, 'viaIsCallable']);\n    }\n    public function viaThis(): void {}\n    public function viaClassString(): int { return 0; }\n    public function viaNew(): void {}\n    public static function viaStaticString(): void {}\n    public static function viaSelfClass(): void {}\n    public static function viaClassConst(): void {}\n    public function viaFromCallable(): void {}\n    public function viaIsCallable(): void {}\n    public function neverUsed(): void {}\n}\n";
    let repo = TestRepo::new();
    repo.write("Plugin.php", plugin);
    repo.write("boot.php", "<?php\n(new Plugin())->boot([]);\n");
    let dead = dead_names(&repo);
    assert_eq!(dead, vec!["Plugin.neverUsed".to_string()], "{dead:?}");

    // 別クラスに同名のメソッドがあっても、callable で渡したメソッドは dead にならない
    repo.write(
        "Other.php",
        "<?php\nclass Other {\n    public function viaThis(): void {}\n}\n",
    );
    let dead = dead_names(&repo);
    assert!(!dead.contains(&"Plugin.viaThis".to_string()), "{dead:?}");
    assert!(dead.contains(&"Plugin.neverUsed".to_string()), "{dead:?}");
}

/// Bash の関数名は `.` を含みうる (`utils.log() { … }`)。dead-code は名前全体で参照を数える。
///
/// 旧実装は qualname の末尾で畳んで `log` を数え、呼ばれている `utils.log` を dead と
/// 報告していた (`str::trim` のように `::` を使うと正しく live になる非対称)。
#[test]
fn bash_dotted_function_names_are_counted_by_full_name() {
    let repo = TestRepo::new();
    repo.write(
        "lib.sh",
        "utils.log() {\n    echo \"$1\"\n}\n\nstr::trim() {\n    echo \"$1\"\n}\n\nunused.fn() {\n    :\n}\n",
    );
    repo.write(
        "main.sh",
        "source ./lib.sh\nutils.log \"start\"\nstr::trim \"  x  \"\n",
    );
    assert_eq!(dead_names(&repo), vec!["unused.fn".to_string()]);
}

/// C# の `override` (修飾子は `modifier` ノードとして 1 つずつ並ぶ) をフレームワークからの
/// 呼び出し口として除外する。旧実装は `modifiers` ノードだけを探しており、`OnModelCreating` /
/// `ToString` / `OnStart` などを dead と報告していた。
#[test]
fn csharp_override_methods_are_not_dead() {
    let repo = TestRepo::new();
    repo.write(
        "AppDb.cs",
        "public class AppDb : DbContext {\n    protected override void OnModelCreating(ModelBuilder mb) {}\n    public override string ToString() { return \"db\"; }\n    public void Unused() {}\n}\n",
    );
    repo.write(
        "Program.cs",
        "public static class Program {\n    public static void Main() { System.Console.WriteLine(new AppDb()); }\n}\n",
    );
    assert_eq!(dead_names(&repo), vec!["AppDb.Unused".to_string()]);
}

/// Celery の `@shared_task` と Django のテンプレートタグ・フィルタ / admin の登録で使われる
/// 関数・クラスを dead にしない。旧実装は `@app.task` だけを登録として扱っていた。
#[test]
fn python_celery_and_django_registrations_are_not_dead() {
    let repo = TestRepo::new();
    repo.create_dir_all("shop/templatetags");
    repo.write(
        "shop/templatetags/shop_tags.py",
        "from django import template\n\nregister = template.Library()\n\n@register.filter\ndef currency(v):\n    return v\n\n@register.simple_tag\ndef site_name():\n    return \"shop\"\n\n@register.inclusion_tag(\"cart.html\")\ndef cart_summary(cart):\n    return {}\n\ndef unused_tag_helper():\n    return 1\n",
    );
    repo.write(
        "shop/admin.py",
        "from django.contrib import admin\nfrom .models import Product\n\n@admin.register(Product)\nclass ProductAdmin(admin.ModelAdmin):\n    pass\n",
    );
    repo.write(
        "shop/tasks.py",
        "from celery import shared_task\nfrom .celery_app import app\n\n@shared_task\ndef rebuild_index():\n    pass\n\n@app.task\ndef send_digest():\n    pass\n",
    );
    let dead = dead_names(&repo);
    for live in [
        "currency",
        "site_name",
        "cart_summary",
        "ProductAdmin",
        "rebuild_index",
        "send_digest",
    ] {
        assert!(!dead.contains(&live.to_string()), "{live}: {dead:?}");
    }
    assert!(dead.contains(&"unused_tag_helper".to_string()), "{dead:?}");
}

/// PHP 8 の属性 (`#[Route]` / `#[Test]` / `#[Before]`) を、同じ意味の docblock のアノテーション
/// (`@Route` / `@test`) と同じく除外する。旧実装は docblock だけを見ていた。
#[test]
fn php_attributes_mark_runtime_entrypoints_like_docblocks() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/Controller");
    repo.create_dir_all("tests");
    repo.write(
        "src/Controller/ProductController.php",
        "<?php\nnamespace App\\Controller;\n\nuse Symfony\\Component\\Routing\\Attribute\\Route;\n\n#[Route('/products')]\nclass ProductController {\n    #[Route('/list', name: 'product_list')]\n    public function list(): array { return []; }\n\n    /** @Route(\"/legacy\", name=\"legacy\") */\n    public function legacy(): array { return []; }\n\n    public function notRouted(): array { return []; }\n}\n",
    );
    repo.write(
        "tests/CalcTest.php",
        "<?php\nuse PHPUnit\\Framework\\TestCase;\nuse PHPUnit\\Framework\\Attributes\\Test;\n\nclass CalcTest extends TestCase {\n    #[Test]\n    public function addsNumbers(): void {}\n\n    /** @test */\n    public function addsZero(): void {}\n\n    #[\\PHPUnit\\Framework\\Attributes\\Before]\n    public function prepare(): void {}\n}\n",
    );
    let dead = dead_names_with(&repo, &["--include-tests"]);
    for live in [
        "ProductController",
        "ProductController.list",
        "ProductController.legacy",
        "CalcTest.addsNumbers",
        "CalcTest.addsZero",
        "CalcTest.prepare",
    ] {
        assert!(!dead.contains(&live.to_string()), "{live}: {dead:?}");
    }
    assert!(
        dead.contains(&"ProductController.notRouted".to_string()),
        "{dead:?}"
    );
}

/// Kotlin の `operator fun` と Swift の `callAsFunction` は演算子の構文 (`a + b` / `a[i]` /
/// `a()` / `x in a`) から呼ばれ、呼び出し側に名前が現れない。dead-code の候補から外す。
#[test]
fn kotlin_operators_and_swift_call_as_function_are_not_dead() {
    let repo = TestRepo::new();
    repo.create_dir_all("kt");
    repo.create_dir_all("swift");
    repo.write(
        "kt/Vec.kt",
        "package v\n\ndata class Vec(val x: Int) {\n    operator fun plus(o: Vec) = Vec(x + o.x)\n    operator fun get(i: Int) = x + i\n    operator fun invoke() = x\n    operator fun contains(v: Int) = v == x\n    fun unusedK() = 0\n}\n\nfun main() {\n    val a = Vec(1)\n    val b = a + a\n    println(b[0] + a() + (if (1 in a) 1 else 0))\n}\n",
    );
    repo.write(
        "swift/Vec.swift",
        "public struct Vec {\n    public let x: Int\n    public func callAsFunction() -> Int { x }\n    public func unusedS() -> Int { 0 }\n}\n\npublic func run() -> Int {\n    let a = Vec(x: 1)\n    return a()\n}\n",
    );
    let dead = dead_names(&repo);
    for live in [
        "Vec.plus",
        "Vec.get",
        "Vec.invoke",
        "Vec.contains",
        "Vec.callAsFunction",
    ] {
        assert!(!dead.contains(&live.to_string()), "{live}: {dead:?}");
    }
    for unused in ["Vec.unusedK", "Vec.unusedS"] {
        assert!(dead.contains(&unused.to_string()), "{unused}: {dead:?}");
    }
}

/// PHP の `instanceof` の右辺のクラス名は大文字小文字を無視して照合する (型ヒント・`new`・
/// `catch` と同じ)。旧実装は完全一致に落ち、`instanceof gadget` からだけ使う `class Gadget` を
/// dead と報告していた。属性 `#[...]` のクラス名の照合は単体テスト
/// (`php_instanceof_and_attribute_class_names_are_case_insensitive`) で固定する
/// (属性の付いたクラスは runtime annotation として候補から外れるので dead-code では確かめられない)。
#[test]
fn php_instanceof_class_name_is_case_insensitive_for_dead_code() {
    let repo = TestRepo::new();
    repo.write("a.php", "<?php\nclass Gadget {}\nclass Unused {}\n");
    repo.write(
        "b.php",
        "<?php\nfunction check($a) { return $a instanceof gadget; }\n",
    );
    repo.write("c.php", "<?php\ncheck(1);\n");
    let dead = dead_names(&repo);
    assert!(!dead.contains(&"Gadget".to_string()), "{dead:?}");
    assert!(dead.contains(&"Unused".to_string()), "{dead:?}");
}

/// serde の `#[serde(default = "default_port")]` からしか使われない関数を dead にしない
/// (serde の derive が呼ぶので、消すとコンパイルできない)。
#[test]
fn rust_serde_default_function_is_not_dead() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"cfg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "#[derive(serde::Deserialize)]\npub struct Config {\n    #[serde(default = \"default_port\")]\n    pub port: u16,\n}\n\npub fn default_port() -> u16 {\n    8080\n}\n\npub fn unused_default() -> u16 {\n    0\n}\n",
    );
    let dead = dead_names(&repo);
    assert!(!dead.contains(&"default_port".to_string()), "{dead:?}");
    assert!(dead.contains(&"unused_default".to_string()), "{dead:?}");
}

/// Pydantic がモデルの検証・シリアライズのときに呼ぶメソッド (`@model_validator` 等) と、
/// 別名で import した登録デコレータ・行末コメント付きの登録デコレータで登録した関数を
/// dead にしない (Issue #85)。旧実装は名前表に Pydantic のデコレータが無く、別名を
/// 解決せず、行末コメントまで名前として照合していた。
#[test]
fn python_pydantic_hooks_and_aliased_registrations_are_not_dead() {
    let repo = TestRepo::new();
    repo.write(
        "record.py",
        "from pydantic import BaseModel, computed_field, field_validator\nfrom pydantic import model_validator as mv\n\n\nclass ReadingPacket(BaseModel):\n    level: int\n\n    @mv(mode=\"after\")\n    def enforce_floor(self):\n        if self.level < 0:\n            raise ValueError(\"negative level\")\n        return self\n\n    @field_validator(\"level\")\n    @classmethod\n    def check_level(cls, v):\n        return v\n\n    @computed_field\n    @property\n    def doubled(self) -> int:\n        return self.level * 2\n\n    def unused_helper(self):\n        return 1\n\n\nReadingPacket.model_validate({\"level\": 2})\n",
    );
    repo.write(
        "tasks.py",
        "from celery import shared_task\nfrom celery import shared_task as st\n\n\n@shared_task  # nightly\ndef rebuild_index():\n    pass\n\n\n@st\ndef send_digest():\n    pass\n\n\ndef unused_job():\n    pass\n",
    );
    assert_eq!(
        dead_names(&repo),
        vec![
            "ReadingPacket.unused_helper".to_string(),
            "unused_job".to_string()
        ]
    );
}
