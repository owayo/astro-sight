//! 改名・移動の相殺 (api.moved / reconcile_with_moves) のテスト。

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

/// `kept_removed` は入力順を保つ。以前は removed を HashMap にバケット化して
/// `into_values()` で回収していたため、RandomState の反復順がそのまま
/// `api.rm` / `removed_dead` の並びになり、実行ごとに順序が変わっていた
/// (snapshot 比較とトリアージ結果の再現性が壊れる)。
#[test]
fn reconcile_with_moves_preserves_removed_input_order() {
    let names = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    ];
    let removed: Vec<ApiSymbolCandidate> = names
        .iter()
        .map(|n| ApiSymbolCandidate {
            name: (*n).into(),
            kind: "function".into(),
            file: "lib.ts".into(),
            signature: format!("export function {n}()"),
        })
        .collect();

    // ペアになる add が無いので全件 kept_removed に残り、順序は入力どおりであるべき。
    // HashMap 反復順に依存していると 1 プロセス内でも実行ごとに変わりうるため、
    // 同一プロセス内で繰り返し呼んで安定性も見る。
    for _ in 0..8 {
        let (_, kept_removed, moved) =
            reconcile_with_moves(Vec::new(), removed.clone(), Vec::new());
        assert!(moved.is_empty());
        let got: Vec<&str> = kept_removed.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(got, names, "kept_removed は removed の入力順を保つべき");
    }
}

/// ペア化された removed だけが除外され、残りは入力順のまま残る。
#[test]
fn reconcile_with_moves_preserves_order_around_matched_entries() {
    let removed: Vec<ApiSymbolCandidate> = ["a", "b", "c", "d"]
        .iter()
        .map(|n| ApiSymbolCandidate {
            name: (*n).into(),
            kind: "function".into(),
            file: "old.ts".into(),
            signature: format!("export function {n}()"),
        })
        .collect();
    // 真ん中の `b` だけが移動先で見つかる
    let added = vec![ApiSymbolCandidate {
        name: "b".into(),
        kind: "function".into(),
        file: "new.ts".into(),
        signature: "export function b()".into(),
    }];

    let (_, kept_removed, moved) = reconcile_with_moves(added.clone(), removed, added);
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].name, "b");
    let got: Vec<&str> = kept_removed.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(got, ["a", "c", "d"], "相殺分だけ抜けて順序は保たれる");
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
