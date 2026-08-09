# Changelog

## Unreleased – האינדקס הלקסיקלי כקורפוס של בניית האינדקס הסמנטי (S4b)

### Added

- **`semantic_corpus::TantivyCorpus` — מימוש `CorpusIndex` ו-`CorpusBooks` מעל
  אינדקס Tantivy חי.** ה-crate הסמנטי אינו מקשר Tantivy ואסור שיקשר — האינדקס,
  הסכמה וסכמת ה-IDs חיים כאן — ולכן ה-builder שלו מקבל את הקורפוס דרך פורט.
  המימוש הזה מחליף את התמלול ל-JSONL ששימש עד עכשיו כתחליף.

  **snapshot אחד לכל הבנייה.** בנייה קוראת את הקורפוס שלוש פעמים לפחות: לגזירת
  קבוצת השורות שהמתכון מטמיע, לגזירת הטקסט להטמעה, ולצירוף כל וקטור מוגמר
  למטא-דאטה שלו. `TantivyCorpus` מחזיק `Searcher` **אחד** לכל חייו ואינו טוען
  מחדש; שלוש קריאות שינחתו על שלושה commits שונים היו מערבבות תכנית מאחד, טקסט
  הקשר משני ומטא-דאטה משלישי. `source_line_sha256` של ה-packer לא היה תופס את
  זה: הוא משווה את שורת **העוגן**, ושכן שהשתנה בין שתי קריאות משנה את מה שהוטמע
  בעוד כל ה-digests מסכימים.

  **בדיקת שלמות מול משהו שהסריקה לא ייצרה.** קבוצת הכיסוי נגזרת מ-`book_keys()`
  ומ-`book_line_ids()`, וה-packer משווה את הווקטורים לאותה קבוצה — ולכן ספר
  שהמימוש הזה היה משמיט בטעות היה נעלם משני הצדדים בבת אחת. `open` משווה את
  הסריקה ל-`Searcher::num_docs`, ספירה ש-tantivy גוזר ממטא-דאטה של הסגמנטים
  ומ-bitset המחיקות, בלי שום עזרה מהסריקה.

  **שלושה שדות נקראים עמודתית ולא מהמסמך.** `sectionId`, `lineHash`
  ו-`contentHash` הם FAST ו**אינם מאוחסנים**, ולכן קריאה שלהם מהמסמך המאוחזר —
  הדרך המתבקשת לכתוב את זה — הייתה מחזירה אפס לכל שורה, בשקט, וכל רשומה בכל
  ארטיפקט הייתה נושאת אפס שה-packer מאמת מול אותו אפס.

  **`corpus_id` מכסה כל שדה של כל שורה.** SHA-256 מעל כל מסמך חי בסדר `line_id`
  עולה, עם המזהה ועם ה-JSON הקנוני של ה-`CorpusLine` כולו. במכשיר אין join מול
  Tantivy — ההתקנה משווה `CorpusIdentity` ואז קוראת וקטורים — ולכן כל שדה שמשפיע
  על מה שהוטמע, על סינון או על קיבוץ חייב להזיז את הערך: `section_id` קובע מה
  שורה קצרה מטמיעה (ההקשר נאסף רק מאותו section) והוא גם מפתח הקיבוץ
  `SameSection`, `line_hash` הוא מפתח `IdenticalText`, ו-`facets` ו-`is_pdf` הם
  מה שה-sidecar מסנן לפיו לפני כל hydration. המחיר: שינוי כותרת מבטל את הווקטורים
  של אותו ספר. זו ברירת המחדל הנכונה — repack של מטא-דאטה בלבד הוא אופטימיזציה
  עתידית, וקבלה שקטה של מטא-דאטה ישן אינה.

  **קריאה קפדנית, בלי ברירות מחדל ובדיוק ערך אחד.** שדות columnar ב-tantivy הם
  רב-ערכיים מתחת, יהיה מה שהסכמה תרמז — ולכן `first(doc)` בודק „לפחות אחד" ולא
  „בדיוק אחד". מסמך עם שני `sectionId` היה נותן לאחסון לבחור מאיזה section שורה
  קצרה שואלת הקשר, ולאיזו קבוצה האפליקציה מקבצת אותה. `id` נקרא גם מהעמודה
  (בזמן ה-enumeration) וגם מהשדה המאוחסן, ושתי הקריאות חייבות להסכים: מסמך ששתי
  ההעתקות שלו נבדלות היה מתויק תחת מזהה אחד ומתאר אחר. טקסט חסר אינו `""`, עמודה חסרה אינה `0`
  ו-`isPdf` חסר אינו `false`: כל אחד מהם היה ערך שהארטיפקט נושא, שה-packer מאמת
  מול אותה בדיה, ושהאפליקציה מסננת ומקבצת לפיו. `contentHash = 0` הוא תשובה
  אמיתית ל-PDF, ולכן „לעמודה אין ערך למסמך הזה" ו„הערך הוא אפס" אינם מתמזגים.

  **סכמת ה-IDs נאכפת ולא רק מוצהרת.** `add_document` הציבורי מקבל כל `u64`,
  ולכן המיון לפי `line_id` יכול היה לסדר שורות בסדר שאיש לא בחר בעודו מצהיר
  `document_id_scheme_version = 1`. נבדק המבנה שהסדר תלוי בו: כל שורות ספר חולקות
  חצי עליון אחד, אין חצי עליון משותף לשני ספרים, ואין שורה במיקום 0, ואין חצי
  עליון **אפס** — תחת סכמה 1 החצי העליון הוא `catalogue_order + 1`, ולכן אפס אינו
  מיקום בקטלוג אלא מה שאורדינל נראה כשדבר לא הרכיב אותו. רציפות **אינה** נדרשת —
  מחיקת שורה משאירה חור, וקורפוס עם שורה שנמחקה הוא קורפוס רגיל.

  ובצד ההזנה, `catalogue_order == u32::MAX` נדחה במקום שבו ה-ID מורכב:
  `(MAX + 1) << 32` גולש ב-`u64` ומתגלגל לבסיס אפס ב-release — מיקום אחד בלתי
  שמיש מתוך ארבעה מיליארד, שנקרא בשמו שם במקום להתגלות כדחיית האינדקס כולו.

- **`build_semantic_artifact` — בינארי למכונת ה-build.** נתיב אינדקס, קובץ מודל
  ומתכון → ארטיפקט מאומת. פותח דרך `TantivyCorpus::from_index_path`, שהוא
  read-only **בפועל**: בדיקת תאימות תחילה, `Index::open_in_dir` (לא
  `open_or_create`), ובלי `IndexWriter` כלל. דרך `SearchEngine` זו הדלת של הכותב —
  נתיב שגוי היה יוצר אינדקס ריק במקום לדווח שאין אחד, אינדקס legacy-compatible היה
  מקבל metadata חדש, נעילת הכותב הייתה מוחזקת לאורך כל הבנייה, וסכמה בלתי-תואמת
  הייתה גורמת `panic` לפני שמישהו יכול לדווח עליה. עד עכשיו המסלול המלא רץ רק בתוך בדיקה; זה הופך אותו
  ל-workflow נתמך ליצרן הארטיפקט. אינו חלק מה-FFI ואינו רץ באפליקציה. נבדק
  ב-[`tests/build_semantic_artifact.rs`](rust/tests/build_semantic_artifact.rs)
  מול אינדקס שנכתב לדיסק ונסגר, דרך הבינארי עצמו — כולל שער ה-backend הלא-סמנטי,
  שחייב להיות ברירת מחדל בכלי שצינור release מריץ ולא רק בספרייה שמתחתיו.

### Changed

- **ה-pin של `otzaria-semantic-search` עודכן** לגרסה שכוללת את ה-builder של S4b.
  שני התאמות נדרשו: `get_semantic_index_diff_from_lexical_hashes` מפריד עכשיו
  בין „המסלול הסמנטי כבוי" (`Ok(None)`) לבין „ההשוואה עצמה נכשלה" (שגיאה),
  ואיחוד השניים היה מדווח על manifest פגום כעל תכונה מכובה; ו-`SearchRequest`
  קיבל `profile` ו-`feature_flags`, ששניהם `None` כאן — בחירה בהם היא החלטה של
  S5 ולא של repin.


## 0.7.2 – 2026-08-06 – תיקון ספירת המילים בהדגשת הספר הפתוח

### Fixed

- **מילים הודגשו בספר במרווח שבו החיפוש עצמו אינו מוצא אותן.** תבנית
  ההדגשה (`generate_highlight_pattern`) ספרה מילה מתווכת אחת כ-`\S+` —
  רצף בלי רווחים — בעוד האינדקס מפצל מילים גם על מקף, פסק וסוף-פסוק. בפסוק
  `וַיֹּאמֶר לְאַבְרָם יָדֹעַ תֵּדַע כִּי־גֵר יִהְיֶה זַרְעֲךָ`, השאילתה
  "תדע זרעך" הודגשה כבר במרווח 2 (`כי־גר` נספרה כמילה אחת) בעוד החיפוש
  דורש 3. באפליקציה זה נראה כך: הטקסט מודגש, וחלונית החיפוש שלידו מציגה
  "אין תוצאות".

  מחלקות התווים של התבנית **נגזרות עכשיו מהטוקנייזר** ולא משוכפלות בעבודת
  יד: `hebrew_tokenizer::continues_token` הוא מקור האמת, ומעליו נבנות שלוש
  מחלקות **זרות זו לזו** — אות/ספרה (רק שם טוקן מתחיל ומסתיים), סימן רך
  (ניקוד, פיסוק שקוף, גרשיים — ממשיך טוקן ואינו פותח אותו), ושובר-טוקן.
  כך `רמב״ם`, `פ.ב.י`, `3.14` ו-`יב[ע]ר` הם מילה אחת, ואילו `כי־גר`,
  `א|ב`, `א/ב`, `א׃ב`, `כי⸗גר` (U+2E17) ו-`כי−גר` (U+2212) הם שתיים.

  הזרות אינה קוסמטית: בצורה הקודמת מחלקת המילה ומחלקת המפריד חפפו, ולכן
  `SEP(?:WORD SEP){0,n}` היה דו-משמעי ו-`RegExp` של Dart נתקע בשורה שאינה
  מתאימה. נמדד ב-Dart על אותה שורה בת 90 תווים עם תגי HTML צפופים:
  1,333ms במרווח 5 ו-4,350ms במרווח 10–30 לפני, **0ms** אחרי.

  `<`/`>` מטופלים עכשיו רק כתג שלם: `תדע<b>זרעך` אינו מודגש בשום מרווח, כי
  `strip_html_for_indexing` מוחק את התג בלי רווח והאינדקס רואה שם טוקן אחד.

- **המפריד של תבנית ההדגשה הליטרלית הורחב לאותה מחלקה.** `[\s־׀|]+` החטיא
  פיסוק דבוק: `תדע, זרעך` נמצא בחיפוש (הפיסוק אינו חלק מהטוקן) ולא הודגש
  בספר. עכשיו שני המסלולים חולקים את אותו `WORD_SEPARATOR`.

  שבעה טסטים אוכפים את השקילות: property test שעובר על כל תו בדומיין
  ומשווה את שלוש המחלקות לפרדיקטים של הטוקנייזר ומאמת את הזרות ביניהן, טסט
  טבלאי שמשווה את המרווח המזערי של התבנית למספר הטוקנים
  ש-`next_token_boundaries` מוצא בפער (18 מקרים), טסט לצמידות דרך פיסוק
  שקוף, שני טסטים לתגי HTML, טסט לאורכי התבנית, וטסט שמריץ חיפוש אמיתי על
  אינדקס ומאמת שהחיפוש וההדגשה מסכימים באותו מרווח. בצד Dart נוספו טסטי
  התאמה על טקסט אמיתי (`test/display_highlight_pattern_test.dart`).

## 0.7.1 – 2026-08-02 – תיקון קישור ב-Apple

### Fixed

- **בנייה ל-macOS/iOS נכשלה בשלב הקישור.** 0.7.0 הביאה איתה את llama.cpp,
  ושתי תלויות נייטיביות שלו לא הוצהרו בשום מקום. cargokit בונה `staticlib`,
  ולכן ההצהרות `cargo:rustc-link-lib` של `llama-cpp-sys-2` לא מגיעות ללינקר
  של Xcode כלל. שתי הבעיות התגלו רק בקישור של אפליקציה — ה-CI של הבינאריים
  המוכנים מייצר את הארכיון בלבד ואף פעם לא מלנקק אפליקציה מולו.

  - **frameworks חסרים ב-podspec** (`macos/`, `ios/`): ggml משתמש ב-vDSP
    (Accelerate) ו-Metal, והקוד עצמו הוא C++. נוספו `s.libraries = 'c++'`
    ו-`s.frameworks = 'Accelerate', 'Metal', 'MetalKit', 'Foundation'`.
    בלעדיהם חסרו מאות סמלים מסוג `_vDSP_*`, `_MTL*` ו-`___cxa_*`.
  - **`common` של llama.cpp נבנה שלא לצורך**: הסיידקר נעוץ מחדש ל-revision
    שמכבה אותו. `llama-cpp-2` הצהיר על `llama-cpp-sys-2` בלי
    `default-features = false`, וה-default של sys הוא `["common"]` — מה
    שגרר את `download.cpp` ואת `cpp-httplib` שאף פעם לא מקושרת, ולכן נשארו
    12 סמלי `httplib::*` בלתי פתורים. הקוד הזה לא היה נגיש מ-Rust מלכתחילה.
    זהות הווקטורים נשמרה (token ids זהים, worst cosine 0.9961), והארכיון
    ל-Apple קטן ב-~13MB.

## 0.7.0 – 2026-07-31 – חיפוש סמנטי היברידי

### Added

- **חיבור החיפוש הסמנטי ל-`SearchEngine` דרך FFI** – משטח API חדש
  ותוספתי לחלוטין; אף קריאה קיימת לא שונתה. הסיידקר
  [`otzaria-semantic-search`](https://github.com/Otzaria/otzaria-semantic-search)
  נקשר לאותה ספרייה נייטיבית של Tantivy, ונעוץ ל-revision מדויק ב-
  `rust/Cargo.toml` ו-`rust/Cargo.lock` — זהות המודל והאינדקס תלויה במימוש
  המדויק, ולכן ענף נייד אסור.

  - **מחזור חיים**: `configureSemantic` / `disableSemantic` / `semanticStatus`.
    קריאה חוזרת ל-`configureSemantic` עם אותם קלטים היא no-op שמחזירה את
    הסטטוס; עם קלטים שונים היא **נכשלת ומציינת איזה שדה השתנה**, כי מאגר
    הווקטורים הוא בזיכרון ופתיחה מחדש הייתה מוחקת אותו בשקט. החלפת מודל או
    ספרייה היא מעשה מפורש: `disableSemantic` תחילה.
  - **אינדוקס**: `semanticIndexBooks` / `semanticIndexDiff` /
    `removeSemanticBooks` / `resetSemanticIndex`. כולם מקבלים `&self` בצד
    Rust, כך ש-flutter_rust_bridge אינו נועל את המנוע כולו — חיפוש לקסיקלי
    ו-polling של סטטוס נשארים רספונסיביים לאורך אינדוקס של ספרייה שלמה.
  - **חיפוש**: `searchSemantic` עם `SemanticRetrievalMode`
    (`hybrid` / `semanticOnly` / `lexicalOnly`), נפרד מ-`SemanticLexicalMode`
    (`exact` / `fuzzy`) — פרשנות לקסיקלית ומצב אחזור הם שני צירים שונים.
    בקשת `hybrid` נופלת ל-Tantivy עם `fallbackReason` מפורש כשהסמנטי לא
    זמין; בקשת `semanticOnly` לעולם אינה מתחזה לתוצאה לקסיקלית.

- **חוזה תצוגה מפורש בתוצאה** – `snippetHtml` תמיד מכיל טקסט להצגה, ולצידו
  `isHighlighted` שאומר אם הוא נצבע. תוצאה סמנטית שלא עמדה בביטוי הלקסיקלי
  מקבלת קטע טקסט נקי ו-`isHighlighted == false`, במקום להיצבע כאילו נמצאה
  לקסיקלית. `SemanticResultSource` (`lexical` / `semantic` / `both`) מוסיף
  לכל תוצאה את מקורה.

### Changed

- **`minSdkVersion` הועלה מ-21 ל-23** – *שינוי שובר לצרכנים שתומכים ב-API 21
  או 22.* `llama.cpp` קורא ל-`posix_madvise`, ש-bionic חושף רק מ-API 23.
  התואם ל-`ANDROID_PLATFORM` שבו נבנים הבינארים המוכנים. אפליקציות על
  `flutter.minSdkVersion` (24) אינן מושפעות.

- **הפיצ'ר `semantic` פעיל בבניות הצרכן** (`rust/cargokit.yaml`) – בדרך כלל
  שקוף, כי הבינארים המוכנים מורדים משוחררים וחתומים. בנייה מקומית שנופלת
  אחורה (crate hash ללא artifacts) מקמפלת `llama.cpp` ולכן **דורשת cmake**.

- **ב-ARM 32-ביט (`armv7-linux-androideabi`) אין embedding backend** –
  `llama-cpp-sys-2` אינו נבנה ליעד הזה, ומודל 0.6B ב-Q4 אינו שמיש עליו
  ממילא. הבנייה מצליחה והחיפוש מתדרדר לבדו ללקסיקלי, אבל `semanticIndexBooks`
  **זורק** שם. הדגל לבדיקה לפני אינדוקס הוא `available` — לא `enabled`, ולא
  diff לא-ריק. ראו "Builds without a backend" ב-README.

## 0.6.9 – 2026-07-15

## 0.6.8 – 2026-07-14 – חיפוש מתקדם מורחב, facets ממדיים ושדרוג ביצועי אינדוקס

### Added

- **התאמה חלקית של מילות השאילתה (`wordMatchMode`) במסלול המתקדם** –
  צמד פרמטרים אופציונליים חדש בכל משפחת ה-advanced (`searchAdvanced`,
  ה-streams, `countAdvanced`, `countByBookAdvanced`,
  `getFacetCountsAdvanced` וגרסאות ה-`withStatus` שלהם):
  `wordMatchMode` — `all` (ברירת המחדל, ההתנהגות הקיימת) / `anyWord`
  (די במילה אחת) / `mostWords` (רוב: `n/2+1`) / `atLeast` (לפחות
  `wordMatchCount` מילים, נחתך ל-`[1, n]`). בכל מצב שאינו `all` דרישת
  הסדר והמרחק בטלה: `wordDistance` מתנהג כ"אותה פסקה" (BooleanQuery
  של Should עם מינימום נדרש — תוצאה עם יותר מילים מקבלת score גבוה
  יותר במיון רלוונטיות), ו-`sameSection` דורש שהסעיף יכיל לפחות את
  מספר המילים הנדרש (ספירת מילים ייחודיות פר סעיף במקום חיתוך).
  מילה שחוזרת בשאילתה נספרת פעם אחת בסף; שאילתת השלילה נשארת תמיד
  "כל המילים"; חלופות ר"ת נשארות ביטוי שלם.
  בהדגשה: מסנן-הביטוי מנוטרל בהתאמה חלקית כך שגם מילה בודדת שנמצאה
  נצבעת.

- **אפשרות "ארמית" פוצלה ל"קידומות ארמיות" ו"סיומות ארמיות"** – שתי
  אפשרויות פר-מילה עצמאיות במקום מפתח `ארמית` היחיד (שלא פורסם):
  "קידומות ארמיות" — קבוצת הקידומות הדקדוקית (ד/כד/אד/מד...) לפני
  המילה; "סיומות ארמיות" — שקילות אות סופית ה↔א (מלכה↔מלכא) ו-ם↔ן
  (חכמים↔חכמין). סימון שתיהן משחזר את ההתנהגות המקורית. בהדגשה:
  וריאנטי השקילות נצבעים תחת "סיומות ארמיות"; זכאות גבול-המילה נשברת
  רק תחת "קידומות ארמיות".

- **אפשרות "ראשי תיבות" – פענוח ר"ת דו-כיווני בחיפוש המתקדם** – מילון
  ראשי-תיבות (`Acronyms.json` של האפליקציה, נטען דרך
  `set_acronyms_dictionary_path`/`has_acronyms_dictionary`) מרחיב שאילתה
  שסומנה לה האפשרות: ר"ת בודד מוצא גם את פענוחיו המלאים
  (`רמב"ם` ← "רבי משה בן מיימון"), וביטוי שהוא פענוח ידוע מוצא גם את
  הר"ת (`רבי משה בן מיימון` ← `רמב"ם`). הפענוח רב-מילי ולכן נבנה
  כתת-שאילתות OR מלאות (slop 0, באותם facets/scope) ולא כחלופות
  חד-מילתיות; ההתאמה הדטרמיניסטית `רמבם`↔`רמב"ם` נשארת ברמת האינדקס
  (הטוקן-התאום). פענוח חד-מילי מדולג בטעינה (מכוסה ממילא ע"י האינדקס);
  תקרת `MAX_ACRONYM_EXPANSIONS = 16` פר כיוון. מצב מנוקד אינו נתמך בשלב
  זה. מפתח UI: `ראשי תיבות`. הדגשה: מילות החלופות מצטרפות לאיחוד
  ההדגשה השטוח, כך שמסמך שנמצא דרך הפענוח (או דרך הר"ת בכיוון ההפוך)
  נצבע — דרך נפילת מסנן-הביטוי לצביעה הרחבה.

- **אפשרות "תרגום ארמי" – הרחבת מילה בתרגומיה** – מילוני הארמית-עברית
  (`dictionary.json` של האפליקציה, נטען דרך
  `set_translation_dictionary_path`/`has_translation_dictionary`) מרחיבים
  מילה שסומנה לה האפשרות בתרגומיה בשני הכיוונים (ארמי↔עברי), כמילים
  חלופיות שזורמות בכל המסלולים הקיימים. כל המילונים שבקובץ ממוזגים
  (פשיטא, שיח ישראל, אונקלוס...) — הגרסה הראשונית לקחה את "המילון
  הראשון", ומפת serde_json ממוינת אלפביתית כך שנטען בפועל "מושגים
  ואישים" (ערך יחיד) וכל התרגומים מתו בשקט (`הכא`↔`כאן` לא עבד). רק
  תרגומים בני מילה אחת נכנסים למפה; תקרת
  `MAX_TRANSLATION_EXPANSIONS = 16`. מפתח UI: `תרגום ארמי`.

- **אפשרות "התעלם מגרשיים" פר-מילה** – גרש/גרשיים שהוקלדו במילה מוסרים
  לפני בניית התבנית (`רמב"ם` מחפש `רמבם`), שמותאמת באינדקס לשתי הצורות
  דרך הטוקן-התאום. מפתח UI: `התעלם מגרשיים`.

- **טוקן-תאום נטול-גרשיים באינדוקס** – במצב האינדוקס (`emit_quote_free`),
  מילה עם גרש/גרשיים מטמיעה גם את צורתה הנקייה באותה עמדה ובאותם
  offsets — חיפוש `רמבם` מוצא `רמב"ם` (וההפך, דרך "התעלם מגרשיים")
  וההדגשה יורשת את טווח המילה המקורית. שינוי *תוכן* מילון הטרמים בלי
  שינוי סכימת tantivy — בדיקת התאימות לא תופסת זאת לבד, ולכן מכוסה
  בהעלאת `INDEX_SCHEMA_VERSION` ל-3 (שטרם פורסמה), שמחייבת בנייה מחדש
  של אינדקסים ישנים.

- **`SearchStreamUpdate.truncated` – איתות "תוצאות חלקיות" ל-UI** – כשמסלול
  המילה-היחידה הרחבה חורג מתקציב איסוף הטרמים (`SINGLE_WORD_POSTINGS_BUDGET`
  / `max_expansions`) הוא ממשיך להגיש את ההרחבות בעדיפות הגבוהה (degrade, לא
  שגיאה) — עד כה בשקט, עם `warn!` ליומן בלבד. כעת דגל ה-truncation מחלחל
  מ-`single_regex_term_query` (כולל שמירה ב-`term_cache`) אל האירוע הראשון של
  ה-stream המשולב, כך שאוצריא מציגה באנר "ייתכן שהתוצאות חלקיות — צמצמו את
  החיפוש". המסלולים המדויק והמקורב נטולי-הסימנים אינם מתדרדרים כך ולכן
  תמיד `false`; המסלולים המנוקדים שלהם כן (מילה מנוקדת מתממשת לסט טרמים
  כמו מילה מתקדמת) ולכן נושאים את הדגל.

- **`add_text_book_bytes` – אינדוקס ספר טקסט מבייטים גולמיים (UTF-8)** –
  האפליקציה קוראת תוכן מ-SQLite שמאוחסן UTF-8; העברתו כ-`Vec<u8>`
  ‏(`Uint8List` בצד Dart) חוסכת את סבב הקידוד UTF-8→UTF-16→UTF-8 שמחרוזת
  Dart עולה על הגשר (~180ms/MB שנמדדו). קלט UTF-8 לא-תקין מתוקן (lossy),
  לעולם לא שגיאה. זהות מלאה ל-`add_text_book` — אותם מסמכים ואותה טביעת
  אצבע (ה-FNV מחושב על אותם בייטים).

- **נרמול מקבילי באינדוקס (rayon)** – ‏`add_text_book` ו-`add_pdf_book`
  מריצים את הנרמול (וסינון הזבל ב-PDF) על כל הליבות: מעבר זול סדרתי פותר
  את ה-reference trail (תלוי-סדר) לאינדקס-לשורה, ואז par_iter על השורות.
  בלוגים הנרמול היה ~85% מזמן ה-CPU של חוט ההזנה (55s מתוך 67s על 942
  ספרים).

- **`add_pdf_book` – אינדוקס PDF שלם בקריאת FFI אחת** – מקבל את עמודי הספר
  (reference, טקסט גולמי, אינדקס עמוד), מנרמל כל שורה
  (`normalize_pdf_text_for_indexing`), מסנן שורות זבל
  (`is_probably_garbage_pdf_text`) ומאנדקס — אותה לוגיקת per-line של
  `normalize_pdf_texts_for_indexing`, בלי שהטקסט המחולץ יחצה את הגשר
  ארבע-חמש פעמים (isolate ‏← נרמול באצוות ‏← SendPort ‏←
  addDocumentsBatch). מחזיר את מספר המסמכים שנוספו; 0 ⇒ אין טקסט שמיש
  (סרוק) והקורא נופל ל-sidecar/סמן-ריק. `segment` = אינדקס העמוד, מזהי
  מסמכים מקודדים סדר קטלוגי כמו `add_text_book`, ‏`contentHash`=0.

- **`set_bulk_indexing` – מצב בנייה מלאה ללא מיזוגי רקע** – בזמן בניית
  ספרייה מלאה `LogMergePolicy` ממזג סגמנטי-ביניים שוב ושוב — עבודה שנזרקת,
  כי הקורא מריץ `optimize` (מיזוג-הכול) פעם אחת בסוף. במצב bulk ה-writer
  (וגם writer שנפתח מחדש בעצלנות) מקבל `NoMergePolicy`; כבוי כברירת מחדל,
  ואינדוקס אינקרמנטלי ממשיך למזג כרגיל.

- **לוגי תזמון לאבחון מהירות אינדוקס** – ‏`add_text_book`/`add_pdf_book`
  מדווחים ב-`info!` מסמכים/בייטים/משך בפירוק prepare (נרמול על חוט ההזנה)
  מול enqueue (לחץ-חוזר מחוטי האינדוקס של tantivy); ‏`commit` מדווח משך
  commit ו-reader reload; ‏`optimize` מדווח סגמנטים לפני/אחרי ומשך. המנוע
  מתקין (פעם אחת, `try_init`) ‏env_logger עם ברירת מחדל
  `search_engine=info` — הלוגים נראים בקונסולת האפליקציה בלי הגדרה בצד
  Dart, ו-`RUST_LOG` עדיין גובר.

- **`*_with_status` למניית תוצאות – איתות truncation גם ל-count/facets** –
  ‏`count`, `count_by_book`, `get_facet_counts` (וגם ה-`_advanced`) זרקו את
  דגל ה-truncation של מסלול המילה-היחידה, כך שעץ סינון ה-facets היה מציג
  ספירות חלקיות בלי סימון. נוספו טיפוסים `CountResult`, `BookCountResult`,
  `FacetCountsResult` (כל אחד עם `truncated`) ומתודות `*_with_status`
  מקבילות. המתודות הישנות נשמרו כתואמות לאחור (מחזירות את הערך הבודד ומשמיטות
  את הדגל) — ומתועדות כלא-מתאימות לתצוגת UI כשחשוב לדעת אם התוצאה חלקית.
  נוספו גם `*_exact_with_status` ו-`*_fuzzy_with_status`: המסלולים
  נטולי-הסימנים שלהם אמנם לעולם אינם מתדרדרים, אבל המסלולים המנוקדים כן —
  ועד כה `count_exact`/`count_fuzzy` והמקבילים זרקו את הדגל.

### Changed

- **חתימת ספר קנונית הכוללת metadata (`computeBookFingerprint`)** –
  `add_text_book`/`add_text_book_bytes` חותמים עתה ב-`contentHash` חתימה
  שכוללת, לצד הטקסט הגולמי, גם את הכותרת, נתיב הקטגוריה, הסדר הקטלוגי,
  סדר הדורות וממדי הסינון (ממוינים ומנוקי-כפילויות, בקידוד קידומת-אורך) —
  שינוי metadata בלבד (למשל תיקון דור או מחבר) מזוהה עתה כספר שדורש
  אינדוקס-מחדש, במקום להשאיר את האינדקס עם facets/מיון/כותרת ישנים.
  **צד האפליקציה חייב לעבור מ-`computeContentFingerprint` (טקסט בלבד,
  נשאר קיים) ל-`computeBookFingerprint` בהשוואות מול
  `getBookFingerprints`** — אחרת כל ספר יזוהה כ"השתנה" בכל הפעלה.

- **הרחבת רף הווריאציות** (VARIATION_CEILING_RESEARCH.md) – ארבעה שינויים
  משלימים:
  - **מסלול מילה בודדת: degrade במקום שגיאה + תקציב postings** – איסוף
    הטרמים נעצר בהגעה לתקציב ומחזיר את הענפים בעדיפות גבוהה שנאספו (אין יותר
    `query exceeded max expansions`). השומר האמיתי הוא עתה תקציב postings
    (סכום doc_freq פר-segment, ‏1M), הנקרא חינם מה-streamer; תקרת מספר
    הטרמים נותרה כשומר זיכרון בלבד והועלתה פי 10 (מורפולוגי: 20k–50k,
    typo: ‏500, ברירת מחדל: 100). סדר הענפים הוא חוזה: כל הצורות המדויקות
    (מילה + חלופות) לפני כל וריאנט typo, מקובע בטסט.
  - **typo במילה בודדת דרך אוטומט לוינשטיין** – כשהדגל היחיד הוא "שגיאות
    כתיב", ההרחבה רצה כסריקת FST אחת לכל טוקן (מרחק 1 + שיכול) במקום ≤128
    סריקות וריאנטים ליטרליים: כל שכונת מרחק-עריכה-1 (על-קבוצה של הרשימה
    הקודמת) במחיר נמוך פי ~100, בכפוף לתקציבי האיסוף — typo בעדיפות הנמוכה
    ביותר, ומדולג אם הצורות המדויקות לבדן מיצו תקציב (מילה שכיחה במיוחד).
    typo בשילוב מורפולוגיה/כתיב נשאר
    במסלול הליטרלי (האוטומט לא מרכיב wildcards). ההדגשות (display + snippets)
    שומרות parity.
  - **תקרות פרַאזה הועלו** – `max_expansions` לפרַאזות: 8,192 אחיד (היה
    100–5,000; ברירת המחדל של tantivy היא 16,384), ותקציבי הענפים
    64/48 ענפים ו-6,000 תווים (היו 48/20 ו-1,000) —
    בזכות ה-vendor הבא.
  - **vendor של tantivy-fst 0.5.0 עם `STATE_LIMIT=8192`** (היה 1,000; שינוי
    יחיד, מנוהל ב-`rust/vendor/tantivy-fst` דרך `[patch.crates-io]`) –
    תבניות פרַאזה מורפולוגיות אמיתיות (48 ענפים / ~800 תווים) שקרסו על תקרת
    ה-DFA מתקמפלות עתה; העלות זיכרון חולף בבניית השאילתה (≈4KB ל-state).
  - תקרות ההדגשה יושרו פרופורציונלית: ‏`MAX_HIGHLIGHT_TERMS` ‏512→2,048,
    ‏`MAX_DISPLAY_PATTERN_CHARS` ‏4,000→12,000.
  - כיול אמפירי של התקציבים (postings budget, תקרות פרַאזה) על אינדקס אמיתי
    דרך `benchmark_cli` — עדיין פתוח.

- **גרשיים וגרש נשמרים בתוך טוקנים** – ראשי-תיבות (`רמב"ם`, `ז"ל`) ומילים עם
  גרש פנימי (`ג'ורג'`, `ד'אש`) מאונדקסים כטרם יחיד. ׳/״ עבריים מקופלים בטרם
  ל-'/" ASCII, וזוג גרשים בין אותיות (`רמב''ם`, מוסכמת קבצים ישנים) מאוחד
  לגרשיים — כל צורות הדפוס מתלכדות לטרם אחד. `splitQueryWords` משקף את אותם
  חוקים, וההדגשות תופסות את כל שלוש צורות הדפוס (`"`, `״`, `''`).
- **חיפוש מדויק רגיש-גרשיים** (מחיר מתועד) – שאילתה `רמבם` ללא גרשיים לא
  תמצא `רמב"ם` בחיפוש מדויק ובמתקדם ללא דגלים, ולהפך. הגישור קיים במקורב
  (גם במרחק 0, דרך הזרקת הצורה הנקייה), בדגל typo (וריאנט-מחיקה) ובדגל
  כתיב מלא/חסר. כמו כן, מרכאות-כציטוט בתחילת מילה (`ה"מגיד`) הופכות לחלק
  מהטוקן — שאילתת `מגיד` מדויקת תחטיא מופע כזה; "חלק ממילה"/מקורב מגשרים.
- **תיקון מפתח ה-lookup הלקסיקלי** – `normalize_hebrew` מוחק עתה גם `"`/`'`
  ASCII, כך שטוקני-גרשיים ממשיכים לקבל הרחבות מ-`lexical.db` ולהיתפס
  ב-blacklist.

### Fixed

- **תקרת הקיבוץ שומרת את הקבוצות הטובות ביותר, גלובלית** – בהגעה
  ל-50,000 קבוצות ה-collector השמיט כל קבוצה *חדשה* לפי סדר סריקת
  המסמכים והסגמנטים — קבוצה טובה (למשל id נמוך במיון קטלוגי, או ציון
  גבוה ברלוונטיות) שהגיעה מאוחר נזרקה בעוד גרועות ממנה נשמרו, והעמוד
  הראשון היה שגוי. כעת בתקרה הקבוצה *הגרועה* לפי סדר המיון מפנה את
  מקומה לקבוצה טובה ממנה (אינדקס BTreeSet לצד המפה) — כל עמוד בטווח
  התקרה מדויק. הצבירה עברה למפה משותפת **אחת לכל החיפוש** (הסגמנטים
  כותבים אליה דרך buffer של ‎4K מסמכים) — במקום עד 50k קבוצות *לכל
  סגמנט* שהצטברו לפני המיזוג למאות MB באינדקס לא-ממוזג; הזיכרון כעת
  מפה גלובלית חסומה ב-50k קבוצות בתוספת buffers חסומים פר-סגמנט,
  ללא תלות במספר הסגמנטים. שארית ה-degrade תחת
  `truncated`: ‏`group_count` נשאר תחתית, ומונה של קבוצה שפונתה וחזרה
  מאבד את חבריה המוקדמים. שימו לב: המסלולים שמחזירים רק
  `List<SearchResult>` משמיטים את הדגל כמו את שאר דגלי הסטטוס — לתצוגת
  קיבוץ השתמשו ב-`searchAndCount*`/`stream_with_counts`.

- **תקציב לספירת הסעיפים בהתאמה חלקית בטווח "תחת אותה כותרת"** – מפת
  ספירת המילים-פר-סעיף של `mostWords`/`atLeast` גדלה כאיחוד סעיפי כל
  המילים — מילים נפוצות הגיעו למאות אלפי רשומות ללא תקרה. כעת תקציב
  קשיח (500k סעיפים): מעבר אליו סעיפים *חדשים* נשמטים עם
  `truncated`, וסעיפים שכבר נספרים ממשיכים להצטבר.

- **חתימת הדה-דופ (`lineHash`) כוללת אלפאנומרי לא-ASCII** – ספרות
  ערביות-הודיות (١٥/١٦), אותיות לטיניות עם סימנים ושאר אלפאנומרי יוניקודי
  נשמטו מהחתימה, כך ששורות שנבדלו רק בהם אוחדו בטעות במצב "טקסט זהה".
  כעת כל `is_alphanumeric` משתתף, בקיפול רישיות יוניקודי; סף 12 האותיות
  נותר עברי בלבד.

- **`SearchPageResult.truncated` – איתות "תוצאות חלקיות" גם במסלול page/count** –
  דגל ה-truncation שנוסף ל-stream המשולב נזרק ב-`search_and_count_advanced`
  וב-`search_and_count` הגנרי (regex), כך שצרכן של ה-API המעומד היה מציג
  תוצאות וספירה חלקיות בלי אזהרה. כעת `SearchPageResult` נושא `truncated`
  באותה סמנטיקה של `SearchStreamUpdate.truncated`; המסלולים המדויק/המקורב
  נטולי-הסימנים תמיד `false`, המנוקדים נושאים את הדגל.

- **שלילה בטווח "תחת אותה כותרת" פוסלת את כל הסעיף** – שאילתת השלילה
  בטווח `SameSection` מחזירה רק את השורות שנושאות מילת שלילה בתוך סעיף
  חותך, כך שב-`MustNot` היא פסלה רק אותן — תוצאה חיובית בשורה אחרת של
  אותו סעיף שרדה בטעות. כעת נאספים ה-`sectionId` שהשלילה חותכת
  (`SectionIdsCollector`) וכל שורה בסעיף כזה נחסמת
  (`SectionFilteredQuery` על `AllQuery`).

- **בדיקת התאימות דורשת meta.json תקין של tantivy** – sidecar תקין עם
  `schema_version` נכון החזיר "תואם" גם כשה-meta.json של tantivy עצמו חסר
  או פגום — מצב שבו פתיחת האינדקס נכשלת בכל מקרה. כעת כשל
  קריאה/פרסור/חוסר-סכימה ב-meta.json מחזיר `rebuild_required` עם הסיבה,
  במקום ליפול בשקט ל-"compatible".

- **בדיקת התאימות משווה גם את סכימת ה-tantivy בפועל** – אינדקס שנבנה
  בגרסת-ביניים של אותה schema_version (למשל `text` עם fast field, לפני
  ההסרה) עבר את בדיקת הקובץ הצדדי ("3=3, תואם") אבל הפיל את פתיחת המנוע
  על SchemaError — והאפליקציה נפלה בשקט לאינדקס זמני ב-Temp שנבנה מחדש
  בכל הפעלה. כעת `check_index_compatibility` משווה את הסכימה השמורה
  ב-meta.json מול סכימת המנוע (אותה השוואה של `Index::open_or_create`)
  ומחזירה `rebuild_required` על סטייה, כך שזרימת הבנייה-מחדש הרגילה
  מטפלת בזה.

- **הטקסט השמור באינדקס משמר פיסוק** (Otzaria issue #446) – נרמול ה-ingestion
  (`normalizeTextForIndexing` / `normalizePdfTextForIndexing`) כבר לא מוחק
  פסיקים, נקודתיים, סוגריים וכו', כך שתוצאות החיפוש מציגות "עא:" ולא "עא".
  שוויון מילון הטרמים מול צד השאילתה נשמר ע"י "תווים שקופים" ב-`HebrewTokenizer`:
  הפיסוק ש-`sanitizeQuery` מוחק אינו שובר טוקן ואינו נכלל בטקסטו ("א.ב" → "אב"),
  וההדגשות (SnippetGenerator) נופלות נכון על הטקסט המקורי דרך ה-offsets.
- **פירוק Hebrew Presentation Forms** (Otzaria issue #500) – תווים מורכבים
  בטווח U+FB1D–U+FB4F (כגון יִ שהוצגה כ"?") מפורקים לאות בסיס + סימן בזמן
  האינדוקס, והסימן מוסר עם שאר הניקוד. מילים שהכילו אותם נעשות ברות-חיפוש.

### Notes

- גרסת סכימת האינדקס עלתה ל-3 (טרמים חדשים + הסרת ה-fast field): אינדקסים
  קיימים ידווחו `rebuild_required` וייבנו מחדש פעם אחת בעדכון.

## 0.6.7 – 2026-06-30

### Fixed

- **פרסום מחדש עם סופי-שורה LF** – הגרסה שפורסמה ב-0.6.6 הכילה את סקריפטי
  cargokit (`run_build_tool.sh`, `build_pod.sh`) עם CRLF, מה ששבר את בניית
  Linux/Android/macOS אצל הצרכן (`/usr/bin/env: 'bash\r'`). אין שינוי קוד.

## 0.6.6 – MagicDictionary fuzzy search – 2026-06-29

### New

- **שילוב MagicDictionary בחיפוש מקורב (fuzzy)** – ניתן לטעון `lexical.db` בזמן
  ריצה, והחיפוש המקורב מרחיב מונחים לצורות מורפולוגיות קשורות בלי לשנות את
  התנהגות החיפוש כאשר המילון לא נטען.

### Improvements

- **דירוג רלוונטיות בחיפוש מקורב** – תוצאות `ResultsOrder::Relevance` מקבלות
  מדרוג ברור יותר: התאמה מדויקת, אחריה צורה מורפולוגית מהמילון, ואחריה התאמת
  fuzzy רגילה.
- **הדגשות בחיפוש מקורב עם מילון** – ההדגשה משקפת גם את הצורות המורפולוגיות
  שהוזרקו מה-`lexical.db`, כולל תמיכה בשאילתות מרובות מילים.

## 0.6.5 – Fix Apple linking – 2026-06-12

### Fixes

- **תיקון קישור (linking) ב-macOS/iOS** – הוספת `module_name = 'search_engine'`
  בגרסה 0.6.4 שינתה את `PRODUCT_NAME` של ה-pod, כך ש-cargokit כתב את
  `libsearch_engine.a` ל-`$PODS_CONFIGURATION_BUILD_DIR/$PRODUCT_NAME` בעוד
  ש-`-force_load` עדיין חיפש ב-`${BUILT_PRODUCTS_DIR}` — שתי תיקיות שונות,
  והבנייה נפלה עם `library 'libsearch_engine.a' not found`. תוקן ביישור
  `output_files` ו-`OTHER_LDFLAGS` ל-`${PODS_CONFIGURATION_BUILD_DIR}/${PRODUCT_NAME}`.

## 0.6.4 – Fix macos – 2026-06-12

### Fixes

- MACOS

## 0.6.3 – Line Endings Fix (Republish) – 2026-06-12

### Fixes

- **תיקון סופי של סיומות השורה (CRLF) בארכיון שפורסם** – למרות שגרסה 0.6.2 נועדה
  לתקן את הבעיה, הארכיון שפורסם בפועל ל-pub.dev עדיין הכיל CRLF בסקריפטי
  cargokit (`build_pod.sh`, `run_build_tool.sh`), כי לא ניתן לפרסם מחדש גרסה
  קיימת ב-pub.dev. כתוצאה מכך הבנייה ב-macOS המשיכה ליפול עם
  `set: - invalid option`. גרסה זו נארזת מחדש ממכונת macOS עם LF בלבד. אין שינוי
  קוד.

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
  final bool truncated; // תוצאות/ספירה חלקיות (חריגה מתקציב הרחבת מילה יחידה)
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
