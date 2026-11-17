// Copyright 2026 The oc-build Authors
// SPDX-License-Identifier: Apache-2.0

//! Typed events from `GET /api/event`.
//!
//! Each SSE `data:` line is a JSON envelope `{"id","created","type","data",...}`.
//! Known `type`s map to a typed variant built from `data`; anything else (new
//! event types, or a known type whose payload no longer matches) becomes
//! [`Event::Unknown`] instead of an error.

use serde::Deserialize;
use serde_json::Value;

/// One event from the global opencode event stream.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    #[serde(rename = "server.connected")]
    ServerConnected {},

    #[serde(rename = "session.execution.started")]
    ExecutionStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.execution.succeeded")]
    ExecutionSucceeded {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.execution.failed")]
    ExecutionFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(default)]
        error: Option<StructuredError>,
    },
    #[serde(rename = "session.execution.interrupted")]
    ExecutionInterrupted {
        #[serde(rename = "sessionID")]
        session_id: String,
        /// `user`, `shutdown`, `superseded` or `inactivity`.
        #[serde(default)]
        reason: Option<String>,
    },
    #[serde(rename = "session.idle")]
    Idle {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    #[serde(rename = "session.status")]
    Status {
        #[serde(rename = "sessionID")]
        session_id: String,
        /// `{"type":"idle"|"busy"|"retry",...}`.
        status: Value,
    },

    #[serde(rename = "session.agent.selected")]
    AgentSelected {
        #[serde(rename = "sessionID")]
        session_id: String,
        agent: String,
        #[serde(default)]
        previous: Option<String>,
    },

    #[serde(rename = "session.model.selected")]
    ModelSelected {
        #[serde(rename = "sessionID")]
        session_id: String,
        model: ModelRef,
        #[serde(default)]
        previous: Option<ModelRef>,
    },

    #[serde(rename = "session.step.started")]
    StepStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        model: Option<ModelRef>,
    },
    #[serde(rename = "session.step.ended")]
    StepEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        /// `stop`, `length`, `tool-calls`, `content-filter`, `error` or `unknown`.
        finish: String,
        #[serde(default)]
        cost: Option<f64>,
        #[serde(default)]
        tokens: Option<TokenUsage>,
    },
    #[serde(rename = "session.step.failed")]
    StepFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        error: StructuredError,
    },

    #[serde(rename = "session.text.started")]
    TextStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
    },
    #[serde(rename = "session.text.delta")]
    TextDelta {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        delta: String,
    },
    #[serde(rename = "session.text.ended")]
    TextEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        text: String,
    },

    #[serde(rename = "session.reasoning.started")]
    ReasoningStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
    },
    #[serde(rename = "session.reasoning.delta")]
    ReasoningDelta {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        delta: String,
    },
    #[serde(rename = "session.reasoning.ended")]
    ReasoningEnded {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        text: String,
    },

    /// The model started streaming a tool call; `name` is only sent here.
    #[serde(rename = "session.tool.input.started")]
    ToolInputStarted {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        id: String,
        name: String,
    },
    #[serde(rename = "session.tool.called")]
    ToolCalled {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        id: String,
        input: Value,
    },
    #[serde(rename = "session.tool.progress")]
    ToolProgress {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        id: String,
        #[serde(default)]
        metadata: Option<Value>,
    },
    #[serde(rename = "session.tool.success")]
    ToolSuccess {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        id: String,
        content: Vec<ToolContent>,
    },
    #[serde(rename = "session.tool.failed")]
    ToolFailed {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "assistantMessageID")]
        message_id: String,
        id: String,
        error: StructuredError,
        #[serde(default)]
        content: Vec<ToolContent>,
    },

    #[serde(rename = "permission.asked")]
    PermissionAsked {
        /// Request id (`per_...`), passed to `Client::reply_permission`.
        id: String,
        #[serde(rename = "sessionID")]
        session_id: String,
        /// e.g. `shell`.
        action: String,
        resources: Vec<String>,
        /// Patterns that an `always` reply would save.
        #[serde(default)]
        save: Vec<String>,
        #[serde(default)]
        source: Option<PermissionSource>,
        #[serde(default)]
        message: Option<String>,
    },
    #[serde(rename = "permission.replied")]
    PermissionReplied {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "requestID")]
        request_id: String,
        /// `once`, `always` or `reject`.
        reply: String,
    },

    /// Any event type not modelled above, or a known type whose payload did
    /// not match. `raw` is the full envelope.
    #[serde(skip_deserializing)]
    Unknown { type_name: String, raw: Value },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StructuredError {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub status: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModelRef {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Selected variant id; absent or `default` for the model's default.
    #[serde(default)]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PermissionSource {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "messageID", default)]
    pub message_id: Option<String>,
    /// Tool call id for `type == "tool"`.
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum ToolContent {
    #[serde(rename = "text")]
    Text { text: String },
    /// File attachments and anything else.
    #[serde(other)]
    Other,
}

/// Concatenate the text parts of a tool result.
pub fn tool_text(content: &[ToolContent]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Text { text } => Some(text.as_str()),
            ToolContent::Other => None,
        })
        .collect()
}

impl Event {
    /// Parse one SSE `data:` payload. Only fails if `data` is not JSON at all.
    pub fn parse(data: &str) -> Result<Event, serde_json::Error> {
        if let Ok(event) = serde_json::from_str::<Event>(data) {
            return Ok(event);
        }
        let raw: Value = serde_json::from_str(data)?;
        let type_name = raw
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(Event::Unknown { type_name, raw })
    }

    /// The wire `type` string, e.g. `session.text.delta`.
    pub fn type_name(&self) -> &str {
        match self {
            Event::ServerConnected {} => "server.connected",
            Event::ExecutionStarted { .. } => "session.execution.started",
            Event::ExecutionSucceeded { .. } => "session.execution.succeeded",
            Event::ExecutionFailed { .. } => "session.execution.failed",
            Event::ExecutionInterrupted { .. } => "session.execution.interrupted",
            Event::Idle { .. } => "session.idle",
            Event::Status { .. } => "session.status",
            Event::AgentSelected { .. } => "session.agent.selected",
            Event::ModelSelected { .. } => "session.model.selected",
            Event::StepStarted { .. } => "session.step.started",
            Event::StepEnded { .. } => "session.step.ended",
            Event::StepFailed { .. } => "session.step.failed",
            Event::TextStarted { .. } => "session.text.started",
            Event::TextDelta { .. } => "session.text.delta",
            Event::TextEnded { .. } => "session.text.ended",
            Event::ReasoningStarted { .. } => "session.reasoning.started",
            Event::ReasoningDelta { .. } => "session.reasoning.delta",
            Event::ReasoningEnded { .. } => "session.reasoning.ended",
            Event::ToolInputStarted { .. } => "session.tool.input.started",
            Event::ToolCalled { .. } => "session.tool.called",
            Event::ToolProgress { .. } => "session.tool.progress",
            Event::ToolSuccess { .. } => "session.tool.success",
            Event::ToolFailed { .. } => "session.tool.failed",
            Event::PermissionAsked { .. } => "permission.asked",
            Event::PermissionReplied { .. } => "permission.replied",
            Event::Unknown { type_name, .. } => type_name,
        }
    }

    /// The session this event belongs to, if any. For [`Event::Unknown`] this
    /// is `data.sessionID` when present.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Event::ServerConnected {} => None,
            Event::ExecutionStarted { session_id }
            | Event::ExecutionSucceeded { session_id }
            | Event::ExecutionFailed { session_id, .. }
            | Event::ExecutionInterrupted { session_id, .. }
            | Event::Idle { session_id }
            | Event::Status { session_id, .. }
            | Event::AgentSelected { session_id, .. }
            | Event::ModelSelected { session_id, .. }
            | Event::StepStarted { session_id, .. }
            | Event::StepEnded { session_id, .. }
            | Event::StepFailed { session_id, .. }
            | Event::TextStarted { session_id, .. }
            | Event::TextDelta { session_id, .. }
            | Event::TextEnded { session_id, .. }
            | Event::ReasoningStarted { session_id, .. }
            | Event::ReasoningDelta { session_id, .. }
            | Event::ReasoningEnded { session_id, .. }
            | Event::ToolInputStarted { session_id, .. }
            | Event::ToolCalled { session_id, .. }
            | Event::ToolProgress { session_id, .. }
            | Event::ToolSuccess { session_id, .. }
            | Event::ToolFailed { session_id, .. }
            | Event::PermissionAsked { session_id, .. }
            | Event::PermissionReplied { session_id, .. } => Some(session_id),
            Event::Unknown { raw, .. } => raw.pointer("/data/sessionID").and_then(Value::as_str),
        }
    }
}
