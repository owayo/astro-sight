//! Python の API 差分検出テスト。

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
