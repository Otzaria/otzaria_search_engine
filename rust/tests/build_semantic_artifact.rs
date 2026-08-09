//! S4b's production path, exercised as a production path.
//!
//! Everything else about the corpus adapter is a unit test holding a `TantivyCorpus` it
//! constructed in-process. This runs the actual build binary against an index that exists on
//! disk, the way a release pipeline would — which is the only way the argument parsing, the
//! read-only open of a directory nothing else has a handle on, and the exit code are
//! covered at all.
//!
//! Compiled only with the deterministic backend: a build is inference, and the real weights
//! are a 396 MB gated download the sidecar's own golden job already fetches.

#![cfg(all(feature = "semantic-mock", not(feature = "semantic-real")))]

use otzaria_semantic_search::distribution::corpus::CorpusIndex;
use otzaria_semantic_search::distribution::packer::validate_artifact;
use otzaria_semantic_search::semantic::chunker::ChunkerConfig;
use otzaria_semantic_search::semantic::embedding::{mock, validate_and_checksum_gguf};
use otzaria_semantic_search::semantic::versioning::ModelIdentity;
use search_engine::api::search_engine::SearchEngine;
use search_engine::semantic_corpus::TantivyCorpus;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

const GENESIS: &str = "/books/genesis.txt";
const BERACHOT: &str = "/books/berachot.txt";
const LIBRARY_VERSION: &str = "otzaria-library-2026-08";

/// The third line is under `min_embeddable_chars`, so the recipe skips it — and an artifact
/// that skips it is complete rather than short. Without a line like it, a build that ignored
/// the recipe entirely would pass.
const GENESIS_TEXT: &str = "בראשית ברא אלהים את השמים ואת הארץ\n\
                            והארץ היתה תהו ובהו וחשך על פני תהום רבה\n\
                            או\n\
                            ויאמר אלהים יהי אור ויהי אור";
const BERACHOT_TEXT: &str = "מאימתי קורין את שמע בערבית משעה שהכהנים נכנסין לאכול בתרומתן";
/// Five lines indexed, four of them embeddable.
const EMBEDDED: u32 = 4;

/// Write a real Tantivy index to disk and close it, so the build opens a directory rather
/// than an object a test is holding open.
fn write_index(dir: &Path) {
    let mut engine = SearchEngine::new(dir.to_str().unwrap());
    engine
        .add_text_book(
            "בראשית".to_string(),
            "/מקרא/תורה".to_string(),
            GENESIS.to_string(),
            0,
            0,
            GENESIS_TEXT.to_string(),
            Some(vec!["/era/תנך".to_string()]),
        )
        .unwrap();
    engine
        .add_text_book(
            "משנה ברכות".to_string(),
            "/משנה/זרעים".to_string(),
            BERACHOT.to_string(),
            1,
            0,
            BERACHOT_TEXT.to_string(),
            None,
        )
        .unwrap();
    engine.commit().unwrap();
}

fn model_identity(checksum: &str, chunking: &ChunkerConfig) -> ModelIdentity {
    ModelIdentity {
        model_id: "EMD123/Otzaria-Embedding-V1-Flash-0.6B".to_string(),
        model_checksum: checksum.to_string(),
        model_quantization: "Q4_K_M".to_string(),
        embedding_backend: "mock-hash-v1".to_string(),
        embedding_dim: 64,
        pooling: "last-token".to_string(),
        max_tokens: 512,
        embedding_text_version: chunking.embedding_text_version,
        normalization_version: chunking.normalization_version,
        chunking_identity: chunking.identity(),
    }
}

/// Index, model and recipe on disk; every path the binary needs.
struct Fixture {
    index: TempDir,
    work: TempDir,
    chunking: ChunkerConfig,
    model: ModelIdentity,
}

fn fixture() -> Fixture {
    let index = TempDir::new().unwrap();
    write_index(index.path());

    let work = TempDir::new().unwrap();
    let chunking = ChunkerConfig::default();
    let model_file = work.path().join("model.gguf");
    mock::write_stub_gguf(&model_file, 3).unwrap();
    let model = model_identity(&validate_and_checksum_gguf(&model_file).unwrap(), &chunking);

    std::fs::write(
        work.path().join("chunking.json"),
        serde_json::to_vec_pretty(&chunking).unwrap(),
    )
    .unwrap();
    std::fs::write(
        work.path().join("model.json"),
        serde_json::to_vec_pretty(&model).unwrap(),
    )
    .unwrap();

    Fixture {
        index,
        work,
        chunking,
        model,
    }
}

fn run(fixture: &Fixture, out: &Path, extra: &[&str]) -> std::process::Output {
    let work = fixture.work.path();
    let mut command = Command::new(env!("CARGO_BIN_EXE_build_semantic_artifact"));
    command.args([
        "--index",
        fixture.index.path().to_str().unwrap(),
        "--library-version",
        LIBRARY_VERSION,
        "--model",
        work.join("model.json").to_str().unwrap(),
        "--model-file",
        work.join("model.gguf").to_str().unwrap(),
        "--chunking",
        work.join("chunking.json").to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--created-at",
        "2026-08-09T00:00:00Z",
        "--batch",
        "2",
    ]);
    command.args(extra);
    command.output().expect("the build binary runs")
}

/// The stage's claim, as a command: an index directory and a model in, a verified artifact
/// out — and what it wrote verifies again against the same index, from a separate process.
#[test]
fn the_build_binary_turns_an_index_and_a_model_into_a_verified_artifact() {
    let fixture = fixture();
    let out = fixture.work.path().join("artifact");

    let built = run(&fixture, &out, &["--allow-non-semantic"]);
    assert!(
        built.status.success(),
        "build failed:\n{}\n{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    let stdout = String::from_utf8_lossy(&built.stdout);
    assert!(
        stdout.contains(&format!("Vectors:       {EMBEDDED}")),
        "the line below min_embeddable_chars must not get a vector:\n{stdout}"
    );

    // Verified independently, against the index the artifact names — a second open of the
    // same directory, in this process, with nothing carried over from the build.
    let engine = SearchEngine::new(fixture.index.path().to_str().unwrap());
    let corpus =
        TantivyCorpus::from_engine(&engine, LIBRARY_VERSION, fixture.chunking.clone()).unwrap();
    let report = validate_artifact(&out, &fixture.model, &corpus).unwrap();
    assert_eq!(report.vector_count, EMBEDDED);
    assert_eq!(report.identity.corpus, corpus.identity().unwrap());

    // The digest the build published is the one a fresh verification arrives at.
    let published = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Digest:"))
        .expect("the binary reports a digest")
        .trim();
    assert_eq!(report.digest, published);
}

/// The stand-in's vectors carry no meaning, and an artifact built from them passes every
/// structural check there is. The refusal has to be the default in the tool a release
/// pipeline runs, not only in the library underneath it.
#[test]
fn the_build_binary_refuses_a_backend_whose_vectors_mean_nothing() {
    let fixture = fixture();
    let out = fixture.work.path().join("refused");

    let built = run(&fixture, &out, &[]);
    assert!(!built.status.success());
    let stderr = String::from_utf8_lossy(&built.stderr);
    assert!(
        stderr.contains("not semantic"),
        "the refusal must say why: {stderr}"
    );
    assert!(!out.exists(), "a refused build writes nothing");
}

/// Every file in a directory, by name, size and modification time.
///
/// Enough to catch a writer lock file appearing, a metadata sidecar being re-stamped, or a
/// segment being rewritten — the three ways going through `SearchEngine` would have touched
/// an index it was only supposed to read.
fn snapshot(dir: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                meta.len(),
                meta.modified().unwrap(),
            )
        })
        .collect();
    entries.sort();
    entries
}

/// **The build reads the index and does not touch it** — on the way through, and on the way
/// out of a failure.
///
/// `SearchEngine::new` is the writer's door: `open_or_create`, an `IndexWriter`, and a
/// metadata file it may stamp. A build that went through it would create an index for a
/// mistyped path, re-stamp a legacy-compatible one, and hold the writer lock for hours.
///
/// **Stated plainly: on an index that already exists and is already current, the writer
/// path happens to leave the files alone, so this test passes either way today.** It is
/// here as the guard for the day that stops being true — the case that discriminates now
/// is `a_path_that_holds_no_index_is_reported_rather_than_populated`.
#[test]
fn the_build_leaves_the_index_byte_for_byte_untouched() {
    let fixture = fixture();
    let index = fixture.index.path();
    let before = snapshot(index);
    assert!(!before.is_empty(), "the fixture wrote a real index");

    let ok = run(
        &fixture,
        &fixture.work.path().join("artifact"),
        &["--allow-non-semantic"],
    );
    assert!(ok.status.success());
    assert_eq!(
        snapshot(index),
        before,
        "a successful build wrote to the index"
    );

    // And on the failing path, where a half-opened index is easiest to leave behind.
    let refused = run(&fixture, &fixture.work.path().join("refused"), &[]);
    assert!(!refused.status.success());
    assert_eq!(
        snapshot(index),
        before,
        "a refused build wrote to the index"
    );
}

/// A path with no index is a fault to report, not an index to create.
///
/// `Index::open_or_create` would leave a brand-new empty index behind and the build would
/// then fail for the wrong reason — "the recipe embeds no line" instead of "there is no
/// index here", with a directory that now looks like one.
#[test]
fn a_path_that_holds_no_index_is_reported_rather_than_populated() {
    let fixture = fixture();
    let empty = TempDir::new().unwrap();

    let work = fixture.work.path();
    let built = Command::new(env!("CARGO_BIN_EXE_build_semantic_artifact"))
        .args([
            "--index",
            empty.path().to_str().unwrap(),
            "--library-version",
            LIBRARY_VERSION,
            "--model",
            work.join("model.json").to_str().unwrap(),
            "--model-file",
            work.join("model.gguf").to_str().unwrap(),
            "--chunking",
            work.join("chunking.json").to_str().unwrap(),
            "--out",
            work.join("nothing").to_str().unwrap(),
            "--allow-non-semantic",
        ])
        .output()
        .expect("the build binary runs");

    assert!(!built.status.success());
    assert_eq!(
        std::fs::read_dir(empty.path()).unwrap().count(),
        0,
        "a missing index must not be created by a tool that only reads"
    );
}
