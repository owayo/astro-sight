//! Git を呼び出す全レイヤーで共有する入力契約。
//!
//! CLI (`commands`) と解析エンジン (`engine`) の双方から利用するため、どちらか一方へ
//! 置くと逆依存になる。セキュリティ境界である revision 検証と既定 revision は、この
//! 中立モジュールを唯一の正本とする。

use anyhow::{Result, bail};

use crate::error::{AstroError, ErrorCode};

/// cochange の既定 base。context / impact / review / dead-code と揃えて
/// 「未コミットの作業ツリー変更」を既定の解析対象にする。
pub const DEFAULT_BLAME_BASE: &str = "HEAD";

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
pub(crate) const GIT_DIFF_PARSEABLE_OUTPUT_ARGS: [&str; 5] = [
    "--no-ext-diff",
    "--no-color",
    "--no-textconv",
    "--src-prefix=a/",
    "--dst-prefix=b/",
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
