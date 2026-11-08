#!/usr/bin/env python3
"""Unmodified economy fixture, balanced binary order, and same-binary controls.

One fresh release build per immutable revision. No timing thresholds, cooldowns,
cache drops, fixture instrumentation, or builds during the measurement schedule.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import resource
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import traceback


REVISIONS = {
    "B": "8ef29fd592793fbc9d5af0299fdd7a8607789bac",
    "H": "4761356749815d283ede41ef956c64b3dbffe048",
}
FIXTURE = "rust/tests/indexing_throughput.rs"
FIXTURE_SHA256 = "58b15ac4a0736284bae336b48f6d334bc1de8bcfd3ac52689ab0417ec264ebca"
LAYOUT_TARGET = "__otzaria_economy_layout"
LAYOUT_SOURCE = '''use search_engine::api::search_engine::SearchEngine;

#[test]
fn layout_metadata() {
    println!(
        "OTZARIA_LAYOUT {{\\"size_bytes\\":{},\\"alignment_bytes\\":{}}}",
        std::mem::size_of::<SearchEngine>(),
        std::mem::align_of::<SearchEngine>()
    );
}

fn main() {}
'''
METRICS = (
    "search_idle_p95_ms", "search_during_p50_ms", "search_during_p95_ms",
    "search_during_p99_ms", "search_samples", "add_ms", "total_ms",
    "docs_per_sec", "rss_before_kib", "peak_rss_kib", "peak_buffered_bytes",
    "spilled_bytes", "peak_active_preparations", "peak_reserved_bytes",
)


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def json_write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def text_command(command, env=None):
    return subprocess.check_output(command, text=True, env=env).strip()


def git(root, *args):
    return text_command(["git", "-C", str(root), *args])


def schedule():
    blocks = [
        ("R1", "real", "BHHB", None),
        ("CB1", "control", "BHHB", "B"),
        ("R2", "real", "HBBH", None),
        ("CH1", "control", "HBBH", "H"),
        ("R3", "real", "HBBH", None),
        ("CB2", "control", "HBBH", "B"),
        ("R4", "real", "BHHB", None),
        ("CH2", "control", "BHHB", "H"),
    ]
    return [
        {"run": block_number * 4 + position, "block": name, "kind": kind,
         "order": order, "position": position, "pseudo_label": label,
         "actual_binary": actual or label}
        for block_number, (name, kind, order, actual) in enumerate(blocks)
        for position, label in enumerate(order, 1)
    ]


def validate_schedule(rows):
    assert len(rows) == 32
    assert [row["run"] for row in rows] == list(range(1, 33))
    real = [row for row in rows if row["kind"] == "real"]
    assert len(real) == 16
    for label in "BH":
        assert sum(row["actual_binary"] == label for row in real) == 8
        for position in range(1, 5):
            assert sum(row["actual_binary"] == label and row["position"] == position
                       for row in real) == 2
    for name in dict.fromkeys(row["block"] for row in rows):
        block = [row for row in rows if row["block"] == name]
        assert len(block) == 4
        assert "".join(row["pseudo_label"] for row in block) == block[0]["order"]
        if block[0]["kind"] == "control":
            assert len({row["actual_binary"] for row in block}) == 1
            if "binary_sha256" in block[0]:
                assert len({row["binary_sha256"] for row in block}) == 1
        else:
            assert all(row["actual_binary"] == row["pseudo_label"] for row in block)
    for actual in "BH":
        controls = [row for row in rows if row["kind"] == "control"
                    and row["actual_binary"] == actual and row["position"] == 1]
        assert sorted(row["order"] for row in controls) == ["BHHB", "HBBH"]


def snapshot():
    result = {"unix_time_ns": time.time_ns(), "monotonic_ns": time.monotonic_ns()}
    for name in ("cpu", "io", "memory"):
        path = Path("/proc/pressure") / name
        result[f"pressure_{name}"] = path.read_text() if path.exists() else None
    meminfo = {}
    for line in Path("/proc/meminfo").read_text().splitlines():
        key, _, value = line.partition(":")
        if key in {"MemAvailable", "Dirty", "Writeback", "WritebackTmp", "Cached"}:
            meminfo[key] = value.strip()
    result["meminfo"] = meminfo
    result["loadavg"] = Path("/proc/loadavg").read_text().strip()
    return result


def source_provenance(root, label):
    commit = git(root, "rev-parse", "HEAD")
    if commit != REVISIONS[label]:
        raise RuntimeError(f"{label} source is {commit}, expected {REVISIONS[label]}")
    if git(root, "status", "--porcelain=v1", "--untracked-files=no"):
        raise RuntimeError(f"{label} tracked source was modified")
    hashes = {path: sha256(root / path) for path in
              (FIXTURE, "rust/Cargo.toml", "rust/Cargo.lock")}
    if hashes[FIXTURE] != FIXTURE_SHA256:
        raise RuntimeError(f"{label} does not contain the immutable original fixture")
    return {"commit": commit, "tree": git(root, "rev-parse", "HEAD^{tree}"),
            "tracked_source_clean": True, "files_sha256": hashes}


def dependency_graph(root, env):
    raw = json.loads(text_command([
        "cargo", "metadata", "--manifest-path", str(root / "rust/Cargo.toml"),
        "--locked", "--format-version=1",
    ], env))
    nodes = [{"id": node["id"], "features": sorted(node["features"]),
              "deps": sorted(node["deps"], key=lambda dep: (dep["name"], dep["pkg"]))}
             for node in raw["resolve"]["nodes"]]
    return sorted(nodes, key=lambda node: node["id"])


def extract_marker(log, marker):
    values = [line.partition(marker)[2] for line in log.read_text().splitlines()
              if marker in line]
    if len(values) != 1:
        raise RuntimeError(f"expected one {marker.strip()} record in {log}")
    return json.loads(values[0])


def build(root, label, output, build_root, environment):
    directory = output / "build-logs" / label
    directory.mkdir(parents=True)
    env = environment | {"CARGO_TARGET_DIR": str(build_root / label)}
    before = source_provenance(root, label)
    graph_before = dependency_graph(root, env)
    json_write(directory / "dependencies-before.json", graph_before)
    helper = root / "rust/examples" / f"{LAYOUT_TARGET}.rs"
    if helper.exists():
        raise RuntimeError(f"refusing to overwrite {helper}")
    helper.parent.mkdir(parents=True, exist_ok=True)
    helper.write_text(LAYOUT_SOURCE)
    try:
        graph_after = dependency_graph(root, env)
        json_write(directory / "dependencies-with-layout-target.json", graph_after)
        if graph_before != graph_after:
            raise RuntimeError("metadata-only example changed resolved dependencies/features")
        command = [
            "cargo", "test", "--manifest-path", str(root / "rust/Cargo.toml"),
            "--locked", "--release", "--test", "indexing_throughput",
            "--example", LAYOUT_TARGET, "--no-run", "--message-format=json",
        ]
        json_write(directory / "command.json", command)
        started = time.monotonic()
        print(f"Building immutable {label}: {REVISIONS[label]}", flush=True)
        with (directory / "cargo.jsonl").open("w") as stdout, \
                (directory / "cargo.stderr.log").open("w") as stderr:
            completed = subprocess.run(command, stdout=stdout, stderr=stderr,
                                       env=env, timeout=1200, check=False)
        json_write(directory / "duration.json", {
            "wall_seconds": time.monotonic() - started,
            "returncode": completed.returncode,
        })
        completed.check_returncode()
        selected = {}
        for line in (directory / "cargo.jsonl").read_text().splitlines():
            message = json.loads(line)
            if message.get("reason") != "compiler-artifact" or not message.get("executable"):
                continue
            name = message["target"]["name"]
            if name in {"indexing_throughput", LAYOUT_TARGET}:
                if name in selected:
                    raise RuntimeError(f"duplicate executable for exact target {name}")
                selected[name] = message
        if set(selected) != {"indexing_throughput", LAYOUT_TARGET}:
            raise RuntimeError(f"missing exact target executable: {list(selected)}")
        json_write(directory / "selected-artifacts.json", selected)
        binary = output / "binaries" / f"{label}-indexing-throughput"
        binary.parent.mkdir(exist_ok=True)
        shutil.copy2(selected["indexing_throughput"]["executable"], binary)
        layout_executable = Path(selected[LAYOUT_TARGET]["executable"])
        with (directory / "layout.log").open("w") as log:
            subprocess.run([str(layout_executable), "layout_metadata", "--exact",
                            "--nocapture", "--test-threads=1"],
                           stdout=log, stderr=subprocess.STDOUT, env=env,
                           timeout=60, check=True)
        layout = extract_marker(directory / "layout.log", "OTZARIA_LAYOUT ")
        if not all(isinstance(layout.get(key), int) and layout[key] > 0
                   for key in ("size_bytes", "alignment_bytes")):
            raise RuntimeError(f"invalid SearchEngine layout metadata: {layout}")
        record = before | {
            "binary_sha256": sha256(binary),
            "layout": layout, "layout_executable_sha256": sha256(layout_executable),
            "resolved_graph_sha256": sha256(directory / "dependencies-before.json"),
            "build_wall_seconds": time.monotonic() - started,
        }
        json_write(directory / "provenance.json", record)
    finally:
        helper.unlink()
    if source_provenance(root, label) != before:
        raise RuntimeError(f"{label} source provenance changed during build")
    return binary, record


def medians(rows):
    return {key: statistics.median(row["sample"][key] for row in rows)
            if rows and all(isinstance(row["sample"].get(key), (float, int))
                            for row in rows) else None for key in METRICS}


def ratio(numerator, denominator):
    return numerator / denominator if numerator is not None and denominator else None


def analyse(records):
    blocks = []
    for name in dict.fromkeys(row["block"] for row in records):
        rows = [row for row in records if row["block"] == name]
        if len(rows) != 4:
            continue
        labels = {label: medians([row for row in rows if row["pseudo_label"] == label])
                  for label in "BH"}
        middle = medians([row for row in rows if row["position"] in (2, 3)])
        edges = medians([row for row in rows if row["position"] in (1, 4)])
        blocks.append({
            "block": name, "kind": rows[0]["kind"], "order": rows[0]["order"],
            "actual_binaries": sorted({row["actual_binary"] for row in rows}),
            "binary_hashes": sorted({row["binary_sha256"] for row in rows}),
            "pseudo_label_medians": labels,
            "pseudo_H_over_B": {key: ratio(labels["H"][key], labels["B"][key])
                                for key in METRICS},
            "middle_over_edges": {key: ratio(middle[key], edges[key]) for key in METRICS},
        })
    real = [row for row in records if row["kind"] == "real"]
    return {
        "completed_processes": len(records), "blocks": blocks,
        "real_binary_medians": {
            label: medians([row for row in real if row["actual_binary"] == label])
            for label in "BH"},
        "real_order_groups": {
            order: {
                "block_p95_H_over_B": [block["pseudo_H_over_B"]["search_during_p95_ms"]
                                      for block in blocks
                                      if block["kind"] == "real" and block["order"] == order],
                "binary_medians": {
                    label: medians([row for row in real if row["actual_binary"] == label
                                    and row["order"] == order]) for label in "BH"},
            } for order in ("BHHB", "HBBH")},
        "interpretation": [
            "Each independent observation is one fresh process, not an individual query.",
            "Ratios compare medians of process-level percentiles; queries are not pooled.",
            "Real blocks compare actual H/B. Control pseudo-label contrasts use one binary.",
            "Inspect both real orders and same-binary middle/edge effects before attribution.",
            "No performance threshold or automatic causal conclusion is applied.",
        ],
    }


def validate_sample(sample, expected_stamps):
    if sample.get("economy") is not True or sample.get("notes") is not False:
        raise RuntimeError("benchmark ran an unexpected workload")
    if sample.get("cpus") != 2 or sample.get("indexed_docs") != 96000:
        raise RuntimeError("benchmark did not use the declared two-core fixture")
    if not isinstance(sample.get("search_samples"), int) or sample["search_samples"] < 1:
        raise RuntimeError("no query overlapped indexing")
    for key in ("search_idle_p95_ms", "search_during_p50_ms", "search_during_p95_ms",
                "search_during_p99_ms", "add_ms", "total_ms", "docs_per_sec"):
        if not isinstance(sample.get(key), (float, int)) or not math.isfinite(sample[key]) \
                or sample[key] <= 0:
            raise RuntimeError(f"missing or invalid metric: {key}")
    if not isinstance(sample.get("stamps"), list) or len(sample["stamps"]) != 8:
        raise RuntimeError("missing indexed document fingerprints")
    if expected_stamps is not None and sample["stamps"] != expected_stamps:
        raise RuntimeError("document fingerprints changed across processes/binaries")
    for key in ("active_preparations", "peak_active_preparations", "reserved_bytes",
                "peak_reserved_bytes"):
        match = re.search(rf"\b{key}: (\d+)", sample.get("diagnostics", ""))
        if not match:
            raise RuntimeError(f"missing expected indexing diagnostic: {key}")
        sample[key] = int(match[1])


def measure(entry, binary, output, affinity, environment, expected_stamps):
    stem = f"{entry['run']:02d}-{entry['block']}-{entry['position']}"
    command = ["taskset", "-c", ",".join(map(str, affinity)), str(binary),
               "measure_search_during_indexing", "--ignored", "--exact", "--nocapture",
               "--test-threads=1"]
    run_directory = output / "runs"
    run_directory.mkdir(exist_ok=True)
    context = {"schedule": entry, "command": command, "before": snapshot()}
    json_write(run_directory / f"{stem}.context.json", context)
    usage_before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    completed = None
    try:
        with (run_directory / f"{stem}.stdout.log").open("w") as log:
            completed = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT,
                                       env=environment, timeout=180, check=False)
    finally:
        usage_after = resource.getrusage(resource.RUSAGE_CHILDREN)
        context["process_wall_seconds"] = time.monotonic() - started
        context["whole_process_cpu_user_seconds"] = usage_after.ru_utime - usage_before.ru_utime
        context["whole_process_cpu_system_seconds"] = usage_after.ru_stime - usage_before.ru_stime
        context["cpu_scope"] = "Whole fixture process, including setup, concurrent indexer/search, commit and cleanup."
        context["returncode"] = completed.returncode if completed is not None else None
        context["after"] = snapshot()
        json_write(run_directory / f"{stem}.context.json", context)
    completed.check_returncode()
    sample = extract_marker(run_directory / f"{stem}.stdout.log", "OTZARIA_RESPONSIVENESS ")
    validate_sample(sample, expected_stamps)
    return entry | {"sample": sample, "process": context}


def self_test():
    rows = schedule()
    for row in rows:
        row["binary_sha256"] = row["actual_binary"] * 64
    validate_schedule(rows)
    records = [row | {"sample": {key: (2 if row["actual_binary"] == "H" else 1)
                                  for key in METRICS}} for row in rows]
    report = analyse(records)
    assert report["completed_processes"] == 32
    for block in report["blocks"]:
        expected = 2 if block["kind"] == "real" else 1
        assert block["pseudo_H_over_B"]["search_during_p95_ms"] == expected
        if block["kind"] == "control":
            assert block["middle_over_edges"]["search_during_p95_ms"] == 1
    broken = [dict(row) for row in rows]
    broken[4]["binary_sha256"] = "different"
    try:
        validate_schedule(broken)
    except AssertionError:
        pass
    else:
        raise AssertionError("mixed-binary negative control was not rejected")
    sample = {key: 1.0 for key in METRICS} | {
        "economy": True, "notes": False, "cpus": 2, "indexed_docs": 96000,
        "search_samples": 100, "stamps": [[1, 2, 3, 4]] * 8,
        "diagnostics": "active_preparations: 0, peak_active_preparations: 2, reserved_bytes: 0, peak_reserved_bytes: 3",
    }
    validate_sample(sample, sample["stamps"])
    for invalid, expected in ((sample | {"search_samples": 0}, None),
                              (sample, [[0, 0, 0, 0]] * 8)):
        try:
            validate_sample(invalid, expected)
        except RuntimeError:
            pass
        else:
            raise AssertionError("invalid overlap/fingerprints were not rejected")
    print("PASS: schedule/control identity, descriptive ratios, overlap and fingerprint gates")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", type=Path)
    parser.add_argument("--head", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--build-root", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if any(getattr(args, key) is None for key in ("base", "head", "out", "build_root")):
        parser.error("--base, --head, --out and --build-root are required")
    base, head, output, build_root = (getattr(args, key).resolve()
                                     for key in ("base", "head", "out", "build_root"))
    if output == build_root or output.is_relative_to(build_root) or build_root.is_relative_to(output):
        parser.error("artifact output and build caches must be separate")
    for root in (base, head):
        if output.is_relative_to(root) or build_root.is_relative_to(root):
            parser.error("artifacts/builds must be outside canonical source checkouts")
    output.mkdir(parents=True, exist_ok=False)
    status = {"state": "running", "completed_processes": 0}
    records = []
    try:
        shutil.copy2(__file__, output / "controller.py")
        (output / "layout_metadata.rs").write_text(LAYOUT_SOURCE)
        fixture_copy = output / "indexing_throughput.rs"
        shutil.copy2(head / FIXTURE, fixture_copy)
        rows = schedule()
        validate_schedule(rows)
        json_write(output / "predeclared-schedule.json", rows)
        print("Predeclared schedule SHA256:", sha256(output / "predeclared-schedule.json"), flush=True)
        allowed = sorted(os.sched_getaffinity(0))
        if len(allowed) < 2:
            raise RuntimeError("two distinct allowed CPUs are required")
        affinity = allowed[:2]
        overrides = [key for key, value in os.environ.items() if value and
                     (key in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER",
                              "RUSTC_WORKSPACE_WRAPPER", "RUSTDOCFLAGS"}
                      or key.startswith("CARGO_PROFILE_RELEASE_"))]
        if overrides:
            raise RuntimeError(f"unapproved compiler override names: {sorted(overrides)}")
        environment = os.environ | {"CARGO_BUILD_JOBS": "1", "OTZARIA_BENCH_LINES": "12000",
                                    "OTZARIA_BENCH_ECONOMY": "1"}
        environment.pop("OTZARIA_BENCH_NOTES", None)
        rustc = text_command(["rustc", "-vV"], environment)
        cargo = text_command(["cargo", "--version"], environment)
        if "release: 1.99.1\n" not in rustc + "\n" or not cargo.startswith("cargo 1.99.1 "):
            raise RuntimeError("diagnostic requires the original Rust/Cargo 1.99.1 toolchain")
        tmpdir = Path(tempfile.gettempdir())
        topology = {}
        for cpu in affinity:
            topology[cpu] = {}
            for field in ("core_id", "physical_package_id", "thread_siblings_list", "core_siblings_list"):
                path = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/{field}")
                topology[cpu][field] = path.read_text().strip() if path.exists() else None
        metadata = {
            "revisions": REVISIONS, "fixture_sha256": FIXTURE_SHA256,
            "controller_sha256": sha256(output / "controller.py"),
            "layout_source_sha256": sha256(output / "layout_metadata.rs"),
            "predeclared_schedule_sha256": sha256(output / "predeclared-schedule.json"),
            "controller_commit": git(Path(__file__).resolve().parents[1], "rev-parse", "HEAD"),
            "platform": platform.platform(), "uname": list(platform.uname()),
            "allowed_affinity": allowed, "measurement_affinity": affinity,
            "selected_cpu_topology": topology,
            "cpu_topology": json.loads(text_command(["lscpu", "--json"])),
            "rustc": rustc, "cargo": cargo, "python": sys.version,
            "tmpdir": str(tmpdir), "tmpdir_filesystem": text_command(["df", "-T", str(tmpdir)]),
            "explicit_build_environment": {"CARGO_BUILD_JOBS": "1"},
            "explicit_runtime_environment": {"OTZARIA_BENCH_LINES": "12000", "OTZARIA_BENCH_ECONOMY": "1"},
            "unset_runtime_environment": ["OTZARIA_BENCH_NOTES"],
            "github": {key: os.environ.get(key) for key in
                       ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA", "GITHUB_REF", "RUNNER_OS", "RUNNER_ARCH")},
            "notes": [
                "Original fixture source is unchanged; separate layout-only example adds no dependency or feature.",
                "Timed latency includes search, result-ID collection and assertion; 10 ms inter-query sleep is excluded.",
                "Thirty idle probes warm each process before concurrent indexing; idle p95 is retained.",
                "Per-query wall/CPU vectors are not instrumented in this first phase.",
                "Whole-process CPU includes the indexer and cannot attribute search-only CPU time.",
                "No cooldown, cache drop, build or extra warmup occurs during the schedule.",
                "Samples are synthetic, warm, two-core Linux economy indexing; not Flutter frame or full-library measurements.",
            ],
        }
        json_write(output / "environment.json", metadata)
        binaries = {}
        for label, root in (("B", base), ("H", head)):
            binaries[label], metadata[label] = build(root, label, output, build_root, environment)
            json_write(output / "environment.json", metadata)
        for row in rows:
            row["binary_sha256"] = metadata[row["actual_binary"]]["binary_sha256"]
            row["source_commit"] = REVISIONS[row["actual_binary"]]
        validate_schedule(rows)
        json_write(output / "resolved-schedule.json", rows)
        json_write(output / "measurement-start.json", snapshot())
        expected_stamps = None
        with (output / "samples.jsonl").open("w") as samples:
            for entry in rows:
                record = measure(entry, binaries[entry["actual_binary"]], output,
                                 affinity, environment, expected_stamps)
                expected_stamps = record["sample"]["stamps"]
                records.append(record)
                samples.write(json.dumps(record, sort_keys=True) + "\n")
                samples.flush()
                status["completed_processes"] = len(records)
                json_write(output / "status.json", status)
                json_write(output / "analysis.json", analyse(records))
                print(json.dumps(entry | {key: record["sample"][key] for key in
                      ("search_idle_p95_ms", "search_during_p50_ms", "search_during_p95_ms",
                       "search_during_p99_ms", "search_samples", "docs_per_sec")}), flush=True)
        validate_schedule(records)
        for label, root in (("B", base), ("H", head)):
            source_provenance(root, label)
            if sha256(binaries[label]) != metadata[label]["binary_sha256"]:
                raise RuntimeError("benchmark binary changed during measurement")
        status["state"] = "complete"
        status["performance_gate"] = "none; descriptive comparison requires independent interpretation"
    except BaseException:
        status["state"] = "failed"
        (output / "failure.log").write_text(traceback.format_exc())
        raise
    finally:
        json_write(output / "status.json", status)
        json_write(output / "analysis.json", analyse(records))
        files = sorted(path for path in output.rglob("*") if path.is_file() and path.name != "SHA256SUMS")
        (output / "SHA256SUMS").write_text("".join(
            f"{sha256(path)}  {path.relative_to(output)}\n" for path in files))


if __name__ == "__main__":
    main()
