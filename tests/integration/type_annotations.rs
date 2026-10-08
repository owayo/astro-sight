use super::support::{TestRepo, cargo_bin};

fn hook(repo: &TestRepo, policy_flags: &[&str]) -> (std::process::Output, serde_json::Value) {
    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .args(policy_flags)
        .output()
        .unwrap();
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
fn type_annotation_only_area_uses_unverified_policy_and_independent_strict_flag() {
    for extension in ["ts", "tsx"] {
        for (old_annotation, new_annotation) in
            [("", ": Dim"), (": Dim", ""), (": Dim", ": Readonly<Dim>")]
        {
            let repo = TestRepo::new();
            repo.init_git();
            let file = format!("area.{extension}");
            let source = |annotation| {
                format!(
                    "export type Dim = {{ width: number; height: number }};\nexport const AREA{annotation} = {{ width: 100 * 2, height: 50 * 2 }};\n"
                )
            };
            repo.write(&file, source(old_annotation));
            repo.write(
                format!("use.{extension}"),
                "import { AREA } from './area';\nexport function half(): number { return AREA.width / 2 + AREA.height / 2; }\n",
            );
            repo.commit_all("initial");
            repo.write(&file, source(new_annotation));
            let api = repo.run_json("review", &["--git"]);
            let changes = api["api_changes"]["type_annotation_changes"]
                .as_array()
                .expect("type annotation bucket");
            assert_eq!(changes.len(), 1, "{api}");
            assert_eq!(changes[0]["name"], "AREA", "{api}");
            assert_eq!(changes[0]["reason"], "type_annotation_only", "{api}");
            assert_eq!(changes[0]["compatibility"], "unverified", "{api}");
            assert!(
                api["api_changes"]["modified"]
                    .as_array()
                    .is_none_or(|items| items.is_empty()),
                "{api}"
            );
            for flags in [vec![], vec!["--strict-public-const-values"]] {
                let (output, json) = hook(&repo, &flags);
                assert!(output.status.success(), "{flags:?}: {json}");
                assert_eq!(json["api"]["type_annotation"][0]["n"], "AREA", "{json}");
                assert_eq!(
                    json["api"]["type_annotation"][0]["compatibility"], "unverified",
                    "{json}"
                );
                assert!(json["impacts"].is_null(), "{json}");
                assert!(
                    json["impact_info"]
                        .as_array()
                        .is_some_and(|refs| !refs.is_empty()),
                    "{json}"
                );
                assert!(
                    json["impact_info"].as_array().unwrap().iter().any(|group| {
                        group["refs"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|reference| reference["ln"] == 1)
                    }),
                    "reading references must remain: {json}"
                );
            }
            let (output, json) = hook(&repo, &["--strict-public-type-annotations"]);
            assert_eq!(output.status.code(), Some(1), "{json}");
            assert!(json["impacts"].is_null(), "{json}");
            assert!(
                json["blocking_categories"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|category| category == "policy.type_annotation"),
                "{json}"
            );
        }
    }
}

#[test]
fn type_annotations_formatting_is_unreported_and_nonblocking() {
    for extension in ["ts", "tsx"] {
        for (before, after) in [
            (": Dim", ":    Dim"),
            (": Dim", " : Dim"),
            (
                ": { width: number; height: number; }",
                ": {\n width : number ;\n height : number ;\n }",
            ),
        ] {
            let repo = TestRepo::new();
            repo.init_git();
            let file = format!("area.{extension}");
            let source = |annotation| {
                format!("export const AREA{annotation} = {{ width: 100 * 2, height: 50 * 2 }};\n")
            };
            repo.write(&file, source(before));
            repo.write(
                format!("use.{extension}"),
                "import { AREA } from './area';\nexport function read() { return AREA; }\n",
            );
            repo.commit_all("initial");
            repo.write(&file, source(after));
            let api = repo.run_json("review", &["--git"]);
            for bucket in [
                "type_annotation_changes",
                "const_value_changes",
                "modified",
                "unchanged_type_annotations",
            ] {
                assert!(
                    api["api_changes"][bucket]
                        .as_array()
                        .is_none_or(|items| items.is_empty()),
                    "{bucket}: {api}"
                );
            }
            for flags in [
                vec![],
                vec!["--strict-public-type-annotations"],
                vec![
                    "--strict-public-type-annotations",
                    "--strict-public-const-values",
                ],
            ] {
                let (output, json) = hook(&repo, &flags);
                assert!(output.status.success(), "{flags:?}: {json}");
                assert!(
                    json["blocking_categories"]
                        .as_array()
                        .is_none_or(|items| items.is_empty()),
                    "{json}"
                );
            }
        }
    }
}

#[test]
fn type_annotation_literal_type_changes_survive_equal_normalized_signatures() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("api.ts", "export const VALUE: 'a b' = getText();\n");
    repo.write(
        "use.ts",
        "import { VALUE } from './api';\nexport function read() { return VALUE; }\n",
    );
    repo.commit_all("initial");
    repo.write("api.ts", "export const VALUE: 'a  b' = getText();\n");
    let api = repo.run_json("review", &["--git"]);
    assert_eq!(
        api["api_changes"]["type_annotation_changes"][0]["name"], "VALUE",
        "{api}"
    );
    let (output, json) = hook(&repo, &[]);
    assert!(output.status.success(), "{json}");
    assert_eq!(json["api"]["type_annotation"][0]["n"], "VALUE", "{json}");
    let (output, json) = hook(&repo, &["--strict-public-type-annotations"]);
    assert_eq!(output.status.code(), Some(1), "{json}");
}

#[test]
fn type_annotation_and_initializer_changes_keep_existing_blocking() {
    for (old, new) in [
        ("{ width: 100 * 2 }", "{ width: 100 * 3 }"),
        ("'a b'", "'a  b'"),
        ("input as Shape", "input as OtherShape"),
        ("(x: number): number => x", "(x: string): number => x"),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write("api.ts", format!("export const VALUE: Before = {old};\n"));
        repo.write(
            "use.ts",
            "import { VALUE } from './api';\nexport function read() { return VALUE; }\n",
        );
        repo.commit_all("initial");
        repo.write("api.ts", format!("export const VALUE: After = {new};\n"));
        let api = repo.run_json("review", &["--git"]);
        assert!(
            api["api_changes"]["type_annotation_changes"].is_null(),
            "{api}"
        );
        let (output, json) = hook(&repo, &[]);
        assert_eq!(output.status.code(), Some(1), "{old} -> {new}: {json}");
        assert!(json["api"]["type_annotation"].is_null(), "{json}");
    }
}

#[test]
fn type_annotations_preserve_other_origins_and_other_symbols() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("first.ts", "export const AREA = { width: 100 * 2 };\nexport function changed(x: number) { return x; }\n");
    repo.write("second.ts", "export const AREA = { width: 100 * 2 };\n");
    repo.write("useFirst.ts", "import { AREA, changed } from './first';\nexport function read() { return AREA.width + changed(1); }\n");
    repo.write(
        "useSecond.ts",
        "import { AREA } from './second';\nexport function read() { return AREA.width; }\n",
    );
    repo.commit_all("initial");
    repo.write("first.ts", "export const AREA: Dim = { width: 100 * 2 };\nexport function changed(x: number, y: number) { return x + y; }\n");
    repo.write(
        "second.ts",
        "export const AREA: Dim = { width: 100 * 3 };\n",
    );
    let (output, json) = hook(&repo, &[]);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert_eq!(json["api"]["type_annotation"][0]["f"], "first.ts", "{json}");
    let impacts = json["impacts"].as_array().unwrap();
    assert!(
        impacts.iter().any(|group| group["src"] == "second.ts"
            && group["syms"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "AREA")),
        "{json}"
    );
    assert!(
        impacts.iter().any(|group| group["src"] == "first.ts"
            && group["syms"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "changed")),
        "{json}"
    );
    assert!(
        !impacts.iter().any(|group| group["src"] == "first.ts"
            && group["syms"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "AREA")),
        "{json}"
    );
}

#[test]
fn type_annotation_strict_does_not_apply_to_existing_const_value_policy() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("api.ts", "export const VALUE: number = 1;\n");
    repo.write(
        "use.ts",
        "import { VALUE } from './api';\nexport function read() { return VALUE; }\n",
    );
    repo.commit_all("initial");
    repo.write("api.ts", "export const VALUE: number = 2;\n");
    for flags in [vec![], vec!["--strict-public-type-annotations"]] {
        let (output, json) = hook(&repo, &flags);
        assert!(output.status.success(), "{flags:?}: {json}");
        assert_eq!(json["api"]["const_value"][0]["n"], "VALUE", "{json}");
        assert!(json["api"]["type_annotation"].is_null(), "{json}");
    }
    let (output, json) = hook(&repo, &["--strict-public-const-values"]);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert!(
        json["blocking_categories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|category| category == "api.const_value"),
        "{json}"
    );
}
