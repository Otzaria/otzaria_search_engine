> **Current branch:** the proposal is now rebased on `8ef29fd592793fbc9d5af0299fdd7a8607789bac`,
> including the merged PR #82 and its additional parallel-scan correction.
> See the [current rebase record](validation/2026-11-07-rebase/README.md) for its
> exact source comparison and fresh checks. The original pre-rebase record below
> is preserved verbatim; its results apply to the earlier source identified there.

# Shared-core validation — 2026-11-07

This record applies to the resumed proposal on canonical engine
`6fa52571ddb307a4a28baf0768d0831106edadb3`, including the already merged writer
lifecycle fix. The current implementation snapshot is
`3173eeededad79aeced96453aee9cae71cec2741`. The four-file concurrency fix from
PR #82 (`11083b6e8a934cf1daa4833f112e782d3911c1ab`) is included as a separate
commit; this proposal does not depend on that PR being merged.

## Recovery and historical evidence

The earlier proposal was recovered before changing any of its content. Its
retained Git tree and source patch reproduced these objects exactly:

| Object | Git object ID |
| --- | --- |
| Retained staged tree | `31f5f34de019b8002a408de2634e336d3b7c2001` |
| Recovered search source blob | `48b02e55065d0d4c9aec0519d62a121c9e5e723a` |
| Complete earlier proposal tree | `fb79f2caba72895dc7897f1201ad4269fb6bfd42` |

That complete tree is preserved on
[`recovery/shared-core-20261105`](https://github.com/Otzaria/otzaria_search_engine/tree/recovery/shared-core-20261105).
The original [2026-11-05 validation report](validation/2026-11-05.md) is retained
verbatim as historical evidence. Its old local commit IDs, eight facade tests,
845 unit tests and temporary PR #77 combination are not the validation of the
newer baseline. The fresh checks below replace those counts for this proposal.

The replay preserves all intervening upstream work: schema-11 note provenance,
readability of schema 10, bounded indexing, the current Tantivy/sidecar pins and
the permanent `writer_closed` guard. Only the intended shared-core change was
replayed; the old large source file did not replace current upstream.

## Fresh Linux checks

Environment: Linux x86_64, Rust/Cargo 1.99.1, nine available logical CPUs,
`CARGO_BUILD_JOBS=3`, `CARGO_INCREMENTAL=0`, and zero debug information for dev/test
profiles. The manifests retain their ordinary `rlib`, `cdylib` and `staticlib`
crate types. Dependencies were resolved from the committed lockfile; no local
path substitution for the Tantivy or semantic-sidecar revision was used.

Commands run from the repository root:

| Check | Command | Result |
| --- | --- | --- |
| Headless host API | `cargo test --manifest-path rust/core/Cargo.toml --locked` | 10 passed, 0 failed; parallel test harness |
| Sidecar-enabled read-only guards | Same command with `--features semantic-integration` | 10 passed, 0 failed |
| Full headless engine unit suite | `cargo test --manifest-path rust/Cargo.toml --locked --no-default-features --features sqlite-bundled --lib` | 882 passed, 0 failed, 27 pre-existing ignored; 69.89 s |
| Writer shutdown | Same headless manifest/features with `--test index_writer_shutdown` | 5 passed, 0 failed; 2.16 s |
| Default Flutter bridge compiles | `cargo check --manifest-path rust/Cargo.toml --locked --lib` | Passed |
| Host SQLite + Flutter bridge compiles | Same check with `--no-default-features --features sqlite-host,flutter-bridge` | Passed |
| Facade Clippy | `cargo clippy --manifest-path rust/core/Cargo.toml --locked --all-targets --no-deps -- -D warnings` | Passed |
| Dependency boundary | `cargo tree --manifest-path rust/core/Cargo.toml --locked -e normal --prefix none --format '{p}'` | No FRB runtime, Tokio, rmcp, allo-isolate, dart-sys, ort or ort-sys |
| Formatting and patch hygiene | `cargo fmt --manifest-path rust/Cargo.toml --all -- --check` and `git diff --check` | Passed |

The small FRB annotation proc-macro remains a build-time dependency. It is not the
FRB runtime. The existing nonsemantic unit-test build reports an unused
`DisplayText::plain` helper from upstream; this is not a test failure and no
unit test was disabled to suppress it. The facade's scoped Clippy gate is clean.

Execution logs and their SHA-256 hashes are retained in
[`validation/2026-11-07`](validation/2026-11-07). They are test/build evidence,
not a full-library benchmark or a cross-platform performance claim.

## Contracts exercised

- An independently produced, byte-preserved **schema-10** index, generated with
  unmodified engine `225a6d5`, matches its 16-case golden exactly: IDs, all result
  fields, relevance/catalogue/generation order, counts, grouping, pagination,
  facets, vocalized and negative queries, snippets and highlights. Every index
  file remains unchanged after close.
- New **schema-11** indexes preserve note markers and note-body highlights when
  read through the facade. Existing legacy/future metadata is rejected without
  mutation, including a schema-11 index falsely labeled as schema 10.
- A read-only handle opens while the canonical writer is still alive. All 21
  tested lexical mutation entry points fail before parsing or acquiring a
  writer, including empty batches and invalid PDF byte input.
- Sidecar configuration/mutation paths remain guarded. Artifact open is denied
  because it may perform recovery, and vector verification is denied because it
  may persist damage markers. The suite runs both without and with the
  backendless sidecar integration compiled.
- A reader refresh observes a new commit and invalidates query/count caches;
  closing, replacing an index directory and reopening returns the replacement.
- Four threads issue 64 searches against the same read-only handle. External
  order changes preserve stable IDs. The carried PR #82 regression deterministically
  holds a cold term-work gate while a real Rayon search must complete, then
  compares its results with the primer's highlight matcher.
- Forced same-path line-source refresh invalidates active independent scans even
  when no cached row-window connection exists, waits for them to release, and
  rejects an empty path without disturbing the current source.
- `read_only` and `writer_closed` remain independent after repeated close calls.
  The five canonical shutdown tests cover successful, failing and panicking
  background merges plus index removal/replacement and rename/replacement.

The first replay run exposed an obsolete test that treated schema 11 as a future
schema. It was updated to the actual policy (10 and 11 readable; 12 future), with
an additional stored-schema mismatch assertion. Headless unit compilation also
exposed a test-only callback helper needing the same feature/test gate as its
caller; that boundary was corrected, retaining the relevant tests.

## CI gates and scope

The existing generated-bindings and Flutter jobs remain required. Full FRB
regeneration, Flutter execution, production inference features, Windows, macOS
and mobile targets are not claimed as local passes. CI adds a separate headless
matrix on Linux and Windows, with default and sidecar-enabled facade contracts,
dependency inspection, Clippy, explicit refresh and deterministic concurrency
regressions, and the host-SQLite bridge compile check.

Cargo invocations for the headless facade are intentionally separate from a
workspace-wide Flutter build: Cargo feature unification would otherwise enable
`flutter-bridge` on the same dependency and invalidate the dependency check.
No transport, admission policy or new inference feature was added to Flutter.
No numerical reduction in the Flutter binary size is asserted.
