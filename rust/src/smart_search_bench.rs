//! Real-data benchmark of smart search on an index another process owns, opened through
//! [`ReadOnlyDirectory`]; every test is `#[ignore]` and env-driven (see [`Config`]).
//!
//! Run: `cargo test --release --features semantic --lib smart_search_bench -- --ignored
//! --nocapture --test-threads=1`.
//!
//! Adding a mode: a [`Mode`] variant, its `label`/`paged`/`available`, an arm in [`run_once`]
//! and an entry in [`MODES`]; rows, summary and CSV follow.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tantivy::directory::error::{
    DeleteError, LockError, OpenDirectoryError, OpenReadError, OpenWriteError,
};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, FileSlice, Lock, MmapDirectory, WatchCallback,
    WatchHandle, WritePtr,
};
use tantivy::Index;

use crate::api::search_engine::{configure_line_source, ResultsOrder, SearchEngine};
use crate::magic::{MagicDictionary, MAX_LEXICAL_FORMS};

/// The queries of the smart-search plan, by group.
const QUERIES: &[(&str, &[&str])] = &[
    (
        "short",
        &[
            "שבת",
            "תפילין",
            "תשובה",
            "ברכה",
            "מלך",
            "הלך",
            "אמונה",
            "צדקה",
        ],
    ),
    (
        "acronym",
        &["רמב\"ם", "רמב״ם", "רש\"י", "שו\"ע", "חז\"ל", "ז\"ל"],
    ),
    ("quoted", &["\"שמע ישראל\"", "\"ויאמר משה\""]),
    (
        "concept",
        &[
            "כבוד אב ואם",
            "תשובה מאהבה",
            "פיקוח נפש דוחה שבת",
            "הכנסת אורחים",
            "שמירת הלשון",
            "מעלת השלום",
            "ענווה של משה רבינו",
            "גדול תלמוד תורה",
            "יסורים של אהבה",
            "למה נברא האדם יחידי",
            "מצות תפילין בלילה",
            "אין עומדין להתפלל אלא מתוך כובד ראש",
        ],
    ),
    ("ref", &["בראשית א א", "ברכות ב"]),
];

/// The words whose dictionary forms are dumped.
const FORM_WORDS: &[&str] = &[
    "שבת",
    "תפילין",
    "תשובה",
    "רמב\"ם",
    "רמב״ם",
    "רמבם",
    "מלך",
    "הלך",
];

/// One way of running a query. Lexical phases take the window `offset + 2 * limit` that
/// `search_semantic` asks for; API modes take the page itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// `search_semantic`'s lexical phase in Exact.
    LexExact,
    /// `search_semantic`'s lexical phase in Fuzzy, at this distance, with the dictionary.
    LexFuzzy(u8),
    /// The regular exact search, one page.
    ApiExact,
    /// The regular fuzzy search, one page.
    ApiFuzzy(u8),
    /// The regular fuzzy count, once per query.
    ApiFuzzyCount(u8),
}

const MODES: &[Mode] = &[
    Mode::LexExact,
    Mode::LexFuzzy(0),
    Mode::LexFuzzy(1),
    Mode::ApiExact,
    Mode::ApiFuzzy(1),
    Mode::ApiFuzzyCount(1),
];

impl Mode {
    fn label(self) -> String {
        match self {
            Mode::LexExact => "lex_exact".into(),
            Mode::LexFuzzy(d) => format!("lex_fuzzy{d}"),
            Mode::ApiExact => "api_exact".into(),
            Mode::ApiFuzzy(d) => format!("api_fuzzy{d}"),
            Mode::ApiFuzzyCount(d) => format!("api_fuzzy{d}_count"),
        }
    }

    fn paged(self) -> bool {
        !matches!(self, Mode::ApiFuzzyCount(_))
    }

    fn available(self) -> bool {
        !matches!(self, Mode::LexExact | Mode::LexFuzzy(_))
            || cfg!(feature = "semantic-integration")
    }
}

/// `OTZ_INDEX` (required), `OTZ_LIBRARY_DB`, `OTZ_LEXICAL_DB`, `OTZ_RUNS` (5), `OTZ_PAGES` (3),
/// `OTZ_LIMIT` (30), `OTZ_OUT` (csv; `_summary.txt` and `_forms.txt` beside it). Optional
/// filters: `OTZ_ONLY` (queries, `|`-separated) and `OTZ_MODES` (labels, `,`-separated).
struct Config {
    index: PathBuf,
    library_db: Option<PathBuf>,
    lexical_db: Option<PathBuf>,
    runs: usize,
    pages: u32,
    limit: u32,
    out: PathBuf,
}

impl Config {
    fn from_env() -> Option<Self> {
        let index = PathBuf::from(std::env::var_os("OTZ_INDEX")?);
        let path = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let number = |name: &str, default: u32| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(default)
        };
        Some(Self {
            index,
            library_db: path("OTZ_LIBRARY_DB"),
            lexical_db: path("OTZ_LEXICAL_DB"),
            runs: number("OTZ_RUNS", 5).max(1) as usize,
            pages: number("OTZ_PAGES", 3).max(1),
            limit: number("OTZ_LIMIT", 30).max(1),
            out: path("OTZ_OUT").unwrap_or_else(|| PathBuf::from("smart_search_bench.csv")),
        })
    }

    fn sibling(&self, suffix: &str) -> PathBuf {
        let stem = self
            .out
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "bench".into());
        self.out.with_file_name(format!("{stem}_{suffix}"))
    }
}

/// The inputs of a semantic session, for modes that need vectors; `None` unless all are set.
#[allow(dead_code)]
struct SemanticInputs {
    vectors: PathBuf,
    model: PathBuf,
    model_identity: PathBuf,
    onnx_runtime: Option<PathBuf>,
}

impl SemanticInputs {
    fn from_env() -> Option<Self> {
        let path = |name: &str| std::env::var_os(name).map(PathBuf::from);
        Some(Self {
            vectors: path("OTZ_VECTORS")?,
            model: path("OTZ_MODEL")?,
            model_identity: path("OTZ_MODEL_IDENTITY")?,
            onnx_runtime: path("OTZ_ONNX_RUNTIME"),
        })
    }
}

/// An `MmapDirectory` that refuses every write and takes no lock file.
#[derive(Clone, Debug)]
struct ReadOnlyDirectory(MmapDirectory);

impl ReadOnlyDirectory {
    fn open(path: &Path) -> Result<Self, OpenDirectoryError> {
        MmapDirectory::open(path).map(Self)
    }
}

fn refused() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "read-only benchmark index")
}

impl Directory for ReadOnlyDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.0.get_file_handle(path)
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        self.0.open_read(path)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(refused()),
            filepath: path.to_path_buf(),
        })
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.0.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::wrap_io_error(refused(), path.to_path_buf()))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.0.atomic_read(path)
    }

    fn atomic_write(&self, _path: &Path, _data: &[u8]) -> io::Result<()> {
        Err(refused())
    }

    fn sync_directory(&self) -> io::Result<()> {
        Ok(())
    }

    // The reader's meta lock guards against the owner's garbage collection; a benchmark
    // leaves the owner's lock file alone and accepts that race.
    fn acquire_lock(&self, _lock: &Lock) -> Result<DirectoryLock, LockError> {
        Ok(DirectoryLock::from(Box::new(())))
    }

    fn watch(&self, _watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        Ok(WatchHandle::empty())
    }
}

/// The engine over `path`, opened without writing anything there.
fn open_read_only(path: &Path) -> Result<SearchEngine> {
    let directory = ReadOnlyDirectory::open(path)
        .with_context(|| format!("opening {} read-only", path.display()))?;
    let index = Index::open(directory).context("reading the index")?;
    SearchEngine::from_read_only_index(index, path)
}

struct Outcome {
    hits: u64,
    total: Option<u64>,
    truncated: bool,
    highlighted: Option<bool>,
}

fn run_once(
    engine: &SearchEngine,
    mode: Mode,
    query: &str,
    page: u32,
    limit: u32,
) -> Result<Outcome> {
    let offset = (page - 1) * limit;
    let window = offset + 2 * limit;
    match mode {
        #[cfg(feature = "semantic-integration")]
        Mode::LexExact | Mode::LexFuzzy(_) => {
            let distance = match mode {
                Mode::LexFuzzy(d) => Some(d),
                _ => None,
            };
            let phase = engine.bench_semantic_lexical_phase(query, &[], window, distance)?;
            Ok(Outcome {
                hits: phase.candidates as u64,
                total: Some(phase.total_count as u64),
                truncated: phase.truncated,
                highlighted: Some(phase.highlighted),
            })
        }
        #[cfg(not(feature = "semantic-integration"))]
        Mode::LexExact | Mode::LexFuzzy(_) => {
            let _ = window;
            bail!("{} needs the semantic-integration feature", mode.label())
        }
        Mode::ApiExact => {
            let results = engine.search_exact(
                query.to_string(),
                Vec::new(),
                limit,
                offset,
                ResultsOrder::Relevance,
                false,
                false,
                None,
            )?;
            Ok(page_outcome(results.len()))
        }
        Mode::ApiFuzzy(d) => {
            let results = engine.search_fuzzy(
                query.to_string(),
                Vec::new(),
                limit,
                offset,
                d,
                ResultsOrder::Relevance,
                false,
                false,
                None,
            )?;
            Ok(page_outcome(results.len()))
        }
        Mode::ApiFuzzyCount(d) => {
            let count =
                engine.count_fuzzy_with_status(query.to_string(), Vec::new(), d, false, false)?;
            Ok(Outcome {
                hits: count.count as u64,
                total: Some(count.count as u64),
                truncated: count.truncated,
                highlighted: None,
            })
        }
    }
}

fn page_outcome(len: usize) -> Outcome {
    Outcome {
        hits: len as u64,
        total: None,
        truncated: false,
        highlighted: None,
    }
}

/// Nearest-rank percentile of sorted samples.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn sorted(mut samples: Vec<f64>) -> Vec<f64> {
    samples.sort_by(|a, b| a.total_cmp(b));
    samples
}

struct Row {
    group: &'static str,
    query: &'static str,
    mode: Mode,
    page: u32,
    window: u32,
    first_ms: f64,
    samples: Vec<f64>,
    outcome: Option<Outcome>,
    error: Option<String>,
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn write_csv(path: &Path, rows: &[Row]) -> Result<()> {
    let mut out = String::from(
        "group,query,mode,page,window,runs,first_ms,p50_ms,p95_ms,min_ms,max_ms,hits,total,truncated,highlighted,error\n",
    );
    for row in rows {
        let s = sorted(row.samples.clone());
        let outcome = row.outcome.as_ref();
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{},{},{},{},{}",
            row.group,
            csv_field(row.query),
            row.mode.label(),
            row.page,
            row.window,
            s.len(),
            row.first_ms,
            percentile(&s, 50.0),
            percentile(&s, 95.0),
            s.first().copied().unwrap_or(f64::NAN),
            s.last().copied().unwrap_or(f64::NAN),
            outcome.map(|o| o.hits.to_string()).unwrap_or_default(),
            outcome
                .and_then(|o| o.total)
                .map(|t| t.to_string())
                .unwrap_or_default(),
            outcome.map(|o| o.truncated.to_string()).unwrap_or_default(),
            outcome
                .and_then(|o| o.highlighted)
                .map(|h| h.to_string())
                .unwrap_or_default(),
            csv_field(row.error.as_deref().unwrap_or("")),
        );
    }
    // BOM so spreadsheet tools read the Hebrew as UTF-8.
    fs::write(path, format!("\u{feff}{out}"))?;
    Ok(())
}

/// Mode label, group, page.
type PoolKey = (String, String, u32);
/// Timed samples, then first runs.
type Pooled = (Vec<f64>, Vec<f64>);

/// Samples pooled per group, mode and page: p50 / p95 / max of everything timed.
fn summary(rows: &[Row]) -> String {
    let mut pooled: BTreeMap<PoolKey, Pooled> = BTreeMap::new();
    for row in rows.iter().filter(|row| row.error.is_none()) {
        for group in [row.group, "ALL"] {
            let entry = pooled
                .entry((row.mode.label(), group.to_string(), row.page))
                .or_default();
            entry.0.extend(&row.samples);
            entry.1.push(row.first_ms);
        }
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<18} {:<8} {:>4} {:>6} {:>9} {:>9} {:>9} {:>9}",
        "mode", "group", "page", "n", "p50_ms", "p95_ms", "max_ms", "first_p50"
    );
    for ((mode, group, page), (samples, firsts)) in pooled {
        let s = sorted(samples);
        let f = sorted(firsts);
        let _ = writeln!(
            out,
            "{:<18} {:<8} {:>4} {:>6} {:>9.2} {:>9.2} {:>9.2} {:>9.2}",
            mode,
            group,
            page,
            s.len(),
            percentile(&s, 50.0),
            percentile(&s, 95.0),
            s.last().copied().unwrap_or(f64::NAN),
            percentile(&f, 50.0),
        );
    }
    let zero: Vec<String> = rows
        .iter()
        .filter(|row| row.page == 1 && row.outcome.as_ref().is_some_and(|o| o.hits == 0))
        .map(|row| format!("{} [{}]", row.query, row.mode.label()))
        .collect();
    let _ = writeln!(
        out,
        "\nzero hits on page 1: {}",
        if zero.is_empty() {
            "none".into()
        } else {
            zero.join(", ")
        }
    );
    let failed: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            Some(format!(
                "{} [{}]: {}",
                row.query,
                row.mode.label(),
                row.error.as_ref()?
            ))
        })
        .collect();
    if !failed.is_empty() {
        let _ = writeln!(out, "errors:\n  {}", failed.join("\n  "));
    }
    out
}

fn time<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let started = Instant::now();
    let value = f();
    (value, started.elapsed().as_secs_f64() * 1000.0)
}

fn open_engine(config: &Config) -> Result<SearchEngine> {
    if let Some(db) = &config.library_db {
        configure_line_source(db.to_string_lossy().into_owned())
            .with_context(|| format!("configuring {}", db.display()))?;
    }
    let (engine, open_ms) = time(|| open_read_only(&config.index));
    let mut engine = engine?;
    println!(
        "index opened read-only in {open_ms:.0} ms: {}",
        config.index.display()
    );
    if let Some(db) = &config.lexical_db {
        if !engine.set_magic_dictionary_path(db.to_string_lossy().into_owned()) {
            bail!("lexical.db at {} did not load", db.display());
        }
    }
    println!(
        "library db: {}; dictionary: {}",
        config
            .library_db
            .as_ref()
            .map_or("none".into(), |p| p.display().to_string()),
        if engine.has_magic_dictionary() {
            "loaded"
        } else {
            "none"
        },
    );
    Ok(engine)
}

#[test]
#[ignore = "needs OTZ_INDEX and a real library"]
fn lexical_baseline() -> Result<()> {
    let Some(config) = Config::from_env() else {
        println!("OTZ_INDEX is not set; nothing to measure");
        return Ok(());
    };
    match SemanticInputs::from_env() {
        Some(inputs) => println!(
            "semantic inputs found ({}); no mode uses them yet",
            inputs.vectors.display()
        ),
        None => println!("semantic inputs not set; vector modes skipped"),
    }
    let engine = open_engine(&config)?;
    let started = Instant::now();
    let mut rows = Vec::new();
    for &(group, queries) in QUERIES {
        for &query in queries {
            if let Ok(only) = std::env::var("OTZ_ONLY") {
                if !only.split('|').any(|q| q == query) {
                    continue;
                }
            }
            for &mode in MODES.iter().filter(|mode| mode.available()) {
                if let Ok(modes) = std::env::var("OTZ_MODES") {
                    if !modes.split(',').any(|m| m == mode.label()) {
                        continue;
                    }
                }
                let pages = if mode.paged() { config.pages } else { 1 };
                for page in 1..=pages {
                    let window =
                        if mode.paged() && matches!(mode, Mode::LexExact | Mode::LexFuzzy(_)) {
                            (page + 1) * config.limit
                        } else {
                            config.limit
                        };
                    let (first, first_ms) =
                        time(|| run_once(&engine, mode, query, page, config.limit));
                    let mut row = Row {
                        group,
                        query,
                        mode,
                        page,
                        window,
                        first_ms,
                        samples: Vec::with_capacity(config.runs),
                        outcome: None,
                        error: None,
                    };
                    match first {
                        Err(err) => row.error = Some(format!("{err:#}")),
                        Ok(outcome) => {
                            row.outcome = Some(outcome);
                            for _ in 0..config.runs {
                                let (result, ms) =
                                    time(|| run_once(&engine, mode, query, page, config.limit));
                                if let Err(err) = result {
                                    row.error = Some(format!("{err:#}"));
                                    break;
                                }
                                row.samples.push(ms);
                            }
                        }
                    }
                    let s = sorted(row.samples.clone());
                    println!(
                        "{group:<8} {:<16} p{page} w{window:<4} first {:>8.1}  p50 {:>8.1}  p95 {:>8.1}  hits {:>6} total {:>8}  {query}",
                        mode.label(),
                        row.first_ms,
                        percentile(&s, 50.0),
                        percentile(&s, 95.0),
                        row.outcome.as_ref().map_or(0, |o| o.hits),
                        row.outcome.as_ref().and_then(|o| o.total).map_or("-".into(), |t| t.to_string()),
                    );
                    if let Some(err) = &row.error {
                        println!("  error: {err}");
                    }
                    rows.push(row);
                }
            }
        }
    }
    if let Some(parent) = config.out.parent() {
        fs::create_dir_all(parent)?;
    }
    write_csv(&config.out, &rows)?;
    let table = summary(&rows);
    let header = format!(
        "runs={} pages={} limit={} index={} elapsed={:.0}s\n\n",
        config.runs,
        config.pages,
        config.limit,
        config.index.display(),
        started.elapsed().as_secs_f64(),
    );
    fs::write(config.sibling("summary.txt"), format!("{header}{table}"))?;
    println!("\n{header}{table}");
    println!("csv: {}", config.out.display());
    Ok(())
}

#[test]
#[ignore = "needs OTZ_LEXICAL_DB"]
fn dictionary_forms() -> Result<()> {
    let Some(config) = Config::from_env() else {
        println!("OTZ_INDEX is not set; nothing to measure");
        return Ok(());
    };
    let Some(db) = &config.lexical_db else {
        println!("OTZ_LEXICAL_DB is not set; no forms to dump");
        return Ok(());
    };
    let dictionary = MagicDictionary::open(db)?;
    let mut out = format!("lexical.db: {}\ncap: {MAX_LEXICAL_FORMS}\n", db.display());
    for &word in FORM_WORDS {
        let (recall, cold_ms) = time(|| dictionary.recall_forms(word, MAX_LEXICAL_FORMS));
        let (_, warm_ms) = time(|| dictionary.recall_forms(word, MAX_LEXICAL_FORMS));
        let highlight = dictionary.highlight_forms(word, MAX_LEXICAL_FORMS);
        let withheld: Vec<&String> = recall
            .iter()
            .filter(|form| !highlight.contains(form))
            .collect();
        let _ = writeln!(
            out,
            "\n== {word}  (lookup cold {cold_ms:.2} ms, cached {warm_ms:.3} ms)\nrecall_forms ({}): {}\nhighlight_forms ({}): {}\nwithheld from highlight ({}): {}",
            recall.len(),
            recall.join(" "),
            highlight.len(),
            highlight.join(" "),
            withheld.len(),
            withheld.iter().map(|form| form.as_str()).collect::<Vec<_>>().join(" "),
        );
    }
    if let Some(parent) = config.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let path = config.sibling("forms.txt");
    fs::write(&path, &out)?;
    println!("{out}\nforms: {}", path.display());
    Ok(())
}
