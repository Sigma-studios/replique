# Réplique protocol, v1

How devices (the NixOS client, the Android app, façade) talk to Réplique's server.
The Rust types are in `crates/replique-proto`; `proto/fixtures/` holds one example
of each message, parsed by the Rust tests and by the Android app's tests. When this
document and the crate disagree, the crate is right and this document has a bug.

## Principles

* **Devices back up on their own.** Each device encrypts with restic and uploads
  straight to its own Storage Box sub-account. Réplique never receives backup
  data; it receives reports about it. A device whose server is down still backs
  up, and replays its reports later.
* **Additive changes only.** No message rejects unknown fields, so either side may
  add optional fields. Enums read an unknown value as `other` where one exists.
  Removing or renaming a field, or changing what it means, needs `/api/v2`.
* **JSON everywhere**, UTF-8, `Content-Type: application/json`. Timestamps are
  RFC 3339 strings in UTC (`2026-11-14T03:12:45Z`); receivers accept any offset.
  Sizes are bytes, durations are seconds.
* **Ids are opaque strings** chosen by the server (`dev_…`, `run_…`). Devices
  store and send them back, never parse them.

## Authentication

| Who | How |
| --- | --- |
| The admin (website) | Password login → HTTP-only, `Secure`, `SameSite=Lax` session cookie; every form carries a CSRF token. Not part of this API. |
| A device | `Authorization: Bearer rpl_<43 characters>`: the report token, 32 random bytes in unpadded base64url. The server stores only its SHA-256. |
| Pairing | No token: the one-time pairing code is the credential. |

A report token is not a login. It can declare its own device's jobs, report its
own device's runs, and read its own device's config and safe-to-delete answer.
Anything else is `403 forbidden`; an unknown or revoked token is `401
unauthorized`. Revoking one device's token on the website touches no other.

Clients identify themselves with `User-Agent: replique-client/0.1.0` (or
`replique-android/…`, `facade/…`).

## Errors

Every non-2xx reply has this body (`error.json`):

```json
{ "error": "code_expired", "message": "This pairing code expired; create a new one on the website." }
```

`error` is a stable code: `unauthorized` (401), `forbidden` (403), `not_found`
(404), `bad_request` (400), `invalid_code` and `code_expired` (400, pairing),
`conflict` (409), `upstream` (502: Hetzner's API failed; retry). `message` is for
humans. Clients treat 5xx and network errors as "server unreachable" (spool and
retry later) and 4xx as a bug or a revoked token (report locally, don't retry
forever).

## Pairing

A device is only ever added on the website. The admin picks a name and kind; the
server creates the device's sub-account on the Storage Box (home
`replique/<owner-slug>/<device-slug>`, SSH on, reachable from outside, a random
password) and its read-only twin, then shows a one-time **pairing code** valid
15 minutes, as a QR code and as a command line:

```
replique://pair?server=https%3A%2F%2Freplique.sigma-dev.eu&code=K7QM-2XPA
replique-client pair https://replique.sigma-dev.eu K7QM-2XPA
```

Codes are 8 characters from `ABCDEFGHJKMNPQRSTUVWXYZ23456789`, shown as
`XXXX-XXXX`; comparison ignores case, spaces and dashes.

The device then generates an ed25519 SSH key and its restic password (both stay
on the device; the password is shown once, to be saved in a password manager) and
calls:

### `POST /api/v1/pair`

Body (`pair_request.json`):

```json
{
  "code": "K7QM-2XPA",
  "device_name": "PC",
  "kind": "nixos",
  "ssh_public_key": "ssh-ed25519 AAAAC3Nza… replique-client@pc",
  "client_version": "replique-client 0.1.0"
}
```

`kind` is `nixos`, `android`, `facade` or `other`. The name and kind chosen on the
website win; the ones sent here are informative.

The server installs the public key on the device's sub-account (using its
password once), writes its own key into the read-only twin's
`authorized_keys` (inside the repository folder, through the same login), then
resets the password to a random value nobody keeps. Reply
(`pair_response.json`):

```json
{
  "device_id": "dev_01JCX4M8Q2N7R5T3V9W1Y6Z0AB",
  "device_name": "PC",
  "report_token": "rpl_6sR5g4u2YYcjoyYnOkbpBimLJQPI8qahB5kL4lVSOgk",
  "storage": {
    "host": "u123456.your-storagebox.de",
    "port": 23,
    "username": "u123456-sub1",
    "repo_path": "/home/restic",
    "known_hosts": ["[u123456.your-storagebox.de]:23 ssh-ed25519 AAAA…"]
  }
}
```

The token appears only in this reply. The code is spent once the reply is sent;
if installing the key fails, the reply is `502 upstream` and the code stays valid
for a retry until it expires.

**Adopting an existing repository** (the PC's `u123456-sub1`): the website's
"import existing" action registers the sub-account and creates its twin, then
shows a pairing code like any other. The device pairs with its existing key
(`replique-client pair --adopt`); the server leaves the sub-account's keys
alone. It does set a password on it once, nobody using it (the device logs in
with its key), to write the twin's `authorized_keys`, then replaces it.

After pairing the device initialises its repository if needed: `restic cat
config` exits 10 when there is none, then `restic init`. Running that again is
harmless.

## Declaring jobs

### `PUT /api/v1/jobs/{name}`

Sent at startup and whenever the job's configuration changes. Job names are 1–64
characters of `a-z 0-9 - _`, starting with a letter or digit. The server keeps the
latest declaration and shows it on the website. Reply: `{"ok": true}`.

```json
{
  "kind": "backup",
  "schedule": "daily",
  "expected_interval_secs": 86400,
  "rules": {
    "paths": ["/home/sigma"],
    "excludes": ["/home/sigma/.cache", "/home/sigma/.rustup"],
    "exclude_caches": true,
    "generated_excludes_for": ["/home/sigma/Projects"],
    "retention": { "policy": "keep-deleted", "keep": { "daily": 7, "weekly": 4, "monthly": 12 } },
    "check_subset": "5%"
  }
}
```

* `kind`: `backup` (restic into the device's own repository) or `mirror` (an
  encrypted byte copy of another device's repository).
* `schedule`: a systemd `OnCalendar` expression, `every 6h` for interval
  schedulers (Android), or absent for a manual job.
* `expected_interval_secs`: what is normal between two runs. The silent-job alert
  fires after this plus a margin (`daily` → 26 h); the admin can change it per job
  on the website. Absent: the server derives it from `schedule`, or never alerts
  for a manual job.
* `rules.excludes` are written as configured. Devices escape them for restic
  themselves: restic expands `$VARS` in exclude patterns, so a literal `$` is
  written `?` (`$RECYCLE.BIN` → `?RECYCLE.BIN`).
* `rules.mirror_source` / `rules.mirror_path` (mirror jobs): the source device id
  and where the copy lives locally.

### Retention policies

restic prunes whole snapshots, not files: a file's data disappears once no kept
snapshot holds it. `rules.retention.policy` is one of:

| Policy | What the device does | What can still be lost |
| --- | --- | --- |
| `never-prune` | never runs `forget` | nothing |
| `keep-deleted` (default for computers) | before pruning, diffs each snapshot about to be dropped against the next kept one (`restic diff`); one holding the last copy of a removed file is tagged `replique-keep`; then `forget --prune --keep-tag replique-keep` plus `keep` (default `daily 7, weekly 4, monthly 12`) | old versions of files that still exist |
| `versions` | `forget --prune` with `keep` | deleted files, once their last snapshot ages out |

`keep` holds restic's `--keep-*` options: `last`, `hourly`, `daily`, `weekly`,
`monthly`, `yearly` (a count or `"unlimited"`) and `within` (a duration like
`14d`). The phone always uses `never-prune`; mirror jobs have no retention.

### `GET /api/v1/mirror-sources`

The repositories this device may mirror (`mirror_sources.json`). When the admin
assigns a mirror on the website (the PC copying the phone), the server adds
this device's SSH public key to the source's read-only twin, and lists the
twin's login here, with `repo_path` `/home` (the twin's home is the repository
folder). The twin cannot write, so restic commands through it need `--no-lock`;
a copy should skip the folder's `.ssh/`.

## The phone's rules

### `GET /api/v1/device/config`

The phone's backup rules, edited on the website (`device_config.json`). The reply
has an `ETag`; the app sends it back as `If-None-Match` and gets `304` while
nothing changed. Until the admin edits anything, the server answers with
`DeviceConfig::android_default()`: all of `/storage/emulated/0` minus
`Android/data`, `Android/obb`, thumbnails, the media store's trash and pending
files, and caches; daily; on unmetered networks while charging; `never-prune`.

Each folder can add its own excludes, a maximum file size, extensions to skip, or
a list of the only extensions to keep. `conditions` apply to scheduled runs, not
to "Back up now". `daily_at` (`"02:00"`, the phone's own time) runs the backup
once a day at that time instead of every `interval_hours`; a run due while the
conditions are not met waits for them.

## Runs

Every restic operation is reported as a run: start, progress, finish. A backup
job's prune and check are their own runs (kinds `prune`, `check`) of the same job.

### `POST /api/v1/runs`

```json
{
  "client_run_id": "6f1c2a9e-3b7d-4e58-9a0c-1d2e3f4a5b6c",
  "job": "hetzner",
  "kind": "backup",
  "started_at": "2026-11-14T03:12:45Z",
  "restic_version": "0.19.1",
  "client_version": "replique-client 0.1.0"
}
```

Reply: `{"run_id": "run_…"}`. `client_run_id` is a UUID the device generates; the
same one always maps to the same run, so a start replayed from the spool is not
counted twice. `kind` is `backup`, `check`, `prune` or `mirror`. A run for a job
never declared is accepted and creates the job with empty rules.

### `POST /api/v1/runs/{id}/progress`

restic's `status` message (run with `--json`), at most every 10 seconds; the
server may drop faster ones. Never spooled. Reply: `{"ok": true}`.

```json
{
  "at": "2026-11-14T05:40:12Z",
  "percent_done": 0.4213,
  "seconds_elapsed": 8847,
  "seconds_remaining": 12153,
  "total_files": 1843221,
  "files_done": 771092,
  "total_bytes": 452381234567,
  "bytes_done": 190588123456,
  "error_count": 2,
  "current_files": ["/home/sigma/Photos/2019/IMG_2041.CR2"]
}
```

Runs that are not a restic backup (a `keep-deleted` diff, a mirror's hash check)
set `phase` to a short label and fill what they can.

A device that can count its own network traffic adds `bytes_uploaded` (sent
since the run started) and `upload_bytes_per_sec` (over its last few seconds).
The Android app does, from the system's per-app counters; restic's `bytes_done`
counts what it read, unchanged files included, so it is no upload measure.

### `POST /api/v1/runs/{id}/finish`

```json
{
  "status": "success",
  "exit_code": 0,
  "finished_at": "2026-11-14T09:01:33Z",
  "summary": { "files_new": 1204, "data_added_packed": 2811445760, "snapshot_id": "60b9…", "…": "…" },
  "errors": [],
  "error_count": 0,
  "unreadable_paths": []
}
```

* `summary`: restic's backup `summary` message, field for field (backup runs).
* `prune`, `check`, `mirror`: what those runs found (see the `run_finish_*.json`
  fixtures). A check lists the snapshot ids it covered; a mirror lists the
  snapshot ids present in its copy.
* `errors`: the first 50 of restic's `error` messages (`message`, `during`,
  `item`); `error_count` is the total.
* `unreadable_paths`: the `item`s restic could not read (exit 3).
* `message`: a one-line reason when the run failed before restic ran
  (`/mnt/windows/c is not a mountpoint`).

Finish is spooled when the server is unreachable and replayed in order (after
replaying the run's start). A repeated finish with the same content is accepted;
a different one for a finished run is `409 conflict`. Reply: `{"ok": true}`.

### Status

Devices derive `status` from restic's exit code:

| Exit code | Status |
| --- | --- |
| 0 | `success` |
| 3 | `partial`: snapshot made, some files unreadable (listed) |
| 10 | `repository_missing` |
| 11 | `lock_failed` |
| 12 | `wrong_password` |
| 130 | `interrupted` (also: stopped on purpose, e.g. Android's time limit) |
| anything else | `failed` |

The server sets two more itself: `running` until a finish arrives, and `lost`
when none arrives within the job's expected duration plus a margin.

## Safe to delete

### `GET /api/v1/devices/{id}/safe-to-delete?since=<timestamp>`

For the phone's "free up space" screen; a device may only ask about itself.
Replies with the newest snapshot that is both

* **verified on Hetzner**: a `check` run of this device's repository finished
  with `success` after the snapshot was made and listed its id, and
* **present in a mirror**: a successful `mirror` run of this repository reported
  its id in `snapshot_ids`.

```json
{
  "snapshot": {
    "snapshot_id": "a3c1…",
    "snapshot_time": "2026-11-13T23:10:00Z",
    "verified_at": "2026-11-14T00:02:41Z",
    "mirrored_at": "2026-11-14T04:30:00Z",
    "mirrored_by": "PC"
  }
}
```

`{}` when there is none, or none newer than `since`. The app lists the snapshot's
files (it has the password) and offers those unchanged since `snapshot_time`;
deleting stays the user's action.

## Storage layout on the Box

* Each device's repository has its own sub-account, home
  `replique/<owner-slug>/<device-slug>`; the repository is the `restic` folder in
  it. Sub-accounts are chrooted, so the device sees it as `/home/restic`.
* A second, **read-only** sub-account (the twin) has the repository folder itself
  as its home, `replique/<owner-slug>/<device-slug>/restic`, and sees the
  repository as `/home`. Réplique measures sizes and serves website restores
  through it (with `restic --no-lock`); a mirroring device copies through it.
* Why one level down: on a Storage Box, two sub-accounts sharing a home share its
  `.ssh/authorized_keys`, so a key given to the read-only login would also open
  the writable one. One level down, the twin reads its own
  `restic/.ssh/authorized_keys`. It cannot write that file itself, so Réplique
  writes it through the device's sub-account with a password set for that
  moment only. restic ignores the extra `.ssh` folder in the repository.
* SSH, SFTP, restic and rclone use port **23** (port 22 is SFTP-only).
* Devices with `ssh` use restic's SFTP backend
  (`sftp:<user>@<host>:/home/restic`, port through an SSH host alias or
  `-o sftp.args='-p 23'`). Android has no `ssh`, so it uses restic's rclone
  backend (`rclone:<remote>:/home/restic`) with an rclone SFTP remote.
