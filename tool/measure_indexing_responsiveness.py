#!/usr/bin/env python3
"""Compare the same opt-in public-API fixture on two checked-out revisions."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import statistics
import subprocess


def build(root, label, output, env):
    build_environment = env | {"CARGO_TARGET_DIR": str(output / "target" / label)}
    completed = subprocess.run(
        ["cargo", "test", "--manifest-path", str(root / "rust/Cargo.toml"),
         "--locked", "--release", "--test", "indexing_throughput", "--no-run",
         "--message-format=json"],
        check=True, text=True, stdout=subprocess.PIPE, env=build_environment,
    )
    executables = []
    for line in completed.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("executable"):
            if message["target"]["name"] == "indexing_throughput":
                executables.append(message["executable"])
    if len(executables) != 1:
        raise RuntimeError(f"expected one benchmark executable, found {executables}")
    binary = output / label
    shutil.copy2(executables[0], binary)
    return binary


def binary_sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as binary:
        for chunk in iter(lambda: binary.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--head", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=2)
    args = parser.parse_args()
    if args.repeats < 2:
        parser.error("at least two repetitions are required")
    args.base, args.head, args.out = (p.resolve() for p in (args.base, args.head, args.out))
    args.out.mkdir(parents=True, exist_ok=True)
    shutil.copy2(args.head / "rust/tests/indexing_throughput.rs", args.base / "rust/tests/indexing_throughput.rs")
    cpus = sorted(os.sched_getaffinity(0))
    environment = os.environ | {
        "CARGO_BUILD_JOBS": "1", "OTZARIA_BENCH_LINES": "12000",
    }
    environment.pop("OTZARIA_BENCH_ECONOMY", None)
    binaries = {label: build(path, label, args.out, environment)
                for label, path in [("before", args.base), ("after", args.head)]}
    metadata = {
        "platform": platform.platform(), "affinity": cpus,
        "base": subprocess.check_output(["git", "-C", str(args.base), "rev-parse", "HEAD"], text=True).strip(),
        "head": subprocess.check_output(["git", "-C", str(args.head), "rev-parse", "HEAD"], text=True).strip(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "fixture_lines_per_book": int(environment["OTZARIA_BENCH_LINES"]),
        "binary_sha256": {label: binary_sha256(binary) for label, binary in binaries.items()},
        "notes": ["Synthetic, local SQLite fixture; not a full library or slow-storage experiment.",
                  "Repeated exact search with warm caches, not cold-query latency.",
                  "CPU affinity is a constrained-core scenario, not a different CPU or RAM device.",
                  "No Flutter UI frames are measured; no performance thresholds fail this job.",
                  "Baseline and head run an identical fixture. Additional diagnostic counters only exist on the head."],
    }
    (args.out / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    cases = [("two-core-normal", cpus[:2], False),
             ("available-cores-normal", cpus, False),
             ("two-core-economy", cpus[:2], True)]
    records = []
    with (args.out / "samples.jsonl").open("w") as samples:
        for case, affinity, economy in cases:
            expected_stamps = None
            for repeat in range(args.repeats):
                for label in (["before", "after"] if repeat % 2 == 0 else ["after", "before"]):
                    env = environment.copy()
                    if economy:
                        env["OTZARIA_BENCH_ECONOMY"] = "1"
                    completed = subprocess.run(
                        ["taskset", "-c", ",".join(map(str, affinity)), str(binaries[label]),
                         "measure_search_during_indexing", "--ignored", "--exact", "--nocapture", "--test-threads=1"],
                        check=True, text=True, stdout=subprocess.PIPE, env=env, timeout=180,
                    )
                    lines = [line.partition("OTZARIA_RESPONSIVENESS ")[2]
                             for line in completed.stdout.splitlines() if "OTZARIA_RESPONSIVENESS " in line]
                    if len(lines) != 1:
                        raise RuntimeError(f"expected one JSON sample: {completed.stdout}")
                    result = json.loads(lines[0]) | {"case": case, "revision": label, "repeat": repeat, "affinity": affinity}
                    diagnostic_fields = ("active_preparations", "peak_active_preparations", "reserved_bytes", "peak_reserved_bytes")
                    for key in diagnostic_fields:
                        match = re.search(rf"\b{key}: (\d+)", result["diagnostics"])
                        result[key] = int(match[1]) if match else None
                    if label == "after" and any(result[key] is None for key in diagnostic_fields):
                        raise RuntimeError("head binary is missing indexing diagnostics; refusing a stale-build comparison")
                    if result["search_samples"] == 0:
                        raise RuntimeError("no search overlapped indexing")
                    if expected_stamps is None:
                        expected_stamps = result["stamps"]
                    elif result["stamps"] != expected_stamps:
                        raise RuntimeError(f"document fingerprints changed in {case}/{label}")
                    records.append(result)
                    samples.write(json.dumps(result) + "\n")
                    samples.flush()
                    print(json.dumps({key: value for key, value in result.items() if key != "stamps"}), flush=True)
    medians = []
    metrics = ["search_during_p95_ms", "search_during_p99_ms", "docs_per_sec", "peak_rss_kib", "peak_buffered_bytes", "spilled_bytes", "peak_active_preparations", "peak_reserved_bytes"]
    for case, _, _ in cases:
        for label in binaries:
            selected = [r for r in records if r["case"] == case and r["revision"] == label]
            medians.append({"case": case, "revision": label} | {
                key: statistics.median(r[key] for r in selected) if all(r[key] is not None for r in selected) else None
                for key in metrics
            })
    (args.out / "medians.json").write_text(json.dumps(medians, indent=2) + "\n")


if __name__ == "__main__":
    main()
