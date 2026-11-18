//! ACP (Agent Communication Protocol) connection management.
//!
//! This module spawns the agent process, initializes the protocol and provides the channel for communication.
/// Shared plan-file size cap for the pager keep.
pub(crate) const MAX_PLAN_FILE_BYTES: usize = 256 * 1024;
pub mod meta;
pub mod model_state;
// oc-build: opencode backend
pub mod opencode_shim;
pub(crate) mod skills_listing;
pub mod spawn;
pub(crate) mod subagent_label_registry;
mod subagent_message;
pub mod tracker;
/// Ext methods that carry a session-scoped update and may stamp `isReplay`.
/// TUI dispatch, headless dispatch, and the session-load ACP barrier all share this list.
/// A new method thus cannot be handled in one path and classified `Unrelated` in another.
pub(crate) fn is_session_update_ext_method(method: &str) -> bool {
    matches!(method, "x.ai/session_notification" | "x.ai/session/update")
}
use crate::client_identity::{HEADLESS_CLIENT_TYPE, PAGER_CLIENT_TYPE, PAGER_CLIENT_VERSION};
use agent_client_protocol as acp;
use anyhow::Result;
pub use model_state::ModelState;
use std::io::Write;
use tokio_util::sync::CancellationToken;
use xai_acp_lib::{AcpAgentTx, AcpClientRx, acp_send};
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::sampling::types::ReasoningEffort;
use xai_grok_telemetry::process_info::{
    Entrypoint, Interactivity, LeaderMode, ProcessIdentity, set_identity,
};
use xai_grok_telemetry::startup;
pub use xai_grok_telemetry::startup::{
    AgentKind, Owner, StartupOutcome, StartupPhase, StartupTimer,
};
/// Construct a `METHOD_NOT_FOUND` error for `WaitForTerminalExit`.
/// Both the interactive pager and headless mode reject this ACP method (the adapter falls back to polling).
/// Centralised here so the error code and message format stay in sync.
pub(crate) fn wait_for_exit_not_supported(context: &str) -> acp::Error {
    acp::Error::new(
        acp::ErrorCode::MethodNotFound.into(),
        format!("{context} does not handle WaitForTerminalExit"),
    )
}
/// The pager's connection to an agent that is running but has not been set up yet.
/// It holds the channel for sending requests to the agent, the channel for receiving its replies, the token that stops the agent, and the place the agent runs.
/// `initialize_connection` sends `initialize` over these channels and returns the finished `AcpConnection`.
pub(in crate::acp) struct AgentEndpoint {
    pub(in crate::acp) tx: AcpAgentTx,
    pub(in crate::acp) rx: AcpClientRx,
    pub(in crate::acp) cancel: CancellationToken,
    /// The agent's thread in this process. Exit joins it so the agent can finish flushing.
    pub(in crate::acp) thread: std::thread::JoinHandle<Result<()>>,
}
impl From<spawn::SpawnedAgent> for AgentEndpoint {
    fn from(spawned: spawn::SpawnedAgent) -> Self {
        AgentEndpoint {
            tx: spawned.channel.tx,
            rx: spawned.channel.rx,
            cancel: spawned.cancel,
            thread: spawned.thread_handle,
        }
    }
}
/// Result of connecting to an agent.
pub struct AcpConnection {
    /// Send requests to the agent.
    pub tx: AcpAgentTx,
    /// Receive notifications from the agent.
    pub rx: AcpClientRx,
    /// Available models and current selection.
    pub models: ModelState,
    /// Whether the agent is a grok-shell instance.
    pub is_grok_shell: bool,
    /// Cancellation token to stop the agent.
    pub cancel: CancellationToken,
    /// In-process agent worker thread. Join after cancel so the agent can flush.
    pub agent_thread: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    /// ACP-advertised slash commands parsed from `InitializeResponse.meta.availableCommands`.
    /// Seeded into every new `AgentSession` so autocomplete has shell builtins and skills immediately, before any `AvailableCommandsUpdate` arrives.
    pub available_commands: Vec<acp::AvailableCommand>,
    /// Whether cancel-rewind is enabled (resolved by shell from config layers).
    pub cancel_rewind_enabled: bool,
    /// Whether the session-recap feature is rolled out for this connection. The client gates its automatic away-recap
    /// poll and the manual `/recap` on this so a disabled feature produces zero `x.ai/recap` traffic.
    pub session_recap_available: bool,
}
/// CLI flags that affect agent configuration, threaded from PagerArgs.
#[derive(Debug, Clone, Default)]
pub struct ConnectFlags {
    /// `--no-subagents`. Only an explicit flag reaches the CLI tier of the resolver; otherwise env, config.toml, and the default decide, exactly as in `grok agent stdio`.
    pub no_subagents: bool,
    /// CLI memory override set by a legacy compatibility flag.
    pub memory_enabled_override: Option<bool>,
    /// Original compatibility flag spelling for leader-mode warnings.
    pub memory_override_flag: Option<&'static str>,
    pub disable_web_search: bool,
    /// Session-scoped `--todo-gate` override. Forces `ReminderPolicy.todo_gate.enabled = true` for this session.
    pub todo_gate: bool,
    /// Session-scoped `--laziness-debug-log <path>` override. Observation-only (no nudges). Prototype and eval use
    /// only; not persisted to config.toml.
    pub laziness_debug_log: Option<std::path::PathBuf>,
    /// Storage mode override.
    pub storage_mode: Option<String>,
    /// Whether this client will draw a status row, advertised as `x.ai/statusLine` so the agent can skip an unpainted payload.
    pub status_line: bool,
    /// Client identifier for ACP Initialize metadata.
    pub client_identifier: Option<String>,
    /// Hunk tracker mode for ACP Initialize capabilities.
    pub hunk_tracker_mode: Option<String>,
    /// Terminal capability in ACP Initialize.
    pub terminal: bool,
    /// Filesystem read capability in ACP Initialize.
    pub fs_read: bool,
    /// Filesystem write capability in ACP Initialize.
    pub fs_write: bool,
    /// Installer field for config.toml.
    pub installer: Option<String>,
    /// Remote settings from early prefetch (used for memory config resolution).
    pub remote_settings: Option<xai_grok_shell::util::config::RemoteSettings>,
    /// Override the entire system prompt.
    pub system_prompt_override: Option<String>,
    /// Extra rules appended to the system prompt (from `--rules`).
    pub rules: Option<String>,
    /// Override reasoning effort for all models.
    pub reasoning_effort_override: Option<ReasoningEffort>,
    /// CLI permission rules from the --allow and --deny flags.
    /// Not supported in leader mode (agent config is set at leader startup).
    pub permission_rules: Vec<xai_grok_workspace::permission::types::PermissionRule>,
    /// Seed agent sessions with always-approve (YOLO) permission mode.
    pub default_yolo_mode: bool,
    /// Seed agent sessions with auto (classifier) permission mode.
    /// Ignored when `default_yolo_mode` is true.
    pub default_auto_mode: bool,
}
/// Finishes connecting to an agent. Sends it `initialize` and returns the `AcpConnection`.
// oc-build: opencode backend; no login or eager authenticate step.
/// Every agent target is connected through this function, so these steps are written once.
pub(in crate::acp) async fn initialize_connection(
    endpoint: AgentEndpoint,
    flags: &ConnectFlags,
) -> Result<AcpConnection> {
    let AgentEndpoint {
        tx,
        rx,
        cancel,
        thread,
    } = endpoint;
    startup::enter(StartupPhase::AcpInitialize);
    let agent = initialize(&tx, flags).await?;
    Ok(AcpConnection {
        tx,
        rx,
        models: agent.models,
        is_grok_shell: agent.is_grok_shell,
        cancel,
        agent_thread: Some(thread),
        available_commands: agent.available_commands,
        cancel_rewind_enabled: agent.cancel_rewind_enabled,
        session_recap_available: agent.session_recap_available,
    })
}
/// Build the per-session `_meta` for `InitializeRequest`.
fn build_initialize_meta(flags: &ConnectFlags) -> serde_json::Value {
    let client_type = flags
        .client_identifier
        .as_deref()
        .unwrap_or(PAGER_CLIENT_TYPE);
    let mut meta = serde_json::json!({
        "clientType": client_type,
        "clientVersion": PAGER_CLIENT_VERSION,
    });
    if let Some(obj) = meta.as_object_mut() {
        if let Some(spo) = &flags.system_prompt_override {
            obj.insert(
                "systemPromptOverride".into(),
                serde_json::Value::String(spo.clone()),
            );
        }
        if let Some(rules) = &flags.rules {
            obj.insert("rules".into(), serde_json::Value::String(rules.clone()));
        }
    }
    meta
}
/// Build `client_capabilities.meta`.
/// The hunk-tracker mode is canonicalized at this connect read so the agent runs exactly what the settings modal displays.
fn client_capabilities_meta(flags: &ConnectFlags) -> serde_json::Value {
    let hunk_mode =
        crate::settings::canonical_hunk_tracker_mode(flags.hunk_tracker_mode.as_deref());
    let mut meta = serde_json::json!({
        "x.ai/incrementalBashOutput": true,
        "x.ai/hunkTracker": { "mode": hunk_mode },
        "x.ai/bashOutputNoColor": true,
        "x.ai/gitHeadChanged": true,
    });
    if let Some(obj) = meta.as_object_mut() {
        obj.insert(
            xai_grok_shell::session::USER_MESSAGE_ECHO_CAPABILITY.into(),
            true.into(),
        );
        obj.insert(
            xai_grok_status_line::STATUS_LINE_CAPABILITY.into(),
            flags.status_line.into(),
        );
    }
    meta
}
/// The parsed `InitializeResponse`.
pub(crate) struct InitializedAgent {
    pub(crate) models: ModelState,
    pub(crate) is_grok_shell: bool,
    pub(crate) available_commands: Vec<acp::AvailableCommand>,
    pub(crate) cancel_rewind_enabled: bool,
    pub(crate) session_recap_available: bool,
}
/// Send InitializeRequest and parse the response.
async fn initialize(tx: &AcpAgentTx, flags: &ConnectFlags) -> Result<InitializedAgent> {
    let mut meta = build_initialize_meta(flags).as_object().cloned();
    crate::app::session_startup::stamp_phase_traceparent(&mut meta);
    let req = acp::InitializeRequest::new(acp::ProtocolVersion::V1)
        .client_capabilities(
            acp::ClientCapabilities::new()
                .fs(acp::FileSystemCapabilities::new()
                    .read_text_file(flags.fs_read)
                    .write_text_file(flags.fs_write))
                .terminal(flags.terminal)
                .meta(client_capabilities_meta(flags).as_object().cloned()),
        )
        .meta(meta);
    let resp: acp::InitializeResponse = {
        let _timer = xai_grok_telemetry::instrumentation::timer("acp_init.initialize_roundtrip");
        acp_send(req, tx).await?
    };
    let is_grok_shell = resp
        .meta
        .as_ref()
        .and_then(|m| m.get("grokShell"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let models: ModelState = resp
        .meta
        .as_ref()
        .and_then(|m| m.get("modelState"))
        .and_then(|v| serde_json::from_value::<acp::SessionModelState>(v.clone()).ok())
        .into();
    let available_commands = parse_available_commands(resp.meta.as_ref());
    let cancel_rewind_enabled = resp
        .meta
        .as_ref()
        .and_then(|m| m.get("cancelRewind"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let session_recap_available = parse_session_recap_available(resp.meta.as_ref());
    Ok(InitializedAgent {
        models,
        is_grok_shell,
        available_commands,
        cancel_rewind_enabled,
        session_recap_available,
    })
}
/// Parse `availableCommands` from an `InitializeResponse.meta` value.
///
/// Extracted as a standalone function for testability (the full `initialize()` function requires an ACP connection).
pub fn parse_available_commands(meta: Option<&acp::Meta>) -> Vec<acp::AvailableCommand> {
    meta.and_then(|m| m.get("availableCommands"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}
/// Parse `sessionRecap` from `InitializeResponse.meta` (shell rollout gate).
///
/// Default `false` when missing or non-bool so older agents and dark-launch defaults produce zero automatic recap traffic.
pub fn parse_session_recap_available(meta: Option<&acp::Meta>) -> bool {
    meta.and_then(|m| m.get("sessionRecap"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn is_session_update_ext_method_covers_both_carriers() {
        assert!(is_session_update_ext_method("x.ai/session_notification"));
        assert!(is_session_update_ext_method("x.ai/session/update"));
        assert!(!is_session_update_ext_method("x.ai/task_completed"));
        assert!(!is_session_update_ext_method("session/update"));
    }
    #[test]
    fn parse_available_commands_from_meta() {
        let meta = serde_json::json!({
            "availableCommands": [
                {
                    "name": "compact",
                    "description": "Compact conversation history",
                    "input": { "hint": "<focus>" }
                },
                {
                    "name": "flush",
                    "description": "Flush memory"
                }
            ]
        });
        let cmds = parse_available_commands(meta.as_object());
        let [compact, flush] = cmds.as_slice() else {
            panic!("expected 2 commands: {cmds:?}");
        };
        assert_eq!(compact.name, "compact");
        assert_eq!(compact.description, "Compact conversation history");
        assert!(compact.input.is_some());
        assert_eq!(flush.name, "flush");
        assert!(flush.input.is_none());
    }
    #[test]
    fn parse_available_commands_missing_key_returns_empty() {
        let meta = serde_json::json!({ "grokShell": true });
        let cmds = parse_available_commands(meta.as_object());
        assert!(cmds.is_empty());
    }
    #[test]
    fn parse_available_commands_none_meta_returns_empty() {
        let cmds = parse_available_commands(None);
        assert!(cmds.is_empty());
    }
    #[test]
    fn parse_available_commands_invalid_json_returns_empty() {
        let meta = serde_json::json!({
            "availableCommands": "not-an-array"
        });
        let cmds = parse_available_commands(meta.as_object());
        assert!(cmds.is_empty());
    }
    #[test]
    fn parse_session_recap_available_true() {
        let meta = serde_json::json!({ "sessionRecap": true });
        assert!(parse_session_recap_available(meta.as_object()));
    }
    #[test]
    fn parse_session_recap_available_false_explicit() {
        let meta = serde_json::json!({ "sessionRecap": false });
        assert!(!parse_session_recap_available(meta.as_object()));
    }
    #[test]
    fn parse_session_recap_available_defaults_off_when_missing() {
        let meta = serde_json::json!({ "grokShell": true, "cancelRewind": true });
        assert!(!parse_session_recap_available(meta.as_object()));
        assert!(!parse_session_recap_available(None));
    }
    #[test]
    fn parse_session_recap_available_non_bool_defaults_off() {
        let meta = serde_json::json!({ "sessionRecap": "yes" });
        assert!(!parse_session_recap_available(meta.as_object()));
    }
    #[test]
    fn build_initialize_meta_includes_rules_when_set() {
        let flags = ConnectFlags {
            rules: Some("Always reply in French.".into()),
            ..Default::default()
        };
        let meta = build_initialize_meta(&flags);
        assert_eq!(
            meta.get("rules").and_then(|v| v.as_str()),
            Some("Always reply in French.")
        );
    }
    #[test]
    fn build_initialize_meta_omits_rules_when_unset() {
        let flags = ConnectFlags::default();
        let meta = build_initialize_meta(&flags);
        assert!(
            meta.get("rules").is_none(),
            "rules key must be absent when --rules is not set; meta={meta:?}"
        );
    }
    #[test]
    fn build_initialize_meta_carries_system_prompt_override() {
        let flags = ConnectFlags {
            system_prompt_override: Some("YOU ARE A PIRATE.".into()),
            ..Default::default()
        };
        let meta = build_initialize_meta(&flags);
        assert_eq!(
            meta.get("systemPromptOverride").and_then(|v| v.as_str()),
            Some("YOU ARE A PIRATE.")
        );
    }
    #[test]
    fn build_initialize_meta_uses_custom_client_identifier_when_set() {
        let flags = ConnectFlags {
            client_identifier: Some("zed".into()),
            ..Default::default()
        };
        let meta = build_initialize_meta(&flags);
        assert_eq!(meta.get("clientType").and_then(|v| v.as_str()), Some("zed"));
    }
    #[test]
    fn client_capabilities_meta_defaults_absent_or_blank_mode_to_off() {
        let absent = client_capabilities_meta(&ConnectFlags::default());
        assert_eq!(
            absent
                .get("x.ai/hunkTracker")
                .and_then(|v| v.get("mode"))
                .and_then(|v| v.as_str()),
            Some("off")
        );
        let blank = client_capabilities_meta(&ConnectFlags {
            hunk_tracker_mode: Some("   ".into()),
            ..Default::default()
        });
        assert_eq!(
            blank
                .get("x.ai/hunkTracker")
                .and_then(|v| v.get("mode"))
                .and_then(|v| v.as_str()),
            Some("off")
        );
    }
    #[test]
    fn client_capabilities_meta_always_requests_user_message_echo() {
        let meta = client_capabilities_meta(&ConnectFlags::default());
        assert_eq!(
            meta.get(xai_grok_shell::session::USER_MESSAGE_ECHO_CAPABILITY)
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }
    /// The agent gates the whole payload on this key, so a misspelling on either side switches the feature off with nothing to show for it.
    #[test]
    fn client_capabilities_meta_advertises_the_status_line_the_config_asked_for() {
        let key = xai_grok_status_line::STATUS_LINE_CAPABILITY;
        for wants_a_row in [true, false] {
            let meta = client_capabilities_meta(&ConnectFlags {
                status_line: wants_a_row,
                ..Default::default()
            });
            assert_eq!(
                meta.get(key).and_then(|v| v.as_bool()),
                Some(wants_a_row),
                "status_line={wants_a_row}"
            );
        }
    }
    #[test]
    fn client_capabilities_meta_canonicalizes_off_and_mixed_case() {
        for raw in ["off", "OFF", "Disabled"] {
            let meta = client_capabilities_meta(&ConnectFlags {
                hunk_tracker_mode: Some(raw.into()),
                ..Default::default()
            });
            assert_eq!(
                meta.get("x.ai/hunkTracker")
                    .and_then(|v| v.get("mode"))
                    .and_then(|v| v.as_str()),
                Some("off"),
                "raw={raw}"
            );
        }
    }
}
