import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

import 'native_library.dart';

/// Exercise the packed PDF API across the real bridge, including its offsets,
/// page indexes, errors, and equivalence with the existing String API.
Future<void> main() async {
  final skipReason = await initNativeEngine();

  group('packed PDF FFI', () {
    late Directory dir;
    late SearchEngine original;
    late SearchEngine packed;
    const filePath = '/library/packed.pdf';
    const title = 'ספר בדיקה';
    const topics = '/בדיקות';
    const facets = ['/author/מחבר', '/era/אחרונים'];

    setUp(() async {
      dir = Directory.systemTemp.createTempSync('otzaria_pdf_bytes');
      original = await SearchEngine.newInstance(
        path: (Directory('${dir.path}/original')..createSync()).path,
      );
      packed = await SearchEngine.newInstance(
        path: (Directory('${dir.path}/packed')..createSync()).path,
      );
    });

    tearDown(() {
      original.dispose();
      packed.dispose();
      try {
        dir.deleteSync(recursive: true);
      } on FileSystemException {
        // Readers may still hold mmapped files on Windows.
      }
    });

    Future<int> addBytes(Uint8List bytes, List<int> ends, List<int> indexes) =>
        packed.addPdfBookBytes(
          title: title,
          topics: topics,
          filePath: filePath,
          bytes: bytes,
          ends: ends,
          pageIndexes: indexes,
          extraFacets: facets,
        );

    List<Object?> snapshot(SearchResult? hit) {
      expect(hit, isNotNull);
      return [
        hit!.id,
        hit.segment,
        hit.title,
        hit.reference,
        hit.text,
        hit.isPdf,
        hit.filePath,
        hit.textStatus,
        hit.continuesToNextLine,
        hit.mergedCount,
        for (final sibling in hit.merged)
          [
            sibling.id,
            sibling.segment,
            sibling.title,
            sibling.reference,
            sibling.filePath,
            sibling.isPdf,
          ],
      ];
    }

    Future<void> compare(List<PdfPageInput> pages, {bool typed = true}) async {
      final builder = BytesBuilder(copy: false);
      final ends = <int>[];
      for (final page in pages) {
        builder.add(utf8.encode(page.reference));
        ends.add(builder.length);
        builder.add(utf8.encode(page.text));
        ends.add(builder.length);
      }
      final indexes = pages.map((page) => page.pageIndex).toList();
      final expected = await original.addPdfBook(
        title: title,
        topics: topics,
        filePath: filePath,
        pages: pages,
        extraFacets: facets,
      );
      expect(
        await addBytes(
          builder.takeBytes(),
          typed ? Uint32List.fromList(ends) : ends,
          typed ? Uint32List.fromList(indexes) : indexes,
        ),
        expected,
      );
      await original.commit();
      await packed.commit();
      expect(
        await packed.getDocumentCount(),
        await original.getDocumentCount(),
      );
      final stamps = await original.getBookStamps();
      expect(await packed.getBookStamps(), stamps);
      expect(
        await packed.getBookFingerprints(),
        await original.getBookFingerprints(),
      );
      for (final stamp in stamps) {
        for (var ordinal = 1; ordinal <= stamp.documents; ordinal++) {
          final id =
              (BigInt.from(stamp.bookNumber) << 32) + BigInt.from(ordinal);
          expect(
            snapshot(await packed.getDocumentById(id: id)),
            snapshot(await original.getDocumentById(id: id)),
          );
        }
      }
      for (final query in ['בראשית ברא', 'שורה שניה', 'שלום עולם']) {
        Future<List<SearchResult>> search(SearchEngine engine) =>
            engine.searchExact(
              query: query,
              facets: const ['/author/מחבר'],
              limit: 20,
              offset: 0,
              order: ResultsOrder.catalogue,
              matchNikud: false,
              matchTaamim: false,
            );
        expect(
          (await search(packed)).map(snapshot).toList(),
          (await search(original)).map(snapshot).toList(),
          reason: query,
        );
      }
    }

    test(
      'typed buffers preserve documents, stamps, facets and search',
      () async {
        await compare(const [
          PdfPageInput(reference: '', text: '', pageIndex: 0),
          PdfPageInput(
            reference: 'עמוד א 😀',
            text: '\n≡≡≡ ∴∴∴ ⊕⊗⊘\nבְּרֵאשִׁית ברא אלהים\r\nשורה שניה',
            pageIndex: 3,
          ),
          PdfPageInput(reference: '', text: 'שלום עולם', pageIndex: 0xffffffff),
        ]);
        expect(await packed.getDocumentCount(), BigInt.from(3));
      },
    );

    test('ordinary offset lists also cross the bridge', () async {
      await compare(const [
        PdfPageInput(reference: 'עמוד', text: 'שלום עולם', pageIndex: 7),
      ], typed: false);
    });

    test('empty book and empty page add no documents', () async {
      expect(await addBytes(Uint8List(0), [], []), 0);
      expect(await addBytes(Uint8List(0), [0, 0], [0]), 0);
      await packed.commit();
      expect(await packed.getDocumentCount(), BigInt.zero);
    });

    test(
      'bad offsets fail before indexing and leave the engine usable',
      () async {
        final bytes = utf8.encode('עמודשלום עולם');
        for (final (ends, indexes) in [
          (<int>[], <int>[]), // Nonempty buffer without pages.
          ([0, bytes.length - 1], [0]), // Unconsumed tail.
          ([0], [0]), // Odd offset count.
          ([0, bytes.length], <int>[]), // Missing page index.
          ([2, 1], [0]), // Descending within a page.
          ([0, 4, 3, bytes.length], [0, 1]), // Descending across pages.
          ([0, 0xffffffff], [0]), // Past the buffer.
        ]) {
          await expectLater(
            addBytes(bytes, ends, indexes),
            throwsA(
              predicate<Object>(
                (error) => error.toString().contains('add_pdf_book_bytes:'),
              ),
            ),
            reason: '$ends / $indexes',
          );
        }
        await compare(const [
          PdfPageInput(reference: 'עמוד', text: 'שלום עולם', pageIndex: 1),
        ]);
        expect(await packed.getDocumentCount(), BigInt.one);
      },
    );

    test('split and invalid UTF-8 are replaced per slice', () async {
      final text = utf8.encode('שלום עולם');
      final bytes = Uint8List.fromList([0xd7, 0xa9, ...text, 0xff]);
      expect(await addBytes(bytes, [1, bytes.length], [7]), 1);
      await packed.commit();
      final stamps = await packed.getBookStamps();
      final id = (BigInt.from(stamps.single.bookNumber) << 32) + BigInt.one;
      final hit = await packed.getDocumentById(id: id);
      expect(hit, isNotNull);
      expect(hit!.reference, '\ufffd');
      expect(hit.segment, BigInt.from(7));
      expect(hit.text, contains('שלום עולם'));
    });
  }, skip: skipReason ?? false);
}
