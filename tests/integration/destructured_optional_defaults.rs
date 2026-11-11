use super::support::{TestRepo, cargo_bin};

fn review_default(
    prelude: &str,
    ty: &str,
    value: &str,
) -> (std::process::Output, serde_json::Value) {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("tone.ts", "export type Tone = 'plain' | 'muted';\n");
    repo.write("panel.tsx", format!("{prelude}\nexport function Panel({{ label }}: {{ label: string }}) {{ return <p>{{label}}</p>; }}\n"));
    repo.write("home.tsx", "import { Panel } from './panel';\nexport function Home() { return <Panel label='Name' />; }\n");
    repo.commit_all("initial");
    repo.write("panel.tsx", format!("{prelude}\nexport function Panel({{ label, tone = {value} }}: {{ label: string; tone?: {ty} }}) {{ return <p data-tone={{tone}}>{{label}}</p>; }}\n"));
    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    let result = serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
        panic!(
            "hook JSON: {error}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, result)
}

fn compatible(result: &serde_json::Value) -> bool {
    result["api"]["mod_compat"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["n"] == "Panel" && item["reason"] == "optional_object_props")
}

#[test]
fn destructured_optional_defaults_literal_unions_are_compatible() {
    for (ty, value) in [("'plain' | 'muted'", "'plain'"), ("1 | 2", "1")] {
        let (output, result) = review_default("", ty, value);
        assert!(output.status.success(), "{ty}: {result}");
        assert!(compatible(&result), "{ty}: {result}");
        assert!(result["impacts"].is_null(), "{ty}: {result}");
        assert!(result["impact_info"].is_array(), "{ty}: {result}");
    }
}

#[test]
fn destructured_optional_defaults_local_alias_is_compatible() {
    let (output, result) = review_default("type Tone = 'plain' | 'muted';", "Tone", "'plain'");
    assert!(output.status.success(), "{result}");
    assert!(compatible(&result), "{result}");
    assert!(result["impacts"].is_null(), "{result}");
}

#[test]
fn destructured_optional_defaults_unknown_and_mismatched_types_stay_blocking() {
    for (prelude, ty, value) in [
        ("", "'plain' | 'muted'", "'loud'"),
        ("import type { Tone } from './tone';", "Tone", "'plain'"),
    ] {
        let (output, result) = review_default(prelude, ty, value);
        assert_eq!(output.status.code(), Some(1), "{prelude}/{ty}: {result}");
        assert!(!compatible(&result), "{prelude}/{ty}: {result}");
        assert!(
            result["api"]["mod"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| item["n"] == "Panel"),
            "{prelude}/{ty}: {result}"
        );
    }
}
