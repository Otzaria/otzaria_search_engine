# Validation after the merged concurrency follow-up

The shared-core proposal was rebased onto canonical engine
`8ef29fd592793fbc9d5af0299fdd7a8607789bac` after PR #82 merged with an additional
parallel-scan correction. The six shared-core commits were replayed in an
isolated worktree; the duplicate of PR #82's initial commit was omitted because
it is now upstream. No conflict was resolved by replacing the new implementation
with the earlier source file.

## Source preservation

The tested source commit is `60913c9be8960bae514225aea01c741c24161e3f`, tree
`c6106d6fb188a25b9ebf9664a7635fe39b6e5b5e`. The subsequent commit only updates
documentation and records these checks. [source-proof.json](source-proof.json)
records the exact replay commits and comparison.

The complete source delta from the previously tested local head `4b6bd560`
(published as `b22aab7`) to the rebased source is precisely upstream follow-up
`9f6719073aa24834cfe53005e1190d8689e1b0d1`. Both have stable patch ID
`c3e6e3fce2d77710fafde6c17ea6ca39b105482e`. Independent QA also compared every
remaining byte after removing only Git index headers and hunk line numbers:
both normalized diffs are 11,766 bytes with SHA-256
`d982421e8e0f8172b67b50b9ded1b684e385ab164f639d8123372476844986f8`.

Only these five source files differ from the earlier proposal:

- `rust/src/api/search_concurrency_tests.rs`
- `rust/src/api/search_engine.rs`
- `rust/src/highlight_cache.rs`
- `rust/src/quality_bench.rs`
- `rust/src/term_automaton.rs`

The four files outside `api/search_engine.rs` match the merged upstream files
byte-for-byte. The API file retains the shared-core read-only and feature guards
alongside the upstream scan-mode change. All other production source, manifests,
lockfile, bridge code and fixture bytes remain unchanged. This source review is
not presented as an independent rerun of the tests below.

## Fresh focused checks

The checks used a fresh isolated Cargo target, Linux x86_64, Rust/Cargo 1.99.1,
three build jobs, no incremental compilation and no dev/test debug information.
Commands, timings, raw-output hashes and compressed-log hashes are recorded in
[manifest.json](manifest.json). These are correctness checks, not timed
performance comparisons.

| Check | Result |
| --- | --- |
| Headless facade contracts | 10 passed, 0 failed |
| Facade with `semantic-integration` | 10 passed, 0 failed |
| `term_automaton::tests` | 10 passed, 0 failed |
| `highlight_cache::tests` | 13 passed, 0 failed |
| Cold search while a primer owns the work gate | 1 passed, 0 failed; exercises one, two and four Rayon workers |
| Facade Clippy, all targets, `-D warnings` | Passed |
| Rust formatting and patch whitespace | Passed |

The scan contracts cover actual segment-scan overlap, inline contenders, capped
result parity, quote-transparent matching and branch priority. Gate tests cover
owner retention, cancellation, shared cache values and concurrent requests. The
facade contracts cover existing schema-10 bytes, schema-11 notes/highlights,
permanent read-only guards, live writer coexistence, refresh, stable IDs and
concurrent searches. The two facade configurations overlap and are not 20 unique
test cases.

The initial attempt reused a build target from another worktree and failed before
tests because a Tantivy rlib artifact was missing. Its exact log and attempt
metadata remain in `existing-target-build-failure.log.gz` and
`existing-target-attempt.json`. The fresh isolated build above passed without
changing source or dependency versions.

## Current versus historical evidence

The earlier head `b22aab7` passed all 20 jobs across its Rust/semantic/FRB,
five-platform native application and formatting workflows. Those results remain
linked to that exact head; the rebased head must pass its own CI before review
completion. No local Flutter invocation was used for this rebase.

The earlier paired MCP measurements were made before `9f671907`. Because that
commit changes scan scheduling, those measurements are historical evidence for
their recorded pin and are not relabeled as measurements of this rebased source.
Any new MCP measurements and its final immutable pin are documented with
[MCP migration #5](https://github.com/Otzaria/otzaria_mcp/pull/5).

The original recovered proposal and the fully validated pre-rebase head remain
available on separate remote branches:

- [Original proposal](https://github.com/Otzaria/otzaria_search_engine/tree/recovery/shared-core-20261105)
- [Validated pre-rebase proposal](https://github.com/Otzaria/otzaria_search_engine/tree/recovery/shared-core-20261107-b22aab7)

The original pre-rebase text in `rust/core/VALIDATION.md` remains verbatim after
its new current-record notice. Its SHA-256 is recorded in `source-proof.json`.
