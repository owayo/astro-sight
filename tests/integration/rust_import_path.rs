use super::support::{TestRepo, cargo_bin};

const OLD: &str = "pub fn make_card() -> crate::model::Card {\n    crate::model::Card\n}\n";
const NEW: &str =
    "use crate::model::Card;\n\npub fn make_card() -> Card {\n    crate::model::Card\n}\n";

fn repository() -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"cards\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    repo.write(
        "src/lib.rs",
        "pub mod model;\npub mod factory;\npub mod consumer;\n",
    );
    repo.write("src/model.rs", "pub struct Card;\n");
    repo.write("src/factory.rs", OLD);
    repo.write(
        "src/consumer.rs",
        "pub fn inspect() { let _ = crate::factory::make_card(); }\n",
    );
    repo.init_git();
    repo.commit_all("base");
    repo.write("src/factory.rs", NEW);
    repo
}

fn hook(repo: &TestRepo, staged: bool) -> (std::process::Output, serde_json::Value) {
    let mut command = cargo_bin();
    command
        .env("__CARGO_TEST_ROOT", repo.root())
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    if staged {
        command.arg("--staged");
    }
    let output = command.output().unwrap();
    let value = serde_json::from_slice(&output.stderr).unwrap();
    (output, value)
}

#[test]
fn rust_import_path_review_and_staged_use_the_same_proof() {
    let repo = repository();
    let (output, value) = hook(&repo, false);
    assert!(output.status.success(), "{value}");
    assert!(value["impacts"].is_null(), "{value}");
    assert_eq!(
        value["api"]["mod_compat"][0]["reason"],
        "rust_explicit_import_path"
    );
    let normal = repo.run_json("review", &["--git"]);
    assert!(
        normal["api_changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{normal}"
    );
    assert_eq!(
        normal["api_changes"]["compatible_modified"][0]["reason"],
        "rust_explicit_import_path"
    );

    repo.stage_all();
    repo.write("src/model.rs", "pub struct Card { pub value: u8 }\n");
    let (output, value) = hook(&repo, true);
    assert!(output.status.success(), "index: {value}");
    let (output, value) = hook(&repo, false);
    assert_eq!(output.status.code(), Some(1), "worktree: {value}");
    assert!(
        value["api"]["mod"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["n"] == "make_card"),
        "{value}"
    );
    assert!(
        value["impacts"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "{value}"
    );

    repo.stage_all();
    repo.write("src/model.rs", "pub struct Card;\n");
    let (output, value) = hook(&repo, true);
    assert_eq!(
        output.status.code(),
        Some(1),
        "changed definition in index: {value}"
    );
    let (output, value) = hook(&repo, false);
    assert!(
        output.status.success(),
        "unchanged definition in worktree: {value}"
    );
}

#[test]
fn rust_import_path_review_keeps_other_types_and_module_changes_blocking() {
    for (path, source) in [
        (
            "src/factory.rs",
            NEW.replace("use crate::model::Card;", "use crate::model::Other;")
                .replace("-> Card", "-> Other"),
        ),
        ("src/model.rs", "pub type Card = u8;\n".to_owned()),
        (
            "src/lib.rs",
            "pub mod model;\n#[cfg(all())]\npub mod factory;\npub mod consumer;\n".to_owned(),
        ),
    ] {
        let repo = repository();
        repo.write(path, source);
        let (output, value) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{path}: {value}");
        assert!(
            value["api"]["mod"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["n"] == "make_card")),
            "{value}"
        );
        assert!(
            value["impacts"]
                .as_array()
                .is_some_and(|items| !items.is_empty()),
            "{value}"
        );
    }
}

#[test]
fn rust_import_path_review_keeps_child_glob_rebindings_blocking() {
    let repo = repository();
    repo.create_dir_all("src/factory");
    let old = "mod inner;\npub fn make_card() -> crate::model::String { crate::model::String }\npub fn width() -> usize { inner::width() }\n";
    repo.write("src/factory.rs", old);
    repo.write("src/model.rs", "pub struct String;\nimpl String { pub fn from(_: &str) -> Self { Self } pub fn len(&self) -> usize { 2 } }\n");
    repo.write(
        "src/factory/inner.rs",
        "use super::*;\npub fn width() -> usize { String::from(\"x\").len() }\n",
    );
    repo.commit_all("parent with child glob");
    repo.write(
        "src/factory.rs",
        format!(
            "use crate::model::String;\n{}",
            old.replace("-> crate::model::String", "-> String")
        ),
    );
    for staged in [false, true] {
        if staged {
            repo.stage_all();
        }
        let (output, value) = hook(&repo, staged);
        assert_eq!(output.status.code(), Some(1), "staged={staged}: {value}");
        assert!(
            value["api"]["mod"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["n"] == "make_card")),
            "{value}"
        );
        assert!(
            value["api"]["mod_compat"]
                .as_array()
                .is_none_or(|items| items.iter().all(|item| item["n"] != "make_card")),
            "{value}"
        );
        assert!(
            value["impacts"]
                .as_array()
                .is_some_and(|items| !items.is_empty()),
            "{value}"
        );
    }
}

#[test]
fn rust_import_path_review_keeps_included_source_rebindings_blocking() {
    for prefix in [
        "",
        "include!(\"factory.rs\");\n",
        "use std::include as load;\nload!(\"factory.rs\");\n",
    ] {
        let repo = repository();
        let old = "pub fn make_card() -> crate::model::String { crate::model::String }\n";
        repo.write("src/factory.rs", old);
        repo.write("src/model.rs", "pub struct String;\nimpl String { pub fn from(_: &str) -> Self { Self } pub fn len(&self) -> usize { 2 } }\n");
        repo.write("src/consumer.rs", format!("{prefix}pub fn inspect() {{ let _ = crate::factory::make_card(); }}\npub fn width() -> usize {{ String::from(\"x\").len() }}\n"));
        repo.commit_all("included factory");
        repo.write(
            "src/factory.rs",
            format!(
                "use crate::model::String;\n{}",
                old.replace("-> crate::model::String", "-> String")
            ),
        );
        let (output, value) = hook(&repo, false);
        if prefix.is_empty() {
            assert!(output.status.success(), "ordinary consumer: {value}");
            assert_eq!(
                value["api"]["mod_compat"][0]["reason"],
                "rust_explicit_import_path"
            );
            assert!(value["impacts"].is_null(), "{value}");
            continue;
        }
        assert_eq!(output.status.code(), Some(1), "{prefix}: {value}");
        assert!(
            value["api"]["mod"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["n"] == "make_card")),
            "{value}"
        );
        assert!(
            value["impacts"]
                .as_array()
                .is_some_and(|items| !items.is_empty()),
            "{value}"
        );
    }
}
