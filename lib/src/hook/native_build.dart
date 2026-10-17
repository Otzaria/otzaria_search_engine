import 'dart:io';

import 'package:code_assets/code_assets.dart';
import 'package:hooks/hooks.dart';
import 'package:path/path.dart' as p;

import 'cargo_build.dart';
import 'crate_hash.dart';
import 'precompiled.dart';
import 'rust_target.dart';

/// Builds one Rust crate into a bundled code asset.
///
/// Uses the signed precompiled binary of the crate hash when it is published,
/// and compiles from source otherwise or when the app sets the user define
/// `build_from_source: true` for this package. `skip_build: true` adds no
/// asset, for test suites that load a library they compiled themselves.
final class NativeBuild {
  const NativeBuild({
    required this.assetName,
    required this.cargo,
    required this.precompiled,
    this.extraCrateDirs = const [],
  });

  /// The asset id is `package:<package>/<assetName>`.
  final String assetName;
  final CargoOptions cargo;
  final PrecompiledBinaries precompiled;

  /// Crate directories beyond `src` whose content changes the binary.
  final List<String> extraCrateDirs;

  Future<void> run(BuildInput input, BuildOutputBuilder output) async {
    if (!input.config.buildCodeAssets) return;
    if (input.userDefines['skip_build'] == true) return;
    final code = input.config.code;
    if (code.linkModePreference == LinkModePreference.static) {
      throw UnsupportedError(
        '${input.packageName} ships a dynamic library only',
      );
    }

    final packageRoot = p.fromUri(input.packageRoot);
    final target = RustTarget.fromCodeConfig(code);
    final fileName = target.libraryFileName(cargo.libName);
    final hashInputs = CrateHash.inputFiles(
      packageRoot: packageRoot,
      crateDir: cargo.crateDir,
      extraCrateDirs: extraCrateDirs,
    );
    output.dependencies.addAll([for (final file in hashInputs) file.uri]);

    File? library;
    if (input.userDefines['build_from_source'] != true) {
      library = await precompiled.fetch(
        crateHash: CrateHash.compute(
          packageRoot: packageRoot,
          crateDir: cargo.crateDir,
          extraCrateDirs: extraCrateDirs,
        ),
        target: target,
        fileName: fileName,
        cacheDir: p.join(p.fromUri(input.outputDirectoryShared), 'precompiled'),
        log: stdout.writeln,
      );
    }
    if (library == null) {
      library = await cargoBuild(
        packageRoot: packageRoot,
        options: cargo,
        target: target,
        toolchain: TargetToolchain.fromCodeConfig(code),
        targetDir: p.join(p.fromUri(input.outputDirectoryShared), 'target'),
        log: stdout.writeln,
      );
      output.dependencies.addAll(cargoDependencies(library));
    }

    // Copied out of the shared directory: another target's build may replace
    // the file there while the bundler still reads this one.
    final bundled = File(p.join(p.fromUri(input.outputDirectory), fileName));
    library.copySync(bundled.path);
    output.assets.code.add(
      CodeAsset(
        package: input.packageName,
        name: assetName,
        linkMode: DynamicLoadingBundled(),
        file: bundled.uri,
      ),
    );
  }
}
