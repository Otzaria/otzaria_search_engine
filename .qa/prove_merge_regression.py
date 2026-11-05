from pathlib import Path
import subprocess

paths = ["rust/Cargo.toml", "rust/Cargo.lock"]
originals = {path: Path(path).read_text() for path in paths}
try:
    for path in paths:
        baseline = subprocess.check_output(
            ["git", "show", "13f19d11eb5a4ba4c2e04a5c097e7500b4e6e827:" + path],
            text=True,
        )
        Path(path).write_text(baseline)
    result = subprocess.run(
        ["cargo", "test", "--manifest-path", "rust/Cargo.toml", "--locked",
         "--test", "index_writer_shutdown",
         "dropping_writer_finishes_metadata_", "--", "--nocapture",
         "--test-threads=1"],
        text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )
finally:
    for path, original in originals.items():
        Path(path).write_text(original)
Path("merge-before.log").write_text(result.stdout)
print(result.stdout)
assert result.returncode != 0, "The original dependency unexpectedly passed"
assert "writer teardown returned while its merge still owned files" in result.stdout, "Expected premature close, not compilation or infrastructure failure"
assert "0 passed; 2 failed" in result.stdout, "Both accepted-metadata regressions must fail before the fix"
print("Confirmed: remove and rename regressions both fail with the original Tantivy revision")
