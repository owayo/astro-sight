//! 公開 const の正規表現・静的テンプレートの値変更と hook policy。

use super::support::*;

fn hook(repo: &TestRepo, staged: bool, strict: bool) -> (std::process::Output, serde_json::Value) {
    let mut cmd = cargo_bin();
    cmd.args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    if staged {
        cmd.arg("--staged");
    }
    if strict {
        cmd.arg("--strict-public-const-values");
    }
    let output = cmd.output().unwrap();
    let json = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    (output, json)
}

fn occurrences(repo: &TestRepo) -> Vec<serde_json::Value> {
    repo.run_json("refs", &["--name", "VERSION_TAG"])["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reference| {
            serde_json::json!([
                reference["path"],
                reference["ln"],
                reference["col"],
                reference["kind"]
            ])
        })
        .collect()
}

#[test]
fn literal_const_values_regex_and_templates_follow_default_and_strict_policy() {
    for extension in ["js", "ts", "tsx"] {
        for staged in [false, true] {
            let repo = TestRepo::new();
            repo.init_git();
            let file = format!("values.{extension}");
            let before = "// 日本語とCRLFの位置を保持\r\nexport const VERSION_TAG = /^v\\d+$/;\r\nexport const ISO_DATE =\r\n    /^(?<year>\\d{4})-(?<month>\\d{2})$/;\r\nexport const GREETING = `hello`;\r\nexport const LABEL = \"hello\";\r\n";
            repo.write(&file, before);
            repo.write(format!("caller.{extension}"),
                "import { VERSION_TAG, ISO_DATE, GREETING, LABEL } from './values';\nexport function describe(input) {\n    if (VERSION_TAG.test(input)) return GREETING;\n    return ISO_DATE.test(input) ? LABEL : input;\n}\n");
            repo.commit_all("initial");
            let refs_before = occurrences(&repo);
            repo.write(
                &file,
                before
                    .replace("/^v\\d+$/", "/^v\\d+(?:\\.\\d+)?$/g")
                    .replace("(?<month>\\d{2})$/", "(?<month>\\d{2})Z?$/y")
                    .replace("`hello`", "`hello, world`")
                    .replace("\"hello\"", "\"hello, world\""),
            );
            if staged {
                repo.stage_all();
            }
            assert_eq!(refs_before, occurrences(&repo));
            let args = if staged {
                &["--git", "--staged"][..]
            } else {
                &["--git"][..]
            };
            let review = repo.run_json("review", args);
            let changes = review["api_changes"]["const_value_changes"]
                .as_array()
                .unwrap();
            assert_eq!(changes.len(), 4, "{review}");
            assert!(
                review["api_changes"]["modified"]
                    .as_array()
                    .unwrap()
                    .is_empty(),
                "{review}"
            );
            for strict in [false, true] {
                let (output, json) = hook(&repo, staged, strict);
                assert_eq!(
                    output.status.success(),
                    !strict,
                    "{extension}, staged={staged}, strict={strict}: {json}"
                );
                assert_eq!(
                    json["api"]["const_value"].as_array().unwrap().len(),
                    4,
                    "{json}"
                );
                assert!(json["api"]["mod"].is_null(), "{json}");
                if strict {
                    assert!(
                        json["blocking_categories"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|category| category == "api.const_value"),
                        "{json}"
                    );
                } else {
                    assert!(json["impacts"].is_null(), "{json}");
                    assert!(
                        json["blocking_categories"].as_array().unwrap().is_empty(),
                        "{json}"
                    );
                }
            }
        }
    }
}

#[test]
fn literal_const_values_keep_different_kinds_dynamic_templates_and_containers_blocking() {
    for extension in ["js", "ts", "tsx"] {
        for (before, after) in [
            ("/a/", "'b'"),
            ("/a/", "`b`"),
            ("`a`", "42"),
            ("`a${suffix}`", "`b${suffix}`"),
            ("String.raw`a`", "String.raw`b`"),
            ("{ pattern: /a/ }", "{ pattern: /b/ }"),
            ("[/a/]", "[/b/]"),
            ("{ text: `a` }", "{ text: `b` }"),
            ("'a' as const", "'b' as const"),
            ("'a' satisfies string", "'b' satisfies string"),
        ] {
            if extension == "js" && (before.contains(" as ") || before.contains(" satisfies ")) {
                continue;
            }
            let repo = TestRepo::new();
            repo.init_git();
            let file = format!("values.{extension}");
            let source =
                |value: &str| format!("const suffix = 'x';\nexport const VALUE = {value};\n");
            repo.write(&file, source(before));
            repo.write(
                format!("caller.{extension}"),
                "import { VALUE } from './values';\nexport function read() { return VALUE; }\n",
            );
            repo.commit_all("initial");
            repo.write(&file, source(after));
            let (output, json) = hook(&repo, false, false);
            assert_eq!(
                output.status.code(),
                Some(1),
                "{extension}: {before} -> {after}: {json}"
            );
            assert!(json["api"]["const_value"].is_null(), "{json}");
            assert_eq!(json["api"]["mod"][0]["n"], "VALUE", "{json}");
        }
    }
}

#[test]
fn literal_const_values_templates_share_string_kind_and_regex_annotations_keep_policy() {
    for (before, after) in [
        ("'a'", "`b`"),
        ("`a`", "'b'"),
        ("``", "`b`"),
        ("`a\\${suffix}`", "`b\\n`"),
    ] {
        for extension in ["js", "ts", "tsx"] {
            let repo = TestRepo::new();
            repo.init_git();
            let file = format!("values.{extension}");
            repo.write(&file, format!("export const VALUE = {before};\n"));
            repo.write(
                format!("caller.{extension}"),
                "import { VALUE } from './values';\nexport function read() { return VALUE; }\n",
            );
            repo.commit_all("initial");
            repo.write(&file, format!("export const VALUE = {after};\n"));
            let (output, json) = hook(&repo, false, false);
            assert!(
                output.status.success(),
                "{extension}: {before} -> {after}: {json}"
            );
            assert_eq!(json["api"]["const_value"][0]["n"], "VALUE", "{json}");
        }
    }
    for extension in ["ts", "tsx"] {
        let repo = TestRepo::new();
        repo.init_git();
        let file = format!("values.{extension}");
        let before = "export const TYPED: RegExp = /a/;\nexport const UNTYPED = /a/;\n";
        repo.write(&file, before);
        repo.write(
            format!("caller.{extension}"),
            "import { TYPED, UNTYPED } from './values';\nTYPED.test('a'); UNTYPED.test('a');\n",
        );
        repo.commit_all("initial");
        repo.write(&file, before.replace("/a/", "/a/g"));
        let (output, json) = hook(&repo, false, false);
        assert!(output.status.success(), "{json}");
        assert_eq!(
            json["api"]["const_value"].as_array().unwrap().len(),
            2,
            "{json}"
        );
    }
}
