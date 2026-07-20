use super::*;
#[allow(unused_imports)]
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MissingCochange,
    MovedSymbol, PropertyToFieldChange, ReviewResult,
};
use std::fs;
use std::io::Cursor;
use std::process::Command;

#[test]
fn read_to_string_limited_accepts_small_input() {
    let text = read_to_string_limited(Cursor::new(b"ok".to_vec()), 4, "stdin").unwrap();
    assert_eq!(text, "ok");
}

#[test]
fn read_to_string_limited_rejects_oversized_input() {
    let err = read_to_string_limited(Cursor::new(b"abcde".to_vec()), 4, "stdin")
        .expect_err("oversized input should fail");

    assert!(err.to_string().contains("exceeds maximum size"));
}

#[test]
fn read_bytes_limited_and_drain_reports_full_size() {
    let err = read_bytes_limited_and_drain(Cursor::new(vec![b'a'; 10]), 4, "git diff output")
        .expect_err("oversized input should fail");

    assert!(err.to_string().contains("10 bytes > 4 bytes"));
}

#[test]
fn read_to_string_limited_rejects_invalid_utf8() {
    let err = read_to_string_limited(Cursor::new(vec![0xff]), 4, "stdin")
        .expect_err("invalid utf-8 should fail");

    assert!(err.to_string().contains("not valid UTF-8"));
}

#[test]
fn read_paths_file_limited_trims_blank_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("paths.txt");
    fs::write(&path, " src/main.rs \n\nCargo.toml\n").expect("write paths file");

    let paths =
        read_paths_file_limited(path.to_str().expect("utf-8 path"), 1024).expect("read paths");

    assert_eq!(paths, vec!["src/main.rs", "Cargo.toml"]);
}

#[test]
fn read_paths_file_limited_rejects_oversized_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("paths.txt");
    fs::write(&path, "abcde").expect("write paths file");

    let err = read_paths_file_limited(path.to_str().expect("utf-8 path"), 4)
        .expect_err("oversized paths-file should fail");

    assert!(err.to_string().contains("exceeds maximum size"));
}

#[test]
fn validate_git_revision_accepts_normal_values() {
    assert!(validate_git_revision("HEAD", "--base").is_ok());
    assert!(validate_git_revision("HEAD^", "--base").is_ok());
    assert!(validate_git_revision("main", "--base").is_ok());
    assert!(validate_git_revision("origin/main", "--base").is_ok());
    assert!(validate_git_revision("feature/foo", "--base").is_ok());
    assert!(validate_git_revision("abc1234", "--base").is_ok());
    assert!(validate_git_revision("v1.0.0", "--base").is_ok());
}

// `--output=/path` 等のオプション注入を拒否する
#[test]
fn validate_git_revision_rejects_option_prefix() {
    let err = validate_git_revision("--output=/tmp/pwn", "--base")
        .expect_err("option-like base should be rejected");
    assert!(err.to_string().contains("must not start with '-'"));

    let err = validate_git_revision("-p", "--base").expect_err("short option should be rejected");
    assert!(err.to_string().contains("must not start with '-'"));
}

#[test]
fn validate_git_revision_rejects_empty() {
    let err = validate_git_revision("", "--base").expect_err("empty revision should be rejected");
    assert!(err.to_string().contains("must not be empty"));
}

#[test]
fn validate_git_revision_rejects_nul() {
    let err =
        validate_git_revision("HEAD\0foo", "--base").expect_err("NUL byte should be rejected");
    assert!(err.to_string().contains("must not contain NUL"));
}

/// 複数行 grouped use ブロックの継続行で import されたシンボルの signature 変更でも、
/// 呼び出し側を同一 diff で更新済みなら modified_closed_in_diff (informational) に
/// 降格される。grouped use 継続行 (`    a, changed_fn, b,`) を未更新 caller と誤判定して
/// blocking しないことを保証する (api.mod 誤検出 2026-05-31 の回帰防止)。
#[test]
fn detect_api_changes_modified_with_multiline_use_import_is_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // 旧: changed_fn を複数行 grouped use で import し呼び出す caller。
    git_commit_files(
        repo,
        &[
            ("src/target.rs", "pub fn changed_fn() -> i32 {\n    1\n}\n"),
            (
                "src/caller.rs",
                "use crate::target::{\n    changed_fn,\n    other_helper,\n};\n\npub fn other_helper() {}\n\npub fn run() {\n    let _ = changed_fn();\n}\n",
            ),
        ],
        "initial",
    );

    // 新: changed_fn の signature 変更 + 呼び出し更新。grouped use 行は不変。
    let src_dir = repo.join("src");
    fs::write(
        src_dir.join("target.rs"),
        "pub fn changed_fn(x: i32) -> i32 {\n    x\n}\n",
    )
    .expect("write new target");
    fs::write(
            src_dir.join("caller.rs"),
            "use crate::target::{\n    changed_fn,\n    other_helper,\n};\n\npub fn other_helper() {}\n\npub fn run() {\n    let _ = changed_fn(1);\n}\n",
        )
        .expect("write new caller");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/target.rs".to_string(),
            new_path: "src/target.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/caller.rs".to_string(),
            new_path: "src/caller.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 9,
                old_count: 1,
                new_start: 9,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];

    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified_closed_in_diff
            .iter()
            .any(|c| c.name == "changed_fn"),
        "grouped use import + 呼び出し更新済みの signature 変更は mod_closed に降格すべき: {api:?}"
    );
    assert!(
        !api.modified.iter().any(|c| c.name == "changed_fn"),
        "changed_fn を blocking な modified に含めるべきでない: {:?}",
        api.modified
    );
}

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

#[test]
fn is_const_value_only_change_rust_const_value_only_is_true() {
    assert!(is_const_value_only_change(
        "pub const ENEMY_SPEED: f32 = 80.0;",
        "pub const ENEMY_SPEED: f32 = 105.0;",
        "constant",
        crate::language::LangId::Rust,
    ));
}

#[test]
fn is_const_value_only_change_rust_static_value_only_is_true() {
    assert!(is_const_value_only_change(
        "pub static MAX_ALIVE: usize = 200;",
        "pub static MAX_ALIVE: usize = 280;",
        "constant",
        crate::language::LangId::Rust,
    ));
}

#[test]
fn is_const_value_only_change_rust_array_value_only_is_true() {
    assert!(is_const_value_only_change(
        "pub const TABLE: [u8; 3] = [1, 2, 3];",
        "pub const TABLE: [u8; 3] = [4, 5, 6];",
        "constant",
        crate::language::LangId::Rust,
    ));
}

#[test]
fn is_const_value_only_change_rust_static_mut_is_not_demoted() {
    // mutable storage の初期値は状態契約になりやすいため demote しない。
    assert!(!is_const_value_only_change(
        "pub static mut COUNT: usize = 1;",
        "pub static mut COUNT: usize = 2;",
        "constant",
        crate::language::LangId::Rust,
    ));
}

#[test]
fn is_const_value_only_change_rust_type_change_stays_api_mod() {
    // 型変更は shape 変更 → 破壊的の可能性があり api.mod に残す。
    assert!(!is_const_value_only_change(
        "pub const X: f32 = 1.0;",
        "pub const X: f64 = 1.0;",
        "constant",
        crate::language::LangId::Rust,
    ));
}

#[test]
fn is_const_value_only_change_ts_typed_value_only_is_true() {
    assert!(is_const_value_only_change(
        "export const NAME: string = \"a\";",
        "export const NAME: string = \"b\";",
        "variable",
        crate::language::LangId::Typescript,
    ));
}

#[test]
fn is_const_value_only_change_ts_untyped_scalar_is_true() {
    assert!(is_const_value_only_change(
        "export const MAX = 100;",
        "export const MAX = 200;",
        "variable",
        crate::language::LangId::Typescript,
    ));
}

#[test]
fn is_const_value_only_change_ts_untyped_function_stays_api_mod() {
    // 型注釈なし + 関数 initializer は shape 推定が危険なため api.mod に残す。
    assert!(!is_const_value_only_change(
        "export const handler = () => 1;",
        "export const handler = () => 2;",
        "variable",
        crate::language::LangId::Typescript,
    ));
}

#[test]
fn is_const_value_only_change_ts_let_is_not_demoted() {
    assert!(!is_const_value_only_change(
        "export let counter = 1;",
        "export let counter = 2;",
        "variable",
        crate::language::LangId::Typescript,
    ));
}

#[test]
fn is_const_value_only_change_non_binding_kind_is_false() {
    assert!(!is_const_value_only_change(
        "fn foo() -> i32",
        "fn foo() -> u32",
        "function",
        crate::language::LangId::Rust,
    ));
}

/// Rust の `pub const` / `pub static` の値 (initializer) のみ変更は破壊的でないため、
/// blocking な `modified` ではなく informational な `const_value_changes` に振り分けられる
/// (Issue 2026-06-02-balance-const-value-changes 回帰防止)。
#[test]
fn detect_api_changes_rust_const_value_only_is_demoted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/constants.rs",
            "pub const ENEMY_SPEED: f32 = 80.0;\npub static MAX_ALIVE: usize = 200;\n",
        )],
        "initial",
    );
    // 値のみ変更 (shape 不変)
    fs::write(
        repo.join("src/constants.rs"),
        "pub const ENEMY_SPEED: f32 = 105.0;\npub static MAX_ALIVE: usize = 280;\n",
    )
    .expect("write new constants");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/constants.rs".to_string(),
        new_path: "src/constants.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified.is_empty(),
        "値のみ変更の const/static は blocking modified に出すべきでない: {:?}",
        api.modified
    );
    assert!(
        api.const_value_changes
            .iter()
            .any(|c| c.name == "ENEMY_SPEED"),
        "const ENEMY_SPEED の値変更は const_value_changes に出すべき: {:?}",
        api.const_value_changes
    );
    assert!(
        api.const_value_changes
            .iter()
            .any(|c| c.name == "MAX_ALIVE"),
        "static MAX_ALIVE の値変更は const_value_changes に出すべき: {:?}",
        api.const_value_changes
    );
}

/// `pub const` の型変更 (shape 変更) は const_value_changes ではなく従来どおり
/// blocking な modified に残す。
#[test]
fn detect_api_changes_rust_const_type_change_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("src/constants.rs", "pub const LIMIT: u32 = 10;\n")],
        "initial",
    );
    fs::write(
        repo.join("src/constants.rs"),
        "pub const LIMIT: u64 = 10;\n",
    )
    .expect("write new constants");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/constants.rs".to_string(),
        new_path: "src/constants.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified.iter().any(|c| c.name == "LIMIT"),
        "型変更は blocking modified に残すべき: {api:?}"
    );
    assert!(
        api.const_value_changes.is_empty(),
        "型変更は const_value_changes に入れるべきでない: {:?}",
        api.const_value_changes
    );
}

#[test]
fn detect_api_changes_uses_old_path_for_renamed_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo)
            .status()
            .expect("git init")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "astro-sight-tests"])
            .current_dir(repo)
            .status()
            .expect("git config user.name")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "astro-sight@example.com"])
            .current_dir(repo)
            .status()
            .expect("git config user.email")
            .success()
    );

    let old_path = src_dir.join("old.rs");
    fs::write(&old_path, "pub fn greet() -> i32 {\n    1\n}\n").expect("write old file");

    assert!(
        Command::new("git")
            .args(["add", "."])
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

    let new_path = src_dir.join("new.rs");
    fs::rename(&old_path, &new_path).expect("rename file");
    fs::write(
        &new_path,
        "pub fn greet(name: &str) -> i32 {\n    name.len() as i32\n}\n",
    )
    .expect("write renamed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/old.rs".to_string(),
        new_path: "src/new.rs".to_string(),
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
        api_changes
            .modified
            .iter()
            .any(|change| change.name == "greet"
                && change.old_signature.as_deref() == Some("pub fn greet() -> i32")
                && change.new_signature.as_deref() == Some("pub fn greet(name: &str) -> i32")),
        "rename を含む差分でも関数シグネチャ変更を検出するべき"
    );
}

/// 宣言の先頭行が同一でも、複数行に跨る引数列が変わった場合は modified として
/// 検出される (Issue 2026-05-14-rename-and-multiline-signature の 3a)。
/// 旧実装は先頭行のみを signature に使っており、引数列が増えても先頭行
/// (`pub fn foo<F>(`) が同じだと false negative になっていた。
#[test]
fn detect_api_changes_modified_includes_multiline_signature_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = "pub fn foo<F>(\n    diff: &str,\n    dir: &str,\n    cb: F,\n) -> Result<(), String>\nwhere\n    F: FnMut() -> Result<(), String>,\n{\n    Ok(())\n}\n";
    fs::write(src_dir.join("foo.rs"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.rs"])
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

    // 引数を 1 つ追加した版 (先頭行 `pub fn foo<F>(` は base と完全一致)
    let after = "pub fn foo<F>(\n    diff: &str,\n    dir: &str,\n    options: &Options,\n    cb: F,\n) -> Result<(), String>\nwhere\n    F: FnMut() -> Result<(), String>,\n{\n    Ok(())\n}\n";
    fs::write(src_dir.join("foo.rs"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.rs".to_string(),
        new_path: "src/foo.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 9,
            new_start: 1,
            new_count: 10,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let foo_change = api_changes
        .modified
        .iter()
        .find(|c| c.name == "foo")
        .expect("foo は multi-line signature の変更で modified に出るべき");
    assert!(
        foo_change
            .old_signature
            .as_deref()
            .map(|s| s.contains("diff: &str") && !s.contains("options"))
            .unwrap_or(false),
        "old_signature は base の引数列のみ含むべき: {:?}",
        foo_change.old_signature
    );
    assert!(
        foo_change
            .new_signature
            .as_deref()
            .map(|s| s.contains("options: &Options"))
            .unwrap_or(false),
        "new_signature は追加された options 引数を含むべき: {:?}",
        foo_change.new_signature
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

/// TSX 関数コンポーネントの destructured props に optional prop を追加するだけの
/// React 後方互換変更は api.mod に出してはならない (Issue
/// 引数なし TS/TSX 関数に、`= {}` default 付きの destructured props を追加する
/// 後方互換変更は api.mod に出してはならない (Issue
/// 2026-05-28-meet-virtual-you-frontend-modernize 対応)。
#[test]
fn detect_api_changes_tsx_no_args_to_destructured_with_default_value_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function TemplateManager() {\n",
        "  return null;\n",
        "}\n"
    );
    fs::write(src_dir.join("TemplateManager.tsx"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/TemplateManager.tsx"])
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

    // 引数なし → destructured props + `= {}` default 付き (省略可能)
    let after = concat!(
        "interface TemplateManagerProps {\n",
        "  onSaved?: (message: string) => void;\n",
        "}\n",
        "export function TemplateManager({ onSaved }: TemplateManagerProps = {}) {\n",
        "  onSaved?.(\"ok\");\n",
        "  return null;\n",
        "}\n"
    );
    fs::write(src_dir.join("TemplateManager.tsx"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/TemplateManager.tsx".to_string(),
        new_path: "src/TemplateManager.tsx".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"TemplateManager"),
        "default `= {{}}` 付きの destructured props 追加は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// 引数なし TS/TSX 関数に、destructured props を追加 (default なし) するが
/// 型注釈の `interface` が同一ファイル内で全 optional な場合、省略可能と
/// 判定して api.mod に出してはならない。
#[test]
fn detect_api_changes_tsx_no_args_to_destructured_with_all_optional_interface_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function SpeakerNameSetting() {\n",
        "  return null;\n",
        "}\n"
    );
    fs::write(src_dir.join("SpeakerNameSetting.tsx"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/SpeakerNameSetting.tsx"])
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

    // 引数なし → destructured props + 同一ファイル内 interface (全 optional)
    let after = concat!(
        "interface SpeakerNameSettingProps {\n",
        "  onSaved?: (message: string) => void;\n",
        "}\n",
        "export function SpeakerNameSetting({ onSaved }: SpeakerNameSettingProps) {\n",
        "  onSaved?.(\"ok\");\n",
        "  return null;\n",
        "}\n"
    );
    fs::write(src_dir.join("SpeakerNameSetting.tsx"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/SpeakerNameSetting.tsx".to_string(),
        new_path: "src/SpeakerNameSetting.tsx".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"SpeakerNameSetting"),
        "同一ファイル内 interface が全 optional なら destructured props 追加は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// 引数なし TS/TSX 関数に、destructured props を追加 (default なし) し、型注釈の
/// inline object type に required field を含む場合は破壊的変更として
/// api.mod に残すべき (副作用回帰防止)。
#[test]
fn detect_api_changes_tsx_no_args_to_destructured_with_required_field_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
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

    // 引数なし → required field を含む inline object type の destructured props
    let after = concat!(
        "export function widget({ name }: { name: string }) {\n",
        "  return name;\n",
        "}\n",
        "export function caller() { return widget({ name: \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "required field を持つ inline object type の destructured props 追加は api.mod に残すべき。got: {mod_names:?}"
    );
}

/// 引数なし TS/TSX 関数に、destructured props を追加 (default なし) し、型注釈が
/// import 型 (同一ファイル内に declaration なし) の場合は省略可能と断定できない
/// ため api.mod に残すべき (副作用回帰防止)。
#[test]
fn detect_api_changes_tsx_no_args_to_destructured_with_imported_type_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
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

    // 引数なし → 同一ファイルに declaration がない type identifier
    let after = concat!(
        "import type { WidgetProps } from './props';\n",
        "export function widget({ name }: WidgetProps) {\n",
        "  return name;\n",
        "}\n",
        "export function caller() { return widget({ name: \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "import 型 (同ファイル内 declaration なし) の destructured props 追加は省略可能と断定できないので api.mod に残すべき。got: {mod_names:?}"
    );
}

/// TS 関数 destructured params の型注釈 (inline object type) で optional field の
/// 型を変更した場合 (`{ x?: string }` → `{ x?: number }`) は呼び出し側に見える
/// 型契約変更なので api.mod に残すべき。「省略可能 destructured を `()` と
/// 同一視する」過剰正規化を防ぐ codex 指摘 1 への回帰防止。
#[test]
fn detect_api_changes_tsx_optional_field_type_change_in_destructured_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function foo({ x }: { x?: string }): string {\n",
        "  return x ?? \"a\";\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), before).expect("write before");
    fs::write(
        src_dir.join("caller.ts"),
        "import { foo } from './foo';\nexport const x = foo({ x: 'a' });\n",
    )
    .expect("write caller");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.ts", "src/caller.ts"])
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

    // optional field の型変更 (string → number)
    let after = concat!(
        "export function foo({ x }: { x?: number }): string {\n",
        "  return String(x ?? 0);\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.ts".to_string(),
        new_path: "src/foo.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"foo"),
        "optional field の型変更は呼び出し側型契約変更なので api.mod に残すべき。got: {mod_names:?}"
    );
}

/// `interface Props extends Base { ... }` で body のフィールドが全 optional でも、
/// base interface が required field を持つ可能性があるため省略可能扱いしない
/// (codex 指摘 2 への回帰防止)。
#[test]
fn detect_api_changes_tsx_interface_with_extends_clause_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
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

    // interface に extends を付けて props を追加 (body は optional だが base が不明)
    let after = concat!(
        "interface BaseProps {\n",
        "  required: string;\n",
        "}\n",
        "interface WidgetProps extends BaseProps {\n",
        "  optional?: number;\n",
        "}\n",
        "export function widget({ optional }: WidgetProps) {\n",
        "  return optional;\n",
        "}\n",
        "export function caller() { return widget({ required: \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 10,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "extends 持ち interface は base 側の required field を否定できないので api.mod に残すべき。got: {mod_names:?}"
    );
}

/// 同名 interface declaration merge で、片方が required field を含む場合、
/// 全体としては省略可能ではないので api.mod に残すべき (codex 指摘 3 への
/// 回帰防止)。
#[test]
fn detect_api_changes_tsx_interface_merge_with_required_field_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
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

    // 同名 interface 宣言が 2 つあり、片方は optional のみ、もう片方は required あり
    let after = concat!(
        "interface WidgetProps {\n",
        "  optional?: number;\n",
        "}\n",
        "interface WidgetProps {\n",
        "  required: string;\n",
        "}\n",
        "export function widget({ optional }: WidgetProps) {\n",
        "  return optional;\n",
        "}\n",
        "export function caller() { return widget({ required: \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 10,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "同名 interface merge で required field があれば省略可能ではないので api.mod に残すべき。got: {mod_names:?}"
    );
}

/// `"name?": string` のような string property name の `?` を optional マーカーと
/// 誤判定しないこと (codex 指摘 4 への回帰防止)。required field を含む型注釈
/// なので api.mod に残るべき。
#[test]
fn detect_api_changes_tsx_string_property_name_with_question_mark_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
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

    // string property name の中に `?` を含む required field を持つ inline object type
    let after = concat!(
        "export function widget(props: { \"name?\": string }) {\n",
        "  return props[\"name?\"];\n",
        "}\n",
        "export function caller() { return widget({ \"name?\": \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "string property name `\"name?\"` の `?` は optional マーカーではなく required field のはず。api.mod に残すべき。got: {mod_names:?}"
    );
}

/// 旧関数が型注釈内に同名の call signature を含む場合でも、AST で旧 parameters を
/// 検査して誤判定しないこと (codex 指摘 5 への回帰防止)。旧 sig 文字列に
/// `foo()` という部分文字列が含まれても、実際の関数 foo は引数を取るので
/// api.mod に残るべき。
#[test]
fn detect_api_changes_tsx_old_signature_contains_inline_call_signature_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    // 旧: 引数あり (引数の型注釈に foo() という inline call signature を含む)
    let before = concat!(
        "export function foo(arg: { foo(): void }) {\n",
        "  arg.foo();\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { foo } from './foo';\nexport const x = foo({ foo: () => {} });\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.ts", "src/user.ts"])
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

    // 新: 引数を destructured + 型注釈に optional のみの inline object に変更
    let after = concat!(
        "export function foo({ x }: { x?: string }) {\n",
        "  return x ?? \"a\";\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.ts".to_string(),
        new_path: "src/foo.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"foo"),
        "旧関数が引数を取る場合は (型注釈内 call signature があっても) api.mod に残すべき。got: {mod_names:?}"
    );
}

/// ネストしたローカル同名関数を拾わないこと (codex 指摘 6 への回帰防止)。
/// 変更対象の exported 関数 widget は required props だが、関数内ネストに
/// 同名 widget があり optional だとしても、トップレベル限定の判定で
/// api.mod に残すべき。
#[test]
fn detect_api_changes_tsx_nested_local_function_does_not_override_top_level_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function widget() {\n",
        "  return null;\n",
        "}\n",
        "export function caller() { return widget(); }\n"
    );
    fs::write(src_dir.join("widget.ts"), before).expect("write before");
    fs::write(
        src_dir.join("user.ts"),
        "import { widget } from './widget';\nexport const x = widget();\n",
    )
    .expect("write user");
    assert!(
        Command::new("git")
            .args(["add", "src/widget.ts", "src/user.ts"])
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

    // 新: トップレベル widget は required props、ネスト widget は optional のみ
    let after = concat!(
        "export function widget({ required }: { required: string }) {\n",
        "  function widget({ optional }: { optional?: string }) {\n",
        "    return optional;\n",
        "  }\n",
        "  return widget({});\n",
        "}\n",
        "export function caller() { return widget({ required: \"x\" }); }\n"
    );
    fs::write(src_dir.join("widget.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"widget"),
        "トップレベル widget は required props なので、ネスト同名関数に惑わされず api.mod に残すべき。got: {mod_names:?}"
    );
}

/// TSX 関数コンポーネントの destructured props に optional prop を追加するだけの
/// React 後方互換変更は api.mod に出してはならない (Issue
/// 2026-05-28-api-mod-optional-props-additive 対応)。
#[test]
fn detect_api_changes_tsx_destructured_props_optional_addition_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export interface Props { templates: string[]; onSelect: (s: string) => void; className?: string }\n",
        "export function PromptTemplateSelector({ templates, onSelect, className = \"\" }: Props) {\n",
        "  return templates;\n",
        "}\n"
    );
    fs::write(src_dir.join("Selector.tsx"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/Selector.tsx"])
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

    // interface に optional prop を追加し、関数の destructure 受け取りも追加。
    // 型注釈 `: Props` 自体は不変。
    let after = concat!(
        "export interface Props { templates: string[]; onSelect: (s: string) => void; className?: string; useExistingContent?: boolean; onChange?: (v: boolean) => void }\n",
        "export function PromptTemplateSelector({ templates, onSelect, className = \"\", useExistingContent = false, onChange }: Props) {\n",
        "  return templates;\n",
        "}\n"
    );
    fs::write(src_dir.join("Selector.tsx"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/Selector.tsx".to_string(),
        new_path: "src/Selector.tsx".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"PromptTemplateSelector"),
        "TSX destructured params の optional 受け取り追加は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// TS 関数の destructured params のデフォルト値変更は signature 不変として扱う
/// (caller-visible な型契約ではなく binding 時の挙動変更)。
#[test]
fn detect_api_changes_typescript_destructured_default_value_change_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export interface Opts { x?: number }\n",
        "export function foo({ x = 0 }: Opts) {\n",
        "  return x;\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), before).expect("write before");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.ts"])
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

    let after = concat!(
        "export interface Opts { x?: number }\n",
        "export function foo({ x = 42 }: Opts) {\n",
        "  return x;\n",
        "}\n"
    );
    fs::write(src_dir.join("foo.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.ts".to_string(),
        new_path: "src/foo.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"foo"),
        "destructured params の default value 変更は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// TS 関数の positional 引数追加は destructure ではなく直接の呼び出し契約変更なので
/// api.mod に残す (destructure normalize の副作用回帰防止)。
#[test]
fn detect_api_changes_typescript_positional_param_added_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function foo(a: number): number {\n",
        "  return a;\n",
        "}\n",
        "export function bar() { return foo(1); }\n"
    );
    fs::write(src_dir.join("foo.ts"), before).expect("write before");
    // 他ファイルからの cross-file 参照を作って closed-in-diff で抑制されないようにする。
    fs::write(
        src_dir.join("caller.ts"),
        "import { foo } from './foo';\nexport const x = foo(1);\n",
    )
    .expect("write caller");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.ts", "src/caller.ts"])
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

    let after = concat!(
        "export function foo(a: number, b: number): number {\n",
        "  return a + b;\n",
        "}\n",
        "export function bar() { return foo(1, 2); }\n"
    );
    fs::write(src_dir.join("foo.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.ts".to_string(),
        new_path: "src/foo.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"foo"),
        "positional 引数追加は destructure ではないので api.mod に残すべき。got: {mod_names:?}"
    );
}

/// TS 関数 destructured params の **inline object type** 注釈変更は signature 変更として
/// 残す (型注釈側は呼び出し側に見える契約)。destructure normalize が type_annotation
/// に踏み込まないことの回帰防止。
#[test]
fn detect_api_changes_typescript_inline_object_type_change_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let src_dir = repo.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");

    let before = concat!(
        "export function foo({ x }: { x: string }): string {\n",
        "  return x;\n",
        "}\n",
        "export function bar() { return foo({ x: 'a' }); }\n"
    );
    fs::write(src_dir.join("foo.ts"), before).expect("write before");
    fs::write(
        src_dir.join("caller.ts"),
        "import { foo } from './foo';\nexport const x = foo({ x: 'a' });\n",
    )
    .expect("write caller");
    assert!(
        Command::new("git")
            .args(["add", "src/foo.ts", "src/caller.ts"])
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

    // inline object type に required な y フィールドを追加 (breaking)
    let after = concat!(
        "export function foo({ x, y }: { x: string; y: number }): string {\n",
        "  return x + y;\n",
        "}\n",
        "export function bar() { return foo({ x: 'a', y: 1 }); }\n"
    );
    fs::write(src_dir.join("foo.ts"), after).expect("write after");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.ts".to_string(),
        new_path: "src/foo.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        mod_names.contains(&"foo"),
        "inline object type 注釈の構造変更は api.mod に残すべき。got: {mod_names:?}"
    );
}

/// テストヘルパー: 一時 git リポジトリを初期化する。
fn init_git_repo_for_test(repo: &std::path::Path) {
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.name", "astro-sight-tests"],
        vec!["config", "user.email", "astro-sight@example.com"],
    ] {
        assert!(
            Command::new("git")
                .args(&args)
                .current_dir(repo)
                .status()
                .expect("git")
                .success()
        );
    }
}

/// テストヘルパー: 与えられたファイル一覧を書き込み、add + commit する。
fn git_commit_files(repo: &std::path::Path, files: &[(&str, &str)], msg: &str) {
    for (rel, content) in files {
        let full = repo.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(full, content).expect("write file");
    }
    assert!(
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(repo)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(repo)
            .status()
            .expect("git commit")
            .success()
    );
}

// --- git worktree 判定 & 非 git ディレクトリの graceful skip ---

#[test]
fn is_git_work_tree_true_inside_repo() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_git_repo_for_test(dir.path());
    assert!(
        is_git_work_tree(dir.path().to_str().expect("utf-8")).expect("rev-parse"),
        "git init 済み dir は worktree 内"
    );
}

#[test]
fn is_git_work_tree_false_outside_repo() {
    // git init しない一時 dir は管理外。
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(
        !is_git_work_tree(dir.path().to_str().expect("utf-8")).expect("rev-parse"),
        "git 管理外 dir は Ok(false)"
    );
}

#[test]
fn resolve_git_diff_skips_non_git_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    match resolve_git_diff(dir.path().to_str().expect("utf-8"), "HEAD", false).expect("resolve") {
        GitDiffInput::Skipped(skip) => {
            assert_eq!(skip.reason.as_str(), "not_git_repository");
            assert_eq!(skip.source.as_str(), "git");
        }
        GitDiffInput::Diff(_) => panic!("非 git dir では Skipped を返すべき"),
    }
}

#[test]
fn resolve_git_diff_rejects_invalid_base_even_when_non_git() {
    // base 不正は git 管理外でも入力契約違反として弾く (skip より優先)。
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(
        resolve_git_diff(dir.path().to_str().expect("utf-8"), "-x", false).is_err(),
        "先頭 '-' の base は非 git でも Err"
    );
}

/// unstaged (`--git`、非 `--staged`) では未追跡の新規ソースファイルを「全行追加の
/// 新規ファイル」として diff に合成する。git diff は仕様上 untracked を出さないため、
/// これが無いと「同一作業で作成した未追跡 sibling への参照」が未解決影響と誤報される。
/// 非ソース (拡張子で言語判定不可) と .gitignore 対象は合成しない。
#[test]
fn run_git_diff_unstaged_includes_untracked_source_excludes_binary_and_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("src/lib.rs", "pub fn existing() {}\n"),
            (".gitignore", "ignored.rs\n"),
        ],
        "initial",
    );

    // 未追跡: ソース (含む) / 非ソース (除外) / gitignore 対象 (除外)。
    fs::write(repo.join("src/new_helper.rs"), "pub fn helper() {}\n").expect("write untracked src");
    fs::write(repo.join("data.bin"), "binarylike\n").expect("write untracked bin");
    fs::write(repo.join("ignored.rs"), "pub fn ignored() {}\n").expect("write ignored");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", false).expect("diff");

    assert!(
        diff.contains("+++ b/src/new_helper.rs") && diff.contains("+pub fn helper() {}"),
        "未追跡の新規ソースは全行追加の新規ファイルとして合成されるべき: {diff}"
    );
    assert!(
        !diff.contains("data.bin"),
        "非ソース (言語判定不可) は合成しない: {diff}"
    );
    assert!(
        !diff.contains("ignored.rs"),
        ".gitignore 対象 (--exclude-standard) は合成しない: {diff}"
    );
}

/// staged モード (`--git --staged`) では未追跡を合成しない (index にある変更のみを尊重)。
#[test]
fn run_git_diff_staged_excludes_untracked_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("src/lib.rs", "pub fn existing() {}\n")], "initial");
    fs::write(repo.join("src/new_helper.rs"), "pub fn helper() {}\n").expect("write untracked src");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", true).expect("diff");
    assert!(
        !diff.contains("new_helper.rs"),
        "staged モードでは未追跡を合成すべきでない: {diff}"
    );
}

/// untracked 新規ファイルが diff の「解決済み範囲」に入ることを end-to-end で確認する。
/// run_git_diff (unstaged) の出力を parse_unified_diff にかけ、未追跡ファイルが
/// new_path を持つ DiffFile として現れることを検証する (impact 誤検出
/// 2026-06-12-untracked-new-file-impact の回帰防止)。
#[test]
fn impact_includes_untracked_new_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("src/lib.rs", "pub fn existing() {}\n")], "initial");

    // 既存ファイルの可視性変更 (tracked diff) + 参照を含む未追跡 sibling (report の再現パターン)。
    fs::write(repo.join("src/lib.rs"), "pub(crate) fn existing() {}\n").expect("modify tracked");
    fs::write(
        repo.join("src/provider.rs"),
        "use crate::existing;\npub fn run() { existing(); }\n",
    )
    .expect("write untracked sibling");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", false).expect("diff");
    let files = crate::engine::diff::parse_unified_diff(&diff);
    assert!(
        files.iter().any(|f| f.new_path == "src/provider.rs"),
        "未追跡 sibling が DiffFile (解決済み範囲) に含まれるべき: {:?}",
        files.iter().map(|f| &f.new_path).collect::<Vec<_>>()
    );
    assert!(
        files.iter().any(|f| f.new_path == "src/lib.rs"),
        "tracked 変更も従来通り含まれるべき"
    );
}

/// 未追跡 rename (A1): tracked file を削除 (unstaged) + 内容同一の untracked sibling を追加
/// すると、high-confidence rename として正規化され api.add / api.rm が出ない。さらに内容同一の
/// 場合は Modified diff を合成しない (hunkless = commit 済みの 100% rename と一致) ので、
/// 削除元・rename 先のどちらも DiffFile に現れず、dead-code / cochange も commit 済みと一致する。
#[test]
fn run_git_diff_untracked_rename_normalizes_and_emits_no_add_or_rm() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/mod_a.rs",
            "pub fn foo(name: &str) -> String {\n    name.to_uppercase()\n}\n",
        )],
        "initial",
    );

    // mod_a.rs を削除 (unstaged) + 内容同一の mod_b.rs を untracked 追加 = rename。
    fs::remove_file(repo.join("src/mod_a.rs")).expect("remove old");
    fs::write(
        repo.join("src/mod_b.rs"),
        "pub fn foo(name: &str) -> String {\n    name.to_uppercase()\n}\n",
    )
    .expect("write new");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", false).expect("diff");
    let diff_files = crate::engine::diff::parse_unified_diff(&diff);

    let api = detect_api_changes(repo.to_str().expect("utf-8"), "HEAD", &diff_files);
    assert!(
        api.added.is_empty(),
        "rename で api.add は出ない: {:?}",
        api.added
    );
    assert!(
        api.removed.is_empty(),
        "rename で api.rm は出ない: {:?}",
        api.removed
    );

    // 内容同一 rename は Modified diff を合成しない → rename 先も削除元も DiffFile に出ない
    // (commit 済みの hunkless 100% rename と一致し、dead-code / cochange の乖離を防ぐ)。
    assert!(
        !diff_files.iter().any(|f| f.new_path == "src/mod_b.rs"),
        "内容同一 rename 先は DiffFile に出ない: {:?}",
        diff_files.iter().map(|f| &f.new_path).collect::<Vec<_>>()
    );
    assert!(
        !diff_files.iter().any(|f| f.old_path == "src/mod_a.rs"),
        "rename 元の Deleted block は除去される: {:?}",
        diff_files.iter().map(|f| &f.old_path).collect::<Vec<_>>()
    );
}

/// 未追跡の symlink は合成しない (パス境界)。symlink_metadata でリンク自身を見て
/// regular file 以外を除外するため、外部のソースファイルを指す symlink でも内容が
/// 合成 diff に漏れない (codex レビュー指摘のセキュリティ境界)。
#[cfg(unix)]
#[test]
fn run_git_diff_unstaged_skips_untracked_symlink() {
    let outside = tempfile::tempdir().expect("outside tempdir");
    let target = outside.path().join("secret.rs");
    fs::write(&target, "pub fn secret() {}\n").expect("write external target");

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("src/lib.rs", "pub fn existing() {}\n")], "initial");
    // 外部のソースファイルを指す未追跡 symlink (ソース拡張子)。
    std::os::unix::fs::symlink(&target, repo.join("link.rs")).expect("symlink");

    let diff =
        crate::commands::run_git_diff(repo.to_str().expect("utf-8"), "HEAD", false).expect("diff");
    assert!(
        !diff.contains("link.rs") && !diff.contains("pub fn secret"),
        "未追跡 symlink (外部ソースを指す) は合成すべきでない: {diff}"
    );
}

#[test]
fn resolve_blame_source_files_skips_non_git_without_explicit_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    match resolve_blame_source_files(
        dir.path().to_str().expect("utf-8"),
        true,
        None,
        None,
        None,
        &[],
    )
    .expect("resolve")
    {
        BlameSourceResolution::Skipped(skip) => {
            assert_eq!(skip.reason.as_str(), "not_git_repository");
        }
        BlameSourceResolution::Files(f) => panic!("非 git + 明示 paths 無しは Skipped: {f:?}"),
    }
}

#[test]
fn resolve_blame_source_files_keeps_explicit_paths_when_non_git() {
    // 管理外でも --paths 明示があれば skip せず明示分を返す (明示優先)。
    let dir = tempfile::tempdir().expect("tempdir");
    match resolve_blame_source_files(
        dir.path().to_str().expect("utf-8"),
        true,
        None,
        Some("a.rs,b.rs"),
        None,
        &[],
    )
    .expect("resolve")
    {
        BlameSourceResolution::Files(f) => {
            assert!(f.contains(&"a.rs".to_string()));
            assert!(f.contains(&"b.rs".to_string()));
        }
        BlameSourceResolution::Skipped(_) => panic!("明示 paths があれば skip しない"),
    }
}

#[test]
fn detect_api_changes_rename_preserves_symbols() {
    // Python スクリプトを rename した際、同名・同シグネチャの関数は
    // api.rm / api.add として報告されないことを確認する（レポートの再現シナリオ）。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let old_content = "\
def iter_plugin_manifests():
    return []

def check_layout():
    return 0

def build_entries():
    return []

def regenerate():
    return None

def main():
    pass
";
    git_commit_files(
        repo,
        &[("scripts/regenerate_marketplace.py", old_content)],
        "initial",
    );

    // 旧ファイル削除 + 新ファイル追加 (git mv と同じ効果)
    fs::remove_file(repo.join("scripts/regenerate_marketplace.py")).expect("rm old");
    let new_content = "\
def iter_plugin_manifests():
    return []

def check_layout():
    return 0

def build_entries():
    return []

def regenerate():
    return None

def main():
    pass
";
    fs::write(repo.join("scripts/marketplace.py"), new_content).expect("write new");

    // git の rename detection で単一 DiffFile として扱われる場合
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "scripts/regenerate_marketplace.py".to_string(),
        new_path: "scripts/marketplace.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 14,
            new_start: 1,
            new_count: 14,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        added.is_empty(),
        "rename で保持された関数は api.add に出るべきではない。got: {added:?}"
    );
    assert!(
        removed.is_empty(),
        "rename で保持された関数は api.rm に出るべきではない。got: {removed:?}"
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

/// 全 cross-file 参照が同一 diff 内の変更 hunk で追随済みの api.mod は
/// modified_closed_in_diff (informational) に降格する (パターンA)。
#[test]
fn detect_api_changes_modified_with_all_callers_in_diff_is_closed() {
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
            ("src/lib.rs", "pub mod detector;\npub mod manager;\n"),
            (
                "src/detector.rs",
                "pub fn create_detector(id: u32) -> u32 {\n    id\n}\n",
            ),
            (
                "src/manager.rs",
                "use crate::detector::create_detector;\npub fn run() -> u32 {\n    create_detector(1)\n}\n",
            ),
        ],
        "base",
    );
    // create_detector に引数追加 + caller (manager.rs) を同一 diff で追随更新
    fs::write(
        repo.join("src/detector.rs"),
        "pub fn create_detector(id: u32, extra: bool) -> u32 {\n    id\n}\n",
    )
    .expect("write");
    fs::write(
            repo.join("src/manager.rs"),
            "use crate::detector::create_detector;\npub fn run() -> u32 {\n    create_detector(1, true)\n}\n",
        )
        .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/detector.rs".to_string(),
            new_path: "src/detector.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/manager.rs".to_string(),
            new_path: "src/manager.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("create_detector")),
        "全 caller が同一 diff 内なら modified_closed_in_diff に降格すべき。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
    assert!(
        !api.modified
            .iter()
            .any(|m| m.name.ends_with("create_detector")),
        "closed-in-diff は blocking な modified に残さない"
    );
}

/// caller が diff 外 (変更 hunk に含まれない) に残る api.mod は blocking な modified のまま。
#[test]
fn detect_api_changes_modified_with_caller_outside_diff_stays_modified() {
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
            ("src/lib.rs", "pub mod detector;\npub mod manager;\n"),
            (
                "src/detector.rs",
                "pub fn create_detector(id: u32) -> u32 {\n    id\n}\n",
            ),
            (
                "src/manager.rs",
                "use crate::detector::create_detector;\npub fn run() -> u32 {\n    create_detector(1)\n}\n",
            ),
        ],
        "base",
    );
    // detector.rs のみシグネチャ変更。manager.rs (caller) は未更新かつ diff にも含めない。
    fs::write(
        repo.join("src/detector.rs"),
        "pub fn create_detector(id: u32, extra: bool) -> u32 {\n    id\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/detector.rs".to_string(),
        new_path: "src/detector.rs".to_string(),
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
        api.modified
            .iter()
            .any(|m| m.name.ends_with("create_detector")),
        "diff 外に未更新 caller が残る場合は blocking な modified に残すべき。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// TS で export 関数と同名のローカル関数が別ファイルにあっても、shadow 解決で
/// ローカル呼び出しを除外し、対象 caller (複数行呼び出しの引数内のみ変更) が追随済みなら
/// closed-in-diff に降格する (Issue 2026-07-12-api-mod-same-diff-informational の完全再現)。
#[test]
fn detect_api_changes_ts_shadowed_local_fn_and_multiline_call_is_closed_in_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n    audio: boolean;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            (
                "src/tap.ts",
                "function startRecording(p: number): number {\n    return p * 2;\n}\nwindow.addEventListener(\"message\", () => {\n    const res = startRecording(1);\n    console.log(res);\n});\n",
            ),
            (
                "src/content.ts",
                "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({\n        fps: 30,\n        audio: true,\n    });\n}\n",
            ),
        ],
        "base",
    );
    // capture.ts: シグネチャに cursor を追加 / content.ts: 複数行呼び出しの引数内にだけ
    // 追随行を追加 (識別子行 `startRecording({` 自体は未変更) / tap.ts は触らない。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    audio: boolean;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({\n        fps: 30,\n        audio: true,\n        cursor: true,\n    });\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib/capture.ts".to_string(),
            new_path: "src/lib/capture.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 6,
                new_start: 1,
                new_count: 7,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/content.ts".to_string(),
            new_path: "src/content.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 4,
                old_count: 4,
                new_start: 4,
                new_count: 5,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "同名ローカル関数の shadow 除外 + 複数行呼び出しの引数内変更で closed に降格すべき。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
    assert!(
        !api.modified
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "closed-in-diff は blocking な modified に残さない"
    );
}

/// 同名ローカル関数の shadow があっても、対象 caller が diff 外なら従来どおり blocking。
#[test]
fn detect_api_changes_ts_shadowed_local_fn_with_caller_outside_diff_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            (
                "src/tap.ts",
                "function startRecording(p: number): number {\n    return p * 2;\n}\nwindow.addEventListener(\"message\", () => {\n    const res = startRecording(1);\n    console.log(res);\n});\n",
            ),
            (
                "src/content.ts",
                "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30 });\n}\n",
            ),
        ],
        "base",
    );
    // capture.ts のみ変更。content.ts (対象 caller) は未更新かつ diff 外。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/capture.ts".to_string(),
        new_path: "src/lib/capture.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "対象 caller が diff 外なら blocking な modified のまま。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// switch body 内の block-level 同名関数は外側の呼び出しを shadow しない: 更新済み caller と
/// 「switch 内同名関数を持つファイルの未更新 caller」が併存する場合、後者を shadow 除外せず
/// blocking を維持する (codex レビュー指摘の switch_body scope 対応)。
#[test]
fn detect_api_changes_ts_switch_scoped_fn_does_not_shadow_unupdated_caller() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            // switch 内に block-level 同名関数 + その外に対象 API への未更新呼び出し
            (
                "src/tap.ts",
                "import { startRecording } from \"./lib/capture\";\nexport function caller(x: number) {\n    switch (x) {\n        case 1:\n            function startRecording() {}\n    }\n    return startRecording({ fps: 30 });\n}\n",
            ),
            (
                "src/content.ts",
                "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30 });\n}\n",
            ),
        ],
        "base",
    );
    // 対象 API 変更 + content.ts (更新済み caller) のみ追随。tap.ts の呼び出しは未更新・diff 外。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30, cursor: true });\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib/capture.ts".to_string(),
            new_path: "src/lib/capture.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 5,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/content.ts".to_string(),
            new_path: "src/content.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 4,
                old_count: 1,
                new_start: 4,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "switch 内関数は shadow にならず、未更新 caller (tap.ts) がある限り blocking 維持。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// for-of の loop 変数は for scope の binding: 同名 loop 変数経由の呼び出し (中身は alias
/// import した対象 API かもしれない) を、外側の同名ローカル関数への束縛と誤解決して shadow
/// 除外しない — 未追随の可能性がある限り blocking を維持する (for ヘッダ binding 対応)。
#[test]
fn detect_api_changes_ts_for_of_loop_variable_does_not_shadow_unupdated_caller() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            // ローカル同名関数 + alias import した対象 API を loop 変数 (同名) 経由で呼ぶ。
            // loop 変数への呼び出しはローカル関数に束縛されない (for scope の binding)。
            (
                "src/tap.ts",
                "import { startRecording as rec } from \"./lib/capture\";\nfunction startRecording() {}\nexport function caller() {\n    for (const startRecording of [rec]) {\n        startRecording({ fps: 30 });\n    }\n    return startRecording;\n}\n",
            ),
            (
                "src/content.ts",
                "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30 });\n}\n",
            ),
        ],
        "base",
    );
    // 対象 API 変更 + content.ts (更新済み caller) のみ追随。tap.ts の loop 変数呼び出しは
    // 未更新・diff 外のまま。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30, cursor: true });\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib/capture.ts".to_string(),
            new_path: "src/lib/capture.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 5,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/content.ts".to_string(),
            new_path: "src/content.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 4,
                old_count: 1,
                new_start: 4,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "loop 変数呼び出しは shadow 除外せず、未更新 caller (tap.ts) がある限り blocking 維持。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// shadow 除外で全参照が消える (対象 API 自体は未使用で、別ファイルの同名ローカル関数と
/// その呼び出ししか無い) 場合は closed にしない — 対象 caller の追随を 1 件も確認して
/// いないため blocking を維持する (codex レビュー指摘の fail-open 回帰テスト)。
#[test]
fn detect_api_changes_ts_all_refs_shadowed_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            (
                "src/tap.ts",
                "function startRecording(p: number): number {\n    return p * 2;\n}\nwindow.addEventListener(\"message\", () => {\n    const res = startRecording(1);\n    console.log(res);\n});\n",
            ),
        ],
        "base",
    );
    // 対象 API のみシグネチャ変更。対象 API への呼び出しはリポジトリに存在しない。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/capture.ts".to_string(),
        new_path: "src/lib/capture.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        !api.modified_closed_in_diff
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "shadow 除外で参照 0 件なら closed に降格しない。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
}

/// `obj.startRecording()` (property 位置) の diff 外参照は shadow 除外できず blocking 維持
/// (member 経由は対象 API への参照か静的に判定できないため fail-closed)。
#[test]
fn detect_api_changes_ts_member_call_outside_diff_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/capture.ts",
                "export function startRecording(options: {\n    fps: number;\n}): string {\n    return `rec:${options.fps}`;\n}\n",
            ),
            (
                "src/tap.ts",
                "function startRecording(p: number): number {\n    return p * 2;\n}\nexport const api = { run: startRecording };\n",
            ),
            (
                "src/content.ts",
                "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30 });\n}\n",
            ),
            (
                "src/other.ts",
                "const recorder = { startRecording: (n: number) => n };\nexport function misc() {\n    return recorder.startRecording(1);\n}\n",
            ),
        ],
        "base",
    );
    // capture.ts + content.ts (対象 caller) を変更。other.ts の `recorder.startRecording(1)`
    // (property 位置、diff 外) が shadow 除外されず blocking に倒すことを確認する。
    fs::write(
        repo.join("src/lib/capture.ts"),
        "export function startRecording(options: {\n    fps: number;\n    cursor: boolean;\n}): string {\n    return `rec:${options.fps}:${options.cursor}`;\n}\n",
    )
    .expect("write");
    fs::write(
        repo.join("src/content.ts"),
        "import { startRecording } from \"./lib/capture\";\n\nexport function onStart() {\n    return startRecording({ fps: 30, cursor: true });\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/lib/capture.ts".to_string(),
            new_path: "src/lib/capture.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 5,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/content.ts".to_string(),
            new_path: "src/content.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 4,
                old_count: 1,
                new_start: 4,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified
            .iter()
            .any(|m| m.name.ends_with("startRecording")),
        "property 位置の diff 外参照は除外できないため blocking 維持。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
    );
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

/// rename された caller で呼び出しが古いまま残る場合は blocking。closed-in-diff の変更行
/// 判定が rename-aware (git diff -M) で、rename を新規全行追加と誤認しないことを検証する
/// (codex 指摘: new_path 単独 pathspec だと未更新呼び出しまで changed に見える)。
#[test]
fn detect_api_changes_renamed_caller_with_unchanged_call_stays_modified() {
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
            ("src/lib.rs", "pub mod api;\npub mod caller;\n"),
            (
                "src/api.rs",
                "pub fn process(id: u32) -> u32 {\n    id\n}\n",
            ),
            (
                "src/caller.rs",
                "use crate::api::process;\npub fn run() -> u32 {\n    process(1)\n}\n",
            ),
        ],
        "base",
    );
    // process に引数追加 (signature 変更)
    fs::write(
        repo.join("src/api.rs"),
        "pub fn process(id: u32, extra: bool) -> u32 {\n    id\n}\n",
    )
    .expect("write");
    // caller.rs を caller2.rs に rename + 無関係コメント追加。process(1) 呼び出しは古いまま。
    std::fs::remove_file(repo.join("src/caller.rs")).expect("rm");
    fs::write(
            repo.join("src/caller2.rs"),
            "use crate::api::process;\n// unrelated comment line\npub fn run() -> u32 {\n    process(1)\n}\n",
        )
        .expect("write");
    fs::write(repo.join("src/lib.rs"), "pub mod api;\npub mod caller2;\n").expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/api.rs".to_string(),
            new_path: "src/api.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/caller.rs".to_string(),
            new_path: "src/caller2.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 4,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.modified.iter().any(|m| m.name.ends_with("process")),
        "rename + 未更新呼び出しが残る場合は blocking。mod={:?} mod_closed={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>()
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

#[test]
fn detect_api_changes_reconciles_delete_and_add_as_rename() {
    // git diff が rename を検出できず、旧ファイル削除 + 新ファイル追加の
    // 2 エントリとして供給された場合でも、同一シグネチャの関数は相殺される。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let old_content = "\
def iter_plugin_manifests():
    return []

def check_layout():
    return 0

def main():
    pass
";
    git_commit_files(
        repo,
        &[("scripts/regenerate_marketplace.py", old_content)],
        "initial",
    );

    // ファイル削除 + 別パスに再配置 (rename detection が無効な想定)
    fs::remove_file(repo.join("scripts/regenerate_marketplace.py")).expect("rm old");
    let new_content = "\
def iter_plugin_manifests():
    return []

def check_layout():
    return 0

def main():
    pass

def new_public_api():
    return 1
";
    fs::write(repo.join("scripts/marketplace.py"), new_content).expect("write new");

    // rename 未検出の diff: delete + add の 2 エントリ
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "scripts/regenerate_marketplace.py".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 9,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "scripts/marketplace.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 12,
            }],
            deleted_old_source: None,
        },
    ];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added_names: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    let removed_names: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();

    // 同一シグネチャの 3 関数は相殺される
    assert!(
        !removed_names.contains(&"iter_plugin_manifests"),
        "同一シグネチャの関数は相殺されるべき。got removed: {removed_names:?}"
    );
    assert!(
        !removed_names.contains(&"check_layout"),
        "同一シグネチャの関数は相殺されるべき。got removed: {removed_names:?}"
    );
    assert!(
        !removed_names.contains(&"main"),
        "同一シグネチャの関数は相殺されるべき。got removed: {removed_names:?}"
    );
    assert!(
        !added_names.contains(&"iter_plugin_manifests"),
        "相殺済みの関数は added にも現れるべきではない。got added: {added_names:?}"
    );

    // ただし純粋な新規関数は api.add に残る
    assert!(
        added_names.contains(&"new_public_api"),
        "新規追加された関数は引き続き検出されるべき。got added: {added_names:?}"
    );

    // 相殺された 3 関数は moved として informational に提示されるべき
    let moved_names: std::collections::HashSet<&str> =
        api_changes.moved.iter().map(|m| m.name.as_str()).collect();
    for name in ["iter_plugin_manifests", "check_layout", "main"] {
        assert!(
            moved_names.contains(name),
            "相殺された関数は moved に積まれるべき。got moved: {moved_names:?}"
        );
    }
    for m in &api_changes.moved {
        assert_eq!(m.from, "scripts/regenerate_marketplace.py");
        assert_eq!(m.to, "scripts/marketplace.py");
    }
}

#[test]
fn detect_api_changes_uses_diff_old_source_when_git_show_fails() {
    // CI 環境で source branch (削除コミット適用後) が HEAD の状態で `--base HEAD` を
    // 渡したケースを再現する。`git show HEAD:old_path` は失敗するが、
    // `--diff-file` 経由で渡された削除 hunk から旧ソースを復元できれば
    // api_changes.removed に反映されるべき。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // 旧ファイルを base にコミット → さらに削除を HEAD としてコミット。
    // `git show HEAD:src/old.py` は HEAD には存在しないため失敗する。
    git_commit_files(
        repo,
        &[("src/old.py", "def removed_fn():\n    return 1\n")],
        "initial",
    );
    fs::remove_file(repo.join("src/old.py")).expect("rm");
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(repo)
        .status()
        .expect("git add");
    Command::new("git")
        .args(["commit", "-m", "delete"])
        .current_dir(repo)
        .status()
        .expect("git commit");

    // hunk から復元される旧ソース (`-` 行から組み立て)
    let deleted_src = b"def removed_fn():\n    return 1\n".to_vec();
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/old.py".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(deleted_src),
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.contains(&"removed_fn"),
        "diff の deleted_old_source からシンボルが復元されるべき。got: {removed:?}"
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
fn detect_api_changes_module_to_package_split_reports_moved_not_removed() {
    // 報告再現: cli.py を cli/ パッケージに分割し、各サブコマンドを
    // cli/_commands/<name>.py に移動。cli/__init__.py は再エクスポートを行う。
    // 旧 cli.py の関数は削除ではなく moved として報告されるべき。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let old_cli = "\
import typer

app = typer.Typer()

@app.command(\"rotate\")
def rotate_command(name: str):
    pass

@app.command(\"list\")
def list_tokens():
    pass

@app.command(\"check\")
def check_command():
    pass

def main():
    app()
";
    git_commit_files(repo, &[("src/token_manager/cli.py", old_cli)], "initial");

    // 旧 cli.py を削除し、cli/ パッケージに分割
    fs::remove_file(repo.join("src/token_manager/cli.py")).expect("rm old");
    fs::create_dir_all(repo.join("src/token_manager/cli/_commands")).expect("create pkg");

    let init_py = "\
import typer

from ._commands.rotate import rotate_command
from ._commands.list import list_tokens
from ._commands.check import check_command

app = typer.Typer()

app.command(\"rotate\")(rotate_command)
app.command(\"list\")(list_tokens)
app.command(\"check\")(check_command)


def main():
    app()
";
    let rotate_py = "\
def rotate_command(name: str):
    pass
";
    let list_py = "\
def list_tokens():
    pass
";
    let check_py = "\
def check_command():
    pass
";
    fs::write(repo.join("src/token_manager/cli/__init__.py"), init_py).expect("write init");
    fs::write(repo.join("src/token_manager/cli/_commands/__init__.py"), "")
        .expect("write _commands init");
    fs::write(
        repo.join("src/token_manager/cli/_commands/rotate.py"),
        rotate_py,
    )
    .expect("write rotate");
    fs::write(
        repo.join("src/token_manager/cli/_commands/list.py"),
        list_py,
    )
    .expect("write list");
    fs::write(
        repo.join("src/token_manager/cli/_commands/check.py"),
        check_py,
    )
    .expect("write check");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/token_manager/cli.py".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 20,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/token_manager/cli/__init__.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 13,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/token_manager/cli/_commands/__init__.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 0,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/token_manager/cli/_commands/rotate.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/token_manager/cli/_commands/list.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/token_manager/cli/_commands/check.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
            }],
            deleted_old_source: None,
        },
    ];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: std::collections::HashSet<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();

    // 移動した関数は api.rm から消えていること（report 再現のコア）
    for name in ["rotate_command", "list_tokens", "check_command", "main"] {
        assert!(
            !removed_names.contains(name),
            "module → package 化で移動したシンボルは api.rm に残らないべき。got removed: {removed_names:?}"
        );
    }

    // 移動した関数は moved に積まれていること
    let moved_by_name: std::collections::HashMap<&str, &crate::models::review::MovedSymbol> =
        api_changes
            .moved
            .iter()
            .map(|m| (m.name.as_str(), m))
            .collect();
    for name in ["rotate_command", "list_tokens", "check_command", "main"] {
        let m = moved_by_name
            .get(name)
            .unwrap_or_else(|| panic!("{name} が moved に含まれていない: {moved_by_name:?}"));
        assert_eq!(
            m.from, "src/token_manager/cli.py",
            "from は旧 cli.py であるべき"
        );
        assert!(
            m.to.starts_with("src/token_manager/cli/"),
            "to は新パッケージ配下であるべき: {}",
            m.to
        );
    }
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

#[test]
fn extract_python_class_fields_collects_typed_annotations_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let py = "\
from dataclasses import dataclass


@dataclass
class A:
    x: int
    y: str = \"default\"
    untyped = 1


class B:
    z: float
";
    fs::write(dir.path().join("m.py"), py).expect("write");

    let a_fields = extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "A");
    assert!(
        a_fields.contains("x"),
        "typed annotation は採取される: {a_fields:?}"
    );
    assert!(
        a_fields.contains("y"),
        "default 値付き typed annotation も採取される: {a_fields:?}"
    );
    assert!(
        !a_fields.contains("untyped"),
        "type annotation が無い代入は採取しない: {a_fields:?}"
    );

    let b_fields = extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "B");
    assert!(
        b_fields.contains("z"),
        "@dataclass でないクラスでも採取する: {b_fields:?}"
    );

    let none = extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "Missing");
    assert!(none.is_empty(), "存在しないクラス名は空集合: {none:?}");
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

#[test]
fn import_specifier_package_name_classifies_internal_and_external() {
    // 外部パッケージ
    assert_eq!(
        import_specifier_package_name("tailwindcss").as_deref(),
        Some("tailwindcss")
    );
    assert_eq!(
        import_specifier_package_name("tailwindcss/plugin").as_deref(),
        Some("tailwindcss")
    );
    assert_eq!(
        import_specifier_package_name("@scope/pkg").as_deref(),
        Some("@scope/pkg")
    );
    assert_eq!(
        import_specifier_package_name("@scope/pkg/sub").as_deref(),
        Some("@scope/pkg")
    );
    // 相対 / alias は None (内部、除外しない)
    assert_eq!(import_specifier_package_name("./config"), None);
    assert_eq!(import_specifier_package_name("../lib/config"), None);
    assert_eq!(import_specifier_package_name("@/config"), None);
    assert_eq!(import_specifier_package_name("~/config"), None);
    assert_eq!(import_specifier_package_name("#internal"), None);
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

#[test]
fn detect_api_changes_skips_linguist_generated_files() {
    // .gitattributes で linguist-generated 指定されたファイルの API 変更は報告しない。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(
        repo,
        &[
            (".gitattributes", "generated.py linguist-generated\n"),
            ("generated.py", "def old_gen():\n    pass\n"),
            ("hand.py", "def old_hand():\n    pass\n"),
        ],
        "initial",
    );
    // 生成ファイルと手書きファイルの双方で関数追加
    fs::write(
        repo.join("generated.py"),
        "def old_gen():\n    pass\n\ndef new_gen():\n    pass\n",
    )
    .expect("write");
    fs::write(
        repo.join("hand.py"),
        "def old_hand():\n    pass\n\ndef new_hand():\n    pass\n",
    )
    .expect("write");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "generated.py".to_string(),
            new_path: "generated.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "hand.py".to_string(),
            new_path: "hand.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        },
    ];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

    assert!(
        !added.contains(&"new_gen"),
        "linguist-generated ファイルの API 変更は除外されるべき。got: {added:?}"
    );
    assert!(
        added.contains(&"new_hand"),
        "通常ファイルの API 追加は検出されるべき。got: {added:?}"
    );
}

/// ファイル先頭に自動生成マーカーコメント (`@generated` / `Automatically generated
/// by ...`) を含むファイルは、.gitattributes が無くても API 変更検出から除外される。
/// (レポート 2026-04-16-tree-sitter-generated-enum-dead-code.md の再現)
#[test]
fn detect_api_changes_skips_auto_generated_marker_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(
        repo,
        &[
            (
                "gen.py",
                "# @generated by tree-sitter\ndef old_gen():\n    pass\n",
            ),
            ("hand.py", "def old_hand():\n    pass\n"),
        ],
        "initial",
    );
    fs::write(
        repo.join("gen.py"),
        "# @generated by tree-sitter\ndef old_gen():\n    pass\n\ndef new_gen():\n    pass\n",
    )
    .expect("write");
    fs::write(
        repo.join("hand.py"),
        "def old_hand():\n    pass\n\ndef new_hand():\n    pass\n",
    )
    .expect("write");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "gen.py".to_string(),
            new_path: "gen.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "hand.py".to_string(),
            new_path: "hand.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        },
    ];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

    assert!(
        !added.contains(&"new_gen"),
        "@generated マーカーのあるファイルは API 変更検出から除外されるべき。got: {added:?}"
    );
    assert!(
        added.contains(&"new_hand"),
        "通常ファイルの API 追加は検出されるべき。got: {added:?}"
    );
}

/// symlink で workspace 外を指す追加ファイルは API 変更検出の対象外。
///
/// 再現シナリオ:
/// - 攻撃者が PR に `evil.rs -> /etc/passwd` のような外部ファイルへの symlink を追加
/// - `is_safe_diff_path` は文字列 check (絶対パス / `..` 拒否) のみで symlink を検出できない
/// - `parser::read_file` は `File::open` でデフォルト follow し、外部ファイルの識別子が
///   `api_changes.added` の `name` / `signature` に流れ込みリークする
///
/// 修正: `should_skip_diff_file` で canonicalize して root 配下かを fail-closed 判定する。
#[test]
#[cfg(unix)]
fn detect_api_changes_skips_symlink_escape_to_outside_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("existing.rs", "pub fn dummy() {}\n")], "initial");

    // workspace 外にシンボリックリンク先のファイルを置く
    let outside_dir = tempfile::tempdir().expect("outside tempdir");
    let outside_file = outside_dir.path().join("secret.rs");
    fs::write(&outside_file, "pub fn SECRET_FROM_OUTSIDE_WORKSPACE() {}\n").expect("write secret");

    // workspace 内に symlink を作成 (.rs 拡張子で言語判定を通す)
    let evil_link = repo.join("evil.rs");
    std::os::unix::fs::symlink(&outside_file, &evil_link).expect("symlink");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "/dev/null".to_string(),
        new_path: "evil.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

    assert!(
        !added.contains(&"SECRET_FROM_OUTSIDE_WORKSPACE"),
        "symlink 越しの workspace 外シンボルは抽出されてはならない。got: {added:?}"
    );
}

/// Angular component の public method が `templateUrl` で紐づく
/// `.component.html` から参照されている場合、dead 判定から除外される。
///
/// 再現元: astro-sight-bug-reports#4 (framework-template-ref)
/// - `@Component({ templateUrl: './foo.component.html' })` で紐づく HTML 内の
///   `(event)="method()"` / `[prop]="method()"` / `[ngStyle]="{ ...: method() }"`
///   等の binding 式で呼ばれている component method が
///   TS AST だけ見ると dead 扱いされる問題を修正。
#[test]
fn detect_dead_excludes_angular_component_methods_referenced_from_template() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // Angular プロジェクト標識として angular.json を置く
    fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

    let component_ts = r#"
import { Component } from '@angular/core';

@Component({
    selector: 'app-sample',
    templateUrl: './sample.component.html',
})
export class SampleComponent {
    public headerCheck: boolean = false;

    public headerCheckChanged(): void {
    }

    public isHeaderDisabled(): boolean {
        return false;
    }

    public reallyUnusedMethod(): void {
    }
}
"#;
    let component_html = r#"
<label [ngStyle]="{'display': isHeaderDisabled() ? 'none' : ''}">
    <input type="checkbox"
           [(ngModel)]="headerCheck"
           (ngModelChange)="headerCheckChanged()">
</label>
"#;
    fs::write(repo.join("sample.component.ts"), component_ts).expect("write ts");
    fs::write(repo.join("sample.component.html"), component_html).expect("write html");

    let files = vec![repo.join("sample.component.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with("headerCheckChanged")),
        "Angular template から (ngModelChange) で参照される method は dead から除外されるべき。got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.ends_with("isHeaderDisabled")),
        "Angular template の [ngStyle] 式から参照される method は dead から除外されるべき。got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with("reallyUnusedMethod")),
        "テンプレートからも参照されない method は dead として検出されるべき。got: {names:?}"
    );
}

#[test]
fn detect_dead_php_duplicate_static_factory_methods_are_owner_aware() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(
        repo.join("A.php"),
        "<?php\nclass A {\n    public static function new(): self { return new self(); }\n}\n",
    )
    .expect("write A");
    fs::write(
        repo.join("B.php"),
        "<?php\nclass B {\n    public static function new(): self { return new self(); }\n}\n",
    )
    .expect("write B");
    fs::write(
        repo.join("use.php"),
        "<?php\nfunction use_classes(): void {\n    $a = new A();\n    $b = new B();\n    A::new();\n}\n",
    )
    .expect("write use");

    let files = vec![repo.join("A.php"), repo.join("B.php"), repo.join("use.php")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"A.new"),
        "A::new() で参照される A.new は dead ではない。got: {names:?}"
    );
    assert!(
        names.contains(&"B.new"),
        "同名 factory が複数 owner にあっても未参照の B.new は dead として検出する。got: {names:?}"
    );
}

#[test]
fn detect_dead_php_duplicate_static_factory_methods_on_single_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(
        repo.join("A.php"),
        "<?php\nclass A { public static function new(): self { return new self(); } }\n",
    )
    .expect("write A");
    fs::write(
        repo.join("B.php"),
        "<?php\nclass B { public static function new(): self { return new self(); } }\n",
    )
    .expect("write B");
    fs::write(
        repo.join("use.php"),
        "<?php\nfunction use_classes(): void { $a = new A(); $b = new B(); A::new(); }\n",
    )
    .expect("write use");

    let files = vec![repo.join("A.php"), repo.join("B.php"), repo.join("use.php")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"A.new") && names.contains(&"B.new"),
        "1行 class 定義でも owner-aware に PHP factory method を判定する。got: {names:?}"
    );
}

#[test]
fn detect_dead_php_duplicate_methods_with_dynamic_call_remain_ambiguous() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(
        repo.join("A.php"),
        "<?php\nclass A {\n    public static function new(): self { return new self(); }\n}\n",
    )
    .expect("write A");
    fs::write(
        repo.join("B.php"),
        "<?php\nclass B {\n    public static function new(): self { return new self(); }\n}\n",
    )
    .expect("write B");
    fs::write(
        repo.join("use.php"),
        "<?php\nfunction use_classes($factory): void {\n    $a = new A();\n    $b = new B();\n    $factory->new();\n}\n",
    )
    .expect("write use");

    let files = vec![repo.join("A.php"), repo.join("B.php"), repo.join("use.php")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"A.new") && !names.contains(&"B.new"),
        "動的呼び出し $factory->new() は owner を確定できないため旧スキップを維持する。got: {names:?}"
    );
}

#[test]
fn detect_dead_cpp_h_header_class_methods_are_parsed_as_cpp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(
        repo.join("GenericClient.h"),
        "class GenericClient {\n\
public:\n\
    int getAdditionalHttpHeaders() const { return 0; }\n\
    int getSetupFormat() const { return 1; }\n\
};\n",
    )
    .expect("write header");

    let files = vec![repo.join("GenericClient.h")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        names.contains(&"GenericClient.getAdditionalHttpHeaders")
            && names.contains(&"GenericClient.getSetupFormat"),
        ".h 内の C++ public getter も dead として検出する。got: {names:?}"
    );
}

/// C/C++ の前方宣言・opaque tag (`typedef struct st_mysql MYSQL;` の `st_mysql`) は
/// 「定義」ではなく宣言なので dead_symbols に含めない。本体を持つ未使用 struct は
/// 引き続き dead として検出される (Issue #11)。
#[test]
fn detect_dead_cpp_forward_declaration_tag_excluded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let header = "typedef struct st_mysql MYSQL;\nstruct UnusedDefined { int x; };\n";
    fs::write(repo.join("mysql_service.h"), header).expect("write header");

    let files = vec![repo.join("mysql_service.h")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"st_mysql"),
        "前方宣言タグ st_mysql は dead に含めない: {names:?}"
    );
    assert!(
        names.contains(&"UnusedDefined"),
        "本体を持つ未使用 struct UnusedDefined は dead として検出されるべき: {names:?}"
    );
}

/// C/C++ の `struct X` 型使用 (引数型 / 変数宣言 / メンバ宣言 / sizeof / cast) は
/// tag 名 `X` の非 Definition 参照として数え、使用中 struct を dead に出さない。
/// GitLab #28 の C/C++ struct 型参照取りこぼしの回帰テスト。
#[test]
fn detect_dead_cpp_struct_tag_type_uses_are_live() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let c_source = "struct voice_options { const char* host; int port; };\n\
struct unused_c_struct { int x; };\n\
static void load_config_file(struct voice_options* option) {\n\
    option->port = 8080;\n\
}\n\
int main(void) {\n\
    struct voice_options voice_option = { .host = \"localhost\", .port = 80 };\n\
    load_config_file(&voice_option);\n\
    return voice_option.port;\n\
}\n";
    let cpp_source = "struct text_server_data { int code; };\n\
struct buffer_data { int size; };\n\
struct unused_cpp_struct { int y; };\n\
class Converter {\n\
    struct buffer_data* buffer;\n\
};\n\
bool read_params_generic(struct text_server_data header) {\n\
    return header.code > 0;\n\
}\n\
int allocate_buffer(void* raw) {\n\
    struct buffer_data* p = (struct buffer_data*)raw;\n\
    return p ? (int)sizeof(struct buffer_data) : 0;\n\
}\n";
    fs::write(repo.join("app_textserver.c"), c_source).expect("write c");
    fs::write(repo.join("VoiceToTextConvertServer.cpp"), cpp_source).expect("write cpp");

    let files = vec![
        repo.join("app_textserver.c"),
        repo.join("VoiceToTextConvertServer.cpp"),
    ];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    for live in ["voice_options", "text_server_data", "buffer_data"] {
        assert!(
            !names.contains(&live),
            "型として使用中の struct {live} は dead に出さない: {names:?}"
        );
    }
    for unused in ["unused_c_struct", "unused_cpp_struct"] {
        assert!(
            names.contains(&unused),
            "未使用 struct {unused} は引き続き dead として検出されるべき: {names:?}"
        );
    }
}

/// C/C++ の enum は、型名が直接使われなくても列挙子のいずれかが参照されていれば live と
/// 判定する。body あり typedef tag も alias 名経由の参照で live と判定する。列挙子も alias も
/// 未使用なら dead として検出される (Issue #12 enumerator liveness / Issue #11 typedef alias)。
#[test]
fn detect_dead_cpp_enum_enumerator_and_typedef_alias_liveness() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let header = "enum StdAgentSatus { POST_WORK = 1, LOGOFF = 10 };\n\
enum UnusedEnum { UE_A = 1, UE_B = 2 };\n\
typedef struct st_local { int v; } LocalAlias;\n\
typedef struct st_unused { int w; } UnusedAlias;\n";
    let main_cpp = "#include \"svc.h\"\n\
int useThem() {\n\
    int x = LOGOFF;\n\
    LocalAlias la;\n\
    la.v = 1;\n\
    return x + la.v;\n\
}\n";
    git_commit_files(
        repo,
        &[("svc.h", header), ("main.cpp", main_cpp)],
        "initial",
    );

    let files = vec![repo.join("svc.h"), repo.join("main.cpp")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"StdAgentSatus"),
        "列挙子 LOGOFF が使用中の enum StdAgentSatus は dead に出さない: {names:?}"
    );
    assert!(
        !names.contains(&"st_local"),
        "alias LocalAlias が使用中の typedef tag st_local は dead に出さない: {names:?}"
    );
    assert!(
        names.contains(&"UnusedEnum"),
        "列挙子も未使用の enum UnusedEnum は dead として検出されるべき: {names:?}"
    );
    assert!(
        names.contains(&"st_unused"),
        "alias 未使用の typedef tag st_unused は dead として検出されるべき: {names:?}"
    );
}

/// codex 指摘の回帰: (1) typedef の配列長式で参照される列挙子は def 誤判定されず enum が
/// live、(2) 複数 declarator (`typedef S A, *B;`) のいずれかの alias 使用で underlying tag が
/// live と判定される (Issue #11/#12)。
#[test]
fn detect_dead_cpp_typedef_array_size_and_multiple_declarators() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let header = "enum Sz { SZ_VAL = 4 };\n\
typedef int IntArr[SZ_VAL];\n\
typedef struct st_multi { int v; } MultiA, *MultiBPtr;\n\
typedef struct st_solo { int w; } SoloAlias;\n";
    let main_cpp = "#include \"svc.h\"\n\
IntArr g_arr;\n\
int useMulti() {\n\
    MultiBPtr p = nullptr;\n\
    return p ? 1 : 0;\n\
}\n";
    git_commit_files(
        repo,
        &[("svc.h", header), ("main.cpp", main_cpp)],
        "initial",
    );

    let files = vec![repo.join("svc.h"), repo.join("main.cpp")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"Sz"),
        "typedef 配列長 IntArr[SZ_VAL] で参照される列挙子の enum Sz は live: {names:?}"
    );
    assert!(
        !names.contains(&"st_multi"),
        "複数 declarator の 2 番目 alias MultiBPtr 使用で st_multi は live: {names:?}"
    );
    assert!(
        names.contains(&"st_solo"),
        "alias SoloAlias 未使用の st_solo は dead として検出されるべき: {names:?}"
    );
}

/// Angular の inline template (`@Component({ template: \`...\` })`) で参照される
/// component method も dead 判定から除外される。
#[test]
fn detect_dead_excludes_angular_inline_template_method_refs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

    let component_ts = r#"
import { Component } from '@angular/core';

@Component({
    selector: 'app-inline',
    template: `<button (click)="onClick()">{{ greeting }}</button>`,
})
export class InlineComponent {
    public greeting: string = 'hi';

    public onClick(): void {
    }

    public reallyUnusedInline(): void {
    }
}
"#;
    fs::write(repo.join("inline.component.ts"), component_ts).expect("write ts");

    let files = vec![repo.join("inline.component.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with("onClick")),
        "inline template の (click) で参照される method は dead から除外されるべき。got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with("reallyUnusedInline")),
        "inline template からも参照されない method は dead として検出されるべき。got: {names:?}"
    );
}

/// GitLab issue #8 再現: `@Component` / `@Directive` 装飾クラスの Angular ライフサイクル
/// フック (`ngAfterViewChecked` 等) は Angular ランタイムが change detection サイクルで
/// 自動呼出するため、静的解析で caller が見つからなくても dead 判定しない。
#[test]
fn detect_dead_excludes_angular_component_lifecycle_hooks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

    let component_ts = r#"
import { Component } from '@angular/core';

@Component({
    template: '<div>example</div>',
})
export class MinimalComponent {
    public ngOnInit(): void {}
    public ngAfterViewChecked(): void {}
    public ngOnDestroy(): void {}

    public reallyUnused(): void {}
}
"#;
    fs::write(repo.join("minimal.component.ts"), component_ts).expect("write ts");

    let files = vec![repo.join("minimal.component.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    for hook in ["ngOnInit", "ngAfterViewChecked", "ngOnDestroy"] {
        assert!(
            !names.iter().any(|n| n.ends_with(hook)),
            "Angular @Component の lifecycle hook {hook} は dead から除外されるべき。got: {names:?}"
        );
    }
    assert!(
        names.iter().any(|n| n.ends_with("reallyUnused")),
        "Angular component の lifecycle hook 以外の未参照 method は引き続き dead として検出されるべき。got: {names:?}"
    );
}

/// `@Directive` 装飾クラスでも lifecycle hook を dead から除外する。
#[test]
fn detect_dead_excludes_angular_directive_lifecycle_hooks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

    let directive_ts = r#"
import { Directive } from '@angular/core';

@Directive({ selector: '[appFoo]' })
export class FooDirective {
    public ngOnInit(): void {}
    public ngOnChanges(): void {}
}
"#;
    fs::write(repo.join("foo.directive.ts"), directive_ts).expect("write ts");

    let files = vec![repo.join("foo.directive.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    for hook in ["ngOnInit", "ngOnChanges"] {
        assert!(
            !names.iter().any(|n| n.ends_with(hook)),
            "Angular @Directive の lifecycle hook {hook} は dead から除外されるべき。got: {names:?}"
        );
    }
}

/// `@Component` / `@Directive` のいずれも持たないクラスで同名メソッドを定義した場合は
/// dead から除外せず引き続き検出対象とする (誤除外の防止)。
#[test]
fn detect_dead_keeps_non_angular_class_methods_with_lifecycle_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // Angular プロジェクトとして認識されるよう angular.json を置く (誤除外の境界確認)
    fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

    let plain_ts = r#"
export class PlainClass {
    public ngOnInit(): void {}
    public ngAfterViewChecked(): void {}
}
"#;
    fs::write(repo.join("plain.ts"), plain_ts).expect("write ts");

    let files = vec![repo.join("plain.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    for hook in ["ngOnInit", "ngAfterViewChecked"] {
        assert!(
            names.iter().any(|n| n.ends_with(hook)),
            "@Component / @Directive を持たないクラスの {hook} は引き続き dead として検出されるべき。got: {names:?}"
        );
    }
}

/// 非 Angular プロジェクトでは `.html` ファイルを参照源としてスキャンしない
/// （誤って HTML 内の単語を参照と誤認しないことの確認）。
#[test]
fn detect_dead_does_not_use_html_in_non_angular_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // angular.json も *.component.ts もない通常の TS プロジェクト
    let ts = r#"
export function ghostHandler(): void {
}
"#;
    fs::write(repo.join("util.ts"), ts).expect("write ts");
    fs::write(
        repo.join("page.html"),
        r#"<button (click)="ghostHandler()">x</button>"#,
    )
    .expect("write html");

    let files = vec![repo.join("util.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        names.contains(&"ghostHandler"),
        "Angular マーカーが無い場合は HTML 参照を生存判定に使わない (非 Angular なので) 。got: {names:?}"
    );
}

/// dead-code 検出でも同じマーカーで生成ファイルは除外される
#[test]
fn detect_dead_symbols_skips_auto_generated_marker_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(
        repo.join("gen.py"),
        "# Automatically generated by tree-sitter\ndef unused_gen():\n    pass\n",
    )
    .expect("write");
    fs::write(repo.join("hand.py"), "def unused_hand():\n    pass\n").expect("write");

    let files = vec![repo.join("gen.py"), repo.join("hand.py")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"unused_gen"),
        "自動生成マーカーのあるファイルは dead-code 検出から除外されるべき。got: {names:?}"
    );
    assert!(
        names.contains(&"unused_hand"),
        "通常ファイルの未使用関数は dead として検出されるべき。got: {names:?}"
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

/// Rust の `pub struct` へ private フィールドを追加しただけでは api.mod に出ない。
/// 宣言行 (`pub struct Foo {`) は変わらず、本体 (フィールド) の変更のため
/// `extract_api_signature` が宣言行のみを見る既存のロジックで自然に除外される。
/// (レポート 2026-04-17-private-field-addition-over-detection.md の再現)
#[test]
fn detect_api_changes_private_field_addition_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
#[derive(Debug, Clone)]
pub struct AiService {
    existing: String,
}
";
    git_commit_files(repo, &[("src/lib.rs", before)], "initial");

    // private フィールド追加のみ（pub struct 宣言行は不変）
    let after = "\
#[derive(Debug, Clone)]
pub struct AiService {
    existing: String,
    codex_reasoning_effort: String,
}
";
    fs::write(repo.join("src/lib.rs"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib.rs".to_string(),
        new_path: "src/lib.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 3,
            old_count: 1,
            new_start: 3,
            new_count: 2,
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
        !mod_names.contains(&"AiService"),
        "pub struct の内部（private フィールド）変更は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// Python で同名メソッドを持つ複数クラスがあるとき、qualname (`ClassName.method`)
/// として区別され、触っていない方は api.mod に出ない。
#[test]
fn detect_api_changes_distinguishes_same_named_python_methods() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
class ClaudeReviewer:
    def execute(self) -> int:
        return 1


class CodexReviewer:
    def execute(self) -> str:
        return \"ok\"


class ReReviewExecutor:
    def execute(self) -> None:
        pass
";
    git_commit_files(repo, &[("svc.py", before)], "initial");

    // ReReviewExecutor.execute だけ本体を変更（シグネチャは同じ）
    let after = "\
class ClaudeReviewer:
    def execute(self) -> int:
        return 1


class CodexReviewer:
    def execute(self) -> str:
        return \"ok\"


class ReReviewExecutor:
    def execute(self) -> None:
        return None
";
    fs::write(repo.join("svc.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "svc.py".to_string(),
        new_path: "svc.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 13,
            old_count: 1,
            new_start: 13,
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

    // bare name `execute` は重複検出されず、qualname で区別されていること
    assert!(
        mod_names.iter().all(|n| *n != "execute"),
        "bare name `execute` は出ないはず（qualname 化されているべき）。got: {mod_names:?}"
    );
    // シグネチャ変更なし（本体のみ変更）なので api.mod には何も出ないはず
    assert!(
        api_changes.modified.is_empty(),
        "本体のみの変更で signature 不変なら modified に出ないはず。got: {:?}",
        api_changes.modified
    );
}

/// Python クラスの private メソッドの本体変更は、クラス自体の modified として上がらない。
/// 宣言行（`class Foo:`）が変わらない限り Class のシグネチャは不変。
#[test]
fn detect_api_changes_class_body_change_does_not_mark_class_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
class PromptBuilder:
    def _build_common(self) -> str:
        return \"v1\"
";
    git_commit_files(repo, &[("pb.py", before)], "initial");

    let after = "\
class PromptBuilder:
    def _build_common(self) -> str:
        return \"v2 with much more text\"
";
    fs::write(repo.join("pb.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "pb.py".to_string(),
        new_path: "pb.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 3,
            old_count: 1,
            new_start: 3,
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
        !mod_names.contains(&"PromptBuilder"),
        "クラス本体の変更でクラス自体を api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// Python で同一クラス内のメソッドシグネチャが変わった場合は qualname で検出される。
#[test]
fn detect_api_changes_detects_qualified_method_signature_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
class Reviewer:
    def execute(self) -> int:
        return 1
";
    git_commit_files(repo, &[("r.py", before)], "initial");

    let after = "\
class Reviewer:
    def execute(self, mode: str) -> int:
        return 1
";
    fs::write(repo.join("r.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "r.py".to_string(),
        new_path: "r.py".to_string(),
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
        mod_names.contains(&"Reviewer.execute"),
        "qualname 形式のメソッドシグネチャ変更を検出すべき。got: {mod_names:?}"
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

/// Python で同一ファイル内から呼ばれている新規 public 関数は api.add に出ない。
#[test]
fn detect_api_changes_python_internally_called_function_is_not_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "def main():\n    print(\"hi\")\n";
    git_commit_files(repo, &[("svc.py", before)], "initial");

    // helper を追加し、main から呼ぶ
    let after = "def helper() -> str:\n    return \"x\"\n\n\
def main():\n    helper()\n    print(\"hi\")\n";
    fs::write(repo.join("svc.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "svc.py".to_string(),
        new_path: "svc.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !added.contains(&"helper"),
        "同一ファイル内で呼ばれている Python 関数は api.add に出してはならない。got: {added:?}"
    );
}

/// Python CLI スクリプト（同一ファイル内でのみ呼ばれる関数）のシグネチャ変更は
/// caller が同じ diff 内で追随できるため api.mod に出さない。
/// (レポート 2026-04-22-closed-in-diff-signature-change-noise.md の再現)
#[test]
fn detect_api_changes_python_cli_signature_change_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
def run_osv_scanner(path: str) -> int:
    return 0


def scan_worktree(path: str) -> int:
    rc = run_osv_scanner(path)
    return rc


if __name__ == \"__main__\":
    scan_worktree(\".\")
";
    git_commit_files(repo, &[("osv_scan.py", before)], "initial");

    // run_osv_scanner の戻り値型を int -> tuple[int, float] に変更。
    // caller (scan_worktree) も同じ diff 内で追随する。
    let after = "\
def run_osv_scanner(path: str) -> tuple[int, float]:
    return (0, 0.0)


def scan_worktree(path: str) -> int:
    _rc, _elapsed = run_osv_scanner(path)
    return _rc


if __name__ == \"__main__\":
    scan_worktree(\".\")
";
    fs::write(repo.join("osv_scan.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "osv_scan.py".to_string(),
        new_path: "osv_scan.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 11,
            new_start: 1,
            new_count: 11,
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
        !mod_names.contains(&"run_osv_scanner"),
        "同一ファイル内でのみ呼ばれる関数のシグネチャ変更は api.mod に出してはならない。got: {mod_names:?}"
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

/// テストディレクトリ配下のシンボル変更は api.add/rm/mod に出さない。
/// (レポート 2026-04-30-test-symbol-api-detection.md / 2026-04-29-junit-reflection-entrypoints.md の再現)
/// Tests/ 配下、`*Test.kt`、`*.test.ts` 等のテストファイルは外部 API 面ではない。
#[test]
fn detect_api_changes_skips_test_directory_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "package fixture\n\nfun helper() {}\n";
    git_commit_files(repo, &[("app/src/test/java/FooTest.kt", before)], "initial");

    // テスト関数を新規追加
    let after = "package fixture\n\nfun helper() {}\n\
@org.junit.Test\nfun testHelperReturnsZero() {}\n";
    fs::write(repo.join("app/src/test/java/FooTest.kt"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "app/src/test/java/FooTest.kt".to_string(),
        new_path: "app/src/test/java/FooTest.kt".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    let modified: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        added.is_empty(),
        "テストファイル配下の新規シンボルは api.add に出してはならない。got: {added:?}"
    );
    assert!(
        removed.is_empty(),
        "テストファイル配下のシンボル削除は api.rm に出してはならない。got: {removed:?}"
    );
    assert!(
        modified.is_empty(),
        "テストファイル配下のシンボル変更は api.mod に出してはならない。got: {modified:?}"
    );
}

/// テストファイル丸ごと削除でも api.rm に出さない。
/// (Issue D 関連: テストファイルの整理は API 削除ではない)
#[test]
fn detect_api_changes_skips_test_file_deletion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "import { describe, it } from 'vitest'\n\
export function testHelper() { return 1 }\n";
    git_commit_files(repo, &[("src/foo.test.ts", before)], "initial");

    std::fs::remove_file(repo.join("src/foo.test.ts")).expect("remove");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/foo.test.ts".to_string(),
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

    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.is_empty(),
        "*.test.ts 削除は api.rm に出してはならない。got: {removed:?}"
    );
}

/// JVM/Gradle 標準の `src/test/` 配下は dead-code 検出から既定で除外される。
/// (レポート 2026-05-21-junit-kotlin-test-dead-symbols.md の再現)
///
/// 2026-04-29 時点の resolved コメントでは「dead 側は既に `test` セグメントで除外済み」と
/// されていたが、当時の `DEFAULT_DEAD_CODE_EXCLUDES_TESTS` に `test` 単数形は無く、
/// API 検出側の `is_test_path` のみが `test` を扱っていた。本テストはこのねじれ解消の
/// 回帰防止: `test` / `androidTest` / `sharedTest` / `integrationTest` セグメントは
/// 共通定数 `TEST_DIRECTORY_SEGMENTS` 経由で dead-code 側でも既定除外されるべき。
#[test]
fn filter_diff_files_for_dead_code_excludes_jvm_src_test_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    std::fs::create_dir_all(repo.join("app/src/test/java/com/example")).expect("mkdir src/test");
    std::fs::write(
        repo.join("app/src/test/java/com/example/FooTest.kt"),
        "package com.example\nclass FooTest\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
        new_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let canonical = std::fs::canonicalize(repo).expect("canonicalize");
    // --include-tests なし (既定): DEFAULT_DEAD_CODE_EXCLUDES_TESTS を適用
    let excludes = resolve_dead_code_excludes(false, false, false);
    let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
        .expect("filter");

    assert!(
        files.is_empty(),
        "JVM/Gradle 標準の src/test/ 配下は --include-tests なしで dead-code 対象から除外されるべき。got: {files:?}"
    );
}

/// `--include-tests` を opt-in した場合は JVM の `src/test/` 配下も走査対象に残る。
/// (上記 `filter_diff_files_for_dead_code_excludes_jvm_src_test_directory` の対照)
#[test]
fn filter_diff_files_for_dead_code_includes_jvm_src_test_directory_when_opted_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    std::fs::create_dir_all(repo.join("app/src/test/java/com/example")).expect("mkdir src/test");
    std::fs::write(
        repo.join("app/src/test/java/com/example/FooTest.kt"),
        "package com.example\nclass FooTest\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
        new_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let canonical = std::fs::canonicalize(repo).expect("canonicalize");
    // --include-tests opt-in: DEFAULT_DEAD_CODE_EXCLUDES_TESTS を適用しない
    let excludes = resolve_dead_code_excludes(false, true, false);
    let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
        .expect("filter");

    assert_eq!(
        files.len(),
        1,
        "--include-tests 時は src/test/ 配下も走査対象に残るべき。got: {files:?}"
    );
}

/// 親ディレクトリ自体に `test` セグメントが含まれていても、root 配下の通常ファイルは
/// 除外されない。`canonical_dir.join(new_path)` 後の絶対パスを判定材料にしていた
/// 過去実装では `/private/tmp/test/<repo>/src/lib.rs` が全部除外される false negative
/// が出た (2026-05-21 codex コミット前レビュー指摘)。除外判定は workspace 相対の
/// `new_path` で行うべき。
#[test]
fn filter_diff_files_for_dead_code_does_not_misclassify_when_ancestor_dir_contains_test_segment() {
    let dir = tempfile::tempdir().expect("tempdir");
    // tempdir 直下にさらに "test" セグメントの親ディレクトリを作って、そこにリポを置く
    let repo = dir.path().join("test/myrepo");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn existing() {}\npub fn newly_dead() {}\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib.rs".to_string(),
        new_path: "src/lib.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let canonical = std::fs::canonicalize(&repo).expect("canonicalize");
    let excludes = resolve_dead_code_excludes(false, false, false);
    let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
        .expect("filter");

    assert_eq!(
        files.len(),
        1,
        "親パスが `/.../test/myrepo` でも、リポ内 `src/lib.rs` は除外されないべき。got: {files:?}"
    );
}

/// Android instrumentation tests (`src/androidTest/`) も既定除外。
#[test]
fn filter_diff_files_for_dead_code_excludes_android_test_source_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    std::fs::create_dir_all(repo.join("app/src/androidTest/java/com/example"))
        .expect("mkdir androidTest");
    std::fs::write(
        repo.join("app/src/androidTest/java/com/example/InstrumentedTest.kt"),
        "package com.example\nclass InstrumentedTest\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "app/src/androidTest/java/com/example/InstrumentedTest.kt".to_string(),
        new_path: "app/src/androidTest/java/com/example/InstrumentedTest.kt".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let canonical = std::fs::canonicalize(repo).expect("canonicalize");
    let excludes = resolve_dead_code_excludes(false, false, false);
    let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
        .expect("filter");

    assert!(
        files.is_empty(),
        "Android `src/androidTest/` も既定で dead-code 対象から除外されるべき。got: {files:?}"
    );
}

/// TS/JS の constructor は dead 候補から除外される。
/// (レポート 2026-04-29-typescript-constructor-implicit-call.md の再現)
/// `new ClassName(...)` で暗黙的に呼ばれるため、`refs --name constructor` で
/// 見つからず dead 判定される問題への対応。
#[test]
fn detect_dead_excludes_typescript_constructor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    std::fs::write(
            repo.join("foo.ts"),
            "export class Foo {\n  constructor(public name: string) {}\n  greet() { return this.name; }\n}\n",
        )
        .expect("write");
    std::fs::write(
        repo.join("usage.ts"),
        "import { Foo } from './foo';\nconst f = new Foo('world');\nconsole.log(f.greet());\n",
    )
    .expect("write");

    let candidates =
        extract_dead_code_candidates_from_file(repo.to_str().expect("utf-8 path"), "foo.ts")
            .expect("candidates");
    let names: Vec<&str> = candidates
        .iter()
        .map(|(name, _, _)| name.as_str())
        .collect();
    assert!(
        !names
            .iter()
            .any(|n| n.ends_with(".constructor") || *n == "constructor"),
        "TS の constructor は dead 候補に含めない。got: {names:?}"
    );
    assert!(
        names.contains(&"Foo"),
        "クラス自体は dead 候補に含まれる。got: {names:?}"
    );
}

/// PHP のメソッド名は case-insensitive。case 違い (`isLocalLInk` 定義 / `isLocalLink`
/// 呼び出し) で参照される public メソッドを dead_symbols に出さない (GitLab #10 の再現)。
#[test]
fn detect_dead_php_case_insensitive_method_call_is_not_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    std::fs::write(
        repo.join("Vo.php"),
        "<?php\nclass Vo {\n    public function isLocalLInk(): bool { return true; }\n}\n",
    )
    .expect("write");
    std::fs::write(
            repo.join("Caller.php"),
            "<?php\nclass Caller {\n    public function check(Vo $vo): bool { return $vo->isLocalLink(); }\n}\n",
        )
        .expect("write");

    let files = vec![repo.join("Vo.php"), repo.join("Caller.php")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let dead_names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();
    assert!(
        !dead_names.iter().any(|n| n.ends_with("isLocalLInk")),
        "case 違いで呼ばれる method を dead にしない。got: {dead_names:?}"
    );
}

/// React.memo (named function expression) の関数本体内の lexical const は api.add に出さない。
/// (レポート 2026-05-04-next-page-and-react-memo-false-positives.md パターン1 の再現)
/// `export const X = memo(function X() { const inner = ... })` の `inner` は
/// 関数本体スコープのローカル変数で公開 API ではない。`is_js_function_body` の
/// `function_expression` 認識で境界停止される。
#[test]
fn detect_api_changes_excludes_memo_wrapper_internal_const() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    std::fs::write(
        repo.join("Card.tsx"),
        "import { memo } from 'react';\n\
export const TaskKanbanCard = memo(function TaskKanbanCard() {\n\
  const hasAssignee = true;\n\
  const milestoneColor = hasAssignee ? 'red' : 'gray';\n\
  return null;\n\
});\n",
    )
    .expect("write");

    let syms = extract_new_file_facts(repo.to_str().expect("utf-8 path"), "Card.tsx")
        .exported
        .expect("symbols");
    let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(
        !names.contains(&"hasAssignee"),
        "memo wrapper 内のローカル const は exported API に含めない。got: {names:?}"
    );
    assert!(
        !names.contains(&"milestoneColor"),
        "memo wrapper 内のローカル const は exported API に含めない。got: {names:?}"
    );
    assert!(
        names.contains(&"TaskKanbanCard"),
        "memo で包んだ exported const 自体は API に含める。got: {names:?}"
    );
}

/// perf #2: `extract_new_file_facts` が new_path を 1 回 read+parse して exported / callees /
/// export surface の 3 facts を正しく導出する。TS の named re-export・local export・呼び出しを
/// 1 ファイルに含め、3 種が分離して取れることを確認する。
#[test]
fn extract_new_file_facts_ts_combines_exported_callees_reexports() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("mod.ts"),
        "export { Helper } from './helper';\n\
export const Widget = () => { compute(); };\n\
function compute() { return 1; }\n",
    )
    .expect("write");

    let facts = extract_new_file_facts(dir.path().to_str().expect("utf-8"), "mod.ts");
    let exported: Vec<&str> = facts
        .exported
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|(n, _, _)| n.as_str())
        .collect();
    assert!(
        exported.contains(&"Widget"),
        "local export const は exported に含まれる。got: {exported:?}"
    );
    assert!(
        facts.export_surface_names.contains("Helper"),
        "named re-export は export surface に含まれる。got: {:?}",
        facts.export_surface_names
    );
    assert!(
        facts.callees.contains("compute"),
        "本体内の呼び出しは callees に含まれる。got: {:?}",
        facts.callees
    );
}

/// perf #2: lexer-only (Xojo) でも `extract_new_file_facts` は panic せず、exported は lexer
/// 経由で取得し callees / reexports は空 (tree-sitter parse を呼ばない)。
#[test]
fn extract_new_file_facts_xojo_lexer_only_no_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Sample.xojo_code"),
        "Class Sample\nSub Greet()\nEnd Sub\nEnd Class\n",
    )
    .expect("write");

    let facts = extract_new_file_facts(dir.path().to_str().expect("utf-8"), "Sample.xojo_code");
    assert!(
        facts.exported.is_some(),
        "lexer-only でも exported は Some (lexer 経由)"
    );
    assert!(
        facts.callees.is_empty() && facts.export_surface_names.is_empty(),
        "lexer-only では callees / export surface は空 (tree-sitter parse を呼ばない)"
    );
}

/// React.memo ラップで宣言種別が function_declaration → lexical_declaration に変わった
/// api.mod は、props 型 (複数行 destructured 含む)・JSX 利用互換なら compatible
/// (react_component_wrapper) に降格する。(レポート 2026-06-02-react-memo-api-mod.md の再現)
#[test]
fn detect_react_wrapper_multiline_destructured_props_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    // old: export function (複数行 destructured props) + JSX のみで参照するファイル
    git_commit_files(
        repo,
        &[
            (
                "ScheduleItem.tsx",
                "export function ScheduleItem({\n  a,\n  b,\n}: ScheduleItemProps) {\n  return null;\n}\n",
            ),
            (
                "TrayPopup.tsx",
                "import { ScheduleItem } from './ScheduleItem';\nexport function TrayPopup() {\n  return <ScheduleItem a={1} b={2} />;\n}\n",
            ),
        ],
        "initial",
    );
    // new: memo ラップ (working tree)
    fs::write(
            repo.join("ScheduleItem.tsx"),
            "import { memo } from 'react';\nexport const ScheduleItem = memo(function ScheduleItem({\n  a,\n  b,\n}: ScheduleItemProps) {\n  return null;\n});\n",
        )
        .expect("write");
    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["ScheduleItem".to_string()]),
    );
    let result = detect_react_wrapper_compatible_mod(
        &ref_index,
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "ScheduleItem.tsx",
        "ScheduleItem.tsx",
        "ScheduleItem",
        "constant",
        "export function ScheduleItem({}: ScheduleItemProps)",
        "export const ScheduleItem = memo(function ScheduleItem({",
        Some(crate::language::LangId::Tsx),
    );
    let compat = result.expect("複数行 destructured props でも memo ラップのみなら compatible");
    assert_eq!(compat.reason, "react_component_wrapper");
    assert_eq!(compat.name, "ScheduleItem");
}

/// memo ラップでもシンボルが関数として直接呼び出されている (`X(...)`) 場合は
/// MemoExoticComponent 化で壊れ得るため blocking (api.mod) を維持する。
#[test]
fn detect_react_wrapper_with_call_usage_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "ScheduleItem.tsx",
                "export function ScheduleItem(props: P) {\n  return null;\n}\n",
            ),
            (
                "usage.tsx",
                "import { ScheduleItem } from './ScheduleItem';\nconst rendered = ScheduleItem({});\n",
            ),
        ],
        "initial",
    );
    fs::write(
            repo.join("ScheduleItem.tsx"),
            "import { memo } from 'react';\nexport const ScheduleItem = memo(function ScheduleItem(props: P) {\n  return null;\n});\n",
        )
        .expect("write");
    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["ScheduleItem".to_string()]),
    );
    let result = detect_react_wrapper_compatible_mod(
        &ref_index,
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "ScheduleItem.tsx",
        "ScheduleItem.tsx",
        "ScheduleItem",
        "constant",
        "old",
        "new",
        Some(crate::language::LangId::Tsx),
    );
    assert!(
        result.is_none(),
        "X(...) 直接呼び出しがあれば blocking 維持 (MemoExoticComponent 非互換)"
    );
}

/// props 型が変わった場合は互換でないため blocking を維持する。
#[test]
fn detect_react_wrapper_changed_props_type_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "ScheduleItem.tsx",
                "export function ScheduleItem(props: OldProps) {\n  return null;\n}\n",
            ),
            (
                "TrayPopup.tsx",
                "import { ScheduleItem } from './ScheduleItem';\nexport const x = <ScheduleItem />;\n",
            ),
        ],
        "initial",
    );
    fs::write(
            repo.join("ScheduleItem.tsx"),
            "import { memo } from 'react';\nexport const ScheduleItem = memo(function ScheduleItem(props: NewProps) {\n  return null;\n});\n",
        )
        .expect("write");
    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["ScheduleItem".to_string()]),
    );
    let result = detect_react_wrapper_compatible_mod(
        &ref_index,
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "ScheduleItem.tsx",
        "ScheduleItem.tsx",
        "ScheduleItem",
        "constant",
        "old",
        "new",
        Some(crate::language::LangId::Tsx),
    );
    assert!(result.is_none(), "props 型が変われば blocking 維持");
}

#[test]
fn extract_component_props_type_handles_function_and_memo_wrapper() {
    // 複数行 destructured props + function 宣言
    let func = b"export function X({\n  a,\n  b,\n}: MyProps) { return null; }\n";
    assert_eq!(
        extract_component_props_type(func, crate::language::LangId::Tsx, "X").as_deref(),
        Some(": MyProps")
    );
    // memo ラップ (内側 function の第1引数を見る)
    let memo = b"import { memo } from 'react';\nexport const X = memo(function X({\n  a,\n}: MyProps) { return null; });\n";
    assert_eq!(
        extract_component_props_type(memo, crate::language::LangId::Tsx, "X").as_deref(),
        Some(": MyProps")
    );
    // 型注釈なしは None (blocking 維持)
    let no_type = b"export function X(props) { return null; }\n";
    assert_eq!(
        extract_component_props_type(no_type, crate::language::LangId::Tsx, "X"),
        None
    );
}

/// 定義ファイル内に named function expression 以外の値利用 (`X({})` 呼び出し) が残る
/// 場合は MemoExoticComponent 化で壊れ得るため blocking 維持 (codex 指摘: def_file 全体
/// 除外でなく named fn 名だけ safe)。
#[test]
fn detect_react_wrapper_same_file_value_usage_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "ScheduleItem.tsx",
            "export function ScheduleItem(props: P) {\n  return null;\n}\nconst probe = ScheduleItem({});\n",
        )],
        "initial",
    );
    fs::write(
            repo.join("ScheduleItem.tsx"),
            "import { memo } from 'react';\nexport const ScheduleItem = memo(function ScheduleItem(props: P) {\n  return null;\n});\nconst probe = ScheduleItem({});\n",
        )
        .expect("write");
    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["ScheduleItem".to_string()]),
    );
    let result = detect_react_wrapper_compatible_mod(
        &ref_index,
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "ScheduleItem.tsx",
        "ScheduleItem.tsx",
        "ScheduleItem",
        "constant",
        "old",
        "new",
        Some(crate::language::LangId::Tsx),
    );
    assert!(
        result.is_none(),
        "同一ファイル内の値呼び出し ScheduleItem({{}}) があれば blocking 維持"
    );
}

/// old 側が既に wrapper (forwardRef) の wrapper-to-wrapper 変更は、ref 型等の差分を
/// 取りこぼすため blocking 維持 (codex 指摘)。
#[test]
fn detect_react_wrapper_old_already_wrapper_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "Btn.tsx",
                "import { forwardRef } from 'react';\nexport const Btn = forwardRef(function Btn(props: P, ref: RefA) {\n  return null;\n});\n",
            ),
            (
                "App.tsx",
                "import { Btn } from './Btn';\nexport const x = <Btn />;\n",
            ),
        ],
        "initial",
    );
    fs::write(
            repo.join("Btn.tsx"),
            "import { forwardRef } from 'react';\nexport const Btn = forwardRef(function Btn(props: P, ref: RefB) {\n  return null;\n});\n",
        )
        .expect("write");
    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["Btn".to_string()]),
    );
    let result = detect_react_wrapper_compatible_mod(
        &ref_index,
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "Btn.tsx",
        "Btn.tsx",
        "Btn",
        "constant",
        "export const Btn = forwardRef(function Btn(props: P, ref: RefA) {",
        "export const Btn = forwardRef(function Btn(props: P, ref: RefB) {",
        Some(crate::language::LangId::Tsx),
    );
    assert!(
        result.is_none(),
        "old が既に wrapper (wrapper-to-wrapper) なら blocking 維持"
    );
}

/// compatible_modified (mod_compat) のみの api 変更は informational として hook JSON に
/// 出すが blocking にはしない。
#[test]
fn build_review_hook_json_compatible_modified_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: vec![CompatibleApiModification {
                name: "ScheduleItem".to_string(),
                kind: "constant".to_string(),
                file: "ScheduleItem.tsx".to_string(),
                old_signature: None,
                new_signature: None,
                reason: "react_component_wrapper".to_string(),
            }],
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };
    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(
        build.value.is_some(),
        "mod_compat は情報提供として hook JSON に出すべき"
    );
    assert!(!build.is_blocking, "mod_compat (互換変更) は非 blocking");
}

/// mod_compat に分類済みのシンボルに紐づく cross-file impact は、破壊的影響ではなく
/// 参考情報として扱う。api 側だけ非 blocking でも impacts が残ると Stop hook が
/// 自己矛盾した blocking 出力になるため、同じシンボルの impacts から除外する。
#[test]
fn build_review_hook_json_compatible_modified_impact_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("TaskDetailHeader.tsx"),
        "export const TaskDetailHeader = memo(function TaskDetailHeader() { return null; });\n",
    )
    .expect("write changed file");
    fs::write(
        dir.path().join("TaskDetailContent.tsx"),
        "export const view = <TaskDetailHeader />;\n",
    )
    .expect("write caller file");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "TaskDetailHeader.tsx".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![crate::models::impact::AffectedSymbol {
                    name: "TaskDetailHeader".to_string(),
                    kind: "function".to_string(),
                    change_type: "modified".to_string(),
                }],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "TaskDetailContent.tsx".to_string(),
                    name: "TaskDetailContent".to_string(),
                    line: 1,
                    symbols: vec!["TaskDetailHeader".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: vec![CompatibleApiModification {
                name: "TaskDetailHeader".to_string(),
                kind: "function".to_string(),
                file: "TaskDetailHeader.tsx".to_string(),
                old_signature: Some("export function TaskDetailHeader()".to_string()),
                new_signature: Some(
                    "export const TaskDetailHeader = memo(function TaskDetailHeader()".to_string(),
                ),
                reason: "react_component_wrapper".to_string(),
            }],
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("hook json should be generated");
    assert!(
        !build.is_blocking,
        "mod_compat 起因の impact だけなら Stop hook を止めないべき"
    );
    assert!(
        hook_json.get("impacts").is_none(),
        "mod_compat と同じシンボルの impacts は hook の blocking 出力から除外されるべき"
    );
    assert_eq!(
        hook_json["api"]["mod_compat"][0]["reason"],
        "react_component_wrapper"
    );
}

#[test]
fn build_review_hook_json_mixed_compatible_and_breaking_impact_keeps_breaking_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("TaskDetailHeader.tsx"),
        "export const TaskDetailHeader = memo(function TaskDetailHeader() { return null; });\nexport function loadTask(id: string, required: boolean) {}\n",
    )
    .expect("write changed file");
    fs::write(
        dir.path().join("TaskDetailContent.tsx"),
        "export const view = <TaskDetailHeader />;\nloadTask('1');\n",
    )
    .expect("write caller file");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "TaskDetailHeader.tsx".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![
                    crate::models::impact::AffectedSymbol {
                        name: "TaskDetailHeader".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    },
                    crate::models::impact::AffectedSymbol {
                        name: "loadTask".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    },
                ],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "TaskDetailContent.tsx".to_string(),
                    name: "TaskDetailContent".to_string(),
                    line: 2,
                    symbols: vec!["TaskDetailHeader".to_string(), "loadTask".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: vec![ApiSymbolChange {
                name: "loadTask".to_string(),
                kind: "function".to_string(),
                file: "TaskDetailHeader.tsx".to_string(),
                old_signature: Some("export function loadTask(id: string)".to_string()),
                new_signature: Some(
                    "export function loadTask(id: string, required: boolean)".to_string(),
                ),
            }],
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: vec![CompatibleApiModification {
                name: "TaskDetailHeader".to_string(),
                kind: "function".to_string(),
                file: "TaskDetailHeader.tsx".to_string(),
                old_signature: Some("export function TaskDetailHeader()".to_string()),
                new_signature: Some(
                    "export const TaskDetailHeader = memo(function TaskDetailHeader()".to_string(),
                ),
                reason: "react_component_wrapper".to_string(),
            }],
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("hook json should be generated");
    assert!(
        build.is_blocking,
        "破壊的 api.mod が同じ caller にあれば blocking を維持すべき"
    );
    let impacts = hook_json["impacts"].as_array().expect("impacts array");
    assert_eq!(impacts[0]["syms"], serde_json::json!(["loadTask"]));
    assert_eq!(impacts[0]["refs"][0]["s"], serde_json::json!(["loadTask"]));
}

#[test]
fn detect_api_changes_ts_trailing_optional_param_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/task-progress.ts",
                "export function computeTaskProgress(phases: string[]): number {\n  return phases.length;\n}\n",
            ),
            (
                "src/components/task-detail.ts",
                "import { computeTaskProgress } from '../lib/task-progress';\nexport const progress = computeTaskProgress([]);\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib/task-progress.ts"),
        "export function computeTaskProgress(phases: string[], description?: string | null): number {\n  return phases.length + (description?.length ?? 0);\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/task-progress.ts".to_string(),
        new_path: "src/lib/task-progress.ts".to_string(),
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
        api_changes.modified.is_empty(),
        "末尾 optional 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "computeTaskProgress")
        .expect("compatible_modified に computeTaskProgress が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

#[test]
fn detect_api_changes_ts_trailing_default_param_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/task-progress.ts",
                "export function computeTaskProgress(phases: string[]): number {\n  return phases.length;\n}\n",
            ),
            (
                "src/components/task-detail.ts",
                "import { computeTaskProgress } from '../lib/task-progress';\nexport const progress = computeTaskProgress([]);\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib/task-progress.ts"),
        "export function computeTaskProgress(phases: string[], description: string | null = null): number {\n  return phases.length + (description?.length ?? 0);\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/task-progress.ts".to_string(),
        new_path: "src/lib/task-progress.ts".to_string(),
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
        api_changes.modified.is_empty(),
        "末尾 default 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "computeTaskProgress")
        .expect("compatible_modified に computeTaskProgress が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

/// Issue 2026-07-12-ts-class-method-trailing-optional-param-api-mod: TS class method への
/// 末尾 optional 引数追加も compatible_modified へ降格する。同名 standalone 関数が別
/// ファイルにあっても `Class.method` qualname で一意解決できるため成立する。
#[test]
fn detect_api_changes_ts_class_method_trailing_optional_param_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/widget.ts",
                "export class Widget {\n  handle(a: string, b: number): void {\n    console.log(a, b);\n  }\n}\n",
            ),
            (
                "src/user.ts",
                "import { Widget } from './widget';\n\nexport function run(w: Widget) {\n  w.handle(\"x\", 1);\n}\n",
            ),
            (
                "src/other/standalone.ts",
                "export function handle(msg: string): void {\n  console.log(msg);\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/widget.ts"),
        "export class Widget {\n  handle(a: string, b: number, c?: readonly number[]): void {\n    console.log(a, b, c);\n  }\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
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
            .any(|m| m.name == "Widget.handle"),
        "class method への末尾 optional 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "Widget.handle")
        .expect("compatible_modified に Widget.handle が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

/// 負ケース: class method へ追加した末尾引数が required なら従来どおり blocking。
#[test]
fn detect_api_changes_ts_class_method_trailing_required_param_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/widget.ts",
                "export class Widget {\n  handle(a: string, b: number): void {\n    console.log(a, b);\n  }\n}\n",
            ),
            (
                "src/user.ts",
                "import { Widget } from './widget';\n\nexport function run(w: Widget) {\n  w.handle(\"x\", 1);\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/widget.ts"),
        "export class Widget {\n  handle(a: string, b: number, c: readonly number[]): void {\n    console.log(a, b, c);\n  }\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
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
        api_changes
            .modified
            .iter()
            .any(|m| m.name == "Widget.handle"),
        "required 引数の追加は blocking な modified を維持すべき: modified={:?} compat={:?}",
        api_changes
            .modified
            .iter()
            .map(|m| &m.name)
            .collect::<Vec<_>>(),
        api_changes
            .compatible_modified
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
}

/// 負ケース: 新側に同名 overload signature が併存する場合は単一 method_definition へ
/// 安全に対応付けられないため blocking を維持する。
#[test]
fn detect_api_changes_ts_class_method_with_overload_signature_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/widget.ts",
                "export class Widget {\n  handle(a: string, b: number): void {\n    console.log(a, b);\n  }\n}\n",
            ),
            (
                "src/user.ts",
                "import { Widget } from './widget';\n\nexport function run(w: Widget) {\n  w.handle(\"x\", 1);\n}\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/widget.ts"),
        "export class Widget {\n  handle(a: string, b: number): void;\n  handle(a: string, b: number, c?: readonly number[]): void;\n  handle(a: string, b: number, c?: readonly number[]): void {\n    console.log(a, b, c);\n  }\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/widget.ts".to_string(),
        new_path: "src/widget.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        !api_changes
            .compatible_modified
            .iter()
            .any(|c| c.name == "Widget.handle"),
        "overload signature 併存時は互換降格しない: compat={:?}",
        api_changes
            .compatible_modified
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
}

/// Python のトップレベル関数に末尾 keyword-only + default 引数を追加した変更は
/// 既存呼び出しを壊さないため、`compatible_modified` (`trailing_optional_params`) に降格する。
/// (レポート 2026-06-18-api-mod-backward-compatible-kwarg の再現)
#[test]
fn detect_api_changes_python_trailing_kwonly_default_param_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/uploader.py",
                "def upload_category_spreadsheets(items):\n    return items\n",
            ),
            (
                "src/main.py",
                "from uploader import upload_category_spreadsheets\n\nresult = upload_category_spreadsheets([])\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/uploader.py"),
        "def upload_category_spreadsheets(items, *, max_spreadsheet_bytes=600_000):\n    return items\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/uploader.py".to_string(),
        new_path: "src/uploader.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.modified.is_empty(),
        "末尾 kwonly+default 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "upload_category_spreadsheets")
        .expect("compatible_modified に upload_category_spreadsheets が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

/// Python トップレベル関数の末尾 positional default 引数追加も同様に compatible_modified に降格する。
#[test]
fn detect_api_changes_python_trailing_default_positional_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/lib/helpers.py",
            "def render(items):\n    return items\n",
        )],
        "initial",
    );
    fs::write(
        repo.join("src/lib/helpers.py"),
        "def render(items, limit=10):\n    return items[:limit]\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/helpers.py".to_string(),
        new_path: "src/lib/helpers.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.modified.is_empty(),
        "末尾 default 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "render")
        .expect("compatible_modified に render が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

/// Python モジュール直下クラスのメソッドに末尾 kwonly+default 引数を追加した変更も
/// `compatible_modified` (`trailing_optional_params`) に降格する。
#[test]
fn detect_api_changes_python_class_method_trailing_kwonly_default_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/svc.py",
                "class Service:\n    def emit(self, payload):\n        return payload\n",
            ),
            (
                "src/main.py",
                "from svc import Service\n\nresult = Service().emit({})\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/svc.py"),
        "class Service:\n    def emit(self, payload, *, retry=0):\n        return payload\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/svc.py".to_string(),
        new_path: "src/svc.py".to_string(),
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
        api_changes
            .modified
            .iter()
            .all(|c| c.name != "Service.emit"),
        "末尾 kwonly+default 引数追加は破壊的 api.mod にすべきでない: {:?}",
        api_changes.modified
    );
    let compat = api_changes
        .compatible_modified
        .iter()
        .find(|c| c.name == "Service.emit")
        .expect("compatible_modified に Service.emit が入るべき");
    assert_eq!(compat.reason, "trailing_optional_params");
}

/// Python デコレータが変わった場合 (例: `@staticmethod` → `@classmethod`) は
/// default 引数追加が併走しても compatible_modified に降格せず modified に残るべき。
/// (呼び出し時の cls / self bind が変わり既存呼出を壊しうるため)
#[test]
fn detect_api_changes_python_decorator_change_with_optional_param_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/svc.py",
                "class Service:\n    @staticmethod\n    def emit(payload):\n        return payload\n",
            ),
            (
                "src/main.py",
                "from svc import Service\n\nresult = Service.emit({})\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/svc.py"),
        "class Service:\n    @classmethod\n    def emit(cls, payload, *, retry=0):\n        return payload\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/svc.py".to_string(),
        new_path: "src/svc.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes
            .compatible_modified
            .iter()
            .all(|c| c.name != "Service.emit"),
        "デコレータ変更 + default 引数追加は compatible_modified に降格してはならない: {:?}",
        api_changes.compatible_modified
    );
}

/// Python の必須引数 (default 無し) 追加は依然 modified (api.mod) として残るべき。
#[test]
fn detect_api_changes_python_trailing_required_param_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "src/lib/helpers.py",
            "def render(items):\n    return items\n",
        )],
        "initial",
    );
    fs::write(
        repo.join("src/lib/helpers.py"),
        "def render(items, limit):\n    return items[:limit]\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/helpers.py".to_string(),
        new_path: "src/lib/helpers.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api_changes.compatible_modified.is_empty(),
        "required 引数追加を compatible_modified に降格してはならない: {:?}",
        api_changes.compatible_modified
    );
    assert!(
        api_changes.modified.iter().any(|c| c.name == "render"),
        "required 引数追加は api.mod に残るべき: {:?}",
        api_changes.modified
    );
}

#[test]
fn detect_api_changes_ts_trailing_required_param_stays_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib/task-progress.ts",
                "export function computeTaskProgress(phases: string[]): number {\n  return phases.length;\n}\n",
            ),
            (
                "src/components/task-detail.ts",
                "import { computeTaskProgress } from '../lib/task-progress';\nexport const progress = computeTaskProgress([]);\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib/task-progress.ts"),
        "export function computeTaskProgress(phases: string[], description: string): number {\n  return phases.length + description.length;\n}\n",
    )
    .expect("write changed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib/task-progress.ts".to_string(),
        new_path: "src/lib/task-progress.ts".to_string(),
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
        api_changes.compatible_modified.is_empty(),
        "required 引数追加を compatible_modified に降格してはならない: {:?}",
        api_changes.compatible_modified
    );
    assert!(
        api_changes
            .modified
            .iter()
            .any(|c| c.name == "computeTaskProgress"),
        "required 引数追加は api.mod に残るべき: {:?}",
        api_changes.modified
    );
}

/// exported object のプロパティ削除で、削除キーへの member access が repo 全体で 0 件なら
/// compatible (unused_object_members) に降格する。(レポート 2026-06-02-provider-avatar の再現)
#[test]
fn detect_object_members_removed_unreferenced_key_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    // old: record value に bgColor。参照側は label のみ使用 (bgColor は未参照)
    git_commit_files(
        repo,
        &[
            (
                "config.tsx",
                "export const providerConfig = {\n  google: { label: 'G', bgColor: 'green' },\n};\n",
            ),
            (
                "App.tsx",
                "import { providerConfig } from './config';\nexport const x = providerConfig.google.label;\n",
            ),
        ],
        "initial",
    );
    // new: bgColor 削除 + bgClass 追加
    fs::write(
        repo.join("config.tsx"),
        "export const providerConfig = {\n  google: { label: 'G', bgClass: 'bg-green' },\n};\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "config.tsx",
        "config.tsx",
        "providerConfig",
        "constant",
        "old",
        "new",
        Some(crate::language::LangId::Tsx),
    );
    let compat = result.expect("削除キー bgColor が未参照なら compatible");
    assert_eq!(compat.reason, "unused_object_members");
}

/// 削除されたキーが member access (`config.google.bgColor`) で残存していれば破壊的なので
/// blocking 維持。
#[test]
fn detect_object_members_removed_referenced_key_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "config.tsx",
                "export const providerConfig = {\n  google: { label: 'G', bgColor: 'green' },\n};\n",
            ),
            (
                "App.tsx",
                "import { providerConfig } from './config';\nexport const x = providerConfig.google.bgColor;\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const providerConfig = {\n  google: { label: 'G', bgClass: 'bg-green' },\n};\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "config.tsx",
        "config.tsx",
        "providerConfig",
        "constant",
        "old",
        "new",
        Some(crate::language::LangId::Tsx),
    );
    assert!(
        result.is_none(),
        "削除キー bgColor が member access で残存 → blocking 維持"
    );
}

/// key 集合が完全一致する initializer 値のみ変更は unused_object_members ではないため
/// blocking 維持。
#[test]
fn detect_object_members_value_only_change_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("config.tsx", "export const c = { enabled: true };\n")],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const c = { enabled: false };\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "config.tsx",
        "config.tsx",
        "c",
        "constant",
        "old",
        "new",
        Some(crate::language::LangId::Tsx),
    );
    assert!(result.is_none(), "値のみ変更は blocking 維持");
}

/// 削除 key なし、追加 key ありの純粋な member 追加は compatible に降格する。
#[test]
fn detect_object_members_pure_member_addition_is_compatible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("config.tsx", "export const c = { enabled: true };\n")],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const c = { enabled: true, mode: 'dark' };\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "config.tsx",
        "config.tsx",
        "c",
        "constant",
        "old",
        "new",
        Some(crate::language::LangId::Tsx),
    );
    let compat = result.expect("純粋な member 追加は compatible");
    assert_eq!(compat.reason, "unused_object_members");
}

/// record value の schema が不揃いなら、同名 key が別 record entry に残っていても
/// 削除有無を安全に判断できないため blocking 維持。
#[test]
fn detect_object_members_record_schema_mismatch_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[(
            "config.tsx",
            "export const providerConfig = {\n  google: { label: 'G', bgColor: 'green' },\n  openai: { label: 'O', bgColor: 'blue' },\n};\n",
        )],
        "initial",
    );
    fs::write(
            repo.join("config.tsx"),
            "export const providerConfig = {\n  google: { label: 'G' },\n  openai: { label: 'O', bgColor: 'blue' },\n};\n",
        )
        .expect("write");
    let result = detect_object_members_compatible_mod(
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "config.tsx",
        "config.tsx",
        "providerConfig",
        "constant",
        "old",
        "new",
        Some(crate::language::LangId::Tsx),
    );
    assert!(result.is_none(), "record schema 不揃いは blocking 維持");
}

/// 削除された key が文字列 bracket access (`obj["key"]`) で残存していれば破壊的なので
/// blocking 維持。
#[test]
fn detect_object_members_removed_bracket_string_ref_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "config.tsx",
                "export const providerConfig = {\n  google: { label: 'G', bgColor: 'green' },\n};\n",
            ),
            (
                "App.tsx",
                "import { providerConfig } from './config';\nexport const x = providerConfig.google[\"bgColor\"];\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const providerConfig = {\n  google: { label: 'G' },\n};\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "config.tsx",
        "config.tsx",
        "providerConfig",
        "constant",
        "old",
        "new",
        Some(crate::language::LangId::Tsx),
    );
    assert!(
        result.is_none(),
        "削除キー bgColor が bracket string access で残存 → blocking 維持"
    );
}

/// spread (`...base`) を含む object は shape を静的確定できないため blocking 維持。
#[test]
fn detect_object_members_spread_stays_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("config.tsx", "export const c = { ...base, a: 1 };\n")],
        "initial",
    );
    fs::write(
        repo.join("config.tsx"),
        "export const c = { ...base, b: 2 };\n",
    )
    .expect("write");
    let result = detect_object_members_compatible_mod(
        repo.to_str().expect("utf-8 path"),
        "HEAD",
        "config.tsx",
        "config.tsx",
        "c",
        "constant",
        "old",
        "new",
        Some(crate::language::LangId::Tsx),
    );
    assert!(result.is_none(), "spread を含む object は blocking 維持");
}

#[test]
fn extract_object_member_keys_collects_record_value_keys() {
    // record 形式: top-level は record entry、value object のキー (label/bgColor) を schema とする
    let src = b"export const c = {\n  google: { label: 'G', bgColor: 'green' },\n  openai: { label: 'O', bgColor: 'blue' },\n};\n";
    let keys =
        extract_object_member_keys(src, crate::language::LangId::Tsx, "c").expect("record keys");
    assert!(keys.member_keys.contains("label"));
    assert!(keys.member_keys.contains("bgColor"));
    let record_keys = keys.record_keys.expect("record keys");
    assert!(record_keys.contains("google"));
    assert!(record_keys.contains("openai"));
    // spread を含むと None (blocking)
    let spread = b"export const c = { ...base, a: 1 };\n";
    assert!(extract_object_member_keys(spread, crate::language::LangId::Tsx, "c").is_none());
}

#[test]
fn new_sig_has_react_wrapper_detects_hocs() {
    assert!(new_sig_has_react_wrapper(
        "export const X = memo(function X() {"
    ));
    assert!(new_sig_has_react_wrapper(
        "export const X = React.forwardRef(function X() {"
    ));
    // 単なる function 宣言や部分一致 (somememo) はラッパーでない
    assert!(!new_sig_has_react_wrapper("export function X(props: T)"));
    assert!(!new_sig_has_react_wrapper("export const X = somememo(fn)"));
}

#[test]
fn ctx_usage_classification_jsx_vs_value() {
    // JSX タグ利用は safe
    assert!(ctx_usage_is_jsx_or_safe(
        "return <ScheduleItem foo={1} />;",
        "ScheduleItem"
    ));
    assert!(ctx_usage_is_jsx_or_safe(
        "  </ScheduleItem>",
        "ScheduleItem"
    ));
    // 値利用は blocking 側
    assert!(!ctx_usage_is_jsx_or_safe(
        "const x = ScheduleItem({});",
        "ScheduleItem"
    ));
    assert!(!ctx_usage_is_jsx_or_safe(
        "typeof ScheduleItem",
        "ScheduleItem"
    ));
    assert!(!ctx_usage_is_jsx_or_safe(
        "ScheduleItem.displayName = 'x';",
        "ScheduleItem"
    ));
    // 裸の代入は判定不能なので blocking 側
    assert!(!ctx_usage_is_jsx_or_safe(
        "const Alias = ScheduleItem;",
        "ScheduleItem"
    ));
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

/// 他ファイルから参照される関数のシグネチャ変更は api.mod に残す（false negative 防止）。
/// 同一ファイル内でも呼び出しが存在するが、他ファイルから import/call されている場合は
/// closed-in-diff とは言えないため、レビュー対象として残す必要がある。
#[test]
fn detect_api_changes_externally_called_signature_change_is_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let lib_before = "\
def run(value: int) -> int:
    return value


def wrapper() -> int:
    return run(1)
";
    let caller_before = "\
from lib import run


def main() -> int:
    return run(2)
";
    git_commit_files(
        repo,
        &[("lib.py", lib_before), ("caller.py", caller_before)],
        "initial",
    );

    // lib.run のシグネチャを変更（必須引数追加）。caller.py は diff に含まれない（追随なし）。
    // 必須引数追加は後方互換でないため compatible_modified に降格せず modified に残るべき。
    let lib_after = "\
def run(value: int, flag: bool) -> int:
    if flag:
        return value
    return value


def wrapper() -> int:
    return run(1, False)
";
    fs::write(repo.join("lib.py"), lib_after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib.py".to_string(),
        new_path: "lib.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 6,
            new_start: 1,
            new_count: 6,
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
        mod_names.contains(&"run"),
        "他ファイルから参照される関数のシグネチャ変更は api.mod に残すべき。got: {mod_names:?}"
    );
}

/// 後方互換なオプショナル引数の追加（末尾にデフォルト値付き引数を追加）は、
/// closed-in-diff 判定により api.mod から除外される。
/// (レポート追記 2026-04-22 コミット c045fdf `json_to_markdown` の再現)
#[test]
fn detect_api_changes_optional_arg_addition_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
def json_to_markdown(raw, impact_file=None):
    return str(raw)


def _finalize_result(raw):
    return json_to_markdown(raw)


if __name__ == \"__main__\":
    _finalize_result({})
";
    git_commit_files(repo, &[("review_mr.py", before)], "initial");

    let after = "\
def json_to_markdown(raw, impact_file=None, osv_scan_file=None):
    return str(raw)


def _finalize_result(raw):
    return json_to_markdown(raw, impact_file=None, osv_scan_file=None)


if __name__ == \"__main__\":
    _finalize_result({})
";
    fs::write(repo.join("review_mr.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "review_mr.py".to_string(),
        new_path: "review_mr.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 10,
            new_start: 1,
            new_count: 10,
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
        !mod_names.contains(&"json_to_markdown"),
        "同一ファイル内でのみ呼ばれる関数へのオプショナル引数追加は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// CLI スクリプト内で関数を rename + 実装置換した場合、api.rm に残してはならない。
/// `api.rm { old_name }` + `api.add { new_name }` の両方が closed-in-diff として
/// 扱えることを確認する。
/// (レポート追記 2026-04-22 コミット 3f2b082 `detect_changed_manifests` の再現)
#[test]
fn detect_api_changes_rename_with_impl_replacement_not_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
def detect_changed_manifests(base, head):
    return []


def main():
    files = detect_changed_manifests(\"a\", \"b\")
    return files


if __name__ == \"__main__\":
    main()
";
    git_commit_files(repo, &[("osv_scan.py", before)], "initial");

    // detect_changed_manifests を削除し、同じ diff 内で list_changed_files を追加。
    // caller (main) も list_changed_files に追随。
    let after = "\
def list_changed_files(base, head):
    return []


def main():
    files = list_changed_files(\"a\", \"b\")
    return files


if __name__ == \"__main__\":
    main()
";
    fs::write(repo.join("osv_scan.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "osv_scan.py".to_string(),
        new_path: "osv_scan.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 10,
            new_start: 1,
            new_count: 10,
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
    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

    assert!(
        !removed.contains(&"detect_changed_manifests"),
        "同一 diff 内で新規関数に切り替わった関数の削除は api.rm に出してはならない。got: {removed:?}"
    );
    // 新規関数側も is_internally_connected により除外される（main から呼ばれている）。
    assert!(
        !added.contains(&"list_changed_files"),
        "同一ファイル内でのみ呼ばれる新規関数は api.add に出してはならない。got: {added:?}"
    );
}

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

/// lib.rs 有りクレートでも、新規 pub シンボルが同一 diff 内の別ファイルから
/// 参照されていれば api.add から除外する。
#[test]
fn detect_api_changes_library_used_in_same_diff_excluded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let cargo_toml = "\
[package]
name = \"demo-lib\"
version = \"0.1.0\"
edition = \"2021\"
";
    let lib_before = "pub mod models;\npub mod consumer;\n";
    let models_before = "pub struct Issue { pub id: u32 }\n";
    let consumer_before = "use crate::models::Issue;\n\npub fn use_issue(i: Issue) {}\n";
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", cargo_toml),
            ("src/lib.rs", lib_before),
            ("src/models.rs", models_before),
            ("src/consumer.rs", consumer_before),
        ],
        "initial",
    );

    // models に新規 pub struct を追加し、同一 diff 内で consumer.rs から参照
    let models_after = "\
pub struct Issue { pub id: u32 }

pub struct MrDiff { pub path: String }
";
    let consumer_after = "\
use crate::models::{Issue, MrDiff};

pub fn use_issue(i: Issue) {}
pub fn use_diff(d: MrDiff) {}
";
    fs::write(repo.join("src/models.rs"), models_after).expect("write models");
    fs::write(repo.join("src/consumer.rs"), consumer_after).expect("write consumer");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/models.rs".to_string(),
            new_path: "src/models.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 4,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/consumer.rs".to_string(),
            new_path: "src/consumer.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 5,
            }],
            deleted_old_source: None,
        },
    ];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !added.contains(&"MrDiff"),
        "同一 diff 内で参照される新規 pub struct は api.add から除外すべき。got: {added:?}"
    );
}

// ------------------------------------------------------------------
// is_internally_connected ヘルパー
// ------------------------------------------------------------------

#[test]
fn is_internally_connected_matches_bare_name() {
    let mut callees = std::collections::HashSet::new();
    callees.insert("foo".to_string());
    assert!(is_internally_connected(&callees, "foo"));
    assert!(!is_internally_connected(&callees, "bar"));
}

#[test]
fn is_internally_connected_matches_qualname_via_bare() {
    let mut callees = std::collections::HashSet::new();
    // Python/Ruby 等では callee 側は bare name のみになることが多い
    callees.insert("execute".to_string());
    assert!(is_internally_connected(&callees, "Reviewer.execute"));
}

#[test]
fn is_internally_connected_does_not_match_disjoint() {
    let mut callees = std::collections::HashSet::new();
    callees.insert("other_fn".to_string());
    assert!(!is_internally_connected(&callees, "Reviewer.execute"));
    assert!(!is_internally_connected(&callees, "execute"));
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

// ------------------------------------------------------------------
// auto_detect_framework ヘルパー
// ------------------------------------------------------------------

#[test]
fn auto_detect_framework_returns_none_without_package_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(auto_detect_framework(dir.path().to_str().expect("utf-8")).is_none());
}

#[test]
fn auto_detect_framework_returns_nextjs_for_dependencies() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("package.json"),
        r#"{"dependencies": {"next": "14.0.0", "react": "18.0.0"}}"#,
    )
    .expect("pkg");
    assert_eq!(
        auto_detect_framework(dir.path().to_str().expect("utf-8")),
        Some("nextjs")
    );
}

#[test]
fn auto_detect_framework_returns_nextjs_for_dev_dependencies() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("package.json"),
        r#"{"devDependencies": {"next": "14.0.0"}}"#,
    )
    .expect("pkg");
    assert_eq!(
        auto_detect_framework(dir.path().to_str().expect("utf-8")),
        Some("nextjs")
    );
}

/// `peerDependencies` / `optionalDependencies` 経由の `next` は library 側の同梱で
/// 誤爆しやすいため対象外とする。
#[test]
fn auto_detect_framework_ignores_peer_dependencies() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("package.json"),
        r#"{"peerDependencies": {"next": "14.0.0"}}"#,
    )
    .expect("pkg");
    assert!(auto_detect_framework(dir.path().to_str().expect("utf-8")).is_none());
}

#[test]
fn auto_detect_framework_returns_none_for_invalid_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("package.json"), "{not valid json").expect("pkg");
    assert!(auto_detect_framework(dir.path().to_str().expect("utf-8")).is_none());
}

#[test]
fn auto_detect_framework_returns_none_when_no_next_dependency() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("package.json"),
        r#"{"dependencies": {"react": "18.0.0"}}"#,
    )
    .expect("pkg");
    assert!(auto_detect_framework(dir.path().to_str().expect("utf-8")).is_none());
}

/// `resolve_framework_globs_with_auto_detect`: 明示指定があれば auto detect は無視する。
#[test]
fn resolve_framework_globs_with_auto_detect_prefers_explicit_framework() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 明示指定が `laravel` のとき、package.json に next があっても laravel プリセットを返す。
    fs::write(
        dir.path().join("package.json"),
        r#"{"dependencies": {"next": "14.0.0"}}"#,
    )
    .expect("pkg");
    let globs = resolve_framework_globs_with_auto_detect(
        Some("laravel"),
        dir.path().to_str().expect("utf-8"),
    )
    .expect("resolve");
    // Laravel プリセットの代表 glob `**/app/Http/**` が含まれていることだけ確認する。
    assert!(globs.iter().any(|g| g.contains("Http")));
}

/// auto detect 経由でも明示指定無し時は nextjs プリセットが返ること。
#[test]
fn resolve_framework_globs_with_auto_detect_uses_auto_when_no_explicit() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("package.json"),
        r#"{"dependencies": {"next": "14.0.0"}}"#,
    )
    .expect("pkg");
    let globs = resolve_framework_globs_with_auto_detect(None, dir.path().to_str().expect("utf-8"))
        .expect("resolve");
    // nextjs プリセットの代表 glob `**/app/**` または `**/pages/**` のどちらかが含まれる。
    assert!(
        globs
            .iter()
            .any(|g| g.contains("app/**") || g.contains("pages/**"))
    );
}

/// package.json も `--framework` も無いケースは空 Vec を返す (Ok(Vec::new()))。
#[test]
fn resolve_framework_globs_with_auto_detect_empty_when_neither() {
    let dir = tempfile::tempdir().expect("tempdir");
    let globs = resolve_framework_globs_with_auto_detect(None, dir.path().to_str().expect("utf-8"))
        .expect("resolve");
    assert!(globs.is_empty());
}

#[test]
fn reconcile_with_moves_pairs_by_signature() {
    // reconcile_with_moves のユニットテスト: 同じ (name,kind,sig) を相殺して
    // moved に分類し、残りだけを返す。
    let added = vec![
        ApiSymbolCandidate {
            name: "foo".into(),
            kind: "function".into(),
            file: "new.py".into(),
            signature: "def foo():".into(),
        },
        ApiSymbolCandidate {
            name: "new_api".into(),
            kind: "function".into(),
            file: "new.py".into(),
            signature: "def new_api():".into(),
        },
    ];
    let removed = vec![
        ApiSymbolCandidate {
            name: "foo".into(),
            kind: "function".into(),
            file: "old.py".into(),
            signature: "def foo():".into(),
        },
        ApiSymbolCandidate {
            name: "gone".into(),
            kind: "function".into(),
            file: "old.py".into(),
            signature: "def gone():".into(),
        },
    ];
    let all_new_candidates = added.clone();

    let (kept_added, kept_removed, moved) =
        reconcile_with_moves(added, removed, all_new_candidates);
    assert_eq!(kept_added.len(), 1);
    assert_eq!(kept_added[0].name, "new_api");
    assert_eq!(kept_removed.len(), 1);
    assert_eq!(kept_removed[0].name, "gone");
    assert_eq!(moved.len(), 1, "同シグネチャは moved に集約される");
    assert_eq!(moved[0].name, "foo");
    assert_eq!(moved[0].from, "old.py");
    assert_eq!(moved[0].to, "new.py");
}

#[test]
fn reconcile_with_moves_keeps_different_signatures() {
    // 同名でもシグネチャが違うなら相殺しない（signature change の検出漏れ防止）。
    let added = vec![ApiSymbolCandidate {
        name: "foo".into(),
        kind: "function".into(),
        file: "b.py".into(),
        signature: "def foo(x):".into(),
    }];
    let removed = vec![ApiSymbolCandidate {
        name: "foo".into(),
        kind: "function".into(),
        file: "a.py".into(),
        signature: "def foo():".into(),
    }];
    let all_new_candidates = added.clone();

    let (kept_added, kept_removed, moved) =
        reconcile_with_moves(added, removed, all_new_candidates);
    assert_eq!(kept_added.len(), 1);
    assert_eq!(kept_removed.len(), 1);
    assert!(
        moved.is_empty(),
        "シグネチャが違えば moved に乗らない。got: {moved:?}"
    );
}

#[test]
fn reconcile_with_moves_uses_filtered_new_candidates_for_pairing() {
    // is_used_in_diff_paths などで `added` から落ちた候補も all_new_candidates
    // に残っていれば removed と相殺する。module → package 化リファクタの中核。
    let added: Vec<ApiSymbolCandidate> = Vec::new();
    let removed = vec![ApiSymbolCandidate {
        name: "rotate_command".into(),
        kind: "function".into(),
        file: "src/cli.py".into(),
        signature: "def rotate_command(name: str):".into(),
    }];
    let all_new_candidates = vec![ApiSymbolCandidate {
        name: "rotate_command".into(),
        kind: "function".into(),
        file: "src/cli/_commands/rotate.py".into(),
        signature: "def rotate_command(name: str):".into(),
    }];

    let (kept_added, kept_removed, moved) =
        reconcile_with_moves(added, removed, all_new_candidates);
    assert!(
        kept_added.is_empty(),
        "added に乗らないので残らない: {kept_added:?}"
    );
    assert!(
        kept_removed.is_empty(),
        "all_new_candidates と組めば removed から消える: {kept_removed:?}"
    );
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].name, "rotate_command");
    assert_eq!(moved[0].from, "src/cli.py");
    assert_eq!(moved[0].to, "src/cli/_commands/rotate.py");
}

#[test]
fn detect_api_changes_skips_python_private_helpers() {
    // Python: `_` プレフィックスのヘルパーを public リファクタで追加しても
    // api.add として通知されないことを確認する（レポートの再現シナリオ）
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.name", "astro-sight-tests"],
        vec!["config", "user.email", "astro-sight@example.com"],
    ] {
        assert!(
            Command::new("git")
                .args(&args)
                .current_dir(repo)
                .status()
                .expect("git")
                .success()
        );
    }

    let script_path = repo.join("tool.py");
    fs::write(&script_path, "def check_layout():\n    return True\n").expect("write old file");

    assert!(
        Command::new("git")
            .args(["add", "."])
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

    // 拡張: private helper 2 個と public helper 1 個を追加
    fs::write(
        &script_path,
        r#"def _add_error(msg):
    return msg

def _check_plugin_manifest(path):
    return _add_error(path)

def check_layout():
    return _check_plugin_manifest("x")

def new_public_api():
    return 1
"#,
    )
    .expect("write new file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "tool.py".to_string(),
        new_path: "tool.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 11,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added_names: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();

    assert!(
        !added_names.contains(&"_add_error"),
        "Python の `_` プレフィックス関数は api.add から除外されるべき。got: {added_names:?}"
    );
    assert!(
        !added_names.contains(&"_check_plugin_manifest"),
        "Python の `_` プレフィックス関数は api.add から除外されるべき。got: {added_names:?}"
    );
    assert!(
        added_names.contains(&"new_public_api"),
        "`_` プレフィックスを持たない関数は引き続き api.add として検出されるべき。got: {added_names:?}"
    );
}

#[test]
fn detect_api_changes_rename_removed_uses_old_path() {
    // ファイルリネーム時にシンボルが削除された場合、removed の file は
    // 旧パス (old_path) を使用することを確認する。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/old.rs",
                "pub fn greet() -> i32 {\n    1\n}\n\npub fn farewell() -> i32 {\n    0\n}\n",
            ),
            (
                // caller を別ファイルに置いて farewell を参照させる (rename 削除でも
                // removed_dead ではなく removed として残ることを確認するため)
                "src/caller.rs",
                "pub fn use_farewell() -> i32 { crate::farewell() }\n",
            ),
        ],
        "initial",
    );

    // リネーム後のファイルから farewell を削除
    let new_path = repo.join("src/new.rs");
    if let Some(parent) = new_path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&new_path, "pub fn greet() -> i32 {\n    1\n}\n").expect("write renamed file");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/old.rs".to_string(),
        new_path: "src/new.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 7,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_farewell = api_changes.removed.iter().find(|s| s.name == "farewell");

    assert!(
        removed_farewell.is_some(),
        "farewell が removed に含まれるべき。got: {:?}",
        api_changes.removed
    );

    assert_eq!(
        removed_farewell.unwrap().file,
        "src/old.rs",
        "削除シンボルの file は旧パス (old_path) であるべき"
    );
}

#[test]
fn detect_api_changes_ignores_moved_trait_impl_methods() {
    // Rust の `impl Trait for Type` 配下の trait メソッドは実装事実であり、
    // 独立した公開 API item として扱うべきではない。`impl` ブロックをファイル間で
    // 移動しただけで `api.rm` / `api.add` に出るのは誤検出。
    // 本テストは mod.rs を複数サブモジュールに分割する際に `on_ref` / `default` が
    // api.rm へ漏れ出していた実例 (2026-04-21 トリアージ) の回帰防止。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // 初期: a.rs に struct Foo と impl Default for Foo
    git_commit_files(
        repo,
        &[(
            "src/a.rs",
            "pub struct Foo;\n\nimpl Default for Foo {\n    fn default() -> Self {\n        Self\n    }\n}\n",
        )],
        "initial",
    );

    // 変更: impl Default for Foo を b.rs に移動 (struct は a.rs に残す)
    fs::write(repo.join("src/a.rs"), "pub struct Foo;\n").expect("rewrite a.rs");
    fs::write(
            repo.join("src/b.rs"),
            "use super::a::Foo;\n\nimpl Default for Foo {\n    fn default() -> Self {\n        Self\n    }\n}\n",
        )
        .expect("write b.rs");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/a.rs".to_string(),
            new_path: "src/a.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 7,
                new_start: 1,
                new_count: 1,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/b.rs".to_string(),
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

    let removed_has_default = api_changes
        .removed
        .iter()
        .any(|s| s.name.ends_with("default"));
    let added_has_default = api_changes
        .added
        .iter()
        .any(|s| s.name.ends_with("default"));

    assert!(
        !removed_has_default,
        "impl Default for Foo の default メソッドは trait impl であり \
             api.rm に計上すべきでない。got removed: {:?}",
        api_changes.removed
    );
    assert!(
        !added_has_default,
        "impl Default for Foo の default メソッドは trait impl であり \
             api.add に計上すべきでない。got added: {:?}",
        api_changes.added
    );
}

#[test]
fn build_review_hook_json_returns_none_when_no_issues() {
    let dir = tempfile::tempdir().expect("tempdir");

    let build = build_review_hook_json(
        &empty_review_result(),
        dir.path().to_str().expect("utf-8 path"),
        false,
    );
    assert!(
        build.value.is_none(),
        "問題がない review 結果では hook JSON を生成しないべき"
    );
    assert!(!build.is_blocking, "出力なしなら blocking にしないべき");
}

/// cochange のみの場合は出力はするが exit 1 にはしない (informational)
#[test]
fn build_review_hook_json_cochange_only_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
        },
        missing_cochanges: vec![MissingCochange {
            file: "a.rs".to_string(),
            expected_with: "b.rs".to_string(),
            confidence: 0.9,
        }],
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(
        build.value.is_some(),
        "cochange は情報提供として JSON 出力はするべき"
    );
    assert!(
        !build.is_blocking,
        "cochange のみの場合は Stop hook を止めないべき"
    );
}

/// import-only などの informational impact は hook JSON に出すが、blocking にはしない。
#[test]
fn build_review_hook_json_impact_info_only_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(
        src_dir.join("lib.ts"),
        "export function compute() { return 1; }\n",
    )
    .expect("write changed file");
    fs::write(
        src_dir.join("consumer.ts"),
        "import { compute } from './lib';\n",
    )
    .expect("write caller file");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "src/lib.ts".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![crate::models::impact::AffectedSymbol {
                    name: "compute".to_string(),
                    kind: "function".to_string(),
                    change_type: "modified".to_string(),
                }],
                signature_changes: Vec::new(),
                impacted_callers: Vec::new(),
                low_confidence_callers: Vec::new(),
                informational_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "src/consumer.ts".to_string(),
                    name: "compute".to_string(),
                    line: 1,
                    symbols: vec!["compute".to_string()],
                    confidence: Some("informational".to_string()),
                }],
            }],
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("impact_info should be emitted");
    assert!(
        !build.is_blocking,
        "impact_info だけなら Stop hook を止めないべき"
    );
    assert!(
        hook_json.get("impacts").is_none(),
        "informational impact は blocking impacts には混ぜない"
    );
    assert_eq!(
        hook_json["impact_info"][0]["refs"][0]["s"],
        serde_json::json!(["compute"])
    );
}

/// api.add のみの場合は informational として出力されるが blocking にはしない
#[test]
fn build_review_hook_json_api_add_only_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: vec![ApiSymbol {
                name: "foo".to_string(),
                kind: "function".to_string(),
                file: "a.rs".to_string(),
            }],
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(build.value.is_some(), "api.add は hook JSON に出すべき");
    assert!(
        !build.is_blocking,
        "api.add のみ (additive) は Stop hook を止めないべき"
    );
}

/// api.removed は破壊的変更の可能性があるため blocking になる
#[test]
fn build_review_hook_json_api_removed_is_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: vec![ApiSymbol {
                name: "foo".to_string(),
                kind: "function".to_string(),
                file: "a.rs".to_string(),
            }],
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(build.value.is_some(), "api.rm は hook JSON に出すべき");
    assert!(build.is_blocking, "api.rm は blocking にすべき");
}

/// api.modified は破壊的変更の可能性があるため blocking になる
#[test]
fn build_review_hook_json_api_modified_is_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: vec![ApiSymbolChange {
                name: "foo".to_string(),
                kind: "function".to_string(),
                file: "a.rs".to_string(),
                old_signature: Some("fn foo()".to_string()),
                new_signature: Some("fn foo(x: u32)".to_string()),
            }],
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(build.value.is_some(), "api.mod は hook JSON に出すべき");
    assert!(build.is_blocking, "api.mod は blocking にすべき");
}

/// `rm_dead` (削除前参照ゼロの dead symbol 削除) は破壊的変更ではないため、
/// 単独では Stop hook を blocking しない (informational)。moon-star-link 報告
/// (2026-06-13) の「rm_dead が hook failure の原因」は誤診断で、実際の blocking は
/// 同時に出ていた api.mod (Kotlin body-only 誤検出) が原因だった。rm_dead が
/// 非 blocking である契約を回帰テストで固定する。
#[test]
fn build_review_hook_json_removed_dead_only_is_not_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: vec![
                ApiSymbol {
                    name: "MapZoomUtils".to_string(),
                    kind: "object".to_string(),
                    file: "MapZoomUtils.kt".to_string(),
                },
                ApiSymbol {
                    name: "MapZoomUtils.zoomForBounds".to_string(),
                    kind: "function".to_string(),
                    file: "MapZoomUtils.kt".to_string(),
                },
            ],
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(
        build.value.is_some(),
        "rm_dead は informational として hook JSON に出すべき"
    );
    assert!(
        !build.is_blocking,
        "rm_dead 単独は blocking にすべきでない (informational)"
    );
}

#[test]
fn build_review_hook_json_const_value_only_is_informational() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: vec![ApiSymbolChange {
                name: "ENEMY_SPEED".to_string(),
                kind: "constant".to_string(),
                file: "src/constants.rs".to_string(),
                old_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
                new_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
            }],
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };
    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    assert!(
        build.value.is_some(),
        "const_value 変更は informational として hook JSON に出すべき"
    );
    assert!(
        !build.is_blocking,
        "const_value のみの変更はデフォルトで blocking にしないべき"
    );
}

#[test]
fn build_review_hook_json_const_value_is_blocking_under_strict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: Vec::new(),
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: vec![ApiSymbolChange {
                name: "ENEMY_SPEED".to_string(),
                kind: "constant".to_string(),
                file: "src/constants.rs".to_string(),
                old_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
                new_signature: Some("pub const ENEMY_SPEED: f32".to_string()),
            }],
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };
    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), true);
    assert!(
        build.is_blocking,
        "--strict-public-const-values 指定時は const_value を blocking に昇格すべき"
    );
}

#[test]
fn build_review_hook_json_uses_changed_symbols_in_summary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(src_dir.join("lib.rs"), "pub fn compute() {}\n").expect("write changed file");
    fs::write(src_dir.join("main.rs"), "fn main() { compute(); }\n").expect("write caller");

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "src/lib.rs".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![crate::models::impact::AffectedSymbol {
                    name: "compute".to_string(),
                    kind: "function".to_string(),
                    change_type: "modified".to_string(),
                }],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "src/main.rs".to_string(),
                    name: "main".to_string(),
                    line: 1,
                    // caller.symbols は「この caller が参照している、変更ファイル内の
                    // シンボル名」(pass3.rs::build_file_impact の構築意図)。
                    // 呼び出し元関数の名前は ImpactedCaller.name 側に入る。
                    symbols: vec!["compute".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("hook json should be generated");
    assert!(build.is_blocking, "impacts があれば blocking にすべき");
    let impacts = hook_json["impacts"]
        .as_array()
        .expect("impacts should be an array");
    assert_eq!(impacts.len(), 1);
    assert_eq!(impacts[0]["src"], "src/lib.rs");
    assert_eq!(impacts[0]["syms"], serde_json::json!(["compute"]));
    assert_eq!(impacts[0]["refs"][0]["s"], serde_json::json!(["compute"]));
}

/// hook の `syms` には cross-file caller を発生させた causal symbol だけを残し、
/// 非 export const や本体未変更の export を除外する (Issue 2026-05-14
/// private-const-and-unchanged-export-noise)。
#[test]
fn build_review_hook_json_filters_non_causal_affected_symbols_from_syms() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(src_dir.join("a.rs"), "pub fn foo() {}\n").expect("write changed file");
    fs::write(src_dir.join("b.rs"), "fn caller() { foo(); }\n").expect("write caller");

    // affected_symbols は変更ファイル内で hunk と overlap した全シンボル。
    // PRIVATE_CONST と unchanged_export は cross-file 検索で is_symbol_exported に
    // 弾かれて caller.symbols には含まれないため、hook の syms にも出てはならない。
    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "src/a.rs".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![
                    crate::models::impact::AffectedSymbol {
                        name: "foo".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    },
                    crate::models::impact::AffectedSymbol {
                        name: "PRIVATE_CONST".to_string(),
                        kind: "constant".to_string(),
                        change_type: "modified".to_string(),
                    },
                    crate::models::impact::AffectedSymbol {
                        name: "unchanged_export".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    },
                ],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "src/b.rs".to_string(),
                    name: "caller".to_string(),
                    line: 1,
                    symbols: vec!["foo".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().expect("utf-8 path"), false);
    let hook_json = build.value.expect("hook json should be generated");
    assert!(build.is_blocking, "未解決 impact があれば blocking");
    let impacts = hook_json["impacts"]
        .as_array()
        .expect("impacts should be an array");
    assert_eq!(impacts.len(), 1);
    assert_eq!(
        impacts[0]["syms"],
        serde_json::json!(["foo"]),
        "syms は cross-file caller を発生させた causal symbol だけになるべき (PRIVATE_CONST と unchanged_export は除外)"
    );
    // refs[].s は元々 caller.symbols そのまま (causal の絞り込みは不要)
    assert_eq!(impacts[0]["refs"][0]["s"], serde_json::json!(["foo"]));
}

/// 新規追加 (`change_type=added`) シンボルへの caller のみがある場合、
/// hook blocking には含めない。同コミットで新規シンボルと新規参照が
/// セットで導入されるのは自然な依存関係で、breaking change ではない
/// (Issue 2026-05-27-added-symbol-initial-reference)。
#[test]
fn build_review_hook_json_added_only_caller_is_not_blocking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(src_dir.join("constants.rs"), "pub const FOO: u32 = 1;\n").unwrap();
    fs::write(
        src_dir.join("user.rs"),
        "use crate::constants::FOO; fn x() { let _ = FOO; }\n",
    )
    .unwrap();

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "src/constants.rs".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![crate::models::impact::AffectedSymbol {
                    name: "FOO".to_string(),
                    kind: "constant".to_string(),
                    change_type: "added".to_string(),
                }],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "src/user.rs".to_string(),
                    name: "x".to_string(),
                    line: 1,
                    symbols: vec!["FOO".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().unwrap(), false);
    // 新規追加シンボルへの caller のみ → impacts は空 (blocking 対象外)
    assert!(
        build.value.is_none() || {
            let v = build.value.as_ref().unwrap();
            v.get("impacts")
                .and_then(|i| i.as_array())
                .is_none_or(|a| a.is_empty())
        },
        "added シンボルのみへの caller は hook impacts から除外されるべき: {:?}",
        build.value
    );
    assert!(
        !build.is_blocking,
        "added のみの場合は Stop hook を止めないべき"
    );
}

/// 同 caller が added と modified の両方を参照している場合、modified だけを
/// causal symbol として残し blocking する。
#[test]
fn build_review_hook_json_mixed_added_and_modified_keeps_only_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(
        src_dir.join("a.rs"),
        "pub fn modified_fn() {}\npub const NEW_CONST: u32 = 1;\n",
    )
    .unwrap();
    fs::write(
            src_dir.join("b.rs"),
            "use crate::a::{modified_fn, NEW_CONST}; fn caller() { modified_fn(); let _ = NEW_CONST; }\n",
        )
        .unwrap();

    let result = ReviewResult {
        impact: crate::models::impact::ContextResult {
            changes: vec![crate::models::impact::FileImpact {
                path: "src/a.rs".to_string(),
                hunks: Vec::new(),
                affected_symbols: vec![
                    crate::models::impact::AffectedSymbol {
                        name: "modified_fn".to_string(),
                        kind: "function".to_string(),
                        change_type: "modified".to_string(),
                    },
                    crate::models::impact::AffectedSymbol {
                        name: "NEW_CONST".to_string(),
                        kind: "constant".to_string(),
                        change_type: "added".to_string(),
                    },
                ],
                signature_changes: Vec::new(),
                impacted_callers: vec![crate::models::impact::ImpactedCaller {
                    path: "src/b.rs".to_string(),
                    name: "caller".to_string(),
                    line: 1,
                    symbols: vec!["modified_fn".to_string(), "NEW_CONST".to_string()],
                    confidence: None,
                }],
                low_confidence_callers: Vec::new(),
                informational_callers: Vec::new(),
            }],
            skipped: None,
        },
        missing_cochanges: Vec::new(),
        api_changes: ApiChanges {
            added: Vec::new(),
            removed: Vec::new(),
            modified: Vec::new(),
            moved: Vec::new(),
            property_to_field: Vec::new(),
            removed_dead: Vec::new(),
            modified_closed_in_diff: Vec::new(),
            const_value_changes: Vec::new(),
            compatible_modified: Vec::new(),
        },
        dead_symbols: Vec::new(),
        test_only_symbols: Vec::new(),
        skipped: None,
    };

    let build = build_review_hook_json(&result, dir.path().to_str().unwrap(), false);
    let hook_json = build.value.expect("hook json should be generated");
    assert!(build.is_blocking, "modified を含むため blocking");
    let impacts = hook_json["impacts"].as_array().expect("impacts array");
    assert_eq!(impacts.len(), 1);
    // syms / refs[].s には modified_fn のみが残り、NEW_CONST (added) は落ちる
    assert_eq!(
        impacts[0]["syms"],
        serde_json::json!(["modified_fn"]),
        "added 由来の NEW_CONST は syms から除外され modified_fn のみ残るべき"
    );
    assert_eq!(
        impacts[0]["refs"][0]["s"],
        serde_json::json!(["modified_fn"]),
        "refs[].s も modified_fn のみに絞られるべき"
    );
}

// ------------------------------------------------------------------
// is_dependency_manifest_pair
// ------------------------------------------------------------------

#[test]
fn is_dependency_manifest_pair_matches_cargo() {
    assert!(is_dependency_manifest_pair("Cargo.toml", "Cargo.lock"));
    assert!(is_dependency_manifest_pair("Cargo.lock", "Cargo.toml"));
}

#[test]
fn is_dependency_manifest_pair_matches_node_lockfiles() {
    for lock in ["package-lock.json", "pnpm-lock.yaml", "yarn.lock"] {
        assert!(
            is_dependency_manifest_pair("package.json", lock),
            "package.json ↔ {lock} should match"
        );
    }
}

#[test]
fn is_dependency_manifest_pair_matches_other_ecosystems() {
    let pairs = [
        ("pyproject.toml", "uv.lock"),
        ("pyproject.toml", "poetry.lock"),
        ("pyproject.toml", "pdm.lock"),
        ("Gemfile", "Gemfile.lock"),
        ("composer.json", "composer.lock"),
        ("go.mod", "go.sum"),
        ("mix.exs", "mix.lock"),
    ];
    for (a, b) in pairs {
        assert!(is_dependency_manifest_pair(a, b), "{a} ↔ {b} should match");
    }
}

#[test]
fn is_dependency_manifest_pair_rejects_unrelated_files() {
    assert!(!is_dependency_manifest_pair("src/lib.rs", "Cargo.toml"));
    assert!(!is_dependency_manifest_pair("Cargo.toml", "README.md"));
    assert!(!is_dependency_manifest_pair(
        "package.json",
        "tsconfig.json"
    ));
}

#[test]
fn is_dependency_manifest_pair_rejects_cross_directory_pairs() {
    // monorepo: 異なるディレクトリのマニフェスト/ロックは別プロジェクトなので除外対象外
    assert!(!is_dependency_manifest_pair(
        "apps/web/package.json",
        "apps/api/package-lock.json"
    ));
    assert!(!is_dependency_manifest_pair(
        "crates/foo/Cargo.toml",
        "crates/bar/Cargo.lock"
    ));
}

#[test]
fn is_dependency_manifest_pair_accepts_same_directory_pairs() {
    assert!(is_dependency_manifest_pair(
        "apps/web/package.json",
        "apps/web/package-lock.json"
    ));
    assert!(is_dependency_manifest_pair(
        "crates/foo/Cargo.toml",
        "crates/foo/Cargo.lock"
    ));
}

// ------------------------------------------------------------------
// detect_missing_cochanges: 依存マニフェスト/ロックペアを除外する
// ------------------------------------------------------------------

/// Cargo.toml ↔ Cargo.lock が過去繰り返し共変更されていても
/// Cargo.lock のみの変更で missing_cochange 警告を出さない。
#[test]
fn detect_missing_cochanges_excludes_cargo_manifest_lock_pair() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // Cargo.toml と Cargo.lock を何度も共変更（cochange 統計を作る）
    for i in 0..4 {
        git_commit_files(
            repo,
            &[
                ("Cargo.toml", &format!("# v{i}\n")),
                ("Cargo.lock", &format!("# lock v{i}\n")),
            ],
            &format!("dep update {i}"),
        );
    }

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    // Cargo.lock のみが変更された状況（cargo update -p 相当）
    changed_files.insert("Cargo.lock".to_string());

    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.3,
        None,
    )
    .expect("detect_missing_cochanges should succeed");

    assert!(
        missing.iter().all(|m| m.file != "Cargo.toml"),
        "Cargo.toml が missing_cochange に含まれてはならない。got: {missing:?}"
    );
}

#[test]
fn detect_missing_cochanges_uses_review_base_for_multi_commit_ranges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(
        repo,
        &[
            (
                "a.rs",
                "fn a() {\n    let first = 0;\n    let second = 0;\n}\n",
            ),
            (
                "b.rs",
                "fn b() {\n    let first = 0;\n    let second = 0;\n}\n",
            ),
        ],
        "initial",
    );
    git_commit_files(
        repo,
        &[
            (
                "a.rs",
                "fn a() {\n    let first = 1;\n    let second = 0;\n}\n",
            ),
            (
                "b.rs",
                "fn b() {\n    let first = 1;\n    let second = 0;\n}\n",
            ),
        ],
        "pair 1",
    );
    git_commit_files(
        repo,
        &[
            (
                "a.rs",
                "fn a() {\n    let first = 1;\n    let second = 2;\n}\n",
            ),
            (
                "b.rs",
                "fn b() {\n    let first = 1;\n    let second = 2;\n}\n",
            ),
        ],
        "pair 2",
    );
    git_commit_files(
        repo,
        &[(
            "a.rs",
            "fn a() {\n    let first = 10;\n    let second = 2;\n}\n",
        )],
        "a only 1",
    );
    git_commit_files(
        repo,
        &[(
            "a.rs",
            "fn a() {\n    let first = 10;\n    let second = 20;\n}\n",
        )],
        "a only 2",
    );

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("a.rs".to_string());

    // 小サンプル (co=2, denom=2) なので新デフォルト β=8 では
    // score=(2+1)/(2+1+8)=0.27 となり、production の min_confidence=0.3
    // からは弾かれる。本テストは「base が blame 解析に正しく渡る」を
    // 確かめるのが目的なので、閾値を 0.0 に下げて信号の有無だけ見る。
    let missing = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        0.0,
        Some("HEAD~2"),
    )
    .expect("detect_missing_cochanges should succeed");

    assert!(
        missing.iter().any(|m| m.file == "b.rs"),
        "review の base が blame 解析に渡らず HEAD~1 のみを見ると b.rs を見落とす。got: {missing:?}"
    );
}

/// review の detect_missing_cochanges が cochange 入力検証エラーを silent に握り潰さず
/// 呼び出し側へ伝播することを確認する回帰テスト。
#[test]
fn detect_missing_cochanges_propagates_invalid_request_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("a.rs", "v1")], "initial");

    let service = AppService::new();
    let mut changed_files = HashSet::new();
    changed_files.insert("a.rs".to_string());

    // NaN は AppService::analyze_cochange の入力検証で InvalidRequest を返すため、
    // detect_missing_cochanges もそのエラーを伝播するはず。
    let result = detect_missing_cochanges(
        &service,
        repo.to_str().expect("utf-8 path"),
        &changed_files,
        f64::NAN,
        None,
    );

    let err = result.expect_err("NaN min_confidence should surface as error");
    let astro_err = err
        .downcast_ref::<crate::error::AstroError>()
        .expect("expect AstroError");
    assert_eq!(astro_err.code, crate::error::ErrorCode::InvalidRequest);
}

#[test]
fn resolve_blame_source_files_filters_default_excludes_for_git_only() {
    // --git 経由で diff から起点ファイルを取得する場合、
    // BLAME_DEFAULT_EXCLUDE_GLOBS に該当する生成物 (dist/, *.lock) は除外される。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(repo, &[("foo.txt", "v1")], "initial");
    git_commit_files(
        repo,
        &[
            ("foo.txt", "v2"),
            ("dist/main.js", "minified"),
            ("Cargo.lock", "lockfile"),
            ("Angular/www/dist/bundle.js", "minified"),
        ],
        "next",
    );

    let BlameSourceResolution::Files(result) = resolve_blame_source_files(
        repo.to_str().expect("utf-8 path"),
        true,
        Some("HEAD~1"),
        None,
        None,
        &[],
    )
    .expect("resolve") else {
        panic!("expected Files");
    };

    assert!(result.contains(&"foo.txt".to_string()), "got: {result:?}");
    assert!(
        !result.iter().any(|p| p == "dist/main.js"),
        "dist/main.js は BLAME_DEFAULT_EXCLUDE_GLOBS で除外されるはず。got: {result:?}"
    );
    assert!(
        !result.iter().any(|p| p == "Cargo.lock"),
        "Cargo.lock は除外されるはず。got: {result:?}"
    );
    assert!(
        !result.iter().any(|p| p == "Angular/www/dist/bundle.js"),
        "サブディレクトリの dist/ も除外されるはず。got: {result:?}"
    );
}

#[test]
fn resolve_blame_source_files_keeps_explicit_paths_unfiltered() {
    // --paths で明示指定した起点はユーザー意図を尊重し、
    // BLAME_DEFAULT_EXCLUDE_GLOBS 該当でも除外しない。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("dummy.txt", "x")], "initial");

    let BlameSourceResolution::Files(result) = resolve_blame_source_files(
        repo.to_str().expect("utf-8 path"),
        false,
        None,
        Some("dist/main.js,Cargo.lock"),
        None,
        &[],
    )
    .expect("resolve") else {
        panic!("expected Files");
    };

    assert!(result.contains(&"dist/main.js".to_string()));
    assert!(result.contains(&"Cargo.lock".to_string()));
}

#[test]
fn resolve_blame_source_files_applies_user_exclude_glob_for_git() {
    // --git 経由のとき --exclude-glob (user_exclude_globs) も適用される。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(repo, &[("foo.txt", "v1")], "initial");
    git_commit_files(
        repo,
        &[
            ("foo.txt", "v2"),
            ("legacy/keep.rs", "old"),
            ("generated/codegen.rs", "auto"),
        ],
        "next",
    );

    let BlameSourceResolution::Files(result) = resolve_blame_source_files(
        repo.to_str().expect("utf-8 path"),
        true,
        Some("HEAD~1"),
        None,
        None,
        &["generated/**".to_string()],
    )
    .expect("resolve") else {
        panic!("expected Files");
    };

    assert!(result.contains(&"foo.txt".to_string()));
    assert!(result.contains(&"legacy/keep.rs".to_string()));
    assert!(
        !result.iter().any(|p| p == "generated/codegen.rs"),
        "ユーザー指定 --exclude-glob は --git 経由の起点に適用される。got: {result:?}"
    );
}

/// dead-code --glob が positive whitelist として絞り込みに使われていることを確認する。
/// 以前は Match::None も許可されており、`**/*.py` 指定でも Rust ファイル等が残っていた。
#[test]
fn filter_diff_files_for_dead_code_glob_acts_as_whitelist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    // glob による絞り込みの単体検証なので、実ファイルは作らず diff 模擬のみ。
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/foo.rs".to_string(),
            hunks: Vec::new(),
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/bar.py".to_string(),
            hunks: Vec::new(),
            deleted_old_source: None,
        },
    ];

    let files = filter_diff_files_for_dead_code(repo, &diff_files, &[], &[], Some("**/*.py"))
        .expect("filter");

    // glob 絞り込み後は Python ファイルだけが残るべき。
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n == "bar.py"),
        "py ファイルは glob に一致するため残る。got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "foo.rs"),
        "rs ファイルは glob にマッチしないため除外される。got: {names:?}"
    );
}

/// detect_api_changes は diff path のトラバーサルを安全に無視する。
/// `../etc/passwd` のような diff を渡しても workspace 外を読まない。
#[test]
fn detect_api_changes_skips_unsafe_diff_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let dir_str = repo.to_str().expect("utf-8 path");

    let unsafe_diff = vec![crate::models::impact::DiffFile {
        old_path: "/dev/null".to_string(),
        new_path: "../etc/passwd".to_string(),
        hunks: Vec::new(),
        deleted_old_source: None,
    }];

    // パス検証で弾かれ、added/removed/modified ともに空配列を返すこと。
    let result = detect_api_changes(dir_str, "HEAD", &unsafe_diff);
    assert!(result.added.is_empty());
    assert!(result.removed.is_empty());
    assert!(result.modified.is_empty());
}

/// `filter_dead_by_wip_added` は同一 diff で新規 export されたシンボルを
/// dead から除外する。多段実装中の WIP ノイズ抑止のための既定挙動 (Issue
/// 2026-06-25-wip-dead-symbol-during-incremental-impl 対応)。
#[test]
fn filter_dead_by_wip_added_drops_symbols_listed_in_added() {
    use crate::models::review::{ApiSymbol, DeadSymbol};
    let dead = vec![
        DeadSymbol {
            name: "matchAssigneeName".to_string(),
            kind: "function".to_string(),
            file: "src/notes.ts".to_string(),
        },
        DeadSymbol {
            name: "legacyUnused".to_string(),
            kind: "function".to_string(),
            file: "src/legacy.ts".to_string(),
        },
    ];
    let added = vec![ApiSymbol {
        name: "matchAssigneeName".to_string(),
        kind: "function".to_string(),
        file: "src/notes.ts".to_string(),
    }];
    let filtered = filter_dead_by_wip_added(dead, &added);
    let names: Vec<&str> = filtered.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["legacyUnused"],
        "WIP added は dead から除外、既存 dead は残す"
    );
}

/// `filter_dead_by_wip_added` は (file, name) ペアで突合せ、同名でもファイルが
/// 異なれば dead として残す (誤抑制防止)。
#[test]
fn filter_dead_by_wip_added_matches_on_file_and_name_pair() {
    use crate::models::review::{ApiSymbol, DeadSymbol};
    let dead = vec![DeadSymbol {
        name: "helper".to_string(),
        kind: "function".to_string(),
        file: "src/a.ts".to_string(),
    }];
    let added = vec![ApiSymbol {
        // 同じ name だが別 file の追加 — dead 側 (a.ts) は残るべき。
        name: "helper".to_string(),
        kind: "function".to_string(),
        file: "src/b.ts".to_string(),
    }];
    let filtered = filter_dead_by_wip_added(dead, &added);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].file, "src/a.ts");
}

/// `filter_dead_by_wip_added` は added が空なら dead を素通しする。
#[test]
fn filter_dead_by_wip_added_passes_through_when_added_is_empty() {
    use crate::models::review::DeadSymbol;
    let dead = vec![DeadSymbol {
        name: "foo".to_string(),
        kind: "function".to_string(),
        file: "src/foo.rs".to_string(),
    }];
    let filtered = filter_dead_by_wip_added(dead.clone(), &[]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "foo");
}

/// TS/JS では `pub` という名前の関数を Rust の `pub(...)` 可視性と誤認して API 面から
/// 落とさない (`pub(` の宣言行チェックは Rust 限定)。
#[test]
fn filter_exported_symbols_ts_function_named_pub_is_not_excluded() {
    let source: &[u8] =
        b"export function pub(topic: string): void {}\nexport function sub(topic: string): void {}\n";
    let lang = crate::language::LangId::Typescript;
    let tree = parser::parse_source(source, lang).expect("parse");
    let root = tree.root_node();
    let syms = crate::engine::symbols::extract_symbols(root, source, lang).expect("symbols");
    let exported = filter_exported_symbols(&syms, root, source, lang, true, false, Some("api.ts"));
    let names: Vec<&str> = exported.iter().map(|(name, _, _)| name.as_str()).collect();
    assert!(
        names.contains(&"pub"),
        "TS の関数 pub は API 面に残るべき。got: {names:?}"
    );
    assert!(
        names.contains(&"sub"),
        "sub も従来どおり API 面に残るべき。got: {names:?}"
    );
}

/// 対照: Rust の `pub(crate)` はクレート内部 API のため従来どおり除外される。
#[test]
fn filter_exported_symbols_rust_pub_crate_is_still_excluded() {
    let source: &[u8] = b"pub(crate) fn internal() {}\npub fn public_api() {}\n";
    let lang = crate::language::LangId::Rust;
    let tree = parser::parse_source(source, lang).expect("parse");
    let root = tree.root_node();
    let syms = crate::engine::symbols::extract_symbols(root, source, lang).expect("symbols");
    let exported =
        filter_exported_symbols(&syms, root, source, lang, true, false, Some("src/lib.rs"));
    let names: Vec<&str> = exported.iter().map(|(name, _, _)| name.as_str()).collect();
    assert!(
        !names.contains(&"internal"),
        "pub(crate) fn は API 面から除外されるべき。got: {names:?}"
    );
    assert!(
        names.contains(&"public_api"),
        "pub fn は従来どおり API 面に残るべき。got: {names:?}"
    );
}

/// framework の実行時入口は、API 差分と dead-code で除外条件が異なる。
/// Flyway は両経路で除外する一方、Laravel relation と Angular lifecycle hook は
/// API 面に残す。この非対称性を一律のフラグ判定へまとめると、過去の誤検出が再発する。
#[test]
fn filter_exported_symbols_framework_flag_matrix_is_pinned() {
    struct Case {
        lang: crate::language::LangId,
        source: &'static [u8],
        path: &'static str,
        symbol: &'static str,
        api_surface: bool,
        dead_code_surface: bool,
    }

    let cases = [
        Case {
            lang: crate::language::LangId::Php,
            source: b"<?php\nclass FooTest { public function testBar(): void {} }\n",
            path: "tests/FooTest.php",
            symbol: "FooTest.testBar",
            api_surface: false,
            dead_code_surface: false,
        },
        Case {
            lang: crate::language::LangId::Java,
            source: b"public class V1__Init extends BaseJavaMigration { public void migrate(Context context) {} }\n",
            path: "db/migration/V1__Init.java",
            symbol: "V1__Init",
            api_surface: false,
            dead_code_surface: false,
        },
        Case {
            lang: crate::language::LangId::Php,
            source: b"<?php\nclass Model { public function posts(): HasOne { return $this->hasOne(Post::class); } }\n",
            path: "app/Models/Model.php",
            symbol: "Model.posts",
            api_surface: true,
            dead_code_surface: false,
        },
        Case {
            lang: crate::language::LangId::Typescript,
            source: b"export class Item { constructor(value: number) {} }\n",
            path: "src/item.ts",
            symbol: "Item.constructor",
            api_surface: false,
            dead_code_surface: false,
        },
        Case {
            lang: crate::language::LangId::Typescript,
            source: b"@Component({})\nexport class Widget { ngOnInit(): void {} }\n",
            path: "src/widget.component.ts",
            symbol: "Widget.ngOnInit",
            api_surface: true,
            dead_code_surface: false,
        },
    ];

    for case in cases {
        let tree = parser::parse_source(case.source, case.lang).expect("parse");
        let root = tree.root_node();
        let syms =
            crate::engine::symbols::extract_symbols(root, case.source, case.lang).expect("symbols");

        for (exclude_framework_entrypoints, expected) in
            [(false, case.api_surface), (true, case.dead_code_surface)]
        {
            let exported = filter_exported_symbols(
                &syms,
                root,
                case.source,
                case.lang,
                true,
                exclude_framework_entrypoints,
                Some(case.path),
            );
            let names: Vec<&str> = exported.iter().map(|(name, _, _)| name.as_str()).collect();
            assert_eq!(
                names.contains(&case.symbol),
                expected,
                "lang={:?} path={} exclude_framework_entrypoints={} symbol={} names={:?}",
                case.lang,
                case.path,
                exclude_framework_entrypoints,
                case.symbol,
                names
            );
        }
    }
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

/// `has_cross_file_refs` は qualname (`Store.get`) を bare 名 (`get`) に正規化して
/// index を引き、cross-file 参照を検出する (qualname のままだと恒久的に 0 件になる)。
#[test]
fn has_cross_file_refs_qualname_uses_bare_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    fs::write(
        repo.join("store.ts"),
        "export class Store {\n  get(k: string): string {\n    return k;\n  }\n}\nexport const local = new Store().get(\"self\");\n",
    )
    .expect("write");
    fs::write(
        repo.join("caller.ts"),
        "import { Store } from \"./store\";\nnew Store().get(\"x\");\n",
    )
    .expect("write");

    let ref_index = ApiRefIndex::build(
        repo.to_str().expect("utf-8 path"),
        &HashSet::from(["get".to_string()]),
    );
    // index には bare 名の参照が収集済み (未収集フォールバックの保守的 true と区別する)。
    assert!(
        ref_index.refs_for("get").is_some(),
        "bare 名 get の参照が index に収集されているべき"
    );
    assert!(
        has_cross_file_refs(&ref_index, "store.ts", "Store.get"),
        "qualname は bare 名照合で caller.ts の cross-file 参照を検出するべき"
    );
}

/// `detect_python_property_to_field` は old_path が Python の場合のみ判定する
/// (他言語の `Container.member` 削除が diff 内 .py の偶然の同名 class+field で
/// informational に降格しない)。
#[test]
fn detect_python_property_to_field_requires_python_old_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("new.py"),
        "from dataclasses import dataclass\n@dataclass\nclass Container:\n    member: int\n",
    )
    .expect("write");
    let dir_str = dir.path().to_str().expect("utf-8 path");
    let diff_new_paths: HashSet<String> = HashSet::from(["new.py".to_string()]);

    assert_eq!(
        detect_python_property_to_field(dir_str, "old.py", "Container.member", &diff_new_paths),
        Some("new.py".to_string()),
        "Python の old_path なら置き換え先 new.py を検出する"
    );
    assert_eq!(
        detect_python_property_to_field(dir_str, "old.ts", "Container.member", &diff_new_paths),
        None,
        "Python 以外の old_path は言語ガードで対象外"
    );
}

/// object destructuring (`const { beta } = config`) も member access 参照として検出する
/// (shorthand / rename / string キー / パラメータ destructuring)。見落とすと破壊的な
/// member 削除が unused_object_members に降格する。
#[test]
fn member_access_ref_detects_object_destructuring() {
    let lang = crate::language::LangId::Typescript;
    // shorthand (`{ beta }`) は shorthand_property_identifier_pattern
    assert!(
        source_has_member_access_ref(b"const { beta } = config;", lang, "beta").expect("parse"),
        "shorthand destructuring は member 参照として検出されるべき"
    );
    // rename (`{ beta: renamed }`) は pair_pattern の key
    assert!(
        source_has_member_access_ref(b"const { beta: renamed } = config;", lang, "beta")
            .expect("parse"),
        "rename destructuring は member 参照として検出されるべき"
    );
    // string キー (`{ \"beta\": renamed }`) も pair_pattern の key (string)
    assert!(
        source_has_member_access_ref(b"const { \"beta\": renamed } = config;", lang, "beta")
            .expect("parse"),
        "string キーの destructuring は member 参照として検出されるべき"
    );
    // 別キーのみの destructuring は検出しない
    assert!(
        !source_has_member_access_ref(b"const { alpha } = config;", lang, "beta").expect("parse"),
        "別キーの destructuring は member 参照ではない"
    );
    // memmem 事前フィルタを通過しても AST 判定で弾かれる (beta が member 位置に無い)
    assert!(
        !source_has_member_access_ref(b"const beta = config.other;", lang, "beta").expect("parse"),
        "member 位置に無い識別子 beta は member 参照ではない"
    );
    // パラメータ destructuring (`function f({ beta }: Opts)`) も同ノードで検出する
    assert!(
        source_has_member_access_ref(b"function f({ beta }: Opts) {}", lang, "beta")
            .expect("parse"),
        "パラメータ destructuring も member 参照として検出されるべき"
    );
}

/// `cochange` の `--ignore-merges` / `--include-merges` の CLI パーサ挙動と、
/// dispatch 側の `resolved_ignore_merges = !include_merges` への等価簡約を固定する。
/// (main.rs `dispatch_cochange` はバイナリクレート側でテストから直接呼べないため、
/// パーサ結果と旧 3 分岐ロジックの一致で等価性を担保する)
mod cochange_ignore_merges {
    use crate::cli::{Cli, Commands};
    use crate::models::cochange::CoChangeOptions;
    use clap::Parser;

    /// パース結果から `(ignore_merges, include_merges)` を取り出す。
    fn parse_flags(args: &[&str]) -> (bool, bool) {
        let cli = Cli::try_parse_from(args).expect("cochange args should parse");
        match cli.command {
            Commands::Cochange {
                ignore_merges,
                include_merges,
                ..
            } => (ignore_merges, include_merges),
            other => panic!("expected Cochange, got {other:?}"),
        }
    }

    /// 旧 3 分岐ロジック (等価性判定の基準)。
    fn legacy_resolved(ignore_merges: bool, include_merges: bool) -> bool {
        let defaults = CoChangeOptions::default();
        if include_merges {
            false
        } else if ignore_merges {
            true
        } else {
            defaults.ignore_merges
        }
    }

    #[test]
    fn default_resolves_ignore_merges_true() {
        let (ignore_merges, include_merges) = parse_flags(&["astro-sight", "cochange"]);
        assert!(!ignore_merges);
        assert!(!include_merges);
        let resolved = !include_merges;
        assert_eq!(resolved, legacy_resolved(ignore_merges, include_merges));
        assert!(resolved, "既定は merge 除外 (ignore_merges=true)");
    }

    #[test]
    fn ignore_merges_flag_resolves_true() {
        let (ignore_merges, include_merges) =
            parse_flags(&["astro-sight", "cochange", "--ignore-merges"]);
        assert!(ignore_merges);
        assert!(!include_merges);
        let resolved = !include_merges;
        assert_eq!(resolved, legacy_resolved(ignore_merges, include_merges));
        assert!(resolved);
    }

    #[test]
    fn include_merges_flag_resolves_false() {
        let (ignore_merges, include_merges) =
            parse_flags(&["astro-sight", "cochange", "--include-merges"]);
        assert!(!ignore_merges);
        assert!(include_merges);
        let resolved = !include_merges;
        assert_eq!(resolved, legacy_resolved(ignore_merges, include_merges));
        assert!(
            !resolved,
            "include-merges 指定で merge を含める (ignore_merges=false)"
        );
    }

    #[test]
    fn both_flags_conflict_is_parse_error() {
        let result = Cli::try_parse_from([
            "astro-sight",
            "cochange",
            "--ignore-merges",
            "--include-merges",
        ]);
        assert!(
            result.is_err(),
            "conflicts_with により両フラグ同時指定は parse エラー"
        );
    }
}

/// `ChangedFileSet` の caller 照合を、相対/絶対パス・canonicalize 失敗の各ケースで固定する。
/// canonicalize 成功時は canonical 集合、失敗時は文字列 fallback 集合だけを見る分岐を検証する。
mod changed_file_set {
    use crate::commands::ChangedFileSet;
    use std::fs;

    #[test]
    fn relative_path_matches_via_canonical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().expect("utf-8 path");
        fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write a");
        fs::write(dir.path().join("b.rs"), "fn b() {}\n").expect("write b");

        // 相対パスで構築 (dir 基準で絶対化 + canonicalize されて canonical 集合に入る)。
        let set = ChangedFileSet::build(dir_str, ["a.rs"]);
        assert!(
            set.contains_caller(dir_str, "a.rs"),
            "同一相対 caller は一致"
        );
        assert!(
            !set.contains_caller(dir_str, "b.rs"),
            "集合に無い既存ファイルは不一致"
        );
    }

    #[test]
    fn absolute_path_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().expect("utf-8 path");
        let abs = dir.path().join("a.rs");
        fs::write(&abs, "fn a() {}\n").expect("write a");
        let abs_str = abs.to_str().expect("utf-8 path");

        // 絶対パスで構築。絶対 caller も、同一ファイルに解決される相対 caller も
        // canonical 集合経由で一致する。
        let set = ChangedFileSet::build(dir_str, [abs_str]);
        assert!(set.contains_caller(dir_str, abs_str), "絶対 caller は一致");
        assert!(
            set.contains_caller(dir_str, "a.rs"),
            "同一ファイルに解決される相対 caller も canonical 経由で一致"
        );
    }

    #[test]
    fn nonexistent_path_falls_back_to_string_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().expect("utf-8 path");

        // canonicalize 失敗 (存在しないファイル) は文字列 fallback 集合で照合する。
        let set = ChangedFileSet::build(dir_str, ["ghost.rs"]);
        assert!(
            set.contains_caller(dir_str, "ghost.rs"),
            "存在しないファイルは文字列 fallback で一致"
        );
        assert!(
            !set.contains_caller(dir_str, "other_ghost.rs"),
            "別の存在しないファイルは不一致"
        );
    }
}
