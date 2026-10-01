//! The Meivin ONNX model, driven through the public API end to end: a lexical index, a
//! sidecar configured with the model's identity, a handful of Hebrew lines indexed, and
//! semantic queries against them.
//!
//! Every other suite here runs the sidecar on its deterministic stand-in, whose vectors
//! mean nothing. These ask what only the real model can answer once the vectors have
//! crossed this crate:
//!
//! * whether they mean something: each query must rank the one line it is about first;
//! * whether they were computed from the text the model was trained to see. A ranking
//!   cannot tell that — with no role prefix on either side these queries still rank their
//!   lines first (measured) — so text recipe 2 is checked against its definition instead:
//!   a session on it must score exactly like a recipe 1 session handed the
//!   `[PASSAGE] ` / `[QUERY] `-prefixed strings literally.
//!
//! In-graph pooling is the only pooling the ONNX backend serves, so a configuration
//! that reached it with any other would not load. The 256-token cap is not exercised,
//! since every line here is far shorter; that it reaches the sidecar is shown by
//! `configure_semantic_hands_each_input_to_the_sidecar`.
//!
//! It needs the model, which is gated and not in the repository, and an ONNX Runtime shared
//! library, which the sidecar loads rather than links. So the tests are `#[ignore]`d *and*
//! skip loudly unless both `OTZARIA_TEST_ONNX_MODEL` and `OTZARIA_ONNX_RUNTIME` name
//! existing files, the pattern of the sidecar's `golden` suite. Where the files are meant
//! to be there, `OTZARIA_REQUIRE_ONNX_MODEL` turns each skip into a failure; CI's
//! real-model job, which runs them on Linux, macOS and Windows, sets it. The sidecar reads
//! `OTZARIA_ONNX_RUNTIME` itself; the reference runtime is Microsoft's ONNX Runtime 1.28.0
//! release. `tokenizer.json` must sit beside the graph, as it does in the published
//! package.
//!
//! The graph is the one the application uses, `seforim-embed-round2-int8.onnx`. The
//! full-precision `seforim-embed-round2-fp32.onnx` published beside it runs the same tests:
//! the quantization label is read off the file name, so each graph is configured under
//! its own identity.
//!
//! One test is the application's own path rather than the scaffolding's: the build binary
//! embeds the lines into an artifact and stamps the index, and `open_semantic_artifact`
//! opens it and serves the queries, embedding nothing but them. It also needs the model's
//! published identity files, `model.json` and `chunking.json`, from the directory
//! `OTZARIA_TEST_ONNX_IDENTITY` names: the sidecar's `config/models/meivin-round2-onnx`
//! for the INT8 graph, `config/models/meivin-round2-onnx-fp32` for the fp32 one. Run them
//! with:
//!
//! ```sh
//! OTZARIA_TEST_ONNX_MODEL=/path/to/judaic-semantic-round2-onnx-zayit/seforim-embed-round2-int8.onnx \
//! OTZARIA_ONNX_RUNTIME=/path/to/onnxruntime-osx-arm64-1.28.0/lib/libonnxruntime.dylib \
//! OTZARIA_TEST_ONNX_IDENTITY=/path/to/otzaria-semantic-search/config/models/meivin-round2-onnx \
//!   cargo test --manifest-path rust/Cargo.toml --features semantic-onnx \
//!   --test semantic_onnx_model -- --ignored --nocapture
//! ```

#![cfg(feature = "semantic-onnx")]

use search_engine::api::search_engine::{
    SearchEngine, SemanticArtifactInput, SemanticBookInput, SemanticBookLineInput,
    SemanticConfigInput, SemanticExecutedMode, SemanticLexicalMode, SemanticRetrievalMode,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const MODEL_ENV: &str = "OTZARIA_TEST_ONNX_MODEL";
/// The directory holding the model's published identity files, for the artifact test.
const IDENTITY_ENV: &str = "OTZARIA_TEST_ONNX_IDENTITY";
/// Read by the sidecar, not by this test: checked here only so that a missing runtime
/// is a loud skip rather than an "ONNX Runtime could not be loaded" failure.
const RUNTIME_ENV: &str = "OTZARIA_ONNX_RUNTIME";
/// Set, to anything, where the files are meant to be there, as in CI's real-model job: a
/// skip there would be a green run that tested nothing, so each one fails the test
/// instead. The Dart suites' `OTZARIA_REQUIRE_NATIVE` does the same for the library.
const REQUIRE_ENV: &str = "OTZARIA_REQUIRE_ONNX_MODEL";
const BOOK_KEY: &str = "/library/meivin-probe.txt";
const TITLE: &str = "probe";
const TOPICS: &str = "/probe";

/// The model's role prefixes, as its model card spells them: learned special tokens it
/// was trained to find before every indexed passage and every query. Spelled out here
/// rather than taken from the sidecar, because the test checks the sidecar against them.
const PASSAGE_PREFIX: &str = "[PASSAGE] ";
const QUERY_PREFIX: &str = "[QUERY] ";

/// How far two scores of the same strings may lie apart. The vectors are bit-identical
/// in practice (same graph, runtime and thread count, one text per run); the tolerance
/// only spares a platform with non-deterministic kernels a failure for the wrong reason.
/// Dropping the prefixes from both sides moves this probe's 15 scores by 0.002 to 0.06 on
/// the INT8 graph and by 0.0006 to 0.06 on the fp32 one (measured), so it is far below
/// anything a wrong recipe does.
const SCORE_TOLERANCE: f32 = 1e-6;

/// Lines on unrelated subjects, each in a section of its own: a line shorter than the
/// recipe's minimum borrows its neighbours' text within a section, and a probe line that
/// carried another line's words would blur exactly what is being measured. All of them
/// are well within the recipe's character cap even with a prefix, so a recipe 1 session
/// handed prefixed lines embeds them whole, exactly as recipe 2 does.
const LINES: [(u64, &str); 5] = [
    (
        101,
        "מאימתי קורין את שמע בערבית משעה שהכהנים נכנסין לאכול בתרומתן",
    ),
    (102, "כבד את אביך ואת אמך למען יאריכון ימיך"),
    (103, "זכור את יום השבת לקדשו ששת ימים תעבד ועשית כל מלאכתך"),
    (
        104,
        "הלומד תורה בילדותו למה הוא דומה לדיו כתובה על נייר חדש",
    ),
    (105, "ואהבת לרעך כמוך רבי עקיבא אומר זה כלל גדול בתורה"),
];

/// Each query and the line it is obviously about.
const QUERIES: [(&str, u64); 3] = [
    ("כיבוד אב ואם", 102),
    ("שמירת השבת ואיסור מלאכה", 103),
    ("זמן קריאת שמע של ערבית", 101),
];

/// The file `variable` names, or what is missing.
fn required_file(variable: &str, needed: &str) -> Result<PathBuf, String> {
    match std::env::var(variable) {
        Ok(path) if !path.trim().is_empty() => {
            let path = PathBuf::from(path.trim());
            if path.exists() {
                Ok(path)
            } else {
                Err(format!(
                    "{variable} points at {path:?}, which does not exist"
                ))
            }
        }
        _ => Err(format!("{variable} is not set. This test needs {needed}.")),
    }
}

/// The graph.
fn model_file() -> Result<PathBuf, String> {
    required_file(
        MODEL_ENV,
        "seforim-embed-round2-int8.onnx (or its -fp32 twin), with its tokenizer.json \
         beside it, from the gated judaic-semantic-round2-onnx-zayit model",
    )
}

/// The runtime the sidecar will load.
fn runtime_library() -> Result<PathBuf, String> {
    required_file(
        RUNTIME_ENV,
        "the ONNX Runtime shared library (libonnxruntime.dylib, libonnxruntime.so or \
         onnxruntime.dll), such as the one in Microsoft's 1.28.0 release",
    )
}

/// Every file a test needs, or `None` once each one missing has been reported. All of
/// them are looked up before any is acted on, so one run reports everything missing.
///
/// A missing file skips the test, loudly, rather than failing it: most runs have none of
/// them — a contributor's, and every CI job but the real-model one — and a test that
/// fails there teaches everyone to ignore it. Under [`REQUIRE_ENV`] it fails the test.
fn needed<const N: usize>(lookups: [Result<PathBuf, String>; N]) -> Option<[PathBuf; N]> {
    let missing: Vec<String> = lookups
        .iter()
        .filter_map(|lookup| lookup.as_ref().err().cloned())
        .collect();
    if missing.is_empty() {
        return Some(lookups.map(Result::unwrap));
    }
    assert!(
        std::env::var_os(REQUIRE_ENV).is_none(),
        "{REQUIRE_ENV} is set, so this test may not skip, and it would have:\n{}",
        missing.join("\n")
    );
    for reason in missing {
        println!("SKIPPED: {reason}");
    }
    None
}

/// The quantization label of a Meivin Round 2 graph, read off its published file name:
/// the two graphs are one model under two identities, and configuring the INT8 graph as
/// `"fp32"` would record an identity that names other weights.
fn quantization_of(graph: &Path) -> &'static str {
    let stem = graph
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.ends_with("-int8") {
        "int8"
    } else if stem.ends_with("-fp32") {
        "fp32"
    } else {
        panic!(
            "{MODEL_ENV} names {}, which is neither seforim-embed-round2-int8.onnx nor \
             seforim-embed-round2-fp32.onnx, so its quantization is unknown",
            graph.display()
        )
    }
}

/// The Meivin identity, exactly as the application configures it for `model`.
fn meivin(root: &TempDir, model: &Path) -> SemanticConfigInput {
    SemanticConfigInput {
        root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
        model_path: model.to_string_lossy().into_owned(),
        model_id: "ArieLLL123/judaic-semantic-round2-onnx-zayit".to_owned(),
        embedding_dim: 256,
        pooling: "in-graph".to_owned(),
        max_tokens: 256,
        model_quantization: quantization_of(model).to_owned(),
        embedding_text_version: 2,
    }
}

/// An engine over [`LINES`] with a sidecar configured as `config` and every line
/// embedded, each as `passage_prefix` followed by the line. Checks that ONNX Runtime,
/// not a stand-in, produced the vectors.
fn indexed_engine(
    root: &TempDir,
    config: SemanticConfigInput,
    passage_prefix: &str,
) -> SearchEngine {
    let index_dir = root.path().join("tantivy");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut engine = SearchEngine::new(index_dir.to_str().unwrap());
    // Semantic-only hits are hydrated from Tantivy by id, so the lexical index holds the
    // same lines under the same ids.
    for (id, text) in LINES {
        engine
            .add_document(
                id,
                TITLE,
                &format!("probe {id}"),
                TOPICS,
                text,
                id,
                false,
                BOOK_KEY,
                Some(id),
                None,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();

    let configured = engine
        .configure_semantic(config)
        .expect("the Meivin identity must configure");
    assert!(configured.enabled);

    let indexed = engine
        .semantic_index_books(vec![SemanticBookInput {
            source_book_key: BOOK_KEY.to_owned(),
            title: TITLE.to_owned(),
            content_fingerprint: 1,
            is_pdf: false,
            topics: TOPICS.to_owned(),
            extra_facets: Vec::new(),
            lines: LINES
                .iter()
                .map(|&(id, text)| SemanticBookLineInput {
                    line_id: id,
                    section_id: id,
                    text: format!("{passage_prefix}{text}"),
                    line_hash: id,
                    reference: format!("probe {id}"),
                    segment: id,
                })
                .collect(),
        }])
        .expect("indexing loads the model and embeds every line");
    assert_eq!(indexed.books_indexed, 1);
    assert_eq!(indexed.chunks_written, LINES.len() as u32);

    let status = engine.semantic_status();
    assert!(status.available, "{:?}", status.last_error);
    assert_eq!(
        status.embedding_backend.as_deref(),
        Some("onnxruntime-sentence-v1"),
        "the ONNX graph must be served by ONNX Runtime, not by a stand-in"
    );
    assert_eq!(status.embedding_dim, 256);
    assert_eq!(status.vector_count, LINES.len() as u32);
    engine
}

/// Every line with its semantic score for `query`, best first.
fn ranking(engine: &SearchEngine, query: &str) -> Vec<(u64, f32)> {
    let response = engine
        .search_semantic(
            query.to_owned(),
            Vec::new(),
            LINES.len() as u32,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            None,
            false,
            false,
        )
        .unwrap();
    assert_eq!(response.executed_mode, SemanticExecutedMode::SemanticOnly);
    assert!(
        response.fallback_reason.is_none(),
        "{:?}",
        response.fallback_reason
    );

    let ranking: Vec<(u64, f32)> = response
        .results
        .iter()
        .map(|hit| {
            let score = hit
                .semantic_score
                .unwrap_or_else(|| panic!("a semantic-only hit has a semantic score: {query}"));
            (hit.id, score)
        })
        .collect();
    assert_eq!(
        ranking.len(),
        LINES.len(),
        "every line is a candidate: {ranking:?}"
    );
    ranking
}

#[test]
#[ignore = "needs the Meivin ONNX model and an ONNX Runtime; set OTZARIA_TEST_ONNX_MODEL \
            and OTZARIA_ONNX_RUNTIME and pass --ignored"]
fn the_meivin_model_ranks_the_line_a_query_is_about_first() {
    let Some([model, _runtime]) = needed([model_file(), runtime_library()]) else {
        return;
    };
    let root = TempDir::new().unwrap();
    let engine = indexed_engine(&root, meivin(&root, &model), "");

    for (query, expected) in QUERIES {
        let ranking = ranking(&engine, query);
        println!("query for line {expected}: {ranking:?}");
        assert_eq!(
            ranking.first().map(|&(id, _)| id),
            Some(expected),
            "the line the query is about must rank first; ranking (id, score): {ranking:?}"
        );
    }
}

/// Text recipe 2 is recipe 1's text behind the role prefixes, on both sides of a search.
/// So a recipe 2 session over the bare lines, queried with bare queries, and a recipe 1
/// session over the lines with `[PASSAGE] ` written in, queried with `[QUERY] ` written
/// in, embed the very same strings and must score every pair alike. (Recipe 2 also trims
/// what it prefixes; nothing here has whitespace at either end to trim.)
///
/// This is what shows that `embedding_text_version` arrives as 2 and that the version
/// means what the model needs: had it been lost, both halves of the first session would
/// be embedded bare, and its scores would be recipe 1's.
#[test]
#[ignore = "needs the Meivin ONNX model and an ONNX Runtime; set OTZARIA_TEST_ONNX_MODEL \
            and OTZARIA_ONNX_RUNTIME and pass --ignored"]
fn recipe_two_scores_exactly_like_the_role_prefixed_text_it_is_defined_as() {
    let Some([model, _runtime]) = needed([model_file(), runtime_library()]) else {
        return;
    };
    let recipe_two_root = TempDir::new().unwrap();
    let recipe_two = indexed_engine(&recipe_two_root, meivin(&recipe_two_root, &model), "");
    let spelled_out_root = TempDir::new().unwrap();
    let spelled_out = indexed_engine(
        &spelled_out_root,
        SemanticConfigInput {
            embedding_text_version: 1,
            ..meivin(&spelled_out_root, &model)
        },
        PASSAGE_PREFIX,
    );

    for (query, _) in QUERIES {
        let mut by_recipe = ranking(&recipe_two, query);
        let mut by_hand = ranking(&spelled_out, &format!("{QUERY_PREFIX}{query}"));
        by_recipe.sort_by_key(|&(id, _)| id);
        by_hand.sort_by_key(|&(id, _)| id);
        for (&(id, score), &(hand_id, hand_score)) in by_recipe.iter().zip(&by_hand) {
            assert_eq!(id, hand_id, "{by_recipe:?} against {by_hand:?}");
            assert!(
                (score - hand_score).abs() <= SCORE_TOLERANCE,
                "line {id} for {query:?}: recipe 2 scored {score} and the prefixed strings \
                 {hand_score}, so recipe 2 did not embed [PASSAGE] / [QUERY] before the text"
            );
        }
    }
}

/// The application's path with the real model: the build binary embeds the library into an
/// artifact and stamps the index, and the device opens that artifact against the index and
/// embeds nothing but the queries. The identity files are the model's published ones, used
/// by both sides exactly as a release would use them, so this also shows the published
/// `model_checksum` names the graph on disk.
#[test]
#[ignore = "needs the Meivin ONNX model, an ONNX Runtime and the model's identity files; set \
            OTZARIA_TEST_ONNX_MODEL, OTZARIA_ONNX_RUNTIME and OTZARIA_TEST_ONNX_IDENTITY and \
            pass --ignored"]
fn an_artifact_built_on_the_build_machine_opens_on_the_device_and_ranks_the_line_first() {
    let identity = required_file(
        IDENTITY_ENV,
        "the directory with the model's model.json and chunking.json, such as the sidecar's \
         config/models/meivin-round2-onnx",
    );
    let Some([model, _runtime, identity]) = needed([model_file(), runtime_library(), identity])
    else {
        return;
    };

    // The library, closed before the build reads it, as a release builds it. One book, so
    // the ids are the ones `add_text_book` composes; every line is long enough to embed on
    // its own, so none borrows a neighbour's text.
    let root = TempDir::new().unwrap();
    let index = root.path().join("tantivy");
    std::fs::create_dir_all(&index).unwrap();
    {
        let mut engine = SearchEngine::new(index.to_str().unwrap());
        let text = LINES.map(|(_, line)| line).join("\n");
        engine
            .add_text_book(
                TITLE.to_owned(),
                TOPICS.to_owned(),
                BOOK_KEY.to_owned(),
                0,
                0,
                text,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
    }

    let artifact = root.path().join("artifact");
    let built = Command::new(env!("CARGO_BIN_EXE_build_semantic_artifact"))
        .args([
            "--index",
            index.to_str().unwrap(),
            "--library-version",
            "meivin-probe",
            "--model",
            identity.join("model.json").to_str().unwrap(),
            "--model-file",
            model.to_str().unwrap(),
            "--chunking",
            identity.join("chunking.json").to_str().unwrap(),
            "--out",
            artifact.to_str().unwrap(),
            "--stamp-index",
        ])
        .output()
        .expect("the build binary runs");
    assert!(
        built.status.success(),
        "build failed:\n{}\n{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );

    let engine = SearchEngine::new(index.to_str().unwrap());
    let status = engine
        .open_semantic_artifact(SemanticArtifactInput {
            artifact_dir: artifact.to_string_lossy().into_owned(),
            model_path: model.to_string_lossy().into_owned(),
            model_identity_json: std::fs::read_to_string(identity.join("model.json")).unwrap(),
            published_digest: None,
        })
        .expect("the artifact built from this index opens against it");
    assert!(status.available, "{:?}", status.last_error);
    assert_eq!(
        status.embedding_backend.as_deref(),
        Some("onnxruntime-sentence-v1")
    );
    assert_eq!(status.vector_count, LINES.len() as u32);

    for (query, expected) in QUERIES {
        let expected_text = LINES
            .iter()
            .find(|(id, _)| *id == expected)
            .map(|(_, text)| *text)
            .unwrap();
        let ranking = ranking(&engine, query);
        let texts: Vec<(String, f32)> = ranking
            .iter()
            .map(|&(id, score)| {
                let line = engine.get_document_by_id(id).unwrap().expect("hydrated");
                (line.text, score)
            })
            .collect();
        println!("artifact query for line {expected}: {ranking:?}");
        assert_eq!(
            texts.first().map(|(text, _)| text.as_str()),
            Some(expected_text),
            "the line the query is about must rank first; ranking (text, score): {texts:?}"
        );
    }
}
