use anyhow::Result;
use rayon::prelude::*;
use tracing::info;

use crate::service::{AppService, AstParams};

use super::common::make_error_line;

/// AST バッチコマンドの worker 数。tree-sitter Parser は大きな生成物を解析すると
/// thread-local の作業領域を保持するため、論理 CPU 数まで増やすとピーク RSS が worker 数に
/// 応じて膨らむ。既定を最大 4 に抑え、明示設定時だけ上限を引き上げる。
fn batch_worker_count() -> usize {
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let configured = std::env::var("ASTRO_SIGHT_BATCH_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok());
    effective_batch_worker_count(available, configured)
}

fn effective_batch_worker_count(available: usize, configured: Option<usize>) -> usize {
    let configured = configured.filter(|&n| n > 0).unwrap_or(4);
    available.max(1).min(configured)
}

fn build_batch_pool() -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(batch_worker_count())
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build batch rayon pool: {e}"))
}

fn batch_ndjson<F>(paths: &[String], process: F) -> Result<()>
where
    F: Fn(&str) -> String + Sync,
{
    let stdout = std::io::stdout();
    let out = std::io::BufWriter::new(stdout.lock());
    let written = batch_ndjson_to(paths, process, out)?;
    info!(
        batch_size = paths.len(),
        output_bytes = written,
        "batch completed"
    );
    Ok(())
}

fn batch_ndjson_to<F, W>(paths: &[String], process: F, out: W) -> Result<usize>
where
    F: Fn(&str) -> String + Sync,
    W: std::io::Write,
{
    // 全スレッドを飽和させながら、未排出の解析結果をワーカー数の定数倍に制限する。
    let window_size = batch_worker_count().saturating_mul(8).max(1);
    batch_ndjson_to_windowed(paths, process, out, window_size)
}

fn batch_ndjson_to_windowed<F, W>(
    paths: &[String],
    process: F,
    mut out: W,
    window_size: usize,
) -> Result<usize>
where
    F: Fn(&str) -> String + Sync,
    W: std::io::Write,
{
    let window_size = window_size.max(1);
    let pool = build_batch_pool()?;
    let mut bytes = 0usize;

    for chunk in paths.chunks(window_size) {
        // IndexedParallelIterator の collect は入力順を保つため、chunk 間も含めて
        // 呼び出し元が指定したパス順の NDJSON を維持できる。
        let lines: Vec<String> = pool.install(|| chunk.par_iter().map(|p| process(p)).collect());
        for line in lines {
            bytes += line.len() + 1;
            writeln!(out, "{line}")?;
        }
        // broken pipe 等を chunk 境界で検出し、残りの解析を早期に打ち切る。
        out.flush()?;
    }

    Ok(bytes)
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
    query: Option<&str>,
) -> Result<()> {
    batch_ndjson(paths, |p| {
        match service.extract_symbols_with_query(p, query) {
            Ok(mut response) => {
                // dir 指定時に絶対パスを相対パスに変換
                if let Some(base) = dir
                    && let Ok(rel) =
                        std::path::Path::new(&response.location.path).strip_prefix(base)
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
        }
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

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{batch_ndjson_to_windowed, effective_batch_worker_count};

    #[test]
    fn batch_worker_count_defaults_to_at_most_four() {
        assert_eq!(effective_batch_worker_count(16, None), 4);
        assert_eq!(effective_batch_worker_count(2, None), 2);
    }

    #[test]
    fn batch_worker_count_honors_valid_override_and_rejects_zero() {
        assert_eq!(effective_batch_worker_count(16, Some(8)), 8);
        assert_eq!(effective_batch_worker_count(4, Some(0)), 4);
        assert_eq!(effective_batch_worker_count(0, None), 1);
    }

    #[test]
    fn batch_ndjson_windowed_preserves_order_and_byte_count() {
        let paths = (0..5).map(|i| i.to_string()).collect::<Vec<_>>();
        let mut output = Vec::new();

        let bytes =
            batch_ndjson_to_windowed(&paths, |path| format!("result-{path}"), &mut output, 2)
                .expect("batch should succeed");

        assert_eq!(
            String::from_utf8(output.clone()).expect("valid UTF-8"),
            "result-0\nresult-1\nresult-2\nresult-3\nresult-4\n"
        );
        assert_eq!(bytes, output.len());
    }

    #[test]
    fn batch_ndjson_windowed_empty_input_does_not_call_processor() {
        let calls = AtomicUsize::new(0);
        let mut output = Vec::new();

        let bytes = batch_ndjson_to_windowed(
            &[],
            |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                String::new()
            },
            &mut output,
            2,
        )
        .expect("empty batch should succeed");

        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(bytes, 0);
        assert!(output.is_empty());
    }

    struct BrokenPipeWriter;

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn batch_ndjson_windowed_writer_error_stops_before_next_window() {
        let paths = (0..5).map(|i| i.to_string()).collect::<Vec<_>>();
        let calls = AtomicUsize::new(0);

        let error = batch_ndjson_to_windowed(
            &paths,
            |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                "result".to_string()
            },
            BrokenPipeWriter,
            2,
        )
        .expect_err("broken pipe should be propagated");

        assert_eq!(
            error.downcast_ref::<io::Error>().expect("I/O error").kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
