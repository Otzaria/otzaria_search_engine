import 'dart:io';

import 'package:code_assets/code_assets.dart';
import 'package:path/path.dart' as p;

import 'rust_target.dart';

/// The cargo flags of one crate, shared by the build hook and the publisher.
final class CargoOptions {
  const CargoOptions({
    required this.libName,
    this.crateDir = 'rust',
    this.noDefaultFeatures = false,
    this.features = const [],
  });

  /// The `[lib] name` of the crate, without platform prefix or extension.
  final String libName;
  final String crateDir;
  final bool noDefaultFeatures;
  final List<String> features;
}

/// Toolchain settings for one target, independent of where they came from.
final class TargetToolchain {
  const TargetToolchain({this.ndkClangDir, this.androidApi, this.appleVersion});

  /// The NDK `bin` directory holding the API-suffixed clang wrappers.
  final String? ndkClangDir;
  final int? androidApi;

  /// `MACOSX_DEPLOYMENT_TARGET` / `IPHONEOS_DEPLOYMENT_TARGET`.
  final String? appleVersion;

  factory TargetToolchain.fromCodeConfig(CodeConfig code) =>
      switch (code.targetOS) {
        OS.android => TargetToolchain(
          ndkClangDir: switch (code.cCompiler) {
            final compiler? => p.dirname(p.fromUri(compiler.compiler)),
            null => throw StateError('Android builds need the NDK compiler'),
          },
          androidApi: code.android.targetNdkApi,
        ),
        OS.iOS => TargetToolchain(appleVersion: '${code.iOS.targetVersion}.0'),
        OS.macOS => TargetToolchain(
          appleVersion: '${code.macOS.targetVersion}.0',
        ),
        _ => const TargetToolchain(),
      };
}

/// Environment cargo needs to compile and link for [target].
Map<String, String> cargoEnvironment(
  RustTarget target,
  TargetToolchain toolchain, {
  Map<String, String>? hostEnvironment,
  bool hostIsWindows = false,
}) {
  final host = hostEnvironment ?? Platform.environment;
  final env = <String, String>{};
  final key = target.triple.replaceAll('-', '_');
  switch (target.os) {
    case OS.android:
      // The API-suffixed wrapper sets the minimum API, so the binary never
      // links libc symbols newer than the app's minSdk.
      final clangTriple = target.triple == 'armv7-linux-androideabi'
          ? 'armv7a-linux-androideabi'
          : target.triple;
      final suffix = hostIsWindows ? '.cmd' : '';
      final dir = toolchain.ndkClangDir!;
      final clang = p.join(
        dir,
        '$clangTriple${toolchain.androidApi}-clang$suffix',
      );
      final exe = hostIsWindows ? '.exe' : '';
      env['CC_$key'] = clang;
      env['CXX_$key'] = p.join(
        dir,
        '$clangTriple${toolchain.androidApi}-clang++$suffix',
      );
      env['AR_$key'] = p.join(dir, 'llvm-ar$exe');
      env['RANLIB_$key'] = p.join(dir, 'llvm-ranlib$exe');
      env['CARGO_TARGET_${key.toUpperCase()}_LINKER'] = clang;
    case OS.iOS:
      env['IPHONEOS_DEPLOYMENT_TARGET'] = toolchain.appleVersion!;
    case OS.macOS:
      env['MACOSX_DEPLOYMENT_TARGET'] = toolchain.appleVersion!;
  }
  if (target.os == OS.iOS || target.os == OS.macOS) {
    // Xcode prepends its own tool directories, which break build scripts
    // compiled for the host.
    final path = host['PATH'];
    if (path != null) {
      env['PATH'] = path
          .split(':')
          .where((entry) => !entry.contains('Contents/Developer/'))
          .join(':');
    }
  }
  return env;
}

/// Compiles the crate for [target] and returns the dynamic library.
Future<File> cargoBuild({
  required String packageRoot,
  required CargoOptions options,
  required RustTarget target,
  required TargetToolchain toolchain,
  required String targetDir,
  void Function(String message)? log,
}) async {
  final crateDir = p.join(packageRoot, options.crateDir);
  final cargo = _tool('cargo');
  final rustupHomes = rustupEnvironment(cargo);
  final environment = {
    ...rustupHomes,
    ...cargoEnvironment(target, toolchain, hostIsWindows: Platform.isWindows),
  };
  // A cargo without rustup (a distribution package) builds its host target.
  final rustup = _findTool('rustup');
  if (rustup != null) {
    await _run(
      rustup,
      ['target', 'add', target.triple],
      workingDirectory: crateDir,
      environment: rustupHomes,
      log: log,
    );
  }
  await _run(
    cargo,
    [
      'build',
      '--release',
      '--locked',
      '--manifest-path',
      p.join(crateDir, 'Cargo.toml'),
      '--target',
      target.triple,
      '--target-dir',
      targetDir,
      if (options.noDefaultFeatures) '--no-default-features',
      if (options.features.isNotEmpty) ...[
        '--features',
        options.features.join(','),
      ],
    ],
    workingDirectory: crateDir,
    environment: environment,
    log: log,
  );
  final library = File(
    p.join(
      targetDir,
      target.triple,
      'release',
      target.libraryFileName(options.libName),
    ),
  );
  if (!library.existsSync()) {
    throw StateError('cargo did not produce ${library.path}');
  }
  return library;
}

/// Source files cargo recorded for the last build of [library].
List<Uri> cargoDependencies(File library) {
  final depFile = File(p.setExtension(library.path, '.d'));
  if (!depFile.existsSync()) return const [];
  final content = depFile.readAsStringSync();
  final colon = content.indexOf(': ');
  if (colon < 0) return const [];
  return [
    for (final path
        in content
            .substring(colon + 2)
            .replaceAll('\\\n', ' ')
            .split(RegExp(r'(?<!\\) +')))
      if (path.trim().isNotEmpty) Uri.file(path.trim().replaceAll(r'\ ', ' ')),
  ];
}

/// `CARGO_HOME` and `RUSTUP_HOME` for the rustup install that owns [cargo].
///
/// Build hooks do not receive these variables, so a CI runner that installs
/// rustup into its workspace would otherwise look for toolchains in `~/.rustup`.
Map<String, String> rustupEnvironment(
  String cargo, {
  Map<String, String>? hostEnvironment,
}) {
  final host = hostEnvironment ?? Platform.environment;
  final bin = p.dirname(cargo);
  final rustup = p.join(bin, p.basename(cargo).replaceFirst('cargo', 'rustup'));
  if (!File(rustup).existsSync()) return const {};
  final cargoHome = p.dirname(bin);
  // Only the rustup layout (`.cargo` beside `.rustup`) can be inferred; a
  // system-wide rustup in /usr/bin keeps its own defaults.
  if (p.basename(cargoHome) != '.cargo') return const {};
  final rustupHome = p.join(p.dirname(cargoHome), '.rustup');
  return {
    if (host['CARGO_HOME'] == null) 'CARGO_HOME': cargoHome,
    if (host['RUSTUP_HOME'] == null && Directory(rustupHome).existsSync())
      'RUSTUP_HOME': rustupHome,
  };
}

/// Prefers the tool on PATH, then rustup's default install location.
String _tool(String name) =>
    _findTool(name) ??
    (throw StateError(
      '$name was not found. Install Rust from https://rustup.rs to build the '
      'native library from source.',
    ));

String? _findTool(String name) {
  final exe = Platform.isWindows ? '$name.exe' : name;
  final home =
      Platform.environment['USERPROFILE'] ?? Platform.environment['HOME'];
  final fallback = home == null ? null : p.join(home, '.cargo', 'bin', exe);
  final onPath = (Platform.environment['PATH'] ?? '')
      .split(Platform.isWindows ? ';' : ':')
      .map((dir) => p.join(dir, exe))
      .where((candidate) => File(candidate).existsSync());
  if (onPath.isNotEmpty) return onPath.first;
  if (fallback != null && File(fallback).existsSync()) return fallback;
  return null;
}

Future<void> _run(
  String executable,
  List<String> arguments, {
  required String workingDirectory,
  Map<String, String>? environment,
  void Function(String message)? log,
}) async {
  log?.call('Running $executable ${arguments.join(' ')}');
  final process = await Process.start(
    executable,
    arguments,
    workingDirectory: workingDirectory,
    environment: environment,
  );
  final output = StringBuffer();
  await Future.wait([
    process.stdout.transform(const SystemEncoding().decoder).forEach((chunk) {
      output.write(chunk);
      stdout.write(chunk);
    }),
    process.stderr.transform(const SystemEncoding().decoder).forEach((chunk) {
      output.write(chunk);
      stderr.write(chunk);
    }),
  ]);
  final exitCode = await process.exitCode;
  if (exitCode != 0) {
    throw ProcessException(
      executable,
      arguments,
      'Exited with $exitCode:\n$output',
      exitCode,
    );
  }
}
