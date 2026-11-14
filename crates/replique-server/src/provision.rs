//! Adding, pairing and removing devices: the sub-accounts on the box, the keys
//! on them, the pairing code and the report token.
//!
//! A sub-account's password exists only between adding a device and pairing
//! it: sealed in the pairing code's row, used once to install the device's
//! key (and Réplique's on the twin), then replaced on Hetzner by a random one
//! nobody keeps. From then on only keys log in.

use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use axum::http::StatusCode;
use replique_proto::{
    DeviceKind, PAIRING_CODE_TTL_SECS, PairRequest, PairResponse, StorageInfo, error_code,
};
use serde::{Deserialize, Serialize};

use crate::App;
use crate::api::ApiErr;
use crate::config::{KeyInstall, MeasureVia};
use crate::db::{self, Device, PairingCode, Repository};
use crate::events::Event;
use crate::hetzner::{AccessSettings, NewSubaccount};
use crate::secrets::{self, Secret, sha256_hex};
use crate::ssh::{self, Endpoint};

/// Sealed in `pairing_codes.secrets` until pairing is done.
#[derive(Default, Serialize, Deserialize)]
struct PairingSecrets {
    device_password: Option<String>,
    twin_password: Option<String>,
}

impl Drop for PairingSecrets {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.device_password.zeroize();
        self.twin_password.zeroize();
    }
}

fn seal(app: &App, s: &PairingSecrets) -> anyhow::Result<Option<Vec<u8>>> {
    if s.device_password.is_none() && s.twin_password.is_none() {
        return Ok(None);
    }
    let json = zeroize::Zeroizing::new(serde_json::to_vec(s)?);
    Ok(Some(app.secrets.seal(&json)))
}

fn unseal(app: &App, sealed: Option<&[u8]>) -> anyhow::Result<PairingSecrets> {
    match sealed {
        Some(bytes) => Ok(serde_json::from_slice(&app.secrets.open(bytes)?)?),
        None => Ok(PairingSecrets::default()),
    }
}

/// Without a Hetzner token, a `local_root` setup simulates the sub-accounts:
/// for development and tests, never for real backups.
fn simulated(app: &App) -> bool {
    app.config.storage_box.local_root.is_some() && app.hetzner().is_err()
}

/// Whether pairing waits for the admin to install keys by hand.
fn manual(app: &App) -> bool {
    app.config.storage_box.key_install == KeyInstall::Manual
        && app.config.storage_box.local_root.is_none()
}

fn uses_twins(app: &App) -> bool {
    app.config.storage_box.measure_via == MeasureVia::Twin
}

pub struct NewDevice {
    pub name: String,
    pub kind: DeviceKind,
    /// An existing sub-account to adopt (its username), instead of creating one.
    pub adopt: Option<String>,
}

/// Creates the device's sub-accounts and a pairing code; returns the device id.
pub async fn add_device(app: &Arc<App>, new: NewDevice) -> anyhow::Result<String> {
    let name = new.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 64 {
        bail!("Give the device a name of 1 to 64 characters.");
    }
    let base = replique_proto::slugify(&name);
    let mut slug = base.clone();
    let mut n = 2;
    while app.store.slug_taken(&app.owner.id, &slug)? {
        slug = format!("{base}-{n}");
        n += 1;
    }
    let device_id = db::new_id("dev");
    let repo_id = db::new_id("repo");
    let now = db::now();
    let mut secrets = PairingSecrets::default();

    let repo = if simulated(app) {
        let home = replique_proto::sub_account_home(&app.owner.slug, &slug);
        Repository {
            id: repo_id,
            device_id: device_id.clone(),
            host: "localhost".into(),
            port: app.config.storage_box.port,
            subaccount_id: None,
            username: format!("local-{slug}"),
            home_directory: home,
            twin_subaccount_id: None,
            twin_username: uses_twins(app).then(|| format!("local-{slug}-ro")),
            repo_path: replique_proto::DEVICE_REPO_PATH.into(),
            adopted: new.adopt.is_some(),
            device_key_installed: false,
            twin_key_installed: false,
            created_at: now,
        }
    } else {
        let (hetzner, box_id) = app.hetzner()?;
        if app.box_host().is_none() {
            crate::measure::refresh_box(app).await?;
        }
        let host = app.box_host().ok_or_else(|| {
            anyhow!("The Storage Box's host is not known; set [storage_box] host.")
        })?;
        let description = format!("Réplique: {name}");
        let (device_sub, adopted) = match &new.adopt {
            Some(username) => {
                let found = hetzner
                    .subaccounts(box_id)
                    .await?
                    .into_iter()
                    .find(|s| s.username == username.trim())
                    .ok_or_else(|| anyhow!("No sub-account named {username} on this box."))?;
                (found, true)
            }
            None => {
                let home = replique_proto::sub_account_home(&app.owner.slug, &slug);
                let password = secrets::new_box_password();
                let sub = hetzner
                    .create_subaccount(
                        box_id,
                        NewSubaccount {
                            home_directory: &home,
                            password: &password,
                            access_settings: AccessSettings::ssh_only(false),
                            description: &description,
                        },
                    )
                    .await
                    .context("creating the device's sub-account")?;
                secrets.device_password = Some(password.expose().to_string());
                (sub, false)
            }
        };
        let mut twin = None;
        if uses_twins(app) {
            let password = secrets::new_box_password();
            let created = hetzner
                .create_subaccount(
                    box_id,
                    NewSubaccount {
                        home_directory: &device_sub.home_directory,
                        password: &password,
                        access_settings: AccessSettings::ssh_only(true),
                        description: &format!("Réplique: {name} (read-only)"),
                    },
                )
                .await;
            match created {
                Ok(t) => {
                    secrets.twin_password = Some(password.expose().to_string());
                    twin = Some(t);
                }
                Err(e) => {
                    // Leave nothing half made behind.
                    if !adopted {
                        let _ = hetzner.delete_subaccount(box_id, device_sub.id).await;
                    }
                    return Err(e.context("creating the read-only twin"));
                }
            }
        }
        Repository {
            id: repo_id,
            device_id: device_id.clone(),
            host,
            port: app.config.storage_box.port,
            subaccount_id: Some(device_sub.id as i64),
            username: device_sub.username.clone(),
            home_directory: device_sub.home_directory.clone(),
            twin_subaccount_id: twin.as_ref().map(|t| t.id as i64),
            twin_username: twin.map(|t| t.username),
            repo_path: replique_proto::DEVICE_REPO_PATH.into(),
            adopted,
            device_key_installed: adopted,
            twin_key_installed: false,
            created_at: now,
        }
    };

    let device = Device {
        id: device_id.clone(),
        owner_id: app.owner.id.clone(),
        name: name.clone(),
        slug,
        kind: new.kind,
        has_token: false,
        ssh_public_key: None,
        client_version: None,
        created_at: now,
        paired_at: None,
        revoked_at: None,
        last_seen_at: None,
    };
    app.store.insert_device(&device, &repo)?;
    insert_code(app, &device_id, &secrets)?;
    tracing::info!(device = %name, repo = %repo.username, adopted = repo.adopted, "device added");
    app.hub.publish(Event::Devices);
    Ok(device_id)
}

fn insert_code(app: &App, device_id: &str, secrets: &PairingSecrets) -> anyhow::Result<String> {
    let code = secrets::new_pairing_code();
    let now = db::now();
    app.store.insert_pairing_code(&PairingCode {
        code: replique_proto::normalize_pairing_code(&code),
        device_id: device_id.to_string(),
        created_at: now,
        expires_at: now + PAIRING_CODE_TTL_SECS as i64,
        used_at: None,
        secrets: seal(app, secrets)?,
    })?;
    Ok(code)
}

/// Shows a stored code as `XXXX-XXXX`.
pub fn display_code(code: &str) -> String {
    if code.len() == 8 {
        format!("{}-{}", &code[..4], &code[4..])
    } else {
        code.to_string()
    }
}

/// A new pairing code for a device already added (a reinstalled device, or
/// the first code expired). The sub-accounts get fresh passwords for it.
pub async fn new_pairing_code(app: &Arc<App>, device_id: &str) -> anyhow::Result<()> {
    let repo = app
        .store
        .repository_of(device_id)?
        .ok_or_else(|| anyhow!("no such device"))?;
    let mut secrets = PairingSecrets::default();
    if !simulated(app) && !manual(app) {
        let (hetzner, box_id) = app.hetzner()?;
        if !repo.adopted
            && let Some(id) = repo.subaccount_id
        {
            let password = secrets::new_box_password();
            hetzner
                .reset_subaccount_password(box_id, id as u64, &password)
                .await?;
            secrets.device_password = Some(password.expose().to_string());
        }
        if !repo.twin_key_installed
            && let Some(id) = repo.twin_subaccount_id
        {
            let password = secrets::new_box_password();
            hetzner
                .reset_subaccount_password(box_id, id as u64, &password)
                .await?;
            secrets.twin_password = Some(password.expose().to_string());
        }
    } else if manual(app) {
        // Keep the passwords the admin still needs for the manual commands.
        if let Some(sealed) = app.store.manual_pairing_secrets(device_id)? {
            secrets = unseal(app, Some(&sealed))?;
        }
    }
    insert_code(app, device_id, &secrets)?;
    app.hub.publish(Event::Devices);
    Ok(())
}

fn upstream(e: anyhow::Error) -> ApiErr {
    tracing::warn!("pairing: {e:#}");
    ApiErr::new(
        StatusCode::BAD_GATEWAY,
        error_code::UPSTREAM,
        format!(
            "Installing the key on the Storage Box failed; the code stays valid, try again. ({e:#})"
        ),
    )
}

/// `POST /api/v1/pair`.
pub async fn pair(app: &Arc<App>, req: PairRequest) -> Result<PairResponse, ApiErr> {
    // One pairing at a time: a code must not be spent twice by two racing
    // devices, and the key installation is not something to interleave.
    let _guard = app.pairing.lock().await;
    let code = replique_proto::normalize_pairing_code(&req.code);
    let invalid = || {
        ApiErr::new(
            StatusCode::BAD_REQUEST,
            error_code::INVALID_CODE,
            "Unknown or already used pairing code.",
        )
    };
    let pairing = app.store.pairing_code(&code)?.ok_or_else(invalid)?;
    if pairing.used_at.is_some() {
        return Err(invalid());
    }
    if pairing.expires_at < db::now() {
        return Err(ApiErr::new(
            StatusCode::BAD_REQUEST,
            error_code::CODE_EXPIRED,
            "This pairing code expired; create a new one on the website.",
        ));
    }
    let public_key = ssh::parse_public_key(&req.ssh_public_key)
        .map_err(|e| ApiErr::bad_request(format!("ssh_public_key: {e}")))?;
    let device = app.store.device(&pairing.device_id)?.ok_or_else(invalid)?;
    let repo = app
        .store
        .repository_of(&device.id)?
        .ok_or_else(|| ApiErr::internal(anyhow!("device without repository")))?;
    let secrets = unseal(app, pairing.secrets.as_deref())?;
    let manual = manual(app);
    let simulated = simulated(app);

    if !manual {
        // The device's own key on its sub-account; an adopted sub-account
        // already has it and is left alone.
        if !repo.adopted {
            let endpoint = Endpoint {
                host: repo.host.clone(),
                port: repo.port,
                username: repo.username.clone(),
            };
            let password = Secret::new(secrets.device_password.clone().unwrap_or_default());
            app.install_keys(
                &endpoint,
                &repo.home_directory,
                &password,
                std::slice::from_ref(&public_key),
            )
            .await
            .map_err(upstream)?;
        }
        // Réplique's key on the read-only twin.
        if let Some(twin) = &repo.twin_username
            && !repo.twin_key_installed
        {
            let endpoint = Endpoint {
                host: repo.host.clone(),
                port: repo.port,
                username: twin.clone(),
            };
            let password = Secret::new(secrets.twin_password.clone().unwrap_or_default());
            app.install_keys(
                &endpoint,
                &repo.home_directory,
                &password,
                &[app.public_key_line()],
            )
            .await
            .map_err(upstream)?;
        }
        if !simulated {
            rotate_passwords(app, &repo).await.map_err(upstream)?;
        }
        app.store
            .set_keys_installed(&repo.id, true, repo.twin_username.is_some())?;
    }

    let token = secrets::new_report_token();
    app.store.finish_pairing(
        &code,
        manual,
        &device.id,
        &sha256_hex(token.expose().as_bytes()),
        &public_key,
        req.client_version.as_deref(),
    )?;
    tracing::info!(device = %device.name, manual, "device paired");
    app.hub.publish(Event::Devices);
    Ok(PairResponse {
        device_id: device.id.clone(),
        device_name: device.name.clone(),
        report_token: token.expose().to_string(),
        storage: StorageInfo {
            known_hosts: app.known_hosts_lines(&repo.host, repo.port),
            host: repo.host.clone(),
            port: repo.port,
            username: repo.username.clone(),
            repo_path: repo.repo_path.clone(),
        },
    })
}

/// Replaces the passwords used for pairing with random ones nobody keeps.
async fn rotate_passwords(app: &App, repo: &Repository) -> anyhow::Result<()> {
    let (hetzner, box_id) = app.hetzner()?;
    if !repo.adopted
        && let Some(id) = repo.subaccount_id
    {
        hetzner
            .reset_subaccount_password(box_id, id as u64, &secrets::new_box_password())
            .await?;
    }
    if let Some(id) = repo.twin_subaccount_id {
        hetzner
            .reset_subaccount_password(box_id, id as u64, &secrets::new_box_password())
            .await?;
    }
    Ok(())
}

/// The one-line commands the admin runs in `manual` mode, with the password
/// each asks for once.
pub struct ManualStep {
    pub command: String,
    pub password: Secret,
}

pub fn manual_steps(app: &App, device: &Device) -> anyhow::Result<Vec<ManualStep>> {
    let Some(sealed) = app.store.manual_pairing_secrets(&device.id)? else {
        return Ok(vec![]);
    };
    let Some(repo) = app.store.repository_of(&device.id)? else {
        return Ok(vec![]);
    };
    let secrets = unseal(app, Some(&sealed))?;
    let mut steps = Vec::new();
    if let (Some(key), Some(password), false) = (
        &device.ssh_public_key,
        &secrets.device_password,
        repo.adopted,
    ) {
        steps.push(ManualStep {
            command: format!(
                "echo '{key}' | ssh -p {} {}@{} install-ssh-key",
                repo.port, repo.username, repo.host
            ),
            password: Secret::new(password.clone()),
        });
    }
    if let (Some(twin), Some(password)) = (&repo.twin_username, &secrets.twin_password) {
        steps.push(ManualStep {
            command: format!(
                "echo '{}' | ssh -p {} {twin}@{} install-ssh-key",
                app.public_key_line(),
                repo.port,
                repo.host
            ),
            password: Secret::new(password.clone()),
        });
    }
    Ok(steps)
}

/// The admin ran the manual commands: rotate the passwords and forget them.
pub async fn finish_manual(app: &Arc<App>, device_id: &str) -> anyhow::Result<()> {
    let repo = app
        .store
        .repository_of(device_id)?
        .ok_or_else(|| anyhow!("no such device"))?;
    rotate_passwords(app, &repo).await?;
    app.store.forget_pairing_secrets(device_id)?;
    app.store
        .set_keys_installed(&repo.id, true, repo.twin_username.is_some())?;
    app.hub.publish(Event::Devices);
    Ok(())
}

fn not_found_is_ok(result: anyhow::Result<()>) -> anyhow::Result<()> {
    match result {
        Err(e)
            if e.downcast_ref::<crate::hetzner::ApiError>()
                .is_some_and(|a| a.status == StatusCode::NOT_FOUND) =>
        {
            Ok(())
        }
        other => other,
    }
}

/// Removes a device: optionally its backups, then its sub-accounts, its token,
/// its runs and its measurements.
pub async fn remove_device(
    app: &Arc<App>,
    device_id: &str,
    delete_data: bool,
) -> anyhow::Result<()> {
    let device = app
        .store
        .device(device_id)?
        .ok_or_else(|| anyhow!("no such device"))?;
    let repo = app.store.repository_of(device_id)?;
    if let Some(repo) = &repo {
        if simulated(app) {
            if delete_data {
                let password = Secret::new(String::new());
                let endpoint = Endpoint {
                    host: repo.host.clone(),
                    port: repo.port,
                    username: repo.username.clone(),
                };
                app.delete_repository_data(&endpoint, &repo.home_directory, &password)
                    .await?;
            }
        } else {
            let (hetzner, box_id) = app.hetzner()?;
            if delete_data && let Some(id) = repo.subaccount_id {
                // Nobody knows the sub-account's password: set one for this.
                let password = secrets::new_box_password();
                hetzner
                    .reset_subaccount_password(box_id, id as u64, &password)
                    .await?;
                let endpoint = Endpoint {
                    host: repo.host.clone(),
                    port: repo.port,
                    username: repo.username.clone(),
                };
                app.delete_repository_data(&endpoint, &repo.home_directory, &password)
                    .await
                    .context("deleting the repository's files")?;
            }
            if let Some(id) = repo.twin_subaccount_id {
                not_found_is_ok(hetzner.delete_subaccount(box_id, id as u64).await)?;
            }
            if let Some(id) = repo.subaccount_id {
                not_found_is_ok(hetzner.delete_subaccount(box_id, id as u64).await)?;
            }
        }
        app.unlocks.forget_repo(&repo.id);
    }
    app.store.clear_alerts_of_device(device_id)?;
    app.store.delete_device(device_id)?;
    tracing::info!(device = %device.name, delete_data, "device removed");
    app.hub.publish(Event::Devices);
    app.hub.publish(Event::Problems);
    Ok(())
}

/// Lets `mirror_id` copy `source_id`'s repository: installs the mirroring
/// device's key on the source's read-only twin (or, without twins, on the
/// source's own sub-account), using a password set just for that.
pub async fn assign_mirror(app: &Arc<App>, mirror_id: &str, source_id: &str) -> anyhow::Result<()> {
    if mirror_id == source_id {
        bail!("A device cannot mirror itself.");
    }
    let mirror = app
        .store
        .device(mirror_id)?
        .ok_or_else(|| anyhow!("no such device"))?;
    let key = mirror
        .ssh_public_key
        .clone()
        .ok_or_else(|| anyhow!("{} is not paired yet.", mirror.name))?;
    let repo = app
        .store
        .repository_of(source_id)?
        .ok_or_else(|| anyhow!("no such source device"))?;
    let (username, subaccount_id) = if uses_twins(app) {
        (repo.twin_username.clone(), repo.twin_subaccount_id)
    } else {
        (Some(repo.username.clone()), repo.subaccount_id)
    };
    let username = username.ok_or_else(|| anyhow!("The source has no read-only login."))?;
    let endpoint = Endpoint {
        host: repo.host.clone(),
        port: repo.port,
        username,
    };
    if simulated(app) {
        app.install_keys(
            &endpoint,
            &repo.home_directory,
            &Secret::new(String::new()),
            &[key],
        )
        .await?;
    } else {
        if manual(app) {
            bail!(
                "Key installation is manual: add {}'s key to {} in the Hetzner Console, then assign again.",
                mirror.name,
                endpoint.username
            );
        }
        let (hetzner, box_id) = app.hetzner()?;
        let id = subaccount_id.ok_or_else(|| anyhow!("unknown sub-account id"))? as u64;
        let password = secrets::new_box_password();
        hetzner
            .reset_subaccount_password(box_id, id, &password)
            .await?;
        let installed = app
            .install_keys(&endpoint, &repo.home_directory, &password, &[key])
            .await;
        hetzner
            .reset_subaccount_password(box_id, id, &secrets::new_box_password())
            .await?;
        installed?;
    }
    app.store.add_mirror(mirror_id, source_id)?;
    app.hub.publish(Event::Devices);
    Ok(())
}
