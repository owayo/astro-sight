use anyhow::Result;
use rayon::prelude::*;
use tracing::info;

use crate::models::skip::SkippedFiles;
use crate::output::{OutputFormat, OutputOptions, estimated_size, serialize_toon_list_item, toon};
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

/// `--format auto` のバッチで形式の勝者を決める標本の件数 (出力順の先頭から数える)。
///
/// **並列度から独立した定数でなければならない**。旧実装は「最初の window
/// (= ワーカー数 × 8 件)」を標本にしていたため、同じ入力でも CPU 数や
/// `ASTRO_SIGHT_BATCH_WORKERS` で出力形式が変わっていた (auto の選択は入力内容だけで
/// 決まるという契約に反する)。値は旧実装の既定 (4 ワーカー × 8 件) に揃えてあり、
/// 4 コア以上のマシンの既定設定では従来と同じ判定になる。
///
/// 標本が揃うまでは両形式の描画を保持するので、保留分は最大でも
/// `AUTO_SAMPLE_RECORDS + window - 1` 件 = 入力件数から独立する。
const AUTO_SAMPLE_RECORDS: usize = 32;

/// バッチ 1 件分の描画結果。
///
/// `auto` は「全レコードを出し終えるまで勝敗が決まらない」一方、バッチは解析結果を
/// 全件バッファしない設計なので、先頭 [`AUTO_SAMPLE_RECORDS`] 件だけ両形式を保持して
/// 勝者を決める (`Both`)。決まった後のレコードは勝者だけを描画する (`One`)。
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

    /// `auto` の集計用。`(json 側の推定サイズ, toon 側の推定サイズ)`。
    /// 単位は `output::estimated_size` (文字数 + 行罰則) で、単一ドキュメント側の
    /// 判定と同じ物差しを使う。
    fn size_metrics(&self) -> (usize, usize) {
        match self {
            BatchRendered::One(text) => {
                let n = estimated_size(text);
                (n, n)
            }
            BatchRendered::Both { json, toon } => (estimated_size(json), estimated_size(toon)),
        }
    }
}

/// バッチ 1 件分の結果を出力形式に合わせて描画する。
///
/// - JSON: 従来どおり 1 行の compact JSON (NDJSON の 1 レコード)
/// - TOON: ルート配列の list item (`  - ...`、複数行になりうる)
/// - auto: 両方 (勝者は呼び出し側が先頭 [`AUTO_SAMPLE_RECORDS`] 件の標本で決める)
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

fn batch_ndjson_with_trailer<F>(
    paths: &[String],
    trailer: Option<serde_json::Value>,
    output: OutputOptions,
    process: F,
) -> Result<()>
where
    F: Fn(&str, OutputOptions) -> BatchRendered + Sync,
{
    let stdout = std::io::stdout();
    let out = std::io::BufWriter::new(stdout.lock());
    let total_records = paths.len() + usize::from(trailer.is_some());
    let written = batch_ndjson_to_windowed_with_trailer(
        paths,
        trailer,
        output,
        process,
        out,
        batch_worker_count().saturating_mul(8).max(1),
    )?;
    info!(
        batch_size = paths.len(),
        total_records,
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

/// 標本 (出力順の先頭 [`AUTO_SAMPLE_RECORDS`] 件) の実測値からバッチ全体の勝者を決める。
///
/// TOON のルート配列ヘッダは **バッチ全体で 1 行きり**のコストなので、標本の合計に
/// 丸ごと足すと入力が多いほど TOON を不当に不利にしてしまう。標本の合計を
/// バッチ全体へ引き伸ばしてから比較する (両辺に標本件数を掛けて整数のまま扱う)。
/// 標本に収まる入力ではこの引き伸ばしが恒等変換になり、比較は厳密になる。
///
/// 各長さは `output::estimated_size` (文字数 + 行罰則) の単位。レコードを区切る改行は
/// 両形式で同数なので相殺され、比較には現れない。
/// 同点は JSON (既定フォーマットで消費側の互換性が高い)。
fn decide_batch_format(
    json_len: usize,
    toon_len: usize,
    header_size: usize,
    sample_records: usize,
    total_records: usize,
) -> OutputFormat {
    let sample_records = sample_records.max(1) as u128;
    let total_records = total_records.max(1) as u128;

    let json_total = json_len as u128 * total_records;
    // ヘッダは 1 回きりのコスト。標本件数を掛けているのは両辺のスケールを合わせるため。
    let toon_total = toon_len as u128 * total_records + header_size as u128 * sample_records;

    if toon_total < json_total {
        OutputFormat::Toon
    } else {
        OutputFormat::Json
    }
}

/// バッチ出力の書き手。TOON ルート配列ヘッダの先出しと、`auto` の勝者が決まるまでの
/// 保留を 1 箇所で扱う。
struct BatchWriter<W> {
    out: W,
    output: OutputOptions,
    /// 確定した出力設定。`auto` は標本が揃うまで `None`。
    resolved: Option<OutputOptions>,
    /// `auto` の判定待ちで保留しているレコード (両形式の描画を持つ)。
    pending: Vec<BatchRendered>,
    /// 標本の推定サイズ合計 (`(json, toon)`) と件数。
    sample_sizes: (usize, usize),
    sample_records: usize,
    total_records: usize,
    bytes: usize,
}

impl<W: std::io::Write> BatchWriter<W> {
    fn new(out: W, output: OutputOptions, total_records: usize) -> Result<Self> {
        let mut writer = Self {
            out,
            output,
            resolved: if output.is_auto() { None } else { Some(output) },
            pending: Vec::new(),
            sample_sizes: (0, 0),
            sample_records: 0,
            total_records,
            bytes: 0,
        };
        // TOON はルート配列を list form (§9.4) で開く。要素数は入力パス数に任意の
        // control record 1 件を加えた値として先に確定できるため、解析結果を溜めずに
        // ヘッダを先出しでき、ピーク RSS を入力件数から独立させたまま
        // 1 個の妥当な TOON ドキュメントになる。外側配列を tabular form (§9.3) にするには
        // 全要素を見る必要があり、この streaming 要件と両立しないため list form を使う。
        if output.is_toon() {
            writer.write_header()?;
        }
        Ok(writer)
    }

    /// 次に描画するレコードの出力設定。`auto` の判定前は両形式を描画させる。
    fn render_options(&self) -> OutputOptions {
        self.resolved.unwrap_or(self.output)
    }

    fn push(&mut self, record: BatchRendered) -> Result<()> {
        if let Some(opts) = self.resolved {
            return self.write_record(record, opts);
        }
        // 標本は「出力順の先頭 N 件」で固定する。window 単位で判定すると、window の大きさ
        // (= 並列度) によって標本が変わり、同じ入力でも出力形式が変わってしまう。
        let (json, toon) = record.size_metrics();
        self.sample_sizes.0 += json;
        self.sample_sizes.1 += toon;
        self.sample_records += 1;
        self.pending.push(record);
        if self.sample_records >= AUTO_SAMPLE_RECORDS {
            self.resolve()?;
        }
        Ok(())
    }

    /// 標本から勝者を決め、ヘッダと保留分を書き出す。
    fn resolve(&mut self) -> Result<()> {
        let header = toon::streaming_array_header(self.total_records);
        // ヘッダと最初の item の区切り改行も本文と同じ物差しで測る。
        let header_size = estimated_size(&header) + estimated_size("\n");
        let winner = decide_batch_format(
            self.sample_sizes.0,
            self.sample_sizes.1,
            header_size,
            self.sample_records,
            self.total_records,
        );
        let opts = self.output.with_format(winner);
        self.resolved = Some(opts);
        if opts.is_toon() {
            self.write_header()?;
        }
        for record in std::mem::take(&mut self.pending) {
            self.write_record(record, opts)?;
        }
        Ok(())
    }

    fn write_header(&mut self) -> Result<()> {
        let header = toon::streaming_array_header(self.total_records);
        self.bytes += header.len();
        write!(self.out, "{header}")?;
        Ok(())
    }

    fn write_record(&mut self, record: BatchRendered, opts: OutputOptions) -> Result<()> {
        let line = record.take(opts.format());
        self.bytes += line.len() + 1;
        if opts.is_toon() {
            write!(self.out, "\n{line}")?;
        } else {
            writeln!(self.out, "{line}")?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }

    /// 全レコードを受け取った後に呼ぶ。標本件数に届かないまま入力が尽きた `auto` は
    /// 全件 (= 標本) で判定する。書き出したバイト数を返す。
    fn finish(mut self) -> Result<usize> {
        if self.resolved.is_none() {
            self.resolve()?;
        }
        self.flush()?;
        Ok(self.bytes)
    }
}

fn batch_ndjson_to_windowed<F, W>(
    paths: &[String],
    output: OutputOptions,
    process: F,
    out: W,
    window_size: usize,
) -> Result<usize>
where
    F: Fn(&str, OutputOptions) -> BatchRendered + Sync,
    W: std::io::Write,
{
    batch_ndjson_to_windowed_with_trailer(paths, None, output, process, out, window_size)
}

fn batch_ndjson_to_windowed_with_trailer<F, W>(
    paths: &[String],
    trailer: Option<serde_json::Value>,
    output: OutputOptions,
    process: F,
    out: W,
    window_size: usize,
) -> Result<usize>
where
    F: Fn(&str, OutputOptions) -> BatchRendered + Sync,
    W: std::io::Write,
{
    let window_size = window_size.max(1);
    let pool = build_batch_pool()?;
    let total_records = paths.len() + usize::from(trailer.is_some());

    // `auto` は先頭 AUTO_SAMPLE_RECORDS 件を両形式で描画してから勝者を決める。以降は
    // 勝者だけを描画するので、二重エンコードのコストは標本分だけで済む
    // (解析自体はどちらの経路でもパス 1 回きり)。全件を見てから決めるには解析結果を
    // 全件保持する必要があり、ピーク RSS の要件を壊すため「同じコマンドの実データによる
    // 標本」で近似する (標本は出力順の先頭で固定なので決定的)。
    let mut writer = BatchWriter::new(out, output, total_records)?;

    for chunk in paths.chunks(window_size) {
        // IndexedParallelIterator の collect は入力順を保つため、chunk 間も含めて
        // 呼び出し元が指定したパス順を維持できる。
        let render_opts = writer.render_options();
        let rendered: Vec<BatchRendered> =
            pool.install(|| chunk.par_iter().map(|p| process(p, render_opts)).collect());
        for record in rendered {
            writer.push(record)?;
        }
        // broken pipe 等を chunk 境界で検出し、残りの解析を早期に打ち切る。
        writer.flush()?;
    }

    if let Some(trailer) = trailer {
        let rendered = render_batch_record(&trailer, writer.render_options());
        writer.push(rendered)?;
    }

    writer.finish()
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

pub struct BatchSymbolsOpts<'a> {
    pub doc: bool,
    pub full: bool,
    pub dir: Option<&'a std::path::Path>,
    pub skipped: Option<SkippedFiles>,
    pub query: Option<&'a str>,
    pub output: OutputOptions,
}

pub fn batch_symbols(
    service: &AppService,
    paths: &[String],
    opts: BatchSymbolsOpts<'_>,
) -> Result<()> {
    let trailer = opts.skipped.map(|skipped| {
        let mut value = serde_json::json!({ "skipped": skipped });
        value.sort_all_objects();
        value
    });
    batch_ndjson_with_trailer(paths, trailer, opts.output, |p, output| {
        match service.extract_symbols_with_query(p, opts.query) {
            Ok(mut response) => {
                // dir 指定時に絶対パスを相対パスに変換
                if let Some(base) = opts.dir
                    && let Ok(rel) =
                        std::path::Path::new(&response.location.path).strip_prefix(base)
                {
                    response.location.path = rel.to_string_lossy().to_string();
                }
                if opts.full {
                    render_batch_record(&response, output)
                } else {
                    render_batch_record(&response.to_compact_symbols(opts.doc), output)
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
        assert_eq!(decide_batch_format(100, 100, 5, 10, 10), OutputFormat::Json);
        // json=100, toon=90 → ヘッダ 5 を足しても TOON が短い。
        assert_eq!(decide_batch_format(100, 90, 5, 10, 10), OutputFormat::Toon);
        // json=100, toon=96 → ヘッダ 5 でひっくり返って JSON。
        assert_eq!(decide_batch_format(100, 96, 5, 10, 10), OutputFormat::Json);

        // window が全体の一部なら、ヘッダは 1 回きりのコストとして薄まる。
        // 同じ window 実測値 (json=100, toon=96) でも、全体 1000 件なら TOON が勝つ。
        assert_eq!(
            decide_batch_format(100, 96, 11, 4, 1000),
            OutputFormat::Toon
        );
    }

    /// `auto` の合成レコード: 先頭 8 件は JSON が短く、以降は TOON が短い。
    /// 旧実装 (先頭 window = ワーカー数 × 8 件で判定) では window 8 だと JSON、
    /// window 16 以上だと TOON を選び、並列度で出力形式が変わっていた。
    fn skewed_record(path: &str, opts: OutputOptions) -> BatchRendered {
        let index: usize = path.parse().expect("numeric path");
        let (json_len, toon_len) = if index < 8 { (10, 30) } else { (100, 20) };
        let json = format!("{{\"i\":{index}}}{}", "j".repeat(json_len));
        let toon = format!("  - i: {index}{}", "t".repeat(toon_len));
        match opts.format() {
            OutputFormat::Json => BatchRendered::One(json),
            OutputFormat::Toon => BatchRendered::One(toon),
            OutputFormat::Auto => BatchRendered::Both { json, toon },
        }
    }

    /// `auto` の選択は入力内容だけで決まり、window の大きさ (= 並列度) に依存しない。
    #[test]
    fn auto_batch_format_does_not_depend_on_window_size() {
        let auto = OutputOptions::new(OutputFormat::Auto, JsonStyle::Compact);
        let paths = (0..40).map(|i| i.to_string()).collect::<Vec<_>>();
        let render = |window: usize| {
            let mut output = Vec::new();
            let bytes = batch_ndjson_to_windowed(&paths, auto, skewed_record, &mut output, window)
                .expect("batch should succeed");
            assert_eq!(bytes, output.len());
            String::from_utf8(output).expect("valid UTF-8")
        };

        let baseline = render(8);
        assert!(
            baseline.starts_with("[40]:"),
            "先頭 32 件の標本では TOON が短い: {baseline:.40}"
        );
        for window in [1, 3, 16, 32, 64] {
            assert_eq!(render(window), baseline, "window={window}");
        }
    }

    /// 標本が揃った時点で書き出しを始める = 保留は入力件数に比例しない。
    #[test]
    fn auto_batch_starts_writing_once_the_sample_is_complete() {
        use super::AUTO_SAMPLE_RECORDS;

        struct FirstWriteProbe<'a> {
            processed: &'a AtomicUsize,
            processed_at_first_write: Option<usize>,
        }
        impl Write for FirstWriteProbe<'_> {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.processed_at_first_write
                    .get_or_insert(self.processed.load(Ordering::SeqCst));
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let auto = OutputOptions::new(OutputFormat::Auto, JsonStyle::Compact);
        let window = 4;
        let paths = (0..400).map(|i| i.to_string()).collect::<Vec<_>>();
        let processed = AtomicUsize::new(0);
        let mut probe = FirstWriteProbe {
            processed: &processed,
            processed_at_first_write: None,
        };
        batch_ndjson_to_windowed(
            &paths,
            auto,
            |path, opts| {
                processed.fetch_add(1, Ordering::SeqCst);
                skewed_record(path, opts)
            },
            &mut probe,
            window,
        )
        .expect("batch should succeed");

        let first = probe
            .processed_at_first_write
            .expect("output should be written");
        assert!(
            first <= AUTO_SAMPLE_RECORDS + window,
            "標本 {AUTO_SAMPLE_RECORDS} 件 + window {window} 件を超えて保留した: {first}"
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
            "[3]:\n  - p: f0.rs\n  - p: f1.rs\n  - p: f2.rs"
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
