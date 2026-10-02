//! `TextStorage::LibraryDb` against `TextStorage::InIndex`: two indexes built from the same
//! synthetic library database must answer every query identically, and the line source
//! must degrade (stale / unavailable) instead of failing when the database moves.

use crate::api::search_engine::*;
use crate::line_source;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// One synthetic library book. `rows[i]` is stored at `line_indexes[i]`.
pub(crate) struct Book {
    id: i64,
    title: &'static str,
    topics: &'static str,
    catalogue_order: u32,
    generation_order: u32,
    line_indexes: Vec<i64>,
    rows: Vec<Vec<u8>>,
    compressed: bool,
}

const BOM: &str = "\u{FEFF}";

fn long_payload() -> String {
    "iVBORw0KGgoAAAANSUhEUgAA".repeat(4)
}

pub(crate) fn books() -> Vec<Book> {
    let long = long_payload();
    let text_rows = |rows: Vec<String>| rows.into_iter().map(String::into_bytes).collect();
    let mut sixth: Vec<String> = (0..120)
        .map(|n| format!("שורה {n} של הספר השישי ובה המילה בראשית ועוד אלהים"))
        .collect();
    sixth.insert(0, "<h1>ספר שישי</h1>".to_string());
    sixth.insert(40, "<h2>חלק ב</h2>".to_string());
    vec![
        Book {
            id: 1,
            title: "ספר ראשון",
            topics: "/תנך/תורה",
            catalogue_order: 0,
            generation_order: 1,
            line_indexes: (0..14).collect(),
            rows: text_rows(vec![
                "<h1>ספר ראשון</h1>".to_string(),
                "<h2>פרק א</h2>".to_string(),
                "בראשית ברא אלהים את השמים ואת הארץ".to_string(),
                "וְהָאָרֶץ הָיְתָה תֹהוּ וָבֹהוּ וְחֹשֶׁךְ עַל פְּנֵי תְהוֹם".to_string(),
                "וַיֹּ֥אמֶר אֱלֹהִ֖ים יְהִ֣י א֑וֹר וַֽיְהִי אֽוֹר׃".to_string(),
                String::new(),
                "שורה עם <b>הדגשה</b> &amp; סימנים \"מרכאות\" ורמב\"ם < > & אלהים".to_string(),
                format!("תמונה <img src=\"data:image/png;base64,{long}\"> ואחריה מילים בראשית"),
                "קצר data:abc נשאר בראשית".to_string(),
                "שורה עם סיום חלונות בראשית\r".to_string(),
                "<h2>פרק ב</h2>".to_string(),
                "ויאמר אלהים יהי אור ויהי אור וירא אלהים את האור כי טוב".to_string(),
                "אור".to_string(),
                "בראשית ברא אלהים את השמים ואת הארץ".to_string(),
            ]),
            compressed: false,
        },
        // Row 0 starts with a BOM and the book contains `data:`: the app decodes it as one
        // string, which drops that BOM (and only that one).
        Book {
            id: 2,
            title: "ספר שני",
            topics: "/תנך/נביאים",
            catalogue_order: 1,
            generation_order: 3,
            line_indexes: (0..4).collect(),
            rows: text_rows(vec![
                format!("{BOM}<h1>ספר שני</h1> בראשית"),
                format!("מילים עם data:{long} בראשית ברא"),
                format!("{BOM}שורה שמתחילה בסימן בראשית אלהים"),
                "ויאמר משה אל העם".to_string(),
            ]),
            compressed: false,
        },
        // lineIndex has gaps: ordinals map through the sorted lineIndex array.
        Book {
            id: 3,
            title: "ספר שלישי",
            topics: "/משנה/זרעים",
            catalogue_order: 2,
            generation_order: 2,
            line_indexes: vec![0, 1, 2, 5, 6, 9, 20, 21],
            rows: text_rows(vec![
                "<h1>ספר שלישי</h1>".to_string(),
                "מאימתי קורין את שמע בערבית".to_string(),
                "משעה שהכהנים נכנסים לאכול בתרומתן".to_string(),
                "בראשית ברא אלהים בספר עם פערים".to_string(),
                "אור".to_string(),
                "<h2>פרק ב</h2>".to_string(),
                "ויאמר אלהים יהי אור".to_string(),
                "בראשית ברא אלהים את השמים ואת הארץ".to_string(),
            ]),
            compressed: false,
        },
        // A BOM in row 0 of a clean book: the app passes bytes, and the BOM stays.
        Book {
            id: 4,
            title: "ספר רביעי",
            topics: "/משנה/מועד",
            catalogue_order: 3,
            generation_order: 5,
            line_indexes: (0..3).collect(),
            rows: text_rows(vec![
                format!("{BOM}בראשית ברא בלי תמונות"),
                "ויאמר אלהים".to_string(),
                "אור".to_string(),
            ]),
            compressed: false,
        },
        // Per-row zstd frames of a trained dictionary (schema 6 with `zstd_dict`).
        Book {
            id: 5,
            title: "ספר חמישי",
            topics: "/תנך/כתובים",
            catalogue_order: 4,
            generation_order: 4,
            line_indexes: (0..6).collect(),
            rows: text_rows(vec![
                "<h1>ספר חמישי</h1>".to_string(),
                "בראשית ברא אלהים את השמים ואת הארץ".to_string(),
                "וְהָאָרֶץ הָיְתָה תֹהוּ וָבֹהוּ".to_string(),
                "שורה דחוסה עם <i>תגית</i> ורמב\"ם אלהים".to_string(),
                String::new(),
                "אור".to_string(),
            ]),
            compressed: true,
        },
        Book {
            id: 6,
            title: "ספר שישי",
            topics: "/תנך/תורה",
            catalogue_order: 5,
            generation_order: 0,
            line_indexes: (0..sixth.len() as i64).collect(),
            rows: text_rows(sixth),
            compressed: false,
        },
    ]
}

fn train_dictionary() -> Vec<u8> {
    let samples: Vec<Vec<u8>> = (0..2000)
        .map(|n| {
            format!(
                "שורה {n} בראשית ברא אלהים את השמים ואת הארץ {} ויאמר",
                ["אור", "חושך", "מים", "רקיע", "ארץ"][n % 5]
            )
            .into_bytes()
        })
        .collect();
    zstd::dict::from_samples(&samples, 4096).expect("training a zstd dictionary")
}

/// Writes `books` as a library database shaped like seforim.db (schema 6).
pub(crate) fn write_library(path: &Path, books: &[Book], with_dictionary: bool) -> Option<Vec<u8>> {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE book (id INTEGER PRIMARY KEY NOT NULL, title TEXT NOT NULL);
         CREATE TABLE line (id INTEGER PRIMARY KEY NOT NULL, bookId INTEGER NOT NULL,
                            lineIndex INTEGER NOT NULL, heRef TEXT);
         CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);
         CREATE TABLE line_content (id INTEGER PRIMARY KEY NOT NULL, content TEXT NOT NULL);",
    )
    .unwrap();
    let dict = with_dictionary.then(train_dictionary);
    if let Some(dict) = &dict {
        conn.execute_batch("CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB NOT NULL)")
            .unwrap();
        conn.execute("INSERT INTO zstd_dict (dict) VALUES (?1)", [dict])
            .unwrap();
    }
    let mut compressor = dict
        .as_ref()
        .map(|d| zstd::bulk::Compressor::with_dictionary(3, d).unwrap());
    let mut line_id = 100;
    for book in books {
        conn.execute(
            "INSERT INTO book (id, title) VALUES (?1, ?2)",
            rusqlite::params![book.id, book.title],
        )
        .unwrap();
        // Insert in reverse so rowid order differs from lineIndex order.
        for (index, row) in book.line_indexes.iter().zip(&book.rows).rev() {
            line_id += 1;
            conn.execute(
                "INSERT INTO line (id, bookId, lineIndex) VALUES (?1, ?2, ?3)",
                rusqlite::params![line_id, book.id, index],
            )
            .unwrap();
            match (&mut compressor, book.compressed) {
                (Some(c), true) => {
                    let frame = c.compress(row).unwrap();
                    conn.execute(
                        "INSERT INTO line_content (id, content) VALUES (?1, ?2)",
                        rusqlite::params![line_id, frame],
                    )
                    .unwrap();
                }
                _ => {
                    conn.execute(
                        "INSERT INTO line_content (id, content) VALUES (?1, ?2)",
                        rusqlite::params![line_id, String::from_utf8(row.clone()).unwrap()],
                    )
                    .unwrap();
                }
            }
        }
    }
    dict
}

/// The indexing input the app builds from a book's rows (`loadTextBookSource`): the rows
/// joined by `\n` as bytes, or — when `data:` occurs anywhere — decoded as one string
/// (dropping its leading BOM) and cleaned of data URIs.
enum BookInput {
    Bytes(Vec<u8>),
    Text(String),
}

fn app_indexing_input(book: &Book) -> BookInput {
    let joined = book.rows.join(&b'\n');
    if joined.windows(5).any(|w| w == b"data:") {
        let text = String::from_utf8_lossy(&joined);
        let text = text.strip_prefix(BOM).unwrap_or(&text);
        BookInput::Text(line_source::strip_data_uris_for_index(text))
    } else {
        BookInput::Bytes(joined)
    }
}

fn index_books(dir: &Path, books: &[Book], storage: TextStorage) -> SearchEngine {
    let mut engine = index_library_books(dir, books, storage);
    index_books_extras(&mut engine);
    engine.commit().unwrap();
    engine
}

fn index_library_books(dir: &Path, books: &[Book], storage: TextStorage) -> SearchEngine {
    let mut engine = SearchEngine::new(dir.to_str().unwrap());
    for book in books {
        let file_path = format!("id:{}", book.id);
        let facets = Some(vec![format!("/era/{}", book.generation_order)]);
        let added = match app_indexing_input(book) {
            BookInput::Bytes(bytes) => engine.add_text_book_bytes(
                book.title.to_string(),
                book.topics.to_string(),
                file_path,
                book.catalogue_order,
                book.generation_order,
                bytes,
                facets,
                storage,
            ),
            BookInput::Text(text) => engine.add_text_book(
                book.title.to_string(),
                book.topics.to_string(),
                file_path,
                book.catalogue_order,
                book.generation_order,
                text,
                facets,
                storage,
            ),
        }
        .unwrap();
        assert_eq!(added as usize, book.rows.len(), "book {}", book.id);
    }
    engine
}

/// Only the library books, committed (the semantic corpus refuses ids that encode no
/// catalogue position, like the marker document [`index_books`] adds).
#[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
pub(crate) fn index_library_books_committed(
    dir: &Path,
    books: &[Book],
    storage: TextStorage,
) -> SearchEngine {
    let mut engine = index_library_books(dir, books, storage);
    engine.commit().unwrap();
    engine
}

fn index_books_extras(engine: &mut SearchEngine) {
    // Text that is never in the library database stays in the index in both modes.
    engine
        .add_pdf_book(
            "ספר סרוק".to_string(),
            "/תנך/תורה".to_string(),
            "/pdf/scan.pdf".to_string(),
            10,
            1,
            vec![PdfPageInput {
                reference: "ספר סרוק, עמוד 1".to_string(),
                text: "בראשית ברא אלהים את השמים\nואת הארץ אור".to_string(),
                page_index: 0,
            }],
            None,
        )
        .unwrap();
    engine
        .add_text_book(
            "ספר אישי".to_string(),
            "/אישי".to_string(),
            "uid:7".to_string(),
            11,
            5,
            "בראשית ברא אלהים\nאור".to_string(),
            None,
            TextStorage::InIndex,
        )
        .unwrap();
    engine
        .add_documents_batch(vec![DocumentInput {
            id: 99,
            title: "סמן".to_string(),
            reference: "סמן".to_string(),
            topics: "/אישי".to_string(),
            text: String::new(),
            segment: 0,
            is_pdf: false,
            file_path: "/empty/book.txt".to_string(),
            content_hash: None,
            text_hash: None,
            text_vocalized: None,
            section_id: None,
            generation_order: None,
            extra_facets: None,
            text_storage: None,
        }])
        .unwrap();
}

/// Everything a result carries, as one comparable string.
fn snap(r: &SearchResult) -> String {
    let merged: Vec<String> = r
        .merged
        .iter()
        .map(|m| format!("{}:{}:{}:{}", m.id, m.segment, m.file_path, m.reference))
        .collect();
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{:?}",
        r.title,
        r.reference,
        r.text,
        r.id,
        r.segment,
        r.is_pdf,
        r.file_path,
        r.merged_count,
        merged
    )
}

fn snaps(results: &[SearchResult]) -> Vec<String> {
    results.iter().map(snap).collect()
}

/// Equal results; relevance probes compare unordered (see `probes`).
fn assert_same(name: &str, stored: &[SearchResult], external: &[SearchResult]) {
    let (mut a, mut b) = (snaps(stored), snaps(external));
    if name.contains("relevance") || name.starts_with("fuzzy") {
        a.sort();
        b.sort();
    }
    if a != b {
        let at = a
            .iter()
            .zip(&b)
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(b.len()));
        panic!(
            "{name}: {} vs {} results, first difference at {at}:
  stored:   {:?}
  external: {:?}",
            a.len(),
            b.len(),
            a.get(at),
            b.get(at)
        );
    }
}

fn assert_all_ok(results: &[SearchResult], what: &str) {
    for r in results {
        assert_eq!(r.text_status, TextStatus::Ok, "{what}: {}", snap(r));
    }
}

struct Fixture {
    _dir: TempDir,
    db: PathBuf,
    stored: SearchEngine,
    external: SearchEngine,
    stored_path: PathBuf,
    external_path: PathBuf,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("seforim.db");
    let books = books();
    write_library(&db, &books, true);
    let stored_path = dir.path().join("stored");
    let external_path = dir.path().join("external");
    std::fs::create_dir_all(&stored_path).unwrap();
    std::fs::create_dir_all(&external_path).unwrap();
    // Index mode reads the same rows the app would, decoded.
    let stored = index_books(&stored_path, &books, TextStorage::InIndex);
    let external = index_books(&external_path, &books, TextStorage::LibraryDb);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    Fixture {
        _dir: dir,
        db,
        stored,
        external,
        stored_path,
        external_path,
    }
}

fn guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = line_source::TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    line_source::reset_for_tests();
    guard
}

type Probe = Box<dyn Fn(&SearchEngine) -> Vec<SearchResult>>;

type OrderOf = fn() -> ResultsOrder;
type Run<'a> = dyn Fn(&SearchEngine) -> Vec<SearchResult> + 'a;

fn orders() -> Vec<(&'static str, OrderOf)> {
    vec![
        ("catalogue", || ResultsOrder::Catalogue),
        ("relevance", || ResultsOrder::Relevance),
        ("generation", || ResultsOrder::Generation),
    ]
}

fn probes() -> Vec<(String, Probe)> {
    let mut probes: Vec<(String, Probe)> = Vec::new();
    let words = [
        "בראשית",
        "אלהים",
        "אור",
        "בראשית ברא",
        "ברא אלהים את",
        "מרכאות",
        "רמבם",
        "רמב\"ם",
        "הארץ",
        "תמונה ואחריה",
        "סימנים",
        "הדגשה",
        "שורה",
        "משה",
        "פערים",
        "תגית",
        "זזזזז",
        "data",
        "abc",
    ];
    let groupings: [(&str, Option<ResultGrouping>); 3] = [
        ("flat", None),
        ("section", Some(ResultGrouping::SameSection)),
        ("identical", Some(ResultGrouping::IdenticalText)),
    ];
    for word in words {
        for (order_name, order) in orders() {
            for (group_name, grouping) in groupings {
                // Relevance ties are broken by segment layout, which differs between
                // two separately built indexes; those probes take the whole result
                // set and compare it unordered.
                let pages: &[(u32, u32)] = if order_name == "relevance" {
                    &[(0, 200)]
                } else {
                    &[(0, 7), (7, 7), (0, 200)]
                };
                for &(offset, limit) in pages {
                    let w = word.to_string();
                    probes.push((
                        format!("exact {word} {order_name} {group_name} {offset}+{limit}"),
                        Box::new(move |e| {
                            e.search_exact(
                                w.clone(),
                                vec![],
                                limit,
                                offset,
                                order(),
                                false,
                                false,
                                grouping,
                            )
                            .unwrap()
                        }),
                    ));
                }
            }
        }
        for distance in [1u8, 2] {
            let w = word.to_string();
            probes.push((
                format!("fuzzy {word} {distance}"),
                Box::new(move |e| {
                    e.search_fuzzy(
                        w.clone(),
                        vec![],
                        500,
                        0,
                        distance,
                        ResultsOrder::Relevance,
                        false,
                        false,
                        None,
                    )
                    .unwrap()
                }),
            ));
        }
    }
    // Vocalized: the displayed text is the vocalized rendering.
    for (word, nikud, taamim) in [
        ("וְהָאָרֶץ", true, false),
        ("הָאָרֶץ", true, true),
        ("וַיֹּ֥אמֶר", false, true),
        ("וַיֹּ֥אמֶר", true, true),
        ("א֑וֹר", true, true),
        ("תֹהוּ וָבֹהוּ", true, false),
    ] {
        let w = word.to_string();
        probes.push((
            format!("vocalized exact {word} {nikud} {taamim}"),
            Box::new(move |e| {
                e.search_exact(
                    w.clone(),
                    vec![],
                    50,
                    0,
                    ResultsOrder::Catalogue,
                    nikud,
                    taamim,
                    None,
                )
                .unwrap()
            }),
        ));
        let w = word.to_string();
        probes.push((
            format!("vocalized fuzzy {word} {nikud} {taamim}"),
            Box::new(move |e| {
                e.search_fuzzy(
                    w.clone(),
                    vec![],
                    50,
                    0,
                    1,
                    ResultsOrder::Catalogue,
                    nikud,
                    taamim,
                    None,
                )
                .unwrap()
            }),
        ));
    }
    // Facets.
    for facet in ["/תנך", "/תנך/תורה", "/משנה", "/era/3", "/אישי"] {
        let f = facet.to_string();
        probes.push((
            format!("facet {facet}"),
            Box::new(move |e| {
                e.search_exact(
                    "בראשית".to_string(),
                    vec![f.clone()],
                    50,
                    0,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    None,
                )
                .unwrap()
            }),
        ));
    }
    // Regex terms with slop.
    for (terms, slop) in [
        (vec!["בראש.*"], 0u32),
        (vec!["ברא", "את"], 2),
        (vec![".*ור"], 0),
    ] {
        let t: Vec<String> = terms.iter().map(|s| s.to_string()).collect();
        probes.push((
            format!("regex {terms:?} {slop}"),
            Box::new(move |e| {
                e.search(
                    t.clone(),
                    vec![],
                    50,
                    0,
                    slop,
                    100,
                    ResultsOrder::Catalogue,
                    None,
                )
                .unwrap()
            }),
        ));
    }
    // Advanced: distance, scopes, options, word-match modes, negatives.
    let partial: HashMap<String, HashMap<String, bool>> = [(
        "ראשי_0".to_string(),
        [("חלק ממילה".to_string(), true)].into_iter().collect(),
    )]
    .into_iter()
    .collect();
    let prefix: HashMap<String, HashMap<String, bool>> = [(
        "ארץ_0".to_string(),
        [("קידומות".to_string(), true)].into_iter().collect(),
    )]
    .into_iter()
    .collect();
    type Scope = fn() -> SearchScope;
    type Options = HashMap<String, HashMap<String, bool>>;
    let advanced: Vec<(&str, &str, u32, Scope, Options, bool)> = vec![
        (
            "בראשית אלהים",
            "",
            3,
            || SearchScope::WordDistance,
            HashMap::new(),
            false,
        ),
        (
            "בראשית אלהים",
            "",
            0,
            || SearchScope::WordDistance,
            HashMap::new(),
            false,
        ),
        (
            "אלהים בראשית",
            "",
            5,
            || SearchScope::SameParagraph,
            HashMap::new(),
            false,
        ),
        (
            "בראשית משה",
            "",
            0,
            || SearchScope::SameSection,
            HashMap::new(),
            false,
        ),
        (
            "ראשי",
            "",
            0,
            || SearchScope::WordDistance,
            partial.clone(),
            false,
        ),
        (
            "ארץ",
            "",
            0,
            || SearchScope::WordDistance,
            prefix.clone(),
            false,
        ),
        (
            "בראשית זזזז",
            "",
            0,
            || SearchScope::WordDistance,
            HashMap::new(),
            true,
        ),
        (
            "בראשית",
            "אלהים",
            0,
            || SearchScope::WordDistance,
            HashMap::new(),
            false,
        ),
    ];
    for (query, negative, distance, scope, options, any_word) in advanced {
        for grouping in [None, Some(ResultGrouping::SameSection)] {
            let (q, n, o) = (query.to_string(), negative.to_string(), options.clone());
            probes.push((
                format!("advanced {query} -{negative} {distance} {grouping:?}"),
                Box::new(move |e| {
                    e.search_advanced(
                        q.clone(),
                        n.clone(),
                        vec![],
                        50,
                        0,
                        distance,
                        0,
                        HashMap::new(),
                        HashMap::new(),
                        HashMap::new(),
                        HashMap::new(),
                        o.clone(),
                        HashMap::new(),
                        ResultsOrder::Catalogue,
                        false,
                        false,
                        scope(),
                        SearchScope::WordDistance,
                        grouping,
                        any_word.then_some(WordMatchMode::AnyWord),
                        None,
                    )
                    .unwrap()
                }),
            ));
        }
    }
    probes
}

impl std::fmt::Debug for ResultGrouping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ResultGrouping::SameSection => "SameSection",
            ResultGrouping::IdenticalText => "IdenticalText",
        })
    }
}

#[test]
pub(crate) fn library_text_answers_every_query_like_stored_text() {
    let _guard = guard();
    let f = fixture();
    let mut compared = 0usize;
    let mut non_empty = 0usize;
    let mut vocalized_library_hits = 0usize;
    for (name, probe) in probes() {
        let stored = probe(&f.stored);
        let external = probe(&f.external);
        assert_all_ok(&external, &name);
        assert_same(&name, &stored, &external);
        compared += stored.len();
        non_empty += usize::from(!stored.is_empty());
        if name.starts_with("vocalized") {
            vocalized_library_hits += external
                .iter()
                .filter(|r| r.file_path.starts_with("id:") && r.text.contains('\u{05B8}'))
                .count();
        }
    }
    assert!(
        vocalized_library_hits > 5,
        "vocalized probes must show library nikud"
    );
    assert!(compared > 2000, "only {compared} results compared");
    assert!(non_empty > 150, "only {non_empty} probes returned results");

    // Counted pages, including group counts and truncation.
    for grouping in [
        None,
        Some(ResultGrouping::SameSection),
        Some(ResultGrouping::IdenticalText),
    ] {
        for (word, offset) in [("בראשית", 0u32), ("אלהים", 3), ("אור", 0)] {
            let page = |e: &SearchEngine| {
                e.search_and_count_exact(
                    word.to_string(),
                    vec![],
                    5,
                    offset,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    grouping,
                )
                .unwrap()
            };
            let (a, b) = (page(&f.stored), page(&f.external));
            assert_eq!(
                (a.total_count, a.truncated, a.group_count),
                (b.total_count, b.truncated, b.group_count)
            );
            assert_eq!(snaps(&a.results), snaps(&b.results), "{word} {grouping:?}");
            let fuzzy = |e: &SearchEngine| {
                e.search_and_count_fuzzy(
                    word.to_string(),
                    vec![],
                    5,
                    offset,
                    1,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    grouping,
                )
                .unwrap()
            };
            let (a, b) = (fuzzy(&f.stored), fuzzy(&f.external));
            assert_eq!(a.total_count, b.total_count);
            assert_eq!(
                snaps(&a.results),
                snaps(&b.results),
                "fuzzy {word} {grouping:?}"
            );
        }
    }

    // The raw line of every document, by id.
    let all = f
        .stored
        .search_exact(
            "בראשית".to_string(),
            vec![],
            1000,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
        )
        .unwrap();
    let mut ids: Vec<u64> = all.iter().map(|r| r.id).collect();
    for book in books() {
        let base = (u64::from(book.catalogue_order) + 1) << 32;
        ids.extend((1..=book.rows.len() as u64).map(|ordinal| base + ordinal));
    }
    ids.push(99);
    for id in ids {
        let a = f
            .stored
            .get_document_by_id(id)
            .unwrap()
            .expect("stored doc");
        let b = f
            .external
            .get_document_by_id(id)
            .unwrap()
            .expect("external doc");
        assert_eq!(b.text_status, TextStatus::Ok, "{id}");
        assert_eq!(snap(&a), snap(&b), "document {id}");
    }

    // The semantic envelope's lexical path paints the same snippets.
    let semantic = |e: &SearchEngine| {
        e.search_semantic(
            "בראשית ברא".to_string(),
            vec![],
            20,
            0,
            SemanticLexicalMode::Exact,
            1,
            SemanticRetrievalMode::LexicalOnly,
            None,
            false,
            false,
        )
        .unwrap()
    };
    let (a, b) = (semantic(&f.stored), semantic(&f.external));
    let flatten = |r: &SemanticSearchResponse| -> Vec<String> {
        let mut flat: Vec<String> = r
            .results
            .iter()
            .map(|x| {
                format!(
                    "{}|{}|{}|{}|{:?}",
                    x.id, x.snippet_html, x.is_highlighted, x.reference, x.text_status
                )
            })
            .collect();
        flat.sort();
        flat
    };
    assert_eq!(flatten(&a), flatten(&b));
    assert!(!a.results.is_empty());
}

#[test]
fn library_documents_keep_no_text_in_the_doc_store() {
    let _guard = guard();
    let f = fixture();
    let size = |p: &Path| -> u64 {
        std::fs::read_dir(p)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".store"))
            .map(|e| e.metadata().unwrap().len())
            .sum()
    };
    assert!(size(&f.external_path) < size(&f.stored_path));
    let searcher = f.external.corpus_searcher();
    let schema = searcher.schema().clone();
    let stored_f = schema.get_field("textStored").unwrap();
    let path_f = schema.get_field("filePath").unwrap();
    let mut library_docs = 0;
    for (ord, reader) in searcher.segment_readers().iter().enumerate() {
        for doc_id in reader.doc_ids_alive() {
            let doc: tantivy::TantivyDocument = searcher
                .doc(tantivy::DocAddress::new(ord as u32, doc_id))
                .unwrap();
            use tantivy::schema::Value;
            let path = doc
                .get_first(path_f)
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            let has_text = doc.get_first(stored_f).is_some();
            assert_eq!(has_text, !path.starts_with("id:"), "{path}");
            library_docs += usize::from(!has_text);
        }
    }
    assert!(library_docs > 100);
}

fn exact(e: &SearchEngine, word: &str) -> Vec<SearchResult> {
    e.search_exact(
        word.to_string(),
        vec![],
        100,
        0,
        ResultsOrder::Catalogue,
        false,
        false,
        None,
    )
    .unwrap()
}

#[test]
fn a_changed_row_is_flagged_stale_and_shown_unpainted() {
    let _guard = guard();
    let f = fixture();
    // Book 3 row 1 ("מאימתי קורין...") becomes something else; its lineHash no longer
    // matches. Row ids were inserted in reverse, so find it by (bookId, lineIndex).
    {
        let conn = Connection::open(&f.db).unwrap();
        conn.execute(
            "UPDATE line_content SET content = ?1 WHERE id = \
             (SELECT id FROM line WHERE bookId = 3 AND lineIndex = 1)",
            ["שורה שהשתנתה <b>אחרי</b> האינדוקס & עוד"],
        )
        .unwrap();
    }
    let results = exact(&f.external, "מאימתי");
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert_eq!(r.text_status, TextStatus::Stale);
    assert_eq!(r.text, "שורה שהשתנתה אחרי האינדוקס &amp; עוד");
    assert!(!r.text.contains("<font"));
    // Untouched rows of the same window stay Ok.
    let results = exact(&f.external, "קורין");
    assert!(results.iter().all(|r| r.text_status == TextStatus::Stale));
    let others = exact(&f.external, "בראשית");
    assert!(others.iter().all(|r| r.text_status == TextStatus::Ok));
    let doc = f.external.get_document_by_id(r.id).unwrap().unwrap();
    assert_eq!(doc.text_status, TextStatus::Stale);
    assert_eq!(doc.text, "שורה שהשתנתה אחרי האינדוקס & עוד");
}

#[test]
fn an_unsigned_line_is_verified_by_its_book_row_count() {
    let _guard = guard();
    let f = fixture();
    let short = |e: &SearchEngine| -> Vec<SearchResult> {
        exact(e, "אור")
            .into_iter()
            .filter(|r| r.file_path == "id:4")
            .collect()
    };
    let before = short(&f.external);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].text_status, TextStatus::Ok);

    // A row added to book 4 while the source is suspended (as an update would) shifts
    // nothing the hash could see, but the row count no longer matches the index.
    suspend_line_source().unwrap();
    {
        let conn = Connection::open(&f.db).unwrap();
        conn.execute(
            "INSERT INTO line (id, bookId, lineIndex) VALUES (5000, 4, 3)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO line_content (id, content) VALUES (5000, 'חדש')",
            [],
        )
        .unwrap();
    }
    let generation = line_source_status().generation;
    resume_line_source().unwrap();
    assert!(
        line_source_status().generation > generation,
        "resume discards caches"
    );
    let after = short(&f.external);
    assert_eq!(after[0].text_status, TextStatus::Stale);
    assert_eq!(after[0].text, "אור");
}

#[test]
fn a_missing_row_is_stale_and_empty() {
    let _guard = guard();
    let f = fixture();
    suspend_line_source().unwrap();
    {
        let conn = Connection::open(&f.db).unwrap();
        conn.execute("DELETE FROM line WHERE bookId = 3 AND lineIndex >= 9", [])
            .unwrap();
    }
    resume_line_source().unwrap();
    let results: Vec<SearchResult> = exact(&f.external, "השמים")
        .into_iter()
        .filter(|r| r.file_path == "id:3")
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].text_status, TextStatus::Stale);
    assert_eq!(results[0].text, "");
}

#[test]
fn an_unconfigured_missing_or_suspended_source_is_unavailable() {
    let _guard = guard();
    let f = fixture();
    let check = |status: TextStatus| {
        let results = exact(&f.external, "בראשית");
        assert!(!results.is_empty());
        for r in &results {
            if r.file_path.starts_with("id:") {
                assert_eq!(r.text_status, status, "{}", snap(r));
                if status == TextStatus::Unavailable {
                    assert_eq!(r.text, "");
                }
            } else {
                // Index-stored text never depends on the source.
                assert_eq!(r.text_status, TextStatus::Ok);
                assert!(r.text.contains("<font"));
            }
        }
    };
    check(TextStatus::Ok);

    suspend_line_source().unwrap();
    suspend_line_source().unwrap();
    let status = line_source_status();
    assert_eq!(status.suspend_depth, 2);
    assert!(!status.open);
    check(TextStatus::Unavailable);
    resume_line_source().unwrap();
    check(TextStatus::Unavailable);
    resume_line_source().unwrap();
    check(TextStatus::Ok);
    assert!(
        resume_line_source().is_err(),
        "an unmatched resume is refused"
    );

    configure_line_source(
        f.db.with_file_name("missing.db")
            .to_string_lossy()
            .into_owned(),
    )
    .unwrap();
    check(TextStatus::Unavailable);
    assert!(!line_source_status().open);

    line_source::reset_for_tests();
    assert!(!line_source_status().configured);
    check(TextStatus::Unavailable);

    configure_line_source(f.db.to_string_lossy().into_owned()).unwrap();
    check(TextStatus::Ok);
    assert!(line_source_status().open);
}

#[test]
fn a_new_path_bumps_the_generation_and_clears_caches() {
    let _guard = guard();
    let f = fixture();
    assert!(exact(&f.external, "בראשית")
        .iter()
        .all(|r| r.text_status == TextStatus::Ok));
    let generation = line_source_status().generation;
    // Same path: nothing is discarded.
    configure_line_source(f.db.to_string_lossy().into_owned()).unwrap();
    assert_eq!(line_source_status().generation, generation);
    assert!(line_source_status().open);

    // A copy with a different book 3 under another path: the cached lineIndex map of
    // book 3 must not survive the switch.
    let other = f.db.with_file_name("other.db");
    let mut changed = books();
    changed[2].line_indexes = (0..changed[2].rows.len() as i64).collect();
    changed[2].rows.swap(1, 2);
    write_library(&other, &changed, true);
    configure_line_source(other.to_string_lossy().into_owned()).unwrap();
    let status = line_source_status();
    assert!(status.generation > generation);
    assert!(!status.open);
    let moved = exact(&f.external, "מאימתי");
    assert_eq!(moved[0].text_status, TextStatus::Stale);
}

#[test]
fn suspend_waits_for_a_window_and_releases_the_file() {
    let _guard = guard();
    let f = fixture();
    let engine = std::sync::Arc::new(f.external);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let (engine, stop) = (engine.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut seen = HashMap::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                for r in exact(&engine, "בראשית") {
                    *seen.entry(format!("{:?}", r.text_status)).or_insert(0u32) += 1;
                }
            }
            seen
        })
    };
    let moved = f.db.with_file_name("moved.db");
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        suspend_line_source().unwrap();
        // The handle is gone once suspend returns: Windows can rename the file.
        std::fs::rename(&f.db, &moved).unwrap();
        std::fs::rename(&moved, &f.db).unwrap();
        resume_line_source().unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let seen = worker.join().expect("searches never panic");
    assert!(seen.contains_key("Ok"), "{seen:?}");
    assert!(!seen.contains_key("Stale"), "{seen:?}");
}

#[test]
fn compressed_rows_that_cannot_be_decoded_are_unavailable() {
    let _guard = guard();
    let f = fixture();
    suspend_line_source().unwrap();
    {
        let conn = Connection::open(&f.db).unwrap();
        // A corrupt frame in book 5, and a BLOB in a TEXT book.
        conn.execute(
            "UPDATE line_content SET content = X'28B52FFD00FFFF' WHERE id = \
             (SELECT id FROM line WHERE bookId = 5 AND lineIndex = 1)",
            [],
        )
        .unwrap();
    }
    resume_line_source().unwrap();
    let results: Vec<SearchResult> = exact(&f.external, "השמים")
        .into_iter()
        .filter(|r| r.file_path.starts_with("id:"))
        .collect();
    let by_book = |book: &str| {
        results
            .iter()
            .find(|r| r.file_path == book)
            .map(|r| r.text_status)
    };
    assert_eq!(by_book("id:5"), Some(TextStatus::Unavailable));
    assert_eq!(by_book("id:1"), Some(TextStatus::Ok));

    // Without zstd_dict a BLOB cannot be text.
    suspend_line_source().unwrap();
    Connection::open(&f.db)
        .unwrap()
        .execute_batch("DROP TABLE zstd_dict")
        .unwrap();
    resume_line_source().unwrap();
    let results = exact(&f.external, "תגית");
    assert_eq!(results[0].text_status, TextStatus::Unavailable);
}

#[test]
fn a_missing_host_api_is_a_defined_error_not_a_panic() {
    let _guard = guard();
    let f = fixture();
    crate::sqlite_host::SIMULATE_UNINITIALIZED.store(true, std::sync::atomic::Ordering::Release);
    let restore = scopeguard(|| {
        crate::sqlite_host::SIMULATE_UNINITIALIZED
            .store(false, std::sync::atomic::Ordering::Release)
    });
    assert!(!line_source_status().host_api_ready);
    let err = line_source::LineStore::open(&f.db).err().unwrap();
    assert!(err.to_string().contains("not initialized"), "{err:#}");
    let results = exact(&f.external, "בראשית");
    assert!(results
        .iter()
        .filter(|r| r.file_path.starts_with("id:"))
        .all(|r| r.text_status == TextStatus::Unavailable && r.text.is_empty()));
    let mut engine = SearchEngine::new(f.stored_path.to_str().unwrap());
    assert!(!engine.set_magic_dictionary_path(f.db.to_string_lossy().into_owned()));
    drop(restore);
    assert!(line_source_status().host_api_ready);
    assert!(exact(&f.external, "בראשית")
        .iter()
        .all(|r| r.text_status == TextStatus::Ok));
}

struct Restore<F: FnMut()>(F);
impl<F: FnMut()> Drop for Restore<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}
fn scopeguard<F: FnMut()>(f: F) -> Restore<F> {
    Restore(f)
}

#[test]
fn library_storage_needs_an_official_book_key() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    for bad in ["/books/a.txt", "uid:3", "id:", "id:x", "id:-1", "id:007"] {
        let err = engine
            .add_text_book(
                "t".to_string(),
                "/a".to_string(),
                bad.to_string(),
                0,
                0,
                "בראשית".to_string(),
                None,
                TextStorage::LibraryDb,
            )
            .unwrap_err();
        assert!(err.to_string().contains("id:<bookId>"), "{bad}: {err:#}");
    }
    let doc = |file_path: &str| DocumentInput {
        id: 1,
        title: "t".to_string(),
        reference: "r".to_string(),
        topics: "/a".to_string(),
        text: "בראשית".to_string(),
        segment: 0,
        is_pdf: false,
        file_path: file_path.to_string(),
        content_hash: None,
        text_hash: None,
        text_vocalized: None,
        section_id: None,
        generation_order: None,
        extra_facets: None,
        text_storage: Some(TextStorage::LibraryDb),
    };
    assert!(engine
        .add_documents_batch(vec![doc("id:1"), doc("/x.txt")])
        .is_err());
    engine.commit().unwrap();
    assert_eq!(
        engine.get_document_count(),
        0,
        "a refused batch writes nothing"
    );
}

#[test]
fn the_v4_schema_is_refused() {
    let dir = TempDir::new().unwrap();
    drop(SearchEngine::new(dir.path().to_str().unwrap()));
    let meta = dir.path().join("otzaria_index_meta.json");
    let raw = std::fs::read_to_string(&meta).unwrap();
    std::fs::write(
        &meta,
        raw.replace("\"schema_version\": 5", "\"schema_version\": 4"),
    )
    .unwrap();
    let compat = check_index_compatibility(dir.path().to_string_lossy().into_owned());
    assert!(!compat.compatible);
    assert_eq!(compat.status, "rebuild_required");
    assert_eq!(compat.required_schema_version, 5);
}

/// Read-only smoke test against a real library: indexes the first books of
/// `OTZARIA_SEFORIM_DB` both ways and compares results. Prints counts only.
#[test]
#[ignore = "needs OTZARIA_SEFORIM_DB pointing at a real seforim.db"]
fn real_library_smoke() {
    let _guard = guard();
    let Ok(db) = std::env::var("OTZARIA_SEFORIM_DB") else {
        return;
    };
    let books_wanted: usize = std::env::var("OTZARIA_SMOKE_BOOKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let root = std::env::var("OTZARIA_SMOKE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("otzaria_smoke"));
    let _ = std::fs::remove_dir_all(&root);
    let (stored_dir, external_dir) = (root.join("stored"), root.join("external"));
    std::fs::create_dir_all(&stored_dir).unwrap();
    std::fs::create_dir_all(&external_dir).unwrap();

    let conn = Connection::open_with_flags(
        format!("file:{}?mode=ro", db.replace('\\', "/")),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    conn.execute_batch("PRAGMA query_only=1").unwrap();
    // A spread of books: every n-th book that has lines, including 7410 (lineIndex gaps).
    let all: Vec<i64> = conn
        .prepare("SELECT DISTINCT bookId FROM line ORDER BY bookId")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let step = (all.len() / books_wanted.max(1)).max(1);
    let mut chosen: Vec<i64> = all
        .iter()
        .step_by(step)
        .take(books_wanted)
        .copied()
        .collect();
    if all.contains(&7410) && !chosen.contains(&7410) {
        chosen.push(7410);
    }
    let mut rows_total = 0usize;
    let mut stored = SearchEngine::new(stored_dir.to_str().unwrap());
    let mut external = SearchEngine::new(external_dir.to_str().unwrap());
    for (order, book_id) in chosen.iter().enumerate() {
        let rows: Vec<Vec<u8>> = conn
            .prepare(
                "SELECT CAST(lc.content AS BLOB) FROM line l LEFT JOIN line_content lc \
                 ON lc.id = l.id WHERE l.bookId = ?1 ORDER BY l.lineIndex",
            )
            .unwrap()
            .query_map([book_id], |r| r.get::<_, Option<Vec<u8>>>(0))
            .unwrap()
            .map(|r| r.unwrap().unwrap_or_default())
            .collect();
        rows_total += rows.len();
        let book = Book {
            id: *book_id,
            title: "ספר",
            topics: "/smoke",
            catalogue_order: order as u32,
            generation_order: (order % 7) as u32,
            line_indexes: Vec::new(),
            rows,
            compressed: false,
        };
        let title = format!("ספר {book_id}");
        for (engine, storage) in [
            (&mut stored, TextStorage::InIndex),
            (&mut external, TextStorage::LibraryDb),
        ] {
            let path = format!("id:{book_id}");
            match app_indexing_input(&book) {
                BookInput::Bytes(bytes) => engine.add_text_book_bytes(
                    title.clone(),
                    "/smoke".to_string(),
                    path,
                    order as u32,
                    0,
                    bytes,
                    None,
                    storage,
                ),
                BookInput::Text(text) => engine.add_text_book(
                    title.clone(),
                    "/smoke".to_string(),
                    path,
                    order as u32,
                    0,
                    text,
                    None,
                    storage,
                ),
            }
            .unwrap();
        }
    }
    stored.commit().unwrap();
    external.commit().unwrap();
    configure_line_source(db.clone()).unwrap();

    let queries = [
        "אמר",
        "רבי",
        "שבת",
        "ישראל",
        "אלהים",
        "משה",
        "תורה",
        "בראשית",
        "כי",
        "לא",
        "אמר רבי",
        "רבי יהודה",
        "בית המקדש",
        "קריאת שמע",
        "ויאמר משה",
        "הקדוש ברוך הוא",
        "שבת",
        "פסח",
        "כהן",
        "לוי",
        "מצוה",
        "ברכה",
        "תפילה",
        "זכור",
        "ירושלים",
        "אברהם",
        "יצחק",
        "יעקב",
        "דוד",
        "שלמה",
    ];
    // Counts: probes run, results compared, probes that differ at all, results whose
    // content differs for the same id, probes that differ only in which equally ranked
    // lines made a relevance cut (segment layout), and results not `Ok`.
    let (mut probes, mut results, mut mismatches) = (0usize, 0usize, 0usize);
    let (mut text_diffs, mut tie_only, mut not_ok) = (0usize, 0usize, 0usize);
    let started = std::time::Instant::now();
    for q in queries {
        for grouping in [None, Some(ResultGrouping::IdenticalText)] {
            let runs: [&Run<'_>; 3] = [
                &|e| {
                    e.search_exact(
                        q.to_string(),
                        vec![],
                        100,
                        0,
                        ResultsOrder::Relevance,
                        false,
                        false,
                        grouping,
                    )
                    .unwrap()
                },
                &|e| {
                    e.search_exact(
                        q.to_string(),
                        vec![],
                        100,
                        0,
                        ResultsOrder::Catalogue,
                        false,
                        false,
                        grouping,
                    )
                    .unwrap()
                },
                &|e| {
                    e.search_fuzzy(
                        q.to_string(),
                        vec![],
                        50,
                        0,
                        1,
                        ResultsOrder::Catalogue,
                        false,
                        false,
                        grouping,
                    )
                    .unwrap()
                },
            ];
            for run in runs {
                let (a, b) = (run(&stored), run(&external));
                probes += 1;
                results += b.len();
                not_ok += b.iter().filter(|r| r.text_status != TextStatus::Ok).count();
                if snaps(&a) == snaps(&b) {
                    continue;
                }
                mismatches += 1;
                let by_id: HashMap<u64, String> = a.iter().map(|r| (r.id, snap(r))).collect();
                let differing = b
                    .iter()
                    .filter(|r| by_id.get(&r.id).is_some_and(|x| *x != snap(r)))
                    .count();
                text_diffs += differing;
                tie_only += usize::from(differing == 0);
            }
        }
    }
    let elapsed = started.elapsed();
    let dir_size = |p: &Path| -> u64 {
        std::fs::read_dir(p)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.metadata().unwrap().len())
            .sum()
    };
    println!(
        "smoke: books={} rows={} probes={probes} results={results} differing_probes={mismatches} same_id_text_diffs={text_diffs} tie_only_probes={tie_only} not_ok={not_ok} stored_index_bytes={} external_index_bytes={} search_time_ms={}",
        chosen.len(),
        rows_total,
        dir_size(&stored_dir),
        dir_size(&external_dir),
        elapsed.as_millis()
    );
    assert_eq!(text_diffs, 0);
    assert_eq!(mismatches, tie_only);
    assert_eq!(not_ok, 0);
}
