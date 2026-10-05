//! Real-data benchmark of smart search on an index another process owns, opened through
//! [`ReadOnlyDirectory`]; every test is `#[ignore]` and env-driven (see [`Config`]).
//!
//! Run: `cargo test --release --features semantic --lib smart_search_bench -- --ignored
//! --nocapture --test-threads=1`.
//!
//! Adding a mode: a [`Mode`] variant, its `label`/`paged`/`available`, an arm in [`run_once`]
//! and an entry in [`MODES`]; rows, summary and CSV follow. [`semantic_pages`] times whole
//! `search_semantic` pages on a real vector set (`OTZ_VECTORS`, `OTZ_MODEL`, ...), and
//! [`passage_highlights`] the passage highlights of their semantic hits.

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
    "רמב\"ן",
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
/// `OTZ_VECTORS` is the set's directory, the one holding `CURRENT`.
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
    // `OTZ_EXTRA` (`|`-separated): more queries, as the group "extra".
    let extra: &'static [&'static str] = Box::leak(
        std::env::var("OTZ_EXTRA")
            .map(|v| {
                v.split('|')
                    .map(|q| &*Box::leak(q.to_string().into_boxed_str()))
                    .collect::<Vec<&'static str>>()
            })
            .unwrap_or_default()
            .into_boxed_slice(),
    );
    let groups: Vec<(&'static str, &'static [&'static str])> = QUERIES
        .iter()
        .copied()
        .chain((!extra.is_empty()).then_some(("extra", extra)))
        .collect();
    for &(group, queries) in &groups {
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

/// Open the vector set `inputs` names on `engine`; how long it took, in ms.
#[cfg(feature = "semantic-integration")]
fn open_vectors(engine: &SearchEngine, inputs: &SemanticInputs) -> Result<f64> {
    use crate::api::search_engine::SemanticArtifactInput;
    let (opened, open_ms) = time(|| {
        engine
            .open_semantic_artifact(SemanticArtifactInput {
                vectors_dir: inputs.vectors.to_string_lossy().into_owned(),
                model_path: inputs.model.to_string_lossy().into_owned(),
                model_identity_json: fs::read_to_string(&inputs.model_identity)?,
                onnx_runtime_path: inputs
                    .onnx_runtime
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                scan_threads: None,
            })
            .map_err(|error| anyhow::anyhow!("{error:?}"))
    });
    let status = opened?;
    println!(
        "vector set opened in {open_ms:.0} ms: {} vectors, state {:?}",
        status.vector_count, status.state
    );
    Ok(open_ms)
}

/// Each step of [`semantic_pages`]: a fresh session's first page, the same page again, the
/// second page (inside the first window), and the pages past it, which widen the window.
#[cfg(feature = "semantic-integration")]
const PAGE_STEPS: &[(&str, u32)] = &[
    ("first", 1),
    ("again", 1),
    ("page2", 2),
    ("page3", 3),
    ("page4", 4),
    ("page5", 5),
];

/// Whole `search_semantic` pages through the session cache, Hybrid with the default ranking,
/// Exact and Fuzzy 0, with where each page's time went. `OTZ_RUNS` fresh sessions per query;
/// `OTZ_GROUPS` (comma-separated) narrows the queries, `OTZ_SHARE` sets the foundational share.
/// Before the runs, the cold path: opening the set, planning, the first search;
/// `OTZ_COLD_ONLY` stops after it. `OTZ_DUMP` names a file for every result of the first run.
/// `OTZ_FORGET_KEYS` starts every session without the keys earlier ones recomputed.
#[cfg(feature = "semantic-integration")]
#[test]
#[ignore = "needs OTZ_INDEX and a real vector set"]
fn semantic_pages() -> Result<()> {
    use crate::api::search_engine::{
        SemanticCancellationToken, SemanticLexicalMode, SemanticRankingOptions,
        SemanticRetrievalMode, SemanticTimings, LAST_SEMANTIC_TIMINGS,
    };
    let groups: Option<Vec<String>> = std::env::var("OTZ_GROUPS").ok().map(|groups| {
        groups
            .split(',')
            .map(|group| group.trim().to_string())
            .collect()
    });
    let ranking = std::env::var("OTZ_SHARE")
        .ok()
        .and_then(|share| share.trim().parse::<f64>().ok())
        .map(|share| SemanticRankingOptions {
            foundational_candidate_share: share,
            ..SemanticRankingOptions::defaults()
        });
    let Some(config) = Config::from_env() else {
        println!("OTZ_INDEX is not set; nothing to measure");
        return Ok(());
    };
    let Some(inputs) = SemanticInputs::from_env() else {
        println!("OTZ_VECTORS, OTZ_MODEL and OTZ_MODEL_IDENTITY are not set; nothing to measure");
        return Ok(());
    };
    let engine = open_engine(&config)?;
    let open_ms = open_vectors(&engine, &inputs)?;
    let search = |query: &str, page: u32, lexical_mode: SemanticLexicalMode| {
        let response = engine.search_semantic(
            query.to_string(),
            Vec::new(),
            config.limit,
            (page - 1) * config.limit,
            lexical_mode,
            0,
            SemanticRetrievalMode::Hybrid,
            None,
            false,
            false,
            ranking.clone(),
            &SemanticCancellationToken::new(),
        );
        (response, LAST_SEMANTIC_TIMINGS.get())
    };

    let plan_ms = engine.plan_semantic_for_bench()?;
    let cold_query = QUERIES[0].1[0];
    let (cold, timings) = search(cold_query, 1, SemanticLexicalMode::Exact);
    let cold = cold.map_err(|error| anyhow::anyhow!("{error:?}"))?;
    engine.invalidate_semantic_sessions_for_bench();
    let (_, warm) = search(cold_query, 1, SemanticLexicalMode::Exact);
    let cold_line = format!(
        "cold: open {open_ms:.0} ms, plan {plan_ms:.0} ms, first search {:.0} ms (lex {:.0} \
         emb {:.0} scan {:.0} res {:.0}; same search warm {:.0} ms, scan {:.0} res {:.0}), \
         n={} {:?}  {cold_query}",
        timings.total_ms,
        timings.lexical_ms,
        timings.embed_ms,
        timings.scan_ms,
        timings.resolve_ms,
        warm.total_ms,
        warm.scan_ms,
        warm.resolve_ms,
        cold.results.len(),
        cold.executed_mode
    );
    println!("{cold_line}");
    if std::env::var_os("OTZ_COLD_ONLY").is_some() {
        return Ok(());
    }
    let mut dump = std::env::var_os("OTZ_DUMP").map(|_| String::new());
    let forget_keys = std::env::var_os("OTZ_FORGET_KEYS").is_some();

    let mut out = String::from(
        "group,query,lexical,step,run,total_ms,expansions,lexical_ms,semantic_ms,embed_ms,scan_ms,resolve_ms,fuse_ms,hydrate_ms,page_ms,results,has_more,executed,total_count,lexical_total\n",
    );
    // Step label, lexical mode: every timing, for the summary.
    let mut pooled: BTreeMap<(String, String), Vec<SemanticTimings>> = BTreeMap::new();
    let wanted = |group: &str| {
        groups
            .as_ref()
            .is_none_or(|groups| groups.iter().any(|g| g == group))
    };
    for &(group, queries) in QUERIES.iter().filter(|(group, _)| wanted(group)) {
        for &query in queries {
            for (lexical_label, lexical_mode) in [
                ("exact", SemanticLexicalMode::Exact),
                ("fuzzy0", SemanticLexicalMode::Fuzzy),
            ] {
                for run in 0..config.runs {
                    engine.invalidate_semantic_sessions_for_bench();
                    if forget_keys {
                        engine.forget_semantic_keys_for_bench();
                    }
                    for &(step, page) in PAGE_STEPS {
                        let (response, timings) = search(query, page, lexical_mode);
                        let response = match response {
                            Ok(response) => response,
                            Err(error) => {
                                println!("{query} [{lexical_label} {step}]: {error:?}");
                                break;
                            }
                        };
                        let _ = writeln!(
                            out,
                            "{group},{},{lexical_label},{step},{run},{:.2},{},{:.2},{:.2},{:.0},{:.0},{:.0},{:.2},{:.2},{:.2},{},{},{:?},{},{}",
                            csv_field(query),
                            timings.total_ms,
                            timings.expansions,
                            timings.lexical_ms,
                            timings.semantic_ms,
                            timings.embed_ms,
                            timings.scan_ms,
                            timings.resolve_ms,
                            timings.fuse_ms,
                            timings.hydrate_ms,
                            timings.page_ms,
                            response.results.len(),
                            response.has_more,
                            response.executed_mode,
                            response.total_count,
                            response.lexical_total_count,
                        );
                        if let (Some(dump), 0) = (dump.as_mut(), run) {
                            for (rank, result) in response.results.iter().enumerate() {
                                let _ = writeln!(
                                    dump,
                                    "{query}\t{lexical_label}\t{step}\t{rank}\t{}\t{}\t{}\t{:?}\t{:?}\t{:?}\t{}\t{}",
                                    result.file_path,
                                    result.id,
                                    result.segment,
                                    result.source,
                                    result.semantic_score,
                                    result.lexical_score,
                                    result.fused_score,
                                    result.merged_count,
                                );
                            }
                        }
                        if run == 0 {
                            println!(
                                "{group:<8} {lexical_label:<6} {step:<6} {:>8.1} ms  x{} lex {:>7.1} sem {:>7.1} (emb {:>4.0} scan {:>5.0} res {:>5.0}) fuse {:>6.1} hyd {:>6.1} page {:>6.1}  n={} more={} {:?}  {query}",
                                timings.total_ms,
                                timings.expansions,
                                timings.lexical_ms,
                                timings.semantic_ms,
                                timings.embed_ms,
                                timings.scan_ms,
                                timings.resolve_ms,
                                timings.fuse_ms,
                                timings.hydrate_ms,
                                timings.page_ms,
                                response.results.len(),
                                response.has_more,
                                response.executed_mode,
                            );
                        }
                        pooled
                            .entry((step.to_string(), lexical_label.to_string()))
                            .or_default()
                            .push(timings);
                        if !response.has_more {
                            break;
                        }
                    }
                }
            }
        }
    }

    if let (Some(dump), Some(path)) = (dump, std::env::var_os("OTZ_DUMP")) {
        fs::write(&path, dump)?;
    }
    let mut table = format!(
        "{cold_line}\n{:<6} {:<7} {:>5} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}\n",
        "step",
        "lexical",
        "n",
        "p50_ms",
        "p95_ms",
        "lex_p50",
        "sem_p50",
        "scan_p50",
        "res_p50",
        "fuse_p50",
        "hyd_p50",
        "page_p50"
    );
    for ((step, lexical), samples) in &pooled {
        let p50 = |field: fn(&SemanticTimings) -> f64| {
            percentile(&sorted(samples.iter().map(field).collect()), 50.0)
        };
        let total = sorted(samples.iter().map(|t| t.total_ms).collect());
        let _ = writeln!(
            table,
            "{step:<6} {lexical:<7} {:>5} {:>9.2} {:>9.2} {:>8.2} {:>8.2} {:>8.0} {:>8.0} {:>8.2} {:>8.2} {:>8.2}",
            samples.len(),
            percentile(&total, 50.0),
            percentile(&total, 95.0),
            p50(|t| t.lexical_ms),
            p50(|t| t.semantic_ms),
            p50(|t| t.scan_ms),
            p50(|t| t.resolve_ms),
            p50(|t| t.fuse_ms),
            p50(|t| t.hydrate_ms),
            p50(|t| t.page_ms),
        );
    }
    if let Some(parent) = config.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let csv = config.sibling("semantic_pages.csv");
    fs::write(&csv, format!("\u{feff}{out}"))?;
    let summary = config.sibling("semantic_pages_summary.txt");
    fs::write(&summary, &table)?;
    println!(
        "\n{table}\ncsv: {}\nsummary: {}",
        csv.display(),
        summary.display()
    );
    Ok(())
}

/// Highlights asked per call, as the application batches them.
#[cfg(feature = "semantic-integration")]
const HIGHLIGHT_BATCH: usize = 6;

/// Highlight batches of each conceptual query's semantic-only hits, and one inference alone and
/// beside running highlights: what a new search's query waits for. Snippets go to `OTZ_HTML`.
#[cfg(feature = "semantic-integration")]
#[test]
#[ignore = "needs OTZ_INDEX and a real vector set"]
fn passage_highlights() -> Result<()> {
    use crate::api::search_engine::{
        SemanticCancellationToken, SemanticHighlightTarget, SemanticLexicalMode,
        SemanticResultSource, SemanticRetrievalMode,
    };
    use crate::semantic_highlight::EMBEDDED_CLAUSES;
    use std::sync::atomic::{AtomicBool, Ordering};

    let Some(config) = Config::from_env() else {
        println!("OTZ_INDEX is not set; nothing to measure");
        return Ok(());
    };
    let Some(inputs) = SemanticInputs::from_env() else {
        println!("OTZ_VECTORS, OTZ_MODEL and OTZ_MODEL_IDENTITY are not set; nothing to measure");
        return Ok(());
    };
    let engine = open_engine(&config)?;
    open_vectors(&engine, &inputs)?;
    let queries = QUERIES
        .iter()
        .find(|(group, _)| *group == "concept")
        .map(|(_, queries)| &queries[..10])
        .expect("the concept group");

    let mut cold = Vec::new();
    let mut warm = Vec::new();
    let mut per_clause = Vec::new();
    let mut marked = 0usize;
    let mut asked = 0usize;
    // Why a target was not highlighted, by reason.
    let mut unmarked: BTreeMap<&str, usize> = BTreeMap::new();
    let mut last_targets = Vec::new();
    let mut html = String::from(
        "<!doctype html><html lang=\"he\" dir=\"rtl\"><head><meta charset=\"utf-8\">\
         <title>Passage highlights</title><style>body{font-family:'David','Times New Roman',serif;\
         max-width:60rem;margin:1rem auto;padding:0 1rem;line-height:1.6}h2{margin-top:2rem}\
         .item{border-top:1px solid #ccc;padding:.5rem 0}.meta{color:#666;font-size:.85em}\
         .before{color:#555}mark{background:rgba(255,200,0,.25)}</style></head><body>\n",
    );
    for &query in queries {
        engine.invalidate_semantic_sessions_for_bench();
        let response = engine
            .search_semantic(
                query.to_string(),
                Vec::new(),
                config.limit,
                0,
                SemanticLexicalMode::Fuzzy,
                0,
                SemanticRetrievalMode::Hybrid,
                None,
                false,
                false,
                None,
                &SemanticCancellationToken::new(),
            )
            .map_err(|error| anyhow::anyhow!("{query}: {error:?}"))?;
        let semantic: Vec<_> = response
            .results
            .iter()
            .filter(|hit| matches!(hit.source, SemanticResultSource::Semantic))
            .collect();
        println!(
            "{query}: {} results, {} semantic-only",
            response.results.len(),
            semantic.len()
        );
        let _ = writeln!(
            html,
            "<h2>{}</h2><p class=\"meta\">{} results, {} semantic-only</p>",
            htmlescape::encode_minimal(query),
            response.results.len(),
            semantic.len()
        );
        for batch in semantic.chunks(HIGHLIGHT_BATCH) {
            let targets: Vec<SemanticHighlightTarget> = batch
                .iter()
                .map(|hit| SemanticHighlightTarget {
                    file_path: hit.file_path.clone(),
                    id: hit.id,
                })
                .collect();
            let before = EMBEDDED_CLAUSES.get();
            let (got, ms) = time(|| {
                engine.semantic_passage_highlights(
                    query.to_string(),
                    targets.clone(),
                    &SemanticCancellationToken::new(),
                )
            });
            let got = got.map_err(|error| anyhow::anyhow!("{query}: {error:?}"))?;
            let clauses = EMBEDDED_CLAUSES.get() - before;
            let (_, again_ms) = time(|| {
                engine.semantic_passage_highlights(
                    query.to_string(),
                    targets.clone(),
                    &SemanticCancellationToken::new(),
                )
            });
            println!(
                "  batch of {}: {ms:>7.1} ms, {clauses:>2} clauses, cached {again_ms:.2} ms",
                targets.len()
            );
            cold.push(ms);
            warm.push(again_ms);
            if clauses > 0 {
                per_clause.push(ms / clauses as f64);
            }
            asked += got.len();
            for (hit, highlight) in batch.iter().zip(&got) {
                marked += usize::from(highlight.is_highlighted);
                if !highlight.is_highlighted {
                    let reason = unmarked_reason(&engine, &hit.file_path, hit.id)?;
                    *unmarked.entry(reason).or_default() += 1;
                }
                let _ = writeln!(
                    html,
                    "<div class=\"item\"><div class=\"meta\">{} — {} · semantic {:.3} · span {}</div>\
                     <div class=\"before\">{}</div><div>{}</div></div>",
                    htmlescape::encode_minimal(&hit.title),
                    htmlescape::encode_minimal(&hit.reference),
                    hit.semantic_score.unwrap_or(f32::NAN),
                    highlight
                        .span_score
                        .map_or("—".to_string(), |score| format!("{score:.3}")),
                    hit.snippet_html,
                    if highlight.is_highlighted {
                        highlight.snippet_html.clone()
                    } else {
                        "<i>(not highlighted)</i>".to_string()
                    },
                );
            }
            last_targets = targets;
        }
    }
    html.push_str("</body></html>\n");

    // A query's embedding alone, then while highlights run without a pause on another thread.
    let probe = "מה היא מעלת השלום בין אדם לחברו";
    let alone: Vec<f64> = (0..30)
        .filter_map(|_| engine.bench_embed_once(probe))
        .collect();
    let stop = AtomicBool::new(false);
    let loaded: Vec<f64> = std::thread::scope(|scope| {
        let background = scope.spawn(|| {
            let mut batches = 0usize;
            while !stop.load(Ordering::Relaxed) {
                // A new epoch, so every batch embeds afresh.
                engine.invalidate_semantic_sessions_for_bench();
                let _ = engine.semantic_passage_highlights(
                    queries[0].to_string(),
                    last_targets.clone(),
                    &SemanticCancellationToken::new(),
                );
                batches += 1;
            }
            batches
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        let samples = (0..30)
            .filter_map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(7));
                engine.bench_embed_once(probe)
            })
            .collect();
        stop.store(true, Ordering::Relaxed);
        let batches = background.join().expect("background highlights");
        println!("background ran {batches} highlight batches");
        samples
    });

    let line = |name: &str, samples: &[f64]| {
        let samples = sorted(samples.to_vec());
        format!(
            "{name:<22} n={:>3} p50 {:>8.2} ms  p95 {:>8.2} ms  max {:>8.2} ms\n",
            samples.len(),
            percentile(&samples, 50.0),
            percentile(&samples, 95.0),
            samples.last().copied().unwrap_or(f64::NAN),
        )
    };
    let mut table = String::new();
    table.push_str(&line("batch (cold)", &cold));
    table.push_str(&line("batch (cached)", &warm));
    table.push_str(&line("per clause", &per_clause));
    table.push_str(&line("embed alone", &alone));
    table.push_str(&line("embed under highlights", &loaded));
    let _ = writeln!(table, "highlighted {marked} of {asked} semantic-only hits");
    let _ = writeln!(table, "not highlighted, by reason: {unmarked:?}");

    if let Some(parent) = config.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let html_path = std::env::var_os("OTZ_HTML")
        .map(PathBuf::from)
        .unwrap_or_else(|| config.sibling("passage_highlights.html"));
    if let Some(parent) = html_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&html_path, html)?;
    let summary = config.sibling("passage_highlights_summary.txt");
    fs::write(&summary, &table)?;
    println!(
        "\n{table}\nhtml: {}\nsummary: {}",
        html_path.display(),
        summary.display()
    );
    Ok(())
}

/// Why the highlight of line `id` of `file_path` marked nothing: its line is missing, stale or
/// unreadable, short, or one clause; "other" for a line whose clauses were embedded.
#[cfg(feature = "semantic-integration")]
fn unmarked_reason(engine: &SearchEngine, file_path: &str, id: u64) -> Result<&'static str> {
    use crate::api::search_engine::TextStatus;
    use crate::semantic_highlight::{clauses, SHORT_LINE_WORDS};
    Ok(match engine.passage_text_for_bench(file_path, id)? {
        None => "missing",
        Some((_, TextStatus::Stale)) => "stale",
        Some((_, TextStatus::Unavailable)) => "unavailable",
        Some((text, TextStatus::Ok)) => {
            let words = text
                .split_whitespace()
                .filter(|word| word.chars().any(char::is_alphanumeric))
                .count();
            if words <= SHORT_LINE_WORDS {
                "short"
            } else if clauses(&text).is_empty() {
                "one clause"
            } else {
                "other"
            }
        }
    })
}

/// One `search_semantic` page, Hybrid, in the session the engine keeps for the query.
#[cfg(feature = "semantic-integration")]
fn semantic_page(
    engine: &SearchEngine,
    query: &str,
    limit: u32,
    page: u32,
    lexical_mode: crate::api::search_engine::SemanticLexicalMode,
    ranking: Option<crate::api::search_engine::SemanticRankingOptions>,
) -> Result<crate::api::search_engine::SemanticSearchResponse> {
    semantic_page_in(engine, query, &[], limit, page, lexical_mode, ranking)
}

/// [`semantic_page`] restricted to `facets`.
#[cfg(feature = "semantic-integration")]
fn semantic_page_in(
    engine: &SearchEngine,
    query: &str,
    facets: &[String],
    limit: u32,
    page: u32,
    lexical_mode: crate::api::search_engine::SemanticLexicalMode,
    ranking: Option<crate::api::search_engine::SemanticRankingOptions>,
) -> Result<crate::api::search_engine::SemanticSearchResponse> {
    use crate::api::search_engine::{SemanticCancellationToken, SemanticRetrievalMode};
    engine
        .search_semantic(
            query.to_string(),
            facets.to_vec(),
            limit,
            (page - 1) * limit,
            lexical_mode,
            0,
            SemanticRetrievalMode::Hybrid,
            None,
            false,
            false,
            ranking,
            &SemanticCancellationToken::new(),
        )
        .map_err(|error| anyhow::anyhow!("{query} p{page}: {error:?}"))
}

#[cfg(feature = "semantic-integration")]
const SEMANTIC_LEXICAL_MODES: &[(&str, crate::api::search_engine::SemanticLexicalMode)] = &[
    (
        "exact",
        crate::api::search_engine::SemanticLexicalMode::Exact,
    ),
    (
        "fuzzy0",
        crate::api::search_engine::SemanticLexicalMode::Fuzzy,
    ),
];

/// A result as paging compares it: book, line, fused score.
#[cfg(feature = "semantic-integration")]
type Shown = (String, u64, f32);

#[cfg(feature = "semantic-integration")]
fn shown(response: &crate::api::search_engine::SemanticSearchResponse) -> Vec<Shown> {
    response
        .results
        .iter()
        .map(|r| (r.file_path.clone(), r.id, r.fused_score))
        .collect()
}

#[cfg(feature = "semantic-integration")]
fn shown_ids(v: &[Shown]) -> Vec<(String, u64)> {
    v.iter().map(|(f, i, _)| (f.clone(), *i)).collect()
}

/// Paging stability in one session: `OTZ_WALK` (5) pages of each query, Exact and Fuzzy 0. After
/// each page every earlier page is asked again and must be unchanged; no line appears twice;
/// page 1 asked last is page 1 as first shown; `has_more` is true on every page but the last.
/// `OTZ_END_QUERIES` (`|`-separated) are walked up to `OTZ_END_MAX` (60) pages to find the end.
#[cfg(feature = "semantic-integration")]
#[test]
#[ignore = "needs OTZ_INDEX and a real vector set"]
fn paging_stability() -> Result<()> {
    use std::collections::HashSet;
    let Some(config) = Config::from_env() else {
        println!("OTZ_INDEX is not set; nothing to measure");
        return Ok(());
    };
    let Some(inputs) = SemanticInputs::from_env() else {
        println!("semantic inputs are not set; nothing to measure");
        return Ok(());
    };
    let walk = std::env::var("OTZ_WALK")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(5)
        .max(2);
    let engine = open_engine(&config)?;
    open_vectors(&engine, &inputs)?;
    let mut queries: Vec<&str> = QUERIES
        .iter()
        .find(|(group, _)| *group == "concept")
        .map(|(_, q)| q.to_vec())
        .unwrap_or_default();
    queries.extend(["שבת", "תפילין", "תשובה", "רמב\"ם", "בראשית א א"]);
    if std::env::var_os("OTZ_SKIP_WALK").is_some() {
        queries.clear();
    }
    let mut out = String::new();
    let mut failures = 0usize;
    for &query in &queries {
        for &(label, mode) in SEMANTIC_LEXICAL_MODES {
            engine.invalidate_semantic_sessions_for_bench();
            let mut pages: Vec<Vec<Shown>> = Vec::new();
            let mut more: Vec<bool> = Vec::new();
            let mut problems: Vec<String> = Vec::new();
            let mut score_drift = 0usize;
            for page in 1..=walk {
                let response = semantic_page(&engine, query, config.limit, page, mode, None)?;
                pages.push(shown(&response));
                more.push(response.has_more);
                for earlier in 1..page {
                    let again = shown(&semantic_page(
                        &engine,
                        query,
                        config.limit,
                        earlier,
                        mode,
                        None,
                    )?);
                    let before = &pages[earlier as usize - 1];
                    let (a, b) = (shown_ids(&again), shown_ids(before));
                    if a != b {
                        let first_diff = a
                            .iter()
                            .zip(&b)
                            .position(|(x, y)| x != y)
                            .unwrap_or(a.len().min(b.len()));
                        problems.push(format!(
                            "page {earlier} changed after page {page} (len {}->{}, first diff at rank {first_diff})",
                            b.len(),
                            a.len()
                        ));
                    } else if again
                        .iter()
                        .zip(before)
                        .any(|(x, y)| (x.2 - y.2).abs() > 1e-6)
                    {
                        score_drift += 1;
                    }
                }
                if !response.has_more {
                    break;
                }
            }
            let again1 = shown(&semantic_page(&engine, query, config.limit, 1, mode, None)?);
            if shown_ids(&pages[0]) != shown_ids(&again1) {
                problems.push("page 1 asked again differs".into());
            }
            let mut seen = HashSet::new();
            let dups: Vec<String> = pages
                .iter()
                .flatten()
                .filter(|(f, i, _)| !seen.insert((f.clone(), *i)))
                .map(|(f, i, _)| format!("{f}#{i}"))
                .collect();
            if !dups.is_empty() {
                problems.push(format!("{} duplicates: {}", dups.len(), dups.join(" ")));
            }
            // Fused order: inversions inside a page, and rises across a page boundary.
            let mut inside = 0usize;
            let mut across = Vec::new();
            let mut inversions = Vec::new();
            for (p, page) in pages.iter().enumerate() {
                for (r, w) in page.windows(2).enumerate() {
                    if w[1].2 > w[0].2 + 1e-7 {
                        inside += 1;
                        inversions.push(format!(
                            "p{} #{}->#{} {:.6}<{:.6} ({}#{} then {}#{})",
                            p + 1,
                            r + 1,
                            r + 2,
                            w[0].2,
                            w[1].2,
                            w[0].0,
                            w[0].1,
                            w[1].0,
                            w[1].1
                        ));
                    }
                }
                if let (Some(last), Some(next)) =
                    (page.last(), pages.get(p + 1).and_then(|n| n.first()))
                {
                    if next.2 > last.2 + 1e-7 {
                        across.push(format!(
                            "p{}->p{} {:.6}<{:.6}",
                            p + 1,
                            p + 2,
                            last.2,
                            next.2
                        ));
                    }
                }
            }
            if inside > 0 {
                problems.push(format!(
                    "{inside} fused inversions inside pages: {}",
                    inversions.join(", ")
                ));
            }
            let fetched = pages.len();
            let more_ok = more[..fetched - 1].iter().all(|m| *m)
                && pages[..fetched - 1]
                    .iter()
                    .all(|p| p.len() == config.limit as usize);
            if !more_ok {
                problems.push(format!(
                    "has_more/fullness wrong: {more:?} lens {:?}",
                    pages.iter().map(Vec::len).collect::<Vec<_>>()
                ));
            }
            let verdict = if problems.is_empty() { "OK" } else { "FAIL" };
            failures += usize::from(!problems.is_empty());
            let line = format!(
                "{verdict:<4} {label:<6} pages {fetched} lens {:?} more {:?} score_drift {score_drift} cross_page_rises {} [{}] {}  {query}",
                pages.iter().map(Vec::len).collect::<Vec<_>>(),
                more,
                across.len(),
                across.join("; "),
                problems.join("; "),
            );
            println!("{line}");
            let _ = writeln!(out, "{line}");
        }
    }

    let end_queries: Vec<String> = std::env::var("OTZ_END_QUERIES")
        .map(|v| v.split('|').map(str::to_string).collect())
        .unwrap_or_else(|_| vec!["ענווה של משה רבינו".into(), "קשקש בלבל זמזם".into()]);
    let end_facets: Vec<String> = std::env::var("OTZ_END_FACETS")
        .map(|v| v.split('|').map(str::to_string).collect())
        .unwrap_or_default();
    let end_max = std::env::var("OTZ_END_MAX")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(60);
    for query in &end_queries {
        for &(label, mode) in SEMANTIC_LEXICAL_MODES {
            engine.invalidate_semantic_sessions_for_bench();
            let mut lens = Vec::new();
            let mut mores = Vec::new();
            let mut seen = HashSet::new();
            let mut dups = 0usize;
            let mut last = None;
            for page in 1..=end_max {
                let response =
                    semantic_page_in(&engine, query, &end_facets, config.limit, page, mode, None)?;
                for r in &response.results {
                    dups += usize::from(!seen.insert((r.file_path.clone(), r.id)));
                }
                lens.push(response.results.len());
                mores.push(response.has_more);
                if !response.has_more {
                    let after = semantic_page_in(
                        &engine,
                        query,
                        &end_facets,
                        config.limit,
                        page + 1,
                        mode,
                        None,
                    )?;
                    last = Some((
                        page,
                        response.total_count,
                        response.candidate_window_truncated,
                        after.results.len(),
                        after.has_more,
                    ));
                    break;
                }
            }
            let early_false = mores[..mores.len().saturating_sub(1)]
                .iter()
                .filter(|m| !**m)
                .count();
            let line = match last {
                Some((page, total, capped, after_len, after_more)) => format!(
                    "END  {label:<6} ended at page {page} (total_count {total}, window_capped {capped}); next page: {after_len} results, more={after_more}; early more=false {early_false}; dups {dups}; lens {lens:?}  {query} {end_facets:?}"
                ),
                None => format!(
                    "OPEN {label:<6} still has_more after {end_max} pages; dups {dups}; last lens {:?}  {query} {end_facets:?}",
                    &lens[lens.len().saturating_sub(3)..]
                ),
            };
            println!("{line}");
            let _ = writeln!(out, "{line}");
        }
    }
    let _ = writeln!(out, "\nfailures: {failures}");
    if let Some(parent) = config.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let path = config.sibling("paging.txt");
    fs::write(&path, &out)?;
    println!("\nfailures: {failures}\nreport: {}", path.display());
    Ok(())
}

/// Whether the line `id` of `file_path` sits in a foundational book (`/base` or under it).
#[cfg(feature = "semantic-integration")]
struct FoundationalProbe {
    searcher: tantivy::Searcher,
    file_path: tantivy::schema::Field,
    id: tantivy::schema::Field,
    topics: tantivy::schema::Field,
}

#[cfg(feature = "semantic-integration")]
impl FoundationalProbe {
    fn open(path: &Path) -> Result<Self> {
        let index = Index::open(ReadOnlyDirectory::open(path)?)?;
        let schema = index.schema();
        let reader: tantivy::IndexReader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            searcher: reader.searcher(),
            file_path: schema.get_field("filePath")?,
            id: schema.get_field("id")?,
            topics: schema.get_field("topics")?,
        })
    }

    fn is_base(&self, file_path: &str, id: u64) -> Result<bool> {
        use tantivy::collector::Count;
        use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
        use tantivy::schema::{Facet, IndexRecordOption};
        use tantivy::Term;
        let term =
            |t: Term| -> Box<dyn Query> { Box::new(TermQuery::new(t, IndexRecordOption::Basic)) };
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                term(Term::from_field_text(self.file_path, file_path)),
            ),
            (Occur::Must, term(Term::from_field_u64(self.id, id))),
            (
                Occur::Must,
                term(Term::from_facet(self.topics, &Facet::from_text("/base")?)),
            ),
        ]);
        Ok(self.searcher.search(&query, &Count)? > 0)
    }
}

/// A ranked result as [`foundational_share`] reports it.
#[cfg(feature = "semantic-integration")]
struct Ranked {
    title: String,
    reference: String,
    key: (String, u64),
    base: bool,
    fused: f32,
    source: String,
}

/// The foundational books' share of each conceptual query's first page: candidate share 0
/// against 0.5 (both at bonus 0.002), and no preference at all; what moved, with its scores.
#[cfg(feature = "semantic-integration")]
#[test]
#[ignore = "needs OTZ_INDEX and a real vector set"]
fn foundational_share() -> Result<()> {
    use crate::api::search_engine::SemanticRankingOptions;
    let Some(config) = Config::from_env() else {
        println!("OTZ_INDEX is not set; nothing to measure");
        return Ok(());
    };
    let Some(inputs) = SemanticInputs::from_env() else {
        println!("semantic inputs are not set; nothing to measure");
        return Ok(());
    };
    let engine = open_engine(&config)?;
    open_vectors(&engine, &inputs)?;
    let probe = FoundationalProbe::open(&config.index)?;
    let options = |share: f64, bonus: f64| SemanticRankingOptions {
        foundational_candidate_share: share,
        foundational_bonus: bonus,
        ..SemanticRankingOptions::defaults()
    };
    let configs = [
        ("none", options(0.0, 0.0)),
        ("share0", options(0.0, 0.002)),
        ("share0.5", options(0.5, 0.002)),
    ];
    let queries = QUERIES
        .iter()
        .find(|(group, _)| *group == "concept")
        .map(|(_, q)| *q)
        .unwrap_or_default();
    let limit = config.limit as usize;
    let mut out = String::new();
    let mut totals: BTreeMap<(String, String), (usize, usize)> = BTreeMap::new();
    for &query in queries {
        for &(label, mode) in SEMANTIC_LEXICAL_MODES {
            let mut lists: Vec<Vec<Ranked>> = Vec::new();
            for (_, ranking) in &configs {
                engine.invalidate_semantic_sessions_for_bench();
                let mut items = Vec::new();
                for page in 1..=3 {
                    let response = semantic_page(
                        &engine,
                        query,
                        config.limit,
                        page,
                        mode,
                        Some(ranking.clone()),
                    )?;
                    for r in &response.results {
                        items.push(Ranked {
                            title: r.title.clone(),
                            reference: r.reference.clone(),
                            key: (r.file_path.clone(), r.id),
                            base: probe.is_base(&r.file_path, r.id)?,
                            fused: r.fused_score,
                            source: format!("{:?}", r.source),
                        });
                    }
                    if !response.has_more {
                        break;
                    }
                }
                lists.push(items);
            }
            let mut line = format!("{label:<6} {query}:");
            for ((name, _), items) in configs.iter().zip(&lists) {
                let n = items.len().min(limit);
                let b = items.iter().take(limit).filter(|i| i.base).count();
                let e = totals
                    .entry((label.to_string(), name.to_string()))
                    .or_default();
                e.0 += b;
                e.1 += n;
                let _ = write!(line, "  {name} {b}/{n}");
            }
            println!("{line}");
            let _ = writeln!(out, "\n{line}");
            let (a, b) = (&lists[1], &lists[2]);
            let rank_in =
                |list: &[Ranked], k: &(String, u64)| list.iter().position(|i| i.key == *k);
            let in_top =
                |list: &[Ranked], k: &(String, u64)| list.iter().take(limit).any(|i| i.key == *k);
            for (r, i) in b.iter().enumerate().take(limit) {
                if in_top(a, &i.key) {
                    continue;
                }
                let _ = writeln!(
                    out,
                    "  IN   base={} #{} fused {:.5} {} | share0: {}  {} — {}",
                    i.base,
                    r + 1,
                    i.fused,
                    i.source,
                    rank_in(a, &i.key).map_or("beyond 90".into(), |x| format!(
                        "#{} fused {:.5}",
                        x + 1,
                        a[x].fused
                    )),
                    i.title,
                    i.reference,
                );
            }
            for (r, i) in a.iter().enumerate().take(limit) {
                if in_top(b, &i.key) {
                    continue;
                }
                let _ = writeln!(
                    out,
                    "  OUT  base={} #{} fused {:.5} {} | share0.5: {}  {} — {}",
                    i.base,
                    r + 1,
                    i.fused,
                    i.source,
                    rank_in(b, &i.key).map_or("beyond 90".into(), |x| format!(
                        "#{} fused {:.5}",
                        x + 1,
                        b[x].fused
                    )),
                    i.title,
                    i.reference,
                );
            }
            // The 30th score of each list: the bar a line had to clear.
            let bar = |l: &[Ranked]| l.get(limit - 1).map_or(f32::NAN, |i| i.fused);
            let _ = writeln!(
                out,
                "  bar(30th fused): share0 {:.5}  share0.5 {:.5}",
                bar(a),
                bar(b)
            );
        }
    }
    let _ = writeln!(out, "\nTOTAL /base in top {limit}:");
    for ((label, name), (b, n)) in &totals {
        let _ = writeln!(
            out,
            "  {label:<6} {name:<8} {b}/{n} = {:.1}%",
            100.0 * *b as f64 / *n as f64
        );
    }
    if let Some(parent) = config.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let path = config.sibling("foundational.txt");
    fs::write(&path, &out)?;
    println!("{out}\nreport: {}", path.display());
    Ok(())
}

/// Acronyms, pointed forms and quoted phrases through `search_semantic`: how the query is read
/// (quoted phrases, query type), the mode that ran, and whether the lexical half found the word.
#[cfg(feature = "semantic-integration")]
#[test]
#[ignore = "needs OTZ_INDEX and a real vector set"]
fn acronym_modes() -> Result<()> {
    use crate::api::search_engine::SemanticResultSource;
    use otzaria_semantic_search::hybrid::hebrew_normalizer::HebrewNormalizer;
    use otzaria_semantic_search::hybrid::ranking::analyze_query;
    let Some(config) = Config::from_env() else {
        println!("OTZ_INDEX is not set; nothing to measure");
        return Ok(());
    };
    let Some(inputs) = SemanticInputs::from_env() else {
        println!("semantic inputs are not set; nothing to measure");
        return Ok(());
    };
    let engine = open_engine(&config)?;
    open_vectors(&engine, &inputs)?;
    let queries = [
        "רמב\"ם",
        "רמב״ם",
        "רש\"י",
        "שו\"ע",
        "חז\"ל",
        "ז\"ל",
        "רַמְבַּ\"ם",
        "רַמְבַּ״ם",
        "רַשִׁ\"י",
        "חֲזַ\"ל",
        "שׁוּ\"ע",
        "דברי חז\"ל על השלום",
        "\"שמע ישראל\"",
        "״שמע ישראל״",
        "\"שְׁמַע יִשְׂרָאֵל\"",
        "שמע ישראל",
    ];
    let mut out = String::new();
    for query in queries {
        let normalized = HebrewNormalizer::new().normalize(query);
        let features = analyze_query(&normalized);
        let exact_count = engine.count_exact(query.to_string(), Vec::new(), false, false)?;
        for &(label, mode) in SEMANTIC_LEXICAL_MODES {
            engine.invalidate_semantic_sessions_for_bench();
            let r = semantic_page(&engine, query, config.limit, 1, mode, None)?;
            let count =
                |s: SemanticResultSource| r.results.iter().filter(|x| x.source == s).count();
            let lexical_snippet = r
                .results
                .iter()
                .find(|x| x.source != SemanticResultSource::Semantic && x.is_highlighted)
                .map(|x| {
                    let s: String = x.snippet_html.chars().take(110).collect();
                    s.replace('\n', " ")
                })
                .unwrap_or_default();
            let line = format!(
                "{query}\t{label}\tnormalized={normalized}\tquoted={:?}\ttype={:?}\texec={:?}\tfallback={:?}/{:?}\tlex_total={}\tcount_exact={exact_count}\tL/S/B={}/{}/{}\thl={}\t{lexical_snippet}",
                features.quoted_phrases,
                features.estimated_type,
                r.executed_mode,
                r.fallback_kind,
                r.fallback_reason,
                r.lexical_total_count,
                count(SemanticResultSource::Lexical),
                count(SemanticResultSource::Semantic),
                count(SemanticResultSource::Both),
                r.results.iter().filter(|x| x.is_highlighted).count(),
            );
            println!("{line}");
            let _ = writeln!(out, "{line}");
        }
    }
    if let Some(parent) = config.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let path = config.sibling("acronyms.tsv");
    fs::write(&path, format!("\u{feff}{out}"))?;
    println!("report: {}", path.display());
    Ok(())
}

/// Exact facet counts of `OTZ_FACET_QUERY` under `OTZ_FACET_PREFIX`, smallest first: where to
/// find a filter narrow enough for a search to run out of results.
#[test]
#[ignore = "needs OTZ_INDEX"]
fn facet_probe() -> Result<()> {
    let Some(config) = Config::from_env() else {
        println!("OTZ_INDEX is not set; nothing to measure");
        return Ok(());
    };
    let engine = open_engine(&config)?;
    let query = std::env::var("OTZ_FACET_QUERY").unwrap_or_else(|_| "שבת".into());
    let prefix = std::env::var("OTZ_FACET_PREFIX").unwrap_or_else(|_| "/".into());
    let mut counts =
        engine.get_facet_counts_exact(query.clone(), Vec::new(), prefix, false, false)?;
    counts.sort_by_key(|c| c.count);
    for c in counts.iter().take(40) {
        println!("{:>8}  {}", c.count, c.path);
    }
    println!("{} facets", counts.len());
    Ok(())
}
