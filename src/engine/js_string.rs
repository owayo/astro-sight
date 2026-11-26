//! JavaScript の文字列と識別子の escape を UTF-16 の値へ復元する。

use std::borrow::Cow;

pub(crate) fn decoded(raw: &str) -> Option<Cow<'_, str>> {
    if !raw.contains('\\') {
        return Some(Cow::Borrowed(raw));
    }
    Some(Cow::Owned(String::from_utf16(&units(raw)?).ok()?))
}

pub(crate) fn units(raw: &str) -> Option<Vec<u16>> {
    let mut chars = raw.chars().peekable();
    let mut out = Vec::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buffer = [0; 2];
            out.extend_from_slice(ch.encode_utf16(&mut buffer));
            continue;
        }
        let escaped = chars.next()?;
        let unit = match escaped {
            'n' => 10,
            'r' => 13,
            't' => 9,
            'b' => 8,
            'f' => 12,
            'v' => 11,
            '0' if !chars.peek().is_some_and(char::is_ascii_digit) => 0,
            'x' => hex(&mut chars, 2)? as u16,
            'u' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut value = 0u32;
                    let mut digits = 0;
                    loop {
                        let digit = chars.next()?;
                        if digit == '}' {
                            break;
                        }
                        digits += 1;
                        if digits > 6 {
                            return None;
                        }
                        value = value.checked_mul(16)?.checked_add(digit.to_digit(16)?)?;
                    }
                    if digits == 0 {
                        return None;
                    }
                    let ch = char::from_u32(value)?;
                    let mut buffer = [0; 2];
                    out.extend_from_slice(ch.encode_utf16(&mut buffer));
                    continue;
                }
                hex(&mut chars, 4)? as u16
            }
            '\n' => continue,
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                continue;
            }
            '\u{2028}' | '\u{2029}' => continue,
            '1'..='9' | '0' => return None,
            other => {
                let mut buffer = [0; 2];
                out.extend_from_slice(other.encode_utf16(&mut buffer));
                continue;
            }
        };
        out.push(unit);
    }
    Some(out)
}

fn hex(chars: &mut impl Iterator<Item = char>, count: usize) -> Option<u32> {
    (0..count).try_fold(0u32, |value, _| {
        value
            .checked_mul(16)?
            .checked_add(chars.next()?.to_digit(16)?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_union_decoder_preserves_common_and_unicode_escapes() {
        for (raw, expected) in [
            (r"leg\u0061cy", "legacy"),
            (r"\x6cegacy", "legacy"),
            (r"\u{1f600}", "😀"),
            (r"\ud83d\ude00", "😀"),
            (r"\n\t", "\n\t"),
            (r"\q", "q"),
            ("a\\\r\nb", "ab"),
            (r"\0", "\0"),
            (r#"\'\""#, "'\""),
        ] {
            assert_eq!(
                String::from_utf16(&units(raw).unwrap()).unwrap(),
                expected,
                "{raw}"
            );
        }
        for raw in [r"\01", r"\8", r"\u{}", r"\u{110000}", r"\xq0", "\\"] {
            assert!(units(raw).is_none(), "{raw}");
        }
        assert_eq!(units(r"\ud800"), Some(vec![0xd800]));
    }
}
