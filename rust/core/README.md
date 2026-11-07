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

The compatibility policy is owned by the canonical engine, not the facade. The
current baseline (`8ef29fd592793fbc9d5af0299fdd7a8607789bac`) creates schema **11**
and explicitly reads schemas **10 and 11**. Opening an existing schema-10 index
never upgrades its metadata or pretends it contains schema-11 note provenance.
Schemas 7–9 require rebuilding in the app; a future schema requires a newer
engine. The metadata version and the actual Tantivy schema must agree. The MCP
keeps its database-only tools available when search is incompatible and must
never rebuild or rewrite an app-owned index itself.

The application checkout `3168cc9e586e48078cfe7c40a59e1d9df2e4f67d` separately
pins engine `b78d36eaffcb2a5fa679d34a52133512ee55c084` in both its manifests.
That engine still reads and writes schema 10. This shared-core proposal does not
advance the application's dependency pin or assume it already uses schema 11.

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

The current execution record is in [VALIDATION.md](VALIDATION.md). It distinguishes
fresh tests on this rebased change from the independently preserved earlier work.
The facade contracts run with parallel test execution; one also issues 64 searches
from four concurrent threads against the same read-only engine. A schema-11
fixture verifies that note markers, note bodies and highlights match the writer's
results. Neither fixture is a full production-library performance benchmark.

## Integration order

1. MCP admission control was merged as [otzaria_mcp#4](https://github.com/Otzaria/otzaria_mcp/pull/4).
   Its later reserved maintenance slot and live response metadata must stay intact.
2. Merge [the shared-core change #84](https://github.com/Otzaria/otzaria_search_engine/pull/84) after its Rust,
   generated-bridge and platform checks pass.
3. Merge [MCP migration #5](https://github.com/Otzaria/otzaria_mcp/pull/5) pinned to that reviewed engine revision (or the
   resulting immutable merge revision), with its `tantivy-fst` patch pinned to
   the same engine commit. Carry the canonical Tantivy fork revision too.

[Writer-lifecycle PR #77](https://github.com/Otzaria/otzaria_search_engine/pull/77)
is already part of the current baseline. `read_only` and `writer_closed` remain
independent permanent guards; `open_writer` checks both. The lifecycle contract
is now an ordinary `core/tests/writer_lifecycle.rs` target, alongside the
canonical `rust/tests/index_writer_shutdown.rs` suite. The current Tantivy pin is
`130b2682807d2a609d09ba432ba4d0ccc8faf7ed`.

[PR #82](https://github.com/Otzaria/otzaria_search_engine/pull/82) is also merged
in this baseline. The rebase drops our duplicate of its initial commit and
preserves follow-up `9f6719073aa24834cfe53005e1190d8689e1b0d1` unchanged. Rayon
workers do not wait on cache work owned by their own suspended ancestor. Gate
owners retain parallel segment scans; duplicate contenders scan inline. Outside
Rayon, the existing single-flight behavior stays. The regression exercises one,
two and four Rayon workers, alongside scan parity, cancellation and gate ownership.

The exact earlier shared-core proposal, including its independent schema-10
fixture, is preserved on
[`recovery/shared-core-20261105`](https://github.com/Otzaria/otzaria_search_engine/tree/recovery/shared-core-20261105).
Its source was recovered from retained Git objects before the changes were
replayed over current upstream. No old source tree was substituted for the newer
engine implementation, and historical validation is not presented as testing the
new combination.

The subsequently validated pre-rebase head is independently preserved on
[`recovery/shared-core-20261107-b22aab7`](https://github.com/Otzaria/otzaria_search_engine/tree/recovery/shared-core-20261107-b22aab7).
The current rebase evidence and the earlier records remain distinguishable in
[VALIDATION.md](VALIDATION.md).
