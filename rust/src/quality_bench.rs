//! Match-quality and DFA cost report over a real index, opened read-only:
//! `PROBE_INDEX=<index dir> cargo test --release --lib quality_bench -- --ignored --nocapture`.
//! Prints, per case, the compile and scan cost of the tantivy-fst reference
//! and of the regex-automata DFAs, the terms each selects, and whether each
//! probe token is matched and kept under the collection budgets.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tantivy::{Index, InvertedIndexReader};
use tantivy_fst::Automaton;

use super::*;
use crate::term_automaton;

const GP: &str = "קידומות דקדוקיות";
const GS: &str = "סיומות דקדוקיות";
const P: &str = "קידומות";
const SU: &str = "סיומות";
const SP: &str = "כתיב מלא/חסר";
const PA: &str = "חלק ממילה";
const TY: &str = "שגיאות כתיב";

fn options(query: &str, flags: &[&str]) -> HashMap<String, HashMap<String, bool>> {
    hebrew_query::split_query_words(&hebrew_query::normalize_for_index(query))
        .iter()
        .enumerate()
        .map(|(i, w)| {
            (
                format!("{w}_{i}"),
                flags.iter().map(|f| (f.to_string(), true)).collect(),
            )
        })
        .collect()
}

fn accepts<A: Automaton>(a: &A, s: &str) -> bool {
    let mut state = a.start();
    for &b in s.as_bytes() {
        if !a.can_match(&state) {
            return false;
        }
        state = a.accept(&state, b);
    }
    a.is_match(&state)
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn scan_terms<A: Automaton>(segments: &[Arc<InvertedIndexReader>], a: &A) -> HashMap<String, u64>
where
    A::State: Clone,
{
    let mut out: HashMap<String, u64> = HashMap::new();
    for inverted in segments {
        let mut stream = inverted.terms().search(a).into_stream().unwrap();
        while stream.advance() {
            if let Ok(t) = std::str::from_utf8(stream.key()) {
                *out.entry(t.to_string()).or_default() += u64::from(stream.value().doc_freq);
            }
        }
    }
    out
}

/// The terms a single-word search keeps, by the engine's collection policy.
fn selected(
    segments: &[Arc<InvertedIndexReader>],
    branches: &[String],
    typo: &[DfaWrapper],
    max_expansions: u32,
) -> (HashSet<String>, bool) {
    let automata = term_automaton::compile_branches(branches).unwrap();
    let (ranked, capped) = term_automaton::collect_ranked(
        segments,
        &automata,
        typo,
        term_automaton::COLLECT_CAP.max(max_expansions as usize),
    )
    .unwrap();
    let budget = std::env::var("BENCH_BUDGET")
        .ok()
        .and_then(|b| b.parse().ok())
        .unwrap_or(SINGLE_WORD_POSTINGS_BUDGET);
    let (kept, truncated) =
        term_automaton::within_budgets(ranked, capped, max_expansions as usize, budget);
    let matched: HashSet<String> = kept.into_iter().map(|t| t.term).collect();
    (matched, truncated)
}

struct Case {
    id: &'static str,
    query: &'static str,
    flags: &'static [&'static str],
    /// (token, desired)
    probes: &'static [(&'static str, bool)],
}

const CASES: &[Case] = &[
    Case {
        id: "1",
        query: "מלך",
        flags: &[GS],
        probes: &[("מלכים", true), ("מלכו", true), ("מלכיהם", true)],
    },
    Case {
        id: "2",
        query: "ארץ",
        flags: &[GS],
        probes: &[("ארצות", true), ("ארצו", true)],
    },
    Case {
        id: "3",
        query: "תורה",
        flags: &[GS],
        probes: &[("תורות", true), ("תורתו", true), ("תורתי", true)],
    },
    Case {
        id: "4",
        query: "תורה",
        flags: &[GS],
        probes: &[("תורהו", false), ("תורהה", false)],
    },
    Case {
        id: "5",
        query: "אמר",
        flags: &[GP, GS],
        probes: &[
            ("אמרתי", true),
            ("אמרת", true),
            ("ואמרתם", true),
            ("אמרנו", true),
        ],
    },
    Case {
        id: "6",
        query: "אמר",
        flags: &[GP],
        probes: &[("כשאמר", true), ("וכשאמר", true)],
    },
    Case {
        id: "7",
        query: "אמר",
        flags: &[GP],
        probes: &[("מאמר", false), ("ואמר", true)],
    },
    Case {
        id: "7b",
        query: "אמר",
        flags: &[GP, GS],
        probes: &[("מאמרים", false)],
    },
    Case {
        id: "8",
        query: "ית",
        flags: &[GP],
        probes: &[("בית", false)],
    },
    Case {
        id: "9",
        query: "מצוה",
        flags: &[SP],
        probes: &[("מצה", false), ("מצווה", true)],
    },
    Case {
        id: "10",
        query: "קודש",
        flags: &[SP],
        probes: &[("קדוש", false), ("קדש", true)],
    },
    Case {
        id: "11",
        query: "שבת",
        flags: &[SP],
        probes: &[("שבות", false), ("שבית", false), ("שובת", false)],
    },
    Case {
        id: "12",
        query: "שבת",
        flags: &[P, GS],
        probes: &[("שבתות", true), ("בשבתות", true)],
    },
    Case {
        id: "13",
        query: "שבת",
        flags: &[GP, SU],
        probes: &[("בשבת", true), ("בשבתא", true)],
    },
    Case {
        id: "14",
        query: "שבת",
        flags: &[TY],
        probes: &[("שבט", true), ("סבת", true), ("שב", true), ("שבתי", true)],
    },
    Case {
        id: "14b",
        query: "אב",
        flags: &[TY],
        probes: &[("א", false), ("ב", false), ("אם", false)],
    },
    Case {
        id: "14c",
        query: "ברכות",
        flags: &[TY],
        probes: &[("ברכת", true), ("בכרות", true)],
    },
    Case {
        id: "15",
        query: "אמר",
        flags: &[GP],
        probes: &[("דאמר", true), ("ואמר", true), ("שאמר", true)],
    },
    Case {
        id: "16",
        query: "אמר",
        flags: &[GP, GS],
        probes: &[
            ("דאמר", true),
            ("ואמר", true),
            ("שאמר", true),
            ("אמרו", true),
        ],
    },
    Case {
        id: "17",
        query: "שבת קודש",
        flags: &[GP, GS, SP],
        probes: &[("הקדוש", false), ("הקודש", true)],
    },
    Case {
        id: "B",
        query: "שבת",
        flags: &[P],
        probes: &[("בשבת", true), ("1שבת", false)],
    },
    Case {
        id: "L",
        query: "שבת",
        flags: &[PA],
        probes: &[("מחשבתו", true), ("בשבתו", true)],
    },
    Case {
        id: "X1",
        query: "שבת",
        flags: &[GP, GS],
        probes: &[("ובשבתותיכם", true), ("לשבתך", true)],
    },
    Case {
        id: "X2",
        query: "תפלה",
        flags: &[GP, GS, SP],
        probes: &[("ובתפילתו", true), ("תפלות", true)],
    },
    Case {
        id: "X3",
        query: "מצוה",
        flags: &[GS, SP],
        probes: &[("מצוות", true), ("מצותיו", true)],
    },
    Case {
        id: "X4",
        query: "אמר",
        flags: &[P, SU],
        probes: &[("ואמרו", true)],
    },
    Case {
        id: "X5",
        query: "שבת",
        flags: &[PA, TY],
        probes: &[("בשבט", true)],
    },
    Case {
        id: "X6",
        query: "ב",
        flags: &[P, SU],
        probes: &[],
    },
];

#[test]
#[ignore]
fn quality_bench() {
    let path = std::env::var("PROBE_INDEX").expect("PROBE_INDEX");
    let index = Index::open_in_dir(&path).unwrap();
    let searcher = index.reader().unwrap().searcher();
    let text = searcher.schema().get_field("text").unwrap();
    let segments: Vec<Arc<InvertedIndexReader>> = searcher
        .segment_readers()
        .iter()
        .map(|r| r.inverted_index(text).unwrap())
        .collect();
    println!("segments={}", segments.len());
    for case in CASES {
        let advanced = hebrew_query::prepare_advanced_query(
            case.query,
            0,
            &HashMap::new(),
            &HashMap::new(),
            &options(case.query, case.flags),
        );
        println!(
            "\n## [{}] {} {:?} max_expansions={} typo_tokens={:?}",
            case.id, case.query, case.flags, advanced.max_expansions, advanced.typo_tokens
        );
        let typo: Vec<DfaWrapper> = advanced
            .typo_tokens
            .iter()
            .map(|t| {
                FuzzyTermQuery::new(Term::from_field_text(text, t), 1, true)
                    .automaton()
                    .unwrap()
            })
            .collect();
        for (w, pattern) in advanced.regex_terms.iter().enumerate() {
            let branches = pattern.branches();
            let t = Instant::now();
            let old: Vec<tantivy_fst::Regex> = branches
                .iter()
                .map(|b| tantivy_fst::Regex::new(b).unwrap())
                .collect();
            let old_compile = ms(t);
            let t = Instant::now();
            let mut old_terms: HashMap<String, u64> = HashMap::new();
            for re in &old {
                old_terms.extend(scan_terms(&segments, re));
            }
            let old_scan = ms(t);
            let t = Instant::now();
            let new = term_automaton::build_uncached(branches).unwrap();
            let new_compile = ms(t);
            let t = Instant::now();
            let mut new_terms: HashMap<String, u64> = HashMap::new();
            for part in &new.parts {
                new_terms.extend(scan_terms(&segments, part.as_ref()));
            }
            let new_scan = ms(t);
            let mut all = new_terms.clone();
            for a in &typo {
                all.extend(scan_terms(&segments, a));
            }
            let same: BTreeSet<&String> = old_terms.keys().collect();
            let same = same == new_terms.keys().collect::<BTreeSet<_>>();
            let df: u64 = all.values().sum();
            println!(
                "  word[{w}] branches={} chars={} | tantivy-fst compile={old_compile:.1}ms scan={old_scan:.1}ms terms={} | regex-automata dfas={} compile={new_compile:.1}ms scan={new_scan:.1}ms terms={} | same={same} | +typo terms={} Σdf={df}",
                branches.len(),
                pattern.joined().chars().count(),
                old_terms.len(),
                new.parts.len(),
                new_terms.len(),
                all.len(),
            );
            let joined = pattern.joined();
            let t = Instant::now();
            let old_joined = tantivy_fst::Regex::new(&joined)
                .map(|_| ())
                .map_err(|e| e.to_string());
            let old_joined_ms = ms(t);
            let t = Instant::now();
            let new_joined = term_automaton::build_uncached(std::slice::from_ref(&joined))
                .map(|a| a.parts[0].memory_usage() / 1024);
            let new_joined_ms = ms(t);
            println!(
                "  joined: tantivy-fst {:?} in {old_joined_ms:.1}ms | regex-automata {:?}KB in {new_joined_ms:.1}ms",
                old_joined.map(|_| "ok"),
                new_joined
            );
            if advanced.regex_terms.len() == 1 {
                let t = Instant::now();
                let (kept, truncated) =
                    selected(&segments, branches, &typo, advanced.max_expansions);
                let select_ms = ms(t);
                let terms: Vec<Term> = kept
                    .iter()
                    .map(|k| Term::from_field_text(text, k))
                    .collect();
                let df: u64 = kept.iter().map(|k| all.get(k).copied().unwrap_or(0)).sum();
                let t = Instant::now();
                let docs = searcher
                    .search(&TermSetQuery::new(terms), &tantivy::collector::Count)
                    .unwrap();
                println!(
                    "  selection: kept={} Σdf={df} truncated={truncated} in {select_ms:.1}ms | count={docs} in {:.1}ms",
                    kept.len(),
                    ms(t)
                );
                for (token, desired) in case.probes {
                    let matched = new.parts.iter().any(|p| accepts(p.as_ref(), token))
                        || typo.iter().any(|a| accepts(a, token));
                    println!(
                        "  | {token} | desired={desired} | matched={matched} | kept={} | df={}",
                        kept.contains(*token),
                        all.get(*token).copied().unwrap_or(0)
                    );
                }
            } else {
                for (token, desired) in case.probes {
                    let matched = new.parts.iter().any(|p| accepts(p.as_ref(), token));
                    println!("  | word{w} {token} | desired={desired} | matched={matched}");
                }
            }
        }
    }
}
