//! A small stand-in for Réplique's server, implementing the device endpoints the
//! client uses, as `docs/protocol.md` describes them, and recording what it got.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use replique_proto::*;
use serde_json::{Value, json};

pub const TOKEN: &str = "rpl_q2V0aGlzLWlzLWEtZmFrZS10b2tlbi1mb3ItdGVzdHM";
pub const DEVICE_ID: &str = "dev_TEST";

#[derive(Debug, Clone)]
pub struct MockRun {
    pub start: StartRun,
    pub progress: Vec<Progress>,
    pub finish: Option<FinishRun>,
}

#[derive(Default)]
pub struct Recorded {
    /// While true, every endpoint answers 503, like a server that is down.
    pub down: bool,
    pub pair_requests: Vec<PairRequest>,
    pub storage: Option<StorageInfo>,
    pub job_puts: Vec<(String, JobSpec)>,
    pub runs: BTreeMap<String, MockRun>,
    pub by_client_id: HashMap<String, String>,
    pub start_calls: usize,
    pub mirror_sources: Vec<MirrorSource>,
    /// Files received on `PUT /vault/{name}`.
    pub vault: Vec<(String, Vec<u8>)>,
}

impl Recorded {
    pub fn runs_of(&self, kind: RunKind) -> Vec<MockRun> {
        self.runs
            .values()
            .filter(|r| r.start.kind == kind)
            .cloned()
            .collect()
    }
}

#[derive(Clone)]
pub struct Mock {
    pub url: String,
    pub rec: Arc<Mutex<Recorded>>,
}

impl Mock {
    pub fn start() -> Mock {
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let app = Router::new()
            .route(paths::PAIR, post(pair))
            .route(paths::JOB, put(put_job))
            .route(paths::VAULT, put(put_vault))
            .route(paths::RUNS, post(start_run))
            .route(paths::RUN_PROGRESS, post(progress))
            .route(paths::RUN_FINISH, post(finish))
            .route(paths::MIRROR_SOURCES, axum::routing::get(mirror_sources))
            .with_state(rec.clone());
        let (tx, rx) = std::sync::mpsc::channel::<SocketAddr>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        let addr = rx.recv().unwrap();
        Mock {
            url: format!("http://{addr}"),
            rec,
        }
    }

    pub fn set_down(&self, down: bool) {
        self.rec.lock().unwrap().down = down;
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut Recorded) -> T) -> T {
        f(&mut self.rec.lock().unwrap())
    }
}

type Shared = State<Arc<Mutex<Recorded>>>;

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "error": code, "message": message }))).into_response()
}

fn gate(rec: &Recorded, headers: &HeaderMap, auth: bool) -> Option<Response> {
    if rec.down {
        return Some(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream",
            "down for the test",
        ));
    }
    if auth {
        let ok = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == format!("Bearer {TOKEN}"));
        if !ok {
            return Some(error(StatusCode::UNAUTHORIZED, "unauthorized", "bad token"));
        }
    }
    None
}

async fn pair(State(rec): Shared, headers: HeaderMap, Json(req): Json<PairRequest>) -> Response {
    let mut rec = rec.lock().unwrap();
    if let Some(r) = gate(&rec, &headers, false) {
        return r;
    }
    if normalize_pairing_code(&req.code) != "K7QM2XPA" {
        return error(StatusCode::BAD_REQUEST, "invalid_code", "unknown code");
    }
    rec.pair_requests.push(req);
    let storage = rec.storage.clone().unwrap_or(StorageInfo {
        host: "box.example".into(),
        port: 23,
        username: "u1-sub1".into(),
        repo_path: DEVICE_REPO_PATH.into(),
        known_hosts: vec!["[box.example]:23 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIICf9svRenC/PLKIL9nk6K/pxQgoiFC41wTNvoIncOxs".into()],
    });
    Json(PairResponse {
        device_id: DEVICE_ID.into(),
        device_name: "PC".into(),
        report_token: TOKEN.into(),
        storage,
    })
    .into_response()
}

async fn put_vault(
    State(rec): Shared,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let mut rec = rec.lock().unwrap();
    if let Some(r) = gate(&rec, &headers, true) {
        return r;
    }
    rec.vault.push((name, body.to_vec()));
    Json(Ack::OK).into_response()
}

async fn put_job(
    State(rec): Shared,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
    Json(spec): Json<JobSpec>,
) -> Response {
    let mut rec = rec.lock().unwrap();
    if let Some(r) = gate(&rec, &headers, true) {
        return r;
    }
    if !valid_job_name(&name) {
        return error(StatusCode::BAD_REQUEST, "bad_request", "bad job name");
    }
    rec.job_puts.push((name, spec));
    Json(Ack::OK).into_response()
}

async fn start_run(State(rec): Shared, headers: HeaderMap, Json(s): Json<StartRun>) -> Response {
    let mut rec = rec.lock().unwrap();
    if let Some(r) = gate(&rec, &headers, true) {
        return r;
    }
    rec.start_calls += 1;
    // The same client_run_id always maps to the same run.
    if let Some(id) = rec.by_client_id.get(&s.client_run_id) {
        return Json(StartRunReply { run_id: id.clone() }).into_response();
    }
    let id = format!("run_{}", rec.runs.len() + 1);
    rec.by_client_id.insert(s.client_run_id.clone(), id.clone());
    rec.runs.insert(
        id.clone(),
        MockRun {
            start: s,
            progress: vec![],
            finish: None,
        },
    );
    Json(StartRunReply { run_id: id }).into_response()
}

async fn progress(
    State(rec): Shared,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Json(p): Json<Progress>,
) -> Response {
    let mut rec = rec.lock().unwrap();
    if let Some(r) = gate(&rec, &headers, true) {
        return r;
    }
    match rec.runs.get_mut(&id) {
        Some(run) => {
            run.progress.push(p);
            Json(Ack::OK).into_response()
        }
        None => error(StatusCode::NOT_FOUND, "not_found", "no such run"),
    }
}

async fn finish(
    State(rec): Shared,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Json(f): Json<FinishRun>,
) -> Response {
    let mut rec = rec.lock().unwrap();
    if let Some(r) = gate(&rec, &headers, true) {
        return r;
    }
    match rec.runs.get_mut(&id) {
        Some(run) => match &run.finish {
            Some(existing) if *existing != f => {
                error(StatusCode::CONFLICT, "conflict", "already finished")
            }
            _ => {
                run.finish = Some(f);
                Json(Ack::OK).into_response()
            }
        },
        None => error(StatusCode::NOT_FOUND, "not_found", "no such run"),
    }
}

async fn mirror_sources(State(rec): Shared, headers: HeaderMap) -> Response {
    let rec = rec.lock().unwrap();
    if let Some(r) = gate(&rec, &headers, true) {
        return r;
    }
    Json(MirrorSources {
        sources: rec.mirror_sources.clone(),
    })
    .into_response()
}

/// A client with its own configuration and state directory.
pub struct Client {
    pub dir: tempfile::TempDir,
    pub config: PathBuf,
    pub state: PathBuf,
}

impl Client {
    pub fn new() -> Client {
        let dir = tempfile::tempdir().unwrap();
        // Canonical, so paths compare equal to what restic records (macOS's
        // temporary folder is behind a symlink).
        let root = dir.path().canonicalize().unwrap();
        Client {
            config: root.join("client.json"),
            state: root.join("state"),
            dir,
        }
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    pub fn write_config(&self, config: Value) {
        std::fs::write(&self.config, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_replique-client"));
        c.env("REPLIQUE_CONFIG", &self.config)
            .env("REPLIQUE_STATE_DIR", &self.state)
            .env_remove("RESTIC_REPOSITORY")
            .env_remove("RESTIC_PASSWORD")
            .args(args);
        c
    }

    pub fn run(&self, args: &[&str]) -> Output {
        let out = self.cmd(args).output().unwrap();
        eprintln!(
            "$ replique-client {}\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// Pairs with the mock using a generated key, without touching a repository.
    pub fn pair(&self, mock: &Mock) {
        let out = self.run(&["pair", "--yes", "--no-init", &mock.url, "K7QM-2XPA"]);
        assert!(out.status.success(), "pairing failed");
    }

    pub fn spooled(&self) -> usize {
        std::fs::read_dir(self.state.join("spool"))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .count()
            })
            .unwrap_or(0)
    }
}

pub fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// restic on a local repository, outside the client, to set up and inspect.
pub fn restic(repo: &Path, password: &str, args: &[&str]) -> Output {
    let out = Command::new("restic")
        .env("RESTIC_REPOSITORY", repo)
        .env("RESTIC_PASSWORD", password)
        .env_remove("RESTIC_PASSWORD_FILE")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "restic {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

pub fn snapshot_ids(repo: &Path, password: &str) -> Vec<String> {
    let out = restic(repo, password, &["snapshots", "--json"]);
    let v: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    v.iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect()
}
