# MagicDictionary → otzaria_search_engine — מסמך מיגרציה

מסמך זה מתאר איך לשלב את מנגנון ההרחבה המורפולוגית **MagicDictionary** של
[kdroidFilter/SeforimLibraryLM](https://github.com/kdroidFilter/SeforimLibraryLM)
ו-[kdroidFilter/SeforimMagicIndexer](https://github.com/kdroidFilter/SeforimMagicIndexer)
לתוך **otzaria_search_engine** (Rust/Tantivy עם Flutter UI).

---

## TL;DR

- **המטרה:** חיפוש בעברית שמוצא צורות הטיה (למה→ההטיות) — "הלך" יחזיר גם "הלכתי", "הולך", "תלך"…
- **הנכס:** קובץ SQLite בשם `lexical.db` שנבנה offline ע"י Gemini AI על קורפוס Sefaria+Otzaria.
- **גודל בפועל:** 54 MB. מכיל 24,559 למות, 137,631 צורות שטח, 190,151 וריאנטים, 594,428 קישורים.
- **המיגרציה אפשרית.** ~250 שורות Rust + שינוי מינ' ב-`build_query`.
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
lexical.db (SQLite, ~XX MB)
    ↓ (downloaded once)
otzaria_search_engine קורא בזמן חיפוש
```

---

## חלק 2 — סכמת `lexical.db`

ארבע טבלאות עיקריות. הסכמה המלאה ב-[Database.sq](https://github.com/kdroidFilter/SeforimMagicIndexer/blob/main/core/src/commonMain/sqldelight/io/github/kdroidfilter/seforim/magicindexer/db/Database.sq):

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

ב-[LuceneSearchEngine.kt](https://github.com/kdroidFilter/SeforimLibraryLM/blob/main/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt) בכל שאילתה:

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

- **Cap על מספר ההרחבות:** `MAX_SYNONYM_BOOST_TERMS = 8` (משתנה גלובלי, [שורה 42](https://github.com/kdroidFilter/SeforimLibraryLM/blob/main/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt#L42)).
- **Hallucination blacklist:** AI לפעמים יוצר מיפויים שגויים. ראה [שורות 50-95](https://github.com/kdroidFilter/SeforimLibraryLM/blob/main/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt#L50-L95) — בלאקליסט ידני שמסונן רק להיילייטינג (לא לחיפוש עצמו, כדי לשמור recall).
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
// ניקוד + meteg + qamatz qatan
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

### 4.5 שילוב ב-`build_query`

ב-[search_engine.rs](rust/src/api/search_engine.rs#L634-L664), שינוי `build_query`:

```rust
fn build_query(
    index: &Index,
    regex_terms: Vec<String>,
    facets: Vec<String>,
    slop: u32,
    max_expansions: u32,
    magic_dict: Option<&MagicDictionary>,  // ← פרמטר חדש (פנימי)
) -> Result<Box<dyn Query>> {
    let schema = index.schema();
    let text_field = schema.get_field("text").unwrap();
    
    let main_query: Box<dyn Query> = match (regex_terms.len(), magic_dict) {
        // יחיד + מילון: BooleanQuery עם boosts
        (1, Some(dict)) if is_plain_hebrew_word(&regex_terms[0]) => {
            build_magic_expanded_query(text_field, &regex_terms[0], dict)?
        }
        // יחיד: כמו היום
        (1, _) => Box::new(RegexQuery::from_pattern(&regex_terms[0], text_field)?),
        // רב-טוקני: RegexPhraseQuery (כמו היום) — בעתיד אפשר להוסיף Cartesian
        _ => {
            let mut p = RegexPhraseQuery::new(text_field, regex_terms);
            p.set_slop(slop);
            p.set_max_expansions(max_expansions);
            Box::new(p)
        }
    };
    // ... המשך כמו היום (facets etc.)
}
```

`build_magic_expanded_query` בונה:
```
BooleanQuery {
    SHOULD: BoostQuery(2.0, TermQuery(token))           ← הטוקן המקורי
    SHOULD: BoostQuery(2.0, TermQuery(surface_i)) for i ← הטיות
    SHOULD: BoostQuery(1.5, TermQuery(variant_j)) for j ← וריאנטים
    SHOULD: BoostQuery(1.0, TermQuery(base))            ← למה
}
```

עם cap של 8 לכל קטגוריה (כמו בקוד המקורי).

---

## חלק 5 — הבדל אדריכלי קריטי: אין `MultiPhraseQuery` ב-Tantivy

ב-Lucene, `MultiPhraseQuery` מאפשרת **בכל מיקום בפראזה לבחור מבין n חלופות**.
ב-Tantivy זה לא קיים כפיצ'ר ישיר.

### האפשרויות:

**אופציה A — Cartesian Product (מומלץ לשלב 1):**
לבנות `BooleanQuery { SHOULD: PhraseQuery_i }` לכל קומבינציה אפשרית.
עם cap של 8 חלופות לכל טוקן ו-3 טוקנים, זה עד 8³=512 — לא נורא.
מעל זה ה-explosion מאיים — להגביל ל-2-3 טוקנים בלבד.

**אופציה B — Synonym injection בזמן indexing:**
לכתוב `MagicSynonymFilter` שפולט `position_inc=0` לכל variant.
**לא מומלץ:**
- ניפוח אינדקס פי 5-10
- כל עדכון של lexical.db דורש reindex מלא של הקורפוס
- קושי לנהל cap דינמי

**אופציה C — חזרה ל-disjunction-only (פשטני):**
לוותר על phrase search גמיש, להחזיר רק `BooleanQuery { SHOULD: TermQuery }` כפי שמתואר ב-4.5. מתאים אם רוב השאילתות הן single-token או short queries.

המלצה: התחל ב-**C**, עבור ל-**A** אם המשתמשים מתלוננים על phrase recall.

---

## חלק 6 — Hallucination Blacklist

ה-AI הפיק לפעמים מיפויים שגויים. הקוד המקורי מנהל בלאקליסט ב-TSV נפרד.

### הקובץ
- **מיקום במקור:** [search/src/jvmMain/resources/hallucination_blacklist.tsv](https://github.com/kdroidFilter/SeforimLibraryLM/blob/main/search/src/jvmMain/resources/hallucination_blacklist.tsv)
- **URL ישיר:** `https://raw.githubusercontent.com/kdroidFilter/SeforimLibraryLM/main/search/src/jvmMain/resources/hallucination_blacklist.tsv`
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
הטעינה: [loadHallucinationBlacklist](https://github.com/kdroidFilter/SeforimLibraryLM/blob/main/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt#L62-L87).

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
  https://raw.githubusercontent.com/kdroidFilter/SeforimLibraryLM/main/search/src/jvmMain/resources/hallucination_blacklist.tsv \
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
[שורות 399-409](https://github.com/kdroidFilter/SeforimLibraryLM/blob/main/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt#L399-L409).
חריגים:
- שמירת "ה" אם השאילתה המקורית כללה "ה׳" (שם השם)
- שמירת טוקנים מספריים

### candidates של final form
לפני lookup, ה-Kotlin בונה רשימה של 4 נוסחים: raw, normalized, raw עם סופית, normalized עם סופית. ראה
[buildLookupCandidates](https://github.com/kdroidFilter/SeforimLibraryLM/blob/main/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/MagicDictionaryIndex.kt#L270-L292).
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
- **SeforimLibraryLM (consumer + integration):** https://github.com/kdroidFilter/SeforimLibraryLM
- **קובץ ה-Lucene integration:** [LuceneSearchEngine.kt](https://github.com/kdroidFilter/SeforimLibraryLM/blob/main/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/LuceneSearchEngine.kt)
- **קובץ ה-Dictionary index:** [MagicDictionaryIndex.kt](https://github.com/kdroidFilter/SeforimLibraryLM/blob/main/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/MagicDictionaryIndex.kt)
- **קובץ Hebrew utils:** [HebrewTextUtils.kt](https://github.com/kdroidFilter/SeforimLibraryLM/blob/main/search/src/jvmMain/kotlin/io/github/kdroidfilter/seforimlibrary/search/HebrewTextUtils.kt)
- **סכמת ה-DB:** [Database.sq](https://github.com/kdroidFilter/SeforimMagicIndexer/blob/main/core/src/commonMain/sqldelight/io/github/kdroidfilter/seforim/magicindexer/db/Database.sq)

### API
- **Latest release (lexical.db):** https://api.github.com/repos/kdroidFilter/SeforimMagicIndexer/releases/latest
- **Releases page:** https://github.com/kdroidFilter/SeforimMagicIndexer/releases

### Tantivy resources
- **Tantivy docs:** https://docs.rs/tantivy/0.26.0/tantivy/
- **BoostQuery:** https://docs.rs/tantivy/0.26.0/tantivy/query/struct.BoostQuery.html
- **PhraseQuery:** https://docs.rs/tantivy/0.26.0/tantivy/query/struct.PhraseQuery.html
- **Tantivy custom tokenizers:** https://docs.rs/tantivy/0.26.0/tantivy/tokenizer/index.html

### Rust crates
- **rusqlite:** https://docs.rs/rusqlite/
- **lru:** https://docs.rs/lru/

---

## נספח A — נתונים שאומתו על lexical.db בפועל

מבוסס על קובץ אמיתי (`/Users/david/Downloads/SeforimLibrary/build/lexical.db`, גרסה מ-2026-05-03):

| מדד | ערך | משמעות לפיתוח |
|---|---|---|
| גודל ה-DB | **54 MB** | OK ל-Desktop. לאנדרואיד שווה לבדוק האם להוריד אחרי first-run ולא לארוז |
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
- מתעדכן ב-git history של [hallucination_blacklist.tsv](https://github.com/kdroidFilter/SeforimLibraryLM/commits/main/search/src/jvmMain/resources/hallucination_blacklist.tsv)
- ראה חלק 6 — embed עם `include_str!`, רענן ידנית.

---

*מסמך זה נוצר ב-2026-05-17 כתוצאה מסשן מחקר של Claude Opus 4.7 על שלושת הריפו הרלוונטיים.*
