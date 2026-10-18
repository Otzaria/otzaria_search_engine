import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

import 'native_library.dart';

/// The any-order flag of the advanced word-distance search across the real
/// FFI boundary: omitted it keeps the ordered search, set it finds the words in
/// either order, and the highlight matcher agrees.
Future<void> main() async {
  final skipReason = await initNativeEngine();

  group('any-order word distance FFI', () {
    late Directory indexDir;
    late SearchEngine engine;

    setUp(() async {
      indexDir = Directory.systemTemp.createTempSync('otzaria_any_order_test');
      engine = await SearchEngine.newInstance(path: indexDir.path);
      await engine.addTextBook(
        title: 'בבא מציעא',
        topics: '/בדיקות',
        filePath: '/library/any_order.txt',
        text: 'להחזיר את החפץ חייב',
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

    Future<List<SearchResult>> advanced({bool? anyOrder, int distance = 2}) =>
        engine.searchAdvanced(
          query: 'חייב להחזיר',
          negativeQuery: '',
          facets: const ['/בדיקות'],
          limit: 10,
          offset: 0,
          distance: distance,
          negativeDistance: 0,
          customSpacing: const {},
          negativeCustomSpacing: const {},
          alternativeWords: const {},
          negativeAlternativeWords: const {},
          searchOptions: const {},
          negativeSearchOptions: const {},
          order: ResultsOrder.catalogue,
          matchNikud: false,
          matchTaamim: false,
          scope: SearchScope.wordDistance,
          negativeScope: SearchScope.wordDistance,
          anyOrder: anyOrder,
        );

    test('omitting the flag keeps the ordered search', () async {
      expect(await advanced(), isEmpty);
      expect(await advanced(anyOrder: false), isEmpty);
    });

    test('the flag finds reversed words within the window', () async {
      final results = await advanced(anyOrder: true);
      expect(results, hasLength(1));
      expect(results.single.text, contains('<font color=red>חייב</font>'));
      expect(results.single.text, contains('<font color=red>להחזיר</font>'));
      expect(await advanced(anyOrder: true, distance: 1), isEmpty);
      final count = await engine.countAdvanced(
        query: 'חייב להחזיר',
        negativeQuery: '',
        facets: const ['/בדיקות'],
        distance: 2,
        negativeDistance: 0,
        customSpacing: const {},
        negativeCustomSpacing: const {},
        alternativeWords: const {},
        negativeAlternativeWords: const {},
        searchOptions: const {},
        negativeSearchOptions: const {},
        matchNikud: false,
        matchTaamim: false,
        scope: SearchScope.wordDistance,
        negativeScope: SearchScope.wordDistance,
        anyOrder: true,
      );
      expect(count, 1);
    });

    test('the highlight matcher follows the flag', () async {
      for (final anyOrder in [false, true]) {
        final pattern = generateHighlightPattern(
          query: 'חייב להחזיר',
          distance: 2,
          customSpacing: const {},
          alternativeWords: const {},
          searchOptions: const {},
          anyOrder: anyOrder,
        )!;
        final indexPattern = await engine.generateIndexHighlightPattern(
          query: 'חייב להחזיר',
          distance: 2,
          customSpacing: const {},
          alternativeWords: const {},
          searchOptions: const {},
          anyOrder: anyOrder,
        );
        for (final matcher in [pattern.matcher!, indexPattern!.matcher!]) {
          final matches = matcher.findMatches(
            data: 'להחזיר את החפץ חייב',
            requireTokenBoundaries: const [],
          );
          expect(matches, hasLength(anyOrder ? 1 : 0), reason: '$anyOrder');
        }
      }
    });
  }, skip: skipReason ?? false);
}
