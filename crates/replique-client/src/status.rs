//! The last run of each job, kept in the state directory for `replique-client
//! status` (a quick look over SSH without the website).

use std::collections::BTreeMap;
use std::fs;

use replique_proto::{FinishRun, RunKind, RunStatus, Timestamp};
use serde::{Deserialize, Serialize};

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
    let mut jobs: Vec<String> = config.jobs.keys().cloned().collect();
    for e in fs::read_dir(state.runs_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        if let Some(name) = e.path().file_stem().and_then(|s| s.to_str())
            && !jobs.iter().any(|j| j == name)
            && e.path().extension().is_some_and(|x| x == "json")
        {
            jobs.push(name.to_string());
        }
    }
    let mut rows = vec![[
        "JOB".to_string(),
        "RUN".into(),
        "STATUS".into(),
        "FINISHED".into(),
        "TOOK".into(),
        "REPORT".into(),
        "DETAIL".into(),
    ]];
    for job in &jobs {
        let recs = load(state, job);
        if recs.is_empty() {
            rows.push([
                job.clone(),
                "-".into(),
                "never run".into(),
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
