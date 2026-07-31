import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter_rust_bridge/flutter_rust_bridge.dart';
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

/// The smallest file the sidecar accepts as a model: a GGUF v3 header with one
/// empty F32 tensor. Mirrors `write_stub_gguf` in the sidecar's mock backend.
void writeStubGguf(File file) {
  Uint8List le32(int value) =>
      (ByteData(4)..setUint32(0, value, Endian.little)).buffer.asUint8List();
  Uint8List le64(int value) =>
      (ByteData(8)..setUint64(0, value, Endian.little)).buffer.asUint8List();

  final out = BytesBuilder();
  out.add(const AsciiEncoder().convert('GGUF'));
  out.add(le32(3)); // version
  out.add(le64(1)); // tensor_count
  out.add(le64(0)); // metadata_kv_count
  out.add(le64(1)); // tensor name length
  out.addByte(0x78); // 'x'
  out.add(le32(1)); // dimension count
  out.add(le64(1)); // one element
  out.add(le32(0)); // F32
  out.add(le64(0)); // data offset
  while (out.length % 32 != 0) {
    out.addByte(0); // GGUF default alignment
  }
  out.add(Uint8List(4)); // the single 0.0f element

  file.writeAsBytesSync(out.takeBytes());
}

/// Returns why the sidecar round-trip cannot run (`null` when it can), by
/// making the library prove it: the probe indexes one line and then reads the
/// status back.
///
/// Nothing cheaper distinguishes the builds. `configureSemantic` succeeds on a
/// library with no embedding backend compiled in — the state 32-bit ARM ships —
/// and `enabled` is `true` there too. `available` is the flag that separates
/// them, but the model loads lazily, so immediately after configuring it is
/// `false` on a *working* build as well:
///
/// | after | mock build | backend-less build |
/// | --- | --- | --- |
/// | `configureSemantic` | `available: false` | `available: false` |
/// | `semanticIndexBooks` | `available: true`, `mock-hash-v1` | throws |
///
/// Checking `available` before indexing would therefore skip the whole suite on
/// a build that can run it perfectly well.
///
/// The backend must be the mock: these tests assert the deterministic vectors
/// it produces, and the stub GGUF is not a model a real backend could load.
///
/// Same rule as [initNativeEngine]: skipping is a local convenience, never a
/// CI one.
Future<String?> semanticSidecarSkipReason() async {
  final probe = Directory.systemTemp.createTempSync('otzaria_ffi_probe');
  try {
    final engine = SearchEngine(
      path: (Directory('${probe.path}/tantivy')..createSync()).path,
    );
    final model = File('${probe.path}/mock.gguf');
    writeStubGguf(model);

    String? failure;
    try {
      await engine.configureSemantic(
        config: SemanticConfigInput(
          rootDir: '${probe.path}/semantic',
          modelPath: model.path,
          modelId: 'probe',
          embeddingDim: 64,
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
    } on AnyhowException catch (error) {
      failure = error.message.trim();
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
