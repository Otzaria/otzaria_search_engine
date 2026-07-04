import 'dart:io';

import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;
import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

/// אימות צד-Dart של `generateHighlightPattern`: התבניות שהמנוע מחזיר
/// חייבות להתקמפל ב-RegExp של Dart ולהתנהג כמו ההדגשה ההיסטורית של
/// אוצריא (סובלנות ניקוד, מרווחים, גבולות מילה, כתיב מלא/חסר).
///
/// דורש build מקומי של המנוע: `cargo build` בתיקיית rust/.
Future<bool> _tryInitEngine() async {
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
      return true;
    }
  }
  return false;
}

HighlightPattern _pattern(
  String query, {
  int distance = 0,
  Map<String, String> spacing = const {},
  Map<int, List<String>> alternatives = const {},
  Map<String, Map<String, bool>> options = const {},
}) {
  final pattern = generateHighlightPattern(
    query: query,
    distance: distance,
    customSpacing: spacing,
    alternativeWords: alternatives,
    searchOptions: options,
  );
  expect(pattern, isNotNull, reason: 'expected a pattern for "$query"');
  return pattern!;
}

RegExp _compile(String pattern) => RegExp(pattern, caseSensitive: false);

Future<void> main() async {
  final engineReady = await _tryInitEngine();

  group(
    'generateHighlightPattern',
    () {
      test('מילה בודדת נתפסת בטקסט נקי ובטקסט מנוקד', () {
        final hl = _pattern('כל');
        final regex = _compile(hl.combinedPattern);
        expect(regex.hasMatch('כל יום טוב'), isTrue);
        expect(regex.hasMatch('הָיָה כָּל הַיּוֹם'), isTrue);
        expect(hl.wordBoundaryEligible, equals([true]));
      });

      test('ביטוי רב-מילים נתפס רק ברצף', () {
        final hl = _pattern('כל היום');
        final regex = _compile(hl.combinedPattern);
        expect(regex.hasMatch('היה זה כל היום טוב'), isTrue);
        expect(regex.hasMatch('כל הספרים היו שם'), isFalse);
      });

      test('פיסוק ותגי HTML בין מילים אינם שוברים התאמה', () {
        final hl = _pattern('רבי יוחנן הוא', distance: 1);
        final regex = _compile(hl.combinedPattern);
        expect(regex.hasMatch('רבי יוחנן: הוא אפילו'), isTrue);
        expect(
          _compile(
            _pattern('אמר רבי יוחנן').combinedPattern,
          ).hasMatch('אמר <b>רבי</b> יוחנן'),
          isTrue,
        );
      });

      test('מרווח מותאם מאפשר מילים ביניים עד הגבול', () {
        final hl = _pattern('כל היום', spacing: {'0-1': '1'});
        final regex = _compile(hl.combinedPattern);
        expect(regex.hasMatch('היה זה כל דבר היום טוב'), isTrue);
        expect(regex.hasMatch('היה זה כל דבר נוסף היום טוב'), isFalse);
      });

      test('ערך מרווח יחיד חל כברירת מחדל על כל הפערים', () {
        final hl = _pattern('אמר שמעון לקיש', spacing: {'0-1': '1'});
        final regex = _compile(hl.combinedPattern);
        expect(regex.hasMatch('אמר רבי שמעון בן לקיש'), isTrue);
      });

      test('searchDistance גלובלי מתנהג כמו מרווח מותאם', () {
        final hl = _pattern('פרעה נבון', distance: 1);
        final regex = _compile(hl.combinedPattern);
        expect(
          regex.hasMatch('וְעַתָּה יֵרֶא פַּרְעֹה אִישׁ נָבוֹן וְחָכָם'),
          isTrue,
        );
      });

      test('מקף בין מילים מנוקדות אינו נבלע לתוך מילה', () {
        final hl = _pattern('עקב אשר שמע אברהם');
        final regex = _compile(hl.combinedPattern);
        const text = 'עֵ֣קֶב אֲשֶׁר־שָׁמַ֣ע אַבְרָהָ֖ם בְּקֹלִ֑י';
        final match = regex.firstMatch(text);
        expect(match, isNotNull);
        // תבניות המילים מאתרות כל מילה בנפרד בתוך ההתאמה.
        var offset = 0;
        final matched = match!.group(0)!;
        for (final wordPattern in hl.wordPatterns) {
          final wordMatch = _compile(
            wordPattern,
          ).firstMatch(matched.substring(offset));
          expect(wordMatch, isNotNull);
          offset += wordMatch!.end;
        }
      });

      test('כתיב מלא/חסר תופס גם את הצורה החסרה', () {
        final hl = _pattern(
          'שלום',
          options: {
            'שלום_0': {'כתיב מלא/חסר': true},
          },
        );
        final regex = _compile(hl.combinedPattern);
        expect(regex.hasMatch('דרשו שלם בעדה'), isTrue);
        expect(regex.hasMatch('שָׁלוֹם רב'), isTrue);
      });

      test('אפשרות מורפולוגית מבטלת את דרישת גבולות המילה', () {
        final hl = _pattern(
          'אמר',
          options: {
            'אמר_0': {'חלק ממילה': true},
          },
        );
        expect(hl.wordBoundaryEligible, equals([false]));
        expect(_compile(hl.combinedPattern).hasMatch('ויאמר משה'), isTrue);
      });

      test('מילים חילופיות נתפסות באותו מיקום', () {
        final hl = _pattern(
          'צדיק',
          alternatives: {
            0: ['חכם'],
          },
        );
        final regex = _compile(hl.combinedPattern);
        expect(regex.hasMatch('איש חָכָם היה'), isTrue);
        expect(regex.hasMatch('איש צַדִּיק היה'), isTrue);
      });

      test('שאילתה ריקה או ניקוד-בלבד מחזירה null', () {
        for (final query in ['', '   ', 'ְָ']) {
          expect(
            generateHighlightPattern(
              query: query,
              distance: 0,
              customSpacing: const {},
              alternativeWords: const {},
              searchOptions: const {},
            ),
            isNull,
            reason: 'query: "$query"',
          );
        }
      });
    },
    skip: engineReady
        ? false
        : 'ספריית המנוע הנייטיבית לא נמצאה — הריצו cargo build בתיקיית rust',
  );
}
