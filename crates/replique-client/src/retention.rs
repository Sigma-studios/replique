//! Retention: which snapshots a job's prune run forgets.
//!
//! restic forgets whole snapshots, and a file's data goes with the last snapshot
//! holding it. `keep-deleted` makes sure that snapshot is never the one a
//! deletion on the device takes away: before forgetting, it diffs every
//! snapshot about to go against the next one kept, and tags `replique-keep`
//! those holding a file the kept one no longer has. `forget --keep-tag
//! replique-keep` then keeps them, so only old versions of files that still
//! exist age out. A tagged snapshot keeps everything it holds, old versions
//! included: restic cannot keep part of a snapshot.

use std::io::{BufRead, BufReader};
use std::process::Stdio;

use chrono::Utc;
use replique_proto::{KEEP_TAG, Progress, PruneSummary, Retention};
use serde::Deserialize;

use crate::restic::{ForgetGroup, Outcome, RETRY_LOCK, Restic, Snapshot, exit_code};
use crate::signals::Stop;

/// The snapshots a job's retention looks at: its own paths (the repository may
/// hold other jobs' snapshots, which group separately anyway) and its host when
/// it sets one.
pub fn filters(paths: &[String], host: Option<&str>) -> Vec<String> {
    let mut f = Vec::new();
    for p in paths {
        let clean = if p.len() > 1 {
            p.trim_end_matches('/')
        } else {
            p
        };
        f.push("--path".into());
        f.push(clean.to_string());
    }
    if let Some(h) = host {
        f.push("--host".into());
        f.push(h.to_string());
    }
    f
}

fn lock_args() -> Vec<String> {
    vec!["--retry-lock".into(), RETRY_LOCK.into()]
}

/// Applies the policy. `None` for `never-prune`: there is no run to report.
pub fn apply(
    restic: &Restic,
    retention: &Retention,
    filters: &[String],
    stop: &Stop,
    mut progress: impl FnMut(Progress),
) -> Option<Result<PruneSummary, Outcome>> {
    let (keep, tagging) = match retention {
        Retention::NeverPrune => return None,
        Retention::KeepDeleted { keep } => (keep, true),
        Retention::Versions { keep } => (keep, false),
    };
    let mut keep_args = keep.to_restic_args();
    if tagging {
        keep_args.extend(["--keep-tag".to_string(), KEEP_TAG.to_string()]);
    }
    Some((|| {
        let mut summary = PruneSummary {
            policy: retention.policy_name().to_string(),
            snapshots_before: 0,
            snapshots_removed: 0,
            snapshots_tagged_keep: 0,
            bytes_freed: None,
        };
        if keep.to_restic_args().is_empty() {
            // restic would forget nothing anyway; say why instead.
            crate::log!("retention has no keep rule: nothing is forgotten");
            return Ok(summary);
        }
        if tagging {
            let plan = forget(restic, &keep_args, filters, true)?;
            summary.snapshots_before = plan.iter().map(|g| count(&g.keep) + count(&g.remove)).sum();
            let to_tag = holding_deleted_files(restic, &plan, stop, &mut progress)?;
            if !to_tag.is_empty() {
                let mut args = lock_args();
                args.extend(["tag".into(), "--add".into(), KEEP_TAG.into()]);
                args.extend(to_tag.iter().cloned());
                restic.output(&args)?;
                summary.snapshots_tagged_keep = to_tag.len() as u64;
            }
        }
        if stop.requested() {
            return Err(interrupted());
        }
        progress(phase("forgetting snapshots", 0.0));
        let done = forget(restic, &keep_args, filters, false)?;
        if !tagging {
            summary.snapshots_before = done.iter().map(|g| count(&g.keep) + count(&g.remove)).sum();
        }
        summary.snapshots_removed = done.iter().map(|g| count(&g.remove)).sum();
        if summary.snapshots_removed > 0 && !stop.requested() {
            progress(phase("pruning", 0.0));
            let mut args = lock_args();
            args.push("prune".into());
            let out = restic.output(&args)?;
            summary.bytes_freed = parse_total_prune(&String::from_utf8_lossy(&out));
        }
        Ok(summary)
    })())
}

fn interrupted() -> Outcome {
    Outcome {
        code: 130,
        message: Some("stopped before the prune finished".into()),
        ..Outcome::default()
    }
}

fn count(v: &Option<Vec<Snapshot>>) -> u64 {
    v.as_ref().map_or(0, |v| v.len() as u64)
}

fn phase(label: &str, percent: f64) -> Progress {
    Progress {
        at: Utc::now(),
        percent_done: percent,
        phase: Some(label.into()),
        ..Progress::default()
    }
}

fn forget(
    restic: &Restic,
    keep_args: &[String],
    filters: &[String],
    dry_run: bool,
) -> Result<Vec<ForgetGroup>, Outcome> {
    let mut args = lock_args();
    args.extend(["forget".into(), "--json".into()]);
    if dry_run {
        args.push("--dry-run".into());
    }
    args.extend_from_slice(keep_args);
    args.extend_from_slice(filters);
    let out = restic.output(&args)?;
    let text = String::from_utf8_lossy(&out);
    // Nothing matching the filters: restic prints nothing, or `null`.
    let json = text.lines().rev().find(|l| l.trim_start().starts_with('['));
    match json {
        None => Ok(vec![]),
        Some(j) => serde_json::from_str(j).map_err(|e| Outcome {
            code: 1,
            message: Some(format!("unexpected `restic forget` output: {e}")),
            ..Outcome::default()
        }),
    }
}

/// The removed snapshots of each group that hold a file the next kept snapshot
/// no longer has.
///
/// Newest first, and a snapshot tagged counts as kept for the older ones: a
/// file in an older snapshot that the tagged one still holds is safe, so only
/// the snapshot holding each deleted file's last copy is tagged, not every
/// snapshot before it.
fn holding_deleted_files(
    restic: &Restic,
    plan: &[ForgetGroup],
    stop: &Stop,
    progress: &mut impl FnMut(Progress),
) -> Result<Vec<String>, Outcome> {
    let total: usize = plan
        .iter()
        .map(|g| g.remove.as_ref().map_or(0, Vec::len))
        .sum();
    let mut done = 0usize;
    let mut tag = Vec::new();
    let order = |s: &Snapshot| (s.time, s.id.clone());
    for group in plan {
        let mut kept: Vec<&Snapshot> = group.keep.iter().flatten().collect();
        let mut removed: Vec<&Snapshot> = group.remove.iter().flatten().collect();
        removed.sort_by_key(|s| std::cmp::Reverse(order(s)));
        for r in removed {
            if stop.requested() {
                return Err(interrupted());
            }
            progress(phase(
                "diffing snapshots",
                done as f64 / total.max(1) as f64,
            ));
            done += 1;
            let next = kept
                .iter()
                .filter(|k| order(k) > order(r))
                .min_by_key(|k| order(k));
            let holds = match next {
                // Newer than everything kept (an odd policy): keep it.
                None => true,
                Some(k) => removes_files(restic, &r.id, &k.id)?,
            };
            if holds {
                tag.push(r.id.clone());
                kept.push(r);
            }
        }
    }
    Ok(tag)
}

#[derive(Deserialize)]
struct Change {
    message_type: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    modifier: String,
}

/// Whether going from `from` to `to` removes a file. Removed folders are listed
/// with a trailing slash and their files on their own lines, so only the files
/// count: an empty folder going away is not worth keeping a snapshot for. A
/// file whose type changed (`T`, a file replaced by a folder) is gone too.
fn removes_files(restic: &Restic, from: &str, to: &str) -> Result<bool, Outcome> {
    let mut cmd = restic.command(&["diff", "--json", from, to]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| Outcome {
        code: 127,
        message: Some(format!("cannot run restic: {e}")),
        ..Outcome::default()
    })?;
    let stdout = child.stdout.take().expect("piped");
    let mut found = false;
    // Read to the end even once a removed file is found: killing restic would
    // leave its lock behind, and the `tag` and `prune` after it would wait on
    // that lock for as long as `--retry-lock` allows.
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if found {
            continue;
        }
        let Ok(c) = serde_json::from_str::<Change>(&line) else {
            continue;
        };
        if c.message_type == "change"
            && (c.modifier == "-" || c.modifier.contains('T'))
            && !c.path.ends_with('/')
        {
            found = true;
        }
    }
    let out = child.wait_with_output().map_err(|e| Outcome {
        code: 1,
        message: Some(e.to_string()),
        ..Outcome::default()
    })?;
    let code = exit_code(out.status);
    if code != 0 {
        return Err(Outcome {
            code,
            message: Some(format!(
                "restic diff {from} {to} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            ..Outcome::default()
        });
    }
    Ok(found)
}

/// restic prune has no JSON output; its `total prune:` line says how much it
/// removes (`total prune:  17 blobs / 3.839 KiB`).
fn parse_total_prune(text: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("total prune:"))?;
    let size = line.split('/').nth(1)?.trim();
    parse_size(size)
}

fn parse_size(s: &str) -> Option<u64> {
    let mut it = s.split_whitespace();
    let n: f64 = it.next()?.parse().ok()?;
    let mult = match it.next().unwrap_or("B") {
        "B" => 1u64,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        "PiB" => 1 << 50,
        _ => return None,
    };
    Some((n * mult as f64).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_sizes() {
        let text = "to repack:  5 blobs / 1.603 KiB\nthis removes: 3 blobs / 919 B\ntotal prune:          17 blobs / 3.839 KiB\nremaining: 6 blobs / 1.280 KiB\n";
        assert_eq!(parse_total_prune(text), Some(3931));
        assert_eq!(parse_size("2 GiB"), Some(2 << 30));
        assert_eq!(parse_size("0 B"), Some(0));
        assert_eq!(parse_total_prune("nothing"), None);
    }

    #[test]
    fn filter_args() {
        assert_eq!(
            filters(&["/home/sigma/".into(), "/".into()], Some("pc")),
            ["--path", "/home/sigma", "--path", "/", "--host", "pc"]
        );
    }
}
