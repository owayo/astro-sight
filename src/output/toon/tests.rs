//! TOON v4.1 の必須表記とストリーミング出力を固定する。

use super::{encode, encode_list_item, streaming_array_header, to_toon_value};
use serde_json::json;

#[test]
fn empty_arrays_use_v41_literal() {
    assert_eq!(encode(&json!([])).unwrap(), "[]");
    assert_eq!(encode(&json!({"empty": []})).unwrap(), "empty: []");
    assert_eq!(streaming_array_header(0), "[]");
}

#[test]
fn ambiguous_strings_and_numbers_follow_v41_rules() {
    assert_eq!(encode(&json!("#tag")).unwrap(), "\"#tag\"");
    assert_eq!(
        encode(&json!({"hash": "#tag", "dash": "-tag", "number": "1e6", "truth": "true"})).unwrap(),
        "hash: \"#tag\"\ndash: \"-tag\"\nnumber: \"1e6\"\ntruth: \"true\""
    );
    assert_eq!(
        encode(&json!({"million": 1e6, "micro": 1e-6, "large": 1e21})).unwrap(),
        "million: 1000000\nmicro: 0.000001\nlarge: 1e+21"
    );
}

#[test]
fn uniform_rows_use_required_tabular_forms() {
    assert_eq!(encode(&json!({"rows": [{"id": 1, "user": {"name": "Ada"}}, {"id": 2, "user": {"name": "Bob"}}]})).unwrap(),
        "rows[2]{id,user{name}}:\n  1,Ada\n  2,Bob");
    assert_eq!(
        encode(&json!({"a": {"id": 1}, "b": {"id": 2}})).unwrap(),
        "[2:]{id}:\n  a: 1\n  b: 2"
    );
}

#[test]
fn struct_order_and_duplicate_keys_match_the_current_json_model() {
    #[derive(serde::Serialize)]
    struct Row {
        z: u32,
        a: u32,
    }
    assert_eq!(encode(&Row { z: 1, a: 2 }).unwrap(), "z: 1\na: 2");

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
    assert_eq!(encode(&Duplicate).unwrap(), "key: 2");
}

#[test]
fn streaming_items_have_matching_count_and_no_trailing_whitespace() {
    let records = [
        json!({"path": "a.rs", "symbols": []}),
        json!({"path": "b.rs", "symbols": [1, 2]}),
    ];
    let mut output = streaming_array_header(records.len());
    for record in &records {
        let item = encode_list_item(record).unwrap();
        assert!(item.starts_with("  - "));
        output.push('\n');
        output.push_str(&item);
    }
    assert!(output.starts_with("[2]:\n  - "));
    assert_eq!(
        output
            .lines()
            .filter(|line| line.starts_with("  - "))
            .count(),
        records.len()
    );
    assert!(!output.ends_with('\n'));
    for line in output.lines() {
        assert_eq!(line, line.trim_end());
    }
}
