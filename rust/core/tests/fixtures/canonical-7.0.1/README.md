# Canonical 7.0.1 index compatibility fixture

This index and `expected.json` were produced by the unmodified search implementation
at commit `225a6d5d7265c478a64d3a609a536d0deed433e7` (7.0.1, schema 10),
before the shared-core extraction. It contains nine synthetic device-related lines,
stored in the index, with three stable book keys and deliberately different
catalogue/generation orders. No external library or user data is included.

The golden covers 16 searches: exact, phrase, fuzzy and identical-text grouping in
all three result orders, pagination, facets, vocalized matching, and an advanced
negative query. It includes every result field, snippets/highlight tags, merged
siblings, total counts, document count and per-book counts. The integration test
copies this index, opens it through the headless facade, compares the complete
golden and verifies that every index file is unchanged after the reader closes.

## Reproducing the fixture

Use a separate checkout at the exact baseline commit above. Copy `generate.rs`
from this directory to its `rust/examples/schema10_fixture.rs` and copy
`../../support/contract.rs` to its `rust/examples/contract.rs`. Then run:

```sh
cargo run --manifest-path rust/Cargo.toml --locked --example schema10_fixture -- /absolute/new/index > expected.json
```

The baseline validation in this change used the same locked dependency graph and
limited the temporary baseline checkout's `crate-type` to `["rlib"]` to avoid
building unneeded native archives. No source, schema, tokenizer or dependency
revision was changed. Segment UUIDs and metadata creation times naturally vary
when regenerated; compare the result golden rather than expecting identical
fresh index bytes. The checked-in index is preserved verbatim, including its
metadata coordination lock files. `.gitattributes` prevents newline conversion.

Do not regenerate this fixture with the implementation being tested: that would
lose its value as an independently produced existing-index contract.
