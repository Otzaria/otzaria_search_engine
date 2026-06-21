//! Hebrew search-query logic. Originally ported from the otzaria app's Dart
//! layer (`lib/search/search_query_builder.dart` +
//! `lib/search/utils/regex_patterns.dart`); this Rust engine is now the
//! authoritative implementation and intentionally diverges where the Dart
//! patterns were wrong or wasteful for this index.
//!
//! This module turns a high-level query string + per-word options into the
//! Tantivy-regex term strings, slop and `max_expansions` that the advanced
//! search mode feeds to `RegexQuery` / `RegexPhraseQuery`.
//!
//! Pure string logic only — no Tantivy dependency. Deliberate divergences from
//! the original Dart, all tuned to tantivy-fst + the nikud-free index:
//! - Patterns are nikud-free: the index strips nikud, so vocalized alternatives
//!   could never match and only bloat the automaton.
//! - Groups are non-capturing `(?:…)` and never anchored (`^…$`): tantivy-fst
//!   matches whole-term on acceptance, rejects anchors, and ignores captures.
//! - Prefix/suffix windows are bounded (`.{0,n}`, never `.*`), and the combined
//!   per-word term is length-capped, to keep the compiled DFA under tantivy-fst's
//!   1000-state limit (a term that exceeds it fails to compile).

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

// Morphological affix groups. The index term dictionary is nikud-free (the app
// strips nikud before indexing and the query is normalized the same way), so
// these patterns are intentionally nikud-free: any vocalized alternative would
// never match a real term and would only enlarge the compiled automaton. They
// also use non-capturing groups `(?:…)` — tantivy-fst matches on acceptance and
// ignores captures, so capturing groups only add states for nothing.
//
// `FULL_SUFFIX_PATTERN` is the richer set (used for full prefix+suffix
// morphology); it is `SUFFIX_PATTERN` plus a few extra endings (יות, יא, תא).
const SUFFIX_PATTERN: &str = r"(?:ותי|ותיך|ותיו|ותיה|ותינו|ותיכם|ותיכן|ותיהם|ותיהן|יי|יך|יו|יה|ינו|יכם|יכן|יהם|יהן|י|ך|ו|ה|נו|כם|כן|ם|ן|ים|ות)?";
const PREFIX_GROUP: &str = r"(?:ו|מ|כ|ב|ש|ל|ה|ד)?(?:כ|ב|ש|ל|ה|ד)?(?:ה)?";
const FULL_SUFFIX_PATTERN: &str = r"(?:ותי|ותיך|ותיו|ותיה|ותינו|ותיכם|ותיכן|ותיהם|ותיהן|יות|יי|יך|יו|יה|יא|תא|ינו|יכם|יכן|יהם|יהן|י|ך|ו|ה|נו|כם|כן|ם|ן|ים|ות)?";

const INSERTION_LETTERS: &[&str] = &[
    "ו", "י", "א", "ה", "פ", "ל", "מ", "נ", "ב", "כ", "ש", "ת", "ר",
];

const MAX_TYPO_TOLERANCE_VARIATIONS: usize = 48;
/// Cap on pattern variations per word when typo tolerance is off.
const MAX_PATTERN_VARIATIONS: usize = 20;

/// Upper bound on the total length (in characters) of a single word's combined
/// regex term. tantivy-fst compiles each term to a DFA capped at 1000 states
/// (`STATE_LIMIT`); a term that exceeds it fails to compile and would error the
/// whole search. The state count grows roughly with the pattern length, so
/// bounding length keeps the DFA safely under the limit even when many large
/// (morphological) branches are combined (e.g. typo tolerance + grammatical
/// affixes). Branches beyond the budget are dropped, trading recall for a query
/// that runs instead of failing.
const MAX_PATTERN_TOTAL_LEN: usize = 1000;

/// Cap on the number of כתיב מלא/חסר spelling variations folded into a single
/// pattern. The generator is exponential (2^n in the count of optional י/ו),
/// so an unbounded fold could blow past the DFA limit on its own.
const MAX_SPELLING_VARIATIONS: usize = 16;

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

/// Normalises text to the index term dictionary's shape: strips nikud and
/// lowercases (the `"default"` analyzer lowercases at index time; Hebrew is
/// unaffected, Latin tokens would otherwise silently never match).
pub fn normalize_for_index(text: &str) -> String {
    strip_nikud(text).to_lowercase()
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
        // The dotted shin/sin forms (שׁ/שׂ) carry nikud-range marks that never
        // appear in the nikud-free index, and the normalized query never feeds
        // them in as input — so they are omitted as substitution targets and
        // have no match arms of their own.
        "ס" => &["ש"],
        "ש" => &["ס"],
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
        r"(?:ו|מ|דא|א|כש|כ|ב|ש|ל|ה|ד)?(?:כ|ב|ש|ל|ה|ד)?(?:ה)?{}",
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
    // Bounded leading window instead of `.*`: a `.*`-prefixed regex forces the
    // term-dictionary scan to consider every key (tantivy-fst warns about this
    // explicitly), while a Hebrew prefix stack is at most a few letters.
    let len = word.chars().count();
    let e = escape_regex(word);
    if len <= 1 {
        format!(".{{0,5}}{}", e)
    } else if len <= 2 {
        format!(".{{0,4}}{}", e)
    } else {
        format!(".{{0,3}}{}", e)
    }
}

fn create_suffix_search_pattern(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    // Bounded trailing window instead of `.*` (see create_prefix_search_pattern);
    // a window of 5 still covers the longest real Hebrew suffixes (e.g. ותיהם).
    let len = word.chars().count();
    let e = escape_regex(word);
    if len <= 1 {
        format!("{}.{{0,7}}", e)
    } else if len <= 2 {
        format!("{}.{{0,6}}", e)
    } else {
        format!("{}.{{0,5}}", e)
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
        .take(MAX_SPELLING_VARIATIONS)
        .map(|v| escape_regex(v))
        .collect();
    format!("(?:{})", escaped.join("|"))
}

fn create_spelling_with_prefix_pattern(word: &str) -> String {
    spelling_combine(word, 10, create_prefix_pattern)
}

fn create_spelling_with_suffix_pattern(word: &str) -> String {
    // Each branch here carries the full suffix alternation, so fewer branches
    // than the prefix variant to keep the compiled DFA bounded.
    spelling_combine(word, 6, create_suffix_pattern)
}

fn create_spelling_with_full_morphology_pattern(word: &str) -> String {
    // The largest per-branch builder (prefix groups + full suffix set), so the
    // tightest branch cap.
    spelling_combine(word, 4, create_full_morphological_pattern)
}

fn spelling_combine(word: &str, limit: usize, builder: fn(&str) -> String) -> String {
    if word.is_empty() {
        return String::new();
    }
    let variations = generate_full_partial_spelling_variations(word);
    let patterns: Vec<String> = variations.iter().take(limit).map(|v| builder(v)).collect();
    format!("(?:{})", patterns.join("|"))
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
        // Cap the (exponential) spelling set before fanning each variation out
        // through a per-variation builder, so the joined pattern stays bounded.
        let variations: Vec<String> = generate_full_partial_spelling_variations(word)
            .into_iter()
            .take(MAX_SPELLING_VARIATIONS)
            .collect();
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
    format!("(?:{})", patterns.join("|"))
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
            // User-supplied alternatives go straight into regex patterns;
            // normalise them like the query words so they can match the index.
            all_options.extend(alts.iter().map(|a| normalize_for_index(a)));
        }
        let valid_options: Vec<String> = all_options
            .into_iter()
            .filter(|w| !w.trim().is_empty())
            .collect();

        if valid_options.is_empty() {
            regex_terms.push(escape_regex(word));
            continue;
        }

        let max_variations = if has_typo {
            MAX_TYPO_TOLERANCE_VARIATIONS
        } else {
            MAX_PATTERN_VARIATIONS
        };
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

        // Keep variations until we hit the count cap OR the combined-length
        // budget — whichever comes first. The length budget is what keeps the
        // compiled DFA under tantivy-fst's state limit when individual branches
        // are large (morphology) and numerous (typo tolerance); at least one
        // branch is always kept so the term is never empty.
        let mut limited: Vec<String> = Vec::new();
        let mut total_len = 0usize;
        for v in variations {
            if v.trim().is_empty() {
                continue;
            }
            if limited.len() >= max_variations {
                break;
            }
            let len = v.chars().count();
            if !limited.is_empty() && total_len + len > MAX_PATTERN_TOTAL_LEN {
                break;
            }
            total_len += len;
            limited.push(v);
        }
        if limited.is_empty() {
            continue;
        }
        let final_pattern = if limited.len() == 1 {
            limited.into_iter().next().unwrap()
        } else {
            format!("(?:{})", limited.join("|"))
        };
        // Defensive fallback: if a single oversized branch still pushed the term
        // past the budget, match the literal word (always small and compilable)
        // instead of risking a term that exceeds the DFA limit and errors.
        let final_pattern = if final_pattern.chars().count() > MAX_PATTERN_TOTAL_LEN {
            escape_regex(word)
        } else {
            final_pattern
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
                    // Saturate instead of `as`-casting, which would wrap
                    // values above u32::MAX around to tiny slops.
                    max_spacing = max_spacing.max(u32::try_from(parsed).unwrap_or(u32::MAX));
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
    // The index's "default" analyzer lowercases, and indexed text is
    // nikud-stripped; normalise the query the same way or the produced regex
    // terms can never match an index term (silent zero results).
    let normalized = normalize_for_index(query);
    let words = split_query_words(&normalized);
    let has_custom_spacing = !custom_spacing.is_empty();
    let has_alternative_words = !alternative_words.is_empty();
    let has_search_options = has_enabled_search_options(search_options);

    // Plain-path words become regex patterns as-is; escape defensively even
    // though sanitised tokens cannot contain regex metacharacters today.
    let escaped = || words.iter().map(|w| escape_regex(w)).collect::<Vec<_>>();
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
        (escaped(), 0)
    } else if has_custom_spacing {
        let slop = get_max_custom_spacing(custom_spacing, words.len());
        (escaped(), slop)
    } else {
        (escaped(), distance)
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
    fn prefix_pattern_groups_are_noncapturing() {
        assert_eq!(
            create_prefix_pattern("ספר"),
            "(?:ו|מ|דא|א|כש|כ|ב|ש|ל|ה|ד)?(?:כ|ב|ש|ל|ה|ד)?(?:ה)?ספר"
        );
    }

    #[test]
    fn typo_substitutions_are_nikud_free() {
        // ס/ש confuse with each other but never with the dotted (vocalized)
        // shin/sin forms, which can't exist in the nikud-free index.
        let v = generate_common_hebrew_typo_variations("ס");
        assert_eq!(v, vec!["ס", "ש"]);
        // No produced variation may carry a nikud-range mark (U+0591–U+05C7).
        let v2 = generate_common_hebrew_typo_variations("שבת");
        for variant in v2 {
            assert!(
                !variant
                    .chars()
                    .any(|c| ('\u{0591}'..='\u{05C7}').contains(&c)),
                "variant {variant:?} contains nikud"
            );
        }
    }

    #[test]
    fn search_patterns_avoid_unbounded_star() {
        // Prefix/suffix windows must be bounded (no `.*`) so the dictionary scan
        // can't be forced to visit every term.
        let p = create_search_pattern("ארוכה", true, false, false, false, false, false);
        assert!(!p.contains(".*"), "prefix pattern still uses .*: {p}");
        let s = create_search_pattern("ארוכה", false, true, false, false, false, false);
        assert!(!s.contains(".*"), "suffix pattern still uses .*: {s}");
    }

    #[test]
    fn heavy_option_combo_stays_within_budget() {
        // typo + grammatical prefix + grammatical suffix is the worst case for
        // automaton size; the produced term must stay under the length budget so
        // it compiles instead of blowing past tantivy-fst's DFA state limit.
        let so = opts(&[(
            "ספר_0",
            &[
                (OPT_TYPO, true),
                (OPT_GRAM_PREFIX, true),
                (OPT_GRAM_SUFFIX, true),
            ],
        )]);
        let terms = build_advanced_regex_terms(&["ספר".to_string()], &HashMap::new(), &so);
        assert_eq!(terms.len(), 1);
        assert!(!terms[0].is_empty());
        assert!(
            terms[0].chars().count() <= MAX_PATTERN_TOTAL_LEN,
            "term length {} exceeds budget",
            terms[0].chars().count()
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
        assert!(terms[0].starts_with("(?:ו|מ|דא"));
    }

    #[test]
    fn advanced_terms_apply_alternatives() {
        let mut alts = HashMap::new();
        alts.insert(0u32, vec!["מלך".to_string()]);
        // Need enabled options OR alternatives to trigger advanced path; alternatives suffice.
        let terms = build_advanced_regex_terms(&["שר".to_string()], &alts, &HashMap::new());
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0], "(?:שר|מלך)");
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

    #[test]
    fn prepare_advanced_normalizes_nikud_and_case() {
        // Vocalized query: regex terms must be nikud-free like the index.
        let q = prepare_advanced_query("סֵפֶר", 0, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(q.regex_terms, vec!["ספר"]);

        // Latin tokens are lowercased like the "default" analyzer does.
        let q2 = prepare_advanced_query(
            "Torah",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(q2.regex_terms, vec!["torah"]);

        // User-supplied alternative words are normalized too.
        let mut alts = HashMap::new();
        alts.insert(0u32, vec!["תּוֹרָה".to_string()]);
        let q3 = prepare_advanced_query("ספר", 0, &HashMap::new(), &alts, &HashMap::new());
        assert_eq!(q3.regex_terms, vec!["(?:ספר|תורה)"]);
    }
}
