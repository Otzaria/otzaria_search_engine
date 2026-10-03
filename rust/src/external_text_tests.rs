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

/// A library database of the one plain book `book_id`, its `rows` at line indexes 0 and on.
#[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
pub(crate) fn write_book(path: &Path, book_id: i64, rows: &[&str]) {
    let book = Book {
        id: book_id,
        title: "ספר",
        topics: "/root",
        catalogue_order: 0,
        generation_order: 0,
        line_indexes: (0..rows.len() as i64).collect(),
        rows: rows.iter().map(|row| row.as_bytes().to_vec()).collect(),
        compressed: false,
    };
    write_library(path, &[book], false);
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
    // Indexing checks every LibraryDb book against the configured source.
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    // Index mode reads the same rows the app would, decoded.
    let stored = index_books(&stored_path, &books, TextStorage::InIndex);
    let external = index_books(&external_path, &books, TextStorage::LibraryDb);
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

type Run<'a> = dyn Fn(&SearchEngine) -> Vec<SearchResult> + 'a;

type OrderOf = fn() -> ResultsOrder;

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
            None,
            &SemanticCancellationToken::new(),
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

/// `(filePath, has textStored, lineCheck)` of every live document.
fn doc_store_view(engine: &SearchEngine) -> Vec<(String, bool, Option<u64>)> {
    use tantivy::schema::Value;
    let searcher = engine.corpus_searcher();
    let schema = searcher.schema().clone();
    let stored_f = schema.get_field("textStored").unwrap();
    let path_f = schema.get_field("filePath").unwrap();
    let mut out = Vec::new();
    for (ord, reader) in searcher.segment_readers().iter().enumerate() {
        let checks = reader.fast_fields().u64("lineCheck").unwrap();
        for doc_id in reader.doc_ids_alive() {
            let doc: tantivy::TantivyDocument = searcher
                .doc(tantivy::DocAddress::new(ord as u32, doc_id))
                .unwrap();
            let path = doc
                .get_first(path_f)
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            out.push((
                path,
                doc.get_first(stored_f).is_some(),
                checks.first(doc_id),
            ));
        }
    }
    out
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
    let mut library_docs = 0;
    for (path, has_text, check) in doc_store_view(&f.external) {
        assert_eq!(has_text, !path.starts_with("id:"), "{path}");
        // Only library lines carry a check, and every one of them does.
        assert_eq!(check.is_some(), !has_text, "{path}");
        library_docs += usize::from(!has_text);
    }
    assert!(library_docs > 100);
    assert!(doc_store_view(&f.stored)
        .iter()
        .all(|(_, has_text, check)| *has_text && check.is_none()));
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

/// Every live line of each book, in line order: its address and what its `chunkKey`
/// column holds.
fn chunk_key_columns(
    engine: &SearchEngine,
) -> std::collections::BTreeMap<String, Vec<(tantivy::DocAddress, u64)>> {
    let searcher = engine.corpus_searcher();
    let mut books: std::collections::BTreeMap<String, Vec<(u64, tantivy::DocAddress, u64)>> =
        Default::default();
    for (ord, reader) in searcher.segment_readers().iter().enumerate() {
        let fast = reader.fast_fields();
        let ids = fast.u64("id").unwrap();
        let keys = fast.u64("chunkKey").unwrap();
        let paths = fast.str("filePath").unwrap().unwrap();
        for doc in reader.doc_ids_alive() {
            let mut path = String::new();
            let ord_of_path = paths.term_ords(doc).next().unwrap();
            paths.ord_to_str(ord_of_path, &mut path).unwrap();
            books.entry(path).or_default().push((
                ids.first(doc).unwrap(),
                tantivy::DocAddress::new(ord as u32, doc),
                keys.first(doc).unwrap(),
            ));
        }
    }
    books
        .into_iter()
        .map(|(path, mut lines)| {
            lines.sort_unstable_by_key(|line| line.0);
            let lines = lines.into_iter().map(|(_, a, k)| (a, k)).collect();
            (path, lines)
        })
        .collect()
}

/// A library line is keyed from the text it was indexed from, as a stored one is: the
/// `chunkKey` columns of the two indexes agree, and a key recomputed from the library row
/// is the column's. A row changed since is keyed as it reads now; a source that cannot be
/// read fails the recomputation instead of keying an empty line.
#[test]
fn library_lines_are_keyed_like_stored_ones() {
    use otzaria_semantic_search::semantic::chunk_key::ChunkKey;
    let _guard = guard();
    let f = fixture();
    let (stored, external) = (chunk_key_columns(&f.stored), chunk_key_columns(&f.external));
    let columns = |books: &std::collections::BTreeMap<String, Vec<(tantivy::DocAddress, u64)>>| {
        books
            .iter()
            .map(|(path, lines)| (path.clone(), lines.iter().map(|l| l.1).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
    };
    assert_eq!(columns(&stored), columns(&external));
    assert!(external.values().flatten().any(|line| line.1 != 0));

    let searcher = f.external.corpus_searcher();
    let recomputed = |path: &str| -> Vec<u64> {
        let book: Vec<_> = external[path].iter().map(|line| line.0).collect();
        crate::semantic_keys::recompute_chunk_keys(&searcher, &book, 0..book.len())
            .unwrap()
            .into_iter()
            .map(|key| key.map_or(0, |key: ChunkKey| key.column_value()))
            .collect()
    };
    for (path, lines) in &external {
        let column: Vec<u64> = lines.iter().map(|line| line.1).collect();
        assert_eq!(recomputed(path), column, "{path}");
    }

    // Book 3 row 1 ("מאימתי קורין...") is long enough to be keyed as its own text.
    let book = &external["id:3"];
    let line = book[1].0;
    let alone = || {
        crate::semantic_keys::recompute_chunk_key(&searcher, &[line], 0)
            .unwrap()
            .expect("a line long enough to stand alone is embedded")
    };
    let before = alone();
    assert_eq!(before.column_value(), book[1].1);
    {
        let conn = Connection::open(&f.db).unwrap();
        conn.execute(
            "UPDATE line_content SET content = ?1 WHERE id = \
             (SELECT id FROM line WHERE bookId = 3 AND lineIndex = 1)",
            ["שורה שהשתנתה אחרי האינדוקס ואין בה מאומה מן הקודמת"],
        )
        .unwrap();
    }
    let after = alone();
    assert_ne!(after, before, "a changed row is keyed as it reads now");

    suspend_line_source().unwrap();
    let refused = crate::semantic_keys::recompute_chunk_key(&searcher, &[line], 0);
    resume_line_source().unwrap();
    let error = refused.expect_err("an unreadable source keys nothing");
    assert!(format!("{error:#}").contains("cannot be read"), "{error:#}");
    // The stored index never asks the source.
    let stored_line = stored["id:3"][1].0;
    suspend_line_source().unwrap();
    let kept =
        crate::semantic_keys::recompute_chunk_key(&f.stored.corpus_searcher(), &[stored_line], 0);
    resume_line_source().unwrap();
    assert_eq!(kept.unwrap(), Some(before));
}

#[test]
fn appending_rows_leaves_existing_lines_ok() {
    let _guard = guard();
    let f = fixture();
    let short = |e: &SearchEngine| -> Vec<SearchResult> {
        exact(e, "אור")
            .into_iter()
            .filter(|r| r.file_path == "id:4")
            .collect()
    };
    assert_eq!(short(&f.external)[0].text_status, TextStatus::Ok);

    // A row appended to book 4 while the source is suspended (as an update would): the
    // indexed rows are where they were, unchanged.
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
    assert_eq!(after[0].text_status, TextStatus::Ok);
    assert!(after[0].text.contains("אור"));
}

/// A library of one book, `rows` at lineIndex 0.., and the book.
fn one_book_library(dir: &Path, rows: &[&str]) -> (PathBuf, Book) {
    let db = dir.join("lib.db");
    let book = Book {
        id: 1,
        title: "ספר",
        topics: "/t",
        catalogue_order: 0,
        generation_order: 0,
        line_indexes: (0..rows.len() as i64).collect(),
        rows: rows.iter().map(|r| r.as_bytes().to_vec()).collect(),
        compressed: false,
    };
    write_library(&db, std::slice::from_ref(&book), false);
    (db, book)
}

/// `book` indexed the way the app does, into `dir/idx`; returns the documents added.
fn index_one(dir: &Path, book: &Book, storage: TextStorage) -> (SearchEngine, u32) {
    let idx = dir.join("idx");
    std::fs::create_dir_all(&idx).unwrap();
    let mut engine = SearchEngine::new(idx.to_str().unwrap());
    let path = format!("id:{}", book.id);
    let added = match app_indexing_input(book) {
        BookInput::Bytes(bytes) => engine.add_text_book_bytes(
            book.title.to_string(),
            book.topics.to_string(),
            path,
            0,
            0,
            bytes,
            None,
            storage,
        ),
        BookInput::Text(text) => engine.add_text_book(
            book.title.to_string(),
            book.topics.to_string(),
            path,
            0,
            0,
            text,
            None,
            storage,
        ),
    }
    .unwrap();
    engine.commit().unwrap();
    (engine, added)
}

const LONG1: &str = "שורה ארוכה ראשונה ובה הרבה מילים בראשית ברא אלהים";
const LONG2: &str = "שורה ארוכה שנייה ובה הרבה מילים ויאמר משה אל העם";

/// Edits `db` with the source suspended, as a library update does.
fn edit_suspended(db: &Path, sql: &str) {
    suspend_line_source().unwrap();
    Connection::open(db).unwrap().execute_batch(sql).unwrap();
    resume_line_source().unwrap();
}

#[test]
fn a_short_line_whose_row_shifted_is_stale() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let (db, book) = one_book_library(dir.path(), &["<h1>ספר</h1>", LONG1, "אור", LONG2, "מים"]);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    let (engine, _) = index_one(dir.path(), &book, TextStorage::LibraryDb);
    assert_eq!(exact(&engine, "אור")[0].text_status, TextStatus::Ok);

    // One row deleted, the rest renumbered and one appended: the book keeps its row count,
    // and ordinal 2 (the indexed "אור") now holds LONG2.
    edit_suspended(
        &db,
        "DELETE FROM line_content WHERE id = (SELECT id FROM line WHERE bookId = 1 AND lineIndex = 1);
         DELETE FROM line WHERE bookId = 1 AND lineIndex = 1;
         UPDATE line SET lineIndex = lineIndex - 1 WHERE bookId = 1 AND lineIndex > 1;
         INSERT INTO line (id, bookId, lineIndex) VALUES (9000, 1, 4);
         INSERT INTO line_content (id, content) VALUES (9000, 'חדש');",
    );
    let after = exact(&engine, "אור");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].text_status, TextStatus::Stale);
    assert_eq!(after[0].text, LONG2);
}

/// `Ok` vouches for the text, not the position: a row deleted without renumbering leaves
/// the lines after it at their `lineIndex`, with the indexed text.
#[test]
fn a_gap_left_by_a_deleted_row_is_stale_only_where_the_text_differs() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let (db, book) = one_book_library(dir.path(), &["<h1>ספר</h1>", LONG1, "אור", LONG2, "מים"]);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    let (engine, _) = index_one(dir.path(), &book, TextStorage::LibraryDb);
    edit_suspended(
        &db,
        "DELETE FROM line_content WHERE id = (SELECT id FROM line WHERE bookId = 1 AND lineIndex = 1);
         DELETE FROM line WHERE bookId = 1 AND lineIndex = 1;",
    );
    let at_its_index = exact(&engine, "אור");
    let shown = at_its_index[0]
        .text
        .replace("<font color=red>", "")
        .replace("</font>", "");
    assert_eq!(
        (at_its_index[0].text_status, shown.as_str()),
        (TextStatus::Ok, "אור")
    );
    // Ordinal 1 has no row at lineIndex 1; by position it is now "אור".
    let gone = exact(&engine, "ראשונה");
    assert_eq!(
        (gone[0].text_status, gone[0].text.as_str()),
        (TextStatus::Stale, "אור")
    );
}

#[test]
fn a_short_line_edited_in_place_is_stale() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let (db, book) = one_book_library(dir.path(), &["<h1>ספר</h1>", LONG1, "אור", LONG2]);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    let (engine, _) = index_one(dir.path(), &book, TextStorage::LibraryDb);
    edit_suspended(
        &db,
        "UPDATE line_content SET content = 'חושך' WHERE id = \
         (SELECT id FROM line WHERE bookId = 1 AND lineIndex = 2)",
    );
    let after = exact(&engine, "אור");
    assert_eq!(after[0].text_status, TextStatus::Stale);
    assert_eq!(after[0].text, "חושך");
}

#[test]
fn spacing_punctuation_and_nikud_changes_are_stale() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let (db, book) = one_book_library(dir.path(), &["<h1>ספר</h1>", LONG1, LONG2]);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    let (engine, _) = index_one(dir.path(), &book, TextStorage::LibraryDb);
    assert!(exact(&engine, "שורה")
        .iter()
        .all(|r| r.text_status == TextStatus::Ok));
    // The letters (all `lineHash` signs) stay; only spacing, a mark and nikud change.
    edit_suspended(
        &db,
        &format!(
            "UPDATE line_content SET content = '{}' WHERE id = \
             (SELECT id FROM line WHERE bookId = 1 AND lineIndex = 1);
             UPDATE line_content SET content = '{}' WHERE id = \
             (SELECT id FROM line WHERE bookId = 1 AND lineIndex = 2);",
            LONG1.replace(' ', "  ").replace("אלהים", "אלהים."),
            LONG2.replace("משה", "מֹשֶׁה"),
        ),
    );
    let after = exact(&engine, "שורה");
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|r| r.text_status == TextStatus::Stale));
}

#[test]
fn a_row_containing_a_newline_keeps_its_book_in_the_index() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let rows = [
        "<h1>ספר</h1>",
        "שורה ראשונה\nעם שבירה פנימית בראשית ברא אלהים",
        LONG2,
        "אור",
    ];
    let (db, book) = one_book_library(dir.path(), &rows);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    let (engine, added) = index_one(dir.path(), &book, TextStorage::LibraryDb);
    // Five lines out of four rows: every ordinal after the break would point one row back.
    assert_eq!(added, 5);
    assert!(doc_store_view(&engine)
        .iter()
        .all(|(_, has_text, check)| *has_text && check.is_none()));
    for (word, text) in [("משה", LONG2), ("אור", "אור")] {
        let results = exact(&engine, word);
        assert_eq!(results[0].text_status, TextStatus::Ok);
        assert!(
            results[0]
                .text
                .replace("<font color=red>", "")
                .replace("</font>", "")
                == text,
            "{word}"
        );
    }
}

#[test]
fn a_book_indexed_without_a_readable_source_keeps_its_text() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let (db, book) = one_book_library(dir.path(), &["<h1>ספר</h1>", LONG1, "אור"]);
    // Unconfigured, then suspended: nothing can vouch for the rows.
    let (engine, _) = index_one(&dir.path().join("a"), &book, TextStorage::LibraryDb);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    suspend_line_source().unwrap();
    let (suspended, _) = index_one(&dir.path().join("b"), &book, TextStorage::LibraryDb);
    resume_line_source().unwrap();
    let (library, _) = index_one(&dir.path().join("c"), &book, TextStorage::LibraryDb);
    for e in [&engine, &suspended] {
        assert!(doc_store_view(e).iter().all(|(_, has_text, _)| *has_text));
    }
    assert!(doc_store_view(&library)
        .iter()
        .all(|(_, has_text, _)| !has_text));
}

#[test]
fn a_write_without_suspend_is_seen_by_the_next_window() {
    let _guard = guard();
    let f = fixture();
    let first = |word: &str| {
        exact(&f.external, word)
            .into_iter()
            .find(|r| r.file_path == "id:3")
            .unwrap()
    };
    assert_eq!(first("מאימתי").text_status, TextStatus::Ok);
    assert_eq!(first("השמים").text_status, TextStatus::Ok);
    // Rows 1 and 2 of book 3 trade places, and its last rows go: with the ordinal map
    // cached from the window above, ordinal 1 would still read the old row.
    {
        let conn = Connection::open(&f.db).unwrap();
        conn.execute_batch(
            "UPDATE line SET lineIndex = 3 - lineIndex WHERE bookId = 3 AND lineIndex IN (1, 2);
             DELETE FROM line WHERE bookId = 3 AND lineIndex >= 9;",
        )
        .unwrap();
    }
    let moved = first("מאימתי");
    assert_eq!(moved.text_status, TextStatus::Stale);
    assert_eq!(moved.text, "משעה שהכהנים נכנסים לאכול בתרומתן");
    let gone = first("השמים");
    assert_eq!(
        (gone.text_status, gone.text.as_str()),
        (TextStatus::Stale, "")
    );
}

#[test]
fn a_unc_path_is_opened_as_a_plain_path() {
    let _guard = guard();
    // No such host: the open fails as a missing file, not as a URI SQLite refuses.
    let err = line_source::LineStore::open(Path::new(r"\\nas\books\seforim.db"))
        .err()
        .expect("there is no such share");
    let message = format!("{err:#}");
    assert!(
        !message.contains("authority") && !message.contains("URI"),
        "{message}"
    );

    // The same database through the administrative share, where the machine has one.
    let dir = TempDir::new().unwrap();
    let (db, _) = one_book_library(dir.path(), &["<h1>ספר</h1>", LONG1]);
    let absolute = std::fs::canonicalize(&db).unwrap();
    let local = absolute
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_string();
    let Some((drive, rest)) = local.split_once(':') else {
        return;
    };
    let unc = PathBuf::from(format!(r"\\localhost\{drive}${rest}"));
    if std::fs::metadata(&unc).is_err() {
        return;
    }
    for path in [unc, PathBuf::from(format!(r"\\?\{local}"))] {
        let mut store = line_source::LineStore::open(&path)
            .unwrap_or_else(|err| panic!("{}: {err:#}", path.display()));
        let rows = store
            .fetch_window(
                &[line_source::LineKey {
                    book_id: 1,
                    ordinal: 1,
                }],
                &[None],
            )
            .unwrap();
        assert!(matches!(&rows[0], line_source::RowText::Found(text) if text == LONG1));
        store.close();
    }
}

#[test]
fn a_busy_database_is_skipped_without_waiting_each_window() {
    let _guard = guard();
    let f = fixture();
    let library_statuses = |results: Vec<SearchResult>| {
        results
            .into_iter()
            .filter(|r| r.file_path.starts_with("id:"))
            .map(|r| format!("{:?}", r.text_status))
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        library_statuses(exact(&f.external, "בראשית")),
        ["Ok".to_string()].into()
    );
    let writer = Connection::open(&f.db).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let started = std::time::Instant::now();
    let first = library_statuses(exact(&f.external, "בראשית"));
    let first_ms = started.elapsed().as_millis();
    let started = std::time::Instant::now();
    let second = library_statuses(exact(&f.external, "בראשית"));
    let second_ms = started.elapsed().as_millis();
    assert_eq!(first, ["Unavailable".to_string()].into());
    assert_eq!(second, ["Unavailable".to_string()].into());
    // One busy timeout, then nothing until the backoff ends.
    assert!(first_ms < 600, "first window {first_ms} ms");
    assert!(second_ms < 60, "second window {second_ms} ms");
    assert!(line_source_status().open, "a busy database is not reopened");
    writer.execute_batch("COMMIT").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(
        library_statuses(exact(&f.external, "בראשית")),
        ["Ok".to_string()].into()
    );
}

#[test]
fn a_whole_book_reads_like_its_windows() {
    let _guard = guard();
    let f = fixture();
    let mut store = line_source::LineStore::open(&f.db).unwrap();
    for book in books() {
        let keys: Vec<line_source::LineKey> = (0..book.rows.len() as u64)
            .map(|ordinal| line_source::LineKey {
                book_id: book.id,
                ordinal,
            })
            .collect();
        let windows = store.fetch_window(&keys, &vec![None; keys.len()]).unwrap();
        let whole = store.fetch_book(book.id).unwrap().unwrap();
        assert_eq!(
            format!("{whole:?}"),
            format!("{windows:?}"),
            "book {}",
            book.id
        );
        // Read by lineIndex first, with their checks: the same rows.
        let checks: Vec<Option<u32>> = whole
            .iter()
            .map(|row| match row {
                line_source::RowText::Found(text) => Some(line_source::line_check(text)),
                _ => None,
            })
            .collect();
        let checked = store.fetch_window(&keys, &checks).unwrap();
        assert_eq!(
            format!("{whole:?}"),
            format!("{checked:?}"),
            "book {}",
            book.id
        );
        assert!(whole
            .iter()
            .all(|row| matches!(row, line_source::RowText::Found(_))));
    }
    assert!(store.fetch_book(999).unwrap().is_none());
    store.close();
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
fn only_books_whose_rows_are_not_at_their_line_index_get_a_row_map() {
    let _guard = guard();
    let f = fixture();
    suspend_line_source().unwrap();
    resume_line_source().unwrap();
    for word in ["בראשית", "אור", "אלהים"] {
        let results = exact(&f.external, word);
        assert_all_ok(&results, word);
    }
    // Book 3's lineIndex has gaps; every other book is read by lineIndex alone.
    assert_eq!(line_source::mapped_books_for_tests(), [3]);
}

/// A library of one book with explicit `(rowid, lineIndex, text)` rows (no `line_content`).
fn library_with_rows(dir: &Path, rows: &[(i64, i64, &str)]) -> (PathBuf, Book) {
    let db = dir.join("lib.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL,
                            lineIndex INTEGER NOT NULL, content TEXT);
         CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);",
    )
    .unwrap();
    for (id, line_index, text) in rows {
        conn.execute(
            "INSERT INTO line (id, bookId, lineIndex, content) VALUES (?1, 1, ?2, ?3)",
            rusqlite::params![id, line_index, text],
        )
        .unwrap();
    }
    let mut ordered = rows.to_vec();
    ordered.sort_by_key(|&(id, line_index, _)| (line_index, id));
    let book = Book {
        id: 1,
        title: "ספר",
        topics: "/t",
        catalogue_order: 0,
        generation_order: 0,
        line_indexes: Vec::new(),
        rows: ordered.iter().map(|r| r.2.as_bytes().to_vec()).collect(),
        compressed: false,
    };
    (db, book)
}

#[test]
fn repeated_and_missing_line_indexes_read_the_indexed_rows() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    // Ordinal 2 is at lineIndex 1 and ordinal 3 (lineIndex 2) holds the same text; ordinal
    // 3 then finds ordinal 4's row at lineIndex 3, and nothing is at lineIndex 4 or 6.
    let rows = [
        (10, 0, "<h1>ספר</h1>"),
        (11, 1, LONG1),
        (12, 1, LONG2),
        (13, 2, LONG2),
        (14, 3, LONG1),
        (15, 5, "אור משה גדול"),
        (16, 9, "סוף בראשית ברא"),
    ];
    let (db, book) = library_with_rows(dir.path(), &rows);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    let (stored, _) = index_one(&dir.path().join("s"), &book, TextStorage::InIndex);
    let (external, added) = index_one(&dir.path().join("x"), &book, TextStorage::LibraryDb);
    assert_eq!(added as usize, rows.len());
    assert!(doc_store_view(&external).iter().all(|(_, text, _)| !text));
    for cold in [true, false, true] {
        if cold {
            suspend_line_source().unwrap();
            resume_line_source().unwrap();
        }
        for word in ["ארוכה", "משה", "בראשית", "אור"] {
            let (a, b) = (exact(&stored, word), exact(&external, word));
            assert!(!b.is_empty(), "{word}");
            assert_all_ok(&b, word);
            assert_same(word, &a, &b);
        }
        for ordinal in 1..=rows.len() as u64 {
            let id = (1u64 << 32) + ordinal;
            let a = stored.get_document_by_id(id).unwrap().unwrap();
            let b = external.get_document_by_id(id).unwrap().unwrap();
            assert_eq!(b.text_status, TextStatus::Ok, "{ordinal}");
            assert_eq!(snap(&a), snap(&b), "{ordinal}");
        }
        assert_eq!(line_source::mapped_books_for_tests(), [1]);
    }
}

#[test]
fn a_changed_row_at_its_line_index_is_still_stale() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let (db, book) = one_book_library(dir.path(), &["<h1>ספר</h1>", LONG1, LONG2]);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    let (engine, _) = index_one(dir.path(), &book, TextStorage::LibraryDb);
    edit_suspended(
        &db,
        &format!("UPDATE line_content SET content = '{LONG1} ' WHERE content = '{LONG1}'"),
    );
    let results = exact(&engine, "ראשונה");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].text_status, TextStatus::Stale);
    // The miss fell back to the ordinal map, which found the same row.
    assert_eq!(line_source::mapped_books_for_tests(), [1]);
}

#[test]
fn indexing_waits_out_a_window_backoff_and_counts_its_fallbacks() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let (db, book) = one_book_library(dir.path(), &["<h1>ספר</h1>", LONG1, "אור", LONG2]);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    let fallbacks = || line_source_status().library_fallbacks;
    let before = fallbacks();
    let writer = Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let (busy, _) = index_one(&dir.path().join("a"), &book, TextStorage::LibraryDb);
    writer.execute_batch("COMMIT").unwrap();
    drop(writer);
    assert_eq!(fallbacks(), before + 1);
    // Still inside the backoff a window would honor: indexing reads the database anyway.
    let (after, _) = index_one(&dir.path().join("b"), &book, TextStorage::LibraryDb);
    let stored = |e: &SearchEngine| doc_store_view(e).iter().filter(|(_, t, _)| *t).count();
    assert_eq!((stored(&busy), stored(&after)), (4, 0));
    assert_eq!(fallbacks(), before + 1);
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
    // Indexing opened the source; the next window must open it again.
    suspend_line_source().unwrap();
    resume_line_source().unwrap();
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
    // A ready-made document cannot be tied to its library row: refused, even for an
    // official book.
    for batch in [vec![doc("id:1")], vec![doc("id:1"), doc("/x.txt")]] {
        let err = engine.add_documents_batch(batch).unwrap_err();
        assert!(err.to_string().contains("add_text_book"), "{err:#}");
    }
    assert!(engine.upsert_documents_batch(vec![doc("id:1")]).is_err());
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
    configure_line_source(db.clone()).unwrap();
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
    let in_index = doc_store_view(&external)
        .iter()
        .filter(|(_, has_text, _)| *has_text)
        .count();

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
        "smoke: books={} rows={} probes={probes} results={results} differing_probes={mismatches} same_id_text_diffs={text_diffs} tie_only_probes={tie_only} not_ok={not_ok} library_lines_stored_in_index={in_index} stored_index_bytes={} external_index_bytes={} search_time_ms={}",
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

/// Read-only check against a real library: for every book with a BOM, `data:` or `\r` in
/// its rows, books with `lineIndex` gaps, and every 50th book, each row the line source
/// reads back must be exactly the line the app's indexing input splits into — the text
/// `lineCheck` is taken over. Prints counts only.
#[test]
#[ignore = "needs OTZARIA_SEFORIM_DB pointing at a real seforim.db"]
fn real_library_rows_match_the_indexing_input() {
    let _guard = guard();
    let Ok(db) = std::env::var("OTZARIA_SEFORIM_DB") else {
        return;
    };
    let conn = Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    conn.execute_batch("PRAGMA query_only=1").unwrap();
    let mut ids: std::collections::BTreeSet<i64> = conn
        .prepare(
            "SELECT DISTINCT l.bookId FROM line l JOIN line_content lc ON lc.id = l.id \
             WHERE substr(lc.content, 1, 1) = char(65279) OR instr(lc.content, 'data:') > 0 \
             OR instr(lc.content, char(13)) > 0 OR instr(lc.content, char(10)) > 0",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let special = ids.len();
    // The filter reads TEXT rows: a zstd library would leave nothing special to check.
    assert!(special > 0, "no special books found (compressed rows?)");
    ids.extend(
        conn.prepare(
            "SELECT bookId FROM line GROUP BY bookId HAVING count(*) <> max(lineIndex) + 1 \
             OR min(lineIndex) <> 0 OR bookId % 50 = 0",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(Result::unwrap),
    );
    let mut store = line_source::LineStore::open(Path::new(&db)).unwrap();
    let (mut rows_checked, mut diffs, mut split_books) = (0u64, 0u64, 0u64);
    for &book_id in &ids {
        let rows: Vec<Vec<u8>> = conn
            .prepare_cached(
                "SELECT CAST(lc.content AS BLOB) FROM line l LEFT JOIN line_content lc \
                 ON lc.id = l.id WHERE l.bookId = ?1 ORDER BY l.lineIndex, l.id",
            )
            .unwrap()
            .query_map([book_id], |r| r.get::<_, Option<Vec<u8>>>(0))
            .unwrap()
            .map(|r| r.unwrap().unwrap_or_default())
            .collect();
        let book = Book {
            id: book_id,
            title: "t",
            topics: "/t",
            catalogue_order: 0,
            generation_order: 0,
            line_indexes: Vec::new(),
            rows,
            compressed: false,
        };
        let input = match app_indexing_input(&book) {
            BookInput::Bytes(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            BookInput::Text(text) => text,
        };
        let app_lines: Vec<&str> = input.split('\n').collect();
        let ours = store.fetch_book(book_id).unwrap().unwrap();
        if ours.len() != app_lines.len() {
            // Indexed with its text in the index (a row containing `\n`).
            split_books += 1;
            continue;
        }
        for (line, row) in app_lines.iter().zip(ours) {
            rows_checked += 1;
            match row {
                line_source::RowText::Found(text) if text == *line => {}
                _ => diffs += 1,
            }
        }
    }
    store.close();
    println!(
        "rows match: books={} special_books={special} rows={rows_checked} diffs={diffs} \
         books_with_split_rows={split_books}",
        ids.len()
    );
    assert_eq!(diffs, 0);
}

/// Working set and private bytes of this process (Windows; zeros elsewhere).
fn process_memory() -> (u64, u64) {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        #[repr(C)]
        #[derive(Default)]
        struct Counters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_non_paged_pool_usage: usize,
            quota_non_paged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
            private_usage: usize,
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentProcess() -> *mut c_void;
            fn K32GetProcessMemoryInfo(
                process: *mut c_void,
                counters: *mut Counters,
                cb: u32,
            ) -> i32;
        }
        let mut c = Counters {
            cb: std::mem::size_of::<Counters>() as u32,
            ..Default::default()
        };
        // SAFETY: a correctly sized PROCESS_MEMORY_COUNTERS_EX for the current process.
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        (c.working_set_size as u64, c.private_usage as u64)
    }
    #[cfg(not(windows))]
    {
        (0, 0)
    }
}

/// Read-only timing against a real library, release build: every `OTZARIA_PERF_STEP`-th
/// book (25) indexed both ways into a temp dir, then the same queries against both, 7
/// repetitions each. Prints numbers only.
#[test]
#[ignore = "needs OTZARIA_SEFORIM_DB; run with --release"]
fn real_library_perf() {
    let _guard = guard();
    let Ok(db) = std::env::var("OTZARIA_SEFORIM_DB") else {
        return;
    };
    let step: usize = std::env::var("OTZARIA_PERF_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);
    let root = std::env::var("OTZARIA_PERF_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("otzaria_perf"));
    let (stored_dir, external_dir) = (root.join("stored"), root.join("external"));
    let compatible = |p: &Path| {
        std::fs::read_dir(p).is_ok_and(|mut d| d.next().is_some())
            && check_index_compatibility(p.to_string_lossy().into_owned()).compatible
    };
    let reuse = std::env::var("OTZARIA_PERF_REUSE").is_ok()
        && compatible(&stored_dir)
        && compatible(&external_dir);
    let started = std::time::Instant::now();
    let (stored, external, books, rows_total) = if reuse {
        (
            SearchEngine::new(stored_dir.to_str().unwrap()),
            SearchEngine::new(external_dir.to_str().unwrap()),
            0,
            0,
        )
    } else {
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&stored_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        // Indexing checks LibraryDb books against the configured source.
        configure_line_source(db.clone()).unwrap();
        let conn = Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        conn.execute_batch("PRAGMA query_only=1").unwrap();
        let all: Vec<i64> = conn
            .prepare("SELECT DISTINCT bookId FROM line ORDER BY bookId")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let chosen: Vec<i64> = all.iter().step_by(step).copied().collect();
        let mut stored = SearchEngine::new(stored_dir.to_str().unwrap());
        let mut external = SearchEngine::new(external_dir.to_str().unwrap());
        let mut rows_total = 0usize;
        for (order, book_id) in chosen.iter().enumerate() {
            let rows: Vec<Vec<u8>> = conn
                .prepare_cached(
                    "SELECT CAST(lc.content AS BLOB) FROM line l LEFT JOIN line_content lc \
                     ON lc.id = l.id WHERE l.bookId = ?1 ORDER BY l.lineIndex, l.id",
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
                topics: "/perf",
                catalogue_order: order as u32,
                generation_order: 0,
                line_indexes: Vec::new(),
                rows,
                compressed: false,
            };
            for (engine, storage) in [
                (&mut stored, TextStorage::InIndex),
                (&mut external, TextStorage::LibraryDb),
            ] {
                let path = format!("id:{book_id}");
                match app_indexing_input(&book) {
                    BookInput::Bytes(bytes) => engine.add_text_book_bytes(
                        "ספר".into(),
                        "/perf".into(),
                        path,
                        order as u32,
                        0,
                        bytes,
                        None,
                        storage,
                    ),
                    BookInput::Text(text) => engine.add_text_book(
                        "ספר".into(),
                        "/perf".into(),
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
        stored.optimize().unwrap();
        external.optimize().unwrap();
        (stored, external, chosen.len(), rows_total)
    };
    let build_ms = started.elapsed().as_millis();
    line_source::reset_for_tests();
    let memory_before = process_memory();
    configure_line_source(db.clone()).unwrap();

    let queries = [
        "אמר",
        "רבי",
        "שבת",
        "ישראל",
        "משה",
        "תורה",
        "כי",
        "לא",
        "אמר רבי",
        "רבי יהודה",
        "בית המקדש",
        "קריאת שמע",
        "הקדוש ברוך הוא",
        "פסח",
        "כהן",
        "מצוה",
        "ברכה",
        "תפילה",
        "ירושלים",
        "אברהם",
        "יעקב",
        "דוד",
        "שלמה",
        "אלא",
        "היינו",
    ];
    let run = |e: &SearchEngine, q: &str, relevance: bool| {
        let order = if relevance {
            ResultsOrder::Relevance
        } else {
            ResultsOrder::Catalogue
        };
        let t = std::time::Instant::now();
        let r = e
            .search_exact(q.to_string(), vec![], 100, 0, order, false, false, None)
            .unwrap();
        let not_ok = r.iter().filter(|x| x.text_status != TextStatus::Ok).count();
        (t.elapsed(), r.len(), not_ok)
    };
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let pct = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[v.len() / 2], v[v.len() * 9 / 10])
    };
    // Cold: the first window after a resume (every cache dropped), against a warm
    // stored-text run of the same query.
    let mut cold = Vec::new();
    let mut cold_delta = Vec::new();
    for q in queries {
        let (s, n, _) = run(&stored, q, false);
        suspend_line_source().unwrap();
        resume_line_source().unwrap();
        let (x, n2, _) = run(&external, q, false);
        assert_eq!(n, n2);
        if n > 0 {
            cold.push(ms(x));
            cold_delta.push((ms(x) - ms(s)) * 100.0 / n as f64);
        }
    }
    for q in queries {
        for relevance in [false, true] {
            run(&stored, q, relevance);
            run(&external, q, relevance);
        }
    }
    let (mut deltas, mut s_times, mut e_times) = (Vec::new(), Vec::new(), Vec::new());
    let (mut total, mut not_ok_total) = (0usize, 0usize);
    for relevance in [false, true] {
        for q in queries {
            let (mut s, mut x, mut n) = (Vec::new(), Vec::new(), 0);
            for _ in 0..7 {
                let (d, c, _) = run(&stored, q, relevance);
                s.push(ms(d));
                let (d, c2, bad) = run(&external, q, relevance);
                x.push(ms(d));
                assert_eq!(c, c2);
                not_ok_total += bad;
                n = c;
            }
            if n == 0 {
                continue;
            }
            total += n;
            let (s50, _) = pct(&mut s);
            let (x50, _) = pct(&mut x);
            let per100 = |v: f64| v * 100.0 / n as f64;
            s_times.push(per100(s50));
            e_times.push(per100(x50));
            deltas.push(per100(x50) - per100(s50));
        }
    }
    let mut equivalence_results = 0;
    for q in queries {
        let a = stored
            .search_exact(
                q.into(),
                vec![],
                100,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
            )
            .unwrap();
        let b = external
            .search_exact(
                q.into(),
                vec![],
                100,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
            )
            .unwrap();
        let signature = |r: &SearchResult| {
            (
                r.id,
                r.segment,
                r.file_path.clone(),
                r.reference.clone(),
                r.text.clone(),
                r.text_status,
            )
        };
        assert_eq!(
            a.iter().map(signature).collect::<Vec<_>>(),
            b.iter().map(signature).collect::<Vec<_>>(),
            "query={q}"
        );
        equivalence_results += a.len();
    }
    println!("perf: catalogue equivalence: {equivalence_results} results");
    let memory_after = process_memory();
    let (map_bytes, page_cache_bytes) = line_source::memory_for_tests().unwrap_or_default();
    let (d50, d90) = pct(&mut deltas);
    let (s50, _) = pct(&mut s_times);
    let (e50, _) = pct(&mut e_times);
    let (c50, c90) = pct(&mut cold);
    let (cd50, cd90) = pct(&mut cold_delta);
    let size = |p: &Path| -> u64 {
        std::fs::read_dir(p)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.metadata().unwrap().len())
            .sum()
    };
    let mb = |b: u64| b as f64 / 1_048_576.0;
    println!(
        "perf: reused={reuse} build_ms={build_ms} books={books} rows={rows_total} probes={} \
         results={total} not_ok={not_ok_total} stored_p50_ms_per100={s50:.2} \
         external_p50_ms_per100={e50:.2} delta_p50={d50:.2} delta_p90={d90:.2} \
         cold_window_ms_p50={c50:.1} cold_window_ms_p90={c90:.1} \
         cold_delta_per100_p50={cd50:.2} cold_delta_per100_p90={cd90:.2} \
         ws_mb_before={:.1} ws_mb_after={:.1} private_mb_before={:.1} private_mb_after={:.1} \
         row_map_kb={} page_cache_kb={} stored_bytes={} external_bytes={}",
        deltas.len(),
        mb(memory_before.0),
        mb(memory_after.0),
        mb(memory_before.1),
        mb(memory_after.1),
        map_bytes / 1024,
        page_cache_bytes / 1024,
        size(&stored_dir),
        size(&external_dir)
    );
    drop(stored);
    drop(external);
    line_source::reset_for_tests();
    if std::env::var("OTZARIA_PERF_KEEP").is_err() {
        let _ = std::fs::remove_dir_all(&root);
    }
}
