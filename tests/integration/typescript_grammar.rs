use super::support::{TestRepo, cargo_bin};
use serde_json::Value;
use std::path::Path;

const TYPESCRIPT_TYPE_QUERY_SOURCE: &str = include_str!("../fixtures/typescript_type_query.ts");
const TSX_TYPE_QUERY_SOURCE: &str = include_str!("../fixtures/typescript_type_query.tsx");

fn typescript_cli_json(command: &str, path: &Path, args: &[&str]) -> Value {
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

fn typescript_ast_nodes(value: &Value) -> Vec<&Value> {
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

fn typescript_ast_field<'a>(node: &'a Value, field: &str) -> &'a Value {
    &node["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["field"] == field)
        .unwrap()["node"]
}

fn assert_typescript_cli_clean(ast: &Value, source: &str) {
    assert!(
        ast["diagnostics"]
            .as_array()
            .is_none_or(|items| items.iter().all(|item| item["severity"] != "error")),
        "有効な TypeScript の構文エラー: {}",
        ast["diagnostics"]
    );
    assert_eq!(
        ast["hash"],
        blake3::hash(source.as_bytes()).to_hex().as_str()
    );
    assert!(
        typescript_ast_nodes(&ast["ast"])
            .iter()
            .all(|node| node["kind"] != "ERROR")
    );
}

#[test]
fn typescript_type_query_cli_preserves_reported_parse_and_locations() {
    for (extension, source, language) in [
        ("ts", TYPESCRIPT_TYPE_QUERY_SOURCE, "typescript"),
        ("tsx", TSX_TYPE_QUERY_SOURCE, "tsx"),
    ] {
        let repo = TestRepo::new();
        let filename = format!("query.{extension}");
        repo.write(&filename, source);
        let path = repo.path(&filename);
        let ast = typescript_cli_json("ast", &path, &["--full", "--depth", "32", "--no-cache"]);
        assert_typescript_cli_clean(&ast, source);
        assert_eq!(ast["language"], language);
        let nodes = typescript_ast_nodes(&ast["ast"]);
        let query = nodes
            .iter()
            .find(|node| node["kind"] == "type_query")
            .unwrap();
        assert_eq!(query["range"]["start"]["line"], 1);
        assert_eq!(query["range"]["start"]["column"], 38);
        assert_eq!(query["range"]["end"]["column"], 62);
        let call = nodes
            .iter()
            .find(|node| {
                node["kind"] == "call_expression"
                    && node["range"]["start"]["line"] == 1
                    && node["children"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|child| child["field"] == "type_arguments")
            })
            .unwrap();
        assert!(
            typescript_ast_nodes(typescript_ast_field(call, "function"))
                .iter()
                .any(|node| {
                    node["kind"] == "identifier"
                        && node["text"] == "importOriginal"
                        && node["range"]["start"]["column"] == 23
                })
        );
        assert_eq!(
            typescript_ast_field(call, "type_arguments")["kind"],
            "type_arguments"
        );
        assert_eq!(
            typescript_ast_field(call, "arguments")["range"]["start"]["column"],
            63
        );
        assert_eq!(call["range"]["end"]["column"], 65);
        assert_eq!(
            nodes
                .iter()
                .filter(|node| node["kind"] == "await_expression")
                .count(),
            1
        );
        assert_eq!(
            nodes
                .iter()
                .filter(|node| node["kind"] == "method_definition")
                .count(),
            2
        );
        if extension == "tsx" {
            assert_eq!(
                nodes
                    .iter()
                    .filter(|node| node["kind"] == "jsx_element")
                    .count(),
                1
            );
        }

        let symbols = typescript_cli_json("symbols", &path, &["--no-cache"]);
        for (name, line, kind) in [
            ("load", 0, "fn"),
            ("read", 6, "method"),
            ("close", 7, "method"),
        ] {
            assert!(
                symbols["symbols"].as_array().unwrap().iter().any(|symbol| {
                    symbol["name"] == name && symbol["ln"] == line && symbol["kind"] == kind
                }),
                "{symbols}"
            );
        }
        let calls = typescript_cli_json("calls", &path, &["--function", "load"]);
        // 括弧なし generic await の callee 抽出は旧文法でも未対応。
        assert!(calls["calls"].as_array().unwrap().is_empty(), "{calls}");
        let method_calls = typescript_cli_json("calls", &path, &["--function", "read"]);
        assert_eq!(method_calls["calls"][0]["callees"][0]["name"], "load");
        assert_eq!(method_calls["calls"][0]["callees"][0]["ln"], 6);
        assert_eq!(method_calls["calls"][0]["callees"][0]["col"], 18);
        let imports = typescript_cli_json("imports", &path, &[]);
        assert_eq!(imports["imports"].as_array().unwrap().len(), 1);
        assert_eq!(imports["imports"][0]["src"], "node:fs");
        assert_eq!(imports["imports"][0]["ln"], 1);
        let refs = repo.run_json("refs", &["--name", "load", "--max-results", "unlimited"]);
        let references = refs["refs"].as_array().unwrap();
        assert_eq!(references.len(), 2, "{refs}");
        for (kind, line, column) in [("def", 0, 15), ("ref", 6, 18)] {
            assert!(references.iter().any(|reference| {
                reference["path"] == filename
                    && reference["kind"] == kind
                    && reference["ln"] == line
                    && reference["col"] == column
            }));
        }
    }
}

#[test]
fn typescript_type_query_cli_preserves_unicode_and_crlf_ranges() {
    for extension in ["ts", "tsx"] {
        let source = format!(
            "// 名前🙂\r\n{}",
            TYPESCRIPT_TYPE_QUERY_SOURCE
                .replace(
                    "await importOriginal<typeof import(\"node:fs\")>()",
                    "await (/* 名前🙂 */ importOriginal<typeof import(\"node:fs\")>())"
                )
                .replace('\n', "\r\n")
        );
        let repo = TestRepo::new();
        let filename = format!("unicode.{extension}");
        repo.write(&filename, &source);
        let path = repo.path(&filename);
        let ast = typescript_cli_json("ast", &path, &["--full", "--depth", "32", "--no-cache"]);
        assert_typescript_cli_clean(&ast, &source);
        let nodes = typescript_ast_nodes(&ast["ast"]);
        let column = "  const actual = await (/* 名前🙂 */ ".len();
        assert!(nodes.iter().any(|node| {
            node["kind"] == "call_expression"
                && node["range"]["start"]["line"] == 2
                && node["range"]["start"]["column"] == column
                && node["range"]["end"]["column"]
                    == column + "importOriginal<typeof import(\"node:fs\")>()".len()
        }));
        let calls = typescript_cli_json("calls", &path, &["--function", "load"]);
        assert_eq!(calls["calls"][0]["callees"][0]["ln"], 2);
        assert_eq!(calls["calls"][0]["callees"][0]["col"], column);
        let refs = repo.run_json(
            "refs",
            &["--name", "importOriginal", "--max-results", "unlimited"],
        );
        assert!(refs["refs"].as_array().unwrap().iter().any(|reference| {
            reference["kind"] == "ref" && reference["ln"] == 2 && reference["col"] == column
        }));
    }
}

#[test]
fn typescript_type_query_cli_keeps_malformed_diagnostics_and_later_methods() {
    for (extension, source) in [
        ("ts", TYPESCRIPT_TYPE_QUERY_SOURCE),
        ("tsx", TSX_TYPE_QUERY_SOURCE),
    ] {
        let source = source.replace("typeof import(\"node:fs\")", "typeof import(@)");
        let repo = TestRepo::new();
        let filename = format!("broken.{extension}");
        repo.write(&filename, &source);
        let path = repo.path(&filename);
        let ast = typescript_cli_json("ast", &path, &["--full", "--depth", "32", "--no-cache"]);
        assert!(
            ast["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["severity"] == "error")
        );
        let symbols = typescript_cli_json("symbols", &path, &["--no-cache"]);
        for (name, line) in [("read", 6), ("close", 7)] {
            assert!(
                symbols["symbols"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|symbol| { symbol["name"] == name && symbol["ln"] == line })
            );
        }
    }
}

#[test]
fn typescript_type_query_cli_direct_and_grouped_calls_have_callees() {
    let source = "function direct() { return importOriginal<typeof import(\"node:fs\")>() }\nasync function grouped() { return await (importOriginal<typeof import(\"node:fs\")>()) }\n";
    for extension in ["ts", "tsx"] {
        let repo = TestRepo::new();
        let filename = format!("calls.{extension}");
        repo.write(&filename, source);
        let path = repo.path(&filename);
        let ast = typescript_cli_json("ast", &path, &["--full", "--depth", "32", "--no-cache"]);
        assert_typescript_cli_clean(&ast, source);
        let nodes = typescript_ast_nodes(&ast["ast"]);
        let outer_calls: Vec<_> = nodes
            .iter()
            .filter(|node| {
                node["kind"] == "call_expression"
                    && node["children"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|child| child["field"] == "type_arguments")
            })
            .collect();
        assert_eq!(outer_calls.len(), 2);
        for call in outer_calls {
            assert_eq!(typescript_ast_field(call, "function")["kind"], "identifier");
            assert_eq!(
                typescript_ast_field(call, "function")["text"],
                "importOriginal"
            );
            assert_eq!(
                typescript_ast_field(call, "type_arguments")["kind"],
                "type_arguments"
            );
        }
        let calls = typescript_cli_json("calls", &path, &[]);
        assert_eq!(calls["calls"].as_array().unwrap().len(), 2);
        for (line, caller) in [(0, "direct"), (1, "grouped")] {
            let call = calls["calls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|call| call["caller"] == caller)
                .unwrap();
            let callees = call["callees"].as_array().unwrap();
            assert_eq!(callees.len(), 1);
            assert_eq!(callees[0]["name"], "importOriginal");
            assert_eq!(callees[0]["ln"], line);
            assert_eq!(
                callees[0]["col"],
                source
                    .lines()
                    .nth(line)
                    .unwrap()
                    .find("importOriginal")
                    .unwrap()
            );
        }
        let imports = typescript_cli_json("imports", &path, &[]);
        assert_eq!(imports["imports"].as_array().unwrap().len(), 2);
        assert!(
            imports["imports"]
                .as_array()
                .unwrap()
                .iter()
                .all(|import| import["src"] == "node:fs")
        );
        let refs = repo.run_json(
            "refs",
            &["--name", "importOriginal", "--max-results", "unlimited"],
        );
        assert_eq!(refs["refs"].as_array().unwrap().len(), 2);
        assert!(
            refs["refs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|reference| reference["kind"] == "ref")
        );
    }
}

#[test]
fn typescript_await_and_tagged_template_cli_no_regression_vs_baseline() {
    let source = "async function bare() { return await call<Item>(); }\nasync function grouped() { return await (call<Item>()); }\nconst tagged = tag<Item>`value`;\n";
    for (extension, language) in [("ts", "typescript"), ("tsx", "tsx")] {
        let repo = TestRepo::new();
        let filename = format!("baseline.{extension}");
        repo.write(&filename, source);
        let path = repo.path(&filename);
        let ast = typescript_cli_json("ast", &path, &["--full", "--depth", "32", "--no-cache"]);
        assert_typescript_cli_clean(&ast, source);
        let calls = typescript_cli_json("calls", &path, &[]);
        assert_eq!(
            calls,
            serde_json::json!({
                "lang": language,
                "calls": [{"caller": "grouped", "range": [1, 0, 1, 57],
                    "callees": [{"name": "call", "ln": 1, "col": 41}]}]
            })
        );
    }
}
