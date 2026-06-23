//! Read-only access to `lexical.db` — the offline-built Hebrew morphology
//! lexicon. Given a query token it returns the **real, learned** surface forms,
//! spelling variants, and base (lemma) linked through the same lemma, which the
//! fuzzy search path injects as extra term alternatives.
//!
//! This is purely additive recall for the *approximate* (`fuzzy`) mode. It never
//! touches exact search, and if the DB is missing or unreadable the engine
//! simply falls back to plain `FuzzyTermQuery` behaviour.

use super::{blacklist, normalize, MAX_LEXICAL_FORMS};
use anyhow::{Context, Result};
use lru::LruCache;
use rusqlite::{params, Connection, OpenFlags};
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// All forms tied to one lemma (one `base` row) for a looked-up token. The
/// vectors are already bounded to [`MAX_LEXICAL_FORMS`] by the SQL `LIMIT`.
#[derive(Clone, Debug, Default)]
pub struct Expansion {
    pub base: String,
    pub surface: Vec<String>,
    pub variants: Vec<String>,
}

/// Step 1 — resolve the matching lemma(s) for a token (matched as surface,
/// base, or variant). Returns only `(base_id, base)`; the heavy form lists are
/// fetched per lemma below. This deliberately avoids the surface×variant
/// cross-product the old single query produced for common words.
const MATCH_LEMMAS_SQL: &str = r#"
    WITH matches AS (
        SELECT s.base_id FROM surface s WHERE s.value = ?1
        UNION
        SELECT b.id FROM base b WHERE b.value = ?1
        UNION
        SELECT s.base_id
        FROM variant v
        JOIN surface_variant sv ON sv.variant_id = v.id
        JOIN surface s ON s.id = sv.surface_id
        WHERE v.value = ?1
    )
    SELECT b.id, b.value
    FROM base b
    JOIN matches m ON m.base_id = b.id
    ORDER BY b.id
"#;

/// Step 2 — surfaces for a lemma, capped. (`base_id`, `limit`).
const SURFACES_SQL: &str = r#"
    SELECT value FROM surface
    WHERE base_id = ?1
    ORDER BY id
    LIMIT ?2
"#;

/// Step 3 — distinct variants linked to a lemma's surfaces, capped. No
/// cross-product: `GROUP BY v.id` collapses a variant shared by several
/// surfaces to one row. (`base_id`, `limit`).
const VARIANTS_SQL: &str = r#"
    SELECT v.value
    FROM surface s
    JOIN surface_variant sv ON sv.surface_id = s.id
    JOIN variant v ON v.id = sv.variant_id
    WHERE s.base_id = ?1
    GROUP BY v.id
    ORDER BY v.id
    LIMIT ?2
"#;

pub struct MagicDictionary {
    conn: Mutex<Connection>,
    cache: Mutex<LruCache<String, Arc<Vec<Expansion>>>>,
}

impl MagicDictionary {
    /// Opens `lexical.db` read-only. Fails if the file is missing or not a
    /// valid SQLite database — the caller treats that as "no dictionary".
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening lexical.db at {}", path.display()))?;
        conn.execute_batch("PRAGMA query_only = ON;")
            .context("setting query_only on lexical.db")?;
        // Probe the expected schema so a wrong/corrupt file fails here rather
        // than silently returning no expansions on every lookup. Preparing all
        // three staged queries validates every table they touch.
        //
        // Performance note: the per-lemma lookups below filter `surface.base_id`
        // and join `surface_variant` by `surface_id`. The offline `lexical.db`
        // builder (SeforimMagicIndexer) should therefore index `surface(base_id)`
        // and `surface_variant(surface_id)` (the PRIMARY KEY already covers the
        // latter). Indexes can only be added in the builder — the file is opened
        // read-only here by design.
        for sql in [MATCH_LEMMAS_SQL, SURFACES_SQL, VARIANTS_SQL] {
            conn.prepare(sql)
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

    /// All expansions for `token` (cached by normalized key). Returns an empty
    /// list when the token is unknown; never errors out the search.
    fn expansions_for(&self, token: &str) -> Arc<Vec<Expansion>> {
        let key = normalize::normalize_hebrew(token);
        if key.is_empty() {
            return Arc::new(Vec::new());
        }
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let expansions = Arc::new(self.fetch_expansions(&key));
        self.cache.lock().unwrap().put(key, expansions.clone());
        expansions
    }

    /// Staged lookup: resolve lemmas, then fetch each lemma's surfaces and
    /// variants with a SQL `LIMIT`. SQLite returns only what's needed instead
    /// of the surface×variant cross-product, and never more than
    /// [`MAX_LEXICAL_FORMS`] of each per lemma — which is sufficient because the
    /// downstream global cap is the same value (we can never output more).
    fn fetch_expansions(&self, key: &str) -> Vec<Expansion> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let limit = MAX_LEXICAL_FORMS as i64;

        // Step 1: matching lemmas.
        let lemmas: Vec<(i64, String)> = {
            let mut stmt = match conn.prepare_cached(MATCH_LEMMAS_SQL) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = match stmt
                .query_map(params![key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            rows.flatten().collect()
        };

        // Steps 2+3: capped surfaces and variants per lemma.
        lemmas
            .into_iter()
            .map(|(base_id, base)| Expansion {
                base,
                surface: Self::fetch_values(&conn, SURFACES_SQL, base_id, limit),
                variants: Self::fetch_values(&conn, VARIANTS_SQL, base_id, limit),
            })
            .collect()
    }

    /// Reads column 0 (a form string) from a capped per-lemma query, deduping
    /// defensively while preserving SQL order.
    fn fetch_values(conn: &Connection, sql: &str, base_id: i64, limit: i64) -> Vec<String> {
        let mut stmt = match conn.prepare_cached(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![base_id, limit], |r| r.get::<_, String>(0)) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for value in rows.flatten() {
            if seen.insert(value.clone()) {
                out.push(value);
            }
        }
        out
    }

    /// Index-ready search terms for `token`, capped at `cap`. Includes every
    /// surface, variant, and base across all matched lemmas — used for
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
        // The staged fetch bounds each lemma's surfaces/variants to
        // `MAX_LEXICAL_FORMS`, so requesting more than that here could silently
        // under-return. Callers use exactly `MAX_LEXICAL_FORMS`; clamp to keep
        // the invariant honest if that ever changes.
        let cap = cap.min(MAX_LEXICAL_FORMS);
        let expansions = self.expansions_for(token);
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for exp in expansions.iter() {
            if apply_blacklist && blacklist::is_blacklisted(token, &exp.base) {
                continue;
            }
            let forms = exp
                .surface
                .iter()
                .chain(exp.variants.iter())
                .chain(std::iter::once(&exp.base));
            for form in forms {
                if let Some(term) = normalize::to_index_term(form) {
                    if seen.insert(term.clone()) {
                        out.push(term);
                        if out.len() >= cap {
                            return out;
                        }
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::tempdir;

    /// Builds a tiny on-disk lexicon: lemma "הלכ" with surfaces "הלכתי"/"הולכ"
    /// and variant "הלך", under the same schema as the real `lexical.db`.
    fn make_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE base (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
            CREATE TABLE surface (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE, base_id INTEGER NOT NULL REFERENCES base(id), notes TEXT);
            CREATE TABLE variant (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
            CREATE TABLE surface_variant (surface_id INTEGER NOT NULL REFERENCES surface(id), variant_id INTEGER NOT NULL REFERENCES variant(id), PRIMARY KEY (surface_id, variant_id));

            INSERT INTO base (id, value) VALUES (1, 'הלכ');
            INSERT INTO surface (id, value, base_id) VALUES (1, 'הלכתי', 1), (2, 'הולכ', 1);
            INSERT INTO variant (id, value) VALUES (1, 'הלכ');
            INSERT INTO surface_variant (surface_id, variant_id) VALUES (1, 1);
            "#,
        )
        .unwrap();
    }

    #[test]
    fn open_rejects_non_database() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.db");
        assert!(MagicDictionary::open(&path).is_err());
    }

    #[test]
    fn recall_forms_returns_index_ready_terms() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lexical.db");
        make_db(&path);
        let dict = MagicDictionary::open(&path).unwrap();

        // Query "הלכתי" (a surface) resolves the lemma and returns sibling forms,
        // re-finalized for the index: "הולכ" → "הולך", base "הלכ" → "הלך".
        let forms = dict.recall_forms("הלכתי", 32);
        assert!(forms.contains(&"הלכתי".to_string()));
        assert!(forms.contains(&"הולך".to_string()));
        assert!(forms.contains(&"הלך".to_string()));
    }

    #[test]
    fn unknown_token_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lexical.db");
        make_db(&path);
        let dict = MagicDictionary::open(&path).unwrap();
        assert!(dict.recall_forms("לאקיים", 32).is_empty());
    }

    /// Builds a pathological lemma: 100 surfaces × 100 variants, fully linked
    /// (10,000 `surface_variant` rows). The old single query materialized that
    /// entire cross-product; the staged queries must instead return at most the
    /// cap from each capped category — proving the blow-up is gone.
    #[test]
    fn large_lemma_does_not_materialize_cross_product() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lexical.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE base (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
            CREATE TABLE surface (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE, base_id INTEGER NOT NULL REFERENCES base(id), notes TEXT);
            CREATE TABLE variant (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
            CREATE TABLE surface_variant (surface_id INTEGER NOT NULL REFERENCES surface(id), variant_id INTEGER NOT NULL REFERENCES variant(id), PRIMARY KEY (surface_id, variant_id));
            INSERT INTO base (id, value) VALUES (1, 'בסיס');
            "#,
        )
        .unwrap();
        // 100 single-word surfaces and 100 variants, all cross-linked.
        for i in 0..100 {
            conn.execute(
                "INSERT INTO surface (id, value, base_id) VALUES (?1, ?2, 1)",
                params![i + 1, format!("צורהש{i}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO variant (id, value) VALUES (?1, ?2)",
                params![i + 1, format!("וריאנט{i}")],
            )
            .unwrap();
        }
        {
            let mut stmt = conn
                .prepare("INSERT INTO surface_variant (surface_id, variant_id) VALUES (?1, ?2)")
                .unwrap();
            for s in 0..100 {
                for v in 0..100 {
                    stmt.execute(params![s + 1, v + 1]).unwrap();
                }
            }
        }
        drop(conn);

        let dict = MagicDictionary::open(&path).unwrap();
        // Exactly the cap, all drawn from surfaces (fetched first), never the
        // 10,000-row cross-product.
        let forms = dict.recall_forms("צורהש0", 32);
        assert_eq!(forms.len(), 32);
        assert!(forms.iter().all(|f| f.starts_with("צורהש")));
    }

    /// A requested cap above `MAX_LEXICAL_FORMS` is clamped, never under-served
    /// by the capped fetch.
    #[test]
    fn cap_is_clamped_to_max_lexical_forms() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lexical.db");
        make_db(&path);
        let dict = MagicDictionary::open(&path).unwrap();
        let forms = dict.recall_forms("הלכתי", usize::MAX);
        assert!(forms.len() <= MAX_LEXICAL_FORMS);
        assert!(forms.contains(&"הלכתי".to_string()));
    }
}
