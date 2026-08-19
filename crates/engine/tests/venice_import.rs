use std::{fs, path::Path};

use tempfile::tempdir;
use uuid::Uuid;
use zeron_engine::{StudioStore, load_venice_image_dump};
use zeron_proto::StudioRunState;
use zeron_studio::{MediaKind, MediaModel, MediaOperation, ProviderId, StudioConversationId};

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

fn write_dump(root: &Path) -> (Uuid, Uuid, Uuid, Uuid) {
    let session = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
    let turn_a = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
    let turn_b = Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap();
    let media_a = Uuid::parse_str("44444444-4444-4444-4444-444444444444").unwrap();
    let media_b = Uuid::parse_str("55555555-5555-5555-5555-555555555555").unwrap();
    let media_c = Uuid::parse_str("66666666-6666-6666-6666-666666666666").unwrap();
    let session_dir = root.join("media").join(session.to_string());
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join(format!("{media_a}.png")), PNG).unwrap();
    fs::write(session_dir.join(format!("{media_b}.png")), PNG).unwrap();
    fs::write(session_dir.join(format!("{media_c}.png")), PNG).unwrap();
    let meta = serde_json::json!({
        "exportedAt": "2026-08-17T00:00:00Z",
        "source": "venice-db-encrypted",
        "counts": { "sessions": 1, "turns": 2, "media": 3 },
        "sessions": [{
            "id": session,
            "ownerId": "user_test",
            "type": "generate",
            "createdAtUnixTimestamp": 1_700_000_000_000i64,
            "updatedAtUnixTimestamp": 1_700_000_100_000i64,
            "name": "Venice Image Studio"
        }],
        "turns": [
            {
                "id": turn_a,
                "sessionId": session,
                "ownerId": "user_test",
                "createdAtUnixTimestamp": 1_700_000_010_000i64,
                "prompt": "a red balloon over a quiet street"
            },
            {
                "id": turn_b,
                "sessionId": session,
                "ownerId": "user_test",
                "createdAtUnixTimestamp": 1_700_000_020_000i64,
                "prompt": "the same balloon at dusk"
            }
        ],
        "media": [
            media_row(media_a, turn_a, session, "seedream-v5-pro", "Seedream V5 Pro", 1_700_000_011_000),
            media_row(media_b, turn_a, session, "qwen-image-3", "Qwen Image 3", 1_700_000_012_000),
            media_row(media_c, turn_b, session, "seedream-v5-pro", "Seedream V5 Pro", 1_700_000_021_000)
        ]
    });
    fs::write(
        root.join("venice-studio-image-meta.json"),
        serde_json::to_vec_pretty(&meta).unwrap(),
    )
    .unwrap();
    (session, turn_a, turn_b, media_a)
}

fn media_row(
    id: Uuid,
    turn_id: Uuid,
    session_id: Uuid,
    model_id: &str,
    model_name: &str,
    created: i64,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "turnId": turn_id,
        "sessionId": session_id,
        "ownerId": "user_test",
        "createdAtUnixTimestamp": created,
        "mimeType": "image/png",
        "fileName": format!("media/{session_id}/{id}.png"),
        "modelId": model_id,
        "modelName": model_name,
        "prompt": 0,
        "imageSettings": {
            "aspectRatio": "9:16",
            "width": 1440,
            "height": 2560,
            "seed": "42",
            "steps": 0,
            "resolution": "2K",
            "format": "png"
        },
        "mediaRole": "output",
        "source": "image"
    })
}

fn image_model(id: &str, name: &str) -> MediaModel {
    MediaModel {
        provider_id: ProviderId::from("venice"),
        id: id.into(),
        display_name: name.into(),
        description: None,
        operation: MediaOperation::TextToImage,
        output_kind: MediaKind::Image,
        output_mime_types: vec!["image/png".into(), "image/webp".into()],
        input_constraints: Vec::new(),
        prompt_maximum_chars: Some(1000),
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

#[test]
fn venice_image_dump_imports_sessions_as_conversations() {
    let dump = tempdir().unwrap();
    let (session, turn_a, _turn_b, media_a) = write_dump(dump.path());
    let catalog = [
        image_model("seedream-v5-pro", "Seedream V5 Pro"),
        image_model("qwen-image-3", "Qwen Image 3"),
    ];
    let history = load_venice_image_dump(dump.path(), Some(&catalog)).unwrap();
    assert_eq!(history.conversations.len(), 1);
    assert_eq!(history.conversations[0].turns.len(), 2);
    assert_eq!(history.conversations[0].turns[0].runs.len(), 2);

    let profile = tempdir().unwrap();
    let store = StudioStore::open(profile.path(), 1024 * 1024).unwrap();
    let report = store
        .import_completed_history(&history, "device-import")
        .unwrap();
    assert_eq!(report.conversations_imported, 1);
    assert_eq!(report.turns_imported, 2);
    assert_eq!(report.artifacts_imported, 3);
    assert_eq!(report.missing_files, 0);

    let view = store
        .conversation_view(StudioConversationId(session))
        .unwrap();
    assert_eq!(view.conversation.title, "a red balloon over a quiet street");
    assert_eq!(view.conversation.turn_count, 2);
    assert_eq!(view.turns[0].id.0, turn_a);
    assert_eq!(view.turns[0].prompt, "a red balloon over a quiet street");
    assert_eq!(view.turns[0].runs.len(), 2);
    assert!(
        view.turns[0]
            .runs
            .iter()
            .all(|run| run.state == StudioRunState::Succeeded)
    );
    assert_eq!(
        view.turns[0].created_at.timestamp_millis(),
        1_700_000_010_000
    );
    let artifact_path = profile
        .path()
        .join("studio/artifacts")
        .join(format!("{media_a}.png"));
    assert!(artifact_path.is_file());

    let again = store
        .import_completed_history(&history, "device-import")
        .unwrap();
    assert_eq!(again.conversations_imported, 0);
    assert_eq!(again.conversations_skipped, 1);
    assert_eq!(store.list_conversations(true).unwrap().len(), 1);
}

#[test]
fn venice_import_keeps_a_prompt_when_the_file_is_missing() {
    let dump = tempdir().unwrap();
    write_dump(dump.path());
    fs::remove_file(dump.path().join(
        "media/11111111-1111-1111-1111-111111111111/66666666-6666-6666-6666-666666666666.png",
    ))
    .unwrap();
    let history = load_venice_image_dump(dump.path(), None).unwrap();
    assert_eq!(history.missing_files, 1);
    let profile = tempdir().unwrap();
    let store = StudioStore::open(profile.path(), 1024 * 1024).unwrap();
    let report = store
        .import_completed_history(&history, "device-import")
        .unwrap();
    assert_eq!(report.artifacts_imported, 2);
    assert_eq!(report.missing_files, 1);
    let view = store
        .conversation_view(history.conversations[0].id)
        .unwrap();
    assert_eq!(view.turns[1].prompt, "the same balloon at dusk");
    assert_eq!(view.turns[1].runs[0].state, StudioRunState::Failed);
}
