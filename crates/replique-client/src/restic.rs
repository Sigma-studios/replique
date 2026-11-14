//! Running restic: the repository and password a job uses, idempotent init, and
//! reading its `--json` output.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use replique_proto::{RunError, Timestamp};
use serde::Deserialize;
use serde_json::Value;

use crate::Error;
use crate::config::{ClientConfig, JobConfig};
use crate::signals::Stop;
use crate::state::State;

/// Longer than any backup's own lock: forget, prune and check wait for a running
/// backup of another job sharing the repository instead of failing.
pub const RETRY_LOCK: &str = "2h";

#[derive(Debug, Clone)]
pub struct Restic {
    pub binary: PathBuf,
    pub repository: String,
    /// Absent for a mirror's copy, whose password only its own device knows:
    /// restic then asks for it.
    pub password_file: Option<PathBuf>,
    /// Extended options (`-o sftp.command=…`).
    pub options: Vec<String>,
    /// For repositories the client must not write to (a mirror's copy).
    pub no_lock: bool,
}

/// What a finished restic process said.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub code: i32,
    /// restic's `error` messages, the first `MAX_REPORTED_ERRORS`.
    pub errors: Vec<RunError>,
    pub error_count: u64,
    /// `item`s of the errors: files restic could not read.
    pub unreadable: Vec<String>,
    /// restic's `exit_error` message, or its last line of plain text.
    pub message: Option<String>,
}

impl Restic {
    /// The restic binary: `$REPLIQUE_RESTIC`, else `restic` from `PATH`.
    pub fn binary() -> PathBuf {
        std::env::var_os("REPLIQUE_RESTIC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("restic"))
    }

    /// The repository of a backup job: the one it names, else the paired
    /// Storage Box sub-account through the state directory's SSH config.
    pub fn for_job(config: &ClientConfig, state: &State, job: &JobConfig) -> Result<Restic, Error> {
        let password_file = Some(
            job.password_file
                .clone()
                .unwrap_or_else(|| state.password_file()),
        );
        if let Some(repo) = &job.repository {
            return Ok(Restic {
                binary: Restic::binary(),
                repository: repo.clone(),
                password_file,
                options: vec![],
                no_lock: false,
            });
        }
        let device = state.load_device()?.ok_or_else(|| {
            Error::msg(format!(
                "not paired yet: run `replique-client pair <server> <code>` (state directory {})",
                state.dir.display()
            ))
        })?;
        let alias = config.ssh_alias();
        Ok(Restic {
            binary: Restic::binary(),
            repository: format!("sftp:{alias}:{}", device.storage.repo_path),
            password_file,
            options: vec![
                "-o".into(),
                format!(
                    "sftp.command={}",
                    sftp_command(&state.ssh_config_file(), alias)
                ),
            ],
            no_lock: false,
        })
    }

    /// The local copy of a mirror job, read without locking.
    pub fn for_mirror(job: &JobConfig) -> Result<Restic, Error> {
        let path = job
            .mirror_path
            .as_ref()
            .ok_or_else(|| Error::msg("this mirror job has no mirror_path"))?;
        Ok(Restic {
            binary: Restic::binary(),
            repository: path.to_string_lossy().into_owned(),
            password_file: job.password_file.clone(),
            options: vec![],
            no_lock: true,
        })
    }

    pub fn command<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Command {
        let mut c = Command::new(&self.binary);
        c.env("RESTIC_REPOSITORY", &self.repository);
        // Whatever the caller's environment holds must not pick another
        // repository or password than the job's.
        for v in [
            "RESTIC_REPOSITORY_FILE",
            "RESTIC_PASSWORD",
            "RESTIC_PASSWORD_COMMAND",
            "RESTIC_PASSWORD_FILE",
        ] {
            c.env_remove(v);
        }
        if let Some(p) = &self.password_file {
            c.env("RESTIC_PASSWORD_FILE", p);
        }
        // With --json restic writes a status line every 1/60 s; one every two
        // seconds is plenty for progress sent every ten.
        if std::env::var_os("RESTIC_PROGRESS_FPS").is_none() {
            c.env("RESTIC_PROGRESS_FPS", "0.5");
        }
        c.args(&self.options);
        if self.no_lock {
            c.arg("--no-lock");
        }
        c.args(args);
        c
    }

    /// `restic version`'s number, e.g. `0.19.1`.
    pub fn version(&self) -> Option<String> {
        let out = Command::new(&self.binary).arg("version").output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.split_whitespace().nth(1).map(str::to_string)
    }

    /// Creates the repository if there is none: `cat config` exits 10 when the
    /// repository does not exist, anything else is a real problem (network,
    /// password) that `init` must not paper over. Returns whether it created one.
    pub fn ensure_repository(&self) -> Result<bool, Outcome> {
        let out = self
            .command(&["cat", "config"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| spawn_failure(&self.binary, e))?;
        let code = exit_code(out.status);
        match code {
            0 => Ok(false),
            10 => {
                let init = self
                    .command(&["init"])
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| spawn_failure(&self.binary, e))?;
                let code = exit_code(init.status);
                if code == 0 {
                    Ok(true)
                } else {
                    Err(Outcome {
                        code,
                        message: Some(format!("restic init failed: {}", last_line(&init.stderr))),
                        ..Outcome::default()
                    })
                }
            }
            _ => Err(Outcome {
                code,
                message: Some(format!(
                    "cannot open the repository: {}",
                    last_line(&out.stderr)
                )),
                ..Outcome::default()
            }),
        }
    }

    /// Runs restic with `--json`, calling `on_line` for each JSON line of its
    /// standard output, collecting its errors from standard error.
    pub fn stream(&self, args: &[String], stop: &Stop, mut on_line: impl FnMut(Value)) -> Outcome {
        let mut cmd = self.command(args);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return spawn_failure(&self.binary, e),
        };
        let stderr = child.stderr.take().expect("piped");
        let collector = std::thread::spawn(move || collect_stderr(stderr));
        let stdout = child.stdout.take().expect("piped");
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                on_line(v);
            }
        }
        let status = child.wait();
        let mut outcome = collector.join().unwrap_or_default();
        outcome.code = match status {
            Ok(s) => exit_code(s),
            Err(_) => 1,
        };
        if stop.requested() && outcome.code != 0 {
            outcome.code = 130;
        }
        outcome
    }

    /// Runs restic and returns its standard output, for commands whose output
    /// is one JSON document (`snapshots --json`, `forget --json`).
    pub fn output(&self, args: &[String]) -> Result<Vec<u8>, Outcome> {
        let out = self
            .command(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| spawn_failure(&self.binary, e))?;
        let code = exit_code(out.status);
        if code == 0 {
            Ok(out.stdout)
        } else {
            let mut o = collect_stderr(&out.stderr[..]);
            o.code = code;
            if o.message.is_none() {
                o.message = Some(format!(
                    "restic {} failed: {}",
                    args.first().map(String::as_str).unwrap_or(""),
                    last_line(&out.stderr)
                ));
            }
            Err(o)
        }
    }

    pub fn snapshots(&self, filters: &[String]) -> Result<Vec<Snapshot>, Outcome> {
        let mut args = vec!["snapshots".to_string(), "--json".into()];
        args.extend_from_slice(filters);
        let out = self.output(&args)?;
        serde_json::from_slice(&out).map_err(|e| Outcome {
            code: 1,
            message: Some(format!("unexpected `restic snapshots` output: {e}")),
            ..Outcome::default()
        })
    }
}

fn spawn_failure(binary: &Path, e: std::io::Error) -> Outcome {
    Outcome {
        code: 127,
        message: Some(format!("cannot run {}: {e}", binary.display())),
        ..Outcome::default()
    }
}

/// The line of restic's standard error worth reporting: its `Fatal:` line (the
/// lines after it are hints), else the last one.
fn last_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    lines
        .iter()
        .find(|l| l.starts_with("Fatal:"))
        .or(lines.last())
        .map(|l| l.to_string())
        .unwrap_or_else(|| "no output".into())
}

/// A process's exit code, with a signal reported as 128 + its number like a
/// shell does.
pub fn exit_code(s: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code()
        .or_else(|| s.signal().map(|n| 128 + n))
        .unwrap_or(1)
}

#[derive(Deserialize)]
struct ErrorLine {
    message_type: String,
    #[serde(default)]
    error: Option<ErrorBody>,
    #[serde(default)]
    during: Option<String>,
    #[serde(default)]
    item: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: String,
}

/// restic writes its `error` and `exit_error` messages, and anything it says in
/// plain text, to standard error.
fn collect_stderr(r: impl std::io::Read) -> Outcome {
    let mut o = Outcome::default();
    let mut last_text: Option<String> = None;
    for line in BufReader::new(r).lines() {
        let Ok(line) = line else { break };
        match serde_json::from_str::<ErrorLine>(&line) {
            Ok(e) if e.message_type == "error" => {
                o.error_count += 1;
                if let Some(item) = &e.item
                    && o.unreadable.len() < 1000
                {
                    o.unreadable.push(item.clone());
                }
                if o.errors.len() < replique_proto::MAX_REPORTED_ERRORS {
                    o.errors.push(RunError {
                        message: e.error.map(|b| b.message).unwrap_or_default(),
                        during: e.during,
                        item: e.item,
                    });
                }
            }
            Ok(e) if e.message_type == "exit_error" => o.message = e.message,
            Ok(_) => {}
            Err(_) => {
                let t = line.trim();
                if !t.is_empty() {
                    eprintln!("{t}");
                    last_text = Some(t.to_string());
                }
            }
        }
    }
    if o.message.is_none() {
        o.message = last_text;
    }
    o
}

/// The `sftp.command` for restic: ssh with the state directory's own config, so
/// neither the user's nor the system's SSH settings change what it connects to.
pub fn sftp_command(ssh_config: &Path, alias: &str) -> String {
    format!(
        "ssh -F {} {alias} -s sftp",
        quote(&ssh_config.to_string_lossy())
    )
}

/// restic splits `sftp.command` on spaces, honouring double quotes.
fn quote(s: &str) -> String {
    if s.contains([' ', '\t', '"', '\'']) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub time: Timestamp,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// One group of `restic forget --json`.
#[derive(Debug, Clone, Deserialize)]
pub struct ForgetGroup {
    #[serde(default)]
    pub keep: Option<Vec<Snapshot>>,
    #[serde(default)]
    pub remove: Option<Vec<Snapshot>>,
}

/// restic's backup `status` message.
#[derive(Debug, Default, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub seconds_elapsed: u64,
    #[serde(default)]
    pub seconds_remaining: Option<u64>,
    #[serde(default)]
    pub percent_done: f64,
    #[serde(default)]
    pub total_files: u64,
    #[serde(default)]
    pub files_done: u64,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub error_count: u64,
    #[serde(default)]
    pub current_files: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_errors() {
        let text = br#"{"message_type":"error","error":{"message":"/s/bad: open src/bad: permission denied"},"during":"archival","item":"/s/bad"}
{"message_type":"exit_error","code":3,"message":"Warning: at least one source file could not be read"}
"#;
        let o = collect_stderr(&text[..]);
        assert_eq!(o.error_count, 1);
        assert_eq!(o.unreadable, ["/s/bad"]);
        assert_eq!(o.errors[0].during.as_deref(), Some("archival"));
        assert_eq!(
            o.message.as_deref(),
            Some("Warning: at least one source file could not be read")
        );
    }

    #[test]
    fn sftp_commands() {
        assert_eq!(
            sftp_command(
                Path::new("/var/lib/replique-client/ssh/config"),
                "replique-storage"
            ),
            "ssh -F /var/lib/replique-client/ssh/config replique-storage -s sftp"
        );
        assert_eq!(
            sftp_command(Path::new("/tmp/a b/config"), "x"),
            "ssh -F \"/tmp/a b/config\" x -s sftp"
        );
    }

    #[test]
    fn fatal_line_wins() {
        assert_eq!(
            last_line(b"Fatal: repository does not exist\nIs there a repository at the following location?\n/x\n"),
            "Fatal: repository does not exist"
        );
        assert_eq!(last_line(b"a\nb\n\n"), "b");
    }
}
