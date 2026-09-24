//! ライブラリへの委譲と、ストリーミング用アダプタのデータ保持を検証する。

use super::{encode, encode_list_item, streaming_array_header, to_toon_value};
use serde_json::{Value, json};

#[test]
fn delegates_documents_to_toon_v3() {
    for value in [
        json!([]),
        json!({}),
        json!({"empty": []}),
        json!({"rows": [{"id": 1}, {"id": 2}]}),
        json!({"rows": [{"id": 1}, {"id": 2, "name": "Ada"}]}),
        json!({"rows": [{"meta": {"x": 1}}, {"meta": {"x": 2}}]}),
        json!({"a": {"id": 1}, "b": {"id": 2}}),
        json!({"text": "日本語\n\"quoted\"\\path", "number": 1.25}),
    ] {
        let actual = encode(&value).unwrap();
        assert_eq!(actual, toon_format::encode_default(&value).unwrap());
        let decoded: Value = toon_format::decode_strict(&actual).unwrap();
        assert_eq!(decoded, value);
        assert!(!actual.ends_with('\n'));
    }
    assert_eq!(encode(&json!([])).unwrap(), "[0]:");
    assert_eq!(encode(&json!({"empty": []})).unwrap(), "empty[0]:");
}

#[test]
fn struct_field_order_and_unsized_inputs_are_preserved() {
    #[derive(serde::Serialize)]
    struct Row {
        z: u32,
        a: u32,
    }
    let row = Row { z: 1, a: 2 };
    assert_eq!(encode(&row).unwrap(), "z: 1\na: 2");
    assert_eq!(encode("hello").unwrap(), "hello");
    assert_eq!(encode(&[1u32, 2][..]).unwrap(), "[2]: 1,2");
    assert_eq!(to_toon_value(&f64::NAN).unwrap(), Value::Null);
}

#[test]
fn duplicate_map_keys_follow_serde_json_last_value_wins() {
    struct Duplicate;
    impl serde::Serialize for Duplicate {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry("key", &1)?;
            map.serialize_entry("key", &2)?;
            map.end()
        }
    }
    assert_eq!(to_toon_value(&Duplicate).unwrap(), json!({"key": 2}));
    let text = encode(&Duplicate).unwrap();
    assert_eq!(
        toon_format::decode_strict::<Value>(&text).unwrap(),
        json!({"key": 2})
    );
}

#[test]
fn streaming_items_round_trip_all_json_shapes() {
    let records = vec![
        json!({}),
        json!([]),
        json!(null),
        json!(false),
        json!(42),
        json!("comma, colon: quote\"\n  - [0]:"),
        json!([1, 2]),
        json!([{"id": 1}, {"id": 2}]),
        json!([{"id": 1}, false]),
        json!({"nested": {"x": 1}, "tail": true}),
        json!({"rows": [{"id": 1}, {"id": 2}], "tail": true}),
        json!({"rows": [{"id": 1}, {"other": 2}], "tail": true}),
        json!({"empty": [], "tail": true}),
    ];
    let mut text = streaming_array_header(records.len());
    for value in &records {
        let item = encode_list_item(value).unwrap();
        assert!(item.starts_with("  -"), "{item}");
        assert!(!item.ends_with('\n'));
        text.push('\n');
        text.push_str(&item);
    }
    let decoded: Vec<Value> = toon_format::decode_strict(&text).unwrap();
    assert_eq!(decoded, records, "{text}");
    assert_eq!(
        toon_format::decode_strict::<Value>(&streaming_array_header(0)).unwrap(),
        json!([])
    );
}
