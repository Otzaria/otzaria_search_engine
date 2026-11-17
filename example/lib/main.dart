import 'package:flutter/widgets.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

Future<void> main() async {
  await RustLib.init();
  runApp(const MyApp());
}

class MyApp extends StatelessWidget {
  const MyApp({super.key});

  @override
  Widget build(BuildContext context) {
    return WidgetsApp(
      color: const Color(0xFFFFFFFF),
      textStyle: const TextStyle(color: Color(0xFF000000), fontSize: 18),
      builder: (context, child) => const ColoredBox(
        color: Color(0xFFFFFFFF),
        child: Center(child: Text('Search Engine Example')),
      ),
    );
  }
}
