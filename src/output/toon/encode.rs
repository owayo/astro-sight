//! TOON v4.1 エンコーダ本体 (§5, §8, §9, §10, §12)。
//!
//! 出力は decoder が strict mode で受け取れる canonical 形を目指す:
//! - uniform な object 配列は tabular form (§9.3)、uniform な object 値を持つ object は
//!   keyed tabular form (§9.5) へ畳む
//! - それ以外の配列は list form (§9.4)、object はインデント形式 (§8)
//! - インデントは 2 スペース固定、行末空白なし、末尾改行なし (§12)

use super::ToonError;
use super::scalar::{self, DELIMITER};
use super::value::ToonValue;

/// 1 レベルあたりのインデント幅 (§12 の既定値)。
pub const INDENT: usize = 2;

/// tabular header のフィールド。`Group` は nested-uniform 列 (§9.3 の nested field group)。
#[derive(Debug, Clone, PartialEq)]
enum Field {
    Leaf(String),
    Group(String, Vec<Field>),
}

/// ルートドキュメントをエンコードする (§5)。末尾に改行は付けない (§12)。
pub fn encode_document(value: &ToonValue) -> Result<String, ToonError> {
    let mut out = String::new();

    match value {
        // 空 object はルートでは空ドキュメント (§8)。
        ToonValue::Object(fields) if fields.is_empty() => {}
        ToonValue::Object(fields) => {
            if let Some(columns) = keyed_tabular_columns(fields) {
                write_keyed_header(None, fields.len(), &columns, 0, &mut out);
                write_keyed_rows(fields, &columns, 1, &mut out)?;
            } else {
                write_object_fields(fields, 0, &mut out)?;
            }
        }
        ToonValue::Array(items) => write_array_body(None, items, 0, &mut out)?,
        primitive => {
            write_primitive(primitive, &mut out)?;
            out.push('\n');
        }
    }

    // §12: encoder は末尾改行を出さない。
    if out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

/// ストリーミング用のルート配列ヘッダ (list form, §9.4)。要素数だけ先に分かっていれば
/// 本体を溜めずに出せる。
///
/// 要素数 0 のときは `[]` を返す — §9.1 は「ルート位置の空配列は `[]` を出す。legacy の
/// `[0]:` 形式は **出してはならない**」と定めている (decoder は両方受け付ける)。
pub fn list_form_array_header(len: usize) -> String {
    if len == 0 {
        return "[]".to_string();
    }
    format!("[{len}]:")
}

/// ストリーミング用の list item 1 件 (`depth` 段目)。末尾改行なし。
pub fn encode_list_item_at(value: &ToonValue, depth: usize) -> Result<String, ToonError> {
    let mut out = String::new();
    write_list_item(value, depth, &mut out)?;
    if out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// object
// ---------------------------------------------------------------------------

fn write_object_fields(
    fields: &[(String, ToonValue)],
    depth: usize,
    out: &mut String,
) -> Result<(), ToonError> {
    for (key, value) in fields {
        write_field(key, value, depth, out)?;
    }
    Ok(())
}

fn write_field(
    key: &str,
    value: &ToonValue,
    depth: usize,
    out: &mut String,
) -> Result<(), ToonError> {
    match value {
        ToonValue::Array(items) => write_array_body(Some(key), items, depth, out)?,
        ToonValue::Object(nested) if nested.is_empty() => {
            // 空 object は `key:` 単独行 (§8)。空配列 `key: []` と区別される。
            write_indent(depth, out);
            scalar::write_key(key, out);
            out.push_str(":\n");
        }
        ToonValue::Object(nested) => {
            if let Some(columns) = keyed_tabular_columns(nested) {
                write_keyed_header(Some(key), nested.len(), &columns, depth, out);
                write_keyed_rows(nested, &columns, depth + 1, out)?;
            } else {
                write_indent(depth, out);
                scalar::write_key(key, out);
                out.push_str(":\n");
                write_object_fields(nested, depth + 1, out)?;
            }
        }
        primitive => {
            write_indent(depth, out);
            scalar::write_key(key, out);
            out.push_str(": ");
            write_primitive(primitive, out)?;
            out.push('\n');
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// array
// ---------------------------------------------------------------------------

/// 配列の header 以降を書く。`key` が `None` ならルート配列 (keyless header)。
fn write_array_body(
    key: Option<&str>,
    items: &[ToonValue],
    depth: usize,
    out: &mut String,
) -> Result<(), ToonError> {
    write_indent(depth, out);

    // 空配列は object field 位置なら `key: []`、ルートなら `[]` (§9.1)。
    if items.is_empty() {
        match key {
            Some(key) => {
                scalar::write_key(key, out);
                out.push_str(": []\n");
            }
            None => out.push_str("[]\n"),
        }
        return Ok(());
    }

    // プリミティブのみなら inline form (§9.1)。
    if items.iter().all(ToonValue::is_primitive) {
        write_array_header(key, items.len(), None, out);
        out.push(' ');
        write_cells_joined(items, out)?;
        out.push('\n');
        return Ok(());
    }

    // uniform な object 配列は tabular form (§9.3)。
    if let Some(columns) = tabular_columns(items) {
        write_array_header(key, items.len(), Some(&columns), out);
        out.push('\n');
        for item in items {
            let fields = item
                .as_non_empty_object()
                .ok_or_else(|| ToonError::Encode("tabular row is not an object".into()))?;
            write_indent(depth + 1, out);
            write_row_cells(fields, &columns, out)?;
            out.push('\n');
        }
        return Ok(());
    }

    // 残りは list form (§9.4)。header 側で colon まで書かれている。
    write_array_header(key, items.len(), None, out);
    out.push('\n');
    for item in items {
        write_list_item(item, depth + 1, out)?;
    }
    Ok(())
}

/// `key[N]` / `[N]` / `key[N]{fields}:` を書く。field list が無い場合は colon を
/// 呼び出し側が付ける (inline 配列は `: ` + 値、list form は `:` + 改行のため)。
fn write_array_header(key: Option<&str>, len: usize, columns: Option<&[Field]>, out: &mut String) {
    if let Some(key) = key {
        scalar::write_key(key, out);
    }
    out.push('[');
    out.push_str(&len.to_string());
    out.push(']');
    match columns {
        Some(columns) => {
            out.push('{');
            write_field_list(columns, out);
            out.push_str("}:");
        }
        None => out.push(':'),
    }
}

fn write_list_item(value: &ToonValue, depth: usize, out: &mut String) -> Result<(), ToonError> {
    match value {
        // 空 object の list item は裸のハイフン (§10)。
        ToonValue::Object(fields) if fields.is_empty() => {
            write_indent(depth, out);
            out.push_str("-\n");
        }
        ToonValue::Object(fields) => {
            // §10 の深さモデル: hyphen 行に載る最初のフィールドは depth+1 相当で、
            // その配下は depth+2。つまり「フィールド群を depth+1 で描画し、先頭行の
            // インデント末尾 2 桁を "- " に差し替える」と正しい形になる
            // (INDENT == 2 なので indent(depth+1) == indent(depth) + 2 桁)。
            let start = out.len();
            write_object_fields(fields, depth + 1, out)?;
            let marker = start + depth * INDENT;
            debug_assert!(out[marker..marker + INDENT].bytes().all(|b| b == b' '));
            out.replace_range(marker..marker + INDENT, "- ");
        }
        ToonValue::Array(items) => {
            // list item 位置の配列は keyless header なので tabular form を使えない
            // (§9.4: fields 付き keyless header はルート限定)。空配列も `- []` ではなく
            // `- [0]:` で出す。
            write_indent(depth, out);
            out.push_str("- [");
            out.push_str(&items.len().to_string());
            out.push(']');
            if items.is_empty() {
                out.push_str(":\n");
            } else if items.iter().all(ToonValue::is_primitive) {
                out.push_str(": ");
                write_cells_joined(items, out)?;
                out.push('\n');
            } else {
                out.push_str(":\n");
                for item in items {
                    write_list_item(item, depth + 1, out)?;
                }
            }
        }
        primitive => {
            write_indent(depth, out);
            out.push_str("- ");
            write_primitive(primitive, out)?;
            out.push('\n');
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// keyed tabular (§9.5)
// ---------------------------------------------------------------------------

fn write_keyed_header(
    key: Option<&str>,
    entries: usize,
    columns: &[Field],
    depth: usize,
    out: &mut String,
) {
    write_indent(depth, out);
    if let Some(key) = key {
        scalar::write_key(key, out);
    }
    out.push('[');
    out.push_str(&entries.to_string());
    out.push_str(":]{");
    write_field_list(columns, out);
    out.push_str("}:\n");
}

fn write_keyed_rows(
    entries: &[(String, ToonValue)],
    columns: &[Field],
    depth: usize,
    out: &mut String,
) -> Result<(), ToonError> {
    for (entry_key, entry_value) in entries {
        let fields = entry_value
            .as_non_empty_object()
            .ok_or_else(|| ToonError::Encode("keyed tabular entry is not an object".into()))?;
        write_indent(depth, out);
        scalar::write_key(entry_key, out);
        out.push_str(": ");
        write_row_cells(fields, columns, out)?;
        out.push('\n');
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 列 (shape) 判定
// ---------------------------------------------------------------------------

/// 配列が tabular form の条件 (§9.3) を満たすなら header の field list を返す。
fn tabular_columns(items: &[ToonValue]) -> Option<Vec<Field>> {
    let objects: Vec<&[(String, ToonValue)]> = items
        .iter()
        .map(ToonValue::as_non_empty_object)
        .collect::<Option<_>>()?;
    uniform_columns(&objects)
}

/// object が keyed tabular form の条件 (§9.5) を満たすなら header の field list を返す。
fn keyed_tabular_columns(fields: &[(String, ToonValue)]) -> Option<Vec<Field>> {
    // エントリが 2 件未満の object は keyed 化しない (§9.5)。
    if fields.len() < 2 {
        return None;
    }
    let objects: Vec<&[(String, ToonValue)]> = fields
        .iter()
        .map(|(_, value)| value.as_non_empty_object())
        .collect::<Option<_>>()?;
    uniform_columns(&objects)
}

/// 非空 object の並びが「同じキー集合 + 各列が uniform-primitive か nested-uniform」を
/// 満たすなら、先頭 object の遭遇順を field order とする field list を返す (§9.3)。
///
/// 判定不能・非 uniform は `None` = 保守的に list form / nested form へ倒す。
fn uniform_columns(objects: &[&[(String, ToonValue)]]) -> Option<Vec<Field>> {
    let first = objects.first()?;
    if first.is_empty() {
        return None;
    }
    // 重複キーを持つ object は header の field list と行の対応が壊れるため畳まない。
    for object in objects {
        if object.len() != first.len() || has_duplicate_keys(object) {
            return None;
        }
        // キー集合の一致 (順序は object ごとに違ってよい)。
        for (key, _) in object.iter() {
            if !first.iter().any(|(k, _)| k == key) {
                return None;
            }
        }
    }

    let mut columns = Vec::with_capacity(first.len());
    for (key, _) in first.iter() {
        let column: Vec<&ToonValue> = objects
            .iter()
            .map(|object| lookup(object, key))
            .collect::<Option<_>>()?;

        if column.iter().all(|value| value.is_primitive()) {
            columns.push(Field::Leaf(key.clone()));
            continue;
        }
        // 全要素が非空 object なら nested-uniform を再帰判定する。
        let nested: Vec<&[(String, ToonValue)]> = column
            .iter()
            .map(|value| value.as_non_empty_object())
            .collect::<Option<_>>()?;
        columns.push(Field::Group(key.clone(), uniform_columns(&nested)?));
    }
    Some(columns)
}

fn has_duplicate_keys(object: &[(String, ToonValue)]) -> bool {
    object
        .iter()
        .enumerate()
        .any(|(i, (key, _))| object[..i].iter().any(|(prev, _)| prev == key))
}

fn lookup<'a>(object: &'a [(String, ToonValue)], key: &str) -> Option<&'a ToonValue> {
    object
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, value)| value)
}

// ---------------------------------------------------------------------------
// 低レベル出力
// ---------------------------------------------------------------------------

fn write_field_list(fields: &[Field], out: &mut String) {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.push(DELIMITER);
        }
        match field {
            Field::Leaf(key) => scalar::write_key(key, out),
            Field::Group(key, nested) => {
                scalar::write_key(key, out);
                out.push('{');
                write_field_list(nested, out);
                out.push('}');
            }
        }
    }
}

/// tabular 行 / keyed entry 行のセル列を field list の深さ優先前順で書く (§9.3)。
fn write_row_cells(
    object: &[(String, ToonValue)],
    columns: &[Field],
    out: &mut String,
) -> Result<(), ToonError> {
    write_row_cells_inner(object, columns, &mut true, out)
}

fn write_row_cells_inner(
    object: &[(String, ToonValue)],
    columns: &[Field],
    first: &mut bool,
    out: &mut String,
) -> Result<(), ToonError> {
    for field in columns {
        match field {
            Field::Leaf(key) => {
                let value = lookup(object, key)
                    .ok_or_else(|| ToonError::Encode(format!("missing tabular column `{key}`")))?;
                if !*first {
                    out.push(DELIMITER);
                }
                *first = false;
                write_primitive(value, out)?;
            }
            Field::Group(key, nested_columns) => {
                let nested = lookup(object, key)
                    .and_then(ToonValue::as_non_empty_object)
                    .ok_or_else(|| {
                        ToonError::Encode(format!("nested tabular column `{key}` is not an object"))
                    })?;
                write_row_cells_inner(nested, nested_columns, first, out)?;
            }
        }
    }
    Ok(())
}

fn write_cells_joined(items: &[ToonValue], out: &mut String) -> Result<(), ToonError> {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(DELIMITER);
        }
        write_primitive(item, out)?;
    }
    Ok(())
}

/// プリミティブ 1 個を書く。配列 / object が来るのは呼び出し側の不変条件違反なので、
/// `null` へ落として黙って値を捨てず **エラーにする** (壊れたドキュメントを出すより、
/// 上位のエラー封筒で失敗を報告する方が安全)。
fn write_primitive(value: &ToonValue, out: &mut String) -> Result<(), ToonError> {
    match value {
        ToonValue::Null => out.push_str("null"),
        ToonValue::Bool(true) => out.push_str("true"),
        ToonValue::Bool(false) => out.push_str("false"),
        ToonValue::Int(v) => out.push_str(&v.to_string()),
        ToonValue::UInt(v) => out.push_str(&v.to_string()),
        ToonValue::Float(v) => scalar::write_f64(*v, out),
        ToonValue::Str(s) => scalar::write_string(s, out),
        ToonValue::Array(_) | ToonValue::Object(_) => {
            return Err(ToonError::Encode(
                "expected a primitive value in a cell position".into(),
            ));
        }
    }
    Ok(())
}

fn write_indent(depth: usize, out: &mut String) {
    for _ in 0..depth * INDENT {
        out.push(' ');
    }
}
