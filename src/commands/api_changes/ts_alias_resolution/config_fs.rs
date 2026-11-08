//! 設定ファイルを読むファイルシステム: 今のファイルシステムか、base の版 (Issue #287)。
//!
//! `stripInternal` の旧版の評価は、旧版のソースを旧版の設定で読む。base の版では、Git のルートの
//! 中で追跡したパスを base の tree から読み、base に無い設定を作業ツリーで補わない。Git のルートの
//! 外 (モノレポの外の共通の設定) と node_modules の下 (追跡しない依存) は、旧新で共通の今の
//! ファイルシステムから読む (依存の過去の状態は再現しない)。

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;

/// `git ls-tree` の出力の上限。超えれば base の tree を作らない (旧版の設定を評価できない)。
const MAX_LISTING_BYTES: usize = 64 * 1024 * 1024;

/// 設定ファイルを読むファイルシステム。
#[derive(Clone, Default)]
pub(crate) enum ConfigFs {
    /// 今のファイルシステム (作業ツリー)。
    #[default]
    Current,
    /// base の版。
    Base(Rc<BaseTree>),
}

/// ディレクトリの一覧。
pub(crate) enum Listing {
    /// 子の名前 (正規化していない)。
    Entries(Vec<String>),
    /// ディレクトリが無い。
    Missing,
    /// 読めない。
    Unreadable,
}

/// base の版の Git の tree。
pub(crate) struct BaseTree {
    /// Git のルート (正規化したパス)。
    root: PathBuf,
    /// 通常のファイル (root 相対、`/` 区切り)。symlink と submodule は入れない。
    files: HashSet<String>,
    /// ディレクトリ (root 相対。root は空文字) → 子の名前。
    directories: HashMap<String, HashSet<String>>,
    /// 大文字小文字を区別しないファイルシステムのための、小文字にしたパス → tree のパス (ファイル
    /// とディレクトリ)。
    folded: HashMap<String, String>,
    /// root を cwd にした base の blob の読み手 (root 相対のパスで読む)。
    blobs: crate::commands::git_input::GitBlobBatch,
}

impl BaseTree {
    /// `dir` を含む Git のリポジトリの、版 `rev` の tree。リポジトリでないか、一覧を取れなければ
    /// None。
    pub(crate) fn load(dir: &str, rev: &str) -> Option<Self> {
        let top = git_output(dir, &["rev-parse", "--show-toplevel"], 64 * 1024)?;
        let top = String::from_utf8(top).ok()?;
        // git は `rev-parse` の返したパスで動かす (Windows の正規化したパス `\\?\C:\…` を cwd に
        // しない)。パスの比較には正規化したパスを使う (設定の探索も正規化したパスで辿る)。
        let top = top.trim().to_owned();
        let root = Path::new(&top).canonicalize().ok()?;
        let listing = git_output(
            &top,
            &["ls-tree", "-r", "-z", "--full-tree", rev],
            MAX_LISTING_BYTES,
        )?;
        let mut files = HashSet::new();
        let mut directories: HashMap<String, HashSet<String>> = HashMap::new();
        directories.insert(String::new(), HashSet::new());
        for entry in listing.split(|byte| *byte == 0) {
            if entry.is_empty() {
                continue;
            }
            let entry = std::str::from_utf8(entry).ok()?;
            let (meta, path) = entry.split_once('\t')?;
            let mode = meta.split(' ').next()?;
            // 通常のファイル (100644・100755) だけ。symlink (120000) と submodule (160000) は
            // 通常のファイルとして読まない。
            if !mode.starts_with("100") {
                continue;
            }
            let parts: Vec<&str> = path.split('/').collect();
            let mut parent = String::new();
            for (index, part) in parts.iter().enumerate() {
                let child = if parent.is_empty() {
                    (*part).to_owned()
                } else {
                    format!("{parent}/{part}")
                };
                directories
                    .entry(parent.clone())
                    .or_default()
                    .insert((*part).to_owned());
                if index + 1 < parts.len() {
                    directories.entry(child.clone()).or_default();
                }
                parent = child;
            }
            files.insert(path.to_owned());
        }
        let folded = files
            .iter()
            .chain(directories.keys())
            .map(|path| (path.to_lowercase(), path.clone()))
            .collect();
        Some(Self {
            root,
            files,
            directories,
            folded,
            blobs: crate::commands::git_input::GitBlobBatch::new(&top, rev),
        })
    }

    /// tree の中のパス `relative` の、tree の綴り。大文字小文字を区別しないファイルシステム
    /// (Windows・macOS) では、今のファイルシステムと同じく綴りの違いを同じパスとみなす。無ければ
    /// そのまま。
    fn spelled(&self, relative: String) -> String {
        if !(cfg!(windows) || cfg!(target_os = "macos"))
            || self.files.contains(&relative)
            || self.directories.contains_key(&relative)
        {
            return relative;
        }
        self.folded
            .get(&relative.to_lowercase())
            .cloned()
            .unwrap_or(relative)
    }

    /// `path` が base の tree から読む場所なら、root 相対のパス。root の外と node_modules の下は
    /// None (今のファイルシステムから読む)。
    fn relative(&self, path: &Path) -> Option<String> {
        let rest = path.strip_prefix(&self.root).ok()?;
        let mut parts = Vec::new();
        for component in rest.components() {
            match component {
                Component::Normal(part) => {
                    let part = part.to_str()?;
                    if part == "node_modules" {
                        return None;
                    }
                    parts.push(part);
                }
                _ => return None,
            }
        }
        Some(parts.join("/"))
    }
}

impl ConfigFs {
    /// `path` を base の tree から読むなら、その tree と root 相対のパス。
    fn base(&self, path: &Path) -> Option<(&BaseTree, String)> {
        match self {
            ConfigFs::Current => None,
            ConfigFs::Base(tree) => Some((tree, tree.spelled(tree.relative(path)?))),
        }
    }

    /// symlink でない通常のファイルがあるか。読めないときは None。
    pub(crate) fn is_regular_file(&self, path: &Path) -> Option<bool> {
        match self.base(path) {
            Some((tree, relative)) => Some(tree.files.contains(&relative)),
            None => super::is_regular_file(path),
        }
    }

    /// symlink でないディレクトリがあるか。読めないときは None。
    pub(crate) fn is_directory(&self, path: &Path) -> Option<bool> {
        match self.base(path) {
            Some((tree, relative)) => Some(tree.directories.contains_key(&relative)),
            None => super::is_directory(path),
        }
    }

    /// 正規化したパス。base の tree の中は、あるファイルかディレクトリだけ (symlink は tree に
    /// 通常のファイルとして入らない)。無ければ None。
    pub(crate) fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        match self.base(path) {
            Some((tree, relative)) => (tree.files.contains(&relative)
                || tree.directories.contains_key(&relative))
            .then(|| {
                relative
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .fold(tree.root.clone(), |path, part| path.join(part))
            }),
            None => path.canonicalize().ok(),
        }
    }

    /// symlink でない通常のファイルなら、その正規化したパス。
    pub(crate) fn regular_file(&self, path: &Path) -> Option<PathBuf> {
        self.is_regular_file(path)?
            .then(|| self.canonicalize(path))
            .flatten()
    }

    /// 上限 `max_bytes` 付きで UTF-8 のファイルを読む。読めなければ None。
    pub(crate) fn read_utf8_limited(&self, path: &Path, max_bytes: u64) -> Option<String> {
        match self.base(path) {
            Some((tree, relative)) => {
                if !tree.files.contains(&relative) {
                    return None;
                }
                let bytes = tree
                    .blobs
                    .read_limited(&relative, usize::try_from(max_bytes).ok()?)?;
                String::from_utf8(bytes).ok()
            }
            None => crate::engine::bounded_read::read_utf8_file_limited(path, max_bytes),
        }
    }

    /// ディレクトリの子の名前。
    pub(crate) fn read_dir(&self, directory: &Path) -> Listing {
        if let Some((tree, relative)) = self.base(directory) {
            return match tree.directories.get(&relative) {
                Some(names) => Listing::Entries(names.iter().cloned().collect()),
                None => Listing::Missing,
            };
        }
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if super::missing(&error) => return Listing::Missing,
            Err(_) => return Listing::Unreadable,
        };
        let mut names = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else {
                return Listing::Unreadable;
            };
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
        Listing::Entries(names)
    }
}

/// `dir` で git を動かし、上限 `max` までの標準出力。失敗か上限超えなら None。
fn git_output(dir: &str, args: &[&str], max: usize) -> Option<Vec<u8>> {
    let mut child = crate::git_support::git_command_for_dir(dir)
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()?
        .take(u64::try_from(max).ok()? + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > max {
        // 残りを書けずに止まらないよう、読み切らない子は止める。
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    child.wait().ok()?.success().then_some(bytes)
}
