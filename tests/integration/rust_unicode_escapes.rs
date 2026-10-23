use super::support::{TestRepo, cargo_bin};
use serde_json::{Value, json};

const SOURCE: &str = include_str!("../fixtures/rust_unicode_escapes.rs");

fn file_json(repo: &TestRepo, command: &str, args: &[&str]) -> Value {
    let output = cargo_bin()
        .arg(command)
        .arg("--path")
        .arg(repo.path("main.rs"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn rust_unicode_separators_preserve_ast_symbols_refs_and_calls() {
    let repo = TestRepo::new();
    repo.write("main.rs", SOURCE);
    let ast = file_json(&repo, "ast", &["--full", "--depth", "32", "--no-cache"]);
    assert!(
        ast["diagnostics"].as_array().is_none_or(Vec::is_empty),
        "{ast}"
    );
    let kinds: Vec<_> = ast["ast"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["const_item", "function_item", "function_item"]);

    let symbols = file_json(&repo, "symbols", &["--no-cache"]);
    assert!(
        symbols["diagnostics"].as_array().is_none_or(Vec::is_empty),
        "{symbols}"
    );
    let names: Vec<_> = symbols["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|symbol| {
            (
                symbol["name"].as_str().unwrap(),
                symbol["ln"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(names, [("LABEL", 0), ("after_unicode", 1), ("main", 2)]);

    let column = SOURCE
        .lines()
        .nth(2)
        .unwrap()
        .find("after_unicode()")
        .unwrap();
    let refs = repo.run_json("refs", &["--name", "after_unicode"]);
    let positions: Vec<_> = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reference| {
            (
                reference["kind"].as_str().unwrap(),
                reference["ln"].as_u64().unwrap(),
                reference["col"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(positions, [("def", 1, 3), ("ref", 2, column as u64)]);

    let calls = file_json(&repo, "calls", &["--function", "main", "--pretty"]);
    let calls = calls["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let call = &calls[0];
    assert_eq!(call["caller"]["name"], "main");
    assert_eq!(call["callee"]["name"], "after_unicode");
    assert_eq!(call["call_site"], json!({"line": 2, "column": column}));
    assert_eq!(
        call["callee"]["range"],
        json!({
            "start": {"line": 2, "column": column},
            "end": {"line": 2, "column": column + "after_unicode()".len()}
        })
    );
}
