//! tsconfig / jsconfig などの JSONC を、JSON として読める形に整える。

/// コメントと末尾の comma だけを空白へ置き換え、文字列と escape は JSON の解釈に任せる。
/// 閉じていないブロックコメントは None。
pub(crate) fn json_bytes(source: &str) -> Option<Vec<u8>> {
    let mut bytes = source.as_bytes().to_vec();
    let mut index = 0;
    let mut quoted = false;
    while index < bytes.len() {
        if quoted {
            if bytes[index] == b'\\' {
                index += 2;
                continue;
            }
            if bytes[index] == b'"' {
                quoted = false;
            }
            index += 1;
            continue;
        }
        if bytes[index] == b'"' {
            quoted = true;
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                bytes[index] = b' ';
                index += 1;
            }
        } else if bytes[index..].starts_with(b"/*") {
            bytes[index] = b' ';
            bytes[index + 1] = b' ';
            index += 2;
            loop {
                if index + 1 >= bytes.len() {
                    return None;
                }
                if bytes[index..].starts_with(b"*/") {
                    bytes[index] = b' ';
                    bytes[index + 1] = b' ';
                    index += 2;
                    break;
                }
                bytes[index] = b' ';
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    let mut quoted = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if quoted => {
                index += 2;
                continue;
            }
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                let next = bytes[index + 1..]
                    .iter()
                    .copied()
                    .find(|byte| !byte.is_ascii_whitespace());
                if matches!(next, Some(b'}' | b']')) {
                    bytes[index] = b' ';
                }
            }
            _ => {}
        }
        index += 1;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_comments_and_trailing_commas_become_whitespace() {
        let source = "{ // line\n \"a\": \"x // y\", /* block */ \"b\": [1, 2,], }";
        let value: serde_json::Value =
            serde_json::from_slice(&json_bytes(source).unwrap()).unwrap();
        assert_eq!(value["a"], "x // y");
        assert_eq!(value["b"], serde_json::json!([1, 2]));
        assert!(json_bytes("{ /* unterminated").is_none());
    }
}
