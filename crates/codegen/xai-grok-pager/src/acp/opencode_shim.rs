// Copyright 2026 The oc-build Authors
// SPDX-License-Identifier: Apache-2.0

//! In-process ACP agent that forwards to an opencode v2 server (`opencode serve`).
//!
//! Serves the agent side of the same `acp_channels()` pair the embedded GrokShell
//! uses, so the TUI is unchanged: `session/new` creates an opencode session,
//! `session/prompt` queues the text and streams the opencode SSE events back as
//! `session/update` notifications until the execution ends, `session/cancel`
//! interrupts it, and `permission.asked` becomes `session/request_permission`.
//!
//! `x.ai/*` extension calls are answered with stubs. Each call and turn step is
//! logged at debug level (target `oc_shim`) and, when env `OC_SHIM_LOG=<path>`
//! is set, appended to that file (see [`ext_log`]) so the set the TUI really
//! uses can be audited.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use agent_client_protocol::{self as acp, Client as _};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use oc_opencode_client::{
    Client, CreateSession, Decision, Effect, Event, Location, Model, PermissionRule, tool_text,
};
use tokio_util::sync::CancellationToken;
use xai_acp_lib::{AcpGatewayReceiver, AcpGatewaySender, acp_channels};
use xai_grok_tools::types::output::{BashOutput, ToolOutput};

use super::spawn::{SpawnedAgent, spawn_runtime_thread};
use super::{AcpConnection, AgentEndpoint, ConnectFlags, initialize_connection};

/// Default model when `--opencode-model` is not given.
pub const DEFAULT_MODEL: &str = "opencode/big-pickle";
/// Auth method advertised so the pager's eager-auth step succeeds without a login screen
/// (an empty list is treated as "needs login").
const AUTH_METHOD_ID: &str = "opencode";
/// How long a cancelled turn waits for `session.execution.interrupted` before giving up.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// Where and how to reach the opencode server.
#[derive(Debug, Clone)]
pub struct OpencodeConfig {
    pub url: String,
    pub password: Option<String>,
    /// `provider/id`.
    pub model: String,
}

/// Spawn the shim and run the standard initialize/authenticate sequence against it.
pub async fn connect(
    cancel: &CancellationToken,
    flags: ConnectFlags,
    config: OpencodeConfig,
) -> Result<AcpConnection> {
    let spawned = spawn_opencode_shim(cancel, config).await?;
    let auth_manager = spawned.auth_manager.clone();
    initialize_connection(AgentEndpoint::from(spawned), &flags, auth_manager).await
}

async fn spawn_opencode_shim(
    cancel: &CancellationToken,
    config: OpencodeConfig,
) -> Result<SpawnedAgent> {
    let (provider_id, model_id) = config
        .model
        .split_once('/')
        .context("--opencode-model must be provider/id")?;
    let model = Model {
        id: model_id.to_owned(),
        provider_id: provider_id.to_owned(),
    };
    let client = Client::new(config.url.clone(), config.password.clone())
        .context("failed to build the opencode client")?;
    let directory = std::env::current_dir().context("failed to resolve cwd")?;
    // Inert: constructed only because `SpawnedAgent` carries one. No refresher or
    // refresh loop is started, so it never talks to xAI.
    let auth_manager = std::sync::Arc::new(xai_grok_login::AuthManager::new_with_proxy_base_url(
        &xai_grok_shell::util::grok_home::grok_home(),
        xai_grok_login::GrokComConfig::default(),
        xai_grok_shell::agent::config::EndpointsConfig::default().proxy_url(),
    ));
    ext_log(&format!(
        "shim start url={} model={} dir={}",
        config.url,
        config.model,
        directory.display()
    ));

    let (acp_client, acp_agent) = acp_channels();
    let agent_cancel = cancel.child_token();
    let thread_cancel = agent_cancel.clone();
    let handle = spawn_runtime_thread("opencode-shim", move |rt| {
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let shim = ShimAgent(Rc::new(OpencodeShim {
                client,
                gateway: AcpGatewaySender::new(acp_agent.tx.clone()),
                model,
                model_label: config.model,
                directory,
                active: RefCell::new(HashMap::new()),
                started: Instant::now(),
            }));
            let receiver = AcpGatewayReceiver::new(acp_agent.rx, shim).with_tracing(true);
            tokio::task::spawn_local(receiver.run());
            thread_cancel.cancelled().await;
        });
        drop(local);
        super::spawn::shutdown_worker_runtime(rt);
        Ok(())
    })
    .await?;

    Ok(SpawnedAgent {
        thread_handle: handle,
        channel: acp_client,
        cancel: agent_cancel,
        auth_manager,
    })
}

struct OpencodeShim {
    client: Client,
    gateway: AcpGatewaySender<acp::AgentSide>,
    model: Model,
    model_label: String,
    directory: PathBuf,
    /// Cancel token of the running turn, keyed by session id.
    active: RefCell<HashMap<String, CancellationToken>>,
    started: Instant,
}

impl OpencodeShim {
    fn model_state(&self) -> acp::SessionModelState {
        acp::SessionModelState::new(
            self.model_label.clone(),
            vec![acp::ModelInfo::new(
                self.model_label.clone(),
                self.model_label.clone(),
            )],
        )
    }

    fn log(&self, line: &str) {
        log_since(self.started, line);
    }

    fn notify(
        &self,
        session_id: &acp::SessionId,
        meta: &Option<acp::Meta>,
        update: acp::SessionUpdate,
    ) {
        let notification =
            acp::SessionNotification::new(session_id.clone(), update).meta(meta.clone());
        self.gateway.forward_fire_and_forget(notification);
    }

    /// Run one opencode turn and translate its events until the execution ends.
    async fn run_turn(
        self: &Rc<Self>,
        session_id: &acp::SessionId,
        text: &str,
        meta: Option<acp::Meta>,
        cancel: CancellationToken,
    ) -> acp::Result<acp::StopReason> {
        let sid = session_id.0.to_string();
        // Subscribe first so no event of this turn is missed.
        let mut events = Box::pin(self.client.events().await.map_err(oc_error)?);
        self.client.prompt(&sid, text).await.map_err(oc_error)?;

        let mut turn = TurnState::new(
            sid.clone(),
            self.directory.display().to_string(),
            self.started,
        );
        let interrupt_deadline = tokio::time::sleep(Duration::MAX);
        tokio::pin!(interrupt_deadline);

        loop {
            let event = tokio::select! {
                () = cancel.cancelled(), if !turn.cancelling => {
                    turn.cancelling = true;
                    interrupt_deadline
                        .as_mut()
                        .reset(tokio::time::Instant::now() + INTERRUPT_GRACE);
                    continue;
                }
                () = &mut interrupt_deadline, if turn.cancelling => {
                    self.log("turn: no interrupted event within grace; reporting cancelled");
                    return Ok(acp::StopReason::Cancelled);
                }
                event = events.next() => match event {
                    Some(Ok(event)) => event,
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "opencode event stream error");
                        continue;
                    }
                    None => return Err(acp_error("opencode event stream ended")),
                },
            };
            match turn.on_event(event) {
                TurnStep::Ignore => {}
                TurnStep::Update(update) => self.notify(session_id, &meta, update),
                TurnStep::Permission {
                    request_id,
                    tool_call,
                } => {
                    let request = acp::RequestPermissionRequest::new(
                        session_id.clone(),
                        tool_call,
                        permission_options(),
                    );
                    let shim = Rc::clone(self);
                    let sid = sid.clone();
                    tokio::task::spawn_local(async move {
                        shim.ask_permission(sid, request_id, request).await;
                    });
                }
                TurnStep::Stop(reason) => return Ok(reason),
                TurnStep::Fail(message) => {
                    return Err(acp_error(&format!("opencode execution failed: {message}")));
                }
            }
        }
    }

    async fn ask_permission(
        &self,
        sid: String,
        request_id: String,
        request: acp::RequestPermissionRequest,
    ) {
        let decision = match self.gateway.request_permission(request).await {
            Ok(resp) => decision_for(&resp.outcome),
            Err(err) => {
                tracing::warn!(error = %err, "request_permission failed; rejecting");
                Decision::Reject
            }
        };
        self.log(&format!("permission {request_id} -> {decision:?}"));
        if let Err(err) = self
            .client
            .reply_permission(&sid, &request_id, decision, None)
            .await
        {
            tracing::warn!(error = %err, "opencode reply_permission failed");
        }
    }
}

/// The `acp::Agent` face of the shim; `Rc` so turns can spawn permission tasks.
struct ShimAgent(Rc<OpencodeShim>);

impl std::ops::Deref for ShimAgent {
    type Target = Rc<OpencodeShim>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct ToolState {
    name: String,
    /// Set for execute tools once the input is known.
    command: Option<String>,
}

/// What the turn loop should do with one opencode event.
#[derive(Debug)]
enum TurnStep {
    Ignore,
    Update(acp::SessionUpdate),
    /// Ask the TUI; `request_id` is the opencode permission id to reply to.
    Permission {
        request_id: String,
        tool_call: acp::ToolCallUpdate,
    },
    Stop(acp::StopReason),
    /// The execution failed; the turn ends with an error carrying this message.
    Fail(String),
}

/// Per-turn translation state: opencode events in, [`TurnStep`]s out. No I/O
/// besides [`ext_log`], so it is unit-testable.
struct TurnState {
    session_id: String,
    /// Shown as the execute block's working directory.
    directory: String,
    log_epoch: Instant,
    tools: HashMap<String, ToolState>,
    started: bool,
    /// Set once the TUI cancelled; new output is dropped from then on.
    cancelling: bool,
}

impl TurnState {
    fn new(session_id: String, directory: String, log_epoch: Instant) -> Self {
        Self {
            session_id,
            directory,
            log_epoch,
            tools: HashMap::new(),
            started: false,
            cancelling: false,
        }
    }

    fn log(&self, line: &str) {
        log_since(self.log_epoch, line);
    }

    fn end_reason(&self) -> acp::StopReason {
        if self.cancelling {
            acp::StopReason::Cancelled
        } else {
            acp::StopReason::EndTurn
        }
    }

    fn on_event(&mut self, event: Event) -> TurnStep {
        if event.session_id() != Some(self.session_id.as_str()) {
            return TurnStep::Ignore;
        }
        match event {
            Event::ExecutionStarted { .. } => {
                self.started = true;
                TurnStep::Ignore
            }
            Event::ExecutionSucceeded { .. } => TurnStep::Stop(self.end_reason()),
            Event::ExecutionInterrupted { reason, .. } => {
                self.log(&format!("turn: interrupted reason={reason:?}"));
                TurnStep::Stop(acp::StopReason::Cancelled)
            }
            Event::ExecutionFailed { error, .. } => TurnStep::Fail(
                error
                    .map(|e| format!("{}: {}", e.kind, e.message))
                    .unwrap_or_else(|| "unknown error".to_owned()),
            ),
            Event::Idle { .. } if self.started => TurnStep::Stop(self.end_reason()),
            // After a cancel, stop painting new output.
            _ if self.cancelling => TurnStep::Ignore,
            Event::TextDelta { delta, .. } => TurnStep::Update(
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(delta.into())),
            ),
            Event::ReasoningDelta { delta, .. } => TurnStep::Update(
                acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(delta.into())),
            ),
            Event::ToolInputStarted { id, name, .. } => {
                self.tools.insert(
                    id,
                    ToolState {
                        name,
                        command: None,
                    },
                );
                TurnStep::Ignore
            }
            Event::ToolCalled { id, input, .. } => {
                let state = self.tools.entry(id.clone()).or_insert_with(|| ToolState {
                    name: String::new(),
                    command: None,
                });
                let kind = tool_kind(&state.name);
                let (title, raw_input) = if kind == acp::ToolKind::Execute {
                    let command = input
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    state.command = Some(command.clone());
                    let raw = match input.get("description") {
                        Some(desc) => {
                            serde_json::json!({ "command": command, "description": desc })
                        }
                        None => serde_json::json!({ "command": command }),
                    };
                    (command, raw)
                } else {
                    (format!("{} {}", state.name, compact(&input)), input)
                };
                let line = format!("tool_call {id} {} kind={kind:?}: {title}", state.name);
                self.log(&line);
                TurnStep::Update(acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(id, title)
                        .kind(kind)
                        .status(acp::ToolCallStatus::InProgress)
                        .raw_input(raw_input),
                ))
            }
            Event::ToolSuccess { id, content, .. } => {
                let output = tool_text(&content);
                self.log(&format!(
                    "tool_success {id} output={}",
                    compact(&output.as_str().into())
                ));
                let update = self.tool_result(&id, &output, true);
                TurnStep::Update(acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(id, update),
                ))
            }
            Event::ToolFailed {
                id, error, content, ..
            } => {
                let mut output = tool_text(&content);
                if !error.message.is_empty() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&error.message);
                }
                self.log(&format!(
                    "tool_failed {id} output={}",
                    compact(&output.as_str().into())
                ));
                let update = self.tool_result(&id, &output, false);
                TurnStep::Update(acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(id, update),
                ))
            }
            Event::PermissionAsked {
                id,
                action,
                resources,
                source,
                ..
            } => {
                let tool_call_id = source.and_then(|s| s.id).unwrap_or_else(|| id.clone());
                let tool = self.tools.get(&tool_call_id);
                let kind = tool.map_or_else(|| tool_kind(&action), |t| tool_kind(&t.name));
                let command = tool
                    .and_then(|t| t.command.clone())
                    .unwrap_or_else(|| resources.join(" "));
                let mut fields = acp::ToolCallUpdateFields::new()
                    .kind(kind)
                    .title(command.clone());
                if kind == acp::ToolKind::Execute {
                    fields = fields.raw_input(serde_json::json!({ "command": command }));
                }
                TurnStep::Permission {
                    request_id: id,
                    tool_call: acp::ToolCallUpdate::new(tool_call_id, fields),
                }
            }
            _ => TurnStep::Ignore,
        }
    }

    fn tool_result(&self, id: &str, output: &str, success: bool) -> acp::ToolCallUpdateFields {
        let status = if success {
            acp::ToolCallStatus::Completed
        } else {
            acp::ToolCallStatus::Failed
        };
        let mut fields = acp::ToolCallUpdateFields::new()
            .status(status)
            .content(vec![output.to_owned().into()]);
        if let Some(command) = self.tools.get(id).and_then(|t| t.command.clone()) {
            // The execute block renders its output from a Bash `raw_output`.
            let bash = BashOutput {
                output: output.as_bytes().to_vec(),
                output_for_prompt: String::new(),
                exit_code: if success { 0 } else { 1 },
                command,
                truncated: false,
                signal: None,
                timed_out: false,
                description: None,
                current_dir: self.directory.clone(),
                output_file: String::new(),
                total_bytes: output.len(),
                output_delta: None,
                was_bare_echo: false,
            };
            if let Ok(value) = serde_json::to_value(ToolOutput::Bash(bash)) {
                fields = fields.raw_output(value);
            }
        }
        fields
    }
}

/// Map the TUI's permission answer to an opencode reply; anything but an allow
/// option (including a cancelled modal) rejects.
fn decision_for(outcome: &acp::RequestPermissionOutcome) -> Decision {
    match outcome {
        acp::RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            "allow-once" => Decision::Once,
            "allow-always" => Decision::Always,
            _ => Decision::Reject,
        },
        _ => Decision::Reject,
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for ShimAgent {
    async fn initialize(
        &self,
        _args: acp::InitializeRequest,
    ) -> acp::Result<acp::InitializeResponse> {
        self.log("initialize");
        let meta = serde_json::json!({
            "modelState": self.model_state(),
            // No server-side rewind; keep the cancelled turn on screen.
            "cancelRewind": false,
        });
        Ok(acp::InitializeResponse::new(acp::ProtocolVersion::V1)
            .auth_methods(vec![acp::AuthMethod::Agent(acp::AuthMethodAgent::new(
                AUTH_METHOD_ID,
                "opencode server",
            ))])
            .meta(meta.as_object().cloned()))
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        self.log("authenticate");
        Ok(acp::AuthenticateResponse::new())
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> acp::Result<acp::NewSessionResponse> {
        let directory = if args.cwd.is_absolute() {
            args.cwd.clone()
        } else {
            self.directory.clone()
        };
        // Ask for shell commands so the TUI's permission modal is exercised; the
        // asked event names the tool `shell`, older builds used `bash`.
        let permissions = ["shell", "bash"]
            .map(|action| PermissionRule {
                action: action.to_owned(),
                resource: "*".to_owned(),
                effect: Effect::Ask,
            })
            .to_vec();
        let session = self
            .client
            .create_session(&CreateSession {
                title: None,
                location: Some(Location {
                    directory: directory.display().to_string(),
                }),
                model: Some(self.model.clone()),
                permissions: Some(permissions),
            })
            .await
            .map_err(oc_error)?;
        self.log(&format!(
            "session/new -> {} in {}",
            session.id,
            directory.display()
        ));
        Ok(acp::NewSessionResponse::new(session.id).models(self.model_state()))
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> acp::Result<acp::LoadSessionResponse> {
        // No history replay yet: the TUI continues on the opencode session id.
        self.log(&format!("session/load {} (no replay)", args.session_id.0));
        Ok(acp::LoadSessionResponse::new().models(self.model_state()))
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> acp::Result<acp::SetSessionModeResponse> {
        self.log(&format!("session/set_mode {} (ignored)", args.mode_id.0));
        Ok(acp::SetSessionModeResponse::new())
    }

    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> acp::Result<acp::SetSessionModelResponse> {
        self.log(&format!("session/set_model {} (ignored)", args.model_id.0));
        Ok(acp::SetSessionModelResponse::new())
    }

    async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        let text = prompt_text(&args.prompt);
        let prompt_id = args.meta.as_ref().and_then(|m| m.get("promptId")).cloned();
        // Echo the prompt id so the TUI attributes chunks to this turn.
        let meta = prompt_id.map(|id| {
            let mut meta = acp::Meta::new();
            meta.insert("promptId".into(), id);
            meta
        });
        let sid = args.session_id.0.to_string();
        self.log(&format!("session/prompt {sid} ({} chars)", text.len()));
        let cancel = CancellationToken::new();
        self.active.borrow_mut().insert(sid.clone(), cancel.clone());
        let result = self.run_turn(&args.session_id, &text, meta, cancel).await;
        self.active.borrow_mut().remove(&sid);
        self.log(&format!("session/prompt {sid} -> {result:?}"));
        result.map(acp::PromptResponse::new)
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        let sid = args.session_id.0.to_string();
        let token = self.active.borrow().get(&sid).cloned();
        self.log(&format!(
            "session/cancel {sid} (turn running: {})",
            token.is_some()
        ));
        if let Some(token) = token {
            token.cancel();
        }
        match self.client.interrupt(&sid).await {
            Ok(resp) => self.log(&format!("interrupt -> interrupted={}", resp.interrupted)),
            Err(err) => tracing::warn!(error = %err, "opencode interrupt failed"),
        }
        Ok(())
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        let (result, label) = stub_ext_method(&args.method);
        self.log(&format!(
            "ext_method {} params={} -> {label}",
            args.method,
            truncate(args.params.get(), 200)
        ));
        result
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        self.log(&format!(
            "ext_notification {} params={} -> ignored",
            args.method,
            truncate(args.params.get(), 200)
        ));
        Ok(())
    }
}

/// Stub for `x.ai/*` methods: method-not-found everywhere, which the TUI already
/// treats as "agent does not support this".
fn stub_ext_method(_method: &str) -> (acp::Result<acp::ExtResponse>, &'static str) {
    (Err(acp::Error::method_not_found()), "method_not_found")
}

fn permission_options() -> Vec<acp::PermissionOption> {
    vec![
        acp::PermissionOption::new(
            "allow-once",
            "Allow once",
            acp::PermissionOptionKind::AllowOnce,
        ),
        acp::PermissionOption::new(
            "allow-always",
            "Always allow",
            acp::PermissionOptionKind::AllowAlways,
        ),
        acp::PermissionOption::new("reject", "Reject", acp::PermissionOptionKind::RejectOnce),
    ]
}

fn tool_kind(name: &str) -> acp::ToolKind {
    match name {
        "shell" | "bash" => acp::ToolKind::Execute,
        _ => acp::ToolKind::Other,
    }
}

fn prompt_text(blocks: &[acp::ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(t) => parts.push(t.text.clone()),
            acp::ContentBlock::ResourceLink(link) => parts.push(link.uri.clone()),
            _ => {}
        }
    }
    parts.join("\n")
}

fn compact(value: &serde_json::Value) -> String {
    truncate(&value.to_string(), 120)
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", s.get(..idx).unwrap_or_default()),
        None => s.to_owned(),
    }
}

fn acp_error(message: &str) -> acp::Error {
    acp::Error::new(acp::ErrorCode::InternalError.into(), message.to_owned())
}

fn oc_error(err: oc_opencode_client::Error) -> acp::Error {
    acp_error(&format!("opencode: {err}"))
}

/// [`ext_log`] with the time since `epoch` as prefix.
fn log_since(epoch: Instant, line: &str) {
    ext_log(&format!("+{:.3}s {line}", epoch.elapsed().as_secs_f64()));
}

/// Emit one diagnostic line at `debug` level (target `oc_shim`, visible with
/// `--debug-file`) and, if env `OC_SHIM_LOG=<path>` is set, append it there.
/// No file is written otherwise.
fn ext_log(line: &str) {
    static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    tracing::debug!(target: "oc_shim", "{line}");
    let path = LOG_PATH.get_or_init(|| {
        std::env::var_os("OC_SHIM_LOG")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    });
    if let Some(path) = path
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
#[path = "opencode_shim_tests.rs"]
mod tests;
