"""Check the adb transport wrapper against an actual child process."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


WRAPPER = Path(__file__).resolve().parents[2] / "tool/ci/adb_non_streaming.sh"
FAKE_ADB = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

Path(os.environ["ADB_CAPTURE"]).write_text(json.dumps(sys.argv[1:]))
print("underlying adb stdout")
print("underlying adb stderr", file=sys.stderr)
sys.exit(int(os.environ["ADB_EXIT_CODE"]))
'''


class AdbNonStreamingTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        # A space in the executable path checks that the wrapper quotes it too.
        self.real_adb = self.root / "real adb"
        self.real_adb.write_text(FAKE_ADB)
        self.real_adb.chmod(0o755)
        self.capture = self.root / "arguments.json"

    def run_adb(self, args, *, exit_code=0):
        result = subprocess.run(
            ["bash", str(WRAPPER), *args], text=True, capture_output=True,
            env={**os.environ, "OTZARIA_REAL_ADB": str(self.real_adb),
                 "ADB_CAPTURE": str(self.capture),
                 "ADB_EXIT_CODE": str(exit_code)}, timeout=10,
        )
        self.assertTrue(self.capture.exists(), result.stderr)
        return result, json.loads(self.capture.read_text())

    def test_install_uses_push_transport_and_preserves_output(self):
        result, args = self.run_adb(["install", "-t", "-r", "app debug.apk"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(args, ["install", "--no-streaming", "-t", "-r", "app debug.apk"])
        self.assertIn("--no-streaming (push transport)", result.stderr)
        self.assertIn("underlying adb stderr", result.stderr)
        self.assertEqual(result.stdout, "underlying adb stdout\n")

    def test_global_options_with_values_stay_before_install(self):
        for option, value in [("-s", "device serial"), ("-t", "42"),
                              ("-H", "host name"), ("-P", "5037"),
                              ("-L", "tcp:localhost:5037")]:
            with self.subTest(option=option):
                result, args = self.run_adb([option, value, "install", "app.apk"])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(args, [option, value, "install", "--no-streaming", "app.apk"])

    def test_global_flags_and_multiple_options_are_forwarded(self):
        for flag in ["-a", "-d", "-e"]:
            with self.subTest(flag=flag):
                prefix = [flag, "-H", "localhost", "-P", "5037"]
                result, args = self.run_adb([*prefix, "install", "-r", "app.apk"])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(args, [*prefix, "install", "--no-streaming", "-r", "app.apk"])

    def test_conflicting_and_repeated_transport_flags_are_removed(self):
        result, args = self.run_adb([
            "-s", "emulator-5554", "install", "--streaming", "-t",
            "--no-streaming", "-r", "--streaming", "--no-streaming", "app.apk",
        ])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(args, ["-s", "emulator-5554", "install", "--no-streaming", "-t", "-r", "app.apk"])

    def test_other_subcommands_and_their_install_arguments_are_untouched(self):
        for command in [
            ["-s", "emulator-5554", "shell", "echo", "install", "--streaming"],
            ["shell", "pm", "install", "app with spaces.apk"],
            ["uninstall", "install"], ["version"], ["--version"], ["help"],
            ["start-server"], [],
        ]:
            with self.subTest(command=command):
                result, args = self.run_adb(command)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(args, command)
                self.assertEqual(result.stderr, "underlying adb stderr\n")

    def test_install_as_a_global_option_value_is_not_the_subcommand(self):
        command = ["-s", "install", "shell", "echo", "install"]
        result, args = self.run_adb(command)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(args, command)
        self.assertEqual(result.stderr, "underlying adb stderr\n")

    def test_missing_global_value_and_unknown_options_are_left_to_adb(self):
        for command in [["-s"], ["--unknown", "install", "app.apk"]]:
            with self.subTest(command=command):
                result, args = self.run_adb(command, exit_code=64)
                self.assertEqual(result.returncode, 64, result.stderr)
                self.assertEqual(args, command)

    def test_apk_argument_is_never_evaluated_by_a_shell(self):
        marker = self.root / "unexpected-file"
        apk = f"a 'quoted' $(touch {marker}) path.apk"
        result, args = self.run_adb(["install", apk])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(args, ["install", "--no-streaming", apk])
        self.assertFalse(marker.exists())

    def test_underlying_failure_statuses_are_preserved(self):
        for status in [1, 7, 64, 124]:
            with self.subTest(status=status):
                result, args = self.run_adb(["install", "app.apk"], exit_code=status)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(args, ["install", "--no-streaming", "app.apk"])


if __name__ == "__main__":
    unittest.main()
