# Reservation-gated snippet rendering: isolated A/B

This branch does not alter the production PR, the MCP adapter, or the original
diagnostic script in its original branch. It reuses the reviewed 32-process
counterbalanced BHHB/HBBH schedule and its same-binary controls, but compares:

- B: 4761356749815d283ede41ef956c64b3dbffe048 (fully tested shared core before rendering gate).
- H: 4bb3d8e7d11499861ce0655c88841eb23e0f1649 (shared core with advisory reservation-gated `with_min_len`).

The existing immutable 12,000-line fixture is unchanged and SHA-gated.
Two independent GitHub Actions jobs run the same schedule on (a) sibling
hardware threads of the same physical CPU core and (b) different cores in the
same package, refusing to run if topology is unavailable.

Both jobs compile the two revisions independently with Rust 1.99.1 and
preserve exact executable hashes, process-level p50/p95/p99, source revision,
document checksum, CPU topology, resource-pressure snapshots and the real /
same-binary schedule. They do not modify either production commit, alter test
thresholds, or claim numerical equivalence to a full Otzaria library.

This experiment isolates the runtime scheduling change from all other changes
in the shared-core migration. Independent QA should compare controls, all real
blocks, physical-topology differences and same-source binary identity before
recommending promotion from draft. Success is not a merge instruction.
