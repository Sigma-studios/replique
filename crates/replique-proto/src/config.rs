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
    /// anything from 1 up works).
    pub interval_hours: u32,
    /// The phone always uses `never-prune`; the field is here so the website and
    /// the app agree on it explicitly.
    pub retention: Retention,
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
            retention: Retention::NeverPrune,
        }
    }
}
