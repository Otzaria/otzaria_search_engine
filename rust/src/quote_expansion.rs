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

use tantivy::query::{BooleanQuery, Occur, PhraseQuery, Query, RegexPhraseQuery, TermQuery};
use tantivy::schema::{Field, IndexRecordOption};
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

/// Matches exactly one byte string.
#[derive(Debug, Clone)]
pub(crate) struct Literal(Vec<u8>);

impl Literal {
    pub(crate) fn new(word: &str) -> Self {
        Self(word.as_bytes().to_vec())
    }
}

impl Automaton for Literal {
    type State = Option<usize>;

    fn start(&self) -> Option<usize> {
        Some(0)
    }

    fn is_match(&self, state: &Option<usize>) -> bool {
        *state == Some(self.0.len())
    }

    fn can_match(&self, state: &Option<usize>) -> bool {
        state.is_some()
    }

    fn accept(&self, state: &Option<usize>, byte: u8) -> Option<usize> {
        state.and_then(|at| (self.0.get(at) == Some(&byte)).then_some(at + 1))
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
    let automaton = QuoteTransparent::new(Literal::new(word));
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
    let mut seen = std::collections::HashSet::new();
    let mut terms = Vec::new();
    for word in words {
        for spelling in spellings(searcher, field, word.as_ref())? {
            if seen.insert(spelling.clone()) {
                terms.push(Term::from_field_text(field, &spelling));
            }
        }
    }
    Ok(terms)
}

/// A `TermQuery` for `word`, or the union of one per spelling when it has several.
pub(crate) fn term_query(
    searcher: &Searcher,
    field: Field,
    word: &str,
    option: IndexRecordOption,
) -> tantivy::Result<Box<dyn Query>> {
    let spellings = spellings(searcher, field, word)?;
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = spellings
        .iter()
        .map(|spelling| {
            (
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(field, spelling),
                    option,
                )) as Box<dyn Query>,
            )
        })
        .collect();
    Ok(if clauses.len() == 1 {
        clauses.pop().expect("one clause").1
    } else {
        Box::new(BooleanQuery::new(clauses))
    })
}

/// The adjacent (within `slop`) phrase of `words`: a `PhraseQuery` when no word has
/// another spelling, else the same phrase over each word's spellings.
pub(crate) fn phrase_query(
    searcher: &Searcher,
    field: Field,
    words: &[String],
    slop: u32,
) -> tantivy::Result<Box<dyn Query>> {
    let mut plain = true;
    for word in words {
        plain &= spellings(searcher, field, word)?.len() == 1;
        if !plain {
            break;
        }
    }
    if plain {
        let mut phrase = PhraseQuery::new(
            words
                .iter()
                .map(|word| Term::from_field_text(field, word))
                .collect(),
        );
        phrase.set_slop(slop);
        return Ok(Box::new(phrase));
    }
    let automata = words
        .iter()
        .enumerate()
        .map(|(offset, word)| {
            (
                offset,
                word.clone(),
                Arc::new(QuoteTransparent::new(Literal::new(word))),
            )
        })
        .collect();
    Ok(Box::new(RegexPhraseQuery::from_automata(
        field, automata, slop,
    )))
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
            matches(Literal::new("רמבם"), DICTIONARY),
            ["רמ\"בם", "רמב\"ם", "רמב'ם", "רמבם"]
        );
    }

    #[test]
    fn a_quoted_word_reaches_only_itself() {
        assert_eq!(matches(Literal::new("רמב\"ם"), DICTIONARY), ["רמב\"ם"]);
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
}
