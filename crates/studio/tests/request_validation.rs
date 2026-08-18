use std::collections::BTreeMap;

use chrono::Utc;
use zeron_studio::{
    ControlId, ControlKind, ControlValidationError, ControlValue, GenerationInput,
    GenerationInputSource, GenerationRequest, InputConstraint, MediaKind, MediaModel,
    MediaOperation, MimeConstraint, ModelControl, RequestValidationError, StudioArtifactId,
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
        features: Vec::new(),
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

fn upscale_model() -> MediaModel {
    MediaModel {
        provider_id: "fake".into(),
        id: "upscaler".into(),
        display_name: "Upscaler".to_owned(),
        description: None,
        operation: MediaOperation::Upscale,
        output_kind: MediaKind::Image,
        output_mime_types: vec!["image/png".to_owned()],
        input_constraints: vec![InputConstraint {
            role: "source".into(),
            minimum_count: 1,
            maximum_count: 1,
            mime: MimeConstraint {
                accepted: vec!["image/png".to_owned()],
                maximum_bytes: Some(25 * 1024 * 1024),
                maximum_width: None,
                maximum_height: None,
            },
        }],
        prompt_maximum_chars: None,
        negative_prompt_maximum_chars: None,
        maximum_output_count: 1,
        controls: Vec::new(),
        pricing: None,
        features: Vec::new(),
        manifest_version: "fixture-v1".to_owned(),
        fetched_at: Utc::now(),
    }
}

fn source_input() -> GenerationInput {
    GenerationInput {
        role: "source".into(),
        ordinal: 0,
        source: GenerationInputSource::Artifact {
            artifact_id: StudioArtifactId::new(),
        },
        content_hash: "abc".to_owned(),
    }
}

fn upscale_request() -> GenerationRequest {
    GenerationRequest {
        provider_id: "fake".into(),
        model_id: "upscaler".into(),
        operation: MediaOperation::Upscale,
        prompt: String::new(),
        negative_prompt: None,
        output_count: 1,
        controls: BTreeMap::new(),
        inputs: vec![source_input()],
        manifest_version: "fixture-v1".to_owned(),
        display_aspect_ratio: (1, 1),
    }
}

#[test]
fn upscale_accepts_an_empty_prompt_and_one_source() {
    upscale_request()
        .validate_against(&upscale_model())
        .unwrap();
}

#[test]
fn upscale_rejects_a_missing_source() {
    let mut request = upscale_request();
    request.inputs.clear();
    assert!(matches!(
        request.validate_against(&upscale_model()),
        Err(RequestValidationError::InvalidInputCount { .. })
    ));
}

#[test]
fn upscale_rejects_an_extra_role() {
    let mut request = upscale_request();
    request.inputs.push(GenerationInput {
        role: "mask".into(),
        ordinal: 0,
        source: GenerationInputSource::Artifact {
            artifact_id: StudioArtifactId::new(),
        },
        content_hash: "def".to_owned(),
    });
    assert!(matches!(
        request.validate_against(&upscale_model()),
        Err(RequestValidationError::UnsupportedInputRole { .. })
    ));
}

#[test]
fn upscale_rejects_more_than_one_output() {
    let mut request = upscale_request();
    request.output_count = 2;
    assert!(matches!(
        request.validate_against(&upscale_model()),
        Err(RequestValidationError::InvalidOutputCount { .. })
    ));
}

fn edit_model() -> MediaModel {
    MediaModel {
        provider_id: "fake".into(),
        id: "image-edit".into(),
        display_name: "Edit".to_owned(),
        description: None,
        operation: MediaOperation::ImageEdit,
        output_kind: MediaKind::Image,
        output_mime_types: vec!["image/png".to_owned()],
        input_constraints: vec![
            InputConstraint {
                role: "source".into(),
                minimum_count: 1,
                maximum_count: 1,
                mime: MimeConstraint {
                    accepted: vec!["image/png".to_owned()],
                    maximum_bytes: Some(25 * 1024 * 1024),
                    maximum_width: None,
                    maximum_height: None,
                },
            },
            InputConstraint {
                role: "mask".into(),
                minimum_count: 0,
                maximum_count: 2,
                mime: MimeConstraint {
                    accepted: vec!["image/png".to_owned()],
                    maximum_bytes: Some(25 * 1024 * 1024),
                    maximum_width: None,
                    maximum_height: None,
                },
            },
        ],
        prompt_maximum_chars: Some(5000),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 1,
        controls: Vec::new(),
        pricing: None,
        features: Vec::new(),
        manifest_version: "fixture-v1".to_owned(),
        fetched_at: Utc::now(),
    }
}

fn edit_request() -> GenerationRequest {
    GenerationRequest {
        provider_id: "fake".into(),
        model_id: "image-edit".into(),
        operation: MediaOperation::ImageEdit,
        prompt: "change the sky".to_owned(),
        negative_prompt: None,
        output_count: 1,
        controls: BTreeMap::new(),
        inputs: vec![source_input()],
        manifest_version: "fixture-v1".to_owned(),
        display_aspect_ratio: (1, 1),
    }
}

#[test]
fn image_edit_accepts_a_source_without_a_mask() {
    edit_request().validate_against(&edit_model()).unwrap();
}

#[test]
fn image_edit_model_reports_whether_it_accepts_a_mask() {
    assert!(edit_model().accepts_mask());
    let mut model = edit_model();
    model
        .input_constraints
        .retain(|c| c.role.as_str() != "mask");
    assert!(!model.accepts_mask());
}

#[test]
fn image_edit_accepts_an_optional_mask() {
    let mut request = edit_request();
    request.inputs.push(GenerationInput {
        role: "mask".into(),
        ordinal: 0,
        source: GenerationInputSource::Asset {
            asset_id: zeron_studio::StudioAssetId::new(),
        },
        content_hash: "mask".to_owned(),
    });
    request.validate_against(&edit_model()).unwrap();
}

#[test]
fn image_edit_rejects_a_missing_source() {
    let mut request = edit_request();
    request.inputs.clear();
    assert!(matches!(
        request.validate_against(&edit_model()),
        Err(RequestValidationError::InvalidInputCount { .. })
    ));
}
