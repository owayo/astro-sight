use super::support::TestRepo;

// ---------------------------------------------------------------------------
// hidden diff 候補ファイルの member liveness 回帰テスト
// ---------------------------------------------------------------------------

fn dead_names(repo: &TestRepo, args: &[&str]) -> Vec<String> {
    repo.run_json("dead-code", args)["dead_symbols"]
        .as_array()
        .expect("dead_symbols should be an array")
        .iter()
        .filter_map(|symbol| symbol["name"].as_str().map(str::to_owned))
        .collect()
}

fn contains(names: &[String], expected: &str) -> bool {
    names.iter().any(|name| name == expected)
}

/// hidden 配下 (`.tools/`) の diff 候補ファイルは workspace walk から漏れるため、
/// duplicate member の owner 別票読み (member liveness) が hidden 内の
/// `AlphaSvc::runJob()` を見られず dead 誤検出していた。候補ファイルを extra_files
/// として走査集合へ合流させる修正の回帰テスト (PHP 経路)。
#[test]
fn dead_code_diff_hidden_candidate_member_liveness_sees_hidden_refs() {
    let repo = TestRepo::new();
    repo.create_dir_all(".tools");
    repo.create_dir_all("src");
    repo.write("README.md", "# fixture\n");
    repo.init_git();
    repo.commit_all("initial");

    // hidden 配下: AlphaSvc の定義と同ファイル内の静的呼び出し
    repo.write(
        ".tools/dup.php",
        "<?php\nclass AlphaSvc { public static function runJob(): void {} }\nAlphaSvc::runJob();\n",
    );
    // 可視側: duplicate set を形成する BetaSvc
    repo.write(
        "src/beta.php",
        "<?php\nclass BetaSvc { public static function runJob(): void {} }\nBetaSvc::runJob();\n",
    );
    // 両ファイルを staged にして diff 候補へ乗せる (commit しない)
    repo.stage_all();

    let names = dead_names(&repo, &["--git", "--staged"]);
    assert!(
        !contains(&names, "AlphaSvc.runJob") && !contains(&names, "BetaSvc.runJob"),
        "hidden 配下の候補ファイル内 `AlphaSvc::runJob()` も member liveness の票に入るべき: {names:?}"
    );
}

/// 同上の TS 経路版。hidden 配下の `AlphaSvc.runJob();` (member access) が
/// JsTsMemberLiveness の走査集合に合流し dead 誤検出しないことを検証する。
#[test]
fn dead_code_diff_hidden_candidate_member_liveness_sees_hidden_refs_ts() {
    let repo = TestRepo::new();
    repo.create_dir_all(".tools");
    repo.create_dir_all("src");
    repo.write("README.md", "# fixture\n");
    repo.init_git();
    repo.commit_all("initial");

    repo.write(
        ".tools/dup.ts",
        "export class AlphaSvc {\n    static runJob(): void {}\n}\nAlphaSvc.runJob();\n",
    );
    repo.write(
        "src/beta.ts",
        "export class BetaSvc {\n    static runJob(): void {}\n}\nBetaSvc.runJob();\n",
    );
    repo.stage_all();

    let names = dead_names(&repo, &["--git", "--staged"]);

    assert!(
        !contains(&names, "AlphaSvc.runJob") && !contains(&names, "BetaSvc.runJob"),
        "hidden 配下の候補ファイル内 `AlphaSvc.runJob()` も member liveness の票に入るべき: {names:?}"
    );
}

/// decorator とメソッド定義の間にコメント行が挟まると decorator 蓄積がクリアされ、
/// `@HostListener` 付きメソッドが dead 誤検出されていた回帰テスト
/// (comment ノードは蓄積を維持したまま読み飛ばす)。
#[test]
fn dead_code_angular_decorator_with_comment_between_is_excluded() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/app");
    let component_src = "\
import { Component, HostListener } from '@angular/core';
@Component({ template: '' })
export class AppComponent {
    @HostListener('window:beforeunload', ['$event'])
    // NOTE: decorator とメソッドの間に挟まったコメント
    onBeforeUnload() {}
    plainHelper() {}
}
";
    repo.write("src/app/app.component.ts", component_src);
    let dead_names = dead_names(&repo, &[]);
    assert!(
        !dead_names.iter().any(|n| n.contains("onBeforeUnload")),
        "コメントを挟んだ @HostListener 付きメソッドも dead から除外されるべき: {dead_names:?}"
    );
    // member decorator が無い通常メソッドは除外対象外 (回帰担保)。
    assert!(
        dead_names.iter().any(|n| n.contains("plainHelper")),
        "member decorator のない通常メソッドは dead として残るべき (回帰担保): {dead_names:?}"
    );
}
