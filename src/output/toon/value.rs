//! TOON v4.1 エンコーダ内部の順序付き値。
//!
//! 公開側の `serde_json::Value` は従来の nullable 列補完にも使う。
//! `preserve_order` を有効にして遭遇順を保ったまま、この表現へ変換する。

use super::ToonError;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ToonValue {
    Null,
    Bool(bool),
    Int(i128),
    UInt(u128),
    Float(f64),
    Str(String),
    Array(Vec<ToonValue>),
    Object(Vec<(String, ToonValue)>),
}

impl ToonValue {
    pub(super) fn is_primitive(&self) -> bool {
        !matches!(self, Self::Array(_) | Self::Object(_))
    }

    pub(super) fn as_non_empty_object(&self) -> Option<&[(String, ToonValue)]> {
        match self {
            Self::Object(fields) if !fields.is_empty() => Some(fields),
            _ => None,
        }
    }
}

pub(super) fn from_json_value(value: &serde_json::Value) -> Result<ToonValue, ToonError> {
    Ok(match value {
        serde_json::Value::Null => ToonValue::Null,
        serde_json::Value::Bool(v) => ToonValue::Bool(*v),
        serde_json::Value::Number(v) => {
            if let Some(n) = v.as_i64() {
                ToonValue::Int(n as i128)
            } else if let Some(n) = v.as_u64() {
                ToonValue::UInt(n as u128)
            } else if let Some(n) = v.as_f64() {
                ToonValue::Float(n)
            } else {
                return Err(ToonError::Encode("JSON の数値を変換できません".into()));
            }
        }
        serde_json::Value::String(v) => ToonValue::Str(v.clone()),
        serde_json::Value::Array(items) => ToonValue::Array(
            items
                .iter()
                .map(from_json_value)
                .collect::<Result<_, _>>()?,
        ),
        serde_json::Value::Object(fields) => ToonValue::Object(
            fields
                .iter()
                .map(|(key, value)| Ok((key.clone(), from_json_value(value)?)))
                .collect::<Result<_, ToonError>>()?,
        ),
    })
}
