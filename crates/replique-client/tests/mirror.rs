//! Mirror jobs: an rclone copy of another device's repository, hash-checked
//! without its password.

mod common;

use std::fs;

use common::*;
use replique_proto::*;
use serde_json::json;
use sha2::{Digest, Sha256};

const PHONE_PASSWORD: &str = "the phone's password, unknown to the PC";

#[test]
fn mirror_copies_verifies_and_keeps_removed_files() {
    let mock = Mock::start();
    let c = Client::new();
    c.pair(&mock);
    let root = c.root();
    let phone = root.join("phone-repo");
    let src = root.join("phone-files");
    restic(&phone, PHONE_PASSWORD, &["init", "--quiet"]);
    write(&src, "DCIM/1.jpg", "photo one");
    restic(
        &phone,
        PHONE_PASSWORD,
        &["backup", "--quiet", src.to_str().unwrap()],
    );

    // An upload in progress on the source, as the phone's rclone names it.
    let pack_dir = fs::read_dir(phone.join("data"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let uploading = pack_dir.join(format!("{}.3f2a9c1d.partial", "ab".repeat(32)));
    fs::write(&uploading, b"half a pack").unwrap();

    let copy = root.join("Backups/phone");
    c.write_config(json!({
        "server": mock.url,
        "jobs": { "phone": {
            "kind": "mirror",
            "schedule": "daily",
            "mirror_source": "dev_PHONE",
            "mirror_path": copy,
            "mirror_from": phone,
        }}
    }));
    assert_eq!(c.run(&["mirror", "phone"]).status.code(), Some(0));

    let first = mock.with(|r| r.runs_of(RunKind::Mirror)[0].finish.clone().unwrap());
    assert_eq!(first.status, RunStatus::Success);
    let m = first.mirror.unwrap();
    assert_eq!(m.source_device_id, "dev_PHONE");
    assert_eq!(m.snapshot_ids, snapshot_ids(&phone, PHONE_PASSWORD));
    assert!(m.files_copied >= 3, "data, index and snapshot files: {m:?}");
    assert_eq!(m.files_verified, m.files_copied);
    assert!(m.hash_mismatches.is_empty());
    assert!(m.total_bytes.unwrap() > 0);
    let copied_partial = walkdir(&copy)
        .into_iter()
        .any(|p| p.to_string_lossy().ends_with(".partial"));
    assert!(!copied_partial, "an upload in progress is not copied");
    fs::remove_file(&uploading).unwrap();
    // The copy is a working repository for whoever has the password.
    assert_eq!(
        snapshot_ids(&copy, PHONE_PASSWORD),
        snapshot_ids(&phone, PHONE_PASSWORD)
    );
    mock.with(|r| {
        let (name, spec) = &r.job_puts[0];
        assert_eq!(name, "phone");
        assert_eq!(spec.kind, JobKind::Mirror);
        assert_eq!(spec.rules.mirror_source.as_deref(), Some("dev_PHONE"));
        assert_eq!(spec.rules.retention, None);
    });

    // A second snapshot on the phone, and the first one forgotten there: the
    // copy follows, and keeps the forgotten snapshot's file aside.
    write(&src, "DCIM/2.jpg", "photo two");
    restic(
        &phone,
        PHONE_PASSWORD,
        &["backup", "--quiet", src.to_str().unwrap()],
    );
    let old = snapshot_ids(&phone, PHONE_PASSWORD)[0].clone();
    restic(&phone, PHONE_PASSWORD, &["forget", "--quiet", &old]);
    assert_eq!(c.run(&["run", "phone"]).status.code(), Some(0));

    let second = mock.with(|r| r.runs_of(RunKind::Mirror)[1].finish.clone().unwrap());
    let m2 = second.mirror.unwrap();
    assert_eq!(m2.snapshot_ids, snapshot_ids(&phone, PHONE_PASSWORD));
    assert!(!m2.snapshot_ids.contains(&old));
    assert_eq!(m2.files_removed, 1);
    // Only the new files were hashed.
    assert_eq!(m2.files_verified, m2.files_copied);
    assert!(m2.files_copied < m.files_copied + 3);
    let removed_root = root.join("Backups/phone.removed");
    let stamps: Vec<_> = fs::read_dir(&removed_root).unwrap().flatten().collect();
    assert_eq!(stamps.len(), 1);
    assert!(stamps[0].path().join("snapshots").join(&old).is_file());

    // A file on the source that does not match its name is caught.
    let bogus = format!("{:x}", Sha256::digest(b"what the name promises"));
    fs::write(phone.join("index").join(&bogus), b"something else").unwrap();
    assert_eq!(c.run(&["run", "phone"]).status.code(), Some(1));
    let third = mock.with(|r| r.runs_of(RunKind::Mirror)[2].finish.clone().unwrap());
    assert_eq!(third.status, RunStatus::Failed);
    assert_eq!(
        third.mirror.unwrap().hash_mismatches,
        [format!("index/{bogus}")]
    );
    assert!(third.message.unwrap().contains("do not match their name"));

    let status = String::from_utf8_lossy(&c.run(&["status"]).stdout).into_owned();
    assert!(status.contains("mirror"), "{status}");
    assert!(status.contains("failed"), "{status}");
}

#[test]
fn a_mirror_the_server_does_not_list_fails_with_a_reason() {
    let mock = Mock::start();
    let c = Client::new();
    c.pair(&mock);
    c.write_config(json!({
        "server": mock.url,
        "jobs": { "phone": {
            "kind": "mirror",
            "mirror_source": "dev_UNKNOWN",
            "mirror_path": c.root().join("copy"),
        }}
    }));
    assert_eq!(c.run(&["mirror", "phone"]).status.code(), Some(1));
    let f = mock.with(|r| r.runs_of(RunKind::Mirror)[0].finish.clone().unwrap());
    assert_eq!(f.status, RunStatus::Failed);
    assert!(
        f.message
            .unwrap()
            .contains("lists no mirror source dev_UNKNOWN")
    );
    // `mirror` refuses a backup job.
    c.write_config(json!({ "jobs": { "home": { "paths": ["/x"] } } }));
    assert!(!c.run(&["mirror", "home"]).status.success());
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}
