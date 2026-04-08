use serde::Serialize;

use super::review::DeadSymbol;

/// dead-code コマンドのレスポンス。
#[derive(Debug, Clone, Serialize)]
pub struct DeadCodeResult {
    pub dir: String,
    pub scanned_files: usize,
    pub dead_symbols: Vec<DeadSymbol>,
}
