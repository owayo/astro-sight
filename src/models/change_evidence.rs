use serde::{Deserialize, Serialize};

/// 構文の変更箇所の証拠。互換性・影響・停止判定には使用しない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeEvidence {
    pub analysis: ChangeAnalysis,
    pub changed_parts: Vec<ChangedPart>,
    pub annotation_only: Option<bool>,
    pub call_shape_unchanged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_reason: Option<ChangeUnknownReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeAnalysis {
    Known,
    Unknown,
}

/// 宣言中の全トークンを分類する固定語彙。列挙順で出力する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangedPart {
    ReturnAnnotation,
    ParamAnnotation,
    Params,
    DefaultValue,
    Visibility,
    ReferencedType,
    Initializer,
    Body,
    Decorator,
    OtherDeclaration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeUnknownReason {
    SourceUnavailable,
    ParseError,
    UnsupportedLanguage,
    UnsupportedDeclarationKind,
    UnsupportedSyntax,
    DeclarationPairingAmbiguous,
    DeclarationPairingUnavailable,
    ResourceLimit,
}

impl ChangeEvidence {
    /// 構文比較に成功した宣言へ、別途確かめた型依存の変更箇所を加える。
    pub(crate) fn add_type_parts(&mut self, parts: impl IntoIterator<Item = ChangedPart>) {
        if self.analysis != ChangeAnalysis::Known {
            return;
        }
        let was_empty = self.changed_parts.is_empty();
        self.changed_parts.extend(parts);
        self.changed_parts.sort_unstable();
        self.changed_parts.dedup();
        if was_empty && !self.changed_parts.is_empty() {
            self.annotation_only = Some(true);
        }
    }

    pub(crate) fn unknown(reason: ChangeUnknownReason) -> Self {
        Self {
            analysis: ChangeAnalysis::Unknown,
            changed_parts: Vec::new(),
            annotation_only: None,
            call_shape_unchanged: None,
            unknown_reason: Some(reason),
        }
    }
}
