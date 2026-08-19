//! Venice catalog normalization.
//!
//! Wire types stay private to this module. Only provider-neutral [`MediaModel`] manifests leave
//! the adapter boundary.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    AdapterFamily, AudioCapability, ControlChoice, ControlId, ControlKind, ControlValue,
    InputConstraint, InputRole, MediaKind, MediaModel, MediaOperation, MimeConstraint,
    ModelControl, ModelFeature, ModelId, PricingEntry, PricingMetadata, PricingUnit, ProviderId,
    ROLE_SOURCE, VideoModelMeta,
    venice_overlay::{ImageGeometry, OverlayError, VeniceVideoOverlay, bundled_video_overlay},
};

pub const VENICE_PROVIDER_ID: &str = "venice";

/// Live `model_spec.constraints` keys the video parser understands.
/// Drift CI fails if a checked-in fixture introduces a key outside this set.
pub const VIDEO_CONSTRAINT_KEYS: &[&str] = &[
    "model_type",
    "aspect_ratios",
    "resolutions",
    "durations",
    "audio",
    "audio_configurable",
    "audio_input",
    "per_reference_audio",
    "video_input",
    "prompt_character_limit",
    "reference_image_min_short_side_pixels",
    "reference_image_min_aspect_ratio",
    "reference_image_max_aspect_ratio",
];

/// Overlay `source` values the design allows. Anything else is drift.
pub const ALLOWED_OVERLAY_SOURCES: &[&str] = &[
    "live fixture + swagger",
    "https://docs.venice.ai/guides/media/seedance-2-0",
    "https://docs.venice.ai/guides/media/reference-to-video",
    "https://docs.venice.ai/api-reference/endpoint/video/queue",
];

/// Constraint keys present on a live model object that the parser does not know.
pub fn unknown_video_constraint_keys(constraints: &serde_json::Value) -> Vec<String> {
    let Some(object) = constraints.as_object() else {
        return Vec::new();
    };
    object
        .keys()
        .filter(|key| !VIDEO_CONSTRAINT_KEYS.contains(&key.as_str()))
        .cloned()
        .collect()
}

const DEFAULT_IMAGE_OUTPUT_MIMES: &[&str] = &["image/webp", "image/png", "image/jpeg"];
/// Venice `POST /image/generate` documents these `format` values. Used when a
/// model honors that field and the catalog does not list a narrower set.
const ENDPOINT_IMAGE_FORMATS: &[&str] = &["webp", "png", "jpeg"];

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
    #[error("Venice model {model_id} has invalid duration {value}")]
    InvalidDuration { model_id: String, value: String },
    #[error(transparent)]
    Overlay(#[from] OverlayError),
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
    #[serde(default)]
    constraints: serde_json::Value,
    pricing: Option<serde_json::Value>,
    #[serde(default, rename = "supportsOptimizePromptThinking")]
    supports_optimize_prompt_thinking: bool,
    #[serde(default)]
    privacy: Option<String>,
    #[serde(default)]
    uncensored: bool,
    #[serde(default)]
    traits: Vec<String>,
    #[serde(default)]
    model_sets: Vec<String>,
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
    #[serde(default)]
    formats: Vec<String>,
    default_format: Option<String>,
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
    #[serde(default)]
    audio_input: bool,
    #[serde(default)]
    video_input: bool,
    #[serde(default)]
    per_reference_audio: bool,
    #[serde(default = "default_video_prompt_limit")]
    prompt_character_limit: u32,
    #[serde(default)]
    reference_image_min_short_side_pixels: Option<u32>,
    #[serde(default)]
    reference_image_min_aspect_ratio: Option<f64>,
    #[serde(default)]
    reference_image_max_aspect_ratio: Option<f64>,
}

fn default_video_prompt_limit() -> u32 {
    2_500
}

/// Normalize one or more Venice `GET /api/v1/models` responses.
///
/// Non-Studio model types are ignored so callers may safely pass an unfiltered catalog. Unknown
/// video operation types and models with unusable constraints are skipped rather than failing the
/// whole catalog — one vendor row must not mark the provider unavailable.
pub fn normalize_model_catalog(
    json: &[u8],
    fetched_at: DateTime<Utc>,
) -> Result<Vec<MediaModel>, VeniceCatalogError> {
    let overlay = bundled_video_overlay()?;
    let response: CatalogResponse = serde_json::from_slice(json)?;
    Ok(response
        .data
        .into_iter()
        .filter(|model| {
            matches!(
                model.media_type.as_str(),
                "image" | "video" | "upscale" | "inpaint"
            )
        })
        .filter_map(|model| normalize_model(model, fetched_at, &overlay).ok())
        .collect())
}

fn normalize_model(
    model: CatalogModel,
    fetched_at: DateTime<Utc>,
    overlay: &VeniceVideoOverlay,
) -> Result<MediaModel, VeniceCatalogError> {
    match model.media_type.as_str() {
        "image" => normalize_image(model, fetched_at),
        "video" => normalize_video(model, fetched_at, overlay),
        "upscale" => normalize_upscale(model, fetched_at),
        "inpaint" => normalize_inpaint(model, fetched_at),
        _ => unreachable!("caller filters non-media model types"),
    }
}

fn normalize_image(
    model: CatalogModel,
    fetched_at: DateTime<Utc>,
) -> Result<MediaModel, VeniceCatalogError> {
    let features = model_features(&model.model_spec);
    let constraints: ImageConstraints = serde_json::from_value(model.model_spec.constraints)?;
    let mut controls = Vec::new();

    if let Some(control) = aspect_ratio_control(
        &constraints.aspect_ratios,
        constraints.default_aspect_ratio.as_deref(),
    ) {
        controls.push(control);
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
    if model.model_spec.supports_optimize_prompt_thinking {
        controls.push(ModelControl {
            id: ControlId::from("reasoning"),
            label: "Reasoning".to_owned(),
            description: Some("Use provider-supported prompt optimization reasoning".to_owned()),
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
    // Submitted at the catalog default. Composer does not surface a knob:
    // hosted models ignore `steps`, and the few diffusion models already
    // ship a tuned default.
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
    let formats = image_format_choices(&model.id, &constraints.formats);
    if !formats.is_empty() {
        let default = constraints
            .default_format
            .as_deref()
            .and_then(normalize_image_format)
            .filter(|value| formats.iter().any(|choice| choice == value))
            .or_else(|| {
                formats
                    .iter()
                    .find(|value| value.as_str() == "webp")
                    .cloned()
            })
            .or_else(|| formats.first().cloned());
        controls.push(choice_control(
            "format",
            "Format",
            ControlKind::Enum,
            &formats,
            default.as_deref(),
        ));
    }
    // Endpoint-level: Venice defaults this to true and returns a blurred placeholder
    // for anything it classifies as adult content. Expose it so the chosen value is
    // persisted on the job, and default it off so generations receive the original.
    controls.push(ModelControl {
        id: ControlId::from("safe_mode"),
        label: "Safe mode".to_owned(),
        description: Some(
            "Blur images Venice classifies as adult content. Off by default.".to_owned(),
        ),
        kind: ControlKind::Boolean,
        required: false,
        default: Some(ControlValue::Boolean { value: false }),
        minimum: None,
        maximum: None,
        step: None,
        choices: Vec::new(),
        visible_when: Vec::new(),
    });

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
        output_mime_types: image_output_mime_types(&formats),
        input_constraints: Vec::new(),
        prompt_maximum_chars: Some(constraints.prompt_character_limit),
        negative_prompt_maximum_chars: Some(constraints.prompt_character_limit),
        maximum_output_count: 4,
        controls,
        pricing: pricing_metadata(model.model_spec.pricing.as_ref()),
        features,
        video: VideoModelMeta::default(),
        manifest_version: String::new(),
        fetched_at,
    })
}

const UPSCALE_MAX_INPUT_BYTES: u64 = 25 * 1024 * 1024;
/// Venice `/image/edit` and `/image/multi-edit` share this upload cap.
const INPAINT_MAX_INPUT_BYTES: u64 = 25 * 1024 * 1024;
/// When `combineImages` is true but `maxInputImages` is omitted, the
/// multi-edit guide documents a 1–3 image envelope (source + up to two layers).
const DEFAULT_INPAINT_INPUT_IMAGES: u32 = 3;

fn normalize_upscale(
    model: CatalogModel,
    fetched_at: DateTime<Utc>,
) -> Result<MediaModel, VeniceCatalogError> {
    if model.id.trim().is_empty() {
        return Err(VeniceCatalogError::MissingField {
            model_id: model.id,
            field: "id",
        });
    }
    let features = model_features(&model.model_spec);
    let mut input = input_constraint("source", &["image/jpeg", "image/png", "image/webp"]);
    input.mime.maximum_bytes = Some(UPSCALE_MAX_INPUT_BYTES);

    let scale_choices = [2_i64, 4];
    let controls = vec![
        ModelControl {
            id: ControlId::from("scale"),
            label: "Scale".to_owned(),
            description: Some(
                "Upscale factor. 4x may be reduced so the output stays within Venice size limits."
                    .to_owned(),
            ),
            kind: ControlKind::Integer,
            required: true,
            default: Some(ControlValue::Integer { value: 2 }),
            minimum: Some(2.0),
            maximum: Some(4.0),
            step: Some(2.0),
            choices: scale_choices
                .iter()
                .map(|value| ControlChoice {
                    value: ControlValue::Integer { value: *value },
                    label: format!("{value}x"),
                })
                .collect(),
            visible_when: Vec::new(),
        },
        ModelControl {
            id: ControlId::from("creativity"),
            label: "Creativity".to_owned(),
            description: Some(
                "How much detail the upscaler invents. Lower stays closer to the source."
                    .to_owned(),
            ),
            kind: ControlKind::Number,
            required: false,
            default: Some(ControlValue::Number { value: 0.01 }),
            minimum: Some(0.0),
            maximum: Some(0.02),
            step: Some(0.001),
            choices: Vec::new(),
            visible_when: Vec::new(),
        },
    ];

    finish_manifest(MediaModel {
        provider_id: ProviderId::from(VENICE_PROVIDER_ID),
        id: ModelId::new(model.id),
        display_name: model
            .model_spec
            .name
            .unwrap_or_else(|| "Venice upscaler".to_owned()),
        description: model.model_spec.description,
        operation: MediaOperation::Upscale,
        output_kind: MediaKind::Image,
        output_mime_types: vec!["image/png".to_owned()],
        input_constraints: vec![input],
        prompt_maximum_chars: None,
        negative_prompt_maximum_chars: None,
        maximum_output_count: 1,
        controls,
        pricing: upscale_pricing_metadata(model.model_spec.pricing.as_ref()),
        features,
        video: VideoModelMeta::default(),
        manifest_version: String::new(),
        fetched_at,
    })
}

/// Venice `GET /models?type=inpaint` rows. They do **not** carry generate
/// `steps`. Extra multi-edit images are "edit layers/masks": we send a
/// source-resolution PNG with opaque white = region to edit and black = keep.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InpaintConstraints {
    prompt_character_limit: u32,
    #[serde(default)]
    aspect_ratios: Vec<String>,
    default_aspect_ratio: Option<String>,
    #[serde(default)]
    combine_images: bool,
    max_input_images: Option<u32>,
    #[serde(default)]
    resolutions: Vec<String>,
    default_resolution: Option<String>,
    #[serde(default)]
    qualities: Vec<String>,
    default_quality: Option<String>,
    #[serde(default)]
    formats: Vec<String>,
    default_format: Option<String>,
}

fn normalize_inpaint(
    model: CatalogModel,
    fetched_at: DateTime<Utc>,
) -> Result<MediaModel, VeniceCatalogError> {
    if model.id.trim().is_empty() {
        return Err(VeniceCatalogError::MissingField {
            model_id: model.id,
            field: "id",
        });
    }
    let features = model_features(&model.model_spec);
    let constraints: InpaintConstraints = serde_json::from_value(model.model_spec.constraints)?;
    let mut source = input_constraint("source", &["image/jpeg", "image/png", "image/webp"]);
    source.mime.maximum_bytes = Some(INPAINT_MAX_INPUT_BYTES);
    let mut input_constraints = vec![source];
    if constraints.combine_images {
        let extra = constraints
            .max_input_images
            .unwrap_or(DEFAULT_INPAINT_INPUT_IMAGES)
            .saturating_sub(1)
            .max(1);
        let mut mask = input_constraint("mask", &["image/jpeg", "image/png", "image/webp"]);
        mask.minimum_count = 0;
        mask.maximum_count = extra;
        mask.mime.maximum_bytes = Some(INPAINT_MAX_INPUT_BYTES);
        input_constraints.push(mask);
    }

    let mut controls = Vec::new();
    if let Some(control) = aspect_ratio_control(
        &constraints.aspect_ratios,
        constraints.default_aspect_ratio.as_deref(),
    ) {
        controls.push(control);
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
    if model.model_spec.supports_optimize_prompt_thinking {
        controls.push(ModelControl {
            id: ControlId::from("reasoning"),
            label: "Reasoning".to_owned(),
            description: Some("Use provider-supported prompt optimization reasoning".to_owned()),
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
    let formats = image_format_choices(&model.id, &constraints.formats);
    if !formats.is_empty() {
        let default = constraints
            .default_format
            .as_deref()
            .and_then(normalize_image_format)
            .filter(|value| formats.iter().any(|choice| choice == value))
            .or_else(|| {
                formats
                    .iter()
                    .find(|value| value.as_str() == "webp")
                    .cloned()
            })
            .or_else(|| formats.first().cloned());
        controls.push(choice_control(
            "format",
            "Format",
            ControlKind::Enum,
            &formats,
            default.as_deref(),
        ));
    }
    controls.push(ModelControl {
        id: ControlId::from("safe_mode"),
        label: "Safe mode".to_owned(),
        description: Some(
            "Blur images Venice classifies as adult content. Off by default.".to_owned(),
        ),
        kind: ControlKind::Boolean,
        required: false,
        default: Some(ControlValue::Boolean { value: false }),
        minimum: None,
        maximum: None,
        step: None,
        choices: Vec::new(),
        visible_when: Vec::new(),
    });

    finish_manifest(MediaModel {
        provider_id: ProviderId::from(VENICE_PROVIDER_ID),
        id: ModelId::new(model.id),
        display_name: model
            .model_spec
            .name
            .unwrap_or_else(|| "Venice edit model".to_owned()),
        description: model.model_spec.description,
        operation: MediaOperation::ImageEdit,
        output_kind: MediaKind::Image,
        output_mime_types: image_output_mime_types(&formats),
        input_constraints,
        prompt_maximum_chars: Some(constraints.prompt_character_limit),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 1,
        controls,
        pricing: inpaint_pricing_metadata(model.model_spec.pricing.as_ref()),
        features,
        video: VideoModelMeta::default(),
        manifest_version: String::new(),
        fetched_at,
    })
}

fn normalize_video(
    model: CatalogModel,
    fetched_at: DateTime<Utc>,
    overlay: &VeniceVideoOverlay,
) -> Result<MediaModel, VeniceCatalogError> {
    let features = model_features(&model.model_spec);
    let constraints: VideoConstraints = serde_json::from_value(model.model_spec.constraints)?;
    // Parsed so live extras are not dropped. Counts/roles come from the overlay.
    let _ = (
        constraints.audio_input,
        constraints.video_input,
        constraints.per_reference_audio,
    );
    let geometry = ImageGeometry {
        minimum_short_side: constraints.reference_image_min_short_side_pixels,
        minimum_aspect_ratio: constraints.reference_image_min_aspect_ratio,
        maximum_aspect_ratio: constraints.reference_image_max_aspect_ratio,
    };
    let (operation, input_constraints) = match constraints.model_type.as_str() {
        "text-to-video" => (MediaOperation::TextToVideo, Vec::new()),
        "image-to-video" => {
            let mut source =
                input_constraint(ROLE_SOURCE, &["image/jpeg", "image/png", "image/webp"]);
            source.mime.minimum_short_side = geometry.minimum_short_side;
            source.mime.minimum_aspect_ratio = geometry.minimum_aspect_ratio;
            source.mime.maximum_aspect_ratio = geometry.maximum_aspect_ratio;
            (MediaOperation::ImageToVideo, vec![source])
        }
        "video" => (
            MediaOperation::VideoToVideo,
            vec![input_constraint(
                ROLE_SOURCE,
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
    if let Some(control) = aspect_ratio_control(&constraints.aspect_ratios, None) {
        controls.push(control);
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
    let generate_audio = if constraints.audio && constraints.audio_configurable {
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
        AudioCapability::Configurable { default: true }
    } else if constraints.audio {
        AudioCapability::ForcedOn
    } else {
        AudioCapability::None
    };

    let live_model_type = constraints.model_type.clone();
    let mut model = MediaModel {
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
        features,
        video: VideoModelMeta {
            adapter_family: AdapterFamily::Hidden,
            generate_audio,
            ..VideoModelMeta::default()
        },
        manifest_version: String::new(),
        fetched_at,
    };
    overlay.apply(&mut model, &live_model_type, geometry)?;
    finish_manifest(model)
}

fn model_features(spec: &ModelSpec) -> Vec<ModelFeature> {
    let mut features = Vec::new();
    let uncensored = spec.uncensored
        || spec
            .traits
            .iter()
            .any(|value| value.eq_ignore_ascii_case("uncensored"))
        || spec
            .model_sets
            .iter()
            .any(|value| value.eq_ignore_ascii_case("uncensored"));
    if uncensored {
        features.push(ModelFeature::Uncensored);
    }
    match spec
        .privacy
        .as_deref()
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("anonymized" | "anonymised" | "anonymous" | "anon") => {
            features.push(ModelFeature::Anon);
        }
        Some("private") => {
            features.push(ModelFeature::Private);
        }
        _ => {}
    }
    features
}

fn parse_aspect_ratio(value: &str) -> Option<ControlValue> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("auto") {
        return Some(ControlValue::AspectRatioAuto);
    }
    if value.eq_ignore_ascii_case("adaptive") {
        return Some(ControlValue::AspectRatioAdaptive);
    }
    let (width, height) = value.split_once(':')?;
    let width = width.parse().ok()?;
    let height = height.parse().ok()?;
    (width > 0 && height > 0).then_some(ControlValue::AspectRatio { width, height })
}

fn aspect_ratio_label(value: &str, parsed: &ControlValue) -> String {
    match parsed {
        ControlValue::AspectRatioAuto => "Auto".to_owned(),
        ControlValue::AspectRatioAdaptive => "Adaptive".to_owned(),
        _ => value.to_owned(),
    }
}

fn aspect_ratio_control(values: &[String], default: Option<&str>) -> Option<ModelControl> {
    let choices = values
        .iter()
        .filter_map(|value| {
            let parsed = parse_aspect_ratio(value)?;
            Some(ControlChoice {
                label: aspect_ratio_label(value, &parsed),
                value: parsed,
            })
        })
        .collect::<Vec<_>>();
    if choices.is_empty() {
        return None;
    }
    let default = default.and_then(parse_aspect_ratio).and_then(|parsed| {
        choices
            .iter()
            .find(|choice| choice.value == parsed)
            .map(|choice| choice.value.clone())
    });
    Some(ModelControl {
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

fn parse_duration(value: &str) -> Option<(ControlValue, String)> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("auto") || trimmed == "-1" {
        return Some((ControlValue::DurationAuto, "Auto".to_owned()));
    }
    let seconds = trimmed.strip_suffix('s').unwrap_or(trimmed);
    let seconds: f64 = seconds.parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some((
        ControlValue::DurationSeconds { value: seconds },
        trimmed.to_owned(),
    ))
}

fn duration_control(model_id: &str, values: &[String]) -> Result<ModelControl, VeniceCatalogError> {
    let choices = values
        .iter()
        .filter_map(|value| {
            let (parsed, label) = parse_duration(value)?;
            Some(ControlChoice {
                value: parsed,
                label,
            })
        })
        .collect::<Vec<_>>();
    if choices.is_empty() {
        return Err(VeniceCatalogError::InvalidDuration {
            model_id: model_id.to_owned(),
            value: values.join(","),
        });
    }
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

fn normalize_image_format(value: &str) -> Option<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "webp" => Some("webp".into()),
        "png" => Some("png".into()),
        "jpeg" | "jpg" => Some("jpeg".into()),
        _ => None,
    }
}

fn image_format_mime(format: &str) -> Option<&'static str> {
    match format {
        "webp" => Some("image/webp"),
        "png" => Some("image/png"),
        "jpeg" => Some("image/jpeg"),
        _ => None,
    }
}

fn advertised_image_formats(values: &[String]) -> Vec<String> {
    let mut formats = Vec::new();
    for value in values {
        let Some(format) = normalize_image_format(value) else {
            continue;
        };
        if !formats.contains(&format) {
            formats.push(format);
        }
    }
    formats
}

/// Grok Imagine through Venice ignores `format` and returns JPEG. Seedream and
/// the other generate models honor the documented endpoint values.
fn honors_endpoint_image_format(model_id: &str) -> bool {
    !model_id.starts_with("grok-imagine")
}

fn image_format_choices(model_id: &str, advertised: &[String]) -> Vec<String> {
    let advertised = advertised_image_formats(advertised);
    if !advertised.is_empty() {
        return advertised;
    }
    if honors_endpoint_image_format(model_id) {
        return ENDPOINT_IMAGE_FORMATS
            .iter()
            .map(|value| (*value).to_owned())
            .collect();
    }
    Vec::new()
}

fn image_output_mime_types(formats: &[String]) -> Vec<String> {
    if formats.is_empty() {
        return DEFAULT_IMAGE_OUTPUT_MIMES
            .iter()
            .map(|mime| (*mime).to_owned())
            .collect();
    }
    formats
        .iter()
        .filter_map(|format| image_format_mime(format).map(str::to_owned))
        .collect()
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
        mime: MimeConstraint::accepting(mime_types.iter().copied()),
    }
}

fn upscale_pricing_metadata(pricing: Option<&serde_json::Value>) -> Option<PricingMetadata> {
    let pricing = pricing?;
    let mut entries = Vec::new();
    let Some(upscale) = pricing.get("upscale").and_then(|value| value.as_object()) else {
        return None;
    };
    for (scale, price) in upscale {
        let Some(amount) = usd_amount(price) else {
            continue;
        };
        let Some(factor) = parse_upscale_factor(scale) else {
            continue;
        };
        entries.push(PricingEntry {
            when: [(
                ControlId::from("scale"),
                ControlValue::Integer { value: factor },
            )]
            .into_iter()
            .collect(),
            amount,
        });
    }
    if entries.is_empty() {
        return None;
    }
    Some(PricingMetadata {
        currency: "USD".to_owned(),
        unit: PricingUnit::PerRequest,
        unit_label: String::new(),
        amount: None,
        entries,
        detail: None,
    })
}

fn parse_upscale_factor(value: &str) -> Option<i64> {
    value
        .trim()
        .trim_end_matches('x')
        .trim_end_matches('X')
        .parse()
        .ok()
        .filter(|value| *value == 2 || *value == 4)
}

fn inpaint_pricing_metadata(pricing: Option<&serde_json::Value>) -> Option<PricingMetadata> {
    let pricing = pricing?;
    let entries = quality_resolution_entries(pricing);
    let amount = pricing.get("inpaint").and_then(usd_amount);
    if entries.is_empty() && amount.is_none() {
        return None;
    }
    Some(PricingMetadata {
        currency: "USD".to_owned(),
        unit: PricingUnit::PerRequest,
        unit_label: String::new(),
        amount: if entries.is_empty() { amount } else { None },
        entries,
        detail: None,
    })
}

fn pricing_metadata(pricing: Option<&serde_json::Value>) -> Option<PricingMetadata> {
    let pricing = pricing?;
    let entries = quality_resolution_entries(pricing);
    let amount = pricing
        .get("generation")
        .and_then(usd_amount)
        .or_else(|| usd_amount(pricing));
    if entries.is_empty() && amount.is_none() {
        return None;
    }
    Some(PricingMetadata {
        currency: "USD".to_owned(),
        unit: PricingUnit::PerOutput,
        unit_label: String::new(),
        amount: if entries.is_empty() { amount } else { None },
        entries,
        detail: None,
    })
}

fn quality_resolution_entries(pricing: &serde_json::Value) -> Vec<PricingEntry> {
    let mut entries = Vec::new();
    if let Some(quality) = pricing.get("quality").and_then(|value| value.as_object()) {
        for (resolution, levels) in quality {
            let Some(levels) = levels.as_object() else {
                continue;
            };
            for (quality, price) in levels {
                if let Some(amount) = usd_amount(price) {
                    entries.push(PricingEntry {
                        when: [
                            (
                                ControlId::from("resolution"),
                                ControlValue::Resolution {
                                    value: resolution.clone(),
                                },
                            ),
                            (
                                ControlId::from("quality"),
                                ControlValue::Enum {
                                    value: quality.clone(),
                                },
                            ),
                        ]
                        .into_iter()
                        .collect(),
                        amount,
                    });
                }
            }
        }
    }
    if let Some(resolutions) = pricing
        .get("resolutions")
        .and_then(|value| value.as_object())
    {
        for (resolution, price) in resolutions {
            if let Some(amount) = usd_amount(price) {
                entries.push(PricingEntry {
                    when: [(
                        ControlId::from("resolution"),
                        ControlValue::Resolution {
                            value: resolution.clone(),
                        },
                    )]
                    .into_iter()
                    .collect(),
                    amount,
                });
            }
        }
    }
    entries
}

fn usd_amount(value: &serde_json::Value) -> Option<f64> {
    value
        .get("usd")
        .and_then(|value| value.as_f64())
        .filter(|amount| amount.is_finite() && *amount >= 0.0)
}

/// Hash the schema a request must satisfy. Display copy, pricing, and fetch
/// time can change without invalidating an already-valid form.
fn finish_manifest(mut model: MediaModel) -> Result<MediaModel, VeniceCatalogError> {
    let mut submit_relevant = serde_json::to_value(&model)?;
    if let Some(object) = submit_relevant.as_object_mut() {
        object.remove("fetched_at");
        object.remove("manifest_version");
        object.remove("display_name");
        object.remove("description");
        object.remove("pricing");
        object.remove("features");
    }
    let bytes = serde_json::to_vec(&submit_relevant)?;
    let digest = Sha256::digest(bytes);
    model.manifest_version = format!("venice-v1:{digest:x}");
    Ok(model)
}
