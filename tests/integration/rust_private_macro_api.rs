//! private な local macro の本体と、公開契約に影響する変更を区別する。

use super::support::{TestRepo, cargo_bin};

const BEFORE: &str = r#"use paint::*;
macro_rules! make_widget {
    ($name:ident, $($items:tt)*) => { struct $name; impl $name { $($items)* } };
}
make_widget!(Tile,
    fn tint(&self, _owner: Option<&str>) -> Pigment { Pigment::Red }
);
pub fn color() -> Pigment { Tile.tint(None) }
"#;

fn review(before: &str, after: &str, staged: bool) -> serde_json::Value {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers=[\"paint\",\"readout\"]\nresolver=\"3\"\n",
    );
    repo.create_dir_all("paint/src");
    repo.create_dir_all("readout/src");
    repo.write(
        "paint/Cargo.toml",
        "[package]\nname=\"paint\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
    );
    repo.write("paint/src/lib.rs", "pub enum Pigment { Red, Blue }\n");
    repo.write("readout/Cargo.toml", "[package]\nname=\"readout\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\npaint={path=\"../paint\"}\n");
    repo.write(
        "readout/src/lib.rs",
        "#[cfg(target_os=\"macos\")]\npub mod window;\n",
    );
    repo.write("readout/src/window.rs", before);
    repo.commit_all("base");
    repo.write("readout/src/window.rs", after);
    if staged {
        repo.stage_all();
        repo.write("readout/src/window.rs", "pub fn divergent() {}\n");
    }
    let mut cmd = cargo_bin();
    cmd.args(["review", "--git", "--dir"]).arg(repo.root());
    if staged {
        cmd.arg("--staged");
    }
    let output = cmd.output().expect("review");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("review JSON")
}

#[test]
fn rust_private_macro_bodies_preserve_api_in_worktree_and_index() {
    for staged in [false, true] {
        for after in [
            BEFORE.replacen("Pigment::Red", "Pigment::Blue", 1),
            BEFORE.replace("{ Tile.tint(None) }", "{ let _ = 0; Tile.tint(None) }"),
        ] {
            let result = review(BEFORE, &after, staged);
            assert_eq!(
                result["api_changes"]["modified"],
                serde_json::json!([]),
                "staged={staged}: {result}"
            );
            for category in ["added", "removed"] {
                assert_eq!(
                    result["api_changes"][category],
                    serde_json::json!([]),
                    "{result}"
                );
            }
            assert!(result.get("truncations").is_none(), "{result}");
        }
    }
}

#[test]
fn rust_private_macro_binding_and_unknown_changes_remain_blocking() {
    for (before, after) in [
        (
            BEFORE.to_owned(),
            BEFORE.replace("make_widget!(Tile,", "make_widget!(Pigment,"),
        ),
        (
            BEFORE.to_owned(),
            BEFORE.replace("Option<&str>", "Option<&String>"),
        ),
        (
            BEFORE.to_owned(),
            BEFORE.replace("struct $name;", "pub struct $name;"),
        ),
        (BEFORE.to_owned(), BEFORE.replace("fn tint", "pub fn tint")),
        (
            BEFORE.replace("($name:ident,", "($name:expr,"),
            BEFORE
                .replace("($name:ident,", "($name:expr,")
                .replace("Pigment::Red", "Pigment::Blue"),
        ),
        (
            BEFORE.replace(
                "    ($name:ident,",
                "    ($($unknown:tt)*) => { pub struct Pigment; };\n    ($name:ident,",
            ),
            BEFORE
                .replace(
                    "    ($name:ident,",
                    "    ($($unknown:tt)*) => { pub struct Pigment; };\n    ($name:ident,",
                )
                .replace("Pigment::Red", "Pigment::Blue"),
        ),
        (
            BEFORE.replace("struct $name;", "struct $name; unknown!();"),
            BEFORE
                .replace("struct $name;", "struct $name; unknown!();")
                .replace("Pigment::Red", "Pigment::Blue"),
        ),
        (
            BEFORE.replace(
                "Pigment::Red",
                "{ #[macro_export] macro_rules! exported { () => {} } Pigment::Red }",
            ),
            BEFORE.replace(
                "Pigment::Red",
                "{ #[macro_export] macro_rules! exported { () => {} } Pigment::Blue }",
            ),
        ),
        (
            BEFORE
                .replace(
                    "fn tint(&self, _owner: Option<&str>) -> Pigment { Pigment::Red }",
                    "fn tint(&self, _owner: Option<&str>) -> impl Copy { 1u8 }",
                )
                .replace("pub fn color() -> Pigment", "pub fn color() -> impl Copy"),
            BEFORE
                .replace(
                    "fn tint(&self, _owner: Option<&str>) -> Pigment { Pigment::Red }",
                    "fn tint(&self, _owner: Option<&str>) -> impl Copy { 2u16 }",
                )
                .replace("pub fn color() -> Pigment", "pub fn color() -> impl Copy"),
        ),
        (
            format!(
                "type Shade = Pigment;\n{}",
                BEFORE.replace("pub fn color() -> Pigment", "pub fn color() -> Shade")
            ),
            format!(
                "type Shade = ();\n{}",
                BEFORE
                    .replace("pub fn color() -> Pigment", "pub fn color() -> Shade")
                    .replace("{ Tile.tint(None) }", "{ () }")
            ),
        ),
    ] {
        let result = review(&before, &after, false);
        assert!(
            !result["api_changes"]["modified"]
                .as_array()
                .expect("modified")
                .is_empty(),
            "{after}: {result}"
        );
    }
    let result = review(
        BEFORE,
        &BEFORE.replace("use paint::*;", "pub use paint::*;"),
        false,
    );
    assert!(
        result["truncations"]
            .as_array()
            .expect("coverage")
            .iter()
            .any(|notice| notice["reason"] == "rust_reexports_incomplete"),
        "{result}"
    );
}

#[test]
fn rust_private_module_additions_preserve_api_in_worktree_and_index() {
    let parser = "use tree_sitter::Parser;\nthread_local! { static POOL: Option<Parser> = None; }\npub fn parse() {}\n";
    let public_parser = parser.replace("use tree_sitter", "pub use tree_sitter");
    let lib = "pub mod parser;\nmod first;\nmod second;\n";
    for public_in_index in [false, true] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.create_dir_all("src");
        repo.write("Cargo.toml", "[package]\nname=\"reader\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\ntree-sitter=\"0.27\"\n");
        repo.write("src/lib.rs", lib);
        repo.write("src/parser.rs", parser);
        for file in ["src/first.rs", "src/second.rs"] {
            repo.write(file, "macro_rules! repeat { () => {} }\n");
        }
        repo.commit_all("base");
        repo.write("src/lib.rs", format!("{lib}mod addition;\n"));
        repo.write(
            "src/addition.rs",
            "use crate::parser;\nunknown!(tree_sitter);\nmacro_rules! own { () => {} }\n",
        );
        repo.write(
            "src/parser.rs",
            if public_in_index {
                &public_parser
            } else {
                parser
            },
        );
        repo.stage_all();
        repo.write(
            "src/parser.rs",
            if public_in_index {
                parser
            } else {
                &public_parser
            },
        );
        for staged in [false, true] {
            let mut cmd = cargo_bin();
            cmd.args(["review", "--git", "--dir"]).arg(repo.root());
            if staged {
                cmd.arg("--staged");
            }
            let output = cmd.output().expect("review");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let result: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("review JSON");
            let changed = ["added", "removed", "modified"]
                .into_iter()
                .any(|category| {
                    result["api_changes"][category]
                        .as_array()
                        .expect("API changes")
                        .iter()
                        .any(|change| change["name"] == "Parser")
                });
            assert_eq!(changed, staged == public_in_index, "{result}");
            assert!(result.get("truncations").is_none(), "{result}");
        }
    }
}
