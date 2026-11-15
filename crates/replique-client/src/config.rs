//! The client's configuration: `/etc/replique/client.json`, written by the NixOS
//! module (or by hand on another system), with every job of this device.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use replique_proto::{JobKind, JobRules, JobSpec, Retention};
use serde::{Deserialize, Serialize};

use crate::Error;

pub const DEFAULT_CONFIG: &str = "/etc/replique/client.json";
pub const DEFAULT_STATE_DIR: &str = "/var/lib/replique-client";
pub const DEFAULT_SSH_ALIAS: &str = "replique-storage";

/// How many days a mirror keeps the files that disappeared from its source.
pub const DEFAULT_KEEP_REMOVED_DAYS: u32 = 30;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientConfig {
    /// The server's URL. When set it wins over the one stored at pairing, so the
    /// server can move to another address without pairing again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<PathBuf>,
    /// The SSH host alias written into the state directory's `ssh/config` at
    /// pairing, which restic's SFTP backend and the `replique-<job>` wrappers use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_alias: Option<String>,
    #[serde(default)]
    pub jobs: BTreeMap<String, JobConfig>,
}

/// One job as the module declares it. Field names follow the module's options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobConfig {
    #[serde(default = "backup")]
    pub kind: JobKind,
    /// systemd `OnCalendar`, or absent for a manual job. Only reported: systemd's
    /// timer does the scheduling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_interval_secs: Option<u64>,
    #[serde(default)]
    pub paths: Vec<String>,
    /// restic exclude patterns as written; `$` is turned into `?` before restic
    /// sees them.
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub exclude_caches: bool,
    #[serde(default)]
    pub generated_excludes_for: Vec<String>,
    /// Paths that must be mountpoints, or the run is refused and reported failed:
    /// backing up an empty mount folder would make every file look deleted.
    #[serde(default)]
    pub require_mounts: Vec<String>,
    /// Absent: `keep-deleted` with its default rules for a backup, nothing for a
    /// mirror.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_subset: Option<String>,
    /// Passed to `restic backup` as they are, after the client's own options.
    #[serde(default)]
    pub extra_backup_args: Vec<String>,
    /// restic `--host` for backups and the snapshot filters of retention. Absent:
    /// restic uses the machine's hostname, as the user's earlier jobs did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Any restic repository instead of the paired Storage Box one (a NAS, a
    /// local disk, tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// The restic password file instead of the paired one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_file: Option<PathBuf>,

    // Mirror jobs.
    /// The device id whose repository is copied (from `GET /mirror-sources`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_source: Option<String>,
    /// Where the copy lives on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_path: Option<PathBuf>,
    /// Where files gone from the source are moved; `<mirror_path>.removed` when
    /// absent. It must be outside `mirror_path`, which rclone keeps identical to
    /// the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_path: Option<PathBuf>,
    #[serde(default = "keep_removed_days")]
    pub keep_removed_days: u32,
    /// An rclone path to copy from instead of the twin login the server lists
    /// (a repository already on a local disk or a NAS share, tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_from: Option<String>,
    /// Files on the source younger than this are left for the next run
    /// (rclone's `--min-age`; `15m` when absent, `0` to copy everything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_min_age: Option<String>,
}

fn backup() -> JobKind {
    JobKind::Backup
}

fn keep_removed_days() -> u32 {
    DEFAULT_KEEP_REMOVED_DAYS
}

impl Default for JobConfig {
    fn default() -> Self {
        JobConfig {
            kind: JobKind::Backup,
            schedule: None,
            expected_interval_secs: None,
            paths: vec![],
            exclude: vec![],
            exclude_caches: false,
            generated_excludes_for: vec![],
            require_mounts: vec![],
            retention: None,
            check_subset: None,
            extra_backup_args: vec![],
            host: None,
            repository: None,
            password_file: None,
            mirror_source: None,
            mirror_path: None,
            removed_path: None,
            keep_removed_days: DEFAULT_KEEP_REMOVED_DAYS,
            mirror_from: None,
            mirror_min_age: None,
        }
    }
}

impl JobConfig {
    /// The retention actually applied: a backup without one gets the protocol's
    /// default (`keep-deleted`), a mirror never prunes.
    pub fn effective_retention(&self) -> Option<Retention> {
        match self.kind {
            JobKind::Mirror => None,
            JobKind::Backup => Some(self.retention.clone().unwrap_or_default()),
        }
    }

    /// The declaration sent with `PUT /jobs/{name}`.
    pub fn spec(&self) -> JobSpec {
        JobSpec {
            kind: self.kind,
            schedule: self.schedule.clone(),
            expected_interval_secs: self.expected_interval_secs,
            rules: JobRules {
                paths: self.paths.clone(),
                excludes: self.exclude.clone(),
                exclude_caches: self.exclude_caches,
                generated_excludes_for: self.generated_excludes_for.clone(),
                retention: self.effective_retention(),
                check_subset: self.check_subset.clone(),
                mirror_source: self.mirror_source.clone(),
                mirror_path: self
                    .mirror_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
            },
        }
    }

    pub fn removed_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.removed_path {
            return Some(p.clone());
        }
        let mirror = self.mirror_path.as_ref()?;
        let mut name = mirror.file_name()?.to_os_string();
        name.push(".removed");
        Some(mirror.with_file_name(name))
    }
}

impl ClientConfig {
    /// Reads the configuration. A missing file is an empty configuration when
    /// `required` is false (pairing works before the module is set up).
    pub fn load(path: &Path, required: bool) -> Result<ClientConfig, Error> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| Error::msg(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => {
                Ok(ClientConfig::default())
            }
            Err(e) => Err(Error::msg(format!("{}: {e}", path.display()))),
        }
    }

    pub fn job(&self, name: &str) -> Result<&JobConfig, Error> {
        self.jobs.get(name).ok_or_else(|| {
            let known: Vec<&str> = self.jobs.keys().map(String::as_str).collect();
            Error::msg(format!(
                "no job named {name:?} in the configuration (jobs: {})",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ))
        })
    }

    pub fn ssh_alias(&self) -> &str {
        self.ssh_alias.as_deref().unwrap_or(DEFAULT_SSH_ALIAS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use replique_proto::{KeepCount, KeepRules};

    #[test]
    fn parses_module_output() {
        let text = r#"{
          "server": "https://replique.example",
          "state_dir": "/var/lib/replique-client",
          "jobs": {
            "hetzner": {
              "kind": "backup",
              "schedule": "daily",
              "paths": ["/home/sigma"],
              "exclude": ["/home/sigma/.cache"],
              "exclude_caches": true,
              "retention": { "policy": "versions", "keep": { "daily": 7, "monthly": "unlimited" } },
              "check_subset": "5%"
            },
            "phone": { "kind": "mirror", "mirror_source": "dev_1", "mirror_path": "/b/phone" }
          }
        }"#;
        let c: ClientConfig = serde_json::from_str(text).unwrap();
        let h = c.job("hetzner").unwrap();
        assert_eq!(
            h.effective_retention(),
            Some(Retention::Versions {
                keep: KeepRules {
                    daily: Some(KeepCount::Count(7)),
                    monthly: Some(KeepCount::Unlimited),
                    ..KeepRules::default()
                }
            })
        );
        let p = c.job("phone").unwrap();
        assert_eq!(p.effective_retention(), None);
        assert_eq!(p.removed_path(), Some(PathBuf::from("/b/phone.removed")));
        assert_eq!(p.keep_removed_days, 30);
        assert_eq!(p.spec().rules.mirror_path.as_deref(), Some("/b/phone"));
        assert!(c.job("nope").is_err());
    }

    #[test]
    fn backup_defaults_to_keep_deleted() {
        let j = JobConfig::default();
        assert_eq!(j.effective_retention(), Some(Retention::default()));
    }
}
