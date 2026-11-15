//! Against a real SFTP server standing in for the Storage Box: an OpenSSH
//! server in Docker whose user is chrooted like a sub-account (it sees its
//! folder as `/home`). Pairing with `--adopt`, a backup through the state
//! directory's SSH config, the `restic` passthrough, and a mirror through the
//! login the server lists in `/mirror-sources`.
//!
//! Needs Docker: `cargo test -p replique-client --test sftp -- --ignored`. It
//! builds a small Alpine image with OpenSSH, starts it as the container
//! `replique-b-sftp` on port 2223 and removes the container after.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::*;
use replique_proto::*;
use serde_json::{Value, json};

const CONTAINER: &str = "replique-b-sftp";
const IMAGE: &str = "replique-b-sftp:test";
const PORT: u16 = 2223;

/// Like a Storage Box sub-account: SFTP only, chrooted, the only writable
/// folder is `/home`. The key comes from the environment at start, so the
/// authorized_keys file is root's whatever owns files on the host.
const DOCKERFILE: &str = r#"
FROM alpine:3.22
RUN apk add --no-cache openssh-server \
 && ssh-keygen -A \
 && adduser -D -u 1001 sigma \
 && sed -i 's/^sigma:!/sigma:*/' /etc/shadow \
 && mkdir -p /srv/sigma/home /etc/ssh/keys \
 && chown sigma /srv/sigma/home \
 && printf 'Match User sigma\n  ChrootDirectory /srv/sigma\n  ForceCommand internal-sftp\n  AuthorizedKeysFile /etc/ssh/keys/%%u\n' >> /etc/ssh/sshd_config
CMD ["sh", "-c", "echo \"$AUTHORIZED_KEY\" > /etc/ssh/keys/sigma && exec /usr/sbin/sshd -D -e"]
"#;

struct Container;

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", CONTAINER])
            .output();
    }
}

fn docker(args: &[&str]) {
    let out = Command::new("docker").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore = "needs Docker"]
fn pair_backup_and_mirror_over_sftp() {
    let c = Client::new();
    let root = c.root();
    let keys = root.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    let key = keys.join("storagebox");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    let pass = keys.join("hetzner.pass");
    std::fs::write(&pass, "sftp test password\n").unwrap();

    let mut build = Command::new("docker")
        .args(["build", "-q", "-t", IMAGE, "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    build
        .stdin
        .take()
        .unwrap()
        .write_all(DOCKERFILE.as_bytes())
        .unwrap();
    assert!(build.wait().unwrap().success(), "docker build failed");

    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .output();
    let _guard = Container;
    let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
    docker(&[
        "run",
        "-d",
        "--name",
        CONTAINER,
        "-p",
        &format!("127.0.0.1:{PORT}:22"),
        "-e",
        &format!("AUTHORIZED_KEY={}", public.trim()),
        IMAGE,
    ]);

    let deadline = Instant::now() + Duration::from_secs(60);
    let known_hosts: Vec<String> = loop {
        let out = Command::new("ssh-keyscan")
            .args(["-p", &PORT.to_string(), "-t", "ed25519", "127.0.0.1"])
            .output()
            .unwrap();
        let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(str::to_string)
            .collect();
        if !lines.is_empty() {
            break lines;
        }
        assert!(Instant::now() < deadline, "the SFTP server never answered");
        std::thread::sleep(Duration::from_secs(1));
    };

    let mock = Mock::start();
    let storage = StorageInfo {
        host: "127.0.0.1".into(),
        port: PORT,
        username: "sigma".into(),
        repo_path: DEVICE_REPO_PATH.into(),
        known_hosts,
    };
    mock.with(|r| {
        r.storage = Some(storage.clone());
        r.mirror_sources = vec![MirrorSource {
            device_id: "dev_PHONE".into(),
            device_name: "Téléphone".into(),
            storage: StorageInfo {
                known_hosts: vec![],
                ..storage.clone()
            },
        }];
    });

    let out = c.run(&[
        "pair",
        "--adopt",
        key.to_str().unwrap(),
        pass.to_str().unwrap(),
        &mock.url,
        "K7QM-2XPA",
    ]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Created the restic repository"),
        "pairing should have initialised the repository"
    );

    let src = root.join("home");
    write(&src, "Documents/thesis.tex", "\\documentclass{article}");
    c.write_config(json!({
        "server": mock.url,
        "jobs": {
            "home": { "paths": [src], "retention": { "policy": "keep-deleted" }, "check_subset": "50%" },
            "phone": { "kind": "mirror", "mirror_source": "dev_PHONE", "mirror_path": root.join("phone"), "mirror_min_age": "0" },
        }
    }));
    assert_eq!(c.run(&["run", "home"]).status.code(), Some(0));
    let snapshot = mock.with(|r| {
        for kind in [RunKind::Backup, RunKind::Prune, RunKind::Check] {
            let f = r.runs_of(kind)[0].finish.clone().unwrap();
            assert_eq!(f.status, RunStatus::Success, "{kind:?}");
        }
        r.runs_of(RunKind::Backup)[0]
            .finish
            .clone()
            .unwrap()
            .summary
            .unwrap()
            .snapshot_id
            .unwrap()
    });

    let out = c.run(&["restic", "home", "snapshots", "--json"]);
    assert!(out.status.success());
    let snaps: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(snaps[0]["id"], snapshot.as_str());

    // The mirror copies the same repository through the listed login.
    assert_eq!(c.run(&["run", "phone"]).status.code(), Some(0));
    let m = mock.with(|r| r.runs_of(RunKind::Mirror)[0].finish.clone().unwrap());
    assert_eq!(m.status, RunStatus::Success);
    let summary = m.mirror.unwrap();
    assert_eq!(summary.snapshot_ids, [snapshot]);
    assert!(summary.hash_mismatches.is_empty());
    assert!(root.join("phone/config").is_file());
}
