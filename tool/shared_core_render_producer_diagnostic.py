#!/usr/bin/env python3
"""Isolated B/H/C render-producer experiment with topology and order controls.

Reuse B/H from the exact phase-1 artifact; build immutable C exactly once. The
SHA-verified phase-1 helper supplies build, layout, measurement and sample gates.
Six real process observations per binary per topology remain separate from all
C-only pseudo-label controls. No timing threshold or automatic promotion.
"""
import argparse
from collections import Counter
import hashlib
import itertools
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import stat
import statistics
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
    "C": "7b1d1329591c0d019c9835b21c28450c32b2d5fc",
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
CANDIDATE = {
    "commit": REVISIONS["C"],
    "parent": REVISIONS["H"],
    "tree": "b5bd824ac930f64a21cd5b3365bd69e9fd1f9c17",
    "path": "rust/src/api/search_engine.rs",
    "source_git_blob": "7566ac88fbb32f176e4f6ef18d0b64b903136d97",
    "source_sha256": "94d670184b239bdc68b7b00290eff4c731ac244283bb50c10605b70fb87d4b99",
}
SIBLINGS = "smt_siblings"
DISTINCT = "distinct_physical_cores"
CASES = (SIBLINGS, DISTINCT)
LABELS = "BHC"
CONTRASTS = (("H", "B"), ("C", "H"), ("C", "B"))
# Cyclic permutations start real blocks on SMT; their reversals start on distinct
# cores. Controls reverse that precedence. Each C-position group is balanced.
# This permutation order also minimizes the within-kind temporal index imbalance
# (11 versus 10); across real+control first-topology block indices sum to 39 each.
PERMUTATIONS = (
    ("BHC", SIBLINGS), ("CHB", DISTINCT),
    ("BCH", DISTINCT), ("HCB", SIBLINGS),
    ("HBC", DISTINCT), ("CBH", SIBLINGS),
)


def digest_stream(source):
    digest = hashlib.sha256()
    for chunk in iter(lambda: source.read(1 << 20), b""):
        digest.update(chunk)
    return digest.hexdigest()


def file_digest(path):
    with Path(path).open("rb") as source:
        return digest_stream(source)


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def verified_source(archive, output=None):
    """Check every archived byte before importing the retained phase-1 helper."""
    if file_digest(archive) != ARTIFACT["zip_sha256"]:
        raise RuntimeError("original artifact ZIP digest changed")
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
            if (path.is_absolute() or ".." in path.parts or name in hashes
                    or not re.fullmatch(r"[0-9a-f]{64}", digest)):
                raise RuntimeError("unsafe or invalid manifest entry")
            hashes[name] = digest
        if len(hashes) != 94 or set(names) != set(hashes) | {"SHA256SUMS"}:
            raise RuntimeError("original artifact inventory changed")
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
        originals = {label: REVISIONS[label] for label in "BH"}
        expected_github = {
            "GITHUB_RUN_ID": str(ARTIFACT["run_id"]),
            "GITHUB_RUN_ATTEMPT": str(ARTIFACT["run_attempt"]),
            "GITHUB_SHA": ARTIFACT["commit"], "GITHUB_REF": ARTIFACT["ref"],
            "RUNNER_OS": "Linux", "RUNNER_ARCH": "X64",
        }
        if environment["revisions"] != originals or any(
                environment["github"].get(key) != value for key, value in expected_github.items()):
            raise RuntimeError("original run/source provenance changed")
        if (environment["controller_commit"] != ARTIFACT["commit"]
                or environment["controller_sha256"] != SOURCE_HASHES["controller.py"]):
            raise RuntimeError("original helper provenance changed")
        for label in "BH":
            record = environment[label]
            if record["commit"] != REVISIONS[label] or record["binary_sha256"] != BINARY_HASHES[label]:
                raise RuntimeError(f"original {label} executable provenance changed")
            if hashes[f"binaries/{label}-indexing-throughput"] != BINARY_HASHES[label]:
                raise RuntimeError(f"original {label} executable bytes changed")
            if record["files_sha256"]["rust/tests/indexing_throughput.rs"] != SOURCE_HASHES["indexing_throughput.rs"]:
                raise RuntimeError("original fixture identity changed")
        original_rows = [json.loads(line) for line in source.read("samples.jsonl").decode().splitlines()]
        if len(original_rows) != 32:
            raise RuntimeError("original raw sample inventory changed")
        expected_stamps = original_rows[0]["sample"]["stamps"]
        if any(row["sample"]["stamps"] != expected_stamps for row in original_rows):
            raise RuntimeError("original document fingerprints disagree")
        helper = source.read("controller.py")
        if output is not None:
            inputs = output / "source-artifact"
            inputs.mkdir()
            retained = [*SOURCE_HASHES, "environment.json", "status.json", "SHA256SUMS",
                        "predeclared-schedule.json", "resolved-schedule.json", "samples.jsonl", "analysis.json"]
            retained.extend(f"build-logs/{label}/{name}" for label in "BH"
                            for name in ("provenance.json", "command.json", "layout.log"))
            for name in retained:
                destination = inputs / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(source.read(name))
            (output / "binaries").mkdir()
            for label in "BH":
                name = f"binaries/{label}-indexing-throughput"
                with source.open(name) as entry, (output / name).open("wb") as destination:
                    shutil.copyfileobj(entry, destination)
                binary = output / name
                binary.chmod(stat.S_IMODE(binary.stat().st_mode) | 0o111)
                if file_digest(binary) != BINARY_HASHES[label]:
                    raise RuntimeError("copied executable digest changed")
    core = types.ModuleType("verified_phase1_controller")
    core.__file__ = str(output / "source-artifact/controller.py") if output else "<verified-phase1-helper>"
    exec(compile(helper, core.__file__, "exec"), core.__dict__)
    if (core.REVISIONS != originals or core.FIXTURE_SHA256 != SOURCE_HASHES["indexing_throughput.rs"]
            or hashlib.sha256(core.LAYOUT_SOURCE.encode()).hexdigest() != SOURCE_HASHES["layout_metadata.rs"]):
        raise RuntimeError("verified helper constants disagree with the retained sources")
    core.validate_schedule(original_rows)
    for row in original_rows:
        core.validate_sample(row["sample"], expected_stamps)
    # Configure only the label-to-revision mapping; build/measure/parser code stays
    # exactly as imported from the SHA-verified phase-1 artifact.
    core.REVISIONS = dict(REVISIONS)
    return core, environment, expected_stamps


def candidate_provenance(core, root, original_environment):
    if not re.fullmatch(r"[0-9a-f]{40}", REVISIONS["C"]):
        raise RuntimeError("immutable candidate commit must be filled before execution")
    record = core.source_provenance(root, "C")
    if core.git(root, "status", "--porcelain=v1", "--untracked-files=all"):
        raise RuntimeError("candidate checkout has unexpected tracked or untracked changes")
    parents = core.git(root, "rev-list", "--parents", "-n", "1", "HEAD").split()
    changed = core.git(root, "diff", "--name-only", "HEAD^", "HEAD").splitlines()
    blob = core.git(root, "rev-parse", f"HEAD:{CANDIDATE['path']}")
    if (parents != [REVISIONS["C"], REVISIONS["H"]] or record["tree"] != CANDIDATE["tree"]
            or changed != [CANDIDATE["path"]] or blob != CANDIDATE["source_git_blob"]
            or file_digest(root / CANDIDATE["path"]) != CANDIDATE["source_sha256"]):
        raise RuntimeError("candidate is not the frozen single-source change on immutable H")
    if record["files_sha256"] != original_environment["H"]["files_sha256"]:
        raise RuntimeError("candidate changed the original fixture, Cargo manifest or Cargo.lock")
    return record | {"candidate": CANDIDATE, "changed_paths_from_H": changed}


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
                    and topology[other]["core_id"] != identity[1]
                    and other not in parse_cpu_list(info["thread_siblings_list"])
                    and cpu not in parse_cpu_list(topology[other]["thread_siblings_list"])]
        if siblings and distinct:
            return {SIBLINGS: [cpu, siblings[0]], DISTINCT: [cpu, distinct[0]]}
    raise RuntimeError("runner needs an allowed SMT-sibling pair and a distinct core in the same package")


def schedule(affinities, binary_hashes):
    rows = []
    case_runs = Counter()
    for permutation, (order, first_real) in enumerate(PERMUTATIONS, 1):
        other = next(case for case in CASES if case != first_real)
        for kind, first in (("real", first_real), ("control", other)):
            second = next(case for case in CASES if case != first)
            block = f"{'R' if kind == 'real' else 'CC'}{permutation}"
            for precedence, case in enumerate((first, second), 1):
                for position, label in enumerate(order, 1):
                    actual = label if kind == "real" else "C"
                    case_runs[case] += 1
                    rows.append({
                        "run": len(rows) + 1, "case_run": case_runs[case],
                        "block": block, "permutation": permutation, "kind": kind,
                        "order": order, "position": position, "C_position": order.index("C") + 1,
                        "pseudo_label": label, "actual_binary": actual,
                        "topology_case": case, "topology_precedence": precedence,
                        "affinity": affinities[case], "source_commit": REVISIONS[actual],
                        "binary_sha256": binary_hashes.get(actual),
                    })
    return rows


def validate_schedule(rows, binary_hashes, resolved=True):
    assert len(rows) == 72
    assert [row["run"] for row in rows] == list(range(1, 73))
    assert len(set(binary_hashes[label] for label in "BH")) == 2
    if resolved:
        assert all(re.fullmatch(r"[0-9a-f]{64}", binary_hashes[label]) for label in LABELS)
    affinities = {case: next(row["affinity"] for row in rows if row["topology_case"] == case)
                  for case in CASES}
    assert affinities[SIBLINGS][0] == affinities[DISTINCT][0]
    assert all(len(set(pair)) == len(pair) == 2 for pair in affinities.values())
    assert len(set(affinities[SIBLINGS]) | set(affinities[DISTINCT])) == 3
    expected_orders = sorted("".join(order) for order in itertools.permutations(LABELS))
    first_blocks = []
    for offset in range(0, 72, 6):
        pair = rows[offset:offset + 6]
        assert len({(row["block"], row["kind"], row["order"]) for row in pair}) == 1
        assert pair[0]["topology_case"] != pair[3]["topology_case"]
        for precedence, half in enumerate((pair[:3], pair[3:]), 1):
            assert [row["position"] for row in half] == [1, 2, 3]
            assert len({row["topology_case"] for row in half}) == 1
            assert all(row["topology_precedence"] == precedence for row in half)
        first_blocks.append(pair[0] | {"block_pair_index": offset // 6 + 1})
    for case in CASES:
        selected = [row for row in rows if row["topology_case"] == case]
        assert [row["case_run"] for row in selected] == list(range(1, 37))
        for row in selected:
            assert row["affinity"] == affinities[case]
            assert row["order"] == PERMUTATIONS[row["permutation"] - 1][0]
            assert row["C_position"] == row["order"].index("C") + 1
            assert row["pseudo_label"] == row["order"][row["position"] - 1]
            assert row["actual_binary"] == (row["pseudo_label"] if row["kind"] == "real" else "C")
            assert row["source_commit"] == REVISIONS[row["actual_binary"]]
            assert row["binary_sha256"] == binary_hashes.get(row["actual_binary"])
        for offset in range(0, 36, 6):
            real, control = selected[offset:offset + 3], selected[offset + 3:offset + 6]
            assert all(row["kind"] == "real" for row in real)
            assert all(row["kind"] == "control" for row in control)
            assert [row["pseudo_label"] for row in real] == [row["pseudo_label"] for row in control]
            assert real[0]["permutation"] == control[0]["permutation"]
            assert real[0]["topology_precedence"] != control[0]["topology_precedence"]
        for kind in ("real", "control"):
            group = [row for row in selected if row["kind"] == kind]
            assert len(group) == 18
            assert sorted(row["order"] for row in group if row["position"] == 1) == expected_orders
            for label in LABELS:
                assert sum(row["pseudo_label"] == label for row in group) == 6
                for position in (1, 2, 3):
                    assert sum(row["pseudo_label"] == label and row["position"] == position for row in group) == 2
            first = [row for row in first_blocks if row["kind"] == kind and row["topology_case"] == case]
            assert len(first) == 3
            assert sorted(row["C_position"] for row in first) == [1, 2, 3]
        assert sum(row["block_pair_index"] for row in first_blocks if row["topology_case"] == case) == 39
    for kind in ("real", "control"):
        sums = [sum(row["permutation"] for row in first_blocks
                    if row["kind"] == kind and row["topology_case"] == case) for case in CASES]
        assert sorted(sums) == [10, 11]
    # Pin the declared chronological plan as well as its balance properties.
    assert rows == schedule(affinities, binary_hashes) or all(
        all(row[key] == value for key, value in planned.items())
        for row, planned in zip(rows, schedule(affinities, binary_hashes)))


def process_medians(rows):
    keys = ("process_wall_seconds", "whole_process_cpu_user_seconds", "whole_process_cpu_system_seconds")
    return {key: statistics.median(row["process"][key] for row in rows)
            if rows and all(key in row.get("process", {}) for row in rows) else None for key in keys}


def label_summary(core, rows, field):
    return {label: {"process_observations": len(group), "fixture_medians": core.medians(group),
                    "whole_process_medians": process_medians(group)}
            for label in LABELS for group in [[row for row in rows if row[field] == label]]}


def contrasts(core, summaries):
    return {f"{numerator}_over_{denominator}": {
        key: core.ratio(summaries[numerator]["fixture_medians"][key],
                        summaries[denominator]["fixture_medians"][key])
        for key in core.METRICS} for numerator, denominator in CONTRASTS}


def analyse(core, records):
    reports = {}
    for case in CASES:
        selected = [row for row in records if row["topology_case"] == case]
        real = [row for row in selected if row["kind"] == "real"]
        controls = [row for row in selected if row["kind"] == "control"]
        actual = label_summary(core, real, "actual_binary")
        pseudo = label_summary(core, controls, "pseudo_label")
        blocks = []
        for block in dict.fromkeys(row["block"] for row in selected):
            group = [row for row in selected if row["block"] == block]
            if len(group) != 3:
                continue
            summary = label_summary(core, group, "pseudo_label")
            blocks.append({
                "block": block, "kind": group[0]["kind"], "order": group[0]["order"],
                "C_position": group[0]["C_position"], "topology_precedence": group[0]["topology_precedence"],
                "actual_binaries": sorted({row["actual_binary"] for row in group}),
                "binary_hashes": sorted({row["binary_sha256"] for row in group}),
                "pseudo_label_summaries": summary, "pseudo_label_ratios": contrasts(core, summary),
            })
        groups = {}
        for position in (1, 2, 3):
            subgroup = [row for row in real if row["C_position"] == position]
            summary = label_summary(core, subgroup, "actual_binary")
            groups[str(position)] = {"real_binary_summaries": summary, "real_ratios": contrasts(core, summary)}
        reports[case] = {
            "completed_processes": len(selected),
            "primary_complete": len(real) == 18 and all(actual[label]["process_observations"] == 6 for label in LABELS),
            "real_binary_summaries": actual, "real_ratios": contrasts(core, actual),
            "same_C_controls": {
                "process_observations": len(controls), "pseudo_label_summaries": pseudo,
                "pseudo_label_ratios": contrasts(core, pseudo),
                "position_medians": {str(position): core.medians([row for row in controls if row["position"] == position])
                                     for position in (1, 2, 3)},
            },
            "real_C_position_groups": groups, "complete_blocks": blocks,
        }
    interaction = {contrast: {key: core.ratio(reports[DISTINCT]["real_ratios"][contrast][key],
                                             reports[SIBLINGS]["real_ratios"][contrast][key])
                              for key in core.METRICS}
                   for contrast in reports[SIBLINGS]["real_ratios"]}
    return {
        "completed_processes": len(records), "schedule_complete": len(records) == 72,
        "by_topology": reports, "distinct_over_sibling_ratio_of_ratios": interaction,
        "interpretation": [
            "Each observation is one fresh fixture process; process-level percentiles are not pooled query samples.",
            "Primary comparisons use only six real observations per binary per topology when complete.",
            "All control pseudo-labels execute the same C binary; their 18 observations per topology never augment the real C sample.",
            "Controls describe positional/temporal drift under C; they do not measure binary-specific drift under B or H.",
            "Reported topology is guest-visible sysfs topology on one runner; both cases allow two logical CPUs and share one anchor CPU.",
            "Whole-process CPU includes setup, concurrent indexing/search, commit and cleanup; PSI snapshots are host-wide.",
            "The source cause remains unproven. Even measured improvement would not by itself prove the ZipProducer mechanism.",
            "No threshold, statistical pass/fail rule or automatic promotion is applied.",
            "Promotion requires measured benefit plus existing behavioral tests and independent review.",
        ],
    }


def self_test(core):
    topology = {cpu: {"physical_package_id": "0", "core_id": str(cpu // 2),
                      "thread_siblings_list": f"{cpu // 2 * 2}-{cpu // 2 * 2 + 1}"} for cpu in range(4)}
    affinities = select_affinities(topology)
    assert affinities == {SIBLINGS: [0, 1], DISTINCT: [0, 2]}
    hashes = BINARY_HASHES | {"C": "c" * 64}
    rows = schedule(affinities, hashes)
    validate_schedule(rows, hashes)
    validate_schedule(schedule(affinities, BINARY_HASHES), BINARY_HASHES, resolved=False)
    records = []
    for row in rows:
        value = {"B": 2, "H": 4, "C": 3}[row["actual_binary"]] if row["kind"] == "real" else 100
        if row["topology_case"] == DISTINCT and row["kind"] == "real":
            value = {"B": 2, "H": 3, "C": 2}[row["actual_binary"]]
        records.append(row | {"sample": {key: value for key in core.METRICS}})
    report = analyse(core, records)
    key = "search_during_p95_ms"
    assert report["by_topology"][SIBLINGS]["real_ratios"]["C_over_H"][key] == 0.75
    assert report["by_topology"][SIBLINGS]["real_binary_summaries"]["C"]["fixture_medians"][key] == 3
    assert report["distinct_over_sibling_ratio_of_ratios"]["H_over_B"][key] == 0.75
    for case in CASES:
        result = report["by_topology"][case]
        assert result["primary_complete"]
        assert all(result["real_binary_summaries"][label]["process_observations"] == 6 for label in LABELS)
        assert result["same_C_controls"]["process_observations"] == 18
        assert all(ratios[key] == 1 for ratios in result["same_C_controls"]["pseudo_label_ratios"].values())
    # A missing or altered real run cannot silently borrow a control observation.
    partial = analyse(core, records[:-12])
    assert not partial["schedule_complete"]
    assert all(not partial["by_topology"][case]["primary_complete"] for case in CASES)
    for index, replacement in ((6, {"actual_binary": "H", "binary_sha256": hashes["H"]}),
                                (0, {"affinity": [0, 0]}),
                                (0, {"source_commit": REVISIONS["H"]})):
        broken = [dict(row) for row in rows]
        broken[index].update(replacement)
        try:
            validate_schedule(broken, hashes)
        except AssertionError:
            pass
        else:
            raise AssertionError("invalid control, affinity or source identity was accepted")
    try:
        select_affinities({cpu: topology[cpu] for cpu in (0, 1)})
    except RuntimeError:
        pass
    else:
        raise AssertionError("missing distinct physical core was accepted")
    print("PASS: verified helper/archive; 72-process balance; identity gates; real/control separation; partial-result handling")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--candidate", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--build-root", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if sys.flags.optimize:
        parser.error("run without Python optimization so schedule assertions remain active")
    if args.self_test:
        core, _, _ = verified_source(args.archive.resolve())
        self_test(core)
        return
    if any(getattr(args, key) is None for key in ("candidate", "out", "build_root")):
        parser.error("--candidate, --out and --build-root are required")
    candidate, output, build_root = (getattr(args, key).resolve() for key in ("candidate", "out", "build_root"))
    if output == build_root or output.is_relative_to(build_root) or build_root.is_relative_to(output):
        parser.error("artifact output and build cache must be separate")
    if any(path == candidate or path.is_relative_to(candidate) for path in (output, build_root)):
        parser.error("artifacts/builds must be outside the canonical candidate checkout")
    if build_root.exists():
        parser.error("candidate build root must be fresh; build caching is not part of this experiment")
    output.mkdir(parents=True, exist_ok=False)
    records = []
    core = None
    status = {"state": "running", "completed_processes": 0, "candidate_build_invocations": 0}
    try:
        shutil.copy2(__file__, output / "render-producer-controller.py")
        write_json(output / "source-artifact.json", ARTIFACT)
        core, original_environment, expected_stamps = verified_source(args.archive.resolve(), output)
        source_before = candidate_provenance(core, candidate, original_environment)
        write_json(output / "candidate-source.json", source_before)
        shutil.copy2(candidate / CANDIDATE["path"], output / "candidate-search_engine.rs")
        allowed = sorted(os.sched_getaffinity(0))
        topology = {cpu: {key: Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/{key}").read_text().strip()
                          for key in ("physical_package_id", "core_id", "thread_siblings_list", "core_siblings_list")}
                    for cpu in allowed}
        affinities = select_affinities(topology)
        overrides = [key for key, value in os.environ.items() if value and
                     (key in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                              "RUSTDOCFLAGS", "RAYON_NUM_THREADS"} or key.startswith("CARGO_PROFILE_RELEASE_"))]
        if overrides:
            raise RuntimeError(f"unexpected compiler or worker overrides: {sorted(overrides)}")
        environment = os.environ | {"CARGO_BUILD_JOBS": "1", "OTZARIA_BENCH_LINES": "12000", "OTZARIA_BENCH_ECONOMY": "1"}
        environment.pop("OTZARIA_BENCH_NOTES", None)
        rustc = core.text_command(["rustc", "-vV"], environment)
        cargo = core.text_command(["cargo", "--version"], environment)
        if ("release: 1.99.1\n" not in rustc + "\n" or not cargo.startswith("cargo 1.99.1 ")
                or rustc != original_environment["rustc"] or cargo != original_environment["cargo"]):
            raise RuntimeError("candidate requires the exact original Rust/Cargo 1.99.1 toolchain identity")
        rows = schedule(affinities, BINARY_HASHES)
        validate_schedule(rows, BINARY_HASHES, resolved=False)
        write_json(output / "predeclared-schedule.json", rows)
        tmpdir = Path(tempfile.gettempdir())
        metadata = {
            "source_artifact": ARTIFACT, "original_environment": original_environment,
            "revisions": REVISIONS, "candidate": CANDIDATE,
            "verified_helper_sha256": SOURCE_HASHES["controller.py"],
            "verified_layout_source_sha256": SOURCE_HASHES["layout_metadata.rs"],
            "fixture_sha256": SOURCE_HASHES["indexing_throughput.rs"],
            "controller_sha256": file_digest(output / "render-producer-controller.py"),
            "controller_commit": core.git(Path(__file__).resolve().parents[1], "rev-parse", "HEAD"),
            "predeclared_schedule_sha256": file_digest(output / "predeclared-schedule.json"),
            "platform": platform.platform(), "uname": list(platform.uname()), "python": sys.version,
            "allowed_affinity": allowed, "topology": topology, "measurement_affinities": affinities,
            "cpu_topology": json.loads(core.text_command(["lscpu", "--json"])),
            "rustc": rustc, "cargo": cargo,
            "tmpdir": str(tmpdir), "tmpdir_filesystem": core.text_command(["df", "-T", str(tmpdir)]),
            "runtime_environment": {key: environment[key] for key in
                                    ("CARGO_BUILD_JOBS", "OTZARIA_BENCH_LINES", "OTZARIA_BENCH_ECONOMY")},
            "unset_runtime_environment": ["OTZARIA_BENCH_NOTES"],
            "expected_stamps": expected_stamps,
            "github": {key: os.environ.get(key) for key in
                       ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA", "GITHUB_REF", "RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion")},
            "notes": [
                "B/H are the exact preserved executable bytes; neither is rebuilt.",
                "C is built once with the original locked release fixture plus the identical layout-only example in the same cargo invocation.",
                "The verified phase-1 helper is reused without duplicating its build/measurement implementation; only its revision mapping adds C.",
                "All six B/H/C permutations run on each topology, each followed in that topology by a same-order C-only control block.",
                "Topology pairs interleave whole real blocks, then whole control blocks; control precedence reverses real precedence.",
                "The first topology is balanced within real/control classes and within every C-position group.",
                "The fixture retains its own 30 idle warmup probes. No extra warmup, cooldown, cache drop or build occurs during the schedule.",
                "Timed latency retains the original search, result-ID collection and assertion scope; no per-query instrumentation is added.",
                "Whole-process CPU and before/after CPU/IO/memory pressure snapshots are retained for every process.",
                "No per-run binary hashing is performed during the measurement schedule; all binaries are checked before and after it.",
                "Cause remains unproven. Promotion requires measured benefit plus existing behavioral tests and independent review.",
            ],
        }
        write_json(output / "environment.json", metadata)
        build_root.mkdir(parents=True, exist_ok=False)
        status["candidate_build_invocations"] = 1
        write_json(output / "status.json", status)
        _, candidate_build = core.build(candidate, "C", output, build_root, environment)
        if candidate_build["layout"] != original_environment["H"]["layout"]:
            raise RuntimeError("candidate SearchEngine layout changed from H")
        if candidate_provenance(core, candidate, original_environment) != source_before:
            raise RuntimeError("candidate source changed during its build")
        metadata["C"] = candidate_build
        binary_hashes = BINARY_HASHES | {"C": candidate_build["binary_sha256"]}
        metadata["binary_sha256"] = binary_hashes
        metadata["candidate_build_invocations"] = 1
        for label, expected in binary_hashes.items():
            if file_digest(output / "binaries" / f"{label}-indexing-throughput") != expected:
                raise RuntimeError(f"{label} binary changed before measurement")
        write_json(output / "environment.json", metadata)
        rows = schedule(affinities, binary_hashes)
        validate_schedule(rows, binary_hashes)
        write_json(output / "resolved-schedule.json", rows)
        print(json.dumps({"schedule_sha256": file_digest(output / "resolved-schedule.json"),
                          "affinities": affinities, "revisions": REVISIONS, "binary_sha256": binary_hashes}), flush=True)
        write_json(output / "measurement-start.json", core.snapshot())
        with (output / "samples.jsonl").open("w") as samples:
            for entry in rows:
                binary = output / "binaries" / f"{entry['actual_binary']}-indexing-throughput"
                record = core.measure(entry, binary, output, entry["affinity"], environment, expected_stamps)
                records.append(record)
                samples.write(json.dumps(record, sort_keys=True) + "\n")
                samples.flush()
                status["completed_processes"] = len(records)
                write_json(output / "status.json", status)
                write_json(output / "analysis.json", analyse(core, records))
                print(json.dumps(entry | {key: record["sample"][key] for key in
                      ("search_idle_p95_ms", "search_during_p50_ms", "search_during_p95_ms",
                       "search_during_p99_ms", "search_samples", "docs_per_sec")}), flush=True)
        validate_schedule(records, binary_hashes)
        for label, expected in binary_hashes.items():
            if file_digest(output / "binaries" / f"{label}-indexing-throughput") != expected:
                raise RuntimeError(f"{label} binary changed during measurement")
        if candidate_provenance(core, candidate, original_environment) != source_before:
            raise RuntimeError("candidate source changed during measurement")
        status["state"] = "complete"
        status["performance_gate"] = "none; measured benefit and behavior tests require independent interpretation"
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
