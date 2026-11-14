//! Website restores: browsing a snapshot and downloading from it once the
//! admin typed the repository's password.
//!
//! The password is held in memory only, under the session that typed it, and
//! forgotten after `idle_timeout_secs` without use or at logout. It reaches
//! restic through its standard input (restic reads the password there when
//! stdin is not a terminal), never through arguments, which any user can see
//! in `ps`, nor the environment. restic always runs with `--no-lock` through
//! the repository's read-only login: a restore must never block a device's
//! backup, and that login could not write a lock anyway.

use std::collections::HashMap;
use std::ffi::OsString;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdout, Command};

use crate::App;
use crate::secrets::Secret;
use crate::storage::Access;

struct Unlocked {
    password: Secret,
    last_used: Instant,
}

/// Session → repository → password.
#[derive(Default)]
pub struct Unlocks(Mutex<HashMap<String, HashMap<String, Unlocked>>>);

impl Unlocks {
    pub fn put(&self, session: &str, repo: &str, password: Secret) {
        self.0
            .lock()
            .entry(session.to_string())
            .or_default()
            .insert(
                repo.to_string(),
                Unlocked {
                    password,
                    last_used: Instant::now(),
                },
            );
    }

    /// The password, if unlocked and used recently; using it keeps it alive.
    pub fn get(&self, session: &str, repo: &str, idle: Duration) -> Option<Secret> {
        let mut all = self.0.lock();
        let repos = all.get_mut(session)?;
        let expired = repos
            .get(repo)
            .is_some_and(|u| u.last_used.elapsed() > idle);
        if expired {
            repos.remove(repo);
            return None;
        }
        let unlocked = repos.get_mut(repo)?;
        unlocked.last_used = Instant::now();
        Some(unlocked.password.clone())
    }

    pub fn is_unlocked(&self, session: &str, repo: &str, idle: Duration) -> bool {
        self.0
            .lock()
            .get(session)
            .and_then(|r| r.get(repo))
            .is_some_and(|u| u.last_used.elapsed() <= idle)
    }

    pub fn forget(&self, session: &str, repo: &str) {
        if let Some(repos) = self.0.lock().get_mut(session) {
            repos.remove(repo);
        }
    }

    pub fn forget_session(&self, session: &str) {
        self.0.lock().remove(session);
    }

    pub fn forget_repo(&self, repo: &str) {
        for repos in self.0.lock().values_mut() {
            repos.remove(repo);
        }
    }

    /// Drops every password idle for longer than `idle`.
    pub fn sweep(&self, idle: Duration) {
        let mut all = self.0.lock();
        for repos in all.values_mut() {
            repos.retain(|_, u| u.last_used.elapsed() <= idle);
        }
        all.retain(|_, repos| !repos.is_empty());
    }

    pub fn count(&self) -> usize {
        self.0.lock().values().map(|r| r.len()).sum()
    }
}

pub async fn sweep_loop(app: std::sync::Arc<App>) {
    let idle = Duration::from_secs(app.config.restore.idle_timeout_secs);
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        app.unlocks.sweep(idle);
        if let Ok(gone) = app.store.prune_sessions() {
            for session in gone {
                app.unlocks.forget_session(&session);
            }
        }
        let _ = app.store.prune_pairing_codes();
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    pub id: String,
    #[serde(default)]
    pub short_id: String,
    pub time: chrono::DateTime<chrono::FixedOffset>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Node {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub mtime: Option<chrono::DateTime<chrono::FixedOffset>>,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

/// restic failed: its exit code and what it said, password scrubbed.
#[derive(Debug)]
pub struct ResticError {
    pub code: Option<i32>,
    pub stderr: String,
}

impl std::fmt::Display for ResticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(12) => f.write_str("wrong password"),
            Some(10) => f.write_str("no repository there yet"),
            code => write!(
                f,
                "restic failed (exit {}): {}",
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                self.stderr.lines().last().unwrap_or("")
            ),
        }
    }
}

impl std::error::Error for ResticError {}

/// Snapshot ids are hex (full or short); `latest` is restic's own.
pub fn valid_snapshot_id(id: &str) -> bool {
    id == "latest"
        || (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Paths inside a snapshot are absolute; anything else could read as an option.
pub fn valid_path(path: &str) -> bool {
    path.starts_with('/') && !path.contains('\0') && !path.split('/').any(|p| p == "..")
}

pub struct Restic<'a> {
    app: &'a App,
    repo_args: Vec<OsString>,
    password: Secret,
}

impl<'a> Restic<'a> {
    pub fn new(app: &'a App, access: &Access, password: Secret) -> anyhow::Result<Restic<'a>> {
        Ok(Restic {
            app,
            repo_args: app.restic_repo_args(access)?,
            password,
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.app.config.restore.restic);
        cmd.args(&self.repo_args)
            .arg("--no-lock")
            .arg("--cache-dir")
            .arg(self.app.config.state_dir.join("restic-cache"))
            .args(args)
            // Only stdin may carry the password: nothing inherited may override it.
            .env_remove("RESTIC_PASSWORD")
            .env_remove("RESTIC_PASSWORD_FILE")
            .env_remove("RESTIC_PASSWORD_COMMAND")
            .env_remove("RESTIC_REPOSITORY")
            .env_remove("RESTIC_REPOSITORY_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    async fn spawn(&self, args: &[&str]) -> anyhow::Result<Child> {
        let mut child = self
            .command(args)
            .spawn()
            .with_context(|| format!("running {}", self.app.config.restore.restic.display()))?;
        let mut stdin = child.stdin.take().expect("piped");
        let line = zeroize::Zeroizing::new(format!("{}\n", self.password.expose()));
        stdin.write_all(line.as_bytes()).await?;
        drop(stdin);
        Ok(child)
    }

    /// Removes the password from anything restic printed, in case it ever
    /// echoes it (it does not today); stderr ends up in logs and pages.
    fn scrub(&self, text: &str) -> String {
        let p = self.password.expose();
        if p.is_empty() {
            text.to_string()
        } else {
            text.replace(p, "••••")
        }
    }

    async fn output(&self, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        let child = self.spawn(args).await?;
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            return Err(ResticError {
                code: out.status.code(),
                stderr: self.scrub(&String::from_utf8_lossy(&out.stderr)),
            }
            .into());
        }
        Ok(out.stdout)
    }

    pub async fn snapshots(&self) -> anyhow::Result<Vec<Snapshot>> {
        let out = self.output(&["snapshots", "--json"]).await?;
        let mut list: Vec<Snapshot> =
            serde_json::from_slice(&out).context("reading restic's snapshot list")?;
        list.sort_by_key(|s| std::cmp::Reverse(s.time));
        Ok(list)
    }

    /// The entries directly inside `dir` of a snapshot.
    pub async fn ls(
        &self,
        snapshot: &str,
        dir: &str,
    ) -> anyhow::Result<(Option<Snapshot>, Vec<Node>)> {
        if !valid_snapshot_id(snapshot) || !valid_path(dir) {
            return Err(anyhow!("invalid snapshot or path"));
        }
        let out = self.output(&["ls", "--json", snapshot, dir]).await?;
        let mut snap = None;
        let mut nodes = Vec::new();
        for line in out.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let value: serde_json::Value = match serde_json::from_slice(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let kind = value
                .get("message_type")
                .or_else(|| value.get("struct_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match kind {
                "snapshot" => snap = serde_json::from_value(value).ok(),
                "node" => {
                    if let Ok(node) = serde_json::from_value::<Node>(value)
                        && node.path.trim_end_matches('/') != dir.trim_end_matches('/')
                    {
                        nodes.push(node);
                    }
                }
                _ => {}
            }
        }
        nodes.sort_by(|a, b| {
            b.is_dir()
                .cmp(&a.is_dir())
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok((snap, nodes))
    }

    /// Starts `restic dump` of a file, or of a folder as a zip, and returns its
    /// output once the first bytes are there (so a failure is still an error
    /// page, not a broken download).
    pub async fn dump(&self, snapshot: &str, path: &str, zip: bool) -> anyhow::Result<Dump> {
        if !valid_snapshot_id(snapshot) || !valid_path(path) {
            return Err(anyhow!("invalid snapshot or path"));
        }
        let mut args = vec!["dump"];
        if zip {
            args.extend(["--archive", "zip"]);
        }
        args.extend([snapshot, path]);
        let mut child = self.spawn(&args).await?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buf).await;
            buf
        });
        let mut first = vec![0u8; 64 * 1024];
        let n = stdout.read(&mut first).await?;
        first.truncate(n);
        if n == 0 {
            let status = child.wait().await?;
            if !status.success() {
                let stderr = stderr_task.await.unwrap_or_default();
                return Err(ResticError {
                    code: status.code(),
                    stderr: self.scrub(&String::from_utf8_lossy(&stderr)),
                }
                .into());
            }
        }
        Ok(Dump {
            first,
            stdout,
            child,
        })
    }
}

/// A download in progress: the bytes already read, and the rest.
pub struct Dump {
    pub first: Vec<u8>,
    pub stdout: ChildStdout,
    /// Kept so restic dies with the download (`kill_on_drop`) when the browser
    /// goes away, and so its exit status can end the stream.
    pub child: Child,
}

impl Dump {
    pub fn into_body(self) -> axum::body::Body {
        use futures_util::StreamExt;
        let Dump {
            first,
            stdout,
            mut child,
        } = self;
        let head = futures_util::stream::once(async move { Ok(bytes::Bytes::from(first)) });
        let rest = tokio_util::io::ReaderStream::new(stdout);
        // A failure mid-way must break the download rather than end it
        // cleanly: a truncated file the browser calls complete is worse.
        let tail = futures_util::stream::once(async move {
            match child.wait().await {
                Ok(s) if s.success() => None,
                Ok(s) => Some(Err(std::io::Error::other(format!(
                    "restic exited with {s}"
                )))),
                Err(e) => Some(Err(e)),
            }
        })
        .filter_map(|x| async move { x });
        axum::body::Body::from_stream(head.chain(rest).chain(tail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_are_checked() {
        assert!(valid_snapshot_id("latest"));
        assert!(valid_snapshot_id("c370df19"));
        assert!(!valid_snapshot_id("--password-file"));
        assert!(valid_path("/home/sigma/a b.txt"));
        assert!(!valid_path("-x"));
        assert!(!valid_path("/a/../etc"));
    }

    #[test]
    fn unlocks_expire() {
        let u = Unlocks::default();
        u.put("s", "r", Secret::new("pw".into()));
        assert!(u.get("s", "r", Duration::from_secs(60)).is_some());
        assert!(u.get("other", "r", Duration::from_secs(60)).is_none());
        std::thread::sleep(Duration::from_millis(20));
        assert!(u.get("s", "r", Duration::from_millis(5)).is_none());
        u.put("s", "r", Secret::new("pw".into()));
        u.forget_session("s");
        assert_eq!(u.count(), 0);
    }
}
