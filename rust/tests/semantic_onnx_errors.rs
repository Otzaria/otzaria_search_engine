//! The failures only a build with the ONNX backend and nothing else can show, each by its
//! `SemanticErrorKind`: a GGUF model, which nothing in the build serves, and an ONNX model
//! whose ONNX Runtime is missing or unusable.
//!
//! Neither needs the real model, nor any ONNX Runtime at all, so these run in every
//! `semantic-onnx` job rather than behind `--ignored` with `tests/semantic_onnx_model.rs`.
//! The stand-in must be out of the build, because it serves both formats and would load
//! either model.
//!
//! The ONNX model is a stub package: a graph that passes the sidecar's structural checks and
//! that no runtime could run, and a tokenizer that loads. Loading stops at the runtime, which
//! is what is being tested, before the graph is ever handed to one. The runtime's path is the
//! one the application passes, `onnx_runtime_path`; else the `OTZARIA_ONNX_RUNTIME` variable;
//! else the file beside the graph. The tests that pass a path test it wherever they run, since
//! a passed path is the only place looked. The others place a file beside the graph, and with
//! the variable set they cannot tell which runtime they are testing, so they skip, loudly.

#![cfg(all(feature = "semantic-onnx", not(feature = "semantic-mock")))]

use search_engine::api::search_engine::{
    SearchEngine, SemanticBookInput, SemanticBookLineInput, SemanticConfigInput, SemanticError,
    SemanticErrorKind,
};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Read by the sidecar, not by these tests: see the module documentation.
const RUNTIME_ENV: &str = "OTZARIA_ONNX_RUNTIME";

/// The platform's ONNX Runtime file name, as the sidecar looks for it beside the graph.
#[cfg(target_os = "macos")]
const RUNTIME_FILE_NAME: &str = "libonnxruntime.dylib";
#[cfg(target_os = "linux")]
const RUNTIME_FILE_NAME: &str = "libonnxruntime.so";
#[cfg(target_os = "windows")]
const RUNTIME_FILE_NAME: &str = "onnxruntime.dll";

/// Just enough of the protobuf wire format for one ONNX graph, written apart from the
/// sidecar's own fixture encoder (which only its stand-in's builds compile) and following
/// the same field numbers of `onnx/onnx.proto3`.
mod proto {
    fn varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let low = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(low);
                return;
            }
            out.push(low | 0x80);
        }
    }

    pub fn uint(out: &mut Vec<u8>, field: u32, value: u64) {
        varint(out, u64::from(field) << 3);
        varint(out, value);
    }

    pub fn bytes(out: &mut Vec<u8>, field: u32, payload: &[u8]) {
        varint(out, (u64::from(field) << 3) | 2);
        varint(out, payload.len() as u64);
        out.extend_from_slice(payload);
    }

    /// A `ValueInfoProto` for a tensor of `elem_type`, its dimensions fixed (`Ok`) or named
    /// (`Err`).
    pub fn value_info(name: &str, elem_type: u64, dims: &[Result<u64, &str>]) -> Vec<u8> {
        let mut shape = Vec::new();
        for dim in dims {
            let mut dimension = Vec::new();
            match dim {
                Ok(value) => uint(&mut dimension, 1, *value),
                Err(param) => bytes(&mut dimension, 2, param.as_bytes()),
            }
            bytes(&mut shape, 1, &dimension);
        }
        let mut tensor_type = Vec::new();
        uint(&mut tensor_type, 1, elem_type);
        bytes(&mut tensor_type, 2, &shape);
        let mut type_proto = Vec::new();
        bytes(&mut type_proto, 1, &tensor_type);
        let mut value_info = Vec::new();
        bytes(&mut value_info, 1, name.as_bytes());
        bytes(&mut value_info, 2, &type_proto);
        value_info
    }
}

/// A sentence encoder's interface and nothing behind it: IR 8, opset 17, `input_ids` and
/// `attention_mask` (`int64[1, sequence_length]`) in, `sentence_embedding` (`float[1, 8]`)
/// out, and no nodes. The sidecar's structural checks pass it; no runtime would run it.
fn stub_graph() -> Vec<u8> {
    const FLOAT: u64 = 1;
    const INT64: u64 = 7;
    let mut graph = Vec::new();
    proto::bytes(&mut graph, 2, b"otzaria-stub-encoder");
    for input in ["input_ids", "attention_mask"] {
        let value = proto::value_info(input, INT64, &[Ok(1), Err("sequence_length")]);
        proto::bytes(&mut graph, 11, &value);
    }
    let output = proto::value_info("sentence_embedding", FLOAT, &[Ok(1), Ok(8)]);
    proto::bytes(&mut graph, 12, &output);

    let mut opset = Vec::new();
    proto::bytes(&mut opset, 1, b"");
    proto::uint(&mut opset, 2, 17);

    let mut model = Vec::new();
    proto::uint(&mut model, 1, 8);
    proto::bytes(&mut model, 2, b"otzaria-stub");
    proto::bytes(&mut model, 7, &graph);
    proto::bytes(&mut model, 8, &opset);
    model
}

/// A word-level Hugging Face tokenizer: enough for the backend to load it and configure
/// truncation, which happens before the runtime is looked for.
const STUB_TOKENIZER_JSON: &str = r#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,"pre_tokenizer":{"type":"Whitespace"},"post_processor":null,"decoder":null,"model":{"type":"WordLevel","vocab":{"[UNK]":0,"[CLS]":1,"[SEP]":2,"[QUERY]":3,"[PASSAGE]":4},"unk_token":"[UNK]"}}"#;

/// The smallest file the sidecar accepts as a GGUF: a v3 header with one empty F32 tensor,
/// as the sidecar's stand-in fixture writes it.
fn write_stub_gguf(path: &Path) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"GGUF");
    bytes.extend_from_slice(&3u32.to_le_bytes()); // version
    bytes.extend_from_slice(&1u64.to_le_bytes()); // tensor_count
    bytes.extend_from_slice(&0u64.to_le_bytes()); // metadata_kv_count
    bytes.extend_from_slice(&1u64.to_le_bytes()); // tensor name length
    bytes.push(b'x');
    bytes.extend_from_slice(&1u32.to_le_bytes()); // one dimension
    bytes.extend_from_slice(&1u64.to_le_bytes()); // one element
    bytes.extend_from_slice(&0u32.to_le_bytes()); // F32
    bytes.extend_from_slice(&0u64.to_le_bytes()); // data offset
    while bytes.len() % 32 != 0 {
        bytes.push(0); // GGUF's default alignment
    }
    bytes.extend_from_slice(&0f32.to_le_bytes());
    std::fs::write(path, bytes).unwrap();
}

/// A lexical index with one line, and a development session configured over `config`.
/// Configuring loads no model, so it succeeds whatever the model is.
fn configured(root: &TempDir, config: SemanticConfigInput) -> SearchEngine {
    let index = root.path().join("tantivy");
    std::fs::create_dir_all(&index).unwrap();
    let mut engine = SearchEngine::new(index.to_str().unwrap());
    engine
        .add_document(
            1,
            "probe",
            "probe 1",
            "/probe",
            "שורה אחת שאפשר לחפש בה",
            1,
            false,
            "/probe.txt",
            Some(1),
            None,
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    let status = engine
        .configure_semantic(config)
        .expect("configuring loads no model");
    assert!(status.enabled && !status.available);
    engine
}

/// Index the one line, which loads the model, and return why that failed.
fn index_failure(engine: &SearchEngine) -> SemanticError {
    let indexed = engine.semantic_index_books(vec![SemanticBookInput {
        source_book_key: "/probe.txt".to_string(),
        title: "probe".to_string(),
        content_fingerprint: 1,
        is_pdf: false,
        topics: "/probe".to_string(),
        extra_facets: Vec::new(),
        lines: vec![SemanticBookLineInput {
            line_id: 1,
            section_id: 1,
            text: "שורה אחת שאפשר לחפש בה".to_string(),
            line_hash: 1,
            reference: "probe 1".to_string(),
            segment: 1,
        }],
    }]);
    match indexed {
        Ok(_) => panic!("the model cannot load, and indexing succeeded"),
        Err(error) => error,
    }
}

fn onnx_config(root: &TempDir, graph: &Path) -> SemanticConfigInput {
    SemanticConfigInput {
        root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
        model_path: graph.to_string_lossy().into_owned(),
        model_id: "onnx-stub".to_string(),
        embedding_dim: 8,
        pooling: "in-graph".to_string(),
        max_tokens: 256,
        model_quantization: "int8".to_string(),
        embedding_text_version: 2,
        onnx_runtime_path: None,
    }
}

/// The stub ONNX package in `dir`, and its graph's path.
fn stub_onnx_package(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let graph = dir.join("model.onnx");
    std::fs::write(&graph, stub_graph()).unwrap();
    std::fs::write(dir.join("tokenizer.json"), STUB_TOKENIZER_JSON).unwrap();
    graph
}

/// Whether these tests know where the sidecar will look for the runtime; see the module
/// documentation.
fn runtime_lookup_is_ours() -> bool {
    if std::env::var_os(RUNTIME_ENV).is_some() {
        println!(
            "SKIPPED: {RUNTIME_ENV} is set, so the runtime the sidecar loads is not the one \
             this test places beside the graph"
        );
        return false;
    }
    true
}

/// A build with only the ONNX backend has nothing for a GGUF: no file fixes that, which is
/// what sets it apart from a runtime that is missing.
#[test]
fn a_gguf_on_a_build_with_only_the_onnx_backend_is_backend_not_in_build() {
    let root = TempDir::new().unwrap();
    let model = root.path().join("model.gguf");
    write_stub_gguf(&model);
    let engine = configured(
        &root,
        SemanticConfigInput {
            root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
            model_path: model.to_string_lossy().into_owned(),
            model_id: "gguf-stub".to_string(),
            embedding_dim: 64,
            pooling: "last-token".to_string(),
            max_tokens: 512,
            model_quantization: "Q4_K_M".to_string(),
            embedding_text_version: 1,
            onnx_runtime_path: None,
        },
    );

    let error = index_failure(&engine);
    assert_eq!(
        error.kind,
        SemanticErrorKind::BackendNotInBuild,
        "{}",
        error.message
    );
    assert!(
        error
            .message
            .contains("No embedding backend is available in this build"),
        "{}",
        error.message
    );
}

/// No runtime where the sidecar looks: the fix is a file put in place.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[test]
fn an_onnx_model_with_no_runtime_to_load_is_onnx_runtime_missing() {
    if !runtime_lookup_is_ours() {
        return;
    }
    let root = TempDir::new().unwrap();
    let graph = stub_onnx_package(&root.path().join("model"));
    let engine = configured(&root, onnx_config(&root, &graph));

    let error = index_failure(&engine);
    assert_eq!(
        error.kind,
        SemanticErrorKind::OnnxRuntimeMissing,
        "{}",
        error.message
    );
    assert!(
        error.message.contains("ONNX Runtime could not be loaded")
            && error.message.contains(RUNTIME_ENV),
        "{}",
        error.message
    );
}

/// A file where the sidecar looks, and not a runtime: the fix is a different file, not one
/// put in place.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[test]
fn an_onnx_runtime_that_does_not_load_is_onnx_runtime_unusable() {
    if !runtime_lookup_is_ours() {
        return;
    }
    let root = TempDir::new().unwrap();
    let package = root.path().join("model");
    let graph = stub_onnx_package(&package);
    std::fs::write(package.join(RUNTIME_FILE_NAME), b"not a shared library").unwrap();
    let engine = configured(&root, onnx_config(&root, &graph));

    let error = index_failure(&engine);
    assert_eq!(
        error.kind,
        SemanticErrorKind::OnnxRuntimeUnusable,
        "{}",
        error.message
    );
    assert!(
        error.message.contains("ONNX Runtime could not be loaded")
            && error.message.contains(RUNTIME_FILE_NAME),
        "{}",
        error.message
    );
}

/// A path the application passes is the only place looked: one that names no file is a
/// missing runtime even with a file beside the graph, which the lookup without a path would
/// have found and called unusable. The message names the path and where it came from, which
/// is also what shows that the path reached the sidecar.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[test]
fn a_passed_runtime_path_that_names_no_file_is_onnx_runtime_missing() {
    let root = TempDir::new().unwrap();
    let package = root.path().join("model");
    let graph = stub_onnx_package(&package);
    std::fs::write(package.join(RUNTIME_FILE_NAME), b"not a shared library").unwrap();
    let passed = root.path().join("bundle").join(RUNTIME_FILE_NAME);
    let engine = configured(
        &root,
        SemanticConfigInput {
            onnx_runtime_path: Some(passed.to_string_lossy().into_owned()),
            ..onnx_config(&root, &graph)
        },
    );

    let error = index_failure(&engine);
    assert_eq!(
        error.kind,
        SemanticErrorKind::OnnxRuntimeMissing,
        "{}",
        error.message
    );
    assert!(
        error.message.contains("the application passed")
            && error.message.contains(&passed.display().to_string()),
        "{}",
        error.message
    );
}

/// A passed path that names a file that is not a runtime: unusable, named as the
/// application's, and refused before the runtime binding is ever handed it, so a later load
/// in this process is not affected.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[test]
fn a_passed_runtime_that_does_not_load_is_onnx_runtime_unusable() {
    let root = TempDir::new().unwrap();
    let graph = stub_onnx_package(&root.path().join("model"));
    let bundle = root.path().join("bundle");
    std::fs::create_dir_all(&bundle).unwrap();
    let passed = bundle.join(RUNTIME_FILE_NAME);
    std::fs::write(&passed, b"not a shared library").unwrap();
    let engine = configured(
        &root,
        SemanticConfigInput {
            onnx_runtime_path: Some(passed.to_string_lossy().into_owned()),
            ..onnx_config(&root, &graph)
        },
    );

    let error = index_failure(&engine);
    assert_eq!(
        error.kind,
        SemanticErrorKind::OnnxRuntimeUnusable,
        "{}",
        error.message
    );
    assert!(
        error.message.contains("(passed by the application)")
            && error.message.contains(RUNTIME_FILE_NAME),
        "{}",
        error.message
    );
}
