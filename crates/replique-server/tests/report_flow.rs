//! The report flow end to end: a device is added on the website (sub-accounts
//! created through the Hetzner mock), pairs, declares a job, backs up a real
//! local restic repository and reports the run; the website, the live events,
//! measurements, alerts and Discord follow.
//!
//! The device here is an in-test HTTP client speaking the protocol with the
//! `replique-proto` types; the real `replique-client` joins in Phase 2.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use futures_util::StreamExt;
use replique_proto::{
    Ack, ApiError, BackupSummary, CheckSummary, DeviceConfig, DeviceKind, FinishRun, JobKind,
    JobRules, JobSpec, MirrorSources, MirrorSummary, PairRequest, PairResponse, Progress,
    Retention, RunKind, RunStatus, SafeToDelete, StartRun, StartRunReply, paths,
};
use reqwest::StatusCode;

/// The device side: base URL and token.
struct Device {
    http: reqwest::Client,
    url: String,
    token: String,
    id: String,
}

impl Device {
    async fn pair(
        url: &str,
        code: &str,
        kind: DeviceKind,
    ) -> Result<(Device, PairResponse), (StatusCode, ApiError)> {
        let http = reqwest::Client::new();
        let r = http
            .post(format!("{url}{}", paths::PAIR))
            .header("User-Agent", "replique-client/0.1.0")
            .json(&PairRequest {
                code: code.to_string(),
                device_name: "test".into(),
                kind,
                ssh_public_key: device_public_key(),
                client_version: Some("replique-client 0.1.0".into()),
            })
            .send()
            .await
            .unwrap();
        if !r.status().is_success() {
            let status = r.status();
            return Err((status, r.json().await.unwrap()));
        }
        let reply: PairResponse = r.json().await.unwrap();
        Ok((
            Device {
                http,
                url: url.to_string(),
                token: reply.report_token.clone(),
                id: reply.device_id.clone(),
            },
            reply,
        ))
    }

    async fn send<B: serde::Serialize>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &B,
    ) -> reqwest::Response {
        self.http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .unwrap()
    }

    async fn start(
        &self,
        job: &str,
        kind: RunKind,
        client_run_id: &str,
        started_at: replique_proto::Timestamp,
    ) -> String {
        let r = self
            .send(
                reqwest::Method::POST,
                paths::RUNS,
                &StartRun {
                    client_run_id: client_run_id.into(),
                    job: job.into(),
                    kind,
                    started_at,
                    restic_version: Some("0.19.1".into()),
                    client_version: Some("replique-client 0.1.0".into()),
                },
            )
            .await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json::<StartRunReply>().await.unwrap().run_id
    }

    async fn finish(&self, run_id: &str, finish: &FinishRun) -> StatusCode {
        self.send(reqwest::Method::POST, &paths::run_finish(run_id), finish)
            .await
            .status()
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.http
            .get(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap()
    }
}

fn finish(status: RunStatus, exit_code: i32) -> FinishRun {
    FinishRun {
        status,
        exit_code,
        finished_at: chrono::Utc::now(),
        summary: None,
        prune: None,
        check: None,
        mirror: None,
        errors: vec![],
        error_count: 0,
        unreadable_paths: vec![],
        message: None,
    }
}

fn uuid() -> String {
    replique_server::secrets::random_b64(16)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_added_paired_and_reporting() {
    let (mock, hetzner_url) = mock_hetzner().await;
    let secrets = tempfile::tempdir().unwrap();
    let token_file = secrets.path().join("hetzner-token");
    std::fs::write(&token_file, MOCK_TOKEN).unwrap();
    let server = start(&format!(
        "id = {MOCK_BOX}\napi_url = \"{hetzner_url}\"\nhetzner_token_file = \"{}\"",
        token_file.display()
    ))
    .await;
    let browser = Browser::login(&server.url).await;

    // Live events, as an open page would get them.
    let events = Arc::new(Mutex::new(String::new()));
    {
        let events = events.clone();
        let stream = browser
            .http
            .get(format!("{}/events", server.url))
            .send()
            .await
            .unwrap();
        assert_eq!(stream.status(), StatusCode::OK);
        tokio::spawn(async move {
            let mut body = stream.bytes_stream();
            while let Some(Ok(chunk)) = body.next().await {
                events
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&chunk));
            }
        });
    }

    // ---- adding the PC on the website creates its sub-account and twin
    let (status, location) = browser
        .post("/devices", &[("name", "PC"), ("kind", "nixos")])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{location}");
    let pc_id = location.trim_start_matches("/devices/").to_string();
    let subs = mock.subaccounts();
    assert_eq!(subs.len(), 2);
    // The twin's home is the repository folder, one level down: sharing the
    // device's home would share its authorized_keys.
    for s in &subs {
        let expected = if s["access_settings"]["readonly"] == true {
            "replique/sigma/pc/restic"
        } else {
            "replique/sigma/pc"
        };
        assert_eq!(s["home_directory"], expected);
    }
    assert_eq!(
        subs.iter()
            .filter(|s| s["access_settings"]["readonly"] == true)
            .count(),
        1
    );
    assert!(
        subs.iter()
            .all(|s| s["access_settings"]["ssh_enabled"] == true
                && s["access_settings"]["samba_enabled"] == false)
    );

    // ---- pairing
    let code = pairing_code(&browser, &pc_id).await;
    assert_eq!(code.len(), 9);
    let typed = code.to_lowercase().replace('-', " ");
    let (pc, reply) = Device::pair(&server.url, &typed, DeviceKind::Nixos)
        .await
        .unwrap();
    assert!(replique_proto::is_well_formed_token(&reply.report_token));
    assert_eq!(reply.device_id, pc_id);
    assert_eq!(reply.device_name, "PC");
    assert_eq!(reply.storage.host, "u1337.your-storagebox.de");
    assert_eq!(reply.storage.port, 23);
    assert_eq!(reply.storage.repo_path, "/home/restic");
    let device_sub = subs
        .iter()
        .find(|s| s["access_settings"]["readonly"] == false)
        .unwrap();
    assert_eq!(reply.storage.username, device_sub["username"]);
    // Each key landed in its own file (the local root stands in for the
    // box): the device's in its home, Réplique's in the twin's home, the
    // repository folder. Réplique's key must not open the writable login.
    let read = |rel: &str| std::fs::read_to_string(server.local_root().join(rel)).unwrap();
    let device_keys = read("replique/sigma/pc/.ssh/authorized_keys");
    let twin_keys = read("replique/sigma/pc/restic/.ssh/authorized_keys");
    let replique_key = &server.app.public_key_line()[..60];
    assert!(device_keys.contains("test-device"));
    assert!(!device_keys.contains(replique_key));
    assert!(twin_keys.contains(replique_key));
    assert!(!twin_keys.contains("test-device"));
    // Only the device's password was used, then replaced; the twin's was
    // random from the start and never kept.
    let device_id = device_sub["id"].clone();
    let resets: Vec<String> = mock
        .commands()
        .into_iter()
        .filter(|c| c.ends_with("reset_subaccount_password"))
        .collect();
    assert_eq!(resets.len(), 1);
    assert!(resets[0].contains(&format!("/subaccounts/{device_id}/")));
    // The code is spent.
    let (status, err) = Device::pair(&server.url, &code, DeviceKind::Nixos)
        .await
        .err()
        .unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err.error, "invalid_code");
    // The page now shows the paired device.
    let (_, page) = browser.get(&format!("/devices/{pc_id}")).await;
    assert!(!page.contains("Pair this device"));

    // ---- authentication
    let r = reqwest::Client::new()
        .get(format!("{}{}", server.url, paths::DEVICE_CONFIG))
        .bearer_auth("rpl_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(r.json::<ApiError>().await.unwrap().error, "unauthorized");

    // ---- the device's rules (ETag, 304)
    let r = pc.get(paths::DEVICE_CONFIG).await;
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert_eq!(
        r.json::<DeviceConfig>().await.unwrap(),
        DeviceConfig::android_default()
    );
    let r = pc
        .http
        .get(format!("{}{}", server.url, paths::DEVICE_CONFIG))
        .bearer_auth(&pc.token)
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);

    // ---- declaring a job
    let spec = JobSpec {
        kind: JobKind::Backup,
        schedule: Some("daily".into()),
        expected_interval_secs: Some(86_400),
        rules: JobRules {
            paths: vec!["/home/sigma".into()],
            excludes: vec!["/home/sigma/.cache".into(), "$RECYCLE.BIN".into()],
            exclude_caches: true,
            retention: Some(Retention::default()),
            check_subset: Some("5%".into()),
            ..JobRules::default()
        },
    };
    let r = pc
        .send(reqwest::Method::PUT, &paths::job("home"), &spec)
        .await;
    assert_eq!(r.json::<Ack>().await.unwrap(), Ack::OK);
    let r = pc
        .send(reqwest::Method::PUT, &paths::job("Bad Name"), &spec)
        .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    // ---- a real backup into the repository on the "box"
    let repo = server.local_root().join("replique/sigma/pc/restic");
    let password = "pc-restic-password";
    let data = server.dir.path().join("data");
    std::fs::create_dir_all(data.join("Documents")).unwrap();
    std::fs::write(data.join("Documents/notes.txt"), "bonjour").unwrap();
    // `cat config` exits 10 on a missing repository, then init: idempotent.
    let probe = restic(&repo, password, &["cat", "config"]);
    assert_eq!(probe.status.code(), Some(10));
    restic(&repo, password, &["init"]);

    let started = chrono::Utc::now();
    let client_run_id = uuid();
    let run_id = pc
        .start("home", RunKind::Backup, &client_run_id, started)
        .await;
    // A start replayed from the spool is the same run.
    assert_eq!(
        pc.start("home", RunKind::Backup, &client_run_id, started)
            .await,
        run_id
    );

    let progress = Progress {
        at: chrono::Utc::now(),
        percent_done: 0.42,
        seconds_elapsed: 10,
        seconds_remaining: Some(14),
        total_files: 2,
        files_done: 1,
        total_bytes: 1000,
        bytes_done: 420,
        current_files: vec!["/home/sigma/Documents/notes.txt".into()],
        ..Progress::default()
    };
    let r = pc
        .send(
            reqwest::Method::POST,
            &paths::run_progress(&run_id),
            &progress,
        )
        .await;
    assert_eq!(r.status(), StatusCode::OK);
    let (_, overview) = browser.get("/fragments/overview").await;
    assert!(
        overview.contains("Running") && overview.contains("ETA 14 s"),
        "{overview}"
    );

    let out = restic(
        &repo,
        password,
        &["backup", "--json", data.to_str().unwrap()],
    );
    let summary_line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains("\"message_type\":\"summary\""))
        .expect("restic printed a summary")
        .to_string();
    let summary: BackupSummary = serde_json::from_str(&summary_line).unwrap();
    let snapshot_id = summary.snapshot_id.clone().unwrap();
    let report = FinishRun {
        summary: Some(summary),
        ..finish(RunStatus::from_restic_exit(0), 0)
    };
    assert_eq!(pc.finish(&run_id, &report).await, StatusCode::OK);
    // A replay is accepted; a different ending is a conflict.
    assert_eq!(pc.finish(&run_id, &report).await, StatusCode::OK);
    let mut other = report.clone();
    other.exit_code = 3;
    other.status = RunStatus::Partial;
    assert_eq!(pc.finish(&run_id, &other).await, StatusCode::CONFLICT);

    // A run that never reports (the phone's app updated mid-backup) is closed
    // as interrupted when the next run of its job starts, not called lost
    // hours later; its real report, if it turns up, still replaces that.
    let silent = pc
        .start(
            "home",
            RunKind::Backup,
            &uuid(),
            chrono::Utc::now() - chrono::Duration::minutes(10),
        )
        .await;
    let next = pc
        .start("home", RunKind::Backup, &uuid(), chrono::Utc::now())
        .await;
    let closed = server.app.store.run(&silent).unwrap().unwrap();
    assert_eq!(closed.status, RunStatus::Interrupted);
    assert_eq!(
        server.app.store.run(&next).unwrap().unwrap().status,
        RunStatus::Running
    );
    let late = finish(RunStatus::Interrupted, 130);
    assert_eq!(pc.finish(&silent, &late).await, StatusCode::OK);
    assert_eq!(
        pc.finish(&next, &finish(RunStatus::Success, 0)).await,
        StatusCode::OK
    );
    // A check that could not lock the repository is a failed run, not an
    // integrity problem, even with restic 0.19's `num_errors: 1` on the way out.
    let locked = pc
        .start("home", RunKind::Check, &uuid(), chrono::Utc::now())
        .await;
    let mut report_locked = finish(RunStatus::LockFailed, 11);
    report_locked.check = Some(CheckSummary {
        read_data_subset: Some("5%".into()),
        num_errors: 1,
        snapshot_ids: vec![],
    });
    assert_eq!(pc.finish(&locked, &report_locked).await, StatusCode::OK);
    let open: Vec<String> = server
        .app
        .store
        .open_alerts()
        .unwrap()
        .into_iter()
        .map(|a| a.title)
        .collect();
    assert!(
        open.iter()
            .any(|t| t.contains("check failed (repository locked)")),
        "{open:?}"
    );
    assert!(!open.iter().any(|t| t.contains("integrity")), "{open:?}");

    // A clean check afterwards clears it.
    let clean = pc
        .start("home", RunKind::Check, &uuid(), chrono::Utc::now())
        .await;
    assert_eq!(
        pc.finish(&clean, &finish(RunStatus::Success, 0)).await,
        StatusCode::OK
    );

    let (_, overview) = browser.get("/fragments/overview").await;
    assert!(
        overview.contains("home") && overview.contains("Success"),
        "{overview}"
    );
    let (_, page) = browser.get(&format!("/devices/{pc_id}")).await;
    assert!(page.contains("/home/sigma/.cache") && page.contains("keep-deleted"));
    assert!(page.contains(&snapshot_id[..8]));
    assert!(eventually(|| events.lock().unwrap().contains("event: runs")).await);
    assert!(events.lock().unwrap().contains("event: devices"));

    // ---- a second device: the phone
    let (_, location) = browser
        .post("/devices", &[("name", "Téléphone"), ("kind", "android")])
        .await;
    let phone_id = location.trim_start_matches("/devices/").to_string();
    let code = pairing_code(&browser, &phone_id).await;
    let (phone, reply) = Device::pair(&server.url, &code, DeviceKind::Android)
        .await
        .unwrap();
    assert_eq!(reply.device_name, "Téléphone");
    let mut homes: Vec<String> = mock
        .subaccounts()
        .iter()
        .filter_map(|s| s["home_directory"].as_str().map(String::from))
        .filter(|h| h.starts_with("replique/sigma/telephone"))
        .collect();
    homes.sort();
    assert_eq!(
        homes,
        [
            "replique/sigma/telephone",
            "replique/sigma/telephone/restic"
        ]
    );

    // One device cannot report for another.
    assert_eq!(phone.finish(&run_id, &report).await, StatusCode::FORBIDDEN);
    let r = pc.get(&paths::safe_to_delete(&phone.id)).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // ---- safe to delete: verified on the box and present in the PC's mirror
    let phone_snapshot = "a3c1".repeat(16);
    let backup = phone
        .start(
            "phone",
            RunKind::Backup,
            &uuid(),
            chrono::Utc::now() - chrono::Duration::minutes(30),
        )
        .await;
    let mut done = finish(RunStatus::Success, 0);
    done.summary = Some(BackupSummary {
        snapshot_id: Some(phone_snapshot.clone()),
        backup_start: Some(chrono::Utc::now() - chrono::Duration::minutes(30)),
        ..BackupSummary::default()
    });
    assert_eq!(phone.finish(&backup, &done).await, StatusCode::OK);
    let none: SafeToDelete = phone
        .get(&paths::safe_to_delete(&phone.id))
        .await
        .json()
        .await
        .unwrap();
    assert!(none.snapshot.is_none());
    let check = phone
        .start("phone", RunKind::Check, &uuid(), chrono::Utc::now())
        .await;
    let mut checked = finish(RunStatus::Success, 0);
    checked.check = Some(CheckSummary {
        read_data_subset: Some("5%".into()),
        num_errors: 0,
        snapshot_ids: vec![phone_snapshot.clone()],
    });
    assert_eq!(phone.finish(&check, &checked).await, StatusCode::OK);

    // The admin lets the PC mirror the phone; the PC sees the twin's login.
    let (status, location) = browser
        .post(
            &format!("/devices/{pc_id}/mirrors"),
            &[("source", &phone_id)],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.contains("ok="), "{location}");
    let sources: MirrorSources = pc.get(paths::MIRROR_SOURCES).await.json().await.unwrap();
    assert_eq!(sources.sources.len(), 1);
    assert_eq!(sources.sources[0].device_id, phone.id);
    let twin = mock
        .subaccounts()
        .into_iter()
        .find(|s| {
            s["home_directory"] == "replique/sigma/telephone/restic"
                && s["access_settings"]["readonly"] == true
        })
        .unwrap();
    assert_eq!(sources.sources[0].storage.username, twin["username"]);
    // The twin's home is the repository folder.
    assert_eq!(sources.sources[0].storage.repo_path, "/home");
    // The PC's key went into the twin's own authorized_keys, next to
    // Réplique's, and not into the phone's: it must not open the writable login.
    let read = |rel: &str| std::fs::read_to_string(server.local_root().join(rel)).unwrap();
    let devices = |keys: &str| keys.lines().filter(|l| l.contains("test-device")).count();
    let twin_keys = read("replique/sigma/telephone/restic/.ssh/authorized_keys");
    assert_eq!(devices(&twin_keys), 1, "the PC's key");
    assert!(twin_keys.contains(&server.app.public_key_line()[..60]));
    let phone_keys = read("replique/sigma/telephone/.ssh/authorized_keys");
    assert_eq!(devices(&phone_keys), 1, "only the phone's own key");
    // Stopping the mirror takes the PC's key off the twin, Réplique's stays;
    // allowing it again puts it back.
    let (status, location) = browser
        .post(&format!("/devices/{pc_id}/mirrors/{phone_id}/remove"), &[])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.contains("ok="), "{location}");
    let twin_keys = read("replique/sigma/telephone/restic/.ssh/authorized_keys");
    assert_eq!(devices(&twin_keys), 0, "the PC's key is gone");
    assert!(twin_keys.contains(&server.app.public_key_line()[..60]));
    let none: MirrorSources = pc.get(paths::MIRROR_SOURCES).await.json().await.unwrap();
    assert!(none.sources.is_empty());
    let (status, _) = browser
        .post(
            &format!("/devices/{pc_id}/mirrors"),
            &[("source", &phone_id)],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let twin_keys = read("replique/sigma/telephone/restic/.ssh/authorized_keys");
    assert_eq!(devices(&twin_keys), 1, "the PC's key is back");

    let mirror = pc
        .start("phone-mirror", RunKind::Mirror, &uuid(), chrono::Utc::now())
        .await;
    let mut mirrored = finish(RunStatus::Success, 0);
    mirrored.mirror = Some(MirrorSummary {
        source_device_id: phone.id.clone(),
        files_copied: 10,
        bytes_copied: 1000,
        files_removed: 0,
        files_verified: 10,
        hash_mismatches: vec![],
        snapshot_ids: vec![phone_snapshot.clone()],
        total_bytes: Some(1000),
    });
    assert_eq!(pc.finish(&mirror, &mirrored).await, StatusCode::OK);
    let safe: SafeToDelete = phone
        .get(&paths::safe_to_delete(&phone.id))
        .await
        .json()
        .await
        .unwrap();
    let safe = safe.snapshot.expect("a safe snapshot");
    assert_eq!(safe.snapshot_id, phone_snapshot);
    assert_eq!(safe.mirrored_by.as_deref(), Some("PC"));
    let since = (chrono::Utc::now() + chrono::Duration::minutes(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let later: SafeToDelete = phone
        .get(&format!(
            "{}?since={since}",
            paths::safe_to_delete(&phone.id)
        ))
        .await
        .json()
        .await
        .unwrap();
    assert!(later.snapshot.is_none());

    // ---- measurements and drift
    replique_server::measure::measure_all(&server.app).await;
    let box_point = server.app.store.latest_measurement("box").unwrap();
    assert!(box_point.is_some(), "the box total from the API");
    let pc_repo = server.app.store.repository_of(&pc_id).unwrap().unwrap();
    let first = server
        .app
        .store
        .latest_measurement(&format!("repo:{}", pc_repo.id))
        .unwrap()
        .unwrap();
    assert!(first.bytes > 0 && first.files.unwrap() > 3);
    // Something writes to the repository with no run going on.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    std::fs::write(repo.join("data/intruder"), "x").unwrap();
    replique_server::measure::measure_all(&server.app).await;
    let drift = server
        .app
        .store
        .open_alert(&format!("drift:{}", pc_repo.id))
        .unwrap();
    assert!(drift.is_some(), "a drift warning");
    let (_, page) = browser.get("/storage/series?scope=repos&range=24h").await;
    assert!(
        page.contains("\"stacked\":true") && page.contains("PC"),
        "{page}"
    );

    // ---- alerts reach Discord once, and recover
    let (posts, webhook) = mock_discord().await;
    let (status, location) = browser
        .post("/settings/discord", &[("webhook", &webhook)])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{location}");
    let failed_run = pc
        .start("home", RunKind::Backup, &uuid(), chrono::Utc::now())
        .await;
    let mut failed = finish(RunStatus::Failed, 1);
    failed.message = Some("/mnt/windows/c is not a mountpoint".into());
    assert_eq!(pc.finish(&failed_run, &failed).await, StatusCode::OK);
    assert!(
        eventually(|| !posts.lock().unwrap().is_empty()).await,
        "Discord got the alert"
    );
    {
        let posts = posts.lock().unwrap();
        let msg = &posts[0];
        assert!(msg["content"].as_str().unwrap().starts_with("@everyone"));
        assert!(
            msg["content"]
                .as_str()
                .unwrap()
                .contains("PC · home: backup failed")
        );
        assert_eq!(msg["allowed_mentions"]["parse"][0], "everyone");
    }
    // Failing again does not ping again.
    let again = pc
        .start("home", RunKind::Backup, &uuid(), chrono::Utc::now())
        .await;
    assert_eq!(pc.finish(&again, &failed).await, StatusCode::OK);
    let (_, problems) = browser.get("/problems").await;
    assert!(problems.contains("backup failed") && problems.contains("not a mountpoint"));
    let ok_run = pc
        .start("home", RunKind::Backup, &uuid(), chrono::Utc::now())
        .await;
    assert_eq!(
        pc.finish(&ok_run, &finish(RunStatus::Success, 0)).await,
        StatusCode::OK
    );
    assert!(eventually(|| posts.lock().unwrap().len() >= 2).await);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    {
        let posts = posts.lock().unwrap();
        assert_eq!(posts.len(), 2, "one alert, one recovery: {posts:?}");
        assert!(
            posts[1]["content"]
                .as_str()
                .unwrap()
                .starts_with("Resolved: PC · home")
        );
        assert!(
            posts[1]["allowed_mentions"]["parse"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    // ---- a partial run lists the unreadable paths
    let partial_run = pc
        .start("home", RunKind::Backup, &uuid(), chrono::Utc::now())
        .await;
    let mut partial = finish(RunStatus::Partial, 3);
    partial.unreadable_paths = vec!["/home/sigma/locked.db".into()];
    partial.error_count = 1;
    assert_eq!(pc.finish(&partial_run, &partial).await, StatusCode::OK);
    let (_, problems) = browser.get("/problems").await;
    assert!(problems.contains("/home/sigma/locked.db"), "{problems}");

    // ---- a silent job: the last backup started 30 hours ago
    let (_, location) = browser
        .post("/devices", &[("name", "Laptop"), ("kind", "nixos")])
        .await;
    let laptop_id = location.trim_start_matches("/devices/").to_string();
    let code = pairing_code(&browser, &laptop_id).await;
    let (laptop, _) = Device::pair(&server.url, &code, DeviceKind::Nixos)
        .await
        .unwrap();
    let daily = JobSpec {
        expected_interval_secs: None,
        ..spec.clone()
    };
    laptop
        .send(reqwest::Method::PUT, &paths::job("home"), &daily)
        .await;
    let old = laptop
        .start(
            "home",
            RunKind::Backup,
            &uuid(),
            chrono::Utc::now() - chrono::Duration::hours(30),
        )
        .await;
    laptop.finish(&old, &finish(RunStatus::Success, 0)).await;
    replique_server::alerts::evaluate(&server.app)
        .await
        .unwrap();
    let open = server.app.store.open_alerts().unwrap();
    assert!(
        open.iter()
            .any(|a| a.kind == "silent" && a.title.starts_with("Laptop · home")),
        "{open:?}"
    );
    laptop
        .start("home", RunKind::Backup, &uuid(), chrono::Utc::now())
        .await;
    let open = server.app.store.open_alerts().unwrap();
    assert!(!open.iter().any(|a| a.kind == "silent"));

    // ---- revoking and removing
    let (status, _) = browser
        .post(&format!("/devices/{laptop_id}/revoke"), &[])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        laptop.get(paths::DEVICE_CONFIG).await.status(),
        StatusCode::UNAUTHORIZED
    );
    // Other devices are untouched.
    assert_eq!(pc.get(paths::DEVICE_CONFIG).await.status(), StatusCode::OK);

    let before = mock.subaccounts().len();
    let (status, location) = browser
        .post(&format!("/devices/{phone_id}/remove"), &[("data", "keep")])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{location}");
    assert_eq!(mock.subaccounts().len(), before - 2);
    assert_eq!(
        phone.get(paths::DEVICE_CONFIG).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(
        server
            .local_root()
            .join("replique/sigma/telephone")
            .exists(),
        "data kept"
    );
    // A device added later under the same name gets a home of its own: the
    // kept folder may hold the old device's backups and keys.
    let (status, location) = browser
        .post("/devices", &[("name", "Téléphone"), ("kind", "android")])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{location}");
    assert!(
        mock.subaccounts()
            .iter()
            .any(|s| s["home_directory"] == "replique/sigma/telephone-2"),
        "{:?}",
        mock.subaccounts()
    );

    // Forms without the CSRF token are refused.
    let r = browser
        .http
        .post(format!("{}/devices/{pc_id}/revoke", server.url))
        .form(&[("x", "y")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(pc.get(paths::DEVICE_CONFIG).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn pages_need_a_login() {
    let server = start("").await;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for path in [
        "/",
        "/storage",
        "/settings",
        "/problems",
        "/events",
        "/devices/new",
    ] {
        let r = http
            .get(format!("{}{path}", server.url))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER, "{path}");
        assert!(
            r.headers()["location"]
                .to_str()
                .unwrap()
                .starts_with("/login")
        );
    }
    // A wrong password does not log in.
    let page = http
        .get(format!("{}/login", server.url))
        .send()
        .await
        .unwrap();
    let cookie = page.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let token = extract(&page.text().await.unwrap(), "name=\"token\" value=\"", "\"").unwrap();
    let r = http
        .post(format!("{}/login", server.url))
        .header("cookie", cookie)
        .form(&[("password", "nope"), ("token", &token)])
        .send()
        .await
        .unwrap();
    assert!(
        r.headers()["location"]
            .to_str()
            .unwrap()
            .contains("failed=1")
    );
    assert!(r.headers().get("set-cookie").is_none());
    // Phone width: the viewport is declared and assets are served locally.
    let browser = Browser::login(&server.url).await;
    let (_, home) = browser.get("/").await;
    assert!(home.contains("width=device-width"));
    assert!(!home.contains("https://unpkg") && !home.contains("cdn"));
    for asset in ["htmx.min.js", "uPlot.min.js", "app.js", "style.css"] {
        let (status, _) = browser.get(&format!("/assets/{asset}")).await;
        assert_eq!(status, StatusCode::OK, "{asset}");
    }
}

#[tokio::test]
async fn simulated_box_without_hetzner() {
    // Development setup: a local root and no token pairs without any API.
    let server = start("").await;
    let browser = Browser::login(&server.url).await;
    let (status, location) = browser
        .post("/devices", &[("name", "Dev box"), ("kind", "other")])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{location}");
    let id = location.trim_start_matches("/devices/").to_string();
    let code = pairing_code(&browser, &id).await;
    let (_, reply) = Device::pair(&server.url, &code, DeviceKind::Other)
        .await
        .unwrap();
    assert_eq!(reply.storage.username, "local-dev-box");
    // A paired device can get a new code (after a reinstall).
    let (status, location) = browser
        .post(&format!("/devices/{id}/pairing-code"), &[])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{location}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_keys_adoption_and_a_full_box() {
    // No local root: nothing but the Hetzner mock; keys are installed by hand.
    let (mock, hetzner_url) = mock_hetzner().await;
    let secrets = tempfile::tempdir().unwrap();
    let token_file = secrets.path().join("hetzner-token");
    std::fs::write(&token_file, MOCK_TOKEN).unwrap();
    let server = start_with(
        &format!(
            "id = {MOCK_BOX}\napi_url = \"{hetzner_url}\"\nhetzner_token_file = \"{}\"\nkey_install = \"manual\"",
            token_file.display()
        ),
        false,
    )
    .await;
    let browser = Browser::login(&server.url).await;
    let (hetzner, _) = server.app.hetzner().unwrap();

    // The PC's existing sub-account, adopted rather than created.
    let existing = hetzner
        .create_subaccount(
            MOCK_BOX,
            replique_server::hetzner::NewSubaccount {
                home_directory: "backups/pc",
                password: &replique_server::secrets::Secret::new("Existing-pass-123".into()),
                access_settings: replique_server::hetzner::AccessSettings::ssh_only(false),
                description: "the PC's",
            },
        )
        .await
        .unwrap();
    let (status, location) = browser
        .post(
            "/devices",
            &[
                ("name", "PC"),
                ("kind", "nixos"),
                ("adopt", &existing.username),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{location}");
    let id = location.trim_start_matches("/devices/").to_string();
    // Only the twin was created, on the adopted home.
    let subs = mock.subaccounts();
    assert_eq!(subs.len(), 2);
    let twin = subs
        .iter()
        .find(|s| s["access_settings"]["readonly"] == true)
        .unwrap()
        .clone();
    assert_eq!(twin["home_directory"], "backups/pc/restic");
    let twin_user = twin["username"].as_str().unwrap().to_string();

    let code = pairing_code(&browser, &id).await;
    let (_, reply) = Device::pair(&server.url, &code, DeviceKind::Nixos)
        .await
        .unwrap();
    assert_eq!(reply.storage.username, existing.username);
    // Manual mode: the page shows one command, writing Réplique's key into
    // the twin's home (the repository folder) through the adopted sub-account,
    // which already has the device's key and gets no install-ssh-key step.
    let (_, page) = browser.get(&format!("/devices/{id}")).await;
    assert!(page.contains("Install the keys by hand"));
    assert!(page.contains("restic/.ssh/authorized_keys"));
    assert!(page.contains(&format!(
        "sftp -P 23 {}@u1337.your-storagebox.de",
        existing.username
    )));
    assert!(!page.contains("install-ssh-key"));
    assert!(!page.contains(&format!("{twin_user}@")));
    let resets = |m: &MockHetzner| {
        m.commands()
            .into_iter()
            .filter(|c| c.ends_with("reset_subaccount_password"))
            .collect::<Vec<_>>()
    };
    // The adopted sub-account got a password for that one command; nothing
    // is rotated before the admin confirms.
    let set = resets(&mock);
    assert_eq!(set.len(), 1);
    assert!(set[0].contains(&format!("/subaccounts/{}/", existing.id)));
    let (status, location) = browser
        .post(&format!("/devices/{id}/manual-done"), &[])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.contains("ok="), "{location}");
    // Then that password is replaced by one nobody keeps; the twin's is
    // never touched.
    let rotated = resets(&mock);
    assert_eq!(rotated.len(), 2);
    assert!(rotated[1].contains(&format!("/subaccounts/{}/", existing.id)));
    assert!(
        !rotated
            .iter()
            .any(|r| r.contains(&format!("/subaccounts/{}/", twin["id"])))
    );
    let (_, page) = browser.get(&format!("/devices/{id}")).await;
    assert!(!page.contains("Install the keys by hand"));

    // The box at 85%: a warning, then an alert past 95%, then back to normal.
    *mock.used_bytes.lock().unwrap() = 850_000_000_000;
    replique_server::measure::refresh_box(&server.app)
        .await
        .unwrap();
    replique_server::alerts::evaluate(&server.app)
        .await
        .unwrap();
    assert!(server.app.store.open_alert("box_warn").unwrap().is_some());
    let (_, settings) = browser.get("/settings").await;
    assert!(
        settings.contains("198 left"),
        "sub-accounts left on Settings"
    );
    *mock.used_bytes.lock().unwrap() = 960_000_000_000;
    replique_server::measure::refresh_box(&server.app)
        .await
        .unwrap();
    replique_server::alerts::evaluate(&server.app)
        .await
        .unwrap();
    assert!(server.app.store.open_alert("box_warn").unwrap().is_none());
    let alert = server.app.store.open_alert("box_alert").unwrap().unwrap();
    assert_eq!(alert.title, "Storage Box 96% full");
    *mock.used_bytes.lock().unwrap() = 100_000_000_000;
    replique_server::measure::refresh_box(&server.app)
        .await
        .unwrap();
    replique_server::alerts::evaluate(&server.app)
        .await
        .unwrap();
    assert!(
        server
            .app
            .store
            .open_alerts()
            .unwrap()
            .iter()
            .all(|a| a.kind != "box_usage")
    );
}
