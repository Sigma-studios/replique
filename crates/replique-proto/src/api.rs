//! API paths, authentication and errors.

use serde::{Deserialize, Serialize};

/// Every device endpoint lives under this prefix. The version only moves for a
/// breaking change; adding optional fields does not bump it.
pub const API_PREFIX: &str = "/api/v1";

/// Path patterns in axum's syntax (`{param}`), so the server can route on them
/// directly. Devices build concrete paths with the functions below.
pub mod paths {
    pub const PAIR: &str = "/api/v1/pair";
    pub const JOB: &str = "/api/v1/jobs/{name}";
    pub const DEVICE_CONFIG: &str = "/api/v1/device/config";
    pub const RUNS: &str = "/api/v1/runs";
    pub const RUN_PROGRESS: &str = "/api/v1/runs/{id}/progress";
    pub const RUN_FINISH: &str = "/api/v1/runs/{id}/finish";
    pub const SAFE_TO_DELETE: &str = "/api/v1/devices/{id}/safe-to-delete";
    pub const MIRROR_SOURCES: &str = "/api/v1/mirror-sources";
    pub const VAULT: &str = "/api/v1/vault/{name}";

    /// Job names are restricted to `[a-z0-9_-]` (see `valid_job_name`), so they
    /// never need escaping in a path.
    pub fn job(name: &str) -> String {
        format!("/api/v1/jobs/{name}")
    }

    pub fn run_progress(run_id: &str) -> String {
        format!("/api/v1/runs/{run_id}/progress")
    }

    pub fn run_finish(run_id: &str) -> String {
        format!("/api/v1/runs/{run_id}/finish")
    }

    pub fn safe_to_delete(device_id: &str) -> String {
        format!("/api/v1/devices/{device_id}/safe-to-delete")
    }

    /// `name` follows the job name rules (`valid_job_name`).
    pub fn vault(name: &str) -> String {
        format!("/api/v1/vault/{name}")
    }
}

/// Device report tokens are `rpl_` followed by 32 random bytes in unpadded
/// base64url (43 characters). The prefix makes a leaked token easy to recognise
/// in logs and secret scanners.
pub const TOKEN_PREFIX: &str = "rpl_";
pub const TOKEN_RANDOM_BYTES: usize = 32;

/// True if `token` has the shape of a report token. Says nothing about whether
/// the server knows it.
pub fn is_well_formed_token(token: &str) -> bool {
    match token.strip_prefix(TOKEN_PREFIX) {
        Some(rest) => {
            rest.len() == 43
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        }
        None => false,
    }
}

/// Devices send progress at most this often. The server may drop progress that
/// arrives faster rather than store it.
pub const PROGRESS_MIN_INTERVAL_SECS: u64 = 10;

/// The largest file a device may send to `PUT /vault/{name}`. A Bitwarden
/// vault cache is a few megabytes; this leaves room for a big vault.
pub const MAX_VAULT_BYTES: usize = 64 * 1024 * 1024;

/// A finish report carries at most this many errors; `error_count` says how many
/// there were in total.
pub const MAX_REPORTED_ERRORS: usize = 50;

/// Job names: 1 to 64 characters of `a-z`, `0-9`, `-` and `_`, starting with a
/// letter or digit. They appear in URLs, systemd unit names and file names.
pub fn valid_job_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
}

/// The body of every non-2xx reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    /// A stable code clients can match on (see `error_code`). Kept as a string so
    /// a client meeting a code newer than itself still reads the message.
    pub error: String,
    /// A sentence for humans; may change between versions.
    pub message: String,
}

/// The error codes the server uses today.
pub mod error_code {
    /// Missing, malformed, unknown or revoked token (401).
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// The token is valid but the resource belongs to another device (403).
    pub const FORBIDDEN: &str = "forbidden";
    pub const NOT_FOUND: &str = "not_found";
    /// The request body or a parameter is invalid (400).
    pub const BAD_REQUEST: &str = "bad_request";
    /// Pairing code unknown or already used (400).
    pub const INVALID_CODE: &str = "invalid_code";
    /// Pairing code older than 15 minutes (400).
    pub const CODE_EXPIRED: &str = "code_expired";
    /// The run is already finished; a repeated finish with the same content is
    /// accepted instead (409).
    pub const CONFLICT: &str = "conflict";
    /// The server failed while doing what was asked, e.g. Hetzner's API refused
    /// to install the key during pairing (502 or 500). Safe to retry.
    pub const UPSTREAM: &str = "upstream";
}

/// The reply of calls that only acknowledge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
}

impl Ack {
    pub const OK: Ack = Ack { ok: true };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        assert!(is_well_formed_token(
            "rpl_q2V0aGlzLWlzLWEtZmFrZS10b2tlbi1mb3ItdGVzdHM"
        ));
        assert!(!is_well_formed_token("rpl_short"));
        assert!(!is_well_formed_token(
            "xyz_q2V0aGlzLWlzLWEtZmFrZS10b2tlbi1mb3ItdGVzdHM"
        ));
        assert!(!is_well_formed_token(
            "rpl_q2V0aGlzLWlzLWEtZmFrZS10b2tlbi1mb3ItdGVzdH="
        ));
    }

    #[test]
    fn job_names() {
        assert!(valid_job_name("hetzner"));
        assert!(valid_job_name("phone-mirror_2"));
        assert!(!valid_job_name(""));
        assert!(!valid_job_name("-x"));
        assert!(!valid_job_name("Windows"));
        assert!(!valid_job_name("a/b"));
        assert!(!valid_job_name(&"a".repeat(65)));
    }
}
