use std::collections::{HashMap, HashSet};
use std::process::Command;

use anyhow::{Result, bail};

use crate::error::{AstroError, ErrorCode};
use crate::models::cochange::{CoChangeEntry, CoChangeOptions, CoChangeResult};

/// `git log` から共変更パターンを解析する。
///
/// フィルタの適用順序:
///   1. merge-base 打ち切り（有効時）
///   2. lookback による走査コミット数制限
///   3. `max_files_per_commit` を超えるコミットの除外
///   4. `min_samples` 未満のペアの除外
///   5. `min_confidence` 未満のペアの除外
///   6. `skip_deleted_files` (HEAD ツリーに存在しないファイルを含むペアの除外)
///   7. `filter_file` による絞り込み
pub fn analyze_cochange(dir: &str, opts: &CoChangeOptions) -> Result<CoChangeResult> {
    // merge-base 打ち切り: 有効かつ算出可能な場合のみコミット範囲を限定する
    let base_revision = if opts.bounded_by_merge_base {
        resolve_merge_base(dir)
    } else {
        None
    };

    // git log のコミット履歴を取得する
    let commits = run_git_log(dir, opts.lookback, base_revision.as_deref())?;
    let commits_analyzed = commits.len();

    // 各ファイルの変更回数をカウントする
    let mut file_counts: HashMap<String, usize> = HashMap::new();
    for commit in &commits {
        for file in commit {
            if let Some(count) = file_counts.get_mut(file.as_str()) {
                *count += 1;
            } else {
                file_counts.insert(file.clone(), 1);
            }
        }
    }

    // 同一コミット内のファイルペアをカウントする。max_files_per_commit を
    // 超えるコミットは初期化・大量生成系としてスキップする。
    let mut pair_counts: HashMap<(String, String), usize> = HashMap::new();
    for commit in &commits {
        if commit.len() < 2 || commit.len() > opts.max_files_per_commit {
            continue;
        }
        let mut files: Vec<&String> = commit.iter().collect();
        files.sort();
        files.dedup();
        for i in 0..files.len() {
            for j in (i + 1)..files.len() {
                let key = (files[i].clone(), files[j].clone());
                *pair_counts.entry(key).or_insert(0) += 1;
            }
        }
    }

    // skip_deleted_files 用に HEAD ツリーのファイル集合を 1 回だけ取得する
    let head_tree: Option<HashSet<String>> = if opts.skip_deleted_files {
        list_head_tree(dir).ok()
    } else {
        None
    };

    let mut entries: Vec<CoChangeEntry> = pair_counts
        .into_iter()
        .filter_map(|((file_a, file_b), co_changes)| {
            if co_changes < opts.min_samples {
                return None;
            }

            let total_a = *file_counts.get(&file_a).unwrap_or(&0);
            let total_b = *file_counts.get(&file_b).unwrap_or(&0);
            let max_total = total_a.max(total_b);
            if max_total == 0 {
                return None;
            }

            let confidence = co_changes as f64 / max_total as f64;
            if confidence < opts.min_confidence {
                return None;
            }

            if let Some(tree) = head_tree.as_ref()
                && (!tree.contains(&file_a) || !tree.contains(&file_b))
            {
                return None;
            }

            if let Some(filter) = opts.filter_file.as_deref()
                && file_a != filter
                && file_b != filter
            {
                return None;
            }

            Some(CoChangeEntry {
                file_a,
                file_b,
                co_changes,
                total_changes_a: total_a,
                total_changes_b: total_b,
                confidence,
            })
        })
        .collect();

    entries.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(CoChangeResult {
        entries,
        commits_analyzed,
    })
}

/// `git log --name-only` を実行しコミットごとの変更ファイル一覧を返す。
/// `base_revision` が Some の場合、その履歴からのみ辿る。
fn run_git_log(
    dir: &str,
    lookback: usize,
    base_revision: Option<&str>,
) -> Result<Vec<Vec<String>>> {
    let mut args: Vec<String> = vec![
        "log".into(),
        "--name-only".into(),
        "--pretty=format:---COMMIT---".into(),
        "-n".into(),
        lookback.to_string(),
    ];
    if let Some(rev) = base_revision {
        args.push(rev.to_string());
    }

    let output = Command::new("git")
        .args(&args)
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(AstroError::new(
            ErrorCode::IoError,
            format!("git log failed: {stderr}"),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut commits: Vec<Vec<String>> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed == "---COMMIT---" {
            if !current_files.is_empty() {
                commits.push(std::mem::take(&mut current_files));
            }
        } else if !trimmed.is_empty() {
            current_files.push(trimmed.to_string());
        }
    }
    if !current_files.is_empty() {
        commits.push(current_files);
    }
    Ok(commits)
}

/// `git merge-base HEAD <default-branch>` を算出する。
/// 算出できない場合（デフォルトブランチ不明、HEAD が単独、等）は None を返す。
fn resolve_merge_base(dir: &str) -> Option<String> {
    let default_branch = detect_default_branch(dir)?;

    let output = Command::new("git")
        .args(["merge-base", "HEAD", &default_branch])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let base = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if base.is_empty() { None } else { Some(base) }
}

/// デフォルトブランチの参照名を検出する。優先順位:
///   1. `origin/HEAD` が指すブランチ
///   2. `origin/main`, `origin/master`
///   3. ローカル `main`, `master`
fn detect_default_branch(dir: &str) -> Option<String> {
    // 1. origin/HEAD symbolic ref
    if let Ok(output) = Command::new("git")
        .args([
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .current_dir(dir)
        .output()
        && output.status.success()
    {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }

    // 2 & 3. 候補を順に試す
    for candidate in [
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/heads/main",
        "refs/heads/master",
    ] {
        let output = Command::new("git")
            .args(["rev-parse", "--quiet", "--verify", candidate])
            .current_dir(dir)
            .output();
        if let Ok(out) = output
            && out.status.success()
        {
            // rev-parse が成功したら、ブランチ名を返す
            let short_name = candidate
                .strip_prefix("refs/remotes/")
                .or_else(|| candidate.strip_prefix("refs/heads/"))
                .unwrap_or(candidate)
                .to_string();
            return Some(short_name);
        }
    }

    None
}

/// `git ls-tree -r --name-only HEAD` で HEAD ツリーのファイル一覧を取得する。
fn list_head_tree(dir: &str) -> Result<HashSet<String>> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(AstroError::new(
            ErrorCode::IoError,
            format!("git ls-tree failed: {stderr}"),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用のヘルパー。git リポジトリを初期化する。
    fn init_repo(repo: &std::path::Path) {
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "test"],
            vec!["config", "user.email", "test@example.com"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(repo)
                .output()
                .unwrap();
        }
    }

    /// テスト用のヘルパー。ファイル群を書き込んでコミットする。
    fn commit_files(repo: &std::path::Path, files: &[(&str, &str)], msg: &str) {
        for (name, content) in files {
            let path = repo.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(repo)
            .output()
            .unwrap();
    }

    fn rm_commit(repo: &std::path::Path, file: &str, msg: &str) {
        std::process::Command::new("git")
            .args(["rm", file])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(repo)
            .output()
            .unwrap();
    }

    fn opts_with(mutator: impl FnOnce(&mut CoChangeOptions)) -> CoChangeOptions {
        let mut o = CoChangeOptions::default();
        // テストは merge-base 打ち切り/skip deleted をデフォルト有効のまま扱う
        // 必要なテストだけ mutator で切り替える
        mutator(&mut o);
        o
    }

    /// git 未初期化のディレクトリでエラーを返す
    #[test]
    fn analyze_cochange_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = analyze_cochange(
            dir.path().to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.3;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
            }),
        );
        assert!(result.is_err());
    }

    /// 空リポジトリでもパニックしない
    #[test]
    fn analyze_cochange_empty_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);
        // コミットがないリポジトリでは git log がエラーを返す
        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.3;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
            }),
        );
        assert!(result.is_err());
    }

    /// 3 回同時変更 → 正常に検出される
    #[test]
    fn analyze_cochange_detects_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}")),
                    ("b.rs", &format!("fn b() {{ {i} }}")),
                ],
                &format!("pair {i}"),
            );
        }

        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.3;
                o.min_samples = 2;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
            }),
        )
        .unwrap();
        assert_eq!(result.commits_analyzed, 3);
        assert!(!result.entries.is_empty());
        assert!(result.entries.iter().any(|e| {
            (e.file_a == "a.rs" && e.file_b == "b.rs") || (e.file_a == "b.rs" && e.file_b == "a.rs")
        }));
    }

    /// filter_file で特定ファイルのペアのみ返す
    #[test]
    fn analyze_cochange_filter_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("// {i}")),
                    ("b.rs", &format!("// {i}")),
                    ("c.rs", &format!("// {i}")),
                ],
                &format!("triple {i}"),
            );
        }

        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.3;
                o.min_samples = 2;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
                o.filter_file = Some("a.rs".into());
            }),
        )
        .unwrap();
        for entry in &result.entries {
            assert!(entry.file_a == "a.rs" || entry.file_b == "a.rs");
        }
    }

    /// max_files_per_commit を超える初期 bulk コミットはスキップされる
    #[test]
    fn analyze_cochange_skips_bulk_initial_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 31 ファイルを 1 コミットで追加（閾値 30 を超える）
        let names: Vec<String> = (0..31).map(|i| format!("file_{i}.rs")).collect();
        let files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "// init")).collect();
        commit_files(repo, &files, "bulk initial commit");

        // 個別ファイルを更新
        commit_files(repo, &[("file_0.rs", "// updated")], "update file_0");

        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 50;
                o.min_confidence = 0.0;
                o.min_samples = 1;
                o.max_files_per_commit = 30;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
            }),
        )
        .unwrap();
        assert!(
            result.entries.is_empty(),
            "bulk commit should not generate cochange pairs, got: {:?}",
            result.entries
        );
    }

    /// min_samples 未満のペアは発報されない
    #[test]
    fn analyze_cochange_requires_minimum_samples() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // a.rs と b.rs を 2 回だけ同時変更
        for i in 0..2 {
            commit_files(
                repo,
                &[("a.rs", &format!("// {i}")), ("b.rs", &format!("// {i}"))],
                &format!("pair {i}"),
            );
        }
        // a.rs を単独で変更
        commit_files(repo, &[("a.rs", "// solo")], "solo");

        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.0;
                o.min_samples = 3;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = false;
            }),
        )
        .unwrap();
        assert!(
            result.entries.is_empty(),
            "co_changes=2 should be filtered when min_samples=3, got: {:?}",
            result.entries
        );
    }

    /// HEAD に存在しないファイルを含むペアはスキップされる
    #[test]
    fn analyze_cochange_skips_deleted_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // a.rs と b.rs を 3 回同時変更（閾値を満たす）
        for i in 0..3 {
            commit_files(
                repo,
                &[("a.rs", &format!("// {i}")), ("b.rs", &format!("// {i}"))],
                &format!("pair {i}"),
            );
        }
        // b.rs を削除
        rm_commit(repo, "b.rs", "remove b.rs");

        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 10;
                o.min_confidence = 0.0;
                o.min_samples = 2;
                o.bounded_by_merge_base = false;
                o.skip_deleted_files = true;
            }),
        )
        .unwrap();
        assert!(
            result
                .entries
                .iter()
                .all(|e| e.file_a != "b.rs" && e.file_b != "b.rs"),
            "deleted file should not appear in cochange entries, got: {:?}",
            result.entries
        );
    }

    /// merge-base 打ち切りで feature branch のコミットが統計に混ざらない
    #[test]
    fn analyze_cochange_bounded_by_merge_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // main 上で x.rs と y.rs を 4 回同時変更（真の cochange ペア）
        for i in 0..4 {
            commit_files(
                repo,
                &[("x.rs", &format!("// {i}")), ("y.rs", &format!("// {i}"))],
                &format!("main pair {i}"),
            );
        }

        // feature ブランチを切って a.rs と b.rs を 3 回同時変更
        std::process::Command::new("git")
            .args(["checkout", "-b", "feature"])
            .current_dir(repo)
            .output()
            .unwrap();
        for i in 0..3 {
            commit_files(
                repo,
                &[("a.rs", &format!("// {i}")), ("b.rs", &format!("// {i}"))],
                &format!("feature pair {i}"),
            );
        }

        // merge-base 打ち切り有効で解析する
        let result = analyze_cochange(
            repo.to_str().unwrap(),
            &opts_with(|o| {
                o.lookback = 50;
                o.min_confidence = 0.3;
                o.min_samples = 2;
                o.bounded_by_merge_base = true;
                o.skip_deleted_files = false;
            }),
        )
        .unwrap();

        // feature 内の a.rs↔b.rs ペアは含まれないはず
        assert!(
            result
                .entries
                .iter()
                .all(|e| !((e.file_a == "a.rs" && e.file_b == "b.rs")
                    || (e.file_a == "b.rs" && e.file_b == "a.rs"))),
            "feature branch pair should not appear, got: {:?}",
            result.entries
        );
        // main 側の x.rs↔y.rs は検出される
        assert!(
            result
                .entries
                .iter()
                .any(|e| (e.file_a == "x.rs" && e.file_b == "y.rs")
                    || (e.file_a == "y.rs" && e.file_b == "x.rs")),
            "main pair should still be detected, got: {:?}",
            result.entries
        );
    }
}
