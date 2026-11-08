#!/usr/bin/env python3
"""Reuse exact economy binaries to test reported SMT versus distinct-core scope.

No Rust builds, source edits, extra warmups, cache drops or timing thresholds.
The original verified controller supplies all measurement and parsing code.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import shutil
import stat
import sys
import tempfile
import traceback
import types
import zipfile


ARTIFACT = {
    "id": 13594355530,
    "name": "economy-diagnostic-42420237162-1",
    "run_id": 42420237162,
    "run_attempt": 1,
    "repository": "Otzaria/otzaria_search_engine",
    "commit": "fc9cc131e070882f6531238a57d7f911249e6752",
    "ref": "refs/heads/diag/shared-core-economy-20261107",
    "zip_sha256": "6f31738c00b39e519e84b5aa9ef076509423a4c64965ee581349f54734b8bd48",
    "manifest_sha256": "3e03fc38f77577816f586bac1fb326c0a9230b42338cdcde8b50eee835794b4a",
}
REVISIONS = {
    "B": "8ef29fd592793fbc9d5af0299fdd7a8607789bac",
    "H": "4761356749815d283ede41ef956c64b3dbffe048",
}
BINARY_HASHES = {
    "B": "a58b7ec047d3b39d0f4417571d652920d14ea7f8370030aa6bddd349dc506e5a",
    "H": "974a92cc60324467d26ebad61c7d21ca0e7512087201eaa037ff357d42151337",
}
SOURCE_HASHES = {
    "controller.py": "d82706e96fc4c1d213870d33e6bc83204ec8d6597da128795554c3226b31850e",
    "layout_metadata.rs": "c85a383fa627537380b6d94c9c0fb60d402f3b0bdd4beb9e48215d3afa8e49e6",
    "indexing_throughput.rs": "58b15ac4a0736284bae336b48f6d334bc1de8bcfd3ac52689ab0417ec264ebca",
}
SIBLINGS = "smt_siblings"
DISTINCT = "distinct_physical_cores"
CASES = (SIBLINGS, DISTINCT)
# The midpoint reversal balances precedence within real/control blocks, each
# real BHHB/HBBH order and each control B/H binary. Strict ABAB would confound
# precedence with the original alternating real/control block classes.
FIRST_CASE = (SIBLINGS, DISTINCT, SIBLINGS, DISTINCT,
              DISTINCT, SIBLINGS, DISTINCT, SIBLINGS)


def digest_stream(source):
    digest = hashlib.sha256()
    for chunk in iter(lambda: source.read(1 << 20), b""):
        digest.update(chunk)
    return digest.hexdigest()


def file_digest(path):
    with path.open("rb") as source:
        return digest_stream(source)


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def verified_source(archive, output=None):
    if file_digest(archive) != ARTIFACT["zip_sha256"]:
        raise RuntimeError("original artifact ZIP digest does not match the immutable source")
    with zipfile.ZipFile(archive) as source:
        names = source.namelist()
        if len(names) != len(set(names)):
            raise RuntimeError("duplicate artifact paths")
        manifest = source.read("SHA256SUMS")
        if hashlib.sha256(manifest).hexdigest() != ARTIFACT["manifest_sha256"]:
            raise RuntimeError("original artifact manifest digest changed")
        hashes = {}
        for line in manifest.decode().splitlines():
            digest, name = line.split("  ", 1)
            path = PurePosixPath(name)
            if path.is_absolute() or ".." in path.parts or name in hashes:
                raise RuntimeError("unsafe or duplicate manifest path")
            hashes[name] = digest
        if len(hashes) != 94 or set(names) != set(hashes) | {"SHA256SUMS"}:
            raise RuntimeError("original artifact file inventory changed")
        for name, expected in hashes.items():
            if stat.S_ISLNK(source.getinfo(name).external_attr >> 16):
                raise RuntimeError("artifact symlinks are not accepted")
            with source.open(name) as entry:
                if digest_stream(entry) != expected:
                    raise RuntimeError(f"original artifact file digest changed: {name}")
        for name, expected in SOURCE_HASHES.items():
            if hashes[name] != expected:
                raise RuntimeError(f"original source/helper identity changed: {name}")
        environment = json.loads(source.read("environment.json"))
        status = json.loads(source.read("status.json"))
        if status.get("state") != "complete" or status.get("completed_processes") != 32:
            raise RuntimeError("original diagnostic did not complete all 32 processes")
        expected_github = {
            "GITHUB_RUN_ID": str(ARTIFACT["run_id"]),
            "GITHUB_RUN_ATTEMPT": str(ARTIFACT["run_attempt"]),
            "GITHUB_SHA": ARTIFACT["commit"], "GITHUB_REF": ARTIFACT["ref"],
            "RUNNER_OS": "Linux", "RUNNER_ARCH": "X64",
        }
        if environment["revisions"] != REVISIONS or any(
                environment["github"].get(key) != value for key, value in expected_github.items()):
            raise RuntimeError("original run/source provenance does not match")
        if (environment["controller_commit"] != ARTIFACT["commit"]
                or environment["controller_sha256"] != SOURCE_HASHES["controller.py"]):
            raise RuntimeError("original controller provenance does not match")
        for label, revision in REVISIONS.items():
            record = environment[label]
            if record["commit"] != revision or record["binary_sha256"] != BINARY_HASHES[label]:
                raise RuntimeError(f"original {label} executable provenance changed")
            if hashes[f"binaries/{label}-indexing-throughput"] != BINARY_HASHES[label]:
                raise RuntimeError(f"original {label} executable bytes changed")
            if record["files_sha256"]["rust/tests/indexing_throughput.rs"] != SOURCE_HASHES["indexing_throughput.rs"]:
                raise RuntimeError("original benchmark source identity changed")
        original_rows = [json.loads(line) for line in source.read("samples.jsonl").decode().splitlines()]
        if len(original_rows) != 32:
            raise RuntimeError("original raw sample inventory changed")
        expected_stamps = original_rows[0]["sample"]["stamps"]
        if any(row["sample"]["stamps"] != expected_stamps for row in original_rows):
            raise RuntimeError("original fingerprints disagree")
        controller = source.read("controller.py")
        if output is not None:
            inputs = output / "source-artifact"
            inputs.mkdir()
            for name in (*SOURCE_HASHES, "environment.json", "status.json", "SHA256SUMS",
                         "predeclared-schedule.json", "resolved-schedule.json", "samples.jsonl", "analysis.json"):
                (inputs / name).write_bytes(source.read(name))
            (output / "binaries").mkdir()
            for label in "BH":
                name = f"binaries/{label}-indexing-throughput"
                with source.open(name) as entry, (output / name).open("wb") as destination:
                    shutil.copyfileobj(entry, destination)
                binary = output / name
                binary.chmod(stat.S_IMODE(binary.stat().st_mode) | 0o111)
                if file_digest(binary) != BINARY_HASHES[label]:
                    raise RuntimeError("copied executable digest changed")
    module = types.ModuleType("verified_economy_controller")
    module.__file__ = str(output / "source-artifact/controller.py") if output else "<verified-artifact-controller>"
    exec(compile(controller, module.__file__, "exec"), module.__dict__)
    module.validate_schedule(original_rows)
    for row in original_rows:
        module.validate_sample(row["sample"], expected_stamps)
    return module, environment, expected_stamps


def parse_cpu_list(value):
    cpus = set()
    for token in value.split(","):
        first, separator, last = token.partition("-")
        cpus.update(range(int(first), int(last) + 1) if separator else [int(first)])
    return cpus


def select_affinities(topology):
    for cpu in sorted(topology):
        info = topology[cpu]
        identity = (info["physical_package_id"], info["core_id"])
        siblings = [other for other in sorted(topology) if other != cpu
                    and (topology[other]["physical_package_id"], topology[other]["core_id"]) == identity
                    and other in parse_cpu_list(info["thread_siblings_list"])
                    and cpu in parse_cpu_list(topology[other]["thread_siblings_list"])]
        distinct = [other for other in sorted(topology)
                    if topology[other]["physical_package_id"] == identity[0]
                    and topology[other]["core_id"] != identity[1]]
        if siblings and distinct:
            return {SIBLINGS: [cpu, siblings[0]], DISTINCT: [cpu, distinct[0]]}
    raise RuntimeError("runner needs an allowed SMT-sibling pair and another core in the same package")


def interleaved_schedule(core, affinities):
    original = core.schedule()
    rows = []
    for block_index, first in enumerate(FIRST_CASE):
        second = next(case for case in CASES if case != first)
        for precedence, case in enumerate((first, second), 1):
            for entry in original[block_index * 4:block_index * 4 + 4]:
                rows.append(entry | {
                    "run": len(rows) + 1, "case_run": entry["run"],
                    "topology_case": case, "topology_precedence": precedence,
                    "affinity": affinities[case],
                    "source_commit": REVISIONS[entry["actual_binary"]],
                    "binary_sha256": BINARY_HASHES[entry["actual_binary"]],
                })
    return rows


def validate_schedule(core, rows):
    assert len(rows) == 64
    assert [row["run"] for row in rows] == list(range(1, 65))
    for case in CASES:
        selected = [row for row in rows if row["topology_case"] == case]
        core.validate_schedule([row | {"run": row["case_run"]} for row in selected])
        assert [row["case_run"] for row in selected] == list(range(1, 33))
        assert len({tuple(row["affinity"]) for row in selected}) == 1
        original = core.schedule()
        for row, expected in zip(selected, original):
            assert all(row["case_run" if key == "run" else key] == value for key, value in expected.items())
            assert row["source_commit"] == REVISIONS[row["actual_binary"]]
            assert row["binary_sha256"] == BINARY_HASHES[row["actual_binary"]]
            assert len(set(row["affinity"])) == 2
    affinities = {case: next(row["affinity"] for row in rows if row["topology_case"] == case)
                  for case in CASES}
    assert affinities[SIBLINGS][0] == affinities[DISTINCT][0]
    assert len(set(affinities[SIBLINGS]) | set(affinities[DISTINCT])) == 3
    first_blocks = []
    for offset in range(0, 64, 8):
        pair = rows[offset:offset + 8]
        assert len({row["block"] for row in pair}) == 1
        for half in (pair[:4], pair[4:]):
            assert [row["position"] for row in half] == [1, 2, 3, 4]
            assert len({row["topology_case"] for row in half}) == 1
        assert pair[0]["topology_case"] != pair[4]["topology_case"]
        assert all(row["topology_precedence"] == 1 for row in pair[:4])
        assert all(row["topology_precedence"] == 2 for row in pair[4:])
        first_blocks.append(pair[0])
    for case in CASES:
        first = [row for row in first_blocks if row["topology_case"] == case]
        assert len(first) == 4
        assert sum(row["kind"] == "real" for row in first) == 2
        assert sum(row["kind"] == "control" for row in first) == 2
        assert sum((row["run"] - 1) // 8 + 1 for row in first) == 18
        for order in ("BHHB", "HBBH"):
            assert sum(row["kind"] == "real" and row["order"] == order for row in first) == 1
            assert sum(row["kind"] == "control" and row["order"] == order for row in first) == 1
        for label in "BH":
            assert sum(row["kind"] == "control" and row["actual_binary"] == label for row in first) == 1


def analyse(core, records):
    reports = {case: core.analyse([row for row in records if row["topology_case"] == case])
               for case in CASES}
    interactions = {}
    for key in core.METRICS:
        ratios = {case: core.ratio(reports[case]["real_binary_medians"]["H"][key],
                                   reports[case]["real_binary_medians"]["B"][key])
                  for case in CASES}
        interactions[key] = {"real_H_over_B": ratios,
                             "distinct_ratio_over_sibling_ratio": core.ratio(ratios[DISTINCT], ratios[SIBLINGS])}
    blocks = []
    for name in ("R1", "R2", "R3", "R4"):
        pair = {case: next((block for block in reports[case]["blocks"] if block["block"] == name), None)
                for case in CASES}
        if all(pair.values()):
            ratios = {case: pair[case]["pseudo_H_over_B"]["search_during_p95_ms"] for case in CASES}
            blocks.append({"block": name, "p95_H_over_B": ratios,
                           "distinct_ratio_over_sibling_ratio": core.ratio(ratios[DISTINCT], ratios[SIBLINGS])})
    return {"completed_processes": len(records), "by_topology": reports,
            "descriptive_interaction": interactions, "matched_real_blocks": blocks,
            "interpretation": [
                "Each topology retains all original real and same-binary control blocks; do not pool them as query replicates.",
                "Reported topology is guest-visible sysfs topology, with the same two-logical-CPU budget in both cases.",
                "This comparison tests topology dependence on one runner; it does not identify a source cause or fix the prior finding.",
                "A smaller distinct-core effect does not excuse a reproduced sibling-core regression.",
                "No performance threshold or automatic causal/ready-state decision is applied.",
            ]}


def self_test(core):
    topology = {
        0: {"physical_package_id": "0", "core_id": "0", "thread_siblings_list": "0-1"},
        1: {"physical_package_id": "0", "core_id": "0", "thread_siblings_list": "0-1"},
        2: {"physical_package_id": "0", "core_id": "1", "thread_siblings_list": "2-3"},
        3: {"physical_package_id": "0", "core_id": "1", "thread_siblings_list": "2-3"},
    }
    affinities = select_affinities(topology)
    assert affinities == {SIBLINGS: [0, 1], DISTINCT: [0, 2]}
    rows = interleaved_schedule(core, affinities)
    validate_schedule(core, rows)
    records = []
    for row in rows:
        value = 2 if row["actual_binary"] == "H" and row["topology_case"] == SIBLINGS else 1
        records.append(row | {"sample": {key: value for key in core.METRICS}})
    report = analyse(core, records)
    assert report["descriptive_interaction"]["search_during_p95_ms"]["distinct_ratio_over_sibling_ratio"] == 0.5
    for case in CASES:
        for block in report["by_topology"][case]["blocks"]:
            if block["kind"] == "control":
                assert block["pseudo_H_over_B"]["search_during_p95_ms"] == 1
    try:
        select_affinities({cpu: topology[cpu] for cpu in (0, 1)})
    except RuntimeError:
        pass
    else:
        raise AssertionError("missing distinct physical core was not rejected")
    broken = [dict(row) for row in rows]
    broken[0]["affinity"] = [0, 0]
    try:
        validate_schedule(core, broken)
    except AssertionError:
        pass
    else:
        raise AssertionError("invalid logical-CPU affinity was not rejected")
    print("PASS: immutable archive, topology detection, 64-run balance, controls and interaction analysis")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if sys.flags.optimize:
        parser.error("run without Python optimization so all schedule assertions remain active")
    if args.self_test:
        core, _, _ = verified_source(args.archive.resolve())
        self_test(core)
        return
    if args.out is None:
        parser.error("--out is required")
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=False)
    records = []
    core = None
    status = {"state": "running", "completed_processes": 0, "rust_builds": 0}
    try:
        shutil.copy2(__file__, output / "topology-controller.py")
        write_json(output / "source-artifact.json", ARTIFACT)
        core, original_environment, expected_stamps = verified_source(args.archive.resolve(), output)
        allowed = sorted(os.sched_getaffinity(0))
        topology = {}
        for cpu in allowed:
            topology[cpu] = {}
            for key in ("physical_package_id", "core_id", "thread_siblings_list", "core_siblings_list"):
                topology[cpu][key] = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/{key}").read_text().strip()
        affinities = select_affinities(topology)
        environment = os.environ | {"CARGO_BUILD_JOBS": "1", "OTZARIA_BENCH_LINES": "12000", "OTZARIA_BENCH_ECONOMY": "1"}
        environment.pop("OTZARIA_BENCH_NOTES", None)
        if environment.get("RAYON_NUM_THREADS"):
            raise RuntimeError("unexpected RAYON_NUM_THREADS override would change the worker budget")
        tmpdir = Path(tempfile.gettempdir())
        metadata = {
            "source_artifact": ARTIFACT, "original_environment": original_environment,
            "controller_sha256": file_digest(output / "topology-controller.py"),
            "controller_commit": core.git(Path(__file__).resolve().parents[1], "rev-parse", "HEAD"),
            "platform": platform.platform(), "uname": list(platform.uname()), "python": sys.version,
            "allowed_affinity": allowed, "topology": topology, "measurement_affinities": affinities,
            "cpu_topology": json.loads(core.text_command(["lscpu", "--json"])),
            "tmpdir": str(tmpdir), "tmpdir_filesystem": core.text_command(["df", "-T", str(tmpdir)]),
            "runtime_environment": {key: environment[key] for key in
                                    ("CARGO_BUILD_JOBS", "OTZARIA_BENCH_LINES", "OTZARIA_BENCH_ECONOMY")},
            "unset_runtime_environment": ["OTZARIA_BENCH_NOTES"], "rust_builds": 0,
            "binary_sha256": BINARY_HASHES, "expected_stamps": expected_stamps,
            "github": {key: os.environ.get(key) for key in
                       ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA", "GITHUB_REF", "RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion")},
            "notes": [
                "The original ZIP, all 94 original manifest entries and both exact original executables were verified before execution.",
                "Both exact compiled fixtures are reused; only executable permission bits were restored, with no Rust compilation or metadata-helper execution.",
                "The verified original controller measures each unmodified fixture; no per-query instrumentation is added.",
                "Topology blocks use two logical CPUs throughout, with one shared anchor CPU and the same CPU package.",
                "First-topology order S,D,S,D,D,S,D,S balances real/control class, original order and control binary.",
                "Whole-process CPU includes fixture setup, indexer/search work, commit and cleanup; PSI is host-wide.",
                "No performance threshold, cooldown, cache dropping, extra warmup or per-run executable hashing is used.",
            ],
        }
        write_json(output / "environment.json", metadata)
        rows = interleaved_schedule(core, affinities)
        validate_schedule(core, rows)
        write_json(output / "predeclared-schedule.json", rows)
        print(json.dumps({"schedule_sha256": file_digest(output / "predeclared-schedule.json"),
                          "affinities": affinities, "binary_sha256": BINARY_HASHES}), flush=True)
        write_json(output / "measurement-start.json", core.snapshot())
        with (output / "samples.jsonl").open("w") as samples:
            for entry in rows:
                record = core.measure(entry, output / "binaries" / f"{entry['actual_binary']}-indexing-throughput",
                                      output, entry["affinity"], environment, expected_stamps)
                records.append(record)
                samples.write(json.dumps(record, sort_keys=True) + "\n")
                samples.flush()
                status["completed_processes"] = len(records)
                write_json(output / "status.json", status)
                write_json(output / "analysis.json", analyse(core, records))
                print(json.dumps(entry | {key: record["sample"][key] for key in
                      ("search_idle_p95_ms", "search_during_p50_ms", "search_during_p95_ms",
                       "search_during_p99_ms", "search_samples", "docs_per_sec")}), flush=True)
        validate_schedule(core, records)
        for label in "BH":
            if file_digest(output / "binaries" / f"{label}-indexing-throughput") != BINARY_HASHES[label]:
                raise RuntimeError("preserved executable changed during measurement")
        status["state"] = "complete"
        status["performance_gate"] = "none; topology dependence does not resolve the source cause"
    except BaseException:
        status["state"] = "failed"
        (output / "failure.log").write_text(traceback.format_exc())
        raise
    finally:
        write_json(output / "status.json", status)
        if core is not None:
            write_json(output / "analysis.json", analyse(core, records))
        files = sorted(path for path in output.rglob("*") if path.is_file() and path != output / "SHA256SUMS")
        (output / "SHA256SUMS").write_text("".join(
            f"{file_digest(path)}  {path.relative_to(output)}\n" for path in files))


if __name__ == "__main__":
    main()
