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
    assert_eq!(first["edges"].as_array().unwrap().len(), 1);
    assert_eq!(first["edges"][0]["from"], "a.ts");
    assert_eq!(first["edges"][0]["to"], "c.ts");
    assert_eq!(first["edges"][0]["evidence"][0]["kind"], "reexport");
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
    assert_eq!(result["diagnostics"], serde_json::json!([]));
    assert_eq!(result["edges"].as_array().unwrap().len(), 2);
    assert_eq!(result["edges"][0]["from"], "lib.rs");
    assert_eq!(result["edges"][0]["to"], "custom.rs");
    assert_eq!(result["edges"][1]["to"], "normal.rs");
    assert_eq!(result["unresolved_dependencies"], serde_json::json!([]));
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

#[test]
fn metrics_cycle_paths_include_source_evidence_after_grouping() {
    let repo = TestRepo::new();
    repo.create_dir_all("a");
    repo.create_dir_all("b");
    repo.write("a/first.ts", "export * from '../b/first';");
    repo.write("b/first.ts", "import '../a/first';");
    let result = repo.run_json("metrics", &["--component", "directory"]);
    let cycle = &result["cycle_evidence"][0];
    assert_eq!(cycle["components"], serde_json::json!(["a", "b"]));
    assert_eq!(cycle["steps"][0]["from"], "a");
    assert_eq!(cycle["steps"][0]["to"], "b");
    assert_eq!(cycle["steps"][0]["evidence"][0]["path"], "a/first.ts");
    assert_eq!(cycle["steps"][0]["evidence"][0]["line"], 0);
    assert_eq!(cycle["steps"][0]["evidence"][0]["target"], "b/first.ts");
    assert_eq!(cycle["steps"][1]["to"], "a");
    assert_eq!(result["cycle_evidence_mode"], "one_representative_per_scc");
}

#[test]
fn metrics_compare_base_uses_same_scope_and_reports_denominator_changes() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("a.ts", "import './b'; export function f() {}");
    repo.write("b.ts", "export const b = 1;");
    repo.commit_all("baseline");
    repo.write("b.ts", "import './a';");
    repo.write("c.ts", "export const c = 1;");
    let result = repo.run_json("metrics", &["--base", "HEAD"]);
    let comparison = &result["comparison"];
    assert_eq!(comparison["baseline"]["dependencies"]["ccd"], 3);
    assert_eq!(comparison["delta"]["ccd"], 2);
    assert_eq!(comparison["normalization_changed"], true);
    assert_eq!(comparison["added_components"], serde_json::json!(["c.ts"]));
    assert_eq!(comparison["added_edges"][0]["from"], "b.ts");
    assert_eq!(comparison["added_edges"][0]["to"], "a.ts");
    assert_eq!(
        comparison["added_cycle_groups"],
        serde_json::json!([["a.ts", "b.ts"]])
    );
    assert_eq!(comparison["base_commit"].as_str().unwrap().len(), 40);
}

#[test]
fn metrics_typescript_paths_extends_fallbacks_and_external_packages() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/lib");
    repo.write("base.json", "{ // base\n \"compilerOptions\": {\"baseUrl\": \".\", \"paths\": {\"@lib/*\": [\"missing/*\", \"src/lib/*\"],},},}");
    repo.write("tsconfig.json", r#"{"extends":"./base.json"}"#);
    repo.write(
        "package.json",
        r#"{"name":"app","dependencies":{"react":"^1.0.0","local":"workspace:*"}}"#,
    );
    repo.write(
        "src/main.ts",
        "import '@lib/value'; import 'react'; import 'local'; import 'node:fs';",
    );
    repo.write("src/lib/value.ts", "export const value = 1;");
    let result = repo.run_json("metrics", &[]);
    assert_eq!(result["edges"].as_array().unwrap().len(), 1);
    assert_eq!(result["edges"][0]["from"], "src/main.ts");
    assert_eq!(result["edges"][0]["to"], "src/lib/value.ts");
    assert_eq!(
        result["edges"][0]["evidence"][0]["resolution"],
        "typescript_paths"
    );
    assert_eq!(result["resolution_summary"]["external_imports"], 2);
    assert_eq!(result["resolution_summary"]["unresolved_imports"], 1);
    assert_eq!(result["unresolved_dependencies"][0]["source"], "local");
}

#[test]
fn metrics_rust_declared_modules_custom_paths_and_external_crates() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/node");
    repo.write(
        "Cargo.toml",
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nserde='1'\n",
    );
    repo.write("src/lib.rs", "#[path=\"logic.rs\"] mod renamed; mod node; use renamed::Real; use std::path::Path; use serde::Serialize;");
    repo.write("src/logic.rs", "pub struct Real;");
    repo.write("src/renamed.rs", "pub struct Decoy;");
    repo.write(
        "src/node/mod.rs",
        "mod child; use child::Child; use crate::renamed::Real;",
    );
    repo.write("src/node/child.rs", "pub struct Child;");
    let result = repo.run_json("metrics", &[]);
    let edges = result["edges"].as_array().unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e["from"] == "src/node/mod.rs" && e["to"] == "src/logic.rs")
    );
    assert!(!edges.iter().any(|e| e["to"] == "src/renamed.rs"));
    let child = edges
        .iter()
        .find(|e| e["from"] == "src/node/mod.rs" && e["to"] == "src/node/child.rs")
        .unwrap();
    assert_eq!(child["evidence"].as_array().unwrap().len(), 2);
    assert_eq!(result["resolution_summary"]["external_imports"], 2);
    assert_eq!(result["resolution_summary"]["unresolved_imports"], 0);
    repo.write(
        "Cargo.toml",
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2015'\n[dependencies]\nserde='1'\n",
    );
    let old_edition = repo.run_json("metrics", &[]);
    assert!(
        old_edition["unresolved_dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["source"] == "child::Child")
    );
}

#[test]
fn metrics_comparison_subdir_deletions_attributes_and_unchanged_git_state() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src/日本語");
    repo.write(
        ".gitattributes",
        "src/generated.ts linguist-generated=true\n",
    );
    repo.write("src/日本語/a.ts", "import '../b';");
    repo.write("src/b.ts", "export const x = 1;");
    repo.write("src/generated.ts", "import './日本語/a';");
    repo.commit_all("baseline");
    repo.remove_file("src/b.ts");
    repo.write(
        ".gitattributes",
        "src/generated.ts linguist-generated=false\n",
    );
    repo.stage_all();
    let before = std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "-z"])
        .current_dir(repo.root())
        .output()
        .unwrap()
        .stdout;
    let result = repo.run_json_in(
        "src",
        std::iter::empty::<(&str, &str)>(),
        "metrics",
        &[
            "--base",
            "HEAD",
            "--component",
            "directory",
            "--component-depth",
            "1",
        ],
    );
    assert_eq!(result["comparison"]["baseline"]["skipped"]["generated"], 1);
    assert!(
        result["comparison"]["baseline"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "日本語/a.ts")
    );
    assert!(
        !result["comparison"]["removed_edges"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let after = std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "-z"])
        .current_dir(repo.root())
        .output()
        .unwrap()
        .stdout;
    assert_eq!(before, after);
}

#[test]
fn metrics_invalid_project_configuration_fails_and_unsupported_extends_is_reported() {
    let repo = TestRepo::new();
    repo.write("a.ts", "import '@alias/value'; import './b';");
    repo.write("b.ts", "export const value = 1;");
    for content in [
        "{broken",
        "[]",
        r#"{"extends":42}"#,
        r#"{"compilerOptions":{"baseUrl":42}}"#,
        r#"{"compilerOptions":{"paths":{"@alias/*":[]}}}"#,
        r#"{"compilerOptions":{"paths":{"x":["foo/*"]}}}"#,
    ] {
        repo.write("tsconfig.json", content);
        let output = cargo_bin()
            .args(["metrics", "--dir", repo.root().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted {content}");
        let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["code"], "INVALID_REQUEST");
    }
    repo.write("tsconfig.json", r#"{"extends":"./base.json"}"#);
    repo.write("base.json", r#"{"extends":"./tsconfig.json"}"#);
    let output = cargo_bin()
        .args(["metrics", "--dir", repo.root().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    repo.write("tsconfig.json", r#"{"extends":"@external/base"}"#);
    let result = repo.run_json("metrics", &[]);
    assert_eq!(result["edges"][0]["to"], "b.ts");
    assert_eq!(
        result["unresolved_dependencies"][0]["reason"],
        "unsupported_config"
    );
    assert_eq!(
        result["diagnostics"][0]["reason"],
        "typescript_config_unsupported"
    );
}

#[test]
fn metrics_typescript_path_specificity_and_ambiguous_patterns() {
    let repo = TestRepo::new();
    repo.create_dir_all("wide");
    repo.create_dir_all("specific");
    repo.write("wide/value.ts", "export const wide = 1;");
    repo.write("specific/value.ts", "export const specific = 1;");
    repo.write("exact.ts", "export const exact = 1;");
    repo.write("a.ts", "import '@lib/value'; import '@lib/exact';");
    repo.write("tsconfig.json", r#"{"compilerOptions":{"paths":{"@*":["wide/*"],"@lib/*":["specific/*"],"@lib/exact":["exact.ts"]}}}"#);
    let result = repo.run_json("metrics", &[]);
    assert_eq!(result["edges"][0]["to"], "exact.ts");
    assert_eq!(result["edges"][1]["to"], "specific/value.ts");
    repo.write("a.ts", "import '@value.ts';");
    repo.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@*ts":["exact.ts"],"@*.ts":["exact.ts"]}}}"#,
    );
    let ambiguous = repo.run_json("metrics", &[]);
    assert_eq!(ambiguous["edges"], serde_json::json!([]));
    assert_eq!(ambiguous["resolution_summary"]["ambiguous_imports"], 1);
}

#[test]
fn metrics_rust_alias_shadowing_and_multiple_owners_remain_unverified() {
    let repo = TestRepo::new();
    repo.write(
        "Cargo.toml",
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nserde='1'\n",
    );
    repo.write("lib.rs", "use std as serde; use serde::Serialize;");
    let result = repo.run_json("metrics", &[]);
    assert_eq!(result["resolution_summary"]["external_imports"], 1);
    assert_eq!(
        result["unresolved_dependencies"][0]["source"],
        "serde::Serialize"
    );
    repo.write(
        "lib.rs",
        "#[path=\"common.rs\"] mod a; #[path=\"common.rs\"] mod b;",
    );
    repo.write("common.rs", "use super::X;");
    let ambiguous = repo.run_json("metrics", &[]);
    assert_eq!(
        ambiguous["resolution_summary"]["all_static_imports_resolved"],
        false
    );
    assert!(
        ambiguous["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["reason"] == "rust_module_context_unverified")
    );
}

#[test]
fn metrics_comparison_empty_base_invalid_revisions_and_changed_coverage() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.git(["commit", "--allow-empty", "-qm", "empty"]);
    repo.write("a.ts", "import './old_missing';");
    let empty = repo.run_json("metrics", &["--base", "HEAD"]);
    assert_eq!(
        empty["comparison"]["baseline"]["dependencies"]["component_count"],
        0
    );
    assert_eq!(empty["comparison"]["delta"]["ccd"], 1);
    repo.commit_all("unknown import");
    repo.write("a.ts", "import './new_missing';");
    let changed = repo.run_json("metrics", &["--base", "HEAD"]);
    assert_eq!(changed["comparison"]["coverage_changed"], true);
    assert_eq!(
        changed["comparison"]["assessment"],
        "partial_observed_graphs"
    );
    for revision in ["does-not-exist", "", "--help"] {
        let output = cargo_bin()
            .args([
                "metrics",
                "--dir",
                repo.root().to_str().unwrap(),
                &format!("--base={revision}"),
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(error["error"]["code"].is_string());
    }
}

#[test]
fn metrics_baseline_reports_omitted_links_and_submodules() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("a.py", "def f():\n    return 1\n");
    repo.commit_all("base");
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    let commit = String::from_utf8(commit.stdout).unwrap().trim().to_string();
    repo.write("link-target.txt", "a.py");
    let blob = std::process::Command::new("git")
        .args(["hash-object", "-w", "--no-filters", "link-target.txt"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    let blob = String::from_utf8(blob.stdout).unwrap().trim().to_string();
    repo.git([
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("120000,{blob},alias.py"),
    ]);
    repo.git([
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{commit},submodule"),
    ]);
    repo.git(["commit", "-qm", "tree entries"]);
    let result = repo.run_json("metrics", &["--base", "HEAD"]);
    let diagnostics = result["comparison"]["baseline"]["diagnostics"]
        .as_array()
        .unwrap();
    assert!(
        diagnostics
            .iter()
            .any(|d| d["reason"] == "baseline_symlink_omitted" && d["path"] == "alias.py")
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d["reason"] == "baseline_submodule_omitted" && d["path"] == "submodule")
    );
    assert_eq!(
        result["comparison"]["assessment"],
        "partial_observed_graphs"
    );
}

#[test]
fn metrics_baseline_supports_sha256_repositories() {
    let repo = TestRepo::new();
    repo.git(["init", "--object-format=sha256", "-q"]);
    repo.init_git();
    repo.write("a.py", "def f():\n    return 1\n");
    repo.commit_all("base");
    let result = repo.run_json("metrics", &["--base", "HEAD"]);
    assert_eq!(
        result["comparison"]["base_commit"].as_str().unwrap().len(),
        64
    );
    assert_eq!(result["comparison"]["delta"]["ccd"], 0);
}

#[test]
fn metrics_mcp_base_comparison() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("sample.py", "def f():\n    return 1\n");
    repo.commit_all("baseline");
    let stdout = mcp_send_after_init_with(
        &[],
        Some(repo.root()),
        &[
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"metrics_analyze","arguments":{"dir":".","glob":"sample.py","base":"HEAD"}}}"#,
        ],
    );
    let response: serde_json::Value = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["id"] == 2)
        .unwrap();
    let result: serde_json::Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["comparison"]["delta"]["ccd"], 0, "{result}");
}

#[test]
fn metrics_session_base_preserves_scope_options() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("a.py", "def f():\n    return 1\n");
    repo.commit_all("baseline");
    let service = astro_sight::service::AppService::sandboxed(repo.root().to_path_buf()).unwrap();
    let request = serde_json::from_value(serde_json::json!({
        "command":"metrics", "base":"HEAD", "component":"directory", "component_depth":2, "glob":"*.py",
    })).unwrap();
    let result = astro_sight::commands::handle_request(&service, request).unwrap();
    assert_eq!(
        result["comparison"]["baseline"]["component_unit"],
        "directory"
    );
    assert_eq!(result["comparison"]["baseline"]["component_depth"], 2);
    assert_eq!(result["comparison"]["baseline"]["glob"], "*.py");
    assert_eq!(result["comparison"]["delta"]["ccd"], 0);
}

#[cfg(unix)]
#[test]
fn metrics_project_configuration_symlink_cannot_escape_sandbox() {
    let repo = TestRepo::new();
    let outside = TestRepo::new();
    repo.write("a.ts", "import '@private/value';");
    outside.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@private/*":["*"]}}}"#,
    );
    std::os::unix::fs::symlink(outside.path("tsconfig.json"), repo.path("tsconfig.json")).unwrap();
    let service = astro_sight::service::AppService::sandboxed(repo.root().to_path_buf()).unwrap();
    let error = service
        .analyze_metrics(&astro_sight::models::metrics::MetricsOptions::default())
        .unwrap_err();
    assert!(error.to_string().contains("PATH_OUT_OF_BOUNDS"));
}

#[test]
fn metrics_function_comparison_reports_complexity_and_ambiguous_overloads() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "a.py",
        "def f(x):\n    return x\n\ndef unchanged(x):\n    return x\n",
    );
    repo.commit_all("baseline");
    repo.write(
        "a.py",
        "def f(x):\n    if x:\n        return 1\n    return 0\n\ndef unchanged(x):\n    return x\n",
    );
    let result = repo.run_json("metrics", &["--base", "HEAD"]);
    let changes = result["comparison"]["function_changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["name"], "f");
    assert_eq!(changes[0]["before"][0]["complexity"], 1);
    assert_eq!(changes[0]["after"][0]["complexity"], 2);
    assert_eq!(changes[0]["pairing"], "unique_name_and_container");
    repo.write(
        "a.py",
        "def f(x):\n    return x\n\ndef f(y):\n    return y\n",
    );
    let ambiguous = repo.run_json("metrics", &["--base", "HEAD"]);
    assert_eq!(
        ambiguous["comparison"]["function_changes"][0]["pairing"],
        "ambiguous_name_and_container"
    );
}

#[test]
fn metrics_rust_explicit_and_automatic_targets_respect_local_packages() {
    let repo = TestRepo::new();
    for directory in ["custom", "tools", "tests", "src/bin"] {
        repo.create_dir_all(directory);
    }
    repo.write(
        "Cargo.toml",
        r#"
[package]
name = "demo"
edition = "2024"
autobins = false
[lib]
name = "api"
path = "custom/root.rs"
[[bin]]
name = "runner"
path = "tools/runner.rs"
[dependencies]
patched = "1"
ext = "1"
[patch.crates-io]
patched = { path = "deps/patched" }
"#,
    );
    repo.write("custom/root.rs", "pub struct Thing;");
    repo.write(
        "tools/runner.rs",
        "use api::Thing; use ext::Item; use patched::Item;",
    );
    repo.write(
        "tests/spec.rs",
        "mod support; use support::Thing; use api::Thing;",
    );
    repo.write("tests/support.rs", "pub struct Thing;");
    repo.write("src/bin/ghost.rs", "use ext::Item;");
    let result = repo.run_json("metrics", &[]);
    let edges = result["edges"].as_array().unwrap();
    for (from, to) in [
        ("tools/runner.rs", "custom/root.rs"),
        ("tests/spec.rs", "tests/support.rs"),
        ("tests/spec.rs", "custom/root.rs"),
    ] {
        assert!(
            edges
                .iter()
                .any(|edge| edge["from"] == from && edge["to"] == to),
            "{result}"
        );
    }
    assert_eq!(result["resolution_summary"]["external_imports"], 1);
    let unresolved = result["unresolved_dependencies"].as_array().unwrap();
    assert!(
        unresolved
            .iter()
            .any(|item| item["source"] == "patched::Item")
    );
    assert!(
        unresolved
            .iter()
            .any(|item| item["path"] == "src/bin/ghost.rs")
    );
}

#[test]
fn metrics_javascript_local_package_is_not_counted_as_external() {
    let repo = TestRepo::new();
    repo.create_dir_all("packages/local");
    repo.write(
        "package.json",
        r#"{"dependencies":{"local":"*","external":"1"}}"#,
    );
    repo.write("a.ts", "import 'local'; import 'external';");
    repo.write("packages/local/package.json", r#"{"name":"local"}"#);
    repo.write("packages/local/index.ts", "export const value = 1;");
    let result = repo.run_json("metrics", &[]);
    assert_eq!(result["resolution_summary"]["external_imports"], 1);
    assert_eq!(result["unresolved_dependencies"][0]["source"], "local");
    assert!(result["edges"].as_array().unwrap().is_empty());
}

#[test]
fn metrics_rust_inline_scope_and_lexical_shadowing_keep_binding_boundaries() {
    let repo = TestRepo::new();
    repo.create_dir_all("owner");
    repo.write(
        "Cargo.toml",
        "[package]\nname='demo'\nedition='2024'\n[dependencies]\next='1'\n",
    );
    repo.write("lib.rs", "mod local; mod owner { mod local; use local::Thing; } use ext::Other; fn f(){ use std as ext; use ext::Thing; }");
    repo.write("local.rs", "pub struct Thing;");
    repo.write("owner/local.rs", "pub struct Thing;");
    let result = repo.run_json("metrics", &[]);
    let uses: Vec<_> = result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|edge| edge["evidence"].as_array().unwrap())
        .filter(|proof| proof["source"] == "local::Thing")
        .collect();
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0]["target"], "owner/local.rs");
    assert_eq!(result["resolution_summary"]["external_imports"], 1);
    assert!(
        result["unresolved_dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["source"] == "ext::Thing")
    );
    repo.write("lib.rs", "extern crate other as ext; use ext::Thing;");
    let aliased = repo.run_json("metrics", &[]);
    assert_eq!(aliased["resolution_summary"]["external_imports"], 0);
    assert_eq!(
        aliased["resolution_summary"]["all_static_imports_resolved"],
        false
    );
}
