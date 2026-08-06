//! C/C++ / Swift / PHP / bash / Kotlin / Tauri の API 差分検出テスト。

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

/// Kotlin の `function_declaration` は tree-sitter-kotlin 0.3.5 で body フィールドを
/// 持たず (`fields: []`)、body は `function_body` 型の直接子として現れる。
/// `extract_api_signature` が body フィールドだけを見て切ると関数全体 (body 込み) が
/// 署名になり、シグネチャ不変で body だけ変えた関数が api.mod に誤検出される。
/// `function_body_start_byte` の kind fallback でこれを抑止する
/// (moon-star-link の @Composable / helper 3 件が api.mod blocking した回帰防止)。
#[test]
fn kotlin_body_only_change_is_not_api_mod() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // 旧: シグネチャ A の関数 + シグネチャ B の関数。
    git_commit_files(
        repo,
        &[(
            "MapZoomUtils.kt",
            "fun fitCameraToLocations(\n    locations: List<LatLng>,\n    maxZoom: Float = 18f\n) {\n    if (locations.size == 1) {\n        center(locations.first())\n        return\n    }\n    zoomForBounds(locations)\n}\n\nfun renameMe(a: Int): Int {\n    return a + 1\n}\n",
        )],
        "initial",
    );

    // 新: fitCameraToLocations は body のみ変更 (シグネチャ不変)。
    //     renameMe はシグネチャ変更 (引数追加) — 過剰抑制ガード。
    fs::write(
        repo.join("MapZoomUtils.kt"),
        "fun fitCameraToLocations(\n    locations: List<LatLng>,\n    maxZoom: Float = 18f\n) {\n    if (locations.allSamePosition()) {\n        center(locations.first())\n        return\n    }\n    fitBounds(locations)\n}\n\nfun renameMe(a: Int, b: Int): Int {\n    return a + b\n}\n",
    )
    .expect("write new MapZoomUtils.kt");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "MapZoomUtils.kt".to_string(),
        new_path: "MapZoomUtils.kt".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 14,
            new_start: 1,
            new_count: 14,
        }],
        deleted_old_source: None,
    }];

    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        !api.modified
            .iter()
            .any(|c| c.name == "fitCameraToLocations"),
        "body のみ変更 (シグネチャ不変) は api.mod に出すべきでない: {:?}",
        api.modified
    );
    assert!(
        api.modified.iter().any(|c| c.name == "renameMe"),
        "シグネチャ変更 (引数追加) は引き続き api.mod に検出すべき (過剰抑制ガード): {:?}",
        api.modified
    );
}

/// C++ のマクロ呼び出し `BOOST_FOREACH(...) { ... }` は tree-sitter-cpp が関数定義として
/// 誤パースし、実関数 body 内にネストした偽の function_definition として現れる。引数列が
/// 変わっても api.mod に出してはならない
/// (Issue #13: 差分外の BOOST_FOREACH を api_changes.modified に拾う誤検出対策)。
#[test]
fn detect_api_changes_cpp_nested_macro_call_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = "void CallInfoManager::Process() {\n    BOOST_FOREACH( const TYPE_CALL_MAP::value_type info, call_inf_map ) {\n        use_it(info.szMyNum);\n    }\n}\n";
    fs::write(src_dir.join("CallInfoManager.cpp"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/CallInfoManager.cpp"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // BOOST_FOREACH の引数を `call_inf_map` → `this->call_inf_map` に変更しただけ。
    let after = "void CallInfoManager::Process() {\n    BOOST_FOREACH (const TYPE_CALL_MAP::value_type info, this->call_inf_map) {\n        use_it(info.szMyNum);\n    }\n}\n";
    fs::write(src_dir.join("CallInfoManager.cpp"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/CallInfoManager.cpp".to_string(),
        new_path: "src/CallInfoManager.cpp".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        !api_changes
            .modified
            .iter()
            .any(|c| c.name == "BOOST_FOREACH"),
        "BOOST_FOREACH (マクロ誤パース) を api.mod に出すべきではない: {:?}",
        api_changes.modified
    );
}

/// C++ のオーバーロード (同名・異シグネチャ) は HashMap<name, sig> で最後の 1 件しか
/// 残らず、別オーバーロード同士を突き合わせる危険がある。同名が複数あるシンボルは曖昧
/// として api.mod から除外する (Issue #13)。
#[test]
fn detect_api_changes_cpp_overload_excluded_from_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before =
        "int compute(int x) {\n    return x;\n}\nint compute(double x) {\n    return 0;\n}\n";
    fs::write(src_dir.join("calc.cpp"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/calc.cpp"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    // HashMap 代表となる 2 番目のオーバーロードのシグネチャを変更する。
    let after = "int compute(int x) {\n    return x;\n}\nint compute(double x, int y) {\n    return 0;\n}\n";
    fs::write(src_dir.join("calc.cpp"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/calc.cpp".to_string(),
        new_path: "src/calc.cpp".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 6,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        !api_changes.modified.iter().any(|c| c.name == "compute"),
        "同名オーバーロード compute は曖昧として modified から除外すべき: {:?}",
        api_changes.modified
    );
}

/// 通常の C++ トップレベル関数のシグネチャ変更は #13 の修正後も api.mod に出る。
/// nested 除外 / 同名複数除外が正常な検出を巻き込まないことの回帰テスト。
#[test]
fn detect_api_changes_cpp_real_function_signature_change_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = "int handle(int x) {\n    return x;\n}\n";
    fs::write(src_dir.join("handler.cpp"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/handler.cpp"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );

    let after = "int handle(int x, int y) {\n    return x + y;\n}\n";
    fs::write(src_dir.join("handler.cpp"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/handler.cpp".to_string(),
        new_path: "src/handler.cpp".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api_changes.modified.iter().any(|c| c.name == "handle"),
        "通常関数 handle の signature 変更は modified に出るべき: {:?}",
        api_changes.modified
    );
}

/// Tauri command の自動注入型引数 (AppHandle) 追加は JS-facing signature 不変なので
/// api.mod / mod_closed のどちらにも出ない (パターンB)。
#[test]
fn detect_api_changes_tauri_command_injected_arg_addition_not_flagged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src-tauri/Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src-tauri/src/lib.rs", "pub mod cmd;\n"),
            (
                "src-tauri/src/cmd.rs",
                "#[tauri::command]\npub fn get_status(id: u32) -> String {\n    String::new()\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
            repo.join("src-tauri/src/cmd.rs"),
            "#[tauri::command]\npub fn get_status(app: tauri::AppHandle, id: u32) -> String {\n    String::new()\n}\n",
        )
        .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src-tauri/src/cmd.rs".to_string(),
        new_path: "src-tauri/src/cmd.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let flagged = api.modified.iter().any(|m| m.name.ends_with("get_status"))
        || api
            .modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("get_status"));
    assert!(
        !flagged,
        "Tauri 自動注入引数の追加は signature 差分にしない。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// Tauri command でも通常引数の追加は呼び出し契約を変えるため signature 差分として検出される。
#[test]
fn detect_api_changes_tauri_command_regular_arg_addition_is_flagged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src-tauri/Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src-tauri/src/lib.rs", "pub mod cmd;\n"),
            (
                "src-tauri/src/cmd.rs",
                "#[tauri::command]\npub fn get_status(id: u32) -> String {\n    String::new()\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
            repo.join("src-tauri/src/cmd.rs"),
            "#[tauri::command]\npub fn get_status(id: u32, verbose: bool) -> String {\n    String::new()\n}\n",
        )
        .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src-tauri/src/cmd.rs".to_string(),
        new_path: "src-tauri/src/cmd.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let flagged = api.modified.iter().any(|m| m.name.ends_with("get_status"))
        || api
            .modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("get_status"));
    assert!(
        flagged,
        "通常引数の追加は signature 差分として検出されるべき"
    );
}

/// Channel<T> は JS 側から渡す引数なので Tauri 自動注入から除外せず signature 差分に残す。
#[test]
fn detect_api_changes_tauri_command_channel_arg_is_flagged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src-tauri/Cargo.toml",
                "[package]\nname = \"app\"\n\n[lib]\nname = \"app_lib\"\n",
            ),
            ("src-tauri/src/lib.rs", "pub mod cmd;\n"),
            (
                "src-tauri/src/cmd.rs",
                "#[tauri::command]\npub fn watch(id: u32) -> String {\n    String::new()\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
            repo.join("src-tauri/src/cmd.rs"),
            "#[tauri::command]\npub fn watch(id: u32, on_event: Channel<String>) -> String {\n    String::new()\n}\n",
        )
        .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src-tauri/src/cmd.rs".to_string(),
        new_path: "src-tauri/src/cmd.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let flagged = api.modified.iter().any(|m| m.name.ends_with("watch"))
        || api
            .modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("watch"));
    assert!(
        flagged,
        "Channel<T> 引数は除外せず signature 差分に残すべき"
    );
}

/// Swift の internal 型 (public/open でない) は外部 API ではないため api.add に出さない。
/// public 型は引き続き出す (パターンD: sidecar/executable 内部型を api.add に出さない)。
#[test]
fn detect_api_changes_swift_internal_type_excluded_from_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("README.md", "init\n")], "base");
    fs::write(
            repo.join("helper.swift"),
            "enum DetectionError: Error {\n    case failed\n}\npublic struct Detector {\n    public func run() -> Int { 0 }\n}\n",
        )
        .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "/dev/null".to_string(),
        new_path: "helper.swift".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !added.iter().any(|n| n.ends_with("DetectionError")),
        "Swift internal enum は api.add に出ない。got: {added:?}"
    );
    assert!(
        added.iter().any(|n| n.contains("Detector")),
        "Swift public struct は api.add に出る。got: {added:?}"
    );
}

/// Swift の public protocol requirement の signature 変更は外部公開 API 変更なので
/// api 差分 (mod / mod_closed) に出る (codex 指摘2 の false negative 回避)。
#[test]
fn detect_api_changes_swift_public_protocol_requirement_signature_change_is_flagged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "Service.swift",
            "public protocol Service {\n    func handle() -> Int\n}\n",
        )],
        "base",
    );
    fs::write(
        repo.join("Service.swift"),
        "public protocol Service {\n    func handle(_ value: Int) -> Int\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "Service.swift".to_string(),
        new_path: "Service.swift".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let flagged = api.modified.iter().any(|m| m.name.ends_with("handle"))
        || api
            .modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("handle"));
    assert!(
        flagged,
        "public protocol requirement の signature 変更は api.mod に出る。mod={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}

/// 複数行の Swift protocol requirement でも signature 変更が AST 抽出で検出される
/// (先頭行 fallback では 2 行目以降の型変更を見逃す、codex 指摘)。
#[test]
fn detect_api_changes_swift_multiline_protocol_requirement_signature_change_is_flagged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "Service.swift",
            "public protocol Service {\n    func handle(\n        _ value: Int\n    ) -> Int\n}\n",
        )],
        "base",
    );
    // 2 行目の型のみ Int → String に変更 (先頭行 `func handle(` は不変)
    fs::write(
        repo.join("Service.swift"),
        "public protocol Service {\n    func handle(\n        _ value: String\n    ) -> Int\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "Service.swift".to_string(),
        new_path: "Service.swift".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 6,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let flagged = api.modified.iter().any(|m| m.name.ends_with("handle"))
        || api
            .modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("handle"));
    assert!(
        flagged,
        "複数行 protocol requirement の型変更も api.mod に出る。mod={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}

/// Bash スクリプトで同一ファイル内から呼ばれている新規関数は api.add に出ない。
/// (レポート 2026-04-17-api-add-bash-connected-function-false-positive.md)
#[test]
fn detect_api_changes_bash_internally_called_function_is_not_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "#!/usr/bin/env bash\n\
sparse_clone_or_update() {\n    echo clone\n}\n\n\
for repo in \"foo\"; do\n    sparse_clone_or_update\ndone\n";
    git_commit_files(repo, &[("sp.sh", before)], "initial");

    // sparse_patterns_for を新規追加し、同ファイル内の sparse_clone_or_update から呼び出す
    let after = "#!/usr/bin/env bash\n\
sparse_patterns_for() {\n    echo pattern\n}\n\n\
sparse_clone_or_update() {\n    sparse_patterns_for\n    echo clone\n}\n\n\
for repo in \"foo\"; do\n    sparse_clone_or_update\ndone\n";
    fs::write(repo.join("sp.sh"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "sp.sh".to_string(),
        new_path: "sp.sh".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 8,
            new_start: 1,
            new_count: 11,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !added.contains(&"sparse_patterns_for"),
        "同一ファイル内から呼ばれている Bash 関数は api.add に出してはならない。got: {added:?}"
    );
}

/// Bash で同一ファイル内から呼ばれていない新規関数は api.add に残る。
#[test]
fn detect_api_changes_bash_disconnected_function_is_still_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "#!/usr/bin/env bash\n\
main() {\n    echo hi\n}\nmain\n";
    git_commit_files(repo, &[("sp.sh", before)], "initial");

    // 新規関数 unused_helper は誰も呼んでいない
    let after = "#!/usr/bin/env bash\n\
unused_helper() {\n    echo unused\n}\n\n\
main() {\n    echo hi\n}\nmain\n";
    fs::write(repo.join("sp.sh"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "sp.sh".to_string(),
        new_path: "sp.sh".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        added.contains(&"unused_helper"),
        "同一ファイル内から呼ばれていない新規関数は api.add に残すべき。got: {added:?}"
    );
}

/// Bash の `trap <fn> SIGNAL` で参照される関数は、同一ファイル内で cleanup
/// ハンドラとして使われるだけのため api.add に出してはならない。
/// (レポート 2026-04-21-bash-trap-exit-handler-false-positive.md の再現)
#[test]
fn detect_api_changes_bash_trap_handler_is_not_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "#!/usr/bin/env bash\n\
echo initial\n";
    git_commit_files(repo, &[("run_review.sh", before)], "initial");

    // 新規に cleanup ハンドラを追加し、trap でのみ参照する
    let after = "#!/usr/bin/env bash\n\
stop_memory_sampler() {\n    echo stop\n}\n\n\
trap stop_memory_sampler EXIT\n\
echo initial\n";
    fs::write(repo.join("run_review.sh"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "run_review.sh".to_string(),
        new_path: "run_review.sh".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !added.contains(&"stop_memory_sampler"),
        "trap <fn> EXIT でのみ参照される bash 関数は api.add に出してはならない。got: {added:?}"
    );
}

/// Bash の内部ヘルパー関数（同一ファイル内でのみ呼ばれる）のシグネチャ変更も
/// api.mod に出さない（パターン A と対称）。
#[test]
fn detect_api_changes_bash_internal_signature_change_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "#!/usr/bin/env bash\n\
timed() {\n    \"$@\"\n}\n\n\
main() {\n    timed echo hi\n}\nmain\n";
    git_commit_files(repo, &[("run.sh", before)], "initial");

    // timed の宣言行を変更（シグネチャ変更相当）
    let after = "#!/usr/bin/env bash\n\
timed() { # wrap with timing\n    \"$@\"\n}\n\n\
main() {\n    timed echo hi\n}\nmain\n";
    fs::write(repo.join("run.sh"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "run.sh".to_string(),
        new_path: "run.sh".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 2,
            old_count: 1,
            new_start: 2,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"timed"),
        "同一ファイル内でのみ呼ばれる bash 関数のシグネチャ変更は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// Bash スクリプトファイルを丸ごと別言語 (Python) に置き換えた場合、
/// 未 export な bash 関数は api.rm から除外する。
/// (レポート 2026-05-01 再発ケース2 / コミット eae0fe0 の再現)
#[test]
fn detect_api_changes_bash_file_replaced_with_python_drops_private_funcs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let bash_before = "#!/usr/bin/env bash\n\
fetch_with_retry() {\n    curl \"$1\"\n}\n\n\
main() {\n    fetch_with_retry https://example.com\n}\nmain\n";
    git_commit_files(repo, &[("scripts/qa_diff.sh", bash_before)], "initial");

    // bash スクリプトを削除し、別言語ファイルを新設
    std::fs::remove_file(repo.join("scripts/qa_diff.sh")).expect("remove bash");
    let py_after = "def fetch_with_retry(url: str) -> str:\n    return url\n\n\
def main() -> None:\n    fetch_with_retry(\"https://example.com\")\n\n\
if __name__ == \"__main__\":\n    main()\n";
    fs::write(repo.join("scripts/qa_diff.py"), py_after).expect("write py");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "scripts/qa_diff.sh".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 8,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "scripts/qa_diff.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 7,
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
        !removed.contains(&"fetch_with_retry"),
        "別言語に置換されたファイル削除でも、未 export bash 関数は api.rm に出してはならない。got: {removed:?}"
    );
}

/// Bash ファイル削除時、`export -f` 済み関数は api.rm に残す。
/// 他リポジトリ消費者向け API として false negative を避ける。
#[test]
fn detect_api_changes_bash_file_deletion_keeps_exported_function() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let lib_before = "#!/usr/bin/env bash\n\
public_helper() {\n    echo public\n}\nexport -f public_helper\n";
    git_commit_files(repo, &[("lib.sh", lib_before)], "initial");

    std::fs::remove_file(repo.join("lib.sh")).expect("remove");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib.sh".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 0,
            new_count: 0,
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
        "ファイル削除でも `export -f` 済み bash 関数は api.rm に残すべき。got: {removed:?}"
    );
}

// ------------------------------------------------------------------
// is_bash_script_path / bash_has_export_f ヘルパー
// ------------------------------------------------------------------

#[test]
fn is_bash_script_path_matches_shell_extensions() {
    assert!(is_bash_script_path("scripts/foo.sh"));
    assert!(is_bash_script_path("scripts/foo.bash"));
    assert!(is_bash_script_path("scripts/foo.zsh"));
    assert!(!is_bash_script_path("scripts/foo.py"));
    assert!(!is_bash_script_path("scripts/Makefile"));
    assert!(!is_bash_script_path("scripts/foo"));
}

#[test]
fn bash_has_export_f_detects_export_minus_f() {
    let src = "#!/usr/bin/env bash\n\
foo() { echo hi; }\n\
export -f foo\n\
bar() { echo bye; }\n";
    assert!(bash_has_export_f(src, "foo"));
    assert!(!bash_has_export_f(src, "bar"));
}

#[test]
fn bash_has_export_f_detects_declare_variants() {
    let src = "    declare -fx foo\n  declare -xf bar\n";
    assert!(bash_has_export_f(src, "foo"));
    assert!(bash_has_export_f(src, "bar"));
}

#[test]
fn bash_has_export_f_supports_multiple_names_per_line() {
    let src = "export -f foo bar baz\n";
    assert!(bash_has_export_f(src, "foo"));
    assert!(bash_has_export_f(src, "bar"));
    assert!(bash_has_export_f(src, "baz"));
    assert!(!bash_has_export_f(src, "qux"));
}

#[test]
fn bash_has_export_f_does_not_match_partial_or_substring() {
    let src = "export -f foo_bar\n";
    assert!(bash_has_export_f(src, "foo_bar"));
    assert!(!bash_has_export_f(src, "foo"));
    assert!(!bash_has_export_f(src, "bar"));
}

#[test]
fn bash_has_export_f_rejects_empty_name() {
    let src = "export -f \n";
    assert!(!bash_has_export_f(src, ""));
}

/// PHPUnit 規約判定は qualname の末尾へ正規化するため、bare name と
/// `Container.name` の結果が一致する。この前提が変わった場合は、公開面の最終判定も
/// 合わせて再検討する必要がある。
#[test]
fn phpunit_test_symbol_is_invariant_to_qualname_prefix() {
    use crate::commands::dead_code::is_phpunit_test_symbol;
    use crate::models::symbol::SymbolKind;

    for (bare, qualname) in [
        ("testBar", "FooTest.testBar"),
        ("setUp", "FooTest.setUp"),
        ("helper", "FooTest.helper"),
    ] {
        assert_eq!(
            is_phpunit_test_symbol(bare, SymbolKind::Method, crate::language::LangId::Php),
            is_phpunit_test_symbol(qualname, SymbolKind::Method, crate::language::LangId::Php),
            "bare={bare} qualname={qualname}"
        );
    }
}
