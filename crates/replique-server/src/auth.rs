//! Who is calling: the admin (password, session cookie, CSRF token) or a device
//! (its report token).

use std::sync::Arc;
use std::time::Duration;

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::body::Body;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use replique_proto::error_code;

use crate::App;
use crate::api::ApiErr;
use crate::db::{self, Device, Session};
use crate::secrets::{eq_ct, random_b64, sha256_hex};

pub const SESSION_COOKIE: &str = "replique_session";
const LOGIN_COOKIE: &str = "replique_login";
/// A session lasts this long after its last use.
const SESSION_TTL: i64 = 7 * 86_400;

pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn set_cookie(app: &App, name: &str, value: &str, max_age: i64) -> HeaderValue {
    let secure = if app.config.secure_cookies() {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    ))
    .expect("cookie values are base64url")
}

/// A logged-in admin. Handlers that take it are only reachable after login.
#[derive(Debug, Clone)]
pub struct Admin {
    /// The session cookie's hash: the key restores are unlocked under.
    pub session: String,
    pub csrf: String,
}

/// Not logged in: pages redirect to the login form, htmx requests are told
/// to.
pub struct LoginRequired {
    htmx: bool,
    next: String,
}

impl IntoResponse for LoginRequired {
    fn into_response(self) -> Response {
        if self.htmx {
            let mut r = StatusCode::UNAUTHORIZED.into_response();
            r.headers_mut()
                .insert("HX-Redirect", HeaderValue::from_static("/login"));
            return r;
        }
        let next = url::form_urlencoded::byte_serialize(self.next.as_bytes()).collect::<String>();
        Redirect::to(&format!("/login?next={next}")).into_response()
    }
}

fn session_of(app: &App, headers: &HeaderMap) -> Option<Session> {
    let token = cookie(headers, SESSION_COOKIE)?;
    let session = app.store.session(&sha256_hex(token.as_bytes())).ok()??;
    // Slide the expiry, but write at most once a minute.
    if db::now() - session.last_seen_at > 60 {
        let _ = app
            .store
            .touch_session(&session.token_hash, db::now() + SESSION_TTL);
    }
    Some(session)
}

impl FromRequestParts<Arc<App>> for Admin {
    type Rejection = LoginRequired;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &Arc<App>,
    ) -> Result<Self, Self::Rejection> {
        match session_of(app, &parts.headers) {
            Some(s) => Ok(Admin {
                session: s.token_hash,
                csrf: s.csrf,
            }),
            None => Err(LoginRequired {
                htmx: parts.headers.contains_key("hx-request"),
                next: parts
                    .uri
                    .path_and_query()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_else(|| "/".into()),
            }),
        }
    }
}

/// Every state-changing request to the website carries the session's CSRF
/// token, as the `X-CSRF-Token` header (htmx) or a `csrf` form field. The
/// device API is exempt (it has no cookies), and so is the login form, which
/// checks its own double-submit cookie.
pub async fn csrf_layer(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let path = request.uri().path();
    if safe || path.starts_with("/api/") || path == "/login" {
        return next.run(request).await;
    }
    let Some(session) = session_of(&app, request.headers()) else {
        // Not logged in: let the handler's `Admin` extractor redirect.
        return next.run(request).await;
    };
    if let Some(sent) = request
        .headers()
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        && eq_ct(sent.as_bytes(), session.csrf.as_bytes())
    {
        return next.run(request).await;
    }
    let is_form = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    if !is_form {
        return (StatusCode::FORBIDDEN, "missing CSRF token").into_response();
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 1 << 20).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "form too large").into_response();
    };
    let ok = url::form_urlencoded::parse(&bytes)
        .any(|(k, v)| k == "csrf" && eq_ct(v.as_bytes(), session.csrf.as_bytes()));
    if !ok {
        return (
            StatusCode::FORBIDDEN,
            "This form is stale (CSRF token mismatch); reload the page.",
        )
            .into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// `GET /login`: the form, with a fresh double-submit token.
pub async fn login_form(
    State(app): State<Arc<App>>,
    axum::extract::Query(q): axum::extract::Query<LoginQuery>,
) -> Response {
    let token = random_b64(16);
    let page = crate::web::pages::login(
        &token,
        q.next.as_deref(),
        q.failed.is_some(),
        app.admin_hash.is_some(),
    );
    let mut r = page.into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        set_cookie(&app, LOGIN_COOKIE, &token, 3600),
    );
    r
}

#[derive(serde::Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
    failed: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct LoginForm {
    password: crate::secrets::Secret,
    token: String,
    next: Option<String>,
}

/// Only local paths are followed after login, so the form cannot bounce
/// anyone to another site.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains('\\') => n.to_string(),
        _ => "/".into(),
    }
}

/// `POST /login`.
pub async fn login(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<LoginForm>,
) -> Response {
    let next = safe_next(form.next.as_deref());
    let cookie_ok =
        cookie(&headers, LOGIN_COOKIE).is_some_and(|c| eq_ct(c.as_bytes(), form.token.as_bytes()));
    if !cookie_ok {
        return (
            StatusCode::FORBIDDEN,
            "This login form is stale; reload the page.",
        )
            .into_response();
    }
    let Some(hash) = app.admin_hash.clone() else {
        return Redirect::to("/login?failed=1").into_response();
    };
    let password = form.password;
    let ok = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .map(|parsed| {
                Argon2::default()
                    .verify_password(password.expose().as_bytes(), &parsed)
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if !ok {
        tracing::warn!("failed admin login");
        // Slows guessing down without any state to keep.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let next = url::form_urlencoded::byte_serialize(next.as_bytes()).collect::<String>();
        return Redirect::to(&format!("/login?failed=1&next={next}")).into_response();
    }
    let token = random_b64(32);
    let t = db::now();
    let session = Session {
        token_hash: sha256_hex(token.as_bytes()),
        csrf: random_b64(24),
        last_seen_at: t,
        expires_at: t + SESSION_TTL,
    };
    if let Err(e) = app.store.insert_session(&session) {
        tracing::error!("saving a session: {e:#}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    tracing::info!("admin logged in");
    let mut r = Redirect::to(&next).into_response();
    r.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&app, SESSION_COOKIE, &token, SESSION_TTL),
    );
    r.headers_mut()
        .append(header::SET_COOKIE, set_cookie(&app, LOGIN_COOKIE, "", 0));
    r
}

/// `POST /logout`: ends the session and forgets every password it unlocked.
pub async fn logout(State(app): State<Arc<App>>, admin: Admin) -> Response {
    let _ = app.store.delete_session(&admin.session);
    app.unlocks.forget_session(&admin.session);
    let mut r = Redirect::to("/login").into_response();
    r.headers_mut()
        .insert(header::SET_COOKIE, set_cookie(&app, SESSION_COOKIE, "", 0));
    r
}

/// A device, authenticated by its report token.
pub struct DeviceAuth(pub Device);

impl FromRequestParts<Arc<App>> for DeviceAuth {
    type Rejection = ApiErr;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &Arc<App>,
    ) -> Result<Self, Self::Rejection> {
        let unauthorized = || {
            ApiErr::new(
                StatusCode::UNAUTHORIZED,
                error_code::UNAUTHORIZED,
                "Missing, unknown or revoked report token.",
            )
        };
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .ok_or_else(unauthorized)?;
        if !replique_proto::is_well_formed_token(token) {
            return Err(unauthorized());
        }
        let device = app
            .store
            .device_by_token_hash(&sha256_hex(token.as_bytes()))
            .map_err(ApiErr::internal)?
            .ok_or_else(unauthorized)?;
        if db::now() - device.last_seen_at.unwrap_or(0) > 60 {
            let _ = app.store.touch_device(&device.id);
        }
        Ok(DeviceAuth(device))
    }
}

/// Prints an argon2 hash of a password, for `admin_password_hash_file`.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    use argon2::PasswordHasher;
    use argon2::password_hash::SaltString;
    let mut salt_bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_stays_local() {
        assert_eq!(safe_next(Some("/devices/x")), "/devices/x");
        assert_eq!(safe_next(Some("//evil.example")), "/");
        assert_eq!(safe_next(Some("https://evil.example")), "/");
        assert_eq!(safe_next(None), "/");
    }

    #[test]
    fn hashes_verify() {
        let h = hash_password("correct horse").unwrap();
        let parsed = PasswordHash::new(&h).unwrap();
        assert!(
            Argon2::default()
                .verify_password(b"correct horse", &parsed)
                .is_ok()
        );
        assert!(
            Argon2::default()
                .verify_password(b"wrong", &parsed)
                .is_err()
        );
    }

    #[test]
    fn cookies_parse() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; replique_session=xyz; b=2"),
        );
        assert_eq!(cookie(&h, SESSION_COOKIE), Some("xyz"));
        assert_eq!(cookie(&h, "c"), None);
    }
}
