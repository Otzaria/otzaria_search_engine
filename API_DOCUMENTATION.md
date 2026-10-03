# Otzaria Search Engine - API Documentation

This document describes the API exposed by the Otzaria Search Engine through Flutter/Dart bindings. The API is generated from Rust code using flutter_rust_bridge.

## Table of Contents

1. [Classes](#classes)
   - [SearchEngine](#searchengine)
     - [Semantic search](#semantic-search)
2. [Top-Level Functions](#top-level-functions)
  - [checkIndexCompatibility](#checkindexcompatibility)
3. [Data Models](#data-models)
   - [SearchResult](#searchresult)
  - [IndexCompatibility](#indexcompatibility)
   - [ResultsOrder](#resultsorder)

---

## Classes

### SearchEngine

Main search engine for full-text search with regex support.

#### Constructor

```dart
SearchEngine.new(String path)
```

**Synchronous** constructor that creates a new search engine instance.

**Parameters:**
- `path` (String): File system path where the search index will be stored

**Returns:** SearchEngine instance

---

#### Methods

##### addDocument

```dart
Future<void> addDocument(
  int id,
  String title,
  String reference,
  String topics,
  String text,
  int segment,
  bool isPdf,
  String filePath
)
```

Adds a document to the search index.

**Parameters:**
- `id` (int/u64): Unique document identifier
- `title` (String): Document title
- `reference` (String): Document reference/citation
- `topics` (String): Faceted topics (hierarchical, e.g., "category/subcategory")
- `text` (String): Full document text to be indexed
- `segment` (int/u64): Segment number within the document
- `isPdf` (bool): Whether the document is a PDF
- `filePath` (String): File path to the document

**Returns:** Future<void>

---

##### commit

```dart
Future<void> commit()
```

Commits all pending document additions to the index. Must be called after adding documents to make them searchable.

**Returns:** Future<void>

---

##### search

```dart
Future<List<SearchResult>> search({
  required List<String> regexTerms,
  required List<String> facets,
  required int limit,
  required int offset,
  required int slop,
  required int maxExpansions,
  required ResultsOrder order,
  HighlightConfig? highlight,
})
```

Performs a search query on the index using regex patterns.

**Parameters:**
- `regexTerms` (List<String>): List of regex patterns to search for
  - Single term: matching index terms are materialized (capped at `maxExpansions`)
  - Multiple terms: uses RegexPhraseQuery with specified slop
- `facets` (List<String>): List of topic facets to filter by (empty list = no facet filter)
- `limit` (int/u32): Maximum number of results to return
- `offset` (int/u32): Number of leading results to skip (pagination)
- `slop` (int/u32): Maximum distance between terms in phrase queries (for multi-term searches)
- `maxExpansions` (int/u32): Regex-expansion ceiling. A single term truncates its term collection at the ceiling (partial results, flagged via the status-bearing variants). A multi-term phrase checks Tantivy's cumulative expansions independently in every segment: when every segment fits, the exact `RegexPhraseQuery` runs; otherwise the engine falls back to a term-list phrase built from per-position materialized term sets (per-position caps, truncation flagged) — never an error
- `order` (ResultsOrder): Sort order for results (Catalogue or Relevance)
- `highlight` (HighlightConfig?, optional): Snippet/highlight configuration; defaults to `<font color=red>` tags and 800 chars

**Returns:** Future<List<SearchResult>>

**Variants:**
- `searchAndCount(...)` → `SearchPageResult` — same arguments, returns the total hit count alongside the page in a single index pass
- `searchStream(..., chunkSize)` → `Stream<List<SearchResult>>` — emits results in chunks as snippets are built
- `count(regexTerms, facets, slop, maxExpansions)` → `int` — count only

---

##### Mode-specific search (exact / fuzzy / advanced)

Higher-level APIs that take a raw query string and build the query in Rust:

```dart
// Exact: term/phrase match after nikud-stripping + tokenization. Fastest.
Future<List<SearchResult>> searchExact({query, facets, limit, offset, order})

// Fuzzy: Levenshtein matching per token (maxDistance edits, 0–2).
Future<List<SearchResult>> searchFuzzy({query, facets, limit, offset, maxDistance, order})

// Advanced: Hebrew morphological query builder (prefixes, suffixes,
// full/deficient spelling, typo tolerance, alternative words, custom spacing).
Future<List<SearchResult>> searchAdvanced({
  query, facets, limit, offset, distance,
  customSpacing, alternativeWords, searchOptions, order,
  wordMatchMode, wordMatchCount,
})
```

`wordMatchMode` (optional, advanced family only) relaxes the all-words
requirement: `all` (default — current behavior), `anyWord`, `mostWords`
(`n/2+1`), or `atLeast` together with `wordMatchCount` (clamped to
`[1, n]`). Any mode other than `all` drops the order/distance requirement:
`wordDistance` scope behaves like `sameParagraph`, and `sameSection`
requires the section to carry at least the required number of distinct
query words. Duplicate query words count once toward the threshold.
The negative query always requires all of its words.

Each mode also provides `count*`, `searchAndCount*` and `search*Stream`
variants (`countExact`, `searchAndCountFuzzy`, `searchAdvancedStream`, …).
Queries are normalized like the index (nikud stripped, lowercased); empty
queries return no results. Advanced-mode highlighting wraps every
morphological variant that actually matched, not just the literal words;
fuzzy-mode highlighting likewise wraps every term within the requested edit
distance, not just exact occurrences of the query words.

---

##### Write & maintenance API

```dart
Future<void> addDocumentsBatch({required List<DocumentInput> docs}) // bulk add, no commit
Future<void> upsertDocument({...})                // delete-by-id + re-insert, no commit
Future<void> upsertDocumentsBatch({required List<DocumentInput> docs})
Future<void> deleteDocumentById({required BigInt id})
Future<void> rollback()                           // discard writes since last commit
Future<void> optimize()                           // commit pending + merge all segments
Future<BigInt> getDocumentCount()
Future<int> getSegmentCount()
Future<List<FacetCount>> getFacetCounts({...})    // per-child facet counts for a prefix
Future<Map<String, int>> countByBook({...})       // per-filePath hit counts for a query
```

---

##### clear

```dart
Future<void> clear()
```

Removes all documents from the index.

**Returns:** Future<void>

---

##### removeDocumentsByTitle

```dart
Future<void> removeDocumentsByTitle(String title)
```

Removes all documents with the specified title from the index.

**Parameters:**
- `title` (String): Title of documents to remove

**Returns:** Future<void>

---

##### countDocumentsByFilePath

```dart
Future<Map<String, int>> countDocumentsByFilePath()
```

Returns the number of live (committed, non-deleted) documents per distinct `filePath` across the whole index.

The result is read from the index itself rather than from any external state, so callers can reconstruct indexing progress directly from an index — e.g. after pointing the engine at a directory that already contains an index built elsewhere — and compare it against the current library to decide whether re-indexing is needed.

**Returns:** Future<Map<String, int>> - Map from `filePath` to its live document count

---

##### getIndexedFilePaths

```dart
Future<List<String>> getIndexedFilePaths()
```

Returns the distinct `filePath` values present in the index — i.e. which books have at least one live document. Convenience wrapper over `countDocumentsByFilePath()`.

**Returns:** Future<List<String>> - List of indexed file paths (unordered)

---

##### Semantic search

```dart
Future<SemanticVectorsInstallReport> installSemanticVectors({required SemanticVectorsInstallInput input, required SemanticCancellationToken cancellation})
Future<SemanticVectorsInfo> semanticVectorsInfo({required String vectorsDir})
Future<SemanticCompactionReport> compactSemanticVectors({required String vectorsDir, int? liveLibraryVersion, SemanticCompactionPolicy? policy, required SemanticCancellationToken cancellation})
Future<SemanticVectorsVerification> verifySemanticVectors({required String vectorsDir, required SemanticCancellationToken cancellation})
Future<SemanticCoverage> semanticCoverage({required String vectorsDir, required SemanticCancellationToken cancellation})
Future<SemanticStatus> openSemanticArtifact({required SemanticArtifactInput config})
Future<void> disableSemantic()
Future<SemanticStatus> semanticStatus()
Future<SemanticSearchResponse> searchSemantic({..., SemanticRankingOptions? ranking, required SemanticCancellationToken cancellation})

// Development and testing: vectors built on this device.
Future<SemanticStatus> configureSemantic({required SemanticConfigInput config})
Future<SemanticIndexDiff> semanticIndexDiff()
Future<SemanticIndexingSummary> semanticIndexBooks({required List<SemanticBookInput> books})
Future<SemanticRemoveResult> removeSemanticBooks({required List<String> sourceBookKeys})
Future<SemanticResetResult> resetSemanticIndex()
```

These reach the semantic sidecar only in a library built with a semantic
feature; any other build reports an explicit `notInBuild` state and serves
lexical results. Cargokit builds the library with `semantic`, the ONNX backend,
and ONNX is the only model format: GGUF models are not supported, and a model
path that does not end in `.onnx` is refused by its name on every build, as a
`SemanticError` of kind `modelInvalid` whose `field` is `model_path`. The
README's "Semantic search integration" section covers the features, the release
contract, the session lifecycle and the fallback contract. Every one of them but
`semanticStatus` and `disableSemantic` throws a `SemanticError` when it fails
(see "Failures and states" below).

**The application never builds the library's vectors.** The build machine
embeds the library into a release of its vectors; the application installs it
into a vector set with `installSemanticVectors`, opens the set with
`openSemanticArtifact`, and embeds only the query. `configureSemantic` and the
calls below it build vectors on the device, for development and testing, and
are not for the library.

`installSemanticVectors` installs a release into the vector set at
`vectorsDir`, creating the set when there is none: a base replaces whatever the
set holds, and a delta brings it from the library version it stands at to the
next. The set is locked throughout and the new generation goes live in one
flip, so a release refused, cancelled or cut off by a crash leaves the set as it
was; an open session on the same set is moved onto the new generation before
the call returns. One install or compaction of a set runs at a time: while
another runs, in this process or another, the call is refused as
`vectorsBusy` with `field` `vectors_dir`, the set is as it was and an open
session keeps serving; try again once the other has finished.

| field | meaning |
| --- | --- |
| `vectorsDir` | the vector set, `<root>/vectors` |
| `segmentPath` | the release's segment as downloaded: `.oxv`, or `.oxv.zst` compressed with zstd, which is expanded into a file of the install's own in the set's `incoming/` folder first, gone when the call returns. A segment inside `incoming/` is moved into the set when the install succeeds, and stays where it is when it fails (on Windows a read-only one is copied instead); anywhere else it is copied and left |
| `manifestJson` | the release manifest published beside the segment (`release.json`), as published |
| `publishedManifestSha256` | optional: the manifest's SHA-256 as the release publishes it outside the manifest; without it an install detects damage and the wrong release, not one rebuilt to match |
| `modelIdentityJson` | the model identity this installation queries with, as for opening: a release it would not open is not installed |

The report says what the release was (`kind`: `base`, `delta` or `compacted`),
the library version and generation the set stands at, the vectors it added and
the older ones it deleted, the set's size, and whether it wants compacting.
`alreadyApplied` is a delta the set already stood at or past; a base always
replaces the set.

`semanticVectorsInfo` reads what is installed from the set's small files, with
nothing opened or cleaned up: `present: false` when nothing is, and otherwise
its identity digest, library version and release tag, generation, segments,
live and dead vectors, size, `needsCompaction`, and `recoveredFromPrevious` when
the live generation did not open and the one before it was opened instead.

`compactSemanticVectors` merges the set into one segment when its
`SemanticCompactionPolicy` asks for it (more than `maxSegments` segments, deltas
past `maxDeltaRatio` of the base, dead vectors past `maxDeadRatio`, or `force`);
`const SemanticCompactionPolicy()` and `SemanticCompactionPolicy.defaults()` are
the sidecar's defaults, and passing none is passing them. `liveLibraryVersion`
is the library version the open index holds, which the application knows: when
it is the set's own and the index has the `chunkKey` column, every record moves
onto the line that holds its text now (`hintsRefreshed`) and records whose book
no longer holds it are dropped (`recordsPruned`). A compaction refuses to start
without `minFreeSpaceFactor` times its output's size free, as
`insufficientDiskSpace`, and a threshold out of range is `invalidInput` whose
`field` is `policy.<option>`. Locked, crash-safe and cancellable as an install
is; an open session follows it.

`verifySemanticVectors` reads every block of every segment and checks it against
its checksum, the check opening leaves out; it reads the whole set. A damaged
segment is marked so that every later open refuses it, and the call throws
`artifactCorrupt`: installing the release again repairs the set (download it
again if it is gone). On Windows a repair under the same segment fails while a
session holds the set open, since a mapped file cannot be replaced, so close
the session (`disableSemantic`) before installing it. An install that replaced
bytes the check had read, while it read them, is not damage: nothing is
condemned, and the call throws `vectorsBusy`, to verify again once the install
has finished. A cancelled verification records nothing. `semanticCoverage` counts the open index's live lines the
recipe embeds and those the set holds a vector for: one pass over the `chunkKey`
column, or, on an index without it, a read of the whole store.

`openSemanticArtifact` opens the vector set read-only and serves
`searchSemantic` from it, hydrating every result from this index. It compares
every field of the set's identity with this installation's, and nothing in its
input is a value to type in:

| field | meaning |
| --- | --- |
| `vectorsDir` | the vector set, as installed |
| `modelPath` | the model queries are embedded with, an ONNX graph with `tokenizer.json` beside it: for the Meivin model, `seforim-embed-round2-int8.onnx` |
| `modelIdentityJson` | the text of the model's identity file, the one the vectors were built with: the sidecar's `config/models/meivin-round2-onnx/model.json` for the Meivin model. It describes the model family, and the graph at `modelPath` must be one of its `query_packages` |
| `onnxRuntimePath` | optional: the ONNX Runtime library the application ships, the first place the runtime is looked for and, once passed, the only one; not part of any identity, and compared on a repeat call, since a process keeps the first runtime it loads |
| `scanThreads` | optional: how many threads a search scans the set with; null for the sidecar's default, half the cores and at most eight. 0 is `invalidInput` |

The application's installation puts the data folder at `<root>/otzaria/`, with
`seforim.db` and the model package in a folder of its own inside it (the graph,
`tokenizer.json`, and the identity file `model.json`), the lexical index at
`<root>/index/`, and the vector set at `<root>/vectors/`. ONNX Runtime ships
with the application (`onnxRuntimePath`; on macOS inside the signed bundle) or
sits in the model's folder beside the graph, as the build for that machine's
operating system and architecture.

A set's vectors are keyed by the text each was embedded from, not by where it
sits in an index, so nothing ties the set to one index: every search resolves
its hits against the index that is open, by the key of each line's text, which
an index of schema version 5 keeps in its `chunkKey` column. A commit after
opening leaves the set serving. On version 5 a line that moved is found where it
is now, in its book or in another (version 4: see below); a text a book holds in
several places is a line for each, so ungrouped every one is a result. A hit
is at most 32 lines: first one for each book that holds its text — each book
the set records it in, or, when it left them, each book the index holds it in
now; under a filter, also each admitted book it arrived in — then those books'
other lines of it, in that order, while the 32 last; lines past them are not
semantic results of that hit, though lexical search still finds every one. A
hit's lines score alike, so an ungrouped page shows them in that order: a line
of each book before any book's second. A line
whose text is gone, or whose embedded text changed with its neighbours, is not
shown; and every line a search returns, grouped siblings included, is checked
first by recomputing its key from the text the index holds. A semantic match
that fails the check is dropped, or, when the lexical side found the line too,
shown as a lexical result; `fallbackReason` counts them. Under a filter, a text
that moved or was copied into an admitted book since the set was built is found
there, and only there: its vector is weighed at its own score beside the scan of
the admitted books, whose results are exactly what they would be had nothing
moved; that needs version 5 too.

An index of schema version 4 has no `chunkKey` column — the published v30
library index is one. Keys are recomputed from the stored text, which is
slower; every line returned is still held to its whole key, and a passage a book
repeats is still a line for each, by its `lineHash`. A line under 20 characters
is keyed with up to two neighbours on each side, so a short text a book holds
in many places among other lines has its key only where the neighbours repeat
too: once one line of the same `lineHash` is found not to hold the key, the
others are recomputed only where their neighbours' `lineHash`es can spell the
hit's text, and a hit stops after 16 lines that were recomputed and did not
hold its key — which takes neighbours too short to have a `lineHash` (under 12
Hebrew letters), or one before the line that fills the 512-character cap alone
— so such a hit may show fewer of a book's repeats than version 5 does. But a
record's line is found
only at the line the set recorded or within 16 lines of it in the same book: a
line moved further within its book, or a text moved to another book, is not
found, filtered or not; a filter scans the books it admits alone, so a text
moved or copied into one of them is not found under it; and compaction keeps
records as they are (re-anchoring needs the column). This matters only while
the index and the vectors are of different library versions, or after the index
changed on the device (a book added, reindexed or moved); an index and a set of
the same library version agree line for line. A rebuild by this engine gives
version 5. On an opened set the calls that build vectors are refused as
read-only. `SemanticStatus` reports the open set's `vectorsLibraryVersion`,
`vectorSegments` and `needsCompaction`. INT8 vectors from x86 and ARM CPUs meet
at about cosine 0.999, the same order as INT8 against fp32.

`configureSemantic` opens a development session. `SemanticConfigInput` states
how the vectors are produced, and nothing in it is read from the model file.
Every field but `rootDir` and `onnxRuntimePath` is part of the index's identity
(the model file by its checksum, once it has loaded), so an index built under
one value reports `needsFullReindex` under another.

| field | meaning | Meivin ONNX |
| --- | --- | --- |
| `rootDir` | the sidecar's own directory | |
| `modelPath` | the model file, an ONNX graph with `tokenizer.json` beside it | `seforim-embed-round2-int8.onnx` |
| `modelId` | the model's name | `ArieLLL123/judaic-semantic-round2-onnx-zayit` |
| `embeddingDim` | the width of every vector | 256 |
| `pooling` | how one vector is made from each text | `in-graph` |
| `maxTokens` | the token cap per text, as the model's backend counts it; at least 2 and at most 65,536 | 256 |
| `modelQuantization` | the precision of the weights; must not be empty | `int8` (`fp32` for the full-precision graph) |
| `embeddingTextVersion` | the text recipe; 2 prefixes `[PASSAGE] ` to texts and `[QUERY] ` to queries | 2 |
| `onnxRuntimePath` | optional: the ONNX Runtime library to load, as on `SemanticArtifactInput`; not identity | |

**An ONNX model needs the ONNX Runtime shared library at run time.** The
library is loaded, not linked, from the first of three places that is set, and
only from it: `onnxRuntimePath`; else the file named by the
`OTZARIA_ONNX_RUNTIME` environment variable; else the platform's default file
name (`onnxruntime.dll`, `libonnxruntime.so` or `libonnxruntime.dylib`) beside
the `.onnx` graph. A path or a variable that names nothing is refused, never
skipped for the next place, and an empty `onnxRuntimePath` is `invalidInput`.
A process holds one runtime: once one has loaded, a session that names another
is refused as `onnxRuntimeUnusable` until the process restarts. The reference
is Microsoft's official ONNX Runtime 1.28.0 release, and the oldest runtime API
accepted is ONNX Runtime 1.17's; Microsoft's macOS build is arm64 only and needs
macOS 14 (its `LC_BUILD_VERSION` minimum is 14.0), so on macOS 12 and 13, which
this plugin supports, it is `onnxRuntimeUnusable`. Without one that loads,
opening an artifact (or indexing, on the development path) throws an error that
says "ONNX Runtime could not be loaded: …" and what each place held; semantic
search reports itself unavailable, and lexical search is unaffected. That is
not the "No embedding backend is available in this build" of a build without
the backend: the fix is the library, not a rebuild. On macOS, a Hardened Runtime
application loads only libraries signed by Apple or with its own Team ID, so
ship the runtime inside the signed application bundle and pass its path as
`onnxRuntimePath`. The ONNX backend is built for desktop targets (Windows, Linux
and macOS) only.

**Cancelling a search.** `searchSemantic` takes a `SemanticCancellationToken`,
an opaque object with a factory constructor `SemanticCancellationToken()`, a
synchronous `cancel()` and a synchronous `isCancelled`. Create one per search
and cancel it when a newer query supersedes the search. The search borrows the
token rather than moving it, so `cancel()` returns at once on the isolate that
started the search, while the search runs. The search looks at the token before
its lexical phase; the sidecar throughout the semantic half (before and after it
embeds the query, every 1,024 records of the vector scan, before and after
fusion); and the search again before it hydrates the sidecar's results and before
it paints the page, or, for a lexical fallback, once the fallback's page is
ready. At the first look after the cancel it throws `SemanticError` with kind
`cancelled`: not a failure, never answered with lexical results instead, and,
when the sidecar stops it, leaving its caches as they were. A search past its
last look returns its results. A token cannot be reset. It is required, since
flutter_rust_bridge 2.13 cannot pass an optional borrowed object: a fresh token
changes nothing.

**Tuning the ranking.** `searchSemantic` takes an optional `ranking`, a
`SemanticRankingOptions` holding every parameter hybrid ranking runs on, in place
of the ranking a search runs on without it. Its constructor's defaults are that
ranking, so a caller names only what it changes, and
`SemanticRankingOptions.defaults()` reads them from the engine; passing them is
passing nothing. **The defaults are unmeasured placeholders**: calibrating them
needs a labelled relevance set and a metric over the page, and this option exists
so that can happen from the application without a release of the engine.

| option | default | allowed | what it does |
| --- | --- | --- | --- |
| `fusionStrategy` | `weighted` | `weighted`, `rrf`, `adaptive` | by weight; by rank, `1 / (rrfK + rank)` from each side; or by weight with BM25 min-max normalized when its scores run high |
| `rrfK` | 60 | at least 1, with `rrf` | RRF's `k`; read by nothing else |
| `alphaOverride` | null | 0 to 1 | one lexical weight for every query |
| `alphaByQueryType` | quoted phrase 1, reference 0.85, one or two words 0.7, three or four 0.5, five or more 0.3, none 0.5 | each 0 to 1 | the lexical weight per kind of query; `1 - alpha` is the semantic side's |
| `bm25SaturationK` | 10 | above 0 | `k` in BM25's normalization `score / (k + score)` |
| `semanticThreshold` | 0 | 0 to 1 | below this normalized similarity a semantic candidate counts for nothing |
| `agreementBonus` | 0.1 | 0 to 1 | added to a line both sides found, fused by weight |
| `phraseMatchBonus`, `rareTermBonus` | 0 | 0 to 1 | scaled by the share of the query's quoted phrases, or rare words, a line contains |
| `sectionCoverageBonus` | 0 | 0 to 1 | added to a line whose section holds another result |
| `duplicatePenalty` | 0 | 0 to 1 | taken from each later line with the same text |
| `metadataRankingEnabled` | false | | a semantic candidate's book and facets add to its score |
| `candidateWindowMultiplier` | 2 | 1 to 10 | semantic candidates fetched for each place in the window |

A value outside its range, or not a number, is refused before the search runs,
with or without a session: `invalidInput`, whose `field` is the option's name in
the Rust struct (`alpha_by_query_type.short`, `rrf_k`). A build without semantic
support ignores the options.

**Failures and states.** A failed semantic call throws `SemanticError`, an
`FrbException` with three fields: `kind`, a `SemanticErrorKind` to branch on;
`message`, the detailed text the call has always produced; and `field`, the
field the failure is about when it is about one. Before, these calls threw
`AnyhowException` with the same text. The status and the search envelope carry
kinds too:

| type | field | meaning |
| --- | --- | --- |
| `SemanticStatus` | `state` | a `SemanticState`: `notInBuild`, `notConfigured`, `ready`; and for a development session `empty` (nothing indexed yet), `needsReindex` or `failed` |
| `SemanticStatus` | `errorKind` | the kind of `lastError`, non-null exactly when it is; a development session's sidecar reports its failures as text only, so they are `internal` here |
| `SemanticSearchResponse` | `fallbackKind` | why the semantic path did not serve the search, when it was asked to: `notConfigured`, `featureNotInBuild` or `queryFailed`; null when it served it, or was not asked |

A kind is decided from the type of the failure, never from its message. Where
one type covers two states, a fact decides: a set with neither `CURRENT` nor
`PREVIOUS` makes unusable metadata a missing set rather than a damaged one, and a file where ONNX Runtime is looked
for makes a runtime that did not load unusable rather than missing. The
installation's own identity values are checked first, by the sidecar's own
functions, so a value no build serves is `invalidInput`, and what opening refuses
after that is the set's, the model's or the runtime's. More kinds will be
added: a `switch` needs a default branch, which is best treated as `internal`.

| kind | thrown by, or reported in | means | what to do |
| --- | --- | --- | --- |
| `notConfigured` | `state`, `fallbackKind` | no session is open | open the vector set |
| `featureNotInBuild` | `state`, `fallbackKind` | no semantic support in this build | hide semantic search |
| `artifactMissing` | `openSemanticArtifact`, `verifySemanticVectors`, `semanticCoverage` | nothing at `vectorsDir`, or nothing ever installed there (no `CURRENT` or `PREVIOUS`) | download and install the vectors |
| `artifactCorrupt` | opening, installing, verifying, `semanticVectorsInfo` | a set whose pointers, metadata or segments do not open or fail their checksums, an identity left unfilled (`field`), or a release whose segment is not the one its manifest describes | install the release again, which repairs the set (download it again if it is gone); on Windows, close the session first |
| `artifactIncompatible` | opening, installing | sound vectors built for something else; `field` is the first field that disagreed: `text.line_text_version`, `model.family_id`, `model.chunking_identity`, `store.store_format_version`, `store.vector_precision`, `metadata_version`, or for a delta that does not follow the set `delta.*`. Installing, `field` `segment_id`: the release is a version the set has installed, published again with other bytes; the set is sound and keeps what it serves | install the vectors built for this application and model; for `segment_id`, do not download it again, which is refused the same way: keep the set, or install the release into a new, empty `vectorsDir` and open that |
| `artifactNotPublished` | `installSemanticVectors` | the manifest is not the one whose digest was published | download the official release |
| `insufficientDiskSpace` | installing, compacting | more free space is needed than the device has | free space, and try again |
| `modelMissing` | `openSemanticArtifact`, `semanticIndexBooks` | no file at `modelPath` | download the model |
| `tokenizerMissing` | `openSemanticArtifact`, `semanticIndexBooks` | an ONNX graph without `tokenizer.json` beside it | install the whole package |
| `modelInvalid` | `openSemanticArtifact`, `semanticIndexBooks` | not a usable model, or its backend could not load it; or, with `field` `model_path`, a path that names no ONNX graph, such as a GGUF | download the model again; for a path, point it at the package's `.onnx` graph |
| `modelIdentityMismatch` | `openSemanticArtifact`, `semanticIndexBooks` | the identity does not describe the model; `field`: `query_packages`, `tokenizer_checksum`, `embedding_dim` or `pooling` | ship the matching identity file or model |
| `onnxRuntimeMissing` | `openSemanticArtifact`, `semanticIndexBooks` | no runtime where one is looked for: at `onnxRuntimePath` when it is passed | provide ONNX Runtime there |
| `onnxRuntimeUnusable` | `openSemanticArtifact`, `semanticIndexBooks` | a runtime file that does not load, is too old, or is not the one already loaded | replace it, or restart |
| `backendNotInBuild` | `openSemanticArtifact`, `semanticIndexBooks` | no ONNX backend in this build, as on Android and iOS | a desktop build |
| `sessionConflict` | `configureSemantic`, `openSemanticArtifact` | another session, or other inputs, is open | `disableSemantic` first |
| `readOnlySession` | `semanticIndexBooks`, `semanticIndexDiff`, `removeSemanticBooks`, `resetSemanticIndex` | a build-side call on an opened vector set | nothing |
| `reindexRequired` | `semanticIndexBooks` | a development session holds vectors from another configuration | `resetSemanticIndex`, index again |
| `queryFailed` | `fallbackKind` | the semantic half of one search failed | show the lexical results |
| `cancelled` | `searchSemantic`, and the calls that install, compact, verify or count | its `SemanticCancellationToken` was cancelled before it finished | drop it: nothing failed, and nothing changed |
| `invalidInput` | `configureSemantic`, `openSemanticArtifact`, `installSemanticVectors`, `searchSemantic`, `compactSemanticVectors` | a value the call cannot take; `field` when known (`model_quantization`, `max_tokens`, `model_identity_json`, `pooling`, `embedding_text_version`, `normalization_version`, `vectors_dir`, `segment_path`, `onnx_runtime_path`, `scan_threads`, a ranking option such as `alpha_by_query_type.short` or `rrf_k`, or `policy.<option>`) | fix the call |
| `internal` | any | an I/O error or a fault, including the lexical index failing under `searchSemantic` | report `message` |
| `vectorsBusy` | `installSemanticVectors`, `compactSemanticVectors`, `verifySemanticVectors` | another install or compaction of the set at `vectorsDir` is running, in this process or another (`field` `vectors_dir`); nothing was read or changed, and an open session keeps serving. Verifying: an install replaced bytes the check had read, and nothing was condemned | try again once it has finished; never `disableSemantic` for it |

---

## Top-Level Functions

### checkIndexCompatibility

```dart
IndexCompatibility checkIndexCompatibility({required String path})
```

Checks whether an existing index is compatible with the current search engine schema.

The engine writes an `otzaria_index_meta.json` sidecar file next to compatible indexes when they are opened. For older indexes without that sidecar, this function falls back to Tantivy's `meta.json` and verifies its full schema against the schemas this engine reads.

This engine reads schema versions 4 and 5, and creates 5. A version 4 index is `compatible` and needs no rebuild: it opens, searches and takes books as it always did, and stays version 4. It lacks only the `chunkKey` column, which only an index this engine creates has.

**Parameters:**
- `path` (String): File system path of the Tantivy index directory

**Returns:** IndexCompatibility

Common `status` values:
- `compatible`: Otzaria metadata exists, declares a schema version this engine reads, and the index has that version's schema
- `legacy_compatible`: Otzaria metadata is missing, but the full Tantivy schema is one this engine reads
- `rebuild_required`: The index schema is older than version 4, or is not the schema its version has, and should be rebuilt
- `engine_too_old`: The index schema is newer than this engine supports
- `missing_index`: The index directory does not exist
- `invalid_index_path`: The given path is not a valid directory path

---

## Data Models

### SearchResult

Result returned from SearchEngine.search()

**Fields:**
```dart
class SearchResult {
  String title;        // Document title
  String reference;    // Document reference/citation
  String text;         // Highlighted snippet or full text
  int id;              // Document ID (u64)
  int segment;         // Segment number (u64)
  bool isPdf;          // Whether document is PDF
  String filePath;     // Path to document file
}
```

**Note:** The `text` field contains a snippet with HTML highlighting when matches are found. Highlights are wrapped in `<font color=red>...</font>` tags by default (configurable via `HighlightConfig`). If no snippet is generated, it contains the full document text.

---

### SearchPageResult

Result returned from the `searchAndCount*` family.

**Fields:**
```dart
class SearchPageResult {
  List<SearchResult> results;  // The requested page
  int totalCount;              // Total hits for the query
}
```

---

### HighlightConfig

Optional snippet/highlight configuration accepted by `search`, `searchAndCount`, `searchStream` and `searchFuzzyTerms`.

**Fields:**
```dart
class HighlightConfig {
  String highlightPrefix;   // default: "<font color=red>"
  String highlightPostfix;  // default: "</font>"
  int maxChars;             // snippet length budget, default: 800
}
```

---

### IndexCompatibility

Result returned from `checkIndexCompatibility()`.

**Fields:**
```dart
class IndexCompatibility {
  bool compatible;             // Whether the current engine can use this index
  String status;               // Machine-readable status
  int? foundSchemaVersion;     // Version found in metadata, when known
  int requiredSchemaVersion;   // Version this engine creates (it also reads 4)
  String engineVersion;        // Rust engine package version
  String metadataPath;         // Expected otzaria_index_meta.json path
  String? reason;              // Human-readable detail for non-trivial states
}
```

Compatibility is controlled by the schema version, not by the package release number: `compatible` is the answer, and `foundSchemaVersion` below `requiredSchemaVersion` is not a reason to rebuild by itself. A patch release can keep the same schema version when no rebuild is required.

---

### ResultsOrder

Enum specifying the sort order for search results.

**Values:**
```dart
enum ResultsOrder {
  Catalogue,   // Sort by document ID (ascending)
  Relevance    // Sort by search relevance score (descending)
}
```

---

## Usage Examples

### Full-Text Search Example

```dart
// Initialize search engine
final searchEngine = SearchEngine.new('/path/to/index');

// Add documents
await searchEngine.addDocument(
  1,
  'Example Book',
  'Chapter 1',
  'category/subcategory',
  'This is the full text content to be indexed',
  1,
  false,
  '/path/to/book.txt'
);

await searchEngine.commit();

// Search with regex
final results = await searchEngine.search(
  ['text', 'content'],  // Search for these terms
  ['category'],          // Filter by topic
  10,                    // Limit to 10 results
  2,                     // Allow 2 words between terms
  1000,                  // Max regex expansions
  ResultsOrder.Relevance
);

// Count matching documents
final count = await searchEngine.count(
  ['text'],
  ['category'],
  0,
  1000
);
```

## Implementation Notes

### Regex Patterns

The SearchEngine uses Rust regex syntax. Common patterns:
- `.` - matches any character
- `.*` - matches any sequence
- `\w+` - matches word characters
- `[אבגד]` - character class (matches any of these Hebrew letters)
- `(pattern1|pattern2)` - alternation

### Topic Facets

Topics use hierarchical facet notation with forward slashes:
- `"category"` - top level
- `"category/subcategory"` - nested
- `"category/subcategory/item"` - deep nesting

Documents match a facet query if their topic starts with any of the specified facets.

### Dimension Facets (author / era / base)

Alongside the category path, a document can carry extra facet values supplied at
indexing time (`extraFacets` on `addTextBook`/`addTextBookBytes`/`addPdfBook`/
`addDocument`/`upsertDocument`, or `DocumentInput.extraFacets`). Three English
roots are reserved as *filter dimensions* (they cannot collide with Hebrew
category names): `/author/<name>`, `/era/<period>`, `/base` (foundational books).

Filtering semantics (`facets` parameter, all search/count functions): the facet
list is partitioned by root — each reserved dimension is its own group, and all
other paths (the category tree) form one group. Within a group facets are OR'ed
(prefix match, as before); across groups they are AND'ed. So
`["/תנך", "/era/ראשונים"]` = "in תנ"ך AND by a rishon", while
`["/era/ראשונים", "/era/אחרונים"]` = "either era". A call with only category
facets behaves exactly as before.

### Result Grouping (deduplication)

The exact/advanced/fuzzy `search*`, `searchAndCount*`, `search*Stream` and
`search*StreamWithCounts` functions accept an optional `grouping` parameter
(`ResultGrouping?`, default null = flat results):

- `sameSection` — hits under the same heading in the same book (same
  `sectionId`; a PDF page is a section) collapse into one result with a count.
- `identicalText` — lines whose Hebrew letters are identical (punctuation,
  spacing and quotes ignored; see `lineHash`) collapse across books. Lines
  shorter than 12 Hebrew letters never merge.

When grouping is active: `limit`/`offset` count *groups*; each returned
`SearchResult` is the group's best representative (by the requested order) and
carries `mergedCount` (total group size) plus `merged` (up to 10 sibling
locations — title/reference/id/segment/isPdf/filePath, no snippet).
`SearchPageResult.groupCount` / the first `SearchStreamUpdate.groupCount` report
the total number of groups; `totalCount` and `bookCounts` remain raw hit counts.

### Index Persistence

The search engine stores its index on disk at the specified path. The index persists between application runs and can be reused without rebuilding.

### Thread Safety

SearchEngine is designed to be used from a single thread. If you need concurrent access, create separate instances or implement your own locking mechanism.

### Memory Considerations

The search engine uses memory-mapped files (MmapDirectory) for efficient index access. The index writer is initialized with a 50MB buffer (`50_000_000` bytes).

---

## Language Implementation Guide

When implementing this API in other languages:

1. **Index Format**: Use Tantivy-compatible index format or implement a translation layer
2. **Regex Engine**: Ensure regex engine supports similar syntax to Rust's regex crate
3. **Field Types**: Map types appropriately:
   - `u64` → unsigned 64-bit integer
   - `String` → UTF-8 string
   - `bool` → boolean
4. **Snippet Highlighting**: Implement HTML snippet generation with configurable highlight tags
5. **Facet Structure**: Implement hierarchical facet matching with `/` delimiter
6. **Async/Sync**: Constructor is synchronous, all other methods are asynchronous

---

## Version Information

This documentation is based on the current implementation as of the latest commit. 
Check the repository for updates and changes to the API.
