//! 明示された Rust 可視性の拡大と旧境界の回帰。

use super::support::*;

fn private_parent_glob_fixture(cargo: bool, directory_module: bool) -> TestRepo {
    let repo = TestRepo::new();
    let base = if cargo { "src/" } else { "" };
    repo.create_dir_all(format!("{base}formatter/lifecycle"));
    if cargo {
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"visibility\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "mod formatter;\n");
    }
    let parent = if directory_module {
        format!("{base}formatter/mod.rs")
    } else {
        format!("{base}formatter.rs")
    };
    repo.write(
        &parent,
        "mod lifecycle;\nfn absolute_path(path: &str) -> &str { path }\n",
    );
    repo.write(
        format!("{base}formatter/lifecycle.rs"),
        "use super::*;\nfn emit(path: &str) -> &str { absolute_path(path) }\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        parent,
        "mod lifecycle;\npub(crate) fn absolute_path(path: &str) -> &str { path }\n",
    );
    repo
}

#[test]
fn rust_private_visibility_widening_preserves_direct_child_parent_globs() {
    for cargo in [false, true] {
        for directory_module in [false, true] {
            let repo = private_parent_glob_fixture(cargo, directory_module);
            let caller = if cargo {
                "src/formatter/lifecycle.rs"
            } else {
                "formatter/lifecycle.rs"
            };
            let base = if cargo { "src/" } else { "" };
            let parent = if directory_module {
                format!("{base}formatter/mod.rs")
            } else {
                format!("{base}formatter.rs")
            };
            let after = std::fs::read_to_string(repo.path(&parent)).unwrap();
            repo.write(&parent, after.replace("pub(crate) ", ""));
            let old_refs = repo.run_json("refs", &["--name", "absolute_path"]);
            let old_dead = repo.run_json("dead-code", &[]);
            repo.write(&parent, &after);
            let new_refs = repo.run_json("refs", &["--name", "absolute_path"]);
            let refs = |result: &serde_json::Value| {
                result["refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|row| row["kind"] == "ref")
                    .cloned()
                    .collect::<Vec<_>>()
            };
            assert_eq!(refs(&old_refs), refs(&new_refs));
            assert_eq!(old_dead, repo.run_json("dead-code", &[]));
            let context = repo.run_json("context", &["--git"]);
            assert!(
                !has_caller(&context, "impacted_callers", caller),
                "{context}"
            );
            assert!(
                has_caller(&context, "informational_callers", caller),
                "{context}"
            );
            for command in ["impact", "review"] {
                let output = cargo_bin()
                    .args([
                        command,
                        "--dir",
                        repo.root().to_str().unwrap(),
                        "--git",
                        "--hook",
                    ])
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{command}: {output:?}");
            }
        }
    }
}

#[test]
fn rust_private_visibility_widening_parent_glob_without_git() {
    let repo = TestRepo::new();
    repo.create_dir_all("formatter");
    repo.write(
        "formatter.rs",
        "mod lifecycle;\npub(crate) fn absolute_path(path: &str) -> &str { path }\n",
    );
    repo.write(
        "formatter/lifecycle.rs",
        "use super /* parent */ :: *;\nfn emit(path: &str) -> &str { absolute_path(path) }\n",
    );
    repo.write("changes.patch", "diff --git a/formatter.rs b/formatter.rs\n--- a/formatter.rs\n+++ b/formatter.rs\n@@ -1,2 +1,2 @@\n mod lifecycle;\n-fn absolute_path(path: &str) -> &str { path }\n+pub(crate) fn absolute_path(path: &str) -> &str { path }\n");
    let patch = repo.path("changes.patch");
    let args = ["--diff-file", patch.to_str().unwrap()];
    let context = repo.run_json("context", &args);
    assert!(
        !has_caller(&context, "impacted_callers", "formatter/lifecycle.rs"),
        "{context}"
    );
    assert!(
        has_caller(&context, "informational_callers", "formatter/lifecycle.rs"),
        "{context}"
    );
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            repo.root().to_str().unwrap(),
            args[0],
            args[1],
            "--hook",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn rust_private_visibility_widening_parent_glob_keeps_uncertain_changes() {
    for case in [
        "body",
        "parameter",
        "attribute",
        "other-glob",
        "outside",
        "ambiguous",
        "ambiguous-parent",
        "path",
        "parse",
        "shrink",
        "staged",
    ] {
        let repo = private_parent_glob_fixture(true, false);
        match case {
            "body" => repo.write("src/formatter.rs", "mod lifecycle;\npub(crate) fn absolute_path(path: &str) -> &str { \"other\" }\n"),
            "parameter" => repo.write("src/formatter.rs", "mod lifecycle;\npub(crate) fn absolute_path(path: String) -> String { path }\n"),
            "attribute" => repo.write("src/formatter.rs", "mod lifecycle;\n#[inline]\npub(crate) fn absolute_path(path: &str) -> &str { path }\n"),
            "other-glob" => repo.write("src/formatter/lifecycle.rs", "use super::*;\nuse crate::other::*;\nfn emit(path: &str) -> &str { absolute_path(path) }\n"),
            "outside" => {
                repo.write("src/outside.rs", "fn emit(path: &str) -> &str { crate::formatter::absolute_path(path) }\n");
                repo.git(["add", "src/outside.rs"]);
                repo.git(["commit", "-m", "outside reference", "-q"]);
            }
            "ambiguous" => repo.write("src/formatter/lifecycle/mod.rs", "fn emit() {}\n"),
            "ambiguous-parent" => repo.write("src/formatter/mod.rs", "mod lifecycle;\n"),
            "path" => repo.write("src/other.rs", "#[path = \"formatter/lifecycle.rs\"]\nmod lifecycle;\n"),
            "parse" => repo.write("src/other.rs", "fn broken(\n"),
            "shrink" => {
                repo.git(["add", "src/formatter.rs"]);
                repo.git(["commit", "-m", "visible parent", "-q"]);
                repo.write("src/formatter.rs", "mod lifecycle;\nfn absolute_path(path: &str) -> &str { path }\n");
            }
            "staged" => repo.stage_all(),
            _ => unreachable!(),
        }
        let args = if case == "staged" {
            vec!["--git", "--staged"]
        } else {
            vec!["--git"]
        };
        let context = repo.run_json("context", &args);
        let caller = if case == "outside" {
            "src/outside.rs"
        } else {
            "src/formatter/lifecycle.rs"
        };
        assert!(
            has_caller(&context, "impacted_callers", caller),
            "{case}: {context}"
        );
    }
}

fn fixture(old: &str, new: &str, caller: &str) -> TestRepo {
    fixture_with_suffix(old, new, caller, "")
}

fn fixture_with_suffix(old: &str, new: &str, caller: &str, suffix: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("src/a/b");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"visibility\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub mod a;\npub mod outside;\n");
    repo.write("src/a.rs", "pub mod b;\npub mod peer;\n");
    repo.write("src/a/peer.rs", "pub fn peer() {}\n");
    repo.write("src/a/b/child.rs", "pub fn child() {}\n");
    repo.write("src/outside.rs", "pub fn outside() {}\n");
    repo.write(
        caller,
        "pub fn caller() -> Option<String> { crate::a::b::normalize(\"a\") }\n",
    );
    repo.write("src/a/b.rs", format!("pub mod child;\n{old} fn normalize(raw: &str) -> Option<String> {{\n let t = raw.trim();\n (!t.is_empty()).then(|| t.to_string())\n}}\n{suffix}"));
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/a/b.rs", format!("pub mod child;\n{new} fn normalize(raw: &str) -> Option<String> {{\n let t = raw.trim();\n (!t.is_empty()).then(|| t.to_string())\n}}\n{suffix}"));
    repo
}

fn has_caller(context: &serde_json::Value, bucket: &str, caller: &str) -> bool {
    context["changes"].as_array().unwrap().iter().any(|change| {
        change[bucket]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["path"] == caller))
    })
}

#[test]
fn rust_visibility_widening_all_explicit_scopes_are_informational() {
    for (old, new, caller) in [
        ("pub(crate)", "pub", "src/outside.rs"),
        ("pub(self)", "pub(super)", "src/a/b/child.rs"),
        ("pub(super)", "pub(crate)", "src/a/peer.rs"),
        (
            "pub(in crate::a::b)",
            "pub(in crate::a)",
            "src/a/b/child.rs",
        ),
        ("pub(in crate::a)", "pub", "src/a/peer.rs"),
        ("pub(in self)", "pub(in super)", "src/a/b/child.rs"),
        ("pub(in super)", "pub(in crate)", "src/a/peer.rs"),
        ("pub(in super::super)", "pub", "src/outside.rs"),
        ("pub(in crate::r#a)", "pub(crate)", "src/a/peer.rs"),
        ("pub(in self)", "pub(in crate::r#a)", "src/a/b/child.rs"),
    ] {
        let repo = fixture(old, new, caller);
        if old == "pub(crate)" {
            let path = "src/a/b.rs";
            let after = std::fs::read_to_string(repo.path(path)).unwrap();
            repo.write(
                path,
                after.replacen("pub fn normalize", "pub(crate) fn normalize", 1),
            );
            let old_refs = repo.run_json("refs", &["--name", "normalize"]);
            let old_dead = repo.run_json("dead-code", &[]);
            repo.write(path, after);
            let new_refs = repo.run_json("refs", &["--name", "normalize"]);
            let new_dead = repo.run_json("dead-code", &[]);
            assert_eq!(
                old_refs["refs"].as_array().unwrap().len(),
                new_refs["refs"].as_array().unwrap().len()
            );
            assert_eq!(old_dead["dead_symbols"], new_dead["dead_symbols"]);
            assert_eq!(old_dead["test_only_symbols"], new_dead["test_only_symbols"]);
        }
        let context = repo.run_json("context", &["--git"]);
        assert!(
            !has_caller(&context, "impacted_callers", caller),
            "{old} -> {new}: {context}"
        );
        assert!(
            has_caller(&context, "informational_callers", caller),
            "{old} -> {new}: {context}"
        );
        let result = cargo_bin()
            .args([
                "review",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{old} -> {new}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn rust_visibility_widening_keeps_distinct_type_and_value_namespaces() {
    let caller = "src/outside.rs";
    let repo = fixture_with_suffix("pub(crate)", "pub", caller, "type normalize = u32;\n");
    let context = repo.run_json("context", &["--git"]);
    assert!(
        !has_caller(&context, "impacted_callers", caller),
        "{context}"
    );
    assert!(
        has_caller(&context, "informational_callers", caller),
        "{context}"
    );
}

#[test]
fn rust_visibility_widening_handles_raw_and_nested_root_module_names() {
    for (module, path) in [
        ("r#type", "src/type.rs"),
        ("a::main", "src/a/main.rs"),
        ("a::lib", "src/a/lib.rs"),
    ] {
        let repo = TestRepo::new();
        repo.create_dir_all("src/a");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"visibility\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        let root_mod = if module == "r#type" { "r#type" } else { "a" };
        repo.write(
            "src/lib.rs",
            format!("pub mod {root_mod};\npub mod outside;\n"),
        );
        if let Some(child) = module.strip_prefix("a::") {
            repo.write("src/a.rs", format!("pub mod {child};\n"));
        }
        repo.write(
            "src/outside.rs",
            format!("pub fn caller() {{ crate::{module}::normalize(); }}\n"),
        );
        repo.write(path, "pub(crate) fn normalize() {}\n");
        repo.init_git();
        repo.commit_all("initial");
        repo.write(path, "pub fn normalize() {}\n");
        let context = repo.run_json("context", &["--git"]);
        assert!(
            !has_caller(&context, "impacted_callers", "src/outside.rs"),
            "{module}: {context}"
        );
        assert!(
            has_caller(&context, "informational_callers", "src/outside.rs"),
            "{module}: {context}"
        );
    }
}

#[test]
fn rust_visibility_widening_keeps_crate_root_reused_as_a_module_blocking() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/lib");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"visibility\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("src/lib.rs", "pub(crate) fn normalize() {}\nmod caller;\n");
    repo.write("src/caller.rs", "fn caller() { crate::normalize(); }\n");
    repo.write("src/main.rs", "mod lib;\nfn main() {}\n");
    repo.write("src/lib/caller.rs", "fn caller() { super::normalize(); }\n");
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/lib.rs", "pub fn normalize() {}\nmod caller;\n");
    let context = repo.run_json("context", &["--git"]);
    assert!(
        has_caller(&context, "impacted_callers", "src/caller.rs"),
        "{context}"
    );
}

#[test]
fn rust_visibility_widening_keeps_outside_equivalent_and_shrinking_scopes() {
    for (old, new, caller) in [
        ("pub(super)", "pub", "src/outside.rs"),
        ("pub(crate)", "pub(self)", "src/a/peer.rs"),
        ("pub(crate)", "", "src/a/peer.rs"),
        ("pub(crate)", "pub(in crate)", "src/outside.rs"),
        ("pub(in crate::missing)", "pub", "src/outside.rs"),
    ] {
        let repo = fixture(old, new, caller);
        let context = repo.run_json("context", &["--git"]);
        assert!(
            has_caller(&context, "impacted_callers", caller),
            "{old} -> {new}: {context}"
        );
    }
}

#[test]
fn rust_visibility_widening_does_not_hide_other_changes_or_ambiguous_layouts() {
    for extra in [
        "body",
        "signature",
        "return-type",
        "attribute",
        "spacing",
        "glob",
        "module",
        "both-targets",
        "staged",
    ] {
        let caller = "src/outside.rs";
        let repo = fixture("pub(crate)", "pub", caller);
        match extra {
            "body" => repo.write("src/a/b.rs", "pub mod child;\npub fn normalize(raw: &str) -> Option<String> {\n Some(raw.to_string())\n}\n"),
            "signature" => repo.write("src/a/b.rs", "pub mod child;\npub fn normalize(raw: &str, extra: bool) -> Option<String> {\n let t = raw.trim();\n (!t.is_empty()).then(|| t.to_string())\n}\n"),
            "return-type" => repo.write("src/a/b.rs", "pub mod child;\npub fn normalize(raw: &str) -> Option<&str> {\n let t = raw.trim();\n (!t.is_empty()).then(|| t.to_string())\n}\n"),
            "attribute" => repo.write("src/a/b.rs", "pub mod child;\n#[inline]\npub fn normalize(raw: &str) -> Option<String> {\n let t = raw.trim();\n (!t.is_empty()).then(|| t.to_string())\n}\n"),
            "spacing" => repo.write("src/a/b.rs", "pub mod child;\npub  fn normalize(raw: &str) -> Option<String> {\n let t = raw.trim();\n (!t.is_empty()).then(|| t.to_string())\n}\n"),
            "glob" => {
                repo.write("src/outside.rs", "use crate::a::b::*;\npub fn caller() -> Option<String> { normalize(\"a\") }\n");
                repo.git(["add", "src/outside.rs"]);
                repo.git(["commit", "-m", "existing glob", "-q"]);
            }
            "module" => repo.write("src/lib.rs", "pub mod a;\npub mod outside;\npub mod added {}\n"),
            "both-targets" => {
                repo.write("src/main.rs", "mod a;\nmod outside;\nfn main() {}\n");
                repo.git(["add", "src/main.rs"]);
                repo.git(["commit", "-m", "add other target", "-q"]);
            }
            "staged" => repo.stage_all(),
            _ => unreachable!(),
        }
        let args = if extra == "staged" {
            vec!["--git", "--staged"]
        } else {
            vec!["--git"]
        };
        let context = repo.run_json("context", &args);
        assert!(
            has_caller(&context, "impacted_callers", caller),
            "{extra}: {context}"
        );
    }
}
