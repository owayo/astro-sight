use super::support::{TestRepo, cargo_bin};
use serde_json::{Value, json};

#[test]
fn typescript_same_function_export_clause_preserves_caller_compatibility() {
    let direct = "export function f(): number { return 1; }\n";
    let clause = "function f(): number { return 1; }\nexport { f };\n";
    for (before, after) in [(direct, clause), (clause, direct)] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("api.ts", before);
        repo.write(
            "use.ts",
            "import { f } from './api.js';\nimport * as api from './api.js';\nf();\napi.f();\n",
        );
        repo.commit_all("base");
        repo.write("api.ts", after);
        for staged in [false, true] {
            if staged {
                repo.stage_all();
            }
            let args = if staged {
                &["--git", "--staged"][..]
            } else {
                &["--git"][..]
            };
            let review = repo.run_json("review", args);
            let changes = &review["impact"]["changes"][0];
            assert_eq!(
                impact_evidence(&review, "api.ts", "f")["changed_parts"],
                json!(["visibility"]),
                "{review}"
            );
            assert!(
                changes["impacted_callers"]
                    .as_array()
                    .is_none_or(Vec::is_empty),
                "{review}"
            );
            for line in [2, 3] {
                assert!(
                    changes["informational_callers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|c| c["path"] == "use.ts" && c["line"] == line),
                    "{review}"
                );
            }
            let mut cmd = cargo_bin();
            cmd.args(["review", "--hook", "--dir"])
                .arg(repo.root())
                .args(args);
            let out = cmd.output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        repo.write("api.ts", after.replace("return 1", "return 2"));
        let (output, code) = hook(&repo, false);
        assert_eq!(code, 1, "{output}");
        assert!(
            output["blocking_categories"]
                .as_array()
                .unwrap()
                .contains(&json!("impacts")),
            "{output}"
        );
    }
}

#[test]
fn typescript_export_clause_keeps_changed_bindings_and_declaration_emit_blocking() {
    let repo = TestRepo::new();
    repo.init_git();
    let f = "function f(): number { return 1; }";
    let g = "export function g(): number { return 2; }";
    repo.write("api.ts", format!("export {f}\n{g}\n"));
    repo.write("use.ts", "import { f } from './api.js';\nf();\n");
    repo.write("other.ts", "export function f(): number { return 9; }\n");
    repo.commit_all("base");
    for after in [
        format!("{f}\nexport type {{f}};\n{g}"),
        format!("{f}\nexport {{type f}};\n{g}"),
        format!("{f}\nexport default f;\n{g}"),
        format!("{f}\nexport {{f as renamed}};\n{g}"),
        format!("{f}\nexport {{g as f}};\n{g}"),
        format!("{f}\nexport {{f}} from './other.js';\n{g}"),
        format!("{f}\nexport {{f}};\n{g}\nf = () => 9;"),
        format!("{f}\n/** @internal */ export {{f}};\n{g}"),
        // CommonJS では exports の挿入順も変わる。
        format!("{f}\n{g}\nexport {{f}};"),
    ] {
        repo.write("api.ts", &after);
        let (out, code) = hook(&repo, false);
        assert_eq!(code, 1, "{after}\n{out}");
    }
}

#[test]
fn typescript_export_clause_preserves_the_directive_prologue_after_a_shebang() {
    let repo = TestRepo::new();
    repo.init_git();
    let head = "#!/usr/bin/env node\n";
    let directive = "\"use client\";\n";
    let f = "function f(): number { return 1; }\n";
    repo.write("api.ts", format!("{head}{directive}export {f}"));
    repo.write("use.ts", "import {f} from './api.js';\nf();\n");
    repo.commit_all("base");
    for (after, expected) in [
        (format!("{head}{directive}export {{ f }};\n{f}"), 0),
        (format!("{head}export {{ f }};\n{directive}{f}"), 1),
    ] {
        repo.write("api.ts", after);
        let out = cargo_bin()
            .args(["review", "--git", "--hook", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(expected),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn typescript_export_clause_with_a_changed_imported_parameter_type_keeps_callers() {
    let repo = TestRepo::new();
    repo.init_git();
    let import = "import type {Input} from './types.js';\n";
    let f = "function f(x: Input): number { return 1; }\n";
    repo.write("types.ts", "export interface Input { value: string }\n");
    repo.write("api.ts", format!("{import}export {f}"));
    repo.write("use.ts", "import {f} from './api.js';\nf({value:'x'});\n");
    repo.commit_all("base");
    repo.write("types.ts", "export interface Input { value: number }\n");
    repo.write("api.ts", format!("{import}{f}export {{f}};\n"));
    for rename in [false, true] {
        if rename {
            repo.write("types.ts", "export interface Input { value: string }\n");
            repo.git(["mv", "types.ts", "renamed.ts"]);
        }
        let context = repo.run_json("context", &["--git"]);
        let api = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == "api.ts")
            .unwrap();
        assert!(
            api["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["path"] == "use.ts" && c["line"] == 1),
            "{context}"
        );
    }
}

fn fixture(old: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("convert.py", old);
    repo.write(
        "entry.py",
        "from convert import enabled\nprint(enabled(1))\n",
    );
    repo.commit_all("initial");
    repo
}

fn hook(repo: &TestRepo, staged: bool) -> (Value, i32) {
    let mut command = cargo_bin();
    command
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    if staged {
        command.arg("--staged");
    }
    let output = command.output().unwrap();
    let value = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    (value, output.status.code().unwrap())
}

fn impact_evidence<'a>(review: &'a Value, path: &str, name: &str) -> &'a Value {
    &impact_symbol(review, path, name)["change_evidence"]
}

fn impact_symbol<'a>(review: &'a Value, path: &str, name: &str) -> &'a Value {
    review["impact"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == path)
        .unwrap()["affected_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == name)
        .unwrap()
}

#[test]
fn change_evidence_annotation_only_keeps_blocking_and_staged_snapshot() {
    let repo = fixture("def enabled(value):\n    return bool(value)\n");
    repo.write(
        "convert.py",
        "def enabled(value) -> bool:\n    return bool(value)\n",
    );
    let normal = repo.run_json("review", &["--git"]);
    let evidence = &normal["api_changes"]["modified"][0]["change_evidence"];
    assert_eq!(evidence["analysis"], "known", "{normal}");
    assert_eq!(evidence["changed_parts"], json!(["return_annotation"]));
    assert_eq!(evidence["annotation_only"], true);
    assert_eq!(evidence["call_shape_unchanged"], true);
    assert_eq!(impact_evidence(&normal, "convert.py", "enabled"), evidence);
    for staged in [false, true] {
        if staged {
            repo.stage_all();
            repo.write(
                "convert.py",
                "def enabled(value) -> bool:\n    return not value\n",
            );
        }
        let (output, code) = hook(&repo, staged);
        assert_eq!(code, 1, "{output}");
        assert!(
            output["blocking_categories"]
                .as_array()
                .unwrap()
                .contains(&json!("api.mod")),
            "{output}"
        );
        assert_eq!(&output["api"]["mod"][0]["change_evidence"], evidence);
        assert_eq!(&output["change_evidence"][0]["change_evidence"], evidence);
    }
}

#[test]
fn change_evidence_mixed_body_and_decorator_are_not_annotation_only() {
    for after in [
        "def enabled(value) -> bool:\n    return not value\n",
        "@wrapper\ndef enabled(value) -> bool:\n    return bool(value)\n",
    ] {
        let repo = fixture("def enabled(value):\n    return bool(value)\n");
        repo.write("convert.py", after);
        let normal = repo.run_json("review", &["--git"]);
        let evidence = &normal["api_changes"]["modified"][0]["change_evidence"];
        assert_eq!(evidence["annotation_only"], false, "{normal}");
        let parts = evidence["changed_parts"].as_array().unwrap();
        assert!(parts.contains(&json!("return_annotation")));
        assert!(parts.contains(&json!(if after.starts_with('@') {
            "decorator"
        } else {
            "body"
        })));
        if after.starts_with('@') {
            assert!(evidence["call_shape_unchanged"].is_null());
        }
        let (output, code) = hook(&repo, false);
        assert_eq!(code, 1, "{output}");
        assert_eq!(&output["api"]["mod"][0]["change_evidence"], evidence);
    }
}

#[test]
fn change_evidence_body_and_initializer_survive_without_api_mod() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("api.ts", "export function enabled(value: number): boolean { return value > 0; }\nexport const FLAG: boolean = true;\n");
    repo.write(
        "entry.ts",
        "import { enabled, FLAG } from './api';\nconsole.log(enabled(1), FLAG);\n",
    );
    repo.commit_all("initial");
    repo.write("api.ts", "export function enabled(value: number): boolean { return value >= 0; }\nexport const FLAG: boolean = false;\n");
    let normal = repo.run_json("review", &["--git"]);
    assert!(
        normal["api_changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{normal}"
    );
    assert_eq!(
        impact_evidence(&normal, "api.ts", "enabled")["changed_parts"],
        json!(["body"]),
        "{normal}"
    );
    assert_eq!(
        impact_evidence(&normal, "api.ts", "FLAG")["changed_parts"],
        json!(["initializer"]),
        "{normal}"
    );
    assert_eq!(
        normal["api_changes"]["const_value_changes"][0]["change_evidence"]["changed_parts"],
        json!(["initializer"])
    );
    let (output, code) = hook(&repo, false);
    assert_eq!(code, 1, "{output}");
    assert_eq!(
        output["blocking_categories"],
        json!(["impacts"]),
        "{output}"
    );
    assert!(
        output["change_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["n"] == "enabled"
                && entry["change_evidence"]["changed_parts"] == json!(["body"]))
    );
    assert_eq!(
        output["api"]["const_value"][0]["change_evidence"]["changed_parts"],
        json!(["initializer"])
    );
    let output = cargo_bin()
        .args(["review", "--git", "--format", "toon", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    assert!(output.status.success());
    let toon = String::from_utf8(output.stdout).unwrap();
    assert!(
        toon.contains("change_evidence")
            && toon.contains("initializer")
            && toon.contains("call_shape_unchanged"),
        "{toon}"
    );

    let quiet = TestRepo::new();
    quiet.init_git();
    quiet.write("main.rs", "fn main() { println!(\"old\"); }\n");
    quiet.commit_all("initial");
    quiet.write("main.rs", "fn main() { println!(\"new\"); }\n");
    let (output, code) = hook(&quiet, false);
    assert_eq!(code, 0, "{output}");
    assert_eq!(output["blocking_categories"], json!([]));
    assert_eq!(output["change_evidence"][0]["n"], "main");
    assert_eq!(
        output["change_evidence"][0]["change_evidence"]["changed_parts"],
        json!(["body"])
    );
}

#[test]
fn change_evidence_reports_types_outside_the_comparison_scope() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "shape.ts",
        "export type Unit = \"px\" | \"em\";\n\nexport interface Shape {\n    readonly width: number;\n}\n",
    );
    repo.commit_all("initial");

    for (variants, categories, exit_code) in [
        ("\"px\" | \"em\" | \"rem\"", json!(["api.mod"]), 1),
        ("\"em\" | \"px\"", json!([]), 0),
    ] {
        repo.write(
            "shape.ts",
            format!("export type Unit = {variants};\n\nexport interface Shape {{\n    readonly width: number;\n    readonly unit?: Unit;\n}}\n"),
        );
        let normal = repo.run_json("review", &["--git"]);
        let (output, code) = hook(&repo, false);
        assert_eq!(code, exit_code, "{output}");
        assert_eq!(output["blocking_categories"], categories, "{output}");
        let expected = json!({
            "analysis":"unknown",
            "unknown_reason":"unsupported_declaration_kind",
            "changed_parts":[],
            "annotation_only":null,
            "call_shape_unchanged":null
        });
        for (name, kind) in [("Unit", "type"), ("Shape", "trait")] {
            let symbol = impact_symbol(&normal, "shape.ts", name);
            assert_eq!(symbol["kind"], kind, "{normal}");
            assert_eq!(symbol["change_type"], "modified", "{normal}");
            assert_eq!(symbol["change_evidence"], expected, "{normal}");
            let evidence = output["change_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["n"] == name && entry["k"] == kind)
                .unwrap();
            assert_eq!(evidence["change_evidence"], expected, "{output}");
        }
        let compatible = normal["api_changes"]["compatible_modified"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "Shape")
            .unwrap();
        assert_eq!(compatible["reason"], "optional_type_members");
        assert_eq!(compatible["change_evidence"], expected, "{normal}");
        let unit_bucket = if exit_code == 0 {
            "compatible_modified"
        } else {
            "modified"
        };
        let unit = normal["api_changes"][unit_bucket]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "Unit")
            .unwrap();
        assert_eq!(unit["change_evidence"], expected, "{normal}");
    }
}

#[test]
fn change_evidence_parse_error_keeps_unknown_boolean_values() {
    let repo = fixture("def enabled(value -> bool:\n    return value\n");
    repo.write(
        "convert.py",
        "def enabled(value) -> bool:\n    return value\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let evidence = &context["changes"][0]["affected_symbols"][0]["change_evidence"];
    assert_eq!(evidence["analysis"], "unknown", "{context}");
    assert_eq!(evidence["unknown_reason"], "parse_error");
    assert!(
        evidence
            .as_object()
            .unwrap()
            .contains_key("annotation_only")
    );
    assert!(
        evidence
            .as_object()
            .unwrap()
            .contains_key("call_shape_unchanged")
    );
    assert!(evidence["annotation_only"].is_null());
    assert!(evidence["call_shape_unchanged"].is_null());
}

fn assert_added_typescript_bindings(arrow: bool, replace_body: bool) {
    let repo = TestRepo::new();
    repo.init_git();
    let header = if arrow {
        "export const attach = (on: (event: string, hook: Hook) => void) => {"
    } else {
        "export function attach(on: (event: string, hook: Hook) => void) {"
    };
    let prefix = "type Hook = (name: string) => void\nconst seen = new Map<string, number>()\n";
    let old_body = "  on('start', (name) => seen.set(name, 1))\n";
    repo.write("events.ts", format!("{prefix}\n{header}\n{old_body}}}\n"));
    repo.write(
        "entry.ts",
        "import { attach } from './events'\nattach((event, hook) => hook(event))\n",
    );
    repo.commit_all("initial");
    let body = if replace_body {
        "  on('start', (name) => {\n    starts += 1\n    seen.set(name, starts)\n  })\n  on('stop', () => {\n    stops += 1\n  })\n"
            .to_owned()
    } else {
        format!("{old_body}  starts += 1\n  stops += 1\n")
    };
    repo.write(
        "events.ts",
        format!("{prefix}let starts = 0\nlet stops = 0\n\n{header}\n{body}}}\n"),
    );
    for staged in [false, true] {
        if staged {
            repo.stage_all();
        }
        let args = if staged {
            vec!["--git", "--staged"]
        } else {
            vec!["--git"]
        };
        let normal = repo.run_json("review", &args);
        let context = repo.run_json("context", &args);
        assert_eq!(normal["impact"]["changes"], context["changes"]);
        let symbols = normal["impact"]["changes"][0]["affected_symbols"]
            .as_array()
            .unwrap();
        for name in ["starts", "stops"] {
            let symbol = symbols
                .iter()
                .find(|symbol| symbol["name"] == name)
                .unwrap();
            assert_eq!(symbol["change_type"], "added", "{normal}");
            assert!(symbol.get("change_evidence").is_none(), "{symbol}");
        }
        let attach = symbols
            .iter()
            .find(|symbol| symbol["name"] == "attach")
            .unwrap();
        assert_eq!(attach["change_type"], "modified", "{attach}");
        assert_eq!(
            attach["change_evidence"]["changed_parts"],
            json!([if arrow { "initializer" } else { "body" }])
        );
        let (output, code) = hook(&repo, staged);
        assert_eq!(code, 0, "{output}");
        assert_eq!(output["blocking_categories"], json!([]));
        let evidence = output["change_evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 1, "{output}");
        assert_eq!(evidence[0]["n"], "attach");
        assert_eq!(evidence[0]["change_evidence"], attach["change_evidence"]);
        let impact = cargo_bin()
            .args(["impact", "--dir"])
            .arg(repo.root())
            .args(&args)
            .arg("--hook")
            .output()
            .unwrap();
        assert!(impact.status.success(), "{impact:?}");
    }
}

#[test]
fn change_evidence_added_typescript_bindings_in_replaced_function_hunk() {
    assert_added_typescript_bindings(false, true);
}

#[test]
fn change_evidence_added_typescript_bindings_in_replaced_arrow_hunk() {
    assert_added_typescript_bindings(true, true);
}

#[test]
fn change_evidence_added_typescript_bindings_in_addition_only_hunk() {
    assert_added_typescript_bindings(false, false);
}

#[test]
fn change_evidence_added_typescript_bindings_across_changed_files() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "shape.ts",
        "export function area(width: number): number { return width * width; }\n",
    );
    repo.write(
        "text.ts",
        "export function label(): string { return 'a'; }\n",
    );
    repo.write(
        "use.ts",
        "import { area } from './shape';\n\nexport const total: number = area(2);\n",
    );
    repo.commit_all("initial");
    repo.write("use.ts", "import { area as size } from './shape';\n\nexport const total: number = size(2);\nexport const wide = size(2) > 10;\n");

    let check = |staged| {
        let args = if staged {
            &["--git", "--staged"][..]
        } else {
            &["--git"][..]
        };
        let review = repo.run_json("review", args);
        let context = repo.run_json("context", args);
        assert_eq!(review["impact"]["changes"], context["changes"]);
        let added = impact_symbol(&review, "use.ts", "wide");
        assert_eq!(added["change_type"], "added", "{review}");
        assert!(added.get("change_evidence").is_none(), "{added}");
        assert_eq!(
            impact_symbol(&review, "use.ts", "total")["change_type"],
            "modified"
        );
        let (report, _) = hook(&repo, staged);
        let entries = report["change_evidence"].as_array().unwrap();
        assert!(
            entries.iter().any(|entry| entry["n"] == "total"),
            "{report}"
        );
        assert!(entries.iter().all(|entry| entry["n"] != "wide"), "{report}");
    };
    check(false);
    repo.write(
        "text.ts",
        "export function label(): string { return 'b'; }\n",
    );
    check(false);
    repo.stage_all();
    repo.write("text.ts", "export function label() {\n");
    check(true);
}

#[test]
fn change_evidence_added_typescript_bindings_check_the_other_old_declarations() {
    for (old, new, expected) in [
        ("export const wide = true;\n", None, "modified"),
        (
            "export function wide() { return true; }\n",
            Some("export function wide() { return false; }\n"),
            "added",
        ),
        (
            "export const broken = (\n",
            Some("export const repaired = true;\n"),
            "modified",
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("old.js", old);
        repo.write("use.ts", "export function status() { return 1; }\n");
        repo.commit_all("initial");
        if let Some(new) = new {
            repo.write("old.js", new);
        } else {
            repo.git(["rm", "old.js"]);
        }
        repo.write(
            "use.ts",
            "export function status() { return 2; }\nexport const wide = true;\n",
        );
        let review = repo.run_json("review", &["--git"]);
        let symbol = impact_symbol(&review, "use.ts", "wide");
        assert_eq!(symbol["change_type"], expected, "{old}: {review}");
        if expected == "modified" {
            assert_eq!(
                symbol["change_evidence"]["unknown_reason"], "declaration_pairing_unavailable",
                "{review}"
            );
        } else {
            assert!(symbol.get("change_evidence").is_none(), "{symbol}");
        }
    }
}

#[test]
fn change_evidence_added_typescript_bindings_keep_hunkless_moves_unproven() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("old.js", "export const wide = true;\n");
    repo.write("use.ts", "export function status() { return 1; }\n");
    repo.commit_all("initial");
    repo.git(["mv", "old.js", "moved.js"]);
    repo.write(
        "use.ts",
        "export function status() { return 2; }\nexport const wide = true;\n",
    );
    let review = repo.run_json("review", &["--git"]);
    let symbol = impact_symbol(&review, "use.ts", "wide");
    assert_eq!(symbol["change_type"], "modified", "{review}");
    assert_eq!(
        symbol["change_evidence"]["unknown_reason"], "declaration_pairing_unavailable",
        "{review}"
    );
}

#[test]
fn change_evidence_added_typescript_bindings_keep_ambiguous_and_kind_changes() {
    for (old, reason) in [
        (
            "let starts = 1;\nlet starts = 2;\n",
            Some("declaration_pairing_ambiguous"),
        ),
        (
            "function starts() {}\n",
            Some("declaration_pairing_unavailable"),
        ),
        ("class starts {}\n", Some("declaration_pairing_unavailable")),
        (
            "import { starts } from './source';\n",
            Some("declaration_pairing_unavailable"),
        ),
        ("const { starts } = source;\n", None),
        ("let starts = (", Some("parse_error")),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("events.ts", old);
        repo.commit_all("initial");
        repo.write("events.ts", "let starts = 0;\n");
        let review = repo.run_json("review", &["--git"]);
        let symbol = &review["impact"]["changes"][0]["affected_symbols"][0];
        assert_eq!(symbol["name"], "starts", "{old}: {review}");
        assert_eq!(symbol["change_type"], "modified", "{old}: {review}");
        assert_eq!(
            symbol["change_evidence"]["unknown_reason"],
            json!(reason),
            "{old}: {review}"
        );
    }
}

#[test]
fn change_evidence_added_typescript_bindings_keep_file_and_cross_file_moves() {
    for rename in [false, true] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "source.ts",
            "let starts = 1;\nexport function attach() {}\n",
        );
        repo.write("events.ts", "export function attach() { return 1; }\n");
        repo.commit_all("initial");
        if rename {
            repo.git(["mv", "source.ts", "moved.ts"]);
            repo.write("moved.ts", "let starts = 0;\nexport function attach() {}\n");
        } else {
            repo.write("source.ts", "export function attach() {}\n");
            repo.write(
                "events.ts",
                "let starts = 0;\nexport function attach() { return 0; }\n",
            );
        }
        let review = repo.run_json("review", &["--git"]);
        let path = if rename { "moved.ts" } else { "events.ts" };
        let file = review["impact"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == path)
            .unwrap();
        let symbol = file["affected_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|symbol| symbol["name"] == "starts")
            .unwrap();
        assert_eq!(symbol["change_type"], "modified", "{review}");
    }
}

#[test]
fn change_evidence_added_typescript_bindings_keep_signature_changes_blocking() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "events.ts",
        "export function attach(value: number) { return value; }\n",
    );
    repo.write(
        "entry.ts",
        "import { attach } from './events';\nattach(1);\n",
    );
    repo.commit_all("initial");
    repo.write(
        "events.ts",
        "let starts = 0;\nexport function attach(value: string) { return starts + value; }\n",
    );
    let (review, code) = hook(&repo, false);
    assert_eq!(code, 1, "{review}");
    assert!(
        review["blocking_categories"]
            .as_array()
            .unwrap()
            .contains(&json!("api.mod")),
        "{review}"
    );
    let impact = cargo_bin()
        .args(["impact", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .unwrap();
    assert_eq!(impact.status.code(), Some(1), "{impact:?}");
}
