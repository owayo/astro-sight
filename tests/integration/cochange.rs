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

/// `--paths ./src/a.rs` のような同値表記で「自分自身との共変更 confidence 1.0」が
/// 出ないこと。
///
/// engine 側の起点除外は生の文字列一致だが、候補は `git diff-tree --name-only` 由来の
/// 正規形なので、`./` 付きや連続スラッシュを素通しすると起点自身が候補に残り、
/// 分母 = 分子で最上位に並ぶ。`is_contained_relative` は `Component::CurDir` を
/// 正当な入力として許可しているため、検証だけでは防げなかった。
/// MCP の `cochange_analyze` はエージェントが `source_files` を組み立てるため
/// 実運用で踏みやすい。
#[test]
fn cochange_normalizes_source_paths_and_excludes_self_pair() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    // a.rs と b.rs を必ず一緒に変更するコミットを積む (共変更の相手を作る)。
    for i in 0..3 {
        repo.write("src/a.rs", format!("pub fn a() {{ let _ = {i}; }}\n"));
        repo.write("src/b.rs", format!("pub fn b() {{ let _ = {i}; }}\n"));
        repo.commit_all(&format!("change {i}"));
    }
    // 起点となる未コミット変更。
    repo.write("src/a.rs", "pub fn a() { let _ = 99; }\n");

    // 同値表記はすべて同じ結果になり、いずれも起点自身を候補にしない。
    for spelling in ["src/a.rs", "./src/a.rs", "src//a.rs", "src/./a.rs"] {
        let json = repo.run_json(
            "cochange",
            &[
                "--paths",
                spelling,
                "--min-confidence",
                "0.0",
                "--min-samples",
                "1",
                "--min-denominator",
                "1",
            ],
        );
        let entries = json["entries"].as_array().expect("entries array");
        let partners: Vec<&str> = entries
            .iter()
            .map(|e| e["file_b"].as_str().unwrap_or_default())
            .collect();
        assert!(
            !partners
                .iter()
                .any(|p| p.trim_start_matches("./").replace("//", "/") == "src/a.rs"),
            "{spelling}: 起点自身を共変更相手にしないこと: {partners:?}"
        );
        // 対照: 本来の共変更相手は引き続き検出する (抑制しすぎていないこと)。
        assert!(
            partners.contains(&"src/b.rs"),
            "{spelling}: 本来の共変更相手 src/b.rs は検出されること: {partners:?}"
        );
    }
}

/// 同値表記の先勝ち dedup と `--max-source-files` の関係を固定する。
///
/// 上限は「指定件数」ではなく **正規化後の unique 件数**に対して掛かる。
/// `["src/a.rs", "./src/a.rs"]` は 1 件に畳まれるので上限 1 でも通り、
/// 別ファイルを足して 2 件になると上限 1 で拒否される。
#[test]
fn cochange_dedups_equivalent_paths_before_max_source_files() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.write("src/b.rs", "pub fn b() {}\n");
    repo.commit_all("init");
    repo.write("src/a.rs", "pub fn a() { let _ = 1; }\n");

    // 同一ファイルの 2 表記 → unique 1 件なので上限 1 を超えない。
    let out = cargo_bin()
        .args(["cochange", "--dir"])
        .arg(repo.root())
        .args([
            "--paths",
            "src/a.rs,./src/a.rs",
            "--max-source-files",
            "1",
            "--min-samples",
            "1",
            "--min-denominator",
            "1",
        ])
        .output()
        .expect("failed to run cochange");
    assert!(
        out.status.success(),
        "同値表記は先勝ちで畳まれるので上限 1 を超えない: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 対照: 別ファイルを足すと unique 2 件になり上限 1 で拒否される。
    let out = cargo_bin()
        .args(["cochange", "--dir"])
        .arg(repo.root())
        .args([
            "--paths",
            "src/a.rs,src/b.rs",
            "--max-source-files",
            "1",
            "--min-samples",
            "1",
            "--min-denominator",
            "1",
        ])
        .output()
        .expect("failed to run cochange");
    assert!(
        !out.status.success(),
        "unique 2 件は上限 1 を超えるので拒否されること"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("max-source-files") || combined.contains("max_source_files"),
        "上限超過であることが分かるエラーを返すこと: {combined}"
    );
}

/// 過去のコミットで削除済みのファイルを共変更候補として提案しないこと。
///
/// 共変更の候補は git 履歴から集めるため、削除済みファイルも候補に上がる。履歴上の
/// 共変更頻度は事実だが、現在は開けるファイルが無いのでアクションが取れない。
/// レポート (2026-08-28-cochange-deleted-path) の最小再現手順をそのまま fixture 化する。
///
/// 対照として「生存している共変更相手は従来どおり提案される」も同じテストで固定する
/// (削除済み除外が効きすぎて本物の候補まで消していないこと)。
#[test]
fn cochange_excludes_candidates_deleted_before_base() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    // a.rs / b.rs / gone.rs を毎回まとめて変更するコミットを 5 回積む。
    for i in 0..5 {
        repo.write("src/a.rs", format!("pub fn a() {{ let _ = {i}; }}\n"));
        repo.write("src/b.rs", format!("pub fn b() {{ let _ = {i}; }}\n"));
        repo.write("src/gone.rs", format!("pub fn gone() {{ let _ = {i}; }}\n"));
        repo.commit_all(&format!("change {i}"));
    }
    // gone.rs だけを削除する (a.rs は触らないので、a.rs の blame は直前の
    // 「3 ファイルまとめて変更」コミットを指したまま)。
    repo.remove_file("src/gone.rs");
    repo.commit_all("remove gone");
    // 起点となる未コミット変更。
    repo.write("src/a.rs", "pub fn a() { let _ = 99; }\n");

    let json = repo.run_json(
        "cochange",
        &[
            "--paths",
            "src/a.rs",
            "--min-confidence",
            "0.0",
            "--min-samples",
            "1",
            "--min-denominator",
            "1",
            "--history-limit",
            "20",
        ],
    );
    let partners: Vec<&str> = json["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .map(|e| e["file_b"].as_str().unwrap_or_default())
        .collect();

    assert!(
        !partners.contains(&"src/gone.rs"),
        "削除済みファイルを共変更候補にしないこと: {partners:?}"
    );
    // 対照: 生存している相手は引き続き提案される (抑制しすぎていないこと)。
    assert!(
        partners.contains(&"src/b.rs"),
        "生存している共変更相手は引き続き提案されること: {partners:?}"
    );
    // 落とした件数を診断で申告する (黙って消さない)。
    let diag = &json["diagnostics"];
    assert!(
        diag["filtered_deleted_candidates"].as_u64().unwrap_or(0) > 0,
        "削除済み候補を落としたことを diagnostics で申告すること: {diag}"
    );
    let reasons: Vec<&str> = diag["reasons"]
        .as_array()
        .map(|a| a.iter().filter_map(|r| r.as_str()).collect())
        .unwrap_or_default();
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("candidate_deleted_at_base")),
        "reasons に candidate_deleted_at_base が含まれること: {reasons:?}"
    );
}
