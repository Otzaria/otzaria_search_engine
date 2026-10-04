//! Phrases that continue from one line to the next (issue #1703).
//!
//! Every line of a book is its own document, so a phrase whose words sit at
//! the end of one line and the start of the next was never found. This module
//! finds it without indexing the words around the line break a second time:
//!
//! - **Index side** — each line records where its content starts and ends
//!   (`lineFirst`/`lineLast`: the positions of its first and last content
//!   words, skipping enumerators such as `(ג)` or `{פ}`), plus the terms of
//!   those two words in the small `lineEdge` field (`<word` / `>word`).
//!   Headings, empty lines and lines next to a dropped PDF line get no entry,
//!   so a phrase never continues across them.
//! - **Query side** — [`CrossLineQuery`] tries every split of the phrase into
//!   a left part that ends line `L` and a right part that starts line `L + 1`.
//!   Each part is matched on its own, anchored to its line edge; the two sets
//!   are joined on the document `id`, which is consecutive within a book. A
//!   match is attributed to `L`. With no allowance across the break, the
//!   `lineEdge` terms drive each part, so even very common words cost only the
//!   lines that start or end with them; with an allowance, the part's rarest
//!   word drives a positional scan.
//!
//! The query is a plain `Query`/`Weight`/`Scorer`: it evaluates its join once
//! per weight (the join needs every segment) and serves each segment's matches
//! as a constant-score doc set, so counting, collecting, grouping and boolean
//! composition see an ordinary doc set. A phrase crosses at most one line break.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Range;
use std::sync::Arc;

use tantivy::columnar::Column;
use tantivy::index::SegmentId;
use tantivy::postings::SegmentPostings;
use tantivy::query::{ConstScorer, EmptyScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::schema::{Field, IndexRecordOption, Schema};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{
    DocId, DocSet, InvertedIndexReader, Score, Searcher, SegmentReader, TantivyDocument, Term,
    TERMINATED,
};

use crate::gap_phrase::{end_anchored_slack, start_anchored_slack, ChainSweep, WordPositions};
use crate::hebrew_tokenizer::{next_token_boundaries, paired_reading_after};

/// Position of a line's first content word, plus one; absent (or 0) when no
/// phrase may continue into the line from the one before.
pub(crate) const LINE_FIRST_FIELD: &str = "lineFirst";
/// Position of a line's last content word, plus one; absent (or 0) when no
/// phrase may continue from the line onto the next.
pub(crate) const LINE_LAST_FIELD: &str = "lineLast";
/// `<term` for the terms of a line's first content word, `>term` for its last.
pub(crate) const LINE_EDGE_FIELD: &str = "lineEdge";
/// Joins the two lines of a cross-line snippet; the painted HTML shows it as `<br>`.
pub(crate) const SNIPPET_LINE_BREAK: &str = "\n";
/// [`SNIPPET_LINE_BREAK`] in the snippet HTML.
pub(crate) const SNIPPET_LINE_BREAK_HTML: &str = "<br>";

const START_PREFIX: &str = "<";
const END_PREFIX: &str = ">";
/// A seam hit scores this share of the in-line score of one occurrence on a
/// line of average length, so it ranks below typical in-line hits.
const SCORE_FACTOR: Score = 0.5;
/// When one part of a gapped split has at most this many matches, the other
/// part is checked only on their neighbour lines, found by id.
const LOOKUP_LIMIT: usize = 4_096;
/// A positional scan does not split a segment into ranges smaller than this.
/// Tests split every few docs, so their small indexes cross range edges too.
const SCAN_CHUNK_MIN_DOCS: u32 = if cfg!(test) { 2 } else { 32_768 };
/// Doc-frequency terms read per segment to score a pattern word.
const SCORE_TERMS_PER_SEGMENT: usize = 256;

// ── Line edges (index side) ───────────────────────────────────────────────────

fn is_marker_char(c: char) -> bool {
    ('\u{05D0}'..='\u{05EA}').contains(&c)
        || c.is_ascii_digit()
        || matches!(c, '\'' | '"' | '\u{05F3}' | '\u{05F4}' | '*')
}

/// End of a tokenizer-recognized `(X) [Y]` pair whose opening `(` is at
/// `open`. Use the same parser as the tokenizer so either reading remains a
/// content word, even when one looks like a short Hebrew enumerator.
fn reading_pair_end(s: &str, open: usize) -> Option<usize> {
    let (start, end) = next_token_boundaries(s, open + 1)?;
    if start != open + 1 {
        return None;
    }
    paired_reading_after(s, start, end).map(|pair| pair.after_pair)
}

/// Byte length of a leading enumerator such as `(יא) `, `[ג]` or `{פ}`,
/// including the whitespace after it; 0 when the line has none.
fn leading_marker_len(s: &str) -> usize {
    let t = s.trim_start();
    let skipped = s.len() - t.len();
    let mut chars = t.char_indices();
    let close = match chars.next() {
        Some((_, '(')) => ')',
        Some((_, '[')) => ']',
        Some((_, '{')) => '}',
        _ => return 0,
    };
    if close == ')' && reading_pair_end(t, 0).is_some() {
        return 0;
    }
    for (n, (i, c)) in chars.enumerate() {
        if c == close {
            if n == 0 || n > 5 {
                return 0;
            }
            let end = i + c.len_utf8();
            let rest = &t[end..];
            return skipped + end + (rest.len() - rest.trim_start().len());
        }
        if !is_marker_char(c) {
            return 0;
        }
    }
    0
}

/// Byte offset where a trailing enumerator such as `{פ}` or `(א):` begins;
/// `s.len()` when the line has none.
fn trailing_marker_start(s: &str) -> usize {
    let t = s.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '.' | ',' | '׃'));
    let mut chars = t.char_indices().rev();
    let open = match chars.next() {
        Some((_, ')')) => '(',
        Some((_, ']')) => '[',
        Some((_, '}')) => '{',
        _ => return s.len(),
    };
    for (n, (i, c)) in chars.enumerate() {
        if c == open {
            if n == 0 || n > 5 {
                return s.len();
            }
            // A final `[Y]` can be the second reading of `(X) [Y]`, rather
            // than a number. Inspect only its immediately preceding group;
            // ordinary trailing markers do not require a full token scan.
            if open == '[' {
                if let Some(before_close) = t[..i].trim_end().strip_suffix(')') {
                    if let Some(pair_open) = before_close.rfind('(') {
                        if reading_pair_end(t, pair_open) == Some(t.len()) {
                            return s.len();
                        }
                    }
                }
            }
            return t[..i].trim_end().len();
        }
        if !is_marker_char(c) {
            return s.len();
        }
    }
    s.len()
}

/// The part of a normalized line a phrase may continue from or into: the line
/// without its leading and trailing enumerators.
pub(crate) fn content_range(plain: &str) -> Range<usize> {
    let start = leading_marker_len(plain);
    start..trailing_marker_start(plain).max(start)
}

/// Where a line's content starts and ends, as `lineFirst`/`lineLast` and the
/// `lineEdge` terms record it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LineEdges {
    pub first: u32,
    pub last: u32,
    start_terms: Vec<String>,
    end_terms: Vec<String>,
}

/// The edges of a normalized line, or `None` when it has no content word.
/// `analyzer` must be the `text` field's indexing analyzer: the edge terms are
/// the terms it indexes the first and last content words as.
pub(crate) fn line_edges(analyzer: &mut TextAnalyzer, plain: &str) -> Option<LineEdges> {
    let content = content_range(plain);
    // Positions as the tokenizer assigns them, without building token texts:
    // only a paired reading `(X) [Y]` puts two words on one position.
    let mut byte = 0usize;
    let mut count = 0u32;
    let mut share = false;
    let mut first: Option<(u32, Range<usize>)> = None;
    let mut last: Option<(u32, Range<usize>)> = None;
    while let Some((start, end)) = next_token_boundaries(plain, byte) {
        let position = if share { count - 1 } else { count };
        if !share {
            count += 1;
        }
        share = paired_reading_after(plain, start, end).is_some();
        byte = end;
        if start < content.start || end > content.end {
            continue;
        }
        match &mut first {
            None => first = Some((position, start..end)),
            Some((p, span)) if *p == position => span.end = end,
            _ => {}
        }
        match &mut last {
            Some((p, span)) if *p == position => span.end = end,
            _ => last = Some((position, start..end)),
        }
    }
    let (first, first_span) = first?;
    let (last, last_span) = last?;
    let mut terms = |span: Range<usize>| {
        let mut out = Vec::new();
        let mut stream = analyzer.token_stream(&plain[span]);
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    };
    let start_terms = terms(first_span);
    let end_terms = terms(last_span);
    Some(LineEdges {
        first,
        last,
        start_terms,
        end_terms,
    })
}

/// The three edge fields of the schema.
#[derive(Clone, Copy)]
pub(crate) struct EdgeFields {
    first: Field,
    last: Field,
    edge: Field,
}

impl EdgeFields {
    pub(crate) fn from_schema(schema: &Schema) -> tantivy::Result<Self> {
        Ok(Self {
            first: schema.get_field(LINE_FIRST_FIELD)?,
            last: schema.get_field(LINE_LAST_FIELD)?,
            edge: schema.get_field(LINE_EDGE_FIELD)?,
        })
    }

    /// Records `edges` on the line's document. `can_start`: a phrase may
    /// continue into this line from the previous one; `can_end`: from this
    /// line onto the next.
    pub(crate) fn add(
        &self,
        document: &mut TantivyDocument,
        edges: &LineEdges,
        can_start: bool,
        can_end: bool,
    ) {
        if can_start {
            document.add_u64(self.first, u64::from(edges.first) + 1);
            for term in &edges.start_terms {
                document.add_text(self.edge, format!("{START_PREFIX}{term}"));
            }
        }
        if can_end {
            document.add_u64(self.last, u64::from(edges.last) + 1);
            for term in &edges.end_terms {
                document.add_text(self.edge, format!("{END_PREFIX}{term}"));
            }
        }
    }
}

/// `left` (line `L`) and `right` (line `L + 1`) joined for a snippet: the
/// content of each, with [`SNIPPET_LINE_BREAK`] between them at the returned
/// byte range.
pub(crate) fn joined_lines(left: &str, right: &str) -> (String, Range<usize>) {
    let left = &left[..content_range(left).end];
    let right = &right[content_range(right).start..];
    let mut joined = String::with_capacity(left.len() + SNIPPET_LINE_BREAK.len() + right.len());
    joined.push_str(left.trim_end());
    let break_start = joined.len();
    joined.push_str(SNIPPET_LINE_BREAK);
    let break_end = joined.len();
    joined.push_str(right.trim_start());
    (joined, break_start..break_end)
}

// ── Query ────────────────────────────────────────────────────────────────────

/// One word of a cross-line phrase.
#[derive(Clone, Debug)]
pub(crate) enum CrossLineWord {
    /// The `text` index terms the word matches.
    Terms(Vec<String>),
    /// A full-match regex over `text` index terms (the advanced search's
    /// joined word pattern).
    Pattern(String),
}

#[derive(Clone, Debug)]
enum WordSource {
    Terms(Vec<Term>),
    Regex(Arc<tantivy_fst::Regex>),
}

impl WordSource {
    fn postings(
        &self,
        inverted: &InvertedIndexReader,
        option: IndexRecordOption,
    ) -> tantivy::Result<Vec<SegmentPostings>> {
        let mut out = Vec::new();
        match self {
            WordSource::Terms(terms) => {
                for term in terms {
                    if let Some(info) = inverted.get_term_info(term)? {
                        out.push(inverted.read_postings_from_terminfo(&info, option)?);
                    }
                }
            }
            WordSource::Regex(regex) => {
                let mut stream = inverted.terms().search(regex.as_ref()).into_stream()?;
                while stream.advance() {
                    out.push(inverted.read_postings_from_terminfo(stream.value(), option)?);
                }
            }
        }
        Ok(out)
    }

    /// Documents that hold the word, summed over its terms (capped for patterns).
    fn doc_freq(&self, inverted: &InvertedIndexReader) -> tantivy::Result<u64> {
        let mut total = 0u64;
        match self {
            WordSource::Terms(terms) => {
                for term in terms {
                    total += inverted.doc_freq(term)? as u64;
                }
            }
            WordSource::Regex(regex) => {
                let mut stream = inverted.terms().search(regex.as_ref()).into_stream()?;
                let mut seen = 0;
                while seen < SCORE_TERMS_PER_SEGMENT && stream.advance() {
                    total += stream.value().doc_freq as u64;
                    seen += 1;
                }
            }
        }
        Ok(total)
    }
}

#[derive(Clone, Debug)]
struct CompiledWord {
    text: WordSource,
    starts: WordSource,
    ends: WordSource,
}

impl CompiledWord {
    fn new(word: &CrossLineWord, text: Field, edge: Field) -> Result<Option<Self>, String> {
        let regex = |pattern: String| {
            tantivy_fst::Regex::new(&pattern)
                .map(|r| WordSource::Regex(Arc::new(r)))
                .map_err(|e| format!("invalid cross-line pattern {pattern:?}: {e}"))
        };
        Ok(Some(match word {
            CrossLineWord::Terms(terms) if terms.is_empty() => return Ok(None),
            CrossLineWord::Terms(terms) => {
                let with = |prefix: &str| {
                    WordSource::Terms(
                        terms
                            .iter()
                            .map(|t| Term::from_field_text(edge, &format!("{prefix}{t}")))
                            .collect(),
                    )
                };
                CompiledWord {
                    text: WordSource::Terms(
                        terms
                            .iter()
                            .map(|t| Term::from_field_text(text, t))
                            .collect(),
                    ),
                    starts: with(START_PREFIX),
                    ends: with(END_PREFIX),
                }
            }
            CrossLineWord::Pattern(pattern) => CompiledWord {
                text: regex(pattern.clone())?,
                starts: regex(format!("{START_PREFIX}(?:{pattern})"))?,
                ends: regex(format!("{END_PREFIX}(?:{pattern})"))?,
            },
        }))
    }
}

/// A phrase that continues from the end of a line onto the start of the next
/// one. See the module docs.
#[derive(Clone, Debug)]
pub(crate) struct CrossLineQuery {
    text: Field,
    edge: Field,
    id: Field,
    words: Vec<CompiledWord>,
    /// `gaps[i]` = allowed intermediate words between words `i` and `i + 1`;
    /// across the line break the words of both lines count.
    gaps: Vec<u32>,
}

impl CrossLineQuery {
    /// `None` for fewer than two words, or when a word matches no term.
    pub(crate) fn new(
        schema: &Schema,
        text: Field,
        words: &[CrossLineWord],
        gaps: &[u32],
    ) -> anyhow::Result<Option<Self>> {
        if words.len() < 2 || gaps.len() + 1 != words.len() {
            return Ok(None);
        }
        let edge = schema.get_field(LINE_EDGE_FIELD)?;
        let id = schema.get_field("id")?;
        let mut compiled = Vec::with_capacity(words.len());
        for word in words {
            match CompiledWord::new(word, text, edge).map_err(anyhow::Error::msg)? {
                Some(word) => compiled.push(word),
                None => return Ok(None),
            }
        }
        Ok(Some(Self {
            text,
            edge,
            id,
            words: compiled,
            gaps: gaps.to_vec(),
        }))
    }

    /// The matching lines of every segment, keyed by segment.
    fn evaluate(
        &self,
        searcher: &Searcher,
    ) -> tantivy::Result<HashMap<SegmentId, Arc<Vec<DocId>>>> {
        let segments: Vec<Segment> = searcher
            .segment_readers()
            .iter()
            .enumerate()
            .map(|(ordinal, reader)| Segment::open(ordinal, reader, self))
            .collect::<tantivy::Result<_>>()?;
        let mut hits: Vec<Vec<DocId>> = vec![Vec::new(); segments.len()];
        let mut scratch = Scratch::default();
        for split in 1..self.words.len() {
            let allowance = self.gaps[split - 1];
            let left = Part {
                words: &self.words[..split],
                gaps: &self.gaps[..split - 1],
                side: Side::EndsLine,
            };
            let right = Part {
                words: &self.words[split..],
                gaps: &self.gaps[split..],
                side: Side::StartsLine,
            };
            for (segment, doc) in
                self.join_split(&segments, &left, &right, allowance, &mut scratch)?
            {
                hits[segment].push(doc);
            }
        }
        Ok(segments
            .iter()
            .zip(hits)
            .map(|(segment, mut docs)| {
                docs.sort_unstable();
                docs.dedup();
                (segment.reader.segment_id(), Arc::new(docs))
            })
            .collect())
    }

    /// Lines `L` (as segment ordinal and doc) where `left` ends `L` and
    /// `right` starts `L + 1` with at most `allowance` words between them.
    fn join_split(
        &self,
        segments: &[Segment],
        left: &Part,
        right: &Part,
        allowance: u32,
        scratch: &mut Scratch,
    ) -> tantivy::Result<Vec<(usize, DocId)>> {
        let mut out = Vec::new();
        if allowance == 0 {
            let starts = self.scan(segments, right, 0, Candidates::Edge)?;
            if starts.is_empty() {
                return Ok(out);
            }
            let starts: HashMap<u64, u32> = starts.iter().map(|m| (m.id, m.slack)).collect();
            for m in self.scan(segments, left, 0, Candidates::Edge)? {
                if starts.contains_key(&(m.id + 1)) {
                    out.push((m.segment, m.doc));
                }
            }
            return Ok(out);
        }
        // The part whose rarest word is rarer goes first; its matches either
        // drive id lookups of their neighbours or meet a second scan.
        let (first, second, neighbour): (&Part, &Part, i64) =
            if self.driver_freq(segments, left)? < self.driver_freq(segments, right)? {
                (left, right, 1)
            } else {
                (right, left, -1)
            };
        let first_matches = self.scan(segments, first, allowance, Candidates::Driver)?;
        if first_matches.is_empty() {
            return Ok(out);
        }
        let second_matches = if first_matches.len() <= LOOKUP_LIMIT {
            let mut per_segment: Vec<Vec<DocId>> = vec![Vec::new(); segments.len()];
            for m in &first_matches {
                let id = m.id.wrapping_add_signed(neighbour);
                if let Some((segment, doc)) = self.find_line(segments, id)? {
                    per_segment[segment].push(doc);
                }
            }
            let mut matches = Vec::new();
            for (segment, mut docs) in per_segment.into_iter().enumerate() {
                if docs.is_empty() {
                    continue;
                }
                docs.sort_unstable();
                docs.dedup();
                matches.extend(self.match_part(
                    &segments[segment],
                    second,
                    allowance,
                    Candidates::Docs(docs),
                    0..DocId::MAX,
                    scratch,
                )?);
            }
            matches
        } else {
            self.scan(segments, second, allowance, Candidates::Driver)?
        };
        let (ends, starts) = if neighbour == 1 {
            (first_matches, second_matches)
        } else {
            (second_matches, first_matches)
        };
        let starts: HashMap<u64, u32> = starts.iter().fold(HashMap::new(), |mut map, m| {
            let slot = map.entry(m.id).or_insert(m.slack);
            *slot = (*slot).min(m.slack);
            map
        });
        for m in ends {
            if starts
                .get(&(m.id + 1))
                .is_some_and(|&slack| slack + m.slack <= allowance)
            {
                out.push((m.segment, m.doc));
            }
        }
        Ok(out)
    }

    /// [`Self::match_part`] over every segment, in parallel. A positional scan
    /// also splits each segment into doc ranges, so one large segment does not
    /// run on a single core.
    fn scan(
        &self,
        segments: &[Segment],
        part: &Part,
        allowance: u32,
        candidates: Candidates,
    ) -> tantivy::Result<Vec<PartMatch>> {
        use rayon::prelude::*;
        let chunks = if matches!(candidates, Candidates::Driver) {
            rayon::current_num_threads().max(1) * 2
        } else {
            1
        };
        let tasks: Vec<(&Segment, std::ops::Range<DocId>)> = segments
            .iter()
            .flat_map(|segment| {
                let max_doc = segment.reader.max_doc();
                let step = max_doc.div_ceil(chunks as u32).max(SCAN_CHUNK_MIN_DOCS);
                (0..max_doc)
                    .step_by(step as usize)
                    .map(move |start| (segment, start..(start + step).min(max_doc)))
            })
            .collect();
        let per_task: Vec<Vec<PartMatch>> = tasks
            .into_par_iter()
            .map(|(segment, range)| {
                self.match_part(
                    segment,
                    part,
                    allowance,
                    candidates.clone(),
                    range,
                    &mut Scratch::default(),
                )
            })
            .collect::<tantivy::Result<_>>()?;
        Ok(per_task.into_iter().flatten().collect())
    }

    /// Documents holding the part's rarest word, over all segments.
    fn driver_freq(&self, segments: &[Segment], part: &Part) -> tantivy::Result<u64> {
        let mut rarest = u64::MAX;
        for word in part.words {
            let mut freq = 0u64;
            for segment in segments {
                freq += word.text.doc_freq(&segment.text)?;
            }
            rarest = rarest.min(freq);
        }
        Ok(rarest)
    }

    /// The lines of one segment, within `range`, where `part` sits at its line
    /// edge, at most `allowance` words away from it.
    fn match_part(
        &self,
        segment: &Segment,
        part: &Part,
        allowance: u32,
        candidates: Candidates,
        range: std::ops::Range<DocId>,
        scratch: &mut Scratch,
    ) -> tantivy::Result<Vec<PartMatch>> {
        let mut out = Vec::new();
        let boundary = match part.side {
            Side::EndsLine => part.words.last(),
            Side::StartsLine => part.words.first(),
        }
        .expect("a part has at least one word");
        // The edge term already places a lone word at its line edge.
        let needs_positions =
            part.words.len() > 1 || allowance > 0 || !matches!(candidates, Candidates::Edge);
        let docs: Box<dyn Iterator<Item = DocId>> = match candidates {
            Candidates::Edge => {
                let source = match part.side {
                    Side::EndsLine => &boundary.ends,
                    Side::StartsLine => &boundary.starts,
                };
                let mut docs = Vec::new();
                for mut postings in source.postings(&segment.edge, IndexRecordOption::Basic)? {
                    let mut doc = postings.doc();
                    while doc != TERMINATED {
                        docs.push(doc);
                        doc = postings.advance();
                    }
                }
                docs.sort_unstable();
                docs.dedup();
                Box::new(docs.into_iter())
            }
            Candidates::Driver => {
                let mut rarest: Option<(u64, &CompiledWord)> = None;
                for word in part.words {
                    let freq = word.text.doc_freq(&segment.text)?;
                    if rarest.is_none_or(|(best, _)| freq < best) {
                        rarest = Some((freq, word));
                    }
                }
                let (_, word) = rarest.expect("a part has at least one word");
                let mut postings = word
                    .text
                    .postings(&segment.text, IndexRecordOption::Basic)?;
                for posting in &mut postings {
                    if posting.doc() < range.start {
                        posting.seek(range.start);
                    }
                }
                Box::new(DocUnion::new(postings))
            }
            Candidates::Docs(docs) => Box::new(docs.into_iter()),
        };
        let end = range.end;
        let docs = docs.take_while(move |&doc| doc < end);
        let mut words: Vec<WordPositions> = Vec::new();
        if needs_positions {
            for word in part.words {
                let postings = word
                    .text
                    .postings(&segment.text, IndexRecordOption::WithFreqsAndPositions)?;
                if postings.is_empty() {
                    return Ok(out);
                }
                words.push(WordPositions::new(postings));
            }
        }
        scratch.positions.resize_with(part.words.len(), Vec::new);
        for doc in docs {
            if !segment.is_alive(doc) {
                continue;
            }
            let anchor = match part.side {
                Side::EndsLine => segment.last.first(doc),
                Side::StartsLine => segment.first.first(doc),
            };
            let Some(anchor) = anchor.filter(|&v| v > 0).map(|v| (v - 1) as u32) else {
                continue;
            };
            let slack = if words.is_empty() {
                Some(0)
            } else {
                let mut present = true;
                for (word, positions) in words.iter_mut().zip(scratch.positions.iter_mut()) {
                    word.positions_in(doc, positions);
                    if positions.is_empty() {
                        present = false;
                        break;
                    }
                }
                if !present {
                    continue;
                }
                let positions = &scratch.positions[..part.words.len()];
                match part.side {
                    Side::EndsLine => end_anchored_slack(
                        &mut scratch.sweep,
                        positions,
                        part.gaps,
                        anchor,
                        allowance,
                    ),
                    Side::StartsLine => start_anchored_slack(
                        &mut scratch.sweep,
                        positions,
                        part.gaps,
                        anchor,
                        allowance,
                    ),
                }
            };
            let Some(slack) = slack else {
                continue;
            };
            let Some(id) = segment.id.first(doc) else {
                continue;
            };
            out.push(PartMatch {
                segment: segment.ordinal,
                doc,
                id,
                slack,
            });
        }
        Ok(out)
    }

    /// The live document with `id`, as segment ordinal and doc.
    fn find_line(&self, segments: &[Segment], id: u64) -> tantivy::Result<Option<(usize, DocId)>> {
        let term = Term::from_field_u64(self.id, id);
        for segment in segments {
            let Some(mut postings) = segment.ids.read_postings(&term, IndexRecordOption::Basic)?
            else {
                continue;
            };
            let mut doc = postings.doc();
            while doc != TERMINATED {
                if segment.is_alive(doc) {
                    return Ok(Some((segment.ordinal, doc)));
                }
                doc = postings.advance();
            }
        }
        Ok(None)
    }

    /// Half the BM25 score of one in-line occurrence on a line of average
    /// length: the sum of the words' idf.
    fn score(&self, searcher: &Searcher) -> tantivy::Result<Score> {
        let total_docs = searcher.num_docs().max(1) as f64;
        let mut idf_sum = 0.0f64;
        for word in &self.words {
            let mut freq = 0u64;
            for reader in searcher.segment_readers() {
                let inverted = reader.inverted_index(self.text)?;
                freq += word.text.doc_freq(&inverted)?;
            }
            let freq = (freq as f64).min(total_docs);
            idf_sum += (1.0 + (total_docs - freq + 0.5) / (freq + 0.5)).ln();
        }
        Ok((idf_sum as Score * SCORE_FACTOR).max(Score::MIN_POSITIVE))
    }
}

impl Query for CrossLineQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        // The join needs every segment; a weight built from the schema alone
        // (no searcher) cannot see them and matches nothing.
        let Some(searcher) = enable_scoring.searcher() else {
            return Ok(Box::new(CrossLineWeight {
                hits: HashMap::new(),
                score: 0.0,
            }));
        };
        let hits = self.evaluate(searcher)?;
        let score = if enable_scoring.is_scoring_enabled() {
            self.score(searcher)?
        } else {
            0.0
        };
        Ok(Box::new(CrossLineWeight { hits, score }))
    }
}

struct CrossLineWeight {
    hits: HashMap<SegmentId, Arc<Vec<DocId>>>,
    score: Score,
}

impl Weight for CrossLineWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        match self.hits.get(&reader.segment_id()) {
            Some(docs) if !docs.is_empty() => Ok(Box::new(ConstScorer::new(
                SortedDocs {
                    docs: Arc::clone(docs),
                    cursor: 0,
                },
                self.score * boost,
            ))),
            _ => Ok(Box::new(EmptyScorer)),
        }
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let matched = self
            .hits
            .get(&reader.segment_id())
            .is_some_and(|docs| docs.binary_search(&doc).is_ok());
        if !matched {
            return Err(tantivy::TantivyError::InvalidArgument(format!(
                "document {doc} does not match the cross-line phrase"
            )));
        }
        Ok(Explanation::new("cross-line phrase", self.score))
    }
}

// ── Evaluation helpers ───────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Side {
    /// The part ends line `L`.
    EndsLine,
    /// The part starts line `L + 1`.
    StartsLine,
}

struct Part<'q> {
    words: &'q [CompiledWord],
    gaps: &'q [u32],
    side: Side,
}

#[derive(Clone)]
enum Candidates {
    /// Lines whose edge holds the part's boundary word.
    Edge,
    /// Lines holding the part's rarest word.
    Driver,
    /// These lines, ascending.
    Docs(Vec<DocId>),
}

struct PartMatch {
    segment: usize,
    doc: DocId,
    id: u64,
    /// Words between the part and its line edge.
    slack: u32,
}

#[derive(Default)]
struct Scratch {
    positions: Vec<Vec<u32>>,
    sweep: ChainSweep,
}

struct Segment<'a> {
    ordinal: usize,
    reader: &'a SegmentReader,
    text: Arc<InvertedIndexReader>,
    edge: Arc<InvertedIndexReader>,
    ids: Arc<InvertedIndexReader>,
    id: Column<u64>,
    first: Column<u64>,
    last: Column<u64>,
}

impl<'a> Segment<'a> {
    fn open(
        ordinal: usize,
        reader: &'a SegmentReader,
        query: &CrossLineQuery,
    ) -> tantivy::Result<Self> {
        let fast = reader.fast_fields();
        Ok(Self {
            ordinal,
            reader,
            text: reader.inverted_index(query.text)?,
            edge: reader.inverted_index(query.edge)?,
            ids: reader.inverted_index(query.id)?,
            id: fast.u64("id")?,
            first: fast.u64(LINE_FIRST_FIELD)?,
            last: fast.u64(LINE_LAST_FIELD)?,
        })
    }

    fn is_alive(&self, doc: DocId) -> bool {
        self.reader
            .alive_bitset()
            .is_none_or(|alive| alive.is_alive(doc))
    }
}

/// One segment's matching lines, ascending.
struct SortedDocs {
    docs: Arc<Vec<DocId>>,
    cursor: usize,
}

impl DocSet for SortedDocs {
    fn advance(&mut self) -> DocId {
        self.cursor = (self.cursor + 1).min(self.docs.len());
        self.doc()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.cursor += self.docs[self.cursor..].partition_point(|&doc| doc < target);
        self.doc()
    }

    fn doc(&self) -> DocId {
        self.docs.get(self.cursor).copied().unwrap_or(TERMINATED)
    }

    fn size_hint(&self) -> u32 {
        (self.docs.len() - self.cursor) as u32
    }
}

/// Ascending union of several postings lists' documents.
struct DocUnion {
    postings: Vec<SegmentPostings>,
    heap: BinaryHeap<Reverse<(DocId, usize)>>,
}

impl DocUnion {
    fn new(postings: Vec<SegmentPostings>) -> Self {
        let heap = postings
            .iter()
            .enumerate()
            .filter(|(_, p)| p.doc() != TERMINATED)
            .map(|(i, p)| Reverse((p.doc(), i)))
            .collect();
        Self { postings, heap }
    }
}

impl Iterator for DocUnion {
    type Item = DocId;

    fn next(&mut self) -> Option<DocId> {
        let Reverse((doc, _)) = *self.heap.peek()?;
        while let Some(&Reverse((at, i))) = self.heap.peek() {
            if at != doc {
                break;
            }
            self.heap.pop();
            let next = self.postings[i].advance();
            if next != TERMINATED {
                self.heap.push(Reverse((next, i)));
            }
        }
        Some(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_drops_enumerators_only() {
        for (line, content) in [
            ("(ג) ויאמר אלהים", "ויאמר אלהים"),
            ("ויהי כן {פ}", "ויהי כן"),
            ("[יא] דבר (א):", "דבר"),
            ("(שם הספר) דבר", "(שם הספר) דבר"),
            ("(abc) x", "(abc) x"),
            ("() x", "() x"),
            ("(אבגדהו) x", "(אבגדהו) x"),
            ("(א)", ""),
        ] {
            assert_eq!(&line[content_range(line)], content, "{line:?}");
        }
    }

    #[test]
    fn content_range_preserves_paired_readings_at_either_edge() {
        for (line, content) in [
            ("הארץ (הוצא) [היצא]", "הארץ (הוצא) [היצא]"),
            ("(הוצא) [היצא] אתך", "(הוצא) [היצא] אתך"),
            ("  (ח')[ו'] אתך", "  (ח')[ו'] אתך"),
            ("הארץ (ח')[ו']:  ", "הארץ (ח')[ו']:  "),
            ("(אריכותהכתיב) [קרי] אתך", "(אריכותהכתיב) [קרי] אתך"),
            ("(כתב) [אריכותהקרי] אתך", "(כתב) [אריכותהקרי] אתך"),
            ("הארץ (אריכותהכתיב) [קרי]׃", "הארץ (אריכותהכתיב) [קרי]׃"),
            ("(א) (הוצא) [היצא] {פ}", "(הוצא) [היצא]"),
            ("(א)[ב](ג)[ד]", "(א)[ב](ג)[ד]"),
            ("הארץ (לך) [לכה־]", "הארץ (לך) [לכה־]"),
        ] {
            assert_eq!(&line[content_range(line)], content, "{line:?}");
        }
    }

    #[test]
    fn line_edges_index_both_readings_as_one_content_position() {
        let mut analyzer = TextAnalyzer::builder(crate::hebrew_tokenizer::HebrewTokenizer {
            emit_quote_free: true,
            keep_marks: false,
        })
        .build();
        for (line, position) in [("(הוצא) [היצא]", 0), ("(א) (הוצא) [היצא] {פ}", 1)]
        {
            let edges = line_edges(&mut analyzer, line).unwrap();
            assert_eq!((edges.first, edges.last), (position, position), "{line}");
            assert_eq!(edges.start_terms, ["הוצא", "היצא"], "{line}");
            assert_eq!(edges.end_terms, ["הוצא", "היצא"], "{line}");
        }
    }

    #[test]
    fn joined_lines_preserve_both_readings_and_skip_actual_markers() {
        for (left, right) in [
            ("הארץ (הוצא) [היצא]", "(ב) אתך"),
            ("הארץ (הוצא) [היצא] {פ}", "(ב) אתך"),
        ] {
            let (joined, line_break) = joined_lines(left, right);
            assert_eq!(joined, "הארץ (הוצא) [היצא]\nאתך");
            assert_eq!(&joined[line_break], SNIPPET_LINE_BREAK);
        }
        let (joined, _) = joined_lines("הארץ {פ}", "(הוצא) [היצא] אתך");
        assert_eq!(joined, "הארץ\n(הוצא) [היצא] אתך");
    }

    #[test]
    fn joined_lines_keep_the_content_around_the_break() {
        let (joined, line_break) = joined_lines("סוף השורה {פ}", "(ב) תחילת הבאה");
        assert_eq!(joined, format!("סוף השורה{SNIPPET_LINE_BREAK}תחילת הבאה"));
        assert_eq!(&joined[line_break], SNIPPET_LINE_BREAK);
    }
}
