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
pub(crate) struct BookNote {
    pub char_pos: i64,
    pub has_marker: bool,
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

/// The byte ends of the `<sup>` markers whose body was moved out: those not followed by one.
fn stripped_markers(html: &str) -> Vec<usize> {
    FOOTNOTE_MARKER
        .find_iter(html)
        .map(|m| m.end())
        .filter(|&end| !FOOTNOTE_BODY_START.is_match(&html[end..]))
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
/// right after it (they belong to the character before them, e.g. `</sup>`).
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
    while i < len && html[i..].starts_with("</") {
        match find(i, b'>') {
            Some(close) => i = close + 1,
            None => break,
        }
    }
    Insertion {
        at: i,
        splits_char: false,
    }
}

fn footnote_html(body: &str) -> String {
    format!("<i class=\"footnote\">{body}</i>")
}

/// `rehydrateInlineNotes`: each body at its `charPos`; a note without one after its marker
/// (the k-th stripped `<sup>` belongs to the k-th note with a marker), else at the end. A line
/// that already holds a footnote body is left alone. `None` when the line is unchanged.
pub(crate) fn rehydrate_inline_notes(line: &str, notes: &[BookNote]) -> Option<String> {
    if notes.is_empty() || FOOTNOTE_BODY.is_match(line) {
        return None;
    }
    let markers = stripped_markers(line);
    let mut anchored_seen = 0;
    let mut inserts: Vec<(Insertion, usize)> = Vec::with_capacity(notes.len());
    for (order, note) in notes.iter().enumerate() {
        let at = if note.char_pos >= 0 {
            raw_insertion_point(line, note.char_pos)
        } else if note.has_marker && anchored_seen < markers.len() {
            Insertion {
                at: markers[anchored_seen],
                splits_char: false,
            }
        } else {
            Insertion {
                at: line.len(),
                splits_char: false,
            }
        };
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
            vec!["<SUP class=\"footnote-marker\">א</SUP>".len()]
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
