//! `toon-format` 0.5 の TOON v3 エンコーダへのアダプタ。
//!
//! comma / 2-space の既定設定を使い、符号化規則はライブラリに委譲する。
//! DTO の省略列補完は `output::nullable` が担当する。

#[cfg(test)]
mod tests;

pub use serde_json::Value as ToonValue;
pub use toon_format::ToonError;

/// 順序を保持する JSON 値へ変換する。重複 map キーは serde_json と同じく後勝ち。
pub fn to_toon_value<T: serde::Serialize + ?Sized>(value: &T) -> Result<ToonValue, ToonError> {
    serde_json::to_value(value).map_err(|e| ToonError::SerializationError(e.to_string()))
}

/// `Serialize` な値を TOON v3 へエンコードする。末尾改行は付かない。
pub fn encode<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, ToonError> {
    toon_format::encode_default(&value)
}

/// 中間表現から TOON ドキュメントを組み立てる。
pub fn encode_value(value: &ToonValue) -> Result<String, ToonError> {
    encode(value)
}

/// バッチのルート配列ヘッダ。空配列も v3 の `[0]:` で表す。
pub fn streaming_array_header(len: usize) -> String {
    format!("[{len}]:")
}

/// 全件を保持せず、ライブラリでルート配列の list item 1 件を符号化する。
pub fn encode_list_item(value: &ToonValue) -> Result<String, ToonError> {
    // 空配列を末尾に足すと primitive / tabular 配列にならず、必ず list form になる。
    // header と末尾の番兵だけを除き、値の quoting・入れ子の字下げには手を加えない。
    let sentinel = ToonValue::Array(Vec::new());
    let document = toon_format::encode_default(&[value, &sentinel])?;
    document
        .strip_prefix("[2]:\n")
        .and_then(|body| body.strip_suffix("\n  - [0]:"))
        .map(str::to_owned)
        .ok_or_else(|| ToonError::SerializationError("unexpected TOON list framing".into()))
}
