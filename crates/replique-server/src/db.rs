//! SQLite persistence. Queries are tiny and few, so one connection behind a
//! mutex is plenty and keeps every call synchronous; nothing holds the lock
//! across an `.await`.
//!
//! Times are Unix seconds. Protocol messages (job rules, progress, finish
//! reports) are stored as the JSON they arrived as, so a field added to the
//! protocol later survives a round trip through an older server.

use std::path::Path;

use anyhow::Context;
use parking_lot::Mutex;
use replique_proto::{
    DeviceConfig, DeviceKind, FinishRun, JobKind, JobRules, JobSpec, Progress, RunKind, RunStatus,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::de::DeserializeOwned;

pub struct Store(Mutex<Connection>);

/// Each entry runs once, in order; `PRAGMA user_version` remembers how many
/// have. Never edit an entry that shipped: add another.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE owners (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL,
    quota_bytes INTEGER,
    created_at INTEGER NOT NULL
);
CREATE TABLE devices (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL REFERENCES owners(id),
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    kind TEXT NOT NULL,
    token_hash TEXT UNIQUE,
    ssh_public_key TEXT,
    client_version TEXT,
    created_at INTEGER NOT NULL,
    paired_at INTEGER,
    revoked_at INTEGER,
    last_seen_at INTEGER,
    UNIQUE (owner_id, slug)
);
CREATE TABLE repositories (
    id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL UNIQUE REFERENCES devices(id) ON DELETE CASCADE,
    host TEXT NOT NULL,
    port INTEGER NOT NULL,
    subaccount_id INTEGER,
    username TEXT NOT NULL,
    home_directory TEXT NOT NULL,
    twin_subaccount_id INTEGER,
    twin_username TEXT,
    repo_path TEXT NOT NULL,
    adopted INTEGER NOT NULL DEFAULT 0,
    device_key_installed INTEGER NOT NULL DEFAULT 0,
    twin_key_installed INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE pairing_codes (
    code TEXT PRIMARY KEY,
    device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER,
    secrets BLOB
);
CREATE TABLE jobs (
    id INTEGER PRIMARY KEY,
    device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    schedule TEXT,
    expected_interval_secs INTEGER,
    interval_override_secs INTEGER,
    rules TEXT NOT NULL,
    declared_at INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE (device_id, name)
);
CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    client_run_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    received_at INTEGER NOT NULL,
    finished_at INTEGER,
    progress TEXT,
    progress_at INTEGER,
    finish TEXT,
    exit_code INTEGER,
    restic_version TEXT,
    client_version TEXT,
    UNIQUE (device_id, client_run_id)
);
CREATE INDEX runs_job ON runs(job_id, started_at);
CREATE INDEX runs_status ON runs(status);
CREATE TABLE measurements (
    scope TEXT NOT NULL,
    resolution INTEGER NOT NULL,
    at INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    files INTEGER,
    source TEXT NOT NULL,
    PRIMARY KEY (scope, resolution, at)
);
CREATE TABLE alerts (
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL,
    kind TEXT NOT NULL,
    severity TEXT NOT NULL,
    title TEXT NOT NULL,
    detail TEXT NOT NULL,
    device_id TEXT,
    opened_at INTEGER NOT NULL,
    cleared_at INTEGER,
    notified INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX alerts_open ON alerts(key) WHERE cleared_at IS NULL;
CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    csrf TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value BLOB NOT NULL
);
CREATE TABLE device_configs (
    device_id TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    config TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE mirrors (
    device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    source_device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    assigned_at INTEGER NOT NULL,
    PRIMARY KEY (device_id, source_device_id)
);
CREATE TABLE host_keys (
    host TEXT NOT NULL,
    port INTEGER NOT NULL,
    key TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    PRIMARY KEY (host, port)
);
"#];

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn to_time(secs: i64) -> replique_proto::Timestamp {
    chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default()
}

/// Opaque ids with a readable prefix (`dev_…`, `run_…`): a sortable time part
/// and randomness, in Crockford base32 like a ULID.
pub fn new_id(prefix: &str) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let millis = chrono::Utc::now().timestamp_millis() as u128;
    let random: u128 = rand::rng().random::<u128>() >> 48;
    let mut value = (millis << 80) | random;
    let mut out = [0u8; 26];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(value & 31) as usize];
        value >>= 5;
    }
    format!("{prefix}_{}", std::str::from_utf8(&out).expect("ASCII"))
}

fn json<T: DeserializeOwned>(raw: Option<String>) -> Option<T> {
    raw.and_then(|r| serde_json::from_str(&r).ok())
}

fn enum_from<T: DeserializeOwned>(raw: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(raw.to_string())).ok()
}

fn enum_str<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

#[derive(Debug, Clone)]
pub struct Owner {
    pub id: String,
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub owner_id: String,
    pub name: String,
    pub slug: String,
    pub kind: DeviceKind,
    pub has_token: bool,
    pub ssh_public_key: Option<String>,
    pub client_version: Option<String>,
    pub created_at: i64,
    pub paired_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub last_seen_at: Option<i64>,
}

const DEVICE_COLUMNS: &str = "id, owner_id, name, slug, kind, token_hash IS NOT NULL, ssh_public_key, \
     client_version, created_at, paired_at, revoked_at, last_seen_at";

fn device_row(r: &Row) -> rusqlite::Result<Device> {
    let kind: String = r.get(4)?;
    Ok(Device {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        name: r.get(2)?,
        slug: r.get(3)?,
        kind: enum_from(&kind).unwrap_or(DeviceKind::Other),
        has_token: r.get(5)?,
        ssh_public_key: r.get(6)?,
        client_version: r.get(7)?,
        created_at: r.get(8)?,
        paired_at: r.get(9)?,
        revoked_at: r.get(10)?,
        last_seen_at: r.get(11)?,
    })
}

#[derive(Debug, Clone)]
pub struct Repository {
    pub id: String,
    pub device_id: String,
    pub host: String,
    pub port: u16,
    pub subaccount_id: Option<i64>,
    pub username: String,
    pub home_directory: String,
    pub twin_subaccount_id: Option<i64>,
    pub twin_username: Option<String>,
    pub repo_path: String,
    pub adopted: bool,
    pub device_key_installed: bool,
    pub twin_key_installed: bool,
    pub created_at: i64,
}

const REPO_COLUMNS: &str = "id, device_id, host, port, subaccount_id, username, home_directory, \
     twin_subaccount_id, twin_username, repo_path, adopted, device_key_installed, \
     twin_key_installed, created_at";

fn repo_row(r: &Row) -> rusqlite::Result<Repository> {
    Ok(Repository {
        id: r.get(0)?,
        device_id: r.get(1)?,
        host: r.get(2)?,
        port: r.get(3)?,
        subaccount_id: r.get(4)?,
        username: r.get(5)?,
        home_directory: r.get(6)?,
        twin_subaccount_id: r.get(7)?,
        twin_username: r.get(8)?,
        repo_path: r.get(9)?,
        adopted: r.get(10)?,
        device_key_installed: r.get(11)?,
        twin_key_installed: r.get(12)?,
        created_at: r.get(13)?,
    })
}

#[derive(Debug, Clone)]
pub struct PairingCode {
    pub code: String,
    pub device_id: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub used_at: Option<i64>,
    pub secrets: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub device_id: String,
    pub name: String,
    pub kind: JobKind,
    pub schedule: Option<String>,
    pub expected_interval_secs: Option<i64>,
    pub interval_override_secs: Option<i64>,
    pub rules: JobRules,
    pub declared_at: Option<i64>,
    pub created_at: i64,
}

const JOB_COLUMNS: &str = "id, device_id, name, kind, schedule, expected_interval_secs, \
     interval_override_secs, rules, declared_at, created_at";

fn job_row(r: &Row) -> rusqlite::Result<Job> {
    let kind: String = r.get(3)?;
    Ok(Job {
        id: r.get(0)?,
        device_id: r.get(1)?,
        name: r.get(2)?,
        kind: enum_from(&kind).unwrap_or(JobKind::Backup),
        schedule: r.get(4)?,
        expected_interval_secs: r.get(5)?,
        interval_override_secs: r.get(6)?,
        rules: json(r.get(7)?).unwrap_or_default(),
        declared_at: r.get(8)?,
        created_at: r.get(9)?,
    })
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub device_id: String,
    pub job_id: i64,
    pub client_run_id: String,
    pub kind: RunKind,
    pub status: RunStatus,
    pub started_at: i64,
    pub received_at: i64,
    pub finished_at: Option<i64>,
    pub progress: Option<Progress>,
    pub progress_at: Option<i64>,
    pub finish: Option<FinishRun>,
    /// The finish report exactly as stored, to tell a replay from a conflict.
    pub finish_raw: Option<String>,
    pub exit_code: Option<i32>,
    pub restic_version: Option<String>,
    pub client_version: Option<String>,
}

const RUN_COLUMNS: &str = "id, device_id, job_id, client_run_id, kind, status, started_at, \
     received_at, finished_at, progress, progress_at, finish, exit_code, restic_version, \
     client_version";

fn run_row(r: &Row) -> rusqlite::Result<Run> {
    let kind: String = r.get(4)?;
    let status: String = r.get(5)?;
    let finish_raw: Option<String> = r.get(11)?;
    Ok(Run {
        id: r.get(0)?,
        device_id: r.get(1)?,
        job_id: r.get(2)?,
        client_run_id: r.get(3)?,
        kind: enum_from(&kind).unwrap_or(RunKind::Backup),
        status: enum_from(&status).unwrap_or(RunStatus::Failed),
        started_at: r.get(6)?,
        received_at: r.get(7)?,
        finished_at: r.get(8)?,
        progress: json(r.get(9)?),
        progress_at: r.get(10)?,
        finish: json(finish_raw.clone()),
        finish_raw,
        exit_code: r.get(12)?,
        restic_version: r.get(13)?,
        client_version: r.get(14)?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Point {
    pub at: i64,
    pub bytes: i64,
    pub files: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub id: i64,
    pub key: String,
    pub kind: String,
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub device_id: Option<String>,
    pub opened_at: i64,
    pub cleared_at: Option<i64>,
}

const ALERT_COLUMNS: &str =
    "id, key, kind, severity, title, detail, device_id, opened_at, cleared_at";

fn alert_row(r: &Row) -> rusqlite::Result<Alert> {
    Ok(Alert {
        id: r.get(0)?,
        key: r.get(1)?,
        kind: r.get(2)?,
        severity: r.get(3)?,
        title: r.get(4)?,
        detail: r.get(5)?,
        device_id: r.get(6)?,
        opened_at: r.get(7)?,
        cleared_at: r.get(8)?,
    })
}

#[derive(Debug, Clone)]
pub struct Session {
    pub token_hash: String,
    pub csrf: String,
    pub last_seen_at: i64,
    pub expires_at: i64,
}

/// Measurement resolutions, in seconds, and how long each is kept: 5-minute
/// points for 7 days, hourly for 90 days, daily forever.
pub const RESOLUTIONS: [(i64, Option<i64>); 3] = [
    (300, Some(7 * 86_400)),
    (3_600, Some(90 * 86_400)),
    (86_400, None),
];

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Store::init(conn)
    }

    pub fn open_in_memory() -> anyhow::Result<Store> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Store> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(migration)
                .with_context(|| format!("database migration {}", i + 1))?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
            tx.commit()?;
        }
        Ok(Store(Mutex::new(conn)))
    }

    // ------------------------------------------------------------ owners

    /// The v1 admin owner, created on first start.
    pub fn ensure_admin_owner(&self, name: &str) -> anyhow::Result<Owner> {
        let conn = self.0.lock();
        let existing = conn
            .query_row(
                "SELECT id, name, slug FROM owners WHERE role = 'admin' ORDER BY created_at LIMIT 1",
                [],
                |r| {
                    Ok(Owner {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        slug: r.get(2)?,
                    })
                },
            )
            .optional()?;
        if let Some(owner) = existing {
            return Ok(owner);
        }
        let owner = Owner {
            id: new_id("own"),
            name: name.to_string(),
            slug: replique_proto::slugify(name),
        };
        conn.execute(
            "INSERT INTO owners (id, name, slug, role, created_at) VALUES (?1, ?2, ?3, 'admin', ?4)",
            params![owner.id, owner.name, owner.slug, now()],
        )?;
        Ok(owner)
    }

    // ------------------------------------------------------------ devices

    pub fn devices(&self) -> anyhow::Result<Vec<Device>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DEVICE_COLUMNS} FROM devices ORDER BY created_at, name"
        ))?;
        let rows = stmt.query_map([], device_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn device(&self, id: &str) -> anyhow::Result<Option<Device>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                &format!("SELECT {DEVICE_COLUMNS} FROM devices WHERE id = ?1"),
                [id],
                device_row,
            )
            .optional()?)
    }

    /// The device a report token belongs to, unless revoked.
    pub fn device_by_token_hash(&self, hash: &str) -> anyhow::Result<Option<Device>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {DEVICE_COLUMNS} FROM devices WHERE token_hash = ?1 AND revoked_at IS NULL"
                ),
                [hash],
                device_row,
            )
            .optional()?)
    }

    pub fn slug_taken(&self, owner_id: &str, slug: &str) -> anyhow::Result<bool> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                "SELECT 1 FROM devices WHERE owner_id = ?1 AND slug = ?2",
                params![owner_id, slug],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn insert_device(&self, d: &Device, repo: &Repository) -> anyhow::Result<()> {
        let mut conn = self.0.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO devices (id, owner_id, name, slug, kind, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![d.id, d.owner_id, d.name, d.slug, d.kind.as_str(), d.created_at],
        )?;
        tx.execute(
            &format!("INSERT INTO repositories ({REPO_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"),
            params![
                repo.id,
                repo.device_id,
                repo.host,
                repo.port,
                repo.subaccount_id,
                repo.username,
                repo.home_directory,
                repo.twin_subaccount_id,
                repo.twin_username,
                repo.repo_path,
                repo.adopted,
                repo.device_key_installed,
                repo.twin_key_installed,
                repo.created_at
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_device(&self, id: &str) -> anyhow::Result<()> {
        let mut conn = self.0.lock();
        let tx = conn.transaction()?;
        let repo: Option<String> = tx
            .query_row(
                "SELECT id FROM repositories WHERE device_id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(repo) = repo {
            tx.execute(
                "DELETE FROM measurements WHERE scope = ?1",
                [format!("repo:{repo}")],
            )?;
        }
        tx.execute("DELETE FROM devices WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_device_token(
        &self,
        id: &str,
        token_hash: &str,
        ssh_public_key: &str,
        client_version: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE devices SET token_hash = ?2, ssh_public_key = ?3, client_version = ?4, \
             paired_at = ?5, revoked_at = NULL WHERE id = ?1",
            params![id, token_hash, ssh_public_key, client_version, now()],
        )?;
        Ok(())
    }

    pub fn revoke_device_token(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE devices SET token_hash = NULL, revoked_at = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn touch_device(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE devices SET last_seen_at = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ repositories

    pub fn repositories(&self) -> anyhow::Result<Vec<Repository>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {REPO_COLUMNS} FROM repositories ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([], repo_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn repository_of(&self, device_id: &str) -> anyhow::Result<Option<Repository>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                &format!("SELECT {REPO_COLUMNS} FROM repositories WHERE device_id = ?1"),
                [device_id],
                repo_row,
            )
            .optional()?)
    }

    pub fn repository(&self, id: &str) -> anyhow::Result<Option<Repository>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                &format!("SELECT {REPO_COLUMNS} FROM repositories WHERE id = ?1"),
                [id],
                repo_row,
            )
            .optional()?)
    }

    pub fn set_keys_installed(
        &self,
        repo_id: &str,
        device: bool,
        twin: bool,
    ) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE repositories SET device_key_installed = ?2, twin_key_installed = ?3 WHERE id = ?1",
            params![repo_id, device, twin],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ pairing

    pub fn insert_pairing_code(&self, p: &PairingCode) -> anyhow::Result<()> {
        let conn = self.0.lock();
        // One live code per device: a new one replaces the others.
        conn.execute(
            "DELETE FROM pairing_codes WHERE device_id = ?1 AND used_at IS NULL",
            [&p.device_id],
        )?;
        conn.execute(
            "INSERT INTO pairing_codes (code, device_id, created_at, expires_at, used_at, secrets) VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![p.code, p.device_id, p.created_at, p.expires_at, p.secrets],
        )?;
        Ok(())
    }

    fn pairing_row(r: &Row) -> rusqlite::Result<PairingCode> {
        Ok(PairingCode {
            code: r.get(0)?,
            device_id: r.get(1)?,
            created_at: r.get(2)?,
            expires_at: r.get(3)?,
            used_at: r.get(4)?,
            secrets: r.get(5)?,
        })
    }

    /// `code` in canonical form (no dash, upper case).
    pub fn pairing_code(&self, code: &str) -> anyhow::Result<Option<PairingCode>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                "SELECT code, device_id, created_at, expires_at, used_at, secrets FROM pairing_codes WHERE code = ?1",
                [code],
                Store::pairing_row,
            )
            .optional()?)
    }

    pub fn live_pairing_code(&self, device_id: &str) -> anyhow::Result<Option<PairingCode>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                "SELECT code, device_id, created_at, expires_at, used_at, secrets FROM pairing_codes \
                 WHERE device_id = ?1 AND used_at IS NULL AND expires_at > ?2 ORDER BY created_at DESC LIMIT 1",
                params![device_id, now()],
                Store::pairing_row,
            )
            .optional()?)
    }

    /// The pairing secrets of a device paired in `manual` mode, kept until the
    /// admin confirms the key is installed.
    pub fn manual_pairing_secrets(&self, device_id: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                "SELECT secrets FROM pairing_codes WHERE device_id = ?1 AND secrets IS NOT NULL \
                 ORDER BY created_at DESC LIMIT 1",
                [device_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn finish_pairing(
        &self,
        code: &str,
        keep_secrets: bool,
        device_id: &str,
        token_hash: &str,
        ssh_public_key: &str,
        client_version: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut conn = self.0.lock();
        let tx = conn.transaction()?;
        let t = now();
        if keep_secrets {
            tx.execute(
                "UPDATE pairing_codes SET used_at = ?2 WHERE code = ?1",
                params![code, t],
            )?;
        } else {
            tx.execute(
                "UPDATE pairing_codes SET used_at = ?2, secrets = NULL WHERE code = ?1",
                params![code, t],
            )?;
        }
        tx.execute(
            "UPDATE devices SET token_hash = ?2, ssh_public_key = ?3, client_version = ?4, \
             paired_at = ?5, revoked_at = NULL, last_seen_at = ?5 WHERE id = ?1",
            params![device_id, token_hash, ssh_public_key, client_version, t],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn forget_pairing_secrets(&self, device_id: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE pairing_codes SET secrets = NULL WHERE device_id = ?1",
            [device_id],
        )?;
        Ok(())
    }

    pub fn prune_pairing_codes(&self) -> anyhow::Result<()> {
        let conn = self.0.lock();
        // Expired unused codes go, with the passwords they held. Codes used in
        // manual mode keep their secrets until the admin finishes.
        conn.execute(
            "DELETE FROM pairing_codes WHERE used_at IS NULL AND expires_at < ?1",
            [now() - 86_400],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ jobs

    pub fn jobs_of(&self, device_id: &str) -> anyhow::Result<Vec<Job>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE device_id = ?1 ORDER BY name"
        ))?;
        let rows = stmt.query_map([device_id], job_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn all_jobs(&self) -> anyhow::Result<Vec<Job>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY name"))?;
        let rows = stmt.query_map([], job_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn job(&self, id: i64) -> anyhow::Result<Option<Job>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id],
                job_row,
            )
            .optional()?)
    }

    /// Stores a job declaration; returns whether it changed anything.
    pub fn declare_job(&self, device_id: &str, name: &str, spec: &JobSpec) -> anyhow::Result<bool> {
        let conn = self.0.lock();
        let rules = serde_json::to_string(&spec.rules)?;
        let kind = enum_str(&spec.kind);
        let t = now();
        let before: Option<(String, Option<String>, Option<i64>, String)> = conn
            .query_row(
                "SELECT kind, schedule, expected_interval_secs, rules FROM jobs WHERE device_id = ?1 AND name = ?2",
                params![device_id, name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let expected = spec.expected_interval_secs.map(|s| s as i64);
        let changed = before.as_ref()
            != Some(&(kind.clone(), spec.schedule.clone(), expected, rules.clone()));
        conn.execute(
            "INSERT INTO jobs (device_id, name, kind, schedule, expected_interval_secs, rules, declared_at, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
             ON CONFLICT (device_id, name) DO UPDATE SET kind = ?3, schedule = ?4, \
             expected_interval_secs = ?5, rules = ?6, declared_at = ?7",
            params![device_id, name, kind, spec.schedule, expected, rules, t],
        )?;
        Ok(changed)
    }

    /// The job a run names, created with empty rules if it was never declared.
    pub fn job_for_run(&self, device_id: &str, name: &str, kind: RunKind) -> anyhow::Result<i64> {
        let conn = self.0.lock();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM jobs WHERE device_id = ?1 AND name = ?2",
                params![device_id, name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(id);
        }
        let job_kind = if kind == RunKind::Mirror {
            "mirror"
        } else {
            "backup"
        };
        conn.execute(
            "INSERT INTO jobs (device_id, name, kind, rules, created_at) VALUES (?1, ?2, ?3, '{}', ?4)",
            params![device_id, name, job_kind, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn set_job_interval_override(&self, job_id: i64, secs: Option<i64>) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE jobs SET interval_override_secs = ?2 WHERE id = ?1",
            params![job_id, secs],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ runs

    /// Inserts a run, or returns the existing one with the same client run id.
    pub fn start_run(&self, run: &Run) -> anyhow::Result<(Run, bool)> {
        let conn = self.0.lock();
        let existing = conn
            .query_row(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM runs WHERE device_id = ?1 AND client_run_id = ?2"
                ),
                params![run.device_id, run.client_run_id],
                run_row,
            )
            .optional()?;
        if let Some(existing) = existing {
            return Ok((existing, false));
        }
        conn.execute(
            "INSERT INTO runs (id, device_id, job_id, client_run_id, kind, status, started_at, received_at, restic_version, client_version) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                run.id,
                run.device_id,
                run.job_id,
                run.client_run_id,
                enum_str(&run.kind),
                run.status.as_str(),
                run.started_at,
                run.received_at,
                run.restic_version,
                run.client_version
            ],
        )?;
        Ok((run.clone(), true))
    }

    pub fn run(&self, id: &str) -> anyhow::Result<Option<Run>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
                [id],
                run_row,
            )
            .optional()?)
    }

    pub fn set_progress(&self, id: &str, progress: &Progress) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE runs SET progress = ?2, progress_at = ?3 WHERE id = ?1 AND status = 'running'",
            params![id, serde_json::to_string(progress)?, now()],
        )?;
        Ok(())
    }

    pub fn finish_run(&self, id: &str, finish: &FinishRun, raw: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE runs SET status = ?2, finished_at = ?3, finish = ?4, exit_code = ?5 WHERE id = ?1",
            params![
                id,
                finish.status.as_str(),
                finish.finished_at.timestamp(),
                raw,
                finish.exit_code
            ],
        )?;
        Ok(())
    }

    pub fn mark_run_lost(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE runs SET status = 'lost', finished_at = ?2 WHERE id = ?1 AND status = 'running'",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn runs_of_job(&self, job_id: i64, limit: usize) -> anyhow::Result<Vec<Run>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE job_id = ?1 ORDER BY started_at DESC, received_at DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![job_id, limit as i64], run_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn runs_of_device(&self, device_id: &str, limit: usize) -> anyhow::Result<Vec<Run>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE device_id = ?1 ORDER BY started_at DESC, received_at DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![device_id, limit as i64], run_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn running_runs(&self) -> anyhow::Result<Vec<Run>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE status = 'running'"
        ))?;
        let rows = stmt.query_map([], run_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Successful runs of one kind for a device, newest first.
    pub fn successful_runs(&self, device_id: &str, kind: RunKind) -> anyhow::Result<Vec<Run>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE device_id = ?1 AND kind = ?2 AND status = 'success' \
             ORDER BY finished_at DESC LIMIT 200"
        ))?;
        let rows = stmt.query_map(params![device_id, enum_str(&kind)], run_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Successful mirror runs (by any device) that copied `source_device_id`.
    pub fn mirror_runs_of(&self, source_device_id: &str) -> anyhow::Result<Vec<(Run, String)>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {}, d.name FROM runs r JOIN devices d ON d.id = r.device_id \
             WHERE r.kind = 'mirror' AND r.status = 'success' AND r.finish IS NOT NULL \
             ORDER BY r.finished_at DESC LIMIT 200",
            RUN_COLUMNS
                .split(", ")
                .map(|c| format!("r.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))?;
        let rows = stmt.query_map([], |r| Ok((run_row(r)?, r.get::<_, String>(15)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (run, name) = row?;
            let copies_source = run
                .finish
                .as_ref()
                .and_then(|f| f.mirror.as_ref())
                .is_some_and(|m| m.source_device_id == source_device_id);
            if copies_source {
                out.push((run, name));
            }
        }
        Ok(out)
    }

    /// Whether any run of the device was running, or finished, in `(since, until]`.
    pub fn device_active_between(
        &self,
        device_id: &str,
        since: i64,
        until: i64,
    ) -> anyhow::Result<bool> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                "SELECT 1 FROM runs WHERE device_id = ?1 AND kind != 'mirror' AND started_at <= ?3 \
                 AND (status = 'running' OR finished_at > ?2 OR received_at > ?2) LIMIT 1",
                params![device_id, since, until],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    // ------------------------------------------------------------ measurements

    /// Records a measurement at every resolution: the 5-minute point as is, and
    /// the hourly and daily buckets it falls in take its value (sizes are a
    /// level, not a rate, so the last value of a bucket is the right summary).
    /// Points past their resolution's lifetime are dropped.
    pub fn record_measurement(
        &self,
        scope: &str,
        at: i64,
        bytes: i64,
        files: Option<i64>,
        source: &str,
    ) -> anyhow::Result<()> {
        let mut conn = self.0.lock();
        let tx = conn.transaction()?;
        for (resolution, keep) in RESOLUTIONS {
            // The finest points keep their exact time: the drift check compares
            // it with when runs finished.
            let bucket = if resolution == RESOLUTIONS[0].0 {
                at
            } else {
                at - at.rem_euclid(resolution)
            };
            tx.execute(
                "INSERT INTO measurements (scope, resolution, at, bytes, files, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (scope, resolution, at) DO UPDATE SET bytes = ?4, files = ?5, source = ?6",
                params![scope, resolution, bucket, bytes, files, source],
            )?;
            if let Some(keep) = keep {
                tx.execute(
                    "DELETE FROM measurements WHERE scope = ?1 AND resolution = ?2 AND at < ?3",
                    params![scope, resolution, at - keep],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn latest_measurement(&self, scope: &str) -> anyhow::Result<Option<Point>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                "SELECT at, bytes, files FROM measurements WHERE scope = ?1 AND resolution = 300 \
                 ORDER BY at DESC LIMIT 1",
                [scope],
                |r| {
                    Ok(Point {
                        at: r.get(0)?,
                        bytes: r.get(1)?,
                        files: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Points of a scope since `since`, at the finest resolution that still
    /// covers that far back.
    pub fn series(&self, scope: &str, since: i64) -> anyhow::Result<Vec<Point>> {
        let age = now() - since;
        let resolution = RESOLUTIONS
            .iter()
            .find(|(_, keep)| keep.is_none_or(|k| age <= k))
            .map(|(r, _)| *r)
            .unwrap_or(86_400);
        let conn = self.0.lock();
        let mut stmt = conn.prepare(
            "SELECT at, bytes, files FROM measurements WHERE scope = ?1 AND resolution = ?2 AND at >= ?3 ORDER BY at",
        )?;
        let rows = stmt.query_map(params![scope, resolution, since - resolution], |r| {
            Ok(Point {
                at: r.get(0)?,
                bytes: r.get(1)?,
                files: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ------------------------------------------------------------ alerts

    pub fn open_alert(&self, key: &str) -> anyhow::Result<Option<Alert>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {ALERT_COLUMNS} FROM alerts WHERE key = ?1 AND cleared_at IS NULL"
                ),
                [key],
                alert_row,
            )
            .optional()?)
    }

    /// Opens an alert unless one with this key is open; returns it if new.
    pub fn insert_alert(
        &self,
        key: &str,
        kind: &str,
        severity: &str,
        title: &str,
        detail: &str,
        device_id: Option<&str>,
    ) -> anyhow::Result<Option<Alert>> {
        let conn = self.0.lock();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO alerts (key, kind, severity, title, detail, device_id, opened_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![key, kind, severity, title, detail, device_id, now()],
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        let id = conn.last_insert_rowid();
        Ok(Some(conn.query_row(
            &format!("SELECT {ALERT_COLUMNS} FROM alerts WHERE id = ?1"),
            [id],
            alert_row,
        )?))
    }

    pub fn clear_alert(&self, key: &str) -> anyhow::Result<Option<Alert>> {
        let conn = self.0.lock();
        let open = conn
            .query_row(
                &format!(
                    "SELECT {ALERT_COLUMNS} FROM alerts WHERE key = ?1 AND cleared_at IS NULL"
                ),
                [key],
                alert_row,
            )
            .optional()?;
        if let Some(mut alert) = open {
            let t = now();
            conn.execute(
                "UPDATE alerts SET cleared_at = ?2 WHERE id = ?1",
                params![alert.id, t],
            )?;
            alert.cleared_at = Some(t);
            return Ok(Some(alert));
        }
        Ok(None)
    }

    pub fn open_alerts(&self) -> anyhow::Result<Vec<Alert>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ALERT_COLUMNS} FROM alerts WHERE cleared_at IS NULL ORDER BY severity = 'alert' DESC, opened_at DESC"
        ))?;
        let rows = stmt.query_map([], alert_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn recent_cleared_alerts(&self, limit: usize) -> anyhow::Result<Vec<Alert>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ALERT_COLUMNS} FROM alerts WHERE cleared_at IS NOT NULL ORDER BY cleared_at DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], alert_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn alert_by_id(&self, id: i64) -> anyhow::Result<Option<Alert>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                &format!("SELECT {ALERT_COLUMNS} FROM alerts WHERE id = ?1"),
                [id],
                alert_row,
            )
            .optional()?)
    }

    pub fn clear_alerts_of_device(&self, device_id: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE alerts SET cleared_at = ?2 WHERE device_id = ?1 AND cleared_at IS NULL",
            params![device_id, now()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ sessions

    pub fn insert_session(&self, s: &Session) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "INSERT INTO sessions (token_hash, csrf, created_at, last_seen_at, expires_at) VALUES (?1, ?2, ?3, ?3, ?4)",
            params![s.token_hash, s.csrf, s.last_seen_at, s.expires_at],
        )?;
        Ok(())
    }

    pub fn session(&self, token_hash: &str) -> anyhow::Result<Option<Session>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                "SELECT token_hash, csrf, last_seen_at, expires_at FROM sessions WHERE token_hash = ?1 AND expires_at > ?2",
                params![token_hash, now()],
                |r| {
                    Ok(Session {
                        token_hash: r.get(0)?,
                        csrf: r.get(1)?,
                        last_seen_at: r.get(2)?,
                        expires_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn touch_session(&self, token_hash: &str, expires_at: i64) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE sessions SET last_seen_at = ?2, expires_at = ?3 WHERE token_hash = ?1",
            params![token_hash, now(), expires_at],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, token_hash: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
        Ok(())
    }

    /// Removes expired sessions and returns their hashes.
    pub fn prune_sessions(&self) -> anyhow::Result<Vec<String>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare("SELECT token_hash FROM sessions WHERE expires_at <= ?1")?;
        let gone: Vec<String> = stmt
            .query_map([now()], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now()])?;
        Ok(gone)
    }

    // ------------------------------------------------------------ settings

    pub fn setting(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn setting_str(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self.setting(key)?.and_then(|v| String::from_utf8(v).ok()))
    }

    pub fn put_setting(&self, key: &str, value: &[u8]) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_setting(&self, key: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    }

    // ------------------------------------------------------------ device config

    pub fn device_config(&self, device_id: &str) -> anyhow::Result<Option<(DeviceConfig, i64)>> {
        let conn = self.0.lock();
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT config, updated_at FROM device_configs WHERE device_id = ?1",
                [device_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(raw, at)| serde_json::from_str(&raw).ok().map(|c| (c, at))))
    }

    pub fn put_device_config(&self, device_id: &str, config: &DeviceConfig) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "INSERT INTO device_configs (device_id, config, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (device_id) DO UPDATE SET config = ?2, updated_at = ?3",
            params![device_id, serde_json::to_string(config)?, now()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ mirrors

    pub fn mirror_sources_of(&self, device_id: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(
            "SELECT source_device_id FROM mirrors WHERE device_id = ?1 ORDER BY assigned_at",
        )?;
        let rows = stmt.query_map([device_id], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mirrors_of_source(&self, source_device_id: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare("SELECT device_id FROM mirrors WHERE source_device_id = ?1")?;
        let rows = stmt.query_map([source_device_id], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn add_mirror(&self, device_id: &str, source_device_id: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "INSERT OR IGNORE INTO mirrors (device_id, source_device_id, assigned_at) VALUES (?1, ?2, ?3)",
            params![device_id, source_device_id, now()],
        )?;
        Ok(())
    }

    pub fn remove_mirror(&self, device_id: &str, source_device_id: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "DELETE FROM mirrors WHERE device_id = ?1 AND source_device_id = ?2",
            params![device_id, source_device_id],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ host keys

    pub fn host_key(&self, host: &str, port: u16) -> anyhow::Result<Option<String>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                "SELECT key FROM host_keys WHERE host = ?1 AND port = ?2",
                params![host, port],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn remember_host_key(&self, host: &str, port: u16, key: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "INSERT OR IGNORE INTO host_keys (host, port, key, first_seen) VALUES (?1, ?2, ?3, ?4)",
            params![host, port, key, now()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_sortable() {
        let a = new_id("dev");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id("dev");
        assert!(a.starts_with("dev_") && a.len() == 30);
        assert!(a < b);
    }

    #[test]
    fn measurements_roll_up() {
        let store = Store::open_in_memory().unwrap();
        let base = now() - now().rem_euclid(86_400);
        store
            .record_measurement("box", base + 10, 100, None, "hetzner-api")
            .unwrap();
        store
            .record_measurement("box", base + 400, 200, None, "hetzner-api")
            .unwrap();
        store
            .record_measurement("box", base + 4000, 300, Some(3), "sftp")
            .unwrap();
        let conn = store.0.lock();
        let count = |res: i64| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM measurements WHERE resolution = ?1",
                [res],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count(300), 3);
        assert_eq!(count(3600), 2);
        assert_eq!(count(86_400), 1);
        let daily: i64 = conn
            .query_row(
                "SELECT bytes FROM measurements WHERE resolution = 86400",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(daily, 300);
        drop(conn);
        // Old 5-minute points are dropped once a newer one arrives.
        store
            .record_measurement("box", base - 8 * 86_400, 1, None, "x")
            .unwrap();
        store
            .record_measurement("box", base + 4300, 301, None, "x")
            .unwrap();
        let conn = store.0.lock();
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM measurements WHERE resolution = 300 AND at < ?1",
                [base],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, 0);
    }

    #[test]
    fn alerts_do_not_repeat() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            store
                .insert_alert("k", "run_failed", "alert", "t", "d", None)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .insert_alert("k", "run_failed", "alert", "t", "d", None)
                .unwrap()
                .is_none()
        );
        assert!(store.clear_alert("k").unwrap().is_some());
        assert!(store.clear_alert("k").unwrap().is_none());
        assert!(
            store
                .insert_alert("k", "run_failed", "alert", "t", "d", None)
                .unwrap()
                .is_some()
        );
    }
}
