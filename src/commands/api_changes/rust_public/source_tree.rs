//! working tree / base revision を透過する Rust source I/O。

use super::*;

/// Rust crate のソースツリーをどこから読むかを表す抽象化。
///
/// `Worktree` は `std::fs` 経由で working tree を直接読み、`Base { blobs }` は base リビジョンを
/// 検出全体で共有する常駐 `git cat-file --batch` の読み手 (`git show <rev>:<path>` と同じ内容) と
/// `git ls-tree <rev>` で読む。`read_rust_module_source` / `collect_rust_rs_files` /
/// `RustReexportCache` の API に渡して I/O 差分を吸収する (リファクタ Step 1: I/O 抽象化、
/// 別 Issue `2026-06-06-refactor-rust-private-module-helpers-with-source-tree-enum.md` 対応)。
#[derive(Clone, Copy, Debug)]
pub(crate) enum RustSourceTree<'a> {
    Worktree,
    Base {
        blobs: &'a crate::commands::git_input::GitBlobBatch,
    },
}

/// `crate_root_rel`/src/`module_rel` を `source` 経由で読む。Worktree なら `std::fs::read`、
/// Base なら `<rev>:<crate_root_rel>/src/<module_rel>` の blob。失敗時は `None`。
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
        RustSourceTree::Base { blobs } => {
            let full_rel = crate_root_rel.join("src").join(module_rel);
            blobs.read(full_rel.to_str()?)
        }
    }
}

/// `manifest_rel` (dir 相対) の manifest (`Cargo.toml`) を `source` から読む。`Ok(None)` は不在。
///
/// base 側は base の snapshot だけで決め、作業ツリーで補わない。blob を読めなかったときは
/// `git ls-tree` で base の tree にその path があるかを確かめ、無ければ不在、あれば
/// (blob でない・上限超過・読み出しの失敗) 読めないとする。作業ツリー側も、無いのか
/// 読めないのかを分ける (不在なら祖先の探索を続けられるが、読めない manifest は判定を止める)。
pub(crate) fn read_rust_manifest(
    source: RustSourceTree<'_>,
    dir: &str,
    manifest_rel: &std::path::Path,
) -> Result<Option<Vec<u8>>, crate::engine::rust_edition::ManifestUnreadable> {
    use crate::engine::rust_edition::ManifestUnreadable;
    match source {
        RustSourceTree::Worktree => {
            let canonical_dir = std::fs::canonicalize(dir).map_err(|_| ManifestUnreadable)?;
            let full = canonical_dir.join(manifest_rel);
            match std::fs::symlink_metadata(&full) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(_) => Err(ManifestUnreadable),
                Ok(_) => crate::engine::bounded_read::read_input_file(&full)
                    .map(Some)
                    .ok_or(ManifestUnreadable),
            }
        }
        RustSourceTree::Base { blobs } => {
            let path = crate::git_support::normalize_workspace_separators(
                manifest_rel.to_str().ok_or(ManifestUnreadable)?,
            );
            if let Some(blob) = blobs.read_limited(&path, crate::commands::MAX_INPUT_SIZE) {
                return Ok(Some(blob));
            }
            let rev = blobs.rev();
            if validate_git_revision(rev, "--base").is_err()
                || validate_git_revision(&path, "diff file path").is_err()
            {
                return Err(ManifestUnreadable);
            }
            // path に glob 文字が含まれても別の path に一致させない。
            let out = crate::git_support::git_command_for_dir(dir)
                .args(["--literal-pathspecs", "ls-tree", "-z", rev, "--", &path])
                .current_dir(dir)
                .output()
                .map_err(|_| ManifestUnreadable)?;
            if !out.status.success() {
                return Err(ManifestUnreadable);
            }
            if out.stdout.is_empty() {
                Ok(None)
            } else {
                Err(ManifestUnreadable)
            }
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
        RustSourceTree::Base { blobs } => {
            let rev = blobs.rev();
            let src_str = src_root_rel.to_str()?;
            if validate_git_revision(rev, "--base").is_err()
                || validate_git_revision(src_str, "diff file path").is_err()
            {
                return None;
            }
            // -z: 区切りを NUL にして quoting を無効化する。改行区切りの出力は非 ASCII
            // (core.quotepath) や `"` / `\` / TAB を含むパスを `"...\343..."` とクォートし、
            // 末尾の `"` で `.rs` 判定から落ちる。crate が非 ASCII 名のディレクトリ配下にあると
            // base 側の全ファイルが消えて `pub use` の辺を 1 本も集められず、再エクスポートで
            // 公開していた API の削除が api.rm / rm_dead のどちらにも出なかった。
            let out = crate::git_support::git_command_for_dir(dir)
                .args(["ls-tree", "-r", "-z", "--name-only", rev, "--", src_str])
                .current_dir(dir)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let text = std::str::from_utf8(&out.stdout).ok()?;
            Some(
                text.split('\0')
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
            RustSourceTree::Base { blobs } => Some(blobs.rev().to_string()),
        };
        Self {
            rev,
            crate_root_rel,
        }
    }
}
