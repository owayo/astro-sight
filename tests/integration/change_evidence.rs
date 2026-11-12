use super::support::{TestRepo, cargo_bin};
use serde_json::{Value, json};

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
    &review["impact"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == path)
        .unwrap()["affected_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == name)
        .unwrap()["change_evidence"]
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
fn change_evidence_added_typescript_bindings_keep_ambiguous_and_kind_changes() {
    for (old, reason) in [
        (
            "let starts = 1;\nlet starts = 2;\n",
            Some("declaration_pairing_ambiguous"),
        ),
        (
            "function starts() {}\n",
            Some("declaration_pairing_ambiguous"),
        ),
        ("class starts {}\n", Some("declaration_pairing_ambiguous")),
        (
            "import { starts } from './source';\n",
            Some("declaration_pairing_ambiguous"),
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
