//! Authenticated, local-first Studio snapshot sync.
//!
//! This is deliberately a snapshot protocol, not a fake row-level SQLite
//! merge. A publish uploads immutable, content-addressed bytes and commits one
//! generation with compare-and-swap. A pull stages and hashes every byte before
//! replacing the local catalog. The caller gets a conflict when another device
//! won, never an automatic overwrite.

use std::{collections::HashMap, path::{Path, PathBuf}, sync::Arc};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{doc_host::EdgeConfig, studio::StudioStore, StudioStoreError};

const MANIFEST_VERSION: u8 = 1;
const MAX_OBJECT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StudioSyncError {
    #[error("Studio sync requires a signed-in WorkOS session")]
    SignedOut,
    #[error("Studio sync request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Studio sync I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Studio sync JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Studio sync storage failed: {0}")]
    Store(#[from] StudioStoreError),
    #[error("Studio sync protocol error: {0}")]
    Protocol(String),
    #[error("Studio changed on another device at generation {generation}; pull before publishing")]
    Conflict { generation: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StudioSyncOutcome {
    NoRemoteSnapshot,
    UpToDate { generation: u64 },
    Pulled { generation: u64, files: usize },
    Published { generation: u64, files: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectRef {
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileRef {
    path: String,
    sha256: String,
    size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    version: u8,
    database: ObjectRef,
    files: Vec<FileRef>,
    published_at: String,
    publisher_device_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestEnvelope {
    generation: u64,
    manifest: Option<Manifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalState {
    generation: u64,
    manifest: Manifest,
}

/// A reusable client. The Engine creates it only for an authenticated profile.
/// It does no work until a caller asks it to pull or publish, which keeps boot
/// and gallery rendering local and predictable.
#[derive(Clone)]
pub struct StudioSync {
    store: Arc<StudioStore>,
    edge: EdgeConfig,
    org_id: String,
    device_id: String,
    http: reqwest::Client,
}

impl StudioSync {
    pub fn new(
        store: Arc<StudioStore>,
        edge: EdgeConfig,
        org_id: impl Into<String>,
        device_id: impl Into<String>,
    ) -> Self {
        Self { store, edge, org_id: org_id.into(), device_id: device_id.into(), http: reqwest::Client::new() }
    }

    /// Download a newer remote snapshot into a private same-volume staging
    /// directory. No live file changes occur until all hashes verify.
    pub async fn pull(&self) -> Result<StudioSyncOutcome, StudioSyncError> {
        let envelope = self.fetch_manifest().await?;
        let Some(manifest) = envelope.manifest else {
            return Ok(StudioSyncOutcome::NoRemoteSnapshot);
        };
        validate_manifest(&manifest)?;
        let state = self.read_state().await?;
        if state.as_ref().is_some_and(|state| state.generation == envelope.generation) {
            return Ok(StudioSyncOutcome::UpToDate { generation: envelope.generation });
        }
        // A remote generation changed. A device with local state needs a hash
        // comparison against the last installed manifest before it can pull;
        // this is the guard that turns a stale two-device race into a conflict.
        if let Some(state) = &state {
            if !self.local_matches(&state.manifest).await? {
                return Err(StudioSyncError::Conflict { generation: envelope.generation });
            }
        } else if self.store.has_sync_content()? {
            return Err(StudioSyncError::Conflict { generation: envelope.generation });
        }
        let stage = self.stage_path("download");
        tokio::fs::create_dir_all(&stage).await?;
        let result = async {
            let database = stage.join("studio.sqlite3");
            let mut cached = HashMap::<String, PathBuf>::new();
            self.download_once(&manifest.database, &database, &mut cached).await?;
            for file in &manifest.files {
                let destination = stage.join(&file.path);
                if let Some(parent) = destination.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                self.download_once(
                    &ObjectRef { sha256: file.sha256.clone(), size_bytes: file.size_bytes },
                    &destination,
                    &mut cached,
                ).await?;
            }
            let store = self.store.clone();
            let install = stage.clone();
            tokio::task::spawn_blocking(move || store.install_sync_snapshot(&install))
                .await
                .map_err(|error| StudioSyncError::Protocol(format!("sync installer panicked: {error}")))??;
            self.write_state(&LocalState { generation: envelope.generation, manifest: manifest.clone() }).await?;
            Ok(StudioSyncOutcome::Pulled { generation: envelope.generation, files: manifest.files.len() })
        }
        .await;
        // install moves the media children out, but the now-empty stage (or a
        // failed partial download) is always disposable.
        let _ = tokio::fs::remove_dir_all(&stage).await;
        result
    }

    /// Snapshot local Studio state, upload only missing immutable objects, then
    /// commit a manifest with the generation observed immediately before it.
    pub async fn publish(&self) -> Result<StudioSyncOutcome, StudioSyncError> {
        let current = self.fetch_manifest().await?;
        let state = self.read_state().await?;
        let may_publish = match state {
            Some(state) => state.generation == current.generation,
            None => current.manifest.is_none() && current.generation == 0,
        };
        if !may_publish {
            return Err(StudioSyncError::Conflict { generation: current.generation });
        }
        let stage = self.stage_path("publish");
        let store = self.store.clone();
        let export = stage.clone();
        tokio::task::spawn_blocking(move || store.export_sync_snapshot(&export))
            .await
            .map_err(|error| StudioSyncError::Protocol(format!("sync exporter panicked: {error}")))??;
        let result = async {
            let device_id = self.device_id.clone();
            let manifest_stage = stage.clone();
            let manifest = tokio::task::spawn_blocking(move || build_manifest(&manifest_stage, &device_id))
                .await
                .map_err(|error| StudioSyncError::Protocol(format!("manifest builder panicked: {error}")))??;
            self.upload_if_missing(&manifest.database, &stage.join("studio.sqlite3")).await?;
            for file in &manifest.files {
                self.upload_if_missing(
                    &ObjectRef { sha256: file.sha256.clone(), size_bytes: file.size_bytes },
                    &stage.join(&file.path),
                ).await?;
            }
            let generation = self.commit_manifest(current.generation, &manifest).await?;
            self.write_state(&LocalState { generation, manifest: manifest.clone() }).await?;
            Ok(StudioSyncOutcome::Published { generation, files: manifest.files.len() })
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&stage).await;
        result
    }

    fn manifest_url(&self) -> String {
        format!("{}/studio/{}/manifest", self.edge.url.trim_end_matches('/'), self.org_id)
    }

    fn object_url(&self, sha256: &str) -> String {
        format!("{}/studio/{}/objects/{sha256}", self.edge.url.trim_end_matches('/'), self.org_id)
    }

    fn stage_path(&self, purpose: &str) -> PathBuf {
        self.store.sync_root().join(format!(".sync-{purpose}-{}", Uuid::new_v4()))
    }

    fn state_path(&self) -> PathBuf {
        self.store.sync_root().join(".studio-sync-state.json")
    }

    async fn read_state(&self) -> Result<Option<LocalState>, StudioSyncError> {
        match tokio::fs::read(self.state_path()).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(Into::into),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn write_state(&self, state: &LocalState) -> Result<(), StudioSyncError> {
        let path = self.state_path();
        let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
        let bytes = serde_json::to_vec(state)?;
        let mut file = tokio::fs::File::create(&temporary).await?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(temporary, path).await?;
        Ok(())
    }

    async fn local_matches(&self, expected: &Manifest) -> Result<bool, StudioSyncError> {
        let stage = self.stage_path("compare");
        let store = self.store.clone();
        let export = stage.clone();
        tokio::task::spawn_blocking(move || store.export_sync_snapshot(&export))
            .await
            .map_err(|error| StudioSyncError::Protocol(format!("sync comparison exporter panicked: {error}")))??;
        let stage_for_manifest = stage.clone();
        let device_id = self.device_id.clone();
        let actual = tokio::task::spawn_blocking(move || build_manifest(&stage_for_manifest, &device_id))
            .await
            .map_err(|error| StudioSyncError::Protocol(format!("sync comparison manifest builder panicked: {error}")))??;
        let _ = tokio::fs::remove_dir_all(stage).await;
        Ok(same_content(&actual, expected))
    }

    async fn bearer(&self) -> Result<String, StudioSyncError> {
        self.edge.bearer().await.ok_or(StudioSyncError::SignedOut)
    }

    async fn fetch_manifest(&self) -> Result<ManifestEnvelope, StudioSyncError> {
        let response = self.http.get(self.manifest_url()).bearer_auth(self.bearer().await?).send().await?;
        if !response.status().is_success() {
            return Err(StudioSyncError::Protocol(format!("manifest GET returned HTTP {}", response.status())));
        }
        Ok(response.json().await?)
    }

    async fn download_once(
        &self,
        object: &ObjectRef,
        destination: &Path,
        cached: &mut HashMap<String, PathBuf>,
    ) -> Result<(), StudioSyncError> {
        if let Some(existing) = cached.get(&object.sha256) {
            tokio::fs::hard_link(existing, destination).await?;
            return Ok(());
        }
        let response = self
            .http
            .get(self.object_url(&object.sha256))
            .bearer_auth(self.bearer().await?)
            .send()
            .await?;
        if response.status() != StatusCode::OK {
            return Err(StudioSyncError::Protocol(format!("object GET returned HTTP {}", response.status())));
        }
        if response.headers().get("x-studio-sha256").and_then(|value| value.to_str().ok()) != Some(object.sha256.as_str()) {
            return Err(StudioSyncError::Protocol("object response omitted or changed its SHA-256".into()));
        }
        let mut output = tokio::fs::File::create(destination).await?;
        let mut digest = Sha256::new();
        let mut bytes = 0_u64;
        let mut response = response;
        while let Some(chunk) = response.chunk().await? {
            bytes = bytes.checked_add(chunk.len() as u64).ok_or_else(|| StudioSyncError::Protocol("object size overflow".into()))?;
            if bytes > object.size_bytes || bytes > MAX_OBJECT_BYTES {
                return Err(StudioSyncError::Protocol("object body exceeds its manifest size".into()));
            }
            digest.update(&chunk);
            output.write_all(&chunk).await?;
        }
        output.sync_all().await?;
        if bytes != object.size_bytes || hex_digest(digest.finalize()) != object.sha256 {
            return Err(StudioSyncError::Protocol("downloaded object SHA-256 mismatch".into()));
        }
        cached.insert(object.sha256.clone(), destination.to_path_buf());
        Ok(())
    }

    async fn upload_if_missing(&self, object: &ObjectRef, path: &Path) -> Result<(), StudioSyncError> {
        let url = self.object_url(&object.sha256);
        let head = self.http.head(&url).bearer_auth(self.bearer().await?).send().await?;
        if head.status() == StatusCode::OK {
            let bytes = head.headers().get(reqwest::header::CONTENT_LENGTH).and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u64>().ok());
            let hash = head.headers().get("x-studio-sha256").and_then(|value| value.to_str().ok());
            if bytes == Some(object.size_bytes) && hash == Some(object.sha256.as_str()) {
                return Ok(());
            }
            return Err(StudioSyncError::Protocol("existing remote object does not match its content address".into()));
        }
        if head.status() != StatusCode::NOT_FOUND {
            return Err(StudioSyncError::Protocol(format!("object HEAD returned HTTP {}", head.status())));
        }
        let file = tokio::fs::File::open(path).await?;
        let response = self
            .http
            .put(url)
            .bearer_auth(self.bearer().await?)
            .header("x-studio-sha256", &object.sha256)
            .header(reqwest::header::CONTENT_LENGTH, object.size_bytes)
            .header(reqwest::header::CONTENT_TYPE, mime_for_path(path))
            .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
            .send()
            .await?;
        if response.status() != StatusCode::CREATED && response.status() != StatusCode::OK {
            return Err(StudioSyncError::Protocol(format!("object PUT returned HTTP {}", response.status())));
        }
        Ok(())
    }

    async fn commit_manifest(&self, generation: u64, manifest: &Manifest) -> Result<u64, StudioSyncError> {
        let response = self
            .http
            .put(self.manifest_url())
            .bearer_auth(self.bearer().await?)
            .header(reqwest::header::IF_MATCH, format!("\"{generation}\""))
            .json(manifest)
            .send()
            .await?;
        if response.status() == StatusCode::CONFLICT {
            let generation = response
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|value| value.get("generation")?.as_u64())
                .unwrap_or(generation);
            return Err(StudioSyncError::Conflict { generation });
        }
        if !response.status().is_success() {
            return Err(StudioSyncError::Protocol(format!("manifest PUT returned HTTP {}", response.status())));
        }
        response
            .json::<ManifestEnvelope>()
            .await
            .map(|envelope| envelope.generation)
            .map_err(Into::into)
    }
}

fn build_manifest(stage: &Path, device_id: &str) -> Result<Manifest, StudioSyncError> {
    let database = object_ref(&stage.join("studio.sqlite3"))?;
    let mut files = Vec::new();
    for directory in ["artifacts", "previews", "inputs"] {
        let root = stage.join(directory);
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| StudioSyncError::Protocol("non-UTF-8 Studio media name".into()))?;
            if !valid_media_name(name) {
                return Err(StudioSyncError::Protocol("invalid Studio media filename".into()));
            }
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(StudioSyncError::Protocol("Studio media entry is not a regular file".into()));
            }
            let object = object_ref(&entry.path())?;
            files.push(FileRef {
                path: format!("{directory}/{name}"),
                sha256: object.sha256,
                size_bytes: object.size_bytes,
                mime_type: Some(mime_for_path(&entry.path()).to_string()),
            });
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Manifest {
        version: MANIFEST_VERSION,
        database,
        files,
        published_at: chrono::Utc::now().to_rfc3339(),
        publisher_device_id: device_id.to_string(),
    })
}

fn object_ref(path: &Path) -> Result<ObjectRef, StudioSyncError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_OBJECT_BYTES {
        return Err(StudioSyncError::Protocol("Studio object is not a permitted regular file".into()));
    }
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)?;
    Ok(ObjectRef { sha256: hex_digest(digest.finalize()), size_bytes: metadata.len() })
}

fn validate_manifest(manifest: &Manifest) -> Result<(), StudioSyncError> {
    if manifest.version != MANIFEST_VERSION || !valid_object(&manifest.database) || manifest.publisher_device_id.is_empty() || manifest.publisher_device_id.len() > 128 {
        return Err(StudioSyncError::Protocol("invalid Studio manifest".into()));
    }
    let mut paths = std::collections::HashSet::new();
    for file in &manifest.files {
        if !valid_object(&ObjectRef { sha256: file.sha256.clone(), size_bytes: file.size_bytes }) || !valid_remote_path(&file.path) || !paths.insert(&file.path) {
            return Err(StudioSyncError::Protocol("invalid Studio manifest file entry".into()));
        }
    }
    Ok(())
}

fn valid_object(object: &ObjectRef) -> bool {
    object.size_bytes <= MAX_OBJECT_BYTES
        && object.sha256.len() == 64
        && object.sha256.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn same_content(left: &Manifest, right: &Manifest) -> bool {
    left.database.sha256 == right.database.sha256
        && left.database.size_bytes == right.database.size_bytes
        && left.files.len() == right.files.len()
        && left.files.iter().zip(&right.files).all(|(left, right)| {
            left.path == right.path
                && left.sha256 == right.sha256
                && left.size_bytes == right.size_bytes
                && left.mime_type == right.mime_type
        })
}

fn valid_remote_path(path: &str) -> bool {
    let Some((directory, name)) = path.split_once('/') else { return false; };
    matches!(directory, "artifacts" | "previews" | "inputs") && !name.contains('/') && valid_media_name(name)
}

fn valid_media_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric())
        && name.len() <= 255
        && chars.all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
}

fn mime_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "webp" => "image/webp",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
