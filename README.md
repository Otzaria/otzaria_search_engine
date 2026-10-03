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

- `semantic` is the production build, and the one Cargokit builds: the ONNX
  backend (`semantic-onnx`), for the Meivin model the application uses. ONNX
  is the only model format there is a backend for.
- GGUF models and `llama.cpp` are not supported. No feature compiles
  `llama.cpp` or ggml, and a model path that does not end in `.onnx`, a GGUF
  included, is refused by its name rather than served: opening a vector set
  with it, or loading it on the development path, fails with `modelInvalid`,
  whose `field` is `model_path`.
- The sidecar builds the ONNX backend for desktop targets only (Windows, Linux
  and macOS) and compiles it out elsewhere. The feature stays on and the build
  succeeds, so Android and iOS have no backend behind it, and there the
  production build serves no model at all; see "Builds without a backend".
- `semantic-mock` selects the deterministic test backend and must not be used
  in an application release. CI builds the library with it so the Dart FFI
  suite can drive a configured sidecar.
- The dependency is pinned in `rust/Cargo.toml` and `rust/Cargo.lock`; a moving
  branch must not be used because model and vector-index identity depend on the
  exact implementation.

**The application never builds the library's vectors.** The build machine
embeds the whole library, from the lexical index the release ships, into a
release of its vectors; the application installs that release into a vector set
with `installSemanticVectors`, opens the set with `openSemanticArtifact`, and
embeds nothing but the query. `configureSemantic`, `semanticIndexBooks` and the
other calls that build vectors on the device are development and testing
scaffolding, described at the end; they are not for the library.

### Installing and opening the library's vectors

```dart
final vectorsDir = '$root/vectors';
final identityJson =
    await File('$root/otzaria/meivin/model.json').readAsString();
// A release is a segment and its manifest, and the manifest's SHA-256
// published beside them.
await engine.installSemanticVectors(
  input: SemanticVectorsInstallInput(
    vectorsDir: vectorsDir,
    segmentPath: downloadedSegment.path, // `.oxv`, or `.oxv.zst`
    manifestJson: await downloadedManifest.readAsString(),
    publishedManifestSha256: release.manifestSha256,
    modelIdentityJson: identityJson,
  ),
  cancellation: SemanticCancellationToken(),
);
final status = await engine.openSemanticArtifact(
  config: SemanticArtifactInput(
    vectorsDir: vectorsDir,
    modelPath: '$root/otzaria/meivin/seforim-embed-round2-int8.onnx',
    // The model's identity file, from the model's folder or the app's assets.
    modelIdentityJson: identityJson,
    // The ONNX Runtime the application ships; leave it out to use the one
    // beside the graph (see "The ONNX Runtime library").
    onnxRuntimePath: bundledOnnxRuntimePath,
  ),
);
```

A vector set holds the library's vectors keyed by the text each was embedded
from, not by where that text sits in the index. A base release replaces what the
set holds; a delta brings it from the library version it stands at to the next.
An install is locked and crash-safe, the new generation goes live in one flip,
and a release that is refused, cancelled or cut off leaves the set as it was; an
open session on the same set moves onto the new generation. What a release must
agree with:

| what | written by | compared with | a mismatch means |
| --- | --- | --- | --- |
| the line recipe | the build: the index's line text and the chunk key over it | this build's (`LINE_TEXT_VERSION`, the key version, and the chunking compiled in) | vectors of other text: install the release built for this application |
| the model identity, `modelIdentityJson` | the model's publisher: the sidecar's `config/models/meivin-round2-onnx/model.json` for the Meivin model, the file the vectors were built with | the release's model family, and on opening the model at `modelPath`: its package among `query_packages`, and once it has loaded its `tokenizer_checksum` | another model, or an identity file that describes other weights |
| the store | the build | the format and codec this build reads | a release of a newer or older store format |
| `publishedManifestSha256` | the build prints it; the release publishes it outside the manifest | the manifest's SHA-256 | a release rebuilt to look like the published one |
| a delta's starting point | the build | the library version the set stands at | a delta that does not follow the set: install the base, or the deltas in between |

Nothing ties a set to one index: every search resolves the set's hits against
the index that is open, by the key of each line's text, which a new index keeps
in its `chunkKey` column. A commit after opening leaves the set serving; a line
that moved is found where it is now, in its book or in another; a text a book
holds in several places is a line for each, up to 32 lines a hit — one for each
book that holds it first, then those books' other lines of it; a line whose text
is gone, or whose
embedded text changed with its neighbours, is not shown; and every line a search
returns, a grouped sibling as much as a result, is checked by recomputing its key
from the text the index holds. Under a filter, a text that moved or was copied
into a book the filter admits since the set was built is found there, and only
there: its vector is weighed at its own score beside the scan of the admitted
books, which is not widened, so their results are exactly what they would be
had nothing moved. An index of schema version 4, without the column, is served the
same way, by recomputing the keys of the books a hit names, which is slower; a
filter there scans the books it admits alone.

Keeping the set, all on `SearchEngine` and all cheap except where noted:

| call | does |
| --- | --- |
| `semanticVectorsInfo` | what is installed: library version, generation, segments, live and dead vectors, size, and whether it wants compacting; `present: false` when nothing is |
| `compactSemanticVectors` | merges the set into one segment when its policy (`SemanticCompactionPolicy`, the sidecar's defaults) asks, or when forced; given the open index's library version, moves every record onto the line that holds its text now. Needs about the set's size free |
| `verifySemanticVectors` | reads every block of every segment against its checksum, the check opening leaves out; a damaged segment is marked and refused from then on. Reads the whole set |
| `semanticCoverage` | the live lines the recipe embeds, and how many the set holds a vector for. One pass over the `chunkKey` column, or, without it, a read of the whole store |

`SemanticStatus` reports the open set's `vectorsLibraryVersion`,
`vectorSegments` and `needsCompaction`.

Every refusal names every field that disagreed, and leaves the set and the
session as they were. It is a `SemanticError`, whose `kind` says which refusal
it was (see "Telling failures apart" below). An opened set is read-only:
`semanticIndexBooks`, `removeSemanticBooks`, `resetSemanticIndex`,
`semanticIndexDiff` and `configureSemantic` are refused by name. Opening the same
set again is a no-op; opening another needs `disableSemantic` first. Opening does
not hold the engine's write lock, so lexical search keeps serving while the model
and the vectors load.

INT8 vectors depend on the CPU's INT8 kernels: ARM (KleidiAI) and x86 (MLAS)
land about cosine 0.999 apart, the same order as INT8 against fp32. A library
built on x86 and queried on an ARM Mac therefore meets at about 0.999. The
sidecar records this as accepted, and as a measurement still to be made on a
weak PC.

### Telling failures apart

A semantic call that fails throws a `SemanticError`, where it used to throw
`AnyhowException`. Its `kind`, a `SemanticErrorKind`, is what to branch on;
`message` is the detailed text, as before, for a developer; and `field` names
the field the failure is about, when it is about one. Beside them,
`SemanticStatus.state` says what the session can do, `SemanticStatus.errorKind`
is the kind of `lastError`, and `SemanticSearchResponse.fallbackKind` says why a
search fell back to lexical results. The kind is decided from the type of the
failure, the sidecar's typed errors and the engine's own, and never from a
message, so rewording a message cannot change it.

On the application's path:

| kind | reported by | means, and what to do |
| --- | --- | --- |
| `artifactMissing` | `openSemanticArtifact`, `verifySemanticVectors`, `semanticCoverage` | nothing installed at `vectorsDir`: download and install the vectors |
| `artifactCorrupt` | opening, installing, verifying | a damaged set (its pointers, metadata or a segment), or a release whose segment is not the one its manifest describes: install the release again, which repairs the set, downloading it again if it is gone. On Windows, close the session first: a mapped segment cannot be replaced |
| `artifactNotPublished` | `installSemanticVectors` | not the release whose manifest digest was published: download the official one |
| `artifactIncompatible` | opening, installing | built for something else; `field` names the first field that disagreed: `text.*` for another line recipe, `model.*` for another model or chunking, `store.*` for another store format, `delta.*` for a delta that does not follow the set. Install the vectors built for this application. With `field` `segment_id`, installing: a version the set has installed, published again with other bytes; the set is sound and keeps serving it. Do not download it again: keep the set, or install the release into a new, empty `vectorsDir` |
| `insufficientDiskSpace` | installing, compacting | not enough free space: free some, and try again |
| `modelMissing`, `tokenizerMissing`, `modelInvalid` | `openSemanticArtifact` | no model, an ONNX graph without its `tokenizer.json`, or a file that is not a usable model: download the model's package. `modelInvalid` with `field` `model_path` is a path that names no ONNX graph, such as a GGUF: point `modelPath` at the package's `.onnx` graph |
| `modelIdentityMismatch` | `openSemanticArtifact` | `modelIdentityJson` does not describe the model at `modelPath`; `field` says which value |
| `onnxRuntimeMissing`, `onnxRuntimeUnusable` | `openSemanticArtifact` | no ONNX Runtime where one is looked for, `onnxRuntimePath` first, or one that does not load (see "The ONNX Runtime library") |
| `backendNotInBuild` | `openSemanticArtifact` | this build has no ONNX backend, as on Android and iOS |
| `sessionConflict` | `openSemanticArtifact`, `configureSemantic` | another session is open: `disableSemantic` first |
| `vectorsBusy` | installing, compacting, verifying | another install or compaction of the set is running, with `field` `vectors_dir`: nothing was changed, and an open session keeps serving; try again once it has finished. Verifying, an install replaced what the check read: nothing was condemned, verify again |
| `readOnlySession` | the calls that build vectors | refused on an opened set; nothing to fix |
| `notConfigured`, `featureNotInBuild` | `state`, `fallbackKind` | no session is open, or the build has no semantic support |
| `queryFailed` | `fallbackKind` | the semantic half of that one search failed; its lexical results were served |
| `cancelled` | `searchSemantic`, and the calls that install, compact, verify or count | its `SemanticCancellationToken` was cancelled (see "Cancelling a search"): nothing failed, and nothing changed |
| `invalidInput` | `openSemanticArtifact`, `configureSemantic`, `searchSemantic`, `compactSemanticVectors` | a value the call cannot take, a ranking option or a compaction threshold out of its range among them: fix the call |
| `internal` | any call | a fault, including the lexical index failing under `searchSemantic`: report `message` |

The development path adds `reindexRequired`, and the states `empty` (nothing
indexed yet), `needsReindex` and `failed`. The doc comment of
`SemanticErrorKind` has every kind, and API_DOCUMENTATION.md the full table.

More kinds will be added, so a `switch` over the kind needs a default branch,
and a kind the application does not know is best handled as `internal`:

```dart
try {
  await engine.openSemanticArtifact(config: input);
} on SemanticError catch (error) {
  switch (error.kind) {
    case SemanticErrorKind.artifactMissing:
    case SemanticErrorKind.artifactCorrupt:
    case SemanticErrorKind.artifactIncompatible:
      offerVectorsDownload();
    case SemanticErrorKind.modelMissing:
    case SemanticErrorKind.tokenizerMissing:
    case SemanticErrorKind.modelInvalid:
      offerModelDownload();
    default:
      // 'SemanticError(<kind>[, <field>]): <message>'
      log(error.toString());
  }
}
```

### Cancelling a search

`searchSemantic` takes a `SemanticCancellationToken`. The application searches
as the user types, so every query but the last is obsolete before it finishes,
and a semantic query embeds the text and then scans every stored vector, about a
second over the whole library; left to run, the abandoned queries would queue up
in front of the one that matters. Create a token for each search, and cancel it
when a newer query supersedes it:

```dart
SemanticCancellationToken? running;

Future<SemanticSearchResponse?> search(String query) async {
  running?.cancel();
  final token = running = SemanticCancellationToken();
  try {
    return await engine.searchSemantic(
      query: query,
      // ... the other parameters ...
      cancellation: token,
    );
  } on SemanticError catch (error) {
    if (error.kind == SemanticErrorKind.cancelled) return null; // superseded
    rethrow;
  }
}
```

The search borrows the token rather than taking it, so the object stays the
application's: `cancel()` is synchronous and returns at once, on the isolate that
started the search and while the search runs, and `isCancelled` reads it. The
search looks at the token before its lexical phase; the sidecar looks at it
throughout the semantic half, before and after it embeds the query, every 1,024
records of the vector scan, and before and after fusion; and the search looks
again before it hydrates the sidecar's results and before it paints the page.
A lexical fallback is looked at before it runs and once its page is ready. The
one stretch a cancel cannot cut short is embedding the query, a single
inference.

At the first look after the cancel the search throws a `SemanticError` of kind
`cancelled`. That is not a failure: it is never answered with lexical results
instead, since nobody is waiting for those either, and a search the sidecar
stops leaves nothing in its caches. A search that passed its last look before
the cancel returns its results, so the application still tells which query a
page answers. A token cannot be reset: a new search takes a new one. The token
is required, because flutter_rust_bridge 2.13 cannot pass an optional borrowed
object; a search with nothing to cancel passes a fresh one, which changes
nothing.

### Tuning the ranking

`searchSemantic` takes an optional `ranking`, a `SemanticRankingOptions` with
every parameter hybrid ranking runs on: the fusion strategy (`weighted`,
`rrf` with its `rrfK`, or `adaptive`); one `alphaOverride`, or the lexical
weight for each kind of query in `alphaByQueryType` (a quoted phrase, a
reference, one or two words, three or four, five or more, none); BM25's
`bm25SaturationK`; the `semanticThreshold`; the agreement, phrase, rare-word
and section bonuses and the duplicate penalty; metadata ranking; and how many
semantic candidates are fetched for each place in the window. Without it a
search ranks exactly as it always has, and so it does with
`const SemanticRankingOptions()`, whose defaults are the same values;
`SemanticRankingOptions.defaults()` reads them from the engine. A caller names
only the options it changes:

```dart
final response = await engine.searchSemantic(
  query: query,
  // ... the other parameters ...
  ranking: const SemanticRankingOptions(
    fusionStrategy: SemanticFusionStrategy.rrf,
    rrfK: 30,
  ),
  cancellation: token,
);
```

An option outside its range, or not a number, is refused before the search
runs, whether or not a session is open: a `SemanticError` of kind
`invalidInput` whose `field` names the option (`alpha_by_query_type.short`,
`rrf_k`), rather than a value clamped into one nobody chose. The ranges are the
sidecar's own; a value is used at the 32-bit precision the ranking computes in.

**The defaults are unmeasured placeholders.** Each was reasoned from a scale or
carried over from the literature, as RRF's `k` of 60 is, and none has been
checked against what a reader of this library finds relevant. Calibrating them
needs a labelled relevance set (Hebrew queries of every type, each with the
lines judged relevant to it), a metric over the page a user sees, such as
nDCG@10 or recall at the page size, and runs that vary one family of options at
a time: the strategy and RRF's `k` first, since RRF needs no calibration of
either side's scores, then the alphas, BM25's `k`, the threshold and the
bonuses. That is what this option is for: the runs, and the tuning after them,
happen from the application, without a release of the engine.

### The model

The Meivin model the application uses is its INT8 graph,
`seforim-embed-round2-int8.onnx`, with its `tokenizer.json` beside it: the
accuracy it gives up is negligible, and it is a quarter of the size, which
matters on weak machines. Its identity file is the sidecar's
`config/models/meivin-round2-onnx/model.json`, which describes the model family:
the full-precision `seforim-embed-round2-fp32.onnx` is its other package, and
queries from either land in the same space, so a vector set that lists both among
its `query_packages` opens with either graph.

### The ONNX Runtime library

The ONNX backend links nothing native. ONNX Runtime is a shared library the
sidecar loads when an ONNX model loads, so the plugin's build downloads nothing
and its binary depends on no new system library, and the application has to
provide the runtime. It is looked for in three places, and the first one that
is set is the only one looked at:

1. `onnxRuntimePath`, on `SemanticArtifactInput` (and on the development
   path's `SemanticConfigInput`): the library the application ships;
2. the file named by the `OTZARIA_ONNX_RUNTIME` environment variable;
3. the platform's default file name (`onnxruntime.dll`, `libonnxruntime.so` or
   `libonnxruntime.dylib`) in the model directory, beside the `.onnx` graph.

A path passed, or a variable set, that names nothing is refused rather than
skipped for the next place, since falling back would load a runtime nobody
chose; an empty `onnxRuntimePath` is refused as `invalidInput` before anything
is opened. Pass an absolute path. Where the runtime lives is not part of any
identity: no manifest or vector set records it, and moving it invalidates
nothing. A process holds one runtime and can neither unload nor replace it, so
`onnxRuntimePath` is compared when the same call is repeated: opening the same
vector set with another path is a `sessionConflict`, not a no-op, and after
`disableSemantic` a session that names a runtime other than the one already
loaded is refused as `onnxRuntimeUnusable` until the process restarts.

The application's installation, and what each input names:

```text
<root>/
├── otzaria/                  the data folder
│   ├── seforim.db
│   └── <model>/              the model package
│       ├── seforim-embed-round2-int8.onnx      modelPath
│       ├── tokenizer.json
│       └── model.json        the identity file, modelIdentityJson
├── index/                    the lexical index
└── vectors/                  the vector set, vectorsDir
```

The vector set is a folder of its own beside `index/`, which installing creates
and keeps: `CURRENT` and `PREVIOUS` name its live generation and the one before
it, `segments/` holds the vectors, and `incoming/` is where a download can be
left for an install to move in rather than copy — moved when the install
succeeds, left where it is when it fails, and on Windows copied when it is
read-only — and where an install expands a compressed one, into a file of its
own beside a lock file it holds, both gone when the install returns.
One install or compaction of a set runs at a time: a second is refused at once.
The runtime either ships with the application, which passes its path as
`onnxRuntimePath` (on macOS from inside the signed application bundle, below),
or sits in `<model>/` beside the graph under the platform's file name, where it
is found with no path passed; it must then be the build for that machine's
operating system and architecture. Neither the identity file nor a runtime in
that folder is part of the model package's checksum: the runtime is code, not
model data.

The reference runtime is Microsoft's official ONNX Runtime 1.28.0 release on
GitHub, and the oldest runtime API accepted is ONNX Runtime 1.17's.
Microsoft's macOS build of 1.28.0 is arm64 only and needs macOS 14 or later
(its `LC_BUILD_VERSION` minimum is 14.0), while this plugin supports macOS 12:
on macOS 12 and 13 that library does not load, and opening reports
`onnxRuntimeUnusable`. An Intel Mac, and macOS 12 or 13, need a runtime built
for them; lexical search is unaffected either way.

Without a runtime that loads (none found, not a runtime, too old, or a different
one already loaded in the process), loading the model fails with "ONNX Runtime
could not be loaded: …", which says what each of the places above held, and
names a library it refused with the place it came from. That text is in the
error `openSemanticArtifact` throws (and, on the development path,
`semanticIndexBooks` and `SemanticStatus.lastError`). It is not the "No
embedding backend is available in this build" of a build without the backend:
here the fix is the library, not a rebuild. The error's kind says which:
`onnxRuntimeMissing` when there is no file where the runtime was looked for,
`onnxRuntimeUnusable` when there is one that does not load, and
`backendNotInBuild` on a build without the backend. Otherwise the model behaves
as on such a build (see below), and lexical search is unaffected.

On macOS, an application built with the Hardened Runtime, which notarization
requires, loads only libraries signed by Apple or with its own Team ID. It may
therefore refuse Microsoft's `libonnxruntime.dylib` from the model directory
even when the file is intact, and the loader's reason then appears in that
message. What works is shipping the library inside the application bundle,
signed with the application's identity, and passing its path as
`onnxRuntimePath`.

### Builds without a backend

A build can hold the integration with no backend for the model: an ONNX model
on Android or iOS, or on a build without `semantic-onnx`. An ONNX model whose
runtime cannot be loaded behaves the same way; only the message differs (see
above).

On such a build `openSemanticArtifact` throws, since the model it has to embed
queries with cannot load, and nothing is left open: `searchSemantic` then falls
back to lexical results with an explicit `fallbackReason`. On the development
path `available`, not `enabled`, is the flag that says so:

| call | on such a build |
| --- | --- |
| `configureSemantic` | succeeds: `enabled: true`, `available: false`, `embeddingBackend: null` |
| `searchSemantic` | falls back to lexical with an explicit `fallbackReason` |
| `semanticIndexDiff` | reports `enabled: true` and lists the books as new |
| `semanticIndexBooks` | **throws** `backendNotInBuild` — there is nothing to embed with |

So a caller must gate indexing on `available`, not on `enabled` or on a
non-empty diff. Search needs no such guard: it degrades on its own.

`SearchEngine` exposes installing, keeping and opening the library's vectors,
status, and unified
lexical/hybrid/semantic search, plus the development path's configuration and
index diff/index/remove/reset. Exact/fuzzy lexical interpretation is kept
separate from the lexical/hybrid/semantic retrieval mode. Hybrid requests fall
back to Tantivy with an explicit reason when semantic support is unavailable;
semantic-only requests never masquerade as lexical results.
`lexicalTotalCount` is Tantivy's corpus count. The sidecar's `totalCount` and
`groupCount` describe its bounded fusion candidate set, so
`countsAreExact` is false for sidecar-backed responses; callers must not use
them as a corpus-wide semantic result count. `candidateWindowTruncated`
separately reports the hard candidate-window cap.

### Validating a vector set before it is published

`validate_semantic_vectors` is the publishing pipeline's last gate on a release:
it opens the release index read-only, installs the releases into a set of its
own as a device installs them, and checks what `assemble --verify` in the
sidecar cannot, because it needs the index. Build it with `--features semantic`
(the retrieval gate loads the query model):

```text
validate_semantic_vectors --index ./index --release ./published --release ./new \
    --plan ./plan --warehouse ./warehouse --model seforim-embed-round2-int8.onnx \
    --model-identity model.json --report gates.json
```

`--release` names an assembled release directory (its `segment.oxv` and
`release.json`), once per release, the published state first and the new
release last; the set they install into is removed after. `--vectors` takes a
set installed already instead.

| gate | runs with | passes when |
| --- | --- | --- |
| G3, coverage | always; `--plan` adds to it | every line of the index the recipe embeds has its text recorded by the set in its own book, by all 128 bits of its key; and with `--plan`, every record of the plan the set was assembled from is reachable in the set |
| G4, resolution | always | every record resolves on the index, by all 128 bits of its key, and every line's `chunkKey` column is its text's; records off their hint are reported, and fail it only past `--max-stale-hints <fraction>` |
| G6, retrieval | `--warehouse`, `--model`, `--model-identity` | the set's scan, as a device runs it, reaches mean recall@10 of `--min-recall-10` (0.98) and recall@50 of `--min-recall-50` (0.99) against the exact `f32` scan of the warehouse's vectors, on the same query vectors |

G6's queries are `--queries <file>`, one per line, or else `--sample-queries`
(200; from 1 to 100,000) spans of the index's lines drawn with a fixed seed, embedded by the
runtime query model (`--onnx-runtime`, or `OTZARIA_ONNX_RUNTIME`). Recall is
counted over keys, which are distinct texts, since that is what a scan returns:
a text in many books is one hit, so repeated texts cannot take the top 50 here
as they do on a results page.

A release passes when every gate ran and passed. A gate whose inputs are not
given has not run, and fails the release like a gate that failed, unless it is
skipped by name with `--skip <gate>` (G3, G4 or G6, once per gate), which the
output and the report record. The exit status is 0 when every gate passed or
was skipped so, 1 when one failed or did not run, and 2 when the arguments are
wrong or an input does not read. `--report` writes `{tool, reportVersion,
passed, index, vectors, releases, skipped, set, gates: [{gate, name, status,
detail, metrics}]}`, `status` being `passed`, `failed`, `skipped` (by
`--skip`) or `notRun` (no inputs); `--help` lists every flag.

### Development and testing: vectors built on the device

`configureSemantic` opens a session that embeds books on this device,
`semanticIndexBooks` embeds them, and `semanticIndexDiff`, `removeSemanticBooks`
and `resetSemanticIndex` maintain what it holds. They exist for development and
testing: the FFI suites, and trying a model before a build machine embeds a
library with it. The application does not use them for the library, whose
vectors come from the build machine. They are not marked `@Deprecated`, because
application code still references them; the doc comments say what they are for.

`SemanticConfigInput` states how the vectors are produced, and nothing in it is
read from the model file, so the values must be the ones the model was built
for. The sidecar records every field but `rootDir` and `onnxRuntimePath` in its
manifest as the index's identity (the model file by its checksum, once it has
loaded): an index built under one value reports `needsFullReindex` under
another, instead of mixing in vectors that cannot be compared. `onnxRuntimePath`
is optional and works as on `SemanticArtifactInput`: configuring again with
another path while a session is open is a `sessionConflict`, as for any other
changed input.

| field | Meivin ONNX |
| --- | --- |
| `modelPath` | `seforim-embed-round2-int8.onnx`, with `tokenizer.json` beside it |
| `modelId` | `ArieLLL123/judaic-semantic-round2-onnx-zayit` |
| `embeddingDim` | 256 |
| `pooling` | `in-graph` |
| `maxTokens` | 256 |
| `modelQuantization` | `int8` (`fp32` for the full-precision graph) |
| `embeddingTextVersion` | 2 |

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
version it has no code for, a token cap below 2 or above 65,536 — is refused by
`configureSemantic` itself, and so is an empty `modelQuantization`. The ceiling
is past the context of any ONNX sentence encoder, and it is what keeps a
negative `maxTokens`, which arrives as a cap in the billions, from reaching the
model's load-time probe.

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

One semantic session is open at a time, opened by `openSemanticArtifact` or by
`configureSemantic`, and closed by `disableSemantic`. An opened vector set's
vectors are on disk (`SemanticStatus.vectorsPersisted` is true) and open again
after a restart; closing the session leaves its files alone.

A session from `configureSemantic` holds its vectors in memory only, so they must
be rebuilt after every process restart, and opening an engine drops the manifest
records whose vectors did not survive. `configureSemantic` therefore does not
re-open a live session: calling it again with the same inputs is a no-op, and
calling it with different inputs fails and names the input that changed.
`disableSemantic` is the explicit way to switch model, recipe or library root,
and it discards the session's vectors. Indexing progress, and cancelling an
indexing run, are not exposed; a search can be cancelled (see "Cancelling a
search").

`openSemanticArtifact`, the calls that install, compact, verify and count a
vector set, `semanticIndexBooks`, `removeSemanticBooks`, `resetSemanticIndex`
and `semanticStatus` are all non-exclusive and asynchronous, so lexical search
and status polling stay responsive while a set installs or loads or a semantic
index is being built.

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
