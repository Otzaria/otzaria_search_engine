import 'package:build_tool/src/android_environment.dart';
import 'package:build_tool/src/artifacts_provider.dart';
import 'package:build_tool/src/target.dart';
import 'package:test/test.dart';

void main() {
  group('remote artifact names', () {
    test('include the shared C++ runtime for Android', () {
      final target = Target.forRustTriple('aarch64-linux-android')!;

      expect(
        getArtifactNames(
          target: target,
          libraryName: 'search_engine',
          remote: true,
        ),
        ['libsearch_engine.so', androidCxxSharedRuntimeName],
      );
    });

    test('leave non-Android targets unchanged', () {
      final target = Target.forRustTriple('x86_64-unknown-linux-gnu')!;

      expect(
        getArtifactNames(
          target: target,
          libraryName: 'search_engine',
          remote: true,
        ),
        ['libsearch_engine.so'],
      );
    });
  });

  test('local Cargo output does not claim to contain the NDK runtime', () {
    final target = Target.forRustTriple('aarch64-linux-android')!;

    expect(
      getArtifactNames(
        target: target,
        libraryName: 'search_engine',
        remote: false,
      ),
      ['libsearch_engine.so'],
    );
  });
}
