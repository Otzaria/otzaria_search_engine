# MagicDictionary → otzaria_search_engine — מסמך מיגרציה

מסמך זה מתאר איך לשלב את מנגנון ההרחבה המורפולוגית **MagicDictionary** של
[kdroidFilter/SeforimLibrary](https://github.com/kdroidFilter/SeforimLibrary)
ו-[kdroidFilter/SeforimMagicIndexer](https://github.com/kdroidFilter/SeforimMagicIndexer)
לתוך **otzaria_search_engine** (Rust/Tantivy עם Flutter UI).

---

> ### ⚠️ עדכון אימות (2026-06-23)
> המסמך אומת מול הקוד בפועל (מקומי + מקורות חיצוניים). שינויים שחלו מאז הכתיבה המקורית:
> - **הריפו `SeforimLibraryLM` שונה שמו ל-`SeforimLibrary`**, וענף ברירת המחדל הוא `master` (לא `main`). כל הקישורים במסמך עודכנו בהתאם. גם ל-`SeforimMagicIndexer` ענף ברירת המחדל הוא `master`.
> - **ה-cap `MAX_SYNONYM_BOOST_TERMS` הוא כעת `256`** (לא 8), ונוסף קבוע `MAX_SYNONYM_TERMS_PER_TOKEN = 32`. ראה חלקים 3, 4.5, 5.
> - **חלק 4.5 נכתב מחדש** כדי לתאום את הארכיטקטורה בפועל: `build_query` היא מתודה (`&self`), הקלט הוא `regex_terms` (דפוסי regex, לא טוקנים גולמיים), והמנוע **כבר** מבצע הרחבה מורפולוגית דרך [hebrew_query.rs](rust/src/hebrew_query.rs). זו נקודת השילוב הטבעית למילון.
> - גרסת Tantivy בפועל: `=0.26.1`. ה-release האחרון של ה-DB: `v0.3.0` (2026-04-26), `lexical.db` ~54.5MB.
> - **אומת כנכון:** סכמת ה-DB, `LOOKUP_SQL`, ערכי ה-boost (2.0/1.5/1.0), הבלאקליסט (25 entries, highlight-only), `buildLookupCandidates`, ו-`normalizeHebrew`.

---

## TL;DR

- **המטרה:** חיפוש בעברית שמוצא צורות הטיה (למה→ההטיות) — "הלך" יחזיר גם "הלכתי", "הולך", "תלך"…
- **הנכס:** קובץ SQLite בשם `lexical.db` שנבנה offline ע"י Gemini AI על קורפוס Sefaria+Otzaria.
- **גודל בפועל:** 54 MB. מכיל 24,559 למות, 137,631 צורות שטח, 190,151 וריאנטים, 594,428 קישורים.
- **המיגרציה אפשרית.** ~250 שורות Rust. נקודת השילוב היא [hebrew_query.rs](rust/src/hebrew_query.rs) (בניית `regex_terms`), לא `build_query` ישירות — ראה חלק 4.5.
- **צד Flutter/Dart לא צריך להשתנות** אם משלבים פנימית.
- **רישיון:** AGPL-3.0 בשני המקורות — תאים לפרויקט יעד GPL/AGPL.

---

## חלק 1 — מה זה MagicDictionary (ולמה זה לא טוקנייזר)

ה"טוקנייזר" שהמפתח מתייחס אליו הוא בעצם **שני רכיבים שונים**:

| שלב | מתי רץ | מה הוא עושה | תלוי ב-AI? |
|---|---|---|---|
| **SeforimMagicIndexer** | Offline, פעם אחת | קורא ל-Gemini API לכל מילה בקורפוס ומפיק `LexicalEntry(surface, base, variants)` | כן — GEMINI_API_KEY |
| **MagicDictionaryIndex** | Runtime, בכל שאילתה | SQLite read-only lookup של הצורות שכבר נוצרו | לא |

**משמעות:** המוצר הסופי (`lexical.db`) הוא רק SQLite. ב-Rust רק צריך לקרוא ממנו עם `rusqlite`. אין AI calls בזמן ריצה.

### זרימה
```
SeforimMagicIndexer + Gemini
    ↓ (offline, slow, expensive)
lexical.db (SQLite, ~54.5 MB)
    ↓ (downloaded once)
otzaria_search_engine קורא בזמן חיפוש
```

---

## חלק 2 — סכמת `lexical.db`

ארבע טבלאות עיקריות. הסכמה המלאה ב-[Database.sq](https://github.com/kdroidFilter/SeforimMagicIndexer/blob/master/core/src/commonMain/sqldelight/io/github/kdroidfilter/seforim/magicindexer/db/Database.sq):

```sql
-- למה (root form) - "הלך"
CREATE TABLE base (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  value TEXT NOT NULL UNIQUE
);

-- צורות שטח (כל ההטיות הנפוצות) - "הלכתי", "הולך", "תלך"
CREATE TABLE surface (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  value TEXT NOT NULL UNIQUE,
  base_id INTEGER NOT NULL REFERENCES base(id),
  notes TEXT
);

-- וריאנטים (כתיב חלופי, ניקוד אחר, וכו')
CREATE TABLE variant (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  value TEXT NOT NULL UNIQUE
);

-- many-to-many בין surfaces ל-variants
CREATE TABLE surface_variant (
  surface_id INTEGER NOT NULL REFERENCES surface(id),
  variant_id INTEGER NOT NULL REFERENCES variant(id),
  PRIMARY KEY (surface_id, variant_id)
);
```

### השאילתה הקריטית (`LOOKUP_SQL`)

לטוקן נתון, מחזיר את כל הצורות המקושרות דרך אותה למה:

```sql
WITH matches AS (
    SELECT s.base_id FROM surface s WHERE s.value = ?
    UNION
    SELECT b.id      FROM base b    WHERE b.value = ?
    UNION
    SELECT s.base_id FROM variant v
      JOIN surface_variant sv ON sv.variant_id = v.id
      JOIN surface s          ON sv.surface_id  = s.id
      WHERE v.value = ?
)
SELECT b.id as base_id, b.value as base,
       s.value as surface, v.value as variant
FROM base b
JOIN matches m       ON m.base_id     = b.id
LEFT JOIN surface s  ON s.base_id     = b.id
LEFT JOIN surface_variant sv ON sv.surface_id = s.id
LEFT JOIN variant v  ON sv.variant_id = v.id;
```

3 הסימני שאלה מקבלים **את אותה מילה** (הראשונה — מחפש לפי surface, השנייה — לפי base, השלישית — לפי variant).

---

## חלק 3 — איך זה משולב במנוע המקורי (Lucene/Kotlin)

ב-[LuceneSearchEngine.kt](https://github.com/kdroidFilter/SeforimLibrary/blob/master/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt) בכל שאילתה:

1. **נירמול עברית** עם `HebrewTextUtils.normalizeHebrew` (ניקוד, טעמים, גרשיים, סופיות).
2. **טוקניזציה** עם StandardAnalyzer של Lucene.
3. **לכל טוקן:** קריאה ל-`magicDict.expansionFor(token)` → מקבל `Expansion(surface[], variants[], base[])`.
4. **שלוש שאילתות Lucene נפרדות** נבנות מהאותן הרחבות:

| שאילתה | תפקיד | מבנה |
|---|---|---|
| `buildPresenceFilterForTokens` | `FILTER` — חובה | לכל טוקן: `(token OR surface₁ OR surface₂ OR ...) MUST` |
| `buildSynonymPhraseQuery` | פראזה גמישה | `MultiPhraseQuery` — בכל מיקום מותר variant |
| `buildMagicBoostQuery` | דירוג | `SHOULD` עם boosts: surface=**2.0**, variants=**1.5**, base=**1.0** |

ה-3 מורכבות ל-`BooleanQuery` סופי.

### מגבלות וזהירויות בקוד המקורי

- **Cap על מספר ההרחבות:** `MAX_SYNONYM_BOOST_TERMS = 256` ו-`MAX_SYNONYM_TERMS_PER_TOKEN = 32` (משתנים גלובליים, [שורות 42-44](https://github.com/kdroidFilter/SeforimLibrary/blob/master/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt#L42-L44)). (בגרסה המקורית של מסמך זה צוין 8 — הערך עלה מאז.)
- **Hallucination blacklist:** AI לפעמים יוצר מיפויים שגויים. ראה [שורות 50-95](https://github.com/kdroidFilter/SeforimLibrary/blob/master/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt#L50-L95) — בלאקליסט ידני שמסונן רק להיילייטינג (לא לחיפוש עצמו, כדי לשמור recall).
- **סטופ-וורדס:** אותיות בודדות מסוננות *לפני* ההרחבה, פרט ל-"ה" אם השאילתה כללה "ה׳" (Hashem).
- **קאשים:** LRU של 1024 tokens + 512 bases בזמן ריצה.

---

## חלק 4 — תוכנית המיגרציה ל-Tantivy

### 4.1 קבצים חדשים

```
rust/src/api/magic/
├── mod.rs              -- pub use של הרכיבים
├── normalize.rs        -- port של HebrewTextUtils (~80 LOC)
├── dictionary.rs       -- port של MagicDictionaryIndex (~150 LOC)
├── blacklist.rs        -- בלאקליסט הזיות (~30 LOC)
└── downloader.rs       -- הורדת lexical.db מ-GitHub Releases (~80 LOC)
```

### 4.2 תוספות ל-Cargo.toml

```toml
[dependencies]
rusqlite = { version = "0.32", features = ["bundled"] }  # bundled = ללא תלות מערכת ב-libsqlite
lru = "0.12"
regex = "1.10"
reqwest = { version = "0.12", features = ["json", "blocking"] }  # רק אם הורדה ב-Rust
```

**הערה לאנדרואיד:** `rusqlite` עם `bundled` עובד טוב על Android (Tantivy שלך כבר משתמש ב-libc native build).

### 4.3 `normalize.rs` — נירמול עברית

נדרש לנרמל את ה-input של המשתמש לאותה צורה שבה הוטמע ה-DB:

```rust
use regex::Regex;
use once_cell::sync::Lazy;

// טעמים U+0591–U+05AF
static TEAMIM: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\u{0591}-\u{05AF}]").unwrap());
// ניקוד + meteg + qamatz qatan.
// הערה: ה-`normalizeHebrew` המקורי ב-Kotlin מסיר את הטווח 05B0–05BD + 05C1,05C2,05C7
// אבל **לא** את 05BF (rafe). השארנו 05BF כאן כי הוא נדיר בקורפוס; אם רוצים התאמה
// מדויקת 1:1 ל-DB — להסיר את \u{05BF} מהטווח.
static NIKUD:  Lazy<Regex> = Lazy::new(|| Regex::new(
    r"[\u{05B0}-\u{05BD}\u{05BF}\u{05C1}\u{05C2}\u{05C7}]"
).unwrap());

pub fn normalize_hebrew(s: &str) -> String {
    let s = TEAMIM.replace_all(s.trim(), "");
    let s = NIKUD.replace_all(&s, "");
    let s = s.replace('\u{05BE}', " ");           // maqaf → space
    let s = s.replace('\u{05F4}', "")             // gershayim
             .replace('\u{05F3}', "");            // geresh
    let s = replace_finals_with_base(&s);
    // collapse whitespace
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn replace_finals_with_base(s: &str) -> String {
    s.chars().map(|c| match c {
        'ך' => 'כ', 'ם' => 'מ', 'ן' => 'נ', 'ף' => 'פ', 'ץ' => 'צ',
        c => c,
    }).collect()
}
```

### 4.4 `dictionary.rs` — שכבת ה-SQLite

```rust
use rusqlite::{Connection, params};
use lru::LruCache;
use std::sync::Mutex;
use std::num::NonZeroUsize;
use std::path::Path;

pub struct Expansion {
    pub surface:  Vec<String>,
    pub variants: Vec<String>,
    pub base:     Vec<String>,
}

pub struct MagicDictionary {
    conn:  Mutex<Connection>,
    cache: Mutex<LruCache<String, Vec<Expansion>>>,
}

const LOOKUP_SQL: &str = r#"
    WITH matches AS (
        SELECT s.base_id FROM surface s WHERE s.value = ?
        UNION
        SELECT b.id FROM base b WHERE b.value = ?
        UNION
        SELECT s.base_id FROM variant v
          JOIN surface_variant sv ON sv.variant_id = v.id
          JOIN surface s ON sv.surface_id = s.id
          WHERE v.value = ?
    )
    SELECT b.id as base_id, b.value as base,
           s.value as surface, v.value as variant
    FROM base b
    JOIN matches m ON m.base_id = b.id
    LEFT JOIN surface s ON s.base_id = b.id
    LEFT JOIN surface_variant sv ON sv.surface_id = s.id
    LEFT JOIN variant v ON sv.variant_id = v.id
"#;

impl MagicDictionary {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.execute_batch("PRAGMA query_only=ON;")?;
        // עבור Desktop: cache=512. עבור Android: cache=128 (RAM-conscious).
        let cache_size = if cfg!(target_os = "android") { 128 } else { 512 };
        Ok(Self {
            conn:  Mutex::new(conn),
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(cache_size).unwrap())),
        })
    }

    pub fn expansion_for(&self, token: &str) -> Option<Expansion> {
        let normalized = crate::api::magic::normalize::normalize_hebrew(token);
        if normalized.is_empty() { return None; }

        if let Some(cached) = self.cache.lock().unwrap().get(&normalized) {
            return cached.iter().max_by_key(|e| e.surface.len()).cloned();
        }

        let expansions = self.fetch_expansions(&normalized)?;
        let best = expansions.iter().max_by_key(|e| e.surface.len()).cloned();
        self.cache.lock().unwrap().put(normalized, expansions);
        best
    }

    fn fetch_expansions(&self, token: &str) -> Option<Vec<Expansion>> {
        // ... יישום של LOOKUP_SQL + צבירה לפי base_id
        // ... כולל candidates של final form (סופיות)
        todo!()
    }
}
```

### 4.5 שילוב במנוע — נקודת השילוב הנכונה היא `hebrew_query.rs`

> **חשוב — תיקון אדריכלי מהגרסה המקורית.** הגרסה המקורית של חלק זה הניחה ש-`build_query`
> היא פונקציה חופשית שמקבלת טוקנים גולמיים, ושמסלול הטוקן-היחיד הוא
> `RegexQuery::from_pattern`. **שני הדברים שגויים בקוד בפועל.** ראה למטה את המצב האמיתי.

#### מה קורה בפועל היום

1. **`build_query` היא מתודה** (`&self`) ב-[search_engine.rs:1185](rust/src/api/search_engine.rs#L1185) — לא פונקציה חופשית, ויש לה גישה ל-`self.index_reader`.
2. **הקלט שלה הוא `regex_terms: Vec<String>` — דפוסי regex, לא מילים גולמיות.** מסלול הטוקן-היחיד הוא [single_regex_term_query](rust/src/api/search_engine.rs#L1228), שמהדר את ה-regex עם `tantivy_fst::Regex`, מזרים את כל מונחי האינדקס התואמים, ובונה מהם `TermSetQuery` (עם אכיפת `max_expansions`). מסלול רב-טוקני בונה `RegexPhraseQuery`.
3. **המנוע כבר מבצע הרחבה מורפולוגית** — ה-`regex_terms` נבנים ב-[hebrew_query.rs](rust/src/hebrew_query.rs) ע"י `create_search_pattern`, שמרכיב לכל מילה דפוס regex לפי האפשרויות: קידומות/סיומות (דקדוקיות וכלליות), כתיב מלא/חסר, חלק-ממילה, ושגיאות-כתיב. נקודת הכניסה: [build_advanced_query → hebrew_query::prepare_advanced_query](rust/src/api/search_engine.rs#L1377).

המשמעות: ה"הרחבה" כאן היא **מבוססת-כללים** (regex של אותיות שימוש), בעוד MagicDictionary היא **מבוססת-לקסיקון** (צורות שטח אמיתיות שנלמדו). אלו שני מנגנונים משלימים, והמילון נכנס בדיוק באותו שלב שבו נבנים ה-`regex_terms`.

#### הגישה המומלצת: הזרקת צורות לקסיקליות לתוך ה-regex term

במקום לגעת ב-`build_query`, להוסיף ל-[hebrew_query.rs](rust/src/hebrew_query.rs) מסלול שמרחיב מילה
לרשימת צורות מהמילון, ואז משלב אותן כ-alternation בתוך אותו דפוס regex שכבר נבנה:

```rust
// בתוך hebrew_query.rs, כאשר magic_dict זמין ומופעל עבור המילה:
fn create_lexical_pattern(word: &str, dict: &MagicDictionary) -> Option<String> {
    let exp = dict.expansion_for(word)?;            // surface[] + variants[] + base[]
    // איחוד הצורות (כולל המילה עצמה), נירמול, escape, וקיבוע ל-anchored alternation
    let mut forms: Vec<String> = std::iter::once(word.to_string())
        .chain(exp.surface)
        .chain(exp.variants)
        .chain(exp.base)
        .map(|f| escape_regex(&normalize_for_index(&f)))
        .collect();
    forms.sort();
    forms.dedup();
    forms.truncate(MAX_LEXICAL_FORMS);              // cap — ראה הערה למטה
    Some(format!("(?:{})", forms.join("|")))
}
```

- הדפוס הזה מתמזג עם הדפוסים הקיימים (קידומת/סיומת/כתיב) בדיוק כמו כל וריאנט אחר ב-`create_search_pattern`.
- מכיוון שהמנוע ממילא מהדר regex→`TermSetQuery`/`RegexPhraseQuery` ואוכף `max_expansions`, **אין צורך לבנות `BooleanQuery` ידני עם boosts** — וגם אין דירוג boost מובנה במסלול ה-regex של Tantivy. אם רוצים את הדירוג surface=2.0/variant=1.5/base=1.0 כמו ב-Lucene, צריך מסלול נפרד (ראה חלק 5, אופציה A/C) — לא חלק מהשלב הראשון.

#### היכן מזריקים את `magic_dict`

`MagicDictionary` הוא נכס קריאה-בלבד לכל אורך חיי המנוע. נכון לאחסן אותו על `SearchEngine`
עצמו (כמו `index_reader`), ולהעביר reference דרך `prepare_advanced_query`:

```rust
// search_engine.rs — שדה חדש על המבנה:
struct SearchEngine {
    // ... index, index_reader, schema ...
    magic_dict: Option<MagicDictionary>,   // נטען ב-open() אם lexical.db קיים
}
```

כך **חתימות ה-API החיצוניות (`search`/`count`/...) לא משתנות כלל**, וצד ה-Dart לא מושפע (ראה חלק 8).

> **הערה על cap:** הערכים בקוד ה-Lucene המקורי (`MAX_SYNONYM_BOOST_TERMS = 256`,
> `MAX_SYNONYM_TERMS_PER_TOKEN = 32`) נוגעים ל-boost-queries של Lucene ולא רלוונטיים ישירות
> כאן. במסלול ה-regex, ה-cap האפקטיבי הוא `max_expansions` של המנוע (שכבר נאכף ב-`single_regex_term_query`).
> מומלץ `MAX_LEXICAL_FORMS` נמוך משלו (למשל 32) כדי לא לנפח את ה-alternation.

---

## חלק 5 — הבדל אדריכלי קריטי: אין `MultiPhraseQuery` ב-Tantivy

ב-Lucene, `MultiPhraseQuery` מאפשרת **בכל מיקום בפראזה לבחור מבין n חלופות**.
ב-Tantivy זה לא קיים כפיצ'ר ישיר.

> **הערה חשובה (אומת בקוד):** המנוע **כבר משתמש ב-`RegexPhraseQuery`** למסלול הרב-טוקני
> ([build_query](rust/src/api/search_engine.rs#L1200)). `RegexPhraseQuery` מאפשרת בכל מיקום בפראזה
> דפוס regex — כלומר *alternation לכל מיקום* — וזה **בדיוק** מה ש-`MultiPhraseQuery` נותן ב-Lucene.
> לכן עם הגישה של חלק 4.5 (הזרקת צורות לקסיקליות כ-alternation לתוך ה-regex של כל מילה) מקבלים
> את היכולת הרב-מיקומית "בחינם" — אין צורך ב-Cartesian product ידני. האפשרויות למטה רלוונטיות
> בעיקר אם רוצים **דירוג boost** (שאין לו תמיכה ישירה במסלול ה-regex).

### האפשרויות:

**אופציה A — Cartesian Product (לדירוג boost בלבד):**
לבנות `BooleanQuery { SHOULD: PhraseQuery_i }` לכל קומבינציה אפשרית, כדי להחיל boost שונה לכל צורה.
שים לב: ה-cap בקוד ה-Lucene הוא כיום **256** (`MAX_SYNONYM_BOOST_TERMS`) ולא 8 — לכן Cartesian מלא
על ערכים כאלה הוא explosion בלתי-אפשרי. אם הולכים בכיוון זה, חובה cap נמוך מאוד (≤4 לכל טוקן)
ולהגביל ל-2-3 טוקנים. במרבית המקרים מיותר — `RegexPhraseQuery` כבר נותן את ה-recall.

**אופציה B — Synonym injection בזמן indexing:**
לכתוב `MagicSynonymFilter` שפולט `position_inc=0` לכל variant.
**לא מומלץ:**
- ניפוח אינדקס פי 5-10
- כל עדכון של lexical.db דורש reindex מלא של הקורפוס
- קושי לנהל cap דינמי

**אופציה C — הזרקת alternation ל-regex (מומלץ — תואם 4.5):**
להזריק את הצורות הלקסיקליות כ-alternation לתוך ה-`regex_terms` (חלק 4.5). מסלול טוקן-יחיד → `TermSetQuery`,
מסלול רב-טוקני → `RegexPhraseQuery` עם alternation לכל מיקום. אין דירוג boost, אבל ה-recall מלא ומשתלב
חלק עם ההרחבה המורפולוגית הקיימת.

המלצה: התחל ב-**C** (זרימה טבעית מ-4.5). עבור ל-**A** רק אם נדרש דירוג עדין לפי סוג הצורה.

---

## חלק 6 — Hallucination Blacklist

ה-AI הפיק לפעמים מיפויים שגויים. הקוד המקורי מנהל בלאקליסט ב-TSV נפרד.

### הקובץ
- **מיקום במקור:** [search/src/jvmMain/resources/hallucination_blacklist.tsv](https://github.com/kdroidFilter/SeforimLibrary/blob/master/search/src/jvmMain/resources/hallucination_blacklist.tsv)
- **URL ישיר:** `https://raw.githubusercontent.com/kdroidFilter/SeforimLibrary/master/search/src/jvmMain/resources/hallucination_blacklist.tsv`
- **גודל:** 29 שורות (25 entries + 4 שורות הערה/הסבר)

### פורמט
```
# Hallucination Blacklist
# Format: token<TAB>base
# Lines starting with # are comments
לחתוך	כנ
לבקש	רצה
מתפלל	בעה
...
```
טוקן (col 1) ובסיס שגוי (col 2). שניהם **מנורמלים עם `normalizeHebrew`** לפני אחסון ב-map.
הטעינה: [loadHallucinationBlacklist](https://github.com/kdroidFilter/SeforimLibrary/blob/master/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt#L62-L87).

### חשוב: סינון רק להיילייטינג
הבלאקליסט בקוד המקורי משמש **רק לסינון היילייטינג**, לא לחיפוש עצמו.
ההיגיון: הזיה ב-recall זה בסדר (יותר תוצאות), אבל הזיה ב-highlight זה בעייתי
(משתמש רואה מילה לא קשורה צבועה אדום).

### יישום מומלץ ב-Rust: `include_str!` בזמן קומפילציה
**אל תוריד בזמן build או ריצה.** הקובץ קטן ומתעדכן רק לעיתים נדירות.
פשוט הטמע אותו בבינארי:

```rust
// rust/src/api/magic/blacklist.rs
use std::collections::{HashMap, HashSet};
use once_cell::sync::Lazy;
use super::normalize::normalize_hebrew;

const BLACKLIST_TSV: &str =
    include_str!("../../../resources/hallucination_blacklist.tsv");

pub static HALLUCINATION_BLACKLIST: Lazy<HashMap<String, HashSet<String>>> =
    Lazy::new(|| {
        let mut map: HashMap<String, HashSet<String>> = HashMap::new();
        for raw in BLACKLIST_TSV.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let mut cols = line.splitn(2, '\t');
            if let (Some(t), Some(b)) = (cols.next(), cols.next()) {
                let token = normalize_hebrew(t);
                let base  = normalize_hebrew(b);
                map.entry(token).or_default().insert(base);
            }
        }
        map
    });

pub fn is_hallucinated(token: &str, expansion: &Expansion) -> bool {
    let nt = normalize_hebrew(token);
    let Some(bad) = HALLUCINATION_BLACKLIST.get(&nt) else { return false; };
    expansion.base.iter().any(|b| bad.contains(&normalize_hebrew(b)))
}
```

### עדכון ידני (script לא-מובנה בבנייה)
```bash
# scripts/sync_blacklist.sh
curl -fsSL \
  https://raw.githubusercontent.com/kdroidFilter/SeforimLibrary/master/search/src/jvmMain/resources/hallucination_blacklist.tsv \
  -o rust/resources/hallucination_blacklist.tsv
```
תריץ ידנית כל כמה חודשים. **אל תכניס לבנייה אוטומטית** — זה יוסיף תלות רשת ל-CI וישבור בילדי mobile cross-compilation דרך CargoKit ללא תועלת אמיתית.

---

## חלק 7 — `downloader.rs` — הורדת lexical.db

### API endpoint
```
https://api.github.com/repos/kdroidFilter/SeforimMagicIndexer/releases/latest
```

### דפוס הקובץ
מחפשים asset שב-`browser_download_url` מסתיים ב-`/lexical.db`.

```rust
const LATEST_API: &str = "https://api.github.com/repos/kdroidFilter/SeforimMagicIndexer/releases/latest";

pub fn ensure_lexical_db(dest: &Path) -> anyhow::Result<PathBuf> {
    let db_path = dest.join("lexical.db");
    if db_path.exists() && db_path.metadata()?.len() > 0 {
        return Ok(db_path);
    }
    let body: serde_json::Value = reqwest::blocking::Client::new()
        .get(LATEST_API)
        .header("User-Agent", "otzaria_search_engine/0.1")
        .send()?.json()?;
    
    let url = body["assets"].as_array().unwrap().iter()
        .find_map(|a| {
            let u = a["browser_download_url"].as_str()?;
            u.ends_with("/lexical.db").then(|| u.to_string())
        })
        .ok_or_else(|| anyhow::anyhow!("no lexical.db asset in latest release"))?;
    
    // ... reqwest download to db_path
    Ok(db_path)
}
```

**Android:** אפשר לעשות את ההורדה מצד Dart (Dio או http) ולקרוא ל-Rust רק עם הנתיב. עדיף, כי Flutter כבר מנהל permissions/storage.

---

## חלק 8 — צד Flutter/Dart

`/lib/` הוא wrapper שנוצר אוטומטית ע"י [flutter_rust_bridge](https://cjycode.com/flutter_rust_bridge/) מהחתימות של [search_engine.rs](rust/src/api/search_engine.rs).

**אם הוספת המילון נעשית פנימית ב-`build_query`:**
- חתימת `search()`/`search_and_count()`/`count()` לא משתנה
- מריצים `flutter_rust_bridge_codegen generate` — אין שינוי בקוד שמשתמש ב-API מצד Dart
- האפליקציה ממשיכה להזריק "regex_terms" ולקבל תוצאות

**אם מוסיפים flag `enable_magic: bool`:**
- שינוי מינ' בצד Dart — תוספת פרמטר בכל קריאה
- מאפשר fallback למצב ישן

המלצה: התחל עם משתנה גלובלי או config פנימי, ללא שינוי API.
פלאג רק אם תרצה גמישות מ-runtime ב-Dart.

---

## חלק 9 — שלבי ביצוע מומלצים

| שלב | פעולה | מאמץ | קריטריון הצלחה |
|---|---|---|---|
| 0 | אימות רישיון AGPL → AGPL (כבר אומת) | ✓ done | - |
| 1 | קוד `magic/normalize.rs` + מבחני יחידה | ~2h | `normalize_hebrew("הָלַ֣ךְ")` → `"הלכ"` |
| 2 | קוד `magic/dictionary.rs` + downloader | ~4h | מילון נטען, `expansion_for("הלך")` מחזיר ≥5 צורות |
| 3 | בלאקליסט הזיות (port מה-Kotlin) | ~1h | מיפוי `HashMap<&str, Vec<&str>>` סטטי |
| 4 | שינוי `build_query` + flag פנימי | ~3h | `search("הלך")` מוצא תוצאות שמכילות "הלכתי" |
| 5 | בנצ'מרק לפני/אחרי | ~2h | latency < +20ms, recall ↑↑ |
| 6 | בדיקה ב-Android (memory profile) | ~2h | RSS < 50MB עם cache=128 |

**סה"כ:** ~14-15 שעות עבודה.

---

## חלק 10 — נקודות זהירות וטיפים מהקוד המקורי

### סטופ-וורדס לפני הרחבה
ב-Lucene: אותיות עבריות בודדות מסוננות *לפני* קריאה ל-MagicDict (חיסכון ענק בזמן+תוצאות מיותרות). ראה
[שורות 399-409](https://github.com/kdroidFilter/SeforimLibrary/blob/master/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt#L399-L409).
חריגים:
- שמירת "ה" אם השאילתה המקורית כללה "ה׳" (שם השם)
- שמירת טוקנים מספריים

### candidates של final form
לפני lookup, ה-Kotlin בונה רשימה של 4 נוסחים: raw, normalized, raw עם סופית, normalized עם סופית. ראה
[buildLookupCandidates](https://github.com/kdroidFilter/SeforimLibrary/blob/master/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/MagicDictionaryIndex.kt#L270-L292).
חשוב לפורט.

### Thread-safety
ב-Kotlin הוא משתמש ב-`ThreadLocal<LookupContext>` — connection נפרד פר thread.
ב-Rust עם `Mutex<Connection>` זה פשוט יותר אבל יעיל פחות תחת load מקבילי. אם זה צוואר בקבוק:
- `r2d2_sqlite` (connection pool)
- או `thread_local!` עם connection פר thread

### גודל ה-cache
- Desktop: 1024 tokens + 512 bases (כמו המקורי)
- Android: 128 + 64 (זיהוי `cfg!(target_os = "android")`)

---

## חלק 11 — קישורים

### קוד מקור
- **SeforimMagicIndexer (offline AI builder):** https://github.com/kdroidFilter/SeforimMagicIndexer
- **SeforimLibrary (consumer + integration):** https://github.com/kdroidFilter/SeforimLibrary
- **קובץ ה-Lucene integration:** [LuceneSearchEngine.kt](https://github.com/kdroidFilter/SeforimLibrary/blob/master/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt)
- **קובץ ה-Dictionary index:** [MagicDictionaryIndex.kt](https://github.com/kdroidFilter/SeforimLibrary/blob/master/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/MagicDictionaryIndex.kt)
- **קובץ Hebrew utils:** [HebrewTextUtils.kt](https://github.com/kdroidFilter/SeforimLibrary/blob/master/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/HebrewTextUtils.kt)
- **סכמת ה-DB:** [Database.sq](https://github.com/kdroidFilter/SeforimMagicIndexer/blob/master/core/src/commonMain/sqldelight/io/github/kdroidfilter/seforim/magicindexer/db/Database.sq)

### API
- **Latest release (lexical.db):** https://api.github.com/repos/kdroidFilter/SeforimMagicIndexer/releases/latest
- **Releases page:** https://github.com/kdroidFilter/SeforimMagicIndexer/releases

### Tantivy resources
- **Tantivy docs:** https://docs.rs/tantivy/0.26.1/tantivy/
- **BoostQuery:** https://docs.rs/tantivy/0.26.1/tantivy/query/struct.BoostQuery.html
- **PhraseQuery:** https://docs.rs/tantivy/0.26.1/tantivy/query/struct.PhraseQuery.html
- **Tantivy custom tokenizers:** https://docs.rs/tantivy/0.26.1/tantivy/tokenizer/index.html

### Rust crates
- **rusqlite:** https://docs.rs/rusqlite/
- **lru:** https://docs.rs/lru/

---

## נספח A — נתונים שאומתו על lexical.db בפועל

מבוסס על קובץ אמיתי (גרסה מ-2026-05-03). **עדכון:** ה-release הרשמי האחרון הוא `v0.3.0`
(2026-04-26), `lexical.db` במשקל 57,122,816 בייט (~54.5MB). המספרים למטה (מספרי שורות בטבלאות)
לא נבדקו מחדש מול v0.3.0 ועשויים לסטות קלות, אך סדרי הגודל תקפים:

| מדד | ערך | משמעות לפיתוח |
|---|---|---|
| גודל ה-DB | **~54.5 MB** (57,122,816 B) | OK ל-Desktop. לאנדרואיד שווה לבדוק האם להוריד אחרי first-run ולא לארוז |
| מספר למות (`base`) | 24,559 | קצת מעבר ל-WordNet העברי הפתוח |
| צורות שטח (`surface`) | 137,631 | היחס ~5.6 surfaces/lemma — סביר מבחינה מורפולוגית |
| וריאנטים (`variant`) | 190,151 | יותר וריאנטים מ-surfaces — הרבה כתיב חלופי |
| `surface_variant` קישורים | 594,428 | יחס cardinality גבוה |

### השלכות מעשיות
- **שלם cache LRU:** עם 137K surfaces, cache של 1024 ב-Desktop מכסה רק 0.7% — אבל ה-locality של שאילתות אמיתיות גבוה, אז זה בסדר. ל-Android cache=128 יספיק.
- **First-load latency:** אין צורך ב-pre-warming — lookup הוא חיפוש לפי index ב-SQLite, מילישניות בודדות.
- **גישה לפלאש באנדרואיד:** 54 MB SSD random read sequential דרך MMap → תכנן להעתיק את ה-DB ל-internal storage לפני שימוש, או השתמש ב-`temp_store=MEMORY` עם connection pragmas.

### בלאקליסט הזיות
- **25 entries** בלבד נכון להיום
- מתעדכן ב-git history של [hallucination_blacklist.tsv](https://github.com/kdroidFilter/SeforimLibrary/commits/master/search/src/jvmMain/resources/hallucination_blacklist.tsv)
- ראה חלק 6 — embed עם `include_str!`, רענן ידנית.

---

*מסמך זה נוצר ב-2026-05-17 כתוצאה מסשן מחקר של Claude Opus 4.7 על שלושת הריפו הרלוונטיים.*
*אומת ועודכן ב-2026-06-23 (Claude Opus 4.8) מול הקוד בפועל — ראה באנר האימות בראש המסמך.*
