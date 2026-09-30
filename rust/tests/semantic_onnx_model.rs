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
//! existing files, the pattern of the sidecar's `golden` suite. The sidecar reads
//! `OTZARIA_ONNX_RUNTIME` itself; the reference runtime is Microsoft's ONNX Runtime 1.28.0
//! release. `tokenizer.json` must sit beside the graph, as it does in the published
//! package. Run them with:
//!
//! ```sh
//! OTZARIA_TEST_ONNX_MODEL=/path/to/judaic-semantic-round2-onnx-zayit/seforim-embed-round2-fp32.onnx \
//! OTZARIA_ONNX_RUNTIME=/path/to/onnxruntime-osx-arm64-1.28.0/lib/libonnxruntime.dylib \
//!   cargo test --manifest-path rust/Cargo.toml --features semantic-onnx \
//!   --test semantic_onnx_model -- --ignored --nocapture
//! ```

#![cfg(feature = "semantic-onnx")]

use search_engine::api::search_engine::{
    SearchEngine, SemanticBookInput, SemanticBookLineInput, SemanticConfigInput,
    SemanticExecutedMode, SemanticLexicalMode, SemanticRetrievalMode,
};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const MODEL_ENV: &str = "OTZARIA_TEST_ONNX_MODEL";
/// Read by the sidecar, not by this test: checked here only so that a missing runtime
/// is a loud skip rather than an "ONNX Runtime could not be loaded" failure.
const RUNTIME_ENV: &str = "OTZARIA_ONNX_RUNTIME";
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
/// Dropping the prefixes from both sides moves this probe's 15 scores by 0.0006 to 0.06
/// (measured), so it is far below anything a wrong recipe does.
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

/// The file `variable` names, or `None` with a loud explanation of what is missing.
/// Skipping rather than failing because CI has neither file, and a test that fails there
/// teaches everyone to ignore it.
fn required_file(variable: &str, needed: &str) -> Option<PathBuf> {
    match std::env::var(variable) {
        Ok(path) if !path.trim().is_empty() => {
            let path = PathBuf::from(path.trim());
            if path.exists() {
                return Some(path);
            }
            println!("SKIPPED: {variable} points at {path:?}, which does not exist");
            None
        }
        _ => {
            println!("SKIPPED: {variable} is not set. This test needs {needed}.");
            None
        }
    }
}

/// The graph, when both it and a runtime are there to run it.
fn model_and_runtime() -> Option<PathBuf> {
    // Both looked up before either is acted on, so one run reports everything missing.
    let model = required_file(
        MODEL_ENV,
        "seforim-embed-round2-fp32.onnx, with its tokenizer.json beside it, from the \
         gated judaic-semantic-round2-onnx-zayit model",
    );
    let runtime = required_file(
        RUNTIME_ENV,
        "the ONNX Runtime shared library (libonnxruntime.dylib, libonnxruntime.so or \
         onnxruntime.dll), such as the one in Microsoft's 1.28.0 release",
    );
    model.zip(runtime).map(|(model, _)| model)
}

/// The Meivin identity, exactly as the application configures it.
fn meivin(root: &TempDir, model: &Path) -> SemanticConfigInput {
    SemanticConfigInput {
        root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
        model_path: model.to_string_lossy().into_owned(),
        model_id: "ArieLLL123/judaic-semantic-round2-onnx-zayit".to_owned(),
        embedding_dim: 256,
        pooling: "in-graph".to_owned(),
        max_tokens: 256,
        model_quantization: "fp32".to_owned(),
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
    let Some(model) = model_and_runtime() else {
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
    let Some(model) = model_and_runtime() else {
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
