//! `replique-client mirror <job>`: an encrypted byte copy of another device's
//! repository (the phone's), pulled through its read-only twin login.
//!
//! This machine has no password for that repository, so `restic check` is out of
//! reach. restic names every data, index and snapshot file after the SHA-256 of
//! its contents, though, so hashing each new file and comparing with its name
//! catches corruption without decrypting anything. Files that vanish from the
//! source (the phone never prunes, so this should not happen) are moved aside
//! and kept for a while rather than deleted.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, NaiveDateTime, Utc};
use replique_proto::{
    FinishRun, MirrorSource, MirrorSources, MirrorSummary, Progress, RunError, RunKind, RunStatus,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::api::ApiError;
use crate::config::{ClientConfig, JobConfig};
use crate::report::Reporter;
use crate::restic::{Outcome, exit_code};
use crate::run::{failed, refuse};
use crate::signals::Stop;
use crate::state::{Kind, State};
use crate::{Error, log};

/// The folders of a restic repository whose files are named by their hash.
/// `config` and `keys/` are not (a key file is named by a random id), and
/// `locks/` is transient and not copied.
const HASHED_DIRS: &[&str] = &["data", "index", "snapshots"];

const REMOVED_STAMP: &str = "%Y-%m-%dT%H%M%SZ";

pub fn run(
    config: &ClientConfig,
    state: &State,
    name: &str,
    job: &JobConfig,
    stop: &Stop,
) -> Result<i32, Error> {
    let reporter = Reporter::new(config, state, name)?;
    reporter.replay();
    reporter.declare(job);

    let Some(dest) = job.mirror_path.clone() else {
        return Ok(refuse(
            &reporter,
            RunKind::Mirror,
            format!("mirror job {name} has no mirror_path"),
        ));
    };
    let Some(removed_root) = job.removed_path() else {
        return Ok(refuse(
            &reporter,
            RunKind::Mirror,
            format!("mirror job {name} has no removed_path"),
        ));
    };
    let source_id = job.mirror_source.clone().unwrap_or_default();
    let source = match &job.mirror_from {
        Some(from) => Source {
            remote: from.clone(),
            flags: vec![],
        },
        None => {
            if source_id.is_empty() {
                return Ok(refuse(
                    &reporter,
                    RunKind::Mirror,
                    format!("mirror job {name} has no mirror_source"),
                ));
            }
            match find_source(&reporter, state, &source_id) {
                Ok(s) => match sftp_source(state, &s) {
                    Ok(src) => src,
                    Err(e) => return Ok(refuse(&reporter, RunKind::Mirror, e.to_string())),
                },
                Err(e) => return Ok(refuse(&reporter, RunKind::Mirror, e.to_string())),
            }
        }
    };

    let mut run = reporter.start(RunKind::Mirror);

    // Copies a run found broken are dropped before copying again. rclone runs
    // with --immutable, so it would refuse to replace them, and the copy would
    // fail the same way every night; removed, they come back whole below. A
    // file that is wrong on the source itself fails again after this.
    let mut previous = load_verified(state, name);
    let recheck = verify(&dest, &previous, stop, |_| {});
    for rel in &recheck.mismatches {
        match fs::remove_file(dest.join(rel)) {
            Ok(()) => log!("{rel}: did not match its name, copying it again"),
            Err(e) => log!("{rel}: did not match its name and cannot be removed: {e}"),
        }
    }
    previous = recheck.verified;

    let stamp = Utc::now().format(REMOVED_STAMP).to_string();
    let backup_dir = removed_root.join(&stamp);
    let mut args: Vec<String> = vec![
        "sync".into(),
        source.remote.clone(),
        dest.to_string_lossy().into_owned(),
        "--backup-dir".into(),
        backup_dir.to_string_lossy().into_owned(),
        // Locks are short-lived files of a running backup on the source; the
        // copy is never written to, so it needs none.
        "--exclude".into(),
        "/locks/**".into(),
        // The source's read-only login keeps its authorized_keys inside the
        // repository folder (its home is that folder, so it cannot share the
        // writable login's keys). Who may read the source is not part of its
        // backups, and a copy of it here would be misleading.
        "--exclude".into(),
        "/.ssh/**".into(),
        // A file the source is still uploading: the phone's rclone writes each
        // one as `<name>.<random>.partial` and renames it once complete. Copied
        // mid-upload, it grows under rclone, which then reports it corrupted
        // ("sizes differ"); it comes with the next run under its real name.
        "--exclude".into(),
        "*.partial".into(),
        // restic never rewrites a file: one changing in place is corruption, and
        // must not overwrite the good copy.
        "--immutable".into(),
        "--use-json-log".into(),
        "--stats".into(),
        "10s".into(),
        "--stats-log-level".into(),
        "NOTICE".into(),
    ];
    // A file the source is still writing: the phone's rclone uploads each
    // pack in place, under its final name, so a copy taken meanwhile is cut
    // short (and fails its hash check). Restic never touches a file again
    // once written, so anything this old is complete.
    let min_age = job.mirror_min_age.clone().unwrap_or_else(|| "15m".into());
    if !matches!(min_age.trim(), "" | "0" | "0s") {
        args.push("--min-age".into());
        args.push(min_age);
    }
    args.extend(source.flags.iter().cloned());

    let rclone = std::env::var_os("REPLIQUE_RCLONE").unwrap_or_else(|| "rclone".into());
    let copy = sync(&rclone, &args, stop, |p| reporter.progress(&mut run, p));

    let verify = verify(&dest, &previous, stop, |p| reporter.progress(&mut run, p));
    save_verified(state, name, &verify.verified);

    let files_removed = count_files(&backup_dir);
    expire_removed(&removed_root, job.keep_removed_days);

    let summary = MirrorSummary {
        source_device_id: source_id,
        files_copied: verify.new_files,
        bytes_copied: verify.new_bytes,
        files_removed,
        files_verified: verify.hashed,
        hash_mismatches: verify.mismatches.clone(),
        snapshot_ids: verify.snapshot_ids.clone(),
        total_bytes: Some(verify.total_bytes),
    };
    let (status, code, message) = if copy.code != 0 {
        let status = if stop.requested() {
            RunStatus::Interrupted
        } else {
            RunStatus::Failed
        };
        (
            status,
            copy.code,
            copy.message
                .clone()
                .or(Some(format!("rclone exited with {}", copy.code))),
        )
    } else if !verify.mismatches.is_empty() {
        (
            RunStatus::Failed,
            1,
            Some(format!(
                "{} file(s) of the copy do not match their name, first {}",
                verify.mismatches.len(),
                verify.mismatches[0]
            )),
        )
    } else {
        (RunStatus::Success, 0, None)
    };
    let finish = FinishRun {
        status,
        exit_code: code,
        mirror: Some(summary),
        message,
        ..failed(&copy)
    };
    log!(
        "mirror {}: {}",
        status.as_str(),
        crate::status::detail(&finish)
    );
    reporter.finish(run, finish);
    reporter.replay();
    Ok(code)
}

struct Source {
    remote: String,
    flags: Vec<String>,
}

/// The source's twin login: from the server, or the copy kept from the last
/// answer when the server is down (a mirror must not depend on it either).
fn find_source(reporter: &Reporter, state: &State, device_id: &str) -> Result<MirrorSource, Error> {
    let cache = state.mirror_dir().join("sources.json");
    let fresh = reporter.api().map(|api| api.mirror_sources());
    let sources: MirrorSources = match fresh {
        Some(Ok(s)) => {
            if let Ok(bytes) = serde_json::to_vec_pretty(&s) {
                let _ = state.write(&cache, &bytes, Kind::Shared);
            }
            s
        }
        other => {
            if let Some(Err(e @ ApiError::Rejected { .. })) = &other {
                log!("the server refused the mirror sources: {e}");
            }
            let text = fs::read_to_string(&cache).map_err(|_| {
                Error::msg("cannot reach the server for the mirror sources, and none are cached")
            })?;
            log!("server unreachable: using the mirror sources it sent last time");
            serde_json::from_str(&text)?
        }
    };
    sources
        .sources
        .into_iter()
        .find(|s| s.device_id == device_id)
        .ok_or_else(|| {
            Error::msg(format!(
                "the server lists no mirror source {device_id}: assign this mirror on the website"
            ))
        })
}

/// rclone's on-the-fly SFTP remote for a twin login, with this device's key.
fn sftp_source(state: &State, source: &MirrorSource) -> Result<Source, Error> {
    let s = &source.storage;
    let mut flags = vec![
        "--sftp-host".into(),
        s.host.clone(),
        "--sftp-port".into(),
        s.port.to_string(),
        "--sftp-user".into(),
        s.username.clone(),
        "--sftp-key-file".into(),
        state.key_file().to_string_lossy().into_owned(),
        // The hash check below reads every new file locally; rclone asking the
        // Storage Box to hash them too would read them twice there.
        "--sftp-disable-hashcheck".into(),
    ];
    // The twin is on the same box as this device's own login, so the host keys
    // from pairing usually cover it.
    let mut known: Vec<String> = s.known_hosts.clone();
    if let Ok(paired) = fs::read_to_string(state.known_hosts_file()) {
        known.extend(paired.lines().map(str::to_string));
    }
    known.retain(|l| !l.trim().is_empty());
    known.dedup();
    if known.is_empty() {
        log!("no host key known for {}: rclone will not check it", s.host);
    } else {
        let path = state.mirror_dir().join("known_hosts");
        state.write(&path, (known.join("\n") + "\n").as_bytes(), Kind::Shared)?;
        flags.push("--sftp-known-hosts-file".into());
        flags.push(path.to_string_lossy().into_owned());
        // Go's SSH client asks for the server's preferred host key type and
        // calls a known host with a key of another type a mismatch. Asking only
        // for the types we hold keys of avoids that.
        flags.push("--sftp-host-key-algorithms".into());
        flags.push(host_key_algorithms(&known).join(" "));
    }
    Ok(Source {
        remote: format!(":sftp:{}", s.repo_path),
        flags,
    })
}

/// The SSH host key algorithms matching the key types of known_hosts lines.
fn host_key_algorithms(known: &[String]) -> Vec<String> {
    let mut algos: Vec<String> = Vec::new();
    for line in known {
        let Some(kind) = line.split_whitespace().nth(1) else {
            continue;
        };
        let names: &[&str] = match kind {
            "ssh-rsa" => &["rsa-sha2-512", "rsa-sha2-256", "ssh-rsa"],
            other => &[other][..],
        };
        for n in names {
            if !algos.iter().any(|a| a == n) {
                algos.push(n.to_string());
            }
        }
    }
    algos
}

#[derive(Deserialize)]
struct LogLine {
    #[serde(default)]
    level: String,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    object: Option<String>,
    #[serde(default)]
    stats: Option<Stats>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stats {
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    total_bytes: u64,
    #[serde(default)]
    elapsed_time: f64,
    #[serde(default)]
    eta: Option<f64>,
    #[serde(default)]
    transfers: u64,
    #[serde(default)]
    total_transfers: u64,
    #[serde(default)]
    errors: u64,
    #[serde(default)]
    transferring: Option<Vec<Transferring>>,
}

#[derive(Deserialize)]
struct Transferring {
    #[serde(default)]
    name: String,
}

/// Runs rclone, turning its JSON log into progress and errors.
fn sync(
    rclone: &std::ffi::OsStr,
    args: &[String],
    stop: &Stop,
    mut progress: impl FnMut(Progress),
) -> Outcome {
    let mut child = match Command::new(rclone)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                code: 127,
                message: Some(format!("cannot run rclone: {e}")),
                ..Outcome::default()
            };
        }
    };
    let mut o = Outcome::default();
    let stderr = child.stderr.take().expect("piped");
    for line in BufReader::new(stderr).lines() {
        let Ok(line) = line else { break };
        let Ok(l) = serde_json::from_str::<LogLine>(&line) else {
            if !line.trim().is_empty() {
                eprintln!("{line}");
            }
            continue;
        };
        if let Some(s) = l.stats {
            progress(Progress {
                at: Utc::now(),
                percent_done: if s.total_bytes > 0 {
                    s.bytes as f64 / s.total_bytes as f64
                } else {
                    0.0
                },
                seconds_elapsed: s.elapsed_time as u64,
                seconds_remaining: s.eta.map(|e| e as u64),
                total_files: s.total_transfers,
                files_done: s.transfers,
                total_bytes: s.total_bytes,
                bytes_done: s.bytes,
                error_count: s.errors,
                // A mirror downloads; the website shows rclone's own rate from
                // `bytes_done` over time instead.
                bytes_uploaded: None,
                upload_bytes_per_sec: None,
                current_files: s
                    .transferring
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t.name)
                    .collect(),
                phase: Some("copying".into()),
            });
        } else if l.level == "error" || l.level == "critical" {
            eprintln!("rclone: {}", l.msg);
            o.error_count += 1;
            if o.errors.len() < replique_proto::MAX_REPORTED_ERRORS {
                o.errors.push(RunError {
                    message: l.msg.clone(),
                    during: Some("copy".into()),
                    item: l.object.clone(),
                });
            }
            o.message = Some(l.msg);
        }
    }
    o.code = child.wait().map(exit_code).unwrap_or(1);
    if stop.requested() && o.code != 0 {
        o.code = 130;
    }
    o
}

#[derive(Debug, Default)]
pub struct Verified {
    /// Repository path → size, for every file that matched its name.
    pub verified: BTreeMap<String, u64>,
    pub hashed: u64,
    pub new_files: u64,
    pub new_bytes: u64,
    pub mismatches: Vec<String>,
    pub snapshot_ids: Vec<String>,
    pub total_bytes: u64,
}

/// Hashes every hash-named file not verified before (or whose size changed)
/// and compares with its name.
pub fn verify(
    root: &Path,
    previous: &BTreeMap<String, u64>,
    stop: &Stop,
    mut progress: impl FnMut(Progress),
) -> Verified {
    let mut out = Verified::default();
    let mut todo: Vec<(String, PathBuf, u64)> = Vec::new();
    for (rel, path, size) in walk(root) {
        out.total_bytes += size;
        let top = rel.split('/').next().unwrap_or("");
        if !HASHED_DIRS.contains(&top) || rel.ends_with(".partial") {
            continue;
        }
        if previous.get(&rel) == Some(&size) {
            out.verified.insert(rel, size);
        } else {
            todo.push((rel, path, size));
        }
    }
    let to_hash: u64 = todo.iter().map(|t| t.2).sum();
    let mut hashed_bytes = 0u64;
    for (rel, path, size) in todo {
        if stop.requested() {
            break;
        }
        progress(Progress {
            at: Utc::now(),
            percent_done: if to_hash > 0 {
                hashed_bytes as f64 / to_hash as f64
            } else {
                0.0
            },
            total_bytes: to_hash,
            bytes_done: hashed_bytes,
            current_files: vec![rel.clone()],
            phase: Some("verifying packs".into()),
            ..Progress::default()
        });
        hashed_bytes += size;
        out.hashed += 1;
        let name = rel.rsplit('/').next().unwrap_or("");
        match sha256_hex(&path) {
            Ok(h) if h == name => {
                if !previous.contains_key(&rel) {
                    out.new_files += 1;
                    out.new_bytes += size;
                }
                out.verified.insert(rel, size);
            }
            Ok(_) => out.mismatches.push(rel),
            Err(e) => {
                log!("{}: {e}", path.display());
                out.mismatches.push(rel);
            }
        }
    }
    out.snapshot_ids = out
        .verified
        .keys()
        .filter_map(|k| k.strip_prefix("snapshots/"))
        .map(str::to_string)
        .collect();
    out
}

fn sha256_hex(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Every regular file under `root`: its path relative to it (with `/`), its
/// full path and its size, sorted.
fn walk(root: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(t) = e.file_type() else { continue };
            let path = e.path();
            if t.is_dir() {
                stack.push(path);
            } else if t.is_file() {
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                let rel = path
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                out.push((rel, path, size));
            }
        }
    }
    out.sort();
    out
}

fn count_files(dir: &Path) -> u64 {
    walk(dir).len() as u64
}

/// Deletes the removed-files folders older than `days`, by the time in their
/// name.
pub fn expire_removed(root: &Path, days: u32) {
    let cutoff = Utc::now() - chrono::Duration::days(days.into());
    for e in fs::read_dir(root).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(t) = NaiveDateTime::parse_from_str(&name, REMOVED_STAMP) else {
            continue;
        };
        if DateTime::<Utc>::from_naive_utc_and_offset(t, Utc) < cutoff
            && let Err(err) = fs::remove_dir_all(e.path())
        {
            log!("could not delete {}: {err}", e.path().display());
        }
    }
}

fn verified_file(state: &State, job: &str) -> PathBuf {
    state.mirror_dir().join(format!("{job}.verified.json"))
}

fn load_verified(state: &State, job: &str) -> BTreeMap<String, u64> {
    fs::read_to_string(verified_file(state, job))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_verified(state: &State, job: &str, v: &BTreeMap<String, u64>) {
    if let Ok(bytes) = serde_json::to_vec(v)
        && let Err(e) = state.write(&verified_file(state, job), &bytes, Kind::Shared)
    {
        log!("could not remember the verified files: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(root: &Path, rel: &str, content: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn hex(content: &[u8]) -> String {
        Sha256::digest(content)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn verifies_names_once() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let pack = b"pack bytes";
        let snap = b"snapshot";
        put(r, &format!("data/{}/{}", &hex(pack)[..2], hex(pack)), pack);
        put(r, &format!("snapshots/{}", hex(snap)), snap);
        put(r, &format!("index/{}", hex(b"index")), b"corrupted");
        put(r, "keys/0123", b"random id, not a hash");
        put(r, "config", b"not a hash either");

        let v = verify(r, &BTreeMap::new(), &Stop::never(), |_| {});
        assert_eq!(v.hashed, 3);
        assert_eq!(v.new_files, 2);
        assert_eq!(v.mismatches, [format!("index/{}", hex(b"index"))]);
        assert_eq!(v.snapshot_ids, [hex(snap)]);

        // A second pass hashes only what was not verified (the bad index).
        let again = verify(r, &v.verified, &Stop::never(), |_| {});
        assert_eq!(again.hashed, 1);
        assert_eq!(again.new_files, 0);
        assert_eq!(again.mismatches.len(), 1);
    }

    #[test]
    fn host_key_types() {
        let known = [
            "[box]:23 ssh-ed25519 AAAA".to_string(),
            "[box]:23 ssh-rsa AAAA".to_string(),
            "[other]:23 ssh-ed25519 AAAA".to_string(),
        ];
        assert_eq!(
            host_key_algorithms(&known),
            ["ssh-ed25519", "rsa-sha2-512", "rsa-sha2-256", "ssh-rsa"]
        );
    }

    #[test]
    fn expires_old_removed_folders() {
        let d = tempfile::tempdir().unwrap();
        let old = (Utc::now() - chrono::Duration::days(31))
            .format(REMOVED_STAMP)
            .to_string();
        let recent = (Utc::now() - chrono::Duration::days(2))
            .format(REMOVED_STAMP)
            .to_string();
        for n in [&old, &recent, &"notes".to_string()] {
            fs::create_dir_all(d.path().join(n)).unwrap();
        }
        expire_removed(d.path(), 30);
        assert!(!d.path().join(&old).exists());
        assert!(d.path().join(&recent).exists());
        assert!(d.path().join("notes").exists());
    }
}
