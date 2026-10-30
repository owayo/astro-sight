//! `--git` の解析が起動する git の回数の退行を検出する。
//!
//! Windows では git の起動 1 回が数十 ms かかり (`git.exe` が本体の git を起動し直す)、Stop hook と
//! CI のテストの両方がこれを払う。コミットやファイルごとに git を起動し直したり、同じ問い合わせを
//! 1 回の実行の中で繰り返したりすると、変更の規模に比例して遅くなる。
//!
//! 起動は Trace2 の `start` イベントで数える。環境変数 (`GIT_TRACE2_EVENT`) ではなくテスト専用の
//! HOME の `.gitconfig` で有効にするのは、staged の一時リポジトリへの git が `GIT_*` を消して
//! 起動されるため (`isolated_git_command`)。git が自分で起動した子の git は数えない。

use std::collections::BTreeMap;

use super::support::{TestRepo, cargo_bin};

struct TracedRun {
    exit: Option<i32>,
    stderr: String,
    /// 起動した git のサブコマンドと引数 (先頭の `-c <設定>` は除く)。
    commands: Vec<Vec<String>>,
}

impl TracedRun {
    /// サブコマンドと、続く引数がオプションならその 1 つ目をキーにした起動回数
    /// (`diff --relative` と `diff --no-ext-diff` を分け、revision や SHA は含めない)。
    fn counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for argv in &self.commands {
            let mut key = argv[0].clone();
            if let Some(option) = argv.get(1).filter(|arg| arg.starts_with('-')) {
                key.push(' ');
                key.push_str(option);
            }
            *counts.entry(key).or_default() += 1;
        }
        counts
    }

    fn listing(&self) -> String {
        self.commands
            .iter()
            .map(|argv| argv.join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn traced_run(repo: &TestRepo, args: &[&str]) -> TracedRun {
    let trace = tempfile::tempdir().expect("trace dir");
    let home = trace.path().join("home");
    let events = trace.path().join("events");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&events).expect("events");
    // git の設定ファイルでは `\` がエスケープなので、Windows のパスも `/` 区切りで書く。
    std::fs::write(
        home.join(".gitconfig"),
        format!(
            "[trace2]\n\teventTarget = {}\n\teventBrief = true\n",
            events.to_string_lossy().replace('\\', "/")
        ),
    )
    .expect("gitconfig");
    let mut command = cargo_bin();
    command.args(args).arg("--dir").arg(repo.root());
    // 実行環境の Trace2 や global 設定の指定が、テスト用の HOME の設定より優先されないようにする。
    for name in [
        "GIT_CONFIG_GLOBAL",
        "GIT_TRACE2",
        "GIT_TRACE2_EVENT",
        "GIT_TRACE2_PERF",
        "XDG_CONFIG_HOME",
    ] {
        command.env_remove(name);
    }
    let output = command
        .env("HOME", &home)
        .output()
        .expect("failed to run astro-sight");

    let mut commands = Vec::new();
    for entry in std::fs::read_dir(&events).expect("events") {
        let text = std::fs::read_to_string(entry.expect("entry").path()).expect("event file");
        for line in text.lines() {
            let event: serde_json::Value = serde_json::from_str(line).expect("trace2 json");
            // git が起動した子の git は sid が `<親>/<子>` になる。
            if event["event"] != "start"
                || event["sid"].as_str().is_none_or(|sid| sid.contains('/'))
            {
                continue;
            }
            let mut argv: Vec<String> = event["argv"]
                .as_array()
                .expect("argv")
                .iter()
                .skip(1)
                .map(|arg| arg.as_str().expect("arg").to_owned())
                .collect();
            while argv.first().is_some_and(|arg| arg == "-c") {
                argv.drain(..2);
            }
            commands.push(argv);
        }
    }
    TracedRun {
        exit: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        commands,
    }
}

/// a / b / c / d を 6 回一緒に変えてきた履歴の上で、a / b / c だけを変える。
/// review は d を共変更の候補に挙げる (証拠は 6 コミット、起点は 3 ファイル)。
fn repo_with_cochange_history() -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    for i in 0..6 {
        for name in ["a", "b", "c", "d"] {
            repo.write(
                format!("src/{name}.rs"),
                format!("pub fn {name}() -> i32 {{\n    {i}\n}}\n"),
            );
        }
        repo.commit_all(&format!("change {i}"));
    }
    for name in ["a", "b", "c"] {
        repo.write(
            format!("src/{name}.rs"),
            format!("pub fn {name}() -> i32 {{\n    10\n}}\n"),
        );
    }
    repo
}

fn counts(entries: &[(&str, usize)]) -> BTreeMap<String, usize> {
    entries
        .iter()
        .map(|(key, count)| ((*key).to_owned(), *count))
        .collect()
}

/// 共変更の証拠コミットの変更ファイル (`diff-tree`) と起点の変更行 (`diff --unified=0`) は、
/// コミットやファイルの数に関わらず 1 回ずつにまとめる。worktree の判定・base の解決・prefix は
/// 1 回の rev-parse で取り、review の Scope は prefix を問い合わせ直さない。2 回起動するのは、
/// 解析後に入力が変わっていないかを確かめる読み直し (差分と未追跡の一覧) だけ。
#[test]
fn review_git_hook_starts_each_git_query_once() {
    let repo = repo_with_cochange_history();
    let run = traced_run(&repo, &["review", "--git", "--hook"]);
    assert_eq!(run.exit, Some(0), "{}", run.stderr);
    assert!(
        run.stderr.contains("\"f\":\"src/d.rs\""),
        "前提: 共変更の解析まで進む: {}",
        run.stderr
    );
    assert_eq!(
        run.counts(),
        counts(&[
            // 起点ごと (3 ファイル)。blame の証拠が 1 コミットなので履歴へ広げる。
            ("blame --line-porcelain", 3),
            ("log --format=%H%x00%aE%x00%at", 3),
            ("cat-file --batch", 1),
            // 起点と候補の生成物判定
            ("check-attr -z", 2),
            // 起点 3 ファイルの変更行 (まとめて 1 回)
            ("diff --no-ext-diff", 1),
            // 差分の取得と解析後の確認
            ("diff --relative", 2),
            ("ls-files --others", 2),
            // 証拠 6 コミットの変更ファイル (まとめて 1 回)
            ("diff-tree --stdin", 1),
            ("ls-files --stage", 1),
            ("ls-tree --full-tree", 1),
            ("ls-tree -r", 1),
            ("rev-list --no-walk", 1),
            // 差分の取得: worktree の判定・base の解決・prefix
            ("rev-parse --is-inside-work-tree", 1),
            // cochange のエンジン (canonicalize した dir で動くので別に取る)
            ("rev-parse --show-prefix", 1),
            // 先行コミットの確認 (upstream が無いのでここで止まる)
            ("rev-parse --is-shallow-repository", 1),
            ("rev-parse --abbrev-ref", 1),
        ]),
        "{}",
        run.listing()
    );
}

/// staged の review は一時リポジトリに index を書き出して解析する。元のリポジトリの値は 2 回の
/// rev-parse で読み、Scope は base と index の tree の一覧で prefix を共有する。
#[test]
fn staged_review_hook_starts_each_git_query_once() {
    let repo = repo_with_cochange_history();
    repo.stage_all();
    let run = traced_run(&repo, &["review", "--git", "--staged", "--hook"]);
    assert_eq!(run.exit, Some(0), "{}", run.stderr);
    assert!(
        run.stderr.contains("\"f\":\"src/d.rs\""),
        "前提: 共変更の解析まで進む: {}",
        run.stderr
    );
    assert_eq!(
        run.counts(),
        counts(&[
            ("blame --line-porcelain", 3),
            ("log --format=%H%x00%aE%x00%at", 3),
            // 一時リポジトリの index tree と、API 差分の base
            ("cat-file --batch", 2),
            ("check-attr -z", 2),
            ("diff --no-ext-diff", 1),
            // staged の差分と作業ツリーの差分をそれぞれ、取得・解析前の確認・解析後の確認で 3 回ずつ。
            // 残る 1 回は一時リポジトリが index と一致するかの確認。
            ("diff --relative", 7),
            ("ls-files --others", 4),
            ("diff-tree --stdin", 1),
            // 一時リポジトリの作成
            ("init --quiet", 1),
            ("write-tree", 1),
            ("update-ref", 1),
            ("read-tree", 1),
            ("checkout-index --stdin", 1),
            ("rev-parse --git-path", 1),
            ("rev-parse --shared-index-path", 1),
            // Scope の base と index tree の一覧
            ("ls-tree --full-tree", 2),
            // 一時リポジトリの一覧と、cochange のエンジンの base の一覧
            ("ls-tree -r", 2),
            ("rev-list --no-walk", 1),
            ("rev-parse --is-inside-work-tree", 1),
            // Scope (一時リポジトリ) とエンジン
            ("rev-parse --show-prefix", 2),
            ("rev-parse --is-shallow-repository", 1),
            ("rev-parse --abbrev-ref", 1),
        ]),
        "{}",
        run.listing()
    );
}

/// staged の変更が無ければ、作業ツリーの差分を記録した後の確認は 1 回だけ (短絡の直前) にする。
#[test]
fn staged_review_without_staged_changes_checks_input_once() {
    let repo = repo_with_cochange_history();
    let run = traced_run(&repo, &["review", "--git", "--staged", "--hook"]);
    assert_eq!(run.exit, Some(0), "{}", run.stderr);
    assert_eq!(
        run.counts(),
        counts(&[
            ("diff --relative", 4),
            ("ls-files --others", 2),
            ("rev-parse --is-inside-work-tree", 1),
        ]),
        "{}",
        run.listing()
    );
}

/// `impact` / `context` の `--git` は、rev-parse 1 回・差分 1 回・未追跡の一覧 1 回だけ。
#[test]
fn impact_and_context_git_start_three_git_processes() {
    let repo = repo_with_cochange_history();
    for subcommand in ["impact", "context"] {
        let run = traced_run(&repo, &[subcommand, "--git"]);
        assert_eq!(run.exit, Some(0), "{subcommand}: {}", run.stderr);
        assert_eq!(
            run.counts(),
            counts(&[
                ("diff --relative", 1),
                ("ls-files --others", 1),
                ("rev-parse --is-inside-work-tree", 1),
            ]),
            "{subcommand}: {}",
            run.listing()
        );
    }
}
