//! The last run of each job, and the one going on, kept in the state directory
//! for `replique-client status` (a quick look over SSH without the website) and
//! for local tools such as a desktop bar widget reading `status --json`.
//!
//! `runs/<job>.json` holds a job's finished runs, `runs/<job>.current.json` the
//! run in progress. Job names cannot contain a dot, so the second can never be
//! a job's own file: `jobs` reads it as `<job>`'s, never as a job named
//! `<job>.current`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use chrono::Utc;
use replique_proto::{FinishRun, Progress, RunKind, RunStatus, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ClientConfig;
use crate::spool::Spool;
use crate::state::{Kind, State};

/// What happened to a run's reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    Sent,
    Spooled,
    /// The server refused it (4xx); it will not be retried.
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub kind: RunKind,
    pub client_run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
    pub status: RunStatus,
    pub exit_code: i32,
    pub delivery: Delivery,
    /// One line for humans.
    pub detail: String,
}

/// `runs/<job>.json`: the last run of each kind.
type JobRecords = BTreeMap<String, RunRecord>;

fn kind_name(k: RunKind) -> &'static str {
    match k {
        RunKind::Backup => "backup",
        RunKind::Check => "check",
        RunKind::Prune => "prune",
        RunKind::Mirror => "mirror",
    }
}

pub fn record(state: &State, job: &str, rec: RunRecord) {
    let path = state.runs_dir().join(format!("{job}.json"));
    let mut all: JobRecords = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    all.insert(kind_name(rec.kind).to_string(), rec);
    if let Ok(bytes) = serde_json::to_vec_pretty(&all)
        && let Err(e) = state.write(&path, &bytes, Kind::Shared)
    {
        crate::log!("could not record the run for `status`: {e}");
    }
}

/// `runs/<job>.current.json`: what a job is doing right now, written when a run
/// starts, refreshed with its progress and removed when it finishes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurrentRun {
    pub kind: RunKind,
    pub client_run_id: String,
    pub started_at: Timestamp,
    /// The client running it, for telling a file left by a killed client.
    pub pid: u32,
    /// Absent until the run reports any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
}

fn current_path(state: &State, job: &str) -> PathBuf {
    state.runs_dir().join(format!("{job}.current.json"))
}

pub fn record_current(state: &State, job: &str, cur: &CurrentRun) -> Result<(), crate::Error> {
    let bytes = serde_json::to_vec_pretty(cur)?;
    state.write(&current_path(state, job), &bytes, Kind::Shared)
}

pub fn clear_current(state: &State, job: &str) {
    let _ = fs::remove_file(current_path(state, job));
}

/// The job's run in progress, if its client is still running. A file its
/// client left behind when it was killed is removed (when this user may).
pub fn current(state: &State, job: &str) -> Option<CurrentRun> {
    let path = current_path(state, job);
    let text = fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<CurrentRun>(&text) {
        Ok(cur) if is_client(cur.pid) => Some(cur),
        _ => {
            let _ = fs::remove_file(&path);
            None
        }
    }
}

/// Whether `pid` is a running replique-client. A live pid is not enough: the
/// file of a client killed hours ago names a pid that any process may have
/// been given since, so that process must also be a client. The age of the
/// last progress is no measure either: restic can stay silent for long.
///
/// On Linux, the target, `/proc/<pid>/cmdline` tells both. Elsewhere (macOS,
/// where the client is developed and tested) there is no `/proc`, and
/// `ps -o comm=` prints the process's executable, or nothing for a pid that is
/// gone, without a new dependency. When neither can be asked, the run counts
/// as live: a file wrongly kept shows a run that is over, a file wrongly
/// removed loses one that is not.
#[cfg(target_os = "linux")]
fn is_client(pid: u32) -> bool {
    match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(cmdline) => names_client(cmdline.split(|b| *b == 0).next().unwrap_or_default()),
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

#[cfg(not(target_os = "linux"))]
fn is_client(pid: u32) -> bool {
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(out) => names_client(out.stdout.trim_ascii()),
        Err(_) => true,
    }
}

/// A substring, not the whole name: the NixOS package's wrapper runs the
/// binary as `.replique-client-wrapped` when called by that path.
fn names_client(argv0: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::path::Path::new(std::ffi::OsStr::from_bytes(argv0))
        .file_name()
        .is_some_and(|n| n.to_string_lossy().contains("replique-client"))
}

/// Every job `runs/` knows of (finished runs or a run in progress), after the
/// configured ones: a job since removed from the configuration still shows.
pub fn jobs(config: &ClientConfig, state: &State) -> Vec<String> {
    let mut jobs: Vec<String> = config.jobs.keys().cloned().collect();
    for e in fs::read_dir(state.runs_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = e.path();
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && path.extension().is_some_and(|x| x == "json")
        {
            let name = stem.strip_suffix(".current").unwrap_or(stem);
            if !jobs.iter().any(|j| j == name) {
                jobs.push(name.to_string());
            }
        }
    }
    jobs
}

/// What `status --json` prints: each configured job's last run of each kind,
/// plus `current` while one runs. A job missing from the configuration shows
/// only while it runs.
pub fn json(config: &ClientConfig, state: &State) -> Result<Value, crate::Error> {
    let mut all = serde_json::Map::new();
    for job in jobs(config, state) {
        let current = current(state, &job);
        if !config.jobs.contains_key(&job) && current.is_none() {
            continue;
        }
        let mut v = serde_json::to_value(load(state, &job))?;
        if let (Some(cur), Some(obj)) = (current, v.as_object_mut()) {
            obj.insert("current".into(), serde_json::to_value(cur)?);
        }
        all.insert(job, v);
    }
    Ok(Value::Object(all))
}

/// `hetzner: backup running for 1h12m, 43% (1.2 TiB of 2.8 TiB), about 1h05m left`
fn current_line(job: &str, cur: &CurrentRun) -> String {
    let running = (Utc::now() - cur.started_at).num_seconds().max(0) as u64;
    let mut line = format!(
        "{job}: {} running for {}",
        kind_name(cur.kind),
        human_duration(running)
    );
    if let Some(p) = &cur.progress {
        if p.percent_done > 0.0 {
            line.push_str(&format!(", {:.0}%", p.percent_done * 100.0));
            if p.total_bytes > 0 {
                line.push_str(&format!(
                    " ({} of {})",
                    human_bytes(p.bytes_done),
                    human_bytes(p.total_bytes)
                ));
            }
        } else if let Some(phase) = &p.phase {
            line.push_str(&format!(", {phase}"));
        }
        if let Some(left) = p.seconds_remaining {
            line.push_str(&format!(", about {} left", human_duration(left)));
        }
    }
    line
}

pub fn load(state: &State, job: &str) -> JobRecords {
    fs::read_to_string(state.runs_dir().join(format!("{job}.json")))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// A one-line description of what a run found.
pub fn detail(f: &FinishRun) -> String {
    if let Some(m) = &f.message {
        return m.clone();
    }
    let mut parts = Vec::new();
    if let Some(s) = &f.summary {
        parts.push(format!(
            "{} new, {} changed, {} added",
            s.files_new,
            s.files_changed,
            human_bytes(s.data_added_packed)
        ));
        if let Some(id) = &s.snapshot_id {
            parts.push(format!("snapshot {}", &id[..id.len().min(8)]));
        }
    }
    if let Some(p) = &f.prune {
        parts.push(format!(
            "{}: {} of {} snapshots removed, {} kept for deleted files",
            p.policy, p.snapshots_removed, p.snapshots_before, p.snapshots_tagged_keep
        ));
        if let Some(b) = p.bytes_freed {
            parts.push(format!("{} freed", human_bytes(b)));
        }
    }
    if let Some(c) = &f.check {
        parts.push(format!(
            "{} errors, {} snapshots{}",
            c.num_errors,
            c.snapshot_ids.len(),
            c.read_data_subset
                .as_ref()
                .map(|s| format!(", read {s}"))
                .unwrap_or_default()
        ));
    }
    if let Some(m) = &f.mirror {
        parts.push(format!(
            "{} files copied ({}), {} removed, {} verified, {} mismatches, {} snapshots",
            m.files_copied,
            human_bytes(m.bytes_copied),
            m.files_removed,
            m.files_verified,
            m.hash_mismatches.len(),
            m.snapshot_ids.len()
        ));
        if !m.source_mismatches.is_empty() {
            parts.push(format!(
                "{} broken on the source (left out)",
                m.source_mismatches.len()
            ));
        }
    }
    if f.error_count > 0 {
        parts.push(format!("{} errors", f.error_count));
    }
    parts.join(", ")
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// The text `replique-client status` prints.
pub fn render(config: &ClientConfig, state: &State) -> String {
    let mut out = String::new();
    match state.load_device() {
        Ok(Some(d)) => out.push_str(&format!(
            "device {} ({}), server {}\nrepository {}\n\n",
            d.device_name,
            d.device_id,
            config.server.as_deref().unwrap_or(&d.server),
            d.storage.restic_sftp_repository()
        )),
        Ok(None) => out.push_str("not paired\n\n"),
        Err(e) => out.push_str(&format!("{e}\n\n")),
    }
    let mut running = Vec::new();
    let mut rows = vec![[
        "JOB".to_string(),
        "RUN".into(),
        "STATUS".into(),
        "FINISHED".into(),
        "TOOK".into(),
        "REPORT".into(),
        "DETAIL".into(),
    ]];
    for job in &jobs(config, state) {
        let recs = load(state, job);
        let current = current(state, job);
        if recs.is_empty() && current.is_none() && !config.jobs.contains_key(job) {
            // Known only from a file its killed client left.
            continue;
        }
        if recs.is_empty() {
            rows.push([
                job.clone(),
                "-".into(),
                if current.is_some() {
                    "running"
                } else {
                    "never run"
                }
                .into(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ]);
        }
        for kind in ["backup", "prune", "check", "mirror"] {
            if let Some(r) = recs.get(kind) {
                let took = (r.finished_at - r.started_at).num_seconds().max(0) as u64;
                rows.push([
                    job.clone(),
                    kind.into(),
                    r.status.as_str().into(),
                    r.finished_at.format("%Y-%m-%d %H:%M UTC").to_string(),
                    human_duration(took),
                    format!("{:?}", r.delivery).to_lowercase(),
                    r.detail.clone(),
                ]);
            }
        }
        if let Some(cur) = current {
            running.push(current_line(job, &cur));
        }
    }
    let widths: Vec<usize> = (0..7)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for r in &rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 6 {
                    c.clone()
                } else {
                    format!("{c:<w$}", w = widths[i])
                }
            })
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    if !running.is_empty() {
        out.push('\n');
        for line in &running {
            out.push_str(line);
            out.push('\n');
        }
    }
    let spooled = Spool::new(state).pending().len();
    if spooled > 0 {
        out.push_str(&format!(
            "\n{spooled} report(s) spooled, sent on the next run once the server answers\n"
        ));
    }
    out
}

pub fn human_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humans() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2_811_445_760), "2.6 GiB");
        assert_eq!(human_duration(20928), "5h48m");
        assert_eq!(human_duration(75), "1m15s");
    }
}
