use serde::{Deserialize, Serialize};

/// A pair of files that frequently change together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoChangeEntry {
    pub file_a: String,
    pub file_b: String,
    /// Number of commits where both files changed.
    pub co_changes: usize,
    /// Total changes for file_a.
    pub total_changes_a: usize,
    /// Total changes for file_b.
    pub total_changes_b: usize,
    /// Confidence score: `co_changes / |C|` (|C| = blame で得たユニークコミット数)。
    pub confidence: f64,
    /// 起点ファイル変更行に関わる過去コミット集合 |C| の大きさ。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub denominator: Option<usize>,
    /// Smoothed ranking score。
    ///
    /// - 既定 (smoothing 有効): `(co + α) / (denom + α + β)` で小サンプルを過信しない
    /// - `--no-smoothing` 指定時: `confidence` と同値 (互換のため必ず Some)
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub score: Option<f64>,
}

impl CoChangeEntry {
    /// ランキング/フィルタに使う値を返す。smoothing 有効なら `score`、
    /// 無効なら raw `confidence`。
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
    /// blame で得たユニークコミット集合 |C| のサイズ。
    pub commits_analyzed: usize,
}

/// Options controlling blame-based co-change analysis.
///
/// astro-sight v26.6.0 で旧 lookback モード (`git log` ベース) は廃止され、
/// blame モード (起点ファイルの変更行に `git blame` を当て、最終修正コミット
/// 集合から共起ファイルを集計する) のみがサポートされる。
#[derive(Debug, Clone)]
pub struct CoChangeOptions {
    /// 起点ファイル (リポジトリ相対パス)。`--git` 経由で diff から自動収集するか
    /// `--paths` / `--paths-file` で明示指定する。
    pub source_files: Vec<String>,
    /// 基準 revision (None のとき `HEAD~1` を既定とする)。
    pub base: Option<String>,
    /// pair を出すために必要な最小 confidence (0.0..=1.0)。
    pub min_confidence: f64,
    /// pair を出すために必要な最小 co_changes 数。
    pub min_samples: usize,
    /// 候補ファイルから除外する glob パターン (BLAME_DEFAULT_EXCLUDE_GLOBS と OR で適用)。
    pub exclude_globs: Vec<String>,
    /// 起点ファイル数の上限。0 = 無制限。超過時は InvalidRequest で停止する。
    pub max_source_files: usize,
    /// 1 コミットあたりの変更ファイル数の上限。これを超えるコミット (大量生成
    /// / squash-merge 等) は共起カウントから除外する。
    pub max_files_per_commit: usize,
    /// `git blame -M` でファイル内移動 + ファイル間 rename を追跡する。
    pub rename: bool,
    /// `git blame -C` でファイル間コピーを検出する (`-M` より重い)。
    pub copy: bool,
    /// blame で取得した SHA 集合からマージコミットを除外する。
    pub ignore_merges: bool,
    /// blame SHA 集合のサイズ上限。0 = 無制限。超過時は InvalidRequest で停止する。
    pub max_blame_commits: usize,
    /// 解析全体のタイムアウト (秒)。0 = 無制限。
    pub timeout_secs: u64,
    /// Bayesian smoothing α (success prior)。`score = (co + α) / (denom + α + β)`。
    pub smoothing_alpha: f64,
    /// Bayesian smoothing β (failure prior)。
    pub smoothing_beta: f64,
    /// `--no-smoothing` 相当。true なら smoothing を無効化し score = confidence を使う。
    pub disable_smoothing: bool,
    /// 起点 blame 集合サイズの下限。`< N` の起点はスキップ。0 / 1 = 既定 (フィルタ無効)。
    pub min_denominator: usize,
    /// 起点ごとの候補上位 N 件に絞る。0 = 無制限。
    pub per_source_limit: usize,
}

impl Default for CoChangeOptions {
    fn default() -> Self {
        Self {
            source_files: Vec::new(),
            base: None,
            min_confidence: 0.3,
            min_samples: 2,
            exclude_globs: Vec::new(),
            max_source_files: 0,
            max_files_per_commit: 30,
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
