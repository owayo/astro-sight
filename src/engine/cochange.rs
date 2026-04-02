use std::collections::HashMap;
use std::process::Command;

use anyhow::{Result, bail};

use crate::error::{AstroError, ErrorCode};
use crate::models::cochange::{CoChangeEntry, CoChangeResult};

/// git log から共変更パターンを解析する。
///
/// - `dir`: the git repository directory
/// - `lookback`: number of recent commits to analyze
/// - `min_confidence`: minimum confidence threshold (0.0 to 1.0)
/// - `filter_file`: optional file path to filter results (only pairs containing this file)
pub fn analyze_cochange(
    dir: &str,
    lookback: usize,
    min_confidence: f64,
    filter_file: Option<&str>,
) -> Result<CoChangeResult> {
    // 1. git log でコミットのファイルリストを取得
    let output = Command::new("git")
        .args([
            "log",
            "--name-only",
            "--pretty=format:---COMMIT---",
            "-n",
            &lookback.to_string(),
        ])
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

    // 2. コミットを解析
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
    // 最後のコミットを忘れずに追加
    if !current_files.is_empty() {
        commits.push(current_files);
    }

    let commits_analyzed = commits.len();

    // 3. 各ファイルの変更回数をカウント
    // get_mut で既存キーを参照検索し、初回のみ clone して挿入する
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

    // 4. 共変更（同一コミット内のファイルペア）をカウント
    let mut pair_counts: HashMap<(String, String), usize> = HashMap::new();
    // ファイル数が多すぎるコミット（初期コミット、大規模リファクタ等）はペア爆発を防ぐためスキップ
    const MAX_FILES_PER_COMMIT: usize = 100;
    for commit in &commits {
        if commit.len() < 2 || commit.len() > MAX_FILES_PER_COMMIT {
            continue;
        }
        // 重複排除済みペアを生成
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

    // 5. confidence を算出しエントリを構築
    let mut entries: Vec<CoChangeEntry> = pair_counts
        .into_iter()
        .filter_map(|((file_a, file_b), co_changes)| {
            let total_a = *file_counts.get(&file_a).unwrap_or(&0);
            let total_b = *file_counts.get(&file_b).unwrap_or(&0);
            let max_total = total_a.max(total_b);
            if max_total == 0 {
                return None;
            }

            let confidence = co_changes as f64 / max_total as f64;
            if confidence < min_confidence {
                return None;
            }

            // ファイル指定時はフィルタ
            if let Some(filter) = filter_file
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

    // 6. confidence の降順でソート
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

#[cfg(test)]
mod tests {
    use super::*;

    /// git 未初期化のディレクトリでエラーを返す
    #[test]
    fn analyze_cochange_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = analyze_cochange(dir.path().to_str().unwrap(), 10, 0.3, None);
        assert!(result.is_err());
    }

    /// コミットが少ない（空リポジトリ）場合でもパニックしない
    #[test]
    fn analyze_cochange_empty_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo)
            .output()
            .unwrap();

        // コミットがないリポジトリでは git log がエラーを返す
        let result = analyze_cochange(repo.to_str().unwrap(), 10, 0.3, None);
        assert!(result.is_err());
    }

    /// 共変更パターンが正しく検出される
    #[test]
    fn analyze_cochange_detects_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

        // git 初期化
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

        // ファイル作成とコミット（a.rs + b.rs を一緒に変更）
        for i in 0..3 {
            std::fs::write(repo.join("a.rs"), format!("fn a() {{ {} }}", i)).unwrap();
            std::fs::write(repo.join("b.rs"), format!("fn b() {{ {} }}", i)).unwrap();
            std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(repo)
                .output()
                .unwrap();
            std::process::Command::new("git")
                .args(["commit", "-m", &format!("commit {}", i)])
                .current_dir(repo)
                .output()
                .unwrap();
        }

        let result = analyze_cochange(repo.to_str().unwrap(), 10, 0.3, None).unwrap();
        assert_eq!(result.commits_analyzed, 3);
        assert!(!result.entries.is_empty());
        // a.rs と b.rs のペアが存在するはず
        assert!(result.entries.iter().any(|e| {
            (e.file_a == "a.rs" && e.file_b == "b.rs") || (e.file_a == "b.rs" && e.file_b == "a.rs")
        }));
    }

    /// filter_file で特定ファイルのペアのみ返す
    #[test]
    fn analyze_cochange_filter_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

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

        for i in 0..3 {
            std::fs::write(repo.join("a.rs"), format!("// {}", i)).unwrap();
            std::fs::write(repo.join("b.rs"), format!("// {}", i)).unwrap();
            std::fs::write(repo.join("c.rs"), format!("// {}", i)).unwrap();
            std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(repo)
                .output()
                .unwrap();
            std::process::Command::new("git")
                .args(["commit", "-m", &format!("commit {}", i)])
                .current_dir(repo)
                .output()
                .unwrap();
        }

        let result = analyze_cochange(repo.to_str().unwrap(), 10, 0.3, Some("a.rs")).unwrap();
        // a.rs を含むペアのみ
        for entry in &result.entries {
            assert!(entry.file_a == "a.rs" || entry.file_b == "a.rs");
        }
    }

    /// 100 ファイル超のコミットはスキップされる
    #[test]
    fn analyze_cochange_skips_large_commits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

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

        // 101 ファイルを一度にコミット
        for i in 0..101 {
            std::fs::write(repo.join(format!("f{}.rs", i)), format!("// {}", i)).unwrap();
        }
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "large commit"])
            .current_dir(repo)
            .output()
            .unwrap();

        let result = analyze_cochange(repo.to_str().unwrap(), 10, 0.0, None).unwrap();
        // ペアは生成されないはず（101ファイルのコミットはスキップ）
        assert!(result.entries.is_empty());
    }
}
