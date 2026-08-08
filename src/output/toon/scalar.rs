//! TOON のプリミティブ表記 — 文字列の quoting / escape (§7.1, §7.2)、キー表記 (§7.3)、
//! 数値の canonical 10 進形 (§2)。
//!
//! delimiter は v1 では comma 固定 (§11 の document delimiter)。仕様上 tab / pipe も
//! 選べるが、切り替え手段を持たない以上「active delimiter = document delimiter = `,`」で
//! 閉じており、quoting 判定も `,` だけを見ればよい。

/// TOON の active / document delimiter。v1 は comma 固定。
pub const DELIMITER: char = ',';

/// §7.2 の quoting 条件。1 つでも該当すれば quote が必須。
pub fn needs_quoting(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    // 先頭・末尾の空白 (U+0020 / U+0009)。
    let first = s.as_bytes()[0];
    let last = s.as_bytes()[s.len() - 1];
    if matches!(first, b' ' | b'\t') || matches!(last, b' ' | b'\t') {
        return true;
    }
    // 予約語と数値風。quote しないと decoder が bool / null / number に化ける。
    if matches!(s, "true" | "false" | "null") {
        return true;
    }
    if is_numeric_like(s) {
        return true;
    }
    // 先頭の `-` は list marker、先頭の `#` は comment 行と衝突する (§5.1)。
    if first == b'-' || first == b'#' {
        return true;
    }
    s.chars().any(|c| {
        c == ':'
            || c == '"'
            || c == '\\'
            || c == '['
            || c == ']'
            || c == '{'
            || c == '}'
            || c == DELIMITER
            || (c as u32) <= 0x1F
    })
}

/// §7.2 の numeric-like 判定: `/^[+-]?[0-9]+(?:\.[0-9]+)?(?:e[+-]?[0-9]+)?$/i` (ASCII 数字のみ)。
fn is_numeric_like(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;

    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    // 整数部は 1 桁以上必須。
    let int_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == int_start {
        return false;
    }
    // 小数部があれば 1 桁以上必須。
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == frac_start {
            return false;
        }
    }
    // 指数部があれば符号は任意、桁は 1 桁以上必須。
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == bytes.len()
}

/// §7.1 の escape を適用して `"..."` で囲む。
pub fn quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) <= 0x1F => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// 文字列値を必要に応じて quote して書き出す。
pub fn write_string(s: &str, out: &mut String) {
    if needs_quoting(s) {
        quote(s, out);
    } else {
        out.push_str(s);
    }
}

/// §7.3: キー / フィールド名は `^[A-Za-z_][A-Za-z0-9_.]*$` のときだけ unquoted で出せる。
pub fn write_key(key: &str, out: &mut String) {
    if is_valid_unquoted_key(key) {
        out.push_str(key);
    } else {
        quote(key, out);
    }
}

fn is_valid_unquoted_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// canonical range の下限 / 上限 (§2)。`n == 0` または `1e-6 <= |n| < 1e21` は
/// 指数表記を使わない 10 進形で出す。
const CANONICAL_MIN: f64 = 1e-6;
const CANONICAL_MAX: f64 = 1e21;

/// §2 / §3 の数値正規化。NaN / ±Infinity は `null` へ落とす (§3)。
pub fn write_f64(v: f64, out: &mut String) {
    if !v.is_finite() {
        out.push_str("null");
        return;
    }
    if v == 0.0 {
        // -0 は 0 へ正規化する (§2)。
        out.push('0');
        return;
    }

    let magnitude = v.abs();
    if (CANONICAL_MIN..CANONICAL_MAX).contains(&magnitude) {
        // Rust の `Display for f64` は最短往復かつ指数表記を使わず、整数値は "1" と
        // 出す (末尾 0 も付かない) ため canonical 形の要件をそのまま満たす。
        out.push_str(&v.to_string());
        return;
    }

    // canonical range 外は指数表記が許される (§2)。決定性のため小文字 `e` と
    // 明示的な符号を付ける (Rust の `{:e}` は正指数で `+` を省くので補う)。
    let exp = format!("{v:e}");
    match exp.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => {
            out.push_str(mantissa);
            out.push_str("e+");
            out.push_str(exponent);
        }
        _ => out.push_str(&exp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(f: impl Fn(&mut String)) -> String {
        let mut out = String::new();
        f(&mut out);
        out
    }

    #[test]
    fn numeric_like_matches_spec_regex() {
        for ok in [
            "42", "-3.14", "05", "+1", "1e-6", "1E+9", "0", "-0", "1.0", "0.5",
        ] {
            assert!(is_numeric_like(ok), "{ok} should be numeric-like");
        }
        for ng in [
            "", ".5", "1.", "+", "-", "1e", "1e+", "0x10", "1_000", "Infinity", "NaN", "1.2.3",
            "1 ", "e5",
        ] {
            assert!(!is_numeric_like(ng), "{ng} should not be numeric-like");
        }
    }

    #[test]
    fn quoting_covers_every_spec_condition() {
        // §7.2 の各条件が 1 つずつ効いていること。
        for q in [
            "",       // 空文字
            " lead",  // 先頭空白
            "trail ", // 末尾空白
            "\ttab",  // 先頭 HTAB
            "true",   // 予約語
            "false",
            "null",
            "42", // 数値風
            "-3.14",
            "a:b",  // colon
            "a\"b", // double quote
            "a\\b", // backslash
            "a[b",  // bracket
            "a]b",
            "a{b", // brace
            "a}b",
            "a,b",       // active delimiter
            "-dash",     // 先頭ハイフン (list marker と衝突)
            "#hash",     // 先頭 # (comment と衝突)
            "ctrl\u{1}", // 制御文字
            "line\nbreak",
            "a\tb", // HTAB (U+0009) も U+0000-U+001F の制御文字なので quote が要る
        ] {
            assert!(needs_quoting(q), "{q:?} should need quoting");
        }
        for plain in [
            "hello",
            "hello world",
            "src/main.rs",
            "日本語",
            "🎉",
            "a-b",  // 先頭以外のハイフンは安全
            "a#b",  // 先頭以外の # は安全
            "v1.2", // 数値風ではない
            "a|b",  // pipe は非 active delimiter なので安全
        ] {
            assert!(!needs_quoting(plain), "{plain:?} should not need quoting");
        }
    }

    #[test]
    fn escape_table_follows_spec() {
        assert_eq!(s(|o| quote("a\\b", o)), r#""a\\b""#);
        assert_eq!(s(|o| quote("a\"b", o)), r#""a\"b""#);
        assert_eq!(s(|o| quote("a\nb", o)), r#""a\nb""#);
        assert_eq!(s(|o| quote("a\rb", o)), r#""a\rb""#);
        assert_eq!(s(|o| quote("a\tb", o)), r#""a\tb""#);
        assert_eq!(s(|o| quote("a\u{1}b", o)), r#""a\u0001b""#);
        // 補助面 (U+10000 以上) はリテラル UTF-8 のまま。
        assert_eq!(s(|o| quote("a🎉b", o)), "\"a🎉b\"");
    }

    #[test]
    fn keys_are_quoted_only_when_required() {
        assert_eq!(s(|o| write_key("name", o)), "name");
        assert_eq!(s(|o| write_key("_a1", o)), "_a1");
        assert_eq!(s(|o| write_key("data.meta", o)), "data.meta");
        assert_eq!(s(|o| write_key("my-key", o)), r#""my-key""#);
        assert_eq!(s(|o| write_key("1abc", o)), r#""1abc""#);
        assert_eq!(s(|o| write_key("", o)), r#""""#);
    }

    #[test]
    fn numbers_use_canonical_decimal_form() {
        assert_eq!(s(|o| write_f64(0.0, o)), "0");
        assert_eq!(s(|o| write_f64(-0.0, o)), "0");
        assert_eq!(s(|o| write_f64(1.0, o)), "1");
        assert_eq!(s(|o| write_f64(1.5000, o)), "1.5");
        assert_eq!(s(|o| write_f64(1e6, o)), "1000000");
        assert_eq!(s(|o| write_f64(1e-6, o)), "0.000001");
        assert_eq!(s(|o| write_f64(-2.75, o)), "-2.75");
        // canonical range 外は指数表記。符号は明示する。
        assert_eq!(s(|o| write_f64(1e-7, o)), "1e-7");
        assert_eq!(s(|o| write_f64(1e21, o)), "1e+21");
        // 非有限は null (§3)。
        assert_eq!(s(|o| write_f64(f64::NAN, o)), "null");
        assert_eq!(s(|o| write_f64(f64::INFINITY, o)), "null");
        assert_eq!(s(|o| write_f64(f64::NEG_INFINITY, o)), "null");
    }

    #[test]
    fn exponent_form_round_trips() {
        // 指数表記で出した値が f64 として往復すること (§2 の precision 要件)。
        for v in [1e-7, 2.5e-9, 1e21, 1.7976931348623157e308, 5e-324] {
            let text = s(|o| write_f64(v, o));
            assert_eq!(text.parse::<f64>().unwrap(), v, "round trip failed for {v}");
        }
    }

    #[test]
    fn canonical_numbers_are_not_numeric_like_traps() {
        // encoder が出す数値表記は decoder の number grammar (§4) に合致する。
        for v in [0.0, 1.0, 1.5, 1e6, 1e-6, -2.75, 1e-7, 1e21] {
            let text = s(|o| write_f64(v, o));
            assert!(
                is_numeric_like(&text),
                "{text} must parse back as a number token"
            );
        }
    }
}
