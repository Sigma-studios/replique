//! Reporting runs: start, throttled progress, finish, with the spool behind them
//! so that a report is never lost while the server is down.

use std::fs;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::Utc;
use replique_proto::{
    FinishRun, PROGRESS_MIN_INTERVAL_SECS, Progress, RunKind, StartRun, Timestamp,
};

use crate::api::{Api, ApiError};
use crate::config::{ClientConfig, JobConfig};
use crate::spool::{Entry, Held, Spool};
use crate::state::{Kind, State};
use crate::status::{self, Delivery, RunRecord};
use crate::{Error, client_version, log};

pub struct Reporter {
    pub state: State,
    pub job: String,
    /// None when the device is not paired (a run with an explicit repository):
    /// reports are spooled for whenever it is.
    api: Option<Api>,
    progress_api: Option<Api>,
    pub restic_version: Option<String>,
}

pub struct Run {
    pub start: StartRun,
    run_id: Option<String>,
    held: Option<Held>,
    rejected: bool,
    progress_tx: Option<SyncSender<Progress>>,
    progress_thread: Option<JoinHandle<()>>,
    last_progress: Option<Instant>,
    started: Instant,
}

impl Run {
    pub fn kind(&self) -> RunKind {
        self.start.kind
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

impl Reporter {
    /// Builds the reporter for a job from the configuration and the pairing.
    pub fn new(config: &ClientConfig, state: &State, job: &str) -> Result<Reporter, Error> {
        let device = state.load_device()?;
        let server = config
            .server
            .clone()
            .or_else(|| device.as_ref().map(|d| d.server.clone()));
        let token = state.token()?;
        let (api, progress_api) = match (server, token) {
            (Some(s), Some(t)) => (
                Some(Api::new(&s, Some(t.clone()))),
                // Progress is best effort and must not hold the run up.
                Some(Api::with_timeout(&s, Some(t), Duration::from_secs(5))),
            ),
            _ => {
                log!("not paired with a server: reports are spooled");
                (None, None)
            }
        };
        Ok(Reporter {
            state: state.clone(),
            job: job.to_string(),
            api,
            progress_api,
            restic_version: None,
        })
    }

    pub fn api(&self) -> Option<&Api> {
        self.api.as_ref()
    }

    /// Sends what earlier runs spooled.
    pub fn replay(&self) {
        let Some(api) = &self.api else { return };
        let spool = Spool::new(&self.state);
        if spool.pending().is_empty() {
            return;
        }
        let r = spool.replay(api);
        if r.sent + r.rejected > 0 {
            log!(
                "replayed spooled reports: {} sent, {} refused, {} still waiting",
                r.sent,
                r.rejected,
                r.remaining
            );
        }
    }

    /// `PUT /jobs/{name}` when the declaration differs from the last one the
    /// server accepted. Not spooled: it is tried again on every run until it
    /// goes through, and a run for an undeclared job is accepted anyway.
    pub fn declare(&self, job: &JobConfig) {
        let Some(api) = &self.api else { return };
        let spec = job.spec();
        let Ok(bytes) = serde_json::to_vec_pretty(&spec) else {
            return;
        };
        let path = self.state.declared_dir().join(format!("{}.json", self.job));
        if fs::read(&path).ok().as_deref() == Some(&bytes[..]) {
            return;
        }
        match api.put_job(&self.job, &spec) {
            Ok(()) => {
                if let Err(e) = self.state.write(&path, &bytes, Kind::Shared) {
                    log!("declared job {}, but could not remember it: {e}", self.job);
                }
            }
            Err(e) => log!("could not declare job {}: {e}", self.job),
        }
    }

    pub fn start(&self, kind: RunKind) -> Run {
        self.start_at(kind, Utc::now())
    }

    pub fn start_at(&self, kind: RunKind, started_at: Timestamp) -> Run {
        let start = StartRun {
            client_run_id: uuid::Uuid::new_v4().to_string(),
            job: self.job.clone(),
            kind,
            started_at,
            restic_version: self.restic_version.clone(),
            client_version: Some(client_version()),
        };
        let mut run = Run {
            start,
            run_id: None,
            held: None,
            rejected: false,
            progress_tx: None,
            progress_thread: None,
            last_progress: None,
            started: Instant::now(),
        };
        let result = match &self.api {
            Some(api) => api.start_run(&run.start),
            None => Err(ApiError::Unreachable("not paired".into())),
        };
        match result {
            Ok(id) => {
                run.run_id = Some(id.clone());
                if let Some(api) = self.progress_api.clone() {
                    let (tx, rx) = sync_channel::<Progress>(1);
                    run.progress_tx = Some(tx);
                    run.progress_thread = Some(std::thread::spawn(move || {
                        for p in rx {
                            // Progress is never spooled: a stale one is useless.
                            let _ = api.progress(&id, &p);
                        }
                    }));
                }
            }
            Err(ApiError::Unreachable(e)) => {
                if self.api.is_some() {
                    log!("server unreachable, spooling this run's reports: {e}");
                }
                match Spool::new(&self.state).hold(&run.start) {
                    Ok(h) => run.held = Some(h),
                    Err(e) => log!("could not spool the run's start: {e}"),
                }
            }
            Err(e) => {
                log!("the server refused this run's start: {e}");
                run.rejected = true;
            }
        }
        run
    }

    /// Forwards progress at most every 10 seconds; never waits on the network.
    pub fn progress(&self, run: &mut Run, p: Progress) {
        let Some(tx) = &run.progress_tx else { return };
        let due = run
            .last_progress
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(PROGRESS_MIN_INTERVAL_SECS));
        if !due {
            return;
        }
        match tx.try_send(p) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => run.last_progress = Some(Instant::now()),
            // The previous one is still being sent: skip this one.
            Err(TrySendError::Full(_)) => {}
        }
    }

    pub fn finish(&self, mut run: Run, finish: FinishRun) {
        drop(run.progress_tx.take());
        if let Some(t) = run.progress_thread.take() {
            let _ = t.join();
        }
        let delivery = if let Some(held) = run.held.take() {
            match held.complete(&finish) {
                Ok(()) => Delivery::Spooled,
                Err(e) => {
                    log!("could not spool the run's finish: {e}");
                    Delivery::Rejected
                }
            }
        } else if run.rejected {
            Delivery::Rejected
        } else if let (Some(api), Some(id)) = (&self.api, &run.run_id) {
            match api.finish(id, &finish) {
                Ok(()) => Delivery::Sent,
                Err(ApiError::Unreachable(e)) => {
                    log!("server unreachable, spooling this run's finish: {e}");
                    let entry = Entry {
                        start: run.start.clone(),
                        finish: Some(finish.clone()),
                    };
                    match Spool::new(&self.state).save(&entry) {
                        Ok(()) => Delivery::Spooled,
                        Err(e) => {
                            log!("could not spool the run's finish: {e}");
                            Delivery::Rejected
                        }
                    }
                }
                Err(e) => {
                    log!("the server refused this run's finish: {e}");
                    Delivery::Rejected
                }
            }
        } else {
            Delivery::Rejected
        };
        status::record(
            &self.state,
            &self.job,
            RunRecord {
                kind: run.start.kind,
                client_run_id: run.start.client_run_id.clone(),
                run_id: run.run_id.clone(),
                started_at: run.start.started_at,
                finished_at: finish.finished_at,
                status: finish.status,
                exit_code: finish.exit_code,
                delivery,
                detail: status::detail(&finish),
            },
        );
    }
}
