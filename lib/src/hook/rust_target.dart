import 'package:code_assets/code_assets.dart';

/// A Rust target triple together with the operating system it builds for.
final class RustTarget {
  const RustTarget(this.triple, this.os);

  final String triple;
  final OS os;

  /// Every target the precompiled binaries cover.
  static const all = [
    RustTarget('aarch64-linux-android', OS.android),
    RustTarget('armv7-linux-androideabi', OS.android),
    RustTarget('x86_64-linux-android', OS.android),
    RustTarget('aarch64-apple-ios', OS.iOS),
    RustTarget('aarch64-apple-ios-sim', OS.iOS),
    RustTarget('x86_64-apple-ios', OS.iOS),
    RustTarget('aarch64-apple-darwin', OS.macOS),
    RustTarget('x86_64-apple-darwin', OS.macOS),
    RustTarget('aarch64-unknown-linux-gnu', OS.linux),
    RustTarget('x86_64-unknown-linux-gnu', OS.linux),
    RustTarget('aarch64-pc-windows-msvc', OS.windows),
    RustTarget('x86_64-pc-windows-msvc', OS.windows),
  ];

  static RustTarget fromTriple(String triple) => all.firstWhere(
    (target) => target.triple == triple,
    orElse: () => throw ArgumentError.value(triple, 'triple', 'Unsupported'),
  );

  /// The target a build hook invocation asks for.
  static RustTarget fromCodeConfig(CodeConfig code) {
    final triple = switch ((code.targetOS, code.targetArchitecture)) {
      (OS.android, Architecture.arm64) => 'aarch64-linux-android',
      (OS.android, Architecture.arm) => 'armv7-linux-androideabi',
      (OS.android, Architecture.x64) => 'x86_64-linux-android',
      (OS.iOS, Architecture.arm64)
          when code.iOS.targetSdk == IOSSdk.iPhoneSimulator =>
        'aarch64-apple-ios-sim',
      (OS.iOS, Architecture.arm64) => 'aarch64-apple-ios',
      (OS.iOS, Architecture.x64) => 'x86_64-apple-ios',
      (OS.macOS, Architecture.arm64) => 'aarch64-apple-darwin',
      (OS.macOS, Architecture.x64) => 'x86_64-apple-darwin',
      (OS.linux, Architecture.arm64) => 'aarch64-unknown-linux-gnu',
      (OS.linux, Architecture.x64) => 'x86_64-unknown-linux-gnu',
      (OS.windows, Architecture.arm64) => 'aarch64-pc-windows-msvc',
      (OS.windows, Architecture.x64) => 'x86_64-pc-windows-msvc',
      (final os, final arch) => throw UnsupportedError(
        'No Rust target for $os on $arch',
      ),
    };
    return fromTriple(triple);
  }

  /// The dynamic library cargo produces for crate library [libName].
  String libraryFileName(String libName) => os.dylibFileName(libName);

  @override
  String toString() => triple;
}
