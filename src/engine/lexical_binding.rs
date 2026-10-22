//! impact 専用の束縛証拠。出現検索と参照件数には適用しない。

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LexicalBinding {
    #[default]
    Unknown,
    FunctionLocal,
    /// Rust の値束縛または定数等のパターン。関数・メソッドだけを除外できる。
    RustValueBinding,
    /// 標準マクロの再束縛検査が必要な値束縛候補。
    RustMacroValueBinding,
}
