use tantivy::tokenizer::{Token, TokenFilter, TokenStream, Tokenizer, BoxTokenStream};

/// A token filter that removes Hebrew characters 'ו' (vav) and 'י' (yod) 
/// from inside words (not at the beginning or end).
pub struct HebrewCharacterFilter;

impl TokenFilter for HebrewCharacterFilter {
    type Tokenizer<T: Tokenizer> = HebrewCharacterFilterTokenizer<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> Self::Tokenizer<T> {
        HebrewCharacterFilterTokenizer {
            inner: tokenizer,
        }
    }
}

#[derive(Clone)]
pub struct HebrewCharacterFilterTokenizer<T> {
    inner: T,
}

impl<T: Tokenizer> Tokenizer for HebrewCharacterFilterTokenizer<T> 
where
    T: Clone,
{
    type TokenStream<'a> = BoxTokenStream<'a> where Self: 'a;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        BoxTokenStream::new(HebrewCharacterFilterTokenStream {
            tail: BoxTokenStream::new(self.inner.token_stream(text)),
        })
    }
}

pub struct HebrewCharacterFilterTokenStream<'a> {
    tail: BoxTokenStream<'a>,
}

impl<'a> TokenStream for HebrewCharacterFilterTokenStream<'a> {
    fn advance(&mut self) -> bool {
        if !self.tail.advance() {
            return false;
        }

        let token = self.tail.token_mut();
        let original_text = token.text.clone();
        
        // Only process if the token has more than 2 characters
        if original_text.chars().count() > 2 {
            let chars: Vec<char> = original_text.chars().collect();
            let mut filtered_chars = Vec::new();
            
            for (i, ch) in chars.iter().enumerate() {
                // Keep the character if:
                // 1. It's at the beginning (i == 0)
                // 2. It's at the end (i == chars.len() - 1)
                // 3. It's not ו or ی
                if i == 0 || i == chars.len() - 1 || (*ch != 'ו' && *ch != 'י') {
                    filtered_chars.push(*ch);
                }
            }
            
            let filtered_text: String = filtered_chars.into_iter().collect();
            
            // Only update if the text actually changed and is not empty
            if filtered_text != original_text && !filtered_text.is_empty() {
                token.text = filtered_text;
            }
        }
        
        true
    }

    fn token(&self) -> &Token {
        self.tail.token()
    }

    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::tokenizer::{SimpleTokenizer, TextAnalyzer};

    #[test]
    fn test_hebrew_character_filter() {
        // Test case 1: Remove י from middle
        {
            let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(HebrewCharacterFilter)
                .build();
            let mut token_stream = tokenizer.token_stream("ביתי");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "בתי");
            assert!(!token_stream.advance());
        }

        // Test case 2: Remove ו from middle, preserve at beginning
        {
            let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(HebrewCharacterFilter)
                .build();
            let mut token_stream = tokenizer.token_stream("והילדים");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "והלדם");
            assert!(!token_stream.advance());
        }

        // Test case 3: Preserve characters at boundaries
        {
            let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(HebrewCharacterFilter)
                .build();
            let mut token_stream = tokenizer.token_stream("ילדי");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "ילדי");
            assert!(!token_stream.advance());
        }

        // Test case 4: Short words (2 chars or less) remain unchanged
        {
            let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(HebrewCharacterFilter)
                .build();
            let mut token_stream = tokenizer.token_stream("וי");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "וי");
            assert!(!token_stream.advance());
        }

        // Test case 5: Single character remains unchanged
        {
            let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(HebrewCharacterFilter)
                .build();
            let mut token_stream = tokenizer.token_stream("ו");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "ו");
            assert!(!token_stream.advance());
        }

        // Test case 6: Multiple words
        {
            let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(HebrewCharacterFilter)
                .build();
            let mut token_stream = tokenizer.token_stream("והילדים ביתי");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "והלדם");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "בתי");
            assert!(!token_stream.advance());
        }

        // Test case 7: Mixed Hebrew and English
        {
            let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(HebrewCharacterFilter)
                .build();
            let mut token_stream = tokenizer.token_stream("hello ביתי world");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "hello");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "בתי");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "world");
            assert!(!token_stream.advance());
        }
    }

    #[test]
    fn test_edge_cases() {
        // Test empty string
        {
            let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(HebrewCharacterFilter)
                .build();
            let mut token_stream = tokenizer.token_stream("");
            assert!(!token_stream.advance());
        }

        // Test only Hebrew characters to be filtered
        {
            let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(HebrewCharacterFilter)
                .build();
            let mut token_stream = tokenizer.token_stream("ויו");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "וו");
            assert!(!token_stream.advance());
        }

        // Test word that becomes empty after filtering (shouldn't happen with our logic)
        // This test ensures we don't create empty tokens
        {
            let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(HebrewCharacterFilter)
                .build();
            let mut token_stream = tokenizer.token_stream("יוי");
            assert!(token_stream.advance());
            assert_eq!(token_stream.token().text, "יי");
            assert!(!token_stream.advance());
        }
    }
}
