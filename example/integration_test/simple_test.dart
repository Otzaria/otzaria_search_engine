import 'package:flutter/rendering.dart';
import 'package:integration_test/integration_test.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

void main() async {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();

  test('Search Engine with custom tokenizer', () async {
    debugPrint("hello from test");
    final engine = await SearchEngine.newInstance(path: "c:\\dev\\index");
    debugPrint(engine.toString());
    engine.addDocument(
        id: BigInt.from(1),
        title: "Document 1",
        reference: "Reference 1",
        text: "יהודים",
        segment: BigInt.from(2),
        topics: "/מוסק",
        isPdf: false,
        filePath: "/path/to/doc1");
    engine.commit();
    final results = await engine.search(
        regexTerms: ["יה.*דים"],
        facets: ["/"],
        limit: 1,
        offset: 0,
        slop: 0,
        maxExpansions: 50,
        order: ResultsOrder.relevance);
    expect(results.length, 1);
    expect(results[0].text, "יהודים");
    expect(results[0].id, BigInt.from(1));
    expect(results[0].title, "Document 1");
    expect(results[0].reference, "Reference 1");
    expect(results[0].segment, BigInt.from(2));
  });
}
