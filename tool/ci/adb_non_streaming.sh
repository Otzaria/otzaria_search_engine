#!/usr/bin/env bash
# Installed temporarily as the disposable CI SDK's adb entry point. Flutter
# calls that absolute path, so changing PATH alone cannot select this transport.
set -euo pipefail

: "${OTZARIA_REAL_ADB:?OTZARIA_REAL_ADB must point to the original adb executable}"

if (( $# == 0 )); then
  exec "$OTZARIA_REAL_ADB"
fi

args=("$@")
command_index=0
while (( command_index < ${#args[@]} )); do
  case "${args[command_index]}" in
    -s|-t|-H|-P|-L)
      command_index=$((command_index + 2))
      ;;
    -a|-d|-e)
      command_index=$((command_index + 1))
      ;;
    *) break ;;
  esac
done

# Do not interpret shell arguments, option values or another subcommand's
# arguments as an install command. Leave invalid invocations for adb to report.
if (( command_index >= ${#args[@]} )) || [[ ${args[command_index]} != install ]]; then
  exec "$OTZARIA_REAL_ADB" "$@"
fi

install_args=("${args[@]:0:command_index+1}" --no-streaming)
for ((i = command_index + 1; i < ${#args[@]}; i++)); do
  case "${args[i]}" in
    --streaming|--no-streaming) ;;
    *) install_args+=("${args[i]}") ;;
  esac
done

echo '[otzaria-ci] adb install: using --no-streaming (push transport)' >&2
exec "$OTZARIA_REAL_ADB" "${install_args[@]}"
