use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ReferenceEvidence {
    pub classification: ReferenceClassification,
    pub attribution: &'static str,
    pub scope: ReferenceScanScope,
    pub references: ReferenceObservations,
    pub external_entry: ExternalEntryEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceClassification {
    NoReferencesInScope,
    TestReferencesOnly,
    NoResolvedDirectCallers,
    Unverified,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferenceScanScope {
    pub root: &'static str,
    pub selection: &'static str,
    pub total_files: usize,
    pub files: Vec<String>,
    pub omitted_files: usize,
    pub complete: bool,
    pub generated_included: bool,
    pub diagnostics: Vec<crate::models::truncation::TruncationInfo>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReferenceObservations {
    pub total: usize,
    pub production_total: usize,
    pub test_total: usize,
    pub shown: usize,
    pub omitted: usize,
    pub sites: Vec<ReferenceEvidenceSite>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ReferenceEvidenceSite {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub test: bool,
    pub test_rules: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExternalEntryEvidence {
    pub status: ExternalEntryStatus,
    pub registrations: Vec<ExternalEntryRegistration>,
    pub unproven_reasons: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalEntryStatus {
    Proven,
    #[default]
    Unproven,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ExternalEntryRegistration {
    pub kind: &'static str,
    pub file: String,
    pub line: usize,
    pub script: String,
    pub target: String,
}
