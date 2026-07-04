pub mod api;
mod display_highlight;
mod frb_generated;
mod hebrew_query;
// מודול tantivy פנימי — מחוץ ל-crate::api כדי ש-FRB לא ינסה לגזור לו bindings.
mod hebrew_tokenizer;
// Lives at the crate root (not under `api`) so flutter_rust_bridge — which
// scans `crate::api` — never generates bindings for the dictionary internals.
// Only `SearchEngine::set_magic_dictionary_path`/`has_magic_dictionary` are
// exposed to Dart; everything here is used purely from Rust.
mod magic;
