#!/usr/bin/env bash
# Runs the instrumented tests on a connected emulator against a local SFTP
# server standing in for the Storage Box, then removes the server.
#
#   scripts/device-test.sh [extra Gradle arguments]
#
# REPLIQUE_SFTP_HOSTNAME: a DNS name the emulator resolves to the host, for the
# by-name backup test (e.g. 10.0.2.2.nip.io; start the emulator with
# -dns-server 8.8.8.8 if the local resolver refuses private answers).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"$here/test-sftp.sh" start
trap '"$here/test-sftp.sh" stop' EXIT

# shellcheck disable=SC2207
args=($("$here/test-sftp.sh" args))
if [ -n "${REPLIQUE_SFTP_HOSTNAME:-}" ]; then
  args+=("-Pandroid.testInstrumentationRunnerArguments.sftpHostname=$REPLIQUE_SFTP_HOSTNAME")
fi
cd "$here/.."
./gradlew connectedDebugAndroidTest --console=plain "${args[@]}" "$@"
