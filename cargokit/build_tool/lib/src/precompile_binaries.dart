/// This is copied from Cargokit (which is the official way to use it currently)
/// Details: https://fzyzcjy.github.io/flutter_rust_bridge/manual/integrate/builtin

import 'dart:io';

import 'package:ed25519_edwards/ed25519_edwards.dart';
import 'package:github/github.dart';
import 'package:logging/logging.dart';
import 'package:path/path.dart' as path;

import 'android_environment.dart';
import 'artifacts_provider.dart';
import 'builder.dart';
import 'cargo.dart';
import 'crate_hash.dart';
import 'options.dart';
import 'rustup.dart';
import 'target.dart';

final _log = Logger('precompile_binaries');

class PrecompileBinaries {
  PrecompileBinaries({
    required this.privateKey,
    required this.githubToken,
    required this.repositorySlug,
    required this.manifestDir,
    required this.targets,
    this.androidSdkLocation,
    this.androidNdkVersion,
    this.androidMinSdkVersion,
    this.tempDir,
  });

  final PrivateKey privateKey;
  final String githubToken;
  final RepositorySlug repositorySlug;
  final String manifestDir;
  final List<Target> targets;
  final String? androidSdkLocation;
  final String? androidNdkVersion;
  final int? androidMinSdkVersion;
  final String? tempDir;

  /// GitHub normalizes release asset names: every run of characters outside
  /// `[A-Za-z0-9._-]` is stored as a single dot, so an asset uploaded as
  /// `aarch64-linux-android_libc++_shared.so` lands as
  /// `aarch64-linux-android_libc._shared.so`. Uploads, downloads and
  /// verification must therefore all address the stored name — otherwise the
  /// C++ runtime is uploaded once and never found again, which drops the whole
  /// Android target from the precompiled set and forces consumers to build the
  /// crate locally. The name of the file on the device is unaffected; it comes
  /// from `Artifact.finalFileName`.
  static String assetName(String name) =>
      name.replaceAll(RegExp(r'[^A-Za-z0-9._-]+'), '.');

  static String fileName(Target target, String name) {
    return assetName('${target.rust}_$name');
  }

  static String signatureFileName(Target target, String name) {
    return '${fileName(target, name)}.sig';
  }

  Future<void> run() async {
    final crateInfo = CrateInfo.load(manifestDir);

    final targets = List.of(this.targets);
    if (targets.isEmpty) {
      targets.addAll([
        ...Target.buildableTargets(),
        if (androidSdkLocation != null) ...Target.androidTargets(),
      ]);
    }

    _log.info('Precompiling binaries for $targets');

    final hash = CrateHash.compute(manifestDir);
    _log.info('Computed crate hash: $hash');

    final String tagName = 'precompiled_$hash';

    final github = GitHub(auth: Authentication.withToken(githubToken));
    final repo = github.repositories;
    final release = await getOrCreateRelease(
      repo: repo,
      repositorySlug: repositorySlug,
      tagName: tagName,
      packageName: crateInfo.packageName,
      hash: hash,
    );

    final tempDir = this.tempDir != null
        ? Directory(this.tempDir!)
        : Directory.systemTemp.createTempSync('precompiled_');

    tempDir.createSync(recursive: true);

    final crateOptions = CargokitCrateOptions.load(
      manifestDir: manifestDir,
    );

    final buildEnvironment = BuildEnvironment(
      configuration: BuildConfiguration.release,
      crateOptions: crateOptions,
      targetTempDir: tempDir.path,
      manifestDir: manifestDir,
      crateInfo: crateInfo,
      isAndroid: androidSdkLocation != null,
      androidSdkPath: androidSdkLocation,
      androidNdkVersion: androidNdkVersion,
      androidMinSdkVersion: androidMinSdkVersion,
    );

    final rustup = Rustup();

    for (final target in targets) {
      final artifactNames = getArtifactNames(
        target: target,
        libraryName: crateInfo.packageName,
        remote: true,
      );
      final expectedAssetNames = <String>{
        for (final name in artifactNames) ...{
          PrecompileBinaries.fileName(target, name),
          PrecompileBinaries.signatureFileName(target, name),
        },
      };
      final existingAssets = (release.assets ?? [])
          .where((asset) => expectedAssetNames.contains(asset.name))
          .toList(growable: false);

      if (existingAssets.length == expectedAssetNames.length) {
        _log.info("All artifacts for $target already exist - skipping");
        continue;
      }

      // A failed upload may leave only a binary or only its signature. Delete
      // the target's partial set before rebuilding so a signature can never be
      // paired with bytes from a different build.
      for (final asset in existingAssets) {
        _log.warning('Deleting incomplete release asset ${asset.name}');
        await repo.deleteReleaseAsset(repositorySlug, asset);
      }

      _log.info('Building for $target');

      final builder =
          RustBuilder(target: target, environment: buildEnvironment);
      builder.prepare(rustup);
      final res = await builder.build();

      final assets = <CreateReleaseAsset>[];
      for (final name in artifactNames) {
        final file =
            name == androidCxxSharedRuntimeName && target.android != null
                ? buildEnvironment
                    .androidEnvironmentFor(target)
                    .packageCxxSharedRuntime(res)
                : File(path.join(res, name));
        if (!file.existsSync()) {
          throw Exception('Missing artifact: ${file.path}');
        }
        _verifyLinuxGlibcFloor(target, file);

        final data = file.readAsBytesSync();
        final create = CreateReleaseAsset(
          name: PrecompileBinaries.fileName(target, name),
          contentType: "application/octet-stream",
          assetData: data,
        );
        final signature = sign(privateKey, data);
        final signatureCreate = CreateReleaseAsset(
          name: signatureFileName(target, name),
          contentType: "application/octet-stream",
          assetData: signature,
        );
        bool verified = verify(public(privateKey), data, signature);
        if (!verified) {
          throw Exception('Signature verification failed');
        }
        assets.add(create);
        assets.add(signatureCreate);
      }
      _log.info('Uploading assets: ${assets.map((e) => e.name)}');
      for (final asset in assets) {
        // This seems to be failing on CI so do it one by one
        int retryCount = 0;
        while (true) {
          try {
            await repo.uploadReleaseAssets(release, [asset]);
            break;
          } on Exception catch (e) {
            if (retryCount == 10) {
              rethrow;
            }
            ++retryCount;
            _log.shout(
                'Upload failed (attempt $retryCount, will retry): ${e.toString()}');
            await Future.delayed(Duration(seconds: 2));
          }
        }
      }
    }

    _log.info('Cleaning up');
    tempDir.deleteSync(recursive: true);
  }

  static void _verifyLinuxGlibcFloor(Target target, File file) {
    if (!target.rust.endsWith('-unknown-linux-gnu')) {
      return;
    }

    final result = Process.runSync('objdump', ['-T', file.path]);
    if (result.exitCode != 0) {
      throw Exception('Unable to inspect ${file.path}: ${result.stderr}');
    }

    for (final match
        in RegExp(r'GLIBC_(\d+)\.(\d+)').allMatches(result.stdout.toString())) {
      final major = int.parse(match.group(1)!);
      final minor = int.parse(match.group(2)!);
      if (major > 2 || major == 2 && minor > 36) {
        throw Exception(
          '${file.path} requires GLIBC_$major.$minor; Linux artifacts must support GLIBC_2.36.',
        );
      }
    }
  }

  static Future<Release> getOrCreateRelease({
    required RepositoriesService repo,
    required RepositorySlug repositorySlug,
    required String tagName,
    required String packageName,
    required String hash,
  }) async {
    Release release;
    try {
      _log.info('Fetching release $tagName');
      release = await repo.getReleaseByTagName(repositorySlug, tagName);
    } on ReleaseNotFound {
      _log.info('Release not found - creating release $tagName');
      try {
        release = await repo.createRelease(
            repositorySlug,
            CreateRelease.from(
              tagName: tagName,
              name: 'Precompiled binaries ${hash.substring(0, 8)}',
              targetCommitish: null,
              isDraft: false,
              isPrerelease: false,
              body: 'Precompiled binaries for crate $packageName, '
                  'crate hash $hash.',
            ));
      } on GitHubError catch (e) {
        if (e.toString().contains('already_exists')) {
          _log.info('Release was created concurrently - re-fetching $tagName');
          release = await repo.getReleaseByTagName(repositorySlug, tagName);
        } else {
          rethrow;
        }
      }
    }
    return release;
  }
}
