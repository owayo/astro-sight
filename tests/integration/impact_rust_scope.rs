//! Rust のローカル束縛と変更関数の caller を区別する。

use super::support::*;

fn rust_scope_repo(consumer: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.write("api.rs", "pub fn comments() -> usize { 1 }\n");
    repo.write("consumer.rs", consumer);
    repo.init_git();
    repo.commit_all("initial");
    repo.write("api.rs", "pub fn comments() -> u32 { 1 }\n");
    repo
}

#[test]
fn impact_rust_scope_review_keeps_calls_through_same_named_function_aliases() {
    for (consumer, call_line) in [
        (
            "pub fn total() -> usize {\n let comments;\n (comments,) = (crate::api::comments,);\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let r#comments;\n r#comments = crate::api::comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let comments;\n let capture = || { comments = crate::api::comments; };\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let comments;\n comments = crate::api::comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let mut comments = || 1usize;\n comments = crate::api::comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total(mut comments: fn() -> usize) -> usize {\n comments = crate::api::comments;\n comments()\n}\n",
            2,
        ),
        (
            "use crate::api::comments;\npub fn total() -> usize {\n let comments;\n comments = crate::api::r#comments;\n comments()\n}\n",
            4,
        ),
        (
            "pub fn total() -> usize {\n let comments;\n comments = identity!(crate::api::comments);\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let comments = crate::api::comments;\n comments()\n}\n",
            2,
        ),
        (
            "use crate::api::comments;\npub fn total() -> usize {\n let comments = comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let comments = 1;\n let comments = crate::api::comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total(comments: usize) -> usize {\n let comments = crate::api::comments;\n comments()\n}\n",
            2,
        ),
        (
            "pub fn total() -> usize {\n let comments = 1;\n #[cfg(feature=\"x\")] let comments = crate::api::comments;\n comments()\n}\n",
            3,
        ),
        (
            "pub fn total() -> usize {\n let comments = identity!(crate::api::comments);\n comments()\n}\n",
            2,
        ),
        (
            "pub fn total() -> usize {\n let comments = || crate::api::comments();\n comments()\n}\n",
            2,
        ),
        (
            "pub fn total() -> usize {\n let r#comments = crate::api::comments;\n comments()\n}\n",
            2,
        ),
    ] {
        let repo = rust_scope_repo(consumer);
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{consumer}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "consumer.rs" && caller["line"] == call_line),
            "{consumer}: {context}"
        );
    }
}

#[test]
fn impact_rust_scope_review2_keeps_body_only_changes_when_same_named_items_are_added() {
    let before = "pub fn comments() -> usize {\n 1\n}\npub struct B;\n\n\n\n\n\n\n\n\n\n\n\n\n";
    let mut failures = Vec::new();
    for added in [
        "impl B { pub fn comments(&self) -> usize { 3 } }\n",
        "pub mod legacy { pub const comments: usize = 1; }\n",
    ] {
        let repo = TestRepo::new();
        repo.write("api.rs", before);
        repo.write(
            "consumer.rs",
            "pub fn f() -> usize { crate::api::comments() }\n",
        );
        repo.init_git();
        repo.commit_all("initial");
        repo.write("api.rs", format!("{}{added}", before.replace(" 1", " 2")));
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        let context = repo.run_json("context", &["--git"]);
        let blocking = context["changes"].as_array().unwrap().iter().any(|change| {
            change["impacted_callers"]
                .as_array()
                .is_some_and(|callers| !callers.is_empty())
        });
        if !output.status.success() || blocking {
            failures.push(format!(
                "{added}: exit={:?} {context}",
                output.status.code()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn impact_rust_scope_review_keeps_old_constant_patterns_when_kind_becomes_function() {
    for (consumer, expected_line) in [
        (
            "use crate::api::comments;\npub fn f(v: u8) -> u8 {\n let comments = v else { return 0; };\n comments\n}\n",
            3,
        ),
        (
            "pub fn f(v: u8) -> u8 {\n use crate::api::comments; let comments = v else { return 0; }; comments\n}\n",
            1,
        ),
    ] {
        let repo = TestRepo::new();
        repo.write(
            "api.rs",
            "#[allow(non_upper_case_globals)]\npub const comments: u8 = 1;\n",
        );
        repo.write("consumer.rs", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write("api.rs", "pub fn comments() -> u8 { 1 }\n");
        let context = repo.run_json("context", &["--git"]);
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{context}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "consumer.rs" && caller["line"] == expected_line),
            "{context}"
        );
    }
}

#[test]
fn impact_rust_scope_keeps_other_source_and_mixed_kind_changes() {
    for (before, after, other_source) in [
        (
            "pub fn comments() -> usize { 1 }\n",
            "pub fn comments() -> u32 { 1 }\n",
            true,
        ),
        (
            "pub fn comments() -> usize { 1 }\npub mod values {\n pub const comments: usize = 1;\n}\n",
            "pub fn comments() -> u32 { 1 }\npub mod values {\n pub const comments: u32 = 1;\n}\n",
            false,
        ),
        (
            "pub fn comments() -> usize { 1 }\n",
            "pub const comments: usize = 1;\n",
            false,
        ),
        (
            "pub struct comments;\n",
            "pub struct comments { pub value: u8 }\n",
            false,
        ),
    ] {
        let repo = TestRepo::new();
        repo.write("api.rs", before);
        if other_source {
            repo.write("values.rs", "pub const comments: usize = 1;\n");
        }
        repo.write("consumer.rs", "use crate::api::*;\nuse crate::values::*;\nfn f() {\n let comments = 1;\n comments;\n}\n");
        repo.init_git();
        repo.commit_all("initial");
        repo.write("api.rs", after);
        if other_source {
            repo.write("values.rs", "pub const comments: u32 = 1;\n");
        }
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{before} → {after}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let context = repo.run_json("context", &["--git"]);
        let changes = context["changes"].as_array().unwrap();
        let source = if other_source { "values.rs" } else { "api.rs" };
        let change = changes
            .iter()
            .find(|change| change["path"] == source)
            .unwrap();
        assert!(
            change["affected_symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|symbol| symbol["name"] == "comments" && symbol["kind"] != "function"),
            "{context}"
        );
        assert!(
            change["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "consumer.rs" && caller["line"] == 4),
            "{context}"
        );
        if other_source {
            let functions = changes
                .iter()
                .find(|change| change["path"] == "api.rs")
                .unwrap();
            assert!(
                functions["impacted_callers"]
                    .as_array()
                    .is_none_or(Vec::is_empty),
                "{context}"
            );
        }
        let mut expected = None;
        for workers in ["1", "3", "1"] {
            let output = cargo_bin()
                .args(["context", "--dir", repo.root().to_str().unwrap(), "--git"])
                .env("ASTRO_SIGHT_IMPACT_WORKERS", workers)
                .output()
                .unwrap();
            assert!(output.status.success());
            if let Some(expected) = &expected {
                assert_eq!(&output.stdout, expected, "workers={workers}");
            } else {
                let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(actual, context);
                expected = Some(output.stdout);
            }
        }
    }
}

#[test]
fn impact_rust_scope_preserves_public_refs_and_dead_code_counts() {
    let repo = rust_scope_repo(
        "use crate::api::comments;\nfn f() {\n let comments = vec![1];\n comments.len();\n let capture = || comments;\n}\n",
    );
    let refs = repo.run_json("refs", &["--name", "comments"]);
    assert_eq!(refs["refs"].as_array().unwrap().len(), 5, "{refs}");
    let dead = repo.run_json("dead-code", &[]);
    assert!(
        !dead["dead_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|symbol| symbol["name"] == "comments"),
        "{dead}"
    );
    let context = repo.run_json("context", &["--git"]);
    assert_eq!(context["changes"].as_array().unwrap().len(), 1);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{context}"
    );
}

#[test]
fn impact_rust_scope_excludes_local_patterns_parameters_and_captures() {
    for import in ["", "use crate::api::comments;\n", "use crate::api::*;\n"] {
        for source in [
            "fn f() { let comments = vec![1usize]; comments.len(); }",
            "fn f(comments: Vec<u8>) { comments.len(); }",
            "fn f((comments, _): (u8, u8)) { comments; }",
            "fn f() { let (comments, _) = pair; comments; }",
            "fn f() { let Data { comments, .. } = value; comments; }",
            "fn f() { let Data { x: comments } = value; comments; }",
            "fn f() { let Some(comments) = value else { return; }; comments; }",
            "fn f() { let comments = 1; { comments; } let capture = || comments; }",
            "fn f() { let comments = 1; let capture = async move { comments }; }",
            "mod tests { use super::*; fn f() { let comments = 1; comments; } }",
        ] {
            let repo = rust_scope_repo(&format!("{import}{source}\n"));
            let output = cargo_bin()
                .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{import}{source}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let context = repo.run_json("context", &["--git"]);
            let change = &context["changes"][0];
            assert_eq!(context["changes"].as_array().unwrap().len(), 1);
            for bucket in [
                "impacted_callers",
                "low_confidence_callers",
                "informational_callers",
            ] {
                assert!(
                    !change[bucket]
                        .as_array()
                        .is_some_and(|callers| callers.iter().any(|caller| caller["path"]
                            == "consumer.rs"
                            && !(bucket == "informational_callers"
                                && !import.is_empty()
                                && caller["line"] == 0))),
                    "{source}: {context}"
                );
            }
        }
    }
}

#[test]
fn impact_rust_scope_keeps_actual_calls_and_unproven_positions() {
    for (source, expected_line) in [
        (
            "fn f() {\n comments();\n let comments = vec![1];\n comments.len();\n}",
            2,
        ),
        (
            "fn f() {\n let comments = comments();\n comments.len();\n}",
            2,
        ),
        ("fn f() {\n let comments = || comments();\n}", 2),
        (
            "fn f() {\n let Some(comments) = value else { comments(); return; };\n}",
            2,
        ),
        ("fn f() {\n { let comments = 1; }\n comments();\n}", 3),
        (
            "fn f() {\n let comments = 1;\n fn inner() { comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n let x = const { comments() };\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n { use crate::api::comments; comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n { use crate::api::*; comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n #[cfg(feature=\"x\")] let comments = 1;\n comments();\n}",
            3,
        ),
        (
            "fn f(#[cfg(feature=\"x\")] comments: Vec<u8>) {\n comments();\n}",
            2,
        ),
        (
            "fn f() {\n let comments = 1;\n create_binding!();\n comments();\n}",
            4,
        ),
        (
            "fn f() {\n let comments = 1;\n crate::api::comments();\n}",
            3,
        ),
        ("fn f() {\n let comments = 1;\n dbg!(comments);\n}", 3),
        (
            "fn f() {\n let comments = 1;\n for value in values { comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n if let Some(x) = value { comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n while let Some(x) = value { comments(); }\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n match value { _ => comments() };\n}",
            3,
        ),
        (
            "fn f() {\n let comments = 1;\n comments();\n let broken = ;\n}",
            3,
        ),
    ] {
        let repo = rust_scope_repo(&format!("use crate::api::comments;\n{source}\n"));
        let output = cargo_bin()
            .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{source}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let context = repo.run_json("context", &["--git"]);
        assert!(
            context["changes"][0]["impacted_callers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|caller| caller["path"] == "consumer.rs" && caller["line"] == expected_line),
            "{source}: {context}"
        );
    }
}

#[test]
fn impact_rust_scope_original_local_declaration_is_not_a_caller() {
    let repo = TestRepo::new();
    repo.write("api.rs", "pub fn comments() -> usize { 1 }\n");
    repo.write(
        "consumer.rs",
        "pub fn group() -> usize {\n    let comments = vec![1usize];\n    comments.len()\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("api.rs", "pub fn comments() -> u32 { 1 }\n");
    let output = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let context = repo.run_json("context", &["--git"]);
    assert_eq!(context["changes"].as_array().unwrap().len(), 1);
    assert!(
        context["changes"][0]["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{context}"
    );
    let review = repo.run_json("review", &["--git"]);
    assert!(
        review["impact"]["changes"][0]["impacted_callers"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{review}"
    );
    assert!(
        !review["api_changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{review}"
    );
}
