//! PiHarness integration tests against the fake pi CLI in
//! `tests/fixtures/fake-pi.sh` (no real `pi` binary involved).

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{CancellationToken, Harness, PiHarness, RunControls, SteerMessage};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, SteeringMode, ToolCall,
    UserInputAnswer,
};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-pi.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> PiHarness {
    PiHarness::new().with_executable(fixture_path())
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec!["Yes".into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

async fn run_to_end(
    harness: &PiHarness,
    req: RunRequest,
    controls: RunControls,
) -> Vec<AgentEvent> {
    let stream = harness.run(req, controls).await.expect("run starts");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("run settles in time")
}

fn text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|ev| match ev {
            AgentEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn done(events: &[AgentEvent]) -> &AgentEvent {
    events
        .iter()
        .rev()
        .find(|ev| matches!(ev, AgentEvent::Done { .. }))
        .expect("a Done event")
}

#[tokio::test]
async fn basic_turn_streams_text_reasoning_usage_and_settles() {
    let (controls, _steer_tx, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:basic"), controls).await;

    let started = events.iter().find_map(|ev| match ev {
        ev @ AgentEvent::SessionStarted { .. } => Some(ev),
        _ => None,
    });
    let Some(AgentEvent::SessionStarted {
        harness,
        model,
        session_id,
        cwd,
        ..
    }) = started
    else {
        panic!("no SessionStarted in {events:?}");
    };
    assert_eq!(*harness, HarnessId::Pi);
    assert_eq!(model, "Claude Sonnet 4.5");
    assert_eq!(session_id, "pi-s-1");
    assert_eq!(cwd, "/tmp");

    // Reasoning deltas stream as their own kind.
    let thinking: String = events
        .iter()
        .filter_map(|ev| match ev {
            AgentEvent::ReasoningDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "thinking about it more");
    assert_eq!(text(&events), "Hello world");

    // Usage is held to the settle boundary, then Done completes.
    assert!(events.iter().any(|ev| matches!(
        ev,
        AgentEvent::Usage {
            input_tokens: 10,
            output_tokens: 5
        }
    )));
    match done(&events) {
        AgentEvent::Done {
            status: DoneStatus::Completed,
            session_id,
            ..
        } => assert_eq!(session_id.as_deref(), Some("pi-s-1")),
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_calls_map_to_wire_kinds_with_output_and_diff() {
    let (controls, _steer_tx, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:tools"), controls).await;

    let call = |id: &str| {
        events.iter().find_map(|ev| match ev {
            AgentEvent::ToolCall { id: i, call } if i == id => Some(call.clone()),
            _ => None,
        })
    };
    assert!(matches!(
        call("t1"),
        Some(ToolCall::Exec { command }) if command == "cargo test"
    ));
    assert!(matches!(
        call("t2"),
        Some(ToolCall::ReadFile { path }) if path == "/w/src/main.rs"
    ));
    assert!(matches!(
        call("t3"),
        Some(ToolCall::EditFile { path, old_string, new_string })
            if path == "/w/src/main.rs"
                && old_string.as_deref() == Some("fn main() {}")
                && new_string.as_deref().is_some()
    ));

    let result = |id: &str| {
        events.iter().find_map(|ev| match ev {
            AgentEvent::ToolResult { id: i, .. } if i == id => Some(ev.clone()),
            _ => None,
        })
    };
    match result("t1") {
        Some(AgentEvent::ToolResult {
            is_error,
            output,
            diff,
            ..
        }) => {
            assert!(!is_error);
            assert_eq!(output.as_deref(), Some("test result: ok"));
            assert!(diff.is_none());
        }
        other => panic!("expected bash ToolResult, got {other:?}"),
    }
    // The edit result carries the structured diff built from the start
    // frame's args (the end frame has none).
    match result("t3") {
        Some(AgentEvent::ToolResult {
            diff: Some(diff), ..
        }) => {
            assert_eq!(diff.path, "/w/src/main.rs");
            assert_eq!(diff.old_text.as_deref(), Some("fn main() {}"));
            assert!(diff.new_text.contains("println!"));
        }
        other => panic!("expected edit ToolResult with diff, got {other:?}"),
    }

    match done(&events) {
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        } => {}
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn in_turn_retry_recovery_completes() {
    // A failed attempt (stopReason: error) that pi retries in-turn must NOT
    // poison the Done: the successful retry's clean message_end supersedes.
    // Usage accumulates across BOTH attempts (5+7 in, 2+3 out).
    let (controls, _steer_tx, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:error"), controls).await;
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            AgentEvent::Usage {
                input_tokens: 12,
                output_tokens: 5
            }
        )),
        "usage must accumulate across the retry: {events:?}"
    );
    match done(&events) {
        AgentEvent::Done {
            status: DoneStatus::Completed,
            error,
            ..
        } => {
            assert!(
                error.is_none(),
                "recovered turn must not carry the retry's error: {error:?}"
            );
        }
        other => panic!("expected Completed after retry, got {other:?}"),
    }
}

#[tokio::test]
async fn final_error_settles_errored() {
    let (controls, _steer_tx, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:final-error"), controls).await;
    match done(&events) {
        AgentEvent::Done {
            status: DoneStatus::Errored,
            error: Some(message),
            ..
        } => assert_eq!(message, "API key invalid"),
        other => panic!("expected Errored, got {other:?}"),
    }
}

#[tokio::test]
async fn mid_turn_steer_injects_and_boundary_steer_starts_next_turn() {
    let (controls, steer_tx, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer"), controls)
        .await
        .expect("run starts");
    let mut events = Vec::new();
    let mut stream = std::pin::pin!(stream);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    // Wait for the tool call to open, then steer mid-turn.
    loop {
        let ev = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timed out waiting for tool call")
            .expect("stream ended early")
            .expect("stream event");
        let is_tool = matches!(ev, AgentEvent::ToolCall { .. });
        events.push(ev);
        if is_tool {
            break;
        }
    }
    steer_tx
        .send(SteerMessage {
            prompt: "change course".into(),
            message_id: None,
        })
        .await
        .expect("steer delivered");

    // First turn settles; a Steered marker rotated the assistant message.
    let first_done = loop {
        let ev = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timed out waiting for first Done")
            .expect("stream ended before Done")
            .expect("stream event");
        let is_done = matches!(ev, AgentEvent::Done { .. });
        events.push(ev);
        if is_done {
            break events.len();
        }
    };
    let first_segment = &events[..first_done];
    assert!(
        first_segment
            .iter()
            .any(|ev| matches!(ev, AgentEvent::Steered { .. })),
        "mid-turn steer must emit the rotation marker: {first_segment:?}"
    );
    assert_eq!(text(first_segment), "steered reply");

    // Steer again — now between turns: it becomes the next prompt directly.
    steer_tx
        .send(SteerMessage {
            prompt: "after the boundary".into(),
            message_id: None,
        })
        .await
        .expect("boundary steer delivered");
    let ev = tokio::time::timeout_at(deadline, stream.next())
        .await
        .expect("timed out waiting for Steered")
        .expect("stream ended before second turn")
        .expect("stream event");
    assert!(matches!(ev, AgentEvent::Steered { .. }), "got {ev:?}");
    events.push(ev);

    // Drain to the second Done, then close the mailbox to end the run.
    let mut second_text = String::new();
    let second_done_idx;
    loop {
        let ev = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timed out waiting for second Done")
            .expect("stream ended before second Done")
            .expect("stream event");
        if let AgentEvent::TextDelta { text } = &ev {
            second_text += text;
        }
        let is_done = matches!(ev, AgentEvent::Done { .. });
        events.push(ev);
        if is_done {
            second_done_idx = events.len();
            break;
        }
    }
    assert_eq!(second_text, "second turn");
    match events.get(second_done_idx - 1) {
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }) => {}
        other => panic!("expected second Completed, got {other:?}"),
    }

    drop(steer_tx);
    let tail: Vec<AgentEvent> = stream
        .by_ref()
        .map(|r| r.expect("stream event"))
        .collect::<Vec<_>>()
        .await;
    assert!(
        tail.iter().all(|ev| !matches!(
            ev,
            AgentEvent::Done {
                status: DoneStatus::Errored,
                ..
            }
        )),
        "teardown must not manufacture an error Done: {tail:?}"
    );
}

#[tokio::test]
async fn steer_that_lost_the_settle_race_becomes_the_next_prompt() {
    // pi rejects the steer with "not streaming" while the harness still
    // believes the turn is active; the arriving agent_settled must deliver
    // the queued steer as a fresh prompt instead of dropping it.
    let (controls, steer_tx, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer-race"), controls)
        .await
        .expect("run starts");
    let mut stream = std::pin::pin!(stream);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    // Wait for text, then send the steer that will be rejected.
    let mut recovered_text = String::new();
    loop {
        let ev = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timed out waiting for text")
            .expect("stream ended early")
            .expect("stream event");
        if let AgentEvent::TextDelta { text } = &ev {
            recovered_text += text;
            break;
        }
    }
    steer_tx
        .send(SteerMessage {
            prompt: "late steer".into(),
            message_id: None,
        })
        .await
        .expect("steer delivered");

    // The first Done (settled turn), then the recovered turn's Done.
    let mut dones = 0;
    while dones < 2 {
        let ev = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timed out waiting for dones")
            .expect("stream ended before both dones")
            .expect("stream event");
        match ev {
            AgentEvent::Done { status, .. } => {
                assert_eq!(status, DoneStatus::Completed);
                dones += 1;
            }
            AgentEvent::TextDelta { text } => recovered_text += &text,
            AgentEvent::Error { message } => panic!("unexpected error: {message}"),
            _ => {}
        }
    }
    // "quick" from the first turn + "recovered turn" from the redelivered
    // steer's turn.
    assert_eq!(recovered_text, "quickrecovered turn");
    drop(steer_tx);
}

#[tokio::test]
async fn interrupt_settles_interrupted() {
    let (controls, _steer_tx, token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");
    let mut stream = std::pin::pin!(stream);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    loop {
        let ev = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timed out waiting for text")
            .expect("stream ended early")
            .expect("stream event");
        if matches!(ev, AgentEvent::TextDelta { .. }) {
            break;
        }
    }
    token.cancel();

    let events: Vec<AgentEvent> = stream
        .by_ref()
        .map(|r| r.expect("stream event"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        done(&events),
        &AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: Some("pi-s-1".into()),
        }
    );
    // Exactly one terminal Done — the abort path must not double-emit.
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(ev, AgentEvent::Done { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn model_and_reasoning_ride_setup_commands() {
    let (controls, _steer_tx, _token) = controls();
    let mut req = request("scenario:configured");
    req.model = Some("anthropic/claude-opus-4-5".into());
    req.reasoning = Some(zeron_proto::ReasoningLevel::High);
    let events = run_to_end(&harness(), req, controls).await;
    // The fake errors out when the setup commands are missing.
    assert_eq!(text(&events), "configured");
}

#[tokio::test]
async fn crash_mid_turn_settles_errored_with_stderr_tail() {
    let (controls, _steer_tx, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:crash"), controls).await;
    match done(&events) {
        AgentEvent::Done {
            status: DoneStatus::Errored,
            error: Some(message),
            ..
        } => assert!(
            message.contains("pi exploded"),
            "crash message must carry the stderr tail: {message}"
        ),
        other => panic!("expected Errored crash, got {other:?}"),
    }
}

#[tokio::test]
async fn resume_passes_session_flag() {
    let (controls, _steer_tx, _token) = controls();
    let mut req = request("scenario:resume");
    req.resume = Some("pi-resume-9".into());
    let events = run_to_end(&harness(), req, controls).await;
    // The fake only answers the resumed run; a missing --session arg fails
    // the handshake ("pi exited").
    assert_eq!(text(&events), "resumed");
}

#[tokio::test]
async fn models_discovered_over_rpc() {
    let models = harness().models().await.expect("models resolve");
    // The fake advertises two: one reasoning model (full ladder) and one
    // plain one (empty ladder); ids ride as provider/id.
    let by_id = |id: &str| models.iter().find(|m| m.id == id);
    let sonnet = by_id("anthropic/claude-sonnet-4-5").expect("sonnet listed");
    assert_eq!(sonnet.label, "Claude Sonnet 4.5");
    assert!(!sonnet.reasoning_levels.is_empty());
    let flash = by_id("google/gemini-2.5-flash").expect("flash listed");
    assert!(flash.reasoning_levels.is_empty());
}

#[tokio::test]
async fn commands_discovered_over_rpc() {
    // pi's get_commands (extension commands, prompt templates, skills)
    // feeds the composer's `/` popup; an entry without a description still
    // lists (empty string).
    let commands = harness().commands().await.expect("commands resolve");
    let by_name = |name: &str| commands.iter().find(|c| c.name == name);
    let fix = by_name("fix-tests").expect("prompt template listed");
    assert_eq!(fix.description, "Fix failing tests");
    assert!(by_name("skill:brave-search").is_some());
    assert_eq!(by_name("no-description").unwrap().description, "");
}

#[tokio::test]
async fn missing_binary_is_not_installed() {
    let harness = PiHarness::new().with_executable("/nonexistent/never-a-pi".into());
    // An explicit executable overrides resolution (installed() trusts it);
    // the spawn is what must fail — NotInstalled, never a hang.
    let err = harness
        .run(request("scenario:basic"), controls().0)
        .await
        .err()
        .expect("run must fail");
    assert!(
        matches!(err, zeron_harness::HarnessError::NotInstalled(_)),
        "{err}"
    );
}

#[test]
fn descriptor_matches_the_registry_contract() {
    let harness = harness();
    assert_eq!(harness.id(), HarnessId::Pi);
    assert_eq!(harness.display_name(), "Pi");
    assert!(harness.supports_steering());
    // Native `steer` injects mid-turn — the same class as codex/claude,
    // unlike the retired adapter's turn-boundary queueing.
    assert_eq!(harness.steering_mode(), SteeringMode::StepBoundary);
    // Done is pi's own agent_settled — no quiesce watchdog needed.
    assert!(harness.deterministic_turn_end());
    assert_eq!(
        harness.reasoning_levels(),
        &[
            zeron_proto::ReasoningLevel::Minimal,
            zeron_proto::ReasoningLevel::Low,
            zeron_proto::ReasoningLevel::Medium,
            zeron_proto::ReasoningLevel::High,
            zeron_proto::ReasoningLevel::XHigh,
            zeron_proto::ReasoningLevel::Max,
        ]
    );
}
