//! replique-client: wraps restic (and rclone for mirrors) on a device and reports
//! every run to Réplique. Backups never depend on the server: when it is down,
//! reports go to a local spool and are sent on a later run.

pub mod api;
pub mod config;
pub mod excludes;
pub mod mirror;
pub mod mounts;
pub mod netmeter;
pub mod pair;
pub mod report;
pub mod restic;
pub mod retention;
pub mod run;
pub mod signals;
pub mod spool;
pub mod state;
pub mod status;

use std::fmt;

/// `replique-client 0.1.0`, sent as `client_version` and in the User-Agent.
pub fn client_version() -> String {
    format!("replique-client {}", env!("CARGO_PKG_VERSION"))
}

/// The client's errors are messages for the person reading the journal; nothing
/// matches on them.
#[derive(Debug)]
pub struct Error(pub String);

impl Error {
    pub fn msg(m: impl Into<String>) -> Error {
        Error(m.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error(e.to_string())
    }
}

/// Logs to stderr, which systemd puts in the journal next to restic's own lines.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("replique: {}", format_args!($($arg)*))
    };
}
