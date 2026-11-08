# Shared-core economy-mode performance investigation

Status: the focused diagnostic reproduced the binary-associated tail-latency
finding in both execution orders. Its source cause remains unresolved.
Production [engine #84](https://github.com/Otzaria/otzaria_search_engine/pull/84)
and [MCP #5](https://github.com/Otzaria/otzaria_mcp/pull/5) remain drafts. No merge
has been performed. This evidence branch preserves observations without changing
either production proposal.

## Immutable revisions

| Role | Revision |
| --- | --- |
| Current canonical base | `8ef29fd592793fbc9d5af0299fdd7a8607789bac` |
| Reviewed shared-core proposal | `4761356749815d283ede41ef956c64b3dbffe048` |
| MCP migration | `3c4161074e896e461db4e07537bc11fcfafc3b9e` |
| Dedicated diagnostic controller | `fc9cc131e070882f6531238a57d7f911249e6752` |

Both engine revisions include the merged writer-lifecycle and WorkGate/ScanMode
changes. The application was checked again at `bfd64c2dabf576c1763b177a87097f74e7469b73`:
both manifests still pin schema-10 engine `b78d36eaffcb2a5fa679d34a52133512ee55c084`.
The proposal reads schemas 10 and 11; no application dependency bump is assumed.

## Original finding

Two executions of the existing responsiveness job compared the same immutable
source revisions and the same executable bytes. Both used the order B,H,H,B for
the economy block, after eight normal-mode processes.

| Attempt / pair | Base p95, ms | Head p95, ms | Change | Query samples B / H |
| --- | ---: | ---: | ---: | ---: |
| 1 / 1 | 3.284019 | 4.338909 | +32.12% | 296 / 290 |
| 1 / 2 | 3.215010 | 3.902604 | +21.39% | 304 / 284 |
| 2 / 1 | 3.173940 | 4.395877 | +38.50% | 295 / 283 |
| 2 / 2 | 3.093030 | 4.316188 | +39.55% | 295 / 280 |

Median per-process p95 increased **3.249515 to 4.120757 ms (+26.81%)** in
attempt 1 and **3.133485 to 4.356033 ms (+39.02%)** in attempt 2. Economy
indexing throughput increased 2.72% and 2.59%; buffer, spill and reservation
peaks were identical. Idle warm-query p95 median increased only 1.05% and
5.59%, respectively, but each idle percentile comes from 30 queries.

The fixture repeatedly searches an unchanged, committed 12,000-document seed
while another thread indexes eight books / 96,000 documents on two allowed
CPUs. The final commit is included in indexing throughput but follows concurrent
query sampling. Timed search includes result-ID collection and assertions; the
10 ms sleep between calls is outside the timed interval. Each process supplies
one percentile observation. Hundreds of queries are not hundreds of independent
process repetitions, and no per-query timing vectors were retained originally.

Original workflow and jobs:

- [Workflow 42413434336](https://github.com/Otzaria/otzaria_search_engine/actions/runs/42413434336).
- [Attempt 1 job 128526250277](https://github.com/Otzaria/otzaria_search_engine/actions/runs/42413434336/job/128526250277).
- [Confirmation job 128534442998](https://github.com/Otzaria/otzaria_search_engine/actions/runs/42413434336/job/128534442998).

The original artifact JSON files and controller are copied byte-for-byte under
`original/`. Their index records artifact IDs, hashes and commands. The archived
environment note saying diagnostic counters exist only on the head is inherited
and stale for this comparison: both actual revisions emit them. It is preserved
as historical evidence and is not repeated as a current conclusion.

The original B executable SHA256 is
`a58b7ec047d3b39d0f4417571d652920d14ea7f8370030aa6bddd349dc506e5a`;
H is `974a92cc60324467d26ebad61c7d21ca0e7512087201eaa037ff357d42151337`.
The originals retained hashes but did not retain executable bytes or physical
CPU topology. Source identity and executable identity remain distinct.

## Focused diagnostic

The [dedicated run 42420237162](https://github.com/Otzaria/otzaria_search_engine/actions/runs/42420237162)
uses a separately reviewed controller and workflow. The exact unchanged fixture
is checked by SHA256. Each immutable revision is built once with Rust 1.99.1.
The measurement schedule contains 16 real B/H processes in four balanced blocks
(BHHB, HBBH, HBBH, BHHB), interleaved with 16 same-binary controls. Each actual
revision occupies every real block position equally. Control labels do not
change the executable used within a block.

The controller preserves binaries, source and executable hashes, exact schedule,
CPU topology, whole-process CPU usage, before/after pressure and Dirty/Writeback,
raw fixture outputs and descriptive contrasts. Whole-process CPU includes the
indexer, setup, commit and cleanup; it cannot establish query-only CPU cost.
There is no threshold adjustment or automated causal verdict. A separate layout
example records `SearchEngine` size/alignment without changing library source,
the benchmark fixture, manifests, lockfile or resolved dependency features.

This is an economy-only experiment. Its preceding workload differs from the
original normal-then-economy schedule. The fresh builds reproduced both original
executable hashes exactly, despite separate build roots and the layout helper.
This removes executable-identity uncertainty for this actual comparison.

The measured affinity `[0, 1]` consists of **two logical CPUs sharing one physical
core (SMT siblings)**: both report package 0 / core 0 / siblings `0-1`. It must not
be described as two independent physical cores. The original attempts did not
record topology, so their physical-core arrangement cannot be established from
this run. Both binaries were measured under the same declared affinity here.

### Controlled results

All 32 processes completed successfully. Independent QA verified the artifact
digest, all 94 file hashes in its manifest, exact schedule/resolved/raw-stdout
agreement, identical document stamps and the original B/H executable identity.
The [lossless raw text archive](controlled/text-evidence.tar.xz) retains all 93
non-binary artifact files, including per-process context, stdout, compiler records
and dependency graphs. Its SHA256 is
`60ed648050fd32f0d4b0326f908ca2309753f527a2416e3210cbaedbc0df628d`.
The primary analysis and environment records are also directly readable under
`controlled/`; `archive-provenance.json` identifies every original file hash,
the Actions artifact and the executable retention scope.

| Real block | Order | Base p95 median, ms | Head p95 median, ms | Change |
| --- | --- | ---: | ---: | ---: |
| R1 | BHHB | 3.392626 | 3.979151 | +17.29% |
| R2 | HBBH | 3.683152 | 4.469672 | +21.35% |
| R3 | HBBH | 3.431622 | 4.099225 | +19.45% |
| R4 | BHHB | 3.058303 | 4.422851 | +44.62% |

Median of the eight real process observations per binary:

| Metric | Base | Head |
| --- | ---: | ---: |
| Concurrent-search p95 | 3.246257 ms | 4.392358 ms |
| Concurrent-search p50 | 0.820677 ms | 0.856379 ms |
| Concurrent-search p99 | 6.871712 ms | 7.401890 ms |
| Idle warm-query p95 | 0.463231 ms | 0.473490 ms |
| Query observations per process | 296.5 | 284.5 |
| Indexing documents / second | 25,437.33 | 26,130.73 |
| Whole-process CPU | 7.453647 s | 7.391175 s |
| Whole-process wall time | 4.674654 s | 4.574505 s |
| Peak RSS | 90,872 KiB | 90,984 KiB |

The p95 change is **+35.30%** in this observed SMT workload. All eight adjacent
real comparisons are positive (+1.70% to +54.13%), and all four block contrasts
are positive in both orders. Individual unpaired B/H ranges overlap.

The same-binary controls are not flat. Their pseudo-H/B p95 contrasts are
**+4.66%, +10.27%, +18.93% and +5.44%** for CB1, CH1, CB2 and CH2; adjacent
control-pair changes range from -4.17% to +36.69%. These show meaningful run-to-run
variation. They do not justify treating every real-process difference as a
precise effect estimate or dismissing the consistent real block contrasts.

All processes have identical resource peaks: 2,162,688 buffered bytes,
205,871,288 spilled bytes, 3,145,728 reserved bytes and one active preparation.
Both `SearchEngine` layouts measure 1,600 bytes with alignment 8. This rules out
object-size growth, but does not establish equal private-field offsets or code
generation. Memory-pressure deltas are zero. I/O pressure is small in aggregate;
before/after snapshots cannot rule out brief query-time stalls. Global CPU
PSI-some medians increase from approximately 1.785 to 1.977 seconds, while
whole-process CPU slightly decreases. These totals include all indexer work.

The +2.73% indexing-throughput observation is coupled to this closed query loop:
the head performs fewer query probes before indexing finishes. It is not an
independent compensation for worse search latency. Simple fixed-middle-position
bias is insufficient to explain this result; the responsible source change is
still unidentified. Both production PRs remain drafts.

The next focused discriminator will reuse these exact binaries, counterbalancing
the SMT sibling pair with two logical CPUs on distinct physical cores. This tests
topology dependence without rebuilding the engine; it does not excuse the
existing observation or establish a source cause on its own.

## Source review and scope

The ordinary runtime dependency graph has 167 packages on both sides with equal
dependency feature sets. The directly imported FRB annotation is the same macro
the former runtime import re-exported. Synchronous query code, the WorkGate and
ScanMode correction, indexing budgets, spill policy and benchmark fixture are
unchanged. The new write guard executes once for the eight-book batch, not once
per document or timed query. The added refresh API is not invoked by the fixture.

`SearchEngine` gains one private immutable `read_only` boolean; no new mutex,
atomic or buffer field is added. Private-field layout and code-generation effects
remain possible despite the equal measured object size. Even a warm cached exact query builds its ten result
snippets through Rayon, so caller-thread CPU would omit worker execution. No
write guard or existing mutex has been removed to influence measurements.

The separate MCP tiny warm serialized dev-build benchmark is not evidence about
this concurrent-indexing workload. All production correctness and platform CI
gates passed; that does not resolve this scoped timing observation.

## Independent review and preservation

Independent QA verified all four original pairs and their executable identity,
the diagnostic schedule/control calculation, the exact published two-file
diagnostic tree, and the complete 32-process result/provenance archive. It
independently recomputed the contrasts and recommended retaining draft status
because the scoped binary-associated finding remains unresolved.

The exact original shared-core proposal remains on
[`recovery/shared-core-20261105`](https://github.com/Otzaria/otzaria_search_engine/tree/recovery/shared-core-20261105),
and the fully validated pre-rebase proposal on
[`recovery/shared-core-20261107-b22aab7`](https://github.com/Otzaria/otzaria_search_engine/tree/recovery/shared-core-20261107-b22aab7).
Earlier measurements are neither discarded nor relabelled as current results.
