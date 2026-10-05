//! Which clause of a semantic hit's line is nearest the query, marked in a snippet centred on it.
//! Outside `crate::api` so flutter_rust_bridge generates no bindings for it.

use lru::LruCache;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::{Mutex, PoisonError};

/// A line of this many words or fewer is shown whole already: nothing to point at.
pub(crate) const SHORT_LINE_WORDS: usize = 12;
/// A part shorter than this is merged with its neighbour.
const MIN_CLAUSE_WORDS: usize = 4;
/// A part longer than this is cut into overlapping windows.
const MAX_CLAUSE_WORDS: usize = 20;
const WINDOW_WORDS: usize = 14;
const WINDOW_STEP: usize = 10;
/// How far past a clause's edge a word of the query still joins its mark.
const EDGE_WORDS: usize = 3;
/// Clauses embedded per line, and per call: each is one inference.
pub(crate) const MAX_CLAUSES_PER_LINE: usize = 6;
pub(crate) const MAX_CLAUSES_PER_CALL: usize = 48;
pub(crate) const CACHE_ENTRIES: usize = 1024;

#[cfg(test)]
thread_local! {
    /// Clauses embedded on this thread: how a test tells a remembered highlight from a new one.
    pub(crate) static EMBEDDED_CLAUSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Characters a clause ends after.
const ENDS_AFTER: [char; 7] = ['.', ':', ';', '?', '!', '׃', ','];

/// The byte ranges of `text`'s words within `range`: whitespace-separated runs holding a letter
/// or a digit, so that a lone punctuation mark is not a word.
fn words(text: &str, range: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    let slice = &text[range.clone()];
    for (offset, c) in slice.char_indices() {
        let at = range.start + offset;
        match (c.is_whitespace(), start) {
            (true, Some(begin)) => {
                out.push(begin..at);
                start = None;
            }
            (false, None) => start = Some(at),
            _ => {}
        }
    }
    if let Some(begin) = start {
        out.push(begin..range.end);
    }
    out.retain(|word| text[word.clone()].chars().any(char::is_alphanumeric));
    out
}

/// `text` cut at its punctuation: after `. : ; ? ! ׃ ,`, before `(`, after `)` and after a dash.
fn raw_parts(text: &str) -> Vec<Range<usize>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut previous: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let next = chars.peek().map(|&(_, c)| c);
        let cut = match c {
            '(' => Some(at),
            ')' | '–' | '—' => Some(at + c.len_utf8()),
            // A hyphen inside a word joins it; only a free-standing one separates.
            '-' if previous.is_none_or(char::is_whitespace)
                || next.is_none_or(char::is_whitespace) =>
            {
                Some(at + c.len_utf8())
            }
            c if ENDS_AFTER.contains(&c) => Some(at + c.len_utf8()),
            _ => None,
        };
        if let Some(cut) = cut {
            if cut > start {
                parts.push(start..cut);
            }
            start = cut;
        }
        previous = Some(c);
    }
    if start < text.len() {
        parts.push(start..text.len());
    }
    parts
}

/// Byte ranges, first word to last, of `text`'s clauses; empty for a line of at most twelve
/// words or of a single clause, where there is nothing to point at.
#[cfg(test)]
pub(crate) fn clauses(text: &str) -> Vec<Range<usize>> {
    clauses_in_parts(text)
        .into_iter()
        .map(|(clause, _)| clause)
        .collect()
}

/// [`clauses`], each with the part it was cut from: a window of a long part is cut by count,
/// not at punctuation.
pub(crate) fn clauses_in_parts(text: &str) -> Vec<(Range<usize>, Range<usize>)> {
    if words(text, 0..text.len()).len() <= SHORT_LINE_WORDS {
        return Vec::new();
    }
    let mut merged: Vec<(Range<usize>, usize)> = Vec::new();
    for part in raw_parts(text) {
        let count = words(text, part.clone()).len();
        match merged.last_mut() {
            Some((last, last_count)) if *last_count < MIN_CLAUSE_WORDS => {
                last.end = part.end;
                *last_count += count;
            }
            _ => merged.push((part, count)),
        }
    }
    if merged.len() > 1 && merged.last().is_some_and(|(_, n)| *n < MIN_CLAUSE_WORDS) {
        let (last, _) = merged.pop().expect("checked above");
        merged.last_mut().expect("checked above").0.end = last.end;
    }

    let mut out = Vec::new();
    for (part, _) in merged {
        let part_words = words(text, part);
        if part_words.is_empty() {
            continue;
        }
        let whole = part_words[0].start..part_words[part_words.len() - 1].end;
        if part_words.len() <= MAX_CLAUSE_WORDS {
            out.push((whole.clone(), whole));
            continue;
        }
        let mut first = 0;
        loop {
            let last = (first + WINDOW_WORDS).min(part_words.len()) - 1;
            out.push((part_words[first].start..part_words[last].end, whole.clone()));
            if last + 1 >= part_words.len() {
                break;
            }
            first += WINDOW_STEP;
        }
    }
    if out.len() < 2 {
        return Vec::new();
    }
    out
}

/// `span` widened, inside `part`, through the nearest [`EDGE_WORDS`] words on each side up to
/// the farthest that `is_query_word` accepts: a window cut by count may end just before one.
pub(crate) fn widen_to_query_words(
    text: &str,
    span: Range<usize>,
    part: Range<usize>,
    is_query_word: impl Fn(&str) -> bool,
) -> Range<usize> {
    let after = words(text, span.end..part.end.max(span.end));
    let end = after
        .iter()
        .take(EDGE_WORDS)
        .rposition(|word| is_query_word(&text[word.clone()]))
        .map_or(span.end, |index| after[index].end);
    let before = words(text, part.start.min(span.start)..span.start);
    let start = before
        .iter()
        .rev()
        .take(EDGE_WORDS)
        .rposition(|word| is_query_word(&text[word.clone()]))
        .map_or(span.start, |index| before[before.len() - 1 - index].start);
    start..end
}

/// The indices of at most `cap` clauses worth embedding, in order: the first clause always, and
/// those that share the most of `wanted` with the query, the earlier on a tie.
pub(crate) fn preselect(
    clause_tokens: &[Vec<String>],
    wanted: &HashSet<String>,
    cap: usize,
) -> Vec<usize> {
    if cap == 0 || clause_tokens.is_empty() {
        return Vec::new();
    }
    let overlap = |tokens: &Vec<String>| tokens.iter().filter(|t| wanted.contains(*t)).count();
    let mut rest: Vec<(usize, usize)> = clause_tokens
        .iter()
        .enumerate()
        .skip(1)
        .map(|(index, tokens)| (index, overlap(tokens)))
        .collect();
    rest.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut chosen: Vec<usize> = std::iter::once(0)
        .chain(rest.into_iter().map(|(index, _)| index))
        .take(cap)
        .collect();
    chosen.sort_unstable();
    chosen
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        aa += f64::from(*x) * f64::from(*x);
        bb += f64::from(*y) * f64::from(*y);
    }
    let norm = (aa * bb).sqrt();
    if norm > 0.0 {
        (dot / norm) as f32
    } else {
        0.0
    }
}

/// The position in `vectors` nearest `query` by cosine, and that cosine; the first on a tie.
pub(crate) fn nearest(query: &[f32], vectors: &[Vec<f32>]) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (index, vector) in vectors.iter().enumerate() {
        let score = cosine(query, vector);
        if best.is_none_or(|(_, top)| score > top) {
            best = Some((index, score));
        }
    }
    best
}

/// `text` cut to `budget` bytes around `span` on word boundaries, HTML-escaped, with the span
/// wrapped in `<mark>` and `…` where the text was cut; `None` if the span alone exceeds it.
pub(crate) fn marked_snippet(text: &str, span: Range<usize>, budget: usize) -> Option<String> {
    let (window, ranges) = crate::api::search_engine::crop_around_first_occurrence(
        text,
        &[(span.start, span.end)],
        1,
        budget,
    )?;
    let &(start, end) = ranges.first()?;
    let offset = window.as_ptr() as usize - text.as_ptr() as usize;
    let mut html = String::with_capacity(window.len() + 32);
    if offset > 0 {
        html.push('…');
    }
    html.push_str(&htmlescape::encode_minimal(&window[..start]));
    html.push_str("<mark>");
    html.push_str(&htmlescape::encode_minimal(&window[start..end]));
    html.push_str("</mark>");
    html.push_str(&htmlescape::encode_minimal(&window[end..]));
    if offset + window.len() < text.len() {
        html.push('…');
    }
    Some(html)
}

/// What a highlight is computed from: the query as normalized for embedding, the line and its
/// text, and the semantic session's epoch, so a new model is never answered from the old one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct HighlightKey {
    pub(crate) epoch: u64,
    pub(crate) query: String,
    pub(crate) file_path: String,
    pub(crate) id: u64,
    pub(crate) text_crc: u32,
}

/// A computed highlight; an empty snippet is a line with nothing to mark.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Highlight {
    pub(crate) snippet_html: String,
    pub(crate) span_score: Option<f32>,
}

impl Highlight {
    pub(crate) fn none() -> Self {
        Self {
            snippet_html: String::new(),
            span_score: None,
        }
    }
}

pub(crate) struct HighlightCache(Mutex<LruCache<HighlightKey, Highlight>>);

impl Default for HighlightCache {
    fn default() -> Self {
        Self(Mutex::new(LruCache::new(
            NonZeroUsize::new(CACHE_ENTRIES).expect("cache size is non-zero"),
        )))
    }
}

impl HighlightCache {
    pub(crate) fn get(&self, key: &HighlightKey) -> Option<Highlight> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    pub(crate) fn put(&self, key: HighlightKey, highlight: Highlight) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .put(key, highlight);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts<'a>(text: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|range| &text[range.clone()]).collect()
    }

    #[test]
    fn a_short_line_has_no_clauses() {
        assert!(clauses("אחת שתים שלש, ארבע חמש שש שבע. שמונה תשע עשר אחת עשרה").is_empty());
        assert!(clauses("").is_empty());
    }

    #[test]
    fn a_line_is_cut_after_hebrew_punctuation_and_at_parentheses_and_dashes() {
        let text =
            "אמר רבי יוחנן הלכה כרבי מאיר׃ ואמר רבי אלעזר בן עזריה בשמו (כדאיתא בברכות דף ב) \
                    ועוד אמרו חכמים זכרונם לברכה – שכל המקיים מצוה זו כראוי; ומה טעם בדבר?";
        assert_eq!(
            texts(text, &clauses(text)),
            [
                "אמר רבי יוחנן הלכה כרבי מאיר׃",
                "ואמר רבי אלעזר בן עזריה בשמו",
                "(כדאיתא בברכות דף ב)",
                "ועוד אמרו חכמים זכרונם לברכה",
                "שכל המקיים מצוה זו כראוי; ומה טעם בדבר?",
            ]
        );
    }

    #[test]
    fn short_parts_merge_forward_and_the_last_backward() {
        let text = "אמר, רבי יוחנן הלכה כרבי מאיר בכל מקום, ואמר רבי אלעזר כן הוא. סוף דבר.";
        assert_eq!(
            texts(text, &clauses(text)),
            [
                "אמר, רבי יוחנן הלכה כרבי מאיר בכל מקום,",
                "ואמר רבי אלעזר כן הוא. סוף דבר.",
            ]
        );
    }

    #[test]
    fn a_hyphen_inside_a_word_does_not_cut() {
        let text = "בן-אדם הולך בדרך הישר והטוב, ושומר את כל המצוות - ואין עוד מלבדו כלל ועיקר";
        assert_eq!(
            texts(text, &clauses(text)),
            [
                "בן-אדם הולך בדרך הישר והטוב,",
                "ושומר את כל המצוות",
                "ואין עוד מלבדו כלל ועיקר",
            ]
        );
    }

    #[test]
    fn a_long_part_becomes_overlapping_windows() {
        let words: Vec<String> = (1..=27).map(|n| format!("מילה{n}")).collect();
        let text = words.join(" ");
        let got = texts(&text, &clauses(&text));
        assert_eq!(
            got,
            [
                words[0..14].join(" "),
                words[10..24].join(" "),
                words[20..27].join(" "),
            ]
        );
    }

    #[test]
    fn a_window_takes_in_a_word_of_the_query_just_past_its_edge() {
        let words: Vec<String> = (1..=27).map(|n| format!("מילה{n}")).collect();
        let text = words.join(" ");
        let found = clauses_in_parts(&text);
        let (first, part) = found[0].clone();
        assert_eq!(part, 0..text.len());
        let query = |wanted: &'static [&'static str]| move |word: &str| wanted.contains(&word);
        let widened = |wanted| {
            texts(
                &text,
                &[widen_to_query_words(
                    &text,
                    first.clone(),
                    part.clone(),
                    query(wanted),
                )],
            )[0]
            .to_string()
        };
        assert_eq!(widened(&["מילה16"]), words[0..16].join(" "));
        assert_eq!(widened(&["מילה15", "מילה17"]), words[0..17].join(" "));
        assert_eq!(widened(&["מילה18"]), words[0..14].join(" "), "too far");
        let (second, _) = found[1].clone();
        let back = widen_to_query_words(&text, second, part, query(&["מילה9"]));
        assert_eq!(&text[back], words[8..24].join(" "));

        let line = "אמר רבי יוחנן הלכה כרבי מאיר בכל מקום, ואמר רבי אלעזר כן הוא ומה טעם בדבר זה.";
        let (clause, part) = clauses_in_parts(line)[0].clone();
        assert_eq!(
            widen_to_query_words(line, clause.clone(), part, query(&["ואמר"])),
            clause,
            "punctuation ends a clause"
        );
    }

    #[test]
    fn a_line_of_one_clause_has_nothing_to_choose() {
        let words: Vec<String> = (1..=16).map(|n| format!("מילה{n}")).collect();
        assert!(clauses(&words.join(" ")).is_empty());
    }

    #[test]
    fn preselection_keeps_the_first_clause_and_the_best_overlaps_in_order() {
        let tokens: Vec<Vec<String>> = [
            &["א", "ב"][..],
            &["שבת", "קודש"],
            &["ג"],
            &["שבת"],
            &["ד"],
            &["קודש", "שבת", "שבת"],
            &["ה"],
            &["ו"],
        ]
        .iter()
        .map(|clause| clause.iter().map(|t| t.to_string()).collect())
        .collect();
        let wanted: HashSet<String> = ["שבת", "קודש"].map(String::from).into();
        assert_eq!(preselect(&tokens, &wanted, 3), [0, 1, 5]);
        assert_eq!(preselect(&tokens, &wanted, 6), [0, 1, 2, 3, 4, 5]);
        assert_eq!(preselect(&tokens, &wanted, 0), [] as [usize; 0]);
    }

    #[test]
    fn the_nearest_vector_wins_and_a_tie_goes_to_the_first() {
        let query = [1.0, 0.0];
        let vectors = vec![
            vec![0.0, 1.0],
            vec![1.0, 1.0],
            vec![2.0, 2.0],
            vec![-1.0, 0.0],
        ];
        let (index, score) = nearest(&query, &vectors).unwrap();
        assert_eq!(index, 1);
        assert!((score - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert_eq!(nearest(&query, &[]), None);
    }

    #[test]
    fn the_snippet_is_centred_escaped_and_cut_on_words() {
        let before = "אלף בית גימל דלת הא וו זין חית טית יוד ".repeat(4);
        let after = " כף למד מם נון סמך עין פא צדי קוף ריש".repeat(4);
        let text = format!("{before}<מצוה> & גדולה{after}");
        let start = before.len();
        let end = start + "<מצוה> & גדולה".len();
        let html = marked_snippet(&text, start..end, 120).unwrap();
        assert!(html.starts_with('…') && html.ends_with('…'), "{html}");
        assert!(
            html.contains("<mark>&lt;מצוה&gt; &amp; גדולה</mark>"),
            "{html}"
        );
        // Whole words at both cuts.
        let inner = html.trim_matches('…');
        let shown = inner.replace("<mark>", "").replace("</mark>", "");
        let shown = shown
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        assert!(text.contains(&shown), "{shown}");
        let first = shown.split(' ').next().unwrap();
        let last = shown.split(' ').next_back().unwrap();
        assert!(text.split(' ').any(|w| w == first), "{first}");
        assert!(text.split(' ').any(|w| w == last), "{last}");
        // Centred: the two sides differ by about a word at most.
        let (left, right) = inner.split_once("<mark>").unwrap();
        let right = right.split_once("</mark>").unwrap().1;
        assert!(left.len().abs_diff(right.len()) <= 20, "{left} | {right}");

        assert_eq!(
            marked_snippet("קצר מאוד", 0.."קצר".len(), 800).unwrap(),
            "<mark>קצר</mark> מאוד"
        );
        assert_eq!(marked_snippet(&text, start..end, 8), None);
    }

    #[test]
    fn the_cache_answers_a_key_it_holds() {
        let cache = HighlightCache::default();
        let key = HighlightKey {
            epoch: 0,
            query: "שבת".into(),
            file_path: "/b".into(),
            id: 1,
            text_crc: 7,
        };
        assert_eq!(cache.get(&key), None);
        cache.put(key.clone(), Highlight::none());
        assert_eq!(cache.get(&key), Some(Highlight::none()));
        let other = HighlightKey { text_crc: 8, ..key };
        assert_eq!(cache.get(&other), None);
    }
}
