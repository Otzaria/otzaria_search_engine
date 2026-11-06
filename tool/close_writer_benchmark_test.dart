import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math' as math;

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

// Run explicitly with flutter test; this measures Dart timer gaps, not UI frames.
// OTZARIA_CLOSE_BENCHMARK_LIBRARY must name a release build of this checkout.
Future<void> main() async {
  final library = Platform.environment['OTZARIA_CLOSE_BENCHMARK_LIBRARY'];
  if (library == null || !File(library).existsSync()) {
    throw StateError(
      'Set OTZARIA_CLOSE_BENCHMARK_LIBRARY to the release library',
    );
  }
  await RustLib.init(externalLibrary: ExternalLibrary.open(library));
  final lines = int.parse(
    Platform.environment['OTZARIA_CLOSE_BENCHMARK_LINES'] ?? '20000',
  );
  final pairs = int.parse(
    Platform.environment['OTZARIA_CLOSE_BENCHMARK_PAIRS'] ?? '3',
  );
  final pendingText = List.generate(
    lines,
    (i) => 'pending benchmark line $i with index terms alpha beta gamma delta',
  ).join('\n');

  test('compare direct disposal with awaited writer close', () async {
    final rows = <Map<String, Object>>[];
    for (var pair = 0; pair < pairs; pair++) {
      final modes = pair.isEven ? [false, true] : [true, false];
      for (final closeFirst in modes) {
        final directory = await Directory.systemTemp.createTemp(
          'writer_bench_',
        );
        final engine = await SearchEngine.newInstance(path: directory.path);
        try {
          await engine.addTextBook(
            title: 'committed',
            topics: '/benchmark',
            filePath: 'id:1',
            text: 'committed sentinel',
            textStorage: TextStorage.inIndex,
          );
          await engine.commit();
          final clock = Stopwatch()..start();
          final ticks = <int>[0];
          final timer = Timer.periodic(
            const Duration(milliseconds: 1),
            (_) => ticks.add(clock.elapsedMicroseconds),
          );
          late final int start;
          late final int end;
          late final int dispatchUs;
          late final int disposeUs;
          try {
            await Future<void>.delayed(const Duration(milliseconds: 20));
            await engine.addTextBook(
              title: 'pending',
              topics: '/benchmark',
              filePath: 'id:2',
              text: pendingText,
              textStorage: TextStorage.inIndex,
            );
            start = clock.elapsedMicroseconds;
            if (closeFirst) {
              final closing = engine.closeIndexWriter();
              dispatchUs = clock.elapsedMicroseconds - start;
              await closing;
              final disposing = clock.elapsedMicroseconds;
              engine.dispose();
              disposeUs = clock.elapsedMicroseconds - disposing;
            } else {
              engine.dispose();
              disposeUs = clock.elapsedMicroseconds - start;
              dispatchUs = disposeUs;
            }
            end = clock.elapsedMicroseconds;
            await Future<void>.delayed(const Duration(milliseconds: 20));
          } finally {
            timer.cancel();
          }
          var maxGap = 0;
          var ticksDuringClose = 0;
          for (var i = 1; i < ticks.length; i++) {
            if (ticks[i] > start && ticks[i - 1] < end) {
              maxGap = math.max(maxGap, ticks[i] - ticks[i - 1]);
            }
            if (ticks[i] >= start && ticks[i] <= end) ticksDuringClose++;
          }

          final reopened = await SearchEngine.newInstance(path: directory.path);
          try {
            await reopened.openIndexWriter();
            await reopened.commit();
            for (final term in ['committed', 'pending']) {
              expect(
                await reopened.count(
                  regexTerms: [term],
                  facets: const ['/'],
                  slop: 0,
                  maxExpansions: 10,
                ),
                term == 'committed' ? 1 : 0,
              );
            }
          } finally {
            await reopened.closeIndexWriter();
            reopened.dispose();
          }
          final row = <String, Object>{
            'pair': pair,
            'mode': closeFirst ? 'await_close_then_dispose' : 'direct_dispose',
            'lines': lines,
            'total_close_us': end - start,
            'synchronous_call_us': dispatchUs,
            'synchronous_dispose_us': disposeUs,
            'max_timer_gap_us': maxGap,
            'timer_ticks_during_close': ticksDuringClose,
          };
          rows.add(row);
          // ignore: avoid_print
          print('CLOSE_WRITER_SAMPLE ${jsonEncode(row)}');
        } finally {
          if (!engine.isDisposed) {
            await engine.closeIndexWriter();
            engine.dispose();
          }
          await directory.delete(recursive: true);
        }
      }
    }
    // ignore: avoid_print
    print(
      'CLOSE_WRITER_RESULT ${jsonEncode({'os': Platform.operatingSystem, 'dart': Platform.version, 'library': library, 'measurement': 'Dart isolate timer gaps; not rendered UI frame times', 'samples': rows})}',
    );
  }, timeout: const Timeout(Duration(minutes: 10)));
}
