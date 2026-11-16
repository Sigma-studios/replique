//! `GET /device/config`: the phone's rules, edited on the website.

use serde::{Deserialize, Serialize};

use crate::Retention;

/// The phone's backup rules. The reply carries an `ETag`; the app sends it back
/// as `If-None-Match` and gets `304 Not Modified` while nothing changed. Until
/// the admin edits anything, the server returns `DeviceConfig::android_default()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// The job name the app reports its runs under.
    pub job: String,
    /// Folders backed up, each with its own filters.
    #[serde(default)]
    pub folders: Vec<FolderRule>,
    /// restic exclude patterns applied everywhere, on top of each folder's.
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub conditions: Conditions,
    /// Hours between two scheduled runs (WorkManager's minimum is 15 minutes, so
    /// anything from 1 up works). Ignored when `daily_at` is set.
    pub interval_hours: u32,
    /// Run once a day at this time instead, `HH:MM` in the phone's own time
    /// zone (`02:00`: every night). The conditions still apply: a run due while
    /// the phone is off its charger waits until it is plugged in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_at: Option<String>,
    /// The phone always uses `never-prune`; the field is here so the website and
    /// the app agree on it explicitly.
    pub retention: Retention,
    /// Files whose age the app reports after each backup (`FinishRun::watched`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched: Vec<WatchedFiles>,
}

/// Files another app writes now and then, whose newest copy should not get
/// old: Aegis's automatic backups. The app only reports how many there are
/// and when the newest was written, never names or contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchedFiles {
    /// What they are, for the website and the alerts: `Aegis backups`.
    pub name: String,
    /// The folder they are written to (not searched below it).
    pub folder: String,
    /// File names to count: `*` matches any run of characters, `?` one.
    pub pattern: String,
    /// The newest older than this raises a warning.
    pub max_age_days: u32,
}

impl WatchedFiles {
    /// Aegis's automatic backups: `aegis-backup-yyyyMMdd-HHmmss.json`, or
    /// `aegis-backup.json` when Aegis keeps a single file. Aegis writes one
    /// when its vault changes, not on a schedule.
    pub fn aegis() -> WatchedFiles {
        WatchedFiles {
            name: "Aegis backups".into(),
            folder: format!("{ANDROID_SHARED_STORAGE}/Aegis"),
            pattern: "aegis-backup*.json".into(),
            max_age_days: 30,
        }
    }
}

/// Whether a file name matches a `WatchedFiles` pattern (`*` and `?`).
pub fn name_matches(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    // Greedy matching with backtracking to the last `*`.
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while j < n.len() {
        if i < p.len() && (p[i] == '?' || p[i] == n[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some((i, j));
            i += 1;
        } else if let Some((si, sj)) = star {
            i = si + 1;
            j = sj + 1;
            star = Some((si, sj + 1));
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

/// One folder and its filters.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FolderRule {
    /// An absolute path, e.g. `/storage/emulated/0` or `/storage/emulated/0/DCIM`.
    pub path: String,
    /// restic exclude patterns for this folder only.
    #[serde(default)]
    pub excludes: Vec<String>,
    /// Skip files larger than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_size_bytes: Option<u64>,
    /// File extensions (lowercase, no dot) skipped in this folder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_extensions: Vec<String>,
    /// If not empty, only these extensions are backed up in this folder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only_extensions: Vec<String>,
}

/// When the phone may run a scheduled backup. "Back up now" ignores them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conditions {
    /// Only on an unmetered network (Wi-Fi, Ethernet).
    #[serde(default = "yes")]
    pub unmetered_only: bool,
    #[serde(default = "yes")]
    pub charging_only: bool,
    /// Do not start below this battery level (ignored while charging).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_battery_percent: Option<u8>,
}

fn yes() -> bool {
    true
}

impl Default for Conditions {
    fn default() -> Self {
        Conditions {
            unmetered_only: true,
            charging_only: true,
            min_battery_percent: None,
        }
    }
}

/// Shared storage on Android. Everything a user sees as "the phone's files".
pub const ANDROID_SHARED_STORAGE: &str = "/storage/emulated/0";

/// What the phone never backs up: other apps' private folders (unreadable, and
/// regenerated), caches, thumbnails, and the trash and half-written files of the
/// media store. restic treats a pattern without a leading `/` as matching at any
/// depth.
pub const ANDROID_DEFAULT_EXCLUDES: &[&str] = &[
    "/storage/emulated/0/Android/data",
    "/storage/emulated/0/Android/obb",
    ".thumbnails",
    ".trashed-*",
    ".pending-*",
    ".cache",
    "cache",
    "Cache",
];

impl DeviceConfig {
    /// All of shared storage, minus `ANDROID_DEFAULT_EXCLUDES`, daily, on Wi-Fi
    /// while charging, never pruned.
    pub fn android_default() -> DeviceConfig {
        DeviceConfig {
            job: "phone".into(),
            folders: vec![FolderRule {
                path: ANDROID_SHARED_STORAGE.into(),
                ..FolderRule::default()
            }],
            excludes: ANDROID_DEFAULT_EXCLUDES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            conditions: Conditions::default(),
            interval_hours: 24,
            daily_at: None,
            retention: Retention::NeverPrune,
            watched: Vec::new(),
        }
    }
}

/// Reads a `daily_at` time: `HH:MM`, 00:00 to 23:59.
pub fn parse_daily_at(s: &str) -> Option<(u8, u8)> {
    let (h, m) = s.trim().split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m) = (h.parse::<u8>().ok()?, m.parse::<u8>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_times() {
        assert_eq!(parse_daily_at("02:00"), Some((2, 0)));
        assert_eq!(parse_daily_at(" 23:59 "), Some((23, 59)));
        assert_eq!(parse_daily_at("24:00"), None);
        assert_eq!(parse_daily_at("2:00"), None);
        assert_eq!(parse_daily_at("02h00"), None);
    }

    #[test]
    fn patterns() {
        let p = &WatchedFiles::aegis().pattern;
        assert!(name_matches(p, "aegis-backup-20261116-143000.json"));
        assert!(name_matches(p, "aegis-backup.json"));
        assert!(!name_matches(p, "aegis-backup-20261116-143000.json.tmp"));
        assert!(!name_matches(p, "aegis-export.json"));
        assert!(name_matches("a?c*", "abc"));
        assert!(name_matches("*b*b", "abab"));
        assert!(!name_matches("a?c", "ac"));
    }
}
