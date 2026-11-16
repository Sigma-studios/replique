# Réplique's server

`replique-server` is the part of Réplique you open in a browser. It creates
each device's storage on the Hetzner Storage Box, pairs the device, receives its
reports, measures what is really stored, draws it, raises alerts on Discord, and
lets you browse and download from any snapshot once you type that repository's
password. It never holds backup data: devices encrypt and upload on their own,
and keep backing up while the server is down.

* **One binary**, static on Linux, with SQLite inside and every script and
  stylesheet compiled in. It needs restic (for website restores) and nothing
  else: SSH and SFTP to the box are built in, and restic reaches the box through
  `replique-server sftp-bridge`.
* **The website** is server-rendered and works at phone width: Aperçu (box
  gauge, a card per device and job with live progress and ETA), a page per
  device (runs, size chart, declared rules, pairing, restore, mirrors, token,
  removal), Storage (box total and per-repository charts that grow live),
  Problems, Add device, the phone's rules, Settings.
* **The device API** (`/api/v1`) is the one in [protocol.md](protocol.md).

## Install

### With Docker (façade)

The repository's `Dockerfile` builds the server and a small Alpine image with
restic:

```sh
docker build -t replique-server .
docker run -d --name replique -p 8080:8080 -v replique:/data \
  -e REPLIQUE_PUBLIC_URL=https://replique.example.com \
  -e REPLIQUE_ADMIN_PASSWORD_HASH='$argon2id$v=19$…' \
  replique-server
```

On façade, deploy the repository as an app built from that Dockerfile, with a
volume on `/data`, port 8080, a domain, and `REPLIQUE_PUBLIC_URL` and
`REPLIQUE_ADMIN_PASSWORD_HASH` as environment variables (the hash is not a
secret worth hiding, but façade keeps it encrypted anyway). The Hetzner token
and the Discord webhook are then typed on the Settings page and stored
encrypted in `/data`.

Everything the server keeps is in `/data`; back up that volume (façade does).
Without `master.key` the stored token and webhook cannot be read, and must be
typed again.

### On NixOS (a NAS)

The flake's `nixosModules.default` provides `services.replique.server`:

```nix
{
  imports = [ replique.nixosModules.default ];
  services.replique.server = {
    enable = true;
    adminPasswordHashFile = "/run/secrets/replique-admin-hash";
    hetznerTokenFile = "/run/secrets/hetzner-token";   # or null: Settings page
    settings = {
      listen = "127.0.0.1:8080";
      public_url = "https://replique.example.com";
      owner_name = "Sigma";
      storage_box.id = 718677;
    };
  };
}
```

It runs one systemd service, `replique-server`, with a dynamic user, its state
in `/var/lib/replique`, the two secret files handed over as systemd credentials
(they never enter the Nix store), and a hardened sandbox. `settings` is written
to `replique.toml` as is; the module sets `state_dir`, `restore.restic` and the
secret paths. Put a reverse proxy with HTTPS in front (`openFirewall` opens the
port directly if you must).

### As a plain binary

```sh
cargo build --release -p replique-server
replique-server --config /etc/replique/replique.toml
```

restic must be on the `PATH` (or set `[restore] restic`).

### First start

1. Make the admin password's hash and point `admin_password_hash_file` (or
   `REPLIQUE_ADMIN_PASSWORD_HASH`) at it:
   ```sh
   echo 'a long password' | replique-server hash-password > /run/secrets/replique-admin-hash
   ```
2. Log in, open **Settings**, enter the Storage Box id (the number in the Hetzner
   Console's address for the box) and an API token with read and write access.
   The page then shows the box, its quota and how many sub-accounts are left
   (each device takes two: its own and a read-only twin).
3. Paste a Discord webhook (Server settings → Integrations → Webhooks) and send
   the test message.
4. **Add device**: a name and a kind. Réplique creates the sub-accounts and shows
   a pairing code, as a QR code for the phone and as a command for computers
   (`replique-client pair <url> <code>`), valid 15 minutes.

`replique-server check-config` validates a configuration file and exits.

## How it reaches the Storage Box

Each device has its own sub-account, home `replique/<owner>/<device>`, SSH only,
reachable from outside Hetzner. Pairing logs in once with the password Réplique
gave it at creation, installs the device's public key, then replaces the
password with a random one nobody keeps. A second, read-only sub-account (the
**twin**) has the repository folder as its home (`replique/<owner>/<device>/restic`);
Réplique writes its own key (shown on the Settings page) into the twin's
`authorized_keys`, inside that folder, through the device's sub-account during
pairing, and uses it to measure the repository and serve website restores,
always with `restic --no-lock`.

The twin is one level down on purpose. Checked on a real Storage Box: two
sub-accounts sharing a home share its `.ssh/authorized_keys`, and
`install-ssh-key` fails on a read-only one. A twin sharing the device's home
would accept the device's key, and any key given to it (Réplique's, a mirroring
PC's) would also open the device's writable login. With the repository folder as
its home, the twin has its own key file, which only the writable login can
write. The device could edit that file too; at worst it removes Réplique's key
(measurements fail, which raises an alert) or lets someone read its own
encrypted repository.

Two switches in `[storage_box]` cover setups where this does not fit:

| Switch | Values | Meaning |
| --- | --- | --- |
| `measure_via` | `twin` (default) | Measure and restore through each repository's read-only twin. |
| | `main` | No twins: measure and restore through the main account. Add Réplique's key (Settings) to the main account's authorized keys by hand. A device mirroring another then gets its key on the source's own sub-account. |
| `key_install` | `install-ssh-key` (default) | Pairing runs Hetzner's `install-ssh-key` over SSH on port 23 with the sub-account's password. |
| | `sftp` | Pairing writes `.ssh/authorized_keys` over SFTP with the password, keeping any key already there. |
| | `manual` | Pairing finishes without touching the box; the device page shows a one-line command per sub-account, with its password, to run once, and a button that then rotates the passwords. |

Both `install-ssh-key` and `sftp` were checked on a real Storage Box with a
sub-account's password. The twin's key is always written over SFTP, whatever
`key_install` says: `install-ssh-key` only writes the account's own `.ssh`.

Host keys are pinned: `[storage_box] known_hosts` if given, otherwise the key
seen on the first connection, which pairing also hands to devices.

## Configuration reference

`replique.toml`; every key is optional. Secrets are never in it, only the
paths of files holding them.

```toml
listen = "127.0.0.1:8080"          # website and device API
public_url = "http://localhost:8080"  # https: makes the session cookie Secure
state_dir = "/var/lib/replique"    # database, master.key, ssh/, restic-cache/
# database = "/var/lib/replique/replique.db"
admin_password_hash_file = "/run/secrets/replique-admin-hash"
owner_name = "admin"               # its slug starts every repository's home

[storage_box]
# id = 718677                      # or on the Settings page
api_url = "https://api.hetzner.com/v1"
# hetzner_token_file = "/run/secrets/hetzner-token"   # or on the Settings page
# host = "u123456.your-storagebox.de"                 # default: from the API
port = 23
measure_via = "twin"               # or "main"
# main_username = "u123456"        # for measure_via = "main"; default: from the API
main_root = "/home"                # where sub-account homes appear for the main account
key_install = "install-ssh-key"    # or "sftp", "manual"
known_hosts = []                   # "[host]:23 ssh-ed25519 AAAA…" lines to require
# local_root = "/tmp/box"          # development only, see below

[measurement]
enabled = true
every_secs = 300

[restore]
restic = "restic"
idle_timeout_secs = 900            # an unlocked repository forgets its password

[alerts]                           # starting values; Settings changes them
box_warn_percent = 80
box_alert_percent = 95
lost_after_secs = 10800            # a run with no news this long is lost
```

These environment variables override the file (a missing file is then fine):

| Variable | Key |
| --- | --- |
| `REPLIQUE_CONFIG` | the file itself (default `/etc/replique/replique.toml`) |
| `REPLIQUE_LISTEN` | `listen` |
| `REPLIQUE_PUBLIC_URL` | `public_url` |
| `REPLIQUE_STATE_DIR` | `state_dir` |
| `REPLIQUE_OWNER_NAME` | `owner_name` |
| `REPLIQUE_ADMIN_PASSWORD_HASH_FILE` | `admin_password_hash_file` |
| `REPLIQUE_ADMIN_PASSWORD_HASH` | the hash itself, when no file is set |
| `REPLIQUE_STORAGE_BOX_ID` | `storage_box.id` |
| `REPLIQUE_STORAGE_BOX_HOST` | `storage_box.host` |
| `REPLIQUE_HETZNER_TOKEN_FILE` | `storage_box.hetzner_token_file` |
| `REPLIQUE_MEASURE_VIA` | `storage_box.measure_via` |
| `REPLIQUE_KEY_INSTALL` | `storage_box.key_install` |
| `REPLIQUE_LOG` | log filter, e.g. `info` or `debug,russh=warn` (logs go to stderr) |

## Recovery

The Recovery page holds what you need when something is lost, and no secret:

* **The paper kit's checklist**: the names of what is handwritten in the kit
  envelopes (Bitwarden's recovery code, the Aegis password, recovery codes of the
  critical accounts…), each with where to find it and when it was written down.
* **The recovery plan**: markdown, pre-filled with a first draft, saying where
  every secret lives and what to do in each disaster.
* **Print the kit**: one print of the plan and a form with room to write each
  entry by hand, every page showing the plan's version. Print it from the
  browser (two copies, one per envelope), never as a PDF.
* **Copies of the Bitwarden vault** sent by devices with
  `services.replique.bitwardenCopy` (the last 10, downloadable).

Until every entry is written down and the current version printed, it is a
Problem with a Discord reminder each week; then again once a year (check both
envelopes and their seals) and whenever the plan or the checklist changes.

Réplique writes the vault copies, the plan and the checklist to the Storage Box
too, in its own folder `replique/_replique` (a writable sub-account it creates
the first time; every other login it holds is read-only), so they can be read
with Hetzner access alone.

## What the website shows

* **Overview:** the Storage Box, split by device; then a row per sub-account
  with one status: *Up to date* (with the last backup), *No backup since …* (a
  scheduled job past its interval plus the silent-job margin), *Error* (a run of
  one of its jobs failed and neither a backup nor a successful run of the same
  kind came since), or *Backing up* with its progress. A row opens its device.
  Under it, the stacked storage chart with its time range.
* **Transfer speeds:** each run line shows its average (bytes sent over the
  whole run, scanning included); each job's card shows the average and the peak
  over 30 days. Peaks are noted as progress comes in: from the phone's and the
  PC's own network counters, and from a mirror's copy step. Runs from before
  this, or from a client that does not count, have an average only.
* **Times:** every relative time ("5 min ago") shows the exact time when hovered.

## Operating notes

**What is measured.** Every `every_secs`: the whole box from Hetzner's API
(`stats.size`, with data and snapshots apart), and every repository's bytes and
file count over SFTP. Points are kept every 5 minutes for 7 days, hourly for 90
days and daily forever. A repository whose size or file count changes while
none of its device's runs was going on or finished since the previous look
raises a drift warning ("changed outside a reported run"); a successful check
run clears it, or dismiss it on the Problems page.

**Alerts.** Each is a Problem and a Discord message mentioning `@everyone`
(sent with `allowed_mentions: {"parse": ["everyone"]}`); its recovery is
another message, without the mention. An alert still open is not sent again.

| Alert | Raised when | Cleared when |
| --- | --- | --- |
| run failed | a backup, prune or mirror ends failed, locked, without repository or with the wrong password | that job's next successful run of the same kind |
| run partial (warning) | a backup could not read some files; lists them | the next successful backup |
| integrity check failed | a check run fails or finds errors, or a mirror's copy is broken (files still not matching their hash after a fresh download fail to download again) | the next successful check or mirror |
| broken on the source (warning) | a mirror finds files wrong on the source itself (a fresh download is still wrong), usually leftovers of an interrupted upload; lists them. The mirror run itself succeeds | a mirror reports none |
| source not checked since (alert) | such files have been reported, and the source's repository has had no passing check since they first appeared, for longer than the source's interval plus the silent-job margin (daily → 26 h) | a passing check of the source, or a mirror reporting none |
| job silent | no run for the job's expected interval plus a margin (daily → 26 h); the interval is declared by the device, derived from its schedule, or set per job on the device page | a run starts |
| run lost (warning) | a running run has no news for `lost_after_secs` (or 1.5 × restic's own estimate) | the job's next finished run |
| box full | 80% (warning) and 95% (alert), set on Settings | usage drops below |
| drift (warning) | see above | a successful check, or dismissed |

**Files broken on a mirror's source.** A mirroring device has no password, so it
cannot tell whether restic uses a file; the source can. restic 0.19 compares the
size of every pack its index lists with the file on the box in a plain `check`
(no `--read-data` needed): a truncated pack in use fails it, an unused one does
not. So a passing check of the source that finished after the broken files were
first seen means none of them is in use: the warning stays (the files are still
there, until a prune on the source removes them) but nothing escalates, and the
device page says they are harmless. Until then, a mirror run with such files does
not count towards "safe to delete" on the phone. The limit: a pack in use damaged
without a size change passes the plain check; the source's
`--read-data-subset` checks and the mirror's hashing find it over time.

The Problems page also lists what is not set up (no token, no webhook) and
Discord deliveries that failed.

**Restores on the website** keep the typed password in memory only, under the
browser session that typed it, and forget it after 15 minutes without use, at
logout, or when the session expires. restic gets it on its standard input,
never in its arguments or environment, and the server never logs it (a test
asserts it on captured logs). Downloads are streamed: `restic dump` for a file,
`restic dump --archive zip` for a folder. The restore runbook
([restore.md](restore.md), also at `/runbook`) works without Réplique at all.

**Sessions** last 7 days from their last use. Every form carries a CSRF token.

**Removing a device** deletes its two sub-accounts through the API and revokes
its token; "delete its backups too" first sets a throwaway password on its
sub-account and deletes the `restic` folder there. Keeping the backups leaves
them in the home folder, readable with the main account.

**Upgrades** keep the database: migrations run at start and only ever add.

## Development

A local setup without any Hetzner account: set `[storage_box] local_root` to a
folder and leave the token unset. Sub-accounts are then simulated, keys are
written under that folder, and repositories are folders inside it:

```toml
listen = "127.0.0.1:8080"
public_url = "http://127.0.0.1:8080"
state_dir = "/tmp/replique-dev/state"
admin_password_hash_file = "/tmp/replique-dev/admin.hash"
[storage_box]
local_root = "/tmp/replique-dev/box"
```

A device paired there gets the username `local-<device>`; point its restic at
`<local_root>/replique/<owner>/<device>/restic` by hand.

Tests: `cargo test -p replique-server` runs the Hetzner client against a mock
built from the OpenAPI spec's examples (`tests/hetzner/`), the report flow end
to end with a protocol client and a real local restic repository, and a website
restore of a fixture snapshot. The SSH side runs against a real SSH server when
`REPLIQUE_TEST_SFTP` names one: a user `dev` with password `DevPass-1234`, a
shell, and an `install-ssh-key` command appending stdin to its
`authorized_keys`. `crates/replique-server/tests/sftp-server.sh` turns an
Alpine container into one:

```sh
docker run -d --name replique-sftp -p 2222:22 \
  -v "$PWD/crates/replique-server/tests/sftp-server.sh:/entry.sh:ro" alpine:3 sh /entry.sh
REPLIQUE_TEST_SFTP=127.0.0.1:2222 cargo test -p replique-server --test sftp
```
