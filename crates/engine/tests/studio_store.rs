use std::io::Read;

use tempfile::tempdir;
use zeron_engine::{
    EngineCore, EngineProfile, HarnessId, StudioProviderRegistry, StudioStore, default_registry,
};
use zeron_proto::{ListStudioConversationsResponse, StudioConversationSummary};
use zeron_rpc::{memory_client, methods};
use zeron_studio::{FakeMediaProvider, FakeSubmissionMode, ProviderId, StudioArtifactId};

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

    engine.shutdown().await;
}
