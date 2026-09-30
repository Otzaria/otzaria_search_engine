//! The Meivin ONNX model, driven through the public API end to end: a lexical index, a
//! sidecar configured with the model's identity, a handful of Hebrew lines indexed, and a
//! semantic query that must rank the one line it is about first.
//!
//! Every other suite here runs the sidecar on its deterministic stand-in, whose vectors
//! mean nothing. This is the one that asks whether the vectors mean something after
//! crossing this crate — whether the recipe `configure_semantic` hands the sidecar
//! (in-graph pooling, a 256-token cap, the `[PASSAGE] ` / `[QUERY] ` text recipe) is the
//! one the model was trained for. A wrong one still produces unit vectors of the right
//! width, so nothing short of a ranking can tell.
//!
//! It needs the model, which is gated and not in the repository, and an ONNX Runtime shared
//! library, which the sidecar loads rather than links. So the test is `#[ignore]`d *and*
//! skips loudly unless both `OTZARIA_TEST_ONNX_MODEL` and `OTZARIA_ONNX_RUNTIME` name
//! existing files, the pattern of the sidecar's `golden` suite. The sidecar reads
//! `OTZARIA_ONNX_RUNTIME` itself; the reference runtime is Microsoft's ONNX Runtime 1.28.0
//! release. `tokenizer.json` must sit beside the graph, as it does in the published
//! package. Run it with:
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
use std::path::PathBuf;
use tempfile::TempDir;

const MODEL_ENV: &str = "OTZARIA_TEST_ONNX_MODEL";
/// Read by the sidecar, not by this test: checked here only so that a missing runtime
/// is a loud skip rather than a `BackendUnavailable` failure.
const RUNTIME_ENV: &str = "OTZARIA_ONNX_RUNTIME";
const BOOK_KEY: &str = "/library/meivin-probe.txt";
const TITLE: &str = "probe";
const TOPICS: &str = "/probe";

/// Lines on unrelated subjects, each in a section of its own: a line shorter than the
/// recipe's minimum borrows its neighbours' text within a section, and a probe line that
/// carried another line's words would blur exactly what is being measured.
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

/// The Meivin identity, exactly as the application configures it.
fn meivin(root: &TempDir, model: &std::path::Path) -> SemanticConfigInput {
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

#[test]
#[ignore = "needs the Meivin ONNX model and an ONNX Runtime; set OTZARIA_TEST_ONNX_MODEL \
            and OTZARIA_ONNX_RUNTIME and pass --ignored"]
fn the_meivin_model_ranks_the_line_a_query_is_about_first() {
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
    let (Some(model), Some(_)) = (model, runtime) else {
        return;
    };

    let root = TempDir::new().unwrap();
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
        .configure_semantic(meivin(&root, &model))
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
                    text: text.to_owned(),
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

    for (query, expected) in QUERIES {
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

        let ranking: Vec<(u64, Option<f32>)> = response
            .results
            .iter()
            .map(|hit| (hit.id, hit.semantic_score))
            .collect();
        println!("query for line {expected}: {ranking:?}");
        assert_eq!(
            ranking.first().map(|&(id, _)| id),
            Some(expected),
            "the line the query is about must rank first; ranking (id, score): {ranking:?}"
        );
    }
}
