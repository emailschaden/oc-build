# oc-opencode-client

Minimal async client for the opencode v2 server (`opencode serve`, tested
against `@opencode/cli` 2.3.0).

- HTTP Basic auth, user `opencode`, password = the server's `OPENCODE_PASSWORD`.
- `{"data": ...}` response envelopes are unwrapped; non-2xx responses become
  `Error::Api { status, body }`.
- `Client::events()` subscribes to the global SSE stream `GET /api/event` and
  yields typed `Event`s. Unmodelled types, and known types whose payload does
  not match, come back as `Event::Unknown { type_name, raw }` instead of errors.
  The stream covers all sessions; filter with `Event::session_id()`.

```rust
let client = Client::new("http://127.0.0.1:4096", Some(password))?;
let mut events = Box::pin(client.events().await?); // subscribe first
let session = client.create_session(&CreateSession::default()).await?;
client.prompt(&session.id, "Reply with OK").await?;
while let Some(event) = events.next().await {
    match event? {
        Event::TextDelta { session_id, delta, .. } if session_id == session.id => print!("{delta}"),
        Event::ExecutionSucceeded { session_id } if session_id == session.id => break,
        _ => {}
    }
}
```

Other calls: `reply_permission(session, request, Decision, message)` and
`interrupt(session)`.

## oc-probe

End-to-end probe against a running server:

```sh
OPENCODE_PASSWORD=... cargo run -p oc-opencode-client --bin oc-probe -- \
  --url http://127.0.0.1:4096 --dir /abs/project --mode text|permission|interrupt
```

Text deltas go to stdout, `[event] ...` lines to stderr. Exit code 0 when the
mode's expected outcome happens, 1 on failure, 2 on timeout (`--timeout-secs`).
In `permission` mode, `--decision reject` makes the server end the execution as
interrupted (reason `shutdown`), which the probe treats as expected.

## Tests

`cargo test -p oc-opencode-client` parses SSE streams captured from a real
server (`tests/fixtures/`) and checks request shapes against an in-process mock
HTTP server.
