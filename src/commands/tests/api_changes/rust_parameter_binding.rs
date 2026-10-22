//! Rust の引数束縛モードと型契約を区別する API 差分テスト。

use crate::commands::tests::common::*;
use crate::commands::*;
use std::fs;

#[test]
fn rust_parameter_binding_mut_is_not_api_mod_but_type_mutability_is() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib.rs",
                "pub mod consumer;\n\npub fn apply<F: FnMut(u32)>(mut callback: F) {\n    drop(callback);\n}\n\npub fn borrow(value: &mut u32) -> u32 {\n    *value\n}\n",
            ),
            (
                "src/consumer.rs",
                "use crate::{apply, borrow};\n\npub fn run(value: &mut u32) {\n    apply(|_| {});\n    let _ = borrow(value);\n}\n",
            ),
        ],
        "initial",
    );

    fs::write(
        repo.join("src/lib.rs"),
        "pub mod consumer;\n\npub fn apply<F: FnMut(u32)>(callback: F) {\n    drop(callback);\n}\n\npub fn borrow(value: &u32) -> u32 {\n    *value\n}\n",
    )
    .expect("write changed lib.rs");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib.rs".to_string(),
        new_path: "src/lib.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 9,
            new_start: 1,
            new_count: 9,
        }],
        deleted_old_source: None,
    }];

    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        !api.modified.iter().any(|change| change.name == "apply"),
        "binding-side の mut 除去は api.mod に出すべきでない: {:?}",
        api.modified
    );
    assert!(
        api.modified.iter().any(|change| change.name == "borrow"),
        "型側の &mut T -> &T は引き続き api.mod に出すべき: {:?}",
        api.modified
    );
}

#[test]
fn rust_parameter_patterns_keep_api_names_and_method_containers() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let old = "pub trait Worker {\n    fn call(&self, _n: u8);\n    fn pair(&self, (a, b): (u8, u8)) -> u8 { a + b }\n}\npub struct A;\npub struct B;\nimpl A { pub fn apply(_n: u8) {} }\nimpl B { pub fn apply(_n: u8) {} }\npub fn double(_n: i32) -> i32 { 0 }\npub fn sum((a, b): (i32, i32)) -> i32 { a + b }\npub fn changed(_old: i32) {}\n";
    let new = "pub trait Worker {\n    fn call(&self, n: u8);\n    fn pair(&self, (x, y): (u8, u8)) -> u8 { x + y }\n}\npub struct A;\npub struct B;\nimpl A { pub fn apply(n: u8) {} }\nimpl B { pub fn apply(n: u16) {} }\npub fn double(n: i32) -> i32 { n * 2 }\npub fn sum((x, y): (i32, i32)) -> i32 { x + y }\npub fn changed(new: i64) {}\n";
    git_commit_files(
        repo,
        &[
            ("src/lib.rs", old),
            (
                "src/consumer.rs",
                "use crate::{Worker, A, B, double, sum, changed};\nfn run(w: &dyn Worker) { w.call(1); w.pair((1, 2)); A::apply(1); B::apply(1); double(1); sum((1, 2)); changed(1); }\n",
            ),
        ],
        "initial",
    );
    fs::write(repo.join("src/lib.rs"), new).unwrap();
    let diff = crate::engine::diff::parse_unified_diff(
        &String::from_utf8(
            std::process::Command::new("git")
                .args(["diff", "HEAD"])
                .current_dir(repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap(),
    );
    let api = detect_api_changes(repo.to_str().unwrap(), "HEAD", &diff);
    let mut names: Vec<_> = api
        .modified
        .iter()
        .map(|change| change.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["B.apply", "changed"], "{api:?}");
    let change = api
        .modified
        .iter()
        .find(|change| change.name == "changed")
        .unwrap();
    assert!(change.old_signature.as_ref().unwrap().contains("_old: i32"));
    assert!(change.new_signature.as_ref().unwrap().contains("new: i64"));
}

#[test]
fn rust_parent_macro_attributes_keep_parameter_renames_in_api_mod() {
    for (prefix, suffix) in [
        ("#[pymethods]\nimpl S {\n", "}\n"),
        ("#[bridge]\npub mod ffi {\n", "}\n"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let old = format!("pub struct S;\n{prefix}pub fn apply(_n: u8) {{}}\n{suffix}");
        let new = format!("pub struct S;\n{prefix}pub fn apply(n: u8) {{}}\n{suffix}");
        git_commit_files(
            repo,
            &[
                ("src/lib.rs", &old),
                (
                    "src/consumer.rs",
                    "use crate::S;\npub fn run() { S::apply(1); }\n",
                ),
            ],
            "initial",
        );
        fs::write(repo.join("src/lib.rs"), new).unwrap();
        let diff = crate::engine::diff::parse_unified_diff(
            &String::from_utf8(
                std::process::Command::new("git")
                    .args(["diff", "HEAD"])
                    .current_dir(repo)
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap(),
        );
        let api = detect_api_changes(repo.to_str().unwrap(), "HEAD", &diff);
        assert!(
            api.modified
                .iter()
                .any(|change| change.name.ends_with("apply")),
            "{prefix}: {api:?}"
        );
    }
}

/// 1 行 trait の表示 signature は容器の本文も含む。fn の証明で容器全体を消さない。
#[test]
fn rust_one_line_trait_container_is_a_known_conservative_limitation() {
    for (old, new) in [
        (
            "pub trait Worker { fn call(&self, _n: u8); }\n",
            "pub trait Worker { fn call(&self, n: u8); }\n",
        ),
        (
            "pub trait Worker { type Out; fn call(&self, _n: u8); }\n",
            "pub trait Worker { type Out: Copy; fn call(&self, n: u8); }\n",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("src/lib.rs", old),
                (
                    "src/consumer.rs",
                    "use crate::Worker;\nfn run<W: Worker>(w: &W) { w.call(1); }\n",
                ),
            ],
            "initial",
        );
        fs::write(repo.join("src/lib.rs"), new).unwrap();
        let text = std::process::Command::new("git")
            .args(["diff", "HEAD"])
            .current_dir(repo)
            .output()
            .unwrap();
        let diff =
            crate::engine::diff::parse_unified_diff(&String::from_utf8(text.stdout).unwrap());
        let api = detect_api_changes(repo.to_str().unwrap(), "HEAD", &diff);
        let names: Vec<_> = api
            .modified
            .iter()
            .map(|change| change.name.as_str())
            .collect();
        assert_eq!(names, ["Worker"], "{api:?}");
    }
}
