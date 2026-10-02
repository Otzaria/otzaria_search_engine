//! `export_semantic_plan` over a small library: the sidecar's plan files, byte for byte what
//! its reference planner writes over the same index, from a version 5 index held to its
//! `chunkKey` column and from a version 4 index keyed from its text alike.

#![cfg(feature = "semantic-integration")]

use otzaria_semantic_search::distribution::plan::{
    plan_from_corpus, HeldVectors, Plan, PlanManifest, PlanRequest, BOOKS_FILE,
    EMBED_MANIFEST_FILE, EMBED_PLAN_FILE, RECORDS_FILE, TOMBSTONES_FILE,
};
use otzaria_semantic_search::semantic::versioning::{ModelIdentity, ModelPackage};
use search_engine::api::search_engine::SearchEngine;
use search_engine::semantic_corpus::TantivyCorpus;
use search_engine::semantic_keys::production_chunking;
use search_engine::semantic_plan::{export_plan, PlanExport};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const LIBRARY_VERSION: u32 = 30;
const RELEASE_TAG: &str = "v30-20261001000000";
const CREATED_AT: &str = "2026-10-01T00:00:00Z";
const REPEATED: &str = "In the beginning the scribe copied every line with care.";

/// Three books: long lines that stand alone, short ones that borrow their neighbours', one
/// too short to embed, and a line repeated inside a book and across two.
fn books() -> Vec<(&'static str, &'static str, u32, String)> {
    vec![
        (
            "Book A",
            "/books/a.txt",
            0,
            format!(
                "<h2>Chapter one</h2>\n{REPEATED}\nA short one\nok\n{REPEATED}\n\
                 The second line of the chapter is long enough to stand alone."
            ),
        ),
        (
            "Book B",
            "/books/b.txt",
            1,
            format!(
                "<h2>Another book</h2>\n{REPEATED}\n\
                 Lines that differ from the first book's lines entirely.\ntiny"
            ),
        ),
        (
            "Book C",
            "/books/c.txt",
            2,
            "Only one line here, but it is long enough to embed.".to_string(),
        ),
    ]
}

fn model() -> ModelIdentity {
    let chunking = production_chunking();
    ModelIdentity {
        family_id: "test-plan@0000000".to_string(),
        tokenizer_checksum: "a".repeat(64),
        embedding_dim: 256,
        pooling: "in-graph".to_string(),
        max_tokens: 256,
        embedding_text_version: chunking.embedding_text_version,
        normalization_version: chunking.normalization_version,
        chunking_identity: chunking.identity(),
        query_packages: vec![
            ModelPackage {
                checksum: "b".repeat(64),
                quantization: "fp32".to_string(),
            },
            ModelPackage {
                checksum: "c".repeat(64),
                quantization: "int8".to_string(),
            },
        ],
    }
}

/// The fixture library's index, of schema version 4 when `version_4` asks: this engine's
/// schema without the `chunkKey` field, and version 4's metadata.
fn library(dir: &Path, version_4: bool) -> PathBuf {
    let index = dir.join("index");
    std::fs::create_dir_all(&index).unwrap();
    if version_4 {
        let probe = TempDir::new().unwrap();
        drop(SearchEngine::new(probe.path().to_str().unwrap()));
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(probe.path().join("meta.json")).unwrap())
                .unwrap();
        let mut fields = meta["schema"].as_array().unwrap().clone();
        fields.retain(|field| field["name"] != "chunkKey");
        let schema: tantivy::schema::Schema =
            serde_json::from_value(serde_json::Value::Array(fields)).unwrap();
        tantivy::Index::create_in_dir(&index, schema).unwrap();
        std::fs::write(
            index.join("otzaria_index_meta.json"),
            serde_json::json!({
                "format": "otzaria-search-index",
                "schema_version": 4,
                "engine_version": "0.8.7",
                "tantivy_version": "0.26.2",
                "created_at_unix_seconds": 0
            })
            .to_string(),
        )
        .unwrap();
    }
    let mut engine = SearchEngine::new(index.to_str().unwrap());
    for (title, path, order, text) in books() {
        engine
            .add_text_book(
                title.to_string(),
                "/test".to_string(),
                path.to_string(),
                order,
                0,
                text,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    index
}

/// `export_semantic_plan` over `index` into `out`, as a build machine runs it.
fn run_export(index: &Path, out: &Path, model_file: &Path) -> std::process::Output {
    let library_version = LIBRARY_VERSION.to_string();
    Command::new(env!("CARGO_BIN_EXE_export_semantic_plan"))
        .args([
            "--index",
            index.to_str().unwrap(),
            "--library-version",
            library_version.as_str(),
            "--release-tag",
            RELEASE_TAG,
            "--model",
            model_file.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--created-at",
            CREATED_AT,
        ])
        .output()
        .expect("the binary runs")
}

fn bytes(dir: &Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

/// The plan opens as the sidecar opens one, and is what its own planner writes over the same
/// index; a version 4 index plans to the same files, keyed from its text.
#[test]
fn the_plan_is_the_sidecars_own_from_either_key_source() {
    let dir = TempDir::new().unwrap();
    let model_file = dir.path().join("model.json");
    std::fs::write(&model_file, serde_json::to_vec(&model()).unwrap()).unwrap();
    let mut planned = Vec::new();
    for version_4 in [false, true] {
        let root = dir.path().join(if version_4 { "v4" } else { "v5" });
        let index = library(&root, version_4);
        let out = root.join("plan");
        let output = run_export(&index, &out, &model_file);
        assert!(
            output.status.success(),
            "the export failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let plan = Plan::open(&out).expect("the sidecar reads the plan");
        let manifest = &plan.manifest;
        assert_eq!(manifest.library_version, LIBRARY_VERSION);
        assert_eq!(manifest.library_release_tag, RELEASE_TAG);
        assert_eq!(
            plan.books.names(),
            ["/books/a.txt", "/books/b.txt", "/books/c.txt"]
        );
        let documents = 6 + 4 + 1;
        assert_eq!(
            manifest.parity.checked,
            if version_4 { 0 } else { documents },
            "a version 5 index is held to its column, line by line; version 4: {version_4}"
        );
        assert_eq!(manifest.parity.mismatches, 0);
        let counts = manifest.counts;
        assert_eq!(counts.records, plan.records.len());
        assert!(
            counts.unique < counts.records,
            "the repeated line is one key: {counts:?}"
        );
        assert_eq!(counts.to_embed, counts.unique);
        assert_eq!(counts.tombstones, 0);
        let keys: HashSet<[u8; 16]> = plan
            .records
            .iter()
            .map(|record| record.key_bytes())
            .collect();
        assert_eq!(keys.len() as u64, counts.unique);

        // The sidecar's own planner, over the same index read as its corpus.
        let corpus = TantivyCorpus::from_index_path(
            &index,
            LIBRARY_VERSION,
            RELEASE_TAG,
            production_chunking(),
        )
        .unwrap();
        let reference = root.join("reference");
        let expected = plan_from_corpus(
            &corpus,
            PlanRequest {
                out_dir: reference.clone(),
                model: model(),
                chunking: production_chunking(),
                passage_package: model().query_packages[0].clone(),
                previous: None,
                warehouse: None,
                created_at: CREATED_AT.to_string(),
            },
        )
        .unwrap();
        for name in [
            RECORDS_FILE,
            BOOKS_FILE,
            EMBED_PLAN_FILE,
            EMBED_MANIFEST_FILE,
            TOMBSTONES_FILE,
        ] {
            assert_eq!(
                bytes(&out, name),
                bytes(&reference, name),
                "{name}, version 4: {version_4}"
            );
        }
        assert_eq!(expected.counts, counts);
        assert_eq!(expected.identity, manifest.identity);
        planned.push(out);
    }
    for name in [RECORDS_FILE, BOOKS_FILE, EMBED_PLAN_FILE] {
        assert_eq!(
            bytes(&planned[0], name),
            bytes(&planned[1], name),
            "{name}: the column and the text plan alike"
        );
    }
}

/// What a warehouse holds stays out of `embed.jsonl`.
struct Holds(HashSet<[u8; 32]>);

impl HeldVectors for Holds {
    fn holds(&self, sha256: &[u8; 32]) -> bool {
        self.0.contains(sha256)
    }
}

/// A text the warehouse has a vector for is not embedded again; a column that does not hold
/// its line's key fails the parity gate, and no reader accepts the plan.
#[test]
fn a_warehouse_vector_is_not_embedded_and_a_column_off_its_text_fails_the_gate() {
    let dir = TempDir::new().unwrap();
    let index = library(dir.path(), false);
    fn request<'a>(
        index: &Path,
        out: PathBuf,
        warehouse: Option<&'a dyn HeldVectors>,
    ) -> PlanExport<'a> {
        PlanExport {
            index_path: index.to_path_buf(),
            out_dir: out,
            library_version: LIBRARY_VERSION,
            library_release_tag: RELEASE_TAG.to_string(),
            model: model(),
            passage_package: model().query_packages[0].clone(),
            previous: None,
            warehouse,
            created_at: CREATED_AT.to_string(),
        }
    }
    let full = export_plan(request(&index, dir.path().join("full"), None)).unwrap();
    let held = Plan::open(&dir.path().join("full"))
        .unwrap()
        .records
        .get(0)
        .sha256;
    let warehouse = Holds(HashSet::from([held]));
    let report = export_plan(request(&index, dir.path().join("rest"), Some(&warehouse))).unwrap();
    let counts = report.manifest.counts;
    assert_eq!(counts.to_embed, full.manifest.counts.to_embed - 1);
    assert_eq!(counts.revived, 1);
    assert_eq!(counts.records, full.manifest.counts.records);
    assert_eq!(report.embed.records, counts.to_embed);

    // A line added past `add_text_book` keeps a `chunkKey` of 0, whatever its text.
    let mut engine = SearchEngine::new(index.to_str().unwrap());
    engine
        .add_document(
            (4u64 << 32) + 1,
            "Book D",
            "Book D 1",
            "/test",
            "A line added one document at a time, long enough to embed.",
            0,
            false,
            "/books/d.txt",
            Some(0),
            Some(0),
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    drop(engine);
    let model_file = dir.path().join("model.json");
    std::fs::write(&model_file, serde_json::to_vec(&model()).unwrap()).unwrap();
    let out = dir.path().join("forged");
    let output = run_export(&index, &out, &model_file);
    assert!(
        !output.status.success(),
        "the parity gate must fail the export"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("parity gate"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: PlanManifest =
        serde_json::from_slice(&bytes(&out, "plan-manifest.json")).unwrap();
    assert_eq!(manifest.parity.mismatches, 1);
    assert!(
        Plan::open(&out).is_err(),
        "no reader accepts a plan that failed its gate"
    );
}
