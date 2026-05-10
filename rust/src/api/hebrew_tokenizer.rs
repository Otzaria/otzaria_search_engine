use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// טוקנייזר עברי שמתנהג כמו SimpleTokenizer אך שומר גרש (') בסוף טוקן.
///
/// דוגמאות:
///   "תוס'"  → ["תוס'"]    (גרש בסוף — נשמר)
///   "ז\"ל"  → ["ז", "ל"]  (גרשיים באמצע — מפריד, בדיוק כמו SimpleTokenizer)
///   "תוס' ד\"ה" → ["תוס'", "ד", "ה"]
#[derive(Clone, Default)]
pub struct HebrewTokenizer;

pub struct HebrewTokenStream<'a> {
    text: &'a str,
    token: Token,
    byte_pos: usize,
    token_count: usize,
}

impl Tokenizer for HebrewTokenizer {
    type TokenStream<'a> = HebrewTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        HebrewTokenStream {
            text,
            token: Token::default(),
            byte_pos: 0,
            token_count: 0,
        }
    }
}

#[inline]
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

impl<'a> HebrewTokenStream<'a> {
    fn find_next_token(text: &str, start_byte: usize) -> Option<(&str, usize, usize)> {
        let slice = &text[start_byte..];

        // מצא את תחילת הטוקן
        let tok_start_rel = slice
            .char_indices()
            .find(|(_, c)| is_word_char(*c))
            .map(|(i, _)| i)?;

        let tok_start = start_byte + tok_start_rel;

        // מצא את סוף רצף תווי-המילה
        let word_slice = &text[tok_start..];
        let word_end_rel = word_slice
            .char_indices()
            .find(|(_, c)| !is_word_char(*c))
            .map(|(i, _)| i)
            .unwrap_or(word_slice.len());

        let word_end = tok_start + word_end_rel;

        // אם אחרי המילה יש גרש (ascii ' או עברי ׳) שאחריו אין תו-מילה — כלול אותו בטוקן
        let after_word = &text[word_end..];
        let tok_end = if after_word.starts_with('\'') || after_word.starts_with('\u{05F3}') {
            let geresh_len = after_word.chars().next().unwrap().len_utf8();
            let next_is_word = after_word[geresh_len..]
                .chars()
                .next()
                .map(is_word_char)
                .unwrap_or(false);
            if next_is_word {
                word_end // גרש באמצע — לא כולל
            } else {
                word_end + geresh_len // גרש בסוף — כולל
            }
        } else {
            word_end
        };

        Some((&text[tok_start..tok_end], tok_start, tok_end))
    }
}

impl<'a> TokenStream for HebrewTokenStream<'a> {
    fn advance(&mut self) -> bool {
        match Self::find_next_token(self.text, self.byte_pos) {
            None => false,
            Some((tok_text, tok_start, tok_end)) => {
                self.token.text.clear();
                self.token.text.push_str(tok_text);
                self.token.offset_from = tok_start;
                self.token.offset_to = tok_end;
                self.token.position = self.token_count;
                self.token_count += 1;
                self.byte_pos = tok_end;
                true
            }
        }
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(text: &str) -> Vec<String> {
        let mut tokenizer = HebrewTokenizer;
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }

    #[test]
    fn test_trailing_geresh_kept() {
        assert_eq!(tokenize("תוס'"), vec!["תוס'"]);
    }

    #[test]
    fn test_gershayim_splits() {
        assert_eq!(tokenize("ז\"ל"), vec!["ז", "ל"]);
    }

    #[test]
    fn test_rambam() {
        assert_eq!(tokenize("רמב\"ם"), vec!["רמב", "ם"]);
    }

    #[test]
    fn test_phrase_with_trailing_geresh() {
        assert_eq!(tokenize("תוס' ד\"ה"), vec!["תוס'", "ד", "ה"]);
    }

    #[test]
    fn test_geresh_in_middle_splits() {
        assert_eq!(tokenize("ד'אש"), vec!["ד", "אש"]);
    }

    #[test]
    fn test_hebrew_geresh_char() {
        assert_eq!(tokenize("תוס\u{05F3}"), vec!["תוס\u{05F3}"]);
    }

    #[test]
    fn test_plain_words() {
        assert_eq!(tokenize("שלום עולם"), vec!["שלום", "עולם"]);
    }

    #[test]
    fn test_standalone_word_no_geresh() {
        assert_eq!(tokenize("תוס"), vec!["תוס"]);
    }
}
