#!/usr/bin/env python3
"""A stand-in for Réplique's server, for trying the app on an emulator.

Implements the device side of docs/protocol.md just enough to pair, receive
reports and answer safe-to-delete:

* POST /api/v1/pair accepts any code and installs the phone's public key on
  the test SFTP server (by appending it to the authorized keys file that
  scripts/test-sftp.sh mounts), as the real server does on the Storage Box.
* GET /api/v1/device/config serves android_default() for a folder of choice,
  with an ETag.
* Every report is printed and appended to a JSON-lines log.
* GET /api/v1/devices/{id}/safe-to-delete names the newest snapshot a
  successful check listed, as if the PC had mirrored it too.

    fake-server.py --port 8787 --authorized-keys build/test-sftp/id_ed25519.pub \
        --known-host "$(cut -d' ' -f2- build/test-sftp/keyscan)" --folder /storage/emulated/0/DCIM

Development only: no TLS, no authentication worth the name.
"""

import argparse
import json
import re
import sys
import uuid
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STATE = {"runs": {}, "client_runs": {}, "checks": [], "snapshots": {}}


def now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class Handler(BaseHTTPRequestHandler):
    def reply(self, status, body=None, headers=None):
        data = b"" if body is None else json.dumps(body).encode()
        self.send_response(status)
        if body is not None:
            self.send_header("Content-Type", "application/json")
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def log_report(self, kind, payload):
        line = {"at": now(), "kind": kind, "path": self.path, "body": payload}
        print(json.dumps(line, ensure_ascii=False), flush=True)
        with open(ARGS.log, "a") as f:
            f.write(json.dumps(line, ensure_ascii=False) + "\n")

    def authorized(self):
        if self.headers.get("Authorization") != "Bearer " + TOKEN:
            self.reply(401, {"error": "unauthorized", "message": "bad token"})
            return False
        return True

    def do_POST(self):
        if self.path == "/api/v1/pair":
            req = self.body()
            self.log_report("pair", req)
            with open(ARGS.authorized_keys, "a") as f:
                f.write("\n" + req["ssh_public_key"].strip() + "\n")
            repo = "/home/e2e-%s/restic" % uuid.uuid4().hex[:8]
            return self.reply(200, {
                "device_id": "dev_fake",
                "device_name": req.get("device_name") or "Téléphone",
                "report_token": TOKEN,
                "storage": {
                    "host": ARGS.sftp_host,
                    "port": ARGS.sftp_port,
                    "username": "replique",
                    "repo_path": repo,
                    "known_hosts": ["[%s]:%d %s" % (ARGS.sftp_host, ARGS.sftp_port, ARGS.known_host)] if ARGS.known_host else [],
                },
            })
        if not self.authorized():
            return
        if self.path == "/api/v1/runs":
            req = self.body()
            self.log_report("start", req)
            run_id = STATE["client_runs"].setdefault(req["client_run_id"], "run_%d" % (len(STATE["client_runs"]) + 1))
            STATE["runs"][run_id] = req
            return self.reply(200, {"run_id": run_id})
        m = re.fullmatch(r"/api/v1/runs/([^/]+)/(progress|finish)", self.path)
        if m:
            req = self.body()
            self.log_report(m.group(2), req)
            run = STATE["runs"].get(m.group(1), {})
            if m.group(2) == "finish" and run.get("kind") == "backup" and req.get("summary"):
                sid = req["summary"].get("snapshot_id")
                if sid:
                    STATE["snapshots"][sid] = req["summary"].get("backup_start") or now()
            if m.group(2) == "finish" and run.get("kind") == "check" and req.get("status") == "success":
                STATE["checks"].append((req["finished_at"], req["check"]["snapshot_ids"]))
            return self.reply(200, {"ok": True})
        self.reply(404, {"error": "not_found", "message": self.path})

    def do_PUT(self):
        if not self.authorized():
            return
        if self.path.startswith("/api/v1/jobs/"):
            self.log_report("job", self.body())
            return self.reply(200, {"ok": True})
        self.reply(404, {"error": "not_found", "message": self.path})

    def do_GET(self):
        if not self.authorized():
            return
        if self.path == "/api/v1/device/config":
            etag = '"fake-1"'
            if self.headers.get("If-None-Match") == etag:
                return self.reply(304, headers={"ETag": etag})
            config = {
                "job": "phone",
                "folders": [{"path": ARGS.folder, "excludes": []}],
                "excludes": ["/storage/emulated/0/Android/data", "/storage/emulated/0/Android/obb", ".thumbnails",
                             ".trashed-*", ".pending-*", ".cache", "cache", "Cache"],
                "conditions": {"unmetered_only": True, "charging_only": True},
                "interval_hours": 24,
                "retention": {"policy": "never-prune"},
            }
            return self.reply(200, config, {"ETag": etag})
        if self.path.startswith("/api/v1/devices/dev_fake/safe-to-delete"):
            if not STATE["checks"]:
                return self.reply(200, {})
            verified_at, ids = STATE["checks"][-1]
            known = [(STATE["snapshots"][i], i) for i in ids if i in STATE["snapshots"]]
            if not known:
                return self.reply(200, {})
            snapshot_time, sid = max(known)
            return self.reply(200, {"snapshot": {
                "snapshot_id": sid, "snapshot_time": snapshot_time, "verified_at": verified_at,
                "mirrored_at": now(), "mirrored_by": "PC",
            }})
        self.reply(404, {"error": "not_found", "message": self.path})

    def log_message(self, fmt, *args):
        sys.stderr.write("%s %s\n" % (self.command, self.path))


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=8787)
    p.add_argument("--authorized-keys", required=True)
    p.add_argument("--known-host", default="", help="the SFTP server's host key, 'ssh-ed25519 AAAA…'")
    p.add_argument("--sftp-host", default="10.0.2.2")
    p.add_argument("--sftp-port", type=int, default=2224)
    p.add_argument("--folder", default="/storage/emulated/0")
    p.add_argument("--log", default="fake-server.jsonl")
    ARGS = p.parse_args()
    TOKEN = "rpl_" + "f" * 43
    ThreadingHTTPServer(("0.0.0.0", ARGS.port), Handler).serve_forever()
