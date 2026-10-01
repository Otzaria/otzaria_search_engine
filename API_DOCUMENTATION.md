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
embeds the library into an artifact; the application opens it with
`openSemanticArtifact` and embeds only the query. `configureSemantic` and the
calls below it build vectors on the device, for development and testing, and
are not for the library.

`openSemanticArtifact` opens a prebuilt artifact read-only and serves
`searchSemantic` from it, hydrating every result from this index. It compares
every field of the artifact's identity with this installation's, and nothing in
its input is a value to type in:

| field | meaning |
| --- | --- |
| `artifactDir` | the artifact directory the build binary wrote |
| `modelPath` | the model queries are embedded with, an ONNX graph with `tokenizer.json` beside it: for the Meivin model, `seforim-embed-round2-int8.onnx` |
| `modelIdentityJson` | the text of the model's identity file, the one the artifact was built with: the sidecar's `config/models/meivin-round2-onnx/model.json` for the Meivin INT8 graph |
| `publishedDigest` | optional: the artifact's digest as published outside it |
| `onnxRuntimePath` | optional: the ONNX Runtime library the application ships, the first place the runtime is looked for and, once passed, the only one; not part of any identity, and compared on a repeat call, since a process keeps the first runtime it loads |

The application's installation puts the data folder at `<root>/otzaria/`, with
`seforim.db` and the model package in a folder of its own inside it (the graph,
`tokenizer.json`, and the identity file `model.json`), the lexical index at
`<root>/index/`, and the vectors artifact in a folder of its own beside
`index/`. ONNX Runtime ships with the application (`onnxRuntimePath`; on macOS
inside the signed bundle) or sits in the model's folder beside the graph, as the
build for that machine's operating system and architecture.

The corpus half of the identity is not an input: it is the corpus stamp the
build machine writes into the lexical index (`--stamp-index`), checked against
the index's segment set, because nothing on a device can recompute `corpus_id`.
A mismatch anywhere is an error naming the fields, and leaves nothing open. On
an opened artifact the calls that build vectors are refused as read-only, and a
commit to the index afterwards makes it stale: searches fall back to lexical
results, and `semanticStatus` reports why. INT8 vectors from x86 and ARM CPUs
meet at about cosine 0.999, the same order as INT8 against fp32.

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
| `SemanticStatus` | `state` | a `SemanticState`: `notInBuild`, `notConfigured`, `ready`, `stale`; and for a development session `empty` (nothing indexed yet), `needsReindex` or `failed` |
| `SemanticStatus` | `errorKind` | the kind of `lastError`, non-null exactly when it is; a development session's sidecar reports its failures as text only, so they are `internal` here |
| `SemanticSearchResponse` | `fallbackKind` | why the semantic path did not serve the search, when it was asked to: `notConfigured`, `featureNotInBuild`, `artifactStale` or `queryFailed`; null when it served it, or was not asked |

A kind is decided from the type of the failure, never from its message. Where
one type covers two states, a fact decides: a missing `manifest.json` makes
unusable metadata a missing artifact, and a file where ONNX Runtime is looked
for makes a runtime that did not load unusable rather than missing. The
installation's own identity values are checked first, by the sidecar's own
functions, so a value no build serves is `invalidInput`, and what opening refuses
after that is the artifact's, the model's or the runtime's. More kinds will be
added: a `switch` needs a default branch, which is best treated as `internal`.

| kind | thrown by, or reported in | means | what to do |
| --- | --- | --- | --- |
| `notConfigured` | `state`, `fallbackKind` | no session is open | open the artifact |
| `featureNotInBuild` | `state`, `fallbackKind` | no semantic support in this build | hide semantic search |
| `artifactMissing` | `openSemanticArtifact` | no directory, or no `manifest.json` in it | download the artifact |
| `artifactCorrupt` | `openSemanticArtifact` | damaged metadata or payload, or an identity left unfilled (`field`) | download it again |
| `artifactIncompatible` | `openSemanticArtifact` | built for another corpus, model or store format; `field` is the first identity field that disagreed (`corpus.library_version`, `model.model_id`, `store.store_format_version`, `metadata_version`) | install the artifact built for this release |
| `artifactNotPublished` | `openSemanticArtifact` | its digest is not the published one | download the official artifact |
| `artifactStale` | `state: stale`, `fallbackKind` | the index was committed to after opening | `disableSemantic`, open the matching pair |
| `indexNotStamped` | `openSemanticArtifact` | no corpus stamp this build reads | install the release's index |
| `indexStampMismatch` | `openSemanticArtifact` | the index changed after it was stamped | install the release's index |
| `modelMissing` | `openSemanticArtifact`, `semanticIndexBooks` | no file at `modelPath` | download the model |
| `tokenizerMissing` | `openSemanticArtifact`, `semanticIndexBooks` | an ONNX graph without `tokenizer.json` beside it | install the whole package |
| `modelInvalid` | `openSemanticArtifact`, `semanticIndexBooks` | not a usable model, or its backend could not load it; or, with `field` `model_path`, a path that names no ONNX graph, such as a GGUF | download the model again; for a path, point it at the package's `.onnx` graph |
| `modelIdentityMismatch` | `openSemanticArtifact`, `semanticIndexBooks` | the identity does not describe the model; `field`: `model_checksum`, `embedding_backend`, `embedding_dim` or `pooling` | ship the matching identity file or model |
| `onnxRuntimeMissing` | `openSemanticArtifact`, `semanticIndexBooks` | no runtime where one is looked for: at `onnxRuntimePath` when it is passed | provide ONNX Runtime there |
| `onnxRuntimeUnusable` | `openSemanticArtifact`, `semanticIndexBooks` | a runtime file that does not load, is too old, or is not the one already loaded | replace it, or restart |
| `backendNotInBuild` | `openSemanticArtifact`, `semanticIndexBooks` | no ONNX backend in this build, as on Android and iOS | a desktop build |
| `sessionConflict` | `configureSemantic`, `openSemanticArtifact` | another session, or other inputs, is open | `disableSemantic` first |
| `readOnlySession` | `semanticIndexBooks`, `semanticIndexDiff`, `removeSemanticBooks`, `resetSemanticIndex` | a build-side call on an opened artifact | nothing |
| `reindexRequired` | `semanticIndexBooks` | a development session holds vectors from another configuration | `resetSemanticIndex`, index again |
| `queryFailed` | `fallbackKind` | the semantic half of one search failed | show the lexical results |
| `cancelled` | `searchSemantic` | its `SemanticCancellationToken` was cancelled before it finished | drop it: nothing failed |
| `invalidInput` | `configureSemantic`, `openSemanticArtifact`, `searchSemantic` | a value the call cannot take; `field` when known (`model_quantization`, `max_tokens`, `model_identity_json`, `pooling`, `embedding_text_version`, `normalization_version`, `artifact_dir`, `onnx_runtime_path`, or a ranking option: `alpha_by_query_type.short`, `rrf_k`, …) | fix the call |
| `internal` | any | an I/O error or a fault, including the lexical index failing under `searchSemantic` | report `message` |

---

## Top-Level Functions

### checkIndexCompatibility

```dart
IndexCompatibility checkIndexCompatibility({required String path})
```

Checks whether an existing index is compatible with the current search engine schema.

The engine writes an `otzaria_index_meta.json` sidecar file next to compatible indexes when they are opened. For older indexes without that sidecar, this function falls back to Tantivy's `meta.json` and verifies the current required schema shape.

**Parameters:**
- `path` (String): File system path of the Tantivy index directory

**Returns:** IndexCompatibility

Common `status` values:
- `compatible`: Otzaria metadata exists and matches the current schema version
- `legacy_compatible`: Otzaria metadata is missing, but the full Tantivy schema matches the current engine
- `rebuild_required`: The index schema is older or incompatible and should be rebuilt
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
  int requiredSchemaVersion;   // Version required by this engine
  String engineVersion;        // Rust engine package version
  String metadataPath;         // Expected otzaria_index_meta.json path
  String? reason;              // Human-readable detail for non-trivial states
}
```

Compatibility is controlled by `requiredSchemaVersion`, not by the package release number. A patch release can keep the same schema version when no rebuild is required.

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
