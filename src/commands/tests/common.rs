//! サブモジュール横断で使うテストヘルパーとフィクスチャ。

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

/// テストヘルパー: 一時 git リポジトリを初期化する。
pub(crate) fn init_git_repo_for_test(repo: &std::path::Path) {
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
pub(crate) fn git_commit_files(repo: &std::path::Path, files: &[(&str, &str)], msg: &str) {
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

/// テストヘルパー: `resolve_git_diff` から diff と打ち切り情報を取り出す。
pub(crate) fn resolve_git_diff_parts(
    repo: &std::path::Path,
) -> (String, Vec<crate::models::truncation::TruncationInfo>) {
    match resolve_git_diff(repo.to_str().expect("utf-8"), "HEAD", false).expect("resolve") {
        GitDiffInput::Diff { diff, truncations } => (diff, truncations),
        GitDiffInput::Skipped(_) => panic!("git リポでは Diff を返すべき"),
    }
}

/// テストヘルパー: リポジトリルート直下と `app/` サブディレクトリの 2 プロジェクト構成を作る。
/// `app/` を `--dir` に渡すサブディレクトリ実行のパス基準テスト用。
pub(crate) fn init_subproject_repo_for_test(repo: &std::path::Path) {
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            // frontend 相当 (リポジトリルート直下の src/)
            ("src/root_side.rs", "pub fn root_side() {}\n"),
            // backend 相当 (app/src/)
            ("app/src/lib.rs", "pub mod kept;\n"),
            ("app/src/kept.rs", "pub fn kept() -> i32 {\n    1\n}\n"),
        ],
        "initial",
    );
}

/// テストヘルパー: 「object 型引数へ必須プロパティ追加 + 呼び出し側は共有 const を渡すだけ」
/// の diff を組み立てる (Issue 2026-08-05-api-mod-callers-updated-indirectly パターン C)。
///
/// `caller_after` で呼び出し側ファイルの変更後内容を差し替えることで、共有 const の更新有無 /
/// binding 形態のバリエーションを 1 つのシナリオ骨格で表現する。
pub(crate) fn ts_shared_const_arg_api_changes(
    repo: &std::path::Path,
    caller_before: &str,
    caller_after: &str,
) -> ApiChanges {
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/build.ts",
                "export function buildSql(deps: {\n\tevents: string;\n\tusers: string;\n}): string {\n\treturn `${deps.events}:${deps.users}`;\n}\n",
            ),
            ("src/build.test.ts", caller_before),
        ],
        "base",
    );
    // 必須プロパティ `groups` を追加。呼び出し式 `buildSql(SHARED_DEPS)` は無変更で、
    // 追随は SHARED_DEPS の定義側で行われる。
    fs::write(
        repo.join("src/build.ts"),
        "export function buildSql(deps: {\n\tevents: string;\n\tusers: string;\n\tgroups: string;\n}): string {\n\treturn `${deps.events}:${deps.users}:${deps.groups}`;\n}\n",
    )
    .expect("write build.ts");
    fs::write(repo.join("src/build.test.ts"), caller_after).expect("write caller");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/build.ts".to_string(),
            new_path: "src/build.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 6,
                new_start: 1,
                new_count: 7,
            }],
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "src/build.test.ts".to_string(),
            new_path: "src/build.test.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 8,
                new_start: 1,
                new_count: 9,
            }],
            deleted_old_source: None,
        },
    ];
    detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files)
}

pub(crate) const TS_SHARED_CONST_CALLER_BEFORE: &str = "import { buildSql } from \"./build\";\n\nconst SHARED_DEPS = {\n\tevents: \"e\",\n\tusers: \"u\",\n};\n\nexport function run(): string {\n\treturn buildSql(SHARED_DEPS);\n}\n";

/// Python モジュールをアトミックに削除するシナリオを組み立て、削除された `search` が
/// blocking な removed に残ったかどうかを返す。`surviving_python` は残存する .py の中身。
pub(crate) fn removed_names_after_atomic_python_module_deletion(
    surviving_python: &str,
) -> (Vec<String>, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "def search(query):\n    return [query]\n";
    git_commit_files(
        repo,
        &[
            ("scripts/core.py", deleted_src),
            // 削除した Python 関数とは無関係な、他言語の同名シンボル群
            (
                "web/app.php",
                "<?php\nfunction search($q) {\n    return $q;\n}\nfunction run() {\n    return search(\"x\");\n}\n",
            ),
            (
                "web/util.js",
                "export function search(q) {\n  return q;\n}\nexport function go() {\n  return search(\"y\");\n}\n",
            ),
            (
                "native/lib.c",
                "int search(int q) { return q; }\nint run(void) { return search(1); }\n",
            ),
            ("tools/check.py", surviving_python),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("scripts/core.py")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "scripts/core.py".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    (
        api.removed.iter().map(|s| s.name.clone()).collect(),
        api.removed_dead.iter().map(|s| s.name.clone()).collect(),
    )
}
