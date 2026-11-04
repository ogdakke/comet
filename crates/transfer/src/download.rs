//! Receiving a file: parts written into place, verified, resumable.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::{StreamExt as _, TryStreamExt as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

use crate::upload::with_retries;
use crate::{EdgeClient, Manifest, PARALLEL_PARTS, TransferError, hex};

const PARTIAL: &str = "data.partial";
const STATE: &str = "state.json";

/// Which parts of `data.partial` are verified, for resuming after a restart.
#[derive(Default, Serialize, Deserialize)]
struct State {
    transfer_id: String,
    size: u64,
    done: BTreeSet<usize>,
}

async fn load_state(work: &Path, transfer_id: &str, size: u64) -> State {
    let fresh = || State {
        transfer_id: transfer_id.to_owned(),
        size,
        done: BTreeSet::new(),
    };
    match tokio::fs::read(work.join(STATE)).await {
        Ok(bytes) => serde_json::from_slice::<State>(&bytes)
            .ok()
            .filter(|s| s.transfer_id == transfer_id && s.size == size)
            .unwrap_or_else(fresh),
        Err(_) => fresh(),
    }
}

async fn save_state(work: &Path, state: &State) -> std::io::Result<()> {
    let tmp = work.join(format!("{STATE}.tmp"));
    tokio::fs::write(&tmp, serde_json::to_vec(state)?).await?;
    tokio::fs::rename(&tmp, work.join(STATE)).await
}

/// Write part `n` at its offset in `partial`, hashing as it streams.
async fn fetch_part(
    client: &EdgeClient,
    chat_id: &str,
    transfer_id: &str,
    manifest: &Manifest,
    n: usize,
    partial: &Path,
) -> Result<(), TransferError> {
    let want = &manifest.parts[n];
    let response = client.get_part(chat_id, transfer_id, n).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(partial)
        .await?;
    file.seek(std::io::SeekFrom::Start(manifest.offset(n)))
        .await?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        written += chunk.len() as u64;
        if written > want.size {
            return Err(TransferError::Integrity(n));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    if written != want.size || hex(&hasher.finalize()) != want.sha256 {
        return Err(TransferError::Integrity(n));
    }
    file.sync_data().await?;
    Ok(())
}

/// Fetch a sealed transfer into `dest`, using `work` (a directory on the same
/// filesystem, private to this transfer) for the partial file and progress.
/// Interrupted, call it again with the same `work` to resume; it fails with
/// [`TransferError::NotReady`] while the sender is still uploading.
pub async fn download(
    client: &EdgeClient,
    chat_id: &str,
    transfer_id: &str,
    dest: &Path,
    work: &Path,
    progress: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
) -> Result<Manifest, TransferError> {
    let status = with_retries(|| client.status(chat_id, transfer_id))
        .await?
        .ok_or(TransferError::NotFound)?;
    if status.cancelled {
        return Err(TransferError::Cancelled);
    }
    let manifest = status.manifest.ok_or(TransferError::NotFound)?;
    if !status.done {
        return Err(TransferError::NotReady {
            present: status.present.len(),
            total: manifest.parts.len(),
            last_part_at: status.last_part_at,
        });
    }

    tokio::fs::create_dir_all(work).await?;
    let partial = work.join(PARTIAL);
    let mut state = load_state(work, transfer_id, manifest.size).await;
    let sized = tokio::fs::metadata(&partial)
        .await
        .is_ok_and(|m| m.len() == manifest.size);
    if !sized {
        // Sparse where the filesystem allows: parts land at their offsets.
        let file = tokio::fs::File::create(&partial).await?;
        file.set_len(manifest.size).await?;
        state.done.clear();
    }
    let received = Arc::new(AtomicU64::new(
        state.done.iter().map(|&n| manifest.parts[n].size).sum(),
    ));
    let report = |bytes: u64| {
        if let Some(progress) = &progress {
            progress(bytes, manifest.size);
        }
    };
    report(received.load(Ordering::Relaxed));

    let missing: Vec<usize> = (0..manifest.parts.len())
        .filter(|n| !state.done.contains(n))
        .collect();
    let state = tokio::sync::Mutex::new(state);
    futures::stream::iter(missing.into_iter().map(Ok::<_, TransferError>))
        .try_for_each_concurrent(PARALLEL_PARTS, |n| {
            let (manifest, partial, state, received, report) =
                (&manifest, &partial, &state, &received, &report);
            async move {
                with_retries(|| fetch_part(client, chat_id, transfer_id, manifest, n, partial))
                    .await?;
                let mut state = state.lock().await;
                state.done.insert(n);
                save_state(work, &state).await?;
                report(
                    received.fetch_add(manifest.parts[n].size, Ordering::Relaxed)
                        + manifest.parts[n].size,
                );
                Ok(())
            }
        })
        .await?;

    tokio::fs::OpenOptions::new()
        .write(true)
        .open(&partial)
        .await?
        .sync_all()
        .await?;
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Atomic: whoever waits on `dest` never sees a half-written file.
    tokio::fs::rename(&partial, dest).await?;
    let _ = tokio::fs::remove_dir_all(work).await;
    Ok(manifest)
}

/// A per-transfer work directory under `root`.
pub fn work_dir(root: &Path, transfer_id: &str) -> PathBuf {
    root.join(format!("xfer-{transfer_id}"))
}
