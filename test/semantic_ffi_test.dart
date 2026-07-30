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
/// No sidecar is configured here, so these expectations hold whether the library
/// was built with or without the semantic feature: the fallback contract is the
/// same either way.
Future<void> main() async {
  final skipReason = await initNativeEngine();

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
}
