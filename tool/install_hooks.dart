// מתקין את ה-git hooks המנוהלים בריפו (.githooks).
// הריצו פעם אחת בכל מחשב לאחר clone:
//
//     dart run tool/install_hooks.dart
//
// פועל על Windows / macOS / Linux ואינו דורש כלים נוספים.
import 'dart:io';

void main() {
  final result = Process.runSync('git', [
    'config',
    'core.hooksPath',
    '.githooks',
  ]);
  if (result.exitCode != 0) {
    stderr.writeln('שגיאה בהגדרת core.hooksPath:\n${result.stderr}');
    exit(result.exitCode);
  }

  // ב-macOS/Linux מוודאים שה-hook בר-הרצה (ב-Windows לא רלוונטי).
  if (!Platform.isWindows) {
    Process.runSync('chmod', ['+x', '.githooks/pre-commit']);
  }

  stdout.writeln('✓ git hooks הותקנו (core.hooksPath=.githooks).');
  stdout.writeln('  מעתה כל git commit יפרמט אוטומטית קבצי Dart/Rust מבוימים.');
}
