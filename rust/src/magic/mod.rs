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

/// Forms taken from a family whose base is the token itself.
const PRIMARY_FAMILY_CAP: usize = 24;
/// Forms taken from a family in which the token is a surface form.
const SECONDARY_FAMILY_CAP: usize = 8;
/// Matched surfaces taken from a family reached only through a spelling
/// variant; the rest of such a family is never pulled in.
const VARIANT_ROUTE_CAP: usize = 4;
/// Hebrew letters a form needs to be emitted at all (`ת'` is noise, not a form).
const MIN_FORM_LETTERS: usize = 2;
/// Hebrew letters a variant-route surface needs; shorter ones (`הל`) pass the
/// stem check by accident.
const MIN_VARIANT_SURFACE_LETTERS: usize = 3;
