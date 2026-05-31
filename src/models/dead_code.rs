use serde::Serialize;

use super::review::DeadSymbol;
use super::skip::SkipInfo;

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
}
