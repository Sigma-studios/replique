//! `replique.toml`: everything the server needs before it can open its database.
//!
//! Secrets never live in this file: it only names the files holding them (the
//! Hetzner token, the admin password hash), so the file itself can sit in a Nix
//! store or a git repository. Settings changed on the website (thresholds, the
//! Discord webhook, a token typed there) are kept in the database instead.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where the website and the device API listen. Behind a reverse proxy
    /// (façade's entry point) this stays on loopback or the container network.
    pub listen: SocketAddr,
    /// The address devices and browsers use, e.g. `https://replique.sigma-dev.eu`.
    /// Goes into pairing links and Discord messages; an `https` URL also makes
    /// the session cookie `Secure`.
    pub public_url: String,
    /// The database, the master key that encrypts secrets at rest, Réplique's
    /// own SSH key and restic's cache.
    pub state_dir: PathBuf,
    /// Defaults to `<state_dir>/replique.db`.
    pub database: Option<PathBuf>,
    /// A file holding the admin password's argon2 hash (`replique-server
    /// hash-password` prints one). `REPLIQUE_ADMIN_PASSWORD_HASH` in the
    /// environment is used when this is unset, for containers whose platform
    /// keeps secrets as environment variables.
    pub admin_password_hash_file: Option<PathBuf>,
    /// The owner every device belongs to in v1. Its slug is the first folder of
    /// each repository's home on the box (`replique/<owner>/<device>`).
    pub owner_name: String,
    pub storage_box: StorageBoxConfig,
    pub measurement: MeasurementConfig,
    pub restore: RestoreConfig,
    pub alerts: AlertsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: "127.0.0.1:8080".parse().unwrap(),
            public_url: "http://localhost:8080".into(),
            state_dir: "/var/lib/replique".into(),
            database: None,
            admin_password_hash_file: None,
            owner_name: "admin".into(),
            storage_box: StorageBoxConfig::default(),
            measurement: MeasurementConfig::default(),
            restore: RestoreConfig::default(),
            alerts: AlertsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageBoxConfig {
    /// The Storage Box's id in Hetzner's API (the number in the Console's URL).
    /// Can also be set on the Settings page.
    pub id: Option<u64>,
    pub api_url: String,
    /// A file holding the Hetzner API token. When unset, the token typed on the
    /// Settings page is used (stored encrypted).
    pub hetzner_token_file: Option<PathBuf>,
    /// The SSH host of the box, e.g. `u123456.your-storagebox.de`. Defaults to
    /// what the API calls the box's `server`.
    pub host: Option<String>,
    /// SSH, SFTP and restic all use 23 on a Storage Box: 22 is SFTP-only and has
    /// no `install-ssh-key`.
    pub port: u16,
    /// How Réplique reads repositories to measure and restore them: `twin` (a
    /// read-only sub-account sharing the device's home, the plan) or `main` (the
    /// main account, with Réplique's own key installed on it by hand), in case
    /// Hetzner refuses two sub-accounts with the same home directory.
    pub measure_via: MeasureVia,
    /// The main account's login for `measure_via = "main"`. Defaults to the
    /// box's `username` from the API.
    pub main_username: Option<String>,
    /// Where sub-account homes appear when logged in as the main account.
    pub main_root: String,
    /// How a device's SSH key gets onto its sub-account during pairing.
    pub key_install: KeyInstall,
    /// `known_hosts` lines to trust for the box. Empty: the first key seen is
    /// remembered and required from then on.
    pub known_hosts: Vec<String>,
    /// Development and tests only: repositories are folders under this path
    /// instead of SFTP logins (the Hetzner API is still used to provision).
    pub local_root: Option<PathBuf>,
}

impl Default for StorageBoxConfig {
    fn default() -> Self {
        StorageBoxConfig {
            id: None,
            api_url: "https://api.hetzner.com/v1".into(),
            hetzner_token_file: None,
            host: None,
            port: replique_proto::STORAGE_BOX_SSH_PORT,
            measure_via: MeasureVia::Twin,
            main_username: None,
            main_root: "/home".into(),
            key_install: KeyInstall::InstallSshKey,
            known_hosts: Vec::new(),
            local_root: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeasureVia {
    Twin,
    Main,
}

impl MeasureVia {
    pub fn as_str(self) -> &'static str {
        match self {
            MeasureVia::Twin => "twin",
            MeasureVia::Main => "main",
        }
    }
}

/// The three ways to put a device's key on its sub-account, pending Hetzner's
/// answer to which one works with a sub-account password.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyInstall {
    /// `install-ssh-key` over SSH on port 23, with the password once.
    InstallSshKey,
    /// Write `.ssh/authorized_keys` over SFTP, with the password once.
    Sftp,
    /// Pairing shows the admin a one-line command to run, and the passwords
    /// are rotated once the admin says it is done.
    Manual,
}

impl KeyInstall {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyInstall::InstallSshKey => "install-ssh-key",
            KeyInstall::Sftp => "sftp",
            KeyInstall::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MeasurementConfig {
    pub enabled: bool,
    /// Seconds between two measurements of the box and of every repository.
    pub every_secs: u64,
}

impl Default for MeasurementConfig {
    fn default() -> Self {
        MeasurementConfig {
            enabled: true,
            every_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RestoreConfig {
    /// The restic binary.
    pub restic: PathBuf,
    /// An unlocked repository forgets its password after this long unused.
    pub idle_timeout_secs: u64,
}

impl Default for RestoreConfig {
    fn default() -> Self {
        RestoreConfig {
            restic: "restic".into(),
            idle_timeout_secs: 15 * 60,
        }
    }
}

/// Starting values; the Settings page changes them without a restart.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlertsConfig {
    pub box_warn_percent: u8,
    pub box_alert_percent: u8,
    /// A running run with no news for this long (or past restic's own estimate
    /// by this much) is declared lost.
    pub lost_after_secs: u64,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        AlertsConfig {
            box_warn_percent: 80,
            box_alert_percent: 95,
            lost_after_secs: 3 * 3600,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Config::parse(&raw).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(raw: &str) -> anyhow::Result<Config> {
        let mut config: Config = toml::from_str(raw)?;
        config.public_url = config.public_url.trim_end_matches('/').to_string();
        url::Url::parse(&config.public_url).context("public_url is not a URL")?;
        if config.measurement.every_secs < 30 {
            bail!("[measurement] every_secs must be at least 30");
        }
        let a = &config.alerts;
        if a.box_warn_percent > 100 || a.box_alert_percent > 100 {
            bail!("[alerts] percentages must be 0 to 100");
        }
        Ok(config)
    }

    pub fn database_path(&self) -> PathBuf {
        self.database
            .clone()
            .unwrap_or_else(|| self.state_dir.join("replique.db"))
    }

    /// Cookies are only marked `Secure` behind HTTPS, so a local
    /// `http://localhost` setup can still log in.
    pub fn secure_cookies(&self) -> bool {
        self.public_url.starts_with("https://")
    }
}

/// Reads a secret from a file, trimmed. The error names the file, never the
/// content.
pub fn read_secret_file(path: &Path) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading secret file {}", path.display()))?;
    let value = raw.trim().to_string();
    if value.is_empty() {
        bail!("secret file {} is empty", path.display());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.storage_box.port, 23);
        assert_eq!(c.storage_box.measure_via, MeasureVia::Twin);
        assert_eq!(c.storage_box.key_install, KeyInstall::InstallSshKey);
        assert_eq!(
            c.database_path(),
            PathBuf::from("/var/lib/replique/replique.db")
        );
    }

    #[test]
    fn switches_parse() {
        let c = Config::parse(
            r#"
            public_url = "https://replique.example.com/"
            [storage_box]
            id = 42
            measure_via = "main"
            key_install = "sftp"
            "#,
        )
        .unwrap();
        assert_eq!(c.public_url, "https://replique.example.com");
        assert!(c.secure_cookies());
        assert_eq!(c.storage_box.measure_via, MeasureVia::Main);
        assert_eq!(c.storage_box.key_install, KeyInstall::Sftp);
        assert!(Config::parse("bogus = 1").is_err());
    }
}
