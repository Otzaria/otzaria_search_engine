import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

import 'native_library.dart';

/// A phrase that continues onto the next line, across the real FFI boundary:
/// `SearchResult.continuesToNextLine` must decode on the Dart side.
Future<void> main() async {
  final skipReason = await initNativeEngine();

  group('cross-line phrase FFI', () {
    late Directory indexDir;
    late SearchEngine engine;

    setUp(() async {
      indexDir = Directory.systemTemp.createTempSync('otzaria_xline_test');
      engine = await SearchEngine.newInstance(path: indexDir.path);
      await engine.addTextBook(
        title: 'בראשית',
        topics: '/תורה',
        filePath: '/library/bereshit.txt',
        catalogueOrder: 0,
        generationOrder: 0,
        text:
            'ויבדל בין המים אשר מתחת לרקיע ובין המים\n(ג) ויאמר אלהים יקוו המים',
        textStorage: TextStorage.inIndex,
      );
      await engine.commit();
    });

    tearDown(() {
      try {
        indexDir.deleteSync(recursive: true);
      } on FileSystemException {
        // Mmapped segment files may still be held on Windows.
      }
    });

    Future<List<SearchResult>> exact(String query) => engine.searchExact(
      query: query,
      facets: const [],
      limit: 10,
      offset: 0,
      order: ResultsOrder.catalogue,
      matchNikud: false,
      matchTaamim: false,
    );

    test('a hit across the break is flagged and joins both lines', () async {
      final results = await exact('ובין המים ויאמר אלהים');
      expect(results, hasLength(1));
      expect(results.single.segment, BigInt.zero);
      expect(results.single.continuesToNextLine, isTrue);
      expect(results.single.text, contains('<br>'));
    });

    test('an in-line hit is not flagged', () async {
      final results = await exact('יקוו המים');
      expect(results, hasLength(1));
      expect(results.single.continuesToNextLine, isFalse);
    });
  }, skip: skipReason ?? false);
}
