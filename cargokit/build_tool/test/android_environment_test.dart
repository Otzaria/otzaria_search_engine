import 'dart:io';

import 'package:build_tool/src/android_environment.dart';
import 'package:build_tool/src/target.dart';
import 'package:build_tool/src/util.dart';
import 'package:path/path.dart' as path;
import 'package:test/test.dart';

void main() {
  test('maps Rust Android triples to NDK library triples', () {
    expect(
      AndroidEnvironment.ndkLibraryTriple(
        Target.forRustTriple('armv7-linux-androideabi')!,
      ),
      'arm-linux-androideabi',
    );
    expect(
      AndroidEnvironment.ndkLibraryTriple(
        Target.forRustTriple('aarch64-linux-android')!,
      ),
      'aarch64-linux-android',
    );
    expect(
      AndroidEnvironment.ndkLibraryTriple(
        Target.forRustTriple('i686-linux-android')!,
      ),
      'i686-linux-android',
    );
    expect(
      AndroidEnvironment.ndkLibraryTriple(
        Target.forRustTriple('x86_64-linux-android')!,
      ),
      'x86_64-linux-android',
    );
  });

  test('rejects non-Android targets', () {
    expect(
      () => AndroidEnvironment.ndkLibraryTriple(
        Target.forRustTriple('x86_64-unknown-linux-gnu')!,
      ),
      throwsArgumentError,
    );
  });

  test('resolves the runtime from the selected NDK sysroot', () {
    final sdk = Directory.systemTemp.createTempSync('cargokit_android_test_');
    addTearDown(() => sdk.deleteSync(recursive: true));
    final target = Target.forRustTriple('aarch64-linux-android')!;
    final hostTag = Platform.isMacOS
        ? 'darwin-x86_64'
        : (Platform.isLinux ? 'linux-x86_64' : 'windows-x86_64');
    final runtime = File(path.joinAll([
      sdk.path,
      'ndk',
      'test-ndk',
      'toolchains',
      'llvm',
      'prebuilt',
      hostTag,
      'sysroot',
      'usr',
      'lib',
      'aarch64-linux-android',
      androidCxxSharedRuntimeName,
    ]));
    runtime.parent.createSync(recursive: true);
    runtime.writeAsBytesSync([1, 2, 3]);

    final environment = AndroidEnvironment(
      sdkPath: sdk.path,
      ndkVersion: 'test-ndk',
      minSdkVersion: 23,
      targetTempDir: sdk.path,
      target: target,
    );

    expect(environment.cxxSharedRuntime.path, runtime.path);

    testRunCommandOverride = (args) {
      expect(path.basename(args.executable), startsWith('llvm-strip'));
      expect(args.arguments[0], '--strip-unneeded');
      expect(args.arguments[1], '-o');
      expect(args.arguments[3], runtime.path);
      File(args.arguments[2]).writeAsBytesSync([4, 5, 6]);
      return TestRunCommandResult();
    };
    addTearDown(() => testRunCommandOverride = null);

    final packaged = environment.packageCxxSharedRuntime(
      path.join(sdk.path, 'output'),
    );
    expect(packaged.readAsBytesSync(), [4, 5, 6]);
  });
}
