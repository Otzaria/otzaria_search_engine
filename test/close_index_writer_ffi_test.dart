import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

import 'native_library.dart';

/// Closing the writer across the bridge releases the index lock while the engine is
/// still open, and discards uncommitted writes exactly as disposing of the engine does.
Future<void> main() async {
  final skipReason = await initNativeEngine();

  test('closeIndexWriter releases the writer before dispose', () async {
    final dir = Directory.systemTemp.createTempSync('otzaria_close_writer');
    final first = await SearchEngine.newInstance(path: dir.path);
    final second = await SearchEngine.newInstance(path: dir.path);
    try {
      await first.openIndexWriter();
      for (var ordinal = 0; ordinal < 200; ordinal++) {
        await first.addDocument(
          ordinal: ordinal,
          title: 'ספר',
          reference: 'ספר $ordinal',
          topics: '/ספר',
          text: 'שלום עולם',
          segment: BigInt.zero,
          isPdf: false,
          filePath: 'id:1',
        );
      }
      await expectLater(second.openIndexWriter(), throwsA(anything));

      await first.closeIndexWriter();
      await first.closeIndexWriter();
      await expectLater(first.openIndexWriter(), throwsA(anything));
      await expectLater(first.commit(), throwsA(anything));
      await second.openIndexWriter();
      await second.commit();
      expect(
        await second.count(
          regexTerms: ['שלום'],
          facets: ['/'],
          slop: 0,
          maxExpansions: 10,
        ),
        0,
      );
    } finally {
      first.dispose();
      second.dispose();
      try {
        dir.deleteSync(recursive: true);
      } on FileSystemException {
        // Readers may still hold mmapped files on Windows.
      }
    }
  }, skip: skipReason);
}
