use std::collections::{HashMap, HashSet};
use std::process::Command;

use anyhow::{Result, bail};
use rayon::prelude::*;

use crate::error::{AstroError, ErrorCode};
use crate::models::cochange::{
    BLAME_DEFAULT_EXCLUDE_GLOBS, CoChangeEntry, CoChangeOptions, CoChangeResult,
};

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
                denominator: None,
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

/// blame ベースの共変更解析。
///
/// 起点ファイル `source_files` の **変更行** に対して `git blame -L` を当て、
/// 最終修正コミット集合 `C` を作る。各 c ∈ C の `git diff-tree --name-only -r c`
/// から起点以外の共起ファイルを集計し、`co_changes / |C|` を confidence とする。
///
/// 旧 lookback ベースより文脈依存性が高く (ユーザーが今修正中のコードに密結合な
/// ペアだけが浮上)、大規模リポでも履歴全体を舐めずに済む。
pub fn analyze_cochange_blame(dir: &str, opts: &CoChangeOptions) -> Result<CoChangeResult> {
    if !opts.blame {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            "analyze_cochange_blame called without blame mode enabled".to_string(),
        ));
    }
    if opts.source_files.is_empty() {
        return Ok(CoChangeResult {
            entries: Vec::new(),
            commits_analyzed: 0,
        });
    }

    let base_rev: &str = opts.base.as_deref().unwrap_or("HEAD~1");

    // Phase 1: 起点ファイルごとに blame で base コミット側の変更行 SHA を集める。
    //          ファイル単位で rayon 並列化する。
    let blame_per_file: Vec<HashSet<String>> = opts
        .source_files
        .par_iter()
        .map(|f| collect_blame_commits_for_file(dir, f, base_rev).unwrap_or_default())
        .collect();

    let mut commit_set: HashSet<String> = HashSet::new();
    for s in &blame_per_file {
        commit_set.extend(s.iter().cloned());
    }
    if commit_set.is_empty() {
        return Ok(CoChangeResult {
            entries: Vec::new(),
            commits_analyzed: 0,
        });
    }

    // Phase 2: 各コミット c の変更ファイルを diff-tree で取得 (並列化)。
    //          max_files_per_commit を超えるコミット (squash / 大量生成) はスキップ。
    let commits: Vec<String> = commit_set.into_iter().collect();
    let denominator = commits.len();
    let commit_files: Vec<Vec<String>> = commits
        .par_iter()
        .map(|sha| {
            let files = collect_files_in_commit(dir, sha).unwrap_or_default();
            if files.len() > opts.max_files_per_commit {
                Vec::new()
            } else {
                files
            }
        })
        .collect();

    // Phase 3: 候補ファイルごとの共起カウント。
    //          起点ファイル自身は候補から除外。
    //          除外 glob は既定 + 利用者指定 を結合して適用。
    let source_set: HashSet<&str> = opts.source_files.iter().map(String::as_str).collect();
    let exclude_matcher = build_exclude_matcher(&opts.exclude_globs)?;

    let mut co_counts: HashMap<String, usize> = HashMap::new();
    for files in &commit_files {
        // 同一コミットで g が複数ファイルに重複登録されないよう dedup
        let mut seen: HashSet<&str> = HashSet::new();
        for f in files {
            let path = f.as_str();
            if !seen.insert(path) {
                continue;
            }
            if source_set.contains(path) {
                continue;
            }
            if exclude_matcher.is_match(path) {
                continue;
            }
            *co_counts.entry(path.to_string()).or_insert(0) += 1;
        }
    }

    // Phase 4: 起点ファイルごとに該当候補のスコアを algun し entries を組む。
    //          1 起点 1 candidate → 1 entry。entry.file_a = source、file_b = candidate。
    let denom_f = denominator as f64;
    let mut entries: Vec<CoChangeEntry> = Vec::new();
    for (i, source) in opts.source_files.iter().enumerate() {
        let blame_set = &blame_per_file[i];
        if blame_set.is_empty() {
            continue;
        }
        // この起点ファイルに紐づく blame コミット集合の中で各候補の共起回数を再集計
        let mut per_source: HashMap<&str, usize> = HashMap::new();
        for (j, sha) in commits.iter().enumerate() {
            if !blame_set.contains(sha) {
                continue;
            }
            let files = &commit_files[j];
            let mut seen: HashSet<&str> = HashSet::new();
            for f in files {
                let path = f.as_str();
                if !seen.insert(path) {
                    continue;
                }
                if source_set.contains(path) {
                    continue;
                }
                if exclude_matcher.is_match(path) {
                    continue;
                }
                *per_source.entry(path).or_insert(0) += 1;
            }
        }

        let local_denom = blame_set.len() as f64;
        for (cand, co) in per_source {
            if co < opts.min_samples {
                continue;
            }
            let confidence = co as f64 / local_denom;
            if confidence < opts.min_confidence {
                continue;
            }
            entries.push(CoChangeEntry {
                file_a: source.clone(),
                file_b: cand.to_string(),
                co_changes: co,
                total_changes_a: blame_set.len(),
                total_changes_b: *co_counts.get(cand).unwrap_or(&0),
                confidence,
                denominator: Some(blame_set.len()),
            });
        }
    }

    entries.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_a.cmp(&b.file_a))
            .then_with(|| a.file_b.cmp(&b.file_b))
    });

    let _ = denom_f; // future use
    Ok(CoChangeResult {
        entries,
        commits_analyzed: denominator,
    })
}

/// 1 ファイルの diff hunk 群を 1 回の `git blame -L S,+C [-L S,+C]...` にまとめて
/// 最終修正コミット SHA 集合を取得する。
/// - 純粋追加 hunk (old_count = 0) は blame 不要なのでスキップする。
/// - 全 hunk が純粋追加だった場合は空集合を返す。
/// - blame の失敗 (binary / non-existent in base) は空集合を返して継続。
fn collect_blame_commits_for_file(dir: &str, file: &str, base: &str) -> Result<HashSet<String>> {
    // diff --unified=0 で hunk header を取得
    let diff_output = Command::new("git")
        .args(["diff", "--unified=0", base, "HEAD", "--", file])
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !diff_output.status.success() {
        return Ok(HashSet::new());
    }
    let diff_text = String::from_utf8_lossy(&diff_output.stdout);

    // hunk 旧側 (start, count) を抽出
    let ranges: Vec<(u64, u64)> = parse_hunk_old_ranges(&diff_text);
    if ranges.is_empty() {
        return Ok(HashSet::new());
    }

    // 1 起動の git blame に複数 -L を渡す
    let mut args: Vec<String> = vec!["blame".into(), "--line-porcelain".into()];
    for (start, count) in &ranges {
        args.push("-L".into());
        args.push(format!("{},+{}", start, count));
    }
    args.push(base.to_string());
    args.push("--".into());
    args.push(file.to_string());

    let blame_output = Command::new("git")
        .args(&args)
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !blame_output.status.success() {
        return Ok(HashSet::new());
    }
    let blame_text = String::from_utf8_lossy(&blame_output.stdout);

    let mut shas: HashSet<String> = HashSet::new();
    for line in blame_text.lines() {
        // line-porcelain: 各エントリの先頭行は `<sha40> <orig_line> <final_line> [count]`。
        // メタデータ行 (author / summary / filename) は日本語等のマルチバイト文字を含む
        // ことがあるため、byte レベルで先頭 40 バイトが hex + 41 番目がスペースか確認する
        // (str スライスを byte 境界で切るとパニックするため)。
        let bytes = line.as_bytes();
        if bytes.len() < 41 || bytes[40] != b' ' {
            continue;
        }
        let sha_bytes = &bytes[..40];
        if !sha_bytes.iter().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        // 安全: ASCII hex のみで構成されているので UTF-8 検証は不要。
        let sha = std::str::from_utf8(sha_bytes).expect("ascii hex is utf-8");
        shas.insert(sha.to_string());
    }
    Ok(shas)
}

/// `@@ -OLDSTART[,OLDCOUNT] +NEWSTART[,NEWCOUNT] @@` の OLDSTART/OLDCOUNT を抽出する。
/// COUNT 省略時は 1。COUNT が 0 の hunk (純粋追加) は除外する。
fn parse_hunk_old_ranges(diff_text: &str) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for line in diff_text.lines() {
        let Some(rest) = line.strip_prefix("@@ -") else {
            continue;
        };
        // rest 例: "32,3 +33,7 @@ ..."
        let Some(end) = rest.find(' ') else {
            continue;
        };
        let token = &rest[..end];
        let (start_s, count_s) = match token.split_once(',') {
            Some((a, b)) => (a, b),
            None => (token, "1"),
        };
        let Ok(start) = start_s.parse::<u64>() else {
            continue;
        };
        let Ok(count) = count_s.parse::<u64>() else {
            continue;
        };
        if count == 0 {
            continue;
        }
        out.push((start, count));
    }
    out
}

/// `git diff-tree --no-commit-id --name-only -r <sha>` でコミット c の変更ファイル一覧を返す。
fn collect_files_in_commit(dir: &str, sha: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", sha])
        .current_dir(dir)
        .output()
        .map_err(|e| AstroError::new(ErrorCode::IoError, format!("Failed to run git: {e}")))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// 除外 glob マッチャ。
///
/// `globset` を直接依存に追加せず、`ignore::overrides` 経由で組み立てる
/// (既存 refs/dead-code と同じスタイルを踏襲)。
/// ignore の Override は「whitelist」を構築する API なので、除外したいパターンは
/// `!` プレフィクスを付与して登録し、`Match::Ignore` を「除外対象」として判定する。
pub(crate) struct CoChangeExclude {
    inner: ignore::overrides::Override,
}

impl CoChangeExclude {
    pub(crate) fn build(user_globs: &[String]) -> Result<Self> {
        // OverrideBuilder には書き込み可能な root が必要だが、glob マッチング自体は
        // パス文字列で行うため任意のディレクトリで構わない。
        let mut ob = ignore::overrides::OverrideBuilder::new(".");
        for pat in BLAME_DEFAULT_EXCLUDE_GLOBS {
            ob.add(&format!("!{pat}")).map_err(|e| {
                AstroError::new(
                    ErrorCode::InvalidRequest,
                    format!("invalid built-in exclude glob {pat}: {e}"),
                )
            })?;
        }
        for pat in user_globs {
            ob.add(&format!("!{pat}")).map_err(|e| {
                AstroError::new(
                    ErrorCode::InvalidRequest,
                    format!("invalid exclude glob {pat}: {e}"),
                )
            })?;
        }
        let inner = ob
            .build()
            .map_err(|e| AstroError::new(ErrorCode::InvalidRequest, format!("glob build: {e}")))?;
        Ok(Self { inner })
    }

    pub(crate) fn is_match(&self, path: &str) -> bool {
        // `!pattern` で登録 → match すると Match::Ignore が返る (= 除外対象)
        self.inner.matched(path, false).is_ignore()
    }
}

fn build_exclude_matcher(user_globs: &[String]) -> Result<CoChangeExclude> {
    CoChangeExclude::build(user_globs)
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

    // ---- blame mode ----

    /// hunk header の旧側 (start,count) を抽出する。COUNT 省略は 1、COUNT=0 は除外。
    #[test]
    fn parse_hunk_old_ranges_basic() {
        let diff = "diff --git a/foo b/foo\n@@ -10,3 +10,3 @@ ctx\n-x\n+x\n@@ -22 +25 @@ ctx\n-y\n+y\n@@ -100,0 +103,2 @@ ctx\n+a\n+b\n";
        let r = parse_hunk_old_ranges(diff);
        assert_eq!(r, vec![(10u64, 3u64), (22u64, 1u64)]);
    }

    /// blame モード: 起点ファイルの過去変更行に関わるコミットで他ファイルが
    /// 一緒に変わっていれば共起ペアとして検出される。
    #[test]
    fn analyze_cochange_blame_detects_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 起点ファイル a.rs を 3 回コミット。各コミットで b.rs も同時に変更し、
        // c.rs はバラバラに 1 回だけ変更。
        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        commit_files(repo, &[("c.rs", "// solo")], "solo c");
        // HEAD で a.rs を再修正 (起点になる差分を作る)
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = opts_with(|o| {
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();

        // a.rs↔b.rs が検出されるはず (a.rs の以前の編集コミットで b.rs も変わっている)
        let has_pair = result
            .entries
            .iter()
            .any(|e| e.file_a == "a.rs" && e.file_b == "b.rs");
        assert!(
            has_pair,
            "expected a.rs↔b.rs pair, got: {:?}",
            result.entries
        );
        // c.rs は a.rs の blame コミット集合に含まれないので出ない
        assert!(
            result.entries.iter().all(|e| e.file_b != "c.rs"),
            "c.rs should not appear, got: {:?}",
            result.entries
        );
    }

    /// 起点ファイル自身は候補に出ない
    #[test]
    fn analyze_cochange_blame_excludes_source_files_from_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("b.rs", &format!("fn b() {{ {i} }}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = opts_with(|o| {
            o.blame = true;
            o.source_files = vec!["a.rs".to_string(), "b.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();

        // 起点が a.rs / b.rs で、両者ともお互いを候補にしてはならない
        for e in &result.entries {
            assert!(
                !(e.file_b == "a.rs" || e.file_b == "b.rs"),
                "source file appeared as candidate: {e:?}"
            );
        }
    }

    /// 純粋追加 hunk (旧 count=0) しかないファイルは blame 対象がなく、
    /// 起点に紐づく blame コミット集合は空になり entries も空になる。
    #[test]
    fn analyze_cochange_blame_pure_addition_yields_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        // 何かしら 1 コミット作って HEAD~1 を解決可能にする
        commit_files(repo, &[("seed.rs", "// seed")], "seed");
        // 新規ファイル a.rs を追加 (= base に存在しない、純粋追加)
        commit_files(repo, &[("a.rs", "fn a() {}\n")], "add a");

        let opts = opts_with(|o| {
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
        assert!(
            result.entries.is_empty() && result.commits_analyzed == 0,
            "pure-addition source should yield empty result, got: {result:?}"
        );
    }

    /// 既定除外 glob (vendor/ 等) に該当する候補は出ない
    #[test]
    fn analyze_cochange_blame_default_excludes_vendor() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_repo(repo);

        for i in 0..3 {
            commit_files(
                repo,
                &[
                    ("a.rs", &format!("fn a() {{ {i} }}\n")),
                    ("vendor/lib.php", &format!("// {i}\n")),
                ],
                &format!("pair {i}"),
            );
        }
        commit_files(repo, &[("a.rs", "fn a() { 99 }\n")], "edit a");

        let opts = opts_with(|o| {
            o.blame = true;
            o.source_files = vec!["a.rs".to_string()];
            o.base = Some("HEAD~1".to_string());
            o.min_confidence = 0.0;
            o.min_samples = 1;
            o.skip_deleted_files = false;
            o.bounded_by_merge_base = false;
        });
        let result = analyze_cochange_blame(repo.to_str().unwrap(), &opts).unwrap();
        assert!(
            result
                .entries
                .iter()
                .all(|e| !e.file_b.starts_with("vendor/")),
            "vendor/ should be excluded by default, got: {:?}",
            result.entries
        );
    }
}
