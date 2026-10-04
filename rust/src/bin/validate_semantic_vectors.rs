//! The gates a vector set passes on the release index before it is published: the ones the
//! sidecar's `assemble --verify` cannot check, because they need the index.
//!
//! ```text
//! validate_semantic_vectors --index ./index --release ./published-v29 --release ./v30 \
//!     --plan ./plan --warehouse ./warehouse --model seforim-embed-round2-int8.onnx \
//!     --model-identity model.json --report gates.json
//! ```
//!
//! The set is the one a device holds after installing the releases given with `--release`,
//! assembled release directories in the order a device installs them — the published state
//! first, the new release last — installed into a set of the validator's own, which is
//! removed after (the sidecar's `simulate_device`); or a set installed already, `--vectors`.
//!
//! * **G3, coverage**: every line of the release index the recipe embeds has its text
//!   recorded by the set in its own book, by all 128 bits of the key recomputed from the
//!   stored text; and with `--plan`, every record of the plan the set was assembled from is
//!   one a scan of the installed set reaches, by the sidecar's own count.
//! * **G4, resolution**: every record of the set resolves on the release index —
//!   its book is there, and holds its text, by all 128 bits of the key recomputed from the
//!   stored text — and every line's `chunkKey` column, which a device resolves by, is its
//!   text's. A record whose text is elsewhere in its book than its hint is stale: reported,
//!   and fatal only past `--max-stale-hints`.
//! * **G6, retrieval** (with `--warehouse`, `--model` and `--model-identity`): recall@10 and
//!   recall@50 of the set's scan, as a device runs it, against the exact `f32` scan of the
//!   same keys in the warehouse the set was assembled from (the sidecar's
//!   `ExactReference`), each query embedded once by the runtime query model and handed to
//!   both. The queries are `--queries`, one per line, or else `--sample-queries` spans of
//!   3–10 words of the index's lines, drawn with a fixed seed: the same queries for the same
//!   index. Recall is counted over keys — distinct texts — since that is what a scan returns:
//!   a text repeated in many books is one hit however many lines it resolves to, so lines
//!   that repeat a text cannot crowd the top 50 here as they do on a results page.
//!
//! A release passes when every gate ran and passed. A gate with nothing to measure — an
//! index with no keyed line, a plan or a set with no record, no live key to recall — fails.
//! A gate whose inputs are not given has not run, which fails the release like a gate that
//! failed, unless the caller skips it by name (`--skip G6`), which the output and the report
//! record; skipping every gate is a wrong argument. Exit 0 when every gate
//! passed or was skipped so, 1 when one failed or did not run, 2 when the inputs could not
//! be read or the arguments are wrong. `--report` writes every gate's verdict and numbers as
//! JSON, whatever the outcome when the gates ran.
//!
//! Nothing is written but the report, and the set `--release` installs. The index is opened
//! read-only and its lines keyed from their text as `export_semantic_plan` keys them; the set
//! is opened as a device opens one, which cleans up after an install that was cut off.
//! G6 loads the model, so it needs a build with an embedding backend: `--features semantic`
//! for the ONNX graph, the stand-in for tests.

#[cfg(not(feature = "semantic-integration"))]
fn main() {
    eprintln!(
        "This binary was compiled without the semantic sidecar.\n\
         Rebuild with --features semantic (or semantic-integration, without the retrieval \
         gate)."
    );
    std::process::exit(2);
}

#[cfg(feature = "semantic-integration")]
fn main() {
    let outcome = gates::run(std::env::args().skip(1).collect());
    std::process::exit(match outcome {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(error) => {
            eprintln!("{error:#}");
            2
        }
    });
}

#[cfg(feature = "semantic-integration")]
mod gates {
    use anyhow::{bail, Context, Result};
    use otzaria_semantic_search::cancellation::CancellationToken;
    use otzaria_semantic_search::distribution::gates::{
        coverage, recall, simulate_device, ExactReference,
    };
    use otzaria_semantic_search::distribution::plan::Plan;
    use otzaria_semantic_search::distribution::warehouse::Warehouse;
    use otzaria_semantic_search::semantic::backend::Pooling;
    use otzaria_semantic_search::semantic::chunk_key::ChunkKey;
    use otzaria_semantic_search::semantic::embedding::{
        EmbeddingConfig, EmbeddingDeployment, EmbeddingRuntime,
    };
    use otzaria_semantic_search::semantic::oxv::scan::ScanRequest;
    use otzaria_semantic_search::semantic::recipe::{
        query_input, EmbeddingTextRecipe, TextNormalizationRecipe,
    };
    use otzaria_semantic_search::semantic::segment_set::SegmentSet;
    use otzaria_semantic_search::semantic::versioning::{ModelIdentity, ModelPackage};
    use search_engine::semantic_plan::{
        installation_identity, sample_queries, validate, Validation, MAX_SAMPLED_QUERIES,
    };
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    /// G6's floors when no flag gives them: the spec's.
    const MIN_RECALL_AT_10: f64 = 0.98;
    const MIN_RECALL_AT_50: f64 = 0.99;
    /// Queries drawn from the index when no `--queries` file is given.
    const SAMPLED_QUERIES: usize = 200;
    /// The version of the report's JSON shape.
    const REPORT_VERSION: u32 = 1;

    /// The gates, by name: what `--skip` takes.
    const GATES: [&str; 3] = ["G3", "G4", "G6"];

    /// The flags, by name, each taking one value.
    const VALUED: &[&str] = &[
        "--index",
        "--vectors",
        "--plan",
        "--max-stale-hints",
        "--warehouse",
        "--model",
        "--model-identity",
        "--onnx-runtime",
        "--queries",
        "--sample-queries",
        "--min-recall-10",
        "--min-recall-50",
        "--threads",
        "--report",
    ];

    struct Args {
        values: HashMap<&'static str, String>,
        /// `--release`, a flag given once per release, in order.
        releases: Vec<PathBuf>,
        /// `--skip`, a flag given once per gate the caller opts out of.
        skipped: Vec<&'static str>,
    }

    impl Args {
        fn parse(args: Vec<String>) -> Result<Option<Self>> {
            let mut values = HashMap::new();
            let mut releases = Vec::new();
            let mut skipped = Vec::new();
            let mut args = args.into_iter();
            while let Some(arg) = args.next() {
                if arg == "--help" || arg == "-h" {
                    return Ok(None);
                }
                if arg == "--release" {
                    let Some(value) = args.next() else {
                        bail!("--release needs a value\n\n{USAGE}");
                    };
                    releases.push(PathBuf::from(value));
                    continue;
                }
                if arg == "--skip" {
                    let Some(value) = args.next() else {
                        bail!("--skip needs a gate\n\n{USAGE}");
                    };
                    let Some(gate) = GATES.iter().find(|gate| gate.eq_ignore_ascii_case(&value))
                    else {
                        bail!(
                            "--skip {value:?} names no gate: {}\n\n{USAGE}",
                            GATES.join(", ")
                        );
                    };
                    if !skipped.contains(gate) {
                        skipped.push(*gate);
                    }
                    continue;
                }
                let Some(name) = VALUED.iter().find(|name| **name == arg) else {
                    bail!("unknown argument {arg:?}\n\n{USAGE}");
                };
                let Some(value) = args.next() else {
                    bail!("{name} needs a value\n\n{USAGE}");
                };
                if values.insert(*name, value).is_some() {
                    bail!("{name} is given twice\n\n{USAGE}");
                }
            }
            skipped.sort_unstable();
            if skipped.len() == GATES.len() {
                bail!("--skip names every gate, which leaves nothing to validate\n\n{USAGE}");
            }
            Ok(Some(Self {
                values,
                releases,
                skipped,
            }))
        }

        fn get(&self, name: &str) -> Option<&str> {
            self.values.get(name).map(String::as_str)
        }

        fn skips(&self, gate: &str) -> bool {
            self.skipped.contains(&gate)
        }

        fn required(&self, name: &str) -> Result<&str> {
            self.get(name)
                .with_context(|| format!("{name} is required\n\n{USAGE}"))
        }

        fn number<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>> {
            self.get(name)
                .map(|value| {
                    value
                        .parse()
                        .ok()
                        .with_context(|| format!("{name} {value:?} is not a number"))
                })
                .transpose()
        }

        fn ratio(&self, name: &str) -> Result<Option<f64>> {
            match self.number::<f64>(name)? {
                Some(value) if !(0.0..=1.0).contains(&value) => {
                    bail!("{name} is {value}, and it is a fraction from 0 to 1")
                }
                value => Ok(value),
            }
        }
    }

    /// What became of one gate.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Status {
        Passed,
        Failed,
        /// Not run because the caller said so, with `--skip`: no verdict, and no failure.
        Skipped,
        /// Not run because its inputs were not given: a release it has not passed.
        NotRun,
    }

    impl Status {
        fn of(passed: bool) -> Self {
            if passed {
                Self::Passed
            } else {
                Self::Failed
            }
        }

        /// Whether a release may go out with the gate so.
        fn clears(self) -> bool {
            matches!(self, Self::Passed | Self::Skipped)
        }
    }

    /// One gate's verdict.
    struct Gate {
        gate: &'static str,
        name: &'static str,
        status: Status,
        detail: String,
        metrics: Value,
    }

    impl Gate {
        fn skipped(gate: &'static str, name: &'static str) -> Self {
            Self {
                gate,
                name,
                status: Status::Skipped,
                detail: format!("skipped: --skip {gate}"),
                metrics: Value::Null,
            }
        }

        fn not_run(gate: &'static str, name: &'static str, missing: &str) -> Self {
            Self {
                gate,
                name,
                status: Status::NotRun,
                detail: format!("not run: no {missing}; give them, or --skip {gate}"),
                metrics: Value::Null,
            }
        }

        fn json(&self) -> Value {
            json!({
                "gate": self.gate,
                "name": self.name,
                "status": match self.status {
                    Status::Passed => "passed",
                    Status::Failed => "failed",
                    Status::Skipped => "skipped",
                    Status::NotRun => "notRun",
                },
                "detail": self.detail,
                "metrics": self.metrics,
            })
        }
    }

    /// Run every gate `args` does not skip: `Ok(true)` when every one passed or was skipped.
    pub(crate) fn run(args: Vec<String>) -> Result<bool> {
        let Some(args) = Args::parse(args)? else {
            println!("{USAGE}");
            return Ok(true);
        };
        let index = PathBuf::from(args.required("--index")?);
        // The set: one installed already, or the one the releases install into a set of this
        // run's own, removed after.
        let installed;
        let vectors = match (args.get("--vectors"), args.releases.as_slice()) {
            (Some(vectors), []) => PathBuf::from(vectors),
            (None, releases) if !releases.is_empty() => {
                installed = tempfile::Builder::new()
                    .prefix("validate-semantic-vectors-")
                    .tempdir()
                    .context("creating a set to install the releases into")?;
                let vectors = installed.path().join("vectors");
                let chain: Vec<&Path> = releases.iter().map(PathBuf::as_path).collect();
                simulate_device(&vectors, &chain).map_err(|error| {
                    anyhow::anyhow!(
                        "the releases do not install as a device installs them: {error}"
                    )
                })?;
                vectors
            }
            _ => bail!("give --vectors, or one --release or more, and not both\n\n{USAGE}"),
        };
        let max_stale_hints = args.ratio("--max-stale-hints")?;
        let min_recall_10 = args.ratio("--min-recall-10")?.unwrap_or(MIN_RECALL_AT_10);
        let min_recall_50 = args.ratio("--min-recall-50")?.unwrap_or(MIN_RECALL_AT_50);
        let sample = match args.number::<u64>("--sample-queries")? {
            Some(count) if !(1..=MAX_SAMPLED_QUERIES as u64).contains(&count) => bail!(
                "--sample-queries is {count}, and it is a count from 1 to {MAX_SAMPLED_QUERIES}"
            ),
            Some(count) => count as usize,
            None => SAMPLED_QUERIES,
        };
        let threads = match args.number::<usize>("--threads")? {
            Some(0) => bail!("--threads is 0, and a scan needs one"),
            Some(threads) => threads,
            None => std::thread::available_parallelism().map_or(1, |cores| cores.get()),
        };
        let retrieval_inputs = [
            args.get("--warehouse"),
            args.get("--model"),
            args.get("--model-identity"),
        ];
        if retrieval_inputs.iter().any(Option::is_some)
            && retrieval_inputs.iter().any(Option::is_none)
        {
            bail!("G6 needs --warehouse, --model and --model-identity together\n\n{USAGE}");
        }

        let set = SegmentSet::open(&vectors)
            .map_err(|error| anyhow::anyhow!("could not open the vector set: {error}"))?;
        let info = set.info();
        println!(
            "Vector set: generation {}, library version {} ({:?}), {} segment(s), {} live and \
             {} dead slot(s)",
            info.generation,
            info.library_version,
            info.library_release_tag,
            info.segments.len(),
            info.slots_live,
            info.slots_dead
        );

        // G3 and G4 both read every book of the index against the set, once.
        let validation = if args.skips("G3") && args.skips("G4") {
            None
        } else {
            Some(validate(&index, &set).context("could not validate against the index")?)
        };
        let coverage_gate = match &validation {
            _ if args.skips("G3") => Gate::skipped("G3", "coverage"),
            Some(validation) => g3(&set, validation, args.get("--plan").map(Path::new))?,
            None => unreachable!("the index is read unless G3 and G4 are both skipped"),
        };
        let resolution_gate = match &validation {
            _ if args.skips("G4") => Gate::skipped("G4", "resolution"),
            Some(validation) => g4(validation, max_stale_hints),
            None => unreachable!("the index is read unless G3 and G4 are both skipped"),
        };
        let retrieval_gate = match retrieval_inputs {
            _ if args.skips("G6") => Gate::skipped("G6", "retrieval"),
            [Some(warehouse), Some(model), Some(identity)] => g6(
                &index,
                &set,
                &Retrieval {
                    warehouse: Path::new(warehouse),
                    model: Path::new(model),
                    identity: Path::new(identity),
                    onnx_runtime: args.get("--onnx-runtime").map(PathBuf::from),
                    queries: args.get("--queries").map(PathBuf::from),
                    sample,
                    min_recall_10,
                    min_recall_50,
                    threads,
                },
            )?,
            _ => Gate::not_run(
                "G6",
                "retrieval",
                "--warehouse, --model and --model-identity",
            ),
        };
        let gates = [coverage_gate, resolution_gate, retrieval_gate];

        for gate in &gates {
            let status = match gate.status {
                Status::Passed => "PASS",
                Status::Failed => "FAIL",
                Status::Skipped => "SKIP",
                Status::NotRun => "NOT RUN",
            };
            println!("{} {:<10} {status}  {}", gate.gate, gate.name, gate.detail);
        }
        let passed = gates.iter().all(|gate| gate.status.clears());
        if let Some(report) = args.get("--report") {
            let document = json!({
                "tool": "validate_semantic_vectors",
                "reportVersion": REPORT_VERSION,
                "passed": passed,
                "index": index.display().to_string(),
                "vectors": vectors.display().to_string(),
                "releases": args
                    .releases
                    .iter()
                    .map(|release| release.display().to_string())
                    .collect::<Vec<_>>(),
                "skipped": args.skipped,
                "set": {
                    "generation": info.generation,
                    "identityDigest": info.identity_digest,
                    "libraryVersion": info.library_version,
                    "libraryReleaseTag": info.library_release_tag,
                    "segments": info.segments.len(),
                    "slotsLive": info.slots_live,
                    "slotsDead": info.slots_dead,
                },
                "gates": gates.iter().map(Gate::json).collect::<Vec<_>>(),
            });
            std::fs::write(
                report,
                serde_json::to_vec_pretty(&document).expect("a report serializes"),
            )
            .with_context(|| format!("writing the report to {report}"))?;
        }
        Ok(passed)
    }

    fn percent(part: u64, whole: u64) -> String {
        if whole == 0 {
            "n/a".to_string()
        } else {
            format!("{:.4}%", 100.0 * part as f64 / whole as f64)
        }
    }

    /// G3: every keyed line of the index is covered by the set; with a plan, every record of
    /// the plan is reachable in the set.
    fn g3(set: &SegmentSet, validation: &Validation, plan: Option<&Path>) -> Result<Gate> {
        let uncovered = validation.keyed_lines - validation.covered_lines;
        let mut faults = Vec::new();
        if validation.keyed_lines == 0 {
            faults.push("the index has no keyed line, so there is no coverage to measure".into());
        }
        if uncovered > 0 {
            faults.push(format!(
                "{uncovered} keyed line(s) of the index have no record of their text in their \
                 book"
            ));
        }
        let mut detail = format!(
            "{} of the index's {} keyed line(s) recorded in their book ({})",
            validation.covered_lines,
            validation.keyed_lines,
            percent(validation.covered_lines, validation.keyed_lines)
        );
        let plan_metrics = match plan {
            Some(plan) => {
                let plan = Plan::open(plan)
                    .map_err(|error| anyhow::anyhow!("could not open the plan: {error}"))?;
                let reached = coverage(set, &plan);
                if reached.records == 0 {
                    faults.push("the plan has no record, so there is no reach to measure".into());
                } else if !reached.complete() {
                    faults.push(format!(
                        "{} of the plan's record(s) not reachable in the set",
                        reached.records - reached.reachable
                    ));
                }
                detail.push_str(&format!(
                    "; {} of the plan's {} record(s) reachable in the set ({})",
                    reached.reachable,
                    reached.records,
                    percent(reached.reachable, reached.records)
                ));
                if let Some((book, ordinal)) = &reached.first_unreachable {
                    detail.push_str(&format!(", the first not: {book} line {ordinal}"));
                }
                json!({
                    "records": reached.records,
                    "reachable": reached.reachable,
                    "firstUnreachable": reached.first_unreachable.map(|(book, ordinal)| {
                        json!({ "book": book, "ordinal": ordinal })
                    }),
                })
            }
            None => {
                detail.push_str("; no --plan, so the plan's reach was not checked");
                Value::Null
            }
        };
        Ok(Gate {
            gate: "G3",
            name: "coverage",
            status: Status::of(faults.is_empty()),
            detail: if faults.is_empty() {
                detail
            } else {
                format!("{}; {detail}", faults.join("; "))
            },
            metrics: json!({
                "keyedLines": validation.keyed_lines,
                "coveredLines": validation.covered_lines,
                "uncoveredLines": uncovered,
                "plan": plan_metrics,
            }),
        })
    }

    /// G4: every record resolves and verifies on the index; stale hints within the limit.
    fn g4(validation: &Validation, max_stale_hints: Option<f64>) -> Gate {
        let stale = if validation.records == 0 {
            0.0
        } else {
            validation.moved as f64 / validation.records as f64
        };
        // 100 × the ratio, as before: `percent` rounds some shares the other way.
        let stale_share = if validation.records == 0 {
            "n/a".to_string()
        } else {
            format!("{:.4}%", 100.0 * stale)
        };
        let mut faults = Vec::new();
        if validation.records == 0 {
            faults.push("the set holds no record, so there is nothing to resolve".to_string());
        }
        if validation.gone > 0 {
            faults.push(format!(
                "{} record(s) resolve to no line of the index{}",
                validation.gone,
                if validation.books_missing > 0 {
                    format!(
                        ", every record of {} book(s) it does not hold among them",
                        validation.books_missing
                    )
                } else {
                    String::new()
                }
            ));
        }
        if validation.column_mismatches > 0 {
            faults.push(format!(
                "{} line(s) hold a chunkKey that is not their text's",
                validation.column_mismatches
            ));
        }
        if let Some(limit) = max_stale_hints {
            if stale > limit {
                faults.push(format!(
                    "{stale_share} of the records are stale, more than --max-stale-hints {limit}"
                ));
            }
        }
        let summary = format!(
            "{} record(s): {} at their hint, {} stale ({}), {} unresolved; chunkKey column {}",
            validation.records,
            validation.at_hint,
            validation.moved,
            stale_share,
            validation.gone,
            if validation.column_checked {
                format!(
                    "held to the text, {} mismatch(es)",
                    validation.column_mismatches
                )
            } else {
                "absent, so keys come from the text alone".to_string()
            }
        );
        Gate {
            gate: "G4",
            name: "resolution",
            status: Status::of(faults.is_empty()),
            detail: if faults.is_empty() {
                summary
            } else {
                format!("{}; {summary}", faults.join("; "))
            },
            metrics: json!({
                "records": validation.records,
                "atHint": validation.at_hint,
                "staleHints": validation.moved,
                "staleRatio": stale,
                "maxStaleHints": max_stale_hints,
                "unresolved": validation.gone,
                "booksMissing": validation.books_missing,
                "columnChecked": validation.column_checked,
                "columnMismatches": validation.column_mismatches,
            }),
        }
    }

    /// What G6 needs.
    struct Retrieval<'a> {
        warehouse: &'a Path,
        model: &'a Path,
        identity: &'a Path,
        onnx_runtime: Option<PathBuf>,
        queries: Option<PathBuf>,
        sample: usize,
        min_recall_10: f64,
        min_recall_50: f64,
        threads: usize,
    }

    /// G6: recall of the runtime scan against the exact scan of the warehouse's vectors.
    fn g6(index: &Path, set: &SegmentSet, request: &Retrieval<'_>) -> Result<Gate> {
        let started = Instant::now();
        let family: ModelIdentity = serde_json::from_str(
            &std::fs::read_to_string(request.identity)
                .with_context(|| format!("reading {}", request.identity.display()))?,
        )
        .with_context(|| format!("{} is not a model identity", request.identity.display()))?;
        let runtime = query_runtime(request, &family)?;
        let checksum = runtime.model_checksum().unwrap_or_default().to_string();
        let package = family
            .query_packages
            .iter()
            .find(|package| package.checksum == checksum)
            .cloned()
            .with_context(|| {
                format!(
                    "the model at {} is the package {checksum}, which is none of the family's \
                     query packages",
                    request.model.display()
                )
            })?;
        // The set opens with this model as a device's does: the same identity, the loaded
        // package one the set takes queries from.
        let expected = installation_identity(ModelIdentity {
            tokenizer_checksum: runtime.tokenizer_checksum().unwrap_or_default().to_string(),
            query_packages: vec![ModelPackage {
                checksum: checksum.clone(),
                quantization: package.quantization.clone(),
            }],
            ..family.clone()
        });
        set.identity().verify_matches(&expected).map_err(|error| {
            anyhow::anyhow!("the set does not take queries from this model: {error}")
        })?;

        let queries = match &request.queries {
            Some(path) => std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>(),
            None => sample_queries(index, request.sample)
                .context("could not draw queries from the index")?,
        };
        if queries.is_empty() {
            bail!("there is no query to measure retrieval with");
        }
        let source = if request.queries.is_some() {
            "file"
        } else {
            "sampled"
        };
        let text = EmbeddingTextRecipe::from_version(family.embedding_text_version)?;
        let normalization = TextNormalizationRecipe::from_version(family.normalization_version)?;

        let warehouse = Warehouse::open(request.warehouse)
            .map_err(|error| anyhow::anyhow!("could not open the warehouse: {error}"))?;
        let reference = ExactReference::new(set, &warehouse).map_err(|error| {
            anyhow::anyhow!("the warehouse is not the one the set was assembled from: {error}")
        })?;
        let cancel = CancellationToken::new();
        let (mut sum_10, mut sum_50) = (0.0, 0.0);
        let (mut worst_10, mut worst_50) = (1.0f64, 1.0f64);
        for query in &queries {
            let input = query_input(text, normalization, query)
                .with_context(|| format!("the query {query:?} has nothing to embed"))?;
            let vector = runtime
                .embed_one(&input)
                .with_context(|| format!("embedding the query {query:?}"))?;
            let scanned: Vec<ChunkKey> = set
                .scan(
                    &vector,
                    &ScanRequest {
                        top_k: 50,
                        books: None,
                        threads: request.threads,
                    },
                    &cancel,
                )
                .map_err(|error| anyhow::anyhow!("scanning the set: {error}"))?
                .into_iter()
                .map(|hit| hit.key)
                .collect();
            let exact: Vec<ChunkKey> = reference
                .top_k(&vector, 50, request.threads)
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            let at = |keys: &[ChunkKey], k: usize| keys[..k.min(keys.len())].to_vec();
            let r10 = recall(&at(&scanned, 10), &at(&exact, 10));
            let r50 = recall(&at(&scanned, 50), &at(&exact, 50));
            sum_10 += r10;
            sum_50 += r50;
            worst_10 = worst_10.min(r10);
            worst_50 = worst_50.min(r50);
        }
        let n = queries.len() as f64;
        let (recall_10, recall_50) = (sum_10 / n, sum_50 / n);
        // The sidecar's recall against no exact key is 1.0, which measures nothing.
        let measured = !reference.is_empty();
        let passed =
            measured && recall_10 >= request.min_recall_10 && recall_50 >= request.min_recall_50;
        Ok(Gate {
            gate: "G6",
            name: "retrieval",
            status: Status::of(passed),
            detail: format!(
                "{}recall@10 {recall_10:.4} (at least {}), recall@50 {recall_50:.4} over \
                 distinct texts (at least {}), on {} {source} queries against {} exact key(s)",
                if measured {
                    ""
                } else {
                    "the set holds no live key, so there is no recall to measure; "
                },
                request.min_recall_10,
                request.min_recall_50,
                queries.len(),
                reference.len()
            ),
            metrics: json!({
                "queries": queries.len(),
                "querySource": source,
                "recallAt10": recall_10,
                "recallAt50": recall_50,
                "minRecallAt10": request.min_recall_10,
                "minRecallAt50": request.min_recall_50,
                "worstRecallAt10": worst_10,
                "worstRecallAt50": worst_50,
                "countedOver": "distinct texts (keys)",
                "exactKeys": reference.len(),
                "queryPackage": { "checksum": checksum, "quantization": package.quantization },
                "elapsedMs": started.elapsed().as_millis() as u64,
            }),
        })
    }

    /// The query model, loaded as a device loads it: one query at a time.
    fn query_runtime(request: &Retrieval<'_>, family: &ModelIdentity) -> Result<EmbeddingRuntime> {
        let mut runtime = EmbeddingRuntime::with_deployment(
            EmbeddingConfig {
                model_path: request.model.to_path_buf(),
                embedding_dim: family.embedding_dim,
                pooling: Pooling::parse(&family.pooling)?,
                max_tokens: family.max_tokens,
                batch_size: 1,
            },
            EmbeddingDeployment {
                onnx_runtime: request.onnx_runtime.clone(),
            },
        );
        runtime
            .load()
            .with_context(|| format!("loading the query model {}", request.model.display()))?;
        Ok(runtime)
    }

    const USAGE: &str = "\
The gates a vector set passes on the release index before it is published.

Usage:
  validate_semantic_vectors --index <dir> (--release <dir>... | --vectors <dir>) [options]

  --index <dir>             The release index, opened read-only
  --release <dir>           An assembled release, its segment.oxv and release.json; given
                            once per release, the published state first and the new one
                            last, they are installed as a device installs them into a set
                            of this run's own, removed after
  --vectors <dir>           Or a vector set installed already

G3, coverage — every keyed line of the index recorded in its book by the set:
  --plan <dir>              And the plan the set was assembled from: every record must be
                            reachable in the set

G4, resolution — every record of the set resolves on the index:
  --max-stale-hints <r>     Fail when more than this fraction of the records is not at its
                            hint (default: report stale hints, never fail on them)

G6, retrieval — recall of the set's scan against the exact f32 scan:
  --warehouse <dir>         The warehouse the set was assembled from
  --model <file>            The query model a device runs: the ONNX graph, its
                            tokenizer.json beside it
  --model-identity <file>   The model family's identity, model.json
  --onnx-runtime <file>     The ONNX Runtime library (default: OTZARIA_ONNX_RUNTIME, then
                            the one beside the model)
  --queries <file>          The queries, one per line (default: drawn from the index)
  --sample-queries <n>      How many to draw from the index without --queries, from 1 to
                            100000 (default 200)
  --min-recall-10 <r>       The least mean recall@10 (default 0.98)
  --min-recall-50 <r>       The least mean recall@50, over distinct texts (default 0.99)
  --threads <n>             Threads to scan with (default: every core)

  --skip <gate>             Do not run G3, G4 or G6, and do not hold the release to it;
                            given once per gate, and recorded in the output and the report.
                            A gate whose inputs are not given, and is not skipped, has not
                            run, and fails the release. At least one gate runs: skipping
                            all three is a wrong argument
  --report <file>           Write every gate's verdict and numbers as JSON

Exit status: 0 when every gate passed or was skipped, 1 when one failed or did not run,
2 when the inputs could not be read or the arguments are wrong.";

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 23 of 640 is a tie that 100 × 23 / 640 and 100 × (23 / 640) round apart.
        #[test]
        fn a_stale_share_prints_alike_in_the_fault_and_the_summary() {
            let validation = Validation {
                records: 640,
                at_hint: 617,
                moved: 23,
                ..Validation::default()
            };
            let detail = g4(&validation, Some(0.01)).detail;
            assert_eq!(detail.matches("3.5937%").count(), 2, "{detail}");
        }
    }
}
