//! API 差分検出の共通経路 (除外規約 / シグネチャ照合 / フレームワーク検出) のテスト。

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
