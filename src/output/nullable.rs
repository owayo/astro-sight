//! astro-sight DTO 向けの「欠損 nullable 列」正規化。
//!
//! # なぜ必要か
//!
//! TOON の tabular form (§9.3) は配列内の全 object が **同じキー集合** を持つことを
//! 要求する。astro-sight の compact DTO は `#[serde(skip_serializing_if = "Option::is_none")]`
//! を多用するため、たとえば `symbols` の 1 要素は関数なら `{name,kind,ln,cx}`、定数なら
//! `{name,kind,ln}` になり、キーが 1 つ欠けただけで配列全体が list form (§9.4) へ落ちる。
//! list form は要素ごとに `- key: value` を繰り返すので、**JSON より冗長**になる
//! (実測: `symbols --dir` が JSON 比 +33%)。トークン削減のために TOON を選んだのに
//! 逆効果、という最悪の結果になる。
//!
//! # 何をするか
//!
//! 配列内 object のキーが「欠けているだけ」で揃うとき、欠損キーを `null` で補って
//! tabular form を成立させる。`skip_serializing_if` は astro-sight の JSON における
//! 純粋なトークン最適化で、DTO 上は `Option<T>` = nullable フィールドなので、
//! null 補完は論理スキーマの復元にあたる。
//!
//! # なぜエンコーダ本体に入れないか
//!
//! 汎用の TOON エンコーダがこれをやると、`{"timeout": null}` (明示的に解除) と `{}`
//! (未設定 / デフォルト適用) を区別できなくする。これは TOON の最適化ではなく入力データの
//! canonicalization であり、任意の JSON に対して行ってよい変換ではない。
//! そのため `output::toon` は spec どおりの純粋なエンコーダのままにし、
//! 「astro-sight が自分の DTO について行う判断」としてこの層に置く。
//!
//! # 保証と非保証
//!
//! - **never worse**: 正規化した出力が厳密出力より **短いときだけ** 採用する
//!   (同点なら厳密側)。適用判断は `output/mod.rs` 側で実バイト数を比較して行う。
//! - **列順は決定的**: 要素を順に走査したときのキーの初出順で固定する。
//! - **round-trip は保証しない**: decode 結果には JSON が省略していたキーが `null` として
//!   現れる。DTO としての意味は保存されるが、JSON 表現との構造的一致は保証しない。
//!
//! # `Option` ではない省略フィールド
//!
//! `null` 補完が正しいのは「JSON で省略されたキー = DTO 上の `Option::None`」のときだけ。
//! `refs_internal: usize` (0 のとき省略) や `no_resolved_internal_callers: bool`
//! (false のとき省略) のように **既定値を省略している非 `Option` フィールド**を `null` で
//! 埋めると、本来 0 / false の値が「不明」と読め、DTO へ decode し直すこともできない。
//! そうしたフィールドは [`OMITTED_DEFAULTS`] に既定値を登録し、補完時は既定値で埋める。
//! 登録漏れと登録キーの衝突は、DTO のソースを走査するテスト
//! (`every_non_option_omitted_field_is_registered`) が検出する。

use super::toon::ToonValue;

/// JSON では既定値のとき省略するが `Option` ではない DTO フィールドの既定値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OmittedDefault {
    /// `usize` の 0。
    UsizeZero,
    /// `bool` の false。
    BoolFalse,
}

impl OmittedDefault {
    fn value(self) -> ToonValue {
        match self {
            OmittedDefault::UsizeZero => ToonValue::UInt(0),
            OmittedDefault::BoolFalse => ToonValue::Bool(false),
        }
    }

    /// 同じ列に現れている値がこのフィールドの型と一致するか。一致しなければ
    /// 「登録したフィールドの列」だと確認できないため補完しない。
    fn matches(self, value: &ToonValue) -> bool {
        match self {
            OmittedDefault::UsizeZero => matches!(value, ToonValue::UInt(_)),
            OmittedDefault::BoolFalse => matches!(value, ToonValue::Bool(_)),
        }
    }
}

/// 非 `Option` の省略フィールドの登録表 (`(DTO 名, 出力上のキー, 既定値)`)。
///
/// 中間表現 (`ToonValue`) は DTO の型を持たないので、実行時の照合はキー名で行う。
/// そのため登録キーは **全 DTO を通じて一意**でなければならない (テストで担保する)。
/// 配列の行にならない DTO のフィールドも登録しておく (将来行になったときに黙って
/// `null` へ戻らないようにするため)。
pub(super) const OMITTED_DEFAULTS: &[(&str, &str, OmittedDefault)] = &[
    ("ApiSymbol", "refs_internal", OmittedDefault::UsizeZero),
    (
        "ApiSymbolChange",
        "no_resolved_internal_callers",
        OmittedDefault::BoolFalse,
    ),
    (
        "ResultSummary",
        "budget_exceeded",
        OmittedDefault::BoolFalse,
    ),
    ("SkippedFiles", "truncated", OmittedDefault::BoolFalse),
    (
        "CoChangeDiagnostics",
        "filtered_deleted_candidates",
        OmittedDefault::UsizeZero,
    ),
    (
        "CoChangeDiagnostics",
        "excluded_generated_sources",
        OmittedDefault::UsizeZero,
    ),
    (
        "CoChangeDiagnostics",
        "filtered_generated_candidates",
        OmittedDefault::UsizeZero,
    ),
    (
        "CoChangeDiagnostics",
        "commit_scan_failures",
        OmittedDefault::UsizeZero,
    ),
];

fn omitted_default_for(key: &str) -> Option<OmittedDefault> {
    OMITTED_DEFAULTS
        .iter()
        .find(|(_, registered, _)| *registered == key)
        .map(|(_, _, default)| *default)
}

/// 配列内の欠損列を補い、変更したかどうかを返す。`Option` の省略は `null`、
/// [`OMITTED_DEFAULTS`] に登録された非 `Option` の省略はその既定値で埋める。
///
/// 補完対象は「全要素が非空 object」かつ「全ての値がプリミティブ」の配列だけ。
/// object を含む列 (nested-uniform 候補) は対象外にすることで、意味の取り違えと
/// 再帰的な二重エンコードのコストを同時に避ける。
pub(super) fn fill_optional_columns(value: &mut ToonValue) -> bool {
    match value {
        ToonValue::Array(items) => {
            // 先に子を処理してから自分自身を評価する (入れ子配列も畳めるように)。
            let mut changed = false;
            for item in items.iter_mut() {
                changed |= fill_optional_columns(item);
            }
            changed | fill_array(items)
        }
        ToonValue::Object(fields) => {
            let mut changed = false;
            for (_, field) in fields.iter_mut() {
                changed |= fill_optional_columns(field);
            }
            changed
        }
        _ => false,
    }
}

fn fill_array(items: &mut [ToonValue]) -> bool {
    // 1 要素では tabular にしても header 分だけ長くなる。
    if items.len() < 2 {
        return false;
    }

    // 全要素が「非空 object・キー重複なし・値は全てプリミティブ」であることを確認しつつ、
    // キーの初出順で union を組み立てる (列順の決定性はここで担保する)。
    let mut union: Vec<String> = Vec::new();
    let mut all_complete = true;
    for item in items.iter() {
        let Some(fields) = item.as_non_empty_object() else {
            return false;
        };
        for (i, (key, value)) in fields.iter().enumerate() {
            if !value.is_primitive() {
                return false;
            }
            if fields[..i].iter().any(|(prev, _)| prev == key) {
                return false;
            }
            if !union.iter().any(|existing| existing == key) {
                union.push(key.clone());
            }
        }
    }

    // 既に全要素が同じキー集合なら、厳密エンコードのままで tabular になる。
    for item in items.iter() {
        let fields = item
            .as_non_empty_object()
            .expect("checked above that every item is a non-empty object");
        if fields.len() != union.len() {
            all_complete = false;
            break;
        }
    }
    if all_complete {
        return false;
    }

    // 列ごとの補完値。`Option` の省略は `null`、登録済みの非 `Option` 省略は既定値。
    // 登録済みのキーでも、列に既定値と型の違う値があれば登録したフィールドの列だと
    // 確認できないので、配列ごと補完を見送る (意味の異なる値で埋めない)。
    let mut fills = Vec::with_capacity(union.len());
    for key in &union {
        let fill = match omitted_default_for(key) {
            None => ToonValue::Null,
            Some(default) => {
                let consistent = items
                    .iter()
                    .filter_map(|item| item.as_non_empty_object())
                    .filter_map(|fields| fields.iter().find(|(k, _)| k == key))
                    .all(|(_, value)| default.matches(value));
                if !consistent {
                    return false;
                }
                default.value()
            }
        };
        fills.push(fill);
    }

    for item in items.iter_mut() {
        let ToonValue::Object(fields) = item else {
            unreachable!("checked above that every item is a non-empty object");
        };
        let mut rebuilt = Vec::with_capacity(union.len());
        for (key, fill) in union.iter().zip(&fills) {
            let value = fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| fill.clone());
            rebuilt.push((key.clone(), value));
        }
        *fields = rebuilt;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(fields: &[(&str, ToonValue)]) -> ToonValue {
        ToonValue::Object(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    fn n(v: i128) -> ToonValue {
        ToonValue::Int(v)
    }

    #[test]
    fn missing_optional_keys_are_filled_with_null() {
        let mut value = ToonValue::Array(vec![
            obj(&[("name", n(1)), ("cx", n(6))]),
            obj(&[("name", n(2))]),
        ]);
        assert!(fill_optional_columns(&mut value));
        assert_eq!(
            value,
            ToonValue::Array(vec![
                obj(&[("name", n(1)), ("cx", n(6))]),
                obj(&[("name", n(2)), ("cx", ToonValue::Null)]),
            ])
        );
    }

    #[test]
    fn column_order_follows_first_appearance() {
        // 決定的な列順: 要素を順に見たときのキー初出順。
        let mut value = ToonValue::Array(vec![
            obj(&[("b", n(1))]),
            obj(&[("a", n(2)), ("b", n(3))]),
            obj(&[("c", n(4))]),
        ]);
        assert!(fill_optional_columns(&mut value));
        let ToonValue::Array(items) = &value else {
            panic!("array expected");
        };
        for item in items {
            let keys: Vec<&str> = item
                .as_non_empty_object()
                .unwrap()
                .iter()
                .map(|(k, _)| k.as_str())
                .collect();
            assert_eq!(keys, vec!["b", "a", "c"]);
        }
    }

    #[test]
    fn uniform_arrays_are_left_untouched() {
        // 既に tabular になる配列は触らない (キー順の入れ替えもしない)。
        let mut value = ToonValue::Array(vec![
            obj(&[("a", n(1)), ("b", n(2))]),
            obj(&[("b", n(4)), ("a", n(3))]),
        ]);
        let before = value.clone();
        assert!(!fill_optional_columns(&mut value));
        assert_eq!(value, before);
    }

    #[test]
    fn columns_holding_non_primitives_are_skipped() {
        // object / 配列を含む列は意味の取り違えを避けるため対象外。
        let mut value = ToonValue::Array(vec![
            obj(&[("a", n(1)), ("nested", obj(&[("x", n(1))]))]),
            obj(&[("a", n(2))]),
        ]);
        let before = value.clone();
        assert!(!fill_optional_columns(&mut value));
        assert_eq!(value, before);
    }

    #[test]
    fn single_element_arrays_are_skipped() {
        let mut value = ToonValue::Array(vec![obj(&[("a", n(1))])]);
        let before = value.clone();
        assert!(!fill_optional_columns(&mut value));
        assert_eq!(value, before);
    }

    #[test]
    fn arrays_of_primitives_are_skipped() {
        let mut value = ToonValue::Array(vec![n(1), n(2)]);
        let before = value.clone();
        assert!(!fill_optional_columns(&mut value));
        assert_eq!(value, before);
    }

    #[test]
    fn nested_arrays_are_normalized_too() {
        let mut value = obj(&[(
            "files",
            ToonValue::Array(vec![obj(&[(
                "symbols",
                ToonValue::Array(vec![obj(&[("n", n(1)), ("cx", n(2))]), obj(&[("n", n(3))])]),
            )])]),
        )]);
        assert!(fill_optional_columns(&mut value));
        let ToonValue::Object(root) = &value else {
            panic!("object expected");
        };
        let ToonValue::Array(files) = &root[0].1 else {
            panic!("array expected");
        };
        let ToonValue::Object(file) = &files[0] else {
            panic!("object expected");
        };
        let ToonValue::Array(symbols) = &file[0].1 else {
            panic!("array expected");
        };
        assert_eq!(symbols[1], obj(&[("n", n(3)), ("cx", ToonValue::Null)]));
    }
    /// 非 `Option` の省略フィールドは `null` ではなく既定値で埋める。
    /// 対照として、同じ配列の `Option` 列は従来どおり `null` で埋める。
    #[test]
    fn registered_non_option_keys_are_filled_with_their_default() {
        let u = |v: u128| ToonValue::UInt(v);
        let mut value = ToonValue::Array(vec![
            obj(&[
                ("name", n(1)),
                ("refs_internal", u(2)),
                ("line", n(3)),
                ("no_resolved_internal_callers", ToonValue::Bool(true)),
            ]),
            obj(&[("name", n(2))]),
        ]);
        assert!(fill_optional_columns(&mut value));
        assert_eq!(
            value,
            ToonValue::Array(vec![
                obj(&[
                    ("name", n(1)),
                    ("refs_internal", u(2)),
                    ("line", n(3)),
                    ("no_resolved_internal_callers", ToonValue::Bool(true)),
                ]),
                obj(&[
                    ("name", n(2)),
                    ("refs_internal", u(0)),
                    ("line", ToonValue::Null),
                    ("no_resolved_internal_callers", ToonValue::Bool(false)),
                ]),
            ])
        );
    }

    /// 登録キーでも列の値の型が既定値と合わなければ、登録したフィールドの列だと確認
    /// できないので補完しない (`null` でも既定値でも埋めない)。
    #[test]
    fn registered_key_with_mismatched_type_is_not_filled() {
        let mut value = ToonValue::Array(vec![
            obj(&[
                ("name", n(1)),
                ("refs_internal", ToonValue::Str("x".into())),
            ]),
            obj(&[("name", n(2))]),
        ]);
        let before = value.clone();
        assert!(!fill_optional_columns(&mut value));
        assert_eq!(value, before);
    }

    /// review の `api_changes` を TOON にしたとき、0 / false のセルが `null` にならない。
    /// JSON は従来どおり 0 / false のキーを省略する (バイト列を固定して確かめる)。
    #[test]
    fn review_api_rows_render_defaults_instead_of_null() {
        use crate::models::review::{ApiChanges, ApiSymbol, ApiSymbolChange};
        use crate::output::{JsonStyle, OutputFormat, OutputOptions, serialize_document};

        let symbol = |name: &str, refs_internal: usize| ApiSymbol {
            name: name.into(),
            kind: "function".into(),
            file: "lib/base.ts".into(),
            refs_internal,
        };
        let change = |name: &str, no_callers: bool| ApiSymbolChange {
            name: name.into(),
            kind: "function".into(),
            file: "lib/base.ts".into(),
            old_signature: Some(format!("function {name}(a)")),
            new_signature: Some(format!("function {name}(a, b)")),
            no_resolved_internal_callers: no_callers,
            contract_change: None,
        };
        let changes = ApiChanges {
            added: vec![symbol("makeFoo", 0), symbol("Shape", 1)],
            modified: vec![change("oldApi", true), change("usedApi", false)],
            ..Default::default()
        };

        let toon = serialize_document(
            &changes,
            OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact),
        )
        .expect("toon");
        assert!(
            toon.contains(concat!(
                "added[2]{name,kind,file,refs_internal}:\n",
                "  makeFoo,function,lib/base.ts,0\n",
                "  Shape,function,lib/base.ts,1"
            )),
            "{toon}"
        );
        assert!(
            toon.contains("  usedApi,function,lib/base.ts,function usedApi(a),\"function usedApi(a, b)\",false"),
            "{toon}"
        );
        assert!(
            !toon.contains("null"),
            "非 Option の列を null で埋めない: {toon}"
        );

        assert_eq!(
            serde_json::to_string(&changes).expect("json"),
            concat!(
                r#"{"added":[{"name":"makeFoo","kind":"function","file":"lib/base.ts"},"#,
                r#"{"name":"Shape","kind":"function","file":"lib/base.ts","refs_internal":1}],"#,
                r#""removed":[],"modified":[{"name":"oldApi","kind":"function","file":"lib/base.ts","#,
                r#""old_signature":"function oldApi(a)","new_signature":"function oldApi(a, b)","#,
                r#""no_resolved_internal_callers":true},{"name":"usedApi","kind":"function","#,
                r#""file":"lib/base.ts","old_signature":"function usedApi(a)","#,
                r#""new_signature":"function usedApi(a, b)"}]}"#
            ),
            "JSON は 0 / false を省略したまま"
        );
    }

    /// TOON で描画されない DTO (JSON 固定のプロトコル面 / 入力 DTO) の省略フィールド。
    /// 登録表の対象外だが、登録キーとの衝突判定には参加する。
    const NEVER_RENDERED_AS_TOON: &[(&str, &str)] = &[
        // `review --hook` の出力は JSON 固定
        ("HookAddedSymbol", "ri"),
        ("HookModifiedSymbol", "no_callers"),
        // session の入力 DTO
        ("AstgenRequest", "include_generated"),
    ];

    /// `derive(Serialize)` な struct のフィールド 1 件。
    #[derive(Debug)]
    struct SerializedField {
        owner: String,
        key: String,
        rust_type: String,
        /// `skip_serializing_if` で省略されうる、`Option` ではないプリミティブ型のフィールド。
        omitted_non_option: bool,
        location: String,
    }

    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("read src dir")
            .map(|e| e.expect("dir entry").path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                // テストコードの struct は出力 DTO ではない。
                if name != "tests" {
                    rust_sources(&path, out);
                }
            } else if name.ends_with(".rs") && name != "tests.rs" {
                out.push(path);
            }
        }
    }

    fn node_text<'a>(node: tree_sitter::Node<'_>, source: &'a str) -> &'a str {
        &source[node.byte_range()]
    }

    /// `#[serde(...)]` の引数から `name = "value"` の値を取り出す。`skip` のような
    /// 値なしの指定は `Some("")` を返す。
    fn serde_arg(attrs: &[tree_sitter::Node<'_>], source: &str, name: &str) -> Option<String> {
        for attr in attrs {
            let text = node_text(*attr, source);
            if !text.starts_with("#[serde") {
                continue;
            }
            let Some(attribute) = attr.named_child(0) else {
                continue;
            };
            let Some(args) = attribute.child_by_field_name("arguments") else {
                continue;
            };
            let mut cursor = args.walk();
            let tokens: Vec<_> = args.named_children(&mut cursor).collect();
            for (i, token) in tokens.iter().enumerate() {
                if token.kind() != "identifier" || node_text(*token, source) != name {
                    continue;
                }
                return Some(match tokens.get(i + 1) {
                    Some(next) if next.kind() == "string_literal" => {
                        node_text(*next, source).trim_matches('"').to_string()
                    }
                    _ => String::new(),
                });
            }
        }
        None
    }

    fn is_primitive_type(ty: tree_sitter::Node<'_>, source: &str) -> bool {
        let text = node_text(ty, source);
        match ty.kind() {
            "primitive_type" => true,
            "type_identifier" => text == "String",
            "reference_type" => text.ends_with("str"),
            _ => false,
        }
    }

    fn collect_struct_fields(
        item: tree_sitter::Node<'_>,
        source: &str,
        location: &str,
        out: &mut Vec<SerializedField>,
    ) {
        let owner = item
            .child_by_field_name("name")
            .map(|n| node_text(n, source).to_string())
            .unwrap_or_default();
        let Some(body) = item.child_by_field_name("body") else {
            return;
        };
        let mut attrs = Vec::new();
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "attribute_item" => attrs.push(child),
                "line_comment" | "block_comment" => {}
                "field_declaration" => {
                    let serialized = serde_arg(&attrs, source, "skip").is_none()
                        && serde_arg(&attrs, source, "flatten").is_none();
                    if serialized {
                        let name = child
                            .child_by_field_name("name")
                            .map(|n| node_text(n, source).to_string())
                            .unwrap_or_default();
                        let ty = child.child_by_field_name("type").expect("field type");
                        let omitted = serde_arg(&attrs, source, "skip_serializing_if").is_some();
                        let key = match serde_arg(&attrs, source, "rename") {
                            None => name,
                            Some(renamed) if !renamed.is_empty() => renamed,
                            // `rename(serialize = "..")` 等はキー名を算出できない (黙って誤った
                            // キーで照合すると衝突・登録漏れを見逃す)。
                            Some(_) => panic!(
                                "{location}: rename の形式がこのテストで未対応 (キー名を算出できない)"
                            ),
                        };
                        out.push(SerializedField {
                            owner: owner.clone(),
                            key,
                            rust_type: node_text(ty, source).to_string(),
                            omitted_non_option: omitted && is_primitive_type(ty, source),
                            location: format!("{location}:{}", child.start_position().row + 1),
                        });
                    }
                    attrs.clear();
                }
                _ => attrs.clear(),
            }
        }
    }

    /// 構文木を辿り、`derive(Serialize)` な struct のフィールドを集める。
    /// `#[cfg(test)]` / `#[test]` の付いた item (テストモジュール・テスト関数) は辿らない。
    fn collect_serialized_fields(
        node: tree_sitter::Node<'_>,
        source: &str,
        file: &str,
        out: &mut Vec<SerializedField>,
    ) {
        let mut attrs: Vec<tree_sitter::Node<'_>> = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "attribute_item" => {
                    attrs.push(child);
                    continue;
                }
                "line_comment" | "block_comment" => continue,
                _ => {}
            }
            let attr_texts: Vec<&str> = attrs.iter().map(|a| node_text(*a, source)).collect();
            let is_test_item = attr_texts
                .iter()
                .any(|t| t.contains("cfg(test)") || *t == "#[test]");
            if !is_test_item {
                if child.kind() == "struct_item" {
                    let derives_serialize = attr_texts.iter().any(|t| {
                        t.starts_with("#[derive")
                            && t.split(|c: char| !c.is_alphanumeric() && c != '_')
                                .any(|token| token == "Serialize")
                    });
                    if derives_serialize {
                        assert!(
                            !attr_texts.iter().any(|t| t.contains("rename_all")),
                            "{file}: struct レベルの rename_all はこのテストが未対応 (キー名を算出できない)"
                        );
                        collect_struct_fields(child, source, file, out);
                    }
                }
                collect_serialized_fields(child, source, file, out);
            }
            attrs.clear();
        }
    }

    /// DTO のソースから「非 `Option` の省略フィールド」を機械的に列挙し、登録表と突き合わせる。
    ///
    /// 登録漏れがあると、そのフィールドが配列の行になった瞬間に既定値のセルが黙って
    /// `null` (= 不明) へ戻る。登録キーが他のフィールドと同名だと、実行時のキー名照合が
    /// 別フィールドの欠損まで既定値で埋めてしまう。どちらも出力上はエラーにならないため
    /// ソース側で検出する。
    #[test]
    fn every_non_option_omitted_field_is_registered() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&src_dir, &mut files);

        let mut fields = Vec::new();
        for path in &files {
            let source = std::fs::read_to_string(path).expect("read source");
            let tree = crate::engine::parser::parse_source(
                source.as_bytes(),
                crate::language::LangId::Rust,
            )
            .expect("parse source");
            let file = path
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .unwrap_or(path)
                .display()
                .to_string();
            collect_serialized_fields(tree.root_node(), &source, &file, &mut fields);
        }
        assert!(
            fields
                .iter()
                .any(|f| f.owner == "ApiSymbol" && f.key == "refs_internal"),
            "走査が DTO を拾えていない (テスト自体の故障): {} fields",
            fields.len()
        );

        let omitted: Vec<&SerializedField> =
            fields.iter().filter(|f| f.omitted_non_option).collect();
        for field in &omitted {
            let registered = OMITTED_DEFAULTS
                .iter()
                .find(|(owner, key, _)| *owner == field.owner && *key == field.key);
            let exempt = NEVER_RENDERED_AS_TOON
                .iter()
                .any(|(owner, key)| *owner == field.owner && *key == field.key);
            match registered {
                Some((_, _, default)) => {
                    let expected_type = match default {
                        OmittedDefault::UsizeZero => "usize",
                        OmittedDefault::BoolFalse => "bool",
                    };
                    assert_eq!(
                        field.rust_type, expected_type,
                        "{}: {}.{} の既定値の型が登録と合わない",
                        field.location, field.owner, field.key
                    );
                }
                None => assert!(
                    exempt,
                    "{}: {}.{} ({}) は Option でないのに JSON で省略される。nullable::OMITTED_DEFAULTS \
                     に既定値を登録するか、Option にすること (TOON の欠損補完が null で埋めてしまう)",
                    field.location, field.owner, field.key, field.rust_type
                ),
            }
        }

        // 登録表・除外表に、もう存在しないフィールドが残っていない。
        for (owner, key) in OMITTED_DEFAULTS
            .iter()
            .map(|(o, k, _)| (*o, *k))
            .chain(NEVER_RENDERED_AS_TOON.iter().copied())
        {
            assert!(
                omitted.iter().any(|f| f.owner == owner && f.key == key),
                "{owner}.{key} は非 Option の省略フィールドとして見つからない (登録が古い)"
            );
        }

        // 実行時はキー名で照合するので、登録キーを出力するフィールドは 1 つだけでなければならない。
        for (owner, key, _) in OMITTED_DEFAULTS {
            let same_key: Vec<String> = fields
                .iter()
                .filter(|f| f.key == *key)
                .map(|f| format!("{}.{} ({})", f.owner, f.key, f.location))
                .collect();
            assert_eq!(
                same_key.len(),
                1,
                "登録キー {owner}.{key} と同名のフィールドがある: {same_key:?}"
            );
        }
    }
}
