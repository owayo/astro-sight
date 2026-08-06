//! MCP サーバーモード (stdio JSON-RPC) の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

#[test]
fn mcp_initialize() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp");

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"test","version":"1.0"}}}}}}"#
    )
    .unwrap();
    // サーバーを終了させるために stdin を閉じる
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");

    let stdout = String::from_utf8(output.stdout).unwrap();
    // 1行目は initialize レスポンス
    let first_line = stdout.lines().next().expect("should have output");
    let json: serde_json::Value =
        serde_json::from_str(first_line).expect("should be valid JSON-RPC");
    assert_eq!(json["jsonrpc"], "2.0");
    assert_eq!(json["id"], 1);
    assert_eq!(json["result"]["serverInfo"]["name"], "astro-sight");
    assert_eq!(json["result"]["serverInfo"]["version"], PKG_VERSION);
    assert!(json["result"]["capabilities"]["tools"].is_object());
}

#[test]
fn mcp_tools_list() {
    let stdout = mcp_send_after_init(&[r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#]);

    let tools_line = stdout
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .expect("tools/list レスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(tools_line).expect("valid JSON-RPC");
    let tools = json["result"]["tools"]
        .as_array()
        .expect("tools 配列が必要");
    assert!(
        tools.len() >= 11,
        "11ツール以上が必要、実際: {}",
        tools.len()
    );
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in [
        "ast_extract",
        "symbols_extract",
        "calls_extract",
        "refs_search",
        "refs_batch_search",
        "context_analyze",
        "imports_extract",
        "lint",
        "sequence_diagram",
        "cochange_analyze",
        "doctor",
    ] {
        assert!(
            tool_names.contains(&expected),
            "ツール '{expected}' が tools/list に含まれるべき"
        );
    }
}

#[test]
fn mcp_tools_call_symbols() {
    let stdout = mcp_send_after_init(&[
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"symbols_extract","arguments":{"path":"tests/fixtures/sample.py"}}}"#,
    ]);

    let result_line = stdout
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .expect("tools/call レスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(result_line).expect("valid JSON-RPC");
    let content = json["result"]["content"]
        .as_array()
        .expect("content 配列が必要");
    assert!(!content.is_empty(), "content が空であってはならない");
    let text = content[0]["text"].as_str().expect("text フィールドが必要");
    let symbols: serde_json::Value =
        serde_json::from_str(text).expect("symbols JSON がパース可能であるべき");
    assert!(
        symbols["symbols"].as_array().is_some(),
        "symbols 配列が必要"
    );
}

#[test]
fn mcp_tools_call_refs_search() {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "refs_search",
            "arguments": {
                "name": "Config",
                "dir": "tests/fixtures",
                "glob": "**/*.py"
            }
        }
    })
    .to_string();
    let stdout = mcp_send_after_init(&[&request]);

    let result_line = stdout
        .lines()
        .find(|line| line.contains("\"id\":2"))
        .expect("refs_search レスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(result_line).expect("valid JSON-RPC");
    let content = json["result"]["content"]
        .as_array()
        .expect("content 配列が必要");
    let text = content[0]["text"].as_str().expect("text フィールドが必要");
    let refs: serde_json::Value =
        serde_json::from_str(text).expect("refs JSON がパース可能であるべき");

    assert_eq!(refs["symbol"], "Config");
    assert!(
        refs["refs"]
            .as_array()
            .is_some_and(|references| references.iter().any(|reference| {
                reference["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("sample.py"))
            })),
        "sample.py 内の Config 参照が必要: {refs}"
    );
}

#[test]
fn mcp_tools_call_context_analyze() {
    let diff = "\
diff --git a/tests/fixtures/sample.py b/tests/fixtures/sample.py\n\
--- a/tests/fixtures/sample.py\n\
+++ b/tests/fixtures/sample.py\n\
@@ -28,1 +28,1 @@\n\
-def create_config(path: str) -> Config:\n\
+def create_config(path: str, strict: bool = False) -> Config:\n";
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "context_analyze",
            "arguments": {
                "diff": diff,
                "dir": "."
            }
        }
    })
    .to_string();
    let stdout = mcp_send_after_init(&[&request]);

    let result_line = stdout
        .lines()
        .find(|line| line.contains("\"id\":2"))
        .expect("context_analyze レスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(result_line).expect("valid JSON-RPC");
    let content = json["result"]["content"]
        .as_array()
        .expect("content 配列が必要");
    let text = content[0]["text"].as_str().expect("text フィールドが必要");
    let context: serde_json::Value =
        serde_json::from_str(text).expect("context JSON がパース可能であるべき");

    assert!(
        context["changes"]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|change| {
                change["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("tests/fixtures/sample.py"))
            })),
        "sample.py の変更影響が必要: {context}"
    );
}

#[test]
fn mcp_tools_call_path_traversal() {
    let stdout = mcp_send_after_init(&[
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"symbols_extract","arguments":{"path":"/etc/hosts"}}}"#,
    ]);

    let result_line = stdout
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .expect("エラーレスポンスが必要");
    let json: serde_json::Value = serde_json::from_str(result_line).expect("valid JSON-RPC");
    assert!(
        json["error"].is_object(),
        "パストラバーサルはエラーを返すべき: {json}"
    );
}

// ---- MCP sandbox fail-closed テスト ----

#[test]
fn mcp_sandbox_fail_closed_with_file_workspace() {
    // MCP サーバーはファイルを workspace root として受け付けない。
    // AppService::sandboxed がファイルを拒否するので、
    // AstroSightServer::new() 相当のロジックが fail-closed であることを
    // AppService レベルで確認。
    let dir = tempfile::TempDir::new().unwrap();
    let file_path = dir.path().join("not_a_dir.txt");
    std::fs::write(&file_path, "content").unwrap();

    let result = astro_sight::service::AppService::sandboxed(file_path);
    assert!(result.is_err(), "ファイルを workspace root にできないべき");
}

#[test]
fn mcp_sandbox_fail_closed_with_nonexistent_dir() {
    // 存在しないディレクトリでサンドボックスは生成できない
    let result = astro_sight::service::AppService::sandboxed(std::path::PathBuf::from(
        "/nonexistent/path/that/does/not/exist",
    ));
    assert!(
        result.is_err(),
        "存在しないディレクトリで sandbox は生成できないべき"
    );
}
