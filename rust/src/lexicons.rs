//! מילוני הרחבה לחיפוש המתקדם.
//!
//! שני מילונים חיים כאן, שניהם קריאה-בלבד אחרי הטעינה ושניהם ממפתחים
//! בצורת טרם-אינדקס (ללא ניקוד/טעמים, אותיות סופיות נשמרות) כדי שההרחבות
//! ניתנות להזרקה ישירה לשאילתה:
//!
//! * [`TranslationLexicon`] — תרגום ארמי↔עברי ברמת מילה בודדת (ראו למטה).
//! * [`AcronymLexicon`] — פענוח ראשי-תיבות דו-כיווני: ר"ת↔ביטוי רב-מילים.
//!   בניגוד לתרגום, הפענוח **רב-מילי** (`רמב"ם`↔`רבי משה בן מיימון`) ולכן
//!   אינו נכנס לערוץ ה-`alternative_words` החד-מילתי אלא נצרך כתת-שאילתת
//!   OR (ראו `SearchEngine::acronym_alternatives`). מפתח ה-ר"ת נשמר בצורתו
//!   נטולת-הגרשיים (`רמבם`), שמותאמת באינדקס לשתי הצורות דרך הטוקן-התאום.
//!
//! [`TranslationLexicon`] טוען את מילוני הארמית-עברית של אוצריא
//! (`assets/dictionary.json` בצד האפליקציה) ומגזר מהם מפת תרגומים
//! דו-כיוונית ברמת מילה בודדת. הקובץ מכיל כמה מילונים ("מילון פשיטא",
//! "מילון שיח ישראל", "מפירוש אונקלוס"...) — כולם ממוזגים. פורמט:
//!
//! ```json
//! { "מילון פשיטא": [ { "אִיתָא": "יש" }, { "אברא": "{אַבָּרָא} אמת. *** {אֲבָרָא } אבר, עופרת." } ] }
//! ```
//!
//! ערך הביאור עשוי לשאת סימון פנימי: בלוקי `{...}` (הצורה המנוקדת), סוגריים
//! `(...)` (הערות), ו-`***` שמפריד משמעויות. משמעות מפורקת גם על פסיקים
//! ונקודות; רק פריט שהוא **מילה בודדת** אחרי הניקוי נכנס למפה — ביאור
//! רב-מילי ("על שוכריהם") אינו תרגום ברמת מילה ולכן מדולג.
//!
//! המפות מפתחיהן וערכיהן בצורת טרם-אינדקס (ללא ניקוד, אותיות סופיות
//! נשמרות), כך שההרחבות ניתנות להזרקה ישירה כמילים-חלופיות של השאילתה.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::hebrew_query::{normalize_for_index, split_query_words, strip_quote_chars};

/// תקרת תרגומים שמוזרקים למילה אחת (שני הכיוונים יחד). מעבר לה — נחתך;
/// הערכים הראשונים במילון מנצחים (סדר הקובץ).
pub const MAX_TRANSLATION_EXPANSIONS: usize = 16;

/// מילון תרגום ארמי↔עברי דו-כיווני, קריאה-בלבד אחרי הטעינה.
pub struct TranslationLexicon {
    /// ערך-ראש ארמי → מילות התרגום העבריות שלו.
    forward: HashMap<String, Vec<String>>,
    /// מילה עברית → ערכי-הראש הארמיים שהיא מתרגמת.
    reverse: HashMap<String, Vec<String>>,
}

impl TranslationLexicon {
    /// טוען את קובץ ה-JSON. שגיאת קריאה/פורמט מוחזרת כ-`Err`; קובץ תקין
    /// בלי אף זוג שמיש נחשב תקין (מילון ריק).
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading translation dictionary at {}", path.display()))?;
        let json: Value =
            serde_json::from_str(&raw).context("translation dictionary is not valid JSON")?;
        // המפתחות העליונים הם שמות מילונים ("מילון פשיטא", "מילון שיח
        // ישראל"...) — כולם ממוזגים. חשוב: לא "הערך הראשון" — מפת
        // serde_json ממוינת אלפביתית (אין preserve_order), כך שלקיחת
        // הראשון טענה בפועל את "מושגים ואישים" (ערך יחיד) והפילה את כל
        // התרגומים בשקט.
        let dictionaries = json
            .as_object()
            .context("translation dictionary: expected a top-level {name: [entries]} object")?;
        let entries = dictionaries.values().filter_map(Value::as_array).flatten();

        let mut forward: HashMap<String, Vec<String>> = HashMap::new();
        let mut reverse: HashMap<String, Vec<String>> = HashMap::new();
        for entry in entries {
            let Some(map) = entry.as_object() else {
                continue;
            };
            for (headword, gloss) in map {
                let Some(gloss) = gloss.as_str() else {
                    continue;
                };
                let head = normalize_entry_word(headword);
                if head.is_empty() {
                    continue;
                }
                for word in gloss_translation_words(gloss) {
                    push_capped(&mut forward, &head, &word);
                    push_capped(&mut reverse, &word, &head);
                }
            }
        }
        Ok(Self { forward, reverse })
    }

    /// תרגומי המילה בשני הכיוונים (ארמי→עברי ועברי→ארמי), בסדר המילון,
    /// ללא המילה עצמה, עד `cap` פריטים.
    pub fn expansions(&self, token: &str, cap: usize) -> Vec<String> {
        let key = normalize_entry_word(token);
        let mut out: Vec<String> = Vec::new();
        for list in [self.forward.get(&key), self.reverse.get(&key)]
            .into_iter()
            .flatten()
        {
            for word in list {
                if *word != key && !out.contains(word) {
                    out.push(word.clone());
                    if out.len() >= cap {
                        return out;
                    }
                }
            }
        }
        out
    }

    /// מספר ערכי-הראש (לצורכי לוג/בדיקות).
    pub fn len(&self) -> usize {
        self.forward.len()
    }
}

fn push_capped(map: &mut HashMap<String, Vec<String>>, key: &str, value: &str) {
    if key == value {
        return;
    }
    let list = map.entry(key.to_string()).or_default();
    if list.len() < MAX_TRANSLATION_EXPANSIONS && !list.iter().any(|v| v == value) {
        list.push(value.to_string());
    }
}

/// מנרמל מילת-ערך לצורת טרם-אינדקס: הסרת ניקוד/טעמים וקיפולים, בלי
/// גרש/גרשיים (שאינם חלק ממילות המילון הזה).
fn normalize_entry_word(word: &str) -> String {
    normalize_for_index(word.trim())
}

/// מפרק ביאור למילות תרגום בודדות: מסיר בלוקי `{...}` ו-`(...)`, מפצל על
/// `***`, פסיקים, נקודות ונקודה-פסיק, ושומר רק פריטים בני מילה אחת.
fn gloss_translation_words(gloss: &str) -> Vec<String> {
    let mut cleaned = String::with_capacity(gloss.len());
    let mut depth_curly = 0usize;
    let mut depth_paren = 0usize;
    for c in gloss.chars() {
        match c {
            '{' => depth_curly += 1,
            '}' => depth_curly = depth_curly.saturating_sub(1),
            '(' => depth_paren += 1,
            ')' => depth_paren = depth_paren.saturating_sub(1),
            _ if depth_curly == 0 && depth_paren == 0 => cleaned.push(c),
            _ => {}
        }
    }

    let mut out = Vec::new();
    for meaning in cleaned.split("***") {
        for piece in meaning.split([',', ';', '.', ':']) {
            let word = piece.trim();
            if word.is_empty() || word.split_whitespace().count() != 1 {
                continue;
            }
            let normalized = normalize_entry_word(word);
            if !normalized.is_empty() && !out.contains(&normalized) {
                out.push(normalized);
            }
        }
    }
    out
}

// ── פענוח ראשי-תיבות (דו-כיווני) ─────────────────────────────────────────────

/// תקרת פענוחים שנשקלים לר"ת אחד (או ר"תים לביטוי אחד). מעבר לה — נחתך;
/// הפריטים הראשונים בקובץ מנצחים.
pub const MAX_ACRONYM_EXPANSIONS: usize = 16;

/// מילון פענוח ראשי-תיבות דו-כיווני, קריאה-בלבד אחרי הטעינה.
///
/// פורמט הקובץ (`assets/Acronyms.json` בצד האפליקציה): מפה שטוחה של ר"ת →
/// רשימת פענוחים אפשריים (רב-משמעי):
///
/// ```json
/// { "א\"ב": ["אין בו", "איכא בנייהו", "איסורי ביאה"], "רמב\"ם": ["רבי משה בן מיימון"] }
/// ```
pub struct AcronymLexicon {
    /// ר"ת נטול-גרשיים → פענוחיו, כל פענוח כרשימת מילים בצורת טרם-אינדקס.
    forward: HashMap<String, Vec<Vec<String>>>,
    /// פענוח (מילים מחוברות ברווח) → ר"תים נטולי-גרשיים שהוא פותח.
    reverse: HashMap<String, Vec<String>>,
}

impl AcronymLexicon {
    /// טוען את קובץ ה-JSON. שגיאת קריאה/פורמט מוחזרת כ-`Err`; קובץ תקין
    /// בלי אף זוג שמיש נחשב תקין (מילון ריק).
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading acronyms dictionary at {}", path.display()))?;
        let json: Value =
            serde_json::from_str(&raw).context("acronyms dictionary is not valid JSON")?;
        let obj = json
            .as_object()
            .context("acronyms dictionary: expected a top-level {acronym: [expansions]} object")?;

        let mut forward: HashMap<String, Vec<Vec<String>>> = HashMap::new();
        let mut reverse: HashMap<String, Vec<String>> = HashMap::new();
        for (acronym, expansions) in obj {
            // מפתח הר"ת נשמר נטול-גרשיים — הצורה שהאינדקס מטמיע לכל ר"ת
            // (הטוקן-התאום) ושאליה השאילתה מנרמלת.
            let key = strip_quote_chars(&normalize_for_index(acronym));
            if key.is_empty() {
                continue;
            }
            let Some(list) = expansions.as_array() else {
                continue;
            };
            for expansion in list {
                let Some(text) = expansion.as_str() else {
                    continue;
                };
                // מילות הפענוח בצורת טרם-אינדקס, בדיוק כמו שיפוצל טוקן שאילתה.
                let words = split_query_words(&normalize_for_index(text));
                // פענוח חד-מילי אינו מוסיף מעבר להתאמת הטוקן-התאום שכבר
                // באינדקס — מדולג כדי לא לנפח את המילון בלי תועלת.
                if words.len() < 2 {
                    continue;
                }
                push_expansion(&mut forward, &key, &words);
                let joined = words.join(" ");
                push_capped(&mut reverse, &joined, &key);
            }
        }
        Ok(Self { forward, reverse })
    }

    /// כיוון א' (ר"ת→פענוח): פענוחי הר"ת, כל אחד כרשימת מילים, עד `cap`.
    /// `token` צפוי כבר בצורת טרם-אינדקס; גרשיים שנותרו מוסרים ליתר ביטחון.
    pub fn expand(&self, token: &str, cap: usize) -> Vec<Vec<String>> {
        let key = strip_quote_chars(token);
        self.forward
            .get(&key)
            .map(|list| list.iter().take(cap).cloned().collect())
            .unwrap_or_default()
    }

    /// כיוון ב' (פענוח→ר"ת): הר"תים שרצף המילים הנתון פותח, עד `cap`.
    /// `words` צפויות בצורת טרם-אינדקס (כמו פלט `split_query_words`).
    pub fn acronyms_for(&self, words: &[String], cap: usize) -> Vec<String> {
        if words.len() < 2 {
            return Vec::new();
        }
        let joined = words.join(" ");
        self.reverse
            .get(&joined)
            .map(|list| list.iter().take(cap).cloned().collect())
            .unwrap_or_default()
    }

    /// מספר הר"תים במילון (לצורכי לוג/בדיקות).
    pub fn len(&self) -> usize {
        self.forward.len()
    }
}

/// מוסיף פענוח למפת ה-forward, ללא כפילויות ועד [`MAX_ACRONYM_EXPANSIONS`].
fn push_expansion(map: &mut HashMap<String, Vec<Vec<String>>>, key: &str, words: &[String]) {
    let list = map.entry(key.to_string()).or_default();
    if list.len() < MAX_ACRONYM_EXPANSIONS && !list.iter().any(|w| w == words) {
        list.push(words.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn lexicon_from(json: &str) -> TranslationLexicon {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(json.as_bytes()).unwrap();
        TranslationLexicon::load(file.path()).unwrap()
    }

    const SAMPLE: &str = r#"{
        "מילון פשיטא": [
            { "אִיתָא": "יש" },
            { "אבא": "יער" },
            { "אאגורייהו": "{אַאָגוּרַיְיהוּ} על שוכריהם" },
            { "אברא": "{אַבָּרָא} אמת. *** {אֲבָרָא } אבר, עופרת." }
        ]
    }"#;

    #[test]
    fn loads_and_translates_both_directions() {
        let lex = lexicon_from(SAMPLE);
        // ערך-ראש מנוקד מנורמל; ארמי→עברי.
        assert_eq!(lex.expansions("איתא", 16), vec!["יש"]);
        // עברי→ארמי.
        assert_eq!(lex.expansions("יש", 16), vec!["איתא"]);
        assert_eq!(lex.expansions("יער", 16), vec!["אבא"]);
        // ריבוי משמעויות (***) ופיצול פסיקים.
        assert_eq!(lex.expansions("אברא", 16), vec!["אמת", "אבר", "עופרת"]);
        assert_eq!(lex.expansions("עופרת", 16), vec!["אברא"]);
    }

    #[test]
    fn multiword_glosses_are_skipped() {
        let lex = lexicon_from(SAMPLE);
        // "על שוכריהם" — ביאור רב-מילי, לא תרגום ברמת מילה.
        assert!(lex.expansions("אאגורייהו", 16).is_empty());
        assert!(lex.expansions("שוכריהם", 16).is_empty());
    }

    #[test]
    fn cap_and_unknown_token() {
        let lex = lexicon_from(SAMPLE);
        assert_eq!(lex.expansions("אברא", 1), vec!["אמת"]);
        assert!(lex.expansions("מילה־שאיננה", 16).is_empty());
    }

    #[test]
    fn all_dictionaries_are_merged() {
        // רגרסיה: dictionary.json האמיתי מכיל כמה מילונים, ומפת serde_json
        // ממוינת אלפביתית — "מושגים ואישים" (ו' לפני י') קודם ל"מילון
        // פשיטא". לקיחת המילון "הראשון" בלבד טענה אותו במקום את התרגומים,
        // וכל האפשרות מתה בשקט (הכא↔כאן לא עבד).
        let lex = lexicon_from(
            r#"{
                "מילון פשיטא": [ { "הכא": "כאן" } ],
                "מילון שיח ישראל": [ { "התם": "שם" } ],
                "מושגים ואישים": [ { "אביי": "אמורא בדור הרביעי" } ]
            }"#,
        );
        // נטען מכל המילונים, לא רק מהראשון-אלפביתית.
        assert_eq!(lex.expansions("הכא", 16), vec!["כאן"]);
        assert_eq!(lex.expansions("כאן", 16), vec!["הכא"]);
        assert_eq!(lex.expansions("שם", 16), vec!["התם"]);
        // ביאור רב-מילי ("אמורא בדור הרביעי") עדיין מדולג.
        assert!(lex.expansions("אביי", 16).is_empty());
    }

    #[test]
    fn load_rejects_garbage() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"not json").unwrap();
        assert!(TranslationLexicon::load(file.path()).is_err());
    }

    // ── AcronymLexicon ──────────────────────────────────────────────────────

    fn acronyms_from(json: &str) -> AcronymLexicon {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(json.as_bytes()).unwrap();
        AcronymLexicon::load(file.path()).unwrap()
    }

    const ACR_SAMPLE: &str = r#"{
        "א\"ב": ["אין בו", "איכא בנייהו", "איסורי ביאה"],
        "רמב\"ם": ["רבי משה בן מיימון"],
        "וכו'": ["וכולי"]
    }"#;

    #[test]
    fn acronym_expands_forward_bidirectional() {
        let lex = acronyms_from(ACR_SAMPLE);
        // כיוון א' — ר"ת (בצורה נטולת-גרשיים, כפי שהשאילתה מנרמלת) → פענוחים.
        assert_eq!(
            lex.expand("אב", 16),
            vec![
                vec!["אין".to_string(), "בו".to_string()],
                vec!["איכא".to_string(), "בנייהו".to_string()],
                vec!["איסורי".to_string(), "ביאה".to_string()],
            ]
        );
        // גרשיים שנותרו בטוקן מוסרים בעת החיפוש.
        assert_eq!(lex.expand("א\"ב", 16), lex.expand("אב", 16));
        assert_eq!(
            lex.expand("רמבם", 16),
            vec![vec![
                "רבי".to_string(),
                "משה".to_string(),
                "בן".to_string(),
                "מיימון".to_string()
            ]]
        );
    }

    #[test]
    fn acronym_reverse_phrase_to_acronym() {
        let lex = acronyms_from(ACR_SAMPLE);
        // כיוון ב' — רצף מילות הפענוח → הר"ת נטול-הגרשיים.
        let words = ["איכא".to_string(), "בנייהו".to_string()];
        assert_eq!(lex.acronyms_for(&words, 16), vec!["אב"]);
        let words = [
            "רבי".to_string(),
            "משה".to_string(),
            "בן".to_string(),
            "מיימון".to_string(),
        ];
        assert_eq!(lex.acronyms_for(&words, 16), vec!["רמבם"]);
    }

    #[test]
    fn acronym_single_word_expansion_skipped() {
        // פענוח חד-מילי ("וכולי") אינו מוסיף מעבר לטוקן-התאום — מדולג.
        let lex = acronyms_from(ACR_SAMPLE);
        assert!(lex.expand("וכו", 16).is_empty());
        assert!(lex.acronyms_for(&["וכולי".to_string()], 16).is_empty());
    }

    #[test]
    fn acronym_cap_and_unknown() {
        let lex = acronyms_from(ACR_SAMPLE);
        assert_eq!(
            lex.expand("אב", 1),
            vec![vec!["אין".to_string(), "בו".to_string()]]
        );
        assert!(lex.expand("שאיננו", 16).is_empty());
        assert!(lex
            .acronyms_for(&["מילה".to_string(), "אקראית".to_string()], 16)
            .is_empty());
    }

    #[test]
    fn acronym_load_rejects_garbage() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"not json").unwrap();
        assert!(AcronymLexicon::load(file.path()).is_err());
    }
}
