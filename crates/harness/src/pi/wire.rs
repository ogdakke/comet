//! pi RPC wire client: newline-framed JSONL over the child's stdio.
//!
//! pi's RPC mode is NOT JSON-RPC 2.0 (unlike codex app-server and ACP):
//! commands are `{"id": N, "type": "prompt", ...}` objects, responses
//! `{"type": "response", "command": "prompt", "id": N, "success": bool,
//! "data": ...}` — matched by id — and agent events are bare
//! `{"type": "agent_start", ...}` objects with no id. The reader task
//! resolves pending responses directly and pumps events into a channel the
//! session loop drains (same shape as [`crate::jsonrpc`], different frame).
//!
//! Framing is strict LF-delimited JSONL: records split on `\n` only (pi's
//! docs call out Node's `readline` as non-compliant because it also splits
//! on U+2028/U+2029 — `AsyncBufReadExt::lines` splits on `\n` and strips an
//! optional trailing `\r`, which is exactly the documented contract).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use crate::HarnessError;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Clone)]
pub(crate) struct PiClient {
    next_id: Arc<AtomicI64>,
    pending: Pending,
    writer: mpsc::UnboundedSender<String>,
}

impl PiClient {
    /// Spawn the writer + reader tasks over the child's stdio; returns the
    /// client and the event channel (every non-response line, in stdout
    /// order). Generic over the IO halves so tests can drive it with
    /// duplexes; production passes `ChildStdin`/`ChildStdout`.
    pub fn new<S, R>(stdin: S, stdout: R) -> (Self, mpsc::Receiver<Value>)
    where
        S: AsyncWrite + Unpin + Send + 'static,
        R: AsyncRead + Unpin + Send + 'static,
    {
        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(write_loop(stdin, writer_rx));
        let pending: Pending = Arc::default();
        let (event_tx, event_rx) = mpsc::channel::<Value>(256);
        tokio::spawn(read_loop(stdout, Arc::clone(&pending), event_tx));
        (
            Self {
                next_id: Arc::new(AtomicI64::new(0)),
                pending,
                writer: writer_tx,
            },
            event_rx,
        )
    }

    /// Send a command object (its `type` field names the command) and await
    /// its response `data`. A `success: false` response resolves to
    /// `Err(error message)` — the caller decides whether that's fatal (a
    /// rejected `prompt`) or a race to recover from (a `steer` that lost the
    /// settle boundary).
    pub async fn request(&self, mut command: Value) -> Result<Value, HarnessError> {
        let name = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>")
            .to_owned();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        if let Value::Object(map) = &mut command {
            map.insert("id".into(), Value::from(id));
        }
        if self.writer.send(command.to_string()).is_err() {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(HarnessError::Protocol(format!("{name}: pi stdin closed")));
        }
        match rx.await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(message)) => Err(HarnessError::Protocol(format!("{name}: {message}"))),
            // Sender dropped: the reader hit EOF and failed all pending.
            Err(_) => Err(HarnessError::Protocol(format!(
                "{name}: pi exited before responding"
            ))),
        }
    }

    /// Fire a command without awaiting its response (interrupt path: `abort`
    /// must never wedge the loop if the child is already dying).
    pub fn fire(&self, mut command: Value) {
        if let Value::Object(map) = &mut command {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
            map.insert("id".into(), Value::from(id));
        }
        let _ = self.writer.send(command.to_string());
    }
}

async fn write_loop<S>(mut stdin: S, mut rx: mpsc::UnboundedReceiver<String>)
where
    S: AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;
    while let Some(line) = rx.recv().await {
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(e) = write.await {
            // EPIPE after the child died — tolerated, like the other drivers.
            tracing::debug!(target: "zeron_harness::pi", "stdin write failed (tolerated): {e}");
            return;
        }
    }
}

async fn read_loop<R>(stdout: R, pending: Pending, event_tx: mpsc::Sender<Value>)
where
    R: AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt as _;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let frame: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(target: "zeron_harness::pi", "unparseable line (skipped): {e}");
                        continue;
                    }
                };
                if frame.get("type").and_then(Value::as_str) == Some("response") {
                    if let Some(id) = frame.get("id").and_then(Value::as_i64) {
                        if let Some(tx) = pending.lock().expect("pending lock").remove(&id) {
                            let success = frame
                                .get("success")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            if success {
                                let _ =
                                    tx.send(Ok(frame.get("data").cloned().unwrap_or(Value::Null)));
                            } else {
                                let message = frame
                                    .get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("command failed")
                                    .to_owned();
                                let _ = tx.send(Err(message));
                            }
                            continue;
                        }
                    }
                    // A response with no pending caller (or no id): nothing
                    // to do — `fire`d commands land here.
                    continue;
                }
                // Everything else is an agent event.
                if event_tx.send(frame).await.is_err() {
                    break; // session loop gone — stop reading
                }
            }
            Ok(None) | Err(_) => {
                // stdout EOF or read error: the pi process exited. Fail all
                // pending requests so awaited commands don't hang.
                let waiters: Vec<oneshot::Sender<Result<Value, String>>> = {
                    let mut map = pending.lock().expect("pending lock");
                    map.drain().map(|(_, tx)| tx).collect()
                };
                for tx in waiters {
                    let _ = tx.send(Err("pi exited".into()));
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Response routing by id; events flow past untouched. The fake pi
    /// answers each command only after reading it, so responses can never
    /// beat their pending-map registration.
    #[tokio::test]
    async fn responses_match_by_id_and_events_stream() {
        use tokio::io::AsyncBufReadExt as _;
        let (stdin_w, stdin_r) = tokio::io::duplex(4096);
        let (mut stdout_w, stdout_r) = tokio::io::duplex(4096);
        use tokio::io::AsyncWriteExt as _;
        let writer = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdin_r);
            let mut buf = String::new();

            // Command 1 (prompt): answer success after an interleaved event
            // so the reader must handle both kinds in one stream.
            reader.read_line(&mut buf).await.unwrap();
            stdout_w
                .write_all(b"{\"type\":\"agent_start\"}\n")
                .await
                .unwrap();
            stdout_w
                .write_all(
                    b"{\"type\":\"response\",\"command\":\"prompt\",\"id\":1,\"success\":true,\"data\":{\"ok\":true}}\n",
                )
                .await
                .unwrap();

            // Command 2 (steer): a failure carries its error message.
            reader.read_line(&mut buf).await.unwrap();
            stdout_w
                .write_all(
                    b"{\"type\":\"response\",\"command\":\"steer\",\"id\":2,\"success\":false,\"error\":\"not streaming\"}\n",
                )
                .await
                .unwrap();
            buf
        });

        let (client, mut events) = PiClient::new(stdin_w, stdout_r);

        let ok = client
            .request(json!({"type": "prompt", "message": "hi"}))
            .await
            .expect("success response");
        assert_eq!(ok, json!({"ok": true}));

        let err = client
            .request(json!({"type": "steer", "message": "x"}))
            .await
            .expect_err("failure response");
        assert!(err.to_string().contains("not streaming"), "{err}");

        let ev = events.recv().await.expect("agent_start event");
        assert_eq!(ev["type"], "agent_start");

        let written = writer.await.unwrap();
        assert!(written.contains("\"id\":1"));
        assert!(written.contains("\"id\":2"));
    }
}
