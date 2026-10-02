//! The application's semantic path, end to end through the public API: a lexical index, a
//! base package built from it by the build binary and installed into a vector set, and that
//! set opened with `SearchEngine::open_semantic_artifact`.
//!
//! Everything here is what a device does and nothing it does not: it never embeds a line.
//! The build binary stands in for the build machine, and the identity the device supplies
//! is the model's identity file the build used.
//!
//! Gated like `tests/build_semantic_artifact.rs`, and for its reasons: the model is the stub
//! ONNX package, which only the deterministic stand-in serves, and `semantic-onnx` would take
//! it ahead of the stand-in and fail to load it. The stand-in embeds a text as a hash of it,
//! so a query that is a line's exact embedded text scores that line 1.0 and nothing else as
//! high, which is what lets these tests know which line must come back.

#![cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]

use otzaria_semantic_search::semantic::chunker::ChunkerConfig;
use otzaria_semantic_search::semantic::embedding::mock;
use otzaria_semantic_search::semantic::model_package::validate_onnx_package;
use otzaria_semantic_search::semantic::versioning::{ModelIdentity, ModelPackage};
use search_engine::api::search_engine::{
    SearchEngine, SemanticArtifactInput, SemanticBookInput, SemanticBookLineInput,
    SemanticConfigInput, SemanticError, SemanticErrorKind, SemanticState,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const GENESIS: &str = "/books/genesis.txt";
const BERACHOT: &str = "/books/berachot.txt";
/// The library edition the build labels the index with.
const LIBRARY_VERSION: u32 = 30;
const RELEASE_TAG: &str = "v30-20261001000000";

/// The third line is under `min_embeddable_chars`, so the recipe skips it: four vectors for
/// five lines.
const GENESIS_TEXT: &str = "בראשית ברא אלהים את השמים ואת הארץ\n\
                            והארץ היתה תהו ובהו וחשך על פני תהום רבה\n\
                            או\n\
                            ויאמר אלהים יהי אור ויהי אור";
const BERACHOT_TEXT: &str = "מאימתי קורין את שמע בערבית משעה שהכהנים נכנסין לאכול בתרומתן";
const EMBEDDED: u32 = 4;

/// What the build machine produced, and where.
struct Library {
    _root: TempDir,
    index: PathBuf,
    vectors: PathBuf,
    model_file: PathBuf,
    model: ModelIdentity,
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
            vectors_dir: self.vectors.to_string_lossy().into_owned(),
            model_path: self.model_file.to_string_lossy().into_owned(),
            model_identity_json,
            onnx_runtime_path: None,
            scan_threads: None,
        }
    }

    /// The index as the application opens it.
    fn engine(&self) -> SearchEngine {
        SearchEngine::new(self.index.to_str().unwrap())
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

/// Bookkeeping files of a directory: what a tool that writes into it would add.
fn bookkeeping(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json") || name.starts_with('.'))
        .collect()
}

/// Build the library the way a release does — the lexical index, closed, then a base
/// package built from it — and install it as a device installs a release.
fn build_library() -> Library {
    let root = TempDir::new().unwrap();
    let index = root.path().join("tantivy");
    let work = root.path().join("work");
    std::fs::create_dir_all(&index).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    add_books(&mut SearchEngine::new(index.to_str().unwrap()));

    let model_file = mock::write_stub_onnx_package(&work.join("model"));
    let chunking = ChunkerConfig::default();
    let model = ModelIdentity {
        family_id: "test-mock@0000000".to_string(),
        tokenizer_checksum: mock::stub_tokenizer_checksum(),
        embedding_dim: 64,
        pooling: "in-graph".to_string(),
        max_tokens: 512,
        embedding_text_version: chunking.embedding_text_version,
        normalization_version: chunking.normalization_version,
        chunking_identity: chunking.identity(),
        query_packages: vec![ModelPackage {
            checksum: validate_onnx_package(&model_file)
                .unwrap()
                .checksum()
                .to_string(),
            quantization: "int8".to_string(),
        }],
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

    let vectors = root.path().join("vectors");
    let library_version = LIBRARY_VERSION.to_string();
    let index_before = bookkeeping(&index);
    let built = Command::new(env!("CARGO_BIN_EXE_build_semantic_artifact"))
        .args([
            "--index",
            index.to_str().unwrap(),
            "--library-version",
            &library_version,
            "--release-tag",
            RELEASE_TAG,
            "--model",
            work.join("model.json").to_str().unwrap(),
            "--model-file",
            model_file.to_str().unwrap(),
            "--chunking",
            work.join("chunking.json").to_str().unwrap(),
            "--out",
            root.path().join("package").to_str().unwrap(),
            "--install",
            vectors.to_str().unwrap(),
            "--created-at",
            "2026-10-01T00:00:00Z",
            "--allow-non-semantic",
        ])
        .output()
        .expect("the build binary runs");
    assert!(
        built.status.success(),
        "build failed:\n{}\n{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    assert_eq!(
        bookkeeping(&index),
        index_before,
        "a vector set needs nothing written into the lexical index"
    );

    Library {
        _root: root,
        index,
        vectors,
        model_file,
        model,
    }
}

fn open_refusal(engine: &SearchEngine, input: SemanticArtifactInput) -> SemanticError {
    match engine.open_semantic_artifact(input) {
        Ok(status) => panic!(
            "the vector set must be refused, and opened: {:?}",
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

/// The set opens against the index it was built from, and reports itself.
#[test]
fn an_installed_vector_set_opens_and_reports_itself() {
    let library = build_library();
    let engine = library.engine();

    let status = engine
        .open_semantic_artifact(library.input())
        .expect("the installed set opens");
    assert!(
        status.enabled && status.available,
        "{:?}",
        status.last_error
    );
    assert_eq!(status.embedding_backend.as_deref(), Some("mock-hash-v1"));
    assert_eq!(status.vector_count, EMBEDDED);
    assert_eq!(status.indexed_book_count, 2);
    assert_eq!(status.needs_full_reindex, None);
    assert!(status.vectors_persisted, "a set's vectors are on disk");
    assert_eq!(status.state, SemanticState::Ready);
    assert_eq!((status.last_error, status.error_kind), (None, None));
    assert_eq!(status.model_id, library.model.family_id);

    // Opening again with the same inputs changes nothing.
    let again = engine.open_semantic_artifact(library.input()).unwrap();
    assert_eq!(again.vector_count, EMBEDDED);
}

/// A model-identity field, and one edit of it alone.
type IdentityEdit = (&'static str, fn(&mut ModelIdentity));

/// Fields the set records, and the package and tokenizer the model file has instead: a
/// disagreement in either kind is refused by name, and leaves nothing open.
#[test]
fn a_model_identity_other_than_the_sets_is_refused_by_name() {
    let library = build_library();
    let engine = library.engine();

    let edits: [IdentityEdit; 2] = [
        ("model.family_id", |m| {
            m.family_id = "another-model@0000000".to_string()
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
        // A set built for another model; `field` is the path the message names.
        assert_refused(
            &engine,
            &error,
            SemanticErrorKind::ArtifactIncompatible,
            Some(field),
        );
    }

    // Not compared with the set by the sidecar, which takes them from the model file: an
    // identity file that disagrees with the model describes other weights, and `field` is
    // the identity file's own key.
    let edits: [IdentityEdit; 2] = [
        ("query_packages", |m| {
            m.query_packages[0].checksum = "0".repeat(64)
        }),
        ("tokenizer_checksum", |m| {
            m.tokenizer_checksum = "0".repeat(64)
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

/// The set is read-only on the device: every call that would build vectors is refused by
/// name, and the session keeps serving.
#[test]
fn every_build_side_call_on_an_opened_set_is_refused_as_read_only() {
    let library = build_library();
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
            text: "ויאמר אלהים יהי אור ויהי אור".to_string(),
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

    let error = match engine.configure_semantic(dev_config(&library, &library.index)) {
        Ok(_) => panic!("configure_semantic must not replace an opened set"),
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

/// A development session over the stub model, rooted in `dir`.
fn dev_config(library: &Library, dir: &Path) -> SemanticConfigInput {
    SemanticConfigInput {
        root_dir: dir.join("semantic").to_string_lossy().into_owned(),
        model_path: library.model_file.to_string_lossy().into_owned(),
        model_id: "test-mock".to_string(),
        embedding_dim: 64,
        pooling: "in-graph".to_string(),
        max_tokens: 512,
        model_quantization: "int8".to_string(),
        embedding_text_version: 1,
        onnx_runtime_path: None,
    }
}

/// A session opened by `configure_semantic` is not silently replaced by a vector set.
#[test]
fn a_vector_set_does_not_replace_a_session_built_on_the_device() {
    let library = build_library();
    let mut engine = library.engine();
    let semantic_root = TempDir::new().unwrap();
    engine
        .configure_semantic(dev_config(&library, semantic_root.path()))
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

/// The runtime path and the scan's threads on the application's own path. No identity field
/// reads either, so the stand-in, which loads nothing, opens the set whatever they say. A
/// repeat is compared on them: the process keeps the first runtime it loads, and a scan's
/// threads are the session's. The same values are a repeat; others are refused naming them.
/// An empty runtime path and zero threads are refused before anything opens.
#[test]
fn a_runtime_path_and_scan_threads_are_compared_on_a_repeat() {
    let library = build_library();
    let mut engine = library.engine();
    let with = |path: Option<&Path>, threads: Option<u32>| SemanticArtifactInput {
        onnx_runtime_path: path.map(|path| path.to_string_lossy().into_owned()),
        scan_threads: threads,
        ..library.input()
    };
    let bundled = library
        .index
        .join("Frameworks")
        .join("libonnxruntime.dylib");

    let status = engine
        .open_semantic_artifact(with(Some(&bundled), Some(2)))
        .expect("neither the runtime path nor the threads are part of the set's identity");
    assert!(status.available, "{:?}", status.last_error);
    let again = engine
        .open_semantic_artifact(with(Some(&bundled), Some(2)))
        .expect("the same inputs are a repeat");
    assert_eq!(again.state, SemanticState::Ready);

    let elsewhere = library.index.join("libonnxruntime.dylib");
    for (changed, threads, field) in [
        (None, Some(2), "onnx_runtime_path"),
        (Some(elsewhere.as_path()), Some(2), "onnx_runtime_path"),
        (Some(bundled.as_path()), Some(3), "scan_threads"),
        (Some(bundled.as_path()), None, "scan_threads"),
    ] {
        let error = match engine.open_semantic_artifact(with(changed, threads)) {
            Ok(_) => panic!("{changed:?} {threads:?}: other inputs must not pass for a repeat"),
            Err(error) => error,
        };
        assert_eq!(error.kind, SemanticErrorKind::SessionConflict, "{field}");
        assert!(
            error.message.contains(&format!("and {field} changed")),
            "{}",
            error.message
        );
    }
    assert_eq!(engine.semantic_status().state, SemanticState::Ready);

    engine.disable_semantic();
    for (input, field) in [
        (
            SemanticArtifactInput {
                onnx_runtime_path: Some(String::new()),
                ..library.input()
            },
            "onnx_runtime_path",
        ),
        (with(None, Some(0)), "scan_threads"),
    ] {
        let error = open_refusal(&engine, input);
        assert_refused(
            &engine,
            &error,
            SemanticErrorKind::InvalidInput,
            Some(field),
        );
    }
}

/// Nothing at the directory, and a directory with nothing installed in it: either way there
/// is no set to open, which is not the same as a damaged one.
#[test]
fn a_missing_vector_set_is_missing_and_not_corrupt() {
    let library = build_library();
    let engine = library.engine();
    let empty = TempDir::new().unwrap();

    for vectors_dir in [library.vectors.join("absent"), empty.path().to_path_buf()] {
        let error = open_refusal(
            &engine,
            SemanticArtifactInput {
                vectors_dir: vectors_dir.to_string_lossy().into_owned(),
                ..library.input()
            },
        );
        assert_refused(&engine, &error, SemanticErrorKind::ArtifactMissing, None);
    }
}

/// A set whose pointer to its live generation, or whose segment, is damaged: the vectors to
/// download again.
#[test]
fn a_damaged_vector_set_is_corrupt() {
    type Damage = (&'static str, fn(&Path));
    let damages: [Damage; 2] = [
        ("garbled CURRENT and PREVIOUS", |vectors| {
            for pointer in ["CURRENT", "PREVIOUS"] {
                std::fs::write(vectors.join(pointer), "not a pointer").unwrap();
            }
        }),
        // A byte inside the header, which its CRC covers.
        ("flipped segment byte", |vectors| {
            for entry in std::fs::read_dir(vectors.join("segments")).unwrap() {
                let path = entry.unwrap().path();
                let mut bytes = std::fs::read(&path).unwrap();
                bytes[100] ^= 0xff;
                std::fs::write(&path, bytes).unwrap();
            }
        }),
    ];
    for (damage, apply) in damages {
        let library = build_library();
        apply(&library.vectors);

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
    let library = build_library();
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
    let library = build_library();
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
    let library = build_library();
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
                .starts_with("failed to open the semantic vectors at")
                && error.message.contains(named),
            "{named}: {}",
            error.message
        );
        assert_refused(&engine, &error, SemanticErrorKind::InvalidInput, field);
    }
}
