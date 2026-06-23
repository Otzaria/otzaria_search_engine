//! Hallucination blacklist — manual `(token, base)` pairs the offline LLM got
//! wrong. Applied **only to highlighting**, never to recall: a bad mapping
//! still expands the search (so we don't lose real matches), but its forms are
//! not painted red in the snippet. Mirrors `LuceneSearchEngine`'s behaviour.
//!
//! The file is embedded at compile time, so there is no runtime path to manage
//! and no chance of a missing-file failure on device.

use super::normalize::normalize_hebrew;
use once_cell::sync::Lazy;
use std::collections::HashSet;

const RAW: &str = include_str!("../../resources/hallucination_blacklist.tsv");

/// Set of `(normalized_token, normalized_base)` pairs. Both columns are
/// normalized with [`normalize_hebrew`] so lookups compare apples to apples
/// regardless of how the caller spells the token.
static PAIRS: Lazy<HashSet<(String, String)>> = Lazy::new(|| {
    let mut set = HashSet::new();
    for line in RAW.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cols = line.split('\t');
        if let (Some(token), Some(base)) = (cols.next(), cols.next()) {
            let token = normalize_hebrew(token);
            let base = normalize_hebrew(base);
            if !token.is_empty() && !base.is_empty() {
                set.insert((token, base));
            }
        }
    }
    set
});

/// True when `(token, base)` is a known hallucination and the expansion under
/// `base` should be withheld from highlighting. Inputs are normalized here, so
/// callers may pass either raw or already-normalized strings.
pub fn is_blacklisted(token: &str, base: &str) -> bool {
    PAIRS.contains(&(normalize_hebrew(token), normalize_hebrew(base)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_pair_is_blacklisted() {
        // "לחתוך" → base "כנ" is a hallucination in the shipped list.
        assert!(is_blacklisted("לחתוך", "כנ"));
        // final-letter folding makes the medial spelling match too.
        assert!(is_blacklisted("לחתוכ", "כן"));
    }

    #[test]
    fn unrelated_pair_is_not_blacklisted() {
        assert!(!is_blacklisted("שלום", "שלמ"));
    }
}
