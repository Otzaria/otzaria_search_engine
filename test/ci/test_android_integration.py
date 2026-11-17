"""Exercise the CI runner with fake tools; no SDK, emulator or sleeps needed."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[2] / "tool/ci/android_integration.sh"
FAKE_TOOL = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
args = sys.argv[1:]
state_path = Path(os.environ["FAKE_STATE"])
state = json.loads(state_path.read_text())
state["calls"].append([name, *args])
status = 0
if name == "flutter":
    result = state["results"].pop(0)
    status = result[0]
    print(result[1])
elif name == "adb":
    if "getprop" in args:
        print("0" if state.get("not_ready") else "1")
    elif "pm" in args:
        print("package:/system/framework/framework-res.apk")
elif name == "timeout":
    state_path.write_text(json.dumps(state))
    os.execvp(args[2], args[2:])
state_path.write_text(json.dumps(state))
sys.exit(status)
'''


class AndroidIntegrationTest(unittest.TestCase):
    def run_script(self, mode, results, *, not_ready=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state_file = root / "state.json"
            state_file.write_text(json.dumps({
                "results": results, "calls": [], "not_ready": not_ready,
            }))
            for name in ["flutter", "adb", "timeout", "sleep"]:
                tool = root / name
                tool.write_text(FAKE_TOOL)
                tool.chmod(0o755)
            result = subprocess.run(
                ["bash", str(SCRIPT), mode], text=True, capture_output=True,
                env={**os.environ, "PATH": f"{root}:{os.environ['PATH']}",
                     "FAKE_STATE": str(state_file)}, timeout=15,
            )
            state = json.loads(state_file.read_text())
            return result, state["calls"]

    def test_success_builds_once(self):
        result, calls = self.run_script("build", [[0, "built"]])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sum(c[0] == "flutter" for c in calls), 1)
        flutter = next(c for c in calls if c[0] == "flutter")
        self.assertIn("--dart-define=INTEGRATION_TEST_SHOULD_REPORT_RESULTS_TO_NATIVE=false", flutter)

    def test_maven_rate_limit_uses_backoff_then_succeeds(self):
        result, calls = self.run_script("build", [
            [1, "Received status code 429 from server: Too Many Requests"],
            [1, "Received status code 503 from server"], [0, "built"],
        ])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([c for c in calls if c[0] == "sleep"],
                         [["sleep", "30"], ["sleep", "60"]])

    def test_compile_failure_is_not_retried(self):
        result, calls = self.run_script("build", [[7, "error: broken source"]])
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertFalse(any(c[0] == "sleep" for c in calls))

    def test_download_retries_are_bounded(self):
        result, calls = self.run_script("build", [[9, "Connection reset"]] * 3)
        self.assertEqual(result.returncode, 9, result.stderr)
        self.assertEqual(sum(c[0] == "flutter" for c in calls), 3)

    def test_build_timeout_is_not_blindly_retried(self):
        result, calls = self.run_script("build", [[124, "build timed out"]])
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertEqual(sum(c[0] == "flutter" for c in calls), 1)

    def test_integration_reuses_dependency_resolution(self):
        result, calls = self.run_script("test", [[0, "passed"]])
        self.assertEqual(result.returncode, 0, result.stderr)
        flutter = next(c for c in calls if c[0] == "flutter")
        self.assertIn("--no-pub", flutter)

    def test_failed_assertion_stays_red_without_retry(self):
        result, calls = self.run_script("test", [[1, "assertion failed"]])
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(any("reboot" in c for c in calls))
        self.assertEqual(sum(c[0] == "flutter" for c in calls), 1)

    def test_timeout_reboots_before_one_retry(self):
        result, calls = self.run_script("test", [[124, "hang"], [0, "passed"]])
        self.assertEqual(result.returncode, 0, result.stderr)
        reboots = [i for i, c in enumerate(calls) if c[0] == "adb" and "reboot" in c]
        tests = [i for i, c in enumerate(calls) if c[0] == "flutter"]
        self.assertEqual(len(reboots), 1)
        self.assertLess(tests[0], reboots[0])
        self.assertLess(reboots[0], tests[1])

    def test_repeated_timeout_stays_red(self):
        result, calls = self.run_script("test", [[124, "hang"]] * 2)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertEqual(sum(c[0] == "flutter" for c in calls), 2)

    def test_package_manager_readiness_is_bounded(self):
        result, calls = self.run_script("test", [], not_ready=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c[0] == "flutter" for c in calls))
        self.assertEqual(sum(c[0] == "adb" for c in calls), 30)


if __name__ == "__main__":
    unittest.main()
