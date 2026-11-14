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

Status: in development. The contract between the server and devices is in
[docs/protocol.md](docs/protocol.md).

## Repository

| Path | What |
| --- | --- |
| `crates/replique-proto` | The protocol's types, shared by every Rust part |
| `crates/replique-server` | HTTP API, website, Hetzner client, measurements, alerts |
| `crates/replique-client` | Runs restic on a device and reports each run |
| `nix/` | Packages and the NixOS module (`nixosModules.default`) |
| `android/` | The Android app |
| `proto/fixtures/` | One example of each protocol message, read by Rust and Kotlin tests |

## Development

`nix develop` gives Rust, restic, rclone, SQLite, Go, a JDK and (on Linux) the
Android SDK. Then `cargo test`.
