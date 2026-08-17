use zeron_proto::{
    ExtendStudioTurnRequest, ListStudioArtifactsResponse, ListStudioProvidersResponse,
    ProviderValidationState, QuoteStudioBatchResponse, QuoteStudioRunView,
    ReadStudioArtifactChunkRequest, SetStudioProviderCredentialRequest,
    SetStudioProviderPreferencesRequest, StudioGalleryItem, StudioProviderBalanceResponse,
    StudioProviderConnection,
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
        }],
    };
    let json = serde_json::to_value(response).unwrap();
    assert!(json["artifacts"][0].get("conversationId").is_some());
    assert!(json["artifacts"][0].get("turnId").is_some());
    assert!(json["artifacts"][0].get("modelDisplayName").is_some());
    assert!(json["artifacts"][0].get("sizeBytes").is_some());
    assert!(json["artifacts"][0].get("createdAt").is_some());
    assert!(json["artifacts"][0].get("conversation_id").is_none());
}
