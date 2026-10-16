import 'dart:io';
import 'dart:typed_data';

import 'package:args/args.dart';
import 'package:code_assets/code_assets.dart';
import 'package:ed25519_edwards/ed25519_edwards.dart' as ed;
import 'package:path/path.dart' as p;

import '../../../hook/build.dart' show nativeBuild;
import '../../../hook/src/cargo_build.dart';
import '../../../hook/src/crate_hash.dart';
import '../../../hook/src/precompiled.dart';
import '../../../hook/src/rust_target.dart';

/// Usage, from `tool/precompiled_binaries` so the package's hook never runs:
///
///   dart run bin/precompiled_binaries.dart hash
///   ... build --target <triple> [--target ...] --out <dir>   (PRIVATE_KEY set)
///   ... verify
Future<void> main(List<String> args) async {
  final parser = ArgParser()
    ..addOption('package-root', defaultsTo: '../..')
    ..addCommand('hash')
    ..addCommand(
      'build',
      ArgParser()
        ..addMultiOption('target')
        ..addOption('out', mandatory: true)
        ..addOption('android-api', defaultsTo: '21')
        ..addOption('ios-version', defaultsTo: '15.0')
        ..addOption('macos-version', defaultsTo: '12.0'),
    )
    ..addCommand('verify');
  final results = parser.parse(args);
  final packageRoot = p.normalize(p.absolute(results.option('package-root')!));
  final crateHash = CrateHash.compute(
    packageRoot: packageRoot,
    crateDir: nativeBuild.cargo.crateDir,
    extraCrateDirs: nativeBuild.extraCrateDirs,
  );

  switch (results.command?.name) {
    case 'hash':
      stdout.writeln(crateHash);
    case 'build':
      await _build(packageRoot, crateHash, results.command!);
    case 'verify':
      await _verify(crateHash);
    default:
      stderr.writeln(parser.usage);
      exit(64);
  }
}

Future<void> _build(
  String packageRoot,
  String crateHash,
  ArgResults command,
) async {
  final privateKey = ed.PrivateKey(
    _hexToBytes(Platform.environment['PRIVATE_KEY'] ?? ''),
  );
  final publicKey = ed.public(privateKey);
  final out = Directory(command.option('out')!)..createSync(recursive: true);
  stdout.writeln('Crate hash: $crateHash');
  if (command.multiOption('target').isEmpty) {
    throw ArgumentError('build needs at least one --target');
  }

  for (final triple in command.multiOption('target')) {
    final target = RustTarget.fromTriple(triple);
    final toolchain = switch (target.os) {
      OS.android => TargetToolchain(
        ndkClangDir: _ndkClangDir(),
        androidApi: int.parse(command.option('android-api')!),
      ),
      OS.iOS => TargetToolchain(appleVersion: command.option('ios-version')),
      OS.macOS => TargetToolchain(
        appleVersion: command.option('macos-version'),
      ),
      _ => const TargetToolchain(),
    };
    final library = await cargoBuild(
      packageRoot: packageRoot,
      options: nativeBuild.cargo,
      target: target,
      toolchain: toolchain,
      targetDir: p.join(packageRoot, nativeBuild.cargo.crateDir, 'target'),
      log: stdout.writeln,
    );
    final data = library.readAsBytesSync();
    final signature = ed.sign(privateKey, data);
    if (!ed.verify(publicKey, data, signature)) {
      throw StateError('Signing $triple produced an invalid signature');
    }
    final asset = PrecompiledBinaries.assetName(
      target,
      p.basename(library.path),
    );
    File(p.join(out.path, asset)).writeAsBytesSync(data);
    File(p.join(out.path, '$asset.sig')).writeAsBytesSync(signature);
    stdout.writeln('Wrote $asset');
  }
}

Future<void> _verify(String crateHash) async {
  final cache = Directory.systemTemp.createTempSync('precompiled_verify');
  final failed = <String>[];
  try {
    for (final target in RustTarget.all) {
      final fileName = target.libraryFileName(nativeBuild.cargo.libName);
      String status;
      try {
        final file = await nativeBuild.precompiled.fetch(
          crateHash: crateHash,
          target: target,
          fileName: fileName,
          cacheDir: cache.path,
          strict: true,
        );
        status = file == null ? 'MISSING' : 'OK';
      } on StateError catch (error) {
        status = 'INVALID ($error)';
      }
      if (status != 'OK') failed.add('$target: $status');
      stdout.writeln('${target.triple.padRight(28)} $status');
    }
  } finally {
    cache.deleteSync(recursive: true);
  }
  if (failed.isNotEmpty) {
    stderr.writeln('Precompiled binaries for $crateHash are incomplete:');
    failed.forEach(stderr.writeln);
    exit(1);
  }
}

/// `<ndk>/toolchains/llvm/prebuilt/<host>/bin` of the newest installed NDK.
String _ndkClangDir() {
  final env = Platform.environment;
  var ndk = env['ANDROID_NDK_HOME'] ?? env['ANDROID_NDK_ROOT'];
  if (ndk == null) {
    final sdk = env['ANDROID_HOME'] ?? env['ANDROID_SDK_ROOT'];
    final versions = sdk == null
        ? <Directory>[]
        : (Directory(
            p.join(sdk, 'ndk'),
          ).listSync().whereType<Directory>().toList()..sort(
            (a, b) => _compareVersions(p.basename(a.path), p.basename(b.path)),
          ));
    if (versions.isEmpty) throw StateError('No Android NDK found');
    ndk = versions.last.path;
  }
  final prebuilt = Directory(p.join(ndk, 'toolchains', 'llvm', 'prebuilt'));
  final host = prebuilt.listSync().whereType<Directory>().single;
  return p.join(host.path, 'bin');
}

int _compareVersions(String a, String b) {
  final left = a.split('.').map(int.tryParse).toList();
  final right = b.split('.').map(int.tryParse).toList();
  for (var i = 0; i < left.length && i < right.length; i++) {
    final diff = (left[i] ?? 0) - (right[i] ?? 0);
    if (diff != 0) return diff;
  }
  return left.length - right.length;
}

Uint8List _hexToBytes(String hex) {
  if (hex.length != 128) {
    throw ArgumentError('PRIVATE_KEY must be a 64-byte ed25519 key in hex');
  }
  return Uint8List.fromList([
    for (var i = 0; i < hex.length; i += 2)
      int.parse(hex.substring(i, i + 2), radix: 16),
  ]);
}
