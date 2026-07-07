use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

use crate::hebrew_query::{fold_presentation_form, is_attached_mark};

/// טוקנייזר עברי שמתנהג כמו SimpleTokenizer עם שלוש תוספות:
/// גרש (' או ׳) בין אותיות או בסוף טוקן נשמר בו; גרשיים (" או ״) בין
/// אותיות נשמרות בטוקן (ראשי-תיבות הם טרם אחד); ותווים "שקופים"
/// ([`TRANSPARENT`]) אינם שוברים טוקן אך מושמטים מטקסטו — כך שטקסט
/// מאונדקס שמשמר פיסוק לתצוגה ממופה לאותם טרמים כמו שאילתה שעברה
/// `sanitize_query`. ׳/״ מקופלים בטקסט הטוקן ל-'/" ASCII, וזוג גרשים
/// רצוף בין אותיות (מוסכמת `רמב''ם` בקבצים ישנים) מאוחד ל-`"`.
///
/// דוגמאות:
///   "תוס'"    → ["תוס'"]     (גרש בסוף — נשמר)
///   "ז\"ל"    → ["ז\"ל"]     (גרשיים באמצע — חלק מהטוקן)
///   "רמב''ם"  → ["רמב\"ם"]   (זוג גרשים → גרשיים)
///   "רמב\""   → ["רמב"]      (גרשיים בקצה — מפריד)
///   "א.ב"     → ["אב"]       (נקודה שקופה — לא שוברת ולא נכללת)
///   "עא:"     → ["עא"]       (פיסוק בסוף — מחוץ לטוקן)
#[derive(Clone, Default)]
pub struct HebrewTokenizer;

/// כמו [`HebrewTokenizer`] אך *שומר* ניקוד וטעמים בטקסט הטוקן — הטוקנייזר
/// של השדה המנוקד (`textVocalized`). גבולות הטוקנים ומיקומיהם זהים
/// אחד-לאחד לטוקנייזר הרגיל (סימן צמוד הוא תו-מילה בשניהם), כך ששני
/// השדות של אותה שורה מטוקננים לאותן עמדות; רק טקסט הטרם שונה.
#[derive(Clone, Default)]
pub struct VocalizedHebrewTokenizer;

pub struct HebrewTokenStream<'a> {
    text: &'a str,
    token: Token,
    byte_pos: usize,
    token_count: usize,
    keep_marks: bool,
}

impl Tokenizer for HebrewTokenizer {
    type TokenStream<'a> = HebrewTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        HebrewTokenStream {
            text,
            token: Token::default(),
            byte_pos: 0,
            token_count: 0,
            keep_marks: false,
        }
    }
}

impl Tokenizer for VocalizedHebrewTokenizer {
    type TokenStream<'a> = HebrewTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        HebrewTokenStream {
            text,
            token: Token::default(),
            byte_pos: 0,
            token_count: 0,
            keep_marks: true,
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

#[inline]
fn is_gershayim(c: char) -> bool {
    c == '"' || c == '\u{05F4}'
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
/// `'` ASCII נכלל בגלל איחוד זוג-הגרשים (`''`→`"`); `"` ASCII מועתק
/// כמות-שהוא ולכן אינו כאן.
#[inline]
fn needs_normalising(c: char) -> bool {
    is_attached_mark(c)
        || is_transparent(c)
        || c == '\''
        || c == '\u{05F3}'
        || c == '\u{05F4}'
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

            // מבט קדימה דרך תווים שקופים: אסוף את רצף הגרשים/הגרשיים
            // (מקופלים ל-ASCII) עד התו הבא שאינו שקוף ואינו גרש — הוא
            // שמכריע אם הרצף פנימי (הטוקן נמשך דרכו) או סוגר.
            let mut quotes = String::new();
            let mut first_quote_end = 0usize;
            let mut next: Option<(usize, char)> = None;
            for (i, c) in s[pos..].char_indices() {
                if is_geresh(c) || is_gershayim(c) {
                    if quotes.is_empty() {
                        first_quote_end = pos + i + c.len_utf8();
                    }
                    quotes.push(if is_geresh(c) { '\'' } else { '"' });
                } else if !is_transparent(c) {
                    next = Some((pos + i, c));
                    break;
                }
            }

            match next {
                Some((i, c)) if is_word_char(c) => match quotes.as_str() {
                    // בלי גרשים — פיסוק שקוף בין אותיות, הטוקן נמשך.
                    // גרש / זוג-גרשים (D2) / גרשיים פנימיים — נכללים
                    // בטוקן והוא נמשך.
                    "" | "'" | "''" | "\"" => pos = i,
                    // כל צירוף אחר ("", '"' כפולות, שלשות...) — מפריד.
                    _ => break pos,
                },
                // סוף מילה אמיתי: גרש סוגר (הראשון בלבד) נכלל בטוקן;
                // גרשיים סוגרות הן מפריד ונשארות בחוץ.
                _ => {
                    break if quotes.starts_with('\'') {
                        first_quote_end
                    } else {
                        pos
                    }
                }
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
                // ופיסוק שקוף, קיפול ׳→' ו-״→", איחוד זוג גרשים רצוף
                // ל-`"` (D2) ופירוק Presentation Forms. במסלול המהיר —
                // הרוב המכריע — העתקה ישירה.
                if tok_text.chars().any(needs_normalising) {
                    for c in tok_text.chars() {
                        if (!self.keep_marks && is_attached_mark(c)) || is_transparent(c) {
                            continue;
                        }
                        if self.keep_marks && is_attached_mark(c) {
                            // מצב מנוקד: הסימן נשמר כמות שהוא (הוא לעולם לא
                            // גרש/גרשיים/Presentation Form).
                            self.token.text.push(c);
                            continue;
                        }
                        if is_geresh(c) {
                            // זוג גרשים (גם דרך שקופים, שכבר סוננו) הוא
                            // פנימי בהכרח — גרש סוגר נבלע יחיד — ולכן
                            // בטוח לאחדו לגרשיים.
                            if self.token.text.ends_with('\'') {
                                self.token.text.pop();
                                self.token.text.push('"');
                            } else {
                                self.token.text.push('\'');
                            }
                        } else if c == '\u{05F4}' {
                            self.token.text.push('"');
                        } else if let Some(folded) = fold_presentation_form(c) {
                            let keep_marks = self.keep_marks;
                            self.token.text.extend(
                                folded
                                    .chars()
                                    .filter(|f| keep_marks || !is_attached_mark(*f)),
                            );
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

    // ── גרשיים בתוך טוקן (ראשי-תיבות הם טרם אחד) ─────────────────────────

    #[test]
    fn test_gershayim_kept_in_token() {
        assert_eq!(tokenize("ז\"ל"), vec!["ז\"ל"]);
        assert_eq!(tokenize("ז\"ל אמר"), vec!["ז\"ל", "אמר"]);
    }

    #[test]
    fn test_rambam() {
        assert_eq!(tokenize("רמב\"ם"), vec!["רמב\"ם"]);
    }

    #[test]
    fn test_hebrew_gershayim_folded_to_ascii() {
        // ״ (U+05F4) בטוקן מקופל ל-" כדי להתאים לשאילתות שעברו sanitize.
        assert_eq!(tokenize("רמב\u{05F4}ם"), vec!["רמב\"ם"]);
    }

    #[test]
    fn test_multiple_internal_gershayim() {
        assert_eq!(tokenize("א\"ב\"ג"), vec!["א\"ב\"ג"]);
    }

    #[test]
    fn test_edge_gershayim_still_separate() {
        // גרשיים בקצה מילה — מפריד, לא נבלעות (בניגוד לגרש סוגר).
        assert_eq!(tokenize("\"רמב"), vec!["רמב"]);
        assert_eq!(tokenize("רמב\""), vec!["רמב"]);
        assert_eq!(tokenize("רמב\"\"ם"), vec!["רמב", "ם"]);
        assert_eq!(tokenize("אמר \"שלום\" לכולם"), vec!["אמר", "שלום", "לכולם"]);
    }

    #[test]
    fn test_gershayim_across_transparent() {
        // גרשיים ואז פיסוק שקוף ואז אות — ה-lookahead עובר דרך השקוף,
        // עקבי עם מנגנון הגרש.
        assert_eq!(tokenize("רמב\".ם"), vec!["רמב\"ם"]);
    }

    #[test]
    fn test_double_geresh_collapsed_to_gershayim() {
        // מוסכמת קבצים ישנים: רמב''ם ≡ רמב"ם (D2).
        assert_eq!(tokenize("רמב''ם"), vec!["רמב\"ם"]);
        assert_eq!(tokenize("רמב\u{05F3}\u{05F3}ם"), vec!["רמב\"ם"]);
        // זוג גרשים בסוף מילה אינו מאוחד — נבלע רק גרש סוגר יחיד.
        assert_eq!(tokenize("וכו''"), vec!["וכו'"]);
    }

    #[test]
    fn test_phrase_with_trailing_geresh() {
        assert_eq!(tokenize("תוס' ד\"ה"), vec!["תוס'", "ד\"ה"]);
    }

    // ── גרש בתוך טוקן ─────────────────────────────────────────────────────

    #[test]
    fn test_geresh_in_middle_kept() {
        assert_eq!(tokenize("ד'אש"), vec!["ד'אש"]);
        assert_eq!(tokenize("ד\u{05F3}אש"), vec!["ד'אש"]);
        assert_eq!(tokenize("ג'ורג'"), vec!["ג'ורג'"]);
        assert_eq!(tokenize("צ'יפס"), vec!["צ'יפס"]);
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
        // "תוס'.ב" — אחרי מחיקת הנקודה הגרש אמצעי → נכלל והטוקן נמשך.
        assert_eq!(tokenize("תוס'.ב"), vec!["תוס'ב"]);
    }

    #[test]
    fn test_gershayim_span_covers_whole_token() {
        // ה-span קובע את גבולות ההדגשה — הגרשיים בתוך הטווח.
        assert_eq!(spans("רמב\"ם"), vec![(0, 9)]);
        assert_eq!(spans("רמב\u{05F4}ם אמר"), vec![(0, 10), (11, 17)]);
        // גרשיים סוגרות מחוץ ל-span.
        assert_eq!(spans("רמב\""), vec![(0, 6)]);
    }

    #[test]
    fn test_tokens_never_contain_hebrew_geresh_forms() {
        // הערובה המורחבת של c7a6e22: טוקן לעולם לא מכיל ׳/״, וגרשיים
        // בטוקן הן תמיד `"` ASCII יחידה.
        let samples = [
            "רמב\u{05F4}ם",
            "תוס\u{05F3} ד\u{05F4}ה",
            "רמב''ם וכו''",
            "ג\u{05F3}ורג\u{05F3}",
            "א\u{05F4}ב\u{05F4}ג",
            "שָׁמַ֣ע רמב\u{05F4}ם",
        ];
        for s in samples {
            for tok in tokenize(s) {
                assert!(
                    !tok.contains('\u{05F3}') && !tok.contains('\u{05F4}'),
                    "token {tok:?} from {s:?} contains a Hebrew geresh form"
                );
                assert!(
                    !tok.contains("\"\"") && !tok.contains("''"),
                    "token {tok:?} from {s:?} contains a double quote run"
                );
            }
        }
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

    // ── הטוקנייזר המנוקד ─────────────────────────────────────────────────

    fn tokenize_vocalized(text: &str) -> Vec<String> {
        let mut tokenizer = VocalizedHebrewTokenizer;
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }

    #[test]
    fn vocalized_keeps_marks_in_tokens() {
        assert_eq!(tokenize_vocalized("בְּרֵאשִׁית בָּרָא"), vec!["בְּרֵאשִׁית", "בָּרָא"]);
        // טעמי מקרא נשמרים גם הם.
        assert_eq!(tokenize_vocalized("שָׁמַ֣ע"), vec!["שָׁמַ֣ע"]);
    }

    #[test]
    fn vocalized_boundaries_match_plain_tokenizer() {
        // גבולות ועמדות זהים לטוקנייזר הרגיל — רק טקסט הטרם שונה.
        let samples = [
            "אֲשֶׁר־שָׁמַע",
            "וַיֹּאמֶר אֱלֹהִים: יְהִי אוֹר!",
            "רמב\"ם תוס' ד'אש",
            "הָאָרֶץ\u{05C3} וְהָאָרֶץ",
            "מ\u{FB1D}ם גנובים",
        ];
        for s in samples {
            let plain = spans(s);
            let mut tokenizer = VocalizedHebrewTokenizer;
            let mut stream = tokenizer.token_stream(s);
            let mut voc = Vec::new();
            while stream.advance() {
                voc.push((stream.token().offset_from, stream.token().offset_to));
            }
            assert_eq!(plain, voc, "boundaries diverged for {s:?}");
        }
    }

    #[test]
    fn vocalized_still_folds_quotes_and_presentation_forms() {
        // גרש/גרשיים מקופלים ל-ASCII כמו ברגיל; Presentation Form מתפרק
        // והסימן שלו נשמר.
        assert_eq!(tokenize_vocalized("רמב\u{05F4}ם"), vec!["רמב\"ם"]);
        assert_eq!(tokenize_vocalized("מ\u{FB1D}ם"), vec!["מ\u{05D9}\u{05B4}ם"]);
    }

    #[test]
    fn vocalized_transparent_still_dropped() {
        // פיסוק שקוף מושמט מהטוקן גם במצב מנוקד.
        assert_eq!(tokenize_vocalized("בָּרָא."), vec!["בָּרָא"]);
    }
}
