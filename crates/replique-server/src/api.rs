//! The device API (`/api/v1`), exactly as `docs/protocol.md` describes it.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use replique_proto::{
    Ack, ApiError, DeviceConfig, DeviceKind, FinishRun, JobSpec, MirrorSource, MirrorSources,
    PairRequest, Progress, RunKind, RunStatus, SafeSnapshot, SafeToDelete, StartRun, StartRunReply,
    StorageInfo, error_code, paths,
};
use serde::de::DeserializeOwned;

use crate::App;
use crate::auth::DeviceAuth;
use crate::config::MeasureVia;
use crate::db::{self, Run};
use crate::events::Event;

/// A non-2xx answer, with the protocol's error body.
#[derive(Debug)]
pub struct ApiErr {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiErr {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiErr {
        ApiErr {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> ApiErr {
        ApiErr::new(StatusCode::BAD_REQUEST, error_code::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> ApiErr {
        ApiErr::new(StatusCode::NOT_FOUND, error_code::NOT_FOUND, message)
    }

    pub fn forbidden(message: impl Into<String>) -> ApiErr {
        ApiErr::new(StatusCode::FORBIDDEN, error_code::FORBIDDEN, message)
    }

    pub fn internal(e: anyhow::Error) -> ApiErr {
        tracing::error!("device API: {e:#}");
        ApiErr::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            error_code::UPSTREAM,
            "The server failed; try again later.",
        )
    }
}

impl From<anyhow::Error> for ApiErr {
    fn from(e: anyhow::Error) -> ApiErr {
        ApiErr::internal(e)
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiError {
                error: self.code.to_string(),
                message: self.message,
            }),
        )
            .into_response()
    }
}

/// `Json` whose rejections use the protocol's error body.
pub struct ApiJson<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiErr;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(e) => Err(rejection(e)),
        }
    }
}

fn rejection(e: JsonRejection) -> ApiErr {
    ApiErr::bad_request(e.body_text())
}

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route(paths::PAIR, post(pair))
        .route(paths::JOB, put(put_job))
        .route(paths::DEVICE_CONFIG, get(device_config))
        .route(paths::RUNS, post(start_run))
        .route(paths::RUN_PROGRESS, post(progress))
        .route(paths::RUN_FINISH, post(finish))
        .route(paths::SAFE_TO_DELETE, get(safe_to_delete))
        .route(paths::MIRROR_SOURCES, get(mirror_sources))
        .route(
            paths::VAULT,
            put(put_vault).layer(axum::extract::DefaultBodyLimit::max(
                replique_proto::MAX_VAULT_BYTES,
            )),
        )
}

/// `PUT /api/v1/vault/{name}`: a file the device sends as it is, encrypted
/// already (see `vault`). Which file is the device's choice, never the
/// server's.
async fn put_vault(
    State(app): State<Arc<App>>,
    DeviceAuth(device): DeviceAuth,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Json<Ack>, ApiErr> {
    if !replique_proto::valid_job_name(&name) {
        return Err(ApiErr::bad_request("Invalid name."));
    }
    if body.is_empty() {
        return Err(ApiErr::bad_request("Empty file."));
    }
    crate::vault::receive(&app, &device, &name, &body)
        .await
        .map_err(ApiErr::internal)?;
    Ok(Json(Ack::OK))
}

async fn pair(
    State(app): State<Arc<App>>,
    ApiJson(req): ApiJson<PairRequest>,
) -> Result<Json<replique_proto::PairResponse>, ApiErr> {
    crate::provision::pair(&app, req).await.map(Json)
}

async fn put_job(
    State(app): State<Arc<App>>,
    DeviceAuth(device): DeviceAuth,
    Path(name): Path<String>,
    ApiJson(spec): ApiJson<JobSpec>,
) -> Result<Json<Ack>, ApiErr> {
    if !replique_proto::valid_job_name(&name) {
        return Err(ApiErr::bad_request(
            "Job names are 1 to 64 characters of a-z, 0-9, - and _.",
        ));
    }
    if app.store.declare_job(&device.id, &name, &spec)? {
        tracing::info!(device = %device.name, job = %name, "job declared");
        app.hub.publish(Event::Runs {
            device_id: device.id.clone(),
        });
    }
    Ok(Json(Ack::OK))
}

/// The phone's rules as stored, or the default until the admin edits them.
pub fn config_for(app: &App, device_id: &str) -> anyhow::Result<DeviceConfig> {
    Ok(app
        .store
        .device_config(device_id)?
        .map(|(c, _)| c)
        .unwrap_or_else(DeviceConfig::android_default))
}

async fn device_config(
    State(app): State<Arc<App>>,
    DeviceAuth(device): DeviceAuth,
    headers: HeaderMap,
) -> Result<Response, ApiErr> {
    let config = config_for(&app, &device.id)?;
    let body = serde_json::to_string(&config).map_err(anyhow::Error::from)?;
    let etag = format!("\"{}\"", &crate::secrets::sha256_hex(body.as_bytes())[..16]);
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    let etag_header = HeaderValue::from_str(&etag).expect("hex");
    if matches {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag_header)]).into_response());
    }
    Ok((
        [
            (header::ETAG, etag_header),
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
        ],
        body,
    )
        .into_response())
}

async fn start_run(
    State(app): State<Arc<App>>,
    DeviceAuth(device): DeviceAuth,
    ApiJson(start): ApiJson<StartRun>,
) -> Result<Json<StartRunReply>, ApiErr> {
    if !replique_proto::valid_job_name(&start.job) {
        return Err(ApiErr::bad_request("Invalid job name."));
    }
    if start.client_run_id.is_empty() || start.client_run_id.len() > 64 {
        return Err(ApiErr::bad_request("client_run_id must be a UUID."));
    }
    let job_id = app.store.job_for_run(&device.id, &start.job, start.kind)?;
    let run = Run {
        id: db::new_id("run"),
        device_id: device.id.clone(),
        job_id,
        client_run_id: start.client_run_id.clone(),
        kind: start.kind,
        status: RunStatus::Running,
        started_at: start.started_at.timestamp(),
        received_at: db::now(),
        finished_at: None,
        progress: None,
        progress_at: None,
        finish: None,
        finish_raw: None,
        exit_code: None,
        restic_version: start.restic_version.clone(),
        client_version: start.client_version.clone(),
        peak_bps: None,
        sent_bytes: None,
    };
    let (run, created) = app.store.start_run(&run)?;
    if created {
        tracing::info!(device = %device.name, job = %start.job, kind = ?start.kind, run = %run.id, "run started");
        close_superseded(&app, &run)?;
        crate::alerts::on_run_started(&app, &device, job_id).await;
        app.hub.publish(Event::Runs {
            device_id: device.id.clone(),
        });
    }
    Ok(Json(StartRunReply { run_id: run.id }))
}

/// What the server writes on a run it closed itself; a real report arriving
/// later for that run (replayed from the device's spool) still replaces it.
pub const SUPERSEDED: &str = "Stopped without reporting (the device restarted, or its app was updated or killed); a newer run of this job took over and continues from what was already uploaded.";

/// A device runs a job's steps one after another (backup, then prune, then
/// check), never two at once, so any run of the job starting means an older
/// one still marked running is over. Closing it as interrupted now, rather
/// than waiting hours to call it lost, keeps a phone update or a reboot from
/// raising a "run lost" alert.
fn close_superseded(app: &App, new: &Run) -> Result<(), ApiErr> {
    for old in app.store.running_runs()? {
        if old.id == new.id
            || old.device_id != new.device_id
            || old.job_id != new.job_id
            || old.started_at > new.started_at
        {
            continue;
        }
        let finish = FinishRun {
            status: RunStatus::Interrupted,
            exit_code: 130,
            finished_at: chrono::DateTime::from_timestamp(new.started_at, 0)
                .unwrap_or_else(chrono::Utc::now),
            summary: None,
            prune: None,
            check: None,
            mirror: None,
            errors: vec![],
            error_count: 0,
            unreadable_paths: vec![],
            message: Some(SUPERSEDED.into()),
        };
        let raw = serde_json::to_string(&finish).map_err(|e| ApiErr::internal(e.into()))?;
        app.store.finish_run(&old.id, &finish, &raw)?;
        tracing::info!(run = %old.id, by = %new.id, "run closed: a newer run of its job started");
    }
    Ok(())
}

fn closed_by_server(run: &Run) -> bool {
    run.finish
        .as_ref()
        .is_some_and(|f| f.message.as_deref() == Some(SUPERSEDED))
}

/// The run `id`, if it is this device's.
fn own_run(app: &App, device_id: &str, id: &str) -> Result<Run, ApiErr> {
    let run = app
        .store
        .run(id)?
        .ok_or_else(|| ApiErr::not_found("No such run."))?;
    if run.device_id != device_id {
        return Err(ApiErr::forbidden("This run belongs to another device."));
    }
    Ok(run)
}

async fn progress(
    State(app): State<Arc<App>>,
    DeviceAuth(device): DeviceAuth,
    Path(id): Path<String>,
    ApiJson(progress): ApiJson<Progress>,
) -> Result<Json<Ack>, ApiErr> {
    let run = own_run(&app, &device.id, &id)?;
    // Progress for a finished run, or faster than devices are asked to send,
    // is acknowledged and dropped.
    let too_soon = run
        .progress_at
        .is_some_and(|at| db::now() - at < (replique_proto::PROGRESS_MIN_INTERVAL_SECS as i64) / 2);
    if run.status == RunStatus::Running && !too_soon {
        // Before the report replaces the previous one: a mirror's rate is the
        // difference between the two.
        let (rate, sent) = crate::transfer::sample(&run, &progress);
        app.store.set_progress(&id, &progress)?;
        if rate.is_some() || sent.is_some() {
            app.store.record_transfer(&id, rate, sent)?;
        }
        app.hub.publish(Event::Runs {
            device_id: device.id.clone(),
        });
    }
    Ok(Json(Ack::OK))
}

async fn finish(
    State(app): State<Arc<App>>,
    DeviceAuth(device): DeviceAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Ack>, ApiErr> {
    let finish: FinishRun = serde_json::from_slice(&body)
        .map_err(|e| ApiErr::bad_request(format!("Invalid finish report: {e}")))?;
    if finish.status == RunStatus::Running {
        return Err(ApiErr::bad_request("A finished run cannot be `running`."));
    }
    let run = own_run(&app, &device.id, &id)?;
    if run.status != RunStatus::Running && run.status != RunStatus::Lost && !closed_by_server(&run)
    {
        // A replay from the spool repeats what was sent; anything else is a
        // second, different ending.
        return if run.finish.as_ref() == Some(&finish) {
            Ok(Json(Ack::OK))
        } else {
            Err(ApiErr::new(
                StatusCode::CONFLICT,
                error_code::CONFLICT,
                "This run already finished with a different report.",
            ))
        };
    }
    let raw = String::from_utf8_lossy(&body);
    app.store.finish_run(&id, &finish, &raw)?;
    tracing::info!(device = %device.name, run = %id, status = finish.status.as_str(), "run finished");
    if let Some(job) = app.store.job(run.job_id)? {
        crate::alerts::on_run_finished(&app, &device, &job, &run, &finish).await;
    }
    app.hub.publish(Event::Runs {
        device_id: device.id.clone(),
    });
    Ok(Json(Ack::OK))
}

#[derive(serde::Deserialize)]
struct SinceQuery {
    since: Option<replique_proto::Timestamp>,
}

async fn safe_to_delete(
    State(app): State<Arc<App>>,
    DeviceAuth(device): DeviceAuth,
    Path(id): Path<String>,
    Query(q): Query<SinceQuery>,
) -> Result<Json<SafeToDelete>, ApiErr> {
    if id != device.id {
        return Err(ApiErr::forbidden("A device may only ask about itself."));
    }
    Ok(Json(SafeToDelete {
        snapshot: safest_snapshot(&app, &device.id, q.since.map(|t| t.timestamp()))?,
    }))
}

/// restic ids may be sent full or short; a prefix match covers both.
fn same_snapshot(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

/// The newest snapshot both verified on the box and present in a mirror.
pub fn safest_snapshot(
    app: &App,
    device_id: &str,
    since: Option<i64>,
) -> anyhow::Result<Option<SafeSnapshot>> {
    let backups = app.store.successful_runs(device_id, RunKind::Backup)?;
    let mut partials = app.store.runs_of_device(device_id, 200)?;
    partials.retain(|r| r.kind == RunKind::Backup && r.status == RunStatus::Partial);
    let checks = app.store.successful_runs(device_id, RunKind::Check)?;
    // Mirror runs that vouch for what they list: no broken copy, and files
    // broken on this device's side (if any) cleared by this device's own check,
    // passed after the last of them was first seen. That check fails on a
    // truncated pack in use, so a pass means the broken files are unused (see
    // `mirrors`). Deleting originals from the phone rests on this: when in
    // doubt, a mirror run does not count.
    let mut histories = crate::mirrors::Histories::default();
    let mut mirrors = Vec::new();
    for (m, name) in app.store.mirror_runs_of(device_id)? {
        let Some(mi) = m.finish.as_ref().and_then(|f| f.mirror.as_ref()) else {
            continue;
        };
        if !mi.hash_mismatches.is_empty() {
            continue;
        }
        if !mi.source_mismatches.is_empty() {
            let vouched = match histories.of_run(app, &m)? {
                Some(found) => crate::mirrors::vouched_at(app, device_id, found.since)?.is_some(),
                None => false,
            };
            if !vouched {
                continue;
            }
        }
        mirrors.push((m, name));
    }
    let mut snapshots: Vec<(String, i64)> = backups
        .iter()
        .chain(partials.iter())
        .filter_map(|r| {
            let summary = r.finish.as_ref()?.summary.as_ref()?;
            let id = summary.snapshot_id.clone()?;
            let time = summary
                .backup_start
                .map(|t| t.timestamp())
                .unwrap_or(r.started_at);
            Some((id, time))
        })
        .collect();
    snapshots.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
    for (snapshot_id, time) in snapshots {
        if since.is_some_and(|s| time <= s) {
            break;
        }
        let verified_at = checks
            .iter()
            .filter(|c| c.finished_at.unwrap_or(0) >= time)
            .filter(|c| {
                c.finish
                    .as_ref()
                    .and_then(|f| f.check.as_ref())
                    .is_some_and(|ch| {
                        ch.num_errors == 0
                            && ch
                                .snapshot_ids
                                .iter()
                                .any(|s| same_snapshot(s, &snapshot_id))
                    })
            })
            .filter_map(|c| c.finished_at)
            .min();
        let mirrored = mirrors
            .iter()
            .filter(|(m, _)| {
                m.finish
                    .as_ref()
                    .and_then(|f| f.mirror.as_ref())
                    .is_some_and(|mi| {
                        mi.snapshot_ids
                            .iter()
                            .any(|s| same_snapshot(s, &snapshot_id))
                    })
            })
            .filter_map(|(m, name)| m.finished_at.map(|at| (at, name.clone())))
            .min_by_key(|(at, _)| *at);
        if let (Some(verified_at), Some((mirrored_at, mirrored_by))) = (verified_at, mirrored) {
            return Ok(Some(SafeSnapshot {
                snapshot_id,
                snapshot_time: db::to_time(time),
                verified_at: db::to_time(verified_at),
                mirrored_at: db::to_time(mirrored_at),
                mirrored_by: Some(mirrored_by),
            }));
        }
    }
    Ok(None)
}

async fn mirror_sources(
    State(app): State<Arc<App>>,
    DeviceAuth(device): DeviceAuth,
) -> Result<Json<MirrorSources>, ApiErr> {
    let mut sources = Vec::new();
    for source_id in app.store.mirror_sources_of(&device.id)? {
        let (Some(source), Some(repo)) = (
            app.store.device(&source_id)?,
            app.store.repository_of(&source_id)?,
        ) else {
            continue;
        };
        // Through the source's read-only twin, whose home is the repository
        // folder; without twins, through the source's own login, on which
        // assigning the mirror installed this device's key.
        let (username, repo_path) = match app.config.storage_box.measure_via {
            MeasureVia::Twin => match repo.twin_username.clone() {
                Some(u) => (u, replique_proto::TWIN_REPO_PATH.to_string()),
                None => continue,
            },
            MeasureVia::Main => (repo.username.clone(), repo.repo_path.clone()),
        };
        sources.push(MirrorSource {
            device_id: source.id.clone(),
            device_name: source.name.clone(),
            storage: StorageInfo {
                known_hosts: app.known_hosts_lines(&repo.host, repo.port),
                host: repo.host.clone(),
                port: repo.port,
                username,
                repo_path,
            },
        });
    }
    Ok(Json(MirrorSources { sources }))
}

/// Kinds the website lets the admin pick.
pub const KINDS: [DeviceKind; 4] = [
    DeviceKind::Nixos,
    DeviceKind::Android,
    DeviceKind::Facade,
    DeviceKind::Other,
];
