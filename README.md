# Réplique

Encrypted backups of every device you own, set up, watched and restored from one
place. Each device (a NixOS PC, an Android phone, a façade server, later a NAS)
encrypts its own backups with restic and uploads them straight to its own Hetzner
Storage Box sub-account. Réplique never holds backup data or standing keys: it
creates the storage, receives each device's reports, measures what is really
stored, draws it, alerts on Discord when something goes wrong, and restores on
demand once you type that backup's password.

```
                 reports                     restic, encrypted
 ┌───────────┐ ◀──────── ┌──────────────┐ ────────▶ ┌──────────────────────────┐
 │ Réplique  │           │ PC (NixOS)   │           │ Hetzner Storage Box      │
 │ server    │ ◀──────── │ Phone (app)  │ ────────▶ │  one sub-account each    │
 │           │ ◀──────── │ façade VPS   │ ────────▶ │                          │
 └─────┬─────┘           └──────────────┘           └────────────▲─────────────┘
       │  Hetzner API + read-only SFTP: provision, measure, restore   │
       └──────────────────────────────────────────────────────────────┘
```

* **One repository per device**, each on its own sub-account with its own restic
  password, which only the device and your password manager know. Backups keep
  running when Réplique is down; reports wait in a spool and arrive later.
* **Devices are added on the website only.** Réplique creates the sub-account
  (and a read-only twin) through Hetzner's API, then pairs the device with a QR
  code or a one-line command. Each device reports with its own revocable token.
* **Deleting on a device never deletes in its backups.** The default retention,
  `keep-deleted`, only lets old versions of files that still exist age out; the
  phone never prunes at all.
* **The phone backs up directly**, with restic and rclone bundled in the app, and
  the PC keeps an encrypted byte copy of the phone's repository that it cannot
  read. A file is "safe to delete" from the phone once it is in a verified
  snapshot on Hetzner and that snapshot is in the PC's copy.
* **Restore always works without Réplique**: restic, the sub-account login and
  the password are enough ([docs/restore.md](docs/restore.md)). On the website, a
  repository can be unlocked for 15 minutes to browse and download files.
* **Alerts** go to Discord (mentioning `@everyone`) and the Problems page: a
  failed, partial or lost run, a job silent past its schedule, the box above 80%
  or 95%, an integrity check failing, a repository changing outside any run.

## Install

| Part | Where it runs | How |
| --- | --- | --- |
| Server | the VPS (through façade, from the `Dockerfile`), or a NAS | [docs/server.md](docs/server.md) |
| Client and NixOS module | the PC, any NixOS machine | [docs/client.md](docs/client.md) |
| Android app | the phone (sideloaded APK) | [android/README.md](android/README.md) |
| façade's offsite copies | the façade server | façade's README, "Offsite copies" |

Then, on the website: add each device (or adopt an existing sub-account), pair it,
and save the restic password it shows once in your password manager.

## Reference

* [docs/protocol.md](docs/protocol.md): how devices talk to the server (pairing,
  jobs, runs, statuses, retention policies, storage layout).
* [docs/server.md](docs/server.md): configuration, how it reaches the Storage Box,
  operating notes.
* [docs/client.md](docs/client.md): the CLI, what a run does, retention, spool,
  mirrors, the module's options, and the mapping from `services.restic.backups`.
* [android/README.md](android/README.md): building the APK, what is and isn't
  backed up, how restic runs on Android.
* [docs/restore.md](docs/restore.md): the restore runbook.

## Repository

| Path | What |
| --- | --- |
| `crates/replique-proto` | The protocol's types, shared by every Rust part |
| `crates/replique-server` | HTTP API, website, Hetzner client, measurements, alerts |
| `crates/replique-client` | Runs restic on a device and reports each run |
| `nix/` | Packages, the NixOS module (`nixosModules.default`) and its VM test |
| `android/` | The Android app |
| `proto/fixtures/` | One example of each protocol message, read by Rust and Kotlin tests |
| `scripts/e2e-local.sh` | The real client against the real server, no Storage Box needed |

## Development

`nix develop` gives Rust, restic, rclone, SQLite, Go, a JDK, Gradle and (on Linux)
the Android SDK and NDK. Then:

```sh
cargo test                        # proto, server and client
scripts/e2e-local.sh              # pairing, runs, an outage and its replay
nix build .#checks.x86_64-linux.vm -L      # the NixOS module on three VMs
(cd android && ./gradlew testDebugUnitTest)
```
