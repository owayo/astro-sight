use serde::{Deserialize, Serialize};

fn is_false(value: &bool) -> bool {
    !*value
}

/// astro-sight エンジンへのリクエスト。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AstgenRequest {
    pub command: Command,
    #[serde(default)]
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// 関数名フィルタ（calls コマンド用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// 検索するシンボル名（refs コマンドの単一検索用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// refs バッチ検索用のシンボル名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
    /// 検索対象ディレクトリ（refs/context コマンド用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// glob パターンフィルタ（refs コマンド用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// generated 判定されたファイルも refs の走査対象に含める。
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_generated: bool,
    /// refs の出力件数上限。数値または `"unlimited"`。省略時は既定値 (100)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<crate::models::result_summary::LimitValue>,
    /// refs 出力全体の推定トークン予算。数値または `"unlimited"`。省略時は既定値 (3000)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<crate::models::result_summary::LimitValue>,
    /// diff 入力（session 経由の context コマンド用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// lint ルール（session 経由の lint コマンド用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<crate::models::lint::Rule>>,
    /// 共変更分析に必要な最小確信度（blame モード）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<f64>,
    /// 共変更分析に必要な最小共有コミット数（blame モード）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_samples: Option<usize>,
    /// 変更ファイル数がこの閾値を超えるコミットは除外する（blame モード）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_files_per_commit: Option<usize>,
    /// blame ベースの共変更分析で起点にするファイル（リポジトリルート相対）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_files: Option<Vec<String>>,
    /// blame ベースの共変更分析の基準 revision（既定値: HEAD = 未コミットの作業ツリー変更）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// 追加で除外するディレクトリ名 (context コマンドの impact cross-file 解析で適用)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_dirs: Vec<String>,
    /// 追加で除外する glob パターン (context コマンドの impact cross-file 解析で適用)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_globs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    Ast,
    Symbols,
    Doctor,
    Calls,
    Refs,
    Context,
    Imports,
    Lint,
    Sequence,
    Cochange,
}
