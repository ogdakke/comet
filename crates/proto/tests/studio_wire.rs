use zeron_proto::{
    ListStudioProvidersResponse, ProviderValidationState, QuoteStudioBatchResponse,
    QuoteStudioRunView, ReadStudioArtifactChunkRequest, SetStudioProviderCredentialRequest,
    SetStudioProviderPreferencesRequest, StudioProviderConnection,
};
use zeron_studio::{Quote, StudioArtifactId};

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
