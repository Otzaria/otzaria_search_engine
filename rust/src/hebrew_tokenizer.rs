use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

use crate::hebrew_query::{fold_presentation_form, is_attached_mark};

/// טוקנייזר עברי שמתנהג כמו SimpleTokenizer עם שתי תוספות:
/// גרש (' או ׳) בסוף טוקן נשמר, ותווים "שקופים" ([`TRANSPARENT`]) אינם
/// שוברים טוקן אך מושמטים מטקסטו — כך שטקסט מאונדקס שמשמר פיסוק לתצוגה
/// ממופה לאותם טרמים כמו שאילתה שעברה `sanitize_query`.
///
/// דוגמאות:
///   "תוס'"  → ["תוס'"]    (גרש בסוף — נשמר)
///   "ז\"ל"  → ["ז", "ל"]  (גרשיים באמצע — מפריד, בדיוק כמו SimpleTokenizer)
///   "א.ב"   → ["אב"]      (נקודה שקופה — לא שוברת ולא נכללת)
///   "עא:"   → ["עא"]      (פיסוק בסוף — מחוץ לטוקן)
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

/// תווים "שקופים": הסט ש-`sanitize_query` מוחק בצד השאילתה. אינם שוברים
/// טוקן ואינם נכללים בטקסטו, אך נשארים בטקסט השמור (שהוא טקסט התצוגה).
/// `|` אינו כאן — הוא מפריד, כמו מקף וסוף-פסוק.
const TRANSPARENT: &[char] = &[
    ',', ';', '!', '?', ':', '*', '(', ')', '[', ']', '{', '}', '^', '$', '\\', '+', '.', '~', '`',
];

#[inline]
fn is_transparent(c: char) -> bool {
    TRANSPARENT.contains(&c)
}

#[inline]
fn is_geresh(c: char) -> bool {
    c == '\'' || c == '\u{05F3}'
}

/// תו שממשיך טוקן: אות/ספרה, או סימן ניקוד/טעם צמוד. הטקסט המנורמל
/// לאינדוקס נקי מניקוד ממילא; קבלת הסימנים כאן היא הגנת-עומק כדי שקלט
/// שלא נורמל (למשל דרך `add_document` ישיר) לא יתפרק באמצע מילה. הניקוד
/// הוא Other_Alphabetic ולכן נתפס גם ב-`is_alphanumeric`; התוספת המפורשת
/// נדרשת בעיקר לטעמי המקרא (Mn בלבד), שאחרת היו מפצלים "שָׁמַ֣ע".
#[inline]
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_attached_mark(c)
}

/// תו שדורש בנייה איטית של טקסט הטוקן (סינון/קיפול) במקום העתקה ישירה.
#[inline]
fn needs_normalising(c: char) -> bool {
    is_attached_mark(c)
        || is_transparent(c)
        || c == '\u{05F3}'
        || fold_presentation_form(c).is_some()
}

impl<'a> HebrewTokenStream<'a> {
    fn find_next_token(text: &str, start_byte: usize) -> Option<(&str, usize, usize)> {
        let slice = &text[start_byte..];

        // מצא את תחילת הטוקן — חייב להתחיל באות/ספרה של ממש. שימו לב:
        // `is_alphanumeric` לבדו לא מספיק, כי סימני נִיקוד (להבדיל מטעמים)
        // מסווגים ב-Unicode כ-Other_Alphabetic; בלי הסינון סימן תועה בין
        // רווחים היה פותח טוקן שמתרוקן אחרי הסרת הסימנים.
        let tok_start_rel = slice
            .char_indices()
            .find(|(_, c)| c.is_alphanumeric() && !is_attached_mark(*c))
            .map(|(i, _)| i)?;

        let tok_start = start_byte + tok_start_rel;
        let s = &text[tok_start..];

        // `pos` — סוף רצף תווי-המילה האחרון. הלולאה מדמה את הטקסט כאילו
        // התווים השקופים נמחקו (כמו בצד השאילתה), אך בגבולות המקוריים.
        let mut pos = 0;
        let tok_end_rel = loop {
            pos += s[pos..]
                .char_indices()
                .find(|(_, c)| !is_word_char(*c))
                .map(|(i, _)| i)
                .unwrap_or(s.len() - pos);

            // מבט קדימה מעבר לגרש אחד ולתווים שקופים: גרש שאחריו (גם דרך
            // שקופים) תו-מילה הוא אמצעי — חותך, כמו אחרי מחיקת הפיסוק.
            let mut geresh_end: Option<usize> = None;
            let mut next: Option<(usize, char)> = None;
            for (i, c) in s[pos..].char_indices() {
                if geresh_end.is_none() && is_geresh(c) {
                    geresh_end = Some(pos + i + c.len_utf8());
                } else if !is_transparent(c) {
                    next = Some((pos + i, c));
                    break;
                }
            }

            match next {
                Some((i, c)) if is_word_char(c) => {
                    if geresh_end.is_some() {
                        break pos; // גרש באמצע — הטוקן נגמר לפניו
                    }
                    pos = i; // פיסוק שקוף בין מילים — הטוקן נמשך
                }
                // סוף מילה אמיתי: גרש סוגר (אם היה) נכלל בטוקן.
                _ => break geresh_end.unwrap_or(pos),
            }
        };

        let tok_end = tok_start + tok_end_rel;
        Some((&text[tok_start..tok_end], tok_start, tok_end))
    }
}

impl<'a> TokenStream for HebrewTokenStream<'a> {
    fn advance(&mut self) -> bool {
        match Self::find_next_token(self.text, self.byte_pos) {
            None => false,
            Some((tok_text, tok_start, tok_end)) => {
                self.token.text.clear();
                // נרמול טקסט הטוקן לצורת מילון הטרמים: הסרת ניקוד/טעמים
                // ופיסוק שקוף, קיפול ׳→' ופירוק Presentation Forms.
                // במסלול המהיר — הרוב המכריע — העתקה ישירה.
                if tok_text.chars().any(needs_normalising) {
                    for c in tok_text.chars() {
                        if is_attached_mark(c) || is_transparent(c) {
                            continue;
                        }
                        if c == '\u{05F3}' {
                            self.token.text.push('\'');
                        } else if let Some(folded) = fold_presentation_form(c) {
                            self.token
                                .text
                                .extend(folded.chars().filter(|f| !is_attached_mark(*f)));
                        } else {
                            self.token.text.push(c);
                        }
                    }
                } else {
                    self.token.text.push_str(tok_text);
                }
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

    fn spans(text: &str) -> Vec<(usize, usize)> {
        let mut tokenizer = HebrewTokenizer;
        let mut stream = tokenizer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push((stream.token().offset_from, stream.token().offset_to));
        }
        out
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
    fn test_hebrew_gershayim_splits() {
        assert_eq!(tokenize("רמב\u{05F4}ם"), vec!["רמב", "ם"]);
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
    fn test_hebrew_geresh_folded_to_ascii() {
        // ׳ (U+05F3) מקופל ל-' בטקסט הטוקן כדי להתאים למילון הטרמים,
        // שנבנה משאילתות שעברו sanitize_query.
        assert_eq!(tokenize("תוס\u{05F3}"), vec!["תוס'"]);
    }

    #[test]
    fn test_plain_words() {
        assert_eq!(tokenize("שלום עולם"), vec!["שלום", "עולם"]);
    }

    // ── תווים שקופים (פיסוק שנשמר לתצוגה אך אינו במילון הטרמים) ──────────

    #[test]
    fn test_transparent_glues_like_query_sanitizer() {
        // sanitize_query מוחק את הנקודה → "אב"; הטוקנייזר חייב לייצר את
        // אותו טרם מהטקסט המשמר-פיסוק.
        assert_eq!(tokenize("א.ב"), vec!["אב"]);
        assert_eq!(tokenize("3.14"), vec!["314"]);
        assert_eq!(tokenize("א.ב.ג"), vec!["אבג"]);
    }

    #[test]
    fn test_trailing_punctuation_outside_token() {
        assert_eq!(tokenize("עא: אמר"), vec!["עא", "אמר"]);
        assert_eq!(tokenize("שלום, עולם!"), vec!["שלום", "עולם"]);
        assert_eq!(tokenize("(שלום)"), vec!["שלום"]);
    }

    #[test]
    fn test_trailing_punctuation_excluded_from_span() {
        // ה-span קובע את גבולות ההדגשה — פיסוק נגרר לא מודגש.
        let text = "שלום, עולם";
        assert_eq!(spans(text), vec![(0, 8), (10, 18)]);
        // אבל פיסוק "בולע" באמצע טוקן כן נכלל ב-span.
        assert_eq!(spans("א.ב"), vec![(0, 5)]);
    }

    #[test]
    fn test_geresh_across_transparent() {
        // "תוס'." — הגרש נשאר סוגר גם כשפיסוק שקוף אחריו.
        assert_eq!(tokenize("תוס'."), vec!["תוס'"]);
        // "תוס'.ב" — אחרי מחיקת הנקודה הגרש אמצעי → חותך בלעדיו.
        assert_eq!(tokenize("תוס'.ב"), vec!["תוס", "ב"]);
    }

    #[test]
    fn test_separators_still_split() {
        // מקף, סוף-פסוק, פסק ו-| הם מפרידים — לא שקופים.
        assert_eq!(tokenize("אל־משה"), vec!["אל", "משה"]);
        assert_eq!(tokenize("הארץ\u{05C3} והארץ"), vec!["הארץ", "והארץ"]);
        assert_eq!(tokenize("א|ב"), vec!["א", "ב"]);
        assert_eq!(tokenize("בית-דין"), vec!["בית", "דין"]);
    }

    // ── Presentation Forms ────────────────────────────────────────────────

    #[test]
    fn test_presentation_forms_folded() {
        // יוד עם חיריק מורכבת (U+FB1D) — הבסיס נשאר, הסימן מוסר.
        assert_eq!(tokenize("מ\u{FB1D}ם גנובים"), vec!["מים", "גנובים"]);
        // שין עם נקודה (U+FB2A) ואלף-למד (U+FB4F).
        assert_eq!(tokenize("\u{FB2A}לום"), vec!["שלום"]);
        assert_eq!(tokenize("\u{FB4F}הים"), vec!["אלהים"]);
    }

    // ── קלט מנוקד (הגנת-עומק — הנרמול העיקרי קורה לפני האינדוקס) ─────────

    #[test]
    fn test_vocalized_word_stays_single_token_and_marks_stripped() {
        // בלי קבלת ניקוד כתו-מילה, "סֵפֶר" היה מתפרק ל-ס/פ/ר; הסימנים
        // מוסרים מהטוקן כדי שימופה לאותו טרם כמו "ספר".
        assert_eq!(tokenize("סֵפֶר"), vec!["ספר"]);
        assert_eq!(tokenize("בְּרֵאשִׁית בָּרָא"), vec!["בראשית", "ברא"]);
        // טעמי מקרא (Mn בלבד, בניגוד לניקוד שהוא Other_Alphabetic) — הבאג
        // המקורי: בלעדי is_attached_mark המילה הייתה נחתכת בטעם.
        assert_eq!(tokenize("שָׁמַ֣ע"), vec!["שמע"]);
    }

    #[test]
    fn test_maqaf_still_separates_vocalized_words() {
        // מקף (U+05BE) הוא מפריד אף שהוא בטווח הניקוד — אסור שייבלע.
        assert_eq!(tokenize("אֲשֶׁר־שָׁמַע"), vec!["אשר", "שמע"]);
    }

    #[test]
    fn test_stray_mark_does_not_open_token() {
        // סימן ניקוד תועה בין רווחים אינו מייצר טוקן ריק.
        assert_eq!(tokenize(" \u{05B0} אבג"), vec!["אבג"]);
        assert_eq!(tokenize("\u{05B0}"), Vec::<String>::new());
    }

    #[test]
    fn test_standalone_word_no_geresh() {
        assert_eq!(tokenize("תוס"), vec!["תוס"]);
    }
}
