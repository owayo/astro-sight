use serde::Serialize;

/// 名前の残存と、置換先との同値性は別々の証拠として報告する。
#[derive(Debug, Clone, Serialize)]
pub struct PythonImportRebinding {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub target: PythonRebindingTarget,
    pub equivalence: RebindingEquivalence,
    pub same_file_uses: Vec<PythonRebindingUse>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PythonRebindingTarget {
    pub file: String,
    pub name: String,
    /// refs と同じく 0 始まり。
    pub line: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PythonRebindingUse {
    pub line: usize,
    pub column: usize,
    /// false の位置は、同名の出現であることだけを確認したもの。
    pub binding_verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RebindingEquivalence {
    Proven,
    Unverified,
}
