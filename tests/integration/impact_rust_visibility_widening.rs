//! 明示された Rust 可視性の拡大と旧境界の回帰。

use super::support::*;

#[test]
fn rust_private_visibility_widening_parent_test_globs_keep_external_calls_independent() {
    let numeric =
        "fn from_str(_: &str) -> usize { 1 }\npub fn total() -> usize { from_str(\"x\") }\n";
    let tests = "#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert!(true); } }\n";
    for location in ["src/lib.rs", "src/numeric/ascii.rs", "src/unrelated.rs"] {
        for visibility in ["pub(crate)", "pub(super)"] {
            let repo = TestRepo::new();
            repo.create_dir_all("src/numeric");
            repo.write("Cargo.toml", "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nserde_json = \"1\"\n");
            for (path, source) in [
                ("src/lib.rs", "pub mod numeric;\npub mod unrelated;\n"),
                ("src/numeric/mod.rs", "pub mod ascii;\n"),
                ("src/numeric/ascii.rs", numeric),
                (
                    "src/unrelated.rs",
                    "pub fn run() -> u32 { serde_json::from_str::<u32>(\"1\").unwrap() }\n",
                ),
            ] {
                let suffix = if path == location { tests } else { "" };
                repo.write(path, format!("{source}{suffix}"));
            }
            repo.init_git();
            repo.commit_all("initial");
            let suffix = if location == "src/numeric/ascii.rs" {
                tests
            } else {
                ""
            };
            repo.write(
                "src/numeric/ascii.rs",
                format!("{visibility} {numeric}{suffix}"),
            );
            let context = repo.run_json("context", &["--git"]);
            assert!(
                !has_caller(&context, "impacted_callers", "src/unrelated.rs"),
                "{location} / {visibility}: {context}"
            );
            for command in ["impact", "review"] {
                let output = run_hook(&repo, command, &["--git"]);
                assert!(
                    output.status.success(),
                    "{location} / {visibility} / {command}: {output:?}"
                );
            }
        }
    }
}

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

/// Issue #206 の変更前。`crate::numeric::ascii` の直下の private 宣言。
const NUMERIC_ASCII: &str = "struct Number;\nfn parse(_: &str) -> Number { Number }\nfn readings(_: &str, _: Number) -> usize { 1 }\n";

/// Issue #206 の変更後。3 つの宣言の可視性だけを `pub(super)` に広げる。
const NUMERIC_ASCII_SUPER: &str = "pub(super) struct Number;\npub(super) fn parse(_: &str) -> Number { Number }\npub(super) fn readings(_: &str, _: Number) -> usize { 1 }\n";

/// `numeric::ascii::parse` を指さない同名の参照 (Issue #206 の `src/unrelated.rs`)。
const UNRELATED_CALLERS: &str = "pub struct Catalog;\nimpl Catalog { pub fn parse(_: &str) -> usize { 2 } }\npub fn run() -> usize {\n    let readings = vec![1usize];\n    let local = readings.iter().map(|readings| readings + 1).sum::<usize>();\n    local + Catalog::parse(\"x\") + \"2\".parse::<usize>().unwrap()\n}\n";

const UNRELATED: &str = "src/unrelated.rs";

/// 旧 private 範囲 (`crate::numeric::ascii` の部分木) の子モジュールから parse を呼ぶ。
const CHILD_CALLER: &str =
    "pub fn call() -> usize {\n    let _value = super::parse(\"x\");\n    1\n}\n";

/// Issue #206 の配置を `files` で差し替えて (足して) コミットし、`src/numeric/ascii.rs` を
/// `after` に書き換える。
fn numeric_fixture(files: &[(&str, &str)], after: &str) -> TestRepo {
    let repo = TestRepo::new();
    let defaults = [
        (
            "Cargo.toml",
            "[package]\nname = \"impact_repro\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("src/lib.rs", "mod numeric;\nmod unrelated;\n"),
        ("src/numeric/mod.rs", "mod ascii;\n"),
        ("src/numeric/ascii.rs", NUMERIC_ASCII),
        (UNRELATED, UNRELATED_CALLERS),
    ];
    for (path, content) in defaults.iter().chain(files) {
        if let Some(parent) = std::path::Path::new(path).parent() {
            repo.create_dir_all(parent);
        }
        repo.write(path, content);
    }
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/numeric/ascii.rs", after);
    repo
}

/// `impact` / `review` を Stop hook と同じ `--hook` 付きで起動する。
fn run_hook(repo: &TestRepo, command: &str, args: &[&str]) -> std::process::Output {
    cargo_bin()
        .arg(command)
        .arg("--dir")
        .arg(repo.root())
        .args(args)
        .arg("--hook")
        .output()
        .unwrap()
}

/// `caller` を含む振り分け先 (impacted / low_confidence / informational) の一覧。
fn caller_buckets(context: &serde_json::Value, caller: &str) -> Vec<&'static str> {
    [
        "impacted_callers",
        "low_confidence_callers",
        "informational_callers",
    ]
    .into_iter()
    .filter(|bucket| has_caller(context, bucket, caller))
    .collect()
}

#[test]
fn rust_visibility_widening_to_restricted_scope_ignores_callers_outside_it() {
    // Issue #206: private な直下宣言を pub(super) / pub(in ...) に広げても、新しい可視範囲
    // (`crate::numeric` の部分木) の外の同名参照は、変更の前も後もその宣言を指せない。
    // 参照の形 (局所型・import した型の関連関数、別モジュールの修飾パス) とマクロの有無に
    // よらず、どの振り分け先にも出さず、Stop hook も止めない。
    let parse_only = NUMERIC_ASCII.replacen("fn parse", "pub(super) fn parse", 1);
    let restricted_path = NUMERIC_ASCII_SUPER.replace("pub(super)", "pub(in crate::numeric)");
    let imported_type: &[(&str, &str)] = &[
        ("src/lib.rs", "mod catalog;\nmod numeric;\nmod unrelated;\n"),
        (
            "src/catalog.rs",
            "pub struct Catalog;\nimpl Catalog { pub fn parse(_: &str) -> usize { 2 } }\n",
        ),
        (
            UNRELATED,
            "use crate::catalog::Catalog;\npub fn run() -> usize {\n    Catalog::parse(\"x\")\n}\n",
        ),
    ];
    let qualified_with_macro: &[(&str, &str)] = &[
        ("src/lib.rs", "mod numeric;\nmod other;\nmod unrelated;\n"),
        ("src/other.rs", "pub fn parse(_: &str) -> usize { 2 }\n"),
        (
            UNRELATED,
            "pub fn run() -> usize {\n    let values = vec![1usize];\n    values.len() + crate::other::parse(\"x\")\n}\n",
        ),
    ];
    let mut failures = Vec::new();
    for (case, files, after) in [
        ("issue", &[][..], NUMERIC_ASCII_SUPER),
        ("parse-only", &[][..], parse_only.as_str()),
        ("restricted-path", &[][..], restricted_path.as_str()),
        ("imported-type", imported_type, NUMERIC_ASCII_SUPER),
        (
            "qualified-with-macro",
            qualified_with_macro,
            NUMERIC_ASCII_SUPER,
        ),
    ] {
        let repo = numeric_fixture(files, after);
        let context = repo.run_json("context", &["--git"]);
        let buckets = caller_buckets(&context, UNRELATED);
        if !buckets.is_empty() {
            failures.push(format!("{case}: {UNRELATED} in {buckets:?}: {context}"));
        }
        for command in ["impact", "review"] {
            let output = run_hook(&repo, command, &["--git"]);
            if !output.status.success() {
                failures.push(format!(
                    "{case}: {command} exited with {:?}: {}{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn rust_visibility_widening_to_restricted_scope_keeps_old_scope_child_informational() {
    // 旧 private 範囲 (`crate::numeric::ascii` の部分木) の子モジュールは、変更前から parse を
    // 呼べる。範囲外の参照を除いても、この呼び出しは除外しない。可視性の拡大では壊れないので
    // 情報提供に分ける。
    let child = "src/numeric/ascii/child.rs";
    let before = format!("mod child;\n{NUMERIC_ASCII}");
    let after = format!("mod child;\n{NUMERIC_ASCII_SUPER}");
    let repo = numeric_fixture(
        &[
            ("src/numeric/ascii.rs", before.as_str()),
            (child, CHILD_CALLER),
        ],
        &after,
    );
    let context = repo.run_json("context", &["--git"]);
    assert_eq!(
        caller_buckets(&context, child),
        ["informational_callers"],
        "{context}"
    );
    assert!(caller_buckets(&context, UNRELATED).is_empty(), "{context}");
    for command in ["impact", "review"] {
        let output = run_hook(&repo, command, &["--git"]);
        assert!(output.status.success(), "{command}: {output:?}");
    }
}

#[test]
fn rust_visibility_widening_to_restricted_scope_keeps_uncertain_callers_blocking() {
    let sibling = "src/numeric/sibling.rs";
    let sibling_mod = ("src/numeric/mod.rs", "mod ascii;\nmod sibling;\n");
    let parameter = NUMERIC_ASCII_SUPER.replace("fn parse(_: &str)", "fn parse(_: &str, _: bool)");
    let body = NUMERIC_ASCII_SUPER.replace("-> usize { 1 }", "-> usize { 2 }");
    let added_use = format!("use std::fmt::Debug;\n{NUMERIC_ASCII_SUPER}");
    let added_mod = format!("mod extra {{}}\n{NUMERIC_ASCII_SUPER}");
    let equivalent = NUMERIC_ASCII.replacen("fn parse", "pub(self) fn parse", 1);
    let same_name_before = format!("struct parse {{}}\n{NUMERIC_ASCII}");
    let same_name_after = format!("pub(super) struct parse {{}}\n{NUMERIC_ASCII_SUPER}");
    for case in [
        "sibling",
        "parent-globs",
        "parameter",
        "body",
        "use",
        "mod",
        "shrink",
        "equivalent",
        "same-name",
        "both-roots",
        "staged",
    ] {
        let (mut files, after, caller): (Vec<(&str, &str)>, &str, &str) = match case {
            // 新しい範囲の中で旧範囲の外: 変更前の参照ではないと証明していないので残す。
            "sibling" => (
                vec![
                    sibling_mod,
                    (
                        sibling,
                        "pub fn run() -> usize {\n    let _value = super::ascii::parse(\"x\");\n    2\n}\n",
                    ),
                ],
                NUMERIC_ASCII_SUPER,
                sibling,
            ),
            // 新しい範囲の中の glob が parse を取り込むと、範囲外の `crate::numeric::parse` が
            // 曖昧になり rustc は E0659 で止まる (本物の影響)。
            "parent-globs" => (
                vec![
                    (
                        "src/numeric/mod.rs",
                        "mod ascii;\nmod other;\npub use ascii::*;\npub use other::*;\n",
                    ),
                    (
                        "src/numeric/other.rs",
                        "pub fn parse(_: &str) -> usize { 2 }\n",
                    ),
                    (
                        UNRELATED,
                        "pub fn run() -> usize {\n    crate::numeric::parse(\"x\")\n}\n",
                    ),
                ],
                NUMERIC_ASCII_SUPER,
                UNRELATED,
            ),
            // 可視性以外の変更が混ざると、可視性だけの変更とは証明できない。
            "parameter" => (Vec::new(), parameter.as_str(), UNRELATED),
            "body" => (Vec::new(), body.as_str(), UNRELATED),
            "use" => (Vec::new(), added_use.as_str(), UNRELATED),
            "mod" => (Vec::new(), added_mod.as_str(), UNRELATED),
            // 縮小は旧範囲の中の呼び出しを壊す。
            "shrink" => (
                vec![
                    sibling_mod,
                    ("src/numeric/ascii.rs", NUMERIC_ASCII_SUPER),
                    (
                        sibling,
                        "pub fn call() -> usize {\n    let _value = super::ascii::parse(\"x\");\n    1\n}\n",
                    ),
                ],
                NUMERIC_ASCII,
                sibling,
            ),
            // 同じ範囲への書き換えは拡大ではない。
            "equivalent" => (Vec::new(), equivalent.as_str(), UNRELATED),
            // 型と値の両方の名前空間で同名の宣言を編集すると、どちらの範囲かを決められない。
            "same-name" => (
                vec![("src/numeric/ascii.rs", same_name_before.as_str())],
                same_name_after.as_str(),
                UNRELATED,
            ),
            // 同じソースが lib と bin の 2 つの crate に入ると、所属のモジュールが一意でない。
            "both-roots" => (
                vec![("src/main.rs", "mod numeric;\nfn main() {}\n")],
                NUMERIC_ASCII_SUPER,
                UNRELATED,
            ),
            "staged" => (Vec::new(), NUMERIC_ASCII_SUPER, UNRELATED),
            _ => unreachable!(),
        };
        // 型パスの独立した証明に頼らず、変更した自由関数への実参照で保守側の判定を確かめる。
        if caller == UNRELATED && !files.iter().any(|(path, _)| *path == UNRELATED) {
            files.push((
                UNRELATED,
                "pub fn run() { let _value = crate::numeric::ascii::parse(\"x\"); }\n",
            ));
        }
        let repo = numeric_fixture(&files, after);
        let args = if case == "staged" {
            repo.stage_all();
            vec!["--git", "--staged"]
        } else {
            vec!["--git"]
        };
        let context = repo.run_json("context", &args);
        assert!(
            has_caller(&context, "impacted_callers", caller),
            "{case}: {context}"
        );
        let output = run_hook(&repo, "impact", &args);
        assert_eq!(output.status.code(), Some(1), "{case}: {output:?}");
    }
}

#[test]
fn rust_visibility_widening_to_restricted_scope_keeps_refs_and_dead_code() {
    // impact で除く参照も、refs の出現と dead-code の判定からは消さない。
    let repo = numeric_fixture(&[], NUMERIC_ASCII_SUPER);
    let refs = |repo: &TestRepo| {
        repo.run_json("refs", &["--name", "parse"])["refs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["kind"] == "ref")
            .cloned()
            .collect::<Vec<_>>()
    };
    let new_refs = refs(&repo);
    let new_dead = repo.run_json("dead-code", &[]);
    assert_eq!(
        new_refs
            .iter()
            .filter(|row| row["path"] == UNRELATED)
            .count(),
        2,
        "{new_refs:?}"
    );
    repo.write("src/numeric/ascii.rs", NUMERIC_ASCII);
    assert_eq!(refs(&repo), new_refs);
    assert_eq!(repo.run_json("dead-code", &[]), new_dead);
}

#[test]
fn rust_private_visibility_widening_to_crate_keeps_excluding_outside_callers() {
    // private な宣言 1 つに pub(crate) を付けただけの変更は、従来の証明のまま扱う。旧 private
    // 範囲の子モジュールは情報提供、旧範囲の外の未変更の同名参照 (兄弟モジュールと別の
    // モジュール) は旧宣言を指せなかったので除外する。
    let child = "src/numeric/ascii/child.rs";
    let sibling = "src/numeric/sibling.rs";
    let before = format!("mod child;\n{NUMERIC_ASCII}");
    let after = format!(
        "mod child;\n{}",
        NUMERIC_ASCII.replacen("fn parse", "pub(crate) fn parse", 1)
    );
    let repo = numeric_fixture(
        &[
            ("src/numeric/mod.rs", "mod ascii;\nmod sibling;\n"),
            ("src/numeric/ascii.rs", before.as_str()),
            (child, CHILD_CALLER),
            (
                sibling,
                "pub struct Catalog;\nimpl Catalog { pub fn parse(_: &str) -> usize { 2 } }\npub fn run() -> usize {\n    let values = vec![1usize];\n    values.len() + Catalog::parse(\"x\")\n}\n",
            ),
        ],
        &after,
    );
    let context = repo.run_json("context", &["--git"]);
    assert_eq!(
        caller_buckets(&context, child),
        ["informational_callers"],
        "{context}"
    );
    for caller in [sibling, UNRELATED] {
        assert!(
            caller_buckets(&context, caller).is_empty(),
            "{caller}: {context}"
        );
    }
    for command in ["impact", "review"] {
        let output = run_hook(&repo, command, &["--git"]);
        assert!(output.status.success(), "{command}: {output:?}");
    }
}

#[test]
fn rust_visibility_widening_to_restricted_scope_keeps_unproven_sources_blocking() {
    // ソースを別のモジュールへ展開し得る構文 (独自の属性マクロ、#[path]、include!、item 位置の
    // マクロ) があると、参照ファイルの所属を証明できないので範囲外とみなさない。
    let attribute =
        UNRELATED_CALLERS.replacen("pub fn run", "#[tracing::instrument]\npub fn run", 1);
    let item_macro = format!("{UNRELATED_CALLERS}make_items!();\n");
    for (case, files) in [
        ("attribute", vec![(UNRELATED, attribute.as_str())]),
        (
            "path",
            vec![(
                "src/numeric/mod.rs",
                "mod ascii;\n#[path = \"../unrelated.rs\"]\nmod alias;\n",
            )],
        ),
        (
            "include",
            vec![(
                "src/numeric/mod.rs",
                "mod ascii;\ninclude!(\"../unrelated.rs\");\n",
            )],
        ),
        ("item-macro", vec![(UNRELATED, item_macro.as_str())]),
    ] {
        let repo = numeric_fixture(&files, NUMERIC_ASCII_SUPER);
        let context = repo.run_json("context", &["--git"]);
        assert!(
            has_caller(&context, "impacted_callers", UNRELATED),
            "{case}: {context}"
        );
        let output = run_hook(&repo, "impact", &["--git"]);
        assert_eq!(output.status.code(), Some(1), "{case}: {output:?}");
    }
}

#[test]
fn rust_visibility_widening_to_restricted_scope_is_off_without_the_confidence_filter() {
    // `ASTRO_SIGHT_NO_CONFIDENCE_FILTER=1` は、可視性の拡大の証明による除外と情報提供への
    // 振り分けも止める。新しい範囲の外の参照も、旧範囲の子モジュールの参照も impacted に残す。
    let child = "src/numeric/ascii/child.rs";
    let before = format!("mod child;\n{NUMERIC_ASCII}");
    let after = format!("mod child;\n{NUMERIC_ASCII_SUPER}");
    let repo = numeric_fixture(
        &[
            ("src/numeric/ascii.rs", before.as_str()),
            (child, CHILD_CALLER),
        ],
        &after,
    );
    // 対照: フィルタがあれば、子モジュールは情報提供、範囲外は除外。
    let context = repo.run_json("context", &["--git"]);
    assert_eq!(
        caller_buckets(&context, child),
        ["informational_callers"],
        "{context}"
    );
    assert!(caller_buckets(&context, UNRELATED).is_empty(), "{context}");
    let output = cargo_bin()
        .args(["context", "--git", "--dir"])
        .arg(repo.root())
        .env("ASTRO_SIGHT_NO_CONFIDENCE_FILTER", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let context: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for caller in [child, UNRELATED] {
        assert_eq!(
            caller_buckets(&context, caller),
            ["impacted_callers"],
            "{caller}: {context}"
        );
    }
    let output = cargo_bin()
        .args(["impact", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .env("ASTRO_SIGHT_NO_CONFIDENCE_FILTER", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}
