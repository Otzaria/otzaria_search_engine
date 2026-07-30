import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/otzaria_search_engine.dart';

/// Locates and initializes the native engine, returning the reason it could not
/// be loaded (`null` on success) so a suite can pass it straight to `skip:`.
///
/// Skipping is a local convenience: a contributor who has not run
/// `cargo build` in `rust/` should still be able to run the pure-Dart suites.
/// It must never be a CI convenience — a job whose every FFI test silently
/// skipped reports green while proving nothing about the bridge. So when
/// `OTZARIA_REQUIRE_NATIVE` is set, a missing library fails instead.
Future<String?> initNativeEngine() async {
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
      return null;
    }
  }

  const message =
      'ספריית המנוע הנייטיבית לא נמצאה — הריצו cargo build בתיקיית rust';
  if (Platform.environment.containsKey('OTZARIA_REQUIRE_NATIVE')) {
    fail(
      '$message\n'
      'OTZARIA_REQUIRE_NATIVE is set, so skipping the FFI suites is not an '
      'option: a green run that tested nothing is worse than a red one. '
      'Searched: ${candidates.join(', ')}',
    );
  }
  return message;
}
