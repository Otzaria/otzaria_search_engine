# Otzaria search engine

A Rust-based full-text search engine for the Otzaria project, built upon Tantivy with bindings to Dart through flutter_rust_bridge.

this is a Dart library and cannot run by itself.

## Paired readings and existing indexes

A single-token `(X) [Y]` reading pair is indexed as two alternatives at the
same word position. Either reading can complete a phrase; trailing separators
inside a reading, such as the maqaf in `(לך) [לכה־]נא`, do not add a word.
Readings containing multiple tokens are kept as separate words.

Existing indexes remain readable and are not automatically invalidated. Books
indexed before this change retain their original token positions: re-index the
affected books, or rebuild the index, to enable paired-reading phrases in those
documents. Newly indexed books use the new positions immediately.

Pasted query pairs select the second reading. `queryWordSpans` maps those engine
words to their exact UTF-16 ranges in the original query, so per-word options
and selections use the same word order. Native library, generated bindings and
the app's prepared highlight matcher integration must be updated together.

## Display highlighting

Native highlight generators return a prepared `HighlightPattern.matcher`.
Call `matcher.findMatches(data: text, requireTokenBoundaries: flags)` for phrase
matches, or `findWordMatches` for independent words. Each match has `start` and
`end` in Dart UTF-16 code units and `wordRanges` relative to `start`. The matcher
retains compiled word regexes and resolves gaps with a bounded token-based
algorithm, including HTML source offsets, nikud, and index quote synonyms.

The legacy `combinedPattern` is preserved for single-word callers. For phrases
it is the never-match sentinel `(?!)`; migrate applications to the prepared
matcher together with the regenerated Flutter Rust Bridge bindings. Compiling
full-phrase ECMAScript regexes with multiple word gaps can otherwise freeze the
rendering isolate on a short near miss. Every native generator supplies a
matcher; the nullable field only preserves manually constructed Dart fixtures.

## Semantic search integration

The native library can optionally link
[`otzaria-semantic-search`](https://github.com/Otzaria/otzaria-semantic-search)
into the same Flutter Rust Bridge library as Tantivy:

- `semantic` is the production build, and the one Cargokit builds. It compiles
  both real embedding backends, and the sidecar picks one per model by the
  model file's format: a path ending in `.onnx` is an ONNX graph for ONNX
  Runtime (`semantic-onnx`), and every other path is a GGUF for `llama.cpp`
  (`semantic-llama`). `semantic-real` remains as an alias of `semantic-llama`.
- The sidecar compiles a backend out on targets it cannot serve. The feature
  stays on and the build succeeds, with no backend behind that format; see
  "Builds without a backend".
  - `llama.cpp` is excluded on 32-bit ARM (`armv7-linux-androideabi`):
    `llama-cpp-sys-2` cannot build for that target, and a Q4 0.6B model would
    be unusable on it regardless.
  - The ONNX backend is built for desktop targets only (Windows, Linux and
    macOS). Android and iOS have none.
- `semantic-mock` selects the deterministic test backend and must not be used
  in an application release. CI builds the library with it so the Dart FFI
  suite can drive a configured sidecar.

### Configuring a model

`SemanticConfigInput` states how the vectors are produced, and nothing in it is
read from the model file, so the values must be the ones the model was built
for. The sidecar records every field but `rootDir` in its manifest as the
index's identity (the model file by its checksum, once it has loaded): an index
built under one value reports `needsFullReindex` under another, instead of
mixing in vectors that cannot be compared.

| field | Qwen3 GGUF | Meivin ONNX |
| --- | --- | --- |
| `modelPath` | the `.gguf` file | `seforim-embed-round2-int8.onnx`, with `tokenizer.json` beside it |
| `modelId` | `EMD123/Otzaria-Embedding-V1-Flash-0.6B` | `ArieLLL123/judaic-semantic-round2-onnx-zayit` |
| `embeddingDim` | 1024 | 256 |
| `pooling` | `last-token` | `in-graph` |
| `maxTokens` | 512 | 256 |
| `modelQuantization` | `Q4_K_M` | `int8` |
| `embeddingTextVersion` | 1 | 2 |

The Meivin model the application uses is its INT8 graph: the accuracy it gives
up is negligible, and it is a quarter of the size, which matters on weak
machines. The full-precision `seforim-embed-round2-fp32.onnx` published beside
it remains an alternative, configured with `modelQuantization: 'fp32'`; the two
are different identities, so vectors from one never mix with the other's.

```dart
await engine.configureSemantic(
  config: SemanticConfigInput(
    rootDir: semanticRoot,
    modelPath: '$modelDir/seforim-embed-round2-int8.onnx',
    modelId: 'ArieLLL123/judaic-semantic-round2-onnx-zayit',
    embeddingDim: 256,
    pooling: 'in-graph',
    maxTokens: 256,
    modelQuantization: 'int8',
    embeddingTextVersion: 2,
  ),
);
```

A value the sidecar does not implement — an unknown pooling, a text recipe
version it has no code for, a token cap below 2, or for an ONNX model a cap
above 65,536 — is refused by `configureSemantic` itself, and so is an empty
`modelQuantization`. The ONNX ceiling is past the context of any ONNX sentence
encoder, and it is what keeps a negative `maxTokens`, which arrives as a cap in
the billions, from reaching the model's load-time probe. A GGUF cap has no
such bound: llama.cpp clamps it to the model's context.

### The ONNX Runtime library

The ONNX backend links nothing native. ONNX Runtime is a shared library the
sidecar loads when an ONNX model loads, so the plugin's build downloads nothing
and its binary depends on no new system library, and the application has to
provide the runtime. The first of these that exists is used:

1. the file named by the `OTZARIA_ONNX_RUNTIME` environment variable;
2. the platform's default file name (`onnxruntime.dll`, `libonnxruntime.so` or
   `libonnxruntime.dylib`) in the model directory, beside the `.onnx` graph.

The reference runtime is Microsoft's official ONNX Runtime 1.28.0 release on
GitHub, and the oldest runtime API accepted is ONNX Runtime 1.17's. The runtime
is code rather than model data, so it is not part of the model checksum.

Without a runtime that loads (none found, not a runtime, too old, or a different
one already loaded in the process), loading the model fails with "ONNX Runtime
could not be loaded: …", which names both places above. That text is in the
error `semanticIndexBooks` throws and in `SemanticStatus.lastError`. It is not
the "No embedding backend is available in this build" of a build without the
backend: here the fix is the library, not a rebuild. Otherwise the model behaves
as on such a build (see below), and lexical search is unaffected.

On macOS, an application built with the Hardened Runtime, which notarization
requires, loads only libraries signed by Apple or with its own Team ID. It may
therefore refuse Microsoft's `libonnxruntime.dylib` from the model directory
even when the file is intact, and the loader's reason then appears in that
message. What works is shipping the library inside the application bundle,
signed with the application's identity, and naming it with
`OTZARIA_ONNX_RUNTIME`.

### Builds without a backend

A build can hold the integration with no backend for the configured model's
format: a GGUF model on 32-bit ARM, an ONNX model on Android or iOS, or a build
whose feature for that format is off. An ONNX model whose runtime cannot be
loaded behaves the same way; only the message `semanticIndexBooks` throws
differs (see above). `available` — not `enabled` — is the flag that says so:

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
explicit way to switch model, recipe or library root, and it discards the
session's vectors.

`semanticIndexBooks`, `removeSemanticBooks`, `resetSemanticIndex` and
`semanticStatus` are all non-exclusive and asynchronous, so lexical search and
status polling stay responsive while a semantic index is being built.

## Getting Started

clone otzaria repo and this repo to the same path, cd to otzaria and run flutter run.

### Android C++ runtime

The signed Android artifacts include the matching `libc++_shared.so`; consumers
do not need a local NDK when a precompiled artifact is available. If another
Flutter plugin also packages that runtime and Gradle reports a duplicate native
library, select one copy in the application module's `android` block:

```groovy
packagingOptions {
    jniLibs.pickFirsts += ['**/libc++_shared.so']
}
```

## Git hooks (one-time setup per machine)

After cloning, run once to enable automatic formatting + LF normalization on every commit:

    dart run tool/install_hooks.dart

This sets `core.hooksPath` to the repo's `.githooks/` directory. The pre-commit hook
runs `dart format` / `rustfmt` on staged files and converts CRLF→LF, preventing the
Windows line-ending issues that break the published pub.dev package on macOS/Linux/Android.
