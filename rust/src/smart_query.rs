//! Placeholder spellings shared by smart retrieval and query embedding.

#[cfg(any(feature = "semantic-integration", test))]
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::OnceLock;

use crate::hebrew_query::{self, VocalizedFlags};
use crate::{
    display_highlight, highlight_cache::DisplayPattern, highlight_matcher::PreparedHighlightMatcher,
};
use otzaria_semantic_search::hybrid::ranking::quoted_phrases;
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, TextAnalyzer, TokenStream};

/// The app's `holyNameForPlaceholderWord`: a whole word, with up to two prefixes.
pub(crate) fn alternative(word: &str) -> Option<String> {
    static WORD: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = WORD.get_or_init(|| {
        regex::Regex::new(
            r"^((?:[ובכלמשה]\p{Mn}*){0,2})י(\p{Mn}*)([קד])(\p{Mn}*)ו(\p{Mn}*)([קד])(\p{Mn}*)$",
        )
        .expect("placeholder word pattern")
    });
    let found = pattern.captures(word)?;
    if found[3] != found[6] {
        return None;
    }
    Some(format!(
        "{}\u{05d9}{}\u{05d4}{}\u{05d5}{}\u{05d4}{}",
        &found[1], &found[2], &found[4], &found[5], &found[7]
    ))
}

pub(crate) fn alternatives(word: &str) -> Vec<String> {
    std::iter::once(word.to_string())
        .chain(alternative(word))
        .collect()
}

pub(crate) fn has_alternatives(query: &str) -> bool {
    hebrew_query::split_query_words(query)
        .iter()
        .any(|word| alternative(word).is_some())
}

/// Literal alternatives keep the marks on their original letters.
pub(crate) fn vocalized_pattern(word: &str, voc: &VocalizedFlags) -> String {
    let original = hebrew_query::vocalized_token_pattern(word, voc);
    match alternative(word) {
        None => original,
        Some(canonical) => format!(
            "(?:{original}|{})",
            hebrew_query::vocalized_token_pattern(&canonical, voc)
        ),
    }
}

/// Semantic scoring uses this copy; session keys and lexical queries retain the input.
#[cfg(any(feature = "semantic-integration", test))]
pub(crate) fn canonical(query: &str) -> Cow<'_, str> {
    let replacements: Vec<_> = hebrew_query::query_word_spans(query)
        .into_iter()
        .filter_map(|(word, start, end)| alternative(&word).map(|word| (word, start, end)))
        .collect();
    if replacements.is_empty() {
        return Cow::Borrowed(query);
    }
    let mut units: Vec<u16> = query.encode_utf16().collect();
    for (word, start, end) in replacements.into_iter().rev() {
        units.splice(start as usize..end as usize, word.encode_utf16());
    }
    Cow::Owned(String::from_utf16(&units).expect("query word spans preserve UTF-16 boundaries"))
}

fn display_tokens(query: &str) -> Vec<String> {
    // Match the index's `hebrew_query` analyzer without opening an index. In
    // particular, keep both qere/ketiv terms and remove the same long tokens.
    let mut analyzer = TextAnalyzer::builder(crate::hebrew_tokenizer::HebrewTokenizer::default())
        .filter(RemoveLongFilter::limit(
            crate::api::search_engine::MAX_TOKEN_BYTES,
        ))
        .filter(LowerCaser)
        .build();
    let mut stream = analyzer.token_stream(query);
    let mut words = Vec::new();
    while let Some(token) = stream.next() {
        words.push(token.text.clone());
    }
    words
}

pub(crate) fn whole_query_quoted(query: &str) -> bool {
    let phrases = quoted_phrases(query);
    if phrases.len() != 1 {
        return false;
    }
    let words = display_tokens(query);
    !words.is_empty() && words == display_tokens(&phrases[0])
}

/// The index-backed and pure-string display paths share quotation boundaries.
pub(crate) fn display_pattern(
    query: &str,
    exact: bool,
    mut outside_terms: impl FnMut(&[String]) -> anyhow::Result<Vec<String>>,
) -> anyhow::Result<Option<DisplayPattern>> {
    use std::collections::HashMap;

    let display = |words: &[String], terms: &[Vec<String>]| {
        display_highlight::build_display_highlight_from_terms(
            &words.join(" "),
            0,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            terms,
            false,
        )
    };
    let literal = |words: &[String], expand_aliases: bool| {
        display(
            words,
            &words
                .iter()
                .map(|word| {
                    if expand_aliases {
                        alternatives(word)
                    } else {
                        vec![word.clone()]
                    }
                })
                .collect::<Vec<_>>(),
        )
    };
    let expand_aliases = has_alternatives(query);
    let mut outside = display_tokens(query);
    let mut groups = Vec::new();
    if exact {
        groups.extend(literal(&outside, expand_aliases));
    } else {
        for phrase in quoted_phrases(query) {
            let words = display_tokens(&phrase);
            // Occurrences outside a quote keep their own fuzzy alternatives.
            for word in &words {
                if let Some(at) = outside.iter().position(|candidate| candidate == word) {
                    outside.remove(at);
                }
            }
            groups.extend(literal(&words, has_alternatives(&phrase)));
        }
        for word in outside {
            let choices = if expand_aliases {
                alternatives(&word)
            } else {
                vec![word.clone()]
            };
            groups.extend(display(&[word], &[outside_terms(&choices)?]));
        }
    }
    if groups.is_empty() {
        return Ok(None);
    }
    let combined_pattern = if groups.len() == 1 {
        groups[0].combined_pattern.clone()
    } else {
        "(?!)".to_string()
    };
    let mut word_patterns = Vec::new();
    let mut word_boundary_eligible = Vec::new();
    let mut matchers = Vec::new();
    for group in groups {
        word_patterns.extend(group.word_patterns);
        word_boundary_eligible.extend(group.word_boundary_eligible);
        matchers.push(group.matcher);
    }
    Ok(Some(DisplayPattern {
        combined_pattern,
        word_patterns,
        word_boundary_eligible,
        matcher: Arc::new(PreparedHighlightMatcher::from_groups(matchers, exact)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_preserve_prefixes_marks_and_original_alternatives() {
        for (word, expected) in [
            ("יקוק", "\u{05d9}\u{05d4}\u{05d5}\u{05d4}"),
            ("ידוד", "\u{05d9}\u{05d4}\u{05d5}\u{05d4}"),
            ("ליקוק", "ל\u{05d9}\u{05d4}\u{05d5}\u{05d4}"),
            ("וּבַידוָד", "וּבַ\u{05d9}\u{05d4}וָ\u{05d4}"),
            ("לַֽיקֹוָֽק", "לַֽ\u{05d9}\u{05d4}ֹוָֽ\u{05d4}"),
        ] {
            assert_eq!(alternative(word).as_deref(), Some(expected), "{word}");
            assert_eq!(alternatives(word), [word, expected]);
        }
        for word in [
            "יקוד",
            "ידוק",
            "אבגידוד",
            "ובשיקוק",
            "יקוקים",
            "יקוק2",
            "aיקוק",
        ] {
            assert_eq!(alternative(word), None, "{word}");
        }
    }

    #[test]
    fn embedding_rewrites_only_complete_engine_words_and_keeps_other_input() {
        let name = "\u{05d9}\u{05d4}\u{05d5}\u{05d4}";
        for (query, expected) in [
            (
                "\"יקוק\" — ידוד?".to_string(),
                format!("\"{name}\" — {name}?"),
            ),
            ("😀 יקוק, ידוד".to_string(), format!("😀 {name}, {name}")),
            ("_יקוק".to_string(), format!("_{name}")),
            (
                "אבג״יקוק יקוק׳אבג".to_string(),
                "אבג״יקוק יקוק׳אבג".to_string(),
            ),
            (
                "אבג.יקוק אבג\u{200f}יקוק".to_string(),
                "אבג.יקוק אבג\u{200f}יקוק".to_string(),
            ),
            ("(יקוק) [אבג]".to_string(), "(יקוק) [אבג]".to_string()),
            (
                "😀 (יקוק) [ידוד]".to_string(),
                format!("😀 (יקוק) [{name}]"),
            ),
            (
                "שאילתה רגילה, עם \"ציטוט\"".to_string(),
                "שאילתה רגילה, עם \"ציטוט\"".to_string(),
            ),
        ] {
            assert_eq!(canonical(&query), expected, "{query}");
        }
        assert!(matches!(canonical("מילה רגילה"), Cow::Borrowed(_)));
    }
}
