//! JavaScript の文字列と識別子の escape を UTF-16 の値へ復元する。

use std::borrow::Cow;

pub(crate) fn decoded(raw: &str) -> Option<Cow<'_, str>> {
    if !raw.contains('\\') {
        return Some(Cow::Borrowed(raw));
    }
    Some(Cow::Owned(String::from_utf16(&units(raw)?).ok()?))
}

pub(crate) fn units(raw: &str) -> Option<Vec<u16>> {
    let mut out = Vec::new();
    visit_units(raw, None, |units| out.extend_from_slice(units))?;
    Some(out)
}

/// JS の単引用符・二重引用符の内側を、行継続も含めて検証し、元の綴りで返す。
pub(crate) fn quoted_contents(literal: &str) -> Option<&str> {
    let quote = literal.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    let inner = literal.strip_prefix(quote)?.strip_suffix(quote)?;
    if inner.contains([quote, '\\', '\r', '\n']) {
        visit_units(inner, Some(quote), |_| {})?;
    }
    Some(inner)
}

/// 不透明な値の中にも通常の文字列は現れる。JSX 属性の直接の引用符は別の字句規則を使う。
pub(crate) fn quoted_strings_are_valid(root: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    use super::tree_walk::{Children, Walk, walk_preorder};
    use std::ops::ControlFlow;

    walk_preorder(root, Children::Named, |node, _| match node.kind() {
        "string" => {
            if node
                .utf8_text(source)
                .ok()
                .and_then(quoted_contents)
                .is_some()
            {
                ControlFlow::Continue(Walk::Skip)
            } else {
                ControlFlow::Break(())
            }
        }
        "jsx_attribute"
            if node
                .named_children(&mut node.walk())
                .any(|c| c.kind() == "string") =>
        {
            ControlFlow::Continue(Walk::Skip)
        }
        _ => ControlFlow::Continue(Walk::Descend),
    })
    .is_continue()
}

fn visit_units(raw: &str, quote: Option<char>, mut emit: impl FnMut(&[u16])) -> Option<()> {
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            if quote.is_some_and(|quote| ch == quote || matches!(ch, '\r' | '\n')) {
                return None;
            }
            let mut buffer = [0; 2];
            emit(ch.encode_utf16(&mut buffer));
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
                    let mut value = chars.next()?.to_digit(16)?;
                    loop {
                        let digit = chars.next()?;
                        if digit == '}' {
                            break;
                        }
                        value = value * 16 + digit.to_digit(16)?;
                        if value > 0x10ffff {
                            return None;
                        }
                    }
                    if let Ok(unit) = u16::try_from(value) {
                        unit
                    } else {
                        let mut buffer = [0; 2];
                        emit(char::from_u32(value)?.encode_utf16(&mut buffer));
                        continue;
                    }
                } else {
                    hex(&mut chars, 4)? as u16
                }
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
                emit(other.encode_utf16(&mut buffer));
                continue;
            }
        };
        emit(&[unit]);
    }
    Some(())
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
    fn quoted_key_contents_distinguish_line_continuations_from_literal_line_breaks() {
        for inner in [
            "",
            "a\\\nb",
            "a\\\rb",
            "a\\\r\nb",
            "a\\\\\\\nb",
            "a\u{2028}b",
            "a\u{2029}b",
            "a\\\u{2028}b",
            "a\\\u{2029}b",
            r"\n",
            r"\0",
            r"\u{00000000000061}",
            r"\u{d800}",
            r"\u{10ffff}",
        ] {
            for quote in ['\'', '"'] {
                assert_eq!(
                    quoted_contents(&format!("{quote}{inner}{quote}")),
                    Some(inner),
                    "{inner:?}"
                );
            }
        }
        for inner in [
            "a\nb",
            "a\rb",
            "a\r\nb",
            "a\\\\\nb",
            "a\\?\nb",
            r"\1",
            r"\8",
            r"\08",
            r"\xG0",
            r"\u{}",
            r"\u{110000}",
            "\\",
        ] {
            assert!(
                quoted_contents(&format!("'{inner}'")).is_none(),
                "{inner:?}"
            );
        }
        assert_eq!(units(r"\u{0000d800}"), Some(vec![0xd800]));
    }
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
