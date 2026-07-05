use anyhow::{Context, Result};
use camino::Utf8Path;
use tree_sitter::{ParseOptions, Parser, Tree};

use crate::error::AstroError;
use crate::language::LangId;

/// ソースファイルをパースし tree-sitter Tree を返す。
pub fn parse_file(path: &Utf8Path, source: &[u8]) -> Result<(Tree, LangId)> {
    let lang_id = detect_lang(path, source)?;

    let tree = parse_source(source, lang_id)?;
    Ok((tree, lang_id))
}

/// パスとソース内容から解析言語を決定する。
///
/// `.h` は既定では C として扱うが、C++ 専用構文を含み、C++ parser の方が明確に
/// parse error が少ない場合だけ C++ に切り替える。C ヘッダを誤って C++ 扱いしない
/// ため、拡張子だけではなく parser の実結果で判定する。
pub fn detect_lang(path: &Utf8Path, source: &[u8]) -> Result<LangId> {
    let lang_id = LangId::from_path(path).or_else(|_| {
        // shebang で言語検出を試行
        let first_line = std::str::from_utf8(source)
            .ok()
            .and_then(|s| s.lines().next())
            .unwrap_or("");
        LangId::from_shebang(first_line)
            .ok_or_else(|| AstroError::unsupported_language(path.extension().unwrap_or("<none>")))
    })?;

    if lang_id == LangId::C && is_c_header_path(path) && header_should_use_cpp(source) {
        Ok(LangId::Cpp)
    } else {
        Ok(lang_id)
    }
}

fn is_c_header_path(path: &Utf8Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("h"))
}

fn header_should_use_cpp(source: &[u8]) -> bool {
    if !header_has_cpp_marker(source) {
        return false;
    }
    let Ok(c_tree) = parse_source(source, LangId::C) else {
        return false;
    };
    let c_score = parse_error_score(c_tree.root_node());
    if c_score == 0 {
        return false;
    }
    let Ok(cpp_tree) = parse_source(source, LangId::Cpp) else {
        return false;
    };
    parse_error_score(cpp_tree.root_node()) < c_score
}

fn header_has_cpp_marker(source: &[u8]) -> bool {
    const MARKERS: &[&[u8]] = &[
        b"class ",
        b"namespace ",
        b"template",
        b"typename ",
        b"public:",
        b"private:",
        b"protected:",
        b"::",
        b"virtual ",
        b"constexpr",
    ];
    MARKERS
        .iter()
        .any(|marker| memchr::memmem::find(source, marker).is_some())
}

fn parse_error_score(node: tree_sitter::Node<'_>) -> usize {
    let mut score = usize::from(node.is_error() || node.is_missing());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        score += parse_error_score(child);
    }
    score
}

/// パースのタイムアウト秒。デフォルト 30 秒、`ASTRO_SIGHT_PARSE_TIMEOUT_SEC` で上書き、
/// 0 で無制限。
///
/// tree-sitter は ambiguous な grammar や複雑な構文で GLR バックトラッキングが
/// 指数的に爆発し、数十 KB のファイル単位で数 GB のメモリを食うことがある。
/// `ParseOptions::progress_callback` で経過時間を監視し、上限超過で parse を
/// 打ち切ることで OOM を防ぐ。
fn parse_timeout_secs() -> u64 {
    std::env::var("ASTRO_SIGHT_PARSE_TIMEOUT_SEC")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30)
}

/// 既知の言語でソースバイト列をパースする。
///
/// `LexerOnly` 言語 (現状 Xojo) は tree-sitter を持たないため `UnsupportedLanguage`
/// エラーを返す。呼び出し側は事前に `lang_id.is_lexer_only()` で振り分けるべき。
pub fn parse_source(source: &[u8], lang_id: LangId) -> Result<Tree> {
    use std::time::{Duration, Instant};

    if lang_id.is_lexer_only() {
        return Err(AstroError::unsupported_language(&format!(
            "{lang_id} is lexer-only; use lexer module instead of tree-sitter parser"
        ))
        .into());
    }

    let mut parser = Parser::new();
    parser
        .set_language(&lang_id.ts_language())
        .context("Failed to set parser language")?;

    let timeout_secs = parse_timeout_secs();
    let tree = if timeout_secs > 0 {
        use std::ops::ControlFlow;
        let start = Instant::now();
        let limit = Duration::from_secs(timeout_secs);
        let mut callback = |_state: &tree_sitter::ParseState| {
            if start.elapsed() > limit {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = ParseOptions::new().progress_callback(&mut callback);
        parser.parse_with_options(
            &mut |byte, _| &source[byte.min(source.len())..],
            None,
            Some(options),
        )
    } else {
        parser.parse(source, None)
    };

    tree.ok_or_else(|| AstroError::parse_error("<source>").into())
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

    // 64KB 超のファイルは mmap でゼロコピー、それ以下は Vec に読み込む。
    // 大半の source ファイル（数百 KB 級）は mmap 経路になり、drop 時に OS が即 unmap するため
    // macOS の libmalloc fragmentation を経由せず RSS が線形に解放される。
    // 小さいファイルだけ Vec で読むことで、mmap syscall オーバーヘッドも最小化する。
    // Safety: mmap 分岐は他プロセスから truncate されると SIGBUS の可能性がある。
    // memmap2 の既知の制限事項であり、CLI ツールとしては許容範囲。
    if metadata.len() > 65536 {
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        // metadata 取得後にファイルが拡大した場合の TOCTOU 防止として、mmap 後の
        // 実サイズも上限と照合する。
        if mmap.len() as u64 > MAX_FILE_SIZE {
            anyhow::bail!(AstroError::new(
                crate::error::ErrorCode::InvalidRequest,
                format!(
                    "File too large after mmap ({} bytes > {} bytes): {}",
                    mmap.len(),
                    MAX_FILE_SIZE,
                    path
                ),
            ));
        }
        Ok(SourceBuf::Mmap(mmap))
    } else {
        use std::io::Read;
        let mut buf = Vec::with_capacity(metadata.len() as usize);
        // metadata 取得後にファイルが拡大しても物理的に上限を超えないよう、
        // read_to_end の前に take() で上限+1 まで読み込みを制限する。
        let mut reader = std::io::BufReader::new(file).take(MAX_FILE_SIZE + 1);
        reader.read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_FILE_SIZE {
            anyhow::bail!(AstroError::new(
                crate::error::ErrorCode::InvalidRequest,
                format!(
                    "File too large during read ({}+ bytes > {} bytes): {}",
                    buf.len(),
                    MAX_FILE_SIZE,
                    path
                ),
            ));
        }
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

    /// `.h` でも C++ 専用構文が C より正しく parse できる場合は C++ と判定する。
    #[test]
    fn detect_lang_cpp_header_when_cpp_parse_is_better() {
        let source =
            b"template <typename T> struct Base {};\nstruct OmnisError : public Base<OmnisError> {};\n";
        let lang = detect_lang(camino::Utf8Path::new("error.h"), source).unwrap();
        assert_eq!(lang, LangId::Cpp);
    }

    /// C として問題なく parse できる `.h` は C のまま扱う。
    #[test]
    fn detect_lang_keeps_plain_c_header_as_c() {
        let source = b"struct plain_c_header { int value; };\n";
        let lang = detect_lang(camino::Utf8Path::new("plain.h"), source).unwrap();
        assert_eq!(lang, LangId::C);
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
