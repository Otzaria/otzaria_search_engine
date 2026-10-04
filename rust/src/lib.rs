pub mod api;
// ביטוי שנמשך משורה לשורה הבאה — מחוץ ל-crate::api כדי ש-FRB לא ינסה לגזור לו bindings.
mod cross_line;
#[cfg(test)]
mod cross_line_tests;
mod display_highlight;
mod frb_generated;
mod highlight_matcher;
// אכיפת מרווח פר-זוג לשאילתות ביטוי — מחוץ ל-crate::api כדי ש-FRB לא ינסה
// לגזור לו bindings.
mod gap_phrase;
mod hebrew_query;
// סינון "תחת אותה כותרת" (חיתוך sectionId) — מחוץ ל-crate::api כדי ש-FRB
// לא ינסה לגזור לו bindings.
mod section_scope;
// מודול tantivy פנימי — מחוץ ל-crate::api כדי ש-FRB לא ינסה לגזור לו bindings.
mod hebrew_tokenizer;
// The index's tantivy directory, with Windows' transient refusals to replace a file retried.
mod index_directory;
// Lives at the crate root (not under `api`) so flutter_rust_bridge — which
// scans `crate::api` — never generates bindings for the dictionary internals.
// Only `SearchEngine::set_magic_dictionary_path`/`has_magic_dictionary` are
// exposed to Dart; everything here is used purely from Rust.
mod magic;
// מילוני ההרחבה של החיפוש המתקדם (תרגום ארמי↔עברי, פענוח ראשי-תיבות) —
// מחוץ ל-crate::api כדי ש-FRB לא ינסה לגזור להם bindings.
mod lexicons;
// Line text of official books read from the library database at search time, and the
// choice of SQLite (bundled or the host's) — internal; only thin wrappers live in
// `crate::api`.
#[cfg(test)]
mod external_text_tests;
mod line_source;
mod sqlite_host;
// The lexical index seen as the semantic builder's corpus (S4b). At the crate root, not
// under `api`: it is a Rust-to-Rust port between this engine and the semantic sidecar, and
// flutter_rust_bridge must not generate bindings for it — Dart never supplies a corpus.
#[cfg(feature = "semantic-integration")]
pub mod semantic_corpus;
// The plan of a vector build, from the release index, as the sidecar's plan files. At the
// crate root for the reason `semantic_corpus` is.
#[cfg(feature = "semantic-integration")]
pub mod semantic_plan;
// Which `SemanticErrorKind` each sidecar failure is. At the crate root for the reason
// `semantic_corpus` is: it matches the sidecar's error types, which Dart never sees, and
// flutter_rust_bridge must not generate bindings for it. The kinds themselves are in
// `crate::api`, where Dart gets them.
#[cfg(feature = "semantic-integration")]
mod semantic_errors;
// The live index as a vector set's resolver: which books a filter admits, and which live
// lines hold the keys a scan returned. At the crate root for the reason `semantic_errors` is:
// it implements the sidecar's port, which Dart never sees.
#[cfg(feature = "semantic-integration")]
mod semantic_resolver;
// What a filtered search scans beyond the books it admits: the texts a live book holds that
// the vector set records elsewhere. At the crate root for the reason `semantic_resolver` is.
#[cfg(feature = "semantic-integration")]
mod semantic_moves;
// What a semantic search's cancellation token holds, and where a search looks at it. At the
// crate root for the same reason: Dart gets the token, in `crate::api`, and nothing here. In
// every build, since the token is part of the API whether or not the sidecar is.
mod search_cancellation;
// The chunk key the index stores for each line, and the recipe it is computed under. At the
// crate root so flutter_rust_bridge generates no bindings for it, and in every build: the
// release index is built by one build and opened by all of them. Public for the Rust side
// alone: the tests and tools hold the recipe to the one a model publishes.
pub mod semantic_keys;
