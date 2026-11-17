// Copyright 2026 The oc-build Authors
// SPDX-License-Identifier: Apache-2.0

//! Request shapes against a tiny in-process HTTP server.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use oc_opencode_client::{
    Client, CreateSession, Decision, Effect, Error, Event, Location, Model, PermissionRule,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// `opencode:secret`
const AUTH: &str = "Basic b3BlbmNvZGU6c2VjcmV0";

#[derive(Debug, Clone)]
struct Request {
    method: String,
    path: String,
    auth: Option<String>,
    body: String,
}

/// Serves one canned response per connection (`Connection: close`) and
/// records every request.
async fn mock_server() -> (String, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let log = log2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 4096];
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                let header = |name: &str| {
                    head.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
                    })
                };
                let len: usize = header("content-length").map_or(0, |v| v.parse().unwrap());
                while buf.len() < header_end + len {
                    let mut chunk = [0u8; 4096];
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let mut first = head.lines().next().unwrap().split(' ');
                let req = Request {
                    method: first.next().unwrap().to_owned(),
                    path: first.next().unwrap().to_owned(),
                    auth: header("authorization"),
                    body: String::from_utf8_lossy(&buf[header_end..header_end + len]).into_owned(),
                };
                log.lock().unwrap().push(req.clone());

                let (status, ctype, body) = route(&req);
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
            });
        }
    });
    (url, log)
}

fn route(req: &Request) -> (&'static str, &'static str, String) {
    const JSON: &str = "application/json";
    if req.auth.as_deref() != Some(AUTH) {
        return (
            "401 Unauthorized",
            JSON,
            r#"{"_tag":"UnauthorizedError"}"#.into(),
        );
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/api/session") => ("200 OK", JSON, r#"{"data":{"id":"ses_1","title":"t","time":{"created":1,"updated":1}}}"#.into()),
        ("POST", "/api/session/ses_1/prompt") => ("200 OK", JSON, r#"{"data":{"id":"msg_1","type":"user"}}"#.into()),
        ("POST", "/api/session/ses_1/permission/per_1/reply") => ("204 No Content", JSON, String::new()),
        ("POST", "/api/session/ses_1/model") => ("204 No Content", JSON, String::new()),
        ("GET", "/api/model") => ("200 OK", JSON, r#"{"location":{"directory":"/tmp/x"},"data":[{"id":"big-pickle","providerID":"opencode","name":"Big Pickle","type":"chat","limit":{"context":200000,"output":32000}},{"id":"glm-5.3-flash-free","providerID":"opencode","name":"GLM-5.3-Flash Free"}]}"#.into()),
        ("GET", "/api/agent") => ("200 OK", JSON, r#"{"location":{"directory":"/tmp/x"},"data":[{"id":"build","name":"Build","description":"The default agent.","mode":"primary","hidden":false,"permissions":[]},{"id":"explore","name":"Explore","mode":"subagent","hidden":false},{"id":"title","name":"Title","mode":"primary","hidden":true}]}"#.into()),
        ("POST", "/api/session/ses_1/agent") => ("204 No Content", JSON, String::new()),
        ("POST", "/api/session/ses_1/interrupt") => ("200 OK", JSON, r#"{"interrupted":true}"#.into()),
        ("GET", "/api/event") => (
            "200 OK",
            "text/event-stream",
            concat!(
                "data: {\"id\":\"e0\",\"type\":\"server.connected\",\"data\":{}}\n\n",
                ": heartbeat\n\n",
                "data: {\"id\":\"e1\",\"created\":1,\"type\":\"session.text.delta\",\"data\":{\"sessionID\":\"ses_1\",\"assistantMessageID\":\"msg_2\",\"ordinal\":0,\"delta\":\"hi\"}}\n\n",
                "data: {\"id\":\"e2\",\"created\":1,\"type\":\"something.new\",\"data\":{}}\n\n",
            )
            .into(),
        ),
        _ => ("400 Bad Request", JSON, r#"{"_tag":"InvalidRequestError","message":"nope","field":"x"}"#.into()),
    }
}

#[tokio::test]
async fn requests_paths_auth_and_bodies() {
    let (url, log) = mock_server().await;
    let client = Client::new(format!("{url}/"), Some("secret".into())).unwrap();

    let session = client
        .create_session(&CreateSession {
            title: Some("t".into()),
            location: Some(Location {
                directory: "/tmp/x".into(),
            }),
            model: Some(Model {
                id: "big-pickle".into(),
                provider_id: "opencode".into(),
            }),
            permissions: Some(vec![PermissionRule {
                action: "bash".into(),
                resource: "*".into(),
                effect: Effect::Ask,
            }]),
        })
        .await
        .unwrap();
    assert_eq!(session.id, "ses_1");
    assert_eq!(session.title.as_deref(), Some("t"));

    assert_eq!(client.prompt("ses_1", "hello").await.unwrap(), "msg_1");
    client
        .reply_permission("ses_1", "per_1", Decision::Reject, Some("no"))
        .await
        .unwrap();
    assert!(client.interrupt("ses_1").await.unwrap().interrupted);

    let reqs = log.lock().unwrap().clone();
    assert_eq!(reqs.len(), 4);
    assert!(reqs.iter().all(|r| r.auth.as_deref() == Some(AUTH)));
    let body = |i: usize| serde_json::from_str::<serde_json::Value>(&reqs[i].body).unwrap();
    assert_eq!(
        body(0),
        serde_json::json!({
            "title": "t",
            "location": {"directory": "/tmp/x"},
            "model": {"id": "big-pickle", "providerID": "opencode"},
            "permissions": [{"action": "bash", "resource": "*", "effect": "ask"}],
        })
    );
    assert_eq!(body(1), serde_json::json!({"text": "hello"}));
    assert_eq!(
        body(2),
        serde_json::json!({"decision": "reject", "message": "no"})
    );
    assert_eq!(reqs[3].path, "/api/session/ses_1/interrupt");
}

#[tokio::test]
async fn events_stream_skips_heartbeats() {
    let (url, _log) = mock_server().await;
    let client = Client::new(url, Some("secret".into())).unwrap();
    let events: Vec<Event> = client
        .events()
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect()
        .await;
    assert_eq!(events.len(), 3);
    assert_eq!(events[0], Event::ServerConnected {});
    assert!(matches!(&events[1], Event::TextDelta { delta, .. } if delta == "hi"));
    assert_eq!(events[1].session_id(), Some("ses_1"));
    assert_eq!(events[2].type_name(), "something.new");
}

#[tokio::test]
async fn errors_surface_status_and_body() {
    let (url, _log) = mock_server().await;

    let client = Client::new(url.clone(), None).unwrap();
    match client.prompt("ses_1", "x").await {
        Err(Error::Api { status: 401, .. }) => {}
        other => panic!("expected 401, got {other:?}"),
    }
    assert!(matches!(
        client.events().await.err(),
        Some(Error::Api { status: 401, .. })
    ));

    let client = Client::new(url, Some("secret".into())).unwrap();
    match client.prompt("ses_missing", "x").await {
        Err(Error::Api { status: 400, body }) => assert!(body.contains("InvalidRequestError")),
        other => panic!("expected 400, got {other:?}"),
    }
}

#[tokio::test]
async fn lists_and_switches_models() {
    let (url, log) = mock_server().await;
    let client = Client::new(url, Some("secret".into())).unwrap();

    let models = client.list_models().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(
        (models[0].provider_id.as_str(), models[0].id.as_str()),
        ("opencode", "big-pickle")
    );
    assert_eq!(models[0].name, "Big Pickle");
    assert_eq!(
        models[0].limit.as_ref().and_then(|l| l.context),
        Some(200_000)
    );
    assert!(models[1].limit.is_none());

    client
        .switch_model(
            "ses_1",
            &Model {
                id: "glm-5.3-flash-free".into(),
                provider_id: "opencode".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        client
            .switch_model(
                "ses_missing",
                &Model {
                    id: "x".into(),
                    provider_id: "y".into()
                }
            )
            .await,
        Err(Error::Api { status: 400, .. })
    ));

    let reqs = log.lock().unwrap().clone();
    assert_eq!(
        (reqs[0].method.as_str(), reqs[0].path.as_str()),
        ("GET", "/api/model")
    );
    assert_eq!(reqs[1].path, "/api/session/ses_1/model");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reqs[1].body).unwrap(),
        serde_json::json!({"model": {"id": "glm-5.3-flash-free", "providerID": "opencode"}})
    );
}

#[tokio::test]
async fn lists_and_switches_agents() {
    let (url, log) = mock_server().await;
    let client = Client::new(url, Some("secret".into())).unwrap();

    let agents = client.list_agents().await.unwrap();
    let summary: Vec<_> = agents
        .iter()
        .map(|a| (a.id.as_str(), a.mode.as_str(), a.hidden))
        .collect();
    assert_eq!(
        summary,
        [
            ("build", "primary", false),
            ("explore", "subagent", false),
            ("title", "primary", true)
        ]
    );
    assert_eq!(agents[0].name, "Build");
    assert_eq!(agents[0].description.as_deref(), Some("The default agent."));

    client.switch_agent("ses_1", "plan").await.unwrap();
    assert!(matches!(
        client.switch_agent("ses_missing", "plan").await,
        Err(Error::Api { status: 400, .. })
    ));

    let reqs = log.lock().unwrap().clone();
    assert_eq!(
        (reqs[0].method.as_str(), reqs[0].path.as_str()),
        ("GET", "/api/agent")
    );
    assert_eq!(
        (reqs[1].method.as_str(), reqs[1].path.as_str()),
        ("POST", "/api/session/ses_1/agent")
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reqs[1].body).unwrap(),
        serde_json::json!({"agent": "plan"})
    );
}
