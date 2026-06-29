//! Hebrew normalization for the lexical (`MagicDictionary`) lookup path.
//!
//! Two distinct normalizations live here, and the difference is the whole
//! point of this module:
//!
//! * [`normalize_hebrew`] — produces the **lookup key** used against
//!   `lexical.db`. The DB was built offline by `SeforimMagicIndexer`, whose
//!   `normalizeHebrew` strips nikud/teamim/punctuation **and folds final
//!   letters** (ך→כ, ם→מ, …). So every key *and value* stored in the DB is in
//!   that folded shape, and a query token must be folded the same way to match.
//!
//! * [`to_index_term`] — converts a form returned **from** the DB back into the
//!   shape the Tantivy `text` index actually stores. The index tokenizer
//!   (`strip_nikud` + the `"default"` analyzer) keeps final letters, so a folded
//!   DB form like `"מלכ"` would never match the index term `"מלך"`. This
//!   re-finalizes the trailing letter so the emitted search term lines up with
//!   the index. Emitting a folded form unchanged is not "wrong" (it just matches
//!   nothing) — it silently drops recall, which is exactly what we must avoid.

/// Final ↔ base letter mapping (Hebrew sofit forms).
const FINALS: [(char, char); 5] = [
    ('ך', 'כ'),
    ('ם', 'מ'),
    ('ן', 'נ'),
    ('ף', 'פ'),
    ('ץ', 'צ'),
];

/// True for nikud/teamim/punctuation removed by `SeforimMagicIndexer`'s
/// `normalizeHebrew`. Mirrors that tool exactly: teamim `U+0591–U+05AF`,
/// vowels `U+05B0–U+05BD`, and `U+05C1,05C2,05C7`. It deliberately does **not**
/// remove `U+05BF` (rafe) — the indexer keeps it, so we must too, or keys drift.
fn is_removed_point(c: char) -> bool {
    matches!(c as u32,
        0x0591..=0x05AF // cantillation (teamim)
        | 0x05B0..=0x05BD // vowels + meteg
        | 0x05C1 | 0x05C2 // shin/sin dots
        | 0x05C7 // qamatz qatan
    )
}

/// Folds a single final letter to its base form. Used by the lookup-key
/// normalization (DB stores folded forms).
fn fold_final(c: char) -> char {
    for (final_form, base) in FINALS {
        if c == final_form {
            return base;
        }
    }
    c
}

/// Re-finalizes the trailing base letter of a single word to its sofit form.
/// Only the last character can be a final letter in well-formed Hebrew, so the
/// transform is deterministic: it exactly inverts the DB's final-folding.
fn finalize_word(word: &str) -> String {
    let mut chars: Vec<char> = word.chars().collect();
    if let Some(last) = chars.last_mut() {
        for (final_form, base) in FINALS {
            if *last == base {
                *last = final_form;
                break;
            }
        }
    }
    chars.into_iter().collect()
}

/// Normalizes a token to the **`lexical.db` lookup key** shape: strips
/// nikud/teamim, drops gershayim/geresh, turns maqaf into a space, folds final
/// letters, and collapses whitespace.
pub fn normalize_hebrew(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.trim().chars() {
        match c {
            _ if is_removed_point(c) => {}        // nikud / teamim
            '\u{05F4}' | '\u{05F3}' => {}         // gershayim / geresh
            '\u{05BE}' => out.push(' '),          // maqaf → space
            _ => out.push(fold_final(c)),
        }
    }
    // collapse whitespace
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Converts a single DB form into a Tantivy `text`-index term, or `None` if it
/// is empty or multi-word (a multi-word form cannot be expressed as one `Term`
/// in a `TermSetQuery`; those are skipped in the first lexical-fuzzy version).
///
/// The form coming out of `lexical.db` already has nikud stripped and finals
/// folded; this re-finalizes the trailing letter and lowercases so the result
/// matches what the index tokenizer produced — same pipeline as
/// `hebrew_query::normalize_for_index`, plus the re-finalization step.
pub fn to_index_term(form: &str) -> Option<String> {
    let normalized = crate::hebrew_query::normalize_for_index(form);
    let mut tokens = normalized.split_whitespace();
    let first = tokens.next()?;
    if tokens.next().is_some() {
        return None; // multi-word form — out of scope for the per-token set
    }
    let term = finalize_word(first);
    if term.is_empty() {
        None
    } else {
        Some(term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_hebrew_strips_nikud_and_teamim() {
        // "הָלַ֣ךְ" → nikud + one taam (munach U+05A3) + final kaf folded
        assert_eq!(normalize_hebrew("הָלַ֣ךְ"), "הלכ");
    }

    #[test]
    fn normalize_hebrew_folds_finals() {
        assert_eq!(normalize_hebrew("מלך"), "מלכ");
        assert_eq!(normalize_hebrew("שלום"), "שלומ");
        assert_eq!(normalize_hebrew("ארץ"), "ארצ");
    }

    #[test]
    fn normalize_hebrew_handles_maqaf_and_gershayim() {
        assert_eq!(normalize_hebrew("בית\u{05BE}הכנסת"), "בית הכנסת");
        assert_eq!(normalize_hebrew("רמב\u{05F4}ם"), "רמבמ");
    }

    #[test]
    fn to_index_term_refinalizes_trailing_letter() {
        // DB stores the folded form; we must recover the index form.
        assert_eq!(to_index_term("מלכ").as_deref(), Some("מלך"));
        assert_eq!(to_index_term("שלומ").as_deref(), Some("שלום"));
        // already-final / non-foldable trailing letter is untouched
        assert_eq!(to_index_term("הלכתי").as_deref(), Some("הלכתי"));
        assert_eq!(to_index_term("בית").as_deref(), Some("בית"));
    }

    #[test]
    fn to_index_term_skips_multiword_and_empty() {
        assert_eq!(to_index_term("בית הכנסת"), None);
        assert_eq!(to_index_term("   "), None);
    }
}
