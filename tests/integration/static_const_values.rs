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

fn rust_const_fixture(layout: &str, before: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    if layout != "no-cargo" {
        repo.write("Cargo.toml", "[package]\nname = \"scan-demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[features]\na = []\n");
    }
    if layout.ends_with("lib") {
        let visibility = if layout == "public-lib" { "pub " } else { "" };
        repo.write("src/lib.rs", format!("{visibility}mod policy;\npub fn read() -> u64 {{ unsafe {{ policy::WINDOW_LIMIT }} }}\n"));
    } else {
        repo.write("src/main.rs", "mod policy;\nfn main() { let value = unsafe { policy::WINDOW_LIMIT }; println!(\"{value}\"); }\n");
    }
    repo.write("src/policy.rs", before);
    repo.commit_all("initial");
    repo
}

#[test]
fn rust_internal_const_value_policy_is_independent_of_cargo_public_surface() {
    for layout in ["no-cargo", "bin", "private-lib", "public-lib"] {
        for binding in ["pub const", "pub static"] {
            let before = format!("{binding} WINDOW_LIMIT: u64 = 20;\n");
            let repo = rust_const_fixture(layout, &before);
            let sites = |refs: serde_json::Value| {
                refs["refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| serde_json::json!([r["path"], r["ln"], r["col"], r["kind"]]))
                    .collect::<Vec<_>>()
            };
            let before_refs = sites(repo.run_json("refs", &["--name", "WINDOW_LIMIT"]));
            assert!(before_refs.len() >= 2, "{before_refs:?}");
            repo.write("src/policy.rs", before.replace("= 20", "= 10 * 4"));
            assert_eq!(
                before_refs,
                sites(repo.run_json("refs", &["--name", "WINDOW_LIMIT"]))
            );
            let review = repo.run_json("review", &["--git"]);
            assert!(
                review["api_changes"]["const_value_changes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["name"] == "WINDOW_LIMIT"),
                "{layout}/{binding}: {review}"
            );
            for bucket in ["added", "removed", "modified"] {
                assert!(
                    review["api_changes"][bucket]
                        .as_array()
                        .is_none_or(|items| items.is_empty()),
                    "{layout}/{binding}: {review}"
                );
            }
            for strict in [false, true] {
                let (output, json) = hook(&repo, strict);
                assert_eq!(
                    output.status.code(),
                    Some(i32::from(strict)),
                    "{layout}/{binding}/{strict}: {json}"
                );
                assert_eq!(json["api"]["const_value"][0]["n"], "WINDOW_LIMIT", "{json}");
                if !strict {
                    assert!(
                        json["blocking_categories"].as_array().unwrap().is_empty(),
                        "{json}"
                    );
                } else {
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
            repo.stage_all();
            repo.write("src/policy.rs", "pub const UNRELATED: u64 = 0;\n");
            for strict in [false, true] {
                let mut command = cargo_bin();
                command
                    .args(["review", "--git", "--staged", "--hook", "--dir"])
                    .arg(repo.root());
                if strict {
                    command.arg("--strict-public-const-values");
                }
                let output = command.output().unwrap();
                let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(i32::from(strict)),
                    "staged {layout}/{binding}/{strict}: {json}"
                );
                assert_eq!(json["api"]["const_value"][0]["n"], "WINDOW_LIMIT", "{json}");
            }
        }
    }
}

#[test]
fn rust_internal_const_value_policy_keeps_shape_mutability_and_ambiguity_blocking() {
    for layout in ["bin", "private-lib", "public-lib"] {
        for (before, after) in [
            (
                "pub const WINDOW_LIMIT: u64 = 20;\n",
                "pub const WINDOW_LIMIT: u32 = 40;\n",
            ),
            (
                "pub const WINDOW_LIMIT: u64 = 20;\n",
                "pub(crate) const WINDOW_LIMIT: u64 = 40;\n",
            ),
            (
                "pub static mut WINDOW_LIMIT: u64 = 20;\n",
                "pub static mut WINDOW_LIMIT: u64 = 40;\n",
            ),
            (
                "#[cfg(feature = \"a\")]\npub const WINDOW_LIMIT: u64 = 20;\n#[cfg(not(feature = \"a\"))]\npub const WINDOW_LIMIT: u64 = 30;\n",
                "#[cfg(feature = \"a\")]\npub const WINDOW_LIMIT: u64 = 40;\n#[cfg(not(feature = \"a\"))]\npub const WINDOW_LIMIT: u64 = 30;\n",
            ),
        ] {
            let repo = rust_const_fixture(layout, before);
            repo.write("src/policy.rs", after);
            for strict in [false, true] {
                let (output, json) = hook(&repo, strict);
                assert_eq!(
                    output.status.code(),
                    Some(1),
                    "{layout}/{after}/{strict}: {json}"
                );
                assert!(
                    json["api"]["const_value"]
                        .as_array()
                        .is_none_or(|items| items.is_empty()),
                    "{json}"
                );
            }
        }
    }
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
fn static_const_values_untyped_array_count_changes_follow_element_shapes() {
    for extension in ["js", "ts", "tsx"] {
        for (old, new) in [
            ("['s', 'm', 'l']", "['s', 'm']"),
            ("[1]", "[2, 3]"),
            ("[1, 'a', 2]", "['b', 3, 4, 'c']"),
            ("{ sizes: ['s', 'm'] }", "{ sizes: ['l'] }"),
            ("[[1, 2], ['a']]", "[['b'], [3]]"),
            ("[[1], [2]]", "[[1, 2]]"),
        ] {
            let repo = TestRepo::new();
            repo.init_git();
            let file = format!("api.{extension}");
            repo.write(&file, format!("export const VALUE = {old};\n"));
            repo.write(
                format!("use.{extension}"),
                "import { VALUE } from './api';\nexport function read() { return VALUE; }\n",
            );
            repo.commit_all("initial");
            repo.write(&file, format!("export const VALUE = {new};\n"));

            let (output, json) = hook(&repo, false);
            assert!(
                output.status.success(),
                "{extension}: {old} -> {new}: {json}"
            );
            assert!(
                json["api"]["const_value"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|change| change["n"] == "VALUE"),
                "{extension}: {old} -> {new}: {json}"
            );
            assert!(
                json["impact_info"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty()),
                "{extension}: {old} -> {new}: {json}"
            );
            assert!(
                json["impacts"]
                    .as_array()
                    .is_none_or(|items| items.is_empty()),
                "{extension}: {old} -> {new}: {json}"
            );
            assert!(
                json["blocking_categories"]
                    .as_array()
                    .is_none_or(|items| items.is_empty()),
                "{extension}: {old} -> {new}: {json}"
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
        ("[1, 'a']", "[2]"),
        ("[]", "[1]"),
        ("[1]", "[]"),
        ("[1, 2] as const", "[3] as const"),
        ("[1 as const, 2 as const]", "[3 as const]"),
        ("{ sizes: [1, 2] } as const", "{ sizes: [3] } as const"),
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

/// 型注釈のないトップレベルの scalar const で値の種類が変わると、推論される公開型が変わる
/// (`TIMEOUT * 1000` が TS2362)。静的 container の葉の同じ変更と同じく blocking な api.mod に残し、
/// 種類が同じ値の変更だけを const_value (非 blocking) に降格する。
#[test]
fn static_const_values_top_level_scalar_kind_change_keeps_blocking_api_mod() {
    for (extension, old, new, blocking) in [
        ("ts", "30", "\"30s\"", true),
        ("ts", "'on'", "true", true),
        ("ts", "1", "1n", true),
        ("js", "30", "\"30s\"", true),
        ("tsx", "null", "0", true),
        // 対照: 同じ種類の値の変更は従来どおり const_value
        ("ts", "30", "45", false),
        ("js", "'a'", "'b'", false),
        ("tsx", "true", "false", false),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        let file = format!("config.{extension}");
        repo.write(&file, format!("export const TIMEOUT = {old};\n"));
        repo.write(
            format!("use.{extension}"),
            "import { TIMEOUT } from './config';\nexport const ms = TIMEOUT;\n",
        );
        repo.commit_all("initial");
        repo.write(&file, format!("export const TIMEOUT = {new};\n"));
        let (output, json) = hook(&repo, false);
        let in_bucket = |bucket: &str| {
            json["api"][bucket]
                .as_array()
                .into_iter()
                .flatten()
                .any(|change| change["n"] == "TIMEOUT")
        };
        if blocking {
            assert_eq!(
                output.status.code(),
                Some(1),
                "{extension}: {old} -> {new}: {json}"
            );
            assert!(in_bucket("mod"), "{extension}: {old} -> {new}: {json}");
            assert!(
                !in_bucket("const_value"),
                "{extension}: {old} -> {new}: {json}"
            );
        } else {
            assert!(
                output.status.success(),
                "{extension}: {old} -> {new}: {json}"
            );
            assert!(
                in_bucket("const_value"),
                "{extension}: {old} -> {new}: {json}"
            );
            assert!(!in_bucket("mod"), "{extension}: {old} -> {new}: {json}");
        }
    }
}
