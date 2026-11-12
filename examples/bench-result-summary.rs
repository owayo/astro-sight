//! 省略サマリの集計だけを測る。パース・ファイル探索・描画の時間は含めない。

use std::hint::black_box;
use std::time::Instant;

use astro_sight::models::result_summary::{RollupRecord, build_rollup};

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct Record<'a> {
    path: &'a str,
    kind: Option<&'static str>,
}

impl RollupRecord for Record<'_> {
    fn rollup_path(&self) -> &str {
        self.path
    }

    fn rollup_kind(&self) -> Option<&'static str> {
        self.kind
    }
}

fn main() {
    for (name, files, records, iterations) in [
        ("small", 4, 100, 10_000),
        ("repeated", 16, 100_000, 30),
        ("many_files", 10_000, 100_000, 10),
        ("unique_files", 10_000, 10_000, 30),
        ("empty", 0, 0, 10_000),
    ] {
        let paths: Vec<String> = (0..files)
            .map(|i| {
                let extension = ["rs", "ts", "php", "unknown"][i % 4];
                format!("src/dir{:04}/file{i:05}.{extension}", i % 127)
            })
            .collect();
        let input: Vec<Record<'_>> = (0..records)
            .map(|i| Record {
                path: &paths[i % files],
                kind: [Some("ref"), Some("def"), None][i % 3],
            })
            .collect();
        let expected = build_rollup(&input, Some(3_000));
        let start = Instant::now();
        for _ in 0..iterations {
            black_box(build_rollup(black_box(&input), Some(3_000)));
        }
        let elapsed = start.elapsed().as_secs_f64() * 1_000.0;
        // 集計結果の全フィールドを比較できるよう、計測の外で JSON にする。
        println!(
            "{}",
            serde_json::json!({
                "case": name,
                "iterations": iterations,
                "ms": elapsed,
                "result": {
                    "by_kind": expected.by_kind,
                    "by_lang": expected.by_lang,
                    "files": expected.files,
                    "other_files": expected.other_files,
                    "rollup_truncated": expected.rollup_truncated,
                },
            })
        );
    }
}
