import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

import 'native_library.dart';

Future<void> main() async {
  final skipReason = await initNativeEngine();
  test('retain both typo variants with affixes and spelling', () async {
    final dir = Directory.systemTemp.createTempSync('otzaria_pr45_review');
    final engine = await SearchEngine.newInstance(path: dir.path);
    try {
      const lines = ['כונטרסים תורה', 'קונטרשים תורה'];
      for (var i = 0; i < lines.length; i++) {
        await engine.addDocument(
          id: BigInt.from(i + 1),
          title: 'בדיקת PR45',
          reference: '',
          topics: '/בדיקות',
          text: lines[i],
          segment: BigInt.zero,
          isPdf: false,
          filePath: '/review/a.txt',
        );
      }
      await engine.commit();
      final pattern = await engine.generateIndexHighlightPattern(
        query: 'קונטרסים תורה',
        distance: 0,
        customSpacing: const {},
        alternativeWords: const {},
        searchOptions: const {
          'קונטרסים_0': {
            'קידומות': true,
            'סיומות': true,
            'שגיאות כתיב': true,
            'כתיב מלא/חסר': true,
          },
        },
      );
      expect(pattern, isNotNull);
      for (final line in lines) {
        expect(
          pattern!.matcher!.findMatches(
            data: line,
            requireTokenBoundaries: pattern.wordBoundaryEligible,
          ),
          hasLength(1),
          reason: 'An indexed typo variant lost its highlight: $line',
        );
      }
      final single = await engine.generateIndexHighlightPattern(
        query: 'קונטרסים',
        distance: 0,
        customSpacing: const {},
        alternativeWords: const {},
        searchOptions: const {
          'קונטרסים_0': {
            'קידומות': true,
            'סיומות': true,
            'שגיאות כתיב': true,
            'כתיב מלא/חסר': true,
          },
        },
      );
      expect(single, isNotNull);
      // Keep the single-word compatibility regex usable when the spelling
      // shape fills its allowance and indexed typo terms extend the pattern.
      expect(single!.combinedPattern.length, greaterThan(12000));
      expect(single.combinedPattern.length, lessThanOrEqualTo(24128));
      final compatibility = RegExp(
        single.combinedPattern,
        caseSensitive: false,
      );
      expect(
        compatibility.hasMatch(List.filled(1000, 'כונטרסי ').join()),
        isFalse,
      );
      for (final word in ['כונטרסים', 'קונטרשים', 'הקונטרסיםא']) {
        expect(compatibility.hasMatch(word), isTrue, reason: word);
        expect(
          single.matcher!.findMatches(
            data: word,
            requireTokenBoundaries: single.wordBoundaryEligible,
          ),
          hasLength(1),
          reason: word,
        );
      }
    } finally {
      engine.dispose();
      dir.deleteSync(recursive: true);
    }
  }, skip: skipReason);
}
