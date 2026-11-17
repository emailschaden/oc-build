// Copyright 2026 The oc-build Authors
// SPDX-License-Identifier: Apache-2.0

//! Event parsing against SSE streams captured from opencode 2.3.0.

use oc_opencode_client::{Event, ToolContent, tool_text};

const TEXT_TURN: &str = include_str!("fixtures/text-turn.sse");
const PERMISSION_TURN: &str = include_str!("fixtures/permission-turn.sse");

fn parse_all(sse: &str) -> Vec<Event> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| Event::parse(data).expect("fixture line is JSON"))
        .collect()
}

fn known_types(events: &[Event]) -> Vec<&str> {
    events
        .iter()
        .filter(|e| !matches!(e, Event::Unknown { .. }))
        .map(Event::type_name)
        .collect()
}

#[test]
fn text_turn() {
    let events = parse_all(TEXT_TURN);
    assert_eq!(events.len(), 19);
    assert_eq!(events[0], Event::ServerConnected {});
    assert_eq!(
        known_types(&events),
        [
            "server.connected",
            "session.execution.started",
            "session.step.started",
            "session.reasoning.started",
            "session.reasoning.delta",
            "session.reasoning.delta",
            "session.text.started",
            "session.reasoning.ended",
            "session.text.delta",
            "session.text.ended",
            "session.step.ended",
            "session.execution.succeeded",
        ]
    );

    let session = "ses_e1739796cffeTTUuFx5gLzNQhO";
    let message = "msg_1e8c686cb001c1qFOMH5G6ab9Q";
    assert!(events.contains(&Event::TextDelta {
        session_id: session.into(),
        message_id: message.into(),
        delta: "OK".into(),
    }));
    assert!(events.contains(&Event::TextEnded {
        session_id: session.into(),
        message_id: message.into(),
        text: "OK".into(),
    }));
    let Some(Event::StepEnded { finish, tokens, .. }) =
        events.iter().find(|e| matches!(e, Event::StepEnded { .. }))
    else {
        panic!("no step.ended");
    };
    assert_eq!(finish, "stop");
    assert_eq!(tokens.as_ref().map(|t| t.output), Some(3));
    assert_eq!(
        events.last(),
        Some(&Event::ExecutionSucceeded {
            session_id: session.into()
        })
    );
    // Every event but server.connected carries the session id.
    assert!(events[1..].iter().all(|e| e.session_id() == Some(session)));
}

#[test]
fn permission_turn() {
    let events = parse_all(PERMISSION_TURN);
    let session = "ses_e1719cc3bffeM1A50AZWoIbZ67";
    let tool = "chatcmpl-tool-b85c4424842a3387";

    let Some(Event::ToolInputStarted { id, name, .. }) = events
        .iter()
        .find(|e| matches!(e, Event::ToolInputStarted { .. }))
    else {
        panic!("no tool.input.started");
    };
    assert_eq!((id.as_str(), name.as_str()), (tool, "shell"));

    let Some(Event::ToolCalled { input, .. }) = events
        .iter()
        .find(|e| matches!(e, Event::ToolCalled { .. }))
    else {
        panic!("no tool.called");
    };
    assert_eq!(input["command"], "echo hello-from-probe");

    let Some(Event::PermissionAsked {
        id,
        session_id,
        action,
        resources,
        save,
        source,
        ..
    }) = events
        .iter()
        .find(|e| matches!(e, Event::PermissionAsked { .. }))
    else {
        panic!("no permission.asked");
    };
    assert_eq!(id, "per_1e8e6672c001G5gp6Vr9xl0qKQ");
    assert_eq!(session_id, session);
    assert_eq!(action, "shell");
    assert_eq!(resources, &["echo hello-from-probe"]);
    assert_eq!(save, &["echo *"]);
    let source = source.as_ref().expect("source");
    assert_eq!(source.kind, "tool");
    assert_eq!(source.id.as_deref(), Some(tool));

    assert!(events.contains(&Event::PermissionReplied {
        session_id: session.into(),
        request_id: "per_1e8e6672c001G5gp6Vr9xl0qKQ".into(),
        reply: "once".into(),
    }));

    let Some(Event::ToolSuccess { id, content, .. }) = events
        .iter()
        .find(|e| matches!(e, Event::ToolSuccess { .. }))
    else {
        panic!("no tool.success");
    };
    assert_eq!(id, tool);
    assert_eq!(tool_text(content), "hello-from-probe\n");

    let finishes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::StepEnded { finish, .. } => Some(finish.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(finishes, ["tool-calls", "stop"]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ToolProgress { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TextDelta { delta, .. } if delta == "done"))
    );
    assert_eq!(
        events.last(),
        Some(&Event::ExecutionSucceeded {
            session_id: session.into()
        })
    );

    // shell.created is unmodelled; its session id lives elsewhere.
    let shell = events
        .iter()
        .find(|e| e.type_name() == "shell.created")
        .expect("shell.created");
    assert!(matches!(shell, Event::Unknown { .. }));
    assert_eq!(shell.session_id(), None);
}

#[test]
fn unknown_type_falls_back() {
    let line = r#"{"id":"evt_1","created":1,"type":"session.inbox.enqueued","data":{"sessionID":"ses_1","inboxID":"msg_1"}}"#;
    let event = Event::parse(line).unwrap();
    let Event::Unknown { type_name, raw } = &event else {
        panic!("expected Unknown, got {event:?}");
    };
    assert_eq!(type_name, "session.inbox.enqueued");
    assert_eq!(raw["data"]["inboxID"], "msg_1");
    assert_eq!(event.session_id(), Some("ses_1"));
}

#[test]
fn schema_drift_falls_back() {
    // Known type, but a required field changed type.
    let line = r#"{"id":"evt_1","created":1,"type":"session.text.delta","data":{"sessionID":"ses_1","assistantMessageID":"msg_1","delta":42}}"#;
    let event = Event::parse(line).unwrap();
    assert!(
        matches!(event, Event::Unknown { ref type_name, .. } if type_name == "session.text.delta")
    );
    assert_eq!(event.session_id(), Some("ses_1"));

    // Extra fields are fine.
    let line =
        r#"{"id":"evt_1","type":"session.idle","data":{"sessionID":"ses_1","new":true},"extra":1}"#;
    assert_eq!(
        Event::parse(line).unwrap(),
        Event::Idle {
            session_id: "ses_1".into()
        }
    );

    // No type at all.
    let event = Event::parse(r#"{"data":{}}"#).unwrap();
    assert!(matches!(event, Event::Unknown { ref type_name, .. } if type_name.is_empty()));

    assert!(Event::parse("not json").is_err());
}

#[test]
fn spec_only_events() {
    let interrupted = r#"{"id":"e","created":1,"type":"session.execution.interrupted","data":{"sessionID":"ses_1","reason":"user"},"durable":{"aggregateID":"ses_1","seq":3,"version":1}}"#;
    assert_eq!(
        Event::parse(interrupted).unwrap(),
        Event::ExecutionInterrupted {
            session_id: "ses_1".into(),
            reason: Some("user".into())
        }
    );

    let failed = r#"{"id":"e","created":1,"type":"session.execution.failed","data":{"sessionID":"ses_1","error":{"type":"ProviderError","message":"boom","status":500}}}"#;
    let Event::ExecutionFailed { error, .. } = Event::parse(failed).unwrap() else {
        panic!("expected ExecutionFailed");
    };
    let error = error.unwrap();
    assert_eq!(
        (error.kind.as_str(), error.message.as_str(), error.status),
        ("ProviderError", "boom", Some(500))
    );

    let tool_failed = r#"{"id":"e","created":1,"type":"session.tool.failed","data":{"sessionID":"ses_1","assistantMessageID":"msg_1","id":"t1","error":{"type":"Rejected","message":"no"},"content":[{"type":"file","uri":"x"}],"executed":false}}"#;
    let Event::ToolFailed { content, error, .. } = Event::parse(tool_failed).unwrap() else {
        panic!("expected ToolFailed");
    };
    assert_eq!(content, [ToolContent::Other]);
    assert_eq!(error.message, "no");

    let status = r#"{"id":"e","created":1,"type":"session.status","data":{"sessionID":"ses_1","status":{"type":"busy"}}}"#;
    assert!(
        matches!(Event::parse(status).unwrap(), Event::Status { status, .. } if status["type"] == "busy")
    );
}
