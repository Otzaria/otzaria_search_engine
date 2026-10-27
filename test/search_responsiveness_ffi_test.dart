import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

import 'native_library.dart';

Future<void> main() async {
  final skipReason = await initNativeEngine();

  group('search responsiveness FFI', () {
    late Directory directory;
    late SearchEngine engine;

    setUp(() async {
      directory = Directory.systemTemp.createTempSync('otzaria_response_ffi');
      engine = await SearchEngine.newInstance(path: directory.path);
      await engine.addTextBook(
        title: 'בדיקת חיפוש',
        topics: '/בדיקות',
        filePath: '/fixture.txt',
        text: List.filled(120, 'שלום עולם ושבת שלום').join('\n'),
        textStorage: TextStorage.inIndex,
      );
      await engine.commit();
    });

    tearDown(() async {
      await engine.optimize();
      engine.dispose();
      await directory.delete(recursive: true);
    });

    Future<List<SearchResult>> page({int? requestId}) => engine.searchExact(
      query: 'שלום',
      facets: const [],
      limit: 10,
      offset: 10,
      order: ResultsOrder.catalogue,
      matchNikud: false,
      matchTaamim: false,
      cancelRequestId: requestId,
    );

    test('stream preserves requested chunk size and complete counts', () async {
      final updates = await engine
          .searchExactStreamWithCounts(
            query: 'שלום',
            facets: const [],
            limit: 100,
            offset: 0,
            order: ResultsOrder.catalogue,
            matchNikud: false,
            matchTaamim: false,
            chunkSize: 50,
          )
          .toList();
      expect(
        updates.where((u) => u.results.isNotEmpty).map((u) => u.results.length),
        [50, 50],
      );
      expect(updates.map((u) => u.totalCount).whereType<int>().single, 120);
      expect(
        updates.expand((u) => u.results).map((r) => r.id).toSet(),
        hasLength(100),
      );
    });

    test(
      'page cancellation crosses FFI and leaves other requests usable',
      () async {
        await engine.cancelLexicalSearch(requestId: 76);
        await expectLater(
          page(requestId: 76),
          throwsA(predicate((e) => e.toString().contains('cancelled'))),
        );
        expect(await page(requestId: 77), hasLength(10));
        expect(await page(), hasLength(10));
      },
    );

    test('memory release preserves snapshot and searchable results', () async {
      final snapshot = await engine.searchSnapshot();
      final ids = (await page()).map((r) => r.id).toList();
      await engine.releaseMemory();
      expect(await engine.searchSnapshot(), snapshot);
      expect((await page()).map((r) => r.id), ids);
      await engine.addTextBook(
        title: 'ספר נוסף',
        topics: '/בדיקות',
        filePath: '/second.txt',
        text: 'שלום עולם',
        textStorage: TextStorage.inIndex,
      );
      await engine.commit();
      expect(await engine.searchSnapshot(), isNot(snapshot));
    });
  }, skip: skipReason);
}
