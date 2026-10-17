import 'dart:io';
import 'dart:typed_data';

import 'package:code_assets/code_assets.dart';
import 'package:ed25519_edwards/ed25519_edwards.dart' as ed;
import 'package:flutter_test/flutter_test.dart';
import 'package:hooks/hooks.dart';
import 'package:path/path.dart' as p;

import '../../hook/src/cargo_build.dart';
import '../../hook/src/crate_hash.dart';
import '../../hook/src/native_build.dart';
import '../../hook/src/precompiled.dart';
import '../../hook/src/rust_target.dart';

String _hex(List<int> bytes) =>
    bytes.map((b) => b.toRadixString(16).padLeft(2, '0')).join();

void main() {
  late Directory temp;

  setUp(() => temp = Directory.systemTemp.createTempSync('native_build_test'));
  tearDown(() => temp.deleteSync(recursive: true));

  void writePackage(String root, {String lineEnding = '\n'}) {
    String text(List<String> lines) => lines.join(lineEnding) + lineEnding;
    File(p.join(root, 'rust', 'src', 'lib.rs'))
      ..createSync(recursive: true)
      ..writeAsStringSync(text(['pub fn a() {}', 'pub fn b() {}']));
    File(
      p.join(root, 'rust', 'Cargo.toml'),
    ).writeAsStringSync(text(['[package]', 'name = "x"']));
    File(p.join(root, 'hook', 'build.dart'))
      ..createSync(recursive: true)
      ..writeAsStringSync(text(['void main() {}']));
  }

  group('CrateHash', () {
    test(
      'ignores line endings, so a CRLF checkout publishes the same hash',
      () {
        final lf = p.join(temp.path, 'lf');
        final crlf = p.join(temp.path, 'crlf');
        writePackage(lf);
        writePackage(crlf, lineEnding: '\r\n');
        expect(
          CrateHash.compute(packageRoot: crlf),
          CrateHash.compute(packageRoot: lf),
        );
      },
    );

    test('changes with the crate sources and with the build hook', () {
      final root = p.join(temp.path, 'pkg');
      writePackage(root);
      final original = CrateHash.compute(packageRoot: root);

      File(p.join(root, 'rust', 'src', 'lib.rs')).writeAsStringSync('// x\n');
      final afterSource = CrateHash.compute(packageRoot: root);
      expect(afterSource, isNot(original));

      File(
        p.join(root, 'hook', 'build.dart'),
      ).writeAsStringSync('void main() { }\n');
      expect(CrateHash.compute(packageRoot: root), isNot(afterSource));
    });

    test('ignores files outside the crate inputs, such as rust/target', () {
      final root = p.join(temp.path, 'pkg');
      writePackage(root);
      final original = CrateHash.compute(packageRoot: root);
      File(p.join(root, 'rust', 'target', 'release', 'x.dll'))
        ..createSync(recursive: true)
        ..writeAsStringSync('binary');
      File(p.join(root, 'rust', 'tests', 'fonts', 'font.ttf'))
        ..createSync(recursive: true)
        ..writeAsStringSync('font');
      expect(CrateHash.compute(packageRoot: root), original);
    });

    test('a byte change in a binary input changes the hash', () {
      final root = p.join(temp.path, 'pkg');
      writePackage(root);
      final blob = File(p.join(root, 'rust', 'src', 'blob.bin'))
        ..writeAsBytesSync([0xff, 0xfe, 0x01]);
      final before = CrateHash.compute(packageRoot: root);
      blob.writeAsBytesSync([0xff, 0xfd, 0x01]);
      expect(CrateHash.compute(packageRoot: root), isNot(before));
    });

    test('covers extra crate directories when asked to', () {
      final root = p.join(temp.path, 'pkg');
      writePackage(root);
      File(p.join(root, 'rust', 'vendor', 'dep', 'lib.rs'))
        ..createSync(recursive: true)
        ..writeAsStringSync('fn v() {}\n');
      final withVendor = CrateHash.compute(
        packageRoot: root,
        extraCrateDirs: ['vendor'],
      );
      File(
        p.join(root, 'rust', 'vendor', 'dep', 'lib.rs'),
      ).writeAsStringSync('fn w() {}\n');
      expect(
        CrateHash.compute(packageRoot: root, extraCrateDirs: ['vendor']),
        isNot(withVendor),
      );
    });
  });

  group('PrecompiledBinaries.fetch', () {
    late HttpServer server;
    late Map<String, List<int>> assets;
    late ed.KeyPair keys;
    late PrecompiledBinaries binaries;
    late int requests;
    const target = RustTarget('x86_64-pc-windows-msvc', OS.windows);
    final library = Uint8List.fromList(List.generate(256, (i) => i % 251));

    setUp(() async {
      keys = ed.generateKey();
      assets = {};
      requests = 0;
      server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
      server.listen((request) {
        requests++;
        final body = assets[request.uri.path];
        if (body == null) {
          request.response.statusCode = HttpStatus.notFound;
        } else {
          request.response.add(body);
        }
        request.response.close();
      });
      binaries = PrecompiledBinaries(
        urlPrefix: 'http://127.0.0.1:${server.port}/download/precompiled_',
        publicKeyHex: _hex(keys.publicKey.bytes),
      );
    });
    tearDown(() => server.close(force: true));

    void publish(List<int> data, List<int> signature, {String hash = 'abc'}) {
      final asset = '/download/precompiled_$hash/x86_64-pc-windows-msvc_x.dll';
      assets[asset] = data;
      assets['$asset.sig'] = signature;
    }

    Future<File?> fetch({bool strict = false}) => binaries.fetch(
      crateHash: 'abc',
      target: target,
      fileName: 'x.dll',
      cacheDir: p.join(temp.path, 'cache'),
      strict: strict,
    );

    test('returns the binary when its signature verifies', () async {
      publish(library, ed.sign(keys.privateKey, library));
      final file = await fetch();
      expect(file!.readAsBytesSync(), library);
    });

    test('reuses the verified cache without downloading again', () async {
      publish(library, ed.sign(keys.privateKey, library));
      await fetch();
      final before = requests;
      expect((await fetch())!.readAsBytesSync(), library);
      expect(requests, before);
    });

    test('replaces a tampered cache with the verified download', () async {
      publish(library, ed.sign(keys.privateKey, library));
      final cached = (await fetch())!;
      cached.writeAsBytesSync([0]);
      final before = requests;
      expect((await fetch())!.readAsBytesSync(), library);
      expect(requests, before + 2);
    });

    test('the build hook bundles the signed download without cargo', () async {
      // This fixture cannot compile: falling back to cargo must fail the test.
      writePackage(temp.path);
      final hash = CrateHash.compute(packageRoot: temp.path);
      publish(library, ed.sign(keys.privateKey, library), hash: hash);
      final out = Directory(p.join(temp.path, 'out'))..createSync();
      final shared = Directory(p.join(temp.path, 'shared'))..createSync();
      final builder = BuildInputBuilder()
        ..setupShared(
          packageRoot: temp.uri,
          packageName: 'opentype_shaper',
          outputDirectoryShared: shared.uri,
          outputFile: out.uri.resolve('output.json'),
        )
        ..setupBuildInput();
      builder.config.setupBuild(linkingEnabled: false);
      builder.addExtension(
        CodeAssetExtension(
          targetOS: OS.windows,
          targetArchitecture: Architecture.x64,
          linkModePreference: LinkModePreference.dynamic,
        ),
      );
      final input = builder.build();
      Directory.fromUri(input.outputDirectory).createSync(recursive: true);
      final output = BuildOutputBuilder();
      await NativeBuild(
        assetName: 'src/bindings.dart',
        cargo: const CargoOptions(libName: 'x'),
        precompiled: binaries,
      ).run(input, output);
      final asset = output.build().assets.code.single;
      expect(asset.id, 'package:opentype_shaper/src/bindings.dart');
      expect(asset.linkMode, isA<DynamicLoadingBundled>());
      expect(File.fromUri(asset.file!).readAsBytesSync(), library);
      expect(asset.file, input.outputDirectory.resolve('x.dll'));
      expect(requests, 2);
    });

    test('returns null when the crate hash was never published', () async {
      expect(await fetch(), isNull);
    });

    test('never uses a binary whose signature does not match', () async {
      final tampered = Uint8List.fromList(library)..[0] ^= 0xff;
      publish(tampered, ed.sign(keys.privateKey, library));
      expect(await fetch(), isNull);
      expect(Directory(p.join(temp.path, 'cache')).existsSync(), isFalse);
      await expectLater(fetch(strict: true), throwsStateError);
    });

    test('never uses a binary signed by another key', () async {
      publish(library, ed.sign(ed.generateKey().privateKey, library));
      expect(await fetch(), isNull);
      await expectLater(fetch(strict: true), throwsStateError);
    });

    test(
      'reports a signature without its binary as a broken release',
      () async {
        assets['/download/precompiled_abc/x86_64-pc-windows-msvc_x.dll.sig'] =
            ed.sign(keys.privateKey, library);
        expect(await fetch(), isNull);
        await expectLater(fetch(strict: true), throwsStateError);
      },
    );

    test('falls back to a source build when the host is unreachable', () async {
      await server.close(force: true);
      expect(await fetch(), isNull);
    });
  });

  group('cargoEnvironment', () {
    test('links Android against the app minimum API, not a newer one', () {
      final env = cargoEnvironment(
        RustTarget.fromTriple('armv7-linux-androideabi'),
        const TargetToolchain(ndkClangDir: '/ndk/bin', androidApi: 24),
      );
      expect(
        env['CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER'],
        p.join('/ndk/bin', 'armv7a-linux-androideabi24-clang'),
      );
      expect(
        env['CC_armv7_linux_androideabi'],
        p.join('/ndk/bin', 'armv7a-linux-androideabi24-clang'),
      );
    });

    test('uses the cmd wrappers of a Windows NDK', () {
      final env = cargoEnvironment(
        RustTarget.fromTriple('aarch64-linux-android'),
        const TargetToolchain(ndkClangDir: 'C:/ndk/bin', androidApi: 26),
        hostIsWindows: true,
      );
      expect(
        env['CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER'],
        p.join('C:/ndk/bin', 'aarch64-linux-android26-clang.cmd'),
      );
      expect(env['AR_aarch64_linux_android'], endsWith('llvm-ar.exe'));
    });

    test('sets the deployment target and drops Xcode tool paths on Apple', () {
      final env = cargoEnvironment(
        RustTarget.fromTriple('aarch64-apple-ios'),
        const TargetToolchain(appleVersion: '15.0'),
        hostEnvironment: {
          'PATH': '/Applications/Xcode.app/Contents/Developer/usr/bin:/usr/bin',
        },
      );
      expect(env['IPHONEOS_DEPLOYMENT_TARGET'], '15.0');
      expect(env['PATH'], '/usr/bin');
    });
  });

  group('rustupEnvironment', () {
    test('points at the .rustup beside a workspace .cargo', () {
      final bin = Directory(p.join(temp.path, '.cargo', 'bin'))
        ..createSync(recursive: true);
      Directory(p.join(temp.path, '.rustup')).createSync();
      final exe = Platform.isWindows ? '.exe' : '';
      File(p.join(bin.path, 'rustup$exe')).createSync();
      final env = rustupEnvironment(
        p.join(bin.path, 'cargo$exe'),
        hostEnvironment: const {},
      );
      expect(env['CARGO_HOME'], p.join(temp.path, '.cargo'));
      expect(env['RUSTUP_HOME'], p.join(temp.path, '.rustup'));
    });

    test('leaves a system-wide rustup to its own defaults', () {
      final bin = Directory(p.join(temp.path, 'usr', 'bin'))
        ..createSync(recursive: true);
      final exe = Platform.isWindows ? '.exe' : '';
      File(p.join(bin.path, 'rustup$exe')).createSync();
      expect(
        rustupEnvironment(
          p.join(bin.path, 'cargo$exe'),
          hostEnvironment: const {},
        ),
        isEmpty,
      );
    });

    test('leaves a cargo that rustup does not manage alone', () {
      final bin = Directory(p.join(temp.path, 'usr', 'bin'))
        ..createSync(recursive: true);
      expect(
        rustupEnvironment(p.join(bin.path, 'cargo'), hostEnvironment: const {}),
        isEmpty,
      );
    });
  });

  test('every published target maps to its library file name', () {
    final names = {
      for (final target in RustTarget.all)
        target.triple: target.libraryFileName('x'),
    };
    expect(names['x86_64-pc-windows-msvc'], 'x.dll');
    expect(names['aarch64-apple-ios-sim'], 'libx.dylib');
    expect(names['aarch64-linux-android'], 'libx.so');
    expect(
      PrecompiledBinaries.assetName(
        RustTarget.fromTriple('aarch64-linux-android'),
        'libc++_shared.so',
      ),
      'aarch64-linux-android_libc._shared.so',
    );
  });
}
