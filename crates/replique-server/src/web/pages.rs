//! The pages: overview, devices, storage, problems, settings.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Form, Path, Query, State};
use axum::http::HeaderValue;
use axum::response::{Html, IntoResponse, Redirect};
use maud::{Markup, html};
use replique_proto::{
    Conditions, DeviceConfig, DeviceKind, FolderRule, JobKind, PairingLink, Retention, RunKind,
    RunStatus,
};
use serde::Deserialize;

use super::{Ctx, Flash, WebError, WebResult, bare, csrf_field, flash, fmt, page, redirect_with};
use crate::alerts::{self, ALERT};
use crate::api::KINDS;
use crate::auth::Admin;
use crate::config::{KeyInstall, MeasureVia};
use crate::db::{self, Device, Job, Repository, Run};
use crate::provision;
use crate::{App, SETTING_BOX_ID, SETTING_HETZNER_TOKEN, TokenSource};

// ---------------------------------------------------------------- helpers

pub fn status_view(status: RunStatus) -> (&'static str, &'static str) {
    match status {
        RunStatus::Running => ("running", "Running"),
        RunStatus::Success => ("good", "Success"),
        RunStatus::Partial => ("warning", "Partial"),
        RunStatus::Interrupted => ("serious", "Interrupted"),
        RunStatus::Lost => ("serious", "Lost"),
        RunStatus::RepositoryMissing => ("critical", "No repository"),
        RunStatus::LockFailed => ("critical", "Locked"),
        RunStatus::WrongPassword => ("critical", "Wrong password"),
        RunStatus::Failed => ("critical", "Failed"),
    }
}

fn status_chip(status: RunStatus) -> Markup {
    let (class, label) = status_view(status);
    html! { span class={ "status " (class) } { (label) } }
}

fn kind_label(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Nixos => "NixOS",
        DeviceKind::Android => "Android",
        DeviceKind::Facade => "façade",
        DeviceKind::Other => "Other",
    }
}

fn run_kind(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Backup => "backup",
        RunKind::Check => "check",
        RunKind::Prune => "prune",
        RunKind::Mirror => "mirror",
    }
}

fn device_or_404(app: &App, id: &str) -> Result<Device, WebError> {
    app.store.device(id)?.ok_or_else(WebError::not_found)
}

struct JobView {
    job: Job,
    running: Option<Run>,
    last: Option<Run>,
}

fn job_views(app: &App, device_id: &str) -> anyhow::Result<Vec<JobView>> {
    let mut out = Vec::new();
    for job in app.store.jobs_of(device_id)? {
        let runs = app.store.runs_of_job(job.id, 20)?;
        let running = runs
            .iter()
            .find(|r| r.status == RunStatus::Running)
            .cloned();
        let last = runs
            .iter()
            .find(|r| r.status != RunStatus::Running)
            .cloned();
        out.push(JobView { job, running, last });
    }
    Ok(out)
}

fn progress_block(run: &Run) -> Markup {
    let p = run.progress.clone().unwrap_or_default();
    let pct = p.percent_done.clamp(0.0, 1.0);
    // Some runs cannot say how far along they are (a structure-only check, a
    // prune, a run that has not reported yet): a bar frozen at 0% reads as
    // stuck, so they get a moving bar, their phase and how long it has run.
    let unknown =
        run.progress.is_none() || (pct == 0.0 && p.total_bytes == 0 && p.total_files == 0);
    if unknown {
        let what = p.phase.clone().unwrap_or_else(|| match run.kind {
            RunKind::Check => "checking the repository".into(),
            RunKind::Prune => "pruning".into(),
            RunKind::Mirror => "copying".into(),
            RunKind::Backup => "starting".into(),
        });
        return html! {
            div.progress.indeterminate role="progressbar" aria-label=(what) { span {} }
            div.small.muted { (what) " · started " (fmt::ago(run.started_at)) }
        };
    }
    html! {
        div.progress role="progressbar" aria-valuenow=(format!("{:.0}", pct * 100.0)) aria-valuemin="0" aria-valuemax="100" {
            span style={ "width: " (format!("{:.1}", pct * 100.0)) "%" } {}
        }
        div.small.muted {
            (fmt::percent(pct))
            @if p.total_bytes > 0 {
                " · " (fmt::bytes(p.bytes_done as i64)) " of " (fmt::bytes(p.total_bytes as i64))
            }
            @if let Some(rate) = p.upload_bytes_per_sec {
                " · ↑ " (fmt::bytes(rate as i64)) "/s"
                @if let Some(sent) = p.bytes_uploaded { " (" (fmt::bytes(sent as i64)) " sent)" }
            } @else if p.seconds_elapsed > 0 && p.bytes_done > 0 {
                // Without a network count (the PC's restic, a mirror), what it
                // read per second on average: an upper bound for the upload.
                " · " (fmt::bytes((p.bytes_done / p.seconds_elapsed) as i64)) "/s read"
            }
            @if let Some(left) = p.seconds_remaining {
                " · ETA " (fmt::duration(left as i64))
            }
            @if let Some(phase) = &p.phase { " · " (phase) }
            @if p.error_count > 0 { " · " (alerts::plural(p.error_count, "error")) }
            @if run.progress.is_none() { " · started " (fmt::ago(run.started_at)) }
        }
        @if let Some(file) = p.current_files.first() {
            div.small.muted.mono style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title=(file) { (file) }
        }
    }
}

fn last_run_line(run: &Run) -> Markup {
    let took = run.finished_at.map(|f| f - run.started_at);
    let added = run
        .finish
        .as_ref()
        .and_then(|f| f.summary.as_ref())
        .map(|s| s.data_added_packed);
    html! {
        span.small.muted {
            (run_kind(run.kind)) " " (fmt::ago(run.finished_at.unwrap_or(run.started_at)))
            @if let Some(t) = took { " · took " (fmt::duration(t)) }
            @if let Some(a) = added { " · +" (fmt::bytes(a as i64)) }
        }
    }
}

fn repo_size(app: &App, repo: Option<&Repository>) -> Markup {
    let Some(repo) = repo else { return html! {} };
    let point = app
        .store
        .latest_measurement(&format!("repo:{}", repo.id))
        .ok()
        .flatten();
    let error = app.measure_errors.lock().get(&repo.id).cloned();
    html! {
        @match point {
            Some(p) => {
                span { strong { (fmt::bytes(p.bytes)) } }
                span.muted.small { (p.files.unwrap_or(0)) " files · measured " (fmt::ago(p.at)) }
            }
            None => span.muted.small { "Not measured yet" }
        }
        @if let Some(e) = error {
            span.small title=(e) { span.status.serious { "Measurement failing" } }
        }
    }
}

// ---------------------------------------------------------------- overview

fn box_card(app: &App) -> Markup {
    let state = app.box_state.read().clone();
    let warn = alerts::percent_setting(
        app,
        alerts::SETTING_BOX_WARN,
        app.config.alerts.box_warn_percent,
    );
    let alert = alerts::percent_setting(
        app,
        alerts::SETTING_BOX_ALERT,
        app.config.alerts.box_alert_percent,
    );
    // What fills the box: each device's repository as last measured, in the
    // colour that device has in every chart (its place in the device list),
    // then whatever else is there (files outside Réplique's repositories, the
    // box's own snapshots).
    let devices = app.store.devices().unwrap_or_default();
    let mut parts: Vec<(String, usize, i64)> = Vec::new();
    for (slot, device) in devices.iter().enumerate() {
        let Some(repo) = app.store.repository_of(&device.id).ok().flatten() else {
            continue;
        };
        if let Some(p) = app
            .store
            .latest_measurement(&format!("repo:{}", repo.id))
            .ok()
            .flatten()
            && p.bytes > 0
        {
            parts.push((device.name.clone(), slot % 8 + 1, p.bytes));
        }
    }
    html! {
        div.card {
            div.spread {
                h3 { "Storage Box" }
                @if let Some(info) = &state.info { span.muted.small { (info.name) " · " (info.storage_box_type.name.to_uppercase()) } }
            }
            @match state.info.as_ref().and_then(|b| b.stats.map(|s| (s, b.storage_box_type.size))) {
                Some((stats, quota)) if quota > 0 => {
                    @let fraction = stats.size as f64 / quota as f64;
                    @let used = stats.size as i64;
                    @let measured: i64 = parts.iter().map(|p| p.2).sum();
                    // Measurements run on their own schedule and count file
                    // sizes, so they can briefly add up to more than the API's
                    // figure: scale them to it rather than overflow the bar.
                    @let scale = if measured > used && measured > 0 { used as f64 / measured as f64 } else { 1.0 };
                    @let other = (used - measured).max(0);
                    @let width = |bytes: i64, s: f64| format!("{:.2}", (bytes as f64 * s / quota as f64 * 100.0).min(100.0));
                    div.row {
                        span.hero { (fmt::percent(fraction)) }
                        span.muted { (fmt::bytes(used)) " of " (fmt::bytes(quota as i64)) }
                        @if fraction * 100.0 >= alert as f64 { span.status.critical { "Above " (alert) "%" } }
                        @else if fraction * 100.0 >= warn as f64 { span.status.warning { "Above " (warn) "%" } }
                    }
                    div.gauge.stacked role="meter" aria-valuenow=(format!("{:.0}", fraction * 100.0)) aria-valuemin="0" aria-valuemax="100" aria-label="Storage Box usage" {
                        @for (name, slot, bytes) in &parts {
                            span style={ "width: " (width(*bytes, scale)) "%; background: var(--series-" (slot) ")" } title={ (name) ": " (fmt::bytes(*bytes)) } {}
                        }
                        @if other > 0 {
                            span.other style={ "width: " (width(other, 1.0)) "%" } title={ "Other: " (fmt::bytes(other)) " (files outside Réplique's repositories, and the box's own snapshots)" } {}
                        }
                    }
                    div.legend.small {
                        @for (name, slot, bytes) in &parts {
                            span { i.swatch style={ "background: var(--series-" (slot) ")" } {} (name) " " span.muted { (fmt::bytes(*bytes)) } }
                        }
                        @if other > 0 {
                            span title="Files outside Réplique's repositories, and the box's own snapshots" { i.swatch.other {} "Other " span.muted { (fmt::bytes(other)) } }
                        }
                        span { i.swatch.free {} "Free " span.muted { (fmt::bytes((quota as i64 - used).max(0))) } }
                    }
                    div.small.muted {
                        "Data " (fmt::bytes(stats.size_data as i64)) " · snapshots " (fmt::bytes(stats.size_snapshots as i64))
                        @if let Some(left) = state.subaccounts_remaining() { " · " (left) " sub-accounts left" }
                        @if let Some(at) = state.checked_at { " · checked " (fmt::ago(at)) }
                    }
                }
                _ => {
                    p.muted {
                        @if let Some(e) = &state.error { "Hetzner's API failed: " (e) }
                        @else if app.hetzner().is_err() { "Add the Hetzner token and the box id in " a href="/settings" { "Settings" } " to see the box." }
                        @else { "Waiting for the first measurement." }
                    }
                }
            }
        }
    }
}

fn device_card(app: &App, device: &Device) -> Markup {
    let repo = app.store.repository_of(&device.id).ok().flatten();
    let jobs = job_views(app, &device.id).unwrap_or_default();
    html! {
        div.card {
            div.spread {
                h3 { a href={ "/devices/" (device.id) } { (device.name) } }
                span.muted.small { (kind_label(device.kind)) }
            }
            @if device.paired_at.is_none() {
                p { span.status.warning { "Waiting for pairing" } " " a href={ "/devices/" (device.id) } { "Show the code" } }
            } @else if device.revoked_at.is_some() {
                p { span.status.critical { "Token revoked" } }
            }
            div.row { (repo_size(app, repo.as_ref())) }
            @if jobs.is_empty() && device.paired_at.is_some() {
                p.muted.small { "No job declared yet." }
            }
            @for v in &jobs {
                div style="margin-top: 0.6rem" {
                    div.spread {
                        strong { (v.job.name) }
                        @if let Some(r) = &v.running { (status_chip(r.status)) }
                        @else if let Some(r) = &v.last { (status_chip(r.status)) }
                        @else { span.status { "Never run" } }
                    }
                    @if let Some(r) = &v.running { (progress_block(r)) }
                    @if let Some(r) = &v.last { div { (last_run_line(r)) } }
                }
            }
        }
    }
}

fn overview_grid(app: &App) -> anyhow::Result<Markup> {
    let devices = app.store.devices()?;
    Ok(html! {
        div.grid #devices hx-get="/fragments/overview" hx-trigger="live:runs from:body, live:devices from:body, live:measured from:body, every 60s" hx-swap="outerHTML" {
            (box_card(app))
            @for d in &devices { (device_card(app, d)) }
        }
    })
}

pub async fn overview(
    State(app): State<Arc<App>>,
    admin: Admin,
    Query(f): Query<Flash>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let devices = app.store.devices()?;
    let grid = overview_grid(&app)?;
    let problems = app.store.open_alerts()?;
    Ok(page(
        "Aperçu",
        "overview",
        &ctx,
        html! {
            (flash(&f))
            @if app.admin_hash.is_none() {
                div.notice.error { "No admin password is configured; set admin_password_hash_file." }
            }
            @if !problems.is_empty() {
                div.notice.error {
                    a href="/problems" { (alerts::plural(problems.len() as u64, "problem")) }
                    ": " (problems[0].title)
                }
            }
            @if devices.is_empty() {
                div.notice { "No device yet. " a href="/devices/new" { "Add the first one" } "." }
            }
            (grid)
        },
    )
    .into_response())
}

pub async fn overview_fragment(State(app): State<Arc<App>>, _admin: Admin) -> WebResult {
    Ok(Html(overview_grid(&app)?.into_string()).into_response())
}

// ---------------------------------------------------------------- add device

fn add_form(ctx: &Ctx, app: &App, error: Option<&str>, name: &str) -> Html<String> {
    let remaining = app.box_state.read().subaccounts_remaining();
    page(
        "Add device",
        "add",
        ctx,
        html! {
            h1 { "Add a device" }
            @if let Some(e) = error { div.notice.error { (e) } }
            div.card {
                form.stack method="post" action="/devices" data-busy="Creating the sub-accounts on the Storage Box… (about 30 s)" {
                    (csrf_field(ctx))
                    label { "Name" input type="text" name="name" required maxlength="64" value=(name) placeholder="PC, Téléphone…"; }
                    label { "Kind"
                        select name="kind" {
                            @for k in KINDS { option value=(k.as_str()) { (kind_label(k)) } }
                        }
                    }
                    details {
                        summary { "Adopt an existing sub-account instead" }
                        label style="margin-top: 0.5rem" { "Sub-account username"
                            span.hint { "e.g. u123456-sub1: kept as is, with its keys; Réplique adds a read-only twin, and changes the password once (nobody uses it) to write the twin's key." }
                            input type="text" name="adopt" placeholder="u123456-sub1";
                        }
                    }
                    p.small.muted {
                        "Réplique creates the device's sub-account (home "
                        code { "replique/" (app.owner.slug) "/<device>" }
                        ", SSH only)"
                        @if app.config.storage_box.measure_via == MeasureVia::Twin { " and its read-only twin" }
                        ", then shows a pairing code valid 15 minutes."
                        @if let Some(r) = remaining { " " (r) " sub-accounts left on the box." }
                    }
                    div { button.primary type="submit" { "Create and show the pairing code" } }
                }
            }
        },
    )
}

pub async fn add_device_form(State(app): State<Arc<App>>, admin: Admin) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    Ok(add_form(&ctx, &app, None, "").into_response())
}

#[derive(Deserialize)]
pub struct AddDevice {
    name: String,
    kind: DeviceKind,
    adopt: Option<String>,
}

pub async fn add_device(
    State(app): State<Arc<App>>,
    admin: Admin,
    Form(form): Form<AddDevice>,
) -> WebResult {
    let adopt = form
        .adopt
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty());
    match provision::add_device(
        &app,
        provision::NewDevice {
            name: form.name.clone(),
            kind: form.kind,
            adopt,
        },
    )
    .await
    {
        Ok(id) => Ok(Redirect::to(&format!("/devices/{id}")).into_response()),
        Err(e) => {
            tracing::warn!("adding a device: {e:#}");
            let ctx = Ctx::new(&app, &admin);
            Ok(add_form(&ctx, &app, Some(&format!("{e:#}")), &form.name).into_response())
        }
    }
}

// ---------------------------------------------------------------- device

fn pairing_block(app: &App, ctx: &Ctx, device: &Device) -> anyhow::Result<Markup> {
    let code = app.store.live_pairing_code(&device.id)?;
    Ok(html! {
        div.card #pairing hx-get={ "/fragments/devices/" (device.id) "/pairing" } hx-trigger="live:devices from:body, every 20s" hx-swap="outerHTML" {
            h2 style="margin-top:0" { "Pair this device" }
            @match code {
                Some(code) => {
                    @let shown = provision::display_code(&code.code);
                    @let link = PairingLink { server: app.config.public_url.clone(), code: shown.clone() }.to_uri();
                    @let cli = format!("replique-client pair {} {}", app.config.public_url, shown);
                    div.row style="align-items: flex-start" {
                        div.qr { (super::qr_svg(&link)) }
                        div style="flex: 1; min-width: 240px" {
                            p { "Code " strong.mono style="font-size: 1.3rem" { (shown) } }
                            p.small.muted { "Valid until " (fmt::datetime(code.expires_at)) " (" (fmt::ago(code.expires_at)) ")." }
                            @if device.kind == DeviceKind::Android {
                                p { "Scan the QR code with the Réplique app." }
                            } @else {
                                p { "On the device, run:" }
                            }
                            div.copy { code { (cli) } button type="button" data-copy=(cli) { "Copy" } }
                            p.small.muted { "The device generates its SSH key and its restic password itself; save that password in your password manager when it shows it." }
                        }
                    }
                }
                None => {
                    p.muted { "No valid code." }
                }
            }
            form method="post" action={ "/devices/" (device.id) "/pairing-code" } style="margin-top: 0.8rem" data-busy="Setting a fresh password on the Storage Box… (about 15 s)" {
                (csrf_field(ctx))
                button type="submit" { "New pairing code" }
            }
        }
    })
}

pub async fn pairing_fragment(
    State(app): State<Arc<App>>,
    admin: Admin,
    Path(id): Path<String>,
) -> WebResult {
    let device = device_or_404(&app, &id)?;
    if device.paired_at.is_some() {
        // Paired while the page was open: reload it to show the device.
        let mut r = Html(String::new()).into_response();
        r.headers_mut()
            .insert("HX-Refresh", HeaderValue::from_static("true"));
        return Ok(r);
    }
    let ctx = Ctx::new(&app, &admin);
    Ok(Html(pairing_block(&app, &ctx, &device)?.into_string()).into_response())
}

fn rules_view(job: &Job) -> Markup {
    let r = &job.rules;
    html! {
        dl.kv.small {
            @if !r.paths.is_empty() { dt { "Paths" } dd.mono { @for p in &r.paths { (p) br; } } }
            @if !r.excludes.is_empty() {
                dt { "Excludes" }
                dd.mono {
                    @if r.excludes.len() > 8 {
                        details { summary { (r.excludes.len()) " patterns" } @for e in &r.excludes { (e) br; } }
                    } @else { @for e in &r.excludes { (e) br; } }
                }
            }
            @if r.exclude_caches { dt { "Caches" } dd { "folders with CACHEDIR.TAG skipped" } }
            @if !r.generated_excludes_for.is_empty() { dt { "Build output" } dd.mono { "scanned under " (r.generated_excludes_for.join(", ")) } }
            @if let Some(ret) = &r.retention {
                dt { "Retention" }
                dd { (ret.policy_name())
                    @match ret {
                        Retention::KeepDeleted { keep } | Retention::Versions { keep } => { " · " code { (keep.to_restic_args().join(" ")) } }
                        Retention::NeverPrune => {}
                    }
                }
            }
            @if let Some(c) = &r.check_subset { dt { "Check" } dd { "reads back " (c) " after each backup" } }
            @if let Some(m) = &r.mirror_source { dt { "Mirrors" } dd.mono { (m) } }
            @if let Some(m) = &r.mirror_path { dt { "Copy at" } dd.mono { (m) } }
        }
    }
}

fn device_live(app: &App, ctx: &Ctx, device: &Device) -> anyhow::Result<Markup> {
    let jobs = job_views(app, &device.id)?;
    let runs = app.store.runs_of_device(&device.id, 40)?;
    let job_names: std::collections::HashMap<i64, String> = jobs
        .iter()
        .map(|v| (v.job.id, v.job.name.clone()))
        .collect();
    Ok(html! {
        div #live hx-get={ "/fragments/devices/" (device.id) } hx-trigger="live:runs from:body, every 60s" hx-swap="outerHTML" {
            h2 { "Jobs" }
            @if jobs.is_empty() { p.muted { "No job declared yet: the device declares its jobs when it first runs." } }
            div.grid {
                @for v in &jobs {
                    div.card {
                        div.spread {
                            h3 { (v.job.name) " " span.muted.small { (if v.job.kind == JobKind::Mirror { "mirror" } else { "backup" }) } }
                            @if let Some(r) = &v.running { (status_chip(r.status)) }
                            @else if let Some(r) = &v.last { (status_chip(r.status)) }
                        }
                        @if let Some(r) = &v.running { (progress_block(r)) }
                        @if let Some(r) = &v.last { div { (last_run_line(r)) } }
                        p.small.muted {
                            "Schedule: " (v.job.schedule.as_deref().unwrap_or("manual"))
                            @if let Some(i) = alerts::expected_interval(&v.job) {
                                " · alert after " (fmt::duration(alerts::silent_after(i))) " without a run"
                            }
                        }
                        (rules_view(&v.job))
                        details {
                            summary.small { "Silent-job alert" }
                            form.row method="post" action={ "/devices/" (device.id) "/jobs/" (v.job.id) "/interval" } style="margin-top: 0.4rem" {
                                (csrf_field(ctx))
                                label.inline { "Expected every"
                                    input type="number" name="hours" min="1" step="1" style="width: 6rem"
                                        value=[v.job.interval_override_secs.map(|s| s / 3600)]
                                        placeholder=(alerts::expected_interval(&Job { interval_override_secs: None, ..v.job.clone() }).map(|s| (s / 3600).to_string()).unwrap_or_else(|| "manual".into()));
                                    "hours"
                                }
                                button type="submit" { "Save" }
                            }
                            p.small.muted { "Empty: derived from what the device declared." }
                        }
                    }
                }
            }
            h2 { "Runs" }
            @if runs.is_empty() { p.muted { "No run reported yet." } } @else {
                div.card.table-wrap {
                    table {
                        thead { tr { th { "Started" } th { "Job" } th { "Status" } th.num.hide-narrow { "Duration" } th.num { "Added" } th.hide-narrow { "Details" } } }
                        tbody {
                            @for r in &runs {
                                tr {
                                    td title=(fmt::datetime(r.started_at)) { (fmt::ago(r.started_at)) }
                                    td { (job_names.get(&r.job_id).map(String::as_str).unwrap_or("?")) " " span.muted.small { (run_kind(r.kind)) } }
                                    td { (status_chip(r.status)) }
                                    td.num.hide-narrow { @if let Some(f) = r.finished_at { (fmt::duration(f - r.started_at)) } }
                                    td.num { @if let Some(s) = r.finish.as_ref().and_then(|f| f.summary.as_ref()) { (fmt::bytes(s.data_added_packed as i64)) } }
                                    td.hide-narrow { (run_details(r)) }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

fn run_details(r: &Run) -> Markup {
    let Some(f) = &r.finish else {
        return html! { @if r.status == RunStatus::Running { span.small.muted { "in progress" } } };
    };
    html! {
        @if let Some(m) = &f.message { div.small { (m) } }
        @if let Some(s) = &f.summary {
            div.small.muted { (s.files_new) " new, " (s.files_changed) " changed files"
                @if let Some(id) = &s.snapshot_id { " · snapshot " span.mono { (id.get(..8).unwrap_or(id)) } } }
        }
        @if let Some(p) = &f.prune { div.small.muted { (p.policy) ": " (p.snapshots_removed) " snapshots removed, " (p.snapshots_tagged_keep) " kept for deleted files" } }
        @if let Some(c) = &f.check { div.small.muted { "checked " (c.read_data_subset.as_deref().unwrap_or("structure")) ", " (c.num_errors) " errors" } }
        @if let Some(m) = &f.mirror { div.small.muted { (m.files_copied) " files copied, " (m.files_verified) " verified" @if !m.hash_mismatches.is_empty() { ", " strong { (m.hash_mismatches.len()) " mismatches" } } } }
        @if !f.errors.is_empty() || !f.unreadable_paths.is_empty() {
            details {
                summary.small { (alerts::plural(f.error_count.max(f.errors.len() as u64), "error")) }
                pre.small {
                    @for e in &f.errors { (e.message) @if let Some(i) = &e.item { " (" (i) ")" } "\n" }
                    @for p in &f.unreadable_paths { "unreadable: " (p) "\n" }
                }
            }
        }
    }
}

pub async fn device(
    State(app): State<Arc<App>>,
    admin: Admin,
    Path(id): Path<String>,
    Query(f): Query<Flash>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let device = device_or_404(&app, &id)?;
    let repo = app.store.repository_of(&id)?;
    let live = device_live(&app, &ctx, &device)?;
    let manual = provision::manual_steps(&app, &device)?;
    let idle = std::time::Duration::from_secs(app.config.restore.idle_timeout_secs);
    let others: Vec<Device> = app
        .store
        .devices()?
        .into_iter()
        .filter(|d| d.id != id)
        .collect();
    let sources = app.store.mirror_sources_of(&id)?;
    let slot = app
        .store
        .devices()?
        .iter()
        .position(|d| d.id == id)
        .unwrap_or(0);
    Ok(page(
        &device.name,
        "overview",
        &ctx,
        html! {
            (flash(&f))
            div.spread {
                h1 { (device.name) }
                span.muted { (kind_label(device.kind)) }
            }
            @if device.paired_at.is_none() { (pairing_block(&app, &ctx, &device)?) }
            @if !manual.is_empty() {
                div.card.notice.warn {
                    h3 { "Install the keys by hand" }
                    p { "Key installation is set to manual. Run each command once (it asks for the password shown), then confirm: Réplique then replaces those passwords with random ones nobody keeps." }
                    @for step in &manual {
                        div.copy { code { (step.command) } button type="button" data-copy=(step.command) { "Copy" } }
                        p.small { "Password: " code { (step.password.expose()) } }
                    }
                    form method="post" action={ "/devices/" (device.id) "/manual-done" } data-busy="Replacing the password on the Storage Box… (about 15 s)" {
                        (csrf_field(&ctx))
                        button.primary type="submit" { "Done: rotate the passwords" }
                    }
                }
            }
            div.card {
                div.row { (repo_size(&app, repo.as_ref())) }
                dl.kv.small style="margin-top: 0.5rem" {
                    dt { "Paired" } dd { @if let Some(p) = device.paired_at { (fmt::datetime(p)) } @else { "not yet" } }
                    @if let Some(v) = &device.client_version { dt { "Client" } dd { (v) } }
                    @if let Some(s) = device.last_seen_at { dt { "Last report" } dd { (fmt::ago(s)) } }
                    @if let Some(repo) = &repo {
                        dt { "Repository" } dd.mono { (format!("sftp:{}@{}:{}", repo.username, repo.host, repo.repo_path)) " (port " (repo.port) ")" }
                        dt { "Home on the box" } dd.mono { (repo.home_directory) }
                        @if let Some(t) = &repo.twin_username { dt { "Read-only twin" } dd.mono { (t) @if !repo.twin_key_installed { " (key not installed yet)" } } }
                        @if repo.adopted { dt { "Adopted" } dd { "existing sub-account, its keys left alone" } }
                    }
                }
                @if let Some(repo) = &repo {
                    div.actions style="margin-top: 0.6rem" {
                        @if device.paired_at.is_some() {
                            @if app.unlocks.is_unlocked(&admin.session, &repo.id, idle) {
                                a.button.primary href={ "/repos/" (repo.id) "/snapshots" } { "Browse snapshots" }
                            } @else {
                                a.button href={ "/repos/" (repo.id) "/unlock" } { "Unlock to browse and restore" }
                            }
                        }
                        @if device.kind == DeviceKind::Android { a.button href={ "/devices/" (device.id) "/rules" } { "Phone rules" } }
                    }
                }
            }
            @if let (Some(repo), true) = (&repo, device.paired_at.is_some()) {
                h2 { "Size" }
                div.card {
                    div.chart data-src={ "/storage/series?scope=repo:" (repo.id) "&range=30d&slot=" (slot) } data-scopes={ "repo:" (repo.id) } {}
                }
            }
            (live)
            @if device.kind != DeviceKind::Android && device.paired_at.is_some() {
                h2 { "Mirrors" }
                div.card {
                    p.small.muted { "This device can keep an encrypted byte copy of another device's repository, pulled through that repository's read-only login. It needs no password and cannot read it." }
                    @for s in &sources {
                        @let name = others.iter().find(|d| &d.id == s).map(|d| d.name.clone()).unwrap_or_else(|| s.clone());
                        form.row method="post" action={ "/devices/" (device.id) "/mirrors/" (s) "/remove" } data-busy="Taking this device's key off the source's read-only login… (about 30 s)" {
                            (csrf_field(&ctx)) span { "Mirrors " strong { (name) } } button.link type="submit" { "stop" }
                        }
                    }
                    @if device.ssh_public_key.is_some() && !others.is_empty() {
                        form.row method="post" action={ "/devices/" (device.id) "/mirrors" } data-busy="Adding this device's key to the source's read-only login… (about 30 s)" {
                            (csrf_field(&ctx))
                            select name="source" style="max-width: 16rem" {
                                @for d in &others { @if !sources.contains(&d.id) { option value=(d.id) { (d.name) } } }
                            }
                            button type="submit" { "Allow mirroring" }
                        }
                    }
                }
            }
            h2 { "Danger zone" }
            div.card.actions {
                @if device.has_token {
                    form method="post" action={ "/devices/" (device.id) "/revoke" } onsubmit="return confirm('Revoke this device\\'s report token? Its backups keep running, but its reports are refused until it pairs again.')" {
                        (csrf_field(&ctx)) button type="submit" { "Revoke report token" }
                    }
                }
                a.button.danger href={ "/devices/" (device.id) "/remove" } { "Remove device…" }
            }
        },
    )
    .into_response())
}

pub async fn device_fragment(
    State(app): State<Arc<App>>,
    admin: Admin,
    Path(id): Path<String>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let device = device_or_404(&app, &id)?;
    Ok(Html(device_live(&app, &ctx, &device)?.into_string()).into_response())
}

pub async fn new_pairing_code(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<String>,
) -> WebResult {
    device_or_404(&app, &id)?;
    match provision::new_pairing_code(&app, &id).await {
        Ok(()) => Ok(Redirect::to(&format!("/devices/{id}")).into_response()),
        Err(e) => Ok(redirect_with(
            &format!("/devices/{id}"),
            "error",
            &format!("{e:#}"),
        )),
    }
}

pub async fn manual_done(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<String>,
) -> WebResult {
    match provision::finish_manual(&app, &id).await {
        Ok(()) => Ok(redirect_with(
            &format!("/devices/{id}"),
            "ok",
            "Passwords rotated; only keys log in now.",
        )),
        Err(e) => Ok(redirect_with(
            &format!("/devices/{id}"),
            "error",
            &format!("{e:#}"),
        )),
    }
}

pub async fn revoke(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<String>,
) -> WebResult {
    let device = device_or_404(&app, &id)?;
    app.store.revoke_device_token(&id)?;
    tracing::info!(device = %device.name, "report token revoked");
    app.hub.publish(crate::events::Event::Devices);
    Ok(redirect_with(
        &format!("/devices/{id}"),
        "ok",
        "Token revoked. Create a new pairing code to pair the device again.",
    ))
}

pub async fn remove_form(
    State(app): State<Arc<App>>,
    admin: Admin,
    Path(id): Path<String>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let device = device_or_404(&app, &id)?;
    let repo = app.store.repository_of(&id)?;
    Ok(page(
        "Remove device",
        "overview",
        &ctx,
        html! {
            h1 { "Remove " (device.name) }
            div.card {
                form.stack method="post" action={ "/devices/" (device.id) "/remove" } data-busy="Removing the device from the Storage Box… (up to a few minutes when deleting data)" {
                    (csrf_field(&ctx))
                    p { "Its sub-accounts are deleted on the Storage Box, its report token stops working, and its runs and measurements are forgotten here." }
                    label.inline { input type="radio" name="data" value="keep" checked; "Keep its backups on the box" }
                    @if let Some(r) = &repo { p.small.muted style="margin: -0.4rem 0 0 1.6rem" { "They stay in " code { (r.home_directory) } ", reachable with the main account; restic can still restore them." } }
                    label.inline { input type="radio" name="data" value="delete"; "Delete its backups too" }
                    @if repo.as_ref().is_some_and(|r| r.adopted) {
                        div.notice.warn { "This sub-account was adopted, not created by Réplique." }
                    }
                    label { "To delete the backups, type the device's name"
                        input type="text" name="confirm" autocomplete="off";
                    }
                    div.actions {
                        button.danger type="submit" { "Remove device" }
                        a href={ "/devices/" (device.id) } { "Cancel" }
                    }
                }
            }
        },
    )
    .into_response())
}

#[derive(Deserialize)]
pub struct RemoveForm {
    data: String,
    confirm: Option<String>,
}

pub async fn remove(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<String>,
    Form(form): Form<RemoveForm>,
) -> WebResult {
    let device = device_or_404(&app, &id)?;
    let delete = form.data == "delete";
    if delete && form.confirm.as_deref().map(str::trim) != Some(device.name.as_str()) {
        return Ok(redirect_with(
            &format!("/devices/{id}/remove"),
            "error",
            "Type the device's name to delete its backups.",
        ));
    }
    match provision::remove_device(&app, &id, delete).await {
        Ok(()) => Ok(redirect_with(
            "/",
            "ok",
            &format!("{} removed.", device.name),
        )),
        Err(e) => Ok(redirect_with(
            &format!("/devices/{id}"),
            "error",
            &format!("Removing failed: {e:#}"),
        )),
    }
}

#[derive(Deserialize)]
pub struct IntervalForm {
    hours: Option<String>,
}

pub async fn job_interval(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path((id, job)): Path<(String, i64)>,
    Form(form): Form<IntervalForm>,
) -> WebResult {
    let job = app
        .store
        .job(job)?
        .filter(|j| j.device_id == id)
        .ok_or_else(WebError::not_found)?;
    let hours = match form
        .hours
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
    {
        None => None,
        Some(h) => Some(
            h.parse::<i64>()
                .map_err(|_| WebError::bad_request("hours must be a number"))?
                .max(1)
                * 3600,
        ),
    };
    app.store.set_job_interval_override(job.id, hours)?;
    Ok(redirect_with(&format!("/devices/{id}"), "ok", "Saved."))
}

// ---------------------------------------------------------------- phone rules

pub async fn rules_form(
    State(app): State<Arc<App>>,
    admin: Admin,
    Path(id): Path<String>,
    Query(f): Query<Flash>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let device = device_or_404(&app, &id)?;
    let config = crate::api::config_for(&app, &id)?;
    let edited = app.store.device_config(&id)?.map(|(_, at)| at);
    let mut folders = config.folders.clone();
    folders.push(FolderRule::default());
    Ok(page(
        "Phone rules",
        "overview",
        &ctx,
        html! {
            (flash(&f))
            h1 { "Phone rules · " (device.name) }
            p.muted { "What the app backs up and when; it fetches these with " code { "GET /api/v1/device/config" } ". "
                @if let Some(at) = edited { "Edited " (fmt::ago(at)) "." } @else { "These are the defaults." } }
            form method="post" action={ "/devices/" (device.id) "/rules" } {
                (csrf_field(&ctx))
                div.card {
                    div.grid {
                        label { "Job name" input type="text" name="job" value=(config.job) required pattern="[a-z0-9][a-z0-9_-]{0,63}"; }
                        label { "Every night at" span.hint { "the phone's time; empty: every so many hours" } input type="time" name="daily_at" value=[config.daily_at.as_deref()]; }
                        label { "Every (hours)" span.hint { "when no time is set" } input type="number" name="interval_hours" min="1" value=(config.interval_hours) required; }
                        label { "Minimum battery (%)" span.hint { "ignored while charging" } input type="number" name="min_battery_percent" min="0" max="100" value=[config.conditions.min_battery_percent]; }
                    }
                    div.row style="margin-top: 0.6rem" {
                        label.inline { input type="checkbox" name="unmetered_only" value="1" checked[config.conditions.unmetered_only]; "Only on Wi-Fi (unmetered)" }
                        label.inline { input type="checkbox" name="charging_only" value="1" checked[config.conditions.charging_only]; "Only while charging" }
                    }
                    p.small.muted { "Conditions apply to scheduled runs, not to “Back up now”. Retention is always never-prune on the phone, which is what makes “free up space” safe." }
                    label { "Excluded everywhere" span.hint { "one restic pattern per line; write ? for a literal $" }
                        textarea name="excludes" { (config.excludes.join("\n")) }
                    }
                }
                @for (i, folder) in folders.iter().enumerate() {
                    div.card {
                        h3 { @if folder.path.is_empty() { "Add a folder" } @else { "Folder" } }
                        div.grid {
                            label { "Path" input type="text" name={ "path_" (i) } value=(folder.path) placeholder="/storage/emulated/0/DCIM"; }
                            label { "Skip files larger than (MB)" input type="number" name={ "max_mb_" (i) } min="0" value=[folder.max_file_size_bytes.map(|b| b / 1_000_000)]; }
                            label { "Skip extensions" span.hint { "comma separated" } input type="text" name={ "exclude_ext_" (i) } value=(folder.exclude_extensions.join(", ")); }
                            label { "Only these extensions" span.hint { "empty: all" } input type="text" name={ "only_ext_" (i) } value=(folder.only_extensions.join(", ")); }
                        }
                        label { "Excludes in this folder" textarea name={ "excludes_" (i) } { (folder.excludes.join("\n")) } }
                        @if !folder.path.is_empty() { p.small.muted { "Empty the path to remove this folder." } }
                    }
                }
                div.actions {
                    button.primary type="submit" name="action" value="save" { "Save rules" }
                    button type="submit" name="action" value="reset" { "Back to the defaults" }
                }
            }
        },
    )
    .into_response())
}

fn split_list(s: &str, sep: char) -> Vec<String> {
    s.split(sep)
        .map(|x| x.trim().trim_start_matches('.').to_lowercase())
        .filter(|x| !x.is_empty())
        .collect()
}

fn lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// Reads the rules form (indexed folder fields) into a `DeviceConfig`.
pub fn parse_rules(pairs: &[(String, String)]) -> Result<DeviceConfig, String> {
    let get = |k: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let job = get("job").unwrap_or("phone").trim().to_string();
    if !replique_proto::valid_job_name(&job) {
        return Err("The job name must be 1 to 64 characters of a-z, 0-9, - and _.".into());
    }
    let interval_hours: u32 = get("interval_hours")
        .and_then(|v| v.trim().parse().ok())
        .filter(|h| *h >= 1)
        .ok_or("Every (hours) must be at least 1.")?;
    let daily_at = match get("daily_at").map(str::trim).filter(|v| !v.is_empty()) {
        // Browsers send `HH:MM` for a time field; some add `:SS`.
        Some(v) => {
            let v = v.get(..5).unwrap_or(v);
            replique_proto::parse_daily_at(v).ok_or("Every night at: a time such as 02:00.")?;
            Some(v.to_string())
        }
        None => None,
    };
    let min_battery_percent = match get("min_battery_percent")
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(v) => Some(
            v.parse::<u8>()
                .ok()
                .filter(|p| *p <= 100)
                .ok_or("Minimum battery must be 0 to 100.")?,
        ),
        None => None,
    };
    let mut folders = Vec::new();
    for i in 0..100 {
        let Some(path) = get(&format!("path_{i}")) else {
            break;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        if !path.starts_with('/') {
            return Err(format!("{path}: folder paths are absolute."));
        }
        let max_mb = get(&format!("max_mb_{i}"))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.parse::<u64>()
                    .map_err(|_| format!("{path}: the size limit is a number of MB."))
            })
            .transpose()?;
        folders.push(FolderRule {
            path: path.to_string(),
            excludes: lines(get(&format!("excludes_{i}")).unwrap_or("")),
            max_file_size_bytes: max_mb.filter(|m| *m > 0).map(|m| m * 1_000_000),
            exclude_extensions: split_list(get(&format!("exclude_ext_{i}")).unwrap_or(""), ','),
            only_extensions: split_list(get(&format!("only_ext_{i}")).unwrap_or(""), ','),
        });
    }
    if folders.is_empty() {
        return Err("Keep at least one folder.".into());
    }
    Ok(DeviceConfig {
        job,
        folders,
        excludes: lines(get("excludes").unwrap_or("")),
        conditions: Conditions {
            unmetered_only: get("unmetered_only").is_some(),
            charging_only: get("charging_only").is_some(),
            min_battery_percent,
        },
        interval_hours,
        daily_at,
        retention: Retention::NeverPrune,
    })
}

pub async fn save_rules(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<String>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> WebResult {
    device_or_404(&app, &id)?;
    let back = format!("/devices/{id}/rules");
    if pairs.iter().any(|(k, v)| k == "action" && v == "reset") {
        app.store
            .put_device_config(&id, &DeviceConfig::android_default())?;
        return Ok(redirect_with(&back, "ok", "Back to the defaults."));
    }
    match parse_rules(&pairs) {
        Ok(config) => {
            app.store.put_device_config(&id, &config)?;
            Ok(redirect_with(
                &back,
                "ok",
                "Saved; the phone picks them up at its next check.",
            ))
        }
        Err(e) => Ok(redirect_with(&back, "error", &e)),
    }
}

// ---------------------------------------------------------------- mirrors

#[derive(Deserialize)]
pub struct MirrorForm {
    source: String,
}

pub async fn add_mirror(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<String>,
    Form(form): Form<MirrorForm>,
) -> WebResult {
    match provision::assign_mirror(&app, &id, &form.source).await {
        Ok(()) => Ok(redirect_with(
            &format!("/devices/{id}"),
            "ok",
            "Mirroring allowed; the device lists it in GET /api/v1/mirror-sources.",
        )),
        Err(e) => Ok(redirect_with(
            &format!("/devices/{id}"),
            "error",
            &format!("{e:#}"),
        )),
    }
}

pub async fn remove_mirror(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path((id, source)): Path<(String, String)>,
) -> WebResult {
    match provision::revoke_mirror(&app, &id, &source).await {
        Ok(()) => Ok(redirect_with(
            &format!("/devices/{id}"),
            "ok",
            "Mirroring stopped; this device's key is off the source's read-only login.",
        )),
        Err(e) => Ok(redirect_with(
            &format!("/devices/{id}"),
            "error",
            &format!("{e:#}"),
        )),
    }
}

// ---------------------------------------------------------------- storage

const RANGES: [(&str, i64); 6] = [
    ("24h", 86_400),
    ("7d", 7 * 86_400),
    ("30d", 30 * 86_400),
    ("90d", 90 * 86_400),
    ("1y", 365 * 86_400),
    ("all", 50 * 365 * 86_400),
];

#[derive(Deserialize)]
pub struct RangeQuery {
    range: Option<String>,
}

pub async fn storage(
    State(app): State<Arc<App>>,
    admin: Admin,
    Query(q): Query<RangeQuery>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let range = q
        .range
        .filter(|r| RANGES.iter().any(|(k, _)| k == r))
        .unwrap_or_else(|| "7d".into());
    let devices = app.store.devices()?;
    let repos = app.store.repositories()?;
    Ok(page(
        "Storage",
        "storage",
        &ctx,
        html! {
            div.spread {
                h1 { "Storage" }
                div.ranges {
                    @for (key, _) in RANGES { a href={ "/storage?range=" (key) } class=[(key == range).then_some("active")] { (key) } }
                }
            }
            (box_card(&app))
            div.card {
                h3 { "Storage Box, total used" }
                div.chart data-src={ "/storage/series?scope=box&range=" (range) } data-scopes="box" {}
            }
            div.card {
                h3 { "Per repository, stacked" }
                div.chart data-src={ "/storage/series?scope=repos&range=" (range) } data-scopes="repo:*" {}
            }
            div.card.table-wrap {
                h3 { "Latest measurements" }
                table {
                    thead { tr { th { "Device" } th.num { "Size" } th.num { "Files" } th { "Measured" } } }
                    tbody {
                        @for repo in &repos {
                            @let name = devices.iter().find(|d| d.id == repo.device_id).map(|d| d.name.clone()).unwrap_or_default();
                            @let p = app.store.latest_measurement(&format!("repo:{}", repo.id)).ok().flatten();
                            tr {
                                td { a href={ "/devices/" (repo.device_id) } { (name) } }
                                td.num { @if let Some(p) = p { (fmt::bytes(p.bytes)) } @else { "–" } }
                                td.num { @if let Some(p) = p { (p.files.unwrap_or(0)) } }
                                td { @if let Some(p) = p { (fmt::ago(p.at)) } @else { span.muted { "not yet" } } }
                            }
                        }
                    }
                }
                p.small.muted {
                    "The box total comes from Hetzner's API; repositories are measured over SFTP through "
                    (if app.config.storage_box.measure_via == MeasureVia::Twin { "their read-only twins" } else { "the main account" })
                    ", every " (fmt::duration(app.config.measurement.every_secs as i64)) ". Points are kept every 5 minutes for 7 days, hourly for 90 days, daily forever."
                }
            }
        },
    )
    .into_response())
}

#[derive(Deserialize)]
pub struct SeriesQuery {
    scope: String,
    range: Option<String>,
    slot: Option<usize>,
}

#[derive(serde::Serialize)]
pub struct SeriesData {
    x: Vec<i64>,
    series: Vec<SeriesLine>,
    stacked: bool,
}

#[derive(serde::Serialize)]
struct SeriesLine {
    label: String,
    slot: usize,
    values: Vec<Option<i64>>,
}

pub async fn series(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<SeriesData>, WebError> {
    let span = RANGES
        .iter()
        .find(|(k, _)| Some(*k) == q.range.as_deref())
        .map(|(_, s)| *s)
        .unwrap_or(7 * 86_400);
    let since = db::now() - span;
    let devices = app.store.devices()?;
    // Colours follow the device (its place in the device list), never its rank
    // in one chart.
    let slot_of = |device_id: &str| devices.iter().position(|d| d.id == device_id).unwrap_or(0);
    let mut lines: Vec<(String, usize, Vec<db::Point>)> = Vec::new();
    let stacked = q.scope == "repos";
    if q.scope == "box" {
        lines.push(("Storage Box".into(), 0, app.store.series("box", since)?));
    } else if stacked {
        for repo in app.store.repositories()? {
            let name = devices
                .iter()
                .find(|d| d.id == repo.device_id)
                .map(|d| d.name.clone())
                .unwrap_or_default();
            let points = app.store.series(&format!("repo:{}", repo.id), since)?;
            // A repository never measured has nothing to stack.
            if !points.is_empty() {
                lines.push((name, slot_of(&repo.device_id), points));
            }
        }
    } else if let Some(repo_id) = q.scope.strip_prefix("repo:") {
        let repo = app
            .store
            .repository(repo_id)?
            .ok_or_else(WebError::not_found)?;
        let name = devices
            .iter()
            .find(|d| d.id == repo.device_id)
            .map(|d| d.name.clone())
            .unwrap_or_default();
        lines.push((
            name,
            q.slot.unwrap_or_else(|| slot_of(&repo.device_id)),
            app.store.series(&q.scope, since)?,
        ));
    } else {
        return Err(WebError::bad_request("unknown scope"));
    }
    let mut x: Vec<i64> = lines
        .iter()
        .flat_map(|(_, _, pts)| pts.iter().map(|p| slot_time(p.at)))
        .collect();
    x.sort_unstable();
    x.dedup();
    let series = lines
        .into_iter()
        .map(|(label, slot, pts)| {
            let by_time: std::collections::HashMap<i64, i64> =
                pts.iter().map(|p| (slot_time(p.at), p.bytes)).collect();
            // In a stack, a repository not measured yet at a time holds its
            // last known size rather than dropping to zero.
            let mut last = None;
            let values = x
                .iter()
                .map(|t| {
                    if let Some(v) = by_time.get(t) {
                        last = Some(*v);
                        Some(*v)
                    } else if stacked {
                        last
                    } else {
                        None
                    }
                })
                .collect();
            SeriesLine {
                label,
                slot,
                values,
            }
        })
        .collect();
    Ok(Json(SeriesData { x, series, stacked }))
}

/// Repositories are measured one after the other, seconds apart: on a chart
/// their points share the 5-minute slot they fall in, so stacks line up.
fn slot_time(at: i64) -> i64 {
    at - at.rem_euclid(300)
}

// ---------------------------------------------------------------- problems

/// Standing conditions that are not alerts: misconfiguration and delivery.
fn conditions(app: &App) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if app.admin_hash.is_none() {
        out.push((
            "No admin password configured".into(),
            "Set admin_password_hash_file (replique-server hash-password prints a hash).".into(),
        ));
    }
    if let Err(e) = app.hetzner() {
        out.push(("Hetzner's API is not set up".into(), format!("{e:#}")));
    } else if let Some(e) = &app.box_state.read().error {
        out.push(("Hetzner's API is failing".into(), e.clone()));
    }
    if alerts::webhook(app).is_none() {
        out.push((
            "Alerts are not sent anywhere".into(),
            "Set a Discord webhook in Settings.".into(),
        ));
    } else if let Ok(Some(e)) = app.store.setting_str(alerts::SETTING_NOTIFY_ERROR) {
        out.push(("Alerts are not reaching Discord".into(), e));
    }
    for (repo, e) in app.measure_errors.lock().iter() {
        out.push((format!("Cannot measure repository {repo}"), e.clone()));
    }
    out
}

fn problems_list(app: &App, ctx: &Ctx) -> anyhow::Result<Markup> {
    let open = app.store.open_alerts()?;
    let cleared = app.store.recent_cleared_alerts(20)?;
    let standing = conditions(app);
    Ok(html! {
        div #problems hx-get="/fragments/problems" hx-trigger="live:problems from:body, every 60s" hx-swap="outerHTML" {
            @if open.is_empty() && standing.is_empty() { div.notice.ok { "Tout va bien: nothing is wrong right now." } }
            @for a in &open {
                div class={ "card problem " (if a.severity == ALERT { "alert" } else { "" }) } {
                    div.spread {
                        strong { (a.title) }
                        span class={ "status " (if a.severity == ALERT { "critical" } else { "warning" }) } { (if a.severity == ALERT { "Alert" } else { "Warning" }) }
                    }
                    @if !a.detail.is_empty() { pre.small style="margin-top: 0.4rem" { (a.detail) } }
                    div.actions.small style="margin-top: 0.4rem" {
                        span.muted { "since " (fmt::ago(a.opened_at)) }
                        @if let Some(d) = &a.device_id { a href={ "/devices/" (d) } { "device" } }
                        @if a.kind == "drift" || a.kind == "run_lost" || a.kind == "run_partial" {
                            form method="post" action={ "/problems/" (a.id) "/dismiss" } {
                                (csrf_field(ctx)) button.link type="submit" { "dismiss" }
                            }
                        }
                    }
                }
            }
            @if !standing.is_empty() {
                h2 { "Setup" }
                @for (title, detail) in &standing {
                    div.card.problem { strong { (title) } p.small.muted { (detail) } }
                }
            }
            @if !cleared.is_empty() {
                h2 { "Recently resolved" }
                div.card.table-wrap {
                    table {
                        tbody {
                            @for a in &cleared {
                                tr { td { (a.title) } td.small.muted { (fmt::ago(a.cleared_at.unwrap_or(a.opened_at))) } }
                            }
                        }
                    }
                }
            }
        }
    })
}

pub async fn problems(State(app): State<Arc<App>>, admin: Admin) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let list = problems_list(&app, &ctx)?;
    Ok(page(
        "Problems",
        "problems",
        &ctx,
        html! { h1 { "Problems" } (list) },
    )
    .into_response())
}

pub async fn problems_fragment(State(app): State<Arc<App>>, admin: Admin) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    Ok(Html(problems_list(&app, &ctx)?.into_string()).into_response())
}

pub async fn dismiss_problem(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<i64>,
) -> WebResult {
    let alert = app.store.alert_by_id(id)?.ok_or_else(WebError::not_found)?;
    alerts::clear_quietly(&app, &alert.key).await;
    Ok(Redirect::to("/problems").into_response())
}

// ---------------------------------------------------------------- settings

pub async fn settings(
    State(app): State<Arc<App>>,
    admin: Admin,
    Query(f): Query<Flash>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let state = app.box_state.read().clone();
    let source = app.token_source();
    let webhook = alerts::webhook(&app).map(|w| {
        let w = w.expose().to_string();
        let cut = w.rfind('/').map_or(w.len(), |i| i + 1);
        format!("{}••••••", &w[..cut])
    });
    let notify_error = app.store.setting_str(alerts::SETTING_NOTIFY_ERROR)?;
    let notify_sent = app
        .store
        .setting_str(alerts::SETTING_NOTIFY_SENT)?
        .and_then(|s| s.parse::<i64>().ok());
    let warn = alerts::percent_setting(
        &app,
        alerts::SETTING_BOX_WARN,
        app.config.alerts.box_warn_percent,
    );
    let alert = alerts::percent_setting(
        &app,
        alerts::SETTING_BOX_ALERT,
        app.config.alerts.box_alert_percent,
    );
    let sb = &app.config.storage_box;
    Ok(page(
        "Settings",
        "settings",
        &ctx,
        html! {
            (flash(&f))
            h1 { "Settings" }
            h2 { "Storage Box" }
            div.card {
                form.stack method="post" action="/settings/hetzner" data-busy="Checking with Hetzner…" {
                    (csrf_field(&ctx))
                    label { "Storage Box id"
                        span.hint { "the number in the Hetzner Console's address for the box" }
                        input type="number" name="box_id" value=[app.box_id()] disabled[sb.id.is_some()];
                    }
                    label { "Hetzner API token"
                        span.hint { @match source {
                            TokenSource::File => { "read from " code { (sb.hetzner_token_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default()) } }
                            TokenSource::Settings => { "set, stored encrypted; type a new one to replace it" }
                            TokenSource::Missing => { "not set; it needs read and write access (it creates sub-accounts)" }
                        } }
                        input type="password" name="token" autocomplete="off" disabled[source == TokenSource::File];
                    }
                    div.actions { button.primary type="submit" { "Save" } }
                }
                form method="post" action="/settings/refresh" style="margin-top: 0.6rem" data-busy="Reading the box from Hetzner…" {
                    (csrf_field(&ctx)) button type="submit" { "Check the box now" }
                }
                dl.kv.small style="margin-top: 0.8rem" {
                    @if let Some(info) = &state.info {
                        dt { "Box" } dd { (info.name) " (" (info.storage_box_type.name) ", " (fmt::bytes(info.storage_box_type.size as i64)) ")" }
                        @if let Some(u) = &info.username { dt { "Main account" } dd.mono { (u) } }
                        @if let Some(h) = app.box_host() { dt { "Host" } dd.mono { (h) ":" (sb.port) } }
                        dt { "Sub-accounts" } dd {
                            (state.subaccounts_used.unwrap_or(0)) " of " (info.storage_box_type.subaccounts_limit) " used"
                            @if let Some(left) = state.subaccounts_remaining() { ", " strong { (left) " left" } }
                            " (each device takes " (if sb.measure_via == MeasureVia::Twin { "two" } else { "one" }) ")"
                        }
                    }
                    @if let Some(e) = &state.error { dt { "Last error" } dd { (e) } }
                    @if let Some(at) = state.checked_at { dt { "Checked" } dd { (fmt::ago(at)) } }
                }
            }
            h2 { "How Réplique reaches the box" }
            div.card {
                p.small.muted { "Set in the configuration file, pending Hetzner's answers to two open questions." }
                dl.kv.small {
                    dt { "Reading repositories" } dd {
                        code { "measure_via = \"" (sb.measure_via.as_str()) "\"" } ": "
                        @if sb.measure_via == MeasureVia::Twin { "through each repository's read-only twin sub-account." }
                        @else { "through the main account; add Réplique's key below to the main account's authorized keys." }
                    }
                    dt { "Installing keys" } dd {
                        code { "key_install = \"" (sb.key_install.as_str()) "\"" } ": "
                        @match sb.key_install {
                            KeyInstall::InstallSshKey => { "install-ssh-key over SSH on port 23, with the sub-account's password once." }
                            KeyInstall::Sftp => { "writes .ssh/authorized_keys over SFTP, with the sub-account's password once." }
                            KeyInstall::Manual => { "pairing shows a one-line command to run by hand." }
                        }
                    }
                    @if sb.local_root.is_some() { dt { "Local root" } dd { "development mode: repositories are local folders" } }
                    dt { "Réplique's SSH key" } dd {
                        div.copy { code { (app.public_key_line()) } button type="button" data-copy=(app.public_key_line()) { "Copy" } }
                    }
                }
            }
            h2 { "Alerts" }
            div.card {
                form.stack method="post" action="/settings/discord" {
                    (csrf_field(&ctx))
                    label { "Discord webhook"
                        span.hint { @if let Some(w) = &webhook { "set: " code { (w) } } @else { "Server settings → Integrations → Webhooks; alerts mention @everyone" } }
                        input type="url" name="webhook" autocomplete="off" placeholder="https://discord.com/api/webhooks/…";
                    }
                    div.actions {
                        button.primary type="submit" { "Save" }
                        @if webhook.is_some() { button type="submit" name="remove" value="1" { "Remove" } }
                    }
                }
                @if webhook.is_some() {
                    form method="post" action="/settings/discord/test" style="margin-top: 0.6rem" data-busy="Sending a test message…" {
                        (csrf_field(&ctx)) button type="submit" { "Send a test message" }
                    }
                }
                p.small.muted {
                    @if let Some(e) = &notify_error { "Last delivery failed: " (e) ". " }
                    @if let Some(at) = notify_sent { "Last message delivered " (fmt::ago(at)) "." }
                }
                form.stack method="post" action="/settings/thresholds" style="margin-top: 1rem" {
                    (csrf_field(&ctx))
                    div.grid {
                        label { "Box warning at (%)" input type="number" name="warn" min="1" max="100" value=(warn); }
                        label { "Box alert at (%)" input type="number" name="alert" min="1" max="100" value=(alert); }
                    }
                    p.small.muted { "A job counts as silent after its expected interval plus a margin (daily: 26 h); change it per job on the device's page." }
                    div { button type="submit" { "Save thresholds" } }
                }
            }
        },
    )
    .into_response())
}

#[derive(Deserialize)]
pub struct HetznerForm {
    box_id: Option<String>,
    token: Option<crate::secrets::Secret>,
}

pub async fn save_hetzner(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Form(form): Form<HetznerForm>,
) -> WebResult {
    if let Some(id) = form
        .box_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let id: u64 = id
            .parse()
            .map_err(|_| WebError::bad_request("The box id is a number."))?;
        app.store
            .put_setting(SETTING_BOX_ID, id.to_string().as_bytes())?;
    }
    if let Some(token) = form.token.filter(|t| !t.expose().trim().is_empty())
        && app.token_source() != TokenSource::File
    {
        let sealed = app.secrets.seal(token.expose().trim().as_bytes());
        app.store.put_setting(SETTING_HETZNER_TOKEN, &sealed)?;
        tracing::info!("Hetzner token changed");
    }
    match crate::measure::refresh_box(&app).await {
        Ok(()) => Ok(redirect_with("/settings", "ok", "Saved; Hetzner answered.")),
        Err(e) => Ok(redirect_with(
            "/settings",
            "error",
            &format!("Saved, but Hetzner's API failed: {e:#}"),
        )),
    }
}

pub async fn refresh_box(State(app): State<Arc<App>>, _admin: Admin) -> WebResult {
    match crate::measure::refresh_box(&app).await {
        Ok(()) => Ok(redirect_with("/settings", "ok", "Hetzner answered.")),
        Err(e) => Ok(redirect_with("/settings", "error", &format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
pub struct DiscordForm {
    webhook: Option<crate::secrets::Secret>,
    remove: Option<String>,
}

pub async fn save_discord(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Form(form): Form<DiscordForm>,
) -> WebResult {
    if form.remove.is_some() {
        app.store.delete_setting(alerts::SETTING_WEBHOOK)?;
        app.store.delete_setting(alerts::SETTING_NOTIFY_ERROR)?;
        return Ok(redirect_with("/settings", "ok", "Webhook removed."));
    }
    let Some(url) = form.webhook.filter(|w| !w.expose().trim().is_empty()) else {
        return Ok(redirect_with(
            "/settings",
            "error",
            "Paste the webhook's URL.",
        ));
    };
    let trimmed = url.expose().trim();
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        return Ok(redirect_with(
            "/settings",
            "error",
            "The webhook must be an http(s) URL.",
        ));
    }
    app.store.put_setting(
        alerts::SETTING_WEBHOOK,
        &app.secrets.seal(trimmed.as_bytes()),
    )?;
    app.store.delete_setting(alerts::SETTING_NOTIFY_ERROR)?;
    Ok(redirect_with("/settings", "ok", "Webhook saved."))
}

pub async fn test_discord(State(app): State<Arc<App>>, _admin: Admin) -> WebResult {
    match alerts::notify(
        &app,
        "@everyone Test message from Réplique: alerts will arrive here.",
        true,
    )
    .await
    {
        Ok(()) => Ok(redirect_with("/settings", "ok", "Sent; check the channel.")),
        Err(e) => Ok(redirect_with("/settings", "error", &format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
pub struct ThresholdForm {
    warn: u8,
    alert: u8,
}

pub async fn save_thresholds(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Form(form): Form<ThresholdForm>,
) -> WebResult {
    if form.warn == 0 || form.alert > 100 || form.warn >= form.alert {
        return Ok(redirect_with(
            "/settings",
            "error",
            "The warning must be below the alert, both between 1 and 100.",
        ));
    }
    app.store
        .put_setting(alerts::SETTING_BOX_WARN, form.warn.to_string().as_bytes())?;
    app.store
        .put_setting(alerts::SETTING_BOX_ALERT, form.alert.to_string().as_bytes())?;
    Ok(redirect_with("/settings", "ok", "Thresholds saved."))
}

// ---------------------------------------------------------------- misc

pub async fn runbook(State(app): State<Arc<App>>, admin: Admin) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    Ok(page("Restore runbook", "", &ctx, html! { div.card { pre style="font-family: inherit; font-size: 0.95em" { (super::RUNBOOK) } } }).into_response())
}

/// The login page (rendered by `auth::login_form`).
pub fn login(token: &str, next: Option<&str>, failed: bool, configured: bool) -> Html<String> {
    bare(
        "Log in",
        html! {
            h1 { "Réplique" }
            @if !configured {
                div.notice.error { "No admin password is configured yet: put the output of " code { "replique-server hash-password" } " in the file named by admin_password_hash_file." }
            }
            @if failed { div.notice.error { "Wrong password." } }
            div.card {
                form.stack method="post" action="/login" data-busy="Logging in…" {
                    input type="hidden" name="token" value=(token);
                    @if let Some(n) = next { input type="hidden" name="next" value=(n); }
                    label { "Password" input type="password" name="password" autocomplete="current-password" autofocus required; }
                    div { button.primary type="submit" { "Log in" } }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_form_parses() {
        let pairs: Vec<(String, String)> = [
            ("job", "phone"),
            ("interval_hours", "12"),
            ("daily_at", "02:30:00"),
            ("unmetered_only", "1"),
            ("excludes", ".thumbnails\n\n.cache"),
            ("path_0", "/storage/emulated/0/DCIM"),
            ("max_mb_0", "500"),
            ("exclude_ext_0", ".TMP, log"),
            ("only_ext_0", ""),
            ("excludes_0", ""),
            ("path_1", ""),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let c = parse_rules(&pairs).unwrap();
        assert_eq!(c.interval_hours, 12);
        assert!(c.conditions.unmetered_only && !c.conditions.charging_only);
        assert_eq!(c.excludes, [".thumbnails", ".cache"]);
        assert_eq!(c.folders.len(), 1);
        assert_eq!(c.folders[0].max_file_size_bytes, Some(500_000_000));
        assert_eq!(c.folders[0].exclude_extensions, ["tmp", "log"]);
        assert_eq!(c.retention, Retention::NeverPrune);
        assert_eq!(c.daily_at.as_deref(), Some("02:30"));
        let bad: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| {
                let v = if k == "daily_at" { "25:00" } else { v.as_str() };
                (k.clone(), v.to_string())
            })
            .collect();
        assert!(parse_rules(&bad).is_err());
    }
}
