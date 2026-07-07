# Vendored tantivy-fst 0.5.0 (local patch)

Verbatim copy of `tantivy-fst 0.5.0` from crates.io (the `data/` bench
fixtures are omitted), wired in through `[patch.crates-io]` in
`rust/Cargo.toml` so that both our direct dependency and tantivy 0.26.1's
internal one resolve to this copy.

**The single functional change** is in `src/regex/dfa.rs`:

```text
const STATE_LIMIT: usize = 8_192;   // upstream: 1_000
```

(`Cargo.toml` additionally allows the `mismatched_lifetime_syntaxes` lint,
which post-0.5.0 compilers raise against the untouched upstream code.)

Why: the regex→DFA determinizer aborts with `Error::TooManyStates` past this
cap. Hebrew phrase queries (`RegexPhraseQuery`) compile each word's joined
alternation as one DFA, and wildcard-wrapped morphology/typo branches overlap
so heavily that real patterns (48 branches / ~800 chars) blow the upstream
1 000-state cap even though every branch alone compiles fine. Raising the cap
to 8 192 admits those patterns; the cost is transient query-compile memory
(~4KB+ per state ⇒ worst case ~32MB+, freed as soon as the query is built)
and linear build time. See VARIATION_CEILING_RESEARCH.md §3.ד1.

Upstream: <https://github.com/quickwit-inc/fst> — the crate is effectively
frozen (fork of BurntSushi's `fst` maintained for tantivy), so drift risk is
low. If tantivy ever bumps its `tantivy-fst` requirement past 0.5, re-vendor
the new version and re-apply the one-line patch.
