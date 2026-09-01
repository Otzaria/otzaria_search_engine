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
//! * Words are joined by [`WORD_SEPARATOR`], optionally allowing up to the
//!   configured spacing count of intermediate words ([`INTERMEDIATE_WORD`])
//!   between adjacent query words. Both are built from the three disjoint
//!   classes below, which are derived from the tokeniser's own predicates —
//!   so gap counting follows the index token-for-token in every case those
//!   predicates decide. Two known gaps remain, because the tokeniser looks at
//!   *sequences* while a character class cannot: a run of two or more
//!   gershayim breaks a token in the index (`רמב""ם` is two tokens) but counts
//!   as one word here, and characters outside [`SEPARATOR_DOMAIN`] default to
//!   word content.
//! * A word with a morphological option (prefixes/suffixes/partial) keeps the
//!   plain root pattern but is flagged as not eligible for the word-boundary
//!   check, so the root may highlight inside a longer inflected word.
//! * כתיב מלא/חסר fans each word (and its alternatives) into spelling
//!   variants, capped at [`hebrew_query`]'s spelling budget — unlike the old
//!   Dart code, the 2^n fan-out is bounded.

use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;

use crate::hebrew_query::{
    aramaic_root_variants, generate_spelling_variations, is_word_mark, normalize_for_index,
    split_query_words, word_flags_at, WordFlags, BREAKING_TAG_NAMES, MAX_SPELLING_BRANCHES,
};
use crate::hebrew_tokenizer::continues_token;

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
/// U+0591–U+05C7 minus the separators in that range: maqaf (U+05BE), paseq
/// (U+05C0), sof pasuq (U+05C3) and nun hafukha (U+05C6) — the same set
/// `hebrew_query::is_attached_mark` excludes. Were they included, the `*`
/// after a letter would swallow a separator between words and break
/// word-boundary detection (e.g. "אשר־שמע") or let a one-word pattern
/// highlight across a verse boundary (e.g. "אב" matching "א׃ב").
/// Also covers the general combining range U+0300–U+036F (Judeo-Arabic
/// transliteration dots: `כלת̇ום`), which the tokenizer treats like attached
/// marks — so a mark-free query term highlights the marked display form.
pub(crate) const ATTACHED_MARKS_CLASS: &str =
    "[\u{0300}-\u{036F}\u{0591}-\u{05BD}\u{05BF}\u{05C1}\u{05C2}\u{05C4}\u{05C5}\u{05C7}]*";

/// אותה מחלקה ללא הכמת — לבניית `+`/`*` מפורשים ב-fragment הגבול.
const ATTACHED_MARKS_SET: &str =
    "[\u{0300}-\u{036F}\u{0591}-\u{05BD}\u{05BF}\u{05C1}\u{05C2}\u{05C4}\u{05C5}\u{05C7}]";

/// טווחי התווים שמהם נגזרות מחלקות המפרידים. מחוץ להם ברירת המחדל היא
/// "תו מילה" — נכון לאותיות של כתבים אחרים (`is_alphanumeric`), ומפספס רק
/// סימני פיסוק אקזוטיים. הטווחים: ASCII + Latin-1, סימנים משולבים והבלוק
/// העברי, ערבית (תעתיק ערבית-יהודית), פיסוק כללי עד Supplemental Punctuation
/// (מקפים טיפוגרפיים כמו `⸗` U+2E17 ו-`−` U+2212), צורות תצוגה עבריות,
/// ופיסוק fullwidth.
const SEPARATOR_DOMAIN: &[(u32, u32)] = &[
    (0x0009, 0x00FF),
    (0x0300, 0x06FF),
    (0x2000, 0x2E7F),
    (0xFB00, 0xFB4F),
    (0xFF00, 0xFFEF),
];

/// גוף מחלקת תווים (בלי הסוגריים) של כל תו בטווחי [`SEPARATOR_DOMAIN`]
/// שעבורו `keep` מחזיר `false`. הטווחים מכווצים ל-`a-b` והתווים
/// נמלטים כ-`\uXXXX`, כדי שהתבנית תישאר קצרה וחוקית ב-`RegExp` של Dart.
fn class_body_excluding(keep: impl Fn(char) -> bool) -> String {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &(start, end) in SEPARATOR_DOMAIN {
        for code in start..=end {
            let Some(c) = char::from_u32(code) else {
                continue;
            };
            if keep(c) {
                continue;
            }
            match runs.last_mut() {
                Some(run) if run.1 + 1 == code => run.1 = code,
                _ => runs.push((code, code)),
            }
        }
    }

    let mut body = String::new();
    for (start, end) in runs {
        body.push_str(&escape_in_class(start));
        // טווח בן שני תווים אינו חוסך דבר על פני שניהם בנפרד.
        if end > start + 1 {
            body.push('-');
        } else if end == start + 1 {
            body.push_str(&escape_in_class(end));
            continue;
        } else {
            continue;
        }
        body.push_str(&escape_in_class(end));
    }
    body
}

fn escape_in_class(code: u32) -> String {
    format!("\\u{code:04X}")
}

/// שלוש מחלקות תווים **זרות זו לזו** שמכסות יחד כל תו, ונגזרות מהפרדיקטים
/// של הטוקנייזר. הזרות היא תנאי הכרחי: כשמחלקת המילה ומחלקת המפריד חופפות,
/// `SEP(?:WORD SEP){0,n}` הופך דו-משמעי, ו-`RegExp` של Dart (מנוע עם
/// backtracking) נתקע על שורה שאינה מתאימה — עשרות שניות ב-thread הראשי.
///
/// * [ALNUM_CLASS] — אות או ספרה; רק כאן טוקן יכול להתחיל ולהסתיים.
/// * [SOFT_CHAR_CLASS] — סימן צמוד, פיסוק שקוף או גרש/גרשיים: ממשיך טוקן
///   אך לעולם אינו פותח אותו.
/// * [BREAK_CHAR_CLASS] — כל השאר: שובר טוקן.
static ALNUM_CLASS: Lazy<String> =
    Lazy::new(|| format!("[^{}]", class_body_excluding(is_alnum_char)));

static SOFT_CHAR_CLASS: Lazy<String> = Lazy::new(|| {
    format!(
        "[{}]",
        class_body_excluding(|c| !(continues_token(c) && !is_alnum_char(c)))
    )
});

static BREAK_CHAR_CLASS: Lazy<String> = Lazy::new(|| {
    format!(
        "[{}]",
        class_body_excluding(|c| continues_token(c) || is_markup_char(c))
    )
});

/// אות או ספרה — לא סימן צמוד. סימני ניקוד מסווגים ב-Unicode כ-alphabetic,
/// ובלי ההחרגה הם היו נחשבים פותחי-טוקן.
fn is_alnum_char(c: char) -> bool {
    c.is_alphanumeric() && !is_word_mark(c)
}

/// `<` ו-`>` מטופלים רק כתג שלם ([`INLINE_TAG`]/[`BREAKING_TAG`]) ולא כתווים
/// בודדים בשום מחלקה, כדי שהחלופות יישארו זרות וכדי שאותיות שבתוך תג לא
/// ייחשבו למילה.
fn is_markup_char(c: char) -> bool {
    c == '<' || c == '>'
}

/// גוף תג שבירה, בלי `<`: `/` פותח אופציונלי, שם מהרשימה המשותפת עם
/// האינדוקס, ואחרי השם `>` מיד או תו לא-אלפאנומרי ואז שאר התג. הארוכים
/// קודם, שגם `pre` וגם `p` ייבחנו בסדר שממצה את החלופות.
static BREAKING_TAG_BODY: Lazy<String> = Lazy::new(|| {
    let mut names: Vec<&str> = BREAKING_TAG_NAMES.to_vec();
    names.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    format!(
        "/?(?:{names})(?:[^>a-zA-Z0-9][^>]*)?>",
        names = names.join("|")
    )
});

/// תג inline (`<b>`, `<span>`…) — אינו שובר טוקן: האינדוקס מוחק אותו בלי
/// רווח, ולכן `מי<b>לה` הוא טוקן אחד. lookahead שלילי (נתמך ב-RegExp של
/// Dart) מוציא ממנו את תגי השבירה, כך ששתי חלופות התג נשארות זרות —
/// הזרות היא מה שמונע התפוצצות backtracking.
static INLINE_TAG: Lazy<String> =
    Lazy::new(|| format!("<(?!{body})[^>]*>", body = &*BREAKING_TAG_BODY));

/// תג שבירה (`<br>`, `</p>`…) — האינדוקס ממיר אותו לרווח
/// ([`BREAKING_TAG_NAMES`]), ולכן בהדגשה הוא שובר טוקן.
static BREAKING_TAG: Lazy<String> = Lazy::new(|| format!("<{body}", body = &*BREAKING_TAG_BODY));

/// מפריד בין שתי מילות שאילתה סמוכות: חייב לכלול לפחות שובר-טוקן אחד
/// (תו שובר או תג שבירה), ולפניו ואחריו מותרים סימנים רכים ותגי inline.
/// `תדע. זרעך` ו-`תדע<br>זרעך` מתאימים, ואילו `תדע<b>זרעך` אינו מתאים —
/// האינדקס רואה שם טוקן אחד ולכן גם החיפוש אינו מוצא אותו.
static WORD_SEPARATOR: Lazy<String> = Lazy::new(|| {
    format!(
        "(?:{soft}|{tag})*(?:{brk}|{btag})(?:{soft}|{brk}|{tag}|{btag})*",
        soft = &*SOFT_CHAR_CLASS,
        brk = &*BREAK_CHAR_CLASS,
        tag = &*INLINE_TAG,
        btag = &*BREAKING_TAG,
    )
});

/// Cumulative per-word pattern length budget. Display patterns are ~3× longer
/// than index-term patterns (each letter carries a marks class), so this is
/// looser than `hebrew_query::MAX_PATTERN_CHARS`; at least one branch is
/// always kept. Raised with the search-side budgets (parity), but capped
/// short of a strict 3× of the index budget — past ~12k chars the app-side
/// `RegExp` gets slow while extra branches stop adding visible highlights.
const MAX_DISPLAY_PATTERN_CHARS: usize = 12_000;

/// מילה שלמה אחת בין שתי מילות השאילתה, כפי שהטוקנייזר של האינדקס מודד
/// אותה: פותחת ומסתיימת באות/ספרה, ובתוכה מותרים סימנים רכים ותגי inline
/// (`רמב״ם`, `פ.ב.י`, `מי<b>לה` — טוקן אחד). תג שבירה אינו מותר בתוך
/// מילה — באינדקס הוא רווח.
///
/// ספירה לפי רווחים (`\S+`) אינה שקולה: `כי־גר` הוא שתי מילים באינדקס, ולכן
/// היא הדגישה `תדע כי־גר יהיה זרעך` במרווח 2 בעוד החיפוש דורש 3 — מילים
/// מודגשות לצד "אין תוצאות".
static INTERMEDIATE_WORD: Lazy<String> = Lazy::new(|| {
    format!(
        "{alnum}(?:(?:{soft}|{tag})*{alnum})*",
        alnum = &*ALNUM_CLASS,
        soft = &*SOFT_CHAR_CLASS,
        tag = &*INLINE_TAG,
    )
});

/// Separator allowing up to `max_intermediate_words` whole words between two
/// adjacent query words (the "מרווח בין מילים" search option).
fn separator_with_spacing(max_intermediate_words: u32) -> String {
    if max_intermediate_words == 0 {
        WORD_SEPARATOR.to_string()
    } else {
        format!(
            "{sep}(?:{word}{sep}){{0,{n}}}",
            sep = &*WORD_SEPARATOR,
            word = &*INTERMEDIATE_WORD,
            n = max_intermediate_words
        )
    }
}

// ── Spacing resolution ─────────────────────────────────────────────────────

/// Resolves the allowed intermediate-word count for the gap after word `i` —
/// the shared [`hebrew_query::resolve_gaps`] (mirroring the historical Dart
/// `_highlightSeparatorForIndex`), so the book highlight and the engine's
/// per-pair phrase verification agree gap-by-gap.
fn spacing_for_gaps(
    custom_spacing: &HashMap<String, String>,
    distance: u32,
    word_count: usize,
) -> Vec<u32> {
    crate::hebrew_query::resolve_gaps(custom_spacing, distance, word_count)
}

// ── Per-word pattern building ──────────────────────────────────────────────

/// גרש/גרשיים אופציונליים בין שתי אותיות עבריות בטקסט התצוגה. האינדקס
/// מטמיע לכל מילה עם גרשיים גם את צורתה הנקייה (הטוקן-התאום), כך שטרם
/// נטול-גרשיים ("רמבם") מתאים בדין לטקסט מודפס `רמב"ם` — וההדגשה חייבת
/// לכסות זאת. עד שני תווים: זוג-גרשים ≡ גרשיים (מוסכמת `רמב''ם`).
/// כולל את הצורות הטיפוגרפיות (U+2018/U+2019/U+201C/U+201D) שהטוקנייזר
/// מקפל — `רמח”ל` בדפוס מודגש ע"י `רמח"ל`/`רמחל`.
pub(crate) const OPTIONAL_QUOTES: &str = "[\"'\\u05F3\\u05F4\\u2018\\u2019\\u201C\\u201D]{0,2}";

/// גרש בטקסט תצוגה: ASCII, עברי, או ציטוט-יחיד טיפוגרפי — הצורות
/// שהטוקנייזר מקפל ל-`'` (ראו `hebrew_tokenizer::is_geresh`).
const GERESH_DISPLAY_CLASS: &str = "['\\u05F3\\u2018\\u2019]";

/// גרשיים בטקסט תצוגה: ", ״, צורה טיפוגרפית, או זוג גרשים מודפס
/// (`רמב''ם`) — הצורות שהטוקנייזר מקפל ל-`"`.
const GERSHAYIM_DISPLAY_CLASS: &str = "(?:[\"\\u05F4\\u201C\\u201D]|['\\u05F3\\u2018\\u2019]{2})";

/// Builds the display pattern for one literal term: each Hebrew base letter
/// may be followed by attached marks, and geresh/gershayim match both the
/// ASCII and Hebrew forms. The term itself is expected to be nikud-free.
fn charwise_display_pattern(term: &str) -> String {
    let mut out = String::with_capacity(term.len() * 4);
    let mut prev_was_letter = false;
    for ch in term.chars() {
        // בין שתי אותיות עבריות רצופות בטרם — גרשיים אופציונליים בתצוגה.
        if prev_was_letter && matches!(ch, 'א'..='ת') {
            out.push_str(OPTIONAL_QUOTES);
        }
        prev_was_letter = matches!(ch, 'א'..='ת');
        match ch {
            'א'..='ת' => {
                out.push(ch);
                out.push_str(ATTACHED_MARKS_CLASS);
            }
            // `"` בטרם ≡ גרשיים בדפוס: ", ״, צורה טיפוגרפית (“ ”), או זוג
            // גרשים (מוסכמת `רמב''ם` בקבצים ישנים — הטוקנייזר מאחד אותו
            // ל-`"` אבל טקסט התצוגה נשמר כפי שנדפס).
            '"' => out.push_str(GERSHAYIM_DISPLAY_CLASS),
            '\'' => out.push_str(GERESH_DISPLAY_CLASS),
            _ => push_escaped_char(&mut out, ch),
        }
    }
    out
}

/// דוחף תו בודד ל-`out` עם escape של מטא-תווי רגקס — בלי הקצאות ביניים
/// (בניגוד ל-`escape_regex` שמקבל מחרוזת). אותה קבוצת תווים כמו
/// `hebrew_query::escape_regex`, תקפה גם ל-RegExp של Dart.
#[inline]
fn push_escaped_char(out: &mut String, ch: char) {
    if matches!(
        ch,
        '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
    ) {
        out.push('\\');
    }
    out.push(ch);
}

/// Expands one query word (plus its alternatives) into display branches:
/// spelling variants when כתיב מלא/חסר is on, then one charwise pattern per
/// term, deduplicated in insertion order and kept within the length budget.
fn build_word_display_pattern(word: &str, flags: &WordFlags, alternatives: &[String]) -> String {
    let mut terms: Vec<String> = Vec::new();
    let mut seen_terms: HashSet<String> = HashSet::new();

    let push_term = |terms: &mut Vec<String>, seen: &mut HashSet<String>, raw: &str| {
        let mut stripped = normalize_for_index(raw);
        // התעלם מגרשיים: התבנית נבנית מהצורה הנקייה — הגרשיים האופציונליים
        // ש-charwise_display_pattern מתיר בין אותיות מכסים את שתי הצורות.
        if flags.ignore_quotes {
            stripped = crate::hebrew_query::strip_quote_chars(&stripped);
        }
        if stripped.trim().is_empty() {
            return;
        }
        // סיומות ארמיות: גם וריאנט השקילות הסופית (מלכה↔מלכא, חכמים↔חכמין)
        // מודגש; קידומות ארמיות מכוסות בכך שהמילה מאבדת את זכאות גבול-המילה
        // (הדפוס מדגיש גם בתוך צורה עם קידומת) — כמו שאר אפשרויות המורפולוגיה.
        let bases = if flags.aramaic_suffix {
            aramaic_root_variants(&stripped)
        } else {
            vec![stripped]
        };
        for base in bases {
            if flags.spelling {
                for variant in generate_spelling_variations(&base, MAX_SPELLING_BRANCHES) {
                    if seen.insert(variant.clone()) {
                        terms.push(variant);
                    }
                }
            } else if seen.insert(base.clone()) {
                terms.push(base);
            }
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
        // קידומות ארמיות שוברות זכאות גבול-מילה (הדגשה גם בתוך צורה עם
        // קידומת); סיומות ארמיות הן וריאנטים שלמים ואינן שוברות.
        let has_expansion = flags.prefix
            || flags.suffix
            || flags.gram_prefix
            || flags.gram_suffix
            || flags.partial
            || flags.aramaic_prefix;
        word_patterns.push(pattern);
        word_boundary_eligible.push(!has_expansion);
    }

    assemble_display_highlight(
        word_patterns,
        word_boundary_eligible,
        distance,
        custom_spacing,
    )
}

/// Joins per-word patterns into the final [`DisplayHighlight`]: the combined
/// pattern chains the words with [`separator_with_spacing`] per gap. Shared by
/// the query-shape and matched-terms entry points.
fn assemble_display_highlight(
    word_patterns: Vec<String>,
    word_boundary_eligible: Vec<bool>,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
) -> Option<DisplayHighlight> {
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

// ── Matched-terms entry point ──────────────────────────────────────────────

/// Longest-first charwise branches for a set of matched index terms.
///
/// Alternation in Dart's `RegExp` is leftmost-first, so a shorter term that
/// prefixes a longer one (`ספר` vs `ספרים`) must come after it or it would
/// truncate the longer word's highlight. Budget-capped like
/// [`build_word_display_pattern`]; the first branch is always kept.
fn build_terms_display_pattern(terms: &[String]) -> String {
    let mut sorted: Vec<&str> = terms.iter().map(String::as_str).collect();
    sorted.sort_by(|a, b| {
        b.chars()
            .count()
            .cmp(&a.chars().count())
            .then_with(|| a.cmp(b))
    });
    sorted.dedup();

    let mut branches: Vec<String> = Vec::new();
    let mut total = 0usize;
    for term in sorted {
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

/// Builds display-highlight patterns from the *index terms the query actually
/// matches* — one `Vec<String>` per query word, aligned with
/// [`split_query_words`] over the engine-normalized query (the same order
/// `hebrew_query::prepare_advanced_query` builds `regex_terms` in).
///
/// Per word, the pattern source depends on the active options:
///
/// - **Typo tolerance** (with or without other options): the matched index
///   terms — Levenshtein variants the query-shape builder cannot reproduce —
///   are painted as whole tokens. Token boundaries are kept unless a
///   boundary-breaking expansion is also active, in which case they are waived
///   so the variant still highlights inside an inflected form.
/// - **Boundary-breaking expansion without typo** (prefix / suffix / partial):
///   the compact query-shape pattern highlights only the typed substring/root
///   with boundaries waived. Unlike the term list it is not budget-truncated,
///   so every occurrence highlights — the term list can drop visible words when
///   the option matches thousands of index tokens.
/// - **No expansion** (plain / spelling): the matched whole tokens, boundaries
///   kept.
///
/// A word with an empty term list (nothing in this index matched, or its
/// automatons failed to compile) falls back to the query-shape
/// [`build_word_display_pattern`] with its original boundary rules — the
/// result is never worse than [`build_display_highlight`].
pub fn build_display_highlight_from_terms(
    query: &str,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
    per_word_terms: &[Vec<String>],
) -> Option<DisplayHighlight> {
    let normalized = normalize_for_index(query);
    let words = split_query_words(&normalized);
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
        let matched = per_word_terms.get(i).map(Vec::as_slice).unwrap_or(&[]);

        // הרחבה ששוברת גבול-מילה (קידומת/סיומת/חלק-ממילה): ה-query-shape
        // קומפקטי, מכסה כל התאמה ומדגיש רק את החלק שהוקלד — בניגוד למונחי-
        // האינדקס שחסומים בתקציב ומדגישים מילים שלמות. מונחי-האינדקס נשמרים
        // לשגיאות-כתיב (וריאנטי Levenshtein שה-query-shape אינו יודע להפיק);
        // כשגם הרחבה פעילה מוותרים על גבול-המילה כדי שהווריאנטים לא ייפסלו.
        let has_expansion = flags.prefix
            || flags.suffix
            || flags.gram_prefix
            || flags.gram_suffix
            || flags.partial
            || flags.aramaic_prefix;
        let (pattern, boundary_eligible) = if !matched.is_empty() && !(has_expansion && !flags.typo)
        {
            (build_terms_display_pattern(matched), !has_expansion)
        } else {
            (
                build_word_display_pattern(word, &flags, alts),
                !has_expansion,
            )
        };
        if pattern.is_empty() {
            continue;
        }
        word_patterns.push(pattern);
        word_boundary_eligible.push(boundary_eligible);
    }

    assemble_display_highlight(
        word_patterns,
        word_boundary_eligible,
        distance,
        custom_spacing,
    )
}

// ── Literal in-book pattern ────────────────────────────────────────────────

/// מחלקת האותיות העבריות לבדיקת גבולות מילה בתבנית הליטרלית: אותיות בסיס
/// (U+05D0–U+05EA), ליגטורות יידיש (U+05F0–U+05F2) וצורות תצוגה
/// (U+FB1D–U+FB4F) — תואם את `_isHebrewLetter` של החיפוש המקומי בספר.
const HEBREW_LETTER_CLASS: &str = r"א-תװ-ײיִ-ﭏ";

/// fragment גרש/גרשיים לבדיקת הגבול, בסמנטיקת הטוקנייזר: רצף-ציטוט חוקי
/// הוא `Q = גרשיים | גרש אחד/שניים`, וכמה רצפים חוקיים המופרדים בסימנים
/// צמודים משתרשרים — `Q (סימן+ Q)*` (רמב''ְ"ם הוא טוקן אחד; ראו
/// test_quote_runs_split_by_marks_dedupe_to_single_gershayim). רצפים
/// צמודים לא-חוקיים ("", ''', '") אינם חיבור — נשארים גבול.
fn quote_boundary_fragment() -> String {
    let q = format!(
        "(?:[{g2}]|[{g1}]{{1,2}})",
        g2 = "\"\u{05F4}\u{201C}\u{201D}",
        g1 = "'\u{05F3}\u{2018}\u{2019}",
    );
    format!("(?:{q}(?:{m}+{q})*)", m = ATTACHED_MARKS_SET)
}

/// Builds the regex for highlighting *literal* in-book search matches (the
/// simple/exact mode that scans the open book locally): the query phrase
/// as-typed, whitespace-joined, nikud-tolerant after every character,
/// geresh/gershayim matching both ASCII and Hebrew forms, and word-boundary
/// lookarounds so `אמר` does not light up inside `ויאמר` — mirroring the
/// local search's `_containsWholeWord` semantics.
///
/// The query is deliberately NOT sanitized or tokenized like engine queries:
/// the local scan matches the text as typed, and the highlight must mirror
/// that scan, not the engine's tokenizer.
///
/// Dart-RegExp dialect; compile with `caseSensitive: false, unicode: true`.
/// Returns `None` for a whitespace-only query.
pub fn build_literal_pattern(query: &str) -> Option<String> {
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let last = words.len() - 1;
    // המפריד בין המילים הוא אותו [`WORD_SEPARATOR`] של החיפוש המתקדם: מה
    // שהאינדקס רואה כשני טוקנים סמוכים חייב להיות מודגש גם במסלול המקומי.
    // רשימה מצומצמת (רווח/מקף/פסק) החטיאה פיסוק דבוק — `תדע, זרעך` נמצא
    // בחיפוש הגלובלי ולא הודגש בספר.
    let phrase = words
        .iter()
        .enumerate()
        .map(|(i, w)| literal_charwise_pattern(w, i == last))
        .collect::<Vec<_>>()
        .join(&WORD_SEPARATOR);
    // גבול תלוי-הקשר: התאמה נפסלת אם לפניה אות (או אות+גרש/גרשיים — המילה
    // ממשיכה מתוך רש״י), או אם אחריה אות (או גרש/גרשיים+אות). גרשיים שאחריו
    // לא-אות (סוגר ציטוט) נשאר גבול — ״הרעתי״ מודגש. ה-lookahead מדלג בעצמו
    // על סימנים צמודים שנותרו: [marks]* שבתבנית נסוג ב-backtracking ומשאיר
    // ניקוד בין ההתאמה לאות הבאה (אמר בתוך אָמַרְתִּי) — הדילוג סוגר את הפרצה.
    let qf = quote_boundary_fragment();
    Some(format!(
        "(?<![{cls}]{marks}{qf}?{marks})(?:{phrase})(?!{marks}{qf}?{marks}[{cls}])",
        cls = HEBREW_LETTER_CLASS,
        marks = ATTACHED_MARKS_CLASS,
    ))
}

/// תבנית תו-אחר-תו לביטוי ליטרלי: אחרי כל תו מותרים סימני ניקוד/טעמים
/// (הטקסט המוצג מנוקד; השאילתה בדרך כלל לא), וגרש/גרשיים תופסים את שתי
/// הצורות — הלועזית והעברית.
fn is_query_quote(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}' | '\'' | '\u{05F3}' | '\u{2018}' | '\u{2019}'
    )
}

/// גרשיים בלבד (לא גרש): גרש סוגר יחיד (תוס׳) הוא חלק מהטוקן לפי
/// הטוקנייזר ולפי generate_highlight_pattern, ולכן נשאר נצרך ומודגש;
/// רק גרשיים סוגר של ציטוט הופך ל-lookahead.
fn is_query_gershayim(ch: char) -> bool {
    matches!(ch, '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}')
}

fn push_quote_class(out: &mut String, ch: char) {
    match ch {
        '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}' => out.push_str(GERSHAYIM_DISPLAY_CLASS),
        _ => out.push_str(GERESH_DISPLAY_CLASS),
    }
}

fn literal_charwise_pattern(word: &str, is_last_word: bool) -> String {
    let total_chars = word.chars().count();
    // רק במילה האחרונה, ורק גרשיים (לא גרש): גרשיים נגרר הופך ל-lookahead
    // (מאומת אך לא נצרך/נצבע) — "הרעתי\u{05F4}" מדגיש "הרעתי" בלבד, כמו
    // generate_highlight_pattern. גרש סוגר (תוס\u{05F3}) הוא חלק מהטוקן —
    // נשאר נצרך. במילים לא-אחרונות הכל נצרך, אחרת המפריד שאחריהן היה פוגש
    // גרש במקום רווח ו"ר\u{05F3} עקיבא" לא היה נמצא.
    let mut trailing_quotes = 0;
    if is_last_word {
        for ch in word.chars().rev() {
            if is_query_gershayim(ch) {
                trailing_quotes += 1;
            } else {
                break;
            }
        }
    }
    // מילה שכולה גרש/גרשיים — אין ליבה; משאירים הכל נצרך (מקרה קצה).
    let core_len = if trailing_quotes == total_chars {
        total_chars
    } else {
        total_chars - trailing_quotes
    };

    // כל תו מוסיף את ATTACHED_MARKS_CLASS (~25 בתים) ואולי מחלקת גרש/גרשיים;
    // הקצאה מראש נדיבה מונעת reallocations בלולאה.
    let mut out = String::with_capacity(word.len() * 30);
    let mut prev_was_letter = false;
    for ch in word.chars().take(core_len) {
        // בין שתי אותיות עבריות רצופות — גרשיים אופציונליים, בדיוק כמו
        // charwise_display_pattern: בלעדיהם "רשי" הדגיש את רש״י בגוף הספר
        // אך החלונית הציגה "אין תוצאות" (Otzaria/otzaria#1054).
        if prev_was_letter && matches!(ch, 'א'..='ת') {
            out.push_str(OPTIONAL_QUOTES);
        }
        prev_was_letter = matches!(ch, 'א'..='ת');
        if is_query_quote(ch) {
            push_quote_class(&mut out, ch);
        } else {
            push_escaped_char(&mut out, ch);
        }
        out.push_str(ATTACHED_MARKS_CLASS);
    }
    if core_len < total_chars {
        out.push_str("(?=");
        for ch in word.chars().skip(core_len) {
            push_quote_class(&mut out, ch);
        }
        out.push(')');
    }
    out
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_marks_set_matches_class() {
        assert_eq!(format!("{ATTACHED_MARKS_SET}*"), ATTACHED_MARKS_CLASS);
    }

    #[test]
    fn attached_marks_class_equals_tokenizer_attached_set() {
        // מפרקים את מחלקת התווים (תווים בודדים וטווחים) ומשווים לסט של
        // is_attached_mark — כך מפרידי הפסוק (מקף, פסק, סוף-פסוק, נו"ן
        // הפוכה) לא יחזרו למחלקה בטעות ויאירו טקסט שחוצה גבול מילה/פסוק.
        let inner: Vec<char> = ATTACHED_MARKS_CLASS
            .trim_end_matches('*')
            .trim_start_matches('[')
            .trim_end_matches(']')
            .chars()
            .collect();
        let mut in_class = HashSet::new();
        let mut i = 0;
        while i < inner.len() {
            if i + 2 < inner.len() && inner[i + 1] == '-' {
                for c in inner[i]..=inner[i + 2] {
                    in_class.insert(c);
                }
                i += 3;
            } else {
                in_class.insert(inner[i]);
                i += 1;
            }
        }
        for cp in 0x0591u32..=0x05C7 {
            let c = char::from_u32(cp).unwrap();
            assert_eq!(
                in_class.contains(&c),
                crate::hebrew_query::is_attached_mark(c),
                "U+{cp:04X} disagrees with is_attached_mark"
            );
        }
    }

    mod literal_pattern {
        use super::super::build_literal_pattern;

        #[test]
        fn whole_word_boundaries() {
            let p = build_literal_pattern("אמר").unwrap();
            assert!(p.starts_with("(?<!["));
            assert!(p.ends_with("])"));
        }

        #[test]
        fn quote_boundary_is_context_dependent() {
            // גרש/גרשיים אינם אות: ״הרעתי״ מודגש. אך בין אותיות (רש״י) הם
            // חלק מהטוקן — ה-lookaround פוסל התאמה שממשיכה דרך גרשיים לאות,
            // מדלג בעצמו על ניקוד שנותר, ומכיר גם בזוג-גרשים ('') כגרשיים.
            let p = build_literal_pattern("הרעתי").unwrap();
            let letters = super::super::HEBREW_LETTER_CLASS;
            let marks = super::super::ATTACHED_MARKS_CLASS;
            let qf = super::super::quote_boundary_fragment();
            assert!(p.starts_with(&format!("(?<![{letters}]{marks}{qf}?{marks})")));
            assert!(p.ends_with(&format!("(?!{marks}{qf}?{marks}[{letters}])")));
        }

        #[test]
        fn multi_word_joined_by_the_shared_separator() {
            // אותו מפריד של החיפוש המתקדם, כדי שמה שהאינדקס רואה כשני
            // טוקנים סמוכים (רווח, מקף, פיסוק דבוק) יודגש גם כאן.
            // ההתאמה עצמה נבדקת בצד Dart — ל-`regex` אין lookbehind.
            let p = build_literal_pattern("  כל   היום  ").unwrap();
            assert!(p.contains(&*super::super::WORD_SEPARATOR));
        }

        #[test]
        fn quotes_match_both_forms() {
            let ascii = build_literal_pattern("ז\"ל").unwrap();
            let hebrew = build_literal_pattern("ז\u{05F4}ל").unwrap();
            let curly = build_literal_pattern("ז\u{201D}ל").unwrap();
            assert_eq!(ascii, hebrew);
            // צורה טיפוגרפית בקלט מתנהגת כמו גרשיים רגילות.
            assert_eq!(ascii, curly);
            // המחלקה תופסת ", ״, צורות טיפוגרפיות וגם זוג גרשים מודפס.
            assert!(ascii.contains(super::super::GERSHAYIM_DISPLAY_CLASS));

            let geresh = build_literal_pattern("תוס'").unwrap();
            assert_eq!(geresh, build_literal_pattern("תוס\u{2019}").unwrap());
            assert!(geresh.contains(super::super::GERESH_DISPLAY_CLASS));
        }

        #[test]
        fn trailing_gershayim_is_lookahead_not_consumed() {
            // גרשיים נגרר בשאילתה מאומת ב-lookahead — אינו נצרך ולכן לא נכלל
            // ב-group(0) ולא נצבע (כמו generate_highlight_pattern).
            let p = build_literal_pattern("הרעתי\"").unwrap();
            assert!(p.contains("(?="), "trailing gershayim must be a lookahead");
            // גרשיים פנימי (ראשי-תיבות) נשאר נצרך ומודגש.
            let acronym = build_literal_pattern("רש\"י").unwrap();
            assert!(
                !acronym.contains("(?="),
                "internal gershayim must stay consuming"
            );
        }

        #[test]
        fn trailing_geresh_stays_consumed() {
            // גרש סוגר יחיד (תוס׳) הוא חלק מהטוקן לפי הטוקנייזר ולפי
            // generate_highlight_pattern — נשאר נצרך ומודגש, לא lookahead.
            let p = build_literal_pattern("תוס\u{05F3}").unwrap();
            assert!(
                !p.contains("(?="),
                "trailing single geresh must stay consumed"
            );
        }

        #[test]
        fn lookahead_only_on_last_word() {
            // גרש במילה לא-אחרונה ("ר׳ עקיבא") נצרך — אחרת המפריד שאחריו יפגוש
            // גרש במקום רווח והביטוי לא יימצא.
            let p = build_literal_pattern("ר\u{05F3} עקיבא").unwrap();
            assert!(
                !p.contains("(?="),
                "non-final abbrev geresh must be consumed, not a lookahead"
            );
        }

        #[test]
        fn word_separator_tolerates_maqaf() {
            // `אשר־שמע` בטקסט המוצג הוא שני טוקנים באינדקס, ולכן המקף חייב
            // להיות בתוך מחלקת המפריד.
            assert!(
                super::super::BREAK_CHAR_CLASS.contains("\\u05BE"),
                "separator must tolerate maqaf"
            );
        }

        #[test]
        fn bare_query_matches_printed_gershayim() {
            // Otzaria/otzaria#1054: המציאה חייבת ללכת אחרי ההדגשה — "רשי"
            // בלי גרשיים מוצא את רש״י המודפס, על כל צורות הגרשיים.
            // הבדיקה על תבנית-המילה בלבד: ל-fancy_regex אין lookbehind
            // באורך משתנה, וגבולות המילה מכוסים בבדיקות המבנה למעלה.
            let word = super::super::literal_charwise_pattern("רשי", true);
            let re = fancy_regex::Regex::new(&format!("^(?:{word})$")).unwrap();
            assert!(re.is_match("רש\u{05F4}י").unwrap());
            assert!(re.is_match("רש\"י").unwrap());
            // זוג-גרשים מודפס (מוסכמת קבצים ישנים) ≡ גרשיים.
            assert!(re.is_match("רש''י").unwrap());
            // בלי גרשיים בטקסט — עדיין נמצא.
            assert!(re.is_match("רשי").unwrap());
            // רווח אינו גרשיים — אין זליגה בין מילים.
            assert!(!re.is_match("רש י").unwrap());
        }

        #[test]
        fn optional_quotes_do_not_relax_explicit_quotes() {
            // שאילתה עם גרשיים מפורשים עדיין דורשת גרשיים בטקסט.
            let word = super::super::literal_charwise_pattern("רש\"י", true);
            let re = fancy_regex::Regex::new(&format!("^(?:{word})$")).unwrap();
            assert!(re.is_match("רש\u{05F4}י").unwrap());
            assert!(!re.is_match("רשי").unwrap());
        }

        #[test]
        fn metacharacters_are_escaped() {
            let p = build_literal_pattern("א.ב").unwrap();
            assert!(p.contains(r"\."));
        }

        #[test]
        fn empty_query_yields_none() {
            assert!(build_literal_pattern("").is_none());
            assert!(build_literal_pattern("   ").is_none());
        }
    }

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
        // בין אותיות — גרשיים אופציונליים: טרם נטול-גרשיים מדגיש גם דפוס
        // עם גרשיים (הטוקן-התאום של האינדקס).
        assert_eq!(
            hl.combined_pattern,
            format!(
                "ס{m}{q}פ{m}{q}ר{m}",
                m = ATTACHED_MARKS_CLASS,
                q = OPTIONAL_QUOTES
            )
        );
        assert_eq!(hl.word_patterns.len(), 1);
        assert_eq!(hl.word_boundary_eligible, vec![true]);
    }

    #[test]
    fn multi_word_joined_by_separator() {
        let hl = build("שלום עולם");
        assert_eq!(hl.word_patterns.len(), 2);
        assert!(hl.combined_pattern.contains(&*WORD_SEPARATOR));
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

    /// מספר המילים שהטוקנייזר של האינדקס מוצא ב-`text` — מקור האמת שאליו
    /// ספירת המילים המתווכות בהדגשה חייבת להתאים.
    fn tokenizer_word_count(text: &str) -> usize {
        let mut count = 0;
        let mut pos = 0;
        while let Some((_, end)) = crate::hebrew_tokenizer::next_token_boundaries(text, pos) {
            count += 1;
            pos = end;
        }
        count
    }

    /// המרווח המזערי שבו התבנית מתאימה את `text`, או `None` עד `max`.
    fn min_matching_distance(query: &str, text: &str, max: u32) -> Option<u32> {
        for distance in 0..=max {
            let hl = build_display_highlight(
                query,
                distance,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .unwrap();
            // fancy-regex: התבנית המשולבת כוללת lookahead, כמו ב-RegExp של Dart.
            let re = fancy_regex::Regex::new(&hl.combined_pattern).unwrap();
            if re.is_match(text).unwrap() {
                return Some(distance);
            }
        }
        None
    }

    #[test]
    fn the_three_classes_partition_every_char_by_tokenizer_rules() {
        // המחלקות נגזרות מהפרדיקטים של הטוקנייזר, והטסט אוכף גם את הגזירה
        // וגם את הזרות שביניהן — הזרות היא מה שמונע התפוצצות backtracking.
        let alnum = regex::Regex::new(&format!("^{}$", &*ALNUM_CLASS)).unwrap();
        let soft = regex::Regex::new(&format!("^{}$", &*SOFT_CHAR_CLASS)).unwrap();
        let brk = regex::Regex::new(&format!("^{}$", &*BREAK_CHAR_CLASS)).unwrap();

        for &(start, end) in SEPARATOR_DOMAIN {
            for code in start..=end {
                let Some(c) = char::from_u32(code) else {
                    continue;
                };
                let text = c.to_string();
                let in_alnum = alnum.is_match(&text);
                let in_soft = soft.is_match(&text);
                let in_break = brk.is_match(&text);

                assert_eq!(
                    in_alnum,
                    is_alnum_char(c),
                    "U+{code:04X} disagrees with is_alnum_char"
                );
                assert_eq!(
                    in_soft,
                    continues_token(c) && !is_alnum_char(c),
                    "U+{code:04X} disagrees with continues_token"
                );
                assert_eq!(
                    in_break,
                    !continues_token(c) && !is_markup_char(c),
                    "U+{code:04X} disagrees with continues_token"
                );
                // `<`/`>` שייכים לחלופת התג בלבד ולכן אינם בשום מחלקה;
                // כל תו אחר שייך לאחת בדיוק.
                assert_eq!(
                    [in_alnum, in_soft, in_break]
                        .iter()
                        .filter(|member| **member)
                        .count(),
                    if is_markup_char(c) { 0 } else { 1 },
                    "U+{code:04X} class membership"
                );
            }
        }
    }

    #[test]
    fn intermediate_word_is_counted_like_the_tokenizer() {
        // כל מקרה: הפער בין "תדע" ל"זרעך". המרווח המזערי שבו ההדגשה מתאימה
        // חייב להיות מספר הטוקנים שהטוקנייזר מוצא באותו פער.
        let gaps = [
            "אחת",
            "אחת שתים",
            // מקף, קו אנכי וסוף-פסוק מפרידים — הבאג שגרם להדגשה בלי תוצאות.
            "כי־גר יהיה",
            "אל־משה",
            "אחת|שתים",
            "אחת׃שתים",
            "אחת,שתים",
            "אחת;שתים",
            "אחת(שתים)",
            "אחת/שתים",
            "אחת=שתים",
            "אחת_שתים",
            // גרש/גרשיים, נקודה וסוגריים מרובעים אינם שוברים טוקן.
            "רמב״ם",
            "תוס'",
            "פ.ב.י",
            "3.14",
            "יב[ע]ר",
            // ניקוד וטעמים צמודים אינם שוברים מילה.
            "יָדֹעַ",
        ];

        for gap in gaps {
            let text = format!("תדע {gap} זרעך");
            let expected = tokenizer_word_count(gap) as u32;
            assert_eq!(
                min_matching_distance("תדע זרעך", &text, expected + 2),
                Some(expected),
                "טקסט {text:?}: הפער {gap:?} הוא {expected} מילים באינדקס"
            );
        }
    }

    #[test]
    fn html_tag_inside_a_word_does_not_split_it() {
        // התגים מוסרים לפני האינדוקס, ולכן `כי<b>גר` הוא טוקן אחד.
        assert_eq!(
            min_matching_distance("תדע זרעך", "תדע כי<b>גר זרעך", 3),
            Some(1)
        );
    }

    #[test]
    fn pattern_length_stays_bounded() {
        // התבנית מהודרת ב-Dart ומוחזקת שם במטמון; אורך חריג היה מייקר את
        // ההידור בכל שאילתה חדשה. חמש מילים במרווח 30 הוא הרע ביותר בפועל.
        let hl = build_display_highlight(
            "אמר רבי יוחנן משום שמעון",
            30,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert!(
            hl.combined_pattern.len() < 40_000,
            "pattern too long: {}",
            hl.combined_pattern.len()
        );
    }

    #[test]
    fn inline_tag_alone_between_the_words_never_matches() {
        // `strip_html_for_indexing` מוחק תג inline בלי רווח, ולכן `תדע<b>זרעך`
        // הוא טוקן אחד באינדקס והחיפוש לא ימצא אותו באף מרווח — ההדגשה
        // חייבת להתנהג כך גם היא, אחרת יש הדגשה בלי תוצאה.
        assert_eq!(min_matching_distance("תדע זרעך", "תדע<b>זרעך", 4), None);
        // תג עם רווח בתוכו הוא כן מפריד: האינדקס רואה שני טוקנים סמוכים.
        assert_eq!(
            min_matching_distance("תדע זרעך", "תדע<b> </b>זרעך", 4),
            Some(0)
        );
    }

    #[test]
    fn breaking_tag_between_the_words_is_a_separator() {
        // תג שבירה הופך לרווח באינדוקס (Otzaria/otzaria#949), ולכן ההדגשה
        // רואה בו מפריד — שתי המילים סמוכות.
        for text in [
            "תדע<br>זרעך",
            "תדע<br/>זרעך",
            "תדע<br />זרעך",
            "תדע</p><p>זרעך",
            "תדע<div class=\"x\">זרעך",
        ] {
            assert_eq!(
                min_matching_distance("תדע זרעך", text, 2),
                Some(0),
                "טקסט {text:?}: תג השבירה הוא מפריד"
            );
        }
        // מילה מתווכת שמופרדת בתג שבירה נספרת כמו באינדקס.
        assert_eq!(
            min_matching_distance("תדע זרעך", "תדע אחת<br>זרעך", 3),
            Some(1)
        );
        // תג שבירה בתוך מילה מפצל אותה — `זר<br>עך` הם שני טוקנים באינדקס,
        // ולכן המילה `זרעך` אינה מודגשת שם.
        assert_eq!(min_matching_distance("תדע זרעך", "תדע זר<br>עך", 4), None);
    }

    #[test]
    fn adjacent_words_match_across_transparent_punctuation() {
        // "תדע." + "זרעך" הם שני טוקנים סמוכים באינדקס (הנקודה נשמטת),
        // ולכן ההדגשה חייבת להתאים כבר במרווח 0.
        for text in ["תדע. זרעך", "תדע, זרעך", "תדע<b> </b>זרעך", "תדע׃ זרעך"]
        {
            assert_eq!(
                min_matching_distance("תדע זרעך", text, 2),
                Some(0),
                "טקסט {text:?}: המילים סמוכות באינדקס"
            );
        }
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
        assert!(hl.combined_pattern.contains(GERESH_DISPLAY_CLASS));
    }

    #[test]
    fn gershayim_stay_inside_word_like_engine_tokenizer() {
        // ז"ל הוא טוקן אחד (כמו HebrewTokenizer); הגרשיים בתבנית המילה
        // תופסות את שתי צורות הדפוס (" ו-״).
        let hl = build("ז\"ל");
        assert_eq!(hl.word_patterns.len(), 1);
        assert!(hl.combined_pattern.contains("ז"));
        // ", ״, צורה טיפוגרפית או זוג גרשים מודפס — טקסט התצוגה נשמר
        // כפי שנדפס.
        assert!(hl.combined_pattern.contains(GERSHAYIM_DISPLAY_CLASS));
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
            .contains(&charwise_display_pattern("שלם")));
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
            .contains(&charwise_display_pattern("חכם")));
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

    // ── Matched-terms patterns ─────────────────────────────────────────

    fn charwise(term: &str) -> String {
        charwise_display_pattern(term)
    }

    #[test]
    fn terms_pattern_sorts_longest_first() {
        // Leftmost-first alternation: ספר before הספרים would truncate the
        // longer word's highlight to its inner substring.
        let p = build_terms_display_pattern(&["ספר".to_string(), "הספרים".to_string()]);
        assert_eq!(p, format!("(?:{}|{})", charwise("הספרים"), charwise("ספר")));
    }

    #[test]
    fn terms_pattern_dedups_and_bounds_budget() {
        let dup = build_terms_display_pattern(&["ספר".to_string(), "ספר".to_string()]);
        assert_eq!(dup, charwise("ספר"));

        // Terms far beyond the char budget: the pattern stays bounded but
        // never empty.
        let many: Vec<String> = (0..2_000).map(|i| format!("מלה{i:04}")).collect();
        let p = build_terms_display_pattern(&many);
        assert!(!p.is_empty());
        // The budget bounds the branch bodies; `|` separators and the `(?:)`
        // wrapper add at most one char per branch plus the group frame.
        let branch_count = p.matches('|').count() + 1;
        assert!(p.chars().count() <= MAX_DISPLAY_PATTERN_CHARS + branch_count + 4);
    }

    #[test]
    fn from_terms_uses_matched_terms_and_keeps_boundaries() {
        // A typo-matched term the query-shape builder could never know about.
        let options = options_for("משה", 0, "שגיאות כתיב");
        let hl = build_display_highlight_from_terms(
            "משה",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &options,
            &[vec!["מסה".to_string()]],
        )
        .unwrap();
        assert_eq!(hl.combined_pattern, charwise("מסה"));
        // Matched terms are complete index tokens — boundaries stay valid
        // even for options that waive them on the query-shape path.
        assert_eq!(hl.word_boundary_eligible, vec![true]);
    }

    #[test]
    fn from_terms_partial_word_uses_query_shape_substring() {
        let options = options_for("ספר", 0, "חלק ממילה");

        let query_shape =
            build_display_highlight("ספר", 0, &HashMap::new(), &HashMap::new(), &options).unwrap();
        assert_eq!(query_shape.word_boundary_eligible, vec![false]);

        let from_terms = build_display_highlight_from_terms(
            "ספר",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &options,
            &[vec!["הספרים".to_string(), "ספר".to_string()]],
        )
        .unwrap();
        // מילת חלק-ממילה מדגישה את התת-מחרוזת שהוקלדה (כמו ה-query-shape),
        // עם כיסוי מלא ולא חסום-תקציב — במקום המילה המנוטה השלמה.
        assert_eq!(from_terms.word_boundary_eligible, vec![false]);
        assert_eq!(from_terms.combined_pattern, query_shape.combined_pattern);
    }

    #[test]
    fn from_terms_typo_with_expansion_keeps_matched_variants() {
        // typo + חלק ממילה: החיפוש מוצא וריאנט שגיאת-כתיב (מסה) שה-query-shape
        // לעולם לא היה מפיק. חייבים לשמור את מונחי-האינדקס גם כשיש הרחבה,
        // אחרת הווריאנט לא יודגש. גבול-המילה מוותר בגלל ההרחבה.
        let options = HashMap::from([(
            "ספר_0".to_string(),
            HashMap::from([
                ("שגיאות כתיב".to_string(), true),
                ("חלק ממילה".to_string(), true),
            ]),
        )]);
        let hl = build_display_highlight_from_terms(
            "ספר",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &options,
            &[vec!["ספר".to_string(), "םפר".to_string()]],
        )
        .unwrap();
        // הווריאנט השגוי נשמר בתבנית, והגבול מוותר (לא נדחה בתוך צורה מורחבת).
        assert!(hl.combined_pattern.contains(&charwise("םפר")));
        assert_eq!(hl.word_boundary_eligible, vec![false]);
    }

    #[test]
    fn from_terms_falls_back_per_word() {
        // Word 0 has no matched terms → query-shape pattern; word 1 has one.
        let hl = build_display_highlight_from_terms(
            "שלום עולם",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &[Vec::new(), vec!["עולם".to_string()]],
        )
        .unwrap();
        assert_eq!(hl.word_patterns.len(), 2);
        assert_eq!(hl.word_patterns[0], charwise("שלום"));
        assert_eq!(hl.word_patterns[1], charwise("עולם"));
        assert!(hl.combined_pattern.contains(&*WORD_SEPARATOR));
    }

    #[test]
    fn from_terms_without_any_terms_matches_query_shape_builder() {
        // No per-word terms at all → same output as build_display_highlight.
        let options = options_for("שלום", 0, "כתיב מלא/חסר");
        let from_terms = build_display_highlight_from_terms(
            "שלום",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &options,
            &[],
        )
        .unwrap();
        let query_shape =
            build_display_highlight("שלום", 0, &HashMap::new(), &HashMap::new(), &options).unwrap();
        assert_eq!(from_terms.combined_pattern, query_shape.combined_pattern);
        assert_eq!(
            from_terms.word_boundary_eligible,
            query_shape.word_boundary_eligible
        );
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
