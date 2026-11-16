//! Runs: start, progress, finish, and what "safe to delete" means.

use serde::{Deserialize, Serialize};

use crate::Timestamp;

/// What a run did. A backup job's run is followed by its prune and check, each
/// reported as its own run of the same job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Backup,
    Check,
    Prune,
    Mirror,
}

/// `POST /api/v1/runs`, sent when a run starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartRun {
    /// A UUID the device generates for this run. Starting a run again with the
    /// same one returns the same run id instead of creating another: reports
    /// replayed from the spool after an outage are not counted twice.
    pub client_run_id: String,
    /// The job's name, as declared with `PUT /jobs/{name}`.
    pub job: String,
    pub kind: RunKind,
    pub started_at: Timestamp,
    /// `restic version`'s number, e.g. `0.19.1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restic_version: Option<String>,
    /// e.g. `replique-client 0.1.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartRunReply {
    pub run_id: String,
}

/// `POST /api/v1/runs/{id}/progress`: restic's `status` message, at most every
/// 10 seconds. Never spooled: stale progress is useless.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Progress {
    /// When the device read this status.
    pub at: Timestamp,
    /// 0.0 to 1.0.
    #[serde(default)]
    pub percent_done: f64,
    #[serde(default)]
    pub seconds_elapsed: u64,
    /// restic's estimate; absent until it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_remaining: Option<u64>,
    #[serde(default)]
    pub total_files: u64,
    #[serde(default)]
    pub files_done: u64,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub error_count: u64,
    /// Files being read right now.
    #[serde(default)]
    pub current_files: Vec<String>,
    /// A short phase label for runs that are not a restic backup (`scanning`,
    /// `diffing snapshots`, `verifying packs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Bytes this device sent over the network since the run started, where it
    /// can tell (Android counts them per app). restic's `bytes_done` counts what
    /// it read, unchanged files included, so it is no upload measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_uploaded: Option<u64>,
    /// The upload rate over the device's last few seconds, in bytes per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_bytes_per_sec: Option<u64>,
}

/// How a run ended. Devices derive it from restic's exit code with
/// `from_restic_exit`; the server stores the status it receives and only sets
/// `Running` and `Lost` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Server only: started, not finished.
    Running,
    /// Exit 0.
    Success,
    /// Exit 3: the snapshot was made but some files could not be read
    /// (`unreadable_paths` lists them).
    Partial,
    /// Exit 10.
    RepositoryMissing,
    /// Exit 11.
    LockFailed,
    /// Exit 12.
    WrongPassword,
    /// Exit 130 (SIGINT), or the device stopped the run on purpose (Android's
    /// foreground-service time limit). The next run resumes from the parent.
    Interrupted,
    /// Any other exit code.
    Failed,
    /// Server only: no finish arrived within the job's expected duration plus a
    /// margin.
    Lost,
}

impl RunStatus {
    pub fn from_restic_exit(code: i32) -> RunStatus {
        match code {
            0 => RunStatus::Success,
            3 => RunStatus::Partial,
            10 => RunStatus::RepositoryMissing,
            11 => RunStatus::LockFailed,
            12 => RunStatus::WrongPassword,
            130 => RunStatus::Interrupted,
            _ => RunStatus::Failed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Success => "success",
            RunStatus::Partial => "partial",
            RunStatus::RepositoryMissing => "repository_missing",
            RunStatus::LockFailed => "lock_failed",
            RunStatus::WrongPassword => "wrong_password",
            RunStatus::Interrupted => "interrupted",
            RunStatus::Failed => "failed",
            RunStatus::Lost => "lost",
        }
    }

    /// Whether the run is over (everything but `Running`).
    pub fn is_final(self) -> bool {
        self != RunStatus::Running
    }
}

/// `POST /api/v1/runs/{id}/finish`. Spooled when the server is unreachable and
/// replayed in order; the server accepts a repeated identical finish.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinishRun {
    pub status: RunStatus,
    /// The process's exit code (restic's, or rclone's for a mirror's copy step).
    pub exit_code: i32,
    /// When the run ended on the device, not when the report arrived.
    pub finished_at: Timestamp,
    /// restic's `summary` message, for a backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<BackupSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune: Option<PruneSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<MirrorSummary>,
    /// The first 50 errors (`MAX_REPORTED_ERRORS`).
    #[serde(default)]
    pub errors: Vec<RunError>,
    /// How many errors there were in all.
    #[serde(default)]
    pub error_count: u64,
    /// Paths restic could not read (exit 3), from its `error` messages' `item`.
    #[serde(default)]
    pub unreadable_paths: Vec<String>,
    /// A one-line reason when the run failed before restic said anything useful
    /// (`/mnt/windows is not a mountpoint`, `restic not found`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// For a backup: what the app found of each of its `DeviceConfig::watched`
    /// entries when the run ended, whatever restic did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched: Vec<WatchedReport>,
}

/// One `WatchedFiles` entry as found on the device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchedReport {
    /// The entry's `name`.
    pub name: String,
    pub folder_found: bool,
    /// Files in the folder matching the pattern.
    pub files: u32,
    /// When the newest of them was last written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest: Option<Timestamp>,
}

/// One of restic's `error` messages, or one of the client's own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunError {
    pub message: String,
    /// restic's `during` (`scan`, `archival`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub during: Option<String>,
    /// restic's `item`: the path concerned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
}

/// restic's backup `summary` message, field for field.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BackupSummary {
    #[serde(default)]
    pub files_new: u64,
    #[serde(default)]
    pub files_changed: u64,
    #[serde(default)]
    pub files_unmodified: u64,
    #[serde(default)]
    pub dirs_new: u64,
    #[serde(default)]
    pub dirs_changed: u64,
    #[serde(default)]
    pub dirs_unmodified: u64,
    #[serde(default)]
    pub data_blobs: u64,
    #[serde(default)]
    pub tree_blobs: u64,
    /// Bytes added before compression.
    #[serde(default)]
    pub data_added: u64,
    /// Bytes actually added to the repository (after compression).
    #[serde(default)]
    pub data_added_packed: u64,
    #[serde(default)]
    pub total_files_processed: u64,
    #[serde(default)]
    pub total_bytes_processed: u64,
    /// Seconds.
    #[serde(default)]
    pub total_duration: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_start: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_end: Option<Timestamp>,
    /// The full 64-hex-digit id. Absent when restic made no snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PruneSummary {
    /// `never-prune`, `keep-deleted` or `versions`.
    pub policy: String,
    #[serde(default)]
    pub snapshots_before: u64,
    #[serde(default)]
    pub snapshots_removed: u64,
    /// Snapshots `keep-deleted` tagged `replique-keep` in this run because they
    /// held the last copy of a removed file.
    #[serde(default)]
    pub snapshots_tagged_keep: u64,
    /// Bytes freed in the repository, when restic said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_freed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckSummary {
    /// `--read-data-subset`'s value, absent for a structure-only check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_data_subset: Option<String>,
    /// restic check's `num_errors`.
    #[serde(default)]
    pub num_errors: u64,
    /// Snapshot ids present when the check ran. A successful check makes them
    /// "verified on Hetzner" for the safe-to-delete answer.
    #[serde(default)]
    pub snapshot_ids: Vec<String>,
}

/// A mirror run: an rclone copy of another device's repository, then a SHA-256
/// check of every new file against its name. restic names each data, index and
/// snapshot file by the hash of its contents, so corruption shows without the
/// password (the `config` and `keys/` files are exempt).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirrorSummary {
    /// The device id of the repository copied.
    pub source_device_id: String,
    #[serde(default)]
    pub files_copied: u64,
    #[serde(default)]
    pub bytes_copied: u64,
    /// Files gone from the source, moved to the backup folder (kept 30 days).
    #[serde(default)]
    pub files_removed: u64,
    #[serde(default)]
    pub files_verified: u64,
    /// Files of the copy whose content did not match their name, and that a
    /// fresh download could not settle: the copy is broken. These fail the run.
    #[serde(default)]
    pub hash_mismatches: Vec<String>,
    /// Files wrong on the source itself: a fresh download of each still did
    /// not match its name. Usually a pack an interrupted upload left behind
    /// (restic indexes a pack only once its upload completed), which nothing
    /// uses; a passing check of the source's repository finished after they
    /// appeared settles that. They are left out of the copy and do not fail the
    /// run. Empty from older clients.
    #[serde(default)]
    pub source_mismatches: Vec<String>,
    /// The snapshot files present in the copy after this run (their names are
    /// the snapshot ids). The server uses them for "present in the PC mirror".
    #[serde(default)]
    pub snapshot_ids: Vec<String>,
    /// Total size of the local copy, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

/// `GET /api/v1/devices/{id}/safe-to-delete?since=<timestamp>`. A token may only
/// ask about its own device.
///
/// A snapshot is safe when it is verified on Hetzner (a check run of that
/// repository finished successfully after the snapshot was made, and listed it)
/// and present in a mirror (a successful mirror run reported its id). Files the
/// snapshot holds can be deleted from the phone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafeToDelete {
    /// The newest safe snapshot; absent if there is none, or none newer than
    /// `since`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SafeSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafeSnapshot {
    pub snapshot_id: String,
    /// When restic started that backup: a file unchanged since then is in it.
    pub snapshot_time: Timestamp,
    pub verified_at: Timestamp,
    pub mirrored_at: Timestamp,
    /// The mirroring device's name, for the app to show (`PC`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirrored_by: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(RunStatus::from_restic_exit(0), RunStatus::Success);
        assert_eq!(RunStatus::from_restic_exit(3), RunStatus::Partial);
        assert_eq!(
            RunStatus::from_restic_exit(10),
            RunStatus::RepositoryMissing
        );
        assert_eq!(RunStatus::from_restic_exit(11), RunStatus::LockFailed);
        assert_eq!(RunStatus::from_restic_exit(12), RunStatus::WrongPassword);
        assert_eq!(RunStatus::from_restic_exit(130), RunStatus::Interrupted);
        assert_eq!(RunStatus::from_restic_exit(1), RunStatus::Failed);
        assert_eq!(RunStatus::from_restic_exit(-9), RunStatus::Failed);
    }

    #[test]
    fn status_names_match_serde() {
        for s in [
            RunStatus::Running,
            RunStatus::Success,
            RunStatus::Partial,
            RunStatus::RepositoryMissing,
            RunStatus::LockFailed,
            RunStatus::WrongPassword,
            RunStatus::Interrupted,
            RunStatus::Failed,
            RunStatus::Lost,
        ] {
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
        }
    }
}
