//! TOON v4.1 の出力アダプタ。
//!
//! `toon-format` 0.5 は v3 形式を出すため、v4.1 の必須規則を満たす
//! エンコーダを使用する。DTO の省略列補完は `output::nullable` が担当する。

mod encode;
mod scalar;
mod value;

#[cfg(test)]
mod tests;

pub use serde_json::Value as ToonValue;

#[derive(Debug)]
pub enum ToonError {
    Serialize(String),
    Encode(String),
}

impl std::fmt::Display for ToonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(message) => write!(f, "TOON の値変換に失敗しました: {message}"),
            Self::Encode(message) => write!(f, "TOON の符号化に失敗しました: {message}"),
        }
    }
}

impl std::error::Error for ToonError {}

/// 順序を保持する JSON 値へ変換する。重複 map キーは serde_json と同じく後勝ち。
pub fn to_toon_value<T: serde::Serialize + ?Sized>(value: &T) -> Result<ToonValue, ToonError> {
    serde_json::to_value(value).map_err(|error| ToonError::Serialize(error.to_string()))
}

/// `Serialize` な値を TOON v4.1 へエンコードする。末尾改行は付かない。
pub fn encode<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, ToonError> {
    encode_value(&to_toon_value(value)?)
}

/// 中間表現から TOON ドキュメントを組み立てる。
pub fn encode_value(value: &ToonValue) -> Result<String, ToonError> {
    encode::encode_document(&value::from_json_value(value)?)
}

/// バッチのルート配列ヘッダ。空配列は v4.1 の `[]` で表す。
pub fn streaming_array_header(len: usize) -> String {
    encode::list_form_array_header(len)
}

/// 全件を保持せず、ルート配列の list item 1 件を符号化する。
pub fn encode_list_item(value: &ToonValue) -> Result<String, ToonError> {
    encode::encode_list_item_at(&value::from_json_value(value)?, 1)
}
