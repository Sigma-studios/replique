//! The state directory (`/var/lib/replique-client`): what pairing left (device
//! id, report token, restic password, SSH key) and what runs leave (spool, last
//! runs, the declared job specs).
//!
//! Two layouts. Set up by the NixOS module, the directory belongs to the
//! `replique` system user and group with the setgid bit, and every job user is in
//! that group: files are group-readable (secrets) or group-writable (spool,
//! status), owned by `replique`. That owner matters: OpenSSH refuses a private key
//! that is readable by others *and owned by the user running ssh*, so a key owned
//! by root (or by the job user) and shared through the group would break either
//! root or the job user. Anywhere else (a plain directory, tests), files are
//! private to whoever runs the client.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use replique_proto::{StorageInfo, Timestamp};
use serde::{Deserialize, Serialize};

use crate::Error;

/// What pairing stored, minus the secrets (each in its own file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub server: String,
    pub device_id: String,
    pub device_name: String,
    pub storage: StorageInfo,
    pub paired_at: Timestamp,
    /// Paired with `--adopt`: the key and password were the user's already.
    #[serde(default)]
    pub adopted: bool,
}

#[derive(Debug, Clone)]
pub struct State {
    pub dir: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The token, the restic password, the private key: never written to by runs.
    Secret,
    /// Anything a job user may need to rewrite.
    Shared,
    /// Readable by anyone (public key, known_hosts, ssh config).
    Public,
}

impl State {
    pub fn new(dir: impl Into<PathBuf>) -> State {
        State { dir: dir.into() }
    }

    pub fn device_file(&self) -> PathBuf {
        self.dir.join("device.json")
    }
    pub fn token_file(&self) -> PathBuf {
        self.dir.join("token")
    }
    pub fn password_file(&self) -> PathBuf {
        self.dir.join("restic-password")
    }
    pub fn ssh_dir(&self) -> PathBuf {
        self.dir.join("ssh")
    }
    pub fn key_file(&self) -> PathBuf {
        self.ssh_dir().join("id_ed25519")
    }
    pub fn known_hosts_file(&self) -> PathBuf {
        self.ssh_dir().join("known_hosts")
    }
    pub fn ssh_config_file(&self) -> PathBuf {
        self.ssh_dir().join("config")
    }
    pub fn spool_dir(&self) -> PathBuf {
        self.dir.join("spool")
    }
    pub fn runs_dir(&self) -> PathBuf {
        self.dir.join("runs")
    }
    pub fn declared_dir(&self) -> PathBuf {
        self.dir.join("declared")
    }
    pub fn excludes_dir(&self) -> PathBuf {
        self.dir.join("excludes")
    }
    pub fn mirror_dir(&self) -> PathBuf {
        self.dir.join("mirror")
    }

    pub fn load_device(&self) -> Result<Option<Device>, Error> {
        match fs::read_to_string(self.device_file()) {
            Ok(t) => Ok(Some(serde_json::from_str(&t).map_err(|e| {
                Error::msg(format!("{}: {e}", self.device_file().display()))
            })?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::msg(format!("{}: {e}", self.device_file().display()))),
        }
    }

    pub fn token(&self) -> Result<Option<String>, Error> {
        match fs::read_to_string(self.token_file()) {
            Ok(t) => Ok(Some(t.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::msg(format!(
                "{}: {e} (is this user in the replique group?)",
                self.token_file().display()
            ))),
        }
    }

    /// The module's layout: the directory carries the setgid bit.
    pub fn shared(&self) -> bool {
        fs::metadata(&self.dir)
            .map(|m| m.permissions().mode() & 0o2000 != 0)
            .unwrap_or(false)
    }

    fn mode(&self, kind: Kind) -> u32 {
        match (kind, self.shared()) {
            (Kind::Secret, true) => 0o640,
            (Kind::Secret, false) => 0o600,
            (Kind::Shared, true) => 0o660,
            (Kind::Shared, false) => 0o600,
            (Kind::Public, true) => 0o664,
            (Kind::Public, false) => 0o644,
        }
    }

    /// Creates the state directory itself if needed (outside the module's
    /// layout, where tmpfiles already made it).
    pub fn create(&self) -> Result<(), Error> {
        if !self.dir.exists() {
            fs::create_dir_all(&self.dir)
                .map_err(|e| Error::msg(format!("{}: {e}", self.dir.display())))?;
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Creates a subdirectory, group-writable and setgid in the shared layout.
    pub fn ensure_dir(&self, path: &Path) -> Result<(), Error> {
        if path.is_dir() {
            return Ok(());
        }
        fs::create_dir_all(path).map_err(|e| Error::msg(format!("{}: {e}", path.display())))?;
        let mode = if self.shared() { 0o2770 } else { 0o700 };
        // The umask applies to create_dir_all; set the mode explicitly.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
        self.adopt_owner(path);
        Ok(())
    }

    /// Writes a file atomically (temporary file, then rename) with the mode its
    /// kind needs.
    pub fn write(&self, path: &Path, contents: &[u8], kind: Kind) -> Result<(), Error> {
        if let Some(parent) = path.parent() {
            self.ensure_dir(parent)?;
        }
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        let mode = self.mode(kind);
        let result = (|| -> std::io::Result<()> {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(contents)?;
            f.sync_all()?;
            // After writing, so a secret is never briefly readable by others,
            // and explicitly, because the umask applies to `mode` above.
            fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(Error::msg(format!("{}: {e}", path.display())));
        }
        self.adopt_owner(&tmp);
        fs::rename(&tmp, path).map_err(|e| Error::msg(format!("{}: {e}", path.display())))?;
        Ok(())
    }

    /// Gives a file the state directory's owner and group. Only root can change
    /// the owner; for anyone else this quietly does what it can (the group comes
    /// from the setgid directory anyway).
    pub fn adopt_owner(&self, path: &Path) {
        let (Ok(dir), Ok(file)) = (fs::metadata(&self.dir), fs::symlink_metadata(path)) else {
            return;
        };
        if dir.uid() != file.uid() || dir.gid() != file.gid() {
            let _ = std::os::unix::fs::chown(path, Some(dir.uid()), Some(dir.gid()));
        }
    }

    /// Opens (creating) a file for a job user to rewrite in place, e.g. a spool
    /// entry held locked while its run goes on.
    pub fn open_shared(&self, path: &Path) -> Result<File, Error> {
        if let Some(parent) = path.parent() {
            self.ensure_dir(parent)?;
        }
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|e| Error::msg(format!("{}: {e}", path.display())))?;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(self.mode(Kind::Shared)));
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_layout() {
        let d = tempfile::tempdir().unwrap();
        let s = State::new(d.path().join("state"));
        s.create().unwrap();
        s.write(&s.token_file(), b"rpl_x", Kind::Secret).unwrap();
        s.write(&s.known_hosts_file(), b"", Kind::Public).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&s.dir), 0o700);
        assert_eq!(mode(&s.token_file()), 0o600);
        assert_eq!(mode(&s.known_hosts_file()), 0o644);
        assert_eq!(mode(&s.ssh_dir()), 0o700);
        assert_eq!(s.token().unwrap().as_deref(), Some("rpl_x"));
    }

    #[test]
    fn shared_layout() {
        let d = tempfile::tempdir().unwrap();
        let s = State::new(d.path());
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o2770)).unwrap();
        assert!(s.shared());
        s.write(&s.token_file(), b"rpl_x", Kind::Secret).unwrap();
        s.write(&s.runs_dir().join("a.json"), b"{}", Kind::Shared)
            .unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&s.token_file()), 0o640);
        assert_eq!(mode(&s.runs_dir()), 0o2770);
        assert_eq!(mode(&s.runs_dir().join("a.json")), 0o660);
    }
}
