use super::support::{TestRepo, cargo_bin};

fn review_default(
    prelude: &str,
    ty: &str,
    value: &str,
) -> (std::process::Output, serde_json::Value) {
    run_review(
        &default_repo(prelude, prelude, &format!("tone?: {ty}"), value, &[]),
        false,
    )
}

fn default_repo(
    old_prelude: &str,
    prelude: &str,
    prop: &str,
    value: &str,
    files: &[(&str, &str)],
) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("tone.ts", "export type Tone = 'plain' | 'muted';\n");
    repo.write("panel.tsx", format!("{old_prelude}\nexport function Panel({{ label }}: {{ label: string }}) {{ return <p>{{label}}</p>; }}\n"));
    repo.write("home.tsx", "import { Panel } from './panel';\nexport function Home() { return <Panel label='Name' />; }\n");
    for (path, text) in files {
        repo.write(path, text);
    }
    repo.commit_all("initial");
    repo.write("panel.tsx", format!("{prelude}\nexport function Panel({{ label, tone = {value} }}: {{ label: string; {prop} }}) {{ return <p data-tone={{tone}}>{{label}}</p>; }}\n"));
    repo
}

fn run_review(repo: &TestRepo, staged: bool) -> (std::process::Output, serde_json::Value) {
    let mut command = cargo_bin();
    command.args(["review", "--git", "--hook"]);
    if staged {
        command.arg("--staged");
    }
    let output = command.arg("--dir").arg(repo.root()).output().unwrap();
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
        ("import type { Tone } from './tone';", "Tone", "'loud'"),
        ("import type { Tone } from './missing';", "Tone", "'plain'"),
        ("import { Tone } from './tone';", "Tone", "'plain'"),
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

const INDEXED: &str = "const tones = [{ value: 'plain' }, { value: 'muted' }] as const;\ntype Tone = (typeof tones)[number]['value'];";

#[test]
fn destructured_optional_defaults_prove_imported_alias_and_indexed_literals() {
    for (prelude, prop, value) in [
        (
            "import type { Tone } from './tone';",
            "tone?: Tone",
            "'plain'",
        ),
        (
            "import type { Tone as T } from './tone.js';",
            "tone?: T",
            "\"muted\"",
        ),
        (
            "import type { Tone } from './tone.ts';",
            "tone?: Tone | null",
            "null",
        ),
        (INDEXED, "tone?: Tone", "\"plain\""),
    ] {
        let repo = default_repo("", prelude, prop, value, &[]);
        repo.stage_all();
        for staged in [false, true] {
            if staged {
                repo.write("tone.ts", "export type Tone = 'other';");
                repo.write("panel.tsx", "export {};");
            }
            let (output, result) = run_review(&repo, staged);
            assert!(output.status.success(), "{prelude}/{staged}: {result}");
            assert!(compatible(&result), "{prelude}/{staged}: {result}");
            assert!(result["impacts"].is_null(), "{result}");
        }
    }
}

#[test]
fn destructured_optional_defaults_keep_unproven_projections_and_required_props_blocking() {
    for (prelude, prop, value) in [
        (INDEXED.to_owned(), "tone?: Tone", "'loud'"),
        (
            INDEXED.replace("value: 'muted'", "name: 'muted'"),
            "tone?: Tone",
            "'plain'",
        ),
        (
            INDEXED.replace("value: 'muted'", "...makeTone()"),
            "tone?: Tone",
            "'plain'",
        ),
        (INDEXED.to_owned(), "tone: Tone", "'plain'"),
        (
            "import type { Tone } from './tone';".to_owned(),
            "tone: Tone",
            "'plain'",
        ),
    ] {
        let (output, result) = run_review(&default_repo("", &prelude, prop, value, &[]), false);
        assert_eq!(output.status.code(), Some(1), "{prelude}/{prop}: {result}");
        assert!(!compatible(&result), "{result}");
    }
}

#[test]
fn destructured_optional_defaults_require_one_exported_type_at_the_import_target() {
    for (path, text) in [
        ("tone.ts", "type Tone = 'plain'; export {};"),
        ("tone.ts", "export type Tone<T> = 'plain';"),
        ("tone.ts", "export type Tone = Missing | 'plain';"),
        ("tone.d.ts", "export type Tone = 'other';"),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"moduleSuffixes":[".native",""]}}"#,
        ),
    ] {
        let repo = default_repo(
            "",
            "import type { Tone } from './tone';",
            "tone?: Tone",
            "'plain'",
            &[(path, text)],
        );
        let (output, result) = run_review(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{path}/{text}: {result}");
        assert!(
            result["api"]["mod"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["n"] == "Panel"),
            "{result}"
        );
    }
}

#[test]
fn destructured_optional_defaults_require_erased_import_for_effectful_module() {
    for (prelude, safe) in [
        ("import { type Tone } from './tone';", false),
        ("import type { Tone } from './tone';", true),
    ] {
        let repo = default_repo(
            "",
            prelude,
            "tone?: Tone",
            "'plain'",
            &[(
                "tone.ts",
                "console.log('loaded');\nexport type Tone = 'plain' | 'muted';",
            )],
        );
        let (output, result) = run_review(&repo, false);
        assert_eq!(output.status.code(), Some(i32::from(!safe)), "{result}");
        assert_eq!(compatible(&result), safe, "{result}");
    }
}
