# Independent QA: controlled economy diagnostic, phase 1

**Verdict: keep engine PR84 and MCP PR5 in draft.** The controlled run reproduces
a binary-associated increase in concurrent-search tail latency under the observed
SMT constraint. It does not identify the responsible source change.

[Diagnostic run 42420237162](https://github.com/Otzaria/otzaria_search_engine/actions/runs/42420237162) used immutable base
`8ef29fd592793fbc9d5af0299fdd7a8607789bac` and head `4761356749815d283ede41ef956c64b3dbffe048`. This audit performed no
new benchmark, build or production edit.

## Integrity and scope

All **94 manifest hashes**, all **32 schedule entries**, and all **32 raw stdout
records** passed independent verification. There were 16 real B/H processes and
16 same-binary control processes, no process failures, and identical document fingerprints
across these processes and all 24 original artifact rows. The downloaded ZIP is
`6f31738c00b39e519e84b5aa9ef076509423a4c64965ee581349f54734b8bd48`.

Both exact benchmark binaries match the hashes from **both original CI attempts**:

- B: `a58b7ec047d3b39d0f4417571d652920d14ea7f8370030aa6bddd349dc506e5a`.
- H: `974a92cc60324467d26ebad61c7d21ca0e7512087201eaa037ff357d42151337`.

The fixture and reviewed controller bytes match. The layout helper changed
neither resolved features/dependencies nor benchmark executable bytes.
Both `SearchEngine` objects are **1600 bytes, alignment 8**; this does not establish
identical field offsets or generated code.

Affinity `[0,1]` selects **two logical CPUs sharing one physical core**, with
`core_id=0`, `physical_package_id=0`, `thread_siblings_list=0-1`, on the recorded
AMD EPYC 7763 runner. These are not two physical cores. Original physical topology
was not recorded, so this detail cannot be back-projected onto the earlier jobs.

## Real comparisons

Each table cell is the median of two process-level p95 values within that block.

| Block | Order | B p95 ms | H p95 ms | H/B change |
| --- | --- | ---: | ---: | ---: |
| R1 | BHHB | 3.392626 | 3.979151 | +17.29% |
| R2 | HBBH | 3.683152 | 4.469672 | +21.35% |
| R3 | HBBH | 3.431622 | 4.099225 | +19.45% |
| R4 | BHHB | 3.058303 | 4.422851 | +44.62% |

All eight adjacent real pairs have higher head p95, ranging from **+1.70% to
+54.13%**. Across the eight real processes per binary, median per-process p95 is
**3.246257 to 4.392358 ms
(+35.31%)**. The effect persists in both run orders;
a simple fixed-middle-position explanation is insufficient.

Median p50 is 0.820677 to 0.856379 ms
(+4.35%), and idle warm p95 is
0.463230 to 0.473490 ms
(+2.21%). The 30-probe idle percentile is imprecise.

## Same-binary controls

Pseudo-labels below do not change the executable. These controls are not flat.

| Block | Actual binary | Pseudo-label order | Pseudo H/B p95 change | Middle/edge change |
| --- | --- | --- | ---: | ---: |
| CB1 | B | BHHB | +4.66% | +4.66% |
| CH1 | H | HBBH | +10.27% | -9.31% |
| CB2 | B | HBBH | +18.93% | -15.92% |
| CH2 | H | BHHB | +5.44% | +5.44% |

Adjacent control contrasts range from **−4.17% to +36.69%**. Current unpaired
B/H ranges overlap. This variation limits the precision and attribution of the
headline effect; the controls must remain visible alongside the real comparisons.

## Resource and interpretation limits

All 32 processes have identical peaks: **2,162,688 buffered bytes**, **205,871,288
spilled bytes**, **3,145,728 reserved bytes**, and **one active preparation**.
Memory-pressure deltas are zero. Aggregate CPU/I/O pressure and whole-process CPU
cannot establish query-only cost or exclude brief query-specific stalls.

Indexing-throughput medians are 25437.33 to
26130.73 documents/s (+2.73%), while
median query counts are 296.5 to 284.5.
The fixed 10 ms sleep makes this a **closed query loop**: latency, query count and
indexer interference are coupled. Throughput improvement is not independent
compensation for the tail-latency finding.

This is a warm repeated exact query over a committed 12,000-document seed while
96,000 documents are indexed; the final commit follows query sampling. It is an
economy-only run, unlike the original eight-normal-process prefix. Per-process
percentiles are observations, not hundreds of independent query replicates.
No source cause or fix has been established, and no threshold has been relaxed.
