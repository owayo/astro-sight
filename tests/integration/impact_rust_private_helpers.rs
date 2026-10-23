//! private helper と別の所有元の同名参照 (Issue #133)。

use super::support::*;

fn private_helpers_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("src/style");
    repo.create_dir_all("src/config");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "src/lib.rs",
        "mod config; mod literal; mod style;\nmacro_rules! marker { () => { const MARKER: u8 = 0; } }\nmarker!();\n",
    );
    repo.write(
        "src/config/mod.rs",
        "mod paths;\nuse paths::{compile_includes, compile_excludes};\n",
    );
    repo.write(
        "src/config/paths.rs",
        "pub fn compile_includes() {}\npub fn compile_excludes() {}\n",
    );
    repo.write(
        "src/style/mod.rs",
        "use super::literal;\nmod string_literals; mod percent_array; mod symbol_array; mod word_array;\n",
    );
    repo.write(
        "src/style/string_literals.rs",
        "fn inspect_body(x: u8) -> u8 { x }\nfn to_string_literal(x: u8) -> u8 { x }\nfn escape_string(x: u8) -> u8 { x }\n",
    );
    repo.write(
        "src/literal.rs",
        "pub(crate) fn inspect_body(x: u8) -> u8 { x }\npub(crate) fn to_string_literal(x: u8) -> u8 { x }\npub(crate) fn escape_string(x: u8) -> u8 { x }\nfn first() -> String { format!(\"{}\", inspect_body(1)) }\nfn second() -> String { format!(\"{}\", inspect_body(1)) }\nfn third() -> String { format!(\"{}\", to_string_literal(1)) }\n",
    );
    repo.write(
        "src/style/percent_array.rs",
        "use super::literal::{escape_string};\nfn call() -> u8 { let out = escape_string(1); out }\n",
    );
    repo.write(
        "src/style/symbol_array.rs",
        "use super::literal::to_string_literal;\nfn call() -> u8 { to_string_literal(1) }\n",
    );
    repo.write(
        "src/style/word_array.rs",
        "use super::literal::to_string_literal;\nfn first() -> u8 { to_string_literal(1) }\nfn second() -> u8 { to_string_literal(1) }\n",
    );
    repo
}

fn change_private_helpers(repo: &TestRepo, suffix: &str) {
    repo.write(
        "src/style/string_literals.rs",
        format!(
            "fn inspect_body(x: u64) -> u64 {{ x }}\nfn to_string_literal(x: u64) -> u64 {{ x }}\nfn escape_string(x: u64) -> u64 {{ x }}\n{suffix}"
        ),
    );
}

fn helper_change(context: &serde_json::Value) -> &serde_json::Value {
    context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["path"] == "src/style/string_literals.rs")
        .unwrap()
}

#[test]
fn impact_rust_private_helpers_ignore_include_substrings_in_imports() {
    let repo = private_helpers_repo();
    repo.init_git();
    repo.commit_all("initial");
    let refs_before = repo.run_json("refs", &["--name", "to_string_literal"]);
    change_private_helpers(&repo, "");

    let context = repo.run_json("context", &["--git"]);
    let change = helper_change(&context);
    assert_eq!(change["affected_symbols"].as_array().unwrap().len(), 3);
    for bucket in [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ] {
        assert!(
            change[bucket]
                .as_array()
                .is_none_or(|callers| callers.is_empty()),
            "{bucket}: {context}"
        );
    }
    let refs_after = repo.run_json("refs", &["--name", "to_string_literal"]);
    let references = |result: &serde_json::Value| {
        result["refs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|reference| reference["kind"] == "ref")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(references(&refs_before), references(&refs_after));
    assert_eq!(references(&refs_after).len(), 6);

    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    repo.stage_all();
    let staged = repo.run_json("context", &["--git", "--staged"]);
    assert!(
        helper_change(&staged)["impacted_callers"]
            .as_array()
            .is_some_and(|callers| !callers.is_empty()),
        "staged の未証明な参照は保持する: {staged}"
    );
}

#[test]
fn impact_rust_private_helpers_keep_real_descendant_and_include_callers() {
    for (extra_source, suffix, caller_path) in [
        (
            "pub fn call() { super::inspect_body(1); }\n",
            "mod child;\n",
            "src/style/string_literals/child.rs",
        ),
        (
            "use std::include as load;\nload!(\"style/string_literals.rs\");\npub fn call() { inspect_body(1); }\n",
            "",
            "src/included.rs",
        ),
    ] {
        let repo = private_helpers_repo();
        repo.create_dir_all("src/style/string_literals");
        repo.write(caller_path, extra_source);
        if !suffix.is_empty() {
            repo.write(
                "src/style/string_literals.rs",
                format!(
                    "fn inspect_body(x: u8) -> u8 {{ x }}\nfn to_string_literal(x: u8) -> u8 {{ x }}\nfn escape_string(x: u8) -> u8 {{ x }}\n{suffix}"
                ),
            );
        } else {
            repo.write(
                "src/lib.rs",
                "mod config; mod literal; mod style; mod included;\nmacro_rules! marker { () => { const MARKER: u8 = 0; } }\nmarker!();\n",
            );
        }
        repo.init_git();
        repo.commit_all("initial");
        change_private_helpers(&repo, suffix);
        let context = repo.run_json("context", &["--git"]);
        let change = helper_change(&context);
        assert!(
            ["impacted_callers", "low_confidence_callers"]
                .iter()
                .any(|bucket| change[bucket]
                    .as_array()
                    .is_some_and(|callers| callers.iter().any(|caller| {
                        caller["path"] == caller_path
                            && caller["symbols"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|name| name == "inspect_body")
                    }))),
            "{caller_path}: {context}"
        );
        if !suffix.is_empty() {
            assert!(
                change["impacted_callers"]
                    .as_array()
                    .is_some_and(|callers| callers
                        .iter()
                        .any(|caller| caller["path"] == caller_path))
            );
            let output = cargo_bin()
                .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1), "{output:?}");
        }
    }
}
