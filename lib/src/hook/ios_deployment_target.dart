import 'dart:io';

/// Refuses to sign a library that its Flutter framework cannot describe.
Future<void> checkIosDeploymentTarget(
  File library, {
  required String maximumVersion,
}) async {
  final arguments = ['vtool', '-show-build', library.path];
  final result = await Process.run('xcrun', arguments);
  if (result.exitCode != 0) {
    throw ProcessException(
      'xcrun',
      arguments,
      '${result.stderr}',
      result.exitCode,
    );
  }
  validateIosDeploymentTarget(
    '${result.stdout}',
    maximumVersion: maximumVersion,
  );
}

/// Checks both modern LC_BUILD_VERSION and legacy LC_VERSION_MIN_IPHONEOS
/// output from `vtool -show-build`, including every slice of a fat binary.
void validateIosDeploymentTarget(
  String buildVersions, {
  required String maximumVersion,
}) {
  final versions = <String>[];
  for (final command in buildVersions.split(
    RegExp(r'^[ \t]*Load command \d+', multiLine: true),
  )) {
    final String field;
    if (command.contains('cmd LC_BUILD_VERSION')) {
      // Its nested tool records also have a "version" (the linker version).
      field = 'minos';
    } else if (command.contains('cmd LC_VERSION_MIN_IPHONEOS')) {
      field = 'version';
    } else {
      continue;
    }
    final match = RegExp(
      '^[ \\t]*$field[ \\t]+(\\d+(?:\\.\\d+)*)[ \\t]*\$',
      multiLine: true,
    ).firstMatch(command);
    if (match == null) {
      throw StateError('Missing $field in iOS load command');
    }
    versions.add(match.group(1)!);
  }
  if (versions.isEmpty) {
    throw StateError('No iOS deployment target found in vtool output');
  }
  final maximum = maximumVersion.split('.').map(int.parse).toList();
  for (final version in versions) {
    final actual = version.split('.').map(int.parse).toList();
    for (var i = 0; i < actual.length || i < maximum.length; i++) {
      final left = i < actual.length ? actual[i] : 0;
      final right = i < maximum.length ? maximum[i] : 0;
      if (left > right) {
        throw StateError(
          'iOS binary requires $version, above the supported framework '
          'minimum $maximumVersion. Refusing to publish it.',
        );
      }
      if (left < right) break;
    }
  }
}
