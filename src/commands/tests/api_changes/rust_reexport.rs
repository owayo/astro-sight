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

/// 非 ASCII 名のディレクトリ配下にある crate でも、base 側の `pub use` 再エクスポートを解決する。
///
/// base 側のファイル一覧 (`collect_rust_rs_files` の `git ls-tree`) が改行区切りだったため、
/// 非 ASCII のパスが `"\343\203\204..."` とクォートされ、末尾の `"` で `.rs` 判定から全件落ちていた。
/// `pub use` の辺を 1 本も集められず、再エクスポートで公開していた `helper` の削除が
/// api.rm / rm_dead のどちらにも出なかった。同じ構成の ASCII 名 crate を対照として並べる。
#[test]
fn detect_api_changes_reexport_in_crate_under_non_ascii_directory_resolves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let manifest = "[package]\nname = \"tool\"\n\n[lib]\npath = \"src/lib.rs\"\n";
    let lib = "mod internal;\npub use internal::helper;\n";
    let internal = "pub fn helper() -> u32 {\n    1\n}\n\npub fn other() -> u32 {\n    2\n}\n";
    let crates = ["ascii_tool", "ツール"];
    let mut files = Vec::new();
    for root in crates {
        files.push((format!("{root}/Cargo.toml"), manifest));
        files.push((format!("{root}/src/lib.rs"), lib));
        files.push((format!("{root}/src/internal.rs"), internal));
    }
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (p.as_str(), *c)).collect();
    git_commit_files(repo, &files, "base");

    let mut diff_files = Vec::new();
    for root in crates {
        fs::write(repo.join(format!("{root}/src/lib.rs")), "mod internal;\n").expect("write");
        fs::write(
            repo.join(format!("{root}/src/internal.rs")),
            "pub fn other() -> u32 {\n    2\n}\n",
        )
        .expect("write");
        for (file, old_count, new_count) in [("lib.rs", 2, 1), ("internal.rs", 7, 3)] {
            let path = format!("{root}/src/{file}");
            diff_files.push(crate::models::impact::DiffFile {
                old_path: path.clone(),
                new_path: path,
                hunks: vec![crate::models::impact::HunkInfo {
                    old_start: 1,
                    old_count,
                    new_start: 1,
                    new_count,
                }],
                deleted_old_source: None,
            });
        }
    }
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<(&str, &str)> = api
        .removed
        .iter()
        .chain(api.removed_dead.iter())
        .map(|s| (s.name.as_str(), s.file.as_str()))
        .collect();
    for root in crates {
        let file = format!("{root}/src/internal.rs");
        assert!(
            removed.contains(&("helper", file.as_str())),
            "{root}: 再エクスポートしていた helper の削除を報告すべき。got: {removed:?}"
        );
    }
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
/// 同じテストで対にして押さえる。返す起点は書かれた path の先頭だけで決まり
/// (`crate` / `self` / `super` は Anchored、識別子は Bare、`::` は Global)、
/// Bare / Global の module パスは従来どおり path_prefix 起点のまま。
#[test]
fn resolve_use_path_node_handles_multi_level_anchors() {
    use crate::commands::api_changes::resolve_use_path_node;
    use crate::engine::rust_use_tree::UsePathOrigin::{Anchored, Bare, Global};

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
            Some((vec!["a".to_string()], Some("x".to_string()), Anchored)),
        ),
        // 2 段: a::b → root
        (
            "use super::super::x;",
            Some((Vec::<String>::new(), Some("x".to_string()), Anchored)),
        ),
        // 3 段: ルートを越えるので解決不能
        ("use super::super::super::x;", None),
        // crate anchor はルート起点
        (
            "use crate::m::x;",
            Some((vec!["m".to_string()], Some("x".to_string()), Anchored)),
        ),
        // self anchor は現 module 起点
        (
            "use self::m::x;",
            Some((
                vec!["a".to_string(), "b".to_string(), "m".to_string()],
                Some("x".to_string()),
                Anchored,
            )),
        ),
        // anchor 無しは path_prefix (ここでは空) 起点。どの module から読むかは edition と
        // スコープで決まるので、起点を Bare として返し、呼び出し側に任せる。
        (
            "use m::x;",
            Some((vec!["m".to_string()], Some("x".to_string()), Bare)),
        ),
        // 先頭の `::` は Global。module パスは Bare と同じく path_prefix 起点のまま
        (
            "use ::m::x;",
            Some((vec!["m".to_string()], Some("x".to_string()), Global)),
        ),
        // 対照: super が先頭以外に現れる不正な path は解決しない
        ("use m::super::x;", None),
        // `self` anchor に続く `super` は有効 (先頭からの連続部分とみなす)
        (
            "use self::super::x;",
            Some((vec!["a".to_string()], Some("x".to_string()), Anchored)),
        ),
        // 末尾の `self` は `use a::b::{self};` と同値で「module 自身」の再エクスポート。
        // tree-sitter は末尾 `self` を kind `identifier` として返すため、畳まないと
        // 「`self` という名前の item」という実在しない edge になる。
        (
            "use outer::m::self;",
            Some((vec!["outer".to_string()], Some("m".to_string()), Bare)),
        ),
        (
            "use crate::m::self;",
            Some((Vec::<String>::new(), Some("m".to_string()), Anchored)),
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
    // 起点は書かれた path の先頭のもの (group の要素では展開側が group の起点を優先する)。
    let prefix = vec!["p".to_string(), "q".to_string()];
    for (src, want) in [
        (
            "use self::x;",
            Some((
                vec!["p".to_string(), "q".to_string()],
                Some("x".to_string()),
                Anchored,
            )),
        ),
        (
            "use super::x;",
            Some((vec!["p".to_string()], Some("x".to_string()), Anchored)),
        ),
        (
            "use crate::x;",
            Some((Vec::<String>::new(), Some("x".to_string()), Anchored)),
        ),
        (
            "use m::x;",
            Some((
                vec!["p".to_string(), "q".to_string(), "m".to_string()],
                Some("x".to_string()),
                Bare,
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
        Some((vec!["p".to_string()], Some("q".to_string()), Anchored)),
        "非空 prefix + 単独 self は prefix の末尾を item へ畳む"
    );
    // 対照: path_prefix が空なら module anchor としての `self` なので畳まない
    // (`use self::{a, b};` の path 位置がこの形で来る)。
    assert_eq!(
        resolve_use_path_node(node, src.as_bytes(), &[], &current_module),
        Some((current_module.clone(), None, Anchored)),
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

// ---- Issue #207: 子 module の中の相対 `pub use` (先頭が識別子の path) ----

/// `manifest` を `Cargo.toml` に置き、`base` を commit してから作業ツリーへ `worktree` を書く。
fn relative_reexport_repo(
    manifest: &str,
    base: &[(&str, &str)],
    worktree: &[(&str, &str)],
) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut files = vec![("Cargo.toml", manifest)];
    files.extend_from_slice(base);
    git_commit_files(repo, &files, "base");
    for (rel, content) in worktree {
        let full = repo.join(rel);
        fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        fs::write(full, content).expect("write");
    }
    dir
}

/// edition を指定した (`None` は省略 = 2015) package の manifest。
fn package_manifest(edition: Option<&str>) -> String {
    let mut manifest = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n".to_string();
    if let Some(edition) = edition {
        manifest.push_str(&format!("edition = \"{edition}\"\n"));
    }
    manifest
}

const NEW_VALUE: &str = "pub struct NewValue;\n\nimpl NewValue {\n    pub fn amount(&self) -> usize {\n        1\n    }\n}\n";

/// Issue の最小例: base は `pub mod reader;` と空の `reader.rs`、作業ツリーで
/// `reader.rs` に `mod value;` と `reexport` を書き、`reader/value.rs` に `NewValue` を足す。
fn issue_207_repo(manifest: &str, reexport: &str) -> tempfile::TempDir {
    relative_reexport_repo(
        manifest,
        &[
            ("src/lib.rs", "pub mod reader;\n"),
            ("src/reader.rs", "// Public facade.\n"),
        ],
        &[
            ("src/reader.rs", &format!("mod value;\n{reexport}\n")),
            ("src/reader/value.rs", NEW_VALUE),
        ],
    )
}

/// `file` の private module 情報からその crate の index を作り、読み直した後の edge を
/// `source::name -> target::item` で返す。`None` は index が不完全 (公開面による抑制を諦める)。
fn reexport_edges(
    source: RustSourceTree<'_>,
    repo: &std::path::Path,
    file: &str,
) -> Option<Vec<String>> {
    let dir = repo.to_str().expect("utf-8 path");
    let info = rust_private_module_info(source, dir, file).expect("private module の中のファイル");
    let index = collect_rust_pub_use_index(source, dir, &info)?;
    let path = |module: &[String], name: &str| {
        module
            .iter()
            .map(String::as_str)
            .chain([name])
            .collect::<Vec<_>>()
            .join("::")
    };
    Some(
        index
            .edges()
            .iter()
            .map(|edge| match edge {
                RustPubUseEdge::Named {
                    source_module,
                    exported_name,
                    target_module,
                    target_item,
                    ..
                } => format!(
                    "{} -> {}",
                    path(source_module, exported_name),
                    path(target_module, target_item)
                ),
                RustPubUseEdge::Wildcard {
                    source_module,
                    target_module,
                    ..
                } => format!(
                    "{} -> {}",
                    path(source_module, "*"),
                    path(target_module, "*")
                ),
            })
            .collect(),
    )
}

fn added_names(api: &ApiChanges) -> Vec<String> {
    let mut names: Vec<String> = api.added.iter().map(|s| s.name.clone()).collect();
    names.sort();
    names
}

fn modified_names(api: &ApiChanges) -> Vec<String> {
    api.modified.iter().map(|s| s.name.clone()).collect()
}

/// Issue #207: edition 2018 以降の子 module の中の `pub use value::NewValue;` は、
/// `pub use self::value::NewValue;` と同じ公開経路。新しい型とメソッドが api.add に出ること、
/// index が作られて (抑制の断念で偶然通るのではなく) 子 module を指す edge を持つことを
/// 確かめる。対照: 再公開しない private module の中は公開面の外のまま。
#[test]
fn detect_api_changes_relative_pub_use_in_child_module_matches_self_form() {
    for edition in ["2018", "2021", "2024"] {
        for reexport in [
            "pub use value::NewValue;",
            "pub use self::value::NewValue;",
            "",
        ] {
            let dir = issue_207_repo(&package_manifest(Some(edition)), reexport);
            let repo = dir.path();
            let added = added_names(&detect_api_changes_from_worktree(repo));
            let edges = reexport_edges(RustSourceTree::Worktree, repo, "src/reader/value.rs")
                .expect("index は作られる");
            if reexport.is_empty() {
                assert!(added.is_empty(), "{edition}: 再公開しない: {added:?}");
                assert!(edges.is_empty(), "{edition}: {edges:?}");
            } else {
                assert_eq!(
                    added,
                    ["NewValue", "NewValue.amount"],
                    "{edition} {reexport}"
                );
                assert_eq!(
                    edges,
                    ["reader::NewValue -> reader::value::NewValue"],
                    "{edition} {reexport}"
                );
            }
        }
    }
}

/// base 側の判定 (api.mod / api.rm) も同じ読み方をする。base で bare 形の再公開をしていた
/// 型のメソッドの署名変更は modified、削除は removed / removed_dead に残る。
#[test]
fn detect_api_changes_relative_pub_use_keeps_base_side_changes() {
    let old_value = "pub struct NewValue;\n\nimpl NewValue {\n    pub fn amount(&self) -> usize {\n        1\n    }\n\n    pub fn gone(&self) {}\n}\n";
    let new_value = "pub struct NewValue;\n\nimpl NewValue {\n    pub fn amount(&self, factor: usize) -> usize {\n        factor\n    }\n}\n";
    for reexport in ["pub use value::NewValue;", "pub use self::value::NewValue;"] {
        let reader = format!("mod value;\n{reexport}\n");
        let dir = relative_reexport_repo(
            &package_manifest(Some("2021")),
            &[
                ("src/lib.rs", "pub mod reader;\n"),
                ("src/reader.rs", &reader),
                ("src/reader/value.rs", old_value),
            ],
            &[("src/reader/value.rs", new_value)],
        );
        let repo = dir.path();
        let api = detect_api_changes_from_worktree(repo);
        let removed: Vec<&str> = api
            .removed
            .iter()
            .chain(&api.removed_dead)
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            modified_names(&api).contains(&"NewValue.amount".to_string()),
            "{reexport}: {:?}",
            modified_names(&api)
        );
        assert!(
            removed.contains(&"NewValue.gone"),
            "{reexport}: {removed:?}"
        );
        let blobs = GitBlobBatch::new(repo.to_str().expect("utf-8 path"), "HEAD");
        let edges = reexport_edges(
            RustSourceTree::Base { blobs: &blobs },
            repo,
            "src/reader/value.rs",
        )
        .expect("base 側の index も作られる");
        assert_eq!(
            edges,
            ["reader::NewValue -> reader::value::NewValue"],
            "{reexport}"
        );
    }
}

/// group・別名・glob・prefix の無い group (`{value::X}` と `{self::value::X}`) も子 module を指す。
/// `{self::..}` は edition に依らず、group の要素の `self` を現在の module として読む。
#[test]
fn detect_api_changes_relative_pub_use_forms_reach_child_module() {
    for (reexport, edge) in [
        (
            "pub use value::{NewValue};",
            "reader::NewValue -> reader::value::NewValue",
        ),
        (
            "pub use value::NewValue as Renamed;",
            "reader::Renamed -> reader::value::NewValue",
        ),
        ("pub use value::*;", "reader::* -> reader::value::*"),
        (
            "pub use {value::NewValue};",
            "reader::NewValue -> reader::value::NewValue",
        ),
        (
            "pub use {self::value::NewValue};",
            "reader::NewValue -> reader::value::NewValue",
        ),
    ] {
        let dir = issue_207_repo(&package_manifest(Some("2021")), reexport);
        let repo = dir.path();
        assert_eq!(
            added_names(&detect_api_changes_from_worktree(repo)),
            ["NewValue", "NewValue.amount"],
            "{reexport}"
        );
        assert_eq!(
            reexport_edges(RustSourceTree::Worktree, repo, "src/reader/value.rs"),
            Some(vec![edge.to_string()]),
            "{reexport}"
        );
    }
}

/// inline mod の中の相対 `pub use` は、その inline mod の子 module を指す
/// (inline mod の中の項目は公開面の判定を諦めるので、index の edge で確かめる)。
#[test]
fn relative_pub_use_in_inline_module_is_rebased_on_the_inline_module() {
    let dir = relative_reexport_repo(
        &package_manifest(Some("2021")),
        &[
            ("src/lib.rs", "pub mod reader;\n"),
            (
                "src/reader.rs",
                "mod value;\npub mod inner {\n    mod value;\n    pub use value::Inner;\n}\n",
            ),
            ("src/reader/value.rs", "pub struct Outer;\n"),
            ("src/reader/inner/value.rs", "pub struct Inner;\n"),
        ],
        &[],
    );
    assert_eq!(
        reexport_edges(RustSourceTree::Worktree, dir.path(), "src/reader/value.rs"),
        Some(vec![
            "reader::inner::Inner -> reader::inner::value::Inner".to_string()
        ])
    );
}

/// 子 module の中の相対 path での module の再公開 (`mod inner; pub use inner::api;`)。
/// 再公開した module の配下の署名変更は modified に残る。
#[test]
fn detect_api_changes_relative_module_reexport_in_child_module_keeps_signature_change() {
    for reexport in ["pub use inner::api;", "pub use self::inner::api;"] {
        let outer = format!("mod inner;\n{reexport}\n");
        let dir = relative_reexport_repo(
            &package_manifest(Some("2021")),
            &[
                ("src/lib.rs", "pub mod outer;\n"),
                ("src/outer.rs", &outer),
                ("src/outer/inner.rs", "pub mod api;\n"),
                (
                    "src/outer/inner/api.rs",
                    "pub fn f() -> bool {\n    true\n}\n",
                ),
            ],
            &[(
                "src/outer/inner/api.rs",
                "pub fn f(strict: bool) -> bool {\n    strict\n}\n",
            )],
        );
        let api = detect_api_changes_from_worktree(dir.path());
        assert_eq!(modified_names(&api), ["f"], "{reexport}");
    }
}

/// 2015 と 2018 以降で読み方が分かれる構成: root と reader の両方に `value` module があり、
/// reader が `pub use value::Root;` を書く。2015 は root の `value::Root` (公開する)、
/// 2018 以降は reader の子 module の `Root` (無いので root の `Root` は公開しない) を指す。
fn edition_split_repo(manifest: &str) -> tempfile::TempDir {
    relative_reexport_repo(
        manifest,
        &[
            ("src/lib.rs", "mod value;\npub mod reader;\n"),
            ("src/value.rs", "// empty\n"),
            ("src/reader.rs", "mod value;\n"),
            ("src/reader/value.rs", "pub struct Child;\n"),
        ],
        &[
            ("src/reader.rs", "mod value;\npub use value::Root;\n"),
            (
                "src/value.rs",
                "pub struct Root;\n\nimpl Root {\n    pub fn amount(&self) -> usize {\n        1\n    }\n}\n",
            ),
        ],
    )
}

/// 対照: edition の指定が無い (2015) crate では、従来どおりクレートルートから読む。
#[test]
fn detect_api_changes_bare_pub_use_in_edition_2015_resolves_from_crate_root() {
    let dir = edition_split_repo(&package_manifest(None));
    let repo = dir.path();
    assert_eq!(
        added_names(&detect_api_changes_from_worktree(repo)),
        ["Root", "Root.amount"]
    );
    assert_eq!(
        reexport_edges(RustSourceTree::Worktree, repo, "src/value.rs"),
        Some(vec!["reader::Root -> value::Root".to_string()])
    );
    // 2018 以降なら同じ path は reader の子 module を指す (root の Root は公開面の外)
    let dir = edition_split_repo(&package_manifest(Some("2021")));
    let repo = dir.path();
    assert!(added_names(&detect_api_changes_from_worktree(repo)).is_empty());
    assert_eq!(
        reexport_edges(RustSourceTree::Worktree, repo, "src/value.rs"),
        Some(vec!["reader::Root -> reader::value::Root".to_string()])
    );
}

/// `[lib].edition` は `[package].edition` より優先する。
#[test]
fn detect_api_changes_relative_pub_use_follows_lib_edition() {
    let lib_2021 = format!("{}\n[lib]\nedition = \"2021\"\n", package_manifest(None));
    let dir = issue_207_repo(&lib_2021, "pub use value::NewValue;");
    assert_eq!(
        added_names(&detect_api_changes_from_worktree(dir.path())),
        ["NewValue", "NewValue.amount"]
    );
    let lib_2015 = format!(
        "{}\n[lib]\nedition = \"2015\"\n",
        package_manifest(Some("2021"))
    );
    let dir = edition_split_repo(&lib_2015);
    let repo = dir.path();
    assert_eq!(
        added_names(&detect_api_changes_from_worktree(repo)),
        ["Root", "Root.amount"]
    );
    assert_eq!(
        reexport_edges(RustSourceTree::Worktree, repo, "src/value.rs"),
        Some(vec!["reader::Root -> value::Root".to_string()])
    );
}

/// `edition.workspace = true` は workspace の root の `[workspace.package].edition` を継承する。
/// 継承の指定が無い member は、root に `[workspace.package].edition` があっても 2015。
#[test]
fn detect_api_changes_relative_pub_use_follows_workspace_edition() {
    for (member_edition, want_added) in [
        (
            "edition.workspace = true\n",
            vec!["NewValue", "NewValue.amount"],
        ),
        ("", vec![]),
    ] {
        let member = format!("{}{member_edition}", package_manifest(None));
        let dir = relative_reexport_repo(
            "[workspace]\nmembers = [\"app\"]\n\n[workspace.package]\nedition = \"2021\"\n",
            &[
                ("app/Cargo.toml", &member),
                ("app/src/lib.rs", "pub mod reader;\n"),
                ("app/src/reader.rs", "// Public facade.\n"),
            ],
            &[
                (
                    "app/src/reader.rs",
                    "mod value;\npub use value::NewValue;\n",
                ),
                ("app/src/reader/value.rs", NEW_VALUE),
            ],
        );
        let repo = dir.path();
        assert_eq!(
            added_names(&detect_api_changes_from_worktree(repo)),
            want_added,
            "{member_edition:?}"
        );
        let target = if want_added.is_empty() {
            "value::NewValue"
        } else {
            "reader::value::NewValue"
        };
        assert_eq!(
            reexport_edges(RustSourceTree::Worktree, repo, "app/src/reader/value.rs"),
            Some(vec![format!("reader::NewValue -> {target}")]),
            "{member_edition:?}"
        );
    }
}

/// base と作業ツリーで edition が違う diff は、それぞれの side の manifest で読む。
/// base (2015) で `pub use value::Root;` が公開していた root の `Root` の署名変更は base の
/// 読み方で modified に残り、作業ツリー (2021) の `pub use value::Fresh;` が公開する子 module
/// の型は作業ツリーの読み方で added に出る。どちらかの side を逆の edition で読むと消える。
#[test]
fn detect_api_changes_relative_pub_use_reads_each_side_with_its_own_edition() {
    let root_value = |params: &str| {
        format!(
            "pub struct Root;\n\nimpl Root {{\n    pub fn amount(&self{params}) -> usize {{\n        1\n    }}\n}}\n"
        )
    };
    let dir = relative_reexport_repo(
        &package_manifest(None),
        &[
            ("src/lib.rs", "mod value;\npub mod reader;\n"),
            ("src/value.rs", &root_value("")),
            ("src/reader.rs", "mod value;\npub use value::Root;\n"),
            ("src/reader/value.rs", "pub struct Child;\n"),
        ],
        &[
            ("Cargo.toml", &package_manifest(Some("2021"))),
            ("src/value.rs", &root_value(", factor: usize")),
            (
                "src/reader.rs",
                "mod value;\npub use value::Root;\npub use value::Fresh;\n",
            ),
            (
                "src/reader/value.rs",
                "pub struct Child;\n\npub struct Fresh;\n",
            ),
        ],
    );
    let repo = dir.path();
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        modified_names(&api).contains(&"Root.amount".to_string()),
        "base は 2015 の読み方: {:?}",
        modified_names(&api)
    );
    assert!(
        added_names(&api).contains(&"Fresh".to_string()),
        "作業ツリーは 2021 の読み方: {:?}",
        added_names(&api)
    );
}

/// 2018 以降で先頭の名前が子 `mod` 以外 (named import) に束縛されるときは、どこを指すか
/// 決められないので index を不完全とし、公開面による抑制を諦める (再公開していない
/// private module の新しい関数も api.add に出る)。対照: anchor 付きの形なら index は作られる。
#[test]
fn detect_api_changes_relative_pub_use_through_named_import_gives_up_suppression() {
    for (reader, gives_up) in [
        ("use crate::value;\npub use value::Root;\n", true),
        ("pub use crate::value::Root;\n", false),
    ] {
        let dir = relative_reexport_repo(
            &package_manifest(Some("2021")),
            &[
                ("src/lib.rs", "mod value;\nmod hidden;\npub mod reader;\n"),
                ("src/value.rs", "pub struct Root;\n"),
                ("src/hidden.rs", "// empty\n"),
                ("src/reader.rs", reader),
            ],
            &[("src/hidden.rs", "pub fn secret() {}\n")],
        );
        let repo = dir.path();
        let added = added_names(&detect_api_changes_from_worktree(repo));
        assert_eq!(
            added.contains(&"secret".to_string()),
            gives_up,
            "{reader}: {added:?}"
        );
        assert_eq!(
            reexport_edges(RustSourceTree::Worktree, repo, "src/hidden.rs").is_none(),
            gives_up,
            "{reader}"
        );
    }
}

/// 外部の Global path はローカルのグラフから外し、Bare の extern 名は子 module に付け替えない。
#[test]
fn relative_pub_use_rebase_skips_global_paths_and_extern_crate_names() {
    let dir = relative_reexport_repo(
        &package_manifest(Some("2021")),
        &[
            ("src/lib.rs", "pub mod reader;\n"),
            (
                "src/reader.rs",
                "mod value;\npub use ::value::NewValue;\npub use serde::Serialize;\npub use self::value::NewValue as Again;\n",
            ),
            ("src/reader/value.rs", NEW_VALUE),
        ],
        &[],
    );
    assert_eq!(
        reexport_edges(RustSourceTree::Worktree, dir.path(), "src/reader/value.rs"),
        Some(vec![
            "reader::Serialize -> serde::Serialize".to_string(),
            "reader::Again -> reader::value::NewValue".to_string(),
        ])
    );
}

/// edition が不明 (壊れた manifest・未知の値) で、子 `mod` を先頭に持つ bare の再公開が
/// あると、読み方を決められないので抑制を諦める。対照: そのような edge が無ければ
/// (anchor 付きの形だけなら) edition が不明でも index は作られる。
#[test]
fn detect_api_changes_relative_pub_use_with_unknown_edition_gives_up_suppression() {
    for manifest in [
        format!("{}[lib\n", package_manifest(Some("2021"))),
        package_manifest(Some("2027")),
    ] {
        for (reexport, gives_up) in [
            ("pub use value::NewValue;", true),
            ("pub use self::value::NewValue;", false),
        ] {
            let dir = relative_reexport_repo(
                &manifest,
                &[
                    ("src/lib.rs", "pub mod reader;\nmod hidden;\n"),
                    ("src/hidden.rs", "// empty\n"),
                    ("src/reader.rs", "// Public facade.\n"),
                ],
                &[
                    ("src/reader.rs", &format!("mod value;\n{reexport}\n")),
                    ("src/reader/value.rs", NEW_VALUE),
                    ("src/hidden.rs", "pub fn secret() {}\n"),
                ],
            );
            let repo = dir.path();
            let added = added_names(&detect_api_changes_from_worktree(repo));
            assert!(
                added.contains(&"NewValue".to_string()),
                "{manifest:?} {reexport}: {added:?}"
            );
            assert_eq!(
                added.contains(&"secret".to_string()),
                gives_up,
                "{manifest:?} {reexport}: {added:?}"
            );
            assert_eq!(
                reexport_edges(RustSourceTree::Worktree, repo, "src/hidden.rs").is_none(),
                gives_up,
                "{manifest:?} {reexport}"
            );
        }
    }
}

/// edition が不明なときに推測の edge を作らない (両方の読み方の edge を作ると、glob の shadow
/// 判定に推測が混ざる)。m は `pub use value::api;` と `pub use self::internal::*;` を持ち、
/// `value::api` は root 起点 (2015) なら module、子 module 起点 (2018 以降) なら関数を指す。
/// 2015 では root の module が glob の `api` module を隠すので `internal::api::found` は公開面の
/// 外、2018 以降は関数なので隠さず公開面の中。edition が不明なら抑制を諦めて残す
/// (root 起点の推測の edge を足すと、glob の module を隠して見逃す)。
#[test]
fn detect_api_changes_relative_pub_use_does_not_shadow_globs_with_guessed_edges() {
    for (edition, reported, edges) in [
        (Some("2021"), true, Some("m::api -> m::value::api")),
        (None, false, Some("m::api -> value::api")),
        (Some("2027"), true, None),
    ] {
        let dir = relative_reexport_repo(
            &package_manifest(edition),
            &[
                ("src/lib.rs", "mod value;\npub mod m;\n"),
                ("src/value.rs", "pub mod api {}\n"),
                (
                    "src/m.rs",
                    "mod value;\nmod internal;\npub use value::api;\npub use self::internal::*;\n",
                ),
                ("src/m/value.rs", "pub fn api() {}\n"),
                ("src/m/internal.rs", "pub mod api;\n"),
                (
                    "src/m/internal/api.rs",
                    "pub fn found() -> u32 {\n    0\n}\n",
                ),
            ],
            &[(
                "src/m/internal/api.rs",
                "pub fn found(strict: bool) -> u32 {\n    u32::from(strict)\n}\n",
            )],
        );
        let repo = dir.path();
        let api = detect_api_changes_from_worktree(repo);
        assert_eq!(
            modified_names(&api).contains(&"found".to_string()),
            reported,
            "{edition:?}: {:?}",
            modified_names(&api)
        );
        let got = reexport_edges(RustSourceTree::Worktree, repo, "src/m/internal/api.rs");
        assert_eq!(
            got.as_ref().map(|edges| edges[0].as_str()),
            edges,
            "{edition:?}: {got:?}"
        );
    }
}

/// 子 mod のファイルの先頭の内部属性 (`#![cfg(..)]`) もその module に掛かる。reader の
/// `mod value;` が cfg で消えると、同じスコープの `use crate::value;` が `value` を束縛し、
/// `pub use value::Root;` の再公開先は root の `Root` になる。子 module へ付け替えると、
/// root の `Root` の追加・署名変更・削除を見逃すので、作業ツリー側でも base 側でも残す。
#[test]
fn detect_api_changes_relative_pub_use_does_not_trust_a_cfg_removed_child_module() {
    let reader = "mod value;\nuse crate::value;\npub use value::Root;\n";
    let root = |methods: &str| format!("pub struct Root;\n\nimpl Root {{\n{methods}}}\n");
    // 作業ツリー側 (api.add)
    let dir = relative_reexport_repo(
        &package_manifest(Some("2024")),
        &[
            ("src/lib.rs", "mod value;\npub mod reader;\n"),
            ("src/value.rs", "pub struct Root;\n"),
            ("src/reader.rs", "// Public facade.\n"),
        ],
        &[
            ("src/reader.rs", reader),
            ("src/reader/value.rs", "#![cfg(any())]\n"),
            ("src/value.rs", &root("    pub fn amount(&self) {}\n")),
        ],
    );
    let added = added_names(&detect_api_changes_from_worktree(dir.path()));
    assert!(added.contains(&"Root.amount".to_string()), "{added:?}");
    // base 側 (api.mod / api.rm)
    let dir = relative_reexport_repo(
        &package_manifest(Some("2024")),
        &[
            ("src/lib.rs", "mod value;\npub mod reader;\n"),
            (
                "src/value.rs",
                &root("    pub fn amount(&self) {}\n\n    pub fn gone(&self) {}\n"),
            ),
            ("src/reader.rs", reader),
            ("src/reader/value.rs", "#![cfg(any())]\n"),
        ],
        &[(
            "src/value.rs",
            &root("    pub fn amount(&self, factor: usize) {\n        let _ = factor;\n    }\n"),
        )],
    );
    let repo = dir.path();
    let api = detect_api_changes_from_worktree(repo);
    let removed: Vec<&str> = api
        .removed
        .iter()
        .chain(&api.removed_dead)
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        modified_names(&api).contains(&"Root.amount".to_string()),
        "{:?}",
        modified_names(&api)
    );
    assert!(removed.contains(&"Root.gone"), "{removed:?}");
    let blobs = GitBlobBatch::new(repo.to_str().expect("utf-8 path"), "HEAD");
    assert_eq!(
        reexport_edges(RustSourceTree::Base { blobs: &blobs }, repo, "src/value.rs"),
        None,
        "base 側の index は不完全"
    );
}

/// 子 mod のファイルの内部属性は、判定する側の snapshot から読む (base 側を作業ツリーで補わない)。
/// 条件 (`cfg`)・`cfg_attr`・未知の属性があるか、ファイルが見つからない子 mod には付け替えず、
/// index を不完全とする。対照: 条件にならない属性 (`allow` / `doc`) だけなら付け替える。
#[test]
fn relative_pub_use_rebase_reads_child_module_file_attributes_from_its_own_side() {
    let child = |attribute: &str| format!("{attribute}{NEW_VALUE}");
    for (attribute, rebased) in [
        ("#![cfg(any())]\n", false),
        ("#![cfg_attr(test, allow(dead_code))]\n", false),
        ("#![rustfmt::skip]\n", false),
        ("#![allow(dead_code)]\n//! module docs\n", true),
        ("", true),
    ] {
        let dir = issue_207_repo(&package_manifest(Some("2021")), "pub use value::NewValue;");
        let repo = dir.path();
        fs::write(repo.join("src/reader/value.rs"), child(attribute)).expect("write");
        let edges = reexport_edges(RustSourceTree::Worktree, repo, "src/reader/value.rs");
        let want = rebased.then(|| vec!["reader::NewValue -> reader::value::NewValue".to_string()]);
        assert_eq!(edges, want, "{attribute:?}");
    }
    // ファイルが見つからない子 mod
    let dir = relative_reexport_repo(
        &package_manifest(Some("2021")),
        &[
            ("src/lib.rs", "pub mod reader;\nmod hidden;\n"),
            ("src/hidden.rs", "pub fn secret() {}\n"),
            ("src/reader.rs", "mod value;\npub use value::NewValue;\n"),
        ],
        &[],
    );
    assert_eq!(
        reexport_edges(RustSourceTree::Worktree, dir.path(), "src/hidden.rs"),
        None
    );
    // base は cfg で消え、作業ツリーでは条件を外した: それぞれの side のファイルで決める
    let dir = relative_reexport_repo(
        &package_manifest(Some("2021")),
        &[
            ("src/lib.rs", "pub mod reader;\n"),
            ("src/reader.rs", "mod value;\npub use value::NewValue;\n"),
            ("src/reader/value.rs", &child("#![cfg(any())]\n")),
        ],
        &[("src/reader/value.rs", NEW_VALUE)],
    );
    let repo = dir.path();
    let blobs = GitBlobBatch::new(repo.to_str().expect("utf-8 path"), "HEAD");
    assert_eq!(
        reexport_edges(
            RustSourceTree::Base { blobs: &blobs },
            repo,
            "src/reader/value.rs"
        ),
        None,
        "base は base の snapshot の内部属性で決める"
    );
    assert_eq!(
        reexport_edges(RustSourceTree::Worktree, repo, "src/reader/value.rs"),
        Some(vec![
            "reader::NewValue -> reader::value::NewValue".to_string()
        ])
    );
}

/// 子 mod と同じ名前を束縛し得るものがスコープにあれば、子 mod を優先しない (コンパイルが
/// 通るなら片方は cfg で消えるはずで、どちらが残るか決められない)。競合に数えるのは型の
/// 名前空間に入り得るもの (named import・別名付きの import・`extern crate`・struct・enum・
/// union・trait・type alias・重複した mod)。対照: 値の名前空間だけの項目 (fn・const・static)・
/// macro_rules と、子 mod の中を指す自分自身の再公開 (`pub use value::value;`) は子 mod と
/// 共存できるので、付け替える。
#[test]
fn relative_pub_use_rebase_gives_up_when_another_binding_competes_with_the_child_module() {
    for (reader, rebased) in [
        (
            "mod value;\nuse crate::other::value;\npub use value::NewValue;\n",
            false,
        ),
        (
            "mod value;\nuse crate::other::Thing as value;\npub use value::NewValue;\n",
            false,
        ),
        (
            "mod value;\nextern crate core as value;\npub use value::NewValue;\n",
            false,
        ),
        (
            "mod value;\npub struct value;\npub use value::NewValue;\n",
            false,
        ),
        (
            "mod value;\npub enum value {}\npub use value::NewValue;\n",
            false,
        ),
        (
            "mod value;\npub union value {\n    a: u8,\n}\npub use value::NewValue;\n",
            false,
        ),
        (
            "mod value;\npub trait value {}\npub use value::NewValue;\n",
            false,
        ),
        (
            "mod value;\npub type value = u8;\npub use value::NewValue;\n",
            false,
        ),
        (
            "mod value;\nmod value {}\npub use value::NewValue;\n",
            false,
        ),
        (
            "mod value;\npub use value::value;\npub use value::NewValue;\n",
            true,
        ),
        (
            "mod value;\nfn value() {}\npub use value::NewValue;\n",
            true,
        ),
        (
            "mod value;\nconst value: u8 = 0;\npub use value::NewValue;\n",
            true,
        ),
        (
            "mod value;\nstatic value: u8 = 0;\npub use value::NewValue;\n",
            true,
        ),
        (
            "mod value;\nmacro_rules! value {\n    () => {};\n}\npub use value::NewValue;\n",
            true,
        ),
    ] {
        let dir = relative_reexport_repo(
            &package_manifest(Some("2021")),
            &[
                ("src/lib.rs", "mod other;\npub mod reader;\n"),
                ("src/other.rs", "pub mod value {}\npub struct Thing;\n"),
                ("src/reader.rs", reader),
                (
                    "src/reader/value.rs",
                    &format!("{NEW_VALUE}\npub fn value() {{}}\n"),
                ),
            ],
            &[],
        );
        let edges = reexport_edges(RustSourceTree::Worktree, dir.path(), "src/reader/value.rs");
        assert_eq!(
            edges.as_ref().map(|edges| edges
                .iter()
                .any(|edge| edge == "reader::NewValue -> reader::value::NewValue")),
            rebased.then_some(true),
            "{reader}: {edges:?}"
        );
    }
}

/// 構文エラー (ERROR / MISSING) のあるファイルが crate にあると、module の直下の公開宣言が
/// エラーのノードに取り込まれて edge が欠け得るので、index を作らず公開面による抑制を諦める
/// (再公開していない private module の新しい関数も api.add に出る)。edge を集めない ignore
/// された module ファイルも、module tree を辿るときに確かめる。対照: エラーが無ければ index は
/// 作られ、抑えたまま。
#[test]
fn detect_api_changes_gives_up_public_surface_when_a_source_file_has_syntax_errors() {
    for (reader, extra, gives_up) in [
        // module の直下の ERROR
        (") pub use self::value::Other;\n", None, true),
        // MISSING (閉じていない項目)
        ("pub struct Broken {\n    pub a: u8,\n", None, true),
        // 関数の本体の中のエラーもファイル単位で諦める
        ("fn f() -> u8 { 1 + }\n", None, true),
        // edge の収集では ignore される module ファイルのエラー
        ("mod ignored;\n", Some("pub fn broken( {}\n"), true),
        ("", None, false),
        ("mod ignored;\n", Some("pub fn fine() {}\n"), false),
    ] {
        let reader = format!("mod value;\npub use self::value::NewValue;\n{reader}");
        let mut base = vec![
            ("src/lib.rs", "pub mod reader;\nmod hidden;\n"),
            ("src/hidden.rs", "// empty\n"),
            ("src/reader.rs", reader.as_str()),
            (
                "src/reader/value.rs",
                "pub struct NewValue;\npub struct Other;\n",
            ),
        ];
        if let Some(ignored) = extra {
            base.push((".gitignore", "/src/reader/ignored.rs\n"));
            base.push(("src/reader/ignored.rs", ignored));
        }
        let dir = relative_reexport_repo(
            &package_manifest(Some("2021")),
            &base,
            &[("src/hidden.rs", "pub fn secret() {}\n")],
        );
        let repo = dir.path();
        let added = added_names(&detect_api_changes_from_worktree(repo));
        assert_eq!(
            added.contains(&"secret".to_string()),
            gives_up,
            "{reader} {extra:?}: {added:?}"
        );
        assert_eq!(
            reexport_edges(RustSourceTree::Worktree, repo, "src/hidden.rs").is_none(),
            gives_up,
            "{reader} {extra:?}"
        );
    }
}

/// 関数の本体などのブロックの中の `pub use` と `mod` は公開経路を作らない (外の module からは
/// 辿れない)。module の edge や module tree に混ぜると、glob で入る `internal::api` を
/// 誤って隠し、`exposed` の署名変更・削除を見逃す (codex の指摘)。
/// 対照: 同じ named re-export を module の直下に置けば、glob は隠される (抑えたまま)。
#[test]
fn detect_api_changes_block_scoped_items_do_not_shadow_module_globs() {
    for (facade, reported) in [
        (
            "pub use crate::internal::*;\n\nfn helper() {\n    pub use {super::other::api};\n}\n",
            true,
        ),
        (
            "pub use crate::internal::*;\n\nfn helper() {\n    pub use crate::other::api;\n}\n",
            true,
        ),
        (
            "pub use crate::internal::*;\n\nfn helper() {\n    mod api {}\n}\n",
            true,
        ),
        (
            "pub use crate::internal::*;\npub use crate::other::api;\n",
            false,
        ),
    ] {
        let dir = relative_reexport_repo(
            &package_manifest(Some("2021")),
            &[
                (
                    "src/lib.rs",
                    "mod internal;\nmod other {\n    pub mod api {}\n}\npub mod facade;\n",
                ),
                ("src/internal.rs", "pub mod api;\n"),
                (
                    "src/internal/api.rs",
                    "pub fn exposed() {}\n\npub fn gone() {}\n",
                ),
                ("src/facade.rs", facade),
            ],
            &[(
                "src/internal/api.rs",
                "pub fn exposed(flag: bool) {\n    let _ = flag;\n}\n",
            )],
        );
        let api = detect_api_changes_from_worktree(dir.path());
        let removed: Vec<&str> = api
            .removed
            .iter()
            .chain(&api.removed_dead)
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(
            modified_names(&api).contains(&"exposed".to_string()),
            reported,
            "{facade}: {:?}",
            modified_names(&api)
        );
        assert_eq!(removed.contains(&"gone"), reported, "{facade}: {removed:?}");
    }
}
