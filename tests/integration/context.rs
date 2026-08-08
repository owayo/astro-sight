//! context サブコマンド (変更前の影響確認) の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

#[test]
fn context_with_diff() {
    use std::io::Write;
    use std::process::Stdio;

    // 合成 diff: extract_symbols のシグネチャ変更。行番号は実コードから動的に取得する。
    let symbols_src = std::fs::read_to_string("src/engine/symbols/mod.rs")
        .expect("read src/engine/symbols/mod.rs");
    let extract_line_idx = symbols_src
        .lines()
        .position(|l| l.starts_with("pub fn extract_symbols("))
        .expect("extract_symbols 関数が見つからない");
    let line_no = extract_line_idx + 1;
    let diff = format!(
        "--- a/src/engine/symbols/mod.rs\n\
         +++ b/src/engine/symbols/mod.rs\n\
         @@ -{line_no},7 +{line_no},7 @@\n\
         -pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {{\n\
         +pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, include_refs: bool) -> Result<Vec<Symbol>> {{\n\
             let query_src = symbol_query(lang_id);\n"
    );

    let mut child = cargo_bin()
        .args(["context", "--dir", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(diff.as_bytes())
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json.get("version").is_none(),
        "context should not have version"
    );

    let changes = json["changes"].as_array().unwrap();
    assert!(!changes.is_empty(), "Should have changes");
    assert_eq!(changes[0]["path"], "src/engine/symbols/mod.rs");

    let affected = changes[0]["affected_symbols"].as_array().unwrap();
    assert!(!affected.is_empty(), "Should have affected symbols");
    assert_eq!(affected[0]["name"], "extract_symbols");
}

#[test]
fn context_diff_file_arg() {
    use std::io::Write;

    // 自己完結フィクスチャ: diff が実ファイルのシンボル範囲に重なるようにし、
    // 「空 FileImpact はスキップする」ガードの下でも --diff-file の疎通を検証できる形にする。
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("sample.rs"),
        "pub fn helper() -> i32 {\n    1\n}\n\npub fn run() -> i32 {\n    helper()\n}\n",
    )
    .unwrap();

    let diff = r#"--- a/sample.rs
+++ b/sample.rs
@@ -1,3 +1,3 @@
-pub fn helper() -> i32 {
-    1
+pub fn helper() -> i64 {
+    2
 }
"#;

    let tmp = std::env::temp_dir().join("astro_sight_test.diff");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(diff.as_bytes()).unwrap();
    drop(f);

    let output = cargo_bin()
        .args([
            "context",
            "--dir",
            dir.path().to_str().unwrap(),
            "--diff-file",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json.get("version").is_none(),
        "context should not have version"
    );
    let changes = json["changes"].as_array().unwrap();
    assert!(!changes.is_empty());
    assert_eq!(changes[0]["path"], "sample.rs");
    let affected = changes[0]["affected_symbols"].as_array().unwrap();
    assert!(
        affected
            .iter()
            .any(|s| s["name"].as_str() == Some("helper")),
        "helper should be affected: {changes:?}"
    );

    let _ = std::fs::remove_file(&tmp);
}

// ---- Phase 3: Batch refs unit test (via context command) ----

#[test]
fn context_batch_refs_consistency() {
    use std::io::Write;
    use std::process::Stdio;

    // batch refs アプローチでの context 出力が従来通り一貫していることを確認する。
    // 行番号は実コードから動的に取得する。
    let symbols_src = std::fs::read_to_string("src/engine/symbols/mod.rs")
        .expect("read src/engine/symbols/mod.rs");
    let extract_line_idx = symbols_src
        .lines()
        .position(|l| l.starts_with("pub fn extract_symbols("))
        .expect("extract_symbols 関数が見つからない");
    let line_no = extract_line_idx + 1;
    let diff = format!(
        "--- a/src/engine/symbols/mod.rs\n\
         +++ b/src/engine/symbols/mod.rs\n\
         @@ -{line_no},7 +{line_no},7 @@\n\
         -pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {{\n\
         +pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, flag: bool) -> Result<Vec<Symbol>> {{\n\
             let query_src = symbol_query(lang_id);\n"
    );

    let mut child = cargo_bin()
        .args(["context", "--dir", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(diff.as_bytes())
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let changes = json["changes"].as_array().unwrap();
    assert!(!changes.is_empty());

    // Verify affected symbols detected
    let affected = changes[0]["affected_symbols"].as_array().unwrap();
    assert!(!affected.is_empty());
    assert_eq!(affected[0]["name"], "extract_symbols");

    // Verify impacted_callers is an array (may or may not have entries depending on workspace)
    assert!(changes[0]["impacted_callers"].is_array());
}

// ---- Context --git tests ----

#[test]
fn context_git_auto_diff() {
    // HEAD を基準にすると差分は空になり得るが、--git オプションの動作確認には十分
    let output = cargo_bin()
        .args(["context", "--dir", ".", "--git"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(json["changes"].is_array());
}

#[test]
fn context_git_staged() {
    let output = cargo_bin()
        .args(["context", "--dir", ".", "--git", "--staged"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(json["changes"].is_array());
}

#[test]
fn context_git_xojo_only_diff_skips_before_parse() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let xojo_fixture = include_str!("../fixtures/sample.xojo_code");

    std::fs::write(root.join("sample.xojo_code"), xojo_fixture).unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("sample.xojo_code"),
        xojo_fixture.replace("Hello, ", "Hello!, "),
    )
    .unwrap();

    let output = cargo_bin()
        .args(["context", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(
        json["changes"].as_array().unwrap().len(),
        0,
        "Xojo のみの diff は parse 前に skip されるべき: {json}"
    );
}

// ---- context 空 diff テスト ----

#[test]
fn context_empty_diff_returns_empty_changes() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = cargo_bin()
        .args(["context", "--dir", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    // 空の diff を入力
    child.stdin.as_mut().unwrap().write_all(b"").unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let changes = json["changes"].as_array().unwrap();
    assert!(changes.is_empty(), "空の diff は空の changes を返すべき");
}

#[test]
fn context_streaming_validation_error_returns_valid_json() {
    let diff = "--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-a\n+b\n";
    let diff_arg = format!("--diff={diff}");
    let output = cargo_bin()
        .args(["context", "--dir", "/nonexistent/dir/path", &diff_arg])
        .output()
        .expect("failed to run context");

    assert!(!output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "FILE_NOT_FOUND");
    assert!(
        json.get("changes").is_none(),
        "streaming prefix should not be emitted before validation errors: {json}"
    );
}

// ---- impact: 既存のファイルが diff に含まれるが存在しない場合 ----

#[test]
fn context_diff_referencing_nonexistent_file() {
    use std::io::Write;
    use std::process::Stdio;

    // diff 内のファイルがワークスペースに存在しない場合はスキップされる
    let diff = r#"--- a/nonexistent_module.rs
+++ b/nonexistent_module.rs
@@ -1,3 +1,3 @@
-fn old_fn() {}
+fn new_fn() {}
"#;

    let dir = tempfile::tempdir().expect("tempdir");

    let mut child = cargo_bin()
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(diff.as_bytes())
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let changes = json["changes"].as_array().unwrap();
    assert!(
        changes.is_empty(),
        "存在しないファイルの diff は changes に含まれないべき"
    );
}

/// Phase 4: PHP `Foo::new()` は `impacted_callers`、`$x->new()` は `low_confidence_callers`
/// に振り分けられる。`new` のような generic name + receiver-bare 呼び出しが
/// 強い impact 信号を汚染しない仕様の回帰テスト。
///
/// `ASTRO_SIGHT_NO_CONFIDENCE_FILTER=1` を設定すると従来挙動 (全 caller を impacted_callers
/// に流す) に戻ることもあわせて確認する。
#[test]
fn context_php_generic_method_bare_call_routed_to_low_confidence() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    // Foo.php: 変更対象クラス (method `new` のシグネチャを変える)
    std::fs::write(
        root.join("Foo.php"),
        "<?php\nclass Foo {\n    public function new() {\n        return 'foo-new';\n    }\n}\n",
    )
    .unwrap();
    // CallerExact.php: `Foo::new()` で ExactOwner として呼び出し
    std::fs::write(
        root.join("CallerExact.php"),
        "<?php\nclass CallerExact {\n    public function callExact() {\n        return Foo::new();\n    }\n}\n",
    )
    .unwrap();
    // CallerBare.php: `$x->new()` で BareNameOnly 呼び出し。
    // `parent_in_this_file` フィルタを通過させるため、ファイル内に `Foo` 識別子を出現させる
    // (Laravel 系の `_ide_helper.php` で起きるノイズ条件を最小再現)。
    std::fs::write(
        root.join("CallerBare.php"),
        "<?php\nclass CallerBare {\n    public function callBare($x) {\n        $tmp = Foo::class;\n        return $x->new();\n    }\n}\n",
    )
    .unwrap();

    let diff = "diff --git a/Foo.php b/Foo.php\n--- a/Foo.php\n+++ b/Foo.php\n@@ -2,5 +2,5 @@\n class Foo {\n-    public function new() {\n+    public function new($flag) {\n         return 'foo-new';\n     }\n";
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, diff).unwrap();

    // 既定: confidence ベースのルーティングが効く
    let output = cargo_bin()
        .args([
            "context",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run context");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let changes = json["changes"].as_array().expect("changes 配列");
    assert_eq!(changes.len(), 1, "1 ファイル分の FileImpact が出るべき");
    let impact = &changes[0];

    let impacted: Vec<&serde_json::Value> = impact["impacted_callers"]
        .as_array()
        .expect("impacted_callers 配列")
        .iter()
        .collect();
    assert_eq!(
        impacted.len(),
        1,
        "ExactOwner だけが impacted_callers に入るべき: {impacted:?}"
    );
    let exact_path = impacted[0]["path"].as_str().expect("path");
    assert!(
        exact_path.ends_with("CallerExact.php"),
        "ExactOwner caller は CallerExact.php であるべき: {exact_path}"
    );
    assert!(
        impacted[0]["confidence"].is_null(),
        "ExactOwner には confidence は付かない: {:?}",
        impacted[0]
    );

    let low: Vec<&serde_json::Value> = impact["low_confidence_callers"]
        .as_array()
        .expect("low_confidence_callers 配列")
        .iter()
        .collect();
    assert_eq!(
        low.len(),
        1,
        "BareNameOnly + generic name は low_confidence_callers に振り分けられるべき: {low:?}"
    );
    let low_path = low[0]["path"].as_str().expect("path");
    assert!(
        low_path.ends_with("CallerBare.php"),
        "low confidence caller は CallerBare.php であるべき: {low_path}"
    );
    assert_eq!(
        low[0]["confidence"].as_str(),
        Some("low"),
        "low_confidence_callers には confidence=low が付くべき: {:?}",
        low[0]
    );

    // ASTRO_SIGHT_NO_CONFIDENCE_FILTER=1: 振り分けが無効化されて全 caller が impacted_callers に
    let output = cargo_bin()
        .env("ASTRO_SIGHT_NO_CONFIDENCE_FILTER", "1")
        .args([
            "context",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run context");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let impact = &json["changes"][0];
    let impacted_paths: Vec<&str> = impact["impacted_callers"]
        .as_array()
        .expect("impacted_callers 配列")
        .iter()
        .filter_map(|c| c["path"].as_str())
        .collect();
    assert_eq!(
        impacted_paths.len(),
        2,
        "fallback 設定下では両 caller が impacted_callers に流れるべき: {impacted_paths:?}"
    );
    assert!(
        impacted_paths
            .iter()
            .any(|p| p.ends_with("CallerExact.php")),
        "fallback 下でも ExactOwner caller は残る: {impacted_paths:?}"
    );
    assert!(
        impacted_paths.iter().any(|p| p.ends_with("CallerBare.php")),
        "fallback 下では BareNameOnly caller も impacted_callers に出る: {impacted_paths:?}"
    );
    assert!(
        impact["low_confidence_callers"].as_array().is_none()
            || impact["low_confidence_callers"]
                .as_array()
                .unwrap()
                .is_empty(),
        "fallback 下では low_confidence_callers は空 (skip_serializing_if で省略) のはず: {impact:?}"
    );
}
