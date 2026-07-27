use serde::{Deserialize, Serialize};

/// lint ルールの重大度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// YAML から読み込む lint ルール定義。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// 一意なルール識別子
    pub id: String,
    /// 対象言語（例: "rust", "javascript"）
    pub language: String,
    /// 重大度
    pub severity: Severity,
    /// 人が読めるメッセージ
    pub message: String,
    /// tree-sitter の S 式クエリ（pattern とは排他）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// 識別子と照合する単純なテキストパターン（query とは排他）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// 単一のパターン一致結果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternMatch {
    /// 一致したルール ID
    pub rule_id: String,
    /// 重大度
    pub severity: Severity,
    /// メッセージ
    pub message: String,
    /// 行番号（0 始まり）
    pub line: usize,
    /// 列番号（0 始まり）
    pub column: usize,
    /// 一致したテキスト
    pub matched_text: String,
}

/// 単一ファイルの lint 結果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintResult {
    #[serde(rename = "lang")]
    pub language: String,
    pub matches: Vec<PatternMatch>,
    /// 除外または不正なルールに関する警告
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}
