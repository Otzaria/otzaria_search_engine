import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

import 'native_library.dart';

/// Drives the semantic API across the real FFI boundary, against a natively
/// built engine.
///
/// The Rust suites cover the sidecar's behaviour; what only Dart can prove is
/// that the bridge itself is wired: that the generated dispatcher ids reach the
/// right functions, that `semanticStatus()` really is asynchronous now, and that
/// `SemanticSearchResponse` — an envelope of nested structs, enums, options and
/// a `BigInt` — decodes on this side of the wire. A regression in any of those
/// is invisible to `cargo test`.
///
/// The first group configures no sidecar, so its expectations hold whether the
/// library was built with or without the semantic feature: the fallback
/// contract is the same either way. The second drives a configured one, which
/// is the only way to prove that `SemanticConfigInput` and `SemanticBookInput`
/// cross *into* Rust correctly and that semantic scores come back.
Future<void> main() async {
  final skipReason = await initNativeEngine();
  final sidecarSkipReason = skipReason ?? await semanticSidecarSkipReason();

  group('semantic FFI', () {
    late Directory indexDir;
    late SearchEngine engine;

    setUp(() async {
      indexDir = Directory.systemTemp.createTempSync('otzaria_ffi_test');
      engine = SearchEngine(path: indexDir.path);
      await engine.addDocument(
        id: BigInt.from(41),
        title: 'בראשית',
        reference: 'בראשית א:א',
        topics: '/תורה',
        text: 'בראשית ברא אלהים את השמים ואת הארץ',
        segment: BigInt.from(2),
        isPdf: false,
        filePath: '/library/bereshit.json',
        sectionId: BigInt.from(7),
      );
      await engine.commit();
    });

    tearDown(() {
      // The engine still holds mmapped segment files on Windows, so a failed
      // cleanup must not fail the test — the OS temp directory is not ours to
      // guarantee.
      try {
        indexDir.deleteSync(recursive: true);
      } on FileSystemException {
        // Left for the OS to reclaim.
      }
    });

    test(
      'semanticStatus is awaited and reports an explicit disabled state',
      () async {
        // Previously `#[frb(sync)]`; if it were still synchronous this would not
        // return a Future at all and the analyzer would reject the await.
        final status = await engine.semanticStatus();

        expect(status.enabled, isFalse);
        expect(status.available, isFalse);
        expect(status.lastError, isNotNull);
        expect(status.vectorsPersisted, isFalse);
      },
    );

    test('a hybrid request falls back to painted lexical results', () async {
      final response = await engine.searchSemantic(
        query: 'בראשית ברא',
        facets: const [],
        limit: 10,
        offset: 0,
        lexicalMode: SemanticLexicalMode.exact,
        fuzzyMaxDistance: 0,
        retrievalMode: SemanticRetrievalMode.hybrid,
        matchNikud: false,
        matchTaamim: false,
      );

      expect(response.executedMode, SemanticExecutedMode.lexicalOnly);
      expect(response.requestedMode, SemanticRetrievalMode.hybrid);
      expect(response.semanticAvailable, isFalse);
      // A fallback always says why, so the UI can distinguish it from a choice.
      expect(response.fallbackReason, isNotNull);
      expect(response.results, hasLength(1));

      final hit = response.results.single;
      expect(hit.id, BigInt.from(41));
      expect(hit.source, SemanticResultSource.lexical);
      expect(hit.segment, BigInt.from(2));
      expect(hit.filePath, '/library/bereshit.json');
      // The display contract, decoded on the Dart side: painted markup, and a
      // flag that agrees with it.
      expect(hit.isHighlighted, isTrue);
      expect(hit.snippetHtml, contains('<font color=red>'));
      expect(hit.snippetHtml, contains('</font>'));
      expect(hit.mergedCount, 1);
      expect(hit.merged, isEmpty);
      expect(hit.lexicalScore, isNull);
      expect(hit.semanticScore, isNull);

      expect(response.lexicalTotalCount, 1);
      expect(response.totalCount, 1);
      expect(response.countsAreExact, isTrue);
      expect(response.candidateWindowTruncated, isFalse);
      expect(response.latencyMs, isA<BigInt>());
    });

    test('a semantic-only request never poses as a lexical result', () async {
      final response = await engine.searchSemantic(
        query: 'בראשית ברא',
        facets: const [],
        limit: 10,
        offset: 0,
        lexicalMode: SemanticLexicalMode.exact,
        fuzzyMaxDistance: 0,
        retrievalMode: SemanticRetrievalMode.semanticOnly,
        matchNikud: false,
        matchTaamim: false,
      );

      expect(response.executedMode, SemanticExecutedMode.semanticOnly);
      expect(response.results, isEmpty);
      // The lexical count still crosses honestly, but nothing is served as if
      // the semantic path had produced it.
      expect(response.lexicalTotalCount, 1);
      expect(response.totalCount, 0);
      expect(response.fallbackReason, isNotNull);
    });

    test('grouping and fuzzy options survive the round trip', () async {
      final response = await engine.searchSemantic(
        query: 'בראשי',
        facets: const [],
        limit: 5,
        offset: 0,
        lexicalMode: SemanticLexicalMode.fuzzy,
        fuzzyMaxDistance: 1,
        retrievalMode: SemanticRetrievalMode.hybrid,
        grouping: SemanticGroupingMode.sameSection,
        matchNikud: false,
        matchTaamim: false,
      );

      expect(response.executedMode, SemanticExecutedMode.lexicalOnly);
      expect(response.results, hasLength(1));
      expect(response.groupCount, 1);
      expect(response.results.single.id, BigInt.from(41));
    });

    test(
      'the index diff reports a disabled sidecar rather than failing',
      () async {
        final diff = await engine.semanticIndexDiff();

        expect(diff.enabled, isFalse);
        expect(diff.newBooks, isEmpty);
        expect(diff.changedBooks, isEmpty);
        expect(diff.removedBooks, isEmpty);
        expect(diff.modelMismatch, isFalse);
      },
    );

    test('the non-exclusive write operations cross the bridge', () async {
      // These take `&self` in Rust, so flutter_rust_bridge dispatches them
      // without an exclusive lock on the engine. Calling them concurrently with
      // a search is the property that matters; that it works at all is what a
      // wrong dispatcher id would break.
      final results = await Future.wait([
        engine.semanticIndexBooks(books: const []),
        engine.removeSemanticBooks(sourceBookKeys: const ['/nothing.json']),
        engine.resetSemanticIndex(),
        engine.semanticStatus(),
      ]);

      expect((results[0] as SemanticIndexingSummary).enabled, isFalse);
      expect((results[1] as SemanticRemoveResult).enabled, isFalse);
      expect((results[2] as SemanticResetResult).enabled, isFalse);
      expect((results[3] as SemanticStatus).enabled, isFalse);
    });
  }, skip: skipReason ?? false);

  group('semantic FFI with a configured sidecar', () {
    const bookKey = '/library/bereshit.json';
    const text = 'בראשית ברא אלהים';
    final lineId = BigInt.from(9001);
    final sectionId = BigInt.from(42);

    late Directory root;
    late SearchEngine engine;

    setUp(() async {
      root = Directory.systemTemp.createTempSync('otzaria_ffi_mock');
      engine = SearchEngine(
        path: (Directory('${root.path}/tantivy')..createSync()).path,
      );
      await engine.addDocument(
        id: lineId,
        title: 'בראשית',
        reference: 'בראשית א:א',
        topics: '/תורה',
        text: text,
        segment: BigInt.from(2),
        isPdf: false,
        filePath: bookKey,
        sectionId: sectionId,
      );
      await engine.commit();

      final model = File('${root.path}/mock.gguf');
      writeStubGguf(model);
      final status = await engine.configureSemantic(
        config: SemanticConfigInput(
          rootDir: '${root.path}/semantic',
          modelPath: model.path,
          modelId: 'test-mock',
          embeddingDim: 64,
        ),
      );
      expect(status.enabled, isTrue, reason: 'the sidecar should be open');

      final indexed = await engine.semanticIndexBooks(
        books: [
          SemanticBookInput(
            sourceBookKey: bookKey,
            title: 'בראשית',
            contentFingerprint: BigInt.from(123),
            isPdf: false,
            topics: '/תורה',
            extraFacets: const [],
            lines: [
              SemanticBookLineInput(
                lineId: lineId,
                sectionId: sectionId,
                text: text,
                lineHash: BigInt.from(1001),
                reference: 'בראשית א:א',
                segment: BigInt.from(2),
              ),
            ],
          ),
        ],
      );
      expect(indexed.enabled, isTrue);
      expect(indexed.booksIndexed, 1);
      expect(indexed.chunksWritten, greaterThan(0));
    });

    tearDown(() {
      try {
        root.deleteSync(recursive: true);
      } on FileSystemException {
        // Left for the OS to reclaim.
      }
    });

    test('the configured sidecar reports itself across the bridge', () async {
      final status = await engine.semanticStatus();

      expect(status.enabled, isTrue);
      expect(status.available, isTrue);
      expect(status.modelId, 'test-mock');
      expect(status.embeddingDim, 64);
      expect(status.indexedBookCount, 1);
      expect(status.vectorCount, greaterThan(0));
      // The in-memory store is the documented contract the app has to honour.
      expect(status.vectorsPersisted, isFalse);
    });

    test('a hybrid search really fuses both halves', () async {
      final response = await engine.searchSemantic(
        query: 'בראשית ברא',
        facets: const [],
        limit: 10,
        offset: 0,
        lexicalMode: SemanticLexicalMode.exact,
        fuzzyMaxDistance: 0,
        retrievalMode: SemanticRetrievalMode.hybrid,
        matchNikud: false,
        matchTaamim: false,
      );

      expect(response.executedMode, SemanticExecutedMode.hybrid);
      expect(response.semanticAvailable, isTrue);
      expect(response.fallbackReason, isNull);
      expect(response.results, hasLength(1));

      final hit = response.results.single;
      expect(hit.id, lineId);
      // Only the BM25 half can populate this, and it survived fusion.
      expect(hit.lexicalScore, isNotNull);
      expect(
        hit.source,
        anyOf(SemanticResultSource.lexical, SemanticResultSource.both),
      );
      expect(hit.isHighlighted, isTrue);
      expect(hit.snippetHtml, contains('<font color=red>'));
    });

    test('a semantic-only search returns a hydrated hit', () async {
      final response = await engine.searchSemantic(
        query: 'בראשית ברא',
        facets: const [],
        limit: 10,
        offset: 0,
        lexicalMode: SemanticLexicalMode.exact,
        fuzzyMaxDistance: 0,
        retrievalMode: SemanticRetrievalMode.semanticOnly,
        matchNikud: false,
        matchTaamim: false,
      );

      expect(response.executedMode, SemanticExecutedMode.semanticOnly);
      expect(response.semanticAvailable, isTrue);
      expect(response.results, hasLength(1));

      final hit = response.results.single;
      expect(hit.id, lineId);
      // A double the Rust side computed, decoded on this side of the wire.
      expect(hit.semanticScore, isNotNull);
      expect(hit.needsHydration, isFalse);
      // Hydration pulled the row from Tantivy, and nothing claims a lexical
      // match the query never made.
      expect(hit.snippetHtml, text);
      expect(hit.isHighlighted, isFalse);
    });

    test('the index diff sees the indexed book', () async {
      final diff = await engine.semanticIndexDiff();

      expect(diff.enabled, isTrue);
      expect(diff.newBooks, isEmpty);
      expect(diff.changedBooks, isEmpty);
      expect(diff.modelMismatch, isFalse);
    });

    test('removing the book empties the semantic index', () async {
      final removed = await engine.removeSemanticBooks(
        sourceBookKeys: const [bookKey],
      );

      expect(removed.enabled, isTrue);
      expect(removed.vectorsRemoved, greaterThan(0));
      expect((await engine.semanticStatus()).indexedBookCount, 0);
    });
  }, skip: sidecarSkipReason ?? false);
}
