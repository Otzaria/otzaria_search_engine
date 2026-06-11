# Otzaria Search Engine - API Documentation

This document describes the API exposed by the Otzaria Search Engine through Flutter/Dart bindings. The API is generated from Rust code using flutter_rust_bridge.

## Table of Contents

1. [Classes](#classes)
   - [SearchEngine](#searchengine)
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
- `maxExpansions` (int/u32): Maximum number of regex expansions allowed; exceeding it returns an error
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
})
```

Each mode also provides `count*`, `searchAndCount*` and `search*Stream`
variants (`countExact`, `searchAndCountFuzzy`, `searchAdvancedStream`, …).
Queries are normalized like the index (nikud stripped, lowercased); empty
queries return no results. Advanced-mode highlighting wraps every
morphological variant that actually matched, not just the literal words.

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
