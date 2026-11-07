# Shared Rust search API

`otzaria-search-core` is the transport-free entry point for Rust hosts, including
`otzaria_mcp`. It re-exports the canonical `search_engine` implementation in this
repository with `default-features = false`. It contains no search implementation
or copied source tree. Query parsing, stable book IDs, index/schema checks,
ordering, snippets, highlights, SQLite line sources and writer lifecycle all have
one owner in `rust/src`.

This extraction establishes an API and dependency boundary. The implementation
has deliberately kept its file locations and owning crate: FRB's generated code
implements bridge traits on those Rust types, and moving the types into another
crate would require a large wrapper/conversion layer and a different generated
ABI. The optional `flutter-bridge` feature contains the existing Dart bindings
and `StreamSink` entry points. The Flutter package selects it explicitly in its
build hook and still builds `search_engine`; standalone Rust hosts select this
facade without it. No Tokio, MCP transport, or admission policy is added to the
Flutter library.

## Dependency and feature contract

Consume `package = "otzaria-search-core"` from an exact reviewed commit of
`Otzaria/otzaria_search_engine`, never a copied tree or an unpinned branch. The
package is at `rust/core`; Cargo finds it when selecting a package from a Git
repository. Existing Rust imports can retain the dependency alias `search_engine`.
The facade's dependency graph must stay free of the `flutter_rust_bridge`
runtime. The small `flutter_rust_bridge_macros` proc-macro remains a build-time
annotation dependency: it preserves the exact shared type declarations and field
defaults, and contributes no Dart runtime or bridge code to the host binary.

The default feature is `sqlite-bundled` for standalone tools. Hosts supplying a
SQLite API table use `default-features = false, features = ["sqlite-host"]`.
These two SQLite modes are mutually exclusive. `semantic`, `semantic-integration`,
`semantic-onnx` and `semantic-mock` forward the canonical features; inference is
not enabled by default. Both consumers use the same chunk-key recipe even when
inference is disabled.

Cargo applies `[patch.crates-io]` only from the workspace root. External hosts must
retain the canonical Tantivy fork revision and patched `tantivy-fst` from this
repository's `rust/Cargo.toml`; merely depending on this facade does not install
the patches. Pin `tantivy-fst` to the same reviewed engine commit. Do not make a
second copy of its source.

## Opening and changing an index

`SearchEngine::open_read_only` is fallible and checks compatibility before opening
an existing index. It never creates an index, upgrades metadata, opens a writer,
or writes the book-order mapping. All public index/vector mutation APIs reject
the returned handle, including empty batches and order-only updates. Dictionary
loading, cache release and lexical searches remain available. Semantic artifact
opening is refused on a read-only engine: the sidecar currently performs recovery
and garbage collection before exposing its read-only session. Vector verification
is also refused because it can persist corrupt-segment markers. A future semantic
read-only open requires a sidecar constructor that performs no recovery writes.

Tantivy still acquires its metadata coordination lock to protect segment loading
against concurrent garbage collection. The associated lock file may be created;
read-only describes index content and writer ownership, not support for an
immutable filesystem or directory permissions that prohibit lock bookkeeping.
The writer lock is never acquired by this constructor or by a rejected mutation.

The reader is a stable snapshot. `refresh_reader(&mut self)` observes committed
changes to the *same* open index and drops query caches. A replacement of the
index directory requires dropping/reopening the handle; a reload is not a
portable directory-replacement mechanism, particularly for mapped files on
Windows. Host admission/snapshot locks determine when outstanding requests finish.
Do not attempt to terminate an active native query by freeing its engine.

`refresh_line_source(path)` invalidates the library SQLite connection, row caches,
and independent scan connections even when the configured path did not change.
It waits for active windows and scans before returning; the next read opens the
current database. This uses the canonical scan invalidation protocol, including
the case where scans are active but the row-window connection is absent.
Line-source configuration is process-wide. A host switching unrelated libraries
must serialize the transition with all requests using the previous configuration.
Use the existing suspend/resume protocol to release handles before a database
replacement on platforms which prohibit replacing open files.

The schema contract has not changed. Version 7.0.1 reads schema 10. Legacy schema
7/8/9 is refused with the canonical rebuild requirement; a future schema is
refused as an engine-too-old condition. The MCP must keep its database-only tools
available and explain that the app must rebuild/upgrade its index. The MCP must
never rebuild or rewrite an app-owned index itself.

## Validation

From the repository root:

```sh
cargo test --manifest-path rust/core/Cargo.toml --locked
cargo test --manifest-path rust/Cargo.toml --locked --no-default-features --features sqlite-bundled --lib
cargo tree --manifest-path rust/core/Cargo.toml --locked -e normal
cargo test --manifest-path rust/Cargo.toml --locked --all-targets
```

CI tests the headless facade in its own invocation, because Cargo unifies features
for the same dependency in one build. A workspace-wide invocation that also builds
the default Flutter package cannot prove that the standalone graph excludes FRB.
The existing FRB regeneration/Flutter tests retain the bridge ABI gate. Targeted
contracts cover a live writer beside a reader, stable IDs/order/snippets, concurrent
reads, refresh after a commit, rejection of all mutation families, legacy/newer
metadata, and same-path refresh with scan-only connections.

`tests/fixtures/canonical-7.0.1` contains a small, independently built schema-10
index from the unmodified `225a6d5d7265c478a64d3a609a536d0deed433e7` baseline.
Its 16-case golden covers all result fields, three orders, snippets/highlights,
counts, grouping, pagination, facets and vocalized/advanced queries. Opening,
searching and closing it must leave every index file byte-identical. A separate
test closes the reader, replaces its directory and verifies the reopened content.
See the fixture README for provenance and reproduction instructions.

Local Linux validation for this extraction passed all eight facade integration
tests and 845 headless engine unit tests; 27 pre-existing tests were ignored. One
unit test that creates a directory under its working directory first failed on a
full workspace disk, then passed from a writable temporary directory using the
same compiled test binary. CI also exercises the facade on Windows and retains
the existing generated-binding and Flutter tests.

## Integration order

1. MCP admission control can merge independently of this crate boundary.
2. Merge the shared-core change in `otzaria_search_engine`.
3. Merge the MCP migration pinned to that reviewed engine revision (or a resulting
   immutable merge revision), after its compatibility and parity tests pass.

The boundary does not require the unmerged writer-lifecycle PR. It preserves the
canonical writer implementation and its source location; when that PR merges,
both hosts receive the same fix on their next pinned engine update. Keep any
read-only guard when resolving changes around writer creation. Do not enable
semantic inference or the Flutter bridge incidentally while updating the pin.

Writer-lifecycle PR #77 at `2da23091237ed0f0a612de3b2d18cfc99d8b7d2a` also updates
the Tantivy fork to `b6dfc7bc93336df1b996a6701b8c86c80206b8d1` so writer teardown
waits for background I/O. Combining it with this boundary must retain both
`read_only` and `writer_closed` in the engine and constructors, both guards in
`open_writer`, and that PR's newer Tantivy pin. Update the MCP root patch to the
same Tantivy revision when advancing its engine pin to the combined revision.
