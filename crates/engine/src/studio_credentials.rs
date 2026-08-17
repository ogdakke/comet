//! Device-scoped Studio provider credentials.
//!
//! Only connection metadata is written below the device root. Secret values are delegated to the
//! platform credential store and never serialized into a profile or an RPC response.

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use uuid::Uuid;
use zeron_proto::{ProviderValidationState, StudioProviderConnection};
use zeron_studio::{ProviderId, Secret};

const SERVICE: &str = "sh.zeron.studio";
const METADATA_FILE: &str = "connections.json";

#[derive(Debug, thiserror::Error)]
pub enum StudioCredentialError {
    #[error("provider credential metadata: {0}")]
    Io(#[from] std::io::Error),
    #[error("provider credential metadata is invalid: {0}")]
    InvalidMetadata(#[from] serde_json::Error),
    #[error("platform secret store failed: {0}")]
    SecretStore(String),
    #[error("studio credential lock is poisoned")]
    LockPoisoned,
    #[error("studio provider credential is not configured")]
    NotConfigured,
}

#[async_trait]
pub trait StudioSecretBackend: Send + Sync {
    async fn set(
        &self,
        provider_id: &ProviderId,
        secret: &Secret,
    ) -> Result<(), StudioCredentialError>;
    async fn get(&self, provider_id: &ProviderId) -> Result<Secret, StudioCredentialError>;
    async fn remove(&self, provider_id: &ProviderId) -> Result<(), StudioCredentialError>;
}

#[derive(Default)]
pub struct SystemStudioSecretBackend;

#[async_trait]
impl StudioSecretBackend for SystemStudioSecretBackend {
    async fn set(
        &self,
        provider_id: &ProviderId,
        secret: &Secret,
    ) -> Result<(), StudioCredentialError> {
        #[cfg(target_os = "macos")]
        let status = Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                SERVICE,
                "-a",
                provider_id.as_str(),
                "-w",
                secret.expose(),
            ])
            .status()
            .await?;

        #[cfg(not(target_os = "macos"))]
        let status = {
            let mut child = Command::new("secret-tool")
                .args([
                    "store",
                    "--label",
                    "Zeron Studio provider credential",
                    "service",
                    SERVICE,
                    "provider",
                    provider_id.as_str(),
                ])
                .stdin(std::process::Stdio::piped())
                .spawn()?;
            child
                .stdin
                .take()
                .ok_or_else(|| {
                    StudioCredentialError::SecretStore("secret-tool stdin unavailable".into())
                })?
                .write_all(secret.expose().as_bytes())
                .await?;
            child.wait().await?
        };

        status.success().then_some(()).ok_or_else(|| {
            StudioCredentialError::SecretStore("could not save the provider credential".into())
        })
    }

    async fn get(&self, provider_id: &ProviderId) -> Result<Secret, StudioCredentialError> {
        #[cfg(target_os = "macos")]
        let output = Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                SERVICE,
                "-a",
                provider_id.as_str(),
                "-w",
            ])
            .output()
            .await?;

        #[cfg(not(target_os = "macos"))]
        let output = Command::new("secret-tool")
            .args([
                "lookup",
                "service",
                SERVICE,
                "provider",
                provider_id.as_str(),
            ])
            .output()
            .await?;

        if !output.status.success() {
            return Err(StudioCredentialError::NotConfigured);
        }
        let value = String::from_utf8(output.stdout).map_err(|_| {
            StudioCredentialError::SecretStore("secret store returned invalid text".into())
        })?;
        let value = value.trim_end_matches(['\r', '\n']);
        if value.is_empty() {
            return Err(StudioCredentialError::NotConfigured);
        }
        Ok(Secret::new(value))
    }

    async fn remove(&self, provider_id: &ProviderId) -> Result<(), StudioCredentialError> {
        #[cfg(target_os = "macos")]
        let status = Command::new("security")
            .args([
                "delete-generic-password",
                "-s",
                SERVICE,
                "-a",
                provider_id.as_str(),
            ])
            .status()
            .await?;

        #[cfg(not(target_os = "macos"))]
        let status = Command::new("secret-tool")
            .args([
                "clear",
                "service",
                SERVICE,
                "provider",
                provider_id.as_str(),
            ])
            .status()
            .await?;

        // Deleting an already-missing secret is intentionally idempotent. Both platform tools use
        // a non-zero status for that case, so metadata remains the source of configured state.
        let _ = status;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionMetadata {
    display_label: String,
    validation_state: ProviderValidationState,
    validated_at: Option<DateTime<Utc>>,
    validation_message: Option<String>,
    #[serde(default)]
    safe_mode: bool,
}

pub struct StudioCredentials {
    metadata_path: PathBuf,
    connections: Mutex<BTreeMap<ProviderId, ConnectionMetadata>>,
    backend: Arc<dyn StudioSecretBackend>,
}

impl StudioCredentials {
    pub fn open(device_root: &Path) -> Result<Self, StudioCredentialError> {
        Self::with_backend(device_root, Arc::new(SystemStudioSecretBackend))
    }

    pub fn with_backend(
        device_root: &Path,
        backend: Arc<dyn StudioSecretBackend>,
    ) -> Result<Self, StudioCredentialError> {
        let root = device_root.join("provider-accounts");
        std::fs::create_dir_all(&root)?;
        let metadata_path = root.join(METADATA_FILE);
        let connections = match std::fs::read(&metadata_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            metadata_path,
            connections: Mutex::new(connections),
            backend,
        })
    }

    pub fn connection(
        &self,
        provider_id: &ProviderId,
    ) -> Result<Option<StudioProviderConnection>, StudioCredentialError> {
        Ok(self
            .connections
            .lock()
            .map_err(|_| StudioCredentialError::LockPoisoned)?
            .get(provider_id)
            .map(|metadata| as_connection(provider_id.clone(), metadata)))
    }

    pub fn list(&self) -> Result<Vec<StudioProviderConnection>, StudioCredentialError> {
        Ok(self
            .connections
            .lock()
            .map_err(|_| StudioCredentialError::LockPoisoned)?
            .iter()
            .map(|(id, metadata)| as_connection(id.clone(), metadata))
            .collect())
    }

    pub async fn set(
        &self,
        provider_id: ProviderId,
        display_label: String,
        secret: Secret,
    ) -> Result<StudioProviderConnection, StudioCredentialError> {
        self.backend.set(&provider_id, &secret).await?;
        let existing = self
            .connections
            .lock()
            .map_err(|_| StudioCredentialError::LockPoisoned)?
            .get(&provider_id)
            .cloned();
        let metadata = ConnectionMetadata {
            display_label,
            validation_state: ProviderValidationState::NotValidated,
            validated_at: None,
            validation_message: None,
            safe_mode: existing.map(|metadata| metadata.safe_mode).unwrap_or(false),
        };
        self.update(provider_id, Some(metadata))
    }

    pub fn set_preferences(
        &self,
        provider_id: ProviderId,
        safe_mode: bool,
    ) -> Result<StudioProviderConnection, StudioCredentialError> {
        let mut connections = self
            .connections
            .lock()
            .map_err(|_| StudioCredentialError::LockPoisoned)?;
        let metadata = connections
            .get_mut(&provider_id)
            .ok_or(StudioCredentialError::NotConfigured)?;
        metadata.safe_mode = safe_mode;
        let result = as_connection(provider_id, metadata);
        persist(&self.metadata_path, &connections)?;
        Ok(result)
    }

    pub async fn secret(&self, provider_id: &ProviderId) -> Result<Secret, StudioCredentialError> {
        if self.connection(provider_id)?.is_none() {
            return Err(StudioCredentialError::NotConfigured);
        }
        self.backend.get(provider_id).await
    }

    pub fn record_validation(
        &self,
        provider_id: ProviderId,
        state: ProviderValidationState,
        message: Option<String>,
    ) -> Result<StudioProviderConnection, StudioCredentialError> {
        let mut connections = self
            .connections
            .lock()
            .map_err(|_| StudioCredentialError::LockPoisoned)?;
        let metadata = connections
            .get_mut(&provider_id)
            .ok_or(StudioCredentialError::NotConfigured)?;
        metadata.validation_state = state;
        metadata.validated_at = Some(Utc::now());
        metadata.validation_message = message;
        let result = as_connection(provider_id, metadata);
        persist(&self.metadata_path, &connections)?;
        Ok(result)
    }

    pub async fn remove(&self, provider_id: &ProviderId) -> Result<(), StudioCredentialError> {
        self.backend.remove(provider_id).await?;
        let mut connections = self
            .connections
            .lock()
            .map_err(|_| StudioCredentialError::LockPoisoned)?;
        connections.remove(provider_id);
        persist(&self.metadata_path, &connections)?;
        Ok(())
    }

    fn update(
        &self,
        provider_id: ProviderId,
        metadata: Option<ConnectionMetadata>,
    ) -> Result<StudioProviderConnection, StudioCredentialError> {
        let mut connections = self
            .connections
            .lock()
            .map_err(|_| StudioCredentialError::LockPoisoned)?;
        let metadata = metadata.ok_or(StudioCredentialError::NotConfigured)?;
        connections.insert(provider_id.clone(), metadata);
        persist(&self.metadata_path, &connections)?;
        Ok(as_connection(
            provider_id.clone(),
            connections.get(&provider_id).expect("just inserted"),
        ))
    }
}

fn as_connection(
    provider_id: ProviderId,
    metadata: &ConnectionMetadata,
) -> StudioProviderConnection {
    StudioProviderConnection {
        provider_id,
        display_label: metadata.display_label.clone(),
        configured: true,
        validation_state: metadata.validation_state,
        validated_at: metadata.validated_at,
        validation_message: metadata.validation_message.clone(),
        safe_mode: metadata.safe_mode,
    }
}

fn persist(
    path: &Path,
    connections: &BTreeMap<ProviderId, ConnectionMetadata>,
) -> Result<(), StudioCredentialError> {
    let bytes = serde_json::to_vec_pretty(connections)?;
    let parent = path.parent().ok_or_else(|| {
        StudioCredentialError::SecretStore("credential metadata has no parent directory".into())
    })?;
    let temporary = parent.join(format!(
        ".{METADATA_FILE}.tmp-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
