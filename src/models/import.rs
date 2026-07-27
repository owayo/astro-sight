use serde::{Deserialize, Serialize};

/// import 文の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportKind {
    Import,
    Use,
    Include,
    Require,
}

/// ソースから抽出した単一の import エッジ。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportEdge {
    /// import されたモジュール、パス、またはパッケージ
    #[serde(rename = "src")]
    pub source: String,
    /// 行番号（0 始まり）
    #[serde(rename = "ln")]
    pub line: usize,
    /// import の種類
    pub kind: ImportKind,
    /// import 文のソーステキスト
    #[serde(rename = "ctx")]
    pub context: String,
}

/// 単一ファイルの import 抽出結果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportsResult {
    #[serde(rename = "lang")]
    pub language: String,
    pub imports: Vec<ImportEdge>,
}
