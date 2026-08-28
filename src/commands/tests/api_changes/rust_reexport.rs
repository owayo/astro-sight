//! Rust の `pub use` 再エクスポート経路による公開 API 面判定のテスト。

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

/// 定義行と re-export 行しか無い新規 export は `refs_internal` が 0 になる。
/// `export type { X }` は公開エクスポート経路であり実利用ではないため数えない
/// (これを数えると「参照あり」に見え、`refs_internal` で未参照を判別できなくなる)
#[test]
fn detect_api_changes_ts_reexport_only_symbol_has_zero_internal_refs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(
        repo,
        &[("module.ts", "export interface Kept {\n  id: string;\n}\n")],
        "initial",
    );

    // Orphan: 参照ゼロ。Local: 同一ファイル内の `export type { Local }` のみ。
    fs::write(
        repo.join("module.ts"),
        "export interface Kept {\n  id: string;\n}\n\nexport interface Orphan {\n  at: string;\n}\n\ninterface Local {\n  v: number;\n}\nexport type { Local };\n",
    )
    .expect("write module.ts");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "module.ts".to_string(),
        new_path: "module.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 12,
        }],
        deleted_old_source: None,
    }];

    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    for name in ["Orphan", "Local"] {
        let symbol = api
            .added
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} は api.add に載るべき: {:?}", api.added));
        assert_eq!(
            symbol.refs_internal, 0,
            "定義 / re-export だけの参照は実利用として数えない: {symbol:?}"
        );
    }
}

/// commands.rs を子モジュールに分割し、親で `pub use sub::name;` で再エクスポートした
/// ケース。利用者から見た公開 API (`crate::name`) は維持されているため `api.rm` に出さない。
/// (2026-06-06 trace report: Rust pub use re-export を api.rm 抑制対象に追加)
#[test]
fn detect_api_changes_rust_pub_use_reexport_excludes_api_rm() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    // base: lib.rs に pub fn を直接定義
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            (
                "src/lib.rs",
                "pub const MAX_INPUT_SIZE: usize = 100;\npub fn serialize_output() {}\n",
            ),
        ],
        "base",
    );
    // new: 定義を子モジュールに移動し、lib.rs で pub use 再エクスポート
    fs::write(
        repo.join("src/lib.rs"),
        "mod common;\n\
         pub use common::{MAX_INPUT_SIZE, serialize_output};\n",
    )
    .expect("write lib.rs");
    fs::create_dir_all(repo.join("src")).expect("mkdir");
    fs::write(
        repo.join("src/common.rs"),
        "pub const MAX_INPUT_SIZE: usize = 100;\npub fn serialize_output() {}\n",
    )
    .expect("write common.rs");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib.rs".to_string(),
            new_path: "src/lib.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/common.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        },
    ];

    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api.removed.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !removed.iter().any(|n| n == &"MAX_INPUT_SIZE"),
        "pub use 再エクスポートされた const は api.rm に出さない。got: {removed:?}"
    );
    assert!(
        !removed.iter().any(|n| n == &"serialize_output"),
        "pub use 再エクスポートされた pub fn は api.rm に出さない。got: {removed:?}"
    );
}

/// `pub use sub::name as alias;` 形式の alias 付き再エクスポート。alias 後の名前が
/// 公開 API として維持されているため、元の名前 → alias 名への変更は api.rm に出さない。
#[test]
fn detect_api_changes_rust_pub_use_alias_excludes_api_rm() {
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
            ("src/lib.rs", "pub fn renamed_target() {}\n"),
        ],
        "base",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "mod sub;\npub use sub::actual_name as renamed_target;\n",
    )
    .expect("write lib.rs");
    fs::write(repo.join("src/sub.rs"), "pub fn actual_name() {}\n").expect("write sub.rs");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib.rs".to_string(),
            new_path: "src/lib.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/sub.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];

    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api.removed.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !removed.iter().any(|n| n == &"renamed_target"),
        "alias 後の公開名 (renamed_target) は api.rm に出さない。got: {removed:?}"
    );
}

/// private module でも root から `pub use` re-export されていれば外部到達可能なので api.add に出る。
#[test]
fn detect_api_changes_private_module_with_pub_use_reexport_included() {
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
                "mod meeting;\npub use meeting::detector::create_detector;\n",
            ),
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
        "pub use re-export された pub fn は api.add に出る。got: {added:?}"
    );
}

/// private module でも root から `pub use` で re-export していた pub fn は外部公開 API
/// 面に含まれるため、削除は api.rm に残す (find_pub_use_reexport で private 判定が解除される)。
#[test]
fn detect_api_changes_private_module_reexported_pub_fn_removal_stays_in_removed() {
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
            ("src/lib.rs", "mod wifi;\npub use wifi::found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    // found を削除 (private mod だが pub use で re-export されている)
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "pub use re-export された private module の pub fn 削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// private module 配下の pub fn が別の public module (`pub mod prelude`) 経由で `pub use`
/// re-export 公開されている場合、その削除は外部公開 API の破壊なので api.rm に残す
/// (codex コミット後レビューで発見した prelude 経由 false negative の回帰防止)。
#[test]
fn detect_api_changes_private_module_reexported_via_public_prelude_removal_stays_in_removed() {
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
            ("src/prelude.rs", "pub use crate::wifi::found;\n"),
            (
                "src/wifi/mod.rs",
                "pub fn found() -> u32 {\n    0\n}\npub fn hidden() -> u32 {\n    1\n}\n",
            ),
        ],
        "base",
    );
    // found (prelude 経由公開) を削除、hidden は残す
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn hidden() -> u32 {\n    1\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
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
        removed.iter().any(|n| n.ends_with("found")),
        "public prelude 経由 re-export された private module の pub fn 削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// public module から private module への wildcard re-export (`pub use crate::wifi::*`) は
/// その module の全 pub を公開するため、配下 pub fn の削除は api.rm に残す。
#[test]
fn detect_api_changes_private_module_wildcard_reexport_via_public_prelude_removal_stays() {
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
            ("src/prelude.rs", "pub use crate::wifi::*;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "wildcard re-export された private module の pub fn 削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// public prelude が同一 private module 内の別シンボル (found) だけを named re-export している
/// 場合、re-export されていない兄弟 (hidden) の削除は外部非公開なので api.rm に出さない
/// (named re-export が同一 module の全シンボルを公開扱いにする粗さを防ぐ = false positive 抑止)。
#[test]
fn detect_api_changes_private_module_unreexported_sibling_removal_excluded() {
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
            ("src/prelude.rs", "pub use crate::wifi::found;\n"),
            (
                "src/wifi/mod.rs",
                "pub fn found() -> u32 {\n    0\n}\npub fn hidden() -> u32 {\n    1\n}\n",
            ),
        ],
        "base",
    );
    // hidden (未 re-export) を削除、found は残す
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn found() -> u32 {\n    0\n}\n",
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
        !removed.iter().any(|n| n.ends_with("hidden")),
        "re-export されていない private module 内シンボルの削除は api.rm に出さない。got: {removed:?}"
    );
}

/// re-export 元の module 自体が private (`mod prelude`) なら、その `pub use` は外部に届かない。
/// private prelude 経由でしか参照されない private module シンボルの削除は api.rm に出さない。
#[test]
fn detect_api_changes_private_module_via_private_prelude_removal_excluded() {
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
            ("src/lib.rs", "mod wifi;\nmod prelude;\n"),
            ("src/prelude.rs", "pub use crate::wifi::found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        !removed.iter().any(|n| n.ends_with("found")),
        "private prelude (mod prelude) 経由の re-export は外部非公開なので api.rm に出さない。got: {removed:?}"
    );
}

/// `pub use crate::{wifi::found};` のような top-level grouped use 経由の re-export を
/// `parse_pub_use_targets` が取りこぼし false negative になる回帰テスト
/// (codex pre-merge レビュー 2 回目の Warning 指摘)。
#[test]
fn detect_api_changes_private_module_top_level_grouped_reexport_stays_in_removed() {
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
            ("src/prelude.rs", "pub use crate::{wifi::found};\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "top-level grouped use 経由 re-export の private module pub fn 削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// `pub use crate::{wifi::found, wifi::hidden};` のような複数要素 grouped use 経由でも
/// 各 named ターゲットを正しく抽出して false negative を起こさない回帰テスト。
#[test]
fn detect_api_changes_private_module_multi_grouped_reexport_stays_in_removed() {
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
            (
                "src/prelude.rs",
                "pub use crate::{wifi::found, wifi::hidden};\n",
            ),
            (
                "src/wifi/mod.rs",
                "pub fn found() -> u32 {\n    0\n}\npub fn hidden() -> u32 {\n    1\n}\n",
            ),
        ],
        "base",
    );
    // found だけ削除、hidden は残す
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn hidden() -> u32 {\n    1\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
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
        removed.iter().any(|n| n.ends_with("found")),
        "複数要素 top-level grouped use 経由 re-export の削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// nested grouped use (`pub use crate::{wifi::{found, hidden}};`) も正しく展開して
/// 各要素を public ターゲット扱いにする回帰テスト。
#[test]
fn detect_api_changes_private_module_nested_grouped_reexport_stays_in_removed() {
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
            (
                "src/prelude.rs",
                "pub use crate::{wifi::{found, hidden}};\n",
            ),
            (
                "src/wifi/mod.rs",
                "pub fn found() -> u32 {\n    0\n}\npub fn hidden() -> u32 {\n    1\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn hidden() -> u32 {\n    1\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
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
        removed.iter().any(|n| n.ends_with("found")),
        "nested grouped use 経由 re-export の削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// `pub use super::wifi::found;` を prelude.rs に書いたケース。super:: は current module
/// (prelude) から 1 つ pop して crate root 起点になり wifi::found に解決される。
/// codex pre-merge レビュー 3 回目の Warning 指摘 #1 の回帰テスト。
#[test]
fn detect_api_changes_private_module_super_reexport_stays_in_removed() {
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
            ("src/prelude.rs", "pub use super::wifi::found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "super:: re-export は current module を pop して解決され、削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// 多段 `super::super::` の re-export が解決されること。
///
/// tree-sitter-rust は `super::super::x` の 2 つ目の `super` を `scoped_identifier` の
/// **name 位置**へ置くため、path 位置だけを anchor として扱う旧実装では `"super"` が
/// 文字列としてモジュールパスに積まれ、`app::a::super::wifi::found` のような実在しない
/// パスへ解決していた。結果、多段 super 経由で再エクスポートされた公開 API の削除が
/// api.rm から漏れていた。既存テストは 1 段の `super` しかカバーしていなかった。
///
/// 対照として、クレートルートを越える `super` は解決不能 (= 公開 API 面にしない) を
/// 同じテストで固定する。
#[test]
fn detect_api_changes_multi_level_super_reexport_resolves() {
    // src/a/b/prelude.rs の `super::super::super::wifi::found` は
    // (a::b::prelude) → pop → (a::b) → pop → (a) → pop → (root) 起点で
    // wifi::found に解決される。
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
            ("src/lib.rs", "mod wifi;\npub mod a;\n"),
            ("src/a/mod.rs", "pub mod b;\n"),
            ("src/a/b/mod.rs", "pub mod prelude;\n"),
            (
                "src/a/b/prelude.rs",
                "pub use super::super::super::wifi::found;\n",
            ),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "多段 super:: re-export も解決され、削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// glob 再エクスポート (`pub use internal::*;`) 経由の公開 API 変更が検出されること。
///
/// 旧実装は `compute_reexport_reachable_modules` が `RustPubUseEdge::Named` 以外を
/// `else { continue; }` で捨てていたため Wildcard 辺が module 到達性の計算から全部落ちていた。
///
/// **target module の直下**にある item は `propagate_live_exports` の exact-match wildcard 伝播
/// (`wildcard_by_target_module.get(&key.module)`) が拾うため従来も検出できていた。落ちていたのは
/// **pub 子 module 配下**の item で、seed の module (`internal::api`) が wildcard の target
/// (`internal`) と一致しないため伝播が止まり、`internal::api` も reachable_modules に無いので
/// 公開 API 面から外れていた。`S::api::found` で外から到達できるのに無音になる。
///
/// 到達性は「target module + その **pub 子孫**」に閉じる必要がある。prefix 一致で伝播すると
/// private 子 module 配下まで公開扱いになるので、3 ケース目に対照を持たせる。
#[test]
fn detect_api_changes_wildcard_reexport_exposes_module_and_pub_descendants() {
    for (case, extra_files, target, want_reported) in [
        // 直下 module の pub fn
        (
            "glob 直下",
            vec![("src/internal/mod.rs", "pub fn found() -> u32 {\n    0\n}\n")],
            "src/internal/mod.rs",
            true,
        ),
        // pub 子 module 配下の pub fn (`S::api::found` で外から到達できる)
        (
            "glob + pub 子 module",
            vec![
                ("src/internal/mod.rs", "pub mod api;\n"),
                ("src/internal/api.rs", "pub fn found() -> u32 {\n    0\n}\n"),
            ],
            "src/internal/api.rs",
            true,
        ),
        // 対照: private 子 module 配下は外から到達できないので公開 API ではない
        (
            "glob + private 子 module",
            vec![
                ("src/internal/mod.rs", "mod api;\n"),
                ("src/internal/api.rs", "pub fn found() -> u32 {\n    0\n}\n"),
            ],
            "src/internal/api.rs",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let mut files: Vec<(&str, &str)> = vec![
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", "mod internal;\npub use internal::*;\n"),
        ];
        files.extend(extra_files.iter().copied());
        git_commit_files(repo, &files, "base");

        fs::write(repo.join(target), "\n").expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: target.to_string(),
            new_path: target.to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        // 残存参照が無い fixture なので removed_dead 側に入る。ここで見たいのは
        // 「公開 API 面として認識されたか」なので両バケットの和で判定する。
        let reported = api
            .removed
            .iter()
            .chain(api.removed_dead.iter())
            .any(|s| s.name.ends_with("found"));
        assert_eq!(
            reported,
            want_reported,
            "{case}: api.rm への計上が期待と異なる (removed={:?} removed_dead={:?})",
            api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
            api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }
}

/// glob import は source namespace の明示 item / named import に shadow される。
///
/// Rust では同一 namespace の明示 item・named import が glob import を shadow するため
/// (Rust Reference: use declarations)、root に `pub mod api;` があれば外部の `crate::api` は
/// root の `api` であって `internal::api` ではない。glob の target 配下を無条件に
/// 到達可能化すると、公開されていない `internal::api::found` の削除を api.rm へ誤計上する。
///
/// 対照として「shadow が無ければ従来どおり到達可能」も同じテストで固定する。
#[test]
fn detect_api_changes_wildcard_reexport_respects_glob_shadowing() {
    for (case, lib_rs, extra, want_reported) in [
        (
            // root の `pub mod api` が glob の `api` を shadow する
            "明示 module による shadow",
            "mod internal;\npub mod api;\npub use internal::*;\n",
            vec![("src/api.rs", "pub fn other() -> u32 {\n    1\n}\n")],
            false,
        ),
        (
            // private な `mod api` でも同じ namespace を占めるので shadow する
            "private module による shadow",
            "mod internal;\nmod api;\npub use internal::*;\n",
            vec![("src/api.rs", "pub fn other() -> u32 {\n    1\n}\n")],
            false,
        ),
        (
            // **module の** named re-export は型名前空間を占めるので shadow する
            "module の named re-export による shadow",
            "mod internal;\nmod other;\npub use other::sub as api;\npub use internal::*;\n",
            vec![
                ("src/other/mod.rs", "pub mod sub;\n"),
                ("src/other/sub.rs", "pub fn other() -> u32 {\n    1\n}\n"),
            ],
            false,
        ),
        (
            // 対照: **関数の** named re-export は値名前空間なので module と共存する。
            // `api::found()` は glob 由来の module を参照できるので、shadow 扱いにすると
            // 実際に公開されている API の削除を見逃す (false negative)。
            "関数の named re-export は shadow しない",
            "mod internal;\nmod other;\npub use other::thing as api;\npub use internal::*;\n",
            vec![("src/other.rs", "pub fn thing() -> u32 {\n    1\n}\n")],
            true,
        ),
        (
            // 対照: shadow が無ければ従来どおり到達可能
            "shadow なし",
            "mod internal;\npub use internal::*;\n",
            vec![],
            true,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let mut files: Vec<(&str, &str)> = vec![
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src/lib.rs", lib_rs),
            ("src/internal/mod.rs", "pub mod api;\n"),
            ("src/internal/api.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ];
        files.extend(extra.iter().copied());
        git_commit_files(repo, &files, "base");

        fs::write(repo.join("src/internal/api.rs"), "\n").expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/internal/api.rs".to_string(),
            new_path: "src/internal/api.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        let reported = api
            .removed
            .iter()
            .chain(api.removed_dead.iter())
            .any(|s| s.name.ends_with("found"));
        assert_eq!(
            reported,
            want_reported,
            "{case}: api.rm への計上が期待と異なる (removed={:?} removed_dead={:?})",
            api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
            api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }
}

/// grouped trailing `self` (`pub use outer::m::{self};`) が直接形と同じ edge を作ること。
///
/// list 経路では単独の `self` が path_prefix = ["outer","m"] を積んだ状態で届くため、
/// 畳まないと leaf_name が None になり Named edge が生成されない。その結果、private な
/// `outer` 配下の公開 module `m` をこの形で再エクスポートしても、`m` 配下の公開 API 削除が
/// api.rm から漏れる。alias 付き (`{self as alias}`) も同経路。
#[test]
fn detect_api_changes_grouped_trailing_self_reexport_resolves() {
    for (case, reexport) in [
        ("grouped", "pub use outer::m::{self};\n"),
        ("grouped alias", "pub use outer::m::{self as api};\n"),
        ("direct", "pub use outer::m::self as api2;\n"),
    ] {
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
                ("src/lib.rs", &format!("mod outer;\n{reexport}")),
                ("src/outer/mod.rs", "pub mod m;\n"),
                ("src/outer/m.rs", "pub fn found() -> u32 {\n    0\n}\n"),
            ],
            "base",
        );
        fs::write(repo.join("src/outer/m.rs"), "\n").expect("write");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/outer/m.rs".to_string(),
            new_path: "src/outer/m.rs".to_string(),
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
            removed.iter().any(|n| n.ends_with("found")),
            "{case}: module 自身の再エクスポート経由でも削除は api.rm に残るべき。got: {removed:?}"
        );
    }
}

/// `resolve_use_path_node` の anchor 解決を直接固定する。
///
/// 段数分だけ pop すること、ルートを越えたら None になること (fail-closed)、
/// `crate` / `self` が従来どおり動くこと、`super` が先頭以外に現れたら None になることを
/// 同じテストで対にして押さえる。
#[test]
fn resolve_use_path_node_handles_multi_level_anchors() {
    use crate::commands::api_changes::resolve_use_path_node;

    // use_declaration > (visibility_modifier) argument
    fn argument<'t>(tree: &'t tree_sitter::Tree) -> tree_sitter::Node<'t> {
        let root = tree.root_node();
        let decl = root.named_child(0).expect("use_declaration");
        decl.child_by_field_name("argument").expect("argument")
    }

    let current_module = vec!["a".to_string(), "b".to_string()];

    for (src, want) in [
        // 1 段: a::b → a
        (
            "use super::x;",
            Some((vec!["a".to_string()], Some("x".to_string()))),
        ),
        // 2 段: a::b → root
        (
            "use super::super::x;",
            Some((Vec::<String>::new(), Some("x".to_string()))),
        ),
        // 3 段: ルートを越えるので解決不能
        ("use super::super::super::x;", None),
        // crate anchor はルート起点
        (
            "use crate::m::x;",
            Some((vec!["m".to_string()], Some("x".to_string()))),
        ),
        // self anchor は現 module 起点
        (
            "use self::m::x;",
            Some((
                vec!["a".to_string(), "b".to_string(), "m".to_string()],
                Some("x".to_string()),
            )),
        ),
        // anchor 無しは path_prefix (ここでは空) 起点
        (
            "use m::x;",
            Some((vec!["m".to_string()], Some("x".to_string()))),
        ),
        // 対照: super が先頭以外に現れる不正な path は解決しない
        ("use m::super::x;", None),
        // `self` anchor に続く `super` は有効 (先頭からの連続部分とみなす)
        (
            "use self::super::x;",
            Some((vec!["a".to_string()], Some("x".to_string()))),
        ),
        // 末尾の `self` は `use a::b::{self};` と同値で「module 自身」の再エクスポート。
        // tree-sitter は末尾 `self` を kind `identifier` として返すため、畳まないと
        // 「`self` という名前の item」という実在しない edge になる。
        (
            "use outer::m::self;",
            Some((vec!["outer".to_string()], Some("m".to_string()))),
        ),
        (
            "use crate::m::self;",
            Some((Vec::<String>::new(), Some("m".to_string()))),
        ),
        // 対照: 末尾以外の `self` は Rust として不正なので解決しない
        ("use m::self::x;", None),
    ] {
        let tree =
            crate::engine::parser::parse_source(src.as_bytes(), crate::language::LangId::Rust)
                .expect("parse");
        let node = argument(&tree);
        let got = resolve_use_path_node(node, src.as_bytes(), &[], &current_module);
        assert_eq!(got, want, "{src}");
    }

    // path_prefix が非空のとき (scoped_use_list / use_wildcard 経路から呼ばれる形) の
    // anchor 挙動を固定する。`self` は prefix が非空なら current_module を積まず prefix を
    // そのまま使い、`super` は prefix を起点に pop、`crate` は prefix を捨ててルート起点。
    let prefix = vec!["p".to_string(), "q".to_string()];
    for (src, want) in [
        (
            "use self::x;",
            Some((
                vec!["p".to_string(), "q".to_string()],
                Some("x".to_string()),
            )),
        ),
        (
            "use super::x;",
            Some((vec!["p".to_string()], Some("x".to_string()))),
        ),
        (
            "use crate::x;",
            Some((Vec::<String>::new(), Some("x".to_string()))),
        ),
        (
            "use m::x;",
            Some((
                vec!["p".to_string(), "q".to_string(), "m".to_string()],
                Some("x".to_string()),
            )),
        ),
    ] {
        let tree =
            crate::engine::parser::parse_source(src.as_bytes(), crate::language::LangId::Rust)
                .expect("parse");
        let node = argument(&tree);
        let got = resolve_use_path_node(node, src.as_bytes(), &prefix, &current_module);
        assert_eq!(got, want, "非空 prefix: {src}");
    }

    // grouped 形式 (`use p::q::{self};`) の list 要素として届く**単独 `self`**。
    // `expand_use_list_edges` が path_prefix = ["p","q"] を積んだ状態で渡すため、
    // 末尾を item へ畳んで直接形 `use p::q::self;` と同じ結果にする。
    let src = "use self;";
    let tree = crate::engine::parser::parse_source(src.as_bytes(), crate::language::LangId::Rust)
        .expect("parse");
    let node = argument(&tree);
    assert_eq!(
        resolve_use_path_node(node, src.as_bytes(), &prefix, &current_module),
        Some((vec!["p".to_string()], Some("q".to_string()))),
        "非空 prefix + 単独 self は prefix の末尾を item へ畳む"
    );
    // 対照: path_prefix が空なら module anchor としての `self` なので畳まない
    // (`use self::{a, b};` の path 位置がこの形で来る)。
    assert_eq!(
        resolve_use_path_node(node, src.as_bytes(), &[], &current_module),
        Some((current_module.clone(), None)),
        "空 prefix + 単独 self は module anchor のまま"
    );
}

/// `pub use crate::{wifi::found /* } */};` のような grouped use 内ブロックコメントの `}` で
/// bracket-balance が誤って崩れない (codex pre-merge レビュー 3 回目 Warning 指摘 #2 の回帰)。
#[test]
fn detect_api_changes_private_module_grouped_reexport_with_block_comment_stays_in_removed() {
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
            ("src/prelude.rs", "pub use crate::{wifi::found /* } */};\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "grouped use 内のブロックコメントの `{{`}} で bracket-balance を崩さず正しく解析され、削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// `// 行コメント\npub /* */ use crate::wifi::found;` のように `pub` と `use` の間に
/// ブロックコメントがあっても AST argument 経由抽出で取りこぼさない (codex 指摘 #3 の回帰)。
#[test]
fn detect_api_changes_private_module_reexport_with_pub_use_comment_stays_in_removed() {
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
            (
                "src/prelude.rs",
                "// 行コメント\npub /* mid */ use crate::wifi::found;\n",
            ),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "// コメント先行と pub /* */ use のコメント混在でも AST argument 経由で解析され、削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// `pub(crate) use crate::wifi::found;` は制限付き visibility で外部公開ではないため、
/// found 削除は api.rm に出さない (visibility_modifier 厳密照合の回帰)。
#[test]
fn detect_api_changes_private_module_pub_crate_reexport_does_not_keep_in_removed() {
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
            ("src/prelude.rs", "pub(crate) use crate::wifi::found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        !removed.iter().any(|n| n.ends_with("found")),
        "pub(crate) use は外部非公開なので削除は api.rm に残さない。got: {removed:?}"
    );
}

/// inline `pub mod prelude { pub use super::wifi::found; }` (file 内に pub mod inline 定義 +
/// 配下に pub use) でも found 削除を api.rm に残す。super:: は prelude から1pop して crate root。
#[test]
fn detect_api_changes_private_module_inline_pub_mod_reexport_stays_in_removed() {
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
                "mod wifi;\npub mod prelude { pub use super::wifi::found; }\n",
            ),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "inline pub mod 配下の super:: re-export 経由削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// inline `mod prelude { pub use super::wifi::found; }` (非 pub inline mod) 配下の pub use は
/// 外部に届かないので削除は api.rm に残さない (inline_private_depth の回帰テスト)。
#[test]
fn detect_api_changes_private_module_inline_private_mod_reexport_does_not_keep() {
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
                "mod wifi;\nmod prelude { pub use super::wifi::found; }\n",
            ),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        !removed.iter().any(|n| n.ends_with("found")),
        "非 pub inline mod 配下の pub use は外部到達不能なので削除は api.rm に残さない。got: {removed:?}"
    );
}

/// `pub use crate :: wifi :: found;` のように `::` の周囲に whitespace を入れても
/// AST argument walker (tree-sitter-rust の scoped_identifier 構造) で正しく解析され、
/// found 削除は api.rm に残る (codex pre-merge レビュー 4 回目 Warning #1 回帰テスト)。
#[test]
fn detect_api_changes_private_module_reexport_with_whitespace_path_stays_in_removed() {
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
            ("src/prelude.rs", "pub use crate :: wifi :: found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "whitespace 入りの :: re-export は AST walker で正規化解決され、削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// `pub use crate::wifi::found\tas\talias;` のように alias 区切りがタブでも AST walker で
/// 解析され、found 削除は api.rm に残る (codex pre-merge レビュー 4 回目 Warning #1b 回帰)。
#[test]
fn detect_api_changes_private_module_reexport_with_tab_alias_stays_in_removed() {
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
            ("src/prelude.rs", "pub use crate::wifi::found\tas\talias;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "タブ区切り as alias の re-export も AST walker で解析され、削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// 二段 re-export (private prelude を経由) で root 側 `pub use prelude::found;` が公開
/// しているケースで、wifi/found 削除は api.rm に残る。codex pre-merge レビュー 4 回目
/// Warning #2 の回帰テスト。Step B の re-export edge graph + 固定点伝播で解決される。
#[test]
fn detect_api_changes_private_module_via_private_prelude_then_root_pub_use_stays_in_removed() {
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
                "mod wifi;\nmod prelude;\npub use prelude::found;\n",
            ),
            ("src/prelude.rs", "pub use crate::wifi::found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "private prelude 経由の二段 re-export で root が公開しているなら削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// 二段 re-export で各 hop が alias 付き: prelude 内で `pub use crate::wifi::found as
/// public_found;`、root で `pub use prelude::public_found;`。wifi/found 削除でも root の
/// 公開名 `public_found` まで alias graph が伝播するため api.rm に残る。
#[test]
fn detect_api_changes_private_module_via_alias_chain_through_prelude_stays_in_removed() {
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
                "mod wifi;\nmod prelude;\npub use prelude::public_found;\n",
            ),
            (
                "src/prelude.rs",
                "pub use crate::wifi::found as public_found;\n",
            ),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "alias chain の二段 re-export 経由削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// 二段 re-export で root 側が wildcard `pub use prelude::*;`、prelude で `pub use
/// crate::wifi::found;` するパターン。wildcard が target_module=prelude にかかり、live
/// (prelude, found) が (root, found) に伝播 → public_modules に到達。
#[test]
fn detect_api_changes_private_module_via_wildcard_chain_through_prelude_stays_in_removed() {
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
                "mod wifi;\nmod prelude;\npub use prelude::*;\n",
            ),
            ("src/prelude.rs", "pub use crate::wifi::found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "wildcard chain の二段 re-export 経由削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// 二段の経路で wildcard を中間に挟む: prelude で `pub use crate::wifi::*;`、root で
/// `pub use prelude::found;`。Wildcard target=[wifi] によって live (wifi, found) が
/// (prelude, found) に伝播し、root の named edge で (root, found) に至る。
#[test]
fn detect_api_changes_private_module_named_then_wildcard_chain_stays_in_removed() {
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
                "mod wifi;\nmod prelude;\npub use prelude::found;\n",
            ),
            ("src/prelude.rs", "pub use crate::wifi::*;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "wildcard→named の二段 chain 経由削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// 循環 re-export (`pub use prelude::found;` 同士で循環) で固定点伝播が無限ループしない
/// (HashSet で重複を防止しているため自然停止)。BFS 単体テスト相当を統合テストで確認。
#[test]
fn detect_api_changes_private_module_cyclic_reexports_terminate() {
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
                "mod wifi;\npub mod a;\npub mod b;\npub use a::found;\n",
            ),
            ("src/a.rs", "pub use crate::b::found;\n"),
            ("src/b.rs", "pub use crate::wifi::found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "循環的な 3 段 re-export chain でも固定点で停止して exposed 判定が成立する。got: {removed:?}"
    );
}

/// `#[path = "..."]` で module 宣言とファイル名がずれるケースで、re-export 経由公開の
/// 削除が誤抑制されないこと (fail-closed: index 全体を `None` にして api.rm に残す)。
/// codex pre-merge レビュー 5 回目の Warning #3 (path attribute) 回帰テスト。
#[test]
fn detect_api_changes_private_module_path_attribute_keeps_removal_in_api_rm() {
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
                "mod wifi;\n#[path = \"hidden.rs\"]\nmod prelude;\npub use prelude::found;\n",
            ),
            ("src/hidden.rs", "pub use crate::wifi::found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi/mod.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "#[path] 付き module 経由の re-export 削除は判定不能 → fail-closed で api.rm に残すべき。got: {removed:?}"
    );
}

/// `#[path]` が削除対象 module 自身 (wifi) に付いていても fail-closed で削除は api.rm に残る。
/// (private module info 構築失敗 → 上流で抑制せず通常経路に戻る = api.rm 残し)
#[test]
fn detect_api_changes_private_module_path_attribute_on_target_module_keeps_removal() {
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
            ("src/lib.rs", "#[path = \"wifi_impl.rs\"]\nmod wifi;\n"),
            ("src/wifi_impl.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(repo.join("src/wifi_impl.rs"), "\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi_impl.rs".to_string(),
        new_path: "src/wifi_impl.rs".to_string(),
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
        removed.iter().any(|n| n.ends_with("found")),
        "#[path] が削除対象 module 自身に付いていても fail-closed で削除は api.rm に残るべき。got: {removed:?}"
    );
}

/// private module 配下の `pub fn` が public prelude 経由で re-export 公開されている場合の
/// signature 変更は外部互換性破壊なので api.mod に残す (codex pre-merge レビュー 6 回目
/// Warning #4 = api.mod 抑制が edge graph を見ない false negative の回帰テスト)。
#[test]
fn detect_api_changes_private_module_reexported_via_public_prelude_signature_change_stays_in_mod() {
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
            ("src/prelude.rs", "pub use crate::wifi::found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn found(x: u32) -> u32 {\n    x\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified.iter().any(|m| m.name.ends_with("found")),
        "public prelude 経由 re-export された private module の pub fn signature 変更は api.mod に残るべき。mod={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}

/// private module 配下の `pub fn` が二段 re-export (`mod prelude;` + `pub use prelude::found;`)
/// 経由で公開されているケースの signature 変更は外部互換性破壊なので api.mod に残す。
#[test]
fn detect_api_changes_private_module_via_private_prelude_then_root_pub_use_signature_change_stays_in_mod()
 {
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
                "mod wifi;\nmod prelude;\npub use prelude::found;\n",
            ),
            ("src/prelude.rs", "pub use crate::wifi::found;\n"),
            ("src/wifi/mod.rs", "pub fn found() -> u32 {\n    0\n}\n"),
        ],
        "base",
    );
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn found(x: u32) -> u32 {\n    x\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified.iter().any(|m| m.name.ends_with("found")),
        "二段 re-export 経由公開シンボルの signature 変更は api.mod に残るべき。mod={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}

/// private module 配下の `pub fn` を新規追加し、別の public-reachable module から `pub use`
/// で re-export 公開されているケースで、追加された API は外部公開 API 面なので `api.add` に出る
/// (Issue 2026-06-05-rust-api-add-private-module-reexport-edge-graph 対応)。
#[test]
fn detect_api_changes_private_module_new_fn_reexported_via_public_prelude_is_added() {
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
            ("src/prelude.rs", "pub use crate::wifi::found;\n"),
            ("src/wifi/mod.rs", "\n"),
        ],
        "base",
    );
    // 新規 pub fn を追加 (prelude::found として既存 re-export 経路で公開される)
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn found() -> u32 {\n    0\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        added.iter().any(|n| n.ends_with("found")),
        "public prelude 経由 re-export 公開される private module の新規 pub fn は api.add に出るべき。got: {added:?}"
    );
}

/// 新規追加された pub fn が同一 diff 内の `pub use crate::wifi::found;` でも参照されているとき、
/// その `pub use` は internal-use ではなく外部公開エクスポートなので api.add から除外しない
/// (`is_used_in_diff_paths` の use_declaration 強化が効くこと)。
#[test]
fn detect_api_changes_private_module_new_fn_with_only_pub_use_ref_is_added() {
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
            ("src/prelude.rs", "\n"),
            ("src/wifi/mod.rs", "\n"),
        ],
        "base",
    );
    // 同一 diff で wifi/mod.rs に pub fn を追加 + prelude.rs に pub use を追加
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn found() -> u32 {\n    0\n}\n",
    )
    .expect("write");
    fs::write(repo.join("src/prelude.rs"), "pub use crate::wifi::found;\n").expect("write prelude");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/wifi/mod.rs".to_string(),
            new_path: "src/wifi/mod.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/prelude.rs".to_string(),
            new_path: "src/prelude.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        added.iter().any(|n| n.ends_with("found")),
        "pub use re-export しか参照がない新規 pub fn は internal-use 扱いせず api.add に出すべき。got: {added:?}"
    );
}

/// private module 配下の `pub fn` を新規追加し、re-export 公開されていない場合は
/// crate 外非到達なので api.add に出さない (false positive 復活を防ぐ回帰テスト)。
#[test]
fn detect_api_changes_private_module_new_fn_without_reexport_excluded_from_added() {
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
            ("src/wifi/mod.rs", "\n"),
        ],
        "base",
    );
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn found() -> u32 {\n    0\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !added.iter().any(|n| n.ends_with("found")),
        "re-export なしの private module 新規 pub fn は外部到達不能なので api.add に出さない。got: {added:?}"
    );
}

/// `pub mod prelude;` + 二段 re-export (`pub use prelude::found;` + prelude.rs に
/// `pub use crate::wifi::found;`) でも新規追加の wifi/found が api.add に残る。
#[test]
fn detect_api_changes_private_module_new_fn_via_two_hop_reexport_is_added() {
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
                "mod wifi;\nmod prelude;\npub use prelude::found;\n",
            ),
            ("src/prelude.rs", "pub use crate::wifi::found;\n"),
            ("src/wifi/mod.rs", "\n"),
        ],
        "base",
    );
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub fn found() -> u32 {\n    0\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/wifi/mod.rs".to_string(),
        new_path: "src/wifi/mod.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        added.iter().any(|n| n.ends_with("found")),
        "二段 re-export 経由公開される新規 pub fn は api.add に残るべき。got: {added:?}"
    );
}

/// ファイル新規作成経路 (`old_path == /dev/null`) で、private module 配下の新規ファイル全体が
/// re-export 公開されていれば、そのファイル内の pub fn は `api.add` に残る。
#[test]
fn detect_api_changes_private_module_new_file_in_reexported_module_is_added() {
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
            ("src/prelude.rs", "\n"),
            ("src/wifi/mod.rs", "pub mod detector;\n"),
            ("src/wifi/detector.rs", "\n"),
        ],
        "base",
    );
    // 新規 wifi/scanner.rs と prelude::scanner の re-export
    fs::write(
        repo.join("src/wifi/mod.rs"),
        "pub mod detector;\npub mod scanner;\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/wifi/scanner.rs"),
        "pub fn scan() -> u32 {\n    0\n}\n",
    )
    .expect("write scanner");
    fs::write(
        repo.join("src/prelude.rs"),
        "pub use crate::wifi::scanner::scan;\n",
    )
    .expect("write prelude");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/wifi/scanner.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/wifi/mod.rs".to_string(),
            new_path: "src/wifi/mod.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/prelude.rs".to_string(),
            new_path: "src/prelude.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        added.iter().any(|n| n.ends_with("scan")),
        "新規ファイル経路 (/dev/null → new) で re-export 公開された pub fn は api.add に残るべき。got: {added:?}"
    );
}
