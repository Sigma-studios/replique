//! `keep-deleted` on a fixture repository: files are created, edited and deleted
//! across a dozen daily snapshots, then the policy prunes. Every deleted file
//! must still be restorable, and the old versions of edited files that only
//! dropped snapshots held must be gone.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use common::{restic, write};
use replique_client::restic::{Restic, Snapshot};
use replique_client::retention;
use replique_client::signals::Stop;
use replique_proto::{KEEP_TAG, KeepCount, KeepRules, Retention};

const PASSWORD: &str = "fixture";
const HOST: &str = "fixture";

fn snapshot(repo: &Path, src: &Path, day: u32) {
    restic(
        repo,
        PASSWORD,
        &[
            "backup",
            "--quiet",
            "--host",
            HOST,
            "--time",
            &format!("2026-01-{day:02} 20:00:00"),
            src.to_str().unwrap(),
        ],
    );
}

fn snapshots(repo: &Path) -> Vec<Snapshot> {
    let out = restic(repo, PASSWORD, &["snapshots", "--json"]);
    let mut s: Vec<Snapshot> = serde_json::from_slice(&out.stdout).unwrap();
    s.sort_by_key(|s| s.time);
    s
}

/// The content of `rel` in each remaining snapshot that has it.
fn versions(repo: &Path, src: &Path, rel: &str) -> BTreeSet<String> {
    let path = src.join(rel);
    snapshots(repo)
        .iter()
        .filter_map(|s| {
            let out = Command::new("restic")
                .env("RESTIC_REPOSITORY", repo)
                .env("RESTIC_PASSWORD", PASSWORD)
                .args(["dump", &s.id, path.to_str().unwrap()])
                .output()
                .unwrap();
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        })
        .collect()
}

fn day_of(s: &Snapshot) -> u32 {
    s.time.format("%d").to_string().parse().unwrap()
}

fn tagged_days(repo: &Path) -> Vec<u32> {
    snapshots(repo)
        .iter()
        .filter(|s| s.tags.iter().flatten().any(|t| t == KEEP_TAG))
        .map(day_of)
        .collect()
}

#[test]
fn keep_deleted_keeps_every_deleted_file_and_drops_old_versions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let src = root.join("home");
    restic(&repo, PASSWORD, &["init", "--quiet"]);

    // edited.txt changes often; the gone* files each disappear at some point.
    // The comment after each step is what the day's snapshot holds.
    write(&src, "keep.txt", "always here");
    write(&src, "edited.txt", "v1");
    write(&src, "gone_a.txt", "a");
    snapshot(&repo, &src, 1); // v1, a
    write(&src, "edited.txt", "v2");
    snapshot(&repo, &src, 2); // v2, a
    write(&src, "edited.txt", "v3");
    write(&src, "gone_b.txt", "b");
    snapshot(&repo, &src, 3); // v3, a, b
    fs::remove_file(src.join("gone_a.txt")).unwrap();
    snapshot(&repo, &src, 4); // v3, b            (a's last copy is day 3)
    write(&src, "edited.txt", "v4");
    snapshot(&repo, &src, 5); // v4, b
    write(&src, "sub/dir/gone_c.txt", "c");
    snapshot(&repo, &src, 6); // v4, b, c
    fs::remove_dir_all(src.join("sub")).unwrap();
    snapshot(&repo, &src, 7); // v4, b            (c's last copy is day 6)
    write(&src, "edited.txt", "v5");
    snapshot(&repo, &src, 8); // v5, b
    write(&src, "edited.txt", "v6");
    write(&src, "gone d [x]*.txt", "d");
    snapshot(&repo, &src, 9); // v6, b, d
    fs::remove_file(src.join("gone_b.txt")).unwrap();
    snapshot(&repo, &src, 10); // v6, d           (b's last copy is day 9)
    write(&src, "edited.txt", "v7");
    fs::remove_file(src.join("gone d [x]*.txt")).unwrap();
    snapshot(&repo, &src, 11); // v7              (d's last copy is day 10, kept)
    snapshot(&repo, &src, 12); // v7

    let r = Restic {
        binary: "restic".into(),
        repository: repo.to_string_lossy().into_owned(),
        password_file: Some(root.join("pass")),
        options: vec![],
        no_lock: false,
    };
    fs::write(root.join("pass"), PASSWORD).unwrap();
    let policy = Retention::KeepDeleted {
        keep: KeepRules {
            daily: Some(KeepCount::Count(3)),
            ..KeepRules::default()
        },
    };
    let filters = retention::filters(&[src.to_string_lossy().into_owned()], Some(HOST));
    let mut phases = Vec::new();
    let summary = retention::apply(&r, &policy, &filters, &Stop::never(), |p| {
        phases.push(p.phase.unwrap_or_default())
    })
    .expect("keep-deleted prunes")
    .expect("prune succeeded");

    assert_eq!(summary.policy, "keep-deleted");
    assert_eq!(summary.snapshots_before, 12);
    // Days 3, 6 and 9 hold the last copy of a, c and b; 10 to 12 are the daily 3.
    assert_eq!(summary.snapshots_tagged_keep, 3);
    assert_eq!(summary.snapshots_removed, 6);
    assert!(summary.bytes_freed.is_some_and(|b| b > 0));
    assert!(phases.iter().any(|p| p == "diffing snapshots"));
    assert_eq!(
        snapshots(&repo).iter().map(day_of).collect::<Vec<_>>(),
        [3, 6, 9, 10, 11, 12]
    );
    assert_eq!(tagged_days(&repo), [3, 6, 9]);

    // Every deleted file can still be restored, with its content.
    for (rel, content) in [
        ("gone_a.txt", "a"),
        ("gone_b.txt", "b"),
        ("sub/dir/gone_c.txt", "c"),
        ("gone d [x]*.txt", "d"),
        ("keep.txt", "always here"),
    ] {
        assert_eq!(
            versions(&repo, &src, rel),
            BTreeSet::from([content.to_string()]),
            "{rel}"
        );
    }
    // The versions of edited.txt only dropped snapshots held are gone; those in
    // kept snapshots (tagged ones included, restic keeps snapshots whole) stay.
    let edited = versions(&repo, &src, "edited.txt");
    assert_eq!(
        edited,
        ["v3", "v4", "v6", "v7"]
            .map(String::from)
            .into_iter()
            .collect()
    );
    for gone in ["v1", "v2", "v5"] {
        assert!(!edited.contains(gone), "{gone} survived");
    }
    restic(&repo, PASSWORD, &["check", "--read-data"]);

    // Again with nothing new: nothing to tag, nothing to remove.
    let again = retention::apply(&r, &policy, &filters, &Stop::never(), |_| {})
        .unwrap()
        .unwrap();
    assert_eq!(
        (again.snapshots_tagged_keep, again.snapshots_removed),
        (0, 0)
    );
    assert_eq!(again.bytes_freed, None);

    // A new day pushes day 10 out of the daily 3. It holds d's last copy
    // (deleted on day 11), so it is tagged instead of dropped.
    snapshot(&repo, &src, 13);
    let next = retention::apply(&r, &policy, &filters, &Stop::never(), |_| {})
        .unwrap()
        .unwrap();
    assert_eq!((next.snapshots_tagged_keep, next.snapshots_removed), (1, 0));
    assert_eq!(tagged_days(&repo), [3, 6, 9, 10]);
    assert_eq!(
        versions(&repo, &src, "gone d [x]*.txt"),
        BTreeSet::from(["d".to_string()])
    );

    // And a day with nothing deleted is simply dropped.
    snapshot(&repo, &src, 14);
    let last = retention::apply(&r, &policy, &filters, &Stop::never(), |_| {})
        .unwrap()
        .unwrap();
    assert_eq!((last.snapshots_tagged_keep, last.snapshots_removed), (0, 1));
    assert_eq!(
        snapshots(&repo).iter().map(day_of).collect::<Vec<_>>(),
        [3, 6, 9, 10, 12, 13, 14]
    );
}

#[test]
fn versions_forgets_deleted_files_and_never_prune_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let src = root.join("home");
    restic(&repo, PASSWORD, &["init", "--quiet"]);
    write(&src, "gone.txt", "g");
    snapshot(&repo, &src, 1);
    fs::remove_file(src.join("gone.txt")).unwrap();
    write(&src, "new.txt", "n");
    snapshot(&repo, &src, 2);
    fs::write(root.join("pass"), PASSWORD).unwrap();
    let r = Restic {
        binary: "restic".into(),
        repository: repo.to_string_lossy().into_owned(),
        password_file: Some(root.join("pass")),
        options: vec![],
        no_lock: false,
    };
    let filters = retention::filters(&[src.to_string_lossy().into_owned()], Some(HOST));

    assert!(
        retention::apply(&r, &Retention::NeverPrune, &filters, &Stop::never(), |_| {}).is_none()
    );
    assert_eq!(snapshots(&repo).len(), 2);

    let keep_last = KeepRules {
        last: Some(KeepCount::Count(1)),
        ..KeepRules::default()
    };
    let s = retention::apply(
        &r,
        &Retention::Versions { keep: keep_last },
        &filters,
        &Stop::never(),
        |_| {},
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (
            s.snapshots_before,
            s.snapshots_removed,
            s.snapshots_tagged_keep
        ),
        (2, 1, 0)
    );
    assert!(versions(&repo, &src, "gone.txt").is_empty());
}
