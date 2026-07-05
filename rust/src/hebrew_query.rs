//! Hebrew search-query logic for the Otzaria search engine.
//!
//! This module turns a raw query string + per-word UI options into the Tantivy
//! regex terms, phrase slop, and `max_expansions` that `RegexPhraseQuery`
//! needs. It is pure string logic with no Tantivy dependency.
//!
//! # Index contract
//!
//! The `"text"` field is indexed after stripping Hebrew nikud (U+0591–U+05C7)
//! and lowercasing. Every pattern produced here is also nikud-free and
//! lowercased so that query terms can actually match index terms.
//!
//! # tantivy-fst constraints
//!
//! Each regex term is compiled to a DFA by tantivy-fst. Two hard limits apply:
//!
//! * The DFA state count is capped at 1 000. A pattern whose length grows
//!   unboundedly (e.g. `.*`) can blow past this and cause a compile error at
//!   query time. Every window/wildcard in this module is therefore bounded.
//! * `RegexPhraseQuery::set_max_expansions` guards against runaway dictionary
//!   scans; we set it conservatively per query shape.
//!
//! # Regex dialect
//!
//! tantivy-fst matches whole terms (acceptance semantics), so anchors (`^$`)
//! are unnecessary and rejected. All groups are non-capturing `(?:…)` because
//! captures add DFA states without benefiting whole-term matching.
//!
//! # Provenance
//!
//! Originally ported from the Otzaria app's Dart layer
//! (`lib/search/search_query_builder.dart` + `lib/search/utils/regex_patterns.dart`);
//! this Rust engine is now the authoritative implementation. Comments that say
//! "Mirrors the Dart `…`" name the specific Dart symbol each piece corresponds
//! to, for cross-checking parity. The patterns intentionally diverge from the
//! Dart where it was wrong or wasteful for this nikud-free index (see above).

use std::collections::{HashMap, HashSet};

use unicode_segmentation::UnicodeSegmentation;

// ── Search-option UI keys (must match the Dart layer exactly) ──────────────

pub const OPT_TYPO: &str = "שגיאות כתיב";
const OPT_PREFIX: &str = "קידומות";
const OPT_SUFFIX: &str = "סיומות";
const OPT_GRAM_PREFIX: &str = "קידומות דקדוקיות";
const OPT_GRAM_SUFFIX: &str = "סיומות דקדוקיות";
const OPT_SPELLING: &str = "כתיב מלא/חסר";
const OPT_PARTIAL: &str = "חלק ממילה";

// ── Morphological affix patterns ──────────────────────────────────────────
//
// Nikud-free (see index contract above). Non-capturing groups. No anchors.
//
// There are intentionally TWO prefix groups:
//   * `GRAM_PREFIX_GROUP` — the richer set used by the standalone grammatical
//     prefix option (קידומות דקדוקיות). Its first alternation additionally
//     carries דא, א, כש so that Aramaic דא־ / bare א־ / כש־ prefixed forms can
//     match. Keep these distinct from PREFIX_GROUP.
//   * `PREFIX_GROUP` — the leaner set used by the full prefix+suffix morphology
//     builder, where the suffix half already widens recall, so the extra
//     first-position alternatives are not needed.
//
// `FULL_SUFFIX_PATTERN` extends `SUFFIX_PATTERN` with three rare endings
// (יות, יא, תא) used only when the full prefix+suffix morphology is active.

const GRAM_PREFIX_GROUP: &str = r"(?:ו|מ|דא|א|כש|כ|ב|ש|ל|ה|ד)?(?:כ|ב|ש|ל|ה|ד)?(?:ה)?";

const PREFIX_GROUP: &str = r"(?:ו|מ|כ|ב|ש|ל|ה|ד)?(?:כ|ב|ש|ל|ה|ד)?(?:ה)?";

const SUFFIX_PATTERN: &str = r"(?:ותי|ותיך|ותיו|ותיה|ותינו|ותיכם|ותיכן|ותיהם|ותיהן|יי|יך|יו|יה|ינו|יכם|יכן|יהם|יהן|י|ך|ו|ה|נו|כם|כן|ם|ן|ים|ות)?";

const FULL_SUFFIX_PATTERN: &str = r"(?:ותי|ותיך|ותיו|ותיה|ותינו|ותיכם|ותיכן|ותיהם|ותיהן|יות|יי|יך|יו|יה|יא|תא|ינו|יכם|יכן|יהם|יהן|י|ך|ו|ה|נו|כם|כן|ם|ן|ים|ות)?";

// Letters tried for single-insertion typo variants (ordered by frequency).
const INSERTION_LETTERS: &[&str] = &[
    "ו", "י", "א", "ה", "פ", "ל", "מ", "נ", "ב", "כ", "ש", "ת", "ר",
];

// ── Budget constants ───────────────────────────────────────────────────────

/// Maximum DFA-pattern length per word. The tantivy-fst state cap (~1 000)
/// grows roughly with pattern length; staying under this keeps compilation
/// safe even when many morphological branches are joined.
const MAX_PATTERN_CHARS: usize = 1_000;

/// Variation cap when typo tolerance is active (substitution + deletion +
/// insertion can produce many candidates; we keep the most useful ones).
const MAX_TYPO_VARIATIONS: usize = 48;

/// Variation cap without typo tolerance (spelling/morphological combos are
/// naturally smaller, so a tighter budget still covers them fully).
const MAX_NORMAL_VARIATIONS: usize = 20;

/// Cap on כתיב מלא/חסר branches folded into one pattern. The generator is
/// 2^n in the count of optional ו/י letters; this keeps it polynomial.
/// Shared with the display-highlight builder so search and highlighting fan
/// out identically.
pub(crate) const MAX_SPELLING_BRANCHES: usize = 16;

// ── Public result type ─────────────────────────────────────────────────────

/// Everything the Tantivy layer needs to execute an advanced search.
pub struct AdvancedQuery {
    /// One regex pattern per query word (in order). Each pattern is a
    /// tantivy-fst–compatible whole-term regex.
    pub regex_terms: Vec<String>,
    /// Maximum allowed word-position gap between adjacent terms (phrase slop).
    pub slop: u32,
    /// Term-dictionary expansion limit passed to `RegexPhraseQuery`.
    pub max_expansions: u32,
}

// ── Per-word search flags ──────────────────────────────────────────────────

/// Typed representation of the per-word option checkboxes in the Flutter UI.
///
/// Constructed by [`WordFlags::from_map`] from the raw
/// `HashMap<String, bool>` the FFI layer delivers.
#[derive(Default, Clone)]
pub struct WordFlags {
    pub typo: bool,
    pub prefix: bool,
    pub suffix: bool,
    pub gram_prefix: bool,
    pub gram_suffix: bool,
    pub spelling: bool,
    pub partial: bool,
}

impl WordFlags {
    /// Parse from the Flutter-side `HashMap<String, bool>`.
    pub fn from_map(map: &HashMap<String, bool>) -> Self {
        let get = |key: &str| map.get(key).copied().unwrap_or(false);
        Self {
            typo: get(OPT_TYPO),
            prefix: get(OPT_PREFIX),
            suffix: get(OPT_SUFFIX),
            gram_prefix: get(OPT_GRAM_PREFIX),
            gram_suffix: get(OPT_GRAM_SUFFIX),
            spelling: get(OPT_SPELLING),
            partial: get(OPT_PARTIAL),
        }
    }

    fn max_variations(&self) -> usize {
        if self.typo {
            MAX_TYPO_VARIATIONS
        } else {
            MAX_NORMAL_VARIATIONS
        }
    }
}

// ── Tokenisation ───────────────────────────────────────────────────────────

/// Normalises punctuation exactly like the Dart `sanitizeQuery`:
/// `״→"`, `׳→'`, `־`/`-`→space; strips `,;!?:*()[]{}^$|\+.~\``;
/// collapses whitespace runs to a single space; trims.
pub fn sanitize_query(query: &str) -> String {
    const STRIP: &[char] = &[
        ',', ';', '!', '?', ':', '*', '(', ')', '[', ']', '{', '}', '^', '$', '|', '\\', '+', '.',
        '~', '`',
    ];
    let mut buf = String::with_capacity(query.len());
    for ch in query.chars() {
        match ch {
            '\u{05F4}' => buf.push('"'),
            '\u{05F3}' => buf.push('\''),
            '\u{05BE}' | '-' => buf.push(' '),
            c if STRIP.contains(&c) => {}
            c => buf.push(c),
        }
    }
    collapse_whitespace(&buf)
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev {
                out.push(' ');
                prev = true;
            }
        } else {
            out.push(ch);
            prev = false;
        }
    }
    out.trim().to_string()
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || ('\u{05D0}'..='\u{05EA}').contains(&c)
        || ('\u{0590}'..='\u{05C7}').contains(&c)
}

/// Splits a sanitised query into word tokens, mirroring the `HebrewTokenizer`
/// the `text` field is indexed with (see `crate::hebrew_tokenizer`):
/// * `"` always separates (`ז"ל` → `ז`, `ל`), exactly like the tokenizer.
/// * `'` between word characters separates too (`ד'אש` → `ד`, `אש`); only a
///   trailing `'` is absorbed as the token's last character (`תוס'`).
///
/// `sanitize_query` has already normalised `״`→`"` and `׳`→`'`, so query
/// tokens line up with the ASCII-geresh index terms.
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
            // A geresh right after the word that is NOT followed by another
            // word character is trailing — absorb it, like the tokenizer.
            if i < chars.len()
                && chars[i] == '\''
                && !(i + 1 < chars.len() && is_word_char(chars[i + 1]))
            {
                i += 1;
            }
            words.push(chars[start..i].iter().collect());
        } else {
            i += 1;
        }
    }
    words
}

/// Strips Hebrew nikud and cantillation marks (U+0591–U+05C7), after folding
/// presentation-form ligatures so their embedded marks are stripped too.
pub fn strip_nikud(text: &str) -> String {
    fold_presentation_forms(text)
        .chars()
        .filter(|c| !('\u{0591}'..='\u{05C7}').contains(c))
        .collect()
}

/// Normalises text to the index term dictionary's shape: strips nikud then
/// lowercases. This is how the `"hebrew"` analyzer maps indexed text to terms.
pub(crate) fn normalize_for_index(text: &str) -> String {
    strip_nikud(text).to_lowercase()
}

// ── Hebrew character classes & folding ──────────────────────────────────────

/// ניקוד וטעמים *הצמודים לאות* — U+0591–U+05C7 ללא המפרידים שבטווח:
/// מקף (U+05BE), פסק (U+05C0), סוף-פסוק (U+05C3) ונו"ן הפוכה (U+05C6).
/// המפרידים הם פיסוק גלוי לעין ולכן נשמרים בטקסט התצוגה ושוברים טוקנים.
#[inline]
pub(crate) fn is_attached_mark(c: char) -> bool {
    matches!(c, '\u{0591}'..='\u{05C7}')
        && !matches!(c, '\u{05BE}' | '\u{05C0}' | '\u{05C3}' | '\u{05C6}')
}

/// מסיר ניקוד וטעמים צמודים בלבד ([`is_attached_mark`]) — משאיר מקף, פסק
/// וסוף-פסוק שהם פיסוק, לא ניקוד.
pub fn strip_attached_marks(text: &str) -> String {
    text.chars().filter(|c| !is_attached_mark(*c)).collect()
}

/// הפירוק הקנוני (NFKD) של Hebrew Presentation Forms — U+FB1D–U+FB4F.
/// גופנים רבים חסרים את הגליפים האלה (יִ הוצגה כ"?") ומילון הטרמים לעולם
/// אינו מכיל אותם, ולכן גם התצוגה וגם הטוקנים מפרקים אותם לאות + סימן.
/// מחזיר `None` לתו רגיל. U+FB29 (סימן פלוס חלופי) נשאר כמות שהוא.
pub(crate) fn fold_presentation_form(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{FB1D}' => "\u{05D9}\u{05B4}",
        '\u{FB1E}' => "", // varika — סימן צמוד ללא פירוק, מושמט
        '\u{FB1F}' => "\u{05F2}\u{05B7}",
        '\u{FB20}' => "\u{05E2}",
        '\u{FB21}' => "\u{05D0}",
        '\u{FB22}' => "\u{05D3}",
        '\u{FB23}' => "\u{05D4}",
        '\u{FB24}' => "\u{05DB}",
        '\u{FB25}' => "\u{05DC}",
        '\u{FB26}' => "\u{05DD}",
        '\u{FB27}' => "\u{05E8}",
        '\u{FB28}' => "\u{05EA}",
        '\u{FB2A}' => "\u{05E9}\u{05C1}",
        '\u{FB2B}' => "\u{05E9}\u{05C2}",
        '\u{FB2C}' => "\u{05E9}\u{05BC}\u{05C1}",
        '\u{FB2D}' => "\u{05E9}\u{05BC}\u{05C2}",
        '\u{FB2E}' => "\u{05D0}\u{05B7}",
        '\u{FB2F}' => "\u{05D0}\u{05B8}",
        '\u{FB30}' => "\u{05D0}\u{05BC}",
        '\u{FB31}' => "\u{05D1}\u{05BC}",
        '\u{FB32}' => "\u{05D2}\u{05BC}",
        '\u{FB33}' => "\u{05D3}\u{05BC}",
        '\u{FB34}' => "\u{05D4}\u{05BC}",
        '\u{FB35}' => "\u{05D5}\u{05BC}",
        '\u{FB36}' => "\u{05D6}\u{05BC}",
        '\u{FB38}' => "\u{05D8}\u{05BC}",
        '\u{FB39}' => "\u{05D9}\u{05BC}",
        '\u{FB3A}' => "\u{05DA}\u{05BC}",
        '\u{FB3B}' => "\u{05DB}\u{05BC}",
        '\u{FB3C}' => "\u{05DC}\u{05BC}",
        '\u{FB3E}' => "\u{05DE}\u{05BC}",
        '\u{FB40}' => "\u{05E0}\u{05BC}",
        '\u{FB41}' => "\u{05E1}\u{05BC}",
        '\u{FB43}' => "\u{05E3}\u{05BC}",
        '\u{FB44}' => "\u{05E4}\u{05BC}",
        '\u{FB46}' => "\u{05E6}\u{05BC}",
        '\u{FB47}' => "\u{05E7}\u{05BC}",
        '\u{FB48}' => "\u{05E8}\u{05BC}",
        '\u{FB49}' => "\u{05E9}\u{05BC}",
        '\u{FB4A}' => "\u{05EA}\u{05BC}",
        '\u{FB4B}' => "\u{05D5}\u{05B9}",
        '\u{FB4C}' => "\u{05D1}\u{05BF}",
        '\u{FB4D}' => "\u{05DB}\u{05BF}",
        '\u{FB4E}' => "\u{05E4}\u{05BF}",
        '\u{FB4F}' => "\u{05D0}\u{05DC}",
        _ => return None,
    })
}

/// מפרק את כל תווי ה-Presentation Forms שבטקסט ([`fold_presentation_form`]).
pub fn fold_presentation_forms(text: &str) -> String {
    if !text.chars().any(|c| fold_presentation_form(c).is_some()) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match fold_presentation_form(c) {
            Some(folded) => out.push_str(folded),
            None => out.push(c),
        }
    }
    out
}

// ── Document ingestion normalisation ───────────────────────────────────────
//
// The single source of truth for the text normalisation applied to every line
// before it is stored in the index. The stored text is also what search
// results DISPLAY, so ingestion keeps punctuation and folds only what must
// never be shown or matched: HTML, nikud/cantillation, presentation forms.
// Term-dictionary equality with the sanitised query side is guaranteed by the
// `HebrewTokenizer`, which treats the punctuation `sanitize_query` strips as
// "transparent" (see `crate::hebrew_tokenizer`).

/// Strips HTML tags and entities, mirroring the Dart `stripHtmlIfNeeded`:
/// the four whitespace entities become a space first (so adjacent words are
/// not merged), then `<…>` tags and remaining `&…;` entities are removed.
///
/// Char-based to match the Dart regex `<[^>]*>|&[^;]+;`: a `<` is dropped
/// through the next `>` (kept verbatim if unterminated); an `&` is dropped
/// through the next `;` only if at least one non-`;` char precedes it.
pub fn strip_html_for_indexing(text: &str) -> String {
    // Named whitespace entities → space, matching Dart's pre-pass order so
    // they are not swallowed by the generic `&…;` rule below.
    let spaced = text
        .replace("&nbsp;", " ")
        .replace("&thinsp;", " ")
        .replace("&ensp;", " ")
        .replace("&emsp;", " ");

    let chars: Vec<char> = spaced.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '<' => {
                // `<[^>]*>` — find the closing `>`; drop the whole span.
                if let Some(close) = chars[i + 1..].iter().position(|&c| c == '>') {
                    i += close + 2; // past the '>'
                } else {
                    out.push('<'); // unterminated: regex would not match
                    i += 1;
                }
            }
            '&' => {
                // `&[^;]+;` — needs ≥1 non-`;` char then a `;`.
                let rest = &chars[i + 1..];
                match rest.iter().position(|&c| c == ';') {
                    Some(semi) if semi >= 1 => i += semi + 2, // past the ';'
                    _ => {
                        out.push('&');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Full ingestion normalisation for text-book lines: strip HTML, decompose
/// presentation forms, strip attached nikud/cantillation, collapse whitespace.
/// Punctuation is intentionally preserved — it is shown in search results.
pub fn normalize_text_for_indexing(input: &str) -> String {
    collapse_whitespace(&strip_attached_marks(&fold_presentation_forms(
        &strip_html_for_indexing(input),
    )))
}

const PDF_INVISIBLE: &[char] = &[
    // Dart `_pdfInvisibleChars`: U+200B–U+200F, U+202A–U+202E, U+2066–U+2069, U+FEFF
    '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}',
    '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{FEFF}',
];

/// Ingestion normalisation for PDF text: like [`normalize_text_for_indexing`]
/// but also drops bidi/zero-width invisibles OCR tends to leave behind.
pub fn normalize_pdf_text_for_indexing(input: &str) -> String {
    let stripped = strip_html_for_indexing(input);
    let without_invisible: String = stripped
        .chars()
        .filter(|c| !PDF_INVISIBLE.contains(c))
        .collect();
    collapse_whitespace(&strip_attached_marks(&fold_presentation_forms(
        &without_invisible,
    )))
}

/// Heuristic mirroring the Dart `isProbablyGarbagePdfText`: after removing all
/// whitespace, flags pages whose ratio of Hebrew/Latin letters and digits is
/// too low to be real content (OCR noise, ligature soup, etc.).
///
/// Counts are over Unicode scalar values; realistic Hebrew/Latin PDF text is
/// entirely in the BMP, so this matches the Dart UTF-16 length exactly.
pub fn is_probably_garbage_pdf_text(normalized_text: &str) -> bool {
    // הספים כוילו על טקסט נטול-פיסוק; הנרמול משמר פיסוק לתצוגה מאז,
    // ולכן מסננים אותו כאן כדי לא להטות את היחסים.
    let compact: Vec<char> = sanitize_query(normalized_text)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let total = compact.len();
    if total == 0 {
        return true;
    }
    let is_letter_or_digit =
        |c: char| ('\u{05D0}'..='\u{05EA}').contains(&c) || c.is_ascii_alphanumeric();
    let letters = compact.iter().filter(|&&c| is_letter_or_digit(c)).count();
    if letters == 0 {
        return true;
    }
    let non_letters = total - letters; // compact has no whitespace
    let ratio_letters = letters as f64 / total as f64;

    if total >= 50 && ratio_letters < 0.10 {
        return true;
    }
    if total >= 20 && ratio_letters < 0.20 && non_letters > letters {
        return true;
    }
    false
}

// ── Regex escaping ─────────────────────────────────────────────────────────

/// Escapes tantivy-fst regex metacharacters. Normalised Hebrew/Latin tokens
/// contain none of these, so this is a no-op for realistic query input.
/// The escaped set is also valid for Dart's `RegExp` (ECMAScript), so the
/// display-highlight builder reuses it.
pub(crate) fn escape_regex(s: &str) -> String {
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

// ── Insertion-ordered dedup helper ─────────────────────────────────────────

fn push_unique(out: &mut Vec<String>, seen: &mut HashSet<String>, value: String) {
    if seen.insert(value.clone()) {
        out.push(value);
    }
}

// ── כתיב מלא/חסר (full/partial spelling) ──────────────────────────────────

/// Generates up to `limit` כתיב מלא/חסר variants by toggling each optional
/// `י ו ' "`. Insertion order matches the Dart implementation (bitmask
/// 0..2^n, bit set means "keep the optional character"); generation stops as
/// soon as `limit` unique variants exist, so a word with many optional
/// letters never enumerates the full 2^n space just to be truncated later.
pub(crate) fn generate_spelling_variations(word: &str, limit: usize) -> Vec<String> {
    if word.is_empty() {
        return vec![String::new()];
    }
    if limit == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = word.chars().collect();
    let optional: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter(|(_, &c)| matches!(c, 'י' | 'ו' | '\'' | '"'))
        .map(|(i, _)| i)
        .collect();
    let n = optional.len();
    if n > 20 {
        // Pathological: only-yod/vav word. Return as-is.
        return vec![word.to_string()];
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for mask in 0..(1usize << n) {
        let mut variant = String::new();
        let mut prev = 0usize;
        for (opt_idx, &pos) in optional.iter().enumerate() {
            variant.extend(&chars[prev..pos]);
            if (mask >> opt_idx) & 1 == 1 {
                variant.push(chars[pos]);
            }
            prev = pos + 1;
        }
        variant.extend(&chars[prev..]);
        push_unique(&mut out, &mut seen, variant);
        if out.len() >= limit {
            break;
        }
    }
    out
}

// ── Typo tolerance (edit-distance 1) ──────────────────────────────────────

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
        "ס" => &["ש"],
        "ש" => &["ס"],
        "צ" => &["ז"],
        "ז" => &["צ"],
        _ => &[],
    }
}

/// Common Hebrew letter confusions + adjacent transposition, seeded with the
/// original word. Mirrors the Dart `generateCommonHebrewTypoVariations`.
fn generate_common_typo_variations(word: &str) -> Vec<String> {
    if word.is_empty() {
        return vec![String::new()];
    }
    let graphemes: Vec<&str> = word.graphemes(true).collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    push_unique(&mut out, &mut seen, word.to_string());

    for (i, &g) in graphemes.iter().enumerate() {
        for &sub in typo_substitutions(g) {
            let mut v = graphemes.clone();
            v[i] = sub;
            push_unique(&mut out, &mut seen, v.concat());
        }
    }
    for i in 0..graphemes.len().saturating_sub(1) {
        if graphemes[i] != graphemes[i + 1] {
            let mut v = graphemes.clone();
            v.swap(i, i + 1);
            push_unique(&mut out, &mut seen, v.concat());
        }
    }
    out
}

/// Full edit-distance-1 expansion: common substitutions/transpositions, then
/// single deletions, then single insertions. Capped at `max` variants.
fn generate_typo_variations(word: &str, max: usize) -> Vec<String> {
    if word.is_empty() {
        return vec![String::new()];
    }
    let graphemes: Vec<&str> = word.graphemes(true).collect();
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Helper: add if new and non-empty; return false when cap is reached.
    let mut add = |v: String| -> bool {
        if v.is_empty() || seen.contains(&v) {
            return true; // skip, but keep going
        }
        seen.insert(v.clone());
        out.push(v);
        out.len() < max
    };

    for v in generate_common_typo_variations(word) {
        if !add(v) {
            return out;
        }
    }
    for i in 0..graphemes.len() {
        let mut g = graphemes.clone();
        g.remove(i);
        if !g.is_empty() && !add(g.concat()) {
            return out;
        }
    }
    // Try beginning and end first, then the interior positions.
    let mut positions = vec![0, graphemes.len()];
    positions.extend(1..graphemes.len());
    for pos in positions {
        for &letter in INSERTION_LETTERS {
            let mut g = graphemes.clone();
            g.insert(pos, letter);
            if !add(g.concat()) {
                return out;
            }
        }
    }
    out
}

// ── Single-word regex pattern builders ────────────────────────────────────
//
// All patterns are whole-term (no `^`/`$`) and use bounded wildcards instead
// of `.*` so the tantivy-fst scanner visits a bounded portion of the term
// dictionary.

fn grammatical_prefix_pattern(root: &str) -> String {
    if root.is_empty() {
        return String::new();
    }
    format!("{}{}", GRAM_PREFIX_GROUP, escape_regex(root))
}

fn grammatical_suffix_pattern(root: &str) -> String {
    if root.is_empty() {
        return String::new();
    }
    format!("{}{}", escape_regex(root), SUFFIX_PATTERN)
}

fn full_morphological_pattern(root: &str) -> String {
    if root.is_empty() {
        return String::new();
    }
    format!(
        "{}{}{}",
        PREFIX_GROUP,
        escape_regex(root),
        FULL_SUFFIX_PATTERN
    )
}

/// Bounded prefix-search: `.{0,k}` before the root, where `k` shrinks as the
/// root grows (shorter root → more room for prefix content).
fn user_prefix_pattern(root: &str) -> String {
    let window = match root.chars().count() {
        0 => return String::new(),
        1 => 5,
        2 => 4,
        _ => 3,
    };
    format!(".{{0,{}}}{}", window, escape_regex(root))
}

/// Bounded suffix-search: `.{0,k}` after the root.
fn user_suffix_pattern(root: &str) -> String {
    let window = match root.chars().count() {
        0 => return String::new(),
        1 => 7,
        2 => 6,
        _ => 5,
    };
    format!("{}.{{0,{}}}", escape_regex(root), window)
}

/// Bounded anywhere-in-word: `.{0,k}` on both sides.
fn partial_word_pattern(root: &str) -> String {
    if root.is_empty() {
        return String::new();
    }
    let window = if root.chars().count() <= 3 { 3 } else { 2 };
    format!(".{{0,{w}}}{}.{{0,{w}}}", escape_regex(root), w = window)
}

/// Joins spelling variants each through a builder, wrapped in a non-capturing
/// group. The spelling-only case passes [`escape_regex`] as the builder.
fn join_spelling(word: &str, limit: usize, build: fn(&str) -> String) -> String {
    let branches: Vec<String> = generate_spelling_variations(word, limit)
        .into_iter()
        .map(|v| build(&v))
        .collect();
    format!("(?:{})", branches.join("|"))
}

/// The core decision tree that maps a root word + [`WordFlags`] to a single
/// tantivy-fst regex pattern. Priority order is preserved from the original
/// Dart `createSearchPattern`.
fn word_to_pattern(root: &str, flags: &WordFlags) -> String {
    if root.is_empty() {
        return String::new();
    }
    if flags.spelling {
        // With spelling: fan each variant through the appropriate morphological
        // builder (or just escape if no morphological option is set).
        if flags.prefix && flags.suffix {
            join_spelling(root, MAX_SPELLING_BRANCHES, partial_word_pattern)
        } else if flags.gram_prefix && flags.gram_suffix {
            join_spelling(root, 4, full_morphological_pattern)
        } else if flags.prefix {
            join_spelling(root, MAX_SPELLING_BRANCHES, user_prefix_pattern)
        } else if flags.suffix {
            join_spelling(root, MAX_SPELLING_BRANCHES, user_suffix_pattern)
        } else if flags.gram_prefix {
            join_spelling(root, 10, grammatical_prefix_pattern)
        } else if flags.gram_suffix {
            join_spelling(root, 6, grammatical_suffix_pattern)
        } else if flags.partial {
            join_spelling(root, MAX_SPELLING_BRANCHES, partial_word_pattern)
        } else {
            // Spelling only: alternation of the escaped variants themselves.
            join_spelling(root, MAX_SPELLING_BRANCHES, escape_regex)
        }
    } else if flags.prefix && flags.suffix {
        partial_word_pattern(root)
    } else if flags.gram_prefix && flags.gram_suffix {
        full_morphological_pattern(root)
    } else if flags.prefix {
        user_prefix_pattern(root)
    } else if flags.suffix {
        user_suffix_pattern(root)
    } else if flags.gram_prefix {
        grammatical_prefix_pattern(root)
    } else if flags.gram_suffix {
        grammatical_suffix_pattern(root)
    } else if flags.partial {
        partial_word_pattern(root)
    } else {
        escape_regex(root)
    }
}

// ── Per-word regex assembly ────────────────────────────────────────────────

/// Builds the final tantivy-fst regex term for one query word, taking into
/// account typo tolerance, spelling variants, morphological affixes, and any
/// user-supplied alternative words.
///
/// The result is either a plain escaped word, a single-branch pattern, or a
/// `(?:branch1|branch2|…)` alternation. Length and count budgets ensure the
/// compiled DFA stays within the tantivy-fst state limit.
fn build_word_regex(word: &str, flags: &WordFlags, alternatives: &[String]) -> String {
    // The canonical word plus the normalized alternatives, in one filtered
    // pass (alternatives may normalize to empty; the query word never does, but
    // filtering it too keeps the pass total).
    let candidates: Vec<String> = std::iter::once(word.to_string())
        .chain(alternatives.iter().map(|a| normalize_for_index(a)))
        .filter(|c| !c.trim().is_empty())
        .collect();
    if candidates.is_empty() {
        return escape_regex(word);
    }

    let max = flags.max_variations();
    let mut branches: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for candidate in &candidates {
        // Typo tolerance: expand the candidate to edit-distance-1 variants
        // before feeding each variant into the pattern builder.
        let roots: Vec<String> = if flags.typo {
            generate_typo_variations(candidate, MAX_TYPO_VARIATIONS)
        } else {
            vec![candidate.clone()]
        };

        for root in roots {
            let pattern = word_to_pattern(&root, flags);
            push_unique(&mut branches, &mut seen, pattern);
            if branches.len() >= max {
                break;
            }
        }
        if branches.len() >= max {
            break;
        }
    }

    // Apply length budget: keep branches while the cumulative character count
    // stays under MAX_PATTERN_CHARS (at least one branch is always kept so the
    // term is never empty).
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;
    for b in branches {
        if b.trim().is_empty() {
            continue;
        }
        let len = b.chars().count();
        if !kept.is_empty() && total + len > MAX_PATTERN_CHARS {
            break;
        }
        total += len;
        kept.push(b);
    }

    if kept.is_empty() {
        return escape_regex(word);
    }
    let joined = if kept.len() == 1 {
        kept.into_iter().next().unwrap()
    } else {
        format!("(?:{})", kept.join("|"))
    };
    // Final safety net: if a single oversized branch somehow still exceeded the
    // budget, fall back to a plain literal (always compiles).
    if joined.chars().count() > MAX_PATTERN_CHARS {
        escape_regex(word)
    } else {
        joined
    }
}

// ── Input parsing helpers ──────────────────────────────────────────────────

/// Extracts per-word [`WordFlags`] from the raw Flutter map.
///
/// The outer key uses the format `"{word}_{index}"` (e.g. `"ספר_0"`). We
/// look up by `"{word}_{index}"` for the word at position `index` in `words`.
pub(crate) fn word_flags_at(
    words: &[String],
    index: usize,
    search_options: &HashMap<String, HashMap<String, bool>>,
) -> WordFlags {
    let key = format!("{}_{}", words[index], index);
    search_options
        .get(&key)
        .map(WordFlags::from_map)
        .unwrap_or_default()
}

/// Returns the per-pair custom spacing as a `Vec<u32>` indexed by the
/// left-word position (position `i` → spacing between words `i` and `i+1`).
/// Missing, unparseable, or non-positive entries are treated as zero; a
/// positive value above `u32::MAX` saturates rather than wrapping or
/// collapsing to zero (a huge slop is harmless; a silent zero would change
/// the phrase semantics).
fn parse_custom_spacing(raw: &HashMap<String, String>, word_count: usize) -> Vec<u32> {
    (0..word_count.saturating_sub(1))
        .map(|i| {
            let key = format!("{}-{}", i, i + 1);
            raw.get(&key)
                .and_then(|v| v.trim().parse::<i64>().ok())
                .filter(|&n| n > 0)
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
                .unwrap_or(0)
        })
        .collect()
}

/// The maximum value in a spacing slice (used as phrase slop).
fn max_spacing(spacing: &[u32]) -> u32 {
    spacing.iter().copied().max().unwrap_or(0)
}

// ── Public entry point ─────────────────────────────────────────────────────

/// Builds the regex terms, phrase slop, and max-expansions for an advanced
/// search query. This is the main entry point called by the Tantivy engine
/// layer.
///
/// # Parameters
///
/// - `query` — raw user query string (may contain nikud, mixed case, etc.)
/// - `distance` — default phrase slop when `custom_spacing` is empty
/// - `custom_spacing` — per-pair overrides keyed `"i-(i+1)"` → spacing value
/// - `alternative_words` — extra synonyms per word position (0-indexed)
/// - `search_options` — per-word option checkboxes from the UI, keyed
///   `"{word}_{index}"`
pub fn prepare_advanced_query(
    query: &str,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
) -> AdvancedQuery {
    let normalized = normalize_for_index(query);
    let words = split_query_words(&normalized);

    let has_options =
        !search_options.is_empty() && search_options.values().any(|m| m.values().any(|&v| v));
    let has_alternatives = !alternative_words.is_empty();

    // ── Plain path (no per-word options or alternatives) ──────────────────
    if !has_options && !has_alternatives {
        let terms: Vec<String> = words.iter().map(|w| escape_regex(w)).collect();
        let slop = if words.len() <= 1 {
            0
        } else if !custom_spacing.is_empty() {
            let spacing = parse_custom_spacing(custom_spacing, words.len());
            max_spacing(&spacing)
        } else {
            distance
        };
        let max_expansions = if words.len() > 1 { 100 } else { 10 };
        return AdvancedQuery {
            regex_terms: terms,
            slop,
            max_expansions,
        };
    }

    // ── Advanced path ─────────────────────────────────────────────────────
    let regex_terms: Vec<String> = words
        .iter()
        .enumerate()
        .map(|(i, word)| {
            let flags = word_flags_at(&words, i, search_options);
            let alts = alternative_words
                .get(&(i as u32))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            build_word_regex(word, &flags, alts)
        })
        .collect();

    let slop = if words.len() <= 1 {
        0
    } else if !custom_spacing.is_empty() {
        let spacing = parse_custom_spacing(custom_spacing, words.len());
        max_spacing(&spacing)
    } else {
        distance
    };

    let max_expansions = compute_max_expansions(&words, search_options);

    AdvancedQuery {
        regex_terms,
        slop,
        max_expansions,
    }
}

// ── max_expansions heuristic ───────────────────────────────────────────────

/// Computes the `max_expansions` limit for `RegexPhraseQuery` based on the
/// active search options. Mirrors the Dart `calculateMaxExpansions` rules.
fn compute_max_expansions(
    words: &[String],
    search_options: &HashMap<String, HashMap<String, bool>>,
) -> u32 {
    let has_typo = search_options
        .values()
        .any(|m| m.get(OPT_TYPO).copied().unwrap_or(false));

    if has_typo {
        return if words.len() > 1 { 100 } else { 50 };
    }

    // Check whether any word uses a morphological or partial option, and find
    // the shortest such word (wider expansion for shorter roots).
    let morph_keys = [
        OPT_PREFIX,
        OPT_SUFFIX,
        OPT_GRAM_PREFIX,
        OPT_GRAM_SUFFIX,
        OPT_PARTIAL,
    ];
    let mut shortest_morph: Option<usize> = None;
    for (i, word) in words.iter().enumerate() {
        let key = format!("{}_{}", word, i);
        if let Some(opts) = search_options.get(&key) {
            if morph_keys
                .iter()
                .any(|k| opts.get(*k).copied().unwrap_or(false))
            {
                let len = word.chars().count();
                shortest_morph = Some(match shortest_morph {
                    None => len,
                    Some(prev) => prev.min(len),
                });
            }
        }
    }

    if let Some(shortest) = shortest_morph {
        return match shortest {
            0 | 1 => 2_000,
            2 => 3_000,
            3 => 4_000,
            _ => 5_000,
        };
    }

    if words.len() > 1 {
        100
    } else {
        10
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_options(pairs: &[(&str, &[(&str, bool)])]) -> HashMap<String, HashMap<String, bool>> {
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

    // ── sanitize_query ───────────────────────────────────────────────────

    #[test]
    fn sanitize_normalises_punctuation() {
        assert_eq!(sanitize_query("שלום, עולם!"), "שלום עולם");
        assert_eq!(sanitize_query("א־ב"), "א ב");
        assert_eq!(sanitize_query("רמב״ם"), "רמב\"ם");
        assert_eq!(sanitize_query("תוס׳"), "תוס'");
    }

    // ── ingestion normalisation (parity with the old Dart IndexingDocumentBuilder) ──

    #[test]
    fn strip_html_matches_dart() {
        assert_eq!(strip_html_for_indexing("<b>שלום</b>"), "שלום");
        // whitespace entities become spaces so adjacent words don't merge
        assert_eq!(
            strip_html_for_indexing("לאמר&nbsp;&nbsp;שירה"),
            "לאמר  שירה"
        );
        // generic entities are removed entirely
        assert_eq!(strip_html_for_indexing("a&amp;b"), "ab");
        // unterminated '<' (no closing '>') is kept, like the Dart regex
        assert_eq!(strip_html_for_indexing("a < b"), "a < b");
        assert_eq!(
            strip_html_for_indexing("<span dir=\"rtl\">טקסט</span>"),
            "טקסט"
        );
    }

    #[test]
    fn strip_attached_marks_keeps_separators() {
        assert_eq!(strip_attached_marks("שָׁלוֹם"), "שלום");
        assert_eq!(strip_attached_marks("בְּרֵאשִׁית"), "בראשית");
        // מקף, פסק וסוף-פסוק הם פיסוק גלוי — נשמרים.
        assert_eq!(strip_attached_marks("א־ב"), "א־ב");
        assert_eq!(strip_attached_marks("א׀ב"), "א׀ב");
        assert_eq!(strip_attached_marks("ברא\u{05C3}"), "ברא\u{05C3}");
        assert_eq!(strip_attached_marks("שלום"), "שלום");
    }

    #[test]
    fn normalize_text_for_indexing_keeps_punctuation_strips_marks() {
        // הטקסט השמור מוצג בתוצאות: פיסוק נשמר, HTML וניקוד מוסרים.
        assert_eq!(
            normalize_text_for_indexing("<b>שָׁלוֹם, עולם!</b>"),
            "שלום, עולם!"
        );
        assert_eq!(normalize_text_for_indexing("רמב״ם"), "רמב״ם");
        assert_eq!(normalize_text_for_indexing("אל־משה"), "אל־משה");
        assert_eq!(normalize_text_for_indexing("(עא:) וכו'"), "(עא:) וכו'");
        // מפרידי הפסוק שבטווח הניקוד הם פיסוק — נשמרים.
        assert_eq!(
            normalize_text_for_indexing("בָּרָא\u{05C3} וְהָאָרֶץ"),
            "ברא\u{05C3} והארץ"
        );
    }

    #[test]
    fn normalize_text_for_indexing_folds_presentation_forms() {
        // issue #500: יִ מורכבת (U+FB1D) שרדה את הסרת הניקוד והוצגה כ"?".
        assert_eq!(normalize_text_for_indexing("מ\u{FB1D}ם"), "מים");
        assert_eq!(normalize_text_for_indexing("\u{FB2A}לום"), "שלום");
        assert_eq!(normalize_text_for_indexing("\u{FB4F}הים"), "אלהים");
    }

    #[test]
    fn normalize_pdf_text_matches_text_chain_plus_invisibles() {
        // zero-width chars are dropped (no space), whitespace collapses
        assert_eq!(
            normalize_pdf_text_for_indexing("שלום\u{200B}עולם"),
            "שלוםעולם"
        );
        assert_eq!(
            normalize_pdf_text_for_indexing("<p>אבג   דהו</p>"),
            "אבג דהו"
        );
        // bidi override + BOM removed
        assert_eq!(
            normalize_pdf_text_for_indexing("\u{FEFF}טקסט\u{202E}רגיל"),
            "טקסטרגיל"
        );
        // punctuation preserved, nikud stripped — like the text-book chain
        assert_eq!(
            normalize_pdf_text_for_indexing("שָׁלוֹם, (עולם)"),
            "שלום, (עולם)"
        );
    }

    #[test]
    fn strip_nikud_folds_presentation_forms() {
        assert_eq!(strip_nikud("מ\u{FB1D}ם"), "מים");
        assert_eq!(strip_nikud("שָׁלוֹם"), "שלום");
    }

    // ── שקילות מילון הטרמים: הצנרת הישנה מול המשמרת-פיסוק ──────────────

    /// `removeVolwels` של הצנרת הישנה: מקף/פסק/`|` → רווח, ואז מחיקת כל
    /// טווח הניקוד. משמש רק כייחוס בטסט השקילות.
    fn old_remove_vowels(text: &str) -> String {
        text.chars()
            .filter_map(|c| match c {
                '\u{05BE}' | '\u{05C0}' | '|' => Some(' '),
                c if ('\u{0591}'..='\u{05C7}').contains(&c) => None,
                c => Some(c),
            })
            .collect()
    }

    /// הצנרת שהייתה בשימוש עד שהטקסט השמור החל לשמר פיסוק.
    fn old_normalize(input: &str) -> String {
        sanitize_query(&old_remove_vowels(&strip_html_for_indexing(input)))
    }

    fn analyzer_terms(text: &str) -> Vec<String> {
        use tantivy::tokenizer::{LowerCaser, TextAnalyzer};
        let mut analyzer = TextAnalyzer::builder(crate::hebrew_tokenizer::HebrewTokenizer)
            .filter(LowerCaser)
            .build();
        let mut stream = analyzer.token_stream(text);
        let mut terms = Vec::new();
        while stream.advance() {
            terms.push(stream.token().text.clone());
        }
        terms
    }

    #[test]
    fn new_ingestion_produces_identical_terms_to_old_pipeline() {
        // כל סטייה כאן = רגרסיית recall שקטה: שאילתה שעברה sanitize_query
        // תחפש טרם שאינו קיים באינדקס המשמר-פיסוק.
        let corpus = [
            "וּבָזֶה יוּבַן שַׁ\"ס דִּזְבָחִים הַמִּזְבֵּחַ מְקַדֵּשׁ (עא:)",
            "אמרו חז\"ל יומא עא. הממלא גרונם של תלמידי חכמים",
            "כאילו מנסך יין על גבי מזבח וכו' ואמר המזבח",
            "רמב״ם הל' תשובה פ\"ג ה\"ד; ועי' תוס׳ ד\"ה אמר",
            "אֲשֶׁר־שָׁמַע אל־משה בית-דין א|ב א׀ב",
            "בְּרֵאשִׁית בָּרָא אֱלֹהִים אֵת הַשָּׁמַיִם וְאֵת הָאָרֶץ",
            "שאלה: מה הדין? תשובה — [עיין] {שם} וצ\"ע!",
            "3.14 ד'אש תוס'. סי' קכ\"ה ס\"ק ז'",
            "hello, world! (test) A.B.C 1+2",
        ];
        for text in corpus {
            assert_eq!(
                analyzer_terms(&normalize_text_for_indexing(text)),
                analyzer_terms(&old_normalize(text)),
                "term drift for: {text}"
            );
        }
    }

    #[test]
    fn garbage_pdf_detection_matches_dart() {
        assert!(is_probably_garbage_pdf_text(""));
        assert!(is_probably_garbage_pdf_text("   \n  "));
        // no letters at all
        assert!(is_probably_garbage_pdf_text("!@#$%^&*()"));
        // real content is not garbage
        assert!(!is_probably_garbage_pdf_text(
            "שלום עולם זה טקסט תקין לגמרי"
        ));
        assert!(!is_probably_garbage_pdf_text("hello world normal text"));
        // total>=50, ratio<0.10 → garbage (5 letters, 50 symbols = 55 chars)
        let low_ratio = format!("{}{}", "אבגדה", "#".repeat(50));
        assert!(is_probably_garbage_pdf_text(&low_ratio));
        // total>=20, ratio<0.20 and non_letters>letters → garbage
        let mid_ratio = format!("{}{}", "אבג", "#".repeat(20));
        assert!(is_probably_garbage_pdf_text(&mid_ratio));
        // just under the length thresholds → not garbage
        assert!(!is_probably_garbage_pdf_text("אב#######"));
    }

    // ── split_query_words ────────────────────────────────────────────────

    #[test]
    fn split_handles_geresh_and_gershayim() {
        // תואם את HebrewTokenizer: גרשיים וגרש פנימי מפרידים; רק גרש בסוף
        // מילה נשמר; ״/׳ עבריים מנורמלים ללועזיים לפני הפיצול.
        assert_eq!(split_query_words("שלום עולם"), vec!["שלום", "עולם"]);
        assert_eq!(split_query_words("תוס'"), vec!["תוס'"]);
        assert_eq!(split_query_words("ז\"ל"), vec!["ז", "ל"]);
        assert_eq!(split_query_words("ד'אש"), vec!["ד", "אש"]);
        assert_eq!(split_query_words("ג'ורג'"), vec!["ג", "ורג'"]);
        assert_eq!(split_query_words("רמב״ם"), vec!["רמב", "ם"]);
        assert_eq!(
            split_query_words("הרב פלוני ז\"ל"),
            vec!["הרב", "פלוני", "ז", "ל"]
        );
        // גרשיים בסוף מילה או בתחילתה אינם חלק מהטוקן.
        assert_eq!(split_query_words("רמב\""), vec!["רמב"]);
        assert_eq!(split_query_words("\"רמב"), vec!["רמב"]);
        // גרש כפול: הראשון נבלע כסופי, השני מפריד (כמו בטוקנייזר).
        assert_eq!(split_query_words("רמב''ם"), vec!["רמב'", "ם"]);
    }

    // ── generate_spelling_variations ────────────────────────────────────

    #[test]
    fn spelling_variations_toggle_optional_letters() {
        assert_eq!(generate_spelling_variations("בוא", 100), vec!["בא", "בוא"]);
        assert_eq!(generate_spelling_variations("גמל", 100), vec!["גמל"]);
        // ה-limit עוצר את המנייה מוקדם — לא רק חותך את התוצאה.
        assert_eq!(generate_spelling_variations("בוא", 1), vec!["בא"]);
    }

    // ── typo variations ──────────────────────────────────────────────────

    #[test]
    fn common_typo_covers_substitution_and_transposition() {
        let v = generate_common_typo_variations("בא");
        assert_eq!(v[0], "בא");
        assert!(v.contains(&"וא".to_string()));
        assert!(v.contains(&"בע".to_string()));
        assert!(v.contains(&"בה".to_string()));
        assert!(v.contains(&"אב".to_string()));
    }

    #[test]
    fn typo_variations_respect_cap() {
        let v = generate_typo_variations("שלום", 48);
        assert!(v.len() <= 48);
        assert!(v.contains(&"שלם".to_string()));
    }

    #[test]
    fn typo_substitutions_are_nikud_free() {
        // ס/ש confuse with each other but never with the dotted (vocalized)
        // shin/sin forms, which can't exist in the nikud-free index. This also
        // pins the substitution map output and seed-first ordering for a
        // single-grapheme word.
        assert_eq!(generate_common_typo_variations("ס"), vec!["ס", "ש"]);
        // No produced variation may carry a nikud-range mark (U+0591–U+05C7).
        let v = generate_common_typo_variations("שבת");
        for variant in v {
            assert!(
                !variant
                    .chars()
                    .any(|c| ('\u{0591}'..='\u{05C7}').contains(&c)),
                "variant {variant:?} contains nikud"
            );
        }
    }

    // ── word_to_pattern ──────────────────────────────────────────────────

    #[test]
    fn plain_word_escapes_only() {
        let flags = WordFlags::default();
        assert_eq!(word_to_pattern("שלום", &flags), "שלום");
    }

    #[test]
    fn grammatical_prefix_uses_full_rich_group() {
        // The standalone grammatical-prefix option uses the RICHER group that
        // includes דא, א, כש — distinct from the leaner PREFIX_GROUP used by
        // full morphology. Pin the exact string so the member set cannot drift.
        let flags = WordFlags {
            gram_prefix: true,
            ..Default::default()
        };
        let p = word_to_pattern("ספר", &flags);
        assert_eq!(p, "(?:ו|מ|דא|א|כש|כ|ב|ש|ל|ה|ד)?(?:כ|ב|ש|ל|ה|ד)?(?:ה)?ספר");
    }

    #[test]
    fn full_morphology_uses_lean_prefix_group() {
        // Cross-check: the full prefix+suffix morphology path deliberately uses
        // the leaner PREFIX_GROUP (no דא/א/כש), as it always has.
        let flags = WordFlags {
            gram_prefix: true,
            gram_suffix: true,
            ..Default::default()
        };
        let p = word_to_pattern("ספר", &flags);
        assert!(
            p.starts_with("(?:ו|מ|כ|ב|ש|ל|ה|ד)?(?:כ|ב|ש|ל|ה|ד)?(?:ה)?ספר"),
            "{p}"
        );
        assert!(
            !p.contains("דא"),
            "full morphology must use the lean group: {p}"
        );
    }

    #[test]
    fn patterns_never_use_unbounded_star() {
        let prefix_flags = WordFlags {
            prefix: true,
            ..Default::default()
        };
        let suffix_flags = WordFlags {
            suffix: true,
            ..Default::default()
        };
        let p = word_to_pattern("ארוכה", &prefix_flags);
        assert!(!p.contains(".*"), "prefix pattern uses .*: {p}");
        let s = word_to_pattern("ארוכה", &suffix_flags);
        assert!(!s.contains(".*"), "suffix pattern uses .*: {s}");
    }

    #[test]
    fn all_optional_letter_word_does_not_widen_to_match_anything() {
        // "וו" is entirely optional letters, so its spelling set includes the
        // empty variant. Under spelling+partial the empty variant must collapse
        // to an empty branch — NOT a `.{0,3}.{0,3}` wildcard that matches any
        // short term. (Regression guard for the dropped empty-root guards.)
        let flags = WordFlags {
            spelling: true,
            partial: true,
            ..Default::default()
        };
        let p = word_to_pattern("וו", &flags);
        assert_eq!(p, "(?:|.{0,3}ו.{0,3}|.{0,3}וו.{0,3})");
        assert!(
            !p.contains(".{0,3}.{0,3}"),
            "empty variant widened to wildcard: {p}"
        );
    }

    #[test]
    fn patterns_have_no_anchors() {
        for flags in [
            WordFlags {
                spelling: true,
                ..Default::default()
            },
            WordFlags {
                gram_prefix: true,
                gram_suffix: true,
                ..Default::default()
            },
            WordFlags {
                partial: true,
                ..Default::default()
            },
        ] {
            let p = word_to_pattern("בוא", &flags);
            assert!(!p.contains('^'), "anchor in: {p}");
            assert!(!p.contains('$'), "anchor in: {p}");
        }
    }

    // ── build_word_regex ─────────────────────────────────────────────────

    #[test]
    fn word_regex_stays_within_length_budget() {
        let flags = WordFlags {
            typo: true,
            gram_prefix: true,
            gram_suffix: true,
            ..Default::default()
        };
        let result = build_word_regex("ספר", &flags, &[]);
        assert!(!result.is_empty());
        assert!(
            result.chars().count() <= MAX_PATTERN_CHARS,
            "pattern length {} exceeds budget",
            result.chars().count()
        );
    }

    #[test]
    fn word_regex_includes_alternatives() {
        let result = build_word_regex("שר", &WordFlags::default(), &["מלך".to_string()]);
        assert_eq!(result, "(?:שר|מלך)");
    }

    // ── parse_custom_spacing ─────────────────────────────────────────────

    #[test]
    fn custom_spacing_parsed_correctly() {
        let mut raw = HashMap::new();
        raw.insert("0-1".to_string(), "3".to_string());
        raw.insert("1-2".to_string(), "7".to_string());
        let spacing = parse_custom_spacing(&raw, 3);
        assert_eq!(spacing, vec![3, 7]);
    }

    #[test]
    fn custom_spacing_missing_entries_are_zero() {
        let raw = HashMap::new();
        let spacing = parse_custom_spacing(&raw, 3);
        assert_eq!(spacing, vec![0, 0]);
    }

    #[test]
    fn custom_spacing_handles_overflow_and_negatives() {
        let mut raw = HashMap::new();
        raw.insert("0-1".to_string(), "5000000000".to_string()); // > u32::MAX
        raw.insert("1-2".to_string(), "-7".to_string()); // negative
        let spacing = parse_custom_spacing(&raw, 3);
        // Overflow saturates to u32::MAX rather than collapsing to 0; a
        // negative value is treated as no spacing.
        assert_eq!(spacing, vec![u32::MAX, 0]);
    }

    // ── prepare_advanced_query ───────────────────────────────────────────

    #[test]
    fn plain_two_word_query_uses_distance_as_slop() {
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
    }

    #[test]
    fn custom_spacing_overrides_distance() {
        let mut spacing = HashMap::new();
        spacing.insert("0-1".to_string(), "5".to_string());
        let q = prepare_advanced_query("שלום עולם", 3, &spacing, &HashMap::new(), &HashMap::new());
        assert_eq!(q.slop, 5);
    }

    #[test]
    fn nikud_is_stripped_from_query() {
        let q = prepare_advanced_query("סֵפֶר", 0, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(q.regex_terms, vec!["ספר"]);
    }

    #[test]
    fn latin_query_is_lowercased() {
        let q = prepare_advanced_query(
            "Torah",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(q.regex_terms, vec!["torah"]);
    }

    #[test]
    fn alternative_words_are_normalized() {
        let mut alts = HashMap::new();
        alts.insert(0u32, vec!["תּוֹרָה".to_string()]);
        let q = prepare_advanced_query("ספר", 0, &HashMap::new(), &alts, &HashMap::new());
        assert_eq!(q.regex_terms, vec!["(?:ספר|תורה)"]);
    }

    #[test]
    fn grammatical_prefix_option_applied() {
        let so = make_options(&[("ספר_0", &[(OPT_GRAM_PREFIX, true)])]);
        let q = prepare_advanced_query("ספר", 0, &HashMap::new(), &HashMap::new(), &so);
        assert_eq!(
            q.regex_terms,
            vec!["(?:ו|מ|דא|א|כש|כ|ב|ש|ל|ה|ד)?(?:כ|ב|ש|ל|ה|ד)?(?:ה)?ספר"]
        );
    }

    #[test]
    fn spelling_with_grammatical_prefix_keeps_rich_group() {
        // The spelling + grammatical-prefix combination must fan each spelling
        // variant through the RICH prefix group too (a second code path that
        // previously dropped דא/א/כש).
        let flags = WordFlags {
            spelling: true,
            gram_prefix: true,
            ..Default::default()
        };
        let p = word_to_pattern("בוא", &flags);
        // Every branch carries the rich group; דא must be present.
        assert!(p.contains("(?:ו|מ|דא|א|כש|כ|ב|ש|ל|ה|ד)?"), "{p}");
    }

    // ── compute_max_expansions ───────────────────────────────────────────

    #[test]
    fn max_expansions_defaults() {
        assert_eq!(
            compute_max_expansions(&["שלום".to_string()], &HashMap::new()),
            10
        );
        assert_eq!(
            compute_max_expansions(&["שלום".to_string(), "עולם".to_string()], &HashMap::new()),
            100
        );
    }

    #[test]
    fn max_expansions_grammatical_prefix_by_word_length() {
        let so = make_options(&[("ספר_0", &[(OPT_GRAM_PREFIX, true)])]);
        assert_eq!(compute_max_expansions(&["ספר".to_string()], &so), 4_000);
    }

    #[test]
    fn max_expansions_typo_tolerance() {
        let so = make_options(&[("ספר_0", &[(OPT_TYPO, true)])]);
        assert_eq!(compute_max_expansions(&["ספר".to_string()], &so), 50);
        assert_eq!(
            compute_max_expansions(&["ספר".to_string(), "תורה".to_string()], &so),
            100
        );
    }
}
