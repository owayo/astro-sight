use serde::{Deserialize, Serialize};

/// シーケンス図の生成結果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceDiagramResult {
    #[serde(rename = "lang")]
    pub language: String,
    pub participants: Vec<String>,
    pub diagram: String,
}
