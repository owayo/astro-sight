use anyhow::Result;
use std::io::Read;

use crate::cache::store::CacheStore;
use crate::error::{AstroError, ErrorCode};

pub const MAX_INPUT_SIZE: usize = 100 * 1024 * 1024;

/// 現在プロセスの RSS を KB 単位で取得 (Linux のみ正確、その他 OS は None)。
/// `astro-sight review` の各フェーズが何 GB 消費しているかを CI の artifacts ログで
/// 観測するため。
pub(crate) fn current_rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::fs;
        let status = fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if let Some(kb) = parts.first().and_then(|s| s.parse::<u64>().ok()) {
                    return Some(kb);
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `ASTRO_SIGHT_LOG_PHASES=1` のときのみ stderr に進捗ログを出す。
///
/// CI で `astro-sight review` がどのフェーズで何 GB を確保するかを観測するための
/// 軽量プロファイラ。出力フォーマットは:
/// `[as] phase=<NAME> status=<start|end> rss=<MB> elapsed=<MS>`
pub(crate) fn log_phase(phase: &str, status: &str, elapsed_ms: u128) {
    if std::env::var("ASTRO_SIGHT_LOG_PHASES").ok().as_deref() != Some("1") {
        return;
    }
    let rss_str = current_rss_kb()
        .map(|kb| format!("{}MB", kb / 1024))
        .unwrap_or_else(|| "?MB".to_string());
    eprintln!("[as] phase={phase} status={status} rss={rss_str} elapsed={elapsed_ms}ms");
}

pub fn classify_error(e: &anyhow::Error) -> (String, String) {
    if let Some(ae) = e.downcast_ref::<AstroError>() {
        (ae.code.to_string(), ae.message.clone())
    } else {
        ("IO_ERROR".to_string(), e.to_string())
    }
}

pub fn serialize_output(value: &impl serde::Serialize, pretty: bool) -> Result<String> {
    if pretty {
        Ok(serde_json::to_string_pretty(value)?)
    } else {
        Ok(serde_json::to_string(value)?)
    }
}

pub(crate) fn make_error_line(e: &anyhow::Error) -> String {
    let (code, message) = classify_error(e);
    let obj = serde_json::json!({ "error": { "code": code, "message": message } });
    serde_json::to_string(&obj).unwrap()
}

pub(crate) fn read_bytes_limited<R: std::io::Read>(
    reader: R,
    max_bytes: usize,
    source_name: &str,
) -> Result<Vec<u8>> {
    let mut limited = reader.take((max_bytes + 1) as u64);
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf)?;

    if buf.len() > max_bytes {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "{source_name} exceeds maximum size ({} bytes > {} bytes)",
                buf.len(),
                max_bytes
            ),
        )
        .into());
    }

    Ok(buf)
}

pub(crate) fn read_bytes_limited_and_drain<R: std::io::Read>(
    mut reader: R,
    max_bytes: usize,
    source_name: &str,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut total_bytes = 0usize;
    let mut chunk = [0u8; 8192];

    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }

        total_bytes = total_bytes.saturating_add(read);
        if buf.len() <= max_bytes {
            let remaining = max_bytes.saturating_add(1).saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..read.min(remaining)]);
        }
    }

    if total_bytes > max_bytes {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "{source_name} exceeds maximum size ({} bytes > {} bytes)",
                total_bytes, max_bytes
            ),
        )
        .into());
    }

    Ok(buf)
}

pub(crate) fn read_to_string_limited<R: std::io::Read>(
    reader: R,
    max_bytes: usize,
    source_name: &str,
) -> Result<String> {
    let buf = read_bytes_limited(reader, max_bytes, source_name)?;
    String::from_utf8(buf).map_err(|e| {
        AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{source_name} is not valid UTF-8: {e}"),
        )
        .into()
    })
}

pub(crate) fn read_file_to_string_limited(path: &str, max_bytes: usize) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > max_bytes as u64 {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "{path} exceeds maximum size ({} bytes > {} bytes)",
                metadata.len(),
                max_bytes
            ),
        )
        .into());
    }
    read_to_string_limited(file, max_bytes, path)
}

pub fn read_paths_file_limited(path: &str, max_bytes: usize) -> Result<Vec<String>> {
    let content = match read_file_to_string_limited(path, max_bytes) {
        Ok(content) => content,
        Err(e) if e.downcast_ref::<AstroError>().is_some() => return Err(e),
        Err(e) => {
            return Err(AstroError::new(
                ErrorCode::IoError,
                format!("failed to read paths file {path}: {e}"),
            )
            .into());
        }
    };

    Ok(content
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

pub(crate) fn cache_hash_for_path(path: &camino::Utf8Path, source: &[u8]) -> String {
    let content_hash = CacheStore::hash(source);
    let path_key = std::fs::canonicalize(path.as_std_path())
        .ok()
        .and_then(|p| p.to_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| path.as_str().to_string());

    // 応答には path/lang が含まれるため、内容が同じ別ファイルとはキャッシュを分離する。
    CacheStore::hash(format!("{path_key}\0{content_hash}").as_bytes())
}
