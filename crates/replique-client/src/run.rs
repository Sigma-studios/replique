//! `replique-client run <job>`: a backup, then its prune and its check, each
//! reported as its own run.

use chrono::Utc;
use replique_proto::{
    BackupSummary, CheckSummary, FinishRun, JobKind, Progress, RunKind, RunStatus,
};
use serde_json::Value;

use crate::config::{ClientConfig, JobConfig};
use crate::excludes::{self, Scan};
use crate::report::Reporter;
use crate::restic::{Outcome, RETRY_LOCK, Restic, Status};
use crate::signals::Stop;
use crate::state::{Kind, State};
use crate::{Error, log, mounts, pair, retention};

/// Runs a job and returns the exit code for the process: restic's (rclone's for
/// a mirror), so systemd marks the unit failed when the backup did.
pub fn run(config: &ClientConfig, state: &State, name: &str, stop: &Stop) -> Result<i32, Error> {
    let job = config.job(name)?;
    match job.kind {
        JobKind::Backup => backup(config, state, name, job, stop),
        JobKind::Mirror => crate::mirror::run(config, state, name, job, stop),
    }
}

/// A finish for a run that failed with `outcome`, before or without a summary.
pub fn failed(outcome: &Outcome) -> FinishRun {
    FinishRun {
        status: RunStatus::from_restic_exit(outcome.code),
        exit_code: outcome.code,
        finished_at: Utc::now(),
        summary: None,
        prune: None,
        check: None,
        mirror: None,
        errors: outcome.errors.clone(),
        error_count: outcome.error_count,
        unreadable_paths: outcome.unreadable.clone(),
        // What restic says on standard error when all went well is noise.
        message: if outcome.code == 0 {
            None
        } else {
            outcome.message.clone()
        },
    }
}

/// A run that ends before restic started: reported failed with `message`.
pub fn refuse(reporter: &Reporter, kind: RunKind, message: String) -> i32 {
    log!("{message}");
    let run = reporter.start(kind);
    reporter.finish(
        run,
        failed(&Outcome {
            code: 1,
            message: Some(message),
            ..Outcome::default()
        }),
    );
    1
}

fn backup(
    config: &ClientConfig,
    state: &State,
    name: &str,
    job: &JobConfig,
    stop: &Stop,
) -> Result<i32, Error> {
    let mut reporter = Reporter::new(config, state, name)?;
    reporter.replay();
    reporter.declare(job);

    if job.paths.is_empty() {
        return Ok(refuse(
            &reporter,
            RunKind::Backup,
            format!("job {name} has no paths"),
        ));
    }
    if let Err(m) = mounts::check(&job.require_mounts) {
        return Ok(refuse(&reporter, RunKind::Backup, m));
    }
    let restic = match Restic::for_job(config, state, job) {
        Ok(r) => r,
        Err(e) => return Ok(refuse(&reporter, RunKind::Backup, e.to_string())),
    };
    if job.repository.is_none() {
        pair::ensure_ssh_config(config, state);
    }
    reporter.restic_version = restic.version();

    let mut run = reporter.start(RunKind::Backup);
    match restic.ensure_repository() {
        Ok(true) => log!("created the repository {}", restic.repository),
        Ok(false) => {}
        Err(o) => {
            log!(
                "{}",
                o.message.as_deref().unwrap_or("cannot open the repository")
            );
            reporter.finish(run, failed(&o));
            return Ok(o.code);
        }
    }

    let exclude_file = match write_excludes(state, name, job) {
        Ok(p) => p,
        Err(e) => {
            let o = Outcome {
                code: 1,
                message: Some(format!("cannot write the exclude file: {e}")),
                ..Outcome::default()
            };
            reporter.finish(run, failed(&o));
            return Ok(1);
        }
    };
    let mut args: Vec<String> = vec![
        "backup".into(),
        "--json".into(),
        "--exclude-file".into(),
        exclude_file.to_string_lossy().into_owned(),
    ];
    if job.exclude_caches {
        args.push("--exclude-caches".into());
    }
    if let Some(h) = &job.host {
        args.extend(["--host".into(), h.clone()]);
    }
    args.extend(job.extra_backup_args.iter().cloned());
    args.push("--".into());
    args.extend(job.paths.iter().cloned());

    let mut summary: Option<BackupSummary> = None;
    let outcome = restic.stream(&args, stop, |v: Value| {
        match v.get("message_type").and_then(Value::as_str) {
            Some("status") => {
                if let Ok(s) = serde_json::from_value::<Status>(v) {
                    reporter.progress(&mut run, progress_of(s));
                }
            }
            Some("summary") => summary = serde_json::from_value(v).ok(),
            _ => {}
        }
    });
    let status = RunStatus::from_restic_exit(outcome.code);
    let finish = FinishRun {
        summary,
        // A partial run's errors say it all; the message is for runs that
        // failed outright.
        message: match status {
            RunStatus::Success | RunStatus::Partial => None,
            _ => outcome.message.clone(),
        },
        ..failed(&outcome)
    };
    log!(
        "backup {}: {}",
        status.as_str(),
        crate::status::detail(&finish)
    );
    reporter.finish(run, finish);

    let mut code = outcome.code;
    if matches!(status, RunStatus::Success | RunStatus::Partial) {
        if !stop.requested() {
            let c = prune(&reporter, &restic, job, stop);
            if code == 0 {
                code = c;
            }
        }
        if let Some(subset) = &job.check_subset
            && !stop.requested()
        {
            let c = check(&reporter, &restic, subset, stop);
            if code == 0 {
                code = c;
            }
        }
    }
    reporter.replay();
    Ok(code)
}

fn progress_of(s: Status) -> Progress {
    Progress {
        at: Utc::now(),
        percent_done: s.percent_done,
        seconds_elapsed: s.seconds_elapsed,
        seconds_remaining: s.seconds_remaining,
        total_files: s.total_files,
        files_done: s.files_done,
        total_bytes: s.total_bytes,
        bytes_done: s.bytes_done,
        error_count: s.error_count,
        current_files: s.current_files.unwrap_or_default(),
        phase: None,
    }
}

/// Writes the job's exclude file (configured patterns plus the build output
/// the scan finds) to the state directory, where it stays for inspection.
pub fn write_excludes(
    state: &State,
    name: &str,
    job: &JobConfig,
) -> Result<std::path::PathBuf, Error> {
    let generated = if job.generated_excludes_for.is_empty() {
        vec![]
    } else {
        let found = Scan {
            roots: &job.generated_excludes_for,
            skip: excludes::literal_paths(&job.exclude),
            exclude_caches: job.exclude_caches,
        }
        .run();
        log!("{} build-output folders excluded", found.len());
        found
    };
    let path = state.excludes_dir().join(format!("{name}.txt"));
    state.write(
        &path,
        excludes::exclude_file(&job.exclude, &generated).as_bytes(),
        Kind::Shared,
    )?;
    Ok(path)
}

fn prune(reporter: &Reporter, restic: &Restic, job: &JobConfig, stop: &Stop) -> i32 {
    let Some(policy) = job.effective_retention() else {
        return 0;
    };
    if policy == replique_proto::Retention::NeverPrune {
        return 0;
    }
    let mut run = reporter.start(RunKind::Prune);
    let filters = retention::filters(&job.paths, job.host.as_deref());
    let result = retention::apply(restic, &policy, &filters, stop, |p| {
        reporter.progress(&mut run, p)
    });
    match result {
        None => 0,
        Some(Ok(summary)) => {
            let finish = FinishRun {
                prune: Some(summary),
                ..failed(&Outcome::default())
            };
            log!("prune: {}", crate::status::detail(&finish));
            reporter.finish(run, finish);
            0
        }
        Some(Err(o)) => {
            log!("prune failed: {}", o.message.as_deref().unwrap_or(""));
            reporter.finish(run, failed(&o));
            o.code
        }
    }
}

fn check(reporter: &Reporter, restic: &Restic, subset: &str, stop: &Stop) -> i32 {
    let mut run = reporter.start(RunKind::Check);
    let snapshot_ids = match restic.snapshots(&[]) {
        Ok(s) => s.into_iter().map(|s| s.id).collect::<Vec<_>>(),
        Err(o) => {
            reporter.finish(run, failed(&o));
            return o.code;
        }
    };
    reporter.progress(
        &mut run,
        Progress {
            at: Utc::now(),
            phase: Some(format!("reading back {subset} of the data")),
            ..Progress::default()
        },
    );
    let args: Vec<String> = vec![
        "--retry-lock".into(),
        RETRY_LOCK.into(),
        "check".into(),
        "--json".into(),
        format!("--read-data-subset={subset}"),
    ];
    let mut num_errors = 0;
    let outcome = restic.stream(&args, stop, |v| {
        if v.get("message_type").and_then(Value::as_str) == Some("summary") {
            num_errors = v.get("num_errors").and_then(Value::as_u64).unwrap_or(0);
        }
    });
    let finish = FinishRun {
        check: Some(CheckSummary {
            read_data_subset: Some(subset.to_string()),
            num_errors,
            snapshot_ids,
        }),
        ..failed(&outcome)
    };
    log!(
        "check {}: {}",
        finish.status.as_str(),
        crate::status::detail(&finish)
    );
    reporter.finish(run, finish);
    outcome.code
}
