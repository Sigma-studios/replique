//! Where a device's repository lives, and how it is reached.

use serde::{Deserialize, Serialize};

/// Storage Box sub-accounts accept SSH commands only on port 23; port 22 is
/// SFTP-only and lacks commands such as `install-ssh-key`. restic and rclone work
/// on either, so everything uses 23 for one less thing to configure.
pub const STORAGE_BOX_SSH_PORT: u16 = 23;

/// The folder holding the restic repository, inside a sub-account's home.
pub const REPO_DIR: &str = "restic";

/// Sub-accounts are chrooted: each one sees its own home as `/home`, so the
/// repository path a device uses is always this, whatever the home is on the box.
pub const DEVICE_REPO_PATH: &str = "/home/restic";

/// The home directory (relative to the box's root) given to a device's
/// sub-account and to its read-only twin.
pub fn sub_account_home(owner_slug: &str, device_slug: &str) -> String {
    format!("replique/{owner_slug}/{device_slug}")
}

/// A lowercase ASCII slug for owner and device names in box paths: letters and
/// digits kept (accents dropped), everything else collapsed to single dashes.
/// Returns `device` for a name with nothing usable, so a path is never empty.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = fold_accent(c).to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.truncate(48);
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() { "device".into() } else { out }
}

/// Folds the accented Latin letters French names use onto plain ASCII.
fn fold_accent(c: char) -> char {
    match c {
        'à' | 'â' | 'ä' | 'á' | 'ã' | 'å' | 'À' | 'Â' | 'Ä' | 'Á' => 'a',
        'ç' | 'Ç' => 'c',
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'e',
        'î' | 'ï' | 'í' | 'ì' | 'Î' | 'Ï' => 'i',
        'ô' | 'ö' | 'ó' | 'ò' | 'õ' | 'Ô' | 'Ö' => 'o',
        'ù' | 'û' | 'ü' | 'ú' | 'Ù' | 'Û' | 'Ü' => 'u',
        'ÿ' | 'ý' => 'y',
        'ñ' => 'n',
        other => other,
    }
}

/// How a device reaches its repository: an SFTP login on the Storage Box.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageInfo {
    /// e.g. `u123456.your-storagebox.de`
    pub host: String,
    /// Always 23 for a Storage Box (see `STORAGE_BOX_SSH_PORT`); another SFTP
    /// server (a NAS, a test server) may use something else.
    pub port: u16,
    /// e.g. `u123456-sub3`
    pub username: String,
    /// The repository path as the sub-account sees it, normally `/home/restic`.
    pub repo_path: String,
    /// `known_hosts` lines for `host` (port included as `[host]:23`), so devices
    /// can check the server's key from the first connection instead of trusting
    /// it blindly. Empty when the server could not scan them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_hosts: Vec<String>,
}

impl StorageInfo {
    /// The restic repository for SFTP through an `ssh` binary. The port is not in
    /// the URL (restic's sftp backend has no place for it): pass it with
    /// `-o sftp.args='-p <port>'` or through an SSH host alias.
    pub fn restic_sftp_repository(&self) -> String {
        format!("sftp:{}@{}:{}", self.username, self.host, self.repo_path)
    }

    /// The restic repository through rclone, for devices without an `ssh` binary
    /// (Android): the rclone remote named `remote` must point at this login.
    pub fn restic_rclone_repository(&self, remote: &str) -> String {
        format!("rclone:{remote}:{}", self.repo_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Téléphone de Sigma"), "telephone-de-sigma");
        assert_eq!(slugify("  PC (NixOS)  "), "pc-nixos");
        assert_eq!(slugify("façade"), "facade");
        assert_eq!(slugify("???"), "device");
        assert_eq!(sub_account_home("sigma", "pc"), "replique/sigma/pc");
    }

    #[test]
    fn repositories() {
        let s = StorageInfo {
            host: "u123456.your-storagebox.de".into(),
            port: 23,
            username: "u123456-sub1".into(),
            repo_path: DEVICE_REPO_PATH.into(),
            known_hosts: vec![],
        };
        assert_eq!(
            s.restic_sftp_repository(),
            "sftp:u123456-sub1@u123456.your-storagebox.de:/home/restic"
        );
        assert_eq!(
            s.restic_rclone_repository("replique"),
            "rclone:replique:/home/restic"
        );
    }
}
