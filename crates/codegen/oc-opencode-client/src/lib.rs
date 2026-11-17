// Copyright 2026 The oc-build Authors
// SPDX-License-Identifier: Apache-2.0

//! Minimal async client for the opencode v2 server (`opencode serve`).
//!
//! Requests are plain JSON over HTTP with Basic auth (user `opencode`); the
//! turn itself streams over the global SSE endpoint, see [`Client::events`].

mod event;

use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use event::{
    Event, ModelRef, PermissionSource, StructuredError, TokenUsage, ToolContent, tool_text,
};

const USERNAME: &str = "opencode";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// Non-success status; `body` is the raw response body, typically
    /// `{"_tag":"InvalidRequestError","message":...}`.
    #[error("server returned {status}: {body}")]
    Api { status: u16, body: String },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("event stream: {0}")]
    Stream(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Body of `POST /api/session`. Every field is optional.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateSession {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Vec<PermissionRule>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Location {
    /// Absolute path.
    pub directory: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Model {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionRule {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Once,
    Always,
    Reject,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
}

/// One entry of `GET /api/model` (only the fields the clients need).
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub limit: Option<ModelLimit>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModelLimit {
    /// Context window in tokens.
    pub context: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InterruptResponse {
    /// `false` when the session was already idle.
    pub interrupted: bool,
}

#[derive(Deserialize)]
struct IdOnly {
    id: String,
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    password: Option<String>,
}

impl Client {
    /// `base_url` like `http://127.0.0.1:4096`; `password` is the server's
    /// `OPENCODE_PASSWORD` (none for an unauthenticated server).
    pub fn new(base_url: impl Into<String>, password: Option<String>) -> Result<Self> {
        // No overall timeout: the event stream is long-lived.
        #[allow(clippy::disallowed_methods)]
        // standalone client for a local opencode server; no grok TLS policy
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            password,
        })
    }

    pub async fn create_session(&self, body: &CreateSession) -> Result<Session> {
        let resp = self.post("/api/session").json(body).send().await?;
        json_data(resp).await
    }

    /// Queue a user prompt; returns the inbox message id (`msg_...`) right
    /// away. The turn runs asynchronously and streams over [`Client::events`].
    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<String> {
        let resp = self
            .post(&format!("/api/session/{session_id}/prompt"))
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await?;
        Ok(json_data::<IdOnly>(resp).await?.id)
    }

    pub async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        decision: Decision,
        message: Option<&str>,
    ) -> Result<()> {
        let resp = self
            .post(&format!(
                "/api/session/{session_id}/permission/{request_id}/reply"
            ))
            .json(&serde_json::json!({ "decision": decision, "message": message }))
            .send()
            .await?;
        check(resp).await.map(drop)
    }

    /// Chat models the server offers, ordered by release date.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self
            .request(reqwest::Method::GET, "/api/model")
            .send()
            .await?;
        json_data(resp).await
    }

    /// Switch the model used by the session's subsequent provider turns.
    pub async fn switch_model(&self, session_id: &str, model: &Model) -> Result<()> {
        let resp = self
            .post(&format!("/api/session/{session_id}/model"))
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await?;
        check(resp).await.map(drop)
    }

    pub async fn interrupt(&self, session_id: &str) -> Result<InterruptResponse> {
        let resp = self
            .post(&format!("/api/session/{session_id}/interrupt"))
            .send()
            .await?;
        json_data(resp).await
    }

    /// Subscribe to the global event stream (all sessions; filter with
    /// [`Event::session_id`]). Resolves once the server accepted the
    /// subscription, so subscribe before prompting to not miss events.
    pub async fn events(&self) -> Result<impl Stream<Item = Result<Event>> + Send + 'static> {
        let resp = self
            .request(reqwest::Method::GET, "/api/event")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await?;
        let resp = check(resp).await?;
        Ok(resp
            .bytes_stream()
            .eventsource()
            .filter_map(|item| async move {
                match item {
                    Ok(ev) if ev.data.trim().is_empty() => None,
                    Ok(ev) => Some(Event::parse(&ev.data).map_err(Error::from)),
                    Err(err) => Some(Err(Error::Stream(err.to_string()))),
                }
            }))
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, path)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let req = self
            .http
            .request(method, format!("{}{}", self.base_url, path));
        match &self.password {
            Some(pw) => req.basic_auth(USERNAME, Some(pw)),
            None => req,
        }
    }
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(Error::Api {
        status: status.as_u16(),
        body,
    })
}

/// Decode a JSON response, unwrapping the `{"data": ...}` envelope if present.
async fn json_data<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    let mut value: Value = check(resp).await?.json().await?;
    if let Some(data) = value.get_mut("data") {
        value = data.take();
    }
    Ok(serde_json::from_value(value)?)
}
