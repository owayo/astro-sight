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
    /// snapshot で、書き出さなかった要素の記録を読めた。パスは `dir` からの相対。
    Snapshot(Vec<(std::path::PathBuf, OmittedKind)>),
    /// snapshot だが、記録が無い・読めない・tree が合わない・不完全か、`dir` 自身が書き出さなかった
    /// 要素の下にある。
    Unusable,
}

impl TreeView {
    /// `dir` の下の、書き出さなかった要素。通常の checkout なら空、記録を使えなければ None。
    pub(crate) fn omitted(&self) -> Option<&[(std::path::PathBuf, OmittedKind)]> {
        match self {
            Self::Checkout => Some(&[]),
            Self::Snapshot(entries) => Some(entries),
            Self::Unusable => None,
        }
    }

    /// `relative` (`dir` からの相対) か、その途中のディレクトリが書き出さなかった要素で、有無や種類が
    /// 実際の checkout と違いうるか。記録を使えなければ常に真。大文字小文字と Unicode の正規化だけが
    /// 違う名前も、それを区別しないファイルシステムでは同じ要素なので、同じとみなす。
    pub(crate) fn hides(&self, relative: &std::path::Path) -> bool {
        self.omitted().is_none_or(|entries| {
            entries.iter().any(|(path, _)| {
                let mut query = relative.components();
                path.components().all(|part| {
                    query
                        .next()
                        .is_some_and(|other| same_entry_name(part.as_os_str(), other.as_os_str()))
                })
            })
        })
    }
}

/// 大文字小文字と Unicode の正規化を区別しないファイルシステムで、同じ名前になりうるか。区別する
/// ファイルシステムでも同じとみなす (証明をやめる側)。
fn same_entry_name(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    use unicode_normalization::UnicodeNormalization;
    let fold = |name: &str| -> String {
        name.nfc()
            .collect::<String>()
            .to_lowercase()
            .nfc()
            .collect()
    };
    match (left.to_str(), right.to_str()) {
        (Some(left), Some(right)) => fold(left) == fold(right),
        _ => left == right,
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
        let prefix = canonical.strip_prefix(root).ok()?;
        let mut entries = Vec::new();
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
            // 解析の root 自身が、書き出さなかった要素 (の下) なら中身を確かめられない。
            if prefix.starts_with(&path) {
                return None;
            }
            let kind = match (entry.kind, entry.target) {
                (OmittedEntryKind::Symlink, Some(target)) => OmittedKind::Symlink(target),
                (OmittedEntryKind::Gitlink, None) => OmittedKind::Gitlink,
                _ => return None,
            };
            if let Ok(relative) = path.strip_prefix(prefix) {
                entries.push((relative.to_owned(), kind));
            }
        }
        Some(TreeView::Snapshot(entries))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// marker と記録を置いた、snapshot の形の一時ディレクトリ。`record` が None なら記録を置かない。
    fn fake_snapshot(tree: &str, record: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::create_dir_all(root.path().join("app/src")).unwrap();
        std::fs::write(root.path().join(".git").join(INDEX_SNAPSHOT_MARKER), tree).unwrap();
        if let Some(record) = record {
            std::fs::write(root.path().join(".git").join(SNAPSHOT_OMISSIONS), record).unwrap();
        }
        root
    }

    #[test]
    fn tree_view_reads_complete_records_and_rejects_the_rest() {
        let tree = "0123456789012345678901234567890123456789";
        let record = |tree: &str, complete: bool, entries: &str| {
            format!(
                "{{\"version\":1,\"tree\":\"{tree}\",\"complete\":{complete},\"entries\":[{entries}]}}"
            )
        };
        let entries = "{\"path\":\"app/src/link.rs\",\"kind\":\"symlink\",\"target\":\"regular_file\"},{\"path\":\"vendor/sub\",\"kind\":\"gitlink\"}";
        // 対照: snapshot でないディレクトリは、実際の checkout。
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(*tree_view(plain.path()), TreeView::Checkout);
        assert!(!tree_view(plain.path()).hides(Path::new("anything")));

        let snapshot = fake_snapshot(tree, Some(&record(tree, true, entries)));
        let view = tree_view(snapshot.path());
        assert_eq!(
            *view,
            TreeView::Snapshot(vec![
                (
                    PathBuf::from("app/src/link.rs"),
                    OmittedKind::Symlink(LinkTarget::RegularFile)
                ),
                (PathBuf::from("vendor/sub"), OmittedKind::Gitlink),
            ])
        );
        assert!(view.hides(Path::new("app/src/link.rs")));
        assert!(view.hides(Path::new("vendor/sub/Cargo.toml")));
        assert!(!view.hides(Path::new("app/src/lib.rs")));
        // 解析の root からの相対パスにする。
        assert_eq!(
            *tree_view(&snapshot.path().join("app")),
            TreeView::Snapshot(vec![(
                PathBuf::from("src/link.rs"),
                OmittedKind::Symlink(LinkTarget::RegularFile)
            )])
        );

        // 記録が無い・tree が違う・不完全・壊れている・版が違う、root が書き出さなかった要素の下。
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
        let nested = fake_snapshot(
            tree,
            Some(&record(
                tree,
                true,
                "{\"path\":\"app\",\"kind\":\"gitlink\"}",
            )),
        );
        assert_eq!(*tree_view(&nested.path().join("app")), TreeView::Unusable);
    }

    #[test]
    fn hides_names_that_differ_only_in_case_or_normalization() {
        let tree = "0123456789012345678901234567890123456789";
        let snapshot = fake_snapshot(
            tree,
            Some(&format!(
                "{{\"version\":1,\"tree\":\"{tree}\",\"complete\":true,\"entries\":[{{\"path\":\"app/src/MAIN.rs\",\"kind\":\"symlink\",\"target\":\"regular_file\"}},{{\"path\":\"app/caf\u{e9}\",\"kind\":\"gitlink\"}}]}}"
            )),
        );
        let view = tree_view(snapshot.path());
        for path in [
            "app/src/MAIN.rs",
            "app/src/main.rs",
            "APP/Src/Main.RS",
            "app/cafe\u{301}/Cargo.toml",
        ] {
            assert!(view.hides(Path::new(path)), "{path}");
        }
        // 対照: 別の名前は隠さない。
        for path in ["app/src/lib.rs", "app/src/main.rs.bak", "app/cafe"] {
            assert!(!view.hides(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn forgetting_tree_views_rereads_a_snapshot_at_the_same_path() {
        let tree = "0123456789012345678901234567890123456789";
        let record = |entries: &str| {
            format!(
                "{{\"version\":1,\"tree\":\"{tree}\",\"complete\":true,\"entries\":[{entries}]}}"
            )
        };
        let snapshot = fake_snapshot(tree, Some(&record("")));
        assert_eq!(*tree_view(snapshot.path()), TreeView::Snapshot(Vec::new()));
        // 同じパスに、書き出さなかった要素のある snapshot を作り直す。
        std::fs::write(
            snapshot.path().join(".git").join(SNAPSHOT_OMISSIONS),
            record("{\"path\":\"vendor/sub\",\"kind\":\"gitlink\"}"),
        )
        .unwrap();
        forget_tree_views();
        assert_eq!(
            *tree_view(snapshot.path()),
            TreeView::Snapshot(vec![(PathBuf::from("vendor/sub"), OmittedKind::Gitlink)])
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
