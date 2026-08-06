//! パス境界・サンドボックス・diff 入力検証の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

// ---- フェーズ 2: セキュリティテスト ----

#[test]
fn sandboxed_service_rejects_path_traversal() {
    // AppService::sandboxed はワークスペース外のパスを拒否する。
    let cwd = std::env::current_dir().unwrap();
    let cwd = std::fs::canonicalize(cwd).unwrap();
    let service = astro_sight::service::AppService::sandboxed(cwd).unwrap();

    // ワークスペース外の /etc/hosts を指定する。
    let params = astro_sight::service::AstParams {
        path: "/etc/hosts",
        line: None,
        col: None,
        end_line: None,
        end_col: None,
        depth: 3,
        context_lines: 3,
    };
    let result = service.extract_ast(&params);
    assert!(result.is_err(), "ワークスペース外のパスは拒否されるべき");

    let err_msg = match result {
        Ok(_) => String::new(),
        Err(err) => err.to_string(),
    };
    assert!(
        err_msg.contains("outside workspace") || err_msg.contains("PATH_OUT_OF_BOUNDS"),
        "エラーにワークスペース境界が含まれるべき: {err_msg}"
    );
}

#[test]
fn sandboxed_service_allows_workspace_paths() {
    // AppService::sandboxed はワークスペース内のパスを許可する。
    let cwd = std::env::current_dir().unwrap();
    let cwd = std::fs::canonicalize(cwd).unwrap();
    let service = astro_sight::service::AppService::sandboxed(cwd).unwrap();

    // src/lib.rs はワークスペース内にある。
    let result = service.extract_symbols("src/lib.rs");
    assert!(result.is_ok(), "ワークスペース内のパスは許可されるべき");
}

#[test]
fn sandboxed_service_rejects_file_workspace_root() {
    let dir = tempfile::TempDir::new().unwrap();
    let file_path = dir.path().join("workspace.txt");
    std::fs::write(&file_path, "not a directory").unwrap();

    let result = astro_sight::service::AppService::sandboxed(file_path.clone());
    assert!(
        result.is_err(),
        "ファイルをワークスペースルートにしてはいけない"
    );

    let err_msg = match result {
        Ok(_) => String::new(),
        Err(err) => err.to_string(),
    };
    assert!(
        err_msg.contains("directory"),
        "エラーにディレクトリ要件が含まれるべき: {err_msg}"
    );
}

// ===========================================================================
// 境界値・異常系・エッジケーステスト
// ===========================================================================

// ---- diff パーサー境界値テスト ----

#[test]
fn diff_empty_input_produces_no_files() {
    // 空の diff 入力は空の結果を返すべき
    let files = astro_sight::engine::diff::parse_unified_diff("");
    assert!(files.is_empty(), "空の diff は空の結果を返すべき");
}

#[test]
fn diff_deleted_file_with_dev_null() {
    // ファイル削除（+++ /dev/null）を正しくパースすること
    let diff = r#"--- a/src/old.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-fn old_fn() {}
-fn another() {}
-// end
"#;
    let files = astro_sight::engine::diff::parse_unified_diff(diff);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].old_path, "src/old.rs");
    assert_eq!(files[0].new_path, "/dev/null");
    assert_eq!(files[0].hunks[0].new_count, 0);
}

#[test]
fn diff_hunk_header_only_no_content_lines() {
    // ハンクヘッダのみで内容行がない diff
    let diff = r#"--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,3 +1,3 @@
"#;
    let files = astro_sight::engine::diff::parse_unified_diff(diff);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].hunks.len(), 1);
}

#[test]
fn diff_missing_hunk_header_produces_no_file() {
    // ハンクヘッダがない場合はファイルとして認識しない（hunks が空）
    let diff = r#"--- a/src/foo.rs
+++ b/src/foo.rs
"#;
    let files = astro_sight::engine::diff::parse_unified_diff(diff);
    assert!(
        files.is_empty(),
        "ハンクなしの diff はファイルを生成しないべき"
    );
}

// ---- AppService 入力サイズ制限テスト ----

#[test]
fn sandboxed_service_validates_input_size() {
    let cwd = std::env::current_dir().unwrap();
    let cwd = std::fs::canonicalize(cwd).unwrap();
    let service = astro_sight::service::AppService::sandboxed(cwd).unwrap();

    // sandboxed は max_input_size = 100MB に設定される。
    // analyze_context で validate_input_size が呼ばれるため、
    // 小さい入力は通ること、巨大入力は別のテスト環境で確認。
    let result = service.analyze_context(
        "",
        ".",
        &astro_sight::models::impact::ContextAnalysisOptions::default(),
    );
    assert!(result.is_ok(), "空の diff 入力はサイズ制限を通過するべき");
}

#[test]
fn git_repo_invalid_base_still_errors() {
    // 正常 repo (プロジェクト自身) で不正 base → exit 1 を維持 (R4)。
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            ".",
            "--git",
            "--base",
            "no-such-ref-xyz-astro",
        ])
        .output()
        .expect("run");
    assert!(!output.status.success(), "不正 base は exit 1 維持");
}

#[test]
fn git_repo_dash_base_rejected() {
    // 先頭 '-' の base は git 管理下でも入力契約違反で exit 1。
    let output = cargo_bin()
        .args(["review", "--dir", ".", "--git", "--base=-x"])
        .output()
        .expect("run");
    assert!(!output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
}
