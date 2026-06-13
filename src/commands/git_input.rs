use anyhow::{Result, anyhow, bail};

use crate::error::{AstroError, ErrorCode};
use crate::models::skip::SkipInfo;

use super::common::{MAX_INPUT_SIZE, read_bytes_limited_and_drain, read_paths_file_limited};

/// `resolve_blame_source_files` の結果。起点ファイルが解決できたか、
/// git 管理外 (かつ明示 `--paths` / `--paths-file` 無し) で skip かを型で表す。
pub enum BlameSourceResolution {
    Files(Vec<String>),
    Skipped(SkipInfo),
}

pub fn resolve_blame_source_files(
    dir: &str,
    git: bool,
    base: Option<&str>,
    paths: Option<&str>,
    paths_file: Option<&str>,
    user_exclude_globs: &[String],
) -> Result<BlameSourceResolution> {
    use std::collections::BTreeSet;

    let mut set: BTreeSet<String> = BTreeSet::new();

    if let Some(file_path) = paths_file {
        for path in read_paths_file_limited(file_path, MAX_INPUT_SIZE)? {
            set.insert(path);
        }
    }
    if let Some(s) = paths {
        for p in s.split(',') {
            let p = p.trim();
            if !p.is_empty() {
                set.insert(p.to_string());
            }
        }
    }
    if git {
        // git 管理外: 明示 --paths/--paths-file があればそれで続行、
        // 無ければ graceful skip (既存の明示優先の優先順位を維持)。
        if !is_git_work_tree(dir)? {
            if set.is_empty() {
                return Ok(BlameSourceResolution::Skipped(
                    SkipInfo::not_git_repository(),
                ));
            }
            return Ok(BlameSourceResolution::Files(set.into_iter().collect()));
        }
        let base_rev = base.unwrap_or("HEAD~1");
        validate_git_revision(base_rev, "base")?;
        let output = std::process::Command::new("git")
            .args(["diff", "--name-only", base_rev, "HEAD"])
            .current_dir(dir)
            .output()
            .map_err(|e| {
                anyhow::Error::from(crate::error::AstroError::new(
                    crate::error::ErrorCode::IoError,
                    format!("failed to run git diff: {e}"),
                ))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(crate::error::AstroError::new(
                crate::error::ErrorCode::IoError,
                format!("git diff failed: {stderr}"),
            ));
        }
        // `--git` 経由は自動収集なので生成物・ロック類を除外する。
        // 明示指定の `--paths` / `--paths-file` には適用しない。
        let matcher = crate::engine::cochange::CoChangeExclude::build(user_exclude_globs)?;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let p = line.trim();
            if p.is_empty() {
                continue;
            }
            if matcher.is_match(p) {
                continue;
            }
            set.insert(p.to_string());
        }
    }

    if set.is_empty() {
        anyhow::bail!(crate::error::AstroError::new(
            crate::error::ErrorCode::InvalidRequest,
            "blame mode requires source files: pass --git, --paths, or --paths-file".to_string(),
        ));
    }
    Ok(BlameSourceResolution::Files(set.into_iter().collect()))
}

/// `git diff` / `git show` に渡す revision を検証する。
/// 先頭が `-` の値は git がオプションとして解釈するため拒否する。
/// (例: `--output=/path` によるファイル書き込みを防ぐ)
pub(crate) fn validate_git_revision(rev: &str, arg_name: &str) -> Result<()> {
    if rev.is_empty() {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{arg_name} must not be empty"),
        ));
    }
    if rev.starts_with('-') {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{arg_name} must not start with '-': {rev}"),
        ));
    }
    // `\0` を含む値はプロセス引数として不正
    if rev.contains('\0') {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{arg_name} must not contain NUL"),
        ));
    }
    Ok(())
}

/// `--git` 入力解決の結果。diff が取れたか、git 管理外で skip かを型で表す。
pub(crate) enum GitDiffInput {
    Diff(String),
    Skipped(SkipInfo),
}

/// `dir` が git worktree 内かを `git rev-parse --is-inside-work-tree` で判定する。
///
/// 管理外 (`not a git repository`) / worktree 外 (`is-inside-work-tree=false`) は
/// `Ok(false)` (skip 対象)、壊れた repo / 権限不足 / git 実行不能などの本物の異常は
/// `Err` (従来どおり `exit 1`)。`git diff` の stderr 解析ではなく事前判定にすることで
/// worktree / submodule / bare repo に堅牢。`LC_ALL=C` で stderr 文言のロケール依存を
/// 排除し `"not a git repository"` のマッチを安定させる。
pub(crate) fn is_git_work_tree(dir: &str) -> Result<bool> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| {
            AstroError::new(ErrorCode::InvalidRequest, format!("Failed to run git: {e}"))
        })?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim() == "true");
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if stderr.contains("not a git repository") {
        return Ok(false);
    }

    // 壊れた repo / 権限など本物の異常は従来どおりエラー (fail-closed)。
    Err(AstroError::new(
        ErrorCode::InvalidRequest,
        format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )
    .into())
}

/// 経路A (context / impact / review / dead-code) の git diff 入力解決。
///
/// git worktree 内なら diff を取得し `Diff`、管理外なら `Skipped` を返す。
/// `base` 検証を worktree 判定より前に置くのは意図的: base が不正なら git 管理外
/// でも `exit 1` にする (入力契約違反を skip より優先)。
pub(crate) fn resolve_git_diff(dir: &str, base: &str, staged: bool) -> Result<GitDiffInput> {
    validate_git_revision(base, "--base")?;

    if !is_git_work_tree(dir)? {
        return Ok(GitDiffInput::Skipped(SkipInfo::not_git_repository()));
    }

    run_git_diff(dir, base, staged).map(GitDiffInput::Diff)
}

pub fn run_git_diff(dir: &str, base: &str, staged: bool) -> Result<String> {
    validate_git_revision(base, "--base")?;
    // git config `diff.renames` がユーザー環境で無効化されていても rename を検出できるよう、
    // 明示的に `--find-renames` を指定する。ファイル rename が api.rm/api.add の誤発報源に
    // なるため、astro-sight 側で強制しておく。
    let mut args = vec!["diff".to_string(), "--find-renames".to_string()];
    if staged {
        args.push("--cached".to_string());
    }
    args.push(base.to_string());

    let mut child = std::process::Command::new("git")
        .args(&args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            AstroError::new(ErrorCode::InvalidRequest, format!("Failed to run git: {e}"))
        })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture git diff stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture git diff stderr"))?;

    let stdout_handle = std::thread::spawn(move || {
        // 子プロセスのパイプは上限超過後も読み捨てて、wait() の詰まりを防ぐ。
        read_bytes_limited_and_drain(stdout, MAX_INPUT_SIZE, "git diff output")
    });
    let stderr_handle = std::thread::spawn(move || {
        read_bytes_limited_and_drain(stderr, MAX_INPUT_SIZE, "git diff stderr")
    });

    let status = child.wait()?;
    let stdout_bytes = stdout_handle
        .join()
        .map_err(|_| anyhow!("Failed to join git diff stdout reader"))??;
    let stderr_bytes = stderr_handle
        .join()
        .map_err(|_| anyhow!("Failed to join git diff stderr reader"))??;

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("git diff failed: {stderr}"),
        )
        .into());
    }

    let mut diff = String::from_utf8(stdout_bytes).map_err(|e| {
        AstroError::new(
            ErrorCode::InvalidRequest,
            format!("git diff output is not valid UTF-8: {e}"),
        )
    })?;

    // unstaged (`--git`、非 `--staged`) では未追跡の新規ソースファイルを「全行追加の
    // 新規ファイル」として diff に合成する。git diff は仕様上 untracked を出力しないため、
    // これを入れないと「同一作業で作成した未追跡 sibling への参照」が impact/context/review で
    // 「diff 外の未解決影響」と誤報される (ファイル分割リファクタで高頻度)。
    // staged / `--diff` / `--diff-file` 経路は対象外 (それぞれ明示された範囲を尊重する)。
    if !staged {
        append_untracked_added_diffs(dir, &mut diff);
    }
    Ok(diff)
}

/// unstaged 解析用に未追跡の新規ソースファイルを「全行追加の新規ファイル」diff として
/// `diff` 末尾に合成する。git diff の生成責務に閉じることで、これを経由する
/// context / impact / review / dead-code が同一の変更範囲を見る。
/// - `git ls-files --others --exclude-standard -z` で .gitignore 除外済みの未追跡を列挙
/// - 言語判定できる (= ソースとみなせる) ファイルのみ対象。バイナリ / 非ソースは合成しない
/// - 100MB 上限 (MAX_INPUT_SIZE) を超えたら打ち切り (既存 git diff と合算)
///
/// 未追跡取得失敗や個別ファイル読込失敗は fail-open (合成せず従来通り) とし、解析本体を止めない。
fn append_untracked_added_diffs(dir: &str, diff: &mut String) {
    let untracked = match list_untracked_files(dir) {
        Ok(paths) => paths,
        Err(_) => return,
    };
    let mut total = diff.len();
    for rel_path in untracked {
        // パスにNUL/改行を含むものは合成 diff を壊すため除外 (ls-files -z 由来では稀)。
        if rel_path.contains('\n') || rel_path.contains('\r') {
            continue;
        }
        // 言語判定できないファイル (バイナリ / 非ソース) は impact 解析対象外なので合成しない。
        let utf8_path = camino::Utf8Path::new(&rel_path);
        if crate::language::LangId::from_path(utf8_path).is_err() {
            continue;
        }
        let full = std::path::Path::new(dir).join(&rel_path);
        // symlink_metadata でリンク自身を見る。symlink は追わない (リンク先が外部の
        // ソースファイルでも内容を合成 diff に含めないためのパス境界)。regular file 以外
        // (symlink / dir / fifo 等) は skip。併せて巨大ファイルも読み込まず skip。
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !meta.file_type().is_file() || meta.len() as usize > MAX_INPUT_SIZE {
            continue;
        }
        // 非 UTF-8 / 読込不可は skip (fail-open)。
        let Ok(content) = std::fs::read_to_string(&full) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        let synth = synthesize_added_file_diff(&rel_path, &content);
        total = total.saturating_add(synth.len());
        if total > MAX_INPUT_SIZE {
            break;
        }
        diff.push_str(&synth);
    }
}

/// `git ls-files --others --exclude-standard -z` で未追跡ファイル (gitignore 除外済み) を列挙する。
/// `-z` で NUL 区切り・クォートなしにし、空白/非 ASCII を含むパスも正しく扱う。
/// stdout を NUL 区切りの byte slice 単位で UTF-8 検証し、非 UTF-8 パスは要素ごとに skip する
/// (from_utf8_lossy の置換文字混入を避ける)。
fn list_untracked_files(dir: &str) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .current_dir(dir)
        .output()
        .map_err(|e| {
            AstroError::new(ErrorCode::InvalidRequest, format!("Failed to run git: {e}"))
        })?;
    if !output.status.success() {
        // 取得失敗は fail-open: 未追跡を含めず従来通りの diff で続行する。
        return Ok(Vec::new());
    }
    Ok(output
        .stdout
        .split(|&b| b == 0)
        .filter(|seg| !seg.is_empty())
        .filter_map(|seg| std::str::from_utf8(seg).ok())
        .map(|s| s.to_string())
        .collect())
}

/// 1 ファイル分の「全行追加の新規ファイル」unified diff を生成する。
/// `parse_unified_diff` は `--- /dev/null` + `+++ b/<path>` + `@@ -0,0 +1,N @@` を新規ファイルと
/// 認識する。`diff --git` / `new file mode` 行はパーサが無視するが、実際の git 出力に合わせて付ける。
fn synthesize_added_file_diff(rel_path: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = String::new();
    out.push_str(&format!("diff --git a/{rel_path} b/{rel_path}\n"));
    out.push_str("new file mode 100644\n");
    out.push_str("--- /dev/null\n");
    out.push_str(&format!("+++ b/{rel_path}\n"));
    out.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
    for line in lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}
