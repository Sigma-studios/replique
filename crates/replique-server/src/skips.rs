//! Scheduled runs that came due on a device but did not start (`POST
//! /jobs/{name}/skipped`): the phone off its charger, its battery too low,
//! Android refusing. Nothing ran, so there is no run to show; the latest skip
//! of each job is kept to say why the job is late, until a run starts after it.

use replique_proto::SkippedRun;

use crate::App;

fn key(job_id: i64) -> String {
    format!("job_skip:{job_id}")
}

pub fn record(app: &App, job_id: i64, skip: &SkippedRun) -> anyhow::Result<()> {
    app.store
        .put_setting(&key(job_id), serde_json::to_string(skip)?.as_bytes())
}

/// The job's latest skip, if no run started since.
pub fn current(app: &App, job_id: i64) -> Option<SkippedRun> {
    let skip: SkippedRun =
        serde_json::from_str(&app.store.setting_str(&key(job_id)).ok()??).ok()?;
    let last_start = app
        .store
        .runs_of_job(job_id, 1)
        .ok()?
        .first()
        .map(|r| r.started_at)
        .unwrap_or(0);
    (skip.at.timestamp() > last_start).then_some(skip)
}
