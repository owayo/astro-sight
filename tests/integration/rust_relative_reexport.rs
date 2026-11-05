//! Rust の子 module の中の相対 `pub use` (先頭が識別子の path) を、edition に合わせて
//! 読む (Issue #207)。

use super::support::*;

#[test]
fn review_rust_public_signatures_expose_hidden_types_individually() {
    for (reader, exposed) in [
        (
            "use crate::hidden::Mode; pub fn current() -> Mode { Mode::Fast }",
            true,
        ),
        ("pub fn current() -> u32 { 1 }", false),
        (
            "use super::*; pub fn parent_glob() -> Mode { Mode::Fast }",
            true,
        ),
        (
            "use crate::{*}; pub fn root_glob() -> Mode { Mode::Fast }",
            true,
        ),
        (
            "use crate::hidden::Mode; pub fn namespace_control() -> Mode { Mode::Fast }",
            true,
        ),
        ("pub use crate::hidden::Mode;", true),
        ("use crate::hidden::Mode; pub use Mode::Fast;", true),
        (
            "use crate::hidden::Mode as Current; pub fn current() -> Current { Current::Fast }",
            true,
        ),
        (
            "use crate::hidden::Mode; type Current = Mode; pub fn current() -> Current { Mode::Fast }",
            true,
        ),
        ("use crate::hidden::Mode; pub fn accept(_: Mode) {}", true),
        (
            "use crate::hidden::Mode; pub struct Wrapper { pub mode: Mode }",
            true,
        ),
        (
            "use crate::hidden::Mode; pub struct Wrapper { mode: Mode }",
            false,
        ),
        (
            "use crate::hidden::Mode; pub fn current() -> impl Tag { Mode::Fast }",
            false,
        ),
        (
            "use crate::hidden::Mode; pub trait Opaque { type Item; } impl Opaque for Mode { type Item = Mode; } pub fn current() -> impl Opaque { Mode::Fast }",
            false,
        ),
        (
            "use crate::hidden::Mode; pub trait Get<T: Default> { fn get(&self) -> T { T::default() } } pub struct Wrapper; impl Get<Mode> for Wrapper {}",
            true,
        ),
        (
            "use crate::hidden::Mode; pub trait Get { type Item; fn get(&self) -> Self::Item; } impl<T> Get for T { type Item = Mode; fn get(&self) -> Mode { Mode::Fast } }",
            true,
        ),
        (
            "use crate::hidden::Mode as r#Current; pub fn current() -> r#Current { r#Current::Fast }",
            true,
        ),
        (
            "use crate::hidden::Mode; pub const FACTORY: fn() -> Mode = || Mode::Fast;",
            true,
        ),
        (
            "use crate::hidden::Mode; unsafe extern \"C\" { pub fn current() -> Mode; }",
            true,
        ),
        (
            "use crate::hidden::Mode; pub fn current<T>(_: T) -> Mode { Mode::Fast }",
            true,
        ),
        (
            "use crate::hidden::Mode; pub enum Wrapper { Value(Mode) }",
            true,
        ),
        (
            "use crate::hidden::Mode; pub struct Wrapper(pub Mode);",
            true,
        ),
        ("use crate::hidden::Mode; pub struct Wrapper(Mode);", false),
        (
            "use crate::hidden::Mode; pub trait Factory { fn mode(&self) -> Mode; }",
            true,
        ),
        (
            "use crate::hidden::Mode; pub trait Factory { type Item: Into<Mode>; }",
            true,
        ),
        (
            "use crate::hidden::Mode; pub struct Wrapper; impl Wrapper { pub fn mode() -> Mode { Mode::Fast } }",
            true,
        ),
        (
            "use crate::hidden::Mode; pub struct Wrapper; impl Wrapper { fn mode() -> Mode { Mode::Fast } }",
            false,
        ),
        (
            "pub mod other { pub struct Mode; } pub fn current() -> other::Mode { other::Mode }",
            false,
        ),
        (
            "pub mod other { pub struct Mode; } use self::other::Mode; pub fn current() -> Mode { Mode }",
            false,
        ),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"signatures\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        let root = if reader.contains("parent_glob") || reader.contains("root_glob") {
            "mod hidden; use hidden::Mode; pub mod reader;\n"
        } else {
            "mod hidden; pub mod reader;\n"
        };
        repo.write("src/lib.rs", root);
        repo.write(
            "src/not_a_module.rs",
            "#[derive(Custom)] pub struct Orphan;\n",
        );
        repo.write("src/reader.rs", format!("pub trait Tag {{}}\n{reader}"));
        let namespace_function = if reader.contains("namespace_control") {
            "pub fn Mode() -> Unrelated { Unrelated }\n"
        } else {
            ""
        };
        repo.write("src/hidden.rs", ["pub enum Mode { Fast }\nimpl Mode { pub fn amount(&self) -> usize { 1 } pub fn next(&self) -> Other { Other } }\nimpl crate::reader::Tag for Mode {}\nimpl Default for Mode { fn default() -> Self { Mode::Fast } }\npub struct Other;\nimpl Other { pub fn amount(&self) -> usize { 1 } }\npub struct Unrelated;\nimpl Unrelated { pub fn untouched(&self) {} }\n", namespace_function].concat());
        repo.init_git();
        repo.commit_all("initial");
        repo.write("src/hidden.rs", ["pub enum Mode { Fast }\nimpl Mode { pub fn amount(&self, scale: usize) -> usize { scale } pub fn next(&self) -> Other { Other } }\nimpl crate::reader::Tag for Mode {}\nimpl Default for Mode { fn default() -> Self { Mode::Fast } }\npub struct Other;\nimpl Other { pub fn amount(&self, scale: usize) -> usize { scale } }\npub struct Unrelated;\nimpl Unrelated { pub fn untouched(&self, extra: usize) {} }\n", namespace_function].concat());
        let review = repo.run_json("review", &["--git"]);
        let modified: Vec<_> = review["api_changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|change| change["name"].as_str())
            .collect();
        assert_eq!(
            modified.contains(&"Mode.amount"),
            exposed,
            "{reader}: {review}"
        );
        assert_eq!(
            modified.contains(&"Other.amount"),
            exposed,
            "{reader}: {review}"
        );
        if !reader.contains("pub use Mode::Fast") {
            assert!(
                !modified.contains(&"Unrelated.untouched"),
                "{reader}: {review}"
            );
        }
        if reader == "use crate::hidden::Mode; pub fn current() -> Mode { Mode::Fast }" {
            repo.stage_all();
            let staged = repo.run_json("review", &["--git", "--staged"]);
            assert!(
                staged["api_changes"]["modified"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|change| change["name"] == "Mode.amount"),
                "{staged}"
            );
            let hook = cargo_bin()
                .args(["review", "--git", "--staged", "--hook", "--dir"])
                .arg(repo.root())
                .output()
                .unwrap();
            assert!(!hook.status.success(), "{hook:?}");
            assert!(
                String::from_utf8_lossy(&hook.stderr).contains("api.mod"),
                "{hook:?}"
            );
        }
    }
}

#[test]
fn review_rust_global_reexport_distinguishes_external_and_local_crates() {
    for (edition, root, reexport, exposed) in [
        ("2021", "mod dep { pub mod api {} }", "::dep::api", true),
        (
            "2021",
            "mod dep { pub mod api {} }",
            "crate::dep::api",
            false,
        ),
        ("2015", "mod dep { pub mod api {} }", "::dep::api", false),
        (
            "2021",
            "extern crate self as dep; pub mod api {}",
            "::dep::api",
            false,
        ),
        (
            "2021",
            "extern crate self as r#dep; pub mod api {}",
            "::r#dep::api",
            false,
        ),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src/internal");
        repo.create_dir_all("dep/src");
        repo.write("Cargo.toml", format!("[package]\nname = \"global\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n[dependencies]\ndep = {{ path = \"dep\" }}\n"));
        repo.write(
            "dep/Cargo.toml",
            "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("dep/src/lib.rs", "pub fn api() -> u32 { 7 }\n");
        repo.write(
            "src/lib.rs",
            format!("{root}\nmod internal;\npub mod facade;\n"),
        );
        repo.write("src/internal.rs", "pub mod api;\n");
        repo.write("src/internal/api.rs", "pub fn found() -> u32 { 1 }\n");
        repo.write(
            "src/facade.rs",
            format!("pub use {reexport};\npub use crate::internal::*;\n"),
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "src/internal/api.rs",
            "pub fn found(scale: u32) -> u32 { scale }\n",
        );
        for staged in [false, true] {
            let mut args = vec!["--git"];
            if staged {
                repo.stage_all();
                args.push("--staged");
                if root.starts_with("extern crate self") {
                    repo.write(
                        "src/lib.rs",
                        "mod dep { pub mod api {} }\nmod internal;\npub mod facade;\n",
                    );
                }
            }
            let review = repo.run_json("review", &args);
            let found = review["api_changes"]["modified"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["name"] == "found");
            assert_eq!(
                found, exposed,
                "{edition} / {root} / {reexport} / staged={staged}: {review}"
            );
            if exposed && staged {
                let hook = cargo_bin()
                    .args(["review", "--git", "--staged", "--hook", "--dir"])
                    .arg(repo.root())
                    .output()
                    .unwrap();
                assert!(!hook.status.success(), "{hook:?}");
                assert!(
                    String::from_utf8_lossy(&hook.stderr).contains("api.mod"),
                    "{hook:?}"
                );
            }
        }
    }
}

#[test]
fn review_rust_glob_module_shadow_requires_unconditional_binding() {
    for (facade, child, exposed) in [
        ("pub use crate::internal::*;", "", true),
        (
            "mod api;\npub use crate::internal::*;",
            "pub fn hidden() {}",
            false,
        ),
        (
            "mod api;\npub use crate::internal::*;",
            "#![cfg(any())]",
            true,
        ),
        (
            "#[cfg(feature = \"x\")] mod api;\npub use crate::internal::*;",
            "pub fn hidden() {}",
            true,
        ),
        (
            "mod api { #![cfg(any())] }\npub use crate::internal::*;",
            "",
            true,
        ),
        (
            "pub mod other {}\npub use self::other as api;\npub use crate::internal::*;",
            "",
            false,
        ),
        (
            "pub mod other { #![cfg(any())] }\n#[cfg(any())] pub use self::other as api;\npub use crate::internal::*;",
            "",
            true,
        ),
        (
            "pub mod other {}\n#[cfg(any())] pub use self::other as api;\npub use crate::internal::*;",
            "",
            true,
        ),
        (
            "pub mod other {}\npub fn value() {}\npub use self::value as api;\n#[cfg(any())] pub use self::other as api;\npub use crate::internal::*;",
            "",
            true,
        ),
        (
            "#[cfg(any())] pub mod parent { pub mod other {} }\n#[cfg(any())] pub use self::parent::other as api;\npub use crate::internal::*;",
            "",
            true,
        ),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src/internal");
        repo.create_dir_all("src/facade");
        repo.write("Cargo.toml", "[package]\nname = \"shadow\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nx = []\n");
        repo.write("src/lib.rs", "mod internal;\npub mod facade;\n");
        repo.write("src/internal.rs", "pub mod api;\n");
        repo.write("src/internal/api.rs", "pub fn found() -> u32 { 1 }\n");
        repo.write("src/facade.rs", facade);
        repo.write("src/facade/api.rs", child);
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "src/internal/api.rs",
            "pub fn found(scale: u32) -> u32 { scale }\n",
        );
        for staged in [false, true] {
            let mut args = vec!["--git"];
            if staged {
                repo.stage_all();
                args.push("--staged");
            }
            let review = repo.run_json("review", &args);
            let found = review["api_changes"]["modified"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["name"] == "found");
            assert_eq!(
                found, exposed,
                "{facade} / {child} / staged={staged}: {review}"
            );
            if exposed && staged {
                let output = cargo_bin()
                    .arg("review")
                    .args(&args)
                    .arg("--hook")
                    .arg("--dir")
                    .arg(repo.root())
                    .output()
                    .unwrap();
                assert!(!output.status.success(), "{output:?}");
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("api.mod"),
                    "{output:?}"
                );
            }
        }
    }
}

const NEW_VALUE: &str = "pub struct NewValue;\n\nimpl NewValue {\n    pub fn amount(&self) -> usize {\n        1\n    }\n}\n";

/// Issue の手順: 初期の commit の後、`reader.rs` に `mod value;` と `reexport` を書き、
/// `reader/value.rs` を足して stage し、`review --git --staged` の api_changes を返す。
fn staged_api_changes(edition: &str, reexport: &str) -> serde_json::Value {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        format!(
            "[package]\nname = \"relative-export-repro\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n\n[workspace]\n"
        ),
    );
    repo.write("src/lib.rs", "pub mod reader;\n");
    repo.write("src/reader.rs", "// Public facade.\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.create_dir_all("src/reader");
    repo.write("src/reader.rs", format!("mod value;\n{reexport}\n"));
    repo.write("src/reader/value.rs", NEW_VALUE);
    repo.stage_all();
    let review = repo.run_json(
        "review",
        &[
            "--git",
            "--staged",
            "--dead-scope",
            "touched-symbols",
            "--format",
            "json",
        ],
    );
    review["api_changes"].clone()
}

fn added_names(api_changes: &serde_json::Value) -> Vec<&str> {
    let mut names: Vec<&str> = api_changes["added"]
        .as_array()
        .expect("added")
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn review_staged_relative_pub_use_in_child_module_matches_self_form() {
    for edition in ["2018", "2024"] {
        let bare = staged_api_changes(edition, "pub use value::NewValue;");
        let anchored = staged_api_changes(edition, "pub use self::value::NewValue;");
        assert_eq!(
            added_names(&bare),
            ["NewValue", "NewValue.amount"],
            "{edition}: {bare}"
        );
        assert_eq!(bare, anchored, "{edition}");
    }
}

/// 対照: edition の指定が無い (2015) crate は、従来どおりクレートルートから読む。
#[test]
fn review_staged_bare_pub_use_in_edition_2015_resolves_from_crate_root() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"relative-export-repro\"\nversion = \"0.1.0\"\n\n[workspace]\n",
    );
    repo.write("src/lib.rs", "mod value;\npub mod reader;\n");
    repo.write("src/reader.rs", "// Public facade.\n");
    repo.write("src/value.rs", "// empty\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/reader.rs", "pub use value::Root;\n");
    repo.write(
        "src/value.rs",
        "pub struct Root;\n\nimpl Root {\n    pub fn amount(&self) -> usize {\n        1\n    }\n}\n",
    );
    repo.stage_all();
    let review = repo.run_json(
        "review",
        &[
            "--git",
            "--staged",
            "--dead-scope",
            "touched-symbols",
            "--format",
            "json",
        ],
    );
    assert_eq!(
        added_names(&review["api_changes"]),
        ["Root", "Root.amount"],
        "{review}"
    );
}
