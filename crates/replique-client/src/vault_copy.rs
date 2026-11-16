//! `replique-client vault-copy <name> <file>`: sends a file that is encrypted
//! already, as it is, to Réplique (`PUT /api/v1/vault/{name}`).
//!
//! For the Bitwarden desktop app's vault, encrypted with the master password:
//! a copy of it outside restic is reachable without any password kept in
//! Bitwarden (see the server's `vault`). The NixOS module runs this whenever
//! the file changes and once a day; the server keeps one copy per content and
//! notes the daily resend as "still current".
//!
//! Only the file named here is sent: the device, not the server, decides.

use std::path::Path;

use replique_proto::{MAX_VAULT_BYTES, valid_job_name};

use crate::api::{Api, ApiError};
use crate::config::ClientConfig;
use crate::state::State;
use crate::{Error, log};

/// sysexits' "temporary failure": the server is down, the next change or the
/// daily timer sends it again.
const EX_TEMPFAIL: i32 = 75;

pub fn run(config: &ClientConfig, state: &State, name: &str, file: &Path) -> Result<i32, Error> {
    if !valid_job_name(name) {
        return Err(Error::msg(format!(
            "{name}: names are 1 to 64 of a-z, 0-9, - and _"
        )));
    }
    let bytes = std::fs::read(file).map_err(|e| Error::msg(format!("{}: {e}", file.display())))?;
    if bytes.is_empty() {
        return Err(Error::msg(format!("{} is empty", file.display())));
    }
    if bytes.len() > MAX_VAULT_BYTES {
        return Err(Error::msg(format!(
            "{} is larger than the {} MB the server takes",
            file.display(),
            MAX_VAULT_BYTES / 1_000_000
        )));
    }
    let device = state.load_device()?;
    let server = config
        .server
        .clone()
        .or_else(|| device.map(|d| d.server))
        .ok_or_else(|| Error::msg("not paired with a server"))?;
    let token = state
        .token()?
        .ok_or_else(|| Error::msg("not paired with a server"))?;
    match Api::new(&server, Some(token)).put_vault(name, &bytes) {
        Ok(()) => {
            log!("{name}: sent {} bytes", bytes.len());
            Ok(0)
        }
        Err(e @ ApiError::Unreachable(_)) => {
            log!("{name}: {e}; it goes with the next change or the daily send");
            Ok(EX_TEMPFAIL)
        }
        Err(e) => Err(Error::msg(format!("{name}: {e}"))),
    }
}
