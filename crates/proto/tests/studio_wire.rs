use zeron_proto::{
    AppendStudioDerivedRunRequest, ExtendStudioTurnRequest, ListStudioArtifactsResponse,
    ListStudioProvidersResponse, ProviderValidationState, QuoteStudioBatchResponse,
    QuoteStudioRunView, ReadStudioArtifactChunkRequest, SetStudioProviderCredentialRequest,
    SetStudioProviderPreferencesRequest, StudioConversationSummary, StudioGalleryItem,
    StudioProviderBalanceResponse, StudioProviderConnection, StudioRunState,
};
use zeron_studio::{
    AccountBalance, MediaKind, Quote, StudioArtifactId, StudioConversationId, StudioTurnId,
};

#[test]
fn provider_responses_have_no_secret_field() {
    let response = ListStudioProvidersResponse {
        providers: vec![StudioProviderConnection {
            provider_id: "venice".into(),
            display_label: "Venice".to_owned(),
            configured: true,
            validation_state: ProviderValidationState::Valid,
            validated_at: None,
            validation_message: None,
            safe_mode: false,
        }],
    };
    let json = serde_json::to_value(response).unwrap();
    assert!(json.to_string().find("secret").is_none());
}

#[test]
fn credential_request_uses_camel_case_wire_shape() {
    let request = SetStudioProviderCredentialRequest {
        provider_id: "venice".into(),
        display_label: "Personal".to_owned(),
        secret: "redacted-test-value".to_owned(),
    };
    let json = serde_json::to_value(request).unwrap();
    assert_eq!(json["providerId"], "venice");
    assert_eq!(json["displayLabel"], "Personal");
}

#[test]
fn provider_preferences_use_camel_case_wire_shape() {
    let request = SetStudioProviderPreferencesRequest {
        provider_id: "venice".into(),
        safe_mode: true,
    };
    let json = serde_json::to_value(request).unwrap();
    assert_eq!(json["providerId"], "venice");
    assert_eq!(json["safeMode"], true);
}

#[test]
fn artifact_reads_are_authorized_by_id_not_path() {
    let request = ReadStudioArtifactChunkRequest {
        artifact_id: StudioArtifactId::new(),
        offset: 42,
    };
    let value = serde_json::to_value(request).unwrap();
    assert!(value.get("artifactId").is_some());
    assert_eq!(
        value.get("offset").and_then(|value| value.as_u64()),
        Some(42)
    );
    assert!(value.get("path").is_none());
}

#[test]
fn extend_turn_uses_camel_case_wire_shape() {
    let request = ExtendStudioTurnRequest {
        turn_id: StudioTurnId::new(),
    };
    let json = serde_json::to_value(request).unwrap();
    assert!(json.get("turnId").is_some());
    assert!(json.get("turn_id").is_none());
}

#[test]
fn append_derived_run_uses_camel_case_wire_shape() {
    let request = AppendStudioDerivedRunRequest {
        source_artifact_id: StudioArtifactId::new(),
        prompt: "make the sky sunrise".into(),
        run: zeron_proto::StudioModelRunSpec {
            provider_id: "venice".into(),
            model_id: "firered-image-edit".into(),
            operation: zeron_studio::MediaOperation::ImageEdit,
            output_count: 1,
            controls: Default::default(),
            inputs: Vec::new(),
            manifest_version: "v1".into(),
            display_aspect_ratio: (1, 1),
        },
        mask_png_base64: None,
    };
    let json = serde_json::to_value(request).unwrap();
    assert!(json.get("sourceArtifactId").is_some());
    assert!(json.get("maskPngBase64").is_none());
    assert_eq!(json["run"]["operation"], "image_edit");
}

#[test]
fn provider_balance_uses_camel_case_wire_shape() {
    let response = StudioProviderBalanceResponse {
        balance: Some(AccountBalance {
            remaining: Quote::catalog("USD", 12.34),
        }),
    };
    let json = serde_json::to_value(response).unwrap();
    assert_eq!(json["balance"]["remaining"]["currency"], "USD");
    assert_eq!(json["balance"]["remaining"]["amount"], 12.34);
    assert!(json["balance"]["remaining"].get("expiresAt").is_some());
}

#[test]
fn quote_batch_uses_camel_case_wire_shape() {
    let response = QuoteStudioBatchResponse {
        runs: vec![QuoteStudioRunView {
            provider_id: "venice".into(),
            model_id: "gpt-image-2".into(),
            quote: Some(Quote::catalog("USD", 0.26)),
        }],
        total: Some(Quote::catalog("USD", 0.26)),
    };
    let json = serde_json::to_value(response).unwrap();
    assert_eq!(json["runs"][0]["providerId"], "venice");
    assert_eq!(json["runs"][0]["modelId"], "gpt-image-2");
    assert_eq!(json["runs"][0]["quote"]["amount"], 0.26);
    assert_eq!(json["runs"][0]["quote"]["source"], "catalog");
    assert_eq!(json["total"]["currency"], "USD");
    assert!(json["runs"][0]["quote"].get("expires_at").is_none());
    assert!(json["runs"][0]["quote"].get("expiresAt").is_some());
}

#[test]
fn conversation_summary_defaults_creating_to_false() {
    let json = serde_json::json!({
        "id": StudioConversationId::new(),
        "title": "Comet studies",
        "turnCount": 0,
        "createdAt": "2026-01-01T00:00:00Z",
        "updatedAt": "2026-01-01T00:00:00Z",
        "archived": false
    });
    let summary: StudioConversationSummary = serde_json::from_value(json).unwrap();
    assert!(!summary.creating);
    assert!(!summary.done);
}

#[test]
fn in_flight_run_states_count_as_creating() {
    assert!(StudioRunState::Queued.is_creating());
    assert!(StudioRunState::Running.is_creating());
    assert!(StudioRunState::Downloading.is_creating());
    assert!(StudioRunState::Cancelling.is_creating());
    assert!(!StudioRunState::Succeeded.is_creating());
    assert!(!StudioRunState::Failed.is_creating());
    assert!(!StudioRunState::Cancelled.is_creating());
}

#[test]
fn gallery_items_use_camel_case_wire_shape() {
    let response = ListStudioArtifactsResponse {
        artifacts: vec![StudioGalleryItem {
            id: StudioArtifactId::new(),
            conversation_id: StudioConversationId::new(),
            turn_id: StudioTurnId::new(),
            output_position: 0,
            media_kind: MediaKind::Image,
            mime_type: "image/png".into(),
            size_bytes: 12,
            width: Some(64),
            height: Some(64),
            prompt: "a comet".into(),
            model_display_name: "Image model".into(),
            created_at: chrono::Utc::now(),
            thumbhash: Some("3OcRJYB4d3h/iIeHeEh3eIhw+j3A".into()),
            source_artifact_id: None,
        }],
    };
    let json = serde_json::to_value(response).unwrap();
    assert!(json["artifacts"][0].get("conversationId").is_some());
    assert!(json["artifacts"][0].get("turnId").is_some());
    assert!(json["artifacts"][0].get("modelDisplayName").is_some());
    assert!(json["artifacts"][0].get("sizeBytes").is_some());
    assert!(json["artifacts"][0].get("createdAt").is_some());
    assert_eq!(
        json["artifacts"][0]
            .get("thumbhash")
            .and_then(|v| v.as_str()),
        Some("3OcRJYB4d3h/iIeHeEh3eIhw+j3A")
    );
    assert!(json["artifacts"][0].get("conversation_id").is_none());
}

#[test]
fn artifact_view_exposes_content_hash() {
    let artifact = zeron_proto::StudioArtifactView {
        id: StudioArtifactId::new(),
        output_position: 0,
        media_kind: MediaKind::Image,
        mime_type: "image/png".into(),
        size_bytes: 12,
        width: Some(64),
        height: Some(64),
        duration_seconds: None,
        metadata: serde_json::json!({}),
        created_at: chrono::Utc::now(),
        thumbhash: None,
        content_hash: "abc123".into(),
    };
    let json = serde_json::to_value(artifact).unwrap();
    assert_eq!(json["contentHash"], "abc123");
    assert!(json.get("content_hash").is_none());
}
