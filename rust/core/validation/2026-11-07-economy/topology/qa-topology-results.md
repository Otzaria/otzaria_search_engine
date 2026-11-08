# Independent QA: exact-binary topology results

**The artifact passes independent QA. The SMT latency regression is reproduced; both production PRs remain drafts.**

Run [42431565386](https://github.com/Otzaria/otzaria_search_engine/actions/runs/42431565386), job 128581413159, completed all 64 scheduled processes with zero Rust builds. The exact original B/H binaries were reused. The controller is commit `1c1630d0b0034ddcfaf724fca92643764f44a9a7`.

## Main result

Each number below is the median of eight real process-level p95 observations. Controls remain separate.

| Guest-visible topology | Affinity | B p95, ms | H p95, ms | H/B change | Positive real blocks | Positive adjacent real pairs |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| SMT siblings | [0, 1] | 3.4050735 | 4.2567320 | +25.01% | 4/4 | 8/8 |
| Distinct cores | [0, 2] | 4.7944705 | 4.5444895 | -5.21% | 2/4 | 2/8 |

The overall distinct/sibling H/B ratio of ratios is **0.758219**. All four matched real-block interactions are below one (0.682780, 0.779244, 0.948437 and 0.976098). The observed H/B pattern changes across the two affinities in this run. The distinct-core blocks are mixed, and the source-level explanation remains unresolved.

## Real blocks and same-binary controls

Entries are pseudo-H/B p95 changes. In control blocks both pseudo-labels execute the same B or H binary.

| Block | Kind / actual binary | Order | SMT change | Distinct-core change |
| --- | --- | --- | ---: | ---: |
| R1 | Real B/H | BHHB | +21.10% | -17.31% |
| CB1 | Control B | BHHB | +4.24% | -5.47% |
| R2 | Real B/H | HBBH | +21.45% | -5.36% |
| CH1 | Control H | HBBH | +18.24% | +15.42% |
| R3 | Real B/H | HBBH | +19.95% | +13.76% |
| CB2 | Control B | HBBH | +19.40% | +10.92% |
| R4 | Real B/H | BHHB | +4.40% | +1.91% |
| CH2 | Control H | BHHB | -8.07% | -9.15% |

The controls are not flat. Adjacent control changes span **−23.83% to +32.01%** on SMT and **−11.04% to +27.49%** on distinct cores. They show meaningful process variation. They do not justify an ad hoc subtraction from the real contrasts or discarding the repeated positive SMT finding.

## Both real orders

Each order summary has four observations per binary.

| Topology | Order | B p95, ms | H p95, ms | Change |
| --- | --- | ---: | ---: | ---: |
| SMT | BHHB | 3.3136530 | 3.9130285 | +18.09% |
| SMT | HBBH | 3.4837955 | 4.2567320 | +22.19% |
| Distinct | BHHB | 4.7944705 | 4.2963725 | -10.39% |
| Distinct | HBBH | 4.8431190 | 4.5858990 | -5.31% |

## Workload and resource context

| Metric | SMT B | SMT H | Distinct B | Distinct H |
| --- | ---: | ---: | ---: | ---: |
| Concurrent p50, ms | 0.800380 | 0.836081 | 0.532594 | 0.555507 |
| Concurrent p99, ms | 7.650527 | 7.344338 | 7.316693 | 7.668117 |
| Idle p95, ms | 0.457925 | 0.470905 | 0.345986 | 0.347760 |
| Query observations per process | 294.0 | 283.0 | 188.0 | 186.0 |
| Indexing documents/second | 25,610.75 | 26,298.03 | 38,731.80 | 39,374.63 |
| Peak RSS, KiB | 92,348 | 89,086 | 88,902 | 89,434 |

Indexing throughput H/B rises 2.68% on SMT and 1.66% on distinct cores. Those observations remain coupled to the closed query loop: H performs fewer probes before indexing finishes. Distinct-core absolute p95 is not uniformly lower than SMT p95, even though indexing is substantially faster. These results do not support a general latency-improvement claim from changing affinity.

Every process has the same peaks: 2,162,688 buffered bytes, 205,871,288 spilled bytes, 3,145,728 reserved bytes and one active preparation. Whole-process CPU includes setup, indexing, search, commit and cleanup; pressure snapshots cannot identify query-only CPU cost or exclude short stalls.

## Verification

- Independently fetched the API artifact identity/digest and successful job status.
- Verified all 148 manifest entries and every extracted byte.
- Matched all 64 predeclared schedule entries to records, individual context JSON and raw stdout.
- Verified exact original B/H executable hashes, approved controller bytes, all retained original input bytes and original document stamps.
- Checked the reciprocal SMT pair [0,1], same-package distinct pair [0,2], two logical CPUs and unchanged fixture environment.
- Recomputed all published real medians, block contrasts and descriptive interactions independently.

Artifact: `economy-topology-42431565386-1`, ID `13597940923`, 11,939,632 bytes. ZIP SHA-256: `45f82b3cba4c894ddcc4f2587eedbb409d2736ee1b28d6c0748313e265a56d48`. Manifest SHA-256: `5c1d187f8602819c9e1ea757adaf57fef79006d2944c13b45eb4dc4d44b8fe5d`.

## Disposition

Retain the prior phase-1 finding and draft state of engine PR #84 and MCP PR #5. This one-run topology interaction narrows the observed conditions; it does not establish a source cause or excuse the repeated SMT regression. No threshold change, guard removal or promotion follows. Candidate B/H/C evidence and behavioral gates remain separate.

QA executed no builds, fixtures, Flutter commands, production edits or remote writes.
