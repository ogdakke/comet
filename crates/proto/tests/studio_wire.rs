use zeron_proto::{
    ListStudioProvidersResponse, ProviderValidationState, ReadStudioArtifactChunkRequest,
    SetStudioProviderCredentialRequest, StudioProviderConnection,
};
use zeron_studio::StudioArtifactId;

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
