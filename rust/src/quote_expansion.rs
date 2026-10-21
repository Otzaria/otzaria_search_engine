//! Quote-free spellings, found at query time (schema 6).
//!
//! Up to schema 5 the indexing tokenizer emitted, beside every word carrying a geresh or
//! gershayim (`רמב"ם`), a quote-free twin at the same position (`רמבם`), so a query for
//! `רמבם` — or one that ignores quotes — matched both. Schema 6 indexes the word alone
//! (~7% of the index), and the query side gives the same answers:
//!
//! - an automaton over the `text`/`textVocalized`/`lineEdge` dictionaries is wrapped in
//!   [`QuoteTransparent`], which accepts a term when the automaton accepts it as written
//!   *or* without its quotes — exactly the terms the twin made reachable;
//! - an exact word becomes all its [`spellings`]: itself and every dictionary term that
//!   is it with quotes inserted, found by walking the dictionary with that same wrapper.
//!
//! Nothing is precomputed: a walk for one word only follows the word's own letters and
//! the quote bytes between them, so it costs about as much as a term lookup.

use std::sync::Arc;

use tantivy::fieldnorm::FieldNormReader;
use tantivy::postings::{Postings, SegmentPostings};
use tantivy::query::{
    Bm25StatisticsProvider, Bm25Weight, BooleanQuery, EmptyScorer, EnableScoring, Explanation,
    Occur, PhraseQuery, Query, RegexPhraseQuery, Scorer, TermQuery, Weight,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocId, DocSet, Score, SegmentReader};
use tantivy::{Searcher, Term};
use tantivy_fst::Automaton;

/// The two quote bytes an index term can hold: the tokenizer folds ׳/״ to them.
#[inline]
pub(crate) fn is_quote(byte: u8) -> bool {
    byte == b'\'' || byte == b'"'
}

/// Accepts a term when `A` accepts it, or accepts it with its quotes removed.
#[derive(Debug)]
pub(crate) struct QuoteTransparent<A> {
    inner: Arc<A>,
}

impl<A> QuoteTransparent<A> {
    pub(crate) fn new(inner: A) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    pub(crate) fn from_arc(inner: Arc<A>) -> Self {
        Self { inner }
    }
}

impl<A> Clone for QuoteTransparent<A> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// The literal run and the quote-free run of the inner automaton; one run until the
/// first quote byte, which is the common case.
#[derive(Clone, Debug)]
pub(crate) enum QuoteState<S> {
    /// No quote yet; `letters`: a byte was consumed.
    Same { state: S, letters: bool },
    Split {
        literal: Option<S>,
        bare: Option<S>,
        /// The quote-free run consumed a letter: an all-quote term has no spelling.
        bare_letters: bool,
    },
}

impl<A: Automaton> Automaton for QuoteTransparent<A>
where
    A::State: Clone,
{
    type State = QuoteState<A::State>;

    fn start(&self) -> Self::State {
        QuoteState::Same {
            state: self.inner.start(),
            letters: false,
        }
    }

    fn is_match(&self, state: &Self::State) -> bool {
        match state {
            QuoteState::Same { state, .. } => self.inner.is_match(state),
            QuoteState::Split {
                literal,
                bare,
                bare_letters,
            } => {
                literal.as_ref().is_some_and(|s| self.inner.is_match(s))
                    || (*bare_letters && bare.as_ref().is_some_and(|s| self.inner.is_match(s)))
            }
        }
    }

    fn can_match(&self, state: &Self::State) -> bool {
        match state {
            QuoteState::Same { state, .. } => self.inner.can_match(state),
            QuoteState::Split { literal, bare, .. } => {
                literal.as_ref().is_some_and(|s| self.inner.can_match(s))
                    || bare.as_ref().is_some_and(|s| self.inner.can_match(s))
            }
        }
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        let alive = |s: A::State| self.inner.can_match(&s).then_some(s);
        match state {
            QuoteState::Same { state, .. } if !is_quote(byte) => QuoteState::Same {
                state: self.inner.accept(state, byte),
                letters: true,
            },
            QuoteState::Same { state, letters } => QuoteState::Split {
                literal: alive(self.inner.accept(state, byte)),
                bare: alive(state.clone()),
                bare_letters: *letters,
            },
            QuoteState::Split {
                literal,
                bare,
                bare_letters,
            } => QuoteState::Split {
                literal: literal
                    .as_ref()
                    .and_then(|s| alive(self.inner.accept(s, byte))),
                bare: if is_quote(byte) {
                    bare.clone()
                } else {
                    bare.as_ref()
                        .and_then(|s| alive(self.inner.accept(s, byte)))
                },
                bare_letters: *bare_letters || !is_quote(byte),
            },
        }
    }
}

/// Matches exactly the given byte strings. A state is the range of the sorted words
/// that start with the bytes read so far, and how many bytes that is.
#[derive(Debug, Clone)]
pub(crate) struct Literal(Vec<Vec<u8>>);

impl Literal {
    pub(crate) fn new<'a>(words: impl IntoIterator<Item = &'a str>) -> Self {
        let mut words: Vec<Vec<u8>> = words.into_iter().map(|w| w.as_bytes().to_vec()).collect();
        words.sort_unstable();
        words.dedup();
        Self(words)
    }
}

impl Automaton for Literal {
    type State = Option<(usize, usize, usize)>;

    fn start(&self) -> Self::State {
        (!self.0.is_empty()).then_some((0, self.0.len(), 0))
    }

    fn is_match(&self, state: &Self::State) -> bool {
        state.is_some_and(|(lo, _, depth)| self.0[lo].len() == depth)
    }

    fn can_match(&self, state: &Self::State) -> bool {
        state.is_some()
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        let (lo, hi, depth) = (*state)?;
        let range = &self.0[lo..hi];
        let start = lo + range.partition_point(|w| w.get(depth).is_none_or(|&b| b < byte));
        let end = lo + range.partition_point(|w| w.get(depth).is_none_or(|&b| b <= byte));
        (start < end).then_some((start, end, depth + 1))
    }
}

/// `word` and every term of `field` that is `word` with quotes inserted, as the
/// dictionaries of `searcher` hold them; `word` first, whether it is indexed or not.
pub(crate) fn spellings(
    searcher: &Searcher,
    field: Field,
    word: &str,
) -> tantivy::Result<Vec<String>> {
    let mut out = vec![word.to_string()];
    if word.bytes().any(is_quote) {
        return Ok(out);
    }
    let automaton = QuoteTransparent::new(Literal::new([word]));
    for reader in searcher.segment_readers() {
        let inverted = reader.inverted_index(field)?;
        let mut stream = inverted.terms().search(&automaton).into_stream()?;
        while stream.advance() {
            if let Ok(term) = std::str::from_utf8(stream.key()) {
                if !out.iter().any(|known| known == term) {
                    out.push(term.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// The [`spellings`] of every word of `words`, as terms of `field`, each once.
pub(crate) fn spelling_terms<S: AsRef<str>>(
    searcher: &Searcher,
    field: Field,
    words: &[S],
) -> tantivy::Result<Vec<Term>> {
    // Quoted inputs are already emitted verbatim below, just as in `spellings`.
    let literal = Literal::new(
        words
            .iter()
            .map(|w| w.as_ref())
            .filter(|word| !word.bytes().any(is_quote)),
    );
    let mut found: std::collections::HashMap<Vec<u8>, Vec<String>> = Default::default();
    if literal.can_match(&literal.start()) {
        // One walk per segment for all expandable words; a term's quote-free form
        // identifies the word it belongs to.
        let automaton = QuoteTransparent::new(literal);
        for reader in searcher.segment_readers() {
            let inverted = reader.inverted_index(field)?;
            let mut stream = inverted.terms().search(&automaton).into_stream()?;
            while stream.advance() {
                if let Ok(term) = std::str::from_utf8(stream.key()) {
                    let word = term.bytes().filter(|&b| !is_quote(b)).collect();
                    found.entry(word).or_default().push(term.to_string());
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut terms = Vec::new();
    for word in words.iter().map(|w| w.as_ref()) {
        let spellings = found.get(word.as_bytes()).into_iter().flatten();
        for spelling in std::iter::once(word).chain(spellings.map(String::as_str)) {
            if seen.insert(spelling) {
                terms.push(Term::from_field_text(field, spelling));
            }
        }
    }
    Ok(terms)
}

/// A `TermQuery` for `word`, or one [`SpellingsTermQuery`] over its spellings when it
/// has several.
pub(crate) fn term_query(
    searcher: &Searcher,
    field: Field,
    word: &str,
    option: IndexRecordOption,
) -> tantivy::Result<Box<dyn Query>> {
    let spellings = spellings(searcher, field, word)?;
    let mut terms: Vec<Term> = spellings
        .iter()
        .map(|spelling| Term::from_field_text(field, spelling))
        .collect();
    Ok(if terms.len() == 1 {
        Box::new(TermQuery::new(terms.pop().expect("one term"), option))
    } else {
        Box::new(SpellingsTermQuery { terms, option })
    })
}

/// The spellings of one word as the one term schema 5's quote-free twin was: matched by
/// any of them, scored by BM25 on their summed frequencies in the line and summed
/// document frequency. A union of `TermQuery`s would give a rare spelling (`א'מ'ר`) its
/// own high IDF, and a line mixing spellings would escape the term-frequency saturation.
#[derive(Clone, Debug)]
pub(crate) struct SpellingsTermQuery {
    terms: Vec<Term>,
    /// Without frequencies a line scores as holding the word once, as a `TermQuery` does.
    option: IndexRecordOption,
}

impl Query for SpellingsTermQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let bm25 = match enable_scoring {
            EnableScoring::Enabled {
                statistics_provider,
                ..
            } => {
                let shared = SharedStatistics {
                    inner: statistics_provider,
                    groups: std::slice::from_ref(&self.terms),
                };
                Some(Bm25Weight::for_terms(&shared, &self.terms[..1])?)
            }
            EnableScoring::Disabled { .. } => None,
        };
        let freqs = self.option.has_freq() && bm25.is_some();
        // tantivy's union of the spellings, each scoring its frequency in the line: the
        // union's sum is the word's frequency.
        let union = BooleanQuery::new(
            self.terms
                .iter()
                .map(|term| {
                    let frequency = TermFrequencyQuery {
                        term: term.clone(),
                        freqs,
                    };
                    (Occur::Should, Box::new(frequency) as Box<dyn Query>)
                })
                .collect(),
        );
        Ok(Box::new(SpellingsTermWeight {
            union: union.weight(enable_scoring)?,
            field: self.terms[0].field(),
            freqs,
            bm25,
        }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        for term in &self.terms {
            visitor(term, false);
        }
    }
}

struct SpellingsTermWeight {
    union: Box<dyn Weight>,
    field: Field,
    freqs: bool,
    bm25: Option<Bm25Weight>,
}

impl Weight for SpellingsTermWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let union = self.union.scorer(reader, 1.0)?;
        let Some(bm25) = &self.bm25 else {
            return Ok(union);
        };
        Ok(Box::new(SpellingsScorer {
            union,
            fieldnorms: reader.get_fieldnorms_reader(self.field)?,
            bm25: bm25.boost_by(boost),
            freqs: self.freqs,
        }))
    }

    fn count(&self, reader: &SegmentReader) -> tantivy::Result<u32> {
        self.union.count(reader)
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) != doc {
            return Err(tantivy::TantivyError::InvalidArgument(format!(
                "Document #({doc}) does not match"
            )));
        }
        Ok(Explanation::new("SpellingsTermQuery", scorer.score()))
    }
}

/// BM25 of the line's length and the union's summed frequency.
struct SpellingsScorer {
    union: Box<dyn Scorer>,
    fieldnorms: FieldNormReader,
    bm25: Bm25Weight,
    freqs: bool,
}

impl DocSet for SpellingsScorer {
    fn advance(&mut self) -> DocId {
        self.union.advance()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.union.seek(target)
    }

    fn doc(&self) -> DocId {
        self.union.doc()
    }

    fn size_hint(&self) -> u32 {
        self.union.size_hint()
    }
}

impl Scorer for SpellingsScorer {
    fn score(&mut self) -> Score {
        let term_freq = if self.freqs {
            self.union.score().round() as u32
        } else {
            1
        };
        let fieldnorm_id = self.fieldnorms.fieldnorm_id(self.union.doc());
        self.bm25.score(fieldnorm_id, term_freq.max(1))
    }
}

/// One spelling, scoring its frequency in the line (1 without frequencies).
#[derive(Clone, Debug)]
struct TermFrequencyQuery {
    term: Term,
    freqs: bool,
}

impl Query for TermFrequencyQuery {
    fn weight(&self, _: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(self.clone()))
    }
}

impl Weight for TermFrequencyQuery {
    fn scorer(&self, reader: &SegmentReader, _: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let option = if self.freqs {
            IndexRecordOption::WithFreqs
        } else {
            IndexRecordOption::Basic
        };
        let inverted = reader.inverted_index(self.term.field())?;
        Ok(match inverted.read_postings(&self.term, option)? {
            Some(postings) => Box::new(TermFrequencyScorer {
                postings,
                freqs: self.freqs,
            }),
            None => Box::new(EmptyScorer),
        })
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) != doc {
            return Err(tantivy::TantivyError::InvalidArgument(format!(
                "Document #({doc}) does not match"
            )));
        }
        Ok(Explanation::new("TermFrequency", scorer.score()))
    }
}

struct TermFrequencyScorer {
    postings: SegmentPostings,
    freqs: bool,
}

impl DocSet for TermFrequencyScorer {
    fn advance(&mut self) -> DocId {
        self.postings.advance()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.postings.seek(target)
    }

    fn fill_buffer(&mut self, buffer: &mut [DocId; tantivy::COLLECT_BLOCK_BUFFER_LEN]) -> usize {
        self.postings.fill_buffer(buffer)
    }

    fn doc(&self) -> DocId {
        self.postings.doc()
    }

    fn size_hint(&self) -> u32 {
        self.postings.size_hint()
    }
}

impl Scorer for TermFrequencyScorer {
    fn score(&mut self) -> Score {
        if self.freqs {
            self.postings.term_freq() as Score
        } else {
            1.0
        }
    }
}

/// Spelling combinations a phrase is served as a union of `PhraseQuery`s up to; past it,
/// one `RegexPhraseQuery` over every word's spellings.
const MAX_SPELLING_PHRASES: usize = 16;

/// The adjacent (within `slop`) phrase of `words`: a `PhraseQuery` when no word has
/// another spelling, else the same phrase over each word's spellings. A few combinations
/// stay `PhraseQuery`s: the written one costs as before, the quoted ones are rare.
pub(crate) fn phrase_query(
    searcher: &Searcher,
    field: Field,
    words: &[String],
    slop: u32,
) -> tantivy::Result<Box<dyn Query>> {
    let spellings = words
        .iter()
        .map(|word| spellings(searcher, field, word))
        .collect::<tantivy::Result<Vec<_>>>()?;
    let combinations = spellings
        .iter()
        .try_fold(1usize, |n, s| n.checked_mul(s.len()))
        .filter(|&n| n <= MAX_SPELLING_PHRASES);
    if let Some(combinations) = combinations {
        let phrase = |pick: usize| {
            let mut rest = pick;
            let terms = spellings
                .iter()
                .map(|options| {
                    let term = Term::from_field_text(field, &options[rest % options.len()]);
                    rest /= options.len();
                    term
                })
                .collect();
            let mut phrase = PhraseQuery::new(terms);
            phrase.set_slop(slop);
            Box::new(phrase) as Box<dyn Query>
        };
        if combinations == 1 {
            return Ok(phrase(0));
        }
        let union = Box::new(BooleanQuery::new(
            (0..combinations)
                .map(|pick| (Occur::Should, phrase(pick)))
                .collect(),
        ));
        return Ok(SpellingScored::wrap(union, field, &spellings));
    }
    let automata = words
        .iter()
        .enumerate()
        .map(|(offset, word)| {
            (
                offset,
                word.clone(),
                Arc::new(QuoteTransparent::new(Literal::new([word.as_str()]))),
            )
        })
        .collect();
    let phrase = Box::new(RegexPhraseQuery::from_automata(field, automata, slop));
    Ok(SpellingScored::wrap(phrase, field, &spellings))
}

/// Scores every spelling of a word as the word itself, as schema 5's quote-free twin
/// did: they share one document frequency, the sum of theirs. Without it a rare quoted
/// spelling (`א'מ'ר`) gets a far higher IDF and tops a relevance ranking.
pub(crate) struct SpellingScored {
    inner: Box<dyn Query>,
    groups: Arc<Vec<Vec<Term>>>,
}

impl Clone for SpellingScored {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.box_clone(),
            groups: Arc::clone(&self.groups),
        }
    }
}

impl SpellingScored {
    /// `inner`, scored with the spellings of each word of `words` sharing their statistics.
    fn wrap(inner: Box<dyn Query>, field: Field, words: &[Vec<String>]) -> Box<dyn Query> {
        let groups: Vec<Vec<Term>> = words
            .iter()
            .filter(|spellings| spellings.len() > 1)
            .map(|spellings| {
                spellings
                    .iter()
                    .map(|s| Term::from_field_text(field, s))
                    .collect()
            })
            .collect();
        if groups.is_empty() {
            return inner;
        }
        Box::new(Self {
            inner,
            groups: Arc::new(groups),
        })
    }
}

impl std::fmt::Debug for SpellingScored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The groups follow from the inner query's terms: the page cache keys by this text.
        std::fmt::Debug::fmt(&self.inner, f)
    }
}

impl Query for SpellingScored {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        match enable_scoring {
            EnableScoring::Enabled {
                searcher,
                statistics_provider,
            } => {
                let shared = SharedStatistics {
                    inner: statistics_provider,
                    groups: &self.groups,
                };
                self.inner
                    .weight(EnableScoring::enabled_from_statistics_provider(
                        &shared, searcher,
                    ))
            }
            disabled => self.inner.weight(disabled),
        }
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        self.inner.query_terms(visitor);
    }
}

struct SharedStatistics<'a> {
    inner: &'a dyn Bm25StatisticsProvider,
    groups: &'a [Vec<Term>],
}

impl Bm25StatisticsProvider for SharedStatistics<'_> {
    fn total_num_tokens(&self, field: Field) -> tantivy::Result<u64> {
        self.inner.total_num_tokens(field)
    }

    fn total_num_docs(&self) -> tantivy::Result<u64> {
        self.inner.total_num_docs()
    }

    fn doc_freq(&self, term: &Term) -> tantivy::Result<u64> {
        match self.groups.iter().find(|group| group.contains(term)) {
            // A line holding two spellings counts twice; BM25 refuses more than every doc.
            Some(group) => Ok(group
                .iter()
                .map(|t| self.inner.doc_freq(t))
                .sum::<tantivy::Result<u64>>()?
                .min(self.inner.total_num_docs()?)),
            None => self.inner.doc_freq(term),
        }
    }
}

/// The documents holding a term `automaton` accepts written or without its quotes,
/// scored a constant as `FuzzyTermQuery` scores (its weight is the same).
pub(crate) struct AutomatonQuery<A> {
    field: Field,
    /// What the automaton accepts, for `Debug`: the page cache keys queries by it.
    label: String,
    automaton: Arc<QuoteTransparent<A>>,
}

impl<A> AutomatonQuery<A> {
    pub(crate) fn new(field: Field, label: String, automaton: A) -> Self {
        Self {
            field,
            label,
            automaton: Arc::new(QuoteTransparent::new(automaton)),
        }
    }
}

impl<A> Clone for AutomatonQuery<A> {
    fn clone(&self) -> Self {
        Self {
            field: self.field,
            label: self.label.clone(),
            automaton: Arc::clone(&self.automaton),
        }
    }
}

impl<A> std::fmt::Debug for AutomatonQuery<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutomatonQuery")
            .field("field", &self.field)
            .field("label", &self.label)
            .finish()
    }
}

impl<A> Query for AutomatonQuery<A>
where
    A: Automaton + Send + Sync + 'static,
    A::State: Clone,
{
    fn weight(
        &self,
        _enable_scoring: tantivy::query::EnableScoring<'_>,
    ) -> tantivy::Result<Box<dyn tantivy::query::Weight>> {
        Ok(Box::new(tantivy::query::AutomatonWeight::<
            QuoteTransparent<A>,
        >::new(
            self.field, Arc::clone(&self.automaton)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy_fst::{IntoStreamer, Map, Streamer};

    fn matches<A: Automaton>(automaton: A, terms: &[&str]) -> Vec<String>
    where
        A::State: Clone,
    {
        let mut sorted: Vec<&str> = terms.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let map = Map::from_iter(sorted.iter().map(|t| (t.as_bytes(), 0u64))).unwrap();
        let mut stream = map.search(QuoteTransparent::new(automaton)).into_stream();
        let mut out = Vec::new();
        while let Some((key, _)) = stream.next() {
            out.push(String::from_utf8(key.to_vec()).unwrap());
        }
        out
    }

    const DICTIONARY: &[&str] = &[
        "רמבם",
        "רמב\"ם",
        "הרמב\"ם",
        "רמ\"בם",
        "רמב'ם",
        "רמבן",
        "\"",
        "'",
        "ש\"ץ",
        "שץ",
    ];

    #[test]
    fn a_quote_free_word_reaches_every_quoted_spelling() {
        assert_eq!(
            matches(Literal::new(["רמבם"]), DICTIONARY),
            ["רמ\"בם", "רמב\"ם", "רמב'ם", "רמבם"]
        );
    }

    #[test]
    fn a_quoted_word_reaches_only_itself() {
        assert_eq!(matches(Literal::new(["רמב\"ם"]), DICTIONARY), ["רמב\"ם"]);
    }

    #[test]
    fn a_pattern_matches_a_term_written_or_without_its_quotes() {
        let prefixed = tantivy_fst::Regex::new("ה?רמבם").unwrap();
        assert_eq!(
            matches(prefixed, DICTIONARY),
            ["הרמב\"ם", "רמ\"בם", "רמב\"ם", "רמב'ם", "רמבם"]
        );
        let quoted = tantivy_fst::Regex::new("ש\"ץ").unwrap();
        assert_eq!(matches(quoted, DICTIONARY), ["ש\"ץ"]);
    }

    #[test]
    fn a_term_of_quotes_alone_has_no_spelling() {
        let anything = tantivy_fst::Regex::new(".*").unwrap();
        let found = matches(anything, DICTIONARY);
        assert!(
            found.contains(&"\"".to_string()),
            "written as is, it matches"
        );
        let empty = tantivy_fst::Regex::new("").unwrap();
        assert!(matches(empty, DICTIONARY).is_empty());
    }

    #[test]
    fn spelling_terms_keep_word_order_across_segments() {
        use tantivy::schema::{Schema, TextFieldIndexing, TextOptions};
        use tantivy::tokenizer::{TextAnalyzer, WhitespaceTokenizer};

        let mut builder = Schema::builder();
        let indexing = TextFieldIndexing::default().set_tokenizer("ws");
        let text = builder.add_text_field(
            "text",
            TextOptions::default().set_indexing_options(indexing),
        );
        let index = tantivy::Index::create_in_ram(builder.build());
        index.tokenizers().register(
            "ws",
            TextAnalyzer::builder(WhitespaceTokenizer::default()).build(),
        );
        let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        writer.set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
        for line in [
            "רמבם רמב\"ם רמ\"בם רמב'ם שבת \" '",
            "רמבם ש\"ץ שבת רמבן",
            "רמבם שבתות רמ",
        ] {
            writer.add_document(tantivy::doc!(text => line)).unwrap();
            writer.commit().unwrap();
        }
        let searcher = index.reader().unwrap().searcher();
        assert_eq!(searcher.segment_readers().len(), 3);

        assert_eq!(
            spellings(&searcher, text, "רמבם").unwrap(),
            ["רמבם", "רמ\"בם", "רמב\"ם", "רמב'ם"]
        );
        let words = [
            "",
            "שבת",
            "\"",
            "'",
            "רמבם",
            "רמב\"ם",
            "חסר",
            "שבת",
            "רמב",
            "שץ",
            "",
        ];
        let found: Vec<String> = spelling_terms(&searcher, text, &words)
            .unwrap()
            .iter()
            .map(|term| term.value().as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            found,
            [
                "",
                "שבת",
                "\"",
                "'",
                "רמבם",
                "רמ\"בם",
                "רמב\"ם",
                "רמב'ם",
                "חסר",
                "רמב",
                "שץ",
                "ש\"ץ"
            ]
        );
    }

    #[test]
    fn quoted_inputs_remain_literal_without_expandable_words() {
        use tantivy::schema::{Schema, STRING};

        let mut builder = Schema::builder();
        let text = builder.add_text_field("text", STRING);
        let index = tantivy::Index::create_in_ram(builder.build());
        let searcher = index.reader().unwrap().searcher();

        // Skipping expansion must still emit the original quoted inputs once,
        // in their original order, even when none are in the dictionary.
        let words = ["רמב\"ם", "'", "\"", "רמב\"ם"];
        let found: Vec<String> = spelling_terms(&searcher, text, &words)
            .unwrap()
            .iter()
            .map(|term| term.value().as_str().unwrap().to_string())
            .collect();
        assert_eq!(found, ["רמב\"ם", "'", "\""]);
        assert!(spelling_terms::<&str>(&searcher, text, &[])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_phrase_with_few_spellings_is_a_union_of_phrases() {
        use tantivy::collector::Count;
        use tantivy::schema::{Schema, TextFieldIndexing, TextOptions};
        use tantivy::tokenizer::{TextAnalyzer, WhitespaceTokenizer};

        let mut builder = Schema::builder();
        let indexing = TextFieldIndexing::default()
            .set_tokenizer("ws")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        let text = builder.add_text_field(
            "text",
            TextOptions::default().set_indexing_options(indexing),
        );
        let index = tantivy::Index::create_in_ram(builder.build());
        index.tokenizers().register(
            "ws",
            TextAnalyzer::builder(WhitespaceTokenizer::default()).build(),
        );
        let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        for line in ["אמר רבי", "אמ\"ר רבי", "אמר ר\"בי", "שלום עולם"] {
            writer.add_document(tantivy::doc!(text => line)).unwrap();
        }
        let mut wide = String::new();
        for i in 1..=4 {
            let (head, tail) = "מלכים".split_at(i * 2);
            wide.push_str(&format!("{head}\"{tail} "));
        }
        writer.add_document(tantivy::doc!(text => wide)).unwrap();
        writer.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();

        let words = ["אמר".to_string(), "רבי".to_string()];
        let query = phrase_query(&searcher, text, &words, 0).unwrap();
        let debug = format!("{query:?}");
        assert!(
            debug.starts_with("BooleanQuery") && !debug.contains("RegexPhraseQuery"),
            "{debug}"
        );
        assert_eq!(searcher.search(&query, &Count).unwrap(), 3);

        let plain = ["שלום".to_string(), "עולם".to_string()];
        let plain = phrase_query(&searcher, text, &plain, 0).unwrap();
        assert!(format!("{plain:?}").starts_with("PhraseQuery"));

        // 5 × 5 spellings: past the union's cap, one regex phrase.
        let many = ["מלכים".to_string(), "מלכים".to_string()];
        let query = phrase_query(&searcher, text, &many, 0).unwrap();
        assert!(format!("{query:?}").starts_with("RegexPhraseQuery"));
        assert_eq!(searcher.search(&query, &Count).unwrap(), 1);
    }
}

#[cfg(test)]
mod scoring_tests {
    use super::*;
    use tantivy::collector::TopDocs;
    use tantivy::schema::{Schema, TextFieldIndexing, TextOptions};
    use tantivy::tokenizer::{TextAnalyzer, WhitespaceTokenizer};

    fn index(lines: &[&str]) -> (tantivy::Index, Field) {
        let mut builder = Schema::builder();
        let indexing = TextFieldIndexing::default()
            .set_tokenizer("ws")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        let text = builder.add_text_field(
            "text",
            TextOptions::default().set_indexing_options(indexing),
        );
        let index = tantivy::Index::create_in_ram(builder.build());
        index.tokenizers().register(
            "ws",
            TextAnalyzer::builder(WhitespaceTokenizer::default()).build(),
        );
        let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        for line in lines {
            writer.add_document(tantivy::doc!(text => *line)).unwrap();
        }
        writer.commit().unwrap();
        (index, text)
    }

    /// Scores of the documents `query` ranks, by document id.
    fn scores(searcher: &Searcher, query: &dyn Query) -> std::collections::HashMap<u32, f32> {
        searcher
            .search(query, &TopDocs::with_limit(100).order_by_score())
            .unwrap()
            .into_iter()
            .map(|(score, address)| (address.doc_id, score))
            .collect()
    }

    /// A rare quoted spelling ranks as the word itself, as the schema-5 twin token did.
    #[test]
    fn every_spelling_of_a_word_scores_as_the_word() {
        let mut lines = vec!["א'מ'ר רבי", "אמר רבי"];
        lines.extend(std::iter::repeat_n("אמר רבי זה וזה", 30));
        let (index, text) = index(&lines);
        let searcher = index.reader().unwrap().searcher();

        let word = term_query(&searcher, text, "אמר", IndexRecordOption::WithFreqs).unwrap();
        let found = scores(&searcher, &*word);
        assert_eq!(found.len(), 32);
        assert!((found[&0] - found[&1]).abs() < 1e-4, "{found:?}");

        let words = ["אמר".to_string(), "רבי".to_string()];
        let phrase = phrase_query(&searcher, text, &words, 0).unwrap();
        let found = scores(&searcher, &*phrase);
        assert_eq!(found.len(), 32);
        assert!((found[&0] - found[&1]).abs() < 1e-4, "{found:?}");
    }

    /// Occurrences of different spellings add up as occurrences of one term would, so a
    /// line mixing them does not escape BM25's saturation.
    #[test]
    fn spellings_in_one_line_count_as_one_term() {
        let mut lines = vec!["אמר אמר א'מ'ר זה", "אמר אמר אמר זה"];
        lines.extend(std::iter::repeat_n("אמר רבי זה וזה", 30));
        let (index, text) = index(&lines);
        let searcher = index.reader().unwrap().searcher();
        let word = term_query(&searcher, text, "אמר", IndexRecordOption::WithFreqs).unwrap();
        let found = scores(&searcher, &*word);
        assert_eq!(found.len(), 32);
        assert!((found[&0] - found[&1]).abs() < 1e-4, "{found:?}");
        let mut alive = Vec::new();
        searcher
            .search(&*word, &tantivy::collector::DocSetCollector)
            .unwrap()
            .into_iter()
            .for_each(|address| alive.push(address.doc_id));
        assert_eq!(alive.len(), 32);

        // Without frequencies (the exact search's `Basic`) a line counts once, as a
        // `TermQuery` does: only the line's length orders equal matches.
        let basic = term_query(&searcher, text, "אמר", IndexRecordOption::Basic).unwrap();
        let found = scores(&searcher, &*basic);
        assert!((found[&0] - found[&2]).abs() < 1e-4, "{found:?}");
    }
}
