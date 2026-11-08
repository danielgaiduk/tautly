#!/usr/bin/env python3
"""Median time from spawning `tautly -p` to its first event (the first stdout line), per
binary, as spec 9.1 measures cold start. Runs alternate between the binaries given, so
they share the machine's conditions. Prints one median in seconds per binary.

usage: first_event.py SCRIPT RUNS BIN [BIN...]
"""
import statistics
import subprocess
import sys
import time


def first_event(binary: str, script: str) -> float:
    t = time.perf_counter()
    p = subprocess.Popen(
        [binary, "-p", "hi", "--mock-script", script, "--output", "json"],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    line = p.stdout.readline()
    took = time.perf_counter() - t
    p.stdout.read()
    p.wait()
    if not line:
        sys.exit(f"{binary} wrote no event")
    return took


def main() -> None:
    script, runs, binaries = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
    for b in binaries:
        for _ in range(5):
            first_event(b, script)
    samples = {b: [] for b in binaries}
    for _ in range(runs):
        for b in binaries:
            samples[b].append(first_event(b, script))
    print(" ".join(str(statistics.median(samples[b])) for b in binaries))


main()
