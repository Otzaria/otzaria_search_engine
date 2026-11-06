//! Footnote provenance kept beside, never inside, the text searched and embedded.
//!
//! All offsets are UTF-8 byte offsets into the exact normalized rendering. They are
//! derived from the original HTML, not by finding a note's words in a line: the same
//! words may occur in its body, in several notes, and in several snippet windows.

use crate::hebrew_query::{
    fold_presentation_form, for_each_html_stripped_piece_at, is_attached_mark,
};
use serde::{Deserialize, Serialize};
use std::ops::{Deref, Range};

pub(crate) const FIELD: &str = "noteSpans";
pub(crate) const NOTE_OPEN: &str = "<otzaria-note>";
pub(crate) const NOTE_CLOSE: &str = "</otzaria-note>";
pub(crate) const MARKER_OPEN: &str = "<otzaria-note-marker>";
pub(crate) const MARKER_CLOSE: &str = "</otzaria-note-marker>";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum Kind {
    Note,
    Marker,
}

impl Kind {
    fn open(self) -> &'static str {
        match self {
            Self::Note => NOTE_OPEN,
            Self::Marker => MARKER_OPEN,
        }
    }

    fn close(self) -> &'static str {
        match self {
            Self::Note => NOTE_CLOSE,
            Self::Marker => MARKER_CLOSE,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Span {
    start: usize,
    end: usize,
    kind: Kind,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rendering {
    crc: u32,
    spans: Vec<Span>,
}

impl Rendering {
    fn checked(self, text: &str) -> Vec<Span> {
        if self.spans.is_empty() {
            return Vec::new();
        }
        let mut end = 0;
        let valid = self.crc == crc32fast::hash(text.as_bytes())
            && self.spans.iter().all(|span| {
                let valid = end <= span.start
                    && span.start < span.end
                    && span.end <= text.len()
                    && text.is_char_boundary(span.start)
                    && text.is_char_boundary(span.end);
                end = span.end;
                valid
            });
        if valid {
            self.spans
        } else {
            Vec::new()
        }
    }
}

/// The optional stored field. The two renderings need different offsets when a line
/// carries attached marks. Invalid metadata degrades to the existing plain snippet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NoteSpans {
    plain: Rendering,
    vocalized: Option<Rendering>,
}

impl NoteSpans {
    pub(crate) fn is_empty(&self) -> bool {
        self.plain.spans.is_empty()
            && self
                .vocalized
                .as_ref()
                .is_none_or(|value| value.spans.is_empty())
    }

    pub(crate) fn from_html(raw: &str, plain: &str, vocalized: Option<&str>) -> Self {
        LineNotes(raw_note_ranges(raw)).normalize(raw, plain, vocalized)
    }

    pub(crate) fn from_renderings(
        raw_plain: &str,
        raw_vocalized: &str,
        plain: &str,
        vocalized: Option<&str>,
    ) -> Self {
        if std::ptr::eq(raw_plain, raw_vocalized) {
            return Self::from_html(raw_plain, plain, vocalized);
        }
        let plain_ranges = raw_note_ranges(raw_plain);
        let vocalized_ranges = raw_note_ranges(raw_vocalized);
        if plain_ranges.is_empty() && vocalized_ranges.is_empty() {
            return Self::default();
        }
        // An already-normalized copy may omit HTML. mapped still requires the whole
        // source rendering to equal the target before borrowing its provenance.
        let (plain_source, plain_source_ranges) = if plain_ranges.is_empty() {
            (raw_vocalized, &vocalized_ranges)
        } else {
            (raw_plain, &plain_ranges)
        };
        let (vocalized_source, vocalized_source_ranges) = if vocalized_ranges.is_empty() {
            (raw_plain, &plain_ranges)
        } else {
            (raw_vocalized, &vocalized_ranges)
        };
        Self {
            plain: mapped(plain_source, plain_source_ranges, plain, true),
            vocalized: vocalized
                .map(|text| mapped(vocalized_source, vocalized_source_ranges, text, false)),
        }
    }

    pub(crate) fn encode(&self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            // Only integers, enums and vectors: serialization cannot reject a value.
            Some(serde_json::to_string(self).expect("serializable note spans"))
        }
    }

    pub(crate) fn decode(value: Option<&str>) -> Self {
        value
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or_default()
    }

    pub(crate) fn display(self, text: String, vocalized: bool) -> DisplayText {
        let spans = if vocalized {
            self.vocalized.unwrap_or_default()
        } else {
            self.plain
        };
        DisplayText {
            spans: spans.checked(&text),
            text,
        }
    }
}

/// Raw offsets for one line, clipped from complete tags in the whole source.
/// A line's searchable text is still normalized independently, exactly as before.
#[derive(Default)]
pub(crate) struct LineNotes(Vec<(Range<usize>, Kind)>);

impl LineNotes {
    pub(crate) fn normalize(&self, raw: &str, plain: &str, vocalized: Option<&str>) -> NoteSpans {
        if self.0.is_empty() {
            return NoteSpans::default();
        }
        NoteSpans {
            plain: mapped(raw, &self.0, plain, true),
            vocalized: vocalized.map(|text| mapped(raw, &self.0, text, false)),
        }
    }
}

/// A first pass proves that each note is closed before any of its lines are
/// decorated. Only offsets are retained; a small spool keeps economy preparation
/// bounded even when a note crosses its normalization windows or the text spills.
pub(crate) struct BookNotes {
    parser: NoteParser,
    ranges: crate::indexing_buffers::TextBuffer,
}

impl BookNotes {
    pub(crate) fn new(directory: &std::path::Path) -> Self {
        Self {
            parser: NoteParser::default(),
            ranges: crate::indexing_buffers::TextBuffer::in_directory(64 << 10, Some(directory)),
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.parser.feed(bytes, |span| {
            self.ranges.push_str(&format!(
                "{} {} {}\n",
                span.start,
                span.end,
                u8::from(span.kind == Kind::Marker)
            ))
        })
    }

    pub(crate) fn push_text(&mut self, text: &str) -> anyhow::Result<()> {
        if has_footnote_hint(text) {
            self.push(text.as_bytes())?;
        }
        Ok(())
    }

    pub(crate) fn reader(&mut self) -> anyhow::Result<BookNoteReader<'_>> {
        // An unfinished tag/note is deliberately never emitted.
        self.parser = NoteParser::default();
        let mut reader = BookNoteReader {
            reader: self.ranges.reader()?,
            next: None,
            buffer: String::new(),
        };
        reader.advance()?;
        Ok(reader)
    }
}

pub(crate) struct BookNoteReader<'a> {
    reader: Box<dyn std::io::BufRead + 'a>,
    next: Option<Span>,
    buffer: String,
}

impl BookNoteReader<'_> {
    fn advance(&mut self) -> anyhow::Result<()> {
        self.buffer.clear();
        self.next = if self.reader.read_line(&mut self.buffer)? == 0 {
            None
        } else {
            let mut parts = self.buffer.split_ascii_whitespace();
            Some(Span {
                start: parts.next().unwrap_or_default().parse()?,
                end: parts.next().unwrap_or_default().parse()?,
                kind: match parts.next() {
                    Some("0") => Kind::Note,
                    Some("1") => Kind::Marker,
                    _ => anyhow::bail!("invalid note range spool"),
                },
            })
        };
        Ok(())
    }

    /// Lines must be requested in source order. A range crossing a line ending
    /// remains pending for the next line, including lines not stored in the index.
    pub(crate) fn line(&mut self, start: usize, len: usize) -> anyhow::Result<LineNotes> {
        let end = start + len;
        let mut ranges = Vec::new();
        while let Some(span) = &self.next {
            if span.start >= end {
                break;
            }
            if span.end > start {
                ranges.push((
                    span.start.max(start) - start..span.end.min(end) - start,
                    span.kind,
                ));
            }
            if span.end > end {
                break;
            }
            self.advance()?;
        }
        Ok(LineNotes(ranges))
    }
}

/// Text plus display provenance. Cropping and joining it carry offsets arithmetically;
/// no caller locates a fragment by substring search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DisplayText {
    pub(crate) text: String,
    pub(crate) spans: Vec<Span>,
}

impl Deref for DisplayText {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl DisplayText {
    #[cfg(any(test, feature = "semantic-integration"))]
    pub(crate) fn plain(text: String) -> Self {
        Self {
            text,
            spans: Vec::new(),
        }
    }

    #[cfg(feature = "semantic-integration")]
    pub(crate) fn annotation_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.spans.hash(&mut hash);
        hash.finish()
    }

    #[cfg(feature = "semantic-integration")]
    pub(crate) fn trimmed(&self) -> Self {
        let start = self.len() - self.trim_start().len();
        self.slice(start..self.trim_end().len().max(start))
    }

    pub(crate) fn slice(&self, range: Range<usize>) -> Self {
        let text = self.text[range.clone()].to_string();
        let spans = self
            .spans
            .iter()
            .filter_map(|span| {
                let start = span.start.max(range.start);
                let end = span.end.min(range.end);
                (start < end).then_some(Span {
                    start: start.saturating_sub(range.start),
                    end: end.saturating_sub(range.start),
                    kind: span.kind,
                })
            })
            .collect();
        Self { text, spans }
    }

    pub(crate) fn join(parts: &[&Self], separator: &str) -> Self {
        let mut out = Self::default();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                out.text.push_str(separator);
            }
            let offset = out.text.len();
            out.spans.extend(part.spans.iter().map(|span| Span {
                start: offset + span.start,
                end: offset + span.end,
                kind: span.kind,
            }));
            out.text.push_str(&part.text);
        }
        out
    }

    #[cfg(feature = "semantic-integration")]
    pub(crate) fn take_chars(&self, count: usize) -> Self {
        let end = self
            .char_indices()
            .nth(count)
            .map_or(self.len(), |(at, _)| at);
        self.slice(0..end)
    }

    #[cfg(feature = "semantic-integration")]
    pub(crate) fn collapse_whitespace(&self) -> Self {
        let mut out = Collector::default();
        let mut at = 0;
        for (offset, c) in self.char_indices() {
            while self.spans.get(at).is_some_and(|span| span.end <= offset) {
                at += 1;
            }
            let note = self
                .spans
                .get(at)
                .filter(|span| span.start <= offset)
                .map(|span| (at, span.kind));
            out.push(c, note);
        }
        out.finish()
    }

    /// Paints a source fragment at its known offset; the caller adds any synthetic
    /// ellipses afterward. Highlight tags are closed/reopened at note edges.
    pub(crate) fn annotate(
        &self,
        html: String,
        offset: usize,
        prefix: &str,
        postfix: &str,
    ) -> String {
        if self.spans.is_empty() {
            return html;
        }
        annotate(&html, &self.text, offset, &self.spans, prefix, postfix).unwrap_or(html)
    }
}

#[derive(Default)]
struct Collector {
    text: String,
    spans: Vec<Span>,
    last_note: Option<(usize, Kind)>,
    pending_space: bool,
}

impl Collector {
    fn push(&mut self, c: char, note: Option<(usize, Kind)>) {
        if c.is_whitespace() {
            self.pending_space |= !self.text.is_empty();
            return;
        }
        if self.pending_space {
            let between = if self.last_note == note { note } else { None };
            self.emit(' ', between);
            self.pending_space = false;
        }
        self.emit(c, note);
    }

    fn emit(&mut self, c: char, note: Option<(usize, Kind)>) {
        let start = self.text.len();
        self.text.push(c);
        if let Some((_, kind)) = note {
            if self.last_note == note && self.spans.last().is_some_and(|span| span.end == start) {
                self.spans.last_mut().expect("checked span").end = self.text.len();
            } else {
                self.spans.push(Span {
                    start,
                    end: self.text.len(),
                    kind,
                });
            }
        }
        self.last_note = note;
    }

    fn finish(self) -> DisplayText {
        DisplayText {
            text: self.text,
            spans: self.spans,
        }
    }
}

fn mapped(
    raw: &str,
    ranges: &[(Range<usize>, Kind)],
    expected: &str,
    strip_marks: bool,
) -> Rendering {
    let crc = crc32fast::hash(expected.as_bytes());
    if ranges.is_empty() {
        return Rendering {
            crc,
            spans: Vec::new(),
        };
    }
    let mut out = Collector::default();
    let mut next = 0;
    for_each_html_stripped_piece_at(raw, |range, piece| {
        let original = raw.get(range.clone()) == Some(piece);
        for (index, c) in piece.char_indices() {
            let offset = range.start + if original { index } else { 0 };
            while ranges
                .get(next)
                .is_some_and(|(range, _)| range.end <= offset)
            {
                next += 1;
            }
            let note = ranges
                .get(next)
                .filter(|(range, _)| range.start <= offset)
                .map(|(_, kind)| (next, *kind));
            let mut emit = |c| {
                if !(strip_marks && is_attached_mark(c)) {
                    out.push(c, note);
                }
            };
            match fold_presentation_form(c) {
                Some(folded) => folded.chars().for_each(emit),
                None => emit(c),
            }
        }
    });
    let out = out.finish();
    Rendering {
        crc,
        spans: if out.text == expected {
            out.spans
        } else {
            Vec::new()
        },
    }
}

/// Only complete, explicitly classified tags count. Nested ordinary formatting is
/// transparent. A body inside a sup belongs to the marker, not to a second note.
fn has_footnote_hint(raw: &str) -> bool {
    raw.as_bytes()
        .windows(8)
        .any(|word| word.eq_ignore_ascii_case(b"footnote"))
}

fn raw_note_ranges(raw: &str) -> Vec<(Range<usize>, Kind)> {
    if !has_footnote_hint(raw) {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    NoteParser::default()
        .feed(raw.as_bytes(), |span| {
            ranges.push((span.start..span.end, span.kind));
            Ok(())
        })
        .expect("collecting offsets cannot fail");
    ranges
}

struct OpenNoteTag {
    name: String,
    start: usize,
    kind: Option<Kind>,
}

#[derive(Default)]
struct NoteParser {
    stack: Vec<OpenNoteTag>,
    offset: usize,
    pending: Vec<u8>,
    pending_start: usize,
    skipping_tag: bool,
    disabled: bool,
}

impl NoteParser {
    // Display metadata must not make malformed HTML consume unbounded memory.
    const MAX_DEPTH: usize = 256;
    const MAX_TAG_BYTES: usize = 64 << 10;
    const MAX_NAME_BYTES: usize = 128;

    fn feed(
        &mut self,
        bytes: &[u8],
        mut emit: impl FnMut(Span) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if self.disabled {
            return Ok(());
        }
        let mut at = 0;
        while at < bytes.len() {
            if self.skipping_tag {
                let Some(end) = bytes[at..].iter().position(|&byte| byte == b'>') else {
                    break;
                };
                at += end + 1;
                self.skipping_tag = false;
                continue;
            }
            let start = if self.pending.is_empty() {
                let Some(next) = bytes[at..].iter().position(|&byte| byte == b'<') else {
                    break;
                };
                at + next
            } else {
                at
            };
            let Some(end) = bytes[start..].iter().position(|&byte| byte == b'>') else {
                if self.pending.is_empty() {
                    self.pending_start = self.offset + start;
                }
                if self.pending.len() + bytes.len() - start > Self::MAX_TAG_BYTES {
                    if oversized_void_tag(&self.pending, &bytes[start..]) {
                        self.pending.clear();
                        self.skipping_tag = true;
                        break;
                    }
                    self.disable();
                    return Ok(());
                }
                self.pending.extend_from_slice(&bytes[start..]);
                break;
            };
            let end = start + end;
            let span = if self.pending.is_empty() {
                if end - start > Self::MAX_TAG_BYTES {
                    if oversized_void_tag(&[], &bytes[start..end]) {
                        at = end + 1;
                        continue;
                    }
                    self.disable();
                    return Ok(());
                }
                self.tag(
                    &bytes[start + 1..end],
                    self.offset + start,
                    self.offset + end + 1,
                )
            } else {
                if self.pending.len() + end - start > Self::MAX_TAG_BYTES {
                    if oversized_void_tag(&self.pending, &bytes[start..end]) {
                        self.pending.clear();
                        at = end + 1;
                        continue;
                    }
                    self.disable();
                    return Ok(());
                }
                let mut pending = std::mem::take(&mut self.pending);
                pending.extend_from_slice(&bytes[start..end]);
                self.tag(&pending[1..], self.pending_start, self.offset + end + 1)
            };
            if let Some(span) = span {
                emit(span)?;
            }
            if self.disabled {
                return Ok(());
            }
            at = end + 1;
        }
        self.offset += bytes.len();
        Ok(())
    }

    fn disable(&mut self) {
        self.disabled = true;
        self.stack.clear();
        self.pending.clear();
        self.skipping_tag = false;
    }

    fn tag(&mut self, bytes: &[u8], start: usize, end: usize) -> Option<Span> {
        let body = std::str::from_utf8(bytes).ok()?;
        let closing = body.starts_with('/');
        let body = body.strip_prefix('/').unwrap_or(body);
        let name_len = body.bytes().take_while(u8::is_ascii_alphanumeric).count();
        if name_len == 0 {
            return None;
        }
        if name_len > Self::MAX_NAME_BYTES {
            self.disable();
            return None;
        }
        let name = &body[..name_len];
        if closing {
            let mut span = None;
            if let Some(index) = self
                .stack
                .iter()
                .rposition(|open| open.name.eq_ignore_ascii_case(name))
            {
                let open = &self.stack[index];
                if let Some(kind) = open.kind {
                    if open.start < start {
                        span = Some(Span {
                            start: open.start,
                            end: start,
                            kind,
                        });
                    }
                }
                self.stack.truncate(index);
            } else {
                // A closing ancestor without an opening inside the tracked
                // note invalidates that note rather than guessing its layout.
                self.stack.clear();
            }
            return span;
        }
        if body.trim_end().ends_with('/') || is_void_tag(name) {
            return None;
        }
        if self.stack.len() == Self::MAX_DEPTH {
            self.disable();
            return None;
        }
        let inside = self.stack.iter().any(|open| open.kind.is_some());
        let in_sup = self
            .stack
            .iter()
            .any(|open| open.name.eq_ignore_ascii_case("sup"));
        let kind = if inside {
            None
        } else if name.eq_ignore_ascii_case("sup")
            && has_class(&body[name_len..], "footnote-marker")
        {
            Some(Kind::Marker)
        } else if !in_sup
            && !name.eq_ignore_ascii_case("sup")
            && has_class(&body[name_len..], "footnote")
        {
            Some(Kind::Note)
        } else {
            None
        };
        // Ancestors outside a note/sup cannot affect classification of valid
        // HTML. Do not retain unclosed heading tags from every book line.
        if kind.is_none() && self.stack.is_empty() && !name.eq_ignore_ascii_case("sup") {
            return None;
        }
        self.stack.push(OpenNoteTag {
            name: name.to_string(),
            start: end,
            kind,
        });
        None
    }
}

fn is_void_tag(name: &str) -> bool {
    [
        "br", "hr", "img", "input", "meta", "link", "wbr", "area", "base", "embed", "param",
        "source", "track", "col",
    ]
    .iter()
    .any(|void| name.eq_ignore_ascii_case(void))
}

fn oversized_void_tag(pending: &[u8], incoming: &[u8]) -> bool {
    // Inline data: images routinely exceed the tag buffer. Their attributes
    // cannot introduce a note, so skip them without dropping later provenance.
    let mut head = [0; NoteParser::MAX_NAME_BYTES + 1];
    let mut len = 0;
    for &byte in pending.iter().chain(incoming).skip(1).take(head.len()) {
        head[len] = byte;
        len += 1;
    }
    let name_len = head[..len]
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric())
        .count();
    name_len < len && std::str::from_utf8(&head[..name_len]).is_ok_and(is_void_tag)
}

fn has_class(mut attributes: &str, class: &str) -> bool {
    while !attributes.is_empty() {
        attributes = attributes.trim_start();
        let end = attributes
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(attributes.len());
        let name = &attributes[..end];
        attributes = attributes[end..].trim_start();
        let Some(rest) = attributes.strip_prefix('=') else {
            if end == 0 {
                return false;
            }
            continue;
        };
        attributes = rest.trim_start();
        let (value, rest) = if let Some(quote @ ('\'' | '"')) = attributes.chars().next() {
            let Some(end) = attributes[1..].find(quote).map(|end| end + 1) else {
                return false;
            };
            (&attributes[1..end], &attributes[end + 1..])
        } else {
            let end = attributes
                .find(char::is_whitespace)
                .unwrap_or(attributes.len());
            (&attributes[..end], &attributes[end..])
        };
        if name.eq_ignore_ascii_case("class") {
            return value
                .split_ascii_whitespace()
                .any(|word| word.eq_ignore_ascii_case(class));
        }
        attributes = rest;
    }
    false
}

enum Piece {
    Text(String),
    Open,
    Close,
}

fn pieces(mut html: &str, prefix: &str, postfix: &str) -> Option<Vec<Piece>> {
    let mut pieces = Vec::new();
    while !html.is_empty() {
        let next = [(prefix, true), (postfix, false)]
            .into_iter()
            .filter(|(tag, _)| !tag.is_empty())
            .filter_map(|(tag, open)| html.find(tag).map(|at| (at, tag.len(), open)))
            .min_by_key(|(at, _, _)| *at);
        let (at, len, open) = next.unwrap_or((html.len(), 0, false));
        if at > 0 {
            pieces.push(Piece::Text(htmlescape::decode_html(&html[..at]).ok()?));
        }
        if len > 0 {
            pieces.push(if open { Piece::Open } else { Piece::Close });
        }
        html = &html[at + len..];
    }
    Some(pieces)
}

fn annotate(
    html: &str,
    text: &str,
    offset: usize,
    spans: &[Span],
    prefix: &str,
    postfix: &str,
) -> Option<String> {
    let pieces = pieces(html, prefix, postfix)?;
    let shown: String = pieces
        .iter()
        .filter_map(|piece| match piece {
            Piece::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let rest = text.get(offset..)?;
    if shown.is_empty() || !rest.starts_with(&shown) {
        return None;
    }
    let mut out = String::with_capacity(html.len() + spans.len() * 32);
    let mut position = offset;
    let mut next = spans.partition_point(|span| span.end <= position);
    let mut active: Option<usize> = None;
    let mut highlighted = false;
    let mut printed_highlight = false;
    for piece in pieces {
        match piece {
            Piece::Open => {
                if highlighted {
                    return None;
                }
                highlighted = true;
            }
            Piece::Close => {
                if !highlighted {
                    return None;
                }
                highlighted = false;
            }
            Piece::Text(piece) => {
                for c in piece.chars() {
                    while spans.get(next).is_some_and(|span| span.end <= position) {
                        next += 1;
                    }
                    let note = spans
                        .get(next)
                        .filter(|span| span.start <= position)
                        .map(|_| next);
                    if note != active || highlighted != printed_highlight {
                        if printed_highlight {
                            out.push_str(postfix);
                            printed_highlight = false;
                        }
                        if note != active {
                            if let Some(index) = active {
                                out.push_str(spans[index].kind.close());
                            }
                            if let Some(index) = note {
                                out.push_str(spans[index].kind.open());
                            }
                            active = note;
                        }
                        if highlighted {
                            out.push_str(prefix);
                            printed_highlight = true;
                        }
                    }
                    out.push_str(&htmlescape::encode_minimal(c.encode_utf8(&mut [0; 4])));
                    position += c.len_utf8();
                }
            }
        }
    }
    if highlighted {
        return None;
    }
    if printed_highlight {
        out.push_str(postfix);
    }
    if let Some(index) = active {
        out.push_str(spans[index].kind.close());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hebrew_query::normalize_line_for_indexing;

    fn display(raw: &str, vocalized: bool) -> DisplayText {
        let (plain, vocal) = normalize_line_for_indexing(raw);
        let notes = NoteSpans::from_html(raw, &plain, vocal.as_deref());
        notes.display(
            if vocalized {
                vocal.unwrap_or(plain)
            } else {
                plain
            },
            vocalized,
        )
    }

    fn shown(raw: &str) -> String {
        let text = display(raw, false);
        text.annotate(htmlescape::encode_minimal(&text), 0, "<mark>", "</mark>")
    }

    #[test]
    fn only_explicit_complete_notes_and_markers_are_annotated() {
        assert_eq!(shown("גוף<sup class='x footnote-marker y'>12</sup><i class=footnote>הערה <b>מפורשת</b></i> סוף"),
            "גוף <otzaria-note-marker>12</otzaria-note-marker> <otzaria-note>הערה מפורשת</otzaria-note> סוף");
        for ordinary in [
            "<i>מודגש</i>",
            "<i class=footnotes>מודגש</i>",
            "<sup>12</sup>",
            "<i data-class='footnote'>מודגש</i>",
            "<i class=footnote>ללא סגירה",
        ] {
            assert!(!shown(ordinary).contains("otzaria-"), "{ordinary}");
        }
        assert_eq!(
            shown("<SUP CLASS=\"footnote-marker\">א<i class='footnote'>ב</i></SUP> סוף"),
            "<otzaria-note-marker>אב</otzaria-note-marker> סוף"
        );
        assert_eq!(shown("<sup>א<i class=footnote>ב</i></sup> סוף"), "אב סוף");
    }

    #[test]
    fn identical_words_in_body_and_notes_keep_their_own_offsets() {
        assert_eq!(
            shown("מילה <i class=footnote>מילה</i> מילה <i class=footnote>מילה</i>"),
            "מילה <otzaria-note>מילה</otzaria-note> מילה <otzaria-note>מילה</otzaria-note>"
        );
        let text = display("מילה <i class=footnote>מילה</i> מילה", false);
        let offset = "מילה ".len();
        assert_eq!(
            text.annotate("<mark>מילה</mark> מילה".into(), offset, "<mark>", "</mark>"),
            "<otzaria-note><mark>מילה</mark></otzaria-note> מילה"
        );
        assert_eq!(
            text.annotate("<mark>מילה</mark>".into(), 0, "<mark>", "</mark>"),
            "<mark>מילה</mark>"
        );
    }

    #[test]
    fn book_note_offsets_survive_every_byte_chunk_boundary_and_line_cut() {
        let raw = "<h1>כותרת\nגוף<sup class='footnote-marker'>א\nב</sup><i class=footnote>אָב &amp; 😀\n\n<b>שני</b> שלישי</i> סוף\n<i class=footnote>לא נסגר";
        for chunk in 1..=raw.len() {
            let dir = tempfile::tempdir().unwrap();
            let mut notes = BookNotes::new(dir.path());
            for piece in raw.as_bytes().chunks(chunk) {
                notes.push(piece).unwrap();
            }
            let mut notes = notes.reader().unwrap();
            let mut at = 0;
            let shown: Vec<_> = raw
                .split('\n')
                .map(|line| {
                    let (plain, vocalized) = normalize_line_for_indexing(line);
                    let layout = notes.line(at, line.len()).unwrap();
                    at += line.len() + 1;
                    let display = layout
                        .normalize(line, &plain, vocalized.as_deref())
                        .display(plain, false);
                    display.annotate(htmlescape::encode_minimal(&display), 0, "", "")
                })
                .collect();
            assert_eq!(shown, [
                "כותרת",
                "גוף <otzaria-note-marker>א</otzaria-note-marker>",
                "<otzaria-note-marker>ב</otzaria-note-marker> <otzaria-note>אב 😀</otzaria-note>",
                "",
                "<otzaria-note>שני שלישי</otzaria-note> סוף",
                "לא נסגר",
            ], "chunk size {chunk}");
        }
    }

    #[test]
    fn book_notes_spill_offsets_without_losing_complete_notes() {
        let dir = tempfile::tempdir().unwrap();
        let mut notes = BookNotes::new(dir.path());
        let line = "<i class=footnote>הערה</i>\n";
        for _ in 0..10_000 {
            notes.push(line.as_bytes()).unwrap();
        }
        assert!(notes.ranges.spilled_bytes() > 0);
        assert!(notes.ranges.buffered_bytes() <= 64 << 10);
        let mut reader = notes.reader().unwrap();
        for at in 0..10_000 {
            let layout = reader.line(at * line.len(), line.len() - 1).unwrap();
            let text = layout
                .normalize(line.trim_end(), "הערה", None)
                .display("הערה".into(), false);
            assert_eq!(
                text.annotate("הערה".into(), 0, "", ""),
                "<otzaria-note>הערה</otzaria-note>"
            );
        }
    }

    #[test]
    fn malformed_source_cannot_grow_the_note_parser_without_bound() {
        for raw in [
            format!(
                "<i class=footnote>{}הערה</i>",
                "<b>".repeat(NoteParser::MAX_DEPTH)
            ),
            format!(
                "<i class=footnote data-x='{}'>הערה</i>",
                "x".repeat(NoteParser::MAX_TAG_BYTES)
            ),
        ] {
            let mut parser = NoteParser::default();
            for piece in raw.as_bytes().chunks(7) {
                parser
                    .feed(piece, |_| panic!("invalid note must not be emitted"))
                    .unwrap();
            }
            assert!(parser.disabled);
            assert!(parser.stack.is_empty());
            assert!(parser.pending.is_empty());
        }
        let raw = format!("{}<i class=footnote>הערה</i>", "<h1>כותרת\n".repeat(1_000));
        assert!(shown(&raw).contains("<otzaria-note>הערה</otzaria-note>"));
    }

    #[test]
    fn large_inline_images_do_not_discard_the_current_or_following_note() {
        let raw = format!("<i class=footnote>לפני<img src='data:image/png;base64,{}'>אחרי</i> <i class=footnote>נוספת</i>", "x".repeat(NoteParser::MAX_TAG_BYTES * 2));
        let dir = tempfile::tempdir().unwrap();
        for chunk in [1, 7, NoteParser::MAX_TAG_BYTES - 1, raw.len()] {
            let mut notes = BookNotes::new(dir.path());
            for piece in raw.as_bytes().chunks(chunk) {
                notes.push(piece).unwrap();
            }
            assert!(!notes.parser.disabled, "chunk size {chunk}");
            let layout = notes.reader().unwrap().line(0, raw.len()).unwrap();
            let (plain, vocalized) = normalize_line_for_indexing(&raw);
            let text = layout
                .normalize(&raw, &plain, vocalized.as_deref())
                .display(plain, false);
            assert_eq!(
                text.annotate(htmlescape::encode_minimal(&text), 0, "", ""),
                "<otzaria-note>לפניאחרי</otzaria-note> <otzaria-note>נוספת</otzaria-note>"
            );
        }
    }

    #[test]
    fn highlights_crossing_a_note_are_split_into_validly_nested_tags() {
        let text = display("גוף <i class=footnote>הערה</i> סוף", false);
        assert_eq!(
            text.annotate("<mark>גוף הערה סוף</mark>".into(), 0, "<mark>", "</mark>"),
            "<mark>גוף </mark><otzaria-note><mark>הערה</mark></otzaria-note><mark> סוף</mark>"
        );
    }

    #[test]
    fn unicode_entities_and_whitespace_never_change_normalized_text() {
        for raw in [
            "<i class=footnote>אָבָה \u{FB4F} 😀 &nbsp; שני &amp; שלישי</i>",
            "א<i class=footnote>ב</i><i class=footnote>ג</i>ד",
            "מְ<sup>נַ</sup>שֶּׁה <i class=footnote>שָׁלוֹם</i>",
            "<p><i class=footnote> א <br> ב </i></p>ג",
            "<i class=footnote>א<b>ב<i>ג</i>ד</b>ה</i>",
        ] {
            let (plain, vocalized) = normalize_line_for_indexing(raw);
            let notes = NoteSpans::from_html(raw, &plain, vocalized.as_deref());
            assert!(!notes.plain.spans.is_empty(), "{raw}");
            assert_eq!(notes.clone().display(plain.clone(), false).text, plain);
            if let Some(vocalized) = vocalized {
                let display = notes.display(vocalized.clone(), true);
                assert!(!display.spans.is_empty(), "{raw}");
                assert_eq!(display.text, vocalized);
            }
        }
    }

    #[test]
    fn stale_malformed_and_non_boundary_offsets_degrade_to_plain_text() {
        let raw = "גוף <i class=footnote>הערה</i>";
        let (plain, _) = normalize_line_for_indexing(raw);
        let notes = NoteSpans::from_html(raw, &plain, None);
        let encoded = notes.encode().unwrap();
        assert_eq!(NoteSpans::decode(Some(&encoded)), notes);
        assert!(NoteSpans::decode(Some("{broken"))
            .display(plain.clone(), false)
            .spans
            .is_empty());
        assert!(notes
            .clone()
            .display("שורה אחרת".into(), false)
            .spans
            .is_empty());
        let mut broken = notes;
        broken.plain.spans[0].start += 1;
        assert!(broken.display(plain, false).spans.is_empty());
        assert!(NoteSpans::from_html(raw, "שונה", None).encode().is_none());
    }

    #[test]
    fn cropped_notes_use_source_offsets_and_reject_mismatched_text() {
        let text = display("א <i class=footnote>ראשון שני שלישי</i> ד", false);
        let offset = "א ראשון ".len();
        assert_eq!(
            text.annotate("<mark>שני</mark>".into(), offset, "<mark>", "</mark>"),
            "<otzaria-note><mark>שני</mark></otzaria-note>"
        );
        assert_eq!(
            text.annotate("שונה".into(), offset, "<mark>", "</mark>"),
            "שונה"
        );
    }

    #[test]
    fn adjacent_notes_stay_separate_and_a_slice_keeps_only_its_overlap() {
        let text = display(
            "א<i class=footnote>ראשון</i><i class=footnote>שני</i>ב",
            false,
        );
        assert_eq!(text.spans.len(), 2);
        let end = "אראשון".len();
        let left = text.slice(0..end);
        let right = text.slice(end..text.len());
        assert_eq!(DisplayText::join(&[&left, &right], ""), text);
        assert_eq!(
            right.annotate(htmlescape::encode_minimal(&right), 0, "", ""),
            "<otzaria-note>שני</otzaria-note>ב"
        );
    }

    #[cfg(feature = "semantic-integration")]
    #[test]
    fn semantic_window_collapse_and_crop_preserve_annotation_offsets() {
        let first = display("<i class=footnote>הערה אחת</i>", false);
        let empty = DisplayText::plain(String::new());
        let last = display("גוף <i class=footnote>הערה שתיים</i>", false);
        let joined = DisplayText::join(&[&first, &empty, &last], " ")
            .take_chars(24)
            .collapse_whitespace();
        assert_eq!(joined.text, "הערה אחת גוף הערה שתיים");
        assert_eq!(
            joined.annotate(htmlescape::encode_minimal(&joined), 0, "", ""),
            "<otzaria-note>הערה אחת</otzaria-note> גוף <otzaria-note>הערה שתיים</otzaria-note>"
        );
    }
}
