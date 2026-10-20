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

pub(crate) fn git_command_for_dir(dir: impl AsRef<std::path::Path>) -> std::process::Command {
    let dir = dir.as_ref();
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if canonical
        .ancestors()
        .any(|parent| parent.join(".git").join(INDEX_SNAPSHOT_MARKER).is_file())
    {
        isolated_git_command()
    } else {
        std::process::Command::new("git")
    }
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
pub(crate) const GIT_DIFF_PARSEABLE_OUTPUT_ARGS: [&str; 11] = [
    "--no-ext-diff",
    "--no-color",
    "--no-textconv",
    "--submodule=short",
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
