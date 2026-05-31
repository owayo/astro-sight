use serde::{Deserialize, Serialize};

/// `--git` 指定だが解析対象の diff を取得できずスキップした理由を機械可読に伝える。
///
/// git 管理外ディレクトリ (または worktree 外) で `--git` が要求されたケースを
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
}
