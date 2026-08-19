//! pi harness: spawns the installed `pi` CLI as `pi --mode rpc` and speaks
//! its JSONL command/response protocol directly — no adapter process in
//! between. Replaces the `pi-acp` ACP adapter path (decision record:
//! docs/research/harness.md), same motivation as the codex/claude native
//! drivers: the adapter held its own turn queue and re-derived done-status
//! from pi's events, while steering (pi's native strength) never crossed
//! the ACP boundary at all.
//!
//! VERSION PIN: validated against pi `@earendil-works/pi-coding-agent`
//! (docs/rpc.md of that version). The RPC surface is additive-friendly —
//! unknown events are tolerated by design — but revalidate the command
//! shapes (`prompt`/`steer`/`abort`/`set_model`/`set_thinking_level`) and
//! `agent_settled` semantics when bumping.
//!
//! - `pi --mode rpc [--session <id>]`: newline-framed JSONL over stdio.
//!   Commands carry an id; responses resolve by id; agent events (`message_
//!   update`, `tool_execution_*`, …) stream bare (see [`wire`]). Extension
//!   UI dialogs (`extension_ui_request` select/confirm/input/editor) block
//!   the agent until answered — with no dialog surface to route them to,
//!   they are auto-cancelled so a run can never wedge on one.
//! - SETUP: `get_state` (session id + model for SessionStarted), optional
//!   `set_model` (`provider/id` catalog ids split on the slash) and
//!   `set_thinking_level`, then `prompt`. The prompt RESPONSE means
//!   accepted, never done — the turn settles on `agent_settled` (pi drains
//!   retries, compaction, and queued follow-ups before emitting it).
//! - STEERING: native `steer` command — delivered by pi after the current
//!   internal turn's tool calls, before the next LLM call: StepBoundary
//!   semantics, the same class as codex `turn/steer`. A steer rejected with
//!   "not streaming" lost the settle race; it is redelivered as the next
//!   `prompt` on the same persistent session (the codex expectedTurnId
//!   fallback, pi-shaped). Steers after a settled turn start new turns
//!   directly — the engine's done→wake handling already folds multi-Done
//!   runs.
//! - INTERRUPT: `abort`, then SIGTERM → SIGKILL escalation; the stream ends
//!   with `Done { status: Interrupted }`.
//! - DONE-STATUS: `agent_settled` completes the turn; a final assistant
//!   `message_end` with `stopReason: "error"` (after pi's own retries are
//!   exhausted) maps to Errored. `agent_settled` fires per settled run, so
//!   every turn shape ends deterministically — no quiesce watchdog needed.
//! - IMAGES: `RunRequest::attachments` are inlined as base64 image blocks
//!   on the prompt (pi's `ImageContent` format).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::{Harness, HarnessError, RunControls, Signal, send_signal, shutdown_child};
use wire::PiClient;
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand,
    SteeringMode, ToolCall, ToolDiff,
};

mod wire;

/// pi's thinking ladder maps 1:1 onto zeron's (pi's extra "off" tier has no
/// zeron equivalent and is left to the agent default; the Claude-style
/// prompt-prefix levels have no pi meaning).
pub(crate) const REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
];

fn to_pi_thinking_level(level: ReasoningLevel) -> Option<&'static str> {
    Some(match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max => "max",
        // Ultra/Ultracode/Ultrathink are harness-specific elsewhere; pi has
        // no equivalent — leave the agent default in place.
        _ => return None,
    })
}

/// Locate the installed pi CLI: `PI_EXECUTABLE`, then our own PATH, then
/// the login-shell PATH snapshot (GUI launches never see the shell init
/// that shapes PATH — see [`crate::shell_env`]), then the node version
/// manager / global bin dirs (pi installs via npm).
fn resolve_pi_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PI_EXECUTABLE")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) { "pi.exe" } else { "pi" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(shell_path) = crate::shell_env::login_shell_path() {
        candidates.extend(
            std::env::split_paths(shell_path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe)),
        );
    }
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|d| d.join(exe)),
    );
    candidates.into_iter().find(|p| p.exists())
}

/// The pi harness. Construct with [`PiHarness::new`]; tests point it at a
/// fake CLI with [`PiHarness::with_executable`].
pub struct PiHarness {
    executable: Option<PathBuf>,
    /// Grace between the `abort` command and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    /// Slash-command cache behind `commands()` — one `get_commands` probe
    /// per harness instance (the ACP pattern). Project-level commands
    /// (`./.pi/agent/`) are cwd-dependent; the probe inherits zeron's
    /// process cwd, so those are only right for the home project — the
    /// same limitation the ACP discovery probe has.
    commands_cache: tokio::sync::OnceCell<Vec<SlashCommand>>,
}

impl Default for PiHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            commands_cache: tokio::sync::OnceCell::new(),
        }
    }
}

impl PiHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Point at a specific binary (tests: `tests/fixtures/fake-pi.sh`).
    pub fn with_executable(mut self, path: PathBuf) -> Self {
        self.executable = Some(path);
        self
    }

    /// Tune the interrupt→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        resolve_pi_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "pi (searched PATH, the login shell's PATH, and fnm/nvm/volta/pnpm/bun \
                 install dirs; install with `npm install -g --ignore-scripts \
                 @earendil-works/pi-coding-agent`; set PI_EXECUTABLE to override)"
                    .into(),
            )
        })
    }

    /// Short-lived `pi --mode rpc --no-session` answering ONE command
    /// (model and slash-command discovery). Bounded so a wedged pi can't
    /// hang the pickers; the child is always shut down before returning.
    async fn rpc_probe(&self, command: Value, timeout_error: &str) -> Result<Value, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        cmd.args(["--mode", "rpc", "--no-session"]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi child has no stdout".into()))?;
        let (client, mut events) = PiClient::new(stdin, stdout);

        // Catalog/extension loading can take a beat on first run; bounded
        // so a wedged pi can't hang the picker.
        let data = tokio::select! {
            res = client.request(command) => match res {
                Ok(data) => data,
                Err(e) => {
                    shutdown_child(&mut child, Duration::from_secs(2)).await;
                    return Err(e);
                }
            },
            _ = tokio::time::sleep(Duration::from_secs(20)) => {
                shutdown_child(&mut child, Duration::from_secs(2)).await;
                return Err(HarnessError::Protocol(timeout_error.into()));
            }
        };
        shutdown_child(&mut child, Duration::from_secs(2)).await;
        // Drain the event channel so the reader task exits.
        while events.try_recv().is_ok() {}
        Ok(data)
    }

    /// pi's `get_commands` (extension commands, prompt templates, skills —
    /// skills ride as `skill:name`, invoked with `/skill:name`) mapped onto
    /// the composer's slash list. `input_hint` has no pi equivalent.
    async fn discover_commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        let data = self
            .rpc_probe(json!({"type": "get_commands"}), "pi get_commands timed out")
            .await?;
        let empty = Vec::new();
        let commands = data
            .get("commands")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or(&empty);
        Ok(commands
            .iter()
            .filter_map(|c| {
                let name = c.get("name").and_then(Value::as_str)?;
                let description = c
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                Some(SlashCommand {
                    name: name.to_owned(),
                    description,
                    input_hint: None,
                })
            })
            .collect())
    }

    /// Build the `pi --mode rpc` command for one run.
    fn build_command(&self, exe: &PathBuf, request: &RunRequest) -> Command {
        let mut cmd = Command::new(exe);
        cmd.arg("--mode").arg("rpc");
        if let Some(resume) = &request.resume {
            cmd.arg("--session").arg(resume);
        }
        if !request.cwd.is_empty() {
            cmd.current_dir(&request.cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

#[async_trait]
impl Harness for PiHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Pi
    }
    fn display_name(&self) -> &str {
        "Pi"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// Native `steer` injects after the current internal turn's tool calls,
    /// before the next LLM call; a steer that misses the settle boundary
    /// becomes the next `prompt` on the same session.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        REASONING_LEVELS
    }
    fn installed(&self) -> bool {
        self.executable.is_some() || resolve_pi_executable().is_some()
    }
    /// Done is pi's own `agent_settled`, for steered follow-up turns too.
    fn deterministic_turn_end(&self) -> bool {
        true
    }

    /// Live discovery: a short-lived `pi --mode rpc --no-session` answering
    /// `get_available_models`. Model ids ride the catalog as `provider/id`
    /// so [`run`] can split them back into `set_model`'s provider/modelId
    /// pair. Models without reasoning support advertise an empty ladder.
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let data = self
            .rpc_probe(
                json!({"type": "get_available_models"}),
                "pi get_available_models timed out",
            )
            .await?;

        let empty = Vec::new();
        let models = data
            .get("models")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or(&empty);
        Ok(models
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(Value::as_str)?;
                let provider = m.get("provider").and_then(Value::as_str)?;
                let name = m.get("name").and_then(Value::as_str).unwrap_or(id);
                let reasoning = m.get("reasoning").and_then(Value::as_bool).unwrap_or(false);
                let context = m
                    .get("contextWindow")
                    .and_then(Value::as_u64)
                    .map(|c| format!("{}k context window", c / 1000));
                Some(Model {
                    id: format!("{provider}/{id}"),
                    label: name.to_owned(),
                    description: context,
                    reasoning_levels: if reasoning {
                        REASONING_LEVELS.to_vec()
                    } else {
                        Vec::new()
                    },
                    options: Vec::new(),
                })
            })
            .collect())
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        self.commands_cache
            .get_or_try_init(|| self.discover_commands())
            .await
            .cloned()
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = self.build_command(&exe, &request);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            use tokio::io::AsyncBufReadExt as _;
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "zeron_harness::pi", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let (client, events) = PiClient::new(stdin, stdout);
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            client,
            events,
            event_tx,
            controls,
            request,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

struct Session {
    child: Child,
    client: PiClient,
    events: mpsc::Receiver<Value>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    interrupt_grace: Duration,
    kill_grace: Duration,
    stderr_tail: crate::StderrTail,
}

/// One settled pi run: text + tool events streamed, usage accumulated from
/// every assistant `message_end` (a tool-loop run spans several LLM calls),
/// and the terminal error (if pi's own retries were exhausted) remembered
/// for the Done.
#[derive(Default)]
struct TurnState {
    /// Accumulated usage of the run's assistant messages, held until
    /// `agent_settled` (emitted just before Done, codex-style).
    pending_usage: Option<AgentEvent>,
    /// Set when the final assistant message ended with `stopReason:
    /// "error"` — the run settled in failure.
    error: Option<String>,
    /// An assistant message ended `aborted` (user Esc / our `abort`).
    aborted: bool,
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Anthropic-style inline-image cap; larger files stay path refs (their path
/// also rides the prompt text via zeron's `withAttachments` transport).
const MAX_INLINE_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Media type for an inline image block — extension first, magic bytes as the
/// fallback (pasted screenshots may carry odd names).
fn image_media_type(path: &std::path::Path, bytes: &[u8]) -> Option<&'static str> {
    let by_ext = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    };
    by_ext.or(match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    })
}

/// Load `RunRequest::attachments` into pi's `ImageContent` blocks
/// (`{"type":"image","data":<base64>,"mimeType":...}`), best-effort: an
/// unreadable, oversized, or unsupported file is skipped — its path ref
/// still rides the prompt text — never fatal to the run.
async fn load_image_blocks(paths: &[String]) -> Vec<Value> {
    use base64::Engine as _;
    let mut blocks = Vec::new();
    for path in paths {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(target: "zeron_harness::pi", %path, error = %err, "attachment unreadable; path ref only");
                continue;
            }
        };
        if bytes.len() as u64 > MAX_INLINE_IMAGE_BYTES {
            tracing::debug!(target: "zeron_harness::pi", %path, "attachment over inline cap; path ref only");
            continue;
        }
        let Some(media_type) = image_media_type(std::path::Path::new(path), &bytes) else {
            tracing::debug!(target: "zeron_harness::pi", %path, "attachment not an inline-supported image; path ref only");
            continue;
        };
        blocks.push(json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "mimeType": media_type,
        }));
    }
    blocks
}

/// Rotate the assistant message id; returns (previous, next).
fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// The per-run event loop: one task multiplexing pi events, the steering
/// mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        mut child,
        client,
        mut events,
        event_tx,
        controls,
        request,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        // pi's RPC surface has no input-request mechanism today (no
        // AskUserQuestion equivalent) — the bridge is dropped; if pi grows
        // one, it lands here.
        request_input: _,
        mut steering,
        interrupt,
    } = controls;

    // ---- setup: get_state → model/thinking → SessionStarted → prompt ----
    // (interruptible, like the codex handshake)
    let setup = async {
        let state = client.request(json!({"type": "get_state"})).await?;
        let session_id = state
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut model_label = state
            .pointer("/model/name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        // Model: the catalog's `provider/id` ids split back into set_model's
        // provider + modelId pair. A bare id (foreign picker entry) is left
        // to pi's configured default.
        if let Some(model) = &request.model
            && let Some((provider, model_id)) = model.split_once('/')
        {
            let switched = client
                .request(json!({
                    "type": "set_model",
                    "provider": provider,
                    "modelId": model_id,
                }))
                .await?;
            // get_state ran BEFORE the switch: the label must come from the
            // set_model response (the full Model object) or SessionStarted
            // names the pre-switch model.
            if let Some(name) = switched.get("name").and_then(Value::as_str) {
                model_label = name.to_owned();
            }
        }
        // Thinking level: our ladder maps 1:1; harness-specific levels have
        // no pi equivalent and are skipped.
        if let Some(level) = request.reasoning
            && let Some(pi_level) = to_pi_thinking_level(level)
        {
            client
                .request(json!({
                    "type": "set_thinking_level",
                    "level": pi_level,
                }))
                .await?;
        }

        let images = load_image_blocks(&request.attachments).await;
        let mut prompt = json!({"type": "prompt", "message": request.prompt});
        if !images.is_empty() {
            prompt["images"] = Value::Array(images);
        }
        // The response means ACCEPTED (queued or running), never done —
        // a failure here (no model/API key) fails the whole run.
        client.request(prompt).await?;
        Ok::<(String, String), HarnessError>((session_id, model_label))
    };
    let (session_id, model_label) = tokio::select! {
        res = setup => match res {
            Ok(pair) => pair,
            Err(e) => {
                let _ = event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(e.to_string()),
                        session_id: None,
                    }))
                    .await;
                shutdown_child(&mut child, kill_grace).await;
                return;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };

    let mut assistant_message_id = new_message_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::Pi,
            model: model_label,
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    let mut turn = TurnState::default();
    // The initial prompt's turn is in flight from setup.
    let mut turn_active = true;
    // Tool calls in flight: call id → (tool name, args) from
    // tool_execution_start, consulted at tool_execution_end for the
    // structured diff (the end frame carries no args).
    let mut open_tools: HashMap<String, (String, Value)> = HashMap::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut any_done = false;
    let mut done_after_interrupt = false;
    // Steers whose `steer` command lost the settle race; delivered as the
    // next `prompt` once `agent_settled` lands.
    let mut queued_steers: std::collections::VecDeque<String> = Default::default();
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(frame) => {
                    let frame_type = frame.get("type").and_then(Value::as_str).unwrap_or("");
                    match frame_type {
                        "message_update" => {
                            let ame = frame.get("assistantMessageEvent");
                            let kind = ame
                                .and_then(|a| a.get("type"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let delta = || {
                                ame.and_then(|a| a.get("delta"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned()
                            };
                            let ev = match kind {
                                "text_delta" => Some(AgentEvent::TextDelta { text: delta() }),
                                "thinking_delta" => {
                                    Some(AgentEvent::ReasoningDelta { text: delta() })
                                }
                                _ => None,
                            };
                            if let Some(ev) = ev && !send(&event_tx, ev).await {
                                break 'main;
                            }
                        }

                        "message_end" => {
                            let message = frame.get("message");
                            if message
                                .and_then(|m| m.get("role"))
                                .and_then(Value::as_str)
                                == Some("assistant")
                            {
                                // Usage accumulates across the run's
                                // internal turns (each assistant message_end
                                // carries ITS LLM call's usage; a tool-loop
                                // run spans several). Held to the settle
                                // boundary, emitted once before Done.
                                let usage = message
                                    .and_then(|m| m.pointer("/usage"))
                                    .and_then(|u| {
                                        let input = u.get("input").and_then(Value::as_u64)?;
                                        let output = u.get("output").and_then(Value::as_u64)?;
                                        Some((input, output))
                                    });
                                if let Some((input, output)) = usage {
                                    let prev = match turn.pending_usage.take() {
                                        Some(AgentEvent::Usage {
                                            input_tokens,
                                            output_tokens,
                                        }) => (input_tokens, output_tokens),
                                        _ => (0, 0),
                                    };
                                    turn.pending_usage = Some(AgentEvent::Usage {
                                        input_tokens: prev.0 + input,
                                        output_tokens: prev.1 + output,
                                    });
                                }
                                match message
                                    .and_then(|m| m.get("stopReason"))
                                    .and_then(Value::as_str)
                                {
                                    Some("error") => {
                                        turn.error = Some(
                                            message
                                                .and_then(|m| m.get("errorMessage"))
                                                .and_then(Value::as_str)
                                                .unwrap_or("pi agent error")
                                                .to_owned(),
                                        );
                                    }
                                    Some("aborted") => turn.aborted = true,
                                    // A clean message_end supersedes an
                                    // earlier failed attempt: pi auto-retries
                                    // transient errors IN TURN, stripping the
                                    // failed assistant message — the last
                                    // message_end before agent_settled is the
                                    // truth.
                                    _ => {
                                        turn.error = None;
                                        turn.aborted = false;
                                    }
                                }
                            }
                        }

                        "tool_execution_start" => {
                            if let (Some(id), Some(name)) = (
                                frame.get("toolCallId").and_then(Value::as_str),
                                frame.get("toolName").and_then(Value::as_str),
                            ) {
                                open_tools.insert(
                                    id.to_owned(),
                                    (
                                        name.to_owned(),
                                        frame.get("args").cloned().unwrap_or(Value::Null),
                                    ),
                                );
                            }
                            let ev = tool_call_event(&frame);
                            if let Some(ev) = ev && !send(&event_tx, ev).await {
                                break 'main;
                            }
                        }

                        "tool_execution_end" => {
                            let ev = tool_result_event(&frame, &open_tools);
                            if let (Some(id), Some(ev)) = (
                                frame.get("toolCallId").and_then(Value::as_str),
                                ev,
                            ) {
                                open_tools.remove(id);
                                if !send(&event_tx, ev).await {
                                    break 'main;
                                }
                            }
                        }

                        "agent_settled" => {
                            // The turn settles: emit held usage, then Done.
                            // Retries and compaction are pi's problem — it
                            // emits agent_settled only when nothing else is
                            // coming automatically.
                            turn_active = false;
                            if let Some(usage) = turn.pending_usage.take()
                                && !send(&event_tx, usage).await
                            {
                                break 'main;
                            }
                            let status = if interrupted || turn.aborted {
                                DoneStatus::Interrupted
                            } else if turn.error.is_some() {
                                DoneStatus::Errored
                            } else {
                                DoneStatus::Completed
                            };
                            any_done = true;
                            if !send(
                                &event_tx,
                                AgentEvent::Done {
                                    status,
                                    result: None,
                                    error: turn.error.clone(),
                                    session_id: Some(session_id.clone()),
                                },
                            )
                            .await
                            {
                                break 'main;
                            }
                            if interrupted {
                                done_after_interrupt = true;
                                break 'main;
                            }
                            // A steer that lost the settle race becomes the
                            // next turn now; otherwise stay alive for the
                            // mailbox — the caller owns teardown.
                            if let Some(text) = queued_steers.pop_front() {
                                if !start_turn(
                                    &client,
                                    &text,
                                    &event_tx,
                                    &mut turn,
                                    &mut turn_active,
                                    &mut assistant_message_id,
                                )
                                .await
                                {
                                    break 'main;
                                }
                            } else if !steering_open {
                                break 'main;
                            }
                        }

                        "extension_ui_request" => {
                            // Extension UI sub-protocol: fire-and-forget
                            // methods (notify/setStatus/setWidget/setTitle/
                            // set_editor_text) are display noise — ignored.
                            // DIALOG methods (select/confirm/input/editor)
                            // BLOCK the agent until an extension_ui_response
                            // arrives; without one the turn hangs forever
                            // (dialogs without a timeout never auto-resolve).
                            // zeron has no dialog surface for pi today, so
                            // answer every dialog with cancelled — a refused
                            // prompt beats a wedged run.
                            let is_dialog = matches!(
                                frame.get("method").and_then(Value::as_str),
                                Some("select" | "confirm" | "input" | "editor")
                            );
                            if is_dialog {
                                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                                tracing::debug!(
                                    target: "zeron_harness::pi",
                                    "auto-cancelling extension dialog {id}"
                                );
                                client.fire(json!({
                                    "type": "extension_ui_response",
                                    "id": id,
                                    "cancelled": true,
                                }));
                            }
                        }

                        "extension_error" => {
                            let message = frame
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("pi extension error")
                                .to_owned();
                            if !send(&event_tx, AgentEvent::Error { message }).await {
                                break 'main;
                            }
                        }

                        // agent_start/agent_end/turn_*/message_start,
                        // queue_update, compaction_*, auto_retry_*,
                        // summarization_retry_*: lifecycle noise the loop
                        // tracks through agent_settled alone. Unknown event
                        // types are tolerated by design.
                        _ => {}
                    }
                }
                None => break 'main, // reader EOF: pi exited
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    if turn_active {
                        // Native mid-turn injection.
                        match client
                            .request(json!({"type": "steer", "message": msg.prompt}))
                            .await
                        {
                            Ok(_) => {
                                let (prev, next) = rotate(&mut assistant_message_id);
                                if !send(
                                    &event_tx,
                                    AgentEvent::Steered {
                                        assistant_message_id: Some(prev),
                                        next_assistant_message_id: Some(next),
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                            // A failed `steer` does NOT mean the text is bad:
                            // most commonly the turn settled between the UI
                            // send and this command (pi rejects with "not
                            // streaming"). Queue for redelivery as the next
                            // prompt when agent_settled lands — the same
                            // boundary the settle event is about to hit.
                            Err(e) => {
                                tracing::debug!(
                                    target: "zeron_harness::pi",
                                    "steer rejected (queued as next prompt): {e}"
                                );
                                queued_steers.push_back(msg.prompt);
                            }
                        }
                    } else {
                        // Between turns on a live session: the steer starts
                        // the next turn directly.
                        if !start_turn(
                            &client,
                            &msg.prompt,
                            &event_tx,
                            &mut turn,
                            &mut turn_active,
                            &mut assistant_message_id,
                        )
                        .await
                        {
                            break 'main;
                        }
                    }
                }
                None => {
                    // Mailbox closed: finish the current turn, then the run.
                    steering_open = false;
                    if !turn_active {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                // Protocol-level abort; pi settles the run and emits
                // agent_settled (which closes with Done::Interrupted).
                client.fire(json!({"type": "abort"}));
                // Escalate if pi doesn't wind down within the grace
                // periods: SIGTERM (pi's own shutdown path), then SIGKILL.
                if let Some(pid) = child.id() {
                    escalation = Some(tokio::spawn(async move {
                        tokio::time::sleep(interrupt_grace).await;
                        send_signal(pid, Signal::Term);
                        tokio::time::sleep(kill_grace).await;
                        send_signal(pid, Signal::Kill);
                    }));
                }
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the
    // consumer already hung up.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(session_id),
                }))
                .await;
        } else if !interrupted && !any_done {
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message("pi", status, &stderr_tail)),
                    session_id: None,
                }))
                .await;
        }
    }

    shutdown_child(&mut child, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
}

/// Start a new turn on the live session (a post-settle steer, or the
/// redelivery of one that lost the settle race). Emits the Steered marker
/// once the prompt is accepted so post-steer output folds into a fresh
/// assistant message. Returns false when the loop should end.
async fn start_turn(
    client: &PiClient,
    text: &str,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    turn: &mut TurnState,
    turn_active: &mut bool,
    assistant_message_id: &mut String,
) -> bool {
    *turn = TurnState::default();
    // Prompt first, THEN the Steered rotation (codex's steer_as_new_turn
    // ordering): a rejected prompt must not rotate the message id for a
    // turn that never started. Events arriving during the await sit in the
    // channel until the loop resumes, so the marker still lands before any
    // of the new turn's deltas.
    match client
        .request(json!({"type": "prompt", "message": text}))
        .await
    {
        Ok(_) => {
            *turn_active = true;
            let (prev, next) = rotate(assistant_message_id);
            send(
                event_tx,
                AgentEvent::Steered {
                    assistant_message_id: Some(prev),
                    next_assistant_message_id: Some(next),
                },
            )
            .await
        }
        Err(e) => {
            let _ = send(
                event_tx,
                AgentEvent::Error {
                    message: format!("Steering failed: {e}"),
                },
            )
            .await;
            false
        }
    }
}

/// `tool_execution_start` → [`AgentEvent::ToolCall`]. pi's built-in tool
/// names map onto the wire kinds (bash→Exec, read→ReadFile, write/edit→
/// file mutation with the args' text, grep→Search, find→Glob); anything
/// else rides as Unknown so the chip still renders.
fn tool_call_event(frame: &Value) -> Option<AgentEvent> {
    let id = frame.get("toolCallId").and_then(Value::as_str)?;
    let name = frame.get("toolName").and_then(Value::as_str).unwrap_or("");
    let args = frame.get("args").cloned().unwrap_or(Value::Null);
    let str_field = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let call = match name {
        "bash" => ToolCall::Exec {
            command: str_field("command"),
        },
        "read" => ToolCall::ReadFile {
            path: str_field("path"),
        },
        "ls" => ToolCall::ReadFile {
            // A directory read — renders as a read chip, like the ACP
            // adapter's "read" kind mapping for ls.
            path: str_field("path"),
        },
        "write" => ToolCall::WriteFile {
            path: str_field("path"),
            content: args
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "edit" => ToolCall::EditFile {
            path: str_field("path"),
            old_string: args
                .get("oldText")
                .and_then(Value::as_str)
                .map(str::to_owned),
            new_string: args
                .get("newText")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "grep" => ToolCall::Search {
            pattern: str_field("pattern"),
            path: args.get("path").and_then(Value::as_str).map(str::to_owned),
        },
        "find" => ToolCall::Glob {
            pattern: str_field("pattern"),
        },
        _ => ToolCall::Unknown {
            name: name.to_owned(),
            input: args.as_object().cloned().map(Value::Object),
        },
    };
    Some(AgentEvent::ToolCall {
        id: id.to_owned(),
        call,
    })
}

/// `tool_execution_end` → [`AgentEvent::ToolResult`] with output text
/// (content blocks joined) and, for file mutations, a structured diff
/// built from the call's start-frame args (the end frame carries no args;
/// pi's own `details.diff` is display-oriented line output, while the args
/// hold the exact old/new text).
fn tool_result_event(
    frame: &Value,
    open_tools: &HashMap<String, (String, Value)>,
) -> Option<AgentEvent> {
    let id = frame.get("toolCallId").and_then(Value::as_str)?;
    let is_error = frame
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let output = frame
        .pointer("/result/content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_owned))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty());

    let diff = open_tools.get(id).and_then(|(name, args)| {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match name.as_str() {
            "edit" => Some(ToolDiff {
                path,
                old_text: args
                    .get("oldText")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                new_text: args
                    .get("newText")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            }),
            "write" => Some(ToolDiff {
                path,
                old_text: None,
                new_text: args
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            }),
            _ => None,
        }
    });

    Some(AgentEvent::ToolResult {
        id: id.to_owned(),
        is_error,
        output,
        diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_ladder_maps_to_pi_levels() {
        assert_eq!(to_pi_thinking_level(ReasoningLevel::Medium), Some("medium"));
        assert_eq!(to_pi_thinking_level(ReasoningLevel::Max), Some("max"));
        // pi has no equivalent for the harness-specific tiers.
        assert_eq!(to_pi_thinking_level(ReasoningLevel::Ultra), None);
        assert_eq!(to_pi_thinking_level(ReasoningLevel::Ultrathink), None);
    }

    #[test]
    fn tool_results_carry_output_and_structured_diff() {
        let mut open: HashMap<String, (String, Value)> = HashMap::new();
        open.insert(
            "c2".into(),
            (
                "edit".into(),
                json!({"path": "/w/a.rs", "oldText": "a", "newText": "b"}),
            ),
        );
        let frame = json!({
            "type": "tool_execution_end",
            "toolCallId": "c2",
            "isError": false,
            "result": {"content": [{"type": "text", "text": "ok"}]}
        });
        match tool_result_event(&frame, &open) {
            Some(AgentEvent::ToolResult {
                is_error,
                output,
                diff: Some(diff),
                ..
            }) => {
                assert!(!is_error);
                assert_eq!(output.as_deref(), Some("ok"));
                assert_eq!(diff.path, "/w/a.rs");
                assert_eq!(diff.old_text.as_deref(), Some("a"));
                assert_eq!(diff.new_text, "b");
            }
            other => panic!("expected ToolResult with diff, got {other:?}"),
        }

        // Without a start frame (result for an unknown call), no diff —
        // never a panic.
        let frame = json!({
            "type": "tool_execution_end",
            "toolCallId": "ghost",
            "isError": true,
            "result": {"content": [{"type": "text", "text": "boom"}]}
        });
        match tool_result_event(&frame, &open) {
            Some(AgentEvent::ToolResult {
                is_error,
                output,
                diff,
                ..
            }) => {
                assert!(is_error);
                assert_eq!(output.as_deref(), Some("boom"));
                assert!(diff.is_none());
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn tool_calls_map_to_wire_kinds() {
        let frame = json!({
            "type": "tool_execution_start",
            "toolCallId": "c1",
            "toolName": "bash",
            "args": {"command": "cargo test"}
        });
        match tool_call_event(&frame) {
            Some(AgentEvent::ToolCall {
                call: ToolCall::Exec { command },
                ..
            }) => assert_eq!(command, "cargo test"),
            other => panic!("expected Exec, got {other:?}"),
        }

        let frame = json!({
            "type": "tool_execution_start",
            "toolCallId": "c2",
            "toolName": "edit",
            "args": {"path": "/w/a.rs", "oldText": "a", "newText": "b"}
        });
        match tool_call_event(&frame) {
            Some(AgentEvent::ToolCall {
                call:
                    ToolCall::EditFile {
                        path,
                        old_string,
                        new_string,
                    },
                ..
            }) => {
                assert_eq!(path, "/w/a.rs");
                assert_eq!(old_string.as_deref(), Some("a"));
                assert_eq!(new_string.as_deref(), Some("b"));
            }
            other => panic!("expected EditFile, got {other:?}"),
        }

        // Unknown tools ride as Unknown, never dropped.
        let frame = json!({
            "type": "tool_execution_start",
            "toolCallId": "c3",
            "toolName": "venice_image_generate",
            "args": {"prompt": "a cat"}
        });
        match tool_call_event(&frame) {
            Some(AgentEvent::ToolCall {
                call: ToolCall::Unknown { name, .. },
                ..
            }) => assert_eq!(name, "venice_image_generate"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}
