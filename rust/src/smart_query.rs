//! Placeholder spellings shared by smart retrieval and query embedding.

use std::borrow::Cow;
use std::sync::OnceLock;

use crate::hebrew_query::{self, VocalizedFlags};

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
