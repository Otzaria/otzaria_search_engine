import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';
import 'package:path/path.dart' as p;

/// Identifies the crate content a precompiled binary was built from.
///
/// Covers the crate sources, its manifests and the build hook with its
/// `lib/src/hook` code, which holds the cargo flags. Text is hashed line by
/// line so a CRLF checkout of the same commit gives the same hash.
final class CrateHash {
  /// Bump when the published artifact set or its naming changes.
  static const _artifactSetVersion = 3;

  static String compute({
    required String packageRoot,
    String crateDir = 'rust',
    List<String> extraCrateDirs = const [],
  }) {
    final files = inputFiles(
      packageRoot: packageRoot,
      crateDir: crateDir,
      extraCrateDirs: extraCrateDirs,
    );
    final buffer = StringBuffer('artifact-set-v$_artifactSetVersion\n');
    for (final file in files) {
      buffer.writeln('== ${_relative(file, packageRoot)}');
      final bytes = file.readAsBytesSync();
      try {
        buffer.writeln(utf8.decode(bytes).replaceAll('\r\n', '\n'));
      } on FormatException {
        // Binary content is hashed as is.
        buffer.writeln(base64.encode(bytes));
      }
    }
    final digest = sha256.convert(utf8.encode(buffer.toString()));
    return digest.bytes
        .sublist(0, 16)
        .map((byte) => byte.toRadixString(16).padLeft(2, '0'))
        .join();
  }

  /// The files whose content defines the hash, in a stable order.
  static List<File> inputFiles({
    required String packageRoot,
    String crateDir = 'rust',
    List<String> extraCrateDirs = const [],
  }) {
    final crate = p.join(packageRoot, crateDir);
    final files = <File>[
      for (final dir in ['src', ...extraCrateDirs])
        ..._filesUnder(p.join(crate, dir)),
      for (final name in ['Cargo.toml', 'Cargo.lock', 'build.rs'])
        if (File(p.join(crate, name)).existsSync()) File(p.join(crate, name)),
      for (final dir in ['hook', 'lib/src/hook'])
        ..._filesUnder(
          p.join(packageRoot, dir),
        ).where((file) => file.path.endsWith('.dart')),
    ];
    files.sort(
      (a, b) => _relative(a, packageRoot).compareTo(_relative(b, packageRoot)),
    );
    return files;
  }

  // Separators differ between platforms, the published hash must not.
  static String _relative(File file, String root) =>
      p.relative(file.path, from: root).replaceAll(r'\', '/');

  static Iterable<File> _filesUnder(String dir) {
    final directory = Directory(dir);
    if (!directory.existsSync()) return const [];
    return directory
        .listSync(recursive: true, followLinks: false)
        .whereType<File>();
  }
}
