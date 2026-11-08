#[test]
fn acquiring_the_writer_cleans_only_abandoned_spool_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    let stale = dir.path().join(".otzaria-prepare-crashed");
    let other = dir.path().join("unrelated-user-file");
    let folder = dir.path().join(".otzaria-prepare-folder");
    std::fs::write(&stale, "abandoned").unwrap();
    std::fs::write(&other, "keep").unwrap();
    std::fs::create_dir(&folder).unwrap();
    engine.open_index_writer().unwrap();
    assert!(!stale.exists());
    assert_eq!(std::fs::read_to_string(other).unwrap(), "keep");
    assert!(folder.is_dir());
}

#[test]
fn another_engine_cannot_clean_spools_before_acquiring_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = SearchEngine::new(dir.path().to_str().unwrap());
    first.open_index_writer().unwrap();
    let owned = dir.path().join(".otzaria-prepare-owned-by-writer");
    std::fs::write(&owned, "still in use").unwrap();
    let mut second = SearchEngine::new(dir.path().to_str().unwrap());
    assert!(owned.exists());
    assert!(second.open_index_writer().is_err());
    assert_eq!(std::fs::read_to_string(&owned).unwrap(), "still in use");
    drop(first);
    second.open_index_writer().unwrap();
    assert!(!owned.exists());
}

#[test]
fn closing_the_writer_discards_uncommitted_writes_and_releases_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = SearchEngine::new(dir.path().to_str().unwrap());
    first.open_index_writer().unwrap();
    for id in 1..=200 {
        add(&mut first, id, "שלום עולם", "id:1");
    }
    first.close_index_writer();
    assert!(!first.holds_writer_for_tests());
    first.close_index_writer();
    assert!(first.open_index_writer().is_err());
    assert!(first.commit().is_err());
    assert!(!first.holds_writer_for_tests());

    let mut second = SearchEngine::new(dir.path().to_str().unwrap());
    second.open_index_writer().unwrap();
    second.commit().unwrap();
    assert_eq!(second.searcher_for_tests().num_docs(), 0);
    drop(first);
}

#[test]
fn spooled_text_book_is_enqueued_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    engine.open_index_writer().unwrap();
    let book = TextBookMeta {
        title: "ספר".into(),
        topics: "/ספר".into(),
        file_path: "id:1".into(),
        extra_facets: Vec::new(),
        source_hash: 0,
    };
    let mut text = crate::indexing_buffers::TextBuffer::new(16);
    text.push_str("ראשונה\nשנייה\nשלישית").unwrap();
    let mut stored = crate::indexing_buffers::OrdinalBuffer::default();
    let mut prepared = engine
        .prepare_text_book_buffer(
            &book,
            &mut text,
            TextStorage::InIndex,
            &mut stored,
            1,
            &|| false,
        )
        .unwrap();
    assert!(prepared.spooled_documents.is_some());
    let writer = engine.index_writer.as_ref().unwrap();
    assert_eq!(prepared.enqueue(writer).unwrap(), 3);
    assert_eq!(prepared.enqueue(writer).unwrap(), 0);
    engine.commit().unwrap();
    assert_eq!(engine.searcher_for_tests().num_docs(), 3);
}

use super::*;

/// The generation a test book is placed at when it names none.
const DEFAULT_GENERATION_ORDER: u32 = 5;

#[test]
fn crop_keeps_the_whole_first_occurrence_and_whole_words() {
    let text = format!(
        "{} מי שטרח בערב שבת {}",
        "אם את ".repeat(200),
        "יאכל ".repeat(200)
    );
    let occ_start = text.find("מי").unwrap();
    let ranges: Vec<(usize, usize)> = ["מי", "שטרח", "בערב", "שבת"]
        .iter()
        .map(|w| {
            let s = text[occ_start..].find(w).unwrap() + occ_start;
            (s, s + w.len())
        })
        .collect();
    let (cropped, shifted) = crop_around_first_occurrence(&text, &ranges, 4, 120).unwrap();
    assert!(cropped.len() <= 120);
    assert!(cropped.contains("מי שטרח בערב שבת"));
    assert_eq!(shifted.len(), 4);
    assert_eq!(&cropped[shifted[1].0..shifted[1].1], "שטרח");
    assert!(!cropped.starts_with(char::is_whitespace));
    assert!(cropped.starts_with("אם") || cropped.starts_with("את"));
}
#[test]
fn crop_handles_multibyte_whitespace() {
    for space in ['\u{00a0}', '\u{2003}', '\u{202f}'] {
        let text = format!("abcdefghij{space}מי שטרח trailing context");
        let start = text.find("מי").unwrap();
        let ranges = [(start, start + 4), (start + 5, start + 13)];
        let (cropped, shifted) = crop_around_first_occurrence(&text, &ranges, 2, 21).unwrap();
        assert!(cropped.contains("מי שטרח"));
        assert_eq!(&cropped[shifted[0].0..shifted[0].1], "מי");
    }
}

#[test]
fn crop_does_not_highlight_partial_neighboring_occurrences() {
    let text = "aa bb aa bb";
    let ranges = [(0, 2), (3, 5), (6, 8), (9, 11)];
    let (cropped, shifted) = crop_around_first_occurrence(text, &ranges, 2, 9).unwrap();
    assert!(cropped.len() <= 9);
    assert_eq!(shifted, [(0, 2), (3, 5)]);
}

#[test]
fn crop_does_not_split_context_words_at_punctuation() {
    let text = "abcdefghij,מי שטרח,klmnopqrst";
    let start = text.find("מי").unwrap();
    let ranges = [(start, start + 4), (start + 5, start + 13)];
    let (cropped, _) = crop_around_first_occurrence(text, &ranges, 2, 21).unwrap();
    assert_eq!(cropped, "מי שטרח");
}

#[test]
fn phrase_window_rejects_occurrences_larger_than_budget() {
    let (engine, _dir) = make_engine();
    let phrase = PhraseHighlight {
        per_word_terms: vec![
            HashSet::from(["משה".into()]),
            HashSet::from(["ואהרן".into()]),
        ],
        proximity: Proximity::Ordered(vec![1000]),
        analyzer: DISPLAY_ANALYZER,
        cross_line: false,
        alternatives: Vec::new(),
        allow_term_fallback: true,
    };
    let text = format!("משה {} ואהרן", "שלום ".repeat(100));
    assert!(SearchEngine::phrase_filtered_snippet_html(
        &engine.index_reader.searcher(),
        &text,
        &phrase,
        &HighlightConfig::default(),
        Some(80),
    )
    .is_none());
}

#[test]
fn phrase_window_does_not_use_quote_free_twin_as_a_second_word() {
    let (engine, _dir) = make_engine();
    let phrase = PhraseHighlight {
        per_word_terms: vec![
            HashSet::from(["רמב\"ם".into()]),
            HashSet::from(["רמבם".into()]),
        ],
        proximity: Proximity::Ordered(vec![0]),
        analyzer: DISPLAY_ANALYZER,
        cross_line: false,
        alternatives: Vec::new(),
        allow_term_fallback: true,
    };
    // The final word produces an indexing-only quote-free twin at the same
    // position. That is one word, not a strict two-word phrase.
    let text = format!("{}{}רמב\"ם", "רמבם ".repeat(100), "סתם ".repeat(100));
    assert!(SearchEngine::phrase_filtered_snippet_html(
        &engine.index_reader.searcher(),
        &text,
        &phrase,
        &HighlightConfig::default(),
        Some(80),
    )
    .is_none());
}

#[test]
fn phrase_occurrences_agree_with_exhaustive_ordered_gap_oracle() {
    fn complete(
        words: &[&str],
        sets: &[HashSet<String>],
        gaps: &[u32],
        chosen: &mut Vec<usize>,
    ) -> bool {
        let w = chosen.len();
        if w == sets.len() {
            return true;
        }
        let start = chosen.last().map_or(0, |&last| last + 1);
        let end = chosen.last().map_or(words.len(), |&last| {
            words.len().min(last + gaps[w - 1] as usize + 2)
        });
        for i in start..end {
            if sets[w].contains(words[i]) {
                chosen.push(i);
                if complete(words, sets, gaps, chosen) {
                    return true;
                }
                chosen.pop();
            }
        }
        false
    }
    let (engine, _dir) = make_engine();
    let searcher = engine.index_reader.searcher();
    let sets = vec![
        HashSet::from(["אחת".into()]),
        HashSet::from(["אחת".into(), "שתים".into()]),
        HashSet::from(["שלש".into()]),
    ];
    for len in 0..=6u32 {
        for mut encoding in 0..3usize.pow(len) {
            let words: Vec<_> = (0..len)
                .map(|_| {
                    let word = ["אחת", "שתים", "שלש"][encoding % 3];
                    encoding /= 3;
                    word
                })
                .collect();
            let text = words.join(" ");
            let mut offsets = Vec::new();
            let mut offset = 0;
            for word in &words {
                offsets.push((offset, offset + word.len()));
                offset += word.len() + 1;
            }
            for gaps in [vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1], vec![2, 2]] {
                let phrase = PhraseHighlight {
                    per_word_terms: sets.clone(),
                    proximity: Proximity::Ordered(gaps.clone()),
                    analyzer: DISPLAY_ANALYZER,
                    cross_line: false,
                    alternatives: Vec::new(),
                    allow_term_fallback: true,
                };
                let mut expected = Vec::new();
                let mut after = 0;
                while after < words.len() {
                    let mut chosen = Vec::new();
                    if !complete(&words[after..], &sets, &gaps, &mut chosen) {
                        break;
                    }
                    expected.extend(chosen.iter().map(|&i| offsets[after + i]));
                    after += chosen.last().unwrap() + 1;
                }
                let actual = SearchEngine::phrase_occurrences(&searcher, &text, &phrase);
                assert_eq!(actual.unwrap_or_default(), expected, "{text:?}, {gaps:?}");
            }
        }
    }
}

#[test]
fn phrase_occurrences_preserve_shared_positions_and_nonoverlap() {
    let (engine, _dir) = make_engine();
    let searcher = engine.index_reader.searcher();
    let phrase = PhraseHighlight {
        per_word_terms: vec![
            HashSet::from(["רמב\"ם".into(), "רמבם".into()]),
            HashSet::from(["רמב\"ם".into(), "רמבם".into()]),
        ],
        proximity: Proximity::Ordered(vec![0]),
        analyzer: DISPLAY_ANALYZER,
        cross_line: false,
        alternatives: Vec::new(),
        allow_term_fallback: true,
    };
    let text = "רמב\"ם רמב\"ם רמב\"ם";
    let ranges = SearchEngine::phrase_occurrences(&searcher, text, &phrase).unwrap();
    assert_eq!(
        ranges.len(),
        2,
        "one non-overlapping occurrence, not quote-free twins"
    );
    assert_eq!(&text[ranges[0].0..ranges[0].1], "רמב\"ם");
    assert_eq!(&text[ranges[1].0..ranges[1].1], "רמב\"ם");
    let phrase = PhraseHighlight {
        per_word_terms: vec![
            HashSet::from(["הוצא".into()]),
            HashSet::from(["היצא".into()]),
        ],
        ..phrase
    };
    assert!(SearchEngine::phrase_occurrences(&searcher, "(הוצא) [היצא]", &phrase).is_none());
    assert_eq!(
        SearchEngine::phrase_occurrences(&searcher, "(הוצא) [היצא] (הוצא) [היצא]", &phrase)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn crop_preserves_utf8_and_budget_across_context_boundaries() {
    for prefix in ["", "שלום ", "abc\u{00a0}", "אבג,", "🙂\u{2003}"] {
        for suffix in ["", " עולם", "\u{202f}abc", ",אבג", "\u{2003}🙂"] {
            let text = format!("{prefix}מי שטרח{suffix}");
            let start = prefix.len();
            let ranges = [(start, start + 4), (start + 5, start + 13)];
            for budget in 0..=text.len() + 2 {
                let result = crop_around_first_occurrence(&text, &ranges, 2, budget);
                if budget < 13 {
                    assert!(result.is_none());
                } else {
                    let (cropped, shifted) = result.unwrap();
                    assert!(cropped.len() <= budget);
                    assert!(cropped.contains("מי שטרח"));
                    assert_eq!(shifted.len(), 2);
                    assert_eq!(&cropped[shifted[0].0..shifted[0].1], "מי");
                    assert_eq!(&cropped[shifted[1].0..shifted[1].1], "שטרח");
                }
            }
        }
    }
}

#[test]
fn phrase_window_fallback_is_used_by_result_renderers() {
    let (mut engine, _dir) = make_engine();
    let text = format!(
        "{} {} &<> משה ואהרן {}",
        "משה ".repeat(100),
        "שלום ".repeat(100),
        "שלום ".repeat(100)
    );
    add(&mut engine, 1, &text, "/books/a.txt");
    engine.commit().unwrap();
    let searcher = engine.index_reader.searcher();
    let phrase = PhraseHighlight {
        per_word_terms: vec![
            HashSet::from(["משה".into()]),
            HashSet::from(["ואהרן".into()]),
        ],
        proximity: Proximity::Ordered(vec![0]),
        analyzer: DISPLAY_ANALYZER,
        cross_line: false,
        alternatives: Vec::new(),
        allow_term_fallback: true,
    };
    let field = engine.schema.get_field("text").unwrap();
    let query = engine
        .terms_query_from_word_sets(&phrase.per_word_terms, field)
        .unwrap()
        .unwrap();
    let hl = HighlightConfig {
        max_chars: 80,
        ..HighlightConfig::default()
    };
    let generator =
        SearchEngine::make_snippet_generator(&searcher, query.as_ref(), field, &hl).unwrap();
    // Prove the term-density fragment misses the phrase, so this exercises
    // the whole-line fallback rather than the original fragment path.
    assert!(!generator.snippet(&text).fragment().contains("ואהרן"));
    let addresses = searcher
        .search(&AllQuery, &TopDocs::with_limit(1).order_by_score())
        .unwrap()
        .into_iter()
        .map(|(_, address)| address)
        .collect();
    let results = SearchEngine::build_results_with_generator(
        &engine.schema,
        &searcher,
        &generator,
        field,
        addresses,
        &hl,
        Some(&phrase),
    )
    .unwrap();
    let html = &results[0].text;
    assert_eq!(highlight_count(html), 2);
    assert!(html.contains("<font color=red>משה</font> <font color=red>ואהרן</font>"));
    let plain = htmlescape::decode_html(
        &html
            .replace(&hl.highlight_prefix, "")
            .replace(&hl.highlight_postfix, ""),
    )
    .unwrap();
    assert!(plain.len() <= hl.max_chars as usize);

    #[cfg(feature = "semantic-integration")]
    {
        let painter = SemanticSnippetPainter {
            searcher,
            generator,
            phrase: Some(phrase),
            hl,
        };
        for confirmed in [false, true] {
            let (semantic_html, painted) = painter.paint(&text, confirmed);
            assert!(painted);
            assert_eq!(highlight_count(&semantic_html), 2);
            assert!(
                semantic_html.contains("<font color=red>משה</font> <font color=red>ואהרן</font>")
            );
            let plain = htmlescape::decode_html(
                &semantic_html
                    .replace(&painter.hl.highlight_prefix, "")
                    .replace(&painter.hl.highlight_postfix, ""),
            )
            .unwrap();
            assert!(plain.len() <= painter.hl.max_chars as usize);
        }
        let (semantic_html, painted) = painter.paint("משה לבדו ואהרן", false);
        assert!(!painted);
        assert_eq!(semantic_html, "משה לבדו ואהרן");

        // A shown window the lexical match is not in gives way to the whole line; a
        // window it is in, or a hit no lexical search confirmed, keeps the window.
        let window = "…שלום שלום שלום…";
        let (shown, painted) = painter.paint_shown(&text, Some(window), true);
        assert!(painted);
        assert!(shown.contains("ואהרן"), "{shown}");
        assert_eq!(
            painter.paint_shown(&text, Some(window), false),
            painter.paint(window, false)
        );
        let marked = "…שלום משה ואהרן שלום…";
        assert_eq!(
            painter.paint_shown(&text, Some(marked), true),
            painter.paint(marked, true)
        );
        assert!(painter.paint(marked, true).1);
        assert_eq!(
            painter.paint_shown(&text, None, true),
            painter.paint(&text, true)
        );
    }
}

#[test]
fn oversized_phrase_window_keeps_existing_bounded_fallback() {
    let (mut engine, _dir) = make_engine();
    let text = format!("משה {} ואהרן", "שלום ".repeat(100));
    add(&mut engine, 1, &text, "/books/a.txt");
    engine.commit().unwrap();
    let searcher = engine.index_reader.searcher();
    let phrase = PhraseHighlight {
        per_word_terms: vec![
            HashSet::from(["משה".into()]),
            HashSet::from(["ואהרן".into()]),
        ],
        proximity: Proximity::Ordered(vec![1000]),
        analyzer: DISPLAY_ANALYZER,
        cross_line: false,
        alternatives: Vec::new(),
        allow_term_fallback: true,
    };
    let field = engine.schema.get_field("text").unwrap();
    let query = engine
        .terms_query_from_word_sets(&phrase.per_word_terms, field)
        .unwrap()
        .unwrap();
    let hl = HighlightConfig {
        max_chars: 80,
        ..HighlightConfig::default()
    };
    let generator =
        SearchEngine::make_snippet_generator(&searcher, query.as_ref(), field, &hl).unwrap();
    let mut original = generator.snippet(&text);
    original.set_snippet_prefix_postfix(&hl.highlight_prefix, &hl.highlight_postfix);
    let addresses = searcher
        .search(&AllQuery, &TopDocs::with_limit(1).order_by_score())
        .unwrap()
        .into_iter()
        .map(|(_, address)| address)
        .collect();
    let results = SearchEngine::build_results_with_generator(
        &engine.schema,
        &searcher,
        &generator,
        field,
        addresses,
        &hl,
        Some(&phrase),
    )
    .unwrap();
    assert_eq!(results[0].text, original.to_html());
    assert!(original.fragment().len() <= hl.max_chars as usize);
    #[cfg(feature = "semantic-integration")]
    {
        let painter = SemanticSnippetPainter {
            searcher,
            generator,
            phrase: Some(phrase),
            hl,
        };
        assert_eq!(painter.paint(&text, true), (original.to_html(), true));
        assert_eq!(
            painter.paint(&text, false),
            (bounded_plain_snippet(&text, 80), false)
        );
    }
}

use serde_json::json;
use tempfile::TempDir;

fn make_engine() -> (SearchEngine, TempDir) {
    let dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(dir.path().to_str().unwrap());
    (engine, dir)
}

fn dir_path_string(dir: &TempDir) -> String {
    dir.path().to_str().unwrap().to_string()
}

#[allow(clippy::too_many_arguments)]
fn search_advanced_default(
    engine: &SearchEngine,
    query: String,
    facets: Vec<String>,
    limit: u32,
    offset: u32,
    distance: u32,
    custom_spacing: HashMap<String, String>,
    alternative_words: HashMap<u32, Vec<String>>,
    search_options: HashMap<String, HashMap<String, bool>>,
    order: ResultsOrder,
    match_nikud: bool,
    match_taamim: bool,
    scope: SearchScope,
) -> Result<Vec<SearchResult>> {
    let negative_scope = same_search_scope(&scope);
    engine.search_advanced(
        query,
        String::new(),
        facets,
        limit,
        offset,
        distance,
        distance,
        custom_spacing,
        HashMap::new(),
        alternative_words,
        HashMap::new(),
        search_options,
        HashMap::new(),
        order,
        match_nikud,
        match_taamim,
        scope,
        negative_scope,
        None,
        None,
        None,
        None,
        None,
    )
}

fn same_search_scope(scope: &SearchScope) -> SearchScope {
    match scope {
        SearchScope::WordDistance => SearchScope::WordDistance,
        SearchScope::SameParagraph => SearchScope::SameParagraph,
        SearchScope::SameSection => SearchScope::SameSection,
    }
}

fn count_advanced_default(
    engine: &SearchEngine,
    query: String,
    facets: Vec<String>,
    distance: u32,
    custom_spacing: HashMap<String, String>,
    alternative_words: HashMap<u32, Vec<String>>,
    search_options: HashMap<String, HashMap<String, bool>>,
    match_nikud: bool,
    match_taamim: bool,
    scope: SearchScope,
) -> Result<u32> {
    let negative_scope = same_search_scope(&scope);
    engine.count_advanced(
        query,
        String::new(),
        facets,
        distance,
        distance,
        custom_spacing,
        HashMap::new(),
        alternative_words,
        HashMap::new(),
        search_options,
        HashMap::new(),
        match_nikud,
        match_taamim,
        scope,
        negative_scope,
        None,
        None,
        None,
        None,
    )
}

fn count_by_book_advanced_default(
    engine: &SearchEngine,
    query: String,
    facets: Vec<String>,
    distance: u32,
    custom_spacing: HashMap<String, String>,
    alternative_words: HashMap<u32, Vec<String>>,
    search_options: HashMap<String, HashMap<String, bool>>,
    match_nikud: bool,
    match_taamim: bool,
    scope: SearchScope,
) -> Result<HashMap<String, u32>> {
    let negative_scope = same_search_scope(&scope);
    engine.count_by_book_advanced(
        query,
        String::new(),
        facets,
        distance,
        distance,
        custom_spacing,
        HashMap::new(),
        alternative_words,
        HashMap::new(),
        search_options,
        HashMap::new(),
        match_nikud,
        match_taamim,
        scope,
        negative_scope,
        None,
        None,
        None,
        None,
    )
}

/// Writes a tiny `lexical.db` into `dir`: lemma "הלכ" with surfaces
/// "הלכתי"/"הולכ" (folded, as the real DB stores them) and returns its path.
fn make_lexical_db(dir: &TempDir) -> String {
    let path = dir.path().join("lexical.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
            r#"
            CREATE TABLE base (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
            CREATE TABLE surface (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE, base_id INTEGER NOT NULL REFERENCES base(id), notes TEXT);
            CREATE TABLE variant (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
            CREATE TABLE surface_variant (surface_id INTEGER NOT NULL REFERENCES surface(id), variant_id INTEGER NOT NULL REFERENCES variant(id), PRIMARY KEY (surface_id, variant_id));
            INSERT INTO base (id, value) VALUES (1, 'הלכ'), (2, 'ישנ'), (3, 'אדמור');
            INSERT INTO surface (id, value, base_id) VALUES
                (1, 'הלכתי', 1),
                (2, 'הולכ', 1),
                (3, 'לכו', 1),
                (4, 'הולכימ', 1),
                (5, 'לישונ', 2),
                (6, 'ישנ', 2),
                (7, 'בלשונ', 2),
                -- מסמן את הפער השיורי של אופציה A (ראו §5.2 בתכנון): ה-DB
                -- מחזיק צורות נקיות בלבד, בעוד שהאינדקס עשוי לשאת אדמו"ר.
                (8, 'אדמור', 3),
                (9, 'אדמורימ', 3);
            "#,
        )
        .unwrap();
    path.to_str().unwrap().to_string()
}

#[test]
fn new_writes_index_metadata_sidecar() {
    let (_engine, dir) = make_engine();
    let metadata_path = index_metadata_path(dir.path());
    assert!(metadata_path.exists());

    let compatibility = check_index_compatibility(dir_path_string(&dir));
    assert!(compatibility.compatible);
    assert_eq!(compatibility.status, "compatible");
    assert_eq!(
        compatibility.found_schema_version,
        Some(INDEX_SCHEMA_VERSION)
    );
}

#[test]
fn sidecar_version_match_but_schema_drift_requires_rebuild() {
    // שחזור התקלה מהשטח: אינדקס שנבנה בגרסת-ביניים של אותה
    // schema_version (למשל `text` עם fast=true לפני ההסרה) — הקובץ
    // הצדדי מצהיר על הגרסה הנכונה, אבל open_or_create היה נופל על
    // SchemaError. הבדיקה חייבת לדרוש בנייה מחדש, לא "תואם".
    let dir = TempDir::new().unwrap();
    let mut schema_builder = Schema::builder();
    schema_builder.add_text_field(
        "text",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("hebrew")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored()
            .set_fast(None),
    );
    let drifted = schema_builder.build();
    Index::create_in_dir(dir.path(), drifted).unwrap();
    // הקובץ הצדדי נכתב במפורש עם הגרסה הנוכחית — כמו אינדקס שנבנה
    // ע"י גרסת-הביניים עצמה, שחתמה "3" עם הסכימה הישנה.
    write_current_index_metadata(dir.path()).unwrap();

    let compatibility = check_index_compatibility(dir_path_string(&dir));
    assert!(!compatibility.compatible);
    assert_eq!(compatibility.status, "rebuild_required");
    assert_eq!(
        compatibility.found_schema_version,
        Some(INDEX_SCHEMA_VERSION)
    );
    assert!(compatibility
        .reason
        .unwrap()
        .contains("differs from the engine schema"));
}

#[test]
fn valid_sidecar_without_tantivy_meta_requires_rebuild() {
    // sidecar תקין לבדו לא מספיק: בלי meta.json של tantivy (חסר או
    // פגום) פתיחת האינדקס תיכשל, ולכן הבדיקה חייבת לדרוש בנייה מחדש
    // ולא להחזיר "תואם" רק על סמך ההצהרה בקובץ הצדדי.
    let dir = TempDir::new().unwrap();
    write_current_index_metadata(dir.path()).unwrap();

    let compatibility = check_index_compatibility(dir_path_string(&dir));
    assert!(!compatibility.compatible);
    assert_eq!(compatibility.status, "rebuild_required");
    assert_eq!(
        compatibility.found_schema_version,
        Some(INDEX_SCHEMA_VERSION)
    );
    assert!(compatibility
        .reason
        .unwrap()
        .contains("missing or unreadable"));

    // אותו דין ל-meta.json קיים אך פגום.
    fs::write(dir.path().join("meta.json"), "not json").unwrap();
    let compatibility = check_index_compatibility(dir_path_string(&dir));
    assert!(!compatibility.compatible);
    assert_eq!(compatibility.status, "rebuild_required");
    assert!(compatibility.reason.unwrap().contains("not valid JSON"));
}

#[test]
fn missing_sidecar_uses_tantivy_schema_fallback() {
    let (_engine, dir) = make_engine();
    fs::remove_file(index_metadata_path(dir.path())).unwrap();

    let compatibility = check_index_compatibility(dir_path_string(&dir));
    assert!(compatibility.compatible);
    assert_eq!(compatibility.status, "legacy_compatible");
    assert_eq!(
        compatibility.found_schema_version,
        Some(INDEX_SCHEMA_VERSION)
    );
}

#[test]
fn committed_terms_without_a_sidecar_require_rebuild_and_are_not_stamped() {
    for version in [7, 8, 9, INDEX_SCHEMA_VERSION] {
        let (mut engine, dir) = make_engine();
        add(&mut engine, 1, "מ<sup>נ</sup>שה", "/books/a.txt");
        engine.commit().unwrap();
        drop(engine);
        write_metadata(&dir, &index_metadata(version, None));
        fs::remove_file(index_metadata_path(dir.path())).unwrap();
        let meta_path = dir.path().join("meta.json");
        let before = fs::read(&meta_path).unwrap();

        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible, "{version}");
        assert_eq!(compatibility.status, "rebuild_required", "{version}");
        assert_eq!(compatibility.found_schema_version, None, "{version}");
        assert!(compatibility.reason.unwrap().contains("recipe"));
        ensure_current_index_metadata(dir.path(), false).unwrap();
        let reopened = SearchEngine::new(dir.path().to_str().unwrap());
        assert!(!index_metadata_path(dir.path()).exists(), "{version}");
        assert_eq!(fs::read(&meta_path).unwrap(), before, "{version}");
        assert_eq!(reopened.index_reader.searcher().num_docs(), 1);
    }
}

#[test]
fn missing_sidecar_with_an_unknown_segment_inventory_is_not_empty() {
    let (engine, dir) = make_engine();
    drop(engine);
    fs::remove_file(index_metadata_path(dir.path())).unwrap();
    let meta_path = dir.path().join("meta.json");
    let original: JsonValue = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    for segments in [None, Some(JsonValue::Null), Some(json!([{}]))] {
        let mut metadata = original.clone();
        match segments {
            Some(segments) => metadata["segments"] = segments,
            None => {
                metadata.as_object_mut().unwrap().remove("segments");
            }
        }
        fs::write(&meta_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible, "{metadata}");
        assert_eq!(compatibility.status, "rebuild_required");
        ensure_current_index_metadata(dir.path(), true).unwrap();
        assert!(!index_metadata_path(dir.path()).exists());
    }
}

#[test]
fn old_sidecar_schema_requires_rebuild() {
    let dir = TempDir::new().unwrap();
    let mut metadata = current_index_metadata();
    metadata.schema_version = MIN_READABLE_SCHEMA_VERSION - 1;
    fs::write(
        index_metadata_path(dir.path()),
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .unwrap();

    let compatibility = check_index_compatibility(dir_path_string(&dir));
    assert!(!compatibility.compatible);
    assert_eq!(compatibility.status, "rebuild_required");
    assert_eq!(
        compatibility.found_schema_version,
        Some(MIN_READABLE_SCHEMA_VERSION - 1)
    );
    assert!(compatibility.reason.unwrap().contains("older than"));
}

#[test]
fn future_sidecar_schema_marks_engine_too_old() {
    let dir = TempDir::new().unwrap();
    let mut metadata = current_index_metadata();
    metadata.schema_version = INDEX_SCHEMA_VERSION + 1;
    fs::write(
        index_metadata_path(dir.path()),
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .unwrap();

    let compatibility = check_index_compatibility(dir_path_string(&dir));
    assert!(!compatibility.compatible);
    assert_eq!(compatibility.status, "engine_too_old");
    assert_eq!(
        compatibility.found_schema_version,
        Some(INDEX_SCHEMA_VERSION + 1)
    );
}

#[test]
fn legacy_tantivy_schema_without_indexed_id_requires_rebuild() {
    let dir = TempDir::new().unwrap();
    let tantivy_metadata = json!({
        "schema": [
            {
                "name": "id",
                "type": "u64",
                "options": {
                    "indexed": false,
                    "fast": true,
                    "stored": true
                }
            }
        ]
    });
    fs::write(dir.path().join("meta.json"), tantivy_metadata.to_string()).unwrap();

    let compatibility = check_index_compatibility(dir_path_string(&dir));
    assert!(!compatibility.compatible);
    assert_eq!(compatibility.status, "rebuild_required");
    assert_eq!(compatibility.found_schema_version, Some(1));
}

#[test]
fn legacy_schema_with_current_id_but_old_file_path_requires_rebuild() {
    let dir = TempDir::new().unwrap();
    {
        // `id` matches the current shape, but `filePath` lacks FAST (and
        // is tokenized) — the engine could not open this index, so the
        // full-schema check must fail it instead of passing on `id` alone.
        let mut b = Schema::builder();
        b.add_text_field("text", TEXT | STORED | FAST);
        b.add_text_field("reference", STORED);
        b.add_text_field(
            "title",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("raw")
                        .set_fieldnorms(false),
                )
                .set_stored(),
        );
        b.add_u64_field("id", STORED | FAST | INDEXED);
        b.add_u64_field("segment", STORED);
        b.add_bool_field("isPdf", STORED);
        b.add_text_field("filePath", TEXT | STORED);
        b.add_facet_field("topics", FacetOptions::default());
        let old_schema = b.build();
        let mmap = tantivy::directory::MmapDirectory::open(dir.path()).unwrap();
        Index::open_or_create(mmap, old_schema).unwrap();
    }

    let compatibility = check_index_compatibility(dir_path_string(&dir));
    assert!(!compatibility.compatible);
    assert_eq!(compatibility.status, "rebuild_required");
}

fn add(engine: &mut SearchEngine, id: u64, text: &str, file_path: &str) {
    engine
        .add_document_at(
            id, "title", "ref", "/root", text, 0, false, file_path, None, None, None,
        )
        .unwrap();
}

// ── The chunkKey column, and the indexes version 5 rebuilds ─────────────

/// The schema of a real version 4 index, as the `meta.json` of the library's release
/// index (6,042,284 lines, built by the engine before the `chunkKey` column) holds it.
const RELEASED_VERSION_4_SCHEMA: &str = r#"[
        {"name": "text", "type": "text", "options": {"indexing": {"record": "position", "fieldnorms": true, "tokenizer": "hebrew"}, "stored": true, "fast": false}},
        {"name": "textVocalized", "type": "text", "options": {"indexing": {"record": "position", "fieldnorms": true, "tokenizer": "hebrew_vocalized"}, "stored": true, "fast": false}},
        {"name": "reference", "type": "text", "options": {"stored": true, "fast": false}},
        {"name": "title", "type": "text", "options": {"indexing": {"record": "basic", "fieldnorms": false, "tokenizer": "raw"}, "stored": true, "fast": false}},
        {"name": "id", "type": "u64", "options": {"indexed": true, "fieldnorms": true, "fast": true, "stored": true}},
        {"name": "segment", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": false, "stored": true}},
        {"name": "isPdf", "type": "bool", "options": {"indexed": false, "fieldnorms": false, "fast": false, "stored": true}},
        {"name": "filePath", "type": "text", "options": {"indexing": {"record": "basic", "fieldnorms": true, "tokenizer": "raw"}, "stored": true, "fast": true}},
        {"name": "contentHash", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "textHash", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "sectionId", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "generationSort", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "lineHash", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "topics", "type": "facet", "options": {"stored": false}}
    ]"#;

/// The fields version 5 adds to the released version 4 schema, beside `chunkKey`: where
/// the text of a document is kept when it is not read from the library database, and
/// where a phrase may continue from one line onto the next.
const VERSION_5_FIELDS: [&str; 6] = [
    LINE_CHECK_FIELD,
    TEXT_STORED_FIELD,
    TEXT_VOCALIZED_STORED_FIELD,
    cross_line::LINE_FIRST_FIELD,
    cross_line::LINE_LAST_FIELD,
    cross_line::LINE_EDGE_FIELD,
];

/// The schema of a real version 5 index, as the `meta.json` of a full library index
/// (6,910,973 lines) built by the engine before version 6 holds it.
const RELEASED_VERSION_5_SCHEMA: &str = r#"[
        {"name": "text", "type": "text", "options": {"indexing": {"record": "position", "fieldnorms": true, "tokenizer": "hebrew"}, "stored": false, "fast": false}},
        {"name": "textVocalized", "type": "text", "options": {"indexing": {"record": "position", "fieldnorms": true, "tokenizer": "hebrew_vocalized"}, "stored": false, "fast": false}},
        {"name": "reference", "type": "text", "options": {"stored": true, "fast": false}},
        {"name": "title", "type": "text", "options": {"indexing": {"record": "basic", "fieldnorms": false, "tokenizer": "raw"}, "stored": true, "fast": false}},
        {"name": "id", "type": "u64", "options": {"indexed": true, "fieldnorms": true, "fast": true, "stored": true}},
        {"name": "segment", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": false, "stored": true}},
        {"name": "isPdf", "type": "bool", "options": {"indexed": false, "fieldnorms": false, "fast": false, "stored": true}},
        {"name": "filePath", "type": "text", "options": {"indexing": {"record": "basic", "fieldnorms": true, "tokenizer": "raw"}, "stored": true, "fast": true}},
        {"name": "contentHash", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "textHash", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "sectionId", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "generationSort", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "lineHash", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "lineCheck", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "topics", "type": "facet", "options": {"stored": false}},
        {"name": "chunkKey", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "textStored", "type": "text", "options": {"stored": true, "fast": false}},
        {"name": "textVocalizedStored", "type": "text", "options": {"stored": true, "fast": false}},
        {"name": "lineFirst", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "lineLast", "type": "u64", "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}},
        {"name": "lineEdge", "type": "text", "options": {"indexing": {"record": "basic", "fieldnorms": false, "tokenizer": "raw"}, "stored": false, "fast": false}}
    ]"#;

/// The version 5 fields versions 6 and 7 change, and how; every other version 5 field is
/// unchanged, and version 10 has the fields of 7. Version 11 adds a stored-only field.
#[test]
fn version_10_is_version_5_with_its_disk_savings_and_stamps() {
    let released: Schema = serde_json::from_str(RELEASED_VERSION_5_SCHEMA).unwrap();
    let current = schema_of_version(10).unwrap();
    let entry = |name: &str| current.get_field_entry(current.get_field(name).unwrap());
    let mut removed = Vec::new();
    for (_, old) in released.fields() {
        match old.name() {
            "id" => {
                let now = entry("id");
                assert!(now.is_fast() && now.is_stored() && !now.is_indexed());
            }
            // Out of the doc store, read from their columns.
            "title" | "isPdf" | "filePath" => {
                let now = entry(old.name());
                assert!(old.is_stored() && !now.is_stored(), "{}", old.name());
                assert!(now.is_fast(), "{}", old.name());
                assert_eq!(now.is_indexed(), old.is_indexed(), "{}", old.name());
                assert!(!now.has_fieldnorms(), "{}", old.name());
            }
            name if current.get_field(name).is_err() => removed.push(name),
            name => assert_eq!(entry(name), old, "{name}"),
        }
    }
    // `sectionId` is derived at query time from `id` and the small column that replaces
    // it; the generation is the order mapping's (`book_order`), not the index's.
    assert_eq!(removed, ["sectionId", "generationSort"]);
    let added: Vec<&str> = current
        .fields()
        .map(|(_, entry)| entry.name())
        .filter(|name| released.get_field(name).is_err())
        .collect();
    assert_eq!(
        added,
        [METADATA_HASH_FIELD, SOURCE_HASH_FIELD, SECTION_OFFSET_FIELD]
    );
    for name in added {
        let added = entry(name);
        assert!(added.is_fast() && !added.is_indexed() && !added.is_stored());
    }

    for version in [5, MIN_READABLE_SCHEMA_VERSION - 1, INDEX_SCHEMA_VERSION + 1] {
        assert_eq!(schema_of_version(version), None, "{version}");
    }
}

/// A new index compresses its doc store with zstd over 16K blocks; one created with
/// other settings is opened with its own.
#[test]
fn a_new_index_compresses_its_store_with_zstd() {
    let (mut engine, dir) = make_engine();
    let settings = engine.index.settings();
    assert_eq!(
        settings.docstore_compression,
        tantivy::store::Compressor::Zstd(tantivy::store::ZstdCompressor {
            compression_level: Some(DOCSTORE_ZSTD_LEVEL)
        })
    );
    assert_eq!(settings.docstore_blocksize, DOCSTORE_BLOCK_SIZE);
    add(&mut engine, 7, "שורה ראשונה", "/a.txt");
    engine.commit().unwrap();
    drop(engine);
    let engine = SearchEngine::new(dir.path().to_str().unwrap());
    let found = engine.get_document_by_id(7).unwrap().unwrap();
    assert_eq!(found.text, "שורה ראשונה");

    let lz4 = TempDir::new().unwrap();
    Index::builder()
        .schema(current_schema())
        .settings(tantivy::IndexSettings {
            docstore_compression: tantivy::store::Compressor::Lz4,
            ..tantivy::IndexSettings::default()
        })
        .create_in_dir(lz4.path())
        .unwrap();
    let mut engine = SearchEngine::new(lz4.path().to_str().unwrap());
    assert_eq!(
        engine.index.settings().docstore_compression,
        tantivy::store::Compressor::Lz4
    );
    add(&mut engine, 8, "שורה שנייה", "/b.txt");
    engine.commit().unwrap();
    engine.index_reader.reload().unwrap();
    assert_eq!(
        engine.get_document_by_id(8).unwrap().unwrap().text,
        "שורה שנייה"
    );
}

/// A version 5 index is rebuilt, with its metadata or without.
#[test]
fn a_released_version_5_index_is_rebuilt() {
    let fields: JsonValue = serde_json::from_str(RELEASED_VERSION_5_SCHEMA).unwrap();
    for version in [Some(5), None] {
        let dir = index_of_schema(fields.clone(), version);
        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible, "{version:?}");
        assert_eq!(compatibility.status, "rebuild_required", "{version:?}");
        assert_eq!(compatibility.found_schema_version, version);
        assert_eq!(compatibility.required_schema_version, INDEX_SCHEMA_VERSION);
    }
}

/// A version 6 index — ids composed from the catalogue position, a `generation` column,
/// no metadata or source stamps — is rebuilt, with its metadata or without, and so is
/// one whose metadata claims this version over that schema.
#[test]
fn a_version_6_index_is_rebuilt() {
    let mut fields = serde_json::to_value(current_schema()).unwrap();
    let list = fields.as_array_mut().unwrap();
    list.retain(|field| {
        ![METADATA_HASH_FIELD, SOURCE_HASH_FIELD].contains(&field["name"].as_str().unwrap())
    });
    let offset = list
        .iter()
        .position(|field| field["name"] == SECTION_OFFSET_FIELD)
        .unwrap();
    let mut generation = list[offset].clone();
    generation["name"] = JsonValue::from("generation");
    list.insert(offset + 1, generation);
    for version in [Some(6), None, Some(INDEX_SCHEMA_VERSION)] {
        let dir = index_of_schema(fields.clone(), version);
        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible, "{version:?}");
        assert_eq!(compatibility.status, "rebuild_required", "{version:?}");
        assert_eq!(compatibility.required_schema_version, INDEX_SCHEMA_VERSION);
    }
}

/// Old text recipes keep the same fields, so their declared version requires rebuilding.
#[test]
fn previous_text_recipe_versions_require_rebuilding_with_their_sidecars() {
    for version in [7, 8, 9] {
        let fields = serde_json::to_value(current_schema()).unwrap();
        let dir = index_of_schema(fields, Some(version));
        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible, "{version}");
        assert_eq!(compatibility.status, "rebuild_required");
        assert_eq!(compatibility.found_schema_version, Some(version));
        assert_eq!(compatibility.required_schema_version, INDEX_SCHEMA_VERSION);
    }
}

/// An empty index of the schema `fields` (a `meta.json` schema array), with the metadata
/// of `version` when one is given.
fn index_of_schema(fields: JsonValue, version: Option<u32>) -> TempDir {
    let dir = TempDir::new().unwrap();
    let schema: Schema = serde_json::from_value(fields).unwrap();
    Index::create_in_dir(dir.path(), schema).unwrap();
    if let Some(version) = version {
        let recipe = (version == INDEX_SCHEMA_VERSION).then(ChunkKeyRecipe::current);
        write_metadata(&dir, &index_metadata(version, recipe));
    }
    dir
}

/// The released version 4 index is rebuilt, with its metadata or without: its text is
/// stored where this build does not read it.
#[test]
fn a_released_version_4_index_is_rebuilt() {
    let fields: JsonValue = serde_json::from_str(RELEASED_VERSION_4_SCHEMA).unwrap();
    for version in [Some(4), None] {
        let dir = index_of_schema(fields.clone(), version);
        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible, "{version:?}");
        assert_eq!(compatibility.status, "rebuild_required", "{version:?}");
        assert_eq!(compatibility.found_schema_version, version);
    }
}

/// A version 5 index written without [`VERSION_5_FIELDS`] — its text stored the
/// version 4 way, beside a `chunkKey` column — is rebuilt: the check reads the fields
/// the index has, not the version its metadata declares.
#[test]
fn a_version_5_index_without_its_fields_is_rebuilt() {
    let mut fields: Vec<JsonValue> = serde_json::from_str(RELEASED_VERSION_4_SCHEMA).unwrap();
    fields.push(json!({
        "name": CHUNK_KEY_FIELD,
        "type": "u64",
        "options": {"indexed": false, "fieldnorms": false, "fast": true, "stored": false}
    }));
    let without: Schema = serde_json::from_value(JsonValue::Array(fields.clone())).unwrap();
    for name in VERSION_5_FIELDS {
        assert!(current_schema().get_field(name).is_ok(), "{name}");
        assert!(without.get_field(name).is_err(), "{name}");
    }
    for version in [Some(INDEX_SCHEMA_VERSION), None] {
        let dir = index_of_schema(JsonValue::Array(fields.clone()), version);
        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible, "{version:?}");
        assert_eq!(compatibility.status, "rebuild_required", "{version:?}");
        if version.is_some() {
            assert!(
                compatibility.reason.as_deref().unwrap().contains("differs"),
                "{:?}",
                compatibility.reason
            );
        }
    }

    // An index of this schema but one of those fields, as a build before it made one.
    let current = serde_json::to_value(current_schema()).unwrap();
    for name in VERSION_5_FIELDS {
        let mut fields = current.as_array().unwrap().clone();
        fields.retain(|field| field["name"] != name);
        let dir = index_of_schema(JsonValue::Array(fields), Some(INDEX_SCHEMA_VERSION));
        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert_eq!(compatibility.status, "rebuild_required", "without {name}");
    }
}

/// The metadata a test writes into an index directory it made by other means.
fn write_metadata(dir: &TempDir, metadata: &IndexMetadata) {
    write_index_metadata(dir.path(), metadata).unwrap();
}

fn read_metadata(dir: &TempDir) -> JsonValue {
    serde_json::from_str(&fs::read_to_string(index_metadata_path(dir.path())).unwrap()).unwrap()
}

/// What the `chunkKey` column holds for each live document, by id: `None` for a
/// document with no value, and for every document of an index without the column.
fn chunk_key_values(engine: &SearchEngine) -> Vec<Option<u64>> {
    let searcher = engine.index_reader.searcher();
    let mut values = Vec::new();
    for reader in searcher.segment_readers() {
        let ids = reader.fast_fields().u64("id").unwrap();
        let column = reader
            .fast_fields()
            .column_opt::<u64>(CHUNK_KEY_FIELD)
            .unwrap();
        for doc in reader.doc_ids_alive() {
            let value = column.as_ref().and_then(|column| column.first(doc));
            values.push((ids.first(doc).unwrap(), value));
        }
    }
    values.sort_unstable();
    values.into_iter().map(|(_, value)| value).collect()
}

/// An index the engine before long-line windows wrote, its column keyed under the
/// published chunking, is read by its column; one recording the windows chunking is not.
#[test]
fn an_index_written_before_the_windows_keeps_its_column() {
    for (identity, used) in [
        (2_685_558_872_390_372_738u64, true),
        (
            crate::semantic_keys::production_chunking().identity(),
            false,
        ),
    ] {
        let (engine, dir) = make_engine();
        drop(engine);
        let mut metadata = index_metadata(
            INDEX_SCHEMA_VERSION,
            Some(ChunkKeyRecipe {
                line_text_version: LINE_TEXT_VERSION,
                key_version: 1,
                chunking_identity: identity,
            }),
        );
        metadata.engine_version = "3.1.0".to_string();
        write_index_metadata(dir.path(), &metadata).unwrap();
        let engine = SearchEngine::new(dir.path().to_str().unwrap());
        assert_eq!(engine.chunk_key_field.is_some(), used, "{identity}");
    }
}

/// A new index is version 5: it has the column, its metadata records the recipe the
/// column is written under, and the engine writes to it.
#[test]
fn a_new_index_has_the_column_and_records_its_recipe() {
    let (mut engine, dir) = make_engine();
    let field = engine.schema.get_field(CHUNK_KEY_FIELD).unwrap();
    assert_eq!(engine.chunk_key_field, Some(field));

    let metadata = read_metadata(&dir);
    assert_eq!(metadata["schema_version"], INDEX_SCHEMA_VERSION);
    let recipe = ChunkKeyRecipe::current();
    assert_eq!(metadata["line_text_version"], recipe.line_text_version);
    assert_eq!(metadata["line_text_version"], LINE_TEXT_VERSION);
    assert_eq!(metadata["chunk_key_version"], recipe.key_version);
    assert_eq!(
        metadata["chunk_key_chunking_identity"].as_u64(),
        Some(recipe.chunking_identity)
    );

    engine
        .add_text_book_placed(
            "בראשית".to_string(),
            "/root".to_string(),
            "/books/genesis.txt".to_string(),
            0,
            0,
            "בראשית ברא אלהים את השמים ואת הארץ\nאו".to_string(),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    engine.index_reader.reload().unwrap();
    let values = chunk_key_values(&engine);
    assert_eq!(values.len(), 2);
    assert!(values[0].is_some_and(|key| key != 0), "{values:?}");
    assert_eq!(values[1], Some(0), "two characters are not embedded");
}

/// Every path but `add_text_book` adds lines that are not a book's, so nothing knows
/// their neighbours: they are written 0, "not embedded".
#[test]
fn every_other_add_path_writes_zero() {
    let (mut engine, _dir) = make_engine();
    let line = "שורה ארוכה דיה כדי לעמוד בפני עצמה";
    let input = |ordinal: u32| DocumentInput {
        ordinal,
        title: "t".to_string(),
        reference: "r".to_string(),
        topics: "/root".to_string(),
        text: line.to_string(),
        segment: 0,
        is_pdf: false,
        file_path: "/books/loose.txt".to_string(),
        content_hash: None,
        text_hash: None,
        source_hash: None,
        section_ordinal: None,
        text_vocalized: None,
        extra_facets: None,
        text_storage: None,
    };
    add(&mut engine, 1, line, "/books/loose.txt");
    engine.add_documents_batch(vec![input(1)]).unwrap();
    engine.upsert_documents_batch(vec![input(2)]).unwrap();
    engine
        .upsert_document(
            3,
            "t",
            "r",
            "/root",
            line,
            0,
            false,
            "/books/loose.txt",
            None,
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    engine.index_reader.reload().unwrap();
    assert_eq!(chunk_key_values(&engine), vec![Some(0); 4]);
}

/// **A column written under another recipe is not a rebuild.** The index stays
/// compatible, its column counts as absent — nothing is written to it — and a metadata
/// value that does not parse is a recipe not known rather than unreadable metadata.
#[test]
fn a_column_written_under_another_recipe_counts_as_absent() {
    let other_recipes: [(&str, JsonValue); 4] = [
        ("line_text_version", json!(LINE_TEXT_VERSION + 1)),
        ("chunk_key_version", json!(99)),
        ("chunk_key_chunking_identity", json!(1)),
        ("line_text_version", json!("one")),
    ];
    for (field, value) in other_recipes {
        let (mut engine, dir) = make_engine();
        engine
            .add_text_book_placed(
                "בראשית".to_string(),
                "/root".to_string(),
                "/books/genesis.txt".to_string(),
                0,
                0,
                "בראשית ברא אלהים את השמים ואת הארץ".to_string(),
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        drop(engine);

        let mut metadata = read_metadata(&dir);
        metadata[field] = value.clone();
        fs::write(index_metadata_path(dir.path()), metadata.to_string()).unwrap();

        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(compatibility.compatible, "{field}={value}");
        assert_eq!(compatibility.status, "compatible", "{field}={value}");
        assert_eq!(
            compatibility.found_schema_version,
            Some(INDEX_SCHEMA_VERSION)
        );

        let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
        assert_eq!(engine.chunk_key_field, None, "{field}={value}");
        engine
            .add_text_book_placed(
                "שמות".to_string(),
                "/root".to_string(),
                "/books/exodus.txt".to_string(),
                1,
                0,
                "ואלה שמות בני ישראל הבאים מצרימה".to_string(),
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        engine.index_reader.reload().unwrap();
        assert_eq!(
            engine.count(vec!["שמות".to_string()], &[], 0, 100).unwrap(),
            1
        );
        let values = chunk_key_values(&engine);
        assert_eq!(values.len(), 2);
        assert!(values[0].is_some(), "the first book's keys stay");
        assert_eq!(values[1], None, "{field}={value}: nothing writes to it");
        // The metadata is left as it was found: only a new index records a recipe.
        assert_eq!(read_metadata(&dir)[field], value);
    }
}

/// What a build reports when no session is open: no session, or no semantic support at
/// all, each as a state and as the kind of `last_error`.
#[cfg(feature = "semantic-integration")]
const CLOSED: (SemanticState, SemanticErrorKind) = (
    SemanticState::NotConfigured,
    SemanticErrorKind::NotConfigured,
);
#[cfg(not(feature = "semantic-integration"))]
const CLOSED: (SemanticState, SemanticErrorKind) = (
    SemanticState::NotInBuild,
    SemanticErrorKind::FeatureNotInBuild,
);

#[test]
fn semantic_status_is_explicit_before_configuration() {
    let (engine, _dir) = make_engine();
    let status = engine.semantic_status();
    assert!(!status.enabled);
    assert!(!status.available);
    assert!(status.last_error.is_some());
    assert_eq!(
        (status.state, status.error_kind),
        (CLOSED.0, Some(CLOSED.1))
    );
}

/// Without the integration every semantic call is answered, and none fails: the opening
/// calls return the `NotInBuild` status, the build-side calls report themselves
/// disabled, and a search serves lexical results whose fallback kind says why.
#[cfg(not(feature = "semantic-integration"))]
#[test]
fn a_build_without_semantic_support_says_so_in_every_semantic_call() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 41, "שלום עולם", "/books/a.txt");
    engine.commit().unwrap();

    let configured = engine
        .configure_semantic(SemanticConfigInput {
            root_dir: "/nowhere".to_string(),
            model_path: "/nowhere/model.onnx".to_string(),
            model_id: "m".to_string(),
            embedding_dim: 256,
            pooling: "in-graph".to_string(),
            max_tokens: 256,
            model_quantization: "int8".to_string(),
            embedding_text_version: 2,
            onnx_runtime_path: None,
        })
        .unwrap();
    let opened = engine
        .open_semantic_artifact(SemanticArtifactInput {
            vectors_dir: "/nowhere/vectors".to_string(),
            model_path: "/nowhere/model.onnx".to_string(),
            model_identity_json: "not even JSON".to_string(),
            onnx_runtime_path: None,
            scan_threads: None,
        })
        .unwrap();
    for status in [configured, opened, engine.semantic_status()] {
        assert_eq!(status.state, SemanticState::NotInBuild);
        assert_eq!(
            status.error_kind,
            Some(SemanticErrorKind::FeatureNotInBuild)
        );
        assert!(!status.enabled && !status.available);
    }

    assert!(!engine.semantic_index_books(Vec::new()).unwrap().enabled);
    assert!(!engine.semantic_index_diff().unwrap().enabled);
    assert!(!engine.remove_semantic_books(Vec::new()).unwrap().enabled);
    assert!(!engine.reset_semantic_index().unwrap().enabled);

    for mode in [
        SemanticRetrievalMode::Hybrid,
        SemanticRetrievalMode::SemanticOnly,
        SemanticRetrievalMode::LexicalOnly,
    ] {
        let response = engine
            .search_semantic(
                "שלום".to_string(),
                Vec::new(),
                10,
                0,
                SemanticLexicalMode::Exact,
                1,
                mode,
                None,
                false,
                false,
                None,
                &SemanticCancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            response.fallback_kind,
            Some(SemanticErrorKind::FeatureNotInBuild),
            "{mode:?}"
        );
        assert!(!response.semantic_available, "{mode:?}");
    }
}

/// A lexical failure under a semantic call is what `?` turns into a `SemanticError`, and
/// it is `Internal`, with the whole context chain in the message.
#[test]
fn an_anyhow_error_under_a_semantic_call_is_internal_and_keeps_its_chain() {
    let error = SemanticError::from(
        anyhow::anyhow!("the segment file is gone").context("reading the index"),
    );
    assert_eq!(error.kind, SemanticErrorKind::Internal);
    assert_eq!(error.message, "reading the index: the segment file is gone");
    assert_eq!(error.field, None);
    assert_eq!(error.to_string(), error.message);
}

/// A search whose token was cancelled before it started does nothing at all, in every
/// build and every mode, and says so by kind rather than with lexical results. One
/// cancelled while its lexical fallback ran is cancelled too, and an uncancelled token
/// changes nothing.
#[test]
fn a_cancelled_search_throws_cancelled_and_an_uncancelled_one_is_served() {
    use search_cancellation::{cancelling_at, SearchCheckpoint as At};
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 41, "שלום עולם", "/books/a.txt");
    engine.commit().unwrap();
    let search = |mode, token: &SemanticCancellationToken| {
        engine.search_semantic(
            "שלום".to_string(),
            Vec::new(),
            10,
            0,
            SemanticLexicalMode::Exact,
            1,
            mode,
            None,
            false,
            false,
            None,
            token,
        )
    };
    let cancelled = |result: Result<SemanticSearchResponse, SemanticError>| match result {
        Ok(response) => panic!(
            "a cancelled search was served, with {:?}",
            response.fallback_kind
        ),
        Err(error) => error,
    };

    let before = SemanticCancellationToken::new();
    before.cancel();
    assert!(before.is_cancelled());
    for mode in [
        SemanticRetrievalMode::Hybrid,
        SemanticRetrievalMode::SemanticOnly,
        SemanticRetrievalMode::LexicalOnly,
    ] {
        let (result, reached) = cancelling_at(None, || search(mode, &before));
        assert_eq!(cancelled(result), SemanticError::cancelled(), "{mode:?}");
        assert_eq!(
            reached,
            [At::Start],
            "{mode:?}: nothing ran past the first look"
        );

        let during = SemanticCancellationToken::new();
        let (result, reached) = cancelling_at(Some(At::Fallback), || search(mode, &during));
        assert_eq!(
            cancelled(result).kind,
            SemanticErrorKind::Cancelled,
            "{mode:?}"
        );
        assert_eq!(reached, [At::Start, At::Fallback], "{mode:?}");

        let fresh = SemanticCancellationToken::new();
        let (result, reached) = cancelling_at(None, || search(mode, &fresh));
        let response = result.unwrap();
        assert_eq!(response.fallback_kind, Some(CLOSED.1), "{mode:?}");
        assert_eq!(reached, [At::Start, At::Fallback], "{mode:?}");
        assert!(!fresh.is_cancelled());
    }
}

#[test]
fn hybrid_request_falls_back_to_ranked_lexical_results_with_reason() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 41, "שלום עולם", "/books/a.txt");
    engine.commit().unwrap();

    let response = engine
        .search_semantic(
            "שלום".to_string(),
            Vec::new(),
            10,
            0,
            SemanticLexicalMode::Exact,
            1,
            SemanticRetrievalMode::Hybrid,
            None,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap();

    assert_eq!(response.executed_mode, SemanticExecutedMode::LexicalOnly);
    assert_eq!(response.lexical_total_count, 1);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].id, 41);
    assert_eq!(response.results[0].source, SemanticResultSource::Lexical);
    assert!(response.fallback_reason.is_some());
    assert_eq!(response.fallback_kind, Some(CLOSED.1));
}

fn disable_auto_merge(engine: &mut SearchEngine) {
    engine
        .writer_mut()
        .unwrap()
        .set_merge_policy(Box::new(NoMergePolicy));
}

fn search_ids(engine: &mut SearchEngine, term: &str) -> Vec<u64> {
    engine
        .search(
            vec![term.to_string()],
            vec!["/root".to_string()],
            100,
            0,
            0,
            100,
            ResultsOrder::Catalogue,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|result| result.id)
        .collect()
}

#[test]
fn generation_order_can_prioritize_sources_before_commentaries() {
    let (mut engine, _dir) = make_engine();
    for (title, path) in [
        ("פירוש מוקדם בקטלוג", "/books/commentary.txt"),
        ("מקור שני", "/books/source-b.txt"),
        ("מקור ראשון", "/books/source-a.txt"),
    ] {
        engine
            .add_document(0, title, "ref", "/root", "שלום", 0, false, path, None, None)
            .unwrap();
    }
    engine.commit().unwrap();
    let place = |path: &str, catalogue_order, generation_order| BookOrderEntry {
        file_path: path.to_string(),
        catalogue_order,
        generation_order,
    };
    engine
        .set_book_order(vec![
            place("/books/commentary.txt", 10, 65),
            place("/books/source-b.txt", 30, 2),
            place("/books/source-a.txt", 20, 2),
        ])
        .unwrap();

    let by_generation: Vec<String> = engine
        .search_exact(
            "שלום".to_string(),
            vec!["/root".to_string()],
            10,
            0,
            ResultsOrder::Generation,
            false,
            false,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|result| result.file_path)
        .collect();

    // Within a generation, the catalogue decides.
    assert_eq!(
        by_generation,
        [
            "/books/source-a.txt",
            "/books/source-b.txt",
            "/books/commentary.txt"
        ]
    );
}

#[test]
fn add_text_book_builds_reference_trail_ids_and_fingerprint() {
    let (mut engine, _dir) = make_engine();
    let text = "<h1>ספר בראשית</h1>\n<h2>פרק א</h2>\nבְּרֵאשִׁית ברא אלהים\n<h2>פרק ב</h2>\nויכלו השמים";
    let added = engine
        .add_text_book(
            "בראשית".to_string(),
            "/root".to_string(),
            "/books/bereshit.txt".to_string(),
            text.to_string(),
            None,
            TextStorage::InIndex,
            None,
            Some(77),
        )
        .unwrap();
    assert_eq!(added, 5);
    engine.commit().unwrap();

    let results = engine
        .search_exact(
            "ויכלו".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    let hit = &results[0];
    // הכותרת החדשה של פרק ב החליפה את פרק א ב-trail (אותו prefix "<h2>").
    assert_eq!(hit.reference, "ספר בראשית, פרק ב");
    assert_eq!(hit.segment, 4);
    // id = (bookNumber << 32) + ordinal+1; the first book of an index is number 1.
    assert_eq!(hit.id, (1u64 << 32) + 5);
    assert_eq!(hit.title, "בראשית");
    assert!(!hit.is_pdf);

    // הטקסט המאונדקס מנורמל (הניקוד הוסר) והחיפוש מוצא אותו.
    let nikud_hit = engine
        .search_exact(
            "בראשית ברא".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(nikud_hit.len(), 1);

    // טביעת האצבע נחתמה על הספר, זהה לחישוב הציבורי הקנוני
    // (טקסט + metadata).
    let fingerprints = engine.get_book_fingerprints().unwrap();
    assert_eq!(
        fingerprints.get("/books/bereshit.txt"),
        Some(&compute_book_fingerprint(
            text.to_string(),
            "בראשית".to_string(),
            "/root".to_string(),
            None,
        ))
    );
    // שינוי metadata בלבד (כותרת) משנה את החתימה — הספר יזוהה
    // כדורש אינדוקס-מחדש.
    assert_ne!(
        compute_book_fingerprint(
            text.to_string(),
            "בראשית".to_string(),
            "/root".to_string(),
            None,
        ),
        compute_book_fingerprint(
            text.to_string(),
            "ספר בראשית".to_string(),
            "/root".to_string(),
            None,
        ),
    );
    // סדר ה-extra_facets אינו משנה את החתימה.
    assert_eq!(
        compute_book_fingerprint(
            text.to_string(),
            "בראשית".to_string(),
            "/root".to_string(),
            Some(vec![
                "/author/רש\"י".to_string(),
                "/era/ראשונים".to_string()
            ]),
        ),
        compute_book_fingerprint(
            text.to_string(),
            "בראשית".to_string(),
            "/root".to_string(),
            Some(vec![
                "/era/ראשונים".to_string(),
                "/author/רש\"י".to_string()
            ]),
        ),
    );

    // חתימת הטקסט-בלבד נחתמה גם היא, זהה לחישוב הציבורי — ובניגוד
    // לקנונית, אינה תלויה ב-metadata.
    let text_fingerprints = engine.get_book_text_fingerprints().unwrap();
    assert_eq!(
        text_fingerprints.get("/books/bereshit.txt"),
        Some(&compute_content_fingerprint(text.to_string()))
    );

    // All the book's stamps in one call: the metadata alone, and the source as given.
    assert_eq!(
        engine.get_book_stamps().unwrap(),
        vec![BookStamps {
            file_path: "/books/bereshit.txt".to_string(),
            book_number: 1,
            documents: 5,
            content_hash: fingerprints["/books/bereshit.txt"],
            text_hash: compute_content_fingerprint(text.to_string()),
            metadata_hash: compute_book_metadata_fingerprint(
                "בראשית".to_string(),
                "/root".to_string(),
                None,
            ),
            source_hash: 77,
        }]
    );
}

#[test]
fn breaking_tag_in_a_line_keeps_the_words_separate() {
    // Otzaria/otzaria#949: `<br>` בתוך שורה (ספרים אישיים מומרים) הדביק
    // את שתי המילים לטוקן אחד — הן הוצגו דבוקות בתוצאות, וחיפוש של כל
    // אחת מהן החטיא.
    let (mut engine, _dir) = make_engine();
    engine
        .add_text_book_placed(
            "חוזק יד".to_string(),
            "/root".to_string(),
            "/books/chozek.txt".to_string(),
            1,
            DEFAULT_GENERATION_ORDER,
            "יותר בכבוד המורים<br>כי אב הביאו לעולם".to_string(),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();

    for query in ["המורים", "כי", "המורים כי"] {
        let results = engine
            .search_exact(
                query.to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(results.len(), 1, "query {query:?}");
        // הטקסט השמור — שממנו נבנה קטע התוצאה — מופרד ברווח (תגי
        // ההדגשה של המנוע עשויים לעטוף את ההתאמות).
        let plain = results[0]
            .text
            .replace("<font color=red>", "")
            .replace("</font>", "");
        assert!(plain.contains("המורים כי"), "stored text: {plain}");
    }
}

#[test]
fn sup_marker_keeps_neighbouring_words_searchable() {
    let lines = [
        "תורה<sup>30</sup> המשך",
        "תורה.<sup>30</sup> המשך",
        "תורה<sup>א</sup> מסוף",
        "תורה<sub>30</sub> המשך",
        "גוף <sup class=\"footnote-marker\">א</sup><i class=\"footnote\">הרי זה</i> סוף",
        "יְהוֹנָתָן בֶּן־גֵּרְשֹׁם בֶּן־מְ<sup>נַ</sup>שֶּׁה",
    ];
    let (mut engine, _dir) = make_engine();
    engine
        .add_text_book_placed(
            "הערות".to_string(),
            "/root".to_string(),
            "/books/notes.txt".to_string(),
            1,
            DEFAULT_GENERATION_ORDER,
            lines.join("\n"),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();

    let segments = |results: Vec<SearchResult>| {
        let mut segments: Vec<u64> = results.iter().map(|r| r.segment).collect();
        segments.sort_unstable();
        segments
    };
    let exact = |query: &str| {
        let q = query.to_string();
        let found = segments(
            engine
                .search_exact(
                    q.clone(),
                    vec![],
                    10,
                    0,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    None,
                    None,
                )
                .unwrap(),
        );
        let page = engine
            .search_and_count_exact(
                q.clone(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(segments(page.results), found, "{query}");
        assert_eq!(page.total_count as usize, found.len(), "{query}");
        let count = engine
            .count_exact(q.clone(), vec![], false, false, None)
            .unwrap();
        assert_eq!(count as usize, found.len(), "{query}");
        let by_book = engine
            .count_by_book_exact(q, vec![], false, false, None)
            .unwrap();
        assert_eq!(
            by_book.values().sum::<u32>() as usize,
            found.len(),
            "{query}"
        );
        found
    };
    let fuzzy = |query: &str, distance: u8| {
        let q = query.to_string();
        let found = segments(
            engine
                .search_fuzzy(
                    q.clone(),
                    vec![],
                    10,
                    0,
                    distance,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    None,
                    None,
                )
                .unwrap(),
        );
        let page = engine
            .search_and_count_fuzzy(
                q.clone(),
                vec![],
                10,
                0,
                distance,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(segments(page.results), found, "{query}");
        assert_eq!(page.total_count as usize, found.len(), "{query}");
        let count = engine
            .count_fuzzy(q.clone(), vec![], distance, false, false, None)
            .unwrap();
        assert_eq!(count as usize, found.len(), "{query}");
        let by_book = engine
            .count_by_book_fuzzy(q, vec![], distance, false, false, None)
            .unwrap();
        assert_eq!(
            by_book.values().sum::<u32>() as usize,
            found.len(),
            "{query}"
        );
        found
    };
    let raw = |term: &str| {
        let terms = vec![term.to_string()];
        let found = segments(
            engine
                .search(
                    terms.clone(),
                    vec![],
                    10,
                    0,
                    0,
                    100,
                    ResultsOrder::Catalogue,
                    None,
                    None,
                )
                .unwrap(),
        );
        let page = engine
            .search_and_count(
                terms.clone(),
                vec![],
                10,
                0,
                0,
                100,
                ResultsOrder::Catalogue,
                None,
                None,
            )
            .unwrap();
        assert_eq!(segments(page.results), found, "{term}");
        assert_eq!(page.total_count as usize, found.len(), "{term}");
        assert_eq!(
            engine.count(terms.clone(), &[], 0, 100).unwrap() as usize,
            found.len()
        );
        let by_book = engine.count_by_book(terms, vec![], 0, 100).unwrap();
        assert_eq!(
            by_book.values().sum::<u32>() as usize,
            found.len(),
            "{term}"
        );
        found
    };

    assert_eq!(exact("תורה"), [0, 1, 2, 3]);
    assert_eq!(raw("תורה"), [0, 1, 2, 3]);
    assert_eq!(fuzzy("תורה", 1), [0, 1, 2, 3]);
    assert_eq!(exact("הרי"), [4]);
    assert_eq!(raw("הרי"), [4]);
    assert!(exact("אהרי").is_empty());
    assert!(raw("אהרי").is_empty());
    assert!(fuzzy("אהרן", 1).is_empty());
    // The marker is a word of its own, merged with neither neighbour.
    assert_eq!(exact("30"), [0, 1, 3]);
    assert_eq!(exact("תורה 30 המשך"), [0, 1, 3]);
    assert!(exact("תורה30").is_empty());
    assert!(exact("30המשך").is_empty());
    // A hanging letter stays inside its word.
    assert_eq!(exact("מנשה"), [5]);
    assert_eq!(fuzzy("מנשה", 1), [5]);

    let text = |query: &str| {
        engine
            .search_exact(
                query.to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap()
            .remove(0)
            .text
    };
    assert!(
        text("הרי").contains("<otzaria-note-marker>א</otzaria-note-marker> <otzaria-note><font color=red>הרי</font> זה</otzaria-note>"),
        "{}",
        text("הרי")
    );
    assert!(
        text("גוף").contains("<font color=red>גוף</font> <otzaria-note-marker>א</otzaria-note-marker> <otzaria-note>הרי זה</otzaria-note>"),
        "{}",
        text("גוף")
    );
}

#[test]
fn paired_readings_share_one_position_so_either_completes_the_phrase() {
    // בראשית ח, יז כפי שהוא במסד: קרי וכתיב בזוג `(X) [Y]`.
    let line = "עַל־הָאָ֖רֶץ <span class=\"mam-kq\"><span class=\"mam-kq-k\">(הוצא)</span> \
                    <span class=\"mam-kq-q\">[הַיְצֵ֣א]</span></span> אִתָּ֑ךְ";
    let (mut engine, _dir) = make_engine();
    for (id, text) in [(1, line), (2, "כי [אם] עונותיכם היו מבדילים")] {
        engine
            .add_document_at(
                id,
                "title",
                "ref",
                "/root",
                text,
                id,
                false,
                &format!("/books/{id}.txt"),
                None,
                None,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    let count = |query: &str, distance: u32| {
        count_advanced_default(
            &engine,
            query.to_string(),
            vec![],
            distance,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
    };

    assert_eq!(count("הארץ היצא אתך", 0), 1);
    assert_eq!(count("הארץ הוצא אתך", 0), 1);
    // הזוג הוא מילה אחת: בלי מרווח הוא אינו נבלע.
    assert_eq!(count("הארץ אתך", 0), 0);
    assert_eq!(count("הארץ אתך", 1), 1);
    // זוג שהודבק לשאילתה מצטמצם לקריאה השנייה.
    assert_eq!(count("הארץ (הוצא) [היצא] אתך", 0), 1);
    // מילה בסוגריים מרובעים לבדה אינה זוג, ונשארת מילה נפרדת.
    assert_eq!(count("כי עונותיכם", 0), 0);
    assert_eq!(count("כי אם עונותיכם", 0), 1);
}

#[test]
fn paired_readings_preserve_query_boundaries_and_real_maqaf_readings() {
    let (mut engine, _dir) = make_engine();
    let documents = [
        // Numbers 23:13 and 1 Samuel 20:24: a maqaf is inside the
        // qere brackets, with the following word immediately adjacent.
        (1, "בלק (לך) [לכה־]נא"),
        (2, "המלך (על) [אל־]הלחם"),
        // No whitespace before '(' still means a separate token.
        (3, "אמר(ת) [רבא] משום"),
        (4, "ראשון (א־ב) [ג] סוף"),
        (5, "שני (א) [ב,ג] סוף"),
        (6, "סימן (ח')[ו'] (ט')[י'] שם"),
    ];
    for (id, text) in documents {
        add(&mut engine, id, text, &format!("/books/{id}.txt"));
    }
    engine.commit().unwrap();
    let count = |query: &str, distance: u32| {
        count_advanced_default(
            &engine,
            query.to_string(),
            vec![],
            distance,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
    };
    for (query, document_id) in [
        ("בלק לך נא", 1),
        ("בלק לכה נא", 1),
        ("בלק (לך) [לכה־]נא", 1),
        ("המלך על הלחם", 2),
        ("המלך אל הלחם", 2),
        ("אמר רבא משום", 3),
        ("אמר(ת) [רבא] משום", 3),
        ("סימן ח ט שם", 6),
        ("סימן ו י שם", 6),
        ("סימן (ח')[ו'] (ט')[י'] שם", 6),
    ] {
        assert_eq!(count(query, 0), 1, "query {query}");
        let source = documents
            .iter()
            .find(|(id, _)| *id == document_id)
            .unwrap()
            .1;
        let pattern = generate_highlight_pattern(
            query.to_string(),
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            None,
        )
        .unwrap();
        assert_eq!(pattern.combined_pattern, "(?!)");
        let matches = pattern
            .matcher
            .unwrap()
            .find_matches(source.to_string(), vec![]);
        assert_eq!(matches.len(), 1, "highlight/index parity: {query}");
        assert_eq!(
            matches[0].word_ranges.len(),
            split_query_words(query.to_string()).len()
        );
    }
    for (query, document_id) in [("ראשון סוף", 4), ("שני סוף", 5)] {
        assert_eq!(count(query, 1), 0, "multi-token reading: {query}");
        assert_eq!(count(query, 3), 1, "full gap: {query}");
        let source = documents
            .iter()
            .find(|(id, _)| *id == document_id)
            .unwrap()
            .1;
        for (distance, expected_matches) in [(1, 0), (3, 1)] {
            let pattern = generate_highlight_pattern(
                query.to_string(),
                distance,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                None,
            )
            .unwrap();
            assert_eq!(
                pattern
                    .matcher
                    .unwrap()
                    .find_matches(source.to_string(), vec![])
                    .len(),
                expected_matches,
                "punctuation highlight/index parity: {query}, distance={distance}"
            );
        }
    }
    // Neither quote-free twins nor paired readings consume an extra word.
    assert_eq!(count("סימן שם", 1), 0);
    assert_eq!(count("סימן שם", 2), 1);
    assert_eq!(count("סימן ח ו ט שם", 0), 0);
}

#[test]
fn query_word_spans_agree_with_full_query_tokens_and_original_utf16_ranges() {
    for query in [
        "הארץ (הוצא) [היצא] אתך",
        "הארץ(הוצא)\n[היצא] אתך",
        "(אב) [אב] אב",
        "😀 הארץ(הוצא) [היצא] אתך 😀",
        "בלק (לך) [לכה־]נא",
        "(א)[ב](ג)[ד] סוף",
        "רמב''ם-משה תוס׳ רמח”ל",
        "א.ב יב[ע]ר לה\u{FEFF}תיר",
        "שָׁלוֹם מ\u{FB1D}ם \u{FB4F}הים",
        "(א־ב) [ג] (א) [ב,ג]",
        "כי [אם] עונותיכם",
    ] {
        let spans = query_word_spans(query.to_string());
        assert_eq!(
            spans.iter().map(|s| s.word.clone()).collect::<Vec<_>>(),
            split_query_words(query.to_string()),
            "full-query words differ for {query}"
        );
        let utf16 = query.encode_utf16().collect::<Vec<_>>();
        let mut previous_end = 0;
        for span in spans {
            assert!(span.start >= previous_end, "overlapping spans: {query}");
            let source = String::from_utf16(&utf16[span.start as usize..span.end as usize])
                .expect("source range split a surrogate pair");
            assert_eq!(
                split_query_words(source),
                vec![span.word],
                "source range changed the selected word: {query}"
            );
            previous_end = span.end;
        }
    }
    assert_eq!(
        query_word_spans("(אב) [אב] אב".to_string()),
        vec![
            QueryWordSpan {
                word: "אב".to_string(),
                start: 6,
                end: 8
            },
            QueryWordSpan {
                word: "אב".to_string(),
                start: 10,
                end: 12
            },
        ]
    );
    assert_eq!(
        query_word_spans("😀 רמב''ם-משה".to_string()),
        vec![
            QueryWordSpan {
                word: "רמב\"ם".to_string(),
                start: 3,
                end: 9
            },
            QueryWordSpan {
                word: "משה".to_string(),
                start: 10,
                end: 13
            },
        ]
    );
}

#[test]
fn add_document_and_batch_normalize_like_add_text_book() {
    // ה-API הישירים חשופים ב-FFI — ההנחה "הקלט כבר מנורמל" נאכפת:
    // HTML מוסר, ניקוד מוסר מהשדה הרגיל, והעותק המנוקד נבנה מהגולמי.
    let raw = "<b>בְּרֵאשִׁית</b>  בָּרָא אלהים";
    let (mut engine, _dir) = make_engine();
    engine
        .add_document_at(
            1,
            "title",
            "ref",
            "/root",
            raw,
            0,
            false,
            "/books/a.txt",
            None,
            None,
            None,
        )
        .unwrap();
    engine
        .add_documents_batch(vec![DocumentInput {
            ordinal: 1,
            title: "title".to_string(),
            reference: "ref".to_string(),
            topics: "/root".to_string(),
            text: raw.to_string(),
            segment: 1,
            is_pdf: false,
            file_path: "/books/b.txt".to_string(),
            content_hash: None,
            text_hash: None,
            source_hash: None,
            text_vocalized: Some("<b>בְּרֵאשִׁית</b>  בָּרָא אלהים".to_string()),
            section_ordinal: None,
            extra_facets: None,
            text_storage: None,
        }])
        .unwrap();
    engine
        .upsert_documents_batch(vec![DocumentInput {
            ordinal: 2,
            title: "title".to_string(),
            reference: "ref".to_string(),
            topics: "/root".to_string(),
            text: raw.to_string(),
            segment: 2,
            is_pdf: false,
            file_path: "/books/c.txt".to_string(),
            content_hash: None,
            text_hash: None,
            source_hash: None,
            text_vocalized: Some("<b>בְּרֵאשִׁית</b>  בָּרָא אלהים".to_string()),
            section_ordinal: None,
            extra_facets: None,
            text_storage: None,
        }])
        .unwrap();
    engine.commit().unwrap();

    // שלושת המסלולים מאונדקסים ומאוחסנים בצורה המנורמלת.
    let results = engine
        .search_exact(
            "בראשית ברא".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(results.len(), 3);
    for hit in &results {
        // הטקסט השמור מנורמל (בלי ה-HTML המקורי ובלי ניקוד בשדה
        // הרגיל); תגי ה-font הם עיטור ההדגשה של תוצאת החיפוש.
        let clean = hit
            .text
            .replace("<font color=red>", "")
            .replace("</font>", "");
        assert_eq!(clean, "בראשית ברא אלהים");
    }

    // העותקים המנוקדים נורמלו גם הם (ה-HTML הוסר): מסלולי ה-batch
    // (add/upsert) דרך הנרמול הנאכף על text_vocalized שסופק, ומסלול
    // add_document דרך העותק שהוא בונה בעצמו מהקלט הגולמי — חיפוש
    // מנוקד מוצא את כולן.
    let vocalized = engine
        .search_exact(
            "בְּרֵאשִׁית".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            true,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(vocalized.len(), 3);
}

#[test]
fn analyzers_drop_tokens_longer_than_cap() {
    // RemoveLongFilter על כל האנליזטורים: זבל base64 (נמדדו ריצות של
    // אלפי תווים בקורפוס) לא נכנס למילון הטרמים — וגם צד השאילתה מפיל
    // את הטוקן, כך שספירת PhraseQuery נשארת עקבית עם האינדקס.
    let (mut engine, _dir) = make_engine();
    let garbage = "א".repeat(80); // 160 בייט — מעל התקרה
    let long_but_legit = "א".repeat(60); // 120 בייט — מתחת לתקרה
    add(
        &mut engine,
        1,
        &format!("שלום {garbage} עולם"),
        "/books/junk.txt",
    );
    engine.commit().unwrap();

    // הטוקן הארוך לא נטמע; המילים סביבו כן.
    assert_eq!(
        engine
            .index_token_texts(&format!("שלום {garbage}"))
            .unwrap(),
        vec!["שלום"]
    );
    assert_eq!(engine.index_token_texts(&long_but_legit).unwrap().len(), 1);
    assert_eq!(search_ids(&mut engine, "שלום"), vec![1]);
    assert_eq!(search_ids(&mut engine, &garbage), Vec::<u64>::new());
}

#[test]
fn add_text_book_bytes_matches_string_path() {
    // מסלול הבייטים (SQLite BLOB → Uint8List) חייב לייצר בדיוק את אותם
    // מסמכים ואותה טביעת אצבע כמו מסלול ה-String.
    let text = "<h1>ספר בראשית</h1>\n<h2>פרק א</h2>\nבְּרֵאשִׁית ברא אלהים\n<h2>פרק ב</h2>\nויכלו השמים";
    let (mut engine, _dir) = make_engine();
    let added = engine
        .add_text_book_bytes_placed(
            "בראשית".to_string(),
            "/root".to_string(),
            "/books/bereshit.txt".to_string(),
            5,
            DEFAULT_GENERATION_ORDER,
            text.as_bytes().to_vec(),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    assert_eq!(added, 5);
    engine.commit().unwrap();

    let results = engine
        .search_exact(
            "ויכלו".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].reference, "ספר בראשית, פרק ב");
    assert_eq!(results[0].id, ((5u64 + 1) << 32) + 5);

    let fingerprints = engine.get_book_fingerprints().unwrap();
    assert_eq!(
        fingerprints.get("/books/bereshit.txt"),
        Some(&compute_book_fingerprint(
            text.to_string(),
            "בראשית".to_string(),
            "/root".to_string(),
            None,
        ))
    );
    let text_fingerprints = engine.get_book_text_fingerprints().unwrap();
    assert_eq!(
        text_fingerprints.get("/books/bereshit.txt"),
        Some(&compute_content_fingerprint(text.to_string()))
    );
}

#[test]
fn add_text_book_empty_text_adds_nothing() {
    let (mut engine, _dir) = make_engine();
    let added = engine
        .add_text_book_placed(
            "ריק".to_string(),
            "/root".to_string(),
            "/books/empty.txt".to_string(),
            1,
            DEFAULT_GENERATION_ORDER,
            String::new(),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    assert_eq!(added, 0);
    engine.commit().unwrap();
    assert_eq!(engine.get_document_count(), 0);
}

#[test]
fn add_pdf_book_filters_garbage_and_encodes_ids() {
    let (mut engine, _dir) = make_engine();
    let pages = vec![
        PdfPageInput {
            reference: "ספר, עמוד 1".to_string(),
            text: "שורה ראשונה בעמוד\n\n≡≡≡ ∴∴∴ ⊕⊗⊘".to_string(),
            page_index: 0,
        },
        PdfPageInput {
            reference: "ספר, עמוד 2".to_string(),
            text: "בְּרֵאשִׁית ברא אלהים".to_string(),
            page_index: 1,
        },
    ];
    let added = engine
        .add_pdf_book_placed(
            "ספר".to_string(),
            "/root".to_string(),
            "C:/books/sefer.pdf".to_string(),
            5,
            DEFAULT_GENERATION_ORDER,
            pages,
            None,
        )
        .unwrap();
    // השורה הריקה ושורת הסימנים סוננו כזבל — נותרו שתי שורות תוכן.
    assert_eq!(added, 2);
    engine.commit().unwrap();

    // הטקסט מנורמל לפני האינדוקס — שאילתה ללא ניקוד מוצאת אותו.
    let results = engine
        .search_exact(
            "בראשית ברא".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    let hit = &results[0];
    assert_eq!(hit.reference, "ספר, עמוד 2");
    // segment = אינדקס העמוד; ordinal רץ על שורות התוכן בלבד.
    assert_eq!(hit.segment, 1);
    assert_eq!(hit.id, ((5u64 + 1) << 32) + 2);
    assert!(hit.is_pdf);

    // ל-PDF אין טביעת אצבע — contentHash נחתם כ-0 (לא נרשם).
    let fingerprints = engine.get_book_fingerprints().unwrap();
    assert_eq!(fingerprints.get("C:/books/sefer.pdf"), Some(&0u64));
}

#[test]
fn pdf_pages_from_bytes_unpacks_references_and_texts() {
    let bytes = "ספר, עמוד 1שורה\nשנייהעמוד 2בראשית".as_bytes();
    let ref1 = "ספר, עמוד 1".len() as u32;
    let text1 = ref1 + "שורה\nשנייה".len() as u32;
    let ref2 = text1 + "עמוד 2".len() as u32;
    let ends = [ref1, text1, ref2, bytes.len() as u32];
    let pages = pdf_pages_from_bytes(bytes, &ends, &[3, 7]).unwrap();
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].reference, "ספר, עמוד 1");
    assert_eq!(pages[0].text, "שורה\nשנייה");
    assert_eq!(pages[0].page_index, 3);
    assert_eq!(pages[1].reference, "עמוד 2");
    assert_eq!(pages[1].text, "בראשית");
    assert_eq!(pages[1].page_index, 7);

    // A page with an empty reference and text is still a page.
    let pages = pdf_pages_from_bytes(b"", &[0, 0], &[0]).unwrap();
    assert_eq!(
        (pages[0].reference.as_ref(), pages[0].text.as_ref()),
        ("", "")
    );

    assert!(pdf_pages_from_bytes(bytes, &ends[..3], &[3, 7]).is_err());
    assert!(pdf_pages_from_bytes(bytes, &[ref1, 1], &[0]).is_err());
    assert!(pdf_pages_from_bytes(bytes, &[ref1, 9999], &[0]).is_err());
}

#[test]
fn pdf_pages_from_bytes_validates_all_offsets_and_consumes_the_buffer() {
    let bytes = b"abcdef";
    for (ends, indexes) in [
        (vec![], vec![]),
        (vec![0, 5], vec![0]),
        (vec![0], vec![0]),
        (vec![0, 6], vec![]),
        (vec![0, 6], vec![0, 1]),
        (vec![2, 1], vec![0]),
        (vec![0, 4, 3, 6], vec![0, 1]),
        (vec![0, u32::MAX], vec![0]),
    ] {
        assert!(pdf_pages_from_bytes(bytes, &ends, &indexes).is_err());
    }
    assert!(pdf_pages_from_bytes(b"", &[], &[]).unwrap().is_empty());
    let pages = pdf_pages_from_bytes(b"", &[0, 0, 0, 0], &[7, 7]).unwrap();
    assert_eq!(pages.len(), 2);
}

#[test]
fn pdf_pages_from_bytes_borrows_utf8_and_replaces_only_invalid_slices() {
    let bytes = "עמודשלום עולם".as_bytes();
    let pages = pdf_pages_from_bytes(bytes, &[8, bytes.len() as u32], &[u32::MAX]).unwrap();
    assert!(matches!(pages[0].reference, Cow::Borrowed(_)));
    assert!(matches!(pages[0].text, Cow::Borrowed(_)));
    assert_eq!(pages[0].reference.as_ptr(), bytes.as_ptr());
    assert_eq!(pages[0].text.as_ptr(), bytes[8..].as_ptr());
    assert_eq!(pages[0].page_index, u32::MAX);

    // A code point split between reference and text is replaced in each
    // slice independently, exactly like invalid UTF-8 in the text itself.
    let bytes = [0xd7, 0xa9, b'a', 0xff];
    let pages = pdf_pages_from_bytes(&bytes, &[1, 4], &[0]).unwrap();
    assert_eq!(pages[0].reference, "\u{fffd}");
    assert_eq!(pages[0].text, "\u{fffd}a\u{fffd}");
    assert!(matches!(pages[0].reference, Cow::Owned(_)));
    assert!(matches!(pages[0].text, Cow::Owned(_)));
}

#[test]
fn add_pdf_book_bytes_matches_documents_stamps_and_search() {
    let (mut original, _original_dir) = make_engine();
    let (mut packed, _packed_dir) = make_engine();
    let pages = vec![
        PdfPageInput {
            reference: String::new(),
            text: String::new(),
            page_index: 0,
        },
        PdfPageInput {
            reference: "עמוד א 😀".into(),
            text: "\n≡≡≡ ∴∴∴ ⊕⊗⊘\nבְּרֵאשִׁית ברא אלהים\r\nשורה שניה".into(),
            page_index: 3,
        },
        PdfPageInput {
            reference: String::new(),
            text: "שלום עולם".into(),
            page_index: u32::MAX,
        },
    ];
    let mut bytes = Vec::new();
    let mut ends = Vec::new();
    let mut indexes = Vec::new();
    for page in &pages {
        bytes.extend_from_slice(page.reference.as_bytes());
        ends.push(bytes.len() as u32);
        bytes.extend_from_slice(page.text.as_bytes());
        ends.push(bytes.len() as u32);
        indexes.push(page.page_index);
    }
    let facets = Some(vec!["/author/מחבר".into(), "/era/אחרונים".into()]);
    let expected = original
        .add_pdf_book(
            "ספר".into(),
            "/בדיקות".into(),
            "/book.pdf".into(),
            pages,
            facets.clone(),
        )
        .unwrap();
    let actual = packed
        .add_pdf_book_bytes(
            "ספר".into(),
            "/בדיקות".into(),
            "/book.pdf".into(),
            bytes,
            ends,
            indexes,
            facets,
        )
        .unwrap();
    assert_eq!(actual, 3);
    assert_eq!(actual, expected);
    original.commit().unwrap();
    packed.commit().unwrap();
    assert_eq!(
        packed.get_book_stamps().unwrap(),
        original.get_book_stamps().unwrap()
    );
    assert_eq!(
        packed.get_book_fingerprints().unwrap(),
        original.get_book_fingerprints().unwrap()
    );

    // Compare every stored field, including ids, page indexes and references.
    let documents = |engine: &SearchEngine| {
        let searcher = engine.index_reader.searcher();
        let hits = searcher
            .search(&AllQuery, &TopDocs::with_limit(20).order_by_score())
            .unwrap();
        let mut docs: Vec<String> = hits
            .into_iter()
            .map(|(_, addr)| {
                searcher
                    .doc::<TantivyDocument>(addr)
                    .unwrap()
                    .to_json(&engine.schema)
            })
            .collect();
        docs.sort();
        docs
    };
    assert_eq!(documents(&packed), documents(&original));
    for query in ["בראשית ברא", "שורה שניה", "אלהים שורה", "שלום עולם"]
    {
        let search = |engine: &SearchEngine| {
            engine
                .search_exact(
                    query.into(),
                    vec!["/author/מחבר".into()],
                    20,
                    0,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    None,
                    None,
                )
                .unwrap()
                .into_iter()
                .map(|hit| {
                    (
                        hit.id,
                        hit.segment,
                        hit.reference,
                        hit.text,
                        hit.continues_to_next_line,
                    )
                })
                .collect::<Vec<_>>()
        };
        let expected = search(&original);
        assert!(!expected.is_empty(), "fixture must exercise query {query}");
        assert_eq!(search(&packed), expected, "query {query}");
    }
}

#[test]
fn add_pdf_book_bytes_rejects_bad_input_without_partial_documents() {
    let (mut engine, _dir) = make_engine();
    // The first page is valid; a bad later page must not enqueue that first page.
    let bytes = "עמודשלום עולם".as_bytes().to_vec();
    let end = bytes.len() as u32;
    assert!(engine
        .add_pdf_book_bytes(
            "ספר".into(),
            "/בדיקות".into(),
            "/book.pdf".into(),
            bytes.clone(),
            vec![8, end, end - 1, end],
            vec![0, 1],
            None,
        )
        .is_err());
    engine.commit().unwrap();
    assert_eq!(engine.get_document_count(), 0);
    assert!(engine.get_book_stamps().unwrap().is_empty());
    assert_eq!(
        engine
            .add_pdf_book_bytes(
                "ספר".into(),
                "/בדיקות".into(),
                "/book.pdf".into(),
                bytes,
                vec![8, end],
                vec![7],
                None,
            )
            .unwrap(),
        1
    );
    engine.commit().unwrap();
    assert_eq!(engine.get_document_count(), 1);
}

#[test]
fn bulk_indexing_skips_merges_and_optimize_bounds_segments() {
    let (mut engine, _dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    // כמה commit-ים ⇒ כמה סגמנטים; ב-bulk אין מיזוג רקע שמאחד אותם.
    for i in 0..3u64 {
        add(&mut engine, i + 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();
    }
    assert!(engine.index.searchable_segment_ids().unwrap().len() > 1);

    engine.set_bulk_indexing(false).unwrap();
    engine.optimize().unwrap();
    assert!(engine.index.searchable_segment_ids().unwrap().len() <= MAX_SEGMENTS_AFTER_OPTIMIZE);
    assert_eq!(engine.get_document_count(), 3);
}

/// כל commit מפזר מסמכים על כמה סגמנטים (thread לכל סגמנט); הבדיקות
/// שצריכות "סגמנט גדול אחד" מאחדות אותו כאן, מחוץ ל-optimize הנבדק.
fn collapse_to_one_segment(engine: &mut SearchEngine) {
    let live = engine.take_writer().unwrap();
    drop(live);
    let mut writer = engine.open_writer_no_merge().unwrap();
    let ids = engine.index.searchable_segment_ids().unwrap();
    writer.merge(&ids).wait().unwrap();
    writer.wait_merging_threads().unwrap();
    engine.restore_writer().unwrap();
    engine.index_reader.reload().unwrap();
    assert_eq!(engine.index.searchable_segment_ids().unwrap().len(), 1);
}

fn count_hits(engine: &SearchEngine, word: &str) -> usize {
    engine.count(vec![word.to_string()], &[], 0, 1).unwrap() as usize
}

#[test]
fn optimize_merges_smallest_segments_down_to_cap() {
    let (mut engine, _dir) = make_engine();
    // bulk נשאר פעיל גם ב-optimize — אחרת LogMergePolicy היה מאחד ברקע
    // והבדיקה לא הייתה מודדת את optimize עצמו.
    engine.set_bulk_indexing(true).unwrap();
    let total = MAX_SEGMENTS_AFTER_OPTIMIZE as u64 + 4;
    for i in 0..total {
        add(&mut engine, i + 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();
    }
    assert_eq!(
        engine.index.searchable_segment_ids().unwrap().len(),
        total as usize
    );

    engine.optimize().unwrap();
    assert_eq!(
        engine.index.searchable_segment_ids().unwrap().len(),
        MAX_SEGMENTS_AFTER_OPTIMIZE
    );
    assert_eq!(engine.get_document_count(), total);
    assert_eq!(count_hits(&engine, "שלום"), total as usize);
}

fn count_idx_files(dir: &TempDir) -> usize {
    std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().extension() == Some("idx".as_ref()))
        .count()
}

fn dir_size_bytes(dir: &TempDir) -> u64 {
    std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum()
}

#[test]
fn optimize_garbage_collects_merged_away_segment_files() {
    let (mut engine, dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    let total = MAX_SEGMENTS_AFTER_OPTIMIZE as u64 + 4;
    for i in 0..total {
        add(&mut engine, i + 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();
    }
    // Pin the pre-optimize segments the way a live search would.
    engine.index_reader.reload().unwrap();
    assert_eq!(count_idx_files(&dir), total as usize);
    let size_before = dir_size_bytes(&dir);

    engine.optimize().unwrap();
    let searchable = engine.index.searchable_segment_ids().unwrap().len();
    assert_eq!(searchable, MAX_SEGMENTS_AFTER_OPTIMIZE);
    assert_eq!(
        count_idx_files(&dir),
        searchable,
        "merged-away segment files must be garbage-collected"
    );
    assert!(
        dir_size_bytes(&dir) <= size_before,
        "index directory must not grow after optimize"
    );
}

#[test]
fn optimize_leaves_large_segment_untouched_next_to_tiny_one() {
    let (mut engine, _dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    for i in 0..500u64 {
        add(&mut engine, i + 1, "בראשית ברא", "/books/big.txt");
    }
    engine.commit().unwrap();
    collapse_to_one_segment(&mut engine);
    let big_id = engine.index.searchable_segment_ids().unwrap()[0];

    add(&mut engine, 1_000, "שלום עולם", "/books/small.txt");
    engine.commit().unwrap();
    assert_eq!(engine.index.searchable_segment_ids().unwrap().len(), 2);

    engine.optimize().unwrap();
    let ids = engine.index.searchable_segment_ids().unwrap();
    assert_eq!(ids.len(), 2);
    // הסגמנט הגדול לא נכתב מחדש — אותו SegmentId עדיין שם.
    assert!(ids.contains(&big_id));
    assert_eq!(engine.get_document_count(), 501);
}

fn large_unique_text() -> String {
    let mut text = String::from("שלום ");
    for i in 0..1_000 {
        text.push_str(&format!("מילה{i} "));
    }
    text
}

#[test]
fn optimize_does_not_rewrite_eight_large_segments_for_one_new_book() {
    let (mut engine, _dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    let large_text = large_unique_text();
    for id in 1..=MAX_SEGMENTS_AFTER_OPTIMIZE as u64 {
        add(&mut engine, id, &large_text, &format!("/books/{id}.txt"));
        engine.commit().unwrap();
    }
    let original_ids = engine.index.searchable_segment_ids().unwrap();
    assert_eq!(original_ids.len(), MAX_SEGMENTS_AFTER_OPTIMIZE);

    add(&mut engine, 100, "שלום", "/books/new.txt");
    engine.commit().unwrap();
    let metas = engine.index.searchable_segment_metas().unwrap();
    assert_eq!(metas.len(), MAX_SEGMENTS_AFTER_OPTIMIZE + 1);
    let mut sizes: Vec<_> = metas
        .iter()
        .map(|meta| segment_disk_bytes(&engine.index_path, meta).unwrap())
        .collect();
    sizes.sort_unstable();
    assert!(
        sizes[1] > sizes[0] * MAX_COMPACT_SIZE_RATIO,
        "test requires a genuinely small new segment: {sizes:?}"
    );

    engine.optimize().unwrap();
    let after = engine.index.searchable_segment_ids().unwrap();
    assert_eq!(after.len(), MAX_SEGMENTS_AFTER_OPTIMIZE + 1);
    assert!(original_ids.iter().all(|id| after.contains(id)));
    assert_eq!(count_hits(&engine, "שלום"), MAX_SEGMENTS_AFTER_OPTIMIZE + 1);

    add(&mut engine, 101, "שלום", "/books/next.txt");
    engine.commit().unwrap();
    engine.optimize().unwrap();
    let after = engine.index.searchable_segment_ids().unwrap();
    assert_eq!(after.len(), MAX_SEGMENTS_AFTER_OPTIMIZE + 1);
    assert!(original_ids.iter().all(|id| after.contains(id)));
    assert_eq!(count_hits(&engine, "שלום"), MAX_SEGMENTS_AFTER_OPTIMIZE + 2);
}

/// אינדקס שנבנה מאפס משאיר עשרות סגמנטים דומים בגודלם ועוד אחד זעיר
/// (השטיפה האחרונה של אחד מחוטי הכתיבה). ריצה 34898795644 סיימה כך עם
/// 193 סגמנטים, ו-optimize ויתר אחרי מיזוג אחד — 188. הסיבה: החסם
/// MAX_COMPACT_SIZE_RATIO נמדד מול הסגמנט הקטן *בכל האינדקס*, כך
/// שסגמנט חריג אחד פוסל את מיזוגם של כל השאר זה עם זה.
#[test]
fn optimize_compacts_similar_segments_that_one_tiny_outlier_anchors() {
    let (mut engine, _dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    let large_text = large_unique_text();
    let large_count = MAX_SEGMENTS_AFTER_OPTIMIZE + 4;
    for id in 1..=large_count as u64 {
        add(&mut engine, id, &large_text, &format!("/books/{id}.txt"));
        engine.commit().unwrap();
    }
    add(&mut engine, 100, "שלום", "/books/tiny.txt");
    engine.commit().unwrap();

    let metas = engine.index.searchable_segment_metas().unwrap();
    assert_eq!(metas.len(), large_count + 1);
    let mut sizes: Vec<_> = metas
        .iter()
        .map(|meta| segment_disk_bytes(&engine.index_path, meta).unwrap())
        .collect();
    sizes.sort_unstable();
    // התרחיש נשען על שתי עובדות: החריג באמת זעיר,
    assert!(
        sizes[1] > sizes[0] * MAX_COMPACT_SIZE_RATIO,
        "the outlier is not small enough to anchor: {sizes:?}"
    );
    // וכל השאר דומים זה לזה — כלומר מיזוגם אינו כתיבה-מחדש מיותרת.
    assert!(
        sizes[large_count] <= sizes[1] * MAX_COMPACT_SIZE_RATIO,
        "the rest are not similarly sized: {sizes:?}"
    );

    engine.optimize().unwrap();
    let after = engine.index.searchable_segment_ids().unwrap();
    assert_eq!(after.len(), MAX_SEGMENTS_AFTER_OPTIMIZE);
    assert_eq!(count_hits(&engine, "שלום"), large_count + 1);
}

#[test]
fn optimize_uses_disk_bytes_instead_of_document_count() {
    let (mut engine, _dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    add(&mut engine, 1, &large_unique_text(), "/books/large.txt");
    engine.commit().unwrap();
    let large_id = engine.index.searchable_segment_ids().unwrap()[0];
    for id in 2..=9 {
        add(&mut engine, id, "שלום", &format!("/books/{id}.txt"));
        engine.commit().unwrap();
    }
    assert_eq!(engine.index.searchable_segment_ids().unwrap().len(), 9);
    engine.optimize().unwrap();
    let after = engine.index.searchable_segment_ids().unwrap();
    assert_eq!(after.len(), MAX_SEGMENTS_AFTER_OPTIMIZE);
    assert!(after.contains(&large_id));
    assert_eq!(engine.get_document_count(), 9);
    assert_eq!(count_hits(&engine, "שלום"), 9);
}

// A handle like the one the reader's meta-file watcher opens every 500 ms: replacing
// meta.json under it failed the commit with "Access is denied. (os error 5)".
#[cfg(windows)]
#[test]
fn replace_meta_json_while_a_reader_holds_it() {
    let (mut engine, dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/1.txt");
    let held = fs::File::open(dir.path().join("meta.json")).unwrap();
    engine.commit().unwrap();
    drop(held);
    assert_eq!(count_hits(&engine, "שלום"), 1);
}

#[cfg(windows)]
#[test]
fn replace_meta_json_held_past_the_retries_fails_and_the_next_commit_recovers() {
    use std::os::windows::fs::OpenOptionsExt;
    let (mut engine, dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/1.txt");
    // FILE_SHARE_READ | FILE_SHARE_WRITE: no delete sharing, which blocks even std's rename.
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2)
        .open(dir.path().join("meta.json"))
        .unwrap();
    let error = engine.commit().unwrap_err();
    assert!(error.to_string().contains("os error 5"), "{error}");
    drop(held);
    engine.commit().unwrap();
    assert_eq!(count_hits(&engine, "שלום"), 1);
}

#[test]
fn index_metadata_survives_a_write_that_fails_before_its_rename() {
    let dir = TempDir::new().unwrap();
    write_index_metadata(dir.path(), &index_metadata(4, None)).unwrap();
    let before = fs::read(index_metadata_path(dir.path())).unwrap();
    crate::index_directory::fail_next_rename();
    assert!(write_current_index_metadata(dir.path()).is_err());
    assert_eq!(fs::read(index_metadata_path(dir.path())).unwrap(), before);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

/// גדלים סינתטיים: כאן נבדק הכלל עצמו, בלי לתלות אותו בגודל שסגמנט
/// אמיתי מזדמן לקבל. `max_doc` הוא 1 כדי שענף המחיקות לא ייגע בזה.
fn sized(engine: &SearchEngine, sizes: &[u64]) -> Vec<(u64, SegmentMeta)> {
    sizes
        .iter()
        .map(|&bytes| {
            (
                bytes,
                engine
                    .index
                    .new_segment_meta(SegmentId::generate_random(), 1),
            )
        })
        .collect()
}

fn chosen(candidate: Option<Vec<SegmentId>>) -> usize {
    candidate.map_or(0, |ids| ids.len())
}

#[test]
fn the_background_writer_merges_only_the_smallest_level() {
    let (engine, _dir) = make_engine();
    let nine_large_behind_one_tiny =
        sized(&engine, &[1, 200, 201, 202, 203, 204, 205, 206, 207, 208]);
    // עשרה סגמנטים, והרמה שמעל היחיד מונה תשעה. זה המקרה ששתי בדיקות
    // המדיניות הקיימות עוצרות סגמנט אחד לפניו, ובו נמדדת ההבטחה של #21
    // על מכונת המשתמש: ברקע לא נכתב כאן דבר מחדש.
    assert_eq!(
        chosen(select_smallest_level(&nine_large_behind_one_tiny)),
        0
    );
    // optimize, כשמבקשים ממנו במפורש, כן מכווץ את אותה רמה.
    assert_eq!(
        chosen(select_level_to_compact(&nine_large_behind_one_tiny)),
        3
    );

    // רמה קטנה ביותר עם שני חברים ומעלה — שתי הדרכים מסכימות, וזו
    // ההתנהגות שהייתה כאן מאז #21.
    for sizes in [
        vec![1, 2, 3, 4, 5, 6, 7, 8, 900],
        vec![100; 100],
        vec![0; 9],
    ] {
        let metas = sized(&engine, &sizes);
        assert_eq!(
            chosen(select_smallest_level(&metas)),
            chosen(select_level_to_compact(&metas)),
            "the two disagree on a smallest level that can merge: {sizes:?}"
        );
    }
}

#[test]
fn neither_selector_merges_at_or_below_the_target() {
    let (engine, _dir) = make_engine();
    for count in 0..=MAX_SEGMENTS_AFTER_OPTIMIZE {
        let metas = sized(&engine, &vec![100; count]);
        assert_eq!(chosen(select_smallest_level(&metas)), 0);
        assert_eq!(chosen(select_level_to_compact(&metas)), 0);
    }
}

#[test]
fn a_compaction_never_takes_the_index_below_the_target() {
    let (engine, _dir) = make_engine();
    for sizes in [
        vec![100; MAX_SEGMENTS_AFTER_OPTIMIZE + 1],
        vec![100; 50],
        vec![1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
        vec![u64::MAX, u64::MAX, 1, 1, 1, 1, 1, 1, 1, 1, 1],
    ] {
        let metas = sized(&engine, &sizes);
        for candidate in [
            select_smallest_level(&metas),
            select_level_to_compact(&metas),
        ] {
            let Some(ids) = candidate else { continue };
            // מיזוג של k סגמנטים מוריד k-1, ולעולם לא מתחת ליעד.
            assert!(ids.len() >= 2, "a merge of {} is pointless", ids.len());
            assert!(
                sizes.len() - ids.len() + 1 >= MAX_SEGMENTS_AFTER_OPTIMIZE,
                "{sizes:?} would drop to {}",
                sizes.len() - ids.len() + 1
            );
        }
    }
}

/// הבדיקה שמתחתיה עוצרת בתשעה סגמנטים, שבהם הרמה הגדולה מונה שמונה.
/// בעשרה היא מונה תשעה — ורק שם מתברר אם מדיניות הרקע התחילה לכתוב
/// מחדש סגמנטים בריאים על מכונה של מישהו באמצע אינדוקס רגיל. המדיניות
/// נקראת כאן ישירות: השארת המיזוג לתזמון של tantivy הפכה בדיקה כזו
/// לריקה מתוכן — היא עברה גם כשהמדיניות הופנתה לבורר האגרסיבי.
#[test]
fn the_background_policy_leaves_nine_large_segments_behind_one_tiny_alone() {
    let (mut engine, _dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    add(&mut engine, 100, "שלום", "/books/tiny.txt");
    engine.commit().unwrap();
    let large_text = large_unique_text();
    for id in 1..=(MAX_SEGMENTS_AFTER_OPTIMIZE as u64 + 1) {
        add(&mut engine, id, &large_text, &format!("/books/{id}.txt"));
        engine.commit().unwrap();
    }
    let metas = engine.index.searchable_segment_metas().unwrap();
    assert_eq!(metas.len(), MAX_SEGMENTS_AFTER_OPTIMIZE + 2);
    let sized = sized_segment_metas(&engine.index_path, metas.clone()).unwrap();
    let mut sizes: Vec<_> = sized.iter().map(|(bytes, _)| *bytes).collect();
    sizes.sort_unstable();
    assert!(
        sizes[1] > sizes[0] * MAX_COMPACT_SIZE_RATIO,
        "the outlier is not small enough to leave the level above it alone: {sizes:?}"
    );

    let policy = SizeAwareMergePolicy {
        index_path: engine.index_path.clone(),
    };
    assert!(
        policy.compute_merge_candidates(&metas).is_empty(),
        "the writer would rewrite large segments in the background"
    );
    // optimize, כשמבקשים ממנו במפורש, כן מכווץ את אותה רמה.
    assert_eq!(
        select_level_to_compact(&sized).map_or(0, |ids| ids.len()),
        3
    );
}

#[test]
fn normal_writer_does_not_merge_eight_large_segments_for_a_tiny_book() {
    let (mut engine, dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    let large_text = large_unique_text();
    for id in 1..=MAX_SEGMENTS_AFTER_OPTIMIZE as u64 {
        add(&mut engine, id, &large_text, &format!("/books/{id}.txt"));
        engine.commit().unwrap();
    }
    let original_ids = engine.index.searchable_segment_ids().unwrap();
    assert_eq!(original_ids.len(), MAX_SEGMENTS_AFTER_OPTIMIZE);

    drop(engine);
    let mut engine = SearchEngine::new(&dir_path_string(&dir));
    add(&mut engine, 100, "שלום", "/books/new.txt");
    engine.commit().unwrap();
    let writer = engine.take_writer().unwrap();
    writer.wait_merging_threads().unwrap();
    engine.restore_writer().unwrap();

    let after = engine.index.searchable_segment_ids().unwrap();
    assert_eq!(after.len(), MAX_SEGMENTS_AFTER_OPTIMIZE + 1);
    assert!(original_ids.iter().all(|id| after.contains(id)));
    engine.index_reader.reload().unwrap();
    assert_eq!(count_hits(&engine, "שלום"), MAX_SEGMENTS_AFTER_OPTIMIZE + 1);
}

#[test]
fn normal_writer_merges_similarly_sized_small_segments() {
    let (mut engine, _dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    for id in 1..=MAX_SEGMENTS_AFTER_OPTIMIZE as u64 {
        add(&mut engine, id, "שלום", &format!("/books/{id}.txt"));
        engine.commit().unwrap();
    }
    assert_eq!(
        engine.index.searchable_segment_ids().unwrap().len(),
        MAX_SEGMENTS_AFTER_OPTIMIZE
    );

    engine.set_bulk_indexing(false).unwrap();
    add(&mut engine, 100, "שלום", "/books/new.txt");
    engine.commit().unwrap();
    let writer = engine.take_writer().unwrap();
    writer.wait_merging_threads().unwrap();
    engine.restore_writer().unwrap();
    engine.index_reader.reload().unwrap();
    assert_eq!(
        engine.index.searchable_segment_ids().unwrap().len(),
        MAX_SEGMENTS_AFTER_OPTIMIZE
    );
    assert_eq!(count_hits(&engine, "שלום"), MAX_SEGMENTS_AFTER_OPTIMIZE + 1);
}

#[test]
fn optimize_commits_pending_document_into_an_empty_index() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/new.txt");
    assert!(engine.index.searchable_segment_ids().unwrap().is_empty());
    engine.optimize().unwrap();
    assert_eq!(engine.get_document_count(), 1);
    assert_eq!(count_hits(&engine, "שלום"), 1);
}

#[test]
#[ignore = "manual timing on an isolated copy of a real index"]
fn benchmark_real_index_one_vs_nine_segments() {
    use std::hint::black_box;
    use tantivy::query::TermQuery;

    fn one_search(searcher: &Searcher, query: &TermQuery, top_docs: bool) -> std::time::Duration {
        let started = Instant::now();
        if top_docs {
            black_box(
                searcher
                    .search(query, &TopDocs::with_limit(10).order_by_score())
                    .unwrap(),
            );
        } else {
            black_box(searcher.search(query, &Count).unwrap());
        }
        started.elapsed()
    }

    fn paired_medians(
        one: &Searcher,
        nine: &Searcher,
        query: &TermQuery,
        top_docs: bool,
    ) -> (std::time::Duration, std::time::Duration) {
        for _ in 0..5 {
            one_search(one, query, top_docs);
            one_search(nine, query, top_docs);
        }
        let mut one_samples = Vec::new();
        let mut nine_samples = Vec::new();
        for i in 0..31 {
            if i % 2 == 0 {
                one_samples.push(one_search(one, query, top_docs));
                nine_samples.push(one_search(nine, query, top_docs));
            } else {
                nine_samples.push(one_search(nine, query, top_docs));
                one_samples.push(one_search(one, query, top_docs));
            }
        }
        one_samples.sort_unstable();
        nine_samples.sort_unstable();
        (one_samples[15], nine_samples[15])
    }

    let path = std::env::var("OTZARIA_REAL_BENCH_INDEX")
        .expect("set OTZARIA_REAL_BENCH_INDEX to an isolated index copy");
    let mut engine = SearchEngine::new(&path);
    assert_eq!(engine.index.searchable_segment_ids().unwrap().len(), 1);
    let large_id = engine.index.searchable_segment_ids().unwrap()[0];
    let one_segment_searcher = engine.index_reader.searcher();

    engine.set_bulk_indexing(true).unwrap();
    let mut eight_segment_searcher = None;
    for id in 0..8 {
        add(
            &mut engine,
            9_000_000_000 + id,
            "שלום רבי אמר ישראל בראשית",
            &format!("/benchmark/new-{id}.txt"),
        );
        engine.commit().unwrap();
        if id == 6 {
            engine.index_reader.reload().unwrap();
            eight_segment_searcher = Some(engine.index_reader.searcher());
        }
    }
    assert_eq!(engine.index.searchable_segment_ids().unwrap().len(), 9);
    engine.index_reader.reload().unwrap();
    let nine_segment_searcher = engine.index_reader.searcher();
    let eight_segment_searcher = eight_segment_searcher.unwrap();
    let field = engine.schema.get_field("text").unwrap();
    for term in ["שלום", "רבי", "אמר", "ישראל", "בראשית"] {
        let query = TermQuery::new(
            Term::from_field_text(field, term),
            IndexRecordOption::WithFreqs,
        );
        for top_docs in [false, true] {
            let (one, nine) = paired_medians(
                &one_segment_searcher,
                &nine_segment_searcher,
                &query,
                top_docs,
            );
            let (eight, nine_again) = paired_medians(
                &eight_segment_searcher,
                &nine_segment_searcher,
                &query,
                top_docs,
            );
            let kind = if top_docs { "top10" } else { "count" };
            eprintln!("{kind} {term}: 1 segment {one:?}, 9 segments {nine:?}");
            eprintln!("{kind} {term}: 8 segments {eight:?}, 9 segments {nine_again:?}");
        }
    }
    engine.index.set_multithread_executor(4).unwrap();
    engine.index_reader = engine
        .index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()
        .unwrap();
    let nine_segment_multithread_searcher = engine.index_reader.searcher();
    for term in ["שלום", "רבי", "אמר", "ישראל", "בראשית"] {
        let query = TermQuery::new(
            Term::from_field_text(field, term),
            IndexRecordOption::WithFreqs,
        );
        for top_docs in [false, true] {
            let (single, multi) = paired_medians(
                &nine_segment_searcher,
                &nine_segment_multithread_searcher,
                &query,
                top_docs,
            );
            let kind = if top_docs { "top10" } else { "count" };
            eprintln!("{kind} {term}: 9 single {single:?}, 9 four-thread {multi:?}");
        }
    }
    drop(one_segment_searcher);
    drop(eight_segment_searcher);
    drop(nine_segment_searcher);
    drop(nine_segment_multithread_searcher);
    let started = Instant::now();
    engine.optimize().unwrap();
    eprintln!("optimize with tiny additions: {:?}", started.elapsed());
    let after = engine.index.searchable_segment_ids().unwrap();
    assert!(after.contains(&large_id));
    assert!(after.len() <= MAX_SEGMENTS_AFTER_OPTIMIZE);
}

#[test]
fn optimize_compacts_delete_heavy_segment() {
    let (mut engine, _dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    for i in 0..20u64 {
        add(&mut engine, i + 1, "בראשית ברא", "/books/gone.txt");
    }
    add(&mut engine, 100, "שלום עולם", "/books/kept.txt");
    engine.commit().unwrap();
    collapse_to_one_segment(&mut engine);
    let heavy_id = engine.index.searchable_segment_ids().unwrap()[0];

    engine
        .delete_documents_by_file_path("/books/gone.txt")
        .unwrap();
    engine.commit().unwrap();
    let metas = engine.index.searchable_segment_metas().unwrap();
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].num_deleted_docs(), 20);

    engine.optimize().unwrap();
    let metas = engine.index.searchable_segment_metas().unwrap();
    assert_eq!(metas.len(), 1);
    assert_ne!(metas[0].id(), heavy_id);
    assert_eq!(metas[0].num_deleted_docs(), 0);
    assert_eq!(metas[0].num_docs(), 1);
    assert_eq!(count_hits(&engine, "שלום"), 1);
}

#[test]
fn economy_indexing_swaps_writer_without_losing_pending_docs() {
    let (mut engine, _dir) = make_engine();
    // מסמך שטרם עבר commit חייב לשרוד את החלפת ה-writer.
    add(&mut engine, 1, "בראשית ברא", "/books/a.txt");
    engine.set_economy_indexing(true).unwrap();
    assert_eq!(engine.writer_heap_size, ECONOMY_WRITER_HEAP_SIZE);
    assert_eq!(engine.get_document_count(), 1);

    // גם בדרך חזרה, וגם כשמצב bulk פעיל — ה-writer החדש שומר NoMergePolicy.
    engine.set_bulk_indexing(true).unwrap();
    add(&mut engine, 2, "שלום עולם", "/books/a.txt");
    engine.set_economy_indexing(false).unwrap();
    assert_eq!(engine.writer_heap_size, DEFAULT_WRITER_HEAP_SIZE);
    engine.set_economy_indexing(false).unwrap();
    assert_eq!(engine.get_document_count(), 2);
}

#[test]
fn add_pdf_book_all_garbage_adds_nothing() {
    let (mut engine, _dir) = make_engine();
    let added = engine
        .add_pdf_book_placed(
            "סרוק".to_string(),
            "/root".to_string(),
            "C:/books/scanned.pdf".to_string(),
            1,
            DEFAULT_GENERATION_ORDER,
            vec![PdfPageInput {
                reference: "סרוק, עמוד 1".to_string(),
                text: "\n≡≡≡≡≡\n∴ ⊕ ⊗ ⊘ ∴".to_string(),
                page_index: 0,
            }],
            None,
        )
        .unwrap();
    assert_eq!(added, 0);
    engine.commit().unwrap();
    assert_eq!(engine.get_document_count(), 0);
}

#[test]
fn exact_search_raw_maqaf_query_becomes_phrase() {
    // רגרסיה: strip_nikud לפני הטוקניזציה מחק את המקף והדביק
    // "אשר־שמע" לטרם בודד ("אשרשמע") שאינו קיים באינדקס.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ברוך אֲשֶׁר־שָׁמַע את הדבר", "/books/a.txt");
    add(&mut engine, 2, "אשר לא שמע דבר", "/books/b.txt");
    engine.commit().unwrap();

    let ids: Vec<u64> = engine
        .search_exact(
            "אשר־שמע".to_string(),
            vec![],
            100,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    // phrase של שתי מילים סמוכות — תופס את 1, לא את 2 (מילים מרוחקות).
    assert_eq!(ids, vec![1]);
}

// ── Single-word alternation splitting (per-branch DFA) ───────────────

/// The exact pattern the generator builds for `משה` with "חלק ממילה" +
/// "שגיאות כתיב": 48 wildcard-wrapped branches, 806 chars. As one regex it
/// exceeded the upstream tantivy-fst 1 000-state DFA cap (the vendored
/// copy raises it to 8 192); each branch alone is tiny.
const BOTH_OPTIONS_PATTERN: &str = "(.{0,3}משה.{0,3}|.{0,3}מסה.{0,3}|.{0,2}משׁה.{0,2}|.{0,2}משׂה.{0,2}|.{0,3}משא.{0,3}|.{0,3}משע.{0,3}|.{0,3}משח.{0,3}|.{0,3}שמה.{0,3}|.{0,3}מהש.{0,3}|.{0,3}שה.{0,3}|.{0,3}מה.{0,3}|.{0,3}מש.{0,3}|.{0,2}ומשה.{0,2}|.{0,2}ימשה.{0,2}|.{0,2}אמשה.{0,2}|.{0,2}המשה.{0,2}|.{0,2}פמשה.{0,2}|.{0,2}למשה.{0,2}|.{0,2}ממשה.{0,2}|.{0,2}נמשה.{0,2}|.{0,2}במשה.{0,2}|.{0,2}כמשה.{0,2}|.{0,2}שמשה.{0,2}|.{0,2}תמשה.{0,2}|.{0,2}רמשה.{0,2}|.{0,2}משהו.{0,2}|.{0,2}משהי.{0,2}|.{0,2}משהא.{0,2}|.{0,2}משהה.{0,2}|.{0,2}משהפ.{0,2}|.{0,2}משהל.{0,2}|.{0,2}משהמ.{0,2}|.{0,2}משהנ.{0,2}|.{0,2}משהב.{0,2}|.{0,2}משהכ.{0,2}|.{0,2}משהש.{0,2}|.{0,2}משהת.{0,2}|.{0,2}משהר.{0,2}|.{0,2}מושה.{0,2}|.{0,2}מישה.{0,2}|.{0,2}מאשה.{0,2}|.{0,2}מהשה.{0,2}|.{0,2}מפשה.{0,2}|.{0,2}מלשה.{0,2}|.{0,2}מנשה.{0,2}|.{0,2}מבשה.{0,2}|.{0,2}מכשה.{0,2}|.{0,2}מששה.{0,2})";

#[test]
fn whole_pattern_compiles_under_vendored_state_limit_and_split_succeeds() {
    // Baseline for the vendored tantivy-fst patch: this combined pattern
    // could not compile as one DFA under the upstream 1 000-state cap
    // (the bug the per-branch split fixed). Under the vendored 8 192-state
    // cap it compiles again — the fact that unlocks the relaxed phrase
    // budgets, since a phrase word is compiled joined.
    assert!(
        tantivy_fst::Regex::new(BOTH_OPTIONS_PATTERN).is_ok(),
        "combined 48-branch pattern no longer compiles — did the vendored \
             tantivy-fst STATE_LIMIT patch get lost?"
    );
    // The split path answers the query either way.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ויאמר משה אל העם", "/books/a.txt");
    add(&mut engine, 2, "ספר בראשית", "/books/b.txt");
    engine.commit().unwrap();
    assert_eq!(search_ids(&mut engine, BOTH_OPTIONS_PATTERN), vec![1]);
}

#[test]
fn both_options_pattern_now_returns_results() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ויאמר משה אל העם", "/books/a.txt");
    add(&mut engine, 2, "ספר בראשית", "/books/b.txt");
    engine.commit().unwrap();

    // The exact pattern built for `משה` with "חלק ממילה" + "שגיאות כתיב".
    // As a single regex it exceeded the upstream 1000-state limit; the
    // per-branch split must find the document regardless of the cap.
    assert_eq!(search_ids(&mut engine, BOTH_OPTIONS_PATTERN), vec![1]);
}

#[test]
fn single_alternation_matches_same_as_combined_regex() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "משה", "/books/a.txt");
    add(&mut engine, 2, "מסה", "/books/b.txt");
    add(&mut engine, 3, "בראשית", "/books/c.txt");
    engine.commit().unwrap();

    // OR of two branches — finds both documents, not the third.
    let mut ids = search_ids(&mut engine, "(משה|מסה)");
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn split_matching_parity_with_whole_pattern() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "משה", "/books/a.txt");
    add(&mut engine, 2, "מסה", "/books/b.txt");
    add(&mut engine, 3, "בראשית", "/books/c.txt");
    engine.commit().unwrap();

    // Capturing and non-capturing wrappers split identically (R1).
    let mut capturing = search_ids(&mut engine, "(משה|מסה)");
    capturing.sort_unstable();
    let mut non_capturing = search_ids(&mut engine, "(?:משה|מסה)");
    non_capturing.sort_unstable();
    assert_eq!(capturing, vec![1, 2]);
    assert_eq!(capturing, non_capturing);

    // A leading empty branch (all-optional-letter word, R7) contributes
    // nothing: no indexed term is empty.
    assert_eq!(search_ids(&mut engine, "(?:|משה)"), vec![1]);

    // A nested (non-top-level) alternation still compiles whole and
    // matches the same set.
    let mut nested = search_ids(&mut engine, ".{0,1}(משה|מסה).{0,1}");
    nested.sort_unstable();
    assert_eq!(nested, vec![1, 2]);
}

#[test]
fn char_class_and_escape_branches_match_end_to_end() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספרים", "/books/a.txt");
    add(&mut engine, 2, "אב", "/books/b.txt");
    add(&mut engine, 3, "shalom", "/books/c.txt");
    engine.commit().unwrap();

    // Top-level alternation between two groups, with `|` inside a char
    // class in the second branch — the class pipe must not be split on.
    let mut ids = search_ids(&mut engine, "([א-ת]{2,4}(ים|ות|ה)?)|([א-ת]+[יו][ם|ן])");
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);

    // A bare char class containing `|` is a single literal pattern.
    assert_eq!(search_ids(&mut engine, "[ם|ן]"), Vec::<u64>::new());
}

#[test]
fn global_max_expansions_enforced_across_all_branches() {
    let (mut engine, _dir) = make_engine();
    // Terms matched by *different* branches, so only the shared union can
    // cross the cap — no single branch does.
    add(&mut engine, 1, "אאא", "/books/a.txt");
    add(&mut engine, 2, "בבב", "/books/b.txt");
    add(&mut engine, 3, "גגג", "/books/c.txt");
    engine.commit().unwrap();

    let run = |engine: &mut SearchEngine, max_expansions: u32| {
        engine.search(
            vec!["(אאא|בבב|גגג)".to_string()],
            vec!["/root".to_string()],
            100,
            0,
            0,
            max_expansions,
            ResultsOrder::Catalogue,
            None,
            None,
        )
    };
    // Union of 3 terms exceeds a cap of 2 — the single-word path degrades
    // instead of erroring, keeping the first branches' terms (branch
    // order is the priority contract).
    let ids: Vec<u64> = run(&mut engine, 2)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![1, 2]);
    // A cap of 3 fits the whole union.
    let ids: Vec<u64> = run(&mut engine, 3)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[test]
fn advanced_typo_partial_single_word_finds_document() {
    // End-to-end through the app path (search_advanced): the exact option
    // combination that used to blow the 1000-state DFA cap and silently
    // return nothing. Runs on the relaxed single-word budget.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ויאמר משה אל העם", "/books/a.txt");
    add(&mut engine, 2, "ספר בראשית", "/books/b.txt");
    engine.commit().unwrap();

    let mut options: HashMap<String, HashMap<String, bool>> = HashMap::new();
    options.insert(
        "משה_0".to_string(),
        HashMap::from([
            ("שגיאות כתיב".to_string(), true),
            ("חלק ממילה".to_string(), true),
        ]),
    );
    let ids: Vec<u64> = search_advanced_default(
        &engine,
        "משה".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        options,
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap()
    .into_iter()
    .map(|r| r.id)
    .collect();
    assert_eq!(ids, vec![1]);
}

#[test]
fn advanced_typo_only_single_word_covers_full_edit_distance() {
    // typo-only single word expands through a Levenshtein-1 automaton
    // scan (VARIATION_CEILING_RESEARCH §3.ג): full edit-distance-1
    // coverage, a superset of the historical literal variant list.
    // "ברכגה" — an insertion of ג, which is NOT in INSERTION_LETTERS — was
    // invisible to the sampled variants and must now match; a distance-2
    // word must not. (A word needs four letters for the full distance.)
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ברכה בראשית", "/books/a.txt");
    add(&mut engine, 2, "ברכגה אחר", "/books/b.txt");
    add(&mut engine, 3, "תורה צוה", "/books/c.txt");
    engine.commit().unwrap();

    let mut options: HashMap<String, HashMap<String, bool>> = HashMap::new();
    options.insert(
        "ברכה_0".to_string(),
        HashMap::from([("שגיאות כתיב".to_string(), true)]),
    );
    let mut ids: Vec<u64> = search_advanced_default(
        &engine,
        "ברכה".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        options,
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap()
    .into_iter()
    .map(|r| r.id)
    .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn invalid_branch_pattern_surfaces_error() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "משה", "/books/a.txt");
    engine.commit().unwrap();

    // One malformed branch must fail the search loudly, not silently
    // return nothing.
    let result = engine.search(
        vec!["משה|[".to_string()],
        vec!["/root".to_string()],
        100,
        0,
        0,
        100,
        ResultsOrder::Catalogue,
        None,
        None,
    );
    assert!(result.is_err());
}

// ── Snippet phrase-highlight filtering (results view ↔ search parity) ─

/// Counts highlight spans (the `<font color=red>` opening tag the default
/// `HighlightConfig` emits and the app's snippet parser expects).
fn highlight_count(text: &str) -> usize {
    text.matches("<font color=red>").count()
}

#[test]
fn advanced_phrase_snippet_highlights_only_adjacent_occurrence() {
    // The reported bug: searching the phrase "משה ואהרן" (no spacing) must
    // NOT light up a lone "משה" that is not followed by "ואהרן". tantivy's
    // term-based SnippetGenerator paints all three occurrences; the phrase
    // filter keeps only the adjacent pair.
    let (mut engine, _dir) = make_engine();
    add(
        &mut engine,
        1,
        "משה ואהרן אמרו שלום ואחר כך משה כהן הלך",
        "/books/a.txt",
    );
    engine.commit().unwrap();

    let results = search_advanced_default(
        &engine,
        "משה ואהרן".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    let text = &results[0].text;
    // Exactly the two words of the one adjacent phrase — not the lone משה.
    assert_eq!(highlight_count(text), 2, "snippet: {text}");
    assert!(text.contains("<font color=red>משה</font>"));
    assert!(text.contains("<font color=red>ואהרן</font>"));
    // The stray occurrence stays plain.
    assert!(text.contains("כך משה כהן"), "snippet: {text}");
}

#[test]
fn advanced_single_word_snippet_still_highlights_every_occurrence() {
    // A single word is not a phrase: every occurrence is a real hit and must
    // stay highlighted — the filter only constrains multi-word phrases.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "משה דיבר ואחר כך משה שתק", "/books/a.txt");
    engine.commit().unwrap();

    let results = search_advanced_default(
        &engine,
        "משה".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        highlight_count(&results[0].text),
        2,
        "snippet: {}",
        results[0].text
    );
}

#[test]
fn exact_phrase_snippet_highlights_only_adjacent_occurrence() {
    // The exact (PhraseQuery) path has the same term-based over-painting; a
    // strict-adjacency (all-zero gaps) filter drops the lone occurrence too.
    let (mut engine, _dir) = make_engine();
    add(
        &mut engine,
        1,
        "משה ואהרן אמרו שלום ואחר כך משה כהן הלך",
        "/books/a.txt",
    );
    engine.commit().unwrap();

    let results = engine
        .search_exact(
            "משה ואהרן".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    let text = &results[0].text;
    assert_eq!(highlight_count(text), 2, "snippet: {text}");
    assert!(text.contains("כך משה כהן"), "snippet: {text}");
}

#[test]
fn advanced_phrase_custom_spacing_gap_is_highlighted_not_dropped() {
    // `distance` is 0 but `custom_spacing` permits one intermediate word, so
    // the search matches "משה <word> ואהרן". The filter's gap allowance must
    // come from the resolved gaps (= the custom spacing), NOT the raw
    // `distance`: with `distance` it would find no occurrence, fall back to
    // the plain term highlight, and re-paint the lone trailing "משה" (3
    // spans). With the gaps it paints exactly the gapped phrase (2 spans).
    let (mut engine, _dir) = make_engine();
    add(
        &mut engine,
        1,
        "משה רבנו ואהרן אמר ואחר כך משה לבדו הלך",
        "/books/a.txt",
    );
    engine.commit().unwrap();

    let results = search_advanced_default(
        &engine,
        "משה ואהרן".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,                                                     // distance 0 …
        HashMap::from([("0-1".to_string(), "1".to_string())]), // … but spacing allows 1
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    let text = &results[0].text;
    assert_eq!(highlight_count(text), 2, "snippet: {text}");
    assert!(text.contains("<font color=red>משה</font>"));
    assert!(text.contains("<font color=red>ואהרן</font>"));
    // The stray trailing occurrence stays plain.
    assert!(text.contains("כך משה לבדו"), "snippet: {text}");
}

// ── Phrase expansion degrade path (TermListPhraseQuery) ───────────────

/// Ten distinct suffix letters so wildcard patterns fan out to ten index
/// terms per position without leaving the Hebrew tokenizer's alphabet.
const FAN_LETTERS: [&str; 10] = ["א", "ב", "ג", "ד", "ה", "ו", "ז", "ח", "ט", "י"];

fn make_fanout_phrase_engine() -> (SearchEngine, TempDir) {
    let (mut engine, dir) = make_engine();
    for (i, letter) in FAN_LETTERS.iter().enumerate() {
        add(
            &mut engine,
            i as u64 + 1,
            &format!("עמוד{letter} שער{letter}"),
            "/books/a.txt",
        );
    }
    // Reversed order — the phrase must reject it on every path.
    add(&mut engine, 50, "שערא עמודא", "/books/b.txt");
    // One intermediate word — admitted only when slop allows it.
    add(&mut engine, 60, "עמודב מלה שערב", "/books/c.txt");
    engine.commit().unwrap();
    (engine, dir)
}

fn phrase_ids(engine: &SearchEngine, slop: u32, max_expansions: u32) -> Vec<u64> {
    let mut ids: Vec<u64> = engine
        .search(
            vec!["עמוד.*".to_string(), "שער.*".to_string()],
            vec!["/root".to_string()],
            100,
            0,
            slop,
            max_expansions,
            ResultsOrder::Catalogue,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn phrase_over_expansion_ceiling_degrades_instead_of_erroring() {
    // Each position matches 10 index terms, so max_expansions = 5 puts
    // the query over tantivy's cumulative ceiling — historically a hard
    // `InvalidArgument("Phrase query exceeded max expansions")`. The
    // degrade path must serve the complete result set instead, with the
    // same in-order adjacency semantics as the exact path.
    let (engine, _dir) = make_fanout_phrase_engine();

    let ids = phrase_ids(&engine, 0, 5);
    assert_eq!(ids, (1..=10).collect::<Vec<u64>>());

    // Only the cumulative ceiling was outgrown — no per-position budget
    // truncated, so the count must not report partial results.
    let status = engine
        .count_with_status(
            vec!["עמוד.*".to_string(), "שער.*".to_string()],
            &["/root".to_string()],
            0,
            5,
        )
        .unwrap();
    assert_eq!(status.count, 10);
    assert!(!status.truncated);
}

#[test]
fn phrase_degrade_path_matches_exact_path_results() {
    // The same query under a roomy ceiling runs the historical
    // RegexPhraseQuery engine; both engines must agree on every doc,
    // at slop 0 and with an allowance.
    let (engine, _dir) = make_fanout_phrase_engine();

    assert_eq!(phrase_ids(&engine, 0, 5), phrase_ids(&engine, 0, 1_000));
    let with_gap = phrase_ids(&engine, 1, 5);
    assert!(with_gap.contains(&60), "gap doc admitted under slop 1");
    assert_eq!(with_gap, phrase_ids(&engine, 1, 1_000));
}

#[test]
fn phrase_expansion_limit_is_per_segment_not_global_union() {
    // Tantivy resets the cumulative phrase-expansion counter for every
    // segment. Each segment below has four expansions (three `עמוד.*`
    // terms plus one `שער.*` term), so max=5 is valid even though the
    // global union contains seven distinct terms. Counting the union
    // would wrongly select the flat-scoring degrade path.
    let (mut engine, _dir) = make_engine();
    disable_auto_merge(&mut engine);
    for (id, letter) in [(1, "א"), (2, "ב"), (3, "ג")] {
        add(
            &mut engine,
            id,
            &format!("עמוד{letter} שער"),
            "/books/a.txt",
        );
    }
    engine.commit().unwrap();
    for (id, letter) in [(4, "ד"), (5, "ה"), (6, "ו")] {
        add(
            &mut engine,
            id,
            &format!("עמוד{letter} שער"),
            "/books/b.txt",
        );
    }
    engine.commit().unwrap();

    let text_field = engine.schema.get_field("text").unwrap();
    let phrase = RegexPhraseQuery::new(text_field, vec!["עמוד.*".into(), "שער".into()]);
    assert!(!engine
        .phrase_exceeds_max_expansions(phrase.regexes().unwrap(), text_field, 5)
        .unwrap());
    assert_eq!(phrase_ids(&engine, 0, 5), (1..=6).collect::<Vec<u64>>());
}

#[test]
fn phrase_degrade_path_keeps_word_order() {
    let (engine, _dir) = make_fanout_phrase_engine();
    // Even with a generous allowance the reversed doc must stay out on
    // the degrade path (the AND driver alone would admit it).
    let ids = phrase_ids(&engine, 2, 5);
    assert!(!ids.contains(&50), "reversed order must not match");
}

#[test]
fn phrase_position_over_budget_truncates_with_flag() {
    // One position fanning out past PHRASE_POSITION_MAX_EXPANSIONS must
    // degrade (truncate from the back of the dictionary order) and
    // surface `truncated`, while the lexicographically-early target term
    // survives and keeps matching.
    let (mut engine, _dir) = make_engine();
    // The truncation order is segment-major. A multi-thread writer can
    // put the earliest term only in a later segment, after the budget is
    // exhausted; its partitioning is not deterministic. Keep this test
    // of dictionary-prefix survival in one explicit segment.
    engine
        .take_writer()
        .unwrap()
        .wait_merging_threads()
        .unwrap();
    engine.index_writer = Some(engine.index.writer_with_num_threads(1, 50_000_000).unwrap());
    disable_auto_merge(&mut engine);

    let letters = [
        "א", "ב", "ג", "ד", "ה", "ו", "ז", "ח", "ט", "י", "כ", "ל", "מ", "נ", "ס", "ע", "פ", "צ",
        "ק", "ר", "ש", "ת",
    ];
    // 22³ = 10 648 distinct terms under the "מלה" prefix — comfortably
    // past the 8 192 per-position ceiling. Spread over a few docs to
    // keep each document small.
    let mut words: Vec<String> = Vec::with_capacity(letters.len().pow(3));
    for a in letters {
        for b in letters {
            for c in letters {
                words.push(format!("מלה{a}{b}{c}"));
            }
        }
    }
    for (chunk_index, chunk) in words.chunks(1_000).enumerate() {
        add(
            &mut engine,
            1_000 + chunk_index as u64,
            &chunk.join(" "),
            "/books/filler.txt",
        );
    }
    add(&mut engine, 1, "ראשא מלהאאא", "/books/a.txt");
    engine.commit().unwrap();
    assert_eq!(engine.get_segment_count().unwrap(), 1);

    let status = engine
        .count_with_status(
            vec!["ראש.*".to_string(), "מלה.*".to_string()],
            &["/root".to_string()],
            0,
            // Force the fallback on every normal-sized segment; the
            // second position then exceeds its own 8,192-term material-
            // ization ceiling across the index and must surface partial
            // results. Passing 8,192 here would correctly stay exact
            // because Tantivy applies that limit per segment.
            100,
        )
        .unwrap();
    assert!(status.truncated, "per-position overflow must surface");
    assert_eq!(status.count, 1, "the surviving target phrase still matches");
}

// ── Per-pair gap enforcement (GapVerifiedPhraseQuery) ─────────────────

#[test]
fn advanced_custom_spacing_is_enforced_per_pair() {
    // spacing = {0-1: 2, 1-2: 0}: up to two words between ויאמר and אל,
    // but אל and משה must be adjacent. Historically the engine collapsed
    // this to one global slop, so doc 2 — where the allowance is "spent"
    // in the wrong gap — also matched.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ויאמר ה' צבאות אל משה", "/books/a.txt");
    add(&mut engine, 2, "ויאמר אל העם ואל משה", "/books/b.txt");
    engine.commit().unwrap();

    let spacing = HashMap::from([
        ("0-1".to_string(), "2".to_string()),
        ("1-2".to_string(), "0".to_string()),
    ]);
    let ids: Vec<u64> = search_advanced_default(
        &engine,
        "ויאמר אל משה".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        spacing.clone(),
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap()
    .into_iter()
    .map(|r| r.id)
    .collect();
    assert_eq!(ids, vec![1]);

    // The counting path runs through the same verified query.
    let count = count_advanced_default(
        &engine,
        "ויאמר אל משה".to_string(),
        vec!["/root".to_string()],
        0,
        spacing,
        HashMap::new(),
        HashMap::new(),
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn advanced_distance_allows_full_gap_in_every_pair() {
    // distance = 2 means "up to two words between EACH adjacent pair".
    // tantivy's slop is a cumulative budget, so passing the raw distance
    // used to reject a match that uses its allowance in both gaps at once.
    let (mut engine, _dir) = make_engine();
    add(
        &mut engine,
        1,
        "ויאמר ה' צבאות אל בני ישראל משה",
        "/books/a.txt",
    );
    // One gap over the allowance must still be rejected.
    add(
        &mut engine,
        2,
        "ויאמר אל דוד המלך איש הבינים משה",
        "/books/b.txt",
    );
    engine.commit().unwrap();

    let ids: Vec<u64> = search_advanced_default(
        &engine,
        "ויאמר אל משה".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        2,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap()
    .into_iter()
    .map(|r| r.id)
    .collect();
    assert_eq!(ids, vec![1]);
}

#[test]
fn advanced_phrase_requires_query_word_order() {
    // tantivy's sloppy phrase matches unordered (positions compare by
    // abs_diff); the gap verifier restores the in-order semantics every
    // comment, highlight filter, and display pattern already promise.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ויאמר להם משה", "/books/a.txt");
    // Reversed within the slop budget: tantivy's phrase scorer accepts
    // it, only the gap verifier rejects it.
    add(&mut engine, 2, "אמר משה ויאמר", "/books/b.txt");
    engine.commit().unwrap();

    let ids: Vec<u64> = search_advanced_default(
        &engine,
        "ויאמר משה".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        2,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap()
    .into_iter()
    .map(|r| r.id)
    .collect();
    assert_eq!(ids, vec![1]);
}

#[test]
fn advanced_per_pair_gap_verifies_across_repeated_words() {
    // The feasibility sweep must consider EVERY chain start, not just the
    // earliest: here the first "אל" satisfies pair 0 but only the second
    // one can reach "משה" within pair 1's allowance.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ויאמר אל העם ושוב אל משה", "/books/a.txt");
    engine.commit().unwrap();

    let spacing = HashMap::from([
        ("0-1".to_string(), "3".to_string()),
        ("1-2".to_string(), "0".to_string()),
    ]);
    let ids: Vec<u64> = search_advanced_default(
        &engine,
        "ויאמר אל משה".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        spacing,
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap()
    .into_iter()
    .map(|r| r.id)
    .collect();
    assert_eq!(ids, vec![1]);
}

#[test]
fn advanced_per_pair_spacing_composes_with_word_options() {
    // Per-pair enforcement must hold when a word expands through an
    // option (grammatical prefixes): "ולמשה" matches the expanded word
    // pattern, and the pair allowances still gate the match.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ויאמר שוב אל ולמשה", "/books/a.txt");
    add(&mut engine, 2, "ויאמר אל העם ולמשה", "/books/b.txt");
    engine.commit().unwrap();

    let options: HashMap<String, HashMap<String, bool>> = HashMap::from([(
        "משה_2".to_string(),
        HashMap::from([("קידומות דקדוקיות".to_string(), true)]),
    )]);
    let spacing = HashMap::from([
        ("0-1".to_string(), "1".to_string()),
        ("1-2".to_string(), "0".to_string()),
    ]);
    let ids: Vec<u64> = search_advanced_default(
        &engine,
        "ויאמר אל משה".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        spacing,
        HashMap::new(),
        options,
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap()
    .into_iter()
    .map(|r| r.id)
    .collect();
    assert_eq!(ids, vec![1]);
}

// ── Index-aware display highlight (parity with search, R5) ───────────

/// Charwise display form of a nikud-free term, as the Dart layer receives
/// it (each Hebrew letter may carry attached marks in displayed text, and
/// between letters optional geresh/gershayim — the quote-free twin token).
fn charwise(term: &str) -> String {
    let mut out = String::new();
    for (i, c) in term.chars().enumerate() {
        if i > 0 {
            out.push_str(crate::display_highlight::OPTIONAL_QUOTES);
        }
        out.push(c);
        out.push_str(crate::display_highlight::ATTACHED_MARKS_CLASS);
    }
    out
}

fn typo_options(word: &str) -> HashMap<String, HashMap<String, bool>> {
    HashMap::from([(
        format!("{word}_0"),
        HashMap::from([("שגיאות כתיב".to_string(), true)]),
    )])
}

#[test]
fn index_highlight_paints_typo_matched_term() {
    let (mut engine, _dir) = make_engine();
    // The document is findable only via the typo variant מסה — the
    // query-shape pattern (which knows only משה + spelling) would leave
    // it with no highlight at all.
    add(&mut engine, 1, "ויקח מסה גדולה", "/books/a.txt");
    engine.commit().unwrap();

    let hl = engine
        .generate_index_highlight_pattern(
            "משה".to_string(),
            0,
            HashMap::new(),
            HashMap::new(),
            typo_options("משה"),
            None,
            None,
        )
        .unwrap()
        .expect("pattern");
    // Only מסה exists in this index, so it is the single branch.
    assert_eq!(hl.combined_pattern, charwise("מסה"));
    assert_eq!(hl.word_boundary_eligible, vec![true]);
}

#[test]
fn index_highlight_partial_word_uses_query_shape_substring() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "הספרים הקדושים", "/books/a.txt");
    add(&mut engine, 2, "ספר תורה", "/books/b.txt");
    engine.commit().unwrap();

    let options = HashMap::from([(
        "ספר_0".to_string(),
        HashMap::from([("חלק ממילה".to_string(), true)]),
    )]);
    let hl = engine
        .generate_index_highlight_pattern(
            "ספר".to_string(),
            0,
            HashMap::new(),
            HashMap::new(),
            options.clone(),
            None,
            None,
        )
        .unwrap()
        .expect("pattern");

    // חלק-ממילה מדגיש את התת-מחרוזת שהוקלדה (query-shape) עם כיסוי מלא —
    // לא את המילים המנוטות השלמות מהאינדקס (שחסומות בתקציב ומחמיצות
    // מילים גלויות). ראה display_highlight::build_display_highlight_from_terms.
    let query_shape = generate_highlight_pattern(
        "ספר".to_string(),
        0,
        HashMap::new(),
        HashMap::new(),
        options,
        None,
    )
    .expect("pattern");
    assert_eq!(hl.combined_pattern, query_shape.combined_pattern);
    assert_eq!(hl.word_boundary_eligible, vec![false]);
}

#[test]
fn index_highlight_falls_back_to_query_shape_when_nothing_matches() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "בראשית ברא", "/books/a.txt");
    engine.commit().unwrap();

    let from_index = engine
        .generate_index_highlight_pattern(
            "משה".to_string(),
            0,
            HashMap::new(),
            HashMap::new(),
            typo_options("משה"),
            None,
            None,
        )
        .unwrap()
        .expect("pattern");
    let query_shape = generate_highlight_pattern(
        "משה".to_string(),
        0,
        HashMap::new(),
        HashMap::new(),
        typo_options("משה"),
        None,
    )
    .expect("pattern");
    // Nothing in the index matches → never worse than the pure-string
    // pattern the app used until now.
    assert_eq!(from_index.combined_pattern, query_shape.combined_pattern);
    assert_eq!(
        from_index.word_boundary_eligible,
        query_shape.word_boundary_eligible
    );
}

#[test]
fn index_highlight_multi_word_paints_each_words_variants() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ויאמר מסה אל העם", "/books/a.txt");
    engine.commit().unwrap();

    let options = HashMap::from([(
        "משה_1".to_string(),
        HashMap::from([("שגיאות כתיב".to_string(), true)]),
    )]);
    let hl = engine
        .generate_index_highlight_pattern(
            "ויאמר משה".to_string(),
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            None,
            None,
        )
        .unwrap()
        .expect("pattern");

    assert_eq!(hl.word_patterns.len(), 2);
    assert_eq!(hl.word_patterns[0], charwise("ויאמר"));
    assert_eq!(hl.word_patterns[1], charwise("מסה"));
    assert_eq!(hl.combined_pattern, "(?!)");
    assert_eq!(
        hl.matcher
            .unwrap()
            .find_matches("ויאמר מסה אל העם".into(), vec![])
            .len(),
        1
    );
}

#[test]
fn index_fuzzy_highlight_paints_edit_distance_matches() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ויקח מסה גדולה", "/books/a.txt");
    engine.commit().unwrap();

    let hl = engine
        .generate_index_fuzzy_highlight_pattern("משה".to_string(), 1, None)
        .unwrap()
        .expect("pattern");
    // The edit-distance-1 match from the index plus the literal token.
    assert!(hl.combined_pattern.contains(&charwise("מסה")));
    assert!(hl.combined_pattern.contains(&charwise("משה")));

    // Distance above the FuzzyTermQuery limit is rejected loudly.
    assert!(engine
        .generate_index_fuzzy_highlight_pattern("משה".to_string(), 3, None)
        .is_err());
}

#[test]
fn index_highlight_plain_query_matches_exact_token() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    engine.commit().unwrap();

    // No options at all (the plain path of prepare_advanced_query).
    let hl = engine
        .generate_index_highlight_pattern(
            "שלום".to_string(),
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            None,
            None,
        )
        .unwrap()
        .expect("pattern");
    assert_eq!(hl.combined_pattern, charwise("שלום"));
}

/// Otzaria/otzaria#1926: affixes with typos must still paint the forms in
/// the text when the index holds thousands of affixed forms of the word.
#[test]
fn index_highlight_affix_typo_paints_found_variants_among_many_forms() {
    let (mut engine, _dir) = make_engine();
    let line_a = "ימים אחר הזמן שייעד להביא כלומר עתה ידעתי כי מצוא אשר צוה";
    let line_b = "אבל כשאין עשרה לא. וכל זמן שאין שלשה מצוה להם ליחלק";
    let letters: Vec<char> = "אבגדהוזחטיכלמנסעפצקרשת".chars().collect();
    let mut filler = Vec::new();
    for a in &letters {
        for b in &letters {
            filler.push(format!("{a}{b}מצוה מצוה{a}{b} {a}{b}זמן"));
        }
    }
    add(&mut engine, 1, line_a, "/books/a.txt");
    add(&mut engine, 2, line_b, "/books/b.txt");
    add(&mut engine, 3, &filler.join(" "), "/books/filler.txt");
    engine.commit().unwrap();

    let word_options: HashMap<String, bool> = ["קידומות", "סיומות", "שגיאות כתיב", "כתיב מלא/חסר"]
        .iter()
        .map(|o| (o.to_string(), true))
        .collect();
    let options = HashMap::from([
        ("זימון_0".to_string(), word_options.clone()),
        ("מצוה_1".to_string(), word_options),
    ]);
    let matcher = engine
        .generate_index_highlight_pattern(
            "זימון מצוה".to_string(),
            10,
            HashMap::new(),
            HashMap::new(),
            options,
            None,
            None,
        )
        .unwrap()
        .expect("pattern")
        .matcher
        .expect("matcher");

    let painted = |line: &str| -> Vec<String> {
        let units: Vec<u16> = line.encode_utf16().collect();
        matcher
            .find_matches(line.to_string(), vec![])
            .iter()
            .flat_map(|m| {
                m.word_ranges.iter().map(|r| {
                    String::from_utf16_lossy(
                        &units[(m.start + r.start) as usize..(m.start + r.end) as usize],
                    )
                })
            })
            .collect()
    };
    assert_eq!(painted(line_b), vec!["זמן", "מצוה"]);
    // Word forms paint their root, as with affixes alone; typo terms paint whole.
    assert_eq!(painted(line_a), vec!["זמן", "מצוא"]);
}

/// A one-sided expansion must keep painting a typo variant its own
/// branches cannot cover (`ברכתא` under prefixes only).
#[test]
fn index_highlight_one_sided_affix_typo_paints_variant() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ברכתא תורה", "/books/a.txt");
    engine.commit().unwrap();

    let options = HashMap::from([(
        "ברכת_0".to_string(),
        HashMap::from([
            ("קידומות".to_string(), true),
            ("שגיאות כתיב".to_string(), true),
        ]),
    )]);
    let matcher = engine
        .generate_index_highlight_pattern(
            "ברכת תורה".to_string(),
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            None,
            None,
        )
        .unwrap()
        .expect("pattern")
        .matcher
        .expect("matcher");
    let found = matcher.find_matches("ברכתא תורה".to_string(), vec![]);
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].start, found[0].end), (0, 10));
}

#[test]
fn test_count_by_book_basic() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    add(&mut engine, 2, "שלום רב", "/books/a.txt");
    add(&mut engine, 3, "שלום חבר", "/books/b.txt");
    engine.commit().unwrap();

    let counts = engine
        .count_by_book(vec!["שלום".to_string()], vec!["/root".to_string()], 0, 100)
        .unwrap();

    assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
    assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
    assert_eq!(counts.len(), 2);
}

#[test]
fn test_count_by_book_empty_result() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    engine.commit().unwrap();

    let counts = engine
        .count_by_book(vec!["ביי".to_string()], vec!["/root".to_string()], 0, 100)
        .unwrap();

    assert!(counts.is_empty());
}

#[test]
fn test_count_by_book_no_cross_contamination() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    add(&mut engine, 2, "שלום ביי", "/books/b.txt");
    engine.commit().unwrap();

    let counts = engine
        .count_by_book(vec!["עולם".to_string()], vec!["/root".to_string()], 0, 100)
        .unwrap();

    assert_eq!(counts.get("/books/a.txt").copied(), Some(1));
    assert_eq!(counts.get("/books/b.txt"), None);
}

#[test]
fn test_count_by_book_multi_segment() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    engine.commit().unwrap();

    add(&mut engine, 2, "שלום רב", "/books/a.txt");
    add(&mut engine, 3, "שלום חבר", "/books/b.txt");
    engine.commit().unwrap();

    let counts = engine
        .count_by_book(vec!["שלום".to_string()], vec!["/root".to_string()], 0, 100)
        .unwrap();

    assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
    assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
    assert_eq!(counts.len(), 2);
}

/// Thousands of paths span many dictionary blocks, and only some of them match:
/// the batched ordinal-to-path pass must pair every path with its own count.
#[test]
fn test_count_by_book_many_paths_sparse_hits() {
    let (mut engine, _dir) = make_engine();
    let mut expected: HashMap<String, u32> = HashMap::new();
    let mut id = 0;
    for book in 0..3_000u32 {
        let path = format!("/books/{:04}/ספר מספר {book} בספרייה.txt", book * 7 % 3_000);
        for line in 0..(book % 4) {
            id += 1;
            let text = if (book + line) % 3 == 0 {
                "שלום עולם"
            } else {
                "ביי"
            };
            add(&mut engine, id, text, &path);
            if text.starts_with("שלום") {
                *expected.entry(path.clone()).or_insert(0) += 1;
            }
        }
        if book == 1_500 {
            engine.commit().unwrap();
        }
    }
    engine.commit().unwrap();

    let counts = engine
        .count_by_book(vec!["שלום".to_string()], vec!["/root".to_string()], 0, 100)
        .unwrap();

    assert!(expected.len() > 1_000);
    assert_eq!(counts, expected);
}

/// Segments are collected on the engine's own pool: concurrent callers, and
/// callers running inside rayon's global pool, neither deadlock nor diverge.
#[test]
fn multithread_search_executor_serves_concurrent_and_nested_searches() {
    use rayon::prelude::*;
    let (mut engine, _dir) = make_engine();
    let mut id = 0;
    for segment in 0..4 {
        for line in 0..50 {
            id += 1;
            let text = if line % 2 == 0 {
                "שלום עולם"
            } else {
                "ביי"
            };
            add(&mut engine, id, text, &format!("/books/{segment}.txt"));
        }
        engine.commit().unwrap();
    }
    assert_eq!(
        matches!(
            engine.index.search_executor(),
            tantivy::Executor::ThreadPool(_)
        ),
        search_executor_threads() > 1
    );
    let by_book = || {
        engine
            .count_by_book(vec!["שלום".to_string()], vec!["/root".to_string()], 0, 100)
            .unwrap()
    };
    let expected = by_book();
    assert_eq!(expected.values().sum::<u32>(), 100);

    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..20 {
                    assert_eq!(by_book(), expected);
                }
            });
        }
    });
    (0..64)
        .into_par_iter()
        .for_each(|_| assert_eq!(by_book(), expected));
}

/// The first page ranks the following ones, so they and the counts are served
/// from the page cache; a commit drops it.
#[test]
fn later_pages_and_counts_come_from_the_page_cache() {
    let (mut engine, _dir) = make_engine();
    for id in 1..=30u64 {
        let text = if id % 3 == 0 {
            "שלום עולם"
        } else {
            "שלום"
        };
        add(&mut engine, id, text, &format!("/books/{}.txt", id % 4));
        if id == 15 {
            engine.commit().unwrap();
        }
    }
    engine.commit().unwrap();
    let page = |engine: &SearchEngine, offset| {
        let page = engine
            .search_and_count_exact(
                "שלום".to_string(),
                vec![],
                4,
                offset,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap();
        let ids: Vec<u64> = page.results.iter().map(|r| r.id).collect();
        (page.total_count, ids)
    };
    let by_book = |engine: &SearchEngine| {
        engine
            .count_by_book_exact("שלום".to_string(), vec![], false, false, None)
            .unwrap()
    };

    assert_eq!(page(&engine, 0), (30, vec![1, 2, 3, 4]));
    assert_eq!(engine.page_cache.len(), 1);
    let cached: Vec<_> = (1..8).map(|n| page(&engine, n * 4)).collect();
    let books = by_book(&engine);
    assert_eq!(by_book(&engine), books);
    assert_eq!(engine.page_cache.len(), 1);
    engine.page_cache.clear();
    let fresh: Vec<_> = (1..8).map(|n| page(&engine, n * 4)).collect();
    assert_eq!(cached, fresh);
    assert_eq!(cached[6], (30, vec![29, 30]));
    engine.page_cache.clear();
    assert_eq!(by_book(&engine), books);
    assert_eq!(books.values().sum::<u32>(), 30);

    add(&mut engine, 31, "שלום", "/books/new.txt");
    engine.commit().unwrap();
    assert_eq!(engine.page_cache.len(), 0);
    assert_eq!(page(&engine, 28), (31, vec![29, 30, 31]));
}

/// A reload that changed no segment (the reader's own watcher reloads after every
/// commit) keeps the cached pages.
#[test]
fn a_reload_without_changes_keeps_the_page_cache() {
    let (mut engine, _dir) = make_engine();
    for id in 1..=5u64 {
        add(&mut engine, id, "שלום", "/books/a.txt");
    }
    engine.commit().unwrap();
    let search = |engine: &SearchEngine| {
        engine
            .search_and_count_exact(
                "שלום".to_string(),
                vec![],
                2,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap()
            .total_count
    };
    assert_eq!(search(&engine), 5);
    engine.index_reader.reload().unwrap();
    assert_eq!(search(&engine), 5);
    assert_eq!(engine.page_cache.len(), 1);
}

/// A loader that keeps each segment's columns reads every document as one read alone.
#[test]
fn a_document_loader_reads_each_document_as_a_single_read_does() {
    let (mut engine, _dir) = make_engine();
    for id in 1..=6u64 {
        add(&mut engine, id, "שלום", &format!("/books/{}.txt", id % 2));
        if id == 3 {
            engine.commit().unwrap();
        }
    }
    add(&mut engine, 7, "סרוק", "/books/scan.pdf");
    engine.commit().unwrap();
    let searcher = engine.index_reader.searcher();
    let addresses: Vec<DocAddress> = searcher
        .segment_readers()
        .iter()
        .enumerate()
        .flat_map(|(ord, reader)| {
            (0..reader.max_doc()).map(move |doc| DocAddress::new(ord as u32, doc))
        })
        .collect();
    assert!(searcher.segment_readers().len() > 1);
    let mut loader = DocumentLoader::new(&searcher);
    for &address in addresses.iter().chain(addresses.iter().rev()) {
        let alone = load_document(&searcher, address).unwrap();
        let loaded = loader.load(address).unwrap();
        assert_eq!(
            loaded.to_json(&engine.schema),
            alone.to_json(&engine.schema)
        );
        assert!(loaded
            .get_first(engine.schema.get_field("title").unwrap())
            .is_some());
    }
}

/// A fuzzy query's cache key names its word: two fuzzy searches never share a page.
#[test]
fn fuzzy_searches_for_different_words_do_not_share_a_cached_page() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/a.txt");
    add(&mut engine, 2, "ירושלים", "/books/b.txt");
    engine.commit().unwrap();
    let ids = |word: &str| -> Vec<u64> {
        engine
            .search_and_count_fuzzy(
                word.to_string(),
                vec![],
                10,
                0,
                1,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap()
            .results
            .iter()
            .map(|r| r.id)
            .collect()
    };
    assert_eq!(ids("שלום"), vec![1]);
    assert_eq!(ids("ירושלים"), vec![2]);
}

/// The index-based display pattern is computed once per index snapshot: equal
/// requests share it, and a commit brings in the variants it added.
#[test]
fn index_highlight_pattern_is_cached_per_snapshot() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ירושלים עיר הקודש", "/books/a.txt");
    engine.commit().unwrap();
    let typo: HashMap<String, HashMap<String, bool>> = [(
        "ירושלים_0".to_string(),
        [("שגיאות כתיב".to_string(), true)].into_iter().collect(),
    )]
    .into_iter()
    .collect();
    let pattern = |engine: &SearchEngine| {
        engine
            .generate_index_highlight_pattern(
                "ירושלים".to_string(),
                0,
                HashMap::new(),
                HashMap::new(),
                typo.clone(),
                None,
                None,
            )
            .unwrap()
            .expect("pattern")
            .word_patterns
    };
    let first = pattern(&engine);
    assert_eq!(pattern(&engine), first);
    engine.highlight_cache.clear();
    assert_eq!(pattern(&engine), first);

    add(&mut engine, 2, "ירושלם עיר הקודש", "/books/a.txt");
    engine.commit().unwrap();
    assert_ne!(pattern(&engine), first);
}

/// A cancelled request id stops its counts, and leaves nothing behind for
/// the same search without it; other ids and untagged calls run.
#[test]
fn cancelled_lexical_request_fails_its_counts_only() {
    let (mut engine, _dir) = make_engine();
    for id in 1..=20u64 {
        add(
            &mut engine,
            id,
            "שלום עולם",
            &format!("/books/{}.txt", id % 3),
        );
    }
    engine.commit().unwrap();
    let by_book =
        |id: Option<u32>| engine.count_by_book_exact("שלום".to_string(), vec![], false, false, id);
    let count = |id: Option<u32>| engine.count_exact("שלום".to_string(), vec![], false, false, id);

    engine.cancel_lexical_search(9);
    for err in [by_book(Some(9)).unwrap_err(), count(Some(9)).unwrap_err()] {
        assert!(LexicalSearchCancelled::is(&err));
        assert_eq!(err.to_string(), LexicalSearchCancelled::MESSAGE);
    }
    let expected = by_book(None).unwrap();
    assert_eq!(expected.values().sum::<u32>(), 20);
    assert_eq!(by_book(Some(10)).unwrap(), expected);
    assert_eq!(count(Some(10)).unwrap(), 20);
    assert_eq!(count(None).unwrap(), 20);
}

#[test]
fn test_count_documents_by_file_path_empty_index() {
    let (engine, _dir) = make_engine();
    assert!(engine.count_documents_by_file_path().unwrap().is_empty());
    assert!(engine.get_indexed_file_paths().unwrap().is_empty());
}

#[test]
fn test_count_documents_by_file_path_basic() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    add(&mut engine, 2, "שלום רב", "/books/a.txt");
    add(&mut engine, 3, "שלום חבר", "/books/b.txt");
    engine.commit().unwrap();

    let counts = engine.count_documents_by_file_path().unwrap();
    assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
    assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
    assert_eq!(counts.len(), 2);

    let mut paths = engine.get_indexed_file_paths().unwrap();
    paths.sort();
    assert_eq!(paths, vec!["/books/a.txt", "/books/b.txt"]);
}

#[test]
fn test_count_documents_by_file_path_respects_deletes() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    add(&mut engine, 2, "שלום רב", "/books/a.txt");
    add(&mut engine, 3, "שלום חבר", "/books/b.txt");
    engine.commit().unwrap();

    engine.delete_document_by_id(1).unwrap();
    engine.delete_document_by_id(3).unwrap();
    engine.commit().unwrap();

    let counts = engine.count_documents_by_file_path().unwrap();
    assert_eq!(counts.get("/books/a.txt").copied(), Some(1));
    assert_eq!(
        counts.get("/books/b.txt"),
        None,
        "a book whose documents were all deleted must not be reported"
    );

    let paths = engine.get_indexed_file_paths().unwrap();
    assert_eq!(paths, vec!["/books/a.txt"]);
}

#[test]
fn test_count_documents_by_file_path_multi_segment() {
    let (mut engine, _dir) = make_engine();
    disable_auto_merge(&mut engine);

    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    engine.commit().unwrap();
    add(&mut engine, 2, "שלום רב", "/books/a.txt");
    add(&mut engine, 3, "שלום חבר", "/books/b.txt");
    engine.commit().unwrap();

    let counts = engine.count_documents_by_file_path().unwrap();
    assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
    assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
    assert_eq!(counts.len(), 2);
}

#[test]
fn test_count_documents_by_file_path_excludes_uncommitted() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    engine.commit().unwrap();
    add(&mut engine, 2, "שלום רב", "/books/b.txt"); // not committed

    let counts = engine.count_documents_by_file_path().unwrap();
    assert_eq!(counts.len(), 1);
    assert_eq!(counts.get("/books/a.txt").copied(), Some(1));
}

#[test]
fn test_count_documents_by_file_path_from_reopened_index() {
    // The motivating scenario: a fresh engine instance opens a directory
    // that already contains an index, and reconstructs which books are
    // indexed from the index itself (no external state).
    let dir = TempDir::new().unwrap();
    {
        let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "שלום חבר", "/books/b.txt");
        engine.commit().unwrap();
    }

    let reopened = SearchEngine::new(dir.path().to_str().unwrap());
    let counts = reopened.count_documents_by_file_path().unwrap();
    assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
    assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
    assert_eq!(counts.len(), 2);
}

#[test]
fn test_delete_document_by_id() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    add(&mut engine, 2, "שלום רב", "/books/a.txt");
    engine.commit().unwrap();

    assert_eq!(
        engine
            .count(vec!["שלום".to_string()], &["/root".to_string()], 0, 100)
            .unwrap(),
        2
    );

    engine.delete_document_by_id(1).unwrap();
    engine.commit().unwrap();

    assert_eq!(
        engine
            .count(vec!["שלום".to_string()], &["/root".to_string()], 0, 100)
            .unwrap(),
        1
    );
}

#[test]
fn test_upsert_document() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "טקסט ישן", "/books/a.txt");
    engine.commit().unwrap();

    engine
        .upsert_document_at(
            1,
            "title",
            "ref",
            "/root",
            "טקסט חדש",
            0,
            false,
            "/books/a.txt",
            None,
            None,
            None,
        )
        .unwrap();
    engine.commit().unwrap();

    // Should have only one doc with id=1
    assert_eq!(
        engine
            .count(vec!["טקסט".to_string()], &["/root".to_string()], 0, 100)
            .unwrap(),
        1
    );
    assert_eq!(
        engine
            .count(vec!["ישן".to_string()], &["/root".to_string()], 0, 100)
            .unwrap(),
        0
    );
    assert_eq!(
        engine
            .count(vec!["חדש".to_string()], &["/root".to_string()], 0, 100)
            .unwrap(),
        1
    );
}

#[test]
fn compute_content_fingerprint_is_stable_and_never_zero() {
    let a = compute_content_fingerprint("בראשית ברא".to_string());
    let b = compute_content_fingerprint("בראשית ברא".to_string());
    let c = compute_content_fingerprint("בראשית ברה".to_string());
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_ne!(a, 0);
    assert_ne!(compute_content_fingerprint(String::new()), 0);
}

#[test]
fn compute_content_fingerprint_bytes_matches_string_form() {
    let text = "בראשית ברא אלהים";
    assert_eq!(
        compute_content_fingerprint_bytes(text.as_bytes().to_vec()),
        compute_content_fingerprint(text.to_string()),
    );
    // UTF-8 קטוע — הפענוח ה-lossy זהה בשני הצדדים.
    let mut broken = text.as_bytes().to_vec();
    broken.truncate(broken.len() - 1);
    assert_eq!(
        compute_content_fingerprint_bytes(broken.clone()),
        compute_content_fingerprint(String::from_utf8_lossy(&broken).into_owned()),
    );
}

/// Line `id - 1` of `file_path`.
fn fingerprint_doc(id: u32, text: &str, file_path: &str, hash: Option<u64>) -> DocumentInput {
    DocumentInput {
        ordinal: id - 1,
        title: "ספר".to_string(),
        reference: "ref".to_string(),
        topics: "/root".to_string(),
        text: text.to_string(),
        segment: 0,
        is_pdf: false,
        file_path: file_path.to_string(),
        content_hash: hash,
        text_hash: hash,
        source_hash: None,
        text_vocalized: None,
        section_ordinal: None,
        extra_facets: None,
        text_storage: None,
    }
}

#[test]
fn get_book_fingerprints_returns_per_book_hash() {
    let (mut engine, _dir) = make_engine();
    let hash_a = compute_content_fingerprint("ספר א".to_string());
    let hash_b = compute_content_fingerprint("ספר ב".to_string());
    engine
        .add_documents_batch(vec![
            fingerprint_doc(1, "שורה ראשונה", "id:1", Some(hash_a)),
            fingerprint_doc(2, "שורה שנייה", "id:1", Some(hash_a)),
            fingerprint_doc(3, "טקסט אחר", "id:2", Some(hash_b)),
            // PDF-כמו: ללא טביעת אצבע — צריך להופיע כ-0.
            fingerprint_doc(4, "עמוד", "C:/books/a.pdf", None),
        ])
        .unwrap();
    engine.commit().unwrap();

    let fingerprints = engine.get_book_fingerprints().unwrap();
    assert_eq!(fingerprints.get("id:1"), Some(&hash_a));
    assert_eq!(fingerprints.get("id:2"), Some(&hash_b));
    assert_eq!(fingerprints.get("C:/books/a.pdf"), Some(&0));
}

#[test]
fn get_book_fingerprints_reflects_reindex_and_delete() {
    let (mut engine, _dir) = make_engine();
    let old_hash = compute_content_fingerprint("ישן".to_string());
    let new_hash = compute_content_fingerprint("חדש".to_string());
    engine
        .add_documents_batch(vec![
            fingerprint_doc(1, "ישן", "id:1", Some(old_hash)),
            fingerprint_doc(2, "אחר", "id:2", Some(old_hash)),
        ])
        .unwrap();
    engine.commit().unwrap();

    // אינדוקס מחדש של ספר: מחיקה לפי כותרת קודם הייתה מוחקת את שניהם —
    // כאן מדמים דרך upsert לפי id של מסמכי הספר בלבד.
    engine
        .upsert_documents_batch(vec![fingerprint_doc(1, "חדש", "id:1", Some(new_hash))])
        .unwrap();
    engine.commit().unwrap();

    let fingerprints = engine.get_book_fingerprints().unwrap();
    assert_eq!(fingerprints.get("id:1"), Some(&new_hash));
    assert_eq!(fingerprints.get("id:2"), Some(&old_hash));

    engine.remove_documents_by_title("ספר").unwrap();
    engine.commit().unwrap();
    assert!(engine.get_book_fingerprints().unwrap().is_empty());
}

#[test]
fn delete_documents_by_file_path_removes_only_that_book() {
    let (mut engine, _dir) = make_engine();
    engine
        .add_documents_batch(vec![
            fingerprint_doc(1, "שורה ראשונה", "uid:1", Some(3)),
            fingerprint_doc(2, "שורה שנייה", "uid:1", Some(3)),
            fingerprint_doc(3, "טקסט אחר", "id:2", Some(5)),
        ])
        .unwrap();
    engine.commit().unwrap();

    engine.delete_documents_by_file_path("uid:1").unwrap();
    engine.commit().unwrap();

    // כל מסמכי uid:1 נמחקו — הספר האחר (בעל אותה כותרת) לא נפגע.
    let counts = engine.count_documents_by_file_path().unwrap();
    assert_eq!(counts.get("uid:1"), None);
    assert_eq!(counts.get("id:2"), Some(&1));

    // הצורה הקבוצתית מוחקת כמה ספרים בקריאה אחת.
    engine
        .delete_documents_by_file_paths(vec!["id:2".to_string(), "uid:404".to_string()])
        .unwrap();
    engine.commit().unwrap();
    assert!(engine.count_documents_by_file_path().unwrap().is_empty());
}

/// The doc store keeps `reference`, `id` and the stored texts, and `segment` only where
/// `id` does not imply it; results still carry every field, read from the columns.
#[test]
fn the_doc_store_keeps_only_what_no_column_answers() {
    let (mut engine, _dir) = make_engine();
    engine
        .add_text_book_placed(
            "ספר טקסט".to_string(),
            "/root".to_string(),
            "id:1".to_string(),
            1,
            DEFAULT_GENERATION_ORDER,
            "<h1>ספר</h1>
שורה עם מילה ייחודית"
                .to_string(),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine
        .add_pdf_book_placed(
            "ספר סרוק".to_string(),
            "/root".to_string(),
            "/pdf/a.pdf".to_string(),
            2,
            DEFAULT_GENERATION_ORDER,
            vec![PdfPageInput {
                reference: "עמוד 5".to_string(),
                text: "עמוד עם מילה ייחודית".to_string(),
                page_index: 4,
            }],
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    engine.index_reader.reload().unwrap();

    let searcher = engine.index_reader.searcher();
    for (ord, reader) in searcher.segment_readers().iter().enumerate() {
        for doc in reader.doc_ids_alive() {
            let stored: TantivyDocument = searcher.doc(DocAddress::new(ord as u32, doc)).unwrap();
            let mut names: Vec<&str> = stored
                .field_values()
                .map(|(field, _)| engine.schema.get_field_name(field))
                .collect();
            names.sort_unstable();
            names.dedup();
            let pdf = names.contains(&"segment");
            let mut expected = vec!["id", "reference", TEXT_STORED_FIELD];
            if pdf {
                expected.push("segment");
            }
            expected.sort_unstable();
            assert_eq!(names, expected);
        }
    }

    let mut results = engine
        .search_exact(
            "ייחודית".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(results.len(), 2);
    let pdf = results.pop().unwrap();
    let text = results.pop().unwrap();
    assert_eq!(
        (
            text.title.as_str(),
            text.is_pdf,
            text.file_path.as_str(),
            text.segment
        ),
        ("ספר טקסט", false, "id:1", 1)
    );
    assert_eq!(
        (
            pdf.title.as_str(),
            pdf.is_pdf,
            pdf.file_path.as_str(),
            pdf.segment
        ),
        ("ספר סרוק", true, "/pdf/a.pdf", 4)
    );
    let by_id = engine.get_document_by_id(pdf.id).unwrap().unwrap();
    assert_eq!(
        (by_id.title, by_id.is_pdf, by_id.segment),
        (pdf.title, true, 4)
    );
}

/// `id` has no term index: deleting, replacing and fetching by id scan its column.
#[test]
fn lookups_by_id_need_no_id_index() {
    let (mut engine, _dir) = make_engine();
    assert!(!engine
        .schema
        .get_field_entry(engine.schema.get_field("id").unwrap())
        .is_indexed());
    for id in 1..=4 {
        engine
            .add_documents_batch(vec![fingerprint_doc(
                id,
                &format!("שורה {id}"),
                "id:1",
                None,
            )])
            .unwrap();
        engine.commit().unwrap();
    }
    // "id:1" is the index's first book: number 1.
    let id = |line: u64| (1u64 << 32) + line;
    engine.delete_document_by_id(id(2)).unwrap();
    engine
        .upsert_documents_batch(vec![
            fingerprint_doc(3, "ראשונה", "id:1", None),
            fingerprint_doc(3, "אחרונה", "id:1", None),
            fingerprint_doc(5, "חדשה", "id:1", None),
        ])
        .unwrap();
    engine.commit().unwrap();
    engine.index_reader.reload().unwrap();

    assert!(engine.get_document_by_id(id(2)).unwrap().is_none());
    assert_eq!(
        engine.get_document_by_id(id(1)).unwrap().unwrap().text,
        "שורה 1"
    );
    assert_eq!(
        engine.get_document_by_id(id(3)).unwrap().unwrap().text,
        "אחרונה"
    );
    assert_eq!(
        engine.get_document_by_id(id(5)).unwrap().unwrap().text,
        "חדשה"
    );
    assert_eq!(engine.get_document_count(), 4);
}

/// One document of a book holds its fingerprints, so the columns cost nothing on the
/// rest; the book's value is read from it, whatever its other documents lack.
#[test]
fn book_fingerprints_live_on_one_document() {
    let (mut engine, _dir) = make_engine();
    let text = "<h1>ספר</h1>
שורה ראשונה
שורה שנייה
שורה שלישית";
    engine
        .add_text_book_placed(
            "ספר".to_string(),
            "/root".to_string(),
            "id:1".to_string(),
            1,
            DEFAULT_GENERATION_ORDER,
            text.to_string(),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine
        .add_documents_batch(vec![
            fingerprint_doc(100, "א", "id:2", Some(5)),
            fingerprint_doc(101, "ב", "id:2", Some(5)),
            fingerprint_doc(102, "ג", "id:2", None),
        ])
        .unwrap();
    engine.commit().unwrap();
    engine.index_reader.reload().unwrap();

    let searcher = engine.index_reader.searcher();
    let mut holders = 0;
    for reader in searcher.segment_readers() {
        for name in ["contentHash", "textHash"] {
            let column = reader.fast_fields().u64(name).unwrap();
            holders += reader
                .doc_ids_alive()
                .filter(|&doc| column.first(doc).is_some())
                .count();
        }
    }
    assert_eq!(holders, 4, "two books, two columns, one document each");

    let text_hash = compute_content_fingerprint(text.to_string());
    assert_eq!(
        engine.get_book_text_fingerprints().unwrap()["id:1"],
        text_hash
    );
    assert_eq!(
        engine
            .get_book_text_fingerprint("id:1".to_string())
            .unwrap(),
        text_hash
    );
    assert_ne!(engine.get_book_fingerprints().unwrap()["id:1"], 0);
    assert_eq!(engine.get_book_fingerprints().unwrap()["id:2"], 5);
    assert_eq!(
        engine
            .get_book_text_fingerprint("id:2".to_string())
            .unwrap(),
        5
    );
}

#[test]
fn get_book_fingerprints_conflicting_docs_collapse_to_zero() {
    let (mut engine, _dir) = make_engine();
    engine
        .add_documents_batch(vec![
            fingerprint_doc(1, "שורה", "id:1", Some(7)),
            fingerprint_doc(2, "שורה", "id:1", Some(9)),
        ])
        .unwrap();
    engine.commit().unwrap();

    let fingerprints = engine.get_book_fingerprints().unwrap();
    assert_eq!(fingerprints.get("id:1"), Some(&0));
}

#[test]
fn get_book_text_fingerprint_matches_map_form_per_book() {
    // בדיקת דריפט לספר אחד אינה משלמת O(total documents): ה-API הממוקד
    // חייב להחזיר בדיוק את מה שצורת המפה מחזירה לאותו ספר.
    let (mut engine, _dir) = make_engine();
    let text_a = "<h1>ספר א</h1>\nשורה";
    let text_b = "<h1>ספר ב</h1>\nשורה אחרת";
    for (order, path, text) in [(1u32, "id:1", text_a), (2, "id:2", text_b)] {
        engine
            .add_text_book_placed(
                "ספר".to_string(),
                "/root".to_string(),
                path.to_string(),
                order,
                DEFAULT_GENERATION_ORDER,
                text.to_string(),
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();

    let map = engine.get_book_text_fingerprints().unwrap();
    for (path, text) in [("id:1", text_a), ("id:2", text_b)] {
        let single = engine.get_book_text_fingerprint(path.to_string()).unwrap();
        assert_eq!(single, map[path]);
        assert_eq!(single, compute_content_fingerprint(text.to_string()));
    }

    // ספר שאינו באינדקס — "לא ניתן לאימות", לא שגיאה.
    assert_eq!(
        engine
            .get_book_text_fingerprint("id:404".to_string())
            .unwrap(),
        0
    );
}

#[test]
fn get_book_text_fingerprint_is_zero_for_conflicting_and_pdf_books() {
    let (mut engine, _dir) = make_engine();
    // שתי חתימות שונות לאותו filePath (אינדוקס חלקי) — לא ניתן לאימות.
    engine
        .add_documents_batch(vec![
            fingerprint_doc(1, "שורה א", "id:1", Some(11)),
            fingerprint_doc(2, "שורה ב", "id:1", Some(22)),
            // PDF-כמו: בלי חתימת טקסט.
            fingerprint_doc(3, "עמוד", "C:/books/a.pdf", None),
        ])
        .unwrap();
    engine.commit().unwrap();

    assert_eq!(
        engine
            .get_book_text_fingerprint("id:1".to_string())
            .unwrap(),
        0
    );
    assert_eq!(
        engine
            .get_book_text_fingerprint("C:/books/a.pdf".to_string())
            .unwrap(),
        0
    );
}

#[test]
fn text_fingerprint_survives_catalogue_order_change() {
    // מקום הספר בקטלוג אינו באינדקס כלל: הזזתו אינה משנה אף חתימה ואף מזהה,
    // ואינדוקס-מחדש של הספר שומר על מספרו (issue Otzaria#828).
    let text = "<h1>ספר</h1>
שורה של תוכן";
    let (mut engine, _dir) = make_engine();
    let index = |engine: &mut SearchEngine| {
        engine
            .add_text_book(
                "ספר".to_string(),
                "/root".to_string(),
                "id:1".to_string(),
                text.to_string(),
                None,
                TextStorage::InIndex,
                None,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
    };
    let place = |engine: &SearchEngine, catalogue_order| {
        engine
            .set_book_order(vec![BookOrderEntry {
                file_path: "id:1".to_string(),
                catalogue_order,
                generation_order: 0,
            }])
            .unwrap()
    };

    index(&mut engine);
    place(&engine, 5);
    let before = engine.get_book_stamps().unwrap();
    assert_eq!(place(&engine, 6), 2);
    assert_eq!(engine.get_book_stamps().unwrap(), before);

    engine.delete_documents_by_file_path("id:1").unwrap();
    index(&mut engine);
    assert_eq!(engine.get_book_stamps().unwrap(), before);
    assert_eq!(before[0].book_number, 1);
    assert_eq!(
        before[0].text_hash,
        compute_content_fingerprint(text.to_string())
    );
}

#[test]
fn test_rollback() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    engine.commit().unwrap();

    add(&mut engine, 2, "שלום רב", "/books/a.txt");
    engine.rollback().unwrap();
    engine.commit().unwrap();

    // doc 2 should not be present
    assert_eq!(
        engine
            .count(vec!["שלום".to_string()], &["/root".to_string()], 0, 100)
            .unwrap(),
        1
    );
}

fn assert_rollback_preserves_indexing_mode(bulk: bool, economy: bool) {
    let (mut engine, _dir) = make_engine();
    engine.set_bulk_indexing(true).unwrap();
    engine.set_economy_indexing(economy).unwrap();
    for id in 1..=MAX_SEGMENTS_AFTER_OPTIMIZE as u64 {
        add(&mut engine, id, "שלום", &format!("/books/{id}.txt"));
        engine.commit().unwrap();
    }
    engine.set_bulk_indexing(bulk).unwrap();
    add(&mut engine, 99, "מבוטל", "/books/cancelled.txt");
    engine.rollback().unwrap();
    add(&mut engine, 100, "שלום", "/books/resumed.txt");
    engine.commit().unwrap();
    engine
        .take_writer()
        .unwrap()
        .wait_merging_threads()
        .unwrap();
    engine.index_reader.reload().unwrap();

    assert!(engine.get_document_by_id(99).unwrap().is_none());
    assert!(engine.get_document_by_id(100).unwrap().is_some());
    assert_eq!(
        engine.get_document_count(),
        MAX_SEGMENTS_AFTER_OPTIMIZE as u64 + 1
    );
    assert_eq!(
        engine.get_segment_count().unwrap() as usize,
        MAX_SEGMENTS_AFTER_OPTIMIZE + usize::from(bulk),
        "rollback changed the merge policy (bulk={bulk}, economy={economy})"
    );
}

#[test]
fn rollback_preserves_normal_indexing_after_resume() {
    assert_rollback_preserves_indexing_mode(false, false);
}

#[test]
fn rollback_preserves_bulk_indexing_after_resume() {
    assert_rollback_preserves_indexing_mode(true, false);
}

#[test]
fn rollback_preserves_economy_indexing_after_resume() {
    assert_rollback_preserves_indexing_mode(false, true);
}

#[test]
fn rollback_preserves_bulk_economy_indexing_after_resume() {
    assert_rollback_preserves_indexing_mode(true, true);
}

#[test]
fn test_get_document_count() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/a.txt");
    add(&mut engine, 2, "עולם", "/books/b.txt");
    engine.commit().unwrap();
    assert_eq!(engine.get_document_count(), 2);
}

#[test]
fn test_get_document_by_id_found() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 42, "תורה ומצוות", "/books/a.txt");
    engine.commit().unwrap();

    let result = engine.get_document_by_id(42).unwrap();
    assert!(result.is_some());
    let doc = result.unwrap();
    assert_eq!(doc.id, 42);
    assert_eq!(doc.text, "תורה ומצוות");
}

#[test]
fn test_get_document_by_id_not_found() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/a.txt");
    engine.commit().unwrap();

    let result = engine.get_document_by_id(999).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_search_fuzzy() {
    let (mut engine, _dir) = make_engine();
    // "שלום" exact match; "שלם" is one edit away (deletion); "ביי" is unrelated
    add(&mut engine, 1, "שלום", "/books/a.txt");
    add(&mut engine, 2, "שלם", "/books/b.txt");
    add(&mut engine, 3, "ביי", "/books/c.txt");
    engine.commit().unwrap();

    // distance=0: only exact match
    let exact = engine
        .search_fuzzy_terms(
            vec!["שלום".to_string()],
            vec!["/root".to_string()],
            10,
            0,
            0,
            ResultsOrder::Relevance,
            None,
        )
        .unwrap();
    let exact_texts: Vec<&str> = exact.iter().map(|r| r.text.as_str()).collect();
    assert!(
        exact_texts.contains(&"<font color=red>שלום</font>"),
        "distance=0 must return the exact match, highlighted"
    );
    assert!(
        !exact_texts.iter().any(|t| t.contains("שלם")),
        "distance=0 must not return near-match"
    );

    // distance=1: must return both "שלום" and the near-match "שלם"
    let fuzzy = engine
        .search_fuzzy_terms(
            vec!["שלום".to_string()],
            vec!["/root".to_string()],
            10,
            0,
            1,
            ResultsOrder::Relevance,
            None,
        )
        .unwrap();
    let fuzzy_texts: Vec<&str> = fuzzy.iter().map(|r| r.text.as_str()).collect();
    assert!(
        fuzzy_texts.contains(&"<font color=red>שלום</font>"),
        "distance=1 must return exact match, highlighted"
    );
    assert!(
        fuzzy_texts.contains(&"<font color=red>שלם</font>"),
        "distance=1 must return near-match one edit away, highlighted"
    );
    assert!(
        !fuzzy_texts.iter().any(|t| t.contains("ביי")),
        "unrelated term must not appear"
    );
}

#[test]
fn test_set_magic_dictionary_path_reports_validity() {
    let (mut engine, dir) = make_engine();
    assert!(!engine.has_magic_dictionary());
    // Missing file → false, no error, no dictionary loaded.
    assert!(
        !engine.set_magic_dictionary_path(dir.path().join("nope.db").to_str().unwrap().to_string())
    );
    assert!(!engine.has_magic_dictionary());
    // Valid lexical.db → true.
    let db = make_lexical_db(&dir);
    assert!(engine.set_magic_dictionary_path(db));
    assert!(engine.has_magic_dictionary());
}

#[test]
fn test_lexical_fuzzy_finds_inflection_exact_does_not() {
    let (mut engine, dir) = make_engine();
    // Only the inflected form is indexed; the lemma "הלך" is 3 edits away,
    // and no other token is within fuzzy distance 2 of it.
    add(&mut engine, 1, "הלכתי", "/books/a.txt");
    add(&mut engine, 2, "מזרח", "/books/b.txt");
    engine.commit().unwrap();

    // Exact "הלך" must NOT leak into the inflected doc.
    let exact = engine
        .search_exact(
            "הלך".to_string(),
            vec!["/root".to_string()],
            10,
            0,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert!(
        exact.is_empty(),
        "exact search must not match the inflection"
    );

    // Fuzzy WITHOUT dictionary: "הלך"→"הלכתי" is >2 edits, still no match.
    let fuzzy_plain = engine
        .search_fuzzy(
            "הלך".to_string(),
            vec!["/root".to_string()],
            10,
            0,
            2,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert!(
        fuzzy_plain.is_empty(),
        "plain fuzzy cannot reach the inflection at distance 2, got: {:?}",
        fuzzy_plain
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
    );

    // Fuzzy WITH dictionary: the lexical expansion injects "הלכתי" → match.
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));
    let fuzzy_lex = engine
        .search_fuzzy(
            "הלך".to_string(),
            vec!["/root".to_string()],
            10,
            0,
            2,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(fuzzy_lex.len(), 1, "lexical fuzzy must find the inflection");
    assert!(fuzzy_lex[0].text.contains("הלכתי"));

    // count_fuzzy must agree with search_fuzzy (same matching logic).
    let count = engine
        .count_fuzzy(
            "הלך".to_string(),
            vec!["/root".to_string()],
            2,
            false,
            false,
            None,
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn lexical_fuzzy_highlight_terms_hold_every_spelling() {
    let (mut engine, dir) = make_engine();
    for (id, line) in [
        (1, "הלכה הלכ\"ה"),
        (2, "הלכתי הלכת'י"),
        (3, "הולכ\"ים הלך"),
        (4, "הלכו מלך"),
    ] {
        add(&mut engine, id, line, &format!("/books/{id}.txt"));
        engine.commit().unwrap();
    }
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));
    let searcher = engine.searcher_for_tests();
    let text_f = engine.schema.get_field("text").unwrap();
    let dict = engine.magic_dict.as_ref().unwrap();
    let tokens = vec!["הלך".to_string()];
    for distance in [0, 1, 2] {
        // Every spelling of every fuzzy, literal and dictionary term.
        let mut words = if distance == 0 {
            HashSet::new()
        } else {
            let automaton = engine.fuzzy_automaton(&tokens[0], distance).unwrap();
            engine
                .automaton_highlight_terms(&searcher, &[automaton])
                .unwrap()
        };
        words.insert(tokens[0].clone());
        words.extend(dict.highlight_forms(&tokens[0], MAX_LEXICAL_FORMS));
        let words: Vec<String> = words.into_iter().collect();
        let expected: std::collections::BTreeSet<Term> =
            quote_expansion::spelling_terms(&searcher, text_f, &words)
                .unwrap()
                .into_iter()
                .collect();
        assert!(expected
            .iter()
            .any(|t| t.value().as_str().unwrap().contains('"')));

        let query = engine
            .build_lexical_fuzzy_highlight_query(&searcher, &tokens, distance)
            .unwrap();
        let mut terms = std::collections::BTreeSet::new();
        query.query_terms(&mut |term, _| {
            terms.insert(term.clone());
        });
        assert_eq!(terms, expected, "distance {distance}");
    }
}

#[test]
fn automaton_highlight_terms_complete_reports_a_capped_walk() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שבת שב\"ת", "/books/a.txt");
    engine.commit().unwrap();
    let searcher = engine.searcher_for_tests();

    let one = [quote_expansion::Literal::new(["שבת"])];
    let (terms, complete) = engine
        .automaton_highlight_terms_complete(&searcher, &one)
        .unwrap();
    assert!(complete);
    assert_eq!(
        terms,
        HashSet::from(["שבת".to_string(), "שב\"ת".to_string()])
    );

    // A share of one term each: every walk stops at its first term.
    let many = vec![quote_expansion::Literal::new(["שבת"]); MAX_HIGHLIGHT_TERMS];
    let (_, complete) = engine
        .automaton_highlight_terms_complete(&searcher, &many)
        .unwrap();
    assert!(!complete);
}

#[test]
fn test_lexical_fuzzy_distance_zero_uses_dictionary_without_edit_distance() {
    let (mut engine, dir) = make_engine();
    add(&mut engine, 1, "הלכה", "/books/a.txt"); // edit distance 1, not a dictionary form
    add(&mut engine, 2, "הלכתי", "/books/b.txt"); // dictionary form
    add(&mut engine, 3, "הלך", "/books/c.txt"); // exact
    engine.commit().unwrap();
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    let by_relevance = fuzzy_ids(&mut engine, "הלך", 0, ResultsOrder::Relevance);
    assert_eq!(
        by_relevance,
        vec![3, 2],
        "distance 0 = exact word, then its dictionary forms; no edit-distance neighbour"
    );
    let by_catalogue = fuzzy_ids(&mut engine, "הלך", 0, ResultsOrder::Catalogue);
    assert_eq!(by_catalogue, vec![2, 3]);

    let count = engine
        .count_fuzzy(
            "הלך".to_string(),
            vec!["/root".to_string()],
            0,
            false,
            false,
            None,
        )
        .unwrap();
    assert_eq!(count, 2);

    let hit = engine
        .search_fuzzy(
            "הלך".to_string(),
            vec!["/root".to_string()],
            10,
            0,
            0,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .find(|r| r.id == 2)
        .unwrap();
    assert!(
        hit.text.contains("<font color=red>הלכתי</font>"),
        "the dictionary form must be highlighted at distance 0, got: {}",
        hit.text
    );
}

#[test]
fn test_lexical_fuzzy_distance_zero_keeps_quote_free_spelling() {
    let (mut engine, dir) = make_engine();
    add(&mut engine, 1, "דברי רמבם", "/books/a.txt");
    add(&mut engine, 2, "דברי רמב\"ם", "/books/b.txt");
    add(&mut engine, 3, "דברי רמבן", "/books/c.txt");
    engine.commit().unwrap();
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    assert_eq!(
        engine.quoted_phrase_token_groups("רמב\"ם").unwrap(),
        Vec::<Vec<String>>::new(),
        "gershayim inside an acronym is not a phrase delimiter"
    );
    let got = ids(engine
        .search_fuzzy(
            "רמב\"ם".to_string(),
            vec!["/root".to_string()],
            10,
            0,
            0,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap());
    assert_eq!(got, vec![1, 2]);
}

fn fuzzy_ids(
    engine: &mut SearchEngine,
    query: &str,
    max_distance: u8,
    order: ResultsOrder,
) -> Vec<u64> {
    engine
        .search_fuzzy(
            query.to_string(),
            vec!["/root".to_string()],
            100,
            0,
            max_distance,
            order,
            false,
            false,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
}

#[test]
fn test_lexical_fuzzy_relevance_tiers_exact_morphology_fuzzy() {
    let (mut engine, dir) = make_engine();
    add(&mut engine, 10, "הלך", "/books/a.txt"); // exact query token
    add(&mut engine, 5, "הלכתי", "/books/b.txt"); // dictionary surface form (distance 3)
    add(&mut engine, 1, "הלכה", "/books/c.txt"); // bare edit-distance neighbour (distance 1)
    engine.commit().unwrap();
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    // Boosting must not change recall: all three are still matched.
    let count = engine
        .count_fuzzy(
            "הלך".to_string(),
            vec!["/root".to_string()],
            2,
            false,
            false,
            None,
        )
        .unwrap();
    assert_eq!(count, 3, "ranking boosts must not change the recall set");

    // Relevance now tiers them: exact > morphology > edit-distance.
    let by_relevance = fuzzy_ids(&mut engine, "הלך", 2, ResultsOrder::Relevance);
    assert_eq!(
        by_relevance,
        vec![10, 5, 1],
        "relevance must rank exact, then dictionary form, then fuzzy"
    );

    // Catalogue ignores score and stays ordered by the catalogue id.
    let by_catalogue = fuzzy_ids(&mut engine, "הלך", 2, ResultsOrder::Catalogue);
    assert_eq!(
        by_catalogue,
        vec![1, 5, 10],
        "catalogue order must be unaffected by ranking"
    );
}

#[test]
fn test_lexical_fuzzy_multi_word_relevance_differs_from_catalogue() {
    // Both hold the exact adjacent phrase (same tiers), so the exact-tier
    // BM25 add-on decides: the higher-frequency document first.
    let (mut engine, dir) = make_engine();
    add(&mut engine, 1, "הלך מזרח", "/books/a.txt"); // phrase once
    add(&mut engine, 2, "הלך מזרח הלך מזרח", "/books/b.txt"); // phrase twice
    engine.commit().unwrap();
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    let by_catalogue = fuzzy_ids(&mut engine, "הלך מזרח", 2, ResultsOrder::Catalogue);
    assert_eq!(by_catalogue, vec![1, 2], "catalogue follows id order");

    let by_relevance = fuzzy_ids(&mut engine, "הלך מזרח", 2, ResultsOrder::Relevance);
    assert_eq!(
        by_relevance,
        vec![2, 1],
        "relevance must place the higher-frequency phrase first"
    );
}

#[test]
fn test_lexical_fuzzy_exact_floor_survives_common_term() {
    // A near-ubiquitous exact term has BM25 idf ≈ 0, so a purely
    // multiplicative boost would sink it below the flat lexical tier. The
    // constant floor must keep exact hits on top regardless of doc frequency.
    let (mut engine, dir) = make_engine();
    for id in 1..=100u64 {
        add(&mut engine, id, "הלך", "/books/a.txt"); // exact in ~99% of docs
    }
    add(&mut engine, 1000, "הלכתי", "/books/b.txt"); // lone dictionary form
    engine.commit().unwrap();
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    let by_relevance: Vec<u64> = engine
        .search_fuzzy(
            "הלך".to_string(),
            vec!["/root".to_string()],
            200,
            0,
            2,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(by_relevance.len(), 101, "all docs must be recalled");
    assert_eq!(
        by_relevance.last(),
        Some(&1000),
        "the lone lexical form must rank below every exact hit despite idf≈0"
    );
}

#[test]
fn test_plain_fuzzy_relevance_ranks_exact_first() {
    // No dictionary loaded — exercises the plain fuzzy builder.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 9, "כתבה", "/books/a.txt"); // edit-distance neighbour (distance 1)
    add(&mut engine, 2, "כתב", "/books/b.txt"); // exact query token
    engine.commit().unwrap();

    let by_relevance = fuzzy_ids(&mut engine, "כתב", 2, ResultsOrder::Relevance);
    assert_eq!(
        by_relevance,
        vec![2, 9],
        "exact match must outrank a bare fuzzy neighbour"
    );

    // distance 0 stays a pure exact match — recall is byte-identical.
    let zero = fuzzy_ids(&mut engine, "כתב", 0, ResultsOrder::Relevance);
    assert_eq!(zero, vec![2], "distance 0 must match only the exact token");
}

fn add_multi_word_corpus(engine: &mut SearchEngine) {
    add(engine, 1, "הלכתי לישון", "/books/a.txt");
    add(
        engine,
        2,
        "הלכתי ואז דיברתי הרבה לפני לישון",
        "/books/b.txt",
    );
    add(engine, 3, "לישון הלכתי", "/books/c.txt");
    add(engine, 4, "הלכתי", "/books/d.txt");
    add(engine, 5, "לכו ונכהו בלשון", "/books/e.txt");
    engine.commit().unwrap();
}

fn fuzzy_count(engine: &SearchEngine, query: &str, max_distance: u8) -> u32 {
    engine
        .count_fuzzy(
            query.to_string(),
            vec!["/root".to_string()],
            max_distance,
            false,
            false,
            None,
        )
        .unwrap()
}

#[test]
fn test_lexical_fuzzy_multi_word_requires_every_word_anywhere() {
    let (mut engine, dir) = make_engine();
    add_multi_word_corpus(&mut engine);
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    for distance in [0, 2] {
        let got = fuzzy_ids(
            &mut engine,
            "הלכתי לישון",
            distance,
            ResultsOrder::Catalogue,
        );
        assert_eq!(
                got,
                vec![1, 2, 3, 5],
                "distance {distance}: each word (or its forms) anywhere in the line; adjacency not required"
            );
        assert_eq!(fuzzy_count(&engine, "הלכתי לישון", distance), 4);
    }
}

#[test]
fn test_lexical_fuzzy_quoted_phrase_keeps_adjacency() {
    let (mut engine, dir) = make_engine();
    add_multi_word_corpus(&mut engine);
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    assert_eq!(
        engine
            .quoted_phrase_token_groups("“הלכתי לישון” רמב\"ם")
            .unwrap(),
        vec![vec!["הלכתי".to_string(), "לישון".to_string()]]
    );
    for distance in [0, 2] {
        let got = fuzzy_ids(
            &mut engine,
            "\"הלכתי לישון\"",
            distance,
            ResultsOrder::Catalogue,
        );
        assert_eq!(
            got,
            vec![1, 5],
            "distance {distance}: a quoted phrase keeps order, one intervening token allowed"
        );
        assert_eq!(fuzzy_count(&engine, "\"הלכתי לישון\"", distance), 2);
        assert_eq!(
            fuzzy_ids(
                &mut engine,
                "\"הלכתי לישון\"",
                distance,
                ResultsOrder::Relevance
            ),
            vec![1, 5],
            "distance {distance}: the exact phrase ranks above its dictionary forms"
        );
    }
}

#[test]
fn test_lexical_fuzzy_quoted_phrase_ranks_exact_hits_by_bm25() {
    let (mut engine, dir) = make_engine();
    add(
        &mut engine,
        1,
        "הלכתי לישון אחרי יום ארוך של עבודה קשה",
        "/books/a.txt",
    );
    add(&mut engine, 2, "הלכתי לישון", "/books/b.txt");
    engine.commit().unwrap();
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    assert_eq!(
        fuzzy_ids(&mut engine, "\"הלכתי לישון\"", 1, ResultsOrder::Relevance),
        vec![2, 1],
        "both hold the exact phrase; the shorter line scores higher"
    );
}

#[test]
fn test_lexical_fuzzy_multi_word_ranks_exact_phrase_then_scattered_then_forms() {
    let (mut engine, dir) = make_engine();
    add(&mut engine, 1, "הלכתי ואז דיברתי לישון", "/books/a.txt"); // exact, scattered
    add(&mut engine, 2, "לכו לישון", "/books/b.txt"); // form + exact, adjacent
    add(&mut engine, 3, "הלכתי לישון", "/books/c.txt"); // exact phrase
    engine.commit().unwrap();
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    for distance in [0, 1] {
        assert_eq!(
            fuzzy_ids(
                &mut engine,
                "הלכתי לישון",
                distance,
                ResultsOrder::Relevance
            ),
            vec![3, 1, 2],
            "distance {distance}"
        );
    }
}

#[test]
fn test_lexical_fuzzy_multi_word_count_matches_hits() {
    let (mut engine, dir) = make_engine();
    add_multi_word_corpus(&mut engine);
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    for query in ["הלכתי לישון", "\"הלכתי לישון\"", "לישון הלכתי"] {
        for distance in [0, 1, 2] {
            let page = engine
                .search_and_count_fuzzy(
                    query.to_string(),
                    vec!["/root".to_string()],
                    100,
                    0,
                    distance,
                    ResultsOrder::Relevance,
                    false,
                    false,
                    None,
                    None,
                )
                .unwrap();
            assert_eq!(
                page.total_count as usize,
                page.results.len(),
                "{query} at distance {distance}"
            );
            assert_eq!(
                fuzzy_count(&engine, query, distance),
                page.total_count,
                "{query} at distance {distance}"
            );
        }
    }
}

#[test]
fn test_lexical_fuzzy_multi_word_highlights_each_word_separately() {
    let (mut engine, dir) = make_engine();
    add_multi_word_corpus(&mut engine);
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    for distance in [0, 1] {
        let hit = engine
            .search_fuzzy(
                "הלכתי לישון".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                distance,
                ResultsOrder::Relevance,
                false,
                false,
                None,
                None,
            )
            .unwrap()
            .into_iter()
            .find(|r| r.id == 2)
            .unwrap();
        assert!(
            hit.text.contains("<font color=red>הלכתי</font>")
                && hit.text.contains("<font color=red>לישון</font>"),
            "distance {distance}: both scattered words must be painted, got: {}",
            hit.text
        );

        let pattern = engine
            .generate_index_fuzzy_highlight_pattern("הלכתי לישון".to_string(), distance, None)
            .unwrap()
            .expect("pattern");
        let matcher = pattern.matcher.expect("matcher");
        assert_eq!(
            matcher
                .find_word_matches("הלכתי ואז דיברתי הרבה לפני לישון".to_string(), vec![])
                .len(),
            2,
            "distance {distance}: the in-book view paints each word on its own"
        );
        assert_eq!(
            matcher
                .find_word_matches("לכו ונכהו בלשון".to_string(), vec![])
                .len(),
            2,
            "distance {distance}: dictionary forms are painted too"
        );
    }
}

#[test]
fn test_lexical_fuzzy_highlights_literal_second_token() {
    let (mut engine, dir) = make_engine();
    add(&mut engine, 1, "הולכים לישון", "/books/a.txt");
    for (idx, term) in one_edit_insertions("לישון").into_iter().enumerate() {
        add(
            &mut engine,
            idx as u64 + 2,
            &format!("רעש {term}"),
            "/books/noise.txt",
        );
    }
    engine.commit().unwrap();
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    let results = engine
        .search_fuzzy(
            "הלכתי לישון".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            2,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    let hit = results.iter().find(|result| result.id == 1).unwrap();
    assert!(
        hit.text.contains("<font color=red>לישון</font>"),
        "the literal second query token must remain highlighted, got: {}",
        hit.text
    );
}

#[test]
fn test_lexical_fuzzy_multi_word_allows_expansions_per_token() {
    let (mut engine, dir) = make_engine();
    add(&mut engine, 1, "הלכתי לישון", "/books/a.txt");

    for (next_id, term) in (2u64..).zip(
        one_edit_insertions("הלכתי")
            .into_iter()
            .chain(one_edit_insertions("לישון")),
    ) {
        add(&mut engine, next_id, &term, "/books/noise.txt");
    }
    engine.commit().unwrap();
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

    let got = ids(engine
        .search_fuzzy(
            "הלכתי לישון".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            1,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap());
    assert_eq!(got, vec![1]);
}

fn one_edit_insertions(token: &str) -> Vec<String> {
    const LETTERS: &[char] = &[
        'א', 'ב', 'ג', 'ד', 'ה', 'ו', 'ז', 'ח', 'ט', 'י', 'כ', 'ל', 'מ', 'נ', 'ס', 'ע', 'פ', 'צ',
        'ק', 'ר', 'ש', 'ת',
    ];

    let chars: Vec<char> = token.chars().collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for position in 0..=chars.len() {
        for letter in LETTERS {
            let mut variant = chars.clone();
            variant.insert(position, *letter);
            let variant: String = variant.into_iter().collect();
            if variant != token && seen.insert(variant.clone()) {
                out.push(variant);
            }
        }
    }
    out
}

#[test]
fn test_new_tolerates_held_writer_lock() {
    let (mut first, dir) = make_engine();
    add(&mut first, 1, "ספר", "/books/a.txt");
    first.commit().unwrap();

    // While `first` holds the writer lock, a second engine must still open
    // (no panic) and serve reads.
    let mut second = SearchEngine::new(dir.path().to_str().unwrap());
    assert_eq!(search_ids(&mut second, "ספר"), vec![1]);

    // Once the lock is released, writes recover lazily via ensure_writer.
    drop(first);
    add(&mut second, 2, "תורה", "/books/b.txt");
    second.commit().unwrap();
    assert_eq!(search_ids(&mut second, "תורה"), vec![2]);
}

#[test]
fn test_search_and_count() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    add(&mut engine, 2, "שלום רב", "/books/a.txt");
    add(&mut engine, 3, "ביי", "/books/b.txt");
    engine.commit().unwrap();

    let page = engine
        .search_and_count(
            vec!["שלום".to_string()],
            vec!["/root".to_string()],
            1,
            0,
            0,
            100,
            ResultsOrder::Relevance,
            None,
            None,
        )
        .unwrap();

    assert_eq!(
        page.total_count, 2,
        "total_count should reflect all hits, not just page size"
    );
    assert_eq!(
        page.results.len(),
        1,
        "results should be limited by limit param"
    );
}

#[test]
fn test_search_offset() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    add(&mut engine, 2, "שלום רב", "/books/b.txt");
    add(&mut engine, 3, "שלום חבר", "/books/c.txt");
    engine.commit().unwrap();

    let page1 = engine
        .search(
            vec!["שלום".to_string()],
            vec!["/root".to_string()],
            2,
            0,
            0,
            100,
            ResultsOrder::Catalogue,
            None,
            None,
        )
        .unwrap();
    let page2 = engine
        .search(
            vec!["שלום".to_string()],
            vec!["/root".to_string()],
            2,
            2,
            0,
            100,
            ResultsOrder::Catalogue,
            None,
            None,
        )
        .unwrap();

    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 1);
    // Pages must not overlap
    let ids1: Vec<u64> = page1.iter().map(|r| r.id).collect();
    let ids2: Vec<u64> = page2.iter().map(|r| r.id).collect();
    assert!(ids1.iter().all(|id| !ids2.contains(id)));
}

#[test]
fn test_optimize_reduces_segments_many_commits() {
    let (mut engine, _dir) = make_engine();
    disable_auto_merge(&mut engine);

    for id in 1..=12 {
        let text = format!("שלום {id}");
        let file_path = format!("/books/{id}.txt");
        add(&mut engine, id, &text, &file_path);
        engine.commit().unwrap();
    }

    let before = engine.get_segment_count().unwrap();
    assert!(before > 1, "test setup should create multiple segments");

    engine.optimize().unwrap();

    let after = engine.get_segment_count().unwrap();

    assert!(
        after <= before,
        "optimize should not increase segment count"
    );
    assert_eq!(
        after, MAX_SEGMENTS_AFTER_OPTIMIZE as u32,
        "optimize compacts the smallest segments down to the cap"
    );
    assert_eq!(engine.get_document_count(), 12);
}

#[test]
fn test_optimize_commits_pending_documents() {
    let (mut engine, _dir) = make_engine();
    disable_auto_merge(&mut engine);
    // Two committed segments so optimize doesn't take the early-skip path.
    add(&mut engine, 1, "ספר", "/books/a.txt");
    engine.commit().unwrap();
    add(&mut engine, 2, "ספר", "/books/b.txt");
    engine.commit().unwrap();

    // A pending document must survive optimize, not vanish with the
    // discarded writer buffer.
    add(&mut engine, 3, "ספר", "/books/c.txt");
    engine.optimize().unwrap();

    assert_eq!(search_ids(&mut engine, "ספר"), vec![1, 2, 3]);
}

#[test]
fn test_optimize_preserves_search_results() {
    let (mut engine, _dir) = make_engine();
    disable_auto_merge(&mut engine);

    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    engine.commit().unwrap();
    add(&mut engine, 2, "שלום רב", "/books/b.txt");
    engine.commit().unwrap();
    add(&mut engine, 3, "ביי", "/books/c.txt");
    engine.commit().unwrap();
    add(&mut engine, 4, "שלום חבר", "/books/d.txt");
    engine.commit().unwrap();

    let before_ids = search_ids(&mut engine, "שלום");
    engine.optimize().unwrap();
    let after_ids = search_ids(&mut engine, "שלום");

    assert_eq!(
        before_ids, after_ids,
        "optimize must preserve search results"
    );
}

#[test]
fn test_optimize_preserves_upsert_and_delete_afterwards() {
    let (mut engine, _dir) = make_engine();
    disable_auto_merge(&mut engine);

    add(&mut engine, 1, "טקסט ישן", "/books/a.txt");
    engine.commit().unwrap();
    add(&mut engine, 2, "למחיקה", "/books/b.txt");
    engine.commit().unwrap();

    engine.optimize().unwrap();

    engine
        .upsert_document_at(
            1,
            "title",
            "ref",
            "/root",
            "טקסט חדש",
            0,
            false,
            "/books/a.txt",
            None,
            None,
            None,
        )
        .unwrap();
    engine.delete_document_by_id(2).unwrap();
    engine.commit().unwrap();

    assert_eq!(search_ids(&mut engine, "ישן"), Vec::<u64>::new());
    assert_eq!(search_ids(&mut engine, "חדש"), vec![1]);
    assert!(engine.get_document_by_id(2).unwrap().is_none());
}

#[test]
fn test_optimize_noop_when_single_segment() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/a.txt");
    engine.commit().unwrap();

    let before = engine.get_segment_count().unwrap();
    engine.optimize().unwrap();
    let after = engine.get_segment_count().unwrap();

    assert_eq!(before, 1);
    assert_eq!(after, 1);

    add(&mut engine, 2, "עולם", "/books/b.txt");
    engine.commit().unwrap();
    assert_eq!(search_ids(&mut engine, "עולם"), vec![2]);
}

#[test]
fn test_writer_reopens_after_transient_reopen_failure() {
    let (mut engine, _dir) = make_engine();

    engine.index_writer = None;
    let competing_writer: IndexWriter<TantivyDocument> =
        engine.index.writer(DEFAULT_WRITER_HEAP_SIZE).unwrap();

    let err = engine
        .add_document_at(
            1,
            "title",
            "ref",
            "/root",
            "שלום",
            0,
            false,
            "/books/a.txt",
            None,
            None,
            None,
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("Failed to acquire index lock")
            || err.to_string().contains("LockFailure"),
        "unexpected error: {err:#}"
    );
    assert!(engine.index_writer.is_none());

    drop(competing_writer);

    add(&mut engine, 1, "שלום", "/books/a.txt");
    engine.commit().unwrap();

    assert_eq!(search_ids(&mut engine, "שלום"), vec![1]);
}

#[test]
fn test_clear_reopens_after_transient_reopen_failure() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/a.txt");
    engine.commit().unwrap();

    engine.index_writer = None;
    let competing_writer: IndexWriter<TantivyDocument> =
        engine.index.writer(DEFAULT_WRITER_HEAP_SIZE).unwrap();

    let err = engine.clear().unwrap_err();
    assert!(
        err.to_string().contains("Failed to acquire index lock")
            || err.to_string().contains("LockFailure"),
        "unexpected error: {err:#}"
    );
    assert!(engine.index_writer.is_none());

    drop(competing_writer);

    engine.clear().unwrap();
    engine.commit().unwrap();

    assert_eq!(engine.get_document_count(), 0);
    assert_eq!(search_ids(&mut engine, "שלום"), Vec::<u64>::new());
}

// ── High-level mode-specific API ─────────────────────────────────────────────

fn ids(results: Vec<SearchResult>) -> Vec<u64> {
    let mut v: Vec<u64> = results.into_iter().map(|r| r.id).collect();
    v.sort();
    v
}

#[test]
fn test_search_exact_single_and_phrase() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    add(&mut engine, 2, "שלום רב", "/books/b.txt");
    engine.commit().unwrap();

    // Single token matches both docs containing the word.
    let got = ids(engine
        .search_exact(
            "שלום".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap());
    assert_eq!(got, vec![1, 2]);

    // Phrase matches only the doc with those adjacent words.
    let got = ids(engine
        .search_exact(
            "שלום עולם".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap());
    assert_eq!(got, vec![1]);
}

#[test]
fn test_search_exact_strips_query_nikud() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/a.txt"); // indexed without nikud
    engine.commit().unwrap();

    // Query carries nikud; exact mode strips it before tokenizing.
    let got = ids(engine
        .search_exact(
            "שָׁלוֹם".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap());
    assert_eq!(got, vec![1]);
}

#[test]
fn test_stored_text_keeps_punctuation_and_highlight_lands_on_it() {
    let (mut engine, _dir) = make_engine();
    // מקצה-לקצה של issue #446/#500: הטקסט השמור משמר פיסוק ונקי מניקוד
    // ומ-Presentation Forms, ושאילתה רגילה מוצאת ומדגישה אותו.
    let raw = "וּבָזֶה יוּבַן שַׁ\"ס (עא:) הַמְמַלֵּא גְּרוֹנָם, מ\u{FB1D}ם וכו'";
    let stored = crate::hebrew_query::normalize_text_for_indexing(raw);
    assert_eq!(stored, "ובזה יובן ש\"ס (עא:) הממלא גרונם, מים וכו'");
    engine
        .add_document_at(
            1,
            "title",
            "ref",
            "/root",
            &stored,
            0,
            false,
            "/books/a.txt",
            None,
            None,
            None,
        )
        .unwrap();
    engine.commit().unwrap();

    for query in ["הממלא", "מים", "עא"] {
        let results = engine
            .search_exact(
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(ids(results.clone()), vec![1], "no hit for {query}");
        assert!(
            results[0]
                .text
                .contains(&format!("<font color=red>{query}</font>")),
            "highlight missing for {query}: {}",
            results[0].text
        );
    }
}

#[test]
fn test_snippet_keeps_punctuation_after_last_word() {
    let (mut engine, _dir) = make_engine();
    // בראשית ב, ג כפי שהוא ב-seforim.db: השורה מסתיימת ב-`{פ}`.
    let raw = "(ג) וַיְבָ֤רֶךְ אֱלֹהִים֙ אֶת־י֣וֹם הַשְּׁבִיעִ֔י וַיְקַדֵּ֖שׁ אֹת֑וֹ \
            כִּ֣י ב֤וֹ שָׁבַת֙ מִכׇּל־מְלַאכְתּ֔וֹ אֲשֶׁר־בָּרָ֥א אֱלֹהִ֖ים \
            לַעֲשֽׂוֹת׃&nbsp;<span class=\"mam-spi-pe\">{פ}</span><br>";
    let stored = crate::hebrew_query::normalize_text_for_indexing(raw);
    assert!(stored.ends_with("לעשות׃ {פ}"), "stored: {stored}");
    add(&mut engine, 1, &stored, "/books/a.txt");
    engine.commit().unwrap();

    // מילה בודדת ומסלול הביטוי (phrase_filtered_snippet_html).
    for query in ["שבת", "אלהים לעשות"] {
        let results = engine
            .search_exact(
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(ids(results.clone()), vec![1], "no hit for {query}");
        assert!(
            results[0].text.ends_with("{פ}"),
            "closing brace dropped for {query}: {}",
            results[0].text
        );
    }
}

#[test]
fn test_snippet_keeps_punctuation_before_later_fragment() {
    let (mut engine, _dir) = make_engine();
    // 795 בתים לפני `{שבת}`: `שבת` חורג מ-800 ופותח קטע שני משלו.
    let prefix = "אא ".repeat(159);
    assert_eq!(prefix.len(), 795);
    add(&mut engine, 1, &format!("{prefix}{{שבת}}"), "/books/a.txt");
    engine.commit().unwrap();

    let results = engine
        .search_exact(
            "שבת".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(ids(results.clone()), vec![1]);
    assert_eq!(results[0].text, "{<font color=red>שבת</font>}");
}

#[test]
fn test_snippet_punctuation_comes_from_the_chosen_fragment() {
    let (mut engine, _dir) = make_engine();
    // `שבת שבת` חוצה את גבול הקטע הראשון (790) ומופיע שוב בסוף, בקטע
    // השלישי (1596) שמנצח בניקוד. חיפוש טקסט היה לוקח את `.` מהמופע הראשון.
    let text = format!(
        "{}שבת שבת. {}{{שבת שבת}}",
        "אא ".repeat(158),
        "אא ".repeat(158)
    );
    add(&mut engine, 1, &text, "/books/a.txt");
    engine.commit().unwrap();

    let results = engine
        .search_exact(
            "שבת".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(ids(results.clone()), vec![1]);
    assert_eq!(
        results[0].text,
        "{<font color=red>שבת</font> <font color=red>שבת</font>}"
    );
}

#[test]
fn glued_punctuation_stops_at_whitespace_and_words() {
    let text = "אמר לעשות׃ {פ} כי־גר רעהו,10 סוף.";
    let glued = |fragment: &str| {
        let start = text.find(fragment).unwrap();
        glued_punctuation(text, start..start + fragment.len())
    };
    assert_eq!(glued("אמר לעשות"), ("", "׃"));
    assert_eq!(glued("לעשות׃ {פ"), ("", "}"));
    assert_eq!(glued("פ} כי"), ("{", ""));
    assert_eq!(glued("רעהו"), ("", ""));
    assert_eq!(glued("סוף"), ("", "."));
    assert_eq!(glued_punctuation(text, 0..0), ("", ""));
}

#[test]
fn test_search_advanced_grammatical_prefix() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר", "/books/a.txt");
    add(&mut engine, 2, "הספר", "/books/b.txt");
    add(&mut engine, 3, "מטבע", "/books/c.txt");
    engine.commit().unwrap();

    let mut word_opts = HashMap::new();
    word_opts.insert("קידומות דקדוקיות".to_string(), true);
    let mut options = HashMap::new();
    options.insert("ספר_0".to_string(), word_opts);

    let got = ids(search_advanced_default(
        &engine,
        "ספר".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        options,
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap());
    assert_eq!(
        got,
        vec![1, 2],
        "grammatical prefix should match ספר and הספר"
    );
}

#[test]
fn test_search_advanced_heavy_option_combo_compiles_and_runs() {
    // typo + grammatical prefix + grammatical suffix produces the largest
    // morphological regex. The length budget must keep it under tantivy-fst's
    // DFA state limit so the search compiles and returns instead of erroring.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר", "/books/a.txt");
    add(&mut engine, 2, "הספרים", "/books/b.txt");
    engine.commit().unwrap();

    let mut word_opts = HashMap::new();
    word_opts.insert(hebrew_query::OPT_TYPO.to_string(), true);
    word_opts.insert("קידומות דקדוקיות".to_string(), true);
    word_opts.insert("סיומות דקדוקיות".to_string(), true);
    let mut options = HashMap::new();
    options.insert("ספר_0".to_string(), word_opts);

    let result = search_advanced_default(
        &engine,
        "ספר".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        options,
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    );
    let got = ids(result.expect("heavy option combo must compile and run"));
    assert!(
        got.contains(&1),
        "the base word ספר should still match, got {got:?}"
    );
}

#[test]
fn test_single_term_respects_max_expansions() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר", "/books/a.txt");
    add(&mut engine, 2, "הספר", "/books/b.txt");
    engine.commit().unwrap();

    // Two index terms match; a cap of 1 truncates the term set (degrade,
    // not error) — exactly one document comes back. Which term survives
    // depends on segment order (each doc may land in its own segment),
    // so only the count is pinned.
    let truncated = ids(engine
        .search(
            vec![".*ספר".to_string()],
            vec![],
            100,
            0,
            0,
            1,
            ResultsOrder::Catalogue,
            None,
            None,
        )
        .unwrap());
    assert_eq!(
        truncated.len(),
        1,
        "a cap of 1 should serve exactly one term's documents, got {truncated:?}"
    );
    assert!(
        truncated[0] == 1 || truncated[0] == 2,
        "unexpected document {truncated:?}"
    );

    let ok = ids(engine
        .search(
            vec![".*ספר".to_string()],
            vec![],
            100,
            0,
            0,
            10,
            ResultsOrder::Catalogue,
            None,
            None,
        )
        .unwrap());
    assert_eq!(ok, vec![1, 2]);
}

#[test]
fn test_single_term_truncation_flag_surfaces() {
    // The degrade path must report itself so the stream can flag partial
    // results to the UI, instead of silently serving a subset.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר", "/books/a.txt");
    add(&mut engine, 2, "הספר", "/books/b.txt");
    engine.commit().unwrap();

    // Two terms match `.*ספר`; a cap of 1 stops collection early.
    let (_, truncated) = engine
        .build_query(vec![".*ספר".to_string()], vec![], 0, 1)
        .unwrap();
    assert!(
        truncated,
        "a cap of 1 over two matching terms must report truncation"
    );

    // A generous cap collects everything — no degrade, no flag.
    let (_, not_truncated) = engine
        .build_query(vec![".*ספר".to_string()], vec![], 0, 100)
        .unwrap();
    assert!(
        !not_truncated,
        "a cap that fits every term must not report truncation"
    );
}

#[test]
fn test_search_and_count_propagates_truncation_flag() {
    // The page/count API must surface the same degrade signal the stream
    // does — dropping it lets a consumer show partial results unwarned.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר", "/books/a.txt");
    add(&mut engine, 2, "הספר", "/books/b.txt");
    engine.commit().unwrap();

    let page = engine
        .search_and_count(
            vec![".*ספר".to_string()],
            vec![],
            100,
            0,
            0,
            1,
            ResultsOrder::Catalogue,
            None,
            None,
        )
        .unwrap();
    assert!(
        page.truncated,
        "a cap of 1 over two matching terms must flag the page result"
    );

    let page = engine
        .search_and_count(
            vec![".*ספר".to_string()],
            vec![],
            100,
            0,
            0,
            100,
            ResultsOrder::Catalogue,
            None,
            None,
        )
        .unwrap();
    assert!(!page.truncated, "an uncapped query must not flag the page");
    assert_eq!(page.total_count, 2);
}

#[test]
fn test_count_apis_with_status_surface_truncation() {
    // count/count_by_book/get_facet_counts feed the facet filter tree; the
    // *_with_status variants must carry the same degrade signal so a
    // consumer can flag partial counts instead of showing them as exact.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר", "/books/a.txt");
    add(&mut engine, 2, "הספר", "/books/b.txt");
    engine.commit().unwrap();

    let capped = engine
        .count_with_status(vec![".*ספר".to_string()], &[], 0, 1)
        .unwrap();
    assert!(capped.truncated, "a cap of 1 must flag count_with_status");

    let books = engine
        .count_by_book_with_status(vec![".*ספר".to_string()], vec![], 0, 1)
        .unwrap();
    assert!(books.truncated, "a cap of 1 must flag the per-book counts");

    let facets = engine
        .get_facet_counts_with_status(vec![".*ספר".to_string()], vec![], "/".to_string(), 0, 1)
        .unwrap();
    assert!(facets.truncated, "a cap of 1 must flag the facet counts");

    let uncapped = engine
        .count_with_status(vec![".*ספר".to_string()], &[], 0, 100)
        .unwrap();
    assert!(!uncapped.truncated, "an uncapped count must not flag");
    assert_eq!(uncapped.count, 2);

    let facets = engine
        .get_facet_counts_with_status(vec![".*ספר".to_string()], vec![], "/".to_string(), 0, 100)
        .unwrap();
    assert!(!facets.truncated, "an uncapped facet count must not flag");
    assert!(
        facets
            .counts
            .iter()
            .any(|f| f.path == "/root" && f.count == 2),
        "facet counts should be exact when not truncated"
    );

    // The bare API stays backward compatible: same count, flag dropped.
    let bare = engine
        .count(vec![".*ספר".to_string()], &[], 0, 100)
        .unwrap();
    assert_eq!(bare, uncapped.count);
}

#[test]
fn test_advanced_count_apis_with_status_surface_truncation() {
    // The advanced *_with_status trio must propagate the degrade signal
    // through build_advanced_query. "חלק ממילה" on a one-letter word gets
    // the relaxed 20 000-term cap (plain_max_expansions), so an index
    // with 20 812 matching terms overflows it.
    let (mut engine, _dir) = make_engine();
    let letters = [
        'א', 'ב', 'ג', 'ד', 'ה', 'ו', 'ז', 'ח', 'ט', 'י', 'כ', 'ל', 'מ', 'נ', 'ס', 'ע', 'פ', 'צ',
        'ק', 'ר', 'ש', 'ת',
    ];
    // 22³ words starting with א plus 21×22² with א second — all distinct,
    // all inside the `.{0,3}א.{0,3}` partial window.
    let mut words: Vec<String> = Vec::new();
    for a in letters {
        for b in letters {
            for c in letters {
                words.push(format!("א{a}{b}{c}"));
                if a != 'א' {
                    words.push(format!("{a}א{b}{c}"));
                }
            }
        }
    }
    for (i, chunk) in words.chunks(4_000).enumerate() {
        add(&mut engine, i as u64 + 1, &chunk.join(" "), "/books/a.txt");
    }
    // Control docs for the under-cap path (no א anywhere).
    add(&mut engine, 100, "שלום עולם", "/books/b.txt");
    add(&mut engine, 101, "שלום", "/books/c.txt");
    engine.commit().unwrap();

    let partial_on = |word: &str| -> HashMap<String, HashMap<String, bool>> {
        HashMap::from([(
            format!("{word}_0"),
            HashMap::from([("חלק ממילה".to_string(), true)]),
        )])
    };

    let count = engine
        .count_advanced_with_status(
            "א".to_string(),
            String::new(),
            vec!["/root".to_string()],
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            partial_on("א"),
            HashMap::new(),
            false,
            false,
            SearchScope::WordDistance,
            SearchScope::WordDistance,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(
        count.truncated,
        "20 812 matching terms over the 20 000 cap must flag the advanced count"
    );

    let books = engine
        .count_by_book_advanced_with_status(
            "א".to_string(),
            String::new(),
            vec!["/root".to_string()],
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            partial_on("א"),
            HashMap::new(),
            false,
            false,
            SearchScope::WordDistance,
            SearchScope::WordDistance,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(
        books.truncated,
        "the advanced per-book counts must carry the same flag"
    );

    let facets = engine
        .get_facet_counts_advanced_with_status(
            "א".to_string(),
            String::new(),
            vec!["/root".to_string()],
            "/".to_string(),
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            partial_on("א"),
            HashMap::new(),
            false,
            false,
            SearchScope::WordDistance,
            SearchScope::WordDistance,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(
        facets.truncated,
        "the advanced facet counts must carry the same flag"
    );

    // A word matching a single term stays far under the cap: no flag,
    // exact counts on every path.
    let count = engine
        .count_advanced_with_status(
            "שלום".to_string(),
            String::new(),
            vec!["/root".to_string()],
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            partial_on("שלום"),
            HashMap::new(),
            false,
            false,
            SearchScope::WordDistance,
            SearchScope::WordDistance,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(!count.truncated, "under the cap the count must not flag");
    assert_eq!(count.count, 2, "both control documents match");

    let facets = engine
        .get_facet_counts_advanced_with_status(
            "שלום".to_string(),
            String::new(),
            vec!["/root".to_string()],
            "/".to_string(),
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            partial_on("שלום"),
            HashMap::new(),
            false,
            false,
            SearchScope::WordDistance,
            SearchScope::WordDistance,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(!facets.truncated, "under the cap the facets must not flag");
    assert!(
        facets
            .counts
            .iter()
            .any(|f| f.path == "/root" && f.count == 2),
        "facet counts should be exact when not truncated"
    );
}

#[test]
fn test_search_advanced_strips_query_nikud() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר תורה", "/books/a.txt");
    engine.commit().unwrap();

    // Pasted vocalized text must still match the nikud-free index terms.
    let got = ids(search_advanced_default(
        &engine,
        "סֵפֶר".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap());
    assert_eq!(got, vec![1], "vocalized advanced query should match");
}

#[test]
fn test_search_advanced_empty_query_returns_no_results() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר", "/books/a.txt");
    engine.commit().unwrap();

    // Empty and punctuation-only queries produce zero regex terms; they must
    // return no results instead of panicking inside RegexPhraseQuery.
    for query in ["", "?!"] {
        let results = search_advanced_default(
            &engine,
            query.to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap();
        assert!(results.is_empty(), "query {query:?} should match nothing");
    }
}

#[test]
fn test_search_skips_empty_facets() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר", "/books/a.txt");
    engine.commit().unwrap();

    let got = ids(engine
        .search(
            vec!["ספר".to_string()],
            vec![],
            100,
            0,
            0,
            100,
            ResultsOrder::Catalogue,
            None,
            None,
        )
        .unwrap());
    assert_eq!(got, vec![1], "empty facet list should not filter anything");
}

#[test]
fn test_search_rejects_invalid_facet() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר", "/books/a.txt");
    engine.commit().unwrap();

    let result = engine.search(
        vec!["ספר".to_string()],
        vec!["not-a-facet".to_string()],
        100,
        0,
        0,
        100,
        ResultsOrder::Catalogue,
        None,
        None,
    );
    assert!(result.is_err(), "malformed facet should error, not panic");
}

#[test]
fn test_search_advanced_highlights_morphological_variant() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "ספר", "/books/a.txt");
    add(&mut engine, 2, "הספר", "/books/b.txt");
    engine.commit().unwrap();

    let mut word_opts = HashMap::new();
    word_opts.insert("קידומות דקדוקיות".to_string(), true);
    let mut options = HashMap::new();
    options.insert("ספר_0".to_string(), word_opts);

    let results = search_advanced_default(
        &engine,
        "ספר".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        options,
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap();

    // The query matched the prefixed variant "הספר" via regex; highlighting
    // must wrap the variant that actually matched, not just the literal "ספר".
    let variant = results
        .iter()
        .find(|r| r.id == 2)
        .expect("הספר document should be in results");
    assert_eq!(
        variant.text, "<font color=red>הספר</font>",
        "morphological variant should be highlighted"
    );
}

#[test]
fn test_search_advanced_alternative_words() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "מלך", "/books/a.txt");
    add(&mut engine, 2, "שר", "/books/b.txt");
    add(&mut engine, 3, "עיר", "/books/c.txt");
    engine.commit().unwrap();

    let mut alts = HashMap::new();
    alts.insert(0u32, vec!["מלך".to_string()]);
    let got = ids(search_advanced_default(
        &engine,
        "שר".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        alts,
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap());
    assert_eq!(got, vec![1, 2], "alternatives should OR שר with מלך");
}

#[test]
fn advanced_search_matches_gershayim_tokens_end_to_end() {
    // HebrewTokenizer שומר גרשיים וגרש פנימי בטוקן (ז"ל, רמב"ם — טרם
    // אחד); split_query_words חייב לפצל את השאילתה באותה צורה — אחרת
    // ז"ל לעולם לא יימצא. כולל נורמליזציה של ״ עברי משני הצדדים
    // וקיפול זוג-הגרשים (רמב''ם) בשאילתה.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "הרב פלוני ז\"ל אמר", "/books/a.txt");
    add(&mut engine, 2, "כתב הרמב\u{05F4}ם בהלכות", "/books/b.txt");
    add(&mut engine, 3, "דברי תוס' שם", "/books/c.txt");
    add(&mut engine, 4, "מסמך עם רמב ם כשתי מילים", "/books/d.txt");
    add(&mut engine, 5, "אמר ג'ורג' לד'אש", "/books/e.txt");
    engine.commit().unwrap();

    let advanced_ids = |engine: &mut SearchEngine, query: &str| {
        ids(search_advanced_default(
            engine,
            query.to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap())
    };

    assert_eq!(advanced_ids(&mut engine, "ז\"ל"), vec![1]);
    assert_eq!(
        advanced_ids(&mut engine, "ז\u{05F4}ל"),
        vec![1],
        "גרשיים עבריים בשאילתה"
    );
    assert_eq!(
        advanced_ids(&mut engine, "הרמב\"ם"),
        vec![2],
        "״ עברי באינדקס, \" בשאילתה"
    );
    assert_eq!(
        advanced_ids(&mut engine, "הרמב''ם"),
        vec![2],
        "זוג גרשים בשאילתה (מוסכמת קבצים ישנים)"
    );
    assert_eq!(advanced_ids(&mut engine, "תוס'"), vec![3], "גרש סופי");
    assert_eq!(
        advanced_ids(&mut engine, "ג'ורג'"),
        vec![5],
        "גרש פנימי + סופי"
    );
    assert!(
        !advanced_ids(&mut engine, "רמב\"ם").contains(&4),
        "רמב ם כשתי מילים אינו צירוף מקרי של רמב\"ם"
    );
}

#[test]
fn exact_search_gershayim_token_is_a_single_term() {
    // כל צורות הדפוס של השאילתה מתלכדות לטרם `רמב"ם` אחד ומוצאות
    // מסמך שנדפס ב-״; צירוף מקרי `רמב ם` (שתי מילים) לא נתפס עוד —
    // זו בדיוק מטרת השינוי. המחיר המקובל (D1): שאילתה נטולת-גרשיים
    // לא מוצאת את המהדורה המנוקדת-בגרשיים בחיפוש מדויק.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "דברי רמב\u{05F4}ם בהלכות", "/books/a.txt");
    add(&mut engine, 2, "צירוף רמב ם מקרי", "/books/b.txt");
    engine.commit().unwrap();

    let exact_ids = |engine: &SearchEngine, query: &str| {
        ids(engine
            .search_exact(
                query.to_string(),
                vec![],
                100,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap())
    };

    for query in ["רמב\"ם", "רמב\u{05F4}ם", "רמב''ם"] {
        assert_eq!(exact_ids(&engine, query), vec![1], "query {query:?}");
    }
    // המחיר ההיסטורי של אופציה A (מדויק רגיש-גרשיים) בוטל: האינדקס
    // מטמיע לכל מילת-גרשיים גם טוקן-תאום נקי, כך ששאילתה נטולת-גרשיים
    // מוצאת את המהדורה המנוקדת-בגרשיים גם בחיפוש מדויק.
    assert_eq!(
        exact_ids(&engine, "רמבם"),
        vec![1],
        "הטוקן-התאום נטול-הגרשיים"
    );
    // ביטוי רב-מילים עם טוקן-גרש: PhraseQuery על הטרמים החדשים.
    add(&mut engine, 3, "דברי תוס' ד\"ה אמר שם", "/books/c.txt");
    engine.commit().unwrap();
    assert_eq!(exact_ids(&engine, "תוס' ד\"ה"), vec![3]);
}

#[test]
fn fuzzy_bridges_gershayim_and_clean_editions() {
    // הגישור המקורב: `"` = עריכת codepoint אחת, ו-Fix 2 מזריק את
    // הצורה הנקייה גם במרחק 0.
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "דברי רמב\"ם בהלכות", "/books/a.txt");
    add(&mut engine, 2, "דברי רמבם בהלכות", "/books/b.txt");
    engine.commit().unwrap();

    let fuzzy_ids = |engine: &SearchEngine, query: &str, d: u8| {
        ids(engine
            .search_fuzzy(
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                d,
                ResultsOrder::Relevance,
                false,
                false,
                None,
                None,
            )
            .unwrap())
    };

    // במרחק 1 — בשני הכיוונים.
    let got = fuzzy_ids(&engine, "רמב\"ם", 1);
    assert!(got.contains(&1) && got.contains(&2), "got {got:?}");
    let got = fuzzy_ids(&engine, "רמבם", 1);
    assert!(got.contains(&1) && got.contains(&2), "got {got:?}");
    // במרחק 0 — הודות לווריאנט הנקי (Fix 2).
    let got = fuzzy_ids(&engine, "רמב\"ם", 0);
    assert!(
        got.contains(&1) && got.contains(&2),
        "הווריאנט הנקי מגשר גם במרחק 0, got {got:?}"
    );
    // תקציב העריכה לא נבלע ע"י הגרשיים: רמכם→רמבם עריכה אחת — ומאז
    // שהאינדקס מטמיע טוקן-תאום נקי, הטרם רמבם קיים גם במסמך הגרשיים,
    // כך ששני המסמכים נתפסים כבר במרחק 1.
    let got = fuzzy_ids(&engine, "רמכם", 1);
    assert!(
        got.contains(&1) && got.contains(&2),
        "got {got:?}: רמכם→רמבם עריכה אחת, בשני המסמכים"
    );
    let got = fuzzy_ids(&engine, "רמכם", 2);
    assert!(got.contains(&1) && got.contains(&2), "got {got:?}");
}

#[test]
fn lexical_fuzzy_expands_quote_bearing_token() {
    // Fix 1 מקצה-לקצה: מפתח ה-lookup של טוקן-גרשיים פוגע ב-lexical.db
    // (אחרי מחיקת `"` ASCII), וההרחבה הלקסיקלית מזריקה קרובים שמרחק
    // העריכה לבדו לעולם לא היה תופס.
    let (mut engine, dir) = make_engine();
    assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));
    add(&mut engine, 1, "דברי אדמורים רבים כאן", "/books/a.txt");
    add(&mut engine, 2, "דברי אדמו\"ר אחד כאן", "/books/b.txt");
    engine.commit().unwrap();

    let got = ids(engine
        .search_fuzzy(
            "אדמו\"ר".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            1,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap());
    assert!(
        got.contains(&1),
        "אדמורים רחוק 3 עריכות — מושג רק דרך ההרחבה הלקסיקלית, got {got:?}"
    );
    assert!(got.contains(&2), "הטרם המדויק עצמו, got {got:?}");

    // הפער השיורי ההיסטורי (§5.2) נסגר: המילון פולט צורות נקיות בלבד,
    // אבל האינדקס מטמיע כעת טוקן-תאום נקי (אדמור) לצד אדמו"ר — כך
    // שהצורה הנקייה שהמילון מחזיר פוגעת בו ישירות.
    let got = ids(engine
        .search_fuzzy(
            "אדמורים".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            1,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap());
    assert!(got.contains(&1), "got {got:?}");
    assert!(
        got.contains(&2),
        "הטוקן-התאום סוגר את הפער השיורי, got {got:?}"
    );
}

#[test]
fn advanced_typo_and_prefix_flags_work_on_gershayim_tokens() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "דברי רמבם בהלכות", "/books/a.txt");
    add(&mut engine, 2, "כתב הרמב\"ם על כך", "/books/b.txt");
    engine.commit().unwrap();

    let search = |engine: &mut SearchEngine, query: &str, opt: &str| {
        let mut word_opts = HashMap::new();
        word_opts.insert(opt.to_string(), true);
        let mut options = HashMap::new();
        options.insert(format!("{query}_0"), word_opts);
        ids(search_advanced_default(
            engine,
            query.to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap())
    };

    // דגל typo: וריאנט-המחיקה של הגרפמה `"` מגשר למהדורות נקיות.
    let got = search(&mut engine, "רמב\"ם", hebrew_query::OPT_TYPO);
    assert!(got.contains(&1), "מחיקת `\"` מייצרת את רמבם, got {got:?}");
    // קידומות דקדוקיות סביב שורש עם `"` literal.
    let got = search(&mut engine, "רמב\"ם", "קידומות דקדוקיות");
    assert!(got.contains(&2), "ה־רמב\"ם עם קידומת, got {got:?}");
}

#[test]
fn advanced_aramaic_option_matches_prefixes_and_final_swaps() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "מלכא קדישא", "/books/a.txt");
    add(&mut engine, 2, "אמרו דמלכא הוא", "/books/b.txt");
    add(&mut engine, 3, "כדמלכה בשעתו", "/books/c.txt");
    add(&mut engine, 4, "מדמלכא נפק", "/books/d.txt");
    add(&mut engine, 5, "אדמלכה קאי", "/books/e.txt");
    add(&mut engine, 6, "חכמין אמרין", "/books/f.txt");
    add(&mut engine, 7, "ספרא אחרינא", "/books/g.txt");
    engine.commit().unwrap();

    let search = |engine: &mut SearchEngine, query: &str, opts: &[&str]| {
        let mut options = HashMap::new();
        if !opts.is_empty() {
            let word_opts: HashMap<String, bool> =
                opts.iter().map(|o| (o.to_string(), true)).collect();
            options.insert(format!("{query}_0"), word_opts);
        }
        ids(search_advanced_default(
            engine,
            query.to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap())
    };

    // שתי האפשרויות יחד — ההתנהגות ההיסטורית: שקילות סופית ה↔א +
    // הקידומות ד/כד/מד/אד על שני הווריאנטים.
    let got = search(&mut engine, "מלכה", &["קידומות ארמיות", "סיומות ארמיות"]);
    for id in [1, 2, 3, 4, 5] {
        assert!(got.contains(&id), "ארמית החמיצה את מסמך {id}, got {got:?}");
    }
    assert!(!got.contains(&7), "ארמית רחבה מדי, got {got:?}");

    // סיומות בלבד: השקילות עובדת (מלכא, id 1) אבל אין קידומות (לא 2-5).
    let got = search(&mut engine, "מלכה", &["סיומות ארמיות"]);
    assert!(
        got.contains(&1),
        "סיומות ארמיות החמיצו את מלכא, got {got:?}"
    );
    for id in [2, 3, 4, 5] {
        assert!(
            !got.contains(&id),
            "סיומות בלבד לא אמורות לתת קידומות (מסמך {id}), got {got:?}"
        );
    }

    // קידומות בלבד: דמלכה עם קידומת נתפס דרך וריאנט? לא — אין שקילות
    // סופית, אז רק צורות של "מלכה" עם קידומת; המסמכים כאן נושאים מלכא
    // חוץ מ-3 ו-5 (כדמלכה, אדמלכה).
    let got = search(&mut engine, "מלכה", &["קידומות ארמיות"]);
    for id in [3, 5] {
        assert!(
            got.contains(&id),
            "קידומות ארמיות החמיצו את מסמך {id}, got {got:?}"
        );
    }
    for id in [1, 2, 4] {
        assert!(
            !got.contains(&id),
            "קידומות בלבד לא אמורות לתת שקילות סופית (מסמך {id}), got {got:?}"
        );
    }

    // ם↔ן: חכמים מוצא חכמין דרך סיומות ארמיות.
    let got = search(&mut engine, "חכמים", &["סיומות ארמיות"]);
    assert!(got.contains(&6), "ם↔ן לא עבד, got {got:?}");

    // בלי האפשרויות — אין שקילות ארמית.
    let got = search(&mut engine, "מלכה", &[]);
    assert!(got.is_empty(), "בלי ארמית לא אמור להימצא דבר, got {got:?}");
}

#[test]
fn quote_free_indexing_and_ignore_quotes_option() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "כתב רמב\"ם על כך", "/books/a.txt");
    add(&mut engine, 2, "דברי רמבם בהלכות", "/books/b.txt");
    engine.commit().unwrap();

    let search = |engine: &mut SearchEngine, query: &str, opt: Option<&str>| {
        let mut options = HashMap::new();
        if let Some(opt) = opt {
            options.insert(
                format!("{query}_0"),
                HashMap::from([(opt.to_string(), true)]),
            );
        }
        ids(search_advanced_default(
            engine,
            query.to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap())
    };

    // בלי שום אפשרות: הצורה הנקייה מוצאת גם את המקור עם הגרשיים —
    // הטוקן-התאום שהאינדקס מטמיע.
    let got = search(&mut engine, "רמבם", None);
    assert!(got.contains(&1) && got.contains(&2), "got {got:?}");

    // רמב"ם בלי האפשרות: התנהגות היסטורית — רק הצורה עם הגרשיים.
    let got = search(&mut engine, "רמב\"ם", None);
    assert!(got.contains(&1) && !got.contains(&2), "got {got:?}");

    // עם "התעלם מגרשיים": שתי הצורות.
    let got = search(&mut engine, "רמב\"ם", Some("התעלם מגרשיים"));
    assert!(got.contains(&1) && got.contains(&2), "got {got:?}");

    // ההדגשה מכסה את הצורה המקורית עם הגרשיים (התאום יורש offsets).
    let results = search_advanced_default(
        &engine,
        "רמבם".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap();
    // הטקסט עובר escape של HTML — הגרשיים מופיעות כ-&quot;.
    let doc1 = results.iter().find(|r| r.id == 1).unwrap();
    assert!(
        doc1.text.contains("<font color=red>רמב&quot;ם</font>"),
        "highlight: {}",
        doc1.text
    );
}

#[test]
fn advanced_translation_option_expands_from_dictionary() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "איתא בגמרא", "/books/a.txt");
    add(&mut engine, 2, "יש דברים בגו", "/books/b.txt");
    engine.commit().unwrap();

    let mut dict_file = tempfile::NamedTempFile::new().unwrap();
    use std::io::Write as _;
    dict_file
        .write_all(r#"{ "מילון פשיטא": [ { "אִיתָא": "יש" } ] }"#.as_bytes())
        .unwrap();
    assert!(engine.set_translation_dictionary_path(dict_file.path().to_string_lossy().into_owned()));
    assert!(engine.has_translation_dictionary());

    let search = |engine: &mut SearchEngine, query: &str, opt: Option<&str>| {
        let mut options = HashMap::new();
        if let Some(opt) = opt {
            options.insert(
                format!("{query}_0"),
                HashMap::from([(opt.to_string(), true)]),
            );
        }
        ids(search_advanced_default(
            engine,
            query.to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap())
    };

    // עברי→ארמי: "יש" עם תרגום ארמי מוצא גם את "איתא".
    let got = search(&mut engine, "יש", Some("תרגום ארמי"));
    assert!(got.contains(&1) && got.contains(&2), "got {got:?}");
    // בלי האפשרות — אין הרחבה.
    let got = search(&mut engine, "יש", None);
    assert!(!got.contains(&1) && got.contains(&2), "got {got:?}");
    // ארמי→עברי.
    let got = search(&mut engine, "איתא", Some("תרגום ארמי"));
    assert!(got.contains(&1) && got.contains(&2), "got {got:?}");
}

#[test]
fn advanced_acronym_option_expands_bidirectionally() {
    let (mut engine, _dir) = make_engine();
    // id 1 — הר"ת עצמו (מאונדקס גם כ-"רמבם" דרך הטוקן-התאום).
    add(&mut engine, 1, "אמר רמב\"ם בהלכות", "/books/a.txt");
    // id 2 — הפענוח המלא ככתוב.
    add(
        &mut engine,
        2,
        "כתב רבי משה בן מיימון בספרו",
        "/books/b.txt",
    );
    // id 3 — לא קשור.
    add(&mut engine, 3, "דבר אחר לגמרי", "/books/c.txt");
    engine.commit().unwrap();

    let mut dict_file = tempfile::NamedTempFile::new().unwrap();
    use std::io::Write as _;
    dict_file
        .write_all(r#"{ "רמב\"ם": ["רבי משה בן מיימון"] }"#.as_bytes())
        .unwrap();
    assert!(engine.set_acronyms_dictionary_path(dict_file.path().to_string_lossy().into_owned()));
    assert!(engine.has_acronyms_dictionary());

    // האפשרות דלוקה על המילה במיקום `word_index`.
    let search = |engine: &SearchEngine, query: &str, opt_on_word: Option<(&str, usize)>| {
        let mut options = HashMap::new();
        if let Some((word, i)) = opt_on_word {
            options.insert(
                format!("{word}_{i}"),
                HashMap::from([("ראשי תיבות".to_string(), true)]),
            );
        }
        ids(search_advanced_default(
            engine,
            query.to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap())
    };

    // כיוון א' (ר"ת→פענוח): "רמב\"ם" עם האפשרות מוצא גם את הפענוח המלא.
    let got = search(&engine, "רמב\"ם", Some(("רמב\"ם", 0)));
    assert!(got.contains(&1) && got.contains(&2), "forward: {got:?}");
    assert!(!got.contains(&3), "forward must not over-match: {got:?}");
    // בלי האפשרות — רק ההתאמה הישירה.
    let got = search(&engine, "רמב\"ם", None);
    assert!(got.contains(&1) && !got.contains(&2), "no-opt: {got:?}");

    // כיוון ב' (פענוח→ר"ת): הביטוי המלא עם האפשרות מוצא גם את הר"ת.
    let got = search(&engine, "רבי משה בן מיימון", Some(("רבי", 0)));
    assert!(got.contains(&1) && got.contains(&2), "reverse: {got:?}");
    // בלי האפשרות — רק ההתאמה הישירה לביטוי.
    let got = search(&engine, "רבי משה בן מיימון", None);
    assert!(
        !got.contains(&1) && got.contains(&2),
        "reverse no-opt: {got:?}"
    );

    // הדגשה: מסמך שנמצא דרך החלופה נצבע — בשני הכיוונים.
    let texts = |query: &str, word: &str| -> Vec<String> {
        let options = HashMap::from([(
            format!("{word}_0"),
            HashMap::from([("ראשי תיבות".to_string(), true)]),
        )]);
        search_advanced_default(
            &engine,
            query.to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.text)
        .collect()
    };
    // ר"ת→פענוח: מילות הפענוח נצבעות במסמך 2.
    let got = texts("רמב\"ם", "רמב\"ם");
    assert!(
        got.iter().any(|t| t.contains("<font color=red>רבי</font>")
            && t.contains("<font color=red>מיימון</font>")),
        "forward highlight missing: {got:?}"
    );
    // וגם הר\"ת עצמו נצבע במסמך 1.
    assert!(
        got.iter().any(|t| t.contains("<font color=red>רמב")),
        "forward self-highlight missing: {got:?}"
    );
    // פענוח→ר"ת: הר"ת נצבע במסמך 1.
    let got = texts("רבי משה בן מיימון", "רבי");
    assert!(
        got.iter().any(|t| t.contains("<font color=red>רמב")),
        "reverse highlight missing: {got:?}"
    );
}

#[test]
fn test_search_fuzzy_high_level() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/a.txt");
    add(&mut engine, 2, "שלם", "/books/b.txt");
    add(&mut engine, 3, "ביי", "/books/c.txt");
    engine.commit().unwrap();

    let texts: Vec<String> = engine
        .search_fuzzy(
            "שלום".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            1,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.text)
        .collect();
    // Fuzzy matching is automaton-based, so highlighting must come from the
    // materialized highlight query: both the exact match and the variant one
    // edit away are wrapped, not just the literal word the user typed.
    assert!(texts.contains(&"<font color=red>שלום</font>".to_string()));
    assert!(texts.contains(&"<font color=red>שלם</font>".to_string()));
    assert!(!texts.iter().any(|t| t.contains("ביי")));
}

#[test]
fn test_search_fuzzy_invalid_distance_errors() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/a.txt");
    engine.commit().unwrap();

    // Tantivy supports edit distances 0–2; anything above must surface as
    // an error from every fuzzy entry point, never a panic.
    let result = engine.search_fuzzy(
        "שלום".to_string(),
        vec!["/root".to_string()],
        10,
        0,
        3,
        ResultsOrder::Relevance,
        false,
        false,
        None,
        None,
    );
    assert!(result.is_err(), "distance > 2 should error, not panic");

    let count = engine.count_fuzzy(
        "שלום".to_string(),
        vec!["/root".to_string()],
        3,
        false,
        false,
        None,
    );
    assert!(count.is_err(), "distance > 2 should error, not panic");
}

#[test]
fn test_search_fuzzy_highlights_near_match_in_context() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "אמר שלם לחברו", "/books/a.txt");
    engine.commit().unwrap();

    let results = engine
        .search_fuzzy(
            "שלום".to_string(),
            vec!["/root".to_string()],
            10,
            0,
            1,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].text, "אמר <font color=red>שלם</font> לחברו",
        "the fuzzy-matched variant must be highlighted inside the snippet"
    );
}

#[test]
fn test_search_fuzzy_empty_query_returns_no_results() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום", "/books/a.txt");
    engine.commit().unwrap();

    // Mirror exact mode: empty/punctuation-only fuzzy queries match
    // nothing instead of returning every document in the facets.
    for query in ["", "?!"] {
        let results = engine
            .search_fuzzy(
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                1,
                ResultsOrder::Relevance,
                false,
                false,
                None,
                None,
            )
            .unwrap();
        assert!(results.is_empty(), "query {query:?} should match nothing");
    }
}

#[test]
fn test_high_level_counts() {
    let (mut engine, _dir) = make_engine();
    add(&mut engine, 1, "שלום עולם", "/books/a.txt");
    add(&mut engine, 2, "שלום רב", "/books/a.txt");
    add(&mut engine, 3, "ביי", "/books/b.txt");
    engine.commit().unwrap();

    assert_eq!(
        engine
            .count_exact(
                "שלום".to_string(),
                vec!["/root".to_string()],
                false,
                false,
                None
            )
            .unwrap(),
        2
    );

    let by_book = engine
        .count_by_book_exact(
            "שלום".to_string(),
            vec!["/root".to_string()],
            false,
            false,
            None,
        )
        .unwrap();
    assert_eq!(by_book.get("/books/a.txt").copied(), Some(2));

    let page = engine
        .search_and_count_exact(
            "שלום".to_string(),
            vec!["/root".to_string()],
            1,
            0,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(page.total_count, 2);
    assert_eq!(page.results.len(), 1);
}

// ── חיפוש מנוקד (textVocalized) ─────────────────────────────────────

/// ספר קטן: שורה מנוקדת, שורה נטולת ניקוד עם אותן מילים, שורה עם
/// ניקוד+טעמים, ושורה מנוקדת עם קידומת.
fn make_vocalized_engine() -> (SearchEngine, TempDir) {
    let (mut engine, dir) = make_engine();
    let text = "<h1>בראשית</h1>\n\
                    בְּרֵאשִׁית בָּרָא אֱלֹהִים\n\
                    ובראשית ברא אלהים בלי ניקוד\n\
                    וַיֹּ\u{05A3}אמֶר אֱלֹהִים יְהִי אוֹר\n\
                    וּבָרָא עוֹלָם";
    engine
        .add_text_book_placed(
            "בראשית".to_string(),
            "/tanakh".to_string(),
            "/books/b.txt".to_string(),
            1,
            DEFAULT_GENERATION_ORDER,
            text.to_string(),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    (engine, dir)
}

#[test]
fn vocalized_exact_requires_typed_marks_frees_untyped() {
    let (engine, _dir) = make_vocalized_engine();
    // קמץ שהוקלד חייב; הדגש שלא הוקלד חופשי — בָרָא מוצא את בָּרָא.
    let hits = engine
        .search_exact(
            "בָרָא".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            true,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    // התוצאה מציגה את העותק המנוקד השמור.
    assert!(hits[0].text.contains("בָּרָא"), "text: {}", hits[0].text);

    // תנועה שגויה במקום קמץ — נפסל.
    let miss = engine
        .search_exact(
            "בֵרָא".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            true,
            false,
            None,
            None,
        )
        .unwrap();
    assert!(miss.is_empty());
}

#[test]
fn vocalized_flag_off_keeps_plain_behaviour() {
    let (engine, _dir) = make_vocalized_engine();
    // בלי דגלים: הניקוד מנורמל החוצה והחיפוש מוצא את שתי השורות.
    let plain = engine
        .search_exact(
            "ברא".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(plain.len(), 2);
    // עם דגל ניקוד ושאילתה לא מנוקדת: כל סימן חופשי, אך רק שורות
    // מנוקדות קיימות בשדה — השורה הנקייה לא נמצאת.
    let voc = engine
        .search_exact(
            "ברא".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            true,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(voc.len(), 1);
}

#[test]
fn plain_search_shows_vocalized_snippets_when_enabled() {
    let (engine, _dir) = make_vocalized_engine();
    let search = |query: &str| {
        let mut texts: Vec<String> = engine
            .search_exact(
                query.to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap()
            .into_iter()
            .map(|r| r.text)
            .collect();
        texts.sort();
        texts
    };
    let red = |word: &str| format!("<font color=red>{word}</font>");
    assert!(search("ברא")
        .iter()
        .all(|t| !hebrew_query::contains_attached_marks(t)));

    crate::vocalized_snippet::set_enabled(true);
    let texts = search("ברא");
    crate::vocalized_snippet::set_enabled(false);
    assert_eq!(
        texts,
        vec![
            format!("בְּרֵאשִׁית {} אֱלֹהִים", red("בָּרָא")),
            format!("ובראשית {} אלהים בלי ניקוד", red("ברא")),
        ]
    );
}

#[test]
fn vocalized_snippet_of_a_long_line_keeps_its_own_marks() {
    let (mut engine, _dir) = make_engine();
    let line = format!("{}בָּרָא שָׁלוֹם", "וַיִּבְרָא אֱלֹהִים ".repeat(80));
    engine
        .add_text_book_placed(
            "ספר".to_string(),
            "/x".to_string(),
            "/books/long.txt".to_string(),
            1,
            DEFAULT_GENERATION_ORDER,
            line,
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();

    crate::vocalized_snippet::set_enabled(true);
    let hits = engine
        .search_exact(
            "שלום".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    crate::vocalized_snippet::set_enabled(false);
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].text.ends_with("בָּרָא <font color=red>שָׁלוֹם</font>"),
        "text: {}",
        hits[0].text
    );
}

#[test]
fn vocalized_taamim_flag_splits_classes() {
    let (engine, _dir) = make_vocalized_engine();
    // שאילתה מנוקדת בלי טעמים מוצאת טקסט עם טעם (הטעם חופשי).
    let nikud_only = engine
        .search_exact(
            "וַיֹּאמֶר".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            true,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(nikud_only.len(), 1);
    // שאילתה עם הטעם שהוקלד ושני הדגלים — עדיין נמצא (הטעם קיים בטקסט).
    let with_taam = engine
        .search_exact(
            "וַיֹּ\u{05A3}אמֶר".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            true,
            true,
            None,
            None,
        )
        .unwrap();
    assert_eq!(with_taam.len(), 1);
    // טעם שגוי (זקף-קטן במקום מונח) עם דגל טעמים — נפסל.
    let wrong_taam = engine
        .search_exact(
            "וַיֹּ\u{0594}אמֶר".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            true,
            true,
            None,
            None,
        )
        .unwrap();
    assert!(wrong_taam.is_empty());
}

#[test]
fn vocalized_exact_phrase_matches_and_counts() {
    let (engine, _dir) = make_vocalized_engine();
    let page = engine
        .search_and_count_exact(
            "בָּרָא אֱלֹהִים".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            true,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(page.total_count, 1);
    assert_eq!(page.results.len(), 1);

    let by_book = engine
        .count_by_book_exact("בָּרָא".to_string(), vec![], true, false, None)
        .unwrap();
    assert_eq!(by_book.get("/books/b.txt"), Some(&1));
}

#[test]
fn vocalized_advanced_prefix_option_matches_prefixed_word() {
    let (engine, _dir) = make_vocalized_engine();
    let options = HashMap::from([(
        "בָרָא_0".to_string(),
        HashMap::from([("קידומות".to_string(), true)]),
    )]);
    let hits = search_advanced_default(
        &engine,
        "בָרָא".to_string(),
        vec![],
        10,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        options,
        ResultsOrder::Catalogue,
        true,
        false,
        SearchScope::WordDistance,
    )
    .unwrap();
    // גם בָּרָא וגם וּבָרָא (הקידומת המנוקדת בתוך חלון ה-prefix).
    assert_eq!(hits.len(), 2);
}

#[test]
fn per_word_nikud_option_routes_to_vocalized_field() {
    let (engine, _dir) = make_vocalized_engine();
    let search = |query: &str, options: HashMap<String, HashMap<String, bool>>| {
        search_advanced_default(
            &engine,
            query.to_string(),
            vec![],
            10,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            // הדגלים הגלובליים כבויים — הבקשה מגיעה מהאפשרות הפר-מילה.
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
    };
    // בלי האפשרות: מסלול רגיל, הניקוד נבלע בנרמול — מוצא את שתי השורות
    // (המנוקדת והלא-מנוקדת).
    assert_eq!(search("בָרָא", HashMap::new()).len(), 2);
    // עם "ניקוד" על המילה: רץ על השדה המנוקד ודורש את הקמץ שהוקלד —
    // רק השורה המנוקדת.
    let options = HashMap::from([(
        "בָרָא_0".to_string(),
        HashMap::from([(hebrew_query::OPT_MATCH_NIKUD.to_string(), true)]),
    )]);
    let hits = search("בָרָא", options);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].text.contains("בָּרָא"), "text: {}", hits[0].text);
    // תנועה שגויה עם האפשרות — נפסל.
    let options = HashMap::from([(
        "בֻרָא_0".to_string(),
        HashMap::from([(hebrew_query::OPT_MATCH_NIKUD.to_string(), true)]),
    )]);
    assert!(search("בֻרָא", options).is_empty());
}

#[test]
fn per_word_nikud_option_leaves_other_words_free() {
    let (engine, _dir) = make_vocalized_engine();
    // "ניקוד" מסומן רק על המילה הראשונה; השנייה מוקלדת בניקוד "שגוי"
    // בכוונה — הסימנים שלה חופשיים ולכן ההתאמה שורדת.
    let options = HashMap::from([(
        "בָּרָא_0".to_string(),
        HashMap::from([(hebrew_query::OPT_MATCH_NIKUD.to_string(), true)]),
    )]);
    let hits = search_advanced_default(
        &engine,
        "בָּרָא אֱלֹהִֻים".to_string(),
        vec![],
        10,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        options,
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].text.contains("בְּרֵאשִׁית"), "text: {}", hits[0].text);
}

#[test]
fn vocalized_fuzzy_finds_edit_distance_variants() {
    let (engine, _dir) = make_vocalized_engine();
    // ברח במרחק עריכה 1 מ-ברא; הווריאנט חופשי-סימנים מוצא את בָּרָא.
    let hits = engine
        .search_fuzzy(
            "בָּרַח".to_string(),
            vec![],
            10,
            0,
            1,
            ResultsOrder::Catalogue,
            true,
            false,
            None,
            None,
        )
        .unwrap();
    assert!(!hits.is_empty());
    // מרחק 0: רק הצורה המדויקת (עם הסימנים שהוקלדו) — ברח לא קיים.
    let exact_only = engine
        .search_fuzzy(
            "בָּרַח".to_string(),
            vec![],
            10,
            0,
            0,
            ResultsOrder::Catalogue,
            true,
            false,
            None,
            None,
        )
        .unwrap();
    assert!(exact_only.is_empty());
}

#[test]
fn vocalized_mark_order_is_canonical() {
    // דגש (ccc 21) לפני קמץ (ccc 18) שקול קנונית לקמץ לפני דגש.
    const DAGESH_FIRST: &str = "ב\u{05BC}\u{05B8}רָא";
    const QAMATS_FIRST: &str = "ב\u{05B8}\u{05BC}רָא";
    // מונח (ccc 220) לפני הדגש והקמץ.
    const TAAM_FIRST: &str = "ב\u{05A3}\u{05BC}\u{05B8}רָא";
    let lines = [
        format!("{DAGESH_FIRST} אֱלֹהִים"),
        format!("{QAMATS_FIRST} אֱלֹהִים"),
        "\u{FB31}\u{05B8}רָא אֱלֹהִים".to_string(),
        format!("{TAAM_FIRST} אֱלֹהִים"),
        "ברא אלהים".to_string(),
    ];
    let (mut engine, _dir) = make_engine();
    engine
        .add_text_book_placed(
            "ספר".to_string(),
            "/x".to_string(),
            "/books/order.txt".to_string(),
            1,
            DEFAULT_GENERATION_ORDER,
            lines.join("\n"),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();

    let segments = |hits: Vec<SearchResult>| {
        let mut segments: Vec<u64> = hits.iter().map(|h| h.segment).collect();
        segments.sort();
        segments
    };
    let exact = |query: &str, taamim: bool| {
        engine
            .search_exact(
                query.to_string(),
                vec![],
                20,
                0,
                ResultsOrder::Catalogue,
                true,
                taamim,
                None,
                None,
            )
            .unwrap()
    };
    let all_marked = vec![0, 1, 2, 3];
    for query in [DAGESH_FIRST, QAMATS_FIRST, "\u{FB31}\u{05B8}רָא"] {
        let hits = exact(query, false);
        for hit in &hits {
            let source = &lines[hit.segment as usize];
            let word = source.split(' ').next().unwrap();
            let word = hebrew_query::fold_presentation_forms(word);
            assert!(
                hit.text.contains(&format!("<font color=red>{word}</font>")),
                "query {query:?}, text: {}",
                hit.text
            );
        }
        assert_eq!(segments(hits), all_marked, "query {query:?}");
        assert_eq!(
            segments(exact(&format!("{query} אֱלֹהִים"), false)),
            all_marked,
            "phrase {query:?}"
        );
        assert_eq!(
            engine
                .count_exact(query.to_string(), vec![], true, false, None)
                .unwrap(),
            4,
            "count {query:?}"
        );
        let fuzzy = engine
            .search_fuzzy(
                query.to_string(),
                vec![],
                20,
                0,
                0,
                ResultsOrder::Catalogue,
                true,
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(segments(fuzzy), all_marked, "fuzzy {query:?}");
        let advanced = search_advanced_default(
            &engine,
            query.to_string(),
            vec![],
            20,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            true,
            false,
            SearchScope::WordDistance,
        )
        .unwrap();
        assert_eq!(segments(advanced), all_marked, "advanced {query:?}");
    }
    // עם טעמים: המונח שהוקלד נדרש בכל סדר שבו הוקלד.
    for query in [
        TAAM_FIRST,
        "ב\u{05BC}\u{05A3}\u{05B8}רָא",
        "ב\u{05B8}\u{05BC}\u{05A3}רָא",
    ] {
        assert_eq!(segments(exact(query, true)), vec![3], "taamim {query:?}");
    }
    assert_eq!(segments(exact(QAMATS_FIRST, true)), all_marked);
    // התוצאה מציגה את השורה בסדר הסימנים שבמקור.
    let hits = exact(DAGESH_FIRST, false);
    let qamats_line = hits.iter().find(|h| h.segment == 1).unwrap();
    assert!(
        qamats_line.text.contains(QAMATS_FIRST),
        "{}",
        qamats_line.text
    );
    assert!(
        !qamats_line.text.contains(DAGESH_FIRST),
        "{}",
        qamats_line.text
    );
}

#[test]
fn vocalized_search_ignores_cgj() {
    // ירושלַ͏ִם as the library spells it: CGJ between the patah and the hiriq, after a
    // meteg in one line and after a zaqef qatan on the lamed in the other.
    const METEG: &str = "י\u{05B0}ר\u{05BD}ו\u{05BC}ש\u{05B8}\u{05C1}ל\u{05B7}\u{034F}\u{05B4}ם";
    const ZAQEF: &str = "י\u{05B0}רו\u{05BC}ש\u{05B8}\u{05C1}ל\u{05B7}\u{0594}\u{034F}\u{05B4}ם";
    const CITY: &str = "ע\u{05B4}יר";
    let lines = [
        format!("{METEG} {CITY}"),
        format!("{ZAQEF} {CITY}"),
        "ירושלים עיר".to_string(),
    ];
    let (mut engine, _dir) = make_engine();
    engine
        .add_text_book_placed(
            "ספר".to_string(),
            "/x".to_string(),
            "/books/cgj.txt".to_string(),
            1,
            DEFAULT_GENERATION_ORDER,
            lines.join("\n"),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();

    let exact = |query: String| {
        let mut hits = engine
            .search_exact(
                query,
                vec![],
                20,
                0,
                ResultsOrder::Catalogue,
                true,
                false,
                None,
                None,
            )
            .unwrap();
        hits.sort_by_key(|h| h.segment);
        hits
    };
    for query in [
        "י\u{05B0}רו\u{05BC}ש\u{05B8}\u{05C1}ל\u{05B7}\u{05B4}ם",
        "י\u{05B0}רו\u{05BC}ש\u{05B8}\u{05C1}ל\u{05B4}\u{05B7}ם",
        "י\u{05B0}רו\u{05BC}ש\u{05B8}\u{05C1}ל\u{05B7}\u{034F}\u{05B4}ם",
    ] {
        let hits = exact(query.to_string());
        let segments: Vec<u64> = hits.iter().map(|h| h.segment).collect();
        assert_eq!(segments, [0, 1], "query {query:?}");
        for (hit, word) in hits.iter().zip([METEG, ZAQEF]) {
            assert!(
                hit.text.contains(&format!("<font color=red>{word}</font>")),
                "query {query:?}, text: {}",
                hit.text
            );
        }
        let phrase: Vec<u64> = exact(format!("{query} {CITY}"))
            .iter()
            .map(|h| h.segment)
            .collect();
        assert_eq!(phrase, [0, 1], "phrase {query:?}");
    }
}

// ── SearchScope: "באותה פסקה" / "תחת אותה כותרת" ─────────────────────

/// ספר עם שתי כותרות משנה: תחת "סימן א" המילים "מתכוין" ו"מתעסק"
/// מפוזרות על שורות שונות; תחת "סימן ב" שורה אחת מכילה את שתיהן —
/// בסדר הפוך לסדר השאילתה.
fn scope_engine() -> (SearchEngine, TempDir, u64) {
    let (mut engine, dir) = make_engine();
    let text = "<h1>ספר הבדיקה</h1>\n\
                    <h2>סימן א</h2>\n\
                    דין אינו מתכוין בשבת\n\
                    ודין מתעסק בחלבים ועריות\n\
                    <h2>סימן ב</h2>\n\
                    כאן נדון רק במלאכת שבת\n\
                    מתעסק וגם אינו מתכוין באותה שורה";
    engine
        .add_text_book_placed(
            "ספר הבדיקה".to_string(),
            "/root".to_string(),
            "/books/scope.txt".to_string(),
            7,
            0,
            text.to_string(),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    // ids: ((catalogue_order+1) << 32) + ordinal + 1
    let id_base = (7u64 + 1) << 32;
    (engine, dir, id_base)
}

fn scope_search(engine: &SearchEngine, query: &str, scope: SearchScope) -> Vec<u64> {
    search_advanced_default(
        engine,
        query.to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        scope,
    )
    .unwrap()
    .into_iter()
    .map(|r| r.id)
    .collect()
}

#[test]
fn same_paragraph_scope_matches_unordered_within_line() {
    let (engine, _dir, id_base) = scope_engine();
    // בסדר השאילתה אין התאמה בשום שורה — מסלול המרחק (סדר + צמידות)
    // לא מוצא דבר.
    assert_eq!(
        scope_search(&engine, "מתכוין מתעסק", SearchScope::WordDistance),
        Vec::<u64>::new()
    );
    // באותה פסקה: רק השורה שמכילה את שתי המילים, למרות הסדר ההפוך.
    assert_eq!(
        scope_search(&engine, "מתכוין מתעסק", SearchScope::SameParagraph),
        vec![id_base + 7]
    );
}

#[test]
fn same_section_scope_matches_across_lines_under_one_heading() {
    let (engine, _dir, id_base) = scope_engine();
    // "מתכוין" ו"מתעסק" בשורות שונות תחת "סימן א", ובאותה שורה תחת
    // "סימן ב" — חוזרות כל השורות שנושאות מילה מהשאילתה בתוך סעיף
    // שמכיל את כל המילים.
    assert_eq!(
        scope_search(&engine, "מתכוין מתעסק", SearchScope::SameSection),
        vec![id_base + 3, id_base + 4, id_base + 7]
    );
    // "בשבת" (סימן א) ו"נדון" (סימן ב) — אף סעיף לא מכיל את שתיהן.
    assert_eq!(
        scope_search(&engine, "בשבת נדון", SearchScope::SameSection),
        Vec::<u64>::new()
    );
}

#[test]
fn same_section_negative_scope_excludes_whole_section() {
    let (engine, _dir, id_base) = scope_engine();
    // "מתכוין" מופיע בסימן א (שורה 3) ובסימן ב (שורה 7). צירוף השלילה
    // "מתעסק בחלבים" נמצא בשורה *אחרת* של סימן א (שורה 4) — שלילה בטווח
    // "תחת אותה כותרת" חייבת לפסול את כל הסעיף, כולל שורות שאין בהן
    // אף מילת שלילה, ולהותיר רק את סימן ב.
    let ids: Vec<u64> = engine
        .search_advanced(
            "מתכוין".to_string(),
            "מתעסק בחלבים".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::SameSection,
            SearchScope::SameSection,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![id_base + 7]);

    // שלילה שאינה חותכת אף סעיף אינה משנה דבר.
    let ids: Vec<u64> = engine
        .search_advanced(
            "מתכוין".to_string(),
            "בחלבים נדון".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::SameSection,
            SearchScope::SameSection,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![id_base + 3, id_base + 7]);
}

#[test]
fn same_section_scope_count_matches_search() {
    let (engine, _dir, _id_base) = scope_engine();
    let count = count_advanced_default(
        &engine,
        "מתכוין מתעסק".to_string(),
        vec!["/root".to_string()],
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        false,
        false,
        SearchScope::SameSection,
    )
    .unwrap();
    assert_eq!(count, 3);

    let by_book = count_by_book_advanced_default(
        &engine,
        "מתכוין מתעסק".to_string(),
        vec!["/root".to_string()],
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        false,
        false,
        SearchScope::SameSection,
    )
    .unwrap();
    assert_eq!(by_book.get("/books/scope.txt"), Some(&3));
}

#[test]
fn same_paragraph_scope_snippet_highlights_every_word() {
    let (engine, _dir, _id_base) = scope_engine();
    let results = search_advanced_default(
        &engine,
        "מתכוין מתעסק".to_string(),
        vec!["/root".to_string()],
        100,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::SameParagraph,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    let text = &results[0].text;
    // שתי המילים מודגשות, בלי מסנן סדר/מרחק.
    assert!(
        text.contains("<font color=red>מתעסק</font>"),
        "snippet: {text}"
    );
    assert!(
        text.contains("<font color=red>מתכוין</font>"),
        "snippet: {text}"
    );
}

fn word_match_results(
    engine: &SearchEngine,
    query: &str,
    scope: SearchScope,
    mode: Option<WordMatchMode>,
    count: Option<u32>,
) -> Vec<SearchResult> {
    let negative_scope = same_search_scope(&scope);
    engine
        .search_advanced(
            query.to_string(),
            String::new(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            scope,
            negative_scope,
            None,
            mode,
            count,
            None,
            None,
        )
        .unwrap()
}

fn word_match_search(
    engine: &SearchEngine,
    query: &str,
    scope: SearchScope,
    mode: Option<WordMatchMode>,
    count: Option<u32>,
) -> Vec<u64> {
    word_match_results(engine, query, scope, mode, count)
        .into_iter()
        .map(|r| r.id)
        .collect()
}

#[test]
fn any_word_matches_lines_with_a_single_query_word() {
    let (engine, _dir, id_base) = scope_engine();
    // ברירת המחדל (כל המילים, מסלול המרחק) — אין שורה עם הצירוף.
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין בחלבים",
            SearchScope::WordDistance,
            None,
            None
        ),
        Vec::<u64>::new()
    );
    // "מילה אחת מספיקה": כל שורה שנושאת אחת מהמילים, בכל scope.
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין בחלבים",
            SearchScope::WordDistance,
            Some(WordMatchMode::AnyWord),
            None
        ),
        vec![id_base + 3, id_base + 4, id_base + 7]
    );
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין בחלבים",
            SearchScope::SameParagraph,
            Some(WordMatchMode::AnyWord),
            None
        ),
        vec![id_base + 3, id_base + 4, id_base + 7]
    );
}

#[test]
fn any_word_snippet_highlights_the_present_word() {
    let (engine, _dir, id_base) = scope_engine();
    let results = word_match_results(
        &engine,
        "מתכוין בחלבים",
        SearchScope::WordDistance,
        Some(WordMatchMode::AnyWord),
        None,
    );
    // שורה שנושאת רק מילה אחת — היא נצבעת, בלי מסנן סדר/מרחק.
    let first = results.iter().find(|r| r.id == id_base + 3).unwrap();
    assert!(
        first.text.contains("<font color=red>מתכוין</font>"),
        "snippet: {}",
        first.text
    );
}

#[test]
fn most_words_requires_a_majority() {
    let (engine, _dir, id_base) = scope_engine();
    // שלוש מילים — רוב = 2. שורה 3 נושאת רק "מתכוין" ונפסלת;
    // שורה 4 נושאת "מתעסק"+"בחלבים"; שורה 7 "מתעסק"+"מתכוין".
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין מתעסק בחלבים",
            SearchScope::WordDistance,
            Some(WordMatchMode::MostWords),
            None
        ),
        vec![id_base + 4, id_base + 7]
    );
}

#[test]
fn at_least_count_is_clamped_and_defaults_to_one() {
    let (engine, _dir, id_base) = scope_engine();
    let at_least = |count: Option<u32>| {
        word_match_search(
            &engine,
            "מתכוין מתעסק בחלבים",
            SearchScope::WordDistance,
            Some(WordMatchMode::AtLeast),
            count,
        )
    };
    // אף שורה לא נושאת את שלוש המילים.
    assert_eq!(at_least(Some(3)), Vec::<u64>::new());
    // מעל מספר המילים — נחתך ל"כולן".
    assert_eq!(at_least(Some(10)), Vec::<u64>::new());
    assert_eq!(at_least(Some(2)), vec![id_base + 4, id_base + 7]);
    // בלי ספירה (או 0) — מילה אחת.
    let any = vec![id_base + 3, id_base + 4, id_base + 7];
    assert_eq!(at_least(Some(1)), any);
    assert_eq!(at_least(None), any);
    assert_eq!(at_least(Some(0)), any);
}

#[test]
fn word_match_same_section_counts_distinct_words_per_section() {
    let (engine, _dir, id_base) = scope_engine();
    // "מתכוין בחלבים נדון", רוב = 2: סימן א נושא מתכוין+בחלבים,
    // סימן ב נושא נדון+מתכוין — שני הסעיפים עוברים, וחוזרות כל
    // השורות שנושאות מילת שאילתה בתוכם.
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין בחלבים נדון",
            SearchScope::SameSection,
            Some(WordMatchMode::MostWords),
            None
        ),
        vec![id_base + 3, id_base + 4, id_base + 6, id_base + 7]
    );
    // כל שלוש המילים — אף סעיף לא מכיל את שלושתן.
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין בחלבים נדון",
            SearchScope::SameSection,
            Some(WordMatchMode::AtLeast),
            Some(3)
        ),
        Vec::<u64>::new()
    );
}

#[test]
fn word_match_all_explicit_matches_default_behavior() {
    let (engine, _dir, id_base) = scope_engine();
    // Some(All) זהה ל-None — מסלול הביטוי הקיים נשאר בתוקף.
    assert_eq!(
        word_match_search(
            &engine,
            "אינו מתכוין",
            SearchScope::WordDistance,
            Some(WordMatchMode::All),
            None
        ),
        vec![id_base + 3, id_base + 7]
    );
    assert_eq!(
        word_match_search(
            &engine,
            "אינו מתכוין",
            SearchScope::WordDistance,
            None,
            None
        ),
        vec![id_base + 3, id_base + 7]
    );
}

#[test]
fn any_word_respects_the_negative_query() {
    let (engine, _dir, id_base) = scope_engine();
    // חיובי בהתאמה חלקית; השלילה נשארת "כל המילים" ופוסלת את שורה 3.
    let ids: Vec<u64> = engine
        .search_advanced(
            "מתכוין בחלבים".to_string(),
            "בשבת".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
            SearchScope::WordDistance,
            None,
            Some(WordMatchMode::AnyWord),
            None,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![id_base + 4, id_base + 7]);
}

/// שקילות בין מה שהחיפוש מוצא לבין מה שההדגשה בספר הפתוח מסמנת.
///
/// שני הצדדים סופרים "כמה מילים מותר בין מילות השאילתה": החיפוש לפי
/// עמדות הטוקנים באינדקס, וההדגשה לפי תבנית רגקס על טקסט התצוגה. סטייה
/// ביניהם מציגה למשתמש מילים מודגשות לצד "אין תוצאות" (או ההפוך).
#[test]
fn display_highlight_distance_matches_search_distance() {
    let (mut engine, _dir) = make_engine();
    // כל שורה: פער אחר בין "תדע" ל"זרעך" — כולל מקף, גרשיים וניקוד.
    let lines = [
        "תדע זרעך",
        "תדע אחת זרעך",
        "תדע אחת שתים זרעך",
        "תדע כי־גר יהיה זרעך",
        "תדע רמב\u{05F4}ם זרעך",
        "תֵּדַע יָדֹעַ זַרְעֲךָ",
    ];
    engine
        .add_text_book_placed(
            "ספר הבדיקה".to_string(),
            "/root".to_string(),
            "/books/highlight_parity.txt".to_string(),
            9,
            0,
            lines.join("\n"),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();

    for (index, line) in lines.iter().enumerate() {
        let mut found_by_search: Option<u32> = None;
        let mut found_by_highlight: Option<u32> = None;
        for distance in 0..=4u32 {
            if found_by_search.is_none() {
                let hits = search_advanced_default(
                    &engine,
                    "תדע זרעך".to_string(),
                    vec!["/root".to_string()],
                    100,
                    0,
                    distance,
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    SearchScope::WordDistance,
                )
                .unwrap();
                if hits.iter().any(|hit| hit.segment as usize == index) {
                    found_by_search = Some(distance);
                }
            }
            if found_by_highlight.is_none() {
                let highlight = crate::display_highlight::build_display_highlight(
                    "תדע זרעך",
                    distance,
                    &HashMap::new(),
                    &HashMap::new(),
                    &HashMap::new(),
                    false,
                )
                .unwrap();
                if !highlight.matcher.find_matches(line, &[]).is_empty() {
                    found_by_highlight = Some(distance);
                }
            }
        }
        assert_eq!(
            found_by_highlight, found_by_search,
            "שורה {line:?}: החיפוש וההדגשה חייבים להתאים באותו מרווח"
        );
    }
}

// ── Any-order word distance (Otzaria/otzaria#2006) ────────────────────

const ANY_ORDER_LINES: [&str; 7] = [
    "חייב להחזיר",
    "להחזיר חייב",
    "להחזיר את החפץ חייב",
    "להחזיר את החפץ הזה חייב",
    "חייב לשלם ולא להחזיר",
    "אמר רבי יוחנן",
    "אמר רבי אמר",
];

/// Line `i` lands at segment `2 * i`: filler lines keep the ordered
/// search's cross-line matches out of these tests.
fn lines_engine(lines: &[&str]) -> (SearchEngine, TempDir) {
    let (mut engine, dir) = make_engine();
    engine
        .add_text_book_placed(
            "ספר הסדר".to_string(),
            "/root".to_string(),
            "/books/any_order.txt".to_string(),
            9,
            0,
            lines.join("\nמילים אחרות לגמרי בשורה\n"),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    (engine, dir)
}

/// One advanced query, run through every public advanced entry point.
struct AdvancedCall<'a> {
    query: &'a str,
    negative: &'a str,
    distance: u32,
    custom_spacing: HashMap<String, String>,
    options: HashMap<String, HashMap<String, bool>>,
    any_order: Option<bool>,
}

impl<'a> AdvancedCall<'a> {
    fn new(query: &'a str, distance: u32, any_order: Option<bool>) -> Self {
        Self {
            query,
            negative: "",
            distance,
            custom_spacing: HashMap::new(),
            options: HashMap::new(),
            any_order,
        }
    }

    fn search(&self, engine: &SearchEngine) -> Vec<SearchResult> {
        self.search_in_order(engine, ResultsOrder::Catalogue)
    }

    fn search_in_order(&self, engine: &SearchEngine, order: ResultsOrder) -> Vec<SearchResult> {
        engine
            .search_advanced(
                self.query.to_string(),
                self.negative.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                self.distance,
                self.distance,
                self.custom_spacing.clone(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                self.options.clone(),
                HashMap::new(),
                order,
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
                None,
                None,
                None,
                self.any_order,
                None,
            )
            .unwrap()
    }

    fn lines(&self, engine: &SearchEngine) -> Vec<u64> {
        self.search(engine).iter().map(|r| r.segment / 2).collect()
    }

    fn count(&self, engine: &SearchEngine) -> u32 {
        engine
            .count_advanced(
                self.query.to_string(),
                self.negative.to_string(),
                vec!["/root".to_string()],
                self.distance,
                self.distance,
                self.custom_spacing.clone(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                self.options.clone(),
                HashMap::new(),
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
                None,
                None,
                self.any_order,
                None,
            )
            .unwrap()
    }

    fn count_by_book(&self, engine: &SearchEngine) -> u32 {
        engine
            .count_by_book_advanced(
                self.query.to_string(),
                self.negative.to_string(),
                vec!["/root".to_string()],
                self.distance,
                self.distance,
                self.custom_spacing.clone(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                self.options.clone(),
                HashMap::new(),
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
                None,
                None,
                self.any_order,
                None,
            )
            .unwrap()
            .values()
            .sum()
    }

    fn facet_count(&self, engine: &SearchEngine) -> u64 {
        engine
            .get_facet_counts_advanced(
                self.query.to_string(),
                self.negative.to_string(),
                vec!["/root".to_string()],
                "/".to_string(),
                self.distance,
                self.distance,
                self.custom_spacing.clone(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                self.options.clone(),
                HashMap::new(),
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
                None,
                None,
                self.any_order,
            )
            .unwrap()
            .iter()
            .filter(|facet| facet.path == "/root")
            .map(|facet| facet.count)
            .sum()
    }

    fn page_count(&self, engine: &SearchEngine) -> u32 {
        engine
            .search_and_count_advanced(
                self.query.to_string(),
                self.negative.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                self.distance,
                self.distance,
                self.custom_spacing.clone(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                self.options.clone(),
                HashMap::new(),
                ResultsOrder::Catalogue,
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
                None,
                None,
                None,
                self.any_order,
                None,
            )
            .unwrap()
            .total_count
    }
}

#[test]
fn any_order_finds_reversed_words_within_the_window_only() {
    let (engine, _dir) = lines_engine(&ANY_ORDER_LINES);
    let lines =
        |distance, any_order| AdvancedCall::new("חייב להחזיר", distance, any_order).lines(&engine);
    // Two words: at most `distance` words between them, in either order.
    assert_eq!(lines(0, Some(true)), [0, 1]);
    assert_eq!(lines(1, Some(true)), [0, 1]);
    assert_eq!(lines(2, Some(true)), [0, 1, 2, 4]);
    assert_eq!(lines(3, Some(true)), [0, 1, 2, 3, 4]);
    assert_eq!(
        AdvancedCall::new("להחזיר חייב", 2, Some(true)).lines(&engine),
        [0, 1, 2, 4]
    );
    // Three words: the window is what an ordered match may span, (k-1)·(d+1).
    assert_eq!(
        AdvancedCall::new("להחזיר החפץ חייב", 1, None).lines(&engine),
        [2, 3]
    );
    for query in ["להחזיר החפץ חייב", "חייב החפץ להחזיר"] {
        assert_eq!(
            AdvancedCall::new(query, 1, Some(true)).lines(&engine),
            [2, 3]
        );
        assert!(AdvancedCall::new(query, 0, Some(true))
            .lines(&engine)
            .is_empty());
    }
    // The ordered search is unchanged, whether the flag is off or absent.
    for any_order in [None, Some(false)] {
        assert_eq!(lines(0, any_order), [0]);
        assert_eq!(lines(3, any_order), [0, 4]);
    }
}

#[test]
fn any_order_relevance_ranks_tighter_windows_first() {
    let (engine, _dir) =
        lines_engine(&["להחזיר את החפץ הזה חייב", "להחזיר את חייב", "חייב להחזיר"]);
    let call = AdvancedCall::new("חייב להחזיר", 3, Some(true));
    let ranked: Vec<u64> = call
        .search_in_order(&engine, ResultsOrder::Relevance)
        .iter()
        .map(|r| r.segment / 2)
        .collect();
    assert_eq!(ranked, [2, 1, 0]);
    assert_eq!(call.lines(&engine), [0, 1, 2]);
}

#[test]
fn any_order_relevance_ranks_the_tightest_overlapping_window() {
    let (engine, _dir) = lines_engine(&["חייב את החפץ הזה להחזיר חייב", "חייב את להחזיר"]);
    let call = AdvancedCall::new("חייב להחזיר", 5, Some(true));
    let ranked: Vec<u64> = call
        .search_in_order(&engine, ResultsOrder::Relevance)
        .iter()
        .map(|r| r.segment / 2)
        .collect();
    assert_eq!(
        ranked,
        [0, 1],
        "the adjacent reversed match must outrank distance two"
    );
}

#[test]
fn any_order_gives_each_repeated_word_its_own_occurrence() {
    let (engine, _dir) = lines_engine(&ANY_ORDER_LINES);
    let lines = |query, distance| AdvancedCall::new(query, distance, Some(true)).lines(&engine);
    // Line 5 holds "אמר" once; line 6 twice, two positions apart.
    assert!(lines("אמר אמר", 0).is_empty());
    assert_eq!(lines("אמר אמר", 1), [6]);
    assert_eq!(lines("אמר רבי אמר", 0), [6]);
    assert_eq!(lines("רבי אמר אמר", 0), [6]);
    assert!(lines("רבי אמר רבי", 5).is_empty());
}

#[test]
fn any_order_is_ignored_with_custom_spacing() {
    let (engine, _dir) = lines_engine(&ANY_ORDER_LINES);
    let mut call = AdvancedCall::new("חייב להחזיר", 0, Some(true));
    call.custom_spacing = HashMap::from([("0-1".to_string(), "3".to_string())]);
    assert_eq!(call.lines(&engine), [0, 4]);
    let pattern = generate_highlight_pattern(
        "חייב להחזיר".to_string(),
        0,
        call.custom_spacing.clone(),
        HashMap::new(),
        HashMap::new(),
        Some(true),
    )
    .unwrap();
    let matcher = pattern.matcher.unwrap();
    assert!(matcher
        .find_matches(ANY_ORDER_LINES[2].to_string(), Vec::new())
        .is_empty());
    assert_eq!(
        matcher
            .find_matches(ANY_ORDER_LINES[4].to_string(), Vec::new())
            .len(),
        1
    );
}

#[test]
fn negative_query_follows_the_any_order_flag() {
    let (engine, _dir) = lines_engine(&ANY_ORDER_LINES);
    // "החפץ" is in lines 2 and 3; the negative words are reversed in both,
    // three positions apart in line 2 and four in line 3.
    let mut call = AdvancedCall::new("החפץ", 2, None);
    call.negative = "חייב להחזיר";
    assert_eq!(call.lines(&engine), [2, 3]);
    call.any_order = Some(true);
    assert_eq!(call.lines(&engine), [3]);
    assert_eq!(call.count(&engine), 1);
}

#[test]
fn any_order_counts_agree_with_search() {
    let (engine, _dir) = lines_engine(&ANY_ORDER_LINES);
    for (query, distance, any_order) in [
        ("חייב להחזיר", 0, Some(true)),
        ("חייב להחזיר", 2, Some(true)),
        ("להחזיר חייב", 3, Some(true)),
        ("להחזיר חייב", 3, None),
        ("אמר אמר", 1, Some(true)),
    ] {
        let call = AdvancedCall::new(query, distance, any_order);
        let found = call.search(&engine).len() as u32;
        let label = format!("{query:?} distance={distance} any_order={any_order:?}");
        assert_eq!(call.count(&engine), found, "{label}");
        assert_eq!(call.count_by_book(&engine), found, "{label}");
        assert_eq!(call.facet_count(&engine), found as u64, "{label}");
        assert_eq!(call.page_count(&engine), found, "{label}");
    }
}

/// The query word count a snippet paints, from its highlight markup.
fn painted_words(html: &str) -> Vec<String> {
    html.split("<font color=red>")
        .skip(1)
        .map(|rest| rest.split("</font>").next().unwrap().to_string())
        .collect()
}

/// Search, results snippet and both book highlighters agree on any-order
/// matches. With a prefix option every word keeps its own bounded affix
/// window — the engine's pattern — even when the caller waives boundaries.
#[test]
fn any_order_highlights_agree_with_search() {
    let lines = [
        "חייב להחזיר",
        "להחזיר חייב",
        "להחזיר את החפץ חייב",
        "להחזיר וחייב",
        "להחזיר אבגדחייב",
        "אבגדחייב להחזיר",
        "וחייב אחד להחזיר",
        "חייב",
    ];
    let (engine, _dir) = lines_engine(&lines);
    let prefix = HashMap::from([(
        "חייב_0".to_string(),
        HashMap::from([("קידומות".to_string(), true)]),
    )]);
    let mut prefixed = AdvancedCall::new("חייב להחזיר", 1, Some(true));
    prefixed.options = prefix.clone();
    // Four letters before the root exceed its prefix window, on either side.
    assert_eq!(prefixed.lines(&engine), [0, 1, 3, 6]);
    for options in [HashMap::new(), prefix] {
        for distance in 0..=2 {
            let mut call = AdvancedCall::new("חייב להחזיר", distance, Some(true));
            call.options = options.clone();
            let results = call.search(&engine);
            let index_pattern = engine
                .generate_index_highlight_pattern(
                    "חייב להחזיר".to_string(),
                    distance,
                    HashMap::new(),
                    HashMap::new(),
                    options.clone(),
                    Some(true),
                    None,
                )
                .unwrap()
                .unwrap()
                .matcher
                .unwrap();
            let plain_pattern = generate_highlight_pattern(
                "חייב להחזיר".to_string(),
                distance,
                HashMap::new(),
                HashMap::new(),
                options.clone(),
                Some(true),
            )
            .unwrap()
            .matcher
            .unwrap();
            for (index, line) in lines.iter().enumerate() {
                let label = format!("{line:?} distance={distance} options={options:?}");
                let hit = results.iter().find(|r| r.segment as usize == 2 * index);
                for matcher in [&index_pattern, &plain_pattern] {
                    for boundaries in [Vec::new(), vec![false, false]] {
                        assert_eq!(
                            !matcher
                                .find_matches(line.to_string(), boundaries)
                                .is_empty(),
                            hit.is_some(),
                            "{label}"
                        );
                    }
                }
                if let Some(hit) = hit {
                    assert_eq!(painted_words(&hit.text).len(), 2, "{label}: {}", hit.text);
                }
            }
        }
    }
}

#[test]
fn any_order_snippet_paints_the_reversed_occurrence_only() {
    let (engine, _dir) = lines_engine(&["חייב בדבר ואחר כך להחזיר חייב"]);
    // Only the last two words are within the window; the first "חייב" stays plain.
    let results = AdvancedCall::new("חייב להחזיר", 0, Some(true)).search(&engine);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].text,
        "חייב בדבר ואחר כך <font color=red>להחזיר</font> <font color=red>חייב</font>"
    );
}

#[test]
fn word_match_count_matches_search() {
    let (engine, _dir, _id_base) = scope_engine();
    let count = engine
        .count_advanced(
            "מתכוין בחלבים".to_string(),
            String::new(),
            vec!["/root".to_string()],
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            false,
            false,
            SearchScope::WordDistance,
            SearchScope::WordDistance,
            Some(WordMatchMode::AnyWord),
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn partial_modes_drop_order_even_when_the_threshold_is_all_words() {
    let (engine, _dir, id_base) = scope_engine();
    // שתי מילים: רוב = 2 = כולן — ובכל זאת הסדר אינו נדרש: שורה 7
    // נושאת אותן בסדר הפוך, ומסלול הביטוי (ברירת המחדל) מפספס אותה.
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין מתעסק",
            SearchScope::WordDistance,
            Some(WordMatchMode::MostWords),
            None
        ),
        vec![id_base + 7]
    );
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין מתעסק",
            SearchScope::WordDistance,
            Some(WordMatchMode::AtLeast),
            Some(2)
        ),
        vec![id_base + 7]
    );
}

#[test]
fn duplicate_query_words_count_once_toward_the_threshold() {
    let (engine, _dir, id_base) = scope_engine();
    // "מתכוין מתכוין בחלבים" עם סף 2 — שורה שנושאת רק "מתכוין" אינה
    // עוברת בזכות הכפילות, ואין שורה עם שתי המילים הייחודיות.
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין מתכוין בחלבים",
            SearchScope::WordDistance,
            Some(WordMatchMode::AtLeast),
            Some(2)
        ),
        Vec::<u64>::new()
    );
    // בטווח הסעיף: סימן א מכיל את שתי הייחודיות (בשורות שונות);
    // סימן ב, שבו רק "מתכוין", נפסל למרות הכפילות.
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין מתכוין בחלבים",
            SearchScope::SameSection,
            Some(WordMatchMode::AtLeast),
            Some(2)
        ),
        vec![id_base + 3, id_base + 4]
    );
}

#[test]
fn duplicate_words_with_different_options_still_count_once() {
    let (engine, _dir, _id_base) = scope_engine();
    // אפשרות פר-מילה על המופע הראשון בלבד נותנת לשני המופעים תבניות
    // שונות — ובכל זאת הם מילת-מקור אחת, והסף לא מסופק פעמיים.
    let options: HashMap<String, HashMap<String, bool>> = HashMap::from([(
        "מתכוין_0".to_string(),
        HashMap::from([("קידומות".to_string(), true)]),
    )]);
    let ids: Vec<u64> = engine
        .search_advanced(
            "מתכוין מתכוין בחלבים".to_string(),
            String::new(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            options,
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
            SearchScope::WordDistance,
            None,
            Some(WordMatchMode::AtLeast),
            Some(2),
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, Vec::<u64>::new());
}

#[test]
fn any_word_in_section_scope_matches_lines_with_any_word() {
    let (engine, _dir, id_base) = scope_engine();
    // סף 1 בטווח הסעיף שקול ל-OR שטוח (המימוש מדלג על ה-pre-pass).
    assert_eq!(
        word_match_search(
            &engine,
            "מתכוין בחלבים",
            SearchScope::SameSection,
            Some(WordMatchMode::AnyWord),
            None
        ),
        vec![id_base + 3, id_base + 4, id_base + 7]
    );
}

// ── Dimension facets (author/era/base) ────────────────────────────────

fn add_with_facets(
    engine: &mut SearchEngine,
    id: u64,
    text: &str,
    topics: &str,
    file_path: &str,
    extra: &[&str],
) {
    engine
        .add_document_at(
            id,
            "title",
            "ref",
            topics,
            text,
            0,
            false,
            file_path,
            None,
            None,
            Some(extra.iter().map(|s| s.to_string()).collect()),
        )
        .unwrap();
}

fn exact_count(engine: &SearchEngine, query: &str, facets: &[&str]) -> u32 {
    engine
        .count_exact(
            query.to_string(),
            facets.iter().map(|s| s.to_string()).collect(),
            false,
            false,
            None,
        )
        .unwrap()
}

#[test]
fn dimension_facets_or_within_and_across() {
    let (mut engine, _dir) = make_engine();
    // ספר א: קטגוריה x, ראשונים, רש"י. ספר ב: קטגוריה x, אחרונים.
    // ספר ג: קטגוריה y, ראשונים, וגם ספר-יסוד.
    add_with_facets(
        &mut engine,
        1,
        "דבר המלך",
        "/root/x",
        "a",
        &["/era/ראשונים", "/author/רשי"],
    );
    add_with_facets(
        &mut engine,
        2,
        "דבר המלך",
        "/root/x",
        "b",
        &["/era/אחרונים"],
    );
    add_with_facets(
        &mut engine,
        3,
        "דבר המלך",
        "/root/y",
        "c",
        &["/era/ראשונים", "/base"],
    );
    engine.commit().unwrap();

    // בלי פילטר — הכל.
    assert_eq!(exact_count(&engine, "המלך", &[]), 3);
    // ממד תקופה: OR בתוך הממד.
    assert_eq!(exact_count(&engine, "המלך", &["/era/ראשונים"]), 2);
    assert_eq!(
        exact_count(&engine, "המלך", &["/era/ראשונים", "/era/אחרונים"]),
        3
    );
    // תקופה AND קטגוריה.
    assert_eq!(
        exact_count(&engine, "המלך", &["/era/ראשונים", "/root/x"]),
        1
    );
    // מחבר AND תקופה סותרת — ריק.
    assert_eq!(
        exact_count(&engine, "המלך", &["/author/רשי", "/era/אחרונים"]),
        0
    );
    // ספרי יסוד.
    assert_eq!(exact_count(&engine, "המלך", &["/base"]), 1);
    // קטגוריות בלבד — ההתנהגות הישנה (OR בין קטגוריות).
    assert_eq!(exact_count(&engine, "המלך", &["/root/x", "/root/y"]), 3);
}

// ── Result grouping ────────────────────────────────────────────────────

#[test]
fn grouping_same_section_collapses_with_count() {
    let (mut engine, _dir) = make_engine();
    let text = "<h1>פרק א\n\
                    המלך דוד אמר שירה גדולה מאוד בלילה\n\
                    ועוד אמר המלך דוד דברי שירה נפלאים מאוד\n\
                    <h1>פרק ב\n\
                    המלך דוד לא אמר כאן דבר"
        .to_string();
    engine
        .add_text_book_placed(
            "תהלים".to_string(),
            "/root".to_string(),
            "book:a".to_string(),
            0,
            0,
            text,
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
    engine.commit().unwrap();

    let page = engine
        .search_and_count_exact(
            "שירה".to_string(),
            vec![],
            50,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            Some(ResultGrouping::SameSection),
            None,
        )
        .unwrap();
    // שתי שורות תואמות באותו סעיף — קבוצה אחת, מונה 2, אחות אחת.
    assert_eq!(page.total_count, 2, "raw hit count");
    assert_eq!(page.group_count, Some(1));
    assert_eq!(page.results.len(), 1);
    let rep = &page.results[0];
    assert_eq!(rep.merged_count, 2);
    assert_eq!(rep.merged.len(), 1);
    assert_eq!(rep.merged[0].title, "תהלים");
    // הנציג הוא המוקדם בסדר הקטלוג.
    assert!(rep.segment < rep.merged[0].segment);
    // בלי קיבוץ — שתי תוצאות שטוחות.
    let flat = engine
        .search_and_count_exact(
            "שירה".to_string(),
            vec![],
            50,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(flat.group_count, None);
    assert_eq!(flat.results.len(), 2);
    assert!(flat.results.iter().all(|r| r.merged_count == 1));
}

#[test]
fn grouping_identical_text_merges_across_books() {
    let (mut engine, _dir) = make_engine();
    // אותה שורה (ארוכה מסף החתימה) בשני ספרים; שורה שונה בספר שלישי.
    let mishna = "אמר רבי עקיבא כל ישראל יש להם חלק לעולם הבא";
    add(&mut engine, (1u64 << 32) + 1, mishna, "book:a");
    add(&mut engine, (2u64 << 32) + 1, mishna, "book:b");
    add(
        &mut engine,
        (3u64 << 32) + 1,
        "רבי עקיבא היה דורש כתרי אותיות של תורה",
        "book:c",
    );
    engine.commit().unwrap();

    let page = engine
        .search_and_count_exact(
            "עקיבא".to_string(),
            vec![],
            50,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            Some(ResultGrouping::IdenticalText),
            None,
        )
        .unwrap();
    assert_eq!(page.total_count, 3);
    assert_eq!(page.group_count, Some(2));
    // הרחק מתקרת הקבוצות — הדגל לא מורם.
    assert!(!page.truncated);
    assert_eq!(page.results.len(), 2);
    let merged = page
        .results
        .iter()
        .find(|r| r.merged_count == 2)
        .expect("merged group");
    assert_eq!(merged.merged.len(), 1);
    // הנציג והאחות מספרים שונים.
    assert_ne!(merged.file_path, merged.merged[0].file_path);

    // דפדוף נספר בקבוצות: עמוד שני של קבוצה-אחת-לכל-עמוד.
    let second = engine
        .search_and_count_exact(
            "עקיבא".to_string(),
            vec![],
            1,
            1,
            ResultsOrder::Catalogue,
            false,
            false,
            Some(ResultGrouping::IdenticalText),
            None,
        )
        .unwrap();
    assert_eq!(second.group_count, Some(2));
    assert_eq!(second.results.len(), 1);
}

#[test]
fn grouping_merges_across_segments_through_the_shared_accumulator() {
    let (mut engine, _dir) = make_engine();
    // commit בין הספרים ⇒ סגמנטים נפרדים; הצבירה המשותפת חייבת לאחד
    // את הקבוצה חוצת-הסגמנטים ולמנות את שני חבריה.
    let mishna = "אמר רבי עקיבא כל ישראל יש להם חלק לעולם הבא";
    add(&mut engine, (1u64 << 32) + 1, mishna, "book:a");
    engine.commit().unwrap();
    add(&mut engine, (2u64 << 32) + 1, mishna, "book:b");
    add(
        &mut engine,
        (3u64 << 32) + 1,
        "רבי עקיבא היה דורש כתרי אותיות של תורה",
        "book:c",
    );
    engine.commit().unwrap();

    let page = engine
        .search_and_count_exact(
            "עקיבא".to_string(),
            vec![],
            50,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            Some(ResultGrouping::IdenticalText),
            None,
        )
        .unwrap();
    assert_eq!(page.total_count, 3);
    assert_eq!(page.group_count, Some(2));
    assert!(!page.truncated);
    // הקבוצה המאוחדת נושאת את שני החברים משני הסגמנטים, והנציג הוא
    // בעל ה-id הנמוך (מהסגמנט הראשון).
    let merged = page
        .results
        .iter()
        .find(|r| r.merged_count == 2)
        .expect("merged group");
    assert_eq!(merged.id, (1u64 << 32) + 1);
    assert_eq!(merged.merged.len(), 1);
}

#[test]
fn grouping_identical_text_skips_short_lines() {
    let (mut engine, _dir) = make_engine();
    // שורה זהה קצרה מסף 12 האותיות — לעולם לא מתאחדת.
    add(&mut engine, (1u64 << 32) + 1, "אמר רבא", "book:a");
    add(&mut engine, (2u64 << 32) + 1, "אמר רבא", "book:b");
    engine.commit().unwrap();

    let page = engine
        .search_and_count_exact(
            "רבא".to_string(),
            vec![],
            50,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            Some(ResultGrouping::IdenticalText),
            None,
        )
        .unwrap();
    assert_eq!(page.total_count, 2);
    assert_eq!(page.group_count, Some(2));
    assert!(page.results.iter().all(|r| r.merged_count == 1));
}

#[test]
fn a_library_row_needs_its_line_hash_as_well_as_its_check() {
    use crate::line_source::RowText;
    let raw = "וַיֹּאמֶר אֱלֹהִים יְהִי אוֹר וַיְהִי אוֹר וירא אלהים";
    let check = crate::line_source::line_check(raw);
    let hash = line_dedup_hash(&hebrew_query::normalize_text_for_indexing(raw));
    assert_ne!(hash, 0);
    let row = |line_hash: u64, rendering: HitRendering| {
        HitText::from_library_row(
            RowText::Found(raw.to_string()),
            Some(check),
            line_hash,
            rendering,
        )
    };
    for rendering in [
        HitRendering::Plain,
        HitRendering::Vocalized,
        HitRendering::PlainAndVocalized,
    ] {
        assert_eq!(row(hash, rendering).status, TextStatus::Ok);
        // 0 is "no signature": only the check vouches for the row.
        assert_eq!(row(0, rendering).status, TextStatus::Ok);
        assert_eq!(row(hash ^ 1, rendering).status, TextStatus::Stale);
    }
    let shown = row(hash, HitRendering::Vocalized);
    assert!(shown.plain.is_empty() && shown.vocalized.is_some());
    let both = row(0, HitRendering::PlainAndVocalized);
    assert!(!both.plain.is_empty() && both.vocalized.is_some());
}

#[test]
fn line_dedup_hash_fast_paths_hash_like_the_unicode_tables() {
    fn reference(text: &str) -> u64 {
        let mut fnv = Fnv::new();
        let mut letters = 0usize;
        let mut buf = [0u8; 4];
        for c in text.chars() {
            if ('א'..='ת').contains(&c) {
                letters += 1;
            } else if !c.is_alphanumeric() {
                continue;
            }
            for lower in c.to_lowercase() {
                fnv.feed(lower.encode_utf8(&mut buf).as_bytes());
            }
        }
        if letters < LINE_DEDUP_MIN_LETTERS {
            return 0;
        }
        fnv.finish()
    }
    let prefix = "אבגדהוזחטיכלמנ";
    for c in (0u32..0x3000).filter_map(char::from_u32) {
        let text = format!("{prefix}{c}");
        assert_eq!(
            line_dedup_hash(&text),
            reference(&text),
            "U+{:04X}",
            c as u32
        );
    }
    let mixed = "וַיֹּ֥אמֶר אֱלֹהִ֖ים, Rabbi ÉLI 15 ١٥ \u{200E}ﬠ ײ׳ \"מ\" שָׁלוֹם";
    assert_eq!(line_dedup_hash(mixed), reference(mixed));
}

#[test]
fn line_dedup_hash_ignores_punctuation_and_spacing() {
    let a = line_dedup_hash("אמר רבי עקיבא, כל ישראל יש להם חלק!");
    let b = line_dedup_hash("אמר  רבי עקיבא כל ישראל — יש להם חלק");
    assert_ne!(a, 0);
    assert_eq!(a, b);
    // שינוי אות משנה חתימה.
    assert_ne!(a, line_dedup_hash("אמר רבי עקיבה כל ישראל יש להם חלק"));
    // קצר מדי — אין חתימה.
    assert_eq!(line_dedup_hash("אמר רבא"), 0);
}

#[test]
fn line_dedup_hash_distinguishes_digits_and_latin() {
    // שורות שנבדלות רק במספר אינן כפילות.
    let a = line_dedup_hash("יש לשלם מאה שקלים עד יום 15 בחודש");
    let b = line_dedup_hash("יש לשלם מאה שקלים עד יום 16 בחודש");
    assert_ne!(a, 0);
    assert_ne!(a, b);
    // וכך גם אותיות לטיניות...
    assert_ne!(
        line_dedup_hash("ראו במהדורת ניו יורק עמוד A דפוס ראשון"),
        line_dedup_hash("ראו במהדורת ניו יורק עמוד B דפוס ראשון"),
    );
    // ...אבל רישיות לטיניות מקופלות, כמו שאר הקיפול הטיפוגרפי.
    assert_eq!(
        line_dedup_hash("ראו במהדורת ניו יורק עמוד a דפוס ראשון"),
        line_dedup_hash("ראו במהדורת ניו יורק עמוד A דפוס ראשון"),
    );
    // אלפאנומרי משתתף בחתימה אך לא נספר לסף האותיות העבריות.
    assert_eq!(line_dedup_hash("1234567890 abcdef אמר רבא"), 0);
    // אלפאנומרי שאינו ASCII משתתף גם הוא: ספרות ערביות-הודיות ואותיות
    // לטיניות עם סימנים מבדילים שורות, בקיפול רישיות יוניקודי.
    assert_ne!(
        line_dedup_hash("יש לשלם מאה שקלים עד יום ١٥ בחודש"),
        line_dedup_hash("יש לשלם מאה שקלים עד יום ١٦ בחודש"),
    );
    assert_ne!(
        line_dedup_hash("ראו במהדורת פריז עמוד é דפוס ראשון"),
        line_dedup_hash("ראו במהדורת פריז עמוד è דפוס ראשון"),
    );
    assert_eq!(
        line_dedup_hash("ראו במהדורת פריז עמוד É דפוס ראשון"),
        line_dedup_hash("ראו במהדורת פריז עמוד é דפוס ראשון"),
    );
}

#[cfg(feature = "semantic-integration")]
#[test]
fn line_dedup_hashes_are_the_hashes_of_the_parts_joined() {
    let parts = ["אמר", "רבי עקיבא,", "", "כל ישראל", "יש להם חלק 15", "A"];
    let hashes: Vec<u64> = line_dedup_hashes(&parts).collect();
    for (at, hash) in hashes.iter().enumerate() {
        assert_eq!(*hash, line_dedup_hash(&parts[..=at].join(" ")), "{at}");
    }
    assert_eq!(hashes[2], 0);
    assert_ne!(hashes[3], 0);
}

#[test]
fn bounded_groups_cap_keeps_counting_existing_groups() {
    // מילוי התקרה; קבוצה קיימת ממשיכה לצבור גם בתקרה, וקבוצות גרועות
    // מכל הקיימות נדחות עם דגל.
    let mut bounded = BoundedGroups::new();
    let cap = GROUP_COLLECTOR_MAX_GROUPS as u64;
    for i in 0..cap {
        bounded.add_entry((1u8, i), (i, i, DocAddress::new(0, i as u32), i));
    }
    bounded.add_entry((1u8, 0), (0, 0, DocAddress::new(1, 0), 0));
    for i in cap..cap + 10 {
        bounded.add_entry((1u8, i), (i, i, DocAddress::new(2, (i % 1000) as u32), i));
    }
    assert!(bounded.truncated);
    assert_eq!(bounded.groups.len(), GROUP_COLLECTOR_MAX_GROUPS);
    // קבוצה קיימת ממשיכה להיספר גם בתקרה — המונה שלה מדויק.
    assert_eq!(bounded.groups[&(1u8, 0)].count, 2);
}

#[test]
fn bounded_groups_keep_the_best_groups_in_any_arrival_order() {
    // מילוי התקרה בקבוצות "גרועות" (sort גבוה); קבוצה טובה שמגיעה
    // אחריהן חייבת להיכנס על חשבון הגרועה ביותר — סדר הסריקה בין
    // הסגמנטים אינו קובע אילו קבוצות שורדות.
    let mut bounded = BoundedGroups::new();
    let cap = GROUP_COLLECTOR_MAX_GROUPS as u64;
    let high_base = 1_000_000u64;
    for i in high_base..high_base + cap {
        bounded.add_entry((1u8, i), (i, i, DocAddress::new(0, (i % 1000) as u32), i));
    }
    bounded.add_entry((1u8, 1), (1, 1, DocAddress::new(1, 1), 1));
    assert!(bounded.truncated);
    assert_eq!(bounded.groups.len(), GROUP_COLLECTOR_MAX_GROUPS);
    // הטובה נכנסה, הגרועה ביותר פונתה.
    assert!(bounded.groups.contains_key(&(1u8, 1)));
    assert!(!bounded.groups.contains_key(&(1u8, high_base + cap - 1)));
    // והעמוד הראשון מתחיל בקבוצה הטובה ביותר.
    let hits = GroupedHits {
        raw_total: cap as u32 + 1,
        truncated: bounded.truncated,
        groups: bounded.groups,
    };
    let page = SearchEngine::finalize_grouped(hits, 1, 0);
    assert_eq!(page.reps[0].id, 1);
    assert!(page.truncated);
}

#[test]
fn accumulate_section_counts_respects_the_budget() {
    let mut counts: HashMap<u64, usize> = HashMap::new();
    // המילה הראשונה ממלאה את התקציב; סעיף חדש מהמילה השנייה נשמט עם
    // דגל, אבל סעיף שכבר נספר ממשיך להצטבר.
    let truncated = accumulate_section_counts(&mut counts, (0..3u64).collect(), 3);
    assert!(!truncated);
    let truncated = accumulate_section_counts(&mut counts, HashSet::from([1u64, 99]), 3);
    assert!(truncated);
    assert_eq!(counts.len(), 3);
    assert_eq!(counts[&1], 2);
    assert!(!counts.contains_key(&99));
}

#[test]
fn bounded_groups_add_entry_evicts_worst_group() {
    // מסלול האיסוף פר-סגמנט: מילוי התקרה במסמכים "גרועים", ואז מסמך
    // טוב — הקבוצה שלו נכנסת; מסמך גרוע מהגרועה ביותר — נדחה.
    let mut bounded = BoundedGroups::new();
    for i in 0..GROUP_COLLECTOR_MAX_GROUPS as u64 {
        let sort = 1000 + i;
        bounded.add_entry(
            (1u8, sort),
            (sort, sort, DocAddress::new(0, i as u32), sort),
        );
    }
    assert!(!bounded.truncated);
    bounded.add_entry((1u8, 5), (5, 5, DocAddress::new(0, 1), 5));
    assert!(bounded.truncated);
    assert!(bounded.groups.contains_key(&(1u8, 5)));
    let worst_key = (1u8, 1000 + GROUP_COLLECTOR_MAX_GROUPS as u64 - 1);
    assert!(!bounded.groups.contains_key(&worst_key));
    // מסמך לקבוצה קיימת נצבר גם בתקרה, והמונה מדויק.
    bounded.add_entry((1u8, 5), (6, 6, DocAddress::new(0, 2), 6));
    assert_eq!(bounded.groups[&(1u8, 5)].count, 2);
    // קבוצה חדשה גרועה מכולן — נדחית.
    bounded.add_entry(
        (1u8, 999_999),
        (999_999, 999_999, DocAddress::new(0, 3), 999_999),
    );
    assert!(!bounded.groups.contains_key(&(1u8, 999_999)));
    assert_eq!(bounded.groups.len(), GROUP_COLLECTOR_MAX_GROUPS);
}

/// [`SemanticConfigKey`] and the mapping [`SearchEngine::configure_semantic`] performs.
///
/// Nothing here loads a model — configuring is lazy — so every build with the
/// integration runs it, whichever backends it has, including none.
#[cfg(feature = "semantic-integration")]
mod semantic_configuration {
    use super::*;
    use otzaria_semantic_search::semantic::chunker::ChunkerConfig;
    use otzaria_semantic_search::semantic::recipe::EmbeddingTextRecipe;

    /// What the sidecar writes when it opens, and so the witness of an open.
    const MANIFEST: &str = "semantic_manifest.json";

    /// A field's name, and one edit of that field alone.
    type FieldEdit = (&'static str, fn(&mut SemanticConfigInput));

    /// The Meivin ONNX identity the application uses, the INT8 graph, rooted in `root`.
    fn meivin(root: &Path) -> SemanticConfigInput {
        SemanticConfigInput {
            root_dir: root.join("semantic").to_string_lossy().into_owned(),
            model_path: root
                .join("seforim-embed-round2-int8.onnx")
                .to_string_lossy()
                .into_owned(),
            model_id: "ArieLLL123/judaic-semantic-round2-onnx-zayit".to_string(),
            embedding_dim: 256,
            pooling: "in-graph".to_string(),
            max_tokens: 256,
            model_quantization: "int8".to_string(),
            embedding_text_version: 2,
            onnx_runtime_path: None,
        }
    }

    /// The full-precision graph published beside it, rooted in `root`: an identity of its
    /// own, which differs from [`meivin`] in the graph and the quantization alone.
    fn meivin_fp32(root: &Path) -> SemanticConfigInput {
        SemanticConfigInput {
            model_path: root
                .join("seforim-embed-round2-fp32.onnx")
                .to_string_lossy()
                .into_owned(),
            model_quantization: "fp32".to_string(),
            ..meivin(root)
        }
    }

    fn manifest(root_dir: &str) -> JsonValue {
        let path = Path::new(root_dir).join(MANIFEST);
        let text =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn an_identical_configuration_is_the_same_key_and_names_no_change() {
        let root = Path::new("/library");
        let active = SemanticConfigKey::from_input(&meivin(root));
        let repeat = SemanticConfigKey::from_input(&meivin(root));

        assert_eq!(active, repeat);
        assert_eq!(active.changed_fields(&repeat), "");
    }

    /// One row per field of [`SemanticConfigInput`]: a key that left one out would
    /// answer a request for different vectors with a no-op.
    #[test]
    fn a_change_to_any_single_input_is_detected_and_named() {
        let root = Path::new("/library");
        let active = SemanticConfigKey::from_input(&meivin(root));
        let changes: [FieldEdit; 9] = [
            ("root_dir", |c| c.root_dir.push_str("-elsewhere")),
            ("model_path", |c| c.model_path.push_str(".bak")),
            ("model_id", |c| c.model_id.push_str("-v2")),
            ("embedding_dim", |c| c.embedding_dim = 768),
            // Compared as given: whether a backend performs it is the sidecar's to say.
            ("pooling", |c| c.pooling = "mean".to_string()),
            ("max_tokens", |c| c.max_tokens = 512),
            ("model_quantization", |c| {
                c.model_quantization = "fp32".to_string()
            }),
            ("embedding_text_version", |c| c.embedding_text_version = 1),
            // Not identity, and a change all the same: the process keeps the first
            // runtime it loads.
            ("onnx_runtime_path", |c| {
                c.onnx_runtime_path = Some("/app/Frameworks/libonnxruntime.dylib".into())
            }),
        ];

        for (field, change) in changes {
            let mut input = meivin(root);
            change(&mut input);
            let requested = SemanticConfigKey::from_input(&input);
            assert_ne!(active, requested, "{field} must be part of the key");
            assert_eq!(active.changed_fields(&requested), field);
        }
    }

    #[test]
    fn switching_models_names_every_input_that_differs() {
        let root = Path::new("/library");
        let int8 = SemanticConfigKey::from_input(&meivin(root));
        let fp32 = SemanticConfigKey::from_input(&meivin_fp32(root));
        assert_eq!(int8.changed_fields(&fp32), "model_path, model_quantization");

        // Another encoder altogether: every input but the root differs, and each is named,
        // in the order the configuration declares them.
        let other = SemanticConfigKey::from_input(&SemanticConfigInput {
            model_path: root
                .join("another-encoder.onnx")
                .to_string_lossy()
                .into_owned(),
            model_id: "example/another-encoder".to_string(),
            embedding_dim: 768,
            pooling: "mean".to_string(),
            max_tokens: 512,
            model_quantization: "fp32".to_string(),
            embedding_text_version: 1,
            onnx_runtime_path: Some("/app/Frameworks/libonnxruntime.dylib".to_string()),
            ..meivin(root)
        });
        assert_eq!(
            int8.changed_fields(&other),
            "model_path, model_id, embedding_dim, pooling, max_tokens, \
                 model_quantization, embedding_text_version, onnx_runtime_path"
        );
    }

    /// Both halves of the contract, through the method itself. Opening the sidecar
    /// writes its manifest, so the manifest is deleted after the first call: a repeat
    /// or a refusal that re-opened anything would write it back.
    #[test]
    fn configure_semantic_ignores_a_repeat_and_refuses_a_change_by_name() {
        let (mut engine, _index) = make_engine();
        let semantic = TempDir::new().unwrap();
        let config = || meivin(semantic.path());
        engine
            .configure_semantic(config())
            .expect("the Meivin identity configures");
        let manifest = Path::new(&config().root_dir).join(MANIFEST);
        assert!(manifest.exists(), "opening the sidecar writes its manifest");
        fs::remove_file(&manifest).unwrap();

        let status = engine
            .configure_semantic(config())
            .expect("an identical repeat is accepted");
        assert!(status.enabled);
        assert!(!manifest.exists(), "an identical repeat must not re-open");

        let changes: [FieldEdit; 4] = [
            ("pooling", |c| c.pooling = "mean".to_string()),
            ("max_tokens", |c| c.max_tokens = 512),
            ("model_quantization", |c| {
                c.model_quantization = "fp32".to_string()
            }),
            ("embedding_text_version", |c| c.embedding_text_version = 1),
        ];
        for (field, change) in changes {
            let mut input = config();
            change(&mut input);
            let error = match engine.configure_semantic(input) {
                Ok(_) => panic!("changing {field} while a session is open must fail"),
                Err(error) => error,
            };
            assert!(
                error.message.contains(&format!("and {field} changed")),
                "the refusal must name {field}: {}",
                error.message
            );
            assert_eq!(error.kind, SemanticErrorKind::SessionConflict, "{field}");
        }
        assert!(
            !manifest.exists(),
            "a refused change must not re-open either"
        );
    }

    /// The mapping, read back from what the sidecar recorded. The model id, dimension,
    /// pooling, cap and quantization all differ from the sidecar's own defaults, so an
    /// input left to its default shows up as the default. The text version is recorded
    /// only inside the chunking identity, so there is one configuration per recipe the
    /// sidecar implements, and each must record its own.
    #[test]
    fn configure_semantic_hands_each_input_to_the_sidecar() {
        let (mut engine, _index) = make_engine();
        for recipe in EmbeddingTextRecipe::ALL {
            let semantic = TempDir::new().unwrap();
            let input = SemanticConfigInput {
                embedding_text_version: recipe.version(),
                ..meivin(semantic.path())
            };
            let root_dir = input.root_dir.clone();
            engine
                .configure_semantic(input)
                .unwrap_or_else(|error| panic!("text recipe {}: {error:#}", recipe.version()));

            let recorded = manifest(&root_dir);
            assert_eq!(
                recorded["embedding_model_id"],
                "ArieLLL123/judaic-semantic-round2-onnx-zayit"
            );
            assert_eq!(recorded["embedding_dim"], 256);
            assert_eq!(recorded["pooling"], "in-graph");
            assert_eq!(recorded["embedding_max_tokens"], 256);
            assert_eq!(recorded["model_quantization"], "int8");
            let chunking = ChunkerConfig {
                embedding_text_version: recipe.version(),
                ..ChunkerConfig::default()
            };
            assert_eq!(recorded["chunking_identity"], chunking.identity());

            engine.disable_semantic();
        }
    }

    /// A refusal is what proves a value reaches the sidecar: every default the
    /// sidecar would otherwise fill in is valid, so only a forwarded value can be
    /// refused. A refused configuration leaves no session behind.
    #[test]
    fn a_value_the_sidecar_cannot_serve_is_refused_when_configuring() {
        let (mut engine, _index) = make_engine();
        let semantic = TempDir::new().unwrap();
        let spoiled: [FieldEdit; 5] = [
            ("last_token", |c| c.pooling = "last_token".to_string()),
            ("embedding_max_tokens", |c| c.max_tokens = 1),
            ("model_quantization", |c| {
                c.model_quantization = " ".to_string()
            }),
            ("embedding_text_version", |c| c.embedding_text_version = 99),
            // Refused here rather than as a runtime the first indexing cannot find.
            ("onnx_runtime_path", |c| {
                c.onnx_runtime_path = Some(String::new())
            }),
        ];

        for (named, spoil) in spoiled {
            let mut input = meivin(semantic.path());
            spoil(&mut input);
            let error = match engine.configure_semantic(input) {
                Ok(_) => panic!("a configuration with a bad {named} must be refused"),
                Err(error) => error,
            };
            assert!(
                error.message.contains(named),
                "the refusal must name {named}: {}",
                error.message
            );
            assert_eq!(error.kind, SemanticErrorKind::InvalidInput, "{named}");
        }
        engine
            .configure_semantic(meivin(semantic.path()))
            .expect("a refused configuration leaves no session behind");
    }

    /// The ONNX token cap has a ceiling, 65,536, and it binds before the manifest
    /// records the cap. `u32::MAX` is the case it exists for: a negative Dart value
    /// arrives as a cap in the billions, and the load-time probe of that many tokens
    /// cannot be allocated. The ceiling itself is accepted.
    #[test]
    fn an_onnx_cap_past_the_ceiling_is_refused_when_configuring() {
        const CEILING: u32 = 65_536;
        let (mut engine, _index) = make_engine();

        for cap in [CEILING + 1, u32::MAX] {
            let semantic = TempDir::new().unwrap();
            let input = SemanticConfigInput {
                max_tokens: cap,
                ..meivin(semantic.path())
            };
            let root_dir = input.root_dir.clone();
            let error = match engine.configure_semantic(input) {
                Ok(_) => panic!("an ONNX cap of {cap} must be refused"),
                Err(error) => error,
            };
            let message = &error.message;
            assert!(
                message.contains(&format!("embedding_max_tokens is {cap}"))
                    && message.contains(&CEILING.to_string()),
                "the refusal must name the cap and the ceiling: {message}"
            );
            assert_eq!(error.kind, SemanticErrorKind::InvalidInput);
            assert!(
                !Path::new(&root_dir).join(MANIFEST).exists(),
                "a refused cap must not be recorded as an index's identity"
            );
        }

        let root = TempDir::new().unwrap();
        engine
            .configure_semantic(SemanticConfigInput {
                max_tokens: CEILING,
                ..meivin(root.path())
            })
            .unwrap_or_else(|error| panic!("a cap of {CEILING}: {error:#}"));
    }
}

/// [`SemanticRankingOptions`] as the sidecar's `RankingProfile`, and what the sidecar's rules
/// refuse of it. Nothing here searches, so every build with the integration runs it.
#[cfg(feature = "semantic-integration")]
mod semantic_ranking {
    use super::*;

    /// The options a test changes one field of.
    fn defaults() -> SemanticRankingOptions {
        SemanticRankingOptions::defaults()
    }

    /// The defaults are the `Balanced` preset with RRF, a threshold at about cosine 0.1, and
    /// the foundational books' preference on; a search passed no options ranks by them.
    #[test]
    fn the_defaults_are_balanced_with_rrf_and_the_foundational_preference() {
        assert_eq!(
            ranking_profile(&defaults()).unwrap(),
            RankingProfile {
                fusion_strategy: FusionStrategy::RRF { k: 60 },
                semantic_threshold: 0.55,
                foundational_bonus: 0.002,
                foundational_candidate_share: 0.5,
                ..RankingProfile::from_profile(SearchProfile::Balanced)
            }
        );
    }

    /// Each option lands on the profile field of its name, narrowed to 32 bits: values
    /// that differ from one another and from the defaults, so a swap or a dropped field
    /// shows.
    #[test]
    fn each_option_reaches_the_profile_field_it_names() {
        let options = SemanticRankingOptions {
            fusion_strategy: SemanticFusionStrategy::Rrf,
            rrf_k: 30,
            alpha_override: Some(0.25),
            alpha_by_query_type: SemanticQueryTypeAlphas {
                quoted_phrase: 0.9,
                exact_reference: 0.8,
                short: 0.6,
                mixed: 0.4,
                conceptual: 0.2,
                unknown: 0.1,
            },
            bm25_saturation_k: 5.0,
            semantic_threshold: 0.55,
            agreement_bonus: 0.2,
            phrase_match_bonus: 0.15,
            rare_term_bonus: 0.05,
            section_coverage_bonus: 0.03,
            duplicate_penalty: 0.08,
            metadata_ranking_enabled: true,
            candidate_window_multiplier: 3.0,
            foundational_bonus: 0.004,
            foundational_candidate_share: 0.25,
            window_penalty: 0.03,
            alias_penalty: 0.05,
        };
        let preset = RankingProfile::from_profile(SearchProfile::Balanced);
        assert_eq!(
            ranking_profile(&options).unwrap(),
            RankingProfile {
                profile: preset.profile,
                fusion_strategy: FusionStrategy::RRF { k: 30 },
                alpha_override: Some(0.25),
                alpha_by_query_type: QueryTypeAlphas {
                    quoted_phrase: 0.9,
                    exact_reference: 0.8,
                    short: 0.6,
                    mixed: 0.4,
                    conceptual: 0.2,
                    unknown: 0.1,
                },
                bm25_saturation_k: 5.0,
                semantic_threshold: 0.55,
                agreement_bonus: 0.2,
                phrase_match_bonus: 0.15,
                rare_term_bonus: 0.05,
                section_coverage_bonus: 0.03,
                duplicate_penalty: 0.08,
                metadata_ranking_enabled: true,
                candidate_window_multiplier: 3.0,
                foundational_bonus: 0.004,
                foundational_candidate_share: 0.25,
                window_penalty: 0.03,
                alias_penalty: 0.05,
                query_cache_enabled: preset.query_cache_enabled,
                embedding_cache_enabled: preset.embedding_cache_enabled,
                telemetry_enabled: preset.telemetry_enabled,
            }
        );
        for (strategy, expected) in [
            (SemanticFusionStrategy::Weighted, FusionStrategy::Weighted),
            (SemanticFusionStrategy::Adaptive, FusionStrategy::Adaptive),
        ] {
            let options = SemanticRankingOptions {
                fusion_strategy: strategy,
                ..defaults()
            };
            assert_eq!(ranking_profile(&options).unwrap().fusion_strategy, expected);
        }
    }

    /// Each option outside its range is refused, as `InvalidInput` naming it by its name
    /// in the options, and never clamped; the ends of every range are accepted, and RRF's
    /// `k` is held to its range only where RRF reads it.
    #[test]
    fn an_option_out_of_range_is_refused_by_its_name() {
        type Spoil = fn(&mut SemanticRankingOptions);
        let cases: [(&str, Spoil); 26] = [
            ("rrf_k", |o| {
                o.fusion_strategy = SemanticFusionStrategy::Rrf;
                o.rrf_k = 0;
            }),
            ("alpha_override", |o| o.alpha_override = Some(1.5)),
            ("alpha_override", |o| o.alpha_override = Some(f64::NAN)),
            ("alpha_by_query_type.quoted_phrase", |o| {
                o.alpha_by_query_type.quoted_phrase = -0.1
            }),
            ("alpha_by_query_type.exact_reference", |o| {
                o.alpha_by_query_type.exact_reference = 1.01
            }),
            ("alpha_by_query_type.short", |o| {
                o.alpha_by_query_type.short = f64::INFINITY
            }),
            ("alpha_by_query_type.mixed", |o| {
                o.alpha_by_query_type.mixed = 2.0
            }),
            ("alpha_by_query_type.conceptual", |o| {
                o.alpha_by_query_type.conceptual = f64::NAN
            }),
            ("alpha_by_query_type.unknown", |o| {
                o.alpha_by_query_type.unknown = -1.0
            }),
            ("bm25_saturation_k", |o| o.bm25_saturation_k = 0.0),
            // Positive as a double, and zero in the 32 bits the ranking computes in.
            ("bm25_saturation_k", |o| o.bm25_saturation_k = 1e-60),
            ("semantic_threshold", |o| o.semantic_threshold = 1.5),
            ("agreement_bonus", |o| o.agreement_bonus = -0.1),
            ("phrase_match_bonus", |o| o.phrase_match_bonus = 10.0),
            ("rare_term_bonus", |o| o.rare_term_bonus = f64::NAN),
            ("section_coverage_bonus", |o| o.section_coverage_bonus = 1.5),
            ("duplicate_penalty", |o| o.duplicate_penalty = -0.5),
            ("candidate_window_multiplier", |o| {
                o.candidate_window_multiplier = 0.5
            }),
            ("candidate_window_multiplier", |o| {
                o.candidate_window_multiplier = 11.0
            }),
            ("candidate_window_multiplier", |o| {
                o.candidate_window_multiplier = f64::NEG_INFINITY
            }),
            ("foundational_bonus", |o| o.foundational_bonus = -0.001),
            ("foundational_candidate_share", |o| {
                o.foundational_candidate_share = 1.5
            }),
            ("window_penalty", |o| o.window_penalty = 0.25),
            ("window_penalty", |o| o.window_penalty = f64::NAN),
            ("alias_penalty", |o| o.alias_penalty = 0.25),
            ("alias_penalty", |o| o.alias_penalty = -0.01),
        ];
        for (field, spoil) in cases {
            let mut options = defaults();
            spoil(&mut options);
            let error = ranking_profile(&options).expect_err(field);
            assert_eq!(error.kind, SemanticErrorKind::InvalidInput, "{field}");
            assert_eq!(error.field.as_deref(), Some(field), "{}", error.message);
            assert!(
                error
                    .message
                    .starts_with("the ranking passed with this search is refused: "),
                "{}",
                error.message
            );
        }

        let unread = SemanticRankingOptions {
            fusion_strategy: SemanticFusionStrategy::Weighted,
            rrf_k: 0,
            ..defaults()
        };
        assert!(ranking_profile(&unread).is_ok(), "only RRF reads its k");
        let ends = SemanticRankingOptions {
            fusion_strategy: SemanticFusionStrategy::Rrf,
            rrf_k: 1,
            alpha_override: Some(0.0),
            alpha_by_query_type: SemanticQueryTypeAlphas {
                quoted_phrase: 0.0,
                exact_reference: 1.0,
                short: 0.0,
                mixed: 1.0,
                conceptual: 0.0,
                unknown: 1.0,
            },
            bm25_saturation_k: f64::from(f32::MIN_POSITIVE),
            semantic_threshold: 1.0,
            agreement_bonus: 0.0,
            phrase_match_bonus: 1.0,
            rare_term_bonus: 0.0,
            section_coverage_bonus: 1.0,
            duplicate_penalty: 0.0,
            metadata_ranking_enabled: true,
            candidate_window_multiplier: 10.0,
            foundational_bonus: 1.0,
            foundational_candidate_share: 0.0,
            window_penalty: 0.2,
            alias_penalty: 0.0,
        };
        ranking_profile(&ends)
            .expect("the ends of every range are values a calibration may land on");
    }
}

/// Where a search with a session stops once its token is cancelled, at each look in
/// turn: the sidecar's own first look, which sees the token only if this crate handed it
/// over, and this crate's before hydration and before painting. The probe cancels at the
/// look a test names, so where the search stops is the test's choice, not a race.
///
/// On the stand-in, which serves the stub ONNX package only in a build without the ONNX
/// backend, as in the integration suites.
#[cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]
mod semantic_cancellation {
    use super::*;
    use crate::search_cancellation::{cancelling_at, SearchCheckpoint as At};
    use otzaria_semantic_search::semantic::embedding::mock::write_stub_onnx_package;

    const BOOK: &str = "/books/genesis.txt";
    const LINES: [(u64, &str); 3] = [
        (1, "בראשית ברא אלהים את השמים ואת הארץ"),
        (2, "והארץ היתה תהו ובהו וחשך על פני תהום"),
        (3, "ויאמר אלהים יהי אור ויהי אור"),
    ];
    /// Every look a search that is served reaches, in order.
    const SERVED: [At; 4] = [At::Start, At::Sidecar, At::Hydration, At::Painting];

    /// An engine over [`LINES`] with a development session on the stand-in that has
    /// embedded every one of them, and the directories that have to outlive it.
    fn indexed() -> (SearchEngine, TempDir, TempDir) {
        let (mut engine, index) = make_engine();
        for (id, text) in LINES {
            add(&mut engine, id, text, BOOK);
        }
        engine.commit().unwrap();
        let semantic = TempDir::new().unwrap();
        let model = write_stub_onnx_package(&semantic.path().join("model"));
        engine
            .configure_semantic(SemanticConfigInput {
                root_dir: semantic
                    .path()
                    .join("semantic")
                    .to_string_lossy()
                    .into_owned(),
                model_path: model.to_string_lossy().into_owned(),
                model_id: "test-mock".to_string(),
                embedding_dim: 64,
                pooling: "in-graph".to_string(),
                max_tokens: 512,
                model_quantization: "int8".to_string(),
                embedding_text_version: 1,
                onnx_runtime_path: None,
            })
            .unwrap();
        let indexed = engine
            .semantic_index_books(vec![SemanticBookInput {
                source_book_key: BOOK.to_string(),
                title: "title".to_string(),
                content_fingerprint: 1,
                is_pdf: false,
                topics: "/root".to_string(),
                extra_facets: Vec::new(),
                lines: LINES
                    .iter()
                    .map(|&(id, text)| SemanticBookLineInput {
                        line_id: id,
                        section_id: id,
                        text: text.to_string(),
                        line_hash: id,
                        reference: "ref".to_string(),
                        segment: 0,
                    })
                    .collect(),
            }])
            .unwrap();
        assert_eq!(indexed.chunks_written, LINES.len() as u32);
        (engine, index, semantic)
    }

    /// What a page is, as bits: an uncancelled search after the cancelled ones must
    /// answer exactly as one before them did.
    fn page(response: &SemanticSearchResponse) -> Vec<(u64, u32, Option<u32>)> {
        response
            .results
            .iter()
            .map(|hit| {
                (
                    hit.id,
                    hit.fused_score.to_bits(),
                    hit.semantic_score.map(f32::to_bits),
                )
            })
            .collect()
    }

    #[test]
    fn a_search_stops_at_the_look_it_was_cancelled_at_and_the_session_serves_on() {
        // A change to the process-wide line source would start another session mid-test.
        let _serial = crate::line_source::TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (engine, _index, _semantic) = indexed();
        let search = |mode, token: &SemanticCancellationToken| {
            engine.search_semantic(
                LINES[2].1.to_string(),
                Vec::new(),
                10,
                0,
                SemanticLexicalMode::Exact,
                0,
                mode,
                None,
                false,
                false,
                None,
                token,
            )
        };

        for mode in [
            SemanticRetrievalMode::Hybrid,
            SemanticRetrievalMode::SemanticOnly,
            SemanticRetrievalMode::LexicalOnly,
        ] {
            let (served, reached) =
                cancelling_at(None, || search(mode, &SemanticCancellationToken::new()));
            let served = served.unwrap();
            assert_eq!(reached, SERVED, "{mode:?}");
            assert_eq!(served.fallback_kind, None, "{mode:?}");
            assert!(!served.results.is_empty(), "{mode:?}");
            // Asked again, the page is the session's: nothing runs but the last look.
            let (again, reached) =
                cancelling_at(None, || search(mode, &SemanticCancellationToken::new()));
            assert_eq!(reached, [At::Start, At::Painting], "{mode:?}");
            assert_eq!(page(&again.unwrap()), page(&served), "{mode:?}");

            // Cancelled as the token is handed over, the search stops at the sidecar's
            // own look and never reaches hydration: had the token not been handed over,
            // it would have run on to this crate's next look.
            for (cancel_at, stops_after) in
                [(At::Sidecar, 2), (At::Hydration, 3), (At::Painting, 4)]
            {
                // Without its session, so that the search runs every look.
                engine.semantic_sessions.invalidate();
                let token = SemanticCancellationToken::new();
                let (result, reached) = cancelling_at(Some(cancel_at), || search(mode, &token));
                match result {
                    Ok(_) => panic!("{mode:?}: cancelled at {cancel_at:?}, and served"),
                    Err(error) => assert_eq!(
                        error,
                        SemanticError::cancelled(),
                        "{mode:?} at {cancel_at:?}"
                    ),
                }
                assert_eq!(
                    reached,
                    SERVED[..stops_after],
                    "{mode:?}: cancelled at {cancel_at:?}"
                );
            }

            let (after, _) =
                cancelling_at(None, || search(mode, &SemanticCancellationToken::new()));
            assert_eq!(
                page(&after.unwrap()),
                page(&served),
                "{mode:?}: the cancelled searches changed what the session serves"
            );
        }
    }
}

/// Passage highlights for semantic hits, on the stand-in: which clause is marked, which
/// lines are left alone, and what a call remembers and abandons.
#[cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]
mod semantic_passage_highlights {
    use super::*;
    use crate::search_cancellation::{cancelling_at, SearchCheckpoint as At};
    use crate::semantic_highlight::{EMBEDDED_CLAUSES, MAX_CLAUSES_PER_LINE};
    use otzaria_semantic_search::semantic::embedding::mock::write_stub_onnx_package;

    const BOOK: &str = "/books/genesis.txt";
    const LONG: (u64, &str) = (
        1,
        "בראשית ברא אלהים את השמים ואת הארץ, והארץ היתה תהו ובהו וחשך על פני תהום, \
             ורוח אלהים מרחפת על פני המים, ויאמר אלהים יהי אור ויהי אור.",
    );
    const SHORT: (u64, &str) = (2, "וירא אלהים את האור כי טוב");
    /// Eight clauses, of which the seventh holds the query's words.
    const MANY: (u64, &str) = (
            3,
            "אחד שנים שלשה ארבעה, חמשה ששה שבעה שמונה, תשעה עשרה אחד עשר, שנים עשר שלשה עשר, \
             ארבעה עשר חמשה עשר, ששה עשר שבעה עשר, ורוח אלהים מרחפת על פני המים, שמונה עשר תשעה עשר.",
        );
    const QUERY: &str = "ורוח אלהים מרחפת על פני המים";
    const MARKED: &str = "<mark>ורוח אלהים מרחפת על פני המים,</mark>";

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        crate::line_source::TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn unconfigured() -> (SearchEngine, TempDir) {
        let (mut engine, index) = make_engine();
        for (id, text) in [LONG, SHORT, MANY] {
            add(&mut engine, id, text, BOOK);
        }
        engine.commit().unwrap();
        (engine, index)
    }

    fn configured() -> (SearchEngine, TempDir, TempDir) {
        let (mut engine, index) = unconfigured();
        let semantic = TempDir::new().unwrap();
        let model = write_stub_onnx_package(&semantic.path().join("model"));
        engine
            .configure_semantic(SemanticConfigInput {
                root_dir: semantic
                    .path()
                    .join("semantic")
                    .to_string_lossy()
                    .into_owned(),
                model_path: model.to_string_lossy().into_owned(),
                model_id: "test-mock".to_string(),
                embedding_dim: 64,
                pooling: "in-graph".to_string(),
                max_tokens: 512,
                model_quantization: "int8".to_string(),
                embedding_text_version: 1,
                onnx_runtime_path: None,
            })
            .unwrap();
        engine
            .semantic_index_books(vec![SemanticBookInput {
                source_book_key: BOOK.to_string(),
                title: "title".to_string(),
                content_fingerprint: 1,
                is_pdf: false,
                topics: "/root".to_string(),
                extra_facets: Vec::new(),
                lines: [LONG, SHORT]
                    .iter()
                    .map(|&(id, text)| SemanticBookLineInput {
                        line_id: id,
                        section_id: id,
                        text: text.to_string(),
                        line_hash: id,
                        reference: "ref".to_string(),
                        segment: 0,
                    })
                    .collect(),
            }])
            .unwrap();
        (engine, index, semantic)
    }

    fn target(file_path: &str, id: u64) -> SemanticHighlightTarget {
        SemanticHighlightTarget {
            file_path: file_path.to_string(),
            id,
            window: 0,
        }
    }

    fn highlights(
        engine: &SearchEngine,
        query: &str,
        targets: &[SemanticHighlightTarget],
    ) -> Result<Vec<SemanticPassageHighlight>, SemanticError> {
        engine.semantic_passage_highlights(
            query.to_string(),
            targets.to_vec(),
            &SemanticCancellationToken::new(),
        )
    }

    fn embedded() -> usize {
        EMBEDDED_CLAUSES.get()
    }

    fn unmarked(target: &SemanticHighlightTarget) -> SemanticPassageHighlight {
        SemanticPassageHighlight {
            file_path: target.file_path.clone(),
            id: target.id,
            snippet_html: String::new(),
            is_highlighted: false,
            span_score: None,
        }
    }

    #[test]
    fn the_nearest_clause_is_marked_and_other_targets_are_answered_in_order() {
        let _serial = serialized();
        let (engine, _index, _semantic) = configured();
        let targets = [
            target(BOOK, SHORT.0),
            target(BOOK, 99),
            target("/books/other.txt", LONG.0),
            target(BOOK, LONG.0),
        ];
        let before = embedded();
        let got = highlights(&engine, QUERY, &targets).unwrap();
        assert_eq!(got.len(), targets.len());
        for (shown, target) in got.iter().zip(&targets).take(3) {
            assert_eq!(shown, &unmarked(target), "{target:?}");
        }
        let long = &got[3];
        assert_eq!((long.file_path.as_str(), long.id), (BOOK, LONG.0));
        assert!(long.is_highlighted);
        assert_eq!(
            long.snippet_html,
            format!(
                "בראשית ברא אלהים את השמים ואת הארץ, והארץ היתה תהו ובהו וחשך על פני תהום, \
                     {MARKED} ויאמר אלהים יהי אור ויהי אור."
            )
        );
        let score = long.span_score.unwrap();
        assert!(score > 0.5 && score <= 1.0 + 1e-6, "{score}");
        // Four clauses, each embedded once; the short line embeds nothing.
        assert_eq!(embedded() - before, 4);
    }

    #[test]
    fn a_line_of_many_clauses_embeds_six_chosen_by_the_words_they_share() {
        let _serial = serialized();
        let (engine, _index, _semantic) = configured();
        let before = embedded();
        let got = highlights(&engine, QUERY, &[target(BOOK, MANY.0)]).unwrap();
        assert_eq!(embedded() - before, MAX_CLAUSES_PER_LINE);
        assert!(got[0].snippet_html.contains(MARKED), "{got:?}");
    }

    #[test]
    fn a_line_already_highlighted_for_the_query_is_not_embedded_again() {
        let _serial = serialized();
        let (engine, _index, _semantic) = configured();
        let targets = [target(BOOK, LONG.0)];
        let first = highlights(&engine, QUERY, &targets).unwrap();
        let before = embedded();
        let again = highlights(&engine, QUERY, &targets).unwrap();
        assert_eq!(embedded(), before, "answered from the cache");
        assert_eq!(again, first);

        // Another query is another highlight.
        let other = highlights(&engine, "והארץ היתה תהו ובהו", &targets).unwrap();
        assert!(embedded() > before);
        assert!(
            other[0].snippet_html.contains("<mark>והארץ היתה"),
            "{other:?}"
        );
    }

    #[test]
    fn without_a_semantic_session_nothing_is_marked() {
        let (engine, _index) = unconfigured();
        let targets = [target(BOOK, LONG.0), target(BOOK, 99)];
        let got = highlights(&engine, QUERY, &targets).unwrap();
        assert_eq!(got, targets.iter().map(unmarked).collect::<Vec<_>>());
    }

    #[test]
    fn a_cancelled_call_ends_cancelled_and_remembers_nothing() {
        let _serial = serialized();
        let (engine, _index, _semantic) = configured();
        let targets = vec![target(BOOK, LONG.0)];

        let token = SemanticCancellationToken::new();
        token.cancel();
        let result = engine.semantic_passage_highlights(QUERY.to_string(), targets.clone(), &token);
        assert_eq!(result, Err(SemanticError::cancelled()));

        let before = embedded();
        let (result, reached) = cancelling_at(Some(At::Highlight), || {
            engine.semantic_passage_highlights(
                QUERY.to_string(),
                targets.clone(),
                &SemanticCancellationToken::new(),
            )
        });
        assert_eq!(result, Err(SemanticError::cancelled()));
        assert_eq!(reached, [At::Start, At::Highlight]);
        assert_eq!(embedded(), before);

        highlights(&engine, QUERY, &targets).unwrap();
        assert_eq!(
            embedded() - before,
            4,
            "nothing was remembered from the cancelled calls"
        );
    }
}

/// A semantic search's pages continue the results its earlier pages showed, from its
/// session: computed once per window, never repeated, never skipped.
#[cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]
mod semantic_sessions {
    use super::*;
    use crate::search_cancellation::{cancelling_at, SearchCheckpoint as At};
    use otzaria_semantic_search::semantic::embedding::mock::write_stub_onnx_package;
    use std::sync::atomic::Ordering;

    const BASE: &str = "/books/base.txt";
    const OTHER: &str = "/books/other.txt";
    const UNRELATED: &str = "/books/unrelated.txt";

    /// The library line source is process-wide, and a session's key reads it: tests that
    /// configure it would start other sessions under these.
    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        crate::line_source::TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// One book: its path, topics and lines.
    type Book<'a> = (&'a str, &'a str, Vec<String>);

    /// An engine over `books`, with a development session on the stand-in that has
    /// embedded the books `embedded` names.
    fn engine_over(books: &[Book<'_>], embedded: &[&str]) -> (SearchEngine, TempDir, TempDir) {
        let (mut engine, index) = make_engine();
        // A background merge is another index generation, and with it another session.
        engine.set_bulk_indexing(true).unwrap();
        let mut id = 0;
        let mut inputs = Vec::new();
        for (order, (path, topics, lines)) in books.iter().enumerate() {
            let mut book_lines = Vec::new();
            for (index, text) in lines.iter().enumerate() {
                id += 1;
                engine
                    .add_document_at(
                        id, "title", "ref", topics, text, 0, false, path, None, None, None,
                    )
                    .unwrap();
                book_lines.push(SemanticBookLineInput {
                    line_id: id,
                    section_id: id,
                    text: text.clone(),
                    line_hash: id,
                    reference: format!("ref {index}"),
                    segment: 0,
                });
            }
            if embedded.contains(path) {
                inputs.push(SemanticBookInput {
                    source_book_key: path.to_string(),
                    title: "title".to_string(),
                    content_fingerprint: order as u64 + 1,
                    is_pdf: false,
                    topics: topics.to_string(),
                    extra_facets: Vec::new(),
                    lines: book_lines,
                });
            }
        }
        engine.commit().unwrap();
        let semantic = TempDir::new().unwrap();
        let model = write_stub_onnx_package(&semantic.path().join("model"));
        engine
            .configure_semantic(SemanticConfigInput {
                root_dir: semantic
                    .path()
                    .join("semantic")
                    .to_string_lossy()
                    .into_owned(),
                model_path: model.to_string_lossy().into_owned(),
                model_id: "test-mock".to_string(),
                embedding_dim: 64,
                pooling: "in-graph".to_string(),
                max_tokens: 512,
                model_quantization: "int8".to_string(),
                embedding_text_version: 1,
                onnx_runtime_path: None,
            })
            .unwrap();
        engine.semantic_index_books(inputs).unwrap();
        (engine, index, semantic)
    }

    /// Forty lines with שבת in two books, every one of them embedded. Each holds the word
    /// a number of times no other line does, so no two lexical scores tie.
    #[test]
    fn an_evicted_continuation_returns_a_flagged_first_page() {
        let _serial = serialized();
        let make = |book: &str, first: usize| -> Vec<String> {
            (0..100)
                .map(|n| format!("{}{book}", "שבת ".repeat(first + 2 * n)))
                .collect()
        };
        let (engine, _index, _semantic) = engine_over(
            &[
                (BASE, "/base/תורה", make("ראשון", 1)),
                (OTHER, "/other", make("שני", 2)),
            ],
            &[BASE, OTHER],
        );
        let limit = 30;
        let first = page(&engine, limit, 0);
        let continued = page(&engine, limit, limit);
        for query in ["תפילין", "תשובה", "רמבם", "מלך", "הלך"] {
            page_of(
                &engine,
                query,
                10,
                0,
                None,
                &SemanticCancellationToken::new(),
            )
            .unwrap();
        }
        let restarted = page(&engine, limit, limit);
        assert!(!first.session_restarted);
        assert!(!continued.session_restarted);
        assert!(
            restarted.session_restarted,
            "a caller must replace the old list"
        );
        assert_eq!(
            bits(&restarted),
            bits(&first),
            "restart uses the first page's window"
        );
        let next = page(&engine, limit, limit);
        assert!(!next.session_restarted);
        assert_eq!(bits(&next), bits(&continued));
    }

    fn sabbath() -> (SearchEngine, TempDir, TempDir) {
        let lines = |book: &str, first: usize| -> Vec<String> {
            (0..20)
                .map(|n| format!("{}{book}", "שבת ".repeat(first + 2 * n)))
                .collect()
        };
        engine_over(
            &[
                (BASE, "/base/תורה", lines("ראשון", 1)),
                (OTHER, "/other", lines("שני", 2)),
            ],
            &[BASE, OTHER],
        )
    }

    #[test]
    fn an_expired_or_invalidated_continuation_restarts_explicitly() {
        let _serial = serialized();
        let (engine, _index, _semantic) = sabbath();
        let first = page(&engine, 5, 0);
        engine.semantic_sessions.expire_for_test();
        let expired = page(&engine, 5, 5);
        assert!(expired.session_restarted);
        assert_eq!(bits(&expired), bits(&first));
        assert!(!page(&engine, 5, 5).session_restarted);
        engine.semantic_sessions.invalidate();
        let invalidated = page(&engine, 5, 10);
        assert!(invalidated.session_restarted);
        assert_eq!(bits(&invalidated), bits(&first));
    }

    #[test]
    fn different_page_sizes_do_not_share_frozen_prefixes() {
        let _serial = serialized();
        let (engine, _index, _semantic) = sabbath();
        let small = page(&engine, 5, 0);
        page(&engine, 10, 0);
        assert_eq!(expansions(&engine), 2);
        let continued = page(&engine, 5, 5);
        assert!(!continued.session_restarted);
        let shown: HashSet<_> = lines(&small).into_iter().collect();
        assert!(lines(&continued).iter().all(|line| !shown.contains(line)));
        let changed = page(&engine, 7, 10);
        assert!(changed.session_restarted);
    }

    #[test]
    fn grouping_and_page_sizes_keep_distinct_sessions_through_eviction() {
        let _serial = serialized();
        let (engine, _index, _semantic) = sabbath();
        let grouped = |limit, offset, grouping| {
            engine
                .search_semantic(
                    "שבת".to_string(),
                    Vec::new(),
                    limit,
                    offset,
                    SemanticLexicalMode::Exact,
                    0,
                    SemanticRetrievalMode::Hybrid,
                    grouping,
                    false,
                    false,
                    None,
                    &SemanticCancellationToken::new(),
                )
                .unwrap()
        };
        for mode in [
            Some(SemanticGroupingMode::SameSection),
            Some(SemanticGroupingMode::IdenticalText),
        ] {
            let first = grouped(4, 0, mode);
            assert!(!first.session_restarted);
            grouped(9, 0, mode);
            grouped(4, 0, None);
            let second = grouped(4, 4, mode);
            assert!(!second.session_restarted);
            let shown: HashSet<_> = lines(&first).into_iter().collect();
            assert!(lines(&second).iter().all(|line| !shown.contains(line)));
            assert_eq!(bits(&grouped(4, 0, mode)), bits(&first));
            for query in ["אב", "תורה", "גמרא", "רמבם", "תפלה"] {
                page_of(
                    &engine,
                    query,
                    4,
                    0,
                    None,
                    &SemanticCancellationToken::new(),
                )
                .unwrap();
            }
            let restart = grouped(4, 8, mode);
            assert!(restart.session_restarted);
            assert_eq!(bits(&restart), bits(&first));
            assert!(!grouped(4, 4, mode).session_restarted);
        }
    }

    #[test]
    fn committed_index_restarts_continuation_with_new_page_zero() {
        let _serial = serialized();
        let (mut engine, _index, _semantic) = sabbath();
        let old = page(&engine, 6, 0);
        page(&engine, 6, 6);
        add(&mut engine, 1000, "שבת שבת שבת חדשה", "/books/qa-new.txt");
        engine.commit().unwrap();
        let restart = page(&engine, 6, 12);
        assert!(restart.session_restarted);
        assert_eq!(bits(&restart), bits(&page(&engine, 6, 0)));
        let next = page(&engine, 6, 6);
        assert!(!next.session_restarted);
        let shown: HashSet<_> = lines(&restart).into_iter().collect();
        assert!(lines(&next).iter().all(|line| !shown.contains(line)));
        assert_eq!(old.results.len(), 6);
    }

    #[test]
    fn cancelled_expired_restart_does_not_publish_a_partial_session() {
        let _serial = serialized();
        for at in [At::Sidecar, At::Hydration, At::Painting] {
            let (engine, _index, _semantic) = sabbath();
            let first = page(&engine, 6, 0);
            engine.semantic_sessions.expire_for_test();
            let token = SemanticCancellationToken::new();
            let (cancelled, _) =
                cancelling_at(Some(at), || page_of(&engine, "שבת", 6, 12, None, &token));
            assert_eq!(cancelled.err(), Some(SemanticError::cancelled()));
            let restarted = page(&engine, 6, 12);
            assert!(
                restarted.session_restarted,
                "cancelled restart persisted at {at:?}"
            );
            assert_eq!(bits(&restarted), bits(&first));
            assert!(!page(&engine, 6, 6).session_restarted);
        }
    }

    fn page_of(
        engine: &SearchEngine,
        query: &str,
        limit: u32,
        offset: u32,
        ranking: Option<SemanticRankingOptions>,
        token: &SemanticCancellationToken,
    ) -> Result<SemanticSearchResponse, SemanticError> {
        engine.search_semantic(
            query.to_string(),
            Vec::new(),
            limit,
            offset,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::Hybrid,
            None,
            false,
            false,
            ranking,
            token,
        )
    }

    fn page(engine: &SearchEngine, limit: u32, offset: u32) -> SemanticSearchResponse {
        page_of(
            engine,
            "שבת",
            limit,
            offset,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap()
    }

    fn lines(response: &SemanticSearchResponse) -> Vec<(String, u64)> {
        response
            .results
            .iter()
            .map(|hit| (hit.file_path.clone(), hit.id))
            .collect()
    }

    /// A page as bits, for pages that must be identical.
    fn bits(response: &SemanticSearchResponse) -> Vec<(String, u64, u32, String)> {
        response
            .results
            .iter()
            .map(|hit| {
                (
                    hit.file_path.clone(),
                    hit.id,
                    hit.fused_score.to_bits(),
                    hit.snippet_html.clone(),
                )
            })
            .collect()
    }

    fn expansions(engine: &SearchEngine) -> u64 {
        engine.semantic_sessions.expansions.load(Ordering::Relaxed)
    }

    /// Paged to the end, every result comes once, the union is what one page as large as
    /// the whole search shows, and only the last page says nothing follows.
    #[test]
    fn pages_show_every_result_once_and_say_when_none_follows() {
        let _serial = serialized();
        let (engine, _index, _semantic) = sabbath();
        let mut shown = Vec::new();
        let mut offset = 0;
        loop {
            let response = page(&engine, 5, offset);
            assert!(response.results.len() <= 5);
            shown.extend(lines(&response));
            offset += 5;
            if !response.has_more {
                assert!(offset >= shown.len() as u32, "a page was cut short");
                break;
            }
            assert_eq!(response.results.len(), 5, "a full page before the last");
            assert!(offset < 100, "paging never ends");
        }
        let unique: HashSet<_> = shown.iter().cloned().collect();
        assert_eq!(unique.len(), shown.len(), "a result shown twice: {shown:?}");

        let (whole, _index, _semantic) = sabbath();
        let all = page(&whole, 500, 0);
        assert!(!all.has_more);
        let all: HashSet<_> = lines(&all).into_iter().collect();
        assert_eq!(unique, all, "a result never shown");
        assert!(all.len() >= 40);
    }

    /// A page asked again is the same page, from the session; a page inside the window
    /// the session fused runs no lexical phase, and one past it widens the window once.
    #[test]
    fn a_page_inside_the_window_is_a_slice_of_the_session() {
        let _serial = serialized();
        let (engine, _index, _semantic) = sabbath();
        let first = page(&engine, 5, 0);
        assert_eq!(expansions(&engine), 1);
        let second = page(&engine, 5, 5);
        assert_eq!(
            expansions(&engine),
            1,
            "the first window holds the second page"
        );
        assert_eq!(bits(&page(&engine, 5, 5)), bits(&second));
        assert_eq!(bits(&page(&engine, 5, 0)), bits(&first));
        assert_eq!(expansions(&engine), 1);

        let third = page(&engine, 5, 10);
        assert_eq!(expansions(&engine), 2, "past the window, one wider fusion");
        assert_eq!(bits(&page(&engine, 5, 10)), bits(&third));
        assert_eq!(
            bits(&page(&engine, 5, 5)),
            bits(&second),
            "earlier pages stand"
        );
        assert_eq!(expansions(&engine), 2);
        assert!(third.has_more && second.has_more && first.has_more);
    }

    /// A result fused in a window but never shown is ranked again with the wider window's:
    /// a page past the window is in fused order, and the pages shown before stand.
    #[test]
    fn a_result_never_shown_is_ranked_with_the_wider_window() {
        let _serial = serialized();
        let (engine, _index, _semantic) = sabbath();
        let first = page(&engine, 5, 0);
        let second = page(&engine, 5, 5);
        let third = page(&engine, 5, 10);
        assert_eq!(expansions(&engine), 2);
        let scores: Vec<f32> = third.results.iter().map(|hit| hit.fused_score).collect();
        assert!(
            scores.windows(2).all(|pair| pair[0] >= pair[1]),
            "out of fused order: {scores:?}"
        );
        assert_eq!(bits(&page(&engine, 5, 0)), bits(&first));
        assert_eq!(bits(&page(&engine, 5, 5)), bits(&second));
        assert_eq!(bits(&page(&engine, 5, 10)), bits(&third));
        assert!(third.has_more);
    }

    /// The counts keep their meaning: the lexical count is the corpus-wide count, and the
    /// candidate counts are a window's, the same on every page that window serves.
    #[test]
    fn the_counts_are_the_windows_and_the_lexical_count_the_corpus() {
        let _serial = serialized();
        let (engine, _index, _semantic) = sabbath();
        let first = page(&engine, 5, 0);
        let second = page(&engine, 5, 5);
        let lexical = engine
            .count_exact_with_status("שבת".to_string(), Vec::new(), false, false, None)
            .unwrap();
        for response in [&first, &second] {
            assert_eq!(response.lexical_total_count, lexical.count);
            assert_eq!(response.truncated, lexical.truncated);
            assert!(!response.counts_are_exact);
            assert!(!response.candidate_window_truncated);
            assert_eq!(response.group_count, None);
            assert!(response.total_count >= 11, "{}", response.total_count);
        }
        assert_eq!(first.total_count, second.total_count);
    }

    /// A commit to the index is another session: its pages are computed afresh.
    #[test]
    fn a_commit_to_the_index_starts_another_session() {
        let _serial = serialized();
        let (mut engine, _index, _semantic) = sabbath();
        let before = page(&engine, 50, 0);
        assert!(!lines(&before).iter().any(|(_, id)| *id == 1000));
        add(&mut engine, 1000, "שבת", "/books/new.txt");
        engine.commit().unwrap();
        let after = page(&engine, 50, 0);
        assert_eq!(expansions(&engine), 2);
        assert!(lines(&after).iter().any(|(_, id)| *id == 1000));
    }

    /// Changing the semantic session starts another one too.
    #[test]
    fn indexing_vectors_starts_another_session() {
        let _serial = serialized();
        let (engine, _index, _semantic) = sabbath();
        page(&engine, 5, 0);
        engine
            .remove_semantic_books(vec![OTHER.to_string()])
            .unwrap();
        page(&engine, 5, 0);
        assert_eq!(expansions(&engine), 2);
    }

    /// A search cancelled while it widened the window adds nothing: the page asked again
    /// is the one a search never cancelled shows.
    #[test]
    fn a_cancelled_expansion_leaves_the_session_as_it_was() {
        let _serial = serialized();
        let (reference, _index, _semantic) = sabbath();
        page(&reference, 5, 0);
        let expected = page(&reference, 5, 10);

        for cancel_at in [At::Sidecar, At::Hydration, At::Painting] {
            let (engine, _index, _semantic) = sabbath();
            page(&engine, 5, 0);
            let token = SemanticCancellationToken::new();
            let (cancelled, _) = cancelling_at(Some(cancel_at), || {
                page_of(&engine, "שבת", 5, 10, None, &token)
            });
            assert_eq!(
                cancelled.err(),
                Some(SemanticError::cancelled()),
                "{cancel_at:?}"
            );
            assert_eq!(
                bits(&page(&engine, 5, 10)),
                bits(&expected),
                "{cancel_at:?}"
            );
            assert_eq!(bits(&page(&engine, 5, 5)), bits(&page(&reference, 5, 5)));
        }
    }

    /// No options and the defaults are one ranking, and one session.
    #[test]
    fn no_ranking_is_the_defaults() {
        let _serial = serialized();
        let (engine, _index, _semantic) = sabbath();
        let token = SemanticCancellationToken::new();
        let none = page_of(&engine, "שבת", 5, 0, None, &token).unwrap();
        let defaults = page_of(
            &engine,
            "שבת",
            5,
            0,
            Some(SemanticRankingOptions::defaults()),
            &token,
        )
        .unwrap();
        assert_eq!(bits(&none), bits(&defaults));
        assert_eq!(expansions(&engine), 1);
    }

    /// A line of a foundational book that only lexical search found gains the bonus, and
    /// passes the better lexical match of another book.
    #[test]
    fn a_foundational_line_found_lexically_alone_moves_up() {
        let _serial = serialized();
        let (engine, _index, _semantic) = engine_over(
            &[
                (
                    BASE,
                    "/base/תורה",
                    vec!["ברכה על המחיה והכלכלה".to_string()],
                ),
                (OTHER, "/other", vec!["ברכה ברכה".to_string()]),
                (UNRELATED, "/other", vec!["דברים אחרים לגמרי".to_string()]),
            ],
            &[UNRELATED],
        );
        let token = SemanticCancellationToken::new();
        let ranked = |ranking| {
            let response = page_of(&engine, "ברכה", 10, 0, ranking, &token).unwrap();
            let order: Vec<String> = response
                .results
                .iter()
                .filter(|hit| hit.source == SemanticResultSource::Lexical)
                .map(|hit| hit.file_path.clone())
                .collect();
            order
        };
        let without = ranked(Some(SemanticRankingOptions {
            foundational_bonus: 0.0,
            ..SemanticRankingOptions::defaults()
        }));
        assert_eq!(without, [OTHER, BASE]);
        assert_eq!(ranked(None), [BASE, OTHER]);
    }

    /// A quoted phrase is looked up verbatim even when the search asked for fuzzy
    /// matching, as the semantic half skips it.
    #[test]
    fn a_quoted_phrase_runs_the_exact_lexical_phase() {
        let _serial = serialized();
        let (engine, _index, _semantic) = engine_over(
            &[(
                OTHER,
                "/other",
                vec!["ברכה גדולה".to_string(), "ברכו את השם".to_string()],
            )],
            &[OTHER],
        );
        let fuzzy = |query: &str| {
            engine
                .search_semantic(
                    query.to_string(),
                    Vec::new(),
                    10,
                    0,
                    SemanticLexicalMode::Fuzzy,
                    1,
                    SemanticRetrievalMode::Hybrid,
                    None,
                    false,
                    false,
                    None,
                    &SemanticCancellationToken::new(),
                )
                .unwrap()
        };
        let lexical = |response: &SemanticSearchResponse| -> Vec<u64> {
            response
                .results
                .iter()
                .filter(|hit| hit.lexical_score.is_some())
                .map(|hit| hit.id)
                .collect()
        };
        let unquoted = fuzzy("ברכה");
        assert_eq!(lexical(&unquoted).len(), 2, "within one edit, both lines");
        let quoted = fuzzy("\"ברכה\"");
        assert_eq!(lexical(&quoted), [1]);
        assert_eq!(quoted.executed_mode, SemanticExecutedMode::LexicalOnly);
    }

    /// An acronym's gershayim is no quotation mark: the semantic half runs.
    #[test]
    fn an_acronym_runs_the_hybrid_search() {
        let _serial = serialized();
        let (engine, _index, _semantic) = engine_over(
            &[(
                OTHER,
                "/other",
                vec!["כתב הרמב\"ם בהלכות תשובה".to_string()],
            )],
            &[OTHER],
        );
        for query in ["רמב\"ם", "רמב״ם"] {
            let response = page_of(
                &engine,
                query,
                10,
                0,
                None,
                &SemanticCancellationToken::new(),
            )
            .unwrap();
            assert_eq!(
                response.executed_mode,
                SemanticExecutedMode::Hybrid,
                "{query}"
            );
            assert!(response.semantic_available, "{query}");
        }
    }
}

/// A verification that an install overtook — it read bytes the install has since
/// replaced — condemned nothing, and is answered as a busy set, to verify again; one
/// that was not is the report.
#[cfg(feature = "semantic-integration")]
#[test]
fn a_verification_an_install_overtook_is_busy_and_not_damage() {
    use otzaria_semantic_search::semantic::segment_set::ScrubReport;
    let report = |superseded| ScrubReport {
        generation: 3,
        segments: 2,
        bytes_checked: 10,
        elapsed_ms: 1,
        superseded,
    };
    let verified = verification_of(&report(false), Path::new("/vectors")).unwrap();
    assert_eq!(
        (
            verified.generation,
            verified.segments,
            verified.bytes_checked
        ),
        (3, 2, 10)
    );
    let Err(overtaken) = verification_of(&report(true), Path::new("/vectors")) else {
        panic!("a verification an install overtook is no report of the set");
    };
    assert_eq!(
        (overtaken.kind, overtaken.field.as_deref()),
        (SemanticErrorKind::VectorsBusy, Some("vectors_dir"))
    );
}

/// The wait for a set's lock ends when the lock is free, when it runs out, and on a
/// cancel, even one seen only once the wait has run out.
#[cfg(feature = "semantic-integration")]
#[test]
fn a_wait_for_the_set_lock_ends_free_out_of_time_or_cancelled() {
    use otzaria_semantic_search::errors::ArtifactError;
    use std::time::Duration;
    let held = || -> Result<(), SemanticSearchError> {
        Err(SemanticSearchError::Artifact(ArtifactError::Io {
            context: "locking".to_string(),
            source: std::io::Error::from(std::io::ErrorKind::WouldBlock),
        }))
    };
    let token = SearchCancellation::new();
    let mut attempts = 0;
    let freed = waiting_for_set_lock(&token, Duration::from_secs(5), || {
        attempts += 1;
        if attempts < 3 {
            held()
        } else {
            Ok(())
        }
    });
    assert!(freed.is_ok() && attempts == 3);
    let busy = waiting_for_set_lock(&token, Duration::ZERO, held);
    assert!(matches!(busy, Err(SemanticSearchError::Artifact(_))));
    token.cancel();
    let cancelled = waiting_for_set_lock(&token, Duration::ZERO, held);
    assert!(matches!(cancelled, Err(SemanticSearchError::Cancelled)));
}

/// A displayed semantic result is checked against its vector by the full 128-bit key,
/// recomputed from the line's text: a line whose column agrees by 64 bits and whose
/// text does not is not shown as a semantic match.
#[cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]
mod semantic_verification {
    use super::*;
    use crate::semantic_corpus::TantivyCorpus;
    use crate::semantic_keys::production_chunking;
    use crate::semantic_resolver::MAX_FAILED_REPEATS;
    use otzaria_semantic_search::cancellation::CancellationToken;
    use otzaria_semantic_search::distribution::builder::{
        build, BuildRequest, RELEASE_MANIFEST_FILENAME, SEGMENT_FILENAME,
    };
    use otzaria_semantic_search::distribution::corpus::CorpusIndex;
    use otzaria_semantic_search::semantic::embedding::mock;
    use otzaria_semantic_search::semantic::model_package::validate_onnx_package;
    use otzaria_semantic_search::semantic::official_index::readable_store_identity;
    use otzaria_semantic_search::semantic::segment_set::{
        install_package, InstallExpectation, InstallSource,
    };
    use otzaria_semantic_search::semantic::versioning::IndexVersion;

    const BOOK: &str = "/books/genesis.txt";
    const PROBE: &str = "ויאמר אלהים יהי אור ויהי אור";

    /// An index of one book, and a base package built from it: what a release is made of.
    struct Built {
        engine: SearchEngine,
        model: ModelIdentity,
        model_file: PathBuf,
        /// The package: its segment and its release manifest.
        package: PathBuf,
        manifest_sha256: String,
        identity: IndexVersion,
    }

    /// An index of one book, a vector set built from it and installed, and the set open.
    fn opened(dir: &TempDir) -> SearchEngine {
        open(dir, built(dir))
    }

    /// `built` installed into `dir`'s vector set, and the set open.
    fn open(dir: &TempDir, built: Built) -> SearchEngine {
        let vectors = dir.path().join("vectors");
        install_package(
            &vectors,
            &InstallSource {
                segment: &built.package.join(SEGMENT_FILENAME),
                manifest_json: &fs::read_to_string(built.package.join(RELEASE_MANIFEST_FILENAME))
                    .unwrap(),
            },
            &InstallExpectation {
                identity: built.identity.clone(),
                published_manifest_sha256: Some(built.manifest_sha256.clone()),
            },
            &CancellationToken::new(),
        )
        .unwrap();
        built
            .engine
            .open_semantic_artifact(SemanticArtifactInput {
                vectors_dir: vectors.to_string_lossy().into_owned(),
                model_path: built.model_file.to_string_lossy().into_owned(),
                model_identity_json: serde_json::to_string(&built.model).unwrap(),
                onnx_runtime_path: None,
                scan_threads: None,
            })
            .unwrap();
        built.engine
    }

    /// [`Built`]: the index and the package, nothing installed.
    fn built(dir: &TempDir) -> Built {
        built_of(
            dir,
            false,
            format!("בראשית ברא אלהים את השמים ואת הארץ\n{PROBE}"),
        )
    }

    /// [`Built`] of `text` as the one book [`BOOK`], in an index whose `chunkKey` column
    /// counts as absent — its metadata records no recipe — when `without_column` asks.
    fn built_of(dir: &TempDir, without_column: bool, text: String) -> Built {
        built_of_books(dir, without_column, &[(BOOK, text)])
    }

    /// [`built_of`] with each of `books`, `(path, text)`, a book of its own.
    fn built_of_books(dir: &TempDir, without_column: bool, books: &[(&str, String)]) -> Built {
        built_with(dir, without_column, books, TextStorage::InIndex, None)
    }

    /// [`built_of_books`], each book's text kept as `storage` says, and the vectors built
    /// with the text of `library` when it is in a library database.
    fn built_with(
        dir: &TempDir,
        without_column: bool,
        books: &[(&str, String)],
        storage: TextStorage,
        library: Option<&Path>,
    ) -> Built {
        built_under(
            dir,
            without_column,
            books,
            storage,
            library,
            production_chunking(),
        )
    }

    /// [`built_with`], the vectors built under `chunking`.
    fn built_under(
        dir: &TempDir,
        without_column: bool,
        books: &[(&str, String)],
        storage: TextStorage,
        library: Option<&Path>,
        chunking: otzaria_semantic_search::semantic::chunker::ChunkerConfig,
    ) -> Built {
        let index = dir.path().join("index");
        fs::create_dir_all(&index).unwrap();
        if without_column {
            Index::create_in_dir(&index, current_schema()).unwrap();
            write_index_metadata(&index, &index_metadata(INDEX_SCHEMA_VERSION, None)).unwrap();
        }
        let mut engine = SearchEngine::new(index.to_str().unwrap());
        assert_eq!(engine.chunk_key_field.is_none(), without_column);
        for (order, (path, text)) in books.iter().enumerate() {
            engine
                .add_text_book_placed(
                    "בראשית".to_string(),
                    "/root".to_string(),
                    path.to_string(),
                    order as u32,
                    0,
                    text.clone(),
                    None,
                    storage,
                    None,
                )
                .unwrap();
        }
        engine.commit().unwrap();

        let model_file = mock::write_stub_onnx_package(&dir.path().join("model"));
        let model = ModelIdentity {
            family_id: "test-mock@0000000".to_string(),
            tokenizer_checksum: mock::stub_tokenizer_checksum(),
            embedding_dim: 64,
            pooling: "in-graph".to_string(),
            max_tokens: 512,
            query_max_tokens: None,
            embedding_text_version: chunking.embedding_text_version,
            normalization_version: chunking.normalization_version,
            chunking_identity: chunking.identity(),
            query_packages: vec![ModelPackage {
                checksum: validate_onnx_package(&model_file)
                    .unwrap()
                    .checksum()
                    .to_string(),
                quantization: "int8".to_string(),
            }],
        };
        let corpus =
            TantivyCorpus::from_engine(&engine, library, 30, "", chunking.clone()).unwrap();
        let package = dir.path().join("package");
        let report = build(
            BuildRequest {
                output_path: package.clone(),
                model_path: model_file.clone(),
                model: model.clone(),
                chunking,
                created_at: "2026-10-01T00:00:00Z".to_string(),
                batch_size: 2,
                codec: Default::default(),
                allow_non_semantic_backend: true,
            },
            &corpus,
        )
        .unwrap();
        let identity = IndexVersion {
            text: corpus.identity().unwrap().text,
            model: model.clone(),
            store: readable_store_identity(),
        };
        drop(corpus);
        Built {
            engine,
            model,
            model_file,
            package,
            manifest_sha256: report.manifest_sha256,
            identity,
        }
    }

    /// The probe line's id and the value its `chunkKey` column holds.
    fn probe(engine: &SearchEngine) -> (u64, u64) {
        let searcher = engine.index_reader.searcher();
        let text_f = engine.schema.get_field(TEXT_STORED_FIELD).unwrap();
        for (ord, reader) in searcher.segment_readers().iter().enumerate() {
            let ids = reader.fast_fields().u64("id").unwrap();
            let keys = reader.fast_fields().u64(CHUNK_KEY_FIELD).unwrap();
            for doc in reader.doc_ids_alive() {
                let stored: TantivyDocument =
                    searcher.doc(DocAddress::new(ord as u32, doc)).unwrap();
                if stored.get_first(text_f).and_then(|v| v.as_str()) == Some(PROBE) {
                    return (ids.first(doc).unwrap(), keys.first(doc).unwrap());
                }
            }
        }
        panic!("the probe line is indexed")
    }

    /// The probe line replaced by `text`, with the probe's id, its section and — the
    /// forgery — the probe's column value: a document the column cannot tell from the
    /// line the vector was built from.
    fn forge(engine: &mut SearchEngine, text: &str) {
        let (id, column) = probe(engine);
        let schema = engine.schema.clone();
        let field = |name: &str| schema.get_field(name).unwrap();
        let mut document = doc!(
            field("title") => "בראשית",
            field("reference") => "בראשית",
            field("text") => text,
            field(TEXT_STORED_FIELD) => text,
            field("id") => id,
            field("segment") => 1u64,
            field("isPdf") => false,
            field("filePath") => BOOK,
            field("topics") => Facet::from_text("/root").unwrap(),
            field("contentHash") => 0u64,
            field("textHash") => 0u64,
            field(SECTION_OFFSET_FIELD) => 0u64,
            field("lineHash") => 0u64
        );
        document.add_u64(field(CHUNK_KEY_FIELD), column);
        let writer = engine.writer_mut().unwrap();
        writer
            .delete_query(Box::new(IdSetQuery::new([id])))
            .unwrap();
        writer.add_document(document).unwrap();
        engine.commit().unwrap();
    }

    fn search(engine: &SearchEngine, mode: SemanticRetrievalMode) -> SemanticSearchResponse {
        engine
            .search_semantic(
                PROBE.to_string(),
                Vec::new(),
                10,
                0,
                SemanticLexicalMode::Exact,
                0,
                mode,
                None,
                false,
                false,
                None,
                &SemanticCancellationToken::new(),
            )
            .unwrap()
    }

    #[test]
    fn the_probe_line_is_a_semantic_match_before_anything_is_forged() {
        let dir = TempDir::new().unwrap();
        let engine = opened(&dir);
        let response = search(&engine, SemanticRetrievalMode::SemanticOnly);
        let top = response.results.first().expect("a semantic hit");
        assert_eq!(top.snippet_html, PROBE);
        assert_eq!(top.source, SemanticResultSource::Semantic);
        assert!(response.fallback_reason.is_none());
    }

    #[test]
    fn semantic_benchmark_opens_vectors_without_enabling_index_writes() {
        use crate::smart_search_bench::{open_read_only, open_vectors, SemanticInputs};

        let dir = TempDir::new().unwrap();
        let built = built(&dir);
        let model_identity = dir.path().join("model.json");
        fs::write(&model_identity, serde_json::to_vec(&built.model).unwrap()).unwrap();
        let inputs = SemanticInputs {
            vectors: dir.path().join("vectors"),
            model: built.model_file.clone(),
            model_identity,
            onnx_runtime: None,
        };
        // Install an actual small vector set with the deterministic mock backend.
        // Close the writer before snapshotting every lexical index file on Windows.
        drop(open(&dir, built));
        let index = dir.path().join("index");
        let files = || {
            fs::read_dir(&index)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = files();
        let mut engine = open_read_only(&index).unwrap();
        let public_open = || SemanticArtifactInput {
            vectors_dir: inputs.vectors.to_string_lossy().into_owned(),
            model_path: inputs.model.to_string_lossy().into_owned(),
            model_identity_json: fs::read_to_string(&inputs.model_identity).unwrap(),
            onnx_runtime_path: None,
            scan_threads: None,
        };
        assert_eq!(
            engine
                .open_semantic_artifact(public_open())
                .err()
                .unwrap()
                .kind,
            SemanticErrorKind::ReadOnlySession
        );
        // Exercise the same vector-opening route as all five real-data benchmarks.
        open_vectors(&engine, &inputs).unwrap();
        open_vectors(&engine, &inputs).unwrap();
        let response = search(&engine, SemanticRetrievalMode::SemanticOnly);
        assert!(response.fallback_reason.is_none());
        let top = response.results.first().expect("a semantic hit");
        assert_eq!(top.snippet_html, PROBE);
        assert_eq!(top.source, SemanticResultSource::Semantic);
        for refusal in [
            engine.open_index_writer(),
            engine.clear(),
            engine.set_book_order(vec![]).map(|_| ()),
        ] {
            assert!(refusal.unwrap_err().to_string().contains("read-only"));
        }
        assert_eq!(
            engine.reset_semantic_index().err().unwrap().kind,
            SemanticErrorKind::ReadOnlySession
        );
        assert_eq!(
            engine
                .open_semantic_artifact(public_open())
                .err()
                .unwrap()
                .kind,
            SemanticErrorKind::ReadOnlySession
        );
        engine.disable_semantic();
        drop(engine);
        assert_eq!(files(), before, "benchmark changed the lexical index");
    }

    #[test]
    fn a_line_whose_text_is_not_the_vectors_is_not_shown_as_semantic() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        forge(&mut engine, "שורה אחרת לגמרי שאין לה דבר עם הווקטור");

        let response = search(&engine, SemanticRetrievalMode::SemanticOnly);
        assert!(
            response
                .results
                .iter()
                .all(|result| result.file_path != BOOK || result.segment != 1),
            "the forged line is not shown as the vector's: {:?}",
            response
                .results
                .iter()
                .map(|result| &result.snippet_html)
                .collect::<Vec<_>>()
        );
        assert!(
            response
                .fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("were not shown as such")),
            "{:?}",
            response.fallback_reason
        );
    }

    /// A line whose text is in the library database is resolved and verified as a stored
    /// one is, with the column or without; a row changed since it was indexed is not shown
    /// as its vector's; and while the database cannot be read, the semantic half fails
    /// rather than keying empty lines.
    #[test]
    fn a_library_line_is_a_semantic_match_like_a_stored_one() {
        let _guard = crate::line_source::TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let rows = ["בראשית ברא אלהים את השמים ואת הארץ", PROBE];
        for without_column in [false, true] {
            crate::line_source::reset_for_tests();
            let dir = TempDir::new().unwrap();
            let db = dir.path().join("seforim.db");
            crate::external_text_tests::write_book(&db, 1, &rows);
            configure_line_source(db.to_string_lossy().into_owned()).unwrap();
            let built = built_with(
                &dir,
                without_column,
                &[("id:1", rows.join("\n"))],
                TextStorage::LibraryDb,
                Some(&db),
            );
            let engine = open(&dir, built);
            let context = format!("without column: {without_column}");

            let response = search(&engine, SemanticRetrievalMode::SemanticOnly);
            let top = response.results.first().expect("a semantic hit");
            assert_eq!(top.snippet_html, PROBE, "{context}");
            assert_eq!(top.source, SemanticResultSource::Semantic, "{context}");
            assert_eq!(top.text_status, TextStatus::Ok, "{context}");
            assert!(response.fallback_reason.is_none(), "{context}");

            suspend_line_source().unwrap();
            let response = search(&engine, SemanticRetrievalMode::SemanticOnly);
            resume_line_source().unwrap();
            assert!(
                response
                    .results
                    .iter()
                    .all(|result| result.source != SemanticResultSource::Semantic),
                "{context}"
            );
            assert!(response.fallback_reason.is_some(), "{context}");

            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "UPDATE line_content SET content = ?1 WHERE id = \
                     (SELECT id FROM line WHERE bookId = 1 AND lineIndex = 1)",
                ["שורה אחרת לגמרי שאין לה דבר עם הווקטור"],
            )
            .unwrap();
            drop(conn);
            let response = search(&engine, SemanticRetrievalMode::SemanticOnly);
            assert!(
                response
                    .results
                    .iter()
                    .all(|result| result.source != SemanticResultSource::Semantic
                        || result.segment != 1),
                "the changed line is not shown as the vector's; {context}"
            );
        }
        crate::line_source::reset_for_tests();
    }

    /// A repeated query must verify the current library text even when an unpaused
    /// writer changed it, including when a lexical window refreshed the row caches first.
    #[test]
    fn cached_library_hits_are_reverified_after_an_unpaused_write() {
        let _guard = crate::line_source::TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for without_column in [false, true] {
            for refresh_rows_first in [false, true] {
                for mode in [
                    SemanticRetrievalMode::SemanticOnly,
                    SemanticRetrievalMode::Hybrid,
                ] {
                    crate::line_source::reset_for_tests();
                    let dir = TempDir::new().unwrap();
                    let db = dir.path().join("seforim.db");
                    let rows = ["<h1>בראשית</h1>", PROBE];
                    crate::external_text_tests::write_book(&db, 1, &rows);
                    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
                    let built = built_with(
                        &dir,
                        without_column,
                        &[("id:1", rows.join("\n"))],
                        TextStorage::LibraryDb,
                        Some(&db),
                    );
                    let engine = open(&dir, built);
                    let context = format!("without_column={without_column}, refresh_rows_first={refresh_rows_first}, mode={mode:?}");
                    let probe_is_semantic = |r: &SemanticSearchResult| {
                        r.segment == 1 && r.source != SemanticResultSource::Lexical
                    };
                    let first = search(&engine, mode);
                    let expansions = || {
                        engine
                            .semantic_sessions
                            .expansions
                            .load(std::sync::atomic::Ordering::Relaxed)
                    };
                    let expansions_before_repeat = expansions();
                    let id = first
                        .results
                        .iter()
                        .find(|r| probe_is_semantic(r))
                        .unwrap_or_else(|| panic!("no semantic probe; {context}"))
                        .id;
                    // The second identical query is served by the search's session.
                    assert!(
                        search(&engine, mode).results.iter().any(probe_is_semantic),
                        "{context}"
                    );
                    assert_eq!(
                        expansions(),
                        expansions_before_repeat,
                        "unchanged queries must still use the session; {context}"
                    );
                    let conn = rusqlite::Connection::open(&db).unwrap();
                    let update = |text: &str| {
                        conn.execute(
                            "UPDATE line_content SET content = ?1 WHERE id = \
                                 (SELECT id FROM line WHERE bookId = 1 AND lineIndex = 1)",
                            [text],
                        )
                        .unwrap();
                    };
                    update("שורה אחרת לגמרי שאין לה דבר עם הווקטור");
                    if refresh_rows_first {
                        let doc = engine.get_document_by_id(id).unwrap().unwrap();
                        assert_eq!(doc.text_status, TextStatus::Stale, "{context}");
                    }
                    for _ in 0..2 {
                        let response = search(&engine, mode);
                        assert!(
                            response.results.iter().all(|r| !probe_is_semantic(r)),
                            "cached semantic hit survived changed text; {context}: {:?}",
                            response
                                .results
                                .iter()
                                .map(|r| (&r.snippet_html, r.text_status, r.source))
                                .collect::<Vec<_>>()
                        );
                    }
                    // A second commit must also discard any cached rejection.
                    update(PROBE);
                    assert!(
                        search(&engine, mode).results.iter().any(probe_is_semantic),
                        "restored row; {context}"
                    );
                }
            }
        }
        crate::line_source::reset_for_tests();
    }

    /// A short line, under the 20 characters a line stands alone at, with letters enough
    /// for a `lineHash`.
    const SHORT: &str = "ויאמר משה אל העם";
    /// How many times the books below hold [`SHORT`] among lines of their own.
    const RECURS: usize = 300;

    /// [`SHORT`] in the window its key spans.
    const FIRST: [&str; 5] = [
        "ויקרא משה אל כל ישראל ויאמר אליהם",
        "שמע ישראל את החוקים ואת המשפטים",
        SHORT,
        "אשר אנכי דובר באזניכם היום",
        "ולמדתם אותם ושמרתם לעשותם",
    ];

    /// [`FIRST`] cut into lines at another space: the same key, other `lineHash`es.
    const RESPLIT: [&str; 5] = [
        "ויקרא משה אל כל ישראל",
        "ויאמר אליהם שמע ישראל את החוקים ואת המשפטים",
        SHORT,
        FIRST[3],
        FIRST[4],
    ];

    /// Blocks of [`SHORT`] in its window: `first`, `recurs` of `neighbour(n)`, `last`.
    fn book_of(
        first: &[&str; 5],
        recurs: usize,
        neighbour: impl Fn(usize) -> String,
        last: &[&[&str; 5]],
    ) -> String {
        let blocks = (0..recurs).map(|block| {
            let n = 4 * block;
            [
                neighbour(n),
                neighbour(n + 1),
                SHORT.to_string(),
                neighbour(n + 2),
                neighbour(n + 3),
            ]
            .join("\n")
        });
        std::iter::once(first.join("\n"))
            .chain(blocks)
            .chain(last.iter().map(|block| block.join("\n")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// [`FIRST`], `RECURS` blocks of `neighbour(n)`, and [`FIRST`] again.
    fn recurring(neighbour: impl Fn(usize) -> String) -> String {
        book_of(&FIRST, RECURS, neighbour, &[&FIRST])
    }

    /// Whether `result` is a [`SHORT`] line of [`book_of`], third in every block. By
    /// position: the snippet of a short line is the window it was embedded with.
    fn is_short(result: &SemanticSearchResult) -> bool {
        result.segment % 5 == 2
    }

    /// The line of each result of `response` that is [`SHORT`], in the page's order.
    fn shorts(response: &SemanticSearchResponse) -> Vec<u64> {
        response
            .results
            .iter()
            .filter(|result| is_short(result))
            .map(|result| result.segment)
            .collect()
    }

    /// The text of `block`'s short line as it is embedded: what a query must repeat for
    /// that line's vector to come first.
    fn window(text: &str, block: usize) -> String {
        text.lines()
            .skip(5 * block)
            .take(5)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn resolver_counts(engine: &SearchEngine) -> (u64, usize) {
        let cache = engine
            .semantic_resolver
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (cache.recomputes, cache.worst_failures)
    }

    fn semantic_page(engine: &SearchEngine, query: &str, limit: u32) -> SemanticSearchResponse {
        engine
            .search_semantic(
                query.to_string(),
                Vec::new(),
                limit,
                0,
                SemanticLexicalMode::Exact,
                0,
                SemanticRetrievalMode::SemanticOnly,
                None,
                false,
                false,
                None,
                &SemanticCancellationToken::new(),
            )
            .unwrap()
    }

    /// Synthetic note markup affects display only, after the same plain text has
    /// been embedded, verified, fused and ranked.
    #[test]
    fn note_provenance_semantic_pages_and_passages_keep_the_note_boundary() {
        let _guard = crate::external_text_tests::guard();
        let dir = TempDir::new().unwrap();
        let note = "דברי ההערה המפורשים מסבירים את הלשון הזאת";
        let raw = format!("הגוף המקורי נמצא בתחילת השורה. <i class='footnote'>{note}.</i> המשך גוף המקור נמצא אחרי ההערה.");
        let engine = open(&dir, built_of(&dir, false, raw));
        for mode in [
            SemanticRetrievalMode::SemanticOnly,
            SemanticRetrievalMode::Hybrid,
            SemanticRetrievalMode::LexicalOnly,
        ] {
            let response = engine
                .search_semantic(
                    note.into(),
                    vec![],
                    10,
                    0,
                    SemanticLexicalMode::Exact,
                    0,
                    mode,
                    None,
                    false,
                    false,
                    None,
                    &SemanticCancellationToken::new(),
                )
                .unwrap();
            assert!(
                response.fallback_reason.is_none(),
                "{:?}",
                response.fallback_reason
            );
            assert_eq!(response.results.len(), 1);
            let hit = &response.results[0];
            assert_eq!(hit.text_status, TextStatus::Ok);
            assert!(
                hit.snippet_html.contains("<otzaria-note>"),
                "{:?}: {}",
                mode,
                hit.snippet_html
            );
            assert!(hit.snippet_html.contains("</otzaria-note>"));
            assert!(!hit.snippet_html.contains("&lt;otzaria-note"));
            assert_eq!(
                hit.is_highlighted,
                mode != SemanticRetrievalMode::SemanticOnly
            );
        }
        let highlights = engine
            .semantic_passage_highlights(
                note.into(),
                vec![SemanticHighlightTarget {
                    file_path: BOOK.into(),
                    id: (1u64 << 32) + 1,
                    window: 0,
                }],
                &SemanticCancellationToken::new(),
            )
            .unwrap();
        assert_eq!(highlights.len(), 1);
        assert!(highlights[0].is_highlighted, "{:?}", highlights[0]);
        assert!(
            highlights[0]
                .snippet_html
                .contains("<otzaria-note><mark>דברי"),
            "{}",
            highlights[0].snippet_html
        );
        assert!(
            highlights[0]
                .snippet_html
                .contains("</mark></otzaria-note>"),
            "{}",
            highlights[0].snippet_html
        );
    }

    #[test]
    fn note_provenance_semantic_late_window_keeps_ellipsis_outside_the_note() {
        let _guard = crate::external_text_tests::guard();
        let dir = TempDir::new().unwrap();
        let long = long_line();
        let piece = window_piece(&long, 2);
        let raw = format!("<i class='footnote'>{long}</i>");
        let engine = open(&dir, built_of(&dir, false, raw));
        let response = semantic_page(&engine, &piece, 10);
        let hit = response.results.first().expect("semantic match");
        assert_eq!(hit.matched_window, 2);
        assert!(
            hit.snippet_html.starts_with("…<otzaria-note>"),
            "{}",
            hit.snippet_html
        );
        assert!(hit.snippet_html.contains("</otzaria-note>"));
        assert!(!hit.is_highlighted);
        let marks = engine
            .semantic_passage_highlights(
                piece,
                vec![SemanticHighlightTarget {
                    file_path: hit.file_path.clone(),
                    id: hit.id,
                    window: hit.matched_window,
                }],
                &SemanticCancellationToken::new(),
            )
            .unwrap();
        assert!(marks[0].is_highlighted);
        assert!(marks[0].snippet_html.contains("<otzaria-note>"));
        assert!(marks[0].snippet_html.contains("<mark>"));
    }

    #[test]
    fn note_provenance_semantic_short_line_context_keeps_its_neighbours_notes() {
        let _guard = crate::external_text_tests::guard();
        let dir = TempDir::new().unwrap();
        let before = "דברי הערה מפורטים שנמצאים לפני השורה הקצרה הזאת";
        let after = "המשך גוף ארוך שנמצא לאחר השורה הקצרה הזאת";
        let short = "ויהי ערב";
        let raw = format!("<i class='footnote'>{before}</i>\n{short}\n{after}");
        let engine = open(&dir, built_of(&dir, false, raw));
        let query = format!("{before} {short} {after}");
        let response = semantic_page(&engine, &query, 10);
        let hit = response
            .results
            .iter()
            .find(|hit| hit.segment == 1)
            .expect("short line's context match");
        assert_eq!(hit.text_status, TextStatus::Ok);
        assert!(
            hit.snippet_html
                .starts_with(&format!("<otzaria-note>{before}</otzaria-note> {short}")),
            "{}",
            hit.snippet_html
        );
    }

    /// R2 of the acceptance run. Without the column a short line's repeats are the lines
    /// of its `lineHash` — here three hundred and one — and its key spans its neighbours,
    /// which differ at all but one. A hit recomputes one line that does not hold its key,
    /// then reads its span, and the others' windows cannot spell the text: all the hits a
    /// page of ten scans for recompute fewer lines together than one of them did alone,
    /// three hundred. The real repeat, neighbours and all, is still a result.
    #[test]
    fn a_short_line_recurring_among_other_lines_is_bounded_and_keeps_its_real_repeat() {
        let dir = TempDir::new().unwrap();
        let text = recurring(|n| format!("שורה {n} של הספר הגדול שאינה חוזרת בשום מקום"));
        let engine = open(&dir, built_of(&dir, true, text.clone()));

        let response = semantic_page(&engine, &window(&text, 0), 10);
        assert!(
            response.fallback_reason.is_none(),
            "{:?}",
            response.fallback_reason
        );
        let shorts: Vec<u64> = response
            .results
            .iter()
            .filter(|result| is_short(result))
            .map(|result| result.segment)
            .collect();
        let repeat = 5 * (RECURS as u64 + 1) + 2;
        assert_eq!(
            shorts[..2],
            [2, repeat],
            "the line and its real repeat, first"
        );

        let (recomputes, worst) = resolver_counts(&engine);
        assert!(
            worst <= 1,
            "a hit recomputed {worst} lines that did not hold its key"
        );
        assert!(recomputes < RECURS as u64, "{recomputes} keys recomputed");
    }

    /// Neighbours with no `lineHash` — under twelve letters — leave the windows nothing to
    /// tell apart, and every block's line is a candidate for every other's. A hit then
    /// stops after [`MAX_FAILED_REPEATS`] lines recomputed that did not hold its key: a
    /// page of one scans for four hits, and recomputes at most four budgets' worth, where
    /// each hit read three hundred lines before.
    #[test]
    fn a_short_line_whose_windows_cannot_be_told_apart_stops_at_the_budget() {
        let dir = TempDir::new().unwrap();
        let text = recurring(|n| format!("פסוק {n}"));
        let engine = open(&dir, built_of(&dir, true, text.clone()));

        let response = semantic_page(&engine, &window(&text, 1), 1);
        assert!(
            response.fallback_reason.is_none(),
            "{:?}",
            response.fallback_reason
        );
        let (recomputes, worst) = resolver_counts(&engine);
        assert_eq!(worst, MAX_FAILED_REPEATS, "every hit reached the budget");
        assert!(
            recomputes <= 4 * MAX_FAILED_REPEATS as u64,
            "{recomputes} keys recomputed"
        );
    }

    /// A text held three times, once cut into lines at other spaces, after a line that
    /// fails: all three are shown, with the column or without.
    #[test]
    fn a_repeat_whose_lines_are_cut_at_other_spaces_is_found_with_the_column_or_without() {
        let text = book_of(
            &FIRST,
            1,
            |n| format!("שורה {n} של הספר הגדול שאינה חוזרת בשום מקום"),
            &[&RESPLIT, &FIRST],
        );
        for without_column in [false, true] {
            let dir = TempDir::new().unwrap();
            let engine = open(&dir, built_of(&dir, without_column, text.clone()));

            let response = semantic_page(&engine, &window(&text, 0), 10);
            assert!(
                response.fallback_reason.is_none(),
                "{:?}",
                response.fallback_reason
            );
            let shorts = shorts(&response);
            assert_eq!(
                shorts.get(..3),
                Some(&[2, 12, 17][..]),
                "without column: {without_column}; every line of the text, first: {shorts:?}"
            );
        }
    }

    /// R2's shape at the size of the library's worst book: a failed recompute per hit,
    /// and the real repeats, however they are cut into lines, still shown.
    #[test]
    fn a_short_line_among_nine_hundred_others_costs_one_failure_and_keeps_every_repeat() {
        let dir = TempDir::new().unwrap();
        let recurs = 900;
        let text = book_of(
            &FIRST,
            recurs,
            |n| format!("שורה {n} של הספר הגדול שאינה חוזרת בשום מקום"),
            &[&RESPLIT, &FIRST],
        );
        let engine = open(&dir, built_of(&dir, true, text.clone()));

        let response = semantic_page(&engine, &window(&text, 0), 10);
        assert!(
            response.fallback_reason.is_none(),
            "{:?}",
            response.fallback_reason
        );
        let repeats = [5 * (recurs as u64 + 1) + 2, 5 * (recurs as u64 + 2) + 2];
        assert_eq!(shorts(&response)[..3], [2, repeats[0], repeats[1]]);
        let (recomputes, worst) = resolver_counts(&engine);
        assert_eq!(worst, 1, "failed recomputes of the worst hit");
        assert!(recomputes < 20, "{recomputes} keys recomputed");
    }

    /// A real repeat behind more candidates the columns cannot tell from it than the budget
    /// allows is shown with the column, and not without it.
    #[test]
    fn a_repeat_past_the_budget_is_found_only_with_the_column() {
        let first = ["פסוק א", "פסוק ב", SHORT, "פסוק ג", "פסוק ד"];
        let recurs = 2 * MAX_FAILED_REPEATS;
        let text = book_of(&first, recurs, |n| format!("פסוק {n}"), &[&first]);
        let repeat = 5 * (recurs as u64 + 1) + 2;
        for without_column in [false, true] {
            let dir = TempDir::new().unwrap();
            let engine = open(&dir, built_of(&dir, without_column, text.clone()));

            let response = semantic_page(&engine, &window(&text, 0), 10);
            assert!(
                response.fallback_reason.is_none(),
                "{:?}",
                response.fallback_reason
            );
            let shorts = shorts(&response);
            assert!(shorts.contains(&2), "{shorts:?}");
            assert_eq!(shorts.contains(&repeat), !without_column, "{shorts:?}");
            if without_column {
                assert_eq!(resolver_counts(&engine).1, MAX_FAILED_REPEATS);
            }
        }
    }

    /// A text the cap cut, held twice in each of two books among twenty other windows of
    /// its short line: the first book's other windows do not spend the budget the second
    /// book's repeat needs.
    #[test]
    fn a_capped_texts_repeat_in_a_second_book_is_found_within_the_budget() {
        let long = |word: &str| format!("{word} ").repeat(40).trim_end().to_string();
        let (a, b, c, d) = (long("אברהם"), long("יצחק"), long("ויעקב"), long("ויוסף"));
        let capped = [a.as_str(), b.as_str(), SHORT, c.as_str(), d.as_str()];
        let recurs = 20;
        let text = book_of(
            &capped,
            recurs,
            |n| format!("שורה {n} של הספר הגדול שאינה חוזרת בשום מקום"),
            &[&capped],
        );
        let query: String = window(&text, 0).chars().take(512).collect();
        let other = "/books/exodus.txt";
        let repeat = 5 * (recurs as u64 + 1) + 2;
        for without_column in [false, true] {
            let dir = TempDir::new().unwrap();
            let books = [(BOOK, text.clone()), (other, text.clone())];
            let engine = open(&dir, built_of_books(&dir, without_column, &books));

            let response = semantic_page(&engine, query.trim_end(), 10);
            assert!(
                response.fallback_reason.is_none(),
                "{:?}",
                response.fallback_reason
            );
            let mut lines: Vec<(&str, u64)> = response
                .results
                .iter()
                .filter(|result| is_short(result))
                .map(|result| (result.file_path.as_str(), result.segment))
                .take(4)
                .collect();
            lines.sort();
            assert_eq!(
                lines,
                [(other, 2), (other, repeat), (BOOK, 2), (BOOK, repeat)],
                "without column: {without_column}"
            );
        }
    }

    /// A heading too short to stand alone is embedded with the lines of its section: its
    /// result shows, and its passage highlight marks, that text rather than the heading.
    #[test]
    fn a_short_headings_result_shows_and_marks_the_text_it_was_embedded_as() {
        const HEADING: &str = "סימן כה";
        const FIRST: &str = "ויאמר אלהים יהי אור ויהי אור, וירא אלהים את האור כי טוב,";
        const SECOND: &str = "ויבדל אלהים בין האור ובין החשך, ויקרא אלהים לאור יום.";
        let text = [
            "<h1>בראשית</h1>",
            "<h2>סימן כה</h2>",
            FIRST,
            SECOND,
            "<h2>סימן כו</h2>",
            "ויהי ערב ויהי בקר יום אחד, ויאמר אלהים יהי רקיע בתוך המים.",
        ]
        .join("\n");
        let embedded = [HEADING, FIRST, SECOND].join(" ");
        let dir = TempDir::new().unwrap();
        let engine = open(&dir, built_of(&dir, false, text));

        let response = semantic_page(&engine, &embedded, 10);
        assert!(
            response.fallback_reason.is_none(),
            "{:?}",
            response.fallback_reason
        );
        let heading = response
            .results
            .iter()
            .find(|result| result.segment == 1)
            .expect("the heading's vector is a result");
        assert_eq!(heading.source, SemanticResultSource::Semantic);
        assert_eq!(heading.snippet_html, embedded);
        assert_eq!(heading.matched_window, 0);

        let marked = engine
            .semantic_passage_highlights(
                "ויבדל אלהים בין האור ובין החשך".to_string(),
                vec![SemanticHighlightTarget {
                    file_path: BOOK.to_string(),
                    id: heading.id,
                    window: heading.matched_window,
                }],
                &SemanticCancellationToken::new(),
            )
            .unwrap();
        assert!(marked[0].is_highlighted, "{marked:?}");
        assert!(
            marked[0].snippet_html.contains("<mark>") && marked[0].snippet_html.contains(HEADING),
            "{}",
            marked[0].snippet_html
        );
    }

    /// A line of 300 distinct words, some 1,500 characters.
    fn long_line() -> String {
        let letters: Vec<char> = "אבגדהוזחטיכלמנסעפצקרשת".chars().collect();
        (0..300)
            .map(|n| format!("{}{}ים", letters[n % 22], letters[(n / 22) % 22]))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn long_book(long: &str) -> String {
        format!("בראשית ברא אלהים את השמים ואת הארץ\n{long}\n{PROBE}")
    }

    /// What the long line's window `window` is in its text.
    fn window_piece(long: &str, window: u8) -> String {
        let (span, _) = crate::semantic_keys::window_span(long, window).unwrap();
        long.chars().skip(span.start).take(span.len()).collect()
    }

    /// A query quoting a long line's middle finds it by that window, which is named, shown
    /// and marked; the column holds only the first window.
    #[test]
    fn a_long_lines_middle_is_found_shown_and_marked_by_its_window() {
        use otzaria_semantic_search::semantic::chunk_key::ChunkKey;
        use otzaria_semantic_search::semantic::chunker::Chunker;
        let long = long_line();
        let piece = window_piece(&long, 2);
        let first_word = long.split(' ').next().unwrap().to_string();
        assert!(!piece.contains(&first_word));
        for without_column in [false, true] {
            let dir = TempDir::new().unwrap();
            let engine = open(&dir, built_of(&dir, without_column, long_book(&long)));
            if !without_column {
                let searcher = engine.index_reader.searcher();
                let address = engine
                    .address_in_book(&searcher, BOOK, (1u64 << 32) + 2)
                    .unwrap()
                    .unwrap();
                let column = searcher
                    .segment_reader(address.segment_ord)
                    .fast_fields()
                    .u64(CHUNK_KEY_FIELD)
                    .unwrap()
                    .first(address.doc_id);
                let alone = [LineRef {
                    text: &long,
                    section: 0,
                }];
                let chunker = Chunker::new(production_chunking()).unwrap();
                let first = chunker.embedded_text(&alone, 0).unwrap();
                assert_eq!(column, Some(ChunkKey::of(&first).column_value()));
            }

            let response = semantic_page(&engine, &piece, 10);
            assert!(
                response.fallback_reason.is_none(),
                "{:?}",
                response.fallback_reason
            );
            let top = &response.results[0];
            assert_eq!(top.segment, 1, "without column: {without_column}");
            assert_eq!(top.source, SemanticResultSource::Semantic);
            assert_eq!(top.matched_window, 2, "without column: {without_column}");
            // Shown within the snippet budget, as every semantic result's line is.
            let shown = format!("…{piece}…");
            assert_eq!(
                top.snippet_html,
                bounded_plain_snippet(&shown, HighlightConfig::default().max_chars)
            );
            assert!(
                response
                    .results
                    .iter()
                    .filter(|result| result.segment == 1)
                    .count()
                    == 1,
                "the line is one result, whichever of its windows match"
            );

            let target = |window| SemanticHighlightTarget {
                file_path: BOOK.to_string(),
                id: top.id,
                window,
            };
            let query: String = piece
                .split(' ')
                .skip(20)
                .take(8)
                .collect::<Vec<_>>()
                .join(" ");
            let marked = engine
                .semantic_passage_highlights(
                    query,
                    vec![target(top.matched_window), target(0)],
                    &SemanticCancellationToken::new(),
                )
                .unwrap();
            assert!(marked[0].is_highlighted, "{marked:?}");
            assert!(marked[0].snippet_html.contains("<mark>"));
            assert!(
                !marked[0].snippet_html.contains(&first_word),
                "{}",
                marked[0].snippet_html
            );
            assert_ne!(marked[0].snippet_html, marked[1].snippet_html);
        }
    }

    /// A set built without windows — the published family's — opens with this build and
    /// resolves as before: one vector a line, every hit its line's first window.
    #[test]
    fn a_set_without_windows_opens_and_resolves_as_before() {
        let long = long_line();
        let mut counts = Vec::new();
        for chunking in [production_chunking().anchor(), production_chunking()] {
            let dir = TempDir::new().unwrap();
            let built = built_under(
                &dir,
                false,
                &[(BOOK, long_book(&long))],
                TextStorage::InIndex,
                None,
                chunking.clone(),
            );
            let engine = open(&dir, built);
            let status = engine.semantic_status();
            assert!(status.available, "{:?}", status.last_error);
            counts.push(status.vector_count);
            let top = search(&engine, SemanticRetrievalMode::SemanticOnly);
            let top = top.results.first().expect("a semantic hit");
            assert_eq!(top.snippet_html, PROBE);
            assert_eq!(top.matched_window, 0);
            if chunking.window_chars == 0 {
                let response = semantic_page(&engine, &window_piece(&long, 2), 10);
                assert!(response.fallback_reason.is_none());
                assert!(response
                    .results
                    .iter()
                    .all(|result| result.matched_window == 0
                        && !result.snippet_html.starts_with('…')));
            }
        }
        let windows = crate::semantic_keys::window_keys(&long).len() as u32;
        assert_eq!(counts, [3, 3 + windows]);
    }

    /// The shown window is the embedded one: capped at `max_chunk_chars`, a blank line's
    /// spaces collapsed, and a short line mid-section with the two lines before it too.
    #[test]
    fn a_short_lines_window_is_shown_capped_and_with_its_section_on_both_sides() {
        let long = "אברהם הוליד את יצחק ".repeat(30).trim_end().to_string();
        let around = [
            "שורה ראשונה ארוכה דיה לעמוד לבדה",
            "שורה שנייה ארוכה דיה לעמוד לבדה",
            "ויהי ערב",
            "שורה שלישית ארוכה דיה לעמוד לבדה",
            "שורה רביעית ארוכה דיה לעמוד לבדה",
        ];
        let mut lines = vec!["<h1>בראשית</h1>", "<h2>סימן כה</h2>", "", &long];
        lines.push("<h2>סימן כו</h2>");
        lines.extend(around);
        lines.push("<h2>סימן כז</h2>");
        let dir = TempDir::new().unwrap();
        let engine = built_of(&dir, false, lines.join("\n")).engine;
        let searcher = engine.index_reader.searcher();
        // The text as the index stores it, a heading's without its tags.
        let line = |ordinal: u64, text| {
            let id = (1u64 << 32) + ordinal + 1;
            let address = engine
                .address_in_book(&searcher, BOOK, id)
                .unwrap()
                .unwrap();
            (address, BOOK, id, text, 0)
        };
        let shown_texts =
            ["סימן כה", around[0], around[2]].map(|text| DisplayText::plain(text.to_string()));
        let asked = [
            line(1, &shown_texts[0]),
            line(5, &shown_texts[1]),
            line(7, &shown_texts[2]),
        ];
        let shown = engine.embedded_window_texts(&searcher, &asked).unwrap();

        let max = production_chunking().max_chunk_chars;
        let capped: String = format!("סימן כה  {long}").chars().take(max).collect();
        let capped = capped.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(shown[0].as_deref(), Some(capped.as_str()));
        assert_eq!(shown[1], None, "a line long enough stands alone");
        assert_eq!(shown[2].as_deref(), Some(around.join(" ").as_str()));
    }

    /// A filtered search after a commit that left the book it admits alone plans again —
    /// a commit is a new generation of the index — and reads none of the book's lines:
    /// its postings are the segments they were, so the arrivals the view keeps for them
    /// hold.
    #[test]
    fn a_plan_after_a_commit_that_left_its_book_alone_reads_none_of_it() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        let filtered = |engine: &SearchEngine| {
            engine
                .search_semantic(
                    PROBE.to_string(),
                    vec!["/root".to_string()],
                    10,
                    0,
                    SemanticLexicalMode::Exact,
                    0,
                    SemanticRetrievalMode::SemanticOnly,
                    None,
                    false,
                    false,
                    None,
                    &SemanticCancellationToken::new(),
                )
                .unwrap()
        };
        let walks = |engine: &SearchEngine| {
            engine
                .semantic_resolver
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .walks
        };
        assert_eq!(filtered(&engine).results[0].snippet_html, PROBE);
        assert_eq!(walks(&engine), 1, "the first plan reads the book");
        engine
            .add_text_book_placed(
                "ספר אחר".to_string(),
                "/other".to_string(),
                "/books/other.txt".to_string(),
                1,
                0,
                "שורה בספר אחר ארוכה דיה לעמוד לבדה".to_string(),
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        assert_eq!(filtered(&engine).results[0].snippet_html, PROBE);
        assert_eq!(
            walks(&engine),
            0,
            "a plan of the new generation reads none of the book it admits"
        );
    }

    const OTHER: &str = "/books/other.txt";

    fn unfiltered(
        engine: &SearchEngine,
        query: &str,
        mode: SemanticRetrievalMode,
    ) -> Result<SemanticSearchResponse, SemanticError> {
        engine.search_semantic(
            query.to_string(),
            Vec::new(),
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            mode,
            None,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
    }

    /// The probe's lines an unfiltered semantic-only search returns, as (book, line).
    fn probe_lines(engine: &SearchEngine) -> Vec<(String, u64)> {
        let response = unfiltered(engine, PROBE, SemanticRetrievalMode::SemanticOnly).unwrap();
        assert!(
            response.semantic_available,
            "{:?}",
            response.fallback_reason
        );
        response
            .results
            .into_iter()
            .filter(|result| result.snippet_html == PROBE)
            .map(|result| (result.file_path, result.segment))
            .collect()
    }

    fn walks(engine: &SearchEngine) -> u64 {
        engine
            .semantic_resolver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .walks
    }

    /// A book that holds the probe at its line 1, committed.
    fn add_copy(engine: &mut SearchEngine, book: &str, order: u32) {
        engine
            .add_text_book_placed(
                "ספר אחר".to_string(),
                "/other".to_string(),
                book.to_string(),
                order,
                0,
                format!("שורה בספר אחר ארוכה דיה לעמוד לבדה\n{PROBE}"),
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
    }

    /// An unfiltered search plans the whole library once a generation, and after a commit
    /// reads only the book it added.
    #[test]
    fn an_unfiltered_plan_reads_each_book_once_and_then_only_what_a_commit_added() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        assert_eq!(probe_lines(&engine), [(BOOK.to_string(), 1)]);
        assert_eq!(walks(&engine), 1);
        unfiltered(
            &engine,
            "בראשית ברא אלהים",
            SemanticRetrievalMode::SemanticOnly,
        )
        .unwrap();
        assert_eq!(walks(&engine), 1, "the plan is kept for the generation");
        add_copy(&mut engine, OTHER, 1);
        assert_eq!(
            probe_lines(&engine),
            [(BOOK.to_string(), 1), (OTHER.to_string(), 1)]
        );
        assert_eq!(walks(&engine), 1, "only the book the commit added");
    }

    /// An unfiltered plan that fails leaves its search unplanned, semantic half and all,
    /// and is not tried again in that generation of the index.
    #[test]
    fn an_unfiltered_search_whose_plan_fails_goes_unplanned() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        add_copy(&mut engine, OTHER, 1);
        crate::semantic_resolver::FAIL_PLANS.with(|fail| fail.set(true));
        let failed = unfiltered(&engine, PROBE, SemanticRetrievalMode::Hybrid);
        crate::semantic_resolver::FAIL_PLANS.with(|fail| fail.set(false));
        let failed = failed.unwrap();
        assert!(failed.semantic_available, "{:?}", failed.fallback_reason);
        assert!(failed.results.iter().any(|result| result.file_path == BOOK
            && result.segment == 1
            && result.semantic_score.is_some()));
        assert_eq!(
            probe_lines(&engine),
            [(BOOK.to_string(), 1)],
            "not tried again"
        );
        add_copy(&mut engine, "/books/third.txt", 2);
        assert_eq!(
            probe_lines(&engine).len(),
            3,
            "planned in the next generation"
        );
    }

    /// Past its cap of arrivals the unfiltered plan is not held, and searches go unplanned.
    #[test]
    fn an_unfiltered_plan_past_its_cap_goes_unplanned() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        add_copy(&mut engine, OTHER, 1);
        add_copy(&mut engine, "/books/third.txt", 2);
        let cap = crate::semantic_resolver::LIBRARY_CAP.with(|cap| cap.replace(1));
        let capped = probe_lines(&engine);
        crate::semantic_resolver::LIBRARY_CAP.with(|limit| limit.set(cap));
        assert_eq!(capped, [(BOOK.to_string(), 1)]);
        add_copy(&mut engine, "/books/fourth.txt", 3);
        assert_eq!(probe_lines(&engine).len(), 4);
    }

    /// Changed-book windows have a bounded discovery budget. Exceeding it never
    /// exposes a partial arrival set as complete: unfiltered planning backs off and
    /// filtered planning backs off in the same way.
    #[test]
    fn changed_window_discovery_overflow_backs_off_without_partial_results() {
        use crate::semantic_resolver::LiveResolver;
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        let long = (0..600)
            .map(|n| format!("מילה{n:04}"))
            .collect::<Vec<_>>()
            .join(" ");
        engine.delete_documents_by_file_path(BOOK).unwrap();
        engine
            .add_text_book_placed(
                "ספר".into(),
                "/root".into(),
                BOOK.into(),
                0,
                0,
                long,
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        let vectors = dir.path().join("vectors");
        let generation = otzaria_semantic_search::semantic::segment_set::info(&vectors)
            .unwrap()
            .unwrap()
            .generation;
        let view = engine.semantic_set_view(&vectors, generation).unwrap();
        let mut resolver = LiveResolver::new(
            engine.index_reader.searcher(),
            engine.chunk_key_field,
            &engine.semantic_resolver,
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let unfiltered = SidecarSearchFilters {
            book_paths: None,
            facets: None,
            include_pdf: None,
        };
        let filtered = SidecarSearchFilters {
            book_paths: None,
            facets: Some(vec!["/root".into()]),
            include_pdf: None,
        };
        let previous = crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.replace(1));
        let unfiltered = resolver.plan(&unfiltered, &view, &cancel);
        let filtered = resolver.plan(&filtered, &view, &cancel);
        crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.set(previous));
        assert!(
            unfiltered.unwrap().is_none(),
            "no truncated unfiltered plan"
        );
        assert!(filtered.unwrap().is_none(), "no truncated filtered plan");
    }

    /// The working-set cap is shared across Rayon workers and changed books,
    /// rather than allowing each book to allocate the complete budget independently.
    #[test]
    fn changed_window_discovery_budget_is_shared_across_books() {
        use crate::semantic_resolver::LiveResolver;
        let dir = TempDir::new().unwrap();
        let mut engine = open(
            &dir,
            built_of_books(
                &dir,
                false,
                &[
                    (BOOK, "ספר ראשון עם שורה ארוכה מספיק לחיפוש".into()),
                    (OTHER, "ספר שני עם שורה אחרת ארוכה מספיק לחיפוש".into()),
                ],
            ),
        );
        let long = (0..180)
            .map(|n| format!("מילה{n:04}"))
            .collect::<Vec<_>>()
            .join(" ");
        let windows = crate::semantic_keys::window_keys(&long).len();
        assert!(windows > 1);
        for (order, path) in [BOOK, OTHER].into_iter().enumerate() {
            engine.delete_documents_by_file_path(path).unwrap();
            engine
                .add_text_book_placed(
                    "ספר".into(),
                    "/root".into(),
                    path.into(),
                    order as u32,
                    0,
                    long.clone(),
                    None,
                    TextStorage::InIndex,
                    None,
                )
                .unwrap();
        }
        engine.commit().unwrap();
        let vectors = dir.path().join("vectors");
        let generation = otzaria_semantic_search::semantic::segment_set::info(&vectors)
            .unwrap()
            .unwrap()
            .generation;
        let view = engine.semantic_set_view(&vectors, generation).unwrap();
        let mut resolver = LiveResolver::new(
            engine.index_reader.searcher(),
            engine.chunk_key_field,
            &engine.semantic_resolver,
        )
        .unwrap();
        let filters = SidecarSearchFilters {
            book_paths: None,
            facets: None,
            include_pdf: None,
        };
        let previous =
            crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.replace(windows + 1));
        let planned = resolver.plan(&filters, &view, &CancellationToken::new());
        crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.set(previous));
        assert!(
            planned.unwrap().is_none(),
            "each book fits, but their combined window working set exceeds the cap"
        );
    }

    /// A cold plan must stop walking books when the shared window budget overflows.
    /// A single worker makes the I/O bound deterministic, independent of host speed.
    #[test]
    fn window_discovery_overflow_stops_before_walking_the_rest_of_the_library() {
        use crate::semantic_resolver::LiveResolver;
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        engine.set_bulk_indexing(true).unwrap();
        let long = (0..600)
            .map(|n| format!("מילה{n:04}"))
            .collect::<Vec<_>>()
            .join(" ");
        for n in 0..32 {
            engine
                .add_text_book_placed(
                    "ספר".into(),
                    "/root".into(),
                    format!("/changed/{n:02}.txt"),
                    n + 1,
                    0,
                    long.clone(),
                    None,
                    TextStorage::InIndex,
                    None,
                )
                .unwrap();
        }
        engine.commit().unwrap();
        let vectors = dir.path().join("vectors");
        let generation = otzaria_semantic_search::semantic::segment_set::info(&vectors)
            .unwrap()
            .unwrap()
            .generation;
        let view = engine.semantic_set_view(&vectors, generation).unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        pool.install(|| {
            let mut resolver = LiveResolver::new(
                engine.index_reader.searcher(),
                engine.chunk_key_field,
                &engine.semantic_resolver,
            )
            .unwrap();
            let filters = SidecarSearchFilters {
                book_paths: None,
                facets: Some(vec!["/".into()]),
                include_pdf: None,
            };
            let previous = crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.replace(1));
            let plan = resolver.plan(&filters, &view, &CancellationToken::new());
            crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.set(previous));
            assert!(plan.unwrap().is_none(), "never expose a partial plan");
            let walked = walks(&engine);
            assert!(
                walked <= 2,
                "overflow should stop early, walked {walked} books"
            );
        });
    }

    /// Reaching the limit exactly is still a complete plan; only a further window
    /// may trigger backoff. A cancellation at that boundary is never cached as overflow.
    #[test]
    fn window_discovery_keeps_exact_budget_plans_and_prioritizes_cancellation() {
        use crate::semantic_resolver::LiveResolver;
        use otzaria_semantic_search::semantic::resolve::ResolveError;
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        let long = (0..600)
            .map(|n| format!("מילה{n:04}"))
            .collect::<Vec<_>>()
            .join(" ");
        let windows = crate::semantic_keys::window_keys(&long).len();
        assert!(windows > 1);
        engine
            .add_text_book_placed(
                "ספר".into(),
                "/other".into(),
                OTHER.into(),
                1,
                0,
                long,
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        let vectors = dir.path().join("vectors");
        let generation = otzaria_semantic_search::semantic::segment_set::info(&vectors)
            .unwrap()
            .unwrap()
            .generation;
        let view = engine.semantic_set_view(&vectors, generation).unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        pool.install(|| {
            let filters = SidecarSearchFilters {
                book_paths: None,
                facets: Some(vec!["/".into()]),
                include_pdf: None,
            };
            let mut resolver = LiveResolver::new(
                engine.index_reader.searcher(),
                engine.chunk_key_field,
                &engine.semantic_resolver,
            )
            .unwrap();
            let cancel = CancellationToken::new();
            let previous =
                crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.replace(windows - 1));
            crate::semantic_resolver::CANCEL_ON_WINDOW_OVERFLOW.with(|flag| flag.set(true));
            let cancelled = resolver.plan(&filters, &view, &cancel);
            crate::semantic_resolver::CANCEL_ON_WINDOW_OVERFLOW.with(|flag| flag.set(false));
            crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.set(previous));
            assert_eq!(cancelled.err(), Some(ResolveError::Cancelled));

            let previous =
                crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.replace(windows));
            let complete = resolver.plan(&filters, &view, &CancellationToken::new());
            crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.set(previous));
            assert!(
                complete.unwrap().is_some(),
                "a cancelled overflow must not cache backoff; exactly the limit fits"
            );
        });
    }

    /// The probe's semantic hit at line 1 of [`BOOK`], in a search's results.
    fn holds_probe_hit(response: &SemanticSearchResponse) -> bool {
        response.results.iter().any(|result| {
            result.file_path == BOOK && result.segment == 1 && result.semantic_score.is_some()
        })
    }

    /// The app's whole-library search is filtered by "/": past the window budget it goes
    /// unplanned, once per generation, and its semantic half is still served.
    #[test]
    fn a_whole_library_filter_past_the_window_budget_still_searches_semantically() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        let long = (0..600)
            .map(|n| format!("מילה{n:04}"))
            .collect::<Vec<_>>()
            .join(" ");
        engine
            .add_text_book_placed(
                "ספר ארוך".into(),
                "/other".into(),
                OTHER.into(),
                1,
                0,
                long,
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        let previous = crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.replace(1));
        let response = engine.search_semantic(
            PROBE.to_string(),
            vec!["/".to_string()],
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            None,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        );
        let walked = walks(&engine);
        assert!(walked > 0, "the plan walked the changed book");
        let again = engine.search_semantic(
            "ויאמר אלהים".to_string(),
            vec!["/".to_string()],
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            None,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        );
        crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.set(previous));
        let response = response.unwrap();
        assert!(
            response.semantic_available,
            "{:?}",
            response.fallback_reason
        );
        assert_eq!(response.fallback_kind, None);
        assert!(holds_probe_hit(&response));
        assert!(again.unwrap().semantic_available);
        assert_eq!(
            walks(&engine),
            walked,
            "not walked again in this generation"
        );
    }

    /// The same query asked again after its semantic half failed searches again, rather
    /// than repeating the failure until the session expires.
    #[test]
    fn a_failed_semantic_half_is_not_kept_for_the_next_search() {
        let dir = TempDir::new().unwrap();
        let engine = opened(&dir);
        crate::semantic_resolver::FAIL_SCANS.with(|fail| fail.set(true));
        let failed = unfiltered(&engine, PROBE, SemanticRetrievalMode::Hybrid);
        crate::semantic_resolver::FAIL_SCANS.with(|fail| fail.set(false));
        let failed = failed.unwrap();
        assert!(!failed.semantic_available);
        assert_eq!(failed.fallback_kind, Some(SemanticErrorKind::QueryFailed));
        assert!(
            failed
                .fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("a test made the scan fail")),
            "{:?}",
            failed.fallback_reason
        );

        let again = unfiltered(&engine, PROBE, SemanticRetrievalMode::Hybrid).unwrap();
        assert!(again.semantic_available, "{:?}", again.fallback_reason);
        assert_eq!(again.fallback_kind, None);
        assert!(holds_probe_hit(&again));
    }

    fn lookups(engine: &SearchEngine) -> u64 {
        engine
            .semantic_resolver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lookups
    }

    /// Past the cap, the arrivals found are kept: the next generation gives up without
    /// asking the set again.
    #[test]
    fn an_unfiltered_plan_past_its_cap_asks_the_set_once() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        add_copy(&mut engine, OTHER, 1);
        add_copy(&mut engine, "/books/third.txt", 2);
        add_copy(&mut engine, "/books/fourth.txt", 3);
        let cap = crate::semantic_resolver::LIBRARY_CAP.with(|cap| cap.replace(1));
        let first = probe_lines(&engine);
        let first_lookups = lookups(&engine);
        engine
            .delete_documents_by_file_path("/books/fourth.txt")
            .unwrap();
        engine.commit().unwrap();
        let second = probe_lines(&engine);
        let second_lookups = lookups(&engine);
        crate::semantic_resolver::LIBRARY_CAP.with(|limit| limit.set(cap));
        assert_eq!(first, [(BOOK.to_string(), 1)]);
        assert_eq!(second, [(BOOK.to_string(), 1)]);
        assert_eq!((first_lookups, second_lookups), (1, 0));
    }

    /// A plan its token cancelled is not one that failed: the next search plans.
    #[test]
    fn a_cancelled_plan_is_not_a_failed_one() {
        use crate::semantic_resolver::LiveResolver;
        use otzaria_semantic_search::semantic::resolve::ResolveError;
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        add_copy(&mut engine, OTHER, 1);
        let vectors = dir.path().join("vectors");
        let generation = otzaria_semantic_search::semantic::segment_set::info(&vectors)
            .unwrap()
            .unwrap()
            .generation;
        let view = engine.semantic_set_view(&vectors, generation).unwrap();
        let mut resolver = LiveResolver::new(
            engine.index_reader.searcher(),
            engine.chunk_key_field,
            &engine.semantic_resolver,
        )
        .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let filters = SidecarSearchFilters {
            book_paths: None,
            facets: None,
            include_pdf: None,
        };
        assert_eq!(
            resolver.plan(&filters, &view, &cancel).err(),
            Some(ResolveError::Cancelled)
        );
        drop(resolver);
        assert_eq!(
            probe_lines(&engine),
            [(BOOK.to_string(), 1), (OTHER.to_string(), 1)]
        );
    }

    /// Each filter a search plans keeps its own plan: the foundational books' query reads
    /// the books its filter admits, and the main query still reads its own.
    #[test]
    fn each_planned_filter_keeps_its_own_admitted_books() {
        use crate::semantic_resolver::LiveResolver;
        use otzaria_semantic_search::semantic::resolve::CandidateResolver;
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        add_copy(&mut engine, OTHER, 1);
        let vectors = dir.path().join("vectors");
        let generation = otzaria_semantic_search::semantic::segment_set::info(&vectors)
            .unwrap()
            .unwrap()
            .generation;
        let view = engine.semantic_set_view(&vectors, generation).unwrap();
        let mut resolver = LiveResolver::new(
            engine.index_reader.searcher(),
            engine.chunk_key_field,
            &engine.semantic_resolver,
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let filter = |facet: &str| SidecarSearchFilters {
            book_paths: None,
            facets: Some(vec![facet.to_string()]),
            include_pdf: None,
        };
        let (root, other) = (filter("/root"), filter("/other"));
        assert!(resolver.plan(&root, &view, &cancel).unwrap().is_some());
        assert!(resolver.plan(&other, &view, &cancel).unwrap().is_some());
        let admitted = |filters: &SidecarSearchFilters| {
            resolver
                .admissible_books(Some(filters))
                .unwrap()
                .map(|books| (books.len(), books.contains(BOOK), books.contains(OTHER)))
        };
        assert_eq!(admitted(&root), Some((1, true, false)));
        assert_eq!(admitted(&other), Some((1, false, true)));
        assert!(resolver.admissible_books(None).unwrap().is_none());
    }

    /// A plan cancelled before it asks the set keeps the books it read.
    #[test]
    fn a_cancelled_plan_keeps_the_books_it_read() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        add_copy(&mut engine, OTHER, 1);
        crate::semantic_resolver::CANCEL_LOOKUPS.with(|cancel| cancel.set(true));
        let cancelled = unfiltered(&engine, PROBE, SemanticRetrievalMode::SemanticOnly);
        crate::semantic_resolver::CANCEL_LOOKUPS.with(|cancel| cancel.set(false));
        assert_eq!(
            cancelled.err().map(|error| error.kind),
            Some(SemanticErrorKind::Cancelled)
        );
        assert_eq!(walks(&engine), 2);
        assert_eq!(
            probe_lines(&engine),
            [(BOOK.to_string(), 1), (OTHER.to_string(), 1)]
        );
        assert_eq!(walks(&engine), 2, "no book read again");
    }

    /// A vector whose line's column is stale resolves nowhere: its record's line and every
    /// line its column value is found at hold another key. That is looked for across the
    /// whole index once in a generation of it, not by every search that hits the vector.
    #[test]
    fn a_key_only_a_stale_column_holds_is_looked_for_once_a_generation() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        forge(&mut engine, "שורה אחרת לגמרי שאין לה דבר עם הווקטור");
        let passes = |engine: &SearchEngine| {
            engine
                .semantic_resolver
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .passes
        };
        // Two queries, so that the second is not answered from the query cache: the set
        // holds two vectors, and every search scans both.
        for (query, expected) in [(PROBE, 1), ("בראשית ברא אלהים", 1)] {
            let response = engine
                .search_semantic(
                    query.to_string(),
                    Vec::new(),
                    10,
                    0,
                    SemanticLexicalMode::Exact,
                    0,
                    SemanticRetrievalMode::SemanticOnly,
                    None,
                    false,
                    false,
                    None,
                    &SemanticCancellationToken::new(),
                )
                .unwrap();
            assert!(
                response
                    .results
                    .iter()
                    .all(|result| result.file_path != BOOK || result.segment != 1),
                "{query}: the stale line is not the vector's"
            );
            assert_eq!(passes(&engine), expected, "{query}");
        }
    }

    /// A filtered search that cannot be planned scans the books its filter admits, unplanned,
    /// and its semantic half is served; the next search is planned.
    #[test]
    fn a_filtered_search_that_cannot_be_planned_goes_unplanned() {
        let dir = TempDir::new().unwrap();
        let engine = opened(&dir);
        let filtered = |query: &str| {
            engine.search_semantic(
                query.to_string(),
                vec!["/root".to_string()],
                10,
                0,
                SemanticLexicalMode::Exact,
                0,
                SemanticRetrievalMode::Hybrid,
                None,
                false,
                false,
                None,
                &SemanticCancellationToken::new(),
            )
        };
        crate::semantic_resolver::FAIL_PLANS.with(|fail| fail.set(true));
        let unplanned = filtered(PROBE);
        crate::semantic_resolver::FAIL_PLANS.with(|fail| fail.set(false));
        let unplanned = unplanned.unwrap();
        assert!(
            unplanned.semantic_available,
            "{:?}",
            unplanned.fallback_reason
        );
        assert_eq!(unplanned.fallback_kind, None);
        assert!(holds_probe_hit(&unplanned));

        let planned = filtered("ויאמר אלהים יהי אור").unwrap();
        assert!(planned.semantic_available, "{:?}", planned.fallback_reason);
        assert_eq!(planned.fallback_kind, None);
    }

    /// Even without a plan, the live facets exclude a book whose category changed
    /// after the vectors were built.
    #[test]
    fn an_unplanned_search_still_applies_live_book_facets() {
        let dir = TempDir::new().unwrap();
        let mut engine = open(
            &dir,
            built_of_books(
                &dir,
                false,
                &[(BOOK, PROBE.to_string()), (OTHER, PROBE.to_string())],
            ),
        );
        engine.delete_documents_by_file_path(OTHER).unwrap();
        engine
            .add_text_book_placed(
                "other".into(),
                "/outside".into(),
                OTHER.into(),
                1,
                0,
                PROBE.into(),
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        crate::semantic_resolver::FAIL_PLANS.with(|fail| fail.set(true));
        let response = engine.search_semantic(
            PROBE.into(),
            vec!["/root".into()],
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            None,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        );
        crate::semantic_resolver::FAIL_PLANS.with(|fail| fail.set(false));
        let response = response.unwrap();
        assert!(response.semantic_available);
        assert!(!response.results.is_empty());
        assert!(response.results.iter().all(|r| r.file_path == BOOK));
    }

    /// Cached lexical pages stay stable after a semantic failure, while an explicit
    /// first-page search retries the recovered semantic path.
    #[test]
    fn a_failed_session_keeps_its_pages_but_a_new_search_retries() {
        let dir = TempDir::new().unwrap();
        let text = (0..12)
            .map(|n| format!("{PROBE} תוספת{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let engine = open(&dir, built_of(&dir, false, text));
        let page = |offset| {
            engine
                .search_semantic(
                    PROBE.into(),
                    vec!["/".into()],
                    1,
                    offset,
                    SemanticLexicalMode::Exact,
                    0,
                    SemanticRetrievalMode::Hybrid,
                    None,
                    false,
                    false,
                    None,
                    &SemanticCancellationToken::new(),
                )
                .unwrap()
        };
        crate::semantic_resolver::FAIL_SCANS.with(|fail| fail.set(true));
        let first = page(0);
        crate::semantic_resolver::FAIL_SCANS.with(|fail| fail.set(false));
        assert!(!first.semantic_available);
        assert!(first.has_more);
        let continuation = page(1);
        assert!(!continuation.session_restarted);
        assert!(!continuation.semantic_available);
        assert_ne!(first.results[0].id, continuation.results[0].id);
        let fresh = page(0);
        assert!(fresh.semantic_available);
        assert!(fresh.fallback_kind.is_none());
    }

    /// A commit clears an overflow backoff: a now-small plan must discover the probe
    /// copied to another book rather than continue using the earlier unplanned scan.
    #[test]
    fn a_filtered_window_overflow_backoff_is_cleared_on_commit() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        let long = (0..600)
            .map(|n| format!("מילה{n:04}"))
            .collect::<Vec<_>>()
            .join(" ");
        engine
            .add_text_book_placed(
                "other".into(),
                "/other".into(),
                OTHER.into(),
                1,
                0,
                long,
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        let search = |engine: &SearchEngine| {
            engine
                .search_semantic(
                    PROBE.into(),
                    vec!["/".into()],
                    10,
                    0,
                    SemanticLexicalMode::Exact,
                    0,
                    SemanticRetrievalMode::SemanticOnly,
                    None,
                    false,
                    false,
                    None,
                    &SemanticCancellationToken::new(),
                )
                .unwrap()
        };
        let old = crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.replace(1));
        let first = search(&engine);
        crate::semantic_resolver::WINDOW_ARRIVAL_CAP.with(|cap| cap.set(old));
        assert!(first.semantic_available);
        engine.delete_documents_by_file_path(OTHER).unwrap();
        add_copy(&mut engine, OTHER, 1);
        let next = search(&engine);
        assert!(next.semantic_available);
        assert!(next
            .results
            .iter()
            .any(|r| r.file_path == OTHER && r.semantic_score.is_some()));
    }

    /// Another process's cleanup can take an expansion's lock file in the instant between
    /// its creation and its locking: lock it, take it for a process gone, and remove it.
    /// The install then takes a lock file of its own again and goes on — whether the
    /// cleaner still holds the one it took, or has removed it and let go — and leaves
    /// nothing in `incoming/`.
    #[test]
    fn an_expansion_lock_another_cleanup_takes_first_is_taken_again() {
        use std::sync::{Arc, Mutex};
        let dir = TempDir::new().unwrap();
        let built = built(&dir);
        let compressed = dir.path().join("segment.oxv.zst");
        fs::write(
            &compressed,
            zstd::stream::encode_all(
                fs::File::open(built.package.join(SEGMENT_FILENAME)).unwrap(),
                1,
            )
            .unwrap(),
        )
        .unwrap();
        let vectors = dir.path().join("vectors");
        // The cleaner: the first lock file it holds and removes, the second it removes
        // and lets go of, the rest it leaves alone.
        let calls = Arc::new(Mutex::new(0u32));
        let held: Arc<Mutex<Option<fs::File>>> = Arc::new(Mutex::new(None));
        {
            let (calls, held) = (Arc::clone(&calls), Arc::clone(&held));
            super::AFTER_EXPANSION_LOCK_FILE.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |path: &Path| {
                    let mut calls = calls.lock().unwrap();
                    *calls += 1;
                    if *calls <= 2 {
                        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
                        file.try_lock().unwrap();
                        fs::remove_file(path).unwrap();
                        *held.lock().unwrap() = (*calls == 1).then_some(file);
                    }
                }));
            });
        }
        let installed = built.engine.install_semantic_vectors(
            SemanticVectorsInstallInput {
                vectors_dir: vectors.to_string_lossy().into_owned(),
                segment_path: compressed.to_string_lossy().into_owned(),
                manifest_json: fs::read_to_string(built.package.join(RELEASE_MANIFEST_FILENAME))
                    .unwrap(),
                published_manifest_sha256: Some(built.manifest_sha256.clone()),
                model_identity_json: serde_json::to_string(&built.model).unwrap(),
            },
            &SemanticCancellationToken::new(),
        );
        super::AFTER_EXPANSION_LOCK_FILE.with(|hook| *hook.borrow_mut() = None);
        drop(held.lock().unwrap().take());
        if let Err(error) = &installed {
            panic!("the install is not refused: {error:?}");
        }
        assert_eq!(*calls.lock().unwrap(), 3, "a lock file a third time");
        let left: Vec<String> = fs::read_dir(vectors.join("incoming"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(left, Vec::<String>::new());
    }

    /// One lexical search also found keeps its lexical half alone.
    #[test]
    fn a_line_lexical_search_also_found_keeps_its_lexical_half() {
        let dir = TempDir::new().unwrap();
        let mut engine = opened(&dir);
        forge(&mut engine, &format!("{PROBE} ועוד מילים שלא היו בו"));

        let response = search(&engine, SemanticRetrievalMode::Hybrid);
        let forged = response
            .results
            .iter()
            .find(|result| result.file_path == BOOK && result.segment == 1)
            .expect("lexical search finds the forged line");
        assert_eq!(forged.source, SemanticResultSource::Lexical);
        assert_eq!(forged.semantic_score, None);
        assert!(forged.lexical_score.is_some());
    }
}
