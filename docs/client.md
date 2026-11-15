# replique-client and the NixOS module

`replique-client` runs restic on a device and reports every run to Réplique: a
backup, then its prune and its integrity check, each a run of its own with live
progress, restic's summary, its errors and the files it could not read. It also
keeps the PC's copy of the phone's repository (a *mirror*). Backups never depend
on the server: when Réplique is unreachable the backup runs anyway and its
reports wait in a local spool until a later run delivers them.

On NixOS, `services.replique.jobs` declares the jobs; the module writes their
configuration, one systemd service and timer per job, the polkit rule that lets
a job block sleep, the SSH host alias of the Storage Box and a `replique-<job>`
command per job for `snapshots`, `mount` and `restore` by hand.

```
replique-client pair https://replique.example.com K7QM-2XPA   # once
systemctl start replique-hetzner                             # or wait for its timer
replique-client status
replique-hetzner snapshots
```

## Install

In the machine's flake:

```nix
{
  inputs.replique = {
    url = "github:Sigma-studios/replique";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, replique, ... }: {
    nixosConfigurations.pc = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        replique.nixosModules.default
        {
          services.replique.client = {
            enable = true;
            server = "https://replique.example.com";
          };
          services.replique.jobs.home = {
            user = "sigma";
            paths = [ "/home/sigma" ];
            exclude = [ "/home/sigma/.cache" ];
            excludeCaches = true;
            checkSubset = "5%";
          };
        }
      ];
    };
  };
}
```

`nixos-rebuild switch`, then pair (below). The package is also
`replique.packages.<system>.replique-client`; it brings restic, rclone and ssh
along. Elsewhere, `cargo build --release -p replique-client` and write
`/etc/replique/client.json` by hand (the module's output, see
[Configuration file](#configuration-file)).

## Pairing

Add the device on the website: it creates the device's Storage Box sub-account
and shows a pairing code. On the device, as root:

```sh
replique-client pair https://replique.example.com K7QM-2XPA
# or with the link inside the QR code:
replique-client pair 'replique://pair?server=https%3A%2F%2Freplique.example.com&code=K7QM-2XPA'
```

It generates an ed25519 SSH key and the repository's restic password, prints the
password **once** and waits until you type `saved`: put it in your password
manager first, because nobody, Réplique included, can restore these backups
without it. Then it calls the server, which installs the key on the
sub-account, stores the device id, the report token, the Storage Box login and
its host keys, and creates the restic repository (`restic cat config` exits 10
when there is none, then `restic init`; running it again is harmless).

**Adopting an existing repository** (a machine already backing up to its
sub-account): use "import existing" on the website, then pair with the key and
password the machine already has. Nothing is generated or printed, and the
server leaves the sub-account's keys alone:

```sh
replique-client pair --adopt /home/sigma/.ssh/storagebox /home/sigma/.config/restic/hetzner.pass \
    https://replique.example.com K7QM-2XPA
```

The files are copied into the state directory; the originals can stay.

### The state directory

`/var/lib/replique-client` (`services.replique.client.stateDir`). The module
makes it `2770 replique-client:replique-client` and puts every job user in the
`replique-client` group; pairing as root gives each file that owner.

| Path | What | Mode |
| --- | --- | --- |
| `device.json` | server URL, device id and name, Storage Box login | 0664 |
| `token` | the report token | 0640 |
| `restic-password` | the repository's password | 0640 |
| `ssh/id_ed25519`, `.pub` | the device's key | 0640 |
| `ssh/known_hosts` | the Storage Box's host keys, from pairing | 0660 |
| `ssh/config` | the host alias (`replique-storage`), for restic's `ssh -F` | 0664 |
| `ssh/include.conf` | the same, root's, included by `/etc/ssh/ssh_config` (OpenSSH refuses an included file that is not root's or is group-writable, and then every connection fails) | 0644 |
| `spool/` | reports waiting for the server; `spool/rejected/` the ones it refused | |
| `runs/<job>.json` | each job's last runs, for `status` | |
| `declared/<job>.json` | the job declaration the server last accepted | |
| `excludes/<job>.txt` | the exclude file of the last run, generated part included | |
| `mirror/` | mirror jobs: verified files, cached mirror sources | |

Why a `replique-client` owner rather than root: OpenSSH refuses a private key
that others can read when the user running ssh owns it. Owned by a third user,
the key works for root and for the job users alike.

## Command line

Global options: `--config <file>` (`$REPLIQUE_CONFIG`, default
`/etc/replique/client.json`) and `--state-dir <dir>` (`$REPLIQUE_STATE_DIR`,
default the configuration's `state_dir`, else `/var/lib/replique-client`).
`$REPLIQUE_RESTIC` and `$REPLIQUE_RCLONE` override the binaries.

| Command | What it does |
| --- | --- |
| `pair <url> <code>` / `pair <link>` | Pairs this device. `--adopt <key-file> <password-file>` keeps an existing key and password; `--name` the name it gives itself (the website's wins); `--yes` skips the confirmation (the password is still printed); `--no-init` leaves the repository alone; `--force` pairs over an existing pairing. |
| `run <job>` | Runs a job: for a backup, the backup, then its prune, then its check; for a mirror, the mirror. Exits with restic's code (rclone's for a mirror): `0` success, `3` some files unreadable, `10` no repository, `11` locked, `12` wrong password, `130` interrupted; `1` when the run was refused (a missing mount). |
| `mirror <job>` | `run` for a mirror job (refuses a backup job). |
| `status [--json]` | The last backup, prune, check and mirror of each job: status, when, how long, whether the report was sent or is spooled, what it found. |
| `restic <job> <args…>` | restic on the job's repository with the job's password and SSH settings: `replique-client restic hetzner snapshots`. For a mirror job, the local copy, with `--no-lock` and no password (restic asks for the source's). The module's `replique-<job>` commands call this. |
| `excludes <job>` | Writes and prints the job's exclude file, as the next run would. |

### What a backup run does

1. Sends what the spool holds, oldest first.
2. Declares the job (`PUT /jobs/<name>`) if its configuration changed since the
   server last accepted it.
3. Refuses to start, and reports the run failed with the reason, if a
   `requireMounts` path is not a mountpoint: an empty mount folder would look
   like every file was deleted.
4. Creates the repository if there is none.
5. Writes the exclude file: the configured patterns, with every `$` turned into
   `?` (restic expands `$NAME` in exclude patterns and files, so
   `$RECYCLE.BIN` would silently become `.BIN`), then the build output found
   under `generatedExcludesFor`, each path escaped (`\`, `*`, `?`, `[`).
6. `restic backup --json`, forwarding restic's status as progress at most every
   10 seconds, then the finish with restic's summary, its first 50 errors and
   the paths it could not read.
7. If the backup succeeded (fully or partly): the prune (per `retention`), then
   `restic check --read-data-subset=<checkSubset>`, each reported as its own
   run; the check lists the snapshots it covered.

On shutdown systemd stops the job: restic and the client both get SIGTERM, the
client waits for restic to stop and reports the run `interrupted` (spooled if
the network is already down). The next run starts from the last snapshot.

**Build output.** `excludeCaches` skips every folder with a `CACHEDIR.TAG`,
which covers cargo's `target/`. Engines leave no tag, so before each run the
client walks `generatedExcludesFor` and excludes, by their siblings:

| Project | Recognised by | Excluded next to it |
| --- | --- | --- |
| Unity | `Assets/` and `ProjectSettings/` | `Library/`, `Temp/`, `obj/`, `Logs/`, `Build/`, `Builds/`, `MemoryCaptures/` |
| Unreal | a `.uproject` file | `Intermediate/`, `DerivedDataCache/`, `Binaries/`, `Saved/` |
| Unreal plugin | a `.uplugin` file | `Intermediate/`, `Binaries/` |
| .NET | a `.csproj`, `.fsproj` or `.vbproj` | `bin/`, `obj/` |
| Godot | `project.godot` | `.godot/`, `.import/` |

Names compare without case (NTFS). The walk skips `.git`, `node_modules`, the
job's literal excludes and, with `excludeCaches`, tagged folders.

### Retention

restic forgets whole snapshots; a file's data goes with the last snapshot that
holds it. `retention.policy`:

| Policy | What the prune does | What can be lost |
| --- | --- | --- |
| `keep-deleted` (default) | Lists what `forget` would drop (`--dry-run`), then, newest first, diffs each snapshot about to go against the next one kept; a snapshot holding a file the kept one no longer has is tagged `replique-keep` (and counts as kept for the older ones). Then `forget --keep-tag replique-keep` with `keep`, and `prune`. | old versions of files that still exist |
| `versions` | `forget` with `keep`, and `prune` | deleted files, once their last snapshot ages out |
| `never-prune` | nothing (no prune run is reported) | nothing |

A tagged snapshot is kept whole, so the old versions it holds stay too. Only
the job's own snapshots are considered (`--path` for each of its paths, `--host`
if set): two jobs can share a repository. Forget, prune and check wait up to two
hours for another job's lock (`--retry-lock`).

### Spool

A report the server cannot take (network error, timeout, 5xx) is kept in
`spool/` and sent at the start and end of later runs, in order. Starts carry a
`client_run_id`, so a start sent twice is still one run. A run whose start was
spooled keeps its entry locked until it ends, so another job's replay leaves it
alone; an unlocked entry without a finish belongs to a client that died, and
only its start is sent (the server marks that run lost). A report the server
refuses (4xx) is moved to `spool/rejected/` and not retried. Progress is never
spooled.

### Mirror jobs

A mirror keeps an encrypted byte copy of another device's repository, the phone's
on the PC. Assign it on the website: the server adds this device's key to the
source's read-only twin login and lists that login in `GET /mirror-sources`
(cached in `mirror/sources.json`, so a mirror runs while the server is down).
Each run:

1. `rclone sync` from the twin to `mirrorPath`, with `--immutable` (restic never
   rewrites a file, so a changed one is corruption, not an update), without
   `locks/` and `.ssh/` (the twin's own keys, kept in the repository folder), and `--backup-dir <mirrorPath>.removed/<time>`: a file gone from the
   source is moved aside and deleted after `keepRemovedDays` (30).
2. Every new file under `data/`, `index/` and `snapshots/` is hashed: restic names
   them by the SHA-256 of their contents, so a mismatch is corruption, found
   without the password. Verified files are remembered and not hashed again.
3. Reports the files copied, removed and verified, the mismatches, the snapshot
   ids present in the copy (what "safe to delete" on the phone relies on) and
   the copy's size. A mismatch fails the run.

The copy is a normal restic repository:
`replique-phone snapshots` (or `restic -r <mirrorPath> --no-lock …`) with the
phone's password.

## Module reference

### `services.replique.client`

| Option | Default | |
| --- | --- | --- |
| `enable` | `false` | |
| `package` | this flake's `replique-client` | |
| `server` | `null` | The server's URL. Pairing stores the URL it was given; this one wins when set. |
| `stateDir` | `"/var/lib/replique-client"` | |
| `sshAlias` | `"replique-storage"` | The Storage Box login's SSH host alias. |

### `services.replique.jobs.<name>`

Names are 1 to 64 of `a-z`, `0-9`, `-`, `_`.

| Option | Default | |
| --- | --- | --- |
| `kind` | `"backup"` | `"backup"` or `"mirror"`. |
| `user` | `"root"` | Runs as this user, added to the `replique-client` group. |
| `paths` | `[ ]` | What a backup backs up. |
| `exclude` | `[ ]` | restic patterns, written as meant (`$RECYCLE.BIN`). |
| `excludeCaches` | `false` | `--exclude-caches`. |
| `generatedExcludesFor` | `[ ]` | Folders scanned for build output before each run. |
| `requireMounts` | `[ ]` | Refuse to run unless these are mountpoints. |
| `schedule` | `"daily"` | Timer `OnCalendar`; `null` for a manual job. |
| `persistent` | `true` | Timer `Persistent=`. |
| `randomizedDelay` | `null` | Timer `RandomizedDelaySec=`, e.g. `"1h"`. |
| `retention.policy` | `"keep-deleted"` | `"keep-deleted"`, `"versions"` or `"never-prune"`. |
| `retention.keep` | `{ daily = 7; weekly = 4; monthly = 12; }` | `last`, `hourly`, `daily`, `weekly`, `monthly`, `yearly` (a count or `"unlimited"`), `within` (`"14d"`). Setting it replaces the default as a whole. |
| `checkSubset` | `null` | `check --read-data-subset` after each backup, e.g. `"5%"`. |
| `nice` | `null` | Service `Nice=`. |
| `ioIdle` | `false` | Service `IOSchedulingClass=idle`. |
| `inhibitSleep` | `false` | Block sleep while running (`systemd-inhibit`), with the polkit rule allowing `user` to. |
| `extraBackupArgs` | `[ ]` | More `restic backup` arguments. |
| `host` | `null` | restic `--host` (default: the machine's hostname). |
| `repository` | `null` | Another restic repository than the paired one (a NAS). |
| `passwordFile` | `null` | Another password file than the paired one. |
| `mirrorSource` | `null` | Mirror: the source's device id, from the website. |
| `mirrorPath` | `null` | Mirror: where the copy lives. |
| `mirrorRemovedPath` | `null` | Mirror: where removed files go (`<mirrorPath>.removed`). |
| `keepRemovedDays` | `30` | Mirror: how long removed files are kept. |
| `mirrorFrom` | `null` | Mirror: an rclone path to copy from instead of the listed login. |

### What it generates

* `/etc/replique/client.json`, the jobs for `replique-client`.
* Per job, `replique-<name>.service`: oneshot, `User=<user>`,
  `SupplementaryGroups=replique-client`, running `replique-client run <name>`
  (inside `systemd-inhibit --what=sleep --mode=block` with `inhibitSleep`),
  restic's cache in `/var/cache/replique-<name>`, `Nice=` and idle I/O as
  set; not restarted by `nixos-rebuild switch`, so a running backup survives a
  rebuild. Plus `replique-<name>.timer` when `schedule` is set.
* `replique-<name>` commands: restic on the job's repository.
* The `replique-client` system user and group, the state directory
  (`2770`), and `Include <stateDir>/ssh/include.conf` in `/etc/ssh/ssh_config` (the
  alias works once paired: `sftp replique-storage`).
* With `inhibitSleep` on any job, `security.polkit.enable` and a rule allowing
  `org.freedesktop.login1.inhibit-block-sleep` for those jobs' users: logind
  asks polkit when a service without a session blocks sleep, and polkit wants
  interactive authentication by default.

### Configuration file

What the module writes, and what `replique-client` reads elsewhere:

```json
{
  "server": "https://replique.example.com",
  "state_dir": "/var/lib/replique-client",
  "ssh_alias": "replique-storage",
  "jobs": {
    "home": {
      "kind": "backup",
      "paths": ["/home/sigma"],
      "exclude": ["/home/sigma/.cache"],
      "exclude_caches": true,
      "generated_excludes_for": ["/home/sigma"],
      "require_mounts": [],
      "schedule": "daily",
      "retention": { "policy": "keep-deleted", "keep": { "daily": 7, "weekly": 4, "monthly": 12 } },
      "check_subset": "5%"
    },
    "phone": { "kind": "mirror", "mirror_source": "dev_…", "mirror_path": "/home/sigma/Backups/phone" }
  }
}
```

## From `services.restic.backups`

The PC's two jobs share one repository (`sftp:storagebox:/home/restic`, the
sub-account `u123456-sub1`), run as `sigma` at `Nice=10` with idle I/O, block
sleep, prune with `--keep-daily 7 --keep-weekly 4 --keep-monthly unlimited` and
read back 5% after each run. Option by option:

| `services.restic.backups.<name>` / around it | `services.replique.jobs.<name>` |
| --- | --- |
| `repository = "sftp:storagebox:/home/restic"` | nothing: the paired repository (pairing with `--adopt` keeps the same one) |
| `passwordFile = "/home/sigma/.config/restic/hetzner.pass"` | nothing: copied into the state directory by `pair --adopt` |
| the `storagebox` SSH alias (port 23, `IdentityFile ~/.ssh/storagebox`) | generated at pairing: `replique-storage`, with the host keys the server scanned |
| `initialize = true` | always: `cat config` exits 10, then `init` |
| `user = "sigma"` | `user = "sigma"` |
| `paths` | `paths` |
| `exclude` | `exclude` (a `$` may now be written as is) |
| `--exclude-caches` | `excludeCaches = true` |
| the `excludes.py` exclude file | `generatedExcludesFor` |
| `timerConfig.OnCalendar = "daily"` | `schedule = "daily"` (`null`: manual, the `windows` job) |
| `timerConfig.Persistent = true` | `persistent = true` |
| `timerConfig.RandomizedDelaySec = "1h"` | `randomizedDelay = "1h"` |
| `pruneOpts = [ "--keep-daily 7" "--keep-weekly 4" "--keep-monthly unlimited" ]` | `retention = { policy = "versions"; keep = { daily = 7; weekly = 4; monthly = "unlimited"; }; }`, or `policy = "keep-deleted"` to keep deleted files for good |
| `checkOpts = [ "--read-data-subset=5%" ]` | `checkSubset = "5%"` |
| `Nice = 10`, `IOSchedulingClass = "idle"` | `nice = 10; ioIdle = true;` |
| `systemd-inhibit` wrapper + the polkit rule | `inhibitSleep = true` |
| the Windows mounts it assumed | `requireMounts` |
| `restic-hetzner` wrapper | `replique-hetzner` |

The whole configuration, also evaluated by the flake's `example` check
([nix/example.nix](../nix/example.nix); adjust the paths marked so):

```nix
{
  services.replique.client = {
    enable = true;
    server = "https://replique.sigma-dev.eu";
  };

  services.replique.jobs = {
    hetzner = {
      user = "sigma";
      paths = [ "/home/sigma" ];
      exclude = [
        "/home/sigma/.local/share/Steam"
        "/home/sigma/.cache"
        "/home/sigma/.rustup"
        "/home/sigma/.cargo/registry"
        "/home/sigma/Games/Heroic"
        "/home/sigma/Backups"
        "/home/sigma/Datasets/large-dataset" # the 116 GB downloadable dataset
        "/home/sigma/.config/*/Default/Service Worker/CacheStorage"
        "/home/sigma/.config/*/Default/Code Cache"
        "/home/sigma/.config/*/Default/GPUCache"
      ];
      excludeCaches = true;
      generatedExcludesFor = [ "/home/sigma" ];
      schedule = "daily";
      persistent = true;
      randomizedDelay = "1h";
      retention = {
        policy = "keep-deleted";
        keep = { daily = 7; weekly = 4; monthly = "unlimited"; };
      };
      checkSubset = "5%";
      nice = 10;
      ioIdle = true;
      inhibitSleep = true;
    };

    windows = {
      user = "sigma";
      paths = [ "/run/media/sigma/Windows/Users" "/run/media/sigma/Data" ];
      requireMounts = [ "/run/media/sigma/Windows" "/run/media/sigma/Data" ];
      exclude = [
        "$RECYCLE.BIN"
        "System Volume Information"
        "/run/media/sigma/Windows/Users/*/AppData/Local/Temp"
        "/run/media/sigma/Windows/Users/*/AppData/Local/*/Cache"
        "/run/media/sigma/Windows/Users/*/AppData/Local/Packages"
        "/run/media/sigma/Data/Games"
        "/run/media/sigma/Data/Installers"
        "/run/media/sigma/Data/Program Files"
      ];
      generatedExcludesFor = [ "/run/media/sigma/Data" ];
      schedule = null;
      retention = {
        policy = "keep-deleted";
        keep = { daily = 7; weekly = 4; monthly = "unlimited"; };
      };
      checkSubset = "5%";
      nice = 10;
      ioIdle = true;
      inhibitSleep = true;
    };

    phone = {
      kind = "mirror";
      user = "sigma";
      mirrorSource = "dev_…"; # the phone's device id, from the website
      mirrorPath = "/home/sigma/Backups/phone";
      randomizedDelay = "1h";
      nice = 10;
      ioIdle = true;
    };
  };
}
```

Switching over: keep the old jobs' timers off (`services.restic.backups`
removed), pair once with `--adopt` as root, then
`systemctl start replique-hetzner`. Snapshots keep the same host and paths, so
the first run continues from the last restic snapshot instead of reading
everything again; restic's cache moves to `/var/cache/replique-hetzner` and is
rebuilt on that first run.

## Development

`cargo test -p replique-client` runs the client against a mock server (the
endpoints of `docs/protocol.md`, in `tests/common`) and local restic
repositories: pairing, adopting, backup + prune + check, a partial run, a
missing mount, the spool across an outage, the exclude generator on a fixture
tree, `keep-deleted` on a fixture history, mirrors. It needs restic and rclone
on `PATH`.

`cargo test -p replique-client --test sftp -- --ignored` adds the real SFTP
path: an OpenSSH server in Docker (container `replique-b-sftp`, port 2223),
chrooted like a sub-account; pairing with `--adopt`, a backup through the
generated SSH config, and a mirror through the listed login.

`nix build .#checks.x86_64-linux.vm -L` (KVM) runs the module on three VMs: an
SFTP server on port 23, a stand-in server (`nix/tests/mock-server.py`) and a
client with jobs shaped like the PC's.
