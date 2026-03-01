use anyhow::Result;
use std::io::{self, BufRead, Write};
use tracing::info;

use crate::models::request::AstgenRequest;

/// Maximum line size for session input: 100 MB.
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
        Err(e) => make_error("IO_ERROR", format!("{e}")),
    })
}

/// Run an NDJSON streaming session: read requests from stdin, process, write responses to stdout.
pub fn run_session(handler: impl Fn(AstgenRequest) -> Result<serde_json::Value>) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = line?;
        if let Some(value) = process_line(&line, MAX_LINE_SIZE, &handler) {
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
}
