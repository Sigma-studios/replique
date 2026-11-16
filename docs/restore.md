# Restoring without Réplique

Anything can be restored from any computer with restic, the device's Storage
Box login and its restic password, even if Réplique, the VPS and the device
are all gone. Réplique only makes this convenient; it is never needed.

You need, for the device to restore:

* its **restic password**, saved in your password manager when the device was
  paired (Réplique never had it);
* its **sub-account login**, e.g. `u123456-sub3`, shown on the device's page in
  Réplique and in the Hetzner Console (Storage Box → Sub-accounts; its
  description reads `Réplique: <device name>`).

## 1. Log in to the sub-account

If you still have the device's SSH key (`/var/lib/replique-client` on a NixOS
machine), use it. If the key died with the device, set a new password on the
sub-account in the Hetzner Console (Sub-accounts → the account → Reset
password) and log in with it; nothing else changes on the box.

Storage Boxes take SSH, SFTP and restic on port **23** (port 22 is SFTP only).

## 2. Install restic

Any recent restic works: `apt install restic`, `brew install restic`,
`nix shell nixpkgs#restic`, or the binary from
https://github.com/restic/restic/releases.

## 3. List the snapshots

```sh
restic -r sftp:u123456-sub3@u123456.your-storagebox.de:/home/restic \
  -o sftp.args='-p 23' snapshots
```

restic asks for the repository password. Sub-accounts are chrooted to their own
folder, so the repository is always `/home/restic` from the sub-account's point
of view.

Logging in with the main account instead (`u123456`) works too; the repository
is then at `/home/replique/<owner>/<device>/restic`.

## 4. Restore

Everything from the latest snapshot:

```sh
restic -r sftp:… -o sftp.args='-p 23' restore latest --target ./restored
```

Only some files, or an older snapshot:

```sh
restic -r sftp:… -o sftp.args='-p 23' restore 1a2b3c4d --target ./restored \
  --include /home/sigma/Documents
```

Or browse every snapshot as folders (Linux and macOS with FUSE):

```sh
mkdir mnt
restic -r sftp:… -o sftp.args='-p 23' mount ./mnt
```

When a backup is running on the device at the same time, add `--no-lock`
to read without waiting for it.

## 5. Phone backups: the copy on the PC

The phone's repository is also copied, still encrypted, to the PC every day
(the mirror job's `mirror_path`, shown on the PC's page in Réplique). It opens
with the phone's restic password and needs no network:

```sh
restic -r /path/to/phone-mirror snapshots
restic -r /path/to/phone-mirror restore latest --target ./restored
```

## façade's offsite copy

façade's sub-account holds no restic repository: façade encrypts its backups with
its own offsite key before they leave, so restic and the website cannot open them.
They come back through façade, with that key from your password manager:

* One volume, on a running server: `facade backup ls <app>` lists the backups and
  where each is; `facade backup restore <app> <volume> <time> --from offsite`
  brings one back.
* A lost server: install façade on a new machine, then `facaded offsite-restore
  --key-file <file holding the key>` restores its database, secrets and
  certificate, and the apps' volumes follow. façade's README, "Rebuilding a lost
  server from its offsite copy", has every step.

## Bitwarden is gone

If `services.replique.bitwardenCopy` is on, the Bitwarden desktop app's vault,
encrypted with your master password, is copied to Réplique whenever it changes.

1. Get the latest copy: Réplique → Recovery → Password vault copies → Download
   (the admin password is in the kit). Without Réplique: it is on the Storage Box,
   in `replique/_replique/vault/<device>/bitwarden/`; in the Hetzner Console,
   reset the password of the sub-account described "Réplique: its own files" and
   download it over SFTP (port 23).
2. On a computer you trust, rename it `data.json` and decrypt it offline with
   your master password, for instance with BitwardenDecrypt
   (github.com/GurpreetKang/BitwardenDecrypt): `python3 BitwardenDecrypt.py
   data.json` asks for the master password and writes the vault as JSON. The file
   is the desktop app's own format: rehearse this once, and keep an occasional
   password-protected export (`bw export --format encrypted_json`) as the
   officially supported fallback.
3. Import the result into a new Bitwarden or Vaultwarden account, then delete the
   decrypted file.

## From the website

While Réplique is up, a device's page has **Unlock to browse and restore**:
type the repository's password, browse snapshots and folders, and download a
file, or a folder as a zip. The password stays in the server's memory for that
session only and is forgotten after 15 minutes without use or at logout.
