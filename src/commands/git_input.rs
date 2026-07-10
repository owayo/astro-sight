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
        // core.quotepath=off: 非 ASCII ファイル名の 8 進クォート (`"\346..."`) を無効化し、
        // 後段のパス照合・blame pathspec が生の UTF-8 名で一致するようにする。
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "core.quotepath=off",
                "diff",
                "--name-only",
                base_rev,
                "HEAD",
            ])
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
    // core.quotepath=off: 非 ASCII ファイル名が 8 進クォート (`--- "a/\346..."`) で出力されると
    // parse_unified_diff がヘッダを認識できず、hunk が直前ファイルへ誤帰属する。
    let mut args = vec![
        "-c".to_string(),
        "core.quotepath=off".to_string(),
        "diff".to_string(),
        "--find-renames".to_string(),
    ];
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

/// unstaged 解析用に未追跡の新規ソースファイルを diff に合成する (rename-aware)。
/// git diff の生成責務に閉じることで、これを経由する context / impact / review / dead-code が
/// 同一の変更範囲を見る。
/// - `git ls-files --others --exclude-standard -z` で .gitignore 除外済みの未追跡を列挙
/// - diff 内の削除ファイルと未追跡を high-confidence な rename と判定できれば、削除 block を
///   除去して `Modified{old_path,new_path}` diff に正規化する (A1)。これにより commit 済み
///   rename (git --find-renames で Modified 処理) と未コミット rename の api_changes が一致する。
/// - 残りの未追跡は従来通り「全行追加の新規ファイル」として合成する
/// - 100MB 上限 (MAX_INPUT_SIZE) を超えたら打ち切り (既存 git diff と合算)
///
/// 未追跡取得失敗や個別ファイル読込失敗は fail-open (合成せず従来通り) とし、解析本体を止めない。
fn append_untracked_added_diffs(dir: &str, diff: &mut String) {
    let untracked = collect_untracked_source_files(dir);
    if untracked.is_empty() {
        return;
    }

    // rename 検出: diff 内の削除候補と未追跡を high-confidence pair に対応付ける。
    let deleted = collect_deleted_rename_candidates(diff);
    let pairs = pair_untracked_renames(&deleted, &untracked);

    // 採用 pair をサイズ予測し、上限内のみ反映する。上限超過 pair は削除 block も Modified も
    // 触らず元の Deleted+Added を残す (削除 block 除去後に Modified 合成途中で打ち切ると
    // api.rm が消える fail-open になるため、予測してから一括反映する)。synth=None は内容同一
    // rename で、削除 block 除去のみ行い Modified を合成しない (commit 済みの hunkless 100%
    // rename と一致させ dead-code / cochange の乖離を防ぐ)。
    let mut projected = diff.len();
    let mut planned: Vec<(&UntrackedRenamePair, Option<String>)> = Vec::new();
    for pair in &pairs {
        let synth = synthesize_modified_file_diff(
            &pair.old_path,
            &pair.new_path,
            &pair.old_source,
            &pair.new_source,
        );
        let synth_len = synth.as_ref().map_or(0, String::len);
        let next = projected
            .saturating_sub(pair.block_text.len())
            .saturating_add(synth_len);
        if next > MAX_INPUT_SIZE {
            continue;
        }
        projected = next;
        planned.push((pair, synth));
    }

    // 採用 pair の削除 block を除去し、内容差分ありの場合のみ Modified diff を追加する。
    // block_text の find が成功した時だけ処理し、失敗時は Deleted を残す (Deleted と Modified の
    // 二重入力を避ける fail-closed)。
    let paired_untracked: std::collections::HashSet<&str> =
        planned.iter().map(|(p, _)| p.new_path.as_str()).collect();
    for (pair, synth) in &planned {
        if let Some(pos) = diff.find(&pair.block_text) {
            diff.replace_range(pos..pos + pair.block_text.len(), "");
            if let Some(s) = synth {
                diff.push_str(s);
            }
        }
    }

    // 未 pair の未追跡は従来通り「全行追加の新規ファイル」として合成する。
    let mut total = diff.len();
    for u in &untracked {
        if paired_untracked.contains(u.rel_path.as_str()) {
            continue;
        }
        let synth = synthesize_added_file_diff(&u.rel_path, &u.content);
        total = total.saturating_add(synth.len());
        if total > MAX_INPUT_SIZE {
            return;
        }
        diff.push_str(&synth);
    }
}

/// 未追跡な新規ソースファイル (rename 先候補)。
struct UntrackedSourceFile {
    rel_path: String,
    content: String,
}

/// 未追跡ファイルのうち言語判定できる regular file を読み込んで返す。symlink / 巨大 /
/// 非 UTF-8 / 空 / NUL・改行を含むパスは除外 (従来の合成フィルタと同一基準)。
fn collect_untracked_source_files(dir: &str) -> Vec<UntrackedSourceFile> {
    let untracked = match list_untracked_files(dir) {
        Ok(paths) => paths,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for rel_path in untracked {
        // パスにNUL/改行を含むものは合成 diff を壊すため除外 (ls-files -z 由来では稀)。
        if rel_path.contains('\n') || rel_path.contains('\r') {
            continue;
        }
        // 言語判定できないファイル (バイナリ / 非ソース) は impact 解析対象外なので含めない。
        let utf8_path = camino::Utf8Path::new(&rel_path);
        if crate::language::LangId::from_path(utf8_path).is_err() {
            continue;
        }
        let full = std::path::Path::new(dir).join(&rel_path);
        // symlink_metadata でリンク自身を見る。symlink は追わない (パス境界)。regular file
        // 以外 (symlink / dir / fifo 等) と巨大ファイルは skip。
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !meta.file_type().is_file() || meta.len() as usize > MAX_INPUT_SIZE {
            continue;
        }
        // 非 UTF-8 / 読込不可 / 空は skip (fail-open)。
        let Ok(content) = std::fs::read_to_string(&full) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        out.push(UntrackedSourceFile { rel_path, content });
    }
    out
}

/// rename ペア検出用の削除ファイル候補。元 diff 内の block 原文を保持し、ペア成立時に
/// その block を diff から除去する。
struct DeletedRenameCandidate {
    old_path: String,
    old_source: Vec<u8>,
    block_text: String,
}

/// diff を `diff --git` 行単位の file block に分割する (各 block は次の `diff --git` または
/// 末尾までの原文)。
fn split_into_file_blocks(diff: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

/// diff 内の削除ファイル (new_path == /dev/null) を候補として収集する。各 block を
/// parse_unified_diff にかけ、削除ファイルの旧ソース (deleted_old_source) を得る。
fn collect_deleted_rename_candidates(diff: &str) -> Vec<DeletedRenameCandidate> {
    let mut out = Vec::new();
    for block in split_into_file_blocks(diff) {
        let files = crate::engine::diff::parse_unified_diff(&block);
        // 1 block = 1 ファイル diff が前提。複数や 0 件の異常 block は安全側で skip。
        if files.len() != 1 {
            continue;
        }
        let df = &files[0];
        if df.new_path == "/dev/null"
            && let Some(src) = &df.deleted_old_source
            && !src.is_empty()
        {
            out.push(DeletedRenameCandidate {
                old_path: df.old_path.clone(),
                old_source: src.clone(),
                block_text: block,
            });
        }
    }
    out
}

/// high-confidence rename ペア。
struct UntrackedRenamePair {
    old_path: String,
    new_path: String,
    old_source: Vec<u8>,
    new_source: String,
    block_text: String,
}

/// 削除候補と未追跡を内容類似度でペアリングする。構造 gate (同一言語・サイズ比・exported
/// symbol 集合一致) を通る (deleted, untracked) エッジのうち、**両端とも degree 1** の
/// 一意対応のみ採用する (ambiguous は fail-closed で pair しない)。
fn pair_untracked_renames(
    deleted: &[DeletedRenameCandidate],
    untracked: &[UntrackedSourceFile],
) -> Vec<UntrackedRenamePair> {
    // 構造 gate を通るエッジを収集。
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (di, d) in deleted.iter().enumerate() {
        for (ui, u) in untracked.iter().enumerate() {
            if rename_gate_passes(d, u) {
                edges.push((di, ui));
            }
        }
    }
    // 両端の degree を数え、1:1 対応のエッジのみ採用 (ambiguous は fail-closed で除外)。
    let mut d_degree = vec![0usize; deleted.len()];
    let mut u_degree = vec![0usize; untracked.len()];
    for &(di, ui) in &edges {
        d_degree[di] += 1;
        u_degree[ui] += 1;
    }
    let mut pairs = Vec::new();
    for &(di, ui) in &edges {
        if d_degree[di] == 1 && u_degree[ui] == 1 {
            let d = &deleted[di];
            let u = &untracked[ui];
            pairs.push(UntrackedRenamePair {
                old_path: d.old_path.clone(),
                new_path: u.rel_path.clone(),
                old_source: d.old_source.clone(),
                new_source: u.content.clone(),
                block_text: d.block_text.clone(),
            });
        }
    }
    pairs
}

/// 削除候補と未追跡が high-confidence rename pair か判定する構造 gate。
/// - 同一言語 (拡張子由来)
/// - サイズ比 2 倍以内
/// - exported symbol 集合 ((name,kind,signature)) が一致し非空
fn rename_gate_passes(d: &DeletedRenameCandidate, u: &UntrackedSourceFile) -> bool {
    let d_lang = crate::language::LangId::from_path(camino::Utf8Path::new(&d.old_path)).ok();
    let u_lang = crate::language::LangId::from_path(camino::Utf8Path::new(&u.rel_path)).ok();
    if d_lang.is_none() || d_lang != u_lang {
        return false;
    }
    if !size_ratio_ok(d.old_source.len(), u.content.len()) {
        return false;
    }
    let d_syms = crate::commands::api_changes::extract_exported_symbols_from_source(
        &d.old_path,
        &d.old_source,
    );
    let u_syms = crate::commands::api_changes::extract_exported_symbols_from_source(
        &u.rel_path,
        u.content.as_bytes(),
    );
    exported_symbols_match(&d_syms, &u_syms)
}

/// 2 ファイルのサイズ比が 2 倍以内か (どちらも非空)。
fn size_ratio_ok(a: usize, b: usize) -> bool {
    if a == 0 || b == 0 {
        return false;
    }
    let (min, max) = if a < b { (a, b) } else { (b, a) };
    max <= min.saturating_mul(2)
}

/// exported symbol 集合 ((name,kind,signature)) が完全一致し非空か。内容ほぼ同一の rename を
/// 高信頼で検出する。signature が変わると集合が一致せず gate を通らない (fail-closed:
/// pair しないと Deleted+Added のまま = api.rm が残る保守側に倒れる)。
fn exported_symbols_match(
    a: &Option<Vec<(String, String, String)>>,
    b: &Option<Vec<(String, String, String)>>,
) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return false;
    }
    let sa: std::collections::HashSet<&(String, String, String)> = a.iter().collect();
    let sb: std::collections::HashSet<&(String, String, String)> = b.iter().collect();
    // 重複シンボル (multiset、overload 等で同名同 signature が複数) は high-confidence rename の
    // 根拠として曖昧なので落とす (set 化で要素数が減る = 重複あり)。
    if sa.len() != a.len() || sb.len() != b.len() {
        return false;
    }
    sa == sb
}

/// rename を `Modified{old_path,new_path}` diff として合成する。共通 prefix/suffix を
/// context、中間を `-`/`+` にした 1 hunk を作る (full replacement を避けて impact の過剰
/// 検出を抑える)。内容同一 (100% rename) は commit 済みの hunkless rename (git -M) と同様、
/// 解析対象 hunk を作らず `None` を返す。削除 block は呼び出し側で除去済みなので、Modified を
/// 合成しないことで api_changes だけでなく dead-code / cochange も commit 済みと一致する。
fn synthesize_modified_file_diff(
    old_path: &str,
    new_path: &str,
    old_source: &[u8],
    new_source: &str,
) -> Option<String> {
    let old_text = String::from_utf8_lossy(old_source);
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_source.lines().collect();

    if old_lines == new_lines {
        return None;
    }

    let mut out = String::new();
    out.push_str(&format!("diff --git a/{old_path} b/{new_path}\n"));
    out.push_str(&format!("rename from {old_path}\n"));
    out.push_str(&format!("rename to {new_path}\n"));
    out.push_str(&format!("--- a/{old_path}\n"));
    out.push_str(&format!("+++ b/{new_path}\n"));
    out.push_str(&simple_line_diff_hunk(&old_lines, &new_lines));
    Some(out)
}

/// 共通 prefix/suffix を context、中間を `-`/`+` にした 1 hunk を生成する (簡易 line diff)。
/// 内容同一なら全行 context (変更なし) となる。
fn simple_line_diff_hunk(old_lines: &[&str], new_lines: &[&str]) -> String {
    let total_old = old_lines.len();
    let total_new = new_lines.len();
    let mut prefix = 0;
    while prefix < total_old && prefix < total_new && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < total_old - prefix
        && suffix < total_new - prefix
        && old_lines[total_old - 1 - suffix] == new_lines[total_new - 1 - suffix]
    {
        suffix += 1;
    }
    let mut body = String::new();
    body.push_str(&format!("@@ -1,{total_old} +1,{total_new} @@\n"));
    for line in old_lines.iter().take(prefix) {
        body.push(' ');
        body.push_str(line);
        body.push('\n');
    }
    for line in old_lines.iter().take(total_old - suffix).skip(prefix) {
        body.push('-');
        body.push_str(line);
        body.push('\n');
    }
    for line in new_lines.iter().take(total_new - suffix).skip(prefix) {
        body.push('+');
        body.push_str(line);
        body.push('\n');
    }
    for line in new_lines.iter().skip(total_new - suffix) {
        body.push(' ');
        body.push_str(line);
        body.push('\n');
    }
    body
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_into_file_blocks_splits_by_diff_git_header() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/y b/y\n--- a/y\n+++ b/y\n@@ -1 +1 @@\n-c\n+d\n";
        let blocks = split_into_file_blocks(diff);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with("diff --git a/x"));
        assert!(blocks[1].starts_with("diff --git a/y"));
    }

    #[test]
    fn size_ratio_ok_within_and_beyond_limits() {
        assert!(size_ratio_ok(100, 150));
        assert!(size_ratio_ok(100, 200));
        assert!(!size_ratio_ok(100, 201));
        assert!(!size_ratio_ok(0, 100));
        assert!(!size_ratio_ok(100, 0));
    }

    #[test]
    fn exported_symbols_match_requires_nonempty_set_equality() {
        let a = Some(vec![(
            "foo".to_string(),
            "function".to_string(),
            "sig1".to_string(),
        )]);
        let b = Some(vec![(
            "foo".to_string(),
            "function".to_string(),
            "sig1".to_string(),
        )]);
        assert!(exported_symbols_match(&a, &b));
        let c = Some(vec![(
            "foo".to_string(),
            "function".to_string(),
            "sig2".to_string(),
        )]);
        assert!(!exported_symbols_match(&a, &c), "signature 違いは不一致");
        let empty: Option<Vec<(String, String, String)>> = Some(vec![]);
        assert!(
            !exported_symbols_match(&empty, &empty),
            "空集合同士は弱証拠で不一致"
        );
        assert!(!exported_symbols_match(&None, &b), "None は不一致");
    }

    #[test]
    fn simple_line_diff_hunk_identical_content_is_all_context() {
        let lines = vec!["fn foo() {", "    1", "}"];
        let hunk = simple_line_diff_hunk(&lines, &lines);
        assert!(hunk.contains("@@ -1,3 +1,3 @@"));
        assert!(!hunk.contains("\n-"), "内容同一なら削除行なし");
        assert!(!hunk.contains("\n+"), "内容同一なら追加行なし");
    }

    #[test]
    fn simple_line_diff_hunk_change_emits_add_remove_with_context() {
        let old = vec!["fn foo() {", "    1", "}"];
        let new = vec!["fn foo() {", "    2", "}"];
        let hunk = simple_line_diff_hunk(&old, &new);
        assert!(hunk.contains("\n-    1\n"));
        assert!(hunk.contains("\n+    2\n"));
        assert!(hunk.contains("\n fn foo() {\n"), "共通 prefix は context");
        assert!(hunk.contains("\n }\n"), "共通 suffix は context");
    }

    #[test]
    fn pair_untracked_renames_identical_rust_pairs() {
        let src = "pub fn foo(name: &str) -> String {\n    name.to_uppercase()\n}\n";
        let deleted = vec![DeletedRenameCandidate {
            old_path: "src/a.rs".to_string(),
            old_source: src.as_bytes().to_vec(),
            block_text: String::new(),
        }];
        let untracked = vec![UntrackedSourceFile {
            rel_path: "src/b.rs".to_string(),
            content: src.to_string(),
        }];
        let pairs = pair_untracked_renames(&deleted, &untracked);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].old_path, "src/a.rs");
        assert_eq!(pairs[0].new_path, "src/b.rs");
    }

    #[test]
    fn pair_untracked_renames_signature_change_not_paired() {
        let old_src = "pub fn foo(name: &str) -> String {\n    name.to_uppercase()\n}\n";
        let new_src = "pub fn foo(name: &str, x: u32) -> String {\n    name.to_uppercase()\n}\n";
        let deleted = vec![DeletedRenameCandidate {
            old_path: "src/a.rs".to_string(),
            old_source: old_src.as_bytes().to_vec(),
            block_text: String::new(),
        }];
        let untracked = vec![UntrackedSourceFile {
            rel_path: "src/b.rs".to_string(),
            content: new_src.to_string(),
        }];
        let pairs = pair_untracked_renames(&deleted, &untracked);
        assert!(
            pairs.is_empty(),
            "signature 違いは pair しない (fail-closed)"
        );
    }

    #[test]
    fn pair_untracked_renames_python_signature_change_not_paired() {
        let old_src = "def foo(name):\n    return name.upper()\n";
        let new_src = "def foo(name, extra):\n    return name.upper()\n";
        let deleted = vec![DeletedRenameCandidate {
            old_path: "src/a.py".to_string(),
            old_source: old_src.as_bytes().to_vec(),
            block_text: String::new(),
        }];
        let untracked = vec![UntrackedSourceFile {
            rel_path: "src/b.py".to_string(),
            content: new_src.to_string(),
        }];
        let pairs = pair_untracked_renames(&deleted, &untracked);
        assert!(pairs.is_empty(), "Python signature 違いも pair しない");
    }

    #[test]
    fn pair_untracked_renames_different_language_not_paired() {
        let deleted = vec![DeletedRenameCandidate {
            old_path: "src/a.rs".to_string(),
            old_source: b"pub fn foo() {}\n".to_vec(),
            block_text: String::new(),
        }];
        let untracked = vec![UntrackedSourceFile {
            rel_path: "src/b.py".to_string(),
            content: "def foo():\n    pass\n".to_string(),
        }];
        let pairs = pair_untracked_renames(&deleted, &untracked);
        assert!(pairs.is_empty(), "異言語は pair しない");
    }

    #[test]
    fn pair_untracked_renames_ambiguous_candidates_not_paired() {
        // 1 deleted に同一シンボルの 2 untracked がマッチ → degree>1 → fail-closed
        let src = "pub fn foo(name: &str) -> String {\n    name.to_uppercase()\n}\n";
        let deleted = vec![DeletedRenameCandidate {
            old_path: "src/a.rs".to_string(),
            old_source: src.as_bytes().to_vec(),
            block_text: String::new(),
        }];
        let untracked = vec![
            UntrackedSourceFile {
                rel_path: "src/b.rs".to_string(),
                content: src.to_string(),
            },
            UntrackedSourceFile {
                rel_path: "src/c.rs".to_string(),
                content: src.to_string(),
            },
        ];
        let pairs = pair_untracked_renames(&deleted, &untracked);
        assert!(
            pairs.is_empty(),
            "ambiguous (複数候補) は pair しない (fail-closed)"
        );
    }

    #[test]
    fn synthesize_modified_file_diff_identical_content_returns_none() {
        // 内容同一 (100% rename) は hunkless = None。commit 済みの git -M と一致し、
        // dead-code / cochange の乖離を防ぐ。
        let src = b"pub fn foo() {}\n";
        let result = synthesize_modified_file_diff("a.rs", "b.rs", src, "pub fn foo() {}\n");
        assert!(result.is_none());
    }

    #[test]
    fn synthesize_modified_file_diff_changed_content_returns_some_hunk() {
        let old = b"pub fn foo() {\n    1\n}\n";
        let diff = synthesize_modified_file_diff("a.rs", "b.rs", old, "pub fn foo() {\n    2\n}\n")
            .expect("内容差分ありは Some");
        assert!(diff.contains("--- a/a.rs"));
        assert!(diff.contains("+++ b/b.rs"));
        assert!(diff.contains("\n-    1\n"));
        assert!(diff.contains("\n+    2\n"));
    }

    #[test]
    fn exported_symbols_match_rejects_duplicate_symbols() {
        // overload 等で同名同 signature が複数 → 曖昧として落とす (high-confidence にしない)。
        let dup = Some(vec![
            ("foo".to_string(), "function".to_string(), "sig".to_string()),
            ("foo".to_string(), "function".to_string(), "sig".to_string()),
        ]);
        assert!(!exported_symbols_match(&dup, &dup));
    }
}
