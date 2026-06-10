//! Hebrew search-query logic, ported 1:1 from the otzaria app's Dart layer
//! (`lib/search/search_query_builder.dart` + `lib/search/utils/regex_patterns.dart`).
//!
//! This module turns a high-level query string + per-word options into the
//! Tantivy-regex term strings, slop and `max_expansions` that the advanced
//! search mode feeds to `RegexQuery` / `RegexPhraseQuery`. Keeping it identical
//! to the Dart implementation is what keeps search results and the app's
//! snippet highlighting consistent.
//!
//! Pure string logic only — no Tantivy dependency. The one intentional
//! deviation from Dart: the full/partial-spelling builder does NOT wrap its
//! alternation in `^…$`. tantivy-fst's regex rejects anchors (`NoEmpty`) and is
//! already implicitly whole-term anchored, so the bare `(?:…)` preserves the
//! Dart intent while staying valid for Tantivy.

use std::collections::{HashMap, HashSet};

use unicode_segmentation::UnicodeSegmentation;

// ── Search-option keys (must match the Dart UI keys exactly) ────────────────────

/// Typo-tolerance option key (`'שגיאות כתיב'`). Public so the engine can reason
/// about fuzzy-ish behaviour if needed.
pub const OPT_TYPO: &str = "שגיאות כתיב";
const OPT_PREFIX: &str = "קידומות";
const OPT_SUFFIX: &str = "סיומות";
const OPT_GRAM_PREFIX: &str = "קידומות דקדוקיות";
const OPT_GRAM_SUFFIX: &str = "סיומות דקדוקיות";
const OPT_SPELLING: &str = "כתיב מלא/חסר";
const OPT_PARTIAL: &str = "חלק ממילה";

// Suffix alternation groups (verbatim from regex_patterns.dart).
const SUFFIX_PATTERN: &str = r"(ותי|ותיך|ותיו|ותיה|ותינו|ותיכם|ותיכן|ותיהם|ותיהן|יי|יך|יו|יה|ינו|יכם|יכן|יהם|יהן|י|ך|ו|ה|נו|כם|כן|ם|ן|ים|ות)?";
const PREFIX_GROUP: &str = r"(ו|מ|כ|ב|ש|ל|ה|ד)?(כ|ב|ש|ל|ה|ד)?(ה)?";
const FULL_SUFFIX_PATTERN: &str = r"(ותי|ותַי|ותיך|ותֶיךָ|ותַיִךְ|ותיו|ותָיו|ותיה|ותֶיהָ|ותינו|ותֵינוּ|ותיכם|ותֵיכם|ותיכן|ותֵיכן|ותיהם|ותֵיהם|ותיהן|ותֵיהן|יות|יי|יַי|יך|יךָ|יִךְ|יו|יה|יא|תא|יהָ|ינו|יכם|יכן|יהם|יהן|י|ך|ךָ|ךְ|ו|ה|הּ|נו|כם|כן|ם|ן|ים|ות)?";

const INSERTION_LETTERS: &[&str] = &[
    "ו", "י", "א", "ה", "פ", "ל", "מ", "נ", "ב", "כ", "ש", "ת", "ר",
];

const MAX_TYPO_TOLERANCE_VARIATIONS: usize = 48;

// ── Result of preparing an advanced query ───────────────────────────────────────

pub struct AdvancedQuery {
    pub regex_terms: Vec<String>,
    pub slop: u32,
    pub max_expansions: u32,
}

// ── Tokenisation (parity with SearchQueryBuilder.sanitizeQuery / splitQueryWords)

/// Removes/normalises punctuation exactly like the Dart `sanitizeQuery`:
/// `״→"`, `׳→'`, `־`/`-`→space, strips `,;!?:*()[]{}^$|\+.~\`` and collapses
/// whitespace runs to a single space, then trims.
pub fn sanitize_query(query: &str) -> String {
    const REMOVE: &[char] = &[
        ',', ';', '!', '?', ':', '*', '(', ')', '[', ']', '{', '}', '^', '$', '|', '\\', '+', '.',
        '~', '`',
    ];
    let mut buf = String::with_capacity(query.len());
    for ch in query.chars() {
        match ch {
            '\u{05F4}' => buf.push('"'),  // ״ gershayim
            '\u{05F3}' => buf.push('\''), // ׳ geresh
            '\u{05BE}' => buf.push(' '),  // ־ maqaf
            '-' => buf.push(' '),
            c if REMOVE.contains(&c) => {}
            c => buf.push(c),
        }
    }
    collapse_whitespace(&buf)
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

/// A query-token char: Latin alphanumerics, Hebrew letters (א-ת) and the Hebrew
/// block marks (֐-ׇ, U+0590–U+05C7). Mirrors the `_tokenExtractor` char class.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || ('\u{05D0}'..='\u{05EA}').contains(&c) // א-ת
        || ('\u{0590}'..='\u{05C7}').contains(&c) // ֐-ׇ (Hebrew marks)
}

/// Splits a query into tokens exactly like the Dart `splitQueryWords`:
/// runs of word-chars, optionally keeping a trailing `'` when it is NOT followed
/// by another word-char (so `תוס'` stays whole but `ד'אש`→`ד`,`אש`); `"` always
/// separates (`ז"ל`→`ז`,`ל`).
pub fn split_query_words(query: &str) -> Vec<String> {
    let cleaned = sanitize_query(query);
    let chars: Vec<char> = cleaned.trim().chars().collect();
    let mut words = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if is_word_char(chars[i]) {
            let start = i;
            while i < chars.len() && is_word_char(chars[i]) {
                i += 1;
            }
            let mut end = i;
            if i < chars.len() && chars[i] == '\'' {
                let next_is_word = i + 1 < chars.len() && is_word_char(chars[i + 1]);
                if !next_is_word {
                    end = i + 1;
                    i += 1;
                }
            }
            let word: String = chars[start..end].iter().collect();
            if !word.is_empty() {
                words.push(word);
            }
        } else {
            i += 1;
        }
    }
    words
}

/// Strips Hebrew nikud and cantillation (U+0591–U+05C7), matching the app's
/// `vowelsAndCantillation` regex. Used to normalise the query for exact-mode
/// term/phrase matching against the (nikud-stripped) index.
pub fn strip_nikud(text: &str) -> String {
    text.chars()
        .filter(|c| !('\u{0591}'..='\u{05C7}').contains(c))
        .collect()
}

// ── Regex escaping ──────────────────────────────────────────────────────────────

/// Escapes the regex metacharacters that tantivy-fst recognises. Sanitised
/// Hebrew/Latin/digit tokens contain none of these, so this is a no-op for
/// realistic input — matching Dart's `RegExp.escape` for those tokens.
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(
            ch,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

// ── Ordered de-duplication helper (mimics Dart's insertion-ordered Set) ─────────

fn push_unique(out: &mut Vec<String>, seen: &mut HashSet<String>, value: String) {
    if seen.insert(value.clone()) {
        out.push(value);
    }
}

// ── Full/partial spelling variations ────────────────────────────────────────────

/// Generates כתיב מלא/חסר variations by toggling each optional `י ו ' "`.
/// Insertion order matches Dart (bitmask 0..2^n, bit set = include the char).
pub fn generate_full_partial_spelling_variations(word: &str) -> Vec<String> {
    if word.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = word.chars().collect();
    let optional_indices: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter(|(_, &c)| c == 'י' || c == 'ו' || c == '\'' || c == '"')
        .map(|(i, _)| i)
        .collect();
    let n = optional_indices.len();
    // Safety guard against pathological input (only-yod/vav words of huge length).
    // Real tokens never approach this; keeps the 1<<n shift well-defined.
    if n > 20 {
        return vec![word.to_string()];
    }
    let num = 1usize << n;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for mask in 0..num {
        let mut variant = String::new();
        let mut orig = 0usize;
        for (opt_idx, &next_optional) in optional_indices.iter().enumerate() {
            variant.extend(&chars[orig..next_optional]);
            if (mask & (1 << opt_idx)) != 0 {
                variant.push(chars[next_optional]);
            }
            orig = next_optional + 1;
        }
        variant.extend(&chars[orig..]);
        push_unique(&mut out, &mut seen, variant);
    }
    out
}

// ── Typo tolerance (edit distance 1) ─────────────────────────────────────────────

fn typo_substitutions(grapheme: &str) -> &'static [&'static str] {
    match grapheme {
        "א" => &["ע", "ה"],
        "ע" => &["א", "ה"],
        "ה" => &["א", "ע", "ח"],
        "ח" => &["ה"],
        "ו" => &["ב"],
        "ב" => &["ו"],
        "כ" => &["ק"],
        "ק" => &["כ"],
        "ט" => &["ת"],
        "ת" => &["ט"],
        "ס" => &["ש", "שׁ", "שׂ"],
        "ש" => &["ס", "שׁ", "שׂ"],
        "שׁ" => &["ס", "ש", "שׂ"],
        "שׂ" => &["ס", "ש", "שׁ"],
        "צ" => &["ז"],
        "ז" => &["צ"],
        _ => &[],
    }
}

/// Common Hebrew letter confusions + adjacent transposition. Starts with the
/// word itself (parity with the Dart `<String>{word}` seed).
pub fn generate_common_hebrew_typo_variations(word: &str) -> Vec<String> {
    if word.is_empty() {
        return vec![String::new()];
    }
    let graphemes: Vec<&str> = word.graphemes(true).collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    push_unique(&mut out, &mut seen, word.to_string());

    for i in 0..graphemes.len() {
        for sub in typo_substitutions(graphemes[i]) {
            let mut g = graphemes.clone();
            g[i] = sub;
            push_unique(&mut out, &mut seen, g.concat());
        }
    }
    for i in 0..graphemes.len().saturating_sub(1) {
        if graphemes[i] != graphemes[i + 1] {
            let mut g = graphemes.clone();
            g.swap(i, i + 1);
            push_unique(&mut out, &mut seen, g.concat());
        }
    }
    out
}

fn prioritized_insertion_positions(length: usize) -> Vec<usize> {
    if length == 0 {
        return vec![0];
    }
    let mut positions = vec![0, length];
    for offset in 1..length {
        positions.push(offset);
    }
    positions
}

/// Adds `value` if new and non-empty. Returns `true` if the caller should keep
/// generating, `false` once `max` distinct variations have been collected.
/// Mirrors the Dart `addVariation` stop semantics exactly.
fn add_capped(
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
    value: String,
    max: usize,
) -> bool {
    if value.is_empty() || seen.contains(&value) {
        return true;
    }
    seen.insert(value.clone());
    out.push(value);
    out.len() < max
}

/// Edit-distance-1 variations: common substitution/transposition, then single
/// deletion, then single insertion, capped at `max_variations`.
pub fn generate_typo_tolerance_variations(word: &str, max_variations: usize) -> Vec<String> {
    if word.is_empty() {
        return vec![String::new()];
    }
    let graphemes: Vec<&str> = word.graphemes(true).collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for v in generate_common_hebrew_typo_variations(word) {
        if !add_capped(&mut out, &mut seen, v, max_variations) {
            return out;
        }
    }
    for i in 0..graphemes.len() {
        let mut g = graphemes.clone();
        g.remove(i);
        if !g.is_empty() && !add_capped(&mut out, &mut seen, g.concat(), max_variations) {
            return out;
        }
    }
    for position in prioritized_insertion_positions(graphemes.len()) {
        for letter in INSERTION_LETTERS {
            let mut g = graphemes.clone();
            g.insert(position, letter);
            if !add_capped(&mut out, &mut seen, g.concat(), max_variations) {
                return out;
            }
        }
    }
    out
}

// ── Single-word pattern builders ─────────────────────────────────────────────────

fn create_prefix_pattern(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    format!(
        r"(ו|מ|דא|א|כש|כ|ב|ש|ל|ה|ד)?(כ|ב|ש|ל|ה|ד)?(ה)?{}",
        escape_regex(word)
    )
}

fn create_suffix_pattern(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    format!("{}{}", escape_regex(word), SUFFIX_PATTERN)
}

fn create_full_morphological_pattern(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    format!(
        "{}{}{}",
        PREFIX_GROUP,
        escape_regex(word),
        FULL_SUFFIX_PATTERN
    )
}

fn create_prefix_search_pattern(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    let len = word.chars().count();
    let e = escape_regex(word);
    if len <= 1 {
        format!(".{{0,5}}{}", e)
    } else if len <= 2 {
        format!(".{{0,4}}{}", e)
    } else if len <= 3 {
        format!(".{{0,3}}{}", e)
    } else {
        format!(".*{}", e)
    }
}

fn create_suffix_search_pattern(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    let len = word.chars().count();
    let e = escape_regex(word);
    if len <= 1 {
        format!("{}.{{0,7}}", e)
    } else if len <= 2 {
        format!("{}.{{0,6}}", e)
    } else if len <= 3 {
        format!("{}.{{0,5}}", e)
    } else {
        format!("{}.*", e)
    }
}

fn create_partial_word_pattern(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    let len = word.chars().count();
    let e = escape_regex(word);
    if len <= 3 {
        format!(".{{0,3}}{}.{{0,3}}", e)
    } else {
        format!(".{{0,2}}{}.{{0,2}}", e)
    }
}

/// כתיב מלא/חסר alternation. Unlike Dart, never wraps in `^…$` (see module docs).
fn create_full_partial_spelling_pattern(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    let escaped: Vec<String> = generate_full_partial_spelling_variations(word)
        .iter()
        .map(|v| escape_regex(v))
        .collect();
    format!("(?:{})", escaped.join("|"))
}

fn create_spelling_with_prefix_pattern(word: &str) -> String {
    spelling_combine(word, 10, create_prefix_pattern)
}

fn create_spelling_with_suffix_pattern(word: &str) -> String {
    spelling_combine(word, 10, create_suffix_pattern)
}

fn create_spelling_with_full_morphology_pattern(word: &str) -> String {
    spelling_combine(word, 8, create_full_morphological_pattern)
}

fn spelling_combine(word: &str, limit: usize, builder: fn(&str) -> String) -> String {
    if word.is_empty() {
        return String::new();
    }
    let variations = generate_full_partial_spelling_variations(word);
    let patterns: Vec<String> = variations.iter().take(limit).map(|v| builder(v)).collect();
    format!("({})", patterns.join("|"))
}

/// The `createSearchPattern` decision tree, priority order preserved verbatim.
pub fn create_search_pattern(
    word: &str,
    has_prefix: bool,
    has_suffix: bool,
    has_gram_prefix: bool,
    has_gram_suffix: bool,
    has_partial: bool,
    has_spelling: bool,
) -> String {
    if word.is_empty() {
        return String::new();
    }

    if has_spelling {
        let variations = generate_full_partial_spelling_variations(word);
        let has_morph_or_partial =
            has_gram_prefix || has_gram_suffix || has_prefix || has_suffix || has_partial;

        if has_prefix && has_suffix {
            join_variations(&variations, create_partial_word_pattern)
        } else if has_gram_prefix && has_gram_suffix {
            create_spelling_with_full_morphology_pattern(word)
        } else if has_prefix {
            join_variations(&variations, create_prefix_search_pattern)
        } else if has_suffix {
            join_variations(&variations, create_suffix_search_pattern)
        } else if has_gram_prefix {
            create_spelling_with_prefix_pattern(word)
        } else if has_gram_suffix {
            create_spelling_with_suffix_pattern(word)
        } else if has_partial {
            join_variations(&variations, create_partial_word_pattern)
        } else if has_morph_or_partial {
            // Unreachable given the branches above, kept for structural parity.
            create_full_partial_spelling_pattern(word)
        } else {
            create_full_partial_spelling_pattern(word)
        }
    } else if has_prefix && has_suffix {
        create_partial_word_pattern(word)
    } else if has_gram_prefix && has_gram_suffix {
        create_full_morphological_pattern(word)
    } else if has_prefix {
        create_prefix_search_pattern(word)
    } else if has_suffix {
        create_suffix_search_pattern(word)
    } else if has_gram_prefix {
        create_prefix_pattern(word)
    } else if has_gram_suffix {
        create_suffix_pattern(word)
    } else if has_partial {
        create_partial_word_pattern(word)
    } else {
        escape_regex(word)
    }
}

fn join_variations(variations: &[String], builder: fn(&str) -> String) -> String {
    let patterns: Vec<String> = variations.iter().map(|v| builder(v)).collect();
    format!("({})", patterns.join("|"))
}

// ── Option helpers ───────────────────────────────────────────────────────────────

fn opt_flag(options: Option<&HashMap<String, bool>>, key: &str) -> bool {
    options.and_then(|m| m.get(key)).copied().unwrap_or(false)
}

fn has_enabled_search_options(options: &HashMap<String, HashMap<String, bool>>) -> bool {
    !options.is_empty() && options.values().any(|wo| wo.values().any(|&v| v))
}

fn has_typo_tolerance_enabled(options: &HashMap<String, HashMap<String, bool>>) -> bool {
    has_enabled_search_options(options)
        && options
            .values()
            .any(|wo| wo.get(OPT_TYPO).copied().unwrap_or(false))
}

// ── Advanced regex term assembly (parity with buildAdvancedQuery, fuzzy=false) ──

pub fn build_advanced_regex_terms(
    words: &[String],
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
) -> Vec<String> {
    let mut regex_terms = Vec::new();

    for (i, word) in words.iter().enumerate() {
        let word_key = format!("{}_{}", word, i);
        let opts = search_options.get(&word_key);

        let has_prefix = opt_flag(opts, OPT_PREFIX);
        let has_suffix = opt_flag(opts, OPT_SUFFIX);
        let has_gram_prefix = opt_flag(opts, OPT_GRAM_PREFIX);
        let has_gram_suffix = opt_flag(opts, OPT_GRAM_SUFFIX);
        let has_typo = opt_flag(opts, OPT_TYPO);
        let has_spelling = opt_flag(opts, OPT_SPELLING);
        let has_partial = opt_flag(opts, OPT_PARTIAL);

        let mut all_options: Vec<String> = vec![word.clone()];
        if let Some(alts) = alternative_words.get(&(i as u32)) {
            all_options.extend(alts.iter().cloned());
        }
        let valid_options: Vec<String> = all_options
            .into_iter()
            .filter(|w| !w.trim().is_empty())
            .collect();

        if valid_options.is_empty() {
            regex_terms.push(word.clone());
            continue;
        }

        let max_variations = if has_typo { 48 } else { 20 };
        let mut variations: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for option in &valid_options {
            let expanded = if has_typo {
                generate_typo_tolerance_variations(option, MAX_TYPO_TOLERANCE_VARIATIONS)
            } else {
                vec![option.clone()]
            };
            for exp in expanded {
                let pattern = create_search_pattern(
                    &exp,
                    has_prefix,
                    has_suffix,
                    has_gram_prefix,
                    has_gram_suffix,
                    has_partial,
                    has_spelling,
                );
                push_unique(&mut variations, &mut seen, pattern);
            }
        }

        let limited: Vec<String> = variations
            .into_iter()
            .take(max_variations)
            .filter(|v| !v.trim().is_empty())
            .collect();
        if limited.is_empty() {
            continue;
        }
        let final_pattern = if limited.len() == 1 {
            limited.into_iter().next().unwrap()
        } else {
            format!("({})", limited.join("|"))
        };
        regex_terms.push(final_pattern);
    }

    regex_terms
}

fn get_max_custom_spacing(custom_spacing: &HashMap<String, String>, word_count: usize) -> u32 {
    let mut max_spacing = 0u32;
    for i in 0..word_count.saturating_sub(1) {
        let key = format!("{}-{}", i, i + 1);
        if let Some(value) = custom_spacing.get(&key) {
            if !value.is_empty() {
                let parsed = value.trim().parse::<i64>().unwrap_or(0);
                if parsed > 0 {
                    max_spacing = max_spacing.max(parsed as u32);
                }
            }
        }
    }
    max_spacing
}

/// Parity with `calculateMaxExpansions(fuzzy=false, ...)`.
pub fn calculate_max_expansions(
    term_count: usize,
    search_options: &HashMap<String, HashMap<String, bool>>,
    words: &[String],
) -> u32 {
    let mut has_suffix_or_prefix = false;
    let mut shortest = 10usize;
    for (i, word) in words.iter().enumerate() {
        let key = format!("{}_{}", word, i);
        let opts = search_options.get(&key);
        let len = word.chars().count();
        if opt_flag(opts, OPT_SUFFIX)
            || opt_flag(opts, OPT_PREFIX)
            || opt_flag(opts, OPT_GRAM_PREFIX)
            || opt_flag(opts, OPT_GRAM_SUFFIX)
            || opt_flag(opts, OPT_PARTIAL)
        {
            has_suffix_or_prefix = true;
            shortest = shortest.min(len);
        } else if opt_flag(opts, OPT_TYPO) {
            shortest = shortest.min(len);
        }
    }

    if has_typo_tolerance_enabled(search_options) {
        if term_count > 1 {
            100
        } else {
            50
        }
    } else if has_suffix_or_prefix {
        if shortest <= 1 {
            2000
        } else if shortest <= 2 {
            3000
        } else if shortest <= 3 {
            4000
        } else {
            5000
        }
    } else if term_count > 1 {
        100
    } else {
        10
    }
}

/// Full parity with `prepareQueryParams(fuzzy=false, ...)`: chooses between the
/// advanced (per-word) builder and the plain raw-word path, computes the
/// effective slop and `max_expansions`.
pub fn prepare_advanced_query(
    query: &str,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
) -> AdvancedQuery {
    let words = split_query_words(query);
    let has_custom_spacing = !custom_spacing.is_empty();
    let has_alternative_words = !alternative_words.is_empty();
    let has_search_options = has_enabled_search_options(search_options);

    let (regex_terms, slop): (Vec<String>, u32) = if has_alternative_words || has_search_options {
        let terms = build_advanced_regex_terms(&words, alternative_words, search_options);
        let slop = if words.len() <= 1 {
            0
        } else if has_custom_spacing {
            get_max_custom_spacing(custom_spacing, words.len())
        } else {
            distance
        };
        (terms, slop)
    } else if words.len() == 1 {
        (words.clone(), 0)
    } else if has_custom_spacing {
        let slop = get_max_custom_spacing(custom_spacing, words.len());
        (words.clone(), slop)
    } else {
        (words.clone(), distance)
    };

    let max_expansions = calculate_max_expansions(regex_terms.len(), search_options, &words);
    AdvancedQuery {
        regex_terms,
        slop,
        max_expansions,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &[(&str, bool)])]) -> HashMap<String, HashMap<String, bool>> {
        pairs
            .iter()
            .map(|(k, flags)| {
                (
                    k.to_string(),
                    flags.iter().map(|(f, v)| (f.to_string(), *v)).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn sanitize_converts_and_strips() {
        assert_eq!(sanitize_query("שלום, עולם!"), "שלום עולם");
        assert_eq!(sanitize_query("א־ב"), "א ב"); // maqaf -> space
        assert_eq!(sanitize_query("רמב״ם"), "רמב\"ם"); // gershayim -> "
        assert_eq!(sanitize_query("תוס׳"), "תוס'"); // geresh -> '
    }

    #[test]
    fn split_words_handles_geresh_and_gershayim() {
        assert_eq!(split_query_words("שלום עולם"), vec!["שלום", "עולם"]);
        assert_eq!(split_query_words("תוס'"), vec!["תוס'"]); // trailing geresh kept
        assert_eq!(split_query_words("ז\"ל"), vec!["ז", "ל"]); // gershayim splits
        assert_eq!(split_query_words("ד'אש"), vec!["ד", "אש"]); // mid geresh splits
        assert_eq!(split_query_words("רמב״ם"), vec!["רמב", "ם"]); // ״ -> " -> splits
    }

    #[test]
    fn spelling_variations_order_and_content() {
        // "בוא": optional ו at index 1 -> omit first (mask 0), then include.
        assert_eq!(
            generate_full_partial_spelling_variations("בוא"),
            vec!["בא", "בוא"]
        );
        // No optional chars -> just the word.
        assert_eq!(
            generate_full_partial_spelling_variations("גמל"),
            vec!["גמל"]
        );
    }

    #[test]
    fn typo_variations_cover_substitution_transposition() {
        let v = generate_common_hebrew_typo_variations("בא");
        assert_eq!(v[0], "בא"); // seed first
        assert!(v.contains(&"וא".to_string())); // ב -> ו
        assert!(v.contains(&"בע".to_string())); // א -> ע
        assert!(v.contains(&"בה".to_string())); // א -> ה
        assert!(v.contains(&"אב".to_string())); // transposition
    }

    #[test]
    fn typo_tolerance_is_capped() {
        let v = generate_typo_tolerance_variations("שלום", 48);
        assert!(v.len() <= 48);
        assert!(v.contains(&"שלם".to_string())); // deletion of ו
    }

    #[test]
    fn prefix_pattern_matches_dart() {
        assert_eq!(
            create_prefix_pattern("ספר"),
            "(ו|מ|דא|א|כש|כ|ב|ש|ל|ה|ד)?(כ|ב|ש|ל|ה|ד)?(ה)?ספר"
        );
    }

    #[test]
    fn spelling_only_pattern_has_no_anchors() {
        let p = create_search_pattern("בוא", false, false, false, false, false, true);
        assert!(!p.contains('^'));
        assert!(!p.contains('$'));
        assert!(p.starts_with("(?:"));
    }

    #[test]
    fn default_pattern_is_plain_word() {
        assert_eq!(
            create_search_pattern("שלום", false, false, false, false, false, false),
            "שלום"
        );
    }

    #[test]
    fn advanced_terms_plain_when_no_options() {
        // With options present but none enabled for the word, falls back to the word.
        let terms = build_advanced_regex_terms(
            &["שלום".to_string(), "עולם".to_string()],
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(terms, vec!["שלום", "עולם"]);
    }

    #[test]
    fn advanced_terms_apply_grammatical_prefix() {
        let so = opts(&[("ספר_0", &[(OPT_GRAM_PREFIX, true)])]);
        let terms = build_advanced_regex_terms(&["ספר".to_string()], &HashMap::new(), &so);
        assert_eq!(terms.len(), 1);
        assert!(terms[0].ends_with("ספר"));
        assert!(terms[0].starts_with("(ו|מ|דא"));
    }

    #[test]
    fn advanced_terms_apply_alternatives() {
        let mut alts = HashMap::new();
        alts.insert(0u32, vec!["מלך".to_string()]);
        // Need enabled options OR alternatives to trigger advanced path; alternatives suffice.
        let terms = build_advanced_regex_terms(&["שר".to_string()], &alts, &HashMap::new());
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0], "(שר|מלך)");
    }

    #[test]
    fn max_expansions_matches_dart_rules() {
        let empty = HashMap::new();
        assert_eq!(
            calculate_max_expansions(1, &empty, &["שלום".to_string()]),
            10
        );
        assert_eq!(
            calculate_max_expansions(2, &empty, &["שלום".to_string(), "עולם".to_string()]),
            100
        );
        let so = opts(&[("ספר_0", &[(OPT_GRAM_PREFIX, true)])]);
        // shortest word length 3 -> 4000
        assert_eq!(calculate_max_expansions(1, &so, &["ספר".to_string()]), 4000);
        let typo = opts(&[("ספר_0", &[(OPT_TYPO, true)])]);
        assert_eq!(calculate_max_expansions(1, &typo, &["ספר".to_string()]), 50);
    }

    #[test]
    fn prepare_advanced_slop_and_terms() {
        // Two words, no options, default distance carries to slop.
        let q = prepare_advanced_query(
            "שלום עולם",
            3,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(q.regex_terms, vec!["שלום", "עולם"]);
        assert_eq!(q.slop, 3);
        assert_eq!(q.max_expansions, 100);

        // Custom spacing overrides distance.
        let mut spacing = HashMap::new();
        spacing.insert("0-1".to_string(), "5".to_string());
        let q2 = prepare_advanced_query("שלום עולם", 3, &spacing, &HashMap::new(), &HashMap::new());
        assert_eq!(q2.slop, 5);
    }
}
