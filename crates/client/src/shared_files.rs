//! Files agents send to the chat (`MessagePart::File`): a streamable URL for
//! URL-only openers (video, audio), and a local copy for everything else.
//!
//! The bytes come from the transfer's R2 copy when there is one, resumable;
//! from the host (`read_attachment`) when there isn't — an older host, an
//! edge without `/xfer`, a transfer that expired.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use zeron_transfer::{EdgeClient, TransferError};

use crate::client::{Client, ClientInner};
use crate::error::{ClientError, Result};

/// Read URLs are refreshed this long before their token expires.
const URL_MARGIN_MS: i64 = 60 * 60 * 1000;

/// Per-client cache of read URLs and fetch progress.
#[derive(Default)]
pub(crate) struct SharedFiles {
    urls: Mutex<HashMap<String, (String, i64)>>,
    progress: Arc<Mutex<HashMap<String, (u64, u64)>>>,
}

/// Where a shared file's bytes can be fetched from.
#[derive(Debug, Clone)]
pub struct SharedFileRef {
    pub device_id: String,
    pub chat_id: String,
    pub transfer_id: Option<String>,
    /// Host path, for the host fallback.
    pub path: String,
    pub name: String,
}

struct TransferBearer(std::sync::Weak<ClientInner>);

#[async_trait::async_trait]
impl zeron_transfer::Bearer for TransferBearer {
    async fn bearer(&self) -> std::result::Result<String, TransferError> {
        let inner = self.0.upgrade().ok_or(TransferError::Unauthorized)?;
        inner.tokens.bearer().await.map_err(|e| match e {
            ClientError::Auth(_) => TransferError::Unauthorized,
            other => TransferError::Unavailable {
                status: 0,
                message: other.to_string(),
            },
        })
    }
}

fn transfer_error(e: TransferError) -> ClientError {
    match e {
        TransferError::Unauthorized => ClientError::Auth(e.to_string()),
        TransferError::NotFound => ClientError::NotFound(e.to_string()),
        TransferError::Network(_) | TransferError::Unavailable { .. } => {
            ClientError::Network(e.to_string())
        }
        other => ClientError::HostError(other.to_string()),
    }
}

/// Worth falling back to the host: the edge doesn't have this transfer.
fn edge_lacks(e: &TransferError) -> bool {
    matches!(
        e,
        TransferError::NotFound
            | TransferError::Cancelled
            | TransferError::Refused(_)
            | TransferError::Unavailable { status: 503, .. }
    )
}

/// A file name safe to put on disk (the agent chose it).
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || c == ':' { '_' } else { c })
        .collect();
    match cleaned.as_str() {
        "" | "." | ".." => "file".into(),
        _ => cleaned,
    }
}

fn cache_key(file: &SharedFileRef) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(file.device_id.as_bytes());
    h.update([0]);
    h.update(file.transfer_id.as_deref().unwrap_or(&file.path).as_bytes());
    zeron_transfer::hex(&h.finalize()[..16])
}

/// Parts are 8 MiB: no whole-request timeout (a slow link would never finish
/// one), only stalls.
fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client")
    })
}

impl Client {
    fn transfer_client(&self) -> Option<EdgeClient> {
        let live = self.inner.live()?;
        Some(EdgeClient::new(
            http().clone(),
            live.edge.clone(),
            Arc::new(TransferBearer(Arc::downgrade(&self.inner))),
        ))
    }

    /// A URL a player (AVPlayer, a web view) can stream with byte ranges,
    /// valid for hours. `None` when the edge has no copy (open the local file
    /// from [`Client::fetch_shared_file`] instead).
    pub async fn shared_file_url(
        &self,
        chat_id: &str,
        transfer_id: &str,
    ) -> Result<Option<String>> {
        let key = format!("{chat_id}/{transfer_id}");
        let now = chrono::Utc::now().timestamp_millis();
        if let Some((url, expires)) = self.inner.shared_files.urls.lock().unwrap().get(&key)
            && *expires - URL_MARGIN_MS > now
        {
            return Ok(Some(url.clone()));
        }
        let Some(client) = self.transfer_client() else {
            return Ok(None);
        };
        match client.tokens(chat_id, transfer_id).await {
            Ok(tokens) => {
                let url = client
                    .content_url(chat_id, transfer_id, &tokens.read_token)
                    .map_err(transfer_error)?;
                self.inner
                    .shared_files
                    .urls
                    .lock()
                    .unwrap()
                    .insert(key, (url.clone(), tokens.expires_at));
                Ok(Some(url))
            }
            Err(e) if edge_lacks(&e) => Ok(None),
            Err(e) => Err(transfer_error(e)),
        }
    }

    /// The file on this device, fetched once (resumable) and kept in the
    /// cache dir. Returns its local path.
    pub async fn fetch_shared_file(&self, file: SharedFileRef) -> Result<String> {
        let dir = self.inner.config.data_dir.join("shared-files");
        let key = cache_key(&file);
        let dest = dir.join(&key).join(safe_name(&file.name));
        if tokio::fs::metadata(&dest).await.is_ok_and(|m| m.is_file()) {
            return Ok(dest.to_string_lossy().into_owned());
        }
        if let (Some(client), Some(transfer_id)) = (self.transfer_client(), &file.transfer_id) {
            let progress = self.inner.shared_files.progress.clone();
            let progress_key = key.clone();
            let report: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(move |got, total| {
                progress
                    .lock()
                    .unwrap()
                    .insert(progress_key.clone(), (got, total));
            });
            let work = zeron_transfer::work_dir(&dir.join("partial"), transfer_id);
            let fetched = zeron_transfer::download(
                &client,
                &file.chat_id,
                transfer_id,
                &dest,
                &work,
                Some(report),
            )
            .await;
            self.inner
                .shared_files
                .progress
                .lock()
                .unwrap()
                .remove(&key);
            match fetched {
                Ok(_) => return Ok(dest.to_string_lossy().into_owned()),
                // Still uploading from the host: say so rather than fall back
                // to a slow relay read of the same bytes.
                Err(e @ TransferError::NotReady { .. }) => {
                    return Err(ClientError::HostUnavailable(e.to_string()));
                }
                Err(e) if edge_lacks(&e) => {}
                Err(e) => return Err(transfer_error(e)),
            }
        }
        let bytes = self.read_attachment(&file.device_id, &file.path).await?;
        write_atomic(&dest, &bytes).await?;
        // The host read is cached in memory too; this copy is the one kept.
        self.inner
            .attachment_cache
            .remove(&file.device_id, &file.path);
        Ok(dest.to_string_lossy().into_owned())
    }

    /// Bytes fetched so far and the total, while [`Client::fetch_shared_file`]
    /// downloads through the edge.
    pub fn shared_file_progress(&self, file: &SharedFileRef) -> Option<(u64, u64)> {
        let key = cache_key(file);
        self.inner
            .shared_files
            .progress
            .lock()
            .unwrap()
            .get(&key)
            .copied()
    }
}

async fn write_atomic(dest: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let storage = |e: std::io::Error| ClientError::Storage(e.to_string());
    let parent = dest.parent().map(PathBuf::from).unwrap_or_default();
    tokio::fs::create_dir_all(&parent).await.map_err(storage)?;
    let tmp = parent.join(".partial");
    tokio::fs::write(&tmp, bytes).await.map_err(storage)?;
    tokio::fs::rename(&tmp, dest).await.map_err(storage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_inside_their_folder() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name("a\\b.txt"), "b.txt");
        assert_eq!(safe_name("demo.mp4"), "demo.mp4");
    }
}
