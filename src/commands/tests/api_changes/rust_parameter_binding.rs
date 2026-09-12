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
