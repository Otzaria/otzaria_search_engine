import 'package:build_tool/src/target.dart';
import 'package:test/test.dart';

void main() {
  group('Windows host targets', () {
    test('selects ARM64 for an ARM64 host', () {
      expect(Target.windowsHostTarget('ARM64').rust, 'aarch64-pc-windows-msvc');
    });

    test('selects x64 for x64 and unknown hosts', () {
      expect(Target.windowsHostTarget('AMD64').rust, 'x86_64-pc-windows-msvc');
      expect(Target.windowsHostTarget(null).rust, 'x86_64-pc-windows-msvc');
    });
  });
}
