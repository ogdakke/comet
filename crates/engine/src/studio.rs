//! Profile-scoped durable storage for Studio metadata and generated media.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, RwLock},
    time::Duration,
};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeron_proto::{
    ListStudioModelsResponse, StudioArtifactChunk, StudioArtifactView, StudioConversationSummary,
    StudioConversationView, StudioGalleryItem, StudioModelRunSpec, StudioRunState, StudioRunView,
    StudioTurnView, UNTITLED_STUDIO_TITLE,
};
use zeron_studio::{
    GenerationRequest, MediaModel, MediaProvider, ProviderArtifact, ProviderId, Quote,
    StudioArtifactId, StudioAttemptId, StudioBatchId, StudioConversationId, StudioRunId,
    StudioTurnId, SubmissionCapabilities,
};

const DATABASE_FILE: &str = "studio.sqlite3";
const SCHEMA_VERSION: i64 = 1;
const MAX_CREATE_TURN_RUNS: usize = 16;
const MAX_TURN_RUNS: usize = 64;
pub(crate) const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const STUDIO_CATALOG_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const ARTIFACT_READ_CHUNK_BYTES: u64 = 192_000;
const ARTIFACT_FORMATS: &[(&str, &str)] = &[
    ("webp", "image/webp"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("mp4", "video/mp4"),
    ("mov", "video/quicktime"),
    ("webm", "video/webm"),
];

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
    #[error("studio artifact was not found")]
    ArtifactNotFound,
    #[error("studio database schema {0} is newer than this application supports")]
    NewerSchema(i64),
    #[error("studio database lock is poisoned")]
    LockPoisoned,
    #[error("invalid studio value: {0}")]
    InvalidValue(String),
    #[error("studio conversation was not found")]
    ConversationNotFound,
    #[error("studio turn was not found")]
    TurnNotFound,
    #[error("studio run was not found")]
    RunNotFound,
}

#[derive(Clone)]
pub struct PreparedStudioRun {
    pub model: MediaModel,
    pub request: GenerationRequest,
    pub quote: Option<Quote>,
}

#[derive(Clone, Debug)]
pub struct StoredStudioRun {
    pub run_id: StudioRunId,
    pub attempt_id: StudioAttemptId,
    pub idempotency_key: String,
    pub request: GenerationRequest,
}

/// SQLite catalog rooted under one active profile.
pub struct StudioStore {
    database_path: PathBuf,
    connection: Mutex<Connection>,
    artifacts: ArtifactStore,
    changes: tokio::sync::watch::Sender<u64>,
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

        let (changes, _) = tokio::sync::watch::channel(0);
        Ok(Self {
            database_path,
            connection: Mutex::new(connection),
            artifacts: ArtifactStore::open(&studio_root, maximum_artifact_bytes)?,
            changes,
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

    /// Resolve image attempts interrupted by a process restart. A prepared attempt is known not to
    /// have sent network bytes and becomes an ordinary retryable failure. Once submission began,
    /// the provider may have charged for work, so preserve the ambiguity and require Retry anyway.
    pub fn recover_interrupted_image_runs(&self) -> Result<usize, StudioStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let now = chrono::Utc::now().timestamp_millis();
        let rows = {
            let mut statement = transaction.prepare(
                "SELECT a.id, a.run_id, a.state
                 FROM studio_attempts a
                 JOIN studio_runs r ON r.id = a.run_id
                 WHERE a.state IN ('prepared', 'submitting', 'queued', 'running')
                   AND r.state IN ('queued', 'running', 'downloading')",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (attempt_id, run_id, old_state) in &rows {
            let (attempt_state, message) = if old_state == "prepared" {
                ("failed", "generation was interrupted before submission")
            } else {
                (
                    "submission_unknown",
                    "generation was interrupted during submission; provider work may have completed",
                )
            };
            let error = serde_json::json!({ "message": message }).to_string();
            transaction.execute(
                "UPDATE studio_attempts SET state = ?2, error_json = ?3, completed_at = CASE WHEN ?2 = 'failed' THEN ?4 ELSE NULL END WHERE id = ?1",
                rusqlite::params![attempt_id, attempt_state, error, now],
            )?;
            transaction.execute(
                "UPDATE studio_runs SET state = 'failed', error_json = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![run_id, error, now],
            )?;
            transaction.execute(
                "INSERT INTO studio_run_events (run_id, attempt_id, state, detail_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![run_id, attempt_id, attempt_state, error, now],
            )?;
        }
        transaction.commit()?;
        if !rows.is_empty() {
            self.notify_change();
        }
        Ok(rows.len())
    }

    pub fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn notify_change(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
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
        self.notify_change();
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

    pub fn list_gallery(&self) -> Result<Vec<StudioGalleryItem>, StudioStoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT a.id, a.output_position, a.media_kind, a.mime_type, a.size_bytes,
                    a.width, a.height, a.created_at,
                    t.conversation_id, t.id, t.prompt, r.model_manifest_json
             FROM studio_artifacts a
             JOIN studio_runs r ON r.id = a.run_id
             JOIN studio_batches b ON b.id = r.batch_id
             JOIN studio_turns t ON t.id = b.turn_id
             WHERE a.deleted_at IS NULL AND a.media_kind = 'image'
             ORDER BY a.created_at DESC, a.id DESC",
        )?;
        let rows = statement.query_map([], gallery_item_from_row)?;
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
        self.notify_change();
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
        self.notify_change();
        self.conversation(id)
    }

    pub fn create_turn(
        &self,
        conversation_id: StudioConversationId,
        prompt: &str,
        source_turn_id: Option<StudioTurnId>,
        runs: &[PreparedStudioRun],
        owner_device_id: &str,
    ) -> Result<Vec<StoredStudioRun>, StudioStoreError> {
        let prompt = prompt.trim();
        if prompt.is_empty() || prompt.chars().count() > 32_000 {
            return Err(StudioStoreError::InvalidValue(
                "prompt must contain 1 to 32000 characters".into(),
            ));
        }
        if runs.is_empty() || runs.len() > MAX_CREATE_TURN_RUNS {
            return Err(StudioStoreError::InvalidValue(
                "a turn must contain 1 to 16 model runs".into(),
            ));
        }
        if runs.iter().any(|run| !run.request.inputs.is_empty()) {
            return Err(StudioStoreError::InvalidValue(
                "studio inputs are not available in the image slice".into(),
            ));
        }

        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM studio_conversations WHERE id = ?1 AND archived_at IS NULL)",
            [conversation_id.0.to_string()],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StudioStoreError::ConversationNotFound);
        }
        let position: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM studio_turns WHERE conversation_id = ?1",
            [conversation_id.0.to_string()],
            |row| row.get(0),
        )?;
        let now = chrono::Utc::now().timestamp_millis();
        let turn_id = StudioTurnId::new();
        let batch_id = StudioBatchId::new();
        transaction.execute(
            "INSERT INTO studio_turns (id, conversation_id, position, prompt, source_turn_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![turn_id.0.to_string(), conversation_id.0.to_string(), position, prompt, source_turn_id.map(|id| id.0.to_string()), now],
        )?;
        transaction.execute(
            "INSERT INTO studio_batches (id, turn_id, state, created_at, updated_at) VALUES (?1, ?2, 'queued', ?3, ?3)",
            rusqlite::params![batch_id.0.to_string(), turn_id.0.to_string(), now],
        )?;

        let stored = insert_prepared_runs(&transaction, batch_id, 0, runs, owner_device_id, now)?;
        if let Some(title) = (position == 0).then(|| title_from_prompt(prompt)).flatten() {
            transaction.execute(
                "UPDATE studio_conversations
                 SET title = CASE WHEN title = ?4 THEN ?2 ELSE title END,
                     updated_at = ?3
                 WHERE id = ?1",
                rusqlite::params![
                    conversation_id.0.to_string(),
                    title,
                    now,
                    UNTITLED_STUDIO_TITLE
                ],
            )?;
        } else {
            transaction.execute(
                "UPDATE studio_conversations SET updated_at = ?2 WHERE id = ?1",
                rusqlite::params![conversation_id.0.to_string(), now],
            )?;
        }
        transaction.commit()?;
        self.notify_change();
        Ok(stored)
    }

    /// Original model-run specs for a turn: one per distinct first-generation
    /// snapshot, so "generate more" never doubles already-appended copies.
    pub fn turn_extend_spec(
        &self,
        turn_id: StudioTurnId,
    ) -> Result<(StudioConversationId, String, Vec<StudioModelRunSpec>), StudioStoreError> {
        let connection = self.connection()?;
        let (conversation_id, prompt, archived): (String, String, Option<i64>) = connection
            .query_row(
                "SELECT t.conversation_id, t.prompt, c.archived_at
                 FROM studio_turns t
                 JOIN studio_conversations c ON c.id = t.conversation_id
                 WHERE t.id = ?1",
                [turn_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::TurnNotFound,
                other => other.into(),
            })?;
        if archived.is_some() {
            return Err(StudioStoreError::ConversationNotFound);
        }
        let mut statement = connection.prepare(
            "SELECT a.request_json
             FROM studio_runs r
             JOIN studio_batches b ON b.id = r.batch_id
             JOIN studio_attempts a ON a.run_id = r.id AND a.attempt_number = 1
             WHERE b.turn_id = ?1
             ORDER BY r.position",
        )?;
        let requests = statement
            .query_map([turn_id.0.to_string()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut specs = Vec::new();
        let mut seen = Vec::new();
        for request_json in requests {
            let request: GenerationRequest = serde_json::from_str(&request_json)
                .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
            let key = (
                request.provider_id.clone(),
                request.model_id.clone(),
                request.controls.clone(),
                request.output_count,
                request.display_aspect_ratio,
            );
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            specs.push(StudioModelRunSpec {
                provider_id: request.provider_id,
                model_id: request.model_id,
                operation: request.operation,
                output_count: request.output_count,
                controls: request.controls,
                inputs: request.inputs,
                manifest_version: request.manifest_version,
                display_aspect_ratio: request.display_aspect_ratio,
            });
        }
        if specs.is_empty() {
            return Err(StudioStoreError::InvalidValue(
                "turn has no model runs to generate more from".into(),
            ));
        }
        Ok((
            StudioConversationId(parse_uuid(&conversation_id)?),
            prompt,
            specs,
        ))
    }

    pub fn extend_turn(
        &self,
        turn_id: StudioTurnId,
        runs: &[PreparedStudioRun],
        owner_device_id: &str,
    ) -> Result<(StudioConversationId, Vec<StoredStudioRun>), StudioStoreError> {
        if runs.is_empty() || runs.len() > MAX_CREATE_TURN_RUNS {
            return Err(StudioStoreError::InvalidValue(
                "generate more must add 1 to 16 model runs".into(),
            ));
        }
        if runs.iter().any(|run| !run.request.inputs.is_empty()) {
            return Err(StudioStoreError::InvalidValue(
                "studio inputs are not available in the image slice".into(),
            ));
        }

        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let (conversation_id, batch_id, archived): (String, String, Option<i64>) = transaction
            .query_row(
                "SELECT t.conversation_id, b.id, c.archived_at
                 FROM studio_turns t
                 JOIN studio_batches b ON b.turn_id = t.id
                 JOIN studio_conversations c ON c.id = t.conversation_id
                 WHERE t.id = ?1",
                [turn_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::TurnNotFound,
                other => other.into(),
            })?;
        if archived.is_some() {
            return Err(StudioStoreError::ConversationNotFound);
        }
        let current: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM studio_runs WHERE batch_id = ?1",
            [batch_id.clone()],
            |row| row.get(0),
        )?;
        if current as usize + runs.len() > MAX_TURN_RUNS {
            return Err(StudioStoreError::InvalidValue(format!(
                "a turn can contain at most {MAX_TURN_RUNS} model runs"
            )));
        }
        let next_position: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM studio_runs WHERE batch_id = ?1",
            [batch_id.clone()],
            |row| row.get(0),
        )?;
        let now = chrono::Utc::now().timestamp_millis();
        let batch_id = StudioBatchId(parse_uuid(&batch_id)?);
        let stored = insert_prepared_runs(
            &transaction,
            batch_id,
            next_position,
            runs,
            owner_device_id,
            now,
        )?;
        if let Some(run) = stored.first() {
            recompute_batch(&transaction, run.run_id, now)?;
        }
        transaction.execute(
            "UPDATE studio_conversations SET updated_at = ?2 WHERE id = ?1",
            rusqlite::params![conversation_id.clone(), now],
        )?;
        transaction.commit()?;
        self.notify_change();
        Ok((StudioConversationId(parse_uuid(&conversation_id)?), stored))
    }

    pub fn delete_conversation(&self, id: StudioConversationId) -> Result<(), StudioStoreError> {
        let artifacts: Vec<(StudioArtifactId, String)> = {
            let connection = self.connection()?;
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM studio_conversations WHERE id = ?1)",
                [id.0.to_string()],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StudioStoreError::ConversationNotFound);
            }
            let mut statement = connection.prepare(
                "SELECT a.id, a.mime_type
                 FROM studio_artifacts a
                 JOIN studio_runs r ON r.id = a.run_id
                 JOIN studio_batches b ON b.id = r.batch_id
                 JOIN studio_turns t ON t.id = b.turn_id
                 WHERE t.conversation_id = ?1",
            )?;
            let rows = statement
                .query_map([id.0.to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|(artifact_id, mime)| {
                    parse_uuid(&artifact_id).map(|uuid| (StudioArtifactId(uuid), mime))
                })
                .collect::<Result<Vec<_>, _>>()?
        };

        {
            let mut connection = self.connection()?;
            let transaction = connection.transaction()?;
            transaction.execute(
                "UPDATE studio_conversations SET forked_from_turn_id = NULL
                 WHERE forked_from_turn_id IN (
                     SELECT id FROM studio_turns WHERE conversation_id = ?1
                 )",
                [id.0.to_string()],
            )?;
            for (artifact_id, _) in &artifacts {
                transaction.execute(
                    "DELETE FROM studio_run_inputs WHERE artifact_id = ?1",
                    [artifact_id.0.to_string()],
                )?;
                transaction.execute(
                    "DELETE FROM studio_artifacts WHERE id = ?1",
                    [artifact_id.0.to_string()],
                )?;
            }
            let changed = transaction.execute(
                "DELETE FROM studio_conversations WHERE id = ?1",
                [id.0.to_string()],
            )?;
            if changed == 0 {
                return Err(StudioStoreError::ConversationNotFound);
            }
            transaction.commit()?;
        }

        for (artifact_id, mime) in artifacts {
            if let Some(extension) = extension_for_mime(&mime) {
                match self.artifacts.delete(artifact_id, extension) {
                    Ok(()) => {}
                    Err(StudioStoreError::Io(error))
                        if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::warn!(
                            artifact = %artifact_id.0,
                            error = %error,
                            "studio conversation delete left an artifact file"
                        );
                    }
                }
            }
        }

        self.notify_change();
        Ok(())
    }

    pub fn cache_models(
        &self,
        provider_id: &ProviderId,
        models: &[MediaModel],
        ttl: std::time::Duration,
    ) -> Result<ListStudioModelsResponse, StudioStoreError> {
        let fetched_at = chrono::Utc::now();
        let expires_at = fetched_at
            + chrono::Duration::from_std(ttl)
                .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
        let catalog = serde_json::to_string(models)
            .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
        self.connection()?.execute(
            "INSERT INTO studio_model_catalogs (provider_id, catalog_json, fetched_at, expires_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(provider_id) DO UPDATE SET catalog_json = excluded.catalog_json, fetched_at = excluded.fetched_at, expires_at = excluded.expires_at",
            rusqlite::params![provider_id.as_str(), catalog, fetched_at.timestamp_millis(), expires_at.timestamp_millis()],
        )?;
        Ok(ListStudioModelsResponse {
            models: models.to_vec(),
            fetched_at,
            stale: false,
        })
    }

    pub fn cached_models(
        &self,
        provider_id: &ProviderId,
    ) -> Result<Option<ListStudioModelsResponse>, StudioStoreError> {
        let connection = self.connection()?;
        let result = connection.query_row(
            "SELECT catalog_json, fetched_at, expires_at FROM studio_model_catalogs WHERE provider_id = ?1",
            [provider_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
        );
        let (catalog, fetched_at, expires_at) = match result {
            Ok(result) => result,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let models: Vec<MediaModel> = serde_json::from_str(&catalog)
            .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
        let expired = chrono::Utc::now().timestamp_millis() >= expires_at;
        let placeholder_pricing = models.iter().any(|model| {
            model
                .pricing
                .as_ref()
                .is_some_and(zeron_studio::PricingMetadata::is_placeholder)
        });
        Ok(Some(ListStudioModelsResponse {
            models,
            fetched_at: timestamp(fetched_at)?,
            stale: expired || placeholder_pricing,
        }))
    }

    pub fn delete_artifact(&self, artifact_id: StudioArtifactId) -> Result<(), StudioStoreError> {
        let connection = self.connection()?;
        let mime_type = connection
            .query_row(
                "SELECT mime_type FROM studio_artifacts WHERE id = ?1 AND deleted_at IS NULL",
                [artifact_id.0.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::ArtifactNotFound,
                other => other.into(),
            })?;
        let extension = extension_for_mime(&mime_type).ok_or(StudioStoreError::InvalidExtension)?;
        let now = chrono::Utc::now().timestamp_millis();
        connection.execute(
            "UPDATE studio_artifacts SET deleted_at = ?2 WHERE id = ?1",
            rusqlite::params![artifact_id.0.to_string(), now],
        )?;
        drop(connection);
        if let Err(error) = self.artifacts.delete(artifact_id, extension) {
            let _ = self.connection()?.execute(
                "UPDATE studio_artifacts SET deleted_at = NULL WHERE id = ?1",
                [artifact_id.0.to_string()],
            );
            return Err(error);
        }
        self.notify_change();
        Ok(())
    }

    pub fn prepare_retry(
        &self,
        run_id: StudioRunId,
        retry_anyway: bool,
    ) -> Result<(StoredStudioRun, StudioConversationId), StudioStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let row = transaction
            .query_row(
                "SELECT a.id, a.request_json, a.state, r.provider_id, t.conversation_id
             FROM studio_runs r
             JOIN studio_batches b ON b.id = r.batch_id
             JOIN studio_turns t ON t.id = b.turn_id
             JOIN studio_attempts a ON a.run_id = r.id
             WHERE r.id = ?1 ORDER BY a.attempt_number DESC LIMIT 1",
                [run_id.0.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::RunNotFound,
                other => other.into(),
            })?;
        if row.2 == "submission_unknown" && !retry_anyway {
            return Err(StudioStoreError::InvalidValue(
                "retrying this uncertain submission may duplicate provider work; explicit confirmation is required".into(),
            ));
        }
        if !matches!(
            row.2.as_str(),
            "failed" | "cancelled" | "submission_unknown"
        ) {
            return Err(StudioStoreError::InvalidValue(
                "only terminal failed or uncertain runs can be retried".into(),
            ));
        }
        let request: GenerationRequest = serde_json::from_str(&row.1)
            .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
        let attempt_number: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(attempt_number), 0) + 1 FROM studio_attempts WHERE run_id = ?1",
            [run_id.0.to_string()],
            |row| row.get(0),
        )?;
        let attempt_id = StudioAttemptId::new();
        let idempotency_key = Uuid::new_v4().to_string();
        let request_hash = format!("{:x}", Sha256::digest(row.1.as_bytes()));
        let now = chrono::Utc::now().timestamp_millis();
        if row.2 == "submission_unknown" {
            transaction.execute(
                "UPDATE studio_attempts SET state = 'failed', completed_at = ?2 WHERE id = ?1",
                rusqlite::params![row.0, now],
            )?;
        }
        transaction.execute(
            "INSERT INTO studio_attempts (id, run_id, attempt_number, idempotency_key, request_json, request_wire_hash, state, provider_connection_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'prepared', ?7, ?8)",
            rusqlite::params![attempt_id.0.to_string(), run_id.0.to_string(), attempt_number, idempotency_key, row.1, request_hash, row.3, now],
        )?;
        transaction.execute(
            "UPDATE studio_runs SET state = 'queued', progress = NULL, error_json = NULL, updated_at = ?2 WHERE id = ?1",
            rusqlite::params![run_id.0.to_string(), now],
        )?;
        transaction.execute(
            "INSERT INTO studio_run_events (run_id, attempt_id, state, created_at) VALUES (?1, ?2, 'prepared', ?3)",
            rusqlite::params![run_id.0.to_string(), attempt_id.0.to_string(), now],
        )?;
        transaction.commit()?;
        self.notify_change();
        Ok((
            StoredStudioRun {
                run_id,
                attempt_id,
                idempotency_key,
                request,
            },
            StudioConversationId(parse_uuid(&row.4)?),
        ))
    }

    pub fn mark_submitting(&self, run: &StoredStudioRun) -> Result<(), StudioStoreError> {
        let now = chrono::Utc::now().timestamp_millis();
        let changed = self.connection()?.execute(
            "UPDATE studio_attempts SET state = 'submitting', submitted_at = ?3 WHERE id = ?1 AND run_id = ?2 AND state = 'prepared'",
            rusqlite::params![run.attempt_id.0.to_string(), run.run_id.0.to_string(), now],
        )?;
        if changed != 1 {
            return Err(StudioStoreError::RunNotFound);
        }
        self.set_run_state(run.run_id, run.attempt_id, "running", None)
    }

    pub fn complete_run(
        &self,
        run: &StoredStudioRun,
        artifacts: &[ProviderArtifact],
    ) -> Result<(), StudioStoreError> {
        if artifacts.len() != run.request.output_count as usize {
            return self.fail_run(
                run,
                &format!(
                    "provider returned {} artifacts; {} were requested",
                    artifacts.len(),
                    run.request.output_count
                ),
                false,
            );
        }
        let mut published = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            if !artifact_bytes_match_mime(&artifact.mime_type, &artifact.bytes) {
                return self.fail_run(
                    run,
                    &format!(
                        "provider artifact bytes do not match declared MIME {}",
                        artifact.mime_type
                    ),
                    false,
                );
            }
            let id = StudioArtifactId::new();
            let extension = extension_for_mime(&artifact.mime_type).ok_or_else(|| {
                StudioStoreError::InvalidValue(format!(
                    "unsupported provider artifact MIME {}",
                    artifact.mime_type
                ))
            })?;
            match self.artifacts.publish(id, extension, &artifact.bytes) {
                Ok(path) => published.push((id, path, extension, artifact)),
                Err(error) => {
                    for (id, _, extension, _) in &published {
                        let _ = self.artifacts.delete(*id, extension);
                    }
                    return Err(error);
                }
            }
        }

        let result = (|| {
            let mut connection = self.connection()?;
            let transaction = connection.transaction()?;
            let now = chrono::Utc::now().timestamp_millis();
            for (position, (id, path, _, artifact)) in published.iter().enumerate() {
                let relative_path = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or(StudioStoreError::InvalidArtifact)?;
                let hash = format!("{:x}", Sha256::digest(&artifact.bytes));
                transaction.execute(
                    "INSERT INTO studio_artifacts (id, run_id, attempt_id, output_position, media_kind, relative_path, mime_type, size_bytes, content_hash, width, height, duration_seconds, metadata_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    rusqlite::params![id.0.to_string(), run.run_id.0.to_string(), run.attempt_id.0.to_string(), position as i64, media_kind_name(artifact.media_kind), relative_path, artifact.mime_type, artifact.bytes.len() as i64, hash, artifact.width, artifact.height, artifact.duration_seconds, artifact.metadata.to_string(), now],
                )?;
            }
            transaction.execute(
                "UPDATE studio_attempts SET state = 'succeeded', completed_at = ?2 WHERE id = ?1",
                rusqlite::params![run.attempt_id.0.to_string(), now],
            )?;
            transaction.execute(
                "UPDATE studio_runs SET state = 'succeeded', progress = 1.0, updated_at = ?2 WHERE id = ?1",
                rusqlite::params![run.run_id.0.to_string(), now],
            )?;
            transaction.execute(
                "INSERT INTO studio_run_events (run_id, attempt_id, state, created_at) VALUES (?1, ?2, 'succeeded', ?3)",
                rusqlite::params![run.run_id.0.to_string(), run.attempt_id.0.to_string(), now],
            )?;
            recompute_batch(&transaction, run.run_id, now)?;
            transaction.commit()?;
            Ok::<(), StudioStoreError>(())
        })();
        if result.is_err() {
            for (id, _, extension, _) in &published {
                let _ = self.artifacts.delete(*id, extension);
            }
        }
        if result.is_ok() {
            self.notify_change();
        }
        result
    }

    pub fn fail_run(
        &self,
        run: &StoredStudioRun,
        message: &str,
        submission_unknown: bool,
    ) -> Result<(), StudioStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let now = chrono::Utc::now().timestamp_millis();
        let attempt_state = if submission_unknown {
            "submission_unknown"
        } else {
            "failed"
        };
        let error = serde_json::json!({ "message": message }).to_string();
        transaction.execute(
            "UPDATE studio_attempts SET state = ?2, error_json = ?3, completed_at = CASE WHEN ?2 = 'failed' THEN ?4 ELSE NULL END WHERE id = ?1",
            rusqlite::params![run.attempt_id.0.to_string(), attempt_state, error, now],
        )?;
        transaction.execute(
            "UPDATE studio_runs SET state = 'failed', error_json = ?2, updated_at = ?3 WHERE id = ?1",
            rusqlite::params![run.run_id.0.to_string(), error, now],
        )?;
        transaction.execute(
            "INSERT INTO studio_run_events (run_id, attempt_id, state, detail_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![run.run_id.0.to_string(), run.attempt_id.0.to_string(), attempt_state, error, now],
        )?;
        recompute_batch(&transaction, run.run_id, now)?;
        transaction.commit()?;
        self.notify_change();
        Ok(())
    }

    fn set_run_state(
        &self,
        run_id: StudioRunId,
        attempt_id: StudioAttemptId,
        state: &str,
        progress: Option<f32>,
    ) -> Result<(), StudioStoreError> {
        let now = chrono::Utc::now().timestamp_millis();
        let connection = self.connection()?;
        connection.execute(
            "UPDATE studio_runs SET state = ?2, progress = ?3, updated_at = ?4 WHERE id = ?1",
            rusqlite::params![run_id.0.to_string(), state, progress, now],
        )?;
        connection.execute(
            "INSERT INTO studio_run_events (run_id, attempt_id, state, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![run_id.0.to_string(), attempt_id.0.to_string(), state, now],
        )?;
        self.notify_change();
        Ok(())
    }

    pub fn conversation_view(
        &self,
        id: StudioConversationId,
    ) -> Result<StudioConversationView, StudioStoreError> {
        let summary = self.conversation(id)?;
        let connection = self.connection()?;
        let mut turns_statement = connection.prepare(
            "SELECT t.id, t.position, t.prompt, t.source_turn_id, b.id, t.created_at FROM studio_turns t JOIN studio_batches b ON b.turn_id = t.id WHERE t.conversation_id = ?1 ORDER BY t.position",
        )?;
        let turn_rows = turns_statement
            .query_map([id.0.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut turns = Vec::with_capacity(turn_rows.len());
        for (turn_id, position, prompt, source_id, batch_id, created_at) in turn_rows {
            let mut runs_statement = connection.prepare(
                "SELECT id, position, provider_id, model_manifest_json, settings_json, output_count, display_aspect_width, display_aspect_height, state, progress, error_json, quote_json FROM studio_runs WHERE batch_id = ?1 ORDER BY position",
            )?;
            let run_rows = runs_statement
                .query_map([batch_id.clone()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, u32>(5)?,
                        row.get::<_, u32>(6)?,
                        row.get::<_, u32>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<f32>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut run_views = Vec::with_capacity(run_rows.len());
            for (
                run_id,
                run_position,
                provider_id,
                model_json,
                settings_json,
                output_count,
                aspect_width,
                aspect_height,
                state,
                progress,
                error_json,
                quote_json,
            ) in run_rows
            {
                let mut artifacts_statement = connection.prepare(
                    "SELECT id, output_position, media_kind, mime_type, size_bytes, width, height, duration_seconds, metadata_json, created_at FROM studio_artifacts WHERE run_id = ?1 AND deleted_at IS NULL ORDER BY output_position",
                )?;
                let artifacts = artifacts_statement
                    .query_map([run_id.clone()], artifact_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                let model: MediaModel = serde_json::from_str(&model_json)
                    .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
                let controls = serde_json::from_str(&settings_json)
                    .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
                let error = error_json
                    .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
                    .and_then(|value| {
                        value
                            .get("message")
                            .and_then(|message| message.as_str())
                            .map(str::to_owned)
                    });
                let quote = quote_json
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
                run_views.push(StudioRunView {
                    id: StudioRunId(parse_uuid(&run_id)?),
                    position: run_position,
                    provider_id: ProviderId::new(provider_id),
                    model,
                    controls,
                    output_count,
                    display_aspect_ratio: (aspect_width, aspect_height),
                    state: parse_run_state(&state)?,
                    progress,
                    error,
                    quote,
                    artifacts,
                });
            }
            turns.push(StudioTurnView {
                id: StudioTurnId(parse_uuid(&turn_id)?),
                position,
                prompt,
                source_turn_id: source_id
                    .map(|value| parse_uuid(&value).map(StudioTurnId))
                    .transpose()?,
                batch_id: StudioBatchId(parse_uuid(&batch_id)?),
                runs: run_views,
                created_at: timestamp(created_at)?,
            });
        }
        Ok(StudioConversationView {
            conversation: summary,
            turns,
        })
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

fn insert_prepared_runs(
    transaction: &rusqlite::Transaction<'_>,
    batch_id: StudioBatchId,
    start_position: i64,
    runs: &[PreparedStudioRun],
    owner_device_id: &str,
    now: i64,
) -> Result<Vec<StoredStudioRun>, StudioStoreError> {
    let mut stored = Vec::with_capacity(runs.len());
    for (offset, prepared) in runs.iter().enumerate() {
        let run_id = StudioRunId::new();
        let attempt_id = StudioAttemptId::new();
        let idempotency_key = Uuid::new_v4().to_string();
        let model_json = serde_json::to_string(&prepared.model)
            .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
        let settings_json = serde_json::to_string(&prepared.request.controls)
            .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
        let request_json = serde_json::to_string(&prepared.request)
            .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
        let request_hash = format!("{:x}", Sha256::digest(request_json.as_bytes()));
        let quote = prepared.quote.clone().or_else(|| {
            prepared
                .model
                .estimate_cost(&prepared.request.controls, prepared.request.output_count)
        });
        let quote_json = quote
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
        let position = start_position + offset as i64;
        transaction.execute(
            "INSERT INTO studio_runs (id, batch_id, position, provider_id, model_id, operation, model_manifest_json, settings_json, owner_device_id, state, quote_json, output_count, display_aspect_width, display_aspect_height, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', ?10, ?11, ?12, ?13, ?14, ?14)",
            rusqlite::params![run_id.0.to_string(), batch_id.0.to_string(), position, prepared.request.provider_id.as_str(), prepared.request.model_id.as_str(), operation_name(prepared.request.operation), model_json, settings_json, owner_device_id, quote_json, prepared.request.output_count, prepared.request.display_aspect_ratio.0, prepared.request.display_aspect_ratio.1, now],
        )?;
        transaction.execute(
            "INSERT INTO studio_attempts (id, run_id, attempt_number, idempotency_key, request_json, request_wire_hash, state, provider_connection_id, created_at) VALUES (?1, ?2, 1, ?3, ?4, ?5, 'prepared', ?6, ?7)",
            rusqlite::params![attempt_id.0.to_string(), run_id.0.to_string(), idempotency_key, request_json, request_hash, prepared.request.provider_id.as_str(), now],
        )?;
        transaction.execute(
            "INSERT INTO studio_run_events (run_id, attempt_id, state, created_at) VALUES (?1, ?2, 'prepared', ?3)",
            rusqlite::params![run_id.0.to_string(), attempt_id.0.to_string(), now],
        )?;
        stored.push(StoredStudioRun {
            run_id,
            attempt_id,
            idempotency_key,
            request: prepared.request.clone(),
        });
    }
    Ok(stored)
}

fn recompute_batch(
    connection: &Connection,
    run_id: StudioRunId,
    now: i64,
) -> Result<(), rusqlite::Error> {
    connection.execute(
        "UPDATE studio_batches SET state = CASE WHEN EXISTS(SELECT 1 FROM studio_runs r WHERE r.batch_id = studio_batches.id AND r.state NOT IN ('succeeded','failed','cancelled')) THEN 'running' WHEN EXISTS(SELECT 1 FROM studio_runs r WHERE r.batch_id = studio_batches.id AND r.state = 'succeeded') THEN 'succeeded' ELSE 'failed' END, updated_at = ?2 WHERE id = (SELECT batch_id FROM studio_runs WHERE id = ?1)",
        rusqlite::params![run_id.0.to_string(), now],
    )?;
    Ok(())
}

fn operation_name(operation: zeron_studio::MediaOperation) -> &'static str {
    match operation {
        zeron_studio::MediaOperation::TextToImage => "text_to_image",
        zeron_studio::MediaOperation::ImageToImage => "image_to_image",
        zeron_studio::MediaOperation::ImageEdit => "image_edit",
        zeron_studio::MediaOperation::Upscale => "upscale",
        zeron_studio::MediaOperation::TextToVideo => "text_to_video",
        zeron_studio::MediaOperation::ImageToVideo => "image_to_video",
        zeron_studio::MediaOperation::ReferenceToVideo => "reference_to_video",
        zeron_studio::MediaOperation::VideoToVideo => "video_to_video",
    }
}

fn media_kind_name(kind: zeron_studio::MediaKind) -> &'static str {
    match kind {
        zeron_studio::MediaKind::Image => "image",
        zeron_studio::MediaKind::Video => "video",
    }
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    ARTIFACT_FORMATS
        .iter()
        .find_map(|(extension, supported)| (*supported == mime).then_some(*extension))
}

fn artifact_bytes_match_mime(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "video/mp4" => bytes.len() >= 12 && &bytes[4..8] == b"ftyp",
        _ => false,
    }
}

fn parse_uuid(value: &str) -> Result<Uuid, StudioStoreError> {
    Uuid::parse_str(value).map_err(|error| StudioStoreError::InvalidValue(error.to_string()))
}

fn timestamp(value: i64) -> Result<chrono::DateTime<chrono::Utc>, StudioStoreError> {
    chrono::DateTime::from_timestamp_millis(value).ok_or_else(|| {
        StudioStoreError::InvalidValue("timestamp is outside the supported range".into())
    })
}

fn parse_run_state(value: &str) -> Result<StudioRunState, StudioStoreError> {
    match value {
        "draft" => Ok(StudioRunState::Draft),
        "quoting" => Ok(StudioRunState::Quoting),
        "awaiting_confirmation" => Ok(StudioRunState::AwaitingConfirmation),
        "queued" => Ok(StudioRunState::Queued),
        "running" => Ok(StudioRunState::Running),
        "downloading" => Ok(StudioRunState::Downloading),
        "succeeded" => Ok(StudioRunState::Succeeded),
        "failed" => Ok(StudioRunState::Failed),
        "cancelling" => Ok(StudioRunState::Cancelling),
        "cancelled" => Ok(StudioRunState::Cancelled),
        _ => Err(StudioStoreError::InvalidValue(format!(
            "unknown run state {value}"
        ))),
    }
}

fn gallery_item_from_row(row: &rusqlite::Row<'_>) -> Result<StudioGalleryItem, rusqlite::Error> {
    use rusqlite::types::Type;
    let parse_uuid = |index, value: String| {
        Uuid::parse_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
        })
    };
    let model_json: String = row.get(11)?;
    let model: MediaModel = serde_json::from_str(&model_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(11, Type::Text, Box::new(error))
    })?;
    let created_at: i64 = row.get(7)?;
    Ok(StudioGalleryItem {
        id: StudioArtifactId(parse_uuid(0, row.get(0)?)?),
        output_position: row.get(1)?,
        media_kind: match row.get::<_, String>(2)?.as_str() {
            "image" => zeron_studio::MediaKind::Image,
            "video" => zeron_studio::MediaKind::Video,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        mime_type: row.get(3)?,
        size_bytes: row.get(4)?,
        width: row.get(5)?,
        height: row.get(6)?,
        created_at: chrono::DateTime::from_timestamp_millis(created_at)
            .ok_or(rusqlite::Error::InvalidQuery)?,
        conversation_id: StudioConversationId(parse_uuid(8, row.get(8)?)?),
        turn_id: StudioTurnId(parse_uuid(9, row.get(9)?)?),
        prompt: row.get(10)?,
        model_display_name: model.display_name,
    })
}

fn artifact_from_row(row: &rusqlite::Row<'_>) -> Result<StudioArtifactView, rusqlite::Error> {
    use rusqlite::types::Type;
    let id: String = row.get(0)?;
    let media_kind: String = row.get(2)?;
    let metadata: String = row.get(8)?;
    let created_at: i64 = row.get(9)?;
    Ok(StudioArtifactView {
        id: StudioArtifactId(Uuid::parse_str(&id).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
        })?),
        output_position: row.get(1)?,
        media_kind: match media_kind.as_str() {
            "image" => zeron_studio::MediaKind::Image,
            "video" => zeron_studio::MediaKind::Video,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        mime_type: row.get(3)?,
        size_bytes: row.get(4)?,
        width: row.get(5)?,
        height: row.get(6)?,
        duration_seconds: row.get(7)?,
        metadata: serde_json::from_str(&metadata).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(8, Type::Text, Box::new(error))
        })?,
        created_at: chrono::DateTime::from_timestamp_millis(created_at)
            .ok_or(rusqlite::Error::InvalidQuery)?,
    })
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

/// First-prompt fallback title: a handful of words, hard-capped so the
/// sidebar row stays short.
fn title_from_prompt(prompt: &str) -> Option<String> {
    let title: String = prompt
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(48)
        .collect();
    let title = title.trim().to_string();
    (!title.is_empty()).then_some(title)
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

    /// Read a bounded byte range using only the artifact's opaque ID.
    ///
    /// The extension and path are resolved inside the jail. Multiple files with the same artifact
    /// ID are rejected as corrupt rather than selecting one nondeterministically.
    pub fn read_chunk(
        &self,
        artifact_id: StudioArtifactId,
        offset: u64,
    ) -> Result<StudioArtifactChunk, StudioStoreError> {
        let (path, extension, mime_type) = self.locate(artifact_id)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StudioStoreError::InvalidArtifact);
        }
        let size = metadata.len();
        let start = offset.min(size);
        let next_offset = (start + ARTIFACT_READ_CHUNK_BYTES).min(size);
        let mut bytes = vec![0; (next_offset - start) as usize];
        let mut file = File::open(&path)?;
        file.seek(std::io::SeekFrom::Start(start))?;
        file.read_exact(&mut bytes)?;

        Ok(StudioArtifactChunk {
            artifact_id,
            file_name: format!("{}.{}", artifact_id.0, extension),
            mime_type: mime_type.to_owned(),
            data: BASE64.encode(bytes),
            next_offset,
            done: next_offset >= size,
        })
    }

    fn path_for(
        &self,
        artifact_id: StudioArtifactId,
        extension: &str,
    ) -> Result<PathBuf, StudioStoreError> {
        let extension = extension.to_ascii_lowercase();
        if !ARTIFACT_FORMATS
            .iter()
            .any(|(supported, _)| *supported == extension)
        {
            return Err(StudioStoreError::InvalidExtension);
        }
        Ok(self.root.join(format!("{}.{}", artifact_id.0, extension)))
    }

    fn locate(
        &self,
        artifact_id: StudioArtifactId,
    ) -> Result<(PathBuf, &'static str, &'static str), StudioStoreError> {
        let mut found = None;
        for &(extension, mime_type) in ARTIFACT_FORMATS {
            let path = self.root.join(format!("{}.{}", artifact_id.0, extension));
            match fs::symlink_metadata(&path) {
                Ok(_) if found.is_some() => return Err(StudioStoreError::InvalidArtifact),
                Ok(_) => found = Some((path, extension, mime_type)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        found.ok_or(StudioStoreError::ArtifactNotFound)
    }
}

fn sync_directory(path: &Path) -> Result<(), std::io::Error> {
    File::open(path)?.sync_all()
}
