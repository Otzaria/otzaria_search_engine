# Otzaria search engine

A Rust-based full-text search engine for the Otzaria project, built upon Tantivy with bindings to Dart through flutter_rust_bridge.

this is a Dart library and cannot run by itself.

## Semantic search integration

The native library can optionally link
[`otzaria-semantic-search`](https://github.com/Otzaria/otzaria-semantic-search)
into the same Flutter Rust Bridge library as Tantivy:

- `semantic` / `semantic-real` builds the production `llama.cpp` backend.
- `semantic-mock` selects the deterministic test backend and must not be used
  in an application release.
- The dependency is pinned in `rust/Cargo.toml` and `rust/Cargo.lock`; a moving
  branch must not be used because model and vector-index identity depend on the
  exact implementation.

`SearchEngine` exposes configuration, status, index diff/index/remove/reset,
and unified lexical/hybrid/semantic search. Exact/fuzzy lexical interpretation
is kept separate from the lexical/hybrid/semantic retrieval mode. Hybrid
requests fall back to Tantivy with an explicit reason when semantic support is
unavailable; semantic-only requests never masquerade as lexical results.
`lexicalTotalCount` is Tantivy's corpus count. The sidecar's `totalCount` and
`groupCount` describe its bounded fusion candidate set, so
`countsAreExact` is false for sidecar-backed responses; callers must not use
them as a corpus-wide semantic result count. `candidateWindowTruncated`
separately reports the hard candidate-window cap.

The current upstream vector store is in-memory. Check
`SemanticStatus.vectorsPersisted` and `needsFullReindex`; until a persistent
backend lands, semantic vectors must be rebuilt after every process restart.
Indexing progress and cooperative cancellation are not yet exposed upstream.

## Getting Started

clone otzaria repo and this repo to the same path, cd to otzaria and run flutter run.

## Git hooks (one-time setup per machine)

After cloning, run once to enable automatic formatting + LF normalization on every commit:

    dart run tool/install_hooks.dart

This sets `core.hooksPath` to the repo's `.githooks/` directory. The pre-commit hook
runs `dart format` / `rustfmt` on staged files and converts CRLF→LF, preventing the
Windows line-ending issues that break the published pub.dev package on macOS/Linux/Android.
