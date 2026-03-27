use serde::Serialize;

use super::impact::ContextResult;

/// review コマンドの統合レスポンス。
#[derive(Debug, Clone, Serialize)]
pub struct ReviewResult {
    pub impact: ContextResult,
    pub missing_cochanges: Vec<MissingCochange>,
    pub api_changes: ApiChanges,
    pub dead_symbols: Vec<DeadSymbol>,
}

/// cochange で検出された「一緒に変更されるはずだが diff に含まれないファイル」。
#[derive(Debug, Clone, Serialize)]
pub struct MissingCochange {
    pub file: String,
    pub expected_with: String,
    pub confidence: f64,
}

/// 公開シンボルの変更サマリ。
#[derive(Debug, Clone, Serialize)]
pub struct ApiChanges {
    pub added: Vec<ApiSymbol>,
    pub removed: Vec<ApiSymbol>,
    pub modified: Vec<ApiSymbolChange>,
}

/// 公開シンボル情報。
#[derive(Debug, Clone, Serialize)]
pub struct ApiSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
}

/// シグネチャが変更された公開シンボル。
#[derive(Debug, Clone, Serialize)]
pub struct ApiSymbolChange {
    pub name: String,
    pub kind: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_signature: Option<String>,
}

/// 参照カウント 0 の公開シンボル。
#[derive(Debug, Clone, Serialize)]
pub struct DeadSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
}
