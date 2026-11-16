//! The client end to end: the binary against the mock server and local restic
//! repositories.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::*;
use replique_proto::*;
use serde_json::{Value, json};

fn mode(p: &std::path::Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

/// A client paired with the mock, with one backup job on a local repository.
fn backup_client(mock: &Mock, job: Value) -> (Client, std::path::PathBuf) {
    let c = Client::new();
    c.pair(mock);
    let repo = c.root().join("repo");
    let mut job = job;
    job["repository"] = json!(repo);
    c.write_config(json!({ "server": mock.url, "jobs": { "home": job } }));
    (c, repo)
}

fn password(c: &Client) -> String {
    fs::read_to_string(c.state.join("restic-password"))
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn pairing_stores_what_jobs_need() {
    let mock = Mock::start();
    let c = Client::new();
    c.pair(&mock);

    let req = mock.with(|r| r.pair_requests[0].clone());
    assert!(req.ssh_public_key.starts_with("ssh-ed25519 "));
    assert_eq!(
        req.client_version.as_deref(),
        Some(client_version_string().as_str())
    );

    let s = &c.state;
    assert_eq!(fs::read_to_string(s.join("token")).unwrap().trim(), TOKEN);
    assert_eq!(mode(&s.join("token")), 0o600);
    assert_eq!(mode(&s.join("restic-password")), 0o600);
    assert_eq!(mode(&s.join("ssh/id_ed25519")), 0o600);
    assert_eq!(password(&c).len(), 43);
    let pubkey = fs::read_to_string(s.join("ssh/id_ed25519.pub")).unwrap();
    assert_eq!(pubkey.trim(), req.ssh_public_key);

    let device: Value = serde_json::from_slice(&fs::read(s.join("device.json")).unwrap()).unwrap();
    assert_eq!(device["device_id"], DEVICE_ID);
    assert_eq!(device["storage"]["username"], "u1-sub1");

    let ssh = fs::read_to_string(s.join("ssh/config")).unwrap();
    assert!(ssh.contains("Host replique-storage\n"), "{ssh}");
    assert!(ssh.contains("Port 23\n"));
    assert!(ssh.contains("User u1-sub1\n"));
    assert!(ssh.contains("StrictHostKeyChecking yes\n"));
    assert!(ssh.contains(&format!(
        "IdentityFile {}\n",
        s.join("ssh/id_ed25519").display()
    )));
    let known = fs::read_to_string(s.join("ssh/known_hosts")).unwrap();
    assert!(known.starts_with("[box.example]:23 ssh-ed25519 "));

    // Pairing again needs --force.
    let again = c.run(&["pair", "--yes", "--no-init", &mock.url, "K7QM-2XPA"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("--force"));

    // A wrong code is the server's message.
    let other = Client::new();
    let bad = other.run(&["pair", "--yes", "--no-init", &mock.url, "AAAA-AAAA"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown code"));
}

fn client_version_string() -> String {
    format!("replique-client {}", env!("CARGO_PKG_VERSION"))
}

#[test]
fn adopting_keeps_the_existing_key_and_password() {
    let mock = Mock::start();
    let c = Client::new();
    let keys = c.root().join("old");
    fs::create_dir_all(&keys).unwrap();
    let key = keys.join("storagebox");
    assert!(
        std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    let pass = keys.join("hetzner.pass");
    fs::write(&pass, "the old password\n").unwrap();
    let link = format!(
        "replique://pair?server={}&code=k7qm2xpa",
        url_escape(&mock.url)
    );
    let out = c.run(&[
        "pair",
        "--adopt",
        key.to_str().unwrap(),
        pass.to_str().unwrap(),
        "--no-init",
        &link,
    ]);
    assert!(out.status.success());
    // Nothing to confirm: the password is the user's already, and not printed.
    assert!(!String::from_utf8_lossy(&out.stdout).contains("the old password"));
    assert_eq!(
        fs::read(c.state.join("restic-password")).unwrap(),
        b"the old password\n"
    );
    assert_eq!(
        fs::read(c.state.join("ssh/id_ed25519")).unwrap(),
        fs::read(&key).unwrap()
    );
    let req = mock.with(|r| r.pair_requests[0].clone());
    let pubkey = fs::read_to_string(keys.join("storagebox.pub")).unwrap();
    assert_eq!(
        req.ssh_public_key.split_whitespace().nth(1),
        pubkey.split_whitespace().nth(1)
    );
    let device: Value =
        serde_json::from_slice(&fs::read(c.state.join("device.json")).unwrap()).unwrap();
    assert_eq!(device["adopted"], true);
}

fn url_escape(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}

#[test]
fn backup_prune_and_check_are_three_reported_runs() {
    let mock = Mock::start();
    let (c, repo) = backup_client(
        &mock,
        json!({
            "kind": "backup",
            "schedule": "daily",
            "paths": [],
            "retention": { "policy": "versions", "keep": { "last": 1 } },
            "check_subset": "100%",
        }),
    );
    let src = c.root().join("src");
    write(&src, "a.txt", "hello");
    let mut config: Value = serde_json::from_slice(&fs::read(&c.config).unwrap()).unwrap();
    config["jobs"]["home"]["paths"] = json!([src]);
    c.write_config(config);

    let out = c
        .cmd(&["run", "home"])
        .env("RESTIC_PROGRESS_FPS", "60")
        .output()
        .unwrap();
    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(0));
    // The repository did not exist: the run created it.
    assert!(repo.join("config").exists());

    write(&src, "b.txt", "second");
    assert_eq!(c.run(&["run", "home"]).status.code(), Some(0));

    mock.with(|r| {
        // Declared once: the second run's rules had not changed.
        assert_eq!(r.job_puts.len(), 1);
        let (name, spec) = &r.job_puts[0];
        assert_eq!(name, "home");
        assert_eq!(spec.schedule.as_deref(), Some("daily"));
        assert_eq!(spec.rules.check_subset.as_deref(), Some("100%"));

        let backups = r.runs_of(RunKind::Backup);
        assert_eq!(backups.len(), 2);
        for b in &backups {
            let f = b.finish.as_ref().expect("finished");
            assert_eq!(f.status, RunStatus::Success);
            let s = f.summary.as_ref().unwrap();
            assert_eq!(s.snapshot_id.as_ref().map(String::len), Some(64));
            assert_eq!(
                b.start
                    .restic_version
                    .as_deref()
                    .map(|v| v.split('.').count()),
                Some(3)
            );
        }
        assert_eq!(
            backups[0]
                .finish
                .as_ref()
                .unwrap()
                .summary
                .as_ref()
                .unwrap()
                .files_new,
            1
        );
        // Throttled to one every ten seconds: the first one only.
        assert!(!backups[0].progress.is_empty(), "no progress forwarded");
        assert!(backups[0].progress.len() <= 2);

        let prunes = r.runs_of(RunKind::Prune);
        assert_eq!(prunes.len(), 2);
        let p = prunes[1].finish.as_ref().unwrap().prune.clone().unwrap();
        assert_eq!(p.policy, "versions");
        assert_eq!(p.snapshots_before, 2);
        assert_eq!(p.snapshots_removed, 1);
        assert!(p.bytes_freed.is_some());

        let checks = r.runs_of(RunKind::Check);
        assert_eq!(checks.len(), 2);
        let ch = checks[1].finish.as_ref().unwrap();
        assert_eq!(ch.status, RunStatus::Success);
        let cs = ch.check.as_ref().unwrap();
        assert_eq!(cs.read_data_subset.as_deref(), Some("100%"));
        let last_snapshot = backups[1]
            .finish
            .as_ref()
            .unwrap()
            .summary
            .as_ref()
            .unwrap()
            .snapshot_id
            .clone()
            .unwrap();
        assert_eq!(cs.snapshot_ids, [last_snapshot]);
    });
    assert_eq!(snapshot_ids(&repo, &password(&c)).len(), 1);

    let status = c.run(&["status"]);
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("home"), "{text}");
    assert!(text.contains("backup"));
    assert!(text.contains("check"));
    assert!(text.contains("success"));
    assert!(text.contains("sent"));

    // The wrapper's way into the repository.
    let snaps = c.run(&["restic", "home", "snapshots", "--json"]);
    assert!(snaps.status.success());
}

#[test]
fn unreadable_files_make_a_partial_run_with_exit_3() {
    let mock = Mock::start();
    let (c, _repo) = backup_client(
        &mock,
        json!({ "paths": [], "retention": { "policy": "never-prune" } }),
    );
    let src = c.root().join("src");
    write(&src, "ok.txt", "fine");
    write(&src, "secret.txt", "locked");
    fs::set_permissions(src.join("secret.txt"), fs::Permissions::from_mode(0o000)).unwrap();
    let mut config: Value = serde_json::from_slice(&fs::read(&c.config).unwrap()).unwrap();
    config["jobs"]["home"]["paths"] = json!([src]);
    c.write_config(config);

    let out = c.run(&["run", "home"]);
    fs::set_permissions(src.join("secret.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(out.status.code(), Some(3));
    mock.with(|r| {
        let f = r.runs_of(RunKind::Backup)[0].finish.clone().unwrap();
        assert_eq!(f.status, RunStatus::Partial);
        assert_eq!(f.exit_code, 3);
        assert!(f.summary.unwrap().snapshot_id.is_some());
        assert_eq!(f.error_count, 1);
        assert_eq!(
            f.unreadable_paths,
            [src.join("secret.txt").to_string_lossy()]
        );
        assert_eq!(f.errors[0].during.as_deref(), Some("archival"));
        // never-prune: no prune run at all.
        assert!(r.runs_of(RunKind::Prune).is_empty());
    });
}

#[test]
fn a_missing_mount_refuses_the_run() {
    let mock = Mock::start();
    let (c, repo) = backup_client(
        &mock,
        json!({ "paths": ["/mnt/windows/c/Users"], "require_mounts": ["/mnt/windows/c"] }),
    );
    let out = c.run(&["run", "home"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!repo.exists(), "restic must not have run");
    mock.with(|r| {
        let f = r.runs_of(RunKind::Backup)[0].finish.clone().unwrap();
        assert_eq!(f.status, RunStatus::Failed);
        assert_eq!(
            f.message.as_deref(),
            Some("/mnt/windows/c is not a mountpoint")
        );
    });
}

#[test]
fn reports_survive_a_server_outage() {
    let mock = Mock::start();
    let (c, _repo) = backup_client(
        &mock,
        json!({ "paths": [], "retention": { "policy": "never-prune" } }),
    );
    let src = c.root().join("src");
    write(&src, "a.txt", "1");
    let mut config: Value = serde_json::from_slice(&fs::read(&c.config).unwrap()).unwrap();
    config["jobs"]["home"]["paths"] = json!([src]);
    c.write_config(config.clone());

    // Down (503): the backup still runs and succeeds, its reports wait.
    mock.set_down(true);
    assert_eq!(c.run(&["run", "home"]).status.code(), Some(0));
    assert_eq!(c.spooled(), 1);
    let status = String::from_utf8_lossy(&c.run(&["status"]).stdout).into_owned();
    assert!(status.contains("spooled"), "{status}");

    // Not even listening: the same.
    let mut gone = config.clone();
    gone["server"] = json!("http://127.0.0.1:9");
    c.write_config(gone);
    write(&src, "b.txt", "2");
    assert_eq!(c.run(&["run", "home"]).status.code(), Some(0));
    assert_eq!(c.spooled(), 2);
    assert!(mock.with(|r| r.runs.is_empty()));

    // Back: the next run sends both before its own, oldest first.
    c.write_config(config);
    mock.set_down(false);
    write(&src, "c.txt", "3");
    assert_eq!(c.run(&["run", "home"]).status.code(), Some(0));
    assert_eq!(c.spooled(), 0);
    mock.with(|r| {
        let runs = r.runs_of(RunKind::Backup);
        assert_eq!(runs.len(), 3);
        let new: Vec<u64> = runs
            .iter()
            .map(|r| {
                let f = r.finish.as_ref().expect("every run finished");
                assert_eq!(f.status, RunStatus::Success);
                f.summary.as_ref().unwrap().files_new
            })
            .collect();
        assert_eq!(new, [1, 1, 1]);
        assert!(runs[0].start.started_at < runs[1].start.started_at);
        assert!(runs[1].start.started_at < runs[2].start.started_at);
    });
}

#[test]
fn a_finish_lost_after_its_start_is_replayed_once() {
    // The start went through, the finish did not: replaying sends the start
    // again (same client_run_id, same run) and then the finish.
    let mock = Mock::start();
    let c = Client::new();
    c.pair(&mock);
    c.write_config(json!({ "server": mock.url, "jobs": {} }));
    let state = replique_client::state::State::new(&c.state);
    let config = replique_client::config::ClientConfig::load(&c.config, true).unwrap();
    let reporter = replique_client::report::Reporter::new(&config, &state, "home").unwrap();
    let run = reporter.start(RunKind::Backup);
    let start = run.start.clone();
    mock.set_down(true);
    reporter.finish(
        run,
        replique_client::run::failed(&replique_client::restic::Outcome::default()),
    );
    assert_eq!(c.spooled(), 1);
    mock.set_down(false);
    reporter.replay();
    assert_eq!(c.spooled(), 0);
    mock.with(|r| {
        assert_eq!(r.runs.len(), 1);
        assert_eq!(r.start_calls, 2);
        let run = r.runs.values().next().unwrap();
        assert_eq!(run.start.client_run_id, start.client_run_id);
        assert_eq!(run.finish.as_ref().unwrap().status, RunStatus::Success);
    });
}

#[test]
fn excludes_cover_build_output_and_escape_paths() {
    let mock = Mock::start();
    let (c, repo) = backup_client(&mock, json!({ "paths": [] }));
    let src = c.root().join("home");
    for (rel, content) in [
        ("Games/Unity/Assets/scene.unity", "s"),
        ("Games/Unity/ProjectSettings/p.asset", "p"),
        ("Games/Unity/Library/huge.bin", "x"),
        ("Games/Unity/Temp/t", "x"),
        ("Games/Shooter/Shooter.uproject", "u"),
        ("Games/Shooter/Content/map.umap", "m"),
        ("Games/Shooter/Intermediate/i.obj", "x"),
        ("Games/Shooter/DerivedDataCache/d", "x"),
        ("Games/Shooter/Binaries/b.exe", "x"),
        ("Games/Shooter/Saved/s.log", "x"),
        ("Code/App/App.csproj", "c"),
        ("Code/App/Program.cs", "c"),
        ("Code/App/bin/Debug/App.dll", "x"),
        ("Code/App/obj/o", "x"),
        ("Code/fast/Cargo.toml", "c"),
        (
            "Code/fast/target/CACHEDIR.TAG",
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        ),
        ("Code/fast/target/debug/fast", "x"),
        ("Code/game/project.godot", "g"),
        ("Code/game/.godot/imported", "x"),
        ("$RECYCLE.BIN/deleted.txt", "x"),
        ("$Games/U/Assets/a", "a"),
        ("$Games/U/ProjectSettings/p", "p"),
        ("$Games/U/Library/l", "x"),
        ("Weird [v2]*/Assets/a", "a"),
        ("Weird [v2]*/ProjectSettings/p", "p"),
        ("Weird [v2]*/Library/l", "x"),
        // Not a Unity project; an unescaped `[v2]*` would have matched it.
        ("Weird vx/Library/keep.txt", "k"),
        ("notes.txt", "n"),
    ] {
        write(&src, rel, content);
    }
    let mut config: Value = serde_json::from_slice(&fs::read(&c.config).unwrap()).unwrap();
    config["jobs"]["home"]["paths"] = json!([src]);
    config["jobs"]["home"]["exclude"] = json!(["$RECYCLE.BIN"]);
    config["jobs"]["home"]["exclude_caches"] = json!(true);
    config["jobs"]["home"]["generated_excludes_for"] = json!([src]);
    c.write_config(config);

    assert_eq!(c.run(&["run", "home"]).status.code(), Some(0));
    let out = restic(&repo, &password(&c), &["ls", "latest", "--json"]);
    let files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "file")
        .map(|v| {
            v["path"]
                .as_str()
                .unwrap()
                .strip_prefix(&format!("{}/", src.display()))
                .unwrap()
                .to_string()
        })
        .collect();
    let mut files = files;
    files.sort();
    assert_eq!(
        files,
        [
            "$Games/U/Assets/a",
            "$Games/U/ProjectSettings/p",
            "Code/App/App.csproj",
            "Code/App/Program.cs",
            "Code/fast/Cargo.toml",
            // restic keeps the tag itself, and nothing else of the folder.
            "Code/fast/target/CACHEDIR.TAG",
            "Code/game/project.godot",
            "Games/Shooter/Content/map.umap",
            "Games/Shooter/Shooter.uproject",
            "Games/Unity/Assets/scene.unity",
            "Games/Unity/ProjectSettings/p.asset",
            "Weird [v2]*/Assets/a",
            "Weird [v2]*/ProjectSettings/p",
            "Weird vx/Library/keep.txt",
            "notes.txt",
        ]
    );
    let excludes = fs::read_to_string(c.state.join("excludes/home.txt")).unwrap();
    assert!(excludes.contains("\n?RECYCLE.BIN\n"), "{excludes}");
    assert!(excludes.contains("/?Games/U/Library\n"), "{excludes}");
    assert!(
        excludes.contains("/Weird \\[v2]\\*/Library\n"),
        "{excludes}"
    );
}

#[test]
fn vault_copy_sends_the_file_as_it_is() {
    let mock = Mock::start();
    let c = Client::new();
    c.pair(&mock);
    c.write_config(json!({ "server": mock.url, "jobs": {} }));
    let file = c.root().join("data.json");
    fs::write(&file, b"{\"encrypted\":\"2.abc|def|ghi\"}").unwrap();
    let out = c.run(&["vault-copy", "bitwarden", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    mock.with(|r| {
        assert_eq!(r.vault.len(), 1);
        assert_eq!(r.vault[0].0, "bitwarden");
        assert_eq!(r.vault[0].1, fs::read(&file).unwrap());
    });
    // The server away: a temporary failure (75), the next change sends it.
    mock.with(|r| r.down = true);
    assert_eq!(
        c.run(&["vault-copy", "bitwarden", file.to_str().unwrap()])
            .status
            .code(),
        Some(75)
    );
    // A missing file is an error.
    assert_eq!(
        c.run(&["vault-copy", "bitwarden", "/nonexistent/data.json"])
            .status
            .code(),
        Some(1)
    );
}
