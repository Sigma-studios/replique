//! `status` while runs go on: `runs/<job>.current.json`, written by the running
//! client and read by `status` and `status --json`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};

/// An executable shell script.
fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A restic whose backup prints one status line, then waits until `gate`
/// exists before printing its summary.
fn fake_restic(dir: &Path, gate: &Path) -> PathBuf {
    let path = dir.join("restic");
    script(
        &path,
        &format!(
            r#"for a in "$@"; do
  case "$a" in
    version) echo "restic 0.18.0 compiled with go1.24 on linux/amd64"; exit 0 ;;
    cat|unlock) exit 0 ;;
    backup)
      echo '{{"message_type":"status","percent_done":0.43,"seconds_elapsed":90,"seconds_remaining":3600,"total_files":10,"files_done":4,"total_bytes":1000000,"bytes_done":430000}}'
      while [ ! -e '{gate}' ]; do sleep 0.05; done
      echo '{{"message_type":"summary","files_new":4,"snapshot_id":"8c5f2a3b8c5f2a3b8c5f2a3b8c5f2a3b8c5f2a3b8c5f2a3b8c5f2a3b8c5f2a3b"}}'
      exit 0 ;;
  esac
done
exit 1
"#,
            gate = gate.display()
        ),
    );
    path
}

/// Real restic, except that `forget` and `check` each wait for a file of that
/// name in `gates`.
fn gated_restic(dir: &Path, gates: &Path) -> PathBuf {
    let path = dir.join("restic");
    script(
        &path,
        &format!(
            r#"for a in "$@"; do
  case "$a" in
    forget|check) while [ ! -e '{gates}/'"$a" ]; do sleep 0.05; done; break ;;
  esac
done
exec restic "$@"
"#,
            gates = gates.display()
        ),
    );
    path
}

fn status_json(c: &Client) -> Value {
    let out = c.cmd(&["status", "--json"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn status_text(c: &Client) -> String {
    String::from_utf8_lossy(&c.run(&["status"]).stdout).into_owned()
}

/// Polls `status --json` until `ok` holds, or fails after 30 seconds.
fn wait_for(c: &Client, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let v = status_json(c);
        if ok(&v) {
            return v;
        }
        assert!(Instant::now() < deadline, "never saw {what}: {v:#}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn spawn_run(c: &Client, restic: &Path) -> Child {
    c.cmd(&["run", "home"])
        .env("REPLIQUE_RESTIC", restic)
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

fn current_file(c: &Client, job: &str) -> PathBuf {
    c.state.join(format!("runs/{job}.current.json"))
}

/// A backup paused halfway: `status` shows it, then forgets it once it is over.
fn backup_in_progress(c: &Client) {
    let gate = c.root().join("go");
    let restic = fake_restic(&c.root(), &gate);
    let mut child = spawn_run(c, &restic);

    let v = wait_for(c, "the backup's progress", |v| {
        v["home"]["current"]["progress"]["percent_done"] == json!(0.43)
    });
    let cur = &v["home"]["current"];
    assert_eq!(cur["kind"], "backup");
    assert_eq!(cur["pid"], json!(child.id()));
    assert_eq!(cur["progress"]["bytes_done"], 430000);
    assert_eq!(cur["progress"]["seconds_remaining"], 3600);
    assert!(cur["client_run_id"].as_str().is_some_and(|s| s.len() == 36));
    assert!(cur["started_at"].as_str().is_some());
    assert!(v["home"].get("backup").is_none(), "{v:#}");

    let text = status_text(c);
    assert!(
        text.contains("home: backup running for "),
        "no line for the run: {text}"
    );
    assert!(
        text.contains(", 43% (419.9 KiB of 976.6 KiB), about 1h00m left\n"),
        "{text}"
    );
    // The file is `home`'s, not a job of its own.
    assert!(!text.contains("home.current"), "{text}");
    assert!(text.contains("home  -    running"), "{text}");

    // A job that is not configured shows while it runs.
    fs::copy(current_file(c, "home"), current_file(c, "old")).unwrap();
    let v = status_json(c);
    assert_eq!(v["old"]["current"]["kind"], "backup", "{v:#}");
    assert!(status_text(c).contains("old: backup running for "));
    fs::remove_file(current_file(c, "old")).unwrap();

    fs::write(&gate, "").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    let v = status_json(c);
    assert!(v["home"].get("current").is_none(), "{v:#}");
    assert_eq!(v["home"]["backup"]["status"], "success");
    assert!(!current_file(c, "home").exists());
    assert!(!status_text(c).contains("running"));
}

fn job_config(server: &str, job: Value) -> Value {
    json!({ "server": server, "jobs": { "home": job } })
}

#[test]
fn a_backup_in_progress_shows_in_status() {
    let mock = Mock::start();
    let c = Client::new();
    c.pair(&mock);
    let src = c.root().join("src");
    write(&src, "a.txt", "a");
    c.write_config(job_config(
        &mock.url,
        json!({
            "paths": [src],
            "repository": c.root().join("repo"),
            "retention": { "policy": "never-prune" },
        }),
    ));
    backup_in_progress(&c);
    // The server got the run as well.
    mock.with(|r| {
        let runs = r.runs_of(replique_proto::RunKind::Backup);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].progress[0].percent_done, 0.43);
    });
}

#[test]
fn a_backup_in_progress_shows_without_the_server() {
    let mock = Mock::start();
    let c = Client::new();
    c.pair(&mock);
    let src = c.root().join("src");
    write(&src, "a.txt", "a");
    // Nothing listens there.
    c.write_config(job_config(
        "http://127.0.0.1:9",
        json!({
            "paths": [src],
            "repository": c.root().join("repo"),
            "retention": { "policy": "never-prune" },
        }),
    ));
    backup_in_progress(&c);
    assert_eq!(c.spooled(), 1);
    assert_eq!(status_json(&c)["home"]["backup"]["delivery"], "spooled");
}

#[test]
fn a_file_left_by_a_killed_client_is_ignored_and_removed() {
    let c = Client::new();
    c.write_config(json!({ "jobs": { "home": { "paths": ["/srv"] } } }));
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    gone.wait().unwrap();
    let left = |pid: u32| {
        json!({
            "kind": "backup",
            "client_run_id": "0b6c8a4e-5d1f-4f7e-9a3c-2e8d7b1f6a90",
            "started_at": "2026-11-18T08:12:14Z",
            "pid": pid,
            "progress": { "at": "2026-11-18T08:13:14Z", "percent_done": 0.43 },
        })
    };
    // A pid that is gone, and one that is alive but not a client (this test,
    // as if the pid had been given to another process since).
    for pid in [gone.id(), std::process::id()] {
        write(&c.state, "runs/home.current.json", &left(pid).to_string());
        write(&c.state, "runs/old.current.json", &left(pid).to_string());
        let v = status_json(&c);
        assert!(v["home"].get("current").is_none(), "{v:#}");
        assert!(v.get("old").is_none(), "{v:#}");
        assert!(!current_file(&c, "home").exists());
        assert!(!current_file(&c, "old").exists());

        write(&c.state, "runs/home.current.json", &left(pid).to_string());
        write(&c.state, "runs/old.current.json", &left(pid).to_string());
        let text = status_text(&c);
        assert!(text.contains("home  -    never run"), "{text}");
        assert!(!text.contains("running"), "{text}");
        assert!(!text.contains("old"), "{text}");
        assert!(!current_file(&c, "home").exists());
        assert!(!current_file(&c, "old").exists());
    }
}

#[test]
fn prune_then_check_take_over_the_file_in_turn() {
    let mock = Mock::start();
    let c = Client::new();
    c.pair(&mock);
    let src = c.root().join("src");
    write(&src, "a.txt", "a");
    c.write_config(job_config(
        &mock.url,
        json!({
            "paths": [src],
            "repository": c.root().join("repo"),
            "retention": { "policy": "versions", "keep": { "last": 1 } },
            "check_subset": "100%",
        }),
    ));
    let gates = c.root().join("gates");
    fs::create_dir_all(&gates).unwrap();
    let restic = gated_restic(&c.root(), &gates);
    let mut child = spawn_run(&c, &restic);

    // Each start writes the file without progress; the phase follows.
    let v = wait_for(&c, "the prune", |v| {
        v["home"]["current"]["progress"]["phase"] == "forgetting snapshots"
    });
    assert_eq!(v["home"]["current"]["kind"], "prune");
    assert_eq!(v["home"]["backup"]["status"], "success");
    let prune_id = v["home"]["current"]["client_run_id"].clone();
    let text = status_text(&c);
    assert!(
        text.contains("home: prune running for ") && text.contains(", forgetting snapshots\n"),
        "{text}"
    );

    fs::write(gates.join("forget"), "").unwrap();
    let v = wait_for(&c, "the check", |v| {
        v["home"]["current"]["progress"]["phase"] == "checking snapshots and trees"
    });
    assert_eq!(v["home"]["current"]["kind"], "check");
    assert_ne!(v["home"]["current"]["client_run_id"], prune_id);
    assert_eq!(v["home"]["prune"]["status"], "success");

    fs::write(gates.join("check"), "").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    let v = status_json(&c);
    assert!(v["home"].get("current").is_none(), "{v:#}");
    assert_eq!(v["home"]["check"]["status"], "success");
    assert!(!current_file(&c, "home").exists());
}
