use serde::Serialize;

use super::review::DeadSymbol;

/// dead-code コマンドのレスポンス。
///
/// `test_only_symbols` は production 側コードからの参照が無く、
/// test/spec ディレクトリ配下からのみ参照されるシンボル。
/// 「テスト用 API として残しておくか、本当に dead として除去するか」を
/// レビュアー判断に委ねるため、`dead_symbols` から分離して報告する。
#[derive(Debug, Clone, Serialize)]
pub struct DeadCodeResult {
    pub dir: String,
    pub scanned_files: usize,
    pub dead_symbols: Vec<DeadSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub test_only_symbols: Vec<DeadSymbol>,
}
