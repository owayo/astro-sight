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
    /// blame モードでの smoothed ranking score。
    ///
    /// - 既定 (smoothing 有効): `(co + α) / (denom + α + β)` で小サンプルを過信しない
    /// - `--no-smoothing` 指定時: `confidence` と同値 (互換のため必ず Some)
    ///
    /// lookback モードでは None。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub score: Option<f64>,
}

impl CoChangeEntry {
    /// ランキング/フィルタに使う値を返す。blame モードで smoothing 有効なら `score`、
    /// 無効または lookback モードなら raw `confidence`。
    /// `score` が `Some` でも `disable_smoothing` 時は呼び出し側が `false` を渡すことで
    /// raw 値に切り替わる。
    pub fn ranking_value(&self, smoothing_on: bool) -> f64 {
        if smoothing_on {
            self.score.unwrap_or(self.confidence)
        } else {
            self.confidence
        }
    }
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
    /// blame モードで `git blame -C` を有効化する (ファイル間コピー検出、`-M` より重い)。
    pub copy: bool,
    /// blame モードで取得した SHA 集合からマージコミットを除外する。
    pub ignore_merges: bool,
    /// blame モードで集めた SHA 集合の上限。0 = 無制限。
    /// 上限超過時は InvalidRequest エラーで停止し、diff-tree 爆発を防ぐ。
    pub max_blame_commits: usize,
    /// blame モードの解析全体のタイムアウト (秒)。0 = 無制限。
    /// 各 Phase 入口で経過時間をチェックし、超過時 InvalidRequest で停止する。
    /// 既に走った subprocess は kill しないため、実際の経過は若干オーバーする可能性がある。
    pub timeout_secs: u64,
    /// blame モードの Bayesian smoothing α (success prior)。既定 1.0。
    /// `score = (co + α) / (denom + α + β)` で小サンプル過信を抑える。
    pub smoothing_alpha: f64,
    /// blame モードの Bayesian smoothing β (failure prior)。既定 4.0。
    pub smoothing_beta: f64,
    /// `--no-smoothing` 相当。true なら smoothing を無効化し score = confidence (raw) を使う。
    pub disable_smoothing: bool,
    /// blame モードの起点 blame 集合サイズの下限。`< N` の起点はスキップ。
    /// 0 / 1 = 既存挙動 (フィルタ無効)。推奨値 2。
    pub min_denominator: usize,
    /// blame モードで起点ごとの候補上位 N 件に絞る。0 = 無制限。推奨値 10。
    pub per_source_limit: usize,
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
            copy: false,
            ignore_merges: false,
            max_blame_commits: 0,
            timeout_secs: 0,
            smoothing_alpha: 1.0,
            smoothing_beta: 4.0,
            disable_smoothing: false,
            min_denominator: 1,
            per_source_limit: 0,
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
