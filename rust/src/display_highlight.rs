//! Display-highlight pattern generation for the Otzaria app.
//!
//! While [`crate::hebrew_query`] builds tantivy-fst regex terms that match
//! *index terms* (nikud-free, lowercased), this module builds regex patterns
//! that match the *displayed text* of an open book — which still contains
//! nikud, cantillation marks, and HTML tags. The Dart layer compiles the
//! returned pattern strings with `RegExp(pattern, caseSensitive: false)` and
//! applies them; it performs no pattern construction of its own.
//!
//! # Regex dialect
//!
//! The output targets Dart's `RegExp` (ECMAScript syntax): non-capturing
//! groups `(?:…)`, bounded quantifiers `{0,n}`, `\s`/`\S` classes, and
//! `\uXXXX` escapes. Nothing here is compiled by tantivy-fst.
//!
//! # Matching semantics (parity with the historical Dart `highLight`)
//!
//! * Every Hebrew base letter in a query word may be followed by attached
//!   nikud/cantillation marks in the text ([`ATTACHED_MARKS_CLASS`]).
//! * A trailing geresh matches both the ASCII and the Hebrew form.
//! * Words are joined by [`WORD_SEPARATOR`] — whitespace, Hebrew marks,
//!   HTML tags, or punctuation — optionally allowing up to the configured
//!   spacing count of intermediate words between adjacent query words.
//! * A word with a morphological option (prefixes/suffixes/partial) keeps the
//!   plain root pattern but is flagged as not eligible for the word-boundary
//!   check, so the root may highlight inside a longer inflected word.
//! * כתיב מלא/חסר fans each word (and its alternatives) into spelling
//!   variants, capped at [`hebrew_query`]'s spelling budget — unlike the old
//!   Dart code, the 2^n fan-out is bounded.

use std::collections::{HashMap, HashSet};

use crate::hebrew_query::{
    escape_regex, generate_spelling_variations, split_query_words, strip_nikud, word_flags_at,
    WordFlags, MAX_SPELLING_BRANCHES,
};

// ── Output type ────────────────────────────────────────────────────────────

/// Everything the Dart layer needs to highlight matches in displayed text.
pub struct DisplayHighlight {
    /// One regex matching the full query phrase (all words + separators).
    pub combined_pattern: String,
    /// Per-word regex (alternation-wrapped when a word has several branches),
    /// used to locate each word's sub-range inside a combined match.
    pub word_patterns: Vec<String>,
    /// `true` when the word carries no morphological expansion option and the
    /// UI may therefore require token boundaries around its match.
    pub word_boundary_eligible: Vec<bool>,
}

// ── Pattern fragments (Dart-RegExp dialect) ────────────────────────────────

/// Character class of nikud/cantillation marks *attached to a letter* —
/// U+0591–U+05C7 minus maqaf (U+05BE) and paseq (U+05C0), which separate
/// words. Were they included, the `*` after a letter would swallow a maqaf
/// between words and break word-boundary detection (e.g. "אשר־שמע").
const ATTACHED_MARKS_CLASS: &str = r"[֑-ֽֿׁ-ׇ]*";

/// Separator between adjacent query words in displayed text: whitespace,
/// Hebrew marks, HTML tags (so markup between words is not a mismatch), or
/// punctuation.
const WORD_SEPARATOR: &str = r#"(?:\s|[֑-ׇ]|<[^>]*>|[.,:;!?'"״׳־\-–—()\[\]{}])+"#;

/// Cumulative per-word pattern length budget. Display patterns are ~3× longer
/// than index-term patterns (each letter carries a marks class), so this is
/// looser than `hebrew_query::MAX_PATTERN_CHARS`; at least one branch is
/// always kept.
const MAX_DISPLAY_PATTERN_CHARS: usize = 4_000;

/// Separator allowing up to `max_intermediate_words` whole words between two
/// adjacent query words (the "מרווח בין מילים" search option).
fn separator_with_spacing(max_intermediate_words: u32) -> String {
    if max_intermediate_words == 0 {
        WORD_SEPARATOR.to_string()
    } else {
        format!(
            "{sep}(?:\\S+{sep}){{0,{n}}}",
            sep = WORD_SEPARATOR,
            n = max_intermediate_words
        )
    }
}

// ── Spacing resolution ─────────────────────────────────────────────────────

/// Resolves the allowed intermediate-word count for the gap after word `i`.
///
/// Custom per-pair values (keyed `"i-(i+1)"`) win; a missing or unparseable
/// entry falls back to the maximum custom value (mirroring the historical
/// Dart `_highlightSeparatorForIndex`). When no custom spacing is set at all,
/// every gap gets the global `distance`.
fn spacing_for_gaps(
    custom_spacing: &HashMap<String, String>,
    distance: u32,
    word_count: usize,
) -> Vec<u32> {
    let gaps = word_count.saturating_sub(1);
    if custom_spacing.is_empty() {
        return vec![distance; gaps];
    }

    let parse = |s: &String| -> Option<u32> {
        s.trim()
            .parse::<i64>()
            .ok()
            .map(|n| n.clamp(0, u32::MAX as i64) as u32)
    };
    let max_custom = custom_spacing.values().filter_map(parse).max().unwrap_or(0);

    (0..gaps)
        .map(|i| {
            let key = format!("{}-{}", i, i + 1);
            custom_spacing
                .get(&key)
                .and_then(parse)
                .unwrap_or(max_custom)
        })
        .collect()
}

// ── Per-word pattern building ──────────────────────────────────────────────

/// Builds the display pattern for one literal term: each Hebrew base letter
/// may be followed by attached marks, and geresh/gershayim match both the
/// ASCII and Hebrew forms. The term itself is expected to be nikud-free.
fn charwise_display_pattern(term: &str) -> String {
    let mut out = String::with_capacity(term.len() * 4);
    for ch in term.chars() {
        match ch {
            'א'..='ת' => {
                out.push(ch);
                out.push_str(ATTACHED_MARKS_CLASS);
            }
            '"' => out.push_str("[\"\\u05F4]"),
            '\'' => out.push_str("['\\u05F3]"),
            _ => out.push_str(&escape_regex(&ch.to_string())),
        }
    }
    out
}

/// Expands one query word (plus its alternatives) into display branches:
/// spelling variants when כתיב מלא/חסר is on, then one charwise pattern per
/// term, deduplicated in insertion order and kept within the length budget.
fn build_word_display_pattern(word: &str, flags: &WordFlags, alternatives: &[String]) -> String {
    let mut terms: Vec<String> = Vec::new();
    let mut seen_terms: HashSet<String> = HashSet::new();

    let push_term = |terms: &mut Vec<String>, seen: &mut HashSet<String>, raw: &str| {
        let stripped = strip_nikud(raw);
        if stripped.trim().is_empty() {
            return;
        }
        if flags.spelling {
            for variant in generate_spelling_variations(&stripped)
                .into_iter()
                .take(MAX_SPELLING_BRANCHES)
            {
                if seen.insert(variant.clone()) {
                    terms.push(variant);
                }
            }
        } else if seen.insert(stripped.clone()) {
            terms.push(stripped);
        }
    };

    push_term(&mut terms, &mut seen_terms, word);
    for alt in alternatives {
        push_term(&mut terms, &mut seen_terms, alt);
    }

    // Length budget: keep branches while the cumulative size stays under the
    // cap; the first branch is always kept so the pattern is never empty.
    let mut branches: Vec<String> = Vec::new();
    let mut total = 0usize;
    for term in &terms {
        let branch = charwise_display_pattern(term);
        let len = branch.chars().count();
        if !branches.is_empty() && total + len > MAX_DISPLAY_PATTERN_CHARS {
            break;
        }
        total += len;
        branches.push(branch);
    }

    match branches.len() {
        0 => String::new(),
        1 => branches.into_iter().next().unwrap(),
        _ => format!("(?:{})", branches.join("|")),
    }
}

// ── Public entry point ─────────────────────────────────────────────────────

/// Builds the display-highlight patterns for a search query.
///
/// # Parameters
///
/// - `query` — the raw query string as typed (nikud and punctuation allowed).
/// - `distance` — default intermediate-word allowance between adjacent words
///   when `custom_spacing` is empty.
/// - `custom_spacing` — per-pair overrides keyed `"i-(i+1)"` → word count.
/// - `alternative_words` — synonyms per word position (0-indexed).
/// - `search_options` — per-word option checkboxes, keyed `"{word}_{index}"`,
///   with the same word tokenization used for engine queries.
///
/// Returns `None` when the query contains no highlightable words.
pub fn build_display_highlight(
    query: &str,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
) -> Option<DisplayHighlight> {
    let words = split_query_words(query);
    if words.is_empty() {
        return None;
    }

    let mut word_patterns: Vec<String> = Vec::new();
    let mut word_boundary_eligible: Vec<bool> = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let flags = word_flags_at(&words, i, search_options);
        let alts = alternative_words
            .get(&(i as u32))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let pattern = build_word_display_pattern(word, &flags, alts);
        if pattern.is_empty() {
            // Word vanished under normalization (e.g. a nikud-only token);
            // skip it rather than emit an empty branch that matches anywhere.
            continue;
        }
        let has_expansion =
            flags.prefix || flags.suffix || flags.gram_prefix || flags.gram_suffix || flags.partial;
        word_patterns.push(pattern);
        word_boundary_eligible.push(!has_expansion);
    }

    if word_patterns.is_empty() {
        return None;
    }

    let spacing = spacing_for_gaps(custom_spacing, distance, word_patterns.len());
    let combined_pattern = if word_patterns.len() == 1 {
        word_patterns[0].clone()
    } else {
        let mut combined = String::new();
        for (i, pattern) in word_patterns.iter().enumerate() {
            combined.push_str(pattern);
            if i < word_patterns.len() - 1 {
                combined.push_str(&separator_with_spacing(spacing[i]));
            }
        }
        combined
    };

    Some(DisplayHighlight {
        combined_pattern,
        word_patterns,
        word_boundary_eligible,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn build(query: &str) -> DisplayHighlight {
        build_display_highlight(query, 0, &HashMap::new(), &HashMap::new(), &HashMap::new())
            .expect("pattern")
    }

    fn options_for(
        word: &str,
        index: usize,
        option: &str,
    ) -> HashMap<String, HashMap<String, bool>> {
        HashMap::from([(
            format!("{}_{}", word, index),
            HashMap::from([(option.to_string(), true)]),
        )])
    }

    #[test]
    fn single_word_is_charwise_with_marks() {
        let hl = build("ספר");
        assert_eq!(
            hl.combined_pattern,
            format!("ס{m}פ{m}ר{m}", m = ATTACHED_MARKS_CLASS)
        );
        assert_eq!(hl.word_patterns.len(), 1);
        assert_eq!(hl.word_boundary_eligible, vec![true]);
    }

    #[test]
    fn multi_word_joined_by_separator() {
        let hl = build("שלום עולם");
        assert_eq!(hl.word_patterns.len(), 2);
        assert!(hl.combined_pattern.contains(WORD_SEPARATOR));
        assert!(!hl.combined_pattern.contains("\\S+"));
    }

    #[test]
    fn distance_adds_intermediate_word_allowance() {
        let hl = build_display_highlight(
            "שלום עולם",
            2,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert!(hl.combined_pattern.contains("{0,2}"));
    }

    #[test]
    fn custom_spacing_overrides_distance() {
        let spacing = HashMap::from([("0-1".to_string(), "3".to_string())]);
        let hl =
            build_display_highlight("שלום עולם", 1, &spacing, &HashMap::new(), &HashMap::new())
                .unwrap();
        assert!(hl.combined_pattern.contains("{0,3}"));
        assert!(!hl.combined_pattern.contains("{0,1}"));
    }

    #[test]
    fn missing_spacing_key_falls_back_to_max_custom_value() {
        let spacing = HashMap::from([("1-2".to_string(), "4".to_string())]);
        let hl = build_display_highlight(
            "אחד שנים שלוש",
            0,
            &spacing,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        // Gap 0-1 has no key → falls back to the max custom value (4).
        assert_eq!(hl.combined_pattern.matches("{0,4}").count(), 2);
    }

    #[test]
    fn nikud_in_query_is_ignored() {
        assert_eq!(build("סֵפֶר").combined_pattern, build("ספר").combined_pattern);
    }

    #[test]
    fn trailing_geresh_matches_both_forms() {
        let hl = build("תוס'");
        assert!(hl.combined_pattern.contains("['\\u05F3]"));
    }

    #[test]
    fn gershayim_splits_like_engine_tokenizer() {
        // ז"ל מתפצל לשני טוקנים (כמו HebrewTokenizer); הגרשיים בטקסט המוצג
        // נתפסות ע"י מפריד-המילים, שכולל " וגם את הצורה העברית ״.
        let hl = build("ז\"ל");
        assert_eq!(hl.word_patterns.len(), 2);
        assert!(hl.combined_pattern.contains(WORD_SEPARATOR));
    }

    #[test]
    fn spelling_option_fans_variants() {
        let options = options_for("שלום", 0, "כתיב מלא/חסר");
        let hl =
            build_display_highlight("שלום", 0, &HashMap::new(), &HashMap::new(), &options).unwrap();
        assert!(hl.combined_pattern.starts_with("(?:"));
        assert!(hl.combined_pattern.contains('|'));
        // The defective spelling (שלם) must be one of the branches.
        assert!(hl
            .combined_pattern
            .contains(&format!("ש{m}ל{m}ם{m}", m = ATTACHED_MARKS_CLASS)));
        assert_eq!(hl.word_boundary_eligible, vec![true]);
    }

    #[test]
    fn morphological_option_disables_boundary() {
        for option in [
            "קידומות",
            "סיומות",
            "קידומות דקדוקיות",
            "סיומות דקדוקיות",
            "חלק ממילה",
        ] {
            let options = options_for("ספר", 0, option);
            let hl = build_display_highlight("ספר", 0, &HashMap::new(), &HashMap::new(), &options)
                .unwrap();
            assert_eq!(hl.word_boundary_eligible, vec![false], "option: {option}");
            // The pattern stays the plain root — the boundary flag alone
            // widens matching to inflected words.
            assert_eq!(hl.combined_pattern, build("ספר").combined_pattern);
        }
    }

    #[test]
    fn alternatives_become_branches() {
        let alternatives = HashMap::from([(0u32, vec!["חכם".to_string()])]);
        let hl =
            build_display_highlight("צדיק", 0, &HashMap::new(), &alternatives, &HashMap::new())
                .unwrap();
        assert!(hl.combined_pattern.starts_with("(?:"));
        assert!(hl
            .combined_pattern
            .contains(&format!("ח{m}כ{m}ם{m}", m = ATTACHED_MARKS_CLASS)));
    }

    #[test]
    fn empty_and_nikud_only_queries_yield_none() {
        for query in ["", "   ", "ְָ"] {
            assert!(
                build_display_highlight(
                    query,
                    0,
                    &HashMap::new(),
                    &HashMap::new(),
                    &HashMap::new()
                )
                .is_none(),
                "query: {query:?}"
            );
        }
    }

    #[test]
    fn spelling_fanout_is_budget_bounded() {
        // A word made of optional letters explodes 2^n in the old Dart code;
        // here the branch count must respect the spelling budget.
        let options = options_for("ויוויוויו", 0, "כתיב מלא/חסר");
        let hl =
            build_display_highlight("ויוויוויו", 0, &HashMap::new(), &HashMap::new(), &options)
                .unwrap();
        let branch_count = hl.word_patterns[0].matches('|').count() + 1;
        assert!(branch_count <= MAX_SPELLING_BRANCHES);
    }
}
