use anyhow::Result;
use std::io::{self, BufRead, Write};

use crate::models::request::AstgenRequest;

/// Maximum line size for session input: 100 MB.
const MAX_LINE_SIZE: usize = 100 * 1024 * 1024;

/// Run an NDJSON streaming session: read requests from stdin, process, write responses to stdout.
pub fn run_session(handler: impl Fn(AstgenRequest) -> Result<serde_json::Value>) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.len() > MAX_LINE_SIZE {
            let err = serde_json::json!({
                "error": {
                    "code": "INVALID_REQUEST",
                    "message": format!("Input line exceeds maximum size ({} bytes > {} bytes)", trimmed.len(), MAX_LINE_SIZE)
                }
            });
            serde_json::to_writer(&mut out, &err)?;
            out.write_all(b"\n")?;
            out.flush()?;
            continue;
        }

        let result = match serde_json::from_str::<AstgenRequest>(trimmed) {
            Ok(req) => handler(req),
            Err(e) => Ok(serde_json::json!({
                "error": {
                    "code": "INVALID_REQUEST",
                    "message": format!("Invalid JSON request: {e}")
                }
            })),
        };

        match result {
            Ok(value) => {
                serde_json::to_writer(&mut out, &value)?;
                out.write_all(b"\n")?;
                out.flush()?;
            }
            Err(e) => {
                let err = serde_json::json!({
                    "error": {
                        "code": "IO_ERROR",
                        "message": format!("{e}")
                    }
                });
                serde_json::to_writer(&mut out, &err)?;
                out.write_all(b"\n")?;
                out.flush()?;
            }
        }
    }

    Ok(())
}
