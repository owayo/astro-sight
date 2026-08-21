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

    let (kept_added, kept_removed, moved, _) =
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
        let (_, kept_removed, moved, _) =
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

    let (_, kept_removed, moved, _) = reconcile_with_moves(added.clone(), removed, added);
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

    let (kept_added, kept_removed, moved, _) =
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

    let (kept_added, kept_removed, moved, _) =
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

/// N→1 集約 (重複していた同名クラスを共有パッケージへ 1 本化するリファクタ) では、
/// 同質な削除のうち 1 件だけを `moved` に相殺してはならない。
///
/// 旧実装は FIFO で先頭の削除だけを消費していたため、どちらが `moved` になり
/// どちらが `api.rm` に残るかが diff の並び順という無関係な要因で決まっていた
/// (Issue 2026-08-21-api-consolidation-many-to-one-move)。
///
/// **対照ケースを同一テスト内に持つ** — 削除が 1 件だけなら従来どおり `moved` に
/// なることを先に固定しないと、「常に moved が空」でもこのテストは通ってしまう。
#[test]
fn reconcile_with_moves_keeps_all_removals_for_many_to_one_consolidation() {
    let destination = ApiSymbolCandidate {
        name: "Store".into(),
        kind: "class".into(),
        file: "shared/store.py".into(),
        signature: "class Store:".into(),
    };
    let removed_a = ApiSymbolCandidate {
        name: "Store".into(),
        kind: "class".into(),
        file: "pkg_a/store.py".into(),
        signature: "class Store:".into(),
    };
    let removed_b = ApiSymbolCandidate {
        name: "Store".into(),
        kind: "class".into(),
        file: "pkg_b/store.py".into(),
        signature: "class Store:".into(),
    };

    // 対照: 削除が 1 件なら対応付けは一意なので従来どおり moved になる。
    let (_, kept_removed, moved, _) = reconcile_with_moves(
        vec![destination.clone()],
        vec![removed_a.clone()],
        vec![destination.clone()],
    );
    assert_eq!(
        moved.len(),
        1,
        "1 対 1 の move 相殺は維持されるべき (対照が壊れるとこのテストは無意味になる)"
    );
    assert_eq!(moved[0].from, "pkg_a/store.py");
    assert!(kept_removed.is_empty(), "got: {kept_removed:?}");

    // 対照: 追加側に同一キーが 1 件も無い純粋な削除は「対応付け不能」ではない。
    // ここを ambiguous に含めると、通常の削除まで removed_dead へ降格できなくなる。
    let (_, _, _, ambiguous) =
        reconcile_with_moves(Vec::new(), vec![removed_a.clone()], Vec::new());
    assert!(
        ambiguous.is_empty(),
        "対応先の無い削除は ambiguous にしない。got: {ambiguous:?}"
    );

    // 本題: 同質な削除が 2 件あるとどちらが移動元か決められないので、
    // 1 件も相殺せず両方 removed に残す。
    let (kept_added, kept_removed, moved, ambiguous) = reconcile_with_moves(
        vec![destination.clone()],
        vec![removed_a.clone(), removed_b.clone()],
        vec![destination.clone()],
    );
    assert!(
        moved.is_empty(),
        "2→1 集約では対応付けが一意にならないので moved を作らない。got: {moved:?}"
    );
    let kept: Vec<&str> = kept_removed.iter().map(|c| c.file.as_str()).collect();
    assert_eq!(
        kept,
        ["pkg_a/store.py", "pkg_b/store.py"],
        "同質な削除は片方だけ消さず、入力順のまま両方残す"
    );
    assert_eq!(
        kept_added.len(),
        1,
        "相殺しなかったので追加側も残る。got: {kept_added:?}"
    );
    assert!(
        ambiguous.contains(&crate::commands::api_changes::api_relation_key(&removed_a)),
        "対応付け不能なキーは blocking 固定のため申告する。got: {ambiguous:?}"
    );
}

/// 対応付けの可否はバケットの中身だけで決まるので、`removed` の並び順を入れ替えても
/// 結果は変わらない。旧 FIFO 実装ではここで `moved[0].from` が入れ替わっていた。
#[test]
fn reconcile_with_moves_many_to_one_result_is_independent_of_input_order() {
    let destination = ApiSymbolCandidate {
        name: "Store".into(),
        kind: "class".into(),
        file: "shared/store.py".into(),
        signature: "class Store:".into(),
    };
    let make = |file: &str| ApiSymbolCandidate {
        name: "Store".into(),
        kind: "class".into(),
        file: file.into(),
        signature: "class Store:".into(),
    };

    for order in [
        ["pkg_a/store.py", "pkg_b/store.py"],
        ["pkg_b/store.py", "pkg_a/store.py"],
    ] {
        let removed: Vec<ApiSymbolCandidate> = order.iter().map(|f| make(f)).collect();
        let (_, kept_removed, moved, _) = reconcile_with_moves(
            vec![destination.clone()],
            removed,
            vec![destination.clone()],
        );
        assert!(
            moved.is_empty(),
            "並び順に依らず moved は作らない。order={order:?} got: {moved:?}"
        );
        let kept: Vec<&str> = kept_removed.iter().map(|c| c.file.as_str()).collect();
        assert_eq!(kept, order, "kept_removed は入力順を保つ");
    }
}

/// 移動先が一意でない (同一キーの追加が複数ファイルにある) 場合も対応付け不能として
/// 削除を残す。1→2 は N→1 と同じく「どちらへ移動したか」を決められない。
#[test]
fn reconcile_with_moves_keeps_removal_when_destination_is_ambiguous() {
    let removed = ApiSymbolCandidate {
        name: "Store".into(),
        kind: "class".into(),
        file: "pkg_a/store.py".into(),
        signature: "class Store:".into(),
    };
    let dest_one = ApiSymbolCandidate {
        name: "Store".into(),
        kind: "class".into(),
        file: "shared/store.py".into(),
        signature: "class Store:".into(),
    };
    let dest_two = ApiSymbolCandidate {
        file: "other/store.py".into(),
        ..dest_one.clone()
    };

    // 対照: 移動先が 1 ファイルなら moved になる。
    let (_, _, moved, _) = reconcile_with_moves(
        vec![dest_one.clone()],
        vec![removed.clone()],
        vec![dest_one.clone()],
    );
    assert_eq!(moved.len(), 1, "移動先が一意なら従来どおり相殺する");

    // 本題: 移動先候補が 2 ファイルあると決められない。
    let (kept_added, kept_removed, moved, _) = reconcile_with_moves(
        vec![dest_one.clone(), dest_two.clone()],
        vec![removed.clone()],
        vec![dest_one, dest_two],
    );
    assert!(
        moved.is_empty(),
        "移動先が一意でなければ moved を作らない。got: {moved:?}"
    );
    assert_eq!(kept_removed.len(), 1, "削除は残る。got: {kept_removed:?}");
    assert_eq!(kept_added.len(), 2, "追加も残る。got: {kept_added:?}");
}

/// 同一 (name, kind, signature, file) の重複候補は 1 件として数える。
/// 重複を移動先 2 件と数えてしまうと、正当な 1 対 1 の move まで相殺できなくなる。
#[test]
fn reconcile_with_moves_treats_duplicate_new_candidates_in_same_file_as_one() {
    let destination = ApiSymbolCandidate {
        name: "Store".into(),
        kind: "class".into(),
        file: "shared/store.py".into(),
        signature: "class Store:".into(),
    };
    let removed = ApiSymbolCandidate {
        name: "Store".into(),
        kind: "class".into(),
        file: "pkg_a/store.py".into(),
        signature: "class Store:".into(),
    };

    let (kept_added, kept_removed, moved, _) = reconcile_with_moves(
        vec![destination.clone()],
        vec![removed],
        // 同じファイルの同じシンボルが 2 度候補に上がっても移動先は 1 ファイル。
        vec![destination.clone(), destination],
    );
    assert_eq!(moved.len(), 1, "重複候補で移動先が増えてはならない");
    assert_eq!(moved[0].to, "shared/store.py");
    assert!(kept_removed.is_empty(), "got: {kept_removed:?}");
    assert!(kept_added.is_empty(), "got: {kept_added:?}");
}

/// E2E: 重複していた同名クラスを共有パッケージへ集約すると、両方の削除が同じ区分で
/// 報告される。対応付けが一意なシンボル (集約先にしか無い `ping`) は従来どおり
/// `moved` に相殺されることも同時に固定する。
#[test]
fn detect_api_changes_many_to_one_consolidation_reports_both_removals() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let store_a = "\
\"\"\"service-a store.\"\"\"


class Store:
    def open(self):
        return 1

    def close(self):
        return 2

    def ping(self):
        return 3
";
    let store_b = "\
\"\"\"service-b store.\"\"\"


class Store:
    def open(self):
        return 1

    def close(self):
        return 2
";
    git_commit_files(
        repo,
        &[("pkg_a/store.py", store_a), ("pkg_b/store.py", store_b)],
        "duplicated",
    );

    // 両方を削除して共有パッケージへ 1 本化する。
    fs::remove_file(repo.join("pkg_a/store.py")).expect("rm a");
    fs::remove_file(repo.join("pkg_b/store.py")).expect("rm b");
    let shared = repo.join("shared/store.py");
    fs::create_dir_all(shared.parent().expect("parent")).expect("mkdir");
    fs::write(&shared, store_a).expect("write shared");

    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "pkg_a/store.py".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: Vec::new(),
            deleted_old_source: Some(store_a.as_bytes().to_vec()),
        },
        crate::models::impact::DiffFile {
            old_path: "pkg_b/store.py".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: Vec::new(),
            deleted_old_source: Some(store_b.as_bytes().to_vec()),
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "shared/store.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 13,
            }],
            deleted_old_source: None,
        },
    ];

    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let moved_names: Vec<(&str, &str)> = api
        .moved
        .iter()
        .map(|m| (m.name.as_str(), m.from.as_str()))
        .collect();
    // 対照: 集約先にしか存在しない `ping` は対応付けが一意なので moved のまま。
    assert!(
        moved_names.contains(&("Store.ping", "pkg_a/store.py")),
        "一意に対応付く移動は従来どおり moved になるべき (対照)。got: {moved_names:?}"
    );

    // 本題: 2 ファイルに重複していた Store / open / close は片方だけ moved にしない。
    //
    // `removed` (blocking) と `removed_dead` (informational) は**別々に**検証する。
    // 合流させて比較すると「片方が blocking / もう片方が informational」でも通ってしまい、
    // 本 Issue が解消したかった非対称をまさに見逃す (codex レビュー指摘)。
    //
    // このフィクスチャには削除シンボルへの残存参照が無いので、全員が informational で
    // 一致する = 区分は揃っているため保守側への引き上げは起きない。
    for name in ["Store", "Store.open", "Store.close"] {
        assert!(
            !api.moved.iter().any(|m| m.name == name),
            "重複していた {name} は対応付け不能なので moved にしてはならない。got: {moved_names:?}"
        );
        let blocking: Vec<&str> = api
            .removed
            .iter()
            .filter(|s| s.name == name)
            .map(|s| s.file.as_str())
            .collect();
        let informational: Vec<&str> = api
            .removed_dead
            .iter()
            .filter(|s| s.name == name)
            .map(|s| s.file.as_str())
            .collect();
        assert_eq!(
            informational,
            ["pkg_a/store.py", "pkg_b/store.py"],
            "{name} は両方の削除元が同じ区分 (removed_dead) に揃うべき。\
             removed={blocking:?} removed_dead={informational:?}"
        );
        assert!(
            blocking.is_empty(),
            "残存参照ゼロで一致しているグループを blocking へ引き上げてはならない。got: {blocking:?}"
        );
    }
}

/// 対応付け不能な削除を blocking へ固定しないと、`removed_dead` への降格判定が
/// 候補ごとの `old_path` に依存するせいで**同質な削除が別区分に割れる**。
///
/// `pkg/alpha.py` と `pkg/beta.py` の同名クラスを共有パッケージへ集約し、HEAD に
/// `alpha.Store()` という属性アクセスだけが残るケースでは、Python 属性帰属
/// (`RefOrigin::PythonAttributeAccess`) が受信側モジュール名を削除元ファイル名と
/// 突き合わせるため、alpha 側だけ「残存参照あり = blocking」、beta 側は
/// 「残存参照なし = informational」になっていた (codex レビューで指摘され実測で確認)。
#[test]
fn detect_api_changes_ambiguous_consolidation_does_not_split_across_removed_buckets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let store = "\
class Store:
    def open(self):
        return 1
";
    // `alpha.Store()` を呼ぶ利用側は HEAD にそのまま残す (追随漏れの再現)。
    git_commit_files(
        repo,
        &[
            ("pkg/__init__.py", ""),
            ("pkg/alpha.py", store),
            ("pkg/beta.py", store),
            (
                "app.py",
                "from pkg import alpha\n\n\ndef run():\n    return alpha.Store()\n",
            ),
        ],
        "duplicated",
    );

    fs::remove_file(repo.join("pkg/alpha.py")).expect("rm alpha");
    fs::remove_file(repo.join("pkg/beta.py")).expect("rm beta");
    let shared = repo.join("shared/store.py");
    fs::create_dir_all(shared.parent().expect("parent")).expect("mkdir");
    fs::write(&shared, store).expect("write shared");

    let deleted = |path: &str| crate::models::impact::DiffFile {
        old_path: path.to_string(),
        new_path: "/dev/null".to_string(),
        hunks: Vec::new(),
        deleted_old_source: Some(store.as_bytes().to_vec()),
    };
    let diff_files = vec![
        deleted("pkg/alpha.py"),
        deleted("pkg/beta.py"),
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "shared/store.py".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 3,
            }],
            deleted_old_source: None,
        },
    ];

    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let blocking: Vec<&str> = api
        .removed
        .iter()
        .filter(|s| s.name == "Store")
        .map(|s| s.file.as_str())
        .collect();
    let informational: Vec<&str> = api
        .removed_dead
        .iter()
        .filter(|s| s.name == "Store")
        .map(|s| s.file.as_str())
        .collect();
    assert_eq!(
        blocking,
        ["pkg/alpha.py", "pkg/beta.py"],
        "同質な削除は両方 blocking に揃うべき。removed={blocking:?} removed_dead={informational:?}"
    );
    assert!(
        informational.is_empty(),
        "帰属判定の差で片方だけ informational へ落としてはならない。got: {informational:?}"
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
