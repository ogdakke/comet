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
fn rejects_stale_manifest_and_out_of_range_values() {
    let mut stale = request();
    stale.manifest_version = "old".to_owned();
    assert_eq!(
        stale.validate_against(&model()),
        Err(RequestValidationError::StaleManifest)
    );

    let mut invalid = request();
    invalid
        .controls
        .insert(ControlId::new("seed"), ControlValue::Integer { value: 11 });
    assert!(matches!(
        invalid.validate_against(&model()),
        Err(RequestValidationError::Control(
            ControlValidationError::AboveMaximum { .. }
        ))
    ));
}
