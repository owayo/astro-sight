//! Rust の子 module の中の相対 `pub use` (先頭が識別子の path) を、edition に合わせて
//! 読む (Issue #207)。

use super::support::*;

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
