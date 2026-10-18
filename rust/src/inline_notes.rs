//! Port of the app's `rehydrateInlineNotes` (`inline_notes_utils.dart`): from schema 7 a
//! footnote's body is in `line_note`, and only its `<sup>` marker stays in the line. The
//! indexing path puts each body back, so that words found only in footnotes are searchable.
//!
//! Offsets follow the app's UTF-16 strings: `charPos` counts UTF-16 units, an entity counts
//! as one. The patterns are Dart (JavaScript) `RegExp`s: ASCII case folding, ASCII `\b` and
//! JavaScript's `\s`.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

/// One `line_note` row of a line, in `noteIndex` order. `char_pos` is -1 when it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookNote {
    /// UTF-16 offset in the stored line, counting each HTML entity as one unit; -1 if absent.
    pub char_pos: i64,
    /// Whether the database marker is non-null (an empty marker still counts).
    pub has_marker: bool,
    /// Original footnote body HTML, without the surrounding footnote element.
    pub body: String,
}

/// A book's notes by `lineIndex`.
pub(crate) type BookNotes = HashMap<i64, Vec<BookNote>>;

const JS_SPACE: &str = r"[\t\n\x0B\x0C\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";
const B: &str = r"(?-u:\b)";

static FOOTNOTE_BODY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r#"(?s)<(?i-u:i){B}[^>]*{B}(?i-u:class){JS_SPACE}*={JS_SPACE}*"[^"]*{B}(?i-u:footnote){B}[^"]*"[^>]*>(.*?)</(?i-u:i)>"#
    ))
    .unwrap()
});

// The generator leaves footnote tags inside any sup in the marker's HTML.
static SUP: Lazy<Regex> =
    Lazy::new(|| Regex::new(&format!(r"(?s)<(?i-u:sup){B}[^>]*>.*?</(?i-u:sup)>")).unwrap());

/// Whether a body outside a sup is already present. Walk both sets of ranges in
/// order, so checking many markers and notes stays linear in the line's length.
fn has_rehydrated_body(html: &str, sup_ranges: &[std::ops::Range<usize>]) -> bool {
    let mut bodies = FOOTNOTE_BODY.find_iter(html).peekable();
    if bodies.peek().is_none() {
        return false;
    }
    let mut sups = sup_ranges.iter().peekable();
    bodies.any(|body| {
        while sups.peek().is_some_and(|sup| sup.end <= body.start()) {
            sups.next();
        }
        !sups.peek().is_some_and(|sup| sup.start <= body.start())
    })
}

// Shared with the app's inline_notes_utils.dart. A closing block or unknown tag
// stays after the restored body; crossing it can merge the next paragraph's words.
const INLINE_TAG_NAMES: &[&str] = &[
    "b", "i", "em", "strong", "u", "s", "strike", "span", "sup", "sub", "small", "big", "font",
    "del", "ins", "a", "abbr", "acronym", "cite", "code", "dfn", "kbd", "q", "samp", "tt", "var",
    "mark", "bdi", "bdo", "ruby", "rt", "rp",
];

static FOOTNOTE_MARKER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r"(?s)<(?i-u:sup){B}[^>]*{B}(?i-u:footnote-marker){B}[^>]*>.*?</(?i-u:sup)>"
    ))
    .unwrap()
});

static FOOTNOTE_BODY_START: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r#"^{JS_SPACE}*<(?i-u:i){B}[^>]*{B}(?i-u:class){JS_SPACE}*={JS_SPACE}*"[^"]*{B}(?i-u:footnote){B}"#
    ))
    .unwrap()
});

/// The byte ranges of markers whose body was moved out: those not followed by one.
fn stripped_markers(html: &str) -> Vec<std::ops::Range<usize>> {
    FOOTNOTE_MARKER
        .find_iter(html)
        .filter(|m| !FOOTNOTE_BODY_START.is_match(&html[m.end()..]))
        .map(|m| m.range())
        .collect()
}

/// Where a body goes: a byte offset, and whether the app's UTF-16 offset falls between the
/// two surrogates of the character starting there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Insertion {
    at: usize,
    splits_char: bool,
}

/// `_rawInsertionPoint`: after the `visible`-th visible UTF-16 unit, with the closing tags
/// right after it only when they close inline formatting (e.g. `</sup>`).
/// Stop before block boundaries so the body remains in its original paragraph.
fn raw_insertion_point(html: &str, visible: i64) -> Insertion {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let end_of_line = Insertion {
        at: len,
        splits_char: false,
    };
    let find = |from: usize, byte: u8| {
        bytes[from..]
            .iter()
            .position(|&b| b == byte)
            .map(|p| p + from)
    };
    let mut seen = 0i64;
    let mut i = 0usize;
    while i < len && seen < visible {
        match bytes[i] {
            b'<' => match find(i, b'>') {
                Some(close) => {
                    i = close + 1;
                    continue;
                }
                None => return end_of_line,
            },
            b'&' => {
                i = match find(i + 1, b';') {
                    Some(end) if html[i..end].encode_utf16().take(10).count() < 10 => end + 1,
                    _ => i + 1,
                };
            }
            _ => {
                let ch = html[i..].chars().next().expect("i is on a char boundary");
                if ch.len_utf16() == 2 {
                    if seen + 1 == visible {
                        return Insertion {
                            at: i,
                            splits_char: true,
                        };
                    }
                    seen += 1;
                }
                i += ch.len_utf8();
            }
        }
        seen += 1;
    }
    if seen < visible {
        return end_of_line;
    }
    Insertion {
        at: after_inline_closings(html, i),
        splits_char: false,
    }
}

fn after_inline_closings(html: &str, mut at: usize) -> usize {
    while html[at..].starts_with("</") {
        let Some(close) = html[at..].find('>').map(|close| at + close) else {
            break;
        };
        let name = html[at + 2..close].trim_end();
        if !INLINE_TAG_NAMES
            .iter()
            .any(|inline| name.eq_ignore_ascii_case(inline))
        {
            break;
        }
        at = close + 1;
    }
    at
}

fn only_tags_between(html: &str, mut at: usize, end: usize) -> bool {
    while at < end {
        if html.as_bytes()[at] != b'<' {
            return false;
        }
        let Some(close) = html[at..end].find('>') else {
            return false;
        };
        at += close + 1;
    }
    at == end
}

fn footnote_html(body: &str) -> String {
    format!("<i class=\"footnote\">{body}</i>")
}

/// Restores schema 7 footnote HTML for database readers and indexing.
///
/// Supply notes in database `noteIndex` order. Each body is inserted at its `charPos`;
/// a note without one is inserted after its marker
/// (the k-th stripped `<sup>` belongs to the k-th note with a marker), else at the end. A line
/// that already holds a body outside a sup is left alone. `None` when unchanged.
pub fn rehydrate_inline_notes(line: &str, notes: &[BookNote]) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let sups: Vec<_> = SUP.find_iter(line).map(|sup| sup.range()).collect();
    if has_rehydrated_body(line, &sups) {
        return None;
    }
    let markers = stripped_markers(line);
    let mut anchored_seen = 0;
    let mut previous_position: Option<(i64, Insertion)> = None;
    let mut inserts: Vec<(Insertion, usize)> = Vec::with_capacity(notes.len());
    for (order, note) in notes.iter().enumerate() {
        let marker = if note.has_marker {
            markers.get(anchored_seen)
        } else {
            None
        };
        let mut at = if note.char_pos >= 0 {
            let at = raw_insertion_point(line, note.char_pos);
            // The generator never extracts a body inside a sup. Empty trailing
            // markup can leave the visible scan there, including for a second,
            // unanchored body after the same marker. Its start is excluded: an
            // unanchored body at that offset may precede the sup entirely.
            let inside = sups
                .get(sups.partition_point(|sup| sup.end <= at.at))
                .filter(|sup| sup.start < at.at);
            // Empty markers have no visible width. A charPos scan can therefore
            // stop before their opening formatting tags. Follow only this note's
            // corresponding marker, and only across complete tags with no text.
            let empty_marker = marker.filter(|marker| {
                at.at <= marker.start && only_tags_between(line, at.at, marker.start)
            });
            match inside.or(empty_marker) {
                Some(sup) => Insertion {
                    at: after_inline_closings(line, sup.end),
                    splits_char: false,
                },
                None => at,
            }
        } else if let Some(marker) = marker {
            Insertion {
                at: marker.end,
                splits_char: false,
            }
        } else {
            Insertion {
                at: line.len(),
                splits_char: false,
            }
        };
        if note.char_pos >= 0 {
            // Adjacent bodies share charPos. Keep their source order even when
            // only the first is anchored to an empty marker at this offset.
            if let Some((char_pos, previous)) = previous_position {
                if char_pos == note.char_pos && previous > at {
                    at = previous;
                }
            }
            previous_position = Some((note.char_pos, at));
        } else {
            previous_position = None;
        }
        if note.has_marker {
            anchored_seen += 1;
        }
        inserts.push((at, order));
    }
    inserts.sort();
    let mut out =
        String::with_capacity(line.len() + notes.iter().map(|n| n.body.len() + 30).sum::<usize>());
    let mut last = 0;
    for (n, &(insert, order)) in inserts.iter().enumerate() {
        out.push_str(&line[last..insert.at]);
        last = insert.at;
        // The app's string holds a lone surrogate on each side, which its UTF-8 encoder
        // writes as U+FFFD.
        let opens_split = insert.splits_char && (n == 0 || inserts[n - 1].0 != insert);
        if opens_split {
            out.push('\u{FFFD}');
        }
        out.push_str(&footnote_html(&notes[order].body));
        let closes_split = insert.splits_char && inserts.get(n + 1).map(|i| i.0) != Some(insert);
        if closes_split {
            out.push('\u{FFFD}');
            last += line[insert.at..].chars().next().map_or(0, char::len_utf8);
        }
    }
    out.push_str(&line[last..]);
    Some(out)
}

/// `rehydrateInlineNotesInText` on one stored row: its `\n`-separated lines are lines
/// `first_line..` of the book's text, as the app splits the joined text.
pub(crate) fn rehydrate_row(row: &str, first_line: i64, notes: &BookNotes) -> Option<String> {
    if !row.contains('\n') {
        return rehydrate_inline_notes(row, notes.get(&first_line)?);
    }
    let mut changed = false;
    let lines: Vec<std::borrow::Cow<str>> = row
        .split('\n')
        .enumerate()
        .map(|(n, line)| {
            match notes
                .get(&(first_line + n as i64))
                .and_then(|notes| rehydrate_inline_notes(line, notes))
            {
                Some(rehydrated) => {
                    changed = true;
                    rehydrated.into()
                }
                None => line.into(),
            }
        })
        .collect();
    changed.then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(char_pos: i64, marker: Option<&str>, body: &str) -> BookNote {
        BookNote {
            char_pos,
            has_marker: marker.is_some(),
            body: body.to_string(),
        }
    }

    fn rehydrate(line: &str, notes: &[BookNote]) -> String {
        rehydrate_inline_notes(line, notes).unwrap_or_else(|| line.to_string())
    }

    const STRIPPED: &str = "גוף<sup class=\"footnote-marker\">א</sup> המשך\
                            <sup class=\"footnote-marker\">ב</sup> סוף";

    fn db_notes() -> Vec<BookNote> {
        vec![
            note(-1, Some("א"), "ראשונה"),
            note(-1, None, "נספחת"),
            note(-1, Some("ב"), "שנייה"),
        ]
    }

    // The app's inline_notes_utils_test.dart, group 'הערות מהמסד (line_note)'.
    #[test]
    fn a_body_goes_back_after_its_marker_and_the_rest_to_the_end() {
        assert_eq!(
            rehydrate(STRIPPED, &db_notes()),
            "גוף<sup class=\"footnote-marker\">א</sup>\
             <i class=\"footnote\">ראשונה</i> המשך\
             <sup class=\"footnote-marker\">ב</sup>\
             <i class=\"footnote\">שנייה</i> סוף\
             <i class=\"footnote\">נספחת</i>"
        );
        assert_eq!(rehydrate_inline_notes(STRIPPED, &[]), None);
        let notes: BookNotes = [(4, db_notes())].into();
        let row = format!("א\n\n\n\n{STRIPPED}");
        assert_eq!(
            rehydrate_row(&row, 0, &notes)
                .unwrap()
                .split('\n')
                .nth(4)
                .unwrap(),
            rehydrate(STRIPPED, &db_notes())
        );
        // The same line as the 5th line of a later row.
        assert_eq!(
            rehydrate_row(STRIPPED, 4, &notes),
            rehydrate_inline_notes(STRIPPED, &db_notes())
        );
        assert_eq!(rehydrate_row(STRIPPED, 3, &notes), None);
    }

    // Group 'line_note עם charPos ושמות קודש בגוף'.
    const STORED: &str = "גוף<sup class=\"footnote-marker\">א</sup> <b>המשך</b> סוף";

    #[test]
    fn each_body_goes_back_at_its_offset() {
        assert_eq!(
            rehydrate(
                STORED,
                &[note(4, Some("א"), "ראשונה"), note(9, None, "באמצע")]
            ),
            "גוף<sup class=\"footnote-marker\">א</sup>\
             <i class=\"footnote\">ראשונה</i> <b>המשך</b>\
             <i class=\"footnote\">באמצע</i> סוף"
        );
    }

    #[test]
    fn an_entity_counts_as_one_char_and_two_notes_at_one_place_keep_their_order() {
        assert_eq!(
            rehydrate("x&amp;y", &[note(2, None, "א"), note(2, None, "ב")]),
            "x&amp;<i class=\"footnote\">א</i><i class=\"footnote\">ב</i>y"
        );
    }

    #[test]
    fn an_offset_past_the_end_goes_to_the_end() {
        assert_eq!(
            rehydrate("קצר", &[note(99, None, "ז")]),
            "קצר<i class=\"footnote\">ז</i>"
        );
    }

    // book_notes_test.dart: 'rehydrate אידמפוטנטי'.
    #[test]
    fn rehydrating_twice_does_not_duplicate_the_body() {
        let stored = "גוף<sup class=\"footnote-marker\">א</sup> סוף";
        let notes = [note(-1, Some("א"), "ראשונה"), note(-1, None, "נספחת")];
        let once = rehydrate(stored, &notes);
        assert_eq!(rehydrate_inline_notes(&once, &notes), None);
        let by_line: BookNotes = [(0, notes.to_vec())].into();
        assert_eq!(rehydrate_row(&once, 0, &by_line), None);
        assert_eq!(once.matches("class=\"footnote\"").count(), 2);
    }

    #[test]
    fn insertion_points_follow_the_apps_utf16_scan() {
        let at = |html: &str, visible| raw_insertion_point(html, visible).at;
        // Tags take no offset; closing tags right after a char belong to it.
        assert_eq!(at("<b>אב</b>ג", 2), "<b>אב</b>".len());
        assert_eq!(at("</b>א", 0), "</b>".len());
        assert_eq!(at("א<b", 5), "א<b".len());
        // An `&` that does not end within 10 units is one char by itself.
        assert_eq!(at("&nbsp;x", 1), "&nbsp;".len());
        assert_eq!(at("&אבגדהוזחטי;x", 2), "&א".len());
        assert_eq!(at("&אבגדהוזח;x", 1), "&אבגדהוזח;".len());
        // A character outside the BMP is two units.
        assert_eq!(at("😀x", 2), "😀".len());
        assert_eq!(
            raw_insertion_point("😀x", 1),
            Insertion {
                at: 0,
                splits_char: true
            }
        );
        assert_eq!(
            rehydrate("a😀b", &[note(2, None, "x"), note(2, None, "y")]),
            "a\u{FFFD}<i class=\"footnote\">x</i><i class=\"footnote\">y</i>\u{FFFD}b"
        );
    }

    #[test]
    fn markers_and_bodies_match_like_the_apps_patterns() {
        // A marker still followed by its body is not a stripped one.
        let kept = "<sup class=\"footnote-marker\">א</sup> <I CLASS = \"x footnote\">";
        assert!(stripped_markers(kept).is_empty());
        let line =
            "<SUP class=\"footnote-marker\">א</SUP>מילה<sup class=\"footnote-markers\">ב</sup>";
        assert_eq!(
            stripped_markers(line),
            vec![0.."<SUP class=\"footnote-marker\">א</SUP>".len()]
        );
        // `footnotes` is not `footnote`; `<i>` of another class is not a body.
        assert!(!FOOTNOTE_BODY.is_match("<i class=\"footnotes\">x</i>"));
        assert!(FOOTNOTE_BODY.is_match("<i data-x=1 class=\"a footnote\">x\ny</I>"));
        assert_eq!(
            rehydrate("<i class=\"footnote\">כבר</i>", &[note(0, None, "x")]),
            "<i class=\"footnote\">כבר</i>"
        );
    }
}

#[cfg(test)]
mod rehydration_regression_tests {
    use super::*;
    #[test]
    fn a_body_before_a_closing_block_preserves_indexed_words() {
        let original =
            "<p>גוף<sup class=\"footnote-marker\">א</sup><i class=\"footnote\">הערה</i></p>המשך";
        let stored = "<p>גוף<sup class=\"footnote-marker\">א</sup></p>המשך";
        let notes = [BookNote {
            char_pos: 4,
            has_marker: true,
            body: "הערה".into(),
        }];
        let actual = rehydrate_inline_notes(stored, &notes).unwrap();
        assert_eq!(
            crate::hebrew_query::strip_html_for_indexing(&actual),
            crate::hebrew_query::strip_html_for_indexing(original)
        );
    }
    #[test]
    fn nested_footnote_tags_in_any_sup_remain_part_of_the_marker() {
        for marker in [
            "<sup class=\"footnote-marker\">1<i class=\"footnote\"></i></sup>",
            "<SUP>1<i class=\"footnote\">בתוך</i></SUP>",
        ] {
            let stored = format!("א{marker} ב");
            let notes = [BookNote {
                char_pos: -1,
                has_marker: marker.starts_with("<sup class="),
                body: "הערה חיצונית".into(),
            }];
            let actual = rehydrate_inline_notes(&stored, &notes).unwrap();
            assert!(actual.contains(marker));
            assert!(actual.contains("<i class=\"footnote\">הערה חיצונית</i>"));
            assert_eq!(rehydrate_inline_notes(&actual, &notes), None);
        }
    }

    #[test]
    fn inline_closings_are_crossed_but_block_and_unknown_closings_are_not() {
        for block in ["p", "DIV", "li", "td", "custom-block"] {
            let stored = format!("<{block}><b><span>אב</span></b></{block}>המשך");
            let expected = format!(
                "<{block}><b><span>אב</span></b><i class=\"footnote\">הערה</i></{block}>המשך"
            );
            assert_eq!(
                rehydrate_inline_notes(
                    &stored,
                    &[BookNote {
                        char_pos: 2,
                        has_marker: false,
                        body: "הערה".into()
                    }]
                ),
                Some(expected)
            );
        }
        assert_eq!(
            raw_insertion_point("<B>אב</B ></p>", 2).at,
            "<B>אב</B >".len()
        );
        assert_eq!(raw_insertion_point("<b>אב</ b></p>", 2).at, "<b>אב".len());
    }
    #[test]
    fn an_anchored_body_stays_outside_empty_tags_at_the_end_of_its_marker() {
        let marker = "<sup class=\"footnote-marker\">1<i class=\"footnote\"></i></sup>";
        let stored = format!("א{marker} ב");
        let notes = [BookNote {
            char_pos: 2,
            has_marker: true,
            body: "הערה זרזיר".into(),
        }];
        let expected = format!("א{marker}<i class=\"footnote\">הערה זרזיר</i> ב");
        assert_eq!(
            rehydrate_inline_notes(&stored, &notes),
            Some(expected.clone())
        );
        assert_eq!(rehydrate_inline_notes(&expected, &notes), None);
    }
    #[test]
    fn adjacent_bodies_after_a_marker_keep_their_source_order() {
        for (marker, char_pos) in [
            (
                "<sup class=\"footnote-marker\">1<i class=\"footnote\"></i></sup>",
                1,
            ),
            ("<sup class=\"footnote-marker\"></sup>", 0),
        ] {
            let notes = [
                BookNote {
                    char_pos,
                    has_marker: true,
                    body: "ראשונה".into(),
                },
                BookNote {
                    char_pos,
                    has_marker: false,
                    body: "שנייה".into(),
                },
            ];
            assert_eq!(
                rehydrate_inline_notes(marker, &notes),
                Some(format!(
                    "{marker}<i class=\"footnote\">ראשונה</i><i class=\"footnote\">שנייה</i>"
                ))
            );
        }
    }

    #[test]
    fn an_empty_anchored_marker_after_leading_tags_keeps_its_body_in_the_block() {
        let marker = "<sup class=\"footnote-marker\"></sup>";
        for (prefix, suffix) in [("<p>", "</p>"), ("<b>", "</b>"), ("<p><b>", "</b></p>")] {
            let stored = format!("{prefix}{marker}המשך{suffix}");
            let expected =
                format!("{prefix}{marker}<i class=\"footnote\">הערה זרזיר</i>המשך{suffix}");
            let actual = rehydrate_inline_notes(
                &stored,
                &[BookNote {
                    char_pos: 0,
                    has_marker: true,
                    body: "הערה זרזיר".into(),
                }],
            )
            .unwrap();
            assert_eq!(actual, expected, "{prefix}");
            assert_eq!(
                crate::hebrew_query::strip_html_for_indexing(&actual),
                crate::hebrew_query::strip_html_for_indexing(&expected)
            );
        }
        // Cross inline closings after the marker, but retain the paragraph boundary.
        let stored = format!("<p><b>{marker}</b></p>המשך");
        assert_eq!(
            rehydrate_inline_notes(
                &stored,
                &[BookNote {
                    char_pos: 0,
                    has_marker: true,
                    body: "הערה זרזיר".into(),
                }]
            ),
            Some(format!(
                "<p><b>{marker}</b><i class=\"footnote\">הערה זרזיר</i></p>המשך"
            ))
        );
    }

    #[test]
    fn adjacent_bodies_after_a_wrapped_empty_marker_keep_their_source_order() {
        let marker = "<sup class=\"footnote-marker\"></sup>";
        let stored = format!("<p><b>{marker}המשך</b></p>");
        let notes = [
            BookNote {
                char_pos: 0,
                has_marker: true,
                body: "ראשונה".into(),
            },
            BookNote {
                char_pos: 0,
                has_marker: false,
                body: "שנייה".into(),
            },
        ];
        assert_eq!(rehydrate_inline_notes(&stored, &notes), Some(format!(
            "<p><b>{marker}<i class=\"footnote\">ראשונה</i><i class=\"footnote\">שנייה</i>המשך</b></p>"
        )));
    }

    #[test]
    fn leading_tags_do_not_move_unanchored_bodies_or_cross_visible_prefixes() {
        let marker = "<sup class=\"footnote-marker\"></sup>";
        let stored = format!("<p><b>{marker}המשך</b></p>");
        assert_eq!(
            rehydrate_inline_notes(
                &stored,
                &[BookNote {
                    char_pos: 0,
                    has_marker: false,
                    body: "לפני".into(),
                }]
            ),
            Some(format!("<i class=\"footnote\">לפני</i>{stored}"))
        );
        for prefix in ["<p> ", "<p>&nbsp;", "<p>א", "<p"] {
            let stored = format!("{prefix}{marker}המשך</p>");
            assert_eq!(
                rehydrate_inline_notes(
                    &stored,
                    &[BookNote {
                        char_pos: 0,
                        has_marker: true,
                        body: "לפני".into(),
                    }]
                ),
                Some(format!("<i class=\"footnote\">לפני</i>{stored}")),
                "{prefix}"
            );
        }
    }

    #[test]
    fn an_unanchored_body_before_a_sup_stays_before_it() {
        let stored = "<sup>1</sup> סוף";
        let notes = [BookNote {
            char_pos: 0,
            has_marker: false,
            body: "לפני".into(),
        }];
        assert_eq!(
            rehydrate_inline_notes(stored, &notes),
            Some(format!("<i class=\"footnote\">לפני</i>{stored}"))
        );
    }
}
