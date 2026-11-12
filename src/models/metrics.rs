use serde::{Deserialize, Serialize};

use super::{skip::SkippedFiles, truncation::TruncationInfo};

#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ComponentUnit {
    #[default]
    File,
    Directory,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MetricsOptions {
    /// Directory to analyze
    pub dir: String,
    /// Restrict the source inventory; targets outside it remain unresolved
    pub glob: Option<String>,
    /// Component granularity: file (default) or directory
    pub component: ComponentUnit,
    /// Directory depth relative to dir (positive, default 1); applies to directory components
    pub component_depth: usize,
    /// Include generated sources
    pub include_generated: bool,
    /// Compare the same scope against this Git commit, tag or revision
    pub base: Option<String>,
}

impl Default for MetricsOptions {
    fn default() -> Self {
        Self {
            dir: ".".into(),
            glob: None,
            component: ComponentUnit::File,
            component_depth: 1,
            include_generated: false,
            base: None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct ComplexityStats {
    pub count: usize,
    pub total: usize,
    pub average: f64,
    pub max: usize,
}

impl ComplexityStats {
    pub(crate) fn add(&mut self, value: usize) {
        self.count += 1;
        self.total += value;
        self.max = self.max.max(value);
        self.average = self.total as f64 / self.count as f64;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FunctionComplexity {
    pub name: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub complexity: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileMetrics {
    pub path: String,
    pub language: String,
    pub complexity: ComplexityStats,
    pub functions: Vec<FunctionComplexity>,
    pub fan_in: usize,
    pub fan_out: usize,
    /// 自己を含む推移的依存数。
    pub depends_on: usize,
    /// 自己を含む推移的被依存数。
    pub used_by: usize,
    /// fan_out / (fan_in + fan_out)。孤立ファイルは 0。
    pub instability: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComponentMetrics {
    pub path: String,
    pub files: Vec<String>,
    pub complexity: ComplexityStats,
    pub fan_in: usize,
    pub fan_out: usize,
    pub depends_on: usize,
    pub used_by: usize,
    pub instability: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyMetrics {
    pub component_count: usize,
    pub edge_count: usize,
    pub cyclic_component_count: usize,
    pub ccd: u64,
    pub acd: f64,
    pub propagation_cost: f64,
    pub propagation_cost_percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyEdge {
    pub from: String,
    pub to: String,
    pub evidence: Vec<DependencyEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct DependencyEvidence {
    pub path: String,
    pub line: usize,
    pub source: String,
    pub target: String,
    pub kind: String,
    pub resolution: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CycleEvidence {
    pub components: Vec<String>,
    pub steps: Vec<DependencyEdge>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ResolutionSummary {
    pub static_imports: usize,
    pub resolved_local_imports: usize,
    pub external_imports: usize,
    pub unresolved_imports: usize,
    pub ambiguous_imports: usize,
    pub dynamic_imports: usize,
    pub unsupported_files: usize,
    pub failed_files: usize,
    pub all_static_imports_resolved: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsDiagnostic {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub reason: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnresolvedDependency {
    pub path: String,
    pub line: usize,
    pub source: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsResult {
    pub component_unit: ComponentUnit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_depth: Option<usize>,
    pub dependency_basis: &'static str,
    pub glob: Option<String>,
    pub include_generated: bool,
    pub limitations: Vec<&'static str>,
    pub complexity: ComplexityStats,
    pub dependencies: DependencyMetrics,
    pub components: Vec<ComponentMetrics>,
    pub files: Vec<FileMetrics>,
    pub file_edges: Vec<DependencyEdge>,
    pub edges: Vec<DependencyEdge>,
    /// 強連結成分。自己 import はファイル間依存から除く。
    pub cycles: Vec<Vec<String>>,
    pub cycle_evidence_mode: &'static str,
    pub cycle_evidence: Vec<CycleEvidence>,
    pub resolution_summary: ResolutionSummary,
    pub external_dependencies: Vec<UnresolvedDependency>,
    pub unresolved_dependencies: Vec<UnresolvedDependency>,
    pub diagnostics: Vec<MetricsDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkippedFiles>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub truncations: Vec<TruncationInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<MetricsComparison>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsDelta {
    pub component_count: i64,
    pub edge_count: i64,
    pub cyclic_component_count: i64,
    pub ccd: i64,
    pub acd: f64,
    pub propagation_cost: f64,
    pub complexity_count: i64,
    pub complexity_total: i64,
    pub complexity_average: f64,
    pub complexity_max: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentChange {
    pub path: String,
    pub before: ComponentMetrics,
    pub after: ComponentMetrics,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsComparison {
    pub base_commit: String,
    pub target: &'static str,
    pub baseline: Box<MetricsResult>,
    pub delta: MetricsDelta,
    pub normalization_changed: bool,
    pub coverage_changed: bool,
    pub assessment: &'static str,
    pub added_components: Vec<String>,
    pub removed_components: Vec<String>,
    pub changed_components: Vec<ComponentChange>,
    pub function_changes: Vec<FunctionChange>,
    pub added_edges: Vec<DependencyEdge>,
    pub removed_edges: Vec<DependencyEdge>,
    pub added_cycle_groups: Vec<Vec<String>>,
    pub removed_cycle_groups: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionChange {
    pub path: String,
    pub name: String,
    pub container: Option<String>,
    pub pairing: &'static str,
    pub before: Vec<FunctionComplexity>,
    pub after: Vec<FunctionComplexity>,
}
