//! Profile-scoped durable storage for Studio metadata and generated media.

use std::{
    collections::{BTreeMap, HashSet},
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
    AttachmentOrigin, ComposerAttachment, ComposerMediaKind, ImportStudioAssetChunk,
    ImportStudioAssetResponse, LEGACY_UNTITLED_STUDIO_TITLE, ListStudioModelsResponse,
    StudioArtifactChunk, StudioArtifactView, StudioConversationSummary, StudioConversationView,
    StudioGalleryItem, StudioModelRunSpec, StudioRunState, StudioRunView, StudioTurnView,
    UNTITLED_STUDIO_TITLE,
};
use zeron_studio::{
    GenerationInput, GenerationInputSource, GenerationRequest, InputRole, MediaKind, MediaModel,
    MediaOperation, MediaProvider, ProviderArtifact, ProviderId, Quote, ROLE_REFERENCE_VIDEO,
    RemoteJob, ResolvedInput, StudioArtifactId, StudioAssetId, StudioAttemptId, StudioBatchId,
    StudioConversationId, StudioRunId, StudioTurnId, SubmissionCapabilities, probe_media,
    sniff_media_mime, validate_inputs_against_bytes,
};

use crate::venice_import::{ImportReport, ImportedStudioHistory};

const DATABASE_FILE: &str = "studio.sqlite3";
const SCHEMA_VERSION: i64 = 4;
pub(crate) const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_IMPORT_B64_CHARS: usize = (MAX_IMPORT_BYTES as usize) / 3 * 4 + 8;
const IMPORT_STAGING_TTL: Duration = Duration::from_secs(10 * 60);
const ASSET_INPUT_REJECTED: &str =
    "studio asset inputs are only accepted for video roles and ImageEdit masks";
const MAX_CREATE_TURN_RUNS: usize = 16;
const MAX_TURN_RUNS: usize = 64;
pub(crate) const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const STUDIO_CATALOG_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const ARTIFACT_READ_CHUNK_BYTES: u64 = 192_000;
const CONVERSATION_SELECT: &str = "\
SELECT c.id, c.title, COUNT(t.id), c.created_at, c.updated_at, c.archived_at, c.forked_from_turn_id, \
EXISTS (\
    SELECT 1 \
    FROM studio_turns active_turn \
    JOIN studio_batches active_batch ON active_batch.turn_id = active_turn.id \
    JOIN studio_runs active_run ON active_run.batch_id = active_batch.id \
    WHERE active_turn.conversation_id = c.id \
      AND active_run.state NOT IN ('succeeded', 'failed', 'cancelled')\
), \
(\
    c.last_seen_at IS NOT NULL \
    AND EXISTS (\
        SELECT 1 \
        FROM studio_turns settled_turn \
        JOIN studio_batches settled_batch ON settled_batch.turn_id = settled_turn.id \
        JOIN studio_runs settled_run ON settled_run.batch_id = settled_batch.id \
        WHERE settled_turn.conversation_id = c.id \
          AND settled_run.state IN ('succeeded', 'failed', 'cancelled') \
          AND settled_run.updated_at > c.last_seen_at\
    )\
)";
const ARTIFACT_FORMATS: &[(&str, &str)] = &[
    ("webp", "image/webp"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("mp4", "video/mp4"),
    ("mov", "video/quicktime"),
    ("webm", "video/webm"),
    ("wav", "audio/wav"),
    ("mp3", "audio/mpeg"),
];

const SCHEMA_V1: &str = r#"
BEGIN IMMEDIATE;

CREATE TABLE IF NOT EXISTS studio_conversations (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    archived_at INTEGER,
    forked_from_turn_id TEXT REFERENCES studio_turns(id),
    last_seen_at INTEGER
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
    duration_seconds REAL CHECK (duration_seconds IS NULL OR duration_seconds >= 0.0),
    media_kind TEXT NOT NULL DEFAULT 'image' CHECK (media_kind IN ('image', 'video', 'audio')),
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
    thumbhash TEXT,
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

PRAGMA user_version = 4;
COMMIT;
"#;

const SCHEMA_V2: &str = r#"
BEGIN IMMEDIATE;
ALTER TABLE studio_artifacts ADD COLUMN thumbhash TEXT;
PRAGMA user_version = 2;
COMMIT;
"#;

const SCHEMA_V3: &str = r#"
BEGIN IMMEDIATE;
ALTER TABLE studio_conversations ADD COLUMN last_seen_at INTEGER;
UPDATE studio_conversations SET last_seen_at = updated_at WHERE last_seen_at IS NULL;
PRAGMA user_version = 3;
COMMIT;
"#;

const SCHEMA_V4: &str = r#"
BEGIN IMMEDIATE;
ALTER TABLE studio_assets ADD COLUMN duration_seconds REAL
    CHECK (duration_seconds IS NULL OR duration_seconds >= 0.0);
ALTER TABLE studio_assets ADD COLUMN media_kind TEXT NOT NULL DEFAULT 'image'
    CHECK (media_kind IN ('image', 'video', 'audio'));
PRAGMA user_version = 4;
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
    #[error("studio asset was not found")]
    AssetNotFound,
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

/// Outcome of [`StudioStore::complete_run`]. Internal `fail_run` is `Failed`,
/// not `Ok` — callers must not treat that as a published artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompleteRun {
    Published,
    Failed,
    AlreadyTerminal,
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

#[derive(Clone, Debug)]
pub struct StudioAttemptCheck {
    pub run: StoredStudioRun,
    pub attempt_state: String,
    pub run_state: String,
    pub remote_job: Option<RemoteJob>,
    pub request_wire_hash: String,
    pub conversation_id: StudioConversationId,
}

/// SQLite catalog rooted under one active profile.
pub struct StudioStore {
    database_path: PathBuf,
    connection: Mutex<Connection>,
    artifacts: ArtifactStore,
    changes: tokio::sync::watch::Sender<u64>,
    /// In-process pollers for a given attempt. Process death clears this, which
    /// is the only case startup resume should start a new 45-minute timer.
    video_polls: Mutex<HashSet<StudioAttemptId>>,
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
        } else {
            if version < 2 {
                connection.execute_batch(SCHEMA_V2)?;
            }
            if version < 3 {
                connection.execute_batch(SCHEMA_V3)?;
            }
            if version < 4 {
                migrate_studio_assets_v4(&connection)?;
            }
        }

        let (changes, _) = tokio::sync::watch::channel(0);
        Ok(Self {
            database_path,
            connection: Mutex::new(connection),
            artifacts: ArtifactStore::open(&studio_root, maximum_artifact_bytes)?,
            changes,
            video_polls: Mutex::new(HashSet::new()),
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
    ///
    /// Video attempts that already reached `queued`/`running` **with** a `remote_job_id`
    /// are left in place so a later resume pass can keep polling. Queued/running video
    /// without an id, and prepared/submitting video, still follow the image path.
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
                   AND r.state IN ('queued', 'running', 'downloading')
                   AND NOT (
                       r.operation IN ('text_to_video', 'image_to_video', 'reference_to_video', 'video_to_video')
                       AND a.state IN ('queued', 'running')
                       AND a.remote_job_id IS NOT NULL
                       AND TRIM(a.remote_job_id) != ''
                   )",
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

    /// Queued/running video attempts that already have a durable `remote_job_id`.
    /// Startup resumes these instead of marking them `submission_unknown`.
    pub fn resumable_video_attempts(&self) -> Result<Vec<StoredStudioRun>, StudioStoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT a.id, a.run_id, a.idempotency_key, a.request_json
             FROM studio_attempts a
             JOIN studio_runs r ON r.id = a.run_id
             WHERE a.state IN ('queued', 'running')
               AND r.state IN ('queued', 'running', 'downloading')
               AND r.operation IN ('text_to_video', 'image_to_video', 'reference_to_video', 'video_to_video')
               AND a.remote_job_id IS NOT NULL
               AND TRIM(a.remote_job_id) != ''
             ORDER BY a.created_at ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut attempts = Vec::new();
        for row in rows {
            let (attempt_id, run_id, idempotency_key, request_json) = row?;
            attempts.push(StoredStudioRun {
                run_id: StudioRunId(parse_uuid(&run_id)?),
                attempt_id: StudioAttemptId(parse_uuid(&attempt_id)?),
                idempotency_key,
                request: serde_json::from_str(&request_json)
                    .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?,
            });
        }
        Ok(attempts)
    }

    pub fn remote_job_for_attempt(
        &self,
        attempt_id: StudioAttemptId,
    ) -> Result<Option<RemoteJob>, StudioStoreError> {
        let connection = self.connection()?;
        let row = connection
            .query_row(
                "SELECT remote_job_id, response_metadata_json FROM studio_attempts WHERE id = ?1",
                [attempt_id.0.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::RunNotFound,
                other => other.into(),
            })?;
        let Some(id) = row.0.filter(|value| !value.trim().is_empty()) else {
            return Ok(None);
        };
        let metadata = match row.1.as_deref() {
            Some(json) => serde_json::from_str(json)
                .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?,
            None => serde_json::Value::Null,
        };
        Ok(Some(RemoteJob { id, metadata }))
    }

    pub fn latest_attempt_for_check(
        &self,
        run_id: StudioRunId,
    ) -> Result<StudioAttemptCheck, StudioStoreError> {
        let connection = self.connection()?;
        let row = connection
            .query_row(
                "SELECT a.id, a.idempotency_key, a.request_json, a.state, a.remote_job_id,
                        a.response_metadata_json, a.request_wire_hash, r.state, t.conversation_id
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
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::RunNotFound,
                other => other.into(),
            })?;
        let remote_job = match row.4.filter(|value| !value.trim().is_empty()) {
            Some(id) => {
                let metadata = match row.5.as_deref() {
                    Some(json) => serde_json::from_str(json)
                        .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?,
                    None => serde_json::Value::Null,
                };
                Some(RemoteJob { id, metadata })
            }
            None => None,
        };
        Ok(StudioAttemptCheck {
            run: StoredStudioRun {
                run_id,
                attempt_id: StudioAttemptId(parse_uuid(&row.0)?),
                idempotency_key: row.1,
                request: serde_json::from_str(&row.2)
                    .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?,
            },
            attempt_state: row.3,
            run_state: row.7,
            remote_job,
            request_wire_hash: row.6.unwrap_or_default(),
            conversation_id: StudioConversationId(parse_uuid(&row.8)?),
        })
    }

    /// Move a `submission_unknown` or `failed` attempt that already has a queue
    /// id back to `queued` so Check status can poll the same attempt.
    ///
    /// Returns `false` when another attempt is already active for the run
    /// (`studio_one_active_attempt_per_run`) or this row is no longer reopenable.
    pub fn reopen_unknown_for_poll(&self, run: &StoredStudioRun) -> Result<bool, StudioStoreError> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let other_active: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM studio_attempts
             WHERE run_id = ?1 AND id != ?2
               AND state IN ('prepared', 'submitting', 'submission_unknown', 'queued', 'running')",
            rusqlite::params![run.run_id.0.to_string(), run.attempt_id.0.to_string()],
            |row| row.get(0),
        )?;
        if other_active > 0 {
            return Ok(false);
        }
        let changed = transaction.execute(
            "UPDATE studio_attempts SET state = 'queued', error_json = NULL
             WHERE id = ?1 AND run_id = ?2
               AND state IN ('submission_unknown', 'failed')
               AND remote_job_id IS NOT NULL AND TRIM(remote_job_id) != ''",
            rusqlite::params![run.attempt_id.0.to_string(), run.run_id.0.to_string()],
        )?;
        if changed != 1 {
            return Ok(false);
        }
        transaction.execute(
            "UPDATE studio_runs SET state = 'queued', progress = NULL, error_json = NULL, updated_at = ?2 WHERE id = ?1",
            rusqlite::params![run.run_id.0.to_string(), now],
        )?;
        transaction.execute(
            "INSERT INTO studio_run_events (run_id, attempt_id, state, created_at) VALUES (?1, ?2, 'queued', ?3)",
            rusqlite::params![run.run_id.0.to_string(), run.attempt_id.0.to_string(), now],
        )?;
        transaction.commit()?;
        self.notify_change();
        Ok(true)
    }

    pub(crate) fn try_begin_video_poll(&self, attempt_id: StudioAttemptId) -> bool {
        self.video_polls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(attempt_id)
    }

    pub(crate) fn end_video_poll(&self, attempt_id: StudioAttemptId) {
        self.video_polls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&attempt_id);
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
            "INSERT INTO studio_conversations (id, title, created_at, updated_at, last_seen_at, forked_from_turn_id) VALUES (?1, ?2, ?3, ?3, ?3, ?4)",
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
        let mut statement = connection.prepare(&format!(
            "{CONVERSATION_SELECT} FROM studio_conversations c
             LEFT JOIN studio_turns t ON t.conversation_id = c.id
             WHERE (?1 OR c.archived_at IS NULL)
             GROUP BY c.id
             ORDER BY c.updated_at DESC, c.id DESC"
        ))?;
        let rows = statement.query_map([include_archived], conversation_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_gallery(&self) -> Result<Vec<StudioGalleryItem>, StudioStoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT a.id, a.output_position, a.media_kind, a.mime_type, a.size_bytes,
                    a.width, a.height, a.created_at,
                    t.conversation_id, t.id,
                    COALESCE(
                        NULLIF(
                            json_extract((
                                SELECT attempt.request_json
                                FROM studio_attempts attempt
                                WHERE attempt.run_id = r.id
                                ORDER BY attempt.attempt_number
                                LIMIT 1
                            ), '$.prompt'),
                            ''
                        ),
                        t.prompt
                    ),
                    r.model_manifest_json,
                    a.thumbhash,
                    (
                        SELECT artifact_id
                        FROM studio_run_inputs
                        WHERE run_id = r.id AND role = 'source' AND ordinal = 0
                        LIMIT 1
                    ),
                    a.duration_seconds
             FROM studio_artifacts a
             JOIN studio_runs r ON r.id = a.run_id
             JOIN studio_batches b ON b.id = r.batch_id
             JOIN studio_turns t ON t.id = b.turn_id
             WHERE a.deleted_at IS NULL AND a.media_kind IN ('image', 'video')
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

    pub fn mark_conversation_seen(
        &self,
        id: StudioConversationId,
    ) -> Result<StudioConversationSummary, StudioStoreError> {
        let now = chrono::Utc::now().timestamp_millis();
        let changed = self.connection()?.execute(
            "UPDATE studio_conversations SET last_seen_at = ?2
             WHERE id = ?1
               AND (
                    last_seen_at IS NULL
                    OR EXISTS (
                        SELECT 1
                        FROM studio_turns t
                        JOIN studio_batches b ON b.turn_id = t.id
                        JOIN studio_runs r ON r.batch_id = b.id
                        WHERE t.conversation_id = studio_conversations.id
                          AND r.state IN ('succeeded', 'failed', 'cancelled')
                          AND r.updated_at > studio_conversations.last_seen_at
                    )
               )",
            rusqlite::params![id.0.to_string(), now],
        )?;
        if changed == 0 {
            return self.conversation(id);
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
        if prompt.chars().count() > 32_000 {
            return Err(StudioStoreError::InvalidValue(
                "prompt must contain 1 to 32000 characters".into(),
            ));
        }
        if runs.is_empty() || runs.len() > MAX_CREATE_TURN_RUNS {
            return Err(StudioStoreError::InvalidValue(
                "a turn must contain 1 to 16 model runs".into(),
            ));
        }
        if prompt.is_empty()
            && !runs
                .iter()
                .all(|run| run.request.operation == MediaOperation::Upscale)
        {
            return Err(StudioStoreError::InvalidValue(
                "prompt must contain 1 to 32000 characters".into(),
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
                 SET title = CASE WHEN title IN (?4, ?5) THEN ?2 ELSE title END,
                     updated_at = ?3
                 WHERE id = ?1",
                rusqlite::params![
                    conversation_id.0.to_string(),
                    title,
                    now,
                    UNTITLED_STUDIO_TITLE,
                    LEGACY_UNTITLED_STUDIO_TITLE
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

    /// Persist already-finished studio history. Conversations whose ids already
    /// exist are skipped so the same dump can be applied more than once.
    pub fn import_completed_history(
        &self,
        history: &ImportedStudioHistory,
        owner_device_id: &str,
    ) -> Result<ImportReport, StudioStoreError> {
        if owner_device_id.trim().is_empty() {
            return Err(StudioStoreError::InvalidValue(
                "import owner device id is required".into(),
            ));
        }
        let mut report = ImportReport {
            missing_files: history.missing_files,
            ..ImportReport::default()
        };
        for conversation in &history.conversations {
            if self.conversation_exists(conversation.id)? {
                report.conversations_skipped += 1;
                continue;
            }
            let title = validate_title(&conversation.title)?.to_owned();
            let mut staged = Vec::new();
            for turn in &conversation.turns {
                if turn.prompt.trim().is_empty() || turn.prompt.chars().count() > 32_000 {
                    return Err(StudioStoreError::InvalidValue(
                        "imported prompt must contain 1 to 32000 characters".into(),
                    ));
                }
                if turn.runs.is_empty() || turn.runs.len() > MAX_TURN_RUNS {
                    return Err(StudioStoreError::InvalidValue(format!(
                        "an imported turn must contain 1 to {MAX_TURN_RUNS} model runs"
                    )));
                }
                for (run_index, run) in turn.runs.iter().enumerate() {
                    let mut published = Vec::new();
                    for artifact in &run.artifacts {
                        let bytes = fs::read(&artifact.path)?;
                        let mime_type = zeron_studio::sniff_media_mime(&bytes)
                            .map(str::to_owned)
                            .unwrap_or_else(|| artifact.mime_type.clone());
                        let extension = extension_for_mime(&mime_type).ok_or_else(|| {
                            StudioStoreError::InvalidValue(format!(
                                "unsupported imported artifact MIME {mime_type}"
                            ))
                        })?;
                        let path = self.artifacts.ensure(artifact.id, extension, &bytes)?;
                        let relative_path = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .ok_or(StudioStoreError::InvalidArtifact)?
                            .to_owned();
                        let preview = self.artifacts.persist_preview(artifact.id, &bytes);
                        published.push((
                            artifact.id,
                            relative_path,
                            mime_type,
                            bytes.len() as i64,
                            format!("{:x}", Sha256::digest(&bytes)),
                            artifact.width,
                            artifact.height,
                            artifact.created_at,
                            artifact.metadata.to_string(),
                            preview,
                        ));
                    }
                    staged.push((turn.id, run_index, published));
                }
            }

            let mut connection = self.connection()?;
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO studio_conversations (id, title, created_at, updated_at, last_seen_at, forked_from_turn_id) VALUES (?1, ?2, ?3, ?4, ?4, NULL)",
                rusqlite::params![
                    conversation.id.0.to_string(),
                    title,
                    conversation.created_at,
                    conversation.updated_at.max(conversation.created_at)
                ],
            )?;
            for (position, turn) in conversation.turns.iter().enumerate() {
                let batch_id = StudioBatchId::new();
                let turn_created = if turn.created_at > 0 {
                    turn.created_at
                } else {
                    conversation.created_at
                };
                transaction.execute(
                    "INSERT INTO studio_turns (id, conversation_id, position, prompt, source_turn_id, created_at) VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                    rusqlite::params![
                        turn.id.0.to_string(),
                        conversation.id.0.to_string(),
                        position as i64,
                        turn.prompt,
                        turn_created
                    ],
                )?;
                let batch_succeeded = turn.runs.iter().enumerate().any(|(run_position, run)| {
                    let published = staged
                        .iter()
                        .find(|(turn_id, staged_index, _)| {
                            *turn_id == turn.id && *staged_index == run_position
                        })
                        .map(|(_, _, published)| published.as_slice())
                        .unwrap_or(&[]);
                    run.succeeded && !published.is_empty()
                });
                transaction.execute(
                    "INSERT INTO studio_batches (id, turn_id, state, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                    rusqlite::params![
                        batch_id.0.to_string(),
                        turn.id.0.to_string(),
                        if batch_succeeded { "succeeded" } else { "failed" },
                        turn_created
                    ],
                )?;
                for (run_position, run) in turn.runs.iter().enumerate() {
                    let published = staged
                        .iter()
                        .find(|(turn_id, staged_index, _)| {
                            *turn_id == turn.id && *staged_index == run_position
                        })
                        .map(|(_, _, published)| published.as_slice())
                        .unwrap_or(&[]);
                    let succeeded = run.succeeded && !published.is_empty();
                    let run_id = StudioRunId::new();
                    let attempt_id = StudioAttemptId::new();
                    let model_json = serde_json::to_string(&run.model)
                        .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
                    let settings_json = serde_json::to_string(&run.request.controls)
                        .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
                    let request_json = serde_json::to_string(&run.request)
                        .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
                    let request_hash = format!("{:x}", Sha256::digest(request_json.as_bytes()));
                    let state = if succeeded { "succeeded" } else { "failed" };
                    let error_json = (!succeeded).then(|| {
                        serde_json::json!({ "message": "venice dump had no output file for this turn" })
                            .to_string()
                    });
                    transaction.execute(
                        "INSERT INTO studio_runs (id, batch_id, position, provider_id, model_id, operation, model_manifest_json, settings_json, owner_device_id, state, quote_json, progress, error_json, output_count, display_aspect_width, display_aspect_height, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12, ?13, ?14, ?15, ?16, ?16)",
                        rusqlite::params![
                            run_id.0.to_string(),
                            batch_id.0.to_string(),
                            run_position as i64,
                            run.request.provider_id.as_str(),
                            run.request.model_id.as_str(),
                            operation_name(run.request.operation),
                            model_json,
                            settings_json,
                            owner_device_id,
                            state,
                            succeeded.then_some(1.0),
                            error_json,
                            run.request.output_count.max(1),
                            run.request.display_aspect_ratio.0,
                            run.request.display_aspect_ratio.1,
                            turn_created
                        ],
                    )?;
                    transaction.execute(
                        "INSERT INTO studio_attempts (id, run_id, attempt_number, idempotency_key, request_json, request_wire_hash, state, provider_connection_id, created_at, submitted_at, completed_at, error_json) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?8, ?9)",
                        rusqlite::params![
                            attempt_id.0.to_string(),
                            run_id.0.to_string(),
                            Uuid::new_v4().to_string(),
                            request_json,
                            request_hash,
                            state,
                            run.request.provider_id.as_str(),
                            turn_created,
                            error_json
                        ],
                    )?;
                    transaction.execute(
                        "INSERT INTO studio_run_events (run_id, attempt_id, state, created_at) VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![
                            run_id.0.to_string(),
                            attempt_id.0.to_string(),
                            state,
                            turn_created
                        ],
                    )?;
                    for (position, published) in published.iter().enumerate() {
                        let (
                            artifact_id,
                            relative_path,
                            mime_type,
                            size_bytes,
                            hash,
                            width,
                            height,
                            created_at,
                            metadata,
                            preview,
                        ) = published;
                        let (preview_relative_path, thumbhash) = match preview {
                            Some((path, hash)) => (Some(path.as_str()), Some(hash.as_str())),
                            None => (None, None),
                        };
                        transaction.execute(
                            "INSERT INTO studio_artifacts (id, run_id, attempt_id, output_position, media_kind, relative_path, mime_type, size_bytes, content_hash, width, height, duration_seconds, metadata_json, preview_relative_path, thumbhash, created_at) VALUES (?1, ?2, ?3, ?4, 'image', ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12, ?13, ?14)",
                            rusqlite::params![
                                artifact_id.0.to_string(),
                                run_id.0.to_string(),
                                attempt_id.0.to_string(),
                                position as i64,
                                relative_path,
                                mime_type,
                                size_bytes,
                                hash,
                                width,
                                height,
                                metadata,
                                preview_relative_path,
                                thumbhash,
                                if *created_at > 0 { *created_at } else { turn_created }
                            ],
                        )?;
                        report.artifacts_imported += 1;
                    }
                    if !succeeded {
                        report.failed_turns += 1;
                    }
                }
                report.turns_imported += 1;
            }
            transaction.commit()?;
            report.conversations_imported += 1;
        }
        if report.conversations_imported > 0 {
            self.notify_change();
        }
        Ok(report)
    }

    fn conversation_exists(&self, id: StudioConversationId) -> Result<bool, StudioStoreError> {
        let exists = self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM studio_conversations WHERE id = ?1)",
            [id.0.to_string()],
            |row| row.get(0),
        )?;
        Ok(exists)
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
        let _ = self.artifacts.delete_preview(artifact_id);
        self.notify_change();
        Ok(())
    }

    /// Derive and persist a gallery preview if one is missing. Cheap when the
    /// JPEG and thumbhash already exist.
    pub fn ensure_preview(&self, artifact_id: StudioArtifactId) -> Result<(), StudioStoreError> {
        let (existing_path, existing_hash): (Option<String>, Option<String>) =
            match self.connection()?.query_row(
                "SELECT preview_relative_path, thumbhash FROM studio_artifacts
                 WHERE id = ?1 AND deleted_at IS NULL AND media_kind IN ('image', 'video')",
                [artifact_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ) {
                Ok(row) => row,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    return Err(StudioStoreError::ArtifactNotFound);
                }
                Err(error) => return Err(error.into()),
            };
        let expected = crate::studio_preview::preview_file_name(artifact_id);
        if existing_path.as_deref() == Some(expected.as_str())
            && existing_hash.is_some()
            && self.artifacts.preview_exists(artifact_id)
        {
            return Ok(());
        }
        let original = self.artifacts.read_all(artifact_id)?;
        let Some((relative_path, thumbhash)) =
            self.artifacts.persist_preview(artifact_id, &original)
        else {
            return Err(StudioStoreError::InvalidValue(
                "could not derive a studio preview from the original".into(),
            ));
        };
        self.connection()?.execute(
            "UPDATE studio_artifacts SET preview_relative_path = ?2, thumbhash = ?3 WHERE id = ?1",
            rusqlite::params![artifact_id.0.to_string(), relative_path, thumbhash],
        )?;
        Ok(())
    }

    pub fn read_preview_chunk(
        &self,
        artifact_id: StudioArtifactId,
        offset: u64,
    ) -> Result<StudioArtifactChunk, StudioStoreError> {
        self.ensure_preview(artifact_id)?;
        self.artifacts.read_preview_chunk(artifact_id, offset)
    }

    pub fn artifacts_missing_previews(&self) -> Result<Vec<StudioArtifactId>, StudioStoreError> {
        let connection = self.connection()?;
        let current = format!(
            "%.{}.{}",
            crate::studio_preview::GALLERY_THUMB_SHORT_EDGE,
            crate::studio_preview::PREVIEW_EXTENSION
        );
        let mut statement = connection.prepare(
            "SELECT id FROM studio_artifacts
             WHERE deleted_at IS NULL AND media_kind IN ('image', 'video')
               AND (
                 preview_relative_path IS NULL
                 OR thumbhash IS NULL
                 OR preview_relative_path NOT LIKE ?1
               )
             ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([current], |row| {
            let id: String = row.get(0)?;
            Uuid::parse_str(&id).map(StudioArtifactId).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn spawn_preview_backfill(store: Arc<Self>) {
        if let Err(error) = std::thread::Builder::new()
            .name("studio-preview-backfill".into())
            .spawn(move || {
                let ids = match store.artifacts_missing_previews() {
                    Ok(ids) => ids,
                    Err(error) => {
                        tracing::warn!(%error, "studio preview backfill could not list artifacts");
                        return;
                    }
                };
                let mut wrote = false;
                for id in ids {
                    match store.ensure_preview(id) {
                        Ok(()) => wrote = true,
                        Err(error) => {
                            tracing::debug!(
                                %error,
                                artifact = %id.0,
                                "studio preview backfill skipped"
                            );
                        }
                    }
                }
                if wrote {
                    store.notify_change();
                }
            })
        {
            tracing::warn!(%error, "studio preview backfill thread failed to start");
        }
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
                "SELECT a.id, a.request_json, a.state, r.provider_id, t.conversation_id, r.state
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
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::RunNotFound,
                other => other.into(),
            })?;
        if matches!(row.5.as_str(), "queued" | "running" | "downloading") {
            return Err(StudioStoreError::InvalidValue(
                "cannot retry a run that is still queued, running, or downloading".into(),
            ));
        }
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

    pub(crate) fn run_model(&self, run_id: StudioRunId) -> Result<MediaModel, StudioStoreError> {
        let json: String = self
            .connection()?
            .query_row(
                "SELECT model_manifest_json FROM studio_runs WHERE id = ?1",
                [run_id.0.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::RunNotFound,
                other => other.into(),
            })?;
        serde_json::from_str(&json)
            .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))
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

    /// First writer of `remote_job_id`. Persist provider metadata (`model`, `download_url`)
    /// and move the attempt + run to `queued`.
    ///
    /// The remote id is written even if the state CAS fails so a later
    /// `submission_unknown` still has a queue id to resume or retry against.
    pub fn mark_queued(
        &self,
        run: &StoredStudioRun,
        remote: &RemoteJob,
    ) -> Result<(), StudioStoreError> {
        let metadata = serde_json::to_string(&remote.metadata)
            .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
        let now = chrono::Utc::now().timestamp_millis();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let persisted = transaction.execute(
            "UPDATE studio_attempts
             SET remote_job_id = ?3,
                 response_metadata_json = ?4,
                 last_polled_at = ?5
             WHERE id = ?1 AND run_id = ?2",
            rusqlite::params![
                run.attempt_id.0.to_string(),
                run.run_id.0.to_string(),
                remote.id,
                metadata,
                now
            ],
        )?;
        if persisted != 1 {
            return Err(StudioStoreError::RunNotFound);
        }
        let queued = transaction.execute(
            "UPDATE studio_attempts SET state = 'queued'
             WHERE id = ?1 AND run_id = ?2 AND state IN ('submitting', 'queued')",
            rusqlite::params![run.attempt_id.0.to_string(), run.run_id.0.to_string()],
        )?;
        if queued != 1 {
            transaction.commit()?;
            self.notify_change();
            return Err(StudioStoreError::InvalidValue(
                "studio attempt could not be marked queued".into(),
            ));
        }
        transaction.execute(
            "UPDATE studio_runs SET state = 'queued', progress = NULL, updated_at = ?2 WHERE id = ?1",
            rusqlite::params![run.run_id.0.to_string(), now],
        )?;
        transaction.execute(
            "INSERT INTO studio_run_events (run_id, attempt_id, state, created_at) VALUES (?1, ?2, 'queued', ?3)",
            rusqlite::params![run.run_id.0.to_string(), run.attempt_id.0.to_string(), now],
        )?;
        transaction.commit()?;
        self.notify_change();
        Ok(())
    }

    pub fn mark_running(
        &self,
        run: &StoredStudioRun,
        progress: Option<f32>,
    ) -> Result<(), StudioStoreError> {
        self.mark_in_flight(run, "running", "running", progress)
    }

    pub fn mark_downloading(
        &self,
        run: &StoredStudioRun,
        progress: Option<f32>,
    ) -> Result<(), StudioStoreError> {
        // Attempt CHECK has no `downloading`; keep the attempt `running`.
        self.mark_in_flight(run, "running", "downloading", progress)
    }

    fn mark_in_flight(
        &self,
        run: &StoredStudioRun,
        attempt_state: &str,
        run_state: &str,
        progress: Option<f32>,
    ) -> Result<(), StudioStoreError> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE studio_attempts
             SET state = ?3,
                 last_polled_at = ?4
             WHERE id = ?1 AND run_id = ?2 AND state IN ('queued', 'running')",
            rusqlite::params![
                run.attempt_id.0.to_string(),
                run.run_id.0.to_string(),
                attempt_state,
                now
            ],
        )?;
        if changed != 1 {
            return Err(StudioStoreError::RunNotFound);
        }
        transaction.execute(
            "UPDATE studio_runs SET state = ?2, progress = ?3, updated_at = ?4 WHERE id = ?1",
            rusqlite::params![run.run_id.0.to_string(), run_state, progress, now],
        )?;
        transaction.execute(
            "INSERT INTO studio_run_events (run_id, attempt_id, state, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![run.run_id.0.to_string(), run.attempt_id.0.to_string(), run_state, now],
        )?;
        transaction.commit()?;
        self.notify_change();
        Ok(())
    }

    /// Sum probed durations of `reference_video` inputs. `None` when no clips are
    /// attached, or when any clip is missing a duration (cannot prove compliance).
    pub fn reference_video_total_duration(
        &self,
        request: &GenerationRequest,
    ) -> Result<Option<f64>, StudioStoreError> {
        let mut total = 0.0;
        let mut any = false;
        for input in &request.inputs {
            if input.role.as_str() != ROLE_REFERENCE_VIDEO {
                continue;
            }
            any = true;
            let duration = match &input.source {
                GenerationInputSource::Asset { asset_id } => self.asset_duration(*asset_id)?,
                GenerationInputSource::Artifact { artifact_id } => {
                    self.artifact_duration(*artifact_id)?
                }
            };
            let Some(duration) = duration.filter(|value| value.is_finite() && *value >= 0.0) else {
                return Ok(None);
            };
            total += duration;
        }
        Ok(any.then_some(total))
    }

    fn asset_duration(&self, asset_id: StudioAssetId) -> Result<Option<f64>, StudioStoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT duration_seconds FROM studio_assets WHERE id = ?1",
                [asset_id.0.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::AssetNotFound,
                other => other.into(),
            })
    }

    fn artifact_duration(
        &self,
        artifact_id: StudioArtifactId,
    ) -> Result<Option<f64>, StudioStoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT duration_seconds FROM studio_artifacts WHERE id = ?1 AND deleted_at IS NULL",
                [artifact_id.0.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::ArtifactNotFound,
                other => other.into(),
            })
    }

    pub fn complete_run(
        &self,
        run: &StoredStudioRun,
        artifacts: &[ProviderArtifact],
    ) -> Result<CompleteRun, StudioStoreError> {
        if artifacts.len() != run.request.output_count as usize {
            self.fail_run(
                run,
                &format!(
                    "provider returned {} artifacts; {} were requested",
                    artifacts.len(),
                    run.request.output_count
                ),
                false,
            )?;
            return Ok(CompleteRun::Failed);
        }
        let model = self.run_model(run.run_id)?;
        let mut published = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            let Some(mime_type) = model.accepted_output_mime(&artifact.bytes) else {
                self.fail_run(
                    run,
                    "provider artifact is not a supported format for this model",
                    false,
                )?;
                return Ok(CompleteRun::Failed);
            };
            let id = StudioArtifactId::new();
            let extension = extension_for_mime(&mime_type).ok_or_else(|| {
                StudioStoreError::InvalidValue(format!(
                    "unsupported provider artifact MIME {mime_type}"
                ))
            })?;
            match self.artifacts.publish(id, extension, &artifact.bytes) {
                Ok(path) => {
                    let preview = matches!(
                        artifact.media_kind,
                        zeron_studio::MediaKind::Image | zeron_studio::MediaKind::Video
                    )
                    .then(|| self.artifacts.persist_preview(id, &artifact.bytes))
                    .flatten();
                    published.push((id, path, extension, artifact, mime_type, preview));
                }
                Err(error) => {
                    for (id, _, extension, _, _, _) in &published {
                        let _ = self.artifacts.delete(*id, extension);
                        let _ = self.artifacts.delete_preview(*id);
                    }
                    return Err(error);
                }
            }
        }

        let result = (|| {
            let mut connection = self.connection()?;
            let transaction = connection.transaction()?;
            let now = chrono::Utc::now().timestamp_millis();
            let claimed = transaction.execute(
                "UPDATE studio_attempts SET state = 'succeeded', completed_at = ?2
                 WHERE id = ?1 AND state IN ('prepared', 'submitting', 'queued', 'running')",
                rusqlite::params![run.attempt_id.0.to_string(), now],
            )?;
            if claimed != 1 {
                return Ok(CompleteRun::AlreadyTerminal);
            }
            for (position, (id, path, _, artifact, mime_type, preview)) in
                published.iter().enumerate()
            {
                let relative_path = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or(StudioStoreError::InvalidArtifact)?;
                let hash = format!("{:x}", Sha256::digest(&artifact.bytes));
                let (preview_relative_path, thumbhash) = match preview {
                    Some((path, hash)) => (Some(path.as_str()), Some(hash.as_str())),
                    None => (None, None),
                };
                transaction.execute(
                    "INSERT INTO studio_artifacts (id, run_id, attempt_id, output_position, media_kind, relative_path, mime_type, size_bytes, content_hash, width, height, duration_seconds, metadata_json, preview_relative_path, thumbhash, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                    rusqlite::params![id.0.to_string(), run.run_id.0.to_string(), run.attempt_id.0.to_string(), position as i64, media_kind_name(artifact.media_kind), relative_path, mime_type, artifact.bytes.len() as i64, hash, artifact.width, artifact.height, artifact.duration_seconds, artifact.metadata.to_string(), preview_relative_path, thumbhash, now],
                )?;
            }
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
            Ok(CompleteRun::Published)
        })();
        match &result {
            Ok(CompleteRun::Published) => self.notify_change(),
            Ok(CompleteRun::AlreadyTerminal) | Err(_) => {
                for (id, _, extension, _, _, _) in &published {
                    let _ = self.artifacts.delete(*id, extension);
                    let _ = self.artifacts.delete_preview(*id);
                }
            }
            Ok(CompleteRun::Failed) => {}
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
        let claimed = transaction.execute(
            "UPDATE studio_attempts SET state = ?2, error_json = ?3, completed_at = CASE WHEN ?2 = 'failed' THEN ?4 ELSE NULL END
             WHERE id = ?1 AND state IN ('prepared', 'submitting', 'queued', 'running')",
            rusqlite::params![run.attempt_id.0.to_string(), attempt_state, error, now],
        )?;
        if claimed != 1 {
            return Ok(());
        }
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

    /// Stamp and verify artifact- and asset inputs.
    pub fn bind_generation_inputs(
        &self,
        request: &mut GenerationRequest,
    ) -> Result<(), StudioStoreError> {
        for input in &mut request.inputs {
            match &input.source {
                GenerationInputSource::Asset { asset_id } => {
                    if !asset_input_allowed(request.operation, input.role.as_str()) {
                        return Err(StudioStoreError::InvalidValue(ASSET_INPUT_REJECTED.into()));
                    }
                    let stored_hash = self.asset_input_hash(*asset_id)?;
                    if input.content_hash.is_empty() {
                        input.content_hash = stored_hash;
                    } else if input.content_hash != stored_hash {
                        return Err(StudioStoreError::InvalidValue(
                            "studio input content hash does not match the asset".into(),
                        ));
                    }
                }
                GenerationInputSource::Artifact { artifact_id } => {
                    let (kind, stored_hash, width, height) =
                        self.artifact_input_row(*artifact_id)?;
                    if !matches!(kind, MediaKind::Image | MediaKind::Video) {
                        return Err(StudioStoreError::InvalidValue(
                            "studio input is not a supported media type.".into(),
                        ));
                    }
                    if input.content_hash.is_empty() {
                        input.content_hash = stored_hash;
                    } else if input.content_hash != stored_hash {
                        return Err(StudioStoreError::InvalidValue(
                            "studio input content hash does not match the artifact".into(),
                        ));
                    }
                    if matches!(
                        request.operation,
                        MediaOperation::Upscale | MediaOperation::ImageEdit
                    ) && input.role.as_str() == "source"
                    {
                        if let (Some(width), Some(height)) = (width, height)
                            && width > 0
                            && height > 0
                        {
                            request.display_aspect_ratio = (width, height);
                        } else if let Ok(aspect) = self.artifact_run_display_aspect(*artifact_id) {
                            request.display_aspect_ratio = aspect;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn resolve_generation_inputs(
        &self,
        request: &GenerationRequest,
        model: &MediaModel,
    ) -> Result<Vec<ResolvedInput>, StudioStoreError> {
        let mut resolved = Vec::with_capacity(request.inputs.len());
        let mut probes = Vec::with_capacity(request.inputs.len());
        for input in &request.inputs {
            let (path, stored_hash) = match &input.source {
                GenerationInputSource::Asset { asset_id } => {
                    if !asset_input_allowed(request.operation, input.role.as_str()) {
                        return Err(StudioStoreError::InvalidValue(ASSET_INPUT_REJECTED.into()));
                    }
                    let (path, hash) = self.asset_input_file(*asset_id)?;
                    (path, hash)
                }
                GenerationInputSource::Artifact { artifact_id } => {
                    let (kind, stored_hash, _, _) = self.artifact_input_row(*artifact_id)?;
                    if !matches!(kind, MediaKind::Image | MediaKind::Video) {
                        return Err(StudioStoreError::InvalidValue(
                            "studio input is not a supported media type.".into(),
                        ));
                    }
                    let (path, _, _) = self.artifacts.locate(*artifact_id)?;
                    (path, stored_hash)
                }
            };
            let mut file = File::open(&path)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let hash = format!("{:x}", Sha256::digest(&bytes));
            if hash != stored_hash || hash != input.content_hash {
                return Err(StudioStoreError::InvalidValue(
                    "studio input content hash does not match the stored file".into(),
                ));
            }
            let mime_type = sniff_media_mime(&bytes).ok_or_else(|| {
                StudioStoreError::InvalidValue("studio input is not a supported media type.".into())
            })?;
            let probe = probe_media(&bytes, mime_type);
            resolved.push(ResolvedInput {
                role: input.role.clone(),
                ordinal: input.ordinal,
                path,
                mime_type: mime_type.to_owned(),
                content_hash: hash,
                size_bytes: bytes.len() as u64,
            });
            probes.push(probe);
        }
        validate_inputs_against_bytes(model, &request.inputs, &probes)
            .map_err(|error| StudioStoreError::InvalidValue(error.to_string()))?;
        Ok(resolved)
    }

    fn artifact_run_display_aspect(
        &self,
        artifact_id: StudioArtifactId,
    ) -> Result<(u32, u32), StudioStoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT r.display_aspect_width, r.display_aspect_height
                 FROM studio_artifacts a
                 JOIN studio_runs r ON r.id = a.run_id
                 WHERE a.id = ?1 AND a.deleted_at IS NULL",
                [artifact_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::ArtifactNotFound,
                other => other.into(),
            })
    }

    fn artifact_input_row(
        &self,
        artifact_id: StudioArtifactId,
    ) -> Result<(MediaKind, String, Option<u32>, Option<u32>), StudioStoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT media_kind, content_hash, width, height FROM studio_artifacts
                 WHERE id = ?1 AND deleted_at IS NULL",
                [artifact_id.0.to_string()],
                |row| {
                    let kind = match row.get::<_, String>(0)?.as_str() {
                        "image" => MediaKind::Image,
                        "video" => MediaKind::Video,
                        _ => {
                            return Err(rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(StudioStoreError::InvalidArtifact),
                            ));
                        }
                    };
                    Ok((kind, row.get(1)?, row.get(2)?, row.get(3)?))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::ArtifactNotFound,
                other => other.into(),
            })
    }

    pub fn turn_id_for_artifact(
        &self,
        artifact_id: StudioArtifactId,
    ) -> Result<StudioTurnId, StudioStoreError> {
        let connection = self.connection()?;
        let turn_id: String = connection
            .query_row(
                "SELECT t.id
                 FROM studio_artifacts a
                 JOIN studio_runs r ON r.id = a.run_id
                 JOIN studio_batches b ON b.id = r.batch_id
                 JOIN studio_turns t ON t.id = b.turn_id
                 WHERE a.id = ?1 AND a.deleted_at IS NULL",
                [artifact_id.0.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::ArtifactNotFound,
                other => other.into(),
            })?;
        Ok(StudioTurnId(parse_uuid(&turn_id)?))
    }

    /// Persist a mask (or other derived input) into the profile jail.
    pub fn publish_asset(
        &self,
        bytes: &[u8],
        mime_type: &str,
    ) -> Result<StudioAssetId, StudioStoreError> {
        let mime_type = sniff_media_mime(bytes).filter(|sniffed| *sniffed == mime_type);
        let mime_type = mime_type.ok_or_else(|| {
            StudioStoreError::InvalidValue("studio asset is not a supported media type.".into())
        })?;
        if bytes.len() as u64 > DEFAULT_MAX_ARTIFACT_BYTES {
            return Err(StudioStoreError::ArtifactTooLarge);
        }
        let extension = extension_for_mime(mime_type).ok_or(StudioStoreError::InvalidExtension)?;
        let id = StudioAssetId::new();
        self.insert_published_asset(id, bytes, mime_type, extension)?;
        Ok(id)
    }

    /// Stage or commit one ImportStudioAsset chunk. Idempotent on
    /// `(asset_id, content_hash)` after a completed import.
    pub fn import_asset_chunk(
        &self,
        asset_id: StudioAssetId,
        offset: u64,
        data: &[u8],
        last: bool,
        expected_hash: Option<&str>,
        mime_hint: Option<&str>,
    ) -> Result<ImportStudioAssetResponse, StudioStoreError> {
        self.artifacts.sweep_import_staging();
        if data.len() as u64 > MAX_IMPORT_BYTES {
            return Err(StudioStoreError::ArtifactTooLarge);
        }
        if last {
            let expected = expected_hash
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    StudioStoreError::InvalidValue(
                        "expectedHash is required when last is true".into(),
                    )
                })?;
            if let Some(existing) = self.asset_attachment(asset_id)? {
                if !existing.content_hash.eq_ignore_ascii_case(expected) {
                    return Err(StudioStoreError::InvalidValue(
                        "studio asset already exists with a different content hash".into(),
                    ));
                }
                self.artifacts.remove_import_staging(asset_id)?;
                return Ok(ImportStudioAssetResponse::Complete(existing));
            }
            let next_offset = self.accept_import_chunk(asset_id, offset, data)?;
            let commit = (|| {
                let bytes = self.artifacts.read_import_staging(asset_id)?;
                if bytes.len() as u64 != next_offset {
                    return Err(StudioStoreError::InvalidValue(
                        "import staging is incomplete".into(),
                    ));
                }
                let hash = format!("{:x}", Sha256::digest(&bytes));
                if !hash.eq_ignore_ascii_case(expected) {
                    return Err(StudioStoreError::InvalidValue(
                        "import content hash does not match expectedHash".into(),
                    ));
                }
                let sniffed = sniff_media_mime(&bytes).ok_or_else(|| {
                    StudioStoreError::InvalidValue(
                        "studio input is not a supported media type.".into(),
                    )
                })?;
                let mime_type = match mime_hint {
                    Some(hint) if hint == sniffed => sniffed,
                    _ => sniffed,
                };
                let kind = composer_kind_from_mime(mime_type).ok_or_else(|| {
                    StudioStoreError::InvalidValue(
                        "studio input is not a supported media type.".into(),
                    )
                })?;
                let extension =
                    extension_for_mime(mime_type).ok_or(StudioStoreError::InvalidExtension)?;
                self.insert_published_asset(asset_id, &bytes, mime_type, extension)?;
                let probe = probe_media(&bytes, mime_type);
                Ok(ComposerAttachment {
                    id: asset_id,
                    kind,
                    pending: false,
                    origin: AttachmentOrigin::Asset,
                    mime_type: mime_type.to_owned(),
                    byte_size: bytes.len() as u64,
                    width: probe.width,
                    height: probe.height,
                    duration_seconds: probe.duration_seconds,
                    content_hash: hash,
                    role_hint: None,
                })
            })();
            match commit {
                Ok(attachment) => {
                    self.artifacts.remove_import_staging(asset_id)?;
                    Ok(ImportStudioAssetResponse::Complete(attachment))
                }
                Err(error) => {
                    let keep_staging = matches!(
                        &error,
                        StudioStoreError::InvalidValue(message)
                            if message.contains("content hash does not match expectedHash")
                    );
                    if !keep_staging {
                        self.artifacts.remove_import_staging(asset_id)?;
                    }
                    Err(error)
                }
            }
        } else if let Some(existing) = self.asset_attachment(asset_id)? {
            self.artifacts.remove_import_staging(asset_id)?;
            Ok(ImportStudioAssetResponse::Continue(
                ImportStudioAssetChunk {
                    asset_id,
                    next_offset: existing.byte_size,
                },
            ))
        } else {
            let next_offset = self.accept_import_chunk(asset_id, offset, data)?;
            Ok(ImportStudioAssetResponse::Continue(
                ImportStudioAssetChunk {
                    asset_id,
                    next_offset,
                },
            ))
        }
    }

    fn accept_import_chunk(
        &self,
        asset_id: StudioAssetId,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, StudioStoreError> {
        let (last_offset, next_offset) = self.artifacts.import_staging_offsets(asset_id)?;
        let write_at = if offset == next_offset {
            next_offset
        } else if last_offset.is_some_and(|last| last == offset) {
            offset
        } else {
            return Err(StudioStoreError::InvalidValue(
                "import offset must equal nextOffset".into(),
            ));
        };
        if next_offset == 0 && last_offset.is_none() && offset != 0 {
            return Err(StudioStoreError::InvalidValue(
                "import offset must equal nextOffset".into(),
            ));
        }
        let assembled = write_at.saturating_add(data.len() as u64);
        if assembled > MAX_IMPORT_BYTES {
            return Err(StudioStoreError::ArtifactTooLarge);
        }
        self.artifacts
            .write_import_chunk(asset_id, write_at, data)?;
        Ok(assembled)
    }

    fn insert_published_asset(
        &self,
        id: StudioAssetId,
        bytes: &[u8],
        mime_type: &str,
        extension: &str,
    ) -> Result<(), StudioStoreError> {
        let relative_path = format!("inputs/{}.{}", id.0, extension);
        self.artifacts.ensure_input(id, extension, bytes)?;
        let hash = format!("{:x}", Sha256::digest(bytes));
        let probe = probe_media(bytes, mime_type);
        let media_kind = match composer_kind_from_mime(mime_type) {
            Some(ComposerMediaKind::Image) => "image",
            Some(ComposerMediaKind::Video) => "video",
            Some(ComposerMediaKind::Audio) => "audio",
            None => {
                return Err(StudioStoreError::InvalidValue(
                    "studio input is not a supported media type.".into(),
                ));
            }
        };
        let now = chrono::Utc::now().timestamp_millis();
        self.connection()?.execute(
            "INSERT INTO studio_assets
             (id, relative_path, mime_type, size_bytes, content_hash, width, height, duration_seconds, media_kind, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                id.0.to_string(),
                relative_path,
                mime_type,
                bytes.len() as i64,
                hash,
                probe.width,
                probe.height,
                probe.duration_seconds,
                media_kind,
                now
            ],
        )?;
        Ok(())
    }

    fn asset_attachment(
        &self,
        asset_id: StudioAssetId,
    ) -> Result<Option<ComposerAttachment>, StudioStoreError> {
        let connection = self.connection()?;
        let row = connection.query_row(
            "SELECT mime_type, size_bytes, content_hash, width, height, duration_seconds, media_kind
             FROM studio_assets WHERE id = ?1",
            [asset_id.0.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<u32>>(3)?,
                    row.get::<_, Option<u32>>(4)?,
                    row.get::<_, Option<f64>>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        );
        match row {
            Ok((
                mime_type,
                size_bytes,
                content_hash,
                width,
                height,
                duration_seconds,
                media_kind,
            )) => {
                let kind = match media_kind.as_str() {
                    "image" => ComposerMediaKind::Image,
                    "video" => ComposerMediaKind::Video,
                    "audio" => ComposerMediaKind::Audio,
                    _ => {
                        return Err(StudioStoreError::InvalidValue(
                            "studio input is not a supported media type.".into(),
                        ));
                    }
                };
                Ok(Some(ComposerAttachment {
                    id: asset_id,
                    kind,
                    pending: false,
                    origin: AttachmentOrigin::Asset,
                    mime_type,
                    byte_size: size_bytes as u64,
                    width,
                    height,
                    duration_seconds,
                    content_hash,
                    role_hint: None,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn asset_input_hash(&self, asset_id: StudioAssetId) -> Result<String, StudioStoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT content_hash FROM studio_assets WHERE id = ?1",
                [asset_id.0.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::AssetNotFound,
                other => other.into(),
            })
    }

    fn asset_input_file(
        &self,
        asset_id: StudioAssetId,
    ) -> Result<(PathBuf, String), StudioStoreError> {
        let connection = self.connection()?;
        let (relative_path, hash): (String, String) = connection
            .query_row(
                "SELECT relative_path, content_hash FROM studio_assets WHERE id = ?1",
                [asset_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StudioStoreError::AssetNotFound,
                other => other.into(),
            })?;
        let path = self.artifacts.input_path_from_relative(&relative_path)?;
        Ok((path, hash))
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
                    "SELECT id, output_position, media_kind, mime_type, size_bytes, width, height, duration_seconds, metadata_json, created_at, thumbhash, content_hash FROM studio_artifacts WHERE run_id = ?1 AND deleted_at IS NULL ORDER BY output_position",
                )?;
                let artifacts = artifacts_statement
                    .query_map([run_id.clone()], artifact_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                let mut inputs_statement = connection.prepare(
                    "SELECT role, ordinal, asset_id, artifact_id, content_hash FROM studio_run_inputs WHERE run_id = ?1 ORDER BY ordinal, role",
                )?;
                let inputs = inputs_statement
                    .query_map([run_id.clone()], run_input_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(artifacts_statement);
                drop(inputs_statement);
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
                let run_prompt = connection
                    .query_row(
                        "SELECT request_json FROM studio_attempts
                         WHERE run_id = ?1
                         ORDER BY attempt_number
                         LIMIT 1",
                        [run_id.clone()],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
                    .and_then(|json| serde_json::from_str::<GenerationRequest>(&json).ok())
                    .map(|request| request.prompt)
                    .filter(|prompt| !prompt.is_empty());
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
                    prompt: run_prompt,
                    inputs,
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
                &format!(
                    "{CONVERSATION_SELECT} FROM studio_conversations c
                     LEFT JOIN studio_turns t ON t.conversation_id = c.id
                     WHERE c.id = ?1
                     GROUP BY c.id"
                ),
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
        for input in &prepared.request.inputs {
            let (asset_id, artifact_id) = match &input.source {
                GenerationInputSource::Asset { asset_id } => (Some(asset_id.0.to_string()), None),
                GenerationInputSource::Artifact { artifact_id } => {
                    (None, Some(artifact_id.0.to_string()))
                }
            };
            transaction.execute(
                "INSERT INTO studio_run_inputs (run_id, role, ordinal, asset_id, artifact_id, content_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    run_id.0.to_string(),
                    input.role.as_str(),
                    input.ordinal as i64,
                    asset_id,
                    artifact_id,
                    input.content_hash
                ],
            )?;
        }
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

fn asset_input_allowed(operation: MediaOperation, role: &str) -> bool {
    match operation {
        MediaOperation::TextToVideo
        | MediaOperation::ImageToVideo
        | MediaOperation::ReferenceToVideo
        | MediaOperation::VideoToVideo => true,
        MediaOperation::ImageEdit => role == "mask",
        _ => false,
    }
}

fn composer_kind_from_mime(mime: &str) -> Option<ComposerMediaKind> {
    if mime.starts_with("image/") {
        Some(ComposerMediaKind::Image)
    } else if mime.starts_with("video/") {
        Some(ComposerMediaKind::Video)
    } else if mime.starts_with("audio/") {
        Some(ComposerMediaKind::Audio)
    } else {
        None
    }
}

fn refuse_symlink(path: &Path) -> Result<(), StudioStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StudioStoreError::InvalidArtifact),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn open_jail_file(path: &Path) -> Result<File, StudioStoreError> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            StudioStoreError::InvalidArtifact
        } else {
            error.into()
        }
    })
}

fn write_jail_file(path: &Path, bytes: &[u8]) -> Result<(), StudioStoreError> {
    let mut file = open_jail_file(path)?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn migrate_studio_assets_v4(connection: &Connection) -> Result<(), StudioStoreError> {
    let has_assets: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'studio_assets'",
        [],
        |row| row.get(0),
    )?;
    if has_assets > 0 {
        connection.execute_batch(SCHEMA_V4)?;
    } else {
        connection.pragma_update(None, "user_version", 4)?;
    }
    Ok(())
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
        thumbhash: row.get(12)?,
        source_artifact_id: row
            .get::<_, Option<String>>(13)?
            .map(|value| parse_uuid(13, value))
            .transpose()?
            .map(StudioArtifactId),
        duration_seconds: row.get(14)?,
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
        thumbhash: row.get(10)?,
        content_hash: row.get(11)?,
    })
}

fn run_input_from_row(row: &rusqlite::Row<'_>) -> Result<GenerationInput, rusqlite::Error> {
    use rusqlite::types::Type;
    let parse_uuid = |index, value: String| {
        Uuid::parse_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
        })
    };
    let role: String = row.get(0)?;
    let ordinal: u32 = row.get(1)?;
    let asset_id: Option<String> = row.get(2)?;
    let artifact_id: Option<String> = row.get(3)?;
    let source = match (asset_id, artifact_id) {
        (Some(asset_id), None) => GenerationInputSource::Asset {
            asset_id: zeron_studio::StudioAssetId(parse_uuid(2, asset_id)?),
        },
        (None, Some(artifact_id)) => GenerationInputSource::Artifact {
            artifact_id: StudioArtifactId(parse_uuid(3, artifact_id)?),
        },
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(GenerationInput {
        role: InputRole::new(role),
        ordinal,
        source,
        content_hash: row.get(4)?,
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
        creating: row.get(7)?,
        done: row.get(8)?,
    })
}

/// ID-addressed storage. Callers never provide a path or filename.
pub struct ArtifactStore {
    root: PathBuf,
    preview_root: PathBuf,
    inputs_root: PathBuf,
    maximum_artifact_bytes: u64,
}

impl ArtifactStore {
    fn open(studio_root: &Path, maximum_artifact_bytes: u64) -> Result<Self, StudioStoreError> {
        let root = studio_root.join("artifacts");
        let preview_root = studio_root.join("previews");
        let inputs_root = studio_root.join("inputs");
        fs::create_dir_all(&root)?;
        fs::create_dir_all(&preview_root)?;
        fs::create_dir_all(&inputs_root)?;
        if fs::symlink_metadata(&root)?.file_type().is_symlink()
            || fs::symlink_metadata(&preview_root)?
                .file_type()
                .is_symlink()
            || fs::symlink_metadata(&inputs_root)?.file_type().is_symlink()
        {
            return Err(StudioStoreError::InvalidArtifact);
        }
        let store = Self {
            root,
            preview_root,
            inputs_root,
            maximum_artifact_bytes,
        };
        store.sweep_import_staging();
        Ok(store)
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

    fn ensure(
        &self,
        artifact_id: StudioArtifactId,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, StudioStoreError> {
        match self.publish(artifact_id, extension, bytes) {
            Ok(path) => Ok(path),
            Err(StudioStoreError::ArtifactExists) => self.path_for(artifact_id, extension),
            Err(error) => Err(error),
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

    fn ensure_input(
        &self,
        asset_id: StudioAssetId,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, StudioStoreError> {
        if bytes.len() as u64 > self.maximum_artifact_bytes {
            return Err(StudioStoreError::ArtifactTooLarge);
        }
        if !ARTIFACT_FORMATS
            .iter()
            .any(|(supported, _)| *supported == extension)
        {
            return Err(StudioStoreError::InvalidExtension);
        }
        let destination = self
            .inputs_root
            .join(format!("{}.{}", asset_id.0, extension));
        if let Ok(metadata) = fs::symlink_metadata(&destination) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(StudioStoreError::InvalidArtifact);
            }
            let existing = fs::read(&destination)?;
            if existing == bytes {
                return Ok(destination);
            }
            fs::remove_file(&destination)?;
        }
        self.write_input_destination(&destination, bytes)
    }

    fn write_input_destination(
        &self,
        destination: &Path,
        bytes: &[u8],
    ) -> Result<PathBuf, StudioStoreError> {
        let temporary = self.inputs_root.join(format!(
            ".{}.tmp-{}-{}",
            destination
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("input"),
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
                sync_directory(&self.inputs_root)?;
                Ok(destination.to_path_buf())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(StudioStoreError::ArtifactExists)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn import_tmp_root(&self) -> PathBuf {
        self.inputs_root.join("tmp")
    }

    fn import_staging_dir(&self, asset_id: StudioAssetId) -> PathBuf {
        self.import_tmp_root().join(asset_id.0.to_string())
    }

    fn sweep_import_staging(&self) {
        let tmp = self.import_tmp_root();
        match fs::symlink_metadata(&tmp) {
            Err(_) => return,
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let _ = fs::remove_file(&tmp);
                return;
            }
            Ok(_) => {}
        }
        let Ok(entries) = fs::read_dir(&tmp) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if refuse_symlink(&path).is_err() {
                let _ = fs::remove_dir_all(&path);
                continue;
            }
            let newest = fs::read_dir(&path)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|file| file.metadata().ok()?.modified().ok())
                .max();
            let expired = match newest {
                Some(at) => at
                    .elapsed()
                    .map(|age| age > IMPORT_STAGING_TTL)
                    .unwrap_or(true),
                None => true,
            };
            if expired {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }

    fn import_staging_offsets(
        &self,
        asset_id: StudioAssetId,
    ) -> Result<(Option<u64>, u64), StudioStoreError> {
        let dir = self.import_staging_dir(asset_id);
        let data_path = dir.join("data");
        let last_path = dir.join("last");
        match fs::symlink_metadata(&dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, 0)),
            Err(error) => return Err(error.into()),
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(StudioStoreError::InvalidArtifact);
            }
            Ok(_) => {}
        }
        refuse_symlink(&data_path)?;
        refuse_symlink(&last_path)?;
        let next_offset = match fs::metadata(&data_path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        let last_offset = match fs::read_to_string(&last_path) {
            Ok(value) => Some(value.trim().parse::<u64>().map_err(|error| {
                StudioStoreError::InvalidValue(format!("invalid import staging offset: {error}"))
            })?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok((last_offset, next_offset))
    }

    fn write_import_chunk(
        &self,
        asset_id: StudioAssetId,
        offset: u64,
        data: &[u8],
    ) -> Result<(), StudioStoreError> {
        self.sweep_import_staging();
        let tmp = self.import_tmp_root();
        refuse_symlink(&tmp)?;
        fs::create_dir_all(&tmp)?;
        refuse_symlink(&tmp)?;
        let dir = self.import_staging_dir(asset_id);
        refuse_symlink(&dir)?;
        fs::create_dir_all(&dir)?;
        refuse_symlink(&dir)?;
        let data_path = dir.join("data");
        let last_path = dir.join("last");
        refuse_symlink(&data_path)?;
        refuse_symlink(&last_path)?;
        write_jail_file(&last_path, offset.to_string().as_bytes())?;
        let mut file = open_jail_file(&data_path)?;
        file.set_len(offset)?;
        file.seek(std::io::SeekFrom::Start(offset))?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok(())
    }

    fn read_import_staging(&self, asset_id: StudioAssetId) -> Result<Vec<u8>, StudioStoreError> {
        let path = self.import_staging_dir(asset_id).join("data");
        refuse_symlink(&path)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn remove_import_staging(&self, asset_id: StudioAssetId) -> Result<(), StudioStoreError> {
        let dir = self.import_staging_dir(asset_id);
        match fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Err(StudioStoreError::InvalidArtifact)
            }
            Ok(_) => fs::remove_dir_all(&dir).map_err(Into::into),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn input_path_from_relative(&self, relative_path: &str) -> Result<PathBuf, StudioStoreError> {
        let Some(name) = relative_path.strip_prefix("inputs/") else {
            return Err(StudioStoreError::InvalidArtifact);
        };
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(StudioStoreError::InvalidArtifact);
        }
        let path = self.inputs_root.join(name);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                StudioStoreError::AssetNotFound
            } else {
                error.into()
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StudioStoreError::InvalidArtifact);
        }
        Ok(path)
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

    fn preview_path(&self, artifact_id: StudioArtifactId) -> PathBuf {
        self.preview_root
            .join(crate::studio_preview::preview_file_name(artifact_id))
    }

    pub fn preview_exists(&self, artifact_id: StudioArtifactId) -> bool {
        let path = self.preview_path(artifact_id);
        fs::symlink_metadata(&path)
            .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
    }

    pub fn persist_preview(
        &self,
        artifact_id: StudioArtifactId,
        original: &[u8],
    ) -> Option<(String, String)> {
        let preview = crate::studio_preview::derive_preview(original).ok()?;
        let path = self.publish_preview(artifact_id, &preview.bytes).ok()?;
        let name = path.file_name()?.to_str()?.to_owned();
        Some((name, preview.thumbhash))
    }

    fn publish_preview(
        &self,
        artifact_id: StudioArtifactId,
        bytes: &[u8],
    ) -> Result<PathBuf, StudioStoreError> {
        let destination = self.preview_path(artifact_id);
        if let Ok(metadata) = fs::symlink_metadata(&destination) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(StudioStoreError::InvalidArtifact);
            }
            return Ok(destination);
        }
        let temporary = self.preview_root.join(format!(
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
                sync_directory(&self.preview_root)?;
                Ok(destination)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(destination),
            Err(error) => Err(error.into()),
        }
    }

    pub fn delete_preview(&self, artifact_id: StudioArtifactId) -> Result<(), StudioStoreError> {
        let path = self.preview_path(artifact_id);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(StudioStoreError::InvalidArtifact)
            }
            Ok(_) => {
                fs::remove_file(path)?;
                sync_directory(&self.preview_root)?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn read_all(&self, artifact_id: StudioArtifactId) -> Result<Vec<u8>, StudioStoreError> {
        let (path, _, _) = self.locate(artifact_id)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StudioStoreError::InvalidArtifact);
        }
        Ok(fs::read(path)?)
    }

    pub fn read_preview_chunk(
        &self,
        artifact_id: StudioArtifactId,
        offset: u64,
    ) -> Result<StudioArtifactChunk, StudioStoreError> {
        let path = self.preview_path(artifact_id);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                StudioStoreError::ArtifactNotFound
            } else {
                error.into()
            }
        })?;
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
            file_name: format!(
                "{}.{}",
                artifact_id.0,
                crate::studio_preview::PREVIEW_EXTENSION
            ),
            mime_type: crate::studio_preview::PREVIEW_MIME.to_owned(),
            data: BASE64.encode(bytes),
            next_offset,
            done: next_offset >= size,
        })
    }
}

fn sync_directory(path: &Path) -> Result<(), std::io::Error> {
    File::open(path)?.sync_all()
}
