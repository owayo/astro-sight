//! Git を呼び出す全レイヤーで共有する入力契約。
//!
//! CLI (`commands`) と解析エンジン (`engine`) の双方から利用するため、どちらか一方へ
//! 置くと逆依存になる。セキュリティ境界である revision 検証と既定 revision は、この
//! 中立モジュールを唯一の正本とする。

use anyhow::{Result, bail};

use crate::error::{AstroError, ErrorCode};

pub(crate) const INDEX_SNAPSHOT_MARKER: &str = "astro-sight-index-snapshot";

pub(crate) fn index_snapshot_tree(dir: impl AsRef<std::path::Path>) -> Option<String> {
    let canonical = std::fs::canonicalize(dir).ok()?;
    for parent in canonical.ancestors() {
        let marker = parent.join(".git").join(INDEX_SNAPSHOT_MARKER);
        if marker.is_file() {
            let tree = std::fs::read_to_string(marker).ok()?;
            let tree = tree.trim();
            return (matches!(tree.len(), 40 | 64) && tree.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| tree.to_owned());
        }
    }
    None
}

/// 一時リポジトリには hook が渡した元の index / worktree 環境を持ち込まない。
pub(crate) fn isolated_git_command() -> std::process::Command {
    let mut command = std::process::Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
}

/// `dir` が staged の解析用に index を書き出した一時リポジトリの中か。
pub(crate) fn is_inside_index_snapshot(dir: impl AsRef<std::path::Path>) -> bool {
    let dir = dir.as_ref();
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    canonical
        .ancestors()
        .any(|parent| parent.join(".git").join(INDEX_SNAPSHOT_MARKER).is_file())
}

pub(crate) fn git_command_for_dir(dir: impl AsRef<std::path::Path>) -> std::process::Command {
    if is_inside_index_snapshot(dir) {
        isolated_git_command()
    } else {
        std::process::Command::new("git")
    }
}

/// snapshot が書き出さなかった symlink と gitlink の記録 (snapshot の `.git` の下、JSON)。
pub(crate) const SNAPSHOT_OMISSIONS: &str = "astro-sight-snapshot-omissions.json";
pub(crate) const SNAPSHOT_OMISSIONS_VERSION: u32 = 1;

/// symlink の先。snapshot と同じ tree の中で、要素を 1 つずつ辿って決める。途中に symlink・gitlink・
/// ファイルがある、tree の外へ出る、tree に無いなら `Unknown`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LinkTarget {
    RegularFile,
    Directory,
    Unknown,
}

/// snapshot が書き出さなかった要素の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OmittedKind {
    Symlink(LinkTarget),
    Gitlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum OmittedEntryKind {
    Symlink,
    Gitlink,
}

/// 記録の 1 件。`path` は snapshot のリポジトリの root からの `/` 区切りのパス。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct OmittedEntry {
    path: String,
    kind: OmittedEntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<LinkTarget>,
}

impl OmittedEntry {
    pub(crate) fn symlink(path: String, target: LinkTarget) -> Self {
        Self {
            path,
            kind: OmittedEntryKind::Symlink,
            target: Some(target),
        }
    }

    pub(crate) fn gitlink(path: String) -> Self {
        Self {
            path,
            kind: OmittedEntryKind::Gitlink,
            target: None,
        }
    }
}

/// [`SNAPSHOT_OMISSIONS`] の中身。`complete` が偽 (書けない名前があった) なら、読む側は使わない。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct OmissionRecord {
    pub(crate) version: u32,
    /// snapshot の marker と同じ tree の OID。
    pub(crate) tree: String,
    pub(crate) complete: bool,
    pub(crate) entries: Vec<OmittedEntry>,
}

/// `dir` の作業ツリーが、実際の checkout と同じに見えるか (Issue #376)。
///
/// staged の解析用に index を展開した snapshot は symlink を書き出さず、gitlink を空のディレクトリに
/// する。ディレクトリを走査して symlink を障壁にする証明や、ファイルの有無で候補を選ぶ証明は、
/// 書き出さなかった要素を実際の checkout の symlink・gitlink と同じに扱う必要がある。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TreeView {
    /// snapshot の中ではない。
    Checkout,
    /// snapshot で、書き出さなかった要素の記録を読めた。
    Snapshot(SnapshotOmissions),
    /// snapshot だが、記録が無い・読めない・tree が合わない・不完全か、記録が snapshot の中身と
    /// 合わない (書き出さなかった symlink の名前で別の要素を開ける、gitlink の空ディレクトリが無い
    /// など) か、`dir` 自身が gitlink の下にある。
    Unusable,
}

/// snapshot の中の、書き出さなかった要素の位置。
///
/// 名前の比べ方 (大文字小文字・Unicode の正規化・比べるときに無視する文字・8.3 の短い名前など) は
/// ファイルシステムごとに違い、文字列では再現しきれない。位置は snapshot のファイルシステムで開いて
/// (canonicalize して) 決め、問い合わせも同じファイルシステムで開いて照らす。snapshot はどの index の
/// 要素も (symlink は親ディレクトリまで) 同じ名前で作るので、開けた名前は実際の checkout でも同じ要素に
/// なる。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SnapshotOmissions {
    /// [`tree_view`] に渡された `dir`。問い合わせはここから開く。
    dir: std::path::PathBuf,
    /// `dir` の下の、書き出さなかった要素の `dir` からの相対パスと種類。親ディレクトリは snapshot での
    /// 実際の名前。
    entries: Vec<(std::path::PathBuf, OmittedKind)>,
    /// 書き出さなかった symlink の親ディレクトリ (canonicalize したもの)。
    symlink_parents: std::collections::BTreeSet<std::path::PathBuf>,
    /// gitlink の空ディレクトリ (canonicalize したもの)。
    gitlinks: Vec<std::path::PathBuf>,
}

impl TreeView {
    /// `dir` の下の、書き出さなかった要素。通常の checkout なら空、記録を使えなければ None。
    pub(crate) fn omitted(&self) -> Option<&[(std::path::PathBuf, OmittedKind)]> {
        match self {
            Self::Checkout => Some(&[]),
            Self::Snapshot(omissions) => Some(&omissions.entries),
            Self::Unusable => None,
        }
    }

    /// `relative` (`dir` からの相対) の有無や種類が、書き出さなかった要素のために実際の checkout と
    /// 違いうるか ([`SnapshotOmissions::hides`])。記録を使えなければ常に真。
    pub(crate) fn hides(&self, relative: &std::path::Path) -> bool {
        match self {
            Self::Checkout => false,
            Self::Snapshot(omissions) => omissions.hides(relative),
            Self::Unusable => true,
        }
    }
}

impl SnapshotOmissions {
    /// `relative` を `dir` から snapshot のファイルシステムで 1 要素ずつ開いて決める。
    ///
    /// - 開けた要素は、実際の checkout でも同じ index の要素になる。gitlink の空ディレクトリかその下
    ///   なら、中身を確かめられないので真。
    /// - 開けない名前は、そのディレクトリの書き出さなかった symlink の別名 (大文字小文字・正規化・
    ///   8.3 の短い名前など) でありうるので、書き出さなかった symlink の親ディレクトリなら真。ほかの
    ///   ディレクトリやファイルの下なら、実際の checkout でも開けない。
    /// - `..` や絶対パスは、途中に symlink があると OS によって辿り方が違うので真。
    fn hides(&self, relative: &std::path::Path) -> bool {
        if self.symlink_parents.is_empty() && self.gitlinks.is_empty() {
            return false;
        }
        let mut names = Vec::new();
        for component in relative.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::Normal(name) => names.push(name),
                _ => return true,
            }
        }
        let mut opened = self.dir.clone();
        let mut missing = false;
        for name in names {
            let next = opened.join(name);
            match std::fs::symlink_metadata(&next) {
                // snapshot は symlink を書き出さない。
                Ok(metadata) if metadata.file_type().is_symlink() => return true,
                Ok(metadata) => {
                    opened = next;
                    if !metadata.is_dir() {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing = true;
                    break;
                }
                Err(_) => return true,
            }
        }
        let Ok(opened) = std::fs::canonicalize(&opened) else {
            return true;
        };
        self.gitlinks
            .iter()
            .any(|gitlink| opened.starts_with(gitlink))
            || (missing && self.symlink_parents.contains(&opened))
    }
}

#[derive(Default)]
struct TreeViewCache {
    /// [`forget_tree_views`] のたびに増やす。読んでいる間に忘れた結果は登録しない。
    generation: u64,
    views: std::collections::HashMap<std::path::PathBuf, std::sync::Arc<TreeView>>,
}

fn tree_view_cache() -> std::sync::MutexGuard<'static, TreeViewCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<TreeViewCache>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(Default::default)
        .lock()
        .expect("tree view cache mutex poisoned")
}

/// `dir` の [`TreeView`]。同じ `dir` は、次に snapshot を作るか捨てるまで ([`forget_tree_views`])
/// 1 度だけ求める。
pub(crate) fn tree_view(dir: &std::path::Path) -> std::sync::Arc<TreeView> {
    let generation = {
        let cache = tree_view_cache();
        if let Some(view) = cache.views.get(dir) {
            return view.clone();
        }
        cache.generation
    };
    let view = std::sync::Arc::new(read_tree_view(dir));
    let mut cache = tree_view_cache();
    if cache.generation == generation {
        cache.views.insert(dir.to_owned(), view.clone());
    }
    view
}

/// [`tree_view`] の結果を忘れる。snapshot を作る・捨てるたびに呼び、同じパスに作り直した snapshot や、
/// 前に通常のディレクトリとして見たパスで、古い見え方を引かない。
pub(crate) fn forget_tree_views() {
    let mut cache = tree_view_cache();
    cache.generation += 1;
    cache.views.clear();
}

fn read_tree_view(dir: &std::path::Path) -> TreeView {
    let Ok(canonical) = std::fs::canonicalize(dir) else {
        return if is_inside_index_snapshot(dir) {
            TreeView::Unusable
        } else {
            TreeView::Checkout
        };
    };
    let Some(root) = canonical
        .ancestors()
        .find(|parent| parent.join(".git").join(INDEX_SNAPSHOT_MARKER).is_file())
    else {
        return TreeView::Checkout;
    };
    (|| {
        let tree = index_snapshot_tree(root)?;
        let path = root.join(".git").join(SNAPSHOT_OMISSIONS);
        if std::fs::metadata(&path).ok()?.len() > 64 * 1024 * 1024 {
            return None;
        }
        let record: OmissionRecord =
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        if record.version != SNAPSHOT_OMISSIONS_VERSION || record.tree != tree || !record.complete {
            return None;
        }
        let mut omissions = SnapshotOmissions {
            dir: dir.to_owned(),
            entries: Vec::new(),
            symlink_parents: std::collections::BTreeSet::new(),
            gitlinks: Vec::new(),
        };
        // 同じ親ディレクトリは 1 度だけ開く。
        let mut parents = std::collections::HashMap::new();
        for entry in record.entries {
            // Windows では `\` も区切りになり、`C:` は prefix になる。記録の writer は書かない形。
            let parts: Vec<&str> = entry.path.split('/').collect();
            if entry.path.contains(['\\', ':'])
                || parts
                    .iter()
                    .any(|part| part.is_empty() || matches!(*part, "." | ".."))
            {
                return None;
            }
            let path: std::path::PathBuf = parts.iter().collect();
            if !path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
            {
                return None;
            }
            let kind = match (entry.kind, entry.target) {
                (OmittedEntryKind::Symlink, Some(target)) => OmittedKind::Symlink(target),
                (OmittedEntryKind::Gitlink, None) => OmittedKind::Gitlink,
                _ => return None,
            };
            let location = match kind {
                OmittedKind::Gitlink => {
                    // snapshot は gitlink を空のディレクトリにする。
                    let metadata = std::fs::symlink_metadata(root.join(&path)).ok()?;
                    if !metadata.is_dir() || metadata.file_type().is_symlink() {
                        return None;
                    }
                    let location = std::fs::canonicalize(root.join(&path)).ok()?;
                    omissions.gitlinks.push(location.clone());
                    location
                }
                OmittedKind::Symlink(_) => {
                    // 書き出さなかった symlink の名前で開ける要素があれば、実際の checkout でどちらが
                    // 残るか分からない。
                    match std::fs::symlink_metadata(root.join(&path)) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        _ => return None,
                    }
                    let parent = path.parent().unwrap_or(std::path::Path::new(""));
                    let opened = match parents.get(parent) {
                        Some(opened) => std::path::PathBuf::clone(opened),
                        None => {
                            let opened = std::fs::canonicalize(root.join(parent)).ok()?;
                            parents.insert(parent.to_owned(), opened.clone());
                            opened
                        }
                    };
                    omissions.symlink_parents.insert(opened.clone());
                    opened.join(path.file_name()?)
                }
            };
            if !location.starts_with(root) {
                return None;
            }
            if let Ok(relative) = location.strip_prefix(&canonical)
                && relative.components().next().is_some()
            {
                omissions.entries.push((relative.to_owned(), kind));
            }
        }
        // 解析の root 自身が gitlink (の下) なら、中身を確かめられない。
        if omissions
            .gitlinks
            .iter()
            .any(|gitlink| canonical.starts_with(gitlink))
        {
            return None;
        }
        Some(TreeView::Snapshot(omissions))
    })()
    .unwrap_or(TreeView::Unusable)
}

/// `--base` を省略したときの既定 base。context / impact / review / dead-code / cochange で
/// 共通に「未コミットの作業ツリー変更」を既定の解析対象にする。
///
/// CLI は省略と明示 (`--base HEAD`) を区別して持ち、この値を補うのは使う直前にする。
/// HEAD がまだコミットを指していない (unborn) とき、省略なら解析を飛ばし、明示ならエラーに
/// するため。
pub const DEFAULT_GIT_BASE: &str = "HEAD";

/// cochange の既定 base (`DEFAULT_GIT_BASE` と同じ)。
pub const DEFAULT_BLAME_BASE: &str = DEFAULT_GIT_BASE;

/// 出力を解析する `git diff` 呼び出しに必ず付ける引数。
///
/// porcelain の `git diff` は利用者の git 設定で出力形式が変わる。パーサは既定の形式
/// (`a/` / `b/` 接頭辞・色なし・内蔵 diff・textconv なし) を前提にしているため、次の
/// どれか 1 つでも設定されていると 1 ファイルも認識できず、`context` が `{"changes":[]}`、
/// `impact --hook` / `review --hook` が exit 0 で破壊的変更を素通しする (fail-open)。
/// - `diff.mnemonicPrefix` / `diff.noprefix` / `diff.srcPrefix` / `diff.dstPrefix`:
///   ヘッダが `--- c/..` / `--- ..` 等になる → 接頭辞をコマンドラインで固定する。
///   `-c diff.mnemonicPrefix=false` のような設定ごとの上書きは、上書きし忘れた設定
///   (`diff.srcPrefix` 等) や将来追加される設定を取りこぼすため採らない
///   (コマンドライン指定はどの設定よりも優先される)。
/// - `diff.external` / `GIT_EXTERNAL_DIFF` / gitattributes の diff driver: 外部ツールの
///   出力に置き換わる (失敗すると git ごと異常終了する) → `--no-ext-diff`
/// - `color.diff` / `color.ui = always`: 行頭に ANSI エスケープが付く → `--no-color`
/// - textconv (`diff=<driver>` 属性 + `diff.<driver>.textconv`): 変換後テキストの diff に
///   なり、解析対象のファイル内容と行が一致しない → `--no-textconv`
/// - `diff.submodule=diff`: gitlink の更新がサブモジュール内部のファイル差分に展開され、
///   親リポジトリに存在しないソースの削除に見える → `--submodule=short`
/// - `diff.ignoreSubmodules` / `.gitmodules` の `ignore`: gitlink の更新が diff から消え、
///   サブモジュールの manifest の変更を Cargo の構成の変更と認識できない (Issue #377)
///   → `--ignore-submodules=none`
pub(crate) const GIT_DIFF_PARSEABLE_OUTPUT_ARGS: [&str; 12] = [
    "--no-ext-diff",
    "--no-color",
    "--no-textconv",
    "--submodule=short",
    "--ignore-submodules=none",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--diff-algorithm=myers",
    "--unified=3",
    "--inter-hunk-context=0",
    "--indent-heuristic",
    "--full-index",
];

/// `git diff` / `git show` / `git blame` に渡す revision または path を検証する。
///
/// 先頭が `-` の値は git がオプションとして解釈するため拒否する。空文字と NUL も
/// プロセス引数として不正なので、Git subprocess を起動する前に共通して弾く。
pub(crate) fn validate_git_revision(value: &str, arg_name: &str) -> Result<()> {
    if value.is_empty() {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{arg_name} must not be empty"),
        ));
    }
    if value.starts_with('-') {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{arg_name} must not start with '-': {value}"),
        ));
    }
    if value.contains('\0') {
        bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{arg_name} must not contain NUL"),
        ));
    }
    Ok(())
}

/// workspace 相対パスの区切り文字を `/` に正規化する。
///
/// unified diff 由来のパス (`DiffFile.new_path` / `ApiSymbol.file` 等) は常に `/` 区切りである
/// 一方、`Path::to_string_lossy` は Windows で `\` を返す。正規化しないと、参照パスと diff の
/// パスの突き合わせ (同一 diff 内の参照・削除シンボルの帰属・dead の絞り込み) が Windows で
/// 常に不一致になり、出力のパス表記 (`refs` の `path` / `impacted_callers` / `dead_symbols` 等)
/// もコマンドごとにばらつく。
///
/// Unix ではバックスラッシュがファイル名の正当な文字なので、`MAIN_SEPARATOR` が
/// `/` のプラットフォームでは何も置換しない。
pub(crate) fn normalize_workspace_separators(path: &str) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        path.to_string()
    } else {
        path.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

/// テスト用: `root` を staged の snapshot の形 (marker と、書き出さなかった要素の記録) にする。`entries`
/// は記録の `entries` の JSON の中身。実際の snapshot と同じく、gitlink の空ディレクトリと symlink の
/// 親ディレクトリも作る。
#[cfg(test)]
pub(crate) fn fake_snapshot_record(root: &std::path::Path, entries: &str) {
    let tree = "0".repeat(40);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git").join(INDEX_SNAPSHOT_MARKER), &tree).unwrap();
    let record =
        format!("{{\"version\":1,\"tree\":\"{tree}\",\"complete\":true,\"entries\":[{entries}]}}");
    std::fs::write(root.join(".git").join(SNAPSHOT_OMISSIONS), &record).unwrap();
    create_recorded_directories(root, &record);
}

/// テスト用: 記録 (JSON) の gitlink の空ディレクトリと symlink の親ディレクトリを、実際の snapshot と
/// 同じく作る。読めない記録や、読む側が拒むパスは作らない。
#[cfg(test)]
fn create_recorded_directories(root: &std::path::Path, record: &str) {
    let Ok(record) = serde_json::from_str::<OmissionRecord>(record) else {
        return;
    };
    for entry in record.entries {
        if entry.path.contains(['\\', ':'])
            || entry
                .path
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
        {
            continue;
        }
        let path = root.join(&entry.path);
        std::fs::create_dir_all(match entry.kind {
            OmittedEntryKind::Gitlink => path,
            OmittedEntryKind::Symlink => path.parent().unwrap().to_owned(),
        })
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// marker と記録を置いた、snapshot の形の一時ディレクトリ。`record` が None なら記録を置かない。
    /// 記録を読めれば、実際の snapshot と同じく gitlink の空ディレクトリと symlink の親も作る。
    fn fake_snapshot(tree: &str, record: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::create_dir_all(root.path().join("app/src")).unwrap();
        std::fs::write(root.path().join(".git").join(INDEX_SNAPSHOT_MARKER), tree).unwrap();
        if let Some(record) = record {
            std::fs::write(root.path().join(".git").join(SNAPSHOT_OMISSIONS), record).unwrap();
            create_recorded_directories(root.path(), record);
        }
        root
    }

    fn record(tree: &str, complete: bool, entries: &str) -> String {
        format!(
            "{{\"version\":1,\"tree\":\"{tree}\",\"complete\":{complete},\"entries\":[{entries}]}}"
        )
    }

    fn symlink(path: &str) -> String {
        format!("{{\"path\":\"{path}\",\"kind\":\"symlink\",\"target\":\"regular_file\"}}")
    }

    fn gitlink(path: &str) -> String {
        format!("{{\"path\":\"{path}\",\"kind\":\"gitlink\"}}")
    }

    #[test]
    fn tree_view_reads_complete_records_and_rejects_the_rest() {
        let tree = "0123456789012345678901234567890123456789";
        let entries = format!("{},{}", symlink("app/src/link.rs"), gitlink("vendor/sub"));
        let entries = entries.as_str();
        // 対照: snapshot でないディレクトリは、実際の checkout。
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(*tree_view(plain.path()), TreeView::Checkout);
        assert!(!tree_view(plain.path()).hides(Path::new("anything")));

        let snapshot = fake_snapshot(tree, Some(&record(tree, true, entries)));
        std::fs::write(snapshot.path().join("app/src/lib.rs"), "").unwrap();
        let view = tree_view(snapshot.path());
        assert!(matches!(*view, TreeView::Snapshot(_)));
        assert_eq!(
            view.omitted(),
            Some(
                &[
                    (
                        PathBuf::from("app/src/link.rs"),
                        OmittedKind::Symlink(LinkTarget::RegularFile)
                    ),
                    (PathBuf::from("vendor/sub"), OmittedKind::Gitlink),
                ][..]
            )
        );
        assert!(view.hides(Path::new("app/src/link.rs")));
        assert!(view.hides(Path::new("vendor/sub")));
        assert!(view.hides(Path::new("vendor/sub/Cargo.toml")));
        // 書き出さなかった symlink のディレクトリで開けない名前は、その別名でありうる。
        assert!(view.hides(Path::new("app/src/main.rs")));
        // 対照: 開けるファイルと、書き出さなかった symlink の無いディレクトリで開けない名前。
        assert!(!view.hides(Path::new("app/src/lib.rs")));
        assert!(!view.hides(Path::new("app/Cargo.toml")));
        assert!(!view.hides(Path::new("vendor/other.rs")));
        // 解析の root からの相対パスにする。
        assert_eq!(
            tree_view(&snapshot.path().join("app")).omitted(),
            Some(
                &[(
                    PathBuf::from("src/link.rs"),
                    OmittedKind::Symlink(LinkTarget::RegularFile)
                )][..]
            )
        );

        // 記録が無い・tree が違う・不完全・壊れている・版が違う・パスが不正。
        for snapshot in [
            fake_snapshot(tree, None),
            fake_snapshot(tree, Some(&record("f".repeat(40).as_str(), true, entries))),
            fake_snapshot(tree, Some(&record(tree, false, entries))),
            fake_snapshot(tree, Some("{")),
            fake_snapshot(
                tree,
                Some(&record(tree, true, entries).replace("\"version\":1", "\"version\":2")),
            ),
            fake_snapshot(
                tree,
                Some(&record(
                    tree,
                    true,
                    "{\"path\":\"app/../x\",\"kind\":\"gitlink\"}",
                )),
            ),
            // Windows では `\` も区切りで、`C:` は prefix になる。
            fake_snapshot(
                tree,
                Some(&record(
                    tree,
                    true,
                    "{\"path\":\"app\\\\..\\\\src\\\\link.rs\",\"kind\":\"gitlink\"}",
                )),
            ),
            fake_snapshot(
                tree,
                Some(&record(
                    tree,
                    true,
                    "{\"path\":\"C:/x\",\"kind\":\"gitlink\"}",
                )),
            ),
        ] {
            let view = tree_view(snapshot.path());
            assert_eq!(*view, TreeView::Unusable);
            assert!(view.hides(Path::new("app/src/lib.rs")));
        }
        // 記録が snapshot の中身と合わない: 書き出さなかった symlink の名前で別の要素を開ける・gitlink の
        // 空ディレクトリが無い・symlink の親ディレクトリが無い。
        let collided = fake_snapshot(tree, Some(&record(tree, true, &symlink("app/src/lib.rs"))));
        std::fs::write(collided.path().join("app/src/lib.rs"), "").unwrap();
        for (snapshot, entries) in [
            (collided, String::new()),
            (fake_snapshot(tree, None), gitlink("vendor/sub")),
            (fake_snapshot(tree, None), symlink("lib/x.rs")),
        ] {
            if !entries.is_empty() {
                std::fs::write(
                    snapshot.path().join(".git").join(SNAPSHOT_OMISSIONS),
                    record(tree, true, &entries),
                )
                .unwrap();
            }
            assert_eq!(*tree_view(snapshot.path()), TreeView::Unusable, "{entries}");
        }
        // 解析の root が gitlink かその下。
        let nested = fake_snapshot(tree, Some(&record(tree, true, &gitlink("app"))));
        assert_eq!(*tree_view(&nested.path().join("app")), TreeView::Unusable);
        assert_eq!(
            *tree_view(&nested.path().join("app/src")),
            TreeView::Unusable
        );
    }

    /// 問い合わせは snapshot のファイルシステムで開いて照らす。ディレクトリの別名はファイルシステムが
    /// 解決し、書き出さなかった symlink のディレクトリで開けない名前は、名前の比べ方に依らず隠す。
    #[test]
    fn hides_names_that_cannot_be_opened_beside_omitted_symlinks() {
        let tree = "0123456789012345678901234567890123456789";
        let snapshot = fake_snapshot(
            tree,
            Some(&record(
                tree,
                true,
                &format!(
                    "{},{}",
                    symlink("app/src/bootstrap-link.rs"),
                    gitlink("lib/sub")
                ),
            )),
        );
        std::fs::write(snapshot.path().join("app/src/lib.rs"), "").unwrap();
        std::fs::create_dir_all(snapshot.path().join("app/src/nested")).unwrap();
        let view = tree_view(snapshot.path());
        for path in [
            // NTFS では symlink の短い名前 (8.3) を `main.rs` にでき、名前を比べても見つからない。
            "app/src/main.rs",
            "app/src/bootstrap-link.rs",
            "app/src/missing/mod.rs",
            "./app/src/main.rs",
            "lib/sub",
            "lib/sub/Cargo.toml",
            // `..` は、途中に symlink があると OS によって辿り方が違う。
            "app/src/nested/../lib.rs",
        ] {
            assert!(view.hides(Path::new(path)), "{path}");
        }
        assert!(view.hides(&snapshot.path().join("app/src/lib.rs")));
        // 対照: 開けるファイル・ディレクトリ、ファイルの下、書き出さなかった symlink の無いディレクトリで
        // 開けない名前。
        for path in [
            "",
            "app/src/lib.rs",
            "app/src/nested",
            "app/src/nested/mod.rs",
            "app/src/lib.rs/x.rs",
            "app/Cargo.toml",
            "lib/other.rs",
            "other/main.rs",
        ] {
            assert!(!view.hides(Path::new(path)), "{path}");
        }
        // 大文字小文字を区別しないファイルシステムでは、ディレクトリの別名も同じディレクトリとして開く。
        if snapshot.path().join("APP").exists() {
            assert!(view.hides(Path::new("APP/SRC/main.rs")));
            assert!(view.hides(Path::new("LIB/SUB/Cargo.toml")));
            assert!(!view.hides(Path::new("APP/SRC/LIB.RS")));
            // 書き出さなかった symlink の名前で、綴りだけが違う要素を開けるなら、どちらが残るか分からない。
            let collided =
                fake_snapshot(tree, Some(&record(tree, true, &symlink("app/src/LIB.rs"))));
            std::fs::write(collided.path().join("app/src/lib.rs"), "").unwrap();
            assert_eq!(*tree_view(collided.path()), TreeView::Unusable);
        }
        // Windows は名前の末尾のドットを除いて開く。
        #[cfg(windows)]
        {
            assert!(view.hides(Path::new("app/src./main.rs")));
            assert!(!view.hides(Path::new("app/src./lib.rs")));
        }
    }

    /// 記録の位置は snapshot のファイルシステムで開いて決める。解析の root を綴りだけが違う名前で
    /// 渡しても、記録の要素を落とさず root からの相対にする。
    #[test]
    fn tree_view_places_records_with_the_snapshot_filesystem() {
        let tree = "0123456789012345678901234567890123456789";
        let snapshot = fake_snapshot(
            tree,
            Some(&record(tree, true, &symlink("Mixed/src/main.rs"))),
        );
        let expected = [(
            PathBuf::from("src/main.rs"),
            OmittedKind::Symlink(LinkTarget::RegularFile),
        )];
        assert_eq!(
            tree_view(&snapshot.path().join("Mixed")).omitted(),
            Some(&expected[..])
        );
        if snapshot.path().join("mixed").exists() {
            let view = tree_view(&snapshot.path().join("mixed"));
            assert_eq!(view.omitted(), Some(&expected[..]));
            assert!(view.hides(Path::new("src/main.rs")));
            assert!(view.hides(Path::new("SRC/MAIN.RS")));
            // root が、綴りだけが違う gitlink の下なら使えない。
            let nested = fake_snapshot(tree, Some(&record(tree, true, &gitlink("Vendor"))));
            assert_eq!(
                *tree_view(&nested.path().join("vendor")),
                TreeView::Unusable
            );
        }
    }

    #[test]
    fn forgetting_tree_views_rereads_a_snapshot_at_the_same_path() {
        let tree = "0123456789012345678901234567890123456789";
        let snapshot = fake_snapshot(tree, Some(&record(tree, true, "")));
        assert!(matches!(*tree_view(snapshot.path()), TreeView::Snapshot(_)));
        assert_eq!(tree_view(snapshot.path()).omitted(), Some(&[][..]));
        // 同じパスに、書き出さなかった要素のある snapshot を作り直す。
        let entries = record(tree, true, &gitlink("vendor/sub"));
        std::fs::write(
            snapshot.path().join(".git").join(SNAPSHOT_OMISSIONS),
            &entries,
        )
        .unwrap();
        create_recorded_directories(snapshot.path(), &entries);
        forget_tree_views();
        assert_eq!(
            tree_view(snapshot.path()).omitted(),
            Some(&[(PathBuf::from("vendor/sub"), OmittedKind::Gitlink)][..])
        );
        // snapshot でなかったパスが、後で snapshot になる場合も同じ。
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(*tree_view(plain.path()), TreeView::Checkout);
        std::fs::create_dir_all(plain.path().join(".git")).unwrap();
        std::fs::write(plain.path().join(".git").join(INDEX_SNAPSHOT_MARKER), tree).unwrap();
        forget_tree_views();
        assert_eq!(*tree_view(plain.path()), TreeView::Unusable);
    }

    #[test]
    fn accepts_normal_revisions() {
        for revision in ["HEAD", "HEAD~3", "main", "origin/main", "v1.0.0", "abc1234"] {
            assert!(validate_git_revision(revision, "--base").is_ok());
        }
    }

    #[test]
    fn rejects_unsafe_revisions() {
        for revision in ["", "--output=/tmp/pwn", "-p", "HEAD\0foo"] {
            assert!(validate_git_revision(revision, "--base").is_err());
        }
    }

    #[test]
    fn normalize_workspace_separators_keeps_forward_slashes() {
        assert_eq!(
            normalize_workspace_separators("src/engine/lib.rs"),
            "src/engine/lib.rs"
        );
    }

    #[cfg(windows)]
    #[test]
    fn normalize_workspace_separators_replaces_backslashes_on_windows() {
        assert_eq!(
            normalize_workspace_separators(r"src\engine\lib.rs"),
            "src/engine/lib.rs"
        );
    }

    /// Unix ではバックスラッシュもファイル名に使える文字なので置き換えない
    #[cfg(unix)]
    #[test]
    fn normalize_workspace_separators_keeps_backslashes_on_unix() {
        assert_eq!(normalize_workspace_separators(r"src\lib.rs"), r"src\lib.rs");
    }
}
