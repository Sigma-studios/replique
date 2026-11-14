//! Réplique's server: the device API, the website, the Hetzner client,
//! storage measurements, alerts and website restores.
//!
//! Devices back up on their own, straight to their Storage Box sub-account;
//! this server provisions that storage, receives their reports, measures what
//! is really stored and shows it. It never holds backup data, and decrypts
//! only during a restore the admin unlocked with that repository's password.

pub mod alerts;
pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod events;
pub mod hetzner;
pub mod measure;
pub mod provision;
pub mod restore;
pub mod secrets;
pub mod ssh;
pub mod storage;
pub mod web;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use axum::Router;
use parking_lot::{Mutex, RwLock};
use russh::keys::ssh_key::PrivateKey;

use crate::config::Config;
use crate::db::{Owner, Store};
use crate::events::Hub;
use crate::hetzner::Hetzner;
use crate::measure::BoxState;
use crate::secrets::{Secret, SecretBox};

pub const SETTING_HETZNER_TOKEN: &str = "hetzner_token";
pub const SETTING_BOX_ID: &str = "box_id";

pub struct App {
    pub config: Config,
    pub store: Store,
    pub secrets: SecretBox,
    pub hub: Hub,
    /// v1 has one owner, the admin; every device belongs to it.
    pub owner: Owner,
    /// Réplique's own SSH key, installed on read-only twins.
    pub ssh_key: PrivateKey,
    pub ssh_key_path: PathBuf,
    /// This program, run by restic as its SFTP transport (`sftp-bridge`).
    pub self_exe: PathBuf,
    pub admin_hash: Option<String>,
    pub box_state: RwLock<BoxState>,
    pub measure_errors: Mutex<HashMap<String, String>>,
    pub unlocks: restore::Unlocks,
    pub pairing: tokio::sync::Mutex<()>,
    pub http: reqwest::Client,
    /// How often a running Hetzner action is polled (tests shorten it).
    pub hetzner_poll: Duration,
}

/// Where the Hetzner token comes from, for the Settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    File,
    Settings,
    Missing,
}

impl App {
    pub fn new(config: Config) -> anyhow::Result<App> {
        std::fs::create_dir_all(&config.state_dir)
            .with_context(|| format!("creating {}", config.state_dir.display()))?;
        // The database names every device and holds sealed secrets; nobody
        // but the server's user has any business reading the folder.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&config.state_dir, std::fs::Permissions::from_mode(0o700));
        }
        let store = Store::open(&config.database_path())?;
        let secrets = SecretBox::load_or_create(&config.state_dir.join("master.key"))?;
        let owner = store.ensure_admin_owner(&config.owner_name)?;
        let ssh_key_path = config.state_dir.join("ssh").join("id_ed25519");
        let ssh_key = ssh::load_or_create_key(&ssh_key_path)?;
        let admin_hash = match &config.admin_password_hash_file {
            Some(path) => Some(config::read_secret_file(path)?),
            None => std::env::var("REPLIQUE_ADMIN_PASSWORD_HASH")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        };
        if let Some(hash) = &admin_hash {
            argon2::PasswordHash::new(hash)
                .map_err(|e| anyhow!("the admin password hash is not an argon2 hash: {e}"))?;
        } else {
            tracing::warn!(
                "no admin password hash configured (admin_password_hash_file or REPLIQUE_ADMIN_PASSWORD_HASH): nobody can log in"
            );
        }
        if let Some(path) = &config.storage_box.hetzner_token_file {
            config::read_secret_file(path)?;
        }
        let self_exe = std::env::current_exe().unwrap_or_else(|_| "replique-server".into());
        Ok(App {
            config,
            store,
            secrets,
            hub: Hub::default(),
            owner,
            ssh_key,
            ssh_key_path,
            self_exe,
            admin_hash,
            box_state: RwLock::new(BoxState::default()),
            measure_errors: Mutex::new(HashMap::new()),
            unlocks: restore::Unlocks::default(),
            pairing: tokio::sync::Mutex::new(()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent(concat!("replique-server/", env!("CARGO_PKG_VERSION")))
                .build()?,
            hetzner_poll: Duration::from_secs(2),
        })
    }

    pub fn public_key_line(&self) -> String {
        let mut line = ssh::key_line(self.ssh_key.public_key());
        line.push_str(" replique-server");
        line
    }

    pub fn token_source(&self) -> TokenSource {
        if self.config.storage_box.hetzner_token_file.is_some() {
            TokenSource::File
        } else if matches!(self.store.setting(SETTING_HETZNER_TOKEN), Ok(Some(_))) {
            TokenSource::Settings
        } else {
            TokenSource::Missing
        }
    }

    fn hetzner_token(&self) -> anyhow::Result<Secret> {
        if let Some(path) = &self.config.storage_box.hetzner_token_file {
            return Ok(Secret::new(config::read_secret_file(path)?));
        }
        let sealed = self
            .store
            .setting(SETTING_HETZNER_TOKEN)?
            .ok_or_else(|| anyhow!("No Hetzner API token yet: add one on the Settings page."))?;
        self.secrets.open_string(&sealed)
    }

    pub fn box_id(&self) -> Option<u64> {
        self.config.storage_box.id.or_else(|| {
            self.store
                .setting_str(SETTING_BOX_ID)
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok())
        })
    }

    /// A client for Hetzner's API and the box id, or why there is none.
    pub fn hetzner(&self) -> anyhow::Result<(Hetzner, u64)> {
        let token = self.hetzner_token()?;
        let box_id = self
            .box_id()
            .ok_or_else(|| anyhow!("No Storage Box id yet: set it on the Settings page."))?;
        let mut client = Hetzner::new(&self.config.storage_box.api_url, token)?;
        client.poll_every = self.hetzner_poll;
        Ok((client, box_id))
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .merge(api::router())
        .merge(web::router())
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            auth::csrf_layer,
        ))
        .with_state(app)
}

/// The background work: measurements, alert checks, expiring passwords.
pub fn spawn_background(app: &Arc<App>) {
    if app.config.measurement.enabled {
        // Its first round runs at once, which also reads the box's quota and
        // sub-account limit for the Settings page.
        tokio::spawn(measure::run_loop(app.clone()));
    } else if app.hetzner().is_ok() {
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = measure::refresh_box(&app).await {
                tracing::warn!("reading the Storage Box from Hetzner: {e:#}");
            }
        });
    }
    tokio::spawn(alerts::run_loop(app.clone()));
    tokio::spawn(restore::sweep_loop(app.clone()));
}

pub async fn serve(app: Arc<App>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(app.config.listen)
        .await
        .with_context(|| format!("listening on {}", app.config.listen))?;
    tracing::info!(
        "Réplique listening on {} ({})",
        app.config.listen,
        app.config.public_url
    );
    spawn_background(&app);
    axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}
