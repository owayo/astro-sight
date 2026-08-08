//! `--format json|toon` と `config.toml` の `format` の統合テスト。
//!
//! 検証の軸は 3 つ:
//! 1. 既定 (JSON) の出力が **1 バイトも変わっていない** こと
//! 2. TOON がデータ面 (単一ドキュメント / バッチ / MCP text content) で有効なこと
//! 3. プロトコル面 (session / hook / エラー) が JSON 固定であること

use super::support::{TestRepo, cargo_bin};

fn sample_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.write(
        "a.rs",
        "pub const MAX: usize = 10;\n\
         pub fn alpha(x: usize) -> usize {\n    if x > 0 { x } else { MAX }\n}\n\
         pub fn beta() -> usize {\n    alpha(1)\n}\n",
    );
    repo.write("b.rs", "pub fn gamma() -> usize {\n    2\n}\n");
    repo
}

fn run(repo: &TestRepo, args: &[&str]) -> std::process::Output {
    cargo_bin()
        .args(args)
        .current_dir(repo.root())
        .output()
        .expect("failed to run astro-sight")
}

fn stdout_of(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "astro-sight failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("stdout is not UTF-8")
}

// ---------------------------------------------------------------------------
// 既定 (JSON) の互換性
// ---------------------------------------------------------------------------

#[test]
fn default_output_is_unchanged_compact_json() {
    let repo = sample_repo();
    let implicit = stdout_of(&run(&repo, &["symbols", "--path", "a.rs"]));
    let explicit = stdout_of(&run(
        &repo,
        &["symbols", "--path", "a.rs", "--format", "json"],
    ));

    assert_eq!(implicit, explicit);
    assert!(implicit.starts_with('{'), "unexpected output: {implicit}");
    serde_json::from_str::<serde_json::Value>(implicit.trim()).expect("valid JSON");
}

#[test]
fn pretty_still_applies_to_json_only() {
    let repo = sample_repo();
    let pretty = stdout_of(&run(&repo, &["symbols", "--path", "a.rs", "--pretty"]));
    assert!(pretty.contains("\n  \""), "pretty JSON expected: {pretty}");

    // TOON には整形の概念が無いので `--pretty` は無視される (エラーにはしない)。
    let toon = stdout_of(&run(
        &repo,
        &["symbols", "--path", "a.rs", "--format", "toon"],
    ));
    let toon_pretty = stdout_of(&run(
        &repo,
        &["symbols", "--path", "a.rs", "--format", "toon", "--pretty"],
    ));
    assert_eq!(toon, toon_pretty);
}

#[test]
fn unknown_format_is_rejected_by_the_argument_parser() {
    let repo = sample_repo();
    let output = run(&repo, &["symbols", "--path", "a.rs", "--format", "yaml"]);
    assert!(!output.status.success());
}

// ---------------------------------------------------------------------------
// データ面の TOON
// ---------------------------------------------------------------------------

#[test]
fn toon_single_document_is_indentation_based() {
    let repo = sample_repo();
    let toon = stdout_of(&run(
        &repo,
        &["symbols", "--path", "a.rs", "--format", "toon"],
    ));

    assert!(toon.starts_with("path: a.rs\n"), "unexpected: {toon}");
    assert!(toon.contains("lang: rust\n"), "unexpected: {toon}");
    assert!(toon.contains("symbols[3]"), "unexpected: {toon}");
    // stdout は行指向ツールとの相互運用のため改行 1 個で終端する。
    assert!(toon.ends_with('\n'));
    assert!(!toon.ends_with("\n\n"));
    for line in toon.lines() {
        assert_eq!(line, line.trim_end(), "trailing whitespace: {line:?}");
        assert!(!line.contains('\t'), "tab in indentation: {line:?}");
    }
}

#[test]
fn toon_uniform_arrays_use_tabular_form() {
    let repo = sample_repo();
    // refs の references は全要素が同じキー集合なので tabular になる。
    let toon = stdout_of(&run(
        &repo,
        &["refs", "--name", "alpha", "--dir", ".", "--format", "toon"],
    ));
    assert!(
        toon.contains("refs[") && toon.contains("]{"),
        "expected a tabular header: {toon}"
    );
}

#[test]
fn toon_batch_emits_one_root_array_document() {
    let repo = sample_repo();
    let toon = stdout_of(&run(
        &repo,
        &[
            "symbols", "--dir", ".", "--glob", "**/*.rs", "--format", "toon",
        ],
    ));

    // ルート配列ヘッダが 1 行目、要素は `  - ` で始まる list item。
    let mut lines = toon.lines();
    let header = lines.next().expect("header line");
    assert!(
        header.starts_with('[') && header.ends_with("]:"),
        "unexpected header: {header}"
    );
    let declared: usize = header
        .trim_start_matches('[')
        .trim_end_matches("]:")
        .parse()
        .expect("array length in header");
    let items = toon.lines().filter(|l| l.starts_with("  - ")).count();
    assert_eq!(
        declared, items,
        "header length must match item count: {toon}"
    );
    assert_eq!(declared, 2, "two source files expected: {toon}");
}

#[test]
fn json_batch_stays_ndjson() {
    let repo = sample_repo();
    let json = stdout_of(&run(&repo, &["symbols", "--dir", ".", "--glob", "**/*.rs"]));
    for line in json.lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("each line is a JSON document");
    }
}

#[test]
fn toon_refs_batch_becomes_a_single_document() {
    let repo = sample_repo();
    let json = stdout_of(&run(
        &repo,
        &["refs", "--names", "alpha,gamma", "--dir", "."],
    ));
    // JSON は従来どおり 1 行 1 レコードの NDJSON。
    assert_eq!(json.trim().lines().count(), 2, "unexpected: {json}");

    let toon = stdout_of(&run(
        &repo,
        &[
            "refs",
            "--names",
            "alpha,gamma",
            "--dir",
            ".",
            "--format",
            "toon",
        ],
    ));
    assert!(toon.starts_with("[2]"), "unexpected: {toon}");
}

// ---------------------------------------------------------------------------
// プロトコル面は JSON 固定
// ---------------------------------------------------------------------------

#[test]
fn explicit_toon_is_rejected_for_the_session_protocol() {
    let repo = sample_repo();
    let output = run(&repo, &["session", "--format", "toon"]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("error output stays JSON");
    assert_eq!(value["error"]["code"], "INVALID_REQUEST");
}

#[test]
fn explicit_toon_is_rejected_for_hook_output() {
    let repo = sample_repo();
    repo.init_git();
    repo.commit_all("init");
    repo.write("b.rs", "pub fn gamma() -> usize {\n    3\n}\n");

    for args in [
        vec![
            "review", "--dir", ".", "--git", "--hook", "--format", "toon",
        ],
        vec![
            "impact", "--dir", ".", "--git", "--hook", "--format", "toon",
        ],
    ] {
        let output = run(&repo, &args);
        assert!(!output.status.success(), "should reject: {args:?}");
    }
}

#[test]
fn errors_stay_json_even_when_toon_is_requested() {
    let repo = sample_repo();
    // エラー封筒は機械可読契約なので `--format` に従わせない。
    let output = run(
        &repo,
        &["symbols", "--path", "missing.rs", "--format", "toon"],
    );
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("error output stays JSON");
    assert_eq!(value["error"]["code"], "FILE_NOT_FOUND");
}

// ---------------------------------------------------------------------------
// config.toml
// ---------------------------------------------------------------------------

#[test]
fn config_file_sets_the_default_format() {
    let repo = sample_repo();
    repo.write("astro-sight.toml", "format = \"toon\"\n");
    let config = repo.path("astro-sight.toml");
    let config = config.to_str().expect("utf-8 path");

    let toon = stdout_of(&run(
        &repo,
        &["--config", config, "symbols", "--path", "a.rs"],
    ));
    assert!(toon.starts_with("path: a.rs\n"), "unexpected: {toon}");

    // CLI の明示指定が config より優先される。
    let json = stdout_of(&run(
        &repo,
        &[
            "--config", config, "symbols", "--path", "a.rs", "--format", "json",
        ],
    ));
    assert!(json.starts_with('{'), "unexpected: {json}");
}

#[test]
fn config_sourced_toon_does_not_break_protocol_surfaces() {
    // `format = "toon"` を設定しただけで session / hook が全滅しないこと
    // (明示指定と違い、config 由来の既定値はプロトコル面では JSON に倒す)。
    let repo = sample_repo();
    repo.write("astro-sight.toml", "format = \"toon\"\n");
    let config = repo.path("astro-sight.toml");
    let config = config.to_str().expect("utf-8 path");

    repo.init_git();
    repo.commit_all("init");
    repo.write("b.rs", "pub fn gamma() -> usize {\n    3\n}\n");

    let output = run(
        &repo,
        &[
            "--config", config, "review", "--dir", ".", "--git", "--hook",
        ],
    );
    // hook は blocking 時 exit 1 になりうるので、成否ではなく「JSON であること」を見る。
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        serde_json::from_str::<serde_json::Value>(stderr.trim())
            .expect("hook output stays JSON even with config format = toon");
    }

    let session = cargo_bin()
        .args(["--config", config, "session"])
        .current_dir(repo.root())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to run session");
    assert!(
        session.status.success(),
        "session must not fail on a config-sourced toon default: {}",
        String::from_utf8_lossy(&session.stderr)
    );
}

#[test]
fn invalid_config_format_is_reported() {
    let repo = sample_repo();
    repo.write("astro-sight.toml", "format = \"yaml\"\n");
    let config = repo.path("astro-sight.toml");
    let config = config.to_str().expect("utf-8 path");

    let output = run(&repo, &["--config", config, "symbols", "--path", "a.rs"]);
    assert!(!output.status.success());
}
