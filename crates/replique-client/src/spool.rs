//! Reports the server could not take, kept on disk and replayed in order on a
//! later run. One file per run, `spool/<started>-<client_run_id>.json`, holding
//! the run's start and, once known, its finish.
//!
//! A run whose start could not be sent holds its entry locked until it ends, so a
//! replay from another job running at the same time leaves it alone; an entry
//! nobody holds and without a finish belongs to a client that died mid-run, and
//! only its start is replayed (the server then marks that run lost).

use std::fs::{self, File, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use replique_proto::{FinishRun, StartRun};
use serde::{Deserialize, Serialize};

use crate::api::{Api, ApiError};
use crate::state::State;
use crate::{Error, log};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub start: StartRun,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<FinishRun>,
}

pub struct Spool<'a> {
    state: &'a State,
}

/// An entry held locked by the run it belongs to.
pub struct Held {
    file: File,
    entry: Entry,
}

#[derive(Debug, Default, PartialEq)]
pub struct Replayed {
    pub sent: usize,
    pub rejected: usize,
    /// Still waiting: the server went away again, or another run holds them.
    pub remaining: usize,
}

impl<'a> Spool<'a> {
    pub fn new(state: &'a State) -> Spool<'a> {
        Spool { state }
    }

    fn dir(&self) -> PathBuf {
        self.state.spool_dir()
    }

    fn path_for(&self, start: &StartRun) -> PathBuf {
        self.dir().join(format!(
            "{}-{}.json",
            start.started_at.format("%Y%m%dT%H%M%S%.3fZ"),
            start.client_run_id
        ))
    }

    /// Spools a run's start and keeps the entry locked until `complete`.
    pub fn hold(&self, start: &StartRun) -> Result<Held, Error> {
        let path = self.path_for(start);
        // Written and locked under a temporary name first: a replay never sees an
        // entry that is not yet locked or not yet written.
        let tmp = path.with_extension("new");
        let file = self.state.open_shared(&tmp)?;
        file.lock()?;
        let mut held = Held {
            file,
            entry: Entry {
                start: start.clone(),
                finish: None,
            },
        };
        held.write()?;
        fs::rename(&tmp, &path)?;
        Ok(held)
    }

    /// Spools a whole run whose start was sent but whose finish was not.
    pub fn save(&self, entry: &Entry) -> Result<(), Error> {
        let path = self.path_for(&entry.start);
        self.state.write(
            &path,
            &serde_json::to_vec_pretty(entry)?,
            crate::state::Kind::Shared,
        )
    }

    /// Entries in replay order (their names start with the run's start time).
    pub fn pending(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(self.dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        v.sort();
        v
    }

    /// Sends what is spooled, oldest first. Stops at the first sign the server
    /// is unreachable, so the order is kept for the next attempt.
    pub fn replay(&self, api: &Api) -> Replayed {
        let mut out = Replayed::default();
        let pending = self.pending();
        let total = pending.len();
        for (i, path) in pending.iter().enumerate() {
            match self.replay_one(api, path) {
                Ok(Outcome::Sent) => out.sent += 1,
                Ok(Outcome::Busy) => out.remaining += 1,
                Ok(Outcome::Rejected) => out.rejected += 1,
                Err(e) => {
                    log!(
                        "server still unreachable, keeping {} spooled reports: {e}",
                        total - i
                    );
                    out.remaining += total - i;
                    break;
                }
            }
        }
        out
    }

    fn replay_one(&self, api: &Api, path: &Path) -> Result<Outcome, ApiError> {
        let Ok(mut file) = fs::OpenOptions::new().read(true).write(true).open(path) else {
            return Ok(Outcome::Busy);
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(Outcome::Busy),
            Err(TryLockError::Error(e)) => {
                log!("{}: {e}", path.display());
                return Ok(Outcome::Busy);
            }
        }
        // Another replay may have finished and removed it while we waited.
        if !path.exists() {
            return Ok(Outcome::Busy);
        }
        let mut text = String::new();
        let entry: Entry = match file
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())
            .and_then(|_| serde_json::from_str(&text).map_err(|e| e.to_string()))
        {
            Ok(e) => e,
            Err(e) => {
                log!(
                    "{}: unreadable spool entry ({e}), set aside",
                    path.display()
                );
                self.set_aside(path);
                return Ok(Outcome::Rejected);
            }
        };
        let run_id = match api.start_run(&entry.start) {
            Ok(id) => id,
            Err(e @ ApiError::Unreachable(_)) => return Err(e),
            Err(e) => {
                log!(
                    "{}: the server refused this run's start, set aside: {e}",
                    path.display()
                );
                self.set_aside(path);
                return Ok(Outcome::Rejected);
            }
        };
        if let Some(finish) = &entry.finish {
            match api.finish(&run_id, finish) {
                Ok(()) => {}
                Err(e) if e.is_conflict() => {
                    log!(
                        "run {run_id} was already finished with another result; dropping the spooled one"
                    );
                }
                Err(e @ ApiError::Unreachable(_)) => return Err(e),
                Err(e) => {
                    log!(
                        "{}: the server refused this run's finish, set aside: {e}",
                        path.display()
                    );
                    self.set_aside(path);
                    return Ok(Outcome::Rejected);
                }
            }
        }
        let _ = fs::remove_file(path);
        Ok(Outcome::Sent)
    }

    /// Moves an entry the server will never accept out of the way, kept for a
    /// human to look at.
    fn set_aside(&self, path: &Path) {
        let dir = self.dir().join("rejected");
        if self.state.ensure_dir(&dir).is_ok()
            && let Some(name) = path.file_name()
        {
            let _ = fs::rename(path, dir.join(name));
        }
    }
}

enum Outcome {
    Sent,
    Busy,
    Rejected,
}

impl Held {
    fn write(&mut self) -> Result<(), Error> {
        let bytes = serde_json::to_vec_pretty(&self.entry)?;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Adds the finish and releases the entry for replay.
    pub fn complete(mut self, finish: &FinishRun) -> Result<(), Error> {
        self.entry.finish = Some(finish.clone());
        self.write()
        // Dropping the file releases the lock.
    }
}
