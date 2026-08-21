//! Live probes against the real installed pi CLI (run with --ignored).
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{CancellationToken, Harness, PiHarness, RunControls, SteerMessage};
use zeron_proto::{AgentEvent, RunRequest, SandboxLevel, UserInputAnswer};

#[tokio::test]
#[ignore = "spawns the real pi CLI"]
async fn live_models_discovery() {
    let models = PiHarness::new().models().await.expect("models");
    for m in models.iter().take(10) {
        println!(
            "{} | {} | ladder: {} | {}",
            m.id,
            m.label,
            m.reasoning_levels.len(),
            m.description.as_deref().unwrap_or("-")
        );
    }
    assert!(!models.is_empty(), "real pi must advertise models");
}

#[tokio::test]
#[ignore = "spawns the real pi CLI + one tiny prompt"]
async fn live_turn_settles() {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel::<Vec<UserInputAnswer>>().1),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    let req = RunRequest {
        prompt: "Reply with exactly the single word: ok".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: std::env::temp_dir().display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    let stream = PiHarness::new()
        .run(req, controls)
        .await
        .expect("run starts");
    drop(steer_tx); // one turn, then teardown
    let events: Vec<AgentEvent> = tokio::time::timeout(
        Duration::from_secs(120),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("live turn settles in time");
    for ev in &events {
        match ev {
            AgentEvent::SessionStarted {
                model, session_id, ..
            } => println!("SessionStarted: model={model} session={session_id}"),
            AgentEvent::TextDelta { text } => print!("{text}"),
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
            } => println!("\nUsage: {input_tokens} in / {output_tokens} out"),
            AgentEvent::Done {
                status,
                error,
                session_id,
                ..
            } => println!("Done: {status:?} err={error:?} session={session_id:?}"),
            _ => {}
        }
    }
    let done = events
        .iter()
        .rev()
        .find(|e| matches!(e, AgentEvent::Done { .. }));
    assert!(
        matches!(
            done,
            Some(AgentEvent::Done {
                status: zeron_proto::DoneStatus::Completed,
                ..
            })
        ),
        "live turn must complete: {done:?}"
    );
}
