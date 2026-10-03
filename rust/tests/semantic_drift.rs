//! A vector set of a base and deltas, and a library that has moved on since: what a
//! filtered search finds when the set's records of a book are dead in its generation.
//!
//! The releases are made as a publishing pipeline makes them, one library version at a
//! time: the version's index, its plan split against the previous release's ledger, the
//! new keys embedded by the deterministic stand-in into one warehouse, and the release
//! assembled from both. A device installs them in order. The stand-in embeds a text as a
//! hash of its words, so a query that is a line's exact text ranks that line's vector
//! first, which is what lets the tests know which line must come back.

#![cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]

use otzaria_semantic_search::distribution::assemble::{assemble, AssembleRequest, EpochChoice};
use otzaria_semantic_search::distribution::gates::simulate_device;
use otzaria_semantic_search::distribution::ledger::Ledger;
use otzaria_semantic_search::distribution::package::PackageKind;
use otzaria_semantic_search::distribution::plan::{EmbedManifest, Plan};
use otzaria_semantic_search::distribution::shard::{
    embed_shard, ShardPolicy, WorkerInfo, MODE_MOCK,
};
use otzaria_semantic_search::distribution::warehouse::{Warehouse, WarehouseIdentity};
use otzaria_semantic_search::semantic::backend::Pooling;
use otzaria_semantic_search::semantic::embedding::{
    mock, EmbeddingConfig, EmbeddingDeployment, EmbeddingRuntime,
};
use otzaria_semantic_search::semantic::model_package::validate_onnx_package;
use otzaria_semantic_search::semantic::oxv::codec::CodecSpec;
use otzaria_semantic_search::semantic::versioning::{ModelIdentity, ModelPackage};
use search_engine::api::search_engine::{
    SearchEngine, SemanticArtifactInput, SemanticCancellationToken, SemanticLexicalMode,
    SemanticRetrievalMode,
};
use search_engine::semantic_keys::production_chunking;
use search_engine::semantic_plan::{export_plan, PlanExport};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const CREATED_AT: &str = "2026-10-01T00:00:00Z";

/// `(title, topics, key, catalogue order, lines)`.
type Book = (&'static str, &'static str, &'static str, u32, Vec<String>);

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

fn index_of(dir: &Path, books: &[Book]) {
    std::fs::create_dir_all(dir).unwrap();
    let mut engine = SearchEngine::new(dir.to_str().unwrap());
    for (title, topics, key, order, lines) in books {
        engine
            .add_text_book(
                title.to_string(),
                topics.to_string(),
                key.to_string(),
                *order,
                0,
                lines.join("\n"),
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
}

/// The publishing side: one model, one warehouse, and the releases made so far.
struct Publisher {
    root: TempDir,
    model_file: PathBuf,
    model: ModelIdentity,
    warehouse: PathBuf,
    /// Each release's directory, in order.
    releases: Vec<PathBuf>,
}

impl Publisher {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
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
        let warehouse = root.path().join("warehouse");
        Warehouse::create(&warehouse, WarehouseIdentity::of(&model, &package)).unwrap();
        Self {
            root,
            model_file,
            model,
            warehouse,
            releases: Vec::new(),
        }
    }

    fn runtime(&self) -> EmbeddingRuntime {
        let mut runtime = EmbeddingRuntime::with_deployment(
            EmbeddingConfig {
                model_path: self.model_file.clone(),
                embedding_dim: self.model.embedding_dim,
                pooling: Pooling::parse(&self.model.pooling).unwrap(),
                max_tokens: self.model.max_tokens,
                batch_size: 16,
            },
            EmbeddingDeployment::default(),
        );
        runtime.load().unwrap();
        runtime
    }

    /// Release `version` of `books`: a base for the first, a delta from the last one after.
    fn release(&mut self, version: u32, books: &[Book]) {
        let index = self.root.path().join(format!("index-{version}"));
        index_of(&index, books);
        let previous = self
            .releases
            .last()
            .map(|dir| Ledger::open(dir, Some(version - 1)).unwrap());
        let plan = self.root.path().join(format!("plan-{version}"));
        {
            let warehouse = Warehouse::open(&self.warehouse).unwrap();
            export_plan(PlanExport {
                index_path: index.clone(),
                out_dir: plan.clone(),
                library_version: version,
                library_release_tag: format!("v{version}-20261001000000"),
                model: self.model.clone(),
                passage_package: self.model.query_packages[0].clone(),
                previous: previous.as_ref(),
                warehouse: Some(&warehouse),
                created_at: CREATED_AT.to_string(),
            })
            .unwrap();
        }
        let embed = EmbedManifest::read(&plan).unwrap();
        if embed.records > 0 {
            let shard = self.root.path().join(format!("shard-{version}"));
            embed_shard(
                &plan,
                &embed,
                0,
                embed.records,
                &self.runtime(),
                16,
                WorkerInfo {
                    name: "drift-test".to_string(),
                    version: "1".to_string(),
                    device: "cpu".to_string(),
                    ep: "cpu".to_string(),
                    mode: MODE_MOCK.to_string(),
                },
                &shard,
            )
            .unwrap();
            Warehouse::open_for_append(&self.warehouse)
                .unwrap()
                .add_shards(
                    Some(&plan),
                    &[shard],
                    &ShardPolicy {
                        allow_non_semantic: true,
                    },
                    CREATED_AT.to_string(),
                )
                .unwrap();
        }
        let out = self.root.path().join(format!("release-{version}"));
        assemble(&AssembleRequest {
            plan: &Plan::open(&plan).unwrap(),
            warehouse: &Warehouse::open(&self.warehouse).unwrap(),
            kind: if previous.is_some() {
                PackageKind::Delta
            } else {
                PackageKind::Base
            },
            previous: previous.as_ref(),
            epoch: match previous {
                Some(_) => EpochChoice::Previous,
                None => EpochChoice::New(CodecSpec::parse("i8-sym-vec", 1.0).unwrap()),
            },
            out_dir: out.clone(),
            created_at: CREATED_AT.to_string(),
            built_by: None,
        })
        .unwrap();
        self.releases.push(out);
    }

    /// A device that installed every release so far, and opened them over `index`.
    fn device(&self, index: &Path) -> SearchEngine {
        let vectors = self.root.path().join("vectors");
        let chain: Vec<&Path> = self.releases.iter().map(PathBuf::as_path).collect();
        simulate_device(&vectors, &chain).unwrap();
        let engine = SearchEngine::new(index.to_str().unwrap());
        engine
            .open_semantic_artifact(SemanticArtifactInput {
                vectors_dir: vectors.to_string_lossy().into_owned(),
                model_path: self.model_file.to_string_lossy().into_owned(),
                model_identity_json: serde_json::to_string(&self.model).unwrap(),
                onnx_runtime_path: None,
                scan_threads: None,
            })
            .unwrap();
        engine
    }
}

/// `(book, line)` of every semantic result of `query` under `facets`.
fn lines_of(engine: &SearchEngine, query: &str, facets: &[&str]) -> Vec<(String, u64)> {
    let response = engine
        .search_semantic(
            query.to_string(),
            facets.iter().map(|facet| facet.to_string()).collect(),
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            None,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap();
    assert!(
        response.fallback_reason.is_none(),
        "{:?}",
        response.fallback_reason
    );
    response
        .results
        .into_iter()
        .map(|hit| (hit.file_path, hit.segment))
        .collect()
}

/// A text the base records in a book, gone from the library in the next version — a
/// tombstone kills the base's slot — and back in the one after in another book, which a
/// delta ships as a new vector. The library on the device has it in the first book again.
/// The set's record of it there is dead, so a filtered scan of that book does not reach the
/// live vector: the text is an arrival of the book, its live vector is weighed besides the
/// scan, and its line in the book is found, and no line of the book the vector's live
/// record names.
#[test]
fn a_text_whose_record_in_a_book_is_dead_is_an_arrival_of_the_book() {
    let (a, w) = ("/books/a.txt", "/books/w.txt");
    let text = line(9, 0);
    let a_lines = |with: bool| -> Vec<String> {
        let mut lines = vec![line(1, 0), line(1, 1)];
        if with {
            lines.push(text.clone());
        }
        lines
    };
    // The big book gains a line in every version, so that each delta ships a vector of its
    // own besides what it tombstones.
    let w_lines = |version: usize, with: bool| -> Vec<String> {
        let mut lines: Vec<String> = (0..2 + version).map(|at| line(2, at)).collect();
        if with {
            lines.push(text.clone());
        }
        lines
    };
    let library = |version: usize, in_a: bool, in_w: bool| -> Vec<Book> {
        vec![
            ("ספר א", "/א", a, 0, a_lines(in_a)),
            ("ספר ב", "/ב", w, 1, w_lines(version, in_w)),
        ]
    };

    let mut publisher = Publisher::new();
    publisher.release(30, &library(0, true, false));
    publisher.release(31, &library(1, false, false));
    publisher.release(32, &library(2, false, true));
    let device_index = publisher.root.path().join("device-index");
    index_of(&device_index, &library(2, true, true));
    let engine = publisher.device(&device_index);

    let found = lines_of(&engine, &text, &["/א"]);
    assert_eq!(
        found.first(),
        Some(&(a.to_string(), 2)),
        "the book holds the text, whose record in it is dead: {found:?}"
    );
    assert!(
        found.iter().all(|(book, _)| book == a),
        "no line of a book the filter does not admit: {found:?}"
    );
    assert_eq!(
        lines_of(&engine, &text, &["/ב"]).first(),
        Some(&(w.to_string(), 4)),
        "the book the live vector's record names finds it as it always did"
    );
}
