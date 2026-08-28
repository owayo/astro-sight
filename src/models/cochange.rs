use serde::{Deserialize, Serialize};

use super::skip::SkipInfo;

/// 起点ファイルのコミット集合をどう構築したかの種別。
///
/// `Blame` が既定かつ最も密結合な証拠 (起点の**変更行**を最後に触ったコミット)。
/// 変更行が取れない起点 (diff 無し / 純粋追加のみ / blame 集合が `min_denominator`
/// 未満) では `History` にフォールバックし、ファイル自身のコミット履歴を代替証拠にする。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoChangeEvidence {
    /// 変更行に対する `git blame` から得たコミット集合。
    Blame,
    /// `git log <base> -- <file>` のファイル履歴から得たコミット集合。
    History,
}

/// 頻繁に同時変更されるファイルの組。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoChangeEntry {
    pub file_a: String,
    pub file_b: String,
    /// 両方のファイルが変更されたコミット数。
    pub co_changes: usize,
    /// file_a の総変更回数。
    pub total_changes_a: usize,
    /// file_b の総変更回数。
    pub total_changes_b: usize,
    /// 確信度: `co_changes / |C|`（|C| = 実際に集計対象となったコミット数）。
    pub confidence: f64,
    /// confidence の分母 |C|。証拠コミットのうち、`max_files_per_commit` 超過等で
    /// 重み 0 となり集計から外れたものを**除いた**実効件数。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub denominator: Option<usize>,
    /// 平滑化したランキングスコア。
    ///
    /// - 既定 (smoothing 有効): `(co + α) / (denom + α + β)` で小サンプルを過信しない
    /// - `--no-smoothing` 指定時: `confidence` と同値 (互換のため必ず Some)
    ///
    /// これは**ランキング専用**の値であり、出力可否のフィルタには使わない
    /// (フィルタは raw `confidence` と、明示指定時のみ `--min-score`)。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub score: Option<f64>,
    /// このペアの証拠がどう作られたか。既定の `Blame` のときは出力を省略する
    /// (= フィールドが無ければ blame 由来)。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub evidence: Option<CoChangeEvidence>,
}

impl CoChangeEntry {
    /// ランキングに使う値を返す。smoothing 有効なら `score`、無効なら raw `confidence`。
    ///
    /// 出力可否のフィルタには使わない。`score` は shrinkage 推定値で、分母が小さいほど
    /// 0 に引き寄せられるため、閾値と突き合わせると「証拠が少ない起点は 100% 共変更でも
    /// 絶対に出力されない」構造になる (既定 β=8 では分母 2 で上限 0.27)。
    pub fn ranking_value(&self, smoothing_on: bool) -> f64 {
        if smoothing_on {
            self.score.unwrap_or(self.confidence)
        } else {
            self.confidence
        }
    }

    /// 履歴 fallback 由来かどうか。ランキングの tie-break で blame 由来を優先するのに使う。
    pub fn is_history_evidence(&self) -> bool {
        matches!(self.evidence, Some(CoChangeEvidence::History))
    }
}

/// 共変更分析が 0 件 / 少数になった理由。`CoChangeDiagnostics::reasons` に
/// ソート済み・重複なしで格納する (出力の決定論性のため)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoChangeDiagnosticReason {
    /// 起点に blame 可能な変更行が無い (diff が空 / 純粋追加 hunk のみ)。
    NoChangedOldLines,
    /// 新規追加ファイルで base 側に履歴が無く、起点にできない。
    NewFileHasNoPriorHistory,
    /// blame も履歴もコミット集合を作れなかった。
    NoCommitEvidence,
    /// 証拠コミット数が `min_denominator` に満たず起点ごとスキップした。
    BelowMinDenominator,
    /// 候補はあったが `min_samples` で落ちた。
    BelowMinSamples,
    /// 候補はあったが `min_confidence` で落ちた。
    BelowMinConfidence,
    /// 候補はあったが `min_score` で落ちた。
    BelowMinScore,
    /// git コマンドが失敗した (証拠なしと区別する)。
    GitCommandFailed,
    /// 証拠コミットの `git diff-tree` に失敗し、そのコミットの共変更を数えられなかった。
    CommitScanFailed,
    /// 起点ファイル数が `max_source_files` を超えたため解析をスキップした。
    /// review 経路では起点過多 (退化した作業ツリー等) で cochange フェーズだけを
    /// 諦め、impact / API 差分 / dead 検出は継続する。
    SourceFilesExceedLimit,
    /// 候補が base リビジョンに存在しない (過去に削除済み) ため除外した。
    CandidateDeletedAtBase,
}

/// 共変更分析の内訳。`entries` が空のときに「共変更が無い」のか
/// 「解析できなかった / 閾値で落ちた」のかを呼び出し側が区別できるようにする。
///
/// 全カウンタが 0 のときは出力から省略される (追加専用フィールド)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CoChangeDiagnostics {
    /// 起点として渡されたファイル数。
    pub sources_requested: usize,
    /// 変更行 blame でコミット集合を構築できた起点数。
    pub sources_with_blame_evidence: usize,
    /// blame が不足し履歴 fallback を使った起点数。
    pub sources_with_history_evidence: usize,
    /// どちらの証拠も作れなかった起点数。
    pub sources_without_evidence: usize,
    /// base 側に存在しない新規追加ファイルで、履歴 fallback も効かなかった起点数。
    pub new_files_without_history: usize,
    /// 閾値適用前の候補ペア数。
    pub candidate_pairs: usize,
    /// `min_denominator` 未満で起点ごとスキップした数。
    pub skipped_min_denominator: usize,
    /// `min_samples` で落ちた候補ペア数。
    pub filtered_min_samples: usize,
    /// `min_confidence` で落ちた候補ペア数。
    pub filtered_min_confidence: usize,
    /// `min_score` で落ちた候補ペア数。
    pub filtered_min_score: usize,
    /// `per_source_limit` で切り捨てた候補ペア数。
    pub truncated_per_source_limit: usize,
    /// base リビジョンに存在しない (過去のコミットで削除済み) ため落とした候補ペア数。
    ///
    /// 追加専用フィールドのため 0 のときは出力しない (既存 JSON 消費側への影響を避ける)。
    #[serde(default, skip_serializing_if = "crate::models::review::is_zero_usize")]
    pub filtered_deleted_candidates: usize,
    /// 証拠コミットのうち `git diff-tree` に失敗して変更ファイルを取得できなかった数。
    ///
    /// 失敗コミットは「変更 0 件のコミット」として集計を続行する (= confidence の実効分母
    /// には入るが共起は 1 件も数えられない) ため、黙っていると「共変更が無かった」と
    /// 区別できない。`commits_analyzed` は証拠集合 |C| のサイズなのでこの分は引かれない。
    ///
    /// 追加専用フィールドのため 0 のときは出力しない (既存 JSON 消費側への影響を避ける)。
    #[serde(default, skip_serializing_if = "crate::models::review::is_zero_usize")]
    pub commit_scan_failures: usize,
    /// 0 件 / 少数になった理由 (ソート済み・重複なし)。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub reasons: Vec<CoChangeDiagnosticReason>,
}

impl CoChangeDiagnostics {
    /// 全カウンタが 0 かつ理由も無い (= 解析自体を行わなかった) か。
    /// `skip_serializing_if` に使い、既存 JSON 消費側への追加影響を最小化する。
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// 理由を追加する。重複は `finalize` で除去される。
    pub fn add_reason(&mut self, reason: CoChangeDiagnosticReason) {
        self.reasons.push(reason);
    }

    /// 出力前に理由をソート + 重複除去する (決定論的出力のため)。
    pub fn finalize(&mut self) {
        self.reasons.sort_unstable();
        self.reasons.dedup();
    }
}

/// 共変更分析の結果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoChangeResult {
    pub entries: Vec<CoChangeEntry>,
    /// 証拠として集めたユニークコミット集合 |C| のサイズ (全起点の和集合)。
    pub commits_analyzed: usize,
    /// 解析の内訳。`entries` が空の理由を機械的に判定できるようにする。
    /// 解析自体を行わなかった場合 (起点 0 件 / git 管理外 skip) は省略される。
    #[serde(skip_serializing_if = "CoChangeDiagnostics::is_empty", default)]
    pub diagnostics: CoChangeDiagnostics,
    /// git 管理外 dir で `--git` が要求され diff を取得できず skip した場合の理由。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub skipped: Option<SkipInfo>,
}

/// blame ベースの共変更分析を制御するオプション。
///
/// astro-sight v26.6.0 で旧 lookback モード (`git log` ベース) は廃止され、
/// blame モード (起点ファイルの変更行に `git blame` を当て、最終修正コミット
/// 集合から共起ファイルを集計する) のみがサポートされる。
#[derive(Debug, Clone)]
pub struct CoChangeOptions {
    /// 起点ファイル (リポジトリ相対パス)。`--git` 経由で diff から自動収集するか
    /// `--paths` / `--paths-file` で明示指定する。
    pub source_files: Vec<String>,
    /// 基準 revision (None のとき `HEAD` を既定とする = 未コミットの作業ツリー変更)。
    /// `git diff <base>` の単一 revision 形式で評価するため、`HEAD~1` を指定すると
    /// 「直前コミット + 未コミット変更」が対象になる。
    pub base: Option<String>,
    /// pair を出すために必要な最小 confidence (0.0..=1.0)。
    ///
    /// **raw `confidence` (= co / 実効分母) に対して適用される**。平滑化した `score` には
    /// 適用しない: score は分母が小さいほど 0 に引き寄せられる shrinkage 推定値のため、
    /// 閾値と突き合わせると変更行 blame 特有の小さい分母では構造的に出力不能になる。
    pub min_confidence: f64,
    /// pair を出すために必要な最小 `score` (0.0 = 無効)。
    ///
    /// 平滑化スコアで追加のゲートを掛けたいときだけ明示指定する。既定は 0.0 で、
    /// score はランキングにのみ使われる。
    pub min_score: f64,
    /// pair を出すために必要な最小 co_changes 数。
    pub min_samples: usize,
    /// 候補ファイルから除外する glob パターン (BLAME_DEFAULT_EXCLUDE_GLOBS と OR で適用)。
    pub exclude_globs: Vec<String>,
    /// 起点ファイル数の上限。0 = 無制限。超過時は InvalidRequest で停止する。
    ///
    /// 既定 500。`--git` の起点収集は `git diff <base>` (作業ツリー比較) なので、
    /// 退化した作業ツリー (no-checkout worktree、大量削除の途中状態など) では
    /// 追跡ファイル全件が起点に化けて blame が扇形展開する。実レビューの diff は
    /// 数十ファイル規模なので 500 は十分に緩く、暴走だけを止める。
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
    /// 変更行 blame で証拠が作れない起点に対する履歴 fallback の上限コミット数。
    /// `0` で fallback 無効 (blame のみ)。
    ///
    /// 変更行 blame は「起点の変更行を最後に触ったコミット」しか見ないため、
    /// diff が無い起点 (`--paths` で任意ファイルを指定した場合) や純粋追加のみの
    /// 変更ではコミット集合が空になり、共変更を一切検出できない。その場合に
    /// `git log <base> -n <limit> -- <file>` でファイル自身の履歴を代替証拠にする。
    pub history_limit: usize,
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
            // score ゲートは既定で無効。score は shrinkage 済みで分母依存のため、
            // 閾値として使うと小分母の起点が構造的に出力不能になる (詳細は min_score の doc)。
            min_score: 0.0,
            min_samples: 2,
            exclude_globs: Vec::new(),
            // 暴走ガード。実レビューの diff (数十ファイル) には当たらない緩さで、
            // 作業ツリーが退化しているときの全件 blame だけを止める。
            max_source_files: 500,
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
            // 推奨値 20: blame 証拠が作れない起点の履歴 fallback 上限。
            // 実測 (123k commits のモノレポ、直近 30 コミットを review 相当で解析) では
            // 10/15/20 が同じカバレッジ (22/29 コミットで結果あり) になり、20 が最も
            // 出力件数が少ない = ノイズが小さい。30 に広げるとハブファイルの共変更が
            // 拡散して 14/29 まで落ちる (窓を広げるほど結合が薄まるため)。
            // `git log` / `diff-tree` の追加コストは起点あたり数十 ms。
            history_limit: 20,
            // 既定 7 (週単位): 同一 author の短時間 burst による偽陽性を抑制する。
            // `0` で旧挙動 (raw weighted 集計、author 圧縮なし) に戻せる。
            author_unit_window_days: 7,
        }
    }
}

/// blame モードの既定除外 glob (生成物ディレクトリ / minified)。
///
/// ロックファイルはここに列挙しない。glob を手で並べると
/// `crate::models::dependency_files::DEPENDENCY_MANIFEST_LOCK_PAIRS` との乖離が起きて
/// 「Cargo.lock は除外されるが uv.lock は残る」形で言語ごとに挙動が変わるため、
/// `CoChangeExclude::is_match` が正本テーブルで意味判定する
/// (`is_dependency_lock_path`)。ここは純粋な glob 規則だけを持つ。
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
    "**/*.min.js",
    "**/*.min.css",
];
