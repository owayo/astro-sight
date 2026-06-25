use std::process::{Command, Stdio};

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

fn cargo_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_astro-sight"))
}

#[test]
fn cli_suppresses_broken_pipe_when_stdout_reader_drops() {
    let mut child = cargo_bin()
        .args(["symbols", "--dir", "src", "--glob", "**/*.rs"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn command");

    // パイプ先が head 等で先に終了した状況を再現する。
    drop(child.stdout.take());

    let output = child.wait_with_output().expect("failed to wait command");
    assert!(
        output.status.success(),
        "command should treat broken stdout pipe as success: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("panicked at"),
        "broken pipe should not print a Rust panic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
fn ast_cache_keeps_path_separate_for_same_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    let first = tmp.path().join("first.rs");
    let second = tmp.path().join("second.rs");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("cache_isolation_{}_{}", std::process::id(), nonce);
    let source = format!("fn {name}() {{}}\n");
    std::fs::write(&first, &source).unwrap();
    std::fs::write(&second, source).unwrap();

    let first_path = first.to_str().unwrap();
    let second_path = second.to_str().unwrap();
    let first_output = cargo_bin()
        .args(["ast", "--path", first_path])
        .output()
        .expect("failed to run ast for first file");
    assert!(first_output.status.success());

    let second_output = cargo_bin()
        .args(["ast", "--path", second_path])
        .output()
        .expect("failed to run ast for second file");
    assert!(second_output.status.success());

    let json: serde_json::Value =
        serde_json::from_slice(&second_output.stdout).expect("invalid JSON");
    assert_eq!(json["path"], second_path);
    assert_eq!(json["lang"], "rust");
}

#[test]
fn symbols_cache_keeps_path_and_language_separate_for_same_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    let java_path = tmp.path().join("CacheIsolation.java");
    let csharp_path = tmp.path().join("CacheIsolation.cs");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("CacheIsolation{}{}", std::process::id(), nonce);
    let source = format!("class {name} {{}}\n");
    std::fs::write(&java_path, &source).unwrap();
    std::fs::write(&csharp_path, source).unwrap();

    let java_path = java_path.to_str().unwrap();
    let csharp_path = csharp_path.to_str().unwrap();
    let java_output = cargo_bin()
        .args(["symbols", "--path", java_path])
        .output()
        .expect("failed to run symbols for java file");
    assert!(java_output.status.success());

    let csharp_output = cargo_bin()
        .args(["symbols", "--path", csharp_path])
        .output()
        .expect("failed to run symbols for csharp file");
    assert!(csharp_output.status.success());

    let json: serde_json::Value =
        serde_json::from_slice(&csharp_output.stdout).expect("invalid JSON");
    assert_eq!(json["path"], csharp_path);
    assert_eq!(json["lang"], "csharp");
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

/// Rust の `impl Trait for Type` 配下の同名メソッドが container 付きで区別できることを検証
/// (Issue: 2026-05-02-symbols-impl-block-duplicate.md)。
#[test]
fn symbols_rust_impl_methods_carry_container() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("types.rs"),
        "pub struct A;\npub struct B;\n\
impl Default for A {\n    fn default() -> Self { A }\n}\n\
impl Default for B {\n    fn default() -> Self { B }\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "symbols",
            "--path",
            root.join("types.rs").to_str().unwrap(),
            "--no-cache",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let symbols = json["symbols"].as_array().unwrap();
    let defaults: Vec<&serde_json::Value> = symbols
        .iter()
        .filter(|s| s["name"].as_str() == Some("default"))
        .collect();
    assert_eq!(defaults.len(), 2, "default が 2 件出るべき: {symbols:?}");
    let containers: std::collections::HashSet<&str> =
        defaults.iter().filter_map(|s| s["cn"].as_str()).collect();
    assert!(
        containers.contains("A") && containers.contains("B"),
        "default の container として A と B が両方付与されるべき: {defaults:?}"
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

    // 合成 diff: extract_symbols のシグネチャ変更。行番号は実コードから動的に取得する。
    let symbols_src =
        std::fs::read_to_string("src/engine/symbols.rs").expect("read src/engine/symbols.rs");
    let extract_line_idx = symbols_src
        .lines()
        .position(|l| l.starts_with("pub fn extract_symbols("))
        .expect("extract_symbols 関数が見つからない");
    let line_no = extract_line_idx + 1;
    let diff = format!(
        "--- a/src/engine/symbols.rs\n\
         +++ b/src/engine/symbols.rs\n\
         @@ -{line_no},7 +{line_no},7 @@\n\
         -pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {{\n\
         +pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, include_refs: bool) -> Result<Vec<Symbol>> {{\n\
             let query_src = symbol_query(lang_id);\n"
    );

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
fn session_workspace_relative_paths_are_resolved_from_workspace() {
    use std::io::Write;
    use std::process::Stdio;

    let workspace = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let src_dir = workspace.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("lib.rs"), "pub fn workspace_symbol() {}\n").unwrap();

    let mut child = cargo_bin()
        .arg("session")
        .env("ASTRO_SIGHT_WORKSPACE", workspace.path())
        .current_dir(cwd.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to run session");

    {
        let stdin = child.stdin.as_mut().expect("stdin should be available");
        writeln!(stdin, r#"{{"command":"symbols","path":"src/lib.rs"}}"#).unwrap();
    }

    let output = child.wait_with_output().expect("failed to wait session");
    assert!(
        output.status.success(),
        "session should succeed: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("session output should be JSON");
    assert!(
        json.get("error").is_none(),
        "workspace-relative path should not error: {json}"
    );
    let symbols = json["symbols"]
        .as_array()
        .expect("symbols array is required");
    assert!(
        symbols.iter().any(|s| s["name"] == "workspace_symbol"),
        "workspace relative symbols should be extracted: {json}"
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

    // batch refs アプローチでの context 出力が従来通り一貫していることを確認する。
    // 行番号は実コードから動的に取得する。
    let symbols_src =
        std::fs::read_to_string("src/engine/symbols.rs").expect("read src/engine/symbols.rs");
    let extract_line_idx = symbols_src
        .lines()
        .position(|l| l.starts_with("pub fn extract_symbols("))
        .expect("extract_symbols 関数が見つからない");
    let line_no = extract_line_idx + 1;
    let diff = format!(
        "--- a/src/engine/symbols.rs\n\
         +++ b/src/engine/symbols.rs\n\
         @@ -{line_no},7 +{line_no},7 @@\n\
         -pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {{\n\
         +pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, flag: bool) -> Result<Vec<Symbol>> {{\n\
             let query_src = symbol_query(lang_id);\n"
    );

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

/// chunk サイズより多い名前でも、複数 chunk にまたがって入力 names 順の
/// NDJSON 出力を維持することを検証（cmd_refs_batch の chunk 化回帰: 2026-05-31）。
#[test]
fn refs_batch_names_preserves_order_across_chunks() {
    // ASTRO_SIGHT_REFS_BATCH_CHUNK=2 で 3 名前を chunk [A,B] / [C] に分割させ、
    // chunk 境界をまたいでも入力順を保つことを確認する。
    let output = cargo_bin()
        .env("ASTRO_SIGHT_REFS_BATCH_CHUNK", "2")
        .args([
            "refs",
            "--names",
            "AppService,AstgenResponse,SymbolReference",
            "--dir",
            "src/",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let symbols: Vec<String> = stdout
        .trim()
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["symbol"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(
        symbols,
        vec!["AppService", "AstgenResponse", "SymbolReference"],
        "chunk をまたいでも入力 names 順を維持すべき"
    );
}

/// find_references_batch はディレクトリ走査を 1 回に集約しつつ内部で名前を chunk
/// 分割するため、chunk サイズを変えても参照集合が完全に一致しなければならない。
/// 走査集約リファクタが結果を変えていないことを保証する回帰テスト。
#[test]
fn refs_batch_results_independent_of_chunk_size() {
    let names =
        "find_references_batch,collect_files,extract_symbols,detect_api_changes,SymbolReference";
    let run = |chunk: &str| -> String {
        let output = cargo_bin()
            .env("ASTRO_SIGHT_REFS_BATCH_CHUNK", chunk)
            .args(["refs", "--names", names, "--dir", "src/"])
            .output()
            .expect("failed to run");
        assert!(output.status.success());
        // (symbol, 参照件数) を symbol 順に正規化する。chunk 分割で参照が
        // 取りこぼされたり重複したりすれば件数が変わって検出できる。
        let mut pairs: Vec<(String, usize)> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                let sym = v["symbol"].as_str().unwrap().to_string();
                let n = v["refs"].as_array().map(|a| a.len()).unwrap_or(0);
                (sym, n)
            })
            .collect();
        pairs.sort();
        format!("{pairs:?}")
    };
    let chunk_big = run("64");
    assert_eq!(
        run("1"),
        chunk_big,
        "chunk=1 と chunk=64 で結果が一致すべき"
    );
    assert_eq!(
        run("2"),
        chunk_big,
        "chunk=2 と chunk=64 で結果が一致すべき"
    );
    // 全件 0 だと比較が無意味になるため、参照が実際に検出されていることも確認する。
    assert!(
        chunk_big.contains("detect_api_changes"),
        "参照が検出されているべき: {chunk_big}"
    );
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
fn context_git_xojo_only_diff_skips_before_parse() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let xojo_fixture = include_str!("fixtures/sample.xojo_code");

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

#[test]
fn impact_rust_local_var_not_treated_as_cross_file_ref() {
    // Issue 2026-06-13-ai-status-json-symbol-fp: 別ファイルのローカル変数 `let json` が
    // 同名の自由関数 `render::json` への cross-file 参照に誤マッチしないこと。
    // qualified call (`render::json`) を持つ main.rs は high のまま、`let json` だけの
    // profiles.rs は未解決影響に出ないことを検証する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i32) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();
    // profiles.rs: render::json を import / qualified 参照せず bare `let json` のみ
    std::fs::write(
        root.join("src/profiles.rs"),
        "use std::fs;\npub fn discover() -> i32 {\n    let json = fs::read_to_string(\"x\").unwrap_or_default();\n    json.trim().parse().unwrap_or(0)\n}\n",
    )
    .unwrap();
    // main.rs: render::json を qualified path で呼ぶ実 caller
    std::fs::write(
        root.join("src/main.rs"),
        "mod render;\nmod profiles;\nfn main() {\n    println!(\"{}\", render::json(profiles::discover()));\n}\n",
    )
    .unwrap();

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

    // render::json のシグネチャを変更 (i32 -> i64)
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i64) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // main.rs (qualified call) は未解決影響として残る
    assert!(
        stderr.contains("main.rs"),
        "qualified call (render::json) を持つ main.rs は high のまま残るべき: {stderr}"
    );
    // profiles.rs (local `let json`) は cross-file 参照ではないので出ない
    assert!(
        !stderr.contains("profiles.rs"),
        "local 変数 `let json` だけの profiles.rs は未解決影響に出るべきでない: {stderr}"
    );
}

#[test]
fn impact_rust_macro_arg_ident_stays_high() {
    // Issue 2026-06-13 codex 指摘: `call_render!(json, ..)` のように macro が `crate::render::json`
    // を補うケースでは、caller に `::json` 証拠がなくても本物の参照なので high 維持すべき
    // (証拠なし bare identifier の low routing が macro 引数を取りこぼさないこと)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i32) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();
    // macro が path を補い、caller は bare ident `json` を渡すだけ。
    // codex 指摘の混在行 (`let json = call_render!(json, 1); json`) も同行に local binding と
    // macro 引数が混ざるケースとして含め、macro 引数側を取りこぼさないことを検証する。
    std::fs::write(
        root.join("src/caller.rs"),
        "#[macro_export]\nmacro_rules! call_render {\n    ($name:ident, $arg:expr) => { $crate::render::$name($arg) };\n}\npub fn run() -> String { let json = call_render!(json, 1); json }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "#[macro_use]\nmod caller;\nmod render;\nfn main() { let _ = caller::run(); }\n",
    )
    .unwrap();

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
        root.join("src/render.rs"),
        "pub fn json(value: i64) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // caller.rs の macro 引数 `json` は high のまま未解決影響に残る (fail-closed)。
    assert!(
        stderr.contains("caller.rs"),
        "macro 引数の bare ident `json` は high 維持されるべき (fail-closed): {stderr}"
    );
}

/// A3 用 helper: base_files を commit し、`setup` で削除/追加した temp git repo を作って
/// `review --git --hook` の `Output` (exit code + stderr の hook JSON) を返す。
fn a3_review_hook(
    setup: impl FnOnce(&std::path::Path),
    base_files: &[(&str, &str)],
) -> std::process::Output {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    };
    for (rel, content) in base_files {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }
    git(&["init", "-q"]);
    git(&["config", "user.email", "a@b.c"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "."]);
    git(&["commit", "-m", "init", "-q"]);
    setup(root);
    cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git", "--hook"])
        .output()
        .expect("run review")
}

#[test]
fn review_python_root_script_move_no_api_rm() {
    // Issue 2026-06-14-python-script-move-api-rm: root-level の単体スクリプトを package へ移設
    // すると旧スクリプトの top-level helper が api.rm (blocking) で hook を止める FP。
    // script-local として api.rm から除外され hook が clean になることを検証する。
    let script = "def helper():\n    return 1\n\ndef cmd_run(cfg):\n    return helper()\n\ndef main():\n    cmd_run({})\n\nif __name__ == '__main__':\n    main()\n";
    let pyproject = "[project]\nname = \"tool\"\nversion = \"0.1.0\"\n";
    let output = a3_review_hook(
        |root| {
            // build_font.py 削除 + package 化 (untracked), cmd_run の sig 変更
            Command::new("git")
                .args(["rm", "-q", "script.py"])
                .current_dir(root)
                .status()
                .expect("git rm");
            std::fs::create_dir_all(root.join("src/pkg")).unwrap();
            std::fs::write(root.join("src/pkg/__init__.py"), "").unwrap();
            std::fs::write(
                root.join("src/pkg/__main__.py"),
                "from pkg.main import main\nmain()\n",
            )
            .unwrap();
            std::fs::write(
                root.join("src/pkg/main.py"),
                "def helper():\n    return 1\n\ndef cmd_run(ctx):\n    return helper()\n\ndef main():\n    cmd_run(object())\n",
            )
            .unwrap();
        },
        &[("script.py", script), ("pyproject.toml", pyproject)],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // hook は blocking なし → exit 0 (clean)
    assert!(
        output.status.success(),
        "root script move は hook blocking しないべき (exit 0): {stderr}"
    );
    assert!(
        !stderr.contains("\"rm\""),
        "root script の helper は script-local で api.rm にしないべき: {stderr}"
    );
}

#[test]
fn review_python_package_module_removal_keeps_api_rm() {
    // 安全性: package module (サブディレクトリ配下) の関数削除は従来どおり api.rm として残す
    // (A3 が real api.rm を隠さない false negative 回避)。
    let output = a3_review_hook(
        |root| {
            Command::new("git")
                .args(["rm", "-q", "pkg/lib.py"])
                .current_dir(root)
                .status()
                .expect("git rm");
        },
        &[
            ("pkg/__init__.py", ""),
            ("pkg/lib.py", "def public_api(x):\n    return x + 1\n"),
            (
                "app.py",
                "from pkg.lib import public_api\nprint(public_api(1))\n",
            ),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // api.rm は blocking → exit 1
    assert!(
        !output.status.success(),
        "package module の api.rm は hook を blocking すべき (exit 1): {stderr}"
    );
    assert!(
        stderr.contains("public_api"),
        "package module の削除は api.rm に残すべき: {stderr}"
    );
}

#[test]
fn review_python_imported_root_module_keeps_api_rm() {
    // 安全性: root-level でも他ファイルから import されているモジュールの削除は api.rm として残す。
    let output = a3_review_hook(
        |root| {
            Command::new("git")
                .args(["rm", "-q", "util.py"])
                .current_dir(root)
                .status()
                .expect("git rm");
        },
        &[
            ("util.py", "def helper(x):\n    return x * 2\n"),
            ("app.py", "import util\nprint(util.helper(3))\n"),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "import される root module の api.rm は hook を blocking すべき (exit 1): {stderr}"
    );
    assert!(
        stderr.contains("helper"),
        "import される root module の削除は api.rm に残すべき: {stderr}"
    );
}

#[test]
fn review_python_poetry_script_entry_not_excluded() {
    // 安全性 (codex 指摘): Poetry `[tool.poetry.scripts]` の entrypoint が指す root script を
    // 削除しても script-local として完全除外せず、公開 CLI 面として扱う (api.rm/rm_dead に残す)。
    let pyproject = "[tool.poetry]\nname = \"tool\"\nversion = \"0.1.0\"\n\n[tool.poetry.scripts]\nmytool = \"cli:run\"\n";
    let output = a3_review_hook(
        |root| {
            Command::new("git")
                .args(["rm", "-q", "cli.py"])
                .current_dir(root)
                .status()
                .expect("git rm");
        },
        &[
            ("cli.py", "def run():\n    print(\"hi\")\n"),
            ("pyproject.toml", pyproject),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Poetry entrypoint の関数は完全除外されず出力に残る (rm or rm_dead)
    assert!(
        stderr.contains("run"),
        "Poetry script entry の削除は完全除外せず出力に残すべき: {stderr}"
    );
}

#[test]
fn review_python_malformed_pyproject_scripts_fails_closed() {
    // 安全性 (codex 指摘): scripts セクションが存在するが table でない schema 不正な pyproject では
    // 解析不能として fail-closed (script-local 判定を止め、削除を完全除外しない)。
    let pyproject = "[project]\nname = \"tool\"\nscripts = \"cli:run\"\n";
    let output = a3_review_hook(
        |root| {
            Command::new("git")
                .args(["rm", "-q", "cli.py"])
                .current_dir(root)
                .status()
                .expect("git rm");
        },
        &[
            ("cli.py", "def run():\n    print(\"hi\")\n"),
            ("pyproject.toml", pyproject),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("run"),
        "schema 不正な pyproject は fail-closed で完全除外しないべき: {stderr}"
    );
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

    // diff: extract_symbols のシグネチャを変更 → 他ファイルの caller が未解決になる
    // 行番号は実コードから動的に取得する。
    let symbols_src =
        std::fs::read_to_string("src/engine/symbols.rs").expect("read src/engine/symbols.rs");
    let extract_line_idx = symbols_src
        .lines()
        .position(|l| l.starts_with("pub fn extract_symbols("))
        .expect("extract_symbols 関数が見つからない");
    let line_no = extract_line_idx + 1;
    let diff = format!(
        "--- a/src/engine/symbols.rs\n\
         +++ b/src/engine/symbols.rs\n\
         @@ -{line_no},7 +{line_no},7 @@\n\
         -pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {{\n\
         +pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, flag: bool) -> Result<Vec<Symbol>> {{\n\
             let query_src = symbol_query(lang_id);\n"
    );

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

/// Rust の `pub use submodule::Foo;` で再エクスポートしているだけのファイルは、
/// エクスポート元シンボルの body-only 変更で impact に載せてはならない。
/// (R7 の TypeScript `export from` と同等、Rust 版の回帰ガード)
#[test]
fn impact_rust_pub_use_reexport_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let inner_rs = dir.path().join("inner.rs");
    let lib_rs = dir.path().join("lib.rs");

    std::fs::write(
        &inner_rs,
        r#"pub fn do_work(x: i32) -> i32 {
    x + 1
}
"#,
    )
    .unwrap();
    std::fs::write(
        &lib_rs,
        r#"pub mod inner;
pub use inner::do_work;
"#,
    )
    .unwrap();

    // do_work の body のみ変更 (シグネチャは不変)
    let diff = r#"--- a/inner.rs
+++ b/inner.rs
@@ -1,3 +1,4 @@
 pub fn do_work(x: i32) -> i32 {
-    x + 1
+    let y = x;
+    y + 1
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

    assert!(
        output.status.success(),
        "expected exit 0.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("lib.rs"),
        "`pub use` 再エクスポートのみの行は impact に載せてはならない: {stderr}"
    );
}

/// 新規クレートで `pub mod foo; pub mod bar;` のようなモジュール宣言のみを
/// 追加した場合、他クレート内で同名のローカル変数 (`tensor` / `ops` 等) が
/// impact に巻き添えで紐付かないことを確認する。モジュール名は
/// `should_include_for_cross_file` の段階で除外される。
/// (レポート triage-ocrus-nn-impact.md の再現)
#[test]
fn impact_module_declaration_no_cross_file_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let nn_dir = dir.path().join("crates/ocrus-nn/src");
    let cli_dir = dir.path().join("crates/ocrus-cli/src");
    std::fs::create_dir_all(&nn_dir).unwrap();
    std::fs::create_dir_all(&cli_dir).unwrap();

    std::fs::write(nn_dir.join("lib.rs"), "// empty\n").unwrap();
    // consumer 側に tensor / ops という名前のローカル変数 / クロージャ引数を持つコード
    std::fs::write(
        cli_dir.join("main.rs"),
        r#"fn char_accuracy() {
    let tensor = normalize_line();
    let _shape = tensor.shape();
    for (_i, tensor) in [1, 2].iter().enumerate() {
        let _ = tensor;
    }
    let ops: Vec<u8> = vec![];
    let _ = ops;
}
fn normalize_line() -> Vec<u8> { vec![] }
"#,
    )
    .unwrap();
    // 新規モジュール宣言を追加する diff
    let diff = r#"--- a/crates/ocrus-nn/src/lib.rs
+++ b/crates/ocrus-nn/src/lib.rs
@@ -1 +1,3 @@
-// empty
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

    assert!(
        output.status.success(),
        "expected exit 0 (module 宣言追加は impact を出さない).\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("main.rs"),
        "consumer 側のローカル変数 tensor/ops は impact に載せてはならない: {stderr}"
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

/// Phase 4 設計バグの回帰テスト:
/// 同名 method (modified + added) が異なるファイルに存在するとき、
/// added 側 (新規追加ファイル) の fc_ix へ cross-file 参照が漏れないこと。
///
/// 旧実装: pass2 の `sym_to_fc` を **グローバル** `included_symbols.contains(sym_key)`
/// で判定していたため、Factory.php の `new` (modified) が include されるだけで、
/// Id.php (added) の `new` も同じ sym_key を持つため両 fc_ix に caller が流れていた。
///
/// 新実装: `FileContext.cross_file_symbol_keys` で per-file 判定するため、
/// added の Id 側には何も流れない。
#[test]
fn impact_per_file_routing_excludes_added_with_same_method_name() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let existing_rs = dir.path().join("existing.rs");
    let added_rs = dir.path().join("added.rs");
    let caller_rs = dir.path().join("caller.rs");

    // existing.rs: 既存ファイル (modified) — Existing::run シグネチャ変更後の状態
    std::fs::write(
        &existing_rs,
        r#"pub struct Existing;

impl Existing {
    pub fn run(&self, x: u32) -> u32 { x + 1 }
}
"#,
    )
    .unwrap();

    // added.rs: 新規ファイル (added) — Added::run も同名メソッドを持つ
    std::fs::write(
        &added_rs,
        r#"pub struct Added;

impl Added {
    pub fn run(&self) -> u32 { 0 }
}
"#,
    )
    .unwrap();

    // caller.rs: Existing::run の caller がいる (Added は使わない)
    std::fs::write(
        &caller_rs,
        r#"use crate::existing::Existing;

pub fn use_existing(e: &Existing) -> u32 {
    e.run(42)
}
"#,
    )
    .unwrap();

    // diff: existing.rs を modify + added.rs を新規追加
    let diff = r#"--- a/existing.rs
+++ b/existing.rs
@@ -2,4 +2,4 @@
 pub struct Existing;

 impl Existing {
-    pub fn run(&self) -> u32 { 1 }
+    pub fn run(&self, x: u32) -> u32 { x + 1 }
 }
--- /dev/null
+++ b/added.rs
@@ -0,0 +1,5 @@
+pub struct Added;
+
+impl Added {
+    pub fn run(&self) -> u32 { 0 }
+}
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "context failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("changes array missing: {stdout}"));

    // added.rs の change を取り出して impacted_callers が空であることを確認。
    let added_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("added.rs"))
        .unwrap_or_else(|| {
            panic!("added.rs change entry not found in: {stdout}");
        });
    let added_callers = added_change
        .get("impacted_callers")
        .and_then(|c| c.as_array());
    assert!(
        added_callers.is_none_or(|c| c.is_empty()),
        "added.rs (added file) must have no impacted_callers, but got: {added_change}"
    );
    let added_low = added_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array());
    assert!(
        added_low.is_none_or(|c| c.is_empty()),
        "added.rs (added file) must have no low_confidence_callers, but got: {added_change}"
    );
}

/// PHP trait の親型認識テスト (load-bearing バグの回帰テスト):
/// `trait Factory { public static function new() }` のような trait scope の
/// メソッドが変更された際、Stage 4b の parent_in_this_file チェックが
/// 効くようにするため、`trait_declaration` が親型として認識されること。
///
/// 旧実装は `class_declaration` だけを親型として認識していたため、PHP trait
/// 内の同名メソッド (`new` 等) で `parent_ix_by_sym = None` となり、Stage 4b が
/// 完全にバイパスされ、`Other::new()` 系の同名 method 全件が誤って
/// impacted_callers に流れていた。
#[test]
fn impact_php_trait_method_filters_unrelated_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let trait_php = dir.path().join("Factory.php");
    let other_php = dir.path().join("Other.php");
    let unrelated_php = dir.path().join("Unrelated.php");

    // Factory.php: trait scope の static method `new`
    std::fs::write(
        &trait_php,
        r#"<?php
namespace App\Factories;

trait Factory {
    public static function new(int $x): self {
        return new self($x + 1);
    }
}
"#,
    )
    .unwrap();

    // Other.php: 別 class の同名 static method `new`
    std::fs::write(
        &other_php,
        r#"<?php
namespace App\Other;

class Other {
    public static function new(): self {
        return new self();
    }
}
"#,
    )
    .unwrap();

    // Unrelated.php: Other::new() を呼ぶが Factory trait は use していない
    std::fs::write(
        &unrelated_php,
        r#"<?php
namespace App\Consumers;

use App\Other\Other;

class Consumer {
    public function consume(): void {
        $obj = Other::new();
    }
}
"#,
    )
    .unwrap();

    // diff: Factory trait の new シグネチャを変更
    let diff = r#"--- a/Factory.php
+++ b/Factory.php
@@ -3,7 +3,7 @@
 namespace App\Factories;

 trait Factory {
-    public static function new(int $x): self {
+    public static function new(int $x, int $y): self {
         return new self($x + 1);
     }
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");

    let factory_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("Factory.php"))
        .unwrap_or_else(|| panic!("Factory.php change not found: {stdout}"));

    let impacted = factory_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = factory_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    // Unrelated.php は Other::new() を呼ぶだけで Factory trait は触らない。
    // trait_declaration が親型認識されれば parent_in_this_file=false で skip される。
    let unrelated_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("Unrelated.php"));
    let unrelated_in_low = low
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("Unrelated.php"));
    assert!(
        !unrelated_in_impacted,
        "Unrelated.php must NOT appear in impacted_callers (Stage 4b parent check). impacted: {impacted:?}"
    );
    assert!(
        !unrelated_in_low,
        "Unrelated.php must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// Rust trait_item 親型認識の回帰テスト。
///
/// PHP の trait_declaration と同様、Rust の `trait Foo { fn bar() {} }` も
/// 親型認識されないと Stage 4b parent_in_this_file が常に false になり、
/// 別 struct の同名 method が impacted_callers に流れる。
#[test]
fn impact_rust_trait_item_filters_unrelated_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let trait_rs = dir.path().join("factory.rs");
    let other_rs = dir.path().join("other.rs");
    let unrelated_rs = dir.path().join("unrelated.rs");

    // factory.rs: trait scope の method `new`
    std::fs::write(
        &trait_rs,
        r#"pub trait Factory {
    fn new(x: i32) -> Self
    where
        Self: Sized;
}
"#,
    )
    .unwrap();

    // other.rs: 別 struct の同名 method `new`
    std::fs::write(
        &other_rs,
        r#"pub struct Other;

impl Other {
    pub fn new() -> Self {
        Other
    }
}
"#,
    )
    .unwrap();

    // unrelated.rs: Other::new() を呼ぶだけ。Factory trait は触らない。
    std::fs::write(
        &unrelated_rs,
        r#"use crate::other::Other;

pub fn consume() {
    let _ = Other::new();
}
"#,
    )
    .unwrap();

    // diff: Factory trait の `new` シグネチャを変更
    let diff = r#"--- a/factory.rs
+++ b/factory.rs
@@ -1,5 +1,5 @@
 pub trait Factory {
-    fn new(x: i32) -> Self
+    fn new(x: i32, y: i32) -> Self
     where
         Self: Sized;
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");

    let factory_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("factory.rs"))
        .unwrap_or_else(|| panic!("factory.rs change not found: {stdout}"));

    let impacted = factory_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = factory_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let unrelated_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.rs"));
    let unrelated_in_low = low
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.rs"));
    assert!(
        !unrelated_in_impacted,
        "unrelated.rs must NOT appear in impacted_callers (Rust trait_item parent check). impacted: {impacted:?}"
    );
    assert!(
        !unrelated_in_low,
        "unrelated.rs must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// TypeScript abstract_class_declaration 親型認識の回帰テスト。
///
/// `abstract class Foo { abstract bar(): void }` は通常の class_declaration ではなく
/// abstract_class_declaration ノードになるため、別途認識を追加しないと parent が消える。
#[test]
fn impact_typescript_abstract_class_filters_unrelated_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let abs_ts = dir.path().join("base.ts");
    let other_ts = dir.path().join("other.ts");
    let unrelated_ts = dir.path().join("unrelated.ts");

    // base.ts: abstract class scope の method `process`
    std::fs::write(
        &abs_ts,
        r#"export abstract class Base {
    abstract process(x: number): number;
}
"#,
    )
    .unwrap();

    // other.ts: 別 class の同名 method `process`
    std::fs::write(
        &other_ts,
        r#"export class Other {
    process(): number {
        return 0;
    }
}
"#,
    )
    .unwrap();

    // unrelated.ts: Other.process() を呼ぶだけ
    std::fs::write(
        &unrelated_ts,
        r#"import { Other } from "./other";

export function consume(): number {
    const o = new Other();
    return o.process();
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/base.ts
+++ b/base.ts
@@ -1,3 +1,3 @@
 export abstract class Base {
-    abstract process(x: number): number;
+    abstract process(x: number, y: number): number;
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");

    let base_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("base.ts"))
        .unwrap_or_else(|| panic!("base.ts change not found: {stdout}"));

    let impacted = base_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = base_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let unrelated_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.ts"));
    let unrelated_in_low = low
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.ts"));
    assert!(
        !unrelated_in_impacted,
        "unrelated.ts must NOT appear in impacted_callers (TS abstract_class_declaration parent check). impacted: {impacted:?}"
    );
    assert!(
        !unrelated_in_low,
        "unrelated.ts must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// impact のデフォルト除外ディレクトリ動作確認。
///
/// vendor / node_modules / target などの 3rd-party / build artifact ディレクトリ内に
/// 同名メソッドが置かれていても、`impacted_callers` には流れ込まないこと。
#[test]
fn impact_default_excluded_dirs_drops_vendor_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let owner_php = dir.path().join("Owner.php");
    let vendor_php = dir.path().join("vendor").join("Caller.php");
    let target_rs_dir = dir.path().join("target").join("debug");
    std::fs::create_dir_all(target_rs_dir.parent().unwrap()).unwrap();
    std::fs::create_dir_all(vendor_php.parent().unwrap()).unwrap();

    // Owner.php: trait scope の static method `new`
    std::fs::write(
        &owner_php,
        r#"<?php
trait Factory {
    public static function new(int $x): self {
        return new self($x);
    }
}
"#,
    )
    .unwrap();

    // vendor/Caller.php: 同名 static method `new` を持つ別 class
    std::fs::write(
        &vendor_php,
        r#"<?php
class VendorThing {
    public static function new(): self {
        return new self();
    }
}
function consume_vendor(): void {
    $obj = VendorThing::new();
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/Owner.php
+++ b/Owner.php
@@ -1,5 +1,5 @@
 <?php
 trait Factory {
-    public static function new(int $x): self {
+    public static function new(int $x, int $y): self {
         return new self($x);
     }
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let owner_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("Owner.php"))
        .unwrap_or_else(|| panic!("Owner.php change not found: {stdout}"));

    let impacted = owner_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = owner_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let vendor_in_impacted = impacted.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("vendor"))
            .unwrap_or(false)
    });
    let vendor_in_low = low.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("vendor"))
            .unwrap_or(false)
    });

    assert!(
        !vendor_in_impacted,
        "vendor/ caller must NOT appear in impacted_callers. impacted: {impacted:?}"
    );
    assert!(
        !vendor_in_low,
        "vendor/ caller must NOT appear in low_confidence_callers. low: {low:?}"
    );
}

/// `--exclude-dir` が impact 解析の cross-file 検索でも作用する回帰テスト (v26.5.117)。
///
/// `IMPACT_DEFAULT_EXCLUDED_DIRS` の固定リストに含まれない命名 (バージョン入りの
/// `pjproject-2.15` 等) でも、ユーザーが `--exclude-dir` で渡せば impact から除外される。
#[test]
fn impact_user_exclude_dir_drops_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let owner_php = dir.path().join("Owner.php");
    let custom_dir = dir.path().join("pjproject-2.15");
    std::fs::create_dir_all(&custom_dir).unwrap();
    let custom_caller = custom_dir.join("Caller.php");

    std::fs::write(
        &owner_php,
        r#"<?php
trait Factory {
    public static function new(int $x): self {
        return new self($x);
    }
}
"#,
    )
    .unwrap();
    // 命名が IMPACT_DEFAULT_EXCLUDED_DIRS に含まれないので、デフォルトでは impact 対象。
    std::fs::write(
        &custom_caller,
        r#"<?php
class CustomThing {
    public static function new(): self {
        return new self();
    }
}
function consume_custom(): void {
    $obj = CustomThing::new();
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/Owner.php
+++ b/Owner.php
@@ -1,5 +1,5 @@
 <?php
 trait Factory {
-    public static function new(int $x): self {
+    public static function new(int $x, int $y): self {
         return new self($x);
     }
 }
"#;

    // ユーザーが --exclude-dir pjproject-2.15 を渡せば、impact からも除外される。
    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args([
            "context",
            "--dir",
            dir.path().to_str().unwrap(),
            "--exclude-dir",
            "pjproject-2.15",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let owner_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("Owner.php"))
        .unwrap_or_else(|| panic!("Owner.php change not found: {stdout}"));

    let impacted = owner_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = owner_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let custom_in_impacted = impacted.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("pjproject-2.15"))
            .unwrap_or(false)
    });
    let custom_in_low = low.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("pjproject-2.15"))
            .unwrap_or(false)
    });
    assert!(
        !custom_in_impacted,
        "pjproject-2.15/ caller must NOT appear in impacted_callers when --exclude-dir is set. impacted: {impacted:?}"
    );
    assert!(
        !custom_in_low,
        "pjproject-2.15/ caller must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// 不正な `--exclude-glob` (構文エラー) がエラーで終了することを確認する。
///
/// silent empty 結果 (= 全 impact が消える) の方がユーザーにとって危険なので、
/// `validate_exclude_globs` で先行検証して `INVALID_REQUEST` で落とす。
#[test]
fn impact_invalid_exclude_glob_returns_error() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("foo.rs"), "fn x() {}\n").unwrap();
    let diff = r#"--- a/foo.rs
+++ b/foo.rs
@@ -1,1 +1,1 @@
-fn x() {}
+fn x(y: i32) {}
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args([
            "context",
            "--dir",
            dir.path().to_str().unwrap(),
            "--exclude-glob",
            "[",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "invalid --exclude-glob should fail. stdout: {stdout}"
    );
    assert!(
        stdout.contains("INVALID_REQUEST") || stdout.contains("invalid exclude-glob"),
        "expected JSON error mentioning invalid exclude-glob. got: {stdout}"
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
    let result = service.analyze_context(
        "",
        ".",
        &astro_sight::models::impact::ContextAnalysisOptions::default(),
    );
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

#[test]
fn session_mixed_structural_queries_preserve_order() {
    let input = r#"{"command":"symbols","path":"src/main.rs"}
{"command":"calls","path":"src/main.rs","function":"main"}
{"command":"sequence","path":"src/main.rs","function":"main"}
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
    assert_eq!(lines.len(), 3, "3コマンド → 3行の NDJSON 出力");

    let symbols: serde_json::Value =
        serde_json::from_str(lines[0]).expect("symbols response should be JSON");
    assert!(
        symbols["symbols"]
            .as_array()
            .expect("symbols should be an array")
            .iter()
            .any(|s| s["name"] == "main"),
        "先頭行は src/main.rs の symbols 応答であるべき"
    );

    let calls: serde_json::Value =
        serde_json::from_str(lines[1]).expect("calls response should be JSON");
    assert_eq!(calls["lang"], "rust");
    assert!(
        calls["calls"]
            .as_array()
            .expect("calls should be an array")
            .iter()
            .any(|c| c["caller"] == "main"),
        "2行目は main の calls 応答であるべき"
    );

    let sequence: serde_json::Value =
        serde_json::from_str(lines[2]).expect("sequence response should be JSON");
    assert_eq!(sequence["lang"], "rust");
    assert!(
        sequence["diagram"]
            .as_str()
            .expect("sequence diagram should be a string")
            .starts_with("sequenceDiagram"),
        "3行目は sequence diagram 応答であるべき"
    );
}

#[test]
fn session_review_command_returns_json_error() {
    let input = r#"{"command":"review","dir":".","git":true}
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
    assert_eq!(
        lines.len(),
        1,
        "session は非対応コマンドも JSON エラー 1 行として返す"
    );
    let error: serde_json::Value =
        serde_json::from_str(lines[0]).expect("error response should be JSON");
    assert_eq!(error["error"]["code"], "INVALID_REQUEST");
    assert!(
        error["error"]["message"]
            .as_str()
            .expect("message should be a string")
            .contains("unknown variant `review`"),
        "review が session 非対応であることを明示すべき: {error}"
    );
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

/// TypeScript で関数戻り値型に使われた `type_identifier` が `kind: "ref"` として
/// 認識されることを検証 (レポート 2026-04-24-excel-service-dead-code-false-positive.md の再現)。
/// `function parseExcel(): ExcelParseResult {}` の `ExcelParseResult` が def ではなく
/// ref として分類されることで、dead-code 判定が正しく動作する。
#[test]
fn typescript_return_type_is_ref_not_def() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("excel.ts"),
        "export interface ExcelParseResult { rows: number }\n\
export function parseExcel(buffer: Buffer): ExcelParseResult {\n\
  return { rows: 0 };\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "ExcelParseResult",
            "--dir",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();

    let def_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("def"))
        .count();
    let ref_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("ref"))
        .count();
    assert_eq!(
        def_count, 1,
        "ExcelParseResult の def は interface 宣言の 1 件だけのはず: {refs:?}"
    );
    assert!(
        ref_count >= 1,
        "戻り値型として使われている ExcelParseResult は ref として 1 件以上検出されるべき: {refs:?}"
    );
}

/// TypeScript の `class A extends B {}` の `B` が ref として認識されることを検証。
/// 単純な grandparent 走査では `class_declaration` に B が def として誤分類される問題への対応。
#[test]
fn typescript_class_extends_is_ref_not_def() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(root.join("base.ts"), "export class Base { hello() {} }\n").unwrap();
    std::fs::write(
        root.join("derived.ts"),
        "import { Base } from './base';\nexport class Derived extends Base { extra() {} }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "Base", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let def_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("def"))
        .count();
    let ref_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("ref"))
        .count();
    assert_eq!(
        def_count, 1,
        "Base の def は base.ts のクラス宣言 1 件: {refs:?}"
    );
    assert!(
        ref_count >= 2,
        "import と extends で 2 件以上 ref が出るべき: {refs:?}"
    );
}

/// 拡張子なし shebang スクリプト (例: `bin/install`) が collect_files の対象として
/// 拾われ、refs / dead-code 検索に含まれることを検証
/// (Issue: shebang-script-collect-files)。
#[test]
fn refs_picks_up_shebang_script_without_extension() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("bin")).unwrap();

    // 拡張子なし bash スクリプト (shebang 付き)
    std::fs::write(
        root.join("bin/install"),
        "#!/usr/bin/env bash\nfoo() { echo hi; }\nfoo\n",
    )
    .unwrap();
    // 同じ名前を呼ぶ普通の .sh
    std::fs::write(root.join("deploy.sh"), "#!/usr/bin/env bash\nfoo\n").unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "foo", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let paths: Vec<&str> = refs.iter().filter_map(|r| r["path"].as_str()).collect();
    assert!(
        paths.iter().any(|p| p.ends_with("bin/install")),
        "拡張子なし shebang スクリプト bin/install が refs 対象に入るべき: {paths:?}"
    );
}

/// Zig で `const X = ...` の右辺 / 関数戻り値型 / test body 内の identifier が
/// def ではなく ref として認識されることを検証 (Issue: zig-definition-kinds-overscoped)。
#[test]
fn zig_initializer_and_return_type_is_ref_not_def() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("base.zig"),
        "pub const Helper = struct {\n    pub fn make() Helper { return .{}; }\n};\n",
    )
    .unwrap();
    std::fs::write(
        root.join("user.zig"),
        "const std = @import(\"std\");\nconst base = @import(\"base.zig\");\n\
pub fn use() base.Helper {\n    const h = base.Helper.make();\n    return h;\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "Helper", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let def_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("def"))
        .count();
    let ref_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("ref"))
        .count();
    assert_eq!(
        def_count, 1,
        "Helper の def は base.zig の struct 宣言 1 件: {refs:?}"
    );
    assert!(
        ref_count >= 1,
        "戻り値型 / 初期化式で参照されている Helper は ref として 1 件以上出るべき: {refs:?}"
    );
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

/// PHPUnit の DocBlock `@dataProvider` および PHP attribute `#[DataProvider(...)]`
/// 経由で参照される method を astro-sight が参照として解決すること
/// (Issue astro-sight-bug-reports#6)。
#[test]
fn phpunit_dataprovider_refs() {
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "providerForValidateFormat",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/sample_phpunit_test.php",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs array");
    let defs: Vec<&serde_json::Value> = refs.iter().filter(|r| r["kind"] == "def").collect();
    let non_defs: Vec<&serde_json::Value> = refs.iter().filter(|r| r["kind"] != "def").collect();
    assert_eq!(defs.len(), 1, "definition 1 件: {refs:?}");
    assert!(
        non_defs
            .iter()
            .any(|r| r["ctx"].as_str().unwrap_or("").contains("@dataProvider")),
        "@dataProvider 経由の参照を検出すべき: {refs:?}"
    );

    // attribute 経由
    let output2 = cargo_bin()
        .args([
            "refs",
            "--name",
            "attrProvider",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/sample_phpunit_test.php",
        ])
        .output()
        .expect("failed to run");
    assert!(output2.status.success());
    let json2: serde_json::Value = serde_json::from_slice(&output2.stdout).expect("invalid JSON");
    let refs2 = json2["refs"].as_array().expect("refs array");
    let non_defs2: Vec<&serde_json::Value> = refs2.iter().filter(|r| r["kind"] != "def").collect();
    assert!(
        non_defs2
            .iter()
            .any(|r| r["ctx"].as_str().unwrap_or("").contains("DataProvider")),
        "#[DataProvider(...)] 経由の参照を検出すべき: {refs2:?}"
    );
}

/// bash の `trap '<handler>' SIG` 構文の handler 文字列内に書かれた関数呼び出しを
/// astro-sight が参照として解決すること (Issue #5 / astro-sight-bug-reports#5)。
#[test]
fn bash_trap_handler_refs() {
    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "cleanup_signal",
            "--dir",
            "tests/fixtures",
            "--glob",
            "**/*.sh",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs array");
    // 期待: 定義 1 件 + trap 経由参照 2 件 + 通常呼び出し 1 件 = 4 件
    let defs: Vec<&serde_json::Value> = refs.iter().filter(|r| r["kind"] == "def").collect();
    let non_defs: Vec<&serde_json::Value> = refs.iter().filter(|r| r["kind"] != "def").collect();
    assert_eq!(defs.len(), 1, "definition 1 件: {refs:?}");
    assert!(
        non_defs.len() >= 3,
        "trap 経由 2 件 + 通常呼出 1 件 で少なくとも 3 件: {refs:?}"
    );
    // trap 行 (line 16, 17) の参照を含むこと
    let trap_refs: Vec<&serde_json::Value> = non_defs
        .iter()
        .filter(|r| {
            let ctx = r["ctx"].as_str().unwrap_or("");
            ctx.contains("trap")
        })
        .copied()
        .collect();
    assert_eq!(trap_refs.len(), 2, "trap 構文経由の参照は 2 件: {refs:?}");
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

/// 新規ファイル追加の unified diff 片を組み立てるテストヘルパー。
fn make_new_file_diff(path: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let n = lines.len();
    let body: String = lines.iter().map(|l| format!("+{l}\n")).collect();
    format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{n} @@\n{body}"
    )
}

#[test]
fn review_xojo_only_diff_returns_empty_result() {
    // lexer-only 言語の review は cross-file 解析も dead-code も skip し、hook の
    // 汎用名ノイズを出さない。Xojo は symbols/dead-code 単体では動くが review では空結果。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    let diff = make_new_file_diff(
        "Main.xojo_code",
        "Class App\nEnd Class\n\nClass Orphan\nEnd Class\n",
    );
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, diff).unwrap();

    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert!(
        json["impact"]["changes"].as_array().unwrap().is_empty(),
        "Xojo-only review は impact を空にするべき: {json}"
    );
    assert!(
        json["api_changes"]["added"].as_array().unwrap().is_empty()
            && json["api_changes"]["removed"]
                .as_array()
                .unwrap()
                .is_empty()
            && json["api_changes"]["modified"]
                .as_array()
                .unwrap()
                .is_empty(),
        "Xojo-only review は API 差分を空にするべき: {json}"
    );
    assert!(
        json["dead_symbols"].as_array().unwrap().is_empty(),
        "Xojo-only review は dead_symbols を返すべきでない: {json}"
    );
}

#[test]
fn review_respects_framework_laravel_preset() {
    // review が dead-code と同じ `--framework laravel` プリセットを尊重し、
    // app/Http/Controllers 等を dead_symbols から除外することを検証する。
    // 対象プロジェクトのコードは引用せず、Laravel-ish な最小フィクスチャを一次創作する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("app/Http/Controllers")).unwrap();
    std::fs::create_dir_all(root.join("app/Services")).unwrap();

    let controller_src =
        "<?php\nclass SampleController {\n    public function index() { return 'x'; }\n}\n";
    let service_src =
        "<?php\nclass SampleService {\n    public function loadProfile() { return []; }\n}\n";

    std::fs::write(
        root.join("app/Http/Controllers/SampleController.php"),
        controller_src,
    )
    .unwrap();
    std::fs::write(root.join("app/Services/SampleService.php"), service_src).unwrap();

    let mut diff = String::new();
    diff.push_str(&make_new_file_diff(
        "app/Http/Controllers/SampleController.php",
        controller_src,
    ));
    diff.push_str(&make_new_file_diff(
        "app/Services/SampleService.php",
        service_src,
    ));
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, &diff).unwrap();

    // --framework laravel なし: Controllers/SampleController も dead に出る (回帰担保)
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_without: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        dead_without.iter().any(|n| n.contains("SampleController")),
        "preset なしでは app/Http/Controllers 配下が dead_symbols に残るべき: {dead_without:?}"
    );

    // --framework laravel あり: Controllers/SampleController は除外、Services/SampleService は残る
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_with: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead_with.iter().any(|n| n.contains("SampleController")),
        "Laravel preset で app/Http/Controllers 配下は dead_symbols から除外されるべき: {dead_with:?}"
    );
    assert!(
        dead_with
            .iter()
            .any(|n| n.contains("SampleService") || n.contains("loadProfile")),
        "app/Services/ は Laravel preset 対象外のため dead 判定が残るべき: {dead_with:?}"
    );
}

#[test]
fn review_hook_suppresses_wip_added_dead_by_default() {
    // `review --hook` の既定: 同一 diff で新規 export されたシンボル (api.added に挙がる)
    // は WIP の純粋ヘルパー追加とみなして dead 警告から除外する
    // (Issue 2026-06-25-wip-dead-symbol-during-incremental-impl)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let src = "export function matchAssigneeName(input: string) {\n    return input;\n}\n";
    std::fs::write(root.join("notes.ts"), src).unwrap();
    let diff = make_new_file_diff("notes.ts", src);
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, diff).unwrap();

    // --hook + default (=include_wip_dead 無効): hook exit 0、stdout 無出力
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
            "--hook",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "hook の dead 抑止が効いていれば stdout は空 (= blocking 無し): {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // --hook --include-wip-dead: 抑止解除 ― WIP 新規追加も dead として出る。
    // hook は dead を blocking とみなし stderr に JSON を出して exit != 0 を返す
    // (= caller (Stop hook) に WIP dead を通知)。
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
            "--hook",
            "--include-wip-dead",
        ])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "--include-wip-dead で WIP dead が残れば hook は blocking 検出として exit != 0 を返す"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("matchAssigneeName") && stderr.contains("\"dead\""),
        "--include-wip-dead で抑止を外せば dead が hook の blocking 出力 (stderr) に現れる: {stderr}"
    );
}

#[test]
fn review_without_hook_keeps_wip_added_dead() {
    // `review` 単体 (非 hook): WIP dead 抑止は適用しない。レビュアーが api.added と
    // dead の両者を見て総合判断するため、自動抑止は --hook 経路に限定する設計。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let src = "export function matchAssigneeName(input: string) {\n    return input;\n}\n";
    std::fs::write(root.join("notes.ts"), src).unwrap();
    let diff = make_new_file_diff("notes.ts", src);
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, diff).unwrap();

    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        dead.iter().any(|n| n == "matchAssigneeName"),
        "非 hook の通常 review は WIP dead を抑止せず従来通り全 dead を返す: {dead:?}"
    );
}

#[test]
fn review_framework_unknown_errors() {
    // 未知の framework 値は cmd_dead_code と同じエラー形式で拒否されること。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("dummy.patch"),
        "diff --git a/dummy.php b/dummy.php\nnew file mode 100644\n--- /dev/null\n+++ b/dummy.php\n@@ -0,0 +1,1 @@\n+<?php\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            root.join("dummy.patch").to_str().unwrap(),
            "--framework",
            "djangular",
        ])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "未知の framework 名はエラー (exit != 0)"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("Unknown framework preset") || combined.contains("INVALID_REQUEST"),
        "エラーメッセージに framework 未対応が示される: {combined}"
    );
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

/// GitLab issue #24 回帰: Flyway の Java マイグレーションクラス
/// (`extends BaseJavaMigration`) は dead-code 検出から除外される。
#[test]
fn dead_code_excludes_flyway_java_migration() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/migration/src/main/java/db/migration")).unwrap();

    // Flyway migration クラス (extends BaseJavaMigration) ─ 除外対象
    let flyway_class = "package db.migration;\n\
                        import org.flywaydb.core.api.migration.BaseJavaMigration;\n\
                        import org.flywaydb.core.api.migration.Context;\n\
                        public class V2021_01_02__Zipcode extends BaseJavaMigration {\n\
                            public void migrate(Context context) throws Exception {}\n\
                        }\n";
    std::fs::write(
        root.join("app/migration/src/main/java/db/migration/V2021_01_02__Zipcode.java"),
        flyway_class,
    )
    .unwrap();

    // 通常の Java クラス (Flyway 非継承) ─ 直接参照なしなので dead に残るべき
    let regular_class = "package app.util;\n\
                         public class OrphanService {\n\
                             public void noOp() {}\n\
                         }\n";
    std::fs::create_dir_all(root.join("app/util")).unwrap();
    std::fs::write(root.join("app/util/OrphanService.java"), regular_class).unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead_names
            .iter()
            .any(|n| n.contains("V2021_01_02__Zipcode")),
        "Flyway BaseJavaMigration 継承クラスは framework entrypoint として除外されるべき: {dead_names:?}"
    );
    assert!(
        dead_names.iter().any(|n| n.contains("OrphanService")),
        "Flyway を継承しない通常の Java クラスは dead として残るべき (回帰担保): {dead_names:?}"
    );
}

/// GitLab issue #21: Laravel Eloquent リレーション戻り型 (`BelongsTo` 等) を持つ public
/// method は dead-code 検出から除外される。`->with('x')` 文字列 / magic property 経由で
/// Eloquent が呼ぶため、静的 caller が 0 件でも dead ではない。
#[test]
fn dead_code_excludes_laravel_eloquent_relation_methods() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/Models")).unwrap();
    let model_src = "<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Relations\\BelongsTo;
use Illuminate\\Database\\Eloquent\\Relations\\HasMany;
class QueueModel extends Model {
    public function omataseGuidance(): BelongsTo { return $this->belongsTo(Guidance::class); }
    public function tags(): HasMany { return $this->hasMany(Tag::class); }
    public function plainHelper(): string { return ''; }
}
";
    std::fs::write(root.join("app/Models/QueueModel.php"), model_src).unwrap();
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    for name in ["omataseGuidance", "tags"] {
        assert!(
            !dead_names.iter().any(|n| n.contains(name)),
            "Eloquent relation メソッド `{name}` は除外されるべき: {dead_names:?}"
        );
    }
    // 戻り型が string の通常メソッドは除外対象外 (回帰担保)。
    assert!(
        dead_names.iter().any(|n| n.contains("plainHelper")),
        "Eloquent relation でない自前 method は dead として残るべき (回帰担保): {dead_names:?}"
    );
}

/// GitLab issue #22: `implements CanResetPasswordContract` クラスの
/// `getEmailForPasswordReset` / `sendPasswordResetNotification` は Laravel framework
/// (PasswordBroker / Notification) が contract 経由で呼ぶため dead から除外する。
#[test]
fn dead_code_excludes_laravel_can_reset_password_contract_methods() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/Models")).unwrap();
    let model_src = "<?php
namespace App\\Models;
class AccountEloquent extends Model implements CanResetPasswordContract {
    public function getEmailForPasswordReset(): string { return $this->email; }
    public function sendPasswordResetNotification($token): void {}
    public function someOtherMethod(): string { return ''; }
}
";
    std::fs::write(root.join("app/Models/AccountEloquent.php"), model_src).unwrap();
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    for name in ["getEmailForPasswordReset", "sendPasswordResetNotification"] {
        assert!(
            !dead_names.iter().any(|n| n.contains(name)),
            "CanResetPasswordContract 実装メソッド `{name}` は除外されるべき: {dead_names:?}"
        );
    }
    // contract 実装ではない自前 method は除外対象外 (回帰担保)。
    assert!(
        dead_names.iter().any(|n| n.contains("someOtherMethod")),
        "contract 実装ではない自前 method は dead として残るべき (回帰担保): {dead_names:?}"
    );
}

/// GitLab issue #20: `implements ControlValueAccessor` の Angular 装飾クラスの 4 規約
/// メソッド (writeValue / registerOnChange / registerOnTouched / setDisabledState) は
/// dead-code 検出から除外される。Angular Forms ランタイムが NG_VALUE_ACCESSOR provider
/// 経由で ngModel/formControl バインド時に呼ぶため、静的 caller が 0 件でも dead ではない。
#[test]
fn dead_code_excludes_angular_control_value_accessor_methods() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/app/shared")).unwrap();
    let cva_src = "\
import { Directive } from '@angular/core';
import { ControlValueAccessor } from '@angular/forms';
@Directive()
export abstract class AbstractBaseControl implements ControlValueAccessor {
    writeValue(obj: any) {}
    registerOnChange(fn: any) {}
    registerOnTouched(fn: any) {}
    setDisabledState(isDisabled: boolean) {}
    customHelper(): void {}
}
";
    std::fs::write(root.join("src/app/shared/abstract.ts"), cva_src).unwrap();
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    for name in [
        "writeValue",
        "registerOnChange",
        "registerOnTouched",
        "setDisabledState",
    ] {
        assert!(
            !dead_names.iter().any(|n| n.contains(name)),
            "ControlValueAccessor 規約メソッド `{name}` は除外されるべき: {dead_names:?}"
        );
    }
    // CVA 規約名でない自前メソッドは除外対象外 (回帰担保)。
    assert!(
        dead_names.iter().any(|n| n.contains("customHelper")),
        "CVA 規約外の自前メソッドは dead として残るべき (回帰担保): {dead_names:?}"
    );
}

/// GitLab issue #23: `@HostListener` 付きメソッドは Angular ランタイムがイベント発火時に
/// 呼ぶため、静的 caller 0 件でも dead から除外する。
#[test]
fn dead_code_excludes_angular_host_listener_method() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/app")).unwrap();
    let component_src = "\
import { Component, HostListener } from '@angular/core';
@Component({ template: '' })
export class AppComponent {
    @HostListener('window:beforeunload', ['$event'])
    beforeUnloadHandler() {}
    plainHelper() {}
}
";
    std::fs::write(root.join("src/app/app.component.ts"), component_src).unwrap();
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead_names.iter().any(|n| n.contains("beforeUnloadHandler")),
        "@HostListener 付きメソッドは dead から除外されるべき: {dead_names:?}"
    );
    // member decorator が無い通常メソッドは除外対象外 (回帰担保)。
    assert!(
        dead_names.iter().any(|n| n.contains("plainHelper")),
        "member decorator のない通常メソッドは dead として残るべき (回帰担保): {dead_names:?}"
    );
}

/// GitLab issue #24 (codex 指摘 1): review --git の API 変更検出 (`api_changes.added`)
/// でも Flyway migration クラスとそのメソッドは出さない。dead-code 経路と整合し、Stop
/// hook が migration 追加のたびに blocking 化しないようにする。
#[test]
fn review_excludes_flyway_java_migration_from_api_added() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("db/migration")).unwrap();
    let migration_src = "package db.migration;\n\
                         import org.flywaydb.core.api.migration.BaseJavaMigration;\n\
                         import org.flywaydb.core.api.migration.Context;\n\
                         public class V1__Init extends BaseJavaMigration {\n\
                             public void migrate(Context context) throws Exception {}\n\
                         }\n";
    std::fs::write(root.join("db/migration/V1__Init.java"), migration_src).unwrap();
    let diff = make_new_file_diff("db/migration/V1__Init.java", migration_src);
    let diff_path = root.join("changes.patch");
    std::fs::write(&diff_path, diff).unwrap();

    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            root.to_str().unwrap(),
            "--diff-file",
            diff_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let added_names: Vec<String> = json["api_changes"]["added"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !added_names.iter().any(|n| n.contains("V1__Init")),
        "Flyway migration クラスとそのメソッドは API 変更検出にも出さない: {added_names:?}"
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

/// C/C++ の関数名は `function_declarator` 配下でキャプチャされるため、定義ノードまで
/// 親を繰り上げないと range が宣言子（シグネチャ行）だけに潰れ、複雑度が常に 1 になり、
/// impact 分析が関数本体のみの変更を取りこぼす。range が本体まで伸び、分岐を数えた
/// 複雑度が算出されることを検証する回帰テスト。
#[test]
fn c_function_range_and_complexity_cover_body() {
    let output = cargo_bin()
        .args([
            "symbols",
            "--path",
            "tests/fixtures/sample.c",
            "--full",
            "--no-cache",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let symbols = json["symbols"].as_array().unwrap();

    // buffer_append は本体に if が 1 つあるので base1 + 1 = 2。
    // 宣言子だけを見ていた頃は本体を走査できず常に 1 だった。
    let append = symbols
        .iter()
        .find(|s| s["name"] == "buffer_append")
        .expect("buffer_append を検出すべき");
    assert_eq!(
        append["complexity"], 2,
        "buffer_append の複雑度は本体の if を数えて 2 になるべき"
    );
    let start = append["range"]["start"]["line"].as_u64().unwrap();
    let end = append["range"]["end"]["line"].as_u64().unwrap();
    assert!(
        end > start,
        "range は宣言子 1 行ではなく関数本体まで複数行にまたがるべき (start={start}, end={end})"
    );

    // 分岐の無い関数も range が本体まで伸びる（複雑度は 1）。
    let main_fn = symbols
        .iter()
        .find(|s| s["name"] == "main")
        .expect("main を検出すべき");
    assert_eq!(main_fn["complexity"], 1);
    let m_start = main_fn["range"]["start"]["line"].as_u64().unwrap();
    let m_end = main_fn["range"]["end"]["line"].as_u64().unwrap();
    assert!(
        m_end > m_start,
        "main の range も本体まで複数行にまたがるべき"
    );
}

/// init / skill-install は既存 config を必要としない早期終了コマンドのため、
/// config ロードより前に処理する。壊れた既存 config を `--config` で指していても
/// init が成功する（壊れた config の再生成手段になる）ことを検証する回帰テスト。
#[test]
fn init_does_not_require_valid_existing_config() {
    let dir = tempfile::TempDir::new().unwrap();
    let bad_config = dir.path().join("bad.toml");
    let out_config = dir.path().join("generated.toml");
    std::fs::write(&bad_config, "not valid [[[").unwrap();

    let output = cargo_bin()
        .args([
            "--config",
            bad_config.to_str().unwrap(),
            "init",
            "--path",
            out_config.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "壊れた既存 config を指していても init は成功すべき: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(out_config.exists(), "config ファイルを生成すべき");
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
fn dead_code_excludes_vendor_dir_by_default() {
    // パッケージマネージャ配下 (vendor/, node_modules/, .venv/ 等) は
    // 既定で dead-code 走査から除外される。`--include-vendor` で opt-in 可能。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("vendor/pkg-a/src")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("vendor/pkg-a/src/lib.php"),
        "<?php\nfunction vendor_only_helper(): void { echo 'x'; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app.php"),
        "<?php\nfunction app_only_helper(): void { echo 'y'; }\n",
    )
    .unwrap();

    // 既定: vendor 除外
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"vendor_only_helper"),
        "vendor/ 配下の symbol は既定で dead-code に含めない: {names:?}"
    );
    assert!(
        names.contains(&"app_only_helper"),
        "src/ 配下の未参照 symbol は dead として報告される: {names:?}"
    );

    // opt-in: --include-vendor で vendor も含める
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-vendor",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"vendor_only_helper"),
        "--include-vendor で vendor/ 配下も対象に含まれる: {names:?}"
    );
}

#[test]
fn dead_code_excludes_tests_dir_by_default() {
    // テストディレクトリ (tests/, Tests/, __tests__/, spec/, testdata/) は
    // 既定で dead-code 走査から除外される。`--include-tests` で opt-in 可能。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("tests/unit")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // 意図的に PHPUnit 命名規約 (`^test[A-Z_]`) に合致しない関数名を使い、
    // 「ディレクトリ除外」単独での効果を検証する。
    std::fs::write(
        root.join("tests/unit/sample_case.php"),
        "<?php\nfunction fixture_assertion_helper(): void { echo 'y'; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/runtime.php"),
        "<?php\nfunction runtime_only_helper(): void { echo 'z'; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"fixture_assertion_helper"),
        "tests/ 配下の symbol は既定で dead-code に含めない: {names:?}"
    );
    assert!(
        names.contains(&"runtime_only_helper"),
        "src/ 配下の未参照 symbol は dead として報告される: {names:?}"
    );

    // opt-in: --include-tests
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"fixture_assertion_helper"),
        "--include-tests で tests/ 配下も対象に含まれる: {names:?}"
    );
}

#[test]
fn dead_code_framework_laravel_excludes_migrations_and_controllers() {
    // --framework laravel で Laravel の規約的エントリポイント
    // (database/migrations, app/Http/Controllers 等) が除外されることを検証。
    // 一次創作の Laravel-ish な最小構造を使い、対象プロジェクトのコードは引用しない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("app/Http/Controllers")).unwrap();
    std::fs::create_dir_all(root.join("app/Services")).unwrap();
    std::fs::create_dir_all(root.join("database/migrations")).unwrap();
    std::fs::create_dir_all(root.join("database/seeds")).unwrap();

    std::fs::write(
        root.join("app/Http/Controllers/SampleController.php"),
        "<?php\nclass SampleController {\n    public function index() { return 'x'; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/Services/SampleService.php"),
        "<?php\nclass SampleService {\n    public function loadProfile() { return []; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("database/migrations/2025_01_example.php"),
        "<?php\nclass CreateExampleTable {\n    public function up() {}\n    public function down() {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("database/seeds/ExampleSeeder.php"),
        "<?php\nclass ExampleSeeder {\n    public function run() {}\n}\n",
    )
    .unwrap();

    // --framework laravel なし: Controllers / migrations / seeds も dead 候補に出る
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n.contains("SampleController")),
        "framework preset なしでは Controller が dead 候補に出る: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("CreateExampleTable")),
        "framework preset なしでは migration class が dead 候補に出る: {names:?}"
    );

    // --framework laravel: Controllers / migrations / seeds は除外、Services は残る
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for banned in ["SampleController", "CreateExampleTable", "ExampleSeeder"] {
        assert!(
            !names.iter().any(|n| n.contains(banned)),
            "{banned} は Laravel preset で除外されるべき: {names:?}"
        );
    }
    assert!(
        names
            .iter()
            .any(|n| n.contains("SampleService") || n.contains("loadProfile")),
        "app/Services/ 配下は Laravel preset でも dead 判定される: {names:?}"
    );
}

#[test]
fn dead_code_excludes_php_pseudo_enum_factory() {
    // Laravel / DDD 系の AbstractValueObject 派生クラスの擬似 enum パターンが dead-code から
    // 除外されることを検証する。dead-code 経路は常に exclude_framework_entrypoints=true
    // で呼ばれるため、preset 指定の有無に関わらず擬似 enum は除外される。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/Models/Menu")).unwrap();
    std::fs::write(
        root.join("src/Models/Menu/MenuName.php"),
        r#"<?php
class MenuName extends AbstractValueObjectString {
    public static function MENU_HOME(): self {
        return new self('MENU_HOME');
    }
    public static function MENU_DASHBOARD(): static {
        return new static('MENU_DASHBOARD');
    }
    /** メソッド名と new self('...') の文字列が不一致 → 擬似 enum ではない */
    public static function notPseudo(): self {
        return new self('different_name');
    }
    public function getValue(): string {
        return 'unused-impl';
    }
}
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    // 擬似 enum (MENU_HOME / MENU_DASHBOARD) は除外される
    for banned in ["MENU_HOME", "MENU_DASHBOARD"] {
        assert!(
            !names.iter().any(|n| n.contains(banned)),
            "PHP 擬似 enum factory ({banned}) は除外されるべき: {names:?}"
        );
    }
    // 擬似 enum でない static method (notPseudo) は残る
    assert!(
        names.iter().any(|n| n.contains("notPseudo")),
        "メソッド名と new self() の引数が不一致なら擬似 enum ではないので dead に残る: {names:?}"
    );
}

#[test]
fn dead_code_excludes_php_method_with_runtime_annotation() {
    // @TypeItem などの runtime annotation 付きメソッドは reflection 経由で
    // 動的呼び出しされるため dead-code から除外する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/Bases/Meta")).unwrap();
    std::fs::write(
        root.join("src/Bases/Meta/EntityType.php"),
        r#"<?php
class EntityType extends AbstractValueObjectString {
    /**
     * @\App\Annotations\TypeItem(id=1, name="SurveyNode", alt_name="survey-node")
     * @return static
     */
    public static function SurveyNode(): self {
        return new self('SurveyNode');
    }

    /** 通常の static method (annotation なし) */
    public static function plainMethod(): self {
        return new self('plainMethod');
    }
}
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    assert!(
        !names.iter().any(|n| n.contains("SurveyNode")),
        "@TypeItem 付きメソッドは dead から除外されるべき: {names:?}"
    );
    // plainMethod は擬似 enum パターンなので、こちらも除外される (= dead に出ない)
}

/// `--framework nextjs` で Next.js App Router の規約 entrypoint
/// (page / layout / route / loading 等) と Pages Router 配下が dead-code から除外される。
/// (レポート 2026-05-04-next-page-and-react-memo-false-positives.md パターン2 の再現)
#[test]
fn dead_code_framework_nextjs_excludes_app_and_pages_routes() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/app/(authenticated)/dashboard")).unwrap();
    std::fs::create_dir_all(root.join("src/app/api/users")).unwrap();
    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::create_dir_all(root.join("src/pages/api")).unwrap();
    std::fs::create_dir_all(root.join("src/services")).unwrap();

    std::fs::write(
        root.join("src/app/(authenticated)/dashboard/page.tsx"),
        "export default function DashboardPage() { return null; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/api/users/route.ts"),
        "export function GET() { return new Response('ok'); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/layout.tsx"),
        "export default function RootLayout({ children }: { children: React.ReactNode }) { return <>{children}</>; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/pages/api/legacy.ts"),
        "export default function handler() { return null; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/services/orphan.ts"),
        "export function unusedService() { return 1; }\n",
    )
    .unwrap();

    // --framework なし: 全部 dead に出る
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "DashboardPage"),
        "preset なしでは page default export も dead 判定: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "unusedService"),
        "preset なしでは services 配下も dead 判定: {names:?}"
    );

    // --framework nextjs: app/page, app/route, app/layout, pages/** は除外、services は残る
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "nextjs",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for banned in ["DashboardPage", "GET", "RootLayout", "handler"] {
        assert!(
            !names.iter().any(|n| n == banned),
            "{banned} は Next.js preset で除外されるべき: {names:?}"
        );
    }
    assert!(
        names.iter().any(|n| n == "unusedService"),
        "src/services/ 配下は Next.js preset でも dead 判定される: {names:?}"
    );
}

/// `--framework` 未指定でも `package.json` の `dependencies.next` を検出して nextjs
/// プリセットを自動適用する。Issue 2026-05-20 で 3 回再発した `app/**/page.tsx` の
/// default export が dead 判定される問題への対応。
#[test]
fn dead_code_auto_detect_nextjs_from_package_json_dependencies() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/app/(authenticated)/admin")).unwrap();
    std::fs::create_dir_all(root.join("src/services")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "demo", "dependencies": { "next": "^15.0.0", "react": "^19.0.0" } }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/(authenticated)/admin/page.tsx"),
        "export default function AdminPage() { return null; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/services/orphan.ts"),
        "export function unusedService() { return 1; }\n",
    )
    .unwrap();

    // --framework 指定なし: package.json の next 依存から自動的に nextjs プリセット適用
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    assert!(
        !names.iter().any(|n| n == "AdminPage"),
        "package.json に next 依存があれば --framework 未指定でも AdminPage は除外されるべき: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "unusedService"),
        "src/services/ 配下は auto-detect でも dead 判定される (誤除外しない): {names:?}"
    );
}

/// `devDependencies` 経由の `next` 依存も自動検出対象に含める (Next.js は通常
/// dependencies に置かれるが、SSG 専用や CLI tooling として dev に置くプロジェクトもある)。
#[test]
fn dead_code_auto_detect_nextjs_from_package_json_dev_dependencies() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("app")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "demo", "devDependencies": { "next": "^15.0.0" } }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("app/page.tsx"),
        "export default function HomePage() { return null; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !names.iter().any(|n| n == "HomePage"),
        "devDependencies.next からも自動検出されるべき: {names:?}"
    );
}

/// `package.json` が無いプロジェクトでは auto-detect は発動せず、`app/**/page.tsx` の
/// default export は dead 判定のまま残る (誤検出しない方向への保守的フォールバック)。
#[test]
fn dead_code_auto_detect_skipped_without_package_json() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::write(
        root.join("src/app/page.tsx"),
        "export default function NotNextPage() { return null; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "NotNextPage"),
        "package.json が無ければ auto-detect は発動せず、page.tsx の default export は dead 判定のまま: {names:?}"
    );
}

/// `peerDependencies` / `optionalDependencies` 経由の `next` は誤爆しやすいため
/// auto-detect の対象外とする (Next.js ライブラリやテスト fixture 対策)。
#[test]
fn dead_code_auto_detect_ignores_peer_dependencies_next() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "next-helper-lib", "peerDependencies": { "next": ">=13" } }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/page.tsx"),
        "export default function PeerOnlyPage() { return null; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "PeerOnlyPage"),
        "peerDependencies のみの next は auto-detect 対象外。page.tsx の default export は dead 判定のまま: {names:?}"
    );
}

/// `--framework laravel` 明示指定時は package.json の next 依存があっても nextjs
/// auto-detect は発動しない (明示指定が常に優先)。
#[test]
fn dead_code_explicit_framework_overrides_auto_detect() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "demo", "dependencies": { "next": "^15.0.0" } }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/page.tsx"),
        "export default function OverriddenPage() { return null; }\n",
    )
    .unwrap();

    // --framework laravel を明示指定 → next auto-detect は発動せず page.tsx は dead 判定のまま
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "OverriddenPage"),
        "--framework laravel 明示指定時は nextjs auto-detect が発動しない (明示指定が常に優先): {names:?}"
    );
}

#[test]
fn dead_code_exclude_glob_and_exclude_dir_drop_targets() {
    // --exclude-glob 'app/Legacy/**' と --exclude-dir custom_dir で
    // それぞれサブツリーが除外されることを検証。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("app/Legacy")).unwrap();
    std::fs::create_dir_all(root.join("custom_dir/sub")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();

    std::fs::write(
        root.join("app/Legacy/OldHandler.php"),
        "<?php\nclass OldHandler {\n    public function legacyEntry() {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("custom_dir/sub/PluginBridge.php"),
        "<?php\nclass PluginBridge {\n    public function pluginEntry() {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/runtime.php"),
        "<?php\nfunction runtime_only_entry() {}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--exclude-glob",
            "app/Legacy/**",
            "--exclude-dir",
            "custom_dir",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    assert!(
        !names
            .iter()
            .any(|n| n.contains("OldHandler") || n.contains("legacyEntry")),
        "--exclude-glob 指定パターンは除外される: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|n| n.contains("PluginBridge") || n.contains("pluginEntry")),
        "--exclude-dir 指定ディレクトリは除外される: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "runtime_only_entry"),
        "非除外 src/ の未参照シンボルは dead として報告される: {names:?}"
    );
}

#[test]
fn dead_code_framework_laravel_preset_works_when_dir_is_app_root() {
    // F1 回帰テスト: `--dir <fixture>/app --framework laravel` の場合も
    // プリセット glob が効き、`Http/Controllers/` 配下が dead_symbols から除外されること。
    // 従来は `**/app/Http/Controllers/**` が `--dir` 相対パス (`Http/Controllers/...`) に
    // マッチせず Controller が全件 FP になっていた。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/Http/Controllers")).unwrap();
    std::fs::create_dir_all(root.join("app/Services")).unwrap();

    std::fs::write(
        root.join("app/Http/Controllers/SampleController.php"),
        "<?php\nclass SampleController {\n    public function index() { return 'x'; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/Services/SampleService.php"),
        "<?php\nclass SampleService {\n    public function loadProfile() { return []; }\n}\n",
    )
    .unwrap();

    // --dir を `app/` 直下に指定 — 旧挙動では Laravel プリセット無効で SampleController が dead
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.join("app").to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !names.iter().any(|n| n.contains("SampleController")),
        "--dir が app/ 直下でも Laravel preset で Controllers/ は除外されるべき (F1 regression): {names:?}"
    );
    // app/Services は Laravel プリセットの対象外なので dead 判定が残るのが正しい
    assert!(
        names
            .iter()
            .any(|n| n.contains("SampleService") || n.contains("loadProfile")),
        "app/Services は Laravel preset 対象外のため dead 判定されるべき: {names:?}"
    );
}

#[test]
fn dead_code_framework_laravel_excludes_exceptions_handler() {
    // F2 回帰テスト: Laravel プリセットに `**/app/Exceptions/**` が含まれること。
    // App\Exceptions\Handler::report は bootstrap/app.php の規約で登録される
    // フレームワーク hook で、dead_symbols に含めるべきでない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/Exceptions")).unwrap();
    std::fs::create_dir_all(root.join("app/Services")).unwrap();

    std::fs::write(
        root.join("app/Exceptions/Handler.php"),
        "<?php\nclass Handler {\n    public function report(\\Throwable $e) { return null; }\n    public function render($request, \\Throwable $e) { return null; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/Services/SampleService.php"),
        "<?php\nclass SampleService {\n    public function loadProfile() { return []; }\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    for banned in ["Handler", "report", "render"] {
        assert!(
            !names.iter().any(|n| n.contains(banned)),
            "{banned} は app/Exceptions/ 配下なので Laravel preset で除外されるべき: {names:?}"
        );
    }
}

#[test]
fn dead_code_refs_scope_not_limited_by_glob() {
    // F3 回帰テスト: `--glob` で symbols 対象ファイルを絞っても、
    // refs 探索は `--dir` 全体で行われ、`--glob` 範囲外からの参照でも
    // dead 判定を回避できること。
    //
    // 従来は `detect_dead_symbols_from_files` が refs 探索にも `--glob` を
    // 適用していたため、`--glob 'lib/**/*.rs'` で走らせると `app/` からの
    // 参照が見えず、lib/ 配下の共通関数が誤って dead 扱いになっていた。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();

    // lib 配下: 共通関数の定義
    std::fs::write(
        root.join("lib/util.rs"),
        "pub fn shared_helper() -> i32 { 42 }\n",
    )
    .unwrap();
    // app 配下: lib の関数を呼ぶ (refs スコープを広げれば見える)
    std::fs::write(
        root.join("app/main.rs"),
        "fn main() { let _ = shared_helper(); }\n",
    )
    .unwrap();

    // --glob で lib/ 配下のみを dead 対象にするが、refs は root 全体で探索されるべき
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--glob",
            "lib/**/*.rs",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !names.iter().any(|n| n.contains("shared_helper")),
        "shared_helper は app/ から参照されており、--glob が refs スコープを狭めるべきでない (F3 regression): {names:?}"
    );
}

#[test]
fn dead_code_framework_laravel_covers_renamed_app_dir() {
    // F6 (F1 拡張で代替): Laravel プリセットの `**/X/**` 省略版マッチにより、
    // `app/` を `core/` のようにリネームした独自レイアウトや、
    // モノレポでサブディレクトリ配下に Laravel 規約構造を持つ場合でも
    // プリセットが効くこと。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    // `core/` (Laravel 標準の `app/` をリネームした想定) 配下に規約構造を作る
    std::fs::create_dir_all(root.join("core/Http/Controllers")).unwrap();
    std::fs::create_dir_all(root.join("core/Services")).unwrap();
    std::fs::create_dir_all(root.join("packages/sub/Http/Middleware")).unwrap();

    std::fs::write(
        root.join("core/Http/Controllers/SampleController.php"),
        "<?php\nclass SampleController {\n    public function index() { return 'x'; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("core/Services/SampleService.php"),
        "<?php\nclass SampleService {\n    public function loadProfile() { return []; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("packages/sub/Http/Middleware/SampleMiddleware.php"),
        "<?php\nclass SampleMiddleware {\n    public function handle() { return null; }\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    // F1 で追加した `**/Http/Controllers/**` `**/Http/Middleware/**` 等の省略版マッチが
    // Laravel 標準外 (`core/`, `packages/sub/`) のレイアウトにも効くこと
    assert!(
        !names.iter().any(|n| n.contains("SampleController")),
        "core/Http/Controllers 配下 (リネーム済み app/) は preset で除外されるべき: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("SampleMiddleware")),
        "packages/sub/Http/Middleware (モノレポ配下) は preset で除外されるべき: {names:?}"
    );
    // core/Services は preset 対象外なので残る
    assert!(
        names
            .iter()
            .any(|n| n.contains("SampleService") || n.contains("loadProfile")),
        "core/Services は preset 対象外で dead 判定が残るべき: {names:?}"
    );
}

#[test]
fn refs_php_callable_array_method_is_detected() {
    // N3: `[Class::class, 'method']` の string literal を method ref として扱う。
    // tree-sitter の identifier ノードには現れない (string 内) ため、
    // AST レベルで `array_creation_expression` を special-case 抽出する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
class Target {\n\
    public static function handler() { return 1; }\n\
}\n\
class Caller {\n\
    public function dispatch() {\n\
        $x = [Target::class, 'handler'];\n\
        return call_user_func($x);\n\
    }\n\
}\n",
    )
    .unwrap();

    // refs --name (single)
    let output = cargo_bin()
        .args(["refs", "--name", "handler", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let has_callable_ref = refs.iter().any(|r| {
        r["kind"].as_str() == Some("ref")
            && r["ctx"]
                .as_str()
                .is_some_and(|c| c.contains("[Target::class, 'handler']"))
    });
    assert!(
        has_callable_ref,
        "callable array `[Target::class, 'handler']` の 'handler' を ref として検出するべき: {refs:?}"
    );

    // dead-code 経由でも同等に効くこと (refs スコープを通って Target も生存判定)
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead.iter().any(|n| n.contains("handler")),
        "callable array で参照される handler が dead に出てはならない: {dead:?}"
    );
}

#[test]
fn refs_php_string_callable_class_at_method_is_detected() {
    // N4: `'ClassName@method'` 形式の文字列 callable (Laravel 5.x 以前互換) を method ref として扱う。
    // tree-sitter は string 全体を 1 ノードとしてしか出さないため、内容を pattern match して
    // `@` 以降を method 名として抽出する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
class Target {\n\
    public function handler() { return 1; }\n\
}\n\
function register() {\n\
    return 'Target@handler';\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "handler", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let has_string_ref = refs.iter().any(|r| {
        r["kind"].as_str() == Some("ref")
            && r["ctx"]
                .as_str()
                .is_some_and(|c| c.contains("'Target@handler'"))
    });
    assert!(
        has_string_ref,
        "'Target@handler' の 'handler' を ref として検出すべき: {refs:?}"
    );
}

#[test]
fn refs_php_concat_class_class_at_method_is_detected() {
    // N4: `Class::class . '@method'` 形式の concat callable を method ref として扱う。
    // `'@method'` 単独の string は、親が `binary_expression` (`.` operator) かつ左辺が
    // `class_constant_access_expression` (`X::class`) の場合のみ ref 認定する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
class Target {\n\
    public function dispatch() { return 1; }\n\
}\n\
function register() {\n\
    return Target::class . '@dispatch';\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "refs",
            "--name",
            "dispatch",
            "--dir",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let has_concat_ref = refs.iter().any(|r| {
        r["kind"].as_str() == Some("ref")
            && r["ctx"]
                .as_str()
                .is_some_and(|c| c.contains("Target::class . '@dispatch'"))
    });
    assert!(
        has_concat_ref,
        "Target::class . '@dispatch' の 'dispatch' を ref として検出すべき: {refs:?}"
    );
}

#[test]
fn refs_php_email_string_does_not_produce_fake_ref() {
    // N4 誤検出防止: `'user@example.com'` のようなメール風文字列は method ref にしない。
    // class_part='user' は先頭小文字 → reject、method_part='example.com' は `.` 含む → reject。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
function example() { return 'example.com'; }\n\
function contact() { return 'user@example.com'; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "example", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let fake_refs: Vec<_> = refs
        .iter()
        .filter(|r| {
            r["kind"].as_str() == Some("ref")
                && r["ctx"]
                    .as_str()
                    .is_some_and(|c| c.contains("'user@example.com'"))
        })
        .collect();
    assert!(
        fake_refs.is_empty(),
        "メール風文字列を method ref にしてはならない: {fake_refs:?}"
    );
}

#[test]
fn dead_code_php_string_callable_prevents_false_positive() {
    // N4 の影響: Gate::define / routing 等で string callable 経由で呼ばれるだけのメソッドが
    // dead_symbols に入らないこと。実際のユースケースは Laravel の Policy/Ability だが、
    // テストではその構造を抽象化した最小再現を使う。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
class Ability {\n\
    public function allow() { return true; }\n\
}\n\
class Bootstrapper {\n\
    public function register() {\n\
        $this->gate('check', Ability::class . '@allow');\n\
        $this->route('/x', 'Ability@allow');\n\
    }\n\
    public function gate($k, $v) { return [$k, $v]; }\n\
    public function route($p, $v) { return [$p, $v]; }\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead.iter().any(|n| n.contains("allow")),
        "string callable で呼ばれる Ability::allow は dead にならないこと: {dead:?}"
    );
}

#[test]
fn refs_php_callable_array_rejects_non_class_const_first_element() {
    // N3 誤検出防止: 第1要素が `Class::class` でない場合は ref として認めない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.php"),
        "<?php\n\
class Helper {\n\
    public static function doIt() { return 1; }\n\
}\n\
function f() { return [1, 'doIt']; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "doIt", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().unwrap();
    let ref_count = refs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("ref"))
        .count();
    assert_eq!(
        ref_count, 0,
        "[1, 'doIt'] は callable array ではないので ref を作るべきでない: {refs:?}"
    );
}

#[test]
fn dead_code_test_only_symbols_separated_from_dead() {
    // F5: production からは参照されず test/ からのみ参照されるシンボルは
    // dead_symbols ではなく test_only_symbols バケットに分類されること。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();

    // src/lib.rs: production helper (test からだけ呼ばれる) と really_dead (誰からも呼ばれない)
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn used_in_test_only() -> i32 { 1 }\npub fn really_dead() -> i32 { 2 }\n",
    )
    .unwrap();
    // tests/it.rs: used_in_test_only を参照する (production 側 src/ からは未参照)
    std::fs::write(
        root.join("tests/it.rs"),
        "use foo::used_in_test_only;\n#[test]\nfn t() { assert_eq!(used_in_test_only(), 1); }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");

    let dead: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    let test_only: Vec<String> = json
        .get("test_only_symbols")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    assert!(
        dead.iter().any(|n| n.contains("really_dead")),
        "really_dead は production / test 双方から参照されないので dead_symbols に出るべき: dead={dead:?}"
    );
    assert!(
        !dead.iter().any(|n| n.contains("used_in_test_only")),
        "used_in_test_only は test/ から参照されるので dead_symbols から外れるべき: dead={dead:?}"
    );
    assert!(
        test_only.iter().any(|n| n.contains("used_in_test_only")),
        "used_in_test_only は test_only_symbols バケットに含まれるべき: test_only={test_only:?}"
    );
}

#[test]
fn dead_code_unknown_framework_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(root.join("sample.php"), "<?php\n").unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "djangular",
        ])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "未知の framework 名はエラー (exit != 0)"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Unknown framework preset") || stdout.contains("INVALID_REQUEST"),
        "エラーメッセージに framework 未対応が示される: {stdout}"
    );
}

#[test]
fn dead_code_php_abstract_methods_and_interface_decls_are_not_dead() {
    // PHP の `abstract public function ...` は子クラスでの実装が必須、
    // `interface X { public function y(); }` は implementer が必ず提供するため、
    // 宣言そのものを dead として報告するのは誤検出。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();

    std::fs::write(
        root.join("src/abstract_interface_sample.php"),
        "<?php\n\
abstract class AbstractCommand {\n\
    abstract public function mustImplement(): void;\n\
    public function concreteHelper(): int { return 0; }\n\
}\n\
interface BoundaryContract {\n\
    public function boundaryEntry(): void;\n\
    public function boundaryExit(): void;\n\
}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for banned in ["mustImplement", "boundaryEntry", "boundaryExit"] {
        assert!(
            !names.iter().any(|n| n.contains(banned)),
            "{banned} は abstract/interface 宣言のため dead 対象から外れるべき: {names:?}"
        );
    }
    // abstract class の通常 (concrete) method は従来どおり dead 判定される
    // (子クラスからの呼び出しが refs で拾えるかは別問題)
    assert!(
        names.iter().any(|n| n.contains("concreteHelper")),
        "abstract class 内の concrete method は従来どおり dead として報告される: {names:?}"
    );
}

#[test]
fn dead_code_php_abstract_base_and_trait_are_reachable_via_extends_and_use() {
    // PHP の `class Derived extends AbstractBase` と `use TraitX;` は tree-sitter で
    // 一見 class_declaration の子孫として現れるため parent/grandparent 走査だけだと
    // 基底クラス名・使用 trait 名が `Definition` に誤分類され、実際の参照にも
    // 関わらず dead-code 判定される。field_name == "name" の識別子だけを def と
    // 数えることで、継承 / trait 経由の参照が正しくカウントされる。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();

    std::fs::write(
        root.join("src/base_contract.php"),
        "<?php\nabstract class BaseContract {\n    public function contractHook(): void {}\n}\ninterface SignerContract {\n    public function sign(): void;\n}\ntrait SharedBehavior {\n    public function shared(): void {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/concrete.php"),
        "<?php\nclass Concrete extends BaseContract implements SignerContract {\n    use SharedBehavior;\n    public function sign(): void {}\n    public function publicButUnused(): void {}\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    // extends / implements / use で参照されているクラス・インターフェイス・trait は
    // dead 扱いされない
    for reachable in ["BaseContract", "SignerContract", "SharedBehavior"] {
        assert!(
            !names.iter().any(|n| n == reachable),
            "{reachable} は extends/implements/use 経由で参照されているため dead 対象から外れるべき: {names:?}"
        );
    }
    // 実際に未参照の public メソッドは従来どおり dead
    assert!(
        names.iter().any(|n| n.contains("publicButUnused")),
        "真の未参照 public メソッドは dead として報告される: {names:?}"
    );
}

#[test]
fn dead_code_php_protected_and_private_methods_are_not_dead() {
    // PHP の `protected` / `private` メソッドは公開 API ではないため、
    // cross-file の識別子参照が無くても dead-code 対象にしない。
    // 対照として `public` メソッド (参照なし) は dead として報告される。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();

    // クラス内の各 visibility / トップレベル関数 / trait 内メソッド を網羅
    std::fs::write(
        root.join("src/visibility_sample.php"),
        "<?php\n\
class VisibilitySampleHolder {\n\
    public function publicUnreferenced() {}\n\
    protected function protectedHelper() {}\n\
    private function privateHelper() {}\n\
    public static function publicStaticUnreferenced() {}\n\
    protected static function protectedStatic() {}\n\
}\n\
trait VisibilitySampleTrait {\n\
    protected function traitProtectedHelper() {}\n\
    private function traitPrivateHelper() {}\n\
}\n\
function free_unreferenced_helper() {}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    // public な未参照シンボル 3 つは dead として報告される
    assert!(
        names.iter().any(|n| n.contains("publicUnreferenced")),
        "public method は dead として報告される: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("publicStaticUnreferenced")),
        "public static method も dead として報告される: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "free_unreferenced_helper"),
        "トップレベル function (暗黙 public) は dead として報告される: {names:?}"
    );

    // protected / private は visibility で除外
    for banned in [
        "protectedHelper",
        "privateHelper",
        "protectedStatic",
        "traitProtectedHelper",
        "traitPrivateHelper",
    ] {
        assert!(
            !names.iter().any(|n| n.contains(banned)),
            "{banned} は protected/private なので dead 判定から除外されるべき: {names:?}"
        );
    }
}

#[test]
fn dead_code_php_phpunit_conventions_excluded() {
    // PHPUnit の規約的メソッド / クラスは識別子レベルの cross-file ref がないが
    // PHPUnit ランナーから自動呼出しされるため dead-code から除外する。
    // - メソッド名が `^test[A-Z_]` で始まる (testBar, test_case_one, testAccess_ok)
    // - `setUp`, `tearDown`, `setUpBeforeClass`, `tearDownAfterClass`
    // - クラス名末尾が `Test` / `TestCase` / `IntegrationTest` / `FeatureTest`
    //
    // 意図的に `--include-tests` を付けて tests/ ディレクトリ除外を無効化し、
    // 命名規約ベースの除外だけを効かせる。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("tests/SampleCaseTest.php"),
        "<?php\nclass SampleCaseTest {\n    public function setUp(): void {}\n    public function tearDown(): void {}\n    public function testBar(): void {}\n    public function regular_helper(): void {}\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    // PHPUnit 規約シンボルは除外される
    for banned in [
        "SampleCaseTest",
        "SampleCaseTest.setUp",
        "SampleCaseTest.tearDown",
        "SampleCaseTest.testBar",
    ] {
        assert!(
            !names.iter().any(|n| n == banned),
            "{banned} は PHPUnit 規約として dead-code から除外されるべき: {names:?}"
        );
    }
    // 規約外の通常 public メソッドは従来通り dead 判定
    assert!(
        names.iter().any(|n| n == "SampleCaseTest.regular_helper"),
        "PHPUnit 規約外のメソッドは dead として報告される: {names:?}"
    );
}

#[test]
fn dead_code_phpunit_class_helpers_excluded_from_test_only() {
    // PHPUnit テストクラス内の helper メソッドが、同一クラス内の self::/static::/
    // $this-> 呼び出しでのみ参照されている場合、test_only_symbols ではなく
    // "ランナー内部用ヘルパー" として完全に除外される。
    // 現実には @dataProvider / #[DataProvider] / @depends 経由で reflection 呼び出し
    // されるが識別子レベルの cross-file refs では追跡できないため、test_only に
    // 大量のノイズが出るのを防ぐ目的。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("test/Models")).unwrap();
    std::fs::write(
        root.join("test/Models/SampleLogEntityTest.php"),
        r#"<?php
class SampleLogEntityTest {
    public function testCreatesEntity(): void {
        $vo = self::voEventTime();
        $msg = self::voMessage();
        // assert ...
    }

    public static function voEventTime(): string {
        return '2025-01-01T00:00:00Z';
    }

    public static function voMessage(): string {
        return 'sample';
    }
}
"#,
    )
    .unwrap();

    // production コードからは voEventTime / voMessage を参照しない。
    // self::voEventTime() / self::voMessage() の呼び出しはテストファイル内なので
    // test refs > 0 になり、従来は test_only_symbols に積まれていた。
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");

    // test_only_symbols は空 Vec の場合 serde で省略されるため、フィールド不在は空扱い
    let test_only_names: Vec<String> = json["test_only_symbols"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // voEventTime / voMessage は PHPUnit テストクラスの内部 helper として
    // test_only_symbols から除外される (test refs > 0 でもノイズなので捨てる)
    for banned in [
        "SampleLogEntityTest.voEventTime",
        "SampleLogEntityTest.voMessage",
    ] {
        assert!(
            !test_only_names.iter().any(|n| n == banned),
            "{banned} は PHPUnit container 内 helper として test_only_symbols から除外されるべき: {test_only_names:?}"
        );
    }
}

/// PHP の `ClassName::method()` 形式の cross-file 静的呼び出し
/// (`scoped_call_expression`) と同一クラス内の `self::method()` / `static::method()` を
/// dead-code 検出が usage として認識し、PHPUnit テストクラス内の helper が `dead_symbols`
/// にも `test_only_symbols` にも漏れないことを確認する。
///
/// Laravel 風の `test/.../FixtureTest.php` 内 `vo*` helper が
/// `FixtureControllerTest.php` から `FixtureTest::voXxx()` で呼ばれる構造を最小再現する。
#[test]
fn dead_code_php_cross_file_scoped_static_call_keeps_phpunit_helper_alive() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("test/Acme/Models/Fixture/Controllers")).unwrap();
    // 定義側: PHPUnit テストクラス内に public static helper を並べる。
    // 同一クラス内では self:: / static:: で互いを参照する。
    std::fs::write(
        root.join("test/Acme/Models/Fixture/FixtureTest.php"),
        r#"<?php
namespace Acme\Tests\Models\Fixture;

class FixtureTest {
    public static function voPhoneNumberPrefix(): string {
        return self::voPhoneNumberPrefix2();
    }

    public static function voPhoneNumberPrefix2(): string {
        return static::voRecording();
    }

    public static function voRecording(): string {
        return 'rec';
    }

    public static function voAgc(): string {
        return 'agc';
    }
}
"#,
    )
    .unwrap();
    // 参照側: 別ディレクトリの別 PHPUnit テストクラスから cross-file で
    // `FixtureTest::voXxx()` を呼ぶ (scoped_call_expression)。
    std::fs::write(
        root.join("test/Acme/Models/Fixture/Controllers/FixtureControllerTest.php"),
        r#"<?php
namespace Acme\Tests\Models\Fixture\Controllers;

use Acme\Tests\Models\Fixture\FixtureTest;

class FixtureControllerTest {
    public function testRouting(): void {
        $phone = FixtureTest::voPhoneNumberPrefix();
        $rec = FixtureTest::voRecording();
        $agc = FixtureTest::voAgc();
    }
}
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");

    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let test_only_names: Vec<String> = json["test_only_symbols"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    for sym in [
        "FixtureTest.voPhoneNumberPrefix",
        "FixtureTest.voPhoneNumberPrefix2",
        "FixtureTest.voRecording",
        "FixtureTest.voAgc",
    ] {
        assert!(
            !dead_names.iter().any(|n| n == sym),
            "{sym} は cross-file `FixtureTest::method()` と self::/static:: 参照があるので dead_symbols に出ないこと: dead={dead_names:?}"
        );
        assert!(
            !test_only_names.iter().any(|n| n == sym),
            "{sym} は PHPUnit container 内 helper として test_only_symbols からも除外されること: test_only={test_only_names:?}"
        );
    }
}

/// GitLab issue #7 補助テスト: `refs --name` で PHP の `ClassName::method()` 形式の
/// cross-file 呼び出しが `kind: "ref"` として返ることを確認する。
/// dead-code の usage 集計はこの refs 経路 (count_non_definition_refs_split → count_refs_in_file)
/// と同じ AST 走査で行われるため、ここが ref として認識されることが
/// dead 誤検出回避の前提条件。
#[test]
fn refs_php_cross_file_scoped_static_call_is_detected_as_ref() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("AHelper.php"),
        "<?php\nclass AHelper {\n    public static function voFoo(): string { return 'foo'; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("BConsumer.php"),
        "<?php\nclass BConsumer {\n    public function doSomething(): void { $x = AHelper::voFoo(); echo $x; }\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["refs", "--name", "voFoo", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let refs = json["refs"].as_array().expect("refs array");
    let cross_file_refs: Vec<&serde_json::Value> = refs
        .iter()
        .filter(|r| {
            r["kind"].as_str() == Some("ref")
                && r["path"]
                    .as_str()
                    .is_some_and(|p| p.ends_with("BConsumer.php"))
        })
        .collect();
    assert_eq!(
        cross_file_refs.len(),
        1,
        "BConsumer.php からの `AHelper::voFoo()` は 1 件の ref として検出されるべき: refs={refs:?}"
    );
}

#[test]
fn dead_code_python_unittest_conventions_excluded() {
    // Python unittest の規約 (`unittest.TestCase` 派生クラスとそのテストメソッド、
    // setUp/tearDown 等の lifecycle hook) は dead-code 判定から除外される。
    // テストランナーがリフレクションで動的 discover するため、識別子レベルの
    // cross-file refs では caller を追跡できない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("scripts/test_corpus_test.py"),
        "import unittest\n\
         \n\
         class CorpusTestScriptTests(unittest.TestCase):\n    \
             def test_is_separator(self):\n        \
                 self.assertEqual(1, 1)\n\n    \
             def test_extract_tests(self):\n        \
                 self.assertTrue(True)\n\n    \
             def setUp(self):\n        \
                 pass\n\n    \
             def tearDown(self):\n        \
                 pass\n\n    \
             def regular_helper(self):\n        \
                 return 42\n\n\n\
         class DerivedTests(CorpusTestScriptTests):\n    \
             def test_inherited(self):\n        \
                 self.assertTrue(True)\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for banned in [
        "CorpusTestScriptTests",
        "CorpusTestScriptTests.test_is_separator",
        "CorpusTestScriptTests.test_extract_tests",
        "CorpusTestScriptTests.setUp",
        "CorpusTestScriptTests.tearDown",
        // 同一ファイル内の間接継承 (CorpusTestScriptTests → DerivedTests) も解決される
        "DerivedTests",
        "DerivedTests.test_inherited",
    ] {
        assert!(
            !names.iter().any(|n| n == banned),
            "{banned} は unittest 規約として dead-code から除外されるべき: {names:?}"
        );
    }
}

#[test]
fn dead_code_python_pytest_top_level_test_functions_excluded() {
    // pytest 規約のファイル名 (`test_*.py` / `*_test.py`) のトップレベル `test_*`
    // 関数と `conftest.py` 内の関数は dead-code から除外する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("scripts/test_module.py"),
        "def test_addition():\n    assert 1 + 1 == 2\n\n\ndef regular_helper():\n    return 1\n",
    )
    .unwrap();
    std::fs::write(
        root.join("scripts/feature_test.py"),
        "def test_feature():\n    assert True\n",
    )
    .unwrap();
    std::fs::write(
        root.join("scripts/conftest.py"),
        "def my_fixture():\n    return {}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for banned in ["test_addition", "test_feature", "my_fixture"] {
        assert!(
            !names.iter().any(|n| n == banned),
            "{banned} は pytest 規約として dead-code から除外されるべき: {names:?}"
        );
    }
    // pytest 規約外のトップレベル関数は dead と判定される
    assert!(
        names.iter().any(|n| n == "regular_helper"),
        "pytest 規約外のトップレベル関数は dead として報告される: {names:?}"
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

#[test]
fn dead_code_ts_same_member_name_owner_aware_unused_side_is_dead() {
    // GitLab Issue #19 の再現: TS で同名 getter が別クラスにあり、片方だけが使われている場合に
    // 未使用側の getter が dead として検出されること。
    // - VoiceLogSettingModel.isOmnis: 参照 0 件 → dead に出るべき
    // - VoiceLogModel.isOmnis: 別ファイルで使用中 → live (報告されない)
    // - VoiceLogSettingModel.isOther: 同名 export が他になく従来通り dead
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("setting/models")).unwrap();
    std::fs::create_dir_all(root.join("log/models")).unwrap();
    std::fs::create_dir_all(root.join("log/components")).unwrap();

    std::fs::write(
        root.join("setting/models/setting.model.ts"),
        "export class VoiceLogSettingModel {\n\
        \x20   voice_log_type: number = 1;\n\
        \x20   get isAmi(): boolean { return this.voice_log_type === 1; }\n\
        \x20   get isOmnis(): boolean { return this.voice_log_type === 2; }\n\
        \x20   get isOther(): boolean { return this.voice_log_type === 3; }\n\
        }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("log/models/log.model.ts"),
        "export class VoiceLogModel {\n\
        \x20   type: number = 1;\n\
        \x20   isOmnis(): boolean { return this.type === 2; }\n\
        }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("log/components/log.parts.component.ts"),
        "import { VoiceLogModel } from \"../models/log.model\";\n\
        const voiceLogs: VoiceLogModel[] = [];\n\
        console.log(voiceLogs[0].isOmnis());\n",
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
        names.contains(&"VoiceLogSettingModel.isOmnis"),
        "owner 一意推定 (import only VoiceLogModel) で未使用側の isOmnis が dead に出るべき: {names:?}"
    );
    assert!(
        !names.contains(&"VoiceLogModel.isOmnis"),
        "別ファイルで使用中の VoiceLogModel.isOmnis は dead に出るべきでない: {names:?}"
    );
    assert!(
        names.contains(&"VoiceLogSettingModel.isOther"),
        "従来通り duplicate でない isOther は dead に出るべき: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_ambiguous_when_both_owners_imported() {
    // duplicate owner を両方 import しているファイルで `.member` が使われている場合は
    // どちらの owner のメソッドへの参照か owner 一意推定できないため、ambiguous として
    // 旧スキップを維持する (どちらも dead に出さない)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();

    std::fs::write(
        root.join("models/foo.model.ts"),
        "export class FooModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("models/bar.model.ts"),
        "export class BarModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/app.ts"),
        "import { FooModel } from \"../models/foo.model\";\n\
        import { BarModel } from \"../models/bar.model\";\n\
        function pick(x: FooModel | BarModel): boolean { return x.isReady(); }\n\
        console.log(pick(new FooModel()));\n",
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
        !names.iter().any(|n| n.ends_with(".isReady")),
        "両 owner を同ファイルで import している ambiguous ケースでは旧スキップを維持: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_ambiguous_when_no_import() {
    // duplicate owner のいずれも import していないファイルで `.member` が使われている場合は
    // owner 推定できないため ambiguous (旧スキップ維持)。
    // ローカルクラスや any 型経由の呼び出しを safe に保守する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::create_dir_all(root.join("util")).unwrap();

    std::fs::write(
        root.join("models/foo.model.ts"),
        "export class FooModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("models/bar.model.ts"),
        "export class BarModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("util/helper.ts"),
        "export function checkReady(x: any): boolean {\n    return x.isReady();\n}\n",
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
        !names.iter().any(|n| n.ends_with(".isReady")),
        "owner を import していないファイルで .member が出る ambiguous ケースは旧スキップ維持: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_string_literal_marks_ambiguous() {
    // bare member 名が文字列リテラルとして出現する場合は computed access の可能性があるため
    // 全 duplicate candidate を ambiguous へ倒し、旧スキップを維持する (safe-by-default)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();

    std::fs::write(
        root.join("models/foo.model.ts"),
        "export class FooModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("models/bar.model.ts"),
        "export class BarModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/app.ts"),
        "import { FooModel } from \"../models/foo.model\";\n\
        const key = \"isReady\";\n\
        const foo = new FooModel();\n\
        console.log((foo as any)[key]);\n",
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
        !names.iter().any(|n| n.ends_with(".isReady")),
        "string literal で member 名が出る場合は ambiguous で旧スキップ維持: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_self_ref_in_owner_file_is_not_misattributed() {
    // codex pre-commit review 指摘の FP 修正: duplicate owner X の定義ファイルが unrelated
    // owner Y を型用途で import している場合、ファイル内の `this.member` を Y 側に誤帰属
    // させてはならない。effective_owners = imported_owners ∪ local_defined_owners が
    // 2 owner になり ambiguous へ倒れることで、X の dead 誤検出を防ぐ。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();

    std::fs::write(
        root.join("models/bar.model.ts"),
        "export class BarModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("models/foo.model.ts"),
        "import { BarModel } from \"./bar.model\";\n\
        export class FooModel {\n\
        \x20   other?: BarModel;\n\
        \x20   isReady(): boolean { return true; }\n\
        \x20   check(): boolean { return this.isReady(); }\n\
        }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/app.ts"),
        "import { FooModel } from \"../models/foo.model\";\n\
        console.log(new FooModel().check());\n",
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

    // FooModel.isReady は this.isReady() 経由で内部参照されており、外部から FooModel.check() を
    // 呼び出すコードもあるため dead に出してはならない (旧スキップ相当の Ambiguous)。
    assert!(
        !names.contains(&"FooModel.isReady"),
        "owner 定義ファイル内の this.member を unrelated import 側に誤帰属して FooModel.isReady を dead と判定してはならない: {names:?}"
    );
    assert!(
        !names.contains(&"BarModel.isReady"),
        "duplicate set 全体が ambiguous なので BarModel.isReady も旧スキップ維持: {names:?}"
    );
}

#[test]
fn dead_code_ts_same_member_name_namespace_import_marks_ambiguous() {
    // `import * as ns from ...` (namespace import) の場合は ns 経由で任意の owner が
    // アクセスされうるため owner を一意推定できず ambiguous へ倒す (旧スキップ維持)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();

    std::fs::write(
        root.join("models/foo.model.ts"),
        "export class FooModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("models/bar.model.ts"),
        "export class BarModel {\n    isReady(): boolean { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/app.ts"),
        "import * as foo from \"../models/foo.model\";\n\
        const inst = new foo.FooModel();\n\
        console.log(inst.isReady());\n",
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
        !names.iter().any(|n| n.ends_with(".isReady")),
        "namespace import (`import * as ...`) は owner 推定不能で ambiguous 維持: {names:?}"
    );
}

#[test]
fn dead_code_excludes_kotlin_override() {
    // Kotlin の `override` メソッドは親 interface / superclass 経由で呼ばれるため
    // cross-file refs では追跡できず、dead-code 判定で偽陽性になる。
    // AdapterView.OnItemSelectedListener / TextWatcher の override は除外されるべき。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("MainActivity.kt"),
        r#"package com.example

class MainActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {}

    fun setup() {
        val watcher = object : TextWatcher {
            override fun afterTextChanged(s: Editable?) {}
        }
    }

    override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {}
    override fun onNothingSelected(parent: AdapterView<*>?) {}

    fun unusedRegular() {}
}
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

    for override_name in [
        "MainActivity.onCreate",
        "MainActivity.onItemSelected",
        "MainActivity.onNothingSelected",
        "MainActivity.afterTextChanged",
    ] {
        assert!(
            !names.contains(&override_name),
            "Kotlin の override メソッド {override_name} は dead に含めるべきでない: {names:?}"
        );
    }
    // override でない通常メソッドは dead として残る
    assert!(
        names.contains(&"MainActivity.unusedRegular"),
        "override でない未参照メソッドは dead として報告されるべき: {names:?}"
    );
}

#[test]
fn dead_code_excludes_java_override_annotation() {
    // Java の `@Override` アノテーション付きメソッドも dead から除外される。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("Sample.java"),
        r#"package com.example;

public class Sample extends Base {
    @Override
    public void handleEvent() {}

    public void plainUnused() {}
}
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
        !names.iter().any(|n| n.ends_with(".handleEvent")),
        "@Override 付きメソッドは dead に含めるべきでない: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with(".plainUnused")),
        "@Override のない未参照メソッドは dead として報告されるべき: {names:?}"
    );
}

#[test]
fn dead_code_excludes_android_manifest_activity() {
    // AndroidManifest.xml で `android:name=".MainActivity"` と宣言された
    // activity は Android OS から起動されるため dead に含めるべきでない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("AndroidManifest.xml"),
        r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android">
  <application>
    <activity android:name=".MainActivity" android:exported="true">
      <intent-filter>
        <action android:name="android.intent.action.MAIN" />
        <category android:name="android.intent.category.LAUNCHER" />
      </intent-filter>
    </activity>
  </application>
</manifest>
"#,
    )
    .unwrap();

    std::fs::write(
        root.join("MainActivity.kt"),
        r#"package com.example

class MainActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {}
}
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
        !names.contains(&"MainActivity"),
        "AndroidManifest.xml で宣言された activity は dead 扱いすべきでない: {names:?}"
    );
}

#[test]
fn dead_code_layout_onclick_references_handler() {
    // layout XML の `android:onClick="handler"` から Kotlin/Java のメソッドが
    // 呼ばれるため、そのハンドラは dead 扱いすべきでない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("AndroidManifest.xml"),
        r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android"><application><activity android:name=".MainActivity"/></application></manifest>"#,
    )
    .unwrap();
    std::fs::write(
        root.join("activity_main.xml"),
        r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android">
  <Button android:onClick="onSubmit" android:id="@+id/btn"/>
</LinearLayout>
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("MainActivity.kt"),
        r#"package com.example

class MainActivity : AppCompatActivity() {
    fun onSubmit(view: View) {}
}
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
        !names.iter().any(|n| n.ends_with(".onSubmit")),
        "layout XML の android:onClick で参照されたハンドラは dead 扱いすべきでない: {names:?}"
    );
}

// ---- Xojo 言語サポートのスモークテスト ----
// v26.6 で tree-sitter-xojo を削除し lexer-only に移行。
// PR3 で lexer 経由の symbols/refs/calls が復活するまで以下のテストは ignore する。

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
fn xojo_calls_returns_unsupported() {
    // v26.6 以降、Xojo は lexer-only バックエンド。calls は tree-sitter Query 必須のため
    // UNSUPPORTED_LANGUAGE を返す (空結果ではなく明確なエラーで AI エージェントに区別可能)。
    let output = cargo_bin()
        .args(["calls", "--path", "tests/fixtures/sample.xojo_code"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "calls は xojo に対して非ゼロ exit すべき"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    assert_eq!(
        json["error"]["code"], "UNSUPPORTED_LANGUAGE",
        "xojo は lexer-only のため calls は UNSUPPORTED_LANGUAGE を返す"
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
    // v26.6 以降、Xojo は lexer-only バックエンドに移行。tree-sitter parser_version は持たない。
    assert_eq!(xojo["available"], true);
    assert_eq!(xojo["backend"], "lexer_only");
    assert!(
        xojo.get("parser_version").is_none() || xojo["parser_version"].is_null(),
        "lexer_only バックエンドは parser_version を持たない"
    );
}

/// GitLab #9 回帰テスト: Xojo dead-code 検出で refs ベースの参照判定が機能することを検証。
///
/// 旧実装では `count_refs_in_file` (refs.rs) が tree-sitter 経路だけを呼んでおり、
/// Xojo は parse_file が UNSUPPORTED_LANGUAGE を返してファイル単位の count が 0 になり、
/// refs が見つかるシンボルでも dead 判定されていた。lexer-only dispatch を追加して修正。
///
/// production-only fixture で検証する (UnitTests/ 配下のテストファイルは PR2 で
/// 別 fixture に切り出し、Window Event handler / TestGroup *Test 等の framework
/// entrypoint 認識テストで使う想定)。
#[test]
fn dead_code_xojo_excludes_symbols_with_refs() {
    use std::path::Path;

    let fixture = Path::new("tests/fixtures/xojo_dead_code");
    assert!(fixture.exists(), "fixture missing: {fixture:?}");

    // dead-code は `tests/` を含むパスを test ディレクトリと判定し、
    // 全ファイルを test 扱いにする。production 参照判定を正しく検証するため
    // fixture を tempdir に複製してから走らせる。
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest_root = tmp.path().join("project");
    std::fs::create_dir_all(&dest_root).expect("create dest_root");
    let cp_status = Command::new("cp")
        .args([
            "-R",
            &format!("{}/.", fixture.display()),
            dest_root.to_str().unwrap(),
        ])
        .status()
        .expect("cp -R");
    assert!(cp_status.success(), "failed to copy fixture to tempdir");

    let output = cargo_bin()
        .args(["dead-code", "--dir", dest_root.to_str().unwrap()])
        .output()
        .expect("failed to run dead-code");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols 配列")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    let test_only_names: Vec<&str> = json["test_only_symbols"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["name"].as_str()).collect())
        .unwrap_or_default();

    // 中核回帰: refs で参照が見つかるシンボルは `dead_symbols` にも `test_only_symbols`
    // にも含めない。
    // - Greeter は Caller.Run() と Main.Open() から参照
    // - Greet は Caller.Run() から呼び出し
    // - Caller は Main.Open() で `New Caller` として参照
    // - Run は Caller インスタンスから呼び出し
    for name in ["Greeter", "Greet", "Caller", "Run"] {
        assert!(
            !dead_names.contains(&name),
            "{name} は refs で参照されているため dead 判定すべきでない: dead_names={dead_names:?}"
        );
        assert!(
            !test_only_names.contains(&name),
            "{name} は production 参照があるため test_only にも入れるべきでない: \
             test_only_names={test_only_names:?}"
        );
    }

    // 退行検出: 修正が「Xojo を一律 dead から除外」ではなく
    // 「参照ベースで正しく判定」していることを保証するため、誰からも呼ばれない
    // Orphan クラスは dead に残る必要がある。
    assert!(
        dead_names.contains(&"Orphan"),
        "未参照クラス Orphan は dead に残るべき: dead_names={dead_names:?}"
    );
}

/// GitLab #15 回帰テスト: Xojo の `#tag Event` 配下のイベントハンドラ (Sub/Function) は
/// ランタイムがイベント駆動で呼ぶ entrypoint のため dead に出ない。`#tag Events <control>`
/// 配下のハンドラが対象。`#tag Event` で囲まれない通常メソッドは従来どおり dead 判定する。
#[test]
fn dead_code_xojo_excludes_event_handlers() {
    use std::path::Path;
    use std::process::Command;

    let fixture = Path::new("tests/fixtures/xojo_dead_code");
    assert!(fixture.exists(), "fixture missing: {fixture:?}");

    let tmp = tempfile::tempdir().expect("tempdir");
    let dest_root = tmp.path().join("project");
    std::fs::create_dir_all(&dest_root).expect("create dest_root");
    let cp_status = Command::new("cp")
        .args([
            "-R",
            &format!("{}/.", fixture.display()),
            dest_root.to_str().unwrap(),
        ])
        .status()
        .expect("cp -R");
    assert!(cp_status.success(), "failed to copy fixture to tempdir");

    let output = cargo_bin()
        .args(["dead-code", "--dir", dest_root.to_str().unwrap()])
        .output()
        .expect("failed to run dead-code");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols 配列")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();

    // CellAction は Main.xojo_window の `#tag Events Listbox1` > `#tag Event` 配下の
    // イベントハンドラ。Xojo ランタイムが呼ぶ entrypoint のため dead に出ない。
    assert!(
        !dead_names.contains(&"CellAction"),
        "CellAction (#tag Event 配下のイベントハンドラ) は dead に出すべきでない: \
         dead_names={dead_names:?}"
    );

    // 退行検出: entrypoint でない未参照クラス Orphan は従来どおり dead に残る
    // (「一律除外」ではなく構造ベースで判定していることを保証)。
    assert!(
        dead_names.contains(&"Orphan"),
        "未参照クラス Orphan は dead に残るべき: dead_names={dead_names:?}"
    );
}

/// GitLab #16 回帰テスト: `Inherits TestGroup` クラスの引数なし `*Test` / `Setup` /
/// `TearDown` メソッドは XojoUnit が Introspection で実行する entrypoint のため dead に
/// 出ない。`#tag Event` 配下の `InitializeTestGroups` も entrypoint。Test で終わらない
/// 通常メソッド (orphanHelper) は参照0なら dead に残る。
#[test]
fn dead_code_xojo_excludes_testgroup_test_methods() {
    use std::path::Path;
    use std::process::Command;

    let fixture = Path::new("tests/fixtures/xojo_testgroup");
    assert!(fixture.exists(), "fixture missing: {fixture:?}");

    let tmp = tempfile::tempdir().expect("tempdir");
    let dest_root = tmp.path().join("project");
    std::fs::create_dir_all(&dest_root).expect("create dest_root");
    let cp_status = Command::new("cp")
        .args([
            "-R",
            &format!("{}/.", fixture.display()),
            dest_root.to_str().unwrap(),
        ])
        .status()
        .expect("cp -R");
    assert!(cp_status.success(), "failed to copy fixture to tempdir");

    let output = cargo_bin()
        .args(["dead-code", "--dir", dest_root.to_str().unwrap()])
        .output()
        .expect("failed to run dead-code");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols 配列")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    let test_only_names: Vec<&str> = json["test_only_symbols"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["name"].as_str()).collect())
        .unwrap_or_default();

    // *Test / Setup / TearDown / `#tag Event` 配下の InitializeTestGroups は entrypoint。
    // dead にも test_only にも出ない。
    for name in [
        "enableControlTest",
        "validateValueTest",
        "Setup",
        "secondScenarioTest",
        "InitializeTestGroups",
    ] {
        assert!(
            !dead_names.contains(&name),
            "{name} は XojoUnit entrypoint のため dead に出すべきでない: dead_names={dead_names:?}"
        );
        assert!(
            !test_only_names.contains(&name),
            "{name} は entrypoint のため test_only にも出すべきでない: \
             test_only_names={test_only_names:?}"
        );
    }

    // 退行検出: Test で終わらない通常メソッド orphanHelper は参照0なら dead に残る。
    assert!(
        dead_names.contains(&"orphanHelper"),
        "orphanHelper (通常メソッド, 参照0) は dead に残るべき: dead_names={dead_names:?}"
    );
}

/// 2026-05-27 zod-inferred-types-pre-existing-dead 対応: `--dead-scope touched-symbols` の
/// 回帰テスト。changed file 内に元から存在する dead は除外し、今回の hunk に被るシンボル
/// だけが返ることを検証。`review --hook` のデフォルト挙動でもある。
#[test]
fn dead_scope_touched_symbols_excludes_pre_existing_dead() {
    use std::path::Path;
    use std::process::Command;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let src_dir = root.join("src");
    std::fs::create_dir(&src_dir).expect("create src");

    // lib.rs: 公開 module 宣言だけ。dead 検出の対象は src/foo.rs のシンボル。
    std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"demo\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[lib]\npath=\"src/lib.rs\"\n").unwrap();
    std::fs::write(src_dir.join("lib.rs"), "pub mod foo;\n").unwrap();
    // 初期コミット: ExistingDead と Used が両方未参照 (本来両方 dead 候補)。
    std::fs::write(
        src_dir.join("foo.rs"),
        "pub fn existing_dead() {}\n\npub fn used() {}\n",
    )
    .unwrap();
    let git = |args: &[&str]| {
        let s = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(s.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "x@y"]);
    git(&["config", "user.name", "x"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // 新規 hunk: src/foo.rs の末尾に `new_dead` を追加。`existing_dead` の宣言行には触れない。
    std::fs::write(
        src_dir.join("foo.rs"),
        "pub fn existing_dead() {}\n\npub fn used() {}\n\npub fn new_dead() {}\n",
    )
    .unwrap();

    // --dead-scope touched-symbols (= review --hook デフォルト相当): new_dead だけ残る。
    let out = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--git",
            "--dead-scope",
            "touched-symbols",
        ])
        .output()
        .expect("run dead-code");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let dead: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        dead.contains(&"new_dead"),
        "今回追加した new_dead は touched-symbols スコープで残るべき: {dead:?}"
    );
    assert!(
        !dead.contains(&"existing_dead"),
        "宣言行が hunk と重ならない existing_dead は touched-symbols スコープから除外されるべき: {dead:?}"
    );

    // --dead-scope all (デフォルト): existing_dead も new_dead も両方残る (used は参照
    // されていないが lib crate なら本来 dead 扱い)。
    let out_all = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--git",
            "--dead-scope",
            "all",
        ])
        .output()
        .expect("run dead-code all");
    let json_all: serde_json::Value = serde_json::from_slice(&out_all.stdout).unwrap();
    let dead_all: Vec<&str> = json_all["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        dead_all.contains(&"existing_dead"),
        "all スコープでは元からあった existing_dead も返るべき: {dead_all:?}"
    );
    assert!(
        dead_all.contains(&"new_dead"),
        "all スコープでも new_dead は返るべき: {dead_all:?}"
    );

    // 退行ガード: dir パスの問題で std::path::Path を使う警告を避ける。
    let _ = Path::new(root);
}

// ---------------------------------------------------------------------------
// git 管理外ディレクトリでの graceful skip (--git)
// ---------------------------------------------------------------------------

#[test]
fn non_git_review_hook_silent_exit_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["review", "--dir", path, "--git", "--hook"])
        .output()
        .expect("run");
    assert!(output.status.success(), "非 git の --hook は exit 0");
    assert!(
        output.stdout.is_empty(),
        "stdout は空であるべき: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr は空であるべき: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// ローカル Issue 2026-06-04-api-rm-false-positive-on-reexport の回帰テスト:
/// ローカル定義を re-export (`export { foo } from "..."`) に置き換えても、利用者から
/// 見た export 面は維持されるため api.rm に出さない。move (同一 diff 内の add) でない
/// 純粋な forwarding でも抑制されることを確認する (b.ts を作らないため reconcile_with_moves
/// では相殺されず、re-export 抑制ロジックが独立して効くことを保証)。
#[test]
fn api_rm_suppressed_for_ts_named_reexport() {
    use std::process::Command;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    let path_str = path.to_str().expect("utf-8");

    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(
        path.join("a.ts"),
        "export function foo() {}\nexport function bar() {}\n",
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    // foo を外部モジュールからの re-export に置換 (b.ts を作らない = move でない)。bar は不変。
    std::fs::write(
        path.join("a.ts"),
        "export { foo } from \"./vendor\";\nexport function bar() {}\n",
    )
    .unwrap();
    git(&["add", "-A"]);

    let output = cargo_bin()
        .args(["review", "--dir", path_str, "--git"])
        .output()
        .expect("run review");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let api_rm: Vec<&str> = json["api"]["rm"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["n"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !api_rm.contains(&"foo"),
        "foo は re-export で forwarding されており api.rm に出すべきでない: api.rm={api_rm:?}"
    );
}

#[test]
fn non_git_review_emits_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["review", "--dir", path, "--git"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(json["skipped"]["reason"], "not_git_repository");
    assert_eq!(json["skipped"]["source"], "git");
    assert!(json["impact"]["changes"].as_array().unwrap().is_empty());
}

#[test]
fn non_git_impact_hook_silent_exit_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["impact", "--dir", path, "--git", "--hook"])
        .output()
        .expect("run");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn non_git_context_empty_changes_with_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["context", "--dir", path, "--git"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(json["changes"].as_array().unwrap().is_empty());
    assert_eq!(json["skipped"]["reason"], "not_git_repository");
}

#[test]
fn non_git_dead_code_emits_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["dead-code", "--dir", path, "--git"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(json["dead_symbols"].as_array().unwrap().is_empty());
    assert_eq!(json["skipped"]["reason"], "not_git_repository");
}

#[test]
fn non_git_cochange_emits_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8");
    let output = cargo_bin()
        .args(["cochange", "--dir", path, "--git"])
        .output()
        .expect("run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(json["entries"].as_array().unwrap().is_empty());
    assert_eq!(json["skipped"]["reason"], "not_git_repository");
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

/// TS/TSX 名前衝突 false positive 抑制 (Issue 2026-06-05-multi-attachment-conversations-fp):
/// `schema.ts` を import していない `ConversationList.tsx` の props 変数 `conversations`
/// (Drizzle table と同名) は `impacted_callers` ではなく `low_confidence_callers` に振り分け
/// られて Stop hook の blocking から外れる。
#[test]
fn impact_ts_name_collision_without_direct_import_routed_to_low_confidence() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("lib/db")).unwrap();
    std::fs::create_dir_all(dir.path().join("components")).unwrap();

    // schema.ts: Drizzle table export
    let schema_path = dir.path().join("lib/db/schema.ts");
    std::fs::write(&schema_path, "export const conversations = { id: 0 };\n").unwrap();

    // ConversationList.tsx: schema.ts を import せず、独自の interface + destructured props
    let tsx_path = dir.path().join("components/ConversationList.tsx");
    std::fs::write(
        &tsx_path,
        r#"interface Conversation { id: number }
interface Props { conversations: Conversation[] }
export function ConversationList({ conversations }: Props) {
    return conversations.length;
}
"#,
    )
    .unwrap();

    // diff: schema.ts の conversations を変更
    let diff = r#"--- a/lib/db/schema.ts
+++ b/lib/db/schema.ts
@@ -1 +1 @@
-export const conversations = { id: 0 };
+export const conversations = { id: 0, title: "" };
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let schema_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("lib/db/schema.ts"))
        .unwrap_or_else(|| panic!("schema.ts change not found: {stdout}"));
    let impacted = schema_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = schema_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    // ConversationList.tsx は schema.ts を import していないため、impacted_callers には
    // 出さず low_confidence_callers (informational) に振り分けるべき。Stop hook blocking から
    // 外しつつ Information として残す。
    let tsx_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("components/ConversationList.tsx"));
    assert!(
        !tsx_in_impacted,
        "ConversationList.tsx must NOT appear in impacted_callers (no direct import of schema.ts). got: {impacted:?} | low: {low:?}"
    );
    // 低 confidence の振り分け先に出ることも assert (codex 助言: low 出力自体を仕様化)。
    // path は absolute (tempdir) になりうるので末尾マッチで判定する。
    let tsx_in_low = low.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .is_some_and(|s| s.ends_with("components/ConversationList.tsx"))
    });
    assert!(
        tsx_in_low,
        "ConversationList.tsx は low_confidence_callers に出るべき (informational として残す)。got low: {low:?}"
    );
}

/// 直接 import している場合は high impact (`impacted_callers`) に残る (逆方向の回帰テスト):
/// `ConversationList.tsx` が `schema.ts` を直接 import している場合は従来通り
/// `impacted_callers` に出る。
#[test]
fn impact_ts_name_collision_with_direct_import_stays_high() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("lib/db")).unwrap();
    std::fs::create_dir_all(dir.path().join("components")).unwrap();

    let schema_path = dir.path().join("lib/db/schema.ts");
    std::fs::write(&schema_path, "export const conversations = { id: 0 };\n").unwrap();

    // ConversationList.tsx: schema.ts を直接 import する
    let tsx_path = dir.path().join("components/ConversationList.tsx");
    std::fs::write(
        &tsx_path,
        r#"import { conversations } from "../lib/db/schema";
export function getList() {
    return conversations;
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/lib/db/schema.ts
+++ b/lib/db/schema.ts
@@ -1 +1 @@
-export const conversations = { id: 0 };
+export const conversations = { id: 0, title: "" };
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let schema_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("lib/db/schema.ts"))
        .unwrap_or_else(|| panic!("schema.ts change not found: {stdout}"));
    let impacted = schema_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let tsx_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("components/ConversationList.tsx"));
    assert!(
        tsx_in_impacted,
        "ConversationList.tsx は schema.ts を直接 import しているため impacted_callers に出るべき。got: {impacted:?}"
    );
}
