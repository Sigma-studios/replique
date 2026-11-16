//! How fast runs move data: a run's average and peak, a job's over a period.
//!
//! The peak can only be noted while a run goes: the server keeps one progress
//! report per run, the latest. So each report's rate is compared with the
//! highest seen so far (`Store::record_transfer`). Where it comes from:
//!
//! * the phone and the PC report what they really sent (`upload_bytes_per_sec`,
//!   from Android's per-app counters and from systemd's IP accounting);
//! * a mirror's rclone counts what it transferred in `bytes_done`, so two
//!   reports of its copy give a rate;
//! * anything else (a client too old to count) has an average but no peak.

use replique_proto::{Progress, RunKind};

use crate::App;
use crate::db::Run;

/// The transfer rate and byte count one progress report shows, if any.
pub fn sample(run: &Run, p: &Progress) -> (Option<i64>, Option<i64>) {
    if let Some(rate) = p.upload_bytes_per_sec {
        return (Some(rate as i64), p.bytes_uploaded.map(|b| b as i64));
    }
    // The mirror's copy step only: its hash step reports bytes read from disk.
    let copying = |p: &Progress| p.phase.as_deref() == Some("copying");
    if run.kind == RunKind::Mirror && copying(p) {
        let rate = run
            .progress
            .as_ref()
            .filter(|prev| copying(prev))
            .and_then(|prev| {
                let ms = (p.at - prev.at).num_milliseconds();
                (ms > 0 && p.bytes_done >= prev.bytes_done)
                    .then(|| ((p.bytes_done - prev.bytes_done) as i64).saturating_mul(1000) / ms)
            });
        return (rate, Some(p.bytes_done as i64));
    }
    (None, p.bytes_uploaded.map(|b| b as i64))
}

pub struct Transfer {
    pub bytes: u64,
    pub secs: f64,
    pub peak: Option<u64>,
}

impl Transfer {
    /// Bytes moved over the whole run's duration, scanning included: what the
    /// run achieved, not what the link can do (that is the peak).
    pub fn average(&self) -> u64 {
        (self.bytes as f64 / self.secs) as u64
    }
}

/// A finished backup or mirror run's transfer: the bytes it reported sending,
/// else what it added to the repository (a backup) or copied (a mirror).
pub fn of_run(run: &Run) -> Option<Transfer> {
    if !matches!(run.kind, RunKind::Backup | RunKind::Mirror) {
        return None;
    }
    let f = run.finish.as_ref()?;
    let bytes = run
        .sent_bytes
        .filter(|b| *b > 0)
        .map(|b| b as u64)
        .or_else(|| {
            f.summary
                .as_ref()
                .map(|s| s.data_added_packed)
                .filter(|b| *b > 0)
        })
        .or_else(|| f.mirror.as_ref().map(|m| m.bytes_copied).filter(|b| *b > 0))?;
    let secs = match f.summary.as_ref().map(|s| s.total_duration) {
        Some(d) if d > 0.0 => d,
        _ => (run.finished_at? - run.started_at) as f64,
    };
    (secs > 0.0).then(|| Transfer {
        bytes,
        secs,
        peak: run.peak_bps.filter(|p| *p > 0).map(|p| p as u64),
    })
}

pub struct JobTransfer {
    /// All the bytes over all the time, so a long run weighs more than a
    /// short one.
    pub average: u64,
    pub peak: Option<u64>,
    pub runs: usize,
}

/// A job's transfers among its runs finished since `since`.
pub fn of_job(app: &App, job_id: i64, since: i64) -> Option<JobTransfer> {
    let runs = app.store.runs_of_job(job_id, 500).ok()?;
    let transfers: Vec<Transfer> = runs
        .iter()
        .filter(|r| r.finished_at.is_some_and(|t| t >= since))
        .filter_map(of_run)
        .collect();
    if transfers.is_empty() {
        return None;
    }
    let bytes: u64 = transfers.iter().map(|t| t.bytes).sum();
    let secs: f64 = transfers.iter().map(|t| t.secs).sum();
    Some(JobTransfer {
        average: (bytes as f64 / secs) as u64,
        peak: transfers.iter().filter_map(|t| t.peak).max(),
        runs: transfers.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use replique_proto::{BackupSummary, FinishRun, RunStatus};

    fn run(kind: RunKind, progress: Option<Progress>, finish: Option<FinishRun>) -> Run {
        Run {
            id: "r".into(),
            device_id: "d".into(),
            job_id: 1,
            client_run_id: "r".into(),
            kind,
            status: RunStatus::Running,
            started_at: 1_000,
            received_at: 1_000,
            finished_at: finish.as_ref().map(|_| 1_100),
            progress,
            progress_at: None,
            finish,
            finish_raw: None,
            exit_code: None,
            restic_version: None,
            client_version: None,
            peak_bps: None,
            sent_bytes: None,
        }
    }

    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn reported_rates_count_as_they_are() {
        let p = Progress {
            at: at(10),
            bytes_uploaded: Some(5_000),
            upload_bytes_per_sec: Some(700),
            ..Progress::default()
        };
        assert_eq!(
            sample(&run(RunKind::Backup, None, None), &p),
            (Some(700), Some(5_000))
        );
        // A backup without a network count has no rate: what restic read is
        // not what it sent.
        let read_only = Progress {
            at: at(10),
            bytes_done: 9_000,
            ..Progress::default()
        };
        assert_eq!(
            sample(&run(RunKind::Backup, None, None), &read_only),
            (None, None)
        );
    }

    #[test]
    fn a_mirror_rate_comes_from_two_copy_reports() {
        let copying = |t, b| Progress {
            at: at(t),
            bytes_done: b,
            phase: Some("copying".into()),
            ..Progress::default()
        };
        let r = run(RunKind::Mirror, Some(copying(10, 1_000_000)), None);
        assert_eq!(
            sample(&r, &copying(20, 6_000_000)),
            (Some(500_000), Some(6_000_000))
        );
        // Its hash step reads from disk: not a transfer.
        let hashing = Progress {
            at: at(30),
            bytes_done: 90_000_000,
            phase: Some("verifying packs".into()),
            ..Progress::default()
        };
        assert_eq!(sample(&r, &hashing), (None, None));
    }

    #[test]
    fn averages_over_the_whole_run() {
        let mut f = FinishRun {
            status: RunStatus::Success,
            exit_code: 0,
            finished_at: at(1_100),
            summary: Some(BackupSummary {
                data_added_packed: 2_000_000,
                total_duration: 40.0,
                ..BackupSummary::default()
            }),
            prune: None,
            check: None,
            mirror: None,
            errors: vec![],
            error_count: 0,
            unreadable_paths: vec![],
            message: None,
            watched: vec![],
        };
        let mut r = run(RunKind::Backup, None, Some(f.clone()));
        // No network count: what it added to the repository.
        assert_eq!(of_run(&r).unwrap().average(), 50_000);
        // With one, that; and the peak noted on the way.
        r.sent_bytes = Some(4_000_000);
        r.peak_bps = Some(300_000);
        let t = of_run(&r).unwrap();
        assert_eq!((t.average(), t.peak), (100_000, Some(300_000)));
        // Nothing moved: nothing to say.
        f.summary = Some(BackupSummary::default());
        assert!(of_run(&run(RunKind::Backup, None, Some(f))).is_none());
    }
}
