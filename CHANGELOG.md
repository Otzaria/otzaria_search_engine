# Changelog

## 0.6.2 – Line Endings Fix – 2026-06-12

### Fixes

- **תיקון סיומות שורה (CRLF) בארכיון שפורסם** – גרסה 0.6.1 פורסמה מ-Windows עם
  `core.autocrlf=true`, וסקריפטי ה-shell של cargokit (`build_pod.sh`,
  `run_build_tool.sh`) נארזו עם CRLF — מה ששבר את הבנייה ב-macOS
  (`set: - invalid option`), Android ו-Linux (exit 127 בגלל shebang פגום).
  אין שינוי קוד; פרסום מחדש עם LF בלבד. נוסף `.gitattributes` שמונע הישנות.

## 0.6.1 – Fuzzy Highlight Fix – 2026-06-12

### Fixes

- **הדגשות בחיפוש מקורב (fuzzy)** – תוצאות `searchFuzzy` / `searchAndCountFuzzy` /
  `searchFuzzyStream` / `searchFuzzyTerms` חזרו עד כה ללא הדגשה כלל, כי
  `FuzzyTermQuery` מבוסס-אוטומט ואינו חושף מונחים למחולל ה-snippets. כעת מונחי
  ההדגשה ממומשים ממילון האינדקס דרך אותו אוטומט לוינשטיין שהחיפוש משתמש בו
  (כמו במצב advanced), כך שגם וריאנטים במרחק עריכה — ולא רק המילה שהוקלדה —
  מודגשים בתוצאות.
- **אימות `max_distance` מראש** – ערך `max_distance` מחוץ לטווח 0–2 ב-`searchFuzzy`
  גורם כעת לשגיאה מפורשת לפני בניית השאילתה, במקום כשל לא צפוי בהמשך.

## 0.6.0 – Mode-Specific Search & Hardening – 2026-06-11

---

### Breaking Changes

#### `ReferenceSearchEngine` הוסר

המנוע הייעודי לחיפוש הפניות (`ReferenceSearchEngine`, `ReferenceSearchResult`,
`ReferenceDocumentInput`) הוסר מה-API הציבורי. אפליקציות שהשתמשו בו צריכות
להסיר את הקריאות לפני עדכון התלות.

#### חבילה שונתה ל-`otzaria_search_engine`

החבילה, ה-export הראשי וה-podspecs של iOS/macOS שונו מ-`tantivy_search_engine`
ל-`otzaria_search_engine` (שם ה-crate הפנימי `search_engine` נשאר).

#### `search()` – חריגה מ-`maxExpansions` במונח בודד מחזירה שגיאה

עד כה התקרה נאכפה רק בשאילתות מרובות מונחים; מונח regex בודד רץ ללא הגבלה.
כעת חריגה מחזירה שגיאה בכל המקרים, בדומה להתנהגות של `RegexPhraseQuery`.

---

### New APIs

- **חיפוש לפי מצב** – `searchExact` / `searchFuzzy` / `searchAdvanced` (+
  `countExact/Fuzzy/Advanced`, `searchAndCountExact/Fuzzy/Advanced`,
  `searchExactStream` / `searchFuzzyStream` / `searchAdvancedStream`).
  המצב המתקדם מקבל `searchOptions` / `alternativeWords` / `customSpacing`
  ומריץ את כל לוגיקת השאילתות העברית (קידומות, סיומות, כתיב מלא/חסר,
  סובלנות לשגיאות) ב-Rust (מודול `hebrew_query`).
- **הדגשות בתוצאות regex/advanced** – מונחי ההדגשה ממומשים ממילון האינדקס
  דרך אותו אוטומט שהחיפוש משתמש בו, כך שכל וריאציה מורפולוגית שתאמה מודגשת.
- **`checkIndexCompatibility(path)`** – בדיקת תאימות אינדקס (sidecar
  `otzaria_index_meta.json` + נפילה חזרה להשוואת הסכמה המלאה של Tantivy).
- **קריאת תוכן האינדקס** – `countDocumentsByFilePath()` ו-`getIndexedFilePaths()`
  לשחזור מצב האינדוקס ישירות מהאינדקס.
- **`searchAndCount` / `searchStream`** – ספירה ותוצאות במעבר יחיד, והזרמת
  תוצאות בנתחים; `search` קיבל `offset` (חובה) ו-`highlight` (אופציונלי).

---

### Fixes & Hardening

- שאילתה ריקה (או סימני פיסוק בלבד) מחזירה אפס תוצאות בכל המצבים —
  ולא panic במצב advanced או *כל* המסמכים במצב fuzzy.
- רשימת facets ריקה כבר לא מאפסת תוצאות בנתיבי ה-regex/advanced;
  facet לא תקין מחזיר שגיאה במקום panic.
- שאילתות advanced מנורמלות כמו האינדקס (הסרת ניקוד + lowercase),
  כך שטקסט מנוקד שהודבק כבר לא מחזיר אפס תוצאות בשקט.
- `SearchEngine.new` לא קורס כשנעילת ה-writer תפוסה — נפתח לקריאה
  והכתיבה הראשונה מנסה שוב; הודעות שגיאה ברורות לסכמה לא תואמת.
- `optimize()` שומר (commit) שינויים ממתינים במקום לזרוק אותם.
- בדיקת התאימות לאינדקסים ישנים משווה את הסכמה המלאה ולא רק את שדה `id`.

### Performance

- מתודות החיפוש הישנות עברו ל-`&self` — חיפושים מקבילים לא מסתנכרנים
  יותר מאחורי נעילת כתיבה.
- `SnippetGenerator` נוצר פעם אחת לכל stream (ולא לכל chunk);
  תקציב מונחי ההדגשה מתחלק שווה בין מילות השאילתה.

### Packaging / CI

- `url_prefix` של הבינארים המקומפלים מצביע על הריפו הקנוני
  (`otzaria/otzaria_search_engine`); סודות ה-CI מוגבלים ל-steps החותמים.

---

## 0.5.0 – Bridge Expansion (Tantivy 0.26 / FRB 2.12) – 2026-05-02

---

### Breaking Changes

#### Schema Change: `id` field is now INDEXED

**Affects:** All existing indices built with the previous version.

שדה `id` שודרג מ-`STORED | FAST` ל-`STORED | FAST | INDEXED`.

**נדרש:** rebuild מלא של כל האינדקסים הקיימים.

**למה:** בלי `INDEXED`, פעולות `delete_term` לא עובדות על השדה הזה, ולכן `deleteDocumentById`, `upsertDocument` ו-`upsertDocumentsBatch` לא היו אפשריות.

---

#### Tantivy 0.26: `TopDocs` no longer implements `Collector` directly

**Affects:** `rust/src/api/search_engine.rs`, `rust/src/api/reference_search_engine.rs`

ב-Tantivy 0.26, `TopDocs` הפסיק לממש את ה-trait `Collector` ישירות.
כדי לקבל collector לתוצאות ממוינות לפי ציון רלוונטיות, חובה לקרוא ל-`.order_by_score()`.

```rust
// לפני (Tantivy < 0.26) – התקמפל אבל כעת שגוי:
let collector = TopDocs::with_limit(100);
searcher.search(&query, &collector)?;

// אחרי (Tantivy 0.26) – חובה:
let collector = TopDocs::with_limit(100).order_by_score();
searcher.search(&query, &collector)?;
```

תוקן בשני המנועים.

---

#### `search()` signature – נוספו פרמטרים

```dart
// לפני:
Future<List<SearchResult>> search({
  required List<String> regexTerms,
  required List<String> facets,
  required int limit,
  required int slop,
  required int maxExpansions,
  required ResultsOrder order,
});

// אחרי:
Future<List<SearchResult>> search({
  required List<String> regexTerms,
  required List<String> facets,
  required int limit,
  required int offset,                 // ← חדש (required, אין ברירת מחדל)
  required int slop,
  required int maxExpansions,
  required ResultsOrder order,
  HighlightConfig? highlight,          // ← חדש (אופציונלי)
});
```

**Migration:** הוסף `offset: 0` לכל קריאות `search()` קיימות.

---

#### `createQuery` / `createSearchQuery` הוסרו

פונקציות אלו חשפו `BoxQuery` ו-`Index` כ-opaque types בלי שום API ציבורי להפעלתן מ-Dart. הן היו dead-ends ממשיים.

**Migration:** אין תחליף ישיר – השתמש ב-`search()`, `searchAndCount()` או `searchFuzzy()`.

---

### New APIs – `SearchEngine`

#### Write

| Method | תיאור |
|---|---|
| `deleteDocumentById(id)` | מחיקה מדויקת לפי מזהה. מחליף את `removeDocumentsByTitle`. |
| `upsertDocument(id, ...)` | מחיקת ישן + הוספת חדש בפעולה אחת. מניעת כפילויות. |
| `addDocumentsBatch(docs)` | הוספת רשימת מסמכים ב-FFI call אחד, ללא delete. מיועד לטעינה ראשונית. |
| `upsertDocumentsBatch(docs)` | כמו batch אבל עם delete-before-add לכל מסמך. מיועד לעדכונים. |
| `rollback()` | ביטול כל השינויים מאז ה-commit האחרון. |

#### Read

| Method | תיאור |
|---|---|
| `getDocumentById(id)` | שליפת מסמך יחיד לפי ID. מחזיר `SearchResult?` עם טקסט גולמי (ללא snippet). |
| `searchAndCount(...)` | חיפוש + ספירה כוללת ב-pass אחד דרך Tantivy (tuple collector). מחזיר `SearchPageResult`. |
| `getFacetCounts(regexTerms, facets, facetPrefix, ...)` | ספירת תוצאות לפי קטגוריה תחת prefix נתון. שימושי ל-drill-down בממשק. |
| `searchFuzzy(terms, facets, limit, offset, maxDistance, order, highlight?)` | חיפוש מקורב אמיתי (Levenshtein) על מילות טקסט רגילות. `maxDistance`: 0=מדויק, 1–2=מקורב. |
| `searchStream(regexTerms, ..., chunkSize)` | מחזיר `Stream<List<SearchResult>>` ב-Dart. שלב ה-TopDocs (דירוג) מסתיים לפני פליטת ה-chunk הראשון; שלב שליפת המסמכים ויצירת ה-snippets מתבצע באופן מוגדר. שימושי כשה-`limit` גדול ויצירת snippets היא צוואר הבקבוק. |

#### Operational

| Method | תיאור |
|---|---|
| `optimize()` | מיזוג כל הסגמנטים לאחד. להריץ ברקע לאחר הרבה עדכונים/מחיקות. |
| `getDocumentCount()` | סך כל המסמכים באינדקס. |
| `getSegmentCount()` | מספר הסגמנטים הנוכחי. גבוה = כדאי להריץ `optimize()`. |

#### Structs חדשים

```dart
class DocumentInput {
  final BigInt id;
  final String title;
  final String reference;
  final String topics;
  final String text;
  final BigInt segment;
  final bool isPdf;
  final String filePath;
}

class HighlightConfig {
  final String highlightPrefix;   // ברירת מחדל: "<font color=red>"
  final String highlightPostfix;  // ברירת מחדל: "</font>"
  final int maxChars;             // ברירת מחדל: 800
}

class SearchPageResult {
  final int totalCount;
  final List<SearchResult> results;
}

class FacetCount {
  final String path;
  final int count;
}
```

---

### New APIs – `ReferenceSearchEngine`

| Method | תיאור |
|---|---|
| `deleteDocumentById(id)` | מחיקה לפי ID |
| `upsertDocument(id, ...)` | עדכון לפי ID |
| `addDocumentsBatch(docs)` | batch הוספה |
| `upsertDocumentsBatch(docs)` | batch עדכון |
| `rollback()` | ביטול שינויים |

Struct חדש:
```dart
class ReferenceDocumentInput {
  final BigInt id;
  final String title;
  final String reference;
  final String shortRef;
  final BigInt segment;
  final bool isPdf;
  final String filePath;
}
```

---

### Bug Fixes

#### `IndexReader` נוצר מחדש בכל חיפוש (SearchEngine)

**לפני:** כל קריאה ל-`search()` / `count()` / `countByBook()` פתחה `IndexReader` חדש מהדיסק – פעולה יקרה מאוד.

```rust
// לפני (בעייתי – קורא metadata מהדיסק בכל חיפוש):
let searcher = index.reader()?.searcher();
```

**אחרי:** `IndexReader` נשמר ב-struct ומשתמשים בו לכל החיפושים. `commit()` מרענן אותו.

```rust
// אחרי (מהיר – reader כבר בזיכרון):
let searcher = self.index_reader.searcher();
```

**השפעה:** שיפור ביצועים משמעותי בחיפוש, במיוחד תחת עומס.

#### `ReferenceSearchEngine` התעלם מה-`IndexReader` הקיים

גם ב-`ReferenceSearchEngine` היה `index_reader` ב-struct אבל לא השתמשו בו בפועל. תוקן.

---

### Notes

- **`removeDocumentsByTitle`** נשמר לתאימות אחורה אבל לא מומלץ לשימוש חדש. השתמש ב-`deleteDocumentById`.
- **fuzzy קיים לעומת חדש:** הקוד הנוכחי באפליקציה משתמש ב-`slop` עם מילים רגילות כ"חיפוש מקורב". `searchFuzzy()` מוסיף חיפוש מקורב אמיתי ברמת ה-Levenshtein – מוצא מסמכים גם כשיש שגיאות כתיב, כתיב מלא/חסר, וכו'.
- **`searchStream` vs pagination:** `searchStream` שולח תוצאות ב-chunks – שלב הדירוג (TopDocs) מסתיים לפני ה-chunk הראשון, אבל שליפת המסמכים ויצירת ה-snippets מתפצלים. ל-pagination רגילה, `search()` עם `offset` מספיקה לרוב המקרים.

---

## 0.0.1

* Initial release.
