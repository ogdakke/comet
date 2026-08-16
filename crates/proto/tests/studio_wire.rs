use zeron_proto::{
    ListStudioProvidersResponse, ProviderValidationState, SetStudioProviderCredentialRequest,
    StudioProviderConnection,
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
