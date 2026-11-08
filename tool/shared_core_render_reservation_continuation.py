#!/usr/bin/env python3
"""Zero-build continuation of the frozen H/F experiment.

Reuse the five verified executables from failed run 42441549309, independently
revalidate H's original E chain, rerun three strict lifecycle gates with separate
stdout/stderr, then execute the unchanged 96-process measurement protocol.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import traceback
import zipfile

PROTOCOL_SHA256 = "e2ef105b8e6452df3d1669256662557632191daab8c3cc88d22cb14038af569a"
PROTOCOL_CONFIG_SHA256 = "5f93e98d2acee137ba5a54c752862bbd0bc286897e582490736312f5dff4f9ff"
FAILED_RUN = 42441549309
FAILED_ARTIFACT = 13602032010
FAILED_WORKFLOW = "421e7cd74f3623e3a5f9e667fc43a3f533d07e1a"
FAILED_ZIP_SHA256 = "5a95ef546089d7c801d563de702571939d0793d0836e19e7507c39a5ee662a0e"
FAILED_ZIP_BYTES = 34139866
BINARY_IDENTITIES = {
    "binaries/H-indexing_throughput": (15012848, "974a92cc60324467d26ebad61c7d21ca0e7512087201eaa037ff357d42151337"),
    "binaries/H-render_scheduling": (15124880, "37f58310a3365b81509c43ecbfa04127e621b5c60d993a72c7b5c3ae3ed2db21"),
    "binaries/F-indexing_throughput": (15031960, "7b7d689750822dd9f7b792154023ab5f20566019ae5ee1ccc0c92fb0637f2861"),
    "binaries/F-render_scheduling": (15142848, "020dbd5ccfb20943799239c9031a56052410973764db3d2f5268c170dbef1d08"),
    "binaries/F-library-tests": (25388616, "8840a8883ef11c3a7505bc163af091162f5058a08f5e0001a06810980100a783"),
}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    result = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1 << 20), b""):
            result.update(chunk)
    return result.hexdigest()


def write_json(path, value):
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def import_protocol(path):
    require(digest(path) == PROTOCOL_SHA256, "approved F protocol bytes changed")
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("otzaria_frozen_F_protocol", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    # This continuation has its own main. Compile-capable entry points must not
    # become reachable accidentally; measurement/helper functions stay intact.
    def forbidden(*args, **kwargs):
        raise RuntimeError("compiler/build/original-main entry point forbidden in zero-build continuation")
    for name in ("main", "build", "dependency_graph", "compiler_environment"):
        setattr(module, name, forbidden)
    return module


def load_continuation(path):
    config = json.loads(path.read_text())
    require(config["schema_version"] == 1 and config["state"] == "frozen",
            "continuation configuration is not frozen")
    require(config["mode"] == "verified_five_executables_zero_build",
            "unexpected continuation mode")
    require(config["protocol_sha256"] == PROTOCOL_SHA256
            and config["protocol_config_sha256"] == PROTOCOL_CONFIG_SHA256,
            "frozen protocol reference changed")
    pin = config["input_failed_run"]
    require(pin["repository"] == "Otzaria/otzaria_search_engine"
            and pin["run_id"] == FAILED_RUN and pin["run_attempt"] == 1
            and pin["workflow_commit"] == FAILED_WORKFLOW
            and pin["artifact_id"] == FAILED_ARTIFACT
            and pin["artifact_name"] == "render-reservation-diagnostic-42441549309-1"
            and pin["zip_sha256"] == FAILED_ZIP_SHA256 and pin["zip_bytes"] == FAILED_ZIP_BYTES,
            "failed-run artifact pin changed")
    require(pin["regular_files"] == 75 and pin["manifest_entries"] == 74
            and len(pin["files"]) == 75 and pin["binaries"] == {},
            "failed-run inventory changed")
    for name, (size, sha) in BINARY_IDENTITIES.items():
        require(pin["files"][name] == {"bytes": size, "sha256": sha},
                "exact executable pin changed: " + name)
    require(config["timeouts_seconds"] == {"controller_budget": 1800, "measured_process": 180,
                                          "lifecycle_test": 180},
            "bounded continuation timeouts changed")
    require(config["expected_current_build_invocations"] == {"H": 0, "F": 0}
            and config["historical_measurements_imported"] is False,
            "zero-build/fresh-measurement contract changed")
    require(config["runtime"] == {"system": "Linux", "machine": "x86_64",
                                 "libc": ["glibc", "2.39"], "checkout_layout": "same_as_original_build"},
            "runtime compatibility/layout policy changed")
    return config


def validate_failed_api(p, run, artifact, pin):
    p.require(run["id"] == pin["run_id"] and run["run_attempt"] == 1
              and run["head_sha"] == pin["workflow_commit"] and run["status"] == "completed"
              and run["conclusion"] == "failure" and run["actor"]["login"] == "palmoni5",
              "input run differs from the exact known failed F run")
    p.require(artifact["id"] == pin["artifact_id"] and artifact["name"] == pin["artifact_name"]
              and artifact["size_in_bytes"] == pin["zip_bytes"]
              and artifact["digest"] == "sha256:" + pin["zip_sha256"] and not artifact["expired"],
              "input failed artifact identity changed")
    p.require(artifact["workflow_run"]["id"] == pin["run_id"]
              and artifact["workflow_run"]["head_sha"] == pin["workflow_commit"],
              "input failed artifact belongs to another workflow revision")


def validate_prior_F(p, core, archive, config, current_sources, roots):
    read = lambda name: json.loads(archive.read(name))
    status, original = read("status.json"), read("environment.json")
    p.require(status["state"] == "failed" and status["phase"] == "lifecycle-tests"
              and status["active_lifecycle_test"] == p.LIFECYCLE_TESTS[0]
              and status["build_invocations"] == {"H": 0, "F": 1}
              and status["completed_lifecycle_tests"] == 0
              and all(status[key] == 0 for key in ("completed_processes", "completed_main_processes",
                                                   "completed_wide_processes", "measured_process_attempts")),
              "input failure prefix is not the reviewed zero-measurement prefix")
    p.require("samples.jsonl" not in archive.namelist()
              and not any(name.startswith(("main/", "wide/")) for name in archive.namelist()),
              "input contains measurements that must not be reused")
    p.require(original["controller_commit"] == FAILED_WORKFLOW
              and original["controller_sha256"] == PROTOCOL_SHA256
              and original["config_sha256"] == PROTOCOL_CONFIG_SHA256
              and original["rustc"] == config["original"]["rustc"]
              and original["cargo"] == config["original"]["cargo"],
              "original F controller/config/toolchain provenance changed")
    p.require(p.byte_digest(archive.read("render-reservation-controller.py")) == PROTOCOL_SHA256
              and p.byte_digest(archive.read("config.json")) == PROTOCOL_CONFIG_SHA256,
              "original F protocol bytes disagree")
    p.require(read("source-provenance.json") == current_sources,
              "current pure H/F source differs from the compiled source")
    p.require(read("build-logs/F/source-before.json") == current_sources["F"]
              and read("build-logs/F/source-after.json") == current_sources["F"],
              "F tracked source was not clean before and after compilation")
    built = read("build-logs/F/provenance.json")
    p.require(all(built[key] == value for key, value in current_sources["F"].items())
              and built["build_invocations"] == 1
              and read("build-logs/F/duration.json")["returncode"] == 0,
              "F successful single-build provenance changed")
    command = read("build-logs/F/command.json")
    original_root = Path(command[command.index("--manifest-path") + 1]).parent.parent
    p.require(command == ["cargo", "test", "--manifest-path", str(original_root / "rust/Cargo.toml"),
                          "--locked", "--release", "--lib", "--test", "indexing_throughput",
                          "--test", config["wide"]["target"], "--example", core.LAYOUT_TARGET,
                          "--no-run", "--message-format=json"],
              "F original build target selection differs")
    p.require(roots["F"] == original_root, "current F checkout path differs from the original build")
    H_command = read("reused-H/original/build-logs/H/command.json")
    H_root = Path(H_command[H_command.index("--manifest-path") + 1]).parent.parent
    p.require(roots["H"] == H_root, "current H checkout path differs from the original build")
    expected_overlays = {"rust/examples/" + core.LAYOUT_TARGET + ".rs": config["original"]["layout_source_sha256"],
                         config["wide"]["overlay_path"]: config["wide"]["source_sha256"]}
    p.require(built["overlays"] == expected_overlays
              and read("build-logs/F/overlays.json") == {"tracked_source_changed": False,
                                                       "files_sha256": expected_overlays},
              "F diagnostic overlay provenance changed")
    graph = read("build-logs/F/dependencies-before.json")
    p.require(graph == read("build-logs/F/dependencies-with-overlays.json")
              and p.graph_digest(graph, original_root) == config["original"]["normalized_dependency_graph_sha256"]
              and built["normalized_dependency_graph_sha256"] == config["original"]["normalized_dependency_graph_sha256"]
              and p.byte_digest(archive.read("build-logs/F/dependencies-before.json")) == built["resolved_graph_sha256"],
              "F compiled dependency/features graph differs from frozen H")
    p.require(built["layout"] == config["original"]["H_original_layout"],
              "F original layout observation changed")
    markers = [json.loads(line.partition("OTZARIA_LAYOUT ")[2])
               for line in archive.read("build-logs/F/layout.log").decode().splitlines()
               if "OTZARIA_LAYOUT " in line]
    p.require(markers == [built["layout"]], "F raw layout marker differs from its receipt")
    selected = read("build-logs/F/selected-artifacts.json")
    wanted = {"indexing_throughput", config["wide"]["target"], core.LAYOUT_TARGET,
              config["lifecycle_tests"]["target_name"]}
    messages = [json.loads(line) for line in archive.read("build-logs/F/cargo.jsonl").decode().splitlines()]
    selected_again = {}
    for item in messages:
        if item.get("reason") == "compiler-artifact" and item.get("executable") and item["target"]["name"] in wanted:
            name = item["target"]["name"]
            p.require(name not in selected_again, "duplicate original Cargo executable target")
            selected_again[name] = item
    p.require(set(selected) == wanted and selected == selected_again
              and [item for item in messages if item.get("reason") == "build-finished"] == [
                  {"reason": "build-finished", "success": True}],
              "F original Cargo executable selection/success is not exact")
    library = selected[config["lifecycle_tests"]["target_name"]]
    p.require(library["target"]["kind"] == config["lifecycle_tests"]["target_kind"]
              and library["target"]["src_path"] == str(original_root / "rust/src/lib.rs")
              and library["profile"]["test"] is True,
              "selected F library artifact is not the exact test harness")
    harness = built["library_test_harness"]
    p.require(harness["profile_test"] is True and harness["retained_in_artifact"] is True
              and harness["target_kind"] == config["lifecycle_tests"]["target_kind"]
              and harness["target_name"] == config["lifecycle_tests"]["target_name"],
              "F library harness identity is incomplete")
    for target in ("indexing_throughput", config["wide"]["target"]):
        identity = BINARY_IDENTITIES["binaries/F-" + target]
        p.require((built["binary_targets"][target]["bytes"], built["binary_targets"][target]["sha256"]) == identity,
                  "F build receipt and exact benchmark executable disagree")
    p.require((harness["bytes"], harness["sha256"]) == BINARY_IDENTITIES["binaries/F-library-tests"],
              "F build receipt and library harness disagree")
    first = read("lifecycle/F-01.context.json")
    p.require(first["returncode"] == 0 and first["test_name"] == p.LIFECYCLE_TESTS[0]
              and first["binary_sha256"] == harness["sha256"]
              and first["stdout_sha256"] == p.byte_digest(archive.read("lifecycle/F-01.log")),
              "reviewed first-test process result changed")
    p.expect_rejected(lambda: p.validate_lifecycle_output(archive.read("lifecycle/F-01.log").decode(),
                                                        p.LIFECYCLE_TESTS[0]),
                      "known combined-stream strict-parser failure")
    p.require("the exact named lifecycle test did not report success" in archive.read("failure.log").decode(),
              "input failure was not the reviewed strict-parser failure")
    return built, original, {"H": str(H_root), "F": str(original_root)}


def reuse_failed_F(p, core, roots, current_sources, config, continuation, output, inputs_root, deadline, reused_H):
    directory = output / "prior-failed-run"
    directory.mkdir()
    pin = continuation["input_failed_run"]
    p.remaining_check(deadline, 180, "read and verify exact failed F artifact")
    run = p.github_json(f"/repos/{pin['repository']}/actions/runs/{pin['run_id']}")
    p.write_json(directory / "api-run.json", run)
    artifact = p.github_json(f"/repos/{pin['repository']}/actions/artifacts/{pin['artifact_id']}")
    p.write_json(directory / "api-artifact.json", artifact)
    validate_failed_api(p, run, artifact, pin)
    archive_path = directory / "original-failed-F-artifact.zip"
    p.download_reused_archive(archive_path, pin, directory / "download-progress.json")
    with zipfile.ZipFile(archive_path) as archive:
        inventory = p.verified_zip_inventory(archive, pin)
        p.require(inventory == pin["files"], "complete failed-F inventory differs")
        p.write_json(directory / "verified-original-inventory.json", inventory)
        # All original source/build/API/failure receipts are preserved exactly;
        # the five binaries are retained once, at the new artifact's root.
        for name in inventory:
            if name in BINARY_IDENTITIES:
                continue
            path = directory / "original" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(archive.read(name))
        built, historical_environment, original_roots = validate_prior_F(
            p, core, archive, config, current_sources, roots)
        p.require(json.loads(archive.read("reused-H/original/build-logs/H/provenance.json"))
                  == reused_H["original_build_provenance"],
                  "failed-F H origin disagrees with independently revalidated full E input")
        for name, (size, sha) in BINARY_IDENTITIES.items():
            if name.startswith("binaries/H-"):
                existing = output / name
                p.require(existing.stat().st_size == size and p.file_digest(existing) == sha,
                          "H bytes from E differ from failed-F executable input")
            else:
                path = output / name
                path.parent.mkdir(exist_ok=True)
                path.write_bytes(archive.read(name))
                path.chmod(0o555)
                p.require(path.stat().st_size == size and p.file_digest(path) == sha,
                          "F exact executable changed on extraction")
    retained = inputs_root / "verified-original-failed-F-artifact.zip"
    receipt = {
        "mode": "verified_reuse_zero_build", "build_invocations": {"H": 0, "F": 0},
        "input_run": {key: pin[key] for key in ("run_id", "run_attempt", "workflow_commit", "artifact_id", "zip_sha256", "zip_bytes")},
        "all_75_original_members_verified": True, "all_74_manifest_entries_verified": True,
        "all_nonbinary_original_members_preserved": True,
        "five_exact_executable_paths": sorted(BINARY_IDENTITIES),
        "H_origin": "Both identical H executables independently recovered and verified against complete original E ZIP.",
        "source_checkout_paths_equal_original_build": original_roots,
        "original_rustc": historical_environment["rustc"], "original_cargo": historical_environment["cargo"],
        "original_build_environment": "prior-failed-run/original/environment.json",
        "historical_build_invocations": {"H_in_E": 1, "H_in_failed_F": 0, "F_in_failed_F": 1},
        "historical_measurements_imported": False, "historical_lifecycle_results_imported_as_current_pass": False,
        "original_input_zip_retained_outside_result_artifact": str(retained),
        "complete_original_artifact_is_separately_preserved": True,
    }
    p.write_json(directory / "verification.json", receipt)
    archive_path.replace(retained)
    retained.chmod(0o444)
    binaries = {target: {"path": str(output / ("binaries/F-" + target)),
                         "bytes": BINARY_IDENTITIES["binaries/F-" + target][0],
                         "sha256": BINARY_IDENTITIES["binaries/F-" + target][1]}
                for target in ("indexing_throughput", config["wide"]["target"])}
    harness = built["library_test_harness"] | {"path": str(output / "binaries/F-library-tests")}
    record = current_sources["F"] | {
        "build_mode": "verified_reuse", "build_invocations": 0,
        "original_build_invocations": 1, "binary_targets": binaries, "library_test_harness": harness,
        "original_build_provenance": built, "reused_from": receipt["input_run"],
        "layout": built["layout"], "layout_executable_sha256": built["layout_executable_sha256"],
        "normalized_dependency_graph_sha256": built["normalized_dependency_graph_sha256"],
        "historical_timing_observations_imported": False,
    }
    p.write_json(output / "reused-F-provenance.json", record)
    return record, receipt


def runtime_environment(p, config, continuation):
    blocked = {"RAYON_NUM_THREADS", "RAYON_RS_NUM_CPUS", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
               "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTDOCFLAGS", "RUSTC", "CARGO_BUILD_TARGET"}
    names = [key for key, value in os.environ.items() if value and
             (key in blocked or key.startswith("CARGO_PROFILE_RELEASE_")
              or (key.startswith("CARGO_TARGET_") and key.endswith("_RUSTFLAGS")))]
    p.require(not names, "unexpected compiler/worker override names: " + str(sorted(names)))
    policy = continuation["runtime"]
    actual = {"system": platform.system(), "machine": platform.machine(), "libc": list(platform.libc_ver())}
    p.require(all(actual[key] == policy[key] for key in actual),
              "current runtime differs from frozen Linux/x86_64/glibc compatibility policy")
    environment = os.environ | {"CARGO_BUILD_JOBS": "1", "OTZARIA_BENCH_LINES": "12000", "OTZARIA_BENCH_ECONOMY": "1"}
    for key in ("GH_TOKEN", "GITHUB_TOKEN", "OTZARIA_BENCH_NOTES",
                config["wide"]["order_environment"], config["wide"]["snapshot_environment"]):
        environment.pop(key, None)
    return environment, actual


def run_lifecycle_tests(p, core, built, config, environment, output, deadline, status):
    directory = output / "lifecycle"
    directory.mkdir()
    harness = built["library_test_harness"]
    binary = Path(harness["path"])
    p.require(p.file_digest(binary) == harness["sha256"] and binary.stat().st_size == harness["bytes"],
              "library harness identity changed before lifecycle tests")
    completed_tests = []
    for number, test_name in enumerate(config["lifecycle_tests"]["test_names"], 1):
        p.remaining_check(deadline, 180, "required fresh lifecycle test " + test_name)
        command = [str(binary), test_name, "--exact", "--test-threads=1", "--format=pretty", "--color=never"]
        log_path, stderr_path = directory / f"F-{number:02}.log", directory / f"F-{number:02}.stderr.log"
        context_path = directory / f"F-{number:02}.context.json"
        context = {"command": command, "test_name": test_name, "before": core.snapshot(),
                   "binary_sha256": harness["sha256"], "timeout_seconds": 180,
                   "stdout_file": log_path.name, "stderr_file": stderr_path.name,
                   "streams_captured_separately": True,
                   "scope": "Fresh required lifecycle gate before, and outside, the fixed 96 measured processes."}
        p.write_json(context_path, context)
        status["phase"], status["active_lifecycle_test"] = "lifecycle-tests", test_name
        p.write_json(output / "status.json", status)
        started, completed = time.monotonic(), None
        try:
            with log_path.open("w") as stdout, stderr_path.open("w") as stderr:
                completed = subprocess.run(command, stdout=stdout, stderr=stderr,
                                           env=environment, timeout=180, check=False)
        finally:
            context["after"] = core.snapshot()
            context["process_wall_seconds"] = time.monotonic() - started
            context["returncode"] = completed.returncode if completed is not None else None
            for field, path in (("stdout_sha256", log_path), ("stderr_sha256", stderr_path)):
                if path.exists():
                    context[field] = p.file_digest(path)
            p.write_json(context_path, context)
        completed.check_returncode()
        result = p.validate_lifecycle_output(log_path.read_text(), test_name)
        p.write_json(directory / f"F-{number:02}.verified.json", result)
        completed_tests.append(result)
        status["completed_lifecycle_tests"] = len(completed_tests)
        status.pop("active_lifecycle_test", None)
        p.write_json(output / "status.json", status)
        p.write_json(directory / "summary.json", {"completed": len(completed_tests), "expected": 3,
                     "tests": completed_tests, "all_required_tests_passed": len(completed_tests) == 3,
                     "streams_captured_separately": True, "historical_passes_counted": 0})
    p.require(p.file_digest(binary) == harness["sha256"], "library harness changed during lifecycle tests")
    return {"all_required_tests_passed": True, "completed": 3, "tests": completed_tests,
            "library_test_harness": harness, "outside_measurement_schedule": True,
            "streams_captured_separately": True, "historical_passes_counted": 0,
            "scope": config["lifecycle_tests"]["scope"]}


def self_test(p, core, config, continuation):
    original = p.self_test(core, config)
    for name in ("main", "build", "dependency_graph", "compiler_environment"):
        p.expect_rejected(lambda name=name: getattr(p, name)(), "zero-build forbidden " + name)
    test_name = p.LIFECYCLE_TESTS[0]
    with tempfile.TemporaryDirectory(prefix="otzaria-continuation-python-selftest-") as temporary:
        directory = Path(temporary)
        # Only this synthetic Python producer is executed by self-test. No
        # artifact controller, Rust test or downloaded executable is run.
        producer = ("import sys\n"
                    "sys.stdout.write('running 1 test\\ntest ' + sys.argv[1] + ' ... '); sys.stdout.flush()\n"
                    "sys.stderr.write('[synthetic stderr context]\\n'); sys.stderr.flush()\n"
                    "sys.stdout.write('ok\\n\\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 928 filtered out; finished in 0.01s\\n'); sys.stdout.flush()\n")
        with (directory / "stdout").open("w") as stdout, (directory / "stderr").open("w") as stderr:
            subprocess.run([sys.executable, "-c", producer, test_name], stdout=stdout, stderr=stderr,
                           check=True, timeout=10)
        p.require(p.validate_lifecycle_output((directory / "stdout").read_text(), test_name)["filtered_out"] == 928,
                  "separate stdout strict-parser reproduction failed")
        p.require((directory / "stderr").read_text() == "[synthetic stderr context]\n",
                  "separate stderr was not retained")
    return {"state": "pass", "frozen_protocol": original, "zero_build_entry_points_disabled": True,
            "split_stream_synthetic_python_check": True, "downloaded_executables_run": 0,
            "rust_builds": 0, "rust_toolchain_queries": 0, "fixed_counts": config["counts"]}


def main():
    here = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("head", "candidate", "out", "input-root"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--config", type=Path, default=here / "shared_core_render_reservation_continuation_config.json")
    parser.add_argument("--protocol", type=Path, default=here / "shared_core_render_reservation_diagnostic.py")
    parser.add_argument("--protocol-config", type=Path, default=here / "shared_core_render_reservation_config.json")
    parser.add_argument("--helper", type=Path, default=here / "shared_core_economy_helper.py")
    parser.add_argument("--wide-fixture", type=Path, default=here / "shared_core_render_scheduling.rs")
    parser.add_argument("--fixture-manifest", type=Path, default=here / "shared_core_render_reservation_manifest.json")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if sys.flags.optimize:
        parser.error("run without Python optimization; frozen helper assertions must remain active")
    if args.self_test:
        p = import_protocol(args.protocol)
        continuation = load_continuation(args.config)
        require(digest(args.protocol_config) == PROTOCOL_CONFIG_SHA256, "approved F configuration bytes changed")
        config = p.load_config(args.protocol_config, frozen=True)
        core = p.import_helper(args.helper)
        p.load_fixture_manifest(args.fixture_manifest, config)
        print(json.dumps(self_test(p, core, config, continuation), sort_keys=True))
        return
    if any(getattr(args, key) is None for key in ("head", "candidate", "out", "input_root")):
        parser.error("--head, --candidate, --out and --input-root are required")
    head, candidate, output, inputs_root = (getattr(args, key).resolve() for key in ("head", "candidate", "out", "input_root"))
    require(head != candidate, "H and F require separate pure source checkouts")
    require(not (output == inputs_root or output.is_relative_to(inputs_root) or inputs_root.is_relative_to(output)),
            "retained input ZIPs must be separate from the result artifact")
    for root in (head, candidate):
        require(not any(path == root or path.is_relative_to(root) for path in (output, inputs_root)),
                "evidence/input paths must be outside pure source checkouts")
    require(not inputs_root.exists(), "input staging directory already exists")
    output.mkdir(parents=True, exist_ok=False)
    status = {"state": "running", "phase": "preflight", "completed_processes": 0,
              "completed_main_processes": 0, "completed_wide_processes": 0,
              "measured_process_attempts": 0, "build_invocations": {"H": 0, "F": 0},
              "completed_lifecycle_tests": 0, "mode": "verified_five_executables_zero_build",
              "performance_gate": "none; independent interpretation and behavioral gates are required"}
    records, snapshot_references, comparisons = [], {}, []
    p, core, config = None, None, None
    write_json(output / "status.json", status)
    try:
        p = import_protocol(args.protocol)
        signal.signal(signal.SIGTERM, p.interrupt_signal)
        continuation = load_continuation(args.config)
        require(digest(args.protocol_config) == PROTOCOL_CONFIG_SHA256, "approved F configuration bytes changed")
        config = p.load_config(args.protocol_config, frozen=True)
        core = p.import_helper(args.helper)
        p.load_fixture_manifest(args.fixture_manifest, config)
        deadline = time.monotonic() + continuation["timeouts_seconds"]["controller_budget"]
        for source, name in ((Path(__file__), "continuation-controller.py"), (args.config, "continuation-config.json"),
                             (args.protocol, "render-reservation-controller.py"), (args.protocol_config, "config.json"),
                             (args.helper, "phase1-helper.py"), (args.fixture_manifest, "wide-fixture-manifest.json"),
                             (args.wide_fixture, "render_scheduling.rs")):
            shutil.copy2(source, output / name)
        require(digest(args.wide_fixture) == config["wide"]["source_sha256"], "wide fixture changed")
        shutil.copy2(head / config["original"]["fixture_path"], output / "indexing_throughput.rs")
        (output / "layout_metadata.rs").write_text(core.LAYOUT_SOURCE)
        p.write_json(output / "self-test.json", self_test(p, core, config, continuation))
        roots = {"H": head, "F": candidate}
        initial_sources = {label: p.source_provenance(root, label, config) for label, root in roots.items()}
        p.write_json(output / "source-provenance.json", initial_sources)
        for label, root in roots.items():
            for relative in p.SOURCE_PATHS:
                destination = output / "source" / f"{label}-{Path(relative).name}"
                destination.parent.mkdir(exist_ok=True)
                shutil.copy2(root / relative, destination)
        environment, runtime = runtime_environment(p, config, continuation)
        allowed = sorted(os.sched_getaffinity(0))
        topology = {cpu: {key: Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/{key}").read_text().strip()
                          for key in ("physical_package_id", "core_id", "thread_siblings_list", "core_siblings_list")}
                    for cpu in allowed}
        affinities = p.select_affinities(topology)
        rows = p.schedule(core, config, affinities)
        p.validate_schedule(core, config, rows, affinities)
        p.write_json(output / "predeclared-schedule.json", rows)
        p.write_json(output / "wide-case-inventory.json",
                     {order: p.ordered_specs(config, order) for order in ("forward", "reverse")})
        expected_stamps = [[int(value) for value in row] for row in config["original"]["expected_document_stamps_decimal_strings"]]
        metadata = {
            "revisions": config["revisions"], "original": config["original"],
            "controller_commit": p.git(Path(__file__).resolve().parents[1], "rev-parse", "HEAD"),
            "controller_sha256": digest(output / "continuation-controller.py"),
            "continuation_config_sha256": digest(output / "continuation-config.json"),
            "frozen_protocol_sha256": PROTOCOL_SHA256, "config_sha256": PROTOCOL_CONFIG_SHA256,
            "verified_helper_sha256": p.HELPER_SHA256,
            "original_fixture_sha256": digest(output / "indexing_throughput.rs"),
            "wide_fixture_sha256": digest(output / "render_scheduling.rs"),
            "wide_fixture_manifest_sha256": digest(output / "wide-fixture-manifest.json"),
            "layout_source_sha256": digest(output / "layout_metadata.rs"),
            "predeclared_schedule_sha256": digest(output / "predeclared-schedule.json"),
            "wide_case_inventory_sha256": digest(output / "wide-case-inventory.json"),
            "platform": platform.platform(), "uname": list(platform.uname()), "python": sys.version,
            "runtime_compatibility": runtime, "allowed_affinity": allowed, "topology": topology,
            "measurement_affinities": affinities, "cpu_topology": json.loads(p.run_text(["lscpu", "--json"])),
            "tmpdir": tempfile.gettempdir(), "tmpdir_filesystem": p.run_text(["df", "-T", tempfile.gettempdir()]),
            "runtime_environment": {key: environment[key] for key in ("CARGO_BUILD_JOBS", "OTZARIA_BENCH_LINES", "OTZARIA_BENCH_ECONOMY")},
            "unset_runtime_environment": ["OTZARIA_BENCH_NOTES", "RAYON_NUM_THREADS", "RAYON_RS_NUM_CPUS"],
            "expected_stamps": expected_stamps, "timeouts_seconds": continuation["timeouts_seconds"],
            "build_invocations": {"H": 0, "F": 0}, "rust_toolchain_queries": 0,
            "build_toolchain_from_original_receipts": {"rustc": config["original"]["rustc"], "cargo": config["original"]["cargo"]},
            "current_host_is_measurement_host_not_build_host": True,
            "github": {key: os.environ.get(key) for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA",
                       "GITHUB_REF", "GITHUB_ACTOR", "RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion")},
            "notes": [
                "Zero builds and zero Rust toolchain queries in this continuation; exact five executable inputs are pinned.",
                "Original E and failed-F source/build/dependency/layout/compiler receipts are historical, separately preserved.",
                "The three exact lifecycle tests run fresh with separate stdout and stderr; no historical pass is counted.",
                "Main64, wide32, both topologies, both case orders and all controls use unchanged frozen functions.",
                "Wide14 timed cases use 5 warmups and 30 measurements each; remaining44 cases are behavior-only.",
                "Wide probes are idle after indexing; original main probes include a concurrent indexing window.",
                "No historical timing samples are imported and controls remain outside real-effect medians.",
                "Wide p99 is the maximum of 30 samples. Same-binary controls and all regressions remain descriptive.",
                "No automatic promotion or production source change follows from completion.",
            ],
        }
        p.write_json(output / "environment.json", metadata)
        inputs_root.mkdir(parents=True, exist_ok=False)
        status["phase"] = "verify-original-H-chain"
        p.write_json(output / "status.json", status)
        metadata["H"] = p.reuse_baseline(core, head, config, output, inputs_root, deadline)
        p.write_json(output / "environment.json", metadata)
        status["phase"] = "verify-and-reuse-failed-F"
        p.write_json(output / "status.json", status)
        metadata["F"], metadata["input_reuse_verification"] = reuse_failed_F(
            p, core, roots, initial_sources, config, continuation, output, inputs_root, deadline, metadata["H"])
        p.write_json(output / "environment.json", metadata)
        binaries, hashes = {}, {}
        for label in p.LABELS:
            for target, details in metadata[label]["binary_targets"].items():
                binaries[(label, target)], hashes[(label, target)] = Path(details["path"]), details["sha256"]
        metadata["lifecycle_tests"] = run_lifecycle_tests(
            p, core, metadata["F"], config, environment, output, deadline, status)
        require(metadata["lifecycle_tests"]["all_required_tests_passed"]
                and status["completed_lifecycle_tests"] == 3
                and status["build_invocations"] == {"H": 0, "F": 0},
                "zero-build reuse and three fresh lifecycle gates must complete before measurements")
        metadata["binary_sha256"] = {label: {target: hashes[(label, target)]
                                    for target in ("indexing_throughput", config["wide"]["target"])}
                                    for label in p.LABELS}
        for key, path in binaries.items():
            require(digest(path) == hashes[key], "benchmark executable changed before measurement")
        p.write_json(output / "environment.json", metadata)
        rows = p.schedule(core, config, affinities, hashes)
        p.validate_schedule(core, config, rows, affinities, hashes)
        p.write_json(output / "resolved-schedule.json", rows)
        p.write_json(output / "measurement-start.json", core.snapshot())
        (output / "main").mkdir()
        (output / "wide").mkdir()
        status["phase"] = "measure"
        with (output / "samples.jsonl").open("w") as samples, (output / "snapshot-comparisons.jsonl").open("w") as matches:
            for entry in rows:
                p.remaining_check(deadline, 180, f"measurement run {entry['run']}")
                status["active_process"] = entry
                status["measured_process_attempts"] += 1
                p.write_json(output / "status.json", status)
                target = entry["target"]
                binary = binaries[(entry["actual_binary"], target)]
                if target == "indexing_throughput":
                    record = core.measure(entry, binary, output / "main", entry["affinity"], environment, expected_stamps)
                    status["completed_main_processes"] += 1
                else:
                    record = p.measure_wide(core, entry, binary, output / "wide", environment, config)
                    compared = p.compare_snapshots(record, snapshot_references, output / "wide")
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
                p.write_json(output / "status.json", status)
                p.write_json(output / "analysis.json", p.analyse(core, config, records))
                progress = {key: entry[key] for key in ("run", "target", "actual_binary", "kind", "topology_case", "case_order")}
                progress["process_wall_seconds"] = record["process"]["process_wall_seconds"]
                if target == "indexing_throughput":
                    progress["search_during_p95_ms"] = record["sample"]["search_during_p95_ms"]
                else:
                    progress["verified_case_snapshots"] = len(record["cases"])
                print(json.dumps(progress, sort_keys=True), flush=True)
        p.validate_schedule(core, config, records, affinities, hashes)
        require(len(snapshot_references) == 58 and len(comparisons) == 32 * 58,
                "incomplete full-output comparison coverage")
        for label, root in roots.items():
            require(p.source_provenance(root, label, config) == initial_sources[label],
                    "pure source changed during measurement")
        for key, path in binaries.items():
            require(digest(path) == hashes[key], "benchmark executable changed during measurement")
        status["state"], status["phase"] = "complete", "complete"
        status["correctness_gate"] = "All58 complete output snapshots match H byte-for-byte in all32 wide processes; original fixture stamps unchanged."
        status["lifecycle_gate"] = "All three exact strengthened library tests passed fresh before the fixed measurement schedule."
    except BaseException:
        status["state"] = "failed"
        (output / "failure.log").write_text(traceback.format_exc())
        raise
    finally:
        write_json(output / "status.json", status)
        if p is not None and core is not None and config is not None:
            write_json(output / "analysis.json", p.analyse(core, config, records))
        files = sorted(path for path in output.rglob("*") if path.is_file() and path != output / "SHA256SUMS")
        (output / "SHA256SUMS").write_text("".join(f"{digest(path)}  {path.relative_to(output)}\n" for path in files))


if __name__ == "__main__":
    main()

