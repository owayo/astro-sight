use anyhow::Result;
use rayon::prelude::*;
use tracing::info;

use crate::output::{OutputFormat, OutputOptions, serialize_toon_list_item, toon};
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

/// バッチ 1 件分の描画結果。
///
/// `auto` は「全レコードを出し終えるまで勝敗が決まらない」一方、バッチは解析結果を
/// 全件バッファしない設計なので、最初の window だけ両形式を保持して勝者を決める
/// (`Both`)。決まった後の window は勝者だけを描画する (`One`)。
pub(crate) enum BatchRendered {
    One(String),
    Both { json: String, toon: String },
}

impl BatchRendered {
    fn take(self, format: OutputFormat) -> String {
        match self {
            BatchRendered::One(text) => text,
            BatchRendered::Both { json, toon } => match format {
                OutputFormat::Toon => toon,
                _ => json,
            },
        }
    }

    /// `auto` の集計用。`(json 側の文字数, toon 側の文字数)`。
    fn char_lens(&self) -> (usize, usize) {
        match self {
            BatchRendered::One(text) => {
                let n = text.chars().count();
                (n, n)
            }
            BatchRendered::Both { json, toon } => (json.chars().count(), toon.chars().count()),
        }
    }
}

/// バッチ 1 件分の結果を出力形式に合わせて描画する。
///
/// - JSON: 従来どおり 1 行の compact JSON (NDJSON の 1 レコード)
/// - TOON: ルート配列の list item (`  - ...`、複数行になりうる)
/// - auto: 両方 (勝者は呼び出し側が window 単位で決める)
pub(crate) fn render_batch_record<T: serde::Serialize>(
    value: &T,
    output: OutputOptions,
) -> BatchRendered {
    match output.format() {
        OutputFormat::Json => BatchRendered::One(render_json_record(value)),
        OutputFormat::Toon => BatchRendered::One(render_toon_record(value, output)),
        OutputFormat::Auto => BatchRendered::Both {
            json: render_json_record(value),
            toon: render_toon_record(value, output),
        },
    }
}

fn render_json_record<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| make_error_line(&e.into()))
}

fn render_toon_record<T: serde::Serialize>(value: &T, output: OutputOptions) -> String {
    serialize_toon_list_item(value)
        .unwrap_or_else(|e| toon_error_item(&anyhow::anyhow!(e.to_string()), output))
}

/// バッチ 1 件分の失敗レコード。TOON でも **必ず 1 要素を出す** — ヘッダで宣言した
/// 要素数 `[N]` と実際の item 数が食い違うと strict decoder が落ちるため。
pub(crate) fn render_batch_error(e: &anyhow::Error, output: OutputOptions) -> BatchRendered {
    match output.format() {
        OutputFormat::Json => BatchRendered::One(make_error_line(e)),
        OutputFormat::Toon => BatchRendered::One(toon_error_item(e, output)),
        OutputFormat::Auto => BatchRendered::Both {
            json: make_error_line(e),
            toon: toon_error_item(e, output),
        },
    }
}

fn toon_error_item(e: &anyhow::Error, _output: OutputOptions) -> String {
    let (code, message) = classify_error(e);
    let value = serde_json::json!({ "error": { "code": code, "message": message } });
    // ここで失敗すると要素数が合わなくなるので、最低限の妥当な list item に倒す。
    serialize_toon_list_item(&value).unwrap_or_else(|_| "  - error: encoding failed".to_string())
}

fn batch_ndjson<F>(paths: &[String], output: OutputOptions, process: F) -> Result<()>
where
    F: Fn(&str, OutputOptions) -> BatchRendered + Sync,
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
    F: Fn(&str, OutputOptions) -> BatchRendered + Sync,
    W: std::io::Write,
{
    // 全スレッドを飽和させながら、未排出の解析結果をワーカー数の定数倍に制限する。
    let window_size = batch_worker_count().saturating_mul(8).max(1);
    batch_ndjson_to_windowed(paths, output, process, out, window_size)
}

/// 先頭 window の実測値からバッチ全体の勝者を決める。
///
/// TOON のルート配列ヘッダは **バッチ全体で 1 行きり**のコストなので、window の合計に
/// 丸ごと足すと window 数が多いほど TOON を不当に不利にしてしまう。window の合計を
/// バッチ全体へ引き伸ばしてから比較する (両辺に window 件数を掛けて整数のまま扱う)。
/// 1 window で収まる入力ではこの引き伸ばしが恒等変換になり、比較は厳密になる。
///
/// レコードごとの改行は両形式で同数なので相殺され、比較には現れない。
/// 同点は JSON (既定フォーマットで消費側の互換性が高い)。
fn decide_batch_format(
    json_len: usize,
    toon_len: usize,
    header_len: usize,
    window_records: usize,
    total_records: usize,
) -> OutputFormat {
    let window_records = window_records.max(1) as u128;
    let total_records = total_records.max(1) as u128;

    let json_total = json_len as u128 * total_records;
    // ヘッダ本体 + 改行 1 個。window 件数を掛けているのは両辺のスケールを合わせるため。
    let toon_total = toon_len as u128 * total_records + (header_len as u128 + 1) * window_records;

    if toon_total < json_total {
        OutputFormat::Toon
    } else {
        OutputFormat::Json
    }
}

fn batch_ndjson_to_windowed<F, W>(
    paths: &[String],
    output: OutputOptions,
    process: F,
    mut out: W,
    window_size: usize,
) -> Result<usize>
where
    F: Fn(&str, OutputOptions) -> BatchRendered + Sync,
    W: std::io::Write,
{
    let window_size = window_size.max(1);
    let pool = build_batch_pool()?;
    let mut bytes = 0usize;

    // `auto` は最初の window を両形式で描画してから勝者を決める。以降の window は
    // 勝者だけを描画するので、二重エンコードのコストは先頭 window 分だけで済む
    // (解析自体はどちらの経路でもパス 1 回きり)。
    let mut resolved = if output.is_auto() { None } else { Some(output) };

    // TOON はルート配列を list form (§9.4) で開く。要素数は入力パス数と 1:1 なので
    // 解析結果を溜めずにヘッダを先出しでき、ピーク RSS を入力件数から独立させたまま
    // 1 個の妥当な TOON ドキュメントになる。外側配列を tabular form (§9.3) にするには
    // 全要素を見る必要があり、この streaming 要件と両立しないため list form を使う。
    if let Some(opts) = resolved
        && opts.is_toon()
    {
        let header = toon::streaming_array_header(paths.len());
        bytes += header.len() + 1;
        writeln!(out, "{header}")?;
    }

    for chunk in paths.chunks(window_size) {
        // IndexedParallelIterator の collect は入力順を保つため、chunk 間も含めて
        // 呼び出し元が指定したパス順を維持できる。
        let render_opts = resolved.unwrap_or(output);
        let rendered: Vec<BatchRendered> =
            pool.install(|| chunk.par_iter().map(|p| process(p, render_opts)).collect());

        let opts = match resolved {
            Some(opts) => opts,
            None => {
                // 先頭 window の実測値で勝者を決める。全件を見てから決めるには
                // 解析結果を全件保持する必要があり、ピーク RSS の要件を壊すため
                // 「同じコマンドの実データによる標本」で近似する (決定的)。
                let (json_len, toon_len) = rendered.iter().fold((0, 0), |(j, t), r| {
                    let (rj, rt) = r.char_lens();
                    (j + rj, t + rt)
                });
                let header = toon::streaming_array_header(paths.len());
                let winner = decide_batch_format(
                    json_len,
                    toon_len,
                    header.chars().count(),
                    chunk.len(),
                    paths.len(),
                );
                let opts = output.with_format(winner);
                resolved = Some(opts);
                if winner == OutputFormat::Toon {
                    bytes += header.len() + 1;
                    writeln!(out, "{header}")?;
                }
                opts
            }
        };

        for record in rendered {
            let line = record.take(opts.format());
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
    batch_ndjson(paths, output, |p, output| {
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
    batch_ndjson(paths, output, |p, output| {
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
    batch_ndjson(paths, output, |p, output| {
        match service.extract_calls(p, func.as_deref()) {
            Ok(result) => render_batch_record(&result.to_compact(), output),
            Err(e) => render_batch_error(&e, output),
        }
    })
}

pub fn batch_imports(service: &AppService, paths: &[String], output: OutputOptions) -> Result<()> {
    batch_ndjson(paths, output, |p, output| {
        match service.extract_imports(p) {
            Ok(result) => render_batch_record(&result, output),
            Err(e) => render_batch_error(&e, output),
        }
    })
}

pub fn batch_lint(
    service: &AppService,
    paths: &[String],
    rules: &[crate::models::lint::Rule],
    output: OutputOptions,
) -> Result<()> {
    batch_ndjson(paths, output, |p, output| {
        match service.lint_file(p, rules) {
            Ok(result) => render_batch_record(&result, output),
            Err(e) => render_batch_error(&e, output),
        }
    })
}

pub fn batch_sequence(
    service: &AppService,
    paths: &[String],
    function: Option<&str>,
    output: OutputOptions,
) -> Result<()> {
    let func = function.map(|s| s.to_string());
    batch_ndjson(paths, output, |p, output| {
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
        BatchRendered, batch_ndjson_to_windowed, effective_batch_worker_count, render_batch_error,
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
            |path, _| BatchRendered::One(format!("result-{path}")),
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
            |_, _| {
                calls.fetch_add(1, Ordering::Relaxed);
                BatchRendered::One(String::new())
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
            |_, _| {
                calls.fetch_add(1, Ordering::Relaxed);
                BatchRendered::One("result".to_string())
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
    fn batch_format_decision_amortizes_the_toon_header() {
        use super::decide_batch_format;

        // 1 window で収まる入力: ヘッダ込みで厳密に比較する。
        // json=100, toon=100 → 同点 + ヘッダ分で JSON。
        assert_eq!(decide_batch_format(100, 100, 4, 10, 10), OutputFormat::Json);
        // json=100, toon=90 → ヘッダ 5 文字を足しても TOON が短い。
        assert_eq!(decide_batch_format(100, 90, 4, 10, 10), OutputFormat::Toon);
        // json=100, toon=96 → ヘッダ 5 文字でひっくり返って JSON。
        assert_eq!(decide_batch_format(100, 96, 4, 10, 10), OutputFormat::Json);

        // window が全体の一部なら、ヘッダは 1 回きりのコストとして薄まる。
        // 同じ window 実測値 (json=100, toon=96) でも、全体 1000 件なら TOON が勝つ。
        assert_eq!(
            decide_batch_format(100, 96, 10, 4, 1000),
            OutputFormat::Toon
        );
    }

    #[test]
    fn batch_format_decision_is_json_on_ties() {
        use super::decide_batch_format;
        assert_eq!(decide_batch_format(50, 50, 0, 1, 1), OutputFormat::Json);
    }

    #[test]
    fn toon_batch_wraps_records_in_a_root_array_header() {
        // 要素数は入力パス数と 1:1 なので、結果を溜めずにヘッダを先出しできる。
        let paths = (0..3).map(|i| format!("f{i}.rs")).collect::<Vec<_>>();
        let mut output = Vec::new();

        let bytes = batch_ndjson_to_windowed(
            &paths,
            toon(),
            |path, opts| render_batch_record(&serde_json::json!({ "p": path }), opts),
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
            |path, opts| render_batch_record(&serde_json::json!({ "p": path }), opts),
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
        let item = render_batch_error(&error, toon()).take(OutputFormat::Toon);
        assert!(item.starts_with("  - error:"), "unexpected item: {item:?}");
        assert!(item.contains("message: boom"), "unexpected item: {item:?}");
        // JSON 経路は従来の 1 行エラーレコードのまま。
        let line = render_batch_error(&error, json()).take(OutputFormat::Json);
        assert!(line.starts_with("{\"error\":"), "unexpected line: {line:?}");
        assert!(!line.contains('\n'));
    }
}
