//! Files a phone watches the age of (`DeviceConfig::watched`): Aegis's
//! automatic backups, typically.
//!
//! The phone's backup already carries those files to the box; what it cannot
//! tell is whether the other app still writes them. Aegis writes a backup when
//! its vault changes, to a folder it may lose access to (the folder deleted or
//! renamed, its permission gone after a restore), and says so only inside
//! Aegis. So after each backup the app reports how many matching files the
//! folder holds and when the newest was written, and a warning stays open
//! while there are none or the newest is older than the entry allows. For
//! Aegis an old backup is usually a vault that did not change, and "Trigger
//! backup" in its settings settles it: a fresh file, and proof the folder is
//! still writable.

use std::sync::Arc;

use replique_proto::{DeviceConfig, DeviceKind, RunKind, WatchedFiles, WatchedReport};

use crate::App;
use crate::alerts::{self, Raise};
use crate::db;

const DAY: i64 = 86_400;

/// What the phone said in its newest finished backup: when, and the reports.
pub fn latest(app: &App, device_id: &str) -> Option<(i64, Vec<WatchedReport>)> {
    let runs = app.store.runs_of_device(device_id, 50).ok()?;
    runs.into_iter()
        .filter(|r| r.kind == RunKind::Backup)
        .find_map(|r| Some((r.finished_at?, r.finish?.watched)))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// No backup has reported this entry yet (it is new, or the app is older).
    Waiting,
    NoFolder {
        at: i64,
    },
    NoFiles {
        at: i64,
    },
    Old {
        newest: i64,
        files: u32,
        at: i64,
    },
    Fresh {
        newest: i64,
        files: u32,
        at: i64,
    },
}

pub fn status(watch: &WatchedFiles, latest: Option<&(i64, Vec<WatchedReport>)>) -> Status {
    let Some((at, reports)) = latest else {
        return Status::Waiting;
    };
    let at = *at;
    let Some(r) = reports.iter().find(|r| r.name == watch.name) else {
        return Status::Waiting;
    };
    if !r.folder_found {
        return Status::NoFolder { at };
    }
    match r.newest.map(|t| t.timestamp()) {
        None => Status::NoFiles { at },
        Some(newest) if db::now() - newest > i64::from(watch.max_age_days) * DAY => Status::Old {
            newest,
            files: r.files,
            at,
        },
        Some(newest) => Status::Fresh {
            newest,
            files: r.files,
            at,
        },
    }
}

/// Whether the phone's backup covers the folder: inside one of its folders,
/// and not under an absolute exclude. (A relative pattern could still match
/// it; that is rarer and left to the eye.)
pub fn covered(config: &DeviceConfig, folder: &str) -> bool {
    let inside = |path: &str, root: &str| {
        let root = root.trim_end_matches('/');
        root.is_empty() || path == root || path.starts_with(&format!("{root}/"))
    };
    let folder = folder.trim_end_matches('/');
    config.folders.iter().any(|f| inside(folder, &f.path))
        && !config
            .excludes
            .iter()
            .chain(config.folders.iter().flat_map(|f| f.excludes.iter()))
            .any(|e| e.starts_with('/') && !e.contains(['*', '?', '[']) && inside(folder, e))
}

fn alert_key(device_id: &str, name: &str) -> String {
    format!("watched:{device_id}:{name}")
}

/// What to do about an entry gone wrong, for the alert and the website.
pub fn advice(watch: &WatchedFiles) -> &'static str {
    if watch.pattern.starts_with("aegis-backup") {
        "Aegis writes a backup only when its vault changes. Open Aegis → Settings → Backups: check the folder, then “Trigger backup”. That writes a fresh one and proves the folder is still writable. This clears after the phone's next backup."
    } else {
        "Check that the app writing them still does, to this folder. This clears after the phone's next backup."
    }
}

/// Raises or clears a warning per watched entry of every phone.
pub async fn check(app: &Arc<App>) -> anyhow::Result<()> {
    let mut wanted = Vec::new();
    for device in app.store.devices()? {
        if device.kind != DeviceKind::Android {
            continue;
        }
        let Some((config, _)) = app.store.device_config(&device.id)? else {
            continue;
        };
        if config.watched.is_empty() {
            continue;
        }
        let latest = latest(app, &device.id);
        for watch in &config.watched {
            let key = alert_key(&device.id, &watch.name);
            wanted.push(key.clone());
            let where_ = format!("{}/{}", watch.folder.trim_end_matches('/'), watch.pattern);
            let problem = match status(watch, latest.as_ref()) {
                Status::Waiting => continue,
                Status::Fresh { .. } => {
                    alerts::clear(app, &key).await;
                    continue;
                }
                Status::NoFolder { at } => (
                    format!("{}: no {} found", device.name, watch.name),
                    format!(
                        "The folder {} did not exist at the backup of {}.",
                        watch.folder,
                        crate::web::fmt::datetime(at)
                    ),
                ),
                Status::NoFiles { at } => (
                    format!("{}: no {} found", device.name, watch.name),
                    format!(
                        "No file matching {where_} at the backup of {}.",
                        crate::web::fmt::datetime(at)
                    ),
                ),
                Status::Old { newest, .. } => (
                    format!(
                        "{}: no new {} for over {} days",
                        device.name, watch.name, watch.max_age_days
                    ),
                    format!(
                        "The newest file matching {where_} was written {}.",
                        crate::web::fmt::datetime(newest)
                    ),
                ),
            };
            // A different problem with the same files replaces the open one.
            if let Some(open) = app.store.open_alert(&key)?
                && open.title != problem.0
            {
                alerts::clear_quietly(app, &key).await;
            }
            alerts::raise(
                app,
                Raise {
                    key,
                    kind: "watched",
                    severity: alerts::WARNING,
                    title: problem.0,
                    detail: format!("{} {}", problem.1, advice(watch)),
                    device_id: Some(&device.id),
                },
            )
            .await;
        }
    }
    // Entries removed from the rules (or their phone gone) take their
    // warning with them.
    for a in app.store.open_alerts()? {
        if a.key.starts_with("watched:") && !wanted.contains(&a.key) {
            alerts::clear_quietly(app, &a.key).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use replique_proto::FolderRule;

    #[test]
    fn coverage() {
        let mut c = DeviceConfig::android_default();
        assert!(covered(&c, "/storage/emulated/0/Aegis"));
        assert!(!covered(&c, "/storage/emulated/0/Android/data/x"));
        c.folders = vec![FolderRule {
            path: "/storage/emulated/0/DCIM".into(),
            ..FolderRule::default()
        }];
        assert!(!covered(&c, "/storage/emulated/0/Aegis"));
        assert!(!covered(&c, "/storage/emulated/0/DCIMx"));
        assert!(covered(&c, "/storage/emulated/0/DCIM/"));
    }

    #[test]
    fn statuses() {
        let w = WatchedFiles::aegis();
        let report = |found, files, age_days: Option<i64>| {
            (
                db::now(),
                vec![WatchedReport {
                    name: w.name.clone(),
                    folder_found: found,
                    files,
                    newest: age_days
                        .map(|d| chrono::DateTime::from_timestamp(db::now() - d * DAY, 0).unwrap()),
                }],
            )
        };
        assert_eq!(status(&w, None), Status::Waiting);
        assert!(matches!(
            status(&w, Some(&report(false, 0, None))),
            Status::NoFolder { .. }
        ));
        assert!(matches!(
            status(&w, Some(&report(true, 0, None))),
            Status::NoFiles { .. }
        ));
        assert!(matches!(
            status(&w, Some(&report(true, 3, Some(31)))),
            Status::Old { .. }
        ));
        assert!(matches!(
            status(&w, Some(&report(true, 3, Some(2)))),
            Status::Fresh { .. }
        ));
        let other = (db::now(), vec![]);
        assert_eq!(status(&w, Some(&other)), Status::Waiting);
    }
}
