#!/usr/bin/env bash
# Run from the generated probe app. Build before booting the emulator so Cargo
# and Gradle do not compete with the API-24 guest for the runner's resources.
set -euo pipefail

device=${ANDROID_SERIAL:-emulator-5554}
build_args=(build apk --debug --target integration_test/search_test.dart
  --target-platform android-x64
  --dart-define=INTEGRATION_TEST_SHOULD_REPORT_RESULTS_TO_NATIVE=false)
# `flutter test` does not support `--use-application-binary`. It repeats the
# build incrementally, reusing the matching target and Dart define above.
test_args=(test integration_test -d "$device" --no-pub)

build_apk() {
  local attempt status log
  log=$(mktemp)
  trap 'rm -f "$log"' RETURN
  for attempt in 1 2 3; do
    if timeout --kill-after=30s 1800s flutter "${build_args[@]}" \
      2>&1 | tee "$log"; then
      return 0
    else
      status=${PIPESTATUS[0]}
    fi
    # Retry only transport/rate-limit failures. A compile error or failed test
    # must stay red. Flutter's own 100ms retry is too short for Maven's 429s.
    if (( attempt == 3 )) || ! grep -Eq \
      'Received status code (429|5[0-9][0-9])|Connection (reset|timed out)|Read timed out|Remote host terminated the handshake' "$log"; then
      return "$status"
    fi
    echo "::warning::Android dependency download failed; retrying in $((attempt * 30)) seconds"
    sleep "$((attempt * 30))"
  done
}

ready() {
  local attempt
  # Bound every adb command too: even asking a stuck daemon for a property
  # can hang, so bounding only the number of iterations is insufficient.
  for attempt in {1..30}; do
    if timeout --kill-after=2s 5s adb -s "$device" shell getprop sys.boot_completed | grep -q '^1' \
      && timeout --kill-after=2s 5s adb -s "$device" shell pm path android | grep -q '^package:'; then
      return 0
    fi
    sleep 2
  done
  echo '::error::Android boot/package manager did not become ready'
  return 1
}

run_test() {
  local attempt status
  for attempt in 1 2; do
    ready
    # This is a disposable CI emulator. API 24's install verifier can stall
    # indefinitely while consulting a service unavailable in the system image.
    timeout --kill-after=2s 10s adb -s "$device" shell settings put global package_verifier_enable 0
    timeout --kill-after=2s 10s adb -s "$device" shell settings put global verifier_verify_adb_installs 0
    if timeout --kill-after=30s 300s flutter "${test_args[@]}"; then
      return 0
    else
      status=$?
    fi
    if (( status != 124 || attempt == 2 )); then
      return "$status"
    fi
    echo '::warning::Android integration timed out; rebooting the emulator before one retry'
    timeout --kill-after=2s 10s adb -s "$device" logcat -d -t 150 || true
    timeout --kill-after=2s 15s adb -s "$device" reboot
    # Give reboot time to disconnect before testing readiness again.
    sleep 10
  done
}

case "${1:-}" in
  check)
    # Parse the exact production arguments with the installed Flutter SDK.
    # --help avoids an SDK/device build but still rejects unsupported flags.
    flutter "${build_args[@]}" --help > /dev/null
    flutter "${test_args[@]}" --help > /dev/null
    ;;
  build) build_apk ;;
  test) run_test ;;
  *) echo 'Usage: android_integration.sh check|build|test' >&2; exit 64 ;;
esac
