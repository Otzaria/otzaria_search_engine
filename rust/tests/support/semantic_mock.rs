use otzaria_semantic_search::semantic::embedding::mock::write_stub_onnx_package;
use search_engine::api::search_engine::{
    SearchEngine, SemanticCancellationToken, SemanticConfigInput, SemanticGroupingMode,
    SemanticLexicalMode, SemanticRetrievalMode, SemanticSearchResponse,
};
use tempfile::TempDir;

/// The folder [`write_stub_model`] writes the stub ONNX package into.
pub(super) fn model_dir(root: &TempDir) -> std::path::PathBuf {
    root.path().join("model")
}

/// Write the structural ONNX fixture served by the deterministic test backend.
pub(super) fn write_stub_model(root: &TempDir) {
    write_stub_onnx_package(&model_dir(root));
}

/// Recipe 1 embeds a query and an identical passage alike in the test backend.
pub(super) fn mock_config(root: &TempDir, model_id: &str) -> SemanticConfigInput {
    SemanticConfigInput {
        root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
        model_path: model_dir(root)
            .join("model.onnx")
            .to_string_lossy()
            .into_owned(),
        model_id: model_id.to_owned(),
        embedding_dim: 64,
        pooling: "in-graph".to_owned(),
        max_tokens: 512,
        model_quantization: "int8".to_owned(),
        embedding_text_version: 1,
        onnx_runtime_path: None,
    }
}

/// Open the sidecar against `root`, writing the stub model it needs.
pub(super) fn configure(engine: &mut SearchEngine, root: &TempDir) {
    write_stub_model(root);
    let status = engine
        .configure_semantic(mock_config(root, "test-mock"))
        .unwrap();
    assert!(
        status.enabled,
        "the sidecar should report itself configured"
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn search(
    engine: &SearchEngine,
    query: &str,
    limit: u32,
    offset: u32,
    lexical_mode: SemanticLexicalMode,
    fuzzy_distance: u8,
    retrieval_mode: SemanticRetrievalMode,
    grouping: Option<SemanticGroupingMode>,
) -> SemanticSearchResponse {
    engine
        .search_semantic(
            query.to_owned(),
            Vec::new(),
            limit,
            offset,
            lexical_mode,
            fuzzy_distance,
            retrieval_mode,
            grouping,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap()
}

pub(super) fn exact(
    engine: &SearchEngine,
    query: &str,
    retrieval_mode: SemanticRetrievalMode,
) -> SemanticSearchResponse {
    search(
        engine,
        query,
        10,
        0,
        SemanticLexicalMode::Exact,
        0,
        retrieval_mode,
        None,
    )
}
