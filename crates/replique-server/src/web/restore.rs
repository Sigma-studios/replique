//! Restore pages: unlock a repository with its password, browse its
//! snapshots and folders, download a file or a folder as a zip.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use maud::html;
use serde::Deserialize;

use super::{Ctx, WebError, WebResult, csrf_field, fmt, page};
use crate::App;
use crate::auth::Admin;
use crate::db::{Device, Repository};
use crate::restore::{Restic, ResticError, valid_path, valid_snapshot_id};
use crate::secrets::Secret;

fn repo_and_device(app: &App, id: &str) -> Result<(Repository, Device), WebError> {
    let repo = app.store.repository(id)?.ok_or_else(WebError::not_found)?;
    let device = app
        .store
        .device(&repo.device_id)?
        .ok_or_else(WebError::not_found)?;
    Ok((repo, device))
}

fn idle(app: &App) -> Duration {
    Duration::from_secs(app.config.restore.idle_timeout_secs)
}

/// restic for this repository with the session's password, or a redirect to
/// the unlock form. The error is the response itself, returned at once by
/// every caller, so its size does not matter.
#[allow(clippy::result_large_err)]
fn restic_for<'a>(app: &'a App, admin: &Admin, repo: &Repository) -> Result<Restic<'a>, Response> {
    let Some(password) = app.unlocks.get(&admin.session, &repo.id, idle(app)) else {
        return Err(Redirect::to(&format!("/repos/{}/unlock", repo.id)).into_response());
    };
    let access = app
        .reader(repo)
        .map_err(|e| WebError::from(e).into_response())?;
    Restic::new(app, &access, password).map_err(|e| WebError::from(e).into_response())
}

fn unlock_page(
    app: &App,
    ctx: &Ctx,
    repo: &Repository,
    device: &Device,
    error: Option<&str>,
) -> Response {
    page(
        "Unlock",
        "overview",
        ctx,
        html! {
            h1 { "Unlock " (device.name) "'s backups" }
            @if let Some(e) = error { div.notice.error { (e) } }
            div.card {
                form.stack method="post" action={ "/repos/" (repo.id) "/unlock" } {
                    (csrf_field(ctx))
                    label { "Restic password of this repository"
                        span.hint { "the one the device showed once at pairing, from your password manager" }
                        input type="password" name="password" autocomplete="off" required autofocus;
                    }
                    div { button.primary type="submit" { "Unlock" } }
                }
                p.small.muted {
                    "The password stays in this server's memory only, for this browser session: it is forgotten after "
                    (fmt::duration(app.config.restore.idle_timeout_secs as i64))
                    " without use or when you log out, and it is never written to disk or to logs. "
                    "Réplique reads the repository through its read-only login, without locking it, so backups carry on meanwhile."
                }
            }
        },
    )
    .into_response()
}

pub async fn unlock_form(
    State(app): State<Arc<App>>,
    admin: Admin,
    Path(id): Path<String>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let (repo, device) = repo_and_device(&app, &id)?;
    Ok(unlock_page(&app, &ctx, &repo, &device, None))
}

#[derive(Deserialize)]
pub struct UnlockForm {
    password: Secret,
}

pub async fn unlock(
    State(app): State<Arc<App>>,
    admin: Admin,
    Path(id): Path<String>,
    Form(form): Form<UnlockForm>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let (repo, device) = repo_and_device(&app, &id)?;
    let access = app.reader(&repo)?;
    let restic = Restic::new(&app, &access, form.password.clone())?;
    match restic.snapshots().await {
        Ok(_) => {
            app.unlocks.put(&admin.session, &repo.id, form.password);
            tracing::info!(device = %device.name, "repository unlocked for browsing");
            Ok(Redirect::to(&format!("/repos/{id}/snapshots")).into_response())
        }
        Err(e) => {
            let message = match e.downcast_ref::<ResticError>() {
                Some(r) if r.code == Some(12) => "Wrong password.".to_string(),
                _ => format!("{e:#}"),
            };
            tracing::info!(device = %device.name, "unlocking failed: {message}");
            Ok(unlock_page(&app, &ctx, &repo, &device, Some(&message)))
        }
    }
}

pub async fn lock(State(app): State<Arc<App>>, admin: Admin, Path(id): Path<String>) -> WebResult {
    let (repo, _) = repo_and_device(&app, &id)?;
    app.unlocks.forget(&admin.session, &repo.id);
    Ok(Redirect::to(&format!("/devices/{}", repo.device_id)).into_response())
}

fn lock_button(ctx: &Ctx, repo: &Repository) -> maud::Markup {
    html! {
        form method="post" action={ "/repos/" (repo.id) "/lock" } {
            (csrf_field(ctx)) button type="submit" { "Lock (forget the password)" }
        }
    }
}

pub async fn snapshots(
    State(app): State<Arc<App>>,
    admin: Admin,
    Path(id): Path<String>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let (repo, device) = repo_and_device(&app, &id)?;
    let restic = match restic_for(&app, &admin, &repo) {
        Ok(r) => r,
        Err(r) => return Ok(r),
    };
    let list = restic.snapshots().await?;
    Ok(page(
        "Snapshots",
        "overview",
        &ctx,
        html! {
            div.spread {
                h1 { "Snapshots · " a href={ "/devices/" (device.id) } { (device.name) } }
                (lock_button(&ctx, &repo))
            }
            @if list.is_empty() { p.muted { "No snapshot yet." } } @else {
                div.card.table-wrap {
                    table {
                        thead { tr { th { "Time" } th { "Id" } th.hide-narrow { "Paths" } th.hide-narrow { "Host" } } }
                        tbody {
                            @for s in &list {
                                tr {
                                    td { a href={ "/repos/" (repo.id) "/browse?snapshot=" (s.id) "&path=/" } { (s.time.format("%Y-%m-%d %H:%M %:z")) } }
                                    td.mono { (if s.short_id.is_empty() { s.id.get(..8).unwrap_or(&s.id) } else { &s.short_id }) }
                                    td.hide-narrow.mono.small { (s.paths.join(", ")) }
                                    td.hide-narrow.small { (s.hostname) @for t in &s.tags { " " code { (t) } } }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
    .into_response())
}

#[derive(Deserialize)]
pub struct BrowseQuery {
    snapshot: String,
    path: Option<String>,
    zip: Option<String>,
}

fn encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub async fn browse(
    State(app): State<Arc<App>>,
    admin: Admin,
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> WebResult {
    let ctx = Ctx::new(&app, &admin);
    let (repo, device) = repo_and_device(&app, &id)?;
    let path = q.path.clone().unwrap_or_else(|| "/".into());
    if !valid_snapshot_id(&q.snapshot) || !valid_path(&path) {
        return Err(WebError::bad_request("Invalid snapshot or path."));
    }
    let restic = match restic_for(&app, &admin, &repo) {
        Ok(r) => r,
        Err(r) => return Ok(r),
    };
    let (snapshot, nodes) = restic.ls(&q.snapshot, &path).await?;
    let mut crumbs = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        acc.push('/');
        acc.push_str(part);
        crumbs.push((acc.clone(), format!("{part}/")));
    }
    let base = format!("/repos/{}", repo.id);
    let snap = encode(&q.snapshot);
    Ok(page(
        "Browse",
        "overview",
        &ctx,
        html! {
            div.spread {
                h1 { a href={ "/devices/" (device.id) } { (device.name) } " · " a href={ (base) "/snapshots" } { "snapshots" } }
                (lock_button(&ctx, &repo))
            }
            @if let Some(s) = &snapshot { p.muted.small { "Snapshot " span.mono { (s.short_id) } " of " (s.time.format("%Y-%m-%d %H:%M %:z")) } }
            div.crumbs {
                @for (p, label) in &crumbs { a href={ (base) "/browse?snapshot=" (snap) "&path=" (encode(p)) } { (label) } }
            }
            div.actions style="margin-bottom: 0.6rem" {
                @if path != "/" { a.button href={ (base) "/download?zip=1&snapshot=" (snap) "&path=" (encode(&path)) } { "Download this folder (zip)" } }
            }
            div.card.table-wrap {
                @if nodes.is_empty() { p.muted { "Empty folder." } } @else {
                    table {
                        thead { tr { th { "Name" } th.num { "Size" } th.hide-narrow { "Modified" } th {} } }
                        tbody {
                            @for n in &nodes {
                                tr {
                                    td.mono {
                                        @if n.is_dir() { a href={ (base) "/browse?snapshot=" (snap) "&path=" (encode(&n.path)) } { (n.name) "/" } }
                                        @else { (n.name) }
                                    }
                                    td.num { @if let Some(s) = n.size { @if !n.is_dir() { (fmt::bytes(s as i64)) } } }
                                    td.hide-narrow.small.muted { @if let Some(m) = n.mtime { (m.format("%Y-%m-%d %H:%M")) } }
                                    td {
                                        @if n.is_dir() { a href={ (base) "/download?zip=1&snapshot=" (snap) "&path=" (encode(&n.path)) } { "zip" } }
                                        @else if n.kind == "file" { a href={ (base) "/download?snapshot=" (snap) "&path=" (encode(&n.path)) } { "download" } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
    .into_response())
}

/// `Content-Disposition` with an ASCII fallback and the UTF-8 name (RFC 6266).
fn attachment(name: &str) -> HeaderValue {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let utf8: String = url::form_urlencoded::byte_serialize(name.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{utf8}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

pub async fn download(
    State(app): State<Arc<App>>,
    admin: Admin,
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> WebResult {
    let (repo, device) = repo_and_device(&app, &id)?;
    let path = q.path.clone().unwrap_or_default();
    if !valid_snapshot_id(&q.snapshot) || !valid_path(&path) || path == "/" && q.zip.is_none() {
        return Err(WebError::bad_request("Invalid snapshot or path."));
    }
    let restic = match restic_for(&app, &admin, &repo) {
        Ok(r) => r,
        Err(r) => return Ok(r),
    };
    let zip = q.zip.is_some();
    let dump = restic.dump(&q.snapshot, &path, zip).await?;
    let base = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(&device.slug);
    let name = if zip {
        format!("{base}.zip")
    } else {
        base.to_string()
    };
    tracing::info!(device = %device.name, path = %path, zip, "restore download");
    let content_type = if zip {
        "application/zip"
    } else {
        "application/octet-stream"
    };
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::CONTENT_DISPOSITION, attachment(&name)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        dump.into_body(),
    )
        .into_response())
}
