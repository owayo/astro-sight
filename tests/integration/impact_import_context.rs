use super::support::*;

fn callers(value: &serde_json::Value, bucket: &str) -> Vec<(String, u64)> {
    let mut result: Vec<_> = value["changes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|change| change[bucket].as_array().cloned().unwrap_or_default())
        .map(|caller| {
            (
                caller["path"].as_str().unwrap().to_owned(),
                caller["line"].as_u64().unwrap(),
            )
        })
        .collect();
    result.sort();
    result
}

#[test]
fn impact_rust_import_context_ignores_literal_and_comment_semicolons() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "mod internal;\npub mod user;\n");
    repo.write("src/internal.rs", "pub fn helper() -> u8 { 1 }\n");
    repo.write(
        "src/user.rs",
        r##"use crate::internal::helper;
fn show<T>(_: T, _: u8) {}
pub fn run() {
    show("; use fake", helper());
    show(r#"; use fake"#, helper());
    show(';', /*; use fake */ helper());
    show({ /*; use fake */ 1 }, helper());
    { use crate::internal::helper; show("; use fake", helper()); }
    println!("; use {:?}", helper());
}
"##,
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/internal.rs",
        "pub fn helper(value: u8) -> u8 { value }\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let impacted = callers(&context, "impacted_callers");
    for line in 3..=8 {
        assert!(
            impacted.contains(&("src/user.rs".to_owned(), line)),
            "{context}"
        );
    }
    let informational = callers(&context, "informational_callers");
    assert!(
        informational.contains(&("src/user.rs".to_owned(), 0)),
        "{context}"
    );
    for line in 3..=6 {
        assert!(
            !informational.contains(&("src/user.rs".to_owned(), line)),
            "{context}"
        );
    }
    let output = cargo_bin()
        .args(["impact", "--dir"])
        .arg(repo.root())
        .arg("--git")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
}

#[test]
fn impact_ts_import_context_ignores_literals_and_comments_but_keeps_imports() {
    let repo = TestRepo::new();
    repo.write("api.ts", "export function helper(): number { return 1; }\n");
    repo.write(
        "user.ts",
        r#"import { helper } from "./api";
export { helper };
const show = (...values: unknown[]) => values;
show("; import fake", helper());
show('; export { fake }', helper());
show(`; import fake`, helper());
show(/; import fake/, helper());
show(/*; import fake */ helper());
import { helper as other } from "./api"; show("; import fake", other());
"#,
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "api.ts",
        "export function helper(value: number): number { return value; }\n",
    );
    let context = repo.run_json("context", &["--git"]);
    let impacted = callers(&context, "impacted_callers");
    for line in 3..=7 {
        assert!(
            impacted.contains(&("user.ts".to_owned(), line)),
            "{context}"
        );
    }
    let informational = callers(&context, "informational_callers");
    assert!(
        informational.contains(&("user.ts".to_owned(), 0)),
        "{context}"
    );
    assert!(
        informational.contains(&("user.ts".to_owned(), 1)),
        "{context}"
    );
    for line in 3..=7 {
        assert!(
            !informational.contains(&("user.ts".to_owned(), line)),
            "{context}"
        );
    }
}
