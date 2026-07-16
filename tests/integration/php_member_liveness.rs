use super::support::TestRepo;

fn dead_names(repo: &TestRepo) -> Vec<String> {
    repo.run_json("dead-code", &[])["dead_symbols"]
        .as_array()
        .expect("dead_symbols should be an array")
        .iter()
        .filter_map(|symbol| symbol["name"].as_str().map(str::to_owned))
        .collect()
}

fn contains(names: &[String], expected: &str) -> bool {
    names.iter().any(|name| name == expected)
}

// PHP member liveness (trait dispatch / late static binding) 回帰テスト
// ---------------------------------------------------------------------------

/// PHP 8.1+ の enum も trait を use できる。enum 経由の `StatusEnum::findByAccount()`
/// が trait メソッドへの確定 dispatch として票読みされることを検証する
/// (enum_declaration が trait use 収集から漏れて dead 誤検出していた回帰)。
#[test]
fn dead_code_php_enum_trait_use_counts_static_dispatch() {
    let repo = TestRepo::new();

    repo.write(
        "QueryA.php",
        "<?php\ntrait QueryA { public static function findByAccount(): void {} }\n",
    );
    repo.write("StatusEnum.php", "<?php\nenum StatusEnum { use QueryA; }\n");
    // duplicate set を作って member liveness 経路に乗せる
    repo.write(
        "QueryB.php",
        "<?php\ntrait QueryB { public static function findByAccount(): void {} }\n",
    );
    repo.write(
        "RepositoryB.php",
        "<?php\nclass RepositoryB { use QueryB; }\n",
    );
    repo.write(
        "Caller.php",
        "<?php\nStatusEnum::findByAccount();\nRepositoryB::findByAccount();\n",
    );

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "QueryA.findByAccount") && !contains(&names, "QueryB.findByAccount"),
        "enum の trait use 経由の静的呼び出しも trait メソッドを live にするべき: {names:?}"
    );
}

/// GitLab #34: trait 本体内の `self::hook()` は合成先ホストの文脈で解決され、
/// 入れ子合成 (Host uses ChildTrait uses BaseTrait) ではホストへ展開された
/// ChildTrait 側 override が優先して呼ばれる。定義元 BaseTrait への確定票にすると
/// override の `ChildTrait.hook` が参照 0 件で dead 誤検出になるため、trait 内
/// `self::` は Ambiguous に倒して duplicate set 全体を live に保つ。
#[test]
fn dead_code_php_trait_self_call_keeps_override_live() {
    let repo = TestRepo::new();

    repo.write(
        "BaseTrait.php",
        "<?php\ntrait BaseTrait {\n    public static function run(): string {\n        return self::hook();\n    }\n    public static function hook(): string { return 'base'; }\n}\n",
    );
    repo.write(
        "ChildTrait.php",
        "<?php\ntrait ChildTrait {\n    use BaseTrait;\n    public static function hook(): string { return 'child'; }\n}\n",
    );
    repo.write("Host.php", "<?php\nclass Host { use ChildTrait; }\n");
    repo.write("main.php", "<?php\necho Host::run();\n");

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "ChildTrait.hook") && !contains(&names, "BaseTrait.hook"),
        "trait 内 self:: は合成先ホスト依存なので override を dead 判定しないべき: {names:?}"
    );
}

/// GitLab #34 の object creation 経路: trait 本体内の `new self()` も合成先ホストの
/// 文脈で解決されるため、`__construct` duplicate set の owner 確定票にせず Ambiguous に
/// 倒す (scoped call と同じ理由。codex レビュー指摘の直接保護テスト)。
#[test]
fn dead_code_php_trait_new_self_keeps_constructor_override_live() {
    let repo = TestRepo::new();

    repo.write(
        "BaseTraitC.php",
        "<?php\ntrait BaseTraitC {\n    public function __construct() {}\n    public static function make(): object {\n        return new self();\n    }\n}\n",
    );
    repo.write(
        "ChildTraitC.php",
        "<?php\ntrait ChildTraitC {\n    use BaseTraitC;\n    public function __construct() {}\n}\n",
    );
    repo.write("HostC.php", "<?php\nclass HostC { use ChildTraitC; }\n");
    repo.write("mainC.php", "<?php\nHostC::make();\n");

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "ChildTraitC.__construct") && !contains(&names, "BaseTraitC.__construct"),
        "trait 内 new self() は合成先ホスト依存なので constructor override を dead 判定しないべき: {names:?}"
    );
}

/// class 本体内の `self::helper()` は宣言クラスへ静的束縛されるため、従来どおり
/// enclosing class への確定票として解決し、無関係な同名メソッドの dead 検出精度を
/// 維持する (GitLab #34 修正が class の self:: を巻き込まない回帰担保)。
#[test]
fn dead_code_php_class_self_call_still_resolves_to_declaring_class() {
    let repo = TestRepo::new();

    repo.write(
        "Base.php",
        "<?php\nclass Base {\n    public static function helper(): void {}\n    public static function go(): void { self::helper(); }\n}\n",
    );
    repo.write(
        "Other.php",
        "<?php\nclass Other { public static function helper(): void {} }\n",
    );
    repo.write("Caller.php", "<?php\nBase::go();\n");

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "Base.helper"),
        "class 内 self:: は宣言クラスへの確定票として数えるべき: {names:?}"
    );
    assert!(
        contains(&names, "Other.helper"),
        "無関係な同名メソッドは引き続き dead として検出すべき (精度回帰担保): {names:?}"
    );
}

/// `static::helper()` は遅延静的束縛 (late static binding) でサブクラス override へ
/// dispatch されうる。enclosing class へ確定解決すると `Child.helper` が dead 誤検出に
/// なるため、Ambiguous に倒して duplicate set 全体を旧スキップへフォールバックさせる。
#[test]
fn dead_code_php_static_late_binding_stays_ambiguous() {
    let repo = TestRepo::new();

    repo.write(
        "Base.php",
        "<?php\nclass Base {\n    public static function helper(): void {}\n    public static function go(): void { static::helper(); }\n}\n",
    );
    repo.write(
        "Child.php",
        "<?php\nclass Child extends Base {\n    public static function helper(): void {}\n}\n",
    );
    repo.write("Consumer.php", "<?php\n(new Child())->go();\n");

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "Child.helper") && !contains(&names, "Base.helper"),
        "static:: は owner を確定解決できないので helper を dead 判定しないべき: {names:?}"
    );
}

/// 合成先クラスが同名の**具象**メソッドを自己宣言している場合、PHP の解決順
/// (自クラス > trait) により `Repo::work()` は trait 側へ到達しない → `QueryA.work` は
/// dead として現れる (誤って trait へ票を入れない)。一方 abstract 宣言は trait 実装で
/// 満たされ dispatch が trait へ通るため `QueryA2.work2` は live のまま。
#[test]
fn dead_code_php_trait_shadowed_by_own_concrete_method_reports_dead_trait() {
    let repo = TestRepo::new();

    // 具象 shadow 側 (duplicate set: QueryA / QueryB)
    repo.write(
        "TraitsWork.php",
        "<?php\ntrait QueryA { public static function work(): void {} }\ntrait QueryB { public static function work(): void {} }\n",
    );
    repo.write(
        "Repo.php",
        "<?php\nclass Repo { use QueryA; public static function work(): void {} }\n",
    );
    repo.write("RepoB.php", "<?php\nclass RepoB { use QueryB; }\n");
    // abstract 側 (duplicate set: QueryA2 / QueryB2)。abstract メソッドを持つ class は
    // abstract class でないと文法エラーになる点に注意。
    repo.write(
        "TraitsWork2.php",
        "<?php\ntrait QueryA2 { public static function work2(): void {} }\ntrait QueryB2 { public static function work2(): void {} }\n",
    );
    repo.write(
        "Repo2.php",
        "<?php\nabstract class Repo2 { use QueryA2; abstract public static function work2(): void; }\n",
    );
    repo.write("RepoB2.php", "<?php\nclass RepoB2 { use QueryB2; }\n");
    repo.write(
        "CallerWork.php",
        "<?php\nRepo::work();\nRepoB::work();\nRepo2::work2();\nRepoB2::work2();\n",
    );

    let names = dead_names(&repo);
    // 具象 shadow: Repo::work() は自クラスの具象メソッドで解決され QueryA へ票が入らない
    assert!(
        contains(&names, "QueryA.work"),
        "own 具象メソッドが trait を shadow するため QueryA.work は dead に出るべき: {names:?}"
    );
    assert!(
        !contains(&names, "QueryB.work"),
        "RepoB::work() は QueryB.work への確定 dispatch なので live のはず: {names:?}"
    );
    // abstract 宣言は trait 実装で満たされ dispatch が trait へ通る
    assert!(
        !contains(&names, "QueryA2.work2") && !contains(&names, "QueryB2.work2"),
        "abstract 宣言は trait 実装で満たされるため QueryA2.work2 は dead に出ないべき: {names:?}"
    );
}

/// `__construct` の duplicate set では `new Foo()` (object_creation_expression) が
/// 参照源になる。owner 確定票として数え、両 constructor の dead 誤検出を防ぐ
/// (Issue 2026-07-10-php-magic-construct-duplicate-dead-fp)。
#[test]
fn dead_code_php_construct_duplicate_counts_object_creation() {
    let repo = TestRepo::new();

    repo.write(
        "Foo.php",
        "<?php\nclass Foo { public function __construct() {} }\n",
    );
    repo.write(
        "Bar.php",
        "<?php\nclass Bar { public function __construct() {} }\n",
    );
    repo.write("Use.php", "<?php\n$f = new Foo();\n$b = new Bar();\n");

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "Foo.__construct") && !contains(&names, "Bar.__construct"),
        "new Foo() / new Bar() が constructor への確定参照として数えられるべき: {names:?}"
    );
}

/// `new Foo()` の無い側の `__construct` は正確に dead と出る (prefilter が `new` を
/// 含むファイルを parse 対象に残すことの検証も兼ねる)。
#[test]
fn dead_code_php_construct_duplicate_unused_side_is_dead() {
    let repo = TestRepo::new();

    repo.write(
        "Foo.php",
        "<?php\nclass Foo { public function __construct() {} }\n",
    );
    repo.write(
        "Bar.php",
        "<?php\nclass Bar { public function __construct() {} }\n",
    );
    repo.write("Use.php", "<?php\n$f = new Foo();\n");

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "Foo.__construct"),
        "new Foo() があるため Foo.__construct は live: {names:?}"
    );
    assert!(
        contains(&names, "Bar.__construct"),
        "new されない Bar.__construct は dead に出るべき: {names:?}"
    );
}

/// `new $var()` (動的クラス名) は owner を静的解決できないため Ambiguous に倒し、
/// duplicate set 全体を旧スキップへフォールバックさせる。
#[test]
fn dead_code_php_construct_dynamic_new_stays_ambiguous() {
    let repo = TestRepo::new();

    repo.write(
        "Foo.php",
        "<?php\nclass Foo { public function __construct() {} }\n",
    );
    repo.write(
        "Bar.php",
        "<?php\nclass Bar { public function __construct() {} }\n",
    );
    repo.write("Use.php", "<?php\n$cls = 'Foo';\n$f = new $cls();\n");

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "Foo.__construct") && !contains(&names, "Bar.__construct"),
        "動的 new は Ambiguous に倒して両 constructor を旧スキップ維持: {names:?}"
    );
}

/// `use Lib\Beta as B; B::fmt();` の alias 経由静的呼び出しを実クラスへ解決して
/// 票を入れる (Issue 2026-07-10-php-use-alias-static-call-dead-fp)。
#[test]
fn dead_code_php_use_alias_static_call_counts_target_owner() {
    let repo = TestRepo::new();

    repo.write(
        "Alpha.php",
        "<?php\nnamespace Lib;\nclass Alpha { public function fmt() { return 'a'; } }\n",
    );
    repo.write(
        "Beta.php",
        "<?php\nnamespace Lib;\nclass Beta { public static function fmt() { return 'b'; } }\n",
    );
    repo.write("Consumer.php", "<?php\nuse Lib\\Beta as B;\nB::fmt();\n");

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "Beta.fmt"),
        "B::fmt() は alias 解決で Beta.fmt への確定参照になるべき: {names:?}"
    );
    assert!(
        contains(&names, "Alpha.fmt"),
        "未参照の Alpha.fmt は dead のまま: {names:?}"
    );
}

/// grouped use (`use Lib\{Beta as B};`) の alias も解決される。
#[test]
fn dead_code_php_grouped_use_alias_static_call_counts_target_owner() {
    let repo = TestRepo::new();

    repo.write(
        "Alpha.php",
        "<?php\nnamespace Lib;\nclass Alpha { public function fmt() { return 'a'; } }\n",
    );
    repo.write(
        "Beta.php",
        "<?php\nnamespace Lib;\nclass Beta { public static function fmt() { return 'b'; } }\n",
    );
    repo.write("Consumer.php", "<?php\nuse Lib\\{Beta as B};\nB::fmt();\n");

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "Beta.fmt"),
        "grouped use の alias 経由 B::fmt() も Beta.fmt への確定参照になるべき: {names:?}"
    );
}

/// 同一 alias 名が競合するファイルでは alias 解決を諦め、alias 名への呼び出しを
/// Ambiguous に倒す (誤帰属より旧スキップ)。
#[test]
fn dead_code_php_conflicting_alias_stays_ambiguous() {
    let repo = TestRepo::new();

    repo.write(
        "Alpha.php",
        "<?php\nnamespace Lib;\nclass Alpha { public function fmt() { return 'a'; } }\n",
    );
    repo.write(
        "Beta.php",
        "<?php\nnamespace Lib;\nclass Beta { public static function fmt() { return 'b'; } }\n",
    );
    // 実 PHP ではコンパイルエラーだが、防御的に競合 alias は解決不能として扱う
    repo.write(
        "Consumer.php",
        "<?php\nuse Lib\\Beta as B;\nuse Other\\Thing as B;\nB::fmt();\n",
    );

    let names = dead_names(&repo);
    assert!(
        !contains(&names, "Beta.fmt") && !contains(&names, "Alpha.fmt"),
        "競合 alias は Ambiguous に倒して両 owner を旧スキップ維持: {names:?}"
    );
}
