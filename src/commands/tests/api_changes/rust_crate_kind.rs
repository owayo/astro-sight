//! bin-only / library crate の判別による API 差分の抑制テスト。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use crate::commands::*;
#[allow(unused_imports)]
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MissingCochange,
    MovedSymbol, PropertyToFieldChange, ReviewResult,
};
#[allow(unused_imports)]
use std::collections::HashSet;
#[allow(unused_imports)]
use std::fs;
#[allow(unused_imports)]
use std::io::Cursor;
#[allow(unused_imports)]
use std::process::Command;

/// 2026-04-24 レポート再現: binary crate (src/lib.rs なし) で新規 pub struct を
/// 追加し、同一 diff 内の別ファイルから use で取り込むケース。gitlab-cli の `MrDiff`
/// 追加と同じ構造。binary-only crate のため api.add の対象外になるべき。
#[test]
fn detect_api_changes_binary_rust_crate_excludes_pub_additions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"demo-bin\"
version = \"0.1.0\"
edition = \"2021\"

[dependencies]
";
    let models_before = "pub struct Issue { pub id: u32 }\n";
    let main_before = "\
use crate::models::Issue;

fn main() {
    let _ = Issue { id: 1 };
}

mod models;
";
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", cargo_toml),
            ("src/models.rs", models_before),
            ("src/main.rs", main_before),
        ],
        "initial",
    );

    // 新規 pub struct MrDiff を models.rs に追加し、main.rs の use に追随させる
    let models_after = "\
pub struct Issue { pub id: u32 }

pub struct MrDiff {
    pub old_path: String,
    pub new_path: String,
}
";
    let main_after = "\
use crate::models::{Issue, MrDiff};

fn main() {
    let _ = Issue { id: 1 };
    let _ = MrDiff { old_path: String::new(), new_path: String::new() };
}

mod models;
";
    fs::write(repo.join("src/models.rs"), models_after).expect("write models");
    fs::write(repo.join("src/main.rs"), main_after).expect("write main");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/models.rs".to_string(),
            new_path: "src/models.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/main.rs".to_string(),
            new_path: "src/main.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 8,
                new_start: 1,
                new_count: 8,
            }],
            deleted_old_source: None,
        },
    ];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !added.contains(&"MrDiff"),
        "binary crate (src/lib.rs なし) の新規 pub struct は api.add に出してはならない。got: {added:?}"
    );
}

/// library crate (src/lib.rs あり) では新規 pub シンボルを api.add に残す。
/// binary crate 判定の副作用で library crate のシンボルまで消さないことを保証する。
#[test]
fn detect_api_changes_library_rust_crate_keeps_pub_additions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"demo-lib\"
version = \"0.1.0\"
edition = \"2021\"
";
    let lib_before = "pub mod models;\n";
    let models_before = "pub struct Issue { pub id: u32 }\n";
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", cargo_toml),
            ("src/lib.rs", lib_before),
            ("src/models.rs", models_before),
        ],
        "initial",
    );

    // library crate に新規 pub struct を追加（同一 diff 内では参照しない）
    let models_after = "\
pub struct Issue { pub id: u32 }

pub struct LibraryApi { pub name: String }
";
    fs::write(repo.join("src/models.rs"), models_after).expect("write models");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/models.rs".to_string(),
        new_path: "src/models.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        added.contains(&"LibraryApi"),
        "library crate (src/lib.rs あり) の新規 pub struct は api.add に残すべき。got: {added:?}"
    );
}

/// 2026-05-19 レポート再現: binary crate (src/lib.rs なし) で `#[allow(dead_code)]`
/// 付き `pub fn` を削除した場合、直前 hook で `dead` 判定されたシンボルを削除した直後
/// に同じシンボルが `api.rm` として再警告される矛盾。bin-only crate の `pub fn` は
/// crate 外から到達できないため、`api.add` 側と対称に `api.rm` 側でも除外する。
#[test]
fn detect_api_changes_binary_rust_crate_excludes_pub_removals() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"demo-bin\"
version = \"0.1.0\"
edition = \"2021\"

[dependencies]
";
    let executor_before = "\
pub struct RusshExecutor;

impl RusshExecutor {
    pub fn new() -> Self { Self }

    #[allow(dead_code)]
    pub fn with_known_hosts(self, _path: &str) -> Self { self }
}
";
    let main_before = "\
use crate::executor::RusshExecutor;

fn main() {
    let _ = RusshExecutor::new();
}

mod executor;
";
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", cargo_toml),
            ("src/executor.rs", executor_before),
            ("src/main.rs", main_before),
        ],
        "initial",
    );

    // dead 判定済みの `with_known_hosts` を削除する
    let executor_after = "\
pub struct RusshExecutor;

impl RusshExecutor {
    pub fn new() -> Self { Self }
}
";
    fs::write(repo.join("src/executor.rs"), executor_after).expect("write executor");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/executor.rs".to_string(),
        new_path: "src/executor.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 9,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.iter().any(|n| n.ends_with("with_known_hosts")),
        "binary crate (src/lib.rs なし) の pub fn 削除は api.rm に出してはならない。got: {removed:?}"
    );
}

/// library crate (src/lib.rs あり) の pub fn 削除は引き続き api.rm に残ること。
/// binary crate 判定の副作用で library crate の削除まで抑止しないことを保証する。
#[test]
fn detect_api_changes_library_rust_crate_keeps_pub_removals() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"demo-lib\"
version = \"0.1.0\"
edition = \"2021\"
";
    let lib_before = "pub mod api;\n";
    let api_before = "\
pub struct Client;

impl Client {
    pub fn new() -> Self { Self }

    pub fn legacy_call(&self) {}
}
";
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", cargo_toml),
            ("src/lib.rs", lib_before),
            ("src/api.rs", api_before),
        ],
        "initial",
    );

    // 外部公開していた pub fn を削除する
    let api_after = "\
pub struct Client;

impl Client {
    pub fn new() -> Self { Self }
}
";
    fs::write(repo.join("src/api.rs"), api_after).expect("write api");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/api.rs".to_string(),
        new_path: "src/api.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 7,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.iter().any(|n| n.ends_with("legacy_call")),
        "library crate の pub fn 削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// 旧ツリーで library crate だったものを同一 diff で `src/lib.rs` 削除 + pub fn 削除に
/// する場合、`api.rm` は **旧 API 面** の判定なので base 時点の crate type を採用する。
/// 新ツリーが bin-only に見えても、削除された公開 API は引き続き api.rm に残ること。
/// (codex pre-commit レビューでの Warning 指摘の回帰テスト)
#[test]
fn detect_api_changes_lib_rs_removal_keeps_pub_removals_via_base() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"was-lib-now-bin\"
version = \"0.1.0\"
edition = \"2021\"
";
    let lib_before = "pub mod api;\n";
    let api_before = "\
pub fn kept() {}
pub fn removed_api() {}
";
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", cargo_toml),
            ("src/lib.rs", lib_before),
            ("src/api.rs", api_before),
        ],
        "initial",
    );

    // 新ツリーで src/lib.rs を削除し、同時に pub fn removed_api も消す
    std::fs::remove_file(repo.join("src/lib.rs")).expect("rm lib.rs");
    let api_after = "pub fn kept() {}\n";
    fs::write(repo.join("src/api.rs"), api_after).expect("write api");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib.rs".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: Some(lib_before.as_bytes().to_vec()),
        },
        crate::models::impact::DiffFile {
            old_path: "src/api.rs".to_string(),
            new_path: "src/api.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.iter().any(|n| n.ends_with("removed_api")),
        "base 時点で library crate だった場合、新ツリーで src/lib.rs を消しても旧公開 API の削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// `Cargo.toml` に `[lib] path = "src/api.rs"` のような custom lib path を書いた crate
/// では `src/lib.rs` が無くても library crate として扱う。`api.rm` 側で誤って公開 API
/// 削除を抑制しないことを保証する (codex pre-commit レビューでの P1 指摘の回帰テスト)。
#[test]
fn detect_api_changes_custom_lib_path_keeps_pub_removals() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"custom-lib\"
version = \"0.1.0\"
edition = \"2021\"

[lib]
path = \"src/api.rs\"
";
    let api_before = "\
pub fn kept() {}
pub fn removed_api() {}
";
    git_commit_files(
        repo,
        &[("Cargo.toml", cargo_toml), ("src/api.rs", api_before)],
        "initial",
    );

    let api_after = "pub fn kept() {}\n";
    fs::write(repo.join("src/api.rs"), api_after).expect("write api");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/api.rs".to_string(),
        new_path: "src/api.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.iter().any(|n| n.ends_with("removed_api")),
        "[lib] path = ... で構成される custom path library crate の pub fn 削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// ファイル丸ごと削除のケースでも、binary crate の pub fn は api.rm 対象外にする。
#[test]
fn detect_api_changes_binary_rust_crate_excludes_pub_removals_on_file_delete() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"demo-bin\"
version = \"0.1.0\"
edition = \"2021\"

[dependencies]
";
    let helper_before = "\
pub fn unused_helper() -> u32 { 42 }
";
    let main_before = "fn main() { println!(\"hi\"); }\nmod helper;\n";
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", cargo_toml),
            ("src/helper.rs", helper_before),
            ("src/main.rs", main_before),
        ],
        "initial",
    );

    // helper.rs を丸ごと削除
    std::fs::remove_file(repo.join("src/helper.rs")).expect("rm helper");
    let main_after = "fn main() { println!(\"hi\"); }\n";
    fs::write(repo.join("src/main.rs"), main_after).expect("write main");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/helper.rs".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: Some(helper_before.as_bytes().to_vec()),
        },
        crate::models::impact::DiffFile {
            old_path: "src/main.rs".to_string(),
            new_path: "src/main.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.iter().any(|n| n.ends_with("unused_helper")),
        "binary crate のファイル丸ごと削除に含まれる pub fn は api.rm に出してはならない。got: {removed:?}"
    );
}

/// 2026-05-20 レポート再現: bin-only crate の `pub fn` シグネチャ変更は外部公開 API の
/// 互換性問題ではなく内部リファクタなので、`api.mod` 対象外にする (api.add / api.rm と
/// 対称な動作)。同コミットで caller も更新済みのケース。
#[test]
fn detect_api_changes_binary_rust_crate_excludes_pub_method_signature_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"demo-bin\"
version = \"0.1.0\"
edition = \"2021\"

[dependencies]
";
    let store_before = "\
pub struct CredentialStore;

impl CredentialStore {
    pub fn get_or_prompt(&mut self, _group: &str, _user: &str, _hint: &str) -> Result<&str, String> {
        Ok(\"password\")
    }
}
";
    let main_before = "\
fn main() {
    use crate::store::CredentialStore;
    let mut s = CredentialStore;
    let _ = s.get_or_prompt(\"g\", \"u\", \"h\");
}

mod store;
";
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", cargo_toml),
            ("src/store.rs", store_before),
            ("src/main.rs", main_before),
        ],
        "initial",
    );

    // シグネチャ変更: 戻り値を `&str` → `(&str, &str)` に拡張、caller も同コミットで追随
    let store_after = "\
pub struct CredentialStore;

impl CredentialStore {
    pub fn get_or_prompt(&mut self, _group: &str, _default_user: &str, _hint: &str) -> Result<(&str, &str), String> {
        Ok((\"user\", \"password\"))
    }
}
";
    let main_after = "\
fn main() {
    use crate::store::CredentialStore;
    let mut s = CredentialStore;
    let _ = s.get_or_prompt(\"g\", \"u\", \"h\");
}

mod store;
";
    fs::write(repo.join("src/store.rs"), store_after).expect("write store");
    fs::write(repo.join("src/main.rs"), main_after).expect("write main");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/store.rs".to_string(),
            new_path: "src/store.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 7,
                new_start: 1,
                new_count: 7,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/main.rs".to_string(),
            new_path: "src/main.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 7,
                new_start: 1,
                new_count: 7,
            }],
            deleted_old_source: None,
        },
    ];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let modified: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !modified.iter().any(|n| n.ends_with("get_or_prompt")),
        "binary crate の pub method シグネチャ変更は api.mod に出してはならない。got: {modified:?}"
    );
}

/// library crate (src/lib.rs あり) の pub fn シグネチャ変更は引き続き `api.mod` に残る。
/// binary crate 判定の副作用で library crate まで抑止しないことを保証する。
#[test]
fn detect_api_changes_library_rust_crate_keeps_pub_signature_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"demo-lib\"
version = \"0.1.0\"
edition = \"2021\"
";
    let lib_before = "pub mod api;\n";
    let api_before = "\
pub fn legacy_call(_x: u32) -> u32 { 0 }
";
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", cargo_toml),
            ("src/lib.rs", lib_before),
            ("src/api.rs", api_before),
        ],
        "initial",
    );

    // シグネチャ変更: 引数追加
    let api_after = "\
pub fn legacy_call(_x: u32, _y: u32) -> u32 { 0 }
";
    fs::write(repo.join("src/api.rs"), api_after).expect("write api");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/api.rs".to_string(),
        new_path: "src/api.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let modified: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        modified.iter().any(|n| n.ends_with("legacy_call")),
        "library crate の pub fn シグネチャ変更は引き続き api.mod に残るべき。got: {modified:?}"
    );
}

/// base 時点で library crate だったが、新ツリーで `src/lib.rs` を削除して
/// シグネチャ変更を行ったケース。`api.mod` は「旧版でも新版でも外部公開 API だった
/// symbol」を対象にすべきなので、旧側基準で library 扱いとなり、新側で bin-only
/// になっていても旧公開 API のシグネチャ変更は api.mod から除外する
/// (codex 設計相談で「old または new のどちらかが bin-only なら除外」採用)。
#[test]
fn detect_api_changes_lib_to_bin_transition_excludes_pub_signature_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"was-lib-now-bin\"
version = \"0.1.0\"
edition = \"2021\"
";
    let lib_before = "pub mod api;\n";
    let api_before = "pub fn frob(_x: u32) -> u32 { 0 }\n";
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", cargo_toml),
            ("src/lib.rs", lib_before),
            ("src/api.rs", api_before),
        ],
        "initial",
    );

    // 新ツリーで src/lib.rs を削除 + シグネチャ変更
    std::fs::remove_file(repo.join("src/lib.rs")).expect("rm lib.rs");
    fs::write(
        repo.join("src/api.rs"),
        "pub fn frob(_x: u32, _y: u32) -> u32 { 0 }\n",
    )
    .expect("write api");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib.rs".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: Some(lib_before.as_bytes().to_vec()),
        },
        crate::models::impact::DiffFile {
            old_path: "src/api.rs".to_string(),
            new_path: "src/api.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let modified: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !modified.iter().any(|n| n.ends_with("frob")),
        "lib → bin 化 + シグネチャ変更のケースは api.mod に出さない (crate target 変更として扱う)。got: {modified:?}"
    );
}

// ------------------------------------------------------------------
// is_binary_only_rust_crate ヘルパー
// ------------------------------------------------------------------

#[test]
fn is_binary_only_rust_crate_true_when_no_lib_rs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"b\"\n").expect("cargo");
    fs::create_dir_all(repo.join("src")).expect("mkdir src");
    fs::write(repo.join("src/main.rs"), "fn main() {}\n").expect("main");

    assert!(is_binary_only_rust_crate(
        repo.to_str().expect("utf-8"),
        "src/main.rs",
    ));
}

#[test]
fn is_binary_only_rust_crate_false_when_lib_rs_present() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"l\"\n").expect("cargo");
    fs::create_dir_all(repo.join("src")).expect("mkdir src");
    fs::write(repo.join("src/lib.rs"), "pub fn public_api() {}\n").expect("lib");

    assert!(!is_binary_only_rust_crate(
        repo.to_str().expect("utf-8"),
        "src/lib.rs",
    ));
}

#[test]
fn is_binary_only_rust_crate_false_for_non_rust_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"b\"\n").expect("cargo");

    assert!(!is_binary_only_rust_crate(
        repo.to_str().expect("utf-8"),
        "src/main.py",
    ));
}

#[test]
fn is_binary_only_rust_crate_false_without_cargo_toml() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    fs::create_dir_all(repo.join("src")).expect("mkdir src");
    fs::write(repo.join("src/main.rs"), "fn main() {}\n").expect("main");

    assert!(!is_binary_only_rust_crate(
        repo.to_str().expect("utf-8"),
        "src/main.rs",
    ));
}

/// `Cargo.toml` に `[lib] path = "..."` を書いて `src/lib.rs` を使わず custom path で
/// library crate を構成しているケース。`src/lib.rs` の有無だけ見ると binary-only と
/// 誤判定し、本物の公開 API 削除を `api.rm` から除外してしまうため、`[lib]` セクション
/// 存在を判定要件に含める。
#[test]
fn is_binary_only_rust_crate_false_when_cargo_lib_section_with_custom_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    fs::create_dir_all(repo.join("src")).expect("mkdir src");
    let cargo_toml = "[package]\nname = \"custom\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/api.rs\"\n";
    fs::write(repo.join("Cargo.toml"), cargo_toml).expect("cargo");
    fs::write(repo.join("src/api.rs"), "pub fn hello() {}\n").expect("api");

    assert!(!is_binary_only_rust_crate(
        repo.to_str().expect("utf-8"),
        "src/api.rs",
    ));
}

// ------------------------------------------------------------------
// cargo_toml_text_declares_lib ヘルパー
// ------------------------------------------------------------------

#[test]
fn cargo_toml_text_declares_lib_true_when_lib_section_present() {
    let text = "[package]\nname = \"x\"\n\n[lib]\npath = \"src/api.rs\"\n";
    assert!(cargo_toml_text_declares_lib(text));
}

#[test]
fn cargo_toml_text_declares_lib_false_when_lib_section_absent() {
    let text = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n";
    assert!(!cargo_toml_text_declares_lib(text));
}

#[test]
fn cargo_toml_text_declares_lib_false_when_empty() {
    // 空 TOML は library 宣言なし
    assert!(!cargo_toml_text_declares_lib(""));
}

/// 不正な TOML は `api.rm` の見逃しを避けるため保守的に true (= library 扱い) を返す。
#[test]
fn cargo_toml_text_declares_lib_true_when_invalid_toml() {
    let text = "this is = not valid = toml\n[unclosed";
    assert!(cargo_toml_text_declares_lib(text));
}

/// `[[bin]]` セクションだけがあって `[lib]` がない場合は binary-only として扱う。
#[test]
fn cargo_toml_text_declares_lib_false_when_only_bin_array_section() {
    let text = "[package]\nname = \"x\"\n\n[[bin]]\nname = \"x\"\npath = \"src/main.rs\"\n";
    assert!(!cargo_toml_text_declares_lib(text));
}
