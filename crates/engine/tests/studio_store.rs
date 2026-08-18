use std::{collections::BTreeMap, io::Read, sync::Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use zeron_engine::studio::PreparedStudioRun;
use zeron_engine::{
    EngineCore, EngineProfile, HarnessId, StudioCredentialError, StudioCredentials,
    StudioProviderRegistry, StudioSecretBackend, StudioStore, default_registry,
};
use zeron_proto::{
    ComposerMediaKind, ComposerMode, ComposerSnapshot, ImportStudioAssetResponse,
    ListStudioArtifactsResponse, ListStudioConversationsResponse, ListStudioModelsResponse,
    ListStudioProvidersResponse, ProviderValidationState, STUDIO_VALIDATION_CODE, SelectedModelRef,
    StudioArtifactChunk, StudioConversationSummary, StudioConversationView,
    StudioProviderConnection, StudioRunState, StudioValidationError,
};
use zeron_rpc::{RpcError, memory_client, methods};
use zeron_studio::{
    AccountBalance, AdapterFamily, ComposerPhase, ConflictCode, ControlKind, ControlValue,
    FakeMediaProvider, FakeSubmissionMode, GenerationInput, GenerationInputSource,
    GenerationRequest, InputConstraint, MediaKind, MediaModel, MediaOperation, MimeConstraint,
    ModelControl, PricingMetadata, PricingUnit, ProviderArtifact, ProviderError, ProviderErrorKind,
    ProviderId, Quote, QuoteSource, Secret, StudioArtifactId, StudioAssetId, VideoModelMeta,
};

#[derive(Default)]
struct MemorySecrets(Mutex<BTreeMap<ProviderId, String>>);

#[async_trait]
impl StudioSecretBackend for MemorySecrets {
    async fn set(
        &self,
        provider_id: &ProviderId,
        secret: &Secret,
    ) -> Result<(), StudioCredentialError> {
        self.0
            .lock()
            .unwrap()
            .insert(provider_id.clone(), secret.expose().to_owned());
        Ok(())
    }

    async fn get(&self, provider_id: &ProviderId) -> Result<Secret, StudioCredentialError> {
        self.0
            .lock()
            .unwrap()
            .get(provider_id)
            .cloned()
            .map(Secret::new)
            .ok_or(StudioCredentialError::NotConfigured)
    }

    async fn remove(&self, provider_id: &ProviderId) -> Result<(), StudioCredentialError> {
        self.0.lock().unwrap().remove(provider_id);
        Ok(())
    }
}

fn image_model(provider_id: &str) -> MediaModel {
    MediaModel {
        provider_id: provider_id.into(),
        id: "image-model".into(),
        display_name: "Image model".into(),
        description: None,
        operation: MediaOperation::TextToImage,
        output_kind: MediaKind::Image,
        output_mime_types: vec!["image/png".into()],
        input_constraints: Vec::new(),
        prompt_maximum_chars: Some(1_000),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 4,
        controls: Vec::new(),
        pricing: None,
        features: Vec::new(),
        video: zeron_studio::VideoModelMeta::default(),
        manifest_version: "fixture-v1".into(),
        fetched_at: chrono::Utc::now(),
    }
}

fn seedance_t2v_model(provider_id: &str) -> MediaModel {
    MediaModel {
        provider_id: provider_id.into(),
        id: "seedance-t2v".into(),
        display_name: "Seedance T2V".into(),
        description: None,
        operation: MediaOperation::TextToVideo,
        output_kind: MediaKind::Video,
        output_mime_types: vec!["video/mp4".into()],
        input_constraints: Vec::new(),
        prompt_maximum_chars: Some(2_500),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 1,
        controls: vec![ModelControl {
            id: "duration".into(),
            label: "Duration".into(),
            description: None,
            kind: ControlKind::Duration,
            required: true,
            default: None,
            minimum: None,
            maximum: None,
            step: None,
            choices: vec![zeron_studio::ControlChoice {
                value: ControlValue::DurationSeconds { value: 6.0 },
                label: "6s".into(),
            }],
            visible_when: Vec::new(),
        }],
        pricing: None,
        features: Vec::new(),
        video: VideoModelMeta {
            adapter_family: AdapterFamily::Seedance,
            generate_audio: zeron_studio::AudioCapability::Configurable { default: true },
            ..VideoModelMeta::default()
        },
        manifest_version: "fixture-v1".into(),
        fetched_at: chrono::Utc::now(),
    }
}

fn hidden_kling_model(provider_id: &str) -> MediaModel {
    MediaModel {
        provider_id: provider_id.into(),
        id: "kling-o3-pro-reference-to-video".into(),
        display_name: "Kling O3".into(),
        description: None,
        operation: MediaOperation::ReferenceToVideo,
        output_kind: MediaKind::Video,
        output_mime_types: vec!["video/mp4".into()],
        input_constraints: Vec::new(),
        prompt_maximum_chars: Some(2_500),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 1,
        controls: Vec::new(),
        pricing: None,
        features: Vec::new(),
        video: VideoModelMeta::default(),
        manifest_version: "fixture-v1".into(),
        fetched_at: chrono::Utc::now(),
    }
}

fn priced_image_model(provider_id: &str, amount: f64) -> MediaModel {
    let mut model = image_model(provider_id);
    model.pricing = Some(PricingMetadata {
        currency: "USD".into(),
        unit: PricingUnit::PerOutput,
        unit_label: String::new(),
        amount: Some(amount),
        entries: Vec::new(),
        detail: None,
    });
    model
}

#[test]
fn studio_catalog_is_profile_scoped_and_migrated() {
    let root = tempdir().unwrap();
    let first = StudioStore::open(&root.path().join("profile-a"), 1024).unwrap();
    let second = StudioStore::open(&root.path().join("profile-b"), 1024).unwrap();

    assert_ne!(first.database_path(), second.database_path());
    assert!(
        first
            .database_path()
            .ends_with("profile-a/studio/studio.sqlite3")
    );
    let version: i64 = first
        .connection()
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
}

#[test]
fn cached_catalog_with_placeholder_pricing_is_stale() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let mut placeholder = image_model("fake");
    placeholder.pricing = Some(PricingMetadata {
        currency: "USD".into(),
        unit: PricingUnit::PerOutput,
        unit_label: "provider-defined generation".into(),
        amount: None,
        entries: Vec::new(),
        detail: Some("Price varies with the selected model controls".into()),
    });
    store
        .cache_models(
            &"fake".into(),
            &[placeholder],
            std::time::Duration::from_secs(6 * 60 * 60),
        )
        .unwrap();
    let cached = store.cached_models(&"fake".into()).unwrap().unwrap();
    assert!(cached.stale, "pre-cost catalog snapshots must be refetched");

    store
        .cache_models(
            &"fake".into(),
            &[priced_image_model("fake", 0.05)],
            std::time::Duration::from_secs(6 * 60 * 60),
        )
        .unwrap();
    let fresh = store.cached_models(&"fake".into()).unwrap().unwrap();
    assert!(!fresh.stale);
}

#[test]
fn restart_turns_interrupted_image_submissions_into_explicit_retry_states() {
    let root = tempdir().unwrap();
    let model = image_model("fake");
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let conversation = store.create_conversation("Recovery", None).unwrap();
    let prepared = PreparedStudioRun {
        model: model.clone(),
        quote: None,
        request: GenerationRequest {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            operation: model.operation,
            prompt: "still durable".into(),
            negative_prompt: None,
            output_count: 1,
            controls: BTreeMap::new(),
            inputs: Vec::new(),
            manifest_version: model.manifest_version.clone(),
            display_aspect_ratio: (1, 1),
        },
    };
    let runs = store
        .create_turn(
            conversation.id,
            "still durable",
            None,
            &[prepared],
            "device-a",
        )
        .unwrap();
    store.mark_submitting(&runs[0]).unwrap();
    drop(store);

    let reopened = StudioStore::open(root.path(), 1024).unwrap();
    assert_eq!(reopened.recover_interrupted_image_runs().unwrap(), 1);
    let view = reopened.conversation_view(conversation.id).unwrap();
    assert_eq!(view.turns[0].runs[0].state, StudioRunState::Failed);
    assert!(
        view.turns[0].runs[0]
            .error
            .as_deref()
            .is_some_and(|message| message.contains("may have completed"))
    );
    assert!(reopened.prepare_retry(runs[0].run_id, false).is_err());
    assert!(reopened.prepare_retry(runs[0].run_id, true).is_ok());
}

#[test]
fn create_turn_persists_catalog_cost() {
    let root = tempdir().unwrap();
    let model = priced_image_model("fake", 0.05);
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let conversation = store.create_conversation("Costs", None).unwrap();
    let prepared = PreparedStudioRun {
        model: model.clone(),
        quote: None,
        request: GenerationRequest {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            operation: model.operation,
            prompt: "priced comet".into(),
            negative_prompt: None,
            output_count: 2,
            controls: BTreeMap::new(),
            inputs: Vec::new(),
            manifest_version: model.manifest_version.clone(),
            display_aspect_ratio: (1, 1),
        },
    };
    store
        .create_turn(
            conversation.id,
            "priced comet",
            None,
            &[prepared],
            "device-a",
        )
        .unwrap();
    let view = store.conversation_view(conversation.id).unwrap();
    let quote = view.turns[0].runs[0].quote.clone().expect("catalog quote");
    assert_eq!(quote.source, QuoteSource::Catalog);
    assert_eq!(quote.currency, "USD");
    assert!((quote.amount - 0.10).abs() < f64::EPSILON);
}

#[test]
fn complete_run_persists_sniffed_supported_mime() {
    let root = tempdir().unwrap();
    let mut model = image_model("fake");
    model.output_mime_types = vec!["image/webp".into(), "image/png".into(), "image/jpeg".into()];
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let conversation = store.create_conversation("mime", None).unwrap();
    let stored = store
        .create_turn(
            conversation.id,
            "a comet",
            None,
            &[prepared_run(&model, "a comet")],
            "device-a",
        )
        .unwrap();
    store
        .complete_run(
            &stored[0],
            &[ProviderArtifact {
                media_kind: MediaKind::Image,
                mime_type: "image/webp".into(),
                bytes: vec![0xff, 0xd8, 0xff, 0xdb, 1, 2, 3],
                width: Some(8),
                height: Some(8),
                duration_seconds: None,
                metadata: serde_json::json!({}),
            }],
        )
        .unwrap();
    let view = store.conversation_view(conversation.id).unwrap();
    assert_eq!(view.turns[0].runs[0].state, StudioRunState::Succeeded);
    assert_eq!(view.turns[0].runs[0].artifacts[0].mime_type, "image/jpeg");
}

fn rgb_png(width: u32, height: u32) -> Vec<u8> {
    let mut raw = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        width,
        height,
        image::Rgb([40, 120, 200]),
    ))
    .write_to(&mut std::io::Cursor::new(&mut raw), image::ImageFormat::Png)
    .unwrap();
    raw
}

#[test]
fn complete_run_persists_a_preview_and_thumbhash() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024 * 1024).unwrap();
    let conversation = store.create_conversation("preview", None).unwrap();
    let stored = store
        .create_turn(
            conversation.id,
            "a comet",
            None,
            &[prepared_run(&image_model("fake"), "a comet")],
            "device-a",
        )
        .unwrap();
    store
        .complete_run(
            &stored[0],
            &[ProviderArtifact {
                media_kind: MediaKind::Image,
                mime_type: "image/png".into(),
                bytes: rgb_png(32, 24),
                width: Some(32),
                height: Some(24),
                duration_seconds: None,
                metadata: serde_json::json!({}),
            }],
        )
        .unwrap();
    let view = store.conversation_view(conversation.id).unwrap();
    let artifact = &view.turns[0].runs[0].artifacts[0];
    assert!(
        artifact
            .thumbhash
            .as_ref()
            .is_some_and(|hash| !hash.is_empty())
    );
    let gallery = store.list_gallery().unwrap();
    assert_eq!(gallery[0].thumbhash, artifact.thumbhash);
    let chunk = store.read_preview_chunk(artifact.id, 0).unwrap();
    assert_eq!(chunk.mime_type, "image/jpeg");
    assert!(chunk.done);
    let preview = BASE64.decode(chunk.data).unwrap();
    assert_eq!(&preview[..2], &[0xff, 0xd8]);
    store.delete_artifact(artifact.id).unwrap();
    assert!(store.read_preview_chunk(artifact.id, 0).is_err());
}

#[test]
fn preview_read_backfills_legacy_artifacts() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024 * 1024).unwrap();
    let conversation = store.create_conversation("legacy", None).unwrap();
    let stored = store
        .create_turn(
            conversation.id,
            "a comet",
            None,
            &[prepared_run(&image_model("fake"), "a comet")],
            "device-a",
        )
        .unwrap();
    store
        .complete_run(
            &stored[0],
            &[ProviderArtifact {
                media_kind: MediaKind::Image,
                mime_type: "image/png".into(),
                bytes: rgb_png(16, 16),
                width: Some(16),
                height: Some(16),
                duration_seconds: None,
                metadata: serde_json::json!({}),
            }],
        )
        .unwrap();
    let id = store.list_gallery().unwrap()[0].id;
    store
        .connection()
        .unwrap()
        .execute(
            "UPDATE studio_artifacts SET preview_relative_path = NULL, thumbhash = NULL WHERE id = ?1",
            [id.0.to_string()],
        )
        .unwrap();
    let _ = store.artifacts().delete_preview(id);
    assert_eq!(store.artifacts_missing_previews().unwrap(), vec![id]);
    store.ensure_preview(id).unwrap();
    let gallery = store.list_gallery().unwrap();
    assert!(gallery[0].thumbhash.is_some());
    assert!(store.artifacts().preview_exists(id));
}

#[test]
fn schema_v1_gains_a_thumbhash_column() {
    let root = tempdir().unwrap();
    let studio = root.path().join("studio");
    std::fs::create_dir_all(&studio).unwrap();
    let connection = rusqlite::Connection::open(studio.join("studio.sqlite3")).unwrap();
    connection
        .execute_batch(
            r#"
            CREATE TABLE studio_conversations (
                id TEXT PRIMARY KEY NOT NULL,
                title TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE studio_artifacts (
                id TEXT PRIMARY KEY NOT NULL,
                run_id TEXT NOT NULL,
                attempt_id TEXT NOT NULL,
                output_position INTEGER NOT NULL,
                media_kind TEXT NOT NULL,
                relative_path TEXT NOT NULL,
                mime_type TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                content_hash TEXT NOT NULL,
                metadata_json TEXT NOT NULL,
                preview_relative_path TEXT,
                created_at INTEGER NOT NULL
            );
            PRAGMA user_version = 1;
            "#,
        )
        .unwrap();
    drop(connection);
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let version: i64 = store
        .connection()
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    let has_thumbhash: i64 = store
        .connection()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('studio_artifacts') WHERE name = 'thumbhash'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(has_thumbhash, 1);
    let has_last_seen: i64 = store
        .connection()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('studio_conversations') WHERE name = 'last_seen_at'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(has_last_seen, 1);
}

#[test]
fn schema_v2_gains_a_last_seen_column() {
    let root = tempdir().unwrap();
    let studio = root.path().join("studio");
    std::fs::create_dir_all(&studio).unwrap();
    let connection = rusqlite::Connection::open(studio.join("studio.sqlite3")).unwrap();
    connection
        .execute_batch(
            r#"
            CREATE TABLE studio_conversations (
                id TEXT PRIMARY KEY NOT NULL,
                title TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            INSERT INTO studio_conversations (id, title, created_at, updated_at)
            VALUES ('c', 'old thread', 1, 50);
            PRAGMA user_version = 2;
            "#,
        )
        .unwrap();
    drop(connection);
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let version: i64 = store
        .connection()
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    let last_seen: i64 = store
        .connection()
        .unwrap()
        .query_row(
            "SELECT last_seen_at FROM studio_conversations WHERE id = 'c'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(last_seen, 50);
}

#[test]
fn schema_v3_gains_asset_duration_and_media_kind() {
    let root = tempdir().unwrap();
    let studio = root.path().join("studio");
    std::fs::create_dir_all(&studio).unwrap();
    let connection = rusqlite::Connection::open(studio.join("studio.sqlite3")).unwrap();
    connection
        .execute_batch(
            r#"
            CREATE TABLE studio_assets (
                id TEXT PRIMARY KEY NOT NULL,
                relative_path TEXT NOT NULL,
                mime_type TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                content_hash TEXT NOT NULL,
                width INTEGER,
                height INTEGER,
                created_at INTEGER NOT NULL
            );
            INSERT INTO studio_assets
                (id, relative_path, mime_type, size_bytes, content_hash, width, height, created_at)
            VALUES ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa', 'inputs/old.png', 'image/png', 4, 'abcd', 8, 8, 1);
            PRAGMA user_version = 3;
            "#,
        )
        .unwrap();
    drop(connection);
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let version: i64 = store
        .connection()
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    let (duration, kind): (Option<f64>, String) = store
        .connection()
        .unwrap()
        .query_row(
            "SELECT duration_seconds, media_kind FROM studio_assets WHERE id = 'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(duration, None);
    assert_eq!(kind, "image");
}

#[test]
fn complete_run_rejects_bytes_outside_the_model_formats() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let conversation = store.create_conversation("mime", None).unwrap();
    let stored = store
        .create_turn(
            conversation.id,
            "a comet",
            None,
            &[prepared_run(&image_model("fake"), "a comet")],
            "device-a",
        )
        .unwrap();
    store
        .complete_run(
            &stored[0],
            &[ProviderArtifact {
                media_kind: MediaKind::Image,
                mime_type: "image/png".into(),
                bytes: vec![0xff, 0xd8, 0xff, 0xdb, 1, 2, 3],
                width: Some(8),
                height: Some(8),
                duration_seconds: None,
                metadata: serde_json::json!({}),
            }],
        )
        .unwrap();
    let view = store.conversation_view(conversation.id).unwrap();
    assert_eq!(view.turns[0].runs[0].state, StudioRunState::Failed);
    assert_eq!(
        view.turns[0].runs[0].error.as_deref(),
        Some("provider artifact is not a supported format for this model")
    );
}

#[test]
fn bind_upscale_copies_source_run_aspect_when_artifact_has_no_pixels() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024 * 1024).unwrap();
    let conversation = store.create_conversation("aspect", None).unwrap();
    let mut prepared = prepared_run(&image_model("fake"), "a comet");
    prepared.request.display_aspect_ratio = (2, 3);
    let stored = store
        .create_turn(conversation.id, "a comet", None, &[prepared], "device-a")
        .unwrap();
    store
        .complete_run(
            &stored[0],
            &[ProviderArtifact {
                media_kind: MediaKind::Image,
                mime_type: "image/png".into(),
                bytes: rgb_png(32, 48),
                width: None,
                height: None,
                duration_seconds: None,
                metadata: serde_json::json!({}),
            }],
        )
        .unwrap();
    let source_id =
        store.conversation_view(conversation.id).unwrap().turns[0].runs[0].artifacts[0].id;
    let mut request = GenerationRequest {
        provider_id: "fake".into(),
        model_id: "upscaler".into(),
        operation: MediaOperation::Upscale,
        prompt: String::new(),
        negative_prompt: None,
        output_count: 1,
        controls: BTreeMap::new(),
        inputs: vec![GenerationInput {
            role: "source".into(),
            ordinal: 0,
            source: GenerationInputSource::Artifact {
                artifact_id: source_id,
            },
            content_hash: String::new(),
        }],
        manifest_version: "fixture-v1".into(),
        display_aspect_ratio: (1, 1),
    };
    store.bind_generation_inputs(&mut request).unwrap();
    assert_eq!(request.display_aspect_ratio, (2, 3));
}

fn i2v_model(provider_id: &str) -> MediaModel {
    MediaModel {
        provider_id: provider_id.into(),
        id: "seedance-i2v".into(),
        display_name: "Seedance I2V".into(),
        description: None,
        operation: MediaOperation::ImageToVideo,
        output_kind: MediaKind::Video,
        output_mime_types: vec!["video/mp4".into()],
        input_constraints: vec![InputConstraint {
            role: "source".into(),
            minimum_count: 1,
            maximum_count: 1,
            mime: MimeConstraint {
                accepted: vec!["image/png".into(), "image/jpeg".into(), "image/webp".into()],
                maximum_bytes: Some(25 * 1024 * 1024),
                minimum_short_side: Some(8),
                ..MimeConstraint::default()
            },
        }],
        prompt_maximum_chars: Some(2_500),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 1,
        controls: vec![ModelControl {
            id: "duration".into(),
            label: "Duration".into(),
            description: None,
            kind: ControlKind::Duration,
            required: true,
            default: None,
            minimum: None,
            maximum: None,
            step: None,
            choices: vec![zeron_studio::ControlChoice {
                value: ControlValue::DurationSeconds { value: 6.0 },
                label: "6s".into(),
            }],
            visible_when: Vec::new(),
        }],
        pricing: None,
        features: Vec::new(),
        video: VideoModelMeta {
            adapter_family: AdapterFamily::Seedance,
            generate_audio: zeron_studio::AudioCapability::Configurable { default: true },
            ..VideoModelMeta::default()
        },
        manifest_version: "fixture-v1".into(),
        fetched_at: chrono::Utc::now(),
    }
}

fn v2v_model(provider_id: &str) -> MediaModel {
    let mut model = i2v_model(provider_id);
    model.id = "seedance-v2v".into();
    model.operation = MediaOperation::VideoToVideo;
    model.input_constraints = vec![InputConstraint {
        role: "source".into(),
        minimum_count: 1,
        maximum_count: 1,
        mime: MimeConstraint {
            accepted: vec!["video/mp4".into()],
            maximum_bytes: Some(50 * 1024 * 1024),
            ..MimeConstraint::default()
        },
    }];
    model
}

fn ftyp_mp4() -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 20];
    bytes.extend_from_slice(b"ftypisom");
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes.extend_from_slice(b"isom");
    bytes
}

#[test]
fn bind_accepts_video_role_assets_and_artifacts() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024 * 1024).unwrap();
    let png = rgb_png(16, 16);
    let asset_id = store.publish_asset(&png, "image/png").unwrap();
    let mut i2v = GenerationRequest {
        provider_id: "fake".into(),
        model_id: "seedance-i2v".into(),
        operation: MediaOperation::ImageToVideo,
        prompt: "a comet".into(),
        negative_prompt: None,
        output_count: 1,
        controls: BTreeMap::new(),
        inputs: vec![GenerationInput {
            role: "source".into(),
            ordinal: 0,
            source: GenerationInputSource::Asset { asset_id },
            content_hash: String::new(),
        }],
        manifest_version: "fixture-v1".into(),
        display_aspect_ratio: (1, 1),
    };
    store.bind_generation_inputs(&mut i2v).unwrap();
    assert!(!i2v.inputs[0].content_hash.is_empty());
    store
        .resolve_generation_inputs(&i2v, &i2v_model("fake"))
        .unwrap();

    let still_rejected = GenerationRequest {
        provider_id: "fake".into(),
        model_id: "image-model".into(),
        operation: MediaOperation::TextToImage,
        prompt: "a comet".into(),
        negative_prompt: None,
        output_count: 1,
        controls: BTreeMap::new(),
        inputs: vec![GenerationInput {
            role: "source".into(),
            ordinal: 0,
            source: GenerationInputSource::Asset { asset_id },
            content_hash: String::new(),
        }],
        manifest_version: "fixture-v1".into(),
        display_aspect_ratio: (1, 1),
    };
    let error = store
        .bind_generation_inputs(&mut still_rejected.clone())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("video roles and ImageEdit masks"),
        "{error}"
    );

    let conversation = store.create_conversation("video bind", None).unwrap();
    let mut video = seedance_t2v_model("fake");
    video.output_mime_types = vec!["video/mp4".into()];
    let stored = store
        .create_turn(
            conversation.id,
            "a comet",
            None,
            &[prepared_run(&video, "a comet")],
            "device-a",
        )
        .unwrap();
    store
        .complete_run(
            &stored[0],
            &[ProviderArtifact {
                media_kind: MediaKind::Video,
                mime_type: "video/mp4".into(),
                bytes: ftyp_mp4(),
                width: Some(64),
                height: Some(36),
                duration_seconds: Some(4.0),
                metadata: serde_json::json!({}),
            }],
        )
        .unwrap();
    let artifact_id =
        store.conversation_view(conversation.id).unwrap().turns[0].runs[0].artifacts[0].id;
    let mut v2v = GenerationRequest {
        provider_id: "fake".into(),
        model_id: "seedance-v2v".into(),
        operation: MediaOperation::VideoToVideo,
        prompt: "remix".into(),
        negative_prompt: None,
        output_count: 1,
        controls: BTreeMap::new(),
        inputs: vec![GenerationInput {
            role: "source".into(),
            ordinal: 0,
            source: GenerationInputSource::Artifact { artifact_id },
            content_hash: String::new(),
        }],
        manifest_version: "fixture-v1".into(),
        display_aspect_ratio: (16, 9),
    };
    store.bind_generation_inputs(&mut v2v).unwrap();
    store
        .resolve_generation_inputs(&v2v, &v2v_model("fake"))
        .unwrap();
}

fn prepared_run(model: &MediaModel, prompt: &str) -> PreparedStudioRun {
    PreparedStudioRun {
        model: model.clone(),
        quote: None,
        request: GenerationRequest {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            operation: model.operation,
            prompt: prompt.into(),
            negative_prompt: None,
            output_count: 1,
            controls: BTreeMap::new(),
            inputs: Vec::new(),
            manifest_version: model.manifest_version.clone(),
            display_aspect_ratio: (1, 1),
        },
    }
}

#[test]
fn first_turn_titles_an_untitled_conversation_from_the_prompt() {
    let root = tempdir().unwrap();
    let model = image_model("fake");
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let conversation = store
        .create_conversation(zeron_proto::UNTITLED_STUDIO_TITLE, None)
        .unwrap();
    store
        .create_turn(
            conversation.id,
            "a red comet over the sea at dusk with extra words",
            None,
            &[prepared_run(
                &model,
                "a red comet over the sea at dusk with extra words",
            )],
            "device-a",
        )
        .unwrap();
    let view = store.conversation_view(conversation.id).unwrap();
    assert_eq!(view.conversation.title, "a red comet over the sea at");
}

#[test]
fn first_turn_titles_a_legacy_untitled_conversation_from_the_prompt() {
    let root = tempdir().unwrap();
    let model = image_model("fake");
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let conversation = store
        .create_conversation(zeron_proto::LEGACY_UNTITLED_STUDIO_TITLE, None)
        .unwrap();
    store
        .create_turn(
            conversation.id,
            "a red comet over the sea at dusk with extra words",
            None,
            &[prepared_run(
                &model,
                "a red comet over the sea at dusk with extra words",
            )],
            "device-a",
        )
        .unwrap();
    let view = store.conversation_view(conversation.id).unwrap();
    assert_eq!(view.conversation.title, "a red comet over the sea at");
}

#[test]
fn extend_turn_appends_the_original_model_set_under_the_same_prompt() {
    let root = tempdir().unwrap();
    let flux = image_model("fake");
    let mut kling = image_model("fake");
    kling.id = "kling".into();
    kling.display_name = "Kling".into();
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let conversation = store.create_conversation("More", None).unwrap();
    store
        .create_turn(
            conversation.id,
            "a red comet",
            None,
            &[
                prepared_run(&flux, "a red comet"),
                prepared_run(&kling, "a red comet"),
            ],
            "device-a",
        )
        .unwrap();
    let view = store.conversation_view(conversation.id).unwrap();
    let turn_id = view.turns[0].id;
    let (_, prompt, specs) = store.turn_extend_spec(turn_id).unwrap();
    assert_eq!(prompt, "a red comet");
    assert_eq!(specs.len(), 2);

    store
        .extend_turn(
            turn_id,
            &[
                prepared_run(&flux, "a red comet"),
                prepared_run(&kling, "a red comet"),
            ],
            "device-a",
        )
        .unwrap();
    let once = store.conversation_view(conversation.id).unwrap();
    assert_eq!(once.turns.len(), 1);
    assert_eq!(once.turns[0].prompt, "a red comet");
    assert_eq!(once.turns[0].runs.len(), 4);

    let (_, _, specs_again) = store.turn_extend_spec(turn_id).unwrap();
    assert_eq!(
        specs_again.len(),
        2,
        "already-appended copies are not templates"
    );

    store
        .extend_turn(
            turn_id,
            &[
                prepared_run(&flux, "a red comet"),
                prepared_run(&kling, "a red comet"),
            ],
            "device-a",
        )
        .unwrap();
    let twice = store.conversation_view(conversation.id).unwrap();
    assert_eq!(twice.turns.len(), 1);
    assert_eq!(twice.turns[0].runs.len(), 6);
    assert!(
        store
            .turn_extend_spec(zeron_studio::StudioTurnId::new())
            .is_err()
    );
}

#[test]
fn first_turn_keeps_a_user_chosen_title() {
    let root = tempdir().unwrap();
    let model = image_model("fake");
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let conversation = store.create_conversation("Night studies", None).unwrap();
    store
        .create_turn(
            conversation.id,
            "a red comet over the sea",
            None,
            &[prepared_run(&model, "a red comet over the sea")],
            "device-a",
        )
        .unwrap();
    let view = store.conversation_view(conversation.id).unwrap();
    assert_eq!(view.conversation.title, "Night studies");
}

#[test]
fn delete_conversation_removes_the_row_and_cascaded_turns() {
    let root = tempdir().unwrap();
    let model = image_model("fake");
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let conversation = store.create_conversation("Gone", None).unwrap();
    store
        .create_turn(
            conversation.id,
            "throwaway",
            None,
            &[prepared_run(&model, "throwaway")],
            "device-a",
        )
        .unwrap();
    store.delete_conversation(conversation.id).unwrap();
    assert!(store.conversation_view(conversation.id).is_err());
    assert!(store.list_conversations(true).unwrap().is_empty());
}

#[tokio::test]
async fn engine_assembly_opens_the_active_profiles_studio() {
    let root = tempdir().unwrap();
    let profile = EngineProfile::synced(root.path(), "org", "user");
    let expected = profile.store_root().join("studio/studio.sqlite3");
    let engine = EngineCore::assemble_with_profile(
        profile,
        std::sync::Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();

    assert_eq!(engine.studio.database_path(), expected);
    engine.shutdown().await;
}

#[test]
fn artifact_publish_is_atomic_id_based_and_non_overwriting() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let id = StudioArtifactId::new();
    let path = store
        .artifacts()
        .publish(id, "PNG", b"image bytes")
        .unwrap();

    assert_eq!(path.parent().unwrap().file_name().unwrap(), "artifacts");
    assert!(path.ends_with(format!("{}.png", id.0)));
    assert!(
        store
            .artifacts()
            .publish(id, "png", b"replacement")
            .is_err()
    );

    let mut bytes = Vec::new();
    store
        .artifacts()
        .open_artifact(id, "png")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"image bytes");
}

#[test]
fn artifact_store_rejects_traversal_and_oversized_data() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 4).unwrap();

    assert!(
        store
            .artifacts()
            .publish(StudioArtifactId::new(), "../png", b"x")
            .is_err()
    );
    assert!(
        store
            .artifacts()
            .publish(StudioArtifactId::new(), "png", b"12345")
            .is_err()
    );
}

#[test]
fn artifact_chunk_reads_resolve_mime_and_path_from_id() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let id = StudioArtifactId::new();
    store
        .artifacts()
        .publish(id, "webp", b"studio image")
        .unwrap();

    let chunk = store.artifacts().read_chunk(id, 0).unwrap();
    assert_eq!(chunk.artifact_id, id);
    assert_eq!(chunk.mime_type, "image/webp");
    assert_eq!(BASE64.decode(chunk.data).unwrap(), b"studio image");
    assert!(chunk.done);
    assert!(!chunk.file_name.contains('/'));
}

#[test]
fn database_enforces_exactly_one_input_source() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024).unwrap();
    let connection = store.connection().unwrap();
    connection
        .execute(
            "INSERT INTO studio_conversations (id, title, created_at, updated_at) VALUES ('c', 'c', 1, 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO studio_turns (id, conversation_id, position, prompt, created_at) VALUES ('t', 'c', 0, 'p', 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO studio_batches (id, turn_id, state, created_at, updated_at) VALUES ('b', 't', 'draft', 1, 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO studio_runs (id, batch_id, position, provider_id, model_id, operation, model_manifest_json, settings_json, owner_device_id, state, output_count, display_aspect_width, display_aspect_height, created_at, updated_at) VALUES ('r', 'b', 0, 'fake', 'm', 'text_to_image', '{}', '{}', 'device', 'draft', 1, 1, 1, 1, 1)",
            [],
        )
        .unwrap();

    let error = connection
        .execute(
            "INSERT INTO studio_run_inputs (run_id, role, ordinal, content_hash) VALUES ('r', 'source', 0, 'hash')",
            [],
        )
        .unwrap_err();
    assert!(error.to_string().contains("CHECK constraint failed"));
}

#[test]
fn provider_registry_accepts_multiple_adapters_without_engine_changes() {
    let registry = StudioProviderRegistry::new();
    registry
        .register(std::sync::Arc::new(FakeMediaProvider::new(
            "fake-one",
            Vec::new(),
            FakeSubmissionMode::Complete(Vec::new()),
        )))
        .unwrap();
    registry
        .register(std::sync::Arc::new(FakeMediaProvider::new(
            "fake-two",
            Vec::new(),
            FakeSubmissionMode::Complete(Vec::new()),
        )))
        .unwrap();

    let providers = registry.list().unwrap();
    assert_eq!(providers.len(), 2);
    assert!(
        registry
            .get(&ProviderId::new("fake-two"))
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn conversation_rpc_round_trips_and_archives_profile_history() {
    let root = tempdir().unwrap();
    let profile = EngineProfile::synced(root.path(), "org", "studio-user");
    let engine = EngineCore::assemble_with_profile(
        profile,
        std::sync::Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    let client = memory_client(engine.rpc_service());

    let created: StudioConversationSummary = serde_json::from_value(
        client
            .call(
                methods::CREATE_STUDIO_CONVERSATION,
                serde_json::json!({ "title": "First study" }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let renamed: StudioConversationSummary = serde_json::from_value(
        client
            .call(
                methods::RENAME_STUDIO_CONVERSATION,
                serde_json::json!({
                    "conversationId": created.id,
                    "title": "Comet studies"
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(renamed.title, "Comet studies");

    client
        .call(
            methods::ARCHIVE_STUDIO_CONVERSATION,
            serde_json::json!({ "conversationId": created.id, "archived": true }),
        )
        .await
        .unwrap();
    let active: ListStudioConversationsResponse = serde_json::from_value(
        client
            .call(methods::LIST_STUDIO_CONVERSATIONS, serde_json::json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(active.conversations.is_empty());
    let all: ListStudioConversationsResponse = serde_json::from_value(
        client
            .call(
                methods::LIST_STUDIO_CONVERSATIONS,
                serde_json::json!({ "includeArchived": true }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(all.conversations.len(), 1);
    assert!(all.conversations[0].archived);

    client
        .call(
            methods::DELETE_STUDIO_CONVERSATION,
            serde_json::json!({ "conversationId": created.id }),
        )
        .await
        .unwrap();
    let after_delete: ListStudioConversationsResponse = serde_json::from_value(
        client
            .call(
                methods::LIST_STUDIO_CONVERSATIONS,
                serde_json::json!({ "includeArchived": true }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(after_delete.conversations.is_empty());

    engine.shutdown().await;
}

#[tokio::test]
async fn artifact_rpc_reads_by_id_without_exposing_a_filesystem_path() {
    let root = tempdir().unwrap();
    let profile = EngineProfile::synced(root.path(), "org", "artifact-user");
    let engine = EngineCore::assemble_with_profile(
        profile,
        std::sync::Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    let id = StudioArtifactId::new();
    engine
        .studio
        .artifacts()
        .publish(id, "png", b"image")
        .unwrap();
    let client = memory_client(engine.rpc_service());

    let value = client
        .call(
            methods::READ_STUDIO_ARTIFACT_CHUNK,
            serde_json::json!({ "artifactId": id, "offset": 0 }),
        )
        .await
        .unwrap();
    assert!(value.get("path").is_none());
    let chunk: StudioArtifactChunk = serde_json::from_value(value).unwrap();
    assert_eq!(chunk.mime_type, "image/png");
    assert_eq!(BASE64.decode(chunk.data).unwrap(), b"image");

    engine.shutdown().await;
}

#[tokio::test]
async fn credentials_persist_only_metadata_and_provider_rpcs_use_the_secret_backend() {
    let root = tempdir().unwrap();
    let profile = EngineProfile::synced(root.path(), "org", "credential-user");
    let mut engine = EngineCore::assemble_with_profile(
        profile.clone(),
        std::sync::Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    let backend = std::sync::Arc::new(MemorySecrets::default());
    engine.studio_credentials =
        std::sync::Arc::new(StudioCredentials::with_backend(root.path(), backend).unwrap());
    engine
        .studio_providers
        .register(std::sync::Arc::new(
            FakeMediaProvider::new(
                "fake",
                vec![image_model("fake")],
                FakeSubmissionMode::Complete(Vec::new()),
            )
            .with_accepted_secret("super-secret-value"),
        ))
        .unwrap();
    let client = memory_client(engine.rpc_service());

    let configured: StudioProviderConnection = serde_json::from_value(
        client
            .call(
                methods::SET_STUDIO_PROVIDER_CREDENTIAL,
                serde_json::json!({
                    "providerId": "fake",
                    "displayLabel": "Fake images",
                    "secret": "super-secret-value"
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(configured.configured);
    assert_eq!(
        configured.validation_state,
        ProviderValidationState::NotValidated
    );

    let validated: StudioProviderConnection = serde_json::from_value(
        client
            .call(
                methods::VALIDATE_STUDIO_PROVIDER,
                serde_json::json!({ "providerId": "fake" }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(validated.validation_state, ProviderValidationState::Valid);

    let models: ListStudioModelsResponse = serde_json::from_value(
        client
            .call(
                methods::LIST_STUDIO_MODELS,
                serde_json::json!({ "providerId": "fake", "mediaKind": "image" }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(models.models.len(), 1);

    let metadata =
        std::fs::read_to_string(root.path().join("provider-accounts/connections.json")).unwrap();
    assert!(metadata.contains("Fake images"));
    assert!(!metadata.contains("super-secret-value"));

    client
        .call(
            methods::REMOVE_STUDIO_PROVIDER_CREDENTIAL,
            serde_json::json!({ "providerId": "fake" }),
        )
        .await
        .unwrap();
    assert!(engine.studio_credentials.list().unwrap().is_empty());
    engine.shutdown().await;
}

#[tokio::test]
async fn venice_safe_mode_defaults_off_and_persists_on_the_connection() {
    let root = tempdir().unwrap();
    let profile = EngineProfile::synced(root.path(), "org", "safe-mode-user");
    let mut engine = EngineCore::assemble_with_profile(
        profile.clone(),
        std::sync::Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    engine.studio_credentials = std::sync::Arc::new(
        StudioCredentials::with_backend(root.path(), std::sync::Arc::new(MemorySecrets::default()))
            .unwrap(),
    );
    let client = memory_client(engine.rpc_service());

    let configured: StudioProviderConnection = serde_json::from_value(
        client
            .call(
                methods::SET_STUDIO_PROVIDER_CREDENTIAL,
                serde_json::json!({
                    "providerId": "venice",
                    "displayLabel": "Venice",
                    "secret": "venice-key"
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(!configured.safe_mode);

    let updated: StudioProviderConnection = serde_json::from_value(
        client
            .call(
                methods::SET_STUDIO_PROVIDER_PREFERENCES,
                serde_json::json!({ "providerId": "venice", "safeMode": true }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(updated.safe_mode);

    let listed: ListStudioProvidersResponse = serde_json::from_value(
        client
            .call(methods::LIST_STUDIO_PROVIDERS, serde_json::json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listed.providers.len(), 1);
    assert!(listed.providers[0].safe_mode);

    let replaced: StudioProviderConnection = serde_json::from_value(
        client
            .call(
                methods::SET_STUDIO_PROVIDER_CREDENTIAL,
                serde_json::json!({
                    "providerId": "venice",
                    "displayLabel": "Venice",
                    "secret": "venice-key"
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(replaced.safe_mode, "replacing the key must keep safe mode");
    engine.shutdown().await;
}

#[tokio::test]
async fn multi_model_turn_persists_successful_siblings_and_failed_runs() {
    let root = tempdir().unwrap();
    let profile = EngineProfile::synced(root.path(), "org", "generation-user");
    let mut engine = EngineCore::assemble_with_profile(
        profile.clone(),
        std::sync::Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    engine.studio_credentials = std::sync::Arc::new(
        StudioCredentials::with_backend(root.path(), std::sync::Arc::new(MemorySecrets::default()))
            .unwrap(),
    );
    engine
        .studio_providers
        .register(std::sync::Arc::new(FakeMediaProvider::new(
            "success",
            vec![image_model("success")],
            FakeSubmissionMode::Complete(vec![ProviderArtifact {
                media_kind: MediaKind::Image,
                mime_type: "image/png".into(),
                bytes: b"\x89PNG\r\n\x1a\ngenerated image".to_vec(),
                width: Some(64),
                height: Some(64),
                duration_seconds: None,
                metadata: serde_json::json!({ "seed": 7 }),
            }]),
        )))
        .unwrap();
    engine
        .studio_providers
        .register(std::sync::Arc::new(FakeMediaProvider::new(
            "failure",
            vec![image_model("failure")],
            FakeSubmissionMode::Fail(ProviderError::new(
                ProviderErrorKind::InsufficientFunds,
                "not enough credits",
            )),
        )))
        .unwrap();
    let client = memory_client(engine.rpc_service());
    for provider in ["success", "failure"] {
        client
            .call(
                methods::SET_STUDIO_PROVIDER_CREDENTIAL,
                serde_json::json!({
                    "providerId": provider,
                    "displayLabel": provider,
                    "secret": "valid"
                }),
            )
            .await
            .unwrap();
    }
    let conversation: StudioConversationSummary = serde_json::from_value(
        client
            .call(
                methods::CREATE_STUDIO_CONVERSATION,
                serde_json::json!({ "title": "Parallel study" }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let run = |provider: &str| {
        serde_json::json!({
            "providerId": provider,
            "modelId": "image-model",
            "operation": "text_to_image",
            "outputCount": 1,
            "controls": {},
            "inputs": [],
            "manifestVersion": "fixture-v1",
            "displayAspectRatio": [1, 1]
        })
    };
    let mut updates = client
        .subscribe(
            methods::WATCH_STUDIO_CONVERSATION,
            serde_json::json!({ "conversationId": conversation.id }),
        )
        .await
        .unwrap();
    let empty: StudioConversationView =
        serde_json::from_value(updates.recv().await.unwrap()).unwrap();
    assert!(empty.turns.is_empty());
    let queued: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::CREATE_STUDIO_TURN,
                serde_json::json!({
                    "conversationId": conversation.id,
                    "prompt": "a comet above the sea",
                    "runs": [run("success"), run("failure")]
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(queued.turns[0].runs[0].state, StudioRunState::Queued);
    assert_eq!(queued.turns[0].runs[1].state, StudioRunState::Queued);

    let view = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let view: StudioConversationView =
                serde_json::from_value(updates.recv().await.unwrap()).unwrap();
            if view.turns[0].runs.iter().all(|run| {
                matches!(
                    run.state,
                    StudioRunState::Succeeded | StudioRunState::Failed
                )
            }) {
                break view;
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(view.turns.len(), 1);
    assert_eq!(view.turns[0].runs.len(), 2);
    assert_eq!(view.turns[0].runs[0].state, StudioRunState::Succeeded);
    assert_eq!(view.turns[0].runs[0].artifacts.len(), 1);
    assert_eq!(view.turns[0].runs[1].state, StudioRunState::Failed);
    assert_eq!(
        view.turns[0].runs[1].error.as_deref(),
        Some("not enough credits")
    );

    let failed_run = view.turns[0].runs[1].id;
    let retrying: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::RETRY_STUDIO_RUN,
                serde_json::json!({ "runId": failed_run }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retrying.turns[0].runs[1].state, StudioRunState::Queued);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let update: StudioConversationView =
                serde_json::from_value(updates.recv().await.unwrap()).unwrap();
            if update.turns[0].runs[1].state == StudioRunState::Failed {
                break;
            }
        }
    })
    .await
    .unwrap();
    let attempt_count: i64 = engine
        .studio
        .connection()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM studio_attempts WHERE run_id = ?1",
            [failed_run.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attempt_count, 2);

    let artifact = view.turns[0].runs[0].artifacts[0].id;
    let chunk: StudioArtifactChunk = serde_json::from_value(
        client
            .call(
                methods::READ_STUDIO_ARTIFACT_CHUNK,
                serde_json::json!({ "artifactId": artifact }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        BASE64.decode(chunk.data).unwrap(),
        b"\x89PNG\r\n\x1a\ngenerated image"
    );
    engine.shutdown().await;
    drop(updates);
    drop(client);
    drop(engine);

    let reopened = EngineCore::assemble_with_profile(
        profile,
        std::sync::Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    let reopened_client = memory_client(reopened.rpc_service());
    let mut reopened_updates = reopened_client
        .subscribe(
            methods::WATCH_STUDIO_CONVERSATION,
            serde_json::json!({ "conversationId": conversation.id }),
        )
        .await
        .unwrap();
    let persisted: StudioConversationView =
        serde_json::from_value(reopened_updates.recv().await.unwrap()).unwrap();
    assert_eq!(persisted.turns[0].runs[0].state, StudioRunState::Succeeded);
    assert_eq!(persisted.turns[0].runs[0].artifacts[0].id, artifact);
    reopened_client
        .call(
            methods::DELETE_STUDIO_ARTIFACT,
            serde_json::json!({ "artifactId": artifact }),
        )
        .await
        .unwrap();
    let deleted: StudioConversationView =
        serde_json::from_value(reopened_updates.recv().await.unwrap()).unwrap();
    assert!(deleted.turns[0].runs[0].artifacts.is_empty());
    assert!(
        reopened_client
            .call(
                methods::READ_STUDIO_ARTIFACT_CHUNK,
                serde_json::json!({ "artifactId": artifact }),
            )
            .await
            .is_err()
    );
    reopened.shutdown().await;
}

fn image_model_with_seed(provider_id: &str, version: &str, maximum: f64) -> MediaModel {
    let mut model = image_model(provider_id);
    model.manifest_version = version.into();
    model.controls = vec![ModelControl {
        id: "seed".into(),
        label: "Seed".into(),
        description: None,
        kind: ControlKind::Integer,
        required: false,
        default: Some(ControlValue::Integer { value: 0 }),
        minimum: Some(0.0),
        maximum: Some(maximum),
        step: Some(1.0),
        choices: Vec::new(),
        visible_when: Vec::new(),
    }];
    model
}

async fn studio_client_with_fake(
    root: &std::path::Path,
    provider: std::sync::Arc<FakeMediaProvider>,
) -> (EngineCore, zeron_rpc::RpcClient) {
    let profile = EngineProfile::synced(root, "org", "catalog-user");
    let mut engine = EngineCore::assemble_with_profile(
        profile,
        std::sync::Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    engine.studio_credentials = std::sync::Arc::new(
        StudioCredentials::with_backend(root, std::sync::Arc::new(MemorySecrets::default()))
            .unwrap(),
    );
    engine.studio_providers.register(provider).unwrap();
    let client = memory_client(engine.rpc_service());
    client
        .call(
            methods::SET_STUDIO_PROVIDER_CREDENTIAL,
            serde_json::json!({
                "providerId": "fake",
                "displayLabel": "Fake",
                "secret": "valid"
            }),
        )
        .await
        .unwrap();
    (engine, client)
}

async fn create_conversation(client: &zeron_rpc::RpcClient) -> StudioConversationSummary {
    serde_json::from_value(
        client
            .call(
                methods::CREATE_STUDIO_CONVERSATION,
                serde_json::json!({ "title": "Catalog" }),
            )
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn conversation_summary_marks_in_flight_runs_as_creating() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake")],
        FakeSubmissionMode::Complete(vec![png_artifact()]),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    assert!(
        !conversation.creating,
        "a brand-new thread has nothing in flight"
    );

    let mut updates = client
        .subscribe(
            methods::WATCH_STUDIO_CONVERSATION,
            serde_json::json!({ "conversationId": conversation.id }),
        )
        .await
        .unwrap();
    let _empty: StudioConversationView =
        serde_json::from_value(updates.recv().await.unwrap()).unwrap();

    let queued: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::CREATE_STUDIO_TURN,
                serde_json::json!({
                    "conversationId": conversation.id,
                    "prompt": "a comet above the sea",
                    "runs": [{
                        "providerId": "fake",
                        "modelId": "image-model",
                        "operation": "text_to_image",
                        "outputCount": 1,
                        "controls": {},
                        "inputs": [],
                        "manifestVersion": "fixture-v1",
                        "displayAspectRatio": [1, 1]
                    }]
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(queued.conversation.creating);
    assert_eq!(queued.turns[0].runs[0].state, StudioRunState::Queued);

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let view: StudioConversationView =
                serde_json::from_value(updates.recv().await.unwrap()).unwrap();
            if view.turns[0].runs[0].state == StudioRunState::Succeeded {
                assert!(!view.conversation.creating);
                break;
            }
        }
    })
    .await
    .expect("generation should settle");

    let listed: ListStudioConversationsResponse = serde_json::from_value(
        client
            .call(methods::LIST_STUDIO_CONVERSATIONS, serde_json::json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listed.conversations.len(), 1);
    assert!(!listed.conversations[0].creating);
    assert!(
        listed.conversations[0].done,
        "a settled generation the user has not opened is Done"
    );

    let seen: StudioConversationSummary = serde_json::from_value(
        client
            .call(
                methods::MARK_STUDIO_CONVERSATION_SEEN,
                serde_json::json!({ "conversationId": conversation.id }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(!seen.done);
    engine.shutdown().await;
}

#[tokio::test]
async fn submit_uses_the_cached_catalog_instead_of_refetching() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake")],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider.clone()).await;
    client
        .call(
            methods::LIST_STUDIO_MODELS,
            serde_json::json!({ "providerId": "fake", "mediaKind": "image" }),
        )
        .await
        .unwrap();
    assert_eq!(provider.list_call_count(), 1);
    provider.set_models(vec![image_model_with_seed("fake", "live-v2", 4.0)]);

    let conversation = create_conversation(&client).await;
    client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "a comet",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "image-model",
                    "operation": "text_to_image",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        provider.list_call_count(),
        1,
        "a fresh cache must be the same catalog the picker used"
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn quote_studio_batch_prefers_live_provider_quote() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(
        FakeMediaProvider::new(
            "fake",
            vec![priced_image_model("fake", 0.05)],
            FakeSubmissionMode::Complete(Vec::new()),
        )
        .with_quote(Quote::provider("USD", 0.99)),
    );
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let quoted: zeron_proto::QuoteStudioBatchResponse = serde_json::from_value(
        client
            .call(
                methods::QUOTE_STUDIO_BATCH,
                serde_json::json!({
                    "prompt": "a comet",
                    "runs": [{
                        "providerId": "fake",
                        "modelId": "image-model",
                        "operation": "text_to_image",
                        "outputCount": 2,
                        "controls": {},
                        "inputs": [],
                        "manifestVersion": "fixture-v1",
                        "displayAspectRatio": [1, 1]
                    }]
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let quote = quoted.runs[0].quote.clone().expect("live quote");
    assert_eq!(quote.source, QuoteSource::Provider);
    assert!((quote.amount - 0.99).abs() < f64::EPSILON);
    let total = quoted.total.expect("batch total");
    assert!((total.amount - 0.99).abs() < f64::EPSILON);
    engine.shutdown().await;
}

#[tokio::test]
async fn get_studio_provider_balance_returns_prepaid_credit() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(
        FakeMediaProvider::new(
            "fake",
            vec![image_model("fake")],
            FakeSubmissionMode::Complete(Vec::new()),
        )
        .with_balance(AccountBalance {
            remaining: Quote::catalog("USD", 12.34),
        }),
    );
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let response: zeron_proto::StudioProviderBalanceResponse = serde_json::from_value(
        client
            .call(
                methods::GET_STUDIO_PROVIDER_BALANCE,
                serde_json::json!({ "providerId": "fake" }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let remaining = response.balance.expect("balance").remaining;
    assert_eq!(remaining.currency, "USD");
    assert!((remaining.amount - 12.34).abs() < f64::EPSILON);
    engine.shutdown().await;
}

#[tokio::test]
async fn submit_silently_rebinds_a_compatible_request_to_the_current_catalog() {
    let root = tempdir().unwrap();
    let current = image_model_with_seed("fake", "current-v2", 10.0);
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![current.clone()],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider.clone()).await;
    engine
        .studio
        .cache_models(
            &"fake".into(),
            &[current],
            std::time::Duration::from_secs(60),
        )
        .unwrap();

    let conversation = create_conversation(&client).await;
    let view: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::CREATE_STUDIO_TURN,
                serde_json::json!({
                    "conversationId": conversation.id,
                    "prompt": "a comet",
                    "runs": [{
                        "providerId": "fake",
                        "modelId": "image-model",
                        "operation": "text_to_image",
                        "outputCount": 1,
                        "controls": { "seed": { "type": "integer", "value": 4 } },
                        "inputs": [],
                        "manifestVersion": "old-picker-version",
                        "displayAspectRatio": [1, 1]
                    }]
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(view.turns[0].runs[0].model.manifest_version, "current-v2");
    assert_eq!(provider.list_call_count(), 0);

    let request_json: String = engine
        .studio
        .connection()
        .unwrap()
        .query_row("SELECT request_json FROM studio_attempts", [], |row| {
            row.get(0)
        })
        .unwrap();
    let request: serde_json::Value = serde_json::from_str(&request_json).unwrap();
    assert_eq!(request["manifest_version"], "current-v2");
    engine.shutdown().await;
}

#[tokio::test]
async fn submit_drops_unknown_controls_replayed_from_a_previous_job() {
    let root = tempdir().unwrap();
    let current = image_model_with_seed("fake", "current-v2", 10.0);
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![current.clone()],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    engine
        .studio
        .cache_models(
            &"fake".into(),
            &[current],
            std::time::Duration::from_secs(60),
        )
        .unwrap();

    let conversation = create_conversation(&client).await;
    let view: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::CREATE_STUDIO_TURN,
                serde_json::json!({
                    "conversationId": conversation.id,
                    "prompt": "a comet",
                    "runs": [{
                        "providerId": "fake",
                        "modelId": "image-model",
                        "operation": "text_to_image",
                        "outputCount": 1,
                        "controls": {
                            "seed": { "type": "integer", "value": 4 },
                            "safe_mode": { "type": "boolean", "value": false }
                        },
                        "inputs": [],
                        "manifestVersion": "old-picker-version",
                        "displayAspectRatio": [1, 1]
                    }]
                }),
            )
            .await
            .expect("reused safe_mode must not fail bind against a catalog that lacks it"),
    )
    .unwrap();
    assert!(
        !view.turns[0].runs[0]
            .controls
            .contains_key(&zeron_studio::ControlId::from("safe_mode"))
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn submit_reports_the_real_constraint_error_when_settings_no_longer_fit() {
    let root = tempdir().unwrap();
    let current = image_model_with_seed("fake", "current-v2", 4.0);
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![current.clone()],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    engine
        .studio
        .cache_models(
            &"fake".into(),
            &[current],
            std::time::Duration::from_secs(60),
        )
        .unwrap();

    let conversation = create_conversation(&client).await;
    let error = client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "a comet",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "image-model",
                    "operation": "text_to_image",
                    "outputCount": 1,
                    "controls": { "seed": { "type": "integer", "value": 11 } },
                    "inputs": [],
                    "manifestVersion": "old-picker-version",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("above maximum"),
        "expected the control error, got {error}"
    );
    assert!(
        !error.to_lowercase().contains("manifest"),
        "catalog identity must not leak to the user: {error}"
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn extend_turn_appends_runs_without_a_new_prompt() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake")],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    let created: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::CREATE_STUDIO_TURN,
                serde_json::json!({
                    "conversationId": conversation.id,
                    "prompt": "a comet above the sea",
                    "runs": [{
                        "providerId": "fake",
                        "modelId": "image-model",
                        "operation": "text_to_image",
                        "outputCount": 2,
                        "controls": {},
                        "inputs": [],
                        "manifestVersion": "fixture-v1",
                        "displayAspectRatio": [1, 1]
                    }]
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(created.turns.len(), 1);
    assert_eq!(created.turns[0].runs.len(), 1);

    let extended: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::EXTEND_STUDIO_TURN,
                serde_json::json!({ "turnId": created.turns[0].id }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(extended.turns.len(), 1);
    assert_eq!(extended.turns[0].prompt, "a comet above the sea");
    assert_eq!(extended.turns[0].runs.len(), 2);
    assert_eq!(extended.turns[0].runs[0].output_count, 2);
    assert_eq!(extended.turns[0].runs[1].output_count, 2);
    assert_eq!(extended.turns[0].runs[1].state, StudioRunState::Queued);

    let again: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::EXTEND_STUDIO_TURN,
                serde_json::json!({ "turnId": created.turns[0].id }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(again.turns.len(), 1);
    assert_eq!(again.turns[0].runs.len(), 3);
    engine.shutdown().await;
}

fn png_artifact() -> ProviderArtifact {
    ProviderArtifact {
        media_kind: MediaKind::Image,
        mime_type: "image/png".into(),
        bytes: b"\x89PNG\r\n\x1a\ngenerated image".to_vec(),
        width: Some(64),
        height: Some(64),
        duration_seconds: None,
        metadata: serde_json::json!({}),
    }
}

async fn wait_for_gallery(
    updates: &mut tokio::sync::mpsc::Receiver<serde_json::Value>,
    count: usize,
) -> ListStudioArtifactsResponse {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let list: ListStudioArtifactsResponse =
                serde_json::from_value(updates.recv().await.unwrap()).unwrap();
            if list.artifacts.len() >= count {
                break list;
            }
        }
    })
    .await
    .expect("gallery did not reach the expected artifact count")
}

#[tokio::test]
async fn gallery_lists_images_across_conversations_newest_first() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake")],
        FakeSubmissionMode::Complete(vec![png_artifact()]),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let first = create_conversation(&client).await;
    let second = serde_json::from_value::<StudioConversationSummary>(
        client
            .call(
                methods::CREATE_STUDIO_CONVERSATION,
                serde_json::json!({ "title": "Later study" }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let run = serde_json::json!({
        "providerId": "fake",
        "modelId": "image-model",
        "operation": "text_to_image",
        "outputCount": 1,
        "controls": {},
        "inputs": [],
        "manifestVersion": "fixture-v1",
        "displayAspectRatio": [1, 1]
    });
    let mut gallery = client
        .subscribe(methods::WATCH_STUDIO_GALLERY, serde_json::json!({}))
        .await
        .unwrap();
    let empty = wait_for_gallery(&mut gallery, 0).await;
    assert!(empty.artifacts.is_empty());

    client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": first.id,
                "prompt": "older comet",
                "runs": [run.clone()]
            }),
        )
        .await
        .unwrap();
    let one = wait_for_gallery(&mut gallery, 1).await;
    assert_eq!(one.artifacts.len(), 1);
    assert_eq!(one.artifacts[0].conversation_id, first.id);
    assert_eq!(one.artifacts[0].prompt, "older comet");
    assert_eq!(one.artifacts[0].model_display_name, "Image model");

    client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": second.id,
                "prompt": "newer comet",
                "runs": [run]
            }),
        )
        .await
        .unwrap();
    let two = wait_for_gallery(&mut gallery, 2).await;
    assert_eq!(two.artifacts.len(), 2);
    assert_eq!(two.artifacts[0].prompt, "newer comet");
    assert_eq!(two.artifacts[1].prompt, "older comet");
    assert_eq!(two.artifacts[0].conversation_id, second.id);

    let listed: ListStudioArtifactsResponse = serde_json::from_value(
        client
            .call(methods::LIST_STUDIO_ARTIFACTS, serde_json::json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listed.artifacts.len(), 2);

    client
        .call(
            methods::DELETE_STUDIO_ARTIFACT,
            serde_json::json!({ "artifactId": two.artifacts[0].id }),
        )
        .await
        .unwrap();
    let after = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let list: ListStudioArtifactsResponse =
                serde_json::from_value(gallery.recv().await.unwrap()).unwrap();
            if list.artifacts.len() == 1 {
                break list;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(after.artifacts[0].prompt, "older comet");
    engine.shutdown().await;
}

fn upscale_model(provider_id: &str) -> MediaModel {
    MediaModel {
        provider_id: provider_id.into(),
        id: "upscaler".into(),
        display_name: "Upscaler".into(),
        description: None,
        operation: MediaOperation::Upscale,
        output_kind: MediaKind::Image,
        output_mime_types: vec!["image/png".into()],
        input_constraints: vec![InputConstraint {
            role: "source".into(),
            minimum_count: 1,
            maximum_count: 1,
            mime: MimeConstraint {
                accepted: vec!["image/png".into(), "image/jpeg".into(), "image/webp".into()],
                maximum_bytes: Some(25 * 1024 * 1024),
                ..MimeConstraint::default()
            },
        }],
        prompt_maximum_chars: None,
        negative_prompt_maximum_chars: None,
        maximum_output_count: 1,
        controls: vec![ModelControl {
            id: "scale".into(),
            label: "Scale".into(),
            description: None,
            kind: ControlKind::Integer,
            required: true,
            default: Some(ControlValue::Integer { value: 2 }),
            minimum: Some(2.0),
            maximum: Some(4.0),
            step: Some(2.0),
            choices: vec![],
            visible_when: Vec::new(),
        }],
        pricing: None,
        features: Vec::new(),
        video: zeron_studio::VideoModelMeta::default(),
        manifest_version: "fixture-v1".into(),
        fetched_at: chrono::Utc::now(),
    }
}

async fn wait_for_success(
    updates: &mut tokio::sync::mpsc::Receiver<serde_json::Value>,
    turns: usize,
) -> StudioConversationView {
    wait_for_view(updates, |view| {
        view.turns.len() >= turns
            && view
                .turns
                .last()
                .and_then(|turn| turn.runs.last())
                .is_some_and(|run| run.state == StudioRunState::Succeeded)
    })
    .await
}

async fn wait_for_succeeded_runs(
    updates: &mut tokio::sync::mpsc::Receiver<serde_json::Value>,
    runs: usize,
) -> StudioConversationView {
    wait_for_view(updates, |view| {
        view.turns
            .iter()
            .flat_map(|turn| turn.runs.iter())
            .filter(|run| run.state == StudioRunState::Succeeded)
            .count()
            >= runs
    })
    .await
}

async fn wait_for_view(
    updates: &mut tokio::sync::mpsc::Receiver<serde_json::Value>,
    pred: impl Fn(&StudioConversationView) -> bool,
) -> StudioConversationView {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let view: StudioConversationView =
                serde_json::from_value(updates.recv().await.unwrap()).unwrap();
            if pred(&view) {
                break view;
            }
        }
    })
    .await
    .expect("run did not succeed")
}

fn edit_model(provider_id: &str) -> MediaModel {
    MediaModel {
        provider_id: provider_id.into(),
        id: "image-edit".into(),
        display_name: "Edit model".into(),
        description: None,
        operation: MediaOperation::ImageEdit,
        output_kind: MediaKind::Image,
        output_mime_types: vec!["image/png".into()],
        input_constraints: vec![
            InputConstraint {
                role: "source".into(),
                minimum_count: 1,
                maximum_count: 1,
                mime: MimeConstraint {
                    accepted: vec!["image/png".into(), "image/jpeg".into(), "image/webp".into()],
                    maximum_bytes: Some(25 * 1024 * 1024),
                    ..MimeConstraint::default()
                },
            },
            InputConstraint {
                role: "mask".into(),
                minimum_count: 0,
                maximum_count: 2,
                mime: MimeConstraint {
                    accepted: vec!["image/png".into(), "image/jpeg".into(), "image/webp".into()],
                    maximum_bytes: Some(25 * 1024 * 1024),
                    ..MimeConstraint::default()
                },
            },
        ],
        prompt_maximum_chars: Some(5_000),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 1,
        controls: Vec::new(),
        pricing: None,
        features: Vec::new(),
        video: zeron_studio::VideoModelMeta::default(),
        manifest_version: "fixture-v1".into(),
        fetched_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn upscale_turn_uses_an_existing_artifact_and_empty_prompt() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake"), upscale_model("fake")],
        FakeSubmissionMode::Complete(vec![png_artifact()]),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider.clone()).await;
    let conversation = create_conversation(&client).await;
    let mut updates = client
        .subscribe(
            methods::WATCH_STUDIO_CONVERSATION,
            serde_json::json!({ "conversationId": conversation.id }),
        )
        .await
        .unwrap();
    let _empty: StudioConversationView =
        serde_json::from_value(updates.recv().await.unwrap()).unwrap();

    client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "a comet",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "image-model",
                    "operation": "text_to_image",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap();
    let generated = wait_for_success(&mut updates, 1).await;
    let source = &generated.turns[0].runs[0].artifacts[0];
    assert!(!source.content_hash.is_empty());

    let upscaled: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::APPEND_STUDIO_DERIVED_RUN,
                serde_json::json!({
                    "sourceArtifactId": source.id,
                    "prompt": "",
                    "run": {
                        "providerId": "fake",
                        "modelId": "upscaler",
                        "operation": "upscale",
                        "outputCount": 1,
                        "controls": { "scale": { "type": "integer", "value": 2 } },
                        "inputs": [],
                        "manifestVersion": "fixture-v1",
                        "displayAspectRatio": [1, 1]
                    }
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(upscaled.turns.len(), 1);
    assert_eq!(upscaled.turns[0].runs.len(), 2);
    assert_eq!(upscaled.turns[0].runs[1].inputs.len(), 1);
    assert_eq!(
        upscaled.turns[0].runs[1].inputs[0].content_hash,
        source.content_hash
    );
    wait_for_succeeded_runs(&mut updates, 2).await;

    let gallery: ListStudioArtifactsResponse = serde_json::from_value(
        client
            .call(methods::LIST_STUDIO_ARTIFACTS, serde_json::json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    let upscaled_item = gallery
        .artifacts
        .iter()
        .find(|item| item.model_display_name == "Upscaler")
        .expect("upscaled image should be in the gallery");
    assert_eq!(
        upscaled_item.source_artifact_id,
        Some(source.id),
        "gallery metadata should retain the original/upscale relationship"
    );

    let input_count: i64 = engine
        .studio
        .connection()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM studio_run_inputs", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(input_count, 1);
    assert!(!provider.last_submit_inputs().is_empty());
    engine.shutdown().await;
}

#[tokio::test]
async fn empty_prompt_is_still_rejected_for_text_to_image() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake")],
        FakeSubmissionMode::Complete(vec![png_artifact()]),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    let error = client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "   ",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "image-model",
                    "operation": "text_to_image",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("prompt"));
    engine.shutdown().await;
}

#[tokio::test]
async fn video_and_edit_submits_are_still_blocked() {
    let root = tempdir().unwrap();
    let mut video = image_model("fake");
    video.id = "video-model".into();
    video.operation = MediaOperation::TextToVideo;
    video.output_kind = MediaKind::Video;
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![video],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    let error = client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "a comet",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "video-model",
                    "operation": "text_to_video",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("text-to-image, image-edit, and upscale")
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn create_turn_rejects_image_edit() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![edit_model("fake")],
        FakeSubmissionMode::Complete(vec![png_artifact()]),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    let error = client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "change the sky",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "image-edit",
                    "operation": "image_edit",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("appended"));
    engine.shutdown().await;
}

#[tokio::test]
async fn append_edit_stays_on_the_source_turn_and_keeps_the_edit_prompt() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake"), edit_model("fake")],
        FakeSubmissionMode::Complete(vec![png_artifact()]),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider.clone()).await;
    let conversation = create_conversation(&client).await;
    let mut updates = client
        .subscribe(
            methods::WATCH_STUDIO_CONVERSATION,
            serde_json::json!({ "conversationId": conversation.id }),
        )
        .await
        .unwrap();
    let _empty: StudioConversationView =
        serde_json::from_value(updates.recv().await.unwrap()).unwrap();
    client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "a comet",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "image-model",
                    "operation": "text_to_image",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap();
    let generated = wait_for_success(&mut updates, 1).await;
    let source = &generated.turns[0].runs[0].artifacts[0];

    let edited: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::APPEND_STUDIO_DERIVED_RUN,
                serde_json::json!({
                    "sourceArtifactId": source.id,
                    "prompt": "make the sky sunrise",
                    "run": {
                        "providerId": "fake",
                        "modelId": "image-edit",
                        "operation": "image_edit",
                        "outputCount": 1,
                        "controls": {},
                        "inputs": [],
                        "manifestVersion": "fixture-v1",
                        "displayAspectRatio": [1, 1]
                    }
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(edited.turns.len(), 1);
    assert_eq!(edited.turns[0].prompt, "a comet");
    assert_eq!(edited.turns[0].runs.len(), 2);
    assert_eq!(
        edited.turns[0].runs[1].prompt.as_deref(),
        Some("make the sky sunrise")
    );
    let finished = wait_for_succeeded_runs(&mut updates, 2).await;
    let child = &finished.turns[0].runs[1].artifacts[0];

    let gallery: ListStudioArtifactsResponse = serde_json::from_value(
        client
            .call(methods::LIST_STUDIO_ARTIFACTS, serde_json::json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    let edit_item = gallery
        .artifacts
        .iter()
        .find(|item| item.id == child.id)
        .expect("edited image should be in the gallery");
    assert_eq!(edit_item.source_artifact_id, Some(source.id));
    assert_eq!(edit_item.prompt, "make the sky sunrise");
    assert!(!provider.last_submit_inputs().is_empty());
    engine.shutdown().await;
}

#[tokio::test]
async fn append_edit_publishes_a_mask_asset() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake"), edit_model("fake")],
        FakeSubmissionMode::Complete(vec![png_artifact()]),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider.clone()).await;
    let conversation = create_conversation(&client).await;
    let mut updates = client
        .subscribe(
            methods::WATCH_STUDIO_CONVERSATION,
            serde_json::json!({ "conversationId": conversation.id }),
        )
        .await
        .unwrap();
    let _empty: StudioConversationView =
        serde_json::from_value(updates.recv().await.unwrap()).unwrap();
    client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "a comet",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "image-model",
                    "operation": "text_to_image",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap();
    let generated = wait_for_success(&mut updates, 1).await;
    let source = &generated.turns[0].runs[0].artifacts[0];
    let mask = BASE64.encode(png_artifact().bytes);

    client
        .call(
            methods::APPEND_STUDIO_DERIVED_RUN,
            serde_json::json!({
                "sourceArtifactId": source.id,
                "prompt": "replace the painted area with a sunrise",
                "maskPngBase64": mask,
                "run": {
                    "providerId": "fake",
                    "modelId": "image-edit",
                    "operation": "image_edit",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }
            }),
        )
        .await
        .unwrap();
    wait_for_succeeded_runs(&mut updates, 2).await;

    let asset_count: i64 = engine
        .studio
        .connection()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM studio_assets", [], |row| row.get(0))
        .unwrap();
    assert_eq!(asset_count, 1);
    let mask_inputs = provider
        .last_submit_inputs()
        .into_iter()
        .filter(|input| input.role.as_str() == "mask")
        .count();
    assert_eq!(mask_inputs, 1);
    engine.shutdown().await;
}

#[tokio::test]
async fn append_edit_requires_a_prompt() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake"), edit_model("fake")],
        FakeSubmissionMode::Complete(vec![png_artifact()]),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    let mut updates = client
        .subscribe(
            methods::WATCH_STUDIO_CONVERSATION,
            serde_json::json!({ "conversationId": conversation.id }),
        )
        .await
        .unwrap();
    let _empty: StudioConversationView =
        serde_json::from_value(updates.recv().await.unwrap()).unwrap();
    client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "a comet",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "image-model",
                    "operation": "text_to_image",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap();
    let generated = wait_for_success(&mut updates, 1).await;
    let source = &generated.turns[0].runs[0].artifacts[0];
    let error = client
        .call(
            methods::APPEND_STUDIO_DERIVED_RUN,
            serde_json::json!({
                "sourceArtifactId": source.id,
                "prompt": "   ",
                "run": {
                    "providerId": "fake",
                    "modelId": "image-edit",
                    "operation": "image_edit",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }
            }),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("prompt"));
    engine.shutdown().await;
}

#[tokio::test]
async fn upscale_rejects_hash_mismatch_deleted_artifact_and_asset_source() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake"), upscale_model("fake")],
        FakeSubmissionMode::Complete(vec![png_artifact()]),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    let mut updates = client
        .subscribe(
            methods::WATCH_STUDIO_CONVERSATION,
            serde_json::json!({ "conversationId": conversation.id }),
        )
        .await
        .unwrap();
    let _empty: StudioConversationView =
        serde_json::from_value(updates.recv().await.unwrap()).unwrap();
    client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "a comet",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "image-model",
                    "operation": "text_to_image",
                    "outputCount": 1,
                    "controls": {},
                    "inputs": [],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap();
    let generated = wait_for_success(&mut updates, 1).await;
    let source = &generated.turns[0].runs[0].artifacts[0];

    let mismatch = client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "upscaler",
                    "operation": "upscale",
                    "outputCount": 1,
                    "controls": { "scale": { "type": "integer", "value": 2 } },
                    "inputs": [{
                        "role": "source",
                        "ordinal": 0,
                        "source": { "source": "artifact", "artifact_id": source.id },
                        "content_hash": "not-the-hash"
                    }],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap_err();
    assert!(mismatch.to_string().contains("hash"));

    let asset = client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "upscaler",
                    "operation": "upscale",
                    "outputCount": 1,
                    "controls": { "scale": { "type": "integer", "value": 2 } },
                    "inputs": [{
                        "role": "source",
                        "ordinal": 0,
                        "source": { "source": "asset", "asset_id": StudioArtifactId::new() },
                        "content_hash": "abc"
                    }],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap_err();
    assert!(asset.to_string().contains("asset"));

    client
        .call(
            methods::DELETE_STUDIO_ARTIFACT,
            serde_json::json!({ "artifactId": source.id }),
        )
        .await
        .unwrap();
    let deleted = client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "",
                "runs": [{
                    "providerId": "fake",
                    "modelId": "upscaler",
                    "operation": "upscale",
                    "outputCount": 1,
                    "controls": { "scale": { "type": "integer", "value": 2 } },
                    "inputs": [{
                        "role": "source",
                        "ordinal": 0,
                        "source": { "source": "artifact", "artifact_id": source.id },
                        "content_hash": ""
                    }],
                    "manifestVersion": "fixture-v1",
                    "displayAspectRatio": [1, 1]
                }]
            }),
        )
        .await
        .unwrap_err();
    assert!(deleted.to_string().contains("not found") || deleted.to_string().contains("artifact"));
    engine.shutdown().await;
}

#[tokio::test]
async fn list_studio_models_omits_hidden_adapter_family() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![
            image_model("fake"),
            seedance_t2v_model("fake"),
            hidden_kling_model("fake"),
        ],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;

    let all: ListStudioModelsResponse = serde_json::from_value(
        client
            .call(
                methods::LIST_STUDIO_MODELS,
                serde_json::json!({ "providerId": "fake" }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let ids: Vec<_> = all
        .models
        .iter()
        .map(|model| model.id.as_str().to_owned())
        .collect();
    assert!(ids.contains(&"image-model".to_owned()));
    assert!(ids.contains(&"seedance-t2v".to_owned()));
    assert!(!ids.contains(&"kling-o3-pro-reference-to-video".to_owned()));

    let video: ListStudioModelsResponse = serde_json::from_value(
        client
            .call(
                methods::LIST_STUDIO_MODELS,
                serde_json::json!({ "providerId": "fake", "mediaKind": "video" }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(video.models.len(), 1);
    assert_eq!(video.models[0].id.as_str(), "seedance-t2v");
    engine.shutdown().await;
}

#[tokio::test]
async fn evaluate_studio_composer_returns_view() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake")],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let snapshot = ComposerSnapshot {
        mode: ComposerMode::Image,
        prompt: "a comet".into(),
        selected: vec![SelectedModelRef::new("fake", "image-model")],
        ..ComposerSnapshot::default()
    };
    let view: zeron_proto::ComposerView = serde_json::from_value(
        client
            .call(
                methods::EVALUATE_STUDIO_COMPOSER,
                serde_json::json!({
                    "composer": snapshot,
                    "providerId": "fake"
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(view.mode, ComposerMode::Image);
    assert!(view.send.enabled);
    assert_eq!(view.phase, ComposerPhase::Editing);
    engine.shutdown().await;
}

#[tokio::test]
async fn create_studio_turn_composer_prompt_mismatch_is_bad_params() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake")],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    let snapshot = ComposerSnapshot {
        prompt: "right prompt".into(),
        selected: vec![SelectedModelRef::new("fake", "image-model")],
        ..ComposerSnapshot::default()
    };
    let err = client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "wrong prompt",
                "composer": snapshot
            }),
        )
        .await
        .unwrap_err();
    // Server emits BadParams; the wire carries it as Failed(err.to_string()).
    assert!(err.to_string().contains("composer.prompt"), "{err:?}");
    engine.shutdown().await;
}

#[tokio::test]
async fn create_studio_turn_composer_blocks_send_as_structured_error() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake")],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    let snapshot = ComposerSnapshot {
        prompt: "a comet".into(),
        ..ComposerSnapshot::default()
    };
    let err = client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "a comet",
                "composer": snapshot
            }),
        )
        .await
        .unwrap_err();
    match err {
        RpcError::FailedStructured { payload, .. } => {
            let error: StudioValidationError = serde_json::from_value(payload).unwrap();
            assert_eq!(error.code, STUDIO_VALIDATION_CODE);
            assert!(
                error
                    .conflicts
                    .iter()
                    .any(|conflict| conflict.code == ConflictCode::EmptyModelSet)
            );
        }
        other => panic!("expected FailedStructured, got {other:?}"),
    }
    engine.shutdown().await;
}

#[tokio::test]
async fn create_studio_turn_composer_projects_image_runs() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake")],
        FakeSubmissionMode::Complete(vec![png_artifact()]),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    let snapshot = ComposerSnapshot {
        conversation_id: Some(conversation.id),
        prompt: "a comet".into(),
        selected: vec![SelectedModelRef::new("fake", "image-model")],
        ..ComposerSnapshot::default()
    };
    let view: StudioConversationView = serde_json::from_value(
        client
            .call(
                methods::CREATE_STUDIO_TURN,
                serde_json::json!({
                    "conversationId": conversation.id,
                    "prompt": "a comet",
                    "composer": snapshot
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(view.turns.len(), 1);
    assert_eq!(view.turns[0].runs.len(), 1);
    assert_eq!(view.turns[0].runs[0].model.id.as_str(), "image-model");
    engine.shutdown().await;
}

#[tokio::test]
async fn create_studio_turn_composer_still_rejects_video_submit() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![seedance_t2v_model("fake")],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let conversation = create_conversation(&client).await;
    let snapshot = ComposerSnapshot {
        mode: ComposerMode::Video,
        prompt: "a comet".into(),
        duration: Some(ControlValue::DurationSeconds { value: 6.0 }),
        selected: vec![SelectedModelRef::new("fake", "seedance-t2v")],
        ..ComposerSnapshot::default()
    };
    let err = client
        .call(
            methods::CREATE_STUDIO_TURN,
            serde_json::json!({
                "conversationId": conversation.id,
                "prompt": "a comet",
                "composer": snapshot
            }),
        )
        .await
        .unwrap_err();
    match err {
        RpcError::Failed(message) => {
            assert!(
                message.contains("text-to-image") || message.contains("video"),
                "{message}"
            );
        }
        other => panic!("expected Failed video-submit gate, got {other:?}"),
    }
    engine.shutdown().await;
}

#[tokio::test]
async fn quote_studio_batch_composer_projects_and_evaluates() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(
        FakeMediaProvider::new(
            "fake",
            vec![image_model("fake")],
            FakeSubmissionMode::Complete(Vec::new()),
        )
        .with_quote(Quote::provider("USD", 0.42)),
    );
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let snapshot = ComposerSnapshot {
        prompt: "a comet".into(),
        selected: vec![SelectedModelRef::new("fake", "image-model")],
        ..ComposerSnapshot::default()
    };
    let quoted: zeron_proto::QuoteStudioBatchResponse = serde_json::from_value(
        client
            .call(
                methods::QUOTE_STUDIO_BATCH,
                serde_json::json!({
                    "prompt": "a comet",
                    "composer": snapshot
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let quote = quoted.runs[0].quote.clone().expect("live quote");
    assert_eq!(quote.source, QuoteSource::Provider);
    assert!((quote.amount - 0.42).abs() < f64::EPSILON);
    engine.shutdown().await;
}

#[tokio::test]
async fn import_studio_asset_assembles_chunks_and_is_idempotent() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(FakeMediaProvider::new(
        "fake",
        vec![image_model("fake")],
        FakeSubmissionMode::Complete(Vec::new()),
    ));
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let bytes = rgb_png(16, 12);
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let asset_id = StudioAssetId::new();
    let mid = bytes.len() / 2;
    let cont: ImportStudioAssetResponse = serde_json::from_value(
        client
            .call(
                methods::IMPORT_STUDIO_ASSET,
                serde_json::json!({
                    "assetId": asset_id,
                    "offset": 0,
                    "data": BASE64.encode(&bytes[..mid]),
                    "last": false
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    match cont {
        ImportStudioAssetResponse::Continue(chunk) => {
            assert_eq!(chunk.asset_id, asset_id);
            assert_eq!(chunk.next_offset, mid as u64);
        }
        other => panic!("expected continue, got {other:?}"),
    }
    let retry_first: ImportStudioAssetResponse = serde_json::from_value(
        client
            .call(
                methods::IMPORT_STUDIO_ASSET,
                serde_json::json!({
                    "assetId": asset_id,
                    "offset": 0,
                    "data": BASE64.encode(&bytes[..mid]),
                    "last": false
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    match retry_first {
        ImportStudioAssetResponse::Continue(chunk) => {
            assert_eq!(chunk.next_offset, mid as u64);
        }
        other => panic!("expected idempotent first-chunk retry, got {other:?}"),
    }
    let offset_err = client
        .call(
            methods::IMPORT_STUDIO_ASSET,
            serde_json::json!({
                "assetId": asset_id,
                "offset": 3,
                "data": BASE64.encode(&bytes[mid..]),
                "last": true,
                "expectedHash": hash
            }),
        )
        .await
        .unwrap_err();
    assert!(
        offset_err.to_string().contains("nextOffset"),
        "{offset_err:?}"
    );
    let first: ImportStudioAssetResponse = serde_json::from_value(
        client
            .call(
                methods::IMPORT_STUDIO_ASSET,
                serde_json::json!({
                    "assetId": asset_id,
                    "offset": mid,
                    "data": BASE64.encode(&bytes[mid..]),
                    "last": true,
                    "expectedHash": hash
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let ImportStudioAssetResponse::Complete(attachment) = first else {
        panic!("expected complete import");
    };
    assert!(!attachment.pending);
    assert_eq!(attachment.kind, ComposerMediaKind::Image);
    assert_eq!(attachment.mime_type, "image/png");
    assert_eq!(attachment.byte_size, bytes.len() as u64);
    assert_eq!(attachment.width, Some(16));
    assert_eq!(attachment.height, Some(12));
    assert_eq!(attachment.content_hash, hash);

    let retry: ImportStudioAssetResponse = serde_json::from_value(
        client
            .call(
                methods::IMPORT_STUDIO_ASSET,
                serde_json::json!({
                    "assetId": asset_id,
                    "offset": 0,
                    "data": BASE64.encode(&bytes),
                    "last": true,
                    "expectedHash": hash
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let ImportStudioAssetResponse::Complete(again) = retry else {
        panic!("expected idempotent complete");
    };
    assert_eq!(again, attachment);

    let different = rgb_png(8, 8);
    let different_hash = format!("{:x}", Sha256::digest(&different));
    let conflict = client
        .call(
            methods::IMPORT_STUDIO_ASSET,
            serde_json::json!({
                "assetId": asset_id,
                "offset": 0,
                "data": BASE64.encode(&different),
                "last": true,
                "expectedHash": different_hash
            }),
        )
        .await
        .unwrap_err();
    assert!(
        conflict.to_string().contains("different content hash"),
        "{conflict:?}"
    );
    engine.shutdown().await;
}

#[test]
fn import_studio_asset_rejects_the_64_mib_cap() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 128 * 1024 * 1024).unwrap();
    let asset_id = StudioAssetId::new();
    let too_big = vec![0u8; 64 * 1024 * 1024 + 1];
    let error = store
        .import_asset_chunk(asset_id, 0, &too_big, false, None, None)
        .unwrap_err();
    assert!(matches!(
        error,
        zeron_engine::StudioStoreError::ArtifactTooLarge
    ));
}

#[test]
fn import_studio_asset_cleans_staging_on_sniff_failure() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024 * 1024).unwrap();
    let asset_id = StudioAssetId::new();
    let garbage = b"not a media file";
    let hash = format!("{:x}", Sha256::digest(garbage));
    let error = store
        .import_asset_chunk(asset_id, 0, garbage, true, Some(&hash), None)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("studio input is not a supported media type"),
        "{error}"
    );
    let staging = root
        .path()
        .join("studio/inputs/tmp")
        .join(asset_id.0.to_string());
    assert!(!staging.exists());
}

#[test]
fn import_studio_asset_does_not_restage_after_commit() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024 * 1024).unwrap();
    let bytes = rgb_png(8, 8);
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let asset_id = StudioAssetId::new();
    store
        .import_asset_chunk(asset_id, 0, &bytes, true, Some(&hash), None)
        .unwrap();
    store
        .import_asset_chunk(asset_id, 0, b"xxxx", false, None, None)
        .unwrap();
    let staging = root
        .path()
        .join("studio/inputs/tmp")
        .join(asset_id.0.to_string());
    assert!(!staging.exists());
}

#[cfg(unix)]
#[test]
fn import_sweep_unlinks_tmp_symlink_without_following() {
    let root = tempdir().unwrap();
    let store = StudioStore::open(root.path(), 1024 * 1024).unwrap();
    let tmp = root.path().join("studio/inputs/tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    let decoy = root.path().join("decoy");
    let victim = decoy.join("keep-me");
    std::fs::create_dir_all(&victim).unwrap();
    std::os::unix::fs::symlink(&decoy, &tmp).unwrap();
    assert!(tmp.symlink_metadata().unwrap().file_type().is_symlink());

    let asset_id = StudioAssetId::new();
    let _ = store.import_asset_chunk(asset_id, 0, b"x", false, None, None);

    assert!(
        !tmp.symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink()),
        "sweep must unlink a tmp symlink instead of walking it"
    );
    assert!(
        victim.is_dir(),
        "sweep must not delete directories through a tmp symlink"
    );
}

#[tokio::test]
async fn quote_studio_batch_accepts_video_specs() {
    let root = tempdir().unwrap();
    let provider = std::sync::Arc::new(
        FakeMediaProvider::new(
            "fake",
            vec![seedance_t2v_model("fake")],
            FakeSubmissionMode::Complete(Vec::new()),
        )
        .with_quote(Quote::provider("USD", 1.25)),
    );
    let (engine, client) = studio_client_with_fake(root.path(), provider).await;
    let snapshot = ComposerSnapshot {
        mode: ComposerMode::Video,
        prompt: "a comet".into(),
        duration: Some(ControlValue::DurationSeconds { value: 6.0 }),
        selected: vec![SelectedModelRef::new("fake", "seedance-t2v")],
        ..ComposerSnapshot::default()
    };
    let quoted: zeron_proto::QuoteStudioBatchResponse = serde_json::from_value(
        client
            .call(
                methods::QUOTE_STUDIO_BATCH,
                serde_json::json!({
                    "prompt": "a comet",
                    "composer": snapshot
                }),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let quote = quoted.runs[0].quote.clone().expect("video quote");
    assert_eq!(quote.source, QuoteSource::Provider);
    assert!((quote.amount - 1.25).abs() < f64::EPSILON);
    engine.shutdown().await;
}
