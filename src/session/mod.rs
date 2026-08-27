use anyhow::Result;
use std::io::{self, BufRead, Write};
use tracing::info;

use crate::models::request::AstgenRequest;

/// Session 入力 1 行あたりの最大サイズ: 100 MB。
const MAX_LINE_SIZE: usize = 100 * 1024 * 1024;

fn make_error(code: &str, message: String) -> serde_json::Value {
    serde_json::json!({
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn process_line<F>(line: &str, max_line_size: usize, handler: &F) -> Option<serde_json::Value>
where
    F: Fn(AstgenRequest) -> Result<serde_json::Value>,
{
    if line.len() > max_line_size {
        return Some(make_error(
            "INVALID_REQUEST",
            format!(
                "Input line exceeds maximum size ({} bytes > {} bytes)",
                line.len(),
                max_line_size
            ),
        ));
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let result = match serde_json::from_str::<AstgenRequest>(trimmed) {
        Ok(req) => {
            info!(
                command = ?req.command,
                path = %req.path,
                "session request"
            );
            let res = handler(req);
            match &res {
                Ok(value) => {
                    let output_bytes = serde_json::to_string(value).map(|s| s.len()).unwrap_or(0);
                    info!(output_bytes = output_bytes, "session response");
                }
                Err(e) => {
                    info!(error = %e, "session response error");
                }
            }
            res
        }
        Err(e) => {
            return Some(make_error(
                "INVALID_REQUEST",
                format!("Invalid JSON request: {e}"),
            ));
        }
    };

    Some(match result {
        Ok(value) => value,
        Err(e) => {
            // CLI / batch と同じ機械可読コードを返す。すべてを "IO_ERROR" に潰すと
            // サンドボックス拒否 (PATH_OUT_OF_BOUNDS) まで I/O エラーとして報告される。
            // また `format!("{e}")` は AstroError の Display が `[CODE] message` を出すため、
            // message 側にもコードが二重に入っていた (他経路は ae.message を使うので付かない)。
            let (code, message) = crate::commands::classify_error(&e);
            make_error(&code, message)
        }
    })
}

enum ReadLine {
    Eof,
    Line(String),
    Oversized(usize),
}

fn read_line_limited<R: BufRead>(
    reader: &mut R,
    max_line_size: usize,
    scratch: &mut Vec<u8>,
) -> io::Result<ReadLine> {
    scratch.clear();
    let mut total_bytes = 0usize;

    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            if total_bytes == 0 {
                return Ok(ReadLine::Eof);
            }
            break;
        }

        if let Some(newline_pos) = chunk.iter().position(|&b| b == b'\n') {
            let line_chunk = &chunk[..newline_pos];
            total_bytes += line_chunk.len();
            if scratch.len() <= max_line_size {
                let remaining = max_line_size
                    .saturating_add(1)
                    .saturating_sub(scratch.len());
                scratch.extend_from_slice(&line_chunk[..line_chunk.len().min(remaining)]);
            }
            reader.consume(newline_pos + 1);
            break;
        }

        total_bytes += chunk.len();
        if scratch.len() <= max_line_size {
            let remaining = max_line_size
                .saturating_add(1)
                .saturating_sub(scratch.len());
            scratch.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        }
        let consumed = chunk.len();
        reader.consume(consumed);
    }

    if total_bytes > max_line_size {
        return Ok(ReadLine::Oversized(total_bytes));
    }

    if scratch.last() == Some(&b'\r') {
        scratch.pop();
    }

    let line = std::str::from_utf8(scratch)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        .to_owned();
    Ok(ReadLine::Line(line))
}

/// NDJSON セッションを実行し、stdin の要求を stdout に逐次返す。
pub fn run_session(handler: impl Fn(AstgenRequest) -> Result<serde_json::Value>) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut out = io::BufWriter::new(stdout.lock());
    let mut scratch = Vec::new();

    loop {
        let next = read_line_limited(&mut input, MAX_LINE_SIZE, &mut scratch)?;
        let value = match next {
            ReadLine::Eof => break,
            ReadLine::Line(line) => process_line(&line, MAX_LINE_SIZE, &handler),
            ReadLine::Oversized(actual) => Some(make_error(
                "INVALID_REQUEST",
                format!(
                    "Input line exceeds maximum size ({} bytes > {} bytes)",
                    actual, MAX_LINE_SIZE
                ),
            )),
        };

        if let Some(value) = value {
            serde_json::to_writer(&mut out, &value)?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_handler(_req: AstgenRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({ "ok": true }))
    }

    #[test]
    fn process_line_skips_blank_input() {
        let result = process_line("   \t", 10, &ok_handler);
        assert!(result.is_none());
    }

    #[test]
    fn process_line_rejects_oversized_raw_line_even_if_trimmed_is_short() {
        let line = format!("{}{}", " ".repeat(11), "{}");
        let result = process_line(&line, 10, &ok_handler).expect("should produce an error JSON");

        assert_eq!(result["error"]["code"], "INVALID_REQUEST");
        assert!(
            result["error"]["message"]
                .as_str()
                .expect("message should be string")
                .contains("exceeds maximum size")
        );
    }

    #[test]
    fn process_line_passes_valid_json_to_handler() {
        let line = r#"{"command":"doctor","path":"."}"#;
        let result = process_line(line, 1024, &ok_handler).expect("should produce JSON");
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn process_line_maps_handler_error_to_io_error() {
        let line = r#"{"command":"doctor","path":"."}"#;
        let failing = |_req: AstgenRequest| -> Result<serde_json::Value> {
            anyhow::bail!("handler failed");
        };
        let result = process_line(line, 1024, &failing).expect("should produce an error JSON");

        assert_eq!(result["error"]["code"], "IO_ERROR");
        assert!(
            result["error"]["message"]
                .as_str()
                .expect("message should be string")
                .contains("handler failed")
        );
    }

    /// `AstroError` は CLI / batch と同じ機械可読コードで返し、message にコードを重ねない。
    ///
    /// 旧実装は全ハンドラエラーを `IO_ERROR` に潰していたため、サンドボックス拒否
    /// (`PATH_OUT_OF_BOUNDS`) まで I/O エラーとして報告されていた。さらに
    /// `format!("{e}")` は `AstroError` の Display (`[CODE] message`) を通るため、
    /// session だけ message に `[CODE] ` が二重に入っていた。
    #[test]
    fn process_line_preserves_astro_error_code_without_duplicating_it_in_message() {
        let line = r#"{"command":"doctor","path":"."}"#;
        let failing = |_req: AstgenRequest| -> Result<serde_json::Value> {
            Err(crate::error::AstroError::new(
                crate::error::ErrorCode::PathOutOfBounds,
                "Path outside workspace boundary: /etc/hosts",
            )
            .into())
        };
        let result = process_line(line, 1024, &failing).expect("should produce an error JSON");

        assert_eq!(result["error"]["code"], "PATH_OUT_OF_BOUNDS");
        let message = result["error"]["message"]
            .as_str()
            .expect("message should be string");
        assert!(
            !message.contains("[PATH_OUT_OF_BOUNDS]"),
            "message must not repeat the code: {message}"
        );
        assert!(
            message.contains("/etc/hosts"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn read_line_limited_rejects_oversized_input_and_continues() {
        let mut input = io::Cursor::new(b"12345\n{}\n".to_vec());
        let mut scratch = Vec::new();

        let first = read_line_limited(&mut input, 4, &mut scratch).expect("read first line");
        match first {
            ReadLine::Oversized(size) => assert_eq!(size, 5),
            _ => panic!("first line should be oversized"),
        }

        let second = read_line_limited(&mut input, 4, &mut scratch).expect("read second line");
        match second {
            ReadLine::Line(line) => assert_eq!(line, "{}"),
            _ => panic!("second line should be readable"),
        }
    }

    #[test]
    fn read_line_limited_strips_crlf() {
        let mut input = io::Cursor::new(b"{}\r\n".to_vec());
        let mut scratch = Vec::new();

        let line = read_line_limited(&mut input, 4, &mut scratch).expect("read line");
        match line {
            ReadLine::Line(line) => assert_eq!(line, "{}"),
            _ => panic!("line should be parsed"),
        }
    }
}
