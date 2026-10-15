//! staged の AST と参照検索を、同じ index tree の raw blob に揃える。

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::Result;

use crate::error::{AstroError, ErrorCode};
use crate::git_support::{INDEX_SNAPSHOT_MARKER, git_command_for_dir, isolated_git_command};
use crate::models::truncation::TruncationInfo;

/// 書き出す通常ファイルの件数と合計バイト数の上限。超えたら作らずに縮退する
/// (`PreparedSnapshot::OverBudget`)。tree の通常ファイルを全部書くので、全件で数える。
const MAX_FILES: usize = 50_000;
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// 並列 checkout の worker 数の上限。高負荷時にコア数まで増やしても速くならなかった
/// (1 万ファイルで workers=8 と自動がともに 3.6 秒、逐次は 6.7 秒)。
const MAX_CHECKOUT_WORKERS: usize = 8;

/// checkout で作業ツリー用の変換を止め、raw blob と同じバイト列を書かせる属性。
/// `$GIT_DIR/info/attributes` は最優先なので、tree 内の `.gitattributes` やマクロ属性より強い。
/// `working-tree-encoding` は true / false を受け付けない (git が die する) ため `!` で未指定に戻す。
const RAW_CHECKOUT_ATTRIBUTES: &str = "* -text -eol -crlf -filter -ident !working-tree-encoding\n";

fn failure(message: impl Into<String>) -> anyhow::Error {
    AstroError::new(ErrorCode::InvalidRequest, message).into()
}

fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut child = git_command_for_dir(dir)
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("Cannot read the staged snapshot command"))?;
    let bytes = crate::commands::common::read_bytes_limited_and_drain(
        stdout,
        64 * 1024 * 1024,
        "staged snapshot metadata",
    );
    if !child.wait()?.success() {
        return Err(failure("Cannot obtain the staged analysis snapshot"));
    }
    bytes
}

fn git_text(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_bytes(dir, args)?)?
        .trim_end_matches('\n')
        .to_owned())
}

fn safe_path(path: &str) -> bool {
    crate::engine::impact::is_safe_diff_path(path)
        && path
            .split('/')
            .all(|part| !part.eq_ignore_ascii_case(".git"))
}

/// tree のパス (生バイト列) を一時ディレクトリ内の相対パスへ変換する。
///
/// 解析対象かどうかで絞らない。非 UTF-8 のパスも Unix では生バイト列のまま書き出し、
/// 作業ツリーと同じ状態にする (扱えるかは解析側が決める)。`..`・`.git`・絶対パスは
/// 書き出し先を一時ディレクトリの外へ出し得るので拒否する。
fn tree_path(raw: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(raw))
    };
    // Windows などは非 UTF-8 のパスを表現できない。黙って省かず失敗させる。
    #[cfg(not(unix))]
    let path = PathBuf::from(
        std::str::from_utf8(raw).map_err(|_| failure("Non-UTF-8 staged source path"))?,
    );
    let normal = path.components().all(|component| {
        matches!(component, Component::Normal(part) if !part.eq_ignore_ascii_case(".git"))
    });
    if raw.is_empty() || !normal || !path.to_str().is_none_or(safe_path) {
        return Err(failure("Unsafe staged snapshot path"));
    }
    Ok(path)
}

/// checkout-index の stdin へ渡す生バイト列。
#[cfg(unix)]
fn tree_path_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

#[cfg(not(unix))]
fn tree_path_bytes(path: &Path) -> &[u8] {
    // `tree_path` は UTF-8 のパスしか作らない。
    path.to_str().unwrap_or_default().as_bytes()
}

/// `ls-tree -r -l -z` から読んだ書き出し対象。
#[derive(Default)]
struct TreeManifest {
    /// 通常ファイル (100644 / 100755)。
    files: Vec<PathBuf>,
    /// サブモジュールは親 index にコードを持たないため、空ディレクトリだけを作る。
    gitlinks: Vec<PathBuf>,
    total_bytes: u64,
}

fn read_manifest(listing: &[u8]) -> Result<TreeManifest> {
    let mut manifest = TreeManifest::default();
    for raw in listing.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        let tab = raw
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| failure("Invalid staged tree entry"))?;
        let metadata = std::str::from_utf8(&raw[..tab])
            .map_err(|_| failure("Invalid staged tree metadata"))?;
        let path = tree_path(&raw[tab + 1..])?;
        let fields: Vec<_> = metadata.split_whitespace().collect();
        let [mode, kind, _oid, size] = fields.as_slice() else {
            return Err(failure("Invalid staged tree metadata"));
        };
        match (*mode, *kind) {
            ("160000", "commit") => manifest.gitlinks.push(path),
            // 共通 walker はリンクを辿らない。外部・dangling を含め展開しない。
            ("120000", "blob") => {}
            ("100644" | "100755", "blob") => {
                let size: u64 = size.parse()?;
                manifest.total_bytes = manifest
                    .total_bytes
                    .checked_add(size)
                    .ok_or_else(|| failure("Staged snapshot size overflow"))?;
                manifest.files.push(path);
            }
            _ => return Err(failure("Unsupported staged tree entry")),
        }
    }
    Ok(manifest)
}

/// 親ディレクトリと gitlink の空ディレクトリを、浅い順に 1 つずつ作る。
///
/// 同じ論理パスは 1 度しか作らないので、既にあれば別の論理パスが FS 上で同じ実体になった
/// (大文字小文字や Unicode 正規化の別名) ことになる。同じディレクトリへ黙って混ぜると、
/// 解析が見るパスと diff のパスが食い違うため失敗させる。ファイル同士やファイルと
/// ディレクトリの衝突は、上書きしない checkout-index が非 0 で終わることで検出する。
fn create_directories(root: &Path, manifest: &TreeManifest) -> Result<()> {
    let mut directories = BTreeSet::new();
    let parents = manifest.files.iter().filter_map(|path| path.parent());
    for directory in parents.chain(manifest.gitlinks.iter().map(PathBuf::as_path)) {
        for ancestor in directory.ancestors() {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            directories.insert((ancestor.components().count(), ancestor.to_path_buf()));
        }
    }
    for (_, directory) in directories {
        match fs::create_dir(root.join(&directory)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(failure("Staged directory paths collide on this filesystem"));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// 通常ファイルを `git checkout-index` で書き出す。
///
/// 1 ファイルずつ `cat-file` に `<tree>:<path>` を問い合わせて書く旧実装は、パス解決と
/// 往復の待ちで 1 万ファイルに 30 秒以上かかっていた。checkout-index は並列に書けるうえ、
/// 上書きしないので衝突も検出できる。変換は `RAW_CHECKOUT_ATTRIBUTES` と `-c` で止める。
fn checkout_files(root: &Path, files: &[PathBuf]) -> Result<()> {
    let git_dir = root.join(".git");
    fs::create_dir_all(git_dir.join("info"))?;
    fs::write(
        git_dir.join("info").join("attributes"),
        RAW_CHECKOUT_ATTRIBUTES,
    )?;
    // パス一覧はファイル経由で stdin に渡す (パイプへの書き込みと子の終了待ちで詰まらせない)。
    let list = git_dir.join("astro-sight-checkout-paths");
    let mut writer = std::io::BufWriter::new(fs::File::create(&list)?);
    for path in files {
        writer.write_all(tree_path_bytes(path))?;
        writer.write_all(b"\0")?;
    }
    writer.into_inner().map_err(|error| error.into_error())?;
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_CHECKOUT_WORKERS);
    let status = isolated_git_command()
        .args(["-c", "core.autocrlf=false", "-c", "core.eol=lf"])
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.sparseCheckout=false",
        ])
        .arg("-c")
        .arg(format!("checkout.workers={workers}"))
        .args(["checkout-index", "--stdin", "-z"])
        .current_dir(root)
        .stdin(fs::File::open(&list)?)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    fs::remove_file(&list)?;
    if !status.success() {
        return Err(failure("Staged file paths collide or cannot be written"));
    }
    Ok(())
}

fn diff_contract(diff: &str) -> String {
    diff.lines()
        .filter(|line| !line.starts_with("index "))
        .map(|line| {
            if line.starts_with("@@ ") {
                line.find(" @@").map_or(line, |end| &line[..end + 3])
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 上限を超えたかの判定に使う予算。テストは小さい値を渡して縮退経路を確かめる。
#[derive(Clone, Copy)]
pub(super) struct SnapshotBudget {
    pub files: usize,
    pub bytes: u64,
}

impl SnapshotBudget {
    pub(super) const DEFAULT: Self = Self {
        files: MAX_FILES,
        bytes: MAX_TOTAL_BYTES,
    };
}

pub(super) enum PreparedSnapshot {
    Ready(IndexSnapshot),
    /// 書き出す前に上限超過が分かったので作らなかった。呼び出し側はスナップショット導入前の
    /// 経路 (作業ツリー + 変更ファイルだけ index の blob) へ縮退し、申告を出す。
    /// 規模だけを理由に review 全体を失敗させない (5 万ファイルで 170 秒かけてエラーになっていた)。
    OverBudget(OverBudget),
}

/// 上限を超えた tree の実測値と、超えた上限。
pub(super) struct OverBudget {
    pub files: usize,
    pub bytes: u64,
    pub budget: SnapshotBudget,
}

impl OverBudget {
    pub(super) fn truncation(&self, working_tree_matches_index: bool) -> TruncationInfo {
        TruncationInfo::staged_snapshot_unavailable(
            self.files,
            self.bytes,
            self.budget.files,
            self.budget.bytes,
            working_tree_matches_index,
        )
    }
}

pub(super) struct IndexSnapshot {
    _temporary: tempfile::TempDir,
    analysis_dir: PathBuf,
}

impl IndexSnapshot {
    pub(super) fn prepare(
        dir: &str,
        base: &str,
        expected_diff: &str,
        budget: SnapshotBudget,
    ) -> Result<PreparedSnapshot> {
        let original = Path::new(dir);
        // hook の index は読むだけにし、tree の作成は一時 index で行う。
        let index = git_text(original, &["rev-parse", "--git-path", "index"])?;
        let shared_index = git_text(original, &["rev-parse", "--shared-index-path"])?;
        let object_format = git_text(original, &["rev-parse", "--show-object-format"])?;
        if !matches!(object_format.as_str(), "sha1" | "sha256") {
            return Err(failure("Unsupported Git object format"));
        }
        let original_root =
            fs::canonicalize(git_text(original, &["rev-parse", "--show-toplevel"])?)?;
        let canonical_dir = fs::canonicalize(original)?;
        let prefix = canonical_dir.strip_prefix(&original_root)?;
        let objects = git_text(original, &["rev-parse", "--git-path", "objects"])?;
        let common = git_text(original, &["rev-parse", "--git-common-dir"])?;
        let absolute = |path: &str| {
            let path = Path::new(path);
            fs::canonicalize(if path.is_absolute() {
                path.to_path_buf()
            } else {
                original.join(path)
            })
        };
        let object_paths = [absolute(&objects)?, absolute(&common)?.join("objects")];
        let temporary = tempfile::Builder::new()
            .prefix("astro-sight-index-")
            .tempdir()?;
        let root = temporary.path();
        let init = isolated_git_command()
            .args(["init", "--quiet", "--template="])
            .arg(format!("--object-format={object_format}"))
            .arg(root)
            .output()?;
        if !init.status.success() {
            return Err(failure("Cannot initialize the staged analysis snapshot"));
        }
        fs::write(root.join(".git").join(INDEX_SNAPSHOT_MARKER), "")?;
        let mut alternates = String::new();
        for path in object_paths {
            let text = path
                .to_str()
                .ok_or_else(|| failure("Non-UTF-8 Git object directory"))?;
            if text.contains(['\n', '\r']) {
                return Err(failure("Unsupported Git object directory"));
            }
            alternates.push_str(text);
            alternates.push('\n');
        }
        fs::write(root.join(".git/objects/info/alternates"), alternates)?;
        let index = Path::new(&index);
        let index = if index.is_absolute() {
            index.to_path_buf()
        } else {
            original.join(index)
        };
        fs::copy(index, root.join(".git/index"))?;
        if !shared_index.is_empty() {
            let shared = Path::new(&shared_index);
            let shared = if shared.is_absolute() {
                shared.to_path_buf()
            } else {
                original.join(shared)
            };
            let name = shared
                .file_name()
                .ok_or_else(|| failure("Invalid shared index path"))?;
            fs::copy(&shared, root.join(".git").join(name))?;
        }
        let tree = git_text(root, &["write-tree"])?;
        if !matches!(tree.len(), 40 | 64) || !tree.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(failure("Invalid staged tree identifier"));
        }
        fs::write(root.join(".git").join(INDEX_SNAPSHOT_MARKER), &tree)?;
        // 独立した一時リポジトリなので、元の index / refs / worktree 登録は変えない。
        git_bytes(root, &["update-ref", "HEAD", base])?;
        git_bytes(root, &["read-tree", &tree])?;
        // 解析が読むファイルを先回りして選ばない。許可リストで絞ると、`.vue` や
        // `AndroidManifest.xml`・`jsconfig.json` が無いまま解析して unstaged と結果が食い違った。
        let manifest = read_manifest(&git_bytes(root, &["ls-tree", "-r", "-l", "-z", &tree])?)?;
        if manifest.files.len() > budget.files || manifest.total_bytes > budget.bytes {
            return Ok(PreparedSnapshot::OverBudget(OverBudget {
                files: manifest.files.len(),
                bytes: manifest.total_bytes,
                budget,
            }));
        }
        create_directories(root, &manifest)?;
        checkout_files(root, &manifest.files)?;
        let analysis_dir = root.join(prefix);
        fs::create_dir_all(&analysis_dir)?;
        let analysis_text = analysis_dir
            .to_str()
            .ok_or_else(|| failure("Non-UTF-8 staged analysis path"))?;
        if diff_contract(&crate::commands::git_input::run_git_diff(
            analysis_text,
            base,
            false,
        )?) != diff_contract(expected_diff)
        {
            return Err(AstroError::new(
                ErrorCode::ReviewInputChanged,
                "Staged input changed while creating the analysis snapshot",
            )
            .into());
        }
        Ok(PreparedSnapshot::Ready(Self {
            _temporary: temporary,
            analysis_dir,
        }))
    }

    pub(super) fn dir(&self) -> Result<&str> {
        self.analysis_dir
            .to_str()
            .ok_or_else(|| failure("Non-UTF-8 staged analysis path"))
    }
}

/// 明示 diff の hunk を、offset の移動を許さず new 側のソースと照合する。
pub(super) fn verify_explicit_new_side(
    dir: &str,
    diff: &str,
    files: &[crate::models::impact::DiffFile],
) -> Result<()> {
    let sections = crate::engine::diff::FileSections::split(diff);
    for file in files {
        let path = if file.new_path == "/dev/null" {
            &file.old_path
        } else {
            &file.new_path
        };
        if !safe_path(path) {
            return Err(failure("Unsafe explicit diff path"));
        }
        let full = Path::new(dir).join(path);
        let utf8 = camino::Utf8Path::new(
            full.to_str()
                .ok_or_else(|| failure("Non-UTF-8 analysis path"))?,
        );
        let known = crate::language::LangId::from_path(camino::Utf8Path::new(path)).is_ok();
        if file.new_path == "/dev/null" {
            if known && full.try_exists()? {
                return Err(failure(format!(
                    "Explicit diff deletes a source file that still exists: {path}"
                )));
            }
            continue;
        }
        let source = match crate::engine::parser::read_file(utf8) {
            Ok(source) => source,
            Err(_) if !known => continue,
            Err(_) => {
                return Err(failure(format!(
                    "Cannot verify explicit diff source: {path}"
                )));
            }
        };
        if crate::engine::parser::detect_lang(utf8, &source).is_err() {
            continue;
        }
        if !file.hunks.is_empty()
            && crate::engine::diff::reconstruct_old_source(&sections.get(path), path, &source)
                .is_none()
        {
            return Err(failure(format!(
                "Explicit diff does not match the source at its declared line offsets: {path}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::tests::common::init_git_repo_for_test;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn write(repo: &Path, path: &str, content: &[u8]) {
        let full = repo.join(path);
        fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        fs::write(full, content).expect("write");
    }

    fn commit_all(repo: &Path, message: &str) {
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-qm", message]);
    }

    fn head(repo: &Path) -> String {
        String::from_utf8(git(repo, &["rev-parse", "HEAD"]))
            .expect("utf-8")
            .trim()
            .to_owned()
    }

    fn prepare_staged(repo: &Path, budget: SnapshotBudget) -> Result<PreparedSnapshot> {
        let dir = repo.to_str().expect("utf-8");
        let base = head(repo);
        let diff = crate::commands::git_input::run_git_diff(dir, &base, true)?;
        IndexSnapshot::prepare(dir, &base, &diff, budget)
    }

    fn ready(prepared: Result<PreparedSnapshot>) -> IndexSnapshot {
        match prepared.expect("snapshot") {
            PreparedSnapshot::Ready(snapshot) => snapshot,
            PreparedSnapshot::OverBudget(_) => panic!("small staged tree must be materialized"),
        }
    }

    /// tree の通常ファイルは、解析対象の言語かどうかに関わらず index の raw blob のまま全部書き出す。
    /// 許可リストで絞っていたころは `.vue` / `AndroidManifest.xml` / `jsconfig.json` などが無く、
    /// staged と unstaged の結果が食い違った。作業ツリー用の変換 (eol / ident /
    /// working-tree-encoding) は掛けず、symlink は書かず、gitlink は空ディレクトリにする。
    #[test]
    fn every_regular_file_is_materialized_as_the_raw_index_blob() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        write(
            repo,
            ".gitattributes",
            b"*.txt text eol=crlf\n*.c ident\n*.utf16 working-tree-encoding=UTF-16\n",
        );
        write(repo, "src/app.ts", b"export const a = 1;\n");
        // 非 UTF-8 のパスは差分入力では拒否する。基準側に置き、生パスの展開だけを確かめる。
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStringExt;
            fs::write(
                repo.join(std::ffi::OsString::from_vec(b"raw-\xff.txt".to_vec())),
                b"raw\n",
            )
            .expect("write non-UTF-8 name");
        }
        commit_all(repo, "base");
        let base = head(repo);
        let files: &[(&str, &[u8])] = &[
            ("src/app.ts", b"export const a = 2;\n"),
            (
                "src/App.vue",
                b"<script>import { a } from './app';</script>\n",
            ),
            (
                "app/src/main/AndroidManifest.xml",
                b"<manifest><activity android:name=\".MainActivity\"/></manifest>\n",
            ),
            (
                "jsconfig.json",
                b"{\"compilerOptions\":{\"paths\":{\"@/*\":[\"src/*\"]}}}\n",
            ),
            ("settings.gradle", b"include ':app'\n"),
            (".cargo/config.toml", b"[build]\n"),
            ("bin/tool", b"#!/bin/sh\necho tool\n"),
            ("assets/logo.png", b"\x89PNG\r\n\x1a\n\x00\x00binary"),
            ("notes/lines.txt", b"one\ntwo\n"),
            ("native/ident.c", b"/* $Id$ */\nint x;\n"),
            // UTF-16 (BOM 付き) で書くと、add で UTF-8 の blob になる。
            ("text/data.utf16", b"\xff\xfep\x00l\x00\n\x00"),
        ];
        for (path, content) in files {
            write(repo, path, content);
        }
        write(repo, "scripts/run.sh", b"#!/bin/sh\necho run\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                repo.join("scripts/run.sh"),
                fs::Permissions::from_mode(0o755),
            )
            .expect("chmod");
            std::os::unix::fs::symlink("src/app.ts", repo.join("link")).expect("symlink");
        }
        git(repo, &["add", "-A"]);
        git(
            repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{base},vendor/module"),
            ],
        );
        // 作業ツリーだけの編集はスナップショットに入れない。
        write(repo, "src/app.ts", b"export const a = 3;\n");
        write(repo, "notes/lines.txt", b"worktree only\n");
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStringExt;
            fs::write(
                repo.join(std::ffi::OsString::from_vec(b"raw-\xff.txt".to_vec())),
                b"worktree only\n",
            )
            .expect("write non-UTF-8 worktree file");
        }

        let snapshot = ready(prepare_staged(repo, SnapshotBudget::DEFAULT));
        let root = Path::new(snapshot.dir().expect("utf-8"));
        let mut paths: Vec<&str> = files.iter().map(|(path, _)| *path).collect();
        paths.extend([".gitattributes", "scripts/run.sh"]);
        for path in paths {
            let blob = git(repo, &["cat-file", "blob", &format!(":{path}")]);
            assert_eq!(fs::read(root.join(path)).expect(path), blob, "{path}");
        }
        assert_eq!(
            fs::read(root.join("notes/lines.txt")).expect("lines"),
            b"one\ntwo\n",
            "CRLF への変換も作業ツリーの編集も入れない"
        );
        assert!(
            root.join("vendor/module").is_dir(),
            "gitlink は空ディレクトリ"
        );
        assert_eq!(
            fs::read_dir(root.join("vendor/module"))
                .expect("gitlink dir")
                .count(),
            0
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(
                fs::symlink_metadata(root.join("link")).is_err(),
                "symlink は書き出さない"
            );
            let mode = fs::metadata(root.join("scripts/run.sh"))
                .expect("run.sh")
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "100755 は実行可能のまま");
            let mode = fs::metadata(root.join("src/app.ts"))
                .expect("app.ts")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStringExt;
            assert_eq!(
                fs::read(root.join(std::ffi::OsString::from_vec(b"raw-\xff.txt".to_vec())))
                    .expect("non-UTF-8 name"),
                b"raw\n"
            );
        }
    }

    /// 上限は書き出す前に、tree の通常ファイル全体の件数と合計バイト数で判定する。
    /// 超えたらエラーにせず、実測値と上限を持って `OverBudget` を返す (境界は上限ちょうどまで可)。
    #[test]
    fn over_budget_is_decided_from_every_regular_file_before_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        write(repo, "a.py", b"def a():\n    return 1\n");
        write(repo, "image.png", b"0123456789");
        commit_all(repo, "base");
        write(repo, "a.py", b"def a():\n    return 2\n");
        git(repo, &["add", "-A"]);
        let files = 2;
        let bytes: u64 = 22 + 10;

        for budget in [
            SnapshotBudget {
                files: files - 1,
                bytes: u64::MAX,
            },
            SnapshotBudget {
                files: usize::MAX,
                bytes: bytes - 1,
            },
        ] {
            match prepare_staged(repo, budget).expect("over budget is not an error") {
                PreparedSnapshot::OverBudget(over) => {
                    assert_eq!((over.files, over.bytes), (files, bytes));
                    let notice = over.truncation(true);
                    assert_eq!(
                        notice.reason,
                        crate::models::truncation::TruncationReason::StagedSnapshotUnavailable
                    );
                    assert!(
                        notice
                            .message
                            .contains(&format!("{files} files / {bytes} bytes")),
                        "{}",
                        notice.message
                    );
                }
                PreparedSnapshot::Ready(_) => panic!("budget {} / {}", budget.files, budget.bytes),
            }
        }
        // 対照: 上限ちょうどなら作る。
        ready(prepare_staged(repo, SnapshotBudget { files, bytes }));
    }

    /// 大文字小文字だけが違うパスは、大文字小文字を区別しない FS では同じ実体になる。
    /// 黙って同じディレクトリ・ファイルへ混ぜず失敗させる。区別する FS では両方を書く。
    #[test]
    fn case_aliased_paths_fail_instead_of_being_merged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let probe = dir.path().join("probe");
        fs::write(&probe, "").expect("probe");
        let case_insensitive = dir.path().join("PROBE").exists();

        for aliases in [["dir/x.txt", "DIR/y.txt"], ["a.txt", "A.txt"]] {
            let repo_dir = tempfile::tempdir().expect("tempdir");
            let repo = repo_dir.path();
            init_git_repo_for_test(repo);
            write(repo, "seed.txt", b"seed\n");
            commit_all(repo, "base");
            let blob = String::from_utf8(git(repo, &["hash-object", "-w", "seed.txt"]))
                .expect("utf-8")
                .trim()
                .to_owned();
            for path in aliases {
                git(
                    repo,
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("100644,{blob},{path}"),
                    ],
                );
            }
            let result = prepare_staged(repo, SnapshotBudget::DEFAULT);
            if case_insensitive {
                let error = result.err().expect("aliases must not be merged");
                assert!(error.to_string().contains("collide"), "{error}");
            } else {
                let snapshot = ready(result);
                for path in aliases {
                    assert!(
                        Path::new(snapshot.dir().expect("utf-8"))
                            .join(path)
                            .is_file()
                    );
                }
            }
        }
    }
}
