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
