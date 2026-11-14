//! How Réplique reaches the repositories on the box: which login reads each
//! one (its read-only twin, or the main account), how keys get installed on a
//! sub-account, and what restic needs to open a repository for a restore.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};

use crate::App;
use crate::config::{KeyInstall, MeasureVia};
use crate::db::Repository;
use crate::secrets::Secret;
use crate::ssh::{self, Auth, Connection, Endpoint, Trust};

/// Where a repository can be read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// Over SFTP with Réplique's own key.
    Sftp { endpoint: Endpoint, path: String },
    /// A local folder (`[storage_box] local_root`, for development and tests).
    Local { path: PathBuf },
}

impl App {
    /// The box's SSH host: the configured one, else what the API calls the
    /// box's server.
    pub fn box_host(&self) -> Option<String> {
        self.config.storage_box.host.clone().or_else(|| {
            self.box_state
                .read()
                .info
                .as_ref()
                .and_then(|b| b.server.clone())
        })
    }

    /// The host keys to require for `host:port`: the configured ones, else the
    /// one seen on the first connection (trust on first use).
    pub fn trust_for(&self, host: &str, port: u16) -> Trust {
        let configured = Trust::from_lines(
            self.config
                .storage_box
                .known_hosts
                .iter()
                .map(String::as_str),
            host,
            port,
        );
        if !configured.keys.is_empty() {
            return configured;
        }
        match self.store.host_key(host, port) {
            Ok(Some(line)) => Trust::from_lines([line.as_str()], host, port),
            _ => Trust::default(),
        }
    }

    /// Connects, remembering the host key the first time a host is met.
    pub async fn ssh_connect(
        &self,
        endpoint: &Endpoint,
        auth: Auth<'_>,
    ) -> anyhow::Result<Connection> {
        let trust = self.trust_for(&endpoint.host, endpoint.port);
        let first = trust.keys.is_empty();
        let conn = ssh::connect(endpoint, auth, trust).await?;
        if first && let Some(key) = conn.host_key() {
            self.store
                .remember_host_key(&endpoint.host, endpoint.port, &ssh::key_line(key))?;
        }
        Ok(conn)
    }

    /// `known_hosts` lines for devices, as `[host]:port alg base64`.
    pub fn known_hosts_lines(&self, host: &str, port: u16) -> Vec<String> {
        self.trust_for(host, port)
            .keys
            .iter()
            .map(|k| format!("[{host}]:{port} {}", ssh::key_line(k)))
            .collect()
    }

    /// The login and path Réplique reads `repo` through.
    pub fn reader(&self, repo: &Repository) -> anyhow::Result<Access> {
        if let Some(root) = &self.config.storage_box.local_root {
            return Ok(Access::Local {
                path: root
                    .join(&repo.home_directory)
                    .join(replique_proto::REPO_DIR),
            });
        }
        match self.config.storage_box.measure_via {
            MeasureVia::Twin => {
                let username = repo
                    .twin_username
                    .clone()
                    .ok_or_else(|| anyhow!("this repository has no read-only twin"))?;
                Ok(Access::Sftp {
                    endpoint: Endpoint {
                        host: repo.host.clone(),
                        port: repo.port,
                        username,
                    },
                    path: repo.repo_path.clone(),
                })
            }
            MeasureVia::Main => {
                let username = self
                    .config
                    .storage_box
                    .main_username
                    .clone()
                    .or_else(|| {
                        self.box_state
                            .read()
                            .info
                            .as_ref()
                            .and_then(|b| b.username.clone())
                    })
                    .ok_or_else(|| anyhow!("the main account's username is not known yet"))?;
                Ok(Access::Sftp {
                    endpoint: Endpoint {
                        host: repo.host.clone(),
                        port: repo.port,
                        username,
                    },
                    path: format!(
                        "{}/{}/{}",
                        self.config.storage_box.main_root.trim_end_matches('/'),
                        repo.home_directory.trim_matches('/'),
                        replique_proto::REPO_DIR
                    ),
                })
            }
        }
    }

    /// Bytes and files in a repository, as stored.
    pub async fn measure(&self, access: &Access) -> anyhow::Result<(u64, u64)> {
        match access {
            Access::Local { path } => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || walk_local(&path)).await?
            }
            Access::Sftp { endpoint, path } => {
                let conn = self.ssh_connect(endpoint, Auth::Key(&self.ssh_key)).await?;
                let sftp = conn.sftp().await?;
                let result = ssh::walk(&sftp, path).await;
                let _ = sftp.close().await;
                conn.close().await;
                result
            }
        }
    }

    /// Puts `keys` on a sub-account, logging in with its password once, the
    /// way `[storage_box] key_install` says. In `manual` mode nothing happens
    /// here: the admin runs the command the website shows.
    pub async fn install_keys(
        &self,
        endpoint: &Endpoint,
        home_directory: &str,
        password: &Secret,
        keys: &[String],
    ) -> anyhow::Result<Option<String>> {
        if let Some(root) = &self.config.storage_box.local_root {
            let dir = root.join(home_directory).join(".ssh");
            std::fs::create_dir_all(&dir)?;
            let file = dir.join("authorized_keys");
            let existing = std::fs::read_to_string(&file).unwrap_or_default();
            std::fs::write(&file, ssh::merge_keys(&existing, keys))?;
            return Ok(None);
        }
        let conn = match self.config.storage_box.key_install {
            KeyInstall::Manual => return Ok(None),
            KeyInstall::InstallSshKey => {
                let conn = self.ssh_connect(endpoint, Auth::Password(password)).await?;
                ssh::install_key_with_command(&conn, keys).await?;
                conn
            }
            KeyInstall::Sftp => {
                let conn = self.ssh_connect(endpoint, Auth::Password(password)).await?;
                let sftp = conn.sftp().await?;
                ssh::write_authorized_keys(&sftp, keys).await?;
                let _ = sftp.close().await;
                conn
            }
        };
        let seen = conn.host_key().map(ssh::key_line);
        conn.close().await;
        Ok(seen)
    }

    /// Deletes a repository's files, logging in to its sub-account with a
    /// password set just for this.
    pub async fn delete_repository_data(
        &self,
        endpoint: &Endpoint,
        home_directory: &str,
        password: &Secret,
    ) -> anyhow::Result<()> {
        if let Some(root) = &self.config.storage_box.local_root {
            let dir = root.join(home_directory);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .with_context(|| format!("removing {}", dir.display()))?;
            }
            return Ok(());
        }
        let conn = self.ssh_connect(endpoint, Auth::Password(password)).await?;
        let sftp = conn.sftp().await?;
        // The sub-account is chrooted to the repository's home, so `restic`
        // here can only be that repository.
        let result = ssh::remove_tree(&sftp, replique_proto::REPO_DIR).await;
        let _ = sftp.close().await;
        conn.close().await;
        result
    }

    /// restic's options to open a repository read through `access`.
    pub fn restic_repo_args(&self, access: &Access) -> anyhow::Result<Vec<OsString>> {
        match access {
            Access::Local { path } => Ok(vec!["-r".into(), path.clone().into_os_string()]),
            Access::Sftp { endpoint, path } => {
                let trust = self.trust_for(&endpoint.host, endpoint.port);
                let Some(host_key) = trust.keys.first() else {
                    bail!(
                        "Réplique has never connected to {}:{}; wait for the next measurement",
                        endpoint.host,
                        endpoint.port
                    );
                };
                // restic splits sftp.command like a shell would, so every
                // value is quoted.
                let command = format!(
                    "{} sftp-bridge --host {} --port {} --user {} --key {} --host-key {}",
                    quote(&self.self_exe.to_string_lossy()),
                    quote(&endpoint.host),
                    endpoint.port,
                    quote(&endpoint.username),
                    quote(&self.ssh_key_path.to_string_lossy()),
                    quote(&ssh::key_line(host_key)),
                );
                Ok(vec![
                    "-r".into(),
                    format!("sftp:{}@{}:{}", endpoint.username, endpoint.host, path).into(),
                    "-o".into(),
                    format!("sftp.command={command}").into(),
                ])
            }
        }
    }
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn walk_local(root: &Path) -> anyhow::Result<(u64, u64)> {
    let mut bytes = 0;
    let mut files = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if dir == root && e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((0, 0));
            }
            Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                pending.push(entry.path());
            } else if meta.is_file() {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    Ok((bytes, files))
}
