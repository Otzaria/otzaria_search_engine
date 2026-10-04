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

    Future<void> addFixture(String text) async {
      await engine.addTextBook(
        title: 'בדיקות גבול שורה',
        topics: '/בדיקות',
        filePath: '/library/fixture.txt',
        catalogueOrder: 1,
        generationOrder: 0,
        text: text,
        textStorage: TextStorage.inIndex,
      );
      await engine.commit();
    }

    Future<List<SearchResult>> advanced(
      String query, {
      int distance = 0,
      Map<String, Map<String, bool>> searchOptions = const {},
    }) => engine.searchAdvanced(
      query: query,
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
      searchOptions: searchOptions,
      negativeSearchOptions: const {},
      order: ResultsOrder.catalogue,
      matchNikud: false,
      matchTaamim: false,
      scope: SearchScope.wordDistance,
      negativeScope: SearchScope.wordDistance,
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

    test('both paired readings at a line edge remain searchable', () async {
      await addFixture('הארץ (הוצא) [היצא]\nאתך כל החיה');
      for (final query in ['הוצא אתך', 'היצא אתך']) {
        final results = await exact(query);
        expect(results, hasLength(1), reason: query);
        expect(results.single.continuesToNextLine, isTrue, reason: query);
        expect(results.single.text, contains('<br>'), reason: query);
        expect(results.single.text, contains('<font color=red>אתך</font>'));
      }
    });

    test(
      'a later repeated word can complete a gapped cross-line hit',
      () async {
        await addFixture('אחת שתים שתים\nמילה שלש');
        final results = await advanced('אחת שתים שלש', distance: 1);
        expect(results, hasLength(1));
        expect(results.single.continuesToNextLine, isTrue);
        expect(results.single.text, contains('<br>'));
        expect(results.single.text, contains('<font color=red>שלש</font>'));
      },
    );

    test('an acronym expansion displays its whole cross-line phrase', () async {
      await addFixture('כתב רבי משה\nבן מיימון בספרו');
      final dictionary = File('${indexDir.path}/acronyms.json')
        ..writeAsStringSync(r'{"רמב\"ם": ["רבי משה בן מיימון"]}');
      expect(engine.setAcronymsDictionaryPath(path: dictionary.path), isTrue);
      final results = await advanced(
        'רמב"ם',
        searchOptions: const {
          'רמב"ם_0': {'ראשי תיבות': true},
        },
      );
      expect(results, hasLength(1));
      expect(results.single.continuesToNextLine, isTrue);
      expect(results.single.text, contains('<br>'));
      expect(results.single.text, contains('<font color=red>מיימון</font>'));
    });
  }, skip: skipReason ?? false);
}
