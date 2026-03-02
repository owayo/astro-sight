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
    assert!(json["languages"].as_array().unwrap().len() >= 14);

    // All languages should be available
    for lang in json["languages"].as_array().unwrap() {
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
    assert_eq!(json["language"], "rust");
    assert!(!json["ast"].as_array().unwrap().is_empty());
    assert!(json["schema"]["range"].as_str().is_some());
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
    assert_eq!(json["language"], "rust");
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
    assert_eq!(json["language"], "rust");

    let symbols = json["symbols"].as_array().unwrap();
    assert!(!symbols.is_empty());

    // Should find the main function
    let main_fn = symbols.iter().find(|s| s["name"] == "main");
    assert!(main_fn.is_some(), "Should find main function");
    assert_eq!(main_fn.unwrap()["kind"], "function");

    // Compact output: has line, no range/hash
    assert!(
        main_fn.unwrap().get("line").is_some(),
        "Compact output should have line field"
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
    assert_eq!(json["language"], "rust");
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
    assert_eq!(json["language"], "rust");

    let calls = json["calls"].as_array().unwrap();
    assert!(!calls.is_empty(), "Should find call edges in main.rs");

    // Should find a call from main to some function
    let main_calls: Vec<_> = calls
        .iter()
        .filter(|c| c["caller"]["name"] == "main")
        .collect();
    assert!(!main_calls.is_empty(), "main should call other functions");
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

    // All callers should be cmd_ast
    for call in calls {
        assert_eq!(call["caller"]["name"], "cmd_ast");
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

    let refs = json["references"].as_array().unwrap();
    assert!(
        refs.len() >= 2,
        "Should find AstgenResponse in multiple files"
    );

    // Should have at least one definition
    let defs: Vec<_> = refs.iter().filter(|r| r["kind"] == "definition").collect();
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
    let refs = json["references"].as_array().unwrap();
    assert!(!refs.is_empty());
}

#[test]
fn context_with_diff() {
    use std::io::Write;
    use std::process::Stdio;

    // Create a synthetic diff
    let diff = r#"--- a/src/engine/symbols.rs
+++ b/src/engine/symbols.rs
@@ -9,7 +9,7 @@
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

    // First: calls response
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert!(first["calls"].is_array());

    // Second: refs response
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert!(second["references"].is_array());
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

// ---- Phase 2: Security tests ----

#[test]
fn sandboxed_service_rejects_path_traversal() {
    // AppService::sandboxed should reject paths outside the workspace boundary
    let cwd = std::env::current_dir().unwrap();
    let cwd = std::fs::canonicalize(cwd).unwrap();
    let service = astro_sight::service::AppService::sandboxed(cwd).unwrap();

    // Try to extract AST for /etc/hosts (outside workspace)
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
    assert!(result.is_err(), "Should reject path outside workspace");

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("outside workspace") || err_msg.contains("PATH_OUT_OF_BOUNDS"),
        "Error should mention workspace boundary: {err_msg}"
    );
}

#[test]
fn sandboxed_service_allows_workspace_paths() {
    // AppService::sandboxed should allow paths within the workspace
    let cwd = std::env::current_dir().unwrap();
    let cwd = std::fs::canonicalize(cwd).unwrap();
    let service = astro_sight::service::AppService::sandboxed(cwd).unwrap();

    // src/lib.rs should be within workspace
    let result = service.extract_symbols("src/lib.rs");
    assert!(result.is_ok(), "Should allow path within workspace");
}

#[test]
fn session_ast_includes_diagnostics() {
    use std::io::Write;
    use std::process::Stdio;

    // Session AST should now include snippet + diagnostics (unified via AppService)
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
    // Session AST should now include snippet (previously missing via AppService unification)
    assert!(
        json.get("snippet").is_some(),
        "Session AST response should include snippet field"
    );
    // hash should also be present
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
@@ -9,7 +9,7 @@
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
    assert_eq!(json["language"], "rust");

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
    assert_eq!(json["language"], "rust");
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
    assert_eq!(json["language"], "rust");
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
    assert_eq!(json["language"], "rust");
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
    assert!(!first["references"].as_array().unwrap().is_empty());

    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["symbol"], "AstgenResponse");
    assert!(!second["references"].as_array().unwrap().is_empty());
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
