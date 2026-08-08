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

use super::toon::ToonValue;

/// 配列内の欠損 nullable 列を `null` で補い、変更したかどうかを返す。
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

    for item in items.iter_mut() {
        let ToonValue::Object(fields) = item else {
            unreachable!("checked above that every item is a non-empty object");
        };
        let mut rebuilt = Vec::with_capacity(union.len());
        for key in &union {
            let value = fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or(ToonValue::Null);
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
}
