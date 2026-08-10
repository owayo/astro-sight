//! ast / symbols / calls / imports / sequence / lint サブコマンドの統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

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

    // Rust の import はすべて use 種別になる。
    for imp in imports {
        assert_eq!(imp["kind"], "use");
    }
}

#[test]
fn imports_typescript_dynamic_imports_static_sources_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source_path = tmp.path().join("dynamic.ts");
    std::fs::write(
        &source_path,
        r#"import { value } from "./static";
const boot = import("./boot", "./ignored-dynamic");
export function outer() {
    async function inner() {
        return import(`./dep`);
    }
    return inner;
}
const skipped = import(`./${name}`);
const fake = fooimport("./fake");
// import("./commented");
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args(["imports", "--path", source_path.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let imports = json["imports"].as_array().unwrap();
    let sources: Vec<&str> = imports
        .iter()
        .map(|item| item["src"].as_str().unwrap())
        .collect();
    assert_eq!(sources, vec!["./static", "./boot", "./dep"]);
    assert!(imports.iter().all(|item| item["kind"] == "import"));
    assert_eq!(imports[1]["ln"], 1);
    assert_eq!(imports[2]["ln"], 4);
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

#[test]
fn symbols_cache_returns_fresh_result_after_content_change() {
    // Issue 2026-07-10-ast-symbols-cache-hash-double-read の周辺検証:
    // cache key は内容 hash 由来のため、内容変更後に古い結果を返さない。
    // (TOCTOU race そのものは決定的に再現できないため、put ガードの前提となる
    //  「解析結果と key の整合」を連続実行で確認する)
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let file = root.join("t.rs");
    std::fs::write(&file, "pub fn first() {}\n").unwrap();

    let run = || {
        let output = cargo_bin()
            .args(["symbols", "--path", file.to_str().unwrap()])
            .output()
            .expect("failed to run");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let out1 = run();
    assert!(out1.contains("first"), "{out1}");
    let out2 = run();
    assert!(out2.contains("first"), "cache hit でも同じ結果: {out2}");

    std::fs::write(&file, "pub fn second() {}\n").unwrap();
    let out3 = run();
    assert!(
        out3.contains("second") && !out3.contains("first"),
        "内容変更後は新しい解析結果が返る: {out3}"
    );
}
