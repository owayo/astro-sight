//! TOON エンコード用の中間表現と `serde::Serializer`。
//!
//! `serde_json::Value` を経由しないのは、`serde_json::Map` が `BTreeMap` のため
//! **struct のフィールド宣言順が辞書順へ潰れる**から。TOON 仕様 §2 は
//! 「object の key 順は encoder が遭遇した順を保持する」ことを要求しており、
//! かつ既存 JSON 出力 (serde_json は Serializer 直行なので宣言順のまま) と
//! キー順を揃える必要がある。そのため順序付き `Vec<(String, ToonValue)>` を持つ
//! 専用の中間表現へ直接シリアライズする。

use serde::{Serialize, ser};
use std::fmt::Display;

use super::ToonError;

/// TOON が扱う JSON データモデルの値 (§2)。
#[derive(Debug, Clone, PartialEq)]
pub enum ToonValue {
    Null,
    Bool(bool),
    Int(i128),
    UInt(u128),
    Float(f64),
    Str(String),
    Array(Vec<ToonValue>),
    /// 遭遇順を保持する object。同一キーの重複は入力側 (serde) では通常起きないが、
    /// map シリアライズ経由では起こりうるためそのまま保持し、tabular 判定側で
    /// 重複を検出したら保守的に非 uniform へ倒す。
    Object(Vec<(String, ToonValue)>),
}

impl ToonValue {
    pub fn is_primitive(&self) -> bool {
        !matches!(self, ToonValue::Array(_) | ToonValue::Object(_))
    }

    /// 非空 object のときだけ中身を返す。tabular / keyed tabular 判定で
    /// 「非空 object であること」が条件になるため専用のヘルパーにする。
    pub fn as_non_empty_object(&self) -> Option<&[(String, ToonValue)]> {
        match self {
            ToonValue::Object(fields) if !fields.is_empty() => Some(fields),
            _ => None,
        }
    }
}

/// 任意の `Serialize` 値を `ToonValue` へ変換する。
pub fn to_toon_value<T: Serialize + ?Sized>(value: &T) -> Result<ToonValue, ToonError> {
    value.serialize(ToonValueSerializer)
}

impl ser::Error for ToonError {
    fn custom<T: Display>(msg: T) -> Self {
        ToonError::Serialize(msg.to_string())
    }
}

pub struct ToonValueSerializer;

impl ser::Serializer for ToonValueSerializer {
    type Ok = ToonValue;
    type Error = ToonError;

    type SerializeSeq = SeqSerializer;
    type SerializeTuple = SeqSerializer;
    type SerializeTupleStruct = SeqSerializer;
    type SerializeTupleVariant = TupleVariantSerializer;
    type SerializeMap = MapSerializer;
    type SerializeStruct = ObjectSerializer;
    type SerializeStructVariant = StructVariantSerializer;

    fn serialize_bool(self, v: bool) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Bool(v))
    }

    fn serialize_i8(self, v: i8) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Int(v as i128))
    }

    fn serialize_i16(self, v: i16) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Int(v as i128))
    }

    fn serialize_i32(self, v: i32) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Int(v as i128))
    }

    fn serialize_i64(self, v: i64) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Int(v as i128))
    }

    fn serialize_i128(self, v: i128) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Int(v))
    }

    fn serialize_u8(self, v: u8) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::UInt(v as u128))
    }

    fn serialize_u16(self, v: u16) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::UInt(v as u128))
    }

    fn serialize_u32(self, v: u32) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::UInt(v as u128))
    }

    fn serialize_u64(self, v: u64) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::UInt(v as u128))
    }

    fn serialize_u128(self, v: u128) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::UInt(v))
    }

    fn serialize_f32(self, v: f32) -> Result<ToonValue, ToonError> {
        // f32 → f64 の直変換は 0.1f32 が 0.10000000149011612 になるため、
        // f32 として最短往復する 10 進表記を経由して f64 へ載せ替える。
        Ok(ToonValue::Float(
            v.to_string().parse::<f64>().unwrap_or(v as f64),
        ))
    }

    fn serialize_f64(self, v: f64) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Float(v))
    }

    fn serialize_char(self, v: char) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Str(v.to_string()))
    }

    fn serialize_str(self, v: &str) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Str(v.to_string()))
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Array(
            v.iter().map(|b| ToonValue::UInt(*b as u128)).collect(),
        ))
    }

    fn serialize_none(self) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Null)
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<ToonValue, ToonError> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Null)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Null)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Str(variant.to_string()))
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<ToonValue, ToonError> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Object(vec![(
            variant.to_string(),
            value.serialize(ToonValueSerializer)?,
        )]))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<SeqSerializer, ToonError> {
        Ok(SeqSerializer {
            items: Vec::with_capacity(len.unwrap_or(0)),
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<SeqSerializer, ToonError> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<SeqSerializer, ToonError> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<TupleVariantSerializer, ToonError> {
        Ok(TupleVariantSerializer {
            variant,
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_map(self, len: Option<usize>) -> Result<MapSerializer, ToonError> {
        Ok(MapSerializer {
            fields: Vec::with_capacity(len.unwrap_or(0)),
            pending_key: None,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<ObjectSerializer, ToonError> {
        Ok(ObjectSerializer {
            fields: Vec::with_capacity(len),
        })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<StructVariantSerializer, ToonError> {
        Ok(StructVariantSerializer {
            variant,
            fields: Vec::with_capacity(len),
        })
    }
}

pub struct SeqSerializer {
    items: Vec<ToonValue>,
}

impl ser::SerializeSeq for SeqSerializer {
    type Ok = ToonValue;
    type Error = ToonError;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), ToonError> {
        self.items.push(value.serialize(ToonValueSerializer)?);
        Ok(())
    }

    fn end(self) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Array(self.items))
    }
}

impl ser::SerializeTuple for SeqSerializer {
    type Ok = ToonValue;
    type Error = ToonError;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), ToonError> {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<ToonValue, ToonError> {
        ser::SerializeSeq::end(self)
    }
}

impl ser::SerializeTupleStruct for SeqSerializer {
    type Ok = ToonValue;
    type Error = ToonError;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), ToonError> {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<ToonValue, ToonError> {
        ser::SerializeSeq::end(self)
    }
}

pub struct TupleVariantSerializer {
    variant: &'static str,
    items: Vec<ToonValue>,
}

impl ser::SerializeTupleVariant for TupleVariantSerializer {
    type Ok = ToonValue;
    type Error = ToonError;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), ToonError> {
        self.items.push(value.serialize(ToonValueSerializer)?);
        Ok(())
    }

    fn end(self) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Object(vec![(
            self.variant.to_string(),
            ToonValue::Array(self.items),
        )]))
    }
}

pub struct MapSerializer {
    fields: Vec<(String, ToonValue)>,
    pending_key: Option<String>,
}

impl ser::SerializeMap for MapSerializer {
    type Ok = ToonValue;
    type Error = ToonError;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), ToonError> {
        self.pending_key = Some(key.serialize(MapKeySerializer)?);
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), ToonError> {
        let key = self
            .pending_key
            .take()
            .ok_or_else(|| ToonError::Serialize("map value serialized before its key".into()))?;
        self.fields
            .push((key, value.serialize(ToonValueSerializer)?));
        Ok(())
    }

    fn end(self) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Object(self.fields))
    }
}

pub struct ObjectSerializer {
    fields: Vec<(String, ToonValue)>,
}

impl ser::SerializeStruct for ObjectSerializer {
    type Ok = ToonValue;
    type Error = ToonError;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), ToonError> {
        self.fields
            .push((key.to_string(), value.serialize(ToonValueSerializer)?));
        Ok(())
    }

    fn end(self) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Object(self.fields))
    }
}

pub struct StructVariantSerializer {
    variant: &'static str,
    fields: Vec<(String, ToonValue)>,
}

impl ser::SerializeStructVariant for StructVariantSerializer {
    type Ok = ToonValue;
    type Error = ToonError;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), ToonError> {
        self.fields
            .push((key.to_string(), value.serialize(ToonValueSerializer)?));
        Ok(())
    }

    fn end(self) -> Result<ToonValue, ToonError> {
        Ok(ToonValue::Object(vec![(
            self.variant.to_string(),
            ToonValue::Object(self.fields),
        )]))
    }
}

/// map のキー用シリアライザ。TOON (と JSON) の object キーは文字列のみのため、
/// 文字列化できる型だけを受け入れ、それ以外は明示エラーにする (silent な取りこぼしを作らない)。
struct MapKeySerializer;

macro_rules! key_from_display {
    ($method:ident, $ty:ty) => {
        fn $method(self, v: $ty) -> Result<String, ToonError> {
            Ok(v.to_string())
        }
    };
}

impl ser::Serializer for MapKeySerializer {
    type Ok = String;
    type Error = ToonError;

    type SerializeSeq = ser::Impossible<String, ToonError>;
    type SerializeTuple = ser::Impossible<String, ToonError>;
    type SerializeTupleStruct = ser::Impossible<String, ToonError>;
    type SerializeTupleVariant = ser::Impossible<String, ToonError>;
    type SerializeMap = ser::Impossible<String, ToonError>;
    type SerializeStruct = ser::Impossible<String, ToonError>;
    type SerializeStructVariant = ser::Impossible<String, ToonError>;

    key_from_display!(serialize_bool, bool);
    key_from_display!(serialize_i8, i8);
    key_from_display!(serialize_i16, i16);
    key_from_display!(serialize_i32, i32);
    key_from_display!(serialize_i64, i64);
    key_from_display!(serialize_i128, i128);
    key_from_display!(serialize_u8, u8);
    key_from_display!(serialize_u16, u16);
    key_from_display!(serialize_u32, u32);
    key_from_display!(serialize_u64, u64);
    key_from_display!(serialize_u128, u128);
    key_from_display!(serialize_char, char);

    fn serialize_f32(self, _v: f32) -> Result<String, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_f64(self, _v: f64) -> Result<String, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_str(self, v: &str) -> Result<String, ToonError> {
        Ok(v.to_string())
    }

    fn serialize_bytes(self, _v: &[u8]) -> Result<String, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_none(self) -> Result<String, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<String, ToonError> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<String, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<String, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<String, ToonError> {
        Ok(variant.to_string())
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<String, ToonError> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<String, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, ToonError> {
        Err(ToonError::Serialize("map key must be a string".into()))
    }
}
