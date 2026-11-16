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
        // One status per device: a run in progress, with its ETA.
        overview.contains("Backing up") && overview.contains("ETA 14 s"),
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
        // One status per device, not its jobs; the row opens the device.
        overview.contains("Up to date")
            && overview.contains("last backup")
            && overview.contains(&format!("data-href=\"/devices/{pc_id}\"")),
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
        source_mismatches: vec![],
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

    // ---- a file broken on the phone's side (an upload cut short): the mirror
    // succeeds and reports it; that copy counts for "free up space" only once
    // the phone's own check passed after the file appeared.
    let broken_pack = format!("data/ab/{}", "ab".repeat(32));
    let newer = "b4d2".repeat(16);
    // An hour ahead, so every run reported earlier in this test (the phone's
    // first check included) comes strictly before this scenario.
    let at = chrono::Utc::now() + chrono::Duration::hours(1);
    let minutes = chrono::Duration::minutes;
    let b2 = phone
        .start("phone", RunKind::Backup, &uuid(), at - minutes(20))
        .await;
    // The phone reports what it really sends: that rate is the run's peak.
    let sending = Progress {
        at: at - minutes(19),
        percent_done: 0.5,
        total_bytes: 900_000_000,
        bytes_done: 450_000_000,
        bytes_uploaded: Some(270_000_000),
        upload_bytes_per_sec: Some(9_000_000),
        ..Progress::default()
    };
    let r = phone
        .send(reqwest::Method::POST, &paths::run_progress(&b2), &sending)
        .await;
    assert_eq!(r.status(), StatusCode::OK);
    let mut done2 = finish(RunStatus::Success, 0);
    done2.finished_at = at - minutes(19);
    done2.summary = Some(BackupSummary {
        snapshot_id: Some(newer.clone()),
        backup_start: Some(at - minutes(20)),
        total_duration: 60.0,
        data_added_packed: 260_000_000,
        ..BackupSummary::default()
    });
    assert_eq!(phone.finish(&b2, &done2).await, StatusCode::OK);
    let recorded = server.app.store.run(&b2).unwrap().unwrap();
    assert_eq!(recorded.peak_bps, Some(9_000_000));
    assert_eq!(recorded.sent_bytes, Some(270_000_000));
    // Its average: what it sent over its duration (270 MB in 60 s).
    let (_, phone_page) = browser.get(&format!("/devices/{phone_id}")).await;
    assert!(phone_page.contains("4.5 MB/s"), "the run's average");
    assert!(
        phone_page.contains("Transfer, last 30 days"),
        "the job's figures"
    );
    assert!(phone_page.contains("9.0 MB/s"), "the job's peak");
    // Every relative time shows the exact time when hovered.
    assert!(
        phone_page.contains("<time datetime=\""),
        "relative times carry their exact time"
    );
    assert!(
        !phone_page.contains(" ago</td>"),
        "no bare relative time in the runs table"
    );
    let check_of = |ids: Vec<String>, at| {
        let mut f = finish(RunStatus::Success, 0);
        f.finished_at = at;
        f.check = Some(CheckSummary {
            read_data_subset: None,
            num_errors: 0,
            snapshot_ids: ids,
        });
        f
    };
    let c2 = phone
        .start("phone", RunKind::Check, &uuid(), at - minutes(15))
        .await;
    let both = vec![phone_snapshot.clone(), newer.clone()];
    assert_eq!(
        phone
            .finish(&c2, &check_of(both.clone(), at - minutes(14)))
            .await,
        StatusCode::OK
    );
    let mirror_of = |broken: Vec<String>, copies: Vec<String>, status, code, at| {
        let mut f = finish(status, code);
        f.finished_at = at;
        f.mirror = Some(MirrorSummary {
            source_device_id: phone.id.clone(),
            files_copied: 3,
            bytes_copied: 300,
            files_removed: 0,
            files_verified: 3,
            hash_mismatches: copies,
            source_mismatches: broken,
            snapshot_ids: vec![phone_snapshot.clone(), newer.clone()],
            total_bytes: Some(1300),
        });
        f
    };
    let m2 = pc
        .start("phone-mirror", RunKind::Mirror, &uuid(), at - minutes(10))
        .await;
    let report = mirror_of(
        vec![broken_pack.clone()],
        vec![],
        RunStatus::Success,
        0,
        at - minutes(9),
    );
    assert_eq!(pc.finish(&m2, &report).await, StatusCode::OK);
    let safe_now = || async {
        phone
            .get(&paths::safe_to_delete(&phone.id))
            .await
            .json::<SafeToDelete>()
            .await
            .unwrap()
            .snapshot
            .map(|s| s.snapshot_id)
    };
    assert_eq!(
        safe_now().await.as_deref(),
        Some(phone_snapshot.as_str()),
        "the newer snapshot waits for a check after the broken file"
    );
    let titles = || {
        server
            .app
            .store
            .open_alerts()
            .unwrap()
            .into_iter()
            .map(|a| a.title)
            .collect::<Vec<_>>()
    };
    let open = titles();
    assert!(
        open.iter()
            .any(|t| t.contains("broken on Téléphone's side")),
        "{open:?}"
    );
    assert!(
        !open
            .iter()
            .any(|t| t.contains("integrity") || t.contains("not checked since")),
        "{open:?}"
    );
    let (_, pc_page) = browser.get(&format!("/devices/{pc_id}")).await;
    assert!(
        pc_page.contains("broken on Téléphone"),
        "the mirror's card explains it"
    );
    assert!(pc_page.contains("Not settled yet"));

    // The phone's next check passes: nothing uses the broken file, the copy counts.
    let c3 = phone
        .start("phone", RunKind::Check, &uuid(), at - minutes(2))
        .await;
    assert_eq!(
        phone
            .finish(&c3, &check_of(both.clone(), at - minutes(1)))
            .await,
        StatusCode::OK
    );
    assert_eq!(safe_now().await.as_deref(), Some(newer.as_str()));
    let open = titles();
    assert!(
        open.iter()
            .any(|t| t.contains("broken on Téléphone's side")),
        "{open:?}"
    );
    assert!(
        !open.iter().any(|t| t.contains("not checked since")),
        "{open:?}"
    );
    let (_, pc_page) = browser.get(&format!("/devices/{pc_id}")).await;
    assert!(pc_page.contains("Harmless"));

    // A clean mirror clears the warning.
    let m3 = pc
        .start("phone-mirror", RunKind::Mirror, &uuid(), at - minutes(1))
        .await;
    assert_eq!(
        pc.finish(&m3, &mirror_of(vec![], vec![], RunStatus::Success, 0, at))
            .await,
        StatusCode::OK
    );
    assert!(
        !titles().iter().any(|t| t.contains("broken on")),
        "{:?}",
        titles()
    );

    // A broken copy is still an integrity problem, as before.
    let m4 = pc.start("phone-mirror", RunKind::Mirror, &uuid(), at).await;
    let copy_broken = mirror_of(vec![], vec![broken_pack.clone()], RunStatus::Failed, 1, at);
    assert_eq!(pc.finish(&m4, &copy_broken).await, StatusCode::OK);
    assert!(
        titles()
            .iter()
            .any(|t| t.contains("integrity check failed")),
        "{:?}",
        titles()
    );
    let m5 = pc.start("phone-mirror", RunKind::Mirror, &uuid(), at).await;
    assert_eq!(
        pc.finish(&m5, &mirror_of(vec![], vec![], RunStatus::Success, 0, at))
            .await,
        StatusCode::OK
    );
    assert!(
        !titles().iter().any(|t| t.contains("integrity")),
        "{:?}",
        titles()
    );

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

/// Files broken on the source with no passing check of it since they appeared:
/// once the source has had time for one, it is an integrity alert, and the
/// mirror does not count for "free up space".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_breakage_unchecked_escalates() {
    let (_mock, hetzner_url) = mock_hetzner().await;
    let secrets = tempfile::tempdir().unwrap();
    let token_file = secrets.path().join("hetzner-token");
    std::fs::write(&token_file, MOCK_TOKEN).unwrap();
    let server = start(&format!(
        "id = {MOCK_BOX}\napi_url = \"{hetzner_url}\"\nhetzner_token_file = \"{}\"",
        token_file.display()
    ))
    .await;
    let browser = Browser::login(&server.url).await;
    let add = |name: &'static str, kind: &'static str| {
        let browser = &browser;
        async move {
            let (_, location) = browser
                .post("/devices", &[("name", name), ("kind", kind)])
                .await;
            location.trim_start_matches("/devices/").to_string()
        }
    };
    let phone_id = add("Phone", "android").await;
    let code = pairing_code(&browser, &phone_id).await;
    let (phone, _) = Device::pair(&server.url, &code, DeviceKind::Android)
        .await
        .unwrap();
    let pc_id = add("PC", "nixos").await;
    let code = pairing_code(&browser, &pc_id).await;
    let (pc, _) = Device::pair(&server.url, &code, DeviceKind::Nixos)
        .await
        .unwrap();
    let (status, _) = browser
        .post(
            &format!("/devices/{pc_id}/mirrors"),
            &[("source", &phone_id)],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Five days ago: a backup and a passing check. Nothing since.
    let now = chrono::Utc::now();
    let days = chrono::Duration::days;
    let snapshot = "c0de".repeat(16);
    let b = phone
        .start("phone", RunKind::Backup, &uuid(), now - days(5))
        .await;
    let mut done = finish(RunStatus::Success, 0);
    done.finished_at = now - days(5);
    done.summary = Some(BackupSummary {
        snapshot_id: Some(snapshot.clone()),
        backup_start: Some(now - days(5)),
        ..BackupSummary::default()
    });
    assert_eq!(phone.finish(&b, &done).await, StatusCode::OK);
    let c = phone
        .start("phone", RunKind::Check, &uuid(), now - days(5))
        .await;
    let mut checked = finish(RunStatus::Success, 0);
    checked.finished_at = now - days(5);
    checked.check = Some(CheckSummary {
        read_data_subset: None,
        num_errors: 0,
        snapshot_ids: vec![snapshot.clone()],
    });
    assert_eq!(phone.finish(&c, &checked).await, StatusCode::OK);

    // An older client's report, without the new field, is still read.
    let m0 = pc
        .start("phone-mirror", RunKind::Mirror, &uuid(), now - days(4))
        .await;
    let old_style = serde_json::json!({
        "status": "success", "exit_code": 0,
        "finished_at": (now - days(4)).to_rfc3339(),
        "mirror": { "source_device_id": phone.id, "hash_mismatches": [], "snapshot_ids": [snapshot] },
        "errors": [], "error_count": 0, "unreadable_paths": []
    });
    let r = pc
        .send(reqwest::Method::POST, &paths::run_finish(&m0), &old_style)
        .await;
    assert_eq!(r.status(), StatusCode::OK);

    // Three days ago and now: one file broken on the phone's side.
    let broken = vec![format!("data/cd/{}", "cd".repeat(32))];
    for (i, when) in [now - days(3), now].into_iter().enumerate() {
        let m = pc
            .start("phone-mirror", RunKind::Mirror, &format!("m{i}"), when)
            .await;
        let mut f = finish(RunStatus::Success, 0);
        f.finished_at = when;
        f.mirror = Some(MirrorSummary {
            source_device_id: phone.id.clone(),
            files_copied: 0,
            bytes_copied: 0,
            files_removed: 0,
            files_verified: 0,
            hash_mismatches: vec![],
            source_mismatches: broken.clone(),
            snapshot_ids: vec![snapshot.clone()],
            total_bytes: None,
        });
        assert_eq!(pc.finish(&m, &f).await, StatusCode::OK);
    }
    replique_server::alerts::evaluate(&server.app)
        .await
        .unwrap();
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
            .any(|t| t.contains("not checked since files broke there")),
        "{open:?}"
    );
    // The old-style run, before the breakage, still counts: the snapshot is
    // safe through it. Without it, nothing would be.
    let safe: SafeToDelete = phone
        .get(&paths::safe_to_delete(&phone.id))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        safe.snapshot.map(|s| s.mirrored_at.timestamp()),
        Some((now - days(4)).timestamp())
    );

    // On the overview: a backup failing after the last good one is an error
    // for the device until a backup succeeds again.
    let failed_at = now + chrono::Duration::minutes(2);
    let b2 = phone
        .start("phone", RunKind::Backup, &uuid(), failed_at)
        .await;
    let mut failed = finish(RunStatus::Failed, 1);
    failed.finished_at = failed_at;
    failed.message = Some("network unreachable".into());
    assert_eq!(phone.finish(&b2, &failed).await, StatusCode::OK);
    let (_, overview) = browser.get("/fragments/overview").await;
    assert!(
        overview.contains(">Error<") && overview.contains("network unreachable"),
        "{overview}"
    );
    let later = failed_at + chrono::Duration::minutes(5);
    let b3 = phone.start("phone", RunKind::Backup, &uuid(), later).await;
    let mut ok = finish(RunStatus::Success, 0);
    ok.finished_at = later + chrono::Duration::minutes(1);
    assert_eq!(phone.finish(&b3, &ok).await, StatusCode::OK);
    let (_, overview) = browser.get("/fragments/overview").await;
    assert!(!overview.contains(">Error<"), "{overview}");
}

/// The Bitwarden copy (a file sent as it is, encrypted already) and the
/// recovery kit: plan, checklist, reminders, print view, Storage Box copies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vault_copies_and_the_recovery_kit() {
    let server = start("").await;
    let browser = Browser::login(&server.url).await;
    let (_, location) = browser
        .post("/devices", &[("name", "PC"), ("kind", "nixos")])
        .await;
    let pc_id = location.trim_start_matches("/devices/").to_string();
    let code = pairing_code(&browser, &pc_id).await;
    let (pc, _) = Device::pair(&server.url, &code, DeviceKind::Nixos)
        .await
        .unwrap();
    let (_, page) = browser.get(&format!("/devices/{pc_id}")).await;
    assert!(
        page.contains("services.replique.bitwardenCopy"),
        "the lines to add"
    );

    let put = |name: &str, body: Vec<u8>, token: Option<&str>| {
        let url = format!("{}{}", server.url, paths::vault(name));
        let token = token.map(str::to_string);
        async move {
            let mut r = reqwest::Client::new().put(url).body(body);
            if let Some(t) = token {
                r = r.bearer_auth(t);
            }
            r.send().await.unwrap().status()
        }
    };
    assert_eq!(
        put("bitwarden", b"sealed-1".to_vec(), Some(&pc.token)).await,
        StatusCode::OK
    );
    // The same content again only confirms the copy.
    assert_eq!(
        put("bitwarden", b"sealed-1".to_vec(), Some(&pc.token)).await,
        StatusCode::OK
    );
    let copies = server.app.store.vault_copies(&pc.id, "bitwarden").unwrap();
    assert_eq!(copies.len(), 1);
    assert!(copies[0].confirmed_at >= copies[0].received_at);
    // Refused: no token, a bad name, nothing.
    assert_eq!(
        put("bitwarden", b"x".to_vec(), None).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        !put("Bad/Name", b"x".to_vec(), Some(&pc.token))
            .await
            .is_success()
    );
    assert_eq!(
        put("bitwarden", vec![], Some(&pc.token)).await,
        StatusCode::BAD_REQUEST
    );
    // Ten kept.
    for i in 2..=12 {
        let body = format!("sealed-{i}").into_bytes();
        assert_eq!(
            put("bitwarden", body, Some(&pc.token)).await,
            StatusCode::OK
        );
    }
    let copies = server.app.store.vault_copies(&pc.id, "bitwarden").unwrap();
    assert_eq!(copies.len(), replique_server::vault::KEEP);
    // Downloaded as it was sent.
    let r = browser
        .http
        .get(format!("{}/vault/{}", server.url, copies[0].id))
        .send()
        .await
        .unwrap();
    assert!(
        r.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("bitwarden-pc-")
    );
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"sealed-12");
    // And on the box, in Réplique's own area.
    replique_server::vault::sync_box(&server.app).await;
    let area = server.local_root().join("replique/_replique");
    let on_box = std::fs::read_dir(area.join("vault/pc/bitwarden"))
        .unwrap()
        .count();
    assert_eq!(on_box, replique_server::vault::KEEP);
    let (_, page) = browser.get(&format!("/devices/{pc_id}")).await;
    assert!(page.contains("Last copy"));

    // A week without a copy: a warning.
    server
        .app
        .store
        .confirm_vault_copy(&copies[0].id, replique_server::db::now() - 8 * 86_400)
        .unwrap();
    replique_server::alerts::evaluate(&server.app)
        .await
        .unwrap();
    let titles = || {
        server
            .app
            .store
            .open_alerts()
            .unwrap()
            .into_iter()
            .map(|a| a.title)
            .collect::<Vec<_>>()
    };
    assert!(
        titles()
            .iter()
            .any(|t| t.contains("no bitwarden copy for a week")),
        "{:?}",
        titles()
    );
    assert_eq!(
        put("bitwarden", b"sealed-12".to_vec(), Some(&pc.token)).await,
        StatusCode::OK
    );
    replique_server::alerts::evaluate(&server.app)
        .await
        .unwrap();
    assert!(
        !titles().iter().any(|t| t.contains("copy for a week")),
        "{:?}",
        titles()
    );

    // ---- the recovery kit: a draft plan and checklist to start with
    let (_, page) = browser.get("/recovery").await;
    assert!(page.contains("Kit checklist") && page.contains("Hetzner: 2FA recovery codes"));
    assert!(page.contains("Not yet"));
    assert!(
        titles().iter().any(|t| t == "Recovery kit: to do"),
        "{:?}",
        titles()
    );
    // Written down, printed: nothing left to do.
    for e in server.app.store.recovery_entries().unwrap() {
        browser
            .post(&format!("/recovery/entries/{}/written", e.id), &[])
            .await;
    }
    browser.post("/recovery/printed", &[]).await;
    replique_server::alerts::evaluate(&server.app)
        .await
        .unwrap();
    assert!(
        !titles().iter().any(|t| t == "Recovery kit: to do"),
        "{:?}",
        titles()
    );
    // An entry saved unchanged leaves the envelopes current.
    let first = server.app.store.recovery_entries().unwrap().remove(0);
    let before = replique_server::recovery::version(&server.app);
    browser
        .post(
            &format!("/recovery/entries/{}", first.id),
            &[("name", first.name.as_str()), ("hint", first.hint.as_str())],
        )
        .await;
    assert_eq!(replique_server::recovery::version(&server.app), before);
    // A changed plan makes the envelopes out of date.
    let (status, _) = browser
        .post(
            "/recovery/plan",
            &[(
                "plan",
                "# Recovery plan\n\nEnvelope 2 is with Alex.\n<b>x</b>",
            )],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    replique_server::alerts::evaluate(&server.app)
        .await
        .unwrap();
    let alert = server
        .app
        .store
        .open_alert("recovery_pending")
        .unwrap()
        .expect("a reminder");
    assert!(alert.detail.contains("print it again"), "{}", alert.detail);
    // The print view: the plan, then the entries to fill in by hand, each page
    // saying its version; raw HTML stays text.
    let version = replique_server::recovery::version(&server.app);
    let (_, print) = browser.get("/recovery/print").await;
    assert!(print.contains(&format!("version {version}")));
    assert!(print.contains("Envelope 2 is with Alex"));
    assert!(print.contains("&lt;b&gt;x&lt;/b&gt;"));
    assert!(print.contains("GitHub: recovery codes"));
    // The plan and the checklist on the box, without any value.
    replique_server::recovery::sync_box(&server.app).await;
    let plan = std::fs::read_to_string(area.join("recovery/plan.md")).unwrap();
    assert!(plan.contains("Envelope 2 is with Alex"));
    let kit = std::fs::read_to_string(area.join("recovery/kit.md")).unwrap();
    assert!(kit.contains("Aegis password") && kit.contains("written:"));
}
