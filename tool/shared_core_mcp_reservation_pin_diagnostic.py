#!/usr/bin/env python3
"""Isolated final MCP F pin comparison; execution requires a frozen configuration.

Two clean, locked dev builds select existing example/compatibility targets only.
Correctness checks precede a fixed 16-process benchmark with separate controls.
This controller never changes a source pin, source file, fixture, or lockfile.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import statistics
import subprocess
import time
import tomllib
import traceback

HOST_REPOSITORY = "Otzaria/otzaria_search_engine"
MCP_REPOSITORY = "Otzaria/otzaria_mcp"
H_MCP = "3c4161074e896e461db4e07537bc11fcfafc3b9e"
H_CORE = "4761356749815d283ede41ef956c64b3dbffe048"
F_CORE = "4bb3d8e7d11499861ce0655c88841eb23e0f1649"
ENGINE_URL = "https://github.com/Otzaria/otzaria_search_engine"
PIN_PACKAGES = {"otzaria-search-core", "search_engine", "tantivy-fst"}
PIN_FILES = {"Cargo.toml", "mcp/Cargo.toml", "Cargo.lock"}
CORE_SOURCE_FILES = {"rust/src/api/search_engine.rs", "rust/src/indexing_diagnostics.rs",
                     "rust/src/library_indexing_tests.rs"}
TARGETS = {"search_contract": "example", "search_benchmark": "example", "existing_index": "test"}
SCHEMA_TEST = "mcp_reads_independently_built_schema10_index_without_changing_it"
METRICS = ("latency_p50_ms", "latency_p95_ms", "latency_p99_ms",
           "throughput_requests_per_s", "elapsed_s", "peak_rss_kib")
PAIRS = (
    ("R1", "real", "HF", None),
    ("CH1", "same_H_control", "HF", "H"),
    ("R2", "real", "FH", None),
    ("CF1", "same_F_control", "FH", "F"),
    ("R3", "real", "FH", None),
    ("CH2", "same_H_control", "FH", "H"),
    ("R4", "real", "HF", None),
    ("CF2", "same_F_control", "HF", "F"),
)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def sha_bytes(value):
    return hashlib.sha256(value).hexdigest()


def sha_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def strict_json(raw):
    def object_pairs(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, f"duplicate JSON key: {key}")
            result[key] = value
        return result

    def invalid_constant(value):
        raise RuntimeError(f"non-finite JSON constant: {value}")

    return json.loads(raw, object_pairs_hook=object_pairs, parse_constant=invalid_constant)


def git(root, *arguments):
    return subprocess.check_output(["git", "-C", str(root), *arguments], text=True).strip()


def tracked_files(root):
    raw = subprocess.check_output(["git", "-C", str(root), "ls-files", "-z"])
    return [name for name in raw.decode().split("\0") if name]


def source_files(root):
    return [name for name in tracked_files(root)
            if name not in PIN_FILES and not name.startswith("docs/") and not name.endswith(".md")]


def inventory(root, names):
    result = {}
    for name in names:
        path = root / name
        require(path.is_file() and not path.is_symlink() and path.resolve().is_relative_to(root.resolve()),
                f"source is not a regular contained file: {name}")
        result[name] = sha_file(path)
    return result


def load_config(path, frozen=True):
    config = strict_json(path.read_bytes())
    require(config["schema_version"] == 1, "unknown configuration schema")
    require(config["hosting_repository"] == HOST_REPOSITORY and config["mcp_repository"] == MCP_REPOSITORY,
            "diagnostic repository identity changed")
    require(set(config["revisions"]) == set(config["pin_files_sha256"]) == {"H", "F"},
            "comparison labels changed")
    require(config["revisions"]["H"]["mcp_commit"] == H_MCP, "current MCP baseline changed")
    require(config["revisions"]["H"]["core_commit"] == H_CORE
            and config["revisions"]["F"]["core_commit"] == F_CORE, "core comparison changed")
    for label in ("H", "F"):
        revision = config["revisions"][label]
        for field, length in (("core_source_files_sha256", 64), ("core_source_git_blobs", 40)):
            require(set(revision[field]) == CORE_SOURCE_FILES
                    and all(re.fullmatch(r"[0-9a-f]{" + str(length) + "}", value)
                            for value in revision[field].values()), "core source identity inventory changed")
    require(config["revisions"]["F"]["core_parent_commit"] == H_CORE,
            "F must be a direct child of the declared H core")
    require(config["toolchain"] == "1.99.1", "toolchain changed")
    require(config["build"] == {"profile": "dev", "opt_level": 0, "debug": 0, "incremental": False,
                                "jobs": 3, "examples": ["search_contract", "search_benchmark"],
                                "test": "existing_index"}, "build scope/profile changed")
    require(config["contracts"]["scenario_count"] == len(config["contracts"]["scenario_names"]) == 17,
            "contract scenario count changed")
    require(len(set(config["contracts"]["scenario_names"])) == 17
            and config["contracts"]["require_output_bytes_equal_golden"] is True,
            "strict contract equality changed")
    require(config["schema10"]["test_name"] == SCHEMA_TEST
            and config["schema10"]["exact_test_count"] == 1
            and config["schema10"]["regeneration_allowed"] is False, "schema10 check changed")
    benchmark = config["benchmark"]
    require([benchmark[key] for key in ("warmup_queries", "measured_queries", "total_hit_checksum",
                                         "real_pairs", "same_H_control_pairs", "same_F_control_pairs", "processes")]
            == [85, 1700, 5700, 4, 2, 2, 16], "fixed benchmark workload changed")
    if frozen:
        require(config["state"] == "frozen", "configuration is intentionally unfrozen; do not execute")
        for label in ("H", "F"):
            for key in ("mcp_commit", "mcp_tree", "core_commit", "core_tree"):
                require(re.fullmatch(r"[0-9a-f]{40}", config["revisions"][label][key] or ""),
                        f"unfrozen {label} {key}")
        require(config["revisions"]["F"]["mcp_commit"] != H_MCP, "candidate is the baseline MCP")
    return config


def schedule():
    result = []
    for pair, kind, order, actual in PAIRS:
        for position, pseudo in enumerate(order, 1):
            result.append({"run": len(result) + 1, "pair": pair, "kind": kind,
                           "pair_order": order, "position": position,
                           "pseudo_label": pseudo, "actual_binary": actual or pseudo})
    return result


def verify_source(root, label, config, out):
    revision = config["revisions"][label]
    require(git(root, "rev-parse", "HEAD") == revision["mcp_commit"], f"{label} checkout commit differs")
    require(git(root, "rev-parse", "HEAD^{tree}") == revision["mcp_tree"], f"{label} checkout tree differs")
    require(not git(root, "status", "--porcelain", "--untracked-files=all"), f"{label} checkout is not clean")
    common = inventory(root, source_files(root))
    require(common == config["unchanged_tracked_files_sha256"], f"{label} MCP code/fixtures changed")
    pins = inventory(root, sorted(PIN_FILES))
    require(pins == config["pin_files_sha256"][label], f"{label} manifest/lock bytes differ")
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    package = tomllib.loads((root / "mcp/Cargo.toml").read_text())
    fst = workspace["patch"]["crates-io"]["tantivy-fst"]
    core = package["dependencies"]["search_engine"]
    require(fst["git"] == core["git"] == ENGINE_URL
            and fst["rev"] == core["rev"] == revision["core_commit"]
            and core["package"] == "otzaria-search-core", f"{label} core/FST manifest pin differs")
    expected_source = f"git+{ENGINE_URL}?rev={revision['core_commit']}#{revision['core_commit']}"
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    entries = [entry for entry in lock["package"] if ENGINE_URL in entry.get("source", "")]
    require(len(entries) == 3 and {entry["name"] for entry in entries} == PIN_PACKAGES
            and all(entry["source"] == expected_source for entry in entries),
            f"{label} must lock all three engine-source packages to one exact revision")
    schema_root = root / config["schema10"]["fixture_directory"]
    schema_inventory = inventory(schema_root, sorted(config["schema10"]["fixture_files_sha256"]))
    require(schema_inventory == config["schema10"]["fixture_files_sha256"], f"{label} schema10 fixture changed")
    require(strict_json((schema_root / "index/otzaria_index_meta.json").read_bytes())["schema_version"] == 10,
            "preserved index no longer identifies schema10")
    golden = root / config["contracts"]["golden_path"]
    require(sha_file(golden) == config["contracts"]["golden_sha256"], "contract golden bytes changed")
    require(sorted(strict_json(golden.read_bytes())) == config["contracts"]["scenario_names"],
            "contract golden scenarios changed")
    record = {"revision": revision, "parents": git(root, "show", "-s", "--format=%P", "HEAD").split(),
              "common_source_sha256": common, "pin_files_sha256": pins,
              "manifest_core_pin": core, "manifest_fst_pin": fst, "all_three_lock_entries": entries,
              "schema10_fixture_sha256": schema_inventory, "tracked_clean": True}
    write_json(out / f"{label}-source.json", record)
    retained = sorted(PIN_FILES | {name for name in common if name.startswith("mcp/examples/")
                                  or name.startswith("mcp/tests/")}
                      | {config["schema10"]["fixture_directory"] + "/" + name
                         for name in config["schema10"]["fixture_files_sha256"]})
    for name in retained:
        destination = out / "source-inputs" / label / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(root / name, destination)
    return record


def host_context():
    result = {}
    for name, path in (("loadavg", "/proc/loadavg"), ("cpu_pressure", "/proc/pressure/cpu"),
                       ("memory_pressure", "/proc/pressure/memory"), ("cgroup_cpu_stat", "/sys/fs/cgroup/cpu.stat")):
        try:
            result[name] = Path(path).read_text()
        except OSError:
            result[name] = None
    return result


def quiet_build_processes():
    found = []
    for path in Path("/proc").iterdir():
        if path.name.isdigit():
            try:
                name = (path / "comm").read_text().strip()
                if name in {"cargo", "rustc", "rustdoc", "rust-lld"}:
                    found.append({"pid": int(path.name), "name": name})
            except OSError:
                continue
    return sorted(found, key=lambda value: value["pid"])


def cpu_list(value):
    result = set()
    for part in value.strip().split(","):
        first, separator, last = part.partition("-")
        result.update(range(int(first), int(last) + 1) if separator else [int(first)])
    return result


def select_affinity():
    allowed = sorted(os.sched_getaffinity(0))
    topology = {}
    for cpu in allowed:
        directory = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology")
        values = {key: (directory / key).read_text().strip()
                  for key in ("physical_package_id", "core_id", "thread_siblings_list")}
        require(int(values["physical_package_id"]) >= 0 and int(values["core_id"]) >= 0
                and cpu in cpu_list(values["thread_siblings_list"]), "inconsistent CPU topology")
        topology[cpu] = values
    for position, first in enumerate(allowed):
        for second in allowed[position + 1:]:
            if (topology[first]["physical_package_id"] == topology[second]["physical_package_id"]
                    and topology[first]["core_id"] != topology[second]["core_id"]
                    and second not in cpu_list(topology[first]["thread_siblings_list"])
                    and first not in cpu_list(topology[second]["thread_siblings_list"])):
                return [first, second], topology
    raise RuntimeError("runner lacks two allowed distinct physical cores in one package")


def capture(command, prefix, cwd, environment, timeout, deadline, affinity=None, quiet=False):
    prefix.parent.mkdir(parents=True, exist_ok=True)
    timeout = min(timeout, deadline - time.monotonic())
    require(timeout > 0, "controller time budget exhausted")
    before_builds = quiet_build_processes() if quiet else None
    launched = list(map(str, command))
    if affinity is not None:
        launched = ["taskset", "--cpu-list", ",".join(map(str, affinity))] + launched
    time_path = prefix.with_suffix(".usage.json")
    time_format = '{"user_cpu_s":%U,"system_cpu_s":%S,"max_rss_kib":%M,"exit_code":%x}'
    launched = ["/usr/bin/time", "-f", time_format, "-o", str(time_path)] + launched
    record = {"state": "starting", "command": launched, "cwd": str(cwd), "affinity": affinity,
              "TMPDIR": environment.get("TMPDIR"), "host_before": host_context(),
              "build_processes_before": before_builds, "started_unix_s": time.time()}
    write_json(prefix.with_suffix(".context.json"), record)
    require(not quiet or not before_builds, "Rust compilation is active before a measurement")
    started = time.monotonic()
    timed_out = False
    with prefix.with_suffix(".stdout").open("wb") as stdout, prefix.with_suffix(".stderr").open("wb") as stderr:
        process = subprocess.Popen(launched, cwd=cwd, env=environment, stdout=stdout, stderr=stderr,
                                   start_new_session=True)
        record.update({"state": "running", "pid": process.pid})
        write_json(prefix.with_suffix(".context.json"), record)
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    record.update({"state": "exited", "returncode": process.returncode, "timed_out": timed_out,
                   "wall_seconds": time.monotonic() - started, "host_after": host_context(),
                   "build_processes_after": quiet_build_processes() if quiet else None})
    if time_path.exists():
        try:
            record["whole_process_usage"] = strict_json(time_path.read_bytes())
        except (RuntimeError, ValueError):
            record["whole_process_usage_parse_error"] = True
    write_json(prefix.with_suffix(".context.json"), record)
    require(not timed_out and process.returncode == 0, f"command failed; see {prefix}.stderr")
    require("whole_process_usage" in record, f"process accounting missing for {prefix}")
    require(not quiet or not record["build_processes_after"], "Rust compilation appeared during measurement")
    return record


def verify_core_sources(checkout, revision):
    hashes = inventory(checkout, sorted(CORE_SOURCE_FILES))
    require(hashes == revision["core_source_files_sha256"], "core source file SHA256 differs")
    blobs = {name: git(checkout, "rev-parse", "HEAD:" + name) for name in sorted(CORE_SOURCE_FILES)}
    require(blobs == revision["core_source_git_blobs"], "core source Git blob differs")
    # Read the stored commit object so a shallow boundary cannot hide its parent.
    headers = git(checkout, "cat-file", "-p", "HEAD").partition("\n\n")[0].splitlines()
    parents = [line.removeprefix("parent ") for line in headers if line.startswith("parent ")]
    expected_parent = revision["core_parent_commit"]
    require(expected_parent is None or parents == [expected_parent], "core parent differs")
    return {"source_files_sha256": hashes, "source_git_blobs": blobs, "parent_commits": parents}


def verify_metadata(metadata, revision, source_record):
    expected_source = f"git+{ENGINE_URL}?rev={revision['core_commit']}#{revision['core_commit']}"
    packages = [package for package in metadata["packages"] if ENGINE_URL in (package.get("source") or "")]
    require(len(packages) == 3 and {package["name"] for package in packages} == PIN_PACKAGES,
            "resolved metadata lacks the exact three engine packages")
    records = []
    for package in packages:
        require(package["source"] == expected_source, "resolved engine package pin differs")
        manifest = Path(package["manifest_path"])
        checkout = Path(git(manifest.parent, "rev-parse", "--show-toplevel"))
        require(git(checkout, "rev-parse", "HEAD") == revision["core_commit"]
                and git(checkout, "rev-parse", "HEAD^{tree}") == revision["core_tree"],
                "Cargo engine checkout commit/tree differs from the declared core")
        require(not git(checkout, "status", "--porcelain", "--untracked-files=no"),
                "Cargo engine checkout has tracked modifications")
        core_provenance = verify_core_sources(checkout, revision)
        records.append({"name": package["name"], "version": package["version"],
                        "package_id": package["id"], "source": package["source"],
                        "manifest_path": str(manifest), "manifest_sha256": sha_file(manifest),
                        "checkout": str(checkout), "commit": revision["core_commit"],
                        "tree": revision["core_tree"], "tracked_clean": True,
                        "core_provenance": core_provenance})
    require({record["name"] for record in records}
            == {entry["name"] for entry in source_record["all_three_lock_entries"]}, "lock/metadata packages differ")
    return records


def build(root, label, config, source_record, out, build_root, environment, deadline):
    target = build_root / label
    require(not target.exists(), f"{label} target directory must be new")
    target.mkdir(parents=True)
    env = environment | {"CARGO_TARGET_DIR": str(target)}
    metadata_prefix = out / "builds" / f"{label}-metadata"
    capture(["cargo", "metadata", "--locked", "--format-version", "1"], metadata_prefix,
            root, env, config["timeouts_seconds"]["metadata"], deadline)
    metadata = strict_json(metadata_prefix.with_suffix(".stdout").read_bytes())
    dependencies = verify_metadata(metadata, config["revisions"][label], source_record)
    write_json(out / "builds" / f"{label}-resolved-pins.json", dependencies)
    command = ["cargo", "build", "--locked", "-p", "otzaria-mcp", "--jobs", "3",
               "--example", "search_contract", "--example", "search_benchmark",
               "--test", "existing_index", "--message-format=json"]
    prefix = out / "builds" / f"{label}-build"
    context = capture(command, prefix, root, env, config["timeouts_seconds"]["build"], deadline)
    selected = {}
    for line in prefix.with_suffix(".stdout").read_text().splitlines():
        message = strict_json(line)
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        name = message["target"]["name"]
        if name not in TARGETS:
            continue
        require(message["target"]["kind"] == [TARGETS[name]], f"unexpected artifact kind for {name}")
        expected_source = root / "mcp" / ("examples" if TARGETS[name] == "example" else "tests") / f"{name}.rs"
        require(Path(message["target"]["src_path"]).resolve() == expected_source.resolve(),
                f"artifact source path differs for {name}")
        require(message["profile"]["test"] is (TARGETS[name] == "test")
                and message["profile"]["opt_level"] == "0" and not message["fresh"],
                f"{label} {name} was not freshly built in the intended profile")
        require(name not in selected, f"duplicate executable artifact for {name}")
        original = Path(message["executable"])
        require(original.resolve().is_relative_to(target.resolve()) and original.is_file(),
                "Cargo executable escaped the clean target directory")
        binary = out / "binaries" / f"{label}-{name}"
        binary.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(original, binary)
        selected[name] = {"path": str(binary), "sha256": sha_file(binary),
                          "bytes": binary.stat().st_size, "cargo_artifact": message}
    require(set(selected) == set(TARGETS), f"{label} did not produce exactly the three selected executables")
    require(inventory(root, sorted(PIN_FILES)) == source_record["pin_files_sha256"]
            and not git(root, "status", "--porcelain", "--untracked-files=all"),
            f"{label} source/lock changed during the locked build")
    result = {"targets": selected, "context": context, "resolved_pins": dependencies}
    write_json(out / "builds" / f"{label}-artifacts.json", result)
    return result


def validate_benchmark(raw, config):
    value = strict_json(raw)
    expected = config["benchmark"]
    keys = {"fixture", "build_profile", "requests", "total_hit_checksum", "elapsed_s",
            "throughput_requests_per_s", "latency_p50_ms", "latency_p95_ms", "latency_p99_ms",
            "peak_rss_kib", "rss_scope", "queue"}
    require(set(value) == keys, "benchmark output fields changed")
    require(value["requests"] == 1700 and value["total_hit_checksum"] == 5700,
            "benchmark query count/checksum changed")
    for name in ("fixture", "build_profile", "rss_scope", "queue"):
        require(value[name] == expected[name], f"benchmark {name} changed")
    for name in METRICS:
        require(type(value[name]) in (int, float) and math.isfinite(value[name]) and value[name] > 0,
                f"invalid benchmark {name}")
    require(type(value["peak_rss_kib"]) is int, "benchmark RSS is not integer KiB")
    require(value["latency_p50_ms"] <= value["latency_p95_ms"] <= value["latency_p99_ms"],
            "benchmark percentiles are unordered")
    require(math.isclose(value["throughput_requests_per_s"], 1700 / value["elapsed_s"], rel_tol=1e-12),
            "benchmark throughput differs from elapsed time")
    return value


def summarize(records):
    groups = {}
    for kind in ("real", "same_H_control", "same_F_control"):
        selected = [record for record in records if record["kind"] == kind and "metrics" in record]
        groups[kind] = {}
        for pseudo in ("H", "F"):
            values = [record for record in selected if record["pseudo_label"] == pseudo]
            groups[kind][pseudo] = {"observations": len(values), "runs": [value["run"] for value in values]}
            for key in METRICS:
                samples = [value["metrics"][key] for value in values]
                groups[kind][pseudo][key] = ({"median": statistics.median(samples),
                                             "min": min(samples), "max": max(samples)} if samples else None)
    contrasts = []
    for pair, kind, order, _ in PAIRS:
        selected = {record["pseudo_label"]: record for record in records
                    if record["pair"] == pair and "metrics" in record}
        if set(selected) == {"H", "F"}:
            contrasts.append({"pair": pair, "kind": kind, "order": order,
                              "pseudo_F_over_H_percent": {key: 100 * (selected["F"]["metrics"][key]
                                                                       / selected["H"]["metrics"][key] - 1)
                                                           for key in METRICS}})
    return {"valid_processes": sum("metrics" in record for record in records), "groups": groups,
            "pair_contrasts": contrasts, "controls_pooled_with_real": False,
            "performance_threshold": None, "automatic_promotion": False}


def artifact_manifest(out):
    files = [path for path in sorted(out.rglob("*")) if path.is_file() and path.name != "SHA256SUMS"]
    (out / "SHA256SUMS").write_text("".join(f"{sha_file(path)}  {path.relative_to(out)}\n" for path in files))


def execute(args, config):
    require(os.environ.get("GITHUB_ACTIONS") == "true"
            and os.environ.get("GITHUB_REPOSITORY") == HOST_REPOSITORY,
            "execution is restricted to the isolated engine-repository Actions job")
    out, build_root, tmp = args.out.resolve(), args.build_root.resolve(), args.tmpdir.resolve()
    require(not out.exists() and not build_root.exists() and not tmp.exists(), "diagnostic paths must be new")
    require(tmp.parent == Path("/dev/shm"), "shared fixture TMPDIR must be directly under /dev/shm")
    out.mkdir(parents=True)
    build_root.mkdir(parents=True)
    tmp.mkdir()
    deadline = time.monotonic() + config["timeouts_seconds"]["controller_budget"]
    records, source_records, built = [], {}, {}
    status = {"complete": False, "phase": "prepared", "planned_build_invocations": 2,
              "build_variants_completed": [], "benchmark_processes_completed": 0,
              "performance_threshold": None, "automatic_promotion": False}

    def progress(**fields):
        status.update(fields)
        write_json(out / "status.json", status)

    progress()
    write_json(out / "config.json", config)
    shutil.copy2(Path(__file__), out / "controller.py")
    shutil.copy2(args.workflow, out / "workflow.yml")
    write_json(out / "schedule.json", schedule())
    try:
        filesystem = strict_json(subprocess.check_output(
            ["findmnt", "-J", "-T", str(tmp), "-o", "SOURCE,FSTYPE,TARGET,OPTIONS"], text=True))
        require(filesystem["filesystems"][0]["fstype"] == "tmpfs", "fixture TMPDIR is not tmpfs")
        affinity, topology = select_affinity()
        env = os.environ.copy()
        removed_runtime_overrides = {key: env.pop(key, None)
                                     for key in ("RAYON_NUM_THREADS", "RAYON_RS_NUM_CPUS")}
        for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                    "CARGO_BUILD_TARGET"):
            env.pop(key, None)
        env.update({"RUSTUP_TOOLCHAIN": config["toolchain"], "CARGO_INCREMENTAL": "0",
                    "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0",
                    "CARGO_PROFILE_DEV_OPT_LEVEL": "0", "CARGO_PROFILE_TEST_OPT_LEVEL": "0",
                    "TMPDIR": str(tmp), "TMP": str(tmp), "TEMP": str(tmp), "LC_ALL": "C"})
        rustc = subprocess.check_output(["rustc", "-Vv"], env=env, text=True)
        require(re.search(r"^release: 1\.99\.1$", rustc, re.M), "resolved rustc differs from the pinned toolchain")
        cpu_models = sorted({line.partition(":")[2].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                             if line.startswith("model name")})
        write_json(out / "environment.json", {"rustc": rustc,
                   "cargo": subprocess.check_output(["cargo", "-Vv"], env=env, text=True),
                   "kernel": platform.release(), "machine": platform.machine(), "cpu_models": cpu_models,
                   "affinity": affinity, "topology": topology, "tmpfs": filesystem,
                   "removed_runtime_overrides": removed_runtime_overrides,
                   "shared_TMPDIR": str(tmp), "selected_build_environment": {key: env[key] for key in
                    ("RUSTUP_TOOLCHAIN", "CARGO_INCREMENTAL", "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG",
                     "CARGO_PROFILE_DEV_OPT_LEVEL", "CARGO_PROFILE_TEST_OPT_LEVEL", "LC_ALL")},
                   "actions": {key: os.environ.get(key) for key in
                    ("GITHUB_REPOSITORY", "GITHUB_SHA", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_REF")},
                   "initial_host_context": host_context()})
        roots = {"H": args.head.resolve(), "F": args.candidate.resolve()}
        for label in ("H", "F"):
            progress(phase="source_verification", active_label=label)
            source_records[label] = verify_source(roots[label], label, config, out)
        for name in PIN_FILES:
            require((roots["H"] / name).read_bytes().replace(H_CORE.encode(), F_CORE.encode())
                    == (roots["F"] / name).read_bytes(), f"{name} changed beyond the paired engine pins")
        for label in ("H", "F"):
            progress(phase="metadata_and_build", active_label=label)
            built[label] = build(roots[label], label, config, source_records[label], out, build_root, env, deadline)
            progress(build_variants_completed=list(built))
        require(not quiet_build_processes(), "compilation is still active after both clean builds")
        golden = (roots["H"] / config["contracts"]["golden_path"]).read_bytes()
        checks = {}
        for label in ("H", "F"):
            targets = built[label]["targets"]
            for target in targets.values():
                require(sha_file(target["path"]) == target["sha256"], "built binary changed before validation")
            prefix = out / "correctness" / f"{label}-contract"
            progress(phase="contract", active_label=label)
            capture([targets["search_contract"]["path"]], prefix, out, env,
                    config["timeouts_seconds"]["contract"], deadline, affinity, quiet=True)
            contract_raw = prefix.with_suffix(".stdout").read_bytes()
            require(sorted(strict_json(contract_raw)) == config["contracts"]["scenario_names"],
                    f"{label} contract omitted or added scenarios")
            require(strict_json(contract_raw) == strict_json(golden) and contract_raw == golden,
                    f"{label} full contract JSON/bytes differ from the existing golden")
            prefix = out / "correctness" / f"{label}-schema10"
            progress(phase="schema10", active_label=label)
            capture([targets["existing_index"]["path"], SCHEMA_TEST, "--exact", "--test-threads=1",
                     "--color", "never", "--format", "pretty"],
                    prefix, out, env, config["timeouts_seconds"]["schema10"], deadline, affinity, quiet=True)
            text = prefix.with_suffix(".stdout").read_text()
            require(re.search(r"^running 1 test$", text, re.M)
                    and re.search(r"^test " + re.escape(SCHEMA_TEST) + r" \.\.\. ok$", text, re.M)
                    and re.search(r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;", text, re.M),
                    f"{label} schema10 filter did not run and pass exactly one expected test")
            schema = inventory(roots[label] / config["schema10"]["fixture_directory"],
                               sorted(config["schema10"]["fixture_files_sha256"]))
            require(schema == config["schema10"]["fixture_files_sha256"], "committed schema10 fixture changed")
            checks[label] = {"contract_scenarios": 17, "contract_sha256": sha_bytes(contract_raw),
                             "full_json_equal": True, "raw_bytes_equal": True,
                             "schema10_tests_run": 1, "schema10_tests_passed": 1,
                             "schema10_fixture_sha256_after": schema}
            write_json(out / "correctness.json", checks)
        for entry in schedule():
            record = dict(entry)
            records.append(record)
            binary = built[entry["actual_binary"]]["targets"]["search_benchmark"]
            require(sha_file(binary["path"]) == binary["sha256"], "benchmark binary hash changed")
            prefix = out / "runs" / f"{entry['run']:02d}-{entry['pair']}-{entry['position']}"
            record.update({"binary_sha256": binary["sha256"], "mcp_commit": config["revisions"][entry["actual_binary"]]["mcp_commit"],
                           "core_commit": config["revisions"][entry["actual_binary"]]["core_commit"],
                           "stdout_file": str(prefix.with_suffix(".stdout").relative_to(out))})
            progress(phase="benchmark", active_label=entry["actual_binary"], active_run=entry["run"])
            write_json(out / "process-records.json", records)
            try:
                record["context"] = capture([binary["path"]], prefix, out, env,
                    config["timeouts_seconds"]["benchmark"], deadline, affinity, quiet=True)
                record["metrics"] = validate_benchmark(prefix.with_suffix(".stdout").read_bytes(), config)
            except Exception as error:
                record["validation_error"] = str(error)
                raise
            finally:
                write_json(out / "process-records.json", records)
                write_json(out / "summary.json", summarize(records))
            progress(benchmark_processes_completed=entry["run"])
            print(f"completed {entry['run']}/16 {entry['pair']} {entry['pseudo_label']} actual={entry['actual_binary']}", flush=True)
        for label in ("H", "F"):
            require(inventory(roots[label], source_files(roots[label])) == config["unchanged_tracked_files_sha256"]
                    and inventory(roots[label], sorted(PIN_FILES)) == config["pin_files_sha256"][label]
                    and not git(roots[label], "status", "--porcelain", "--untracked-files=all"),
                    f"{label} source/fixtures/locks changed during execution")
        require(len(records) == 16 and all("metrics" in record for record in records), "fixed schedule is incomplete")
        status.update({"complete": True, "phase": "complete", "active_label": None, "active_run": None,
                       "build_invocations": 2, "benchmark_processes": 16,
                       "real_observations_per_binary": 4, "correctness": checks})
    except Exception as error:
        status.update({"error": str(error), "traceback": traceback.format_exc()})
        raise
    finally:
        write_json(out / "process-records.json", records)
        write_json(out / "summary.json", summarize(records))
        write_json(out / "status.json", status)
        artifact_manifest(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("check", "emit-revisions", "run"):
        subparser = commands.add_parser(command)
        subparser.add_argument("--config", type=Path, required=True)
        if command == "check":
            subparser.add_argument("--allow-unfrozen", action="store_true")
        elif command == "emit-revisions":
            subparser.add_argument("--github-output", type=Path, required=True)
        else:
            for name in ("head", "candidate", "out", "build-root", "tmpdir", "workflow"):
                subparser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    config = load_config(args.config, frozen=not (args.command == "check" and args.allow_unfrozen))
    if args.command == "check":
        print(json.dumps({"configuration_state": config["state"], "processes": len(schedule()),
                          "candidate_mcp_commit": config["revisions"]["F"]["mcp_commit"],
                          "builds_or_measurements_performed": 0}, indent=2))
    elif args.command == "emit-revisions":
        with args.github_output.open("a") as output:
            for label in ("H", "F"):
                output.write(f"{label.lower()}={config['revisions'][label]['mcp_commit']}\n")
    else:
        execute(args, config)


if __name__ == "__main__":
    main()
