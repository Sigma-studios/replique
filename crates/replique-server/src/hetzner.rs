//! Hetzner's API for the Storage Box (`https://api.hetzner.com/v1`).
//!
//! Field names follow the official OpenAPI spec
//! (https://docs.hetzner.cloud/hetzner.spec.json). Every call that changes
//! something answers with an action that is still `running`; the change has
//! only happened once polling `GET /storage_boxes/actions/{id}` says `success`,
//! so every mutating method here waits for it before returning.

use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::secrets::Secret;

#[derive(Clone)]
pub struct Hetzner {
    base: String,
    token: Secret,
    http: reqwest::Client,
    /// Pause between two looks at a running action. Tests shorten it.
    pub poll_every: Duration,
    /// Give up on an action after this long.
    pub action_timeout: Duration,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageBox {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    pub storage_box_type: StorageBoxType,
    #[serde(default)]
    pub status: String,
    /// The main account's login, e.g. `u123456`.
    pub username: Option<String>,
    /// The box's host, e.g. `u123456.your-storagebox.de`.
    pub server: Option<String>,
    pub stats: Option<StorageBoxStats>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageBoxType {
    #[serde(default)]
    pub name: String,
    /// The quota, in bytes.
    pub size: u64,
    pub subaccounts_limit: u64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct StorageBoxStats {
    /// Everything used, in bytes: data and snapshots.
    pub size: u64,
    pub size_data: u64,
    pub size_snapshots: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Subaccount {
    pub id: u64,
    pub username: String,
    pub server: Option<String>,
    pub home_directory: String,
    pub access_settings: AccessSettings,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessSettings {
    pub reachable_externally: bool,
    pub samba_enabled: bool,
    pub ssh_enabled: bool,
    pub webdav_enabled: bool,
    pub readonly: bool,
}

impl AccessSettings {
    /// SSH only, reachable from outside Hetzner, writable or not.
    pub fn ssh_only(readonly: bool) -> AccessSettings {
        AccessSettings {
            reachable_externally: true,
            samba_enabled: false,
            ssh_enabled: true,
            webdav_enabled: false,
            readonly,
        }
    }
}

pub struct NewSubaccount<'a> {
    pub home_directory: &'a str,
    pub password: &'a Secret,
    pub access_settings: AccessSettings,
    pub description: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Action {
    pub id: u64,
    #[serde(default)]
    pub command: String,
    pub status: String,
    #[serde(default)]
    pub progress: u64,
    pub error: Option<ActionError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionError {
    pub code: String,
    pub message: String,
}

/// An error answer from the API: `{"error": {"code", "message"}}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
}

impl std::error::Error for ApiError {}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Hetzner answered {} ({}): {}",
            self.status.as_u16(),
            self.code,
            self.message
        )
    }
}

impl Hetzner {
    pub fn new(base: &str, token: Secret) -> anyhow::Result<Hetzner> {
        Ok(Hetzner {
            base: base.trim_end_matches('/').to_string(),
            token,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("replique-server/", env!("CARGO_PKG_VERSION")))
                .build()?,
            poll_every: Duration::from_secs(2),
            action_timeout: Duration::from_secs(300),
        })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<T> {
        let url = format!("{}{path}", self.base);
        let mut request = self
            .http
            .request(method.clone(), &url)
            .bearer_auth(self.token.expose());
        if let Some(body) = &body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("{method} {path}"))?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            #[derive(Deserialize)]
            struct Wrapper {
                error: Inner,
            }
            #[derive(Deserialize)]
            struct Inner {
                code: String,
                message: String,
            }
            let (code, message) = match serde_json::from_str::<Wrapper>(&text) {
                Ok(w) => (w.error.code, w.error.message),
                Err(_) => ("unknown".into(), text.chars().take(200).collect()),
            };
            return Err(ApiError {
                status,
                code,
                message,
            }
            .into());
        }
        // DELETE without an action, or an empty body, reads as JSON null.
        let text = if text.trim().is_empty() {
            "null"
        } else {
            &text
        };
        serde_json::from_str(text).with_context(|| format!("reading the answer to {method} {path}"))
    }

    /// Polls an action until it succeeds; an `error` status is an error.
    pub async fn wait(&self, action: Action) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let mut action = action;
        loop {
            match action.status.as_str() {
                "success" => return Ok(()),
                "error" => {
                    let (code, message) = action
                        .error
                        .map(|e| (e.code, e.message))
                        .unwrap_or_else(|| ("unknown".into(), "no detail".into()));
                    bail!(
                        "Hetzner action {} ({}) failed: {code}: {message}",
                        action.id,
                        action.command
                    );
                }
                _ => {}
            }
            if started.elapsed() > self.action_timeout {
                bail!(
                    "Hetzner action {} ({}) still running after {:?}",
                    action.id,
                    action.command,
                    self.action_timeout
                );
            }
            tokio::time::sleep(self.poll_every).await;
            #[derive(Deserialize)]
            struct Wrapper {
                action: Action,
            }
            let w: Wrapper = self
                .call(
                    Method::GET,
                    &format!("/storage_boxes/actions/{}", action.id),
                    None,
                )
                .await?;
            action = w.action;
        }
    }

    pub async fn storage_box(&self, id: u64) -> anyhow::Result<StorageBox> {
        #[derive(Deserialize)]
        struct Wrapper {
            storage_box: StorageBox,
        }
        let w: Wrapper = self
            .call(Method::GET, &format!("/storage_boxes/{id}"), None)
            .await?;
        Ok(w.storage_box)
    }

    pub async fn subaccounts(&self, box_id: u64) -> anyhow::Result<Vec<Subaccount>> {
        #[derive(Deserialize)]
        struct Wrapper {
            subaccounts: Vec<Subaccount>,
        }
        let w: Wrapper = self
            .call(
                Method::GET,
                &format!("/storage_boxes/{box_id}/subaccounts"),
                None,
            )
            .await?;
        Ok(w.subaccounts)
    }

    pub async fn subaccount(&self, box_id: u64, id: u64) -> anyhow::Result<Subaccount> {
        #[derive(Deserialize)]
        struct Wrapper {
            subaccount: Subaccount,
        }
        let w: Wrapper = self
            .call(
                Method::GET,
                &format!("/storage_boxes/{box_id}/subaccounts/{id}"),
                None,
            )
            .await?;
        Ok(w.subaccount)
    }

    /// Creates a sub-account, waits for it, and returns it as the API then
    /// describes it (its username is only known at that point).
    pub async fn create_subaccount(
        &self,
        box_id: u64,
        new: NewSubaccount<'_>,
    ) -> anyhow::Result<Subaccount> {
        #[derive(Deserialize)]
        struct Created {
            id: u64,
        }
        #[derive(Deserialize)]
        struct Wrapper {
            subaccount: Created,
            action: Action,
        }
        let body = json!({
            "home_directory": new.home_directory,
            "password": new.password.expose(),
            "access_settings": new.access_settings,
            "description": new.description,
            "labels": { "managed-by": "replique" },
        });
        let w: Wrapper = self
            .call(
                Method::POST,
                &format!("/storage_boxes/{box_id}/subaccounts"),
                Some(body),
            )
            .await?;
        self.wait(w.action).await?;
        self.subaccount(box_id, w.subaccount.id).await
    }

    pub async fn delete_subaccount(&self, box_id: u64, id: u64) -> anyhow::Result<()> {
        #[derive(Deserialize)]
        struct Wrapper {
            action: Option<Action>,
        }
        let w: Option<Wrapper> = self
            .call(
                Method::DELETE,
                &format!("/storage_boxes/{box_id}/subaccounts/{id}"),
                None,
            )
            .await?;
        if let Some(action) = w.and_then(|w| w.action) {
            self.wait(action).await?;
        }
        Ok(())
    }

    async fn subaccount_action(
        &self,
        box_id: u64,
        id: u64,
        action: &str,
        body: serde_json::Value,
    ) -> anyhow::Result<()> {
        #[derive(Deserialize)]
        struct Wrapper {
            action: Action,
        }
        let w: Wrapper = self
            .call(
                Method::POST,
                &format!("/storage_boxes/{box_id}/subaccounts/{id}/actions/{action}"),
                Some(body),
            )
            .await?;
        self.wait(w.action).await
    }

    pub async fn update_access_settings(
        &self,
        box_id: u64,
        id: u64,
        settings: AccessSettings,
    ) -> anyhow::Result<()> {
        self.subaccount_action(
            box_id,
            id,
            "update_access_settings",
            serde_json::to_value(settings)?,
        )
        .await
    }

    pub async fn reset_subaccount_password(
        &self,
        box_id: u64,
        id: u64,
        password: &Secret,
    ) -> anyhow::Result<()> {
        self.subaccount_action(
            box_id,
            id,
            "reset_subaccount_password",
            json!({ "password": password.expose() }),
        )
        .await
    }

    pub async fn change_home_directory(
        &self,
        box_id: u64,
        id: u64,
        home_directory: &str,
    ) -> anyhow::Result<()> {
        self.subaccount_action(
            box_id,
            id,
            "change_home_directory",
            json!({ "home_directory": home_directory }),
        )
        .await
    }
}
