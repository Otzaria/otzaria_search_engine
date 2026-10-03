import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

/// Locates and initializes the native engine, returning the reason it could not
/// be loaded (`null` on success) so a suite can pass it straight to `skip:`.
///
/// Skipping is a local convenience: a contributor who has not run
/// `cargo build` in `rust/` should still be able to run the pure-Dart suites.
/// It must never be a CI convenience — a job whose every FFI test silently
/// skipped reports green while proving nothing about the bridge. So when
/// `OTZARIA_REQUIRE_NATIVE` is set, a missing library fails instead.
Future<String?> initNativeEngine() async {
  const candidates = [
    'rust/target/debug/search_engine.dll',
    'rust/target/release/search_engine.dll',
    'rust/target/debug/libsearch_engine.so',
    'rust/target/release/libsearch_engine.so',
    'rust/target/debug/libsearch_engine.dylib',
    'rust/target/release/libsearch_engine.dylib',
  ];
  for (final path in candidates) {
    if (File(path).existsSync()) {
      await RustLib.init(externalLibrary: ExternalLibrary.open(path));
      return null;
    }
  }

  const message =
      'ספריית המנוע הנייטיבית לא נמצאה — הריצו cargo build בתיקיית rust';
  if (Platform.environment.containsKey('OTZARIA_REQUIRE_NATIVE')) {
    fail(
      '$message\n'
      'OTZARIA_REQUIRE_NATIVE is set, so skipping the FFI suites is not an '
      'option: a green run that tested nothing is worse than a red one. '
      'Searched: ${candidates.join(', ')}',
    );
  }
  return message;
}

/// Just enough of the protobuf wire format to write the stub graph: varints,
/// and length-delimited fields.
final class _Protobuf {
  final _out = BytesBuilder();

  void _varint(int value) {
    while (value >= 0x80) {
      _out.addByte((value & 0x7f) | 0x80);
      value >>= 7;
    }
    _out.addByte(value);
  }

  void uint(int field, int value) {
    _varint(field << 3);
    _varint(value);
  }

  void bytes(int field, List<int> payload) {
    _varint((field << 3) | 2);
    _varint(payload.length);
    _out.add(payload);
  }

  void string(int field, String value) => bytes(field, utf8.encode(value));

  Uint8List take() => _out.takeBytes();
}

/// An ONNX `ValueInfoProto` naming a tensor of [elemType] whose [dims] are each
/// a size (`int`) or a symbolic name (`String`).
Uint8List _valueInfo(String name, int elemType, List<Object> dims) {
  final shape = _Protobuf();
  for (final dim in dims) {
    final dimension = _Protobuf();
    switch (dim) {
      case int size:
        dimension.uint(1, size);
      case String param:
        dimension.string(2, param);
    }
    shape.bytes(1, dimension.take());
  }
  final tensorType = _Protobuf()
    ..uint(1, elemType)
    ..bytes(2, shape.take());
  final type = _Protobuf()..bytes(1, tensorType.take());
  final valueInfo = _Protobuf()
    ..string(1, name)
    ..bytes(2, type.take());
  return valueInfo.take();
}

/// The `tokenizer.json` of the stub package: the sidecar's `STUB_TOKENIZER_JSON`.
const _stubTokenizerJson =
    '{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],'
    '"normalizer":null,"pre_tokenizer":{"type":"Whitespace"},'
    '"post_processor":null,"decoder":null,"model":{"type":"WordLevel",'
    '"vocab":{"[UNK]":0,"[CLS]":1,"[SEP]":2,"[QUERY]":3,"[PASSAGE]":4},'
    '"unk_token":"[UNK]"}}';

/// The smallest model the sidecar accepts: an ONNX package in [dir], the graph
/// `model.onnx` and its `tokenizer.json`, whose graph is returned. Mirrors
/// `write_stub_onnx_package` in the sidecar's mock backend: IR 8, opset 17, the
/// inputs `input_ids` and `attention_mask` (`int64[1, sequence_length]`) and
/// the output `sentence_embedding` (`float[1, 8]`). It has no nodes, so no
/// runtime could run it; only the stand-in serves it.
File writeStubOnnxPackage(Directory dir) {
  const int64 = 7;
  const float = 1;
  final inputs = [
    for (final name in const ['input_ids', 'attention_mask'])
      _valueInfo(name, int64, const [1, 'sequence_length']),
  ];
  final output = _valueInfo('sentence_embedding', float, const [1, 8]);
  final graph = _Protobuf()..string(2, 'otzaria-stub-encoder');
  for (final input in inputs) {
    graph.bytes(11, input);
  }
  graph.bytes(12, output);
  final opset = _Protobuf()
    ..string(1, '')
    ..uint(2, 17);
  final model = _Protobuf()
    ..uint(1, 8)
    ..string(2, 'otzaria-stub')
    ..bytes(7, graph.take())
    ..bytes(8, opset.take());

  dir.createSync(recursive: true);
  final file = File('${dir.path}/model.onnx')..writeAsBytesSync(model.take());
  File('${dir.path}/tokenizer.json').writeAsStringSync(_stubTokenizerJson);
  return file;
}

/// The sidecar's checksum of the ONNX package whose graph is [graph], as a
/// model identity's `query_packages` names it: the SHA-256 of its manifest,
/// `otzaria-onnx-package-v1` and then a line of name, size and SHA-256 for each
/// file, in name order. A package without external data, as the stub is, holds
/// the graph and `tokenizer.json`.
String onnxPackageChecksum(File graph) {
  final files = [graph, File('${graph.parent.path}/tokenizer.json')];
  final named = {
    for (final file in files)
      file.uri.pathSegments.last: file.readAsBytesSync(),
  };
  final manifest = StringBuffer('otzaria-onnx-package-v1\n');
  for (final name in named.keys.toList()..sort()) {
    final bytes = named[name]!;
    manifest.write('$name\t${bytes.length}\t${sha256.convert(bytes)}\n');
  }
  return sha256.convert(utf8.encode(manifest.toString())).toString();
}

/// The sidecar's `tokenizer_checksum` for the ONNX package whose graph is
/// [graph]: the SHA-256 of the `tokenizer.json` beside it.
String onnxTokenizerChecksum(File graph) => sha256
    .convert(File('${graph.parent.path}/tokenizer.json').readAsBytesSync())
    .toString();

/// The configuration the FFI suites open the sidecar with: the stub graph at
/// [modelPath], under the pooling the stand-in claims for an ONNX graph, and
/// text recipe 1, so that a query that is a line's exact text embeds as the
/// line does. A suite that must see a value arrive, or be refused, passes it
/// instead of the default.
SemanticConfigInput stubOnnxConfig({
  required String rootDir,
  required String modelPath,
  required String modelId,
  String pooling = 'in-graph',
  int maxTokens = 512,
  String modelQuantization = 'int8',
  int embeddingTextVersion = 1,
  String? onnxRuntimePath,
}) => SemanticConfigInput(
  rootDir: rootDir,
  modelPath: modelPath,
  modelId: modelId,
  embeddingDim: 64,
  pooling: pooling,
  maxTokens: maxTokens,
  modelQuantization: modelQuantization,
  embeddingTextVersion: embeddingTextVersion,
  onnxRuntimePath: onnxRuntimePath,
);

/// Returns why the sidecar round-trip cannot run (`null` when it can), by
/// making the library prove it: the probe indexes one line and then reads the
/// status back.
///
/// Nothing cheaper distinguishes the builds. `configureSemantic` succeeds on a
/// library with no embedding backend compiled in — the state Android and iOS
/// ship — and `enabled` is `true` there too. `available` is the flag that
/// separates them, but the model loads lazily, so immediately after
/// configuring it is `false` on a *working* build as well:
///
/// | after | mock build | backend-less build |
/// | --- | --- | --- |
/// | `configureSemantic` | `available: false` | `available: false` |
/// | `semanticIndexBooks` | `available: true`, `mock-hash-v1` | throws `backendNotInBuild` |
///
/// Checking `available` before indexing would therefore skip the whole suite on
/// a build that can run it perfectly well.
///
/// The backend must be the mock: these tests assert the deterministic vectors
/// it produces, and the stub graph is not a model a real backend could load.
///
/// Same rule as [initNativeEngine]: skipping is a local convenience, never a
/// CI one.
Future<String?> semanticSidecarSkipReason() async {
  final probe = Directory.systemTemp.createTempSync('otzaria_ffi_probe');
  try {
    final engine = await SearchEngine.newInstance(
      path: (Directory('${probe.path}/tantivy')..createSync()).path,
    );
    final model = writeStubOnnxPackage(Directory('${probe.path}/model'));

    String? failure;
    try {
      await engine.configureSemantic(
        config: stubOnnxConfig(
          rootDir: '${probe.path}/semantic',
          modelPath: model.path,
          modelId: 'probe',
        ),
      );
      await engine.semanticIndexBooks(
        books: [
          SemanticBookInput(
            sourceBookKey: '/probe.json',
            title: 'probe',
            contentFingerprint: BigInt.one,
            isPdf: false,
            topics: '/probe',
            extraFacets: const [],
            lines: [
              SemanticBookLineInput(
                lineId: BigInt.one,
                sectionId: BigInt.one,
                text: 'בראשית ברא אלהים',
                lineHash: BigInt.one,
                reference: 'probe',
                segment: BigInt.one,
              ),
            ],
          ),
        ],
      );
      final status = await engine.semanticStatus();
      if (status.available && status.embeddingBackend == MockBackend.id) {
        return null;
      }
      failure =
          'available=${status.available} '
          'embeddingBackend=${status.embeddingBackend}';
    } on SemanticError catch (error) {
      failure = error.toString();
    }

    const message =
        'הספרייה הנייטיבית נבנתה ללא ${MockBackend.id} — הריצו '
        'cargo build --features semantic-mock בתיקיית rust';
    if (Platform.environment.containsKey('OTZARIA_REQUIRE_NATIVE')) {
      fail(
        '$message\n'
        'OTZARIA_REQUIRE_NATIVE is set, so the semantic round trip may not be '
        'skipped: the fallback suites alone prove nothing about indexing or '
        'hybrid retrieval across the bridge. Reported: $failure',
      );
    }
    return message;
  } finally {
    try {
      probe.deleteSync(recursive: true);
    } on FileSystemException {
      // Left for the OS to reclaim.
    }
  }
}

/// The sidecar's deterministic stand-in, the only backend these tests accept.
abstract final class MockBackend {
  static const id = 'mock-hash-v1';
}

/// The build machine's artifact builder, compiled beside the library the suites
/// load (`cargo build --features semantic-mock --bin build_semantic_artifact`),
/// or `null` when it is not there.
///
/// A device never builds an artifact; the suite needs one to open, and the
/// binary is how the release pipeline makes it, so it stands in for the build
/// machine. As with the library itself, a missing binary skips the suite
/// locally and fails it under `OTZARIA_REQUIRE_NATIVE`.
File? findArtifactBuilder() {
  final name = Platform.isWindows
      ? 'build_semantic_artifact.exe'
      : 'build_semantic_artifact';
  for (final dir in const ['rust/target/debug', 'rust/target/release']) {
    final file = File('$dir/$name');
    if (file.existsSync()) return file;
  }
  return null;
}

/// Why the prebuilt-artifact suite cannot run (`null` when it can).
String? artifactBuilderSkipReason() {
  if (findArtifactBuilder() != null) return null;
  const message =
      'build_semantic_artifact לא נמצא — הריצו cargo build --features '
      'semantic-mock --bin build_semantic_artifact בתיקיית rust';
  if (Platform.environment.containsKey('OTZARIA_REQUIRE_NATIVE')) {
    fail(
      '$message\n'
      'OTZARIA_REQUIRE_NATIVE is set, so the prebuilt-artifact suite may not be '
      'skipped: opening an artifact is the application\'s semantic path.',
    );
  }
  return message;
}
