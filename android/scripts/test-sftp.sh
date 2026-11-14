#!/usr/bin/env bash
# A local SFTP server standing in for the Storage Box in the instrumented tests.
#
#   test-sftp.sh start   generate a throwaway key, start the container
#   test-sftp.sh args    print the Gradle -P options passing it to the tests
#   test-sftp.sh stop    remove the container
#
# The container mimics a Storage Box sub-account: the user is chrooted to its
# home, which it sees as "/", with a writable "/home" folder inside, so the
# repository path is /home/restic exactly as on Hetzner. The emulator reaches
# the host at 10.0.2.2.
set -euo pipefail

name="${REPLIQUE_SFTP_CONTAINER:-replique-test-sftp}"
port="${REPLIQUE_SFTP_PORT:-2224}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
state="$here/../build/test-sftp"

case "${1:-}" in
  start)
    mkdir -p "$state"
    [ -f "$state/id_ed25519" ] || ssh-keygen -q -t ed25519 -N '' -C replique-test -f "$state/id_ed25519"
    docker build -q -t replique-test-sftp "$here/sftp" >/dev/null
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker run -d --name "$name" -p "$port:22" \
      -v "$state/id_ed25519.pub:/etc/ssh/keys/replique.pub:ro" \
      replique-test-sftp >/dev/null
    # Wait for sshd, then record its host key the way the server would put it
    # in the pairing reply's known_hosts (with the emulator's view of the host).
    for _ in $(seq 1 30); do
      if ssh-keyscan -p "$port" -t ed25519 127.0.0.1 >"$state/keyscan" 2>/dev/null && [ -s "$state/keyscan" ]; then
        break
      fi
      sleep 1
    done
    [ -s "$state/keyscan" ] || { echo "sshd in $name did not come up" >&2; exit 1; }
    echo "started $name on port $port" >&2
    ;;
  args)
    key="$(base64 <"$state/id_ed25519" | tr -d '\n')"
    hostkey="$(cut -d' ' -f2- "$state/keyscan" | head -n 1)"
    hostkey64="$(printf '%s' "$hostkey" | base64 | tr -d '\n')"
    echo "-Pandroid.testInstrumentationRunnerArguments.sftpPort=$port" \
      "-Pandroid.testInstrumentationRunnerArguments.sftpKey=$key" \
      "-Pandroid.testInstrumentationRunnerArguments.sftpHostKey=$hostkey64"
    ;;
  stop)
    docker rm -f "$name" >/dev/null 2>&1 || true
    ;;
  *)
    echo "usage: test-sftp.sh start|args|stop" >&2
    exit 2
    ;;
esac
