//! Prepared, bounded display matching. Regexes only inspect individual index
//! tokens; phrase gaps are resolved with dynamic programming, never regex
//! backtracking. Work is O(words * tokens), plus linear token-regex matching.

use std::collections::HashMap;
use std::sync::Arc;

use regex::{Regex, RegexBuilder};
use tantivy::tokenizer::{TokenStream, Tokenizer};

use crate::hebrew_query::{fold_presentation_form, is_breaking_tag, is_word_mark};
use crate::hebrew_tokenizer::{is_geresh, is_gershayim, is_transparent, HebrewTokenizer};

pub(crate) struct PreparedHighlightMatcher {
    // Mode bits waive the token boundary on the outer left/right, respectively.
    words: Vec<[Arc<Regex>; 4]>,
    independent_words: Vec<[Arc<Regex>; 2]>,
    gaps: Vec<u32>,
    boundary_eligible: Vec<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Range {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Match {
    pub start: usize,
    pub end: usize,
    /// Relative to `start`, in Dart/JavaScript UTF-16 code units.
    pub word_ranges: Vec<Range>,
}

impl PreparedHighlightMatcher {
    pub(crate) fn new(
        branches: Vec<Vec<(String, String, String)>>,
        gaps: Vec<u32>,
        boundary_eligible: Vec<bool>,
    ) -> Result<Self, regex::Error> {
        let mut cache = HashMap::<String, Arc<Regex>>::new();
        let word_count = branches.len();
        let mut words = Vec::with_capacity(word_count);
        let mut independent_words = Vec::with_capacity(word_count);
        let mut compile = |pattern: String| -> Result<Arc<Regex>, regex::Error> {
            if let Some(regex) = cache.get(&pattern) {
                return Ok(regex.clone());
            }
            let regex = Arc::new(RegexBuilder::new(&pattern).case_insensitive(true).build()?);
            cache.insert(pattern, regex.clone());
            Ok(regex)
        };
        for (i, branches) in branches.into_iter().enumerate() {
            let roots: Vec<String> = branches
                .iter()
                .map(|(_, root, _)| format!("({root})"))
                .collect();
            let roots = roots.join("|");
            independent_words.push([
                compile(format!("\\A(?:{roots})\\z"))?,
                compile(format!("(?:{roots})"))?,
            ]);
            let mut modes = Vec::with_capacity(4);
            for mode in 0..4 {
                let alternatives: Vec<String> = branches
                    .iter()
                    .map(|(lead, root, trail)| {
                        let lead = if i == 0 {
                            if mode & 1 != 0 {
                                ".*?"
                            } else {
                                ""
                            }
                        } else {
                            lead.as_str()
                        };
                        let trail = if i + 1 == word_count {
                            if mode & 2 != 0 {
                                ".*"
                            } else {
                                ""
                            }
                        } else {
                            trail.as_str()
                        };
                        format!("(?:{lead})({root})(?:{trail})")
                    })
                    .collect();
                modes.push(compile(format!("\\A(?:{})\\z", alternatives.join("|")))?);
            }
            words.push(modes.try_into().unwrap());
        }
        Ok(Self {
            words,
            independent_words,
            gaps,
            boundary_eligible,
        })
    }

    pub(crate) fn find_matches(&self, data: &str, require_boundaries: &[bool]) -> Vec<Match> {
        if self.words.is_empty() || data.is_empty() {
            return Vec::new();
        }
        if self.words.len() == 1 {
            return self.find_word_matches(data, require_boundaries);
        }
        let text = MappedText::strip_markup(data);
        let mut tokenizer = HebrewTokenizer {
            emit_quote_free: true,
            keep_marks: false,
        };
        let mut stream = tokenizer.token_stream(&text.text);
        let mut candidates: Vec<Vec<Option<Range>>> = vec![Vec::new(); self.words.len()];
        while stream.advance() {
            let token = stream.token();
            let position = token.position;
            if candidates[0].len() <= position {
                for row in &mut candidates {
                    row.resize(position + 1, None);
                }
            }
            let mapped = text.normalized_token(
                token.offset_from,
                token.offset_to,
                !token.text.contains(['\'', '"']),
            );
            for (i, patterns) in self.words.iter().enumerate() {
                if candidates[i][position].is_some() {
                    continue;
                }
                let boundary = require_boundaries
                    .get(i)
                    .copied()
                    .unwrap_or(self.boundary_eligible[i]);
                let mode = usize::from(i == 0 && !boundary)
                    | (usize::from(i + 1 == self.words.len() && !boundary) << 1);
                if let Some(captures) = patterns[mode].captures(&mapped.text) {
                    if let Some(root) = captures.iter().skip(1).flatten().next() {
                        if !root.is_empty() {
                            candidates[i][position] = Some(Range {
                                start: mapped.starts[root.start()],
                                end: mapped.ends[root.end() - 1],
                            });
                        }
                    }
                }
            }
        }
        self.resolve(candidates)
    }

    pub(crate) fn find_word_matches(&self, data: &str, require_boundaries: &[bool]) -> Vec<Match> {
        let text = MappedText::strip_markup(data);
        let mut tokenizer = HebrewTokenizer {
            emit_quote_free: true,
            keep_marks: false,
        };
        let mut stream = tokenizer.token_stream(&text.text);
        let mut ranges = Vec::new();
        while stream.advance() {
            let token = stream.token();
            let mapped = text.normalized_token(
                token.offset_from,
                token.offset_to,
                !token.text.contains(['\'', '"']),
            );
            for (i, patterns) in self.independent_words.iter().enumerate() {
                let boundary = require_boundaries
                    .get(i)
                    .copied()
                    .unwrap_or(self.boundary_eligible[i]);
                let pattern = &patterns[usize::from(!boundary)];
                for captures in pattern.captures_iter(&mapped.text) {
                    let Some(root) = captures.iter().skip(1).flatten().next() else {
                        break;
                    };
                    if root.is_empty() {
                        break;
                    }
                    let root_start = root.start();
                    let root_end = root.end();
                    ranges.push(Range {
                        start: mapped.starts[root_start],
                        end: mapped.ends[root_end - 1],
                    });
                    if boundary {
                        break;
                    }
                }
            }
        }
        ranges.sort_by_key(|r| (r.start, r.end));
        let mut matches = Vec::new();
        let mut previous_end = 0;
        for range in ranges {
            if range.start < previous_end {
                continue;
            }
            previous_end = range.end;
            matches.push(Match {
                start: range.start,
                end: range.end,
                word_ranges: vec![Range {
                    start: 0,
                    end: range.end - range.start,
                }],
            });
        }
        matches
    }

    fn resolve(&self, candidates: Vec<Vec<Option<Range>>>) -> Vec<Match> {
        let token_count = candidates[0].len();
        if token_count == 0 {
            return Vec::new();
        }
        let word_count = candidates.len();
        // A successor is the earliest next position that can complete the
        // entire remaining phrase. A greedy choice without this viability
        // pass loses valid paths when a later word has a tighter gap.
        let mut successor = vec![vec![None; token_count]; word_count.saturating_sub(1)];
        let mut viable: Vec<bool> = candidates[word_count - 1]
            .iter()
            .map(Option::is_some)
            .collect();
        for i in (0..word_count.saturating_sub(1)).rev() {
            let mut next = vec![None; token_count + 1];
            for p in (0..token_count).rev() {
                next[p] = if viable[p] { Some(p) } else { next[p + 1] };
            }
            for (p, candidate) in candidates[i].iter().enumerate() {
                let max_next = p.saturating_add(self.gaps[i] as usize).saturating_add(1);
                successor[i][p] = if candidate.is_some() {
                    next[p + 1].filter(|&q| q <= max_next)
                } else {
                    None
                };
                viable[p] = successor[i][p].is_some();
            }
        }
        let mut matches = Vec::new();
        let mut previous_end = 0;
        for (p, &can_complete) in viable.iter().enumerate() {
            if !can_complete {
                continue;
            }
            let first = candidates[0][p].unwrap();
            if first.start < previous_end {
                continue;
            }
            let mut ranges = vec![first];
            let mut position = p;
            for i in 0..word_count.saturating_sub(1) {
                position = successor[i][position].unwrap();
                ranges.push(candidates[i + 1][position].unwrap());
            }
            let end = ranges.last().unwrap().end;
            previous_end = end;
            matches.push(Match {
                start: first.start,
                end,
                word_ranges: ranges
                    .into_iter()
                    .map(|r| Range {
                        start: r.start - first.start,
                        end: r.end - first.start,
                    })
                    .collect(),
            });
        }
        matches
    }
}

/// Each emitted UTF-8 byte maps to the original UTF-16 character range.
/// Token normalization may delete punctuation or expand a presentation form;
/// this mapping preserves exact display offsets through both operations.
#[derive(Default)]
struct MappedText {
    text: String,
    starts: Vec<usize>,
    ends: Vec<usize>,
}

impl MappedText {
    fn push(&mut self, c: char, start: usize, end: usize) {
        self.text.push(c);
        self.starts.extend(std::iter::repeat_n(start, c.len_utf8()));
        self.ends.extend(std::iter::repeat_n(end, c.len_utf8()));
    }

    fn extend_last(&mut self, end: usize) {
        if let Some(c) = self.text.chars().next_back() {
            let n = self.ends.len();
            self.ends[n - c.len_utf8()..].fill(end);
        }
    }

    fn strip_markup(data: &str) -> Self {
        let mut out = Self::default();
        let mut byte = 0;
        let mut utf16 = 0;
        let mut has_tag_end = true;
        let mut has_entity_end = true;
        while byte < data.len() {
            let rest = &data[byte..];
            let c = rest.chars().next().unwrap();
            if c == '<' && has_tag_end {
                if let Some(close) = rest.find('>') {
                    let end = utf16 + rest[..=close].encode_utf16().count();
                    if is_breaking_tag(&rest[1..close].chars().collect::<Vec<_>>()) {
                        out.push(' ', utf16, end);
                    }
                    byte += close + 1;
                    utf16 = end;
                    continue;
                }
                // Once no closing delimiter remains, never rescan the suffix
                // for each later '<'. Malformed markup stays linear too.
                has_tag_end = false;
            } else if c == '&' && has_entity_end {
                if let Some(close) = rest.find(';') {
                    if close <= 1 {
                        out.push(c, utf16, utf16 + c.len_utf16());
                        byte += c.len_utf8();
                        utf16 += c.len_utf16();
                        continue;
                    }
                    let end = utf16 + rest[..=close].encode_utf16().count();
                    if matches!(&rest[..=close], "&nbsp;" | "&thinsp;" | "&ensp;" | "&emsp;") {
                        out.push(' ', utf16, end);
                    }
                    byte += close + 1;
                    utf16 = end;
                    continue;
                }
                has_entity_end = false;
            }
            let end = utf16 + c.len_utf16();
            out.push(c, utf16, end);
            byte += c.len_utf8();
            utf16 = end;
        }
        out
    }

    fn normalized_token(&self, start: usize, end: usize, quote_free: bool) -> Self {
        let mut out = Self::default();
        for (i, c) in self.text[start..end].char_indices() {
            let source_start = self.starts[start + i];
            let source_end = self.ends[start + i];
            if is_word_mark(c) {
                out.extend_last(source_end);
            } else if is_transparent(c) {
                continue;
            } else if is_geresh(c) || is_gershayim(c) {
                if quote_free {
                    continue;
                }
                let mut quote = if is_geresh(c) { '\'' } else { '"' };
                if quote == '\'' && out.text.ends_with('\'') {
                    let previous_start = out.starts.pop().unwrap();
                    out.ends.pop();
                    out.text.pop();
                    quote = '"';
                    out.push(quote, previous_start, source_end);
                } else if quote == '"' && out.text.ends_with('"') {
                    out.extend_last(source_end);
                } else {
                    out.push(quote, source_start, source_end);
                }
            } else if let Some(folded) = fold_presentation_form(c) {
                for f in folded.chars().filter(|c| !is_word_mark(*c)) {
                    out.push(f, source_start, source_end);
                }
            } else {
                out.push(c, source_start, source_end);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_highlight::{build_display_highlight, build_display_highlight_from_terms};

    fn options(query: &str, enabled: &[&str]) -> HashMap<String, HashMap<String, bool>> {
        crate::hebrew_query::split_query_words(query)
            .iter()
            .enumerate()
            .map(|(i, word)| {
                (
                    format!("{word}_{i}"),
                    enabled.iter().map(|s| (s.to_string(), true)).collect(),
                )
            })
            .collect()
    }

    fn matcher(query: &str, enabled: &[&str], distance: u32) -> PreparedHighlightMatcher {
        build_display_highlight(
            query,
            distance,
            &HashMap::new(),
            &HashMap::new(),
            &options(query, enabled),
        )
        .unwrap()
        .matcher
    }

    #[test]
    fn morphology_uses_each_spelling_variants_bound() {
        let plan = matcher("אמר תורה", &["חלק ממילה", "כתיב מלא/חסר"], 0);
        let found = plan.find_matches("אמר אאבתרה", &[]);
        assert_eq!(
            found,
            vec![Match {
                start: 0,
                end: 10,
                word_ranges: vec![Range { start: 0, end: 3 }, Range { start: 7, end: 10 }]
            }]
        );
        assert!(plan.find_matches("אמר אאאבתורה", &[]).is_empty());
        let from_terms = build_display_highlight_from_terms(
            "אמר תורה",
            0,
            &HashMap::new(),
            &HashMap::new(),
            &options("אמר תורה", &["חלק ממילה", "כתיב מלא/חסר"]),
            &[vec!["אמר".into()], vec!["אאבתרה".into()]],
        )
        .unwrap();
        assert_eq!(from_terms.matcher.find_matches("אמר אאבתרה", &[]), found);
    }

    #[test]
    fn shorter_alternatives_keep_their_own_prefix_bound() {
        let plan = build_display_highlight(
            "אמר תורה",
            0,
            &HashMap::new(),
            &HashMap::from([(1, vec!["ב".into()])]),
            &options("אמר תורה", &["קידומות"]),
        )
        .unwrap()
        .matcher;
        assert_eq!(plan.find_matches("אמר אאגגדב", &[]).len(), 1);
        assert!(plan.find_matches("אמר אאגגדדתורה", &[]).is_empty());
    }

    #[test]
    fn grammatical_prefixes_do_not_accept_arbitrary_letters() {
        let plan = matcher("אמר תורה", &["קידומות דקדוקיות"], 0);
        assert_eq!(plan.find_matches("אמר כשבהתורה", &[]).len(), 1);
        assert!(plan.find_matches("אמר אבגדתורה", &[]).is_empty());
    }

    #[test]
    fn gap_viability_can_choose_a_later_intermediate_match() {
        let plan = build_display_highlight(
            "א ב ג",
            0,
            &HashMap::from([("0-1".into(), "2".into()), ("1-2".into(), "0".into())]),
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap()
        .matcher;
        let found = plan.find_matches("א ב ד ב ג", &[]);
        assert_eq!(
            found[0].word_ranges,
            vec![
                Range { start: 0, end: 1 },
                Range { start: 6, end: 7 },
                Range { start: 8, end: 9 }
            ]
        );
    }

    #[test]
    fn near_misses_are_bounded_even_when_final_word_is_present_elsewhere() {
        let plan = matcher("אמר אמר אמר אמר גיטין", &["קידומות"], 30);
        let repeated = "ואמר ".repeat(1000);
        assert!(plan.find_matches(&repeated, &[]).is_empty());
        assert!(plan
            .find_matches(&format!("גיטין {repeated}"), &[])
            .is_empty());
        assert!(plan
            .find_matches(&format!("{repeated}{}גיטין", "דבר ".repeat(31)), &[])
            .is_empty());
        let found = plan.find_matches(&format!("{repeated}גיטין"), &[]);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn single_word_partial_finds_repeated_roots_without_overlap() {
        let plan = matcher("א", &["חלק ממילה"], 0);
        let found = plan.find_matches("אאא", &[]);
        assert_eq!(found.len(), 3);
        assert_eq!(
            found.iter().map(|m| m.start).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(plan.find_matches("אאא", &[true]).is_empty());
        assert_eq!(
            plan.find_word_matches(&"א".repeat(10_000), &[]).len(),
            10_000
        );
    }

    #[test]
    fn source_ranges_use_utf16_and_include_marks_across_inline_markup() {
        let plan = matcher("אמר משה", &["קידומות"], 0);
        let data = "😀 אָמַר <b>וּ</b>מֹשֶׁה";
        let found = plan.find_matches(data, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start, 3);
        let units: Vec<u16> = data.encode_utf16().collect();
        let roots: Vec<String> = found[0]
            .word_ranges
            .iter()
            .map(|r| {
                String::from_utf16(&units[found[0].start + r.start..found[0].start + r.end])
                    .unwrap()
            })
            .collect();
        assert_eq!(roots, ["אָמַר", "מֹשֶׁה"]);
        assert_eq!(
            matcher("מילה", &[], 0).find_matches("מי<b>לה", &[]).len(),
            1
        );
        assert!(matcher("מילה", &[], 0)
            .find_matches("מי<br>לה", &[])
            .is_empty());
    }

    #[test]
    fn quote_free_twins_share_positions_and_source_offsets() {
        let plan = matcher("רמבם אמר", &[], 0);
        let found = plan.find_matches("רמב״ם אמר", &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].word_ranges[0], Range { start: 0, end: 5 });
        assert!(matcher("רמבם רמבם", &[], 0)
            .find_matches("רמב״ם", &[])
            .is_empty());
    }

    #[test]
    fn transparent_punctuation_matches_index_tokens_and_whitespace_entities_break() {
        let plan = matcher("אמר משה", &[], 0);
        assert_eq!(plan.find_matches("א.מר&nbsp;משה", &[]).len(), 1);
        assert!(plan.find_matches("אמר&amp;משה", &[]).is_empty());
        assert_eq!(matcher("אב", &[], 0).find_matches("א&amp;ב", &[]).len(), 1);
    }

    #[test]
    fn malformed_markup_keeps_linear_scanning_and_valid_suffix_matches() {
        let plan = matcher("אמר משה", &[], 0);
        for marker in ['<', '&'] {
            let data = format!("{}אמר משה", marker.to_string().repeat(100_000));
            assert_eq!(plan.find_matches(&data, &[]).len(), 1);
        }
    }

    #[test]
    fn per_word_matches_sort_and_trim_overlaps() {
        let plan = matcher("אב אבג", &["חלק ממילה"], 0);
        let found = plan.find_word_matches("אבג אב אבג", &[]);
        assert_eq!(
            found.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>(),
            vec![(0, 2), (4, 6), (7, 9)]
        );
    }

    #[test]
    fn oversized_query_does_not_panic_when_regex_exceeds_its_memory_limit() {
        let query = "א".repeat(100_000);
        assert!(build_display_highlight(
            &query,
            0,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new()
        )
        .is_none());
    }
}
