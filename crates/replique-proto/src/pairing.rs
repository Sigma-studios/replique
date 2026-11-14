//! Pairing a device: the one-time code, the link in the QR code, `POST /pair`.

use serde::{Deserialize, Serialize};

use crate::StorageInfo;

/// A pairing code is valid this long after the website creates it, and once.
pub const PAIRING_CODE_TTL_SECS: u64 = 15 * 60;

/// The scheme of the link encoded in the pairing QR code.
pub const PAIRING_SCHEME: &str = "replique";

/// What kind of machine a device is. Decides which screens the website shows
/// for it (the phone rules editor only for `android`) and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Nixos,
    Android,
    Facade,
    /// Also what an older peer reads for a kind added after it was built.
    #[serde(other)]
    Other,
}

impl DeviceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceKind::Nixos => "nixos",
            DeviceKind::Android => "android",
            DeviceKind::Facade => "facade",
            DeviceKind::Other => "other",
        }
    }
}

/// `POST /api/v1/pair`, sent by the device without a token: the code is the
/// credential. The website chose the device's name and kind when it created the
/// code; the ones sent here are what the device calls itself, shown for
/// confirmation, and the website's win.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairRequest {
    pub code: String,
    pub device_name: String,
    pub kind: DeviceKind,
    /// An OpenSSH public key line (`ssh-ed25519 AAAA… comment`). The server
    /// installs it on the device's sub-account, the only key that can log in.
    pub ssh_public_key: String,
    /// e.g. `replique-client 0.1.0` or `replique-android 0.1.0 (Android 15)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

/// The reply to a successful pairing. The token is shown only here: the server
/// keeps its hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairResponse {
    pub device_id: String,
    pub device_name: String,
    /// `rpl_…`, sent as `Authorization: Bearer <token>` on every later call.
    pub report_token: String,
    pub storage: StorageInfo,
}

/// The pairing link: `replique://pair?server=<url>&code=<code>`. The CLI takes the
/// same two values as arguments (`replique-client pair <url> <code>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingLink {
    /// The server's public URL, e.g. `https://replique.sigma-dev.eu`, without a
    /// trailing slash.
    pub server: String,
    pub code: String,
}

impl PairingLink {
    pub fn to_uri(&self) -> String {
        let mut url = url::Url::parse("replique://pair").expect("static URL");
        url.query_pairs_mut()
            .append_pair("server", &self.server)
            .append_pair("code", &self.code);
        url.into()
    }

    pub fn parse(uri: &str) -> Option<PairingLink> {
        let url = url::Url::parse(uri.trim()).ok()?;
        if url.scheme() != PAIRING_SCHEME || url.host_str() != Some("pair") {
            return None;
        }
        let mut server = None;
        let mut code = None;
        for (k, v) in url.query_pairs() {
            match &*k {
                "server" => server = Some(v.trim_end_matches('/').to_string()),
                "code" => code = Some(v.into_owned()),
                _ => {}
            }
        }
        Some(PairingLink {
            server: server.filter(|s| !s.is_empty())?,
            code: code.filter(|c| !c.is_empty())?,
        })
    }
}

/// Pairing codes are shown to humans and typed on phones: 8 characters from an
/// alphabet without look-alikes, displayed as `ABCD-EFGH`. Comparison ignores
/// case, spaces and dashes; this puts a typed code in the canonical form.
pub fn normalize_pairing_code(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// The alphabet pairing codes are drawn from (no 0/O, 1/I/L).
pub const PAIRING_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
pub const PAIRING_CODE_LEN: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_round_trip() {
        let link = PairingLink {
            server: "https://replique.sigma-dev.eu".into(),
            code: "K7QM-2XPA".into(),
        };
        let uri = link.to_uri();
        assert_eq!(
            uri,
            "replique://pair?server=https%3A%2F%2Freplique.sigma-dev.eu&code=K7QM-2XPA"
        );
        assert_eq!(PairingLink::parse(&uri), Some(link));
    }

    #[test]
    fn link_rejects_others() {
        assert_eq!(PairingLink::parse("https://pair?server=x&code=y"), None);
        assert_eq!(PairingLink::parse("replique://pair?server=x"), None);
        assert_eq!(PairingLink::parse("replique://other?server=x&code=y"), None);
    }

    #[test]
    fn codes() {
        assert_eq!(normalize_pairing_code(" k7qm-2xpa "), "K7QM2XPA");
    }
}
