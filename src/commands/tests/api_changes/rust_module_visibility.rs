//! Rust の module 可視性による公開 API 面判定のテスト。

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

/// private module (`mod meeting;`) 配下の新規 pub fn は crate 外から到達できないため
/// api.add に出さない (パターンC)。
#[test]
fn detect_api_changes_private_module_pub_fn_excluded_from_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "mod meeting;\n"),
            ("src/meeting/mod.rs", "pub mod detector;\n"),
        ],
        "base",
    );
    fs::write(
        repo.join("src/meeting/detector.rs"),
        "pub fn create_detector() -> u32 {\n    0\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "/dev/null".to_string(),
        new_path: "src/meeting/detector.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !added.iter().any(|n| n.ends_with("create_detector")),
        "private module 配下の pub fn は api.add に出ない。got: {added:?}"
    );
}

/// `pub mod` 経路で到達可能なモジュール配下の新規 pub fn は api.add に出る。
#[test]
fn detect_api_changes_public_module_pub_fn_included_in_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "pub mod meeting;\n"),
            ("src/meeting/mod.rs", "pub mod detector;\n"),
        ],
        "base",
    );
    fs::write(
        repo.join("src/meeting/detector.rs"),
        "pub fn create_detector() -> u32 {\n    0\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "/dev/null".to_string(),
        new_path: "src/meeting/detector.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        added.iter().any(|n| n.ends_with("create_detector")),
        "pub mod 経路で到達可能な pub fn は api.add に出る。got: {added:?}"
    );
}

/// new と base 両方で private module 配下の pub fn の signature 変更は api.mod に出さない。
#[test]
fn detect_api_changes_private_module_signature_change_excluded_from_mod() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "mod meeting;\n"),
            ("src/meeting/mod.rs", "pub mod detector;\n"),
            (
                "src/meeting/detector.rs",
                "pub fn create_detector(id: u32) -> u32 {\n    id\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
        repo.join("src/meeting/detector.rs"),
        "pub fn create_detector(id: u32, extra: bool) -> u32 {\n    id\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/meeting/detector.rs".to_string(),
        new_path: "src/meeting/detector.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let flagged = api
        .modified
        .iter()
        .any(|m| m.name.ends_with("create_detector"))
        || api
            .modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("create_detector"));
    assert!(
        !flagged,
        "new/base 両方 private module 配下の signature 変更は api.mod に出ない。mod={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}

/// base で公開 (pub mod) だったモジュールを同 diff で private 化しつつ配下 pub fn の
/// signature を変えた場合、旧 API の破壊的変更なので api.mod に残す (codex 指摘2)。
#[test]
fn detect_api_changes_module_made_private_in_diff_keeps_mod_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "pub mod meeting;\n"),
            ("src/meeting/mod.rs", "pub mod detector;\n"),
            (
                "src/meeting/detector.rs",
                "pub fn create_detector(id: u32) -> u32 {\n    id\n}\n",
            ),
        ],
        "base",
    );
    // meeting を private 化 (pub mod → mod) しつつ create_detector の signature を変更
    fs::write(repo.join("src/lib.rs"), "mod meeting;\n").expect("write");
    fs::write(
        repo.join("src/meeting/detector.rs"),
        "pub fn create_detector(id: u32, extra: bool) -> u32 {\n    id\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib.rs".to_string(),
            new_path: "src/lib.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/meeting/detector.rs".to_string(),
            new_path: "src/meeting/detector.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified
            .iter()
            .any(|m| m.name.ends_with("create_detector")),
        "base で公開だったモジュールの private 化 + signature 変更は blocking。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// crate-private module (`mod wifi`) 配下の pub fn をファイルごと削除しても、crate 外
/// 非到達 = 外部 API ではないため api.rm (removed / removed_dead) に出さない
/// (Issue 2026-06-05-wifi-module-removal: Tauri アプリの内部 mod 削除誤検出対策)。
#[test]
fn detect_api_changes_private_module_pub_fn_file_delete_excluded_from_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let wifi_src = "pub fn found() -> u32 {\n    0\n}\npub fn failed() -> u32 {\n    1\n}\n";
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "mod wifi;\n"),
            ("src/wifi/mod.rs", wifi_src),
        ],
        "base",
    );
    // wifi モジュールを丸ごと削除
    std::fs::remove_file(repo.join("src/wifi/mod.rs")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 6,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(wifi_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed
            .iter()
            .any(|n| n.ends_with("found") || n.ends_with("failed")),
        "private module 配下の pub fn 削除 (ファイル丸ごと) は api.rm に出ない。got: {removed:?}"
    );
}

/// crate-private module 配下の pub fn を同一ファイル内で一部だけ削除した場合も、
/// (同一 crate 内に caller が残っていても) crate 外非到達なので api.rm に出さない。
#[test]
fn detect_api_changes_private_module_pub_fn_same_file_removal_excluded_from_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "mod wifi;\n"),
            (
                "src/wifi/mod.rs",
                "pub mod caller;\npub fn found() -> u32 {\n    0\n}\npub fn kept() -> u32 {\n    1\n}\n",
            ),
            (
                "src/wifi/caller.rs",
                "pub fn call() -> u32 {\n    super::found()\n}\n",
            ),
        ],
        "base",
    );
    // found だけ削除し kept は残す (caller の super::found() 参照は残存)
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub mod caller;\npub fn kept() -> u32 {\n    1\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 2,
            old_count: 3,
            new_start: 2,
            new_count: 0,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.iter().any(|n| n.ends_with("found")),
        "private module 配下の pub fn 削除 (同一ファイル一部) は api.rm に出ない。got: {removed:?}"
    );
}

/// 同一 old_path に複数の private-module pub fn 削除がある場合でも、全件が api.rm から
/// 除外される。base 側 crate 判定 (`is_binary_only_at_base` / `private_module_info_at_base`) を
/// per-symbol で `git show` し直さず old_path 単位でメモ化する perf #1 の behavior-preserving
/// 回帰テスト (メモ有無で結果が一致することを担保)。
#[test]
fn detect_api_changes_private_module_multiple_pub_fn_removal_all_excluded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "mod wifi;\n"),
            (
                "src/wifi/mod.rs",
                "pub fn found() -> u32 {\n    0\n}\npub fn scanned() -> u32 {\n    1\n}\npub fn kept() -> u32 {\n    2\n}\n",
            ),
        ],
        "base",
    );
    // found と scanned を削除、kept は残す (同一 old_path で 2 symbol が memo パスを踏む)
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn kept() -> u32 {\n    2\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 6,
            new_start: 1,
            new_count: 0,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed
            .iter()
            .any(|n| n.ends_with("found") || n.ends_with("scanned")),
        "private module 配下の複数 pub fn 削除は全件 api.rm に出ない。got: {removed:?}"
    );
}

/// base で公開 (pub mod) だったモジュール配下の pub fn を、同一 diff で private 化しつつ
/// 削除した場合、旧 API は base 時点で公開だったため api.rm に残す (at_base 判定)。
#[test]
fn detect_api_changes_module_made_private_in_diff_keeps_removal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "pub mod wifi;\n"),
            (
                "src/wifi/mod.rs",
                "pub fn found() -> u32 {\n    0\n}\npub fn kept() -> u32 {\n    1\n}\n",
            ),
        ],
        "base",
    );
    // wifi を private 化 (pub mod → mod) しつつ found を削除
    fs::write(repo.join("src/lib.rs"), "mod wifi;\n").expect("write");
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn kept() -> u32 {\n    1\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib.rs".to_string(),
            new_path: "src/lib.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/wifi/mod.rs".to_string(),
            new_path: "src/wifi/mod.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.iter().any(|n| n.ends_with("found")),
        "base で pub mod だったモジュールの pub fn 削除は private 化しても api.rm に残すべき。got: {removed:?}"
    );
}

/// `pub mod` の階層 (`pub mod outer;` + `outer.rs` 内 `pub mod inner;`) 配下の pub fn 削除は
/// 外部公開 API の削除なので api.rm に残す。
///
/// pub 到達 module 集合は `collect_all_modules` の pub 子リストから導出する
/// (旧実装は pub 経路専用の walk を別に持ち、file-style module の子解決を
/// 片方だけ直して階層 module を取りこぼした経緯がある)。この経路の回帰テスト。
#[test]
fn detect_api_changes_nested_pub_mod_file_style_removal_stays_in_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "pub mod outer;\n"),
            // file-style module: outer.rs の子は outer/ 配下を指す
            ("src/outer.rs", "pub mod inner;\n"),
            ("src/outer/inner.rs", "pub fn deep() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/outer/inner.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/outer/inner.rs".to_string(),
        new_path: "src/outer/inner.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.iter().any(|n| n.ends_with("deep")),
        "pub mod 階層 (file-style module) 配下の pub fn 削除は外部公開 API の削除。got: {removed:?}"
    );
}

/// private な中間 module (`mod outer;`) 配下の pub fn 削除は外部到達不能なので api.rm から外す。
/// pub 到達性の導出が「pub 子リストのみを辿る」ことの回帰テスト
/// (親が private なら子が `pub mod` でも到達不能)。
#[test]
fn detect_api_changes_pub_mod_under_private_parent_removal_excluded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "mod outer;\n"),
            ("src/outer.rs", "pub mod inner;\n"),
            ("src/outer/inner.rs", "pub fn deep() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/outer/inner.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/outer/inner.rs".to_string(),
        new_path: "src/outer/inner.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.iter().any(|n| n.ends_with("deep")),
        "private 親 module 配下は pub mod でも外部到達不能なので api.rm に出さない。got: {removed:?}"
    );
}

/// 削除対象シンボルが file 内 inline `mod_item` の body に定義されている場合、
/// ファイルパスベースの module_segments と実 module path がずれて edge graph seed が
/// 誤合致するため、fail-closed で抑制を諦め `api.rm` に残す。
/// (codex Step B コミット前レビュー 1 回目の Warning 指摘の回帰テスト)
#[test]
fn detect_api_changes_inline_child_mod_pub_fn_deletion_stays_in_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "mod wifi;\npub mod prelude;\n"),
            ("src/prelude.rs", "pub use crate::wifi::scanner::scan;\n"),
            (
                "src/wifi.rs",
                "pub mod scanner { pub fn scan() -> u32 { 0 } }\n",
            ),
        ],
        "base",
    );
    // scan を削除
    fs::write(repo.join("src/wifi.rs"), "pub mod scanner {}\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi.rs".to_string(),
        new_path: "src/wifi.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.iter().any(|n| n.ends_with("scan")),
        "inline child mod 内の pub fn 削除は fail-closed で api.rm に残すべき。got: {removed:?}"
    );
}

/// 新規追加シンボルが file 内 inline `mod_item` の body にある場合も fail-closed で
/// `api.add` に残す。target 側 inline module の false negative を防ぐ。
#[test]
fn detect_api_changes_inline_child_mod_pub_fn_addition_stays_in_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "mod wifi;\npub mod prelude;\n"),
            ("src/prelude.rs", "pub use crate::wifi::scanner::scan;\n"),
            ("src/wifi.rs", "pub mod scanner {}\n"),
        ],
        "base",
    );
    // scan を inline mod 内に新規追加
    fs::write(
        repo.join("src/wifi.rs"),
        "pub mod scanner { pub fn scan() -> u32 { 0 } }\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi.rs".to_string(),
        new_path: "src/wifi.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        added.iter().any(|n| n.ends_with("scan")),
        "inline child mod 内の pub fn 新規追加は fail-closed で api.add に残すべき。got: {added:?}"
    );
}

/// Rust の `pub mod foo;` 宣言追加は api.add に出してはならない。
/// モジュール宣言はファイル構成の整理であり、公開 API 面としての意味が薄いため
/// `filter_exported_symbols` で `SymbolKind::Module` を除外している。
/// (Stop hook 改善時に導入。`extract_all_callees` 追加コミットで Stop hook が
/// `pub mod generated;` を api.add 通知した問題の再発防止)
#[test]
fn detect_api_changes_skips_module_declaration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
pub mod existing;
pub fn hello() {}
";
    git_commit_files(repo, &[("src/lib.rs", before)], "initial");

    // 新規モジュール宣言を追加 (副ファイルは存在しなくても tree-sitter パースには影響しない)
    let after = "\
pub mod existing;
pub mod generated;
pub fn hello() {}
";
    fs::write(repo.join("src/lib.rs"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib.rs".to_string(),
        new_path: "src/lib.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

    assert!(
        !added.contains(&"generated"),
        "pub mod 追加は api.add に出してはならない。got: {added:?}"
    );
    assert!(
        !added.contains(&"existing"),
        "既存 pub mod も api.add に出してはならない。got: {added:?}"
    );
}

/// `pub(crate) struct` の inherent impl 内 `pub fn` のシグネチャ変更は外部公開 API の
/// 変更ではない (実効可視性 = min(宣言, 所有型, モジュール))。
///
/// レシーバ型がクレート外から到達できない以上メソッドも呼べないが、宣言の `pub` だけを
/// 見ていたため内部リファクタのたびに blocking な api.mod が出ていた。
/// 同名メソッドを持つ別の型を置いて `is_modified_closed_in_diff` の
/// 「同名複数定義 → 即 blocking」ガードを通過させ、実効可視性の判定だけを検証する。
/// owner が `pub struct` の場合は blocking のまま残ること (対照) も固定する。
#[test]
fn detect_api_changes_crate_internal_owner_type_excludes_method_from_mod() {
    // (owner の可視性修飾, api.mod に出るべきか)
    for (owner_vis, expect_flagged) in [("pub(crate) ", false), ("pub ", true)] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let holder_before = format!(
            "{owner_vis}struct Holder;\n\n\
impl Holder {{\n    pub fn value(&self, key: &str) -> u8 {{\n        key.len() as u8\n    }}\n}}\n\n\
pub fn entry() -> u8 {{\n    Holder.value(\"a\")\n}}\n"
        );
        git_commit_files(
            repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
                ),
                ("src/lib.rs", "pub mod holder;\npub mod twin;\n"),
                // 同名メソッドを別の型にも置き、qualname の同名複数定義ガードを通す
                (
                    "src/twin.rs",
                    "pub struct Twin;\n\nimpl Twin {\n    pub fn value(&self) -> u8 { 0 }\n}\n",
                ),
                ("src/holder.rs", holder_before.as_str()),
            ],
            "base",
        );
        let holder_after = format!(
            "{owner_vis}struct Holder;\n\n\
impl Holder {{\n    pub fn value(&self, key: &str, extra: u8) -> u8 {{\n        key.len() as u8 + extra\n    }}\n}}\n\n\
pub fn entry() -> u8 {{\n    Holder.value(\"a\", 1)\n}}\n"
        );
        fs::write(repo.join("src/holder.rs"), &holder_after).expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/holder.rs".to_string(),
            new_path: "src/holder.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 10,
                new_start: 1,
                new_count: 10,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let flagged = api.modified.iter().any(|m| m.name.ends_with("value"));
        assert_eq!(
            flagged,
            expect_flagged,
            "owner が `{owner_vis}struct` のとき api.mod に出るか: {:?}",
            api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
    }
}

/// レシーバ型の宣言が同一ファイルに無い (別ファイルの型への inherent impl) 場合は、
/// 実効可視性を確定できないため fail-closed で公開扱いを維持する。
#[test]
fn detect_api_changes_owner_type_in_another_file_keeps_mod_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            (
                "src/lib.rs",
                "pub mod types;\npub mod ext;\npub mod twin;\n",
            ),
            ("src/types.rs", "pub(crate) struct Holder;\n"),
            (
                "src/twin.rs",
                "pub struct Twin;\n\nimpl Twin {\n    pub fn value(&self) -> u8 { 0 }\n}\n",
            ),
            (
                "src/ext.rs",
                "use crate::types::Holder;\n\n\
impl Holder {\n    pub fn value(&self, key: &str) -> u8 {\n        key.len() as u8\n    }\n}\n\n\
pub fn entry() -> u8 {\n    Holder.value(\"a\")\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
        repo.join("src/ext.rs"),
        "use crate::types::Holder;\n\n\
impl Holder {\n    pub fn value(&self, key: &str, extra: u8) -> u8 {\n        key.len() as u8 + extra\n    }\n}\n\n\
pub fn entry() -> u8 {\n    Holder.value(\"a\", 1)\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/ext.rs".to_string(),
        new_path: "src/ext.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 10,
            new_start: 1,
            new_count: 10,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified.iter().any(|m| m.name.ends_with("value")),
        "owner 型が別ファイルなら実効可視性を確定できないので blocking 維持: {:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}
