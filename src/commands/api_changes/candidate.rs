//! API 差分の候補 (旧新どちらかの版の公開の宣言 1 つ) と、出力の型への変換。

use super::*;

/// 内部用: reconcile のために signature を保持する一時構造。
#[derive(Debug, Clone)]
pub(crate) struct ApiSymbolCandidate {
    pub(crate) name: String,
    /// Rust の項目が inline module の中にあれば、ファイルの中のその経路 (Issue #282)。
    pub(crate) inline_module: Option<String>,
    pub(crate) kind: String,
    pub(crate) file: String,
    pub(crate) signature: String,
}

impl ApiSymbolCandidate {
    pub(crate) fn into_api_symbol(self) -> ApiSymbol {
        ApiSymbol {
            name: self.name,
            inline_module: self.inline_module,
            kind: self.kind,
            file: self.file,
            refs_internal: 0,
            cfg_availability: None,
        }
    }

    /// 同一ファイル内参照数を添えて `ApiSymbol` にする (api.add 専用)。
    pub(crate) fn into_added_api_symbol(self, refs_internal: usize) -> ApiSymbol {
        ApiSymbol {
            refs_internal,
            ..self.into_api_symbol()
        }
    }

    /// 旧い版の signature `old_signature` から、この候補 (新しい版の宣言) の signature へ変わった変更。
    pub(crate) fn into_modified(self, old_signature: String) -> ApiSymbolChange {
        ApiSymbolChange {
            inline_module: self.inline_module,
            ..ApiSymbolChange::new(
                self.name,
                self.kind,
                self.file,
                Some(old_signature),
                Some(self.signature),
            )
        }
    }
}
