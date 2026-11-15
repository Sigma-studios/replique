//! Jobs as devices declare them: `PUT /jobs/{name}` and the retention policies.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::StorageInfo;

/// What a job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    /// restic backup of local paths into the device's own repository.
    Backup,
    /// An encrypted byte copy of another device's repository, pulled through its
    /// read-only twin (the PC copying the phone's repository).
    Mirror,
}

/// `PUT /api/v1/jobs/{name}`: a device declares one of its jobs, at startup and
/// whenever its configuration changes. The server stores the latest declaration;
/// it is what the website shows and what the silent-job alert is derived from.
/// Declaring the same spec again is harmless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub kind: JobKind,
    /// A systemd `OnCalendar` expression (`daily`, `*-*-* 03:00`), `every 6h` for
    /// interval-based schedulers such as Android's WorkManager, or absent for a
    /// job only run by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// How long between two runs is normal. The server derives the silent-job
    /// alert from it (plus a margin; `daily` becomes 26 h). Absent for manual jobs
    /// or when the device leaves it to the server to work out from `schedule`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_interval_secs: Option<u64>,
    pub rules: JobRules,
}

/// What a job covers and keeps, shown on the website as declared.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct JobRules {
    /// Absolute paths backed up (empty for a mirror).
    #[serde(default)]
    pub paths: Vec<String>,
    /// restic exclude patterns, as written in the configuration (before the
    /// `$` → `?` escaping the client applies).
    #[serde(default)]
    pub excludes: Vec<String>,
    /// restic `--exclude-caches`: skip folders holding a `CACHEDIR.TAG`.
    #[serde(default)]
    pub exclude_caches: bool,
    /// Roots scanned before each run for build output (Unity, Unreal, .NET,
    /// Godot) to exclude.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated_excludes_for: Vec<String>,
    /// Absent for a mirror, which never prunes (its copy follows the source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
    /// restic `check --read-data-subset` after each backup (`5%`, `1/10`, `2G`);
    /// absent for no check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_subset: Option<String>,
    /// For a mirror: the device id whose repository it copies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_source: Option<String>,
    /// For a mirror: where the copy lives on this device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_path: Option<String>,
}

/// How a job's repository forgets old snapshots. restic prunes whole snapshots,
/// not files: a file's data goes once no kept snapshot holds it, so the policy
/// decides what can still be lost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "kebab-case")]
pub enum Retention {
    /// No `forget` at all: nothing is ever lost. The phone uses it, which is what
    /// makes "free up space" safe.
    NeverPrune,
    /// Before each prune, every snapshot about to go is diffed against the next
    /// kept one; one holding the last copy of a removed file is tagged
    /// `replique-keep` and kept. Only old versions of files that still exist are
    /// lost. The default for computers.
    KeepDeleted {
        #[serde(default = "KeepRules::default_versions")]
        keep: KeepRules,
    },
    /// Plain `forget --prune` with these rules: deleted files go once their last
    /// snapshot ages out. For disposable data.
    Versions { keep: KeepRules },
}

/// The tag `keep-deleted` puts on snapshots that hold the last copy of a file.
pub const KEEP_TAG: &str = "replique-keep";

impl Default for Retention {
    fn default() -> Self {
        Retention::KeepDeleted {
            keep: KeepRules::default_versions(),
        }
    }
}

impl Retention {
    pub fn policy_name(&self) -> &'static str {
        match self {
            Retention::NeverPrune => "never-prune",
            Retention::KeepDeleted { .. } => "keep-deleted",
            Retention::Versions { .. } => "versions",
        }
    }
}

/// restic's `--keep-*` options. Absent means the option is not passed.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct KeepRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hourly: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yearly: Option<KeepCount>,
    /// restic `--keep-within`, a duration such as `14d` or `1y2m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<String>,
}

impl KeepRules {
    /// `--keep-daily 7 --keep-weekly 4 --keep-monthly 12`, the default version
    /// retention of `keep-deleted`.
    pub fn default_versions() -> KeepRules {
        KeepRules {
            daily: Some(KeepCount::Count(7)),
            weekly: Some(KeepCount::Count(4)),
            monthly: Some(KeepCount::Count(12)),
            ..KeepRules::default()
        }
    }

    /// The restic arguments, e.g. `["--keep-daily", "7", "--keep-monthly", "unlimited"]`.
    pub fn to_restic_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        let counts = [
            ("--keep-last", &self.last),
            ("--keep-hourly", &self.hourly),
            ("--keep-daily", &self.daily),
            ("--keep-weekly", &self.weekly),
            ("--keep-monthly", &self.monthly),
            ("--keep-yearly", &self.yearly),
        ];
        for (flag, count) in counts {
            if let Some(count) = count {
                args.push(flag.to_string());
                args.push(count.to_restic_arg());
            }
        }
        if let Some(within) = &self.within {
            args.push("--keep-within".into());
            args.push(within.clone());
        }
        args
    }
}

/// A `--keep-*` count: a number, or `"unlimited"` in JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepCount {
    Count(u32),
    Unlimited,
}

impl KeepCount {
    /// restic takes `unlimited` (since 0.17) as well as `-1`; the word reads
    /// better in the commands the website shows.
    pub fn to_restic_arg(self) -> String {
        match self {
            KeepCount::Count(n) => n.to_string(),
            KeepCount::Unlimited => "unlimited".into(),
        }
    }
}

impl Serialize for KeepCount {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            KeepCount::Count(n) => s.serialize_u32(*n),
            KeepCount::Unlimited => s.serialize_str("unlimited"),
        }
    }
}

impl<'de> Deserialize<'de> for KeepCount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Count(u32),
            Word(String),
        }
        match Raw::deserialize(d)? {
            Raw::Count(n) => Ok(KeepCount::Count(n)),
            Raw::Word(w) if w == "unlimited" => Ok(KeepCount::Unlimited),
            Raw::Word(w) => Err(serde::de::Error::custom(format!(
                "expected a count or \"unlimited\", got {w:?}"
            ))),
        }
    }
}

/// `GET /api/v1/mirror-sources`: the repositories this device may mirror. When
/// the admin assigns a mirror on the website, the server installs this device's
/// SSH key on the source's read-only twin; the login here is that twin's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirrorSources {
    #[serde(default)]
    pub sources: Vec<MirrorSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirrorSource {
    pub device_id: String,
    pub device_name: String,
    /// The read-only twin's login. It cannot write, so restic commands through it
    /// need `--no-lock`, and rclone must not try to delete anything there. Its
    /// home is the repository folder, so `repo_path` is `/home`, and the folder
    /// holds the twin's own `.ssh/`, which a copy should skip.
    pub storage: StorageInfo,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_args() {
        let k = KeepRules {
            daily: Some(KeepCount::Count(7)),
            weekly: Some(KeepCount::Count(4)),
            monthly: Some(KeepCount::Unlimited),
            ..KeepRules::default()
        };
        assert_eq!(
            k.to_restic_args(),
            [
                "--keep-daily",
                "7",
                "--keep-weekly",
                "4",
                "--keep-monthly",
                "unlimited"
            ]
        );
    }

    #[test]
    fn retention_default_keep() {
        let r: Retention = serde_json::from_str(r#"{"policy":"keep-deleted"}"#).unwrap();
        assert_eq!(r, Retention::default());
    }
}
