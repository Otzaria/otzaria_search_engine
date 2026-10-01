//! The application's semantic path, end to end through the public API: a lexical index, an
//! artifact built from it by the build binary with `--stamp-index`, and that artifact opened
//! with `SearchEngine::open_semantic_artifact`, searched, and hydrated from the index.
//!
//! Everything here is what a device does and nothing it does not: it never embeds a line.
//! The build binary stands in for the build machine, and the identities the device supplies
//! are the files the build used: the model's identity JSON, and the corpus stamp the build
//! wrote into the index directory.
//!
//! Gated like `tests/build_semantic_artifact.rs`, and for its reasons: the model is the stub
//! ONNX package, which only the deterministic stand-in serves, and `semantic-onnx` would take
//! it ahead of the stand-in and fail to load it. The stand-in embeds a text as a hash of it,
//! so a query that is a line's exact text scores that line 1.0 and nothing else as high,
//! which is what lets these tests know which line must come back.

#![cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]

use otzaria_semantic_search::semantic::chunker::ChunkerConfig;
use otzaria_semantic_search::semantic::embedding::mock;
use otzaria_semantic_search::semantic::model_package::validate_onnx_package;
use otzaria_semantic_search::semantic::versioning::ModelIdentity;
use otzaria_semantic_search::semantic::zevc_store::VECTORS_FILENAME;
use search_engine::api::search_engine::{
    SearchEngine, SemanticArtifactInput, SemanticBookInput, SemanticBookLineInput,
    SemanticCancellationToken, SemanticConfigInput, SemanticError, SemanticErrorKind,
    SemanticExecutedMode, SemanticLexicalMode, SemanticResultSource, SemanticRetrievalMode,
    SemanticSearchResponse, SemanticState,
};
use search_engine::semantic_corpus::CORPUS_STAMP_FILE_NAME;
use serde_json::Value as JsonValue;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const GENESIS: &str = "/books/genesis.txt";
const BERACHOT: &str = "/books/berachot.txt";
const LIBRARY_VERSION: &str = "otzaria-library-2026-10";

/// The third line is under `min_embeddable_chars`, so the recipe skips it: four vectors for
/// five lines.
const GENESIS_TEXT: &str = "בראשית ברא אלהים את השמים ואת הארץ\n\
                            והארץ היתה תהו ובהו וחשך על פני תהום רבה\n\
                            או\n\
                            ויאמר אלהים יהי אור ויהי אור";
const BERACHOT_TEXT: &str = "מאימתי קורין את שמע בערבית משעה שהכהנים נכנסין לאכול בתרומתן";
const EMBEDDED: u32 = 4;

/// A line whose exact text a query repeats, so the stand-in must rank it first.
const PROBE_LINE: &str = "ויאמר אלהים יהי אור ויהי אור";

/// What the build machine produced, and where.
struct Library {
    _root: TempDir,
    index: PathBuf,
    artifact: PathBuf,
    model_file: PathBuf,
    model: ModelIdentity,
    /// The digest the build binary reported, which a release would publish.
    digest: String,
}

impl Library {
    /// The model identity the build used, as the application would ship it.
    fn model_json(&self) -> String {
        serde_json::to_string_pretty(&self.model).unwrap()
    }

    fn input(&self) -> SemanticArtifactInput {
        self.input_with_identity(self.model_json())
    }

    fn input_with_identity(&self, model_identity_json: String) -> SemanticArtifactInput {
        SemanticArtifactInput {
            artifact_dir: self.artifact.to_string_lossy().into_owned(),
            model_path: self.model_file.to_string_lossy().into_owned(),
            model_identity_json,
            published_digest: None,
            onnx_runtime_path: None,
        }
    }

    /// The index as the application opens it.
    fn engine(&self) -> SearchEngine {
        SearchEngine::new(self.index.to_str().unwrap())
    }

    fn stamp_path(&self) -> PathBuf {
        self.index.join(CORPUS_STAMP_FILE_NAME)
    }
}

fn add_books(engine: &mut SearchEngine) {
    engine
        .add_text_book(
            "בראשית".to_string(),
            "/מקרא/תורה".to_string(),
            GENESIS.to_string(),
            0,
            0,
            GENESIS_TEXT.to_string(),
            None,
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

/// Change what the index holds: a third book, committed.
fn add_another_book(engine: &mut SearchEngine) {
    engine
        .add_text_book(
            "ספר נוסף".to_string(),
            "/אחר".to_string(),
            "/books/another.txt".to_string(),
            2,
            0,
            "שורה שלא הייתה בספרייה כשהארטיפקט נבנה ממנה".to_string(),
            None,
        )
        .unwrap();
    engine.commit().unwrap();
}

/// Build the library the way a release does: the lexical index, closed, then the artifact
/// from it — stamping the index when `stamp` asks for it.
fn build_library(stamp: bool) -> Library {
    let root = TempDir::new().unwrap();
    let index = root.path().join("tantivy");
    let work = root.path().join("work");
    std::fs::create_dir_all(&index).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    add_books(&mut SearchEngine::new(index.to_str().unwrap()));

    let model_file = mock::write_stub_onnx_package(&work.join("model"));
    let chunking = ChunkerConfig::default();
    let model = ModelIdentity {
        model_id: "test-mock".to_string(),
        model_checksum: validate_onnx_package(&model_file)
            .unwrap()
            .checksum()
            .to_string(),
        model_quantization: "int8".to_string(),
        embedding_backend: "mock-hash-v1".to_string(),
        embedding_dim: 64,
        pooling: "in-graph".to_string(),
        max_tokens: 512,
        embedding_text_version: chunking.embedding_text_version,
        normalization_version: chunking.normalization_version,
        chunking_identity: chunking.identity(),
    };
    std::fs::write(
        work.join("model.json"),
        serde_json::to_vec_pretty(&model).unwrap(),
    )
    .unwrap();
    std::fs::write(
        work.join("chunking.json"),
        serde_json::to_vec_pretty(&chunking).unwrap(),
    )
    .unwrap();

    let artifact = root.path().join("artifact");
    let mut command = Command::new(env!("CARGO_BIN_EXE_build_semantic_artifact"));
    command.args([
        "--index",
        index.to_str().unwrap(),
        "--library-version",
        LIBRARY_VERSION,
        "--model",
        work.join("model.json").to_str().unwrap(),
        "--model-file",
        model_file.to_str().unwrap(),
        "--chunking",
        work.join("chunking.json").to_str().unwrap(),
        "--out",
        artifact.to_str().unwrap(),
        "--created-at",
        "2026-10-01T00:00:00Z",
        "--allow-non-semantic",
    ]);
    if stamp {
        command.arg("--stamp-index");
    }
    let built = command.output().expect("the build binary runs");
    let stdout = String::from_utf8_lossy(&built.stdout).into_owned();
    assert!(
        built.status.success(),
        "build failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let digest = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Digest:"))
        .expect("the binary reports a digest")
        .trim()
        .to_string();

    Library {
        _root: root,
        index,
        artifact,
        model_file,
        model,
        digest,
    }
}

fn open_refusal(engine: &SearchEngine, input: SemanticArtifactInput) -> SemanticError {
    match engine.open_semantic_artifact(input) {
        Ok(status) => panic!(
            "the artifact must be refused, and opened: {:?}",
            status.model_id
        ),
        Err(error) => error,
    }
}

/// The refusal's kind and field, which an application branches on, and nothing left open:
/// every refusal leaves the engine as it was.
fn assert_refused(
    engine: &SearchEngine,
    error: &SemanticError,
    kind: SemanticErrorKind,
    field: Option<&str>,
) {
    assert_eq!(error.kind, kind, "{}", error.message);
    assert_eq!(error.field.as_deref(), field, "{}", error.message);
    assert!(
        !engine.semantic_status().enabled,
        "a refusal leaves no session open"
    );
}

fn search(
    engine: &SearchEngine,
    query: &str,
    mode: SemanticRetrievalMode,
) -> SemanticSearchResponse {
    engine
        .search_semantic(
            query.to_string(),
            Vec::new(),
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            mode,
            None,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap()
}

/// The whole path: stamped, opened, reported, searched both ways, hydrated.
#[test]
fn an_artifact_built_from_the_index_opens_and_answers_with_hydrated_lines() {
    let library = build_library(true);
    let engine = library.engine();

    let status = engine
        .open_semantic_artifact(library.input())
        .expect("the artifact built from this index opens against it");
    assert!(
        status.enabled && status.available,
        "{:?}",
        status.last_error
    );
    assert_eq!(status.embedding_backend.as_deref(), Some("mock-hash-v1"));
    assert_eq!(status.vector_count, EMBEDDED);
    assert_eq!(status.indexed_book_count, 2);
    assert_eq!(status.needs_full_reindex, None);
    assert!(
        status.vectors_persisted,
        "an artifact's vectors are on disk"
    );
    assert_eq!(status.state, SemanticState::Ready);
    assert_eq!((status.last_error, status.error_kind), (None, None));

    // Semantic-only: the one line the query repeats comes back first, hydrated from this
    // index and not from the artifact.
    let response = search(&engine, PROBE_LINE, SemanticRetrievalMode::SemanticOnly);
    assert_eq!(response.executed_mode, SemanticExecutedMode::SemanticOnly);
    assert!(
        response.semantic_available,
        "{:?}",
        response.fallback_reason
    );
    assert_eq!(response.fallback_kind, None);
    let top = response.results.first().expect("a semantic hit");
    assert_eq!(top.source, SemanticResultSource::Semantic);
    assert!(!top.needs_hydration);
    assert_eq!(top.snippet_html, PROBE_LINE);
    assert_eq!(top.file_path, GENESIS);
    let stored = engine
        .get_document_by_id(top.id)
        .unwrap()
        .expect("the hit's id names a line of this index");
    assert_eq!(stored.text, PROBE_LINE);

    // Hybrid: both halves reach fusion, and the line both found is marked as such.
    let response = search(&engine, PROBE_LINE, SemanticRetrievalMode::Hybrid);
    assert_eq!(response.executed_mode, SemanticExecutedMode::Hybrid);
    assert!(
        response.fallback_reason.is_none(),
        "{:?}",
        response.fallback_reason
    );
    assert_eq!(response.fallback_kind, None);
    let top = response.results.first().expect("a fused hit");
    assert_eq!(top.id, stored.id);
    assert_eq!(top.source, SemanticResultSource::Both);

    // Opening again with the same inputs changes nothing.
    let again = engine.open_semantic_artifact(library.input()).unwrap();
    assert_eq!(again.vector_count, EMBEDDED);
}

/// `--stamp-index` writes the stamp and nothing else into the index, and the stamp names the
/// corpus the artifact was built for.
#[test]
fn the_stamp_is_the_one_file_the_build_writes_into_the_index() {
    let unstamped = build_library(false);
    let stamped = build_library(true);

    // Segment files are named by a fresh id in every index, so only the bookkeeping files
    // (Tantivy's meta and locks, this crate's metadata, the stamp) can be compared by name.
    let bookkeeping = |dir: &Path| -> BTreeSet<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".json") || name.starts_with('.'))
            .collect()
    };
    assert!(!unstamped.stamp_path().exists());
    let extra: Vec<String> = bookkeeping(&stamped.index)
        .difference(&bookkeeping(&unstamped.index))
        .cloned()
        .collect();
    assert_eq!(extra, vec![CORPUS_STAMP_FILE_NAME.to_string()]);

    let stamp: JsonValue =
        serde_json::from_str(&std::fs::read_to_string(stamped.stamp_path()).unwrap()).unwrap();
    let manifest: JsonValue = serde_json::from_str(
        &std::fs::read_to_string(stamped.artifact.join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stamp["corpus"], manifest["identity"]["corpus"]);
    assert_eq!(stamp["corpus"]["library_version"], LIBRARY_VERSION);
}

#[test]
fn an_index_without_a_corpus_stamp_is_refused_and_says_how_to_make_one() {
    let library = build_library(false);
    let engine = library.engine();
    let error = open_refusal(&engine, library.input());
    let message = &error.message;
    assert!(
        message.contains(CORPUS_STAMP_FILE_NAME) && message.contains("--stamp-index"),
        "{message}"
    );
    assert_refused(&engine, &error, SemanticErrorKind::IndexNotStamped, None);
}

/// A stamp this build cannot read says nothing about the index, and is no stamp to it.
#[test]
fn a_damaged_corpus_stamp_is_no_stamp() {
    let library = build_library(true);
    std::fs::write(library.stamp_path(), "{ not a stamp").unwrap();

    let engine = library.engine();
    let error = open_refusal(&engine, library.input());
    assert!(
        error.message.contains("is not a corpus stamp"),
        "{}",
        error.message
    );
    assert_refused(&engine, &error, SemanticErrorKind::IndexNotStamped, None);
}

/// A stamp that names another corpus — another release of the library — is refused by the
/// identity comparison, which names the field.
#[test]
fn a_stamp_for_another_corpus_is_refused_by_name() {
    let library = build_library(true);
    let mut stamp: JsonValue =
        serde_json::from_str(&std::fs::read_to_string(library.stamp_path()).unwrap()).unwrap();
    stamp["corpus"]["library_version"] = JsonValue::from("otzaria-library-1999-01");
    std::fs::write(library.stamp_path(), stamp.to_string()).unwrap();

    let engine = library.engine();
    let error = open_refusal(&engine, library.input());
    assert!(
        error.message.contains("corpus.library_version"),
        "{}",
        error.message
    );
    assert_refused(
        &engine,
        &error,
        SemanticErrorKind::ArtifactIncompatible,
        Some("corpus.library_version"),
    );
}

/// The index this artifact was built for, changed afterwards: the stamp no longer vouches
/// for it, since nothing on the device can recompute `corpus_id`.
#[test]
fn an_index_changed_after_it_was_stamped_is_refused() {
    let library = build_library(true);
    let mut engine = library.engine();
    add_another_book(&mut engine);

    let error = open_refusal(&engine, library.input());
    assert!(
        error
            .message
            .contains("has changed since its corpus stamp was written"),
        "{}",
        error.message
    );
    assert_refused(&engine, &error, SemanticErrorKind::IndexStampMismatch, None);
}

/// A model-identity field, and one edit of it alone.
type IdentityEdit = (&'static str, fn(&mut ModelIdentity));

/// Fields the artifact records, and the checksum the sidecar reads off the loaded model
/// instead: a disagreement in either kind is refused by name, and leaves nothing open.
#[test]
fn a_model_identity_other_than_the_artifacts_is_refused_by_name() {
    let library = build_library(true);
    let engine = library.engine();

    let edits: [IdentityEdit; 3] = [
        ("model.model_id", |m| {
            m.model_id = "another-model".to_string()
        }),
        ("model.model_quantization", |m| {
            m.model_quantization = "fp32".to_string()
        }),
        ("model.chunking_identity", |m| m.chunking_identity ^= 1),
    ];
    for (field, edit) in edits {
        let mut model = library.model.clone();
        edit(&mut model);
        let error = open_refusal(
            &engine,
            library.input_with_identity(serde_json::to_string(&model).unwrap()),
        );
        assert!(error.message.contains(field), "{field}: {}", error.message);
        // An artifact built for another model; `field` is the path the message names.
        assert_refused(
            &engine,
            &error,
            SemanticErrorKind::ArtifactIncompatible,
            Some(field),
        );
    }

    // Not compared with the artifact by the sidecar, which takes them from the loaded
    // model: an identity file that disagrees with the model describes other weights, and
    // `field` is the identity file's own key.
    let edits: [IdentityEdit; 2] = [
        ("model_checksum", |m| m.model_checksum = "0".repeat(64)),
        ("embedding_backend", |m| {
            m.embedding_backend = "onnxruntime-sentence-v1".to_string()
        }),
    ];
    for (field, edit) in edits {
        let mut model = library.model.clone();
        edit(&mut model);
        let error = open_refusal(
            &engine,
            library.input_with_identity(serde_json::to_string(&model).unwrap()),
        );
        let message = &error.message;
        assert!(
            message.contains(field) && message.contains("describes other weights"),
            "{message}"
        );
        assert_refused(
            &engine,
            &error,
            SemanticErrorKind::ModelIdentityMismatch,
            Some(field),
        );
    }

    let error = open_refusal(&engine, library.input_with_identity("{".to_string()));
    assert!(
        error.message.contains("not a model identity"),
        "{}",
        error.message
    );
    assert_refused(
        &engine,
        &error,
        SemanticErrorKind::InvalidInput,
        Some("model_identity_json"),
    );

    let status = engine
        .open_semantic_artifact(library.input())
        .expect("no refused identity left a session behind");
    assert!(status.available);
}

#[test]
fn a_published_digest_is_checked() {
    let library = build_library(true);
    let engine = library.engine();

    let wrong = SemanticArtifactInput {
        published_digest: Some("0".repeat(64)),
        ..library.input()
    };
    let error = open_refusal(&engine, wrong);
    assert!(
        error.message.contains("was published for it"),
        "{}",
        error.message
    );
    assert_refused(
        &engine,
        &error,
        SemanticErrorKind::ArtifactNotPublished,
        None,
    );

    let right = SemanticArtifactInput {
        published_digest: Some(library.digest.clone()),
        ..library.input()
    };
    assert!(engine.open_semantic_artifact(right).unwrap().available);
}

/// The artifact is read-only on the device: every call that would build vectors is refused
/// by name, and the session keeps serving.
#[test]
fn every_build_side_call_on_an_opened_artifact_is_refused_as_read_only() {
    let library = build_library(true);
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();

    let book = SemanticBookInput {
        source_book_key: GENESIS.to_string(),
        title: "בראשית".to_string(),
        content_fingerprint: 1,
        is_pdf: false,
        topics: "/מקרא/תורה".to_string(),
        extra_facets: Vec::new(),
        lines: vec![SemanticBookLineInput {
            line_id: 1,
            section_id: 1,
            text: PROBE_LINE.to_string(),
            line_hash: 1,
            reference: "בראשית א".to_string(),
            segment: 0,
        }],
    };
    let refusals = [
        (
            "semantic_index_books",
            engine.semantic_index_books(vec![book]).err(),
        ),
        (
            "remove_semantic_books",
            engine
                .remove_semantic_books(vec![GENESIS.to_string()])
                .err(),
        ),
        ("reset_semantic_index", engine.reset_semantic_index().err()),
        ("semantic_index_diff", engine.semantic_index_diff().err()),
    ];
    for (call, refusal) in refusals {
        let error = refusal.unwrap_or_else(|| panic!("{call} succeeded"));
        assert!(
            error.message.contains("read-only"),
            "{call}: {}",
            error.message
        );
        assert_eq!(error.kind, SemanticErrorKind::ReadOnlySession, "{call}");
        assert_eq!(error.field, None, "{call}");
    }

    let error = match engine.configure_semantic(SemanticConfigInput {
        root_dir: library
            .index
            .join("semantic")
            .to_string_lossy()
            .into_owned(),
        model_path: library.model_file.to_string_lossy().into_owned(),
        model_id: "test-mock".to_string(),
        embedding_dim: 64,
        pooling: "in-graph".to_string(),
        max_tokens: 512,
        model_quantization: "int8".to_string(),
        embedding_text_version: 1,
        onnx_runtime_path: None,
    }) {
        Ok(_) => panic!("configure_semantic must not replace an opened artifact"),
        Err(error) => error,
    };
    assert!(
        error.message.contains("prebuilt semantic artifact is open"),
        "{}",
        error.message
    );
    assert_eq!(error.kind, SemanticErrorKind::SessionConflict);

    let status = engine.semantic_status();
    assert!(status.available, "the refusals left the session serving");
    assert_eq!(status.state, SemanticState::Ready);
}

/// A session opened by `configure_semantic` is not silently replaced by an artifact.
#[test]
fn an_artifact_does_not_replace_a_session_built_on_the_device() {
    let library = build_library(true);
    let mut engine = library.engine();
    let semantic_root = TempDir::new().unwrap();
    engine
        .configure_semantic(SemanticConfigInput {
            root_dir: semantic_root.path().to_string_lossy().into_owned(),
            model_path: library.model_file.to_string_lossy().into_owned(),
            model_id: "test-mock".to_string(),
            embedding_dim: 64,
            pooling: "in-graph".to_string(),
            max_tokens: 512,
            model_quantization: "int8".to_string(),
            embedding_text_version: 1,
            onnx_runtime_path: None,
        })
        .unwrap();

    let error = open_refusal(&engine, library.input());
    assert!(
        error.message.contains("configure_semantic"),
        "{}",
        error.message
    );
    assert_eq!(error.kind, SemanticErrorKind::SessionConflict);
    assert_eq!(
        engine.semantic_status().state,
        SemanticState::Empty,
        "the session built on the device is still the open one, with nothing indexed"
    );

    engine.disable_semantic();
    assert!(
        engine
            .open_semantic_artifact(library.input())
            .unwrap()
            .available
    );
}

/// The runtime path on the application's own path. No identity field reads it, so the
/// stand-in, which loads nothing, opens the artifact whatever it names. A repeat is compared
/// on it, since the process keeps the first runtime it loads: the same path is a repeat, and
/// none, or another, is refused naming it. An empty one is refused before anything opens.
#[test]
fn a_runtime_path_opens_no_other_artifact_and_is_compared_on_a_repeat() {
    let library = build_library(true);
    let mut engine = library.engine();
    let with_runtime = |path: Option<&Path>| SemanticArtifactInput {
        onnx_runtime_path: path.map(|path| path.to_string_lossy().into_owned()),
        ..library.input()
    };
    let bundled = library
        .index
        .join("Frameworks")
        .join("libonnxruntime.dylib");

    let status = engine
        .open_semantic_artifact(with_runtime(Some(&bundled)))
        .expect("the runtime path is not part of the artifact's identity");
    assert!(status.available, "{:?}", status.last_error);
    let again = engine
        .open_semantic_artifact(with_runtime(Some(&bundled)))
        .expect("the same runtime path is a repeat");
    assert_eq!(again.state, SemanticState::Ready);

    let elsewhere = library.index.join("libonnxruntime.dylib");
    for changed in [None, Some(elsewhere.as_path())] {
        let error = match engine.open_semantic_artifact(with_runtime(changed)) {
            Ok(_) => panic!("{changed:?}: another runtime path must not pass for a repeat"),
            Err(error) => error,
        };
        assert_eq!(
            error.kind,
            SemanticErrorKind::SessionConflict,
            "{changed:?}"
        );
        assert!(
            error.message.contains("and onnx_runtime_path changed"),
            "{}",
            error.message
        );
    }
    assert_eq!(engine.semantic_status().state, SemanticState::Ready);

    engine.disable_semantic();
    let error = open_refusal(
        &engine,
        SemanticArtifactInput {
            onnx_runtime_path: Some(String::new()),
            ..library.input()
        },
    );
    assert_refused(
        &engine,
        &error,
        SemanticErrorKind::InvalidInput,
        Some("onnx_runtime_path"),
    );
}

/// A commit after opening: the artifact may now name lines that moved, so it is not asked,
/// and both the status and every search say why.
#[test]
fn a_commit_after_opening_makes_the_artifact_stale_and_says_so() {
    let library = build_library(true);
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    add_another_book(&mut engine);

    let status = engine.semantic_status();
    assert!(status.enabled && !status.available);
    assert_eq!(status.state, SemanticState::Stale);
    assert_eq!(status.error_kind, Some(SemanticErrorKind::ArtifactStale));
    let reason = status.last_error.expect("the status says why");
    assert!(
        reason.contains("has changed since the semantic artifact"),
        "{reason}"
    );

    let hybrid = search(&engine, PROBE_LINE, SemanticRetrievalMode::Hybrid);
    assert_eq!(hybrid.executed_mode, SemanticExecutedMode::LexicalOnly);
    assert_eq!(hybrid.fallback_reason.as_deref(), Some(reason.as_str()));
    assert_eq!(hybrid.fallback_kind, Some(SemanticErrorKind::ArtifactStale));
    assert!(
        !hybrid.results.is_empty(),
        "lexical results are still served"
    );

    let semantic = search(&engine, PROBE_LINE, SemanticRetrievalMode::SemanticOnly);
    assert!(semantic.results.is_empty());
    assert_eq!(semantic.fallback_reason.as_deref(), Some(reason.as_str()));
    assert_eq!(
        semantic.fallback_kind,
        Some(SemanticErrorKind::ArtifactStale)
    );

    engine.disable_semantic();
    assert_eq!(engine.semantic_status().state, SemanticState::NotConfigured);
    let error = open_refusal(&engine, library.input());
    assert!(
        error
            .message
            .contains("has changed since its corpus stamp was written"),
        "a stale index is refused at open too: {}",
        error.message
    );
    assert_refused(&engine, &error, SemanticErrorKind::IndexStampMismatch, None);
}

/// Nothing at the artifact directory, and a directory with nothing installed in it: either
/// way there is no artifact to open, which is not the same as a damaged one.
#[test]
fn a_missing_artifact_is_missing_and_not_corrupt() {
    let library = build_library(true);
    let engine = library.engine();
    let empty = TempDir::new().unwrap();

    for artifact_dir in [library.artifact.join("absent"), empty.path().to_path_buf()] {
        let error = open_refusal(
            &engine,
            SemanticArtifactInput {
                artifact_dir: artifact_dir.to_string_lossy().into_owned(),
                ..library.input()
            },
        );
        assert!(
            error.message.contains("manifest.json"),
            "{}: {}",
            artifact_dir.display(),
            error.message
        );
        assert_refused(&engine, &error, SemanticErrorKind::ArtifactMissing, None);
    }
}

/// One way to damage each layer the open checks: the metadata, a payload's presence, and a
/// payload's bytes. All three are the artifact to download again.
#[test]
fn a_damaged_artifact_is_corrupt() {
    type Damage = (&'static str, fn(&Path));
    let damages: [Damage; 3] = [
        ("garbled manifest", |artifact| {
            std::fs::write(artifact.join("manifest.json"), "{ not json").unwrap()
        }),
        ("missing payload", |artifact| {
            std::fs::remove_file(artifact.join(VECTORS_FILENAME)).unwrap()
        }),
        // Same length, so only the payload's checksums can see it.
        ("flipped payload byte", |artifact| {
            let path = artifact.join(VECTORS_FILENAME);
            let mut bytes = std::fs::read(&path).unwrap();
            bytes[0] ^= 0xff;
            std::fs::write(&path, bytes).unwrap();
        }),
    ];
    for (damage, apply) in damages {
        let library = build_library(true);
        apply(&library.artifact);

        let engine = library.engine();
        let error = open_refusal(&engine, library.input());
        assert_eq!(
            error.kind,
            SemanticErrorKind::ArtifactCorrupt,
            "{damage}: {}",
            error.message
        );
        assert_refused(&engine, &error, SemanticErrorKind::ArtifactCorrupt, None);
    }
}

/// The model this device embeds queries with: absent, a package whose graph is not a
/// model, and a graph without the tokenizer its package needs. Each is a different file to
/// install.
#[test]
fn a_missing_or_unusable_model_is_named_by_kind() {
    let library = build_library(true);
    let engine = library.engine();
    let work = TempDir::new().unwrap();

    let absent = work.path().join("absent").join("model.onnx");
    // The tokenizer is checked first, so the package has one, and only the graph is wrong.
    let garbage = mock::write_stub_onnx_package(&work.path().join("garbage"));
    std::fs::write(&garbage, b"not a model").unwrap();
    // A graph alone: the tokenizer is looked for beside it, and is not there.
    let graph = mock::write_stub_onnx_package(&work.path().join("graph-alone"));
    std::fs::remove_file(graph.with_file_name("tokenizer.json")).unwrap();

    for (model_path, kind) in [
        (&absent, SemanticErrorKind::ModelMissing),
        (&garbage, SemanticErrorKind::ModelInvalid),
        (&graph, SemanticErrorKind::TokenizerMissing),
    ] {
        let error = open_refusal(
            &engine,
            SemanticArtifactInput {
                model_path: model_path.to_string_lossy().into_owned(),
                ..library.input()
            },
        );
        assert!(
            error
                .message
                .contains(&*model_path.parent().unwrap().to_string_lossy()),
            "{kind:?}: {}",
            error.message
        );
        assert_refused(&engine, &error, kind, None);
    }
}

/// A model path that names no ONNX graph, a GGUF among them, is refused by its name before
/// anything is opened: a model to replace, about `model_path`, with the sidecar's message.
#[test]
fn a_model_that_is_not_an_onnx_graph_is_invalid_and_named() {
    let library = build_library(true);
    let engine = library.engine();
    let work = TempDir::new().unwrap();
    let gguf = work.path().join("model.gguf");
    std::fs::write(&gguf, b"GGUF").unwrap();

    let error = open_refusal(
        &engine,
        SemanticArtifactInput {
            model_path: gguf.to_string_lossy().into_owned(),
            ..library.input()
        },
    );
    assert!(
        error.message.contains("GGUF support was removed"),
        "{}",
        error.message
    );
    assert_refused(
        &engine,
        &error,
        SemanticErrorKind::ModelInvalid,
        Some("model_path"),
    );
}

/// Values in the installation's own identity that no build could serve are the caller's
/// input to fix, and the message is the one the sidecar's own refusal of them has.
#[test]
fn an_identity_value_no_build_serves_is_invalid_input() {
    let library = build_library(true);
    let engine = library.engine();

    // The field the refusal names, if it names one, what its message says, and the edit.
    type Refused = (Option<&'static str>, &'static str, fn(&mut ModelIdentity));
    let edits: [Refused; 3] = [
        (
            Some("embedding_text_version"),
            "embedding_text_version is 99",
            |m| m.embedding_text_version = 99,
        ),
        (Some("pooling"), "last_token", |m| {
            m.pooling = "last_token".to_string()
        }),
        (None, "max_tokens is 1", |m| m.max_tokens = 1),
    ];
    for (field, named, edit) in edits {
        let mut model = library.model.clone();
        edit(&mut model);
        let error = open_refusal(
            &engine,
            library.input_with_identity(serde_json::to_string(&model).unwrap()),
        );
        assert!(
            error
                .message
                .starts_with("failed to open the semantic artifact at")
                && error.message.contains(named),
            "{named}: {}",
            error.message
        );
        assert_refused(&engine, &error, SemanticErrorKind::InvalidInput, field);
    }
}

/// A query with nothing to embed fails the semantic half of that one search: the lexical
/// half is served and the session goes on serving.
#[test]
fn a_query_with_nothing_to_embed_falls_back_for_that_query_only() {
    let library = build_library(true);
    let engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();

    let response = search(&engine, " ", SemanticRetrievalMode::SemanticOnly);
    assert!(!response.semantic_available);
    assert!(
        response
            .fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("nothing to embed")),
        "{:?}",
        response.fallback_reason
    );
    assert_eq!(response.fallback_kind, Some(SemanticErrorKind::QueryFailed));

    assert_eq!(engine.semantic_status().state, SemanticState::Ready);
    let next = search(&engine, PROBE_LINE, SemanticRetrievalMode::SemanticOnly);
    assert!(next.semantic_available);
    assert_eq!(next.fallback_kind, None);
}
