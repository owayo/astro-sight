//! TOON v4.1 エンコーダの仕様適合テスト。
//!
//! 各テストは仕様の節番号を明記し、「その節が要求する形」を期待値に固定する。

use super::value::ToonValue;
use super::{encode, encode_list_item, encode_value, streaming_array_header, to_toon_value};

fn obj(fields: &[(&str, ToonValue)]) -> ToonValue {
    ToonValue::Object(
        fields
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
    )
}

fn s(v: &str) -> ToonValue {
    ToonValue::Str(v.to_string())
}

fn n(v: i128) -> ToonValue {
    ToonValue::Int(v)
}

fn arr(items: &[ToonValue]) -> ToonValue {
    ToonValue::Array(items.to_vec())
}

fn enc(value: &ToonValue) -> String {
    encode_value(value).expect("encode succeeds")
}

// ---------------------------------------------------------------------------
// §5 ルート形式
// ---------------------------------------------------------------------------

#[test]
fn root_object_emits_fields_at_depth_zero() {
    assert_eq!(
        enc(&obj(&[("id", n(1)), ("name", s("Ada"))])),
        "id: 1\nname: Ada"
    );
}

#[test]
fn empty_root_object_is_an_empty_document() {
    // §8: ルートの空 object は行を 1 つも持たない。
    assert_eq!(enc(&ToonValue::Object(vec![])), "");
}

#[test]
fn root_primitive_is_a_single_line() {
    assert_eq!(enc(&s("hello")), "hello");
    assert_eq!(enc(&n(42)), "42");
    assert_eq!(enc(&ToonValue::Bool(true)), "true");
    assert_eq!(enc(&ToonValue::Null), "null");
}

#[test]
fn root_arrays_omit_the_key() {
    // §9.1 / §9.3: ルート配列は keyless header。
    assert_eq!(enc(&arr(&[n(1), n(2), n(3)])), "[3]: 1,2,3");
    assert_eq!(enc(&ToonValue::Array(vec![])), "[]");
    assert_eq!(
        enc(&arr(&[
            obj(&[("id", n(1)), ("n", s("a"))]),
            obj(&[("id", n(2)), ("n", s("b"))]),
        ])),
        "[2]{id,n}:\n  1,a\n  2,b"
    );
}

#[test]
fn document_has_no_trailing_newline() {
    // §12: encoder は末尾改行を出さない。
    for value in [
        obj(&[("a", n(1))]),
        arr(&[n(1)]),
        s("x"),
        arr(&[obj(&[("a", n(1))]), obj(&[("a", n(2))])]),
    ] {
        let text = enc(&value);
        assert!(!text.ends_with('\n'), "{text:?} must not end with newline");
    }
}

#[test]
fn no_line_has_trailing_whitespace() {
    // §12: 行末空白を出さない。空 object フィールドの `key:` が代表例。
    let text = enc(&obj(&[
        ("empty_obj", ToonValue::Object(vec![])),
        ("nested", obj(&[("a", n(1))])),
        (
            "list",
            arr(&[obj(&[("a", n(1)), ("b", arr(&[n(1), n(2)]))])]),
        ),
    ]));
    for line in text.lines() {
        assert_eq!(line, line.trim_end(), "trailing whitespace in {line:?}");
    }
}

// ---------------------------------------------------------------------------
// §8 オブジェクト
// ---------------------------------------------------------------------------

#[test]
fn nested_objects_indent_by_two_spaces() {
    assert_eq!(
        enc(&obj(&[("user", obj(&[("id", n(1)), ("name", s("Ada"))]))])),
        "user:\n  id: 1\n  name: Ada"
    );
}

#[test]
fn empty_nested_object_is_bare_key_colon() {
    // §8: `key:` 単独は空/入れ子 object。空配列 (`key: []`) と衝突しない。
    assert_eq!(enc(&obj(&[("meta", ToonValue::Object(vec![]))])), "meta:");
}

#[test]
fn key_order_follows_encounter_order() {
    // §2: object の key 順は encoder が遭遇した順。辞書順ではない。
    assert_eq!(
        enc(&obj(&[("z", n(1)), ("a", n(2)), ("m", n(3))])),
        "z: 1\na: 2\nm: 3"
    );
}

// ---------------------------------------------------------------------------
// §9.1 プリミティブ配列
// ---------------------------------------------------------------------------

#[test]
fn primitive_arrays_are_inline_with_length() {
    assert_eq!(
        enc(&obj(&[("tags", arr(&[s("admin"), s("ops"), s("dev")]))])),
        "tags[3]: admin,ops,dev"
    );
}

#[test]
fn empty_array_field_uses_the_bracket_literal() {
    // §9.1: 空配列は `key: []`。legacy の `key[0]:` は出さない。
    assert_eq!(enc(&obj(&[("tags", ToonValue::Array(vec![]))])), "tags: []");
}

#[test]
fn delimiter_bearing_values_are_quoted_inside_inline_arrays() {
    assert_eq!(
        enc(&obj(&[("tags", arr(&[s("a,b"), s("c")]))])),
        r#"tags[2]: "a,b",c"#
    );
}

// ---------------------------------------------------------------------------
// §9.3 tabular form
// ---------------------------------------------------------------------------

#[test]
fn uniform_object_arrays_collapse_into_a_table() {
    assert_eq!(
        enc(&obj(&[(
            "items",
            arr(&[
                obj(&[("sku", s("A1")), ("qty", n(2))]),
                obj(&[("sku", s("B2")), ("qty", n(1))]),
            ])
        )])),
        "items[2]{sku,qty}:\n  A1,2\n  B2,1"
    );
}

#[test]
fn tabular_rows_follow_header_order_even_when_row_keys_are_reordered() {
    // §9.3: キー集合が同じなら object ごとの順序は違ってよい。行は header 順で書く。
    assert_eq!(
        enc(&arr(&[
            obj(&[("a", n(1)), ("b", n(2))]),
            obj(&[("b", n(4)), ("a", n(3))]),
        ])),
        "[2]{a,b}:\n  1,2\n  3,4"
    );
}

#[test]
fn nested_uniform_columns_become_nested_field_groups() {
    // §9.3: nested-uniform 列は `field{sub…}` に畳み、行は leaf を深さ優先で並べる。
    assert_eq!(
        enc(&obj(&[(
            "orders",
            arr(&[
                obj(&[
                    ("id", n(1)),
                    ("customer", obj(&[("name", s("Ada")), ("country", s("DK"))])),
                    ("total", n(99)),
                ]),
                obj(&[
                    ("id", n(2)),
                    ("customer", obj(&[("name", s("Bob")), ("country", s("UK"))])),
                    ("total", n(149)),
                ]),
            ])
        )])),
        "orders[2]{id,customer{name,country},total}:\n  1,Ada,DK,99\n  2,Bob,UK,149"
    );
}

#[test]
fn non_uniform_arrays_fall_back_to_list_form() {
    // §9.4: キー集合が違う / 列が混在するなら list form へ保守的に倒す。
    assert_eq!(
        enc(&arr(&[obj(&[("a", n(1))]), obj(&[("b", n(2))])])),
        "[2]:\n  - a: 1\n  - b: 2"
    );
    // null と object が混ざる列は tabular 不可。
    assert_eq!(
        enc(&arr(&[
            obj(&[("a", ToonValue::Null)]),
            obj(&[("a", obj(&[("x", n(1))]))]),
        ])),
        "[2]:\n  - a: null\n  - a:\n      x: 1"
    );
}

#[test]
fn arrays_containing_an_empty_object_use_list_form() {
    // §9.3: 空 object を含む配列は tabular 不可。
    assert_eq!(
        enc(&arr(&[obj(&[("a", n(1))]), ToonValue::Object(vec![])])),
        "[2]:\n  - a: 1\n  -"
    );
}

#[test]
fn columns_holding_arrays_disqualify_tabular_form() {
    assert_eq!(
        enc(&arr(&[
            obj(&[("a", arr(&[n(1)]))]),
            obj(&[("a", arr(&[n(2)]))]),
        ])),
        "[2]:\n  - a[1]: 1\n  - a[1]: 2"
    );
}

// ---------------------------------------------------------------------------
// §9.5 keyed tabular form
// ---------------------------------------------------------------------------

#[test]
fn objects_of_uniform_objects_collapse_into_keyed_tables() {
    assert_eq!(
        enc(&obj(&[(
            "users",
            obj(&[
                ("alice", obj(&[("age", n(30)), ("city", s("Berlin"))])),
                ("bob", obj(&[("age", n(25)), ("city", s("Oslo"))])),
            ])
        )])),
        "users[2:]{age,city}:\n  alice: 30,Berlin\n  bob: 25,Oslo"
    );
}

#[test]
fn keyed_tabular_applies_at_the_document_root() {
    assert_eq!(
        enc(&obj(&[
            ("alice", obj(&[("age", n(30))])),
            ("bob", obj(&[("age", n(25))])),
        ])),
        "[2:]{age}:\n  alice: 30\n  bob: 25"
    );
}

#[test]
fn single_entry_objects_stay_nested() {
    // §9.5: エントリ 2 件未満では keyed 化しない。
    assert_eq!(
        enc(&obj(&[(
            "users",
            obj(&[("alice", obj(&[("age", n(30))]))])
        )])),
        "users:\n  alice:\n    age: 30"
    );
}

// ---------------------------------------------------------------------------
// §9.4 / §10 list form
// ---------------------------------------------------------------------------

#[test]
fn mixed_arrays_use_hyphen_list_items() {
    assert_eq!(
        enc(&obj(&[(
            "items",
            arr(&[n(1), obj(&[("id", n(2))]), s("text")])
        )])),
        "items[3]:\n  - 1\n  - id: 2\n  - text"
    );
}

#[test]
fn list_item_objects_carry_the_first_field_on_the_hyphen_line() {
    // §10: 残りのフィールドは depth+1、最初のフィールドが開くスコープは depth+2。
    assert_eq!(
        enc(&arr(&[
            obj(&[("a", n(1)), ("b", n(2))]),
            obj(&[("c", obj(&[("d", n(3))])), ("e", n(4))]),
        ])),
        "[2]:\n  - a: 1\n    b: 2\n  - c:\n      d: 3\n    e: 4"
    );
}

#[test]
fn list_item_object_with_tabular_first_field_puts_rows_two_levels_deeper() {
    // §10: 先頭フィールドが tabular 配列なら header は hyphen 行、行は depth+2。
    assert_eq!(
        enc(&arr(&[
            obj(&[
                ("rows", arr(&[obj(&[("x", n(1))]), obj(&[("x", n(2))])])),
                ("tail", n(9)),
            ]),
            obj(&[("other", n(0))]),
        ])),
        "[2]:\n  - rows[2]{x}:\n      1\n      2\n    tail: 9\n  - other: 0"
    );
}

#[test]
fn empty_object_list_item_is_a_bare_hyphen() {
    assert_eq!(
        enc(&arr(&[ToonValue::Object(vec![]), obj(&[("a", n(1))])])),
        "[2]:\n  -\n  - a: 1"
    );
}

#[test]
fn arrays_of_primitive_arrays_use_inner_headers() {
    // §9.2: 内側の空配列は `- []` ではなく `- [0]:`。
    assert_eq!(
        enc(&obj(&[(
            "matrix",
            arr(&[arr(&[n(1), n(2)]), ToonValue::Array(vec![])])
        )])),
        "matrix[2]:\n  - [2]: 1,2\n  - [0]:"
    );
}

#[test]
fn nested_object_arrays_inside_list_items_stay_in_list_form() {
    // §9.4: list item 位置の配列は keyless header なので tabular 化できない。
    assert_eq!(
        enc(&arr(&[arr(&[obj(&[("a", n(1))]), obj(&[("a", n(2))])])])),
        "[1]:\n  - [2]:\n    - a: 1\n    - a: 2"
    );
}

// ---------------------------------------------------------------------------
// §7 quoting (encoder の出力側)
// ---------------------------------------------------------------------------

#[test]
fn ambiguous_strings_are_quoted_in_every_position() {
    assert_eq!(
        enc(&obj(&[
            ("empty", s("")),
            ("numlike", s("42")),
            ("keyword", s("true")),
            ("dash", s("-x")),
            ("hash", s("#x")),
            ("colon", s("a:b")),
        ])),
        concat!(
            "empty: \"\"\n",
            "numlike: \"42\"\n",
            "keyword: \"true\"\n",
            "dash: \"-x\"\n",
            "hash: \"#x\"\n",
            "colon: \"a:b\""
        )
    );
}

#[test]
fn keys_needing_quotes_are_quoted_in_headers_too() {
    assert_eq!(
        enc(&obj(&[("my-key", arr(&[n(1), n(2)]))])),
        r#""my-key"[2]: 1,2"#
    );
    assert_eq!(
        enc(&arr(&[obj(&[("my-key", n(1))]), obj(&[("my-key", n(2))])])),
        "[2]{\"my-key\"}:\n  1\n  2"
    );
}

// ---------------------------------------------------------------------------
// serde 連携
// ---------------------------------------------------------------------------

#[test]
fn serde_structs_keep_declaration_order() {
    #[derive(serde::Serialize)]
    struct Row {
        name: String,
        line: usize,
        exported: bool,
    }
    #[derive(serde::Serialize)]
    struct Doc {
        path: String,
        symbols: Vec<Row>,
    }

    let doc = Doc {
        path: "src/a.rs".into(),
        symbols: vec![
            Row {
                name: "foo".into(),
                line: 10,
                exported: true,
            },
            Row {
                name: "bar".into(),
                line: 20,
                exported: false,
            },
        ],
    };
    assert_eq!(
        encode(&doc).unwrap(),
        "path: src/a.rs\nsymbols[2]{name,line,exported}:\n  foo,10,true\n  bar,20,false"
    );
}

#[test]
fn skipped_options_do_not_appear() {
    #[derive(serde::Serialize)]
    struct Doc {
        a: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        b: Option<u32>,
        c: Option<u32>,
    }
    assert_eq!(
        encode(&Doc {
            a: 1,
            b: None,
            c: None
        })
        .unwrap(),
        "a: 1\nc: null"
    );
}

#[test]
fn enums_use_serde_external_tagging() {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum Kind {
        Function,
    }
    assert_eq!(encode(&Kind::Function).unwrap(), "function");
}

#[test]
fn numbers_round_trip_through_serde() {
    #[derive(serde::Serialize)]
    struct Nums {
        int: i64,
        uint: u64,
        big: u64,
        float: f64,
        whole: f64,
        nan: f64,
    }
    assert_eq!(
        encode(&Nums {
            int: -7,
            uint: 7,
            big: u64::MAX,
            float: 0.25,
            whole: 3.0,
            nan: f64::NAN,
        })
        .unwrap(),
        "int: -7\nuint: 7\nbig: 18446744073709551615\nfloat: 0.25\nwhole: 3\nnan: null"
    );
}

// ---------------------------------------------------------------------------
// ストリーミング API (バッチ経路)
// ---------------------------------------------------------------------------

#[test]
fn streaming_pieces_assemble_into_the_same_document_as_list_form() {
    #[derive(serde::Serialize)]
    struct Rec {
        path: String,
        count: usize,
    }
    let records = vec![
        Rec {
            path: "a.rs".into(),
            count: 1,
        },
        Rec {
            path: "b.rs".into(),
            count: 2,
        },
    ];

    let mut assembled = streaming_array_header(records.len());
    for record in &records {
        assembled.push('\n');
        assembled.push_str(&encode_list_item(&to_toon_value(record).unwrap()).unwrap());
    }

    assert_eq!(
        assembled,
        "[2]:\n  - path: a.rs\n    count: 1\n  - path: b.rs\n    count: 2"
    );
}

#[test]
fn streaming_header_uses_the_empty_array_literal_for_zero_items() {
    // §9.1: ルート位置の空配列は `[]`。legacy の `[0]:` は encoder が出してはならない。
    assert_eq!(streaming_array_header(0), "[]");
    assert_eq!(streaming_array_header(1), "[1]:");
}

#[test]
fn streaming_items_are_indented_for_depth_one() {
    let item = encode_list_item(&to_toon_value(&serde_json::json!({"a": 1})).unwrap()).unwrap();
    assert!(item.starts_with("  - "), "unexpected item: {item:?}");
}
