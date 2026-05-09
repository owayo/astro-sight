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
    /// score 計算時の commit-size weighting のピボット。
    /// `0` で size weighting 無効 (旧挙動)。`> 0` で `min(1.0, sqrt(pivot/file_count))` の
    /// 重みを各コミットに掛け、大コミット由来の偶然共起を抑制する。
    /// 推奨値 8 (= 「典型的な PR は 8 ファイル前後」のヒューリスティック)。
    pub commit_size_pivot: usize,
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
    /// 同一 author × 時間 window で commit を 1 knowledge unit として圧縮するときの
    /// window (日)。`0` で無効化 (= raw weighted 集計、旧挙動)。
    /// `> 0` のとき、score は `(|co_units| + α) / (|denom_units| + α + β)` で計算され、
    /// 同じ author の短時間 burst による偽陽性を抑制する。
    /// 推奨値 7 (週単位)。
    pub author_unit_window_days: u64,
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
            // hard cap は緩めにし、実際の抑制は size weighting に任せる。
            max_files_per_commit: 100,
            commit_size_pivot: 8,
            rename: false,
            copy: false,
            // マージコミットは diff-tree が広く候補をぶれさせるため、既定で除外する。
            // `--include-merges` (CLI) で旧挙動 (false) に戻せる。
            ignore_merges: true,
            max_blame_commits: 0,
            timeout_secs: 0,
            smoothing_alpha: 1.0,
            // β を 8 に上げて co=2/denom=2 のような小サンプル過信を抑える。
            smoothing_beta: 8.0,
            disable_smoothing: false,
            // 推奨値 2: blame 集合が 1 件しかない起点はノイズになりやすい。
            min_denominator: 2,
            // 推奨値 10: 起点ごとの候補を上位 10 件に絞り、出力ノイズを抑える。
            per_source_limit: 10,
            // 既定 7 (週単位): 同一 author の短時間 burst による偽陽性を抑制する。
            // `0` で旧挙動 (raw weighted 集計、author 圧縮なし) に戻せる。
            author_unit_window_days: 7,
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
