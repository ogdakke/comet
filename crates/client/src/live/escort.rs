//! Attachment escorts (queued-attachments flow, hosts ≥ 0.2.12).
//!
//! A send with images writes its command at once with `pending://{id}/{name}`
//! refs; the bytes are stashed on disk and pushed to the host with chunked
//! `UploadChunk`/`UploadCommit` under the SAME upload id + file name the ref
//! names (the host resolves the ref to that path and defers the command
//! until it lands). Escorts retry with backoff for up to the host's wait
//! window and are re-derived from the stash on relaunch.
//!
//! A host with `attachments-r2-v1` takes the bytes through the edge instead:
//! the escort sends the stash as transfer `{uploadId}` (8 MiB parts, resumable,
//! up to 4 GiB) and the host fetches it from there. Where the edge has no
//! transfer routes, files the relay can carry go that way.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::client::ClientInner;
use crate::{lock, now_ms};

/// The host stops waiting for a deferred attachment after this long.
const ESCORT_MAX_MS: i64 = 15 * 60_000;
/// Through the edge the host waits while parts keep arriving; the transfer's
/// write token lasts 48 h.
const EDGE_ESCORT_MAX_MS: i64 = 48 * 60 * 60_000;
const BACKOFF_BASE: Duration = Duration::from_secs(2);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
const EDGE_BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);
/// The host's own cap on relayed attachments (uploads.rs MAX_BYTES).
const RELAY_MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StashMeta {
    pub upload_id: String,
    pub chat_id: String,
    pub host_device_id: String,
    pub name: String,
    pub created_at_ms: i64,
    /// Send through the edge (the host fetches it there).
    #[serde(default)]
    pub edge: bool,
    #[serde(default)]
    pub mime_type: String,
}

pub(crate) struct Escorts {
    dir: PathBuf,
    active: Mutex<HashSet<String>>,
}

impl Escorts {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("uploads"),
            active: Mutex::new(HashSet::new()),
        }
    }

    fn bin(&self, upload_id: &str) -> PathBuf {
        self.dir.join(format!("{upload_id}.bin"))
    }

    fn meta(&self, upload_id: &str) -> PathBuf {
        self.dir.join(format!("{upload_id}.json"))
    }

    /// Durable copy of the bytes (before the command is written).
    pub(crate) fn stash(&self, meta: &StashMeta, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.bin(&meta.upload_id), bytes)?;
        std::fs::write(
            self.meta(&meta.upload_id),
            serde_json::to_vec(meta).unwrap_or_default(),
        )
    }

    /// Durable copy of a file on disk: moved in when it can be (the caller
    /// handed it over), copied otherwise. Never read into memory.
    pub(crate) fn stash_file(&self, meta: &StashMeta, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let bin = self.bin(&meta.upload_id);
        if std::fs::rename(path, &bin).is_err() {
            std::fs::copy(path, &bin)?;
            let _ = std::fs::remove_file(path);
        }
        std::fs::write(
            self.meta(&meta.upload_id),
            serde_json::to_vec(meta).unwrap_or_default(),
        )
    }

    fn remove(&self, upload_id: &str) {
        let _ = std::fs::remove_file(self.bin(upload_id));
        let _ = std::fs::remove_file(self.meta(upload_id));
        let _ = std::fs::remove_file(self.plan(upload_id));
    }

    fn plan(&self, upload_id: &str) -> PathBuf {
        self.dir.join(format!("{upload_id}.plan"))
    }

    fn load_meta(&self, upload_id: &str) -> Option<StashMeta> {
        serde_json::from_slice(&std::fs::read(self.meta(upload_id)).ok()?).ok()
    }

    fn save_meta(&self, meta: &StashMeta) {
        let _ = std::fs::write(
            self.meta(&meta.upload_id),
            serde_json::to_vec(meta).unwrap_or_default(),
        );
    }

    /// The stash's parts and their hashes (hashed once, then kept).
    async fn manifest(&self, meta: &StashMeta) -> std::io::Result<zeron_transfer::Manifest> {
        let plan = self.plan(&meta.upload_id);
        let parts = match std::fs::read(&plan)
            .ok()
            .and_then(|b| serde_json::from_slice::<(u64, Vec<zeron_transfer::Part>)>(&b).ok())
        {
            Some(planned) => planned,
            None => {
                let bin = self.bin(&meta.upload_id);
                let planned = tokio::task::spawn_blocking(move || {
                    zeron_transfer::plan_file(&bin, zeron_transfer::PART_SIZE)
                })
                .await
                .map_err(std::io::Error::other)??;
                let _ = std::fs::write(&plan, serde_json::to_vec(&planned).unwrap_or_default());
                planned
            }
        };
        let (size, parts) = parts;
        Ok(zeron_transfer::Manifest {
            name: meta.name.clone(),
            mime: if meta.mime_type.is_empty() {
                "application/octet-stream".into()
            } else {
                meta.mime_type.clone()
            },
            size,
            part_size: zeron_transfer::PART_SIZE,
            parts,
            direction: zeron_transfer::Direction::Up,
            notify_device: Some(meta.host_device_id.clone()),
        })
    }

    /// Stashed escorts not yet delivered for `chat_id`.
    pub(crate) fn pending_for(&self, chat_id: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".json")?.to_owned();
                let meta: StashMeta =
                    serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
                (meta.chat_id == chat_id).then_some(id)
            })
            .collect()
    }

    /// Relaunch: re-derive every stashed escort.
    pub(crate) fn respawn(&self, inner: &Arc<ClientInner>) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json") {
                self.spawn(inner, id);
            }
        }
    }

    /// Retry (user-driven): restart every escort for the chat.
    pub(crate) fn respawn_chat(&self, inner: &Arc<ClientInner>, chat_id: &str) {
        for id in self.pending_for(chat_id) {
            self.spawn(inner, &id);
        }
    }

    pub(crate) fn spawn(&self, inner: &Arc<ClientInner>, upload_id: &str) {
        if !lock(&self.active).insert(upload_id.to_owned()) {
            return;
        }
        let weak: Weak<ClientInner> = Arc::downgrade(inner);
        let upload_id = upload_id.to_owned();
        let cancel = inner.cancel.clone();
        crate::runtime::shared().spawn(async move {
            run(weak.clone(), &upload_id, cancel).await;
            if let Some(inner) = weak.upgrade()
                && let Some(live) = inner.live()
            {
                lock(&live.escorts.active).remove(&upload_id);
            }
        });
    }
}

async fn run(
    weak: Weak<ClientInner>,
    upload_id: &str,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut backoff = BACKOFF_BASE;
    loop {
        let Some(inner) = weak.upgrade() else { return };
        let Some(live) = inner.live() else { return };
        let Some(mut meta) = live.escorts.load_meta(upload_id) else {
            return;
        };
        let max = if meta.edge {
            EDGE_ESCORT_MAX_MS
        } else {
            ESCORT_MAX_MS
        };
        if now_ms() - meta.created_at_ms > max {
            tracing::warn!(upload = %upload_id, chat = %meta.chat_id, "attachment escort expired");
            live.escorts.remove(upload_id);
            return;
        }
        let bin = live.escorts.bin(upload_id);
        let Ok(size) = std::fs::metadata(&bin).map(|m| m.len()) else {
            live.escorts.remove(upload_id);
            return;
        };
        let core = inner.session_core(&meta.chat_id);
        let progress: Option<crate::rpc::ProgressFn> = core.as_ref().map(|core| {
            let core = Arc::downgrade(core);
            Arc::new(move |fraction: f64| {
                if let Some(core) = core.upgrade() {
                    core.set_transfer_progress(Some(fraction));
                }
            }) as crate::rpc::ProgressFn
        });
        let outcome = if meta.edge {
            send_via_edge(&inner, &live.escorts, &meta, &bin, progress).await
        } else if size <= RELAY_MAX_BYTES {
            match std::fs::read(&bin) {
                Ok(bytes) => live
                    .relay
                    .upload(
                        &meta.host_device_id,
                        upload_id,
                        &meta.name,
                        &bytes,
                        progress,
                    )
                    .await
                    .map(drop)
                    .map_err(|e| Outcome::Retry(e.to_string())),
                Err(e) => Err(Outcome::GiveUp(e.to_string())),
            }
        } else {
            Err(Outcome::GiveUp(format!(
                "{size} bytes is too large for the relay"
            )))
        };
        if let Some(core) = &core {
            core.set_transfer_progress(None);
        }
        match outcome {
            Ok(()) => {
                tracing::info!(upload = %upload_id, edge = meta.edge, "attachment escorted");
                live.escorts.remove(upload_id);
                inner.nudge_host(&meta.chat_id);
                return;
            }
            Err(Outcome::Relay(why)) if size <= RELAY_MAX_BYTES => {
                tracing::info!(upload = %upload_id, %why, "edge can't take the attachment; using the relay");
                meta.edge = false;
                live.escorts.save_meta(&meta);
                continue;
            }
            Err(Outcome::Relay(err) | Outcome::Retry(err)) => {
                tracing::warn!(upload = %upload_id, error = %err, "attachment escort failed; retrying");
            }
            Err(Outcome::GiveUp(err)) => {
                tracing::warn!(upload = %upload_id, error = %err, "attachment escort gave up");
                live.escorts.remove(upload_id);
                return;
            }
        }
        drop(inner);
        if !super::wait_backoff(&cancel, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(if meta.edge {
            EDGE_BACKOFF_CAP
        } else {
            BACKOFF_CAP
        });
    }
}

enum Outcome {
    /// Try again after a backoff.
    Retry(String),
    /// The edge can't take it (no transfer routes): the relay may.
    Relay(String),
    GiveUp(String),
}

async fn send_via_edge(
    inner: &Arc<ClientInner>,
    escorts: &Escorts,
    meta: &StashMeta,
    bin: &Path,
    progress: Option<crate::rpc::ProgressFn>,
) -> Result<(), Outcome> {
    use zeron_transfer::TransferError as E;
    let client = crate::shared_files::edge_client(inner)
        .ok_or_else(|| Outcome::Relay("signed out".into()))?;
    let manifest = escorts
        .manifest(meta)
        .await
        .map_err(|e| Outcome::GiveUp(e.to_string()))?;
    let source = zeron_transfer::PartSource::File(bin.to_path_buf());
    let report = progress.map(|p| {
        Arc::new(move |sent: u64, total: u64| {
            p(if total == 0 {
                1.0
            } else {
                sent as f64 / total as f64
            })
        }) as Arc<dyn Fn(u64, u64) + Send + Sync>
    });
    let job = zeron_transfer::Upload {
        chat_id: &meta.chat_id,
        transfer_id: &meta.upload_id,
        manifest: &manifest,
        source: &source,
        progress: report,
    };
    zeron_transfer::upload(&client, job)
        .await
        .map_err(|e| match e {
            // No `/xfer` on this edge (older deploy) or no signing secret.
            E::NotFound | E::Unavailable { status: 503, .. } => Outcome::Relay(e.to_string()),
            E::Cancelled | E::TooLarge | E::BadId(_) | E::Refused(_) => {
                Outcome::GiveUp(e.to_string())
            }
            other => Outcome::Retry(other.to_string()),
        })
}
