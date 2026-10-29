"""旧新 CLI の参照検索を交互に測定し、全出力の一致と生データを保存する。"""

import argparse
import csv
import ctypes
import hashlib
import json
import os
import platform
import statistics
import subprocess
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack, contextmanager
from functools import partial
from pathlib import Path
from tempfile import TemporaryFile


def process_memory_reader(pid):
    """Windows のプロセス終了後にもピーク working set を取得できるハンドルを持つ。"""
    if os.name != "nt":
        return lambda: None
    from ctypes import wintypes

    class Counters(ctypes.Structure):
        _fields_ = [("cb", wintypes.DWORD), ("faults", wintypes.DWORD)] + [
            (name, ctypes.c_size_t)
            for name in (
                "peak_working_set",
                "working_set",
                "peak_paged",
                "paged",
                "peak_nonpaged",
                "nonpaged",
                "pagefile",
                "peak_pagefile",
            )
        ]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    psapi.GetProcessMemoryInfo.argtypes = [
        wintypes.HANDLE,
        ctypes.POINTER(Counters),
        wintypes.DWORD,
    ]
    handle = kernel.OpenProcess(0x0410, False, pid)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())

    def read_and_close():
        try:
            counters = Counters()
            counters.cb = ctypes.sizeof(counters)
            if not psapi.GetProcessMemoryInfo(
                handle, ctypes.byref(counters), counters.cb
            ):
                raise ctypes.WinError(ctypes.get_last_error())
            return counters.peak_working_set
        finally:
            kernel.CloseHandle(handle)

    return read_and_close


def measured(executable, arguments):
    started = time.perf_counter()
    with subprocess.Popen(
        [str(executable), *map(str, arguments)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ) as process:
        try:
            read_memory = process_memory_reader(process.pid)
        except BaseException:
            process.kill()
            process.communicate()
            raise
        try:
            stdout, stderr = process.communicate(timeout=120)
            elapsed = (time.perf_counter() - started) * 1000
        except BaseException:
            process.kill()
            process.communicate()
            raise
        finally:
            peak = read_memory()
        if process.returncode:
            raise RuntimeError(
                f"exit {process.returncode}: {stderr.decode(errors='replace')}"
            )
    return stdout, elapsed, peak


def cases(input_root):
    """深さ・出現数だけが異なる入力を保存し、通常入力と負の対照も測る。"""
    result = []

    def add(name, source, symbol):
        file = f"{name}.rs"
        (input_root / file).write_bytes(source.encode())
        result.append(
            (
                name,
                [
                    "refs",
                    "--dir",
                    input_root,
                    "--glob",
                    file,
                    "--name",
                    symbol,
                    "--format",
                    "json",
                ],
            )
        )

    for depth in (500, 1000, 2000):
        add(
            f"deep-{depth}",
            "use crate::target::" + "nested::" * depth + "leaf;\npub fn target() {}\n",
            "target",
        )
    for depth in (250, 500):
        add(f"many-{depth}", "use crate::" + "nested::" * depth + "leaf;\n", "nested")
    add(
        "leaf-control",
        "use crate::" + "nested::" * 2000 + "target;\npub fn target() {}\n",
        "target",
    )
    result.append(
        (
            "absent-control",
            [
                "refs",
                "--dir",
                input_root,
                "--glob",
                "deep-2000.rs",
                "--name",
                "absent",
                "--format",
                "json",
            ],
        )
    )
    add(
        "ordinary",
        "pub fn target() {}\n"
        + "\n".join(
            f"pub fn f{i}() {{ target(); ordinary!(target); }}" for i in range(500)
        )
        + "\n",
        "target",
    )
    fixtures = Path(__file__).resolve().parents[1] / "tests" / "fixtures"
    result.append(
        (
            "mixed-languages",
            [
                "refs",
                "--dir",
                fixtures,
                "--names",
                "main,greet,add",
                "--format",
                "json",
            ],
        )
    )
    return result


@contextmanager
def session_measurements(executable):
    """起動をウォームアップへ含め、同じ session 内の要求時間を測る。"""
    with (
        TemporaryFile() as errors,
        subprocess.Popen(
            [str(executable), "session"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=errors,
        ) as process,
    ):
        executor = ThreadPoolExecutor(max_workers=1)

        def run(arguments):
            args = list(map(str, arguments))
            directory = args[args.index("--dir") + 1]
            glob = args[args.index("--glob") + 1] if "--glob" in args else None
            names = (
                args[args.index("--names") + 1].split(",")
                if "--names" in args
                else [args[args.index("--name") + 1]]
            )
            output = []
            started = time.perf_counter()
            for name in names:
                request = {"command": "refs", "dir": directory, "name": name}
                if glob is not None:
                    request["glob"] = glob
                process.stdin.write(json.dumps(request).encode() + b"\n")
                process.stdin.flush()
                line = executor.submit(process.stdout.readline).result(timeout=120)
                if not line or "error" in json.loads(line):
                    raise RuntimeError(f"session request failed: {line!r}")
                output.append(line)
            elapsed = (time.perf_counter() - started) * 1000
            return b"".join(output), elapsed, process_memory_reader(process.pid)()

        try:
            yield run
        finally:
            process.stdin.close()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            executor.shutdown(wait=True)


@contextmanager
def measurement_pair(variants, mode):
    with ExitStack() as stack:
        yield {
            variant: (
                partial(measured, executable)
                if mode == "cli"
                else stack.enter_context(session_measurements(executable))
            )
            for variant, executable in variants.items()
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output-directory", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=9)
    parser.add_argument("--mode", choices=("cli", "session"), default="cli")
    options = parser.parse_args()
    if options.repeats < 3:
        parser.error("--repeats must be at least 3")
    output = options.output_directory.resolve()
    inputs = output / "inputs"
    inputs.mkdir(parents=True, exist_ok=True)
    variants = {
        "before": options.baseline.resolve(strict=True),
        "after": options.candidate.resolve(strict=True),
    }
    rows, summary = [], []
    for name, arguments in cases(inputs):
        with measurement_pair(variants, options.mode) as runs:
            expected, _, _ = runs["before"](arguments)
            actual, _, _ = runs["after"](arguments)
            if expected != actual:
                raise RuntimeError(f"output mismatch: {name}")
            (output / f"{name}.json").write_bytes(expected)
            # ウォームアップ後は実行順を交互に反転する。
            for iteration in range(1, options.repeats + 1):
                order = ("before", "after") if iteration % 2 else ("after", "before")
                for variant in order:
                    actual, elapsed, peak = runs[variant](arguments)
                    if actual != expected:
                        raise RuntimeError(f"unstable output: {name} / {variant}")
                    rows.append(
                        {
                            "case": name,
                            "variant": variant,
                            "iteration": iteration,
                            "ms": elapsed,
                            "peak_working_set_bytes": peak,
                        }
                    )
            stats = {}
            for variant in variants:
                samples = [
                    row
                    for row in rows
                    if row["case"] == name and row["variant"] == variant
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
            summary.append(dict(case=name, **stats, improvement_percent=improvement))
            print(
                f"{name}: {stats['before']['median_ms']:.2f} -> "
                f"{stats['after']['median_ms']:.2f} ms ({improvement:.1f}%)",
                flush=True,
            )
    with (output / "samples.csv").open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    report = {
        "repeats": options.repeats,
        "mode": options.mode,
        "os": platform.platform(),
        "processors": os.cpu_count(),
        "python": platform.python_version(),
        "cases": summary,
        "executable_sha256": {
            name: hashlib.sha256(path.read_bytes()).hexdigest()
            for name, path in variants.items()
        },
    }
    (output / "summary.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8"
    )


if __name__ == "__main__":
    main()
