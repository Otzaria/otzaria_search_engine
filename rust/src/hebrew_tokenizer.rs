use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

use crate::hebrew_query::{
    fold_presentation_form, is_attached_mark, is_invisible_char, is_word_mark,
};

/// טוקנייזר עברי שמתנהג כמו SimpleTokenizer עם שלוש תוספות:
/// גרש (' או ׳) בין אותיות או בסוף טוקן נשמר בו; גרשיים (" או ״) בין
/// אותיות נשמרות בטוקן (ראשי-תיבות הם טרם אחד); ותווים "שקופים"
/// ([`is_transparent`]) אינם שוברים טוקן אך מושמטים מטקסטו — כך שטקסט
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
///
/// עם `emit_quote_free` (מצב האינדוקס): טוקן שמכיל גרש/גרשיים פולט מיד
/// אחריו טוקן-תאום נטול-גרשיים באותה עמדה ובאותם offsets (כמו נרדפת), כך
/// שחיפוש "רמבם" מוצא גם `רמב"ם` ושאילתה שמסירה גרשיים מוצאת את שתי
/// הצורות. בצד השאילתה נרשמים האנליזטורים בלי הפליטה הכפולה — שאילתה
/// לעולם לא מתפרקת לשני טוקנים על אותה מילה.
///
/// עם `keep_marks` (השדה המנוקד, `textVocalized`): ניקוד/טעמים/combining
/// *נשמרים* בטקסט הטוקן. גבולות הטוקנים ומיקומיהם זהים אחד-לאחד בשני
/// המצבים (סימן צמוד הוא תו-מילה בשניהם), כך ששני השדות של אותה שורה
/// מטוקננים לאותן עמדות; רק טקסט הטרם שונה.
#[derive(Clone, Default)]
pub struct HebrewTokenizer {
    pub emit_quote_free: bool,
    pub keep_marks: bool,
}

pub struct HebrewTokenStream<'a> {
    text: &'a str,
    token: Token,
    byte_pos: usize,
    token_count: usize,
    keep_marks: bool,
    emit_quote_free: bool,
    /// טוקן-תאום נטול-גרשיים שממתין להיפלט (ראו `emit_quote_free`).
    pending_quote_free: Option<Token>,
}

impl Tokenizer for HebrewTokenizer {
    type TokenStream<'a> = HebrewTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        HebrewTokenStream {
            text,
            token: Token::default(),
            byte_pos: 0,
            token_count: 0,
            keep_marks: self.keep_marks,
            emit_quote_free: self.emit_quote_free,
            pending_quote_free: None,
        }
    }
}

/// תו "שקוף": הסט ש-`sanitize_query` מוחק בצד השאילתה. אינו שובר טוקן
/// ואינו נכלל בטקסטו, אך נשאר בטקסט השמור (שהוא טקסט התצוגה).
/// נקודה נשארת שקופה (ראשי-תיבות `פ.ב.י`, מספרים `3.14`) וכן `[]`
/// (השלמות עריכתיות תוך-מילה: `יב[ע]ר`), ואיתם התווים הבלתי-נראים
/// (bidi/zero-width — עקבי עם מסלול ה-PDF שמוחק אותם לפני אינדוקס).
/// פיסוק ההפסקה — `,;:!?(){}` — הוא *מפריד* (שובר טוקן), כמו מקף, `|`
/// וסוף-פסוק: בקורפוס הוא מדביק בעיקר מספרי הערות-שוליים (`רעהו,10`)
/// ופיסוק חסר-רווח (`שלום,עולם`).
#[inline]
fn is_transparent(c: char) -> bool {
    matches!(
        c,
        '*' | '[' | ']' | '^' | '$' | '\\' | '+' | '.' | '~' | '`'
    ) || is_invisible_char(c)
}

/// גרש: ASCII, עברי (U+05F3), או ציטוט-יחיד טיפוגרפי (U+2018/U+2019 —
/// ברינדור RTL שתי הצורות משמשות באותו תפקיד, ראו סריקת הקורפוס).
#[inline]
fn is_geresh(c: char) -> bool {
    matches!(c, '\'' | '\u{05F3}' | '\u{2018}' | '\u{2019}')
}

/// גרשיים: ASCII, עברי (U+05F4), או ציטוט-כפול טיפוגרפי (U+201C/U+201D).
#[inline]
fn is_gershayim(c: char) -> bool {
    matches!(c, '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}')
}

/// תו שממשיך טוקן: אות/ספרה, או סימן צמוד ([`is_word_mark`] — ניקוד/טעם
/// עברי, combining כללי, varika). הטקסט המנורמל לאינדוקס נקי מניקוד ממילא;
/// קבלת הסימנים כאן היא הגנת-עומק כדי שקלט שלא נורמל (למשל דרך
/// `add_document` ישיר) לא יתפרק באמצע מילה, ותמיכה אמיתית ב-combining
/// הכלליים שנשמרים בטקסט (תעתיק ערבית-יהודית: `כלת̇ום`).
#[inline]
pub(crate) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_word_mark(c)
}

/// האם `c` יכול להופיע *בתוך* טוקן בלי לשבור אותו: תו-מילה, תו שקוף
/// (`פ.ב.י`, `יב[ע]ר`) או גרש/גרשיים (`רמב״ם`, `תוס'`).
///
/// מכאן נגזרות מחלקות התווים של [`crate::display_highlight`], כדי שספירת
/// המילים שבין מילות השאילתה בהדגשה תהיה זהה לספירת הטוקנים באינדקס.
/// שכפול ידני של קבוצת התווים סטה מכאן בעבר: `כי־גר` נספרה כמילה אחת
/// (מקף) בעוד האינדקס מפצל אותה לשתיים.
#[inline]
pub(crate) fn continues_token(c: char) -> bool {
    is_word_char(c) || is_transparent(c) || is_geresh(c) || is_gershayim(c)
}

/// דוחף `"` לטקסט הטוקן, אלא אם הוא כבר מסתיים ב-`"`. שני רצפי-גרשיים
/// שמופרדים רק בסימנים צמודים (למשל `רמב''ְ"ם` — קלט פתולוגי אך אפשרי)
/// היו מייצרים `""` צמודות אחרי הסרת הסימנים — בניגוד לערובה שטוקן לעולם
/// אינו מכיל רצף גרשיים כפול; הצורה הקנונית במילון היא `"` יחידה.
#[inline]
fn push_gershayim_deduped(text: &mut String) {
    if !text.ends_with('"') {
        text.push('"');
    }
}

/// תו שדורש בנייה איטית של טקסט הטוקן (סינון/קיפול) במקום העתקה ישירה.
/// `'` ASCII נכלל בגלל איחוד זוג-הגרשים (`''`→`"`); `"` ASCII מועתק
/// כמות-שהוא ולכן אינו כאן.
#[inline]
fn needs_normalising(c: char) -> bool {
    is_word_mark(c)
        || is_transparent(c)
        || is_geresh(c)
        || (is_gershayim(c) && c != '"')
        || fold_presentation_form(c).is_some()
}

/// גבולות הטוקן הבא החל מ-`start_byte`: `(tok_start, tok_end)` בבייטים,
/// על גבולות char. **ליבת-הגבולות המשותפת** — הטוקנייזר (צד האינדקס) סורק
/// איתה טקסט משמר-פיסוק, ו-`split_query_words` (צד השאילתה) סורק איתה
/// שאילתה שעברה `sanitize_query` — מקור אמת אחד לכללי "היכן מילה נשברת",
/// כך ששני הצדדים אינם יכולים לסטות זה מזה.
pub(crate) fn next_token_boundaries(text: &str, start_byte: usize) -> Option<(usize, usize)> {
    let slice = &text[start_byte..];

    // מצא את תחילת הטוקן — חייב להתחיל באות/ספרה של ממש. שימו לב:
    // `is_alphanumeric` לבדו לא מספיק, כי סימני נִיקוד (להבדיל מטעמים)
    // וגם כמה combining כלליים (כמו U+0345) מסווגים ב-Unicode
    // כ-Other_Alphabetic; בלי הסינון סימן תועה בין רווחים היה פותח
    // טוקן שמתרוקן אחרי הסרת הסימנים.
    let tok_start_rel = slice
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric() && !is_word_mark(*c))
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

        // מבט קדימה דרך תווים שקופים: ספור את רצף הגרשים/הגרשיים עד התו
        // הבא שאינו שקוף ואינו גרש — הוא שמכריע אם הרצף פנימי (הטוקן
        // נמשך דרכו) או סוגר. מונים בלבד — בלי הקצאת מחרוזת: הרצף מורכב
        // מגרשים בלבד, ולכן ספירת (גרש, גרשיים) קובעת את הצורה חד-ערכית.
        let mut geresh_n = 0usize;
        let mut gershayim_n = 0usize;
        let mut first_is_geresh = false;
        let mut first_quote_end = 0usize;
        let mut next: Option<(usize, char)> = None;
        for (i, c) in s[pos..].char_indices() {
            if is_geresh(c) || is_gershayim(c) {
                if geresh_n + gershayim_n == 0 {
                    first_quote_end = pos + i + c.len_utf8();
                    first_is_geresh = is_geresh(c);
                }
                if is_geresh(c) {
                    geresh_n += 1;
                } else {
                    gershayim_n += 1;
                }
            } else if !is_transparent(c) {
                next = Some((pos + i, c));
                break;
            }
        }

        match next {
            Some((i, c)) if is_word_char(c) => match (geresh_n, gershayim_n) {
                // בלי גרשים — פיסוק שקוף בין אותיות, הטוקן נמשך.
                // גרש / זוג-גרשים (D2) / גרשיים פנימיים — נכללים
                // בטוקן והוא נמשך.
                (0, 0) | (1, 0) | (2, 0) | (0, 1) => pos = i,
                // כל צירוף אחר (גרשיים כפולות, שלשות, ערבוב...) — מפריד.
                _ => break pos,
            },
            // סוף מילה אמיתי: גרש סוגר (הראשון בלבד) נכלל בטוקן;
            // גרשיים סוגרות הן מפריד ונשארות בחוץ.
            _ => {
                break if first_is_geresh {
                    first_quote_end
                } else {
                    pos
                }
            }
        }
    };

    Some((tok_start, tok_start + tok_end_rel))
}

impl<'a> TokenStream for HebrewTokenStream<'a> {
    fn advance(&mut self) -> bool {
        // טוקן-תאום ממתין (נטול-גרשיים, אותה עמדה) — נפלט לפני המילה הבאה.
        if let Some(pending) = self.pending_quote_free.take() {
            self.token = pending;
            return true;
        }
        match next_token_boundaries(self.text, self.byte_pos) {
            None => false,
            Some((tok_start, tok_end)) => {
                let tok_text = &self.text[tok_start..tok_end];
                self.token.text.clear();
                // נרמול טקסט הטוקן לצורת מילון הטרמים: הסרת ניקוד/טעמים
                // ופיסוק שקוף, קיפול ׳→' ו-״→", איחוד זוג גרשים רצוף
                // ל-`"` (D2) ופירוק Presentation Forms. במסלול המהיר —
                // הרוב המכריע — העתקה ישירה.
                if tok_text.chars().any(needs_normalising) {
                    for c in tok_text.chars() {
                        // סימן צמוד (ניקוד/טעם/combining כללי): מוסר במצב
                        // הרגיל, נשמר במצב המנוקד — למעט varika (U+FB1E),
                        // שהוא Presentation Form ומקופל ל-"" בהמשך.
                        if is_word_mark(c) && c != '\u{FB1E}' {
                            if self.keep_marks {
                                self.token.text.push(c);
                            }
                            continue;
                        }
                        if is_transparent(c) {
                            continue;
                        }
                        if is_geresh(c) {
                            // זוג גרשים (גם דרך שקופים, שכבר סוננו) הוא
                            // פנימי בהכרח — גרש סוגר נבלע יחיד — ולכן
                            // בטוח לאחדו לגרשיים.
                            if self.token.text.ends_with('\'') {
                                self.token.text.pop();
                                push_gershayim_deduped(&mut self.token.text);
                            } else {
                                self.token.text.push('\'');
                            }
                        } else if is_gershayim(c) {
                            push_gershayim_deduped(&mut self.token.text);
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
                // מצב אינדוקס: מילה עם גרש/גרשיים מטמיעה גם את צורתה
                // הנקייה באותה עמדה ובאותם offsets — שאילתות phrase רואות
                // עמדה אחת, וההדגשה יורשת את טווח המילה המקורית.
                if self.emit_quote_free && self.token.text.contains(['\'', '"']) {
                    let stripped: String = self
                        .token
                        .text
                        .chars()
                        .filter(|c| *c != '\'' && *c != '"')
                        .collect();
                    if !stripped.is_empty() && stripped != self.token.text {
                        let mut dup = self.token.clone();
                        dup.text = stripped;
                        self.pending_quote_free = Some(dup);
                    }
                }
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
        let mut tokenizer = HebrewTokenizer::default();
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }

    fn spans(text: &str) -> Vec<(usize, usize)> {
        let mut tokenizer = HebrewTokenizer::default();
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

    #[test]
    fn test_quote_runs_split_by_marks_dedupe_to_single_gershayim() {
        // רגרסיה שנמצאה ב-property test: שני רצפי-גרשיים שמופרדים רק
        // בסימן צמוד (שנופל מהטקסט) היו מייצרים `""` צמודות בטרם.
        // הצורה הקנונית היא `"` יחידה.
        assert_eq!(tokenize("רמב''\u{05B0}\"ם"), vec!["רמב\"ם"]);
        assert_eq!(tokenize("רמב\"\u{0307}\"ם"), vec!["רמב\"ם"]);
        assert_eq!(tokenize("רמב\"\u{05B8}''ם"), vec!["רמב\"ם"]);
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
        // הערובה המורחבת של c7a6e22: טוקן לעולם לא מכיל ׳/״ או צורה
        // טיפוגרפית, וגרשיים בטוקן הן תמיד `"` ASCII יחידה.
        let samples = [
            "רמב\u{05F4}ם",
            "תוס\u{05F3} ד\u{05F4}ה",
            "רמב''ם וכו''",
            "ג\u{05F3}ורג\u{05F3}",
            "א\u{05F4}ב\u{05F4}ג",
            "שָׁמַ֣ע רמב\u{05F4}ם",
            "רמח\u{201D}ל שד\u{201C}ל",
            "תוס\u{2019} ד\u{2018}אש",
            "רמב\u{2019}\u{2019}ם",
        ];
        for s in samples {
            for tok in tokenize(s) {
                assert!(
                    !tok.contains(['\u{05F3}', '\u{05F4}'])
                        && !tok.contains(['\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}']),
                    "token {tok:?} from {s:?} contains a non-ASCII quote form"
                );
                assert!(
                    !tok.contains("\"\"") && !tok.contains("''"),
                    "token {tok:?} from {s:?} contains a double quote run"
                );
            }
        }
    }

    // ── גרשיים טיפוגרפיים (Word/OCR: ‘ ’ “ ”) ───────────────────────────

    #[test]
    fn test_typographic_quotes_folded_like_hebrew_forms() {
        // ברינדור RTL צורת ה"פתיחה" וה"סגירה" משמשות לסירוגין באותו
        // תפקיד (בקורפוס: `רמח”ל` וגם `רמח“ל`) — כל הארבע מקופלות.
        assert_eq!(tokenize("רמח\u{201D}ל"), vec!["רמח\"ל"]);
        assert_eq!(tokenize("רמח\u{201C}ל"), vec!["רמח\"ל"]);
        assert_eq!(tokenize("תוס\u{2019}"), vec!["תוס'"]);
        assert_eq!(tokenize("ד\u{2018}אש"), vec!["ד'אש"]);
        // זוג ציטוטים-יחידים טיפוגרפיים ≡ גרשיים (מוסכמת `רמב''ם`).
        assert_eq!(tokenize("רמב\u{2019}\u{2019}ם"), vec!["רמב\"ם"]);
        // בקצוות — מפריד, כמו גרשיים רגילות.
        assert_eq!(
            tokenize("אמר \u{201C}שלום\u{201D} לכולם"),
            vec!["אמר", "שלום", "לכולם"]
        );
    }

    // ── combining marks כלליים (U+0300–U+036F) ו-varika ─────────────────

    #[test]
    fn test_general_combining_marks_continue_word_and_strip() {
        // תעתיק ערבית-יהודית: הנקודה העילית אינה שוברת את המילה ומוסרת
        // מטקסט הטרם — `כלת̇ום` נמצא בחיפוש `כלתום`.
        assert_eq!(tokenize("כלת\u{0307}ום"), vec!["כלתום"]);
        assert_eq!(tokenize("מצ\u{0307}ארע גדול"), vec!["מצארע", "גדול"]);
        // varika (U+FB1E) — ממשיך-מילה שנבלע דרך קיפול Presentation Forms.
        assert_eq!(tokenize("א\u{FB1E}ב"), vec!["אב"]);
        // סימן תועה בין רווחים אינו פותח טוקן.
        assert_eq!(tokenize(" \u{0307} אבג"), vec!["אבג"]);
    }

    #[test]
    fn test_vocalized_keeps_general_combining_marks() {
        // בשדה המנוקד הסימן נשמר בטרם (כמו ניקוד) — והגבולות זהים לרגיל.
        assert_eq!(tokenize_vocalized("כלת\u{0307}ום"), vec!["כלת\u{0307}ום"]);
        assert_eq!(spans("כלת\u{0307}ום"), {
            let mut tokenizer = HebrewTokenizer {
                keep_marks: true,
                ..Default::default()
            };
            let mut stream = tokenizer.token_stream("כלת\u{0307}ום");
            let mut voc = Vec::new();
            while stream.advance() {
                voc.push((stream.token().offset_from, stream.token().offset_to));
            }
            voc
        });
    }

    // ── פיסוק מפריד (הופרד מ-TRANSPARENT) ────────────────────────────────

    #[test]
    fn test_breaking_punctuation_splits_tokens() {
        // מספרי הערות-שוליים דבוקים ופיסוק חסר-רווח — הדפוס המזיק שנמדד
        // בקורפוס (439K מופעים): הפיסוק שובר, לא מדביק.
        assert_eq!(tokenize("שלום,עולם"), vec!["שלום", "עולם"]);
        assert_eq!(tokenize("רעהו,10"), vec!["רעהו", "10"]);
        assert_eq!(tokenize("איפא:5"), vec!["איפא", "5"]);
        assert_eq!(tokenize("א;ב!ג?ד"), vec!["א", "ב", "ג", "ד"]);
        assert_eq!(tokenize("א(ב)ג{ד}ה"), vec!["א", "ב", "ג", "ד", "ה"]);
        // נקודה ו-[] נשארים שקופים: ראשי-תיבות מנוקדים והשלמות עריכתיות.
        assert_eq!(tokenize("פ.ב.י"), vec!["פבי"]);
        assert_eq!(tokenize("יב[ע]ר"), vec!["יבער"]);
    }

    // ── תווים בלתי-נראים (bidi/zero-width) — שקופים ─────────────────────

    #[test]
    fn test_invisible_chars_are_transparent() {
        // נבלעים בלי לשבור — עקבי עם מסלול ה-PDF שמוחק אותם לפני אינדוקס.
        assert_eq!(tokenize("לה\u{FEFF}תיר"), vec!["להתיר"]);
        assert_eq!(tokenize("שלום\u{200F}עולם"), vec!["שלוםעולם"]);
        // ירושלים בכתיב המסורתי — ZWJ בין הלמ"ד לחיריק של הקרי.
        assert_eq!(tokenize("יְרוּשָׁלָ\u{200D}ִם"), vec!["ירושלם"]);
        assert_eq!(tokenize("א\u{202B}ב\u{202C}ג"), vec!["אבג"]);
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

    // ── מצב אינדוקס: טוקן-תאום נטול-גרשיים ──────────────────────────────

    fn tokenize_indexing(text: &str) -> Vec<(String, usize, usize, usize)> {
        let mut tokenizer = HebrewTokenizer {
            emit_quote_free: true,
            ..Default::default()
        };
        let mut stream = tokenizer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            let t = stream.token();
            out.push((t.text.clone(), t.position, t.offset_from, t.offset_to));
        }
        out
    }

    #[test]
    fn indexing_mode_emits_quote_free_twin_same_position() {
        let tokens = tokenize_indexing("רמב\"ם אמר");
        assert_eq!(
            tokens
                .iter()
                .map(|(t, p, _, _)| (t.as_str(), *p))
                .collect::<Vec<_>>(),
            vec![("רמב\"ם", 0), ("רמבם", 0), ("אמר", 1)]
        );
        // התאום יורש את ה-offsets של המילה המקורית — ההדגשה מכסה אותה.
        assert_eq!(tokens[0].2..tokens[0].3, tokens[1].2..tokens[1].3);
    }

    #[test]
    fn indexing_mode_twin_for_geresh_and_plain_words_unaffected() {
        let tokens = tokenize_indexing("תוס' שלום");
        assert_eq!(
            tokens
                .iter()
                .map(|(t, p, _, _)| (t.as_str(), *p))
                .collect::<Vec<_>>(),
            vec![("תוס'", 0), ("תוס", 0), ("שלום", 1)]
        );
    }

    #[test]
    fn query_mode_never_emits_twin() {
        // ברירת המחדל (emit_quote_free=false) — התנהגות היסטורית.
        assert_eq!(tokenize("רמב\"ם"), vec!["רמב\"ם"]);
    }

    // ── הטוקנייזר המנוקד ─────────────────────────────────────────────────

    fn tokenize_vocalized(text: &str) -> Vec<String> {
        let mut tokenizer = HebrewTokenizer {
            keep_marks: true,
            ..Default::default()
        };
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
            "כלת\u{0307}ום מצ\u{0307}ארע",
            "רמח\u{201D}ל תוס\u{2019}",
            "שלום,עולם רעהו,10",
            "לה\u{FEFF}תיר",
        ];
        for s in samples {
            let plain = spans(s);
            let mut tokenizer = HebrewTokenizer {
                keep_marks: true,
                ..Default::default()
            };
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

    // ── בדיקות property (זרע קבוע — דטרמיניסטי, בלי תלות חדשה) ──────────

    /// xorshift64 — מחולל פסאודו-אקראי זעיר ודטרמיניסטי לבדיקות.
    fn xorshift(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    /// אלפבית מלא: אותיות, ספרות, כל צורות הגרשים, מפרידים, שקופים,
    /// בלתי-נראים, ניקוד/טעמים, combining כלליים ו-Presentation Forms.
    const FULL_ALPHABET: &[char] = &[
        'א', 'ב', 'ג', 'ש', 'ת', 'ם', 'ן', '3', '7', '\'', '"', '\u{05F3}', '\u{05F4}', '\u{2018}',
        '\u{2019}', '\u{201C}', '\u{201D}', ' ', ' ', '-', '\u{05BE}', '|', ',', ';', ':', '!',
        '?', '(', ')', '{', '}', '\u{05C0}', '\u{05C3}', '.', '[', ']', '*', '+', '\u{200F}',
        '\u{FEFF}', '\u{05B0}', '\u{05B8}', '\u{05BC}', '\u{0591}', '\u{05A3}', '\u{0307}',
        '\u{0300}', '\u{FB1E}', '\u{FB1D}', '\u{FB2A}', '\u{FB4F}',
    ];

    fn random_string(state: &mut u64, alphabet: &[char], max_len: usize) -> String {
        let len = (xorshift(state) as usize) % max_len;
        (0..len)
            .map(|_| alphabet[(xorshift(state) as usize) % alphabet.len()])
            .collect()
    }

    #[test]
    fn property_token_stream_invariants() {
        // לכל קלט אקראי, בכל ארבעת מצבי הטוקנייזר: טקסט טוקן לא ריק,
        // offsets עולים על גבולות char, אין צורות גרש לא-מקופלות, עמדות
        // עולות (התאום חוזר על עמדת המקור ועל ה-offsets שלו), וגבולות
        // המצב המנוקד זהים לרגיל.
        let mut state = 0x5EED_CAFE_F00D_u64;
        for _ in 0..2_000 {
            let s = random_string(&mut state, FULL_ALPHABET, 24);
            let mut plain_spans: Option<Vec<(usize, usize)>> = None;
            for keep_marks in [false, true] {
                for emit_quote_free in [false, true] {
                    let mut tokenizer = HebrewTokenizer {
                        emit_quote_free,
                        keep_marks,
                    };
                    let mut stream = tokenizer.token_stream(&s);
                    let mut tokens: Vec<Token> = Vec::new();
                    while stream.advance() {
                        tokens.push(stream.token().clone());
                    }
                    for (idx, t) in tokens.iter().enumerate() {
                        assert!(!t.text.is_empty(), "empty token for {s:?}");
                        assert!(
                            t.offset_from < t.offset_to
                                && s.is_char_boundary(t.offset_from)
                                && s.is_char_boundary(t.offset_to),
                            "bad span {:?} for {s:?}",
                            (t.offset_from, t.offset_to)
                        );
                        assert!(
                            !t.text.contains([
                                '\u{05F3}', '\u{05F4}', '\u{2018}', '\u{2019}', '\u{201C}',
                                '\u{201D}'
                            ]) && !t.text.contains("''")
                                && !t.text.contains("\"\""),
                            "unfolded quotes in {:?} for {s:?}",
                            t.text
                        );
                        if idx > 0 {
                            let prev = &tokens[idx - 1];
                            if t.position == prev.position {
                                // טוקן-תאום: רק במצב אינדוקס, יורש offsets,
                                // נטול-גרשיים.
                                assert!(emit_quote_free, "twin outside indexing mode for {s:?}");
                                assert_eq!(
                                    (t.offset_from, t.offset_to),
                                    (prev.offset_from, prev.offset_to)
                                );
                                assert!(!t.text.contains(['\'', '"']));
                            } else {
                                assert_eq!(t.position, prev.position + 1, "position gap for {s:?}");
                                assert!(
                                    t.offset_from >= prev.offset_to,
                                    "overlapping spans for {s:?}"
                                );
                            }
                        }
                    }
                    // גבולות בסיס (בלי תאומים) — זהים בכל המצבים.
                    let spans: Vec<(usize, usize)> = tokens
                        .iter()
                        .zip(std::iter::once(usize::MAX).chain(tokens.iter().map(|t| t.position)))
                        .filter(|(t, prev_pos)| t.position != *prev_pos)
                        .map(|(t, _)| (t.offset_from, t.offset_to))
                        .collect();
                    match &plain_spans {
                        None => plain_spans = Some(spans),
                        Some(reference) => {
                            assert_eq!(reference, &spans, "boundaries diverged for {s:?}")
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn vocalized_transparent_still_dropped() {
        // פיסוק שקוף מושמט מהטוקן גם במצב מנוקד.
        assert_eq!(tokenize_vocalized("בָּרָא."), vec!["בָּרָא"]);
    }
}
