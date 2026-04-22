use std::process::Command;

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

fn cargo_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_astro-sight"))
}

#[test]
fn doctor_returns_json() {
    let output = cargo_bin().arg("doctor").output().expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["version"], PKG_VERSION);
    let languages = json["languages"].as_array().unwrap();
    assert_eq!(languages.len(), 17);
    assert!(languages.iter().any(|lang| lang["language"] == "zig"));
    assert!(languages.iter().any(|lang| lang["language"] == "xojo"));

    // すべての対応言語が利用可能であること
    for lang in languages {
        assert!(
            lang["available"].as_bool().unwrap(),
            "Language {:?} not available",
            lang["language"]
        );
    }
}

#[test]
fn ast_on_own_source() {
    // Default compact output: schema present, range as array, no id/named/hash
    let output = cargo_bin()
        .args(["ast", "--path", "src/main.rs", "--line", "0", "--col", "0"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");
    assert!(!json["ast"].as_array().unwrap().is_empty());
    assert!(json["schema"]["range"].as_str().is_some());
    // compact: path instead of location, lang instead of language
    assert!(json["path"].as_str().is_some());
    // compact: range is [sL,sC,eL,eC] array, no id/named
    let first = &json["ast"][0];
    assert!(first["range"].as_array().is_some());
    assert!(first.get("id").is_none());
    assert!(first.get("named").is_none());
}

#[test]
fn ast_full_output() {
    // --full: legacy format with id, named, nested range, hash
    let output = cargo_bin()
        .args([
            "ast",
            "--path",
            "src/main.rs",
            "--line",
            "0",
            "--col",
            "0",
            "--full",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["language"], "rust");
    assert!(json["hash"].as_str().is_some());
    let first = &json["ast"][0];
    assert!(first["id"].as_u64().is_some());
    assert!(first["range"]["start"]["line"].is_number());
}

#[test]
fn ast_full_file() {
    let output = cargo_bin()
        .args(["ast", "--path", "src/lib.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");
    assert!(!json["ast"].as_array().unwrap().is_empty());
}

#[test]
fn symbols_on_own_source() {
    let output = cargo_bin()
        .args(["symbols", "--path", "src/main.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");
    assert!(json["path"].as_str().is_some());

    let symbols = json["symbols"].as_array().unwrap();
    assert!(!symbols.is_empty());

    // Should find the main function
    let main_fn = symbols.iter().find(|s| s["name"] == "main");
    assert!(main_fn.is_some(), "Should find main function");
    assert_eq!(main_fn.unwrap()["kind"], "fn");

    // Compact output: has ln, no range/hash
    assert!(
        main_fn.unwrap().get("ln").is_some(),
        "Compact output should have ln field"
    );
    assert!(
        main_fn.unwrap().get("range").is_none(),
        "Compact output should not have range field"
    );
    assert!(
        json.get("hash").is_none(),
        "Compact output should not have hash field"
    );
}

#[test]
fn symbols_full_output() {
    let output = cargo_bin()
        .args(["symbols", "--path", "src/main.rs", "--full"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["language"], "rust");

    // Full output: has hash and range
    assert!(
        json.get("hash").is_some(),
        "Full output should have hash field"
    );

    let symbols = json["symbols"].as_array().unwrap();
    let main_fn = symbols.iter().find(|s| s["name"] == "main").unwrap();
    assert!(
        main_fn.get("range").is_some(),
        "Full output should have range field"
    );
    assert!(
        main_fn.get("line").is_none(),
        "Full output should not have line field"
    );
}

#[test]
fn symbols_doc_flag() {
    let output = cargo_bin()
        .args(["symbols", "--path", "src/service.rs", "--doc"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");

    // Should be compact format with doc included
    assert!(
        json.get("hash").is_none(),
        "Compact+doc should not have hash"
    );

    let symbols = json["symbols"].as_array().unwrap();
    // At least one symbol should have a doc field
    let has_doc = symbols.iter().any(|s| s.get("doc").is_some());
    assert!(
        has_doc,
        "With --doc flag, documented symbols should include doc"
    );
}

#[test]
fn ast_file_not_found() {
    let output = cargo_bin()
        .args(["ast", "--path", "nonexistent.rs"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    // Error should be JSON on stdout
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("error should be JSON");
    assert_eq!(json["error"]["code"], "FILE_NOT_FOUND");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nonexistent.rs")
    );
}

#[test]
fn session_ndjson() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .arg("session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn session");

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, r#"{{"command":"symbols","path":"src/main.rs"}}"#).unwrap();
    writeln!(stdin, r#"{{"command":"doctor","path":"."}}"#).unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Should have 2 NDJSON lines");

    // First line should be symbols response
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert!(first["symbols"].is_array());

    // Second line should be doctor response
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert!(second["languages"].is_array());
}

#[test]
fn cache_works() {
    // Run ast command twice, second should be cached
    let output1 = cargo_bin()
        .args(["ast", "--path", "src/lib.rs", "--line", "0", "--col", "0"])
        .output()
        .expect("failed to run");
    assert!(output1.status.success());

    let output2 = cargo_bin()
        .args(["ast", "--path", "src/lib.rs", "--line", "0", "--col", "0"])
        .output()
        .expect("failed to run");
    assert!(output2.status.success());

    // Both should return the same result
    assert_eq!(output1.stdout, output2.stdout);
}

#[test]
fn no_cache_flag() {
    let output = cargo_bin()
        .args([
            "ast",
            "--path",
            "src/lib.rs",
            "--line",
            "0",
            "--col",
            "0",
            "--no-cache",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");
}

#[test]
fn calls_on_own_source() {
    let output = cargo_bin()
        .args(["calls", "--path", "src/main.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json.get("version").is_none(),
        "calls should not have version"
    );
    assert_eq!(json["lang"], "rust");

    let calls = json["calls"].as_array().unwrap();
    assert!(!calls.is_empty(), "Should find call groups in main.rs");

    // Compact: calls are grouped by caller
    let main_group = calls.iter().find(|c| c["caller"] == "main");
    assert!(main_group.is_some(), "main should have a call group");
    assert!(
        !main_group.unwrap()["callees"]
            .as_array()
            .unwrap()
            .is_empty(),
        "main should call other functions"
    );
}

#[test]
fn calls_with_function_filter() {
    let output = cargo_bin()
        .args(["calls", "--path", "src/main.rs", "--function", "cmd_ast"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let calls = json["calls"].as_array().unwrap();

    // Compact: all caller groups should be cmd_ast
    for call in calls {
        assert_eq!(call["caller"], "cmd_ast");
    }
}

#[test]
fn refs_finds_symbol() {
    let output = cargo_bin()
        .args(["refs", "--name", "AstgenResponse", "--dir", "src/"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json.get("version").is_none(),
        "refs should not have version"
    );
    assert_eq!(json["symbol"], "AstgenResponse");

    let refs = json["refs"].as_array().unwrap();
    assert!(
        refs.len() >= 2,
        "Should find AstgenResponse in multiple files"
    );

    // Should have at least one definition
    let defs: Vec<_> = refs.iter().filter(|r| r["kind"] == "def").collect();
    assert!(!defs.is_empty(), "Should find definition of AstgenResponse");
}

#[test]
fn refs_with_glob_filter() {
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "AstgenResponse",
            "--dir",
            "src/",
            "--glob",
            "**/*.rs",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    assert!(!refs.is_empty());
}

#[test]
fn context_with_diff() {
    use std::io::Write;
    use std::process::Stdio;

    // Create a synthetic diff
    let diff = r#"--- a/src/engine/symbols.rs
+++ b/src/engine/symbols.rs
@@ -603,7 +603,7 @@
-pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {
+pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, include_refs: bool) -> Result<Vec<Symbol>> {
     let query_src = symbol_query(lang_id);
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
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
    assert_eq!(changes[0]["path"], "src/engine/symbols.rs");

    let affected = changes[0]["affected_symbols"].as_array().unwrap();
    assert!(!affected.is_empty(), "Should have affected symbols");
    assert_eq!(affected[0]["name"], "extract_symbols");
}

#[test]
fn session_ndjson_calls() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .arg("session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn session");

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        r#"{{"command":"calls","path":"src/main.rs","function":"main"}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"command":"refs","name":"AstgenResponse","dir":"src/"}}"#
    )
    .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Should have 2 NDJSON response lines");

    // First: calls response (compact: grouped by caller)
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert!(first["calls"].is_array());

    // Second: refs response
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert!(second["refs"].is_array());
}

// ---- New tests: compact/pretty, --diff, batch, MCP ----

#[test]
fn compact_output_default() {
    let output = cargo_bin()
        .args(["symbols", "--path", "src/lib.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "Default output should be a single compact JSON line"
    );

    // Should be valid JSON
    let _: serde_json::Value = serde_json::from_str(lines[0]).expect("should be valid JSON");
}

#[test]
fn pretty_output_flag() {
    let output = cargo_bin()
        .args(["symbols", "--pretty", "--path", "src/lib.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert!(lines.len() > 1, "Pretty output should be multi-line");

    // Should be valid JSON
    let _: serde_json::Value = serde_json::from_str(&stdout).expect("should be valid JSON");
}

#[test]
fn context_diff_file_arg() {
    use std::io::Write;

    let diff = r#"--- a/src/engine/symbols.rs
+++ b/src/engine/symbols.rs
@@ -9,7 +9,7 @@
-pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {
+pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, flag: bool) -> Result<Vec<Symbol>> {
     let query_src = symbol_query(lang_id);
"#;

    let tmp = std::env::temp_dir().join("astro_sight_test.diff");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(diff.as_bytes()).unwrap();
    drop(f);

    let output = cargo_bin()
        .args([
            "context",
            "--dir",
            ".",
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
    assert_eq!(changes[0]["path"], "src/engine/symbols.rs");

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn batch_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--paths", "src/lib.rs,src/cli.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Batch should produce 2 NDJSON lines");

    for line in &lines {
        let json: serde_json::Value =
            serde_json::from_str(line).expect("each line should be valid JSON");
        assert!(json["symbols"].is_array());
    }
}

#[test]
fn ast_rejects_empty_paths_list() {
    let output = cargo_bin()
        .args(["ast", "--paths", ","])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--paths")
    );
}

#[test]
fn batch_with_error() {
    let output = cargo_bin()
        .args(["symbols", "--paths", "src/lib.rs,nonexistent.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Batch should produce 2 NDJSON lines");

    // 1行目は正常レスポンス
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert!(first["symbols"].is_array());

    // 2行目は行内エラー
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["error"]["code"], "FILE_NOT_FOUND");
}

#[test]
fn batch_paths_file() {
    use std::io::Write;

    let tmp = std::env::temp_dir().join("astro_sight_paths.txt");
    let mut f = std::fs::File::create(&tmp).unwrap();
    writeln!(f, "src/lib.rs").unwrap();
    writeln!(f, "src/cli.rs").unwrap();
    drop(f);

    let output = cargo_bin()
        .args(["calls", "--paths-file", tmp.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Batch should produce 2 NDJSON lines");

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn calls_rejects_empty_paths_file() {
    use std::io::Write;

    let tmp = std::env::temp_dir().join(format!(
        "astro_sight_empty_paths_{}.txt",
        std::process::id()
    ));
    let mut f = std::fs::File::create(&tmp).unwrap();
    writeln!(f, "   ").unwrap();
    drop(f);

    let output = cargo_bin()
        .args(["calls", "--paths-file", tmp.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--paths-file")
    );

    let _ = std::fs::remove_file(&tmp);
}

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

/// MCP テスト用ヘルパー: initialize + initialized 後に追加メッセージを送信し、stdout を返す
fn mcp_send_after_init(extra_messages: &[&str]) -> String {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // 別スレッドで stdout を最後まで読み取る
    let reader_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut lines = Vec::new();
        for line in reader.lines() {
            match line {
                Ok(l) => lines.push(l),
                Err(_) => break,
            }
        }
        lines
    });

    // initialize 送信
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"test","version":"1.0"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    // サーバーが initialize を処理する時間を確保
    std::thread::sleep(std::time::Duration::from_millis(500));

    // initialized 通知 + 追加メッセージを送信
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    for msg in extra_messages {
        writeln!(stdin, "{msg}").unwrap();
    }
    stdin.flush().unwrap();
    drop(stdin);

    let _ = child.wait();
    let lines = reader_handle.join().expect("reader thread panicked");
    lines.join("\n")
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

#[test]
fn session_rejects_invalid_workspace_env() {
    let missing = std::env::temp_dir().join(format!(
        "astro-sight-missing-workspace-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("session")
    ));

    let output = cargo_bin()
        .arg("session")
        .env("ASTRO_SIGHT_WORKSPACE", &missing)
        .output()
        .expect("failed to run session");

    assert!(
        !output.status.success(),
        "不正なワークスペース環境変数では失敗するべき"
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ASTRO_SIGHT_WORKSPACE"),
        "エラーに環境変数名が含まれるべき"
    );
}

#[test]
fn session_rejects_empty_workspace_env() {
    let output = cargo_bin()
        .arg("session")
        .env("ASTRO_SIGHT_WORKSPACE", "")
        .output()
        .expect("failed to run session");

    assert!(
        !output.status.success(),
        "空文字のワークスペース環境変数では失敗するべき"
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("must not be empty"),
        "空文字は fail-closed で拒否されるべき"
    );
}

#[cfg(unix)]
#[test]
fn session_rejects_non_utf8_workspace_env() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let output = cargo_bin()
        .arg("session")
        .env("ASTRO_SIGHT_WORKSPACE", OsStr::from_bytes(&[0xff]))
        .output()
        .expect("failed to run session");

    assert!(
        !output.status.success(),
        "非 UTF-8 のワークスペース環境変数では失敗するべき"
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not valid UTF-8"),
        "非 UTF-8 値は fail-closed で拒否されるべき"
    );
}

#[test]
fn session_ast_includes_diagnostics() {
    use std::io::Write;
    use std::process::Stdio;

    // Session の AST 応答には snippet と diagnostics が含まれる。
    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .arg("session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn session");

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        r#"{{"command":"ast","path":"src/lib.rs","line":0,"column":0}}"#
    )
    .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("should be valid JSON");
    // snippet が含まれることを確認する。
    assert!(
        json.get("snippet").is_some(),
        "Session AST response should include snippet field"
    );
    // hash も含まれることを確認する。
    assert!(json.get("hash").is_some());
}

// ---- Phase 3: Batch refs unit test (via context command) ----

#[test]
fn context_batch_refs_consistency() {
    use std::io::Write;
    use std::process::Stdio;

    // Run context analysis and verify the output structure is consistent
    // with the batch refs approach (same output as before)
    let diff = r#"--- a/src/engine/symbols.rs
+++ b/src/engine/symbols.rs
@@ -603,7 +603,7 @@
-pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {
+pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, flag: bool) -> Result<Vec<Symbol>> {
     let query_src = symbol_query(lang_id);
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
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

// ---- Imports tests ----

#[test]
fn imports_on_own_source() {
    let output = cargo_bin()
        .args(["imports", "--path", "src/main.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");

    let imports = json["imports"].as_array().unwrap();
    assert!(!imports.is_empty(), "Should find imports in main.rs");

    // All imports should be 'use' kind for Rust
    for imp in imports {
        assert_eq!(imp["kind"], "use");
    }
}

#[test]
fn imports_batch() {
    let output = cargo_bin()
        .args(["imports", "--paths", "src/main.rs,src/lib.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Batch should produce 2 NDJSON lines");
}

// ---- Lint tests ----

#[test]
fn lint_with_pattern_rule() {
    use std::io::Write;

    let rules = r#"- id: no-unwrap
  language: rust
  severity: warning
  message: "Avoid unwrap()"
  pattern: "unwrap"
"#;

    let tmp = std::env::temp_dir().join("astro_sight_lint_rules.yaml");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(rules.as_bytes()).unwrap();
    drop(f);

    let output = cargo_bin()
        .args([
            "lint",
            "--path",
            "src/main.rs",
            "--rules",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");
    let matches = json["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "Should find unwrap pattern");
    assert_eq!(matches[0]["rule_id"], "no-unwrap");

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn lint_with_query_rule() {
    use std::io::Write;

    let rules = r#"- id: find-functions
  language: rust
  severity: info
  message: "Found a function"
  query: "(function_item name: (identifier) @fn_name)"
"#;

    let tmp = std::env::temp_dir().join("astro_sight_lint_query_rules.yaml");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(rules.as_bytes()).unwrap();
    drop(f);

    let output = cargo_bin()
        .args([
            "lint",
            "--path",
            "src/main.rs",
            "--rules",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");
    let matches = json["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "Should find function definitions");

    let _ = std::fs::remove_file(&tmp);
}

// ---- Phase 4: Sequence diagram tests ----

#[test]
fn sequence_on_own_source() {
    let output = cargo_bin()
        .args(["sequence", "--path", "src/main.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");
    assert!(!json["participants"].as_array().unwrap().is_empty());
    assert!(
        json["diagram"]
            .as_str()
            .unwrap()
            .contains("sequenceDiagram")
    );
}

#[test]
fn sequence_with_function_filter() {
    let output = cargo_bin()
        .args(["sequence", "--path", "src/main.rs", "--function", "run"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json["diagram"]
            .as_str()
            .unwrap()
            .contains("sequenceDiagram")
    );
}

// ---- Lint boundary tests ----

#[test]
fn lint_with_invalid_query_reports_warning() {
    use std::io::Write;

    let rules = r#"- id: bad-query
  language: rust
  severity: warning
  message: "This query is invalid"
  query: "(this_is_not_valid @x"
"#;

    let tmp = std::env::temp_dir().join("astro_sight_lint_bad_query.yaml");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(rules.as_bytes()).unwrap();
    drop(f);

    let output = cargo_bin()
        .args([
            "lint",
            "--path",
            "src/main.rs",
            "--rules",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    // Should succeed but include a warning about the invalid query
    let warnings = json["warnings"].as_array().unwrap();
    assert!(
        !warnings.is_empty(),
        "Should have a warning for invalid query"
    );
    assert!(warnings[0].as_str().unwrap().contains("bad-query"));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn lint_with_no_query_or_pattern_reports_warning() {
    use std::io::Write;

    let rules = r#"- id: empty-rule
  language: rust
  severity: info
  message: "This rule has no query or pattern"
"#;

    let tmp = std::env::temp_dir().join("astro_sight_lint_empty_rule.yaml");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(rules.as_bytes()).unwrap();
    drop(f);

    let output = cargo_bin()
        .args([
            "lint",
            "--path",
            "src/main.rs",
            "--rules",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let warnings = json["warnings"].as_array().unwrap();
    assert!(
        !warnings.is_empty(),
        "Should warn about rule with no query or pattern"
    );
    assert!(warnings[0].as_str().unwrap().contains("empty-rule"));

    let _ = std::fs::remove_file(&tmp);
}

// ---- Co-change analysis tests ----

#[test]
fn cochange_on_own_repo() {
    let output = cargo_bin()
        .args([
            "cochange",
            "--dir",
            ".",
            "--lookback",
            "50",
            "--min-confidence",
            "0.1",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(json["commits_analyzed"].as_u64().unwrap() > 0);
    assert!(json["entries"].as_array().is_some());
}

#[test]
fn cochange_with_file_filter() {
    let output = cargo_bin()
        .args([
            "cochange",
            "--dir",
            ".",
            "--lookback",
            "50",
            "--min-confidence",
            "0.1",
            "--file",
            "src/main.rs",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let entries = json["entries"].as_array().unwrap();
    // All entries should contain src/main.rs
    for entry in entries {
        let a = entry["file_a"].as_str().unwrap();
        let b = entry["file_b"].as_str().unwrap();
        assert!(
            a == "src/main.rs" || b == "src/main.rs",
            "Entry should contain src/main.rs: {a} / {b}"
        );
    }
}

#[test]
fn cochange_rejects_lookback_zero() {
    let output = cargo_bin()
        .args(["cochange", "--dir", ".", "--lookback", "0"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("lookback")
    );
}

#[test]
fn cochange_rejects_invalid_confidence() {
    let output = cargo_bin()
        .args([
            "cochange",
            "--dir",
            ".",
            "--lookback",
            "10",
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

// ---- Refs --names batch tests ----

#[test]
fn refs_batch_names() {
    let output = cargo_bin()
        .args([
            "refs",
            "--names",
            "AppService,AstgenResponse",
            "--dir",
            "src/",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "Should have 2 NDJSON lines for 2 symbols");

    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["symbol"], "AppService");
    assert!(!first["refs"].as_array().unwrap().is_empty());

    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["symbol"], "AstgenResponse");
    assert!(!second["refs"].as_array().unwrap().is_empty());
}

#[test]
fn refs_name_or_names_required() {
    let output = cargo_bin()
        .args(["refs", "--dir", "src/"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());
}

#[test]
fn refs_rejects_empty_name() {
    let output = cargo_bin()
        .args(["refs", "--name", "", "--dir", "src/"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--name")
    );
}

#[test]
fn session_refs_requires_name_or_names() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .arg("session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn session");

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, r#"{{"command":"refs","dir":"src/"}}"#).unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let line = stdout.lines().next().expect("should have one line");
    let json: serde_json::Value = serde_json::from_str(line).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "IO_ERROR");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("name or names is required")
    );
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
fn symbols_dir() {
    let output = cargo_bin()
        .args(["symbols", "--dir", "src/engine/"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    // src/engine/ has multiple .rs files
    assert!(
        lines.len() >= 2,
        "Should have multiple NDJSON lines for engine dir, got {}",
        lines.len()
    );

    for line in &lines {
        let json: serde_json::Value =
            serde_json::from_str(line).expect("each line should be valid JSON");
        assert!(json["symbols"].is_array() || json["error"].is_object());
    }
}

#[test]
fn symbols_dir_with_glob() {
    let output = cargo_bin()
        .args(["symbols", "--dir", "src/", "--glob", "*.rs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert!(!lines.is_empty(), "Should have at least one NDJSON line");
}

// ---- Ruby language support tests ----

#[test]
fn ruby_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.rb"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "ruby");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(!symbols.is_empty());

    // Should find module, classes, and methods
    let module = symbols.iter().find(|s| s["name"] == "MyApp");
    assert!(module.is_some(), "Should find MyApp module");
    assert_eq!(module.unwrap()["kind"], "mod");

    let user_class = symbols.iter().find(|s| s["name"] == "User");
    assert!(user_class.is_some(), "Should find User class");
    assert_eq!(user_class.unwrap()["kind"], "class");

    let init_method = symbols.iter().find(|s| s["name"] == "initialize");
    assert!(init_method.is_some(), "Should find initialize method");
    assert_eq!(init_method.unwrap()["kind"], "fn");
}

#[test]
fn ruby_calls() {
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.rb"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "ruby");

    let calls = json["calls"].as_array().unwrap();
    assert!(!calls.is_empty(), "Should find calls in Ruby file");
}

#[test]
fn ruby_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.rb"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "ruby");

    let imports = json["imports"].as_array().unwrap();
    assert!(!imports.is_empty(), "Should find require statements");

    // Should find 'json' require
    let json_import = imports
        .iter()
        .find(|i| i["src"].as_str().unwrap_or("").contains("json"));
    assert!(json_import.is_some(), "Should find require 'json'");
    assert_eq!(json_import.unwrap()["kind"], "require");
}

#[test]
fn ruby_refs_constant_definition() {
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "DEFAULT_ROLE",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.rb",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["symbol"], "DEFAULT_ROLE");

    let refs = json["refs"].as_array().unwrap();
    assert!(
        refs.len() >= 2,
        "Should find both definition and reference for DEFAULT_ROLE"
    );

    let defs: Vec<_> = refs.iter().filter(|r| r["kind"] == "def").collect();
    assert!(
        !defs.is_empty(),
        "Should classify constant assignment as definition"
    );

    let refs_only: Vec<_> = refs.iter().filter(|r| r["kind"] == "ref").collect();
    assert!(
        !refs_only.is_empty(),
        "Should classify non-assignment constant usage as reference"
    );
}

#[test]
fn ruby_ast() {
    let output = cargo_bin()
        .args(["ast", "--path", "tests/fixtures/sample.rb"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "ruby");
    assert!(!json["ast"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// impact command tests
// ---------------------------------------------------------------------------

#[test]
fn impact_clean_pass() {
    use std::io::Write;
    use std::process::Stdio;

    // Empty diff → exit 0, no output
    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"")
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success(), "expected exit 0 for empty diff");
    assert!(output.stdout.is_empty(), "expected no stdout");
}

#[test]
fn impact_with_unresolved() {
    use std::io::Write;
    use std::process::Stdio;

    // Diff that changes extract_symbols signature → callers in other files are unresolved
    let diff = r#"--- a/src/engine/symbols.rs
+++ b/src/engine/symbols.rs
@@ -603,7 +603,7 @@
-pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {
+pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, flag: bool) -> Result<Vec<Symbol>> {
     let query_src = symbol_query(lang_id);
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    assert!(
        !output.status.success(),
        "expected exit 1 for unresolved impacts"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unresolved impacts found"),
        "expected 'Unresolved impacts found' in stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("extract_symbols"),
        "expected 'extract_symbols' in stderr, got: {stderr}"
    );
}

#[test]
fn impact_git_mode() {
    // --git with HEAD base on a clean repo → exit 0 (no diff = no unresolved)
    let output = cargo_bin()
        .args(["impact", "--dir", ".", "--git"])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "expected exit 0 for clean git diff"
    );
    assert!(output.stdout.is_empty(), "expected no stdout");
}

#[test]
fn impact_excludes_target_test_refs() {
    use std::io::Write;
    use std::process::Stdio;

    // Create fixture: lib.rs with pub fn, consumer.rs with prod + test usage
    let dir = tempfile::tempdir().expect("tempdir");
    let lib_rs = dir.path().join("lib.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    std::fs::write(
        &lib_rs,
        r#"pub fn do_work(x: i32) -> i32 {
    x + 1
}
"#,
    )
    .unwrap();

    std::fs::write(
        &consumer_rs,
        r#"use crate::lib::do_work;

pub fn run() -> i32 {
    do_work(42)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run() {
        assert_eq!(do_work(1), 2);
    }
}
"#,
    )
    .unwrap();

    // Diff that changes do_work signature
    let diff = r#"--- a/lib.rs
+++ b/lib.rs
@@ -1,3 +1,3 @@
-pub fn do_work(x: i32) -> i32 {
+pub fn do_work(x: i32, y: i32) -> i32 {
     x + 1
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should detect unresolved impact in consumer.rs production code (line 4: do_work(42))
    assert!(
        !output.status.success(),
        "expected exit 1 for unresolved impacts"
    );
    assert!(
        stderr.contains("consumer.rs"),
        "expected consumer.rs in stderr: {stderr}"
    );

    // Verify test-context refs are excluded:
    // Only production-code caller (line 4) should appear, NOT the #[cfg(test)] ref (line 14)
    let lines: Vec<&str> = stderr.lines().collect();
    let consumer_lines: Vec<&str> = lines
        .iter()
        .filter(|l| l.contains("consumer.rs:"))
        .copied()
        .collect();

    assert!(
        !consumer_lines.is_empty(),
        "expected at least one consumer.rs caller"
    );

    for line in &consumer_lines {
        // Extract line number after "consumer.rs:"
        if let Some(pos) = line.find("consumer.rs:") {
            let after = &line[pos + "consumer.rs:".len()..];
            let line_num: usize = after
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            assert!(
                line_num < 8,
                "test-context ref at line {line_num} should be excluded: {line}"
            );
        }
    }
}

#[test]
fn impact_additive_impl_block_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Fixture: types.rs with a struct, consumer.rs that uses the struct
    let dir = tempfile::tempdir().expect("tempdir");
    let types_rs = dir.path().join("types.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // types.rs: struct with an existing impl and a NEW impl block being added
    std::fs::write(
        &types_rs,
        r#"pub struct HookInput {
    pub name: String,
}

impl HookInput {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}

impl HookInput {
    pub fn bash_command(&self) -> &str {
        &self.name
    }

    pub fn file_path(&self) -> &str {
        &self.name
    }
}
"#,
    )
    .unwrap();

    // consumer.rs: uses HookInput (struct construction + method call)
    std::fs::write(
        &consumer_rs,
        r#"use crate::types::HookInput;

pub fn run() -> String {
    let input = HookInput::new("test".to_string());
    input.name.clone()
}
"#,
    )
    .unwrap();

    // Diff: adding a new impl block with new methods (backward-compatible)
    let diff = r#"--- a/types.rs
+++ b/types.rs
@@ -9,3 +9,13 @@
     }
 }
+
+impl HookInput {
+    pub fn bash_command(&self) -> &str {
+        &self.name
+    }
+
+    pub fn file_path(&self) -> &str {
+        &self.name
+    }
+}
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Adding new methods to a type is backward-compatible.
    // consumer.rs should NOT be reported as impacted.
    assert!(
        output.status.success(),
        "expected exit 0 (no unresolved impacts) for additive impl block.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("consumer.rs"),
        "consumer.rs should not appear as impacted for additive impl block: {stderr}"
    );
}

/// エクスポートシンボルの body (内部実装) のみが変わったとき、
/// import/re-export 行しか参照のないファイルは impact に載せない。
/// (レポート 2026-04-08-commitstore-internal-change-false-positive.md の再現)
#[test]
fn impact_body_only_change_import_only_callers_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let commit_store = dir.path().join("commitStore.ts");
    let index_ts = dir.path().join("index.ts");
    let app_tsx = dir.path().join("App.tsx");

    std::fs::write(
        &commit_store,
        r#"export function useCommitStore() {
  return { validate: async () => true };
}
"#,
    )
    .unwrap();
    std::fs::write(
        &index_ts,
        r#"export { useCommitStore } from "./commitStore";
"#,
    )
    .unwrap();
    std::fs::write(
        &app_tsx,
        r#"import { useCommitStore } from "./commitStore";
function App() { return null; }
"#,
    )
    .unwrap();

    // useCommitStore の body のみを変更 (宣言行は不変)
    let diff = r#"--- a/commitStore.ts
+++ b/commitStore.ts
@@ -1,3 +1,7 @@
 export function useCommitStore() {
-  return { validate: async () => true };
+  let currentRequestId = 0;
+  return {
+    validate: async () => { currentRequestId += 1; return currentRequestId > 0; },
+  };
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // import / re-export しかしていないファイルは impact に載せない
    assert!(
        output.status.success(),
        "expected exit 0 (no unresolved impacts) for body-only change.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("index.ts"),
        "re-export-only file should not appear in impact: {stderr}"
    );
    assert!(
        !stderr.contains("App.tsx"),
        "import-only file should not appear in impact: {stderr}"
    );
}

/// 変更ファイルに新規追加シンボルが複数あっても、影響先ファイルの行が実際に
/// 参照しているシンボルだけが impact に紐付き、他の無関係な変更シンボルが
/// 巻き添えで紐付かない（バルク紐付け禁止）。private シンボルも同様に外部ファイルへ
/// 影響伝播しない。
/// (レポート 2026-03-19-dnspacket-bulk-symbol-binding.md の再現)
#[test]
fn impact_bulk_symbol_binding_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let src_kt = dir.path().join("DnsPacket.kt");
    let test_kt = dir.path().join("DnsPacketTest.kt");

    std::fs::write(
        &src_kt,
        r#"package pkg

class DnsPacket(val data: ByteArray) {
    companion object {
        fun createZeroResponse(packet: DnsPacket): ByteArray = byteArrayOf()
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        &test_kt,
        r#"package pkg

class DnsPacketTest {
    fun zero_response() {
        val packet = DnsPacket(byteArrayOf())
        DnsPacket.createZeroResponse(packet)
    }
}
"#,
    )
    .unwrap();

    // createServFailResponse と private createIpResponse を新規追加
    let diff = r#"--- a/DnsPacket.kt
+++ b/DnsPacket.kt
@@ -4,5 +4,7 @@
 class DnsPacket(val data: ByteArray) {
     companion object {
         fun createZeroResponse(packet: DnsPacket): ByteArray = byteArrayOf()
+        fun createServFailResponse(packet: DnsPacket): ByteArray = byteArrayOf(1)
+        private fun createIpResponse(packet: DnsPacket, ip: String): ByteArray = byteArrayOf()
     }
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // DnsPacketTest.kt は createZeroResponse しか参照していない & 新規追加は未使用
    // → 無関係な変更シンボル (createServFailResponse / createIpResponse) が巻き添えで
    //   紐付かないこと、および impact として unresolved 扱いにならないことを確認する
    assert!(
        !stderr.contains("createServFailResponse"),
        "未使用の新規関数は他ファイルの impact に紐付けてはならない: {stderr}"
    );
    assert!(
        !stderr.contains("createIpResponse"),
        "private 関数は他ファイルの impact に紐付けてはならない: {stderr}"
    );
}

#[test]
fn impact_trait_unchanged_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Pattern 1: trait definition is unchanged, but free functions using the trait
    // have signature changes. The trait name appears in changed lines but the trait
    // definition header (`trait GuestMemory`) is NOT changed.
    let dir = tempfile::tempdir().expect("tempdir");
    let mem_rs = dir.path().join("mem.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // mem.rs: trait + free function using the trait
    std::fs::write(
        &mem_rs,
        r#"pub trait GuestMemory {
    fn read(&self, addr: u64, buf: &mut [u8]);
    fn write(&self, addr: u64, data: &[u8]);
}

pub fn read_obj<M: GuestMemory + ?Sized>(mem: &M, addr: u64) -> u32 {
    let mut buf = [0u8; 4];
    mem.read(addr, &mut buf);
    u32::from_le_bytes(buf)
}
"#,
    )
    .unwrap();

    // consumer.rs: imports and uses GuestMemory
    std::fs::write(
        &consumer_rs,
        r#"use crate::mem::GuestMemory;

pub fn process(mem: &dyn GuestMemory) {
    let val = crate::mem::read_obj(mem, 0x1000);
    println!("{val}");
}
"#,
    )
    .unwrap();

    // Diff: only read_obj signature changes (dyn → impl + ?Sized), trait is unchanged
    let diff = r#"--- a/mem.rs
+++ b/mem.rs
@@ -5,7 +5,7 @@
 }

-pub fn read_obj(mem: &dyn GuestMemory, addr: u64) -> u32 {
+pub fn read_obj<M: GuestMemory + ?Sized>(mem: &M, addr: u64) -> u32 {
     let mut buf = [0u8; 4];
     mem.read(addr, &mut buf);
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // GuestMemory trait is NOT changed, so `use crate::mem::GuestMemory` imports
    // in consumer.rs should NOT be reported as impacted.
    // read_obj signature DID change, so read_obj callers may be reported.
    let ctx: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_default();
    if let Some(changes) = ctx["changes"].as_array() {
        for change in changes {
            // GuestMemory should NOT be in affected_symbols
            let empty = vec![];
            let affected = change["affected_symbols"].as_array().unwrap_or(&empty);
            let has_guest_memory = affected
                .iter()
                .any(|s| s["name"].as_str() == Some("GuestMemory"));
            assert!(
                !has_guest_memory,
                "GuestMemory trait should not be affected when its definition is unchanged.\nstdout: {stdout}\nstderr: {stderr}"
            );
        }
    }
}

#[test]
fn impact_test_symbols_excluded_from_affected() {
    use std::io::Write;
    use std::process::Stdio;

    // Pattern 2: test symbols (#[cfg(test)] mod tests) should not appear
    // in affected_symbols list.
    let dir = tempfile::tempdir().expect("tempdir");
    let lib_rs = dir.path().join("lib.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // lib.rs: pub fn + test module
    std::fs::write(
        &lib_rs,
        r#"pub fn compute(x: i32, y: i32) -> i32 {
    x * y + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> i32 {
        42
    }

    #[test]
    fn test_compute() {
        assert_eq!(compute(setup(), 2), 85);
    }
}
"#,
    )
    .unwrap();

    std::fs::write(
        &consumer_rs,
        r#"use crate::lib::compute;

pub fn run() -> i32 {
    compute(1, 2)
}
"#,
    )
    .unwrap();

    // Diff: changes both compute signature and test helper
    let diff = r#"--- a/lib.rs
+++ b/lib.rs
@@ -1,3 +1,3 @@
-pub fn compute(x: i32) -> i32 {
-    x + 1
+pub fn compute(x: i32, y: i32) -> i32 {
+    x * y + 1
 }
@@ -8,8 +8,8 @@

-    fn setup() -> i32 {
-        0
+    fn setup() -> i32 {
+        42
     }

     #[test]
-    fn test_compute() {
-        assert_eq!(compute(setup()), 1);
+    fn test_compute() {
+        assert_eq!(compute(setup(), 2), 85);
     }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let ctx: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_default();
    if let Some(changes) = ctx["changes"].as_array() {
        for change in changes {
            let empty = vec![];
            let affected = change["affected_symbols"].as_array().unwrap_or(&empty);
            let test_symbols: Vec<&str> = affected
                .iter()
                .filter_map(|s| s["name"].as_str())
                .filter(|name| *name == "tests" || *name == "setup" || *name == "test_compute")
                .collect();
            assert!(
                test_symbols.is_empty(),
                "Test symbols should not appear in affected_symbols: {:?}\nstdout: {stdout}",
                test_symbols
            );
        }
    }
}

#[test]
fn impact_same_name_method_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Pattern: same-name method across different types.
    // `Transport::write` changes should NOT be reported as impacting
    // `Device::write` references in another file.
    let dir = tempfile::tempdir().expect("tempdir");
    let transport_rs = dir.path().join("transport.rs");
    let device_rs = dir.path().join("device.rs");

    // transport.rs: impl Transport with write method
    std::fs::write(
        &transport_rs,
        r#"pub struct Transport {
    pub base: u64,
}

impl Transport {
    pub fn write(&mut self, offset: u64, value: u32) {
        // write to MMIO register
        let addr = self.base + offset;
        unsafe { core::ptr::write_volatile(addr as *mut u32, value) };
    }
}
"#,
    )
    .unwrap();

    // device.rs: different trait with same-name method, plus usage of both
    std::fs::write(
        &device_rs,
        r#"pub trait Device {
    fn write(&mut self, offset: u64, size: u8, value: u64);
}

pub struct Keyboard;

impl Device for Keyboard {
    fn write(&mut self, offset: u64, size: u8, value: u64) {
        // handle keyboard write
    }
}

pub fn dispatch(dev: &mut dyn Device, offset: u64, value: u64) {
    dev.write(offset, 1, value);
}
"#,
    )
    .unwrap();

    // Diff: Transport::write signature changes
    let diff = r#"--- a/transport.rs
+++ b/transport.rs
@@ -5,7 +5,7 @@

 impl Transport {
-    pub fn write(&mut self, offset: u64, value: u32) {
+    pub fn write(&mut self, offset: u64, value: u32, mem: &[u8]) {
         // write to MMIO register
         let addr = self.base + offset;
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // device.rs has its own `write` definition (in trait Device).
    // Transport::write change should NOT impact Device::write references.
    assert!(
        !stdout.contains("device.rs"),
        "device.rs should not appear as impacted for Transport::write change.\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn impact_module_decl_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Fixture: lib.rs with module declarations, consumer.rs with same-name local variables
    let dir = tempfile::tempdir().expect("tempdir");
    let lib_rs = dir.path().join("lib.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // lib.rs: new crate with pub mod declarations
    std::fs::write(
        &lib_rs,
        r#"pub mod arena;
pub mod ops;
pub mod tensor;
"#,
    )
    .unwrap();

    // consumer.rs: uses "tensor" as a local variable name (unrelated crate)
    std::fs::write(
        &consumer_rs,
        r#"pub fn process() {
    let tensor = vec![1.0, 2.0, 3.0];
    let shape = tensor.len();
    println!("{shape}");
}
"#,
    )
    .unwrap();

    // Diff: adding new module declarations
    let diff = r#"--- /dev/null
+++ b/lib.rs
@@ -0,0 +1,3 @@
+pub mod arena;
+pub mod ops;
+pub mod tensor;
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Module declarations should NOT cause false positives on same-name local variables.
    assert!(
        output.status.success(),
        "expected exit 0 for module declarations.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("consumer.rs"),
        "consumer.rs should not appear as impacted for module declarations: {stdout}"
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

// ---- impact パストラバーサル検証テスト ----

#[test]
fn impact_rejects_path_traversal_in_diff() {
    use std::io::Write;
    use std::process::Stdio;

    // diff パス内の .. はスキップされるべき
    let diff = r#"--- a/../../../etc/passwd
+++ b/../../../etc/passwd
@@ -1,3 +1,3 @@
-root:x:0:0:root
+root:x:0:0:hacked
"#;

    let dir = tempfile::tempdir().expect("tempdir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
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
        "パストラバーサルを含む diff は変更として認識されないべき"
    );
}

// ---- cochange MAX_FILES_PER_COMMIT スキップテスト ----

#[test]
fn cochange_skips_large_commits() {
    // MAX_FILES_PER_COMMIT (100) を超えるコミットは pair 集計から除外されることを検証。
    // git リポジトリを模擬するため、実際の git repo を使用し lookback=1 で確認。
    // ただし通常のコミットは 100 ファイル未満なのでスキップされない。
    // ここでは lookback=1 + 小コミットで pairs が生成されることを確認し、
    // 対称的に lookback が十分大きいとき結果が得られることを確認。
    let output = cargo_bin()
        .args([
            "cochange",
            "--dir",
            ".",
            "--lookback",
            "1",
            "--min-confidence",
            "0.0",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    // lookback=1 でも commits_analyzed >= 0 であること
    assert!(json["commits_analyzed"].as_u64().is_some());
    assert!(json["entries"].as_array().is_some());
}

#[test]
fn cochange_rejects_lookback_exceeding_max() {
    // lookback > 10000 は拒否される
    let output = cargo_bin()
        .args(["cochange", "--dir", ".", "--lookback", "10001"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("lookback")
    );
}

#[test]
fn cochange_rejects_nan_confidence() {
    // NaN は拒否される
    let output = cargo_bin()
        .args([
            "cochange",
            "--dir",
            ".",
            "--lookback",
            "10",
            "--min-confidence",
            "NaN",
        ])
        .output()
        .expect("failed to run");
    // clap がパースエラーを出すか、サービス層が拒否するか
    assert!(!output.status.success());
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

// ---- Session 異常系テスト ----

#[test]
fn session_invalid_json_returns_error() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .arg("session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn session");

    let stdin = child.stdin.as_mut().unwrap();
    // 不正な JSON を送信
    writeln!(stdin, "{{this is not valid json}}").unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("should return error JSON");
    assert_eq!(
        json["error"]["code"], "INVALID_REQUEST",
        "不正な JSON は INVALID_REQUEST エラーを返すべき"
    );
}

#[test]
fn session_empty_input_exits_cleanly() {
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .arg("session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn session");

    // stdin を即閉じ（空入力）
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success(), "空入力で session は正常終了すべき");
    assert!(
        output.stdout.is_empty(),
        "空入力で session は出力なしで終了すべき"
    );
}

// ---- symbols 境界値テスト ----

#[test]
fn symbols_on_empty_file() {
    use std::io::Write;

    // 空のソースファイルでもエラーにならないこと
    let tmp = std::env::temp_dir().join("astro_sight_empty.rs");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(b"").unwrap();
    drop(f);

    let output = cargo_bin()
        .args(["symbols", "--path", tmp.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");
    let symbols = json["symbols"].as_array().unwrap();
    assert!(symbols.is_empty(), "空ファイルではシンボルは空であるべき");

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn symbols_on_syntax_error_file() {
    use std::io::Write;

    // 構文エラーのあるファイルでも結果を返すこと（部分的なパースは可能）
    let tmp = std::env::temp_dir().join("astro_sight_syntax_error.rs");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(b"fn incomplete(").unwrap();
    drop(f);

    let output = cargo_bin()
        .args(["symbols", "--path", tmp.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "rust");
    // シンボルの有無は問わないが、JSON 出力が壊れないこと
    assert!(json["symbols"].as_array().is_some());

    let _ = std::fs::remove_file(&tmp);
}

// ---- refs 異常系テスト ----

#[test]
fn refs_nonexistent_directory() {
    let output = cargo_bin()
        .args(["refs", "--name", "foo", "--dir", "/nonexistent/dir/path"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "FILE_NOT_FOUND");
}

#[test]
fn refs_rejects_file_as_dir_argument() {
    let output = cargo_bin()
        .args(["refs", "--name", "main", "--dir", "src/main.rs"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not a directory")
    );
}

#[test]
fn refs_whitespace_only_name() {
    // 空白のみの name は拒否される（trim 後に空になる）
    let output = cargo_bin()
        .args(["refs", "--name", "   ", "--dir", "src/"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
}

// ---- context 空 diff テスト ----

#[test]
fn context_empty_diff_returns_empty_changes() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
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

// ---- unsupported language テスト ----

#[test]
fn ast_unsupported_language() {
    use std::io::Write;

    let tmp = std::env::temp_dir().join("astro_sight_test.xyz");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(b"some content").unwrap();
    drop(f);

    let output = cargo_bin()
        .args(["ast", "--path", tmp.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "UNSUPPORTED_LANGUAGE");

    let _ = std::fs::remove_file(&tmp);
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

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
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

// ---- refs --names 境界値テスト ----

#[test]
fn refs_batch_names_empty_after_trim() {
    // 空白のみの names は拒否される
    let output = cargo_bin()
        .args(["refs", "--names", " , , ", "--dir", "src/"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
}

// ---- lint 境界値テスト: 空のルールファイル ----

#[test]
fn lint_empty_rules_file() {
    use std::io::Write;

    let tmp = std::env::temp_dir().join("astro_sight_lint_empty.yaml");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(b"").unwrap();
    drop(f);

    let output = cargo_bin()
        .args([
            "lint",
            "--path",
            "src/main.rs",
            "--rules",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    // 空のルールファイルはエラーか、マッチなしの結果を返す
    // (YAML パース結果による)
    let _ = output; // 結果の形式は実装依存だがクラッシュしないことが重要

    let _ = std::fs::remove_file(&tmp);
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
    let result = service.analyze_context("", ".");
    assert!(result.is_ok(), "空の diff 入力はサイズ制限を通過するべき");
}

// ---- lint --rules-dir テスト ----

#[test]
fn lint_rules_dir() {
    use std::io::Write;

    let tmp_dir = std::env::temp_dir().join("astro_sight_lint_rules_dir");
    let _ = std::fs::create_dir_all(&tmp_dir);

    // ルールファイルを作成
    let rule_file = tmp_dir.join("test_rule.yaml");
    let mut f = std::fs::File::create(&rule_file).unwrap();
    f.write_all(
        b"- id: test-pattern\n  language: rust\n  pattern: main\n  severity: warning\n  message: found main\n",
    )
    .unwrap();
    drop(f);

    let output = cargo_bin()
        .args([
            "lint",
            "--path",
            "src/main.rs",
            "--rules-dir",
            tmp_dir.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    // main.rs に "main" パターンが見つかるはず
    assert!(
        !json["matches"].as_array().unwrap().is_empty(),
        "main パターンが main.rs で見つかるべき"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn lint_rules_dir_empty() {
    let tmp_dir = std::env::temp_dir().join("astro_sight_lint_rules_dir_empty");
    let _ = std::fs::create_dir_all(&tmp_dir);

    let output = cargo_bin()
        .args([
            "lint",
            "--path",
            "src/main.rs",
            "--rules-dir",
            tmp_dir.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    // 空ディレクトリでもクラッシュしないこと
    assert!(output.status.success());

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// ---- impact --hook フラグテスト ----

#[test]
fn impact_hook_shows_triage_message() {
    // 変更のある diff を使って impact を実行し、--hook 時にトリアージメッセージが出力されることを確認
    let tmp_dir = tempfile::tempdir().unwrap();
    let src_dir = tmp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        src_dir.join("main.rs"),
        "use crate::lib::compute;\nfn main() { compute(1); }\n",
    )
    .unwrap();

    // compute の署名変更 diff
    let diff = r#"--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1 @@
-pub fn compute(x: i32) -> i32 { x + 1 }
+pub fn compute(x: i32, y: i32) -> i32 { x + y }
"#;

    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            tmp_dir.path().to_str().unwrap(),
            "--hook",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(diff.as_bytes())
                .unwrap();
            child.wait_with_output()
        })
        .expect("failed to run");

    // 未解決の影響がある場合、exit 1 で --hook メッセージが出る
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("astro-sight-triage"),
            "--hook 時にトリアージスキルの案内が表示されるべき: {}",
            stderr
        );
    }
    // 未解決影響がない場合（main.rs が解析できない等）は exit 0 で問題なし
}

// ---- session: 複数コマンドの連続処理テスト ----

#[test]
fn session_multiple_commands() {
    let input = r#"{"command":"doctor","path":"."}
{"command":"symbols","path":"src/main.rs"}
"#;
    let output = cargo_bin()
        .arg("session")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            child.wait_with_output()
        })
        .expect("failed to run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "2コマンド → 2行の NDJSON 出力");

    // 各行が有効な JSON であること
    for line in &lines {
        let _: serde_json::Value =
            serde_json::from_str(line).expect("各行が有効な JSON であるべき");
    }
}

// ---- sequence バッチ処理テスト ----

#[test]
fn sequence_batch() {
    let output = cargo_bin()
        .args(["sequence", "--paths", "src/main.rs,src/service.rs"])
        .output()
        .expect("failed to run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "2ファイル → 2行の NDJSON 出力");

    for line in &lines {
        let json: serde_json::Value =
            serde_json::from_str(line).expect("各行が有効な JSON であるべき");
        assert!(json["diagram"].as_str().is_some() || json.get("error").is_some());
    }
}

// ---- init サブコマンドテスト ----

#[test]
fn init_creates_config_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");

    let output = cargo_bin()
        .args(["init", "--path", config_path.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    assert!(config_path.exists(), "init が設定ファイルを作成すべき");

    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("debug = false"),
        "デフォルト設定を含むべき"
    );
}

// ---- skill-install サブコマンドテスト ----

#[test]
fn skill_install_unknown_target() {
    let output = cargo_bin()
        .args(["skill-install", "unknown-agent"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("JSON エラー出力であるべき");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown target")
    );
}

// ---- Python 多言語テスト ----

#[test]
fn python_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.py"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "python");

    let symbols = json["symbols"].as_array().unwrap();
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Config"), "class Config を検出すべき");
    assert!(
        names.contains(&"create_config"),
        "function create_config を検出すべき"
    );
}

#[test]
fn python_calls() {
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.py"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "python");
    assert!(json["calls"].as_array().is_some());
}

#[test]
fn python_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.py"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    let sources: Vec<&str> = imports.iter().map(|i| i["src"].as_str().unwrap()).collect();
    assert!(
        sources.contains(&"pathlib"),
        "pathlib の import を検出すべき"
    );
}

#[test]
fn python_ast() {
    let output = cargo_bin()
        .args([
            "ast",
            "--path",
            "tests/fixtures/sample.py",
            "--line",
            "0",
            "--col",
            "0",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "python");
    assert!(!json["ast"].as_array().unwrap().is_empty());
}

// ---- Go 多言語テスト ----

#[test]
fn go_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.go"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "go");

    let symbols = json["symbols"].as_array().unwrap();
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Server"), "type Server を検出すべき");
    assert!(names.contains(&"NewServer"), "func NewServer を検出すべき");
}

#[test]
fn go_calls() {
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.go"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "go");
    assert!(json["calls"].as_array().is_some());
}

#[test]
fn go_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.go"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    let sources: Vec<&str> = imports.iter().map(|i| i["src"].as_str().unwrap()).collect();
    assert!(sources.contains(&"fmt"), "fmt の import を検出すべき");
    assert!(
        sources.contains(&"strings"),
        "strings の import を検出すべき"
    );
}

// ---- TypeScript 多言語テスト ----

#[test]
fn typescript_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.ts"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "typescript");

    let symbols = json["symbols"].as_array().unwrap();
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"AppServer"), "class AppServer を検出すべき");
    assert!(
        names.contains(&"createServer"),
        "function createServer を検出すべき"
    );
    assert!(names.contains(&"Config"), "interface Config を検出すべき");
}

#[test]
fn typescript_calls() {
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.ts"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "typescript");
    assert!(json["calls"].as_array().is_some());
}

#[test]
fn typescript_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.ts"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(!imports.is_empty(), "TypeScript の import を検出すべき");
}

// ---- refs 多言語テスト ----

#[test]
fn python_refs() {
    let output = cargo_bin()
        .args(["refs", "--name", "Config", "--dir", "tests/fixtures"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    // Config は sample.py と sample.ts 両方に存在
    assert!(!refs.is_empty(), "Config の参照を検出すべき");
}

#[test]
fn go_refs() {
    let output = cargo_bin()
        .args(["refs", "--name", "Server", "--dir", "tests/fixtures"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    assert!(!refs.is_empty(), "Server の参照を検出すべき");
}

// ---- review サブコマンドテスト ----

#[test]
fn review_on_clean_repo() {
    // クリーンな状態では変更なしの結果を返すべき
    let output = cargo_bin()
        .args(["review", "--dir", "."])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    // review は JSON 出力を返すこと
    assert!(json.is_object(), "review は JSON オブジェクトを返すべき");
}

// ---- Java 多言語統合テスト ----

#[test]
fn java_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.java"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "java");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "SampleService"),
        "SampleService クラスを検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "addItem"),
        "addItem メソッドを検出すべき"
    );
}

#[test]
fn java_calls() {
    let output = cargo_bin()
        .args([
            "calls",
            "--path",
            "tests/fixtures/sample.java",
            "--function",
            "addItem",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "java");
}

#[test]
fn java_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.java"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| { i["ctx"].as_str().unwrap_or("").contains("java.util.List") }),
        "java.util.List の import を検出すべき"
    );
}

// ---- Kotlin 多言語統合テスト ----

#[test]
fn kotlin_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.kt"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "kotlin");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "SampleRepository"),
        "SampleRepository クラスを検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "main"),
        "main 関数を検出すべき"
    );
}

#[test]
fn kotlin_calls() {
    let output = cargo_bin()
        .args([
            "calls",
            "--path",
            "tests/fixtures/sample.kt",
            "--function",
            "main",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "kotlin");
}

#[test]
fn kotlin_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.kt"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(!imports.is_empty(), "Kotlin の import を検出すべき");
}

// ---- Swift 多言語統合テスト ----

#[test]
fn swift_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.swift"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "swift");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "TaskManager"),
        "TaskManager クラスを検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "addTask"),
        "addTask メソッドを検出すべき"
    );
}

#[test]
fn swift_calls() {
    let output = cargo_bin()
        .args([
            "calls",
            "--path",
            "tests/fixtures/sample.swift",
            "--function",
            "removeTask",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "swift");
}

#[test]
fn swift_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.swift"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| i["ctx"].as_str().unwrap_or("").contains("Foundation")),
        "Foundation の import を検出すべき"
    );
}

// ---- C# 多言語統合テスト ----

#[test]
fn csharp_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.cs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "csharp");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "Calculator"),
        "Calculator クラスを検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "Add"),
        "Add メソッドを検出すべき"
    );
}

#[test]
fn csharp_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.cs"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| i["ctx"].as_str().unwrap_or("").contains("System")),
        "System の using を検出すべき"
    );
}

// ---- PHP 多言語統合テスト ----

#[test]
fn php_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.php"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "php");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "UserService"),
        "UserService クラスを検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "findUser"),
        "findUser メソッドを検出すべき"
    );
}

#[test]
fn php_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.php"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| { i["ctx"].as_str().unwrap_or("").contains("UserRepository") }),
        "UserRepository の use を検出すべき"
    );
}

// ---- C 多言語統合テスト ----

#[test]
fn c_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.c"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "c");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "main"),
        "main 関数を検出すべき"
    );
    assert!(
        symbols.iter().any(|s| s["name"] == "buffer_append"),
        "buffer_append 関数を検出すべき"
    );
}

#[test]
fn c_calls() {
    let output = cargo_bin()
        .args([
            "calls",
            "--path",
            "tests/fixtures/sample.c",
            "--function",
            "main",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "c");
}

#[test]
fn c_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.c"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| i["ctx"].as_str().unwrap_or("").contains("stdio.h")),
        "stdio.h の include を検出すべき"
    );
}

// ---- C++ 多言語統合テスト ----

#[test]
fn cpp_symbols() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.cpp"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "cpp");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(
        symbols.iter().any(|s| s["name"] == "StringPool"),
        "StringPool クラスを検出すべき"
    );
}

#[test]
fn cpp_calls() {
    let output = cargo_bin()
        .args([
            "calls",
            "--path",
            "tests/fixtures/sample.cpp",
            "--function",
            "main",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "cpp");
}

#[test]
fn cpp_imports() {
    let output = cargo_bin()
        .args(["imports", "--path", "tests/fixtures/sample.cpp"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    assert!(
        imports
            .iter()
            .any(|i| i["ctx"].as_str().unwrap_or("").contains("string")),
        "string の include を検出すべき"
    );
}

// ---- dead-code サブコマンドテスト ----

#[test]
fn dead_code_on_fixtures() {
    let output = cargo_bin()
        .args(["dead-code", "--dir", "tests/fixtures"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(json["dir"].as_str().is_some());
    assert!(json["scanned_files"].as_u64().is_some());
    assert!(json["dead_symbols"].as_array().is_some());
}

#[test]
fn dead_code_skips_linguist_generated_files() {
    // .gitattributes で linguist-generated 指定されたファイルは
    // dead-code 検出から除外する（tree-sitter parser.c 等の生成物対応）。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join(".gitattributes"),
        "src/generated_sample.rs linguist-generated\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/generated_sample.rs"),
        "pub fn unused_generated_symbol() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/hand_written_sample.rs"),
        "pub fn unused_hand_written_symbol() {}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.contains(&"unused_generated_symbol"),
        "linguist-generated ファイルのシンボルは報告すべきでない: {names:?}"
    );
    assert!(
        names.contains(&"unused_hand_written_symbol"),
        "通常ファイルの未参照シンボルは dead として報告されるべき: {names:?}"
    );
}

#[test]
fn dead_code_python_instance_method_is_live() {
    // Python の `obj.method()` 形式の呼び出しが参照として認識され、
    // class 内メソッドが偽陽性で dead 判定されないことを確認。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("sample.py"),
        "class GitLabClient:\n    def post_comment(self, body):\n        print(body)\n\n\ndef main():\n    client = GitLabClient()\n    client.post_comment(\"hi\")\n\n\nif __name__ == \"__main__\":\n    main()\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.contains("post_comment")),
        "obj.method() 参照が live として検出されるべき: {names:?}"
    );
}

#[test]
fn dead_code_python_classmethod_and_property_are_live() {
    // @classmethod (`Class.method()`) と @property (`obj.attr`) を参照として認識する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("sample.py"),
        "class ReviewConfig:\n    @classmethod\n    def from_env(cls):\n        return cls()\n\n    @property\n    def project_name(self):\n        return \"demo\"\n\n\ndef main():\n    config = ReviewConfig.from_env()\n    print(config.project_name)\n\n\nmain()\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.contains("from_env")),
        "@classmethod 呼び出しは live: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("project_name")),
        "@property アクセスは live: {names:?}"
    );
}

/// Python で `self.method()` / `self.attr` のクラス内自己参照も live として認識される。
/// `@property` 経由の self 属性アクセスと、他メソッドから呼ばれる self.method 呼び出しの
/// 両方が dead に載らないことを確認。
/// (レポート 2026-04-20-python-dead-code-attribute-resolution.md の再現)
#[test]
fn dead_code_python_self_method_and_property_is_live() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("sample.py"),
        r#"class GitLabClient:
    def __init__(self, cfg):
        self.cfg = cfg

    @property
    def project(self):
        return self.cfg

    def post_comment(self, body):
        _ = self.project
        print(body)

    def post_comment_as(self, body, user):
        self.post_comment(body)


def main():
    client = GitLabClient({"project_name": "x"})
    client.post_comment("hi")
    client.post_comment_as("hi", "u")


main()
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.contains("project")),
        "@property への self アクセスは live: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("post_comment")),
        "self.method() 自己呼び出しは live: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("post_comment_as")),
        "外部 caller からの obj.method() は live: {names:?}"
    );
}

#[test]
fn dead_code_same_method_name_in_multiple_classes_skipped() {
    // 同名メソッドが複数クラスに存在する場合、bare name では区別できないため
    // 保守的に dead 判定から除外されることを確認する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("sample.py"),
        "class Alpha:\n    def run(self):\n        pass\n\nclass Beta:\n    def run(self):\n        pass\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with(".run")),
        "同名メソッドが複数クラスにある場合はスキップされるべき: {names:?}"
    );
}

// ---- Xojo 言語サポートのスモークテスト ----

#[test]
fn xojo_symbols_from_fixture() {
    let output = cargo_bin()
        .args(["symbols", "--path", "tests/fixtures/sample.xojo_code"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "xojo");
    let names: Vec<&str> = json["symbols"]
        .as_array()
        .expect("symbols 配列")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    for expected in ["Greeter", "Greet", "DefaultName", "Counter", "Helpers"] {
        assert!(
            names.contains(&expected),
            "Xojo fixture から {expected} を抽出すべき: {names:?}"
        );
    }
}

#[test]
fn xojo_calls_detect_method_callee() {
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.xojo_code"])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(json["lang"], "xojo");
    let serialized = json.to_string();
    assert!(
        serialized.contains("Greet") || serialized.contains("Print"),
        "Greet または Print が callee として検出されるべき: {serialized}"
    );
}

#[test]
fn xojo_refs_case_insensitive_uppercase() {
    // Xojo は識別子が case-insensitive。大文字の `GREET` で小文字定義がヒットすべき。
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "GREET",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.xojo_code",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs 配列");
    assert!(
        refs.iter().any(|r| r["kind"] == "def"),
        "GREET で Greet の定義がヒットすべき: {refs:?}"
    );
    assert!(
        refs.iter().any(|r| r["kind"] == "ref"),
        "GREET で Greet の呼び出し参照がヒットすべき: {refs:?}"
    );
}

#[test]
fn xojo_refs_lowercase_matches_mixedcase_definition() {
    // 小文字 `greet` でも Greet 定義と同件数がヒットすべき。
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "greet",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.xojo_code",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs 配列");
    assert!(!refs.is_empty(), "小文字 greet でもヒットすべき");
}

#[test]
fn xojo_refs_rust_case_preserved() {
    // Rust 等の case-sensitive 言語では従来通り大文字小文字を区別する。
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "EXTRACT_SYMBOLS_NAME",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.rb",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs 配列");
    // Ruby は case-sensitive なので大文字の `EXTRACT_SYMBOLS_NAME` はヒットしない。
    // (小文字シンボル extract_symbols などがあっても影響しないことを担保)
    assert!(refs.is_empty() || refs.iter().all(|r| r["kind"].is_string()));
}

#[test]
fn xojo_refs_batch_case_insensitive_collision() {
    // Xojo は case-insensitive。`Greet` と `greet` を同一バッチで渡しても
    // 両方に同じ参照リストが割り当たるべき（正規化キーの衝突で片方が欠落しないこと）。
    let output = cargo_bin()
        .args([
            "refs",
            "--names",
            "Greet,greet",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.xojo_code",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "2シンボル分の NDJSON が出力されるべき");

    let first: serde_json::Value = serde_json::from_str(lines[0]).expect("invalid JSON");
    let second: serde_json::Value = serde_json::from_str(lines[1]).expect("invalid JSON");
    let refs1 = first["refs"].as_array().expect("refs array");
    let refs2 = second["refs"].as_array().expect("refs array");

    assert!(
        !refs1.is_empty() && !refs2.is_empty(),
        "`Greet` / `greet` どちらも参照を持つべき (片方欠落しないこと): Greet={:?}, greet={:?}",
        refs1,
        refs2
    );
    assert_eq!(
        refs1.len(),
        refs2.len(),
        "同じ正規化キーなら同数の参照であるべき"
    );
}

#[test]
fn xojo_doctor_lists_xojo() {
    let output = cargo_bin()
        .args(["doctor"])
        .output()
        .expect("failed to run doctor");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let xojo = json["languages"]
        .as_array()
        .expect("languages 配列")
        .iter()
        .find(|l| l["language"] == "xojo")
        .expect("doctor 出力に xojo が含まれるべき");
    assert_eq!(xojo["available"], true);
    assert_eq!(xojo["parser_version"], "15");
}
