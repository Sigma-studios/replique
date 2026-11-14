//! The contract between Réplique's server and the devices that report to it.
//!
//! Every message a device and the server exchange is defined here, along with the
//! API paths and the conventions they share (tokens, pairing links, where a
//! repository lives on the Storage Box). `docs/protocol.md` describes the same
//! thing in prose; `proto/fixtures/` holds one example of each message, read by the
//! tests here and by the Android app's tests, which mirror these types in Kotlin.
//!
//! Compatibility rule: no type here denies unknown fields. Either side may add an
//! optional field without the other breaking; anything that would break an older
//! peer (removing or renaming a field, changing its meaning) needs `/api/v2`.

pub mod api;
pub mod config;
pub mod jobs;
pub mod pairing;
pub mod runs;
pub mod storage;

pub use api::*;
pub use config::*;
pub use jobs::*;
pub use pairing::*;
pub use runs::*;
pub use storage::*;

/// Timestamps travel as RFC 3339 strings in UTC (`2026-11-14T03:12:45Z`).
pub type Timestamp = chrono::DateTime<chrono::Utc>;
