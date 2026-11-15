//! Files a mirror finds broken on the source itself, and whether the source's
//! own check has vouched for them since.
//!
//! A mirroring device has no password for the repository it copies, so it
//! cannot tell whether restic uses a file. It can tell that a fresh download is
//! still not what the file's name promises: the file is wrong on the source.
//! Usually that is a pack an interrupted upload left behind. restic indexes a
//! pack only once its upload completed, so nothing uses it, and on a phone that
//! never prunes it stays for good.
//!
//! What settles it is the source's own check. restic 0.19 compares the size of
//! every pack its index lists with the file in the repository, without
//! `--read-data` (checked: a truncated pack in use fails a plain `check` with
//! `num_errors: 1`; an unused truncated one passes). So a check of the source
//! that passed after the files were first seen means none of them is in use.
//!
//! Limitation: a pack in use damaged without a size change passes that check.
//! The source's `--read-data-subset` checks find it over time, and so does the
//! mirror's own hashing: such a file would show here, with that check's errors.

use std::collections::HashMap;

use replique_proto::RunKind;

use crate::App;
use crate::db::Run;

pub struct SourceMismatches {
    pub files: Vec<String>,
    /// The latest first sighting among them: a passing check of the source
    /// finished after this covers every one.
    pub since: i64,
    pub source_device_id: String,
}

/// What the mirror run `run` reports as broken on the source, if anything,
/// with `history` its job's runs, newest first (`Store::runs_of_job`).
///
/// A file's first sighting is the oldest run of the unbroken streak of runs,
/// ending with `run`, that list it. A run in between that does not list it, or
/// says nothing (failed before reporting), ends the streak: that makes the
/// sighting later, never earlier, so the answer errs on the safe side.
pub fn of_run_in(run: &Run, history: &[Run]) -> Option<SourceMismatches> {
    let m = run.finish.as_ref()?.mirror.as_ref()?;
    if m.source_mismatches.is_empty() {
        return None;
    }
    let at = |r: &Run| r.finished_at.unwrap_or(r.started_at);
    let start = history.iter().position(|r| r.id == run.id);
    let older: &[Run] = match start {
        Some(i) => &history[i..],
        None => std::slice::from_ref(run),
    };
    let mut since = i64::MIN;
    for file in &m.source_mismatches {
        let mut first = at(run);
        for r in older.iter().filter(|r| r.kind == RunKind::Mirror) {
            let listed = r
                .finish
                .as_ref()
                .and_then(|f| f.mirror.as_ref())
                .is_some_and(|m| m.source_mismatches.contains(file));
            if !listed {
                break;
            }
            first = at(r);
        }
        since = since.max(first);
    }
    Some(SourceMismatches {
        files: m.source_mismatches.clone(),
        since,
        source_device_id: m.source_device_id.clone(),
    })
}

/// `of_run_in`, reading the job's history.
pub fn of_run(app: &App, run: &Run) -> anyhow::Result<Option<SourceMismatches>> {
    let history = app.store.runs_of_job(run.job_id, 500)?;
    Ok(of_run_in(run, &history))
}

/// Job histories read once for many runs (`safest_snapshot` weighs up to 200
/// mirror runs, mostly of one job).
#[derive(Default)]
pub struct Histories(HashMap<i64, Vec<Run>>);

impl Histories {
    pub fn of_run(&mut self, app: &App, run: &Run) -> anyhow::Result<Option<SourceMismatches>> {
        let history = match self.0.entry(run.job_id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(app.store.runs_of_job(run.job_id, 500)?)
            }
        };
        Ok(of_run_in(run, history))
    }
}

/// When a check of the source's repository passed (0 errors) after `since`,
/// the earliest such time.
pub fn vouched_at(app: &App, source_device_id: &str, since: i64) -> anyhow::Result<Option<i64>> {
    let checks = app
        .store
        .successful_runs(source_device_id, RunKind::Check)?;
    Ok(checks
        .iter()
        .filter(|c| {
            c.finish
                .as_ref()
                .and_then(|f| f.check.as_ref())
                .is_some_and(|ch| ch.num_errors == 0)
        })
        .filter_map(|c| c.finished_at)
        .filter(|t| *t > since)
        .min())
}

/// How long the source has to run a passing check once such files appear
/// before it becomes an integrity alert: its jobs' interval with the silent-job
/// margin (a daily phone: 26 h), a day and two hours when unknown.
pub fn grace(app: &App, source_device_id: &str) -> i64 {
    let interval = app
        .store
        .jobs_of(source_device_id)
        .unwrap_or_default()
        .iter()
        .filter_map(crate::alerts::expected_interval)
        .max()
        .unwrap_or(86_400);
    crate::alerts::silent_after(interval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use replique_proto::{FinishRun, MirrorSummary, RunStatus};

    fn mirror_run(id: &str, at: i64, broken: &[&str]) -> Run {
        Run {
            id: id.into(),
            device_id: "dev_PC".into(),
            job_id: 1,
            client_run_id: id.into(),
            kind: RunKind::Mirror,
            status: RunStatus::Success,
            started_at: at - 60,
            received_at: at - 60,
            finished_at: Some(at),
            progress: None,
            progress_at: None,
            finish: Some(FinishRun {
                status: RunStatus::Success,
                exit_code: 0,
                finished_at: chrono::DateTime::from_timestamp(at, 0).unwrap(),
                summary: None,
                prune: None,
                check: None,
                mirror: Some(MirrorSummary {
                    source_device_id: "dev_PHONE".into(),
                    files_copied: 0,
                    bytes_copied: 0,
                    files_removed: 0,
                    files_verified: 0,
                    hash_mismatches: vec![],
                    source_mismatches: broken.iter().map(|s| s.to_string()).collect(),
                    snapshot_ids: vec![],
                    total_bytes: None,
                }),
                errors: vec![],
                error_count: 0,
                unreadable_paths: vec![],
                message: None,
            }),
            finish_raw: None,
            exit_code: Some(0),
            restic_version: None,
            client_version: None,
            peak_bps: None,
            sent_bytes: None,
        }
    }

    #[test]
    fn first_sightings() {
        // Newest first: "a" listed since run 1, "b" only since run 3.
        let history = vec![
            mirror_run("r4", 4000, &["a", "b"]),
            mirror_run("r3", 3000, &["a", "b"]),
            mirror_run("r2", 2000, &["a"]),
            mirror_run("r1", 1000, &["a"]),
            mirror_run("r0", 500, &[]),
        ];
        let s = of_run_in(&history[0], &history).unwrap();
        assert_eq!(s.files, ["a", "b"]);
        // The latest first sighting: b's, run 3.
        assert_eq!(s.since, 3000);
        // As of run 2, only "a", seen since run 1.
        assert_eq!(of_run_in(&history[2], &history).unwrap().since, 1000);
        // A clean run reports nothing.
        assert!(of_run_in(&history[4], &history).is_none());
    }

    #[test]
    fn a_gap_restarts_the_streak() {
        let history = vec![
            mirror_run("r3", 3000, &["a"]),
            mirror_run("r2", 2000, &[]),
            mirror_run("r1", 1000, &["a"]),
        ];
        assert_eq!(of_run_in(&history[0], &history).unwrap().since, 3000);
    }
}
