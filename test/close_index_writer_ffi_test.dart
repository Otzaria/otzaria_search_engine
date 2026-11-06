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
      await first.closeIndexWriter();
      first.dispose();
      await second.closeIndexWriter();
      second.dispose();
      await dir.delete(recursive: true);
    }
  }, skip: skipReason);

  test(
    'queued indexing and concurrent closes cannot reopen the writer',
    () async {
      final dir = await Directory.systemTemp.createTemp('otzaria_close_queue');
      final engine = await SearchEngine.newInstance(path: dir.path);
      try {
        await engine.addTextBook(
          title: 'committed',
          topics: '/fixture',
          filePath: 'id:1',
          text: 'committed sentinel',
          textStorage: TextStorage.inIndex,
        );
        await engine.commit();
        final indexing = engine
            .addTextBook(
              title: 'pending',
              topics: '/fixture',
              filePath: 'id:2',
              text: List.filled(20000, 'pending alpha beta gamma').join('\n'),
              textStorage: TextStorage.inIndex,
            )
            .then<void>(
              (_) {},
              onError: (Object error) {
                expect(error.toString(), contains('permanently closed'));
              },
            );
        await Future.wait([
          indexing,
          engine.closeIndexWriter(),
          engine.closeIndexWriter(),
        ]);
        await expectLater(engine.openIndexWriter(), throwsA(anything));
        await expectLater(engine.commit(), throwsA(anything));
        for (final term in ['committed', 'pending']) {
          expect(
            await engine.count(
              regexTerms: [term],
              facets: const ['/'],
              slop: 0,
              maxExpansions: 10,
            ),
            term == 'committed' ? 1 : 0,
          );
        }
      } finally {
        await engine.closeIndexWriter();
        engine.dispose();
        await dir.delete(recursive: true);
      }
    },
    skip: skipReason,
  );

  test('close and dispose allow replacing the index directory', () async {
    final root = await Directory.systemTemp.createTemp('otzaria_close_replace');
    final path = '${root.path}/index';
    await Directory(path).create();
    final engine = await SearchEngine.newInstance(path: path);
    SearchEngine? replacement;
    try {
      await engine.addTextBook(
        title: 'original',
        topics: '/fixture',
        filePath: 'id:1',
        text: 'original sentinel',
        textStorage: TextStorage.inIndex,
      );
      await engine.commit();
      await engine.closeIndexWriter();
      engine.dispose();

      await Directory(path).rename('${root.path}/retired');
      await Directory(path).create();
      replacement = await SearchEngine.newInstance(path: path);
      await replacement.addTextBook(
        title: 'replacement',
        topics: '/fixture',
        filePath: 'id:2',
        text: 'replacement sentinel',
        textStorage: TextStorage.inIndex,
      );
      await replacement.commit();
      for (final term in ['original', 'replacement']) {
        expect(
          await replacement.count(
            regexTerms: [term],
            facets: const ['/'],
            slop: 0,
            maxExpansions: 10,
          ),
          term == 'replacement' ? 1 : 0,
        );
      }
    } finally {
      if (!engine.isDisposed) {
        await engine.closeIndexWriter();
        engine.dispose();
      }
      if (replacement != null) {
        await replacement.closeIndexWriter();
        replacement.dispose();
      }
      await root.delete(recursive: true);
    }
  }, skip: skipReason);
}
