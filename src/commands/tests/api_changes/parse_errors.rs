//! 代替文法の解析失敗と、本当に消えた公開関数を区別する。

use crate::commands::tests::common::*;

const BEFORE: &str = include_str!("../../../../tests/fixtures/bash_parse_recovery/before.zsh");
const AFTER: &str = include_str!("../../../../tests/fixtures/bash_parse_recovery/after.zsh");

#[test]
fn zsh_parse_errors_do_not_prove_api_removal() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("sample.zsh", BEFORE),
            (
                "consumer.zsh",
                "source ./sample.zsh\ncheck_revision 9.5\nload_record\ngone\n",
            ),
        ],
        "base",
    );
    std::fs::write(repo.join("sample.zsh"), AFTER).unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.removed.is_empty() && api.removed_dead.is_empty(),
        "{api:?}"
    );
    let json = serde_json::to_value(&api).unwrap();
    let uncertain = json["uncertain_removals"].as_array().unwrap();
    assert_eq!(uncertain.len(), 2);
    assert_eq!(uncertain[0]["name"], "check_revision");
    assert_eq!(uncertain[0]["line"], 1);
    assert_eq!(uncertain[1]["name"], "load_record");
    assert_eq!(uncertain[1]["line"], 7);
    assert!(
        uncertain
            .iter()
            .all(|s| s["file"] == "sample.zsh" && s["reason"] == "parse_error_region")
    );

    // 同じ ERROR に覆われる実削除も、呼び出しが残る限り blocking の削除として残す。
    std::fs::write(
        repo.join("sample.zsh"),
        format!("{BEFORE}function gone() {{ :; }}\n"),
    )
    .unwrap();
    git_commit_files(repo, &[], "add removable function");
    std::fs::write(repo.join("sample.zsh"), AFTER).unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert_eq!(
        api.removed
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["gone"]
    );
    assert!(api.removed_dead.is_empty());
}

#[test]
fn zsh_parse_errors_do_not_hide_deletions_with_textual_lookalikes() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let before = format!("{BEFORE}function gone() {{ :; }}\n");
    git_commit_files(
        repo,
        &[("sample.zsh", &before), ("consumer.zsh", "gone\n")],
        "base",
    );
    for lookalike in [
        "gone argument\n",
        "# gone() {\n",
        "echo 'gone() {'\n",
        "cat <<EOF\ngone() {\nEOF\n",
        "cat <<'EOF'\ngone() {\nEOF\n",
        "echo '\ngone() {\n'\n",
        "echo \"\ngone() {\n\"\n",
        "cat <<EOF\ngone() {\n",
        "echo '\ngone() {\n",
    ] {
        std::fs::write(repo.join("sample.zsh"), format!("{AFTER}{lookalike}")).unwrap();
        let api = detect_api_changes_from_worktree(repo);
        assert!(
            api.removed.iter().any(|s| s.name == "gone"),
            "{lookalike:?}: {api:?}"
        );
        assert!(!api.uncertain_removals.iter().any(|s| s.name == "gone"));
    }
    // 抽出済みの宣言には不確実性を追加しない。
    std::fs::write(repo.join("sample.zsh"), BEFORE).unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(api.removed.iter().any(|s| s.name == "gone"));
    assert!(api.uncertain_removals.is_empty());
}

#[test]
fn zsh_parse_errors_follow_renames_without_inventing_moves() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("sample.zsh", BEFORE), ("consumer.zsh", "load_record\n")],
        "base",
    );
    std::fs::remove_file(repo.join("sample.zsh")).unwrap();
    std::fs::write(repo.join("renamed.zsh"), AFTER).unwrap();
    // Git の類似度閾値には依存せず、rename 差分の帰属を検証する。
    let api = crate::commands::detect_api_changes(
        repo.to_str().unwrap(),
        "HEAD",
        &[crate::models::impact::DiffFile {
            old_path: "sample.zsh".into(),
            new_path: "renamed.zsh".into(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 11,
                new_start: 1,
                new_count: 15,
            }],
            deleted_old_source: None,
        }],
    );
    assert!(
        api.removed.is_empty() && api.removed_dead.is_empty() && api.moved.is_empty(),
        "{api:?}"
    );
    assert_eq!(api.uncertain_removals.len(), 2);
    assert!(
        api.uncertain_removals
            .iter()
            .all(|s| s.file == "renamed.zsh")
    );
}

#[test]
fn zsh_parse_errors_do_not_disambiguate_duplicate_declarations() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let before = format!("{BEFORE}function load_record() {{ :; }}\n");
    git_commit_files(
        repo,
        &[("sample.zsh", &before), ("consumer.zsh", "load_record\n")],
        "base",
    );
    std::fs::write(repo.join("sample.zsh"), AFTER).unwrap();
    let api = detect_api_changes_from_worktree(repo);
    assert!(
        api.removed.iter().any(|s| s.name == "load_record"),
        "{api:?}"
    );
    assert!(
        !api.uncertain_removals
            .iter()
            .any(|s| s.name == "load_record")
    );
}

#[test]
fn zsh_parse_errors_recognize_alternative_function_headers() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("sample.zsh", BEFORE), ("consumer.zsh", "load_record\n")],
        "base",
    );
    for header in [
        "function load_record {",
        "load_record () {",
        "load_record()\n{",
        "load_record() (",
    ] {
        let after = AFTER.replace("function load_record() {", header);
        std::fs::write(repo.join("sample.zsh"), after).unwrap();
        let api = detect_api_changes_from_worktree(repo);
        assert!(
            api.removed.is_empty() && api.removed_dead.is_empty(),
            "{header}: {api:?}"
        );
        assert!(
            api.uncertain_removals
                .iter()
                .any(|s| s.name == "load_record"),
            "{header}: {api:?}"
        );
    }
}

#[test]
fn zsh_parse_errors_do_not_recover_nested_declarations_as_top_level() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            ("sample.zsh", "function gone() { :; }\n"),
            ("consumer.zsh", "gone\n"),
        ],
        "base",
    );
    for opener in [
        "x=$(\n",
        "cat <(\n",
        "cat >(\n",
        "outer() {\n",
        "(\n",
        "$((\n",
        "((\n",
        "[[\n",
        "x=$[\n",
        "echo $'\n",
    ] {
        let after = format!("{opener}{}", AFTER.replace("load_record", "gone"));
        std::fs::write(repo.join("sample.zsh"), after).unwrap();
        let api = detect_api_changes_from_worktree(repo);
        assert!(
            api.removed.iter().any(|s| s.name == "gone"),
            "{opener:?}: {api:?}"
        );
        assert!(!api.uncertain_removals.iter().any(|s| s.name == "gone"));
    }
}
