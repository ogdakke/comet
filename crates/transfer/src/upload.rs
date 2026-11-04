//! Sending a file: create, the missing parts, seal.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::TryStreamExt as _;

use crate::plan::part_path;
use crate::{EdgeClient, Manifest, PARALLEL_PARTS, TransferError};

/// Where part `n`'s bytes are on disk.
#[derive(Debug, Clone)]
pub enum PartSource {
    /// One file; part `n` starts at `n * part_size`.
    File(PathBuf),
    /// A [`crate::split_into_parts`] directory: part `n` is `{dir}/{n}.part`.
    Split(PathBuf),
}

impl PartSource {
    fn locate(&self, manifest: &Manifest, n: usize) -> (PathBuf, u64) {
        match self {
            Self::File(path) => (path.clone(), manifest.offset(n)),
            Self::Split(dir) => (part_path(dir, n), 0),
        }
    }
}

pub struct Upload<'a> {
    pub chat_id: &'a str,
    pub transfer_id: &'a str,
    pub manifest: &'a Manifest,
    pub source: &'a PartSource,
    /// Bytes sent so far (including parts already on the edge) and the total.
    pub progress: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
}

/// Retry a transient failure a few times with backoff before giving up; the
/// caller's own (much longer) retry loop takes it from there.
pub(crate) async fn with_retries<T, F, Fut>(mut attempt: F) -> Result<T, TransferError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, TransferError>>,
{
    let mut delay = Duration::from_millis(250);
    for _ in 0..3 {
        match attempt().await {
            Err(e) if e.is_transient() => {
                tracing::debug!(error = %e, "transfer request failed; retrying");
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            other => return other,
        }
    }
    attempt().await
}

/// Send the file, skipping parts the edge already holds, and seal it.
/// Idempotent: after any failure, run it again to resume.
pub async fn upload(client: &EdgeClient, job: Upload<'_>) -> Result<(), TransferError> {
    let Upload {
        chat_id,
        transfer_id,
        manifest,
        source,
        progress,
    } = job;
    let created = with_retries(|| client.create(chat_id, transfer_id, manifest)).await?;
    let report = |sent: u64| {
        if let Some(progress) = &progress {
            progress(sent, manifest.size);
        }
    };
    let have: std::collections::HashSet<usize> = created.present.iter().copied().collect();
    let sent = Arc::new(AtomicU64::new(
        have.iter().map(|&n| manifest.parts[n].size).sum(),
    ));
    report(sent.load(Ordering::Relaxed));
    if !created.done {
        let missing = (0..manifest.parts.len()).filter(|n| !have.contains(n));
        futures::stream::iter(missing.map(Ok::<_, TransferError>))
            .try_for_each_concurrent(PARALLEL_PARTS, |n| {
                let (path, offset) = source.locate(manifest, n);
                let len = manifest.parts[n].size;
                let sent = sent.clone();
                let report = &report;
                async move {
                    with_retries(|| client.put_part(chat_id, transfer_id, n, &path, offset, len))
                        .await?;
                    report(sent.fetch_add(len, Ordering::Relaxed) + len);
                    Ok(())
                }
            })
            .await?;
        with_retries(|| client.done(chat_id, transfer_id)).await?;
    }
    Ok(())
}
