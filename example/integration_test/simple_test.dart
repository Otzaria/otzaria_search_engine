import 'package:flutter/rendering.dart';
import 'package:integration_test/integration_test.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:search_engine/search_engine.dart';
import 'package:search_engine/src/rust/api/reference_search_engine.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  debugPrint("test1");
  setUpAll(() async => await RustLib.init());
  debugPrint("test2");
  test('Reference Search Engine', () async {
    final engine = ReferenceSearchEngine(path: "./ref_index");
    debugPrint(engine.toString());
    engine.addDocument(
        id: BigInt.from(1),
        title: "Document 1",
        reference: "Reference 1",
        shortRef: "Ref 1",
        segment: BigInt.from(2),
        isPdf: false,
        filePath: "/path/to/doc1");
    engine.commit();
    final results = await engine.search(
        query: "Reference",
        limit: 10,
        fuzzy: false,
        order: ResultsOrder.relevance);
    expect(results.length, 1);
    expect(results[0].reference, "Reference 1");
  });
}
