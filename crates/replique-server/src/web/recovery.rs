//! The Recovery page: the Bitwarden copies, the paper kit's checklist, the
//! recovery plan, and the kit's print view. See `crate::recovery` and
//! `crate::vault` for why none of it holds a secret.

use std::sync::Arc;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{Html, IntoResponse};
use maud::{DOCTYPE, html};
use serde::Deserialize;

use super::{Ctx, Flash, WebError, WebResult, csrf_field, flash, fmt, page, redirect_with};
use crate::App;
use crate::auth::Admin;
use crate::db;
use crate::recovery;

pub async fn page_view(
    State(app): State<Arc<App>>,
    admin: Admin,
    Query(f): Query<Flash>,
) -> WebResult {
    recovery::seed(&app)?;
    let ctx = Ctx::new(&app, &admin);
    let todo = recovery::pending(&app);
    let entries = app.store.recovery_entries()?;
    let copies = app.store.latest_vault_copies()?;
    let devices = app.store.devices()?;
    let version = recovery::version(&app);
    let printed = recovery::printed_version(&app);
    Ok(page(
        "Recovery",
        "recovery",
        &ctx,
        html! {
            (flash(&f))
            h1 { "Recovery" }
            p.muted { "What to do when something is lost: the plan, the paper kit and copies of your password vault. Nothing secret is stored here; the secrets are written by hand in the kit." }

            div.card {
                div.spread {
                    h2 style="margin: 0" { "Paper kit" }
                    span.muted.small { "version " (version) }
                }
                @if todo.is_empty() {
                    p { span.status.good { "Up to date" } " "
                        span.muted.small { "printed version " (printed)
                            @if let Some(at) = recovery::reviewed_at(&app) { ", envelopes checked " (fmt::when(at)) } } }
                } @else {
                    ul { @for t in &todo { li { (t) } } }
                    p.small.muted { "Réplique reminds you on Discord every week until this is done, then once a year, and whenever the plan or the checklist changes." }
                }
                div.actions {
                    a.button.primary href="/recovery/print" target="_blank" { "Print the kit" }
                    form method="post" action="/recovery/printed" {
                        (csrf_field(&ctx))
                        button type="submit" { "I printed it and filled it in" }
                    }
                    form method="post" action="/recovery/reviewed" {
                        (csrf_field(&ctx))
                        button type="submit" { "I checked both envelopes" }
                    }
                }
                p.small.muted { "Print it from the browser (two copies, one per envelope) and write the codes by hand. Do not save it as a PDF, and never photograph the filled-in sheets." }
            }

            div.card.table-wrap {
                h2 style="margin-top: 0" { "Kit checklist" }
                p.small.muted { "What each envelope holds, written by hand. Only the names live here." }
                table {
                    thead { tr { th { "Entry" } th.hide-narrow { "Where to find it" } th { "Written down" } th {} } }
                    tbody {
                        @for e in &entries {
                            tr {
                                td { strong { (e.name) } }
                                td.hide-narrow.small.muted { (e.hint) }
                                td.small {
                                    @if let Some(at) = e.written_at { (fmt::when(at)) } @else { span.status.warning { "Not yet" } }
                                }
                                td {
                                    div.actions {
                                        form method="post" action={ "/recovery/entries/" (e.id) "/written" } {
                                            (csrf_field(&ctx))
                                            button.link type="submit" { (if e.written_at.is_some() { "Written again" } else { "Written down" }) }
                                        }
                                        details {
                                            summary.small { "Edit" }
                                            form.stack method="post" action={ "/recovery/entries/" (e.id) } style="margin-top: 0.4rem" {
                                                (csrf_field(&ctx))
                                                label { "Name" input type="text" name="name" value=(e.name) required; }
                                                label { "Where to find it" input type="text" name="hint" value=(e.hint); }
                                                div.actions {
                                                    button type="submit" { "Save" }
                                                    button.danger type="submit" formaction={ "/recovery/entries/" (e.id) "/delete" } { "Remove" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                details style="margin-top: 0.6rem" {
                    summary { "Add an entry" }
                    form.stack method="post" action="/recovery/entries" style="margin-top: 0.4rem" {
                        (csrf_field(&ctx))
                        label { "Name" span.hint { "e.g. “Bank: 2FA recovery codes”" } input type="text" name="name" required; }
                        label { "Where to find it" input type="text" name="hint"; }
                        div { button type="submit" { "Add" } }
                    }
                }
            }

            div.card.table-wrap {
                h2 style="margin-top: 0" { "Password vault copies" }
                p.small.muted { "Copies of the Bitwarden desktop app's vault, encrypted with your master password: only it opens them. Kept here (the last 10) and on the Storage Box, in " code { "replique/_replique/vault/" } "." }
                @if copies.is_empty() {
                    p.muted { "No copy yet. On a NixOS device, turn on the Bitwarden copy (its device page shows the lines to add)." }
                } @else {
                    table {
                        thead { tr { th { "From" } th { "File" } th.num { "Size" } th { "Last copy" } th {} } }
                        tbody {
                            @for c in &copies {
                                @let device = devices.iter().find(|d| d.id == c.device_id);
                                tr {
                                    td { (device.map(|d| d.name.as_str()).unwrap_or("?")) }
                                    td.mono { (c.name) }
                                    td.num { (fmt::bytes(c.size)) }
                                    td.small {
                                        "changed " (fmt::when(c.received_at))
                                        @if c.confirmed_at > c.received_at { ", unchanged since, checked " (fmt::when(c.confirmed_at)) }
                                        @if !c.on_box { " · " span.muted { "not on the box yet" } }
                                    }
                                    td { a href={ "/vault/" (c.id) } { "Download" } }
                                }
                            }
                        }
                    }
                }
            }

            div.card {
                div.spread {
                    h2 style="margin: 0" { "Recovery plan" }
                    span.muted.small { "also on the Storage Box, in " code { "replique/_replique/recovery/" } }
                }
                div.plan { (recovery::render(&recovery::plan(&app))) }
                details {
                    summary { "Edit the plan" }
                    form.stack method="post" action="/recovery/plan" style="margin-top: 0.4rem" {
                        (csrf_field(&ctx))
                        textarea name="plan" rows="28" { (recovery::plan(&app)) }
                        p.small.muted { "Markdown. Locations and steps only, never a password or a code." }
                        div { button.primary type="submit" { "Save" } }
                    }
                }
            }
        },
    )
    .into_response())
}

/// The kit as printed: the plan, then one block per checklist entry with room
/// to write by hand, each page saying which version it is.
pub async fn print_view(State(app): State<Arc<App>>, _admin: Admin) -> WebResult {
    recovery::seed(&app)?;
    let version = recovery::version(&app);
    let today = db::to_time(db::now()).format("%Y-%m-%d").to_string();
    let entries = app.store.recovery_entries()?;
    let footer = format!("Recovery kit · version {version} · printed {today}");
    Ok(Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="utf-8";
                    title { (footer) }
                    style { (maud::PreEscaped(PRINT_CSS)) (maud::PreEscaped(page_footer(&footer))) }
                }
                body {
                    div.footer { (footer) }
                    p.lead { (footer) br;
                        "Open in an emergency. Fill in by hand; never photograph or scan these sheets. "
                        "If a seal was broken, regenerate the codes concerned." }
                    div.plan { (recovery::render(&recovery::plan(&app))) }
                    section.kit {
                        h1 { "Kit: written by hand " span.version { "(version " (version) ")" } }
                        @for e in &entries {
                            div.entry {
                                h2 { (e.name) }
                                @if !e.hint.is_empty() { p.hint { (e.hint) } }
                                div.lines { @for _ in 0..5 { div.line {} } }
                                p.written { "Written on: ____________   Checked on: ____________   · kit version " (version) }
                            }
                        }
                    }
                    script { (maud::PreEscaped("window.addEventListener('load', function () { window.print(); });")) }
                }
            }
        }
        .into_string(),
    )
    .into_response())
}

const PRINT_CSS: &str = r#"
@page { size: A4; margin: 16mm 16mm 20mm; }
body { font: 11pt/1.45 -apple-system, "Segoe UI", Roboto, sans-serif; color: #000; background: #fff; margin: 0 auto; max-width: 180mm; }
h1 { font-size: 17pt; margin: 0 0 6pt; }
h2 { font-size: 13pt; margin: 14pt 0 4pt; }
h3 { font-size: 11.5pt; margin: 10pt 0 2pt; }
table { border-collapse: collapse; width: 100%; margin: 4pt 0 8pt; }
th, td { border: 0.6pt solid #888; padding: 3pt 5pt; text-align: left; vertical-align: top; }
code { font-family: ui-monospace, Menlo, monospace; font-size: 9.5pt; }
.lead { border: 1pt solid #000; padding: 6pt 8pt; font-weight: 600; }
.kit { break-before: page; }
.entry { break-inside: avoid; border: 1pt solid #000; padding: 6pt 10pt 8pt; margin: 0 0 10pt; }
.entry h2 { margin-top: 0; }
.hint { margin: 0 0 6pt; color: #333; font-size: 9.5pt; }
.line { border-bottom: 0.8pt solid #555; height: 22pt; }
.written { margin: 8pt 0 0; font-size: 9.5pt; }
.version { font-weight: normal; font-size: 11pt; color: #333; }
/* On screen, the version above the page; on paper, in each page's bottom
   margin (page_footer), where it cannot cover any text. Browsers without page
   margin boxes still show it in the banner, the kit heading and every entry. */
.footer { margin-bottom: 12px; font-size: 9pt; color: #333; }
@media print { .footer { display: none; } }
@media screen { body { padding: 20px; } }
"#;

/// The bottom margin of every printed page: version, date, page numbers.
fn page_footer(text: &str) -> String {
    let text = text.replace(['"', '\\'], "");
    format!(
        "@page {{ @bottom-center {{ content: \"{text} · page \" counter(page) \" of \" counter(pages); font: 8.5pt -apple-system, sans-serif; color: #333; }} }}"
    )
}

#[derive(Deserialize)]
pub struct PlanForm {
    plan: String,
}

pub async fn save_plan(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Form(f): Form<PlanForm>,
) -> WebResult {
    // Browsers send CRLF from textareas; keep the plan's own line endings.
    recovery::set_plan(&app, &f.plan.replace("\r\n", "\n"))?;
    Ok(redirect_with(
        "/recovery",
        "ok",
        "Plan saved. Print the kit again when you are done editing.",
    ))
}

#[derive(Deserialize)]
pub struct EntryForm {
    name: String,
    #[serde(default)]
    hint: String,
}

pub async fn add_entry(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Form(f): Form<EntryForm>,
) -> WebResult {
    let name = f.name.trim();
    if name.is_empty() {
        return Ok(redirect_with(
            "/recovery",
            "error",
            "Give the entry a name.",
        ));
    }
    app.store.add_recovery_entry(name, f.hint.trim())?;
    recovery::bump(&app)?;
    Ok(redirect_with(
        "/recovery",
        "ok",
        "Entry added: write it down, then print the kit again.",
    ))
}

pub async fn update_entry(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<i64>,
    Form(f): Form<EntryForm>,
) -> WebResult {
    let name = f.name.trim();
    if name.is_empty() {
        return Ok(redirect_with(
            "/recovery",
            "error",
            "Give the entry a name.",
        ));
    }
    let hint = f.hint.trim();
    // Saved unchanged: the envelopes are still current.
    let unchanged = app
        .store
        .recovery_entries()?
        .iter()
        .any(|e| e.id == id && e.name == name && e.hint == hint);
    if !unchanged {
        app.store.update_recovery_entry(id, name, hint)?;
        recovery::bump(&app)?;
    }
    Ok(redirect_with("/recovery", "ok", "Entry saved."))
}

pub async fn delete_entry(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<i64>,
) -> WebResult {
    app.store.delete_recovery_entry(id)?;
    recovery::bump(&app)?;
    Ok(redirect_with(
        "/recovery",
        "ok",
        "Entry removed. Print the kit again, and destroy the old sheets.",
    ))
}

pub async fn entry_written(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<i64>,
) -> WebResult {
    app.store.set_recovery_written(id, Some(db::now()))?;
    Ok(redirect_with("/recovery", "ok", "Noted."))
}

pub async fn printed(State(app): State<Arc<App>>, _admin: Admin) -> WebResult {
    recovery::mark_printed(&app)?;
    crate::recovery::remind(&app).await;
    Ok(redirect_with(
        "/recovery",
        "ok",
        "Noted: the envelopes hold the current version.",
    ))
}

pub async fn reviewed(State(app): State<Arc<App>>, _admin: Admin) -> WebResult {
    recovery::mark_reviewed(&app)?;
    crate::recovery::remind(&app).await;
    Ok(redirect_with(
        "/recovery",
        "ok",
        "Noted: both envelopes checked.",
    ))
}

/// One Bitwarden copy, as the file it was.
pub async fn download_copy(
    State(app): State<Arc<App>>,
    _admin: Admin,
    Path(id): Path<String>,
) -> WebResult {
    let copy = app.store.vault_copy(&id)?.ok_or_else(WebError::not_found)?;
    let device = app
        .store
        .device(&copy.device_id)?
        .ok_or_else(WebError::not_found)?;
    let bytes = crate::vault::read(&app, &copy)?;
    let name = crate::vault::download_name(&device, &copy);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            ),
        ],
        bytes,
    )
        .into_response())
}
