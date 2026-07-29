use std::io::Read;
use std::path::Path;

/// UTF-8 テキストファイルを指定上限まで読み込む。
///
/// metadata 確認後にファイルが拡大する TOCTOU も、上限 + 1 byte だけ読むことで検出する。
pub(crate) fn read_utf8_file_limited(path: &Path, max_bytes: u64) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    read_utf8_limited(file, max_bytes)
}

fn read_utf8_limited(reader: impl Read, max_bytes: u64) -> Option<String> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > max_bytes {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::read_utf8_limited;

    #[test]
    fn accepts_exact_limit() {
        let content = std::io::Cursor::new(b"test".to_vec());
        assert_eq!(read_utf8_limited(content, 4), Some("test".to_string()));
    }

    #[test]
    fn rejects_one_byte_over_limit() {
        let content = std::io::Cursor::new(b"tests".to_vec());
        assert!(read_utf8_limited(content, 4).is_none());
    }

    #[test]
    fn rejects_invalid_utf8() {
        let content = std::io::Cursor::new(vec![0xff]);
        assert!(read_utf8_limited(content, 1).is_none());
    }
}
