use serde::{Deserialize, Serialize};

/// A parsed hunk from a unified diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HunkInfo {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
}

/// A symbol affected by a change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedSymbol {
    pub name: String,
    pub kind: String,
    pub change_type: String,
}

/// A detected signature change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureChange {
    pub name: String,
    pub old_signature: String,
    pub new_signature: String,
}

/// A caller impacted by a change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactedCaller {
    pub path: String,
    pub name: String,
    pub line: usize,
}

/// A parsed diff file entry with change and hunk info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffFile {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<HunkInfo>,
}

/// The impact analysis for a single changed file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileImpact {
    pub path: String,
    pub hunks: Vec<HunkInfo>,
    pub affected_symbols: Vec<AffectedSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub signature_changes: Vec<SignatureChange>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub impacted_callers: Vec<ImpactedCaller>,
}

/// The context (impact analysis) response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextResult {
    pub changes: Vec<FileImpact>,
}
