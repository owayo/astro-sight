//! TOON (Token-Oriented Object Notation) v4.1 のエンコーダ。
//!
//! <https://toonformat.dev/> / 仕様: <https://github.com/toon-format/spec> (v4.1, 2026-07-25)。
//!
//! # なぜ自前実装か
//!
//! crates.io の `toon-format` は通常依存で `serde_json` の `preserve_order` feature を
//! 有効化する。cargo の feature unification はワークスペース全体に効くため、astro-sight
//! 自身が `serde_json::Map` で組み立てている hook 出力等のキー順が「辞書順」から
//! 「挿入順」へ変わる = **既存 JSON 出力のバイト列が変わる**。「JSON 出力を一切変えずに
//! 新フォーマットを足す」という本変更の目的と相容れないため、encoder のみを自前で持つ。
//! (default feature が ratatui / syntect / arboard を引く点、リリースが spec v3 世代である
//! 点も加味しているが、決め手は feature unification。)
//!
//! # 実装範囲
//!
//! - **encoder のみ**。decoder は astro-sight の用途 (出力) に不要なので持たない。
//! - **delimiter は comma 固定**。tab / pipe は仕様上の選択肢だが切り替え手段を設けない。
//! - **indent は 2 スペース固定** (§12 の既定値)。
//! - ホスト型の正規化 (§3) は `serde::Serialize` に委ねる。非有限の浮動小数は `null`。
//! - `encode` の戻り値に末尾改行は含めない (§12)。CLI が stdout へ書く際の 1 個の
//!   改行は行指向ツールとの相互運用のために付ける (decoder は §12 で許容している)。

mod encode;
mod scalar;
mod value;

#[cfg(test)]
mod tests;

pub use value::{ToonValue, to_toon_value};

/// TOON エンコード時のエラー。
#[derive(Debug)]
pub enum ToonError {
    /// `serde::Serialize` 側から報告されたエラー。
    Serialize(String),
    /// エンコーダ内部の不変条件違反 (tabular 列の欠落など)。
    Encode(String),
}

impl std::fmt::Display for ToonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToonError::Serialize(msg) => write!(f, "TOON serialization failed: {msg}"),
            ToonError::Encode(msg) => write!(f, "TOON encoding failed: {msg}"),
        }
    }
}

impl std::error::Error for ToonError {}

/// `Serialize` な値を TOON ドキュメントへエンコードする。末尾改行は付かない。
pub fn encode<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, ToonError> {
    encode_value(&to_toon_value(value)?)
}

/// 中間表現から TOON ドキュメントを組み立てる。
pub fn encode_value(value: &ToonValue) -> Result<String, ToonError> {
    encode::encode_document(value)
}

/// ストリーミング出力用: 要素数 `len` のルート配列を list form (§9.4) で開くヘッダ行。
///
/// バッチ (`--paths` / `--paths-file` / `--dir`) は解析結果を全件バッファせずに逐次
/// 書き出す設計のため、配列全体を見てからでないと決まらない tabular form (§9.3) は
/// 使えない。要素数だけは入力パス数から先に分かるので、list form なら
/// ピーク RSS を入力件数から独立させたまま 1 個の妥当な TOON ドキュメントを出せる。
pub fn streaming_array_header(len: usize) -> String {
    encode::list_form_array_header(len)
}

/// ストリーミング出力用: ルート配列の要素 1 件を list item として書き出す。
/// 末尾改行は付かない。
pub fn encode_list_item(value: &ToonValue) -> Result<String, ToonError> {
    encode::encode_list_item_at(value, 1)
}
