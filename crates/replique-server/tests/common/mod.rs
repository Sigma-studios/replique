//! Shared test harness: a server on a random port, a logged-in browser, a
//! mock of Hetzner's API, a mock Discord webhook, and captured logs.
#![allow(dead_code, clippy::result_large_err)]

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use replique_server::App;
use replique_server::config::Config;
use serde_json::{Value, json};

// ---------------------------------------------------------------- logs

static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static INIT: Once = Once::new();

struct Capture;

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every log line of the test binary, at every level, goes to a buffer the
/// tests can search.
pub fn init_logs() {
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter("trace")
            .with_writer(|| Capture)
            .with_ansi(false)
            .init();
    });
}

pub fn logs() -> String {
    String::from_utf8_lossy(&LOGS.lock().unwrap()).into_owned()
}

// ---------------------------------------------------------------- server

pub const ADMIN_PASSWORD: &str = "correct horse battery staple";

pub struct TestServer {
    pub app: Arc<App>,
    pub url: String,
    pub dir: tempfile::TempDir,
}

impl TestServer {
    pub fn local_root(&self) -> PathBuf {
        self.dir.path().join("box")
    }
}

/// Starts a server whose repositories are folders under `<tmp>/box`;
/// `extra` is appended to its TOML configuration.
pub async fn start(extra: &str) -> TestServer {
    start_with(extra, true).await
}

/// Like `start`, optionally without the local root (to reach a real SFTP
/// server).
pub async fn start_with(extra: &str, local_root: bool) -> TestServer {
    init_logs();
    let dir = tempfile::tempdir().unwrap();
    let hash_file = dir.path().join("admin.hash");
    std::fs::write(
        &hash_file,
        replique_server::auth::hash_password(ADMIN_PASSWORD).unwrap(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let toml = format!(
        r#"
listen = "{addr}"
public_url = "http://{addr}"
state_dir = "{state}"
admin_password_hash_file = "{hash}"
owner_name = "Sigma"
[measurement]
enabled = false
[storage_box]
{root}
{extra}
"#,
        state = dir.path().join("state").display(),
        hash = hash_file.display(),
        root = if local_root {
            format!("local_root = {:?}", dir.path().join("box").display().to_string())
        } else {
            String::new()
        },
    );
    let config = Config::parse(&toml).unwrap();
    let mut app = App::new(config).unwrap();
    app.hetzner_poll = Duration::from_millis(10);
    app.self_exe = env!("CARGO_BIN_EXE_replique-server").into();
    let app = Arc::new(app);
    let router = replique_server::router(app.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    TestServer {
        app,
        url: format!("http://{addr}"),
        dir,
    }
}

/// A browser with a cookie jar, logged in as the admin.
pub struct Browser {
    pub http: reqwest::Client,
    pub url: String,
    pub csrf: String,
}

pub fn extract(html: &str, before: &str, after: &str) -> Option<String> {
    let start = html.find(before)? + before.len();
    let end = html[start..].find(after)? + start;
    Some(html[start..end].to_string())
}

impl Browser {
    pub async fn login(url: &str) -> Browser {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let page = http
            .get(format!("{url}/login"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let token = extract(&page, "name=\"token\" value=\"", "\"").expect("login token");
        let r = http
            .post(format!("{url}/login"))
            .form(&[("password", ADMIN_PASSWORD), ("token", &token)])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER, "login failed");
        let home = http
            .get(format!("{url}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let csrf = extract(&home, "name=\"csrf\" value=\"", "\"").expect("csrf token");
        Browser {
            http,
            url: url.to_string(),
            csrf,
        }
    }

    pub async fn get(&self, path: &str) -> (StatusCode, String) {
        let r = self
            .http
            .get(format!("{}{path}", self.url))
            .send()
            .await
            .unwrap();
        let status = r.status();
        (status, r.text().await.unwrap())
    }

    /// Posts a form with the CSRF token; returns the status and `Location`.
    pub async fn post(&self, path: &str, fields: &[(&str, &str)]) -> (StatusCode, String) {
        let mut form: Vec<(&str, &str)> = fields.to_vec();
        form.push(("csrf", &self.csrf));
        let r = self
            .http
            .post(format!("{}{path}", self.url))
            .form(&form)
            .send()
            .await
            .unwrap();
        let status = r.status();
        let location = r
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let body = r.text().await.unwrap();
        (status, location.unwrap_or(body))
    }
}

// ---------------------------------------------------------------- devices

/// What a device would generate at pairing: an ed25519 public key line.
pub fn device_public_key() -> String {
    let dir = tempfile::tempdir().unwrap();
    let key = replique_server::ssh::load_or_create_key(&dir.path().join("k")).unwrap();
    format!(
        "{} test-device",
        replique_server::ssh::key_line(key.public_key())
    )
}

/// The live pairing code of a device, read from the website's page.
pub async fn pairing_code(browser: &Browser, device_id: &str) -> String {
    let (_, page) = browser.get(&format!("/devices/{device_id}")).await;
    extract(
        &page,
        "Code <strong class=\"mono\" style=\"font-size: 1.3rem\">",
        "<",
    )
    .expect("pairing code on the page")
}

// ---------------------------------------------------------------- restic

pub fn restic(repo: &Path, password: &str, args: &[&str]) -> std::process::Output {
    let mut child = std::process::Command::new("restic")
        .arg("-r")
        .arg(repo)
        .args(args)
        .env_remove("RESTIC_PASSWORD")
        .env_remove("RESTIC_REPOSITORY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("restic must be installed");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{password}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success() || args.first() == Some(&"cat"),
        "restic {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

// ---------------------------------------------------------------- Hetzner mock

/// A stand-in for `https://api.hetzner.com/v1`, answering with the examples of
/// Hetzner's OpenAPI spec (`tests/hetzner/*.json`, extracted from
/// https://docs.hetzner.cloud/hetzner.spec.json) adjusted to what was asked.
/// Actions answer `running` on their first poll and `success` after, so the
/// client's polling is exercised.
#[derive(Default)]
pub struct MockHetzner {
    pub requests: Mutex<Vec<(String, String, Value)>>,
    subs: Mutex<HashMap<u64, Value>>,
    polls: Mutex<HashMap<u64, u32>>,
    next: Mutex<u64>,
    pub used_bytes: Mutex<u64>,
    /// Make the next action of this command fail.
    pub fail_command: Mutex<Option<String>>,
}

pub const MOCK_TOKEN: &str = "test-token";
pub const MOCK_BOX: u64 = 42;

pub fn example(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/hetzner")
        .join(format!("{name}.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

type Mock = Arc<MockHetzner>;

fn authorized(headers: &HeaderMap) -> Result<(), axum::response::Response> {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {MOCK_TOKEN}"));
    if ok {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(example("get_storage_box")["error"].clone()),
        )
            .into_response())
    }
}

impl MockHetzner {
    fn record(&self, method: &str, path: String, body: Value) {
        self.requests
            .lock()
            .unwrap()
            .push((method.into(), path, body));
    }

    fn action(&self, name: &str, command: &str) -> Value {
        let mut a = example(name)["response"]["action"].clone();
        let id = {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            1000 + *n
        };
        a["id"] = json!(id);
        a["command"] = json!(command);
        a["status"] = json!("running");
        a["error"] = Value::Null;
        if self.fail_command.lock().unwrap().as_deref() == Some(command) {
            *self.fail_command.lock().unwrap() = None;
            self.polls.lock().unwrap().insert(id, 1000);
        } else {
            self.polls.lock().unwrap().insert(id, 0);
        }
        a
    }

    pub fn subaccounts(&self) -> Vec<Value> {
        self.subs.lock().unwrap().values().cloned().collect()
    }

    pub fn commands(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m != "GET")
            .map(|(m, p, _)| format!("{m} {p}"))
            .collect()
    }
}

async fn get_box(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath(id): AxPath<u64>,
) -> axum::response::Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    m.record("GET", format!("/storage_boxes/{id}"), Value::Null);
    let mut b = example("get_storage_box")["response"].clone();
    b["storage_box"]["id"] = json!(id);
    b["storage_box"]["storage_box_type"]["size"] = json!(1_000_000_000_000u64);
    b["storage_box"]["stats"]["size"] = json!(*m.used_bytes.lock().unwrap());
    b["storage_box"]["stats"]["size_data"] = json!(*m.used_bytes.lock().unwrap());
    axum::Json(b).into_response()
}

async fn list_subs(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath(id): AxPath<u64>,
) -> axum::response::Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    m.record(
        "GET",
        format!("/storage_boxes/{id}/subaccounts"),
        Value::Null,
    );
    axum::Json(json!({ "subaccounts": m.subaccounts() })).into_response()
}

async fn create_sub(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath(id): AxPath<u64>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    m.record(
        "POST",
        format!("/storage_boxes/{id}/subaccounts"),
        body.clone(),
    );
    // The spec requires these two.
    let (Some(home), Some(_)) = (body["home_directory"].as_str(), body["password"].as_str()) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "missing fields").into_response();
    };
    let sid = {
        let mut n = m.next.lock().unwrap();
        *n += 1;
        *n
    };
    let mut sub = example("get_subaccount")["response"]["subaccount"].clone();
    sub["id"] = json!(sid);
    sub["storage_box"] = json!(id);
    sub["username"] = json!(format!("u1337-sub{sid}"));
    sub["server"] = json!(format!("u1337-sub{sid}.your-storagebox.de"));
    sub["home_directory"] = json!(home);
    sub["access_settings"] = body["access_settings"].clone();
    sub["description"] = body["description"].clone();
    m.subs.lock().unwrap().insert(sid, sub);
    let mut r = example("create_subaccount")["response"].clone();
    r["subaccount"] = json!({ "id": sid, "storage_box": id });
    r["action"] = m.action("create_subaccount", "create_subaccount");
    (StatusCode::CREATED, axum::Json(r)).into_response()
}

async fn get_sub(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath((id, sid)): AxPath<(u64, u64)>,
) -> axum::response::Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    m.record(
        "GET",
        format!("/storage_boxes/{id}/subaccounts/{sid}"),
        Value::Null,
    );
    match m.subs.lock().unwrap().get(&sid) {
        Some(s) => axum::Json(json!({ "subaccount": s })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": {"code": "not_found", "message": "Subaccount not found", "details": null}})),
        )
            .into_response(),
    }
}

async fn delete_sub(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath((id, sid)): AxPath<(u64, u64)>,
) -> axum::response::Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    m.record(
        "DELETE",
        format!("/storage_boxes/{id}/subaccounts/{sid}"),
        Value::Null,
    );
    if m.subs.lock().unwrap().remove(&sid).is_none() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": {"code": "not_found", "message": "Subaccount not found", "details": null}})),
        )
            .into_response();
    }
    let action = m.action("delete_subaccount", "delete_subaccount");
    (StatusCode::CREATED, axum::Json(json!({ "action": action }))).into_response()
}

async fn sub_action(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath((id, sid, name)): AxPath<(u64, u64, String)>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    m.record(
        "POST",
        format!("/storage_boxes/{id}/subaccounts/{sid}/actions/{name}"),
        body.clone(),
    );
    let example_name = match name.as_str() {
        "update_access_settings" | "reset_subaccount_password" | "change_home_directory" => {
            name.clone()
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // Each example request's fields are what the endpoint takes.
    let wanted = example(&example_name)["request"].clone();
    for key in wanted.as_object().unwrap().keys() {
        if body.get(key).is_none() {
            return (StatusCode::UNPROCESSABLE_ENTITY, format!("missing {key}")).into_response();
        }
    }
    if let Some(sub) = m.subs.lock().unwrap().get_mut(&sid) {
        if name == "change_home_directory" {
            sub["home_directory"] = body["home_directory"].clone();
        }
        if name == "update_access_settings" {
            sub["access_settings"] = body.clone();
        }
    }
    let action = m.action(&example_name, &name);
    (StatusCode::CREATED, axum::Json(json!({ "action": action }))).into_response()
}

async fn get_action(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath(aid): AxPath<u64>,
) -> axum::response::Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    m.record("GET", format!("/storage_boxes/actions/{aid}"), Value::Null);
    let mut a = example("get_action")["response"].clone();
    let polls = {
        let mut p = m.polls.lock().unwrap();
        let n = p.entry(aid).or_insert(0);
        *n += 1;
        *n
    };
    a["action"]["id"] = json!(aid);
    if polls > 1000 {
        a["action"]["status"] = json!("error");
        a["action"]["error"] = json!({"code": "action_failed", "message": "Action failed"});
    } else if polls >= 2 {
        a["action"]["status"] = json!("success");
        a["action"]["error"] = Value::Null;
    } else {
        a["action"]["status"] = json!("running");
        a["action"]["error"] = Value::Null;
    }
    axum::Json(a).into_response()
}

/// Starts the mock; returns its base URL.
pub async fn mock_hetzner() -> (Mock, String) {
    let mock = Mock::default();
    let router = Router::new()
        .route("/storage_boxes/{id}", get(get_box))
        .route(
            "/storage_boxes/{id}/subaccounts",
            get(list_subs).post(create_sub),
        )
        .route(
            "/storage_boxes/{id}/subaccounts/{sid}",
            get(get_sub).delete(delete_sub),
        )
        .route(
            "/storage_boxes/{id}/subaccounts/{sid}/actions/{name}",
            post(sub_action),
        )
        .route("/storage_boxes/actions/{aid}", get(get_action))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (mock, url)
}

// ---------------------------------------------------------------- Discord mock

pub type Posts = Arc<Mutex<Vec<Value>>>;

pub async fn mock_discord() -> (Posts, String) {
    let posts = Posts::default();
    let state = posts.clone();
    let router = Router::new()
        .route(
            "/api/webhooks/1/secret",
            post(
                |State(p): State<Posts>, axum::Json(v): axum::Json<Value>| async move {
                    p.lock().unwrap().push(v);
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/webhooks/1/secret",
        listener.local_addr().unwrap()
    );
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (posts, url)
}

/// Waits until `f` holds, for up to five seconds.
pub async fn eventually(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}
