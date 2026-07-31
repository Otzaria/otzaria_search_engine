# Otzaria search engine

A Rust-based full-text search engine for the Otzaria project, built upon Tantivy with bindings to Dart through flutter_rust_bridge.

this is a Dart library and cannot run by itself.

## Semantic search integration

The native library can optionally link
[`otzaria-semantic-search`](https://github.com/Otzaria/otzaria-semantic-search)
into the same Flutter Rust Bridge library as Tantivy:

- `semantic` / `semantic-real` builds the production `llama.cpp` backend —
  except on 32-bit ARM (`armv7-linux-androideabi`), where the sidecar excludes
  it by target. `llama-cpp-sys-2` cannot build for that target, and a Q4 0.6B
  model would be unusable on it regardless. The feature stays on and the build
  succeeds, but there is no backend behind it. See "Builds without a backend".
- `semantic-mock` selects the deterministic test backend and must not be used
  in an application release. CI builds the library with it so the Dart FFI
  suite can drive a configured sidecar.

### Builds without a backend

A `semantic` build on 32-bit ARM compiles the integration but has no embedding
backend. `available` — not `enabled` — is the flag that says so:

| call | on such a build |
| --- | --- |
| `configureSemantic` | succeeds: `enabled: true`, `available: false`, `embeddingBackend: null` |
| `searchSemantic` | falls back to lexical with an explicit `fallbackReason` |
| `semanticIndexDiff` | reports `enabled: true` and lists the books as new |
| `semanticIndexBooks` | **throws** — there is nothing to embed with |

So a caller must gate indexing on `available`, not on `enabled` or on a
non-empty diff. Search needs no such guard: it degrades on its own.
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

### The display contract

`SemanticSearchResult.snippetHtml` is the display string, in the same format
every other search API here returns: HTML-escaped and painted with the default
`HighlightConfig` markup where the lexical query matched. `max_chars` bounds how
much of the line is shown, with markup and escaping added on top. This holds on
the sidecar path and on the lexical fallback alike, so the app's snippet parser
cannot tell which one served a page, and a purely semantic hit never arrives as a
raw unbounded line. Use `getDocumentById` when the full line is needed.

`isHighlighted` says whether markup is present, and it never overstates the
match. A multi-word query carries a phrase constraint: when the chosen fragment
holds no complete in-order occurrence, the lexical API falls back to painting the
individual words, which is sound only because Tantivy already proved the document
satisfies the phrase query. A result that reached the page through vector
similarity alone never passed that query, so it is left unpainted rather than
have its scattered words suggest a phrase match.

Snippets are built after fusion and pagination, for the returned page only. The
sidecar path paints against the mark-free stored `text` field, since that is the
copy the sidecar indexes and hydration reads back: a vocalized query still
selects documents by their marks, but the line it shows is the mark-free one.

### Session lifecycle

The current upstream vector store is in-memory. Check
`SemanticStatus.vectorsPersisted` and `needsFullReindex`; until a persistent
backend lands, semantic vectors must be rebuilt after every process restart.
Indexing progress and cooperative cancellation are not yet exposed upstream.

Because the store is in-memory, opening an engine drops the manifest records
whose vectors did not survive, so `configureSemantic` does not re-open a live
session: calling it again with the same inputs is a no-op, and calling it with
different inputs fails and names the input that changed. `disableSemantic` is the
explicit way to switch model or library root, and it discards the session's
vectors.

`semanticIndexBooks`, `removeSemanticBooks`, `resetSemanticIndex` and
`semanticStatus` are all non-exclusive and asynchronous, so lexical search and
status polling stay responsive while a semantic index is being built.

## Getting Started

clone otzaria repo and this repo to the same path, cd to otzaria and run flutter run.

## Git hooks (one-time setup per machine)

After cloning, run once to enable automatic formatting + LF normalization on every commit:

    dart run tool/install_hooks.dart

This sets `core.hooksPath` to the repo's `.githooks/` directory. The pre-commit hook
runs `dart format` / `rustfmt` on staged files and converts CRLF→LF, preventing the
Windows line-ending issues that break the published pub.dev package on macOS/Linux/Android.
