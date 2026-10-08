use super::support::{TestRepo, cargo_bin};
use serde_json::Value;
use std::path::Path;

const SWIFT_COALESCING_SOURCE: &str = include_str!("../fixtures/swift_nil_coalescing.swift");
const SWIFT_EMPTY_TUPLE_SOURCE: &str = include_str!("../fixtures/swift_empty_tuple.swift");

fn swift_cli_json(command: &str, path: &Path, args: &[&str]) -> Value {
    let output = cargo_bin()
        .arg(command)
        .arg("--path")
        .arg(path)
        .args(args)
        .output()
        .expect("failed to run astro-sight");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("invalid JSON")
}

fn swift_ast_nodes(value: &Value) -> Vec<&Value> {
    let mut nodes = Vec::new();
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(map) => {
                if map.contains_key("kind") {
                    nodes.push(value);
                }
                pending.extend(map.values());
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    nodes
}

#[test]
fn swift_empty_tuple_cli_parses_values_and_patterns_without_diagnostics() {
    let repo = TestRepo::new();
    repo.write("unit.swift", SWIFT_EMPTY_TUPLE_SOURCE);
    let path = repo.path("unit.swift");

    for command in ["ast", "symbols"] {
        let output = swift_cli_json(command, &path, &["--no-cache"]);
        assert!(
            output["diagnostics"]
                .as_array()
                .is_none_or(|items| items.is_empty()),
            "{command}: {}",
            output["diagnostics"]
        );
    }

    let ast = swift_cli_json("ast", &path, &["--full", "--depth", "32", "--no-cache"]);
    let nodes = swift_ast_nodes(&ast["ast"]);
    assert!(nodes.iter().all(|node| node["kind"] != "ERROR"));
    assert!(nodes.iter().any(|node| {
        node["kind"] == "tuple_expression"
            && node["range"]["start"]["line"] == 3
            && node["children"]
                .as_array()
                .is_none_or(|children| children.is_empty())
    }));
    assert!(nodes.iter().any(|node| {
        node["kind"] == "tuple_expression"
            && node["range"]["start"]["line"] == 8
            && node["children"]
                .as_array()
                .is_none_or(|children| children.is_empty())
    }));
    assert!(nodes.iter().any(|node| {
        node["kind"] == "value_arguments"
            && node["range"]["start"]["line"] == 34
            && node["children"]
                .as_array()
                .is_none_or(|children| children.is_empty())
    }));
    assert!(
        nodes
            .iter()
            .any(|node| { node["kind"] == "tuple_type" && node["range"]["start"]["line"] == 36 })
    );
    assert!(nodes.iter().any(|node| {
        node["kind"] == "lambda_function_type" && node["range"]["start"]["line"] == 36
    }));
}

#[test]
fn swift_nil_coalescing_cli_preserves_structure_and_locations() {
    let repo = TestRepo::new();
    repo.write("chain.swift", SWIFT_COALESCING_SOURCE);
    let path = repo.path("chain.swift");
    let ast = swift_cli_json("ast", &path, &["--full", "--depth", "32", "--no-cache"]);
    assert!(
        ast["diagnostics"]
            .as_array()
            .is_none_or(|items| items.iter().all(|item| item["severity"] != "error")),
        "有効な Swift の構文エラー: {}",
        ast["diagnostics"]
    );
    assert_eq!(ast["language"], "swift");
    assert_eq!(
        ast["hash"],
        blake3::hash(SWIFT_COALESCING_SOURCE.as_bytes())
            .to_hex()
            .as_str()
    );
    let nodes = swift_ast_nodes(&ast["ast"]);
    assert!(nodes.iter().all(|node| node["kind"] != "ERROR"));
    assert!(nodes.iter().all(|node| node["kind"] != "optional_type"));
    assert_eq!(
        nodes
            .iter()
            .filter(|node| node["kind"] == "nil_coalescing_expression")
            .count(),
        2
    );
    let mut casts: Vec<_> = nodes
        .iter()
        .filter(|node| node["kind"] == "as_expression")
        .collect();
    casts.sort_by_key(|node| node["range"]["start"]["line"].as_u64().unwrap());
    assert_eq!(casts.len(), 2);
    for (cast, line, start, end) in [(casts[0], 4, 4, 34), (casts[1], 5, 7, 30)] {
        assert_eq!(cast["range"]["start"]["line"], line);
        assert_eq!(cast["range"]["start"]["column"], start);
        assert_eq!(cast["range"]["end"]["line"], line);
        assert_eq!(cast["range"]["end"]["column"], end);
    }
    assert!(nodes.iter().any(|node| {
        node["kind"] == "function_declaration"
            && node["range"]["start"]["line"] == 10
            && node["range"]["end"]["line"] == 12
            && node["range"]["end"]["column"] == 1
    }));

    let symbols = swift_cli_json("symbols", &path, &["--no-cache"]);
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
    assert_eq!(names, [("displayName", 2), ("afterChain", 10)]);
    let calls = swift_cli_json("calls", &path, &["--function", "afterChain"]);
    assert_eq!(calls["calls"].as_array().unwrap().len(), 1);
    assert_eq!(calls["calls"][0]["callees"].as_array().unwrap().len(), 1);
    assert_eq!(calls["calls"][0]["callees"][0]["name"], "displayName");
    assert_eq!(calls["calls"][0]["callees"][0]["ln"], 11);
    assert_eq!(calls["calls"][0]["callees"][0]["col"], 9);
    let imports = swift_cli_json("imports", &path, &[]);
    assert_eq!(imports["imports"][0]["src"], "Foundation");
    assert_eq!(imports["imports"][0]["ln"], 0);

    let refs = repo.run_json(
        "refs",
        &["--name", "displayName", "--max-results", "unlimited"],
    );
    let references = refs["refs"].as_array().unwrap();
    assert_eq!(references.len(), 2);
    assert!(references.iter().any(|reference| {
        reference["path"] == "chain.swift"
            && reference["kind"] == "def"
            && reference["ln"] == 2
            && reference["col"] == 5
    }));
    assert!(references.iter().any(|reference| {
        reference["path"] == "chain.swift"
            && reference["kind"] == "ref"
            && reference["ln"] == 11
            && reference["col"] == 9
    }));
}

#[test]
fn swift_nil_coalescing_cli_preserves_crlf_and_unicode() {
    let prefix = "    /* 名前🙂 */ ";
    let source = format!(
        "// 名前🙂\r\n{}",
        SWIFT_COALESCING_SOURCE
            .replace(
                "    info[\"displayName\"]",
                &format!("{prefix}info[\"displayName\"]")
            )
            .replace('\n', "\r\n")
    );
    let repo = TestRepo::new();
    repo.write("unicode.swift", &source);
    let path = repo.path("unicode.swift");
    let ast = swift_cli_json("ast", &path, &["--full", "--depth", "32", "--no-cache"]);
    assert!(
        ast["diagnostics"]
            .as_array()
            .is_none_or(|items| items.iter().all(|item| item["severity"] != "error"))
    );
    assert_eq!(
        ast["hash"],
        blake3::hash(source.as_bytes()).to_hex().as_str()
    );
    let nodes = swift_ast_nodes(&ast["ast"]);
    let cast = nodes
        .iter()
        .find(|node| node["kind"] == "as_expression" && node["range"]["start"]["line"] == 5)
        .unwrap();
    assert_eq!(cast["range"]["start"]["column"], prefix.len());
    assert_eq!(cast["range"]["end"]["line"], 5);
    assert_eq!(
        cast["range"]["end"]["column"],
        prefix.len() + "info[\"displayName\"] as? String".len()
    );
    assert!(nodes.iter().any(|node| {
        node["kind"] == "function_declaration"
            && node["range"]["start"]["line"] == 11
            && node["range"]["end"]["line"] == 13
            && node["range"]["end"]["column"] == 1
    }));
    let refs = repo.run_json(
        "refs",
        &["--name", "displayName", "--max-results", "unlimited"],
    );
    assert!(refs["refs"].as_array().unwrap().iter().any(|reference| {
        reference["path"] == "unicode.swift"
            && reference["kind"] == "ref"
            && reference["ln"] == 12
            && reference["col"] == 9
    }));
    assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
}

#[test]
fn swift_nil_coalescing_cli_keeps_malformed_diagnostics() {
    let source = "func broken(_ value: Any) {\n  let text = value as? @\n}\nfunc afterBroken() -> Int { return 1 }\n";
    let repo = TestRepo::new();
    repo.write("broken.swift", source);
    let path = repo.path("broken.swift");
    let ast = swift_cli_json("ast", &path, &["--full", "--depth", "32", "--no-cache"]);
    assert!(
        ast["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["severity"] == "error")
    );
    let symbols = swift_cli_json("symbols", &path, &["--no-cache"]);
    assert!(
        symbols["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["severity"] == "error")
    );
    assert!(
        symbols["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| { symbol["name"] == "afterBroken" && symbol["ln"] == 3 })
    );
}
