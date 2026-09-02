import 'package:build_tool/src/android_environment.dart';
import 'package:build_tool/src/precompile_binaries.dart';
import 'package:build_tool/src/target.dart';
import 'package:test/test.dart';

void main() {
  group('release asset names', () {
    test('match how GitHub stores a name containing plus signs', () {
      final target = Target.forRustTriple('aarch64-linux-android')!;

      // GitHub collapses every run of characters outside [A-Za-z0-9._-] into a
      // single dot, so this is the name the asset actually gets. Addressing the
      // unsanitized name made every download and every verification miss.
      expect(
        PrecompileBinaries.fileName(target, androidCxxSharedRuntimeName),
        'aarch64-linux-android_libc._shared.so',
      );
      expect(
        PrecompileBinaries.signatureFileName(
            target, androidCxxSharedRuntimeName),
        'aarch64-linux-android_libc._shared.so.sig',
      );
    });

    test('leave names that need no normalization unchanged', () {
      final target = Target.forRustTriple('aarch64-linux-android')!;

      expect(
        PrecompileBinaries.fileName(target, 'libsearch_engine.so'),
        'aarch64-linux-android_libsearch_engine.so',
      );
      expect(
        PrecompileBinaries.signatureFileName(target, 'libsearch_engine.so'),
        'aarch64-linux-android_libsearch_engine.so.sig',
      );
    });
  });
}
