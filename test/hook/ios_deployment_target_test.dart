import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:otzaria_search_engine/src/hook/ios_deployment_target.dart';
import 'package:otzaria_search_engine/src/hook/package_build.dart';

void main() {
  test('accepts a device binary matching Flutter 3.38 framework metadata', () {
    validateIosDeploymentTarget('''
Load command 10
      cmd LC_BUILD_VERSION
 platform IOS
    minos 12.0
      sdk 26.5
    ntools 1
      tool LD
   version 1230.1
''', maximumVersion: precompiledIosVersion);
  });

  test('rejects the old iOS 15 deployment target before signing', () {
    expect(
      () => validateIosDeploymentTarget('''
Load command 10
      cmd LC_BUILD_VERSION
 platform IOS
    minos 15.0
      sdk 26.5
''', maximumVersion: precompiledIosVersion),
      throwsStateError,
    );
  });

  test('checks legacy load commands and every architecture slice', () {
    expect(
      () => validateIosDeploymentTarget('''
libsearch_engine.dylib (architecture arm64):
Load command 9
      cmd LC_VERSION_MIN_IPHONEOS
  version 12.0
      sdk 16.0
libsearch_engine.dylib (architecture arm64e):
Load command 9
      cmd LC_VERSION_MIN_IPHONEOS
  version 12.0.1
      sdk 16.0
''', maximumVersion: precompiledIosVersion),
      throwsStateError,
    );
  });

  test(
    'refuses unrecognized output instead of approving an unchecked file',
    () {
      expect(
        () => validateIosDeploymentTarget(
          '',
          maximumVersion: precompiledIosVersion,
        ),
        throwsStateError,
      );
    },
  );

  test('checks real iOS Mach-O files produced by Xcode', () async {
    final temp = Directory.systemTemp.createTempSync('ios_deployment_test');
    addTearDown(() => temp.deleteSync(recursive: true));
    final source = File('${temp.path}/probe.c')
      ..writeAsStringSync('int probe(void) { return 42; }\n');
    final sdk = await Process.run('xcrun', [
      '--sdk',
      'iphoneos',
      '--show-sdk-path',
    ]);
    expect(sdk.exitCode, 0, reason: '${sdk.stderr}');

    for (final version in ['12.0', '15.0']) {
      final library = File('${temp.path}/probe-$version.dylib');
      final compiled = await Process.run('xcrun', [
        'clang',
        '-target',
        'arm64-apple-ios$version',
        '-isysroot',
        '${sdk.stdout}'.trim(),
        '-dynamiclib',
        source.path,
        '-o',
        library.path,
      ]);
      expect(compiled.exitCode, 0, reason: '${compiled.stderr}');
      final checked = checkIosDeploymentTarget(
        library,
        maximumVersion: precompiledIosVersion,
      );
      if (version == '12.0') {
        await checked;
      } else {
        await expectLater(checked, throwsStateError);
      }
    }
  }, skip: !Platform.isMacOS);
}
