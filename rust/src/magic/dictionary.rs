//! Read-only access to `lexical.db` — the offline-built Hebrew morphology
//! lexicon. Given a query token it returns the **real, learned** surface forms,
//! spelling variants, and base (lemma) linked through the same lemma, which the
//! fuzzy search path injects as extra term alternatives.
//!
//! This is purely additive recall for the *approximate* (`fuzzy`) mode. It never
//! touches exact search, and if the DB is missing or unreadable the engine
//! simply falls back to plain `FuzzyTermQuery` behaviour.

use super::{blacklist, normalize};
use anyhow::{Context, Result};
use lru::LruCache;
use rusqlite::{params, Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// All forms tied to one lemma (one `base` row) for a looked-up token.
#[derive(Clone, Debug, Default)]
pub struct Expansion {
    pub base: String,
    pub surface: Vec<String>,
    pub variants: Vec<String>,
}

/// Order-preserving accumulator used while grouping `LOOKUP_SQL` rows by lemma.
/// The `*_seen` sets make dedup O(1) per row instead of `Vec::contains`.
struct Grouping {
    base: String,
    surface: Vec<String>,
    surface_seen: HashSet<String>,
    variants: Vec<String>,
    variant_seen: HashSet<String>,
}

impl Grouping {
    fn new(base: String) -> Self {
        Self {
            base,
            surface: Vec::new(),
            surface_seen: HashSet::new(),
            variants: Vec::new(),
            variant_seen: HashSet::new(),
        }
    }

    fn into_expansion(self) -> Expansion {
        Expansion {
            base: self.base,
            surface: self.surface,
            variants: self.variants,
        }
    }
}

/// Resolves a lemma from any of its surfaces/variants, then collects every
/// surface and variant under that lemma. The three `?` placeholders all receive
/// the same normalized token (matched as surface, base, and variant in turn).
const LOOKUP_SQL: &str = r#"
    WITH matches AS (
        SELECT s.base_id FROM surface s WHERE s.value = ?1
        UNION
        SELECT b.id FROM base b WHERE b.value = ?1
        UNION
        SELECT s.base_id FROM variant v
          JOIN surface_variant sv ON sv.variant_id = v.id
          JOIN surface s ON sv.surface_id = s.id
          WHERE v.value = ?1
    )
    SELECT b.id AS base_id, b.value AS base,
           s.value AS surface, v.value AS variant
    FROM base b
    JOIN matches m ON m.base_id = b.id
    LEFT JOIN surface s ON s.base_id = b.id
    LEFT JOIN surface_variant sv ON sv.surface_id = s.id
    LEFT JOIN variant v ON sv.variant_id = v.id
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
        // than silently returning no expansions on every lookup.
        conn.prepare(LOOKUP_SQL)
            .context("lexical.db is missing the expected morphology schema")?;

        let cache_size = if cfg!(target_os = "android") { 128 } else { 512 };
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

    /// Runs `LOOKUP_SQL` and groups the flat row set by lemma.
    fn fetch_expansions(&self, key: &str) -> Vec<Expansion> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut stmt = match conn.prepare_cached(LOOKUP_SQL) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![key], |row| {
            Ok((
                row.get::<_, i64>(0)?,            // base_id
                row.get::<_, String>(1)?,         // base
                row.get::<_, Option<String>>(2)?, // surface
                row.get::<_, Option<String>>(3)?, // variant
            ))
        });
        let rows = match rows {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        // `LOOKUP_SQL` returns the surface×variant cross-product per lemma, so a
        // single form repeats across many rows. Dedup with `HashSet` guards
        // (O(1)) instead of `Vec::contains` (O(n²)) — for lemmas with many
        // variants this is exactly where it matters. Insertion order is kept so
        // the per-token cap is applied deterministically downstream.
        let mut order: Vec<i64> = Vec::new();
        let mut groups: HashMap<i64, Grouping> = HashMap::new();
        for row in rows.flatten() {
            let (base_id, base, surface, variant) = row;
            let group = groups.entry(base_id).or_insert_with(|| {
                order.push(base_id);
                Grouping::new(base)
            });
            if let Some(s) = surface {
                if group.surface_seen.insert(s.clone()) {
                    group.surface.push(s);
                }
            }
            if let Some(v) = variant {
                if group.variant_seen.insert(v.clone()) {
                    group.variants.push(v);
                }
            }
        }
        order
            .into_iter()
            .filter_map(|id| groups.remove(&id).map(Grouping::into_expansion))
            .collect()
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
}
