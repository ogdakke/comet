use std::collections::BTreeMap;

use chrono::Utc;
use zeron_studio::{
    ControlId, ControlKind, ControlValidationError, ControlValue, GenerationRequest, MediaKind,
    MediaModel, MediaOperation, ModelControl, RequestValidationError,
};

fn model() -> MediaModel {
    MediaModel {
        provider_id: "fake".into(),
        id: "image-model".into(),
        display_name: "Image model".to_owned(),
        description: None,
        operation: MediaOperation::TextToImage,
        output_kind: MediaKind::Image,
        output_mime_types: vec!["image/png".to_owned()],
        input_constraints: Vec::new(),
        prompt_maximum_chars: Some(20),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 4,
        controls: vec![ModelControl {
            id: "seed".into(),
            label: "Seed".to_owned(),
            description: None,
            kind: ControlKind::Integer,
            required: true,
            default: None,
            minimum: Some(0.0),
            maximum: Some(10.0),
            step: Some(1.0),
            choices: Vec::new(),
            visible_when: Vec::new(),
        }],
        pricing: None,
        manifest_version: "fixture-v1".to_owned(),
        fetched_at: Utc::now(),
    }
}

fn request() -> GenerationRequest {
    GenerationRequest {
        provider_id: "fake".into(),
        model_id: "image-model".into(),
        operation: MediaOperation::TextToImage,
        prompt: "a comet".to_owned(),
        negative_prompt: None,
        output_count: 1,
        controls: BTreeMap::from([(ControlId::new("seed"), ControlValue::Integer { value: 4 })]),
        inputs: Vec::new(),
        manifest_version: "fixture-v1".to_owned(),
        display_aspect_ratio: (1, 1),
    }
}

#[test]
fn accepts_a_request_that_matches_the_manifest() {
    request().validate_against(&model()).unwrap();
}

#[test]
fn rejects_unknown_controls_instead_of_forwarding_them() {
    let mut request = request();
    request.controls.insert(
        ControlId::new("provider_magic"),
        ControlValue::Boolean { value: true },
    );

    assert!(matches!(
        request.validate_against(&model()),
        Err(RequestValidationError::Control(
            ControlValidationError::UnknownControl { .. }
        ))
    ));
}

#[test]
fn drops_unknown_controls_before_rebinding_a_reused_form() {
    let mut request = request();
    request.controls.insert(
        ControlId::new("safe_mode"),
        ControlValue::Boolean { value: false },
    );

    request.drop_unknown_controls(&model());
    request.bind_to(&model()).unwrap();
    assert!(!request.controls.contains_key(&ControlId::new("safe_mode")));
}

#[test]
fn binds_a_compatible_request_to_the_current_manifest() {
    let mut request = request();
    request.manifest_version = "old".to_owned();
    request.bind_to(&model()).unwrap();
    assert_eq!(request.manifest_version, "fixture-v1");
}

#[test]
fn rejects_out_of_range_values_against_the_current_manifest() {
    let mut invalid = request();
    invalid.manifest_version = "old".to_owned();
    invalid
        .controls
        .insert(ControlId::new("seed"), ControlValue::Integer { value: 11 });
    assert!(matches!(
        invalid.bind_to(&model()),
        Err(RequestValidationError::Control(
            ControlValidationError::AboveMaximum { .. }
        ))
    ));
}
