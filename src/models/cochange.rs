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

/// Options controlling co-change analysis behaviour.
#[derive(Debug, Clone)]
pub struct CoChangeOptions {
    /// Maximum number of commits to walk when computing statistics.
    pub lookback: usize,
    /// Minimum confidence (ratio) required for a pair to be emitted (0.0..=1.0).
    pub min_confidence: f64,
    /// Minimum number of shared commits required for a pair to be emitted.
    pub min_samples: usize,
    /// Commits touching more files than this threshold are excluded from the
    /// statistics (initial dumps, bulk refactors, generated artefacts).
    pub max_files_per_commit: usize,
    /// Limit the commit walk to history reachable from
    /// `merge-base(HEAD, <default branch>)`. Falls back to the full walk when
    /// no default branch can be inferred.
    pub bounded_by_merge_base: bool,
    /// Drop pairs when either file is missing from the current `HEAD` tree
    /// (renamed/deleted files).
    pub skip_deleted_files: bool,
    /// Optional filter: only keep pairs that include this file.
    pub filter_file: Option<String>,
}

impl Default for CoChangeOptions {
    fn default() -> Self {
        Self {
            lookback: 200,
            min_confidence: 0.7,
            min_samples: 2,
            max_files_per_commit: 30,
            bounded_by_merge_base: true,
            skip_deleted_files: true,
            filter_file: None,
        }
    }
}
