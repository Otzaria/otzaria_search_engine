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

use otzaria_semantic_search::semantic::embedding::mock;
use otzaria_semantic_search::semantic::model_package::validate_onnx_package;
use otzaria_semantic_search::semantic::versioning::{ModelIdentity, ModelPackage};
use search_engine::api::search_engine::{
    SearchEngine, SemanticArtifactInput, SemanticBookInput, SemanticBookLineInput,
    SemanticCancellationToken, SemanticCompactionPolicy, SemanticConfigInput, SemanticError,
    SemanticErrorKind, SemanticExecutedMode, SemanticGroupingMode, SemanticLexicalMode,
    SemanticResultSource, SemanticRetrievalMode, SemanticSearchResponse, SemanticState,
    SemanticVectorsInstallInput, SemanticVectorsPackageKind,
};
use search_engine::semantic_keys::production_chunking;
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

/// A line whose exact text a query repeats, so the stand-in must rank it first.
const PROBE_LINE: &str = "ויאמר אלהים יהי אור ויהי אור";

/// One book as `add_text_book` takes it: title, topics, key, catalogue order, text.
type Book = (&'static str, &'static str, &'static str, u32, String);

/// The two books every library here starts from.
fn default_books() -> Vec<Book> {
    vec![
        ("בראשית", "/מקרא/תורה", GENESIS, 0, GENESIS_TEXT.to_string()),
        (
            "משנה ברכות",
            "/משנה/זרעים",
            BERACHOT,
            1,
            BERACHOT_TEXT.to_string(),
        ),
    ]
}

/// What the build machine produced, and where.
struct Library {
    _root: TempDir,
    index: PathBuf,
    /// Where the build put its inputs: the model's identity and the chunking.
    work: PathBuf,
    /// The base package of [`LIBRARY_VERSION`], as published.
    package: PathBuf,
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

fn add_books(engine: &mut SearchEngine, books: &[Book]) {
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

/// Replace one book, as the application reindexes a book that changed.
fn replace_book(engine: &mut SearchEngine, book: Book) {
    engine.delete_documents_by_file_path(book.2).unwrap();
    add_books(engine, &[book]);
}

/// An empty index of schema version 4, as the engine before the `chunkKey` column made
/// one: this engine's schema without that field, and version 4's metadata.
fn version_4_index(dir: &Path) {
    let probe = TempDir::new().unwrap();
    drop(SearchEngine::new(probe.path().to_str().unwrap()));
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(probe.path().join("meta.json")).unwrap())
            .unwrap();
    let mut fields = meta["schema"].as_array().unwrap().clone();
    fields.retain(|field| field["name"] != "chunkKey");
    let schema: tantivy::schema::Schema =
        serde_json::from_value(serde_json::Value::Array(fields)).unwrap();
    tantivy::Index::create_in_dir(dir, schema).unwrap();
    std::fs::write(
        dir.join("otzaria_index_meta.json"),
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
    build_library_of(&default_books(), false)
}

/// [`build_library`] of `books`, into an index of schema version 4 when `version_4` asks.
fn build_library_of(books: &[Book], version_4: bool) -> Library {
    let root = TempDir::new().unwrap();
    let index = root.path().join("tantivy");
    let work = root.path().join("work");
    std::fs::create_dir_all(&index).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    if version_4 {
        version_4_index(&index);
    }
    add_books(&mut SearchEngine::new(index.to_str().unwrap()), books);

    let model_file = mock::write_stub_onnx_package(&work.join("model"));
    // The chunking this build keys the index's lines under, as the library's vectors are
    // built with it. The stand-in embeds a bag of words, so a query still ranks first the
    // line whose words it repeats, role prefixes aside.
    let chunking = production_chunking();
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
    let package = root.path().join("package");
    let index_before = bookkeeping(&index);
    build_package(
        &index,
        &work,
        &model_file,
        LIBRARY_VERSION,
        RELEASE_TAG,
        &package,
        Some(&vectors),
    );
    assert_eq!(
        bookkeeping(&index),
        index_before,
        "a vector set needs nothing written into the lexical index"
    );

    Library {
        _root: root,
        index,
        work,
        package,
        vectors,
        model_file,
        model,
    }
}

/// A base package of `index` as library version `library_version`, written to `out`, and
/// installed into `install` by the build binary when given.
fn build_package(
    index: &Path,
    work: &Path,
    model_file: &Path,
    library_version: u32,
    release_tag: &str,
    out: &Path,
    install: Option<&Path>,
) {
    let library_version = library_version.to_string();
    let mut args = vec![
        "--index",
        index.to_str().unwrap(),
        "--library-version",
        library_version.as_str(),
        "--release-tag",
        release_tag,
        "--model",
        work.join("model.json").to_str().unwrap(),
        "--model-file",
        model_file.to_str().unwrap(),
        "--chunking",
        work.join("chunking.json").to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--created-at",
        "2026-10-01T00:00:00Z",
        "--allow-non-semantic",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    if let Some(install) = install {
        args.extend([
            "--install".to_string(),
            install.to_string_lossy().into_owned(),
        ]);
    }
    let built = Command::new(env!("CARGO_BIN_EXE_build_semantic_artifact"))
        .args(&args)
        .output()
        .expect("the build binary runs");
    assert!(
        built.status.success(),
        "build failed:\n{}\n{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
}

fn search(
    engine: &SearchEngine,
    query: &str,
    facets: &[&str],
    mode: SemanticRetrievalMode,
) -> SemanticSearchResponse {
    engine
        .search_semantic(
            query.to_string(),
            facets.iter().map(|facet| facet.to_string()).collect(),
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

/// The semantic-only results of `query`, as (book, line text, ordinal).
fn semantic_lines(
    engine: &SearchEngine,
    query: &str,
    facets: &[&str],
) -> Vec<(String, String, u64)> {
    let response = search(engine, query, facets, SemanticRetrievalMode::SemanticOnly);
    assert!(
        response.semantic_available,
        "{:?}",
        response.fallback_reason
    );
    response
        .results
        .into_iter()
        .map(|result| (result.file_path, result.snippet_html, result.segment))
        .collect()
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

/// The whole path: installed, opened, searched both ways, and every result a line of this
/// index, hydrated from it.
#[test]
fn an_opened_set_answers_with_live_lines() {
    let library = build_library();
    let engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();

    // Semantic-only: the one line the query repeats comes back first, hydrated from this
    // index.
    let response = search(
        &engine,
        PROBE_LINE,
        &[],
        SemanticRetrievalMode::SemanticOnly,
    );
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
    assert_eq!(top.segment, 3);
    let stored = engine
        .get_document_by_id(top.id)
        .unwrap()
        .expect("the hit's id names a line of this index");
    assert_eq!(stored.text, PROBE_LINE);

    // Hybrid: both halves reach fusion, and the line both found is marked as such.
    let response = search(&engine, PROBE_LINE, &[], SemanticRetrievalMode::Hybrid);
    assert_eq!(response.executed_mode, SemanticExecutedMode::Hybrid);
    assert!(
        response.fallback_reason.is_none(),
        "{:?}",
        response.fallback_reason
    );
    let top = response.results.first().expect("a fused hit");
    assert_eq!(top.id, stored.id);
    assert_eq!(top.source, SemanticResultSource::Both);
}

/// Nothing goes stale: a commit after opening leaves the set serving, and its lines are
/// the index's as they are now.
#[test]
fn a_commit_after_opening_leaves_the_set_serving() {
    let library = build_library();
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    add_books(
        &mut engine,
        &[(
            "ספר נוסף",
            "/אחר",
            "/books/another.txt",
            2,
            "שורה שלא הייתה בספרייה כשהווקטורים נבנו ממנה".to_string(),
        )],
    );

    let status = engine.semantic_status();
    assert_eq!(status.state, SemanticState::Ready);
    assert!(status.available);
    assert_eq!(
        semantic_lines(&engine, PROBE_LINE, &[]).first(),
        Some(&(GENESIS.to_string(), PROBE_LINE.to_string(), 3))
    );
}

/// A line pushed down by one inserted above it is found where it moved, by its key.
#[test]
fn a_line_inserted_above_is_found_where_it_moved() {
    for version_4 in [false, true] {
        let library = build_library_of(&default_books(), version_4);
        let mut engine = library.engine();
        engine.open_semantic_artifact(library.input()).unwrap();
        replace_book(
            &mut engine,
            (
                "בראשית",
                "/מקרא/תורה",
                GENESIS,
                0,
                format!("שורה חדשה בראש הספר שלא הייתה בו קודם\n{GENESIS_TEXT}"),
            ),
        );

        assert_eq!(
            semantic_lines(&engine, PROBE_LINE, &[]).first(),
            Some(&(GENESIS.to_string(), PROBE_LINE.to_string(), 4)),
            "schema version 4: {version_4}"
        );
    }
}

/// A text that left its book for another is found in the other one: the column is passed
/// over once for every hit its own books no longer hold.
#[test]
fn a_text_moved_to_another_book_is_found_there() {
    let library = build_library();
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    engine.delete_documents_by_file_path(BERACHOT).unwrap();
    add_books(
        &mut engine,
        &[(
            "משנה ברכות, מהדורה אחרת",
            "/משנה/זרעים",
            "/books/berachot-2.txt",
            5,
            format!("פתיחה למהדורה האחרת ארוכה דיה\n{BERACHOT_TEXT}"),
        )],
    );

    assert_eq!(
        semantic_lines(&engine, BERACHOT_TEXT, &[]).first(),
        Some(&(
            "/books/berachot-2.txt".to_string(),
            BERACHOT_TEXT.to_string(),
            1
        ))
    );
}

/// A line that is gone, and a short line whose neighbour changed, are not shown for the
/// vector of what they were: neither text is in the index any more.
#[test]
fn a_line_that_is_gone_or_whose_context_changed_is_not_shown() {
    // The middle line is short, so its vector is of it and its neighbours.
    let short = "ויהי ערב ויהי";
    let first = "שורה ראשונה ארוכה דיה לעמוד לבדה";
    let third = "שורה שלישית ארוכה דיה לעמוד לבדה";
    let embedded = format!("{first} {short} {third}");
    let book: Book = (
        "ספר קצרות",
        "/בדיקה",
        "/books/short.txt",
        2,
        format!("{first}\n{short}\n{third}"),
    );
    let mut books = default_books();
    books.push(book.clone());
    let library = build_library_of(&books, false);
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    assert_eq!(
        semantic_lines(&engine, &embedded, &[]).first(),
        Some(&("/books/short.txt".to_string(), short.to_string(), 1)),
        "the short line is found by its text and its neighbours'"
    );

    replace_book(
        &mut engine,
        (
            book.0,
            book.1,
            book.2,
            book.3,
            format!("{first}\n{short}\nשורה שלישית אחרת לגמרי מזו שהייתה"),
        ),
    );
    assert!(
        !semantic_lines(&engine, &embedded, &[])
            .iter()
            .any(|(_, text, _)| text == short),
        "the short line's text is now of other neighbours"
    );

    engine.delete_documents_by_file_path(GENESIS).unwrap();
    engine.commit().unwrap();
    assert!(
        !semantic_lines(&engine, PROBE_LINE, &[])
            .iter()
            .any(|(_, text, _)| text == PROBE_LINE),
        "a deleted line is not shown"
    );
}

/// One text in two books is one vector, resolved in both books, each line once.
#[test]
fn a_text_in_two_books_resolves_in_both() {
    let mut books = default_books();
    books.push((
        "בראשית, עותק",
        "/מקרא/תורה",
        "/books/genesis-copy.txt",
        2,
        GENESIS_TEXT.to_string(),
    ));
    let library = build_library_of(&books, false);
    let engine = library.engine();
    let status = engine.open_semantic_artifact(library.input()).unwrap();
    assert_eq!(
        status.vector_count, EMBEDDED,
        "a repeated text is one vector"
    );

    let lines = semantic_lines(&engine, PROBE_LINE, &[]);
    let found: BTreeSet<&str> = lines
        .iter()
        .filter(|(_, text, _)| text == PROBE_LINE)
        .map(|(book, _, _)| book.as_str())
        .collect();
    assert_eq!(found, BTreeSet::from([GENESIS, "/books/genesis-copy.txt"]));
    assert_eq!(
        lines
            .iter()
            .filter(|(_, text, _)| text == PROBE_LINE)
            .count(),
        2,
        "each line once"
    );
}

/// One page of `query` in `mode`, grouped by `grouping`.
fn search_page(
    engine: &SearchEngine,
    query: &str,
    limit: u32,
    offset: u32,
    mode: SemanticRetrievalMode,
    grouping: Option<SemanticGroupingMode>,
) -> SemanticSearchResponse {
    engine
        .search_semantic(
            query.to_string(),
            Vec::new(),
            limit,
            offset,
            SemanticLexicalMode::Exact,
            0,
            mode,
            grouping,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap()
}

/// A passage the book holds in two sections is one vector with one record — a set records a
/// text once per book, at its first line — and each line that holds it is a line of its
/// own: two results without grouping, two groups by section, one group of two by text, and
/// one result on each of two pages of one. The same from the column and, in a version 4
/// index, from the text.
#[test]
fn a_passage_repeated_in_one_book_is_a_result_for_each_occurrence() {
    let books = vec![(
        "בראשית",
        "/מקרא/תורה",
        GENESIS,
        0,
        format!("<h2>פרק א</h2>\n{PROBE_LINE}\n<h2>פרק ב</h2>\n{PROBE_LINE}"),
    )];
    for version_4 in [false, true] {
        let library = build_library_of(&books, version_4);
        let engine = library.engine();
        engine.open_semantic_artifact(library.input()).unwrap();
        let semantic_only = SemanticRetrievalMode::SemanticOnly;
        let occurrences = |response: &SemanticSearchResponse| -> Vec<(u64, u32)> {
            response
                .results
                .iter()
                .filter(|hit| hit.snippet_html == PROBE_LINE)
                .map(|hit| (hit.segment, hit.merged_count))
                .collect()
        };

        let ungrouped = search_page(&engine, PROBE_LINE, 10, 0, semantic_only, None);
        assert_eq!(
            occurrences(&ungrouped),
            vec![(1, 1), (3, 1)],
            "both sections hold the text; version 4: {version_4}"
        );
        assert!(
            ungrouped.fallback_reason.is_none(),
            "{:?}",
            ungrouped.fallback_reason
        );

        // Each section's heading borrows the passage after it, so it is that section's
        // other line.
        let by_section = search_page(
            &engine,
            PROBE_LINE,
            10,
            0,
            semantic_only,
            Some(SemanticGroupingMode::SameSection),
        );
        let sections: Vec<(u64, Vec<u64>)> = by_section
            .results
            .iter()
            .filter(|hit| hit.snippet_html == PROBE_LINE)
            .map(|hit| {
                (
                    hit.segment,
                    hit.merged.iter().map(|sibling| sibling.segment).collect(),
                )
            })
            .collect();
        assert_eq!(sections, vec![(1, vec![0]), (3, vec![2])]);

        let by_text = search_page(
            &engine,
            PROBE_LINE,
            10,
            0,
            semantic_only,
            Some(SemanticGroupingMode::IdenticalText),
        );
        let group = by_text
            .results
            .iter()
            .find(|hit| hit.snippet_html == PROBE_LINE)
            .expect("the repeated text is a group");
        assert_eq!(group.merged_count, 2);
        assert_eq!(group.merged.len(), 1);
        assert_eq!(
            [group.segment, group.merged[0].segment],
            [1, 3],
            "the sibling is the other section's line"
        );

        // Paged one at a time, the same lines in the same order, none twice.
        let paged: Vec<(u64, u64)> = (0..ungrouped.results.len() as u32)
            .map(|offset| {
                let page = search_page(&engine, PROBE_LINE, 1, offset, semantic_only, None);
                assert_eq!(page.results.len(), 1, "offset {offset}");
                (page.results[0].id, page.results[0].segment)
            })
            .collect();
        let whole: Vec<(u64, u64)> = ungrouped
            .results
            .iter()
            .map(|hit| (hit.id, hit.segment))
            .collect();
        assert_eq!(paged, whole);
    }
}

/// A hit's lines are capped, and the cap does not go to one book first: a passage one book
/// holds forty times and another once is one vector with a record in each, and each record
/// is a line of the hit's before any book's other lines of it are: the other book's line,
/// and 31 of the forty — whichever of the two books comes first by name, and in a version 4
/// index as from the column.
#[test]
fn review_d_repeats_in_one_book_crowd_out_another_books_record() {
    let passage = "שורה חוזרת ארוכה דיה לעמוד לבדה בלי הקשר";
    let repeated = vec![passage; 40].join("\n");
    for version_4 in [false, true] {
        for (many, once) in [
            ("/books/a-many.txt", "/books/z-once.txt"),
            ("/books/z-many.txt", "/books/a-once.txt"),
        ] {
            let books = vec![
                ("רבים", "/א", many, 0, repeated.clone()),
                (
                    "יחיד",
                    "/ב",
                    once,
                    1,
                    format!("שורה פותחת בספר היחיד ארוכה דיה\n{passage}"),
                ),
            ];
            let library = build_library_of(&books, version_4);
            let engine = library.engine();
            engine.open_semantic_artifact(library.input()).unwrap();
            let response = search_page(
                &engine,
                passage,
                50,
                0,
                SemanticRetrievalMode::SemanticOnly,
                None,
            );
            let of = |book: &str| -> Vec<u64> {
                response
                    .results
                    .iter()
                    .filter(|hit| hit.file_path == book && hit.snippet_html == passage)
                    .map(|hit| hit.segment)
                    .collect()
            };
            let context = format!("{many} and {once}, version 4: {version_4}");
            assert_eq!(
                of(once),
                [1],
                "the other book's record is a line: {context}"
            );
            assert_eq!(
                of(many),
                (0..31).collect::<Vec<u64>>(),
                "the rest of the cap, in the book's order: {context}"
            );
        }
    }
}

/// The cap is shared alike when a text left the book the set records it in: found by the
/// pass over the whole column, the first line of each book that holds it now is a line of
/// the hit's before any book's second — the other book's line and 31 of the forty, though
/// the forty come first in the index.
#[test]
fn a_moved_passage_one_book_repeats_leaves_the_other_book_its_line() {
    let passage = "שורה חוזרת ארוכה דיה לעמוד לבדה בלי הקשר";
    let opening = "שורה פותחת בספר הישן ארוכה דיה לעמוד לבדה";
    let (old, many, once) = ("/books/old.txt", "/books/many.txt", "/books/once.txt");
    let library = build_library_of(
        &[("ישן", "/א", old, 0, format!("{opening}\n{passage}"))],
        false,
    );
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    // The old book loses the passage; one new book holds it forty times, another once.
    replace_book(&mut engine, ("ישן", "/א", old, 0, opening.to_string()));
    add_books(
        &mut engine,
        &[
            ("רבים", "/ב", many, 1, vec![passage; 40].join("\n")),
            (
                "יחיד",
                "/ג",
                once,
                2,
                format!("שורה פותחת בספר היחיד ארוכה דיה\n{passage}"),
            ),
        ],
    );
    let response = search_page(
        &engine,
        passage,
        50,
        0,
        SemanticRetrievalMode::SemanticOnly,
        None,
    );
    let of = |book: &str| -> Vec<u64> {
        response
            .results
            .iter()
            .filter(|hit| hit.file_path == book && hit.snippet_html == passage)
            .map(|hit| hit.segment)
            .collect()
    };
    assert_eq!(of(once), [1], "the other book's line");
    let lines = of(many);
    assert_eq!(lines.len(), 31, "{lines:?}");
}

/// A line whose `chunkKey` column holds a vector's key and whose text is another is no line
/// of that vector's, whatever card it would be on: not a result, not a grouped sibling,
/// under any grouping and in either mode that searches semantically. The index is edited
/// below the engine, as a writer that kept a column it should have recomputed leaves it.
#[test]
fn a_line_whose_column_is_stale_is_neither_a_result_nor_a_sibling() {
    use tantivy::schema::{Facet, Value};
    use tantivy::{doc, DocAddress, Index, TantivyDocument, Term};

    let library = build_library();
    // Genesis's second line, replaced by a text of no vector's, its columns kept.
    let replaced = "והארץ היתה תהו ובהו וחשך על פני תהום רבה";
    let forged_id = {
        let index = Index::open_in_dir(&library.index).unwrap();
        for name in ["hebrew", "hebrew_vocalized"] {
            index.tokenizers().register(
                name,
                tantivy::tokenizer::TextAnalyzer::from(
                    tantivy::tokenizer::SimpleTokenizer::default(),
                ),
            );
        }
        let searcher = index.reader().unwrap().searcher();
        let schema = index.schema();
        let field = |name: &str| schema.get_field(name).unwrap();
        let address = searcher
            .segment_readers()
            .iter()
            .enumerate()
            .find_map(|(segment, reader)| {
                reader.doc_ids_alive().find_map(|doc| {
                    let address = DocAddress::new(segment as u32, doc);
                    let stored: TantivyDocument = searcher.doc(address).unwrap();
                    (stored
                        .get_first(field("text"))
                        .and_then(|value| value.as_str())
                        == Some(replaced))
                    .then_some(address)
                })
            })
            .expect("the line to replace");
        let columns = searcher.segment_reader(address.segment_ord).fast_fields();
        let column = |name: &str| columns.u64(name).unwrap().first(address.doc_id).unwrap();
        let id = column("id");
        let mut forged = doc!(
            field("title") => "בראשית",
            field("reference") => "",
            field("text") => "שורה זרה שאינה הטקסט שהווקטור נבנה ממנו",
            field("id") => id,
            field("segment") => 1u64,
            field("isPdf") => false,
            field("filePath") => GENESIS,
            field("topics") => Facet::from_text("/מקרא/תורה").unwrap(),
            field("contentHash") => 0u64,
            field("textHash") => 0u64,
            field("sectionId") => column("sectionId"),
            field("generationSort") => 0u64,
            field("lineHash") => column("lineHash"),
        );
        forged.add_u64(field("chunkKey"), column("chunkKey"));
        let mut writer = index.writer(15_000_000).unwrap();
        writer.delete_term(Term::from_field_u64(field("id"), id));
        writer.add_document(forged).unwrap();
        writer.commit().unwrap();
        id
    };

    let engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    for mode in [
        SemanticRetrievalMode::SemanticOnly,
        SemanticRetrievalMode::Hybrid,
    ] {
        for grouping in [
            None,
            Some(SemanticGroupingMode::SameSection),
            Some(SemanticGroupingMode::IdenticalText),
        ] {
            let response = search_page(&engine, PROBE_LINE, 10, 0, mode, grouping);
            let shown: Vec<(u64, Vec<u64>)> = response
                .results
                .iter()
                .map(|hit| {
                    (
                        hit.id,
                        hit.merged.iter().map(|sibling| sibling.id).collect(),
                    )
                })
                .collect();
            assert!(
                shown
                    .iter()
                    .all(|(id, siblings)| *id != forged_id && !siblings.contains(&forged_id)),
                "{mode:?}, {grouping:?}: {shown:?}"
            );
            assert!(
                response
                    .results
                    .iter()
                    .any(|hit| hit.file_path == GENESIS && hit.segment == 3),
                "the query's own line is still found: {mode:?}, {grouping:?}"
            );
            let reason = response.fallback_reason.unwrap_or_default();
            assert!(
                reason.contains("1 semantic match(es) were not shown"),
                "{mode:?}, {grouping:?}: {reason}"
            );
        }
    }
}

/// Two books can share ids — an index updated book by book can give two the same catalogue
/// position, which a release index never has — so a grouped sibling is hydrated as the line
/// of its own book, never as whichever line has its id. Here the sibling is one lexical
/// search alone found, a line added after the vectors were built, to a book reindexed at the
/// catalogue position another book has.
#[test]
fn a_grouped_sibling_is_hydrated_from_its_own_book() {
    let order = 7;
    let other = "/books/other.txt";
    let first = "המילה המיוחדת מופיעה כאן בשורה ארוכה דיה לעמוד לבדה";
    let added = "וגם בשורה הזאת המילה המיוחדת מופיעה בשורה אחרת";
    let books = vec![
        (
            "ספר ראשון",
            "/ראשון",
            "/books/first.txt",
            order,
            "שורה ראשונה בספר הראשון ארוכה דיה לעמוד לבדה\n\
             שורה שנייה בספר הראשון ארוכה דיה גם היא\n\
             שורה שלישית בספר הראשון ארוכה דיה גם היא"
                .to_string(),
        ),
        (
            "ספר אחר",
            "/אחר",
            other,
            order + 1,
            format!("<h2>פרק</h2>\n{first}"),
        ),
    ];
    let library = build_library_of(&books, false);
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    replace_book(
        &mut engine,
        (
            "ספר אחר",
            "/אחר",
            other,
            order,
            format!("<h2>פרק</h2>\n{first}\n{added}"),
        ),
    );

    for mode in [
        SemanticRetrievalMode::Hybrid,
        SemanticRetrievalMode::LexicalOnly,
    ] {
        let response = search_page(
            &engine,
            "המילה המיוחדת",
            10,
            0,
            mode,
            Some(SemanticGroupingMode::SameSection),
        );
        let group = response
            .results
            .iter()
            .find(|hit| hit.file_path == other && hit.merged_count == 2)
            .unwrap_or_else(|| panic!("{mode:?}: the section's two lines are one group"));
        assert_eq!(group.merged.len(), 1, "{mode:?}");
        let sibling = &group.merged[0];
        assert_eq!(
            (sibling.file_path.as_str(), sibling.title.as_str()),
            (other, "ספר אחר"),
            "{mode:?}: a sibling of a section's group is of its book"
        );
        let mut lines = [group.segment, sibling.segment];
        lines.sort_unstable();
        assert_eq!(lines, [1, 2], "{mode:?}: the section's two lines");
    }
}

/// A filter admits books by what the live index says of them: a book moved to another
/// category is found under that one, and not under the one it left.
#[test]
fn a_book_moved_to_another_category_is_filtered_by_its_new_one() {
    let library = build_library();
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    assert!(semantic_lines(&engine, PROBE_LINE, &["/מקרא/תורה"])
        .iter()
        .any(|(book, _, _)| book == GENESIS));

    replace_book(
        &mut engine,
        (
            "בראשית",
            "/אחר/קטגוריה",
            GENESIS,
            0,
            GENESIS_TEXT.to_string(),
        ),
    );
    assert!(!semantic_lines(&engine, PROBE_LINE, &["/מקרא/תורה"])
        .iter()
        .any(|(book, _, _)| book == GENESIS));
    assert_eq!(
        semantic_lines(&engine, PROBE_LINE, &["/אחר/קטגוריה"]).first(),
        Some(&(GENESIS.to_string(), PROBE_LINE.to_string(), 3))
    );
}

/// A text that left its book for one in another category is found under the filter of the
/// book it moved into, although its vector's records name only the book it left: the vector
/// is weighed besides the scan of the admitted book, and resolves in that book alone. Under
/// the filter of the book it left, it is gone. Paged one at a time, the search pages as any.
#[test]
fn a_text_moved_into_another_category_is_found_under_its_filter() {
    let library = build_library();
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    // Genesis loses the probe line, and a book in a category of its own gains it.
    replace_book(
        &mut engine,
        (
            "בראשית",
            "/מקרא/תורה",
            GENESIS,
            0,
            "בראשית ברא אלהים את השמים ואת הארץ".to_owned(),
        ),
    );
    let new_book = "/books/new.txt";
    add_books(
        &mut engine,
        &[(
            "חדש",
            "/חדש",
            new_book,
            2,
            format!("שורה פותחת בספר החדש ארוכה דיה לעמוד\n{PROBE_LINE}"),
        )],
    );

    let unfiltered = semantic_lines(&engine, PROBE_LINE, &[]);
    assert_eq!(
        unfiltered.first(),
        Some(&(new_book.to_string(), PROBE_LINE.to_string(), 1)),
        "{unfiltered:?}"
    );
    let filtered = semantic_lines(&engine, PROBE_LINE, &["/חדש"]);
    assert_eq!(
        filtered.first(),
        Some(&(new_book.to_string(), PROBE_LINE.to_string(), 1)),
        "a live admitted book holds the text: {filtered:?}"
    );
    assert!(
        filtered.iter().all(|(book, _, _)| book == new_book),
        "the vector weighed for the moved text resolves in the admitted book it arrived in, \
         and in no book its records name: {filtered:?}"
    );
    assert!(
        !semantic_lines(&engine, PROBE_LINE, &["/מקרא/תורה"])
            .iter()
            .any(|(_, text, _)| text == PROBE_LINE),
        "the book it left holds it no more"
    );

    let filtered_page = |limit, offset| {
        engine
            .search_semantic(
                PROBE_LINE.to_string(),
                vec!["/חדש".to_string()],
                limit,
                offset,
                SemanticLexicalMode::Exact,
                0,
                SemanticRetrievalMode::SemanticOnly,
                None,
                false,
                false,
                None,
                &SemanticCancellationToken::new(),
            )
            .unwrap()
    };
    let all: Vec<u64> = filtered_page(10, 0)
        .results
        .iter()
        .map(|hit| hit.id)
        .collect();
    let paged: Vec<u64> = (0..all.len() as u32)
        .flat_map(|offset| filtered_page(1, offset).results)
        .map(|hit| hit.id)
        .collect();
    assert_eq!(paged, all);
}

/// A text copied into an admitted book from a big book of another category is weighed at
/// its own score, beside the admitted books' hits, which stay exactly as they were: the
/// filtered scan is not widened by the big book, whose vectors would crowd the small book's
/// lines out of the candidate window. No line of the big book comes back under the filter.
#[test]
fn review_e_widening_crowds_out_the_admitted_books() {
    let word = |book: usize, n: usize| -> String {
        // A distinct "word" per (book, n): letters from a small counter.
        let letters: Vec<char> = "אבגדהוזחטיכלמנסעפצקרשת".chars().collect();
        let mut x = book * 100_000 + n * 7 + 13;
        let mut word = String::new();
        for _ in 0..5 {
            word.push(letters[x % letters.len()]);
            x /= letters.len();
        }
        word
    };
    let query = "אלפא ביתא גימלא דלתא";
    let a_lines: Vec<String> = (0..6)
        .map(|n| {
            format!(
                "{} {} {} {query}",
                word(1, 3 * n),
                word(1, 3 * n + 1),
                word(1, 3 * n + 2)
            )
        })
        .collect();
    let w_lines: Vec<String> = (0..400)
        .map(|n| format!("{query} {} {}", word(2, 2 * n), word(2, 2 * n + 1)))
        .collect();
    let (a, w, copy) = ("/books/a.txt", "/books/w.txt", "/books/n.txt");
    let books = vec![
        ("ספר א", "/א", a, 0, a_lines.join("\n")),
        ("ספר ב", "/ב", w, 1, w_lines.join("\n")),
    ];
    let library = build_library_of(&books, false);
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    // (book, line, semantic score), in the order of the results.
    let results = |engine: &SearchEngine, facets: &[&str]| -> Vec<(String, u64, Option<f32>)> {
        let response = search(engine, query, facets, SemanticRetrievalMode::SemanticOnly);
        assert!(
            response.fallback_reason.is_none(),
            "{:?}",
            response.fallback_reason
        );
        response
            .results
            .into_iter()
            .map(|hit| (hit.file_path, hit.segment, hit.semantic_score))
            .collect()
    };
    let of = |results: &[(String, u64, Option<f32>)], book: &str| -> Vec<(u64, Option<f32>)> {
        results
            .iter()
            .filter(|(of, _, _)| of == book)
            .map(|(_, line, score)| (*line, *score))
            .collect()
    };

    let before = results(&engine, &["/א"]);
    assert_eq!(of(&before, a).len(), 6, "{before:?}");
    // One line of the big book copied into a new book of the admitted category.
    add_books(
        &mut engine,
        &[("ספר חדש", "/א", copy, 2, w_lines[0].clone())],
    );
    let after = results(&engine, &["/א"]);
    assert_eq!(
        of(&after, a),
        of(&before, a),
        "the admitted book's hits are the same, in the same order, at the same scores"
    );
    let copied = of(&after, copy);
    assert_eq!(copied.len(), 1, "the copied line is found: {after:?}");
    assert!(
        after.iter().all(|(book, _, _)| book == a || book == copy),
        "no line of a book the filter does not admit: {after:?}"
    );
    // At the score its vector has: the one the big book's line, which the set records it
    // in, has under the big book's own filter.
    let in_w = engine
        .search_semantic(
            query.to_string(),
            vec!["/ב".to_string()],
            400,
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
    let source = in_w
        .results
        .iter()
        .find(|hit| hit.file_path == w && hit.segment == 0)
        .expect("the big book's line is found under its own filter");
    assert_eq!(copied[0].1, source.semantic_score);
}

/// A text copied into a book of another category, and still in its own, is found under
/// either filter, each in its own book. An unfiltered search finds it where the set records
/// it until the vectors are updated.
#[test]
fn a_text_copied_into_another_category_is_found_under_each_filter() {
    let library = build_library();
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    let copy = "/books/copy.txt";
    add_books(
        &mut engine,
        &[("עותק", "/עותקים", copy, 2, BERACHOT_TEXT.to_string())],
    );

    let copies = semantic_lines(&engine, BERACHOT_TEXT, &["/עותקים"]);
    assert_eq!(
        copies.first(),
        Some(&(copy.to_string(), BERACHOT_TEXT.to_string(), 0))
    );
    assert!(
        copies.iter().all(|(book, _, _)| book == copy),
        "the vector's records name berachot, which still holds the text, and which the \
         filter does not admit: no line of it comes back: {copies:?}"
    );
    assert_eq!(
        semantic_lines(&engine, BERACHOT_TEXT, &["/משנה/זרעים"]).first(),
        Some(&(BERACHOT.to_string(), BERACHOT_TEXT.to_string(), 0))
    );
    assert_eq!(
        semantic_lines(&engine, BERACHOT_TEXT, &[]).first(),
        Some(&(BERACHOT.to_string(), BERACHOT_TEXT.to_string(), 0))
    );
}

/// A text copied into a book of its own category is an arrival of that book whose vector the
/// scan reaches already, through the book the set records it in: it is not weighed again,
/// and both books' lines of it come back under the category's filter.
#[test]
fn a_text_copied_within_its_category_is_found_in_both_books() {
    let library = build_library();
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    let copy = "/books/copy.txt";
    add_books(
        &mut engine,
        &[("עותק", "/משנה/זרעים", copy, 2, BERACHOT_TEXT.to_string())],
    );
    let mut found: Vec<(String, u64)> = semantic_lines(&engine, BERACHOT_TEXT, &["/משנה/זרעים"])
        .into_iter()
        .filter(|(_, text, _)| text == BERACHOT_TEXT)
        .map(|(book, _, line)| (book, line))
        .collect();
    found.sort();
    assert_eq!(found, [(BERACHOT.to_string(), 0), (copy.to_string(), 0)]);
}

/// A text two admitted books hold, each many times over, and the set records in neither: more
/// lines of it than a hit resolves to. Which of them come back does not depend on how a map
/// of books happens to iterate, so it is the same after every commit, each of which plans
/// the filter afresh.
#[test]
fn the_lines_of_a_planned_search_are_the_same_whatever_the_plan_iterates() {
    let library = build_library();
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    let copies = |key: &'static str, order: u32| -> Book {
        (
            "עותקים",
            "/עותקים",
            key,
            order,
            vec![BERACHOT_TEXT; 20].join("\n"),
        )
    };
    add_books(
        &mut engine,
        &[
            copies("/books/copies-b.txt", 7),
            copies("/books/copies-a.txt", 8),
        ],
    );
    let lines = |engine: &SearchEngine| -> Vec<(String, u64)> {
        let response = engine
            .search_semantic(
                BERACHOT_TEXT.to_string(),
                vec!["/עותקים".to_string()],
                50,
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
        response
            .results
            .into_iter()
            .filter(|hit| hit.snippet_html == BERACHOT_TEXT)
            .map(|hit| (hit.file_path, hit.segment))
            .collect()
    };
    let first = lines(&engine);
    assert_eq!(first.len(), 32, "one hit's lines, at most: {first:?}");
    assert!(
        first
            .iter()
            .filter(|(book, _)| book == "/books/copies-a.txt")
            .count()
            == 20,
        "the admitted books in name order: {first:?}"
    );
    for round in 0..6u32 {
        engine
            .add_text_book(
                "ספר נוסף".to_string(),
                "/אחר".to_string(),
                format!("/books/another-{round}.txt"),
                20 + round,
                0,
                format!("שורה נוספת ארוכה דיה לעמוד לבדה מספר {round}"),
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        assert_eq!(lines(&engine), first, "after commit {round}");
    }
}

/// A query with nothing to embed fails the semantic half of that one search: the lexical
/// half is served and the session goes on serving.
#[test]
fn a_query_with_nothing_to_embed_falls_back_for_that_query_only() {
    let library = build_library();
    let engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();

    let response = search(&engine, " ", &[], SemanticRetrievalMode::SemanticOnly);
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
    let next = search(
        &engine,
        PROBE_LINE,
        &[],
        SemanticRetrievalMode::SemanticOnly,
    );
    assert!(next.semantic_available);
    assert_eq!(next.fallback_kind, None);
}

/// The release as the application downloads it: its segment, its manifest, and the
/// manifest's digest as the release publishes it.
fn release(package: &Path) -> (PathBuf, String, String) {
    use sha2::Digest;
    let manifest = std::fs::read_to_string(package.join("release.json")).unwrap();
    let digest = format!("{:x}", sha2::Sha256::digest(manifest.as_bytes()));
    (package.join("segment.oxv"), manifest, digest)
}

impl Library {
    /// What installing `segment` and `manifest_json` into `vectors_dir` takes, for this
    /// model.
    fn install_input(
        &self,
        vectors_dir: &Path,
        segment: &Path,
        manifest_json: &str,
        published_manifest_sha256: Option<String>,
    ) -> SemanticVectorsInstallInput {
        SemanticVectorsInstallInput {
            vectors_dir: vectors_dir.to_string_lossy().into_owned(),
            segment_path: segment.to_string_lossy().into_owned(),
            manifest_json: manifest_json.to_string(),
            published_manifest_sha256,
            model_identity_json: self.model_json(),
        }
    }
}

fn dir_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A release installs through the API as the build binary installs it, and reports itself,
/// opens, serves and verifies.
#[test]
fn a_release_installs_through_the_api_and_reports_itself() {
    let library = build_library();
    let engine = library.engine();
    let token = SemanticCancellationToken::new();
    let vectors = library.package.with_file_name("fresh");
    assert!(
        !engine
            .semantic_vectors_info(dir_string(&vectors))
            .unwrap()
            .present,
        "nothing installed is not an error"
    );

    let (segment, manifest, digest) = release(&library.package);
    let input = || library.install_input(&vectors, &segment, &manifest, Some(digest.clone()));
    let report = engine.install_semantic_vectors(input(), &token).unwrap();
    assert_eq!(report.kind, SemanticVectorsPackageKind::Base);
    assert_eq!(report.library_version, LIBRARY_VERSION);
    assert_eq!(report.segments, 1);
    assert_eq!(report.slots_added, u64::from(EMBEDDED));
    assert!(!report.already_applied && !report.needs_compaction);
    assert!(segment.exists(), "a segment outside incoming/ is copied");

    let info = engine.semantic_vectors_info(dir_string(&vectors)).unwrap();
    assert!(info.present && !info.recovered_from_previous);
    assert_eq!(info.generation, report.generation);
    assert_eq!(
        (info.library_version, info.library_release_tag.as_str()),
        (LIBRARY_VERSION, RELEASE_TAG)
    );
    assert_eq!(info.identity_digest.len(), 64);
    assert_eq!(info.slots_live, u64::from(EMBEDDED));
    assert_eq!(info.bytes_on_disk, report.bytes_on_disk);
    assert_eq!(info.segments.len(), 1);
    assert_eq!(info.segments[0].kind, SemanticVectorsPackageKind::Base);
    assert_eq!(info.segments[0].to_library_version, LIBRARY_VERSION);

    // A base replaces whatever the set holds, itself included.
    let again = engine.install_semantic_vectors(input(), &token).unwrap();
    assert!(!again.already_applied);
    assert!(again.generation > report.generation);
    assert_eq!(again.segments, 1);

    let verified = engine
        .verify_semantic_vectors(dir_string(&vectors), &token)
        .unwrap();
    assert_eq!(
        (verified.generation, verified.segments),
        (again.generation, 1)
    );
    assert!(verified.bytes_checked > 0);

    engine
        .open_semantic_artifact(SemanticArtifactInput {
            vectors_dir: dir_string(&vectors),
            ..library.input()
        })
        .unwrap();
    assert_eq!(
        semantic_lines(&engine, PROBE_LINE, &[]).first(),
        Some(&(GENESIS.to_string(), PROBE_LINE.to_string(), 3))
    );
}

/// A segment published compressed is expanded and installed, and nothing of it is left
/// behind, installed or refused.
#[test]
fn a_compressed_release_is_expanded_and_installed() {
    let library = build_library();
    let engine = library.engine();
    let token = SemanticCancellationToken::new();
    let (segment, manifest, digest) = release(&library.package);
    let compressed = segment.with_extension("oxv.zst");
    let bytes = zstd::stream::encode_all(std::fs::File::open(&segment).unwrap(), 3).unwrap();
    std::fs::write(&compressed, &bytes).unwrap();
    let truncated = segment.with_extension("truncated.oxv.zst");
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
    let incoming = |vectors: &Path| {
        std::fs::read_dir(vectors.join("incoming"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    };

    let vectors = library.package.with_file_name("from-zst");
    let error = match engine.install_semantic_vectors(
        library.install_input(&vectors, &truncated, &manifest, Some(digest.clone())),
        &token,
    ) {
        Ok(_) => panic!("a truncated segment must be refused"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind,
        SemanticErrorKind::ArtifactCorrupt,
        "{}",
        error.message
    );
    assert_eq!(incoming(&vectors), 0);
    assert!(
        !engine
            .semantic_vectors_info(dir_string(&vectors))
            .unwrap()
            .present
    );

    let report = engine
        .install_semantic_vectors(
            library.install_input(&vectors, &compressed, &manifest, Some(digest)),
            &token,
        )
        .unwrap();
    assert_eq!(report.slots_added, u64::from(EMBEDDED));
    assert_eq!(incoming(&vectors), 0);
    assert!(compressed.exists(), "the download is the application's");
    engine
        .verify_semantic_vectors(dir_string(&vectors), &token)
        .unwrap();
}

/// Expansions in progress in the set at `vectors`: `(name, bytes)`.
fn expansions(vectors: &Path) -> Vec<(String, u64)> {
    std::fs::read_dir(vectors.join("incoming"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| {
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        entry.metadata().map_or(0, |meta| meta.len()),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A compressed "segment" that expands to 4 GiB of zeros, from 4,096 copies of one frame,
/// so an install of it is still expanding whenever a test looks.
fn endless_download(path: &Path) {
    let frame = zstd::stream::encode_all(&vec![0u8; 1 << 20][..], 1).unwrap();
    std::fs::write(path, frame.repeat(4096)).unwrap();
}

fn assert_busy(result: Result<impl Sized, SemanticError>, doing: &str) {
    match result {
        Ok(_) => panic!("{doing} must be refused while another runs"),
        Err(error) => {
            // A busy set is no session conflict: what an application does about one —
            // `disableSemantic` — would drop a session that is serving, for an install that
            // only has to wait.
            assert_eq!(
                (error.kind, error.field.as_deref()),
                (SemanticErrorKind::VectorsBusy, Some("vectors_dir")),
                "{doing}: {}",
                error.message
            );
        }
    }
}

/// One install or compaction of a set at a time. While an install is expanding a
/// compressed download, a second install of the same set — a valid release — and a
/// compaction are refused at once as busy, without reading or writing anything; an
/// install into another set is not held up. The first, cancelled, leaves nothing in
/// `incoming/` and the set as it was, and the valid release then installs.
#[test]
fn a_second_install_of_a_set_while_one_expands_is_refused_and_changes_nothing() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let library = build_library();
    let downloads = library.work.join("downloads");
    std::fs::create_dir_all(downloads.join("a")).unwrap();
    std::fs::create_dir_all(downloads.join("b")).unwrap();
    let endless = downloads.join("a/segment.oxv.zst");
    endless_download(&endless);
    let (segment, manifest, digest) = release(&library.package);
    // The same file name as the other download, in another folder.
    let valid = downloads.join("b/segment.oxv.zst");
    std::fs::write(
        &valid,
        zstd::stream::encode_all(std::fs::File::open(&segment).unwrap(), 1).unwrap(),
    )
    .unwrap();
    let engine = Arc::new(library.engine());
    let vectors = library.vectors.clone();
    let before = engine.semantic_vectors_info(dir_string(&vectors)).unwrap();

    let token = Arc::new(SemanticCancellationToken::new());
    let first = {
        let engine = Arc::clone(&engine);
        let token = Arc::clone(&token);
        let input = library.install_input(&vectors, &endless, &manifest, Some(digest.clone()));
        std::thread::spawn(move || engine.install_semantic_vectors(input, &token))
    };
    let started = Instant::now();
    while !expansions(&vectors)
        .iter()
        .any(|(name, bytes)| name.starts_with(".expanding-") && *bytes > 1 << 20)
    {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the first install never started expanding"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    let fresh = SemanticCancellationToken::new();
    assert_busy(
        engine.install_semantic_vectors(
            library.install_input(&vectors, &valid, &manifest, Some(digest.clone())),
            &fresh,
        ),
        "a second install",
    );
    assert_busy(
        engine.compact_semantic_vectors(
            dir_string(&vectors),
            None,
            Some(SemanticCompactionPolicy {
                force: true,
                ..SemanticCompactionPolicy::defaults()
            }),
            &fresh,
        ),
        "a compaction",
    );
    // Another set is another set.
    let elsewhere = library.package.with_file_name("elsewhere");
    engine
        .install_semantic_vectors(
            library.install_input(&elsewhere, &valid, &manifest, Some(digest.clone())),
            &fresh,
        )
        .unwrap();
    // The first install's expansion, its segment and its lock file, and nothing else.
    let mut written: Vec<String> = expansions(&vectors)
        .into_iter()
        .map(|(name, _)| {
            name.rsplit_once('.')
                .map_or(name.clone(), |(_, suffix)| suffix.to_string())
        })
        .collect();
    written.sort();
    assert_eq!(
        written,
        ["lock", "oxv"],
        "the refused install wrote nothing: {:?}",
        expansions(&vectors)
    );

    token.cancel();
    let error = match first.join().unwrap() {
        Ok(_) => panic!("4 GiB of zeros is no segment"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind,
        SemanticErrorKind::Cancelled,
        "{}",
        error.message
    );
    assert_eq!(expansions(&vectors), Vec::new(), "nothing is left behind");
    let after = engine.semantic_vectors_info(dir_string(&vectors)).unwrap();
    assert_eq!(
        (after.generation, after.bytes_on_disk),
        (before.generation, before.bytes_on_disk)
    );

    let report = engine
        .install_semantic_vectors(
            library.install_input(&vectors, &valid, &manifest, Some(digest)),
            &fresh,
        )
        .unwrap();
    assert!(report.generation > before.generation);
    assert_eq!(expansions(&vectors), Vec::new());
    engine
        .verify_semantic_vectors(dir_string(&vectors), &fresh)
        .unwrap();
}

/// In the other order — the valid release first, a download that is not it second — the
/// second is refused for what it is and leaves the set as the first made it, and nothing of
/// either expansion stays behind.
#[test]
fn a_refused_install_after_a_valid_one_leaves_the_set_as_it_made_it() {
    let library = build_library();
    let engine = library.engine();
    let token = SemanticCancellationToken::new();
    let (segment, manifest, digest) = release(&library.package);
    let valid = library.work.join("segment.oxv.zst");
    std::fs::write(
        &valid,
        zstd::stream::encode_all(std::fs::File::open(&segment).unwrap(), 1).unwrap(),
    )
    .unwrap();
    let zeros = library.work.join("zeros.oxv.zst");
    std::fs::write(
        &zeros,
        zstd::stream::encode_all(&vec![0u8; 1 << 20][..], 1).unwrap(),
    )
    .unwrap();
    let vectors = &library.vectors;

    let installed = engine
        .install_semantic_vectors(
            library.install_input(vectors, &valid, &manifest, Some(digest.clone())),
            &token,
        )
        .unwrap();
    let error = match engine.install_semantic_vectors(
        library.install_input(vectors, &zeros, &manifest, Some(digest)),
        &token,
    ) {
        Ok(_) => panic!("zeros are no segment"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind,
        SemanticErrorKind::ArtifactCorrupt,
        "{}",
        error.message
    );
    let info = engine.semantic_vectors_info(dir_string(vectors)).unwrap();
    assert_eq!(info.generation, installed.generation);
    assert_eq!(expansions(vectors), Vec::new());
    engine
        .verify_semantic_vectors(dir_string(vectors), &token)
        .unwrap();
}

/// An install or a compaction in another process holds the sidecar's lock on the set, and
/// meets this one there: refused as busy too, by the kind of the sidecar's refusal.
#[test]
fn an_install_or_compaction_under_another_process_lock_is_refused_as_busy() {
    let library = build_library();
    let engine = library.engine();
    let token = SemanticCancellationToken::new();
    let (segment, manifest, digest) = release(&library.package);
    // What another process's install holds while it runs.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(library.vectors.join(".lock"))
        .unwrap();
    lock.try_lock().unwrap();

    assert_busy(
        engine.install_semantic_vectors(
            library.install_input(&library.vectors, &segment, &manifest, Some(digest.clone())),
            &token,
        ),
        "an install",
    );
    assert_busy(
        engine.compact_semantic_vectors(
            dir_string(&library.vectors),
            None,
            Some(SemanticCompactionPolicy {
                force: true,
                ..SemanticCompactionPolicy::defaults()
            }),
            &token,
        ),
        "a compaction",
    );

    drop(lock);
    engine
        .install_semantic_vectors(
            library.install_input(&library.vectors, &segment, &manifest, Some(digest)),
            &token,
        )
        .unwrap();
}

/// An expansion is its install's until the install returns, though its file is written and
/// closed before the sidecar takes it: its lock file stays locked all along, so an install
/// in another process does not take the file for abandoned in between. One whose process is
/// gone — its lock file unlocked — is removed, lock file and all; an expansion file with no
/// lock file only once it is old. The application's own files are left alone.
#[test]
fn an_expansion_waiting_for_its_install_is_not_taken_for_abandoned() {
    let library = build_library();
    let engine = library.engine();
    let token = SemanticCancellationToken::new();
    let (segment, manifest, digest) = release(&library.package);
    let compressed = library.work.join("segment.oxv.zst");
    std::fs::write(
        &compressed,
        zstd::stream::encode_all(std::fs::File::open(&segment).unwrap(), 1).unwrap(),
    )
    .unwrap();
    let incoming = library.vectors.join("incoming");
    std::fs::create_dir_all(&incoming).unwrap();
    for name in [
        // Written and closed, its install in another process not yet returned.
        ".expanding-waiting.lock",
        ".expanding-waiting.oxv",
        // Its process gone.
        ".expanding-gone.lock",
        ".expanding-gone.oxv",
        // No lock file: one a moment old, one an hour and more.
        ".expanding-stray.oxv",
        ".expanding-old.oxv",
        "download.part",
    ] {
        std::fs::write(incoming.join(name), b"half").unwrap();
    }
    let waiting = std::fs::OpenOptions::new()
        .write(true)
        .open(incoming.join(".expanding-waiting.lock"))
        .unwrap();
    waiting.try_lock().unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(incoming.join(".expanding-old.oxv"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7_200))
        .unwrap();

    let install = || {
        engine
            .install_semantic_vectors(
                library.install_input(
                    &library.vectors,
                    &compressed,
                    &manifest,
                    Some(digest.clone()),
                ),
                &token,
            )
            .unwrap()
    };
    let left = || {
        let mut left: Vec<String> = expansions(&library.vectors)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        left.sort();
        left
    };
    install();
    assert_eq!(
        left(),
        [
            ".expanding-stray.oxv",
            ".expanding-waiting.lock",
            ".expanding-waiting.oxv",
            "download.part"
        ]
    );

    drop(waiting);
    install();
    assert_eq!(left(), [".expanding-stray.oxv", "download.part"]);
}

/// A release that is not the one published, or not for this installation, is refused by
/// kind and field, and the set is left as it was.
#[test]
fn a_release_not_published_or_not_for_this_installation_is_refused() {
    let library = build_library();
    let engine = library.engine();
    let token = SemanticCancellationToken::new();
    let (segment, manifest, digest) = release(&library.package);
    let before = engine
        .semantic_vectors_info(dir_string(&library.vectors))
        .unwrap();
    let refusal =
        |input: SemanticVectorsInstallInput| match engine.install_semantic_vectors(input, &token) {
            Ok(_) => panic!("the release must be refused"),
            Err(error) => error,
        };

    let error = refusal(library.install_input(
        &library.vectors,
        &segment,
        &manifest,
        Some(digest.replace(&digest[..2], "00")),
    ));
    assert_eq!(
        error.kind,
        SemanticErrorKind::ArtifactNotPublished,
        "{}",
        error.message
    );

    let error = refusal(library.install_input(
        &library.vectors,
        &segment,
        "{\"not\": \"a release\"}",
        None,
    ));
    assert_eq!(
        (error.kind, error.field.as_deref()),
        (SemanticErrorKind::ArtifactCorrupt, Some("manifest_json")),
        "{}",
        error.message
    );
    let error = refusal(library.install_input(
        &library.vectors,
        &library.package.join("absent.oxv"),
        &manifest,
        None,
    ));
    assert_eq!(
        (error.kind, error.field.as_deref()),
        (SemanticErrorKind::InvalidInput, Some("segment_path")),
        "{}",
        error.message
    );

    let edits: [IdentityEdit; 2] = [
        ("model.chunking_identity", |model| {
            model.chunking_identity ^= 1
        }),
        ("model.family_id", |model| {
            model.family_id.push_str("-other")
        }),
    ];
    for (field, edit) in edits {
        let mut model = library.model.clone();
        edit(&mut model);
        let error = refusal(SemanticVectorsInstallInput {
            model_identity_json: serde_json::to_string(&model).unwrap(),
            ..library.install_input(&library.vectors, &segment, &manifest, Some(digest.clone()))
        });
        assert_eq!(
            error.kind,
            SemanticErrorKind::ArtifactIncompatible,
            "{}",
            error.message
        );
        assert_eq!(error.field.as_deref(), Some(field), "{}", error.message);
    }

    let after = engine
        .semantic_vectors_info(dir_string(&library.vectors))
        .unwrap();
    assert_eq!(
        (after.generation, after.bytes_on_disk),
        (before.generation, before.bytes_on_disk)
    );
}

/// The next release, installed under an open session, is what that session serves.
#[test]
fn an_install_moves_the_open_session_onto_the_release() {
    let library = build_library();
    let mut engine = library.engine();
    engine.open_semantic_artifact(library.input()).unwrap();
    assert_eq!(
        engine.semantic_status().vectors_library_version,
        Some(LIBRARY_VERSION)
    );

    replace_book(
        &mut engine,
        (
            "בראשית",
            "/מקרא/תורה",
            GENESIS,
            0,
            format!("שורה חדשה בראש הספר לפני כל השאר\n{GENESIS_TEXT}"),
        ),
    );
    let next = library.package.with_file_name("package-next");
    build_package(
        &library.index,
        &library.work,
        &library.model_file,
        LIBRARY_VERSION + 1,
        "v31-20261101000000",
        &next,
        None,
    );
    let (segment, manifest, digest) = release(&next);
    let report = engine
        .install_semantic_vectors(
            library.install_input(&library.vectors, &segment, &manifest, Some(digest)),
            &SemanticCancellationToken::new(),
        )
        .unwrap();
    assert_eq!(report.library_version, LIBRARY_VERSION + 1);

    let status = engine.semantic_status();
    assert_eq!(status.state, SemanticState::Ready);
    assert_eq!(status.vectors_library_version, Some(LIBRARY_VERSION + 1));
    assert_eq!(status.vector_count, EMBEDDED + 1);
    assert_eq!(
        semantic_lines(&engine, PROBE_LINE, &[]).first(),
        Some(&(GENESIS.to_string(), PROBE_LINE.to_string(), 4))
    );
}

/// A set compacts when its policy asks, or when forced; the open session follows it, and
/// records move onto the lines that hold their text when the index has the column and is
/// of the set's library version.
#[test]
fn a_set_compacts_when_asked_and_the_session_follows() {
    for version_4 in [false, true] {
        let library = build_library_of(&default_books(), version_4);
        let mut engine = library.engine();
        let token = SemanticCancellationToken::new();
        let vectors = dir_string(&library.vectors);
        engine.open_semantic_artifact(library.input()).unwrap();

        let unforced = engine
            .compact_semantic_vectors(vectors.clone(), None, None, &token)
            .unwrap();
        assert!(!unforced.compacted, "one base wants no compaction");

        replace_book(
            &mut engine,
            (
                "בראשית",
                "/מקרא/תורה",
                GENESIS,
                0,
                format!("ויהי ערב ויהי בקר יום אחד ושני ושלישי\n{GENESIS_TEXT}"),
            ),
        );
        let forced = SemanticCompactionPolicy {
            force: true,
            ..SemanticCompactionPolicy::defaults()
        };
        let report = engine
            .compact_semantic_vectors(
                vectors.clone(),
                Some(LIBRARY_VERSION),
                Some(forced.clone()),
                &token,
            )
            .unwrap();
        assert!(report.compacted, "{}", report.reason);
        assert!(report.generation > unforced.generation);
        assert_eq!(report.slots_after, u64::from(EMBEDDED));
        if version_4 {
            assert_eq!(
                (report.hints_refreshed, report.records_pruned),
                (0, 0),
                "without the column, records are kept as they are"
            );
        } else {
            assert!(report.hints_refreshed >= 1, "{}", report.reason);
        }

        let info = engine.semantic_vectors_info(vectors.clone()).unwrap();
        assert_eq!(info.generation, report.generation);
        assert_eq!(info.segments.len(), 1);
        assert_eq!(info.segments[0].kind, SemanticVectorsPackageKind::Compacted);
        let status = engine.semantic_status();
        assert_eq!(status.state, SemanticState::Ready);
        assert!(!status.needs_compaction);
        assert_eq!(
            semantic_lines(&engine, PROBE_LINE, &[]).first(),
            Some(&(GENESIS.to_string(), PROBE_LINE.to_string(), 4)),
            "schema version 4: {version_4}"
        );

        // Another library version's index re-anchors nothing.
        let other = engine
            .compact_semantic_vectors(
                vectors.clone(),
                Some(LIBRARY_VERSION + 1),
                Some(forced),
                &token,
            )
            .unwrap();
        assert_eq!((other.hints_refreshed, other.records_pruned), (0, 0));
    }
}

/// A policy out of range is refused before anything is read, by the option at fault.
#[test]
fn a_compaction_policy_out_of_range_is_invalid_input() {
    let library = build_library();
    let engine = library.engine();
    let policies = [
        (
            "policy.min_free_space_factor",
            SemanticCompactionPolicy {
                min_free_space_factor: 0.5,
                ..SemanticCompactionPolicy::defaults()
            },
        ),
        (
            "policy.max_delta_ratio",
            SemanticCompactionPolicy {
                max_delta_ratio: f64::NAN,
                ..SemanticCompactionPolicy::defaults()
            },
        ),
        (
            "policy.max_dead_ratio",
            SemanticCompactionPolicy {
                max_dead_ratio: -0.1,
                ..SemanticCompactionPolicy::defaults()
            },
        ),
    ];
    for (field, policy) in policies {
        let error = match engine.compact_semantic_vectors(
            dir_string(&library.vectors),
            None,
            Some(policy),
            &SemanticCancellationToken::new(),
        ) {
            Ok(report) => panic!("{field} must be refused: {}", report.reason),
            Err(error) => error,
        };
        assert_eq!(
            error.kind,
            SemanticErrorKind::InvalidInput,
            "{}",
            error.message
        );
        assert_eq!(error.field.as_deref(), Some(field));
    }
}

/// A damaged block, which opening does not read, is what verifying finds; after it, opening
/// refuses the segment too.
#[test]
fn verifying_finds_a_damaged_block_and_opening_refuses_it_after() {
    let library = build_library();
    let engine = library.engine();
    let token = SemanticCancellationToken::new();
    for entry in std::fs::read_dir(library.vectors.join("segments")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "oxv") {
            let mut bytes = std::fs::read(&path).unwrap();
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0xff;
            std::fs::write(&path, bytes).unwrap();
        }
    }

    let error = match engine.verify_semantic_vectors(dir_string(&library.vectors), &token) {
        Ok(report) => panic!("the damage must be found: {} bytes", report.bytes_checked),
        Err(error) => error,
    };
    assert_eq!(
        error.kind,
        SemanticErrorKind::ArtifactCorrupt,
        "{}",
        error.message
    );
    let error = open_refusal(&engine, library.input());
    assert_refused(&engine, &error, SemanticErrorKind::ArtifactCorrupt, None);

    let missing = engine
        .verify_semantic_vectors(dir_string(&library.vectors.join("absent")), &token)
        .err()
        .expect("nothing to verify");
    assert_eq!(
        missing.kind,
        SemanticErrorKind::ArtifactMissing,
        "{}",
        missing.message
    );
}

/// Coverage counts the live lines the recipe embeds and those the set holds, the same from
/// the column and recomputed without it.
#[test]
fn coverage_counts_the_live_lines_the_set_holds() {
    for version_4 in [false, true] {
        let library = build_library_of(&default_books(), version_4);
        let mut engine = library.engine();
        let token = SemanticCancellationToken::new();
        let vectors = dir_string(&library.vectors);
        let coverage = engine.semantic_coverage(vectors.clone(), &token).unwrap();
        assert_eq!(
            (
                coverage.live_keyed_lines,
                coverage.covered_lines,
                coverage.books_live,
                coverage.books_covered,
                coverage.vectors_library_version,
            ),
            (
                u64::from(EMBEDDED),
                u64::from(EMBEDDED),
                2,
                2,
                LIBRARY_VERSION
            ),
            "schema version 4: {version_4}"
        );
        assert_eq!(coverage.ratio, 1.0);

        add_books(
            &mut engine,
            &[(
                "ספר נוסף",
                "/אחר",
                "/books/another.txt",
                2,
                "שורה שלא הייתה בספרייה כשהווקטורים נבנו ממנה".to_string(),
            )],
        );
        let coverage = engine.semantic_coverage(vectors.clone(), &token).unwrap();
        assert_eq!(
            (
                coverage.live_keyed_lines,
                coverage.covered_lines,
                coverage.books_live,
                coverage.books_covered,
            ),
            (u64::from(EMBEDDED) + 1, u64::from(EMBEDDED), 3, 2),
            "schema version 4: {version_4}"
        );

        let cancelled = SemanticCancellationToken::new();
        cancelled.cancel();
        let error = engine
            .semantic_coverage(vectors.clone(), &cancelled)
            .err()
            .expect("a cancelled count");
        assert_eq!(error.kind, SemanticErrorKind::Cancelled);
    }
    let library = build_library();
    let missing = library
        .engine()
        .semantic_coverage(
            dir_string(&library.vectors.join("absent")),
            &SemanticCancellationToken::new(),
        )
        .err()
        .expect("nothing installed");
    assert_eq!(
        (missing.kind, missing.field.as_deref()),
        (SemanticErrorKind::ArtifactMissing, Some("vectors_dir"))
    );
}

/// A set built from the index finds every record at its hint and covers every keyed line;
/// after a line is inserted above, the records below it are counted as moved.
#[test]
fn the_validator_finds_every_record_at_its_hint_until_lines_move() {
    use otzaria_semantic_search::semantic::segment_set::SegmentSet;
    use search_engine::semantic_plan::validate;
    let library = build_library();
    let fresh = validate(&library.index, &SegmentSet::open(&library.vectors).unwrap()).unwrap();
    let embedded = u64::from(EMBEDDED);
    assert_eq!(
        (
            fresh.keyed_lines,
            fresh.covered_lines,
            fresh.records,
            fresh.at_hint
        ),
        (embedded, embedded, embedded, embedded)
    );

    let mut engine = library.engine();
    replace_book(
        &mut engine,
        (
            "בראשית",
            "/מקרא/תורה",
            GENESIS,
            0,
            format!("שורה חדשה בראש הספר לפני כל השאר\n{GENESIS_TEXT}"),
        ),
    );
    drop(engine);
    let moved = validate(&library.index, &SegmentSet::open(&library.vectors).unwrap()).unwrap();
    assert_eq!(moved.records, embedded);
    assert_eq!(moved.keyed_lines, embedded + 1);
    assert!(moved.moved >= 1, "{moved:?}");
    assert_eq!(moved.at_hint + moved.moved + moved.gone, moved.records);
}
