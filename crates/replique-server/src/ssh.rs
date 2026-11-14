//! SSH and SFTP to the Storage Box, in-process.
//!
//! Why a Rust SSH stack rather than the `ssh` and `sftp` binaries: pairing logs
//! in with a password once (OpenSSH only takes one interactively, or through
//! `sshpass`/`SSH_ASKPASS` tricks), measuring walks a whole repository (parsing
//! `sftp ls` output is fragile), and pairing must hand devices the box's host
//! key (which the handshake gives us here, instead of `ssh-keyscan`). restic
//! still needs an SFTP transport for restores: `replique-server sftp-bridge`
//! (see `bridge`) gives it one through `-o sftp.command`, so neither the Docker
//! image nor a NAS needs OpenSSH installed, only restic.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use parking_lot::Mutex;
use russh::ChannelMsg;
use russh::client::{self, AuthResult};
use russh::keys::ssh_key::{self, LineEnding, PrivateKey, PublicKey};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::secrets::Secret;

/// Who to log in as, where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub username: String,
}

pub enum Auth<'a> {
    Password(&'a Secret),
    Key(&'a PrivateKey),
}

/// Which host keys to accept. Empty means trust on first use: the caller
/// remembers the key seen (`Connection::host_key`) and passes it next time.
#[derive(Debug, Clone, Default)]
pub struct Trust {
    pub keys: Vec<PublicKey>,
}

impl Trust {
    /// Keys from `known_hosts`-style lines (`[host]:port alg base64`), or bare
    /// `alg base64` lines, for this host and port.
    pub fn from_lines<'a>(
        lines: impl IntoIterator<Item = &'a str>,
        host: &str,
        port: u16,
    ) -> Trust {
        let bracketed = format!("[{host}]:{port}");
        let mut keys = Vec::new();
        for line in lines {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let key = match fields.as_slice() {
                [hosts, alg, b64, ..]
                    if !hosts.starts_with("ssh-") && !hosts.starts_with("ecdsa-") =>
                {
                    let matches = hosts
                        .split(',')
                        .any(|h| h == bracketed || (port == 22 && h == host));
                    if !matches {
                        continue;
                    }
                    format!("{alg} {b64}")
                }
                [alg, b64, ..] => format!("{alg} {b64}"),
                _ => continue,
            };
            if let Ok(k) = PublicKey::from_openssh(&key) {
                keys.push(k);
            }
        }
        Trust { keys }
    }
}

struct Handler {
    trust: Trust,
    seen: Arc<Mutex<Option<PublicKey>>>,
}

impl client::Handler for Handler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_key else {
            return Ok(false);
        };
        *self.seen.lock() = Some(key.clone());
        if self.trust.keys.is_empty() {
            return Ok(true);
        }
        let ok = self
            .trust
            .keys
            .iter()
            .any(|k| k.key_data() == key.key_data());
        if !ok {
            tracing::warn!(
                fingerprint = %key.fingerprint(ssh_key::HashAlg::Sha256),
                "the SSH server's host key does not match the one remembered"
            );
        }
        Ok(ok)
    }
}

pub struct Connection {
    handle: client::Handle<Handler>,
    host_key: Option<PublicKey>,
}

/// The key as `alg base64`, without comment.
pub fn key_line(key: &PublicKey) -> String {
    let full = key.to_openssh().unwrap_or_default();
    full.split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn connect(
    endpoint: &Endpoint,
    auth: Auth<'_>,
    trust: Trust,
) -> anyhow::Result<Connection> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(120)),
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let seen = Arc::new(Mutex::new(None));
    let handler = Handler {
        trust,
        seen: seen.clone(),
    };
    let addr = (endpoint.host.as_str(), endpoint.port);
    let mut handle = tokio::time::timeout(
        Duration::from_secs(30),
        client::connect(config, addr, handler),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "connecting to {}:{} timed out",
            endpoint.host,
            endpoint.port
        )
    })?
    .with_context(|| format!("SSH to {}:{}", endpoint.host, endpoint.port))?;
    let result = match auth {
        Auth::Password(password) => {
            handle
                .authenticate_password(&endpoint.username, password.expose())
                .await?
        }
        Auth::Key(key) => {
            let key = PrivateKeyWithHashAlg::new(Arc::new(key.clone()), None);
            handle
                .authenticate_publickey(&endpoint.username, key)
                .await?
        }
    };
    if !matches!(result, AuthResult::Success) {
        bail!(
            "{}@{}:{} refused the login",
            endpoint.username,
            endpoint.host,
            endpoint.port
        );
    }
    let host_key = seen.lock().clone();
    Ok(Connection { handle, host_key })
}

impl Connection {
    /// The key the server presented, to remember for next time and to hand to
    /// devices as their `known_hosts`.
    pub fn host_key(&self) -> Option<&PublicKey> {
        self.host_key.as_ref()
    }

    pub async fn sftp(&self) -> anyhow::Result<SftpSession> {
        let channel = self.handle.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| anyhow!("starting SFTP: {e}"))?;
        Ok(sftp)
    }

    /// Runs a command with `stdin`, returning its exit status and output.
    pub async fn exec(&self, command: &str, stdin: &[u8]) -> anyhow::Result<(u32, String)> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        if !stdin.is_empty() {
            channel.data_bytes(stdin.to_vec()).await?;
        }
        channel.eof().await?;
        let mut output = Vec::new();
        let mut status = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => output.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => output.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        let output = String::from_utf8_lossy(&output).trim().to_string();
        Ok((status.unwrap_or(255), output))
    }

    pub async fn close(self) {
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }
}

/// Hetzner's way of adding a key with the account's password:
/// `install-ssh-key` on port 23 reads keys on stdin and adds them to the
/// account's `authorized_keys` (in both formats its ports want).
pub async fn install_key_with_command(conn: &Connection, keys: &[String]) -> anyhow::Result<()> {
    let input = format!("{}\n", keys.join("\n"));
    let (status, output) = conn.exec("install-ssh-key", input.as_bytes()).await?;
    if status != 0 {
        bail!("install-ssh-key exited with {status}: {output}");
    }
    Ok(())
}

/// Adds `keys` to `.ssh/authorized_keys` in the account's home over SFTP,
/// keeping the keys already there. Merging rather than replacing matters if
/// two sub-accounts share a home (the read-only twin): they may then share
/// this file too, and replacing it would lock the device out.
pub async fn write_authorized_keys(sftp: &SftpSession, keys: &[String]) -> anyhow::Result<()> {
    if !sftp.try_exists(".ssh").await.unwrap_or(false) {
        sftp.create_dir(".ssh")
            .await
            .map_err(|e| anyhow!("creating .ssh: {e}"))?;
    }
    let existing = match sftp.read(".ssh/authorized_keys").await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    };
    let content = merge_keys(&existing, keys);
    let mut file = sftp
        .create(".ssh/authorized_keys")
        .await
        .map_err(|e| anyhow!("writing .ssh/authorized_keys: {e}"))?;
    file.write_all(content.as_bytes()).await?;
    file.shutdown().await?;
    Ok(())
}

/// `existing` plus every key of `keys` not in it yet (compared on algorithm
/// and key, ignoring comments).
pub fn merge_keys(existing: &str, keys: &[String]) -> String {
    let id = |line: &str| {
        line.split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut lines: Vec<String> = existing
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    for key in keys {
        if !lines.iter().any(|l| id(l) == id(key)) {
            lines.push(key.trim().to_string());
        }
    }
    format!("{}\n", lines.join("\n"))
}

/// Total bytes and file count under `root`, following no symlinks.
pub async fn walk(sftp: &SftpSession, root: &str) -> anyhow::Result<(u64, u64)> {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut pending = vec![root.trim_end_matches('/').to_string()];
    while let Some(dir) = pending.pop() {
        let entries = sftp
            .read_dir(dir.clone())
            .await
            .map_err(|e| anyhow!("listing {dir}: {e}"))?;
        for entry in entries {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let meta = entry.metadata();
            match meta.file_type() {
                russh_sftp::protocol::FileType::Dir => pending.push(format!("{dir}/{name}")),
                russh_sftp::protocol::FileType::File => {
                    files += 1;
                    bytes += meta.len();
                }
                _ => {}
            }
        }
    }
    Ok((bytes, files))
}

/// Deletes `path` and everything under it.
pub async fn remove_tree(sftp: &SftpSession, path: &str) -> anyhow::Result<()> {
    let path = path.trim_end_matches('/').to_string();
    let mut dirs = vec![path.clone()];
    let mut order = Vec::new();
    while let Some(dir) = dirs.pop() {
        let entries = match sftp.read_dir(dir.clone()).await {
            Ok(entries) => entries,
            Err(_) if dir == path => return Ok(()),
            Err(e) => return Err(anyhow!("listing {dir}: {e}")),
        };
        for entry in entries {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let child = format!("{dir}/{name}");
            if entry.metadata().file_type().is_dir() {
                dirs.push(child);
            } else {
                sftp.remove_file(child.clone())
                    .await
                    .map_err(|e| anyhow!("removing {child}: {e}"))?;
            }
        }
        order.push(dir);
    }
    for dir in order.into_iter().rev() {
        sftp.remove_dir(dir.clone())
            .await
            .map_err(|e| anyhow!("removing {dir}: {e}"))?;
    }
    Ok(())
}

/// Pipes stdin and stdout to an SFTP subsystem on the server: what restic
/// runs as its `sftp.command`. Returns when either side closes.
pub async fn bridge(conn: Connection) -> anyhow::Result<()> {
    let channel = conn.handle.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let stream = channel.into_stream();
    let (mut from_server, mut to_server) = tokio::io::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let upload = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = stdin.read(&mut buf).await?;
            if n == 0 {
                to_server.shutdown().await?;
                return anyhow::Ok(());
            }
            to_server.write_all(&buf[..n]).await?;
        }
    };
    let download = async {
        tokio::io::copy(&mut from_server, &mut stdout).await?;
        stdout.flush().await?;
        anyhow::Ok(())
    };
    tokio::select! {
        r = upload => r?,
        r = download => r?,
    }
    conn.close().await;
    Ok(())
}

/// Réplique's own key: generated on first start, kept in the state folder,
/// installed on every read-only twin.
pub fn load_or_create_key(path: &std::path::Path) -> anyhow::Result<PrivateKey> {
    if path.exists() {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        return PrivateKey::from_openssh(raw)
            .with_context(|| format!("parsing {}", path.display()));
    }
    let mut seed = zeroize::Zeroizing::new([0u8; 32]);
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut *seed);
    let pair = ssh_key::private::Ed25519Keypair::from_seed(&seed);
    let mut key = PrivateKey::from(pair);
    key.set_comment("replique-server");
    let pem = key.to_openssh(LineEnding::LF)?;
    crate::secrets::write_private(path, pem.as_bytes())?;
    Ok(key)
}

/// Checks a public key line sent by a device and returns it normalised
/// (`alg base64 comment`).
pub fn parse_public_key(line: &str) -> anyhow::Result<String> {
    let key = PublicKey::from_openssh(line.trim()).context("not an OpenSSH public key")?;
    let mut out = key_line(&key);
    if !key.comment().is_empty() {
        out.push(' ');
        out.push_str(key.comment().as_str_lossy());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_reads_known_hosts_lines() {
        let dir = tempfile::tempdir().unwrap();
        let key = load_or_create_key(&dir.path().join("k")).unwrap();
        let line = key_line(key.public_key());
        let t = Trust::from_lines(
            [
                format!("[box.example]:23 {line}").as_str(),
                format!("[other]:23 {line}").as_str(),
            ],
            "box.example",
            23,
        );
        assert_eq!(t.keys.len(), 1);
        assert!(Trust::from_lines([line.as_str()], "x", 23).keys.len() == 1);
        // The same file again loads the same key.
        let again = load_or_create_key(&dir.path().join("k")).unwrap();
        assert_eq!(key_line(again.public_key()), line);
    }

    #[test]
    fn keys_merge() {
        let merged = merge_keys(
            "ssh-ed25519 AAA dev\n",
            &["ssh-ed25519 AAA other".into(), "ssh-ed25519 BBB r".into()],
        );
        assert_eq!(merged, "ssh-ed25519 AAA dev\nssh-ed25519 BBB r\n");
    }

    #[test]
    fn public_keys_are_checked() {
        assert!(parse_public_key("ssh-ed25519 nope").is_err());
        let dir = tempfile::tempdir().unwrap();
        let key = load_or_create_key(&dir.path().join("k")).unwrap();
        let line = key.public_key().to_openssh().unwrap();
        assert_eq!(parse_public_key(&line).unwrap(), line);
    }
}
