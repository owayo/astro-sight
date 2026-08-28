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
}
