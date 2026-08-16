//! Venice catalog normalization.
//!
//! Wire types stay private to this module. Only provider-neutral [`MediaModel`] manifests leave
//! the adapter boundary.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    ControlChoice, ControlId, ControlKind, ControlValue, InputConstraint, InputRole, MediaKind,
    MediaModel, MediaOperation, MimeConstraint, ModelControl, ModelId, PricingMetadata, ProviderId,
};

pub const VENICE_PROVIDER_ID: &str = "venice";

#[derive(Debug, thiserror::Error)]
pub enum VeniceCatalogError {
    #[error("invalid Venice model catalog: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("Venice model {model_id} is missing {field}")]
    MissingField {
        model_id: String,
        field: &'static str,
    },
    #[error("Venice model {model_id} has unsupported video model type {model_type}")]
    UnsupportedVideoType {
        model_id: String,
        model_type: String,
    },
    #[error("Venice model {model_id} has invalid aspect ratio {value}")]
    InvalidAspectRatio { model_id: String, value: String },
    #[error("Venice model {model_id} has invalid duration {value}")]
    InvalidDuration { model_id: String, value: String },
}

#[derive(Deserialize)]
struct CatalogResponse {
    data: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    id: String,
    #[serde(rename = "type")]
    media_type: String,
    model_spec: ModelSpec,
}

#[derive(Deserialize)]
struct ModelSpec {
    name: Option<String>,
    description: Option<String>,
    constraints: serde_json::Value,
    pricing: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageConstraints {
    prompt_character_limit: u32,
    steps: StepConstraints,
    #[serde(default)]
    aspect_ratios: Vec<String>,
    default_aspect_ratio: Option<String>,
    #[serde(default)]
    resolutions: Vec<String>,
    default_resolution: Option<String>,
    #[serde(default)]
    qualities: Vec<String>,
    default_quality: Option<String>,
}

#[derive(Deserialize)]
struct StepConstraints {
    default: i64,
    max: i64,
}

#[derive(Deserialize)]
struct VideoConstraints {
    model_type: String,
    #[serde(default)]
    aspect_ratios: Vec<String>,
    #[serde(default)]
    resolutions: Vec<String>,
    #[serde(default)]
    durations: Vec<String>,
    audio: bool,
    audio_configurable: bool,
    #[serde(default = "default_video_prompt_limit")]
    prompt_character_limit: u32,
}

fn default_video_prompt_limit() -> u32 {
    2_500
}

/// Normalize one or more Venice `GET /api/v1/models` responses.
///
/// Non-Studio model types are ignored so callers may safely pass an unfiltered catalog. Unknown
/// video operation types fail closed rather than being guessed from the model ID.
pub fn normalize_model_catalog(
    json: &[u8],
    fetched_at: DateTime<Utc>,
) -> Result<Vec<MediaModel>, VeniceCatalogError> {
    let response: CatalogResponse = serde_json::from_slice(json)?;
    response
        .data
        .into_iter()
        .filter(|model| matches!(model.media_type.as_str(), "image" | "video"))
        .map(|model| normalize_model(model, fetched_at))
        .collect()
}

fn normalize_model(
    model: CatalogModel,
    fetched_at: DateTime<Utc>,
) -> Result<MediaModel, VeniceCatalogError> {
    match model.media_type.as_str() {
        "image" => normalize_image(model, fetched_at),
        "video" => normalize_video(model, fetched_at),
        _ => unreachable!("caller filters non-media model types"),
    }
}

fn normalize_image(
    model: CatalogModel,
    fetched_at: DateTime<Utc>,
) -> Result<MediaModel, VeniceCatalogError> {
    let constraints: ImageConstraints = serde_json::from_value(model.model_spec.constraints)?;
    let mut controls = Vec::new();

    if !constraints.aspect_ratios.is_empty() {
        controls.push(aspect_ratio_control(
            &model.id,
            &constraints.aspect_ratios,
            constraints.default_aspect_ratio.as_deref(),
        )?);
    }
    if !constraints.resolutions.is_empty() {
        controls.push(choice_control(
            "resolution",
            "Resolution",
            ControlKind::Resolution,
            &constraints.resolutions,
            constraints.default_resolution.as_deref(),
        ));
    }
    if !constraints.qualities.is_empty() {
        controls.push(choice_control(
            "quality",
            "Quality",
            ControlKind::Enum,
            &constraints.qualities,
            constraints.default_quality.as_deref(),
        ));
    }
    controls.push(ModelControl {
        id: ControlId::from("steps"),
        label: "Steps".to_owned(),
        description: Some("Number of inference steps".to_owned()),
        kind: ControlKind::Integer,
        required: false,
        default: Some(ControlValue::Integer {
            value: constraints.steps.default,
        }),
        minimum: Some(1.0),
        maximum: Some(constraints.steps.max as f64),
        step: Some(1.0),
        choices: Vec::new(),
        visible_when: Vec::new(),
    });
    controls.push(choice_control(
        "format",
        "Format",
        ControlKind::Enum,
        &["webp".to_owned(), "png".to_owned(), "jpeg".to_owned()],
        Some("webp"),
    ));

    finish_manifest(MediaModel {
        provider_id: ProviderId::from(VENICE_PROVIDER_ID),
        id: ModelId::new(model.id),
        display_name: model
            .model_spec
            .name
            .unwrap_or_else(|| "Venice image model".to_owned()),
        description: model.model_spec.description,
        operation: MediaOperation::TextToImage,
        output_kind: MediaKind::Image,
        output_mime_types: vec![
            "image/webp".to_owned(),
            "image/png".to_owned(),
            "image/jpeg".to_owned(),
        ],
        input_constraints: Vec::new(),
        prompt_maximum_chars: Some(constraints.prompt_character_limit),
        negative_prompt_maximum_chars: Some(constraints.prompt_character_limit),
        maximum_output_count: 4,
        controls,
        pricing: pricing_metadata(model.model_spec.pricing.as_ref()),
        manifest_version: String::new(),
        fetched_at,
    })
}

fn normalize_video(
    model: CatalogModel,
    fetched_at: DateTime<Utc>,
) -> Result<MediaModel, VeniceCatalogError> {
    let constraints: VideoConstraints = serde_json::from_value(model.model_spec.constraints)?;
    let (operation, input_constraints) = match constraints.model_type.as_str() {
        "text-to-video" => (MediaOperation::TextToVideo, Vec::new()),
        "image-to-video" => (
            MediaOperation::ImageToVideo,
            vec![input_constraint(
                "source",
                &["image/jpeg", "image/png", "image/webp"],
            )],
        ),
        "video" => (
            MediaOperation::VideoToVideo,
            vec![input_constraint(
                "source",
                &["video/mp4", "video/quicktime", "video/webm"],
            )],
        ),
        model_type => {
            return Err(VeniceCatalogError::UnsupportedVideoType {
                model_id: model.id,
                model_type: model_type.to_owned(),
            });
        }
    };

    let mut controls = Vec::new();
    if !constraints.aspect_ratios.is_empty() {
        controls.push(aspect_ratio_control(
            &model.id,
            &constraints.aspect_ratios,
            None,
        )?);
    }
    if !constraints.resolutions.is_empty() {
        controls.push(choice_control(
            "resolution",
            "Resolution",
            ControlKind::Resolution,
            &constraints.resolutions,
            None,
        ));
    }
    if !constraints.durations.is_empty() {
        controls.push(duration_control(&model.id, &constraints.durations)?);
    }
    if constraints.audio && constraints.audio_configurable {
        controls.push(ModelControl {
            id: ControlId::from("audio"),
            label: "Generate audio".to_owned(),
            description: None,
            kind: ControlKind::Boolean,
            required: false,
            default: Some(ControlValue::Boolean { value: true }),
            minimum: None,
            maximum: None,
            step: None,
            choices: Vec::new(),
            visible_when: Vec::new(),
        });
    }

    finish_manifest(MediaModel {
        provider_id: ProviderId::from(VENICE_PROVIDER_ID),
        id: ModelId::new(model.id),
        display_name: model
            .model_spec
            .name
            .unwrap_or_else(|| "Venice video model".to_owned()),
        description: model.model_spec.description,
        operation,
        output_kind: MediaKind::Video,
        output_mime_types: vec!["video/mp4".to_owned()],
        input_constraints,
        prompt_maximum_chars: Some(constraints.prompt_character_limit),
        negative_prompt_maximum_chars: Some(constraints.prompt_character_limit),
        maximum_output_count: 1,
        controls,
        pricing: pricing_metadata(model.model_spec.pricing.as_ref()),
        manifest_version: String::new(),
        fetched_at,
    })
}

fn aspect_ratio_control(
    model_id: &str,
    values: &[String],
    default: Option<&str>,
) -> Result<ModelControl, VeniceCatalogError> {
    let choices = values
        .iter()
        // Venice uses `auto` to mean "omit aspect_ratio and let the model decide". It is not a
        // geometric ratio and must not poison the rest of an otherwise valid live catalog.
        .filter(|value| value.as_str() != "auto")
        .map(|value| {
            let (width, height) =
                value
                    .split_once(':')
                    .ok_or_else(|| VeniceCatalogError::InvalidAspectRatio {
                        model_id: model_id.to_owned(),
                        value: value.clone(),
                    })?;
            let width = width
                .parse()
                .map_err(|_| VeniceCatalogError::InvalidAspectRatio {
                    model_id: model_id.to_owned(),
                    value: value.clone(),
                })?;
            let height = height
                .parse()
                .map_err(|_| VeniceCatalogError::InvalidAspectRatio {
                    model_id: model_id.to_owned(),
                    value: value.clone(),
                })?;
            Ok(ControlChoice {
                value: ControlValue::AspectRatio { width, height },
                label: value.clone(),
            })
        })
        .collect::<Result<Vec<_>, VeniceCatalogError>>()?;
    let default = default
        .filter(|value| *value != "auto")
        .and_then(|value| choices.iter().find(|choice| choice.label == value))
        .map(|choice| choice.value.clone());
    Ok(ModelControl {
        id: ControlId::from("aspect_ratio"),
        label: "Aspect ratio".to_owned(),
        description: None,
        kind: ControlKind::AspectRatio,
        required: false,
        default,
        minimum: None,
        maximum: None,
        step: None,
        choices,
        visible_when: Vec::new(),
    })
}

fn duration_control(model_id: &str, values: &[String]) -> Result<ModelControl, VeniceCatalogError> {
    let choices = values
        .iter()
        .map(|value| {
            let seconds =
                value
                    .strip_suffix('s')
                    .ok_or_else(|| VeniceCatalogError::InvalidDuration {
                        model_id: model_id.to_owned(),
                        value: value.clone(),
                    })?;
            let seconds = seconds
                .parse()
                .map_err(|_| VeniceCatalogError::InvalidDuration {
                    model_id: model_id.to_owned(),
                    value: value.clone(),
                })?;
            Ok(ControlChoice {
                value: ControlValue::DurationSeconds { value: seconds },
                label: value.clone(),
            })
        })
        .collect::<Result<Vec<_>, VeniceCatalogError>>()?;
    Ok(ModelControl {
        id: ControlId::from("duration"),
        label: "Duration".to_owned(),
        description: None,
        kind: ControlKind::Duration,
        required: true,
        default: None,
        minimum: None,
        maximum: None,
        step: None,
        choices,
        visible_when: Vec::new(),
    })
}

fn choice_control(
    id: &'static str,
    label: &'static str,
    kind: ControlKind,
    values: &[String],
    default: Option<&str>,
) -> ModelControl {
    let make_value = |value: &str| match kind {
        ControlKind::Resolution => ControlValue::Resolution {
            value: value.to_owned(),
        },
        _ => ControlValue::Enum {
            value: value.to_owned(),
        },
    };
    ModelControl {
        id: ControlId::from(id),
        label: label.to_owned(),
        description: None,
        kind,
        required: false,
        default: default.map(make_value),
        minimum: None,
        maximum: None,
        step: None,
        choices: values
            .iter()
            .map(|value| ControlChoice {
                value: make_value(value),
                label: value.clone(),
            })
            .collect(),
        visible_when: Vec::new(),
    }
}

fn input_constraint(role: &'static str, mime_types: &[&str]) -> InputConstraint {
    InputConstraint {
        role: InputRole::from(role),
        minimum_count: 1,
        maximum_count: 1,
        mime: MimeConstraint {
            accepted: mime_types.iter().map(|value| (*value).to_owned()).collect(),
            maximum_bytes: None,
            maximum_width: None,
            maximum_height: None,
        },
    }
}

fn pricing_metadata(pricing: Option<&serde_json::Value>) -> Option<PricingMetadata> {
    pricing.map(|_| PricingMetadata {
        currency: "USD".to_owned(),
        unit_label: "provider-defined generation".to_owned(),
        amount: None,
        detail: Some(
            "Price varies with the selected model controls; request a quote when supported"
                .to_owned(),
        ),
    })
}

fn finish_manifest(mut model: MediaModel) -> Result<MediaModel, VeniceCatalogError> {
    let mut submit_relevant = serde_json::to_value(&model)?;
    if let Some(object) = submit_relevant.as_object_mut() {
        object.remove("fetched_at");
        object.remove("manifest_version");
    }
    let bytes = serde_json::to_vec(&submit_relevant)?;
    let digest = Sha256::digest(bytes);
    model.manifest_version = format!("venice-v1:{digest:x}");
    Ok(model)
}
