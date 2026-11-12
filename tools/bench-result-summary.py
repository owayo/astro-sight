"""省略サマリ集計の旧新版を交互に測り、出力一致・時間・ピークメモリを保存する。"""

import argparse
import csv
import hashlib
import json
import platform
import runpy
import statistics
from pathlib import Path


def main():
    """同じ入力で旧新版を比較し、全試行の結果を保存する。

    Raises:
        RuntimeError: 集計結果が旧新版または試行間で一致しない場合。

    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output-directory", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=9)
    options = parser.parse_args()
    if options.repeats < 3:
        parser.error("--repeats must be at least 3")
    measured = runpy.run_path(str(Path(__file__).with_name("bench-refs.py")))[
        "measured"
    ]
    variants = {
        "before": options.baseline.resolve(strict=True),
        "after": options.candidate.resolve(strict=True),
    }
    output = options.output_directory.resolve()
    output.mkdir(parents=True, exist_ok=True)
    expected, rows = None, []
    for iteration in range(options.repeats + 1):
        order = ("before", "after") if iteration % 2 else ("after", "before")
        for variant in order:
            stdout, _, peak = measured(variants[variant], [])
            results = [json.loads(line) for line in stdout.splitlines()]
            actual = {row["case"]: row["result"] for row in results}
            if expected is None:
                expected = actual
            if actual != expected:
                raise RuntimeError(f"output mismatch: {variant} / {iteration}")
            if iteration == 0:
                continue
            for result in results:
                rows.append(
                    {
                        "case": result["case"],
                        "variant": variant,
                        "iteration": iteration,
                        "ms": result["ms"] / result["iterations"],
                        "peak_working_set_bytes": peak,
                    }
                )
    summary = []
    for case in expected:
        stats = {}
        for variant in variants:
            samples = [
                row for row in rows if row["case"] == case and row["variant"] == variant
            ]
            times = [row["ms"] for row in samples]
            memory = [
                row["peak_working_set_bytes"]
                for row in samples
                if row["peak_working_set_bytes"] is not None
            ]
            stats[variant] = {
                "median_ms": statistics.median(times),
                "min_ms": min(times),
                "max_ms": max(times),
                "max_working_set_bytes": max(memory) if memory else None,
            }
        improvement = 100 * (
            1 - stats["after"]["median_ms"] / stats["before"]["median_ms"]
        )
        summary.append(dict(case=case, **stats, improvement_percent=improvement))
        print(
            f"{case}: {stats['before']['median_ms']:.4f} -> "
            f"{stats['after']['median_ms']:.4f} ms ({improvement:.1f}%)",
            flush=True,
        )
    with (output / "samples.csv").open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=list(rows[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)
    report = {
        "repeats": options.repeats,
        "os": platform.platform(),
        "python": platform.python_version(),
        "memory_scope": "whole benchmark process (all cases)",
        "cases": summary,
        "executable_sha256": {
            name: hashlib.sha256(path.read_bytes()).hexdigest()
            for name, path in variants.items()
        },
    }
    for filename, value in [("summary.json", report), ("results.json", expected)]:
        (output / filename).write_text(
            json.dumps(value, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
            newline="\n",
        )


if __name__ == "__main__":
    main()
