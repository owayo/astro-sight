use anyhow::{Context, Result};
use camino::Utf8Path;
use tree_sitter::{Parser, Tree};

use crate::error::AstroError;
use crate::language::LangId;

/// Parse a source file and return the tree-sitter Tree.
pub fn parse_file(path: &Utf8Path, source: &[u8]) -> Result<(Tree, LangId)> {
    let lang_id = LangId::from_path(path).or_else(|_| {
        // Try shebang detection
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

/// Parse source bytes with a known language.
pub fn parse_source(source: &[u8], lang_id: LangId) -> Result<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang_id.ts_language())
        .context("Failed to set parser language")?;

    parser
        .parse(source, None)
        .ok_or_else(|| AstroError::parse_error("<source>").into())
}

/// Maximum file size: 100 MB.
const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// Read a file using mmap for efficiency.
pub fn read_file(path: &Utf8Path) -> Result<Vec<u8>> {
    use std::fs::File;
    let file =
        File::open(path.as_std_path()).map_err(|_| AstroError::file_not_found(path.as_str()))?;
    let metadata = file.metadata()?;

    if metadata.len() == 0 {
        return Ok(Vec::new());
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

    // Use mmap for files > 64KB
    if metadata.len() > 65536 {
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(mmap.to_vec())
    } else {
        use std::io::Read;
        let mut buf = Vec::with_capacity(metadata.len() as usize);
        let mut reader = std::io::BufReader::new(file);
        reader.read_to_end(&mut buf)?;
        Ok(buf)
    }
}
