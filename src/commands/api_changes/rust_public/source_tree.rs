//! working tree / base revision を透過する Rust source I/O。

use super::*;

/// Rust crate のソースツリーをどこから読むかを表す抽象化。
///
/// `Worktree` は `std::fs` 経由で working tree を直接読み、`Base { rev }` は `git show <rev>:<path>` /
/// `git ls-tree <rev>` 経由で base リビジョンを読む。`read_rust_module_source` / `collect_rust_rs_files` /
/// `RustReexportCache` の API に渡して I/O 差分を吸収する (リファクタ Step 1: I/O 抽象化、
/// 別 Issue `2026-06-06-refactor-rust-private-module-helpers-with-source-tree-enum.md` 対応)。
#[derive(Clone, Copy, Debug)]
pub(crate) enum RustSourceTree<'a> {
    Worktree,
    Base { rev: &'a str },
}

/// `crate_root_rel`/src/`module_rel` を `source` 経由で読む。Worktree なら `std::fs::read`、
/// Base なら `git show <rev>:<crate_root_rel>/src/<module_rel>`。失敗時は `None`。
pub(crate) fn read_rust_module_source(
    source: RustSourceTree<'_>,
    dir: &str,
    crate_root_rel: &std::path::Path,
    module_rel: &std::path::Path,
) -> Option<Vec<u8>> {
    match source {
        RustSourceTree::Worktree => {
            let canonical_dir = std::fs::canonicalize(dir).ok()?;
            let full = canonical_dir
                .join(crate_root_rel)
                .join("src")
                .join(module_rel);
            std::fs::read(full).ok()
        }
        RustSourceTree::Base { rev } => {
            let full_rel = crate_root_rel.join("src").join(module_rel);
            git_show_blob(dir, rev, full_rel.to_str()?)
        }
    }
}

/// `src_root_rel` 配下の `.rs` ファイル列 (repo 相対) を `source` 経由で取得する。
/// Worktree なら `ignore::WalkBuilder`、Base なら `git ls-tree -r --name-only`。
pub(crate) fn collect_rust_rs_files(
    source: RustSourceTree<'_>,
    dir: &str,
    src_root_rel: &std::path::Path,
) -> Option<Vec<std::path::PathBuf>> {
    match source {
        RustSourceTree::Worktree => {
            use ignore::WalkBuilder;
            let canonical_dir = std::fs::canonicalize(dir).ok()?;
            let src_full = canonical_dir.join(src_root_rel);
            if !src_full.is_dir() {
                return None;
            }
            let mut files = Vec::new();
            for entry in WalkBuilder::new(&src_full).hidden(false).build().flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                let rel = match path.strip_prefix(&canonical_dir) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                files.push(rel);
            }
            Some(files)
        }
        RustSourceTree::Base { rev } => {
            let src_str = src_root_rel.to_str()?;
            if validate_git_revision(rev, "--base").is_err()
                || validate_git_revision(src_str, "diff file path").is_err()
            {
                return None;
            }
            let out = std::process::Command::new("git")
                .args(["ls-tree", "-r", "--name-only", rev, "--", src_str])
                .current_dir(dir)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let text = std::str::from_utf8(&out.stdout).ok()?;
            Some(
                text.lines()
                    .filter(|l| l.ends_with(".rs"))
                    .map(std::path::PathBuf::from)
                    .collect(),
            )
        }
    }
}

/// 統合 cache キー (リファクタ Step 2: cache 統合)。`rev = None` で working tree、
/// `rev = Some(<rev>)` で base リビジョンを表す。型で意図を明確化することで、
/// `(Option<String>, PathBuf)` の生 tuple よりも事故りにくい。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct RustSourceTreeKey {
    rev: Option<String>,
    crate_root_rel: std::path::PathBuf,
}

impl RustSourceTreeKey {
    pub(super) fn from_source(
        source: RustSourceTree<'_>,
        crate_root_rel: std::path::PathBuf,
    ) -> Self {
        let rev = match source {
            RustSourceTree::Worktree => None,
            RustSourceTree::Base { rev } => Some(rev.to_string()),
        };
        Self {
            rev,
            crate_root_rel,
        }
    }
}
