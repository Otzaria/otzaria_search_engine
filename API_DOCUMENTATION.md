# Otzaria Search Engine - API Documentation

This document describes the API exposed by the Otzaria Search Engine through Flutter/Dart bindings. The API is generated from Rust code using flutter_rust_bridge.

## Table of Contents

1. [Classes](#classes)
   - [SearchEngine](#searchengine)
   - [ReferenceSearchEngine](#referencesearchengine)
2. [Data Models](#data-models)
   - [SearchResult](#searchresult)
   - [ReferenceSearchResult](#referencesearchresult)
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
Future<List<SearchResult>> search(
  List<String> regexTerms,
  List<String> facets,
  int limit,
  int slop,
  int maxExpansions,
  ResultsOrder order
)
```

Performs a search query on the index using regex patterns.

**Parameters:**
- `regexTerms` (List<String>): List of regex patterns to search for
  - Single term: uses RegexQuery
  - Multiple terms: uses RegexPhraseQuery with specified slop
- `facets` (List<String>): List of topic facets to filter by (must match document topics)
- `limit` (int/u32): Maximum number of results to return
- `slop` (int/u32): Maximum distance between terms in phrase queries (for multi-term searches)
- `maxExpansions` (int/u32): Maximum number of regex expansions allowed
- `order` (ResultsOrder): Sort order for results (Catalogue or Relevance)

**Returns:** Future<List<SearchResult>>

---

##### count

```dart
Future<int> count(
  List<String> regexTerms,
  List<String> facets,
  int slop,
  int maxExpansions
)
```

Counts the number of documents matching the search criteria without retrieving them.

**Parameters:**
- `regexTerms` (List<String>): List of regex patterns to search for
- `facets` (List<String>): List of topic facets to filter by
- `slop` (int/u32): Maximum distance between terms in phrase queries
- `maxExpansions` (int/u32): Maximum number of regex expansions allowed

**Returns:** Future<int> - Number of matching documents

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

### ReferenceSearchEngine

Specialized search engine for searching document references/citations.

#### Constructor

```dart
ReferenceSearchEngine.new(String path)
```

**Synchronous** constructor that creates a new reference search engine instance.

**Parameters:**
- `path` (String): File system path where the reference index will be stored

**Returns:** ReferenceSearchEngine instance

---

#### Methods

##### addDocument

```dart
Future<void> addDocument(
  int id,
  String title,
  String reference,
  String shortRef,
  int segment,
  bool isPdf,
  String filePath
)
```

Adds a document reference to the index.

**Parameters:**
- `id` (int/u64): Unique document identifier
- `title` (String): Document title
- `reference` (String): Full reference/citation text
- `shortRef` (String): Short/abbreviated reference (used for searching)
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
Future<List<ReferenceSearchResult>> search(
  String query,
  int limit,
  bool fuzzy,
  ResultsOrder order
)
```

Searches for references matching the query.

**Parameters:**
- `query` (String): Search query string
- `limit` (int/u32): Maximum number of results to return
- `fuzzy` (bool): Enable fuzzy matching (allows 1 character difference)
- `order` (ResultsOrder): Sort order for results (Catalogue or Relevance)

**Returns:** Future<List<ReferenceSearchResult>>

---

##### count

```dart
Future<int> count(
  String query,
  bool fuzzy
)
```

Counts the number of references matching the query without retrieving them.

**Parameters:**
- `query` (String): Search query string
- `fuzzy` (bool): Enable fuzzy matching

**Returns:** Future<int> - Number of matching references

---

##### clear

```dart
Future<void> clear()
```

Removes all references from the index.

**Returns:** Future<void>

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

**Note:** The `text` field contains a snippet with HTML highlighting when matches are found. Highlights are wrapped in `<font color=red>...</font>` tags. If no snippet is generated, it contains the full document text.

---

### ReferenceSearchResult

Result returned from ReferenceSearchEngine.search()

**Fields:**
```dart
class ReferenceSearchResult {
  String title;        // Document title
  String reference;    // Full reference text
  String shortRef;     // Short reference
  int id;              // Document ID (u64)
  int segment;         // Segment number (u64)
  bool isPdf;          // Whether document is PDF
  String filePath;     // Path to document file
}
```

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

### Reference Search Example

```dart
// Initialize reference search engine
final refEngine = ReferenceSearchEngine.new('/path/to/ref_index');

// Add reference
await refEngine.addDocument(
  1,
  'Torah',
  'Genesis 1:1',
  'Gen 1:1',
  1,
  false,
  '/path/to/genesis.txt'
);

await refEngine.commit();

// Search references
final results = await refEngine.search(
  'Genesis 1',
  10,
  false,  // Exact matching
  ResultsOrder.Catalogue
);

// Search with fuzzy matching
final fuzzyResults = await refEngine.search(
  'Genesi',  // Will match 'Genesis'
  10,
  true,   // Enable fuzzy matching
  ResultsOrder.Catalogue
);
```

---

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

Both search engines store their indices on disk at the specified path. The indices persist between application runs and can be reused without rebuilding.

### Thread Safety

Both SearchEngine and ReferenceSearchEngine are designed to be used from a single thread. If you need concurrent access, create separate instances or implement your own locking mechanism.

### Memory Considerations

The search engines use memory-mapped files (MmapDirectory) for efficient index access. The index writer is initialized with a 50MB buffer (`50_000_000` bytes).

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
