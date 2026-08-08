use anyhow::Result;
use rayon::prelude::*;
use tracing::info;

use crate::output::{OutputOptions, toon};
use crate::service::{AppService, AstParams};

use super::common::{classify_error, make_error_line};

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

/// バッチ 1 件分の結果を出力形式に合わせて描画する。
///
/// - JSON: 従来どおり 1 行の compact JSON (NDJSON の 1 レコード)
/// - TOON: ルート配列の list item (`  - ...`、複数行になりうる)
pub(crate) fn render_batch_record<T: serde::Serialize>(
    value: &T,
    output: OutputOptions,
) -> String {
    if output.is_toon() {
        return toon::streaming_array_item(value)
            .unwrap_or_else(|e| render_batch_error(&anyhow::anyhow!(e.to_string()), output));
    }
    serde_json::to_string(value).unwrap_or_else(|e| make_error_line(&e.into()))
}

/// バッチ 1 件分の失敗レコード。TOON でも **必ず 1 要素を出す** — ヘッダで宣言した
/// 要素数 `[N]` と実際の item 数が食い違うと strict decoder が落ちるため。
pub(crate) fn render_batch_error(e: &anyhow::Error, output: OutputOptions) -> String {
    if output.is_toon() {
        let (code, message) = classify_error(e);
        let value = serde_json::json!({ "error": { "code": code, "message": message } });
        // ここで失敗すると要素数が合わなくなるので、最低限の妥当な list item に倒す。
        return toon::streaming_array_item(&value)
            .unwrap_or_else(|_| "  - error: encoding failed".to_string());
    }
    make_error_line(e)
}

fn batch_ndjson<F>(paths: &[String], output: OutputOptions, process: F) -> Result<()>
where
    F: Fn(&str) -> String + Sync,
{
    let stdout = std::io::stdout();
    let out = std::io::BufWriter::new(stdout.lock());
    let written = batch_ndjson_to(paths, output, process, out)?;
    info!(
        batch_size = paths.len(),
        output_bytes = written,
        format = output.format().as_str(),
        "batch completed"
    );
    Ok(())
}

fn batch_ndjson_to<F, W>(
    paths: &[String],
    output: OutputOptions,
    process: F,
    out: W,
) -> Result<usize>
where
    F: Fn(&str) -> String + Sync,
    W: std::io::Write,
{
    // 全スレッドを飽和させながら、未排出の解析結果をワーカー数の定数倍に制限する。
    let window_size = batch_worker_count().saturating_mul(8).max(1);
    batch_ndjson_to_windowed(paths, output, process, out, window_size)
}

fn batch_ndjson_to_windowed<F, W>(
    paths: &[String],
    output: OutputOptions,
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

    // TOON はルート配列を list form (§9.4) で開く。要素数は入力パス数と 1:1 なので
    // 解析結果を溜めずにヘッダを先出しでき、ピーク RSS を入力件数から独立させたまま
    // 1 個の妥当な TOON ドキュメントになる。外側配列を tabular form (§9.3) にするには
    // 全要素を見る必要があり、この streaming 要件と両立しないため list form を使う。
    if output.is_toon() {
        let header = toon::streaming_array_header(paths.len());
        bytes += header.len() + 1;
        writeln!(out, "{header}")?;
    }

    for chunk in paths.chunks(window_size) {
        // IndexedParallelIterator の collect は入力順を保つため、chunk 間も含めて
        // 呼び出し元が指定したパス順を維持できる。
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
    output: OutputOptions,
) -> Result<()> {
    batch_ndjson(paths, output, |p| {
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
                    render_batch_record(&response, output)
                } else {
                    render_batch_record(&response.to_compact_ast(), output)
                }
            }
            Err(e) => render_batch_error(&e, output),
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
    output: OutputOptions,
) -> Result<()> {
    batch_ndjson(paths, output, |p| {
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
                    render_batch_record(&response, output)
                } else {
                    render_batch_record(&response.to_compact_symbols(doc), output)
                }
            }
            Err(e) => render_batch_error(&e, output),
        }
    })
}

pub fn batch_calls(
    service: &AppService,
    paths: &[String],
    function: Option<&str>,
    output: OutputOptions,
) -> Result<()> {
    let func = function.map(|s| s.to_string());
    batch_ndjson(paths, output, |p| {
        match service.extract_calls(p, func.as_deref()) {
            Ok(result) => render_batch_record(&result.to_compact(), output),
            Err(e) => render_batch_error(&e, output),
        }
    })
}

pub fn batch_imports(
    service: &AppService,
    paths: &[String],
    output: OutputOptions,
) -> Result<()> {
    batch_ndjson(paths, output, |p| match service.extract_imports(p) {
        Ok(result) => render_batch_record(&result, output),
        Err(e) => render_batch_error(&e, output),
    })
}

pub fn batch_lint(
    service: &AppService,
    paths: &[String],
    rules: &[crate::models::lint::Rule],
    output: OutputOptions,
) -> Result<()> {
    batch_ndjson(paths, output, |p| match service.lint_file(p, rules) {
        Ok(result) => render_batch_record(&result, output),
        Err(e) => render_batch_error(&e, output),
    })
}

pub fn batch_sequence(
    service: &AppService,
    paths: &[String],
    function: Option<&str>,
    output: OutputOptions,
) -> Result<()> {
    let func = function.map(|s| s.to_string());
    batch_ndjson(paths, output, |p| {
        match service.generate_sequence(p, func.as_deref()) {
            Ok(result) => render_batch_record(&result, output),
            Err(e) => render_batch_error(&e, output),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::output::{JsonStyle, OutputFormat, OutputOptions};

    use super::{
        batch_ndjson_to_windowed, effective_batch_worker_count, render_batch_error,
        render_batch_record,
    };

    fn json() -> OutputOptions {
        OutputOptions::new(OutputFormat::Json, JsonStyle::Compact)
    }

    fn toon() -> OutputOptions {
        OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact)
    }

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

        let bytes = batch_ndjson_to_windowed(
            &paths,
            json(),
            |path| format!("result-{path}"),
            &mut output,
            2,
        )
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
            json(),
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
            json(),
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

    #[test]
    fn toon_batch_wraps_records_in_a_root_array_header() {
        // 要素数は入力パス数と 1:1 なので、結果を溜めずにヘッダを先出しできる。
        let paths = (0..3).map(|i| format!("f{i}.rs")).collect::<Vec<_>>();
        let mut output = Vec::new();

        let bytes = batch_ndjson_to_windowed(
            &paths,
            toon(),
            |path| render_batch_record(&serde_json::json!({ "p": path }), toon()),
            &mut output,
            2,
        )
        .expect("batch should succeed");

        assert_eq!(
            String::from_utf8(output.clone()).expect("valid UTF-8"),
            "[3]:\n  - p: f0.rs\n  - p: f1.rs\n  - p: f2.rs\n"
        );
        assert_eq!(bytes, output.len());
    }

    #[test]
    fn toon_batch_emits_no_header_for_json() {
        // JSON 経路は従来どおり NDJSON のまま (ヘッダ行を足さない)。
        let mut output = Vec::new();
        batch_ndjson_to_windowed(
            &["a".to_string()],
            json(),
            |path| render_batch_record(&serde_json::json!({ "p": path }), json()),
            &mut output,
            2,
        )
        .expect("batch should succeed");

        assert_eq!(
            String::from_utf8(output).expect("valid UTF-8"),
            "{\"p\":\"a\"}\n"
        );
    }

    #[test]
    fn toon_batch_failures_still_produce_exactly_one_item() {
        // ヘッダの `[N]` と item 数が食い違うと strict decoder が落ちるため、
        // 失敗レコードも 1 要素として出す必要がある。
        let error = anyhow::anyhow!("boom");
        let item = render_batch_error(&error, toon());
        assert!(item.starts_with("  - error:"), "unexpected item: {item:?}");
        assert!(item.contains("message: boom"), "unexpected item: {item:?}");
        // JSON 経路は従来の 1 行エラーレコードのまま。
        let line = render_batch_error(&error, json());
        assert!(line.starts_with("{\"error\":"), "unexpected line: {line:?}");
        assert!(!line.contains('\n'));
    }
}
