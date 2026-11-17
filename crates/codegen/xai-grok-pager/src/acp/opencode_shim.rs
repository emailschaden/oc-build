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
//! The model list comes from `GET /api/model` and `session/set_model` switches the
//! opencode session's model. A model's `variants` named after reasoning-effort levels are
//! offered as its effort menu (`_meta.reasoningEfforts`), and the effort sent with
//! `session/set_model` selects the matching variant. `session.model.selected` during a turn
//! becomes an `x.ai/session/update` `ModelChanged`. Session modes are opencode's primary
//! agents (`GET /api/agent`): `build` is the pager's `default` mode and `plan` its `plan`
//! mode; `session/set_mode` switches the session agent. Always-approve (`_meta.yoloMode` on `session/new`,
//! `x.ai/yolo_mode_changed`) makes the shim answer `permission.asked` with `once` itself. A rejected permission is replied with a message (the
//! user's feedback or [`DEFAULT_REJECT_MESSAGE`]) so the model continues the turn.
//!
//! `x.ai/*` extension calls are answered with stubs. Each call and turn step is
//! logged at debug level (target `oc_shim`) and, when env `OC_SHIM_LOG=<path>`
//! is set, appended to that file (see [`ext_log`]) so the set the TUI really
//! uses can be audited.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use agent_client_protocol::{self as acp, Client as _};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use oc_opencode_client::{
    AgentInfo, Client, CreateSession, Decision, Effect, Event, Location, Model, ModelVariant,
    PermissionRule, tool_text,
};
use tokio_util::sync::CancellationToken;
use xai_acp_lib::{AcpGatewayReceiver, AcpGatewaySender, acp_channels};
use xai_grok_shell::extensions::notification::{
    SessionNotification as XaiSessionNotification, SessionUpdate as XaiSessionUpdate,
};
use xai_grok_shell::sampling::types::{
    REASONING_EFFORT_META_KEY, REASONING_EFFORTS_META_KEY, ReasoningEffort,
    SUPPORTS_REASONING_EFFORT_META_KEY, effort_label, parse_reasoning_effort_meta,
    parse_reasoning_efforts_meta,
};
use xai_grok_tools::types::SessionMode;
use xai_grok_tools::types::output::{BashOutput, ToolOutput};

use super::spawn::{SpawnedAgent, spawn_runtime_thread};
use super::{AcpConnection, AgentEndpoint, ConnectFlags, initialize_connection};

/// Default model when `--opencode-model` is not given.
pub const DEFAULT_MODEL: &str = "opencode/big-pickle";
/// Server URL when neither `--opencode-url`, `OPENCODE_URL` nor `[opencode] url` is set
/// (`opencode serve`'s default port).
pub const DEFAULT_URL: &str = "http://127.0.0.1:4096";
/// Auth method advertised so the pager's eager-auth step succeeds without a login screen
/// (an empty list is treated as "needs login").
const AUTH_METHOD_ID: &str = "opencode";
/// Sent with a reject that has no typed feedback. opencode ends the run on a bare
/// reject; with a message the model gets it as the tool error and continues.
const DEFAULT_REJECT_MESSAGE: &str =
    "The user rejected this tool call. Do not retry it; ask the user how to proceed.";
/// Shown in the rejected tool block.
const REJECTED_LABEL: &str = "Rejected by user";
/// opencode's default primary agent; the pager calls it the `default` mode.
const BUILD_AGENT: &str = "build";
/// Turns the plan from a post-turn approve (`_meta.executePlan`) into a build prompt.
const EXECUTE_PLAN_PROMPT: &str = "The user approved the plan. Implement it now.";
/// How long a cancelled turn waits for `session.execution.interrupted` before giving up.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// Builtin slash commands with no opencode equivalent (canonical names). In opencode mode
/// they are left out of the completion menu, and a typed one shows
/// [`unavailable_message`] instead of running.
pub const UNAVAILABLE_COMMANDS: &[&str] = &[
    "login",
    "logout",
    "usage",
    "privacy",
    "feedback",
    "marketplace",
    "hooks",
    "workflows",
    "workflow",
    "remember",
    "context-window",
    // Dictation goes to the xAI voice service.
    "voice",
];
/// Error message for the `x.ai/*` list calls of features opencode can back later.
const NOT_YET_MESSAGE: &str = "not available in opencode mode yet";

static ACTIVE: OnceLock<()> = OnceLock::new();

/// Mark the process as running against opencode. Called first thing by every entry point
/// that starts the shim (TUI and headless), so it is always set in a real run.
pub fn set_active() {
    let _ = ACTIVE.set(());
}

/// True once the TUI or headless run has started; opencode is the only backend.
/// TODO(oc-build PR b): drop this flag together with the xAI-only features it hides. It
/// stays a runtime flag (not `true`) only so their unit tests, deleted in (b), keep running.
pub fn is_active() -> bool {
    ACTIVE.get().is_some()
}

/// True for a builtin command (canonical name) that has no opencode equivalent.
pub fn is_unavailable_command(name: &str) -> bool {
    UNAVAILABLE_COMMANDS.contains(&name)
}

/// Shown when the user types a command from [`UNAVAILABLE_COMMANDS`] in full.
pub fn unavailable_message(name: &str) -> String {
    format!("/{name} is not available in opencode mode")
}

/// True for a pager effect that only calls an xAI-only `x.ai/*` method (account, billing,
/// consent, suggestions, bundle sync, marketplace, hooks, workflows), or that would save an
/// opencode model id or effort into `~/.grok/config.toml`, which the built-in agent shares
/// (preferred model, `default_model`, `fork_secondary_model`). In opencode mode the effect
/// runner drops these, so they never reach the shim or the config file.
pub(crate) fn skips_effect(effect: &crate::app::actions::Effect) -> bool {
    use crate::app::actions::Effect;
    matches!(
        effect,
        Effect::CheckSubscription { .. }
            | Effect::HydrateTeamCapability { .. }
            | Effect::CreditLimitRecheck { .. }
            // Background refreshes only; a usage modal waiting on a nonce still gets its error.
            | Effect::FetchBilling { silent: true, nonce: 0, .. }
            | Effect::FetchAppBilling { nonce: 0 }
            | Effect::RecordConsentUpstream { .. }
            | Effect::FetchShellSuggestions { .. }
            | Effect::FetchPromptSuggestion { .. }
            | Effect::FetchBundleStatus
            | Effect::FetchMarketplaceList { .. }
            | Effect::FetchPluginCtaCatalog { .. }
            | Effect::CheckMarketplaceUpdates { .. }
            | Effect::FetchHooksList { .. }
            | Effect::FetchWorkflowsList { .. }
            | Effect::PersistPreferredModel { .. }
            | Effect::PersistSetting {
                key: "default_model" | "fork_secondary_model",
                ..
            }
    )
}

/// Where and how to reach the opencode server.
#[derive(Debug, Clone)]
pub struct OpencodeConfig {
    pub url: String,
    pub password: Option<String>,
    /// `provider/id`.
    pub model: String,
}

/// The server URL: `cli` (`--opencode-url` or `OPENCODE_URL`, as clap merges them), then
/// `config` (`[opencode] url`), then [`DEFAULT_URL`]. Blank values are skipped.
pub fn resolve_server_url(cli: Option<&str>, config: Option<&str>) -> String {
    [cli, config]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|u| !u.is_empty())
        .unwrap_or(DEFAULT_URL)
        .to_owned()
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

/// Start the shim on its own runtime thread and return the raw ACP channel pair
/// (`headless` drives it directly).
pub(crate) async fn spawn_opencode_shim(
    cancel: &CancellationToken,
    config: OpencodeConfig,
) -> Result<SpawnedAgent> {
    let model = parse_model_id(&config.model).context("--opencode-model must be provider/id")?;
    let client = Client::new(config.url.clone(), config.password.clone())
        .context("failed to build the opencode client")?;
    // Fail at startup, not at the first session, when the server is down or the password is wrong.
    client.list_agents().await.map_err(|err| {
        anyhow::anyhow!(
            "can't reach the opencode server at {}: {err}\n\
             Start it with `opencode serve`, or point --opencode-url / OPENCODE_URL / \
             [opencode] url at a running server (password from OPENCODE_PASSWORD).",
            config.url
        )
    })?;
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
                catalog: RefCell::new(None),
                session_models: RefCell::new(HashMap::new()),
                modes: RefCell::new(None),
                session_modes: RefCell::new(HashMap::new()),
                yolo: RefCell::new(HashSet::new()),
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
    /// `GET /api/model`, fetched once.
    catalog: RefCell<Option<Vec<acp::ModelInfo>>>,
    /// Model id (`provider/id`) chosen via `session/set_model`, keyed by session id.
    session_models: RefCell<HashMap<String, String>>,
    /// Session modes from `GET /api/agent`, fetched once.
    modes: RefCell<Option<Vec<acp::SessionMode>>>,
    /// Current mode id per session (absent means `default`).
    session_modes: RefCell<HashMap<String, String>>,
    /// Sessions in always-approve: `permission.asked` is answered `once` without the TUI.
    yolo: RefCell<HashSet<String>>,
    /// Cancel token of the running turn, keyed by session id.
    active: RefCell<HashMap<String, CancellationToken>>,
    started: Instant,
}

impl OpencodeShim {
    /// The model picker state for `session_id` (or a new session when `None`).
    async fn model_state(&self, session_id: Option<&str>) -> acp::SessionModelState {
        let current = session_id
            .and_then(|sid| self.session_models.borrow().get(sid).cloned())
            .unwrap_or_else(|| self.model_label.clone());
        acp::SessionModelState::new(current, self.available_models().await)
    }

    async fn available_models(&self) -> Vec<acp::ModelInfo> {
        if let Some(models) = self.catalog.borrow().as_ref() {
            return models.clone();
        }
        let listed = match self.client.list_models().await {
            Ok(listed) => listed,
            Err(err) => {
                // Not cached, so a later session retries.
                tracing::warn!(error = %err, "opencode list_models failed");
                return model_infos(&[], &self.model_label);
            }
        };
        let models = model_infos(&listed, &self.model_label);
        self.log(&format!("models: {} listed", models.len()));
        *self.catalog.borrow_mut() = Some(models.clone());
        models
    }

    /// The mode picker state for `session_id` (or a new session when `None`). `None` when
    /// the server lists no usable agents, so the pager keeps its built-in Normal/Plan.
    async fn mode_state(&self, session_id: Option<&str>) -> Option<acp::SessionModeState> {
        let available = self.available_modes().await;
        if available.is_empty() {
            return None;
        }
        let current = session_id
            .and_then(|sid| self.session_modes.borrow().get(sid).cloned())
            .unwrap_or_else(|| SessionMode::Default.as_id().to_owned());
        Some(acp::SessionModeState::new(current, available))
    }

    async fn available_modes(&self) -> Vec<acp::SessionMode> {
        if let Some(modes) = self.modes.borrow().as_ref() {
            return modes.clone();
        }
        let agents = match self.client.list_agents().await {
            Ok(agents) => agents,
            Err(err) => {
                // Not cached, so a later session retries.
                tracing::warn!(error = %err, "opencode list_agents failed");
                return Vec::new();
            }
        };
        let modes = session_modes(&agents);
        self.log(&format!(
            "modes: {} of {} agents",
            modes.len(),
            agents.len()
        ));
        *self.modes.borrow_mut() = Some(modes.clone());
        modes
    }

    /// Switch the session's opencode agent to the one behind `mode_id` and confirm the
    /// mode to the TUI.
    async fn switch_mode(&self, session_id: &acp::SessionId, mode_id: &str) -> acp::Result<()> {
        let sid = session_id.0.to_string();
        let agent = agent_for_mode(mode_id);
        self.client
            .switch_agent(&sid, agent)
            .await
            .map_err(oc_error)?;
        self.log(&format!(
            "session/set_mode {sid} {mode_id} -> agent {agent}"
        ));
        self.session_modes
            .borrow_mut()
            .insert(sid, mode_id.to_owned());
        self.notify(
            session_id,
            &None,
            acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(mode_id.to_owned())),
        );
        Ok(())
    }

    /// opencode switched the session's model (another client, or opencode itself): record it
    /// and tell the TUI with a `ModelChanged`, effort from the selected variant.
    async fn on_model_selected(
        &self,
        session_id: &acp::SessionId,
        model_id: String,
        variant: Option<&str>,
    ) {
        let models = self.available_models().await;
        let effort = models
            .iter()
            .find(|m| m.model_id.0.as_ref() == model_id)
            .and_then(|info| effort_for_variant(info, variant?));
        self.log(&format!(
            "model selected {model_id} variant={variant:?} -> effort {effort:?}"
        ));
        self.session_models
            .borrow_mut()
            .insert(session_id.0.to_string(), model_id.clone());
        let payload = XaiSessionNotification {
            session_id: session_id.clone(),
            update: XaiSessionUpdate::model_changed(
                model_id,
                effort.map(|e| e.as_ref().to_owned()),
                None,
            ),
            meta: None,
        };
        match serde_json::value::to_raw_value(&payload) {
            Ok(params) => {
                self.gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/session/update",
                        params.into(),
                    ));
            }
            Err(err) => tracing::warn!(error = %err, "ModelChanged serialize failed"),
        }
    }

    fn current_mode(&self, sid: &str) -> String {
        self.session_modes
            .borrow()
            .get(sid)
            .cloned()
            .unwrap_or_else(|| SessionMode::Default.as_id().to_owned())
    }

    fn set_yolo(&self, sid: &str, on: bool) {
        let changed = if on {
            self.yolo.borrow_mut().insert(sid.to_owned())
        } else {
            self.yolo.borrow_mut().remove(sid)
        };
        if changed {
            self.log(&format!("always-approve {sid} -> {on}"));
        }
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
                TurnStep::Update(update) => {
                    if let acp::SessionUpdate::CurrentModeUpdate(mode) = &update {
                        self.session_modes
                            .borrow_mut()
                            .insert(sid.clone(), mode.current_mode_id.0.to_string());
                    }
                    self.notify(session_id, &meta, update);
                }
                TurnStep::ModelSelected { model_id, variant } => {
                    self.on_model_selected(session_id, model_id, variant.as_deref())
                        .await;
                }
                TurnStep::Permission { request_id, .. } if self.yolo.borrow().contains(&sid) => {
                    // Always-approve: the TUI would answer allow-once anyway; skip the round trip.
                    self.log(&format!("permission {request_id} -> Once (always-approve)"));
                    if let Err(err) = self
                        .client
                        .reply_permission(&sid, &request_id, Decision::Once, None)
                        .await
                    {
                        tracing::warn!(error = %err, "opencode reply_permission failed");
                    }
                }
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
        let (decision, message) = match self.gateway.request_permission(request).await {
            Ok(resp) => permission_reply(&resp),
            Err(err) => {
                tracing::warn!(error = %err, "request_permission failed; rejecting");
                (Decision::Reject, None)
            }
        };
        self.log(&format!(
            "permission {request_id} -> {decision:?} message={message:?}"
        ));
        if let Err(err) = self
            .client
            .reply_permission(&sid, &request_id, decision, message.as_deref())
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
    /// opencode selected another model for the session (`provider/id`, variant id).
    ModelSelected {
        model_id: String,
        variant: Option<String>,
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
    /// Set once a tool call was rejected by the user.
    rejected: bool,
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
            rejected: false,
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
                // A bare reject makes opencode end the run with `shutdown`; that is
                // the user's answer, not a cancelled turn.
                if self.rejected && !self.cancelling && reason.as_deref() == Some("shutdown") {
                    TurnStep::Stop(acp::StopReason::EndTurn)
                } else {
                    TurnStep::Stop(acp::StopReason::Cancelled)
                }
            }
            Event::ExecutionFailed { error, .. } => TurnStep::Fail(
                error
                    .map(|e| format!("{}: {}", e.kind, e.message))
                    .unwrap_or_else(|| "unknown error".to_owned()),
            ),
            Event::Idle { .. } if self.started => TurnStep::Stop(self.end_reason()),
            // Another client (or opencode itself) switched the agent mid-turn.
            Event::AgentSelected { agent, .. } => {
                TurnStep::Update(acp::SessionUpdate::CurrentModeUpdate(
                    acp::CurrentModeUpdate::new(mode_for_agent(&agent)),
                ))
            }
            Event::ModelSelected { model, .. } => TurnStep::ModelSelected {
                model_id: format!("{}/{}", model.provider_id, model.id),
                variant: model.variant,
            },
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
            Event::ToolFailed { id, error, .. } if error.kind == "permission.rejected" => {
                self.rejected = true;
                let mut output = REJECTED_LABEL.to_owned();
                if !error.message.is_empty() && error.message != DEFAULT_REJECT_MESSAGE {
                    output.push_str(": ");
                    output.push_str(&error.message);
                }
                self.log(&format!("tool_rejected {id}"));
                let update = self.tool_result(&id, &output, false);
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

/// Map the TUI's permission answer to an opencode reply. Anything but an allow
/// option rejects. A selected reject carries a message so the model continues:
/// the TUI's typed feedback (`_meta.followup_message`) or
/// [`DEFAULT_REJECT_MESSAGE`]. A cancelled modal (Esc, turn cancel) rejects bare,
/// which ends the opencode run.
fn permission_reply(resp: &acp::RequestPermissionResponse) -> (Decision, Option<String>) {
    let acp::RequestPermissionOutcome::Selected(selected) = &resp.outcome else {
        return (Decision::Reject, None);
    };
    match selected.option_id.0.as_ref() {
        "allow-once" => (Decision::Once, None),
        "allow-always" => (Decision::Always, None),
        _ => {
            let feedback = resp
                .meta
                .as_ref()
                .and_then(|m| m.get("followup_message"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let message = feedback.unwrap_or(DEFAULT_REJECT_MESSAGE);
            (Decision::Reject, Some(message.to_owned()))
        }
    }
}

/// Split a picker model id `provider/id`.
fn parse_model_id(model_id: &str) -> Option<Model> {
    let (provider_id, id) = model_id.split_once('/')?;
    (!provider_id.is_empty() && !id.is_empty()).then(|| Model {
        id: id.to_owned(),
        provider_id: provider_id.to_owned(),
    })
}

/// The pager's mode id for an opencode agent: `build` is `default`, others keep their id
/// (`plan` is already the pager's plan mode).
fn mode_for_agent(agent: &str) -> String {
    if agent == BUILD_AGENT {
        SessionMode::Default.as_id().to_owned()
    } else {
        agent.to_owned()
    }
}

/// The opencode agent behind a pager mode id; inverse of [`mode_for_agent`].
fn agent_for_mode(mode_id: &str) -> &str {
    if mode_id == SessionMode::Default.as_id() {
        BUILD_AGENT
    } else {
        mode_id
    }
}

/// Session modes for the pager: opencode's selectable (primary, not hidden) agents, `build`
/// first so it is the cycle's start. Subagents and the internal compaction/title/summary
/// agents are left out.
fn session_modes(agents: &[AgentInfo]) -> Vec<acp::SessionMode> {
    let mut modes: Vec<acp::SessionMode> = agents
        .iter()
        .filter(|a| !a.hidden && matches!(a.mode.as_str(), "primary" | "all"))
        .map(|a| {
            let name = if a.name.is_empty() {
                a.id.clone()
            } else {
                a.name.clone()
            };
            acp::SessionMode::new(mode_for_agent(&a.id), name).description(a.description.clone())
        })
        .collect();
    let default = SessionMode::Default.as_id();
    modes.sort_by_key(|m| m.id.0.as_ref() != default);
    modes
}

/// The build prompt for a post-turn plan approve: the pager sends empty text with the
/// plan in `_meta.executePlan.planFileContent`. `None` for any other prompt.
fn execute_plan_prompt(meta: Option<&acp::Meta>) -> Option<String> {
    let plan = meta?.get("executePlan")?;
    let content = plan
        .get("planFileContent")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or_default();
    Some(if content.is_empty() {
        EXECUTE_PLAN_PROMPT.to_owned()
    } else {
        format!("{EXECUTE_PLAN_PROMPT}\n\n{content}")
    })
}

/// `(session id, always-approve on)` from an `x.ai/yolo_mode_changed` notification.
/// `None` when the sender left `yolo_mode` out (it only speaks for another setting).
fn yolo_change(params: &serde_json::Value) -> Option<(String, bool)> {
    let sid = params.get("sessionId")?.as_str()?;
    let on = params.get("yolo_mode")?.as_bool()?;
    Some((sid.to_owned(), on))
}

/// The picker entries for opencode's model list, ids `provider/id`. `configured`
/// is kept (first) when the server does not list it, so `current` always resolves.
fn model_infos(listed: &[oc_opencode_client::ModelInfo], configured: &str) -> Vec<acp::ModelInfo> {
    let mut models: Vec<acp::ModelInfo> = listed
        .iter()
        .map(|m| {
            let id = format!("{}/{}", m.provider_id, m.id);
            let name = if m.name.is_empty() {
                id.clone()
            } else {
                m.name.clone()
            };
            let mut meta = acp::Meta::new();
            if let Some(tokens) = m.limit.as_ref().and_then(|l| l.context) {
                meta.insert("totalContextTokens".into(), tokens.into());
            }
            let efforts = effort_options(&m.variants);
            if !efforts.is_empty() {
                meta.insert(SUPPORTS_REASONING_EFFORT_META_KEY.into(), true.into());
                meta.insert(REASONING_EFFORTS_META_KEY.into(), efforts.into());
            }
            let meta = (!meta.is_empty()).then_some(meta);
            acp::ModelInfo::new(id.clone(), name)
                .description(id)
                .meta(meta)
        })
        .collect();
    if !models.iter().any(|m| m.model_id.0.as_ref() == configured) {
        models.insert(
            0,
            acp::ModelInfo::new(configured.to_owned(), configured.to_owned()),
        );
    }
    models
}

/// The pager's effort menu for a model's opencode variants (`_meta.reasoningEfforts` entries):
/// variants named after a reasoning-effort level (`low`, `high`, `max`, …), or whose
/// `settings.reasoningEffort` is one, in server order. The first variant per level wins;
/// the rest (e.g. `default`) are left out.
fn effort_options(variants: &[ModelVariant]) -> Vec<serde_json::Value> {
    let mut seen = Vec::new();
    variants
        .iter()
        .filter_map(|v| {
            let effort = v.id.parse::<ReasoningEffort>().ok().or_else(|| {
                v.settings
                    .get(REASONING_EFFORT_META_KEY)?
                    .as_str()?
                    .parse()
                    .ok()
            })?;
            (!seen.contains(&effort)).then(|| {
                seen.push(effort);
                let description = crate::slash::commands::effort_levels::effort_description(effort);
                let mut option = serde_json::Map::new();
                option.insert("id".into(), v.id.clone().into());
                option.insert("value".into(), effort.as_ref().into());
                option.insert("description".into(), description.into());
                if v.id == effort.as_ref() {
                    option.insert("label".into(), effort_label(effort).into());
                }
                serde_json::Value::Object(option)
            })
        })
        .collect()
}

/// The opencode variant behind `effort` on a picker model, if it offers that level.
fn variant_for_effort(info: &acp::ModelInfo, effort: ReasoningEffort) -> Option<String> {
    parse_reasoning_efforts_meta(info.meta.as_ref())?
        .into_iter()
        .find(|opt| opt.value == effort)
        .map(|opt| opt.id)
}

/// The effort level of opencode variant `variant` on a picker model; inverse of
/// [`variant_for_effort`]. `None` for `default` and other unmapped variants.
fn effort_for_variant(info: &acp::ModelInfo, variant: &str) -> Option<ReasoningEffort> {
    parse_reasoning_efforts_meta(info.meta.as_ref())?
        .into_iter()
        .find(|opt| opt.id == variant)
        .map(|opt| opt.value)
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for ShimAgent {
    async fn initialize(
        &self,
        _args: acp::InitializeRequest,
    ) -> acp::Result<acp::InitializeResponse> {
        self.log("initialize");
        let meta = serde_json::json!({
            "modelState": self.model_state(None).await,
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
        let yolo = args
            .meta
            .as_ref()
            .and_then(|m| m.get("yoloMode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.set_yolo(&session.id, yolo);
        self.log(&format!(
            "session/new -> {} in {}",
            session.id,
            directory.display()
        ));
        Ok(acp::NewSessionResponse::new(session.id)
            .models(self.model_state(None).await)
            .modes(self.mode_state(None).await))
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> acp::Result<acp::LoadSessionResponse> {
        // No history replay yet: the TUI continues on the opencode session id.
        self.log(&format!("session/load {} (no replay)", args.session_id.0));
        let models = self.model_state(Some(&args.session_id.0)).await;
        let modes = self.mode_state(Some(&args.session_id.0)).await;
        Ok(acp::LoadSessionResponse::new().models(models).modes(modes))
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> acp::Result<acp::SetSessionModeResponse> {
        self.switch_mode(&args.session_id, &args.mode_id.0).await?;
        Ok(acp::SetSessionModeResponse::new())
    }

    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> acp::Result<acp::SetSessionModelResponse> {
        let sid = args.session_id.0.to_string();
        let label = args.model_id.0.to_string();
        let model = parse_model_id(&label).ok_or_else(|| {
            acp::Error::invalid_params().data(format!("model id {label:?} is not provider/id"))
        })?;
        let effort = parse_reasoning_effort_meta(args.meta.as_ref());
        let variant = match effort {
            Some(effort) => {
                let models = self.available_models().await;
                let info = models.iter().find(|m| m.model_id.0.as_ref() == label);
                info.and_then(|info| variant_for_effort(info, effort))
            }
            None => None,
        };
        self.client
            .switch_model(&sid, &model, variant.as_deref())
            .await
            .map_err(oc_error)?;
        self.log(&format!(
            "session/set_model {sid} -> {label} effort={effort:?} variant={variant:?}"
        ));
        self.session_models.borrow_mut().insert(sid, label);
        Ok(acp::SetSessionModelResponse::new())
    }

    async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        let mut text = prompt_text(&args.prompt);
        // Plan approve: opencode's plan agent has no exit tool, so leave it here and
        // prompt the build agent with the approved plan.
        if let Some(build) = execute_plan_prompt(args.meta.as_ref()) {
            let default = SessionMode::Default.as_id();
            if self.current_mode(&args.session_id.0) != default {
                self.switch_mode(&args.session_id, default).await?;
            }
            text = build;
        }
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
        let params: serde_json::Value = serde_json::from_str(args.params.get()).unwrap_or_default();
        let handled = match args.method.as_ref() {
            "x.ai/yolo_mode_changed" => yolo_change(&params)
                .map(|(sid, on)| self.set_yolo(&sid, on))
                .is_some(),
            "x.ai/toggle_plan_mode" => match params.get("sessionId").and_then(|v| v.as_str()) {
                Some(sid) => {
                    let plan = SessionMode::Plan.as_id();
                    let next = if self.current_mode(sid) == plan {
                        SessionMode::Default.as_id()
                    } else {
                        plan
                    };
                    let session_id = acp::SessionId::new(sid.to_owned());
                    if let Err(err) = self.switch_mode(&session_id, next).await {
                        tracing::warn!(error = %err, "opencode toggle_plan_mode failed");
                    }
                    true
                }
                None => false,
            },
            _ => false,
        };
        self.log(&format!(
            "ext_notification {} params={} -> {}",
            args.method,
            truncate(args.params.get(), 200),
            if handled { "handled" } else { "ignored" }
        ));
        Ok(())
    }
}

/// Stub for `x.ai/*` methods: method-not-found everywhere, which the TUI already
/// treats as "agent does not support this". The extensions-modal tabs opencode can
/// back later (plugins, skills, MCP) get a message saying so instead of "Method not found".
fn stub_ext_method(method: &str) -> (acp::Result<acp::ExtResponse>, &'static str) {
    match method {
        "x.ai/plugins/list" | "x.ai/skills/list" | "x.ai/mcp/list" => (
            Err(acp::Error::new(
                acp::ErrorCode::MethodNotFound.into(),
                NOT_YET_MESSAGE.to_owned(),
            )),
            "not_yet",
        ),
        _ => (Err(acp::Error::method_not_found()), "method_not_found"),
    }
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
