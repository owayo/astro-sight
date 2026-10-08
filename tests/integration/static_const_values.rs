use super::support::{TestRepo, cargo_bin};

fn hook(repo: &TestRepo, strict: bool) -> (std::process::Output, serde_json::Value) {
    let mut command = cargo_bin();
    command
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    if strict {
        command.arg("--strict-public-const-values");
    }
    let output = command.output().unwrap();
    let json = if output.stderr.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
            panic!(
                "invalid hook output: {error}: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
    };
    (output, json)
}

#[test]
fn static_const_values_plain_sheet_use_existing_policy_and_api_bucket() {
    for extension in ["js", "ts", "tsx"] {
        let repo = TestRepo::new();
        repo.init_git();
        let file = format!("api.{extension}");
        let mut before =
            "export const PLAIN = { width: 297, margin: { top: 12, left: 14 } };\n".to_string();
        let mut usage =
            "import { PLAIN } from './api';\nexport function readPlain() { return PLAIN; }\n"
                .to_string();
        if extension != "js" {
            before.push_str(
                "export const SHEET = { width: 297, margin: { top: 12, left: 14 } } as const;\n",
            );
            usage.push_str(
                "import { SHEET } from './api';\nexport function readSheet() { return SHEET; }\n",
            );
        }
        repo.write(&file, &before);
        repo.write(format!("use.{extension}"), usage);
        repo.commit_all("initial");
        repo.write(
            &file,
            before.replace("top: 12, left: 14", "top: 10, left: 12"),
        );

        let api = repo.run_json("review", &["--git"]);
        let names: Vec<_> = api["api_changes"]["const_value_changes"]
            .as_array()
            .expect("const value bucket")
            .iter()
            .map(|change| change["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"PLAIN"), "{extension}: {api}");
        if extension != "js" {
            assert!(names.contains(&"SHEET"), "{extension}: {api}");
        }
        assert!(
            api["api_changes"]["modified"]
                .as_array()
                .is_none_or(|items| items.is_empty()),
            "{api}"
        );

        let (output, json) = hook(&repo, false);
        assert!(output.status.success(), "{extension}: {json}");
        assert!(
            json["api"]["const_value"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["n"] == "PLAIN"),
            "{json}"
        );
        assert!(
            json["blocking_categories"]
                .as_array()
                .is_none_or(|items| items.is_empty()),
            "{json}"
        );
        let (output, json) = hook(&repo, true);
        assert_eq!(output.status.code(), Some(1), "{extension}: {json}");
        assert!(
            json["api"]["const_value"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["n"] == "PLAIN"),
            "{json}"
        );
        assert!(
            json["blocking_categories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item
                    .as_str()
                    .is_some_and(|name| name.contains("const_value"))),
            "{json}"
        );
    }
}

#[test]
fn static_const_values_derived_literal_types_follow_value_policy_without_compatibility_guarantee() {
    for extension in ["ts", "tsx"] {
        let repo = TestRepo::new();
        repo.init_git();
        let file = format!("api.{extension}");
        let before = "export const ROLES = ['reader', 'writer'] as const;\nexport type Role = typeof ROLES[number];\n";
        repo.write(&file, before);
        repo.write(
            format!("use.{extension}"),
            "import { ROLES, type Role } from './api';\nexport function readRoles(): readonly Role[] { return ROLES; }\nexport const current: Role = 'reader';\n",
        );
        repo.commit_all("initial");
        repo.write(&file, before.replace("'reader'", "'viewer'"));

        let (output, json) = hook(&repo, false);
        assert!(output.status.success(), "{extension}: {json}");
        assert!(
            json["api"]["const_value"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["n"] == "ROLES"),
            "{json}"
        );
        assert!(
            !json["api"]["mod"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|change| change["n"] == "ROLES"),
            "{json}"
        );
        let (output, json) = hook(&repo, true);
        assert_eq!(output.status.code(), Some(1), "{extension}: {json}");
        assert!(
            json["api"]["const_value"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["n"] == "ROLES"),
            "{json}"
        );
        assert!(
            json["blocking_categories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item
                    .as_str()
                    .is_some_and(|name| name.contains("const_value"))),
            "{json}"
        );
    }
}

#[test]
fn static_const_values_formatting_is_not_reported_or_blocked_by_strict_policy() {
    for (before, after) in [
        (
            "export const VALUE = { a: 1 };\n",
            "\nexport   const   VALUE   =   {\n  a:   1\n};\n",
        ),
        (
            "export const VALUE = { a: 1 };\n",
            "export const VALUE = { a: /* ordinary */ 1 };\n",
        ),
        (
            "export const VALUE = { a: 1 };\n",
            "export const VALUE={a:1};\n",
        ),
        (
            "export const VALUE = { a: 1 };\n",
            "export const VALUE = { a: 1, };\n",
        ),
        (
            "export const VALUE = { a: 1 };\n",
            "export const VALUE = ({ a: 1 });\n",
        ),
        (
            "export const VALUE = { a: 'text' } as const;\n",
            "export const VALUE = { a: \"text\" } as const;\n",
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("api.ts", before);
        repo.write(
            "use.ts",
            "import { VALUE } from './api';\nexport function read() { return VALUE; }\n",
        );
        repo.commit_all("initial");
        repo.write("api.ts", after);
        let json = repo.run_json("review", &["--git"]);
        assert!(
            !json["api_changes"]["const_value_changes"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|change| change["name"] == "VALUE"),
            "{after}: {json}"
        );
        assert!(
            json["api_changes"]["modified"]
                .as_array()
                .is_none_or(|items| items.is_empty()),
            "{after}: {json}"
        );
        for strict in [false, true] {
            let (output, json) = hook(&repo, strict);
            assert!(output.status.success(), "{after}: {json}");
            assert!(
                json["blocking_categories"]
                    .as_array()
                    .is_none_or(|items| items.is_empty()),
                "{after}: {json}"
            );
        }
    }
}

#[test]
fn static_const_values_unsafe_changes_keep_blocking_api_mod() {
    for (old, new) in [
        ("{ a: 1 }", "{ a: 2, b: 3 }"),
        ("{ a: 1, b: 2 }", "{ b: 3, a: 4 }"),
        ("{ a: 1 }", "{ a: 'two' }"),
        ("{ a: 1 }", "{ a: 2n }"),
        ("{ a: 1 }", "{ a: 2 } as const"),
        ("{ a: 1 } as const", "{ a: 2 }"),
        ("{ a: 1 as const }", "{ a: 2 }"),
        ("{ a: 1, ...extra }", "{ a: 2, ...extra }"),
        ("{ a: 1, fn: () => 1 }", "{ a: 2, fn: () => 1 }"),
        ("{ a: 1, value: getValue() }", "{ a: 2, value: getValue() }"),
        ("{ a: 1, [key]: 2 }", "{ a: 2, [key]: 2 }"),
        ("{ a: 1, a: 2 }", "{ a: 3, a: 4 }"),
        ("{ __proto__: { a: 1 } }", "{ __proto__: { a: 2 } }"),
        ("[1, 2]", "[3, 4, 5]"),
        ("[1,,2]", "[3,,4]"),
        ("{ a: 1 } satisfies Shape", "{ a: 2 } satisfies Shape"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("api.ts", format!("export const VALUE = {old};\n"));
        repo.write(
            "use.ts",
            "import { VALUE } from './api';\nexport function read() { return VALUE; }\n",
        );
        repo.commit_all("initial");
        repo.write("api.ts", format!("export const VALUE = {new};\n"));
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{old} -> {new}: {json}");
        assert!(
            json["api"]["mod"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["n"] == "VALUE"),
            "{old} -> {new}: {json}"
        );
        assert!(
            !json["api"]["const_value"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|change| change["n"] == "VALUE"),
            "{json}"
        );
    }
}
