//! Files a device sends as they are, because they are encrypted already: the
//! Bitwarden desktop app's vault cache, encrypted with the master password.
//!
//! The point is a copy reachable without any password kept in Bitwarden: the
//! restic backups hold the same file, but opening them needs a restic password
//! that lives in the vault. So the copy is kept here, outside restic (the last
//! ten, in the state folder, downloadable from the Recovery page), and in
//! Réplique's own area on the Storage Box. Nothing here can read it.
//!
//! What a device may send is decided on the device (its NixOS config names the
//! file); the server only receives. A server able to ask for a file could ask
//! for an SSH key.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Context, anyhow, bail};

use crate::App;
use crate::config::KeyInstall;
use crate::db::{self, Device, VaultCopy};
use crate::events::Event;
use crate::hetzner::{AccessSettings, NewSubaccount};
use crate::secrets;
use crate::ssh::{self, Auth, Endpoint};

/// Copies kept per file; older ones go, here and on the box.
pub const KEEP: usize = 10;
/// A file not sent again for this long raises a warning: the PC may be off,
/// or the app may have stopped syncing.
pub const STALE_AFTER: i64 = 7 * 86_400;

const SETTING_AREA_ID: &str = "own_area_subaccount_id";
const SETTING_AREA_USER: &str = "own_area_username";
/// Réplique's own folder on the box. Device slugs never start with `_`, so it
/// cannot meet a device's home.
const AREA_HOME_SLUG: &str = "_replique";

fn file_path(app: &App, id: &str) -> PathBuf {
    app.config.state_dir.join("vault").join(format!("{id}.bin"))
}

/// Where a copy goes in Réplique's area on the box.
fn box_path(device: &Device, c: &VaultCopy) -> String {
    let at = db::to_time(c.received_at).format("%Y%m%dT%H%M%SZ");
    format!(
        "vault/{}/{}/{at}-{}.bin",
        device.slug,
        c.name,
        &c.sha256[..8]
    )
}

/// What to call a downloaded copy: the desktop app's own file name for a
/// Bitwarden copy.
pub fn download_name(device: &Device, c: &VaultCopy) -> String {
    let day = db::to_time(c.received_at).format("%Y-%m-%d");
    let ext = if c.name == "bitwarden" { "json" } else { "bin" };
    format!("{}-{}-{day}.{ext}", c.name, device.slug)
}

pub fn read(app: &App, c: &VaultCopy) -> anyhow::Result<Vec<u8>> {
    std::fs::read(file_path(app, &c.id)).with_context(|| format!("reading copy {}", c.id))
}

/// Takes a file a device sent. The same content as the newest copy only
/// confirms that copy (the device is alive and the copy current).
pub async fn receive(
    app: &Arc<App>,
    device: &Device,
    name: &str,
    bytes: &[u8],
) -> anyhow::Result<VaultCopy> {
    let sha256 = secrets::sha256_hex(bytes);
    let now = db::now();
    let copies = app.store.vault_copies(&device.id, name)?;
    if let Some(latest) = copies.first()
        && latest.sha256 == sha256
    {
        app.store.confirm_vault_copy(&latest.id, now)?;
        return Ok(VaultCopy {
            confirmed_at: now,
            ..latest.clone()
        });
    }
    let copy = VaultCopy {
        id: db::new_id("vlt"),
        device_id: device.id.clone(),
        name: name.to_string(),
        sha256,
        size: bytes.len() as i64,
        received_at: now,
        confirmed_at: now,
        on_box: false,
    };
    secrets::write_private(&file_path(app, &copy.id), bytes)?;
    app.store.insert_vault_copy(&copy)?;
    tracing::info!(device = %device.name, name, size = bytes.len(), "vault copy received");
    for old in copies.iter().skip(KEEP - 1) {
        let _ = std::fs::remove_file(file_path(app, &old.id));
        app.store.delete_vault_copy(&old.id)?;
        if old.on_box
            && let Err(e) = remove_from_area(app, &box_path(device, old)).await
        {
            tracing::warn!("removing an old vault copy from the box: {e:#}");
        }
    }
    app.hub.publish(Event::Devices);
    let app = app.clone();
    tokio::spawn(async move { sync_box(&app).await });
    Ok(copy)
}

/// Copies not on the box yet go there. Tried after each new copy and from the
/// periodic checks, at most every ten minutes after a failure.
pub async fn sync_box(app: &Arc<App>) {
    static LAST_FAILURE: AtomicI64 = AtomicI64::new(0);
    if db::now() - LAST_FAILURE.load(Ordering::Relaxed) < 600 {
        return;
    }
    let Ok(pending) = app.store.vault_copies_off_box() else {
        return;
    };
    for c in pending {
        let result = async {
            let device = app
                .store
                .device(&c.device_id)?
                .ok_or_else(|| anyhow!("device gone"))?;
            let bytes = read(app, &c)?;
            write_to_area(app, &box_path(&device, &c), &bytes).await?;
            app.store.set_vault_copy_on_box(&c.id)
        }
        .await;
        if let Err(e) = result {
            tracing::warn!("copying {} to the Storage Box: {e:#}", c.id);
            LAST_FAILURE.store(db::now(), Ordering::Relaxed);
            return;
        }
    }
}

/// A warning for every file whose device stopped sending it.
pub async fn check_stale(app: &Arc<App>) -> anyhow::Result<()> {
    for c in app.store.latest_vault_copies()? {
        let key = format!("vault_stale:{}:{}", c.device_id, c.name);
        if db::now() - c.confirmed_at > STALE_AFTER {
            let device = app.store.device(&c.device_id)?;
            let device_name = device.map(|d| d.name).unwrap_or_default();
            crate::alerts::raise(
                app,
                crate::alerts::Raise {
                    key,
                    kind: "vault_stale",
                    severity: crate::alerts::WARNING,
                    title: format!("{device_name}: no {} copy for a week", c.name),
                    detail: format!(
                        "The last copy of {} from {device_name} arrived {}. Is the computer on, and its app still syncing?",
                        c.name,
                        crate::web::fmt::datetime(c.confirmed_at)
                    ),
                    device_id: Some(&c.device_id),
                },
            )
            .await;
        } else {
            crate::alerts::clear(app, &key).await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- own area

enum Area {
    /// `[storage_box] local_root`: development and tests.
    Local(PathBuf),
    Sftp(Endpoint),
}

/// Réplique's own writable folder on the box, `replique/_replique`: a
/// sub-account of its own, created the first time something is written, with
/// Réplique's key and a password nobody keeps. Every other login Réplique
/// holds is read-only.
async fn area(app: &App) -> anyhow::Result<Area> {
    if let Some(root) = &app.config.storage_box.local_root {
        return Ok(Area::Local(root.join("replique").join(AREA_HOME_SLUG)));
    }
    let host = app
        .box_host()
        .ok_or_else(|| anyhow!("the Storage Box's host is not known yet"))?;
    if let Some(username) = app.store.setting_str(SETTING_AREA_USER)? {
        return Ok(Area::Sftp(Endpoint {
            host,
            port: app.config.storage_box.port,
            username,
        }));
    }
    // One sub-account change at a time, as for devices.
    let _guard = app.pairing.lock().await;
    if let Some(username) = app.store.setting_str(SETTING_AREA_USER)? {
        return Ok(Area::Sftp(Endpoint {
            host,
            port: app.config.storage_box.port,
            username,
        }));
    }
    if app.config.storage_box.key_install == KeyInstall::Manual {
        bail!("key installation is manual: Réplique cannot set up its own area on the box");
    }
    let (hetzner, box_id) = app.hetzner()?;
    let home = format!("replique/{AREA_HOME_SLUG}");
    let password = secrets::new_box_password();
    let sub = hetzner
        .create_subaccount(
            box_id,
            NewSubaccount {
                home_directory: &home,
                password: &password,
                access_settings: AccessSettings::ssh_only(false),
                description: "Réplique: its own files (vault copies, recovery plan)",
            },
        )
        .await
        .context("creating Réplique's own sub-account")?;
    let endpoint = Endpoint {
        host,
        port: app.config.storage_box.port,
        username: sub.username.clone(),
    };
    app.install_keys(&endpoint, &home, &password, &[app.public_key_line()])
        .await
        .context("installing Réplique's key on its own sub-account")?;
    hetzner
        .reset_subaccount_password(box_id, sub.id, &secrets::new_box_password())
        .await?;
    app.store
        .put_setting(SETTING_AREA_ID, sub.id.to_string().as_bytes())?;
    app.store
        .put_setting(SETTING_AREA_USER, sub.username.as_bytes())?;
    tracing::info!(username = %sub.username, "Réplique's own area created on the box");
    Ok(Area::Sftp(endpoint))
}

/// Writes a file in Réplique's area (`rel` under `replique/_replique`).
pub(crate) async fn write_to_area(app: &App, rel: &str, bytes: &[u8]) -> anyhow::Result<()> {
    match area(app).await? {
        Area::Local(root) => {
            let path = root.join(rel);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, bytes)?;
            Ok(())
        }
        Area::Sftp(endpoint) => {
            let conn = app.ssh_connect(&endpoint, Auth::Key(&app.ssh_key)).await?;
            let sftp = conn.sftp().await?;
            let result = ssh::write_file(&sftp, rel, bytes).await;
            let _ = sftp.close().await;
            conn.close().await;
            result
        }
    }
}

async fn remove_from_area(app: &App, rel: &str) -> anyhow::Result<()> {
    match area(app).await? {
        Area::Local(root) => match std::fs::remove_file(root.join(rel)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        },
        Area::Sftp(endpoint) => {
            let conn = app.ssh_connect(&endpoint, Auth::Key(&app.ssh_key)).await?;
            let sftp = conn.sftp().await?;
            let result = sftp
                .remove_file(rel)
                .await
                .map_err(|e| anyhow!("removing {rel}: {e}"));
            let _ = sftp.close().await;
            conn.close().await;
            result
        }
    }
}
