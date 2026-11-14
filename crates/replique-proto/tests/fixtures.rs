//! Every fixture in `proto/fixtures` parses into its type and survives a round
//! trip. Unknown fields are ignored by design, so a misspelt field in a fixture
//! would silently vanish; `assert_subset` catches that by requiring every value
//! in the fixture to come back out of the serialised type.

use std::path::PathBuf;

use replique_proto::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../proto/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(name: &str) -> T {
    let text = fixture(name);
    let original: Value = serde_json::from_str(&text).unwrap();
    let parsed: T = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
    let out = serde_json::to_value(&parsed).unwrap();
    assert_subset(name, "", &original, &out);
    let again: T = serde_json::from_value(out).unwrap();
    assert_eq!(parsed, again, "{name}");
    parsed
}

fn assert_subset(name: &str, at: &str, fixture: &Value, out: &Value) {
    match (fixture, out) {
        (Value::Object(f), Value::Object(o)) => {
            for (k, v) in f {
                let o = o
                    .get(k)
                    .unwrap_or_else(|| panic!("{name}: field {at}.{k} is not part of the type"));
                assert_subset(name, &format!("{at}.{k}"), v, o);
            }
        }
        (Value::Array(f), Value::Array(o)) => {
            assert_eq!(f.len(), o.len(), "{name}: {at}");
            for (i, (f, o)) in f.iter().zip(o).enumerate() {
                assert_subset(name, &format!("{at}[{i}]"), f, o);
            }
        }
        (Value::String(f), Value::String(o)) if f != o => {
            // Timestamps may be re-serialised differently (offsets become Z).
            let ft = chrono::DateTime::parse_from_rfc3339(f);
            let ot = chrono::DateTime::parse_from_rfc3339(o);
            match (ft, ot) {
                (Ok(ft), Ok(ot)) if ft == ot => {}
                _ => panic!("{name}: {at}: {f:?} came back as {o:?}"),
            }
        }
        (Value::Number(f), Value::Number(o)) => {
            assert_eq!(f.as_f64(), o.as_f64(), "{name}: {at}");
        }
        (f, o) => assert_eq!(f, o, "{name}: {at}"),
    }
}

#[test]
fn pairing() {
    let req: PairRequest = round_trip("pair_request.json");
    assert_eq!(req.kind, DeviceKind::Nixos);
    let resp: PairResponse = round_trip("pair_response.json");
    assert!(is_well_formed_token(&resp.report_token));
    assert_eq!(resp.storage.port, STORAGE_BOX_SSH_PORT);
    assert_eq!(resp.storage.repo_path, DEVICE_REPO_PATH);

    let link = PairingLink::parse(&fixture("pairing_link.txt")).unwrap();
    assert_eq!(link.server, "https://replique.sigma-dev.eu");
    assert_eq!(
        normalize_pairing_code(&link.code),
        normalize_pairing_code(&req.code)
    );
    assert_eq!(link.to_uri(), fixture("pairing_link.txt").trim());
}

#[test]
fn jobs() {
    let backup: JobSpec = round_trip("job_spec_backup.json");
    assert_eq!(backup.kind, JobKind::Backup);
    assert_eq!(backup.rules.retention, Some(Retention::default()));

    let manual: JobSpec = round_trip("job_spec_manual.json");
    assert_eq!(manual.schedule, None);
    match manual.rules.retention {
        Some(Retention::Versions { keep }) => {
            assert_eq!(keep.monthly, Some(KeepCount::Unlimited));
            assert_eq!(
                keep.to_restic_args(),
                [
                    "--keep-daily",
                    "7",
                    "--keep-weekly",
                    "4",
                    "--keep-monthly",
                    "unlimited",
                    "--keep-within",
                    "14d"
                ]
            );
        }
        other => panic!("{other:?}"),
    }

    let mirror: JobSpec = round_trip("job_spec_mirror.json");
    assert_eq!(mirror.kind, JobKind::Mirror);
    assert!(mirror.rules.retention.is_none());

    let ack: Ack = round_trip("ack.json");
    assert_eq!(ack, Ack::OK);

    let sources: MirrorSources = round_trip("mirror_sources.json");
    assert_eq!(
        sources.sources[0].device_id,
        mirror.rules.mirror_source.unwrap()
    );
}

#[test]
fn device_config() {
    let config: DeviceConfig = round_trip("device_config.json");
    assert_eq!(config.retention, Retention::NeverPrune);
    assert_eq!(config.conditions.min_battery_percent, Some(30));
    assert_eq!(config.folders.len(), 3);
    // The default the server serves must survive the same trip.
    let default = DeviceConfig::android_default();
    let json = serde_json::to_string(&default).unwrap();
    assert_eq!(
        serde_json::from_str::<DeviceConfig>(&json).unwrap(),
        default
    );
    assert_eq!(config.excludes, default.excludes);
}

#[test]
fn runs() {
    let start: StartRun = round_trip("run_start.json");
    assert_eq!(start.kind, RunKind::Backup);
    let _: StartRunReply = round_trip("run_start_reply.json");
    let progress: Progress = round_trip("run_progress.json");
    assert_eq!(progress.seconds_remaining, Some(12153));
    let phase: Progress = round_trip("run_progress_phase.json");
    assert_eq!(phase.seconds_remaining, None);

    let ok: FinishRun = round_trip("run_finish_backup.json");
    assert_eq!(ok.status, RunStatus::from_restic_exit(ok.exit_code));
    let partial: FinishRun = round_trip("run_finish_partial.json");
    assert_eq!(partial.status, RunStatus::Partial);
    assert_eq!(partial.unreadable_paths.len(), 1);
    let failed: FinishRun = round_trip("run_finish_failed.json");
    assert!(failed.summary.is_none() && failed.message.is_some());
    let prune: FinishRun = round_trip("run_finish_prune.json");
    assert_eq!(prune.prune.unwrap().policy, "keep-deleted");
    let check: FinishRun = round_trip("run_finish_check.json");
    assert_eq!(check.check.unwrap().snapshot_ids.len(), 2);
    let mirror: FinishRun = round_trip("run_finish_mirror.json");
    assert!(mirror.mirror.unwrap().hash_mismatches.is_empty());
}

#[test]
fn safe_to_delete() {
    let some: SafeToDelete = round_trip("safe_to_delete.json");
    assert!(some.snapshot.is_some());
    let none: SafeToDelete = round_trip("safe_to_delete_none.json");
    assert!(none.snapshot.is_none());
}

#[test]
fn errors() {
    let e: ApiError = round_trip("error.json");
    assert_eq!(e.error, error_code::CODE_EXPIRED);
}

#[test]
fn unknown_fields_and_kinds_are_tolerated() {
    let json = r#"{"code":"X","device_name":"NAS","kind":"toaster","ssh_public_key":"k","from_the_future":1}"#;
    let req: PairRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.kind, DeviceKind::Other);
}

#[test]
fn restic_summary_parses_as_is() {
    // restic's own message, with its local-offset timestamps and message_type.
    let line = r#"{"message_type":"summary","files_new":2,"files_changed":0,"files_unmodified":0,"dirs_new":1,"dirs_changed":0,"dirs_unmodified":0,"data_blobs":4,"tree_blobs":2,"data_added":3001262,"data_added_packed":3001165,"total_files_processed":2,"total_bytes_processed":3000003,"total_duration":0.753420583,"backup_start":"2026-11-14T22:36:07.812187+01:00","backup_end":"2026-11-14T22:36:08.565599+01:00","snapshot_id":"60b9632cb99dc43c73727a1da5bb5c48f719c8942bfee76c11b0deadfa744b61"}"#;
    let s: BackupSummary = serde_json::from_str(line).unwrap();
    assert_eq!(s.data_added_packed, 3001165);
    assert_eq!(
        s.backup_start.unwrap().to_rfc3339(),
        "2026-11-14T21:36:07.812187+00:00"
    );
}
