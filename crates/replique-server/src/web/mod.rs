//! The website: server-rendered pages (maud), htmx for fragments and forms,
//! Server-Sent Events for live updates, uPlot for charts. Every script and
//! stylesheet is compiled into the binary; nothing loads from a CDN.

pub mod fmt;
pub mod pages;
pub mod restore;

use std::sync::Arc;

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::App;
use crate::auth;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The restore runbook, also served by the website (`/runbook`). The copy
/// in `docs/restore.md` is the same text; a test keeps them equal.
pub const RUNBOOK: &str = include_str!("../../assets/restore.md");

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/", get(pages::overview))
        .route("/fragments/overview", get(pages::overview_fragment))
        .route("/devices/new", get(pages::add_device_form))
        .route("/devices", post(pages::add_device))
        .route("/devices/{id}", get(pages::device))
        .route("/fragments/devices/{id}", get(pages::device_fragment))
        .route(
            "/fragments/devices/{id}/pairing",
            get(pages::pairing_fragment),
        )
        .route("/devices/{id}/pairing-code", post(pages::new_pairing_code))
        .route("/devices/{id}/manual-done", post(pages::manual_done))
        .route("/devices/{id}/revoke", post(pages::revoke))
        .route(
            "/devices/{id}/remove",
            get(pages::remove_form).post(pages::remove),
        )
        .route(
            "/devices/{id}/jobs/{job}/interval",
            post(pages::job_interval),
        )
        .route(
            "/devices/{id}/rules",
            get(pages::rules_form).post(pages::save_rules),
        )
        .route("/devices/{id}/mirrors", post(pages::add_mirror))
        .route(
            "/devices/{id}/mirrors/{source}/remove",
            post(pages::remove_mirror),
        )
        .route("/storage", get(pages::storage))
        .route("/storage/series", get(pages::series))
        .route("/problems", get(pages::problems))
        .route("/fragments/problems", get(pages::problems_fragment))
        .route("/problems/{id}/dismiss", post(pages::dismiss_problem))
        .route("/settings", get(pages::settings))
        .route("/settings/hetzner", post(pages::save_hetzner))
        .route("/settings/discord", post(pages::save_discord))
        .route("/settings/discord/test", post(pages::test_discord))
        .route("/settings/thresholds", post(pages::save_thresholds))
        .route("/settings/refresh", post(pages::refresh_box))
        .route(
            "/repos/{id}/unlock",
            get(restore::unlock_form).post(restore::unlock),
        )
        .route("/repos/{id}/lock", post(restore::lock))
        .route("/repos/{id}/snapshots", get(restore::snapshots))
        .route("/repos/{id}/browse", get(restore::browse))
        .route("/repos/{id}/download", get(restore::download))
        .route("/runbook", get(pages::runbook))
        .route("/login", get(auth::login_form).post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/events", get(crate::events::stream))
        .route("/assets/{name}", get(asset))
        .route("/healthz", get(|| async { "ok" }))
}

async fn asset(Path(name): Path<String>) -> Response {
    let (body, kind): (&'static [u8], &str) = match name.as_str() {
        "style.css" => (include_bytes!("../../assets/style.css"), "text/css"),
        "app.js" => (include_bytes!("../../assets/app.js"), "text/javascript"),
        "htmx.min.js" => (
            include_bytes!("../../assets/vendor/htmx-2.0.4.min.js"),
            "text/javascript",
        ),
        "uPlot.min.js" => (
            include_bytes!("../../assets/vendor/uPlot-1.6.31.iife.min.js"),
            "text/javascript",
        ),
        "uPlot.min.css" => (
            include_bytes!("../../assets/vendor/uPlot-1.6.31.min.css"),
            "text/css",
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_str(&format!("{kind}; charset=utf-8")).unwrap(),
            ),
            // URLs carry the version, so a new release is a new URL.
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=604800"),
            ),
        ],
        body,
    )
        .into_response()
}

/// What every page's frame needs.
pub struct Ctx {
    pub csrf: String,
    pub open_problems: usize,
}

impl Ctx {
    pub fn new(app: &App, admin: &auth::Admin) -> Ctx {
        Ctx {
            csrf: admin.csrf.clone(),
            open_problems: app.store.open_alerts().map(|a| a.len()).unwrap_or(0),
        }
    }
}

pub fn csrf_field(ctx: &Ctx) -> Markup {
    html! { input type="hidden" name="csrf" value=(ctx.csrf); }
}

/// The frame of every logged-in page.
pub fn page(title: &str, active: &str, ctx: &Ctx, body: Markup) -> Html<String> {
    let nav = [
        ("/", "Aperçu", "overview"),
        ("/storage", "Storage", "storage"),
        ("/problems", "Problems", "problems"),
        ("/devices/new", "Add device", "add"),
        ("/settings", "Settings", "settings"),
    ];
    let headers = format!("{{\"X-CSRF-Token\": \"{}\"}}", ctx.csrf);
    Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width, initial-scale=1";
                    title { (title) " · Réplique" }
                    link rel="stylesheet" href={ "/assets/style.css?v=" (VERSION) };
                    link rel="stylesheet" href={ "/assets/uPlot.min.css?v=" (VERSION) };
                    script defer src={ "/assets/htmx.min.js?v=" (VERSION) } {}
                    script defer src={ "/assets/uPlot.min.js?v=" (VERSION) } {}
                    script defer src={ "/assets/app.js?v=" (VERSION) } {}
                }
                body data-live="1" hx-headers=(headers) {
                    header.top {
                        a.brand href="/" { "Réplique" }
                        nav {
                            @for (href, label, key) in nav {
                                a href=(href) class=[(key == active).then_some("active")] {
                                    (label)
                                    @if key == "problems" && ctx.open_problems > 0 {
                                        span.count { (ctx.open_problems) }
                                    }
                                }
                            }
                        }
                        form method="post" action="/logout" {
                            (csrf_field(ctx))
                            button.link type="submit" { "Log out" }
                        }
                    }
                    main { (body) }
                    footer {
                        "Réplique " (VERSION) " · times in UTC · "
                        a href="/runbook" { "Restore runbook" }
                        " (works without Réplique: restic, the sub-account login and the password are enough)"
                    }
                }
            }
        }
        .into_string(),
    )
}

/// A page without the logged-in frame (login, errors).
pub fn bare(title: &str, body: Markup) -> Html<String> {
    Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width, initial-scale=1";
                    title { (title) " · Réplique" }
                    link rel="stylesheet" href={ "/assets/style.css?v=" (VERSION) };
                }
                body {
                    main style="max-width: 460px; margin-top: 8vh;" { (body) }
                }
            }
        }
        .into_string(),
    )
}

/// An error shown as a page.
pub struct WebError {
    pub status: StatusCode,
    pub message: String,
}

impl WebError {
    pub fn not_found() -> WebError {
        WebError {
            status: StatusCode::NOT_FOUND,
            message: "Nothing here.".into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> WebError {
        WebError {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for WebError {
    fn from(e: anyhow::Error) -> WebError {
        tracing::error!("website: {e:#}");
        WebError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{e:#}"),
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let body = bare(
            "Error",
            html! {
                h1 { "Something went wrong" }
                div.notice.error { (self.message) }
                p { a href="/" { "Back to Réplique" } }
            },
        );
        (self.status, body).into_response()
    }
}

pub type WebResult = Result<Response, WebError>;

/// Redirects after a form, with a message for the next page.
pub fn redirect_with(path: &str, key: &str, message: &str) -> Response {
    let message = url::form_urlencoded::byte_serialize(message.as_bytes()).collect::<String>();
    let sep = if path.contains('?') { '&' } else { '?' };
    axum::response::Redirect::to(&format!("{path}{sep}{key}={message}")).into_response()
}

#[derive(serde::Deserialize, Default)]
pub struct Flash {
    pub ok: Option<String>,
    pub error: Option<String>,
}

pub fn flash(f: &Flash) -> Markup {
    html! {
        @if let Some(m) = &f.ok { div.notice.ok { (m) } }
        @if let Some(m) = &f.error { div.notice.error { (m) } }
    }
}

/// A QR code as inline SVG, dark on light whatever the theme, so phones read it.
pub fn qr_svg(text: &str) -> Markup {
    match qrcode::QrCode::new(text.as_bytes()) {
        Ok(code) => PreEscaped(
            code.render::<qrcode::render::svg::Color>()
                .min_dimensions(220, 220)
                .quiet_zone(true)
                .build(),
        ),
        Err(_) => html! { p.muted { "(QR code unavailable)" } },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn runbook_matches_docs() {
        let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/restore.md");
        if let Ok(text) = std::fs::read_to_string(docs) {
            assert_eq!(
                text,
                super::RUNBOOK,
                "docs/restore.md and assets/restore.md differ"
            );
        }
    }
}
