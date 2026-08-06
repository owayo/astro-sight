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
