//! `validate_semantic_vectors` as a publishing pipeline runs it: a release index, the plan
//! exported from it, a warehouse of the plan's vectors, a base assembled from both and
//! installed as a device installs it — and the gates' verdicts, exit status and report, on
//! that set and on sets and inputs that should fail one gate each.
//!
//! The vectors are the deterministic stand-in's, as in `tests/semantic_artifact.rs`, and the
//! query model is the stub package it serves: what G6 compares is the set's scan with the
//! exact scan of the same vectors, which needs no meaning.

#![cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]

use otzaria_semantic_search::distribution::assemble::{assemble, AssembleRequest, EpochChoice};
use otzaria_semantic_search::distribution::gates::simulate_device;
use otzaria_semantic_search::distribution::ledger::Ledger;
use otzaria_semantic_search::distribution::package::PackageKind;
use otzaria_semantic_search::distribution::plan::{
    EmbedManifest, Parity, Plan, PlanCounts, PlanManifest, RecordsWriter, BOOKS_FILE, RECORDS_FILE,
};
use otzaria_semantic_search::distribution::shard::{
    embed_shard, ShardManifest, ShardPolicy, WorkerInfo, KEYS_FILE, MODE_MOCK, SHARD_FORMAT,
    SHARD_FORMAT_VERSION, SHARD_MANIFEST_FILE, VECTORS_FILE,
};
use otzaria_semantic_search::distribution::warehouse::{Warehouse, WarehouseIdentity};
use otzaria_semantic_search::semantic::backend::Pooling;
use otzaria_semantic_search::semantic::embedding::{
    mock, EmbeddingConfig, EmbeddingDeployment, EmbeddingRuntime,
};
use otzaria_semantic_search::semantic::model_package::validate_onnx_package;
use otzaria_semantic_search::semantic::oxv::codec::CodecSpec;
use otzaria_semantic_search::semantic::versioning::{ModelIdentity, ModelPackage};
use search_engine::api::search_engine::SearchEngine;
use search_engine::semantic_keys::production_chunking;
use search_engine::semantic_plan::{export_plan, PlanExport};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const CREATED_AT: &str = "2026-10-01T00:00:00Z";

/// `(title, topics, key, catalogue order, text)`.
type Book = (&'static str, &'static str, &'static str, u32, String);

/// Line `line` of book `book`: eight to twelve words of four to seven letters drawn from a
/// seed of their own, so no two lines are alike and each stands alone.
fn line(book: usize, line: usize) -> String {
    let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ (((book as u64) << 32) | (line as u64 + 1));
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let letters: Vec<char> = "אבגדהוזחטיכלמנסעפצקרשת".chars().collect();
    let words = 8 + next() % 5;
    (0..words)
        .map(|_| {
            let len = 4 + next() % 4;
            (0..len)
                .map(|_| letters[(next() % letters.len() as u64) as usize])
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Three books of lines that stand alone, one line repeated across two of them.
fn books() -> Vec<Book> {
    let text = |book: usize, lines: usize| {
        (0..lines)
            .map(|at| line(book, at))
            .collect::<Vec<_>>()
            .join("\n")
    };
    vec![
        ("ספר אחד", "/מקרא", "/books/one.txt", 0, text(1, 20)),
        (
            "ספר שניים",
            "/משנה",
            "/books/two.txt",
            1,
            format!("{}\n{}", text(2, 15), line(1, 3)),
        ),
        ("ספר שלושה", "/תלמוד", "/books/three.txt", 2, text(3, 25)),
    ]
}

fn add_books(index: &Path, books: &[Book]) {
    let mut engine = SearchEngine::new(index.to_str().unwrap());
    for (title, topics, key, order, text) in books {
        engine
            .add_text_book(
                title.to_string(),
                topics.to_string(),
                key.to_string(),
                *order,
                0,
                text.clone(),
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
}

/// A published release and what it was made from.
struct Release {
    root: TempDir,
    index: PathBuf,
    plan: PathBuf,
    warehouse: PathBuf,
    /// The assembled release: its segment and `release.json`.
    assembled: PathBuf,
    /// The set a device holds after installing it.
    vectors: PathBuf,
    model_file: PathBuf,
    identity: PathBuf,
    model: ModelIdentity,
}

impl Release {
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
}

fn runtime(model_file: &Path, model: &ModelIdentity) -> EmbeddingRuntime {
    let mut runtime = EmbeddingRuntime::with_deployment(
        EmbeddingConfig {
            model_path: model_file.to_path_buf(),
            embedding_dim: model.embedding_dim,
            pooling: Pooling::parse(&model.pooling).unwrap(),
            max_tokens: model.max_tokens,
            batch_size: 16,
        },
        EmbeddingDeployment::default(),
    );
    runtime.load().unwrap();
    runtime
}

fn worker() -> WorkerInfo {
    WorkerInfo {
        name: "validate-test".to_string(),
        version: "1".to_string(),
        device: "cpu".to_string(),
        ep: "cpu".to_string(),
        mode: MODE_MOCK.to_string(),
    }
}

/// The pipeline, from `books`: the index, its plan, the plan's vectors embedded by the
/// stand-in into a warehouse, a base assembled from both, and that base installed.
fn release(books: &[Book]) -> Release {
    let root = TempDir::new().unwrap();
    let index = root.path().join("index");
    std::fs::create_dir_all(&index).unwrap();
    add_books(&index, books);

    let model_file = mock::write_stub_onnx_package(&root.path().join("model"));
    let chunking = production_chunking();
    let package = ModelPackage {
        checksum: validate_onnx_package(&model_file)
            .unwrap()
            .checksum()
            .to_string(),
        quantization: "int8".to_string(),
    };
    let model = ModelIdentity {
        family_id: "test-mock@0000000".to_string(),
        tokenizer_checksum: mock::stub_tokenizer_checksum(),
        embedding_dim: 64,
        pooling: "in-graph".to_string(),
        max_tokens: 512,
        embedding_text_version: chunking.embedding_text_version,
        normalization_version: chunking.normalization_version,
        chunking_identity: chunking.identity(),
        query_packages: vec![package.clone()],
    };
    let identity = root.path().join("model.json");
    std::fs::write(&identity, serde_json::to_vec_pretty(&model).unwrap()).unwrap();

    let plan = root.path().join("plan");
    export(&index, &plan, &model);
    let shard = root.path().join("shard");
    let embed = EmbedManifest::read(&plan).unwrap();
    embed_shard(
        &plan,
        &embed,
        0,
        embed.records,
        &runtime(&model_file, &model),
        16,
        worker(),
        &shard,
    )
    .unwrap();
    let warehouse = root.path().join("warehouse");
    fill_warehouse(&warehouse, &plan, &shard, &model, &package);

    let assembled = root.path().join("release");
    assemble(&AssembleRequest {
        plan: &Plan::open(&plan).unwrap(),
        warehouse: &Warehouse::open(&warehouse).unwrap(),
        kind: PackageKind::Base,
        previous: None,
        epoch: EpochChoice::New(CodecSpec::parse("i8-sym-vec", 1.0).unwrap()),
        out_dir: assembled.clone(),
        created_at: CREATED_AT.to_string(),
        built_by: None,
    })
    .unwrap();
    let vectors = root.path().join("vectors");
    simulate_device(&vectors, &[&assembled]).unwrap();
    Release {
        root,
        index,
        plan,
        warehouse,
        assembled,
        vectors,
        model_file,
        identity,
        model,
    }
}

fn export(index: &Path, out: &Path, model: &ModelIdentity) {
    export_plan(PlanExport {
        index_path: index.to_path_buf(),
        out_dir: out.to_path_buf(),
        library_version: 30,
        library_release_tag: "v30-20261001000000".to_string(),
        model: model.clone(),
        passage_package: model.query_packages[0].clone(),
        previous: None,
        warehouse: None,
        created_at: CREATED_AT.to_string(),
    })
    .unwrap();
}

fn fill_warehouse(
    dir: &Path,
    plan: &Path,
    shard: &Path,
    model: &ModelIdentity,
    package: &ModelPackage,
) {
    Warehouse::create(dir, WarehouseIdentity::of(model, package)).unwrap();
    Warehouse::open_for_append(dir)
        .unwrap()
        .add_shards(
            Some(plan),
            &[shard.to_path_buf()],
            &ShardPolicy {
                allow_non_semantic: true,
            },
            CREATED_AT.to_string(),
        )
        .unwrap();
}

/// A warehouse of the same keys as `release`'s, every vector another: what a set that was not
/// assembled from it would be measured against.
fn other_warehouse(release: &Release) -> PathBuf {
    let embed = EmbedManifest::read(&release.plan).unwrap();
    let shard = release.path("other-shard");
    std::fs::create_dir_all(&shard).unwrap();
    let (mut keys, mut vectors) = (Vec::new(), Vec::new());
    for (n, line) in std::fs::read_to_string(release.plan.join("embed.jsonl"))
        .unwrap()
        .lines()
        .enumerate()
    {
        let record: Value = serde_json::from_str(line).unwrap();
        let digest = Sha256::digest(record["embedding_text"].as_str().unwrap().as_bytes());
        keys.extend_from_slice(&digest);
        let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ (n as u64 + 1);
        let mut vector: Vec<f32> = (0..64)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5
            })
            .collect();
        let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        vector.iter_mut().for_each(|x| *x /= norm);
        vectors.extend(vector.iter().flat_map(|x| x.to_le_bytes()));
    }
    std::fs::write(shard.join(KEYS_FILE), &keys).unwrap();
    std::fs::write(shard.join(VECTORS_FILE), &vectors).unwrap();
    let manifest = ShardManifest {
        format: SHARD_FORMAT.to_string(),
        version: SHARD_FORMAT_VERSION,
        plan_sha256: embed.plan_sha256.clone(),
        skip: 0,
        take: embed.records,
        records: embed.records,
        dim: 64,
        vectors_sha256: format!("{:x}", Sha256::digest(&vectors)),
        keys_sha256: format!("{:x}", Sha256::digest(&keys)),
        model: embed.model.clone(),
        passage_package: embed.passage_package.clone(),
        worker: worker(),
        parity: None,
    };
    std::fs::write(
        shard.join(SHARD_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let warehouse = release.path("other-warehouse");
    fill_warehouse(
        &warehouse,
        &release.plan,
        &shard,
        &release.model,
        &release.model.query_packages[0],
    );
    warehouse
}

/// Run the binary with `args`: its exit status and what it printed.
fn validate(args: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_validate_semantic_vectors"))
        .args(args)
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr),
    )
}

/// Every gate's inputs for `release`'s installed set, with `warehouse` for G6, writing the
/// report to `report`.
fn all_gates(release: &Release, warehouse: &Path, report: &Path) -> Vec<String> {
    let mut args = vec![
        "--vectors".to_string(),
        release.vectors.to_string_lossy().into_owned(),
    ];
    args.extend(gate_inputs(release, warehouse, report));
    args
}

/// Every gate's inputs but the set.
fn gate_inputs(release: &Release, warehouse: &Path, report: &Path) -> Vec<String> {
    [
        "--index",
        release.index.to_str().unwrap(),
        "--plan",
        release.plan.to_str().unwrap(),
        "--warehouse",
        warehouse.to_str().unwrap(),
        "--model",
        release.model_file.to_str().unwrap(),
        "--model-identity",
        release.identity.to_str().unwrap(),
        "--report",
        report.to_str().unwrap(),
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect()
}

fn as_args(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

fn report(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The verdict of `gate` in `report`, with its metrics.
fn gate<'r>(report: &'r Value, gate: &str) -> (&'r str, &'r Value) {
    let found = report["gates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["gate"] == gate)
        .unwrap();
    (found["status"].as_str().unwrap(), &found["metrics"])
}

/// A release assembled from the index's own plan and warehouse, installed as a device
/// installs it into a set of the validator's own, passes every gate, and its report says so
/// with the numbers. Nothing of that set is left after.
#[test]
fn a_release_assembled_from_its_index_passes_every_gate() {
    let release = release(&books());
    let out = release.path("report.json");
    let mut args = vec![
        "--release".to_string(),
        release.assembled.to_string_lossy().into_owned(),
    ];
    args.extend(gate_inputs(&release, &release.warehouse, &out));
    let (code, printed) = validate(&as_args(&args));
    assert_eq!(code, 0, "{printed}");
    let report = report(&out);
    assert_eq!(report["passed"], true);
    assert_eq!(report["reportVersion"], 1);
    assert_eq!(report["set"]["libraryVersion"], 30);
    assert_eq!(
        report["releases"][0],
        release.assembled.to_string_lossy().as_ref()
    );
    let installed = PathBuf::from(report["vectors"].as_str().unwrap());
    assert!(!installed.exists(), "the set it installed is removed");

    assert_eq!(report["skipped"], serde_json::json!([]));
    let (status, g3) = gate(&report, "G3");
    assert_eq!(status, "passed", "{printed}");
    assert!(printed.contains("(100.0000%)"), "{printed}");
    assert_eq!(g3["keyedLines"], g3["coveredLines"]);
    assert_eq!(g3["uncoveredLines"], 0);
    assert_eq!(g3["plan"]["records"], g3["plan"]["reachable"]);
    assert!(g3["plan"]["firstUnreachable"].is_null());

    let (status, g4) = gate(&report, "G4");
    assert_eq!(status, "passed", "{printed}");
    assert_eq!(g4["unresolved"], 0);
    assert_eq!(g4["staleHints"], 0);
    assert_eq!(g4["booksMissing"], 0);
    assert_eq!(g4["columnChecked"], true);
    assert_eq!(g4["columnMismatches"], 0);
    assert_eq!(g4["records"], g4["atHint"]);

    let (status, g6) = gate(&report, "G6");
    assert_eq!(status, "passed", "{printed}");
    assert_eq!(g6["querySource"], "sampled");
    assert_eq!(g6["queries"], 200);
    assert!(g6["recallAt10"].as_f64().unwrap() >= 0.98, "{g6}");
    assert!(g6["recallAt50"].as_f64().unwrap() >= 0.99, "{g6}");
    assert_eq!(g6["minRecallAt10"], 0.98);
    assert_eq!(g6["minRecallAt50"], 0.99);
    assert_eq!(g6["countedOver"], "distinct texts (keys)");
}

/// A gate whose inputs are not given has not run, and the release has not passed it; one
/// the caller skips by name is recorded, and holds nothing back. G3 runs on the index alone
/// without a plan. Queries can come from a file, one per line, blank lines aside; and the
/// floors are the flags'.
#[test]
fn a_gate_is_skipped_only_by_name_and_queries_come_from_a_file() {
    let release = release(&books());
    let out = release.path("report.json");
    let base = [
        "--index",
        release.index.to_str().unwrap(),
        "--vectors",
        release.vectors.to_str().unwrap(),
        "--report",
        out.to_str().unwrap(),
    ];
    let (code, printed) = validate(&base);
    assert_eq!(code, 1, "{printed}");
    let not_run = report(&out);
    assert_eq!(not_run["passed"], false);
    assert_eq!(gate(&not_run, "G3").0, "passed");
    assert!(gate(&not_run, "G3").1["plan"].is_null());
    assert_eq!(gate(&not_run, "G4").0, "passed");
    assert_eq!(gate(&not_run, "G6").0, "notRun");
    assert!(printed.contains("--skip G6"), "{printed}");

    let mut skipping = base.to_vec();
    skipping.extend(["--skip", "g6", "--skip", "G6"]);
    let (code, printed) = validate(&skipping);
    assert_eq!(code, 0, "{printed}");
    let skipped = report(&out);
    assert_eq!(skipped["passed"], true);
    assert_eq!(skipped["skipped"], serde_json::json!(["G6"]));
    assert_eq!(gate(&skipped, "G6").0, "skipped");

    let queries = release.path("queries.txt");
    std::fs::write(&queries, "שורה של ספר\n\nמילים נוספות ארוכה\nספר שלושה\n").unwrap();
    let mut args = all_gates(&release, &release.warehouse, &out);
    args.extend(
        [
            "--queries",
            queries.to_str().unwrap(),
            "--min-recall-10",
            "0.5",
            "--min-recall-50",
            "0.5",
        ]
        .map(String::from),
    );
    let (code, printed) = validate(&as_args(&args));
    assert_eq!(code, 0, "{printed}");
    let from_file = report(&out);
    let (_, g6) = gate(&from_file, "G6");
    assert_eq!(
        (g6["querySource"].as_str(), g6["queries"].as_u64()),
        (Some("file"), Some(3))
    );
    assert_eq!(g6["minRecallAt10"], 0.5);
}

/// An index with a line the set does not cover fails G3 on the index alone, without a plan;
/// and a gate whose inputs are not given has not passed: the run fails unless the caller
/// skips that gate by name, which the report records.
#[test]
fn review_uncovered_lines_without_plan_or_warehouse() {
    let release = release(&books());
    add_books(
        &release.index,
        &[(
            "ספר ארבעה",
            "/אחר",
            "/books/four.txt",
            3,
            "שורה של ספר ארבעה שלא היה בספרייה כשהווקטורים נבנו".to_string(),
        )],
    );
    let out = release.path("report.json");
    let base = [
        "--index",
        release.index.to_str().unwrap(),
        "--vectors",
        release.vectors.to_str().unwrap(),
        "--report",
        out.to_str().unwrap(),
    ];
    let (code, printed) = validate(&base);
    assert_eq!(code, 1, "{printed}");
    let failed = report(&out);
    assert_eq!(failed["passed"], false);
    let (status, g3) = gate(&failed, "G3");
    assert_eq!(status, "failed", "{printed}");
    assert_eq!(
        g3["keyedLines"].as_u64().unwrap() - g3["coveredLines"].as_u64().unwrap(),
        1,
        "{g3}"
    );
    assert_eq!(g3["uncoveredLines"], 1);
    assert!(g3["plan"].is_null(), "no plan was given: {g3}");
    assert_eq!(gate(&failed, "G4").0, "passed", "{printed}");
    assert_eq!(gate(&failed, "G6").0, "notRun", "{printed}");

    // Skipping G6 by name runs the rest, and still fails on G3.
    let mut skipping = base.to_vec();
    skipping.extend(["--skip", "G6"]);
    let (code, printed) = validate(&skipping);
    assert_eq!(code, 1, "{printed}");
    let skipped = report(&out);
    assert_eq!(gate(&skipped, "G6").0, "skipped");
    assert_eq!(skipped["skipped"], serde_json::json!(["G6"]));
    assert_eq!(gate(&skipped, "G3").0, "failed");
}

/// A record whose book the index no longer holds, or whose book no longer holds its text,
/// resolves nowhere: G4 fails, and the binary exits 1, with the report written.
#[test]
fn a_record_the_index_no_longer_holds_fails_resolution() {
    let release = release(&books());
    let mut engine = SearchEngine::new(release.index.to_str().unwrap());
    engine
        .delete_documents_by_file_path("/books/three.txt")
        .unwrap();
    engine.commit().unwrap();
    drop(engine);

    let out = release.path("report.json");
    let (code, printed) = validate(&[
        "--index",
        release.index.to_str().unwrap(),
        "--vectors",
        release.vectors.to_str().unwrap(),
        "--report",
        out.to_str().unwrap(),
        "--skip",
        "G6",
    ]);
    assert_eq!(code, 1, "{printed}");
    let report = report(&out);
    assert_eq!(report["passed"], false);
    assert_eq!(gate(&report, "G3").0, "passed", "{printed}");
    let (status, g4) = gate(&report, "G4");
    assert_eq!(status, "failed", "{printed}");
    assert_eq!(g4["booksMissing"], 1);
    assert_eq!(g4["unresolved"], 25);
}

/// A record whose line moved in its book is stale: reported, and a failure only past
/// `--max-stale-hints`.
#[test]
fn stale_hints_are_reported_and_fail_only_past_the_limit() {
    let mut books = books();
    let release = release(&books);
    let (title, topics, key, order, text) = books.remove(0);
    // The book's first line moved to its end: every text still there, every hint stale.
    let mut lines: Vec<&str> = text.lines().collect();
    lines.rotate_left(1);
    let mut engine = SearchEngine::new(release.index.to_str().unwrap());
    engine.delete_documents_by_file_path(key).unwrap();
    engine
        .add_text_book(
            title.to_string(),
            topics.to_string(),
            key.to_string(),
            order,
            0,
            lines.join("\n"),
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    drop(engine);
    let base = [
        "--index",
        release.index.to_str().unwrap(),
        "--vectors",
        release.vectors.to_str().unwrap(),
        "--skip",
        "G6",
    ];

    let (code, printed) = validate(&base);
    assert_eq!(code, 0, "stale hints are not fatal by default: {printed}");
    assert!(printed.contains("stale"), "{printed}");
    let mut strict = base.to_vec();
    strict.extend(["--max-stale-hints", "0.1"]);
    let (code, printed) = validate(&strict);
    assert_eq!(code, 1, "{printed}");
    assert!(printed.contains("--max-stale-hints"), "{printed}");
}

/// A plan with records the set does not reach — exported from the index after a book was
/// added — fails G3.
#[test]
fn a_plan_the_set_does_not_reach_fails_coverage() {
    let release = release(&books());
    add_books(
        &release.index,
        &[(
            "ספר ארבעה",
            "/אחר",
            "/books/four.txt",
            3,
            "שורה של ספר ארבעה שלא היה בספרייה כשהווקטורים נבנו".to_string(),
        )],
    );
    let newer = release.path("newer-plan");
    export(&release.index, &newer, &release.model);
    let out = release.path("report.json");
    let (code, printed) = validate(&[
        "--index",
        release.index.to_str().unwrap(),
        "--vectors",
        release.vectors.to_str().unwrap(),
        "--plan",
        newer.to_str().unwrap(),
        "--report",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{printed}");
    let report = report(&out);
    let (status, g3) = gate(&report, "G3");
    assert_eq!(status, "failed", "{printed}");
    assert_eq!(
        g3["plan"]["records"].as_u64().unwrap() - g3["plan"]["reachable"].as_u64().unwrap(),
        1
    );
    assert_eq!(g3["plan"]["firstUnreachable"]["book"], "/books/four.txt");
    assert_eq!(
        g3["uncoveredLines"], 1,
        "the index's own line is uncovered too"
    );
}

/// Measured against a warehouse it was not assembled from, the set's scan misses the exact
/// top-k: G6 fails.
#[test]
fn a_warehouse_the_set_was_not_assembled_from_fails_retrieval() {
    let release = release(&books());
    let other = other_warehouse(&release);
    let out = release.path("report.json");
    let (code, printed) = validate(&as_args(&all_gates(&release, &other, &out)));
    assert_eq!(code, 1, "{printed}");
    let report = report(&out);
    let (status, g6) = gate(&report, "G6");
    assert_eq!(status, "failed", "{printed}");
    assert!(g6["recallAt10"].as_f64().unwrap() < 0.98, "{g6}");
    assert_eq!(gate(&report, "G4").0, "passed");
}

/// A number of queries to draw that no index could give, or none at all, is a wrong
/// argument: exit 2, as for any, not a crash.
#[test]
fn a_sample_of_queries_out_of_range_is_a_wrong_argument() {
    let release = release(&books());
    let out = release.path("report.json");
    for count in ["18446744073709551615", "100000000", "0"] {
        let mut args = all_gates(&release, &release.warehouse, &out);
        args.extend(["--sample-queries".to_string(), count.to_string()]);
        let (code, printed) = validate(&as_args(&args));
        assert_eq!(code, 2, "--sample-queries {count}: {printed}");
        assert!(printed.contains("--sample-queries"), "{printed}");
    }
}

/// Every gate skipped is no validation: a wrong argument, exit 2, never a pass.
#[test]
fn skipping_every_gate_is_a_wrong_argument() {
    let release = release(&books());
    let out = release.path("report.json");
    let (code, printed) = validate(&[
        "--index",
        release.index.to_str().unwrap(),
        "--vectors",
        release.vectors.to_str().unwrap(),
        "--report",
        out.to_str().unwrap(),
        "--skip",
        "G3",
        "--skip",
        "G4",
        "--skip",
        "G6",
    ]);
    assert_eq!(code, 2, "{printed}");
    assert!(printed.contains("--skip"), "{printed}");
    assert!(!out.exists(), "no verdict is written");
}

/// The plan of the release after `release`, with no record: one no planner writes, since
/// both refuse a library with nothing to embed.
fn empty_plan(release: &Release) -> PathBuf {
    let manifest = Plan::open(&release.plan).unwrap().manifest;
    let ledger = Ledger::open(&release.assembled, Some(30)).unwrap();
    let dir = release.path("empty-plan");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(release.plan.join(BOOKS_FILE), dir.join(BOOKS_FILE)).unwrap();
    RecordsWriter::create(&dir.join(RECORDS_FILE))
        .unwrap()
        .finish()
        .unwrap();
    PlanManifest::write(
        &dir,
        manifest.identity,
        31,
        "v31-20261002000000".to_string(),
        Some(ledger.as_previous()),
        PlanCounts::default(),
        Parity::default(),
        CREATED_AT.to_string(),
    )
    .unwrap();
    dir
}

/// `release` and then the delta of `plan`, an [`empty_plan`], installed: every key
/// tombstoned, none live. A base of no slot does not install.
fn empty_set(release: &Release, plan: &Path) -> PathBuf {
    let delta = release.path("empty-delta");
    assemble(&AssembleRequest {
        plan: &Plan::open(plan).unwrap(),
        warehouse: &Warehouse::open(&release.warehouse).unwrap(),
        kind: PackageKind::Delta,
        previous: Some(&Ledger::open(&release.assembled, Some(30)).unwrap()),
        epoch: EpochChoice::Previous,
        out_dir: delta.clone(),
        created_at: CREATED_AT.to_string(),
        built_by: None,
    })
    .unwrap();
    let vectors = release.path("empty-vectors");
    simulate_device(&vectors, &[&release.assembled, &delta]).unwrap();
    vectors
}

/// A plan of no record, for a set that covers its index: G3 fails on the plan.
#[test]
fn an_empty_plan_fails_coverage() {
    let release = release(&books());
    let out = release.path("report.json");
    let (code, printed) = validate(&[
        "--index",
        release.index.to_str().unwrap(),
        "--vectors",
        release.vectors.to_str().unwrap(),
        "--plan",
        empty_plan(&release).to_str().unwrap(),
        "--report",
        out.to_str().unwrap(),
        "--skip",
        "G6",
    ]);
    assert_eq!(code, 1, "{printed}");
    let report = report(&out);
    let (status, g3) = gate(&report, "G3");
    assert_eq!(status, "failed", "{printed}");
    assert_eq!(g3["uncoveredLines"], 0);
    assert_eq!(g3["plan"]["records"], 0);
    assert_eq!(gate(&report, "G4").0, "passed", "{printed}");
}

/// A set with no live key fails G4 and G6 too, so skipping G3 does not let it through.
#[test]
fn a_set_with_no_live_key_fails_every_gate() {
    let release = release(&books());
    let out = release.path("report.json");
    let mut args = vec![
        "--vectors".to_string(),
        empty_set(&release, &empty_plan(&release))
            .to_string_lossy()
            .into_owned(),
    ];
    args.extend(gate_inputs(&release, &release.warehouse, &out));
    let (code, printed) = validate(&as_args(&args));
    assert_eq!(code, 1, "{printed}");
    let all = report(&out);
    assert_eq!(all["set"]["slotsLive"], 0);
    assert_eq!(gate(&all, "G3").0, "failed", "{printed}");
    let (status, g4) = gate(&all, "G4");
    assert_eq!(status, "failed", "{printed}");
    assert_eq!(g4["records"], 0);
    let (status, g6) = gate(&all, "G6");
    assert_eq!(status, "failed", "{printed}");
    assert_eq!(g6["exactKeys"], 0);
    assert_eq!(g6["queries"], 200, "{g6}");

    args.extend(["--skip".to_string(), "G3".to_string()]);
    let (code, printed) = validate(&as_args(&args));
    assert_eq!(code, 1, "{printed}");
    let skipped = report(&out);
    assert_eq!(gate(&skipped, "G4").0, "failed", "{printed}");
    assert_eq!(gate(&skipped, "G6").0, "failed", "{printed}");
}

/// An index of no book: G3 has no keyed line to measure, and fails, whatever the set. With
/// the empty set and plan too, every gate has nothing to measure, and every gate fails.
#[test]
fn an_empty_index_passes_no_gate() {
    let release = release(&books());
    let index = release.path("empty-index");
    std::fs::create_dir_all(&index).unwrap();
    add_books(&index, &[]);
    let queries = release.path("queries.txt");
    std::fs::write(&queries, "שורה של ספר\n").unwrap();
    let out = release.path("report.json");
    let run = |vectors: &Path, plan: &Path| {
        validate(&[
            "--index",
            index.to_str().unwrap(),
            "--vectors",
            vectors.to_str().unwrap(),
            "--plan",
            plan.to_str().unwrap(),
            "--warehouse",
            release.warehouse.to_str().unwrap(),
            "--model",
            release.model_file.to_str().unwrap(),
            "--model-identity",
            release.identity.to_str().unwrap(),
            "--queries",
            queries.to_str().unwrap(),
            "--report",
            out.to_str().unwrap(),
        ])
    };

    let (code, printed) = run(&release.vectors, &release.plan);
    assert_eq!(code, 1, "{printed}");
    let wrong_index = report(&out);
    let (status, g3) = gate(&wrong_index, "G3");
    assert_eq!(status, "failed", "{printed}");
    assert_eq!(g3["keyedLines"], 0);
    assert_eq!(g3["plan"]["records"], g3["plan"]["reachable"]);

    let plan = empty_plan(&release);
    let (code, printed) = run(&empty_set(&release, &plan), &plan);
    assert_eq!(code, 1, "{printed}");
    let report = report(&out);
    assert_eq!(report["passed"], false);
    for name in ["G3", "G4", "G6"] {
        assert_eq!(gate(&report, name).0, "failed", "{name}: {printed}");
    }
    assert!(
        !printed.contains("0000%"),
        "none of none is no share: {printed}"
    );
}

/// Arguments that are wrong, and inputs that do not read, exit 2 without a verdict.
#[test]
fn wrong_arguments_and_unreadable_inputs_exit_2() {
    let release = release(&books());
    let index = release.index.to_str().unwrap();
    let vectors = release.vectors.to_str().unwrap();
    for args in [
        vec!["--vectors", vectors],
        vec!["--index", index, "--vectors", vectors, "--frobnicate", "1"],
        vec![
            "--index",
            index,
            "--vectors",
            vectors,
            "--warehouse",
            vectors,
        ],
        vec![
            "--index",
            index,
            "--vectors",
            vectors,
            "--max-stale-hints",
            "2",
        ],
        vec!["--index", index, "--vectors", "/nowhere/at/all"],
        vec!["--index", index],
        vec![
            "--index",
            index,
            "--vectors",
            vectors,
            "--release",
            release.assembled.to_str().unwrap(),
        ],
        vec!["--index", index, "--release", index],
        vec![
            "--index",
            index,
            "--vectors",
            vectors,
            "--max-stale-hints",
            "NaN",
        ],
        vec![
            "--index",
            index,
            "--vectors",
            vectors,
            "--min-recall-10",
            "NaN",
        ],
        vec!["--index", index, "--vectors", vectors, "--skip", "G5"],
        vec!["--index", index, "--vectors", vectors, "--skip"],
    ] {
        let (code, printed) = validate(&args);
        assert_eq!(code, 2, "{args:?}: {printed}");
    }
    let (code, printed) = validate(&["--help"]);
    assert_eq!(code, 0, "{printed}");
    assert!(printed.contains("--min-recall-10"), "{printed}");
}
