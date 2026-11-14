//! What is really stored: the whole box from Hetzner's API, each repository
//! over SFTP through its read-only login, every few minutes.
//!
//! A repository whose size moves while none of its device's runs is going on
//! (or finished since the last look) was changed by something that does not
//! report: a raised drift warning says so.

use std::sync::Arc;
use std::time::Duration;

use crate::App;
use crate::alerts::{self, Raise, WARNING};
use crate::db::{self, Repository};
use crate::events::Event;
use crate::hetzner::StorageBox;

/// What the website shows about the box, refreshed with each measurement.
#[derive(Debug, Clone, Default)]
pub struct BoxState {
    pub info: Option<StorageBox>,
    /// Sub-accounts that exist now, ours or not.
    pub subaccounts_used: Option<u64>,
    pub checked_at: Option<i64>,
    pub error: Option<String>,
}

impl BoxState {
    pub fn subaccounts_remaining(&self) -> Option<u64> {
        let limit = self.info.as_ref()?.storage_box_type.subaccounts_limit;
        Some(limit.saturating_sub(self.subaccounts_used?))
    }
}

/// Reads the box from the API and records its total.
pub async fn refresh_box(app: &Arc<App>) -> anyhow::Result<()> {
    let (hetzner, box_id) = app.hetzner()?;
    let result = async {
        let info = hetzner.storage_box(box_id).await?;
        let subaccounts = hetzner.subaccounts(box_id).await?;
        anyhow::Ok((info, subaccounts.len() as u64))
    }
    .await;
    let now = db::now();
    match result {
        Ok((info, used)) => {
            if let Some(stats) = info.stats {
                app.store
                    .record_measurement("box", now, stats.size as i64, None, "hetzner-api")?;
                app.hub.publish(Event::Measured {
                    scope: "box".into(),
                    at: now,
                    bytes: stats.size as i64,
                });
            }
            let mut state = app.box_state.write();
            state.info = Some(info);
            state.subaccounts_used = Some(used);
            state.checked_at = Some(now);
            state.error = None;
            Ok(())
        }
        Err(e) => {
            let mut state = app.box_state.write();
            state.checked_at = Some(now);
            state.error = Some(format!("{e:#}"));
            Err(e)
        }
    }
}

/// Measures one repository; raises a drift warning if it changed with no run
/// to explain it.
pub async fn measure_repo(app: &Arc<App>, repo: &Repository) -> anyhow::Result<()> {
    let access = app.reader(repo)?;
    let scope = format!("repo:{}", repo.id);
    let previous = app.store.latest_measurement(&scope)?;
    let (bytes, files) = app.measure(&access).await?;
    let now = db::now();
    let source = match access {
        crate::storage::Access::Local { .. } => "local",
        crate::storage::Access::Sftp { .. } => "sftp",
    };
    app.store
        .record_measurement(&scope, now, bytes as i64, Some(files as i64), source)?;
    app.measure_errors.lock().remove(&repo.id);
    app.hub.publish(Event::Measured {
        scope,
        at: now,
        bytes: bytes as i64,
    });
    if let Some(prev) = previous {
        let changed = prev.bytes != bytes as i64 || prev.files != Some(files as i64);
        if changed
            && !app
                .store
                .device_active_between(&repo.device_id, prev.at, now)?
        {
            let device = app.store.device(&repo.device_id)?;
            let name = device
                .map(|d| d.name)
                .unwrap_or_else(|| repo.device_id.clone());
            alerts::raise(
                app,
                Raise {
                    key: format!("drift:{}", repo.id),
                    kind: "drift",
                    severity: WARNING,
                    title: format!("{name}: repository changed outside a reported run"),
                    detail: format!(
                        "Between {} and {} it went from {} in {} files to {} in {} files, with no run of this device in progress or finished. A successful check run clears this.",
                        crate::web::fmt::datetime(prev.at),
                        crate::web::fmt::datetime(now),
                        crate::web::fmt::bytes(prev.bytes),
                        prev.files.unwrap_or(0),
                        crate::web::fmt::bytes(bytes as i64),
                        files
                    ),
                    device_id: Some(&repo.device_id),
                },
            )
            .await;
        }
    }
    Ok(())
}

/// One round: the box, then every repository Réplique can read.
pub async fn measure_all(app: &Arc<App>) {
    if app.hetzner().is_ok()
        && let Err(e) = refresh_box(app).await
    {
        tracing::warn!("reading the Storage Box from Hetzner: {e:#}");
    }
    let repos = match app.store.repositories() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("listing repositories: {e:#}");
            return;
        }
    };
    for repo in repos {
        // A twin can only be read once pairing installed Réplique's key on it.
        let readable = app.config.storage_box.local_root.is_some()
            || app.config.storage_box.measure_via == crate::config::MeasureVia::Main
            || repo.twin_key_installed;
        if !readable {
            continue;
        }
        if let Err(e) = measure_repo(app, &repo).await {
            tracing::warn!(repo = %repo.id, "measuring a repository: {e:#}");
            app.measure_errors
                .lock()
                .insert(repo.id.clone(), format!("{e:#}"));
        }
    }
}

pub async fn run_loop(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(app.config.measurement.every_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        measure_all(&app).await;
    }
}
