//! 新規公開名の参照を、分類とは独立して差分の内外に分ける。

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AddedReferenceSummary {
    pub file: String,
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_module: Option<String>,
    pub scope: AddedReferenceScope,
    pub in_diff: AddedReferenceCount,
    pub outside_diff: AddedReferenceCount,
    pub scan: AddedReferenceScan,
}

/// 名前の出現を数える範囲。束縛・到達性・外部利用の証明ではない。
#[derive(Debug, Clone, Serialize)]
pub struct AddedReferenceScope {
    pub new_side: AddedReferenceSource,
    pub partition: &'static str,
    pub references: &'static str,
    pub attribution: &'static str,
    pub selection: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AddedReferenceSource {
    WorkingTree,
    Index,
}

#[derive(Debug, Clone, Serialize)]
pub struct AddedReferenceCount {
    /// 解析できた範囲の件数。未完了のときは下限でしかない。
    pub observed: usize,
    /// 入力が完全なときだけ確定する。未完了は null。
    pub total: Option<usize>,
    pub complete: bool,
    pub references: Vec<AddedReferenceLocation>,
    /// 標本から省いた観測済みの参照数。
    pub omitted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct AddedReferenceLocation {
    pub path: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AddedReferenceScan {
    pub failed_files: usize,
    pub parse_error_files: usize,
    pub scan_failed: bool,
    pub unknown_references: usize,
    pub truncations: Vec<crate::models::truncation::TruncationInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_files_skipped: Option<crate::models::skip::SkippedFiles>,
}
