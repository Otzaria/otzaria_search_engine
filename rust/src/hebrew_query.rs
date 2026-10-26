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

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use unicode_segmentation::UnicodeSegmentation;

use crate::gap_phrase::Proximity;

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

// ── Morphological affix alternatives ───────────────────────────────────────
//
// Nikud-free (see index contract above). Rendered as optional non-capturing
// groups by [`optional_alts_group`] — mark-free for the plain dictionary,
// with free-mark runs interleaved for the vocalized one. No anchors.

/// First slot of the grammatical prefix group (קידומות דקדוקיות); דא and כש
/// let Aramaic דא־ / כש־ prefixed forms match. No bare א: it is a root letter
/// far more often than a prefix (אשבת, אמר→אאמר).
const GRAM_PREFIX_ALTS_A: &[&str] = &["ו", "מ", "דא", "כש", "כ", "ב", "ש", "ל", "ה", "ד"];
/// A grammatical prefix never leaves fewer root letters than this, or a
/// short word would match inside unrelated words (ית→בית) — the
/// `MIN_STEM_LETTERS` of the dictionary path. Abbreviations (ר', רמב"ם)
/// are exempt: their letters stand for whole words.
const MIN_GRAM_PREFIX_ROOT_LETTERS: usize = 3;
/// The same floor for grammatical suffixes: a short root takes none (מה↛מות, ה↛ת).
const MIN_GRAM_SUFFIX_ROOT_LETTERS: usize = 3;
/// Second slot, after the first prefix (ו+ב, ש+ל, ו+כש…).
const GRAM_PREFIX_ALTS_B: &[&str] = &["כש", "כ", "ב", "ש", "ל", "ה", "ד"];
/// The definite article, last before the root.
const HE_ALTS: &[&str] = &["ה"];
/// First slot of the Aramaic prefix group (קידומות ארמיות): ד/דא/א, and
/// כד/אד/מד through the second slot.
const ARAMAIC_PREFIX_ALTS_A: &[&str] = &["ו", "מ", "דא", "א", "כש", "כ", "ב", "ש", "ל", "ה", "ד"];
/// Grammatical suffixes (סיומות דקדוקיות): possessive, plural and the rare
/// יות/יא/תא endings.
const GRAM_SUFFIX_ALTS: &[&str] = &[
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
/// Past-tense person endings (אמרתי, אמרת, אמרתם, אמרתן); נו/ו are already
/// grammatical suffixes. Only for a verb-sized root — see [`takes_verb_suffixes`].
const VERB_SUFFIX_ALTS: &[&str] = &["תי", "ת", "תם", "תן"];
/// A root ending in ה drops it before a suffix: ת (construct — תורתו) or ות
/// (plural — תורות, תורותיו).
const HE_STEM_ALTS: &[&str] = &["ות", "ת"];

// Letters tried for single-insertion typo variants (ordered by frequency).
const INSERTION_LETTERS: &[&str] = &[
    "ו", "י", "א", "ה", "פ", "ל", "מ", "נ", "ב", "כ", "ש", "ת", "ר",
];

// ── Budget constants ───────────────────────────────────────────────────────

/// Char budget for a *joined* per-word pattern on the phrase path, where
/// `RegexPhraseQuery` compiles each word's whole pattern as one DFA. A pattern
/// the DFA limits refuse falls back to the term-list engine, so this only bounds
/// build cost: 12K keeps ~8 typo × spelling × affix branches (~60ms to build, cached).
const MAX_PATTERN_CHARS: usize = 12_000;

/// Phrase-path variation cap when typo tolerance is active (substitution +
/// deletion + insertion can produce many candidates; we keep the most useful
/// ones). The historical worst shape (wildcard-wrapped typo+partial branches)
/// determinizes to roughly ~100 states per branch, so 64 branches ≈ 6.4k
/// states — inside the vendored 8 192-state cap with margin.
const MAX_TYPO_VARIATIONS: usize = 64;

/// Phrase-path variation cap without typo tolerance. Raised 48→96 together
/// with the spelling-branch caps: the degrade phrase engine compiles each
/// branch as its own small DFA, so the joined-DFA state cap no longer bounds
/// how many branches a phrase word may carry — the exact phrase path
/// pre-compiles its joined pattern and falls back to the degrade engine when
/// the branch set is too rich for one DFA. The char budget below still trims
/// the joined form the exact path and the highlight automatons consume.
const MAX_NORMAL_VARIATIONS: usize = 96;

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
/// out identically. Raised 16→32 once the phrase path gained its degrade
/// engine: overflow anywhere downstream now truncates by priority instead of
/// erroring, so the cap costs only linear scan time (one FST scan per branch
/// per segment, cached across a search's repeated engine calls).
pub(crate) const MAX_SPELLING_BRANCHES: usize = 32;

/// Cap on כתיב מלא/חסר variants that fan through the *grammatical*
/// morphological builders (prefix group / suffix alternation around every
/// variant — denser patterns per branch than the plain window builders).
/// Historically 4–10 depending on the combo, sized against the joined-DFA
/// state cap; unified at 16 now that the search side compiles per branch and
/// the exact phrase path pre-checks its joined DFA before use. The
/// display-highlight builder fans out at [`MAX_SPELLING_BRANCHES`] (a
/// superset), so every variant the search matches still highlights.
const MAX_GRAMMATICAL_SPELLING_BRANCHES: usize = 16;

/// `max_expansions` ceiling for the phrase path.
///
/// tantivy's `RegexPhraseQuery` enforces it cumulatively across all word
/// positions per segment and *errors* on overflow — so the engine's phrase
/// builder (`phrase_query_with_degrade`) pre-counts each position's matches
/// and uses this value only as the switch between execution engines: at or
/// under it the historical `RegexPhraseQuery` runs (identical scoring and
/// behavior, provably unable to trip tantivy's ceiling); over it the
/// term-list degrade path runs instead, which has per-*position* budgets and
/// truncates (highest-priority branches first) rather than erroring. Since
/// overflow no longer fails the query, the value is purely a "stay on the
/// well-trodden tantivy path while it is cheap" threshold.
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
    let query = keep_second_of_paired_readings(query);
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

/// זוג `(X) [Y]` שהודבק לשאילתה נשאר Y בלבד: באינדקס שתי הקריאות חולקות
/// עמדה אחת, ושתי מילים בשאילתה היו דורשות שתי עמדות.
fn keep_second_of_paired_readings(query: &str) -> std::borrow::Cow<'_, str> {
    use crate::hebrew_tokenizer::{next_token_boundaries, paired_reading_after};
    let mut out = String::new();
    let mut copied = 0;
    let mut pos = 0;
    while let Some((start, end)) = next_token_boundaries(query, pos) {
        pos = end;
        if let Some(pair) = paired_reading_after(query, start, end) {
            out.push_str(&query[copied..start - 1]);
            // Parentheses break tokens even when attached to the preceding
            // word. Preserve that boundary when replacing the entire pair.
            out.push(' ');
            out.push_str(&query[pair.second_start..pair.second_end]);
            out.push(' ');
            copied = pair.after_pair;
            pos = copied;
        }
    }
    if copied == 0 {
        return std::borrow::Cow::Borrowed(query);
    }
    out.push_str(&query[copied..]);
    std::borrow::Cow::Owned(out)
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

/// The same query words as `split_query_words`, together with their precise
/// source ranges in UTF-16 code units for Flutter text selections. Walk the
/// full input so paired readings spanning whitespace select only the qere;
/// normalizing each whitespace chunk independently would shift option keys.
pub(crate) fn query_word_spans(query: &str) -> Vec<(String, u32, u32)> {
    use crate::hebrew_tokenizer::{next_token_boundaries, paired_reading_after};

    let mut spans = Vec::new();
    let mut pos = 0;
    let mut source_byte = 0;
    let mut source_utf16 = 0u32;
    while let Some((mut start, mut end)) = next_token_boundaries(query, pos) {
        pos = end;
        if let Some(pair) = paired_reading_after(query, start, end) {
            start = pair.second_start;
            end = pair.second_end;
            pos = pair.after_pair;
        }
        source_utf16 += query[source_byte..start].encode_utf16().count() as u32;
        let start_utf16 = source_utf16;
        source_utf16 += query[start..end].encode_utf16().count() as u32;
        source_byte = end;
        // Quote folding, transparent punctuation and doubled geresh must use
        // exactly the same normalization as the complete query.
        for word in split_query_words(&query[start..end]) {
            spans.push((word, start_utf16, source_utf16));
        }
    }
    spans
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

/// Unicode canonical combining class of a word mark ([`is_word_mark`]); 0 for every
/// other char, CGJ (U+034F) included. Arm order matters: the first match wins.
pub(crate) fn mark_combining_class(c: char) -> u8 {
    match c {
        '\u{05B0}' => 10,
        '\u{05B1}' => 11,
        '\u{05B2}' => 12,
        '\u{05B3}' => 13,
        '\u{05B4}' => 14,
        '\u{05B5}' => 15,
        '\u{05B6}' => 16,
        '\u{05B7}' => 17,
        '\u{05B8}' | '\u{05C7}' => 18,
        '\u{05B9}' | '\u{05BA}' => 19,
        '\u{05BB}' => 20,
        '\u{05BC}' => 21,
        '\u{05BD}' => 22,
        '\u{05BF}' => 23,
        '\u{05C1}' => 24,
        '\u{05C2}' => 25,
        '\u{0591}'
        | '\u{0596}'
        | '\u{059B}'
        | '\u{05A2}'..='\u{05A7}'
        | '\u{05AA}'
        | '\u{05C5}' => 220,
        '\u{059A}' | '\u{05AD}' => 222,
        '\u{05AE}' => 228,
        '\u{0592}'..='\u{05AF}' | '\u{05C4}' => 230,
        '\u{0334}'..='\u{0338}' => 1,
        '\u{0321}' | '\u{0322}' | '\u{0327}' | '\u{0328}' => 202,
        '\u{031B}' => 216,
        '\u{0316}'..='\u{0319}'
        | '\u{031C}'..='\u{0320}'
        | '\u{0323}'..='\u{0326}'
        | '\u{0329}'..='\u{0333}'
        | '\u{0339}'..='\u{033C}'
        | '\u{0347}'..='\u{0349}'
        | '\u{034D}'
        | '\u{034E}'
        | '\u{0353}'..='\u{0356}'
        | '\u{0359}'
        | '\u{035A}' => 220,
        '\u{0315}' | '\u{031A}' | '\u{0358}' => 232,
        '\u{035C}' | '\u{035F}' | '\u{0362}' => 233,
        '\u{035D}' | '\u{035E}' | '\u{0360}' | '\u{0361}' => 234,
        '\u{0345}' => 240,
        '\u{0300}'..='\u{034E}' | '\u{0350}'..='\u{036F}' => 230,
        _ => 0,
    }
}

/// Combining grapheme joiner: invisible, and dropped from vocalized terms and queries.
pub(crate) const CGJ: char = '\u{034F}';

/// `s` with every run of marks stably sorted by [`mark_combining_class`], as NFC orders
/// them, so canonically-equivalent spellings (דגש+קמץ / קמץ+דגש) are one term. [`CGJ`] is
/// dropped first, so the marks on its two sides (`ירושלַ͏ִם`) sort as one run.
pub(crate) fn canonical_mark_order(s: &str) -> Cow<'_, str> {
    let mut prev = 0;
    let in_order = s.chars().all(|c| {
        let class = mark_combining_class(c);
        let ok = c != CGJ && (class == 0 || prev <= class);
        prev = class;
        ok
    });
    if in_order {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut run: Vec<(u8, char)> = Vec::new();
    let flush = |run: &mut Vec<(u8, char)>, out: &mut String| {
        run.sort_by_key(|&(class, _)| class);
        out.extend(run.drain(..).map(|(_, mark)| mark));
    };
    for c in s.chars().filter(|&c| c != CGJ) {
        match mark_combining_class(c) {
            0 => {
                flush(&mut run, &mut out);
                out.push(c);
            }
            class => run.push((class, c)),
        }
    }
    flush(&mut run, &mut out);
    Cow::Owned(out)
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

/// sup/sub: their content is read as its own word, so their tags separate words
/// ([`tag_breaks_word`]) — otherwise `תורה<sup>30</sup>` would index as `תורה30`.
pub(crate) const SCRIPT_TAG_NAMES: &[&str] = &["sub", "sup"];

/// The tag's name and whether it closes, for a body (the text between `<` and `>`) of
/// an optional `/` and an ASCII alphanumeric name of at most 10 characters.
fn tag_name(body: &str) -> Option<(bool, &str)> {
    let (closing, name) = match body.strip_prefix('/') {
        Some(name) => (true, name),
        None => (false, body),
    };
    let len = name.bytes().take_while(u8::is_ascii_alphanumeric).count();
    (1..=10).contains(&len).then(|| (closing, &name[..len]))
}

fn is_listed(names: &[&str], name: &str) -> bool {
    names.iter().any(|listed| listed.eq_ignore_ascii_case(name))
}

fn is_hebrew_letter(c: char) -> bool {
    matches!(c, '\u{05D0}'..='\u{05EA}')
}

/// A Hebrew letter, or a presentation form that folds to one letter and its marks.
fn is_script_letter(c: char) -> bool {
    match fold_presentation_form(c) {
        Some(folded) => {
            let mut folded = folded.chars();
            folded.next().is_some_and(is_hebrew_letter) && folded.all(is_attached_mark)
        }
        None => is_hebrew_letter(c),
    }
}

/// Whether the tag `source[lt..=gt]` separates words: block tags and sup/sub do, except
/// a sup/sub around one letter inside a word (a hanging letter, `מְ<sup>נַ</sup>שֶּׁה`).
pub(crate) fn tag_breaks_word(source: &str, lt: usize, gt: usize) -> bool {
    let Some((closing, name)) = tag_name(&source[lt + 1..gt]) else {
        return false;
    };
    if is_listed(BREAKING_TAG_NAMES, name) {
        return true;
    }
    if !is_listed(SCRIPT_TAG_NAMES, name) {
        return false;
    }
    let open = if closing {
        script_open_before(source, lt)
    } else {
        Some((lt, gt))
    };
    !open.is_some_and(|(lt, gt)| is_word_internal_script(source, lt, gt))
}

/// The tag that ends right before the lone letter (and its marks) preceding `lt`.
fn script_open_before(source: &str, lt: usize) -> Option<(usize, usize)> {
    let mut before = trim_script_context_end(&source[..lt]).chars();
    before.next_back().filter(|&c| is_script_letter(c))?;
    let gt = trim_script_formatting_end(before.as_str())
        .len()
        .checked_sub(1)?;
    if source.as_bytes()[gt] != b'>' {
        return None;
    }
    let lt = source[..gt].rfind('<')?;
    (!source[lt + 1..gt].contains('>')).then_some((lt, gt))
}

fn is_inline_tag(body: &str) -> bool {
    !tag_name(body).is_some_and(|(_, name)| {
        is_listed(BREAKING_TAG_NAMES, name) || is_listed(SCRIPT_TAG_NAMES, name)
    })
}

/// Zero-width characters which neither text field keeps in a term. They must not
/// change the classification of a neighbouring tag before the tokenizer drops them.
fn is_script_invisible(c: char) -> bool {
    is_invisible_char(c) || c == CGJ || c == '\u{FB1E}'
}

/// Walk the original source without allocating or changing its UTF-16 offsets.
/// Formatting and invisible characters can alternate, so trim them together.
fn trim_script_formatting_start(mut source: &str) -> &str {
    loop {
        source = source.trim_start_matches(is_script_invisible);
        let Some(body) = source.strip_prefix('<') else {
            break;
        };
        let Some(gt) = body.find('>') else { break };
        if !is_inline_tag(&body[..gt]) {
            break;
        }
        source = &body[gt + 1..];
    }
    source
}

fn trim_script_formatting_end(mut source: &str) -> &str {
    loop {
        source = source.trim_end_matches(is_script_invisible);
        let Some(body) = source.strip_suffix('>') else {
            break;
        };
        let Some(lt) = body.rfind('<') else { break };
        if body[lt + 1..].contains('>') || !is_inline_tag(&body[lt + 1..]) {
            break;
        }
        source = &body[..lt];
    }
    source
}

fn trim_script_context_end(mut source: &str) -> &str {
    loop {
        let trimmed = trim_script_formatting_end(source).trim_end_matches(is_word_mark);
        if trimmed.len() == source.len() {
            return source;
        }
        source = trimmed;
    }
}

fn trim_script_context_start(mut source: &str) -> &str {
    loop {
        let trimmed = trim_script_formatting_start(source).trim_start_matches(is_word_mark);
        if trimmed.len() == source.len() {
            return source;
        }
        source = trimmed;
    }
}

/// The word `footnote-marker` in a tag's attributes, as inline_notes.rs finds the marker.
fn marks_footnote(attributes: &str) -> bool {
    let lower = attributes.to_ascii_lowercase();
    let word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    lower.match_indices("footnote-marker").any(|(i, found)| {
        !word(lower.as_bytes()[..i].last()) && !word(lower.as_bytes().get(i + found.len()))
    })
}

/// Whether the open tag `source[lt..=gt]` wraps one letter (with marks) between letters.
fn is_word_internal_script(source: &str, lt: usize, gt: usize) -> bool {
    let Some((false, name)) = tag_name(&source[lt + 1..gt]) else {
        return false;
    };
    if marks_footnote(&source[lt + 1..gt]) {
        return false;
    }
    if !trim_script_context_end(&source[..lt])
        .chars()
        .next_back()
        .is_some_and(is_script_letter)
    {
        return false;
    }
    let mut content = trim_script_formatting_start(&source[gt + 1..]).chars();
    if !content.next().is_some_and(is_script_letter) {
        return false;
    }
    let rest = trim_script_context_start(content.as_str());
    let Some(close) = rest.strip_prefix('<') else {
        return false;
    };
    let Some(close_gt) = close.find('>') else {
        return false;
    };
    matches!(tag_name(&close[..close_gt]), Some((true, closing)) if closing.eq_ignore_ascii_case(name))
        && trim_script_context_start(&close[close_gt + 1..])
            .chars()
            .next()
            .is_some_and(is_script_letter)
}

/// [`tag_breaks_word`] over a char vector, written apart so the reference scan checks it.
#[cfg(test)]
fn tag_breaks_word_chars(chars: &[char], lt: usize, gt: usize) -> bool {
    let name_of = |lt: usize, gt: usize| -> Option<(bool, String)> {
        let body = &chars[lt + 1..gt];
        let closing = body.first() == Some(&'/');
        let name: String = body[usize::from(closing)..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        (1..=10).contains(&name.len()).then_some((closing, name))
    };
    let Some((closing, name)) = name_of(lt, gt) else {
        return false;
    };
    if BREAKING_TAG_NAMES.contains(&name.as_str()) {
        return true;
    }
    if !SCRIPT_TAG_NAMES.contains(&name.as_str()) {
        return false;
    }
    let letter = |i: usize| {
        chars.get(i).is_some_and(|&c| {
            let folded: Vec<char> =
                fold_presentation_form(c).map_or(vec![c], |f| f.chars().collect());
            folded.first().is_some_and(|&f| is_hebrew_letter(f))
                && folded[1..].iter().all(|&m| is_attached_mark(m))
        })
    };
    let mark = |i: usize| chars.get(i).is_some_and(|&c| is_word_mark(c));
    let invisible = |i: usize| {
        chars
            .get(i)
            .is_some_and(|&c| is_invisible_char(c) || matches!(c, '\u{034F}' | '\u{FB1E}'))
    };
    let inline = |lt, gt| {
        !name_of(lt, gt).is_some_and(|(_, name)| {
            BREAKING_TAG_NAMES.contains(&name.as_str()) || SCRIPT_TAG_NAMES.contains(&name.as_str())
        })
    };
    let skip_start = |mut i: usize| {
        loop {
            while invisible(i) {
                i += 1;
            }
            if chars.get(i) != Some(&'<') {
                break;
            }
            let Some(gt) = chars[i..].iter().position(|&c| c == '>').map(|p| i + p) else {
                break;
            };
            if !inline(i, gt) {
                break;
            }
            i = gt + 1;
        }
        i
    };
    let skip_end = |mut i: usize| {
        loop {
            while i > 0 && invisible(i - 1) {
                i -= 1;
            }
            if i == 0 || chars[i - 1] != '>' {
                break;
            }
            let Some(lt) = chars[..i - 1].iter().rposition(|&c| c == '<') else {
                break;
            };
            if chars[lt + 1..i - 1].contains(&'>') || !inline(lt, i - 1) {
                break;
            }
            i = lt;
        }
        i
    };
    let (open_lt, open_gt) = if closing {
        let mut i = lt;
        loop {
            let old = i;
            i = skip_end(i);
            while i > 0 && mark(i - 1) {
                i -= 1;
            }
            if i == old {
                break;
            }
        }
        if i == 0 || !letter(i - 1) {
            return true;
        }
        let i = skip_end(i - 1);
        if i == 0 || chars[i - 1] != '>' {
            return true;
        }
        let open_gt = i - 1;
        let Some(open_lt) = chars[..open_gt].iter().rposition(|&c| c == '<') else {
            return true;
        };
        if chars[open_lt + 1..open_gt].contains(&'>') {
            return true;
        }
        (open_lt, open_gt)
    } else {
        (lt, gt)
    };
    let attributes: Vec<char> = chars[open_lt + 1..open_gt]
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let marker: Vec<char> = "footnote-marker".chars().collect();
    let word = |c: Option<&char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_');
    let footnote = (0..attributes.len()).any(|i| {
        attributes[i..].starts_with(&marker)
            && (i == 0 || !word(attributes.get(i - 1)))
            && !word(attributes.get(i + marker.len()))
    });
    let mut before = open_lt;
    loop {
        let old = before;
        before = skip_end(before);
        while before > 0 && mark(before - 1) {
            before -= 1;
        }
        if before == old {
            break;
        }
    }
    let content = skip_start(open_gt + 1);
    if footnote
        || name_of(open_lt, open_gt) != Some((false, name.clone()))
        || before == 0
        || !letter(before - 1)
        || !letter(content)
    {
        return true;
    }
    let mut i = content + 1;
    loop {
        let old = i;
        i = skip_start(i);
        while mark(i) {
            i += 1;
        }
        if i == old {
            break;
        }
    }
    if chars.get(i) != Some(&'<') {
        return true;
    }
    let Some(close_gt) = chars[i..].iter().position(|&c| c == '>').map(|p| i + p) else {
        return true;
    };
    let mut after = close_gt + 1;
    loop {
        let old = after;
        after = skip_start(after);
        while mark(after) {
            after += 1;
        }
        if after == old {
            break;
        }
    }
    name_of(i, close_gt) != Some((true, name)) || !letter(after)
}

/// The whitespace entities [`strip_html_for_indexing`] turns into a space.
const SPACE_ENTITIES: [&[u8]; 4] = [b"&nbsp;", b"&thinsp;", b"&ensp;", b"&emsp;"];

fn space_entity_len(at: &[u8]) -> Option<usize> {
    SPACE_ENTITIES
        .iter()
        .find(|entity| at.starts_with(entity))
        .map(|entity| entity.len())
}

/// Feeds `f` [`strip_html_for_indexing`]'s output piece by piece, without building it.
fn for_each_html_stripped_piece(text: &str, mut f: impl FnMut(&str)) {
    if !text.contains(['<', '&']) {
        f(text);
        return;
    }
    let bytes = text.as_bytes();
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let (replacement, next) = match bytes[i] {
            b'<' => match bytes[i + 1..].iter().position(|&b| b == b'>') {
                Some(close) => {
                    let breaking = tag_breaks_word(text, i, i + 1 + close);
                    (if breaking { " " } else { "" }, i + close + 2)
                }
                None => ("<", i + 1),
            },
            b'&' => match space_entity_len(&bytes[i..]) {
                Some(len) => (" ", i + len),
                None => {
                    // A space entity inside the span is one character, and its `;` is gone.
                    let mut j = i + 1;
                    let mut semi = None;
                    while j < bytes.len() {
                        match bytes[j] {
                            b';' => {
                                semi = Some(j);
                                break;
                            }
                            b'&' => j += space_entity_len(&bytes[j..]).unwrap_or(1),
                            _ => j += 1,
                        }
                    }
                    match semi {
                        Some(semi) if semi > i + 1 => ("", semi + 1),
                        _ => ("&", i + 1),
                    }
                }
            },
            _ => {
                i += 1;
                continue;
            }
        };
        if copied < i {
            f(&text[copied..i]);
        }
        if !replacement.is_empty() {
            f(replacement);
        }
        i = next;
        copied = next;
    }
    if copied < bytes.len() {
        f(&text[copied..]);
    }
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
    let mut out = String::with_capacity(text.len());
    for_each_html_stripped_piece(text, |piece| out.push_str(piece));
    out
}

/// [`strip_html_for_indexing`] as the char-vector scan its stream must match.
#[cfg(test)]
pub(crate) fn strip_html_for_indexing_reference(text: &str) -> String {
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
                    if tag_breaks_word_chars(&chars, i, i + 1 + close) {
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

/// Whitespace runs collapsed to one space, none leading or trailing.
struct Collapsed {
    out: String,
    pending_space: bool,
}

impl Collapsed {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            out: String::with_capacity(capacity),
            pending_space: false,
        }
    }

    #[inline]
    fn push(&mut self, c: char) {
        if c.is_whitespace() {
            if !self.out.is_empty() {
                self.pending_space = true;
            }
        } else {
            if self.pending_space {
                self.out.push(' ');
                self.pending_space = false;
            }
            self.out.push(c);
        }
    }

    /// [`Self::push`] of each char of `run`, a run [`plain_run_len`] vouched for.
    #[inline]
    fn push_plain(&mut self, run: &str) {
        if self.pending_space {
            self.out.push(' ');
            self.pending_space = false;
        }
        self.out.push_str(run);
    }
}

/// Bytes at the start of `bytes` that every normalization copies unchanged: printable
/// ASCII and the Hebrew block from U+05C8 on (letters, geresh, gershayim) — no whitespace,
/// attached mark, presentation form or invisible among them.
#[inline]
fn plain_run_len(bytes: &[u8]) -> usize {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x21..=0x7E => i += 1,
            0xD7 if matches!(bytes.get(i + 1), Some(0x88..=0xBF)) => i += 2,
            _ => break,
        }
    }
    i
}

/// Calls `run` with the runs [`plain_run_len`] finds in `piece`, and `other` with every
/// other char.
#[inline]
fn for_each_run(piece: &str, mut run: impl FnMut(&str), mut other: impl FnMut(char)) {
    let bytes = piece.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let len = plain_run_len(&bytes[i..]);
        if len > 0 {
            run(&piece[i..i + len]);
            i += len;
            continue;
        }
        let c = piece[i..].chars().next().expect("on a char boundary");
        other(c);
        i += c.len_utf8();
    }
}

/// Feeds `f` `c` with its presentation form decomposed.
#[inline]
fn for_each_folded(c: char, mut f: impl FnMut(char)) {
    match fold_presentation_form(c) {
        Some(folded) => folded.chars().for_each(f),
        None => f(c),
    }
}

/// HTML strip → fold presentation forms → strip attached marks (`strip_marks`) → collapse
/// whitespace, in one pass. `drop` filters characters before folding (the PDF path drops
/// invisibles).
fn normalize_one_pass(input: &str, strip_marks: bool, drop: impl Fn(char) -> bool) -> String {
    let out = std::cell::RefCell::new(Collapsed::with_capacity(input.len()));
    for_each_html_stripped_piece(input, |piece| {
        for_each_run(
            piece,
            |run| out.borrow_mut().push_plain(run),
            |c| {
                if drop(c) {
                    return;
                }
                for_each_folded(c, |c| {
                    if !(strip_marks && is_attached_mark(c)) {
                        out.borrow_mut().push(c);
                    }
                });
            },
        );
    });
    out.into_inner().out
}

/// Both texts a line is indexed as, in one pass: [`normalize_text_for_indexing`], and
/// [`normalize_vocalized_text_for_indexing`] when the line carries attached marks
/// ([`contains_attached_marks`]) and it is not empty.
pub fn normalize_line_for_indexing(input: &str) -> (String, Option<String>) {
    if !contains_attached_marks(input) {
        return (normalize_one_pass(input, true, |_| false), None);
    }
    let out = std::cell::RefCell::new((
        Collapsed::with_capacity(input.len()),
        Collapsed::with_capacity(input.len()),
    ));
    for_each_html_stripped_piece(input, |piece| {
        for_each_run(
            piece,
            |run| {
                let (plain, vocalized) = &mut *out.borrow_mut();
                plain.push_plain(run);
                vocalized.push_plain(run);
            },
            |c| {
                let (plain, vocalized) = &mut *out.borrow_mut();
                for_each_folded(c, |c| {
                    if !is_attached_mark(c) {
                        plain.push(c);
                    }
                    vocalized.push(c);
                });
            },
        );
    });
    let (plain, vocalized) = out.into_inner();
    let vocalized = (!vocalized.out.is_empty()).then_some(vocalized.out);
    (plain.out, vocalized)
}

/// Fold presentation forms → strip attached marks → collapse whitespace, char by char,
/// over text already stripped of HTML: what [`normalize_one_pass`] and
/// [`normalize_line_for_indexing`] must produce after [`strip_html_for_indexing_reference`].
#[cfg(test)]
pub(crate) fn fold_strip_collapse(
    s: &str,
    strip_marks: bool,
    drop: impl Fn(char) -> bool,
) -> String {
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

/// Asserts that every one-pass normalization of `line` equals its separate passes.
#[cfg(test)]
pub(crate) fn assert_normalization_parity(line: &str) {
    let html = strip_html_for_indexing_reference(line);
    let plain = fold_strip_collapse(&html, true, |_| false);
    let vocalized = contains_attached_marks(line)
        .then(|| fold_strip_collapse(&html, false, |_| false))
        .filter(|v| !v.is_empty());
    assert_eq!(normalize_text_for_indexing(line), plain, "{line:?}");
    assert_eq!(
        normalize_vocalized_text_for_indexing(line),
        fold_strip_collapse(&html, false, |_| false),
        "{line:?}"
    );
    assert_eq!(
        normalize_pdf_text_for_indexing(line),
        fold_strip_collapse(&html, true, is_invisible_char),
        "{line:?}"
    );
    assert_eq!(
        normalize_line_for_indexing(line),
        (plain, vocalized),
        "{line:?}"
    );
}

/// Full ingestion normalisation for text-book lines: strip HTML, decompose
/// presentation forms, strip attached nikud/cantillation, collapse whitespace.
/// Punctuation is intentionally preserved — it is shown in search results.
pub fn normalize_text_for_indexing(input: &str) -> String {
    normalize_one_pass(input, true, |_| false)
}

/// Ingestion normalisation for the vocalized field (`textVocalized`): like
/// [`normalize_text_for_indexing`] but KEEPS attached nikud/cantillation.
/// Applied only to lines that carry at least one mark
/// ([`contains_attached_marks`]); the stored copy is what vocalized search
/// results display.
pub fn normalize_vocalized_text_for_indexing(input: &str) -> String {
    normalize_one_pass(input, false, |_| false)
}

/// Ingestion normalisation for PDF text: like [`normalize_text_for_indexing`]
/// but also drops bidi/zero-width invisibles OCR tends to leave behind
/// ([`is_invisible_char`] — the Dart `_pdfInvisibleChars` set).
pub fn normalize_pdf_text_for_indexing(input: &str) -> String {
    normalize_one_pass(input, true, is_invisible_char)
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
/// * **לא משנים מילה** — אין השמטה והוספה של אותה אות באותו וריאנט (קודש
///   ↛ קדוש); אין השמטת י/ו שלפני ה סופית (מצוה ↛ מצה); השמטה לא משאירה
///   פחות משלוש אותיות, ומילה בת שלוש אותיות מקבלת רק ו אחרי האות הראשונה
///   (כהן → כוהן; שבת ↛ שבות/שבית).
/// * גרש/גרשיים נותרים ניתנים להשמטה בכל מיקום, ללא תקרה (התנהגות קודמת).
///
/// הסדר: המילה המקורית, אחר כך וריאנטים של עריכה אחת (השמטות לפני הוספות,
/// משמאל לימין), אחר כך שתי עריכות, וכן הלאה — כך שקיצוץ תקציב מהסוף משמר
/// תמיד את הצורות הקרובות ביותר למה שהוקלד.
pub(crate) fn generate_spelling_variations(word: &str, limit: usize) -> Vec<String> {
    /// השמטה לעולם לא משאירה פחות אותיות מזה.
    const MIN_SPELLING_LETTERS: usize = 3;
    /// מילה קצרה מזה מקבלת לכל היותר ו אחרי האות הראשונה.
    const MIN_SPELLING_INSERTION_LETTERS: usize = 4;
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
    let letters = chars.iter().filter(|c| !matches!(c, '\'' | '"')).count();

    // מועמדי עריכה בסדר דטרמיניסטי: השמטות משמאל לימין, אחר כך הוספות
    // (לכל מרווח: ו לפני י).
    let mut edits: Vec<Edit> = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        match c {
            'י' | 'ו' => {
                // נציג השמטה אחד לכל רצף: האיבר הפנימי הראשון שלו. י/ו שלפני
                // ה סופית עיצורית (מצוה, שתיה) — השמטתה יוצרת מילה אחרת (מצה).
                let run_start = i == 0 || chars[i - 1] != c;
                if run_start {
                    if let Some(del_at) = (i..n)
                        .take_while(|&j| chars[j] == c)
                        .find(|&j| j > 0 && j + 1 < n)
                        .filter(|&j| !(j + 2 == n && chars[n - 1] == 'ה'))
                    {
                        edits.push(Edit::Del(del_at));
                    }
                }
            }
            '\'' | '"' => edits.push(Edit::Del(i)),
            _ => {}
        }
    }
    // במילה בת שלוש אותיות כמעט כל הוספה יוצרת מילה אחרת (שבת→שבות/שבית);
    // רק ו אחרי האות הראשונה היא כתיב מלא שכיח (כהן→כוהן, קדש→קודש).
    if letters >= MIN_SPELLING_INSERTION_LETTERS {
        for gap in 1..n {
            edits.push(Edit::Ins(gap, 'ו'));
            edits.push(Edit::Ins(gap, 'י'));
        }
    } else if letters == MIN_SPELLING_LETTERS && n == letters {
        edits.push(Edit::Ins(1, 'ו'));
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
        let mut deleted_letters = 0usize;
        let mut gaps: Vec<usize> = Vec::new();
        let mut removed: Vec<char> = Vec::new();
        let mut added: Vec<char> = Vec::new();
        for &i in idx {
            match edits[i] {
                Edit::Del(pos) => {
                    deleted[pos] = true;
                    removed.push(chars[pos]);
                    match chars[pos] {
                        'י' => yod += 1,
                        'ו' => vav += 1,
                        _ => {}
                    }
                    if !matches!(chars[pos], '\'' | '"') {
                        deleted_letters += 1;
                    }
                }
                Edit::Ins(gap, letter) => {
                    gaps.push(gap);
                    added.push(letter);
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
        // השמטה והוספה של אותה אות מזיזות אותה (קודש→קדוש) — מילה אחרת.
        if removed.iter().any(|c| added.contains(c)) {
            return false;
        }
        if letters - deleted_letters < MIN_SPELLING_LETTERS {
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

/// Typo tolerance by word length: no letters to spare in a word of up to
/// two letters (אב would match א, ב, אם…), only the common letter confusions
/// in a three-letter word, and the full edit distance 1 from four letters.
const MIN_TYPO_SUBSTITUTION_LETTERS: usize = 3;
const MIN_TYPO_EDIT_LETTERS: usize = 4;

/// Letters of `word` that a typo may change: quotes and marks not counted.
pub(crate) fn typo_letter_count(word: &str) -> usize {
    word.chars()
        .filter(|&c| !matches!(c, '\'' | '"') && !is_attached_mark(c))
        .count()
}

/// Whether a word gets the full edit-distance-1 neighborhood (the engine's
/// Levenshtein scan or [`generate_typo_variations`]' edits).
pub(crate) fn takes_typo_edits(word: &str) -> bool {
    typo_letter_count(word) >= MIN_TYPO_EDIT_LETTERS
}

/// Typo variants of `word`, original first, capped at `max`: by length (see
/// [`MIN_TYPO_EDIT_LETTERS`]) nothing, the common letter confusions, or the
/// full edit-distance-1 expansion — confusions/transpositions, then single
/// deletions, then single insertions.
fn generate_typo_variations(word: &str, max: usize) -> Vec<String> {
    if word.is_empty() {
        return vec![String::new()];
    }
    let letters = typo_letter_count(word);
    if letters < MIN_TYPO_SUBSTITUTION_LETTERS {
        return vec![word.to_string()];
    }
    let graphemes: Vec<&str> = word.graphemes(true).collect();
    if letters < MIN_TYPO_EDIT_LETTERS {
        let mut out = vec![word.to_string()];
        let mut seen: HashSet<String> = HashSet::from([word.to_string()]);
        for (i, &g) in graphemes.iter().enumerate() {
            for &sub in typo_substitutions(g) {
                let mut v = graphemes.clone();
                v[i] = sub;
                push_unique(&mut out, &mut seen, v.concat());
            }
        }
        out.truncate(max.max(1));
        return out;
    }
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

// ── Affix arms: the one source of the morphology options ──────────────────
//
// Every option that surrounds a root — prefix/suffix windows, grammatical
// prefixes/suffixes, partial word, Aramaic prefixes — becomes `AffixArm`s:
// a regex before the root, the root form, a regex after it. Search patterns
// (plain and vocalized) and the display highlight all render the same arms.
// Windows are bounded (`{0,k}`) so a scan visits a bounded slice of the
// term dictionary.

/// One way a root form may be surrounded. `core` is the literal root form
/// (vocalized on the vocalized path); `lead`/`trail` are regexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AffixArm {
    pub(crate) lead: String,
    pub(crate) core: String,
    pub(crate) trail: String,
}

impl AffixArm {
    fn render(&self, core: impl Fn(&str) -> String) -> String {
        format!("{}{}{}", self.lead, core(&self.core), self.trail)
    }
}

fn user_prefix_window(root_len: usize) -> usize {
    match root_len {
        0 => 0,
        1 => 5,
        2 => 4,
        _ => 3,
    }
}

fn user_suffix_window(root_len: usize) -> usize {
    match root_len {
        0 => 0,
        1 => 7,
        2 => 6,
        _ => 5,
    }
}

fn partial_word_window(root_len: usize) -> usize {
    if root_len <= 3 {
        3
    } else {
        2
    }
}

/// What a prefix/suffix window may add around the root: a Hebrew letter
/// (Yiddish digraphs included) or a geresh/gershayim, in the index's ASCII
/// form — never a digit, a Latin letter or another script.
const AFFIX_LETTER: &str = "[א-תװ-ײ\"']";

/// Digits a window before the root also admits: a footnote number glued to the word
/// (`28שבת`, `154רמב"ם`).
const NOTE_DIGITS: &str = "[0-9]*";

/// A branch's length against the char budget: [`AFFIX_LETTER`] costs as the one `.` it
/// narrows, so narrowing a window does not crowd typo branches out of a phrase word, and
/// [`NOTE_DIGITS`] costs nothing.
fn budget_chars(branch: &str) -> usize {
    branch.chars().count()
        - branch.matches(AFFIX_LETTER).count() * (AFFIX_LETTER.chars().count() - 1)
        - branch.matches(NOTE_DIGITS).count() * NOTE_DIGITS.chars().count()
}

/// Up to `n` letters before or after the root.
fn affix_window(n: usize, voc: bool) -> String {
    affix_window_between(0, n, voc)
}

/// Between `min` and `max` letters. On the vocalized dictionary each unit is
/// a letter plus its attached marks — a window over chars would be spent on
/// marks (three vocalized letters are 6–9 chars).
fn affix_window_between(min: usize, max: usize, voc: bool) -> String {
    if voc {
        format!("(?:{AFFIX_LETTER}{VOC_FREE_MARKS}){{{min},{max}}}")
    } else {
        format!("{AFFIX_LETTER}{{{min},{max}}}")
    }
}

fn prefix_group(first: &[&str], voc: bool) -> String {
    format!(
        "{}{}{}",
        optional_alts_group(first, voc),
        optional_alts_group(GRAM_PREFIX_ALTS_B, voc),
        optional_alts_group(HE_ALTS, voc)
    )
}

fn alternation(parts: Vec<String>) -> String {
    match parts.len() {
        0 => String::new(),
        1 => parts.into_iter().next().unwrap(),
        _ => format!("(?:{})", parts.join("|")),
    }
}

/// Letters of `root`, attached marks not counted.
fn base_len(root: &str) -> usize {
    root.chars().filter(|&c| !is_attached_mark(c)).count()
}

/// The affix arms of one root form under `flags`: the Aramaic prefix arm
/// when asked for, and the arm of the remaining options — prefix and suffix
/// options compose (each side is the union of its selected options).
pub(crate) fn affix_arms(root: &str, flags: &WordFlags, voc: bool) -> Vec<AffixArm> {
    let letters = base_len(root);
    if letters == 0 {
        return Vec::new();
    }
    let abbreviation = root.contains(['\'', '"']);
    let gram_suffix =
        flags.gram_suffix && (abbreviation || letters >= MIN_GRAM_SUFFIX_ROOT_LETTERS);
    let mut arms = Vec::new();
    if flags.aramaic_prefix {
        let lead = prefix_group(ARAMAIC_PREFIX_ALTS_A, voc);
        for (core, trail) in suffix_sides(root, letters, None, gram_suffix, voc) {
            arms.push(AffixArm {
                lead: lead.clone(),
                core,
                trail,
            });
        }
        // The Aramaic arm already covers the bare form (its prefixes are
        // optional); the base arm joins only for an option it lacks.
        if !flags.expands_besides_aramaic() {
            return arms;
        }
    }
    let both_windows = flags.partial || (flags.prefix && flags.suffix);
    let mut leads = Vec::new();
    let mut trail_window = None;
    if both_windows {
        leads.push(format!(
            "{NOTE_DIGITS}{}",
            affix_window(partial_word_window(letters), voc)
        ));
        trail_window = Some(partial_word_window(letters));
    } else {
        if flags.prefix {
            leads.push(format!(
                "{NOTE_DIGITS}{}",
                affix_window(user_prefix_window(letters), voc)
            ));
        }
        if flags.suffix {
            trail_window = Some(user_suffix_window(letters));
        }
    }
    if flags.gram_prefix && (abbreviation || letters >= MIN_GRAM_PREFIX_ROOT_LETTERS) {
        leads.push(prefix_group(GRAM_PREFIX_ALTS_A, voc));
    }
    let lead = alternation(leads);
    for (core, trail) in suffix_sides(root, letters, trail_window, gram_suffix, voc) {
        let arm = AffixArm {
            lead: lead.clone(),
            core,
            trail,
        };
        if !arms.contains(&arm) {
            arms.push(arm);
        }
    }
    arms
}

/// The final-letter forms of the root letters, before a suffix.
fn base_form_of_final(c: char) -> Option<char> {
    match c {
        'ך' => Some('כ'),
        'ם' => Some('מ'),
        'ן' => Some('נ'),
        'ף' => Some('פ'),
        'ץ' => Some('צ'),
        _ => None,
    }
}

/// Byte index and char of the root's last letter (marks skipped).
fn last_letter(root: &str) -> Option<(usize, char)> {
    root.char_indices()
        .rev()
        .find(|&(_, c)| !is_attached_mark(c))
}

/// Past-tense endings suit a verb-sized root (3–4 letters) that does not
/// end in ה or ת, whose past forms change shape (עשה→עשיתי, כרת→כרתי).
fn takes_verb_suffixes(root: &str, letters: usize) -> bool {
    (3..=4).contains(&letters)
        && !root.contains(['\'', '"'])
        && last_letter(root).is_some_and(|(_, c)| c != 'ה' && c != 'ת')
}

/// The `(root form, trail)` pairs of the suffix options. Before a suffix a
/// final letter takes its base form (מלך→מלכים), and a final ה gives way to
/// ת/ות under the grammatical suffixes (תורה→תורתו/תורות, never תורהו).
fn suffix_sides(
    root: &str,
    letters: usize,
    window: Option<usize>,
    gram: bool,
    voc: bool,
) -> Vec<(String, String)> {
    if window.is_none() && !gram {
        return vec![(root.to_string(), String::new())];
    }
    let mut suffixes: Vec<&str> = GRAM_SUFFIX_ALTS.to_vec();
    if takes_verb_suffixes(root, letters) {
        suffixes.extend(VERB_SUFFIX_ALTS);
    }
    let (at, last) = last_letter(root).expect("a root with letters");
    if let Some(base) = base_form_of_final(last) {
        let stem = format!("{}{base}{}", &root[..at], &root[at + last.len_utf8()..]);
        let mut required = Vec::new();
        if let Some(n) = window {
            required.push(affix_window_between(1, n, voc));
        }
        if gram {
            required.push(alts_group(&suffixes, voc));
        }
        return vec![
            (root.to_string(), String::new()),
            (stem, alternation(required)),
        ];
    }
    let window = window.map(|n| affix_window(n, voc));
    if last == 'ה' && gram {
        return vec![
            (root.to_string(), window.unwrap_or_default()),
            (
                root[..at].to_string(),
                format!(
                    "{}{}",
                    alts_group(HE_STEM_ALTS, voc),
                    optional_alts_group(&suffixes, voc)
                ),
            ),
        ];
    }
    let mut trails: Vec<String> = window.into_iter().collect();
    if gram {
        trails.push(optional_alts_group(&suffixes, voc));
    }
    vec![(root.to_string(), alternation(trails))]
}

// ── ארמית: שקילות אות סופית ───────────────────────────────────────────────

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

/// Spelling variants fan through the grammatical arms at a tighter cap than
/// through the plain windows (denser patterns per variant).
fn spelling_limit(flags: &WordFlags) -> usize {
    if flags.gram_prefix || flags.gram_suffix || flags.aramaic_prefix {
        MAX_GRAMMATICAL_SPELLING_BRANCHES
    } else {
        MAX_SPELLING_BRANCHES
    }
}

/// The whole-term pattern of one root under `flags`: every root form —
/// the Aramaic final-letter variants ("סיומות ארמיות"), each fanned out into
/// its כתיב מלא/חסר variants — through its affix arms, as one alternation.
fn word_to_pattern(root: &str, flags: &WordFlags) -> String {
    let variants = if flags.aramaic_suffix {
        aramaic_root_variants(root)
    } else {
        vec![root.to_string()]
    };
    let mut branches: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for variant in &variants {
        let forms = if flags.spelling {
            generate_spelling_variations(variant, spelling_limit(flags))
        } else {
            vec![variant.clone()]
        };
        for form in forms {
            for arm in affix_arms(&form, flags, false) {
                push_unique(&mut branches, &mut seen, arm.render(escape_regex));
            }
        }
    }
    alternation(branches)
}

// ── Vocalized (nikud/te'amim) query patterns ───────────────────────────────
//
// The vocalized field (`textVocalized`) indexes surface forms WITH their
// attached marks, so a vocalized query compiles to a whole-term regex over
// that dictionary. The matching contract is "הוקלד → חייב; לא הוקלד →
// חופשי": every typed mark of an enabled class must appear, in canonical
// order; any other attached mark in the term is free. A base letter therefore becomes
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
/// enabled class are required in canonical order (as the tokenizer indexes
/// them), everything else is free.
pub(crate) fn vocalized_token_pattern(token: &str, voc: &VocalizedFlags) -> String {
    let token = canonical_mark_order(token);
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

/// `(?:a|b|…)` over the alternatives; `voc` interleaves free-mark runs so a
/// vocalized affix (וּ, בְּ…) still matches.
fn alts_group(alts: &[&str], voc: bool) -> String {
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
    format!("(?:{})", inner.join("|"))
}

/// [`alts_group`], optional.
fn optional_alts_group(alts: &[&str], voc: bool) -> String {
    format!("{}?", alts_group(alts, voc))
}

/// Vocalized counterpart of [`word_to_pattern`] for one root form: `text`
/// keeps the marks of `required`'s classes required, every other mark free
/// (pass `VocalizedFlags::default()` for a variant whose letters differ from
/// what was typed). Spelling variants arrive as their own root forms (see
/// [`build_word_regex_vocalized`]).
fn word_to_pattern_vocalized(text: &str, required: &VocalizedFlags, flags: &WordFlags) -> String {
    let mut branches: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for arm in affix_arms(text, flags, true) {
        let branch = arm.render(|core| vocalized_token_pattern(core, required));
        push_unique(&mut branches, &mut seen, branch);
    }
    alternation(branches)
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
    let free = VocalizedFlags::default();
    let push_core = |branches: &mut Vec<String>,
                     seen: &mut HashSet<String>,
                     text: &str,
                     required: &VocalizedFlags| {
        let pattern = word_to_pattern_vocalized(text, required, flags);
        push_unique(branches, seen, pattern);
        branches.len() >= max
    };

    // Pass 0 — exact candidates, typed marks required.
    'exact: for candidate in &candidates {
        if push_core(&mut branches, &mut seen, candidate, voc) {
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
                if push_core(&mut branches, &mut seen, &variant, &free) {
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
                if push_core(&mut branches, &mut seen, &variant, &free) {
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
                if push_core(&mut branches, &mut seen, &variant, &free) {
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
        let len = budget_chars(&b);
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
        let joined_chars = kept.iter().map(|b| budget_chars(b)).sum::<usize>()
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

// ── Relevance tiers ────────────────────────────────────────────────────────

/// How closely a matched index term reproduces what was typed, for ranking
/// an advanced search by relevance: 0 the typed form (or an alternative
/// word) itself, 1 that form inside a longer word (prefixes/suffixes, a final
/// letter in its base form, ה→ת), 2 a כתיב מלא/חסר variant of it (ו/י and
/// quotes aside, affixed or not), 3 anything else the options matched
/// (typos, Aramaic final-letter swaps).
pub(crate) fn match_tier(term: &str, roots: &[String]) -> u8 {
    fn unquoted(s: &str) -> String {
        s.chars().filter(|c| !matches!(c, '\'' | '"')).collect()
    }
    fn skeleton(s: &str) -> String {
        s.chars()
            .filter(|c| !matches!(c, '\'' | '"' | 'ו' | 'י'))
            .map(|c| base_form_of_final(c).unwrap_or(c))
            .collect()
    }
    let roots: Vec<String> = roots
        .iter()
        .map(|r| unquoted(r))
        .filter(|r| !r.is_empty())
        .collect();
    let term = unquoted(term);
    if roots.contains(&term) {
        return 0;
    }
    // The suffixed forms of a root: final letter in its base form (מלכ־),
    // final ה turned to ת/ות (תורת־, תורות).
    let inside = |root: &str| {
        let Some(last) = root.chars().last() else {
            return false;
        };
        let head = &root[..root.len() - last.len_utf8()];
        let stems: Vec<String> = match (base_form_of_final(last), last) {
            (Some(base), _) => vec![format!("{head}{base}")],
            (None, 'ה') => vec![format!("{head}ת"), format!("{head}ות")],
            _ => Vec::new(),
        };
        term.contains(root) || stems.iter().any(|stem| term.contains(stem.as_str()))
    };
    if roots.iter().any(|r| inside(r)) {
        return 1;
    }
    let term_skeleton = skeleton(&term);
    if roots.iter().any(|r| {
        let root = skeleton(r);
        !root.is_empty() && term_skeleton.contains(&root)
    }) {
        return 2;
    }
    3
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
        let len = budget_chars(&b);
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
    // Final safety net (phrase path only): the branch totals passed the
    // budget, but the joined form the exact phrase path / highlight
    // automatons compile also carries `(?:`/`)`/pipe separators. Drop the
    // lowest-priority branches until the joined form fits — never nuke the
    // whole option set. Only a *single* branch that alone exceeds the budget
    // (impossible for the builders above, possible for a hostile string-API
    // pattern) still falls back to the plain literal, which always compiles.
    if let Some(max_chars) = budget.max_pattern_chars {
        let joined_chars = |kept: &[String]| {
            kept.iter().map(|b| budget_chars(b)).sum::<usize>()
                + if kept.len() == 1 {
                    0
                } else {
                    kept.len() - 1 + "(?:)".len()
                }
        };
        while kept.len() > 1 && joined_chars(&kept) > max_chars {
            kept.pop();
        }
        if joined_chars(&kept) > max_chars {
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

/// The proximity of an advanced query whose resolved per-pair allowances are
/// `gaps`. `any_order` holds only without `custom_spacing`, whose per-pair
/// keys presume an order; the search and both highlight builders share this.
pub(crate) fn resolve_proximity(
    gaps: Vec<u32>,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    any_order: bool,
) -> Proximity {
    if any_order && custom_spacing.is_empty() {
        Proximity::any_order(gaps.len() + 1, distance)
    } else {
        Proximity::Ordered(gaps)
    }
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
    // Only words long enough for the full edit distance take the automaton;
    // shorter ones keep the literal (length-limited) variants.
    let typo_literal = typo_tokens.iter().any(|t| !takes_typo_edits(t));
    let typo_tokens: Vec<String> = typo_tokens
        .into_iter()
        .filter(|t| takes_typo_edits(t))
        .collect();

    let regex_terms: Vec<WordPattern> = words
        .iter()
        .enumerate()
        .map(|(i, word)| {
            let mut flags = word_flags_at(&words, i, search_options);
            if !typo_tokens.is_empty() && !typo_literal {
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
/// - **Phrase**: one flat ceiling (see [`PHRASE_MAX_EXPANSIONS`]) that the
///   engine's phrase builder uses as the switch between the exact
///   `RegexPhraseQuery` engine and the term-list degrade path — overflow
///   selects the degrade path instead of erroring.
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
    fn sanitize_keeps_the_second_of_paired_readings() {
        assert_eq!(sanitize_query("הארץ (הוצא) [היצא] אתך"), "הארץ היצא אתך");
        assert_eq!(sanitize_query("(לא) [אפילו] בשביל"), "אפילו בשביל");
        assert_eq!(sanitize_query("כי [אם] עונותיכם"), "כי אם עונותיכם");
        assert_eq!(sanitize_query("הארץ(הוצא) [היצא] אתך"), "הארץ היצא אתך");
        assert_eq!(sanitize_query("אמר(ת) [רבא] משום"), "אמר רבא משום");
        assert_eq!(sanitize_query("בלק (לך) [לכה־]נא"), "בלק לכה נא");
        assert_eq!(sanitize_query("(א)[ב](ג)[ד]"), "ב ד");
        // More than one token in either reading is not a synonymous pair.
        assert_eq!(sanitize_query("(א־ב) [ג]"), "א ב ג");
        assert_eq!(sanitize_query("(א) [ב,ג]"), "א ב ג");
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
    fn strip_html_script_tags_are_word_boundaries() {
        for (html, stripped) in [
            ("תורה<sup>30</sup> המשך", "תורה 30  המשך"),
            ("תורה<sub>30</sub> המשך", "תורה 30  המשך"),
            ("תורה<sup>א</sup> מסוף", "תורה א  מסוף"),
            ("תורה.<sup>30</sup>", "תורה. 30 "),
            ("תורה&nbsp;<sup>30</sup>&nbsp;המשך", "תורה  30  המשך"),
            ("תורה<sup class=\"x\">30</sup>המשך", "תורה 30 המשך"),
            ("תורה<SUP>30</SUP>המשך", "תורה 30 המשך"),
            ("תורה<b><sup>30</sup></b>המשך", "תורה 30 המשך"),
            ("תורה<sup>30המשך", "תורה 30המשך"),
            ("תּוֹרָה<sup>30</sup>הֶמְשֵׁךְ", "תּוֹרָה 30 הֶמְשֵׁךְ"),
            ("תחלה<sup>31</sup>א", "תחלה 31 א"),
            (
                "גוף<sup class=\"footnote-marker\">א</sup><i class=\"footnote\">הרי זה</i>",
                "גוף א הרי זה",
            ),
            // A name that only starts with sup/sub is an inline tag.
            ("א<supx>ב", "אב"),
            ("מי<b>לה</b>", "מילה"),
            // A hanging letter inside a word stays part of it.
            ("מְ<sup>נַ</sup>שֶּׁה", "מְנַשֶּׁה"),
            ("מ<b><sup>נ</sup></b>שה", "מנשה"),
            ("מ<sup>נ</sup><b>שה</b>", "מנשה"),
            ("מ<b>נ</b>שה", "מנשה"),
            ("מ<sup><b>נ</b></sup>שה", "מנשה"),
            ("מ<b><sub><i>נַ</i></sub></b>שה", "מנַשה"),
            ("מ<b><sup>שַׁ</sup></b>ה", "משַׁה"),
            ("מ<b><sup>נ</sub></b>שה", "מ נ שה"),
            ("מ<b><sup>נש</sup></b>ה", "מ נש ה"),
            ("מ<b><sup>נ </sup></b>שה", "מ נ  שה"),
            ("מ<b> <sup>נ</sup></b>שה", "מ  נ שה"),
            ("מ<sup><b>נ<br></b></sup>שה", "מ נ  שה"),
            ("מ<br><sup>נ</sup>שה", "מ  נ שה"),
            ("מ<sup>נ</sup><br>שה", "מ נ  שה"),
            ("מ<b><sup class=\"footnote-marker\">נ</sup></b>שה", "מ נ שה"),
            (
                "מ<b><SUP CLASS='x Footnote-Marker'><i>נ</i></SUP></b>שה",
                "מ נ שה",
            ),
            ("מִיָּ֑<sup>עַ</sup>ר", "מִיָּ֑עַר"),
            ("ו<sup>ג</sup>וש", "וגוש"),
            ("ו<SUB>ג</SUB>וש", "וגוש"),
            ("ו<sup>ג</sub>וש", "ו ג וש"),
            ("ו<sup>גד</sup>וש", "ו גד וש"),
            ("ו <sup>ג</sup>וש", "ו  ג וש"),
            ("ו<sup>ג</sup> וש", "ו ג  וש"),
            // A footnote marker always separates.
            (
                "גוף<sup class=\"footnote-marker\">א</sup>המשך",
                "גוף א המשך",
            ),
            (
                "גוף<SUP CLASS=\"x Footnote-Marker\">א</SUP>המשך",
                "גוף א המשך",
            ),
            ("גוף<sup class=\"footnote-markers\">א</sup>המשך", "גוףאהמשך"),
            // Presentation forms count as the letters they fold to.
            ("\u{FB2A}<sup>נ</sup>ה", "\u{FB2A}נה"),
            ("מ<sup>\u{FB2A}</sup>ה", "מ\u{FB2A}ה"),
            ("מ<sup>נ</sup>\u{FB31}", "מנ\u{FB31}"),
            ("מ<sup>\u{FB4F}</sup>ה", "מ \u{FB4F} ה"),
        ] {
            assert_eq!(strip_html_for_indexing(html), stripped, "{html:?}");
            assert_eq!(
                strip_html_for_indexing_reference(html),
                stripped,
                "{html:?}"
            );
        }
    }

    #[test]
    fn hanging_letter_context_ignores_zero_width_formatting_but_not_separators() {
        let plain = |html: &str| {
            normalize_text_for_indexing(html)
                .chars()
                .filter(|&c| !is_invisible_char(c) && !is_word_mark(c))
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let controls = [
            '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}',
            '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
            '\u{FEFF}', '\u{034F}', '\u{FB1E}',
        ];
        for c in controls {
            for (html, expected) in [
                (format!("מ{c}<sup>נ</sup>שה"), "מנשה"),
                (format!("מ<sup>{c}נ</sup>שה"), "מנשה"),
                (format!("מ<sup>נ{c}</sup>שה"), "מנשה"),
                (format!("מ<sup>נ</sup>{c}שה"), "מנשה"),
                (
                    format!("מ{c}<b>{c}<SUB>{c}<i>{c}נ{c}ַ{c}</i>{c}</SUB>{c}</b>{c}שה"),
                    "מנשה",
                ),
                (format!("מ{c}<sup>שַׁ{c}</sup>{c}ה"), "משה"),
                (format!("מ{c}<sup>נ</sup>{c}ַשה"), "מנשה"),
                (format!("מ{c}<sup>נ{c}ש</sup>ה"), "מ נש ה"),
                (format!("מ{c} <sup>נ</sup>{c}שה"), "מ נ שה"),
                (format!("מ{c}<sup>נ</sup> {c}שה"), "מ נ שה"),
                (format!("מ{c}<br><sup>נ</sup>{c}שה"), "מ נ שה"),
                (format!("מ{c}<sup>נ</sup>{c}<br>שה"), "מ נ שה"),
                (format!("מ{c}<sup>נ{c}<br></sup>שה"), "מ נ שה"),
                (
                    format!("מ{c}<sup class='footnote-marker'>{c}נ</sup>{c}שה"),
                    "מ נ שה",
                ),
                (format!("מ{c}<sup>נ</sub>{c}שה"), "מ נ שה"),
                (format!("מ{c}<sup>נ</sup>{c}"), "מ נ"),
                (format!("{c}ַ<sup>נ</sup>{c}שה"), "נ שה"),
            ] {
                assert_eq!(plain(&html), expected, "{html:?}");
                assert_eq!(
                    strip_html_for_indexing(&html),
                    strip_html_for_indexing_reference(&html),
                    "{html:?}"
                );
                assert_normalization_parity(&html);
            }
        }
        // A general combining mark belongs to the preceding letter, including
        // when a formatting boundary separates it from the base character.
        assert_eq!(plain("מ\u{0307}<sup>נ\u{0307}</sup>\u{0307}שה"), "מנשה");
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

    /// Random lines over markup, entities, marks and presentation forms: the one-pass
    /// normalizations against the separate passes they replace.
    #[test]
    fn one_pass_normalization_matches_the_separate_passes() {
        const PIECES: &[&str] = &[
            "א",
            "שלום",
            " ",
            "  ",
            "\t",
            "\u{00A0}",
            "<",
            ">",
            "<b>",
            "</b>",
            "<br>",
            "<BR/>",
            "</p>",
            "<h2 class=\"x\">",
            "<figcaption>",
            "<span>",
            "<sup>",
            "</sup>",
            "<SUP class=\"footnote-marker\">",
            "<sub>",
            "</sub>",
            "<sup>ב</sup>",
            "מ<b><sup>נ</sup></b>שה",
            "מ<sup><b>נַ</b></sup>שה",
            "מ<br><sup>נ</sup>שה",
            "מ<sup>נ</sub>שה",
            "מ\u{200F}<sup>נ</sup>שה",
            "מ<sup>\u{2067}<b>נ\u{034F}</b></sup>\u{2069}שה",
            "<sup class=\"footnote-marker\">ב</sup>",
            "ב\u{05B7}",
            "&",
            ";",
            "&amp;",
            "&nbsp;",
            "&thinsp;",
            "&ensp;",
            "&emsp;",
            "&nbsp",
            "&;",
            "&&",
            "nbsp;",
            "\u{05B8}",
            "\u{0591}",
            "\u{05BE}",
            "\u{FB2A}",
            "\u{FB1E}",
            "\u{FB4F}",
            "\u{200F}",
            "\u{200E}",
            "\u{2067}",
            "\u{2069}",
            "\u{034F}",
            "\u{FEFF}",
            "a",
            "1",
            "'",
            "\"",
            "/",
            "=",
            "(",
            "[",
        ];
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        for _ in 0..200_000 {
            let len = 1 + next(20);
            let line: String = (0..len).map(|_| PIECES[next(PIECES.len())]).collect();
            assert_eq!(
                strip_html_for_indexing(&line),
                strip_html_for_indexing_reference(&line),
                "{line:?}"
            );
            assert_normalization_parity(&line);
        }
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
    fn mark_combining_class_matches_unicode() {
        use unicode_normalization_alignments::char::canonical_combining_class;
        for cp in (0x0300u32..=0x036F).chain(0x0591..=0x05C7) {
            let c = char::from_u32(cp).unwrap();
            assert_eq!(
                mark_combining_class(c),
                canonical_combining_class(c),
                "U+{cp:04X}"
            );
        }
        assert_eq!(mark_combining_class('א'), 0);
        assert_eq!(mark_combining_class('\u{05BE}'), 0);
    }

    #[test]
    fn canonical_mark_order_sorts_runs_stably() {
        assert_eq!(
            canonical_mark_order("ב\u{05BC}\u{05B8}"),
            "ב\u{05B8}\u{05BC}"
        );
        assert_eq!(
            canonical_mark_order("ש\u{05C1}\u{05BC}\u{05B8}"),
            "ש\u{05B8}\u{05BC}\u{05C1}"
        );
        // Same-class accents (230) keep their order; the dagesh moves before both.
        assert_eq!(
            canonical_mark_order("ב\u{0599}\u{0592}\u{05BC}"),
            "ב\u{05BC}\u{0599}\u{0592}"
        );
        // CGJ is dropped, and the marks around it sort as one run.
        assert_eq!(
            canonical_mark_order("ב\u{05BC}\u{034F}\u{05B8}"),
            "ב\u{05B8}\u{05BC}"
        );
        assert_eq!(
            canonical_mark_order("ל\u{05B7}\u{0594}\u{034F}\u{05B4}ם"),
            "ל\u{05B4}\u{05B7}\u{0594}ם"
        );
        assert_eq!(
            canonical_mark_order("ס\u{0307}\u{0323}"),
            "ס\u{0323}\u{0307}"
        );
        // Each run sorts on its own.
        assert_eq!(
            canonical_mark_order("ב\u{05BC}\u{05B8}ר\u{05B8}ש\u{05C1}\u{05B4}"),
            "ב\u{05B8}\u{05BC}ר\u{05B8}ש\u{05B4}\u{05C1}"
        );
        for sorted in ["", "ברא", "ב\u{05B8}\u{05BC}רָא"] {
            assert!(matches!(canonical_mark_order(sorted), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn vocalized_token_pattern_is_mark_order_insensitive() {
        for voc in [
            VocalizedFlags::new(true, false),
            VocalizedFlags::new(true, true),
        ] {
            let canonical = vocalized_token_pattern("ב\u{05B8}\u{05BC}\u{05A3}רָא", &voc);
            for typed in ["ב\u{05BC}\u{05B8}\u{05A3}רָא", "ב\u{05A3}\u{05BC}\u{05B8}רָא"] {
                assert_eq!(vocalized_token_pattern(typed, &voc), canonical, "{voc:?}");
            }
            assert!(accepts(&canonical, "ב\u{05B8}\u{05BC}\u{05A3}רָא"));
        }
    }

    fn gram_group() -> String {
        prefix_group(GRAM_PREFIX_ALTS_A, false)
    }

    fn aramaic_group() -> String {
        prefix_group(ARAMAIC_PREFIX_ALTS_A, false)
    }

    fn suffix_group() -> String {
        optional_alts_group(GRAM_SUFFIX_ALTS, false)
    }

    /// Whether `pattern` accepts the whole term.
    fn accepts(pattern: &str, term: &str) -> bool {
        use tantivy_fst::Automaton;
        let dfa = crate::term_automaton::compile_pattern(pattern).unwrap();
        let mut state = dfa.start();
        for &b in term.as_bytes() {
            state = dfa.accept(&state, b);
        }
        dfa.is_match(&state)
    }

    fn word_accepts(word: &str, flags: &WordFlags, term: &str) -> bool {
        accepts(&word_to_pattern(word, flags), term)
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
        let options = make_options(&[("בְּרָכָה_0", &[(OPT_TYPO, true)])]);
        let q = prepare_advanced_query_vocalized(
            "בְּרָכָה",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &options,
            &voc,
        );
        // הטוקן לסריקת ה-Levenshtein חוזר נטול סימנים (נסרק מול המילון הרגיל).
        assert_eq!(q.typo_tokens, vec!["ברכה".to_string()]);
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
        let v = generate_spelling_variations("שלום", 100);
        // המקורית תמיד ראשונה, ההשמטה לפני ההוספות.
        assert_eq!(v[0], "שלום");
        assert_eq!(v[1], "שלם");
        // הוספות פנימיות (חסר→מלא) קיימות.
        assert!(v.contains(&"שלוום".to_string()));
        // limit==1 משאיר רק את המקורית — המנייה נעצרת מוקדם.
        assert_eq!(generate_spelling_variations("שלום", 1), vec!["שלום"]);
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
    fn spelling_variations_never_make_another_word() {
        let has = |word: &str, variant: &str| {
            generate_spelling_variations(word, 1000).contains(&variant.to_string())
        };
        // מילה בת שלוש אותיות — בלי השמטות, ומהוספות רק ו אחרי האות הראשונה
        // (כהן→כוהן, קדש→קודש).
        for v in ["שבות", "שבית"] {
            assert!(!has("שבת", v), "{v}");
        }
        assert!(has("שבת", "שובת"));
        assert!(has("כהן", "כוהן"));
        assert!(has("קדש", "קודש"));
        for v in ["כיהן", "כהון", "כוהון", "כויהן"] {
            assert!(!has("כהן", v), "{v}");
        }
        // ארבע אותיות ומעלה — כמו קודם: שלום לא מוצא שולים.
        assert!(!has("שלום", "שולים"));
        assert!(!has("שור", "שר"));
        // י/ו שלפני ה סופית עיצורית.
        assert!(!has("מצוה", "מצה"));
        assert!(!has("שתיה", "שתה"));
        assert!(has("מצוה", "מצווה"));
        // השמטה והוספה של אותה אות.
        assert!(!has("קודש", "קדוש"));
        assert!(has("קודש", "קדש"));
        assert!(has("קדש", "קדש"));
        assert!(has("תפלה", "תפילה"));
        assert!(has("תפילה", "תפלה"));
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
    fn match_tiers_order_exact_affix_spelling_typo() {
        let roots = vec!["תורה".to_string(), "ספר".to_string()];
        for (term, tier) in [
            ("תורה", 0),
            ("ספר", 0),
            ("ובתורה", 1),
            ("תורתו", 1),
            ("תורות", 1),
            ("ספרים", 1),
            ("תוריה", 2),
            ("ובתוריה", 2),
            ("סופר", 2),
            ("תודה", 3),
            ("ספט", 3),
        ] {
            assert_eq!(match_tier(term, &roots), tier, "{term}");
        }
        let roots = vec!["רמב\"ם".to_string(), "מלך".to_string()];
        assert_eq!(match_tier("רמבם", &roots), 0);
        assert_eq!(match_tier("להרמב\"ם", &roots), 1);
        assert_eq!(match_tier("מלכים", &roots), 1);
    }

    #[test]
    fn typo_tolerance_scales_with_word_length() {
        // עד שתי אותיות — אין טעות; שלוש — רק חילופי אותיות מטבלת הבלבול;
        // מארבע — מרחק עריכה 1 מלא.
        assert_eq!(generate_typo_variations("אב", 100), vec!["אב"]);
        let three = generate_typo_variations("שבת", 100);
        for v in ["שבת", "סבת", "שות", "שבט"] {
            assert!(three.contains(&v.to_string()), "{v}");
        }
        for v in ["שב", "שבתי", "בשת", "ששבת"] {
            assert!(!three.contains(&v.to_string()), "{v}");
        }
        let four = generate_typo_variations("ברכה", 500);
        for v in ["ברכ", "רבכה", "בברכה", "ברקה"] {
            assert!(four.contains(&v.to_string()), "{v}");
        }
        // מרחק העריכה במנוע (אוטומט לבנשטיין) רק למילה של ארבע אותיות לפחות;
        // מילה קצרה נשארת עם הווריאנטים המוגבלים.
        let typo = |w: &str| make_options(&[(&format!("{w}_0"), &[(OPT_TYPO, true)])]);
        let q = prepare_advanced_query("אב", 0, &HashMap::new(), &HashMap::new(), &typo("אב"));
        assert!(q.typo_tokens.is_empty());
        assert_eq!(joined_terms(&q), vec!["אב"]);
        let q = prepare_advanced_query("שבת", 0, &HashMap::new(), &HashMap::new(), &typo("שבת"));
        assert!(q.typo_tokens.is_empty());
        assert!(q.regex_terms[0].branches().contains(&"שבט".to_string()));
        let q =
            prepare_advanced_query("ברכות", 0, &HashMap::new(), &HashMap::new(), &typo("ברכות"));
        assert_eq!(q.typo_tokens, vec!["ברכות"]);
        assert_eq!(joined_terms(&q), vec!["ברכות"]);
        // גרשיים אינם נספרים כאותיות.
        assert!(!takes_typo_edits("ר\"ת"));
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
        assert_eq!(p, "(?:ו|מ|דא|כש|כ|ב|ש|ל|ה|ד)?(?:כש|כ|ב|ש|ל|ה|ד)?(?:ה)?ספר");
    }

    #[test]
    fn full_morphology_composes_prefix_and_suffix_groups() {
        // קידומות + סיומות דקדוקיות: אותה קבוצת קידומות כמו האפשרות לבדה,
        // ואחריה קבוצת הסיומות.
        let flags = WordFlags {
            gram_prefix: true,
            gram_suffix: true,
            ..Default::default()
        };
        let p = word_to_pattern("ברכות", &flags);
        assert_eq!(p, format!("{}ברכות{}", gram_group(), suffix_group()));
    }

    #[test]
    fn affix_windows_add_only_hebrew_letters_and_quotes() {
        for flags in [
            WordFlags {
                prefix: true,
                ..Default::default()
            },
            WordFlags {
                suffix: true,
                ..Default::default()
            },
            WordFlags {
                partial: true,
                ..Default::default()
            },
        ] {
            let p = word_to_pattern("שבת", &flags);
            assert!(accepts(&p, "בשבת") || accepts(&p, "שבתו"), "{p}");
            for foreign in ["aשבת", "שבת1", "שבתx", "\u{0301}שבת", "a1שבת"] {
                assert!(!accepts(&p, foreign), "{p} accepted {foreign:?}");
            }
            // מספר הערה צמוד לפני המילה (28שבת) — רק בחלון שלפני השורש.
            assert_eq!(accepts(&p, "28שבת"), !flags.suffix, "{p}");
        }
        let partial = WordFlags {
            partial: true,
            ..Default::default()
        };
        assert!(word_accepts("רמב", &partial, "הרמב\"ם"));
        assert!(word_accepts("שבת", &partial, "מחשבתו"));
        assert!(word_accepts("ברכ", &partial, "448ברכות"));
        let prefixes = WordFlags {
            prefix: true,
            ..Default::default()
        };
        assert!(word_accepts("אמר", &prefixes, "70ואמר"));
        assert!(!word_accepts("אמר", &prefixes, "7a0אמר"));
        // ספרות לא נוספות לקידומות הדקדוקיות.
        let gram = WordFlags {
            gram_prefix: true,
            ..Default::default()
        };
        assert!(!word_accepts("שבת", &gram, "28שבת"));
        // אותו כלל במסלול המנוקד: אות + ריצת סימנים, לא תו כלשהו.
        let voc = word_to_pattern_vocalized("שבת", &VocalizedFlags::default(), &partial);
        assert!(accepts(&voc, "בְּשַׁבָּת"));
        assert!(accepts(&voc, "1שַׁבָּת"));
        assert!(!accepts(&voc, "aשַׁבָּת"));
    }

    #[test]
    fn grammatical_suffixes_need_a_root() {
        let gram = WordFlags {
            gram_suffix: true,
            ..Default::default()
        };
        // שורש קצר משלוש אותיות אינו מקבל סיומות דקדוקיות, וגם לא ה→ת/ות.
        for (word, term) in [("מה", "מות"), ("מה", "מתי"), ("זה", "זות"), ("אב", "אבות")]
        {
            assert!(!word_accepts(word, &gram, term), "{word} {term}");
            assert!(word_accepts(word, &gram, word), "{word}");
        }
        // "ה" לבדה אינה הופכת לשורש ריק.
        for term in ["ת", "בת", "ותו"] {
            assert!(!word_accepts("ה", &gram, term), "{term}");
        }
        assert!(word_accepts("ה", &gram, "ה"));
        assert!(word_accepts("תורה", &gram, "תורתו"));
        assert!(word_accepts("מלך", &gram, "מלכים"));
        // ראשי תיבות פטורים, כמו בקידומות.
        assert!(word_accepts("ר'", &gram, "ר'ים"));
    }

    #[test]
    fn grammatical_prefixes_need_a_root_and_skip_bare_alef() {
        let gram = WordFlags {
            gram_prefix: true,
            ..Default::default()
        };
        // שורש קצר משתי אותיות אינו מקבל קידומות דקדוקיות.
        assert!(!word_accepts("ית", &gram, "בית"));
        assert!(word_accepts("ית", &gram, "ית"));
        // ראשי תיבות פטורים: ור' / להרמב"ם.
        assert!(word_accepts("ר'", &gram, "ור'"));
        assert!(word_accepts("רמב\"ם", &gram, "להרמב\"ם"));
        // א אינה קידומת; ד/דא/ש/כש כן.
        assert!(!word_accepts("שבת", &gram, "אשבת"));
        assert!(!word_accepts("אמר", &gram, "אאמר"));
        for form in ["דאמר", "שאמר", "כשאמר", "ואמר"] {
            assert!(word_accepts("אמר", &gram, form), "{form}");
        }
        // הקידומות הארמיות שומרות את א.
        let aramaic = WordFlags {
            aramaic_prefix: true,
            ..Default::default()
        };
        assert!(word_accepts("שבתא", &aramaic, "אשבתא"));
    }

    #[test]
    fn suffixes_follow_final_letters_and_final_he() {
        let gs = WordFlags {
            gram_suffix: true,
            ..Default::default()
        };
        for (root, forms) in [
            ("מלך", &["מלך", "מלכים", "מלכו", "מלכיהם", "מלכם"][..]),
            ("ארץ", &["ארץ", "ארצות", "ארצו"]),
            (
                "תורה",
                &["תורה", "תורות", "תורתו", "תורתי", "תורותיו", "תורתם"],
            ),
            ("מצוה", &["מצות", "מצוות", "מצותיו"]),
        ] {
            for form in forms {
                assert!(word_accepts(root, &gs, form), "{root} → {form}");
            }
        }
        for (root, form) in [
            ("מלך", "מלךים"),
            ("ארץ", "ארץות"),
            ("תורה", "תורהו"),
            ("תורה", "תורהה"),
        ] {
            assert!(!word_accepts(root, &gs, form), "{root} ↛ {form}");
        }
        // חלון סיומת: גם הוא אחרי האות הלא-סופית.
        let window = WordFlags {
            suffix: true,
            ..Default::default()
        };
        assert!(word_accepts("מלך", &window, "מלכים"));
        assert!(!word_accepts("מלך", &window, "מלךים"));
        // אותו כלל במסלול המנוקד.
        let voc = word_to_pattern_vocalized("מלך", &VocalizedFlags::default(), &gs);
        assert!(accepts(&voc, "מְלָכִים"));
    }

    #[test]
    fn grammatical_morphology_covers_verbs_and_stacked_prefixes() {
        let both = WordFlags {
            gram_prefix: true,
            gram_suffix: true,
            ..Default::default()
        };
        for form in ["אמרתי", "אמרת", "ואמרתם", "אמרתן", "אמרנו", "ואמרו"]
        {
            assert!(word_accepts("אמר", &both, form), "{form}");
        }
        // סיומות פועל רק לשורש בגודל פועל שאינו נגמר ב-ה/ת.
        assert!(!word_accepts("ירושלים", &both, "ירושלימתי"));
        assert!(!word_accepts("שבת", &both, "שבתתי"));
        let gp = WordFlags {
            gram_prefix: true,
            ..Default::default()
        };
        for form in ["כשאמר", "וכשאמר", "ושכשאמר"] {
            assert_eq!(word_accepts("אמר", &gp, form), form != "ושכשאמר", "{form}");
        }
    }

    #[test]
    fn prefix_and_suffix_options_compose() {
        // קידומות (חלון) + סיומות דקדוקיות: אף צד לא נבלע.
        let window_and_gram = WordFlags {
            prefix: true,
            gram_suffix: true,
            ..Default::default()
        };
        assert!(word_accepts("שבת", &window_and_gram, "שבתות"));
        assert!(word_accepts("שבת", &window_and_gram, "בשבתות"));
        assert!(word_accepts("שבת", &window_and_gram, "ובשבת"));
        // ולהפך: קידומות דקדוקיות + סיומות (חלון).
        let gram_and_window = WordFlags {
            gram_prefix: true,
            suffix: true,
            ..Default::default()
        };
        assert!(word_accepts("שבת", &gram_and_window, "בשבת"));
        assert!(word_accepts("שבת", &gram_and_window, "בשבתא"));
        assert!(word_accepts("שבת", &gram_and_window, "שבתות"));
        // קידומות דקדוקיות + חלון קידומת: איחוד — גם קידומת של ארבע אותיות.
        let both_prefixes = WordFlags {
            prefix: true,
            gram_prefix: true,
            ..Default::default()
        };
        assert!(word_accepts("שבת", &both_prefixes, "כשבהשבת"));
        assert!(word_accepts("שבת", &both_prefixes, "אבגשבת"));
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
        let w = affix_window(3, false);
        assert_eq!(p, format!("{NOTE_DIGITS}{w}וו{w}"));
        assert!(
            !p.contains(&format!("{w}{w}")),
            "empty variant widened to wildcard: {p}"
        );
        let q = word_to_pattern("''", &flags);
        assert!(
            !q.contains(&format!("{w}{w}")),
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
        assert_eq!(p, format!("(?:{g}מלכה|{g}מלכא)", g = aramaic_group()));
        // בלי סופית כזו — ענף קידומות יחיד.
        assert_eq!(
            word_to_pattern("ספר", &flags),
            format!("{}ספר", aramaic_group())
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
            format!("{}מלכה", aramaic_group())
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
        assert_eq!(p, format!("(?:{g}מלכה|{g}מלכא)", g = gram_group()));
    }

    #[test]
    fn aramaic_with_gram_suffix_carries_suffixes_on_both_variants() {
        let flags = WordFlags {
            aramaic_prefix: true,
            aramaic_suffix: true,
            gram_suffix: true,
            ..Default::default()
        };
        // הענף הארמי נושא גם את הסיומות — צורה עם קידומת ארמית וגם סיומת
        // לא נופלת בין הענפים, בשני הווריאנטים.
        for form in ["ודמלכה", "ודמלכא", "דמלכתו", "ודמלכאי"] {
            assert!(word_accepts("מלכה", &flags, form), "{form}");
        }
    }

    #[test]
    fn vocalized_aramaic_uses_voc_prefix_group() {
        let flags = WordFlags {
            aramaic_prefix: true,
            ..Default::default()
        };
        let p = word_to_pattern_vocalized("מלכה", &VocalizedFlags::default(), &flags);
        assert!(
            p.starts_with(&prefix_group(ARAMAIC_PREFIX_ALTS_A, true)),
            "{p}"
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
            vec!["(?:ו|מ|דא|כש|כ|ב|ש|ל|ה|ד)?(?:כש|כ|ב|ש|ל|ה|ד)?(?:ה)?ספר"]
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
        assert!(p.contains("(?:ו|מ|דא|כש|כ|ב|ש|ל|ה|ד)?"), "{p}");
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
