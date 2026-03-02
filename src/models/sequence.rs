use serde::{Deserialize, Serialize};

/// Result of sequence diagram generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceDiagramResult {
    #[serde(rename = "lang")]
    pub language: String,
    pub participants: Vec<String>,
    pub diagram: String,
}
