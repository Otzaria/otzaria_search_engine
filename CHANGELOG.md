# Changelog

## Unreleased

> Breaking for Dart code that constructs `SemanticConfigInput` or
> `SemanticStatus`, that calls `searchSemantic`, or that catches the semantic
> calls' `AnyhowException`, and for an application that configures a GGUF model,
> so this must not ship as a 0.8.x patch: `^0.8.7` would take it on its own
> (see 0.8.0). An existing lexical index is not one of them: it opens, and
> searches, as it did.

**The application never builds the library's vectors.** The build machine
embeds the whole library into a release of its vectors; the application
installs it into a vector set with the new `installSemanticVectors`, opens the
set with the new `openSemanticArtifact`, and embeds only the query.
`configureSemantic`, `semanticIndexBooks`, `semanticIndexDiff`,
`removeSemanticBooks` and `resetSemanticIndex` build vectors on the device, and
are now documented as development and testing scaffolding, not for the library.

### Breaking

- **`SemanticConfigInput` states the whole recipe: four new required fields.**
  `pooling`, `maxTokens`, `modelQuantization` and `embeddingTextVersion` were
  taken silently from the sidecar's defaults, which fit only the Qwen3 GGUF
  model 0.8.7 served, and recorded `model_quantization` as `"Q4"` where that
  model's identity says `"Q4_K_M"`. Each is part of the index's identity, and
  the ONNX model differs in all four, so they are now the caller's to state:

  | field | Meivin ONNX |
  | --- | --- |
  | `pooling` | `'in-graph'` |
  | `maxTokens` | 256 |
  | `modelQuantization` | `'int8'` |
  | `embeddingTextVersion` | 2 |

  The Meivin column is its INT8 graph, `seforim-embed-round2-int8.onnx`, the
  model the application uses: negligibly less accurate than the full-precision
  `seforim-embed-round2-fp32.onnx` and a quarter of its size. The fp32 graph
  remains an alternative under `'fp32'`, a different identity.

  They map onto the sidecar's `pooling`, `embedding_max_tokens`,
  `model_quantization` and `chunking.embedding_text_version`, and the sidecar
  validates them when `configureSemantic` opens it. It refuses an unknown
  pooling, a text recipe it has no code for, a cap below 2 and a cap above
  65,536, which is also where a negative `maxTokens` lands: it arrives as a cap
  in the billions. An empty `modelQuantization` is refused before that.
  `configureSemantic` now compares all eight fields, so a different recipe
  under the same model file is refused by name, like a different model,
  instead of being accepted as a repeat.

  **What changes for consumers.** Every construction has to pass the four
  fields. The Otzaria app constructs `SemanticConfigInput` only in
  `test/search/semantic_search_gateway_test.dart`. A sidecar root written by
  0.8.7 or earlier holds vectors of the Qwen3 GGUF model, which no build serves
  any more (next entry); the in-memory store needed a full re-index after every
  restart anyway.
- **GGUF models and llama.cpp are no longer supported: `semantic`, the
  production feature, is the ONNX backend.** In 0.8.7 `semantic` was llama.cpp,
  for the Qwen3 GGUF model. It is now the ONNX backend, for the Meivin model the
  application uses, and ONNX is the only format any build serves: the
  `semantic-llama` feature and its `semantic-real` alias are removed, so no
  build compiles llama.cpp or ggml, through cmake or otherwise. The sidecar is
  pinned at dc11d59, the merge that removed GGUF from it too. A GGUF model, or
  any model path that does not end in `.onnx`, is refused by its name rather
  than served: opening an artifact with it, or indexing with it on the
  development path, throws a `SemanticError` of kind `modelInvalid` whose
  `field` is `model_path`, with the sidecar's message ("… GGUF support was
  removed …"). `backendNotInBuild` keeps its meaning: an ONNX graph on a build
  without the ONNX backend. `cargokit.yaml` still builds `--features semantic`.
  On Android and iOS, which have no ONNX backend, the production build serves
  no model at all. The last commit of this plugin with GGUF support is eb42ebd.

  The build settings only llama.cpp needed went with it. The podspecs no longer
  link `c++` or the Accelerate, Metal, MetalKit and Foundation frameworks. The
  plugin's Android `minSdkVersion`, and the Android platform the precompiled
  binaries are built for, are back from 23 to 21, which only llama.cpp's
  `posix_madvise` had raised. The Linux release container no longer installs
  cmake or libclang-dev, and the Windows ARM release build sets only
  `CC=clang-cl`, without Ninja or llama.cpp's C++ flags. Cargo.lock loses
  llama-cpp-2, llama-cpp-sys-2, bindgen, cmake, clang-sys and the 14 other
  crates only they pulled in.

  **What changes for consumers.** An application that configures a GGUF model
  has to move to the ONNX model: no build of this release serves the GGUF one.

- **The semantic calls throw `SemanticError`, not `AnyhowException`.**
  `configureSemantic`, `openSemanticArtifact`, `searchSemantic`,
  `semanticIndexBooks`, `semanticIndexDiff`, `removeSemanticBooks` and
  `resetSemanticIndex` return `Result<_, SemanticError>` in Rust, which
  flutter_rust_bridge throws as an exception class of its own, an
  `FrbException` with `kind`, `message` and `field` (next entry); an
  `on AnyhowException` clause no longer catches them. `message` is the text the
  calls already produced, except that a context chain now reads on one line
  (`reading …: …`), where the exception's debug rendering put each cause on a
  `Caused by:` line of its own. No other API changes its error type.
- **`searchSemantic` takes a required `cancellation`**, a
  `SemanticCancellationToken` (next section). Required rather than optional
  because flutter_rust_bridge 2.13 cannot pass an optional borrowed opaque
  object: an `Option<&T>` argument generates Rust that does not compile, and
  passing the token by value would move it, and dispose it on the Dart side. A
  call with nothing to cancel passes `SemanticCancellationToken()`, which
  changes nothing.
- **`SemanticStatus` gains a required `state`**, and `errorKind` beside
  `lastError`; **`SemanticSearchResponse` gains `fallbackKind`** beside
  `fallbackReason`. Dart code that constructs a `SemanticStatus`, such as a test
  fake, has to pass `state`.

### Added

- **New indexes store each line's chunk key, and a version 4 index keeps
  working as it is.** A new `chunkKey` column holds, for every line
  `addTextBook` adds, the key of the text the line is embedded as: the first
  eight bytes of the SHA-256 of that text, read big-endian (the sidecar's
  `ChunkKey::column_value`). It is 0 for a line the recipe does not embed, for
  a PDF's lines, and for documents added one by one or in batches. The column
  is what ties a stored vector to the lines that hold its text, by content
  rather than position, so that a library update leaves the vector of every
  unchanged line usable.

  The key is computed after normalization, from what the index stores: the
  line, its neighbours within two lines in its section, and the sections the
  `<h` headings open. It uses the sidecar's own chunker under Meivin Round 2's
  chunking, which is compiled in because indexing runs with no model. The
  SHA-256 runs in parallel. `FAST` only: the column is neither indexed nor
  stored. Measured on 300 books of the library, 286,235 lines, on an Apple M4:
  the column costs 8.0 bytes a line, which is 55 MB over the library's 6.9
  million lines, about 1.3% of the 3.9 GiB release index. The keys took 0.21 s
  of an indexing run of about 3.1 s, which is 0.75 µs a line, or 1.9 µs a
  line on one core: about 13 s of CPU for the whole library.

  `INDEX_SCHEMA_VERSION` is 5, and **an index of version 4 is not rebuilt**.
  `checkIndexCompatibility` reports it `compatible`. The engine opens it under
  its own schema, searches it, and adds books to it as before, and it stays
  version 4 with no column. Only an index this version creates, a new one or a
  rebuild, has the column. Its `otzaria_index_meta.json` records the recipe the
  column was written under: `line_text_version`, `chunk_key_version` and
  `chunk_key_chunking_identity`. A column written under another recipe, or
  under none that is known, counts as absent: it never means a rebuild, and
  nothing writes to it. An engine before this one reports a version 5 index
  as `engine_too_old`, which it is.

  `semantic_keys::recompute_chunk_key` (Rust only) computes a line's key again
  from the stored text of its window and the `sectionId` column. This is what
  an index without a usable column is asked, and what a result is checked
  against before it is shown.

  Tested:
  - a synthetic book keyed to the values Python's hashlib computes over
    embedded strings written out by hand;
  - every line recomputes to the key its column holds;
  - the parallel keys equal the sidecar's `chunk_keys`;
  - the compiled-in chunking is the one the model family publishes, against
    the pinned sidecar's files in the real-model job;
  - version 4's schema is the one in the release index's `meta.json`;
  - a version 4 index, with or without its metadata, opens compatible,
    searches, takes books without the field, and stays version 4;
  - a new index records its recipe;
  - every add path but `addTextBook` writes 0;
  - a column under another recipe, or unparseable metadata, counts as absent
    and leaves the index compatible.

  A copy of the release index (6,042,284 lines, version 4) opened compatible
  under this engine, served searches, and took a book. On the 300 books, every
  line's column equalled its key recomputed from the stored text.
- **Typed semantic failures and states, for an application to switch on.** The
  sidecar types its errors and leaves turning them into user-facing states to
  the host; now the plugin does. `SemanticErrorKind` names what stopped the
  semantic path: no session open, or no semantic support in the build; the
  vectors missing, corrupt, incompatible (with the first field that
  disagreed) or not the release published; too little disk space to install
  or compact them; the model missing, invalid or not the one its
  identity describes, or its tokenizer missing; ONNX Runtime missing or
  unusable; no backend for the model's format in this build; another session
  open, a read-only session, a re-index needed, one query's semantic half
  failed, a search cancelled; an invalid input; or an internal fault. `SemanticState` says what a
  session can do: `notInBuild`, `notConfigured` and `ready` on the
  application's path, and `empty`, `needsReindex` and `failed` for a session
  built on the device. The doc comment of `SemanticErrorKind`, the README and
  API_DOCUMENTATION have the table of each kind, what it means, and what the
  application should do.

  A kind is decided from the type of the failure, the sidecar's typed errors and
  the plugin's own (the model identity, the chunking, sessions), and never by
  reading a message. Where the sidecar uses one type for two states, a fact
  decides: a set with neither `CURRENT` nor `PREVIOUS` makes unusable metadata
  a missing set rather than a damaged one, and a file where ONNX Runtime is looked for makes a
  runtime that did not load unusable rather than missing. The installation's own
  identity values are checked first, by the sidecar's own functions, so a value
  no build serves is `invalidInput` and what opening refuses after it is the
  set's, the model's or the runtime's. The matches name every sidecar
  variant with no wildcard, so a repin that adds one does not compile until it
  is classified. Where nothing can place a failure precisely it gets the broad
  kind that is true of it: a semantic half that failed during a search is
  `queryFailed`, since the sidecar reports it as text only, and a development
  session's `lastError` is `internal`, while the call that failed threw the
  precise kind.

  Tests produce each kind from the real failure: in
  `rust/tests/semantic_artifact.rs`, a missing set, garbled pointers, a flipped
  segment byte, a damaged block found by verifying, a truncated compressed
  segment, every kind of wrong model identity, another chunking, a
  missing, invalid or tokenizer-less model, an invalid identity value, a
  manifest that is not the published one, a compaction threshold out of range,
  the read-only refusals and a query with nothing to embed; in `rust/tests/semantic_mock_integration.rs`, the
  development session's states and refusals; in the new
  `rust/tests/semantic_onnx_errors.rs`, a GGUF model, which no build serves,
  and an ONNX model with no runtime or one that does not load, which
  needs neither the real model nor a runtime and so runs in every
  `semantic-onnx` job; and a build without semantic support, in the unit tests.
  The FFI suite matches its refusals by kind across the bridge.

- **A semantic search can be cancelled.** The application searches as the user
  types, so every query but the last is obsolete before it finishes, and a
  semantic query embeds the text and then scans every vector, about a second
  over the library. `SemanticCancellationToken` is an opaque object with a
  factory constructor and a synchronous `cancel()` and `isCancelled`; Rust holds
  the sidecar's own `CancellationToken` in it (the sidecar's since 62f0c44), and
  `searchSemantic` borrows it, so the application keeps the object and cancels
  it from the isolate that started the search while the search runs: both take
  it by shared reference, and neither waits for the other. The search looks
  before its lexical phase, hands the token to the sidecar's
  `search_cancellable`, which looks before and after it embeds the query, every
  1,024 records of the scan and around fusion, and looks again before it
  hydrates and before it paints; a lexical fallback is looked at before it runs
  and once its page is ready. The first look after a cancel throws
  `SemanticError` with the new kind `cancelled`: not a failure, never answered
  with lexical results instead, and, when the sidecar stops it, leaving its
  caches untouched. Tested: in the unit tests, with a probe that cancels at a
  chosen look, a search with a session stops at exactly that look in every mode,
  the sidecar's own included (which fails if the token is not handed over), and
  serves the same page afterwards; a pre-cancelled search does nothing in any
  build; through the public API and across the bridge, a cancelled search throws
  `cancelled` in every mode, the token outlives the search that borrowed it, and
  `cancel()` returns while a search holds it.
- **Every ranking parameter can be passed with a search.** `searchSemantic`
  takes an optional `ranking`, a `SemanticRankingOptions`: the fusion strategy
  (`SemanticFusionStrategy`: weighted, RRF with its `rrfK`, adaptive), one
  `alphaOverride` or an alpha per kind of query (`SemanticQueryTypeAlphas`),
  BM25's saturation `k`, the semantic threshold, the agreement, phrase,
  rare-word and section bonuses and the duplicate penalty, metadata ranking and
  the semantic candidate window's multiplier, mirroring the sidecar's
  `RankingProfile` field for field and handed to it as `HybridSearchParams::ranking`
  (since the sidecar's 62f0c44). Without it, or with the defaults, which the
  Dart constructor carries and `SemanticRankingOptions.defaults()` reads from
  the engine, a search ranks exactly as before: the defaults are the sidecar's
  `Balanced` preset, value for value. **They are unmeasured placeholders**; calibrating
  them needs a labelled relevance set, and this lets that happen from the
  application without a release of the engine. An option out of its range, or
  not a number, is refused before the search runs, with or without a session,
  by the sidecar's own `RankingProfile::validate`: `invalidInput`, whose
  `field` names the option (`alpha_by_query_type.short`, `rrf_k`), never a
  clamped value. Values are passed as doubles and ranked at 32 bits. A build
  without semantic support ignores them. Tested: the defaults map to the
  preset exactly, every option to its own field, and every out-of-range value
  is refused by name, in the unit tests; `None` and the defaults rank every
  page alike to the bit, an RRF ranking scores the line both sides rank first
  `1 / 31` from each, and a bad option is refused with no session open, in the
  mock suite; and the same across the bridge, where the Dart defaults equal the
  engine's.
- **`openSemanticArtifact`: the application's semantic path.** It opens a
  vector set read-only, verifies every field of its identity against this
  installation, and serves `searchSemantic` from it with each result hydrated
  from the lexical index. `SemanticArtifactInput` takes the set's directory,
  the model file, the text of the model's identity file (the one the vectors
  were built with, such as the sidecar's
  `config/models/meivin-round2-onnx/model.json`) and, optionally, the ONNX
  Runtime and the number of threads a search scans with. The sidecar is pinned
  at 04a2cc9, its `onnx-backend` with the `store-v2` branch, its two rounds of
  audit fixes and `scan-with` merged, which keys a vector by the text it was
  embedded from, so a set's
  identity is a line recipe and a model family, with nothing positional in it:
  - The text half is the line recipe of the index, which this plugin declares
    as `LINE_TEXT_VERSION` 1: split on `\n`, `normalize_text_for_indexing`, a
    line starting with `<h` opens a section, one document per line. The other
    part of it is the sidecar's key version.
  - The model half describes a family: `family_id`, `tokenizer_checksum`, the
    recipe the vectors were built under, the chunking among it, and the
    `query_packages` a query may come from, which are the INT8 and fp32
    graphs. The graph at `modelPath` must be one of those packages, by its
    checksum, and its tokenizer must be `tokenizer_checksum`; either refusal
    is `modelIdentityMismatch`. A set chunked otherwise than this build keys
    lines is `artifactIncompatible` on `model.chunking_identity`.

  On an opened set, `semanticIndexBooks`, `removeSemanticBooks`,
  `resetSemanticIndex`, `semanticIndexDiff` and `configureSemantic` are refused
  as read-only. It takes the engine's read lock, so lexical search keeps
  serving while the model and the vectors load.
- **A set's hits are resolved against the open index, by the key of their
  text.** Nothing ties a set to one index, and nothing is stamped into one: a
  commit after opening leaves the set serving. Each hit the sidecar scans names
  its records by book and a hint, the line it held when the set was built. The
  plugin's resolver checks the hint, then the book's lines by distance from it,
  then, for what its books no longer hold, every book's `chunkKey` column in one
  pass, and remembers what it found nowhere until the index changes. On an index
  without the column it recomputes the keys around the hint from the stored
  text instead. Filters are applied by book, from a directory of the index's
  books built once per index generation. On an index with the column (schema
  version 5) a line that moved is found where it is now, and a text that left
  its book for another is found there. On one of version 4 — the published v30
  library index is one — a line is found only within 16 lines of where the set
  recorded it, in the same book: not a line moved further, and not a text in
  another book. That matters only while the index and the vectors are of
  different library versions, or after the index changed on the device. A
  line whose text is gone, or whose embedded text changed with its neighbours,
  is not shown.
  Results hydrate by their address in the index the search read, and every line
  shown is checked first by recomputing its full 128-bit key from the stored
  text: a semantic match that fails is dropped, and one the lexical side found
  too is shown as lexical; the response's note counts them.
- **Installing and keeping the library's vectors.** `installSemanticVectors`
  installs a release, a segment and its manifest, into the set at `vectorsDir`,
  checked against the manifest's published SHA-256 and this installation's
  identity: a base replaces the set, a delta brings it to the next library
  version, a segment compressed with zstd (`.oxv.zst`) is expanded first, and an
  open session on the set moves onto the new generation. `semanticVectorsInfo`
  reads what is installed from its small files; `compactSemanticVectors` merges
  the set into one segment under a `SemanticCompactionPolicy`, the sidecar's
  defaults on both sides of the bridge, and, given the index's library version,
  re-anchors every record on the line that holds its text now;
  `verifySemanticVectors` checks every block of every segment against its
  checksum; and `semanticCoverage` counts the live lines the recipe embeds and
  those the set holds. All are cancellable through a
  `SemanticCancellationToken`, and the set is left as it was by any that is
  refused, cancelled or cut off. `SemanticStatus` gains `vectorsLibraryVersion`,
  `vectorSegments` and `needsCompaction`, and `SemanticErrorKind` gains
  `insufficientDiskSpace`. The decoder is the zstd crate tantivy already builds,
  so `Cargo.lock` gains no crate; `sha2` is now a test dependency only.
- **The ONNX embedding backend, `semantic-onnx`**, for ONNX graphs such as the
  Meivin model. It links nothing native: the sidecar loads the ONNX Runtime
  shared library when a model loads, from the path the application passes
  (`onnxRuntimePath`, next entry), else `OTZARIA_ONNX_RUNTIME`, else the
  platform's default file name beside the `.onnx` graph. **An application that
  configures an ONNX model has to provide that library**; the reference is
  Microsoft's ONNX Runtime 1.28.0 release, and the oldest runtime API accepted
  is 1.17's. Without one that loads, opening a vector set (or, on the
  development path, indexing) throws an error that says "ONNX Runtime could not
  be loaded: …" and what each place held, not the no-backend error of a build
  without it; semantic search reports itself unavailable, and lexical search is
  unaffected. On macOS a Hardened Runtime application loads
  only libraries signed by Apple or with its own Team ID, so the runtime belongs
  inside the signed bundle, passed as `onnxRuntimePath`. The runtime is
  not part of the model checksum. The backend is built for desktop targets only.
- **`onnxRuntimePath`: the ONNX Runtime the application ships.**
  `SemanticArtifactInput` and `SemanticConfigInput` gain an optional
  `onnxRuntimePath`, the shared library an ONNX model runs on, which the plugin
  hands to the sidecar as its `EmbeddingDeployment` (which the sidecar added in
  62f0c44). It is the first place looked and, once passed, the
  only one: a path that names no file is `onnxRuntimeMissing` and one that does
  not load `onnxRuntimeUnusable`, never a fall-back to `OTZARIA_ONNX_RUNTIME` or
  to the file beside the graph, which stay the second and third places; an empty
  one is `invalidInput` before anything is opened. No identity reads it: the
  manifest does not record it, and a vector set opens whatever it names. Both
  calls compare it on a repeat all the same, since a process keeps the first
  runtime it loads and cannot replace it: another path while a session is open
  is a `sessionConflict` naming `onnx_runtime_path`, rather than a no-op that
  would leave the caller believing its runtime is in use, and after
  `disableSemantic` a session that names another runtime is
  `onnxRuntimeUnusable` until the process restarts. The README and
  API_DOCUMENTATION give the layout the application installs: `<root>/otzaria/`
  holds `seforim.db` and the model package's folder (the graph,
  `tokenizer.json` and the identity file `model.json`), `<root>/index/` the
  lexical index, and `<root>/vectors/` the vector set; the
  runtime ships with the application, or sits beside the graph as the build for
  that machine. Microsoft's macOS build of 1.28.0 is arm64 only and needs macOS
  14 (its `LC_BUILD_VERSION` minimum), so on macOS 12 and 13, which the plugin
  supports, it is `onnxRuntimeUnusable`. Tested: a passed path that names no
  file, and one that does not load, in `rust/tests/semantic_onnx_errors.rs`; the
  comparison on a repeat and the empty path, for both calls, with the stand-in
  and across the bridge; and in the real-model suite, the vector set opened on the
  passed runtime alone, in a child process without `OTZARIA_ONNX_RUNTIME` and
  with nothing beside the graph, and a second runtime refused after it.
- **Tests against the real Meivin model**, `rust/tests/semantic_onnx_model.rs`:
  a handful of lines indexed through the public API, and queries that must rank
  the line they are about first. A ranking cannot tell whether the role
  prefixes reached the model, so text recipe 2 is also checked against its
  definition: it must score every pair exactly as recipe 1 does when handed the
  `[PASSAGE] ` / `[QUERY] `-prefixed strings. A third runs the application's
  path: the build binary embeds the lines into a base package and installs it,
  and `openSemanticArtifact` opens the set and must rank the same lines first,
  using the model's published identity files from `OTZARIA_TEST_ONNX_IDENTITY`.
  `#[ignore]`d, and they skip loudly unless `OTZARIA_TEST_ONNX_MODEL`,
  `OTZARIA_ONNX_RUNTIME` and, for the third, `OTZARIA_TEST_ONNX_IDENTITY` name
  what they need; with `OTZARIA_REQUIRE_ONNX_MODEL` set, as the Dart suites
  have `OTZARIA_REQUIRE_NATIVE`, each skip is a failure instead.
- **`export_semantic_plan` writes the sidecar's plan of a vector build**, in one
  step from the release index: `records.bin`, `books.json`, `embed.jsonl` and
  `embed-manifest.json`, `tombstones.bin` and `plan-manifest.json`, each by the
  sidecar's own writer, for its `embed-shard`, `warehouse-add` and `assemble`.
  `--warehouse` leaves out of `embed.jsonl` every text the warehouse holds a
  vector for, refusing a warehouse of another model or package, and
  `--previous-ledger` splits the plan against the release before it, which
  writes its tombstones. Lines are keyed from the text the index stores, under
  the chunking compiled in, so a version 4 index plans as a version 5 one does;
  a version 5 index's `chunkKey` column is held to that text line by line, and
  a plan whose column disagrees fails the manifest's parity gate and the export.
  A PDF's lines are not planned, since the index keys them 0 and a device never
  resolves to one. On the v30 release index (6,042,284 lines in 7,376 books,
  version 4) it plans 5,753,225 records and 5,510,809 distinct texts, leaving
  out 132,076 PDF lines, in 60 s and 3.6 GB on an Apple M4. That index holds
  53,493 line ids that two books share, which the build binary's corpus
  refuses; the plan keys a line by its book and position and is not affected.
  Tested over a small library, where the plan is byte for byte what the
  sidecar's `plan_from_corpus` writes, from a version 5 and a version 4 index
  alike.
- **`validate_semantic_vectors` is the publishing pipeline's gate on a
  release**, for what the sidecar's `assemble --verify` cannot check without
  the release index. It installs the releases given with `--release` (the
  published chain first, the new one last) into a set of its own as a device
  installs them, or takes one installed already with `--vectors`, and checks
  **G3**: every line of the index the recipe embeds has its text recorded by
  the set in its own book, by all 128 bits of the key, and with `--plan`
  every record of the plan is reachable in the set; **G4**: every record
  resolves on the index by all 128 bits of its key, a
  record of a book the index lacks counting as unresolved, and every line's
  `chunkKey` column is its text's, with records off their hint reported and
  failing it only past `--max-stale-hints`; and **G6**, with `--warehouse`,
  `--model` and `--model-identity`: mean recall@10 and recall@50 of the set's
  scan against the sidecar's exact `f32` reference over the warehouse, on
  queries embedded once by the runtime query model, at least `--min-recall-10`
  (0.98) and `--min-recall-50` (0.99). The queries are `--queries`, one per
  line, or 200 spans of the index's lines drawn with a fixed seed. Recall is
  counted over keys, which are distinct texts: a scan returns a text once,
  however many books hold it, so repeated texts cannot take the top 50 as they
  do on a page of lines. A release passes when every gate ran and passed: a
  gate whose inputs are not given has not run, which fails it, unless the
  caller skips that gate by name (`--skip G6`), which the output and the
  report record; skipping all three is a wrong argument, since it validates
  nothing. Exit 0 when every gate passed or was skipped so, 1 when one
  failed or did not run, 2 for wrong arguments or inputs that do not read;
  `--report` writes every gate's verdict and numbers as JSON. On the v30 set a scratch harness of
  the same measurement gave recall@10 0.987 to 0.994 and recall@50, over
  distinct texts, 0.993 to 0.995. Tested through the pipeline itself: a plan,
  a warehouse of the stand-in's vectors, a base assembled and installed, which
  passes; and a book gone, stale hints past the limit, a line of the index the
  set does not cover (with no plan and no warehouse given), a plan the set
  does not reach, a warehouse of other vectors and wrong arguments, which each
  fail.
- **Tests of the vector-set path with the stand-in**, `rust/tests/semantic_artifact.rs`:
  a base package built by the binary from a small index, installed by the
  binary and through the API, plain and compressed, opened, searched both ways
  and hydrated; lines inserted above, moved to another book, gone, of a changed
  context, in two books, and in a book moved to another category, on an index
  with the column; on one of version 4, a line inserted above, a passage
  repeated, compaction, coverage, and what it does not find (a line moved
  beyond 16 lines, a text moved to another book, filtered or not); refusals
  of every kind of wrong model identity, of another chunking, and of a manifest that is not the
  published one; every build-side call refused as read-only; an install and a
  compaction under an open session, which follows them; re-anchoring, which
  needs the column and the set's library version; a damaged block found by
  verifying and refused by opening after; and coverage, the same from the
  column and recomputed. The FFI suite installs and opens a set across the
  bridge too, so CI builds `build_semantic_artifact` beside the library, and
  the package gains `crypto` as a dev dependency for the stub model's checksum.
- **INT8 vectors depend on the CPU's INT8 kernels**, documented: ARM (KleidiAI)
  and x86 (MLAS) land about cosine 0.999 apart, the same order as INT8 against
  fp32, so a library built on x86 and queried on an ARM Mac meets at about
  0.999. The sidecar records this as accepted, and as a measurement still to be
  made on a weak PC.

### Changed

- **The sidecar is compiled into every build**, declared with
  `default-features = false`, because a line's chunk key has to be the same
  function of its text in every build: the release index is built once and
  ships to every platform. `semantic-integration` and the backend features
  still gate the code that talks to the sidecar. The inference crates (ort,
  tokenizers, libloading) still come only with `semantic-onnx`, and only on
  desktop targets, so an Android or iOS build pulls in none of them. Cargo.lock
  gains no crate.
- **The plugin calls the sidecar's `HybridCoordinator` itself**, not the
  `OtzariaHybridEngine` wrapper, which turned every error into a string, so the
  typed error reaches the classification; nothing else the wrapper did is lost.
- **`build_semantic_artifact` writes a vector set's base package**: the segment
  and its release manifest, whose SHA-256 it prints for the release to publish,
  and installs it into a set with `--install`. It takes the library version
  (`db_version`) and `--release-tag`, writes nothing into the lexical index,
  and writes the sidecar's default codec (`i8-sym-vec`). `pack_semantic_artifact`,
  which assembled the shards of the old artifact, says it is retired and exits
  with status 2: the sidecar's `assemble` builds a release from a plan and its
  vectors.
- **The calls that build vectors on the device are documented as development
  and testing scaffolding**: `configureSemantic`, `semanticIndexBooks`,
  `semanticIndexDiff`, `removeSemanticBooks` and `resetSemanticIndex`, in their
  doc comments (and so in the Dart docs), the README and API_DOCUMENTATION. They
  are kept, since application code references them, and not `#[deprecated]`:
  flutter_rust_bridge's codegen does not carry the attribute to Dart, so the
  application would see nothing, while every Rust call site would warn, the
  generated wrappers included.
- **`build_semantic_artifact` documents an ONNX graph as `--model-file`**, and
  its no-backend message names `semantic-onnx` and `semantic-mock`. The bins
  that need no model no longer say "GGUF".
- **The tests that drive the stand-in open it on a stub ONNX package**, where
  they used a stub GGUF: the sidecar's `write_stub_onnx_package` in Rust, and a
  Dart copy of it in the FFI suites, which compute its package checksum as the
  sidecar defines it. The Rust ones run with `semantic-mock` and without
  `semantic-onnx`, which would take the stub ahead of the stand-in and fail to
  load it. Before, the binary's test was gated off `semantic-real` and the
  corpus adapter's test had no gate at all, so llama.cpp could have claimed
  their stub GGUF.
- **CI checks, lints and runs the tests with `--features semantic-onnx`**, the
  one job that runs the integration's tests without the stand-in, and exactly
  what `semantic` turns on. The real-backend job compiles `semantic` on Linux,
  macOS and Windows; nothing in CI compiles llama.cpp any more.
- **CI runs the real-model tests on Linux, macOS and Windows.** The new "Real
  ONNX model" job fetches the INT8 graph and its `tokenizer.json` from the
  project's private Hugging Face mirror with the `OTZARIA_HF_TOKEN` secret and
  checks their SHA-256, fetches Microsoft's ONNX Runtime 1.28.0 as the
  sidecar's CI does, takes the model's identity files from the sidecar at the
  pinned revision, and runs `rust/tests/semantic_onnx_model.rs` under
  `OTZARIA_REQUIRE_ONNX_MODEL`. Without the secret it fails rather than skips;
  it does not run for pull requests from forks, which get no secrets.

### Fixed

- **Every semantic line a search returns holds its vector's text by the whole
  key, grouped siblings included.** The resolver found a vector's lines by
  their `chunkKey` column, a key's first 64 bits, and only a page's primaries
  were held to the full 128 before they were shown; a group's siblings were
  hydrated as they came, so a line whose text was replaced while its column
  value was kept could cross the bridge as the sibling of the line that does
  hold the text. The resolver now checks every line it returns against the key
  recomputed from the line's text and its neighbours' — at a hint, in a book
  searched for a moved line, among a book's repeats, and in the pass over the
  whole column — so a line that fails reaches neither fusion nor grouping nor a
  page, and is counted in `fallbackReason` as primaries were. A line long
  enough to stand alone is checked at one document rather than five: on a
  synthetic set of 1,050,000 lines an unfiltered semantic-only search takes
  8.4 ms where it took 10.9, since the page no longer checks its primaries
  separately. Where the pass over the whole column found each value it looked
  for — or that it found one nowhere — is kept for the index's generation (the
  last 1,024 values, up to 1,024 places each), so a vector whose only line is
  one a stale column holds costs one pass a generation, not one a search; and
  the lines the pass finds share the cap as a hit's records do, one line of
  each book first.
- **A line no vector resolved is hydrated by its book and its id.** A grouped
  sibling that only lexical search found, and a line of a session built on the
  device, were looked up by id alone, and two books can share ids when an index
  is updated book by book: such a sibling came back as another book's line.
- **A passage a book holds in two places is a result for each.** A vector set
  records a text once per book, at its first line, and the resolver stopped at
  the first line that held it, so the second section's copy never came back,
  even ungrouped. A hit now resolves in two passes, up to the sidecar's 32
  lines a hit: first one line for each record — each book the set records the
  text in, at its hint or where the book holds it now — and then, in that
  order, each of those books' other lines of the same text, found by the
  book's `chunkKey` values (its `lineHash` in a version 4 index), while the cap
  lasts. So a book that repeats a passage forty times takes 31 of the 32 and
  never the line of another book that holds it once. Without grouping each is
  a result; grouped by section they head their sections' groups, and grouped
  by text they are one group. Pagination is unchanged by it.
- **A filtered search finds a text that moved, or was copied, into a book it
  admits.** The scan reads only the vectors with a record in an admitted book,
  and a set's records are where its texts were when it was built, so under the
  filter of the category a text had moved into, it was not there until the
  vectors were updated. A filtered search of an opened set is now planned
  first, when the index has the `chunkKey` column. An admitted book's live
  texts that no live record of the set places in it are its arrivals, looked
  for in it and held to their whole key. The vector of an arrival that no
  admitted book's live records reach is named to the sidecar
  (`CandidateResolver::unreached`), which weighs it at its own score beside
  the scan of the admitted books and never in place of one of their hits: the
  scan is not widened, so the admitted books' results are exactly what they
  would be had nothing moved. Such a vector is resolved in the admitted books
  the text arrived in, never in the books its records name. When nothing
  moved the plan is the admitted books alone, as before. Liveness is the
  set's own: a record counts when a scan reaches a live slot through it, and
  a vector when its slot is live. A book's arrivals are kept for the set's
  generation under its postings (the segments that hold its lines, and their
  deletions) and its text hash, so a plan after a commit that left a book
  alone reads none of its lines; a plan per filter is kept for the index's
  generation. A filtered search that cannot be planned, because the index
  could not be read for it, fails its semantic half alone: the lexical
  results are served with the reason, `fallbackKind` `queryFailed`. A version
  4 index, which has no column (the published v30 library index is one),
  scans the admitted books alone as before, so a text moved or copied into an
  admitted book is not found under the filter there until the index is
  rebuilt as version 5; an unfiltered search finds a copied text where the
  set records it, until the vectors are updated. Measured on 1,050,000 lines in 1,501 books with
  1,030,500 vectors (Apple M4, medians of five runs, against the widening this
  replaces): a filter to one six-line book with a line copied into its
  category from a 700-line book of another one keeps all six of its lines,
  where the widened scan lost one, and finds the copy; searches under a
  category of 50 books take 5.0 ms (6.4), under that one book 0.2 ms (0.4),
  under a category after 50 texts were copied into one of its books 1.1 to
  1.3 ms (1.0 to 1.1), and unfiltered 12.5 ms (14.2). The first search under
  every book after texts moved takes 24 ms (17); the process holds 396 MB
  after planning (394), the mapped index and vectors nearly all of it.
- **A version installed already, published again, and a verification an
  install overtook, are not damage.** The sidecar (04a2cc9) refuses a release
  whose segment is a version the set has installed, published again with
  other bytes, while a generation that opens serves the installed bytes:
  `SegmentIdTaken`, which is `artifactIncompatible` with `field` `segment_id`
  here, not `artifactCorrupt` — the set and the release are sound, the set
  keeps what it serves, and downloading the release again would be refused
  the same way. A scrub that read bytes an install has since replaced now
  reports it and condemns nothing, and `verifySemanticVectors` throws
  `vectorsBusy` for it, to verify again, rather than `artifactCorrupt`.
  Installing a release again now repairs a damaged or condemned set; on
  Windows close the session first, since a mapped segment cannot be replaced.
  A cancelled verification records nothing, and a download in `incoming/` is
  moved into the set only when the install succeeds (on Windows a read-only
  one is copied).
- **One install or compaction of a vector set at a time, and each expansion
  its own.** A compressed release was expanded to `incoming/<download name>`
  before the set was locked, so two installs of one set from downloads of the
  same name wrote one file, and a valid release was refused as corrupt because
  the other install was still writing over it. An install or a compaction now
  holds the set for the process before it reads anything, and a second one is
  refused at once as `vectorsBusy` with `field` `vectors_dir`, nothing read or
  changed; one in another process meets it at the sidecar's lock, whose
  refusal was `internal` and is now the same `vectorsBusy`. That is a kind of
  its own, `SemanticErrorKind.vectorsBusy`, added last to the enum: try again
  once the other has finished. It is not `sessionConflict`, whose answer —
  `disableSemantic` — would close a session that is serving, for an install
  that only has to wait. A compressed
  segment is expanded into a file of the install's own, beside a lock file
  the install holds from before the segment is written until it returns, so
  an install in another process never takes it for abandoned — not while it
  is written, and not once it is closed and waiting for the sidecar to take
  it. Both are removed when the install returns, installed or not; the next
  install of a set removes what a stopped process left there (a lock file
  nothing holds), and an expansion file with no lock file once it is an hour
  old.

## 0.8.7 – 2026-09-29

### Fixed

- **The result snippet keeps punctuation glued to its first and last word.**
  Tantivy's `SnippetGenerator` cuts the fragment at the offsets of the first and
  last tokens, so glued marks such as `{פ}` or `׃` were chopped off — בראשית ב, ג
  ended in "{פ" instead of "{פ}", and a later fragment of a long line showed
  "שבת}" for `{שבת}`. The fragment is now extended to the nearest space on each
  side, as long as that run does not reach another word.
- **That punctuation is taken from the fragment actually chosen.** It was located
  with `text.find`, so in a long line where the fragment's text also appears
  earlier, the punctuation came from the earlier occurrence. The position now
  comes from `Snippet::fragment_range`, the exact range tantivy cut.
- **tantivy 0.26.2.** In 0.26.1 a union (OR) filtered by a cheap filter could
  return a document that matches none of its branches (quickwit-oss/tantivy#3086)
  — the shape of a phrase search with acronym alternatives and a category
  filter. A regression test covers it. The index format is unchanged; no
  reindex is needed.

### Changed

- **tantivy now comes from the Otzaria fork** (`Otzaria/tantivy@otzaria-0.26`):
  0.26.2 plus quickwit-oss/tantivy#3134 (`Snippet::fragment_range`), #3135
  (`RegexPhraseQuery` compiles its regexes once rather than per segment), #3136
  (`FuzzyTermQuery::automaton`) and #3137 (`RegexPhraseQuery::regexes`). A TODO
  marks the return to crates.io once a release includes all four.
- **A phrase search compiles each word pattern once.** The engine compiled every
  pattern twice more before tantivy — once to check that it compiles, once to
  count expansions per segment — and `GapVerifiedPhraseQuery` a further time.
  All of them now use the DFA the query keeps.
- **Fuzzy highlight terms use tantivy's own automaton.** The engine kept a copy of
  tantivy's private `DfaWrapper` and pinned `levenshtein_automata` to tantivy's
  version so highlight terms would match the query's expansion. Automatons are
  now built through `FuzzyTermQuery::automaton`, with the same configuration and
  cache as the query, and the direct `levenshtein_automata` dependency is gone.
- **CI: the Windows compaction and merge-policy tests run serially**
  (`--test-threads=1`), since they all create and compact temporary indexes and
  Windows cannot unlink files another test still holds.

## 0.8.6 – 2026-09-19

### Fixed

- **An approximate (fuzzy) search no longer loses the typed word from the
  highlight of an opened book.** The per-word display pattern keeps a character
  budget, and it was filled longest-first; with the lexical dictionary a word
  has thousands of variants, so the short word the user actually typed was the
  first to be cut. "מי שטרח בערב שבת יאכל בשבת" at distance 2 painted nothing in
  a line that contained it verbatim. The budget now keeps the word itself first,
  then the terms closest to it in length; only the kept terms are ordered
  longest-first for the alternation.
- **The result snippet of a phrase search is taken around the phrase.** Tantivy
  picks the fragment by term density, and in fuzzy mode the short variants
  (אם, את, אות) pulled it to where the phrase was cut off. The phrase filter
  then found no complete occurrence and fell back to painting every variant.
  When the chosen fragment holds no complete occurrence, the filter now runs on
  the whole line and the snippet is cut around its first occurrence, within the
  same `max_chars` budget and on word boundaries. Lines without an occurrence
  keep the previous behaviour. Applies to the semantic path too.

## 0.8.5 – 2026-09-15

### Fixed

- **`optimize` now groups segments into size levels instead of measuring every
  segment against the single smallest one in the index.** One tiny outlier — the
  short final flush a from-scratch build always leaves behind — was enough to
  veto the merging of every other segment with its own kind, so a full
  SeforimLibrary build ended at 188 segments where an identical input had
  previously reached 8. Both indexes are correct, but the second reads 188 term
  dictionaries per query, and nothing reported the difference. Segments are now
  walked as levels: a level begins at the smallest segment not yet placed and
  holds everything within the size ratio of it.

  The two callers no longer share one rule, because they do not want the same
  thing. The background merge policy, which tantivy consults on every commit
  while someone is indexing, still merges the smallest level and nothing else —
  byte for byte the behaviour 0.8.4 gave it — so a user's library reaching ten
  segments never triggers a large rewrite in a background thread.
  `optimize`, which is asked for explicitly, also takes the first level above the
  smallest that alone holds more segments than the index is meant to end with;
  that is what compacts a from-scratch build. `optimize` therefore still settles
  above its target on some shapes — a singleton under a level of eight or fewer
  is left alone — and that stays deliberate: the alternative is rewriting healthy
  large segments to retire one or two, which is what its documented soft target
  means.

## 0.8.4 – 2026-09-13

### Added

- **A sharded semantic build: `export_semantic_plan` and `pack_semantic_artifact`.**
  The library's vectors are not produced on the machine that holds the library,
  so the build splits in two. `export_semantic_plan` applies the embedding
  recipe to the Tantivy index and writes `plan.jsonl` — the finished embedding
  text and both digests, one record per line that gets a vector — plus its
  manifest and the corpus identity to pack against. `pack_semantic_artifact`
  takes the vectors back from wherever they were embedded and joins them to the
  live index, verifying there what nothing upstream can: that every
  `source_line_sha256` matches the line the index holds, and that the id set is
  exactly the one the recipe embeds. Neither binary links an inference backend,
  so both build without llama.cpp; both still require `semantic-integration`.

### Changed

- **`optimize` compacts instead of collapsing.** It no longer merges every
  segment into one: it targets eight segments by merging similarly sized ones,
  selected by their on-disk bytes. The target is soft when merging would rewrite
  a segment more than four times the size of the smallest one. Segments with
  more than 30% deleted docs are compacted independently. This keeps a small
  incremental addition from rewriting an unrelated large segment. The normal
  writer uses the same size rule for background merges, so a commit cannot
  undo this protection before `optimize` runs.
- **`optimize` garbage-collects merged-away segment files.** GC runs again after
  the reader reload releases them, so the index directory shrinks instead of
  growing (tantivy’s post-merge GC saw them as still pinned by the old searcher).
- **The corpus id is computed in parallel.** Reading six million stored
  documents is the cost of opening the corpus, and it is embarrassingly
  parallel; the hash is not, so lines are read and serialized by a thread pool
  and fed to the digest in order. The resulting `corpus_id` is bit-identical to
  the single-threaded one. The plan cache became a `Mutex`, which makes
  `TantivyCorpus` `Sync`.

## 0.8.3 – 2026-09-07 – flutter_rust_bridge 2.13.0

### Changed

- **flutter_rust_bridge עולה ל‑2.13.0.** ה‑crate וה‑runtime מוצמדים לאותה
  גרסה מדויקת, ולכן העדכון הוא בנייה מחדש של הבינדינגים ולא שינוי חוזה: אף
  חתימה בצד ה‑Dart לא זזה, וצרכני החבילה אינם צריכים התאמה. בקוד ה‑Rust
  המחולל `Ok(...)` הוכשר במלואו ל‑`std::result::Result::Ok(...)`, ונוסף
  `mismatched_lifetime_syntaxes` לרשימת ה‑allow — שניהם מהתבנית של 2.13.0.

## 0.8.2 – 2026-09-03 – ה‑C++ runtime נארז עם הבינאריים המוקדמים באנדרואיד

> גרסה 0.8.1 נשאה את אותו שינוי ובוטלה לפני פרסום: שמות הנכסים שלה לא היו
> ניתנים לכתובת, כך שכל יעדי אנדרואיד היו נושרים ממסלול הבינאריים המוכנים.

### Fixed

- **ה־C++ runtime נארז עם הבינאריים המוקדמים באנדרואיד.**
  `libc++_shared.so` עובר stripping, נחתם ומופץ לכל ABI לצד
  `libsearch_engine.so`, ומועתק לאותו `jniLibs` גם בבנייה מקומית. כך אפליקציות
  צרכניות אינן תלויות ב־NDK מקומי במסלול הבינארי המוכן, וה־runtime תמיד מגיע
  מאותו NDK שבנה את מנוע החיפוש.

- **שמות הנכסים ב־release מנורמלים כפי שגיטהאב שומר אותם.**
  גיטהאב מחליף כל רצף תווים שאינו `[A-Za-z0-9._-]` בנקודה אחת, כך שנכס שהועלה
  בשם `aarch64-linux-android_libc++_shared.so` נשמר כ־`..._libc._shared.so`.
  ההעלאה, ההורדה והאימות פנו כולם לשם המקורי וקיבלו 404; מכיוון שיעד מסופק רק
  עם סט נכסים שלם, ארבעת יעדי אנדרואיד נשרו כליל — גם `libsearch_engine.so`
  התקין שלהם לא נוצל — וכל בנייה צרכנית חזרה לקמפל את ה־crate מקומית. שם הקובץ
  על המכשיר לא נגזר מכאן אלא מ־`Artifact.finalFileName`, ולכן נשאר
  `libc++_shared.so`.

- **`verify-binaries` מחזיר קוד יציאה שאינו אפס כשנכס חסר או פסול.**
  קודם הוא רק הדפיס `MISSING`, כך ששלב האימות ב־workflow נשאר ירוק בעוד לצרכנים
  לא היה מה להוריד — שער שלא שמר על כלום.

### Changed

- **רצפת ה־SDK עלתה ל־`>=3.10.0` ו־`ffigen` ל־`^21.0.0`.**
  פותר את התלות בגרסאות שאינן תואמות עוד לכלים המעודכנים. פרויקטים על SDK מוקדם
  יותר ימשיכו להיפתר ל־0.8.0.

## 0.8.0 – 2026-09-02 – פתיחת אינדקס ובדיקת תאימות עברו לאסינכרוני (שובר תאימות)

> גרסה 0.7.8 נשאה את אותו שינוי ובוטלה: מספר תיקון (patch) נכנס לטווח של
> `^0.7.7`, כך ששינוי שובר תאימות היה מגיע מעצמו לצרכנים קיימים.

### Breaking

- **`SearchEngine.new` ו-`check_index_compatibility` אינן עוד `#[frb(sync)]`.**
  שתיהן עושות I/O כבד — הראשונה פותחת `MmapDirectory` וקוראת את ה-footer של כל
  segment באינדקס, השנייה קוראת קובץ מטא מהדיסק — ו-`#[frb(sync)]` גרם
  ל-flutter_rust_bridge לייצר קריאה חוסמת שרצה על ה-isolate הראשי של הצרכן.
  באנדרואיד זה התבטא ב-ANR: `Executor::execute_sync` על ה-thread הראשי בתוך
  `Footer::extract_footer`. כעת שתיהן רצות על ה-thread pool של FRB.

  **מה משתנה אצל הצרכנים** — שני שינויי API בקוד המחולל:

  | לפני | אחרי |
  | --- | --- |
  | `SearchEngine(path: p)` | `await SearchEngine.newInstance(path: p)` |
  | `checkIndexCompatibility(path: p)` | `await checkIndexCompatibility(path: p)` |

  ה-constructor הסינכרוני `factory SearchEngine({required String path})` הוסר;
  במקומו נוצר `static Future<SearchEngine> newInstance({required String path})`,
  ו-`checkIndexCompatibility` מחזירה כעת `Future<IndexCompatibility>`.

  שאר ה-`#[frb(sync)]` בחבילה נשארו כפי שהיו — הן חישוב טהור ומהיר
  (`sanitize_query`, `normalize_text_for_indexing`, `generate_highlight_pattern`,
  `split_query_words`, `compute_content_fingerprint`, טעינת/בדיקת מילונים),
  ונמצאות במסלולים חמים שבהם `await` היה עולה יותר מהחישוב עצמו.


## 0.7.7 – 2026-09-02 – יעדי פריסה של Apple מקובעים בבינאריים המוקדמים

### Fixed

- **הבינאריים המוקדמים ל-Apple נבנים כעת ליעד פריסה מוצהר.** בלי קיבוע,
  `cc-rs` הידר את תלויות ה-C/C++‏ (`zstd`, `sqlite3`, `llama.cpp`/`ggml`,
  `dart_api_dl`) לברירת המחדל של ה-SDK שעל שרת הבנייה — `minos 15.5` —
  בעוד `rustc` הידר את הקוד שלו ל-11.0. שני המינימומים נכנסו לאותה
  `libsearch_engine.a`, וכל צרכן קיבל אזהרת linker אחת לכל קובץ אובייקט
  (`object file ... was built for newer 'macOS' version (15.5) than being
  linked`), ובפועל הצהיר תמיכה בגרסת macOS שאינו יכול לספק. ה-workflow
  מקבע כעת `MACOSX_DEPLOYMENT_TARGET=12.0` ליד הפין הקיים של iOS.
- **הפין של iOS הועלה מ-13.0 ל-15.0**, הרצפה הנוכחית של Flutter.
- **ה-podspecs הועלו בהתאם** — `:osx, '12.0'` ו-`:ios, '15.0'` במקום 10.11
  ו-11.0. אפליקציות ה-example יושרו איתם כדי ש-`pod install` שם ימשיך
  להיפתר.

### Changed

- **גרסת ה-crate ב-`rust/Cargo.toml` מיושרת לגרסת החבילה** (הייתה תקועה על
  0.1.0). ה-hash שמזהה בינארי מוקדם נגזר מתוכן `rust/` בלבד, כך ששחרור
  שאינו נוגע שם חוזר על בינארי קיים במקום להיבנות מחדש.

## 0.7.6 – 2026-09-01 – מנוע degrade לפרזות רחבות במקום שגיאה + תקרות וריאציות גבוהות יותר

### Performance

- **מימוש עמדות הפרזה במסלול ה-degrade ממוקבל.** רק כאשר מסלול
  `RegexPhraseQuery` אכן אינו זמין, סריקות ה-FST הבלתי-תלויות של עמדות
  הפרזה רצות במקביל על ה-pool של rayon, עם איחוד מילים חוזרות לסריקה אחת.
  המסלול המדויק נשאר מנוקד ומדורג בידי Tantivy, ואינו מממש מראש קבוצות
  מונחים גלובליות.

### Changed

- **תקרות הווריאציות פר-מילה הועלו** — מהלך שהפך בטוח רק בזכות תיקון
  ה-degrade שלמטה (קודם כל וריאציה נוספת קירבה שאילתות רב-מיליות לצוק
  השגיאה של תקרת ההרחבות):
  - `MAX_SPELLING_BRANCHES` (וריאנטי כתיב מלא/חסר פר-תבנית): ‏16 → **32**.
    משותף לבוני ההדגשה, כך שחיפוש והדגשה ממשיכים להתפרס זהה.
  - מכסות הכתיב בבונים הדקדוקיים אוחדו ל-`MAX_GRAMMATICAL_SPELLING_BRANCHES
    = 16` (היו 4 לקידומות+סיומות דקדוקיות יחד, 6 לסיומות, 10 לקידומות
    ולענף הארמי) — החיתוך ל-4 בצירוף הנפוץ היה אובדן הריקול השקוף הגדול
    ביותר.
  - `MAX_NORMAL_VARIATIONS` (סך ענפים פר-מילה בפרזה, בלי שגיאות כתיב):
    ‏48 → **96**.
  - שתי רשתות ביטחון חדשות מלוות את ההעלאה: (1) מסלול הפרזה המדויק
    **מקמפל מראש** את התבנית המאוחדת של כל מילה ונופל למנוע ה-degrade אם
    היא חורגת ממגבלות ה-DFA (במקום שגיאת חיפוש); (2) חריגת תקציב-התווים של
    התבנית המאוחדת חותכת ענפים מהסוף לפי סדר עדיפויות במקום ליפול לליטרל
    עירום שמאבד את כל האפשרויות.

### Fixed

- **שאילתות פרזה רחבות כבר לא נכשלות על תקרת ההרחבות — degrade במקום שגיאה.**
  מסלול "מרווח בין מילים · כל המילים" (וגם פרזה מדויקת מנוקדת) רץ דרך
  `RegexPhraseQuery` של tantivy, שסופר את מונחי-האינדקס התואמים **במצטבר על
  כל עמדות המילים** ומפיל את השאילתה עם
  `InvalidArgument("Phrase query exceeded max expansions")` בחריגה מ-8,192.
  שאילתה סבירה כמו 4 מילים עם קידומות+סיומות+כתיב מלא/חסר (שממופות לתבניות
  חלון כמו `.{0,3}מידה.{0,3}`) חצתה את התקרה בקלות — בעוד שאותה מילה עם
  אותן אפשרויות עבדה מצוין לבדה (מסלול המילה הבודדת מוותר בעדינות).

  התיקון (`phrase_query_with_degrade`): תחילה נספרות ההרחבות באותה סמנטיקה
  של Tantivy — במצטבר על פני עמדות המילים, אך **בנפרד בכל סגמנט**. רק אם
  סגמנט חורג, כל עמדת-מילה ממומשת לקבוצת מונחים תחת תקציבי איסוף
  **פר-עמדה** (`PHRASE_POSITION_MAX_EXPANSIONS = 8,192` מונחים, תקציב
  postings של 20M).

  - **בתוך התקרה בכל סגמנט** — `RegexPhraseQuery` ההיסטורי (התנהגות וניקוד
    זהים, מובטח שלא ייפול על התקרה);
  - **מעבר לה** — `TermListPhraseQuery` חדש (`gap_phrase.rs`): הצירוף מונע
    ע"י AND של `TermSetQuery` פר-עמדה, ו-`GapVerifiedScorer` הקיים מאמת סדר
    ומרווחים פר-זוג מול ה-postings הפוזיציוניים. אין DFA מאוחד ואין תקרת
    הרחבות ליפול עליה: כשרק התקרה *המצטברת* נחצתה מוגשות **תוצאות מלאות**
    (וללא דגל truncated); כשעמדה בודדת חורגת מתקציביה — האיסוף נחתך מהסוף
    לפי סדר העדיפויות של הענפים ו-`truncated: true` עולה ל-UI, כמו במסלול
    המילה הבודדת. `maxExpansions` של ה-API המחרוזתי הפך בהתאם ממחולל-שגיאה
    למתג בחירת מנוע (עודכן ב-API_DOCUMENTATION).

- **חלונית התוצאות מוצאת גרשיים מודפסים כמו ההדגשה.** חיפוש `רשי` הדגיש את
  `רש״י` בגוף הספר (`charwise_display_pattern` מזריק `OPTIONAL_QUOTES` בין
  אותיות) אך `literal_charwise_pattern` — המסלול שמאחורי חלונית התוצאות —
  לא, והחזיר "אין תוצאות" לאותה שאילתה. שני המסלולים מזריקים עכשיו את אותם
  גרשיים אופציונליים בין כל שתי אותיות עבריות רצופות (Otzaria/otzaria#1054).

## 0.7.5 – 2026-08-25 – מצב אינדוקס חסכוני ותמיכת Windows on ARM

### Added

- **`setEconomyIndexing(enabled)` — מצב אינדוקס חסכוני** (Otzaria/otzaria#834).
  מכווץ את תקציב הזיכרון של ה-writer לטביעת הרגל של מובייל (50MB), מה שמגביל
  את tantivy ל-3 threads של אינדוקס במקום 8 — המחשב נשאר שמיש בזמן בנייה
  ארוכה. `false` מחזיר את ברירת המחדל של הפלטפורמה. התקציב נקבע ביצירת
  ה-writer, ולכן writer חי מוחלף תוך כדי: המסמכים הממתינים עוברים commit
  קודם ודבר אינו אובד; מותר להחליף מצב באמצע אינדוקס. כבוי כברירת מחדל.

- **תמיכת Windows on ARM (`windows-arm64`).** היעד `aarch64-pc-windows-msvc`
  נוסף לטבלת המטרות של cargokit, וה-workflow של הבינאריים המוקדמים בונה
  אותו נייטיבית על רץ `windows-11-arm` — כך שבניית האפליקציה על מכונת ARM
  מורידה בינארי מוכן במקום לקמפל את llama.cpp מקומית.

- **`CARGOKIT_TEMP_DIR` בסביבה עוקף את נתיב ה-scratch של cargokit.** ברירת
  המחדל (בתוך עץ ה-build של Flutter) עמוקה מספיק לחצות את MAX_PATH בבניית
  crates גדולים; משתנה הסביבה מאפשר נתיב קצר כמו `C:\ck` בבנייה מקומית.

## 0.7.4 – 2026-08-23 – הצמדת flutter_rust_bridge ל-2.12.0 + תגי שבירה כרווח

### Fixed

- **אילוץ ה-Dart של `flutter_rust_bridge` מוצמד ל-`2.12.0`.** צד ה-Rust
  מוצמד `=2.12.0` והקוד המחולל הוא codegen 2.12.0, ו-`RustLib.init` דורש
  התאמה מדויקת בין ה-runtime לקוד המחולל. האילוץ הפתוח `^2.12.0` אפשר ל-pub
  לפתור את ה-runtime של 2.13.0 (פורסם 23/8) — וכל גרסה אחרת, כולל patch
  עתידי של 2.12, תיכשל באתחול. לכן כל צרכן שעושה `pub get` טרי מקבל בדיוק
  את ה-runtime התואם למנוע.

- **תגי שבירה (`<br>`, `</p>`, כותרות, תאי טבלה…) הופכים לרווח באינדוקס**
  (Otzaria/otzaria#949). `strip_html_for_indexing` מחק כל תג בלי רווח, ולכן
  שתי מילים משני צדי מעבר שורה (`המורים<br>כי`) נטמעו כטוקן אחד: הן הוצגו
  דבוקות בתוצאות החיפוש, וחיפוש של כל אחת מהן החטיא את ההופעה. תגי inline
  (`<b>`, `<span>`…) נשארים מחיקה נטו — מילה שפוצלה באמצע על ידי עיצוב היא
  עדיין מילה אחת. תבניות ההדגשה (`generate_highlight_pattern`,
  `generate_literal_highlight_pattern`) מיישרות קו: תג שבירה בין מילים נחשב
  מפריד, ותג inline נשאר שקוף. התבנית המשולבת כוללת כעת lookahead שלילי
  (נתמך ב-RegExp של Dart שמקמפל אותה).

  שינוי הנרמול משנה את הטוקנים ואת הטקסט השמור, אבל **אין העלאת גרסת
  סכימה נוספת**: v4 (שהועלתה ב-0.7.3) עדיין לא הגיעה לציבור, כך שהבנייה
  מחדש שהיא כופה מכסה גם את השינוי הזה.

## 0.7.3 – 2026-08-23 – האינדקס הלקסיקלי כקורפוס של בניית האינדקס הסמנטי (S4b)

### Added

- **עמודת `textHash` + `get_book_text_fingerprints` — חתימת טקסט-בלבד לצד
  החתימה הקנונית** (Otzaria#828). החתימה הקנונית (`contentHash`) כוללת את
  הסדר הקטלוגי, ולכן הוספת ספר אחד לספרייה פוסלת אותה לכל הספרים שאחריו —
  אימות דריפט תוכן שנשען עליה חסם פתיחת תוצאות תקינות. מסלולי הספר השלם
  (`add_text_book`/`add_text_book_bytes`) חותמים כעת גם
  `compute_content_fingerprint` על הטקסט הגולמי בעמודה נפרדת, ו-
  `get_book_text_fingerprints` קורא אותה עמודתית (0 = לא ניתן לאימות, כמו
  ב-`get_book_fingerprints`). ל-`DocumentInput` נוסף `text_hash` אופציונלי
  למסלולי ה-batch. שינוי סכימה: `INDEX_SCHEMA_VERSION` הועלה ל-4 — אינדקסים
  קיימים ייבנו מחדש.

  **`get_book_text_fingerprint(file_path)`** — צורת ספר-בודד: `TermQuery`
  על `filePath` וקריאה עמודתית של `textHash` מהמסמכים החיים של אותו ספר
  בלבד, במקום `AllQuery` על כל האינדקס. בדיקת דריפט לספר אחד אינה משלמת
  O(כל המסמכים): על אינדקס של 84,000 מסמכים ב-2,000 ספרים נמדדו 293ms
  לסריקה המלאה מול 20.7µs לקריאה ממוקדת. `0` = לא ניתן לאימות, כמו בצורת
  המפה (ספר שאינו באינדקס, PDF, או מסמכים חלוקים).

  **תיאום שחרור (חובה):** גרסת הסכימה נכנסת ל-`CorpusIdentity`, ולכן
  artifacts סמנטיים שנבנו על v3 יידחו תחת v4 גם כשתוכן המסמכים לא השתנה.
  לפני פרסום הגרסה יש לבנות ולפרסם artifacts סמנטיים של v4 (או לוודא
  מסלול rebuild/fallback תקין בכל הפלטפורמות) — אחרת החיפוש הסמנטי לא
  יהיה זמין מיד אחרי העדכון.

- **`semantic_corpus::TantivyCorpus` — מימוש `CorpusIndex` ו-`CorpusBooks` מעל
  אינדקס Tantivy חי.** ה-crate הסמנטי אינו מקשר Tantivy ואסור שיקשר — האינדקס,
  הסכמה וסכמת ה-IDs חיים כאן — ולכן ה-builder שלו מקבל את הקורפוס דרך פורט.
  המימוש הזה מחליף את התמלול ל-JSONL ששימש עד עכשיו כתחליף.

  **snapshot אחד לכל הבנייה.** בנייה קוראת את הקורפוס שלוש פעמים לפחות: לגזירת
  קבוצת השורות שהמתכון מטמיע, לגזירת הטקסט להטמעה, ולצירוף כל וקטור מוגמר
  למטא-דאטה שלו. `TantivyCorpus` מחזיק `Searcher` **אחד** לכל חייו ואינו טוען
  מחדש; שלוש קריאות שינחתו על שלושה commits שונים היו מערבבות תכנית מאחד, טקסט
  הקשר משני ומטא-דאטה משלישי. `source_line_sha256` של ה-packer לא היה תופס את
  זה: הוא משווה את שורת **העוגן**, ושכן שהשתנה בין שתי קריאות משנה את מה שהוטמע
  בעוד כל ה-digests מסכימים.

  **בדיקת שלמות מול משהו שהסריקה לא ייצרה.** קבוצת הכיסוי נגזרת מ-`book_keys()`
  ומ-`book_line_ids()`, וה-packer משווה את הווקטורים לאותה קבוצה — ולכן ספר
  שהמימוש הזה היה משמיט בטעות היה נעלם משני הצדדים בבת אחת. `open` משווה את
  הסריקה ל-`Searcher::num_docs`, ספירה ש-tantivy גוזר ממטא-דאטה של הסגמנטים
  ומ-bitset המחיקות, בלי שום עזרה מהסריקה.

  **שלושה שדות נקראים עמודתית ולא מהמסמך.** `sectionId`, `lineHash`
  ו-`contentHash` הם FAST ו**אינם מאוחסנים**, ולכן קריאה שלהם מהמסמך המאוחזר —
  הדרך המתבקשת לכתוב את זה — הייתה מחזירה אפס לכל שורה, בשקט, וכל רשומה בכל
  ארטיפקט הייתה נושאת אפס שה-packer מאמת מול אותו אפס.

  **`corpus_id` מכסה כל שדה של כל שורה.** SHA-256 מעל כל מסמך חי בסדר `line_id`
  עולה, עם המזהה ועם ה-JSON הקנוני של ה-`CorpusLine` כולו. במכשיר אין join מול
  Tantivy — ההתקנה משווה `CorpusIdentity` ואז קוראת וקטורים — ולכן כל שדה שמשפיע
  על מה שהוטמע, על סינון או על קיבוץ חייב להזיז את הערך: `section_id` קובע מה
  שורה קצרה מטמיעה (ההקשר נאסף רק מאותו section) והוא גם מפתח הקיבוץ
  `SameSection`, `line_hash` הוא מפתח `IdenticalText`, ו-`facets` ו-`is_pdf` הם
  מה שה-sidecar מסנן לפיו לפני כל hydration. המחיר: שינוי כותרת מבטל את הווקטורים
  של אותו ספר. זו ברירת המחדל הנכונה — repack של מטא-דאטה בלבד הוא אופטימיזציה
  עתידית, וקבלה שקטה של מטא-דאטה ישן אינה.

  **קריאה קפדנית, בלי ברירות מחדל ובדיוק ערך אחד.** שדות columnar ב-tantivy הם
  רב-ערכיים מתחת, יהיה מה שהסכמה תרמז — ולכן `first(doc)` בודק „לפחות אחד" ולא
  „בדיוק אחד". מסמך עם שני `sectionId` היה נותן לאחסון לבחור מאיזה section שורה
  קצרה שואלת הקשר, ולאיזו קבוצה האפליקציה מקבצת אותה. `id` נקרא גם מהעמודה
  (בזמן ה-enumeration) וגם מהשדה המאוחסן, ושתי הקריאות חייבות להסכים: מסמך ששתי
  ההעתקות שלו נבדלות היה מתויק תחת מזהה אחד ומתאר אחר. טקסט חסר אינו `""`, עמודה חסרה אינה `0`
  ו-`isPdf` חסר אינו `false`: כל אחד מהם היה ערך שהארטיפקט נושא, שה-packer מאמת
  מול אותה בדיה, ושהאפליקציה מסננת ומקבצת לפיו. `contentHash = 0` הוא תשובה
  אמיתית ל-PDF, ולכן „לעמודה אין ערך למסמך הזה" ו„הערך הוא אפס" אינם מתמזגים.

  **סכמת ה-IDs נאכפת ולא רק מוצהרת.** `add_document` הציבורי מקבל כל `u64`,
  ולכן המיון לפי `line_id` יכול היה לסדר שורות בסדר שאיש לא בחר בעודו מצהיר
  `document_id_scheme_version = 1`. נבדק המבנה שהסדר תלוי בו: כל שורות ספר חולקות
  חצי עליון אחד, אין חצי עליון משותף לשני ספרים, ואין שורה במיקום 0, ואין חצי
  עליון **אפס** — תחת סכמה 1 החצי העליון הוא `catalogue_order + 1`, ולכן אפס אינו
  מיקום בקטלוג אלא מה שאורדינל נראה כשדבר לא הרכיב אותו. רציפות **אינה** נדרשת —
  מחיקת שורה משאירה חור, וקורפוס עם שורה שנמחקה הוא קורפוס רגיל.

  ובצד ההזנה, `catalogue_order == u32::MAX` נדחה במקום שבו ה-ID מורכב:
  `(MAX + 1) << 32` גולש ב-`u64` ומתגלגל לבסיס אפס ב-release — מיקום אחד בלתי
  שמיש מתוך ארבעה מיליארד, שנקרא בשמו שם במקום להתגלות כדחיית האינדקס כולו.

- **`build_semantic_artifact` — בינארי למכונת ה-build.** נתיב אינדקס, קובץ מודל
  ומתכון → ארטיפקט מאומת. פותח דרך `TantivyCorpus::from_index_path`, שהוא
  read-only **בפועל**: בדיקת תאימות תחילה, `Index::open_in_dir` (לא
  `open_or_create`), ובלי `IndexWriter` כלל. דרך `SearchEngine` זו הדלת של הכותב —
  נתיב שגוי היה יוצר אינדקס ריק במקום לדווח שאין אחד, אינדקס legacy-compatible היה
  מקבל metadata חדש, נעילת הכותב הייתה מוחזקת לאורך כל הבנייה, וסכמה בלתי-תואמת
  הייתה גורמת `panic` לפני שמישהו יכול לדווח עליה. עד עכשיו המסלול המלא רץ רק בתוך בדיקה; זה הופך אותו
  ל-workflow נתמך ליצרן הארטיפקט. אינו חלק מה-FFI ואינו רץ באפליקציה. נבדק
  ב-[`tests/build_semantic_artifact.rs`](rust/tests/build_semantic_artifact.rs)
  מול אינדקס שנכתב לדיסק ונסגר, דרך הבינארי עצמו — כולל שער ה-backend הלא-סמנטי,
  שחייב להיות ברירת מחדל בכלי שצינור release מריץ ולא רק בספרייה שמתחתיו.

### Changed

- **ה-pin של `otzaria-semantic-search` עודכן** לגרסה שכוללת את ה-builder של S4b.
  שני התאמות נדרשו: `get_semantic_index_diff_from_lexical_hashes` מפריד עכשיו
  בין „המסלול הסמנטי כבוי" (`Ok(None)`) לבין „ההשוואה עצמה נכשלה" (שגיאה),
  ואיחוד השניים היה מדווח על manifest פגום כעל תכונה מכובה; ו-`SearchRequest`
  קיבל `profile` ו-`feature_flags`, ששניהם `None` כאן — בחירה בהם היא החלטה של
  S5 ולא של repin.


## 0.7.2 – 2026-08-06 – תיקון ספירת המילים בהדגשת הספר הפתוח

### Fixed

- **מילים הודגשו בספר במרווח שבו החיפוש עצמו אינו מוצא אותן.** תבנית
  ההדגשה (`generate_highlight_pattern`) ספרה מילה מתווכת אחת כ-`\S+` —
  רצף בלי רווחים — בעוד האינדקס מפצל מילים גם על מקף, פסק וסוף-פסוק. בפסוק
  `וַיֹּאמֶר לְאַבְרָם יָדֹעַ תֵּדַע כִּי־גֵר יִהְיֶה זַרְעֲךָ`, השאילתה
  "תדע זרעך" הודגשה כבר במרווח 2 (`כי־גר` נספרה כמילה אחת) בעוד החיפוש
  דורש 3. באפליקציה זה נראה כך: הטקסט מודגש, וחלונית החיפוש שלידו מציגה
  "אין תוצאות".

  מחלקות התווים של התבנית **נגזרות עכשיו מהטוקנייזר** ולא משוכפלות בעבודת
  יד: `hebrew_tokenizer::continues_token` הוא מקור האמת, ומעליו נבנות שלוש
  מחלקות **זרות זו לזו** — אות/ספרה (רק שם טוקן מתחיל ומסתיים), סימן רך
  (ניקוד, פיסוק שקוף, גרשיים — ממשיך טוקן ואינו פותח אותו), ושובר-טוקן.
  כך `רמב״ם`, `פ.ב.י`, `3.14` ו-`יב[ע]ר` הם מילה אחת, ואילו `כי־גר`,
  `א|ב`, `א/ב`, `א׃ב`, `כי⸗גר` (U+2E17) ו-`כי−גר` (U+2212) הם שתיים.

  הזרות אינה קוסמטית: בצורה הקודמת מחלקת המילה ומחלקת המפריד חפפו, ולכן
  `SEP(?:WORD SEP){0,n}` היה דו-משמעי ו-`RegExp` של Dart נתקע בשורה שאינה
  מתאימה. נמדד ב-Dart על אותה שורה בת 90 תווים עם תגי HTML צפופים:
  1,333ms במרווח 5 ו-4,350ms במרווח 10–30 לפני, **0ms** אחרי.

  `<`/`>` מטופלים עכשיו רק כתג שלם: `תדע<b>זרעך` אינו מודגש בשום מרווח, כי
  `strip_html_for_indexing` מוחק את התג בלי רווח והאינדקס רואה שם טוקן אחד.

- **המפריד של תבנית ההדגשה הליטרלית הורחב לאותה מחלקה.** `[\s־׀|]+` החטיא
  פיסוק דבוק: `תדע, זרעך` נמצא בחיפוש (הפיסוק אינו חלק מהטוקן) ולא הודגש
  בספר. עכשיו שני המסלולים חולקים את אותו `WORD_SEPARATOR`.

  שבעה טסטים אוכפים את השקילות: property test שעובר על כל תו בדומיין
  ומשווה את שלוש המחלקות לפרדיקטים של הטוקנייזר ומאמת את הזרות ביניהן, טסט
  טבלאי שמשווה את המרווח המזערי של התבנית למספר הטוקנים
  ש-`next_token_boundaries` מוצא בפער (18 מקרים), טסט לצמידות דרך פיסוק
  שקוף, שני טסטים לתגי HTML, טסט לאורכי התבנית, וטסט שמריץ חיפוש אמיתי על
  אינדקס ומאמת שהחיפוש וההדגשה מסכימים באותו מרווח. בצד Dart נוספו טסטי
  התאמה על טקסט אמיתי (`test/display_highlight_pattern_test.dart`).

## 0.7.1 – 2026-08-02 – תיקון קישור ב-Apple

### Fixed

- **בנייה ל-macOS/iOS נכשלה בשלב הקישור.** 0.7.0 הביאה איתה את llama.cpp,
  ושתי תלויות נייטיביות שלו לא הוצהרו בשום מקום. cargokit בונה `staticlib`,
  ולכן ההצהרות `cargo:rustc-link-lib` של `llama-cpp-sys-2` לא מגיעות ללינקר
  של Xcode כלל. שתי הבעיות התגלו רק בקישור של אפליקציה — ה-CI של הבינאריים
  המוכנים מייצר את הארכיון בלבד ואף פעם לא מלנקק אפליקציה מולו.

  - **frameworks חסרים ב-podspec** (`macos/`, `ios/`): ggml משתמש ב-vDSP
    (Accelerate) ו-Metal, והקוד עצמו הוא C++. נוספו `s.libraries = 'c++'`
    ו-`s.frameworks = 'Accelerate', 'Metal', 'MetalKit', 'Foundation'`.
    בלעדיהם חסרו מאות סמלים מסוג `_vDSP_*`, `_MTL*` ו-`___cxa_*`.
  - **`common` של llama.cpp נבנה שלא לצורך**: הסיידקר נעוץ מחדש ל-revision
    שמכבה אותו. `llama-cpp-2` הצהיר על `llama-cpp-sys-2` בלי
    `default-features = false`, וה-default של sys הוא `["common"]` — מה
    שגרר את `download.cpp` ואת `cpp-httplib` שאף פעם לא מקושרת, ולכן נשארו
    12 סמלי `httplib::*` בלתי פתורים. הקוד הזה לא היה נגיש מ-Rust מלכתחילה.
    זהות הווקטורים נשמרה (token ids זהים, worst cosine 0.9961), והארכיון
    ל-Apple קטן ב-~13MB.

## 0.7.0 – 2026-07-31 – חיפוש סמנטי היברידי

### Added

- **חיבור החיפוש הסמנטי ל-`SearchEngine` דרך FFI** – משטח API חדש
  ותוספתי לחלוטין; אף קריאה קיימת לא שונתה. הסיידקר
  [`otzaria-semantic-search`](https://github.com/Otzaria/otzaria-semantic-search)
  נקשר לאותה ספרייה נייטיבית של Tantivy, ונעוץ ל-revision מדויק ב-
  `rust/Cargo.toml` ו-`rust/Cargo.lock` — זהות המודל והאינדקס תלויה במימוש
  המדויק, ולכן ענף נייד אסור.

  - **מחזור חיים**: `configureSemantic` / `disableSemantic` / `semanticStatus`.
    קריאה חוזרת ל-`configureSemantic` עם אותם קלטים היא no-op שמחזירה את
    הסטטוס; עם קלטים שונים היא **נכשלת ומציינת איזה שדה השתנה**, כי מאגר
    הווקטורים הוא בזיכרון ופתיחה מחדש הייתה מוחקת אותו בשקט. החלפת מודל או
    ספרייה היא מעשה מפורש: `disableSemantic` תחילה.
  - **אינדוקס**: `semanticIndexBooks` / `semanticIndexDiff` /
    `removeSemanticBooks` / `resetSemanticIndex`. כולם מקבלים `&self` בצד
    Rust, כך ש-flutter_rust_bridge אינו נועל את המנוע כולו — חיפוש לקסיקלי
    ו-polling של סטטוס נשארים רספונסיביים לאורך אינדוקס של ספרייה שלמה.
  - **חיפוש**: `searchSemantic` עם `SemanticRetrievalMode`
    (`hybrid` / `semanticOnly` / `lexicalOnly`), נפרד מ-`SemanticLexicalMode`
    (`exact` / `fuzzy`) — פרשנות לקסיקלית ומצב אחזור הם שני צירים שונים.
    בקשת `hybrid` נופלת ל-Tantivy עם `fallbackReason` מפורש כשהסמנטי לא
    זמין; בקשת `semanticOnly` לעולם אינה מתחזה לתוצאה לקסיקלית.

- **חוזה תצוגה מפורש בתוצאה** – `snippetHtml` תמיד מכיל טקסט להצגה, ולצידו
  `isHighlighted` שאומר אם הוא נצבע. תוצאה סמנטית שלא עמדה בביטוי הלקסיקלי
  מקבלת קטע טקסט נקי ו-`isHighlighted == false`, במקום להיצבע כאילו נמצאה
  לקסיקלית. `SemanticResultSource` (`lexical` / `semantic` / `both`) מוסיף
  לכל תוצאה את מקורה.

### Changed

- **`minSdkVersion` הועלה מ-21 ל-23** – *שינוי שובר לצרכנים שתומכים ב-API 21
  או 22.* `llama.cpp` קורא ל-`posix_madvise`, ש-bionic חושף רק מ-API 23.
  התואם ל-`ANDROID_PLATFORM` שבו נבנים הבינארים המוכנים. אפליקציות על
  `flutter.minSdkVersion` (24) אינן מושפעות.

- **הפיצ'ר `semantic` פעיל בבניות הצרכן** (`rust/cargokit.yaml`) – בדרך כלל
  שקוף, כי הבינארים המוכנים מורדים משוחררים וחתומים. בנייה מקומית שנופלת
  אחורה (crate hash ללא artifacts) מקמפלת `llama.cpp` ולכן **דורשת cmake**.

- **ב-ARM 32-ביט (`armv7-linux-androideabi`) אין embedding backend** –
  `llama-cpp-sys-2` אינו נבנה ליעד הזה, ומודל 0.6B ב-Q4 אינו שמיש עליו
  ממילא. הבנייה מצליחה והחיפוש מתדרדר לבדו ללקסיקלי, אבל `semanticIndexBooks`
  **זורק** שם. הדגל לבדיקה לפני אינדוקס הוא `available` — לא `enabled`, ולא
  diff לא-ריק. ראו "Builds without a backend" ב-README.

## 0.6.9 – 2026-07-15

## 0.6.8 – 2026-07-14 – חיפוש מתקדם מורחב, facets ממדיים ושדרוג ביצועי אינדוקס

### Added

- **התאמה חלקית של מילות השאילתה (`wordMatchMode`) במסלול המתקדם** –
  צמד פרמטרים אופציונליים חדש בכל משפחת ה-advanced (`searchAdvanced`,
  ה-streams, `countAdvanced`, `countByBookAdvanced`,
  `getFacetCountsAdvanced` וגרסאות ה-`withStatus` שלהם):
  `wordMatchMode` — `all` (ברירת המחדל, ההתנהגות הקיימת) / `anyWord`
  (די במילה אחת) / `mostWords` (רוב: `n/2+1`) / `atLeast` (לפחות
  `wordMatchCount` מילים, נחתך ל-`[1, n]`). בכל מצב שאינו `all` דרישת
  הסדר והמרחק בטלה: `wordDistance` מתנהג כ"אותה פסקה" (BooleanQuery
  של Should עם מינימום נדרש — תוצאה עם יותר מילים מקבלת score גבוה
  יותר במיון רלוונטיות), ו-`sameSection` דורש שהסעיף יכיל לפחות את
  מספר המילים הנדרש (ספירת מילים ייחודיות פר סעיף במקום חיתוך).
  מילה שחוזרת בשאילתה נספרת פעם אחת בסף; שאילתת השלילה נשארת תמיד
  "כל המילים"; חלופות ר"ת נשארות ביטוי שלם.
  בהדגשה: מסנן-הביטוי מנוטרל בהתאמה חלקית כך שגם מילה בודדת שנמצאה
  נצבעת.

- **אפשרות "ארמית" פוצלה ל"קידומות ארמיות" ו"סיומות ארמיות"** – שתי
  אפשרויות פר-מילה עצמאיות במקום מפתח `ארמית` היחיד (שלא פורסם):
  "קידומות ארמיות" — קבוצת הקידומות הדקדוקית (ד/כד/אד/מד...) לפני
  המילה; "סיומות ארמיות" — שקילות אות סופית ה↔א (מלכה↔מלכא) ו-ם↔ן
  (חכמים↔חכמין). סימון שתיהן משחזר את ההתנהגות המקורית. בהדגשה:
  וריאנטי השקילות נצבעים תחת "סיומות ארמיות"; זכאות גבול-המילה נשברת
  רק תחת "קידומות ארמיות".

- **אפשרות "ראשי תיבות" – פענוח ר"ת דו-כיווני בחיפוש המתקדם** – מילון
  ראשי-תיבות (`Acronyms.json` של האפליקציה, נטען דרך
  `set_acronyms_dictionary_path`/`has_acronyms_dictionary`) מרחיב שאילתה
  שסומנה לה האפשרות: ר"ת בודד מוצא גם את פענוחיו המלאים
  (`רמב"ם` ← "רבי משה בן מיימון"), וביטוי שהוא פענוח ידוע מוצא גם את
  הר"ת (`רבי משה בן מיימון` ← `רמב"ם`). הפענוח רב-מילי ולכן נבנה
  כתת-שאילתות OR מלאות (slop 0, באותם facets/scope) ולא כחלופות
  חד-מילתיות; ההתאמה הדטרמיניסטית `רמבם`↔`רמב"ם` נשארת ברמת האינדקס
  (הטוקן-התאום). פענוח חד-מילי מדולג בטעינה (מכוסה ממילא ע"י האינדקס);
  תקרת `MAX_ACRONYM_EXPANSIONS = 16` פר כיוון. מצב מנוקד אינו נתמך בשלב
  זה. מפתח UI: `ראשי תיבות`. הדגשה: מילות החלופות מצטרפות לאיחוד
  ההדגשה השטוח, כך שמסמך שנמצא דרך הפענוח (או דרך הר"ת בכיוון ההפוך)
  נצבע — דרך נפילת מסנן-הביטוי לצביעה הרחבה.

- **אפשרות "תרגום ארמי" – הרחבת מילה בתרגומיה** – מילוני הארמית-עברית
  (`dictionary.json` של האפליקציה, נטען דרך
  `set_translation_dictionary_path`/`has_translation_dictionary`) מרחיבים
  מילה שסומנה לה האפשרות בתרגומיה בשני הכיוונים (ארמי↔עברי), כמילים
  חלופיות שזורמות בכל המסלולים הקיימים. כל המילונים שבקובץ ממוזגים
  (פשיטא, שיח ישראל, אונקלוס...) — הגרסה הראשונית לקחה את "המילון
  הראשון", ומפת serde_json ממוינת אלפביתית כך שנטען בפועל "מושגים
  ואישים" (ערך יחיד) וכל התרגומים מתו בשקט (`הכא`↔`כאן` לא עבד). רק
  תרגומים בני מילה אחת נכנסים למפה; תקרת
  `MAX_TRANSLATION_EXPANSIONS = 16`. מפתח UI: `תרגום ארמי`.

- **אפשרות "התעלם מגרשיים" פר-מילה** – גרש/גרשיים שהוקלדו במילה מוסרים
  לפני בניית התבנית (`רמב"ם` מחפש `רמבם`), שמותאמת באינדקס לשתי הצורות
  דרך הטוקן-התאום. מפתח UI: `התעלם מגרשיים`.

- **טוקן-תאום נטול-גרשיים באינדוקס** – במצב האינדוקס (`emit_quote_free`),
  מילה עם גרש/גרשיים מטמיעה גם את צורתה הנקייה באותה עמדה ובאותם
  offsets — חיפוש `רמבם` מוצא `רמב"ם` (וההפך, דרך "התעלם מגרשיים")
  וההדגשה יורשת את טווח המילה המקורית. שינוי *תוכן* מילון הטרמים בלי
  שינוי סכימת tantivy — בדיקת התאימות לא תופסת זאת לבד, ולכן מכוסה
  בהעלאת `INDEX_SCHEMA_VERSION` ל-3 (שטרם פורסמה), שמחייבת בנייה מחדש
  של אינדקסים ישנים.

- **`SearchStreamUpdate.truncated` – איתות "תוצאות חלקיות" ל-UI** – כשמסלול
  המילה-היחידה הרחבה חורג מתקציב איסוף הטרמים (`SINGLE_WORD_POSTINGS_BUDGET`
  / `max_expansions`) הוא ממשיך להגיש את ההרחבות בעדיפות הגבוהה (degrade, לא
  שגיאה) — עד כה בשקט, עם `warn!` ליומן בלבד. כעת דגל ה-truncation מחלחל
  מ-`single_regex_term_query` (כולל שמירה ב-`term_cache`) אל האירוע הראשון של
  ה-stream המשולב, כך שאוצריא מציגה באנר "ייתכן שהתוצאות חלקיות — צמצמו את
  החיפוש". המסלולים המדויק והמקורב נטולי-הסימנים אינם מתדרדרים כך ולכן
  תמיד `false`; המסלולים המנוקדים שלהם כן (מילה מנוקדת מתממשת לסט טרמים
  כמו מילה מתקדמת) ולכן נושאים את הדגל.

- **`add_text_book_bytes` – אינדוקס ספר טקסט מבייטים גולמיים (UTF-8)** –
  האפליקציה קוראת תוכן מ-SQLite שמאוחסן UTF-8; העברתו כ-`Vec<u8>`
  ‏(`Uint8List` בצד Dart) חוסכת את סבב הקידוד UTF-8→UTF-16→UTF-8 שמחרוזת
  Dart עולה על הגשר (~180ms/MB שנמדדו). קלט UTF-8 לא-תקין מתוקן (lossy),
  לעולם לא שגיאה. זהות מלאה ל-`add_text_book` — אותם מסמכים ואותה טביעת
  אצבע (ה-FNV מחושב על אותם בייטים).

- **נרמול מקבילי באינדוקס (rayon)** – ‏`add_text_book` ו-`add_pdf_book`
  מריצים את הנרמול (וסינון הזבל ב-PDF) על כל הליבות: מעבר זול סדרתי פותר
  את ה-reference trail (תלוי-סדר) לאינדקס-לשורה, ואז par_iter על השורות.
  בלוגים הנרמול היה ~85% מזמן ה-CPU של חוט ההזנה (55s מתוך 67s על 942
  ספרים).

- **`add_pdf_book` – אינדוקס PDF שלם בקריאת FFI אחת** – מקבל את עמודי הספר
  (reference, טקסט גולמי, אינדקס עמוד), מנרמל כל שורה
  (`normalize_pdf_text_for_indexing`), מסנן שורות זבל
  (`is_probably_garbage_pdf_text`) ומאנדקס — אותה לוגיקת per-line של
  `normalize_pdf_texts_for_indexing`, בלי שהטקסט המחולץ יחצה את הגשר
  ארבע-חמש פעמים (isolate ‏← נרמול באצוות ‏← SendPort ‏←
  addDocumentsBatch). מחזיר את מספר המסמכים שנוספו; 0 ⇒ אין טקסט שמיש
  (סרוק) והקורא נופל ל-sidecar/סמן-ריק. `segment` = אינדקס העמוד, מזהי
  מסמכים מקודדים סדר קטלוגי כמו `add_text_book`, ‏`contentHash`=0.

- **`set_bulk_indexing` – מצב בנייה מלאה ללא מיזוגי רקע** – בזמן בניית
  ספרייה מלאה `LogMergePolicy` ממזג סגמנטי-ביניים שוב ושוב — עבודה שנזרקת,
  כי הקורא מריץ `optimize` (מיזוג-הכול) פעם אחת בסוף. במצב bulk ה-writer
  (וגם writer שנפתח מחדש בעצלנות) מקבל `NoMergePolicy`; כבוי כברירת מחדל,
  ואינדוקס אינקרמנטלי ממשיך למזג כרגיל.

- **לוגי תזמון לאבחון מהירות אינדוקס** – ‏`add_text_book`/`add_pdf_book`
  מדווחים ב-`info!` מסמכים/בייטים/משך בפירוק prepare (נרמול על חוט ההזנה)
  מול enqueue (לחץ-חוזר מחוטי האינדוקס של tantivy); ‏`commit` מדווח משך
  commit ו-reader reload; ‏`optimize` מדווח סגמנטים לפני/אחרי ומשך. המנוע
  מתקין (פעם אחת, `try_init`) ‏env_logger עם ברירת מחדל
  `search_engine=info` — הלוגים נראים בקונסולת האפליקציה בלי הגדרה בצד
  Dart, ו-`RUST_LOG` עדיין גובר.

- **`*_with_status` למניית תוצאות – איתות truncation גם ל-count/facets** –
  ‏`count`, `count_by_book`, `get_facet_counts` (וגם ה-`_advanced`) זרקו את
  דגל ה-truncation של מסלול המילה-היחידה, כך שעץ סינון ה-facets היה מציג
  ספירות חלקיות בלי סימון. נוספו טיפוסים `CountResult`, `BookCountResult`,
  `FacetCountsResult` (כל אחד עם `truncated`) ומתודות `*_with_status`
  מקבילות. המתודות הישנות נשמרו כתואמות לאחור (מחזירות את הערך הבודד ומשמיטות
  את הדגל) — ומתועדות כלא-מתאימות לתצוגת UI כשחשוב לדעת אם התוצאה חלקית.
  נוספו גם `*_exact_with_status` ו-`*_fuzzy_with_status`: המסלולים
  נטולי-הסימנים שלהם אמנם לעולם אינם מתדרדרים, אבל המסלולים המנוקדים כן —
  ועד כה `count_exact`/`count_fuzzy` והמקבילים זרקו את הדגל.

### Changed

- **חתימת ספר קנונית הכוללת metadata (`computeBookFingerprint`)** –
  `add_text_book`/`add_text_book_bytes` חותמים עתה ב-`contentHash` חתימה
  שכוללת, לצד הטקסט הגולמי, גם את הכותרת, נתיב הקטגוריה, הסדר הקטלוגי,
  סדר הדורות וממדי הסינון (ממוינים ומנוקי-כפילויות, בקידוד קידומת-אורך) —
  שינוי metadata בלבד (למשל תיקון דור או מחבר) מזוהה עתה כספר שדורש
  אינדוקס-מחדש, במקום להשאיר את האינדקס עם facets/מיון/כותרת ישנים.
  **צד האפליקציה חייב לעבור מ-`computeContentFingerprint` (טקסט בלבד,
  נשאר קיים) ל-`computeBookFingerprint` בהשוואות מול
  `getBookFingerprints`** — אחרת כל ספר יזוהה כ"השתנה" בכל הפעלה.

- **הרחבת רף הווריאציות** (VARIATION_CEILING_RESEARCH.md) – ארבעה שינויים
  משלימים:
  - **מסלול מילה בודדת: degrade במקום שגיאה + תקציב postings** – איסוף
    הטרמים נעצר בהגעה לתקציב ומחזיר את הענפים בעדיפות גבוהה שנאספו (אין יותר
    `query exceeded max expansions`). השומר האמיתי הוא עתה תקציב postings
    (סכום doc_freq פר-segment, ‏1M), הנקרא חינם מה-streamer; תקרת מספר
    הטרמים נותרה כשומר זיכרון בלבד והועלתה פי 10 (מורפולוגי: 20k–50k,
    typo: ‏500, ברירת מחדל: 100). סדר הענפים הוא חוזה: כל הצורות המדויקות
    (מילה + חלופות) לפני כל וריאנט typo, מקובע בטסט.
  - **typo במילה בודדת דרך אוטומט לוינשטיין** – כשהדגל היחיד הוא "שגיאות
    כתיב", ההרחבה רצה כסריקת FST אחת לכל טוקן (מרחק 1 + שיכול) במקום ≤128
    סריקות וריאנטים ליטרליים: כל שכונת מרחק-עריכה-1 (על-קבוצה של הרשימה
    הקודמת) במחיר נמוך פי ~100, בכפוף לתקציבי האיסוף — typo בעדיפות הנמוכה
    ביותר, ומדולג אם הצורות המדויקות לבדן מיצו תקציב (מילה שכיחה במיוחד).
    typo בשילוב מורפולוגיה/כתיב נשאר
    במסלול הליטרלי (האוטומט לא מרכיב wildcards). ההדגשות (display + snippets)
    שומרות parity.
  - **תקרות פרַאזה הועלו** – `max_expansions` לפרַאזות: 8,192 אחיד (היה
    100–5,000; ברירת המחדל של tantivy היא 16,384), ותקציבי הענפים
    64/48 ענפים ו-6,000 תווים (היו 48/20 ו-1,000) —
    בזכות ה-vendor הבא.
  - **vendor של tantivy-fst 0.5.0 עם `STATE_LIMIT=8192`** (היה 1,000; שינוי
    יחיד, מנוהל ב-`rust/vendor/tantivy-fst` דרך `[patch.crates-io]`) –
    תבניות פרַאזה מורפולוגיות אמיתיות (48 ענפים / ~800 תווים) שקרסו על תקרת
    ה-DFA מתקמפלות עתה; העלות זיכרון חולף בבניית השאילתה (≈4KB ל-state).
  - תקרות ההדגשה יושרו פרופורציונלית: ‏`MAX_HIGHLIGHT_TERMS` ‏512→2,048,
    ‏`MAX_DISPLAY_PATTERN_CHARS` ‏4,000→12,000.
  - כיול אמפירי של התקציבים (postings budget, תקרות פרַאזה) על אינדקס אמיתי
    דרך `benchmark_cli` — עדיין פתוח.

- **גרשיים וגרש נשמרים בתוך טוקנים** – ראשי-תיבות (`רמב"ם`, `ז"ל`) ומילים עם
  גרש פנימי (`ג'ורג'`, `ד'אש`) מאונדקסים כטרם יחיד. ׳/״ עבריים מקופלים בטרם
  ל-'/" ASCII, וזוג גרשים בין אותיות (`רמב''ם`, מוסכמת קבצים ישנים) מאוחד
  לגרשיים — כל צורות הדפוס מתלכדות לטרם אחד. `splitQueryWords` משקף את אותם
  חוקים, וההדגשות תופסות את כל שלוש צורות הדפוס (`"`, `״`, `''`).
- **חיפוש מדויק רגיש-גרשיים** (מחיר מתועד) – שאילתה `רמבם` ללא גרשיים לא
  תמצא `רמב"ם` בחיפוש מדויק ובמתקדם ללא דגלים, ולהפך. הגישור קיים במקורב
  (גם במרחק 0, דרך הזרקת הצורה הנקייה), בדגל typo (וריאנט-מחיקה) ובדגל
  כתיב מלא/חסר. כמו כן, מרכאות-כציטוט בתחילת מילה (`ה"מגיד`) הופכות לחלק
  מהטוקן — שאילתת `מגיד` מדויקת תחטיא מופע כזה; "חלק ממילה"/מקורב מגשרים.
- **תיקון מפתח ה-lookup הלקסיקלי** – `normalize_hebrew` מוחק עתה גם `"`/`'`
  ASCII, כך שטוקני-גרשיים ממשיכים לקבל הרחבות מ-`lexical.db` ולהיתפס
  ב-blacklist.

### Fixed

- **תקרת הקיבוץ שומרת את הקבוצות הטובות ביותר, גלובלית** – בהגעה
  ל-50,000 קבוצות ה-collector השמיט כל קבוצה *חדשה* לפי סדר סריקת
  המסמכים והסגמנטים — קבוצה טובה (למשל id נמוך במיון קטלוגי, או ציון
  גבוה ברלוונטיות) שהגיעה מאוחר נזרקה בעוד גרועות ממנה נשמרו, והעמוד
  הראשון היה שגוי. כעת בתקרה הקבוצה *הגרועה* לפי סדר המיון מפנה את
  מקומה לקבוצה טובה ממנה (אינדקס BTreeSet לצד המפה) — כל עמוד בטווח
  התקרה מדויק. הצבירה עברה למפה משותפת **אחת לכל החיפוש** (הסגמנטים
  כותבים אליה דרך buffer של ‎4K מסמכים) — במקום עד 50k קבוצות *לכל
  סגמנט* שהצטברו לפני המיזוג למאות MB באינדקס לא-ממוזג; הזיכרון כעת
  מפה גלובלית חסומה ב-50k קבוצות בתוספת buffers חסומים פר-סגמנט,
  ללא תלות במספר הסגמנטים. שארית ה-degrade תחת
  `truncated`: ‏`group_count` נשאר תחתית, ומונה של קבוצה שפונתה וחזרה
  מאבד את חבריה המוקדמים. שימו לב: המסלולים שמחזירים רק
  `List<SearchResult>` משמיטים את הדגל כמו את שאר דגלי הסטטוס — לתצוגת
  קיבוץ השתמשו ב-`searchAndCount*`/`stream_with_counts`.

- **תקציב לספירת הסעיפים בהתאמה חלקית בטווח "תחת אותה כותרת"** – מפת
  ספירת המילים-פר-סעיף של `mostWords`/`atLeast` גדלה כאיחוד סעיפי כל
  המילים — מילים נפוצות הגיעו למאות אלפי רשומות ללא תקרה. כעת תקציב
  קשיח (500k סעיפים): מעבר אליו סעיפים *חדשים* נשמטים עם
  `truncated`, וסעיפים שכבר נספרים ממשיכים להצטבר.

- **חתימת הדה-דופ (`lineHash`) כוללת אלפאנומרי לא-ASCII** – ספרות
  ערביות-הודיות (١٥/١٦), אותיות לטיניות עם סימנים ושאר אלפאנומרי יוניקודי
  נשמטו מהחתימה, כך ששורות שנבדלו רק בהם אוחדו בטעות במצב "טקסט זהה".
  כעת כל `is_alphanumeric` משתתף, בקיפול רישיות יוניקודי; סף 12 האותיות
  נותר עברי בלבד.

- **`SearchPageResult.truncated` – איתות "תוצאות חלקיות" גם במסלול page/count** –
  דגל ה-truncation שנוסף ל-stream המשולב נזרק ב-`search_and_count_advanced`
  וב-`search_and_count` הגנרי (regex), כך שצרכן של ה-API המעומד היה מציג
  תוצאות וספירה חלקיות בלי אזהרה. כעת `SearchPageResult` נושא `truncated`
  באותה סמנטיקה של `SearchStreamUpdate.truncated`; המסלולים המדויק/המקורב
  נטולי-הסימנים תמיד `false`, המנוקדים נושאים את הדגל.

- **שלילה בטווח "תחת אותה כותרת" פוסלת את כל הסעיף** – שאילתת השלילה
  בטווח `SameSection` מחזירה רק את השורות שנושאות מילת שלילה בתוך סעיף
  חותך, כך שב-`MustNot` היא פסלה רק אותן — תוצאה חיובית בשורה אחרת של
  אותו סעיף שרדה בטעות. כעת נאספים ה-`sectionId` שהשלילה חותכת
  (`SectionIdsCollector`) וכל שורה בסעיף כזה נחסמת
  (`SectionFilteredQuery` על `AllQuery`).

- **בדיקת התאימות דורשת meta.json תקין של tantivy** – sidecar תקין עם
  `schema_version` נכון החזיר "תואם" גם כשה-meta.json של tantivy עצמו חסר
  או פגום — מצב שבו פתיחת האינדקס נכשלת בכל מקרה. כעת כשל
  קריאה/פרסור/חוסר-סכימה ב-meta.json מחזיר `rebuild_required` עם הסיבה,
  במקום ליפול בשקט ל-"compatible".

- **בדיקת התאימות משווה גם את סכימת ה-tantivy בפועל** – אינדקס שנבנה
  בגרסת-ביניים של אותה schema_version (למשל `text` עם fast field, לפני
  ההסרה) עבר את בדיקת הקובץ הצדדי ("3=3, תואם") אבל הפיל את פתיחת המנוע
  על SchemaError — והאפליקציה נפלה בשקט לאינדקס זמני ב-Temp שנבנה מחדש
  בכל הפעלה. כעת `check_index_compatibility` משווה את הסכימה השמורה
  ב-meta.json מול סכימת המנוע (אותה השוואה של `Index::open_or_create`)
  ומחזירה `rebuild_required` על סטייה, כך שזרימת הבנייה-מחדש הרגילה
  מטפלת בזה.

- **הטקסט השמור באינדקס משמר פיסוק** (Otzaria issue #446) – נרמול ה-ingestion
  (`normalizeTextForIndexing` / `normalizePdfTextForIndexing`) כבר לא מוחק
  פסיקים, נקודתיים, סוגריים וכו', כך שתוצאות החיפוש מציגות "עא:" ולא "עא".
  שוויון מילון הטרמים מול צד השאילתה נשמר ע"י "תווים שקופים" ב-`HebrewTokenizer`:
  הפיסוק ש-`sanitizeQuery` מוחק אינו שובר טוקן ואינו נכלל בטקסטו ("א.ב" → "אב"),
  וההדגשות (SnippetGenerator) נופלות נכון על הטקסט המקורי דרך ה-offsets.
- **פירוק Hebrew Presentation Forms** (Otzaria issue #500) – תווים מורכבים
  בטווח U+FB1D–U+FB4F (כגון יִ שהוצגה כ"?") מפורקים לאות בסיס + סימן בזמן
  האינדוקס, והסימן מוסר עם שאר הניקוד. מילים שהכילו אותם נעשות ברות-חיפוש.

### Notes

- גרסת סכימת האינדקס עלתה ל-3 (טרמים חדשים + הסרת ה-fast field): אינדקסים
  קיימים ידווחו `rebuild_required` וייבנו מחדש פעם אחת בעדכון.

## 0.6.7 – 2026-06-30

### Fixed

- **פרסום מחדש עם סופי-שורה LF** – הגרסה שפורסמה ב-0.6.6 הכילה את סקריפטי
  cargokit (`run_build_tool.sh`, `build_pod.sh`) עם CRLF, מה ששבר את בניית
  Linux/Android/macOS אצל הצרכן (`/usr/bin/env: 'bash\r'`). אין שינוי קוד.

## 0.6.6 – MagicDictionary fuzzy search – 2026-06-29

### New

- **שילוב MagicDictionary בחיפוש מקורב (fuzzy)** – ניתן לטעון `lexical.db` בזמן
  ריצה, והחיפוש המקורב מרחיב מונחים לצורות מורפולוגיות קשורות בלי לשנות את
  התנהגות החיפוש כאשר המילון לא נטען.

### Improvements

- **דירוג רלוונטיות בחיפוש מקורב** – תוצאות `ResultsOrder::Relevance` מקבלות
  מדרוג ברור יותר: התאמה מדויקת, אחריה צורה מורפולוגית מהמילון, ואחריה התאמת
  fuzzy רגילה.
- **הדגשות בחיפוש מקורב עם מילון** – ההדגשה משקפת גם את הצורות המורפולוגיות
  שהוזרקו מה-`lexical.db`, כולל תמיכה בשאילתות מרובות מילים.

## 0.6.5 – Fix Apple linking – 2026-06-12

### Fixes

- **תיקון קישור (linking) ב-macOS/iOS** – הוספת `module_name = 'search_engine'`
  בגרסה 0.6.4 שינתה את `PRODUCT_NAME` של ה-pod, כך ש-cargokit כתב את
  `libsearch_engine.a` ל-`$PODS_CONFIGURATION_BUILD_DIR/$PRODUCT_NAME` בעוד
  ש-`-force_load` עדיין חיפש ב-`${BUILT_PRODUCTS_DIR}` — שתי תיקיות שונות,
  והבנייה נפלה עם `library 'libsearch_engine.a' not found`. תוקן ביישור
  `output_files` ו-`OTHER_LDFLAGS` ל-`${PODS_CONFIGURATION_BUILD_DIR}/${PRODUCT_NAME}`.

## 0.6.4 – Fix macos – 2026-06-12

### Fixes

- MACOS

## 0.6.3 – Line Endings Fix (Republish) – 2026-06-12

### Fixes

- **תיקון סופי של סיומות השורה (CRLF) בארכיון שפורסם** – למרות שגרסה 0.6.2 נועדה
  לתקן את הבעיה, הארכיון שפורסם בפועל ל-pub.dev עדיין הכיל CRLF בסקריפטי
  cargokit (`build_pod.sh`, `run_build_tool.sh`), כי לא ניתן לפרסם מחדש גרסה
  קיימת ב-pub.dev. כתוצאה מכך הבנייה ב-macOS המשיכה ליפול עם
  `set: - invalid option`. גרסה זו נארזת מחדש ממכונת macOS עם LF בלבד. אין שינוי
  קוד.

## 0.6.2 – Line Endings Fix – 2026-06-12

### Fixes

- **תיקון סיומות שורה (CRLF) בארכיון שפורסם** – גרסה 0.6.1 פורסמה מ-Windows עם
  `core.autocrlf=true`, וסקריפטי ה-shell של cargokit (`build_pod.sh`,
  `run_build_tool.sh`) נארזו עם CRLF — מה ששבר את הבנייה ב-macOS
  (`set: - invalid option`), Android ו-Linux (exit 127 בגלל shebang פגום).
  אין שינוי קוד; פרסום מחדש עם LF בלבד. נוסף `.gitattributes` שמונע הישנות.

## 0.6.1 – Fuzzy Highlight Fix – 2026-06-12

### Fixes

- **הדגשות בחיפוש מקורב (fuzzy)** – תוצאות `searchFuzzy` / `searchAndCountFuzzy` /
  `searchFuzzyStream` / `searchFuzzyTerms` חזרו עד כה ללא הדגשה כלל, כי
  `FuzzyTermQuery` מבוסס-אוטומט ואינו חושף מונחים למחולל ה-snippets. כעת מונחי
  ההדגשה ממומשים ממילון האינדקס דרך אותו אוטומט לוינשטיין שהחיפוש משתמש בו
  (כמו במצב advanced), כך שגם וריאנטים במרחק עריכה — ולא רק המילה שהוקלדה —
  מודגשים בתוצאות.
- **אימות `max_distance` מראש** – ערך `max_distance` מחוץ לטווח 0–2 ב-`searchFuzzy`
  גורם כעת לשגיאה מפורשת לפני בניית השאילתה, במקום כשל לא צפוי בהמשך.

## 0.6.0 – Mode-Specific Search & Hardening – 2026-06-11

---

### Breaking Changes

#### `ReferenceSearchEngine` הוסר

המנוע הייעודי לחיפוש הפניות (`ReferenceSearchEngine`, `ReferenceSearchResult`,
`ReferenceDocumentInput`) הוסר מה-API הציבורי. אפליקציות שהשתמשו בו צריכות
להסיר את הקריאות לפני עדכון התלות.

#### חבילה שונתה ל-`otzaria_search_engine`

החבילה, ה-export הראשי וה-podspecs של iOS/macOS שונו מ-`tantivy_search_engine`
ל-`otzaria_search_engine` (שם ה-crate הפנימי `search_engine` נשאר).

#### `search()` – חריגה מ-`maxExpansions` במונח בודד מחזירה שגיאה

עד כה התקרה נאכפה רק בשאילתות מרובות מונחים; מונח regex בודד רץ ללא הגבלה.
כעת חריגה מחזירה שגיאה בכל המקרים, בדומה להתנהגות של `RegexPhraseQuery`.

---

### New APIs

- **חיפוש לפי מצב** – `searchExact` / `searchFuzzy` / `searchAdvanced` (+
  `countExact/Fuzzy/Advanced`, `searchAndCountExact/Fuzzy/Advanced`,
  `searchExactStream` / `searchFuzzyStream` / `searchAdvancedStream`).
  המצב המתקדם מקבל `searchOptions` / `alternativeWords` / `customSpacing`
  ומריץ את כל לוגיקת השאילתות העברית (קידומות, סיומות, כתיב מלא/חסר,
  סובלנות לשגיאות) ב-Rust (מודול `hebrew_query`).
- **הדגשות בתוצאות regex/advanced** – מונחי ההדגשה ממומשים ממילון האינדקס
  דרך אותו אוטומט שהחיפוש משתמש בו, כך שכל וריאציה מורפולוגית שתאמה מודגשת.
- **`checkIndexCompatibility(path)`** – בדיקת תאימות אינדקס (sidecar
  `otzaria_index_meta.json` + נפילה חזרה להשוואת הסכמה המלאה של Tantivy).
- **קריאת תוכן האינדקס** – `countDocumentsByFilePath()` ו-`getIndexedFilePaths()`
  לשחזור מצב האינדוקס ישירות מהאינדקס.
- **`searchAndCount` / `searchStream`** – ספירה ותוצאות במעבר יחיד, והזרמת
  תוצאות בנתחים; `search` קיבל `offset` (חובה) ו-`highlight` (אופציונלי).

---

### Fixes & Hardening

- שאילתה ריקה (או סימני פיסוק בלבד) מחזירה אפס תוצאות בכל המצבים —
  ולא panic במצב advanced או *כל* המסמכים במצב fuzzy.
- רשימת facets ריקה כבר לא מאפסת תוצאות בנתיבי ה-regex/advanced;
  facet לא תקין מחזיר שגיאה במקום panic.
- שאילתות advanced מנורמלות כמו האינדקס (הסרת ניקוד + lowercase),
  כך שטקסט מנוקד שהודבק כבר לא מחזיר אפס תוצאות בשקט.
- `SearchEngine.new` לא קורס כשנעילת ה-writer תפוסה — נפתח לקריאה
  והכתיבה הראשונה מנסה שוב; הודעות שגיאה ברורות לסכמה לא תואמת.
- `optimize()` שומר (commit) שינויים ממתינים במקום לזרוק אותם.
- בדיקת התאימות לאינדקסים ישנים משווה את הסכמה המלאה ולא רק את שדה `id`.

### Performance

- מתודות החיפוש הישנות עברו ל-`&self` — חיפושים מקבילים לא מסתנכרנים
  יותר מאחורי נעילת כתיבה.
- `SnippetGenerator` נוצר פעם אחת לכל stream (ולא לכל chunk);
  תקציב מונחי ההדגשה מתחלק שווה בין מילות השאילתה.

### Packaging / CI

- `url_prefix` של הבינארים המקומפלים מצביע על הריפו הקנוני
  (`otzaria/otzaria_search_engine`); סודות ה-CI מוגבלים ל-steps החותמים.

---

## 0.5.0 – Bridge Expansion (Tantivy 0.26 / FRB 2.12) – 2026-05-02

---

### Breaking Changes

#### Schema Change: `id` field is now INDEXED

**Affects:** All existing indices built with the previous version.

שדה `id` שודרג מ-`STORED | FAST` ל-`STORED | FAST | INDEXED`.

**נדרש:** rebuild מלא של כל האינדקסים הקיימים.

**למה:** בלי `INDEXED`, פעולות `delete_term` לא עובדות על השדה הזה, ולכן `deleteDocumentById`, `upsertDocument` ו-`upsertDocumentsBatch` לא היו אפשריות.

---

#### Tantivy 0.26: `TopDocs` no longer implements `Collector` directly

**Affects:** `rust/src/api/search_engine.rs`, `rust/src/api/reference_search_engine.rs`

ב-Tantivy 0.26, `TopDocs` הפסיק לממש את ה-trait `Collector` ישירות.
כדי לקבל collector לתוצאות ממוינות לפי ציון רלוונטיות, חובה לקרוא ל-`.order_by_score()`.

```rust
// לפני (Tantivy < 0.26) – התקמפל אבל כעת שגוי:
let collector = TopDocs::with_limit(100);
searcher.search(&query, &collector)?;

// אחרי (Tantivy 0.26) – חובה:
let collector = TopDocs::with_limit(100).order_by_score();
searcher.search(&query, &collector)?;
```

תוקן בשני המנועים.

---

#### `search()` signature – נוספו פרמטרים

```dart
// לפני:
Future<List<SearchResult>> search({
  required List<String> regexTerms,
  required List<String> facets,
  required int limit,
  required int slop,
  required int maxExpansions,
  required ResultsOrder order,
});

// אחרי:
Future<List<SearchResult>> search({
  required List<String> regexTerms,
  required List<String> facets,
  required int limit,
  required int offset,                 // ← חדש (required, אין ברירת מחדל)
  required int slop,
  required int maxExpansions,
  required ResultsOrder order,
  HighlightConfig? highlight,          // ← חדש (אופציונלי)
});
```

**Migration:** הוסף `offset: 0` לכל קריאות `search()` קיימות.

---

#### `createQuery` / `createSearchQuery` הוסרו

פונקציות אלו חשפו `BoxQuery` ו-`Index` כ-opaque types בלי שום API ציבורי להפעלתן מ-Dart. הן היו dead-ends ממשיים.

**Migration:** אין תחליף ישיר – השתמש ב-`search()`, `searchAndCount()` או `searchFuzzy()`.

---

### New APIs – `SearchEngine`

#### Write

| Method | תיאור |
|---|---|
| `deleteDocumentById(id)` | מחיקה מדויקת לפי מזהה. מחליף את `removeDocumentsByTitle`. |
| `upsertDocument(id, ...)` | מחיקת ישן + הוספת חדש בפעולה אחת. מניעת כפילויות. |
| `addDocumentsBatch(docs)` | הוספת רשימת מסמכים ב-FFI call אחד, ללא delete. מיועד לטעינה ראשונית. |
| `upsertDocumentsBatch(docs)` | כמו batch אבל עם delete-before-add לכל מסמך. מיועד לעדכונים. |
| `rollback()` | ביטול כל השינויים מאז ה-commit האחרון. |

#### Read

| Method | תיאור |
|---|---|
| `getDocumentById(id)` | שליפת מסמך יחיד לפי ID. מחזיר `SearchResult?` עם טקסט גולמי (ללא snippet). |
| `searchAndCount(...)` | חיפוש + ספירה כוללת ב-pass אחד דרך Tantivy (tuple collector). מחזיר `SearchPageResult`. |
| `getFacetCounts(regexTerms, facets, facetPrefix, ...)` | ספירת תוצאות לפי קטגוריה תחת prefix נתון. שימושי ל-drill-down בממשק. |
| `searchFuzzy(terms, facets, limit, offset, maxDistance, order, highlight?)` | חיפוש מקורב אמיתי (Levenshtein) על מילות טקסט רגילות. `maxDistance`: 0=מדויק, 1–2=מקורב. |
| `searchStream(regexTerms, ..., chunkSize)` | מחזיר `Stream<List<SearchResult>>` ב-Dart. שלב ה-TopDocs (דירוג) מסתיים לפני פליטת ה-chunk הראשון; שלב שליפת המסמכים ויצירת ה-snippets מתבצע באופן מוגדר. שימושי כשה-`limit` גדול ויצירת snippets היא צוואר הבקבוק. |

#### Operational

| Method | תיאור |
|---|---|
| `optimize()` | מיזוג כל הסגמנטים לאחד. להריץ ברקע לאחר הרבה עדכונים/מחיקות. |
| `getDocumentCount()` | סך כל המסמכים באינדקס. |
| `getSegmentCount()` | מספר הסגמנטים הנוכחי. גבוה = כדאי להריץ `optimize()`. |

#### Structs חדשים

```dart
class DocumentInput {
  final BigInt id;
  final String title;
  final String reference;
  final String topics;
  final String text;
  final BigInt segment;
  final bool isPdf;
  final String filePath;
}

class HighlightConfig {
  final String highlightPrefix;   // ברירת מחדל: "<font color=red>"
  final String highlightPostfix;  // ברירת מחדל: "</font>"
  final int maxChars;             // ברירת מחדל: 800
}

class SearchPageResult {
  final int totalCount;
  final List<SearchResult> results;
  final bool truncated; // תוצאות/ספירה חלקיות (חריגה מתקציב הרחבת מילה יחידה)
}

class FacetCount {
  final String path;
  final int count;
}
```

---

### New APIs – `ReferenceSearchEngine`

| Method | תיאור |
|---|---|
| `deleteDocumentById(id)` | מחיקה לפי ID |
| `upsertDocument(id, ...)` | עדכון לפי ID |
| `addDocumentsBatch(docs)` | batch הוספה |
| `upsertDocumentsBatch(docs)` | batch עדכון |
| `rollback()` | ביטול שינויים |

Struct חדש:
```dart
class ReferenceDocumentInput {
  final BigInt id;
  final String title;
  final String reference;
  final String shortRef;
  final BigInt segment;
  final bool isPdf;
  final String filePath;
}
```

---

### Bug Fixes

#### `IndexReader` נוצר מחדש בכל חיפוש (SearchEngine)

**לפני:** כל קריאה ל-`search()` / `count()` / `countByBook()` פתחה `IndexReader` חדש מהדיסק – פעולה יקרה מאוד.

```rust
// לפני (בעייתי – קורא metadata מהדיסק בכל חיפוש):
let searcher = index.reader()?.searcher();
```

**אחרי:** `IndexReader` נשמר ב-struct ומשתמשים בו לכל החיפושים. `commit()` מרענן אותו.

```rust
// אחרי (מהיר – reader כבר בזיכרון):
let searcher = self.index_reader.searcher();
```

**השפעה:** שיפור ביצועים משמעותי בחיפוש, במיוחד תחת עומס.

#### `ReferenceSearchEngine` התעלם מה-`IndexReader` הקיים

גם ב-`ReferenceSearchEngine` היה `index_reader` ב-struct אבל לא השתמשו בו בפועל. תוקן.

---

### Notes

- **`removeDocumentsByTitle`** נשמר לתאימות אחורה אבל לא מומלץ לשימוש חדש. השתמש ב-`deleteDocumentById`.
- **fuzzy קיים לעומת חדש:** הקוד הנוכחי באפליקציה משתמש ב-`slop` עם מילים רגילות כ"חיפוש מקורב". `searchFuzzy()` מוסיף חיפוש מקורב אמיתי ברמת ה-Levenshtein – מוצא מסמכים גם כשיש שגיאות כתיב, כתיב מלא/חסר, וכו'.
- **`searchStream` vs pagination:** `searchStream` שולח תוצאות ב-chunks – שלב הדירוג (TopDocs) מסתיים לפני ה-chunk הראשון, אבל שליפת המסמכים ויצירת ה-snippets מתפצלים. ל-pagination רגילה, `search()` עם `offset` מספיקה לרוב המקרים.

---

## 0.0.1

* Initial release.
