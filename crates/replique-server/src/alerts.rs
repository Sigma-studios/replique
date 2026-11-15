//! Alerts: each one a Problem on the website and a Discord message.
//!
//! An alert is keyed by what it is about (`run_failed:<job>:<kind>`,
//! `silent:<job>`, `box_alert`…). Raising one that is already open does
//! nothing, so a job failing every night pings once, not every night; it
//! clears when the condition goes (the next good run, the box back under its
//! threshold) and a recovery message says so, without pinging.

use std::sync::Arc;
use std::time::Duration;

use replique_proto::{FinishRun, JobKind, RunKind, RunStatus};
use serde_json::json;

use crate::App;
use crate::db::{self, Alert, Device, Job, Run};
use crate::events::Event;
use crate::secrets::Secret;

pub const WARNING: &str = "warning";
pub const ALERT: &str = "alert";

pub const SETTING_WEBHOOK: &str = "discord_webhook";
pub const SETTING_NOTIFY_ERROR: &str = "notify_error";
pub const SETTING_NOTIFY_SENT: &str = "notify_sent_at";
pub const SETTING_BOX_WARN: &str = "box_warn_percent";
pub const SETTING_BOX_ALERT: &str = "box_alert_percent";

pub struct Raise<'a> {
    pub key: String,
    pub kind: &'a str,
    pub severity: &'a str,
    pub title: String,
    pub detail: String,
    pub device_id: Option<&'a str>,
}

/// Opens an alert unless it is open already, and tells Discord.
pub async fn raise(app: &Arc<App>, r: Raise<'_>) {
    match app
        .store
        .insert_alert(&r.key, r.kind, r.severity, &r.title, &r.detail, r.device_id)
    {
        Ok(Some(alert)) => {
            tracing::warn!(key = %alert.key, title = %alert.title, "alert");
            app.hub.publish(Event::Problems);
            let text = message_for(app, &alert, false);
            spawn_notify(app, text, true);
        }
        Ok(None) => {}
        Err(e) => tracing::error!("saving an alert: {e:#}"),
    }
}

/// Clears an open alert and sends the recovery message.
pub async fn clear(app: &Arc<App>, key: &str) {
    clear_inner(app, key, true).await
}

/// Clears without a message: the alert was replaced by a graver one about
/// the same thing.
pub async fn clear_quietly(app: &Arc<App>, key: &str) {
    clear_inner(app, key, false).await
}

async fn clear_inner(app: &Arc<App>, key: &str, say: bool) {
    match app.store.clear_alert(key) {
        Ok(Some(alert)) => {
            tracing::info!(key = %alert.key, "alert cleared");
            app.hub.publish(Event::Problems);
            if say {
                let text = message_for(app, &alert, true);
                spawn_notify(app, text, false);
            }
        }
        Ok(None) => {}
        Err(e) => tracing::error!("clearing an alert: {e:#}"),
    }
}

fn message_for(app: &App, alert: &Alert, recovered: bool) -> String {
    let link = match &alert.device_id {
        Some(id) => format!("{}/devices/{id}", app.config.public_url),
        None => format!("{}/problems", app.config.public_url),
    };
    if recovered {
        format!("Resolved: {}\n<{link}>", alert.title)
    } else {
        let marker = if alert.severity == ALERT {
            "Alert"
        } else {
            "Warning"
        };
        let detail = if alert.detail.is_empty() {
            String::new()
        } else {
            format!("\n{}", truncate(&alert.detail, 1500))
        };
        format!("@everyone **{marker}: {}**{detail}\n<{link}>", alert.title)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

pub fn webhook(app: &App) -> Option<Secret> {
    let sealed = app.store.setting(SETTING_WEBHOOK).ok()??;
    app.secrets.open_string(&sealed).ok()
}

fn spawn_notify(app: &Arc<App>, text: String, ping: bool) {
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = notify(&app, &text, ping).await {
            tracing::warn!("Discord: {e:#}");
        }
    });
}

/// Posts to the Discord webhook, recording how it went for the Problems page.
/// `allowed_mentions` makes Discord honour `@everyone` only when we mean it.
pub async fn notify(app: &App, text: &str, ping: bool) -> anyhow::Result<()> {
    let Some(url) = webhook(app) else {
        return Ok(());
    };
    let body = json!({
        "username": "Réplique",
        "content": text,
        "allowed_mentions": { "parse": if ping { vec!["everyone"] } else { vec![] } },
    });
    let mut result = post_webhook(app, url.expose(), &body).await;
    if result.is_err() {
        tokio::time::sleep(Duration::from_secs(2)).await;
        result = post_webhook(app, url.expose(), &body).await;
    }
    match &result {
        Ok(()) => {
            let _ = app.store.delete_setting(SETTING_NOTIFY_ERROR);
            let _ = app
                .store
                .put_setting(SETTING_NOTIFY_SENT, db::now().to_string().as_bytes());
        }
        Err(e) => {
            let _ = app
                .store
                .put_setting(SETTING_NOTIFY_ERROR, format!("{e:#}").as_bytes());
            app.hub.publish(Event::Problems);
        }
    }
    result
}

async fn post_webhook(app: &App, url: &str, body: &serde_json::Value) -> anyhow::Result<()> {
    let response = app.http.post(url).json(body).send().await?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        anyhow::bail!("Discord rate-limited the webhook");
    }
    if !status.is_success() {
        // The URL holds the webhook's secret; only the status is kept.
        anyhow::bail!("Discord answered {status}");
    }
    Ok(())
}

pub fn percent_setting(app: &App, key: &str, default: u8) -> u8 {
    app.store
        .setting_str(key)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .filter(|v: &u8| *v <= 100)
        .unwrap_or(default)
}

fn label(device: &Device, job: &Job) -> String {
    format!("{} · {}", device.name, job.name)
}

fn kind_name(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Backup => "backup",
        RunKind::Check => "check",
        RunKind::Prune => "prune",
        RunKind::Mirror => "mirror",
    }
}

/// A run started: the job is not silent any more.
pub async fn on_run_started(app: &Arc<App>, _device: &Device, job_id: i64) {
    clear(app, &format!("silent:{job_id}")).await;
}

/// Raises or clears the alerts a finished run settles.
pub async fn on_run_finished(
    app: &Arc<App>,
    device: &Device,
    job: &Job,
    run: &Run,
    finish: &FinishRun,
) {
    let kind = kind_name(run.kind);
    let failed_key = format!("run_failed:{}:{kind}", job.id);
    let partial_key = format!("run_partial:{}", job.id);
    let integrity_key = format!("check_failed:{}", job.id);
    clear(app, &format!("lost:{}", job.id)).await;
    let reason = finish
        .message
        .clone()
        .or_else(|| finish.errors.first().map(|e| e.message.clone()))
        .unwrap_or_default();
    let integrity_problem = match run.kind {
        // Only a check that ran says anything about integrity: restic exits 1
        // when it found errors. A check that could not start (locked, wrong
        // password, network) is a failed run, reported as such below, even if
        // restic printed `num_errors: 1` on its way out (0.19 does when locked).
        RunKind::Check => {
            matches!(finish.status, RunStatus::Success | RunStatus::Failed)
                && (finish.exit_code == 1
                    || finish.check.as_ref().is_some_and(|c| c.num_errors > 0))
        }
        RunKind::Mirror => finish
            .mirror
            .as_ref()
            .is_some_and(|m| !m.hash_mismatches.is_empty()),
        _ => false,
    };
    if integrity_problem {
        let detail = match (&finish.check, &finish.mirror) {
            (Some(c), _) if c.num_errors > 0 => {
                format!("restic check found {} errors. {reason}", c.num_errors)
            }
            (_, Some(m)) if !m.hash_mismatches.is_empty() => format!(
                "{} files in the mirror do not match their hash:\n{}",
                m.hash_mismatches.len(),
                m.hash_mismatches
                    .iter()
                    .take(10)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            _ => format!("exit {}: {reason}", finish.exit_code),
        };
        raise(
            app,
            Raise {
                key: integrity_key,
                kind: "integrity",
                severity: ALERT,
                title: format!("{}: integrity check failed", label(device, job)),
                detail,
                device_id: Some(&device.id),
            },
        )
        .await;
        return;
    }
    match finish.status {
        RunStatus::Success => {
            clear(app, &failed_key).await;
            if run.kind == RunKind::Backup {
                clear(app, &partial_key).await;
            }
            if matches!(run.kind, RunKind::Check | RunKind::Mirror) {
                clear(app, &integrity_key).await;
            }
            if run.kind == RunKind::Check
                && let Ok(Some(repo)) = app.store.repository_of(&device.id)
            {
                // A clean check vouches for whatever changed the size.
                clear(app, &format!("drift:{}", repo.id)).await;
            }
        }
        RunStatus::Partial => {
            clear(app, &failed_key).await;
            let paths = if finish.unreadable_paths.is_empty() {
                finish
                    .errors
                    .iter()
                    .filter_map(|e| e.item.clone())
                    .collect::<Vec<_>>()
            } else {
                finish.unreadable_paths.clone()
            };
            let mut detail = paths
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            if paths.len() > 20 {
                detail.push_str(&format!("\n… and {} more", paths.len() - 20));
            }
            raise(
                app,
                Raise {
                    key: partial_key,
                    kind: "run_partial",
                    severity: WARNING,
                    title: format!(
                        "{}: {kind} partial, {} unreadable",
                        label(device, job),
                        plural(finish.error_count.max(paths.len() as u64), "file")
                    ),
                    detail,
                    device_id: Some(&device.id),
                },
            )
            .await;
        }
        // Stopped on purpose (Android's time limit) or by hand: the next run
        // carries on from the parent snapshot.
        RunStatus::Interrupted => {}
        status => {
            let what = match status {
                RunStatus::RepositoryMissing => "repository missing".to_string(),
                RunStatus::LockFailed => "repository locked".to_string(),
                RunStatus::WrongPassword => "wrong repository password".to_string(),
                _ => format!("exit {}", finish.exit_code),
            };
            raise(
                app,
                Raise {
                    key: failed_key,
                    kind: "run_failed",
                    severity: ALERT,
                    title: format!("{}: {kind} failed ({what})", label(device, job)),
                    detail: reason,
                    device_id: Some(&device.id),
                },
            )
            .await;
        }
    }
}

pub fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// How long between two runs of a job is normal: the admin's override, else
/// what the device declared, else what its schedule implies. `None` for a
/// manual job or a schedule too unusual to read.
pub fn expected_interval(job: &Job) -> Option<i64> {
    job.interval_override_secs
        .or(job.expected_interval_secs)
        .or_else(|| job.schedule.as_deref().and_then(interval_of_schedule))
        .filter(|s| *s > 0)
}

/// Reads the common systemd `OnCalendar` shorthands and `every <n><unit>`.
pub fn interval_of_schedule(schedule: &str) -> Option<i64> {
    let s = schedule.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_prefix("every ") {
        let rest = rest.trim();
        let (num, unit) = rest.split_at(rest.find(|c: char| !c.is_ascii_digit())?);
        let n: i64 = num.parse().ok()?;
        let unit = match unit.trim() {
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 7 * 86_400,
            _ => return None,
        };
        return Some(n * unit);
    }
    match s.as_str() {
        "minutely" => Some(60),
        "hourly" => Some(3600),
        "daily" => Some(86_400),
        "weekly" => Some(7 * 86_400),
        "monthly" => Some(31 * 86_400),
        "quarterly" => Some(92 * 86_400),
        "semiannually" | "semi-annually" => Some(184 * 86_400),
        "yearly" | "annually" => Some(366 * 86_400),
        _ => {
            // `*-*-* 03:00` and `*-*-* 03:00:00`: once a day.
            let parts: Vec<&str> = s.split_whitespace().collect();
            match parts.as_slice() {
                ["*-*-*", time]
                    if time
                        .split(':')
                        .all(|p| p.chars().all(|c| c.is_ascii_digit())) =>
                {
                    Some(86_400)
                }
                [days, "*-*-*", _] if days.chars().all(|c| c.is_ascii_alphabetic()) => {
                    Some(7 * 86_400)
                }
                _ => None,
            }
        }
    }
}

/// When a job with this interval counts as silent: the interval plus a
/// margin for jitter and a late start (`daily` → 26 h).
pub fn silent_after(interval: i64) -> i64 {
    interval + (interval / 12).max(2 * 3600)
}

/// The periodic part: lost runs, silent jobs, the box filling up.
pub async fn evaluate(app: &Arc<App>) -> anyhow::Result<()> {
    let now = db::now();
    let lost_after = app.config.alerts.lost_after_secs as i64;
    for run in app.store.running_runs()? {
        let last_news = run
            .progress_at
            .unwrap_or(0)
            .max(run.received_at)
            .max(run.started_at);
        let estimate = run
            .progress
            .as_ref()
            .and_then(|p| p.seconds_remaining)
            .map(|s| (s as i64) * 3 / 2)
            .unwrap_or(0);
        if now - last_news <= lost_after.max(estimate) {
            continue;
        }
        app.store.mark_run_lost(&run.id)?;
        let (Some(device), Some(job)) = (
            app.store.device(&run.device_id)?,
            app.store.job(run.job_id)?,
        ) else {
            continue;
        };
        raise(
            app,
            Raise {
                key: format!("lost:{}", job.id),
                kind: "run_lost",
                severity: WARNING,
                title: format!("{}: {} run lost", label(&device, &job), kind_name(run.kind)),
                detail: format!(
                    "Started {}; no news since {}. The device may have shut down mid-run.",
                    crate::web::fmt::datetime(run.started_at),
                    crate::web::fmt::datetime(last_news)
                ),
                device_id: Some(&device.id),
            },
        )
        .await;
        app.hub.publish(Event::Runs {
            device_id: device.id.clone(),
        });
    }

    for device in app.store.devices()? {
        if device.paired_at.is_none() || device.revoked_at.is_some() {
            continue;
        }
        for job in app.store.jobs_of(&device.id)? {
            let key = format!("silent:{}", job.id);
            let Some(interval) = expected_interval(&job) else {
                clear(app, &key).await;
                continue;
            };
            let main_kind = if job.kind == JobKind::Mirror {
                RunKind::Mirror
            } else {
                RunKind::Backup
            };
            let last = app
                .store
                .runs_of_job(job.id, 50)?
                .into_iter()
                .find(|r| r.kind == main_kind)
                .map(|r| r.started_at)
                .unwrap_or(job.declared_at.unwrap_or(job.created_at));
            if now - last > silent_after(interval) {
                raise(
                    app,
                    Raise {
                        key,
                        kind: "silent",
                        severity: ALERT,
                        title: format!(
                            "{}: no {} for {}",
                            label(&device, &job),
                            kind_name(main_kind),
                            crate::web::fmt::duration(now - last)
                        ),
                        detail: format!(
                            "Expected every {}; the last one started {}.",
                            crate::web::fmt::duration(interval),
                            crate::web::fmt::datetime(last)
                        ),
                        device_id: Some(&device.id),
                    },
                )
                .await;
            } else {
                clear(app, &key).await;
            }
        }
    }

    let usage = {
        let state = app.box_state.read();
        state.info.as_ref().and_then(|b| {
            let used = b.stats?.size;
            let quota = b.storage_box_type.size;
            (quota > 0).then_some((used, quota))
        })
    };
    if let Some((used, quota)) = usage {
        let percent = used as f64 * 100.0 / quota as f64;
        let warn =
            percent_setting(app, SETTING_BOX_WARN, app.config.alerts.box_warn_percent) as f64;
        let alert =
            percent_setting(app, SETTING_BOX_ALERT, app.config.alerts.box_alert_percent) as f64;
        let detail = format!(
            "{} used of {}.",
            crate::web::fmt::bytes(used as i64),
            crate::web::fmt::bytes(quota as i64)
        );
        if percent >= alert {
            clear_quietly(app, "box_warn").await;
            raise(
                app,
                Raise {
                    key: "box_alert".into(),
                    kind: "box_usage",
                    severity: ALERT,
                    title: format!("Storage Box {percent:.0}% full"),
                    detail,
                    device_id: None,
                },
            )
            .await;
        } else {
            clear(app, "box_alert").await;
            if percent >= warn {
                raise(
                    app,
                    Raise {
                        key: "box_warn".into(),
                        kind: "box_usage",
                        severity: WARNING,
                        title: format!("Storage Box {percent:.0}% full"),
                        detail,
                        device_id: None,
                    },
                )
                .await;
            } else {
                clear(app, "box_warn").await;
            }
        }
    }
    Ok(())
}

pub async fn run_loop(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = evaluate(&app).await {
            tracing::error!("checking alerts: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules() {
        assert_eq!(interval_of_schedule("daily"), Some(86_400));
        assert_eq!(interval_of_schedule("*-*-* 03:00"), Some(86_400));
        assert_eq!(interval_of_schedule("every 6h"), Some(6 * 3600));
        assert_eq!(interval_of_schedule("every 30 min"), Some(1800));
        assert_eq!(interval_of_schedule("Mon *-*-* 04:00"), Some(7 * 86_400));
        assert_eq!(interval_of_schedule("Mon..Fri 9:00"), None);
        assert_eq!(silent_after(86_400), 26 * 3600);
    }
}
