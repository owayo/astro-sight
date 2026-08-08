//! `--format json|toon` と `config.toml` の `format` の統合テスト。
//!
//! 検証の軸は 3 つ:
//! 1. 既定 (JSON) の出力が **1 バイトも変わっていない** こと
//! 2. TOON がデータ面 (単一ドキュメント / バッチ / MCP text content) で有効なこと
//! 3. プロトコル面 (session / hook / エラー) が JSON 固定であること

use super::support::{TestRepo, cargo_bin, cargo_bin_with_explicit_config};

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
    let mut command = if args.contains(&"--config") {
        cargo_bin_with_explicit_config()
    } else {
        cargo_bin()
    };
    command
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
    // TOON v4.1 の canonical encoder は文書末尾の改行を禁止する。
    assert!(!toon.ends_with('\n'));
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
    assert!(
        !toon.ends_with('\n'),
        "TOON must not have a trailing newline"
    );
}

#[test]
fn json_batch_stays_ndjson() {
    let repo = sample_repo();
    let json = stdout_of(&run(&repo, &["symbols", "--dir", ".", "--glob", "**/*.rs"]));
    // 件数まで固定する。行を回すだけだと空出力でも素通りしてしまう。
    let docs: Vec<serde_json::Value> = json
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is a JSON document"))
        .collect();
    assert_eq!(docs.len(), 2, "two source files expected: {json}");
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
    assert!(
        !toon.ends_with('\n'),
        "TOON must not have a trailing newline"
    );
}

// ---------------------------------------------------------------------------
// auto (json / toon のうち短い方)
// ---------------------------------------------------------------------------

/// `auto` の判定指標 (`output::size_metric` と同じ規則)。
/// 素の文字数ではなく「文字数 + 3 × 改行数」— BPE では改行 + インデントが
/// 1 行あたり約 1 トークンを消費するため。
fn size_metric(text: &str) -> usize {
    text.chars().count() + 3 * text.bytes().filter(|b| *b == b'\n').count()
}

#[test]
fn auto_picks_the_smaller_estimate() {
    let repo = sample_repo();
    for args in [
        vec!["symbols", "--path", "a.rs"],
        vec!["refs", "--name", "alpha", "--dir", "."],
        vec!["refs", "--names", "alpha,gamma", "--dir", "."],
        vec!["imports", "--path", "a.rs"],
        vec!["calls", "--path", "a.rs"],
        vec!["ast", "--path", "a.rs", "--line", "1", "--col", "0"],
    ] {
        let json = stdout_of(&run(&repo, &args));
        let mut toon_args = args.clone();
        toon_args.extend(["--format", "toon"]);
        let toon = stdout_of(&run(&repo, &toon_args));
        let mut auto_args = args.clone();
        auto_args.extend(["--format", "auto"]);
        let auto = stdout_of(&run(&repo, &auto_args));

        let expected = if size_metric(&toon) < size_metric(&json) {
            &toon
        } else {
            &json
        };
        assert_eq!(
            &auto, expected,
            "auto should pick the smaller estimate for {args:?}"
        );
        assert!(size_metric(&auto) <= size_metric(&json));
        assert!(size_metric(&auto) <= size_metric(&toon));
    }
}

#[test]
fn auto_uses_the_line_penalty_not_raw_char_count() {
    // 実トークナイザで裏を取ったケース (o200k_base / cl100k_base 共通):
    //   json 25 文字 / 17 tokens、toon 19 文字 / 19 tokens
    // 素の文字数で選ぶと TOON を選んで損をする。auto は JSON を選ぶ。
    let repo = TestRepo::new();
    repo.write("t.rs", "pub const A: usize = 1;\n");

    let json = stdout_of(&run(&repo, &["imports", "--path", "t.rs"]));
    let toon = stdout_of(&run(
        &repo,
        &["imports", "--path", "t.rs", "--format", "toon"],
    ));
    let auto = stdout_of(&run(
        &repo,
        &["imports", "--path", "t.rs", "--format", "auto"],
    ));

    // このフィクスチャでは TOON の方が文字数は少ないが行数が多い。
    assert!(
        toon.chars().count() < json.chars().count(),
        "fixture must favour TOON on raw chars: {json:?} / {toon:?}"
    );
    assert_eq!(
        auto,
        if size_metric(&toon) < size_metric(&json) {
            toon.clone()
        } else {
            json.clone()
        }
    );
}

#[test]
fn auto_batch_emits_exactly_one_valid_format() {
    let repo = sample_repo();
    let auto = stdout_of(&run(
        &repo,
        &[
            "symbols", "--dir", ".", "--glob", "**/*.rs", "--format", "auto",
        ],
    ));

    if auto.starts_with('[') {
        // TOON を選んだ場合: ヘッダの要素数と item 数が一致すること。
        let header = auto.lines().next().expect("header line");
        let declared: usize = header
            .trim_start_matches('[')
            .trim_end_matches("]:")
            .parse()
            .expect("array length in header");
        let items = auto.lines().filter(|l| l.starts_with("  - ")).count();
        assert_eq!(
            declared, items,
            "header length must match item count: {auto}"
        );
        assert_eq!(declared, 2, "two source files expected: {auto}");
        assert!(
            !auto.ends_with('\n'),
            "auto-selected TOON must not have a trailing newline"
        );
    } else {
        // JSON を選んだ場合: 全行が独立した JSON ドキュメントで、件数も合うこと。
        let docs: Vec<serde_json::Value> = auto
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is a JSON document"))
            .collect();
        assert_eq!(docs.len(), 2, "two source files expected: {auto}");
    }

    // どちらを選んだにせよ、両候補以下の長さになる。
    let json = stdout_of(&run(&repo, &["symbols", "--dir", ".", "--glob", "**/*.rs"]));
    let toon = stdout_of(&run(
        &repo,
        &[
            "symbols", "--dir", ".", "--glob", "**/*.rs", "--format", "toon",
        ],
    ));
    assert!(auto.chars().count() <= json.chars().count());
    assert!(auto.chars().count() <= toon.chars().count());
}

#[test]
fn auto_never_mixes_the_two_formats_in_one_document() {
    // バッチは window 単位で描画するため、途中でフォーマットが切り替わると
    // どちらの decoder でも読めない出力になる。1 本に統一されていることを固定する。
    let repo = TestRepo::new();
    for i in 0..40 {
        repo.write(
            format!("f{i}.rs"),
            format!(
                "pub const C{i}: usize = {i};
pub fn f{i}() -> usize {{
    C{i}
}}
"
            ),
        );
    }
    let auto = stdout_of(&run(
        &repo,
        &[
            "symbols", "--dir", ".", "--glob", "**/*.rs", "--format", "auto",
        ],
    ));

    let json_lines = auto
        .lines()
        .filter(|l| l.starts_with('{') && l.ends_with('}'))
        .count();
    let toon_items = auto.lines().filter(|l| l.starts_with("  - ")).count();
    assert!(
        json_lines == 0 || toon_items == 0,
        "output mixes NDJSON and TOON items: {auto}"
    );
}

#[test]
fn auto_config_default_works_and_cli_overrides_it() {
    let repo = sample_repo();
    repo.write("astro-sight.toml", "format = \"auto\"\n");
    let config = repo.path("astro-sight.toml");
    let config = config.to_str().expect("utf-8 path");

    let auto = stdout_of(&run(
        &repo,
        &["--config", config, "symbols", "--path", "a.rs"],
    ));
    let explicit_auto = stdout_of(&run(
        &repo,
        &["symbols", "--path", "a.rs", "--format", "auto"],
    ));
    assert_eq!(auto, explicit_auto);

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
fn auto_is_accepted_on_json_protocol_surfaces() {
    // `auto` は「TOON で出せ」という要求ではないので、session でもエラーにしない。
    let repo = sample_repo();
    let session = cargo_bin()
        .args(["session", "--format", "auto"])
        .current_dir(repo.root())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to run session");
    assert!(
        session.status.success(),
        "session must accept --format auto: {}",
        String::from_utf8_lossy(&session.stderr)
    );
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
        // hook は blocking 検出でも exit 1 になるため、終了コードだけでは
        // 「拒否された」ことの証拠にならない。エラー封筒まで確認する。
        let stdout = String::from_utf8_lossy(&output.stdout);
        let value: serde_json::Value =
            serde_json::from_str(stdout.trim()).expect("error output stays JSON");
        assert_eq!(value["error"]["code"], "INVALID_REQUEST", "args: {args:?}");
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

/// hook 出力が必ず出る差分を作る: `alpha` のシグネチャを変え、diff 外の `c.rs` に
/// 呼び出しを残す。これで impact が未解決 caller を検出し、hook が blocking になる。
fn repo_with_blocking_hook_output() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("a.rs", "pub fn alpha(x: usize) -> usize {\n    x\n}\n");
    repo.write(
        "c.rs",
        "use crate::a::alpha;\npub fn caller() -> usize {\n    alpha(1)\n}\n",
    );
    repo.init_git();
    repo.commit_all("init");
    repo.write(
        "a.rs",
        "pub fn alpha(x: usize, y: usize) -> usize {\n    x + y\n}\n",
    );
    repo
}

/// hook 出力 (stderr) を取り出す。出力が空ならテストの前提が壊れているので落とす
/// (条件付きで検証を飛ばすと、hook が何も出さなくなった退行を見逃す)。
fn hook_stderr(output: &std::process::Output) -> String {
    assert!(
        output.stdout.is_empty(),
        "hook writes to stderr only, stdout was: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr.clone()).expect("stderr is not UTF-8");
    assert!(
        !stderr.trim().is_empty(),
        "fixture must produce blocking hook output"
    );
    stderr
}

#[test]
fn config_sourced_toon_does_not_break_protocol_surfaces() {
    // `format = "toon"` を設定しただけで session / hook が全滅しないこと
    // (明示指定と違い、config 由来の既定値はプロトコル面では JSON に倒す)。
    let repo = repo_with_blocking_hook_output();
    repo.write("astro-sight.toml", "format = \"toon\"\n");
    let config = repo.path("astro-sight.toml");
    let config = config.to_str().expect("utf-8 path");

    let output = run(
        &repo,
        &[
            "--config", config, "review", "--dir", ".", "--git", "--hook",
        ],
    );
    let stderr = hook_stderr(&output);
    serde_json::from_str::<serde_json::Value>(stderr.trim())
        .expect("hook output stays JSON even with config format = toon");

    let session = cargo_bin_with_explicit_config()
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
fn protocol_surfaces_stay_json_under_auto() {
    // `auto` はプロトコル面でエラーにならない = 検証をすり抜ける。そのぶん、実際の出力が
    // JSON のままであることをここで固定する
    // (`OutputOptions::ensure_json_protocol` の不変条件のリグレッションテスト)。
    let repo = repo_with_blocking_hook_output();

    let stderr = hook_stderr(&run(
        &repo,
        &[
            "review", "--dir", ".", "--git", "--hook", "--format", "auto",
        ],
    ));
    let value: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("hook output stays JSON under --format auto");
    assert!(value.is_object(), "unexpected hook payload: {stderr}");

    // session: 1 行 1 レスポンスの NDJSON が保たれること。
    let mut child = cargo_bin()
        .args(["session", "--format", "auto"])
        .current_dir(repo.root())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn session");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("missing child stdin");
        stdin
            .write_all(b"{\"command\":\"symbols\",\"path\":\"a.rs\"}\n")
            .expect("failed to write session request");
    }
    drop(child.stdin.take());
    let out = child
        .wait_with_output()
        .expect("failed to wait for session");
    let stdout = String::from_utf8(out.stdout).expect("stdout is not UTF-8");
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "one response line expected: {stdout}");
    serde_json::from_str::<serde_json::Value>(lines[0])
        .expect("session response stays JSON under --format auto");
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
