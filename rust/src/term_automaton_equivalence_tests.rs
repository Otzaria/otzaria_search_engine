//! Equivalence of the `regex-automata` term DFAs with `tantivy_fst::Regex`:
//! for every pattern the query builders emit — every option combination,
//! spelling, typo variants, partial words, nikud, quotes — both engines must
//! select exactly the same terms from a dictionary built to exercise them.

use std::collections::{BTreeSet, HashMap};

use tantivy_fst::{Automaton, IntoStreamer, Map, Streamer};

use crate::hebrew_query::{
    normalize_for_index, prepare_advanced_query, prepare_advanced_query_vocalized,
    split_query_words, VocalizedFlags, WordPattern,
};
use crate::term_automaton::{compile_branches, compile_pattern};

const GP: &str = "קידומות דקדוקיות";
const GS: &str = "סיומות דקדוקיות";
const P: &str = "קידומות";
const SU: &str = "סיומות";
const SP: &str = "כתיב מלא/חסר";
const PA: &str = "חלק ממילה";
const TY: &str = "שגיאות כתיב";
const IQ: &str = "התעלם מגרשיים";
const AP: &str = "קידומות ארמיות";
const AS: &str = "סיומות ארמיות";
const NK: &str = "ניקוד";

const WORDS: &[&str] = &[
    "שבת",
    "אמר",
    "תורה",
    "מלך",
    "ארץ",
    "מצוה",
    "מצווה",
    "קודש",
    "קדש",
    "תפלה",
    "תפילה",
    "ית",
    "בית",
    "אב",
    "ב",
    "רמב\"ם",
    "רמבם",
    "ר'",
    "יהודה",
    "ירושלים",
    "ברכות",
    "השם",
    "קידוש",
    "זימון",
    "ספר",
    "משה",
    "חכמים",
    "מלכא",
    "שבתא",
    "דאמר",
    "ובשבתות",
    "שבתות",
    "תוס'",
];

const PREFIXES: &[&str] = &[
    "", "ו", "ב", "ה", "ש", "כש", "וכש", "ד", "דא", "א", "ל", "מ", "וב", "ול", "וה", "שב", "מה",
    "בה", "כ", "לה", "ושב", "כשה",
];

const SUFFIXES: &[&str] = &[
    "",
    "ו",
    "י",
    "ה",
    "ים",
    "ות",
    "ותיו",
    "יהם",
    "תי",
    "ת",
    "תם",
    "נו",
    "א",
    "ין",
    "ותיהם",
    "ך",
    "תא",
    "יות",
    "כם",
];

const LETTERS: &[char] = &[
    'א', 'ב', 'ג', 'ד', 'ה', 'ו', 'ז', 'ח', 'ט', 'י', 'כ', 'ך', 'ל', 'מ', 'ם', 'נ', 'ן', 'ס', 'ע',
    'פ', 'ף', 'צ', 'ץ', 'ק', 'ר', 'ש', 'ת', '"', '\'',
];

fn non_final(c: char) -> char {
    match c {
        'ך' => 'כ',
        'ם' => 'מ',
        'ן' => 'נ',
        'ף' => 'פ',
        'ץ' => 'צ',
        c => c,
    }
}

/// A deterministic dictionary: affixed and edited forms of [`WORDS`], some
/// vocalized surfaces, and pseudo-random strings over the Hebrew alphabet.
fn dictionary() -> Map<Vec<u8>> {
    let mut terms: BTreeSet<String> = BTreeSet::new();
    for word in WORDS {
        let chars: Vec<char> = word.chars().collect();
        let mut stems = vec![word.to_string()];
        let last = *chars.last().unwrap();
        if non_final(last) != last || last == 'ה' {
            let mut stem: String = chars[..chars.len() - 1].iter().collect();
            if last != 'ה' {
                stem.push(non_final(last));
            }
            stems.push(stem);
        }
        for stem in &stems {
            for prefix in PREFIXES {
                for suffix in SUFFIXES {
                    terms.insert(format!("{prefix}{stem}{suffix}"));
                }
            }
        }
        for i in 0..chars.len() {
            let mut deleted = chars.clone();
            deleted.remove(i);
            terms.insert(deleted.iter().collect());
            for &c in LETTERS {
                let mut substituted = chars.clone();
                substituted[i] = c;
                terms.insert(substituted.iter().collect());
            }
        }
        for i in 0..=chars.len() {
            for &c in LETTERS {
                let mut inserted = chars.clone();
                inserted.insert(i, c);
                terms.insert(inserted.iter().collect());
            }
        }
    }
    for vocalized in [
        "שַׁבָּת",
        "בְּשַׁבָּת",
        "וּבַשַּׁבָּת",
        "אָמַר",
        "וַיֹּאמֶר",
        "תּוֹרָה",
        "בָּרָ\u{0307}א",
        "שַׁבָּ֑ת",
        "מֶלֶךְ",
    ] {
        terms.insert(vocalized.to_string());
    }
    for other in ["abc", "x1", "x12", "ספר1", "123"] {
        terms.insert(other.to_string());
    }
    let mut seed: u64 = 0x5eed;
    for _ in 0..4000 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = 2 + (seed >> 59) as usize % 7;
        let s: String = (0..len)
            .map(|i| LETTERS[((seed >> (i * 5)) as usize) % LETTERS.len()])
            .collect();
        terms.insert(s);
    }
    terms.retain(|t| !t.is_empty());
    Map::from_iter(terms.into_iter().map(|t| (t, 0u64))).unwrap()
}

fn search<A: Automaton>(map: &Map<Vec<u8>>, automaton: &A) -> BTreeSet<Vec<u8>> {
    let mut out = BTreeSet::new();
    let mut stream = map.search(automaton).into_stream();
    while let Some((key, _)) = stream.next() {
        out.insert(key.to_vec());
    }
    out
}

fn options(query: &str, flags: &[&str]) -> HashMap<String, HashMap<String, bool>> {
    let words = split_query_words(&normalize_for_index(query));
    words
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

/// Every word pattern of the builder over the query × option matrix.
/// `heavy` adds the widest combinations (spelling × typo × windows), whose
/// tantivy-fst reference compiles take minutes in a debug build.
fn builder_patterns(heavy: bool) -> Vec<(String, WordPattern)> {
    let light: &[&[&str]] = &[
        &[],
        &[GP],
        &[GS],
        &[GP, GS],
        &[P],
        &[SU],
        &[P, SU],
        &[PA],
        &[SP],
        &[TY],
        &[IQ],
        &[GP, GS, SP],
        &[GP, TY],
        &[GS, TY],
        &[P, GS],
        &[GP, SU],
        &[AP],
        &[AS],
        &[AP, AS],
        &[AP, AS, GS],
        &[AP, SP],
        &[IQ, GP, GS],
    ];
    let wide: &[&[&str]] = &[&[GP, GS, SP, TY], &[P, SU, SP, TY], &[SP, TY], &[PA, TY]];
    let combos: Vec<&[&str]> = if heavy {
        light.iter().chain(wide).copied().collect()
    } else {
        light
            .iter()
            .copied()
            .filter(|c| !wide.contains(c))
            .collect()
    };
    let queries = [
        "שבת",
        "אמר",
        "תורה",
        "מלך",
        "ארץ",
        "מצוה",
        "קודש",
        "ית",
        "אב",
        "ב",
        "רמב\"ם",
        "ר' יהודה",
        "שבת קודש",
        "קידוש השם",
        "ובשבתות",
        "תוס'",
    ];
    let mut out = Vec::new();
    for query in queries {
        for flags in &combos {
            let advanced = prepare_advanced_query(
                query,
                0,
                &HashMap::new(),
                &HashMap::new(),
                &options(query, flags),
            );
            for (i, pattern) in advanced.regex_terms.into_iter().enumerate() {
                out.push((format!("{query} {flags:?} word {i}"), pattern));
            }
        }
    }
    let alternatives: HashMap<u32, Vec<String>> = HashMap::from([(0, vec!["שבתא".to_string()])]);
    for flags in [&[TY][..], &[GP, GS], &[SP]] {
        let advanced = prepare_advanced_query(
            "שבת",
            0,
            &HashMap::new(),
            &alternatives,
            &options("שבת", flags),
        );
        out.push((
            format!("שבת+alt {flags:?}"),
            advanced.regex_terms[0].clone(),
        ));
    }
    for (query, flags) in [
        ("שַׁבָּת", &[][..]),
        ("שַׁבָּת", &[GP, GS]),
        ("שַׁבָּת", &[P, SU]),
        ("שַׁבָּת", &[SP, TY]),
        ("אָמַר", &[NK, GP]),
        ("וַיֹּאמֶר", &[PA]),
    ] {
        let words = split_query_words(&crate::hebrew_query::normalize_for_index_vocalized(query));
        let opts: HashMap<String, HashMap<String, bool>> = words
            .iter()
            .enumerate()
            .map(|(i, w)| {
                (
                    format!("{w}_{i}"),
                    flags.iter().map(|f| (f.to_string(), true)).collect(),
                )
            })
            .collect();
        let advanced = prepare_advanced_query_vocalized(
            query,
            0,
            &HashMap::new(),
            &HashMap::new(),
            &opts,
            &VocalizedFlags::new(true, false),
        );
        for (i, pattern) in advanced.regex_terms.into_iter().enumerate() {
            out.push((format!("vocalized {query} {flags:?} word {i}"), pattern));
        }
    }
    for raw in [
        ".*ספר",
        "x[0-9]+",
        "(?i)ABC",
        "ספר.{0,3}",
        "[^א]{2}",
        "(?:שבת|אמר)(?:ו|י)?",
    ] {
        out.push((format!("raw {raw}"), WordPattern::parse(raw)));
    }
    out
}

#[test]
fn regex_automata_terms_equal_tantivy_fst_terms() {
    assert_equivalent(false, 2_000);
}

/// The full matrix; run in release: `cargo test --release -- --ignored equivalent`.
#[test]
#[ignore]
fn regex_automata_terms_equal_tantivy_fst_terms_wide() {
    assert_equivalent(true, usize::MAX);
}

/// `joined_limit` skips the joined-pattern comparison above that many chars.
fn assert_equivalent(heavy: bool, joined_limit: usize) {
    let map = dictionary();
    let mut checked = 0usize;
    for (name, pattern) in builder_patterns(heavy) {
        let branches = pattern.branches();
        let old: Vec<tantivy_fst::Regex> = branches
            .iter()
            .map(|b| tantivy_fst::Regex::new(b).unwrap_or_else(|e| panic!("{name}: {b:?}: {e}")))
            .collect();
        let mut expected: BTreeSet<Vec<u8>> = BTreeSet::new();
        for regex in &old {
            expected.extend(search(&map, regex));
        }
        let automata = compile_branches(branches).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut actual: BTreeSet<Vec<u8>> = BTreeSet::new();
        for part in &automata.parts {
            actual.extend(search(&map, part.as_ref()));
        }
        assert_eq!(
            actual.len(),
            expected.len(),
            "{name}: term count differs (branches {branches:?})"
        );
        assert_eq!(actual, expected, "{name}: term sets differ");
        for term in &actual {
            let first_old = old.iter().position(|re| {
                let mut state = re.start();
                for &b in term {
                    state = re.accept(&state, b);
                }
                re.is_match(&state)
            });
            let first_new = automata.parts.iter().find_map(|p| p.matched_branch(term));
            assert_eq!(
                first_new,
                first_old,
                "{name}: branch of {:?}",
                String::from_utf8_lossy(term)
            );
        }
        let joined = pattern.joined();
        if joined.chars().count() > joined_limit {
            checked += 1;
            continue;
        }
        if let Ok(old_joined) = tantivy_fst::Regex::new(&joined) {
            let new_joined = compile_pattern(&joined).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                search(&map, new_joined.as_ref()),
                search(&map, &old_joined),
                "{name}: joined pattern differs"
            );
        }
        checked += 1;
    }
    assert!(checked > 400, "only {checked} patterns checked");
}
