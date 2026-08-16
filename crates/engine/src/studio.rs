//! Profile-scoped durable storage for Studio metadata and generated media.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, RwLock},
};

use rusqlite::Connection;
use uuid::Uuid;
use zeron_proto::StudioConversationSummary;
use zeron_studio::{MediaProvider, ProviderId, StudioArtifactId, SubmissionCapabilities};
use zeron_studio::{StudioConversationId, StudioTurnId};

const DATABASE_FILE: &str = "studio.sqlite3";
const SCHEMA_VERSION: i64 = 1;
pub(crate) const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

const SCHEMA_V1: &str = r#"
BEGIN IMMEDIATE;

CREATE TABLE IF NOT EXISTS studio_conversations (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    archived_at INTEGER,
    forked_from_turn_id TEXT REFERENCES studio_turns(id)
) STRICT;

CREATE TABLE IF NOT EXISTS studio_turns (
    id TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES studio_conversations(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    prompt TEXT NOT NULL,
    source_turn_id TEXT REFERENCES studio_turns(id),
    created_at INTEGER NOT NULL,
    UNIQUE (conversation_id, position)
) STRICT;

CREATE TABLE IF NOT EXISTS studio_batches (
    id TEXT PRIMARY KEY NOT NULL,
    turn_id TEXT NOT NULL UNIQUE REFERENCES studio_turns(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (state IN (
        'draft', 'quoting', 'awaiting_confirmation', 'queued', 'running', 'downloading',
        'succeeded', 'failed', 'cancelling', 'cancelled'
    )),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS studio_runs (
    id TEXT PRIMARY KEY NOT NULL,
    batch_id TEXT NOT NULL REFERENCES studio_batches(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    operation TEXT NOT NULL,
    model_manifest_json TEXT NOT NULL CHECK (json_valid(model_manifest_json)),
    settings_json TEXT NOT NULL CHECK (json_valid(settings_json)),
    owner_device_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN (
        'draft', 'quoting', 'awaiting_confirmation', 'queued', 'running', 'downloading',
        'succeeded', 'failed', 'cancelling', 'cancelled'
    )),
    quote_json TEXT CHECK (quote_json IS NULL OR json_valid(quote_json)),
    progress REAL CHECK (progress IS NULL OR (progress >= 0.0 AND progress <= 1.0)),
    error_json TEXT CHECK (error_json IS NULL OR json_valid(error_json)),
    output_count INTEGER NOT NULL CHECK (output_count > 0),
    display_aspect_width INTEGER NOT NULL CHECK (display_aspect_width > 0),
    display_aspect_height INTEGER NOT NULL CHECK (display_aspect_height > 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (batch_id, position)
) STRICT;

CREATE TABLE IF NOT EXISTS studio_assets (
    id TEXT PRIMARY KEY NOT NULL,
    relative_path TEXT NOT NULL UNIQUE,
    mime_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    content_hash TEXT NOT NULL,
    width INTEGER CHECK (width IS NULL OR width > 0),
    height INTEGER CHECK (height IS NULL OR height > 0),
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS studio_run_inputs (
    run_id TEXT NOT NULL REFERENCES studio_runs(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    asset_id TEXT REFERENCES studio_assets(id),
    artifact_id TEXT REFERENCES studio_artifacts(id),
    content_hash TEXT NOT NULL,
    PRIMARY KEY (run_id, role, ordinal),
    CHECK ((asset_id IS NOT NULL) != (artifact_id IS NOT NULL))
) STRICT;

CREATE TABLE IF NOT EXISTS studio_attempts (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL REFERENCES studio_runs(id) ON DELETE CASCADE,
    attempt_number INTEGER NOT NULL CHECK (attempt_number > 0),
    idempotency_key TEXT NOT NULL UNIQUE,
    request_json TEXT NOT NULL CHECK (json_valid(request_json)),
    request_wire_hash TEXT,
    remote_job_id TEXT,
    state TEXT NOT NULL CHECK (state IN (
        'prepared', 'submitting', 'submission_unknown', 'queued', 'running', 'succeeded',
        'failed', 'cancelled'
    )),
    provider_connection_id TEXT NOT NULL,
    response_metadata_json TEXT CHECK (
        response_metadata_json IS NULL OR json_valid(response_metadata_json)
    ),
    error_json TEXT CHECK (error_json IS NULL OR json_valid(error_json)),
    created_at INTEGER NOT NULL,
    submitted_at INTEGER,
    last_polled_at INTEGER,
    completed_at INTEGER,
    UNIQUE (run_id, attempt_number)
) STRICT;

CREATE UNIQUE INDEX IF NOT EXISTS studio_one_active_attempt_per_run
ON studio_attempts(run_id)
WHERE state IN ('prepared', 'submitting', 'submission_unknown', 'queued', 'running');

CREATE TABLE IF NOT EXISTS studio_artifacts (
    id TEXT PRIMARY KEY NOT NULL,
    run_id TEXT NOT NULL REFERENCES studio_runs(id) ON DELETE RESTRICT,
    attempt_id TEXT NOT NULL REFERENCES studio_attempts(id) ON DELETE RESTRICT,
    output_position INTEGER NOT NULL CHECK (output_position >= 0),
    media_kind TEXT NOT NULL CHECK (media_kind IN ('image', 'video')),
    relative_path TEXT NOT NULL UNIQUE,
    mime_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    content_hash TEXT NOT NULL,
    width INTEGER CHECK (width IS NULL OR width > 0),
    height INTEGER CHECK (height IS NULL OR height > 0),
    duration_seconds REAL CHECK (duration_seconds IS NULL OR duration_seconds >= 0.0),
    metadata_json TEXT NOT NULL CHECK (json_valid(metadata_json)),
    preview_relative_path TEXT,
    created_at INTEGER NOT NULL,
    deleted_at INTEGER,
    UNIQUE (run_id, output_position)
) STRICT;

CREATE TABLE IF NOT EXISTS studio_model_catalogs (
    provider_id TEXT PRIMARY KEY NOT NULL,
    catalog_json TEXT NOT NULL CHECK (json_valid(catalog_json)),
    fetched_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS studio_run_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES studio_runs(id) ON DELETE CASCADE,
    attempt_id TEXT REFERENCES studio_attempts(id) ON DELETE CASCADE,
    state TEXT NOT NULL,
    detail_json TEXT CHECK (detail_json IS NULL OR json_valid(detail_json)),
    created_at INTEGER NOT NULL
) STRICT;

PRAGMA user_version = 1;
COMMIT;
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StudioProviderDescriptor {
    pub id: ProviderId,
    pub submission_capabilities: SubmissionCapabilities,
}

#[derive(Debug, thiserror::Error)]
pub enum StudioRegistryError {
    #[error("studio provider {0:?} is already registered")]
    Duplicate(ProviderId),
    #[error("studio provider registry lock is poisoned")]
    LockPoisoned,
}

/// Runtime provider registry. Engine job logic depends only on [`MediaProvider`].
#[derive(Default)]
pub struct StudioProviderRegistry {
    providers: RwLock<BTreeMap<ProviderId, Arc<dyn MediaProvider>>>,
}

impl StudioProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, provider: Arc<dyn MediaProvider>) -> Result<(), StudioRegistryError> {
        let id = provider.id();
        let mut providers = self
            .providers
            .write()
            .map_err(|_| StudioRegistryError::LockPoisoned)?;
        if providers.contains_key(&id) {
            return Err(StudioRegistryError::Duplicate(id));
        }
        providers.insert(id, provider);
        Ok(())
    }

    pub fn get(
        &self,
        provider_id: &ProviderId,
    ) -> Result<Option<Arc<dyn MediaProvider>>, StudioRegistryError> {
        Ok(self
            .providers
            .read()
            .map_err(|_| StudioRegistryError::LockPoisoned)?
            .get(provider_id)
            .cloned())
    }

    pub fn list(&self) -> Result<Vec<StudioProviderDescriptor>, StudioRegistryError> {
        Ok(self
            .providers
            .read()
            .map_err(|_| StudioRegistryError::LockPoisoned)?
            .values()
            .map(|provider| StudioProviderDescriptor {
                id: provider.id(),
                submission_capabilities: provider.submission_capabilities(),
            })
            .collect())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StudioStoreError {
    #[error("studio database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("studio filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid artifact extension")]
    InvalidExtension,
    #[error("artifact is larger than the configured limit")]
    ArtifactTooLarge,
    #[error("artifact already exists")]
    ArtifactExists,
    #[error("artifact is not a regular file")]
    InvalidArtifact,
    #[error("studio database schema {0} is newer than this application supports")]
    NewerSchema(i64),
    #[error("studio database lock is poisoned")]
    LockPoisoned,
    #[error("invalid studio value: {0}")]
    InvalidValue(String),
    #[error("studio conversation was not found")]
    ConversationNotFound,
}

/// SQLite catalog rooted under one active profile.
pub struct StudioStore {
    database_path: PathBuf,
    connection: Mutex<Connection>,
    artifacts: ArtifactStore,
}

impl StudioStore {
    pub fn open(
        profile_store_root: &Path,
        maximum_artifact_bytes: u64,
    ) -> Result<Self, StudioStoreError> {
        let studio_root = profile_store_root.join("studio");
        fs::create_dir_all(&studio_root)?;
        let database_path = studio_root.join(DATABASE_FILE);
        let connection = Connection::open(&database_path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StudioStoreError::NewerSchema(version));
        }
        if version == 0 {
            connection.execute_batch(SCHEMA_V1)?;
        }

        Ok(Self {
            database_path,
            connection: Mutex::new(connection),
            artifacts: ArtifactStore::open(&studio_root, maximum_artifact_bytes)?,
        })
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn artifacts(&self) -> &ArtifactStore {
        &self.artifacts
    }

    pub fn connection(&self) -> Result<MutexGuard<'_, Connection>, StudioStoreError> {
        self.connection
            .lock()
            .map_err(|_| StudioStoreError::LockPoisoned)
    }

    pub fn create_conversation(
        &self,
        title: &str,
        forked_from_turn_id: Option<StudioTurnId>,
    ) -> Result<StudioConversationSummary, StudioStoreError> {
        let title = validate_title(title)?;
        let id = StudioConversationId::new();
        let now = chrono::Utc::now().timestamp_millis();
        self.connection()?.execute(
            "INSERT INTO studio_conversations (id, title, created_at, updated_at, forked_from_turn_id) VALUES (?1, ?2, ?3, ?3, ?4)",
            rusqlite::params![
                id.0.to_string(),
                title,
                now,
                forked_from_turn_id.map(|id| id.0.to_string())
            ],
        )?;
        self.conversation(id)
    }

    pub fn list_conversations(
        &self,
        include_archived: bool,
    ) -> Result<Vec<StudioConversationSummary>, StudioStoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT c.id, c.title, COUNT(t.id), c.created_at, c.updated_at, c.archived_at, c.forked_from_turn_id
             FROM studio_conversations c
             LEFT JOIN studio_turns t ON t.conversation_id = c.id
             WHERE (?1 OR c.archived_at IS NULL)
             GROUP BY c.id
             ORDER BY c.updated_at DESC, c.id DESC",
        )?;
        let rows = statement.query_map([include_archived], conversation_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn rename_conversation(
        &self,
        id: StudioConversationId,
        title: &str,
    ) -> Result<StudioConversationSummary, StudioStoreError> {
        let title = validate_title(title)?;
        let changed = self.connection()?.execute(
            "UPDATE studio_conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
            rusqlite::params![
                id.0.to_string(),
                title,
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        if changed == 0 {
            return Err(StudioStoreError::ConversationNotFound);
        }
        self.conversation(id)
    }

    pub fn archive_conversation(
        &self,
        id: StudioConversationId,
        archived: bool,
    ) -> Result<StudioConversationSummary, StudioStoreError> {
        let now = chrono::Utc::now().timestamp_millis();
        let changed = self.connection()?.execute(
            "UPDATE studio_conversations SET archived_at = ?2, updated_at = ?3 WHERE id = ?1",
            rusqlite::params![id.0.to_string(), archived.then_some(now), now],
        )?;
        if changed == 0 {
            return Err(StudioStoreError::ConversationNotFound);
        }
        self.conversation(id)
    }

    fn conversation(
        &self,
        id: StudioConversationId,
    ) -> Result<StudioConversationSummary, StudioStoreError> {
        self.connection()?
            .query_row(
                "SELECT c.id, c.title, COUNT(t.id), c.created_at, c.updated_at, c.archived_at, c.forked_from_turn_id
                 FROM studio_conversations c
                 LEFT JOIN studio_turns t ON t.conversation_id = c.id
                 WHERE c.id = ?1
                 GROUP BY c.id",
                [id.0.to_string()],
                conversation_from_row,
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::ConversationNotFound,
                other => other.into(),
            })
    }
}

fn validate_title(title: &str) -> Result<&str, StudioStoreError> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(StudioStoreError::InvalidValue(
            "conversation title must contain 1 to 200 characters".to_owned(),
        ));
    }
    Ok(title)
}

fn conversation_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<StudioConversationSummary, rusqlite::Error> {
    use rusqlite::types::Type;
    let parse_uuid = |index, value: String| {
        Uuid::parse_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
        })
    };
    let created_at_ms: i64 = row.get(3)?;
    let updated_at_ms: i64 = row.get(4)?;
    let timestamp = |index, value| {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(value).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Integer,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "timestamp is outside the supported range",
                )),
            )
        })
    };
    let forked: Option<String> = row.get(6)?;
    Ok(StudioConversationSummary {
        id: StudioConversationId(parse_uuid(0, row.get(0)?)?),
        title: row.get(1)?,
        turn_count: row.get(2)?,
        created_at: timestamp(3, created_at_ms)?,
        updated_at: timestamp(4, updated_at_ms)?,
        archived: row.get::<_, Option<i64>>(5)?.is_some(),
        forked_from_turn_id: forked
            .map(|value| parse_uuid(6, value).map(StudioTurnId))
            .transpose()?,
    })
}

/// ID-addressed storage. Callers never provide a path or filename.
pub struct ArtifactStore {
    root: PathBuf,
    maximum_artifact_bytes: u64,
}

impl ArtifactStore {
    fn open(studio_root: &Path, maximum_artifact_bytes: u64) -> Result<Self, StudioStoreError> {
        let root = studio_root.join("artifacts");
        fs::create_dir_all(&root)?;
        if fs::symlink_metadata(&root)?.file_type().is_symlink() {
            return Err(StudioStoreError::InvalidArtifact);
        }
        Ok(Self {
            root,
            maximum_artifact_bytes,
        })
    }

    pub fn publish(
        &self,
        artifact_id: StudioArtifactId,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, StudioStoreError> {
        if bytes.len() as u64 > self.maximum_artifact_bytes {
            return Err(StudioStoreError::ArtifactTooLarge);
        }
        let destination = self.path_for(artifact_id, extension)?;
        let temporary = self.root.join(format!(
            ".{}.tmp-{}-{}",
            artifact_id.0,
            std::process::id(),
            Uuid::new_v4()
        ));
        let write_result = (|| -> Result<(), StudioStoreError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }

        let publish_result = fs::hard_link(&temporary, &destination);
        let _ = fs::remove_file(&temporary);
        match publish_result {
            Ok(()) => {
                sync_directory(&self.root)?;
                Ok(destination)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(StudioStoreError::ArtifactExists)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn open_artifact(
        &self,
        artifact_id: StudioArtifactId,
        extension: &str,
    ) -> Result<File, StudioStoreError> {
        let path = self.path_for(artifact_id, extension)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StudioStoreError::InvalidArtifact);
        }
        Ok(File::open(path)?)
    }

    pub fn delete(
        &self,
        artifact_id: StudioArtifactId,
        extension: &str,
    ) -> Result<(), StudioStoreError> {
        let path = self.path_for(artifact_id, extension)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StudioStoreError::InvalidArtifact);
        }
        fs::remove_file(path)?;
        sync_directory(&self.root)?;
        Ok(())
    }

    fn path_for(
        &self,
        artifact_id: StudioArtifactId,
        extension: &str,
    ) -> Result<PathBuf, StudioStoreError> {
        if extension.is_empty()
            || extension.len() > 10
            || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err(StudioStoreError::InvalidExtension);
        }
        Ok(self.root.join(format!(
            "{}.{}",
            artifact_id.0,
            extension.to_ascii_lowercase()
        )))
    }
}

fn sync_directory(path: &Path) -> Result<(), std::io::Error> {
    File::open(path)?.sync_all()
}
