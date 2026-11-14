#!/usr/bin/env bash
# The real client against the real server, on this machine, with no Storage Box:
# the server simulates sub-accounts under a local folder (`local_root`) and the
# client's job points at that folder. It adds a device through the website the
# way a browser would, pairs, backs up, then stops the server during a second run
# and checks the spooled reports arrive with a third. Needs restic and curl.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cargo build -q --manifest-path "$root/Cargo.toml" -p replique-server -p replique-client
server_bin="$root/target/debug/replique-server"
client_bin="$root/target/debug/replique-client"

work=$(mktemp -d)
port=${REPLIQUE_E2E_PORT:-18080}
url="http://127.0.0.1:$port"
server_pid=
cleanup() {
  [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

echo "e2e-admin-password" | "$server_bin" hash-password >"$work/admin.hash" 2>/dev/null
cat >"$work/server.toml" <<TOML
listen = "127.0.0.1:$port"
public_url = "$url"
state_dir = "$work/state"
admin_password_hash_file = "$work/admin.hash"
[storage_box]
local_root = "$work/box"
TOML

start_server() {
  "$server_bin" -c "$work/server.toml" >>"$work/server.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 50); do
    curl -sf -o /dev/null "$url/healthz" && return
    sleep 0.2
  done
  echo "the server did not start:" >&2
  cat "$work/server.log" >&2
  exit 1
}
stop_server() {
  kill "$server_pid"
  wait "$server_pid" 2>/dev/null || true
  server_pid=
}

# A tree with a Unity project, whose Library/ must be excluded by the scan, and
# a `$` in a configured exclude, which restic would otherwise expand.
data="$work/data"
mkdir -p "$data/docs" "$data/projects/game/"{Assets,ProjectSettings,Library}
echo hello >"$data/docs/a.txt"
echo junk >"$data/projects/game/Library/cache.bin"
echo scene >"$data/projects/game/Assets/s.unity"

start_server
jar="$work/cookies"
field() { grep -o "name=\"$1\" value=\"[^\"]*\"" | head -1 | sed 's/.*value="//;s/"$//'; }
token=$(curl -sf -c "$jar" -b "$jar" "$url/login" | field token)
curl -sf -c "$jar" -b "$jar" -o /dev/null --data-urlencode "password=e2e-admin-password" \
  --data-urlencode "token=$token" "$url/login"
csrf=$(curl -sf -c "$jar" -b "$jar" "$url/devices/new" | field csrf)
device_url=$(curl -sf -c "$jar" -b "$jar" -o /dev/null -w '%{redirect_url}' \
  --data-urlencode "csrf=$csrf" -d name=PC -d kind=nixos "$url/devices")
code=$(curl -sf -b "$jar" "$device_url" | grep -oE "replique-client pair $url [A-Z0-9-]+" | head -1 | awk '{print $4}')
[ -n "$code" ] || { echo "no pairing code on $device_url" >&2; exit 1; }

repo="$work/box/replique/admin/pc/restic"
cat >"$work/client.json" <<JSON
{
  "server": "$url",
  "state_dir": "$work/client",
  "jobs": {
    "home": {
      "kind": "backup",
      "repository": "$repo",
      "paths": ["$data"],
      "exclude": ["$data/docs/\$RECYCLE.BIN"],
      "exclude_caches": true,
      "generated_excludes_for": ["$data/projects"],
      "schedule": "daily",
      "retention": { "policy": "keep-deleted" },
      "check_subset": "5%"
    }
  }
}
JSON
client() { "$client_bin" --config "$work/client.json" "$@"; }

client pair --yes --no-init "$url" "$code" >/dev/null
client run home >"$work/run1.log" 2>&1
grep -q "Library" <(client excludes home 2>/dev/null) || { echo "Unity Library/ not excluded" >&2; exit 1; }
grep -q '?RECYCLE.BIN' <(client excludes home 2>/dev/null) || { echo '$ not escaped' >&2; exit 1; }

stop_server
rm "$data/docs/a.txt"
echo v2 >"$data/docs/new.txt"
client run home >"$work/run2.log" 2>&1
grep -q "spool" "$work/run2.log" || { echo "run 2 did not spool" >&2; exit 1; }

start_server
client run home >"$work/run3.log" 2>&1
grep -q "3 sent, 0 refused, 0 still waiting" "$work/run3.log" || {
  echo "spooled reports were not replayed:" >&2
  cat "$work/run3.log" >&2
  exit 1
}

# The page wraps snapshot ids in markup: strip the tags before matching.
page=$(curl -sf -b "$jar" "$device_url" | sed 's/<[^>]*>//g')
snapshots=$(grep -oE 'snapshot [0-9a-f]{8}' <<<"$page" | sort -u | wc -l | tr -d ' ')
[ "$snapshots" = 3 ] || { echo "the server shows $snapshots snapshots, expected 3" >&2; exit 1; }
echo "e2e: paired, 3 backups each with prune and check, outage spooled and replayed"
