"""同じ入力で複数の実行条件を交互に流し、時間・ピーク working set・出力の一致を比べる。

使い方:
  python -I bench_variants.py <設定 JSON> <結果 CSV>

設定 JSON:
  {"cwd": "...", "rounds": 9, "warmup": 1,
   "variants": [{"label": "w1", "exe": "...", "args": [...], "env": {"K": "V"}}, ...]}

各ラウンドで全条件を 1 回ずつ実行し、ラウンドごとに順序を反転する (ABBA)。
条件ごとの stdout・stderr・終了コードは全試行で一致しなければならない。
"""

import csv
import ctypes
import json
import os
import statistics
import subprocess
import sys
import time
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


KERNEL = ctypes.WinDLL("kernel32", use_last_error=True)
PSAPI = ctypes.WinDLL("psapi", use_last_error=True)
KERNEL.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
KERNEL.OpenProcess.restype = wintypes.HANDLE
KERNEL.CloseHandle.argtypes = [wintypes.HANDLE]
PSAPI.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]


def run_once(variant, cwd):
    env = dict(os.environ)
    for key in variant.get("unset", []):
        env.pop(key, None)
    env.update(variant.get("env", {}))
    started = time.perf_counter()
    with subprocess.Popen(
        [variant["exe"], *variant["args"]],
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ) as process:
        handle = KERNEL.OpenProcess(0x0410, False, process.pid)
        out, err = process.communicate()
        elapsed = time.perf_counter() - started
        counters = Counters()
        counters.cb = ctypes.sizeof(counters)
        peak = None
        if handle and PSAPI.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
            peak = counters.peak_working_set
        if handle:
            KERNEL.CloseHandle(handle)
        return elapsed, peak, (process.returncode, out, err)


def main() -> int:
    config = json.loads(open(sys.argv[1], encoding="utf-8").read())
    cwd = config["cwd"]
    variants = config["variants"]
    rounds = config.get("rounds", 9)
    for _ in range(config.get("warmup", 1)):
        for variant in variants:
            run_once(variant, cwd)
    samples = []
    reference = {}
    for round_index in range(rounds):
        order = variants if round_index % 2 == 0 else list(reversed(variants))
        for variant in order:
            elapsed, peak, result = run_once(variant, cwd)
            key = variant["label"]
            reference.setdefault(key, result)
            if reference[key] != result:
                raise SystemExit(f"{key}: output changed between runs")
            samples.append((round_index, key, elapsed, peak))
    labels = [variant["label"] for variant in variants]
    first = reference[labels[0]]
    for label in labels[1:]:
        status = "same" if reference[label] == first else "DIFFERENT"
        print(f"output {label} vs {labels[0]}: {status}")
    with open(sys.argv[2], "w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle)
        writer.writerow(["round", "label", "seconds", "peak_working_set_bytes"])
        writer.writerows(samples)
    for label in labels:
        times = [s[2] for s in samples if s[1] == label]
        peaks = [s[3] for s in samples if s[1] == label and s[3] is not None]
        print(
            f"{label}: median {statistics.median(times):.3f}s "
            f"min {min(times):.3f}s max {max(times):.3f}s "
            f"peak WS median {statistics.median(peaks) / 1048576:.1f} MiB"
            f" (max {max(peaks) / 1048576:.1f})"
        )
    print(f"exit codes: {[reference[label][0] for label in labels]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
