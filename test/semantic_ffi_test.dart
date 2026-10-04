import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

import 'native_library.dart';

/// Drives the semantic API across the real FFI boundary, against a natively
/// built engine.
///
/// The Rust suites cover the sidecar's behaviour; what only Dart can prove is
/// that the bridge itself is wired: that the generated dispatcher ids reach the
/// right functions, that `semanticStatus()` really is asynchronous now, that
/// `SemanticSearchResponse` — an envelope of nested structs, enums, options and
/// a `BigInt` — decodes on this side of the wire, and that a search borrows its
/// `SemanticCancellationToken`, an opaque object, rather than moving it, so this
/// side can still cancel it. A regression in any of those is invisible to
/// `cargo test`.
///
/// The first group configures no sidecar, so its expectations hold whether the
/// library was built with or without the semantic feature: the fallback
/// contract is the same either way. The second drives a configured one, which
/// is the only way to prove that `SemanticConfigInput` and `SemanticBookInput`
/// cross *into* Rust correctly and that semantic scores come back. The third
/// opens a prebuilt artifact, which is the application's own semantic path.
///
/// A refused call throws a `SemanticError`, whose `kind` is the value an
/// application branches on; every refusal here is matched by its kind, and by
/// its message only where the message is what is being shown to cross.
Future<void> main() async {
  final skipReason = await initNativeEngine();
  final sidecarSkipReason = skipReason ?? await semanticSidecarSkipReason();
  final artifactSkipReason = sidecarSkipReason ?? artifactBuilderSkipReason();

  group('semantic FFI', () {
    late Directory indexDir;
    late SearchEngine engine;

    setUp(() async {
      indexDir = Directory.systemTemp.createTempSync('otzaria_ffi_test');
      engine = await SearchEngine.newInstance(path: indexDir.path);
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
        // Which of the two depends on the build, and each says so as a state
        // and as the kind of `lastError`.
        expect(status.errorKind, closedKind(status.state));
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
        cancellation: SemanticCancellationToken(),
      );

      expect(response.executedMode, SemanticExecutedMode.lexicalOnly);
      expect(response.requestedMode, SemanticRetrievalMode.hybrid);
      expect(response.semanticAvailable, isFalse);
      // A fallback always says why, so the UI can distinguish it from a choice,
      // and says it as a kind to branch on too.
      expect(response.fallbackReason, isNotNull);
      expect(response.fallbackKind, isIn(closedKinds));
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
        cancellation: SemanticCancellationToken(),
      );

      expect(response.executedMode, SemanticExecutedMode.semanticOnly);
      expect(response.results, isEmpty);
      // The lexical count still crosses honestly, but nothing is served as if
      // the semantic path had produced it.
      expect(response.lexicalTotalCount, 1);
      expect(response.totalCount, 0);
      expect(response.fallbackReason, isNotNull);
      expect(response.fallbackKind, isIn(closedKinds));
    });

    test(
      'a cancellation token is created, cancelled, and only borrowed',
      () async {
        Future<SemanticSearchResponse> search(
          SemanticCancellationToken token,
        ) => engine.searchSemantic(
          query: 'בראשית ברא',
          facets: const [],
          limit: 10,
          offset: 0,
          lexicalMode: SemanticLexicalMode.exact,
          fuzzyMaxDistance: 0,
          retrievalMode: SemanticRetrievalMode.hybrid,
          matchNikud: false,
          matchTaamim: false,
          cancellation: token,
        );

        final token = SemanticCancellationToken();
        expect(token.isCancelled, isFalse);
        // An uncancelled token changes nothing, and the search borrowed it
        // rather than taking it: the object is still usable on this side.
        expect((await search(token)).results, hasLength(1));
        expect(token.isCancelled, isFalse);

        token
          ..cancel()
          ..cancel();
        expect(token.isCancelled, isTrue);
        // Cancelled, the search throws by kind rather than falling back.
        await expectLater(
          search(token),
          throwsA(isSemanticError(SemanticErrorKind.cancelled)),
        );
        expect(token.isCancelled, isTrue);
        token.dispose();
      },
    );

    test('the ranking options default to the ranking of a search without', () {
      // The constructor's defaults are written in Dart, `defaults()` is read from
      // the engine: one value apart and they would rank differently.
      expect(const SemanticRankingOptions(), SemanticRankingOptions.defaults());
      expect(
        SemanticRankingOptions.defaults().fusionStrategy,
        SemanticFusionStrategy.rrf,
      );
      expect(SemanticRankingOptions.defaults().rrfK, 60);
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
        cancellation: SemanticCancellationToken(),
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
    // Not the recipe's 512, which is also the sidecar's own default: only a
    // value the sidecar would not have chosen shows that this one arrived.
    const maxTokens = 384;
    final lineId = BigInt.from(9001);
    final sectionId = BigInt.from(42);

    late Directory root;
    late SearchEngine engine;

    setUp(() async {
      root = Directory.systemTemp.createTempSync('otzaria_ffi_mock');
      engine = await SearchEngine.newInstance(
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

      final model = writeStubOnnxPackage(Directory('${root.path}/model'));
      final status = await engine.configureSemantic(
        config: stubOnnxConfig(
          rootDir: '${root.path}/semantic',
          modelPath: model.path,
          modelId: 'test-mock',
          maxTokens: maxTokens,
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
      expect(status.state, SemanticState.ready);
      expect(status.errorKind, isNull);
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
        cancellation: SemanticCancellationToken(),
      );

      expect(response.executedMode, SemanticExecutedMode.hybrid);
      expect(response.semanticAvailable, isTrue);
      expect(response.fallbackReason, isNull);
      expect(response.fallbackKind, isNull);
      expect(response.results, hasLength(1));

      final hit = response.results.single;
      expect(hit.id, lineId);
      // Both halves reached the fusion and both scores survived it. Accepting a
      // lexical-only source here would let the suite pass with the semantic
      // half silently contributing nothing.
      expect(hit.source, SemanticResultSource.both);
      expect(hit.lexicalScore, isNotNull);
      expect(hit.semanticScore, isNotNull);
      expect(hit.isHighlighted, isTrue);
      expect(hit.snippetHtml, contains('<font color=red>'));
    });

    test(
      'a restarted session flag crosses native FFI after cache eviction',
      () async {
        // More than one page makes both a resumed page and the replacement first
        // page observable. The configured sidecar stays open for every request.
        for (var n = 1; n <= 12; n++) {
          await engine.addDocument(
            id: lineId + BigInt.from(n),
            title: 'בראשית',
            reference: 'בראשית א:${n + 1}',
            topics: '/תורה',
            text: '$text עוד שורה $n',
            segment: BigInt.from(n + 2),
            isPdf: false,
            filePath: bookKey,
            sectionId: sectionId,
          );
        }
        await engine.commit();

        Future<SemanticSearchResponse> page(
          int offset, {
          String query = 'בראשית ברא',
        }) => engine.searchSemantic(
          query: query,
          facets: const [],
          limit: 2,
          offset: offset,
          lexicalMode: SemanticLexicalMode.exact,
          fuzzyMaxDistance: 0,
          retrievalMode: SemanticRetrievalMode.hybrid,
          matchNikud: false,
          matchTaamim: false,
          cancellation: SemanticCancellationToken(),
        );
        Object contents(SemanticSearchResponse response) => [
          for (final hit in response.results)
            (hit.filePath, hit.id, hit.fusedScore, hit.snippetHtml),
        ];

        final first = await page(0);
        final second = await page(2);
        expect(first.executedMode, SemanticExecutedMode.hybrid);
        expect(first.semanticAvailable, isTrue);
        expect(first.results, hasLength(2));
        expect(second.results, hasLength(2));
        expect(first.sessionRestarted, isFalse);
        expect(second.sessionRestarted, isFalse);
        expect(first.hasMore, isTrue);
        expect(
          second.results.map((hit) => hit.id),
          everyElement(isNot(isIn(first.results.map((hit) => hit.id)))),
        );

        // The LRU holds four sessions; five distinct queries necessarily evict
        // the original. Query, generation, grouping and page size stay unchanged.
        for (final query in ['תפילין', 'שבת', 'תשובה', 'מלך', 'גמרא']) {
          final other = await page(0, query: query);
          expect(other.semanticAvailable, isTrue);
        }
        final restarted = await page(2);
        expect(restarted.sessionRestarted, isTrue);
        expect(contents(restarted), contents(first));
        final resumed = await page(2);
        expect(resumed.sessionRestarted, isFalse);
        expect(contents(resumed), contents(second));
      },
    );

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
        cancellation: SemanticCancellationToken(),
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

    Future<SemanticSearchResponse> searchWith(
      SemanticCancellationToken token, {
      SemanticRetrievalMode mode = SemanticRetrievalMode.hybrid,
    }) => engine.searchSemantic(
      query: 'בראשית ברא',
      facets: const [],
      limit: 10,
      offset: 0,
      lexicalMode: SemanticLexicalMode.exact,
      fuzzyMaxDistance: 0,
      retrievalMode: mode,
      matchNikud: false,
      matchTaamim: false,
      cancellation: token,
    );

    test('ranking options cross into the ranking, and are checked', () async {
      Future<SemanticSearchResponse> ranked(SemanticRankingOptions? ranking) =>
          engine.searchSemantic(
            query: 'בראשית ברא',
            facets: const [],
            limit: 10,
            offset: 0,
            lexicalMode: SemanticLexicalMode.exact,
            fuzzyMaxDistance: 0,
            retrievalMode: SemanticRetrievalMode.hybrid,
            matchNikud: false,
            matchTaamim: false,
            ranking: ranking,
            cancellation: SemanticCancellationToken(),
          );

      final none = (await ranked(null)).results.single;
      final defaults = (await ranked(
        const SemanticRankingOptions(),
      )).results.single;
      expect(defaults.fusedScore, none.fusedScore);

      // Reciprocal rank fusion at k = 30: the one line, first on both sides,
      // scores 1 / 31 from each.
      final rrf = (await ranked(
        const SemanticRankingOptions(
          fusionStrategy: SemanticFusionStrategy.rrf,
          rrfK: 30,
        ),
      )).results.single;
      expect(rrf.source, SemanticResultSource.both);
      expect(rrf.fusedScore, closeTo(2 / 31, 1e-6));

      await expectLater(
        ranked(
          const SemanticRankingOptions(
            alphaByQueryType: SemanticQueryTypeAlphas(short: -0.2),
          ),
        ),
        throwsA(
          isSemanticError(
            SemanticErrorKind.invalidInput,
            field: 'alpha_by_query_type.short',
          ),
        ),
      );
      await expectLater(
        ranked(
          const SemanticRankingOptions(
            fusionStrategy: SemanticFusionStrategy.rrf,
            rrfK: 0,
          ),
        ),
        throwsA(
          isSemanticError(SemanticErrorKind.invalidInput, field: 'rrf_k'),
        ),
      );
    });

    test('a cancelled search throws cancelled in every mode', () async {
      final token = SemanticCancellationToken()..cancel();
      for (final mode in SemanticRetrievalMode.values) {
        await expectLater(
          searchWith(token, mode: mode),
          throwsA(isSemanticError(SemanticErrorKind.cancelled)),
          reason: '$mode: a cancel is not answered with lexical results',
        );
      }
      // The session serves the next search, whose token is not cancelled.
      final served = await searchWith(SemanticCancellationToken());
      expect(served.executedMode, SemanticExecutedMode.hybrid);
      expect(served.fallbackKind, isNull);
    });

    test('cancelling while the search runs returns at once', () async {
      final token = SemanticCancellationToken();
      final search = searchWith(token);
      // Synchronous, on the isolate that started the search, while the search
      // holds the token: it neither waits for the search nor finds the token
      // moved into it.
      token.cancel();
      expect(token.isCancelled, isTrue);
      try {
        // A search that passed its last look before the cancel is served.
        expect((await search).semanticAvailable, isTrue);
      } on SemanticError catch (error) {
        expect(error.kind, SemanticErrorKind.cancelled);
      }
    });

    test('every recipe field reaches the sidecar\'s manifest', () async {
      // The sidecar records the configuration it was opened with, so its
      // manifest shows what arrived on the Rust side. A field the codec lost or
      // misordered shows up here as the sidecar's default, or as a swap.
      final manifest =
          jsonDecode(
                File(
                  '${root.path}/semantic/semantic_manifest.json',
                ).readAsStringSync(),
              )
              as Map<String, dynamic>;

      expect(manifest['embedding_model_id'], 'test-mock');
      expect(manifest['embedding_dim'], 64);
      expect(manifest['pooling'], 'in-graph');
      expect(manifest['embedding_max_tokens'], maxTokens);
      expect(manifest['model_quantization'], 'int8');
    });

    test(
      'a recipe value the sidecar cannot serve is refused by name',
      () async {
        // The defaults name the one pooling the stand-in claims for a graph
        // and a text recipe the sidecar implements, so a value the sidecar
        // refuses is what shows that these two fields reach Rust.
        await engine.disableSemantic();
        final model = '${root.path}/model/model.onnx';
        final refused = {
          'last_token': stubOnnxConfig(
            rootDir: '${root.path}/semantic',
            modelPath: model,
            modelId: 'test-mock',
            pooling: 'last_token',
          ),
          'embedding_text_version': stubOnnxConfig(
            rootDir: '${root.path}/semantic',
            modelPath: model,
            modelId: 'test-mock',
            embeddingTextVersion: 99,
          ),
        };
        for (final MapEntry(key: named, value: config) in refused.entries) {
          await expectLater(
            engine.configureSemantic(config: config),
            throwsA(
              isSemanticError(
                SemanticErrorKind.invalidInput,
              ).having((error) => error.message, 'message', contains(named)),
            ),
          );
        }
      },
    );

    test(
      'a negative ONNX token cap arrives in the billions and is refused',
      () async {
        // maxTokens crosses as a u32, so -1 reaches Rust as 4294967295: the
        // case the ONNX cap's ceiling exists for, since the load-time probe of
        // that many tokens could not be allocated. It is refused while it is
        // still a configuration, naming the cap and the ceiling. Nothing is
        // loaded, so no graph has to exist at the path.
        await engine.disableSemantic();
        await expectLater(
          engine.configureSemantic(
            config: SemanticConfigInput(
              rootDir: '${root.path}/semantic-onnx',
              modelPath: '${root.path}/model.onnx',
              modelId: 'ArieLLL123/judaic-semantic-round2-onnx-zayit',
              embeddingDim: 256,
              pooling: 'in-graph',
              maxTokens: -1,
              modelQuantization: 'int8',
              embeddingTextVersion: 2,
            ),
          ),
          throwsA(
            isSemanticError(SemanticErrorKind.invalidInput).having(
              (error) => error.message,
              'message',
              allOf(
                contains('embedding_max_tokens is 4294967295'),
                contains('65536'),
              ),
            ),
          ),
        );
      },
    );

    test(
      'the runtime path reaches Rust and is an input of the session',
      () async {
        // The stand-in loads no runtime, so the path need not name a file. That
        // it arrived is shown by the session open without one refusing it as a
        // change, by name; an empty one is refused as an input.
        await expectLater(
          engine.configureSemantic(
            config: stubOnnxConfig(
              rootDir: '${root.path}/semantic',
              modelPath: '${root.path}/model/model.onnx',
              modelId: 'test-mock',
              maxTokens: maxTokens,
              onnxRuntimePath: '${root.path}/Frameworks/libonnxruntime.dylib',
            ),
          ),
          throwsA(
            isSemanticError(SemanticErrorKind.sessionConflict).having(
              (error) => error.message,
              'message',
              contains('onnx_runtime_path changed'),
            ),
          ),
        );
        await engine.disableSemantic();
        await expectLater(
          engine.configureSemantic(
            config: stubOnnxConfig(
              rootDir: '${root.path}/semantic',
              modelPath: '${root.path}/model/model.onnx',
              modelId: 'test-mock',
              onnxRuntimePath: '',
            ),
          ),
          throwsA(
            isSemanticError(
              SemanticErrorKind.invalidInput,
              field: 'onnx_runtime_path',
            ),
          ),
        );
      },
    );

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

  group('semantic FFI with a prebuilt artifact', () {
    const bookKey = '/books/genesis.txt';
    const probeLine = 'ויאמר אלהים יהי אור ויהי אור';
    // The chunking this build keys the index's lines under, Meivin Round 2's,
    // which the vector set's model identity has to name: `chunking.json` below,
    // and its `ChunkerConfig::identity()`.
    const chunkingIdentity = 2685558872390372738;

    late Directory root;
    late SearchEngine engine;
    late Map<String, Object> identity;
    late SemanticVectorsInstallReport installed;

    SemanticArtifactInput input(
      Map<String, Object> modelIdentity, {
      String? onnxRuntimePath,
    }) => SemanticArtifactInput(
      vectorsDir: '${root.path}/vectors',
      modelPath: '${root.path}/model/model.onnx',
      modelIdentityJson: jsonEncode(modelIdentity),
      onnxRuntimePath: onnxRuntimePath,
    );

    setUp(() async {
      root = Directory.systemTemp.createTempSync('otzaria_ffi_artifact');
      final index = Directory('${root.path}/tantivy')..createSync();
      engine = await SearchEngine.newInstance(path: index.path);
      await engine.addTextBook(
        title: 'בראשית',
        topics: '/מקרא/תורה',
        filePath: bookKey,
        catalogueOrder: 0,
        generationOrder: 0,
        text: 'בראשית ברא אלהים את השמים ואת הארץ\n$probeLine',
        textStorage: TextStorage.inIndex,
      );
      await engine.commit();

      // The build machine's half: the model's identity, the recipe, and the
      // base package built from this index; then the device's, installing it.
      final model = writeStubOnnxPackage(Directory('${root.path}/model'));
      identity = {
        'family_id': 'test-mock@0000000',
        'tokenizer_checksum': onnxTokenizerChecksum(model),
        'embedding_dim': 64,
        'pooling': 'in-graph',
        'max_tokens': 512,
        'embedding_text_version': 2,
        'normalization_version': 1,
        'chunking_identity': chunkingIdentity,
        'query_packages': [
          {'checksum': onnxPackageChecksum(model), 'quantization': 'int8'},
        ],
      };
      File('${root.path}/model.json').writeAsStringSync(jsonEncode(identity));
      File('${root.path}/chunking.json').writeAsStringSync(
        jsonEncode({
          'min_meaningful_chars': 20,
          'context_window_lines': 2,
          'max_chunk_chars': 512,
          'min_embeddable_chars': 5,
          'chunking_version': 1,
          'embedding_text_version': 2,
          'normalization_version': 1,
        }),
      );
      final built = await Process.run(findArtifactBuilder()!.path, [
        '--index',
        index.path,
        '--library-version',
        '1',
        '--release-tag',
        'otzaria-library-ffi',
        '--model',
        '${root.path}/model.json',
        '--model-file',
        model.path,
        '--chunking',
        '${root.path}/chunking.json',
        '--out',
        '${root.path}/package',
        '--created-at',
        '2026-10-01T00:00:00Z',
        '--allow-non-semantic',
      ]);
      expect(
        built.exitCode,
        0,
        reason: 'the build failed:\n${built.stdout}\n${built.stderr}',
      );
      // Published beside the release, as the build prints it.
      final digest = RegExp(
        r'Manifest SHA-256: ([0-9a-f]{64})',
      ).firstMatch(built.stdout as String)!.group(1);
      installed = await engine.installSemanticVectors(
        input: SemanticVectorsInstallInput(
          vectorsDir: '${root.path}/vectors',
          segmentPath: '${root.path}/package/segment.oxv',
          manifestJson: File(
            '${root.path}/package/release.json',
          ).readAsStringSync(),
          publishedManifestSha256: digest,
          modelIdentityJson: jsonEncode(identity),
        ),
        cancellation: SemanticCancellationToken(),
      );
    });

    tearDown(() {
      try {
        root.deleteSync(recursive: true);
      } on FileSystemException {
        // Left for the OS to reclaim.
      }
    });

    test('an opened vector set serves a hydrated semantic-only hit', () async {
      final status = await engine.openSemanticArtifact(config: input(identity));
      expect(status.enabled, isTrue);
      expect(status.available, isTrue, reason: status.lastError);
      expect(status.state, SemanticState.ready);
      expect(status.errorKind, isNull);
      expect(status.embeddingBackend, MockBackend.id);
      expect(status.vectorCount, 2);
      expect(status.vectorsPersisted, isTrue);

      final response = await engine.searchSemantic(
        query: probeLine,
        facets: const [],
        limit: 10,
        offset: 0,
        lexicalMode: SemanticLexicalMode.exact,
        fuzzyMaxDistance: 0,
        retrievalMode: SemanticRetrievalMode.semanticOnly,
        matchNikud: false,
        matchTaamim: false,
        cancellation: SemanticCancellationToken(),
      );
      expect(response.executedMode, SemanticExecutedMode.semanticOnly);
      expect(response.semanticAvailable, isTrue);
      expect(response.fallbackKind, isNull);
      final hit = response.results.first;
      expect(hit.source, SemanticResultSource.semantic);
      expect(hit.needsHydration, isFalse);
      expect(hit.snippetHtml, probeLine);
      expect(hit.filePath, bookKey);
    });

    test('a commit after opening leaves the vector set serving', () async {
      await engine.openSemanticArtifact(config: input(identity));
      await engine.addTextBook(
        title: 'נוסף',
        topics: '/אחר',
        filePath: '/books/another.txt',
        catalogueOrder: 1,
        generationOrder: 0,
        text: 'שורה שלא הייתה בספרייה כשהווקטורים נבנו ממנה',
        textStorage: TextStorage.inIndex,
      );
      await engine.commit();

      final status = await engine.semanticStatus();
      expect(status.state, SemanticState.ready);
      final response = await engine.searchSemantic(
        query: probeLine,
        facets: const [],
        limit: 10,
        offset: 0,
        lexicalMode: SemanticLexicalMode.exact,
        fuzzyMaxDistance: 0,
        retrievalMode: SemanticRetrievalMode.semanticOnly,
        matchNikud: false,
        matchTaamim: false,
        cancellation: SemanticCancellationToken(),
      );
      expect(response.semanticAvailable, isTrue);
      expect(response.results.first.snippetHtml, probeLine);
    });

    test(
      'the runtime path is compared when the artifact is opened again',
      () async {
        // No identity field reads the runtime path, and the stand-in loads no
        // runtime, so the artifact opens; opening it again is a repeat only with
        // the same path, since the process keeps the first runtime it loads.
        final bundled = '${root.path}/Frameworks/libonnxruntime.dylib';
        final opened = await engine.openSemanticArtifact(
          config: input(identity, onnxRuntimePath: bundled),
        );
        expect(opened.available, isTrue, reason: opened.lastError);
        final again = await engine.openSemanticArtifact(
          config: input(identity, onnxRuntimePath: bundled),
        );
        expect(again.state, SemanticState.ready);
        await expectLater(
          engine.openSemanticArtifact(config: input(identity)),
          throwsA(
            isSemanticError(SemanticErrorKind.sessionConflict).having(
              (error) => error.message,
              'message',
              contains('onnx_runtime_path changed'),
            ),
          ),
        );
      },
    );

    test('indexing on an opened artifact is refused as read-only', () async {
      await engine.openSemanticArtifact(config: input(identity));
      await expectLater(
        engine.semanticIndexBooks(
          books: [
            SemanticBookInput(
              sourceBookKey: bookKey,
              title: 'בראשית',
              contentFingerprint: BigInt.one,
              isPdf: false,
              topics: '/מקרא/תורה',
              extraFacets: const [],
              lines: [
                SemanticBookLineInput(
                  lineId: BigInt.one,
                  sectionId: BigInt.one,
                  text: probeLine,
                  lineHash: BigInt.one,
                  reference: 'בראשית א',
                  segment: BigInt.zero,
                ),
              ],
            ),
          ],
        ),
        throwsA(
          isSemanticError(
            SemanticErrorKind.readOnlySession,
          ).having((error) => error.message, 'message', contains('read-only')),
        ),
      );
    });

    test(
      'a model identity the artifact was not built with is refused',
      () async {
        await expectLater(
          engine.openSemanticArtifact(
            config: input({...identity, 'family_id': 'another-model@0000000'}),
          ),
          throwsA(
            isSemanticError(
              SemanticErrorKind.artifactIncompatible,
              field: 'model.family_id',
            ).having(
              (error) => error.message,
              'message',
              contains('model.family_id'),
            ),
          ),
        );
        final status = await engine.semanticStatus();
        expect(status.enabled, isFalse);
        expect(status.state, SemanticState.notConfigured);
      },
    );

    test(
      'the installed set reports itself, its coverage and its checks',
      () async {
        expect(installed.kind, SemanticVectorsPackageKind.base);
        expect(installed.libraryVersion, 1);
        expect(installed.slotsAdded, BigInt.from(2));
        expect(installed.alreadyApplied, isFalse);

        final vectorsDir = '${root.path}/vectors';
        final info = await engine.semanticVectorsInfo(vectorsDir: vectorsDir);
        expect(info.present, isTrue);
        expect(info.generation, installed.generation);
        expect(info.libraryReleaseTag, 'otzaria-library-ffi');
        expect(info.identityDigest, hasLength(64));
        expect(info.segments.single.kind, SemanticVectorsPackageKind.base);
        expect(
          (await engine.semanticVectorsInfo(
            vectorsDir: '${root.path}/not-installed',
          )).present,
          isFalse,
        );

        final coverage = await engine.semanticCoverage(
          vectorsDir: vectorsDir,
          cancellation: SemanticCancellationToken(),
        );
        expect(coverage.liveKeyedLines, BigInt.from(2));
        expect(coverage.coveredLines, BigInt.from(2));
        expect(coverage.booksCovered, 1);
        expect(coverage.ratio, 1.0);

        final verified = await engine.verifySemanticVectors(
          vectorsDir: vectorsDir,
          cancellation: SemanticCancellationToken(),
        );
        expect(verified.segments, 1);
        expect(verified.bytesChecked, greaterThan(BigInt.zero));
      },
    );

    test(
      'a compaction follows its policy, whose defaults are the engine\'s',
      () async {
        // The constructor's defaults are written in Dart, `defaults()` is read
        // from the engine.
        expect(
          const SemanticCompactionPolicy(),
          SemanticCompactionPolicy.defaults(),
        );
        final vectorsDir = '${root.path}/vectors';
        await engine.openSemanticArtifact(config: input(identity));

        final unforced = await engine.compactSemanticVectors(
          vectorsDir: vectorsDir,
          cancellation: SemanticCancellationToken(),
        );
        expect(unforced.compacted, isFalse, reason: unforced.reason);
        final forced = await engine.compactSemanticVectors(
          vectorsDir: vectorsDir,
          liveLibraryVersion: 1,
          policy: const SemanticCompactionPolicy(force: true),
          cancellation: SemanticCancellationToken(),
        );
        expect(forced.compacted, isTrue, reason: forced.reason);
        expect(forced.generation, greaterThan(installed.generation));
        final status = await engine.semanticStatus();
        expect(status.state, SemanticState.ready);
        expect(status.vectorsLibraryVersion, 1);
        expect(status.vectorSegments, 1);

        await expectLater(
          engine.compactSemanticVectors(
            vectorsDir: vectorsDir,
            policy: const SemanticCompactionPolicy(minFreeSpaceFactor: 0.5),
            cancellation: SemanticCancellationToken(),
          ),
          throwsA(
            isSemanticError(
              SemanticErrorKind.invalidInput,
              field: 'policy.min_free_space_factor',
            ),
          ),
        );
      },
    );

    test(
      'a release that is not the published one is refused, by kind',
      () async {
        await expectLater(
          engine.installSemanticVectors(
            input: SemanticVectorsInstallInput(
              vectorsDir: '${root.path}/vectors',
              segmentPath: '${root.path}/package/segment.oxv',
              manifestJson: File(
                '${root.path}/package/release.json',
              ).readAsStringSync(),
              publishedManifestSha256: '0' * 64,
              modelIdentityJson: jsonEncode(identity),
            ),
            cancellation: SemanticCancellationToken(),
          ),
          throwsA(isSemanticError(SemanticErrorKind.artifactNotPublished)),
        );
      },
    );

    test('a missing vector set is refused as missing, by kind', () async {
      Object? thrown;
      try {
        await engine.openSemanticArtifact(
          config: SemanticArtifactInput(
            vectorsDir: '${root.path}/not-installed',
            modelPath: '${root.path}/model/model.onnx',
            modelIdentityJson: jsonEncode(identity),
          ),
        );
      } on SemanticError catch (error) {
        thrown = error;
      }

      expect(thrown, isSemanticError(SemanticErrorKind.artifactMissing));
      // Readable where it is logged: the kind and the message together.
      expect(
        thrown.toString(),
        allOf(
          startsWith('SemanticError(artifactMissing)'),
          contains('not-installed'),
        ),
      );
    });
  }, skip: artifactSkipReason ?? false);
}

/// The two states a library reports with no session open, and the kind of
/// `lastError` each comes with: no session, in a build with semantic support,
/// or no semantic support at all.
const closedKinds = [
  SemanticErrorKind.notConfigured,
  SemanticErrorKind.featureNotInBuild,
];

SemanticErrorKind closedKind(SemanticState state) => switch (state) {
  SemanticState.notConfigured => SemanticErrorKind.notConfigured,
  SemanticState.notInBuild => SemanticErrorKind.featureNotInBuild,
  _ => throw StateError('a session is open: $state'),
};

/// A `SemanticError` of [kind], naming [field] when it is given.
TypeMatcher<SemanticError> isSemanticError(
  SemanticErrorKind kind, {
  String? field,
}) {
  final matcher = isA<SemanticError>().having(
    (error) => error.kind,
    'kind',
    kind,
  );
  return field == null
      ? matcher
      : matcher.having((error) => error.field, 'field', field);
}
