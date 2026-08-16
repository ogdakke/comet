use std::{collections::BTreeMap, io::Read, sync::Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tempfile::tempdir;
use zeron_engine::studio::PreparedStudioRun;
use zeron_engine::{
    EngineCore, EngineProfile, HarnessId, StudioCredentialError, StudioCredentials,
    StudioProviderRegistry, StudioSecretBackend, StudioStore, default_registry,
};
use zeron_proto::{
    ListStudioConversationsResponse, ListStudioModelsResponse, ListStudioProvidersResponse,
    ProviderValidationState, StudioArtifactChunk, StudioConversationSummary,
    StudioConversationView, StudioProviderConnection, StudioRunState,
};
use zeron_rpc::{memory_client, methods};
use zeron_studio::{
    ControlKind, ControlValue, FakeMediaProvider, FakeSubmissionMode, GenerationRequest, MediaKind,
    MediaModel, MediaOperation, ModelControl, PricingMetadata, PricingUnit, ProviderArtifact,
    ProviderError, ProviderErrorKind, ProviderId, Quote, QuoteSource, Secret, StudioArtifactId,
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
    assert_eq!(version, 1);
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
    assert!(
        cached.stale,
        "pre-cost catalog snapshots must be refetched"
    );

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
