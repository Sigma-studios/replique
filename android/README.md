# Réplique for Android

The phone's side of Réplique. It backs up the phone's shared storage (photos,
downloads, documents: everything you see as "the phone's files") with a bundled
restic, encrypted on the phone, straight to the phone's own Storage Box
sub-account, in the background, and reports every run to Réplique. It also tells
you which files are safe to delete from the phone, and lets you delete them.

```
 Phone                                            Hetzner Storage Box
 ┌───────────────────────────────┐   SFTP :23     ┌─────────────────────────┐
 │ WorkManager → foreground job  │ ─────────────▶ │ phone's sub-account     │
 │   librestic.so ─stdio─▶       │   encrypted    │   /home/restic          │
 │   librclone.so (serve restic) │                └─────────────────────────┘
 └──────────────┬────────────────┘
                │ reports (start, progress, finish), rules, safe-to-delete
                ▼
           Réplique server
```

* **The phone encrypts and uploads by itself.** Its restic password and SSH key
  are made on the phone at pairing and never leave it (the password also goes
  into your password manager). Réplique only ever sees reports.
* **restic reaches the Storage Box through rclone.** Android has no `ssh`, so
  restic's own SFTP backend cannot work; restic runs `librclone.so serve restic
  --stdio` instead, and rclone holds the SFTP connection. Both are Go programs
  built from pinned sources and shipped as "native libraries", the only files an
  app may execute since Android 10.
* **Backups never depend on the server.** Reports are spooled while Réplique is
  unreachable and sent, in order, later. The rules last received keep applying.
* **Nothing is ever pruned.** The phone's repository is `never-prune`: the app
  never runs `forget`, so whatever reached a snapshot stays there. That is what
  makes "free up space" safe.
* **Deleting is always your action.** The app lists files that are in a snapshot
  verified on Hetzner and copied to the PC, and unchanged since; you pick and
  confirm.

## Build the APK

Needs a JDK 17, Go (1.26 or newer, for rclone 1.76), the Android SDK with
platform 37 and the NDK `29.0.14206865`, and network access for the first build
(Gradle, the restic and rclone sources, their Go modules).

```sh
cd android
echo "sdk.dir=$HOME/Library/Android/sdk" > local.properties   # or set ANDROID_HOME
./gradlew assembleDebug
# → app/build/outputs/apk/debug/app-debug.apk
```

The build cross-compiles restic and rclone itself (`buildNativeBinaries`, run by
`native/build-binaries.sh`): it downloads the release source tarballs, checks
them against the SHA-256 pinned in the script, and builds them with
`GOOS=android GOARCH=arm64` and cgo through the NDK's clang. Only arm64 is built:
every phone this is for is arm64, and so is the emulator on an Apple Silicon Mac.

For a release build, `./gradlew assembleRelease` gives an unsigned, minified APK
(about 22 MB); sign it with your own key (`apksigner sign --ks …`). The debug APK
works just as well for a personal phone; it is larger and also allows plain HTTP
to `10.0.2.2` (the emulator's host) for development.

## Install and pair

1. On Réplique's website, add a device of kind Android. It creates the phone's
   sub-account and shows a pairing QR code (valid 15 minutes).
2. Install the APK (`adb install app-debug.apk`, or open it on the phone and
   allow installing from that source).
3. Scan the QR code with the phone's camera app (it opens Réplique), or open
   Réplique and tap **Scanner le code QR**, or paste the `replique://pair?…` link.
4. Réplique shows the **backup password**, once. Save it in your password manager
   now: without it nobody can restore the phone's backups, not Réplique, not the
   PC's copy. Tick the box, tap **Appairer ce téléphone**.
5. Grant **All files access** when Home asks for it (Settings → Apps → Réplique →
   All files access). The first backup starts once the phone is charging on Wi-Fi
   (or tap **Sauvegarder maintenant**).
6. Tap **Exempter Réplique** to exempt it from battery optimisation (see below).

Unpairing (Réglages → Désappairer) makes the phone forget its key, token and
password. The backups stay on the Storage Box; remove the device on the website
if you no longer want them.

## Battery optimisation

Exempt Réplique from battery optimisation. Without it, Android 12 and later may
refuse to start the backup's foreground service from the background, and some
manufacturers (Samsung, Xiaomi, Huawei, OnePlus…) delay or kill background work
far more aggressively than stock Android. If backups stop happening on schedule:

* Settings → Apps → Réplique → Battery → **Unrestricted** (wording varies).
* On Samsung, also take Réplique out of "Sleeping apps" / "Deep sleeping apps".
* https://dontkillmyapp.com lists what each manufacturer needs.

Android 15 and later also cap "data sync" foreground services at about 6 hours
per 24 hours. A long first backup (hundreds of GB at home upload speeds) stops a
little before that limit, is reported as `interrupted`, and continues on its own
when the allowance is back. restic writes an index of what it has uploaded at
least every ten minutes, so the next run skips everything already sent. Opening
the app resets the allowance (Android does the same).

## When it runs

Daily by default, as soon as the phone is charging on Wi-Fi. In the phone rules
on the website, **Every night at** picks a time instead (`02:00`, the phone's
own time): the backup starts then, or as soon as the conditions are met after
it. Home shows the next run, and during a run the upload rate (`Envoi`), from
Android's count of what the app sent; the website shows it too.

## What is backed up, and what is not

By default (until the rules are edited on the website), everything in shared
storage, `/storage/emulated/0`, except:

| Not backed up | Why |
| --- | --- |
| Apps and their private data | Android does not let another app read them. Reinstall apps from their source; apps with their own cloud backup (messaging apps…) need it turned on |
| `Android/data`, `Android/obb` | Other apps' files, unreadable since Android 11 and regenerated |
| `.thumbnails`, `cache`, `Cache`, `.cache` folders | Regenerated |
| `.trashed-*`, `.pending-*` files | The media store's trash and half-written files |

The rules come from Réplique (`GET /device/config`): which folders, extra
excludes, per-folder size limits, extensions to skip or the only extensions to
keep, how often, and whether scheduled runs wait for Wi-Fi and charging. "Back
up now" ignores the conditions. The Home screen lists what is and is not covered.
A memory card (`/storage/XXXX-XXXX`) is not backed up unless the rules add it.

After every backup the app runs `restic check` (structure only, no data read
back) and reports it as a `check` run listing the snapshot ids it covered; that
is what makes them "verified on Hetzner" for the safe-to-delete answer.

## Restore

Restoring needs no phone and no Réplique: restic, the sub-account's login and the
backup password, from any computer (see `docs/restore.md`). The phone's
snapshots carry its name as host (`restic snapshots --host <name>`) and the tag
`replique`.

## Reference

### Layout

| Path | What |
| --- | --- |
| `native/build-binaries.sh` | Fetches and cross-compiles restic and rclone (versions and hashes pinned here) |
| `native/rclone/main.go` | A trimmed rclone: SFTP backend, `serve restic`, `version`, `lsf` (20 MB instead of 70+) |
| `app/src/main/java/…/proto` | The Kotlin mirror of `crates/replique-proto` |
| `…/restic` | Running restic (`Restic`), its JSON output, the backup plan from the rules |
| `…/backup` | `BackupRunner` (one backup + check, with reports), the WorkManager worker, scheduling, Android 15's time budget |
| `…/api` | HTTP client and the report spool |
| `…/data` | State, Keystore-encrypted secrets, key generation, pairing, safe-to-delete |
| `…/ui` | The four screens (Compose) |
| `scripts/` | Test SFTP server, device tests, a fake Réplique server |

### Pinned versions

| What | Version |
| --- | --- |
| restic | 0.19.1 (release source tarball, SHA-256 in the script) |
| rclone | 1.76.0 (release source tarball, SHA-256 in the script) |
| NDK | 29.0.14206865, linked against API 26 (the app's minSdk) |
| Gradle / AGP / Kotlin | 9.9.1 / 9.5.1 / 2.4.21 |
| SDK | minSdk 26, targetSdk 36, compileSdk 37 (the current AndroidX releases require it) |

### Why cgo

A Go program built without cgo resolves host names with Go's own DNS client,
which reads `/etc/resolv.conf`. Android has no such file, so the lookup goes to
`[::1]:53` and fails (`lookup u123456.your-storagebox.de on [::1]:53: … connection
refused`). With cgo, Go on Android always asks bionic's resolver. The binaries
are therefore linked with the NDK. `REPLIQUE_NATIVE_CGO=0
native/build-binaries.sh <dir>` builds the cgo-less variant to see the failure.

### How restic runs

* Repository `rclone:replique:/home/restic`, with `-o rclone.program=<native
  library dir>/librclone.so` and `-o rclone.args=serve restic --stdio` (restic's
  default adds `--b2-hard-delete`, which the trimmed rclone does not know).
* The rclone remote is entirely environment variables: `RCLONE_CONFIG_REPLIQUE_TYPE=sftp`,
  `_HOST`, `_PORT` (23 for a Storage Box), `_USER`, `_KEY_PEM` (the key never
  touches the disk unencrypted), `_KNOWN_HOSTS_FILE` and `_HOST_KEY_ALGORITHMS`
  from the pairing reply's `known_hosts` (otherwise Go asks for another host key
  type than the one known and the check fails), `_SHELL_TYPE=none`.
* `RESTIC_PASSWORD` in the environment, `TMPDIR` and the cache in the app's cache
  folder, `HOME`/`USER` set (Go's `os/user` on Android only reads those),
  `RESTIC_PROGRESS_FPS=0.5` and `--json` for progress.
* Excludes go through exclude files with `$` written `?`: restic expands `$VARS`
  in patterns, even in files.
* Stopping: restic is started through `sh -c 'echo pid; exec restic …'` so the
  app knows its pid and can send it SIGTERM (restic then unlocks the repository
  and exits). `Process.destroy()` closes the pipes first, restic's farewell line
  then kills it with SIGPIPE and leaves a stale lock.

### Tests

```sh
./gradlew testDebugUnitTest      # JVM: every proto/fixtures file, restic's JSON, plan, spool, keys, budget
REPLIQUE_SFTP_HOSTNAME=10.0.2.2.nip.io scripts/device-test.sh   # on a running emulator
```

The unit tests read `../proto/fixtures` in place and fail on a fixture they do
not know. restic's JSON samples in `app/src/test/resources/restic` were recorded
from restic 0.19.1.

`scripts/device-test.sh` starts a local SFTP server in Docker shaped like a
Storage Box sub-account (chrooted, repository at `/home/restic`, port 2224 on the
host), runs the instrumented tests against it from the emulator (`10.0.2.2`),
and removes it. They run restic and rclone from the native library directory:
idempotent init, a backup with the default excludes (`$RECYCLE.BIN` included),
`ls`, `snapshots`, `check`, a stopped backup leaving no lock, the system resolver
answering for the binaries, a backup to the server by DNS name, and whole
`BackupRunner` runs (reports, check snapshot ids, deadline stop, offline spool).
For the by-name test, start the emulator with `-dns-server 8.8.8.8` if your
router refuses DNS answers that point at private addresses.

To try the whole app on an emulator, run `scripts/test-sftp.sh start`, then
`scripts/fake-server.py` (it pairs anything and installs the phone's key on the
test server), and open
`replique://pair?server=http%3A%2F%2F10.0.2.2%3A8787&code=TEST-CODE` on the
emulator (`adb shell am start -a android.intent.action.VIEW -d '…'`).
