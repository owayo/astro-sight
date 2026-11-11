//! Rust の明示 use による同型の戻り値表記の短縮 (Issue #351)。

use crate::commands::tests::common::*;

const MANIFEST: &str = "[package]\nname = \"cards\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const ROOT: &str = "pub mod model;\npub mod factory;\npub mod consumer;\n";
const OLD: &str = "pub fn make_card() -> crate::model::Card {\n    crate::model::Card\n}\n";
const NEW: &str =
    "use crate::model::Card;\n\npub fn make_card() -> Card {\n    crate::model::Card\n}\n";
const MODEL: &str = "pub struct Card;\n";

fn changes(
    old: &str,
    new: &str,
    root: &str,
    model: &str,
    changed_model: Option<&str>,
) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", MANIFEST),
            ("src/lib.rs", root),
            ("src/factory.rs", old),
            ("src/model.rs", model),
            (
                "src/consumer.rs",
                "pub fn inspect() { let _ = crate::factory::make_card(); }\n",
            ),
        ],
        "base",
    );
    std::fs::write(repo.join("src/factory.rs"), new).unwrap();
    if let Some(model) = changed_model {
        std::fs::write(repo.join("src/model.rs"), model).unwrap();
    }
    detect_api_changes_from_worktree(repo)
}

#[test]
fn rust_import_path_return_is_compatible() {
    for (model, value) in [
        (MODEL, "crate::model::Card"),
        ("pub enum Card { One }\n", "crate::model::Card::One"),
        (
            "pub union Card { pub value: u8 }\n",
            "crate::model::Card { value: 0 }",
        ),
    ] {
        let old = OLD.replace("crate::model::Card\n", &format!("{value}\n"));
        let new = NEW.replace("crate::model::Card\n", &format!("{value}\n"));
        let api = changes(&old, &new, ROOT, model, None);
        assert!(
            api.modified.iter().all(|entry| entry.name != "make_card"),
            "{api:?}"
        );
        assert!(
            api.compatible_modified
                .iter()
                .any(|entry| entry.name == "make_card"
                    && entry.reason == "rust_explicit_import_path"),
            "{api:?}"
        );
    }
}

#[test]
fn rust_import_path_uncertain_namespaces_stay_blocking() {
    for (old, new, root, model) in [
        (
            OLD.to_owned(),
            NEW.replace("use crate::model::Card;", "use crate::model::*;"),
            ROOT.to_owned(),
            MODEL.to_owned(),
        ),
        (
            OLD.to_owned(),
            NEW.replace(
                "use crate::model::Card;",
                "#[cfg(all())]\nuse crate::model::Card;",
            ),
            ROOT.to_owned(),
            MODEL.to_owned(),
        ),
        (
            OLD.to_owned(),
            NEW.replace(
                "use crate::model::Card;",
                "use crate::model::Card as Value;",
            )
            .replace("-> Card", "-> Value"),
            ROOT.to_owned(),
            MODEL.to_owned(),
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            ROOT.replace("pub mod model;", "#[cfg(all())]\npub mod model;"),
            MODEL.to_owned(),
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            format!("macro_rules! names {{ () => {{}} }}\nnames!();\n{ROOT}"),
            MODEL.to_owned(),
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            ROOT.to_owned(),
            "pub type Card = u8;\n".to_owned(),
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            ROOT.to_owned(),
            "#[cfg(all())]\npub struct Card;\n".to_owned(),
        ),
        (
            format!("struct Card;\n{OLD}"),
            format!("struct Card;\n{NEW}"),
            ROOT.to_owned(),
            MODEL.to_owned(),
        ),
        (
            OLD.replace("make_card()", "make_card<Card>()"),
            NEW.replace("make_card()", "make_card<Card>()"),
            ROOT.to_owned(),
            MODEL.to_owned(),
        ),
        (
            OLD.to_owned(),
            NEW.replace("use crate::model::Card;", "use crate::other::Card;"),
            ROOT.to_owned(),
            MODEL.to_owned(),
        ),
    ] {
        let api = changes(&old, &new, &root, &model, None);
        assert!(
            api.modified.iter().any(|entry| entry.name == "make_card"),
            "{old} -> {new}: {api:?}"
        );
        assert!(
            api.compatible_modified
                .iter()
                .all(|entry| entry.name != "make_card"),
            "{api:?}"
        );
    }
}

#[test]
fn rust_import_path_other_changes_stay_blocking() {
    for new in [
        NEW.replace("crate::model::Card\n", "panic!()\n"),
        NEW.replace("make_card()", "make_card(_: bool)"),
        NEW.replace("-> Card", "-> Other"),
    ] {
        let api = changes(OLD, &new, ROOT, MODEL, None);
        assert!(
            api.modified.iter().any(|entry| entry.name == "make_card"),
            "{new}: {api:?}"
        );
    }
    for model in [
        "pub struct Card { pub value: u8 }\n",
        "pub type Card = u8;\n",
    ] {
        let api = changes(OLD, NEW, ROOT, MODEL, Some(model));
        assert!(
            api.modified.iter().any(|entry| entry.name == "make_card"),
            "{model}: {api:?}"
        );
    }
}

#[test]
fn rust_import_path_raw_existing_binding_stays_blocking() {
    let old = "pub fn make_card() -> crate::model::String { crate::model::String }\npub fn width() -> usize { r#String::from(\"x\").len() }\n";
    let new = format!(
        "use crate::model::String;\n{}",
        old.replace("-> crate::model::String", "-> String")
    );
    let model = "pub struct String;\nimpl String { pub fn from(_: &str) -> Self { Self } pub fn len(&self) -> usize { 2 } }\n";
    let api = changes(old, &new, ROOT, model, None);
    assert!(
        api.modified.iter().any(|entry| entry.name == "make_card"),
        "{api:?}"
    );
}

#[test]
fn rust_import_path_child_modules_stay_blocking() {
    for module in [
        "mod inner;\n",
        "mod inner { pub fn width() -> usize { 1 } }\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let old = format!(
            "{module}pub fn make_card() -> crate::model::String {{ crate::model::String }}\npub fn width() -> usize {{ inner::width() }}\n"
        );
        let new = format!(
            "use crate::model::String;\n{}",
            old.replace("-> crate::model::String", "-> String")
        );
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", ROOT),
                (
                    "src/model.rs",
                    "pub struct String;\nimpl String { pub fn from(_: &str) -> Self { Self } pub fn len(&self) -> usize { 2 } }\n",
                ),
                ("src/factory.rs", &old),
                (
                    "src/factory/inner.rs",
                    "use super::*;\npub fn width() -> usize { String::from(\"x\").len() }\n",
                ),
                (
                    "src/consumer.rs",
                    "pub fn inspect() { let _ = crate::factory::make_card(); }\n",
                ),
            ],
            "base",
        );
        std::fs::write(repo.join("src/factory.rs"), new).unwrap();
        let api = detect_api_changes_from_worktree(repo);
        assert!(
            api.modified.iter().any(|entry| entry.name == "make_card"),
            "{module}: {api:?}"
        );
        assert!(
            api.compatible_modified
                .iter()
                .all(|entry| entry.name != "make_card"),
            "{module}: {api:?}"
        );
    }
}

#[test]
fn rust_import_path_source_inclusion_stays_blocking() {
    for (prefix, ignored) in [
        ("", false),
        ("include!(\"factory.rs\");\n", false),
        ("use std::include as load;\nload!(\"factory.rs\");\n", false),
        (
            "macro_rules! load { () => { include!(\"factory.rs\"); } }\nload!();\n",
            false,
        ),
        ("#[unknown]\nfn opaque() {}\n", false),
        ("unknown!();\n", false),
        ("", true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let old = "pub fn make_card() -> crate::model::String { crate::model::String }\n";
        let new =
            "use crate::model::String;\npub fn make_card() -> String { crate::model::String }\n";
        let consumer = format!(
            "{prefix}pub fn inspect() {{ let _ = crate::factory::make_card(); }}\npub fn width() -> usize {{ String::from(\"x\").len() }}\n"
        );
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", MANIFEST),
                ("src/lib.rs", ROOT),
                (
                    "src/model.rs",
                    "pub struct String;\nimpl String { pub fn from(_: &str) -> Self { Self } pub fn len(&self) -> usize { 2 } }\n",
                ),
                ("src/factory.rs", old),
                ("src/consumer.rs", &consumer),
                (".gitignore", "/ignored.rs\n"),
            ],
            "base",
        );
        std::fs::write(repo.join("src/factory.rs"), new).unwrap();
        if ignored {
            std::fs::write(repo.join("ignored.rs"), "include!(\"src/factory.rs\");\n").unwrap();
        }
        let api = detect_api_changes_from_worktree(repo);
        let blocking = !prefix.is_empty() || ignored;
        assert_eq!(
            api.modified.iter().any(|entry| entry.name == "make_card"),
            blocking,
            "{prefix}, ignored={ignored}: {api:?}"
        );
        assert_eq!(
            api.compatible_modified
                .iter()
                .any(|entry| entry.name == "make_card"
                    && entry.reason == "rust_explicit_import_path"),
            !blocking,
            "{prefix}, ignored={ignored}: {api:?}"
        );
    }
}
