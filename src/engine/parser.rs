use anyhow::{Context, Result};
use camino::Utf8Path;
use tree_sitter::{Parser, Tree};

use crate::error::AstroError;
use crate::language::LangId;

/// ソースファイルをパースし tree-sitter Tree を返す。
pub fn parse_file(path: &Utf8Path, source: &[u8]) -> Result<(Tree, LangId)> {
    let lang_id = LangId::from_path(path).or_else(|_| {
        // shebang で言語検出を試行
        let first_line = std::str::from_utf8(source)
            .ok()
            .and_then(|s| s.lines().next())
            .unwrap_or("");
        LangId::from_shebang(first_line)
            .ok_or_else(|| AstroError::unsupported_language(path.extension().unwrap_or("<none>")))
    })?;

    let tree = parse_source(source, lang_id)?;
    Ok((tree, lang_id))
}

/// 既知の言語でソースバイト列をパースする。
pub fn parse_source(source: &[u8], lang_id: LangId) -> Result<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang_id.ts_language())
        .context("Failed to set parser language")?;

    parser
        .parse(source, None)
        .ok_or_else(|| AstroError::parse_error("<source>").into())
}

/// ファイルサイズ上限: 100 MB。
const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// ゼロコピー対応ソースバッファ。
/// 64KB 超のファイルは mmap（コピー不要）、それ以下は Vec<u8> を使用。
pub enum SourceBuf {
    Mmap(memmap2::Mmap),
    Vec(Vec<u8>),
}

impl SourceBuf {
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            SourceBuf::Mmap(m) => m,
            SourceBuf::Vec(v) => v,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
    }
}

impl std::ops::Deref for SourceBuf {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for SourceBuf {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// ファイルをゼロコピーバッファに読み込む（大ファイルは mmap、小ファイルは Vec）。
pub fn read_file(path: &Utf8Path) -> Result<SourceBuf> {
    use std::fs::File;
    let file =
        File::open(path.as_std_path()).map_err(|_| AstroError::file_not_found(path.as_str()))?;
    let metadata = file.metadata()?;

    if metadata.len() == 0 {
        return Ok(SourceBuf::Vec(Vec::new()));
    }

    if metadata.len() > MAX_FILE_SIZE {
        anyhow::bail!(AstroError::new(
            crate::error::ErrorCode::InvalidRequest,
            format!(
                "File too large ({} bytes > {} bytes): {}",
                metadata.len(),
                MAX_FILE_SIZE,
                path
            ),
        ));
    }

    // 64KB 超のファイルは mmap でゼロコピー
    // Safety: ファイルが他プロセスから truncate されると SIGBUS の可能性がある。
    // memmap2 の既知の制限事項であり、CLI ツールとしては許容範囲。
    if metadata.len() > 65536 {
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(SourceBuf::Mmap(mmap))
    } else {
        use std::io::Read;
        let mut buf = Vec::with_capacity(metadata.len() as usize);
        let mut reader = std::io::BufReader::new(file);
        reader.read_to_end(&mut buf)?;
        Ok(SourceBuf::Vec(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::LangId;

    /// 有効な Rust ソースを正常にパースできる
    #[test]
    fn parse_valid_rust() {
        let source = b"fn main() { println!(\"hello\"); }";
        let tree = parse_source(source, LangId::Rust).unwrap();
        assert!(!tree.root_node().has_error());
    }

    /// 有効な Python ソースを正常にパースできる
    #[test]
    fn parse_valid_python() {
        let source = b"def hello():\n    print('hello')\n";
        let tree = parse_source(source, LangId::Python).unwrap();
        assert!(!tree.root_node().has_error());
    }

    /// SourceBuf::Vec の Deref が正しくバイト列を返す
    #[test]
    fn source_buf_vec_deref() {
        let buf = SourceBuf::Vec(b"hello".to_vec());
        assert_eq!(buf.len(), 5);
        assert!(!buf.is_empty());
        assert_eq!(&*buf, b"hello");
    }

    /// SourceBuf の len/is_empty が空バッファで正しく動作する
    #[test]
    fn source_buf_mmap_len() {
        let buf = SourceBuf::Vec(Vec::new());
        assert_eq!(buf.len(), 0);
        assert!(buf.is_empty());
    }

    /// 存在しないファイルでエラーを返す
    #[test]
    fn read_file_nonexistent() {
        let result = read_file(camino::Utf8Path::new("/nonexistent/file.rs"));
        assert!(result.is_err());
    }

    /// 空ファイルで SourceBuf::Vec(empty) を返す
    #[test]
    fn read_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.rs");
        std::fs::write(&path, b"").unwrap();
        let utf8_path = camino::Utf8Path::from_path(&path).unwrap();
        let buf = read_file(utf8_path).unwrap();
        assert!(buf.is_empty());
        // Vec バリアントであることを確認
        assert!(matches!(buf, SourceBuf::Vec(v) if v.is_empty()));
    }
}
