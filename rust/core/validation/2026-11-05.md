# Shared-core validation

Validated on Linux on 2026-11-05. The implementation and compatibility fixture are
at `75c7dfc6f0a6ee5f7d850b646d56166fb1bfb741` (implementation introduced by
`0969a45e7c774abb229746e0f254d423a445a94e`), based on canonical 7.0.1 commit
`225a6d5d7265c478a64d3a609a536d0deed433e7`.

Implementation/fix commit IDs in this report identify local validation snapshots.
Publication may assign different commit IDs while preserving the reviewed source
tree; they are not dependency pins or a claim that those local commits are remote.

## Build and test results

Commands ran from the repository root with `CARGO_BUILD_JOBS=1`,
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0` and
`CARGO_PROFILE_TEST_DEBUG=0`. A writable temporary directory was supplied through
`TMPDIR`; the workspace disk was full, so build outputs and temporary fixtures
were stored separately. Dependencies were resolved from the locked offline cache.

| Check | Command | Result |
| --- | --- | --- |
| Headless facade | `cargo test --manifest-path rust/core/Cargo.toml --locked --offline --tests -- --test-threads=1` | 8 passed, 0 failed |
| Headless engine | `cargo test --manifest-path rust/Cargo.toml --locked --offline --no-default-features --features sqlite-bundled --lib -- --test-threads=1` | 844 passed, 1 environment failure, 27 pre-existing ignored |
| Environment failure rerun | The same compiled engine test executable, `--exact index_directory::tests::a_relative_path_opens_and_is_written_into --test-threads=1`, from a writable working directory | 1 passed; 845 engine unit tests validated in total |
| Default Flutter bridge | `cargo check --manifest-path rust/Cargo.toml --locked --offline --lib` | passed |
| Host SQLite + bridge | `cargo check --manifest-path rust/Cargo.toml --locked --offline --lib --no-default-features --features sqlite-host,flutter-bridge` | passed |
| Transport-free dependencies | `cargo tree --manifest-path rust/core/Cargo.toml --locked --offline -e normal --prefix none --format '{p}'` | no FRB runtime, Tokio, rmcp, allo-isolate, dart-sys or ort |
| Formatting/patch hygiene | Rustfmt on changed modules and `git diff --check` | passed |

The one engine-suite failure was `StorageFull` while a pre-existing test used
`tempdir_in(".")`; it was not a search or locking failure. The test passed without
recompilation when its working directory was writable. No test was disabled to
obtain these results.

The facade tests exercise 21 lexical mutation entry points, including invalid PDF
byte inputs, before parsing or writer-lock acquisition. They also cover semantic
mutation refusal, a live writer beside a read-only handle, 64 searches from four
threads, stable IDs under external book-order changes, refresh after a commit,
missing/legacy/future metadata refusal, and close/replace/reopen of an index.
The two new line-source tests passed in the engine suite, including a same-path
refresh with an active independent scan and no cached row-window connection.

The first new highlight assertion incorrectly expected `<b>`; execution exposed
that the canonical default is `<font color=red>`. The test now checks the exact
canonical tag. A replacement test initially omitted creation of the new directory;
the fixture was corrected to honor the existing constructor's contract.

## Independent read-only audit and final regression checks

A second independent source audit identified two sidecar operations whose names
concealed writes. In the pinned sidecar, `verify_semantic_vectors` can persist a
`segments/<id>.corrupt` marker; `open_semantic_artifact` performs recovery and
removes staging files and unused data before exposing its read-only session.
Both now reject a read-only engine before input parsing or backend dispatch.
The normal Flutter-owned writer-capable handles retain their existing behavior.

The fixes are in `41d22a6` and `82f0f32`. The existing sidecar-mutation test also
checks artifact open and install with invalid inputs, ensuring the read-only
refusal precedes filesystem/model validation. Final source at `82f0f32` passed
all eight facade tests twice: with default headless features and with
`--features semantic-integration`. Both used the ordinary manifest crate types
and exact locked dependencies. No test was disabled. The latter build initially
ran out of temporary build storage while creating a native archive; after
reclaiming only obsolete generated artifacts, the full test invocation passed.

The independent audit found no remaining public mutation bypass. Runtime logs
are `core-guard-tests.log` and `core-semantic-guard-tests.log` in the validation
workspace. The original 845-engine-test and PR #77 integration runs below were on
the earlier implementation snapshot; these final changes add guards to two
semantic methods and do not alter writer-close state, index schemas or search
algorithms. Full regenerated-bridge and platform CI remains a merge gate.

## Independently produced existing index

`tests/fixtures/canonical-7.0.1` was generated by the unmodified canonical search
implementation at the baseline commit above, not by the extracted implementation.
The only temporary baseline manifest change selected `crate-type = ["rlib"]` to
avoid unneeded native archives. The engine source and locked dependencies were
unchanged.

The nine-line synthetic fixture has three stable book keys and different catalogue
and generation orders. Its 16-case golden contains every result field, snippets
and highlights, merged siblings, counts, pagination, facets, vocalized matching,
advanced negative matching and exact/phrase/fuzzy search in all three orders.
The headless reader matched the complete golden and left every index file
byte-identical after closing. The fixture README and generator document how to
reproduce it; newline conversion is disabled for its index files.

## Writer lifecycle PR #77 integration

Checked against PR #77 head `2da23091237ed0f0a612de3b2d18cfc99d8b7d2a`, fetched
during this validation. It was still a separate change. No remote branch or PR
was merged.

A temporary local combination of that head and the shared-core commit required
four conflict resolutions in `search_engine.rs`: keep both `read_only` and
`writer_closed` in the engine and its two constructors, and retain both checks in
`open_writer`. The newer Tantivy pin from PR #77,
`b6dfc7bc93336df1b996a6701b8c86c80206b8d1`, was retained as the source under test.

For the temporary integration build, a clean checkout verified at that exact
Tantivy commit was selected through a local `[patch]` path because the full
workspace disk prevented the isolated Git cache from updating. A comparison of
both lockfiles confirmed that all 271 package names and versions were identical;
only the source location of the same Tantivy revision changed. The temporary
search-engine crate was again built as `rlib` only. Neither workaround is part of
the shared-core manifest or lockfile.

The eight facade tests passed again, including the independently produced schema-10
index. The additional test in `qa/writer_lifecycle_compat.rs`, copied into the
temporary combination's `core/tests`, also passed. It verifies that closing a
read-only handle cannot enable mutation, closing a writer permanently rejects
reopen/commit/queued writes, another writer can acquire the released lock, and the
read-only snapshot still refreshes correctly. This test is kept outside the normal
Cargo test targets so the shared core has no mandatory dependency on unmerged API.

All five `rust/tests/index_writer_shutdown.rs` tests from PR #77 passed against the
Tantivy artifact built in that combination: successful, failed and panicking
background merges, plus removal and renaming of a replaced index after metadata
I/O. To avoid building unrelated CLI binaries under the storage constraint, that
unchanged integration-test source was compiled directly with:

```sh
rustc --edition=2021 --test rust/tests/index_writer_shutdown.rs \
  -C debuginfo=0 --extern tantivy=PATH_TO_VERIFIED_B6DFC7B_RLIB \
  --extern tempfile=PATH_TO_TEMPFILE_RLIB -L dependency=TARGET/debug/deps \
  -o TEMP/pr77_shutdown_tests
TEMP/pr77_shutdown_tests --test-threads=1
```

The artifact's dependency file identified the verified checkout above. The five
tests completed in 10.52 seconds. The source combination was temporary and was not
committed as a merge.

When combining these changes in the future, retain the two independent guards and
advance the MCP workspace's Tantivy patch together with its engine pin. The shared
core is usable against the current baseline without waiting for PR #77.

## Remaining CI gates

The generated Rust bridge compiled in both default and host-SQLite configurations.
Full FRB regeneration was not run locally because `cargo-expand` was unavailable.
The existing generated-binding CI job remains the regeneration/ABI gate. Flutter
tests, Windows execution and production inference-feature builds are also CI
gates; they are not claimed as local passes. No new transport or inference feature
was added to Flutter's feature selection.
