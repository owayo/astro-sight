use serde::{Deserialize, Serialize};

use super::result_summary::ResultLimitKind;

fn is_false(value: &bool) -> bool {
    !*value
}

/// Directory scans that omitted generated files.
///
/// `generated` is always the complete count. `paths` is a bounded, sorted sample
/// so callers can diagnose the omission without making output size depend on a
/// repository's generated-file count. A missing `skipped` field means zero files
/// were omitted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkippedFiles {
    pub generated: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "is_false", default)]
    pub truncated: bool,
    /// 出力の上限に収めるために `paths` をさらに縮めたときだけ付く理由 (`token_budget`)。
    /// `truncated` だけでは、固定の件数上限で切れたのか予算で切れたのかを区別できない。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub paths_limited_by: Option<ResultLimitKind>,
}

impl SkippedFiles {
    /// 代表パスを先頭 `cap` 件までに絞った写し。実際に縮めたときだけ `limited_by` を記録する。
    ///
    /// `generated` (全件数) はそのまま保ち、`truncated` は残したパスの件数で付け直す。
    /// 出力の予算に収めるために標本だけを縮める用途 ([`crate::output::limit`])。
    pub fn with_path_cap(&self, cap: usize, limited_by: ResultLimitKind) -> Self {
        if cap >= self.paths.len() {
            return self.clone();
        }
        let paths = self.paths[..cap].to_vec();
        Self {
            generated: self.generated,
            truncated: self.generated > paths.len(),
            paths,
            paths_limited_by: Some(limited_by),
        }
    }
}

/// `--git` 指定だが解析対象の diff を取得できずスキップした理由を機械可読に伝える。
///
/// git 管理外ディレクトリ (または worktree 外) で `--git` が要求されたケースと、
/// `--base` を省略したのに HEAD がまだコミットを指していない (unborn) ケースを
/// 「想定内の skip」として表現する。真のエラー (壊れた repo / 不正 base /
/// git 実行不能 / 権限不足) は従来どおり `exit 1` のエラー JSON を返し、ここには
/// 到達しない。
///
/// 出力契約は **追加のみ** で後方互換: 各結果型に `Option<SkipInfo>` として乗り、
/// `None` のときは serialize されない。既存パーサに対し JSON 加法的・非破壊。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkipInfo {
    /// 機械判定用の安定キー。例: `"not_git_repository"`。
    pub reason: String,
    /// スキップ要因のソース。例: `"git"`。
    pub source: String,
    /// 人間向けの補足メッセージ。
    pub message: String,
}

impl SkipInfo {
    /// `--dir` が git worktree 内でないために解析対象 diff を取得できなかった skip。
    ///
    /// `not a git repository` (管理外) と `is-inside-work-tree=false` (bare repo の
    /// `.git` 内など worktree 外) の双方を表す。reason は両ケースで安定キー
    /// `"not_git_repository"` に統一し、機械判定を単純化する。
    pub fn not_git_repository() -> Self {
        Self {
            reason: "not_git_repository".to_string(),
            source: "git".to_string(),
            message: "--git was requested but --dir is not inside a git worktree".to_string(),
        }
    }

    /// `--base` を省略した `--git` で、HEAD がまだコミットを指していない (unborn) ため
    /// 比較の基点が無い skip。
    ///
    /// `git init` の直後や `git checkout --orphan` の直後の正常な状態なので、git 管理外と
    /// 同じく解析を飛ばす。`--base` を明示した場合は skip せず、コミットに解決できなければ
    /// `HEAD` でも従来どおりエラーにする。
    pub fn unborn_head() -> Self {
        Self {
            reason: "unborn_head".to_string(),
            source: "git".to_string(),
            message: "--git was requested without --base, but HEAD does not point to a commit yet"
                .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 代表パスだけを縮め、全件数は保ち、縮めたら `truncated` と縮めた理由を付ける。
    #[test]
    fn with_path_cap_keeps_the_count_and_declares_the_shrink() {
        let budget = ResultLimitKind::TokenBudget;
        let full = SkippedFiles {
            generated: 3,
            paths: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
            truncated: false,
            paths_limited_by: None,
        };
        // 対照: パスの件数以上の上限では元のまま (truncated も理由も付けない)。
        assert_eq!(full.with_path_cap(3, budget), full);
        assert_eq!(full.with_path_cap(50, budget), full);
        assert_eq!(
            serde_json::to_value(&full).unwrap(),
            serde_json::json!({"generated": 3, "paths": ["a.rs", "b.rs", "c.rs"]})
        );

        assert_eq!(
            full.with_path_cap(1, budget),
            SkippedFiles {
                generated: 3,
                paths: vec!["a.rs".into()],
                truncated: true,
                paths_limited_by: Some(budget),
            }
        );
        assert_eq!(
            serde_json::to_value(full.with_path_cap(0, budget)).unwrap(),
            serde_json::json!({"generated": 3, "truncated": true, "paths_limited_by": "token_budget"})
        );

        // 固定の上限 (50 件) で既に切れていたものを、さらに縮めた場合も理由を付ける。
        let capped = SkippedFiles {
            generated: 60,
            paths: vec!["a.rs".into(), "b.rs".into()],
            truncated: true,
            paths_limited_by: None,
        };
        let shrunk = capped.with_path_cap(1, budget);
        assert!(shrunk.truncated);
        assert_eq!(shrunk.generated, 60);
        assert_eq!(shrunk.paths_limited_by, Some(budget));
    }
}
