//! 削除シンボルの帰属判定 (api.rm / removed_dead) のテスト。

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

/// 明示 diff の旧ソースだけがあるときは、旧公開面を推測して削除候補を捨てない。
#[test]
fn detect_api_changes_deleted_source_without_old_base_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let old = "pub struct Record { pub value: i32 }\n";
    git_commit_files(
        repo,
        &[
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\n",
            ),
            ("core/src/lib.rs", old),
        ],
        "old",
    );
    fs::remove_file(repo.join("core/src/lib.rs")).expect("remove old source");
    git_commit_files(
        repo,
        &[("App/Caller.swift", "func call() { let _ = Record.self }\n")],
        "new",
    );
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(old.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|symbol| symbol.name == "Record"),
        "missing old base must keep the deletion blocking: {api:?}"
    );
    assert!(api.parse_truncations.iter().any(|item| {
        item.reason == crate::models::truncation::TruncationReason::DiffBaseMismatch
    }));
    fs::remove_file(repo.join("App/Caller.swift")).expect("remove caller");
    let no_refs = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        no_refs.removed.iter().any(|symbol| symbol.name == "Record"),
        "unverified source must not become removed_dead: {no_refs:?}"
    );
}

#[test]
fn detect_api_changes_deleted_source_with_matching_old_base_keeps_normal_classification() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\n",
            ),
            (
                "core/src/lib.rs",
                "pub struct Record { pub value: i32 }\r\n",
            ),
        ],
        "old",
    );
    fs::remove_file(repo.join("core/src/lib.rs")).expect("remove old source");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(b"pub struct Record { pub value: i32 }\n".to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(api.parse_truncations.is_empty(), "matching base: {api:?}");
    assert!(
        api.removed_dead
            .iter()
            .any(|symbol| symbol.name == "Record"),
        "verified unreferenced deletion remains informational: {api:?}"
    );
}

#[test]
fn detect_api_changes_deleted_source_conflicting_with_base_uses_diff_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\n",
            ),
            ("core/src/lib.rs", "pub struct Other;\n"),
        ],
        "base",
    );
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(b"pub struct Record;\n".to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|symbol| symbol.name == "Record"),
        "{api:?}"
    );
    assert!(!api.removed.iter().any(|symbol| symbol.name == "Other"));
    assert!(api.parse_truncations.iter().any(|item| {
        item.reason == crate::models::truncation::TruncationReason::DiffBaseMismatch
    }));
}

#[test]
fn detect_api_changes_incomplete_deleted_source_prefers_complete_base() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\n",
            ),
            ("core/src/lib.rs", "pub struct Complete;\n"),
        ],
        "base",
    );
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(b"pub struct Partial;\n".to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|symbol| symbol.name == "Complete"),
        "{api:?}"
    );
    assert!(!api.removed.iter().any(|symbol| symbol.name == "Partial"));
    assert!(api.parse_truncations.iter().any(|item| {
        item.reason == crate::models::truncation::TruncationReason::DiffBaseMismatch
    }));
}

#[test]
fn detect_api_changes_deleted_source_missing_everywhere_reports_mismatch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("README.md", "temporary\n")], "base");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(api.removed.is_empty());
    assert!(api.parse_truncations.iter().any(|item| {
        item.path.as_deref() == Some("core/src/lib.rs")
            && item.reason == crate::models::truncation::TruncationReason::DiffBaseMismatch
    }));
}

/// 削除と、対応言語の外への内容同一の rename は、どちらも旧ファイルの削除として準備する。
/// base と照合できない旧ソースは、diff の順にそれぞれの主語で申告する。
#[test]
fn detect_api_changes_reports_unverified_deleted_and_renamed_sources_in_diff_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let kept = "export function kept(): void {}\n";
    git_commit_files(repo, &[("kept.ts", kept)], "base");
    fs::remove_file(repo.join("kept.ts")).expect("remove renamed source");
    fs::write(repo.join("kept.ts.bak"), kept).expect("write verified rename");
    fs::write(repo.join("moved.rs.bak"), "pub struct Moved;\n").expect("write unverified rename");
    let deleted = crate::models::impact::DiffFile {
        old_path: "gone/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: None,
    };
    let renamed = crate::models::impact::DiffFile {
        old_path: "moved.rs".to_string(),
        new_path: "moved.rs.bak".to_string(),
        hunks: vec![],
        deleted_old_source: None,
    };
    // 対照: base にある旧ソースは照合でき、申告せずに旧ファイルの API を削除として扱う。
    let verified_rename = crate::models::impact::DiffFile {
        old_path: "kept.ts".to_string(),
        new_path: "kept.ts.bak".to_string(),
        hunks: vec![],
        deleted_old_source: None,
    };
    let api = detect_api_changes(
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        &[renamed, verified_rename, deleted],
    );
    let mismatches: Vec<_> = api
        .parse_truncations
        .iter()
        .filter(|item| item.reason == crate::models::truncation::TruncationReason::DiffBaseMismatch)
        .map(|item| (item.path.as_deref(), item.message.as_str()))
        .collect();
    assert_eq!(
        mismatches,
        vec![
            (
                Some("moved.rs"),
                "renamed file's old source cannot be verified against the requested base",
            ),
            (
                Some("gone/src/lib.rs"),
                "deleted file's old source cannot be verified against the requested base",
            ),
        ],
        "{api:?}"
    );
    assert!(
        api.removed
            .iter()
            .chain(&api.removed_dead)
            .any(|symbol| symbol.name == "kept" && symbol.file == "kept.ts"),
        "a rename with a verified old source must still remove its API: {api:?}"
    );
}

#[test]
fn detect_api_changes_deleted_non_source_and_gitlink_do_not_report_mismatch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("placeholder.txt", "temporary\n")], "base");
    let diff_files = [
        ("README.md", b"# Documentation\n".as_slice()),
        (
            "dependency",
            b"Subproject commit 0123456789abcdef0123456789abcdef01234567\n".as_slice(),
        ),
        (
            "dependency.rs",
            b"Subproject commit 0123456789abcdef0123456789abcdef01234567\n".as_slice(),
        ),
    ]
    .into_iter()
    .map(|(path, old)| crate::models::impact::DiffFile {
        old_path: path.to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(old.to_vec()),
    })
    .collect::<Vec<_>>();
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(api.removed.is_empty(), "non-source deletion: {api:?}");
    assert!(
        api.parse_truncations.is_empty(),
        "non-source deletion: {api:?}"
    );
}

/// 削除前の Rust crate に FFI 面が無ければ、Swift の同名型・利用は旧 Rust 型への参照にならない。
/// 同名 Swift 定義が複数ある場合も、残存定義数だけを根拠に blocking に戻さない。
#[test]
fn detect_api_changes_rust_to_swift_rewrite_with_no_ffi_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "pub struct Record { pub value: String }\npub enum Phase { Ready, Done }\n";
    git_commit_files(
        repo,
        &[
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n",
            ),
            ("core/src/lib.rs", deleted_src),
        ],
        "base",
    );
    fs::remove_file(repo.join("core/src/lib.rs")).expect("remove Rust source");
    fs::create_dir_all(repo.join("App")).expect("create Swift directory");
    fs::write(repo.join("App/Record.swift"), "struct Record { let value: String }\nenum Phase { case ready, done }\nfunc useRecord() { let _ = Record(value: \"x\"); let _ = Phase.ready }\n").expect("write Swift source");
    fs::create_dir_all(repo.join("OtherModule")).expect("create second Swift module");
    fs::write(
        repo.join("OtherModule/Record.swift"),
        "struct Record { let value: Int }\n",
    )
    .expect("write second Swift source");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    for name in ["Record", "Phase"] {
        assert!(
            api.removed_dead.iter().any(|s| s.name == name),
            "{name} should be informational: {api:?}"
        );
        assert!(
            !api.removed.iter().any(|s| s.name == name),
            "{name} should not block: {api:?}"
        );
    }
}

/// wasm-bindgen / napi-rs / neon は生成した JS モジュールから Rust の関数を同じ識別子で公開し、
/// C ABI の関数も wasm の `instance.exports.<名前>` として同じ名前で呼べる。旧ツリーにその痕跡が
/// あれば、JS からの同名参照を「言語をまたいで束縛できない別物」とみなさず blocking に残す。
/// 痕跡が無い crate なら、JS の無関係な同名関数は従来どおり別物として降格する (対照)。
#[test]
fn detect_api_changes_rust_removal_called_from_js_keeps_blocking_with_binding_evidence() {
    let plain_toml = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\", \"rlib\"]\n";
    let wasm_toml = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\", \"rlib\"]\n[dependencies]\nwasm-bindgen = \"0.2\"\n";
    let napi_toml = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n[dependencies]\nnapi-derive = \"2\"\n";
    for (label, toml, old_lib, new_lib, js, expect_blocking) in [
        (
            "wasm-bindgen",
            wasm_toml,
            "use wasm_bindgen::prelude::*;\n#[wasm_bindgen]\npub fn greet(name: &str) -> String { format!(\"hi {name}\") }\n#[wasm_bindgen]\npub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            "use wasm_bindgen::prelude::*;\n#[wasm_bindgen]\npub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            "import init, { greet, add } from \"../pkg/demo.js\";\nawait init();\nexport const text = greet(\"world\") + add(1, 2);\n",
            true,
        ),
        (
            "wasm C ABI export",
            plain_toml,
            "#[no_mangle]\npub extern \"C\" fn greet() -> i32 { 1 }\n#[no_mangle]\npub extern \"C\" fn add() -> i32 { 2 }\n",
            "#[no_mangle]\npub extern \"C\" fn add() -> i32 { 2 }\n",
            "export function run(instance) { return instance.exports.greet(); }\n",
            true,
        ),
        (
            "napi-rs",
            napi_toml,
            "use napi_derive::napi;\n#[napi]\npub fn greet() -> u32 { 1 }\n#[napi]\npub fn add() -> u32 { 2 }\n",
            "use napi_derive::napi;\n#[napi]\npub fn add() -> u32 { 2 }\n",
            "const { greet } = require(\"../index.node\");\nmodule.exports = () => greet();\n",
            true,
        ),
        (
            "no binding evidence",
            plain_toml,
            "pub fn greet() -> u32 { 1 }\npub fn add() -> u32 { 2 }\n",
            "pub fn add() -> u32 { 2 }\n",
            "function greet() { return 1; }\nexport const value = greet();\n",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", toml),
                ("src/lib.rs", old_lib),
                ("web/main.js", js),
            ],
            "base",
        );
        fs::write(repo.join("src/lib.rs"), new_lib).expect("write lib.rs");
        let api = detect_api_changes_from_worktree(repo);
        let blocking = api.removed.iter().any(|s| s.name == "greet");
        let informational = api.removed_dead.iter().any(|s| s.name == "greet");
        assert_eq!(
            (blocking, informational),
            (expect_blocking, !expect_blocking),
            "{label}: removed={:?} removed_dead={:?}",
            api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
            api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }
}

/// 拡張子の折り畳みや別名の include で FFI を見落とす場合は降格しない。
#[test]
fn detect_api_changes_rust_swift_uncertain_ffi_sources_stay_blocking() {
    for (source_path, source) in [
        (
            "core/src/ffi.RS",
            "#[no_mangle] pub extern \"C\" fn ffi() {}\n",
        ),
        (
            "core/src/ffi.rſ",
            "#[no_mangle] pub extern \"C\" fn ffi() {}\n",
        ),
        ("core/src/loader.rs", "#[path = \"ffi.inc\"] mod ffi;\n"),
        ("core/src/loader.rs", "# [path = \"ffi.inc\"] mod ffi;\n"),
        (
            "core/src/loader.rs",
            "#[path /*note*/ = \"ffi.inc\"] mod ffi;\n",
        ),
        ("core/src/loader.rs", "include /*note*/ !(\"ffi.inc\");\n"),
        (
            "core/src/loader.rs",
            "macro_rules! load { () => { #[cfg_attr(all(), path = \"ffi.inc\")] mod ffi; } }\n",
        ),
        (
            "core/src/loader.rs",
            "use std::include as load; load!(\"ffi.inc\");\n",
        ),
        ("core/src/loader.rs", "include!{\"ffi.inc\"}\n"),
        (
            "core/src/loader.rs",
            "#[cfg_attr(all(), path = \"ffi.inc\")] mod ffi;\n",
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let deleted_src = "pub struct Record { pub value: i32 }\n";
        git_commit_files(
            repo,
            &[
                (
                    "core/Cargo.toml",
                    "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                ("core/src/lib.rs", deleted_src),
                (source_path, source),
            ],
            "base",
        );
        fs::remove_file(repo.join("core/src/lib.rs")).expect("remove Rust source");
        fs::create_dir_all(repo.join("App")).expect("create Swift directory");
        fs::write(
            repo.join("App/Caller.swift"),
            "func useOld() { let _ = Record.self }\n",
        )
        .expect("write Swift source");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "core/src/lib.rs".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![],
            deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api.removed.iter().any(|s| s.name == "Record"),
            "{source_path} must remain blocking: {api:?}"
        );
    }
}

/// FFI で公開されていた Rust の関数・型は、Swift の同名利用が残る限り blocking を保つ。
#[test]
fn detect_api_changes_rust_ffi_rewrite_keeps_blocking() {
    for (deleted_src, name) in [
        (
            "#[no_mangle]\npub extern \"C\" fn openStore() {}\n",
            "openStore",
        ),
        ("#[repr(C)]\npub struct Point { pub x: i32 }\n", "Point"),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                (
                    "core/Cargo.toml",
                    "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n",
                ),
                ("core/src/lib.rs", deleted_src),
            ],
            "base",
        );
        fs::remove_file(repo.join("core/src/lib.rs")).expect("remove Rust source");
        fs::create_dir_all(repo.join("App")).expect("create Swift directory");
        fs::write(
            repo.join("App/Caller.swift"),
            format!("func useOld() {{ let _ = {name} }}\n"),
        )
        .expect("write Swift source");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "core/src/lib.rs".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![],
            deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api.removed.iter().any(|s| s.name == name),
            "{name} must remain blocking: {api:?}"
        );
    }
}

/// 別 crate に FFI ブリッジがある場合も、旧型を Swift から使い得るので blocking。
#[test]
fn detect_api_changes_rust_ffi_in_another_crate_keeps_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "pub struct Record { pub value: i32 }\n";
    git_commit_files(
        repo,
        &[
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("core/src/lib.rs", deleted_src),
            (
                "bridge/Cargo.toml",
                "[package]\nname = \"bridge\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ncore = { path = \"../core\" }\n",
            ),
            (
                "bridge/src/lib.rs",
                "use core::Record;\n#[no_mangle]\npub extern \"C\" fn new_record() -> *mut Record { std::ptr::null_mut() }\n",
            ),
        ],
        "base",
    );
    fs::remove_file(repo.join("core/src/lib.rs")).expect("remove Rust source");
    fs::create_dir_all(repo.join("App")).expect("create Swift directory");
    fs::write(
        repo.join("App/Caller.swift"),
        "func useOld() { let _ = Record.self }\n",
    )
    .expect("write Swift source");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "Record"),
        "cross-crate FFI must remain blocking: {api:?}"
    );
}

/// Swift 以外の言語は今回の降格に含めず、既存の保守的な判定を維持する。
#[test]
fn detect_api_changes_rust_rewrite_python_refs_keep_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "pub struct Record { pub value: i32 }\n";
    git_commit_files(
        repo,
        &[
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("core/src/lib.rs", deleted_src),
        ],
        "base",
    );
    fs::remove_file(repo.join("core/src/lib.rs")).expect("remove Rust source");
    fs::write(
        repo.join("caller.py"),
        "def use_old():\n    return Record()\n",
    )
    .expect("write Python source");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "Record"),
        "Python reference must remain blocking: {api:?}"
    );
}

/// Swift の同名参照を除外できても、Rust 側に残った参照は削除 API の利用として扱う。
#[test]
fn detect_api_changes_rust_rewrite_mixed_refs_keep_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "pub struct Record { pub value: i32 }\n";
    git_commit_files(
        repo,
        &[
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("core/src/lib.rs", deleted_src),
        ],
        "base",
    );
    fs::remove_file(repo.join("core/src/lib.rs")).expect("remove Rust source");
    fs::create_dir_all(repo.join("App")).expect("create Swift directory");
    fs::write(
        repo.join("App/Caller.swift"),
        "func useOld() { let _ = Record.self }\n",
    )
    .expect("write Swift source");
    fs::write(
        repo.join("caller.rs"),
        "fn use_old(value: Record) { let _ = value; }\n",
    )
    .expect("write Rust source");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "Record"),
        "same-language reference must remain blocking: {api:?}"
    );
}

/// 複数の他言語定義に対する従来の定義数ガードは、Swift の修正で緩めない。
#[test]
fn detect_api_changes_python_class_with_two_php_definitions_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "class Config:\n    pass\n";
    git_commit_files(
        repo,
        &[
            ("pkg/config.py", deleted_src),
            ("a.php", "<?php\nclass Config {}\n"),
            ("b.php", "<?php\nclass Config {}\n"),
        ],
        "base",
    );
    fs::remove_file(repo.join("pkg/config.py")).expect("remove Python source");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "pkg/config.py".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "Config"),
        "other languages keep the old definition-count guard: {api:?}"
    );
}

/// Swift の同名定義だけが複数残る場合は、使用参照ゼロでも FFI 証拠に応じて分類する。
/// Rust 定義が混じる対照では定義数ガードを維持する。
#[test]
fn detect_api_changes_rust_swift_duplicate_definitions_respect_ffi() {
    for case in ["plain", "ffi", "mixed"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let deleted_src = if case == "ffi" {
            "#[repr(C)]\npub struct Record { pub value: i32 }\n"
        } else {
            "pub struct Record { pub value: i32 }\n"
        };
        git_commit_files(
            repo,
            &[
                (
                    "core/Cargo.toml",
                    "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                ("core/src/lib.rs", deleted_src),
            ],
            "base",
        );
        fs::remove_file(repo.join("core/src/lib.rs")).expect("remove old Rust source");
        fs::create_dir_all(repo.join("App")).expect("create Swift directory");
        fs::create_dir_all(repo.join("OtherModule")).expect("create second module");
        fs::write(repo.join("App/Record.swift"), "struct Record {}\n")
            .expect("write Swift definition");
        fs::write(repo.join("OtherModule/Record.swift"), "struct Record {}\n")
            .expect("write second Swift definition");
        if case == "mixed" {
            fs::write(
                repo.join("core/src/another.rs"),
                "pub struct Record { pub changed: u8 }\n",
            )
            .expect("write Rust definition");
        }
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "core/src/lib.rs".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![],
            deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert_eq!(
            api.removed.iter().any(|s| s.name == "Record"),
            case != "plain",
            "case={case}: {api:?}"
        );
        assert_eq!(
            api.removed_dead.iter().any(|s| s.name == "Record"),
            case == "plain",
            "case={case}: {api:?}"
        );
    }
}

/// 旧ツリーの gitlink 内は検査できないため、Swift の同名参照を除外しない。
#[test]
fn detect_api_changes_rust_swift_gitlink_keeps_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "pub struct Record { pub value: i32 }\n";
    git_commit_files(
        repo,
        &[
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("core/src/lib.rs", deleted_src),
        ],
        "base",
    );
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("read HEAD");
    assert!(head.status.success());
    let head = String::from_utf8(head.stdout).expect("utf-8 commit");
    let add = Command::new("git")
        .args(["update-index", "--add", "--cacheinfo"])
        .arg(format!("160000,{},ffi", head.trim()))
        .current_dir(repo)
        .status()
        .expect("stage gitlink");
    assert!(add.success());
    let commit = Command::new("git")
        .args(["commit", "-m", "add gitlink"])
        .current_dir(repo)
        .status()
        .expect("commit gitlink");
    assert!(commit.success());
    fs::remove_file(repo.join("core/src/lib.rs")).expect("remove Rust source");
    fs::create_dir_all(repo.join("App")).expect("create Swift directory");
    fs::write(
        repo.join("App/Caller.swift"),
        "func useOld() { let _ = Record.self }\n",
    )
    .expect("write Swift source");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "Record"),
        "gitlink must keep blocking: {api:?}"
    );
}

/// 追跡済み Markdown 文書へのリンクは除外し、ソースリンクは blocking を保つ。
#[cfg(unix)]
#[test]
fn detect_api_changes_rust_swift_document_and_source_symlinks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "pub struct Record { pub value: i32 }\n";
    git_commit_files(
        repo,
        &[
            (
                "core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("core/src/lib.rs", deleted_src),
            ("AGENTS.md", "# Project notes\n"),
        ],
        "base",
    );
    std::os::unix::fs::symlink("AGENTS.md", repo.join("CLAUDE.md")).expect("create document link");
    assert!(
        Command::new("git")
            .args(["add", "CLAUDE.md"])
            .current_dir(repo)
            .status()
            .expect("stage link")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "add document link"])
            .current_dir(repo)
            .status()
            .expect("commit link")
            .success()
    );
    fs::remove_file(repo.join("core/src/lib.rs")).expect("remove Rust source");
    fs::create_dir_all(repo.join("App")).expect("create Swift directory");
    fs::write(
        repo.join("App/Caller.swift"),
        "func useOld() { let _ = Record.self }\n",
    )
    .expect("write Swift source");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "core/src/lib.rs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed_dead.iter().any(|s| s.name == "Record"),
        "document link must not block: {api:?}"
    );

    std::os::unix::fs::symlink("missing.rs", repo.join("ffi.rs")).expect("create source link");
    assert!(
        Command::new("git")
            .args(["add", "ffi.rs"])
            .current_dir(repo)
            .status()
            .expect("stage source link")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "add source link"])
            .current_dir(repo)
            .status()
            .expect("commit source link")
            .success()
    );
    let blocked = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        blocked.removed.iter().any(|s| s.name == "Record"),
        "source link must remain blocking: {blocked:?}"
    );
}

/// --dir がサブディレクトリでも、旧リポジトリ外側の bridge を見逃さない。
/// UDL と manifest だけの証拠、証拠無しの対照も同じ入力で検証する。
#[test]
fn detect_api_changes_rust_swift_subdirectory_checks_repository_wide_ffi() {
    for bridge in ["none", "rust", "udl", "manifest"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let deleted_src = "pub struct Record { pub value: i32 }\n";
        let mut files = vec![
            (
                "apps/core/Cargo.toml",
                "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("apps/core/src/lib.rs", deleted_src),
        ];
        match bridge {
            "rust" => files.push((
                "ffi/src/lib.rs",
                "#[no_mangle]\npub extern \"C\" fn record_new() {}\n",
            )),
            "udl" => files.push(("ffi/api.udl", "namespace records {};\n")),
            "manifest" => files.push(("ffi/Cargo.toml", "[dependencies]\nuniffi = \"0.1\"\n")),
            _ => {}
        }
        git_commit_files(repo, &files, "base");
        fs::remove_file(repo.join("apps/core/src/lib.rs")).expect("remove Rust source");
        fs::create_dir_all(repo.join("apps/App")).expect("create Swift directory");
        fs::write(
            repo.join("apps/App/Caller.swift"),
            "func useOld() { let _ = Record.self }\n",
        )
        .expect("write Swift source");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "core/src/lib.rs".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![],
            deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
        }];
        let api = detect_api_changes(
            repo.join("apps").to_str().expect("utf-8 path"),
            "HEAD",
            &diff_files,
        );
        assert_eq!(
            api.removed.iter().any(|s| s.name == "Record"),
            bridge != "none",
            "bridge={bridge}: {api:?}"
        );
        assert_eq!(
            api.removed_dead.iter().any(|s| s.name == "Record"),
            bridge == "none",
            "bridge={bridge}: {api:?}"
        );
    }
}

/// GitLab #33: PHP メソッドへの Eloquent リレーション戻り型付与 (`monitorLogs()` →
/// `monitorLogs(): HasOne`) は removed ではなく modified。Laravel entrypoint 除外が
/// API 差分経路 (exclude_framework_entrypoints=false) に効いて新側だけ除外され、
/// 実在メソッドが api.rm に誤分類されていた。
#[test]
fn detect_api_changes_php_eloquent_relation_return_type_added_is_modified_not_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/Models/VoiceLogSummaryEloquent.php",
                "<?php\n\nclass VoiceLogSummaryEloquent extends AbstractEloquent {\n    public function monitorLogs() {\n        return $this->hasMany(MonitorLogEloquent::class, 'request_id', 'request_id');\n    }\n}\n",
            ),
            (
                "src/Repositories/VoiceLogSummaryRepositoryQuery.php",
                "<?php\n\nclass VoiceLogSummaryRepositoryQuery {\n    public function fetch($eloquent) {\n        $monitorLog = $eloquent->monitorLogs;\n        return $monitorLog;\n    }\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
        repo.join("src/Models/VoiceLogSummaryEloquent.php"),
        "<?php\n\nuse Illuminate\\Database\\Eloquent\\Relations\\HasOne;\n\nclass VoiceLogSummaryEloquent extends AbstractEloquent {\n    public function monitorLogs(): HasOne {\n        return $this->hasOne(MonitorLogEloquent::class, 'request_id', 'request_id');\n    }\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/Models/VoiceLogSummaryEloquent.php".to_string(),
        new_path: "src/Models/VoiceLogSummaryEloquent.php".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 7,
            new_start: 1,
            new_count: 9,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        !api.removed
            .iter()
            .chain(api.removed_dead.iter())
            .any(|s| s.name.ends_with("monitorLogs")),
        "実在メソッドの返り型付与を removed/removed_dead に分類しない。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        api.modified
            .iter()
            .any(|m| m.name == "VoiceLogSummaryEloquent.monitorLogs"),
        "返り型付与はシグネチャ変更として modified に分類する。modified={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}

/// GitLab #33 の裏面: Eloquent リレーションメソッドの実削除は api.rm として検出する
/// (旧実装は old 側抽出でも entrypoint 除外され silent false negative だった)。
#[test]
fn detect_api_changes_php_eloquent_relation_removed_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/Models/VoiceLogSummaryEloquent.php",
                "<?php\n\nuse Illuminate\\Database\\Eloquent\\Relations\\HasOne;\n\nclass VoiceLogSummaryEloquent extends AbstractEloquent {\n    public function monitorLogs(): HasOne {\n        return $this->hasOne(MonitorLogEloquent::class, 'request_id', 'request_id');\n    }\n\n    public function keepMe() {\n        return 1;\n    }\n}\n",
            ),
            (
                "src/Repositories/VoiceLogSummaryRepositoryQuery.php",
                "<?php\n\nclass VoiceLogSummaryRepositoryQuery {\n    public function fetch($eloquent) {\n        $monitorLog = $eloquent->monitorLogs;\n        return $monitorLog;\n    }\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
        repo.join("src/Models/VoiceLogSummaryEloquent.php"),
        "<?php\n\nclass VoiceLogSummaryEloquent extends AbstractEloquent {\n    public function keepMe() {\n        return 1;\n    }\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/Models/VoiceLogSummaryEloquent.php".to_string(),
        new_path: "src/Models/VoiceLogSummaryEloquent.php".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 13,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed
            .iter()
            .any(|s| s.name == "VoiceLogSummaryEloquent.monitorLogs"),
        "参照が残る Eloquent リレーションメソッドの削除は removed として報告する。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// Issue 2026-07-15-ts-add-refactor-delete-chain-api-rm-fp: クラスをファイルごと削除し
/// 呼び出し側を別クラスへ切替えた diff で、owner クラス (`GwsCalendarClient`) は参照 0 件で
/// removed_dead (informational) になるのに、メソッド (`GwsCalendarClient.listEvents`) は
/// bare name カウントが切替先クラスの同名メソッド参照を拾って removed (blocking) に残って
/// いた。owner 型が removed_dead なら member も追従して removed_dead へ移す。
#[test]
fn detect_api_changes_deleted_class_member_follows_dead_owner_to_removed_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let gws_src = "export class GwsCalendarClient {\n    async listEvents(day: string): Promise<string[]> {\n        return [day];\n    }\n}\n";
    git_commit_files(
        repo,
        &[
            ("src/services/gwsCalendar.ts", gws_src),
            (
                "src/services/googleCalendar.ts",
                "export class GoogleCalendarClient {\n    async listEvents(day: string): Promise<string[]> {\n        return [\"g:\" + day];\n    }\n}\n",
            ),
            (
                "src/index.ts",
                "import { GwsCalendarClient } from './services/gwsCalendar';\n\nexport async function main() {\n    const client = new GwsCalendarClient();\n    return client.listEvents(\"2026-07-15\");\n}\n",
            ),
        ],
        "base",
    );
    // gws 実装をファイルごと削除し、呼び出し側は google 実装へ切替
    std::fs::remove_file(repo.join("src/services/gwsCalendar.ts")).expect("rm");
    fs::write(
        repo.join("src/index.ts"),
        "import { GoogleCalendarClient } from './services/googleCalendar';\n\nexport async function main() {\n    const client = new GoogleCalendarClient();\n    return client.listEvents(\"2026-07-15\");\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/services/gwsCalendar.ts".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 5,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: Some(gws_src.as_bytes().to_vec()),
        },
        crate::models::impact::DiffFile {
            old_path: "src/index.ts".to_string(),
            new_path: "src/index.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 6,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed_dead
            .iter()
            .any(|s| s.name == "GwsCalendarClient.listEvents"),
        "owner クラスが removed_dead なら member も removed_dead に追従すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        !api.removed
            .iter()
            .any(|s| s.name == "GwsCalendarClient.listEvents"),
        "member を blocking な removed に残さない。removed={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// 負ケース: owner クラス名への参照が新ツリーに残っている (owner が removed_kept) 場合、
/// member は従来どおり removed (blocking) に残す — owner 経由の到達経路が残り得るため。
#[test]
fn detect_api_changes_deleted_member_with_live_owner_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let gws_src = "export class GwsCalendarClient {\n    async listEvents(day: string): Promise<string[]> {\n        return [day];\n    }\n}\n";
    git_commit_files(
        repo,
        &[
            ("src/services/gwsCalendar.ts", gws_src),
            (
                "src/index.ts",
                "import { GwsCalendarClient } from './services/gwsCalendar';\n\nexport async function main() {\n    const client = new GwsCalendarClient();\n    return client.listEvents(\"2026-07-15\");\n}\n",
            ),
        ],
        "base",
    );
    // クラスファイルだけ削除し、呼び出し側 (owner 名への参照) は残したまま
    std::fs::remove_file(repo.join("src/services/gwsCalendar.ts")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/services/gwsCalendar.ts".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(gws_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed
            .iter()
            .any(|s| s.name == "GwsCalendarClient.listEvents")
            && api.removed.iter().any(|s| s.name == "GwsCalendarClient"),
        "owner への参照が残る削除は owner / member とも blocking な removed を維持する。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// 負ケース (codex レビュー指摘): owner 型の別定義が新ツリーに残る (定義 1・参照 0) 場合、
/// owner は第 1 パスで removed_dead に入るが型は生存しているため、member を removed_dead へ
/// 降格してはならない。partial class / open class / extension を模した構成で、削除ファイルの
/// `Svc` と同名の `Svc` が別ファイルに残り、削除メソッド名 `doWork` は別コードから参照される。
#[test]
fn detect_api_changes_deleted_member_with_surviving_owner_definition_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let gone_src = "export class Svc {\n    doWork(): void {}\n}\n";
    git_commit_files(
        repo,
        &[
            ("src/gone.ts", gone_src),
            // 同名 Svc の別定義 (新ツリーに残る = owner の def_count を 1 に押し上げる)。
            // Svc 名は誰からも参照されないため ref_count は 0。
            (
                "src/keep.ts",
                "export class Svc {\n    other(): void {}\n}\n",
            ),
            // 削除される member 名 `doWork` への参照だけを残す (owner Svc は参照しない)。
            (
                "src/consumer.ts",
                "export function run(r: any): void {\n    r.doWork();\n}\n",
            ),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("src/gone.ts")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/gone.ts".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(gone_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "Svc.doWork"),
        "owner 型の別定義が新ツリーに残る場合、member は blocking な removed を維持すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// Issue 2026-07-19-bulk-subsystem-removal: 削除された bash 関数と同名のローカル関数が
/// 複数の残存スクリプトに定義され、参照がすべて各定義ファイル内で閉じている場合、bare
/// name カウントは def_count > 1 + ref_count > 0 で従来 blocking に残していた。参照の
/// 帰属確認 (同ファイル定義 = 削除ファイルが消えても未定義にならない) により
/// removed_dead (informational) へ降格する。
#[test]
fn detect_api_changes_bulk_removal_bash_same_name_local_functions_demoted_to_removed_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "#!/bin/bash\nusage() {\n  echo \"usage: deleted-tool\"\n}\nusage\n";
    git_commit_files(
        repo,
        &[
            ("scripts/deleted-tool.sh", deleted_src),
            (
                "scripts/keep-a.sh",
                "#!/bin/bash\nusage() {\n  echo \"usage: keep-a\"\n}\nusage\n",
            ),
            (
                "scripts/keep-b.sh",
                "#!/bin/bash\nusage() {\n  echo \"usage: keep-b\"\n}\nif [ -z \"$1\" ]; then usage >&2; fi\n",
            ),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("scripts/deleted-tool.sh")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "scripts/deleted-tool.sh".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed_dead.iter().any(|s| s.name == "usage"),
        "同名ローカル関数へ帰属確認できた削除は removed_dead に降格すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        !api.removed.iter().any(|s| s.name == "usage"),
        "blocking な removed に残さない。removed={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// Issue 2026-07-19-bulk-subsystem-removal: 削除された mjs export と同名の独立シンボルが
/// 残存し、参照が残存側への相対 import で束縛されている場合、bare name カウントは
/// 「削除シンボルへの残存参照」と誤認して blocking に残していた。import specifier の
/// 相対解決で残存定義ファイルへの帰属を証明し removed_dead へ降格する。
#[test]
fn detect_api_changes_bulk_removal_import_attributed_to_survivor_demoted_to_removed_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src =
        "export function loadEnvFiles(dir) {\n  return { dir };\n}\nloadEnvFiles(\".\");\n";
    git_commit_files(
        repo,
        &[
            ("plugins/setup.mjs", deleted_src),
            (
                "api/src/config.ts",
                "export function loadEnvFiles(baseDir = \".\", env = {}) {\n  return { baseDir, env };\n}\n",
            ),
            (
                "api/test/config.test.ts",
                "import { loadEnvFiles } from \"../src/config\";\n\nexport function testConfig() {\n  return loadEnvFiles(\".\", {});\n}\n",
            ),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("plugins/setup.mjs")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "plugins/setup.mjs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed_dead.iter().any(|s| s.name == "loadEnvFiles"),
        "残存定義への import で帰属確認できた削除は removed_dead に降格すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        !api.removed.iter().any(|s| s.name == "loadEnvFiles"),
        "blocking な removed に残さない。removed={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// Issue 2026-08-06-api-rm-atomic-module-deletion: Python モジュールを呼び出し元ごと
/// アトミックに削除しても、polyglot リポジトリでは無関係な他言語の同名シンボル
/// (PHP / JS / C の `search`) への参照が bare name カウントに混入し、blocking な
/// api.rm に残っていた。参照ファイルの言語から削除ファイルの言語へ識別子束縛の経路が
/// 無いことを証明し、残る同一言語の参照も属性アクセス (`re.search`) と判れば
/// removed_dead (informational) へ降格する。
#[test]
fn detect_api_changes_atomic_python_module_deletion_demoted_to_removed_dead() {
    let (removed, removed_dead) = removed_names_after_atomic_python_module_deletion(
        "import re\n\ndef check(pw):\n    return re.search(\"x\", pw)\n",
    );
    assert!(
        removed_dead.iter().any(|n| n == "search"),
        "他言語の同名参照と stdlib 属性アクセスしか残らない削除は removed_dead へ降格すべき。removed={removed:?} removed_dead={removed_dead:?}"
    );
    assert!(
        !removed.iter().any(|n| n == "search"),
        "blocking な removed に残さない。removed={removed:?}"
    );
}

/// 上の降格は fail-closed を保つ。同一言語 (Python) から削除モジュールへ到達する参照が
/// 残っていれば、他言語ノイズが同居していても blocking な api.rm に残す。
#[test]
fn detect_api_changes_atomic_python_module_deletion_keeps_blocking_on_residual_python_ref() {
    // モジュール修飾呼び出し: レシーバ `core` が削除モジュール名と一致する
    let (removed, removed_dead) = removed_names_after_atomic_python_module_deletion(
        "import core\n\ndef check(pw):\n    return core.search(pw)\n",
    );
    assert!(
        removed.iter().any(|n| n == "search"),
        "削除モジュールを修飾した属性アクセスが残る場合は blocking を維持すべき。removed={removed:?} removed_dead={removed_dead:?}"
    );

    // bare 呼び出し: 属性アクセスではないので証明できない
    let (removed, removed_dead) = removed_names_after_atomic_python_module_deletion(
        "from core import search\n\ndef check(pw):\n    return search(pw)\n",
    );
    assert!(
        removed.iter().any(|n| n == "search"),
        "削除モジュールからの import + bare 呼び出しが残る場合は blocking を維持すべき。removed={removed:?} removed_dead={removed_dead:?}"
    );
}

/// Python のモジュールを辞書や変数に入れて呼ぶ (`BACKENDS[name].search(q)` /
/// `backend.search(q)`) 利用は、レシーバの名前から削除モジュールかどうかを決められないので、
/// 削除した `search` を removed_dead に降格しない。import した名前で直接呼ぶ利用は従来どおり
/// blocking、stdlib の属性アクセスだけなら従来どおり降格する (対照)。
#[test]
fn detect_api_changes_python_module_held_in_container_or_variable_stays_removed() {
    for (router, expect_blocking) in [
        (
            "from pkg import core, fast\nBACKENDS = {\"core\": core, \"fast\": fast}\n\ndef run(name, q):\n    return BACKENDS[name].search(q)\n",
            true,
        ),
        (
            "from pkg import core\nbackend = core\n\ndef run(q):\n    return backend.search(q)\n",
            true,
        ),
        (
            "from pkg import core\n\ndef run(q):\n    return core.search(q)\n",
            true,
        ),
        (
            "import re\n\ndef run(q):\n    return re.search(\"x\", q)\n",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("pkg/__init__.py", ""),
                (
                    "pkg/core.py",
                    "def search(q):\n    return [q]\n\ndef index(items):\n    return items\n",
                ),
                ("pkg/fast.py", "def other(q):\n    return q\n"),
                ("pkg/router.py", router),
            ],
            "base",
        );
        fs::write(
            repo.join("pkg/core.py"),
            "def index(items):\n    return items\n",
        )
        .expect("write");
        let api = detect_api_changes_from_worktree(repo);
        let blocking = api.removed.iter().any(|s| s.name == "search");
        let informational = api.removed_dead.iter().any(|s| s.name == "search");
        assert_eq!(
            (blocking, informational),
            (expect_blocking, !expect_blocking),
            "{router}\nremoved={:?} removed_dead={:?}",
            api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
            api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }
}

/// `pkg/util.py` の `helper` と、それを再エクスポートする `pkg/__init__.py` の
/// `from .util import helper` を同時に削除し、`app_py` を残したときの (removed, removed_dead)。
fn removed_names_after_python_package_reexport_deletion(
    app_py: &str,
) -> (Vec<String>, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "def helper():\n    return 1\n";
    git_commit_files(
        repo,
        &[
            ("pkg/util.py", deleted_src),
            ("pkg/__init__.py", "from .util import helper\n"),
            ("app.py", app_py),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("pkg/util.py")).expect("rm");
    fs::write(repo.join("pkg/__init__.py"), "").expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "pkg/util.py".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
        },
        crate::models::impact::DiffFile {
            old_path: "pkg/__init__.py".to_string(),
            new_path: "pkg/__init__.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    (
        api.removed.iter().map(|s| s.name.clone()).collect(),
        api.removed_dead.iter().map(|s| s.name.clone()).collect(),
    )
}

/// パッケージの `__init__.py` が再エクスポートしていた関数は `pkg.helper()` の形でも
/// 呼べる。レシーバが祖先パッケージ名なら削除シンボルへの残存参照として blocking を維持する
/// (旧実装はレシーバに削除モジュール名 `util` が無いことだけで別物と証明し、実行時に
/// AttributeError になる削除を removed_dead へ降格していた)。
#[test]
fn detect_api_changes_python_package_reexport_attribute_access_stays_removed() {
    for app_py in [
        "import pkg\n\n\ndef run():\n    return pkg.helper()\n",
        "import pkg as p\n\n\ndef run():\n    return p.helper()\n",
    ] {
        let (removed, removed_dead) = removed_names_after_python_package_reexport_deletion(app_py);
        assert!(
            removed.iter().any(|n| n == "helper"),
            "祖先パッケージ経由の属性アクセスが残る削除は blocking を維持すべき。app.py={app_py:?} removed={removed:?} removed_dead={removed_dead:?}"
        );
    }

    // 対照: 削除モジュール名を修飾した呼び出しは従来どおり blocking
    let (removed, removed_dead) = removed_names_after_python_package_reexport_deletion(
        "from pkg import util\n\n\ndef run():\n    return util.helper()\n",
    );
    assert!(
        removed.iter().any(|n| n == "helper"),
        "削除モジュールを修飾した属性アクセスは blocking を維持すべき。removed={removed:?} removed_dead={removed_dead:?}"
    );

    // 対照: 削除モジュールとも祖先パッケージとも無関係なレシーバは従来どおり降格する
    let (removed, removed_dead) = removed_names_after_python_package_reexport_deletion(
        "import other\n\n\ndef run():\n    return other.helper()\n",
    );
    assert!(
        removed_dead.iter().any(|n| n == "helper") && !removed.iter().any(|n| n == "helper"),
        "無関係なレシーバの属性アクセスしか残らない削除は removed_dead へ降格すべき。removed={removed:?} removed_dead={removed_dead:?}"
    );
}

/// 負ケース: 参照ファイルの import specifier が削除ファイル自身に解決される場合は、
/// 同名の残存定義があっても破壊的削除として blocking な removed を維持する。
#[test]
fn detect_api_changes_removed_function_imported_from_deleted_file_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "export function doWork() {\n  return 1;\n}\n";
    git_commit_files(
        repo,
        &[
            ("src/deleted.ts", deleted_src),
            (
                "src/keep.ts",
                "export function doWork() {\n  return 2;\n}\n",
            ),
            (
                "src/caller.ts",
                "import { doWork } from \"./deleted\";\n\nexport function run() {\n  return doWork();\n}\n",
            ),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("src/deleted.ts")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/deleted.ts".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "doWork"),
        "削除ファイルへの import が残る削除は blocking な removed を維持すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// 負ケース: 参照スクリプト自身に同名関数が定義されていても、リテラル `source` が
/// 削除ファイルを指している (削除実装への明示依存が残る) 場合は blocking を維持する。
#[test]
fn detect_api_changes_bash_literal_source_of_deleted_file_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "#!/bin/bash\nhelper() {\n  echo \"deleted helper\"\n}\n";
    git_commit_files(
        repo,
        &[
            ("src/deleted-lib.sh", deleted_src),
            (
                "src/runner.sh",
                "#!/bin/bash\nhelper() {\n  echo \"local fallback\"\n}\nsource ./deleted-lib.sh\nhelper\n",
            ),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("src/deleted-lib.sh")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/deleted-lib.sh".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "helper"),
        "削除ファイルをリテラル source する参照が残る削除は blocking を維持すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[test]
fn detect_api_changes_skips_removed_when_no_old_source_available() {
    // `git show base:old_path` が失敗し、かつ deleted_old_source も無い場合は
    // 従来通り何も報告しない (false positive を出さない)。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("README.md", "# repo\n")], "initial");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/old.py".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api_changes.removed.is_empty(),
        "旧ソース取得不能時は removed に出すべきではない"
    );
}

#[test]
fn detect_api_changes_python_property_to_field_replacement_is_not_removed() {
    // 報告再現: Python の `@property def x(self) -> str` を `@dataclass` フィールド
    // `x: str` に置き換えると、`obj.x` 属性アクセス API は維持されるため
    // `api.rm` ではなく `property_to_field` カテゴリに分類されるべき。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let old_content = "\
from dataclasses import dataclass
from urllib.parse import urlparse


@dataclass
class ReviewConfig:
    project_url: str

    @property
    def gitlab_base_url(self) -> str:
        parsed = urlparse(self.project_url)
        return f\"{parsed.scheme}://{parsed.netloc}\"
";
    git_commit_files(repo, &[("scripts/review_mr.py", old_content)], "initial");

    let new_content = "\
from dataclasses import dataclass


@dataclass
class ReviewConfig:
    project_url: str
    gitlab_base_url: str
";
    fs::write(repo.join("scripts/review_mr.py"), new_content).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "scripts/review_mr.py".to_string(),
        new_path: "scripts/review_mr.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 12,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: std::collections::HashSet<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed_names.contains(&"ReviewConfig.gitlab_base_url"),
        "@property → dataclass field 置き換えは api.rm に残らないべき。got: {removed_names:?}"
    );

    let p2f_names: Vec<&str> = api_changes
        .property_to_field
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert!(
        p2f_names.contains(&"ReviewConfig.gitlab_base_url"),
        "@property → dataclass field 置き換えは property_to_field に積まれるべき。got: {p2f_names:?}"
    );
}

#[test]
fn detect_api_changes_python_property_removed_without_field_remains_removed() {
    // 安全網: クラスから @property を削除し、対応するフィールドも追加しない場合は
    // 通常通り api.rm として残るべき。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let old_content = "\
from dataclasses import dataclass


@dataclass
class Foo:
    name: str

    @property
    def computed(self) -> str:
        return self.name.upper()
";
    git_commit_files(repo, &[("foo.py", old_content)], "initial");

    let new_content = "\
from dataclasses import dataclass


@dataclass
class Foo:
    name: str
";
    fs::write(repo.join("foo.py"), new_content).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "foo.py".to_string(),
        new_path: "foo.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 10,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: std::collections::HashSet<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed_names.contains(&"Foo.computed"),
        "対応 field が無い @property 削除は api.rm に残るべき。got: {removed_names:?}"
    );
    assert!(
        api_changes.property_to_field.is_empty(),
        "対応 field が無い場合は property_to_field に積まれないべき。got: {:?}",
        api_changes.property_to_field
    );
}

/// `models.py` の `User` を `before` から `after` に書き換え (`extra` のファイルも同時に更新) た
/// ときの API 差分。呼び出し側 `app.py` は変更しない。
fn python_user_member_replacement(
    before: &str,
    after: &str,
    extra: &[(&str, &str, &str)],
) -> ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut base_files = vec![
        ("models.py", before),
        (
            "app.py",
            "from models import User\n\n\ndef show(u: User):\n    return u.name()\n",
        ),
    ];
    base_files.extend(extra.iter().map(|(path, old, _)| (*path, *old)));
    git_commit_files(repo, &base_files, "base");
    let mut diff_files = Vec::new();
    for (path, content) in std::iter::once(("models.py", after))
        .chain(extra.iter().map(|(path, _, new)| (*path, *new)))
    {
        fs::write(repo.join(path), content).expect("write");
        diff_files.push(crate::models::impact::DiffFile {
            old_path: path.to_string(),
            new_path: path.to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 20,
                new_start: 1,
                new_count: 20,
            }],
            deleted_old_source: None,
        });
    }
    detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files)
}

/// 素のメソッドをフィールドに置き換えると `u.name()` は TypeError になる。property ではない
/// 定義の置き換えは property_to_field (informational) に降格させず、blocking な removed に残す。
#[test]
fn detect_api_changes_python_method_replaced_by_field_stays_removed() {
    let after = "from dataclasses import dataclass\n\n\n@dataclass\nclass User:\n    first: str\n    name: str\n";
    let api = python_user_member_replacement(
        "from dataclasses import dataclass\n\n\n@dataclass\nclass User:\n    first: str\n\n    def name(self) -> str:\n        return self.first\n",
        after,
        &[],
    );
    assert!(
        api.removed.iter().any(|s| s.name == "User.name") && api.property_to_field.is_empty(),
        "素のメソッド → フィールドは removed に残すべき。removed={:?} property_to_field={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.property_to_field
    );

    // 対照: setter 付き property / functools.cached_property の置き換えは従来どおり降格する
    let api = python_user_member_replacement(
        "import functools\n\n\nclass User:\n    @property\n    def name(self) -> str:\n        return self._name\n\n    @name.setter\n    def name(self, value: str) -> None:\n        self._name = value\n\n    @functools.cached_property\n    def slug(self) -> str:\n        return self._name.lower()\n",
        "from dataclasses import dataclass\n\n\n@dataclass\nclass User:\n    name: str\n    slug: str\n",
        &[],
    );
    let p2f: HashSet<&str> = api
        .property_to_field
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert!(
        p2f.contains("User.name") && p2f.contains("User.slug"),
        "property の置き換えは property_to_field に積むべき。property_to_field={p2f:?} removed={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        !api.removed
            .iter()
            .any(|s| s.name == "User.name" || s.name == "User.slug"),
        "property の置き換えを removed に残さない。removed={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// property を削除し、別ファイルの同名クラスに同名フィールドを足しても置き換えの根拠に
/// ならない。変更後の同じファイルの同じクラスに現れたフィールドだけを置き換え先とみなす。
#[test]
fn detect_api_changes_python_property_removed_with_field_in_other_file_stays_removed() {
    let before = "class User:\n    def __init__(self, first):\n        self.first = first\n\n    @property\n    def name(self) -> str:\n        return self.first\n";
    let api = python_user_member_replacement(
        before,
        "class User:\n    def __init__(self, first):\n        self.first = first\n",
        &[(
            "other.py",
            "class User:\n    age: int\n",
            "class User:\n    age: int\n    name: str\n",
        )],
    );
    assert!(
        api.removed.iter().any(|s| s.name == "User.name") && api.property_to_field.is_empty(),
        "別ファイルの同名クラスへのフィールド追加で property 削除を降格しない。removed={:?} property_to_field={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.property_to_field
    );

    // 対照: 同じファイルの同じクラスにフィールドを置けば従来どおり降格する
    let api = python_user_member_replacement(
        before,
        "class User:\n    name: str\n\n    def __init__(self, first):\n        self.first = first\n",
        &[],
    );
    assert!(
        api.property_to_field
            .iter()
            .any(|p| p.name == "User.name" && p.file == "models.py"),
        "同じファイルの property → field は property_to_field に積むべき。property_to_field={:?}",
        api.property_to_field
    );
}

/// 他ファイルから参照されていない exported シンボルを削除した場合、
/// `removed` ではなく `removed_dead` カテゴリに振り分けられること
/// (Issue 2026-05-28-meet-virtual-you-gemini-multi-select 対応)。
/// HEAD ツリーで参照 0 件 = repo 内 dead removal を informational として提示。
#[test]
fn detect_api_changes_unreferenced_removal_goes_to_removed_dead_not_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // foo / bar 両方を定義。caller なし (dead-code 想定)。
    git_commit_files(
        repo,
        &[("mod.py", "def foo():\n    pass\n\ndef bar():\n    pass\n")],
        "initial",
    );
    // bar を削除 (HEAD で bar への参照は 0 件)
    fs::write(repo.join("mod.py"), "def foo():\n    pass\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "mod.py".to_string(),
        new_path: "mod.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_dead_names: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_names: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed_dead_names.contains(&"bar"),
        "HEAD で参照 0 件の削除は removed_dead に振り分けられるべき。got removed_dead: {removed_dead_names:?}, removed: {removed_names:?}"
    );
    assert!(
        !removed_names.contains(&"bar"),
        "removed_dead に振り分けられた symbol は removed には残ってはならない。got: {removed_names:?}"
    );
}

#[test]
fn detect_api_changes_unreferenced_method_removal_survives_unrelated_addition() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "shop/catalog.py",
                "class Catalog:\n    def total(self, skus: list[str]) -> int:\n        return len(skus)\n\n    def legacy_total(self, skus: list[str]) -> int:\n        return len(skus) * 2\n",
            ),
            (
                "shop/report.py",
                "from shop.catalog import Catalog\n\ndef summary(catalog: Catalog) -> str:\n    return str(catalog.total(['a']))\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("shop/catalog.py"),
        "class Catalog:\n    def total(self, skus: list[str]) -> int:\n        return len(skus)\n\n    def describe(self) -> str:\n        return 'catalog'\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "shop/catalog.py".to_string(),
        new_path: "shop/catalog.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 6,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];
    let changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        changes
            .removed_dead
            .iter()
            .any(|symbol| symbol.name == "Catalog.legacy_total"),
        "unrelated method addition must not hide an already unused removal: {changes:?}"
    );
    assert!(
        !changes
            .removed
            .iter()
            .any(|symbol| symbol.name == "Catalog.legacy_total"),
        "the unused removal should remain informational: {changes:?}"
    );
}

#[test]
fn detect_api_changes_used_method_rename_remains_closed_in_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "shop/catalog.py",
            "class Catalog:\n    def old_total(self, skus: list[str]) -> int:\n        return len(skus)\n\ndef summary(catalog: Catalog) -> int:\n    return catalog.old_total(['a'])\n",
        )],
        "initial",
    );
    fs::write(
        repo.join("shop/catalog.py"),
        "class Catalog:\n    def new_total(self, skus: list[str]) -> int:\n        return len(skus)\n\ndef summary(catalog: Catalog) -> int:\n    return catalog.new_total(['a'])\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "shop/catalog.py".to_string(),
        new_path: "shop/catalog.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 6,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];
    let changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        changes
            .removed
            .iter()
            .chain(changes.removed_dead.iter())
            .all(|symbol| symbol.name != "Catalog.old_total"),
        "a used method renamed with its caller is closed in the diff: {changes:?}"
    );
}

#[test]
fn detect_api_changes_cross_file_method_rename_remains_closed_in_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "shop/catalog.py",
                "class Catalog:\n    def old_total(self) -> int:\n        return 1\n",
            ),
            (
                "shop/report.py",
                "from shop.catalog import Catalog\n\ndef summary(catalog: Catalog) -> int:\n    return catalog.old_total()\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("shop/catalog.py"),
        "class Catalog:\n    def new_total(self) -> int:\n        return 1\n",
    )
    .expect("write catalog");
    fs::write(
        repo.join("shop/report.py"),
        "from shop.catalog import Catalog\n\ndef summary(catalog: Catalog) -> int:\n    return catalog.new_total()\n",
    )
    .expect("write report");
    let diff_files = ["shop/catalog.py", "shop/report.py"]
        .iter()
        .map(|path| crate::models::impact::DiffFile {
            old_path: (*path).to_string(),
            new_path: (*path).to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        })
        .collect::<Vec<_>>();
    let changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        changes
            .removed
            .iter()
            .chain(changes.removed_dead.iter())
            .all(|symbol| symbol.name != "Catalog.old_total"),
        "a cross-file caller updated with a method rename is closed in the diff: {changes:?}"
    );
}

#[test]
fn detect_api_changes_removed_callers_do_not_prove_method_rename() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "shop/catalog.py",
                "class Catalog:\n    def legacy_total(self, n: int) -> int:\n        return 0 if n == 0 else self.legacy_total(n - 1)\n",
            ),
            (
                "shop/report.py",
                "from shop.catalog import Catalog\n\ndef summary(catalog: Catalog) -> str:\n    return 'old'\n",
            ),
            (
                "tests/test_catalog.py",
                "from shop.catalog import Catalog\n\ndef test_legacy():\n    assert Catalog().legacy_total(1) == 0\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("shop/catalog.py"),
        "class Catalog:\n    def describe(self) -> str:\n        return 'catalog'\n",
    )
    .expect("write catalog");
    fs::write(
        repo.join("shop/report.py"),
        "from shop.catalog import Catalog\n\ndef summary(catalog: Catalog) -> str:\n    return catalog.describe()\n",
    )
    .expect("write report");
    fs::remove_file(repo.join("tests/test_catalog.py")).expect("remove test");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "shop/catalog.py".to_string(),
            new_path: "shop/catalog.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "shop/report.py".to_string(),
            new_path: "shop/report.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "tests/test_catalog.py".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![],
            deleted_old_source: None,
        },
    ];
    let changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        changes
            .removed_dead
            .iter()
            .any(|symbol| symbol.name == "Catalog.legacy_total"),
        "self-recursion and deleted tests cannot prove a rename: {changes:?}"
    );
}

/// HEAD ツリーで他ファイルから参照されているシンボル (alive) の削除は、
/// `removed_dead` ではなく `removed` に残ること (副作用回帰防止)。
/// 「破壊的削除」と「dead-code 整理」の区別が機能していることを確認。
#[test]
fn detect_api_changes_referenced_removal_stays_in_removed_not_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // foo / bar を定義。caller.py で bar を参照 (alive)。
    git_commit_files(
        repo,
        &[
            ("mod.py", "def foo():\n    pass\n\ndef bar():\n    pass\n"),
            ("caller.py", "from mod import bar\nbar()\n"),
        ],
        "initial",
    );
    // bar を削除 (caller.py はそのままで bar への参照を維持 = 破壊的削除)
    fs::write(repo.join("mod.py"), "def foo():\n    pass\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "mod.py".to_string(),
        new_path: "mod.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_dead_names: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed_names.contains(&"bar"),
        "HEAD で参照ありのシンボル削除は removed (破壊的削除) に残るべき。got removed: {removed_names:?}, removed_dead: {removed_dead_names:?}"
    );
    assert!(
        !removed_dead_names.contains(&"bar"),
        "参照ありの削除は removed_dead に振り分けてはならない。got: {removed_dead_names:?}"
    );
}

/// 削除した interface `Config` の唯一の HEAD 参照が外部パッケージ (tailwindcss) の同名
/// import 由来なら、別モジュールの型として参照カウントから除外し api.rm ではなく
/// api.rm_dead に振り分ける。(レポート 2026-06-03-extension-task-only-cleanup の再現)
#[test]
fn detect_api_changes_removed_symbol_with_external_import_same_name_is_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "package.json",
                "{\n  \"devDependencies\": { \"tailwindcss\": \"^3.4.0\" }\n}\n",
            ),
            (
                "lib/config.ts",
                "export interface Config {\n  url: string;\n}\nexport function getConfig(): Config {\n  return { url: '' };\n}\n",
            ),
            (
                "tailwind.config.ts",
                "import type { Config } from \"tailwindcss\";\nexport default {} satisfies Config;\n",
            ),
        ],
        "initial",
    );
    // lib/config.ts から Config / getConfig を削除 (tailwind.config.ts は無関係な別 Config)
    fs::write(repo.join("lib/config.ts"), "export const VERSION = '1';\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib/config.ts".to_string(),
        new_path: "lib/config.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 6,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_dead: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.contains(&"Config"),
        "外部 import (tailwindcss) の同名 Config は参照に数えず、Config は api.rm に出ない。got removed: {removed:?}"
    );
    assert!(
        removed_dead.contains(&"Config"),
        "Config は removed_dead に振り分けられるべき。got removed_dead: {removed_dead:?}"
    );
}

/// 削除シンボルが内部 (相対 import) で実際に参照されている場合は、外部 import 除外の
/// 対象外で api.rm (破壊的削除) を維持する (false negative 防止)。
#[test]
fn detect_api_changes_removed_symbol_with_internal_relative_reference_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "package.json",
                "{\n  \"devDependencies\": { \"tailwindcss\": \"^3.4.0\" }\n}\n",
            ),
            (
                "lib/config.ts",
                "export interface Config {\n  url: string;\n}\n",
            ),
            (
                "app.ts",
                "import type { Config } from \"./lib/config\";\nexport const c: Config = { url: '' };\n",
            ),
        ],
        "initial",
    );
    // Config を削除するが app.ts は相対 import で参照を維持 (破壊的削除)
    fs::write(repo.join("lib/config.ts"), "export const X = 1;\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib/config.ts".to_string(),
        new_path: "lib/config.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.contains(&"Config"),
        "相対 import で内部参照される Config は api.rm を維持すべき。got removed: {removed:?}"
    );
}

/// 外部 alias import (`import { Config as TailwindConfig } from "tailwindcss"`、local は
/// TailwindConfig) と内部相対 import の同名 Config が同一ファイルに共存する場合、削除した
/// Config は内部参照が残るので api.rm を維持する (codex 指摘: 逆 alias false negative 防止)。
#[test]
fn detect_api_changes_removed_symbol_external_alias_with_internal_reference_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "package.json",
                "{\n  \"devDependencies\": { \"tailwindcss\": \"^3.4.0\" }\n}\n",
            ),
            (
                "lib/config.ts",
                "export interface Config {\n  url: string;\n}\n",
            ),
            (
                "app.ts",
                "import { Config as TailwindConfig } from \"tailwindcss\";\nimport type { Config } from \"./lib/config\";\nexport const c: Config = { url: '' };\nexport const t = {} as TailwindConfig;\n",
            ),
        ],
        "initial",
    );
    fs::write(repo.join("lib/config.ts"), "export const X = 1;\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib/config.ts".to_string(),
        new_path: "lib/config.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.contains(&"Config"),
        "外部 alias import (Config as TailwindConfig) があっても内部相対 import の Config 参照が残れば api.rm 維持。got removed: {removed:?}"
    );
}

/// 削除した内部 Config に実参照がなく、別ファイルに外部 alias import の import 元名
/// `Config` だけが残る場合 (`import { Config as TailwindConfig } from "tailwindcss"`、
/// Config 自体は未使用) は、import 元名を別モジュールの export として除外し removed_dead に
/// 振り分ける (codex 指摘: alias-only false positive 防止)。
#[test]
fn detect_api_changes_removed_symbol_external_alias_only_import_name_is_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "package.json",
                "{\n  \"devDependencies\": { \"tailwindcss\": \"^3.4.0\" }\n}\n",
            ),
            (
                "lib/config.ts",
                "export interface Config {\n  url: string;\n}\n",
            ),
            (
                "app.ts",
                "import { Config as TailwindConfig } from \"tailwindcss\";\nexport const t = {} as TailwindConfig;\n",
            ),
        ],
        "initial",
    );
    fs::write(repo.join("lib/config.ts"), "export const X = 1;\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib/config.ts".to_string(),
        new_path: "lib/config.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];
    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_dead: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.contains(&"Config"),
        "外部 alias import の import 元名のみの Config は api.rm に出ない。got removed: {removed:?}"
    );
    assert!(
        removed_dead.contains(&"Config"),
        "Config は removed_dead に振り分けられるべき。got removed_dead: {removed_dead:?}"
    );
}

/// detect_api_changes の早期 continue 経路 (closed-in-diff for api.rm) でも
/// qualname 対応が機能すること (codex 2 回目指摘への回帰防止)。
/// 「qualname method 削除 + 同ファイルに新規関数追加 + 外部 caller 残存」のケースで
/// removed_dead に誤分類されず removed に残る。
#[test]
fn detect_api_changes_qualname_method_with_inline_addition_and_external_caller_stays_in_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // 旧: Foo.bar あり、caller.py で Foo().bar() を参照
    git_commit_files(
        repo,
        &[
            (
                "foo.py",
                "class Foo:\n    def bar(self):\n        return 1\n",
            ),
            (
                "caller.py",
                "from foo import Foo\n\ndef use():\n    return Foo().bar()\n",
            ),
        ],
        "initial",
    );
    // 新: bar を削除し、同ファイルに新規関数 helper を追加
    // → new_symbols_in_current_file が空でないので closed-in-diff 早期 continue
    //   経路に入る (line 1836 周辺)
    fs::write(
        repo.join("foo.py"),
        "class Foo:\n    pass\n\n\ndef helper():\n    return 0\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "foo.py".to_string(),
        new_path: "foo.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_dead_names: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    // 早期 continue 経路でも bare name + def_count 判定が効く
    assert!(
        removed_names.iter().any(|n| n.contains("bar")),
        "qualname method 削除 + 同ファイル新規追加 + 外部 caller 残存は removed に残るべき。got removed: {removed_names:?}, removed_dead: {removed_dead_names:?}"
    );
    assert!(
        !removed_dead_names.iter().any(|n| n.contains("bar")),
        "上記ケースを removed_dead に振り分けてはならない。got: {removed_dead_names:?}"
    );
}

/// qualname (`Container.method`) 形式の class method 削除でも、別ファイルから
/// bare name で参照されていれば破壊的削除として `removed` に残ること
/// (codex 指摘 1: qualname 誤分類への回帰防止)。
#[test]
fn detect_api_changes_qualname_method_with_external_caller_stays_in_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // class Foo の method bar を削除するが、caller.py で Foo().bar() を呼んでいる
    git_commit_files(
        repo,
        &[
            (
                "foo.py",
                "class Foo:\n    def bar(self):\n        return 1\n",
            ),
            (
                "caller.py",
                "from foo import Foo\n\ndef use():\n    return Foo().bar()\n",
            ),
        ],
        "initial",
    );
    // method bar を削除
    fs::write(repo.join("foo.py"), "class Foo:\n    pass\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "foo.py".to_string(),
        new_path: "foo.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_dead_names: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    // bare name 'bar' で検索すると caller.py の Foo().bar() で参照あり
    // qualname を bare で正規化していなければ常に refs 0 件で removed_dead に
    // 誤分類される
    assert!(
        removed_names.iter().any(|n| n.contains("bar")),
        "外部 caller がいる qualname method 削除は removed に残るべき。got removed: {removed_names:?}, removed_dead: {removed_dead_names:?}"
    );
    assert!(
        !removed_dead_names.iter().any(|n| n.contains("bar")),
        "外部 caller がいる qualname method 削除を removed_dead に振り分けてはならない。got: {removed_dead_names:?}"
    );
}

#[test]
fn detect_api_changes_still_detects_genuine_removal() {
    // リネームではなく純粋に関数を削除した場合は api.rm が発報される。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(
        repo,
        &[("mod.py", "def foo():\n    pass\n\ndef bar():\n    pass\n")],
        "initial",
    );
    // bar を削除
    fs::write(repo.join("mod.py"), "def foo():\n    pass\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "mod.py".to_string(),
        new_path: "mod.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 2,
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
        removed.contains(&"bar"),
        "純粋な関数削除は api.rm として検出されるべき。got: {removed:?}"
    );
}

#[test]
fn detect_api_changes_cpp_h_header_inheritance_redefinition_is_modified_not_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(
        repo,
        &[(
            "error.h",
            "template <typename T> struct BaseError {};\n\
struct OmnisError {\n\
    void set_error(int code);\n\
    int code;\n\
};\n",
        )],
        "initial",
    );
    fs::write(
        repo.join("error.h"),
        "template <typename T> struct BaseError {};\n\
struct OmnisError : public BaseError<OmnisError> {};\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "error.h".to_string(),
        new_path: "error.h".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 2,
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
    let modified: Vec<&str> = api_changes
        .modified
        .iter()
        .chain(api_changes.modified_closed_in_diff.iter())
        .map(|s| s.name.as_str())
        .collect();

    assert!(
        !removed.contains(&"OmnisError"),
        ".h の C++ 継承付き再定義を api.rm にしてはならない。removed={removed:?}, modified={modified:?}"
    );
    assert!(
        modified.contains(&"OmnisError"),
        "継承付き再定義は削除ではなく変更として扱うべき。modified={modified:?}"
    );
}

/// Bash の未 export 関数を caller ごと同一 diff 内で削除した場合は api.rm に出さない。
/// (レポート 2026-05-01-bash-private-function-removal-flagged-as-api-rm.md の再現)
/// `dump_shallow_state` / `boundary_is_old_enough` のように、CLI スクリプト内の
/// クロージャ的なヘルパー関数を、同 diff 内で全 caller と一緒に削除したとき、
/// `export -f` が無いなら外部 API 面ではないため除外する必要がある。
#[test]
fn detect_api_changes_bash_pure_removal_without_export_is_not_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "#!/usr/bin/env bash\n\
dump_shallow_state() {\n    echo state\n}\n\n\
boundary_is_old_enough() {\n    return 0\n}\n\n\
main() {\n    dump_shallow_state\n    while ! boundary_is_old_enough; do\n        sleep 1\n    done\n}\nmain\n";
    git_commit_files(repo, &[("qa_diff.sh", before)], "initial");

    let after = "#!/usr/bin/env bash\n\
main() {\n    echo done\n}\nmain\n";
    fs::write(repo.join("qa_diff.sh"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "qa_diff.sh".to_string(),
        new_path: "qa_diff.sh".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 14,
            new_start: 1,
            new_count: 4,
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
        !removed.contains(&"dump_shallow_state"),
        "未 export な bash 関数を caller ごと同一 diff で削除した場合は api.rm に出してはならない。got: {removed:?}"
    );
    assert!(
        !removed.contains(&"boundary_is_old_enough"),
        "未 export な bash 関数を caller ごと同一 diff で削除した場合は api.rm に出してはならない。got: {removed:?}"
    );
}

/// 削除した bash ファイルの関数を、拡張子なしの bash スクリプト (`bin/deploy`、shebang で
/// bash と判定される) が `source` して呼んでいれば api.rm に残す。参照ファイルの言語を拡張子
/// だけで判定していたため「bash からは未参照」とみなされ、何も報告されなかった。
/// 拡張子なしでも shebang が別言語のファイルの同名参照は、従来どおり bash の参照に数えない (対照)。
#[test]
fn detect_api_changes_deleted_bash_function_used_by_extensionless_script_stays_removed() {
    for (script, content, expect_removed) in [
        (
            "bin/deploy",
            "#!/usr/bin/env bash\nsource \"$(dirname \"$0\")/../lib/common.sh\"\ngreet world\n",
            true,
        ),
        (
            "bin/deploy.sh",
            "#!/usr/bin/env bash\nsource \"$(dirname \"$0\")/../lib/common.sh\"\ngreet world\n",
            true,
        ),
        (
            "bin/tool",
            "#!/usr/bin/env python3\ndef greet(name):\n    print(name)\n\ngreet('world')\n",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("lib/common.sh", "greet() {\n  echo \"hello $1\"\n}\n"),
                (script, content),
            ],
            "initial",
        );
        git_in(repo, &["rm", "-q", "lib/common.sh"]);
        let api = detect_api_changes_from_worktree(repo);
        let removed: Vec<&str> = api.removed.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            removed.contains(&"greet"),
            expect_removed,
            "{script}: removed={removed:?} removed_dead={:?}",
            api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }
}

/// Bash で `export -f <name>` されている関数の削除は api.rm に残す。
/// 他リポジトリ消費者向け API として残す必要があるため false negative を避ける。
#[test]
fn detect_api_changes_bash_exported_function_removal_is_still_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "#!/usr/bin/env bash\n\
public_helper() {\n    echo public\n}\nexport -f public_helper\n\n\
main() {\n    echo hi\n}\nmain\n";
    git_commit_files(repo, &[("lib.sh", before)], "initial");

    let after = "#!/usr/bin/env bash\n\
main() {\n    echo hi\n}\nmain\n";
    fs::write(repo.join("lib.sh"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib.sh".to_string(),
        new_path: "lib.sh".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 8,
            new_start: 1,
            new_count: 4,
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
        removed.contains(&"public_helper"),
        "`export -f` された bash 関数の削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// Bash の未 export 関数でも、他ファイルから参照されているなら api.rm に残す。
/// `source common.sh` 経由で他スクリプトが呼ぶケースを考慮し、
/// cross-file refs が 1 件以上なら除外しない。
#[test]
fn detect_api_changes_bash_unexported_function_with_cross_file_ref_is_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "#!/usr/bin/env bash\n\
shared_helper() {\n    echo shared\n}\n\n\
main() {\n    shared_helper\n}\nmain\n";
    let consumer = "#!/usr/bin/env bash\n\
source ./common.sh\nshared_helper\n";
    git_commit_files(
        repo,
        &[("common.sh", before), ("consumer.sh", consumer)],
        "initial",
    );

    let after = "#!/usr/bin/env bash\n\
main() {\n    echo hi\n}\nmain\n";
    fs::write(repo.join("common.sh"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "common.sh".to_string(),
        new_path: "common.sh".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 7,
            new_start: 1,
            new_count: 4,
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
        removed.contains(&"shared_helper"),
        "他ファイルから source 経由で参照されている bash 関数の削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// 同名 overload の片方だけを削除した変更は api.rm に出す。
///
/// 削除判定が「新側に同名があるか」だけを見ていたため、`run(int)` が残ると `run(String)` の
/// 削除を検出できず、別ファイルの `run("x")` がコンパイルできなくなるのに hook が通っていた。
/// 件数が減った削除は曖昧ではないので、(kind, signature) の多重集合の差分を削除として積む。
/// 件数が変わらない overload のシグネチャ変更は、どの overload の変更か決められないため
/// 従来どおり api.rm / api.mod のどちらにも出さない (対照)。
#[test]
fn detect_api_changes_java_partial_overload_removal_is_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let runner =
        |overloads: &str| format!("package demo;\n\npublic class Runner {{\n{overloads}}}\n");
    let run_string = "    public void run(String s) {\n        System.out.println(s);\n    }\n";
    let run_int = "    public void run(int n) {\n        System.out.println(n);\n    }\n";
    let run_long = "    public void run(long n) {\n        System.out.println(n);\n    }\n";
    git_commit_files(
        repo,
        &[
            (
                "src/Runner.java",
                &runner(&format!("{run_string}{run_int}")),
            ),
            (
                "src/Main.java",
                "package demo;\n\npublic class Main {\n    public static void main(String[] args) {\n        Runner r = new Runner();\n        r.run(\"x\");\n        r.run(1);\n    }\n}\n",
            ),
        ],
        "initial",
    );

    // run(String) だけを削除 (run(int) は残る)。
    fs::write(repo.join("src/Runner.java"), runner(run_int)).expect("write");
    let api = detect_api_changes_from_worktree(repo);
    let removed: Vec<(&str, &str)> = api
        .removed
        .iter()
        .map(|s| (s.name.as_str(), s.file.as_str()))
        .collect();
    assert_eq!(
        removed,
        vec![("Runner.run", "src/Runner.java")],
        "overload の片方の削除は api.rm に出すべき。removed_dead={:?} modified={:?}",
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.modified.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    // 対照: 件数が同じまま片方のシグネチャだけ変えた場合は、どの overload の変更か
    // 決められないため従来どおり何も出さない (削除として積まない)。
    fs::write(
        repo.join("src/Runner.java"),
        runner(&format!("{run_long}{run_int}")),
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.removed.is_empty() && api.removed_dead.is_empty() && api.modified.is_empty(),
        "件数が変わらない overload 変更は曖昧として扱うべき。removed={:?} removed_dead={:?} modified={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.modified.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// `#[cfg(..)]` / `#ifdef` の分岐ごとに同じシグネチャで定義した関数を 1 つにまとめても、
/// 同じシグネチャが残る限り api.rm にしない。
///
/// 件数の減った削除を (kind, signature) の多重集合の差分で数えると、分岐の片方が消えた
/// だけで生きている関数が blocking な api.rm になる。まとめた結果シグネチャが変わった
/// 場合は旧シグネチャが残らないので、従来どおり削除として報告する (対照)。
#[test]
fn detect_api_changes_same_signature_variant_consolidation_is_not_removal() {
    let removed_names = |api: &ApiChanges| -> Vec<String> {
        api.removed
            .iter()
            .chain(&api.removed_dead)
            .map(|s| s.name.clone())
            .collect()
    };

    // Rust: プラットフォーム別の `#[cfg]` 分岐。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
            ),
            (
                "src/lib.rs",
                "#[cfg(target_os = \"macos\")]\npub fn open_url(url: &str) -> bool {\n    !url.is_empty()\n}\n\n#[cfg(not(target_os = \"macos\"))]\npub fn open_url(url: &str) -> bool {\n    url.starts_with(\"http\")\n}\n",
            ),
            (
                "src/main.rs",
                "fn main() {\n    println!(\"{}\", demo::open_url(\"x\"));\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn open_url(url: &str) -> bool {\n    !url.is_empty()\n}\n",
    )
    .expect("write lib.rs");
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        removed_names(&api).is_empty() && api.modified.is_empty(),
        "同じシグネチャが残る cfg 分岐の統合を削除にしてはならない。removed={:?} modified={:?}",
        removed_names(&api),
        api.modified.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    // 対照: まとめた結果シグネチャが変わった場合は、旧シグネチャが残らないので削除として報告する。
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn open_url(url: &str) -> Result<(), String> {\n    if url.is_empty() { Err(String::new()) } else { Ok(()) }\n}\n",
    )
    .expect("write lib.rs");
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(
        api.removed
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>(),
        vec!["open_url".to_string(), "open_url".to_string()],
        "旧シグネチャが残らない統合は blocking な削除として報告すべき。removed_dead={:?}",
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    // C: `#ifdef` 分岐。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "util.c",
                "#ifdef _WIN32\nint open_file(const char *p) {\n    return p != 0;\n}\n#else\nint open_file(const char *p) {\n    return p != 0 && p[0] != 0;\n}\n#endif\n",
            ),
            (
                "main.c",
                "int open_file(const char *p);\nint main(void) { return open_file(\"x\"); }\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("util.c"),
        "int open_file(const char *p) {\n    return p != 0 && p[0] != 0;\n}\n",
    )
    .expect("write util.c");
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        removed_names(&api).is_empty(),
        "同じシグネチャが残る #ifdef 分岐の統合を削除にしてはならない。removed={:?}",
        removed_names(&api)
    );
}

/// 別々の `impl` にある同名の関連定数は、owner 付きの qualname (`A.KIND`) で区別する。
///
/// 旧実装は関数 / メソッド以外を bare 名のままにしていたため、`impl A` と `impl B` の
/// `KIND` が同一視されていた。A 側だけの削除は件数の減少としてしか見えず、型の変更は
/// 「同名が複数 = 曖昧」として api.mod から落ちていた。
#[test]
fn detect_api_changes_rust_associated_const_is_qualified_by_owner() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let lib = |a_items: &str| {
        format!(
            "pub struct A;\npub struct B;\n\nimpl A {{\n{a_items}}}\n\nimpl B {{\n    pub const KIND: &'static str = \"b\";\n}}\n"
        )
    };
    git_commit_files(
        repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
            ),
            (
                "src/lib.rs",
                &lib("    pub const KIND: &'static str = \"a\";\n"),
            ),
            (
                "src/main.rs",
                "fn main() {\n    println!(\"{}\", demo::A::KIND);\n    println!(\"{}\", demo::B::KIND);\n}\n",
            ),
        ],
        "initial",
    );
    let names =
        |symbols: &[ApiSymbol]| -> Vec<String> { symbols.iter().map(|s| s.name.clone()).collect() };
    let changed_names = |changes: &[ApiSymbolChange]| -> Vec<String> {
        changes.iter().map(|s| s.name.clone()).collect()
    };

    // A 側だけ削除: `demo::A::KIND` の利用が残るので blocking な api.rm。
    fs::write(repo.join("src/lib.rs"), lib("")).expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(
        names(&api.removed),
        vec!["A.KIND".to_string()],
        "A 側の関連定数の削除を owner 付きで報告すべき。removed_dead={:?}",
        names(&api.removed_dead)
    );

    // 型の変更: 同名が別 impl にあっても owner で区別できるので api.mod に出る。
    fs::write(
        repo.join("src/lib.rs"),
        lib("    pub const KIND: u32 = 1;\n"),
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(
        changed_names(&api.modified),
        vec!["A.KIND".to_string()],
        "A 側の関連定数の型変更は api.mod に出すべき。const_value={:?}",
        changed_names(&api.const_value_changes)
    );

    // 対照: 値だけの変更は従来どおり const_value_changes (非 blocking)。
    fs::write(
        repo.join("src/lib.rs"),
        lib("    pub const KIND: &'static str = \"z\";\n"),
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.modified.is_empty() && api.removed.is_empty(),
        "値だけの変更を blocking にしてはならない。modified={:?} removed={:?}",
        changed_names(&api.modified),
        names(&api.removed)
    );
    assert_eq!(
        changed_names(&api.const_value_changes),
        vec!["A.KIND".to_string()]
    );
}

/// 同名シンボルの 1 つだけを、同名が既にある別ファイルへ移した変更は moved として相殺する。
///
/// 件数の減った削除を api.rm に積むようになったため、移動先の「件数の増えた追加」も
/// move 突き合わせの候補にしないと、同名が両ファイルに残るだけの移動が破壊的削除に化ける。
#[test]
fn detect_api_changes_partial_same_name_move_is_reconciled_as_moved() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let string_fmt = "fun String.fmt(): String = this\n";
    let int_fmt = "fun Int.fmt(): String = toString()\n";
    let long_fmt = "fun Long.fmt(): String = toString()\n";
    git_commit_files(
        repo,
        &[
            (
                "src/A.kt",
                &format!("package demo\n\n{string_fmt}\n{int_fmt}"),
            ),
            ("src/B.kt", &format!("package demo\n\n{long_fmt}")),
            (
                "src/Main.kt",
                "package demo\n\nfun main() {\n    println(1.fmt())\n}\n",
            ),
        ],
        "initial",
    );

    // `Int.fmt` を A.kt から B.kt へ移す (両ファイルに別の `fmt` が残る)。
    fs::write(
        repo.join("src/A.kt"),
        format!("package demo\n\n{string_fmt}"),
    )
    .expect("write");
    fs::write(
        repo.join("src/B.kt"),
        format!("package demo\n\n{long_fmt}\n{int_fmt}"),
    )
    .expect("write");
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.removed.is_empty(),
        "同名の 1 つを移しただけで api.rm にしてはならない。removed={:?}",
        api.removed
            .iter()
            .map(|s| (&s.name, &s.file))
            .collect::<Vec<_>>()
    );
    let moved: Vec<(&str, &str, &str)> = api
        .moved
        .iter()
        .map(|m| (m.name.as_str(), m.from.as_str(), m.to.as_str()))
        .collect();
    assert_eq!(moved, vec![("fmt", "src/A.kt", "src/B.kt")]);
}

/// 独立した同名定義・束縛と、削除した自由関数への実参照を対照する。
fn removal_scope_fixture(old_path: &str, extras: &[(&str, &str)]) -> ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let (old, new) = if old_path.ends_with(".rs") {
        (
            "pub fn total(x: u32) -> u32 { x }\npub fn keep() -> u32 { 1 }\n",
            "pub fn keep() -> u32 { 1 }\n",
        )
    } else if old_path.ends_with(".mjs") {
        (
            "export function abort() {}\nexport function keep() {}\n",
            "export function keep() {}\n",
        )
    } else if old_path.ends_with(".sh") {
        ("abort() { :; }\nkeep() { :; }\n", "keep() { :; }\n")
    } else {
        (
            "def abort(message):\n    raise SystemExit(message)\ndef version():\n    return '1.0'\n",
            "def version():\n    return '1.0'\n",
        )
    };
    let mut files = vec![(old_path, old)];
    files.extend_from_slice(extras);
    git_commit_files(repo, &files, "base");
    fs::write(repo.join(old_path), new).expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: old_path.into(),
        new_path: old_path.into(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: old.lines().count(),
            new_start: 1,
            new_count: new.lines().count(),
        }],
        deleted_old_source: None,
    }];
    detect_api_changes(repo.to_str().expect("path"), "HEAD", &diff_files)
}

#[test]
fn removed_free_function_ignores_independent_python_definitions_and_calls() {
    for extras in [
        vec![
            (
                "tools/report.py",
                "def abort(message):\n    print(message)\n",
            ),
            (
                "tools/notify.py",
                "def abort(message):\n    print(message)\n",
            ),
        ],
        vec![(
            "tools/report.py",
            "def abort(message):\n    print(message)\nabort('report failed')\n",
        )],
    ] {
        let api = removal_scope_fixture("tools/util.py", &extras);
        assert!(api.removed.is_empty(), "{:?}", api.removed);
        assert!(api.removed_dead.iter().any(|s| s.name == "abort"));
    }
}

#[test]
fn removed_free_function_ignores_rust_local_values() {
    let api = removal_scope_fixture(
        "src/a.rs",
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/b.rs", "pub fn run() -> u32 { let total = 3; total }\n"),
        ],
    );
    assert!(api.removed.is_empty(), "{:?}", api.removed);
    assert!(api.removed_dead.iter().any(|s| s.name == "total"));
}

#[test]
fn removed_free_function_preserves_js_and_bash_definition_guards() {
    for (path, extras) in [
        (
            "src/api.mjs",
            vec![
                ("src/report.mjs", "export class abort {}\n"),
                ("src/notify.mjs", "export class abort {}\n"),
            ],
        ),
        (
            "src/api.sh",
            vec![
                ("src/report.sh", "abort() { :; }\n"),
                ("src/notify.sh", "abort() { :; }\n"),
            ],
        ),
    ] {
        let api = removal_scope_fixture(path, &extras);
        assert!(
            api.removed.iter().any(|s| s.name == "abort"),
            "{path}\n{:?}",
            api.removed_dead
        );
    }
}

#[test]
fn removed_free_function_keeps_unproven_python_origins_blocking() {
    for source in [
        "from tools.util import abort\nabort('failed')\n",
        "import tools.util as util\nutil.abort('failed')\n",
        "def abort(message):\n    print(message)\nfrom tools.util import abort\nabort('failed')\n",
        "from tools.util import *\ndef abort(message):\n    print(message)\nabort('failed')\n",
        "def abort(message):\n    print(message)\nexec('abort = replacement')\nabort('failed')\n",
        "import builtins\ndef abort(message):\n    pass\ngetattr(builtins, 'exec')('from tools.util import abort')\nabort('failed')\n",
        "import builtins\ndef abort(message):\n    pass\nbuiltins.__dict__['exec']('from tools.util import abort')\nabort('failed')\n",
        "def abort(message):\n    print(message)\ndef run(abort):\n    abort('failed')\n",
        "def abort(message):\n    print(message)\nabort = replacement\nabort('failed')\n",
        "def abort(message):\n    print(message)\nclass abort:\n    pass\nabort('failed')\n",
        "def abort(message):\n    print(message)\ndef abort(message):\n    print(message)\nabort('failed')\n",
        "if enabled:\n    def abort(message):\n        print(message)\nabort('failed')\n",
    ] {
        let api = removal_scope_fixture("tools/util.py", &[("tools/report.py", source)]);
        assert!(
            api.removed.iter().any(|s| s.name == "abort"),
            "{source}\n{:?}",
            api.removed_dead
        );
    }
    // 未知の同名定義を、別ファイルの証明済み呼び出しだけで降格しない。
    let api = removal_scope_fixture(
        "tools/util.py",
        &[
            (
                "tools/report.py",
                "def abort(message):\n    print(message)\nabort('failed')\n",
            ),
            ("tools/notify.py", "class abort:\n    pass\n"),
        ],
    );
    assert!(api.removed.iter().any(|s| s.name == "abort"));
    for path in [
        "tools/util.pyi",
        "tools/util/__init__.py",
        "tools/util/__init__.pyi",
    ] {
        let api = removal_scope_fixture(
            "tools/util.py",
            &[(
                path,
                "def abort(message):\n    print(message)\nabort('failed')\n",
            )],
        );
        assert!(api.removed.iter().any(|s| s.name == "abort"), "{path}");
    }
}

#[test]
fn removed_free_function_rust_scope_proof_stays_inside_value_bindings() {
    for source in [
        "pub fn run() -> u32 { let total = 3; total }\n",
        "pub fn run(total: u32) -> u32 { total }\n",
        "pub fn run() -> u32 { let (_, total) = (1, 3); total }\n",
        "pub fn run() -> u32 { let total = 3; { total } }\n",
        "pub fn run() -> u32 { let f = |total| total; f(3) }\n",
        "struct Cfg { total: u32 }\npub fn run(c: Cfg) -> u32 { c.total }\n",
    ] {
        let api = removal_scope_fixture(
            "src/a.rs",
            &[
                ("src/lib.rs", "pub mod a;\npub mod b;\n"),
                ("src/b.rs", source),
            ],
        );
        assert!(api.removed.is_empty(), "{source}\n{:?}", api.removed);
        assert!(api.removed_dead.iter().any(|s| s.name == "total"));
    }
    for source in [
        "pub fn run() -> u32 { let total = total(3); total }\n",
        "pub fn run() -> u32 { { let total = 3; } total(3) }\n",
        "pub fn run() -> u32 { if let total = value {} total(3) }\n",
        "pub fn run() -> u32 { let Some(total) = value else { return total(3); }; total }\n",
        "pub fn run() -> u32 { #[cfg(test)] let total = 3; total(3) }\n",
        "pub fn run(#[cfg(test)] total: u32) -> u32 { total(3) }\n",
        "pub fn run(#[cfg_attr(test, cfg(test))] total: u32) -> u32 { total(3) }\n",
        "pub fn run() -> u32 { let total = 3; crate::a::total(3) }\n",
        "pub fn run() -> u32 { let total = 3; m!(total) }\n",
        "pub fn run() -> u32 { let total = 3; { import_total!(); total(3) } }\n",
        "pub fn run() -> u32 { let total = 3; { include!(\"imports.rs\"); total(3) } }\n",
        "make_constants!();\npub fn run(total: ()) { let _ = total; }\n",
        "pub fn run() -> u32 { let total = 3; { use crate::a::*; total(3) } }\n",
        "pub fn run() -> u32 { let total = 3; fn inner() -> u32 { total(3) } inner() }\n",
        "use crate::a::total;\npub fn run() -> u32 { total(3) }\n",
        "pub fn total(x: u32) -> u32 { x }\npub fn run() -> u32 { crate::a::total(3) }\n",
        "struct Table { total: fn(u32) -> u32 }\npub fn run() { let table = Table { total }; }\n",
    ] {
        let api = removal_scope_fixture(
            "src/a.rs",
            &[
                ("src/lib.rs", "pub mod a;\npub mod b;\n"),
                ("src/b.rs", source),
            ],
        );
        assert!(
            api.removed.iter().any(|s| s.name == "total"),
            "{source}\n{:?}",
            api.removed_dead
        );
    }
}

#[test]
fn removed_free_function_rust_expression_macros_do_not_hide_independent_values() {
    for source in [
        "pub fn run() -> u32 { let total = make!(); total }\n",
        "pub fn run() -> u32 { let total = 3; let other = make!(); total + other }\n",
        "pub fn log() { log!(); }\npub fn run(total: u32) -> u32 { total }\n",
    ] {
        let api = removal_scope_fixture(
            "src/a.rs",
            &[
                ("src/lib.rs", "pub mod a;\npub mod b;\n"),
                ("src/b.rs", source),
            ],
        );
        assert!(api.removed.is_empty(), "{source}\n{:?}", api.removed);
        assert!(api.removed_dead.iter().any(|s| s.name == "total"));
    }
    for source in [
        "pub fn run() -> u32 { let total = 3; bind!(total); total }\n",
        "#[rewrite] pub fn run(total: u32) -> u32 { total }\n",
        "#[rewrite] impl Store { pub fn run(total: u32) -> u32 { total } }\n",
        "#[rewrite] mod nested { pub fn run(total: u32) -> u32 { total } }\n",
        "#[rewrite] fn outer() { fn inner(total: u32) -> u32 { total } }\n",
        "pub fn run() -> u32 { let total = 3; total = total(3); total }\n",
        "pub fn run() -> u32 { let total = 3; let total = total; total(3) }\n",
    ] {
        let api = removal_scope_fixture(
            "src/a.rs",
            &[
                ("src/lib.rs", "pub mod a;\npub mod b;\n"),
                ("src/b.rs", source),
            ],
        );
        assert!(
            api.removed.iter().any(|s| s.name == "total"),
            "{source}\n{:?}",
            api.removed_dead
        );
    }
}

#[test]
fn removed_free_function_ignores_initialized_submodule_references_only() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("tools/old.py", "def main():\n    return 0\n")],
        "base",
    );
    let oid = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    let entry = format!(
        "160000,{},vendor/plugin",
        String::from_utf8(oid.stdout).unwrap().trim()
    );
    assert!(
        std::process::Command::new("git")
            .args(["update-index", "--add", "--cacheinfo", &entry])
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["commit", "-m", "submodule"])
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir_all(repo.join("vendor/plugin")).unwrap();
    fs::write(
        repo.join("vendor/plugin/client.py"),
        "from tools.old import main\nmain()\n",
    )
    .unwrap();
    fs::remove_file(repo.join("tools/old.py")).unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(api.removed.iter().any(|symbol| symbol.name == "main"));
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.join("vendor/plugin"))
            .status()
            .unwrap()
            .success()
    );
    let api = detect_api_changes_from_worktree(repo);
    assert!(api.removed.is_empty(), "{:?}", api.removed);
    assert!(api.removed_dead.iter().any(|symbol| symbol.name == "main"));
    fs::write(
        repo.join("vendor/plugin-copy.py"),
        "from tools.old import main\nmain()\n",
    )
    .unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(api.removed.iter().any(|symbol| symbol.name == "main"));
}
