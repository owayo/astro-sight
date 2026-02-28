use serde::{Deserialize, Serialize};

/// A pair of files that frequently change together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoChangeEntry {
    pub file_a: String,
    pub file_b: String,
    /// Number of commits where both files changed
    pub co_changes: usize,
    /// Total changes for file_a
    pub total_changes_a: usize,
    /// Total changes for file_b
    pub total_changes_b: usize,
    /// Confidence score: co_changes / max(total_a, total_b)
    pub confidence: f64,
}

/// Result of co-change analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoChangeResult {
    pub entries: Vec<CoChangeEntry>,
    /// Number of commits analyzed
    pub commits_analyzed: usize,
}
