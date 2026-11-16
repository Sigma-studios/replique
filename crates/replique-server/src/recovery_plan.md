# Recovery plan

Where everything is, and what to do when something is lost. **No secrets here,
only where they are.** This page is the original; the printed copy is in both kit
envelopes. Print it again after every change.

## Where the root secrets are

| Secret | Where |
| --- | --- |
| Bitwarden master password | In my head only. Emergency Access: _(who, waiting period)_ |
| Aegis password | In my head, and in the kit |
| Bitwarden 2FA recovery code | Kit |
| Réplique admin password | Bitwarden, and the kit |
| façade offsite key | Bitwarden, and the kit |
| PC and phone restic passwords | Bitwarden _(optionally the kit)_ |

## Accounts

| Account | Password | 2FA | Recovery codes |
| --- | --- | --- | --- |
| Bitwarden | My head | Aegis _(+ security keys)_ | Kit |
| VPS provider (façade, mail) | Bitwarden | Aegis | Kit |
| Mail (valet, on façade) | Bitwarden | _(…)_ | _(…)_ |
| Hetzner | Bitwarden | Aegis | Kit |
| Porkbun | Bitwarden | Aegis | Kit |
| GitHub | Bitwarden | Aegis | Kit |
| Bank | Bitwarden | _(…)_ | Kit |
| Everything else | Bitwarden | Bitwarden, or Authy (its backup password in Bitwarden) | Bitwarden |

## Where the copies are

- Kit envelope 1: _(home, where)_. Envelope 2: _(away, with whom)_. Seals signed across the flap.
- Bitwarden vault copies: Réplique → Recovery, and the Storage Box (`replique/_replique/vault/`).
- This plan: Réplique, the Storage Box (`replique/_replique/recovery/`), both envelopes.
- Aegis backups: the phone's shared storage, backed up by Réplique to the Storage Box and the PC's copy.

## In case of

### Phone lost or stolen
1. From the PC: revoke the phone's token on Réplique (its device page); sign it out of the important accounts; wipe it remotely.
2. New phone: Bitwarden (master password); Aegis, importing its latest backup (restored from the phone's backup with the phone's restic password, in Bitwarden); Réplique, paired again.
3. Accounts still on Authy: a new SIM with the same number from the carrier, then Authy with its backup password.

### PC dead
1. Restore with plain restic: the restore runbook (linked in Réplique's footer). Passwords: Bitwarden on the phone.
2. Reinstall NixOS from the config; pair the PC again with `--adopt`.

### Bitwarden unreachable, or the account lost
1. The apps on the PC and the phone still open offline: export from there first if they do.
2. Otherwise: Réplique → Recovery → download the latest Bitwarden copy (admin password in the kit) and decrypt it offline with the master password (restore runbook, "Bitwarden is gone").
3. Bitwarden's 2FA recovery code: the kit.

### VPS lost (façade, Réplique, the mail)
1. Rebuild façade from its offsite copy: façade's README, "Rebuilding a lost server" (offsite key in the kit).
2. Réplique and the mail come back with façade's apps.
3. Meanwhile, backups can still be restored with plain restic from the Storage Box (restore runbook).

### House fire: the devices and envelope 1 gone
1. Get envelope 2.
2. Bitwarden on any computer: master password, and the recovery code from the kit.
3. Then as "PC dead" and "phone lost".

### If something happens to me
- Bitwarden Emergency Access: _(who)_, after _(waiting period)_.
- Envelope 2 is with _(who)_, who knows what it is and when to open it.

## Review log

| Date | What |
| --- | --- |
| _(date)_ | First version |
