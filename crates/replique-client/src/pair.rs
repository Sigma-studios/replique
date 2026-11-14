//! `replique-client pair`: joins this device to a Réplique server with the
//! one-time code from the website, and sets up what its jobs need to reach the
//! device's Storage Box sub-account.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine;
use chrono::Utc;
use replique_proto::{DeviceKind, PairRequest, PairingLink, is_well_formed_token};

use crate::api::{Api, ApiError};
use crate::config::{ClientConfig, JobConfig};
use crate::restic::Restic;
use crate::state::{Device, Kind, State};
use crate::{Error, client_version, log};

pub struct PairOptions {
    pub server: String,
    pub code: String,
    /// `--adopt <key-file> <password-file>`: keep an existing SSH key and restic
    /// password (a repository that already exists) instead of generating them.
    pub adopt: Option<(PathBuf, PathBuf)>,
    pub name: Option<String>,
    /// Skip the "type saved" confirmation (the password is still printed).
    pub yes: bool,
    /// Initialise the repository right away if it does not exist.
    pub init: bool,
    pub force: bool,
}

impl PairOptions {
    /// Takes `<server-url> <code>` or the website's `replique://pair?…` link.
    pub fn target(first: &str, second: Option<&str>) -> Result<(String, String), Error> {
        if let Some(link) = PairingLink::parse(first) {
            return Ok((link.server, link.code));
        }
        let code = second.ok_or_else(|| {
            Error::msg("give the server's URL and the pairing code, or the replique://pair link")
        })?;
        Ok((first.trim_end_matches('/').to_string(), code.to_string()))
    }
}

pub fn pair(config: &ClientConfig, state: &State, opts: &PairOptions) -> Result<(), Error> {
    state.create()?;
    if let Some(d) = state.load_device()?
        && !opts.force
    {
        return Err(Error::msg(format!(
            "already paired as {} ({}) with {}; pass --force to pair again",
            d.device_name, d.device_id, d.server
        )));
    }
    state.ensure_dir(&state.ssh_dir())?;

    let key = state.key_file();
    let public_key = match &opts.adopt {
        Some((existing, _)) => {
            let bytes = fs::read(existing)
                .map_err(|e| Error::msg(format!("{}: {e}", existing.display())))?;
            state.write(&key, &bytes, Kind::Secret)?;
            public_key_of(&key)?
        }
        None if key.exists() => {
            log!("reusing the SSH key of an earlier, unfinished pairing");
            public_key_of(&key)?
        }
        None => generate_key(&key, &hostname())?,
    };
    state.write(
        &key.with_extension("pub"),
        format!("{public_key}\n").as_bytes(),
        Kind::Public,
    )?;
    // ssh-keygen made the key private to whoever ran it; give it the state
    // directory's mode and owner like every other secret.
    let bytes = fs::read(&key)?;
    state.write(&key, &bytes, Kind::Secret)?;

    let password_file = state.password_file();
    match &opts.adopt {
        Some((_, existing)) => {
            let bytes = fs::read(existing)
                .map_err(|e| Error::msg(format!("{}: {e}", existing.display())))?;
            if String::from_utf8_lossy(&bytes).trim().is_empty() {
                return Err(Error::msg(format!("{} is empty", existing.display())));
            }
            state.write(&password_file, &bytes, Kind::Secret)?;
        }
        None => {
            // Kept from an unfinished pairing: it may already be in the password
            // manager, and it is shown again below either way.
            let password = match fs::read_to_string(&password_file) {
                Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
                _ => {
                    let p = generate_password()?;
                    state.write(&password_file, format!("{p}\n").as_bytes(), Kind::Secret)?;
                    p
                }
            };
            show_password(&password, opts.yes)?;
        }
    }

    let request = PairRequest {
        code: opts.code.clone(),
        device_name: opts.name.clone().unwrap_or_else(hostname),
        kind: if Path::new("/etc/NIXOS").exists() {
            DeviceKind::Nixos
        } else {
            DeviceKind::Other
        },
        ssh_public_key: public_key,
        client_version: Some(client_version()),
    };
    let api = Api::new(&opts.server, None);
    let reply = api.pair(&request).map_err(|e| match e {
        ApiError::Rejected { message, .. } => Error::msg(format!("pairing refused: {message}")),
        other => Error::msg(format!("pairing failed, try again: {other}")),
    })?;
    if !is_well_formed_token(&reply.report_token) {
        return Err(Error::msg("the server sent a malformed report token"));
    }

    state.write(
        &state.token_file(),
        format!("{}\n", reply.report_token).as_bytes(),
        Kind::Secret,
    )?;
    let mut known = reply.storage.known_hosts.join("\n");
    if !known.is_empty() {
        known.push('\n');
    }
    state.write(&state.known_hosts_file(), known.as_bytes(), Kind::Shared)?;
    let device = Device {
        server: opts.server.clone(),
        device_id: reply.device_id.clone(),
        device_name: reply.device_name.clone(),
        storage: reply.storage.clone(),
        paired_at: Utc::now(),
        adopted: opts.adopt.is_some(),
    };
    state.write(
        &state.device_file(),
        &serde_json::to_vec_pretty(&device)?,
        Kind::Public,
    )?;
    write_ssh_config(config, state, &device)?;
    println!(
        "Paired as {} ({}). Repository: {} on {}:{}.",
        device.device_name,
        device.device_id,
        device.storage.repo_path,
        device.storage.host,
        device.storage.port
    );
    if reply.storage.known_hosts.is_empty() {
        log!("the server sent no host key: the first connection will trust the Storage Box's key");
    }

    if opts.init {
        let restic = Restic::for_job(config, state, &JobConfig::default())?;
        match restic.ensure_repository() {
            Ok(true) => println!("Created the restic repository."),
            Ok(false) => println!("The restic repository is there and opens with this password."),
            Err(o) => log!(
                "could not open or create the repository now (the first run will try again): {}",
                o.message.unwrap_or_default()
            ),
        }
    }
    Ok(())
}

/// `ssh/config` in the state directory: the Storage Box login under the alias
/// restic's `sftp.command` uses (with `ssh -F`). `ssh/include.conf`, the same
/// written by root, is what the system's `ssh_config` includes, so `sftp
/// <alias>` works by hand too.
pub fn ssh_config_text(config: &ClientConfig, state: &State, device: &Device) -> String {
    let s = &device.storage;
    let strict = if s.known_hosts.is_empty() {
        "accept-new"
    } else {
        "yes"
    };
    format!(
        "# Written by replique-client: this device's Storage Box login.\n\
         Host {alias}\n\
         \x20   HostName {host}\n\
         \x20   Port {port}\n\
         \x20   User {user}\n\
         \x20   IdentityFile {key}\n\
         \x20   IdentitiesOnly yes\n\
         \x20   UserKnownHostsFile {known}\n\
         \x20   StrictHostKeyChecking {strict}\n\
         \x20   ServerAliveInterval 60\n\
         \x20   ServerAliveCountMax 10\n",
        alias = config.ssh_alias(),
        host = s.host,
        port = s.port,
        user = s.username,
        key = ssh_quote(&state.key_file()),
        known = ssh_quote(&state.known_hosts_file()),
    )
}

fn ssh_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s.into_owned()
    }
}

fn write_ssh_config(config: &ClientConfig, state: &State, device: &Device) -> Result<(), Error> {
    let text = ssh_config_text(config, state, device);
    state.write(&state.ssh_config_file(), text.as_bytes(), Kind::Public)?;
    // Written only when running as root (pairing is), skipped otherwise.
    state.write(&state.ssh_include_file(), text.as_bytes(), Kind::System)
}

/// Rewrites the SSH config if the alias or the state directory moved since
/// pairing. Failing to is not fatal: the old file still works for its alias.
pub fn ensure_ssh_config(config: &ClientConfig, state: &State) {
    let Ok(Some(device)) = state.load_device() else {
        return;
    };
    let text = ssh_config_text(config, state, &device);
    if fs::read_to_string(state.ssh_config_file()).ok().as_deref() != Some(&text)
        && let Err(e) = state.write(&state.ssh_config_file(), text.as_bytes(), Kind::Public)
    {
        log!("could not update the SSH config: {e}");
    }
    if fs::read_to_string(state.ssh_include_file()).ok().as_deref() != Some(&text) {
        let _ = state.write(&state.ssh_include_file(), text.as_bytes(), Kind::System);
    }
}

fn generate_key(path: &Path, host: &str) -> Result<String, Error> {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C"])
        .arg(format!("replique-client@{host}"))
        .arg("-f")
        .arg(path)
        .status()
        .map_err(|e| Error::msg(format!("cannot run ssh-keygen: {e}")))?;
    if !status.success() {
        return Err(Error::msg("ssh-keygen failed"));
    }
    // ssh-keygen wrote its own .pub; ours replaces it with the same line.
    public_key_of(path)
}

fn public_key_of(key: &Path) -> Result<String, Error> {
    let out = Command::new("ssh-keygen")
        .arg("-y")
        .arg("-f")
        .arg(key)
        .output()
        .map_err(|e| Error::msg(format!("cannot run ssh-keygen: {e}")))?;
    if !out.status.success() {
        return Err(Error::msg(format!(
            "{}: not a usable private key without a passphrase ({})",
            key.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 32 random bytes, base64url: 43 characters, 256 bits.
fn generate_password() -> Result<String, Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| Error::msg(format!("no randomness: {e}")))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn show_password(password: &str, yes: bool) -> Result<(), Error> {
    let mut out = std::io::stdout();
    writeln!(
        out,
        "\nThe restic password of this device's backups:\n\n    {password}\n\n\
         Save it in your password manager now. Without it nobody, Réplique included,\n\
         can restore these backups if this machine is lost. It is also kept in the\n\
         state directory for the jobs, readable only by them.\n"
    )?;
    if yes {
        return Ok(());
    }
    let stdin = std::io::stdin();
    loop {
        write!(out, "Type \"saved\" once it is in your password manager: ")?;
        out.flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Err(Error::msg(
                "stopped before the password was confirmed; run pair again (it shows the same password)",
            ));
        }
        if line.trim().eq_ignore_ascii_case("saved") {
            return Ok(());
        }
    }
}

pub fn hostname() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(h) = fs::read_to_string("/proc/sys/kernel/hostname") {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "device".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            PairOptions::target(
                "replique://pair?server=https%3A%2F%2Fr.example&code=K7QM-2XPA",
                None
            )
            .unwrap(),
            ("https://r.example".into(), "K7QM-2XPA".into())
        );
        assert_eq!(
            PairOptions::target("https://r.example/", Some("K7QM-2XPA")).unwrap(),
            ("https://r.example".into(), "K7QM-2XPA".into())
        );
        assert!(PairOptions::target("https://r.example", None).is_err());
    }

    #[test]
    fn passwords() {
        let p = generate_password().unwrap();
        assert_eq!(p.len(), 43);
        assert_ne!(p, generate_password().unwrap());
    }
}
