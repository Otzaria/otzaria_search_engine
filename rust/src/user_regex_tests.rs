use super::*;
use crate::api::search_engine::{
    configure_attached_line_source, configure_line_source, parse_user_regex,
    prepare_user_regex_matcher, resume_line_source, suspend_line_source, ResultsOrder,
    SearchEngine, TextStorage, UserRegexCancellationToken, UserRegexKind, UserRegexStreamUpdate,
};
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn kind(raw: &str) -> Result<RegexKind, String> {
    parse(raw).map(|r| r.kind)
}

fn part(raw: &str) -> TermPart {
    let mut regex = parse(raw).unwrap();
    assert_eq!(regex.parts.len(), 1, "{raw}");
    regex.parts.remove(0)
}

#[test]
fn patterns_are_classified_by_explicit_whitespace() {
    for raw in [
        "/מש.ה/",
        "/ספר.*/",
        "/^ספר$/",
        r"/\bספר\b/",
        "/(משה|אהרן)/",
        "/משה|אהרן/",
        "/[א-ת]+ים/",
        "/.{0,200}ספר.{0,200}/",
        "/רמב\"ם/",
        "/א{2,3}/",
        "/שמע.*ישראל/",
    ] {
        assert_eq!(kind(raw), Ok(RegexKind::Word), "{raw}");
    }
    for raw in [
        "/שמע ישראל/",
        r"/שמע\s+ישראל/",
        "/(משה|אהרן) רבנו/",
        "/ מש.ה  רבנו /",
        "/שמע־ישראל/",
        r"/שמע.{0,5}\sישראל/",
    ] {
        assert_eq!(kind(raw), Ok(RegexKind::Sequence), "{raw}");
    }
    for raw in [
        "/שמע .* ישראל/",
        r"/שמע\W+ישראל/",
        r"/(\S+\s){2}ישראל/",
        "/משה|אהרן רבנו/",
        r"/שמע\bישראל/",
        r"/^\S+\s\S+$/",
        "/שמע\\s*ישראל/",
    ] {
        assert_eq!(kind(raw), Ok(RegexKind::Line), "{raw}");
    }
    for raw in ["שמע", "/", "//", "/(/", "/   /", "/^$/", "/שמע", "/[ב-א]/"] {
        assert!(kind(raw).is_err(), "{raw}");
    }
    let long = format!("/{}/", "א".repeat(MAX_PATTERN_CHARS + 1));
    assert!(kind(&long).is_err());

    let plan = parse_user_regex("/(משה|אהרן) רבנו/".to_string());
    assert_eq!(plan.kind, UserRegexKind::WordSequence);
    assert_eq!(plan.words.len(), 2);
    let plan = parse_user_regex("שמע".to_string());
    assert_eq!(plan.kind, UserRegexKind::Invalid);
    assert!(plan.reason.is_some());
}

/// Patterns whose naive DFA explodes: each is accepted or refused, never slow.
#[test]
fn explosive_patterns_compile_fast_or_are_refused() {
    let limit = if cfg!(debug_assertions) {
        Duration::from_millis(1500)
    } else {
        Duration::from_millis(200)
    };
    for raw in [
        "/.{0,200}ספר.{0,200}/",
        r"/\w+/",
        "/(?:[א-ת]|ו){50}/",
        "/((א|ב){1,7}){1,7}/",
        "/[א-ת]{1,40}/",
        "/(.{0,7}){0,7}(.{0,7}){0,7}/",
        r"/\w{0,100}\d{0,100}/",
        "/(a|aa)*b/",
        "/[^ ]{3,250}/",
        "/(.*){40}/",
        r"/\p{L}{50,250}/",
        "/(((.{0,7}){0,7}){0,7}){0,7}/",
        "/שמע .{0,250} ישראל/",
        r"/(\S+\s){1,100}ישראל/",
        "/(.{1,100} ){1,100}/",
        "/.*ספר.*/",
    ] {
        let started = Instant::now();
        let parsed = parse(raw);
        let took = started.elapsed();
        println!(
            "{raw:<40} {:>8.1} ms  {}",
            took.as_secs_f64() * 1000.0,
            match &parsed {
                Ok(r) => format!(
                    "{:?} relaxed={}",
                    r.kind,
                    r.parts.iter().any(|p| p.verify.is_some())
                ),
                Err(e) => format!("refused: {e}"),
            }
        );
        assert!(took < limit, "{raw} took {took:?}");
    }
}

#[test]
fn relaxed_term_patterns_are_verified_exactly() {
    let p = part("/א.{8,9}ב/");
    assert!(p.relaxed());
    assert!(p.accepts(&format!("א{}ב", "ג".repeat(8))));
    assert!(p.accepts(&format!("א{}ב", "ג".repeat(9))));
    assert!(!p.accepts(&format!("א{}ב", "ג".repeat(7))));
    assert!(!p.accepts(&format!("א{}ב", "ג".repeat(10))));

    let p = part("/ספר.*/");
    assert!(!p.relaxed());
    assert!(p.accepts("ספרים") && p.accepts("ספר"));
    assert!(!p.accepts("הספר"));

    // A whole term, whatever the anchors.
    for raw in ["/ספר/", "/^ספר$/", r"/\bספר\b/"] {
        let p = part(raw);
        assert!(
            p.accepts("ספר") && !p.accepts("ספרים") && !p.accepts("הספר"),
            "{raw}"
        );
    }
}

#[test]
fn final_letters_quotes_nikud_and_case_fold_like_the_index() {
    let p = part("/.*מ/");
    assert!(!p.accepts("שלום"), "a final mem is not a mem");
    assert!(part("/.*ם/").accepts("שלום"));

    let ascii = parse("/רמב\"ם/").unwrap();
    let hebrew = parse("/רמב״ם/").unwrap();
    assert_eq!(ascii.pattern, hebrew.pattern);
    assert!(hebrew.parts[0].accepts("רמב\"ם"));
    assert!(part("/תוס׳/").accepts("תוס'"));
    // Schema 6 indexes no quote-free twin: a quote-free pattern reaches the quoted term.
    assert!(part("/רמבם/").accepts("רמב\"ם"));
    assert!(part("/א.{8,9}ם/").accepts("אבגד\"הוזחטים"));

    assert!(part("/שָׁלוֹם/").accepts("שלום"));
    assert!(part("/ABC/").accepts("abc"));
    assert!(part(r"/\w+ים/").accepts("ספרים"));
}

#[test]
fn line_view_folds_like_clean_line_for_search_and_maps_back() {
    let source = "<b>שָׁלוֹם</b>&nbsp;עֲלֵיכֶם־ורבנו ״ת״ <br>סוף׃ A|B";
    let view = LineView::of(source, true);
    assert_eq!(view.text, "שלום עליכם ורבנו \"ת\" סוף a b");
    let at = view.text.find("עליכם").unwrap();
    let (start, end) = view.source_range(&(at..at + "עליכם".len()));
    let units: Vec<u16> = source.encode_utf16().collect();
    assert_eq!(
        String::from_utf16(&units[start as usize..end as usize]).unwrap(),
        "עֲלֵיכֶם"
    );
}

#[test]
fn fixed_words_of_a_line_pattern_become_required_clauses() {
    let plan = parse(r"/שמע \S+ ישראל/").unwrap().line.unwrap();
    assert_eq!(
        plan.clauses,
        vec![
            vec![Fragment {
                text: "שמע".into(),
                whole: true
            }],
            vec![Fragment {
                text: "ישראל".into(),
                whole: true
            }],
        ]
    );
    let plan = parse(r"/(משה|אהרן)\W+\S*ות/").unwrap().line.unwrap();
    assert_eq!(plan.clauses.len(), 2, "{:?}", plan.clauses);
    assert_eq!(plan.clauses[0].len(), 2);
    assert!(!plan.clauses[1][0].whole);
    assert!(parse(r"/^\S+\s\S+$/")
        .unwrap()
        .line
        .unwrap()
        .clauses
        .is_empty());
}

// ── Engine ─────────────────────────────────────────────────────────────────────

struct TestBook<'a> {
    title: &'a str,
    topics: &'a str,
    path: String,
    order: u32,
    lines: Vec<&'a str>,
}

fn stored_engine(dir: &Path, books: &[TestBook<'_>]) -> SearchEngine {
    let mut engine = SearchEngine::new(dir.to_str().unwrap());
    for book in books {
        engine
            .add_text_book_placed(
                book.title.to_string(),
                book.topics.to_string(),
                book.path.clone(),
                book.order,
                0,
                book.lines.join("\n"),
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    engine
}

fn sample_books() -> Vec<TestBook<'static>> {
    vec![
        TestBook {
            title: "ספר א",
            topics: "/תנך/תורה",
            path: "a.txt".into(),
            order: 0,
            lines: vec![
                "שמע ישראל יהוה אלהינו",
                "משה רבנו אמר",
                "אהרן רבנו",
                "ויאמר שמע",
                "ישראל עם קדוש",
                "ספרים רבים",
                "רמב\"ם כתב",
            ],
        },
        TestBook {
            title: "ספר ב",
            topics: "/תנך/נביאים",
            path: "b.txt".into(),
            order: 1,
            lines: vec!["והיה שְׁמַע בני ישראל", "שמע ישראל"],
        },
    ]
}

fn events(
    engine: &SearchEngine,
    raw: &str,
    facets: &[&str],
    cancel: &AtomicBool,
) -> Vec<UserRegexStreamUpdate> {
    let facets: Vec<String> = facets.iter().map(|f| f.to_string()).collect();
    let mut out = Vec::new();
    engine
        .user_regex_stream_into(
            raw,
            &facets,
            50,
            0,
            &ResultsOrder::Catalogue,
            10,
            cancel,
            &mut |u| {
                out.push(u);
                true
            },
        )
        .unwrap();
    out
}

fn results(events: &[UserRegexStreamUpdate]) -> Vec<&SearchResult> {
    events.iter().flat_map(|e| e.results.iter()).collect()
}

fn last(events: &[UserRegexStreamUpdate]) -> &UserRegexStreamUpdate {
    let last = events.last().expect("an event");
    assert!(last.done);
    assert_eq!(events.iter().filter(|e| e.done).count(), 1);
    last
}

fn count(engine: &SearchEngine, raw: &str, facets: &[&str]) -> u32 {
    engine
        .count_user_regex(
            raw.to_string(),
            facets.iter().map(|f| f.to_string()).collect(),
            &UserRegexCancellationToken::new(),
        )
        .unwrap()
        .count
}

#[test]
fn term_patterns_search_the_index_and_paint_their_snippets() {
    let dir = TempDir::new().unwrap();
    let engine = stored_engine(dir.path(), &sample_books());
    let none = AtomicBool::new(false);

    let found = events(&engine, "/ספר.*/", &[], &none);
    assert_eq!(last(&found).total_count, Some(1));
    assert_eq!(found[0].total_count, Some(1), "counts come first");
    let hits = results(&found);
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].text.contains("<font color=red>ספרים</font>"),
        "{}",
        hits[0].text
    );

    let found = events(&engine, "/(משה|אהרן) רבנו/", &[], &none);
    let hits = results(&found);
    assert_eq!(hits.len(), 2);
    assert!(hits
        .iter()
        .all(|h| h.text.contains("<font color=red>רבנו</font>")));

    assert_eq!(count(&engine, "/רמב״ם/", &[]), 1);
    assert_eq!(count(&engine, "/רמבם/", &[]), 1);
    assert_eq!(
        count(&engine, "/שמע/", &[]),
        4,
        "nikud in the text is ignored"
    );
    assert_eq!(count(&engine, "/שמע/", &["/תנך/נביאים"]), 2);
}

#[test]
fn term_patterns_sort_by_generation() {
    let dir = TempDir::new().unwrap();
    let engine = stored_engine(dir.path(), &sample_books());
    let mut out = Vec::new();
    engine
        .user_regex_stream_into(
            "/שמע/",
            &[],
            50,
            0,
            &ResultsOrder::Generation,
            10,
            &AtomicBool::new(false),
            &mut |u| {
                out.push(u);
                true
            },
        )
        .unwrap();
    assert_eq!(last(&out).total_count, Some(4));
    assert_eq!(results(&out).len(), 4);
}

#[test]
fn a_word_sequence_continues_onto_the_next_line() {
    let dir = TempDir::new().unwrap();
    let engine = stored_engine(dir.path(), &sample_books());
    let found = events(&engine, r"/שמע\s+ישראל/", &[], &AtomicBool::new(false));
    let hits = results(&found);
    assert_eq!(
        last(&found).total_count,
        Some(3),
        "two lines, and one across a line break"
    );
    assert_eq!(hits.iter().filter(|h| h.continues_to_next_line).count(), 1);
    let books = last(&found).book_counts.clone().unwrap();
    assert_eq!(books.get("a.txt"), Some(&2));
    assert_eq!(books.get("b.txt"), Some(&1));
}

#[test]
fn a_cross_space_pattern_verifies_candidate_lines() {
    let dir = TempDir::new().unwrap();
    let engine = stored_engine(dir.path(), &sample_books());
    let found = events(&engine, r"/שמע \S+ ישראל/", &[], &AtomicBool::new(false));
    let end = last(&found);
    assert_eq!(end.total_count, Some(1));
    assert!(!end.truncated && !end.cancelled);
    let hits = results(&found);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file_path, "b.txt");
    assert!(
        hits[0]
            .text
            .contains("<font color=red>שמע בני ישראל</font>"),
        "{}",
        hits[0].text
    );
    let ranges: Vec<_> = found.iter().flat_map(|e| e.match_ranges.iter()).collect();
    assert_eq!(ranges.len(), 1);
    let line = engine.get_document_by_id(hits[0].id).unwrap().unwrap().text;
    let units: Vec<u16> = line.encode_utf16().collect();
    let r = &ranges[0].ranges[0];
    assert_eq!(
        String::from_utf16(&units[r.start as usize..r.end as usize]).unwrap(),
        "שמע בני ישראל"
    );
    // A boundary is implied at both edges.
    assert_eq!(count(&engine, r"/מע \S+ ישראל/", &[]), 0);
}

#[test]
fn a_cross_space_pattern_finds_words_with_characters_the_index_drops() {
    let dir = TempDir::new().unwrap();
    let engine = stored_engine(
        dir.path(),
        &[TestBook {
            title: "ספר ג",
            topics: "/תנך",
            path: "c.txt".into(),
            order: 0,
            lines: vec!["כי יב[ע]ר את הקוצים", "פרק 3.14 סוף"],
        }],
    );
    assert_eq!(count(&engine, r"/יב\[ע\]ר\W+את/", &[]), 1);
    assert_eq!(count(&engine, r"/פרק\W+3\.14\W/", &[]), 1);
}

#[test]
fn a_cross_space_pattern_without_fixed_words_reads_the_books_in_scope() {
    let dir = TempDir::new().unwrap();
    let engine = stored_engine(dir.path(), &sample_books());
    let pattern = r"/^\S+\s\S+$/";
    assert!(parse(pattern).unwrap().line.unwrap().clauses.is_empty());
    // Every line of two words or more.
    assert_eq!(count(&engine, pattern, &[]), 9);
    assert_eq!(count(&engine, pattern, &["/תנך/נביאים"]), 2);
    let by_book = engine
        .count_by_book_user_regex(
            pattern.to_string(),
            Vec::new(),
            &UserRegexCancellationToken::new(),
        )
        .unwrap();
    assert_eq!(by_book.counts.get("a.txt"), Some(&7));
    let facets = engine
        .get_facet_counts_user_regex(
            pattern.to_string(),
            Vec::new(),
            "/תנך".to_string(),
            &UserRegexCancellationToken::new(),
        )
        .unwrap();
    let mut counts: Vec<(String, u64)> = facets
        .counts
        .into_iter()
        .map(|f| (f.path, f.count))
        .collect();
    counts.sort();
    assert_eq!(
        counts,
        vec![("/תנך/נביאים".to_string(), 2), ("/תנך/תורה".to_string(), 7)]
    );
}

#[test]
fn hits_beyond_the_cap_truncate_the_search() {
    let dir = TempDir::new().unwrap();
    let engine = stored_engine(dir.path(), &sample_books());
    let regex = parse(r"/^\S+\s\S+$/").unwrap();
    let searcher = engine.searcher_for_tests();
    let cancel = AtomicBool::new(false);
    let (order_epoch, order) = engine.order_tables_for_tests();
    let ctx = LineSearch {
        searcher: &searcher,
        schema: searcher.schema(),
        scope: None,
        facets: Vec::new(),
        cancel: &cancel,
        max_hits: 3,
        order,
        order_epoch,
    };
    let mut updates = Vec::new();
    let outcome = run_line_search(&ctx, &regex, None, &mut |u| {
        updates.push(u);
        true
    })
    .unwrap()
    .unwrap();
    assert_eq!(outcome.count(), 3);
    assert!(outcome.truncated);
    assert!(updates.last().unwrap().truncated);
}

#[test]
fn a_cancelled_search_stops_and_says_so() {
    let dir = TempDir::new().unwrap();
    let books: Vec<TestBook<'static>> = (0..300)
        .map(|n| TestBook {
            title: "ספר",
            topics: "/t",
            path: format!("{n}.txt"),
            order: n,
            lines: vec!["שורה ראשונה בספר", "שורה שנייה"],
        })
        .collect();
    let engine = stored_engine(dir.path(), &books);
    let pattern = r"/^\S+\s\S+$/";

    let token = UserRegexCancellationToken::new();
    token.cancel();
    assert!(token.is_cancelled());
    let err = engine
        .count_user_regex(pattern.to_string(), Vec::new(), &token)
        .err()
        .expect("a cancelled count fails");
    assert!(err.to_string().contains(CANCELLED));

    // Cancelled while reading: after the first batch of books.
    let cancel = AtomicBool::new(false);
    let mut out = Vec::new();
    engine
        .user_regex_stream_into(
            pattern,
            &[],
            10,
            0,
            &ResultsOrder::Catalogue,
            10,
            &cancel,
            &mut |u| {
                if u.progress_total > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
                out.push(u);
                true
            },
        )
        .unwrap();
    let end = last(&out);
    assert!(end.cancelled);
    let progress = out.iter().map(|e| e.progress_done).max().unwrap();
    assert!(progress < 300, "stopped after {progress} of 300 books");

    // Term kinds look at the token too.
    let cancelled = AtomicBool::new(true);
    let found = events(&engine, "/שורה/", &[], &cancelled);
    assert!(last(&found).cancelled);
}

fn three_hundred_books(dir: &Path) -> SearchEngine {
    let books: Vec<TestBook<'static>> = (0..300)
        .map(|n| TestBook {
            title: "ספר",
            topics: "/t",
            path: format!("{n}.txt"),
            order: n,
            lines: vec!["שמע בני ישראל", "שורה שנייה"],
        })
        .collect();
    stored_engine(dir, &books)
}

#[test]
fn a_cancelled_line_search_resumes_where_it_stopped() {
    // The kept search belongs to the line sources' generation, which other tests change.
    let _guard = crate::external_text_tests::guard();
    let dir = TempDir::new().unwrap();
    let engine = three_hundred_books(dir.path());
    // Candidate lines read one by one, and books read whole: two batches each.
    for (pattern, total) in [(r"/שמע \S+ ישראל/", 300), (r"/^\S+\s\S+$/", 600)] {
        let cancel = AtomicBool::new(false);
        let mut first = Vec::new();
        engine
            .user_regex_stream_into(
                pattern,
                &[],
                50,
                0,
                &ResultsOrder::Catalogue,
                10,
                &cancel,
                &mut |u| {
                    if u.progress_total > 0 {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    first.push(u);
                    true
                },
            )
            .unwrap();
        let stopped = last(&first);
        assert!(stopped.cancelled, "{pattern}");
        let read = stopped.progress_done;
        assert!(
            read > 0 && read < stopped.progress_total,
            "{pattern}: read {read}"
        );

        let resumed = events(&engine, pattern, &[], &AtomicBool::new(false));
        let end = last(&resumed);
        assert!(!end.cancelled, "{pattern}");
        assert_eq!(end.total_count, Some(total), "{pattern}");
        let progress: Vec<u32> = resumed
            .iter()
            .filter(|e| !e.done && e.progress_total > 0)
            .map(|e| e.progress_done)
            .collect();
        assert!(
            !progress.is_empty() && progress.iter().all(|&done| done > read),
            "{pattern}: read again from the start: {progress:?} after {read}"
        );
        let ids = |events: &[UserRegexStreamUpdate]| -> Vec<u64> {
            results(events).iter().map(|r| r.id).collect()
        };
        assert_eq!(ids(&resumed).len(), 50, "{pattern}");
        assert_eq!(
            ids(&resumed),
            ids(&first),
            "{pattern}: the page keeps what was found"
        );
    }
}

#[test]
fn a_concurrent_call_waits_for_the_running_scan() {
    // The kept search belongs to the line sources' generation, which other tests change.
    let _guard = crate::external_text_tests::guard();
    let dir = TempDir::new().unwrap();
    let engine = three_hundred_books(dir.path());
    let pattern = r"/^\S+\s\S+$/";
    let (started, wait_started) = std::sync::mpsc::channel();
    let (release, wait_release) = std::sync::mpsc::channel::<()>();
    let cancel_third = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let engine = &engine;
        let first = scope.spawn(move || {
            let mut out = Vec::new();
            let mut paused = false;
            engine
                .user_regex_stream_into(
                    pattern,
                    &[],
                    50,
                    0,
                    &ResultsOrder::Catalogue,
                    10,
                    &AtomicBool::new(false),
                    &mut |u| {
                        if !paused && u.progress_total > 0 {
                            paused = true;
                            started.send(()).unwrap();
                            wait_release.recv().unwrap();
                        }
                        out.push(u);
                        true
                    },
                )
                .unwrap();
            out
        });
        wait_started.recv().unwrap();
        let second = scope.spawn(move || events(engine, pattern, &[], &AtomicBool::new(false)));
        let cancel = &cancel_third;
        let third = scope.spawn(move || events(engine, pattern, &[], cancel));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!second.is_finished(), "waits for the running scan");
        cancel.store(true, Ordering::Relaxed);
        assert!(
            last(&third.join().unwrap()).cancelled,
            "a waiting call can be cancelled"
        );
        release.send(()).unwrap();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        // Two words anywhere: every line.
        assert_eq!(last(&second).total_count, Some(600));
        assert_eq!(last(&first).total_count, Some(600));
        assert!(
            second.iter().all(|e| e.progress_total == 0),
            "the waiting call read nothing itself"
        );
        assert_eq!(results(&second).len(), 50);
    });
}

#[test]
fn a_narrower_scope_reads_nothing_after_a_complete_wider_search() {
    // The kept search belongs to the line sources' generation, which other tests change.
    let _guard = crate::external_text_tests::guard();
    let dir = TempDir::new().unwrap();
    let engine = stored_engine(dir.path(), &sample_books());
    let none = AtomicBool::new(false);
    let pattern = r"/^\S+\s\S+$/";
    assert_eq!(
        last(&events(&engine, pattern, &[], &none)).total_count,
        Some(9)
    );
    let narrow = events(&engine, pattern, &["/תנך/נביאים"], &none);
    let end = last(&narrow);
    assert_eq!(end.total_count, Some(2));
    assert_eq!(
        end.book_counts.clone().unwrap(),
        HashMap::from([("b.txt".to_string(), 2)])
    );
    assert!(narrow.iter().all(|e| e.progress_total == 0), "no line read");
    let hits = results(&narrow);
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.file_path == "b.txt"));

    // A finished search that does not cover the scope does not answer it.
    let other = r"/^\S+\s+\S+$/";
    assert_eq!(
        last(&events(&engine, other, &["/תנך/נביאים"], &none)).total_count,
        Some(2)
    );
    let outside = events(&engine, other, &["/תנך/תורה"], &none);
    assert_eq!(last(&outside).total_count, Some(7));
    assert!(
        outside.iter().any(|e| e.progress_total > 0),
        "read its own scope"
    );
}

#[test]
fn every_count_fails_on_a_cancelled_token_even_when_cached() {
    let dir = TempDir::new().unwrap();
    let engine = stored_engine(dir.path(), &sample_books());
    let cancelled = UserRegexCancellationToken::new();
    cancelled.cancel();
    let fresh = UserRegexCancellationToken::new;
    // A term pattern, a cross-space one with fixed words, and one read whole.
    for pattern in ["/שמע/", r"/שמע \S+ ישראל/", r"/^\S+\s\S+$/"] {
        for warm in [false, true] {
            let label = format!("{pattern} warm={warm}");
            if warm {
                engine
                    .count_user_regex(pattern.into(), vec![], &fresh())
                    .unwrap();
            }
            let failures = [
                engine
                    .count_user_regex(pattern.into(), vec![], &cancelled)
                    .err(),
                engine
                    .count_by_book_user_regex(pattern.into(), vec![], &cancelled)
                    .err(),
                engine
                    .get_facet_counts_user_regex(pattern.into(), vec![], "/תנך".into(), &cancelled)
                    .err(),
            ];
            for err in failures {
                let err = err.unwrap_or_else(|| panic!("{label}: a cancelled count fails"));
                assert!(err.to_string().contains(CANCELLED), "{label}: {err}");
            }
            let found = events(&engine, pattern, &[], &AtomicBool::new(true));
            assert!(last(&found).cancelled, "{label}");
            assert!(results(&found).is_empty(), "{label}");
        }
    }
}

#[test]
fn the_book_view_matcher_finds_matches_in_displayed_html() {
    let data = "<b>שְׁמַע</b> בְּנִי יִשְׂרָאֵל וְעוֹד";
    let units: Vec<u16> = data.encode_utf16().collect();
    let text = |s: u32, e: u32| String::from_utf16(&units[s as usize..e as usize]).unwrap();

    let line = prepare_user_regex_matcher(r"/שמע \S+ ישראל/".to_string()).unwrap();
    let found = line.find_matches(data.to_string());
    assert_eq!(found.len(), 1);
    let m = &found[0];
    let painted = text(m.start, m.end);
    assert!(
        painted.starts_with("שְׁמַע") && painted.ends_with("יִשְׂרָאֵל"),
        "{painted}"
    );

    let words = prepare_user_regex_matcher("/(שמע|בני) .*אל/".to_string());
    let found = words.unwrap().find_matches(data.to_string());
    assert_eq!(found.len(), 1);
    assert_eq!(text(found[0].start, found[0].end), "בְּנִי יִשְׂרָאֵל");

    let word = prepare_user_regex_matcher("/ב.י/".to_string()).unwrap();
    let found = word.find_matches(data.to_string());
    assert_eq!(found.len(), 1);
    assert_eq!(text(found[0].start, found[0].end), "בְּנִי");

    assert!(prepare_user_regex_matcher("שמע".to_string()).is_none());
}

// ── Library database ───────────────────────────────────────────────────────────

/// A seforim.db of one book per entry of `books`, its rows at lineIndex 0..; rows starting
/// with `*` are stored as zstd frames of a dictionary.
fn write_library(path: &Path, books: &[(i64, &[&str])]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE book (id INTEGER PRIMARY KEY NOT NULL, title TEXT NOT NULL);
         CREATE TABLE line (id INTEGER PRIMARY KEY NOT NULL, bookId INTEGER NOT NULL,
                            lineIndex INTEGER NOT NULL, heRef TEXT);
         CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);
         CREATE TABLE line_content (id INTEGER PRIMARY KEY NOT NULL, content BLOB NOT NULL);
         CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB NOT NULL);",
    )
    .unwrap();
    let dict = crate::external_text_tests::train_dictionary();
    conn.execute("INSERT INTO zstd_dict (dict) VALUES (?1)", [&dict])
        .unwrap();
    let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
    let mut id = 0;
    for (book, rows) in books {
        conn.execute("INSERT INTO book (id, title) VALUES (?1, 'x')", [book])
            .unwrap();
        for (index, row) in rows.iter().enumerate() {
            id += 1;
            conn.execute(
                "INSERT INTO line (id, bookId, lineIndex) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, book, index as i64],
            )
            .unwrap();
            match row.strip_prefix('*') {
                Some(text) => conn.execute(
                    "INSERT INTO line_content (id, content) VALUES (?1, ?2)",
                    rusqlite::params![id, compressor.compress(text.as_bytes()).unwrap()],
                ),
                None => conn.execute(
                    "INSERT INTO line_content (id, content) VALUES (?1, ?2)",
                    rusqlite::params![id, row],
                ),
            }
            .unwrap();
        }
    }
}

fn library_engine(dir: &Path, books: &[(i64, &[&str])]) -> SearchEngine {
    let db = dir.join("seforim.db");
    write_library(&db, books);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    let idx = dir.join("idx");
    std::fs::create_dir_all(&idx).unwrap();
    let mut engine = SearchEngine::new(idx.to_str().unwrap());
    for (n, (book, rows)) in books.iter().enumerate() {
        let text: Vec<&str> = rows.iter().map(|r| r.trim_start_matches('*')).collect();
        engine
            .add_text_book_placed(
                format!("ספר {book}"),
                format!("/lib/{book}"),
                format!("id:{book}"),
                n as u32,
                0,
                text.join("\n"),
                None,
                TextStorage::LibraryDb,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    engine
}

#[test]
fn a_cross_space_pattern_reads_library_books_whole_and_compressed() {
    let _guard = crate::external_text_tests::guard();
    let dir = TempDir::new().unwrap();
    let first: &[&str] = &[
        "<h1>ספר ראשון</h1>",
        "שמע בני ישראל",
        "*וְשָׁמַע   עמי ישראל דחוס",
        "מילה",
    ];
    let second: &[&str] = &["*שמע קולי ישראל", "אחת שתיים"];
    let engine = library_engine(dir.path(), &[(7, first), (9, second)]);

    // No fixed word: every line of two words and more, read from the database.
    let pattern = r"/^\S+\s\S+$/";
    let found = events(&engine, pattern, &[], &AtomicBool::new(false));
    let end = last(&found);
    assert_eq!(end.total_count, Some(5));
    assert!(!end.truncated);
    assert!(results(&found)
        .iter()
        .all(|r| r.text_status == TextStatus::Ok));
    assert_eq!(count(&engine, pattern, &["/lib/9"]), 2, "one book only");

    // Fixed words: candidates read one by one, the compressed row among them.
    let found = events(&engine, r"/\S*שמע \S+ ישראל/", &[], &AtomicBool::new(false));
    let hits = results(&found);
    assert_eq!(
        hits.len(),
        3,
        "{:?}",
        hits.iter().map(|h| &h.text).collect::<Vec<_>>()
    );
    assert!(hits
        .iter()
        .any(|h| h.text.contains("<font color=red>ושמע עמי ישראל</font>")));

    // A suspended database cannot be read: the answer is partial and says so.
    suspend_line_source().unwrap();
    let found = events(&engine, r"/^\S+\s\S+\s\S+$/", &[], &AtomicBool::new(false));
    assert!(last(&found).truncated);
    resume_line_source().unwrap();
}

#[test]
fn a_database_that_could_not_be_read_is_read_again_by_the_next_call() {
    let _guard = crate::external_text_tests::guard();
    let dir = TempDir::new().unwrap();
    let rows: &[&str] = &["שמע בני ישראל", "*שמע עמי ישראל"];
    let engine = library_engine(dir.path(), &[(7, rows)]);
    let pattern = r"/^\S+\s\S+\s\S+$/";

    // A writer's exclusive lock makes every read busy.
    let writer = rusqlite::Connection::open(dir.path().join("seforim.db")).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let found = events(&engine, pattern, &[], &AtomicBool::new(false));
    assert!(last(&found).truncated);
    writer.execute_batch("COMMIT").unwrap();
    let found = events(&engine, pattern, &[], &AtomicBool::new(false));
    assert!(!last(&found).truncated);
    assert_eq!(last(&found).total_count, Some(2));
}

#[test]
fn a_cross_space_pattern_reads_attached_databases() {
    let _guard = crate::external_text_tests::guard();
    let dir = TempDir::new().unwrap();
    let first: &[&str] = &["<h1>מצורף</h1>", "שמע בני ישראל", "*שמע עמי ישראל"];
    let second: &[&str] = &["*שמע קולי ישראל", "מילה"];
    let idx = dir.path().join("idx");
    std::fs::create_dir_all(&idx).unwrap();
    let mut engine = SearchEngine::new(idx.to_str().unwrap());
    // Two databases with the same book id, and an `InIndex` book of a third slug.
    for (n, (slug, rows)) in [("a", first), ("b", second), ("c", second)]
        .iter()
        .enumerate()
    {
        let db = dir.path().join(format!("{slug}.db"));
        write_library(&db, &[(7, rows)]);
        if *slug != "c" {
            configure_attached_line_source(
                slug.to_string(),
                Some(db.to_string_lossy().into_owned()),
                false,
            )
            .unwrap();
        }
        let text: Vec<&str> = rows.iter().map(|r| r.trim_start_matches('*')).collect();
        engine
            .add_text_book_placed(
                format!("ספר {slug}"),
                format!("/att/{slug}"),
                format!("db:{slug}:7"),
                n as u32,
                0,
                text.join("\n"),
                None,
                if *slug == "c" {
                    TextStorage::InIndex
                } else {
                    TextStorage::LibraryDb
                },
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();

    let pattern = r"/^\S+\s\S+\s\S+$/";
    let found = events(&engine, pattern, &[], &AtomicBool::new(false));
    assert!(!last(&found).truncated);
    let mut hits: Vec<(String, u64)> = results(&found)
        .iter()
        .map(|r| (r.file_path.clone(), r.segment))
        .collect();
    hits.sort();
    let expected = [("db:a:7", 1), ("db:a:7", 2), ("db:b:7", 0), ("db:c:7", 0)];
    assert_eq!(
        hits,
        expected.map(|(path, segment)| (path.to_string(), segment))
    );
    assert!(results(&found)
        .iter()
        .all(|r| r.text_status == TextStatus::Ok));
    assert_eq!(count(&engine, pattern, &["/att/b"]), 1, "one database only");

    // A removed database cannot be read: the answer is partial and says so.
    configure_attached_line_source("a".to_string(), None, false).unwrap();
    let found = events(&engine, pattern, &[], &AtomicBool::new(false));
    assert!(last(&found).truncated);
    assert_eq!(results(&found).len(), 2);
}

#[test]
fn suspending_the_source_waits_for_a_scan_and_stops_it() {
    let _guard = crate::external_text_tests::guard();
    let dir = TempDir::new().unwrap();
    let rows: &[&str] = &["שורה אחת", "*שורה שנייה"];
    let _engine = library_engine(dir.path(), &[(7, rows)]);
    let session = line_source::begin_scan(Source::Library).unwrap();
    let read = session.read(|store| store.fetch_book_raw(7)).unwrap();
    assert_eq!(read.unwrap().0.len(), 2);

    let (done, waited) = std::sync::mpsc::channel();
    let suspender = std::thread::spawn(move || {
        suspend_line_source().unwrap();
        done.send(()).unwrap();
    });
    assert!(
        waited.recv_timeout(Duration::from_millis(300)).is_err(),
        "suspend waits for the open scan"
    );
    assert!(matches!(
        session.read(|store| store.fetch_book_raw(7)),
        Err(line_source::SourceUnavailable::Suspended)
    ));
    drop(session);
    waited.recv_timeout(Duration::from_secs(10)).unwrap();
    suspender.join().unwrap();
    assert!(line_source::begin_scan(Source::Library).is_err());
    resume_line_source().unwrap();
    assert!(line_source::begin_scan(Source::Library).is_ok());
}

// ── Real data (ignored) ────────────────────────────────────────────────────────

/// Times parsing, term searches and cross-space searches on a real index and library:
/// `OTZ_INDEX` (opened read-only) and `OTZ_LIBRARY_DB` (seforim.db, read-only).
///
/// `cargo test --release --lib user_regex::tests::real_data -- --ignored --nocapture`
#[test]
#[ignore]
fn real_data() {
    let _ = env_logger::builder().is_test(true).try_init();
    let index = std::env::var("OTZ_INDEX").expect("OTZ_INDEX");
    if let Ok(db) = std::env::var("OTZ_LIBRARY_DB") {
        configure_line_source(db).unwrap();
    }
    let engine = crate::smart_search_bench::open_read_only(Path::new(&index)).unwrap();
    let patterns = [
        "/מש.ה/",
        "/ספר.*/",
        "/.*ספר.*/",
        "/[א-ת]+ות/",
        "/.{0,200}ספר.{0,200}/",
        r"/\w+/",
        "/(משה|אהרן) רבנו/",
        r"/שמע\s+ישראל/",
        r"/שמע \S+ ישראל/",
        r"/ר' \S+ אומר/",
        r"/\bתורה\b.{0,20}\bמצוה\b/",
        r"/[א-ת]+ות\s+[א-ת]+ים/",
        r"/^\S+\s\S+$/",
        r"/[א-ת]{11,}\s[א-ת]{11,}/",
    ];
    let only: Option<String> = std::env::var("OTZ_ONLY").ok();
    let book_facet = largest_book_facet(&engine);
    println!("in-book runs use {book_facet}");
    let runs = patterns.iter().map(|raw| (*raw, None)).chain(
        [
            r"/שמע \S+ ישראל/",
            r"/[א-ת]{11,}\s[א-ת]{11,}/",
            r"/^\S+\s\S+$/",
        ]
        .into_iter()
        .map(|raw| (raw, Some(book_facet.clone()))),
    );
    for (raw, facet) in runs {
        if only.as_deref().is_some_and(|o| !raw.contains(o)) {
            continue;
        }
        let facets: Vec<String> = facet.into_iter().collect();
        let started = Instant::now();
        let parsed = parse(raw);
        let parse_ms = started.elapsed().as_secs_f64() * 1000.0;
        let Ok(parsed) = parsed else {
            println!("{raw:<36} parse {parse_ms:7.1} ms  refused");
            continue;
        };
        let started = Instant::now();
        let mut first_results: Option<f64> = None;
        let mut out = Vec::new();
        engine
            .user_regex_stream_into(
                raw,
                &facets,
                50,
                0,
                &ResultsOrder::Catalogue,
                50,
                &AtomicBool::new(false),
                &mut |u| {
                    if first_results.is_none() && !u.results.is_empty() {
                        first_results = Some(started.elapsed().as_secs_f64() * 1000.0);
                    }
                    out.push(u);
                    true
                },
            )
            .unwrap();
        let total = started.elapsed().as_secs_f64() * 1000.0;
        let end = out.last().unwrap();
        let mode = match &parsed.line {
            Some(line) => format!("line clauses={}", line.clauses.len()),
            None => format!("{:?}", parsed.kind),
        };
        let started = Instant::now();
        let facet_refs: Vec<&str> = facets.iter().map(String::as_str).collect();
        let again = count(&engine, raw, &facet_refs);
        let cached = started.elapsed().as_secs_f64() * 1000.0;
        let raw = if facets.is_empty() {
            raw.to_string()
        } else {
            format!("{raw} [book]")
        };
        println!(
            "{raw:<36} parse {parse_ms:7.1} ms | {mode:<16} | total {:>9} trunc={} | first {:>8.0} ms | all {total:>8.0} ms | count again {cached:>7.0} ms ({again})",
            end.total_count.unwrap_or(0),
            end.truncated,
            first_results.unwrap_or(f64::NAN),
        );
    }
}

/// The `topics` facet of the book with the most lines.
fn largest_book_facet(engine: &SearchEngine) -> String {
    let (path, _) = engine
        .count_documents_by_file_path()
        .unwrap()
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .unwrap();
    let searcher = engine.searcher_for_tests();
    let query = TermQuery::new(
        Term::from_field_text(searcher.schema().get_field("filePath").unwrap(), &path),
        IndexRecordOption::Basic,
    );
    let docs = searcher.search(&query, &IdAddressCollector).unwrap();
    let address = docs[0].1;
    let reader = searcher.segment_reader(address.segment_ord);
    let facets = reader.facet_reader("topics").unwrap();
    let mut facet = tantivy::schema::Facet::root();
    let ord = facets.facet_ords(address.doc_id).next().unwrap();
    facets.facet_from_ord(ord, &mut facet).unwrap();
    println!("largest book {path}: {} lines", docs.len());
    facet.to_string()
}

/// Raw row reading of every library book: one connection, then several.
#[test]
#[ignore]
fn raw_read_throughput() {
    use rayon::prelude::*;
    let index = std::env::var("OTZ_INDEX").expect("OTZ_INDEX");
    let db = std::path::PathBuf::from(std::env::var("OTZ_LIBRARY_DB").expect("OTZ_LIBRARY_DB"));
    let engine = crate::smart_search_bench::open_read_only(Path::new(&index)).unwrap();
    let searcher = engine.searcher_for_tests();
    let books = searcher
        .search(
            &tantivy::query::AllQuery,
            &ScopeBooksCollector {
                keep_documents: false,
            },
        )
        .unwrap();
    let ids: Vec<i64> = books
        .iter()
        .filter_map(|b| line_source_book(&b.file_path).map(|(_, id)| id))
        .collect();
    for threads in [1usize, 4, 8, 12] {
        let started = Instant::now();
        let bytes: usize = ids
            .par_chunks(ids.len().div_ceil(threads))
            .map(|chunk| {
                let mut store = line_source::LineStore::open(&db).unwrap();
                let mut bytes = 0;
                for &id in chunk {
                    if let Some((rows, _)) = store.fetch_book_raw(id).unwrap() {
                        bytes += rows
                            .iter()
                            .map(|r| match r {
                                RawRow::Text(b) | RawRow::Frame(b) => b.len(),
                            })
                            .sum::<usize>();
                    }
                }
                store.close();
                bytes
            })
            .sum();
        println!(
            "{threads:>2} connections: {} books, {} MB raw in {:?}",
            ids.len(),
            bytes >> 20,
            started.elapsed()
        );
    }
}

/// Where a scan's CPU goes, single-threaded, over the first books of the library.
#[test]
#[ignore]
fn scan_step_costs() {
    let index = std::env::var("OTZ_INDEX").expect("OTZ_INDEX");
    let db = std::path::PathBuf::from(std::env::var("OTZ_LIBRARY_DB").expect("OTZ_LIBRARY_DB"));
    let engine = crate::smart_search_bench::open_read_only(Path::new(&index)).unwrap();
    let searcher = engine.searcher_for_tests();
    let mut books = searcher
        .search(
            &tantivy::query::AllQuery,
            &ScopeBooksCollector {
                keep_documents: false,
            },
        )
        .unwrap();
    books.sort_unstable_by_key(|b| b.min_id);
    let plan = parse(r"/שמע \S+ ישראל/").unwrap().line.unwrap();
    let mut store = line_source::LineStore::open(&db).unwrap();
    let mut dctx = FrameDecoder::context().unwrap();
    let (mut fetch, mut decode, mut prepare, mut fill, mut prefilter, mut full) = (
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
    );
    let (mut lines, mut bytes, mut hits) = (0usize, 0usize, 0usize);
    let mut view = LineView::default();
    for book in books.iter().take(600) {
        let Some((_, id)) = line_source_book(&book.file_path) else {
            continue;
        };
        let t = Instant::now();
        let Some((rows, decoder)) = store.fetch_book_raw(id).unwrap() else {
            continue;
        };
        fetch += t.elapsed();
        for row in &rows {
            let t = Instant::now();
            let bytes_row = match row {
                RawRow::Text(b) => b.clone(),
                RawRow::Frame(f) => decoder.as_ref().unwrap().decode(&mut dctx, f).unwrap(),
            };
            decode += t.elapsed();
            let t = Instant::now();
            let text = line_source::prepare_row(&bytes_row, false);
            prepare += t.elapsed();
            let t = Instant::now();
            view.fill(&text, false);
            fill += t.elapsed();
            let t = Instant::now();
            let pre = plan.prefilter.is_match(&view.text);
            prefilter += t.elapsed();
            let t = Instant::now();
            if pre && plan.is_match(&view.text) {
                hits += 1;
            }
            full += t.elapsed();
            lines += 1;
            bytes += text.len();
        }
    }
    println!(
        "{lines} lines {} MB: fetch {fetch:?} decode {decode:?} prepare {prepare:?} view {fill:?} prefilter {prefilter:?} verify {full:?} hits {hits}",
        bytes >> 20
    );
}

#[test]
fn a_scan_that_panicked_starts_over_once() {
    let scan = Arc::new(ScanSlot::new(LineScan {
        scope: None,
        state: Progress::new(10),
        work: None,
        outcome: None,
    }));
    let held = scan.clone();
    let _ = std::thread::spawn(move || {
        let _guard = lock_unless_cancelled(&held, &AtomicBool::new(false)).unwrap();
        panic!("a scan panics");
    })
    .join();
    let none = AtomicBool::new(false);
    lock_unless_cancelled(&scan, &none).unwrap().outcome = Some(Arc::new(LineOutcome {
        hits: Vec::new(),
        book_counts: HashMap::new(),
        truncated: false,
    }));
    assert!(lock_unless_cancelled(&scan, &none)
        .unwrap()
        .outcome
        .is_some());
}

#[test]
fn a_call_waiting_for_a_running_scan_sleeps_until_it_ends() {
    let scan = Arc::new(ScanSlot::new(LineScan {
        scope: None,
        state: Progress::new(10),
        work: None,
        outcome: None,
    }));
    let (held, release) = std::sync::mpsc::channel();
    let holder = {
        let scan = scan.clone();
        std::thread::spawn(move || {
            let _guard = lock_unless_cancelled(&scan, &AtomicBool::new(false)).unwrap();
            held.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
        })
    };
    release.recv().unwrap();
    WAKEUPS.with(|w| w.set(0));
    let started = Instant::now();
    assert!(lock_unless_cancelled(&scan, &AtomicBool::new(false)).is_some());
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(
        WAKEUPS.with(|w| w.get()) <= 10,
        "{}",
        WAKEUPS.with(|w| w.get())
    );
    holder.join().unwrap();
}

#[test]
fn cached_answers_are_keyed_by_segment_order() {
    use tantivy::directory::Directory;
    use tantivy::schema::{SchemaBuilder, STRING};
    use tantivy::{doc, Index, IndexReader, ReloadPolicy};

    let mut builder = SchemaBuilder::new();
    let field = builder.add_text_field("t", STRING);
    let index = Index::create_in_ram(builder.build());
    let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
    writer.set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
    for word in ["a", "b"] {
        writer.add_document(doc!(field => word)).unwrap();
        writer.commit().unwrap();
    }
    let reader = |index: &Index| -> IndexReader {
        index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .unwrap()
    };
    let before = reader(&index).searcher();
    assert_eq!(before.segment_readers().len(), 2);

    // Same segments, listed the other way round: every cached DocAddress now points elsewhere.
    let meta_path = Path::new("meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_slice(&index.directory().atomic_read(meta_path).unwrap()).unwrap();
    meta["segments"].as_array_mut().unwrap().reverse();
    index
        .directory()
        .atomic_write(meta_path, &serde_json::to_vec(&meta).unwrap())
        .unwrap();
    let after = reader(&index).searcher();
    assert_ne!(
        before.segment_readers()[0].segment_id(),
        after.segment_readers()[0].segment_id()
    );

    assert_ne!(index_identity(&before), index_identity(&after));
}

#[test]
fn line_search_cache_observes_commits_to_every_source() {
    let _guard = crate::external_text_tests::guard();
    for source in [
        Source::Library,
        Source::UserBooks,
        Source::attached("cache-commits").unwrap(),
    ] {
        for pattern in [r"/^\S+\s\S+\s\S+$/", r"/שמע .* ישראל/"] {
            let dir = TempDir::new().unwrap();
            let db = dir.path().join("books.db");
            write_library(&db, &[(7, &["שמע בני ישראל"])]);
            let path = db.to_str().unwrap();
            let file_path = match source {
                Source::Library => {
                    configure_line_source(path.into()).unwrap();
                    "id:7"
                }
                Source::UserBooks => {
                    crate::api::search_engine::configure_user_books_line_source(Some(path.into()))
                        .unwrap();
                    "uid:7"
                }
                Source::Attached(_) => {
                    configure_attached_line_source(
                        "cache-commits".into(),
                        Some(path.into()),
                        false,
                    )
                    .unwrap();
                    "db:cache-commits:7"
                }
            };
            let index = dir.path().join("idx");
            std::fs::create_dir_all(&index).unwrap();
            let mut engine = SearchEngine::new(index.to_str().unwrap());
            engine
                .add_text_book_placed(
                    "ספר".into(),
                    "/root".into(),
                    file_path.into(),
                    0,
                    0,
                    "שמע בני ישראל".into(),
                    None,
                    TextStorage::LibraryDb,
                    None,
                )
                .unwrap();
            engine.commit().unwrap();
            assert_eq!(count(&engine, pattern, &[]), 1);
            let writer = rusqlite::Connection::open(&db).unwrap();
            writer.execute_batch("PRAGMA journal_mode=WAL").unwrap();
            for (text, expected) in [("מילה", 0), ("שמע בני ישראל", 1)] {
                writer
                    .execute("UPDATE line_content SET content = ?1", [text])
                    .unwrap();
                // A different reader must not consume the cache's invalidation.
                line_source::with_store(source, |store| store.book_row_count(7)).unwrap();
                assert_eq!(count(&engine, pattern, &["/root"]), expected);
                assert_eq!(
                    count(&engine, pattern, &[]),
                    expected,
                    "{source:?}: {pattern}"
                );
                assert_eq!(count(&engine, pattern, &[]), expected);
                let found = events(&engine, pattern, &[], &AtomicBool::new(false));
                assert_eq!(last(&found).total_count, Some(expected));
                assert_eq!(results(&found).len(), expected as usize);
            }
        }
    }
}

#[test]
fn a_count_only_scan_tracks_commits_when_the_source_started_closed() {
    let _guard = crate::external_text_tests::guard();
    let dir = TempDir::new().unwrap();
    let engine = library_engine(dir.path(), &[(7, &["שמע בני ישראל"])]);
    suspend_line_source().unwrap();
    resume_line_source().unwrap();
    assert!(!line_source::status(Source::Library).open);
    let pattern = r"/^\S+\s\S+\s\S+$/";
    assert_eq!(count(&engine, pattern, &[]), 1);
    let writer = rusqlite::Connection::open(dir.path().join("seforim.db")).unwrap();
    writer
        .execute("UPDATE line_content SET content = 'מילה'", [])
        .unwrap();
    assert_eq!(count(&engine, pattern, &[]), 0);
}

#[test]
fn unreadable_rows_mark_whole_book_scans_partial() {
    let _guard = crate::external_text_tests::guard();
    let dir = TempDir::new().unwrap();
    let engine = library_engine(dir.path(), &[(7, &["שמע בני ישראל"])]);
    let conn = rusqlite::Connection::open(dir.path().join("seforim.db")).unwrap();
    conn.execute("UPDATE line_content SET content = ?1", [vec![1u8, 2, 3]])
        .unwrap();
    let found = events(&engine, r"/^\S+\s\S+\s\S+$/", &[], &AtomicBool::new(false));
    assert!(
        last(&found).truncated,
        "an unreadable row must not become a complete zero-hit result"
    );
}

#[test]
fn unreadable_rows_preserve_other_hits_and_are_retried() {
    let _guard = crate::external_text_tests::guard();
    for missing_decoder in [false, true] {
        let dir = TempDir::new().unwrap();
        let engine = library_engine(dir.path(), &[(7, &["שמע בני ישראל", "שמע עמי ישראל"])]);
        let writer = rusqlite::Connection::open(dir.path().join("seforim.db")).unwrap();
        writer
            .execute(
                "UPDATE line_content SET content = ?1 WHERE id = 1",
                [vec![1u8, 2, 3]],
            )
            .unwrap();
        if missing_decoder {
            writer.execute_batch("DROP TABLE zstd_dict").unwrap();
        }
        let pattern = r"/^\S+\s\S+\s\S+$/";
        for _ in 0..2 {
            let found = events(&engine, pattern, &[], &AtomicBool::new(false));
            assert!(last(&found).truncated, "missing_decoder={missing_decoder}");
            assert_eq!(last(&found).total_count, Some(1));
            assert_eq!(results(&found).len(), 1);
            assert_eq!(results(&found)[0].segment, 1);
        }
        writer
            .execute(
                "UPDATE line_content SET content = 'שמע בני ישראל' WHERE id = 1",
                [],
            )
            .unwrap();
        let found = events(&engine, pattern, &[], &AtomicBool::new(false));
        assert!(!last(&found).truncated);
        assert_eq!(last(&found).total_count, Some(2));
        assert_eq!(results(&found).len(), 2);
    }
}

#[test]
fn a_commit_discards_the_partial_hits_of_a_cancelled_scan() {
    let _guard = crate::external_text_tests::guard();
    let dir = TempDir::new().unwrap();
    let rows: &[&str] = &["שמע בני ישראל"];
    let books: Vec<_> = (0..300).map(|id| (id, rows)).collect();
    let engine = library_engine(dir.path(), &books);
    let pattern = r"/^\S+\s\S+\s\S+$/";
    let cancel = AtomicBool::new(false);
    let mut first = Vec::new();
    engine
        .user_regex_stream_into(
            pattern,
            &[],
            10,
            0,
            &ResultsOrder::Catalogue,
            10,
            &cancel,
            &mut |update| {
                if update.progress_total > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
                first.push(update);
                true
            },
        )
        .unwrap();
    let stopped = last(&first);
    assert!(stopped.cancelled);
    assert!(stopped.progress_done > 0 && stopped.progress_done < stopped.progress_total);
    assert!(stopped.found_so_far > 0);
    let writer = rusqlite::Connection::open(dir.path().join("seforim.db")).unwrap();
    writer
        .execute("UPDATE line_content SET content = 'מילה'", [])
        .unwrap();
    assert_eq!(count(&engine, pattern, &[]), 0);
    let found = events(&engine, pattern, &[], &AtomicBool::new(false));
    assert!(!last(&found).cancelled);
    assert!(!last(&found).truncated);
    assert!(results(&found).is_empty());
}
