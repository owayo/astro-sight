use serde::Serialize;

use super::review::DeadSymbol;
use super::skip::{SkipInfo, SkippedFiles};

/// dead-code コマンドのレスポンス。
///
/// `test_only_symbols` は production 側コードからの参照が無く、
/// test/spec ディレクトリ配下からのみ参照されるシンボル。
/// 「テスト用 API として残しておくか、本当に dead として除去するか」を
/// レビュアー判断に委ねるため、`dead_symbols` から分離して報告する。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeadCodeResult {
    pub dir: String,
    pub scanned_files: usize,
    pub dead_symbols: Vec<DeadSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub test_only_symbols: Vec<DeadSymbol>,
    /// git 管理外 dir で `--git` が要求され diff を取得できず skip した場合の理由。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub skipped: Option<SkipInfo>,
    /// 解析対象から意図的に外したもの (未追跡の巨大ファイル等)。空なら出力に含まれない。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub truncations: Vec<crate::models::truncation::TruncationInfo>,
    /// 生成物として dead 判定の候補から外したファイル (`refs` の `skipped` と同じ形)。
    /// 中の参照は数えている (外したのは「その中のシンボルが dead か」の判定だけ)。
    /// `--include-generated` で候補に含められる。0 件なら出力に含まれない。
    ///
    /// 既存の `skipped` は「コマンド全体を解析しなかった」大域 skip なので別キーにする。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub generated_candidates_skipped: Option<SkippedFiles>,
}
