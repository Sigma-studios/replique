"""A stand-in for Réplique's server in the NixOS VM test: the device endpoints
replique-client uses (docs/protocol.md), with everything it receives kept in a
JSON file so that a restart (the test stops it to simulate an outage) keeps
the history. GET /debug/state returns that history.

Usage: mock-server.py <port> <state-file> <storage-host> <storage-port> <storage-user>
"""

import json
import os
import re
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, STATE, HOST, SSH_PORT, USER = sys.argv[1:6]
TOKEN = "rpl_q2V0aGlzLWlzLWEtZmFrZS10b2tlbi1mb3ItdGVzdHM"
LOCK = threading.Lock()


def load():
    if os.path.exists(STATE):
        with open(STATE) as f:
            return json.load(f)
    return {"pairs": [], "jobs": {}, "runs": {}, "by_client_id": {}}


state = load()


def save():
    tmp = STATE + ".tmp"
    with open(tmp, "w") as f:
        json.dump(state, f, indent=2)
    os.replace(tmp, STATE)


def storage():
    # What the real server does at pairing: scan the box's host keys.
    scan = subprocess.run(
        ["ssh-keyscan", "-p", SSH_PORT, HOST], capture_output=True, text=True
    )
    keys = [l for l in scan.stdout.splitlines() if l and not l.startswith("#")]
    return {
        "host": HOST,
        "port": int(SSH_PORT),
        "username": USER,
        "repo_path": "/home/restic",
        "known_hosts": keys,
    }


class Handler(BaseHTTPRequestHandler):
    def reply(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def error(self, code, err, message):
        self.reply(code, {"error": err, "message": message})

    def body(self):
        n = int(self.headers.get("Content-Length", 0))
        return json.loads(self.rfile.read(n) or b"null")

    def authorized(self):
        if self.headers.get("Authorization") != f"Bearer {TOKEN}":
            self.error(401, "unauthorized", "bad token")
            return False
        return True

    def do_GET(self):
        with LOCK:
            if self.path == "/debug/state":
                return self.reply(200, state)
            if not self.authorized():
                return
            if self.path == "/api/v1/mirror-sources":
                s = storage()
                return self.reply(
                    200,
                    {
                        "sources": [
                            {
                                "device_id": "dev_PHONE",
                                "device_name": "Téléphone",
                                "storage": s,
                            }
                        ]
                    },
                )
            self.error(404, "not_found", self.path)

    def do_PUT(self):
        with LOCK:
            if not self.authorized():
                return
            m = re.fullmatch(r"/api/v1/jobs/([a-z0-9][a-z0-9_-]{0,63})", self.path)
            if not m:
                return self.error(404, "not_found", self.path)
            state["jobs"][m.group(1)] = self.body()
            save()
            self.reply(200, {"ok": True})

    def do_POST(self):
        with LOCK:
            body = self.body()
            if self.path == "/api/v1/pair":
                if body["code"].replace("-", "").upper() != "K7QM2XPA":
                    return self.error(400, "invalid_code", "unknown code")
                state["pairs"].append(body)
                save()
                return self.reply(
                    200,
                    {
                        "device_id": "dev_PC",
                        "device_name": "PC",
                        "report_token": TOKEN,
                        "storage": storage(),
                    },
                )
            if not self.authorized():
                return
            if self.path == "/api/v1/runs":
                cid = body["client_run_id"]
                if cid not in state["by_client_id"]:
                    rid = f"run_{len(state['runs']) + 1}"
                    state["by_client_id"][cid] = rid
                    state["runs"][rid] = {"start": body, "progress": [], "finish": None}
                    save()
                return self.reply(200, {"run_id": state["by_client_id"][cid]})
            m = re.fullmatch(r"/api/v1/runs/([^/]+)/(progress|finish)", self.path)
            if not m or m.group(1) not in state["runs"]:
                return self.error(404, "not_found", self.path)
            run = state["runs"][m.group(1)]
            if m.group(2) == "progress":
                run["progress"].append(body)
            else:
                if run["finish"] is not None and run["finish"] != body:
                    return self.error(409, "conflict", "already finished")
                run["finish"] = body
            save()
            self.reply(200, {"ok": True})


ThreadingHTTPServer(("0.0.0.0", int(PORT)), Handler).serve_forever()
