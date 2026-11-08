#!/usr/bin/env python3
"""Frozen H/E render-budget experiment; separate concurrent and wide targets.

Two clean release builds. Unchanged phase-1 measurement helper for the original
fixture. Fixed main/wide schedules, same-binary controls, exact output checks and
preserved partial evidence. No timing threshold or automatic promotion.
"""
import argparse
import copy
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import re
import resource
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import traceback

HELPER_SHA256 = "d82706e96fc4c1d213870d33e6bc83204ec8d6597da128795554c3226b31850e"
H_COMMIT = "4761356749815d283ede41ef956c64b3dbffe048"
E_COMMIT = "1042c9f2d08bec0015970d1ee5035e75d85967fe"
SIBLINGS = "smt_siblings"
DISTINCT = "distinct_physical_cores"
TOPOLOGIES = (SIBLINGS, DISTINCT)
LABELS = ("H", "E")
MAIN_FIRST = (SIBLINGS, DISTINCT, SIBLINGS, DISTINCT,
              DISTINCT, SIBLINGS, DISTINCT, SIBLINGS)
WIDE_BLOCKS = (
    ("R1", "real", "HEEH", None, SIBLINGS, ("forward", "reverse", "forward", "reverse")),
    ("CH1", "control", "HEEH", "H", DISTINCT, ("reverse", "forward", "reverse", "forward")),
    ("R2", "real", "EHHE", None, DISTINCT, ("reverse", "forward", "reverse", "forward")),
    ("CE1", "control", "EHHE", "E", SIBLINGS, ("forward", "reverse", "forward", "reverse")),
)
WIDE_METRICS = ("p50_ms", "p95_ms", "p99_ms", "measured_queries", "warmup_queries")
RESULT_FIELDS = {"title", "reference", "text", "id", "segment", "is_pdf", "file_path",
                 "merged_count", "merged", "text_status", "continues_to_next_line"}
SIBLING_FIELDS = {"title", "reference", "id", "segment", "is_pdf", "file_path"}
PHASE1_MAP = {"B": "H", "H": "E"}
PHASE1_BLOCKS = {"R1": "R1", "CB1": "CH1", "R2": "R2", "CH1": "CE1",
                 "R3": "R3", "CB2": "CH2", "R4": "R4", "CH2": "CE2"}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def file_digest(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def byte_digest(data):
    return hashlib.sha256(data).hexdigest()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def run_text(command, environment=None, timeout=60):
    return subprocess.check_output(command, text=True, env=environment, timeout=timeout).strip()


def git(root, *args):
    return run_text(["git", "-C", str(root), *args])


def import_helper(path):
    require(file_digest(path) == HELPER_SHA256, "phase-1 helper bytes changed")
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("otzaria_frozen_phase1_helper", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_config(path, frozen):
    config = json.loads(path.read_text())
    require(config["schema_version"] == 1, "unexpected diagnostic schema")
    require(config["revisions"]["H"]["commit"] == H_COMMIT, "H source changed")
    require(config["revisions"]["E"]["commit"] == E_COMMIT, "E source changed")
    require(config["revisions"]["E"]["parent"] == H_COMMIT, "E parent changed")
    require(config["original"]["helper_sha256"] == HELPER_SHA256, "helper identity changed")
    require(config["wide"]["total_cases"] == 58 and config["wide"]["timed_cases"] == 14
            and config["wide"]["behavior_cases"] == 44, "wide matrix changed")
    require(config["wide"]["warmup_queries"] == 5 and config["wide"]["measured_queries"] == 30,
            "wide warmup/sample counts changed")
    require(config["timeouts_seconds"] == {"build": 1200, "measured_process": 180,
                                            "layout": 60, "controller_budget": 3300},
            "diagnostic bounds changed")
    require(config["counts"] == {"main_processes": 64, "wide_processes": 32},
            "fixed process counts changed")
    if frozen:
        require(config["state"] == "frozen", "refusing to execute an unfrozen protocol")
        for label in LABELS:
            revision = config["revisions"][label]
            for key, length in (("commit", 40), ("tree", 40), ("source_git_blob", 40),
                                ("source_sha256", 64)):
                require(re.fullmatch(r"[0-9a-f]{" + str(length) + r"}", revision[key] or ""),
                        f"unfrozen {label} {key}")
        require(re.fullmatch(r"[0-9a-f]{64}", config["wide"]["source_sha256"] or ""),
                "wide fixture hash is not frozen")
    return config



def load_fixture_manifest(path, config):
    require(file_digest(path) == config["wide"]["manifest_sha256"], "frozen fixture manifest bytes changed")
    manifest = json.loads(path.read_text())
    require(manifest["fixture"]["sha256"] == config["wide"]["source_sha256"],
            "fixture manifest refers to different source bytes")
    require(manifest["source_pair"]["H"] == H_COMMIT
            and manifest["source_pair"]["E"] == E_COMMIT
            and manifest["source_pair"]["E_parent"] == H_COMMIT
            and manifest["source_pair"]["E_source_sha256"] == config["revisions"]["E"]["source_sha256"],
            "fixture manifest source pair changed")
    for direction in ("forward", "reverse"):
        require(manifest["orders"][direction] == [case["case_id"] for case in ordered_specs(config, direction)],
                "fixture manifest case order differs from the fixed controller protocol")
    expected = ordered_specs(config, "forward")
    require(len(manifest["cases"]) == len(expected), "fixture manifest case count differs")
    for declared, case in zip(manifest["cases"], expected):
        require(all(declared[key] == case[key] for key in ("case_id", "storage", "profile", "requested_hits", "timed", "phase")),
                "fixture manifest case identity differs from the fixed controller protocol")
    require(set(manifest["snapshot_schema"]["result_fields"]) == RESULT_FIELDS
            and set(manifest["snapshot_schema"]["merged_sibling_fields"]) == SIBLING_FIELDS,
            "fixture manifest omits public result fields")
    return manifest


def parse_cpu_list(value):
    cpus = set()
    for entry in value.split(","):
        left, dash, right = entry.partition("-")
        cpus.update(range(int(left), int(right) + 1) if dash else [int(left)])
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
    raise RuntimeError("runner needs allowed reciprocal SMT siblings and another core in the same package")


def case_specs(config):
    wide = config["wide"]
    preflight, standard = [], []
    for storage in wide["storage_modes"]:
        for size in wide["boundary_totals"]:
            preflight.append({"case_id": f"{storage}-boundary-{size}-hits-010",
                              "storage": storage, "profile": "byte-boundary", "requested_hits": 10,
                              "timed": False, "phase": "preflight", "boundary_total": size})
        for profile in ("vocalized-budget", "cross-line-long-neighbor"):
            preflight.append({"case_id": f"{storage}-{profile}-hits-002", "storage": storage,
                              "profile": profile, "requested_hits": 2,
                              "timed": False, "phase": "preflight"})
    for storage in wide["storage_modes"]:
        for profile in wide["profiles"]:
            for limit in wide["limits"]:
                standard.append({"case_id": f"{storage}-{profile}-hits-{limit:03}",
                                 "storage": storage, "profile": profile, "requested_hits": limit,
                                 "timed": limit in wide["timed_limits"][profile], "phase": "standard"})
    require(len(preflight) == 10 and len(standard) == 48, "case expansion changed")
    require(sum(case["timed"] for case in preflight + standard) == 14, "timed matrix changed")
    return preflight, standard


def ordered_specs(config, order):
    require(order in ("forward", "reverse"), "unknown wide case order")
    preflight, standard = case_specs(config)
    return preflight + standard if order == "forward" else list(reversed(preflight)) + list(reversed(standard))


def schedule(core, config, affinities, hashes=None):
    hashes = hashes or {}
    rows = []
    original = core.schedule()
    for block_index, first in enumerate(MAIN_FIRST):
        second = next(case for case in TOPOLOGIES if case != first)
        for precedence, topology in enumerate((first, second), 1):
            for old in original[block_index * 4:block_index * 4 + 4]:
                actual = PHASE1_MAP[old["actual_binary"]]
                rows.append({
                    "run": len(rows) + 1, "target": "indexing_throughput",
                    "case_run": old["run"], "block": PHASE1_BLOCKS[old["block"]],
                    "original_block": old["block"], "kind": old["kind"],
                    "order": "".join(PHASE1_MAP[x] for x in old["order"]),
                    "position": old["position"], "pseudo_label": PHASE1_MAP[old["pseudo_label"]],
                    "actual_binary": actual, "case_order": None,
                    "topology_case": topology, "topology_precedence": precedence,
                    "affinity": affinities[topology], "source_commit": config["revisions"][actual]["commit"],
                    "binary_sha256": hashes.get((actual, "indexing_throughput")),
                })
    for block_index, (name, kind, order, binary, first, directions) in enumerate(WIDE_BLOCKS):
        second = next(case for case in TOPOLOGIES if case != first)
        for precedence, topology in enumerate((first, second), 1):
            for position, pseudo in enumerate(order, 1):
                actual = binary or pseudo
                rows.append({
                    "run": len(rows) + 1, "target": config["wide"]["target"],
                    "case_run": block_index * 4 + position, "block": name, "kind": kind,
                    "order": order, "position": position, "pseudo_label": pseudo,
                    "actual_binary": actual, "case_order": directions[position - 1],
                    "topology_case": topology, "topology_precedence": precedence,
                    "affinity": affinities[topology], "source_commit": config["revisions"][actual]["commit"],
                    "binary_sha256": hashes.get((actual, config["wide"]["target"])),
                })
    return rows


def validate_schedule(core, config, rows, affinities, hashes=None):
    expected = schedule(core, config, affinities, hashes)
    require(len(rows) == 96, "schedule must have 96 processes")
    require(all({key: row[key] for key in plan} == plan for row, plan in zip(rows, expected)),
            "schedule differs from frozen plan")
    require([row["run"] for row in rows] == list(range(1, 97)), "non-contiguous run numbers")
    require(affinities[SIBLINGS][0] == affinities[DISTINCT][0]
            and len(set(affinities[SIBLINGS]) | set(affinities[DISTINCT])) == 3,
            "topologies must share one anchor and keep the same two-logical-CPU budget")
    for target, expected_count, per_real in (("indexing_throughput", 64, 8),
                                             (config["wide"]["target"], 32, 4)):
        selected = [row for row in rows if row["target"] == target]
        require(len(selected) == expected_count, "target process count changed")
        for topology in TOPOLOGIES:
            case = [row for row in selected if row["topology_case"] == topology]
            require(len(case) == expected_count // 2, "topology process count changed")
            real = [row for row in case if row["kind"] == "real"]
            for label in LABELS:
                require(sum(row["actual_binary"] == label for row in real) == per_real,
                        "unbalanced real binary count")
            for block in dict.fromkeys(row["block"] for row in case):
                group = [row for row in case if row["block"] == block]
                require(len(group) == 4 and [row["position"] for row in group] == [1, 2, 3, 4],
                        "incomplete or reordered block")
                if group[0]["kind"] == "control":
                    require(len({row["actual_binary"] for row in group}) == 1,
                            "mixed executable in a same-binary control")
                    require(len({row["binary_sha256"] for row in group}) == 1,
                            "mixed executable bytes in a same-binary control")
            if target != "indexing_throughput":
                for kind in ("real", "control"):
                    class_rows = [row for row in case if row["kind"] == kind]
                    for label in LABELS:
                        for field in ("actual_binary", "pseudo_label"):
                            group = [row for row in class_rows if row[field] == label]
                            require(sum(row["case_order"] == "forward" for row in group) == len(group) // 2,
                                    "wide case order is unbalanced within a binary/label marginal")
                    for position in range(1, 5):
                        group = [row for row in class_rows if row["position"] == position]
                        require(sum(row["case_order"] == "forward" for row in group) == len(group) // 2,
                                "wide case order is unbalanced within position")
                first_blocks = [row for row in case if row["position"] == 1 and row["topology_precedence"] == 1]
                require(len(first_blocks) == 2 and sum(row["kind"] == "real" for row in first_blocks) == 1,
                        "wide real/control topology precedence is unbalanced")


def normalize_graph(value, root):
    if isinstance(value, str):
        return value.replace(root.as_uri(), "file://$CHECKOUT")
    if isinstance(value, list):
        return [normalize_graph(item, root) for item in value]
    if isinstance(value, dict):
        return {key: normalize_graph(item, root) for key, item in value.items()}
    return value


def graph_digest(graph, root):
    canonical = json.dumps(normalize_graph(graph, root), sort_keys=True, separators=(",", ":")).encode()
    return byte_digest(canonical)


def dependency_graph(root, environment):
    raw = json.loads(run_text([
        "cargo", "metadata", "--manifest-path", str(root / "rust/Cargo.toml"),
        "--locked", "--format-version=1",
    ], environment, timeout=180))
    return sorted([
        {"id": node["id"], "features": sorted(node["features"]),
         "deps": sorted(node["deps"], key=lambda dep: (dep["name"], dep["pkg"]))}
        for node in raw["resolve"]["nodes"]
    ], key=lambda node: node["id"])


def source_provenance(root, label, config, overlays=None):
    revision = config["revisions"][label]
    require(git(root, "rev-parse", "HEAD") == revision["commit"], f"unexpected {label} commit")
    require(git(root, "rev-parse", "HEAD^{tree}") == revision["tree"], f"unexpected {label} tree")
    allowed = overlays or {}
    for line in git(root, "status", "--porcelain=v1", "--untracked-files=all").splitlines():
        require(line[:2] == "??" and line[3:] in allowed, f"unexpected {label} worktree change: {line}")
    for relative, expected in allowed.items():
        path = root / relative
        require(path.is_file() and not path.is_symlink() and file_digest(path) == expected,
                f"{label} overlay changed: {relative}")
        require(not git(root, "ls-files", "--", relative), f"overlay is tracked: {relative}")
    source = root / config["source_path"]
    require(file_digest(source) == revision["source_sha256"], f"{label} source bytes changed")
    require(git(root, "rev-parse", "HEAD:" + config["source_path"]) == revision["source_git_blob"],
            f"{label} source blob changed")
    expected_inputs = config["original"]["H_manifest_and_lock_sha256"]
    inputs = {name: file_digest(root / name) for name in expected_inputs}
    require(inputs == expected_inputs, f"{label} original fixture/manifest/lockfile changed")
    if label == "E":
        parents = git(root, "rev-list", "--parents", "-n", "1", "HEAD").split()
        require(parents == [E_COMMIT, H_COMMIT], "E must have H as its only parent")
        require(git(root, "diff", "--name-only", H_COMMIT, E_COMMIT) == config["source_path"],
                "E changes files outside the frozen source delta")
    return {"commit": revision["commit"], "tree": revision["tree"],
            "source_git_blob": revision["source_git_blob"], "source_sha256": revision["source_sha256"],
            "tracked_source_clean": True, "files_sha256": inputs,
            "explicit_untracked_overlays": allowed}


def remaining_check(deadline, requested, phase):
    require(time.monotonic() + requested < deadline,
            f"controller budget exhausted before {phase}; preserving a complete prefix")


def build(core, root, label, config, wide_fixture, output, build_root, environment, deadline):
    directory = output / "build-logs" / label
    directory.mkdir(parents=True)
    before = source_provenance(root, label, config)
    env = environment | {"CARGO_TARGET_DIR": str(build_root / label)}
    require(not (build_root / label).exists(), f"{label} target directory is not fresh")
    graph_before = dependency_graph(root, env)
    write_json(directory / "dependencies-before.json", graph_before)
    normalized_digest = graph_digest(graph_before, root)
    require(normalized_digest == config["original"]["normalized_dependency_graph_sha256"],
            f"{label} dependencies/features differ from immutable H after path normalization")
    layout_relative = f"rust/examples/{core.LAYOUT_TARGET}.rs"
    wide_relative = config["wide"]["overlay_path"]
    contents = {layout_relative: core.LAYOUT_SOURCE.encode(), wide_relative: wide_fixture.read_bytes()}
    overlays = {relative: byte_digest(data) for relative, data in contents.items()}
    require(overlays[layout_relative] == config["original"]["layout_source_sha256"], "layout source changed")
    require(overlays[wide_relative] == config["wide"]["source_sha256"], "wide fixture source changed")
    for relative in contents:
        require(not (root / relative).exists(), f"refusing to overwrite source path {relative}")
        require(not git(root, "ls-files", "--", relative), f"overlay path is already tracked: {relative}")
    write_json(directory / "source-before.json", before)
    write_json(directory / "overlays.json", {"tracked_source_changed": False, "files_sha256": overlays})
    created = []
    record = None
    try:
        for relative, data in contents.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            created.append(path)
        source_provenance(root, label, config, overlays)
        graph_after = dependency_graph(root, env)
        write_json(directory / "dependencies-with-overlays.json", graph_after)
        require(graph_after == graph_before, f"{label} overlays changed dependency features")
        command = ["cargo", "test", "--manifest-path", str(root / "rust/Cargo.toml"),
                   "--locked", "--release", "--test", "indexing_throughput",
                   "--test", config["wide"]["target"], "--example", core.LAYOUT_TARGET,
                   "--no-run", "--message-format=json"]
        write_json(directory / "command.json", command)
        write_json(directory / "build-environment.json", {"CARGO_TARGET_DIR": env["CARGO_TARGET_DIR"],
                   "CARGO_BUILD_JOBS": env["CARGO_BUILD_JOBS"], "timeout_seconds": 1200})
        remaining_check(deadline, 1200, f"{label} build")
        print(f"Building immutable {label} once: {config['revisions'][label]['commit']}", flush=True)
        started, completed = time.monotonic(), None
        try:
            with (directory / "cargo.jsonl").open("w") as stdout, (directory / "cargo.stderr.log").open("w") as stderr:
                completed = subprocess.run(command, stdout=stdout, stderr=stderr,
                                           env=env, timeout=1200, check=False)
        finally:
            write_json(directory / "duration.json", {"wall_seconds": time.monotonic() - started,
                       "returncode": completed.returncode if completed is not None else None})
        completed.check_returncode()
        selected = {}
        wanted = {"indexing_throughput", config["wide"]["target"], core.LAYOUT_TARGET}
        for line in (directory / "cargo.jsonl").read_text().splitlines():
            message = json.loads(line)
            if message.get("reason") != "compiler-artifact" or not message.get("executable"):
                continue
            name = message["target"]["name"]
            if name in wanted:
                require(name not in selected, f"duplicate compiler artifact for {name}")
                selected[name] = message
        require(set(selected) == wanted, f"missing exact compiler targets for {label}")
        write_json(directory / "selected-artifacts.json", selected)
        binaries = {}
        for target in ("indexing_throughput", config["wide"]["target"]):
            path = output / "binaries" / f"{label}-{target}"
            path.parent.mkdir(exist_ok=True)
            shutil.copy2(selected[target]["executable"], path)
            binaries[target] = {"path": str(path), "sha256": file_digest(path), "bytes": path.stat().st_size}
        layout_executable = Path(selected[core.LAYOUT_TARGET]["executable"])
        remaining_check(deadline, 60, f"{label} layout inspection")
        with (directory / "layout.log").open("w") as log:
            subprocess.run([str(layout_executable), "layout_metadata", "--exact", "--nocapture", "--test-threads=1"],
                           stdout=log, stderr=subprocess.STDOUT, env=env, timeout=60, check=True)
        layout = core.extract_marker(directory / "layout.log", "OTZARIA_LAYOUT ")
        require(layout == config["original"]["H_original_layout"], f"{label} SearchEngine layout changed")
        source_provenance(root, label, config, overlays)
        record = before | {"build_invocations": 1, "overlays": overlays,
                          "binary_targets": binaries, "layout": layout,
                          "layout_executable_sha256": file_digest(layout_executable),
                          "normalized_dependency_graph_sha256": normalized_digest,
                          "resolved_graph_sha256": file_digest(directory / "dependencies-before.json"),
                          "build_wall_seconds": time.monotonic() - started}
        if label == "H":
            observed = binaries["indexing_throughput"]["sha256"]
            expected = config["original"]["H_original_executable_sha256"]
            record["original_executable_identity"] = {"observed_sha256": observed, "expected_sha256": expected,
                "matches": observed == expected,
                "policy": "Record the fresh binary identity; a mismatch is not relabeled as the original executable."}
        write_json(directory / "provenance.json", record)
    finally:
        # Only paths created by this invocation can be removed. Preserve their
        # bytes in the artifact; canonical source and user files are untouched.
        for path in created:
            if path.exists():
                path.unlink()
        after = source_provenance(root, label, config)
        write_json(directory / "source-after.json", after)
        require(after == before, f"{label} pure source changed during build")
    return record


def snapshot_results(data, expected_count):
    results = json.loads(data)
    require(isinstance(results, list) and len(results) == expected_count, "snapshot result count changed")
    require(len({row.get("id") for row in results}) == len(results), "duplicate result IDs in snapshot")
    for row in results:
        require(isinstance(row, dict) and set(row) == RESULT_FIELDS, "snapshot omits or adds SearchResult fields")
        for key in ("title", "reference", "text", "file_path"):
            require(isinstance(row[key], str), f"invalid snapshot string {key}")
        for key in ("id", "segment", "merged_count"):
            require(type(row[key]) is int and row[key] >= 0, f"invalid snapshot integer {key}")
        require(type(row["is_pdf"]) is bool and type(row["continues_to_next_line"]) is bool,
                "invalid snapshot booleans")
        require(row["text_status"] in ("Ok", "Stale", "Unavailable"), "invalid snapshot text status")
        require(isinstance(row["merged"], list), "invalid merged sibling list")
        for sibling in row["merged"]:
            require(isinstance(sibling, dict) and set(sibling) == SIBLING_FIELDS,
                    "snapshot omits or adds MergedSibling fields")
            require(all(isinstance(sibling[key], str) for key in ("title", "reference", "file_path")),
                    "invalid sibling string fields")
            require(all(type(sibling[key]) is int and sibling[key] >= 0 for key in ("id", "segment"))
                    and type(sibling["is_pdf"]) is bool, "invalid sibling scalar fields")
    return results


def percentile(values, fraction):
    return sorted(values)[math.ceil((len(values) - 1) * fraction)]


def validate_case(record, spec, config, snapshot_data):
    require(record["case_id"] == spec["case_id"], "unexpected case identity/order")
    require(record["storage"] == spec["storage"], "unexpected case storage")
    if spec["phase"] == "standard":
        require(record["profile"] == spec["profile"], "unexpected standard profile")
    require(record["timed"] is spec["timed"], "timed and behavioral cases were mixed")
    require(record["vocalized_display"] is (spec["profile"] == "vocalized-budget"),
            "vocalized display escaped its behavior-only preflight")
    require(record["requested_hits"] == record["result_count"] == spec["requested_hits"],
            "case does not return its exact requested result window")
    require(record["snapshot_file"] == spec["case_id"] + ".json", "unsafe or unexpected snapshot filename")
    require(record["post_timing_validation_queries"] == (1 if spec["timed"] else 0)
            and record["post_timing_snapshot_verified"] is spec["timed"],
            "explicit post-timing snapshot validation changed")
    results = snapshot_results(snapshot_data, spec["requested_hits"])
    require(all(row["text_status"] == "Ok" for row in results), "wide fixture returned stale/unavailable text")
    values = record["latency_ms"]
    require(isinstance(values, list), "case latency vector is not a list")
    if spec["timed"]:
        require(record["warmup_queries"] == config["wide"]["warmup_queries"], "warmup count changed")
        require(record["measured_queries"] == len(values) == config["wide"]["measured_queries"],
                "measured query count changed")
        require(all(type(value) in (int, float) and math.isfinite(value) and value > 0 for value in values),
                "invalid raw query latency")
        for key, fraction in (("p50_ms", 0.5), ("p95_ms", 0.95), ("p99_ms", 0.99)):
            require(record[key] == percentile(values, fraction), f"{key} differs from raw-vector percentile")
    else:
        require(record["warmup_queries"] == record["measured_queries"] == 0 and values == [],
                "behavior-only case acquired timed observations")
        require(all(record[key] is None for key in ("p50_ms", "p95_ms", "p99_ms")),
                "behavior-only case acquired percentiles")
    totals = {}
    for array_name, total_name in (("line_bytes", "total_line_bytes"),
                                    ("vocalized_line_bytes", "total_vocalized_bytes"),
                                    ("next_line_bytes", "total_next_line_bytes")):
        lengths = record[array_name]
        require(isinstance(lengths, list) and len(lengths) == spec["requested_hits"],
                f"invalid per-hit byte inventory {array_name}")
        require(all(type(value) is int and value >= 0 for value in lengths),
                f"invalid byte length in {array_name}")
        totals[total_name] = sum(lengths)
        require(record[total_name] == totals[total_name], f"incorrect byte total {total_name}")
    require(record["total_render_bytes"] == sum(totals.values()), "incorrect complete render byte total")
    if "boundary_total" in spec:
        require(record["total_render_bytes"] == spec["boundary_total"], "byte-boundary case drifted")
    if spec["profile"] == "cross-line-long-neighbor":
        require(record["next_line_bytes"] == [65536, 65536]
                and record["total_line_bytes"] < 16384
                and all(row["continues_to_next_line"] for row in results),
                "long-neighbor case does not cover short current lines and long cross-line text")
    if spec["profile"] == "vocalized-budget":
        require(record["total_line_bytes"] < 16384 < record["total_render_bytes"]
                and record["total_vocalized_bytes"] > 0,
                "optional vocalized text does not cross the rendering budget")
    return record | {"snapshot_sha256": byte_digest(snapshot_data), "snapshot_bytes": len(snapshot_data)}


def parse_markers(log, config):
    cases, complete, errors = [], [], []
    for number, line in enumerate(log.read_text().splitlines(), 1):
        for marker, destination in ((config["wide"]["case_marker"], cases),
                                     (config["wide"]["complete_marker"], complete)):
            if marker in line:
                try:
                    destination.append(json.loads(line.partition(marker)[2]))
                except (ValueError, TypeError) as error:
                    errors.append({"line": number, "marker": marker.strip(), "error": str(error)})
    return {"cases": cases, "complete_records": complete, "parse_errors": errors}


def validate_complete(parsed, entry, config, snapshot_directory):
    require(not parsed["parse_errors"], "wide fixture emitted malformed JSON")
    require(len(parsed["complete_records"]) == 1, "wide fixture must emit one completion record")
    complete = parsed["complete_records"][0]
    specs = ordered_specs(config, entry["case_order"])
    expected_ids = [case["case_id"] for case in specs]
    require(len(parsed["cases"]) == len(specs), "wide fixture omitted or duplicated cases")
    require([case["case_id"] for case in parsed["cases"]] == expected_ids,
            "wide case order differs from its predeclared direction")
    require(complete["schema_version"] == 1 and complete["case_order"] == entry["case_order"],
            "wide completion schema/order changed")
    require(complete["completed_timed_cases"] == 14 and complete["completed_behavior_cases"] == 44
            and complete["total_cases"] == 58, "wide completion counts changed")
    require(complete["ordered_case_ids"] == expected_ids, "completion inventory differs from case records")
    require(complete["timed_case_ids"] == [case["case_id"] for case in specs if case["timed"]]
            and complete["behavior_case_ids"] == [case["case_id"] for case in specs if not case["timed"]],
            "completion changed the timed/behavioral matrix")
    require(complete["warmup_queries"] == 5 and complete["measured_queries"] == 30,
            "wide completion changed fixed query counts")
    require(complete["post_timing_validation_queries_per_timed_case"] == 1,
            "wide completion omitted post-timing output validation")
    require(complete["cpus"] == 2 and complete["indexing_overlapped"] is False,
            "wide fixture must remain a two-logical-CPU idle-search measurement")
    require(complete["fixture_source_sha256"] == config["wide"]["source_sha256"], "wide source self-identity differs")
    require(complete["default_vocalized_display"] is False, "vocalized display default was not restored")
    require(all(type(complete[key]) is int and complete[key] > 0 for key in ("rss_before_kib", "peak_rss_kib")),
            "wide process RSS measurements are missing")
    expected_files = {case["case_id"] + ".json" for case in specs}
    require({path.name for path in snapshot_directory.iterdir()} == expected_files,
            "snapshot directory inventory changed")
    verified = []
    for record, spec in zip(parsed["cases"], specs):
        path = snapshot_directory / (spec["case_id"] + ".json")
        require(path.is_file() and not path.is_symlink(), "invalid snapshot file")
        verified.append(validate_case(record, spec, config, path.read_bytes()))
    return verified, complete


def measure_wide(core, entry, binary, output, environment, config):
    stem = f"{entry['run']:02d}-{entry['block']}-{entry['position']}"
    run_directory = output / "runs"
    run_directory.mkdir(parents=True, exist_ok=True)
    snapshots = run_directory / (stem + ".snapshots")
    snapshots.mkdir(exist_ok=False)
    env = environment | {config["wide"]["order_environment"]: entry["case_order"],
                         config["wide"]["snapshot_environment"]: str(snapshots)}
    command = ["taskset", "-c", ",".join(map(str, entry["affinity"])), str(binary),
               config["wide"]["test_name"], "--ignored", "--exact", "--nocapture", "--test-threads=1"]
    context = {"schedule": entry, "command": command, "before": core.snapshot(),
               "explicit_runtime_environment": {key: env[key] for key in
                   (config["wide"]["order_environment"], config["wide"]["snapshot_environment"])},
               "scope": "Idle searches after indexing; case validation and snapshot serialization are outside each query timer.",
               "timeout_seconds": 180}
    context_path = run_directory / (stem + ".context.json")
    log_path = run_directory / (stem + ".stdout.log")
    write_json(context_path, context)
    usage_before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started, completed = time.monotonic(), None
    try:
        with log_path.open("w") as log:
            completed = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT,
                                       env=env, timeout=180, check=False)
    finally:
        usage_after = resource.getrusage(resource.RUSAGE_CHILDREN)
        context["process_wall_seconds"] = time.monotonic() - started
        context["whole_process_cpu_user_seconds"] = usage_after.ru_utime - usage_before.ru_utime
        context["whole_process_cpu_system_seconds"] = usage_after.ru_stime - usage_before.ru_stime
        context["returncode"] = completed.returncode if completed is not None else None
        context["after"] = core.snapshot()
        context["cpu_scope"] = "Whole wide fixture: setup/indexing, idle queries, warmups, behavioral checks, serialization and cleanup."
        write_json(context_path, context)
        if log_path.exists():
            write_json(run_directory / (stem + ".partial-records.json"), parse_markers(log_path, config))
    completed.check_returncode()
    parsed = parse_markers(log_path, config)
    cases, complete = validate_complete(parsed, entry, config, snapshots)
    context.update({key: complete[key] for key in ("rss_before_kib", "peak_rss_kib")})
    write_json(context_path, context)
    record = entry | {"cases": cases, "completion": complete, "process": context,
                      "snapshot_directory": str(snapshots)}
    write_json(run_directory / (stem + ".verified-record.json"), record)
    return record


def compare_snapshots(record, references, output):
    compared = []
    for case in record["cases"]:
        key = case["case_id"]
        path = Path(record["snapshot_directory"]) / case["snapshot_file"]
        data = path.read_bytes()
        if key not in references:
            require(record["actual_binary"] == "H", "first wide reference must use fresh H")
            references[key] = {"run": record["run"], "actual_binary": "H", "path": str(path),
                               "source_commit": H_COMMIT, "sha256": byte_digest(data), "bytes": len(data)}
        expected = references[key]
        matches = data == Path(expected["path"]).read_bytes()
        result = {"run": record["run"], "case_id": key, "actual_binary": record["actual_binary"],
                  "reference_run": expected["run"], "expected_sha256": expected["sha256"],
                  "observed_sha256": byte_digest(data), "equal_full_bytes": matches}
        compared.append(result)
        if not matches:
            write_json(output / "snapshot-mismatch.json", result | {"expected_path": expected["path"], "observed_path": str(path)})
            raise RuntimeError(f"full output changed for {key} at run {record['run']}")
    write_json(output / "snapshot-references.json", references)
    return compared


def metric_medians(rows, keys, field):
    return {key: statistics.median(row[field][key] for row in rows) if rows else None for key in keys}


def ratio(numerator, denominator):
    return numerator / denominator if numerator is not None and denominator else None


def process_summary(rows):
    keys = ("process_wall_seconds", "whole_process_cpu_user_seconds", "whole_process_cpu_system_seconds")
    summary = {key: statistics.median(row["process"][key] for row in rows) if rows else None for key in keys}
    summary["whole_process_cpu_total_seconds"] = statistics.median(
        row["process"]["whole_process_cpu_user_seconds"] + row["process"]["whole_process_cpu_system_seconds"]
        for row in rows) if rows else None
    for key in ("rss_before_kib", "peak_rss_kib"):
        if rows and all(key in row["process"] for row in rows):
            summary[key] = statistics.median(row["process"][key] for row in rows)
    return summary


def main_analysis(core, records):
    output = {}
    for topology in TOPOLOGIES:
        rows = [row for row in records if row["target"] == "indexing_throughput" and row["topology_case"] == topology]
        real = [row for row in rows if row["kind"] == "real"]
        medians = {label: core.medians([row for row in real if row["actual_binary"] == label]) for label in LABELS}
        blocks = []
        for name in dict.fromkeys(row["block"] for row in rows):
            group = [row for row in rows if row["block"] == name]
            if len(group) != 4:
                continue
            labels = {label: core.medians([row for row in group if row["pseudo_label"] == label]) for label in LABELS}
            blocks.append({"block": name, "kind": group[0]["kind"], "order": group[0]["order"],
                           "actual_binaries": sorted({row["actual_binary"] for row in group}),
                           "pseudo_label_medians": labels,
                           "pseudo_E_over_H": {key: ratio(labels["E"][key], labels["H"][key]) for key in core.METRICS}})
        output[topology] = {"completed_processes": len(rows), "real_binary_medians": medians,
                            "real_process_counts": {label: sum(row["actual_binary"] == label for row in real) for label in LABELS},
                            "real_E_over_H": {key: ratio(medians["E"][key], medians["H"][key]) for key in core.METRICS},
                            "real_whole_process_medians": {label: process_summary([row for row in real if row["actual_binary"] == label]) for label in LABELS},
                            "blocks": blocks}
    return output


def wide_analysis(config, records):
    output = {}
    timed_ids = [case["case_id"] for phase in case_specs(config) for case in phase if case["timed"]]
    for topology in TOPOLOGIES:
        processes = [row for row in records if row["target"] == config["wide"]["target"] and row["topology_case"] == topology]
        cases = {}
        for case_id in timed_ids:
            rows = [process | {"case_sample": next(case for case in process["cases"] if case["case_id"] == case_id)}
                    for process in processes]
            real = [row for row in rows if row["kind"] == "real"]
            medians = {label: metric_medians([row for row in real if row["actual_binary"] == label], WIDE_METRICS, "case_sample")
                       for label in LABELS}
            blocks = []
            for name in dict.fromkeys(row["block"] for row in rows):
                group = [row for row in rows if row["block"] == name]
                if len(group) != 4:
                    continue
                labels = {label: metric_medians([row for row in group if row["pseudo_label"] == label], WIDE_METRICS, "case_sample")
                          for label in LABELS}
                blocks.append({"block": name, "kind": group[0]["kind"], "order": group[0]["order"],
                               "actual_binaries": sorted({row["actual_binary"] for row in group}),
                               "pseudo_label_medians": labels,
                               "pseudo_E_over_H": {key: ratio(labels["E"][key], labels["H"][key]) for key in WIDE_METRICS}})
            cases[case_id] = {"real_binary_medians": medians,
                              "real_process_counts": {label: sum(row["actual_binary"] == label for row in real) for label in LABELS},
                              "real_E_over_H": {key: ratio(medians["E"][key], medians["H"][key]) for key in WIDE_METRICS},
                              "blocks": blocks}
        real_processes = [row for row in processes if row["kind"] == "real"]
        output[topology] = {"completed_processes": len(processes), "cases": cases,
                           "real_whole_process_medians": {label: process_summary([row for row in real_processes if row["actual_binary"] == label]) for label in LABELS}}
    return output


def analyse(core, config, records):
    return {
        "completed_processes": len(records), "schedule_complete": len(records) == 96,
        "concurrent_original_target": main_analysis(core, records),
        "idle_wide_target": wide_analysis(config, records),
        "interpretation": [
            "The original concurrent fixture and wider idle fixture are separate targets; do not pool them.",
            "Each process supplies one percentile observation per timed case. Controls remain separate from real binary comparisons.",
            "Main real counts are eight per binary per topology; wide counts are four per binary per topology, each with the same fourteen timed cases.",
            "Wide p99 is the maximum of thirty timed queries; no precise tail or significance claim follows from these small samples.",
            "All forty-four untimed wide cases validate behavior only and contribute no latency statistics.",
            "Full serialized outputs are compared byte-for-byte after each wide process, outside query timing; every result and sibling field is preserved.",
            "Main indexing throughput is coupled to its closed query loop. Wide processes finish indexing before any query timing.",
            "Whole-process CPU/PSI includes setup, indexing, warmups, validation, output and cleanup, not isolated query CPU.",
            "Guest-visible CPU topology and model are recorded. Unchanged H source is rebuilt; original binary identity is checked, never assumed.",
            "No timing threshold, causal verdict, branch promotion or PR readiness change is automated.",
        ],
    }


def fake_case(spec, config, value):
    """Small protocol fixtures only; never execute or model a Rust benchmark."""
    count = spec["requested_hits"]
    results = [{"title": "fixture", "reference": str(index), "text": "<b>query</b>",
                "id": index + 1, "segment": index, "is_pdf": False,
                "file_path": "id:1", "merged_count": 1, "merged": [], "text_status": "Ok",
                "continues_to_next_line": spec["profile"] == "cross-line-long-neighbor"}
               for index in range(count)]
    data = json.dumps(results, separators=(",", ":")).encode()
    plain, vocalized, following = [1024] * count, [0] * count, [0] * count
    if "boundary_total" in spec:
        total = spec["boundary_total"]
        plain = [total // count + (index < total % count) for index in range(count)]
    elif spec["profile"] == "vocalized-budget":
        plain, vocalized = [4450] * count, [6050] * count
    elif spec["profile"] == "cross-line-long-neighbor":
        following = [65536] * count
    latencies = [value] * 30 if spec["timed"] else []
    record = {"case_id": spec["case_id"], "storage": spec["storage"], "profile": spec["profile"],
              "timed": spec["timed"], "vocalized_display": spec["profile"] == "vocalized-budget",
              "requested_hits": count, "result_count": count, "snapshot_file": spec["case_id"] + ".json",
              "warmup_queries": 5 if spec["timed"] else 0, "measured_queries": len(latencies),
              "post_timing_validation_queries": 1 if spec["timed"] else 0,
              "post_timing_snapshot_verified": spec["timed"],
              "latency_ms": latencies, "p50_ms": value if latencies else None,
              "p95_ms": value if latencies else None, "p99_ms": value if latencies else None,
              "line_bytes": plain, "vocalized_line_bytes": vocalized, "next_line_bytes": following,
              "total_line_bytes": sum(plain), "total_vocalized_bytes": sum(vocalized),
              "total_next_line_bytes": sum(following),
              "total_render_bytes": sum(plain) + sum(vocalized) + sum(following)}
    return record, data


def expect_rejected(function, reason):
    try:
        function()
    except (RuntimeError, AssertionError, KeyError):
        return
    raise AssertionError("negative protocol test was accepted: " + reason)


def self_test(core, config):
    core.self_test()
    topology = {
        0: {"physical_package_id": "0", "core_id": "0", "thread_siblings_list": "0-1"},
        1: {"physical_package_id": "0", "core_id": "0", "thread_siblings_list": "0-1"},
        2: {"physical_package_id": "0", "core_id": "1", "thread_siblings_list": "2-3"},
        3: {"physical_package_id": "0", "core_id": "1", "thread_siblings_list": "2-3"},
    }
    affinities = select_affinities(topology)
    require(affinities == {SIBLINGS: [0, 1], DISTINCT: [0, 2]}, "topology selection failed")
    hashes = {(label, target): byte_digest((label + target).encode())
              for label in LABELS for target in ("indexing_throughput", config["wide"]["target"])}
    rows = schedule(core, config, affinities, hashes)
    validate_schedule(core, config, rows, affinities, hashes)
    validate_schedule(core, config, schedule(core, config, affinities), affinities)
    specs = ordered_specs(config, "forward")
    fake_cases = {}
    for label, value in (("H", 1.0), ("E", 2.0)):
        fake_cases[label] = []
        for spec in specs:
            record, data = fake_case(spec, config, value)
            fake_cases[label].append(validate_case(record, spec, config, data))
    records = []
    for row in rows:
        value = 2 if row["actual_binary"] == "E" else 1
        record = row | {"process": {"process_wall_seconds": float(value),
                                    "whole_process_cpu_user_seconds": float(value),
                                    "whole_process_cpu_system_seconds": 0.0}}
        if row["target"] == "indexing_throughput":
            record["sample"] = {key: float(value) for key in core.METRICS}
        else:
            record["cases"] = fake_cases[row["actual_binary"]]
        records.append(record)
    report = analyse(core, config, records)
    require(report["schedule_complete"], "full schedule was treated as partial")
    for topology_name in TOPOLOGIES:
        main = report["concurrent_original_target"][topology_name]
        require(main["real_process_counts"] == {"H": 8, "E": 8}, "main real counts mixed controls")
        require(main["real_E_over_H"]["search_during_p95_ms"] == 2, "main real ratio calculation failed")
        for block in main["blocks"]:
            require(block["pseudo_E_over_H"]["search_during_p95_ms"] == (2 if block["kind"] == "real" else 1),
                    "main controls were mixed into real differences")
        wide = report["idle_wide_target"][topology_name]
        require(len(wide["cases"]) == 14, "behavior cases entered timing analysis")
        for case in wide["cases"].values():
            require(case["real_process_counts"] == {"H": 4, "E": 4}, "wide real counts mixed controls")
            require(case["real_E_over_H"]["p95_ms"] == 2, "wide real ratio calculation failed")
            for block in case["blocks"]:
                require(block["pseudo_E_over_H"]["p95_ms"] == (2 if block["kind"] == "real" else 1),
                        "wide controls were mixed into real differences")
    for prefix in range(len(records) + 1):
        partial = analyse(core, config, records[:prefix])
        require(partial["completed_processes"] == prefix, "partial-prefix analysis lost records")
        require(partial["schedule_complete"] is (prefix == 96), "partial-prefix analysis claimed completeness")
    broken = copy.deepcopy(rows)
    control = next(row for row in broken if row["kind"] == "control")
    control["actual_binary"] = next(label for label in LABELS if label != control["actual_binary"])
    expect_rejected(lambda: validate_schedule(core, config, broken, affinities, hashes), "mixed-binary control")
    broken = copy.deepcopy(rows)
    broken[64]["case_order"] = "reverse"
    expect_rejected(lambda: validate_schedule(core, config, broken, affinities, hashes), "wide case-order imbalance")
    expect_rejected(lambda: select_affinities({key: topology[key] for key in (0, 1)}), "missing distinct core")
    contradictory = {key: value | {"thread_siblings_list": "0-2"}
                     for key, value in topology.items() if key in (0, 1, 2)}
    expect_rejected(lambda: select_affinities(contradictory), "contradictory core IDs and sibling lists")
    spec = next(case for case in specs if case["timed"])
    record, data = fake_case(spec, config, 1.0)
    changed = dict(record, p95_ms=99)
    expect_rejected(lambda: validate_case(changed, spec, config, data), "percentile/raw-vector disagreement")
    missing_field = json.loads(data)
    missing_field[0].pop("continues_to_next_line")
    expect_rejected(lambda: snapshot_results(json.dumps(missing_field), spec["requested_hits"]), "omitted public result field")
    untimed = next(case for case in specs if not case["timed"])
    untimed_record, untimed_data = fake_case(untimed, config, 1.0)
    expect_rejected(lambda: validate_case(untimed_record | {"p95_ms": 1}, untimed, config, untimed_data),
                    "timing leaked into behavioral case")
    preflight, standard = case_specs(config)
    require([case["case_id"] for case in ordered_specs(config, "reverse")]
            == [case["case_id"] for case in list(reversed(preflight)) + list(reversed(standard))],
            "reverse case order changed phase boundaries")
    require(percentile(list(range(1, 31)), 0.99) == 30, "wide p99 definition changed")
    result = {"state": "pass", "main_processes": 64, "wide_processes": 32,
              "all_97_prefixes_checked": True, "wide_cases": 58, "timed_cases": 14,
              "same_binary_control_ratios_checked": True,
              "negative_checks": ["mixed control", "case order", "missing topology", "contradictory topology", "percentile mismatch",
                                  "omitted SearchResult field", "behavior timing"],
              "rust_builds": 0, "fixture_processes": 0}
    print("PASS: fixed 96-process protocol, 58-case schema, controls, negative checks and all 97 prefixes", flush=True)
    return result


def compiler_environment(config):
    blocked_names = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                     "RUSTDOCFLAGS", "RAYON_NUM_THREADS", "RAYON_RS_NUM_CPUS", "RUSTC", "CARGO_BUILD_TARGET"}
    overrides = [key for key, value in os.environ.items() if value and
                 (key in blocked_names or key.startswith("CARGO_PROFILE_RELEASE_")
                  or (key.startswith("CARGO_TARGET_") and key.endswith("_RUSTFLAGS")))]
    require(not overrides, f"unexpected compiler/worker override names: {sorted(overrides)}")
    environment = os.environ | {"CARGO_BUILD_JOBS": "1", "OTZARIA_BENCH_LINES": "12000", "OTZARIA_BENCH_ECONOMY": "1"}
    environment.pop("OTZARIA_BENCH_NOTES", None)
    for key in (config["wide"]["order_environment"], config["wide"]["snapshot_environment"]):
        environment.pop(key, None)
    rustc, cargo = run_text(["rustc", "-vV"], environment), run_text(["cargo", "--version"], environment)
    require(rustc == config["original"]["rustc"] and cargo == config["original"]["cargo"],
            "exact immutable Rust/Cargo 1.99.1 toolchain identity required")
    return environment, rustc, cargo


def write_checksums(output):
    files = sorted(path for path in output.rglob("*") if path.is_file() and path != output / "SHA256SUMS")
    (output / "SHA256SUMS").write_text("".join(f"{file_digest(path)}  {path.relative_to(output)}\n" for path in files))


def interrupt_signal(number, frame):
    raise InterruptedError(f"diagnostic received signal {number}; preserving partial evidence")


def main():
    here = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--head", type=Path)
    parser.add_argument("--candidate", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--build-root", type=Path)
    parser.add_argument("--config", type=Path, default=here / "config.json")
    parser.add_argument("--helper", type=Path, default=here / "phase1_helper.py")
    parser.add_argument("--wide-fixture", type=Path, default=here / "render_scheduling.rs")
    parser.add_argument("--fixture-manifest", type=Path, default=here / "fixture-manifest.json")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if sys.flags.optimize:
        parser.error("run without Python optimization so protocol checks and frozen helper assertions stay active")
    if args.self_test:
        config = load_config(args.config, frozen=False)
        core = import_helper(args.helper)
        load_fixture_manifest(args.fixture_manifest, config)
        self_test(core, config)
        return
    if any(getattr(args, key) is None for key in ("head", "candidate", "out", "build_root")):
        parser.error("--head, --candidate, --out and --build-root are required")
    head, candidate, output, build_root = (getattr(args, key).resolve() for key in ("head", "candidate", "out", "build_root"))
    require(head != candidate, "H and E require separate clean checkouts")
    require(not (output == build_root or output.is_relative_to(build_root) or build_root.is_relative_to(output)),
            "artifact output and build caches must be separate")
    for root in (head, candidate):
        require(not any(path == root or path.is_relative_to(root) for path in (output, build_root)),
                "artifacts/builds must be outside canonical source checkouts")
    require(not build_root.exists(), "clean build root already exists")
    output.mkdir(parents=True, exist_ok=False)
    signal.signal(signal.SIGTERM, interrupt_signal)
    deadline = time.monotonic() + 3300
    status = {"state": "running", "phase": "preflight", "completed_processes": 0,
              "completed_main_processes": 0, "completed_wide_processes": 0,
              "measured_process_attempts": 0, "build_invocations": {"H": 0, "E": 0},
              "performance_gate": "none; independent interpretation and behavioral gates are required"}
    records, snapshot_references, comparisons = [], {}, []
    core, config = None, None
    write_json(output / "status.json", status)
    try:
        shutil.copy2(__file__, output / "render-budget-controller.py")
        shutil.copy2(args.config, output / "config.json")
        shutil.copy2(args.helper, output / "phase1-helper.py")
        config = load_config(args.config, frozen=True)
        core = import_helper(args.helper)
        load_fixture_manifest(args.fixture_manifest, config)
        shutil.copy2(args.fixture_manifest, output / "wide-fixture-manifest.json")
        require(file_digest(args.wide_fixture) == config["wide"]["source_sha256"], "wide source hash changed")
        shutil.copy2(args.wide_fixture, output / "render_scheduling.rs")
        shutil.copy2(head / config["original"]["fixture_path"], output / "indexing_throughput.rs")
        (output / "layout_metadata.rs").write_text(core.LAYOUT_SOURCE)
        write_json(output / "self-test.json", self_test(core, config))
        initial_sources = {}
        for label, root in (("H", head), ("E", candidate)):
            initial_sources[label] = source_provenance(root, label, config)
            source_output = output / "source" / f"{label}-search_engine.rs"
            source_output.parent.mkdir(exist_ok=True)
            shutil.copy2(root / config["source_path"], source_output)
        write_json(output / "source-provenance.json", initial_sources)
        environment, rustc, cargo = compiler_environment(config)
        allowed = sorted(os.sched_getaffinity(0))
        topology = {cpu: {key: Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/{key}").read_text().strip()
                          for key in ("physical_package_id", "core_id", "thread_siblings_list", "core_siblings_list")}
                    for cpu in allowed}
        affinities = select_affinities(topology)
        rows = schedule(core, config, affinities)
        validate_schedule(core, config, rows, affinities)
        write_json(output / "predeclared-schedule.json", rows)
        write_json(output / "wide-case-inventory.json", {order: ordered_specs(config, order) for order in ("forward", "reverse")})
        expected_stamps = [[int(value) for value in row]
                           for row in config["original"]["expected_document_stamps_decimal_strings"]]
        tmpdir = Path(tempfile.gettempdir())
        metadata = {
            "revisions": config["revisions"], "original": config["original"],
            "controller_commit": git(Path(__file__).resolve().parents[1], "rev-parse", "HEAD"),
            "controller_sha256": file_digest(output / "render-budget-controller.py"),
            "config_sha256": file_digest(output / "config.json"),
            "verified_helper_sha256": HELPER_SHA256,
            "original_fixture_sha256": file_digest(output / "indexing_throughput.rs"),
            "wide_fixture_sha256": file_digest(output / "render_scheduling.rs"),
            "wide_fixture_manifest_sha256": file_digest(output / "wide-fixture-manifest.json"),
            "layout_source_sha256": file_digest(output / "layout_metadata.rs"),
            "predeclared_schedule_sha256": file_digest(output / "predeclared-schedule.json"),
            "wide_case_inventory_sha256": file_digest(output / "wide-case-inventory.json"),
            "platform": platform.platform(), "uname": list(platform.uname()), "python": sys.version,
            "allowed_affinity": allowed, "topology": topology, "measurement_affinities": affinities,
            "cpu_topology": json.loads(run_text(["lscpu", "--json"])),
            "rustc": rustc, "cargo": cargo, "tmpdir": str(tmpdir),
            "tmpdir_filesystem": run_text(["df", "-T", str(tmpdir)]),
            "runtime_environment": {key: environment[key] for key in ("CARGO_BUILD_JOBS", "OTZARIA_BENCH_LINES", "OTZARIA_BENCH_ECONOMY")},
            "unset_runtime_environment": ["OTZARIA_BENCH_NOTES", "RAYON_NUM_THREADS", "RAYON_RS_NUM_CPUS"],
            "expected_stamps": expected_stamps, "timeouts_seconds": config["timeouts_seconds"],
            "github": {key: os.environ.get(key) for key in
                       ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA", "GITHUB_REF", "GITHUB_ACTOR",
                        "RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion")},
            "notes": [
                "H and E are each compiled once from clean immutable checkouts with the same explicit untracked wide-test and layout overlays.",
                "The original indexing fixture is unchanged. The original phase-1 measure function is imported verbatim by SHA.",
                "Every dependency/feature graph equals immutable H after exact source-root URI normalization; overlays do not change the graph.",
                "Fresh H original-target identity is compared with the historical 974a... executable and reported, not assumed.",
                "Both benchmark targets and each layout artifact identity are retained; no fixture builds happen during measurement.",
                "Main64 preserves the former topology schedule under B-to-H/H-to-E label mapping. Wide32 is separate and follows main64.",
                "Wide forward/reverse orders balance actual binary, pseudo-label and position marginals within each topology and real/control class.",
                "Wide topology precedence balances real/control classes; one control block per actual binary/topology cannot independently balance that joint cell.",
                "Wide cases are idle after indexing: fourteen timed cases, forty-four behavior-only cases, fixed five warmups and thirty observations.",
                "The original fixture retains its own thirty idle probes and closed concurrent-query loop. No cooldown, cache drop or extra warmup is added.",
                "Full output comparison is exact and outside timed queries; no field, ID, ordering, snippet or highlight normalization is applied.",
                "Each timed wide case also makes one explicit untimed post-timing query to verify its warm-cache full snapshot; this is recorded separately from five warmups and thirty measurements.",
                "No per-run executable hashing occurs inside the measurement schedule; all binaries are checked before and after.",
                "No performance threshold, automatic promotion or production edit follows from completion.",
            ],
        }
        write_json(output / "environment.json", metadata)
        build_root.mkdir(parents=True, exist_ok=False)
        binaries, hashes = {}, {}
        for label, root in (("H", head), ("E", candidate)):
            status["phase"] = "build-" + label
            status["build_invocations"][label] = 1
            write_json(output / "status.json", status)
            built = build(core, root, label, config, args.wide_fixture.resolve(), output, build_root, environment, deadline)
            metadata[label] = built
            for target, details in built["binary_targets"].items():
                binaries[(label, target)] = Path(details["path"])
                hashes[(label, target)] = details["sha256"]
            write_json(output / "environment.json", metadata)
        metadata["binary_sha256"] = {label: {target: hashes[(label, target)] for target in ("indexing_throughput", config["wide"]["target"])} for label in LABELS}
        metadata["build_invocations"] = dict(status["build_invocations"])
        for key, path in binaries.items():
            require(file_digest(path) == hashes[key], "benchmark executable changed before measurement")
        write_json(output / "environment.json", metadata)
        rows = schedule(core, config, affinities, hashes)
        validate_schedule(core, config, rows, affinities, hashes)
        write_json(output / "resolved-schedule.json", rows)
        write_json(output / "measurement-start.json", core.snapshot())
        print(json.dumps({"schedule_sha256": file_digest(output / "resolved-schedule.json"),
                          "affinities": affinities, "binary_sha256": metadata["binary_sha256"],
                          "H_reproduces_original": metadata["H"]["original_executable_identity"]["matches"]}), flush=True)
        (output / "main").mkdir()
        (output / "wide").mkdir()
        status["phase"] = "measure"
        with (output / "samples.jsonl").open("w") as samples, (output / "snapshot-comparisons.jsonl").open("w") as matches:
            for entry in rows:
                remaining_check(deadline, 180, f"measurement run {entry['run']}")
                status["active_process"] = entry
                status["measured_process_attempts"] += 1
                write_json(output / "status.json", status)
                target = entry["target"]
                binary = binaries[(entry["actual_binary"], target)]
                if target == "indexing_throughput":
                    record = core.measure(entry, binary, output / "main", entry["affinity"], environment, expected_stamps)
                    status["completed_main_processes"] += 1
                else:
                    record = measure_wide(core, entry, binary, output / "wide", environment, config)
                    compared = compare_snapshots(record, snapshot_references, output / "wide")
                    for comparison in compared:
                        matches.write(json.dumps(comparison, sort_keys=True) + "\n")
                    matches.flush()
                    comparisons.extend(compared)
                    record["all_full_output_snapshots_equal"] = True
                    status["completed_wide_processes"] += 1
                records.append(record)
                samples.write(json.dumps(record, sort_keys=True) + "\n")
                samples.flush()
                status["completed_processes"] = len(records)
                status["validated_snapshot_comparisons"] = len(comparisons)
                status.pop("active_process", None)
                write_json(output / "status.json", status)
                write_json(output / "analysis.json", analyse(core, config, records))
                progress = {key: entry[key] for key in ("run", "target", "actual_binary", "kind", "topology_case", "case_order")}
                progress["process_wall_seconds"] = record["process"]["process_wall_seconds"]
                if target == "indexing_throughput":
                    progress["search_during_p95_ms"] = record["sample"]["search_during_p95_ms"]
                else:
                    progress["verified_case_snapshots"] = len(record["cases"])
                print(json.dumps(progress, sort_keys=True), flush=True)
        validate_schedule(core, config, records, affinities, hashes)
        require(len(snapshot_references) == 58 and len(comparisons) == 32 * 58, "incomplete full-output comparison coverage")
        for label, root in (("H", head), ("E", candidate)):
            require(source_provenance(root, label, config) == initial_sources[label], "pure source changed during measurement")
        for key, path in binaries.items():
            require(file_digest(path) == hashes[key], "benchmark executable changed during measurement")
        status["state"] = "complete"
        status["phase"] = "complete"
        status["correctness_gate"] = "All58 complete output snapshots match H byte-for-byte in all32 wide processes; original fixture stamps remain unchanged."
    except BaseException:
        status["state"] = "failed"
        (output / "failure.log").write_text(traceback.format_exc())
        raise
    finally:
        write_json(output / "status.json", status)
        if core is not None and config is not None:
            write_json(output / "analysis.json", analyse(core, config, records))
        write_checksums(output)


if __name__ == "__main__":
    main()
