//! Hebrew normalization for the lexical (`MagicDictionary`) lookup path.
//!
//! `lexical.db` stores its values with final letters and gershayim as written
//! (`תפילין`, `רמב"ם`, mostly ASCII `"`), so [`lookup_keys`] probes a few
//! spellings of the token instead of one folded key. A minority of values were
//! stored folded (`אדמדמ`); the legacy [`normalize_hebrew`] key still reaches them.
//!
//! * [`normalize_hebrew`] — the folded shape (no nikud, no quotes, finals
//!   folded). Used as the last lookup key, by the hallucination blacklist (both
//!   columns folded the same way) and by the shared-stem check.
//!
//! * [`to_index_term`] — converts a form returned **from** the DB into the
//!   shape the Tantivy `text` index stores: final letter at the end and Hebrew
//!   gershayim/geresh folded to ASCII, as the index tokenizer does. A form in
//!   any other shape matches nothing and silently drops recall.

/// Final ↔ base letter mapping (Hebrew sofit forms).
const FINALS: [(char, char); 5] = [('ך', 'כ'), ('ם', 'מ'), ('ן', 'נ'), ('ף', 'פ'), ('ץ', 'צ')];

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

/// Folds a single final letter to its base form.
fn fold_final(c: char) -> char {
    for (final_form, base) in FINALS {
        if c == final_form {
            return base;
        }
    }
    c
}

/// Re-finalizes the trailing base letter of a single word to its sofit form.
/// Only the last character can be a final letter in well-formed Hebrew.
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

/// Hebrew prefix letters (ו ב כ ל מ ש ה ד) that may be glued to a word.
const PREFIX_LETTERS: [char; 8] = ['ו', 'ב', 'כ', 'ל', 'מ', 'ש', 'ה', 'ד'];
const MAX_PREFIX_LETTERS: usize = 3;
/// Prefix stripping never leaves fewer letters than a root, or `שבת` would
/// shrink to `ת` and accept any surface.
const MIN_STEM_LETTERS: usize = 3;

/// The first of [`lookup_keys`]: nikud/teamim stripped and maqaf turned into a
/// space, final letters and quotes kept as written, whitespace collapsed.
pub fn canonical_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.trim().chars() {
        match c {
            _ if is_removed_point(c) => {}
            '\u{05BE}' => out.push(' '),
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn finalize_words(s: &str) -> String {
    s.split(' ')
        .map(finalize_word)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Up to five deduplicated `lexical.db` keys for `token`, most literal first:
/// as written, final letter at the end, the other gershayim style, without
/// quotes, and the legacy folded [`normalize_hebrew`] shape. The first key is
/// the canonical cache key; an empty vector means there is nothing to look up.
pub fn lookup_keys(token: &str) -> Vec<String> {
    let as_written = canonical_key(token);
    let finalized = finalize_words(&as_written);
    let other_quotes = if finalized.contains('"') {
        Some(finalized.replace('"', "\u{05F4}"))
    } else if finalized.contains('\u{05F4}') {
        Some(finalized.replace('\u{05F4}', "\""))
    } else {
        None
    };
    let unquoted: String = finalized
        .chars()
        .filter(|c| !matches!(c, '"' | '\'' | '\u{05F4}' | '\u{05F3}'))
        .collect();

    let mut keys: Vec<String> = Vec::with_capacity(5);
    let candidates = [
        Some(as_written),
        Some(finalized),
        other_quotes,
        Some(finalize_words(&unquoted)),
        Some(normalize_hebrew(token)),
    ];
    for key in candidates.into_iter().flatten() {
        if !key.is_empty() && !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Hebrew letters (א–ת, finals included) in `s`; quotes, parentheses and
/// digits do not count.
pub fn hebrew_letter_count(s: &str) -> usize {
    s.chars()
        .filter(|c| ('\u{05D0}'..='\u{05EA}').contains(c))
        .count()
}

/// Prefix-stripped readings of a folded word: the word itself, then each
/// deeper strip of [`PREFIX_LETTERS`] that still leaves a root-sized stem.
fn stems(word: &[char]) -> impl Iterator<Item = &[char]> {
    let deepest = (0..MAX_PREFIX_LETTERS)
        .take_while(|&i| word.len() > i + MIN_STEM_LETTERS && PREFIX_LETTERS.contains(&word[i]))
        .count();
    (0..=deepest).map(move |depth| &word[depth..])
}

/// Shared-stem check for surfaces reached only through a spelling variant.
pub struct StemProbe {
    word: Vec<char>,
}

impl StemProbe {
    pub fn new(word: &str) -> Self {
        Self {
            word: normalize_hebrew(word).chars().collect(),
        }
    }

    /// True when a prefix-stripped reading of `surface` starts with the first
    /// two letters of a stripped reading of the word, or `surface` contains one.
    pub fn matches(&self, surface: &str) -> bool {
        let surface: Vec<char> = normalize_hebrew(surface).chars().collect();
        if self.word.is_empty() || surface.is_empty() {
            return false;
        }
        stems(&self.word).any(|w| {
            let head = &w[..w.len().min(2)];
            surface.windows(w.len()).any(|window| window == w)
                || stems(&surface).any(|s| s.starts_with(head))
        })
    }
}

/// Folds a token to the blacklist/legacy shape: strips nikud/teamim, drops
/// gershayim/geresh (Hebrew and ASCII), turns maqaf into a space, folds final
/// letters, and collapses whitespace.
pub fn normalize_hebrew(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.trim().chars() {
        match c {
            _ if is_removed_point(c) => {}             // nikud / teamim
            '\u{05F4}' | '\u{05F3}' | '"' | '\'' => {} // gershayim / geresh
            '\u{05BE}' => out.push(' '),               // maqaf → space
            _ => out.push(fold_final(c)),
        }
    }
    // collapse whitespace
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Converts a single DB form into a Tantivy `text`-index term, or `None` if it
/// is empty or multi-word (a multi-word form cannot be one `Term` in a
/// `TermSetQuery`). Same pipeline as `hebrew_query::normalize_for_index`, plus
/// gershayim/geresh folded to ASCII and the trailing letter finalized.
pub fn to_index_term(form: &str) -> Option<String> {
    let normalized = crate::hebrew_query::normalize_for_index(form);
    let mut tokens = normalized.split_whitespace();
    let first = tokens.next()?;
    if tokens.next().is_some() {
        return None;
    }
    let term = finalize_word(&first.replace('\u{05F4}', "\"").replace('\u{05F3}', "'"));
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
    fn normalize_hebrew_drops_ascii_quotes_like_the_index_folds() {
        // הטוקנייזר מקפל ׳/״ ל-'/" ASCII — הצורה המקופלת חייבת למחוק גם אותם,
        // אחרת כל טוקן-גרשיים מחטיא את ה-blacklist.
        assert_eq!(normalize_hebrew("רמב\"ם"), "רמבמ");
        assert_eq!(normalize_hebrew("ז\"ל"), "זל");
        assert_eq!(normalize_hebrew("ג'ורג'"), "גורג");
        assert_eq!(normalize_hebrew("תוס'"), "תוס");
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
    fn lookup_keys_keep_finals_and_quotes_first() {
        assert_eq!(lookup_keys("תְּפִלִּין"), vec!["תפלין", "תפלינ"]);
        assert_eq!(lookup_keys("תפילין"), vec!["תפילין", "תפילינ"]);
        // A medial trailing letter is also tried finalized.
        assert_eq!(lookup_keys("מלכ"), vec!["מלכ", "מלך"]);
        assert_eq!(
            lookup_keys("רמב\"ם"),
            vec!["רמב\"ם", "רמב\u{05F4}ם", "רמבם", "רמבמ"]
        );
        assert_eq!(
            lookup_keys("רמב\u{05F4}ם"),
            vec!["רמב\u{05F4}ם", "רמב\"ם", "רמבם", "רמבמ"]
        );
        assert_eq!(lookup_keys("בית\u{05BE}הכנסת"), vec!["בית הכנסת"]);
        assert!(lookup_keys("  ").is_empty());
    }

    #[test]
    fn stem_probe_accepts_prefixed_relatives_only() {
        let shares_stem = |word: &str, surface: &str| StemProbe::new(word).matches(surface);
        assert!(shares_stem("שבת", "שבתו"));
        assert!(shares_stem("שבת", "ושבת"));
        assert!(shares_stem("שבת", "בשבתך"));
        assert!(shares_stem("תשובה", "תשוב"));
        assert!(!shares_stem("שבת", "בבת"));
        assert!(!shares_stem("שבת", "ובת"));
        assert!(!shares_stem("תשובה", "וכן"));
        assert!(!shares_stem("תשובה", "ישובו"));
        assert!(!shares_stem("הלך", "אזל"));
        // Compared folded: a final letter in the word still matches a medial one.
        assert!(shares_stem("מלך", "המלכים"));
    }

    #[test]
    fn hebrew_letter_count_ignores_quotes_parentheses_and_digits() {
        assert_eq!(hebrew_letter_count("ת'"), 1);
        assert_eq!(hebrew_letter_count("(רמב\"ם"), 4);
        assert_eq!(hebrew_letter_count("31"), 0);
    }

    #[test]
    fn to_index_term_folds_hebrew_gershayim_like_the_index() {
        assert_eq!(to_index_term("הרמב\u{05F4}ם").as_deref(), Some("הרמב\"ם"));
        assert_eq!(to_index_term("תוס\u{05F3}").as_deref(), Some("תוס'"));
    }

    #[test]
    fn to_index_term_skips_multiword_and_empty() {
        assert_eq!(to_index_term("בית הכנסת"), None);
        assert_eq!(to_index_term("   "), None);
    }
}
