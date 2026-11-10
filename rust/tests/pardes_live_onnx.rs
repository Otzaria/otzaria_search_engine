//! Small actual-model smoke test of the application's public semantic path.
//! It never downloads a model and uses synthetic fixture text, not evaluation data.
#![cfg(feature = "semantic-onnx")]

use search_engine::api::search_engine::{
    SearchEngine, SemanticArtifactInput, SemanticCancellationToken, SemanticHighlightTarget,
    SemanticLexicalMode, SemanticRetrievalMode, SemanticVectorsInstallInput, TextStorage,
};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;

#[test]
#[ignore = "requires OTZARIA_PARDES_MODEL_DIR and OTZARIA_ONNX_RUNTIME, no downloads"]
fn compact_pardes_installs_opens_queries_highlights_and_reopens_with_actual_onnx() {
    let model =
        PathBuf::from(std::env::var_os("OTZARIA_PARDES_MODEL_DIR").expect("model directory"));
    let runtime = std::env::var("OTZARIA_ONNX_RUNTIME").expect("ONNX Runtime shared library");
    let graph_sha = format!(
        "{:x}",
        Sha256::digest(std::fs::read(model.join("model.onnx")).unwrap())
    );
    assert_eq!(
        graph_sha,
        "c0a9ca9d764ae40ebba313150305702fd82832312ae9e11a7eb449e5a388f090"
    );
    let identity = std::fs::read_to_string(model.join("model.json")).unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&identity).unwrap();
    assert_eq!(metadata["family_id"], "otzaria/Pardes-QA491-int8-384");
    assert_eq!(metadata["embedding_dim"], 384);
    assert_eq!(metadata["query_max_tokens"], 128);
    assert_eq!(metadata["max_tokens"], 512);
    let root = tempfile::TempDir::new().unwrap();
    let index = root.path().join("index");
    std::fs::create_dir_all(&index).unwrap();
    let package = root.path().join("package");
    let vectors = root.path().join("vectors");
    let passage = "דיני מוקצה בשבת נאמרו בכלים ובחפצים שדרכם להשתמש בהם לצורך גופם או מקומם בבית";
    let mut engine = SearchEngine::new(index.to_str().unwrap());
    engine.add_text_book(
        "מקור ניסוי".into(), "/הלכה".into(), "/fixture/pardes.txt".into(),
        format!("פתיחה ארוכה ובה פרטים נוספים שאינם נוגעים לשאלה כלל. {passage}. סוף המקור דן בתפילה ובברכות ובסדר הקריאה ביום טוב\nמקור שני בענין השבת אבידה וקנינים והלכות ממונות"),
        None, TextStorage::InIndex, None, None,
    ).unwrap();
    engine.commit().unwrap();
    drop(engine);
    let built = Command::new(env!("CARGO_BIN_EXE_build_semantic_artifact"))
        .args([
            "--index",
            index.to_str().unwrap(),
            "--model",
            model.join("model.json").to_str().unwrap(),
            "--model-file",
            model.join("model.onnx").to_str().unwrap(),
            "--chunking",
            model.join("chunking.json").to_str().unwrap(),
            "--library-version",
            "37",
            "--release-tag",
            "v37-20261105163520",
            "--out",
            package.to_str().unwrap(),
            "--batch",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    let manifest = std::fs::read_to_string(package.join("release.json")).unwrap();
    let manifest_sha = format!("{:x}", Sha256::digest(manifest.as_bytes()));
    let mut engine = SearchEngine::new(index.to_str().unwrap());
    let cancel = SemanticCancellationToken::new();
    engine
        .install_semantic_vectors(
            SemanticVectorsInstallInput {
                vectors_dir: vectors.to_string_lossy().into_owned(),
                segment_path: package.join("segment.oxv").to_string_lossy().into_owned(),
                manifest_json: manifest,
                published_manifest_sha256: Some(manifest_sha),
                model_identity_json: identity.clone(),
            },
            &cancel,
        )
        .unwrap();
    let input = || SemanticArtifactInput {
        vectors_dir: vectors.to_string_lossy().into_owned(),
        model_path: model.join("model.onnx").to_string_lossy().into_owned(),
        model_identity_json: identity.clone(),
        onnx_runtime_path: Some(runtime.clone()),
        scan_threads: Some(2),
    };
    for iteration in 0..2 {
        let status = engine.open_semantic_artifact(input()).unwrap();
        assert!(status.available);
        let response = engine
            .search_semantic(
                "מהם דיני מוקצה בשבת בכלים לצורך גופם ומקומם?".into(),
                vec!["/הלכה".into()],
                20,
                0,
                SemanticLexicalMode::Exact,
                0,
                SemanticRetrievalMode::SemanticOnly,
                None,
                false,
                false,
                None,
                &cancel,
            )
            .unwrap();
        assert!(
            response.semantic_available,
            "{:?}",
            response.fallback_reason
        );
        assert!(
            response.fallback_reason.is_none(),
            "{:?}",
            response.fallback_reason
        );
        let hit = response
            .results
            .iter()
            .find(|hit| hit.segment == 0)
            .expect("actual vector resolves to original first line");
        let highlights = engine
            .semantic_passage_highlights(
                passage.into(),
                vec![SemanticHighlightTarget {
                    file_path: hit.file_path.clone(),
                    id: hit.id,
                    window: hit.matched_window,
                }],
                &cancel,
            )
            .unwrap();
        assert!(highlights[0].is_highlighted);
        assert!(highlights[0].span_score.is_some_and(f32::is_finite));
        assert!(highlights[0].snippet_html.contains("<mark>"));
        assert!(!highlights[0].snippet_html.contains("query:"));
        println!("actual compact Pardes public API pass {iteration}:384D, query128/doc512 identity, install/query/filter/highlight/reopen");
        engine.disable_semantic();
    }
}
