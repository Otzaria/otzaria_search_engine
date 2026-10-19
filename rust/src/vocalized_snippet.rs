//! Snippets shown with the line's nikud and cantillation, for searches that ran on the
//! mark-free text. See [`crate::api::search_engine::set_vocalized_snippets`].

use crate::hebrew_query::is_attached_mark;

#[cfg(not(test))]
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(not(test))]
pub(crate) fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(test))]
pub(crate) fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

// Per thread in tests, so a test that turns it on leaves parallel tests' snippets alone.
#[cfg(test)]
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn set_enabled(enabled: bool) {
    ENABLED.with(|cell| cell.set(enabled));
}

#[cfg(test)]
pub(crate) fn enabled() -> bool {
    ENABLED.with(|cell| cell.get())
}

/// `html` — the snippet of `plain` that starts at byte `offset`: HTML-escaped text between
/// `prefix`/`postfix` highlight tags — with each letter carrying the marks it has in
/// `vocalized`. `None` when `vocalized` is not `plain` with attached marks added.
pub(crate) fn vocalize_snippet_html(
    html: &str,
    plain: &str,
    offset: usize,
    vocalized: &str,
    prefix: &str,
    postfix: &str,
) -> Option<String> {
    let mut pieces = split_tags(html, prefix, postfix)?;
    // A bounded snippet ends in an ellipsis the line does not have.
    let ellipsis = match pieces.last_mut() {
        Some(Piece::Text(text)) if text.ends_with('…') => {
            text.pop();
            true
        }
        _ => false,
    };
    let shown: String = pieces
        .iter()
        .filter_map(|piece| match piece {
            Piece::Text(text) => Some(text.as_str()),
            Piece::Tag(_) => None,
        })
        .collect();
    if shown.is_empty() || !plain.get(offset..)?.starts_with(&shown) {
        return None;
    }

    let mut aligned = Aligned {
        plain: plain.chars(),
        vocalized,
        at: 0,
    };
    aligned.take(plain[..offset].chars().count())?;
    let mut out = String::with_capacity(html.len() * 2);
    for piece in &pieces {
        match piece {
            Piece::Tag(tag) => out.push_str(tag),
            Piece::Text(text) => {
                let marked = aligned.take(text.chars().count())?;
                out.push_str(&htmlescape::encode_minimal(marked));
            }
        }
    }
    if ellipsis {
        out.push('…');
    }
    Some(out)
}

/// Walks `plain` and `vocalized` together, as far as the snippet reaches.
struct Aligned<'a> {
    plain: std::str::Chars<'a>,
    vocalized: &'a str,
    at: usize,
}

impl<'a> Aligned<'a> {
    /// The next `count` chars of `plain` as they read in `vocalized`, with the marks after them.
    fn take(&mut self, count: usize) -> Option<&'a str> {
        let start = self.at;
        for _ in 0..count {
            let c = self.plain.next()?;
            let mut rest = self.vocalized[self.at..].chars();
            if rest.next()? != c {
                return None;
            }
            self.at += c.len_utf8();
            for mark in rest.take_while(|&m| is_attached_mark(m)) {
                self.at += mark.len_utf8();
            }
        }
        Some(&self.vocalized[start..self.at])
    }
}

enum Piece<'a> {
    Tag(&'a str),
    /// Unescaped text.
    Text(String),
}

fn split_tags<'a>(html: &'a str, prefix: &str, postfix: &str) -> Option<Vec<Piece<'a>>> {
    let mut pieces = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        let (at, len) = [prefix, postfix]
            .into_iter()
            .filter(|tag| !tag.is_empty())
            .filter_map(|tag| rest.find(tag).map(|at| (at, tag.len())))
            .min()
            .unwrap_or((rest.len(), 0));
        if at > 0 {
            pieces.push(Piece::Text(htmlescape::decode_html(&rest[..at]).ok()?));
        }
        if len > 0 {
            pieces.push(Piece::Tag(&rest[at..at + len]));
        }
        rest = &rest[at + len..];
    }
    Some(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRE: &str = "<font color=red>";
    const POST: &str = "</font>";

    #[test]
    fn highlighted_word_gets_its_marks() {
        let plain = "בראשית ברא אלהים";
        let vocalized = "בְּרֵאשִׁ֖ית בָּרָ֣א אֱלֹהִ֑ים";
        let html = "בראשית <font color=red>ברא</font> אלהים";
        assert_eq!(
            vocalize_snippet_html(html, plain, 0, vocalized, PRE, POST).as_deref(),
            Some("בְּרֵאשִׁ֖ית <font color=red>בָּרָ֣א</font> אֱלֹהִ֑ים")
        );
    }

    #[test]
    fn fragment_is_placed_by_its_offset_not_by_search() {
        let plain = "ויברא אלהים ברא אלהים";
        let vocalized = "וַיִּבְרָא אֱלֹהִים בָּרָא אֱלֹהִים";
        let html = "<font color=red>ברא</font> אלהים";
        let offset = plain.rfind("ברא אלהים").unwrap();
        assert_eq!(
            vocalize_snippet_html(html, plain, offset, vocalized, PRE, POST).as_deref(),
            Some("<font color=red>בָּרָא</font> אֱלֹהִים")
        );
    }

    #[test]
    fn escaped_text_and_ellipsis_are_kept() {
        let html = "א &amp; <font color=red>ב</font>…";
        assert_eq!(
            vocalize_snippet_html(html, "א & ב ג", 0, "אָ & בַּ גִ", PRE, POST).as_deref(),
            Some("אָ &amp; <font color=red>בַּ</font>…")
        );
    }

    #[test]
    fn mismatched_renderings_fall_back() {
        assert_eq!(
            vocalize_snippet_html("ברא", "ברא", 0, "בָּרָה", PRE, POST),
            None
        );
        assert_eq!(
            vocalize_snippet_html("ברא", "ברא", 0, "ָבָּרָא", PRE, POST),
            None
        );
        // The snippet must sit at its offset.
        assert_eq!(
            vocalize_snippet_html("ברא", "ויברא", 0, "וַיִּבְרָא", PRE, POST),
            None
        );
        assert_eq!(
            vocalize_snippet_html("ברא", "ברא", 9, "בָּרָא", PRE, POST),
            None
        );
    }
}
