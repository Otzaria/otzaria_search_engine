//! Lexical morphology expansion for the **approximate (`fuzzy`) search path**.
//!
//! `MagicDictionary` wraps a read-only `lexical.db` (built offline) and turns a
//! query token into the real surface forms / spelling variants / lemma linked
//! to it. The fuzzy query builder injects those as extra term alternatives, so
//! "approximate" search finds morphological relatives ("הלך" → "הלכתי") on top
//! of the existing edit-distance matches — without changing exact search or any
//! public API signature.
//!
//! Everything here is optional and best-effort: no DB → no expansion → the
//! engine behaves exactly as before.

mod blacklist;
mod dictionary;
mod normalize;

pub use dictionary::MagicDictionary;

/// Upper bound on lexical forms injected per query token. Kept small so the
/// per-token `TermSetQuery` stays cheap; mirrors `MAX_SYNONYM_TERMS_PER_TOKEN`
/// in the reference Lucene engine.
pub const MAX_LEXICAL_FORMS: usize = 32;
