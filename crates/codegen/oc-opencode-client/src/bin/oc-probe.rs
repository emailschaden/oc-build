// Copyright 2026 The oc-build Authors
// SPDX-License-Identifier: Apache-2.0

//! Live probe for an `opencode serve` instance: runs one turn in `text`,
//! `permission` or `interrupt` mode and reports what the event stream did.
//!
//! Text deltas go to stdout, everything else to stderr. Exit code 0 on the
//! expected outcome, 1 on failure, 2 on timeout.

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use futures_util::StreamExt;
use oc_opencode_client::{
    Client, CreateSession, Decision, Effect, Event, Location, Model, PermissionRule, tool_text,
};

#[derive(Parser)]
#[command(about = "Probe an opencode v2 server end-to-end")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:4096")]
    url: String,
    #[arg(long, env = "OPENCODE_PASSWORD", hide_env_values = true)]
    password: Option<String>,
    /// Session directory (defaults to the current directory).
    #[arg(long)]
    dir: Option<std::path::PathBuf>,
    /// `provider/model`.
    #[arg(long, default_value = "opencode/big-pickle")]
    model: String,
    #[arg(long, value_enum, default_value_t = Mode::Text)]
    mode: Mode,
    /// Override the mode's default prompt.
    #[arg(long)]
    prompt: Option<String>,
    /// Reply to permission requests with this decision.
    #[arg(long, value_enum, default_value_t = CliDecision::Once)]
    decision: CliDecision,
    /// Interrupt mode: interrupt after this many text deltas (or 3 s after the first).
    #[arg(long, default_value_t = 10)]
    interrupt_after: usize,
    #[arg(long, default_value_t = 120)]
    timeout_secs: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    Text,
    Permission,
    Interrupt,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliDecision {
    Once,
    Always,
    Reject,
}

impl From<CliDecision> for Decision {
    fn from(d: CliDecision) -> Self {
        match d {
            CliDecision::Once => Decision::Once,
            CliDecision::Always => Decision::Always,
            CliDecision::Reject => Decision::Reject,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("[probe] error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let dir = match args.dir {
        Some(dir) => std::path::absolute(dir)?,
        None => std::env::current_dir()?,
    };
    let (provider_id, model_id) = args
        .model
        .split_once('/')
        .ok_or("--model must be provider/id")?;
    let prompt = args.prompt.unwrap_or_else(|| {
        match args.mode {
            Mode::Text => "Reply with a short haiku about Rust. No tools.",
            Mode::Permission => "Run the bash command: echo hello-from-oc-probe . Then reply done.",
            Mode::Interrupt => "Count from 1 to 500, one number per line, no tools.",
        }
        .to_owned()
    });
    // The asked event reports the tool as `shell`; cover both names.
    let permissions = (args.mode == Mode::Permission).then(|| {
        ["bash", "shell"]
            .map(|action| PermissionRule {
                action: action.to_owned(),
                resource: "*".to_owned(),
                effect: Effect::Ask,
            })
            .to_vec()
    });

    let client = Client::new(args.url, args.password)?;
    // Subscribe first so no event of the new session is missed.
    let mut events = Box::pin(client.events().await?);
    let session = client
        .create_session(&CreateSession {
            title: Some("oc-probe".to_owned()),
            location: Some(Location {
                directory: dir.to_string_lossy().into_owned(),
            }),
            model: Some(Model {
                id: model_id.to_owned(),
                provider_id: provider_id.to_owned(),
            }),
            permissions,
        })
        .await?;
    eprintln!("[probe] session {} in {}", session.id, dir.display());
    let message_id = client.prompt(&session.id, &prompt).await?;
    eprintln!("[probe] prompt {message_id}: {prompt:?}");

    let deadline = tokio::time::sleep(Duration::from_secs(args.timeout_secs));
    tokio::pin!(deadline);
    // Interrupt mode: fires 3 s after the first text delta.
    let interrupt_timer = tokio::time::sleep(Duration::MAX);
    tokio::pin!(interrupt_timer);
    let mut interrupt_armed = false;
    let mut interrupted = false;

    let mut text_deltas = 0usize;
    let mut reasoning_deltas = 0usize;
    let mut text = String::new();
    let mut execution_started = false;
    let mut permission_asks = 0usize;

    let outcome = loop {
        let event = tokio::select! {
            _ = &mut deadline => break Outcome::Timeout,
            _ = &mut interrupt_timer, if interrupt_armed && !interrupted => None,
            event = events.next() => match event {
                Some(event) => Some(event?),
                None => break Outcome::Failed("event stream ended".to_owned()),
            },
        };

        let mut want_interrupt = event.is_none();
        if let Some(event) = event {
            if matches!(event, Event::ServerConnected {}) {
                eprintln!("[event] server.connected");
                continue;
            }
            if event.session_id() != Some(session.id.as_str()) {
                continue;
            }
            match &event {
                Event::TextDelta { delta, .. } => {
                    text_deltas += 1;
                    text.push_str(delta);
                    print!("{delta}");
                    std::io::stdout().flush()?;
                    if args.mode == Mode::Interrupt && !interrupted {
                        if text_deltas >= args.interrupt_after {
                            want_interrupt = true;
                        } else if !interrupt_armed {
                            interrupt_armed = true;
                            interrupt_timer
                                .as_mut()
                                .reset(tokio::time::Instant::now() + Duration::from_secs(3));
                        }
                    }
                }
                Event::ReasoningDelta { .. } => reasoning_deltas += 1,
                Event::TextEnded { .. } => {
                    println!();
                    eprintln!("[event] session.text.ended ({text_deltas} deltas so far)");
                }
                Event::StepEnded { finish, tokens, .. } => {
                    eprintln!("[event] session.step.ended finish={finish} tokens={tokens:?}")
                }
                Event::ToolInputStarted { id, name, .. } => {
                    eprintln!("[event] session.tool.input.started {name} ({id})")
                }
                Event::ToolCalled { id, input, .. } => {
                    eprintln!("[event] session.tool.called {id} input={input}")
                }
                Event::ToolSuccess { id, content, .. } => {
                    eprintln!(
                        "[event] session.tool.success {id} output={:?}",
                        tool_text(content)
                    )
                }
                Event::ToolFailed { id, error, .. } => eprintln!(
                    "[event] session.tool.failed {id} {}: {}",
                    error.kind, error.message
                ),
                Event::PermissionAsked {
                    id,
                    action,
                    resources,
                    save,
                    ..
                } => {
                    permission_asks += 1;
                    let decision = Decision::from(args.decision);
                    eprintln!(
                        "[event] permission.asked {id} action={action} resources={resources:?} save={save:?} -> {decision:?}"
                    );
                    client
                        .reply_permission(&session.id, id, decision, None)
                        .await?;
                }
                Event::PermissionReplied {
                    request_id, reply, ..
                } => eprintln!("[event] permission.replied {request_id} reply={reply}"),
                Event::ExecutionStarted { .. } => {
                    execution_started = true;
                    eprintln!("[event] session.execution.started");
                }
                Event::ExecutionSucceeded { .. } => break Outcome::Succeeded,
                Event::ExecutionFailed { error, .. } => {
                    break Outcome::Failed(format!("execution failed: {error:?}"));
                }
                Event::ExecutionInterrupted { reason, .. } => {
                    break Outcome::Interrupted(reason.clone());
                }
                Event::Idle { .. } if execution_started => break Outcome::Idle,
                Event::StepFailed { error, .. } => eprintln!(
                    "[event] session.step.failed {}: {}",
                    error.kind, error.message
                ),
                other => eprintln!("[event] {}", other.type_name()),
            }
        }

        if want_interrupt && !interrupted {
            interrupted = true;
            let resp = client.interrupt(&session.id).await?;
            eprintln!(
                "[probe] interrupt after {text_deltas} text deltas -> interrupted={}",
                resp.interrupted
            );
        }
    };
    println!();

    let elapsed = started.elapsed().as_secs_f32();
    let expected = match args.mode {
        Mode::Interrupt => matches!(outcome, Outcome::Interrupted(_)),
        // A rejected tool call ends the execution as interrupted (reason
        // `shutdown` on 2.3.0) rather than letting the model continue.
        Mode::Permission => match args.decision {
            CliDecision::Reject => permission_asks > 0 && !matches!(outcome, Outcome::Timeout),
            _ => permission_asks > 0 && matches!(outcome, Outcome::Succeeded | Outcome::Idle),
        },
        Mode::Text => matches!(outcome, Outcome::Succeeded | Outcome::Idle),
    };
    eprintln!(
        "[probe] {} outcome={outcome} text_deltas={text_deltas} reasoning_deltas={reasoning_deltas} \
         text_chars={} permission_asks={permission_asks} elapsed={elapsed:.1}s",
        if expected { "OK" } else { "FAIL" },
        text.chars().count(),
    );
    Ok(match outcome {
        Outcome::Timeout => ExitCode::from(2),
        _ if expected => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

enum Outcome {
    Succeeded,
    Idle,
    Interrupted(Option<String>),
    Failed(String),
    Timeout,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Succeeded => f.write_str("execution.succeeded"),
            Outcome::Idle => f.write_str("session.idle"),
            Outcome::Interrupted(reason) => write!(
                f,
                "execution.interrupted(reason={})",
                reason.as_deref().unwrap_or("?")
            ),
            Outcome::Failed(msg) => write!(f, "failed({msg})"),
            Outcome::Timeout => f.write_str("timeout"),
        }
    }
}
