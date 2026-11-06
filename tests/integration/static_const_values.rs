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

/// 非公開の Rust の const / static の fixture (Issue #257)。`src/notes/greeting.rs` を変え、親の
/// `src/notes.rs` が `greeting::GREETING` と `greeting::LIMIT` を関数の本体で読む。
fn rust_private_value_fixture(greeting: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src/notes");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"notes\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    repo.write(
        "src/lib.rs",
        "mod notes;\n\npub struct Note {\n    pub name: &'static str,\n    pub text: &'static str,\n}\n\npub fn all() -> Vec<&'static Note> {\n    notes::all()\n}\n\npub fn limit() -> u32 {\n    notes::limit()\n}\n",
    );
    repo.write(
        "src/notes.rs",
        "mod greeting;\n\npub(crate) fn all() -> Vec<&'static crate::Note> {\n    vec![&greeting::GREETING]\n}\n\npub(crate) fn limit() -> u32 {\n    greeting::LIMIT\n}\n",
    );
    repo.write("src/notes/greeting.rs", greeting);
    repo.commit_all("initial");
    repo
}

const PRIVATE_GREETING: &str = "use crate::Note;\n\npub(super) static GREETING: Note = Note {\n    name: \"greeting\",\n    text: \"Hello.\",\n};\n\npub(super) const LIMIT: u32 = 10;\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn has_a_name() {\n        assert_eq!(GREETING.name, \"greeting\");\n    }\n}\n";

/// `review --git` の通常出力で、`symbol` の参照が `bucket` (`impacted_callers` /
/// `informational_callers`) にある `src/notes.rs` の行。
fn notes_callers(review: &serde_json::Value, bucket: &str, symbol: &str) -> Vec<u64> {
    review["impact"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|change| change[bucket].as_array().cloned().unwrap_or_default())
        .filter(|caller| {
            caller["path"] == "src/notes.rs"
                && caller["symbols"]
                    .as_array()
                    .is_some_and(|symbols| symbols.iter().any(|name| name == symbol))
        })
        .map(|caller| caller["line"].as_u64().unwrap())
        .collect()
}

#[test]
fn rust_private_value_policy_is_independent_of_other_changes_in_the_file() {
    // 同じファイルのほかの差分 (テストの追加・テストの `use` の追加・テストの外の関数の追加)。
    // どれも変更した定数の名前を変更行に含む。
    let extras = [
        ("none", ""),
        (
            "test",
            "\n    #[test]\n    fn has_a_text() {\n        assert!(!GREETING.text.is_empty() && LIMIT > 0);\n    }\n",
        ),
        (
            "test-use",
            "\n    use std::collections::HashSet;\n\n    #[test]\n    fn has_a_text() {\n        let seen: HashSet<u32> = [LIMIT].into();\n        assert!(!GREETING.text.is_empty() && !seen.is_empty());\n    }\n",
        ),
        ("outside", ""),
    ];
    for (symbol, line, from, to) in [
        ("GREETING", 3, "text: \"Hello.\"", "text: \"Hello, world.\""),
        ("LIMIT", 7, "LIMIT: u32 = 10;", "LIMIT: u32 = 10 * 4;"),
    ] {
        for (extra, test) in extras {
            let repo = rust_private_value_fixture(PRIVATE_GREETING);
            let mut after = PRIVATE_GREETING
                .replace(from, to)
                .replace("    }\n}\n", &format!("    }}\n{test}}}\n"));
            if extra == "outside" {
                after = after.replace(
                    "\n#[cfg(test)]",
                    "\npub(super) fn greeting_text() -> &'static str {\n    if LIMIT > 0 { GREETING.text } else { \"\" }\n}\n\n#[cfg(test)]",
                );
            }
            repo.write("src/notes/greeting.rs", &after);
            let case = format!("{symbol}/{extra}");

            // 通常出力: 参照は消さず、修正の要らない情報提供の参照に分ける。
            let review = repo.run_json("review", &["--git"]);
            assert_eq!(
                notes_callers(&review, "impacted_callers", symbol),
                Vec::<u64>::new(),
                "{case}: {review}"
            );
            assert_eq!(
                notes_callers(&review, "informational_callers", symbol),
                [line],
                "{case}: {review}"
            );
            // hook: 既定でも strict (公開定数の方針) でも止めない。
            let stricts: &[bool] = if extra == "none" {
                &[false, true]
            } else {
                &[false]
            };
            for &strict in stricts {
                let (output, json) = hook(&repo, strict);
                assert_eq!(output.status.code(), Some(0), "{case}/{strict}: {json}");
                assert!(json.get("impacts").is_none(), "{case}/{strict}: {json}");
                assert!(
                    json["blocking_categories"]
                        .as_array()
                        .is_some_and(|items| items.is_empty()),
                    "{case}/{strict}: {json}"
                );
                assert!(
                    json["impact_info"].as_array().is_some_and(|groups| {
                        groups.iter().any(|group| {
                            group["refs"].as_array().is_some_and(|refs| {
                                refs.iter()
                                    .any(|r| r["p"] == "src/notes.rs" && r["ln"] == line)
                            })
                        })
                    }),
                    "{case}/{strict}: {json}"
                );
            }
        }
    }

    // index の内容を解析する `--staged` も同じ分類にする (作業ツリーの未ステージの変更に依らない)。
    let repo = rust_private_value_fixture(PRIVATE_GREETING);
    repo.write(
        "src/notes/greeting.rs",
        PRIVATE_GREETING
            .replace("text: \"Hello.\"", "text: \"Hello, world.\"")
            .replace(
                "    }\n}\n",
                "    }\n\n    #[test]\n    fn has_a_text() {\n        assert!(!GREETING.text.is_empty());\n    }\n}\n",
            ),
    );
    repo.stage_all();
    repo.write(
        "src/notes/greeting.rs",
        "pub(super) const UNRELATED: u32 = 0;\n",
    );
    let output = cargo_bin()
        .args(["review", "--git", "--staged", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(0), "staged: {json}");
    assert!(json.get("impacts").is_none(), "staged: {json}");

    // 対照: テストだけの追加は従来どおり何も出さない。
    let repo = rust_private_value_fixture(PRIVATE_GREETING);
    repo.write(
        "src/notes/greeting.rs",
        PRIVATE_GREETING.replace(
            "    }\n}\n",
            "    }\n\n    #[test]\n    fn has_a_text() {\n        assert!(!GREETING.text.is_empty());\n    }\n}\n",
        ),
    );
    let (output, json) = hook(&repo, false);
    assert_eq!(output.status.code(), Some(0), "test only: {json}");
    assert_eq!(json, serde_json::Value::Null, "test only");
}

#[test]
fn rust_private_value_policy_keeps_contract_changes_and_compile_time_uses_blocking() {
    // 宣言の変更 (型・可視性・可変性・種類・属性・同名の条件付き宣言) と、初期値の変更でも参照の
    // 修正が要りうる位置 (パターン・配列長・const の借用) は blocking のまま。
    let conditional = PRIVATE_GREETING.replace(
        "pub(super) const LIMIT: u32 = 10;",
        "#[cfg(feature = \"a\")]\npub(super) const LIMIT: u32 = 10;\n#[cfg(not(feature = \"a\"))]\npub(super) const LIMIT: u32 = 30;",
    );
    for (case, base, from, to, caller) in [
        (
            "type",
            PRIVATE_GREETING,
            "LIMIT: u32 = 10;",
            "LIMIT: u64 = 20;",
            None,
        ),
        (
            "visibility",
            PRIVATE_GREETING,
            "pub(super) const LIMIT",
            "const LIMIT",
            None,
        ),
        (
            "kind",
            PRIVATE_GREETING,
            "pub(super) const LIMIT: u32 = 10;",
            "pub(super) static LIMIT: u32 = 20;",
            None,
        ),
        (
            "mutability",
            PRIVATE_GREETING,
            "pub(super) static GREETING",
            "pub(super) static mut GREETING",
            None,
        ),
        (
            "attribute",
            PRIVATE_GREETING,
            "pub(super) const LIMIT: u32 = 10;",
            "#[cfg(feature = \"a\")]\npub(super) const LIMIT: u32 = 20;",
            None,
        ),
        (
            "conditional",
            conditional.as_str(),
            "LIMIT: u32 = 10;",
            "LIMIT: u32 = 20;",
            None,
        ),
        (
            "pattern",
            PRIVATE_GREETING,
            "LIMIT: u32 = 10;",
            "LIMIT: u32 = 20;",
            Some(
                "pub(crate) fn limit() -> u32 {\n    match 10 {\n        greeting::LIMIT => 1,\n        _ => 0,\n    }\n}\n",
            ),
        ),
        (
            "array length",
            PRIVATE_GREETING,
            "LIMIT: u32 = 10;",
            "LIMIT: u32 = 20;",
            Some(
                "pub(crate) fn limit() -> u32 {\n    vec![[0u8; greeting::LIMIT as usize]].len() as u32\n}\n",
            ),
        ),
        (
            "const borrow",
            PRIVATE_GREETING,
            "LIMIT: u32 = 10;",
            "LIMIT: u32 = 20;",
            Some("pub(crate) fn limit() -> u32 {\n    *&greeting::LIMIT\n}\n"),
        ),
        (
            "const method receiver",
            PRIVATE_GREETING,
            "LIMIT: u32 = 10;",
            "LIMIT: u32 = 20;",
            Some("pub(crate) fn limit() -> u32 {\n    greeting::LIMIT.pow(1)\n}\n"),
        ),
    ] {
        let repo = rust_private_value_fixture(base);
        if let Some(caller) = caller {
            repo.write(
                "src/notes.rs",
                format!(
                    "mod greeting;\n\npub(crate) fn all() -> Vec<&'static crate::Note> {{\n    vec![&greeting::GREETING]\n}}\n\n{caller}"
                ),
            );
            repo.commit_all("caller");
        }
        repo.write("src/notes/greeting.rs", base.replacen(from, to, 1));
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{case}: {json}");
        assert!(
            json["impacts"].as_array().is_some_and(|groups| {
                groups.iter().any(|group| {
                    group["refs"]
                        .as_array()
                        .is_some_and(|refs| refs.iter().any(|r| r["p"] == "src/notes.rs"))
                })
            }),
            "{case}: {json}"
        );
    }

    // 型の名前の宣言を差分のほかのファイルが変えた場合も、同じ字面の型の意味が変わりうるので
    // blocking に戻す (`crate::Note` のフィールドの名前の変更と、それに合わせた初期値の変更)。
    // 修正前は、変更行に `GREETING` が無いので参照を探さず、壊れる `GREETING.text` を見逃していた。
    let repo = rust_private_value_fixture(PRIVATE_GREETING);
    repo.write(
        "src/notes.rs",
        "mod greeting;\n\npub(crate) fn all() -> Vec<&'static crate::Note> {\n    vec![&greeting::GREETING]\n}\n\npub(crate) fn limit() -> u32 {\n    greeting::GREETING.text.len() as u32\n}\n",
    );
    repo.commit_all("caller");
    repo.write(
        "src/lib.rs",
        "mod notes;\n\npub struct Note {\n    pub name: &'static str,\n    pub body: &'static str,\n}\n\npub fn all() -> Vec<&'static Note> {\n    notes::all()\n}\n\npub fn limit() -> u32 {\n    notes::limit()\n}\n",
    );
    repo.write(
        "src/notes/greeting.rs",
        PRIVATE_GREETING.replace("text: \"Hello.\"", "body: \"Hello.\""),
    );
    let (output, json) = hook(&repo, false);
    assert_eq!(output.status.code(), Some(1), "cross-file type: {json}");
    assert!(
        json["impacts"].as_array().is_some_and(|groups| {
            groups.iter().any(|group| {
                group["syms"]
                    .as_array()
                    .is_some_and(|syms| syms.contains(&"GREETING".into()))
            })
        }),
        "cross-file type: {json}"
    );

    // 対照: 無修飾 `pub` の static は従来どおり API 差分の const_value の方針 (既定 0 / strict 1)。
    let repo = rust_private_value_fixture(
        &PRIVATE_GREETING.replace("pub(super) static GREETING", "pub static GREETING"),
    );
    repo.write(
        "src/notes/greeting.rs",
        PRIVATE_GREETING
            .replace("pub(super) static GREETING", "pub static GREETING")
            .replace("text: \"Hello.\"", "text: \"Hello, world.\"")
            .replace(
                "    }\n}\n",
                "    }\n\n    #[test]\n    fn has_a_text() {\n        assert!(!GREETING.text.is_empty());\n    }\n}\n",
            ),
    );
    for strict in [false, true] {
        let (output, json) = hook(&repo, strict);
        assert_eq!(
            output.status.code(),
            Some(i32::from(strict)),
            "pub/{strict}: {json}"
        );
        assert_eq!(json["api"]["const_value"][0]["n"], "GREETING", "{json}");
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
