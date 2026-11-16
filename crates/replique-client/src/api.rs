//! The server's device API (`docs/protocol.md`), over blocking HTTP.

use std::time::Duration;

use replique_proto::{
    ApiError as Body, FinishRun, JobSpec, MirrorSources, PairRequest, PairResponse, Progress,
    StartRun, StartRunReply, paths,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// How a call failed, which decides what the caller does with the report.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// Network error, timeout or 5xx: the server may come back, so spool and
    /// retry later.
    Unreachable(String),
    /// 4xx: a bug or a revoked token. Retrying the same request will not help.
    Rejected {
        status: u16,
        code: String,
        message: String,
    },
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unreachable(m) => write!(f, "server unreachable: {m}"),
            ApiError::Rejected {
                status,
                code,
                message,
            } => write!(f, "server refused ({status} {code}): {message}"),
        }
    }
}

impl ApiError {
    pub fn is_conflict(&self) -> bool {
        matches!(self, ApiError::Rejected { status: 409, .. })
    }
}

#[derive(Clone)]
pub struct Api {
    base: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl Api {
    pub fn new(base: &str, token: Option<String>) -> Api {
        Api::with_timeout(base, token, Duration::from_secs(30))
    }

    pub fn with_timeout(base: &str, token: Option<String>, timeout: Duration) -> Api {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .user_agent(format!("replique-client/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Api {
            base: base.trim_end_matches('/').to_string(),
            token,
            agent,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn auth(&self) -> Option<String> {
        self.token.as_ref().map(|t| format!("Bearer {t}"))
    }

    fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: &B,
    ) -> Result<T, ApiError> {
        let url = self.url(path);
        let req = match method {
            "PUT" => self.agent.put(&url),
            _ => self.agent.post(&url),
        };
        let req = match self.auth() {
            Some(a) => req.header("Authorization", a),
            None => req,
        };
        let resp = req
            .send_json(body)
            .map_err(|e| ApiError::Unreachable(e.to_string()))?;
        read(resp)
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let req = self.agent.get(self.url(path));
        let req = match self.auth() {
            Some(a) => req.header("Authorization", a),
            None => req,
        };
        let resp = req
            .call()
            .map_err(|e| ApiError::Unreachable(e.to_string()))?;
        read(resp)
    }

    pub fn pair(&self, req: &PairRequest) -> Result<PairResponse, ApiError> {
        self.send("POST", paths::PAIR, req)
    }

    pub fn put_job(&self, name: &str, spec: &JobSpec) -> Result<(), ApiError> {
        self.send::<_, serde_json::Value>("PUT", &paths::job(name), spec)
            .map(drop)
    }

    pub fn start_run(&self, start: &StartRun) -> Result<String, ApiError> {
        self.send::<_, StartRunReply>("POST", paths::RUNS, start)
            .map(|r| r.run_id)
    }

    pub fn progress(&self, run_id: &str, p: &Progress) -> Result<(), ApiError> {
        self.send::<_, serde_json::Value>("POST", &paths::run_progress(run_id), p)
            .map(drop)
    }

    pub fn finish(&self, run_id: &str, f: &FinishRun) -> Result<(), ApiError> {
        self.send::<_, serde_json::Value>("POST", &paths::run_finish(run_id), f)
            .map(drop)
    }

    /// Sends a file as it is (`PUT /vault/{name}`): one encrypted already.
    pub fn put_vault(&self, name: &str, bytes: &[u8]) -> Result<(), ApiError> {
        let req = self
            .agent
            .put(self.url(&paths::vault(name)))
            .header("Content-Type", "application/octet-stream");
        let req = match self.auth() {
            Some(a) => req.header("Authorization", a),
            None => req,
        };
        let resp = req
            .send(bytes)
            .map_err(|e| ApiError::Unreachable(e.to_string()))?;
        read::<serde_json::Value>(resp).map(drop)
    }

    pub fn mirror_sources(&self) -> Result<MirrorSources, ApiError> {
        self.get(paths::MIRROR_SOURCES)
    }
}

fn read<T: DeserializeOwned>(mut resp: ureq::http::Response<ureq::Body>) -> Result<T, ApiError> {
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| ApiError::Unreachable(e.to_string()))?;
    if (200..300).contains(&status) {
        return serde_json::from_str(if text.trim().is_empty() {
            "null"
        } else {
            &text
        })
        .map_err(|e| ApiError::Rejected {
            status,
            code: "bad_reply".into(),
            message: format!("unexpected reply: {e}"),
        });
    }
    let body: Option<Body> = serde_json::from_str(&text).ok();
    let (code, message) = match body {
        Some(b) => (b.error, b.message),
        None => (String::new(), text.chars().take(200).collect()),
    };
    if status >= 500 {
        Err(ApiError::Unreachable(format!("{status} {code}: {message}")))
    } else {
        Err(ApiError::Rejected {
            status,
            code,
            message,
        })
    }
}
