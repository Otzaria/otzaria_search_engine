import 'dart:async';
import 'dart:io';
import 'dart:typed_data';

import 'package:ed25519_edwards/ed25519_edwards.dart' as ed;
import 'package:path/path.dart' as p;

import 'rust_target.dart';

/// Where signed precompiled binaries are published, one release per crate hash.
final class PrecompiledBinaries {
  const PrecompiledBinaries({
    required this.urlPrefix,
    required this.publicKeyHex,
  });

  /// Release download URL up to the crate hash, e.g. `.../download/precompiled_`.
  final String urlPrefix;
  final String publicKeyHex;

  /// The release tag for [crateHash].
  static String tagFor(String crateHash) => 'precompiled_$crateHash';

  /// The release asset name of [fileName] built for [target].
  static String assetName(RustTarget target, String fileName) =>
      '${target.triple}_$fileName'.replaceAll(RegExp(r'[^A-Za-z0-9._-]+'), '.');

  Uri assetUri(String crateHash, String asset) =>
      Uri.parse('$urlPrefix$crateHash/$asset');

  bool verify(Uint8List data, Uint8List signature) =>
      ed.verify(ed.PublicKey(_hexToBytes(publicKeyHex)), data, signature);

  /// Downloads and verifies [fileName] for [target] into [cacheDir].
  ///
  /// Returns null when the release has no such asset, cannot be reached or
  /// serves bytes that fail verification, so the caller builds from source;
  /// unverified bytes are never used. [strict] throws on a broken release
  /// instead, for the publisher's check.
  Future<File?> fetch({
    required String crateHash,
    required RustTarget target,
    required String fileName,
    required String cacheDir,
    void Function(String message)? log,
    bool strict = false,
  }) async {
    final asset = assetName(target, fileName);
    final cached = File(p.join(cacheDir, crateHash, asset));
    final cachedSignature = File('${cached.path}.sig');
    if (cached.existsSync() && cachedSignature.existsSync()) {
      if (verify(cached.readAsBytesSync(), cachedSignature.readAsBytesSync())) {
        return cached;
      }
      cached.deleteSync();
    }

    final Uint8List? signature;
    final Uint8List? data;
    try {
      signature = await _download(assetUri(crateHash, '$asset.sig'));
      if (signature == null) {
        log?.call('No precompiled $asset for crate hash $crateHash');
        return null;
      }
      data = await _download(assetUri(crateHash, asset));
    } on IOException catch (error) {
      log?.call('Precompiled $asset unreachable ($error)');
      return null;
    } on TimeoutException catch (error) {
      log?.call('Precompiled $asset unreachable ($error)');
      return null;
    }
    // A filtering proxy or a release mid-upload also lands here.
    final problem = data == null
        ? 'Release for $crateHash has $asset.sig but not $asset'
        : verify(data, signature)
        ? null
        : 'Signature verification failed for ${assetUri(crateHash, asset)}';
    if (problem != null) {
      if (strict) throw StateError(problem);
      log?.call('WARNING: $problem; building from source instead.');
      return null;
    }
    cached.parent.createSync(recursive: true);
    // Written beside the target first so a concurrent hook never reads half.
    final partial = File('${cached.path}.$pid.partial')
      ..writeAsBytesSync(data!);
    File('${cachedSignature.path}.$pid.partial')
      ..writeAsBytesSync(signature)
      ..renameSync(cachedSignature.path);
    partial.renameSync(cached.path);
    return cached;
  }

  /// Returns the body, or null on 404.
  static Future<Uint8List?> _download(Uri uri) async {
    final client = HttpClient()
      ..connectionTimeout = const Duration(seconds: 30);
    try {
      for (var attempt = 1; ; attempt++) {
        try {
          final request = await client.getUrl(uri);
          final response = await request.close().timeout(
            const Duration(minutes: 5),
          );
          if (response.statusCode == HttpStatus.notFound) {
            await response.drain<void>();
            return null;
          }
          if (response.statusCode != HttpStatus.ok) {
            await response.drain<void>();
            throw HttpException('HTTP ${response.statusCode}', uri: uri);
          }
          final builder = BytesBuilder(copy: false);
          await response
              .forEach(builder.add)
              .timeout(const Duration(minutes: 10));
          return builder.takeBytes();
        } on IOException {
          if (attempt >= 3) rethrow;
          await Future<void>.delayed(Duration(seconds: attempt));
        }
      }
    } finally {
      client.close(force: true);
    }
  }

  static Uint8List _hexToBytes(String hex) => Uint8List.fromList([
    for (var i = 0; i < hex.length; i += 2)
      int.parse(hex.substring(i, i + 2), radix: 16),
  ]);
}
