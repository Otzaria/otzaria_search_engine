//! Read-only access to `lexical.db`, the offline-built Hebrew morphology
//! lexicon. A query token is resolved to families (one `base` row each) and a
//! bounded selection of their forms becomes extra term alternatives for the
//! approximate (`fuzzy`) search path. Families are reached by three routes:
//!
//! * route 0 — the token is the family's base: base, surfaces, then variants,
//!   up to [`PRIMARY_FAMILY_CAP`];
//! * route 1 — the token is one of the family's surfaces: up to
//!   [`SECONDARY_FAMILY_CAP`] forms;
//! * route 2 — the token is only a spelling variant of some of the family's
//!   surfaces: just those surfaces that share a stem with the token, up to
//!   [`VARIANT_ROUTE_CAP`], and only in capacity routes 0/1 left over. Variant
//!   links are how unrelated families leak in (`שבת` → `בת`, `תשובה` → `לא`).
//!
//! Within a route, families matched by a more literal lookup key come first.
//! Exact search is never touched; a missing or unreadable DB means no expansion.

use super::normalize::{self, StemProbe};
use super::{
    blacklist, MAX_LEXICAL_FORMS, MIN_FORM_LETTERS, MIN_VARIANT_SURFACE_LETTERS,
    PRIMARY_FAMILY_CAP, SECONDARY_FAMILY_CAP, VARIANT_ROUTE_CAP,
};
use anyhow::{Context, Result};
use lru::LruCache;
use rusqlite::{params, Connection, OpenFlags};
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// One family's share of the selection: index-ready terms, deduplicated
/// against every family selected before it.
#[derive(Clone, Debug)]
struct Family {
    base: String,
    terms: Vec<String>,
}

// Every query binds one key with `=` on a UNIQUE or rowid index. An `IN` list
// lets STAT4 re-plan the statement on each binding, which cost ~0.2ms per run.

/// Route 0: the family whose base is the key.
const BASE_BY_VALUE_SQL: &str = "SELECT id, value FROM base WHERE value = ?1";

/// Route 1: the family in which the key is a surface.
const BASE_BY_SURFACE_SQL: &str = r#"
    SELECT b.id, b.value FROM surface s JOIN base b ON b.id = s.base_id WHERE s.value = ?1
"#;

/// Route 2: surfaces linked to the key through `variant`, with their family.
const VARIANT_SURFACES_SQL: &str = r#"
    SELECT s.base_id, s.id, s.value
    FROM variant v
    JOIN surface_variant sv ON sv.variant_id = v.id
    JOIN surface s ON s.id = sv.surface_id
    WHERE v.value = ?1
"#;

const BASE_VALUE_SQL: &str = "SELECT value FROM base WHERE id = ?1";

/// A family's surfaces, streamed in index order and abandoned at the cap.
const SURFACES_SQL: &str = "SELECT value FROM surface WHERE base_id = ?1 ORDER BY id";

/// Variants of a family's surfaces, streamed in index order (duplicates are
/// dropped in Rust) and abandoned at the cap.
const VARIANTS_SQL: &str = r#"
    SELECT v.value
    FROM surface s
    JOIN surface_variant sv ON sv.surface_id = s.id
    JOIN variant v ON v.id = sv.variant_id
    WHERE s.base_id = ?1
    ORDER BY s.id, sv.variant_id
"#;

/// Rank offset of route 1 over route 0; the key position is added to it.
const SURFACE_ROUTE_RANK: usize = 10;

/// Terms already taken across the whole selection, bounded by
/// [`MAX_LEXICAL_FORMS`].
#[derive(Default)]
struct Picked {
    seen: HashSet<String>,
}

impl Picked {
    fn room(&self) -> usize {
        MAX_LEXICAL_FORMS.saturating_sub(self.seen.len())
    }

    /// Adds `form` to `terms` when it is a new single-word index term with at
    /// least [`MIN_FORM_LETTERS`] Hebrew letters. Returns true once `terms`
    /// holds `cap` entries.
    fn offer(&mut self, terms: &mut Vec<String>, form: &str, cap: usize) -> bool {
        if terms.len() < cap && normalize::hebrew_letter_count(form) >= MIN_FORM_LETTERS {
            if let Some(term) = normalize::to_index_term(form) {
                if !self.seen.contains(&term) {
                    self.seen.insert(term.clone());
                    terms.push(term);
                }
            }
        }
        terms.len() >= cap
    }
}

pub struct MagicDictionary {
    conn: Mutex<Connection>,
    cache: Mutex<LruCache<String, Arc<Vec<Family>>>>,
}

impl MagicDictionary {
    /// Opens `lexical.db` read-only. Fails if the file is missing or not a
    /// valid SQLite database — the caller treats that as "no dictionary".
    pub fn open(path: &Path) -> Result<Self> {
        crate::sqlite_host::ensure_ready()?;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening lexical.db at {}", path.display()))?;
        conn.execute_batch("PRAGMA query_only = ON;")
            .context("setting query_only on lexical.db")?;
        // Family rows are scattered; mapping avoids a read syscall per page miss.
        // Best-effort (SQLite falls back to reads); winClose releases the view.
        let _ = conn.execute_batch("PRAGMA mmap_size = 268435456;");
        // Preparing every query validates the schema here, so a wrong file
        // fails now instead of returning no expansions on every lookup.
        for sql in [
            BASE_BY_VALUE_SQL,
            BASE_BY_SURFACE_SQL,
            VARIANT_SURFACES_SQL,
            BASE_VALUE_SQL,
            SURFACES_SQL,
            VARIANTS_SQL,
        ] {
            conn.prepare_cached(sql)
                .context("lexical.db is missing the expected morphology schema")?;
        }

        let cache_size = if cfg!(any(target_os = "android", target_os = "ios")) {
            128
        } else {
            512
        };
        Ok(Self {
            conn: Mutex::new(conn),
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(cache_size).unwrap())),
        })
    }

    /// The selected families for `token`, cached by its first lookup key.
    /// Unknown tokens and query failures yield an empty list.
    fn families_for(&self, token: &str) -> Arc<Vec<Family>> {
        let cache_key = normalize::canonical_key(token);
        if cache_key.is_empty() {
            return Arc::new(Vec::new());
        }
        if let Some(hit) = self.cache.lock().unwrap().get(&cache_key) {
            return hit.clone();
        }
        let keys = normalize::lookup_keys(token);
        let selected = match self.conn.lock() {
            Ok(conn) => Self::select_in_one_read(&conn, token, &keys),
            Err(_) => return Arc::new(Vec::new()),
        };
        match selected {
            Ok(families) => {
                let families = Arc::new(families);
                self.cache.lock().unwrap().put(cache_key, families.clone());
                families
            }
            Err(_) => Arc::new(Vec::new()),
        }
    }

    /// Runs the whole selection in one read transaction: on Windows each
    /// implicit per-statement one re-locks the file and probes for a journal.
    fn select_in_one_read(
        conn: &Connection,
        token: &str,
        keys: &[String],
    ) -> rusqlite::Result<Vec<Family>> {
        let read = conn.unchecked_transaction()?;
        let families = Self::select_families(&read, token, keys)?;
        read.commit()?;
        Ok(families)
    }

    fn select_families(
        conn: &Connection,
        token: &str,
        keys: &[String],
    ) -> rusqlite::Result<Vec<Family>> {
        let lemmas = Self::match_lemmas(conn, keys)?;
        let mut picked = Picked::default();
        let mut families: Vec<Family> = Vec::new();
        for (rank, base_id, base) in &lemmas {
            let family_cap = if *rank < SURFACE_ROUTE_RANK {
                PRIMARY_FAMILY_CAP
            } else {
                SECONDARY_FAMILY_CAP
            };
            let cap = family_cap.min(picked.room());
            if cap == 0 {
                break;
            }
            let mut terms = Vec::new();
            if !picked.offer(&mut terms, base, cap)
                && !Self::stream_into(conn, SURFACES_SQL, *base_id, &mut picked, &mut terms, cap)?
            {
                Self::stream_into(conn, VARIANTS_SQL, *base_id, &mut picked, &mut terms, cap)?;
            }
            if !terms.is_empty() {
                families.push(Family {
                    base: base.clone(),
                    terms,
                });
            }
        }

        if picked.room() > 0 {
            let reached: HashSet<i64> = lemmas.iter().map(|(_, id, _)| *id).collect();
            Self::select_variant_route(conn, token, keys, &reached, &mut picked, &mut families)?;
        }
        Ok(families)
    }

    /// Routes 0 and 1 as `(rank, base_id, base)`, one row per family at its
    /// best rank: route offset plus the position of the matching key.
    fn match_lemmas(
        conn: &Connection,
        keys: &[String],
    ) -> rusqlite::Result<Vec<(usize, i64, String)>> {
        let mut lemmas: Vec<(usize, i64, String)> = Vec::new();
        for (route_rank, sql) in [
            (0, BASE_BY_VALUE_SQL),
            (SURFACE_ROUTE_RANK, BASE_BY_SURFACE_SQL),
        ] {
            let mut stmt = conn.prepare_cached(sql)?;
            for (position, key) in keys.iter().enumerate() {
                let mut rows = stmt.query(params![key])?;
                while let Some(row) = rows.next()? {
                    lemmas.push((route_rank + position, row.get(0)?, row.get(1)?));
                }
            }
        }
        lemmas.sort_unstable_by_key(|(rank, id, _)| (*id, *rank));
        lemmas.dedup_by_key(|(_, id, _)| *id);
        lemmas.sort_unstable_by_key(|(rank, id, _)| (*rank, *id));
        Ok(lemmas)
    }

    /// Feeds column 0 of a per-family query into `terms` until `cap`; returns
    /// whether the cap was reached. Rows past the cap are never stepped.
    fn stream_into(
        conn: &Connection,
        sql: &str,
        base_id: i64,
        picked: &mut Picked,
        terms: &mut Vec<String>,
        cap: usize,
    ) -> rusqlite::Result<bool> {
        let mut stmt = conn.prepare_cached(sql)?;
        let mut rows = stmt.query(params![base_id])?;
        while let Some(row) = rows.next()? {
            if picked.offer(terms, row.get_ref(0)?.as_str()?, cap) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Route 2: stem-sharing surfaces linked to a key through `variant`, from
    /// families not reached by routes 0/1, in family-id then surface-id order.
    fn select_variant_route(
        conn: &Connection,
        token: &str,
        keys: &[String],
        reached: &HashSet<i64>,
        picked: &mut Picked,
        families: &mut Vec<Family>,
    ) -> rusqlite::Result<()> {
        let probe = StemProbe::new(token);
        let mut matched: Vec<(i64, i64, String)> = Vec::new();
        let mut stmt = conn.prepare_cached(VARIANT_SURFACES_SQL)?;
        for key in keys {
            let mut rows = stmt.query(params![key])?;
            while let Some(row) = rows.next()? {
                let base_id: i64 = row.get(0)?;
                if reached.contains(&base_id) {
                    continue;
                }
                let surface = row.get_ref(2)?.as_str()?;
                if normalize::hebrew_letter_count(surface) >= MIN_VARIANT_SURFACE_LETTERS
                    && probe.matches(surface)
                {
                    matched.push((base_id, row.get(1)?, surface.to_owned()));
                }
            }
        }
        matched.sort_unstable_by_key(|(base_id, surface_id, _)| (*base_id, *surface_id));
        matched.dedup_by_key(|(_, surface_id, _)| *surface_id);

        let mut base_value = conn.prepare_cached(BASE_VALUE_SQL)?;
        for group in matched.chunk_by(|a, b| a.0 == b.0) {
            let cap = VARIANT_ROUTE_CAP.min(picked.room());
            if cap == 0 {
                break;
            }
            let mut terms = Vec::new();
            for (_, _, surface) in group {
                if picked.offer(&mut terms, surface, cap) {
                    break;
                }
            }
            if !terms.is_empty() {
                let base = base_value.query_row(params![group[0].0], |r| r.get(0))?;
                families.push(Family { base, terms });
            }
        }
        Ok(())
    }

    /// Index-ready search terms for `token`, capped at `cap` — used for
    /// **recall** (no blacklist filtering, to preserve matches).
    pub fn recall_forms(&self, token: &str, cap: usize) -> Vec<String> {
        self.collect_forms(token, cap, false)
    }

    /// Like [`recall_forms`](Self::recall_forms) but withholds forms whose lemma
    /// is blacklisted for this token — used for **highlighting** only.
    pub fn highlight_forms(&self, token: &str, cap: usize) -> Vec<String> {
        self.collect_forms(token, cap, true)
    }

    fn collect_forms(&self, token: &str, cap: usize, apply_blacklist: bool) -> Vec<String> {
        // The cached selection holds at most `MAX_LEXICAL_FORMS` terms.
        let cap = cap.min(MAX_LEXICAL_FORMS);
        self.families_for(token)
            .iter()
            .filter(|family| !(apply_blacklist && blacklist::is_blacklisted(token, &family.base)))
            .flat_map(|family| family.terms.iter().cloned())
            .take(cap)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::time::Instant;
    use tempfile::tempdir;

    const SCHEMA: &str = r#"
        CREATE TABLE base (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
        CREATE TABLE surface (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE, base_id INTEGER NOT NULL REFERENCES base(id), notes TEXT);
        CREATE TABLE variant (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
        CREATE TABLE surface_variant (surface_id INTEGER NOT NULL REFERENCES surface(id), variant_id INTEGER NOT NULL REFERENCES variant(id), PRIMARY KEY (surface_id, variant_id));
        CREATE INDEX surface_base_id_index ON surface(base_id);
        CREATE INDEX surface_variant_variant_id_index ON surface_variant(variant_id);
    "#;

    /// A lexicon shaped like the real one: finals and gershayim stored as
    /// written, one legacy folded base, and foreign families that a variant
    /// link attaches to `שבת` and `תשובה`.
    const FIXTURE: &str = r#"
        INSERT INTO base (id, value) VALUES
            (1, 'הלכ'), (2, 'בת'), (3, 'שבת'), (4, 'לא'), (5, 'כן'), (6, 'שוב'),
            (7, 'תשובה'), (8, 'תפילין'), (9, 'רמב"ם'), (10, 'רמבם'), (11, 'אדמדמ'),
            (12, 'ישב');
        INSERT INTO surface (id, value, base_id) VALUES
            (1, 'הלכתי', 1), (2, 'הולכ', 1),
            (10, 'בת', 2), (11, 'ובת', 2), (12, 'בתו', 2), (13, 'שבתו', 2), (14, 'בבת', 2),
            (20, 'בשבת', 3), (21, 'שבתות', 3), (22, 'השבת', 3),
            (30, 'ולא', 4), (31, 'א'' דלא', 4),
            (40, 'וכן', 5), (41, 'לחתוך', 5),
            (50, 'תשוב', 6), (51, 'ישובו', 6),
            (60, 'בתשובה', 7), (61, 'תשובות', 7), (62, 'ת''', 7),
            (70, 'בתפילין', 8), (71, 'תפלין', 8),
            (80, 'הרמב"ם', 9), (81, 'לרמב"ם', 9),
            (90, 'הרמבם', 10), (91, 'הרמב״ם', 10),
            (100, 'אדמדמים', 11),
            (110, 'שב', 12), (111, 'ישבו', 12);
        INSERT INTO variant (id, value) VALUES (1, 'הלכ'), (2, 'שבת'), (3, 'תשובה');
        INSERT INTO surface_variant (surface_id, variant_id) VALUES
            (1, 1),
            (13, 2), (14, 2), (20, 2), (110, 2),
            (31, 3), (40, 3), (50, 3), (51, 3), (60, 3);
    "#;

    fn open_fixture(extra_sql: &str) -> (tempfile::TempDir, MagicDictionary) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lexical.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(FIXTURE).unwrap();
        conn.execute_batch(extra_sql).unwrap();
        drop(conn);
        let dict = MagicDictionary::open(&path).unwrap();
        (dir, dict)
    }

    fn has(forms: &[String], form: &str) -> bool {
        forms.iter().any(|f| f == form)
    }

    #[test]
    fn open_rejects_non_database() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.db");
        assert!(MagicDictionary::open(&path).is_err());
    }

    #[test]
    fn surface_token_reaches_its_family_as_index_terms() {
        let (_dir, dict) = open_fixture("");
        // "הלכתי" is a surface (route 1); medial-final DB forms are re-finalized.
        let forms = dict.recall_forms("הלכתי", 32);
        assert_eq!(forms, vec!["הלך", "הלכתי", "הולך"]);
    }

    #[test]
    fn unknown_token_returns_empty() {
        let (_dir, dict) = open_fixture("");
        assert!(dict.recall_forms("לאקיים", 32).is_empty());
        assert!(dict.recall_forms("  ", 32).is_empty());
    }

    #[test]
    fn final_letters_are_looked_up_as_written() {
        let (_dir, dict) = open_fixture("");
        let forms = dict.recall_forms("תפילין", 32);
        assert_eq!(forms.first().map(String::as_str), Some("תפילין"));
        assert!(has(&forms, "בתפילין") && has(&forms, "תפלין"));
        // Nikud is stripped; the defective spelling is a surface of the family.
        assert!(has(&dict.recall_forms("תְּפִלִּין", 32), "בתפילין"));
        // A base stored folded is still reached through the legacy key.
        assert!(has(&dict.recall_forms("אדמדם", 32), "אדמדמים"));
    }

    #[test]
    fn gershayim_reach_both_spellings_and_fold_to_ascii() {
        let (_dir, dict) = open_fixture("");
        for token in ["רמב\"ם", "רמב\u{05F4}ם"] {
            let forms = dict.recall_forms(token, 32);
            assert_eq!(forms.first().map(String::as_str), Some("רמב\"ם"), "{token}");
            assert!(has(&forms, "הרמב\"ם") && has(&forms, "לרמב\"ם"), "{token}");
            assert!(has(&forms, "רמבם") && has(&forms, "הרמבם"), "{token}");
            assert!(
                forms.iter().all(|f| !f.contains('\u{05F4}')),
                "{token}: {forms:?}"
            );
        }
    }

    #[test]
    fn variant_route_takes_only_stem_sharing_surfaces() {
        let (_dir, dict) = open_fixture("");
        let forms = dict.recall_forms("שבת", 32);
        assert_eq!(forms.first().map(String::as_str), Some("שבת"));
        assert!(has(&forms, "בשבת") && has(&forms, "שבתו"));
        // "שב" shares the stem but is too short to trust through a variant.
        for foreign in ["בת", "ובת", "בתו", "בבת", "שב"] {
            assert!(!has(&forms, foreign), "{foreign} leaked: {forms:?}");
        }

        let forms = dict.recall_forms("תשובה", 32);
        assert!(has(&forms, "בתשובה") && has(&forms, "תשוב"));
        for foreign in ["לא", "ולא", "כן", "וכן", "ישובו", "שוב"] {
            assert!(!has(&forms, foreign), "{foreign} leaked: {forms:?}");
        }
    }

    #[test]
    fn forms_with_fewer_than_two_hebrew_letters_are_dropped() {
        let (_dir, dict) = open_fixture("");
        for forms in [
            dict.recall_forms("תשובה", 32),
            dict.highlight_forms("תשובה", 32),
        ] {
            assert!(has(&forms, "תשובות"));
            assert!(!has(&forms, "ת'"), "{forms:?}");
        }
    }

    #[test]
    fn highlight_withholds_blacklisted_family_only() {
        let (_dir, dict) = open_fixture("");
        // "לחתוך" → base "כן" is listed in the hallucination blacklist.
        let recall = dict.recall_forms("לחתוך", 32);
        assert!(has(&recall, "כן") && has(&recall, "וכן"));
        let highlight = dict.highlight_forms("לחתוך", 32);
        assert!(!has(&highlight, "כן") && !has(&highlight, "וכן"));
    }

    /// 100 surfaces × 100 variants in one family: the selection streams rows
    /// and stops at the family cap instead of reading the cross-product.
    #[test]
    fn family_caps_bound_the_selection() {
        let mut sql = String::from(
            "INSERT INTO base (id, value) VALUES (200, 'בסיס');
             INSERT INTO base (id, value) VALUES (201, 'אחר');",
        );
        for i in 0..100 {
            sql.push_str(&format!(
                "INSERT INTO surface (id, value, base_id) VALUES ({}, 'צורהש{i}', 200);
                 INSERT INTO variant (id, value) VALUES ({}, 'וריאנט{i}');",
                1000 + i,
                1000 + i
            ));
        }
        sql.push_str(
            "INSERT INTO surface_variant (surface_id, variant_id)
             SELECT s.id, v.id FROM surface s, variant v WHERE s.base_id = 200 AND v.id >= 1000;",
        );
        // A second family in which "בסיס" is a surface: route 1, capped at 8.
        for i in 0..20 {
            sql.push_str(&format!(
                "INSERT INTO surface (id, value, base_id) VALUES ({}, 'אחרת{i}', 201);",
                2000 + i
            ));
        }
        sql.push_str("INSERT INTO surface (id, value, base_id) VALUES (3000, 'בסיסי', 201);");
        let (_dir, dict) = open_fixture(&sql);

        let forms = dict.recall_forms("בסיס", 32);
        assert_eq!(forms.len(), PRIMARY_FAMILY_CAP);
        assert_eq!(forms[0], "בסיס");
        assert!(forms[1..].iter().all(|f| f.starts_with("צורהש")));

        let forms = dict.recall_forms("צורהש0", usize::MAX);
        assert_eq!(forms.len(), SECONDARY_FAMILY_CAP);

        let forms = dict.recall_forms("בסיסי", 32);
        assert_eq!(forms.len(), SECONDARY_FAMILY_CAP);
        assert_eq!(forms[0], "אחר");
    }

    #[test]
    fn global_cap_spans_several_primary_families() {
        // Both quote spellings are route-0 bases; together they exceed the cap.
        let mut sql = String::new();
        for i in 0..40 {
            sql.push_str(&format!(
                "INSERT INTO surface (id, value, base_id) VALUES ({}, 'א{i}רמב\"ם', 9);
                 INSERT INTO surface (id, value, base_id) VALUES ({}, 'ב{i}רמבם', 10);",
                5000 + i,
                6000 + i
            ));
        }
        let (_dir, dict) = open_fixture(&sql);
        let forms = dict.recall_forms("רמב\"ם", usize::MAX);
        assert_eq!(forms.len(), MAX_LEXICAL_FORMS);
        let unique: HashSet<&String> = forms.iter().collect();
        assert_eq!(unique.len(), forms.len());
        assert_eq!(dict.recall_forms("רמב\"ם", 5).len(), 5);
    }

    fn real_db() -> MagicDictionary {
        let path = std::env::var("OTZ_LEXICAL_DB").expect("set OTZ_LEXICAL_DB to lexical.db");
        MagicDictionary::open(Path::new(&path)).unwrap()
    }

    /// Index terms of one family in the real DB (read-only), for leak checks.
    fn real_family_terms(base: &str) -> HashSet<String> {
        let path = std::env::var("OTZ_LEXICAL_DB").unwrap();
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT b.value FROM base b WHERE b.value = ?1
                 UNION ALL
                 SELECT s.value FROM surface s JOIN base b ON b.id = s.base_id WHERE b.value = ?1",
            )
            .unwrap();
        let values: Vec<String> = stmt
            .query_map([base], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        values
            .iter()
            .filter_map(|v| normalize::to_index_term(v))
            .collect()
    }

    #[test]
    #[ignore = "needs the real lexical.db via OTZ_LEXICAL_DB"]
    fn real_lexical_db_selection() {
        let dict = real_db();
        for word in ["שבת", "תשובה", "תפילין", "רמב\"ם", "מלך", "הלך"] {
            println!(
                "RECALL {word} => {:?}",
                dict.recall_forms(word, MAX_LEXICAL_FORMS)
            );
        }

        let shabbat = dict.recall_forms("שבת", MAX_LEXICAL_FORMS);
        assert_eq!(shabbat.first().map(String::as_str), Some("שבת"));
        let own = real_family_terms("שבת");
        let first_foreign = shabbat.iter().position(|f| !own.contains(f));
        assert!(
            first_foreign.is_none_or(|i| shabbat[i..].iter().all(|f| !own.contains(f))),
            "family שבת must come first: {shabbat:?}"
        );
        let bat = real_family_terms("בת");
        let leaked: Vec<&String> = shabbat
            .iter()
            .filter(|f| bat.contains(*f) && !own.contains(*f) && f.as_str() != "שבתו")
            .collect();
        assert!(leaked.is_empty(), "family בת leaked {leaked:?}");

        for word in ["שבת", "תשובה", "תפילין", "רמב\"ם", "מלך", "הלך"] {
            for forms in [
                dict.recall_forms(word, MAX_LEXICAL_FORMS),
                dict.highlight_forms(word, MAX_LEXICAL_FORMS),
            ] {
                let short: Vec<&String> = forms
                    .iter()
                    .filter(|f| normalize::hebrew_letter_count(f) < MIN_FORM_LETTERS)
                    .collect();
                assert!(short.is_empty(), "{word}: too short {short:?}");
            }
        }
        assert!(!has(&dict.recall_forms("הלך", MAX_LEXICAL_FORMS), "הל"));

        let teshuva = dict.recall_forms("תשובה", MAX_LEXICAL_FORMS);
        assert!(!has(&teshuva, "לא") && !has(&teshuva, "וכן"), "{teshuva:?}");
        assert!(!dict.recall_forms("תפילין", MAX_LEXICAL_FORMS).is_empty());
        let rambam = dict.recall_forms("רמב\"ם", MAX_LEXICAL_FORMS);
        // The literal spelling's family leads even though `רמבם` has a lower id.
        assert_eq!(rambam.first().map(String::as_str), Some("רמב\"ם"));
    }

    #[test]
    #[ignore = "needs the real lexical.db via OTZ_LEXICAL_DB"]
    fn real_lexical_db_latency() {
        let words = [
            "שבת",
            "תשובה",
            "תפילין",
            "רמב\"ם",
            "מלך",
            "הלך",
            "אמר",
            "ירושלים",
            "שולחן",
            "לאקיים",
        ];
        // Cold: a fresh connection. Uncached: a second connection with the
        // file in the OS cache but an empty LRU. Cached: LRU hits.
        let time_first_lookups = |dict: &MagicDictionary| {
            words.map(|word| {
                let started = Instant::now();
                let forms = dict.recall_forms(word, MAX_LEXICAL_FORMS);
                (started.elapsed(), forms.len())
            })
        };
        let cold = time_first_lookups(&real_db());
        let dict = real_db();
        let uncached = time_first_lookups(&dict);
        const ROUNDS: u32 = 1000;
        for (i, word) in words.iter().enumerate() {
            let started = Instant::now();
            for _ in 0..ROUNDS {
                std::hint::black_box(dict.highlight_forms(word, MAX_LEXICAL_FORMS));
            }
            let cached = started.elapsed() / ROUNDS;
            println!(
                "LATENCY {word}: cold {:?}, uncached {:?}, cached {cached:?}, {} forms",
                cold[i].0, uncached[i].0, uncached[i].1
            );
        }
    }
}
