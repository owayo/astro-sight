use serde::Serialize;

/// 宣言名以外の構造が等しい対応候補。API 互換性や改名の確定を意味しない。
#[derive(Debug, Clone, Serialize)]
pub struct RenameCandidate {
    pub old: RenameDeclaration,
    pub new: RenameDeclaration,
    pub status: RenameCandidateStatus,
    pub evidence: Vec<RenameEvidence>,
    pub ambiguous: bool,
    pub new_candidates_for_old: usize,
    pub old_candidates_for_new: usize,
    pub old_api_presence: OldApiPresence,
}

#[derive(Debug, Clone, Serialize)]
pub struct RenameDeclaration {
    pub file: String,
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameCandidateStatus {
    Candidate,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameEvidence {
    NameIndependentAstEqual,
}

/// 旧名の公開経路の証拠。候補の対応付けとは別に報告する。
#[derive(Debug, Clone, Serialize)]
pub struct OldApiPresence {
    pub status: OldApiPresenceStatus,
    pub compatibility: RenameCompatibility,
    pub evidence: Vec<RenameDeclaration>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OldApiPresenceStatus {
    Unverified,
    DirectReexportPresent,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameCompatibility {
    Unverified,
}

#[derive(Debug, Clone, Serialize)]
pub struct RenameCandidateDiagnostic {
    pub file: Option<String>,
    pub side: Option<RenameSourceSide>,
    pub reason: RenameDiagnosticReason,
    pub omitted: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameSourceSide {
    Old,
    New,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameDiagnosticReason {
    SourceUnavailable,
    UnsupportedLanguage,
    SourceSizeLimit,
    TotalSourceSizeLimit,
    ParseError,
    NodeLimit,
    DeclarationUnsupported,
    CandidateLimit,
}
