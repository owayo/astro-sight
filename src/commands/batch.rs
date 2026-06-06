use anyhow::{Result, anyhow};
use rayon::prelude::*;
use tracing::info;

use crate::service::{AppService, AstParams};

use super::common::make_error_line;

fn batch_ndjson<F>(paths: &[String], process: F) -> Result<()>
where
    F: Fn(&str) -> String + Sync,
{
    use std::io::Write;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    if paths.is_empty() {
        return Ok(());
    }

    let batch_size = paths.len();
    // 各スロットは排出時に `take` されるため Mutex<Option<String>>
    let slots: Vec<Mutex<Option<String>>> = (0..batch_size).map(|_| Mutex::new(None)).collect();
    let (tx, rx) = mpsc::channel::<usize>();
    let next_to_write = AtomicUsize::new(0);

    std::thread::scope(|scope| -> Result<()> {
        let slots_ref = &slots;
        let next_to_write_ref = &next_to_write;

        let writer = scope.spawn(move || -> Result<usize> {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let mut bytes = 0usize;
            // 完了通知を受け取り、次に書くべきインデックスが揃っている間は順次排出する
            for _ in rx {
                loop {
                    let cur = next_to_write_ref.load(Ordering::Acquire);
                    if cur >= batch_size {
                        break;
                    }
                    let taken = {
                        let mut guard = slots_ref[cur].lock().expect("slot mutex poisoned");
                        guard.take()
                    };
                    if let Some(line) = taken {
                        bytes += line.len() + 1;
                        writeln!(out, "{line}")?;
                        next_to_write_ref.store(cur + 1, Ordering::Release);
                    } else {
                        break;
                    }
                }
            }
            Ok(bytes)
        });

        paths
            .par_iter()
            .enumerate()
            .for_each_with(tx, |tx, (i, p)| {
                let line = process(p);
                *slots_ref[i].lock().expect("slot mutex poisoned") = Some(line);
                let _ = tx.send(i);
            });

        let written = writer
            .join()
            .map_err(|_| anyhow!("batch_ndjson writer thread panicked"))??;
        info!(
            batch_size = batch_size,
            output_bytes = written,
            "batch completed"
        );
        Ok(())
    })
}

pub fn batch_ast(
    service: &AppService,
    paths: &[String],
    depth: usize,
    context_lines: usize,
    full: bool,
) -> Result<()> {
    batch_ndjson(paths, |p| {
        let params = AstParams {
            path: p,
            line: None,
            col: None,
            end_line: None,
            end_col: None,
            depth,
            context_lines,
        };
        match service.extract_ast(&params) {
            Ok(response) => {
                if full {
                    serde_json::to_string(&response).unwrap_or_else(|e| make_error_line(&e.into()))
                } else {
                    serde_json::to_string(&response.to_compact_ast())
                        .unwrap_or_else(|e| make_error_line(&e.into()))
                }
            }
            Err(e) => make_error_line(&e),
        }
    })
}

pub fn batch_symbols(
    service: &AppService,
    paths: &[String],
    doc: bool,
    full: bool,
    dir: Option<&std::path::Path>,
) -> Result<()> {
    batch_ndjson(paths, |p| match service.extract_symbols(p) {
        Ok(mut response) => {
            // dir 指定時に絶対パスを相対パスに変換
            if let Some(base) = dir
                && let Ok(rel) = std::path::Path::new(&response.location.path).strip_prefix(base)
            {
                response.location.path = rel.to_string_lossy().to_string();
            }
            if full {
                serde_json::to_string(&response).unwrap_or_else(|e| make_error_line(&e.into()))
            } else {
                let compact = response.to_compact_symbols(doc);
                serde_json::to_string(&compact).unwrap_or_else(|e| make_error_line(&e.into()))
            }
        }
        Err(e) => make_error_line(&e),
    })
}

pub fn batch_calls(service: &AppService, paths: &[String], function: Option<&str>) -> Result<()> {
    let func = function.map(|s| s.to_string());
    batch_ndjson(paths, |p| match service.extract_calls(p, func.as_deref()) {
        Ok(result) => serde_json::to_string(&result.to_compact())
            .unwrap_or_else(|e| make_error_line(&e.into())),
        Err(e) => make_error_line(&e),
    })
}

pub fn batch_imports(service: &AppService, paths: &[String]) -> Result<()> {
    batch_ndjson(paths, |p| match service.extract_imports(p) {
        Ok(result) => serde_json::to_string(&result).unwrap_or_else(|e| make_error_line(&e.into())),
        Err(e) => make_error_line(&e),
    })
}

pub fn batch_lint(
    service: &AppService,
    paths: &[String],
    rules: &[crate::models::lint::Rule],
) -> Result<()> {
    batch_ndjson(paths, |p| match service.lint_file(p, rules) {
        Ok(result) => serde_json::to_string(&result).unwrap_or_else(|e| make_error_line(&e.into())),
        Err(e) => make_error_line(&e),
    })
}

pub fn batch_sequence(
    service: &AppService,
    paths: &[String],
    function: Option<&str>,
) -> Result<()> {
    let func = function.map(|s| s.to_string());
    batch_ndjson(paths, |p| {
        match service.generate_sequence(p, func.as_deref()) {
            Ok(result) => {
                serde_json::to_string(&result).unwrap_or_else(|e| make_error_line(&e.into()))
            }
            Err(e) => make_error_line(&e),
        }
    })
}
