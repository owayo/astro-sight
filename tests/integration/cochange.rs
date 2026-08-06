//! cochange サブコマンド (共変更検出) の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

// ---- Co-change analysis tests (blame mode) ----

#[test]
fn cochange_blame_runs_with_explicit_paths() {
    // blame モードが JSON を返すことを確認する。CI の shallow clone (fetch-depth=1)
    // でも安定動作させるため、`--paths` で起点を明示し、`git diff <base> HEAD`
    // が解決できないケースでも `collect_blame_commits_for_file` 内で空集合を
    // 返して `commits_analyzed=0` で正常終了する経路を踏ませる。
    let output = cargo_bin()
        .args([
            "cochange",
            "--dir",
            ".",
            "--paths",
            "src/main.rs",
            "--min-confidence",
            "0.0",
            "--min-samples",
            "1",
            "--min-denominator",
            "1",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(json["entries"].as_array().is_some());
    assert!(json["commits_analyzed"].as_u64().is_some());
}

/// CLI 表面: `--min-score` / `--history-limit` が受理され、`diagnostics` が
/// JSON に出ること。0 件でも「共変更なし」と「解析できなかった」を区別できる。
#[test]
fn cochange_cli_exposes_min_score_history_limit_and_diagnostics() {
    let output = cargo_bin()
        .args([
            "cochange",
            "--dir",
            ".",
            "--paths",
            "src/main.rs",
            "--min-score",
            "0.0",
            "--history-limit",
            "5",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let diag = &json["diagnostics"];
    assert_eq!(
        diag["sources_requested"].as_u64(),
        Some(1),
        "起点 1 件が診断に出ること: {json}"
    );
    // history_limit=5 の fallback が効いて証拠が作られる (CI の shallow clone では
    // 履歴が浅く 0 件もあり得るため、キーの存在だけを固定する)
    assert!(diag["sources_with_history_evidence"].as_u64().is_some());
    assert!(diag["sources_with_blame_evidence"].as_u64().is_some());
}

/// `--min-score` は 0.0..=1.0 の範囲外を拒否する (min_confidence と同じ検証系列)。
#[test]
fn cochange_rejects_out_of_range_min_score() {
    let output = cargo_bin()
        .args([
            "cochange",
            "--dir",
            ".",
            "--paths",
            "src/main.rs",
            "--min-score",
            "1.5",
        ])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("min_score"),
        "min_score の範囲エラーが返るべき: {msg}"
    );
}

/// `--git` 指定で差分が全て除外 glob (vendor/dist 等) に該当した場合は、
/// 入力エラーではなく「解析対象なし」として exit 0 + 空結果を返す。
/// 生成物だけを触ったコミットで cochange / review 全体が落ちないようにする。
#[test]
fn cochange_git_with_only_excluded_files_returns_empty_not_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(repo.join("seed.rs"), "// seed\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "seed"]);
    // HEAD で vendor/ 配下だけを変更 (既定除外 glob に全件該当)
    std::fs::create_dir_all(repo.join("vendor")).unwrap();
    std::fs::write(repo.join("vendor/lib.php"), "<?php\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "vendor only"]);

    let output = cargo_bin()
        .args(["cochange", "--dir", repo.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "全件除外は exit 0 であるべき。stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["entries"].as_array().map(Vec::len), Some(0));
    assert_eq!(json["commits_analyzed"].as_u64(), Some(0));
}

#[test]
fn cochange_rejects_missing_source_files() {
    // --git / --paths のいずれも指定しない場合は InvalidRequest で拒否される。
    let output = cargo_bin()
        .args(["cochange", "--dir", "."])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("--git") || msg.contains("--paths") || msg.contains("source files"),
        "expected source-file requirement message, got: {msg}"
    );
}

#[test]
fn cochange_rejects_invalid_confidence() {
    // CI の shallow clone でも `--git` 非依存で min_confidence 検証が走るよう
    // `--paths` で起点を明示する (これが無いと resolve_blame_source_files の
    // git diff が先にエラー化し、エラーメッセージが "min_confidence" を含まない)。
    let output = cargo_bin()
        .args([
            "cochange",
            "--dir",
            ".",
            "--paths",
            "src/main.rs",
            "--min-confidence",
            "1.5",
        ])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("min_confidence")
    );
}

#[test]
fn cochange_rejects_invalid_smoothing_priors() {
    for (arg, expected) in [
        ("--smoothing-alpha=-1", "smoothing_alpha"),
        ("--smoothing-beta=-1", "smoothing_beta"),
    ] {
        let output = cargo_bin()
            .args(["cochange", "--dir", ".", "--paths", "src/main.rs", arg])
            .output()
            .expect("failed to run");
        assert!(!output.status.success(), "{arg} should fail");

        let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "expected {expected} in error message, got: {json}"
        );
    }
}

// ---- cochange 入力検証テスト ----

#[test]
fn cochange_rejects_nan_confidence() {
    // NaN は拒否される (clap または service 層で)。
    // CI の shallow clone 非依存にするため `--paths` で起点を明示する。
    let output = cargo_bin()
        .args([
            "cochange",
            "--dir",
            ".",
            "--paths",
            "src/main.rs",
            "--min-confidence",
            "NaN",
        ])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());
}

/// `cochange --git` は未コミットの作業ツリー変更を起点にする。
///
/// 旧実装は `git diff --name-only <base> HEAD` (2 revision 形式) で起点を集めており、
/// 既定 base=HEAD~1 と合わせて「`--git` を付けても未コミット変更を一切見ない」状態だった。
/// context / impact / review / dead-code はいずれも `git diff <base>` (作業ツリー比較) で
/// 未コミット変更を見るため、同じ `--git` でも cochange だけ解析対象がずれていた。
#[test]
fn cochange_git_picks_up_uncommitted_working_tree_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);

    // a.rs と b.rs を 3 回同時変更して履歴を作る
    for i in 0..3 {
        std::fs::write(repo.join("a.rs"), format!("fn a() {{ {i} }}\n")).unwrap();
        std::fs::write(repo.join("b.rs"), format!("fn b() {{ {i} }}\n")).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "pair"]);
    }
    // a.rs を未コミットで書き換える (コミットしない)
    std::fs::write(repo.join("a.rs"), "fn a() { 99 }\n").unwrap();

    let output = cargo_bin()
        .args(["cochange", "--dir", repo.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let diag = &json["diagnostics"];
    assert_eq!(
        diag["sources_requested"].as_u64(),
        Some(1),
        "未コミットの a.rs が起点になること: {json}"
    );
    let entries = json["entries"].as_array().expect("entries");
    assert!(
        entries.iter().any(|e| e["file_b"] == "b.rs"),
        "共変更相手 b.rs が出ること: {json}"
    );
}

/// 作業ツリーがクリーンなら `cochange --git` は空を返す (review / impact と同じ既定)。
#[test]
fn cochange_git_is_empty_on_clean_worktree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);

    let output = cargo_bin()
        .args(["cochange", "--dir", repo.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["entries"].as_array().map(Vec::len), Some(0));
    assert_eq!(json["commits_analyzed"].as_u64(), Some(0));
}
