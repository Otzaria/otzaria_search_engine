# Exact-binary topology experiment

**The SMT tail-latency finding is reproduced. Changing the CPU topology changes the observed comparison, but the source cause remains unresolved.** This is an additive record of the completed experiment; the original attempts and the earlier controlled experiment remain preserved. At this experiment's completion, independent QA recommended retaining engine [PR #84](https://github.com/Otzaria/otzaria_search_engine/pull/84) and MCP [PR #5](https://github.com/Otzaria/otzaria_mcp/pull/5) as drafts. No production source or merge was changed by this diagnostic.

## Immutable inputs and method

[Run 42431565386](https://github.com/Otzaria/otzaria_search_engine/actions/runs/42431565386), job `128581413159`, completed on 2026-11-08. The controller is commit [`1c1630d0b0034ddcfaf724fca92643764f44a9a7`](https://github.com/Otzaria/otzaria_search_engine/commit/1c1630d0b0034ddcfaf724fca92643764f44a9a7). It executed **64 fresh processes and zero Rust builds**, reusing the exact executables from the [earlier controlled experiment](../README.md#focused-diagnostic).

| Label | Engine source revision | Executable SHA-256 |
| --- | --- | --- |
| B | `8ef29fd592793fbc9d5af0299fdd7a8607789bac` | `a58b7ec047d3b39d0f4417571d652920d14ea7f8370030aa6bddd349dc506e5a` |
| H | `4761356749815d283ede41ef956c64b3dbffe048` | `974a92cc60324467d26ebad61c7d21ca0e7512087201eaa037ff357d42151337` |

Both source revisions contain the merged writer-lifecycle and WorkGate/ScanMode changes. Both executables match the original responsiveness jobs as well as the earlier controlled run. The fixture SHA-256 is `58b15ac4a0736284bae336b48f6d334bc1de8bcfd3ac52689ab0417ec264ebca`. It repeatedly searches an unchanged committed seed of 12,000 documents while another thread adds eight books / 96,000 documents. Search timing excludes the 10 ms interval between calls; the final indexing commit contributes to throughput after the concurrent search sample window ends. Original document stamps and runtime settings were verified unchanged.

The runner exposed AMD EPYC 7763 CPUs and four logical CPUs. The two affinity masks share anchor CPU 0 and package 0:

| Case | Affinity | Guest-visible mapping |
| --- | --- | --- |
| SMT siblings | `[0, 1]` | Both logical CPUs belong to physical core 0; reciprocal sibling list `0-1` |
| Distinct physical cores | `[0, 2]` | Logical CPUs belong to physical cores 0 and 1 in the same package |

These are guest-visible topology records, not a claim about unexposed host allocation. Each mask retains a budget of two logical CPUs. Each topology has 16 real B/H processes and 16 same-binary controls. Real orders are balanced across BHHB and HBBH, and topology precedence is counterbalanced across the eight blocks. No extra warmup, cache dropping, cooldown, threshold change or executable rebuild was introduced. The complete predeclared schedule and individual contexts are in the raw archive.

## Main result

Each cell is the median of **eight real process-level percentile observations per executable per topology**. Control observations are kept separate; individual queries are not pooled as independent replications.

| Topology | B p95, ms | H p95, ms | H/B change | Positive real blocks | Positive adjacent real pairs |
| --- | ---: | ---: | ---: | ---: | ---: |
| SMT siblings | 3.4050735 | 4.2567320 | +25.011% | 4/4 | 8/8 |
| Distinct cores | 4.7944705 | 4.5444895 | -5.214% | 2/4 | 2/8 |

The distinct/sibling H/B ratio of ratios is **0.758219**. Every matched real block has a smaller H/B ratio on distinct cores, but the distinct-core block results are mixed. This supports a descriptive topology interaction in this run. It does not identify the source-level mechanism, quantify a universal effect, or erase the positive SMT observation. The prior controlled SMT result (B 3.246257 ms; H 4.3923575 ms, +35.30%) and all four original positive pairs remain unchanged in their original records.

### Real blocks and same-binary controls

The control columns use pseudo-H/B labels while executing the same binary within each control block.

| Block | Actual comparison | Order | SMT p95 change | Distinct-core p95 change |
| --- | --- | --- | ---: | ---: |
| R1 | Real B/H | BHHB | +21.10% | -17.31% |
| CB1 | Control B | BHHB | +4.24% | -5.47% |
| R2 | Real B/H | HBBH | +21.45% | -5.36% |
| CH1 | Control H | HBBH | +18.24% | +15.42% |
| R3 | Real B/H | HBBH | +19.95% | +13.76% |
| CB2 | Control B | HBBH | +19.40% | +10.92% |
| R4 | Real B/H | BHHB | +4.40% | +1.91% |
| CH2 | Control H | BHHB | -8.07% | -9.15% |

The controls are not flat. Adjacent control comparisons span **−23.83% to +32.01%** on SMT and **−11.04% to +27.49%** on distinct cores. Meaningful process variation remains visible. There is no ad hoc control subtraction or dismissal of the repeated positive real SMT contrasts. Both real execution orders independently show positive SMT median changes, +18.09% for BHHB and +22.19% for HBBH.

## Latency, throughput and memory context

Values are medians of the same eight real processes per binary and topology.

| Metric | SMT B | SMT H | Distinct B | Distinct H |
| --- | ---: | ---: | ---: | ---: |
| Concurrent p50, ms | 0.800380 | 0.836081 | 0.532594 | 0.555507 |
| Concurrent p99, ms | 7.650527 | 7.344338 | 7.316693 | 7.668117 |
| Idle p95, ms | 0.457925 | 0.470905 | 0.345986 | 0.347760 |
| Query observations per process | 294.0 | 283.0 | 188.0 | 186.0 |
| Indexing documents/second | 25,610.75 | 26,298.03 | 38,731.80 | 39,374.63 |
| Peak RSS, KiB | 92,348 | 89,086 | 88,902 | 89,434 |

H indexing throughput is 2.68% higher on SMT and 1.66% higher on distinct cores. This remains coupled to the closed query loop: H performs fewer query probes before indexing finishes. Throughput is therefore not an independent compensation for worse search latency. Distinct-core absolute p95 is not uniformly lower than SMT p95 despite substantially faster indexing; this is not evidence for a general latency benefit from affinity changes.

All 64 processes have identical resource peaks: **2,162,688 buffered bytes**, **205,871,288 spilled bytes**, **3,145,728 reserved bytes** and **one active preparation**. Whole-process CPU measurements include setup, indexing, searches, the final commit and cleanup. Host-wide pressure snapshots cannot isolate query CPU cost or exclude brief stalls. Mutexes, write guards and cancellation logic were not altered for this experiment.

## Independent verification and preservation

[Independent QA](qa-topology-results.md) fetched the artifact identity/digest and successful job status, verified all 148 entries of the original artifact manifest and every extracted byte, matched all 64 schedule/context/stdout records, checked the executable identities and document stamps, and independently recomputed all published comparisons. The machine-readable [QA report](qa-topology-results.json), exact [recalculation](qa-recalculation.json), [audit code](qa-audit.py) and [API provenance](qa-api-provenance.json) are preserved alongside the original [analysis](analysis.json) and [environment](environment.json).

The deterministic, lossless [raw text archive](text-evidence.tar.xz) contains **all 147 nonbinary files** from the Actions artifact, including every run's stdout/context, both schedules, original input/controller copies, full JSONL records, environment and original `SHA256SUMS`. Member paths and bodies are unchanged; only tar metadata are standardized for deterministic packaging. Two independent archive constructions produced identical bytes, and every archived member was compared byte-for-byte with both the downloaded ZIP and extracted input.

| Evidence | Value |
| --- | --- |
| Actions artifact | `economy-topology-42431565386-1` / ID `13597940923` |
| Actions ZIP size | 11,939,632 bytes |
| Actions ZIP SHA-256 | `45f82b3cba4c894ddcc4f2587eedbb409d2736ee1b28d6c0748313e265a56d48` |
| Original manifest SHA-256 | `5c1d187f8602819c9e1ea757adaf57fef79006d2944c13b45eb4dc4d44b8fe5d` |
| Lossless text archive size | 71,624 bytes |
| Lossless text archive SHA-256 | `f4875e2ed3227eb47fdc2ba35a431b1b252bd6de473df4b229176b16f063cab7` |

[Archive provenance](archive-provenance.json) lists every archived file's length and SHA-256, full immutable input identities, artifact metadata and omitted executable hashes. The two executable files remain in the original Actions artifact under its reported retention, expiring 2026-11-22; they are not committed to this text-evidence tree. After that retention ends, the source revisions, prior build commands/environment and expected hashes remain available for rebuilding and checking the binaries. The original `SHA256SUMS` still references those two executables; `raw_text_files` in the provenance verifies the text-only archive independently.

## Disposition

This experiment narrows the observed conditions of the regression. It does not resolve the source cause, remove the existing performance hold, or establish a production fix. Candidate-source experiments and behavioral gates must be evaluated separately. All earlier findings remain preserved, and both production proposals are unchanged by this evidence publication.
