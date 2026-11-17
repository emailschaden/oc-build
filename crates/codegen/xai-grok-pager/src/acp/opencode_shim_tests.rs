// Copyright 2026 The oc-build Authors
// SPDX-License-Identifier: Apache-2.0

use super::*;

const SID: &str = "ses_e1719cc3bffeM1A50AZWoIbZ67";
const MSG: &str = "msg_1e8e65cae001kU7Oy6xfXRGsMV";
const TOOL: &str = "chatcmpl-tool-b85c4424842a3387";

fn turn() -> TurnState {
    TurnState::new(SID.to_owned(), "/tmp/ocproj".to_owned(), Instant::now())
}

/// Build an event from an SSE envelope with the given `type` and `data`.
fn event(kind: &str, data: serde_json::Value) -> Event {
    let envelope = serde_json::json!({ "id": "evt_test", "type": kind, "data": data });
    Event::parse(&envelope.to_string()).expect("valid json")
}

fn text_of(chunk: &acp::ContentChunk) -> &str {
    match &chunk.content {
        acp::ContentBlock::Text(t) => &t.text,
        other => panic!("expected text, got {other:?}"),
    }
}

fn update(step: TurnStep) -> acp::SessionUpdate {
    match step {
        TurnStep::Update(update) => update,
        other => panic!("expected an update, got {other:?}"),
    }
}

fn shell_called(turn: &mut TurnState) -> TurnStep {
    turn.on_event(event(
        "session.tool.input.started",
        serde_json::json!({ "sessionID": SID, "assistantMessageID": MSG, "id": TOOL, "name": "shell" }),
    ));
    turn.on_event(event(
        "session.tool.called",
        serde_json::json!({
            "sessionID": SID, "assistantMessageID": MSG, "id": TOOL,
            "input": { "command": "echo hello-from-probe" }, "executed": false,
        }),
    ))
}

fn tool_update(step: TurnStep) -> acp::ToolCallUpdate {
    match update(step) {
        acp::SessionUpdate::ToolCallUpdate(u) => u,
        other => panic!("expected ToolCallUpdate, got {other:?}"),
    }
}

fn content_text(u: &acp::ToolCallUpdate) -> String {
    u.fields
        .content
        .iter()
        .flatten()
        .filter_map(|c| match c {
            acp::ToolCallContent::Content(c) => match &c.content {
                acp::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn text_delta_becomes_agent_message_chunk() {
    let step = turn().on_event(event(
        "session.text.delta",
        serde_json::json!({ "sessionID": SID, "assistantMessageID": MSG, "ordinal": 0, "delta": "done" }),
    ));
    match update(step) {
        acp::SessionUpdate::AgentMessageChunk(chunk) => assert_eq!(text_of(&chunk), "done"),
        other => panic!("expected AgentMessageChunk, got {other:?}"),
    }
}

#[test]
fn reasoning_delta_becomes_agent_thought_chunk() {
    let step = turn().on_event(event(
        "session.reasoning.delta",
        serde_json::json!({
            "sessionID": SID, "assistantMessageID": MSG, "ordinal": 0,
            "delta": "The user wants me to run a bash",
        }),
    ));
    match update(step) {
        acp::SessionUpdate::AgentThoughtChunk(chunk) => {
            assert_eq!(text_of(&chunk), "The user wants me to run a bash");
        }
        other => panic!("expected AgentThoughtChunk, got {other:?}"),
    }
}

#[test]
fn shell_tool_call_is_execute_with_command_title() {
    let mut turn = turn();
    match update(shell_called(&mut turn)) {
        acp::SessionUpdate::ToolCall(call) => {
            assert_eq!(call.tool_call_id.0.as_ref(), TOOL);
            assert_eq!(call.kind, acp::ToolKind::Execute);
            assert_eq!(call.status, acp::ToolCallStatus::InProgress);
            assert!(
                call.title.contains("echo hello-from-probe"),
                "{}",
                call.title
            );
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[test]
fn tool_success_completes_with_output() {
    let mut turn = turn();
    shell_called(&mut turn);
    let u = tool_update(turn.on_event(event(
        "session.tool.success",
        serde_json::json!({
            "sessionID": SID, "assistantMessageID": MSG, "id": TOOL,
            "content": [{ "type": "text", "text": "hello-from-probe\n" }],
            "metadata": { "status": "completed", "truncated": false, "exit": 0 },
        }),
    )));
    assert_eq!(u.tool_call_id.0.as_ref(), TOOL);
    assert_eq!(u.fields.status, Some(acp::ToolCallStatus::Completed));
    assert_eq!(content_text(&u), "hello-from-probe\n");
}

#[test]
fn tool_failure_is_failed_with_error_message() {
    let mut turn = turn();
    shell_called(&mut turn);
    let u = tool_update(turn.on_event(event(
        "session.tool.failed",
        serde_json::json!({
            "sessionID": SID, "assistantMessageID": MSG, "id": TOOL,
            "error": { "type": "ToolError", "message": "exit status 1" },
            "content": [],
        }),
    )));
    assert_eq!(u.fields.status, Some(acp::ToolCallStatus::Failed));
    assert_eq!(content_text(&u), "exit status 1");
}

#[test]
fn permission_asked_targets_the_tool_call() {
    let mut turn = turn();
    shell_called(&mut turn);
    let step = turn.on_event(event(
        "permission.asked",
        serde_json::json!({
            "id": "per_1e8e6672c001G5gp6Vr9xl0qKQ", "sessionID": SID, "action": "shell",
            "resources": ["echo hello-from-probe"], "save": ["echo *"],
            "source": { "type": "tool", "messageID": MSG, "id": TOOL },
        }),
    ));
    match step {
        TurnStep::Permission {
            request_id,
            tool_call,
        } => {
            assert_eq!(request_id, "per_1e8e6672c001G5gp6Vr9xl0qKQ");
            assert_eq!(tool_call.tool_call_id.0.as_ref(), TOOL);
            assert_eq!(tool_call.fields.kind, Some(acp::ToolKind::Execute));
            assert_eq!(
                tool_call.fields.title.as_deref(),
                Some("echo hello-from-probe")
            );
        }
        other => panic!("expected Permission, got {other:?}"),
    }
}

#[test]
fn other_session_and_unknown_events_are_ignored() {
    let mut turn = turn();
    let other = turn.on_event(event(
        "session.text.delta",
        serde_json::json!({ "sessionID": "ses_other", "assistantMessageID": MSG, "delta": "x" }),
    ));
    assert!(matches!(other, TurnStep::Ignore), "{other:?}");
    let unknown = turn.on_event(event(
        "session.usage.updated",
        serde_json::json!({ "sessionID": SID }),
    ));
    assert!(matches!(unknown, TurnStep::Ignore), "{unknown:?}");
}

#[test]
fn execution_end_maps_to_stop_reason() {
    let ended = |kind: &str, cancelling: bool| {
        let mut turn = turn();
        turn.cancelling = cancelling;
        turn.on_event(event(kind, serde_json::json!({ "sessionID": SID })))
    };
    assert!(matches!(
        ended("session.execution.succeeded", false),
        TurnStep::Stop(acp::StopReason::EndTurn)
    ));
    assert!(matches!(
        ended("session.execution.succeeded", true),
        TurnStep::Stop(acp::StopReason::Cancelled)
    ));
    assert!(matches!(
        ended("session.execution.interrupted", false),
        TurnStep::Stop(acp::StopReason::Cancelled)
    ));
}

#[test]
fn execution_failed_ends_turn_with_error() {
    let step = turn().on_event(event(
        "session.execution.failed",
        serde_json::json!({ "sessionID": SID, "error": { "type": "ProviderError", "message": "rate limited" } }),
    ));
    match step {
        TurnStep::Fail(message) => assert_eq!(message, "ProviderError: rate limited"),
        other => panic!("expected Fail, got {other:?}"),
    }
}

#[test]
fn cancel_suppresses_further_output() {
    let mut turn = turn();
    turn.cancelling = true;
    let step = turn.on_event(event(
        "session.text.delta",
        serde_json::json!({ "sessionID": SID, "assistantMessageID": MSG, "delta": "late" }),
    ));
    assert!(matches!(step, TurnStep::Ignore), "{step:?}");
}

fn reply(option_id: &str, followup: Option<&str>) -> (Decision, Option<String>) {
    let meta = followup.and_then(|text| {
        serde_json::json!({ "followup_message": text })
            .as_object()
            .cloned()
    });
    permission_reply(
        &acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Selected(
            acp::SelectedPermissionOutcome::new(option_id.to_owned()),
        ))
        .meta(meta),
    )
}

#[test]
fn permission_outcome_maps_to_decision() {
    assert_eq!(reply("allow-once", None), (Decision::Once, None));
    assert_eq!(reply("allow-always", None), (Decision::Always, None));
    assert_eq!(
        permission_reply(&acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Cancelled
        )),
        (Decision::Reject, None)
    );
}

#[test]
fn reject_sends_feedback_or_default_message() {
    assert_eq!(
        reply("reject", Some("  no shell please  ")),
        (Decision::Reject, Some("no shell please".to_owned()))
    );
    for blank in [None, Some(""), Some("   ")] {
        assert_eq!(
            reply("reject", blank),
            (Decision::Reject, Some(DEFAULT_REJECT_MESSAGE.to_owned()))
        );
    }
}

fn rejected(turn: &mut TurnState, message: &str) -> acp::ToolCallUpdate {
    tool_update(turn.on_event(event(
        "session.tool.failed",
        serde_json::json!({
            "sessionID": SID, "assistantMessageID": MSG, "id": TOOL,
            "error": { "type": "permission.rejected", "message": message },
        }),
    )))
}

#[test]
fn rejected_tool_shows_rejected_with_feedback() {
    let mut turn = turn();
    shell_called(&mut turn);
    let u = rejected(&mut turn, "no shell please");
    assert_eq!(u.fields.status, Some(acp::ToolCallStatus::Failed));
    assert_eq!(content_text(&u), "Rejected by user: no shell please");

    let mut turn = self::turn();
    shell_called(&mut turn);
    let u = rejected(&mut turn, DEFAULT_REJECT_MESSAGE);
    assert_eq!(content_text(&u), "Rejected by user");
}

#[test]
fn shutdown_after_reject_ends_turn_not_cancelled() {
    let interrupted = |turn: &mut TurnState, reason: &str| {
        turn.on_event(event(
            "session.execution.interrupted",
            serde_json::json!({ "sessionID": SID, "reason": reason }),
        ))
    };
    let mut turn = turn();
    shell_called(&mut turn);
    rejected(&mut turn, "");
    assert!(matches!(
        interrupted(&mut turn, "shutdown"),
        TurnStep::Stop(acp::StopReason::EndTurn)
    ));
    // A real cancel still reads as cancelled.
    turn.cancelling = true;
    assert!(matches!(
        interrupted(&mut turn, "shutdown"),
        TurnStep::Stop(acp::StopReason::Cancelled)
    ));
    let mut fresh = self::turn();
    assert!(matches!(
        interrupted(&mut fresh, "shutdown"),
        TurnStep::Stop(acp::StopReason::Cancelled)
    ));
}

fn listed(
    provider: &str,
    id: &str,
    name: &str,
    context: Option<u64>,
) -> oc_opencode_client::ModelInfo {
    serde_json::from_value(serde_json::json!({
        "id": id, "providerID": provider, "name": name,
        "limit": context.map(|c| serde_json::json!({ "context": c })),
    }))
    .expect("model info")
}

#[test]
fn model_list_uses_provider_slash_id() {
    let models = model_infos(
        &[
            listed("opencode", "big-pickle", "Big Pickle", Some(200_000)),
            listed("opencode", "glm-5.3-flash-free", "", None),
        ],
        "opencode/big-pickle",
    );
    let ids: Vec<&str> = models.iter().map(|m| m.model_id.0.as_ref()).collect();
    assert_eq!(ids, ["opencode/big-pickle", "opencode/glm-5.3-flash-free"]);
    assert_eq!(models[0].name, "Big Pickle");
    assert_eq!(models[1].name, "opencode/glm-5.3-flash-free");
    assert_eq!(
        models[0]
            .meta
            .as_ref()
            .and_then(|m| m.get("totalContextTokens"))
            .and_then(|v| v.as_u64()),
        Some(200_000)
    );
}

#[test]
fn configured_model_is_listed_even_if_server_omits_it() {
    let models = model_infos(
        &[listed("opencode", "big-pickle", "Big Pickle", None)],
        "local/custom",
    );
    assert_eq!(models[0].model_id.0.as_ref(), "local/custom");
    assert_eq!(models.len(), 2);
    assert_eq!(model_infos(&[], "a/b").len(), 1);
}

#[test]
fn model_id_parses_provider_and_id() {
    let model = parse_model_id("opencode/glm-5.3-flash-free").expect("valid");
    assert_eq!(
        (model.provider_id.as_str(), model.id.as_str()),
        ("opencode", "glm-5.3-flash-free")
    );
    assert!(parse_model_id("big-pickle").is_none());
    assert!(parse_model_id("/x").is_none());
}

/// Builtins with the runtime voice gate open, as in a live session.
fn builtin_registry() -> crate::slash::registry::CommandRegistry {
    let mut reg =
        crate::slash::registry::CommandRegistry::new(crate::slash::commands::builtin_commands());
    reg.set_voice_visible(true);
    reg
}

fn opencode_registry() -> crate::slash::registry::CommandRegistry {
    let mut reg = builtin_registry();
    reg.hide_unavailable_in_opencode();
    reg
}

#[test]
fn unavailable_commands_are_real_builtins() {
    let reg = builtin_registry();
    for name in UNAVAILABLE_COMMANDS {
        let cmd = reg.get_for_dispatch(name);
        assert_eq!(
            cmd.map(|c| c.name()),
            Some(*name),
            "/{name} is not a builtin"
        );
    }
}

#[test]
fn unavailable_commands_leave_the_menu_but_still_resolve() {
    let reg = opencode_registry();
    let offered: Vec<&str> = reg
        .triggers()
        .iter()
        .map(|t| t.canonical.as_str())
        .collect();
    for name in UNAVAILABLE_COMMANDS {
        assert!(!offered.contains(name), "/{name} is still offered");
        assert!(reg.get(name).is_none(), "/{name} still completes");
        // A typed one still resolves, so the prompt can say it is unavailable.
        assert!(reg.get_for_dispatch(name).is_some());
    }
    // The `/cost` alias of `/usage` resolves to the hidden command too.
    let cost = reg.get_for_dispatch("cost").expect("alias resolves");
    assert!(is_unavailable_command(cost.name()));
    for kept in ["help", "model", "plugins", "skills", "mcps", "new"] {
        assert!(offered.contains(&kept), "/{kept} should stay offered");
    }
}

#[test]
fn unavailable_command_message_names_the_command() {
    assert!(is_unavailable_command("usage"));
    assert!(!is_unavailable_command("model"));
    assert_eq!(
        unavailable_message("usage"),
        "/usage is not available in opencode mode"
    );
}

#[test]
fn later_mappable_list_calls_say_not_yet() {
    for method in ["x.ai/plugins/list", "x.ai/skills/list", "x.ai/mcp/list"] {
        let (result, _) = stub_ext_method(method);
        let err = result.expect_err("stubbed");
        assert_eq!(err.code, acp::ErrorCode::MethodNotFound);
        assert_eq!(err.to_string(), NOT_YET_MESSAGE);
    }
    let (result, label) = stub_ext_method("x.ai/billing");
    assert_eq!(result.expect_err("stubbed"), acp::Error::method_not_found());
    assert_eq!(label, "method_not_found");
}

#[test]
fn xai_only_effects_are_skipped() {
    use crate::app::actions::Effect;
    assert!(skips_effect(&Effect::FetchBundleStatus));
    assert!(skips_effect(&Effect::CheckSubscription { verify: None }));
    assert!(skips_effect(&Effect::FetchAppBilling { nonce: 0 }));
    // A usage-modal fetch (non-zero nonce) still runs so the modal settles.
    assert!(!skips_effect(&Effect::FetchAppBilling { nonce: 7 }));
    assert!(!skips_effect(&Effect::Quit));
    assert!(!skips_effect(&Effect::ResetMouseReporting));
}

fn agent(id: &str, mode: &str, hidden: bool) -> AgentInfo {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "name": format!("{}{}", id[..1].to_uppercase(), &id[1..]),
        "mode": mode,
        "hidden": hidden,
    }))
    .expect("agent info")
}

#[test]
fn modes_are_the_visible_primary_agents_with_build_as_default() {
    // Server order from opencode 2.3.0: build, subagents, hidden primaries, then plan.
    let agents = [
        agent("general", "subagent", false),
        agent("build", "primary", false),
        agent("explore", "subagent", false),
        agent("compaction", "primary", true),
        agent("title", "primary", true),
        agent("summary", "primary", true),
        agent("plan", "primary", false),
    ];
    let modes = session_modes(&agents);
    let ids: Vec<_> = modes
        .iter()
        .map(|m| (m.id.0.as_ref(), m.name.as_str()))
        .collect();
    assert_eq!(ids, [("default", "Build"), ("plan", "Plan")]);
    // The pager parses these ids as its own Normal/Plan modes.
    assert_eq!(SessionMode::from_id(&modes[0].id.0), SessionMode::Default);
    assert!(SessionMode::from_id(&modes[1].id.0).is_plan());
}

#[test]
fn mode_ids_map_to_agents_and_back() {
    assert_eq!(agent_for_mode("default"), "build");
    assert_eq!(agent_for_mode("plan"), "plan");
    assert_eq!(mode_for_agent("build"), "default");
    assert_eq!(mode_for_agent("plan"), "plan");
    // A custom primary agent keeps its id both ways.
    assert_eq!(agent_for_mode(&mode_for_agent("review")), "review");
}

#[test]
fn agent_selected_mid_turn_updates_the_mode() {
    let mut t = turn();
    let step = t.on_event(event(
        "session.agent.selected",
        serde_json::json!({ "sessionID": SID, "agent": "build", "previous": "plan" }),
    ));
    match update(step) {
        acp::SessionUpdate::CurrentModeUpdate(m) => {
            assert_eq!(m.current_mode_id.0.as_ref(), "default")
        }
        other => panic!("expected CurrentModeUpdate, got {other:?}"),
    }
    let other_session = t.on_event(event(
        "session.agent.selected",
        serde_json::json!({ "sessionID": "ses_other", "agent": "plan" }),
    ));
    assert!(matches!(other_session, TurnStep::Ignore));
}

#[test]
fn execute_plan_meta_becomes_a_build_prompt() {
    let meta = |v: serde_json::Value| v.as_object().cloned().expect("object");
    let with_plan = meta(serde_json::json!({
        "promptId": "p1",
        "executePlan": { "planFileContent": "# Plan\n1. write x\n" },
    }));
    let prompt = execute_plan_prompt(Some(&with_plan)).expect("execute plan");
    assert!(prompt.starts_with(EXECUTE_PLAN_PROMPT));
    assert!(prompt.ends_with("# Plan\n1. write x"));

    let empty = meta(serde_json::json!({ "executePlan": { "planFileContent": "  " } }));
    assert_eq!(
        execute_plan_prompt(Some(&empty)).as_deref(),
        Some(EXECUTE_PLAN_PROMPT)
    );

    let normal = meta(serde_json::json!({ "promptId": "p2" }));
    assert_eq!(execute_plan_prompt(Some(&normal)), None);
    assert_eq!(execute_plan_prompt(None), None);
}

#[test]
fn yolo_change_reads_the_pager_notification() {
    let on = serde_json::json!({
        "sessionId": SID, "yolo_mode": true, "auto_mode": false, "permission_mode": "always-approve",
    });
    assert_eq!(yolo_change(&on), Some((SID.to_owned(), true)));
    let off = serde_json::json!({ "sessionId": SID, "yolo_mode": false, "permission_mode": "ask" });
    assert_eq!(yolo_change(&off), Some((SID.to_owned(), false)));
    // The settings-sync sender omits `yolo_mode` on purpose: leave the state alone.
    let other = serde_json::json!({ "sessionId": SID, "auto_mode": true });
    assert_eq!(yolo_change(&other), None);
}
