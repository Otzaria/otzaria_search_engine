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
//! * The DFA state count is capped at 8 192 (vendored tantivy-fst; upstream
//!   caps at 1 000 — see vendor/tantivy-fst/README.md). A pattern whose
//!   length grows unboundedly (e.g. `.*`) can blow past this and cause a
//!   compile error at query time. Every window/wildcard in this module is
//!   therefore bounded.
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
/// מפתחות "ניקוד"/"טעמים" הפר-מילה: כשמסומנים למילה, סימנים שהוקלדו בה
/// (מהמחלקה המסומנת) נדרשים להופיע בטרם — בעוד מילים לא-מסומנות נשארות
/// חופשיות-סימנים. עצם נוכחות מפתח דלוק באחת המילים מעבירה את השאילתה
/// כולה לשדה המנוקד (`textVocalized`), שמאונדקסות בו רק שורות מנוקדות.
pub const OPT_MATCH_NIKUD: &str = "ניקוד";
pub const OPT_MATCH_TAAMIM: &str = "טעמים";
const OPT_PREFIX: &str = "קידומות";
const OPT_SUFFIX: &str = "סיומות";
const OPT_GRAM_PREFIX: &str = "קידומות דקדוקיות";
const OPT_GRAM_SUFFIX: &str = "סיומות דקדוקיות";
const OPT_SPELLING: &str = "כתיב מלא/חסר";
const OPT_PARTIAL: &str = "חלק ממילה";
/// אפשרות "קידומות ארמיות" פר-מילה: קידומות ארמיות (ד/כד/אד/מד וכו' —
/// קבוצת הקידומות הדקדוקיות, שכבר נושאת את הצורות הארמיות) לפני המילה.
const OPT_ARAMAIC_PREFIX: &str = "קידומות ארמיות";
/// אפשרות "סיומות ארמיות" פר-מילה: שקילות אות סופית ה↔א (מלכה↔מלכא)
/// ו-ם↔ן (חכמים↔חכמין). סימון שתי אפשרויות הארמית יחד משחזר את התנהגות
/// אפשרות "ארמית" ההיסטורית (קידומות על כל וריאנט סופית).
const OPT_ARAMAIC_SUFFIX: &str = "סיומות ארמיות";
/// "התעלם מגרשיים": גרש/גרשיים שהוקלדו במילה מוסרים ממנה לפני בניית
/// התבנית — `רמב"ם` מחפש `רמבם`, שמותאם באינדקס גם לצורות עם גרשיים
/// (דרך הטוקן-התאום נטול-הגרשיים שהאינדוקס מטמיע לכל מילה כזו).
const OPT_IGNORE_QUOTES: &str = "התעלם מגרשיים";
/// "תרגום ארמי": הרחבת המילה בתרגומיה מהמילון הארמי-עברי (בשני
/// הכיוונים). ההרחבה קורית בשכבת המנוע (שם נטען המילון) — המפתח מוגדר
/// כאן כדי שכל מפתחות ה-UI ירוכזו במקום אחד.
pub const OPT_TRANSLATION: &str = "תרגום ארמי";
/// "ראשי תיבות": פענוח ר"ת דו-כיווני מהמילון (`רמב"ם`↔`רבי משה בן
/// מיימון`). כמו התרגום, ההרחבה קורית בשכבת המנוע — אך הפענוח רב-מילי
/// ולכן נצרך כתת-שאילתת OR ולא כחלופה חד-מילתית (ראו
/// `SearchEngine::acronym_alternatives`). מוגדר כאן לריכוז מפתחות ה-UI.
pub const OPT_ACRONYM: &str = "ראשי תיבות";

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

/// Char budget for a *joined* per-word pattern on the phrase path, where
/// `RegexPhraseQuery` compiles each word's whole pattern as one DFA. It is a
/// crude proxy for the vendored tantivy-fst 8 192-state cap (chars ≠ states —
/// heavily overlapping wildcard branches can fail far below this), kept as a
/// cheap guard for the path that has no per-branch alternative yet. Scaled ×6
/// with the state-cap raise from 1 000 (see vendor/tantivy-fst/README.md),
/// leaving headroom for shape variance; calibrate via `benchmark_cli`.
const MAX_PATTERN_CHARS: usize = 6_000;

/// Phrase-path variation cap when typo tolerance is active (substitution +
/// deletion + insertion can produce many candidates; we keep the most useful
/// ones). The historical worst shape (wildcard-wrapped typo+partial branches)
/// determinizes to roughly ~100 states per branch, so 64 branches ≈ 6.4k
/// states — inside the vendored 8 192-state cap with margin.
const MAX_TYPO_VARIATIONS: usize = 64;

/// Phrase-path variation cap without typo tolerance (spelling/morphological
/// combos are naturally smaller, so a tighter budget still covers them fully).
const MAX_NORMAL_VARIATIONS: usize = 48;

/// Per-word branch budgets, chosen by query shape.
///
/// The phrase path compiles each word's joined pattern as a single DFA, so it
/// needs both the char budget and tight variation caps. The single-word path
/// compiles per branch — no joined DFA exists to protect — so the char budget
/// drops entirely and the variation caps relax. They stay finite: each branch
/// costs one term-dictionary scan per segment at collection time (an
/// unproductive branch never trips `max_expansions` but still pays its scan),
/// so the count cap remains the collection-cost guard.
struct VariationBudget {
    /// Branch-count cap when typo tolerance is active.
    typo_variations: usize,
    /// Branch-count cap without typo tolerance.
    normal_variations: usize,
    /// Char budget for the joined pattern; `None` when compiled per branch.
    max_pattern_chars: Option<usize>,
}

const PHRASE_BUDGET: VariationBudget = VariationBudget {
    typo_variations: MAX_TYPO_VARIATIONS,
    normal_variations: MAX_NORMAL_VARIATIONS,
    max_pattern_chars: Some(MAX_PATTERN_CHARS),
};

const SINGLE_WORD_BUDGET: VariationBudget = VariationBudget {
    typo_variations: 128,
    normal_variations: 64,
    max_pattern_chars: None,
};

/// Cap on כתיב מלא/חסר branches folded into one pattern. The generator is
/// 2^n in the count of optional ו/י letters; this keeps it polynomial.
/// Shared with the display-highlight builder so search and highlighting fan
/// out identically.
pub(crate) const MAX_SPELLING_BRANCHES: usize = 16;

/// `max_expansions` ceiling for the phrase path (`RegexPhraseQuery`).
///
/// tantivy enforces it cumulatively across all word positions *before*
/// loading postings, and its doc_freq bucketing contains the post-expansion
/// cost — so the ceiling guards the memory of materialized expansions, not
/// scan time. Half of tantivy's own default (16 384) as a conservative first
/// step; overflowing it surfaces tantivy's `InvalidArgument` to the caller
/// (unlike the single-word path, which degrades instead of erroring).
const PHRASE_MAX_EXPANSIONS: u32 = 8_192;

/// Term-count ceiling for a single vocalized word with no expansion options.
/// Free-mark runs make even a "plain" vocalized pattern an expansion (every
/// vocalization of the word matches), so the mark-free literal ceiling (10)
/// is far too tight; the postings budget in `single_regex_term_query`
/// remains the true cost guard.
const VOC_SINGLE_WORD_MAX_EXPANSIONS: u32 = 4_096;

// ── Public result type ─────────────────────────────────────────────────────

/// Everything the Tantivy layer needs to execute an advanced search.
pub struct AdvancedQuery {
    /// One pattern per query word (in order). Each is a tantivy-fst–compatible
    /// whole-term regex with its top-level alternation kept structured, so the
    /// single-word path can compile every branch as its own small DFA.
    pub regex_terms: Vec<WordPattern>,
    /// מילות המקור המנורמלות, מיושרות ל-`regex_terms` — מאפשרות לזהות מילה
    /// שהוקלדה פעמיים גם כשהאפשרויות פר-מילה מייצרות לה תבניות שונות.
    pub words: Vec<String>,
    /// Per-pair intermediate-word allowance: `gaps[i]` is how many words may
    /// separate query words `i` and `i+1` (length `words-1`; empty for a
    /// single word). Resolved from `custom_spacing`/`distance` by
    /// [`resolve_gaps`], the same resolution the display-highlight builder
    /// uses — so what the engine matches and what an opened book highlights
    /// agree pair-by-pair.
    pub gaps: Vec<u32>,
    /// Term-dictionary expansion limit passed to `RegexPhraseQuery`.
    pub max_expansions: u32,
    /// Tokens to expand through a Levenshtein-1 automaton scan (single word
    /// with typo tolerance and no other expansion option — see
    /// [`prepare_advanced_query`]). When non-empty, `regex_terms` carries only
    /// the exact candidate forms and the engine adds one automaton scan per
    /// token per segment: the whole edit-distance-1 neighborhood for the cost
    /// of a single scan, instead of ≤128 sampled literal-variant scans. The
    /// scan runs after the exact branches and under the same collection
    /// budgets — lowest priority, skipped if the exact forms already exhaust
    /// a budget. Empty on every other query shape.
    pub typo_tokens: Vec<String>,
}

// ── Word patterns (structured top-level alternation) ───────────────────────

/// A per-word regex term with its top-level alternation kept structured.
///
/// tantivy-fst compiles a whole pattern into one DFA capped at 8 192 states
/// (vendored; upstream caps at 1 000). Wildcard-wrapped branches overlap
/// heavily, so a combined `(?:b1|…|bN)` can blow the cap even when every
/// branch alone is tiny (the real typo+partial pattern — 48 branches, 806
/// chars — failed under the upstream cap while all 48 branches compile
/// individually). Keeping the branches separate lets the engine compile each
/// one as its own small DFA and stream all matches into a single
/// `TermSetQuery`, which never touches the state limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordPattern {
    /// A pattern with no top-level alternation (compiled as-is).
    Literal(String),
    /// Top-level alternation branches; each is a standalone whole-term regex.
    Alternation(Vec<String>),
}

impl WordPattern {
    /// The branches to compile individually (a literal is its own single
    /// branch).
    pub fn branches(&self) -> &[String] {
        match self {
            WordPattern::Literal(pattern) => std::slice::from_ref(pattern),
            WordPattern::Alternation(branches) => branches,
        }
    }

    /// The combined single-regex form (`(?:b1|b2|…)`), used where one pattern
    /// string is required: `RegexPhraseQuery` terms and highlight automatons.
    pub fn joined(&self) -> String {
        match self {
            WordPattern::Literal(pattern) => pattern.clone(),
            WordPattern::Alternation(branches) => format!("(?:{})", branches.join("|")),
        }
    }

    /// Appends extra standalone branches (lowest priority — the collection
    /// budgets truncate from the back). Used by the vocalized typo path to
    /// ride plain-dictionary Levenshtein variants onto a word's branch list.
    pub fn with_extra_branches(self, extra: Vec<String>) -> WordPattern {
        if extra.is_empty() {
            return self;
        }
        let mut branches = match self {
            WordPattern::Literal(pattern) => vec![pattern],
            WordPattern::Alternation(branches) => branches,
        };
        branches.extend(extra);
        WordPattern::Alternation(branches)
    }

    /// Parses a raw regex string (a term arriving through the public string
    /// API) by splitting its top-level alternation. Patterns without one —
    /// including alternations nested inside a larger expression — stay
    /// [`WordPattern::Literal`], preserving their exact semantics.
    pub fn parse(pattern: &str) -> WordPattern {
        let branches = split_top_level_alternation(pattern);
        if branches.len() <= 1 {
            WordPattern::Literal(pattern.to_string())
        } else {
            WordPattern::Alternation(branches)
        }
    }
}

/// Scanner state for `[...]` character classes, where `|`/`(`/`)` are
/// literals. A `]` immediately after `[` (or after `[^`) is itself a literal
/// and does not close the class — the same rule regex-syntax applies.
#[derive(Default)]
struct CharClassScanner {
    in_class: bool,
    index: usize, // chars seen since `[` (for the leading literal-`]` rule)
    negated: bool,
}

impl CharClassScanner {
    /// Processes one char while inside a class. Returns `true` when the char
    /// was consumed here (i.e. the scanner is inside a class).
    fn consume_inside(&mut self, ch: char) -> bool {
        if !self.in_class {
            return false;
        }
        if self.index == 0 && ch == '^' {
            self.negated = true;
        } else {
            let literal_close_index = usize::from(self.negated);
            if ch == ']' && self.index > literal_close_index {
                self.in_class = false;
            }
        }
        self.index += 1;
        true
    }

    /// Opens a new character class (called on `[` outside a class).
    fn open(&mut self) {
        self.in_class = true;
        self.index = 0;
        self.negated = false;
    }
}

/// Returns the inner content when one group wraps the whole pattern — either
/// capturing `(…)` or the non-capturing `(?:…)` form [`build_word_regex`]
/// emits. Escape-, class-, and nesting-aware; a group that closes mid-pattern
/// (e.g. `(ו|מ)?משה`) is not stripped. Other `(?…)` constructs are left alone.
fn strip_enclosing_group(pattern: &str) -> &str {
    if !pattern.starts_with('(') {
        return pattern;
    }
    let inner_start = if pattern.starts_with("(?:") {
        3
    } else if pattern[1..].starts_with('?') {
        return pattern;
    } else {
        1
    };
    let mut depth: i32 = 0;
    let mut escaped = false;
    let mut class = CharClassScanner::default();
    for (i, ch) in pattern.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if class.consume_inside(ch) {
            continue;
        }
        match ch {
            '[' => class.open(),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    // The group opened at char 0 just closed — it wraps the
                    // whole pattern only if this is the last char.
                    return if i == pattern.len() - 1 {
                        &pattern[inner_start..i]
                    } else {
                        pattern
                    };
                }
            }
            _ => {}
        }
    }
    pattern
}

/// Splits a regex pattern on top-level (depth-0) `|`, aware of escapes,
/// nested groups, and `[...]` classes (so `[ם|ן]` is never split), after
/// stripping one enclosing group. Empty branches match only the empty string
/// and no indexed term is empty, so they are dropped. A pattern without a
/// top-level alternation comes back as a single element.
pub(crate) fn split_top_level_alternation(pattern: &str) -> Vec<String> {
    let inner = strip_enclosing_group(pattern);

    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth: i32 = 0;
    let mut escaped = false;
    let mut class = CharClassScanner::default();

    for ch in inner.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            current.push(ch);
            escaped = true;
            continue;
        }
        if class.consume_inside(ch) {
            current.push(ch);
            continue;
        }
        match ch {
            '[' => {
                class.open();
                current.push(ch);
            }
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
            }
            '|' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    parts.push(current);

    parts.into_iter().filter(|p| !p.is_empty()).collect()
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
    /// קידומות ארמיות ([`OPT_ARAMAIC_PREFIX`]): ד/כד/אד/מד וכו' לפני המילה.
    pub aramaic_prefix: bool,
    /// סיומות ארמיות ([`OPT_ARAMAIC_SUFFIX`]): שקילות סופיות ה↔א, ם↔ן.
    pub aramaic_suffix: bool,
    /// התעלם מגרשיים ([`OPT_IGNORE_QUOTES`]): גרש/גרשיים מוסרים מהמילה
    /// לפני בניית התבנית. לא אפשרות הרחבה (המילה נשארת ליטרל יחיד) ולכן
    /// אינה חלק מ-[`Self::expands_beyond_typo`].
    pub ignore_quotes: bool,
    /// התאמת ניקוד פר-מילה ([`OPT_MATCH_NIKUD`]) — נגזרת ל-[`VocalizedFlags`]
    /// של המילה, לא אפשרות הרחבה (ולכן לא חלק מ-[`Self::expands_beyond_typo`]).
    pub nikud: bool,
    /// כמו [`Self::nikud`] עבור טעמי המקרא ([`OPT_MATCH_TAAMIM`]).
    pub taamim: bool,
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
            aramaic_prefix: get(OPT_ARAMAIC_PREFIX),
            aramaic_suffix: get(OPT_ARAMAIC_SUFFIX),
            ignore_quotes: get(OPT_IGNORE_QUOTES),
            nikud: get(OPT_MATCH_NIKUD),
            taamim: get(OPT_MATCH_TAAMIM),
        }
    }

    fn max_variations(&self, budget: &VariationBudget) -> usize {
        if self.typo {
            budget.typo_variations
        } else {
            budget.normal_variations
        }
    }

    /// Whether any expansion option besides typo tolerance is active. Typo
    /// composes with these (each typo variant is fed through the same
    /// morphology/spelling pattern builder), which a Levenshtein automaton
    /// cannot replicate in one scan — so the automaton path only replaces
    /// literal typo variants when this is `false`.
    fn expands_beyond_typo(&self) -> bool {
        self.prefix
            || self.suffix
            || self.gram_prefix
            || self.gram_suffix
            || self.spelling
            || self.partial
            || self.aramaic_prefix
            || self.aramaic_suffix
    }

    /// האם סומנה אפשרות הרחבה שאינה ארמית — קובע אם הענף הארמי צריך
    /// להתלוות לענף הרגיל (שממשיך לשאת את שאר האפשרויות) או לעמוד לבדו.
    fn expands_besides_aramaic(&self) -> bool {
        self.prefix
            || self.suffix
            || self.gram_prefix
            || self.gram_suffix
            || self.spelling
            || self.partial
    }
}

// ── Tokenisation ───────────────────────────────────────────────────────────

/// Normalises punctuation to the tokenizer's rules (the query-side mirror of
/// `HebrewTokenizer`):
/// * גרשיים: `״`/`“`/`”`→`"`, גרש: `׳`/`‘`/`’`→`'` (הצורות הטיפוגרפיות
///   נפוצות בטקסטים שעברו Word/OCR ומשמשות לסירוגין ברינדור RTL).
/// * מפרידים → רווח: מקף/מינוס, `|`, ופיסוק דבוק `,;:!?(){}` — כמו
///   בטוקנייזר, שם הם שוברים טוקן.
/// * שקופים נמחקים: `*[]^$\+.~\`` ותווים בלתי-נראים (bidi/zero-width) —
///   כמו בטוקנייזר, שם הם נבלעים בלי לשבור.
/// * כיווץ רצפי רווחים לרווח יחיד; trim.
pub fn sanitize_query(query: &str) -> String {
    const STRIP: &[char] = &['*', '[', ']', '^', '$', '\\', '+', '.', '~', '`'];
    let mut buf = String::with_capacity(query.len());
    for ch in query.chars() {
        match ch {
            '\u{05F4}' | '\u{201C}' | '\u{201D}' => buf.push('"'),
            '\u{05F3}' | '\u{2018}' | '\u{2019}' => buf.push('\''),
            '\u{05BE}' | '-' | '|' | ',' | ';' | ':' | '!' | '?' | '(' | ')' | '{' | '}' => {
                buf.push(' ')
            }
            c if STRIP.contains(&c) || is_invisible_char(c) => {}
            c => buf.push(c),
        }
    }
    collapse_whitespace(&buf)
}

pub(crate) fn collapse_whitespace(s: &str) -> String {
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

/// Splits a sanitised query into word tokens, mirroring the `HebrewTokenizer`
/// the `text` field is indexed with — literally: the boundaries come from
/// the tokenizer's own scan core (`next_token_boundaries`), so the two sides
/// cannot drift apart. The quote rules, for reference:
/// * `"` between word characters is part of the token (`ז"ל`, `רמב"ם`);
///   at a word edge it separates (`רמב"` → `רמב`).
/// * `'` between word characters is part of the token (`ד'אש`, `ג'ורג'`);
///   a trailing `'` is absorbed as the token's last character (`תוס'`).
/// * `''` between word characters collapses to a single `"` (the old-file
///   convention `רמב''ם` ≡ `רמב"ם`); any other quote run separates.
///
/// `sanitize_query` has already folded `״`/`׳` and the typographic forms to
/// ASCII quotes, turned the separators into spaces and stripped the
/// transparent punctuation — so unlike the index side, the only token-text
/// normalisation left here is the `''`→`"` collapse. Attached marks are
/// intentionally KEPT in the word (the vocalized path needs them; the plain
/// path normalises the query before splitting).
pub fn split_query_words(query: &str) -> Vec<String> {
    let cleaned = sanitize_query(query);
    let mut words = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = crate::hebrew_tokenizer::next_token_boundaries(&cleaned, pos) {
        let mut word = String::with_capacity(end - start);
        for c in cleaned[start..end].chars() {
            if c == '\'' && word.ends_with('\'') {
                // זוג גרשים בתוך הגבולות הוא פנימי בהכרח (גרש סוגר נבלע
                // יחיד) — מאוחד לגרשיים, כמו בטוקנייזר.
                word.pop();
                word.push('"');
            } else {
                word.push(c);
            }
        }
        words.push(word);
        pos = end;
    }
    words
}

/// Normalises text to the index term dictionary's shape: folds presentation
/// forms, strips *attached* nikud/cantillation marks and lowercases. This is
/// how the `"hebrew"` analyzer maps indexed text to terms.
///
/// Deliberately does NOT delete the whole U+0591–U+05C7 range: that would
/// also remove the separator punctuation inside it (maqaf, paseq, sof pasuq),
/// gluing `"אשר־שמע"` into one word before `split_query_words` gets to treat
/// the maqaf as a word break.
pub(crate) fn normalize_for_index(text: &str) -> String {
    fold_presentation_forms(text)
        .chars()
        .filter(|c| !is_word_mark(*c))
        .collect::<String>()
        .to_lowercase()
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

/// Combining marks כלליים (U+0300–U+036F) — בקורפוס: נקודה עילית (U+0307)
/// וחברותיה בתעתיק ערבית-יהודית (`כלת̇ום`, `מצ̇ארע`). מטופלים כמו סימן צמוד
/// עברי: ממשיכי-מילה שמוסרים מטרם השדה הרגיל ונשמרים בשדה המנוקד. בכוונה
/// *לא* קטגוריית `Mark` המלאה של Unicode — היא כוללת את הטווח העברי שכבר
/// מטופל עם החרגות-מפרידים, וכתבים אחרים עם דקויות משלהם.
#[inline]
pub(crate) fn is_general_combining_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}')
}

/// כל סימן ממשיך-מילה: סימן עברי צמוד, combining mark כללי, או varika
/// (U+FB1E — Presentation Form צמוד שמקופל ל-`""`; בלי ההכללה כאן הוא היה
/// שובר מילה בקלט לא-מנורמל למרות שהקיפול פשוט בולע אותו).
#[inline]
pub(crate) fn is_word_mark(c: char) -> bool {
    is_attached_mark(c) || is_general_combining_mark(c) || c == '\u{FB1E}'
}

/// תווים בלתי-נראים (bidi controls, zero-width, BOM) — הסט שמסלול ה-PDF
/// מוחק מאז ומתמיד ([`normalize_pdf_text_for_indexing`]). בטוקנייזר הם
/// שקופים (נבלעים, לא שוברים) וב-`sanitize_query` נמחקים — עקבי עם
/// התנהגות ה-PDF שמדביקה אותם.
#[inline]
pub(crate) fn is_invisible_char(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// מסיר ניקוד וטעמים צמודים בלבד ([`is_attached_mark`]) — משאיר מקף, פסק
/// וסוף-פסוק שהם פיסוק, לא ניקוד.
pub fn strip_attached_marks(text: &str) -> String {
    text.chars().filter(|c| !is_attached_mark(*c)).collect()
}

// ── Vocalized (nikud/te'amim) mark classes ─────────────────────────────────

/// ניקוד במובן הצר: תנועות (U+05B0–U+05BB), דגש/מפיק (U+05BC), רפה
/// (U+05BF), נקודות שי"ן/שׂי"ן (U+05C1/U+05C2) וקמץ קטן (U+05C7).
#[inline]
pub(crate) fn is_nikud_mark(c: char) -> bool {
    matches!(
        c,
        '\u{05B0}'..='\u{05BC}' | '\u{05BF}' | '\u{05C1}' | '\u{05C2}' | '\u{05C7}'
    )
}

/// טעמי המקרא + מתג (U+05BD) + הנקודות העליונות/תחתונות (U+05C4/U+05C5) —
/// כל סימן צמוד שאינו ניקוד. המתג מסווג כאן בכוונה: הוא מגיע כמעט תמיד
/// מהדבקת פסוק מטקסט-מקור (לא מהקלדה מכוונת), ולכן דגל הניקוד לבדו לא
/// יהפוך אותו לדרישה שתפסול טקסטים מנוקדים ללא מתג.
#[inline]
pub(crate) fn is_taam_mark(c: char) -> bool {
    is_attached_mark(c) && !is_nikud_mark(c)
}

/// האם השורה נושאת סימן צמוד כלשהו — בדיקת-הכניסה הזולה של צינור האינדוקס
/// (רק שורות כאלה נכתבות לשדה `textVocalized`).
pub fn contains_attached_marks(text: &str) -> bool {
    text.chars().any(is_attached_mark)
}

/// אילו מחלקות סימנים "נחשבות" בחיפוש מנוקד — נגזר מדגלי
/// `match_nikud`/`match_taamim` של ה-API. סימן שהוקלד ממחלקה דלוקה נדרש
/// להופיע בטרם; סימן ממחלקה כבויה נמחק מהשאילתה (ולעולם אינו דרישה);
/// סימנים שלא הוקלדו חופשיים תמיד.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct VocalizedFlags {
    pub nikud: bool,
    pub taamim: bool,
}

impl VocalizedFlags {
    pub fn new(nikud: bool, taamim: bool) -> Self {
        Self { nikud, taamim }
    }

    /// האם חיפוש מנוקד בכלל פעיל (אחרת מסלול השאילתה הרגיל רץ כרגיל).
    pub fn any(&self) -> bool {
        self.nikud || self.taamim
    }

    /// האם סימן שהוקלד הוא דרישת-התאמה תחת הדגלים האלה.
    pub(crate) fn requires(&self, c: char) -> bool {
        (self.nikud && is_nikud_mark(c)) || (self.taamim && is_taam_mark(c))
    }

    /// איחוד (OR) — ממזג את הדגלים הגלובליים של ה-API עם דגלים שנגזרו
    /// מאפשרויות פר-מילה.
    pub fn or(self, other: Self) -> Self {
        Self {
            nikud: self.nikud || other.nikud,
            taamim: self.taamim || other.taamim,
        }
    }
}

/// אילו מחלקות סימנים מבקשות אפשרויות ה-UI הפר-מילה (איחוד על כל המילים).
/// קובע האם שאילתה מתקדמת רצה על השדה המנוקד; הדרישה פר-תו נגזרת פר-מילה
/// בנפרד בתוך [`prepare_advanced_query_vocalized`]. הסריקה עוברת על כל
/// מפות האפשרויות בלי לאמת את מפתחות `"{word}_{index}"` — מפתח תקול ממילא
/// נופל בשקט בשלב בניית התבניות, אבל בקשת-ניקוד לא צריכה להיעלם איתו.
pub fn options_vocalized_flags(
    search_options: &HashMap<String, HashMap<String, bool>>,
) -> VocalizedFlags {
    let mut flags = VocalizedFlags::default();
    for map in search_options.values() {
        flags.nikud |= map.get(OPT_MATCH_NIKUD).copied().unwrap_or(false);
        flags.taamim |= map.get(OPT_MATCH_TAAMIM).copied().unwrap_or(false);
    }
    flags
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

/// שמות תגי HTML שהדפדפן מציג כהפרדה ויזואלית — מעבר שורה או גבול בלוק.
/// באינדוקס הם הופכים לרווח, אחרת `המורים<br>כי` נטמע כטוקן אחד: מילים
/// מוצגות דבוקות בתוצאות, וחיפוש של כל אחת מהן מחטיא (Otzaria/otzaria#949).
/// תגי inline (`<b>`, `<span>`…) נשארים מחיקה נטו — `מי<b>לה` היא מילה אחת.
pub(crate) const BREAKING_TAG_NAMES: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "caption",
    "center",
    "dd",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
];

/// האם תוכן תג (`body` — התווים שבין `<` ל-`>`) הוא תג שבירה: `/` פותח
/// אופציונלי, שם באדישות לרישיות, ואחריו רק תו שאינו אות/ספרה (או כלום).
fn is_breaking_tag(body: &[char]) -> bool {
    let mut idx = 0;
    if idx < body.len() && body[idx] == '/' {
        idx += 1;
    }
    let start = idx;
    while idx < body.len() && body[idx].is_ascii_alphanumeric() {
        idx += 1;
    }
    // השם הארוך ביותר בן 10 תווים (figcaption) — חריגה היא לא-תג מהר.
    if idx == start || idx - start > 10 {
        return false;
    }
    let name: String = body[start..idx]
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    BREAKING_TAG_NAMES.contains(&name.as_str())
}

/// Strips HTML tags and entities like the Dart `stripHtmlIfNeeded`, with one
/// deliberate divergence: breaking tags ([`BREAKING_TAG_NAMES`]) become a
/// space, so words the reader sees on separate lines stay separate tokens.
/// The whitespace entities become a space first (so adjacent words are not
/// merged), then `<…>` tags and remaining `&…;` entities are removed.
///
/// Char-based like the Dart regex `<[^>]*>|&[^;]+;`: a `<` is dropped
/// through the next `>` (kept verbatim if unterminated); an `&` is dropped
/// through the next `;` only if at least one non-`;` char precedes it.
pub fn strip_html_for_indexing(text: &str) -> String {
    // Fast path: this runs on every corpus line at indexing time, and a line
    // with no markup at all needs none of the passes (or allocations) below.
    if !text.contains(['<', '&']) {
        return text.to_string();
    }
    // Named whitespace entities → space, matching Dart's pre-pass order so
    // they are not swallowed by the generic `&…;` rule below. Each `replace`
    // allocates, so skip the chain when the line has no entity at all.
    let spaced = if text.contains('&') {
        text.replace("&nbsp;", " ")
            .replace("&thinsp;", " ")
            .replace("&ensp;", " ")
            .replace("&emsp;", " ")
    } else {
        text.to_string()
    };

    let chars: Vec<char> = spaced.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '<' => {
                // `<[^>]*>` — find the closing `>`; drop the whole span.
                if let Some(close) = chars[i + 1..].iter().position(|&c| c == '>') {
                    if is_breaking_tag(&chars[i + 1..i + 1 + close]) {
                        out.push(' ');
                    }
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

/// Fused fold(presentation forms) → strip(attached marks) → collapse
/// whitespace, in a single pass with one output allocation. Semantically
/// identical to `collapse_whitespace(&strip_attached_marks(
/// &fold_presentation_forms(s)))` (verified by the ingestion parity tests) —
/// fusing matters because this runs on every line of the corpus. `drop`
/// filters extra characters before folding (the PDF path drops invisibles);
/// `strip_marks: false` keeps nikud/cantillation (the vocalized field).
fn fold_strip_collapse(s: &str, strip_marks: bool, drop: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(s.len());
    // Collapse+trim in one go: a whitespace run becomes a single pending
    // space, emitted only when more content follows (never leading/trailing).
    let mut pending_space = false;
    let mut emit = |out: &mut String, c: char| {
        if strip_marks && is_attached_mark(c) {
            return;
        }
        if c.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    };
    for c in s.chars() {
        if drop(c) {
            continue;
        }
        match fold_presentation_form(c) {
            Some(folded) => {
                for fc in folded.chars() {
                    emit(&mut out, fc);
                }
            }
            None => emit(&mut out, c),
        }
    }
    out
}

/// Full ingestion normalisation for text-book lines: strip HTML, decompose
/// presentation forms, strip attached nikud/cantillation, collapse whitespace.
/// Punctuation is intentionally preserved — it is shown in search results.
pub fn normalize_text_for_indexing(input: &str) -> String {
    fold_strip_collapse(&strip_html_for_indexing(input), true, |_| false)
}

/// Ingestion normalisation for the vocalized field (`textVocalized`): like
/// [`normalize_text_for_indexing`] but KEEPS attached nikud/cantillation.
/// Applied only to lines that carry at least one mark
/// ([`contains_attached_marks`]); the stored copy is what vocalized search
/// results display.
pub fn normalize_vocalized_text_for_indexing(input: &str) -> String {
    fold_strip_collapse(&strip_html_for_indexing(input), false, |_| false)
}

/// Ingestion normalisation for PDF text: like [`normalize_text_for_indexing`]
/// but also drops bidi/zero-width invisibles OCR tends to leave behind
/// ([`is_invisible_char`] — the Dart `_pdfInvisibleChars` set).
pub fn normalize_pdf_text_for_indexing(input: &str) -> String {
    fold_strip_collapse(&strip_html_for_indexing(input), true, is_invisible_char)
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
        push_escaped(&mut out, ch);
    }
    out
}

/// Single-char form of [`escape_regex`], for builders that walk chars.
fn push_escaped(out: &mut String, ch: char) {
    if matches!(
        ch,
        '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
    ) {
        out.push('\\');
    }
    out.push(ch);
}

// ── Insertion-ordered dedup helper ─────────────────────────────────────────

fn push_unique(out: &mut Vec<String>, seen: &mut HashSet<String>, value: String) {
    if seen.insert(value.clone()) {
        out.push(value);
    }
}

// ── התעלמות מגרשיים ────────────────────────────────────────────────────────

/// מסיר גרש/גרשיים (ASCII והצורות העבריות) מהמילה — צורת החיפוש של
/// [`OPT_IGNORE_QUOTES`]. הצורות העבריות מטופלות ליתר ביטחון: הנרמול
/// והטוקניזציה כבר מקפלים אותן ל-ASCII.
pub(crate) fn strip_quote_chars(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\u{05F3}' | '\u{05F4}'))
        .collect()
}

// ── כתיב מלא/חסר (full/partial spelling) ──────────────────────────────────

/// Generates up to `limit` כתיב מלא/חסר variants by rule-based edits of
/// `י`/`ו` (plus the historical geresh/gershayim dropping):
///
/// * **השמטה** — רק י/ו שאינה בקצה המילה; לכל רצף אותיות זהות — השמטה אחת
///   לכל היותר (מצוות→מצות, אבל לא מצת). ברצף שנוגע בקצה מותר להשמיט את
///   האיבר הפנימי (וורד→ורד).
/// * **הוספה** — י או ו בפנים המילה בלבד (לא לפני האות הראשונה ולא אחרי
///   האחרונה), הוספה אחת לכל מרווח — כך שלעולם לא נוצרות שתי אותיות
///   *מוכנסות* צמודות (מצת לא ימצא מצוות), אבל הוספה ליד אות קיימת זהה
///   מותרת (מצות ימצא מצוות).
/// * **תקרה פר-אות** — לכל מילה עד [`MAX_EDITS_PER_LETTER`] עריכות של י
///   ועד כך של ו (בנפרד).
/// * גרש/גרשיים נותרים ניתנים להשמטה בכל מיקום, ללא תקרה (התנהגות קודמת).
///
/// הסדר: המילה המקורית, אחר כך וריאנטים של עריכה אחת (השמטות לפני הוספות,
/// משמאל לימין), אחר כך שתי עריכות, וכן הלאה — כך שקיצוץ תקציב מהסוף משמר
/// תמיד את הצורות הקרובות ביותר למה שהוקלד.
pub(crate) fn generate_spelling_variations(word: &str, limit: usize) -> Vec<String> {
    /// תקרת עריכות לכל אות (י בנפרד, ו בנפרד) בווריאנט אחד.
    const MAX_EDITS_PER_LETTER: usize = 3;
    /// תקרת מועמדי-עריכה למילה — שומרת את חיפוש הקומבינציות פולינומיאלי
    /// גם למילים פתולוגיות; המועמדים שמעבר לה פשוט לא מוצעים.
    const MAX_EDIT_CANDIDATES: usize = 24;

    #[derive(Clone, Copy)]
    enum Edit {
        /// השמטת התו במקום `pos`.
        Del(usize),
        /// הוספת האות לפני התו במקום `gap`.
        Ins(usize, char),
    }

    if word.is_empty() {
        return vec![String::new()];
    }
    if limit == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();

    // מועמדי עריכה בסדר דטרמיניסטי: השמטות משמאל לימין, אחר כך הוספות
    // (לכל מרווח: ו לפני י).
    let mut edits: Vec<Edit> = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        match c {
            'י' | 'ו' => {
                // נציג השמטה אחד לכל רצף: האיבר הפנימי הראשון שלו.
                let run_start = i == 0 || chars[i - 1] != c;
                if run_start {
                    if let Some(del_at) = (i..n)
                        .take_while(|&j| chars[j] == c)
                        .find(|&j| j > 0 && j + 1 < n)
                    {
                        edits.push(Edit::Del(del_at));
                    }
                }
            }
            '\'' | '"' => edits.push(Edit::Del(i)),
            _ => {}
        }
    }
    for gap in 1..n {
        edits.push(Edit::Ins(gap, 'ו'));
        edits.push(Edit::Ins(gap, 'י'));
    }
    edits.truncate(MAX_EDIT_CANDIDATES);
    let m = edits.len();

    // קומבינציה תקפה: שתי אותיות *מוכנסות* לעולם לא יוצאות צמודות בתוצאה —
    // לא באותו מרווח, וגם לא במרווחים שונים שכל התווים ביניהם נמחקו
    // (בוא ↛ בייא) — ועד MAX_EDITS_PER_LETTER עריכות לכל אות.
    let valid = |idx: &[usize]| -> bool {
        let mut yod = 0usize;
        let mut vav = 0usize;
        let mut deleted = [false; 64];
        let mut gaps: Vec<usize> = Vec::new();
        for &i in idx {
            match edits[i] {
                Edit::Del(pos) => {
                    deleted[pos] = true;
                    match chars[pos] {
                        'י' => yod += 1,
                        'ו' => vav += 1,
                        _ => {}
                    }
                }
                Edit::Ins(gap, letter) => {
                    gaps.push(gap);
                    match letter {
                        'י' => yod += 1,
                        _ => vav += 1,
                    }
                }
            }
        }
        if yod > MAX_EDITS_PER_LETTER || vav > MAX_EDITS_PER_LETTER {
            return false;
        }
        // מועמדי ההוספה נוצרו ממוינים לפי מרווח, והקומבינציות שומרות סדר —
        // די לבדוק זוגות שכנים: צמודים אם כל התווים בין המרווחים נמחקו.
        gaps.windows(2).all(|pair| {
            let (g1, g2) = (pair[0], pair[1]);
            g1 != g2 && !(g1..g2).all(|p| deleted[p])
        })
    };

    let apply = |idx: &[usize]| -> String {
        let mut deleted = [false; 64];
        let mut inserts: Vec<(usize, char)> = Vec::new();
        for &i in idx {
            match edits[i] {
                Edit::Del(pos) => deleted[pos] = true,
                Edit::Ins(gap, letter) => inserts.push((gap, letter)),
            }
        }
        let mut out = String::with_capacity(word.len() + inserts.len() * 2);
        for i in 0..=n {
            for &(gap, letter) in &inserts {
                if gap == i {
                    out.push(letter);
                }
            }
            if i < n && !deleted[i] {
                out.push(chars[i]);
            }
        }
        out
    };

    // אורך מילה מעל מערך ה-deleted לא קורה בפועל (מילים בעברית), אבל
    // ליתר ביטחון — נפילה לצורה המקורית בלבד.
    if n >= 64 {
        return vec![word.to_string()];
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    push_unique(&mut out, &mut seen, word.to_string());

    // מעבר על קומבינציות בגודל עולה (k עריכות), בסדר לקסיקוגרפי של
    // אינדקסי המועמדים; עצירה מוקדמת כשמכסת ה-limit מתמלאת.
    let max_k = m.min(2 * MAX_EDITS_PER_LETTER + 2);
    'sizes: for k in 1..=max_k {
        if out.len() >= limit {
            break;
        }
        let mut idx: Vec<usize> = (0..k).collect();
        loop {
            if valid(&idx) {
                push_unique(&mut out, &mut seen, apply(&idx));
                if out.len() >= limit {
                    break 'sizes;
                }
            }
            // הקומבינציה הבאה: האינדקס הימני ביותר שניתן לקדם.
            let mut i = k;
            loop {
                if i == 0 {
                    continue 'sizes;
                }
                i -= 1;
                if idx[i] < m - k + i {
                    idx[i] += 1;
                    for j in i + 1..k {
                        idx[j] = idx[j - 1] + 1;
                    }
                    break;
                }
            }
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

// ── ארמית: שקילות אות סופית + קידומות ─────────────────────────────────────

/// וריאנט השקילות הארמית של האות הסופית: ה↔א (מלכה↔מלכא), ם↔ן
/// (חכמים↔חכמין). `None` כשהמילה לא מסתיימת באחת מהאותיות האלה.
pub(crate) fn aramaic_final_swap(word: &str) -> Option<String> {
    let mut chars: Vec<char> = word.chars().collect();
    let last = chars.last()?;
    let swapped = match last {
        'ה' => 'א',
        'א' => 'ה',
        'ם' => 'ן',
        'ן' => 'ם',
        _ => return None,
    };
    *chars.last_mut().unwrap() = swapped;
    Some(chars.into_iter().collect())
}

/// השורש + וריאנט השקילות הסופית שלו (אם קיים), בסדר הזה.
pub(crate) fn aramaic_root_variants(word: &str) -> Vec<String> {
    let mut out = vec![word.to_string()];
    if let Some(swapped) = aramaic_final_swap(word) {
        out.push(swapped);
    }
    out
}

/// הענף הארמי הבסיסי: קבוצת הקידומות הדקדוקית (שנושאת את הצורות הארמיות
/// ד/דא/א, ולכן גם כד/אד/מד דרך המשבצת השנייה) לפני השורש. זהה ל-
/// [`grammatical_prefix_pattern`] — קיים כשם נפרד כדי שקריאת הקוד של מסלול
/// הארמית לא תתלה בסמנטיקה של "קידומות דקדוקיות".
fn aramaic_prefix_pattern(root: &str) -> String {
    grammatical_prefix_pattern(root)
}

/// כמו [`aramaic_prefix_pattern`] בתוספת סיומות דקדוקיות — משמש כשסומנו
/// גם ארמית וגם "סיומות דקדוקיות", כדי שצורה עם קידומת ארמית *וגם* סיומת
/// (ודמלכתא) לא תיפול בין שני הענפים.
fn aramaic_prefix_suffix_pattern(root: &str) -> String {
    if root.is_empty() {
        return String::new();
    }
    format!(
        "{}{}{}",
        GRAM_PREFIX_GROUP,
        escape_regex(root),
        SUFFIX_PATTERN
    )
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
///
/// אפשרויות הארמית מפוצלות: "סיומות ארמיות" מרחיבה את השורש לווריאנטי
/// השקילות הסופית (מלכה↔מלכא), ו"קידומות ארמיות" עוטפת כל וריאנט בענף
/// הקידומות הארמי (וסיומות דקדוקיות אם סומנו). כשסומנה גם אפשרות הרחבה
/// נוספת (חלון קידומת חופשית, חלק ממילה וכו'), ענף עץ-הבסיס מתלווה לכל
/// וריאנט כדי ששאר האפשרויות ימשיכו לחול גם עליו.
fn word_to_pattern(root: &str, flags: &WordFlags) -> String {
    if root.is_empty() {
        return String::new();
    }
    if !flags.aramaic_prefix && !flags.aramaic_suffix {
        return word_to_pattern_base(root, flags);
    }
    // סיומות ארמיות: השורש + וריאנט השקילות; בלעדיהן — השורש בלבד.
    let variants = if flags.aramaic_suffix {
        aramaic_root_variants(root)
    } else {
        vec![root.to_string()]
    };
    let mut branches: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for variant in &variants {
        if flags.aramaic_prefix {
            let aramaic_builder: fn(&str) -> String = if flags.gram_suffix {
                aramaic_prefix_suffix_pattern
            } else {
                aramaic_prefix_pattern
            };
            let aramaic_branch = if flags.spelling {
                join_spelling(variant, 10, aramaic_builder)
            } else {
                aramaic_builder(variant)
            };
            push_unique(&mut branches, &mut seen, aramaic_branch);
            // הענף הארמי כבר מכסה את הצורה המדויקת (הקידומות אופציונליות),
            // אז ענף הבסיס מתווסף רק כשיש אפשרות הרחבה שהענף לא נושא.
            if flags.expands_besides_aramaic() {
                push_unique(
                    &mut branches,
                    &mut seen,
                    word_to_pattern_base(variant, flags),
                );
            }
        } else {
            // סיומות בלבד: כל וריאנט עובר את עץ-הבסיס המלא (ליטרל כשאין
            // אפשרויות נוספות), כך ששאר האפשרויות חלות על שתי הצורות.
            push_unique(
                &mut branches,
                &mut seen,
                word_to_pattern_base(variant, flags),
            );
        }
    }
    if branches.len() == 1 {
        branches.into_iter().next().unwrap()
    } else {
        format!("(?:{})", branches.join("|"))
    }
}

/// עץ ההחלטות ללא ארמית — הגוף ההיסטורי של [`word_to_pattern`].
fn word_to_pattern_base(root: &str, flags: &WordFlags) -> String {
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

// ── Vocalized (nikud/te'amim) query patterns ───────────────────────────────
//
// The vocalized field (`textVocalized`) indexes surface forms WITH their
// attached marks, so a vocalized query compiles to a whole-term regex over
// that dictionary. The matching contract is "הוקלד → חייב; לא הוקלד →
// חופשי": every typed mark of an enabled class must appear, in order; any
// other attached mark in the term is free. A base letter therefore becomes
// `X[marks]*` and a required mark `m[marks]*` — the free class overlaps the
// required mark, which is harmless under DFA acceptance.

/// Free-marks regex atom: any run of attached marks. The class range also
/// covers the separator punctuation inside U+0591–U+05C7 (maqaf, paseq, sof
/// pasuq, nun hafukha), which can never appear inside a token — the
/// tokenizer breaks on them — so the wider class costs nothing. Includes the
/// general combining range (U+0300–U+036F): the vocalized tokenizer keeps
/// those marks in term text, and they are always free (never a requirement).
pub(crate) const VOC_FREE_MARKS: &str = "[\u{0300}-\u{036F}\u{0591}-\u{05C7}]*";

/// Whole-term pattern for one vocalized query token: typed marks of an
/// enabled class are required in order, everything else is free.
pub(crate) fn vocalized_token_pattern(token: &str, voc: &VocalizedFlags) -> String {
    let mut out = String::with_capacity(token.len() * 4);
    for c in token.chars() {
        if is_general_combining_mark(c) {
            // סימן combining כללי אינו ניקוד ואינו טעם — לעולם לא דרישה;
            // ריצת הסימנים החופשיים שאחרי אות הבסיס כבר מכסה אותו.
            continue;
        }
        if is_attached_mark(c) {
            if voc.requires(c) {
                // Marks are never regex metacharacters — pushed verbatim.
                out.push(c);
                out.push_str(VOC_FREE_MARKS);
            }
            // A typed mark of a disabled class is dropped: the free run
            // already emitted after its base letter covers it.
        } else {
            push_escaped(&mut out, c);
            out.push_str(VOC_FREE_MARKS);
        }
    }
    out
}

/// Mark-free core: matches `base` under ANY vocalization. Used for expansion
/// variants (typo/spelling/lexical forms) whose letters differ from what the
/// user typed — the typed marks no longer have positions to attach to.
pub(crate) fn vocalized_free_pattern(base: &str) -> String {
    vocalized_token_pattern(base, &VocalizedFlags::default())
}

/// Bounded window of `n` letter-units on the vocalized dictionary. Each unit
/// is one char plus its attached marks — a bare `.{{0,n}}` would spend the
/// window on marks (three vocalized letters are 6–9 chars). The unit's `.`
/// can itself consume a mark, over-accepting slightly; the term dictionary
/// constrains what actually matches.
fn voc_window(n: usize) -> String {
    format!("(?:.{VOC_FREE_MARKS}){{0,{n}}}")
}

/// A vocalized root ready for affix composition: its whole-term pattern
/// (marks required or free) plus the mark-free letter count that sizes the
/// affix windows exactly like the plain path sizes them from `root`.
struct VocCore {
    pattern: String,
    base_len: usize,
}

fn voc_user_prefix(core: &VocCore) -> String {
    let window = match core.base_len {
        0 => return String::new(),
        1 => 5,
        2 => 4,
        _ => 3,
    };
    format!("{}{}", voc_window(window), core.pattern)
}

fn voc_user_suffix(core: &VocCore) -> String {
    let window = match core.base_len {
        0 => return String::new(),
        1 => 7,
        2 => 6,
        _ => 5,
    };
    format!("{}{}", core.pattern, voc_window(window))
}

fn voc_partial(core: &VocCore) -> String {
    if core.base_len == 0 {
        return String::new();
    }
    let w = if core.base_len <= 3 { 3 } else { 2 };
    format!("{}{}{}", voc_window(w), core.pattern, voc_window(w))
}

// The grammatical affix groups, as alternative lists. The plain-path string
// constants above stay the source of truth for the mark-free field; these
// arrays exist so the vocalized builders can interleave free-mark runs into
// every alternative, and parity tests assert the mark-free rendering of each
// array equals its string constant — the two forms cannot drift apart.
const GRAM_PREFIX_ALTS_A: &[&str] = &["ו", "מ", "דא", "א", "כש", "כ", "ב", "ש", "ל", "ה", "ד"];
const GRAM_PREFIX_ALTS_B: &[&str] = &["כ", "ב", "ש", "ל", "ה", "ד"];
const PREFIX_ALTS_A: &[&str] = &["ו", "מ", "כ", "ב", "ש", "ל", "ה", "ד"];
const HE_ALTS: &[&str] = &["ה"];
const SUFFIX_ALTS: &[&str] = &[
    "ותי",
    "ותיך",
    "ותיו",
    "ותיה",
    "ותינו",
    "ותיכם",
    "ותיכן",
    "ותיהם",
    "ותיהן",
    "יי",
    "יך",
    "יו",
    "יה",
    "ינו",
    "יכם",
    "יכן",
    "יהם",
    "יהן",
    "י",
    "ך",
    "ו",
    "ה",
    "נו",
    "כם",
    "כן",
    "ם",
    "ן",
    "ים",
    "ות",
];
const FULL_SUFFIX_ALTS: &[&str] = &[
    "ותי",
    "ותיך",
    "ותיו",
    "ותיה",
    "ותינו",
    "ותיכם",
    "ותיכן",
    "ותיהם",
    "ותיהן",
    "יות",
    "יי",
    "יך",
    "יו",
    "יה",
    "יא",
    "תא",
    "ינו",
    "יכם",
    "יכן",
    "יהם",
    "יהן",
    "י",
    "ך",
    "ו",
    "ה",
    "נו",
    "כם",
    "כן",
    "ם",
    "ן",
    "ים",
    "ות",
];

/// `(?:a|b|…)?` over the alternatives; `voc` interleaves free-mark runs so a
/// vocalized affix (וּ, בְּ…) still matches.
fn optional_alts_group(alts: &[&str], voc: bool) -> String {
    let inner: Vec<String> = alts
        .iter()
        .map(|a| {
            if voc {
                vocalized_free_pattern(a)
            } else {
                (*a).to_string()
            }
        })
        .collect();
    format!("(?:{})?", inner.join("|"))
}

fn voc_gram_prefix_group() -> String {
    format!(
        "{}{}{}",
        optional_alts_group(GRAM_PREFIX_ALTS_A, true),
        optional_alts_group(GRAM_PREFIX_ALTS_B, true),
        optional_alts_group(HE_ALTS, true)
    )
}

fn voc_prefix_group() -> String {
    format!(
        "{}{}{}",
        optional_alts_group(PREFIX_ALTS_A, true),
        optional_alts_group(GRAM_PREFIX_ALTS_B, true),
        optional_alts_group(HE_ALTS, true)
    )
}

/// Vocalized counterpart of [`word_to_pattern`]'s morphological decision
/// tree. Spelling is intentionally absent: on the vocalized path spelling
/// variants are generated on the mark-free base and fed through here as
/// their own cores (see [`build_word_regex_vocalized`]).
fn word_to_pattern_vocalized(core: &VocCore, flags: &WordFlags) -> String {
    if core.base_len == 0 {
        return String::new();
    }
    if flags.aramaic_prefix {
        // המראה המנוקדת של זרוע הקידומות הארמיות ב-[`word_to_pattern`]:
        // קבוצת הקידומות הדקדוקית (הנושאת את הצורות הארמיות) סביב ליבת
        // המילה, בתוספת סיומות כשסומנו; וריאנטי השקילות הסופית ("סיומות
        // ארמיות") נכנסים כליבות נפרדות ב-[`build_word_regex_vocalized`]
        // (שינוי אות משמיט את עוגני הסימנים).
        let suffix = if flags.gram_suffix {
            optional_alts_group(SUFFIX_ALTS, true)
        } else {
            String::new()
        };
        let aramaic_branch = format!("{}{}{}", voc_gram_prefix_group(), core.pattern, suffix);
        if !flags.expands_besides_aramaic() {
            return aramaic_branch;
        }
        let base_branch = word_to_pattern_vocalized_base(core, flags);
        if base_branch == aramaic_branch {
            return aramaic_branch;
        }
        return format!("(?:{}|{})", aramaic_branch, base_branch);
    }
    word_to_pattern_vocalized_base(core, flags)
}

/// עץ ההחלטות המנוקד ללא ארמית — הגוף ההיסטורי של
/// [`word_to_pattern_vocalized`].
fn word_to_pattern_vocalized_base(core: &VocCore, flags: &WordFlags) -> String {
    if core.base_len == 0 {
        return String::new();
    }
    if flags.prefix && flags.suffix {
        voc_partial(core)
    } else if flags.gram_prefix && flags.gram_suffix {
        format!(
            "{}{}{}",
            voc_prefix_group(),
            core.pattern,
            optional_alts_group(FULL_SUFFIX_ALTS, true)
        )
    } else if flags.prefix {
        voc_user_prefix(core)
    } else if flags.suffix {
        voc_user_suffix(core)
    } else if flags.gram_prefix {
        format!("{}{}", voc_gram_prefix_group(), core.pattern)
    } else if flags.gram_suffix {
        format!("{}{}", core.pattern, optional_alts_group(SUFFIX_ALTS, true))
    } else if flags.partial {
        voc_partial(core)
    } else {
        core.pattern.clone()
    }
}

/// Vocalized counterpart of [`build_word_regex`]: assembles the branch list
/// for one query word on the vocalized field.
///
/// The branch-order contract carries over — exact candidate forms (typed
/// marks REQUIRED) come first, then spelling variants, then typo variants —
/// so budget truncation always drops approximations before typed intent.
/// Variants are generated on the mark-free base and match under ANY
/// vocalization: altering letters leaves the typed marks without positions.
/// The identity variant is skipped in the variant passes — pass 0 already
/// carries it with its marks required; re-adding it mark-free would silently
/// erase the "הוקלד → חייב" constraint whenever an expansion option is on.
fn build_word_regex_vocalized(
    word: &str,
    flags: &WordFlags,
    alternatives: &[String],
    voc: &VocalizedFlags,
    budget: &VariationBudget,
) -> WordPattern {
    let candidates: Vec<String> = std::iter::once(word.to_string())
        .chain(
            alternatives
                .iter()
                .map(|a| normalize_for_index_vocalized(a)),
        )
        .map(|c| {
            if flags.ignore_quotes {
                strip_quote_chars(&c)
            } else {
                c
            }
        })
        .filter(|c| !strip_attached_marks(c).trim().is_empty())
        .collect();
    let fallback = || WordPattern::Literal(vocalized_token_pattern(word, voc));
    if candidates.is_empty() {
        return fallback();
    }

    let max = flags.max_variations(budget);
    let mut branches: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut full = false;
    let push_core = |branches: &mut Vec<String>, seen: &mut HashSet<String>, core: VocCore| {
        let pattern = word_to_pattern_vocalized(&core, flags);
        push_unique(branches, seen, pattern);
        branches.len() >= max
    };

    // Pass 0 — exact candidates, typed marks required.
    'exact: for candidate in &candidates {
        let base = strip_attached_marks(candidate);
        let core = VocCore {
            pattern: vocalized_token_pattern(candidate, voc),
            base_len: base.chars().count(),
        };
        if push_core(&mut branches, &mut seen, core) {
            full = true;
            break 'exact;
        }
    }
    // Pass 0.5 — סיומות ארמיות: וריאנטי שקילות סופית (ה↔א, ם↔ן) כליבות
    // חסרות-סימנים. מדורג מיד אחרי הצורות המדויקות: זו הסמנטיקה המבוקשת
    // של האפשרות, לא קירוב — עדיפה על וריאנטי כתיב ושגיאות.
    if !full && flags.aramaic_suffix {
        'aramaic: for candidate in &candidates {
            let base = strip_attached_marks(candidate);
            if let Some(variant) = aramaic_final_swap(&base) {
                let core = VocCore {
                    base_len: variant.chars().count(),
                    pattern: vocalized_free_pattern(&variant),
                };
                if push_core(&mut branches, &mut seen, core) {
                    full = true;
                    break 'aramaic;
                }
            }
        }
    }
    // Pass 1 — spelling variants (mark-free).
    if !full && flags.spelling {
        'spelling: for candidate in &candidates {
            let base = strip_attached_marks(candidate);
            for variant in generate_spelling_variations(&base, MAX_SPELLING_BRANCHES) {
                if variant == base {
                    continue;
                }
                let core = VocCore {
                    base_len: variant.chars().count(),
                    pattern: vocalized_free_pattern(&variant),
                };
                if push_core(&mut branches, &mut seen, core) {
                    full = true;
                    break 'spelling;
                }
            }
        }
    }
    // Pass 2 — typo variants (mark-free), lowest priority.
    if !full && flags.typo {
        'typo: for candidate in &candidates {
            let base = strip_attached_marks(candidate);
            for variant in generate_typo_variations(&base, budget.typo_variations) {
                if variant == base {
                    continue;
                }
                let core = VocCore {
                    base_len: variant.chars().count(),
                    pattern: vocalized_free_pattern(&variant),
                };
                if push_core(&mut branches, &mut seen, core) {
                    break 'typo;
                }
            }
        }
    }

    // Char budget (phrase path): identical policy to `build_word_regex`.
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;
    for b in branches {
        if b.trim().is_empty() {
            continue;
        }
        let len = b.chars().count();
        if let Some(max_chars) = budget.max_pattern_chars {
            if !kept.is_empty() && total + len > max_chars {
                break;
            }
        }
        total += len;
        kept.push(b);
    }
    if kept.is_empty() {
        return fallback();
    }
    if let Some(max_chars) = budget.max_pattern_chars {
        let joined_chars = kept.iter().map(|b| b.chars().count()).sum::<usize>()
            + if kept.len() == 1 {
                0
            } else {
                kept.len() - 1 + "(?:)".len()
            };
        if joined_chars > max_chars {
            return fallback();
        }
    }
    if kept.len() == 1 {
        WordPattern::Literal(kept.into_iter().next().unwrap())
    } else {
        WordPattern::Alternation(kept)
    }
}

/// Query-side normalisation for the vocalized field: folds presentation
/// forms and lowercases but KEEPS attached marks — the vocalized analog of
/// [`normalize_for_index`].
pub(crate) fn normalize_for_index_vocalized(text: &str) -> String {
    fold_presentation_forms(text).to_lowercase()
}

// ── Per-word regex assembly ────────────────────────────────────────────────

/// Builds the final tantivy-fst regex term for one query word, taking into
/// account typo tolerance, spelling variants, morphological affixes, and any
/// user-supplied alternative words.
///
/// The result is either a plain escaped word, a single-branch pattern
/// ([`WordPattern::Literal`]), or a structured [`WordPattern::Alternation`]
/// whose branches the engine compiles individually. `budget` selects the
/// caps: the phrase budget keeps the *joined* form (compiled as one DFA by
/// `RegexPhraseQuery`) within the tantivy-fst state limit; the single-word
/// budget has no char cap and looser variation caps because each branch is
/// compiled on its own.
fn build_word_regex(
    word: &str,
    flags: &WordFlags,
    alternatives: &[String],
    budget: &VariationBudget,
) -> WordPattern {
    // The canonical word plus the normalized alternatives, in one filtered
    // pass (alternatives may normalize to empty; the query word never does, but
    // filtering it too keeps the pass total).
    let candidates: Vec<String> = std::iter::once(word.to_string())
        .chain(alternatives.iter().map(|a| normalize_for_index(a)))
        .map(|c| {
            if flags.ignore_quotes {
                strip_quote_chars(&c)
            } else {
                c
            }
        })
        .filter(|c| !c.trim().is_empty())
        .collect();
    if candidates.is_empty() {
        return WordPattern::Literal(escape_regex(word));
    }

    let max = flags.max_variations(budget);
    let mut branches: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Branch order is a contract (see `single_regex_term_query`): collection
    // budgets truncate the branch list from the back, so the exact form of
    // *every* candidate — the query word and each alternative — must come
    // before any typo variant. A legitimate alternative spelling must never
    // lose its slot to a typo variant of the primary word. Pass 0 emits the
    // exact forms; pass 1 (typo only) emits the edit-distance-1 variants,
    // whose regenerated originals dedupe against pass 0 via `seen`.
    'passes: for pass in 0..=usize::from(flags.typo) {
        for candidate in &candidates {
            let roots: Vec<String> = if pass == 0 {
                vec![candidate.clone()]
            } else {
                generate_typo_variations(candidate, budget.typo_variations)
            };
            for root in roots {
                let pattern = word_to_pattern(&root, flags);
                push_unique(&mut branches, &mut seen, pattern);
                if branches.len() >= max {
                    break 'passes;
                }
            }
        }
    }

    // Apply the length budget when one exists (phrase path): keep branches
    // while the cumulative character count stays under it (at least one
    // branch is always kept so the term is never empty).
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;
    for b in branches {
        if b.trim().is_empty() {
            continue;
        }
        let len = b.chars().count();
        if let Some(max_chars) = budget.max_pattern_chars {
            if !kept.is_empty() && total + len > max_chars {
                break;
            }
        }
        total += len;
        kept.push(b);
    }

    if kept.is_empty() {
        return WordPattern::Literal(escape_regex(word));
    }
    // Final safety net (phrase path only): if the joined form — the one
    // `RegexPhraseQuery` compiles as a single DFA — still exceeds the budget
    // (a single oversized branch, or branch totals that pass but overflow
    // once `(?:`/`)`/pipes are added), fall back to a plain literal (always
    // compiles).
    if let Some(max_chars) = budget.max_pattern_chars {
        let joined_chars = kept.iter().map(|b| b.chars().count()).sum::<usize>()
            + if kept.len() == 1 {
                0
            } else {
                kept.len() - 1 + "(?:)".len()
            };
        if joined_chars > max_chars {
            return WordPattern::Literal(escape_regex(word));
        }
    }
    if kept.len() == 1 {
        WordPattern::Literal(kept.into_iter().next().unwrap())
    } else {
        WordPattern::Alternation(kept)
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

/// Resolves the allowed intermediate-word count for every adjacent word pair
/// (index `i` → the gap between words `i` and `i+1`).
///
/// When `custom_spacing` is empty, every gap gets the global `distance`.
/// Otherwise the per-pair value (keyed `"i-(i+1)"`) wins; a missing or
/// unparseable entry falls back to the maximum custom value, and negatives
/// clamp to zero. This mirrors `display_highlight::spacing_for_gaps` (the
/// historical Dart `_highlightSeparatorForIndex`) exactly, so the search
/// admits precisely the occurrences an opened book would highlight.
pub(crate) fn resolve_gaps(
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

// ── Public entry point ─────────────────────────────────────────────────────

/// Builds the regex terms, phrase slop, and max-expansions for an advanced
/// search query. This is the main entry point called by the Tantivy engine
/// layer.
///
/// # Parameters
///
/// - `query` — raw user query string (may contain nikud, mixed case, etc.)
/// - `distance` — default per-pair intermediate-word allowance when
///   `custom_spacing` is empty
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
    prepare_advanced_query_impl(
        query,
        distance,
        custom_spacing,
        alternative_words,
        search_options,
        None,
    )
}

/// Vocalized-field variant of [`prepare_advanced_query`]: the patterns
/// target the `textVocalized` term dictionary — typed marks of the enabled
/// classes are required, all other marks free. The enabled classes are
/// per-word: the global `voc` flags OR-ed with the word's own
/// [`OPT_MATCH_NIKUD`]/[`OPT_MATCH_TAAMIM`] options, so checking "ניקוד" on
/// one word binds only that word's typed marks. `typo_tokens` come back as
/// mark-free bases; the engine expands them against the PLAIN dictionary
/// (edit distance over marked terms would count each mark as an edit) and
/// re-projects the variants onto the vocalized dictionary.
pub fn prepare_advanced_query_vocalized(
    query: &str,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
    voc: &VocalizedFlags,
) -> AdvancedQuery {
    prepare_advanced_query_impl(
        query,
        distance,
        custom_spacing,
        alternative_words,
        search_options,
        Some(voc),
    )
}

fn prepare_advanced_query_impl(
    query: &str,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
    voc: Option<&VocalizedFlags>,
) -> AdvancedQuery {
    // Vocalized queries keep their marks through normalisation and word
    // splitting, so option keys ("{word}_{index}") and per-word patterns are
    // built from the same marked tokens the app derives from the raw query.
    let normalized = match voc {
        Some(_) => normalize_for_index_vocalized(query),
        None => normalize_for_index(query),
    };
    let words = split_query_words(&normalized);

    let has_options =
        !search_options.is_empty() && search_options.values().any(|m| m.values().any(|&v| v));
    let has_alternatives = !alternative_words.is_empty();

    // ── Plain path (no per-word options or alternatives) ──────────────────
    if !has_options && !has_alternatives {
        let terms: Vec<WordPattern> = words
            .iter()
            .map(|w| match voc {
                Some(v) => WordPattern::Literal(vocalized_token_pattern(w, v)),
                None => WordPattern::Literal(escape_regex(w)),
            })
            .collect();
        // A plain vocalized pattern is still an expansion (free marks match
        // every vocalization of the word), so the mark-free literal ceilings
        // (10/100 — sized for ~1 term per word) would truncate legitimate
        // vocalization variants of a common word.
        let max_expansions = match (voc, words.len() > 1) {
            (None, true) => 100,
            (None, false) => 10,
            (Some(_), true) => PHRASE_MAX_EXPANSIONS,
            (Some(_), false) => VOC_SINGLE_WORD_MAX_EXPANSIONS,
        };
        return AdvancedQuery {
            regex_terms: terms,
            gaps: resolve_gaps(custom_spacing, distance, words.len()),
            max_expansions,
            typo_tokens: Vec::new(),
            words,
        };
    }

    // ── Advanced path ─────────────────────────────────────────────────────
    // A single word is executed per branch (TermSetQuery path) and gets the
    // relaxed budget; a phrase compiles each word's joined pattern as one DFA
    // and must keep the tight caps (R2 — relaxing them here would make the
    // same word work alone but crash inside a phrase).
    let budget = if words.len() == 1 {
        &SINGLE_WORD_BUDGET
    } else {
        &PHRASE_BUDGET
    };

    // A single word whose only expansion option is typo tolerance skips the
    // literal edit-distance-1 variants entirely: the engine scans the term
    // dictionary once per candidate with a Levenshtein-1 automaton instead —
    // every substitution/deletion/insertion/transposition, not the sampled
    // Hebrew confusion list, for ~1/128 of the scan work. The coverage is
    // still subject to the collection budgets, and typo sits at the lowest
    // priority: when the exact forms alone exhaust a budget (an extremely
    // common word), the typo scan is skipped rather than pushing past it.
    // Typo combined with morphology/spelling keeps the literal-variant path,
    // which composes each variant through the pattern builder (an automaton
    // can't).
    let first_flags = word_flags_at(&words, 0, search_options);
    let typo_tokens: Vec<String> =
        if words.len() == 1 && first_flags.typo && !first_flags.expands_beyond_typo() {
            // Vocalized mode hands the engine mark-free bases: the Levenshtein
            // scan runs against the PLAIN dictionary (each mark would count as
            // an edit against marked terms) and the variants are re-projected
            // onto the vocalized dictionary as free-mark patterns.
            std::iter::once(match voc {
                Some(_) => strip_attached_marks(&words[0]),
                None => words[0].clone(),
            })
            .chain(
                alternative_words
                    .get(&0)
                    .into_iter()
                    .flatten()
                    .map(|a| normalize_for_index(a)),
            )
            .map(|c| {
                // התעלמות מגרשיים חלה גם על מסלול אוטומט-הטעויות: הסריקה רצה
                // על הצורה הנקייה (שקיימת באינדקס לכל מילה עם גרשיים).
                if first_flags.ignore_quotes {
                    strip_quote_chars(&c)
                } else {
                    c
                }
            })
            .filter(|c| !c.trim().is_empty())
            .collect()
        } else {
            Vec::new()
        };

    let regex_terms: Vec<WordPattern> = words
        .iter()
        .enumerate()
        .map(|(i, word)| {
            let mut flags = word_flags_at(&words, i, search_options);
            if !typo_tokens.is_empty() {
                // The Levenshtein automaton supplies the typo coverage; the
                // branches keep only the exact candidate forms.
                flags.typo = false;
            }
            let alts = alternative_words
                .get(&(i as u32))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            match voc {
                // דגלי המילה: הדגלים הגלובליים של ה-API מאוחדים עם אפשרויות
                // ה"ניקוד"/"טעמים" שסומנו למילה הזו — כך שסימון פר-מילה מחייב
                // רק את הסימנים שהוקלדו באותה מילה.
                Some(v) => {
                    let word_voc = v.or(VocalizedFlags::new(flags.nikud, flags.taamim));
                    build_word_regex_vocalized(word, &flags, alts, &word_voc, budget)
                }
                None => build_word_regex(word, &flags, alts, budget),
            }
        })
        .collect();

    let max_expansions = compute_max_expansions(&words, search_options, voc.is_some());

    AdvancedQuery {
        regex_terms,
        gaps: resolve_gaps(custom_spacing, distance, words.len()),
        max_expansions,
        typo_tokens,
        words,
    }
}

// ── max_expansions heuristic ───────────────────────────────────────────────

/// Computes the term-expansion ceiling for the query, shaped by path:
///
/// - **Single word** (TermSetQuery path): overflow *degrades* — collection
///   truncates at the ceiling and serves the highest-priority branches — and
///   the real cost guard is the postings budget in
///   `single_regex_term_query`. The ceiling only bounds the materialized
///   `Vec<Term>` memory, so it sits ×10 above the historical error-guard
///   values. A single word combining typo with a morphological/partial
///   option uses the (much higher) morphological ceilings — its relaxed
///   branch set legitimately matches many terms.
/// - **Phrase** (`RegexPhraseQuery`): tantivy enforces the ceiling itself,
///   cumulatively across positions, and overflow is an error — one flat
///   ceiling at half the tantivy default (see [`PHRASE_MAX_EXPANSIONS`]).
fn compute_max_expansions(
    words: &[String],
    search_options: &HashMap<String, HashMap<String, bool>>,
    vocalized: bool,
) -> u32 {
    // בשדה המנוקד כל תבנית היא הרחבה — ריצות הסימנים החופשיים מתאימות לכל
    // ניקוד של המילה — ולכן תקרות המסלול הרגיל (שמכוילות ל"טרם אחד למילה",
    // כמו 100/1024) היו חונקות וריאציות ניקוד לגיטימיות של מילה נפוצה.
    // רצפת תקרות המסלול המנוקד חלה לפני החישוב הרגיל.
    if vocalized {
        return if words.len() == 1 {
            plain_max_expansions(words, search_options).max(VOC_SINGLE_WORD_MAX_EXPANSIONS)
        } else {
            PHRASE_MAX_EXPANSIONS
        };
    }
    plain_max_expansions(words, search_options)
}

fn plain_max_expansions(
    words: &[String],
    search_options: &HashMap<String, HashMap<String, bool>>,
) -> u32 {
    let single = words.len() == 1;
    let has_typo = search_options
        .values()
        .any(|m| m.get(OPT_TYPO).copied().unwrap_or(false));

    // Check whether any word uses a morphological or partial option, and find
    // the shortest such word (wider expansion for shorter roots).
    let morph_keys = [
        OPT_PREFIX,
        OPT_SUFFIX,
        OPT_GRAM_PREFIX,
        OPT_GRAM_SUFFIX,
        OPT_PARTIAL,
        OPT_ARAMAIC_PREFIX,
        OPT_ARAMAIC_SUFFIX,
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

    // Phrase path: one flat ceiling for every expansion-heavy shape (typo
    // and/or morphology). The per-word shape no longer matters — tantivy
    // enforces the ceiling cumulatively across positions, and its bucketing
    // keeps the cost of what passes contained.
    if !single && (has_typo || shortest_morph.is_some()) {
        return PHRASE_MAX_EXPANSIONS;
    }

    if let Some(shortest) = shortest_morph {
        // Single word: executed per branch as a TermSetQuery, where overflow
        // degrades (truncates) instead of erroring and the real cost guard is
        // the postings budget in `single_regex_term_query`. These ceilings
        // only bound the materialized `Vec<Term>` memory, so they sit ×10
        // above the old error-guard values.
        return match shortest {
            0 | 1 => 20_000,
            2 => 30_000,
            3 => 40_000,
            _ => 50_000,
        };
    }

    if has_typo {
        // Single word, typo without any morphological option: the branch set
        // is edit-distance-1 literals, which match few terms each.
        500
    } else if !single {
        // Spelling/alternative literal branches only — each matches at most a
        // handful of terms per position, but positions accumulate.
        1_024
    } else {
        100
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

    #[test]
    fn sanitize_folds_typographic_quotes() {
        // הצורות הטיפוגרפיות (Word/OCR) מתקפלות ל-ASCII — שתי צורות הכיוון
        // משמשות לסירוגין ברינדור RTL, לכן כל הארבע מקופלות.
        assert_eq!(sanitize_query("רמח\u{201D}ל"), "רמח\"ל");
        assert_eq!(sanitize_query("רמח\u{201C}ל"), "רמח\"ל");
        assert_eq!(sanitize_query("תוס\u{2019}"), "תוס'");
        assert_eq!(sanitize_query("תוס\u{2018}"), "תוס'");
    }

    #[test]
    fn sanitize_breaking_punctuation_becomes_space() {
        // פיסוק ההפסקה ו-`|` הם מפרידים (כמו בטוקנייזר) — רווח, לא מחיקה:
        // `שלום,עולם` חייב להתפצל לשתי מילים, לא להידבק.
        assert_eq!(sanitize_query("א|ב"), "א ב");
        assert_eq!(sanitize_query("שלום,עולם"), "שלום עולם");
        assert_eq!(sanitize_query("רעהו,10"), "רעהו 10");
        assert_eq!(sanitize_query("איפא:5"), "איפא 5");
        assert_eq!(sanitize_query("א;ב!ג?ד(ה)ו{ז}"), "א ב ג ד ה ו ז");
        // נקודה ו-[] נשארים שקופים — נמחקים בלי לפצל.
        assert_eq!(sanitize_query("פ.ב.י"), "פבי");
        assert_eq!(sanitize_query("יב[ע]ר"), "יבער");
        assert_eq!(sanitize_query("3.14"), "314");
    }

    #[test]
    fn sanitize_drops_invisible_chars() {
        // תווים בלתי-נראים (bidi/zero-width/BOM) נמחקים — מדביקים, לא
        // מפצלים — עקבי עם מסלול ה-PDF ועם שקיפותם בטוקנייזר.
        assert_eq!(sanitize_query("לה\u{FEFF}תיר"), "להתיר");
        assert_eq!(sanitize_query("שלום\u{200F}עולם"), "שלוםעולם");
        assert_eq!(sanitize_query("א\u{202B}ב\u{202C}ג"), "אבג");
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
    fn strip_html_breaking_tags_become_a_space() {
        // Otzaria/otzaria#949 — מעבר שורה שנמחק בלי רווח הדביק מילים.
        assert_eq!(strip_html_for_indexing("המורים<br>כי"), "המורים כי");
        assert_eq!(strip_html_for_indexing("המורים<br/>כי"), "המורים כי");
        assert_eq!(strip_html_for_indexing("המורים<BR />כי"), "המורים כי");
        assert_eq!(strip_html_for_indexing("א</p><p>ב"), "א  ב");
        assert_eq!(strip_html_for_indexing("<td>א</td><td>ב</td>"), " א  ב ");
        assert_eq!(
            strip_html_for_indexing("<h2 class=\"x\">כותרת</h2>גוף"),
            " כותרת גוף"
        );
        // תגי inline נשארים מחיקה נטו — מילה שפוצלה בעיצוב היא מילה אחת.
        assert_eq!(strip_html_for_indexing("מי<b>לה"), "מילה");
        assert_eq!(strip_html_for_indexing("מי<big>לה"), "מילה");
        // שם תג ששבירה היא רק קידומת שלו אינו תג שבירה.
        assert_eq!(strip_html_for_indexing("א<param>ב"), "אב");
        assert_eq!(strip_html_for_indexing("א<h7>ב"), "אב");
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
    fn normalize_for_index_keeps_separators() {
        // רגרסיה: strip_nikud היה מוחק גם את המקף/סוף-פסוק שבטווח הניקוד,
        // ומדביק מילים לפני שהפיצול הספיק לראות את המפריד.
        assert_eq!(normalize_for_index("אֲשֶׁר־שָׁמַע"), "אשר־שמע");
        assert_eq!(
            normalize_for_index("בָּרָא\u{05C3} וְהָאָרֶץ"),
            "ברא\u{05C3} והארץ"
        );
        assert_eq!(normalize_for_index("מ\u{FB1D}ם"), "מים");
        assert_eq!(normalize_for_index("Torah"), "torah");
    }

    #[test]
    fn normalize_for_index_strips_general_combining_marks() {
        // תעתיק ערבית-יהודית: הנקודה העילית (U+0307) מוסרת מצורת המילון —
        // כמו ניקוד — כך ש`כלת̇ום` ו-`כלתום` ממופים לאותו טרם.
        assert_eq!(normalize_for_index("כלת\u{0307}ום"), "כלתום");
        assert_eq!(normalize_for_index("מצ\u{0307}ארע"), "מצארע");
        // varika (U+FB1E) נבלע דרך קיפול ה-Presentation Forms.
        assert_eq!(normalize_for_index("א\u{FB1E}ב"), "אב");
    }

    #[test]
    fn general_combining_marks_are_word_marks_not_attached() {
        for cp in 0x0300u32..=0x036F {
            let c = char::from_u32(cp).unwrap();
            assert!(is_general_combining_mark(c) && is_word_mark(c));
            // לא ניקוד ולא טעם — לעולם לא דרישת-התאמה בחיפוש מנוקד.
            assert!(!is_attached_mark(c) && !is_nikud_mark(c) && !is_taam_mark(c));
        }
        assert!(!is_general_combining_mark('א') && !is_general_combining_mark('\u{05B8}'));
    }

    // ── חיפוש מנוקד ─────────────────────────────────────────────────────

    #[test]
    fn mark_classes_partition_attached_marks() {
        // כל סימן צמוד הוא או ניקוד או טעם — לעולם לא שניהם ולא אף אחד.
        for cp in 0x0591u32..=0x05C7 {
            let c = char::from_u32(cp).unwrap();
            if is_attached_mark(c) {
                assert!(
                    is_nikud_mark(c) ^ is_taam_mark(c),
                    "U+{cp:04X} must be exactly one class"
                );
            } else {
                assert!(!is_nikud_mark(c) && !is_taam_mark(c));
            }
        }
        // דוגמאות עוגן: קמץ=ניקוד, דגש=ניקוד, מונח=טעם, מתג=טעם.
        assert!(is_nikud_mark('\u{05B8}'));
        assert!(is_nikud_mark('\u{05BC}'));
        assert!(is_taam_mark('\u{05A3}'));
        assert!(is_taam_mark('\u{05BD}'));
    }

    #[test]
    fn normalize_vocalized_keeps_marks_strips_html() {
        assert_eq!(
            normalize_vocalized_text_for_indexing("<b>בְּרֵאשִׁית</b>  בָּרָא"),
            "בְּרֵאשִׁית בָּרָא"
        );
        // Presentation form מתפרק והסימן נשמר (בניגוד לנרמול הרגיל).
        assert_eq!(
            normalize_vocalized_text_for_indexing("מ\u{FB1D}ם"),
            "מ\u{05D9}\u{05B4}ם"
        );
        assert!(contains_attached_marks("בָּרָא"));
        assert!(!contains_attached_marks("ברא"));
    }

    #[test]
    fn vocalized_token_pattern_combining_marks_never_required() {
        // combining כללי שהוקלד אינו דרישה — גם כששתי המחלקות דלוקות:
        // התבנית זהה לזו של המילה הנקייה, והריצה החופשית (שכוללת את
        // U+0300–036F) מכסה אותו בטרמים מנוקדים.
        let all = VocalizedFlags::new(true, true);
        assert_eq!(
            vocalized_token_pattern("כלת\u{0307}ום", &all),
            vocalized_token_pattern("כלתום", &all)
        );
        assert!(VOC_FREE_MARKS.contains("\u{0300}"));
        // תבנית של מילה מנוקדת עדיין מקמפלת ב-tantivy-fst.
        tantivy_fst::Regex::new(&vocalized_token_pattern("בָּרָ\u{0307}א", &all)).unwrap();
    }

    #[test]
    fn vocalized_token_pattern_requires_typed_frees_untyped() {
        let nikud_only = VocalizedFlags::new(true, false);
        // בָרָא: הקמצים נדרשים, אחרי כל אות ריצת-סימנים חופשית.
        let pat = vocalized_token_pattern("בָרָא", &nikud_only);
        assert_eq!(
            pat,
            format!("ב{m}\u{05B8}{m}ר{m}\u{05B8}{m}א{m}", m = VOC_FREE_MARKS)
        );
        let re = tantivy_fst::Regex::new(&pat).unwrap();
        use tantivy_fst::Automaton;
        let accepts = |re: &tantivy_fst::Regex, s: &str| {
            let mut state = re.start();
            for &b in s.as_bytes() {
                state = re.accept(&state, b);
            }
            re.is_match(&state)
        };
        // דגש שלא הוקלד — חופשי; טעם שלא הוקלד — חופשי.
        assert!(accepts(&re, "בָּרָא"));
        assert!(accepts(&re, "בָּרָ\u{05A3}א"));
        assert!(accepts(&re, "בָרָא"));
        // תנועה אחרת במקום קמץ — נפסל; חסר סימן נדרש — נפסל.
        assert!(!accepts(&re, "בְּרֹא"));
        assert!(!accepts(&re, "ברא"));
    }

    #[test]
    fn vocalized_token_pattern_taamim_class_split() {
        // טעם שהוקלד כשדגל הטעמים כבוי — נמחק (חופשי); כשהוא דלוק — נדרש.
        let word = "וַיֹּ\u{05A3}אמֶר";
        let nikud_only = VocalizedFlags::new(true, false);
        let both = VocalizedFlags::new(true, true);
        assert!(!vocalized_token_pattern(word, &nikud_only).contains('\u{05A3}'));
        assert!(vocalized_token_pattern(word, &both).contains('\u{05A3}'));
    }

    #[test]
    fn alt_groups_match_plain_constants() {
        // חוזה ה-parity: הרינדור נטול-הסימנים של מערכי החלופות חייב להיות
        // זהה בייטים לקבועי המחרוזת של המסלול הרגיל.
        assert_eq!(
            format!(
                "{}{}{}",
                optional_alts_group(GRAM_PREFIX_ALTS_A, false),
                optional_alts_group(GRAM_PREFIX_ALTS_B, false),
                optional_alts_group(HE_ALTS, false)
            ),
            GRAM_PREFIX_GROUP
        );
        assert_eq!(
            format!(
                "{}{}{}",
                optional_alts_group(PREFIX_ALTS_A, false),
                optional_alts_group(GRAM_PREFIX_ALTS_B, false),
                optional_alts_group(HE_ALTS, false)
            ),
            PREFIX_GROUP
        );
        assert_eq!(optional_alts_group(SUFFIX_ALTS, false), SUFFIX_PATTERN);
        assert_eq!(
            optional_alts_group(FULL_SUFFIX_ALTS, false),
            FULL_SUFFIX_PATTERN
        );
    }

    #[test]
    fn prepare_vocalized_plain_path_builds_required_mark_patterns() {
        let voc = VocalizedFlags::new(true, false);
        let q = prepare_advanced_query_vocalized(
            "בָּרָא אֱלֹהִים",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &voc,
        );
        assert_eq!(q.regex_terms.len(), 2);
        for term in &q.regex_terms {
            let joined = term.joined();
            assert!(joined.contains(VOC_FREE_MARKS));
            // כל branch חייב להתקמפל.
            for b in term.branches() {
                tantivy_fst::Regex::new(b).unwrap();
            }
        }
        assert!(q.typo_tokens.is_empty());
    }

    #[test]
    fn prepare_vocalized_single_typo_word_yields_stripped_typo_token() {
        let voc = VocalizedFlags::new(true, false);
        let options = make_options(&[("בָּרָא_0", &[(OPT_TYPO, true)])]);
        let q = prepare_advanced_query_vocalized(
            "בָּרָא",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &options,
            &voc,
        );
        // הטוקן לסריקת ה-Levenshtein חוזר נטול סימנים (נסרק מול המילון הרגיל).
        assert_eq!(q.typo_tokens, vec!["ברא".to_string()]);
        // וה-branch המדויק עדיין דורש את הסימנים שהוקלדו.
        assert!(q.regex_terms[0].joined().contains('\u{05B8}'));
    }

    #[test]
    fn vocalized_variant_passes_skip_identity() {
        // עם כתיב מלא/חסר: branch ראשון דורש סימנים; אף branch חופשי לא
        // חוזר על אותיות הבסיס עצמן (זה היה מרוקן את דרישת הסימנים).
        let voc = VocalizedFlags::new(true, false);
        let flags = WordFlags {
            spelling: true,
            ..WordFlags::default()
        };
        let pattern = build_word_regex_vocalized("שָׁלוֹם", &flags, &[], &voc, &SINGLE_WORD_BUDGET);
        let branches = pattern.branches();
        assert!(branches[0].contains('\u{05B8}'));
        let identity_free = vocalized_free_pattern("שלום");
        assert!(!branches[1..].contains(&identity_free));
        for b in branches {
            tantivy_fst::Regex::new(b).unwrap();
        }
    }

    // ── אפשרויות "ניקוד"/"טעמים" פר-מילה ───────────────────────────────

    #[test]
    fn options_vocalized_flags_scans_all_words() {
        assert_eq!(
            options_vocalized_flags(&HashMap::new()),
            VocalizedFlags::default()
        );
        let opts = make_options(&[
            ("שלום_0", &[(OPT_TYPO, true)]),
            ("עולם_1", &[(OPT_MATCH_TAAMIM, true)]),
        ]);
        assert_eq!(
            options_vocalized_flags(&opts),
            VocalizedFlags::new(false, true)
        );
        let opts = make_options(&[("שלום_0", &[(OPT_MATCH_NIKUD, true)])]);
        assert_eq!(
            options_vocalized_flags(&opts),
            VocalizedFlags::new(true, false)
        );
    }

    #[test]
    fn per_word_nikud_option_binds_only_that_word() {
        // "ניקוד" מסומן רק למילה הראשונה: הסימנים שהוקלדו בה נדרשים,
        // ואילו סימני המילה השנייה נמחקים מהתבנית (חופשיים).
        let opts = make_options(&[("בָּרָא_0", &[(OPT_MATCH_NIKUD, true)])]);
        let q = prepare_advanced_query_vocalized(
            "בָּרָא שָׁלוֹם",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &opts,
            &VocalizedFlags::default(),
        );
        assert_eq!(q.regex_terms.len(), 2);
        let w0 = q.regex_terms[0].joined();
        assert!(
            w0.contains('\u{05B8}'),
            "typed kamatz must be required: {w0}"
        );
        let w1 = q.regex_terms[1].joined();
        assert!(
            !w1.contains('\u{05B8}') && !w1.contains('\u{05C1}'),
            "unflagged word keeps its marks free: {w1}"
        );
        // מצב מנוקד ⇒ תקרת ההרחבה של מסלול הביטוי המנוקד, לא 1024 הרגילה.
        assert_eq!(q.max_expansions, PHRASE_MAX_EXPANSIONS);
    }

    #[test]
    fn per_word_option_unions_with_global_flags() {
        // דגל גלובלי (ניקוד) + אפשרות פר-מילה (טעמים) על המילה הראשונה:
        // במילה הראשונה שתי המחלקות מחייבות, בשנייה רק הגלובלית.
        let word = "בְּרֵאשִׁ֖ית"; // שווא/צירה/חיריק + טעם (טיפחא U+0596)
        let key = format!("{word}_0");
        let opts = make_options(&[(key.as_str(), &[(OPT_MATCH_TAAMIM, true)])]);
        let q = prepare_advanced_query_vocalized(
            &format!("{word} בָּרָ֣א"),
            0,
            &HashMap::new(),
            &HashMap::new(),
            &opts,
            &VocalizedFlags::new(true, false),
        );
        let w0 = q.regex_terms[0].joined();
        assert!(w0.contains('\u{05B0}'), "global nikud binds word 0: {w0}");
        assert!(
            w0.contains('\u{0596}'),
            "per-word taamim binds word 0: {w0}"
        );
        let w1 = q.regex_terms[1].joined();
        assert!(
            w1.contains('\u{05B8}'),
            "global nikud still binds word 1: {w1}"
        );
        assert!(
            !w1.contains('\u{05A3}'),
            "munach stays free on word 1: {w1}"
        );
        for term in &q.regex_terms {
            for b in term.branches() {
                tantivy_fst::Regex::new(b).unwrap();
            }
        }
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
        let mut analyzer =
            TextAnalyzer::builder(crate::hebrew_tokenizer::HebrewTokenizer::default())
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
            // גרשיים טיפוגרפיים (Word/OCR), פיסוק דבוק, תעתיק עם combining
            // ותווים בלתי-נראים — דפוסי הקורפוס שהובילו לכללים החדשים.
            "אמר רמח\u{201D}ל ותוס\u{2019} על שד\u{201C}ל",
            "וְאָהַבְתָּ לְרֵעֲךָ,10 כָּמוֹךָ",
            "שלום,עולם איפא:5 א{ב}ג",
            "כלת\u{0307}ום ומצ\u{0307}ארע בתעתיק",
            "לה\u{FEFF}תיר ושלום\u{200F}עולם",
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
        // תואם את HebrewTokenizer: גרשיים וגרש בין אותיות הם חלק מהמילה;
        // גרש סוגר נבלע; ״/׳ עבריים מנורמלים ללועזיים לפני הפיצול.
        assert_eq!(split_query_words("שלום עולם"), vec!["שלום", "עולם"]);
        assert_eq!(split_query_words("תוס'"), vec!["תוס'"]);
        assert_eq!(split_query_words("ז\"ל"), vec!["ז\"ל"]);
        assert_eq!(split_query_words("ד'אש"), vec!["ד'אש"]);
        assert_eq!(split_query_words("ג'ורג'"), vec!["ג'ורג'"]);
        assert_eq!(split_query_words("רמב״ם"), vec!["רמב\"ם"]);
        assert_eq!(split_query_words("א\"ב\"ג"), vec!["א\"ב\"ג"]);
        assert_eq!(
            split_query_words("הרב פלוני ז\"ל"),
            vec!["הרב", "פלוני", "ז\"ל"]
        );
        // גרשיים בסוף מילה או בתחילתה אינם חלק מהטוקן.
        assert_eq!(split_query_words("רמב\""), vec!["רמב"]);
        assert_eq!(split_query_words("\"רמב"), vec!["רמב"]);
        assert_eq!(split_query_words("רמב\"\"ם"), vec!["רמב", "ם"]);
        // זוג גרשים בין אותיות מאוחד לגרשיים (D2); בסוף מילה — נבלע
        // רק הראשון, כגרש סוגר.
        assert_eq!(split_query_words("רמב''ם"), vec!["רמב\"ם"]);
        assert_eq!(split_query_words("וכו''"), vec!["וכו'"]);
    }

    #[test]
    fn split_query_words_matches_index_analyzer() {
        // טסט החוזה שכל המערכת תלויה בו: המסלול המתקדם וההדגשות מפצלים
        // ב-split_query_words, בעוד המדויק/המקורב עוברים דרך ה-analyzer
        // החי — כל סטייה ביניהם היא טרם שלעולם לא יימצא.
        let samples = [
            "רמב\"ם",
            "רמב״ם",
            "רמב''ם",
            "ז\"ל אמר",
            "ג'ורג' וד'אש",
            "תוס' ד\"ה",
            "תוס׳ ד״ה",
            "א\"ב\"ג",
            "\"רמב",
            "רמב\"",
            "רמב\"\"ם",
            "וכו''",
            "הרב פלוני ז״ל; ועי' תוס׳!",
            "שאלה: מה הדין? (עא:) וצ\"ע",
            // ללא ניקוד: הסרת הסימנים מטקסט הטוקן היא נרמול של ה-analyzer
            // שאינו חלק מחוזה גבולות-הפיצול.
            "אשר־שמע אל־משה בית-דין",
            "3.14 סי' קכ\"ה ס\"ק ז'",
            "ה\"מגיד משנה\" כתב",
            // הכללים החדשים: `|` ופיסוק דבוק מפרידים בשני המסלולים,
            // צורות טיפוגרפיות מתקפלות, בלתי-נראים מדביקים.
            "א|ב",
            "שלום,עולם רעהו,10 איפא:5",
            "רמח\u{201D}ל תוס\u{2019} שד\u{201C}ל",
            "אמר \u{201C}שלום\u{201D} לכולם",
            "יב[ע]ר פ.ב.י (עא:) {שם}",
            "לה\u{FEFF}תיר שלום\u{200F}עולם",
        ];
        for s in samples {
            assert_eq!(
                split_query_words(s),
                analyzer_terms(s),
                "split/tokenizer drift for: {s}"
            );
        }
    }

    #[test]
    fn property_split_query_words_matches_index_analyzer() {
        // הרחבת ה-property של טסט הדוגמאות: לכל קלט אקראי (זרע קבוע —
        // דטרמיניסטי) משני הצדדים יוצאים אותם טרמים בדיוק. האלפבית מכסה
        // אותיות, ספרות, כל צורות הגרשים, מפרידים, שקופים ובלתי-נראים —
        // אך לא ניקוד/Presentation Forms: שם ה-analyzer מנרמל את טקסט
        // הטוקן (הסרת סימנים/קיפול) בעוד split_query_words משמר בכוונה
        // (המסלול המנוקד צריך את הסימנים) — הגבולות זהים, הטקסט לא.
        const ALPHABET: &[char] = &[
            'א', 'ב', 'ג', 'ש', 'ת', 'ם', 'ן', '3', '7', '\'', '"', '\u{05F3}', '\u{05F4}',
            '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', ' ', ' ', '-', '\u{05BE}', '|', ',',
            ';', ':', '!', '?', '(', ')', '{', '}', '\u{05C0}', '\u{05C3}', '.', '[', ']', '*',
            '+', '~', '`', '^', '$', '\\', '\u{200F}', '\u{FEFF}', '\u{202B}',
        ];
        fn xorshift(state: &mut u64) -> u64 {
            let mut x = *state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *state = x;
            x
        }
        let mut state = 0xDEAD_BEEF_5EED_u64;
        for _ in 0..2_000 {
            let len = (xorshift(&mut state) as usize) % 24;
            let s: String = (0..len)
                .map(|_| ALPHABET[(xorshift(&mut state) as usize) % ALPHABET.len()])
                .collect();
            assert_eq!(
                split_query_words(&s),
                analyzer_terms(&s),
                "split/tokenizer drift for: {s:?}"
            );
        }
    }

    #[test]
    fn split_breaks_on_hebrew_separators_like_tokenizer() {
        // סוף-פסוק, פסק ונו"ן הפוכה שוברים טוקן — כמו ב-HebrewTokenizer
        // (test_separators_still_split). בלעדי זה `ברא׃` נשאר טוקן אחד
        // שאינו קיים במילון האינדקס.
        assert_eq!(split_query_words("ברא\u{05C3} והארץ"), vec!["ברא", "והארץ"]);
        assert_eq!(split_query_words("ברא\u{05C3}והארץ"), vec!["ברא", "והארץ"]);
        assert_eq!(split_query_words("א\u{05C0}ב"), vec!["א", "ב"]);
        assert_eq!(split_query_words("א\u{05C6}ב"), vec!["א", "ב"]);
        // ניקוד וטעמים צמודים עדיין אינם שוברים מילה.
        assert_eq!(split_query_words("שָׁמַ֣ע"), vec!["שָׁמַ֣ע"]);
    }

    #[test]
    fn prepare_advanced_query_breaks_sof_pasuq_like_index() {
        // המסלול המלא: שאילתה עם סוף-פסוק מפיקה שני טרמים שקיימים באינדקס,
        // לא טרם דבוק `ברא׃` שלעולם לא יתאים.
        let q = prepare_advanced_query(
            "ברא\u{05C3} והארץ",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(q.regex_terms.len(), 2);
        assert_eq!(q.regex_terms[0].joined(), "ברא");
        assert_eq!(q.regex_terms[1].joined(), "והארץ");
    }

    // ── generate_spelling_variations ────────────────────────────────────

    #[test]
    fn spelling_variations_original_first_then_single_edits() {
        let v = generate_spelling_variations("בוא", 100);
        // המקורית תמיד ראשונה, ההשמטה לפני ההוספות.
        assert_eq!(v[0], "בוא");
        assert_eq!(v[1], "בא");
        // הוספות פנימיות (חסר→מלא) קיימות.
        assert!(v.contains(&"בווא".to_string()));
        // limit==1 משאיר רק את המקורית — המנייה נעצרת מוקדם.
        assert_eq!(generate_spelling_variations("בוא", 1), vec!["בוא"]);
    }

    #[test]
    fn spelling_variations_follow_the_male_haser_rules() {
        // מצות ↔ מצוות: הוספת ו ליד ו קיימת מותרת, וההשמטה ההפוכה מותרת.
        assert!(generate_spelling_variations("מצות", 100).contains(&"מצוות".to_string()));
        assert!(generate_spelling_variations("מצוות", 100).contains(&"מצות".to_string()));
        // מצת ↛ מצוות: שתי אותיות מוכנסות צמודות אסורות; ההשמטה הכפולה
        // ההפוכה (מצוות ↛ מצת) אסורה גם היא — השמטה אחת לכל רצף.
        assert!(!generate_spelling_variations("מצת", 1000).contains(&"מצוות".to_string()));
        assert!(!generate_spelling_variations("מצוות", 1000).contains(&"מצת".to_string()));
        // השמטה+שתי הוספות שהופכות צמודות אחרי המחיקה אסורות (בוא ↛ בייא).
        assert!(!generate_spelling_variations("בוא", 1000).contains(&"בייא".to_string()));
        // אות בקצה המילה אינה נמחקת: אותו ↛ אות, ויהי שומרת את ה-ו הפותחת.
        assert!(!generate_spelling_variations("אותו", 1000).contains(&"אות".to_string()));
        assert!(!generate_spelling_variations("ויהי", 1000)
            .iter()
            .any(|v| !v.starts_with('ו')));
        // ברצף שנוגע בקצה מותר להשמיט את האיבר הפנימי (וורד→ורד).
        assert!(generate_spelling_variations("וורד", 100).contains(&"ורד".to_string()));
        // אין הוספה לפני האות הראשונה או אחרי האחרונה.
        assert!(!generate_spelling_variations("גמל", 1000)
            .iter()
            .any(|v| v.starts_with('ו')
                || v.starts_with('י')
                || v.ends_with('ו')
                || v.ends_with('י')));
        // גרשיים עדיין ניתנים להשמטה בכל מיקום.
        assert!(generate_spelling_variations("רמב\"ם", 100).contains(&"רמבם".to_string()));
        assert!(generate_spelling_variations("תוס'", 100).contains(&"תוס".to_string()));
    }

    #[test]
    fn spelling_variations_cap_edits_per_letter() {
        // מילה עם הרבה מרווחים: אף וריאנט לא צובר יותר מ-3 עריכות של אותה
        // אות. נספרות ההוספות בלבד (אין במילה י/ו להשמטה).
        let base = "אבגדהזחט";
        for v in generate_spelling_variations(base, 10_000) {
            let added_vav = v.matches('ו').count();
            let added_yod = v.matches('י').count();
            assert!(added_vav <= 3, "יותר מ-3 ו' נוספו: {v}");
            assert!(added_yod <= 3, "יותר מ-3 י' נוספו: {v}");
        }
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
        // כללי הקצה (אין השמטה בקצות המילה) מונעים מ"וו" להתרוקן, אבל מילת
        // גרשיים בלבד עדיין מייצרת וריאנט ריק — והוא חייב לקרוס לענף ריק,
        // לא להתרחב ל-`.{0,3}.{0,3}` שתופס כל טרם קצר. (שימור ההגנה על
        // שורש ריק בבוני התבניות.)
        let flags = WordFlags {
            spelling: true,
            partial: true,
            ..Default::default()
        };
        let p = word_to_pattern("וו", &flags);
        assert_eq!(p, "(?:.{0,3}וו.{0,3}|.{0,3}ווו.{0,3}|.{0,3}ויו.{0,3})");
        assert!(
            !p.contains(".{0,3}.{0,3}"),
            "empty variant widened to wildcard: {p}"
        );
        let q = word_to_pattern("''", &flags);
        assert!(
            !q.contains(".{0,3}.{0,3}"),
            "empty variant widened to wildcard: {q}"
        );
    }

    // ── ארמית ────────────────────────────────────────────────────────────

    #[test]
    fn aramaic_final_swap_pairs() {
        assert_eq!(aramaic_final_swap("מלכה").as_deref(), Some("מלכא"));
        assert_eq!(aramaic_final_swap("מלכא").as_deref(), Some("מלכה"));
        assert_eq!(aramaic_final_swap("חכמים").as_deref(), Some("חכמין"));
        assert_eq!(aramaic_final_swap("חכמין").as_deref(), Some("חכמים"));
        assert_eq!(aramaic_final_swap("ספר"), None);
        assert_eq!(aramaic_final_swap(""), None);
    }

    #[test]
    fn aramaic_both_options_build_prefixed_swap_branches() {
        // שתי אפשרויות הארמית יחד — ההתנהגות ההיסטורית של "ארמית".
        let flags = WordFlags {
            aramaic_prefix: true,
            aramaic_suffix: true,
            ..Default::default()
        };
        // סופית ניתנת להחלפה: שני ענפים, לכל וריאנט קבוצת הקידומות
        // הדקדוקית (שנושאת את הצורות הארמיות ד/כד/אד/מד).
        let p = word_to_pattern("מלכה", &flags);
        assert_eq!(p, format!("(?:{g}מלכה|{g}מלכא)", g = GRAM_PREFIX_GROUP));
        // בלי סופית כזו — ענף קידומות יחיד.
        assert_eq!(
            word_to_pattern("ספר", &flags),
            format!("{}ספר", GRAM_PREFIX_GROUP)
        );
    }

    #[test]
    fn aramaic_prefix_only_keeps_root_final_letter() {
        let flags = WordFlags {
            aramaic_prefix: true,
            ..Default::default()
        };
        // קידומות בלבד: אין וריאנט שקילות סופית — ענף קידומות יחיד.
        assert_eq!(
            word_to_pattern("מלכה", &flags),
            format!("{}מלכה", GRAM_PREFIX_GROUP)
        );
    }

    #[test]
    fn aramaic_suffix_only_swaps_without_prefixes() {
        let flags = WordFlags {
            aramaic_suffix: true,
            ..Default::default()
        };
        // סיומות בלבד: שני ליטרלים (השורש + וריאנט השקילות), בלי קידומות.
        assert_eq!(word_to_pattern("מלכה", &flags), "(?:מלכה|מלכא)");
        // בלי סופית ניתנת להחלפה — המילה נשארת ליטרל.
        assert_eq!(word_to_pattern("ספר", &flags), "ספר");
    }

    #[test]
    fn aramaic_suffix_only_composes_with_other_options() {
        // סיומות ארמיות + קידומות דקדוקיות: כל וריאנט עובר את עץ-הבסיס.
        let flags = WordFlags {
            aramaic_suffix: true,
            gram_prefix: true,
            ..Default::default()
        };
        let p = word_to_pattern("מלכה", &flags);
        assert_eq!(p, format!("(?:{g}מלכה|{g}מלכא)", g = GRAM_PREFIX_GROUP));
    }

    #[test]
    fn aramaic_with_gram_suffix_carries_suffixes_on_both_variants() {
        let flags = WordFlags {
            aramaic_prefix: true,
            aramaic_suffix: true,
            gram_suffix: true,
            ..Default::default()
        };
        let p = word_to_pattern("מלכה", &flags);
        // הענף הארמי נושא גם את הסיומות — צורה עם קידומת ארמית וגם סיומת
        // (ודמלכתא) לא נופלת בין הענפים.
        assert!(p.contains(&format!("{}מלכה{}", GRAM_PREFIX_GROUP, SUFFIX_PATTERN)));
        assert!(p.contains(&format!("{}מלכא{}", GRAM_PREFIX_GROUP, SUFFIX_PATTERN)));
    }

    #[test]
    fn vocalized_aramaic_uses_voc_prefix_group() {
        let flags = WordFlags {
            aramaic_prefix: true,
            ..Default::default()
        };
        let core = VocCore {
            pattern: vocalized_free_pattern("מלכה"),
            base_len: 4,
        };
        let p = word_to_pattern_vocalized(&core, &flags);
        assert!(p.starts_with(&voc_gram_prefix_group()), "{p}");
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
        let result = build_word_regex("ספר", &flags, &[], &PHRASE_BUDGET).joined();
        assert!(!result.is_empty());
        assert!(
            result.chars().count() <= MAX_PATTERN_CHARS,
            "pattern length {} exceeds budget",
            result.chars().count()
        );
    }

    #[test]
    fn word_regex_includes_alternatives() {
        let result = build_word_regex(
            "שר",
            &WordFlags::default(),
            &["מלך".to_string()],
            &PHRASE_BUDGET,
        );
        assert_eq!(
            result,
            WordPattern::Alternation(vec!["שר".to_string(), "מלך".to_string()])
        );
        assert_eq!(result.joined(), "(?:שר|מלך)");
    }

    #[test]
    fn single_word_budget_relaxes_branch_count() {
        // With the phrase budget, typo+partial caps at MAX_TYPO_VARIATIONS
        // branches; the single-word budget must go beyond it. A long word is
        // needed to saturate both caps — a 3-letter word's edit-distance-1
        // neighborhood is naturally smaller than the raised phrase cap.
        let flags = WordFlags {
            typo: true,
            partial: true,
            ..Default::default()
        };
        let phrase = build_word_regex("בראשית", &flags, &[], &PHRASE_BUDGET);
        let single = build_word_regex("בראשית", &flags, &[], &SINGLE_WORD_BUDGET);
        assert_eq!(phrase.branches().len(), MAX_TYPO_VARIATIONS);
        assert!(
            single.branches().len() > MAX_TYPO_VARIATIONS,
            "single-word budget produced only {} branches",
            single.branches().len()
        );
    }

    #[test]
    fn single_word_branches_each_stay_under_dfa_limit() {
        // The char budget is gone on the single-word path; the invariant that
        // replaces it is that every branch compiles as its own DFA. This is
        // the guard MAX_PATTERN_CHARS pretended to be (it counts chars, not
        // states — the joined 48-branch pattern passes it at 806 chars and
        // still fails to compile).
        let flags = WordFlags {
            typo: true,
            partial: true,
            ..Default::default()
        };
        let pattern = build_word_regex("משה", &flags, &[], &SINGLE_WORD_BUDGET);
        for branch in pattern.branches() {
            assert!(
                tantivy_fst::Regex::new(branch).is_ok(),
                "branch failed to compile: {branch}"
            );
        }
    }

    #[test]
    fn phrase_words_keep_tight_caps() {
        // R2: a multi-word query compiles each word's joined pattern as one
        // DFA inside RegexPhraseQuery — the relaxed single-word budget must
        // not leak there.
        let so = make_options(&[("משה_0", &[(OPT_TYPO, true), (OPT_PARTIAL, true)])]);
        let q = prepare_advanced_query("משה עולם", 0, &HashMap::new(), &HashMap::new(), &so);
        assert_eq!(q.regex_terms.len(), 2);
        assert!(q.regex_terms[0].branches().len() <= MAX_TYPO_VARIATIONS);
        assert!(q.regex_terms[0].joined().chars().count() <= MAX_PATTERN_CHARS);
    }

    // ── WordPattern::parse / split_top_level_alternation ────────────────

    #[test]
    fn parse_splits_non_capturing_group() {
        assert_eq!(
            WordPattern::parse("(?:משה|מסה)"),
            WordPattern::Alternation(vec!["משה".to_string(), "מסה".to_string()])
        );
    }

    #[test]
    fn parse_splits_capturing_group() {
        assert_eq!(
            WordPattern::parse("(משה|מסה)"),
            WordPattern::Alternation(vec!["משה".to_string(), "מסה".to_string()])
        );
    }

    #[test]
    fn parse_splits_bare_top_level_alternation() {
        assert_eq!(
            WordPattern::parse("([א-ת]{2,4}(ים|ות|ה)?)|([א-ת]+[יו][ם|ן])"),
            WordPattern::Alternation(vec![
                "([א-ת]{2,4}(ים|ות|ה)?)".to_string(),
                "([א-ת]+[יו][ם|ן])".to_string(),
            ])
        );
    }

    #[test]
    fn pipe_inside_char_class_is_not_split() {
        assert_eq!(
            WordPattern::parse("[ם|ן]"),
            WordPattern::Literal("[ם|ן]".to_string())
        );
    }

    #[test]
    fn escaped_pipe_is_not_split() {
        assert_eq!(
            WordPattern::parse(r"א\|ב"),
            WordPattern::Literal(r"א\|ב".to_string())
        );
    }

    #[test]
    fn nested_alternation_stays_whole() {
        // A group that does not wrap the whole pattern must not be stripped,
        // and its inner `|` is not top-level (R1).
        assert_eq!(
            WordPattern::parse(".{0,2}(א|ב).{0,2}"),
            WordPattern::Literal(".{0,2}(א|ב).{0,2}".to_string())
        );
        assert_eq!(
            WordPattern::parse("(ו|מ)?משה"),
            WordPattern::Literal("(ו|מ)?משה".to_string())
        );
    }

    #[test]
    fn empty_branches_are_dropped() {
        // The all-optional-letter word emits a leading empty branch (R7); it
        // matches only the empty string, which no indexed term is.
        assert_eq!(
            WordPattern::parse("(?:|.{0,3}ו.{0,3}|.{0,3}וו.{0,3})"),
            WordPattern::Alternation(vec![
                ".{0,3}ו.{0,3}".to_string(),
                ".{0,3}וו.{0,3}".to_string(),
            ])
        );
        // Degenerate all-empty alternation collapses to a literal.
        assert_eq!(
            WordPattern::parse("(?:|)"),
            WordPattern::Literal("(?:|)".to_string())
        );
    }

    #[test]
    fn parse_round_trips_generator_output() {
        // parse(joined()) must reproduce the generator's own branches, so the
        // raw-string API path and the structured path stay in lockstep.
        let flags = WordFlags {
            typo: true,
            partial: true,
            ..Default::default()
        };
        let pattern = build_word_regex("משה", &flags, &[], &SINGLE_WORD_BUDGET);
        assert!(matches!(pattern, WordPattern::Alternation(_)));
        assert_eq!(WordPattern::parse(&pattern.joined()), pattern);
    }

    // ── resolve_gaps ─────────────────────────────────────────────────────

    #[test]
    fn custom_spacing_parsed_correctly() {
        let mut raw = HashMap::new();
        raw.insert("0-1".to_string(), "3".to_string());
        raw.insert("1-2".to_string(), "7".to_string());
        let gaps = resolve_gaps(&raw, 9, 3);
        assert_eq!(gaps, vec![3, 7]);
    }

    #[test]
    fn empty_custom_spacing_falls_back_to_distance() {
        let raw = HashMap::new();
        let gaps = resolve_gaps(&raw, 4, 3);
        assert_eq!(gaps, vec![4, 4]);
    }

    #[test]
    fn missing_custom_spacing_entry_gets_max_custom_value() {
        // Mirrors display_highlight::spacing_for_gaps: a pair with no entry
        // falls back to the widest custom value, not to `distance` or zero.
        let mut raw = HashMap::new();
        raw.insert("1-2".to_string(), "5".to_string());
        let gaps = resolve_gaps(&raw, 3, 3);
        assert_eq!(gaps, vec![5, 5]);
    }

    #[test]
    fn custom_spacing_handles_overflow_and_negatives() {
        let mut raw = HashMap::new();
        raw.insert("0-1".to_string(), "5000000000".to_string()); // > u32::MAX
        raw.insert("1-2".to_string(), "-7".to_string()); // negative
        let gaps = resolve_gaps(&raw, 0, 3);
        // Overflow clamps to u32::MAX rather than collapsing to 0; a
        // negative value clamps to no spacing.
        assert_eq!(gaps, vec![u32::MAX, 0]);
    }

    // ── prepare_advanced_query ───────────────────────────────────────────

    /// The joined (single-string) form of each term, for string assertions.
    fn joined_terms(q: &AdvancedQuery) -> Vec<String> {
        q.regex_terms.iter().map(WordPattern::joined).collect()
    }

    #[test]
    fn plain_two_word_query_uses_distance_as_slop() {
        let q = prepare_advanced_query(
            "שלום עולם",
            3,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(joined_terms(&q), vec!["שלום", "עולם"]);
        assert_eq!(q.gaps, vec![3]);
        assert_eq!(q.max_expansions, 100);
    }

    #[test]
    fn custom_spacing_overrides_distance() {
        let mut spacing = HashMap::new();
        spacing.insert("0-1".to_string(), "5".to_string());
        let q = prepare_advanced_query("שלום עולם", 3, &spacing, &HashMap::new(), &HashMap::new());
        assert_eq!(q.gaps, vec![5]);
    }

    #[test]
    fn per_pair_gaps_survive_into_the_query() {
        let mut spacing = HashMap::new();
        spacing.insert("0-1".to_string(), "3".to_string());
        spacing.insert("1-2".to_string(), "5".to_string());
        let q = prepare_advanced_query(
            "ויאמר אל משה",
            0,
            &spacing,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(q.gaps, vec![3, 5]);
    }

    #[test]
    fn nikud_is_stripped_from_query() {
        let q = prepare_advanced_query("סֵפֶר", 0, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(joined_terms(&q), vec!["ספר"]);
    }

    #[test]
    fn maqaf_in_raw_query_separates_words() {
        // רגרסיה: הנרמול רץ לפני פיצול המילים, ומחיקת המקף בו הדביקה
        // "אשר־שמע" למילה אחת ("אשרשמע") שאינה במילון הטרמים.
        let q = prepare_advanced_query(
            "אֲשֶׁר־שָׁמַע",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(joined_terms(&q), vec!["אשר", "שמע"]);
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
        assert_eq!(joined_terms(&q), vec!["torah"]);
    }

    #[test]
    fn alternative_words_are_normalized() {
        let mut alts = HashMap::new();
        alts.insert(0u32, vec!["תּוֹרָה".to_string()]);
        let q = prepare_advanced_query("ספר", 0, &HashMap::new(), &alts, &HashMap::new());
        assert_eq!(joined_terms(&q), vec!["(?:ספר|תורה)"]);
    }

    #[test]
    fn grammatical_prefix_option_applied() {
        let so = make_options(&[("ספר_0", &[(OPT_GRAM_PREFIX, true)])]);
        let q = prepare_advanced_query("ספר", 0, &HashMap::new(), &HashMap::new(), &so);
        assert_eq!(
            joined_terms(&q),
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
            compute_max_expansions(&["שלום".to_string()], &HashMap::new(), false),
            100
        );
        assert_eq!(
            compute_max_expansions(
                &["שלום".to_string(), "עולם".to_string()],
                &HashMap::new(),
                false
            ),
            1_024
        );
    }

    #[test]
    fn max_expansions_grammatical_prefix_by_word_length() {
        let so = make_options(&[("ספר_0", &[(OPT_GRAM_PREFIX, true)])]);
        assert_eq!(
            compute_max_expansions(&["ספר".to_string()], &so, false),
            40_000
        );
    }

    #[test]
    fn max_expansions_typo_tolerance() {
        let so = make_options(&[("ספר_0", &[(OPT_TYPO, true)])]);
        assert_eq!(
            compute_max_expansions(&["ספר".to_string()], &so, false),
            500
        );
        assert_eq!(
            compute_max_expansions(&["ספר".to_string(), "תורה".to_string()], &so, false),
            PHRASE_MAX_EXPANSIONS
        );
    }

    #[test]
    fn max_expansions_single_word_typo_with_morph_uses_morph_ceiling() {
        // typo+partial on a single word runs on the relaxed per-branch path
        // and legitimately matches many terms; a tight typo ceiling would
        // truncate it far too early (R4). Multi-word takes the flat phrase
        // ceiling regardless of shape.
        let so = make_options(&[("משה_0", &[(OPT_TYPO, true), (OPT_PARTIAL, true)])]);
        assert_eq!(
            compute_max_expansions(&["משה".to_string()], &so, false),
            40_000
        );
        assert_eq!(
            compute_max_expansions(&["משה".to_string(), "עם".to_string()], &so, false),
            PHRASE_MAX_EXPANSIONS
        );
    }

    // ── Branch-order contract (degrade truncates from the back) ─────────

    #[test]
    fn typo_branches_put_every_exact_form_before_any_typo_variant() {
        // Truncation keeps a prefix of the branch list, so the exact form of
        // each candidate (query word AND alternatives) must precede all typo
        // variants — an alternative spelling must never lose its slot to a
        // typo variant of the primary word.
        let flags = WordFlags {
            typo: true,
            ..Default::default()
        };
        let alts = vec!["תורה".to_string()];
        let pattern = build_word_regex("ספר", &flags, &alts, &SINGLE_WORD_BUDGET);
        let branches = pattern.branches();
        assert_eq!(branches[0], "ספר");
        assert_eq!(branches[1], "תורה");
        // Everything after the exact forms is a typo variant of one of them.
        assert!(branches.len() > 2, "typo expansion produced no variants");
    }
}
