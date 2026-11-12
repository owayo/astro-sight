use super::support::*;

#[test]
fn metrics_chain_and_complexity() {
    let repo = TestRepo::new();
    repo.write(
        "a.ts",
        "import './b'; export function f(x: boolean) { if (x) return 1; return 0; }",
    );
    repo.write("b.ts", "import './c'; import './c';");
    repo.write("c.ts", "export function g() { return 1; }");
    let result = repo.run_json("metrics", &[]);
    assert_eq!(result["dependencies"]["component_count"], 3);
    assert_eq!(result["dependencies"]["edge_count"], 2);
    assert_eq!(result["dependencies"]["ccd"], 6);
    assert_eq!(result["dependencies"]["acd"], 2.0);
    assert!(
        (result["dependencies"]["propagation_cost"].as_f64().unwrap() - 2.0 / 3.0).abs() < 1e-12
    );
    assert_eq!(result["complexity"]["count"], 2);
    assert_eq!(result["complexity"]["total"], 3);
    assert_eq!(result["complexity"]["max"], 2);
    assert_eq!(result["cycles"], serde_json::json!([]));
    assert_eq!(result["components"][0]["fan_out"], 1);
    assert_eq!(result["components"][0]["depends_on"], 3);
    assert_eq!(result["components"][2]["used_by"], 3);
}

#[test]
fn metrics_directory_components_recompute_cycles_and_depth() {
    let repo = TestRepo::new();
    repo.create_dir_all("a/sub");
    repo.create_dir_all("b");
    repo.write("a/one.ts", "import '../b/one'; import './sub/two';");
    repo.write("a/sub/two.ts", "export function f() {}");
    repo.write("b/one.ts", "export const x = 1;");
    repo.write("b/two.ts", "import '../a/sub/two';");
    let files = repo.run_json("metrics", &[]);
    assert_eq!(files["cycles"], serde_json::json!([]));
    let groups = repo.run_json("metrics", &["--component", "directory"]);
    assert_eq!(groups["dependencies"]["component_count"], 2);
    assert_eq!(groups["dependencies"]["edge_count"], 2);
    assert_eq!(groups["dependencies"]["ccd"], 4);
    assert_eq!(groups["dependencies"]["acd"], 2.0);
    assert_eq!(groups["dependencies"]["propagation_cost"], 1.0);
    assert_eq!(groups["cycles"], serde_json::json!([["a", "b"]]));
    assert_eq!(groups["file_edges"], files["file_edges"]);
    assert_eq!(groups["complexity"], files["complexity"]);
    assert_eq!(
        groups["components"][0]["files"],
        serde_json::json!(["a/one.ts", "a/sub/two.ts"])
    );
    let deeper = repo.run_json(
        "metrics",
        &["--component", "directory", "--component-depth", "2"],
    );
    assert_eq!(deeper["dependencies"]["component_count"], 3);
    assert_eq!(deeper["cycles"], serde_json::json!([]));
    assert_eq!(deeper["dependencies"]["ccd"], 6);
}

#[test]
fn metrics_empty_isolated_and_internal_cycles() {
    let repo = TestRepo::new();
    let empty = repo.run_json("metrics", &[]);
    assert_eq!(empty["dependencies"]["ccd"], 0);
    assert_eq!(empty["dependencies"]["propagation_cost"], 0.0);
    assert_eq!(empty["complexity"]["average"], 0.0);
    repo.create_dir_all("a");
    repo.write("a/one.js", "import './two.js'; import './one.js';");
    repo.write("a/two.js", "export * from './one.js';");
    repo.write("isolated.js", "export const x = 1;");
    let result = repo.run_json("metrics", &[]);
    assert_eq!(result["dependencies"]["ccd"], 5);
    assert_eq!(result["dependencies"]["cyclic_component_count"], 2);
    assert_eq!(
        result["cycles"],
        serde_json::json!([["a/one.js", "a/two.js"]])
    );
    let grouped = repo.run_json("metrics", &["--component", "directory"]);
    assert_eq!(grouped["dependencies"]["ccd"], 2);
    assert_eq!(grouped["dependencies"]["edge_count"], 0);
    assert_eq!(grouped["cycles"], serde_json::json!([]));
    assert_eq!(grouped["components"][0]["path"], ".");
}

#[test]
fn metrics_resolution_coverage_and_determinism() {
    let repo = TestRepo::new();
    repo.write("a.ts", "import './b'; import './missing'; import 'external'; import('.' + name); export * from './c.js';");
    repo.write("b.ts", "export const x = 1;");
    repo.write("b.tsx", "export const y = 1;");
    repo.write("c.ts", "export const z = 1;");
    repo.write("bad.py", "def bad(:\n");
    repo.write("other.go", "package other\nimport \"fmt\"\n");
    repo.write("view.vue", "<script>import './a';</script>");
    let first = repo.run_json("metrics", &[]);
    assert!(
        first["unresolved_dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == "other.go" && d["source"] == "fmt")
    );
    assert_eq!(
        first["edges"],
        serde_json::json!([{"from":"a.ts", "to":"c.ts"}])
    );
    assert!(
        first["unresolved_dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["reason"] == "ambiguous")
    );
    for reason in [
        "parse_error",
        "dependency_resolution_unsupported",
        "dynamic_import",
    ] {
        assert!(
            first["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["reason"] == reason),
            "{first}"
        );
    }
    assert!(
        first["truncations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["reason"] == "unanalyzable_source")
    );
    assert_eq!(first, repo.run_json("metrics", &[]));
    let filtered = repo.run_json("metrics", &["--glob", "a.ts"]);
    assert_eq!(filtered["dependencies"]["component_count"], 1);
    assert_eq!(filtered["dependencies"]["edge_count"], 0);
    assert_eq!(filtered["glob"], "a.ts");
}

#[test]
fn metrics_rust_modules_grouped_imports_and_inline_scopes() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/a");
    repo.write(
        "src/lib.rs",
        "mod a; mod b; use {crate::a::A, crate::b::B}; use crate::missing::X;",
    );
    repo.write(
        "src/a.rs",
        "pub struct A; use crate::b::B; mod nested { use super::super::b::B; } mod child;",
    );
    repo.write("src/a/child.rs", "use super::super::b::B;");
    repo.write("src/b.rs", "pub struct B; use crate::a::A;");
    let result = repo.run_json("metrics", &[]);
    assert_eq!(
        result["cycles"],
        serde_json::json!([["src/a.rs", "src/a/child.rs", "src/b.rs"]])
    );
    assert_eq!(result["dependencies"]["edge_count"], 6);
    assert!(
        result["unresolved_dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["source"] == "crate::missing::X")
    );
    assert!(
        result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["from"] == "src/a/child.rs" && e["to"] == "src/b.rs")
    );
}

#[test]
fn metrics_local_python_c_ruby_and_zig_imports() {
    let repo = TestRepo::new();
    repo.create_dir_all("pkg");
    repo.write("pkg/__init__.py", "");
    repo.write("pkg/a.py", "from . import b\n");
    repo.write("pkg/b.py", "from .a import thing\nthing = 1\n");
    repo.write("a.c", "#include \"b.h\"\n#include <stdlib.h>\n");
    repo.write("b.h", "int f(void);\n");
    repo.write("a.rb", "require_relative 'b'\n");
    repo.write("b.rb", "require 'external'\n");
    repo.write("a.zig", "const b = @import(\"b.zig\");");
    repo.write("b.zig", "pub const x = 1;");
    let result = repo.run_json("metrics", &[]);
    let edges = result["edges"].as_array().unwrap();
    for (a, b) in [
        ("a.c", "b.h"),
        ("a.rb", "b.rb"),
        ("a.zig", "b.zig"),
        ("pkg/a.py", "pkg/b.py"),
        ("pkg/b.py", "pkg/a.py"),
    ] {
        assert!(
            edges.iter().any(|e| e["from"] == a && e["to"] == b),
            "Missing {a} -> {b}: {result}"
        );
    }
    assert_eq!(
        result["cycles"],
        serde_json::json!([["pkg/a.py", "pkg/b.py"]])
    );
}

#[test]
fn metrics_generated_attributes_and_ignore_rules() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(".gitignore", "ignored.ts\n");
    repo.write(".gitattributes", "generated.ts linguist-generated=true\n");
    repo.write("a.ts", "import './generated';");
    repo.write("ignored.ts", "import './a';");
    repo.write("generated.ts", "import './a';");
    let result = repo.run_json("metrics", &[]);
    assert_eq!(result["dependencies"]["component_count"], 1);
    assert_eq!(result["skipped"]["generated"], 1);
    let included = repo.run_json("metrics", &["--include-generated"]);
    assert_eq!(included["dependencies"]["component_count"], 2);
    assert_eq!(
        included["cycles"],
        serde_json::json!([["a.ts", "generated.ts"]])
    );
}

#[test]
fn metrics_session_scope_and_invalid_depth() {
    let repo = TestRepo::new();
    repo.write("a.py", "def f():\n    return 1\n");
    let service = astro_sight::service::AppService::sandboxed(repo.root().to_path_buf()).unwrap();
    let req = serde_json::from_value(serde_json::json!({
        "command":"metrics", "dir":".", "component":"directory", "component_depth":2,
    }))
    .unwrap();
    let result = astro_sight::commands::handle_request(&service, req).unwrap();
    assert_eq!(result["component_unit"], "directory");
    assert_eq!(result["complexity"]["count"], 1);
    let outside = TestRepo::new();
    let opts = astro_sight::models::metrics::MetricsOptions {
        dir: outside.root().to_str().unwrap().into(),
        ..Default::default()
    };
    assert!(
        service
            .analyze_metrics(&opts)
            .unwrap_err()
            .to_string()
            .contains("outside")
    );
    let opts = astro_sight::models::metrics::MetricsOptions {
        component_depth: 0,
        ..Default::default()
    };
    assert!(
        service
            .analyze_metrics(&opts)
            .unwrap_err()
            .to_string()
            .contains("positive")
    );
}

#[test]
fn metrics_mcp_and_output_formats() {
    let stdout = mcp_send_after_init(&[
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"metrics_analyze","arguments":{"dir":"src/engine/metrics","glob":"graph.rs","component":"directory","component_depth":2}}}"#,
    ]);
    let response: serde_json::Value = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["id"] == 2)
        .expect("metrics response");
    let result: serde_json::Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["component_unit"], "directory");
    assert_eq!(result["dependencies"]["component_count"], 1);
    let repo = TestRepo::new();
    repo.write("a.py", "def f():\n    return 1\n");
    let output = cargo_bin()
        .args([
            "metrics",
            "--dir",
            repo.root().to_str().unwrap(),
            "--format",
            "toon",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("component_unit: file"), "{text}");
    assert!(text.contains("ccd: 1"), "{text}");
}

#[test]
fn metrics_session_protocol_continues_after_invalid_options() {
    use std::io::Write;
    use std::process::Stdio;
    let repo = TestRepo::new();
    repo.write("a.ts", "export function f() {}");
    let mut child = cargo_bin()
        .arg("session")
        .current_dir(repo.root())
        .env("ASTRO_SIGHT_WORKSPACE", repo.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.as_mut().unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"command":"metrics", "component_depth":0})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"command":"metrics", "component":"directory"})
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let results: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["error"]["code"], "INVALID_REQUEST");
    assert_eq!(results[1]["complexity"]["count"], 1);
}

#[test]
fn metrics_rust_path_attribute_is_explicit_and_not_a_substring_match() {
    let repo = TestRepo::new();
    repo.write(
        "lib.rs",
        "#[doc = \"path\"] mod normal; #[path = \"custom.rs\"] mod renamed;",
    );
    repo.write("normal.rs", "pub struct Normal;");
    repo.write("custom.rs", "pub struct Custom;");
    repo.write("renamed.rs", "pub struct Decoy;");
    let result = repo.run_json("metrics", &[]);
    assert_eq!(result["diagnostics"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["diagnostics"][0]["reason"],
        "rust_path_attribute_unresolved"
    );
    assert_eq!(
        result["edges"],
        serde_json::json!([{"from":"lib.rs", "to":"normal.rs"}])
    );
    assert_eq!(
        result["unresolved_dependencies"][0]["source"],
        "mod renamed"
    );
}

#[test]
fn metrics_nested_complexity_keeps_function_boundaries() {
    let repo = TestRepo::new();
    repo.write("a.py", "def outer(x):\n    def inner(y):\n        if y:\n            return 1\n        return 0\n    if x:\n        return inner(x)\n    return 0\n");
    let result = repo.run_json("metrics", &[]);
    assert_eq!(result["complexity"]["count"], 2);
    assert_eq!(result["complexity"]["total"], 4);
    assert_eq!(result["files"][0]["functions"][0]["complexity"], 2);
    assert_eq!(result["files"][0]["functions"][1]["complexity"], 2);
}

#[test]
fn metrics_file_components_keep_identity_with_nested_prefix_paths() {
    let repo = TestRepo::new();
    repo.create_dir_all("a");
    repo.write("a.js", "import './c.js';");
    repo.write("a/b.js", "export const b = 1;");
    repo.write("c.js", "import './a/b.js';");
    let result = repo.run_json("metrics", &[]);
    for file in result["files"].as_array().unwrap() {
        let component = result["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["path"] == file["path"])
            .unwrap();
        for field in ["fan_in", "fan_out", "depends_on", "used_by", "instability"] {
            assert_eq!(component[field], file[field], "{}: {field}", file["path"]);
        }
    }
    assert_eq!(result["components"][0]["path"], "a.js");
    assert_eq!(result["components"][0]["depends_on"], 3);
    assert_eq!(result["components"][1]["path"], "a/b.js");
    assert_eq!(result["components"][1]["depends_on"], 1);
    assert_eq!(result["cycles"], serde_json::json!([]));

    repo.write("c.js", "import './a.js';");
    let cyclic = repo.run_json("metrics", &[]);
    assert_eq!(cyclic["cycles"], serde_json::json!([["a.js", "c.js"]]));
    assert_eq!(cyclic["dependencies"]["ccd"], 5);
}
