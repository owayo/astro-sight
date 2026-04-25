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
    /// Confidence score.
    /// - lookback モード: `co_changes / max(total_a, total_b)`
    /// - blame モード: `co_changes / |C|` (|C| = blame で得たユニークコミット数)
    pub confidence: f64,
    /// blame モードでの分母 (= |C|: 起点ファイル変更行に関わる過去コミット集合の大きさ)。
    /// lookback モードでは None。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub denominator: Option<usize>,
}

/// Result of co-change analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoChangeResult {
    pub entries: Vec<CoChangeEntry>,
    /// lookback モード: 走査コミット数。blame モード: |C|。
    pub commits_analyzed: usize,
}

/// Options controlling co-change analysis behaviour.
#[derive(Debug, Clone)]
pub struct CoChangeOptions {
    /// Maximum number of commits to walk when computing statistics. (lookback モードのみ)
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
    /// no default branch can be inferred. (lookback モードのみ)
    pub bounded_by_merge_base: bool,
    /// Drop pairs when either file is missing from the current `HEAD` tree
    /// (renamed/deleted files). (lookback モードのみ)
    pub skip_deleted_files: bool,
    /// Optional filter: only keep pairs that include this file. (lookback モードのみ)
    pub filter_file: Option<String>,
    /// blame モードを有効化する。
    pub blame: bool,
    /// blame モードでの起点ファイル (リポジトリ相対パス)。
    pub source_files: Vec<String>,
    /// blame モードでの基準 revision (None のとき "HEAD~1" を既定とする)。
    pub base: Option<String>,
    /// blame モードで候補ファイルから除外する glob パターン。
    pub exclude_globs: Vec<String>,
    /// blame モードでの起点ファイル数の上限。0 = 無制限。
    /// 上限超過時は InvalidRequest エラーで停止し、暴走を防ぐ。
    pub max_source_files: usize,
    /// blame モードで `git blame -M` を有効化する (リネーム/移動を追跡)。
    pub rename: bool,
    /// blame モードで取得した SHA 集合からマージコミットを除外する。
    pub ignore_merges: bool,
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
            blame: false,
            source_files: Vec::new(),
            base: None,
            exclude_globs: Vec::new(),
            max_source_files: 0,
            rename: false,
            ignore_merges: false,
        }
    }
}

/// blame モードの既定除外 glob (生成物 / ロック / vendored)。
pub const BLAME_DEFAULT_EXCLUDE_GLOBS: &[&str] = &[
    "vendor/**",
    "**/vendor/**",
    "node_modules/**",
    "**/node_modules/**",
    "dist/**",
    "**/dist/**",
    "build/**",
    "**/build/**",
    "target/**",
    "**/target/**",
    "**/composer.lock",
    "**/package-lock.json",
    "**/yarn.lock",
    "**/pnpm-lock.yaml",
    "**/Cargo.lock",
    "**/*.min.js",
    "**/*.min.css",
];
