//! Per-family Venice video queue/quote field tables.
//!
//! The composer never sees these strings. Seedance (and Wan tagged Seedance)
//! and Grok Imagine have different duration, aspect, audio, and reference
//! encodings. Hidden families are not queued.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde_json::{Map, Value};

use crate::{
    AdapterFamily, AudioCapability, ControlValue, GenerationRequest, ProviderError,
    ProviderErrorKind, ProviderResult, ROLE_AUDIO, ROLE_LAST_FRAME, ROLE_REFERENCE,
    ROLE_REFERENCE_AUDIO, ROLE_REFERENCE_VIDEO, ROLE_SOURCE, ResolvedInput, SubmitContext,
    venice_overlay::bundled_video_overlay,
};

/// Raw HTTP body cap for `POST /video/queue` (data URLs only).
pub(crate) const MAX_QUEUE_BODY_BYTES: usize = 35 * 1024 * 1024;

pub(crate) fn adapter_family_for(model_id: &str) -> ProviderResult<AdapterFamily> {
    let overlay = bundled_video_overlay()
        .map_err(|error| ProviderError::new(ProviderErrorKind::Other, error.to_string()))?;
    Ok(overlay.adapter_family(model_id))
}

pub(crate) fn require_queueable_family(family: AdapterFamily) -> ProviderResult<AdapterFamily> {
    match family {
        AdapterFamily::Hidden => Err(ProviderError::new(
            ProviderErrorKind::Unsupported,
            "Hidden Venice video models are not queued",
        )),
        AdapterFamily::Seedance | AdapterFamily::Grok => Ok(family),
    }
}

pub(crate) fn video_quote_payload(
    request: &GenerationRequest,
    reference_video_total_duration: Option<f64>,
) -> ProviderResult<Value> {
    let family = require_queueable_family(adapter_family_for(request.model_id.as_str())?)?;
    let mut payload = Map::from_iter([(
        "model".into(),
        Value::String(request.model_id.as_str().into()),
    )]);
    apply_video_controls(&mut payload, request, family)?;
    if !payload.contains_key("duration") {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "video quote requires a duration",
        ));
    }
    if request
        .inputs
        .iter()
        .any(|input| input.role.as_str() == ROLE_REFERENCE_VIDEO)
    {
        // Quote has no file bytes. The caller supplies the probed total when
        // any reference clip is present so source-matched jobs are not underquoted.
        let total = reference_video_total_duration.ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "video quote requires reference_video_total_duration when reference clips are present",
            )
        })?;
        if !total.is_finite() || total < 0.0 {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "reference_video_total_duration must be a finite non-negative duration",
            ));
        }
        payload.insert("reference_video_total_duration".into(), total.into());
    }
    Ok(payload.into())
}

pub(crate) fn video_queue_payload(
    request: &GenerationRequest,
    context: &SubmitContext,
    family: AdapterFamily,
) -> ProviderResult<Value> {
    require_queueable_family(family)?;
    if request.output_count != 1 {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Venice video queue returns exactly one video",
        ));
    }
    if request.prompt.trim().is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Venice video queue requires a prompt",
        ));
    }
    let mut payload = Map::from_iter([
        (
            "model".into(),
            Value::String(request.model_id.as_str().into()),
        ),
        ("prompt".into(), Value::String(request.prompt.clone())),
    ]);
    apply_video_controls(&mut payload, request, family)?;
    if !payload.contains_key("duration") {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "video queue requires a duration",
        ));
    }
    apply_family_references(&mut payload, context, family)?;
    let value = Value::Object(payload);
    let encoded = serde_json::to_vec(&value).map_err(|error| {
        ProviderError::new(ProviderErrorKind::InvalidRequest, error.to_string())
    })?;
    if encoded.len() > MAX_QUEUE_BODY_BYTES {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Venice video queue body exceeds the 35MB data-URL limit",
        ));
    }
    Ok(value)
}

fn apply_video_controls(
    payload: &mut Map<String, Value>,
    request: &GenerationRequest,
    family: AdapterFamily,
) -> ProviderResult<()> {
    let audio_capability = audio_capability_for(family, &request.controls);
    if let Some(audio) = wire_audio(
        audio_capability,
        request.controls.get(&crate::ControlId::from("audio")),
    ) {
        payload.insert("audio".into(), audio.into());
    }
    for (id, value) in &request.controls {
        match (id.as_str(), value) {
            ("duration", _) => {
                payload.insert("duration".into(), wire_duration(family, value)?.into());
            }
            ("resolution", ControlValue::Resolution { value }) => {
                payload.insert("resolution".into(), value.clone().into());
            }
            ("aspect_ratio", _) => {
                payload.insert("aspect_ratio".into(), wire_aspect(family, value)?.into());
            }
            ("audio", _) => {}
            (unknown, _) => {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!("unsupported Venice video control {unknown}"),
                ));
            }
        }
    }
    Ok(())
}

fn audio_capability_for(
    family: AdapterFamily,
    controls: &BTreeMap<crate::ControlId, ControlValue>,
) -> AudioCapability {
    match family {
        AdapterFamily::Hidden => AudioCapability::None,
        AdapterFamily::Grok => match controls.get(&crate::ControlId::from("audio")) {
            Some(ControlValue::Boolean { value }) => {
                AudioCapability::Configurable { default: *value }
            }
            // Grok R2V has no audio toggle; sending true/false 400s.
            _ => AudioCapability::None,
        },
        AdapterFamily::Seedance => match controls.get(&crate::ControlId::from("audio")) {
            Some(ControlValue::Boolean { value }) => {
                AudioCapability::Configurable { default: *value }
            }
            // Seedance without a toggle is ForcedOn (live `audio` and not configurable).
            _ => AudioCapability::ForcedOn,
        },
    }
}

pub(crate) fn wire_duration(family: AdapterFamily, value: &ControlValue) -> ProviderResult<String> {
    match (family, value) {
        (AdapterFamily::Hidden, _) => Err(ProviderError::new(
            ProviderErrorKind::Unsupported,
            "Hidden Venice video models are not queued",
        )),
        (AdapterFamily::Seedance, ControlValue::DurationAuto) => Ok("auto".into()),
        (AdapterFamily::Seedance, ControlValue::DurationSeconds { value }) => {
            Ok(format_duration_number(*value, true))
        }
        (AdapterFamily::Grok, ControlValue::DurationSeconds { value }) => {
            Ok(format_duration_number(*value, false))
        }
        (AdapterFamily::Grok, ControlValue::DurationAuto) => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok duration has no Auto",
        )),
        _ => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "video requires a duration",
        )),
    }
}

pub(crate) fn wire_aspect(family: AdapterFamily, value: &ControlValue) -> ProviderResult<String> {
    match (family, value) {
        (_, ControlValue::AspectRatio { width, height }) => Ok(format!("{width}:{height}")),
        (AdapterFamily::Seedance, ControlValue::AspectRatioAuto) => Ok("auto".into()),
        (AdapterFamily::Seedance, ControlValue::AspectRatioAdaptive) => Ok("adaptive".into()),
        (
            AdapterFamily::Grok,
            ControlValue::AspectRatioAuto | ControlValue::AspectRatioAdaptive,
        ) => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok aspect ratio must be W:H",
        )),
        (AdapterFamily::Hidden, _) => Err(ProviderError::new(
            ProviderErrorKind::Unsupported,
            "Hidden Venice video models are not queued",
        )),
        _ => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "unsupported Venice video aspect ratio",
        )),
    }
}

pub(crate) fn wire_audio(
    capability: AudioCapability,
    control: Option<&ControlValue>,
) -> Option<bool> {
    match capability {
        AudioCapability::None => None,
        AudioCapability::ForcedOn => Some(true),
        AudioCapability::Configurable { default } => match control {
            Some(ControlValue::Boolean { value }) => Some(*value),
            _ => Some(default),
        },
    }
}

fn format_duration_number(value: f64, suffix_s: bool) -> String {
    let number = if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    };
    if suffix_s {
        format!("{number}s")
    } else {
        number
    }
}

fn apply_family_references(
    payload: &mut Map<String, Value>,
    context: &SubmitContext,
    family: AdapterFamily,
) -> ProviderResult<()> {
    match family {
        AdapterFamily::Hidden => Err(ProviderError::new(
            ProviderErrorKind::Unsupported,
            "Hidden Venice video models are not queued",
        )),
        AdapterFamily::Grok => apply_grok_references(payload, context),
        AdapterFamily::Seedance => apply_seedance_references(payload, context),
    }
}

fn apply_grok_references(
    payload: &mut Map<String, Value>,
    context: &SubmitContext,
) -> ProviderResult<()> {
    if let Some(input) = context
        .inputs
        .iter()
        .find(|input| !matches!(input.role.as_str(), ROLE_REFERENCE | ROLE_SOURCE))
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!(
                "Grok video only accepts a source clip and reference images, not {}",
                input.role.as_str()
            ),
        ));
    }
    if let Some(url) = single_role_data_url(context, ROLE_SOURCE)? {
        payload.insert("video_url".into(), url.into());
    }
    let urls = role_data_urls(context, ROLE_REFERENCE)?;
    if !urls.is_empty() {
        payload.insert("referenceImageUrls".into(), urls.into());
    }
    if !payload.contains_key("video_url") && !payload.contains_key("referenceImageUrls") {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok video requires a source clip or reference images",
        ));
    }
    Ok(())
}

fn apply_seedance_references(
    payload: &mut Map<String, Value>,
    context: &SubmitContext,
) -> ProviderResult<()> {
    if let Some(url) = single_role_data_url(context, ROLE_SOURCE)? {
        let mime = context
            .inputs
            .iter()
            .find(|input| input.role.as_str() == ROLE_SOURCE)
            .map(|input| input.mime_type.as_str())
            .unwrap_or("");
        if mime.starts_with("video/") {
            payload.insert("video_url".into(), url.into());
        } else {
            payload.insert("image_url".into(), url.into());
        }
    }
    if let Some(url) = single_role_data_url(context, ROLE_LAST_FRAME)? {
        payload.insert("end_image_url".into(), url.into());
    }
    if let Some(url) = single_role_data_url(context, ROLE_AUDIO)? {
        payload.insert("audio_url".into(), url.into());
    }
    let reference_images = role_data_urls(context, ROLE_REFERENCE)?;
    if !reference_images.is_empty() {
        payload.insert("reference_image_urls".into(), reference_images.into());
    }
    let reference_videos = role_data_urls(context, ROLE_REFERENCE_VIDEO)?;
    if !reference_videos.is_empty() {
        payload.insert("reference_video_urls".into(), reference_videos.into());
    }
    let reference_audios = role_data_urls(context, ROLE_REFERENCE_AUDIO)?;
    if !reference_audios.is_empty() {
        payload.insert("reference_audio_urls".into(), reference_audios.into());
    }
    Ok(())
}

fn single_role_data_url(context: &SubmitContext, role: &str) -> ProviderResult<Option<String>> {
    let mut urls = role_data_urls(context, role)?;
    if urls.len() > 1 {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("Venice video accepts at most one {role} input"),
        ));
    }
    Ok(urls.pop())
}

fn role_data_urls(context: &SubmitContext, role: &str) -> ProviderResult<Vec<String>> {
    let mut inputs: Vec<&ResolvedInput> = context
        .inputs
        .iter()
        .filter(|input| input.role.as_str() == role)
        .collect();
    inputs.sort_by_key(|input| input.ordinal);
    inputs.into_iter().map(data_url_for).collect()
}

fn data_url_for(input: &ResolvedInput) -> ProviderResult<String> {
    let bytes = std::fs::read(&input.path).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!(
                "could not read video input {}: {error}",
                input.role.as_str()
            ),
        )
    })?;
    Ok(format!(
        "data:{};base64,{}",
        input.mime_type,
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::{
        ControlValue, GenerationInput, GenerationInputSource, MediaOperation, ROLE_REFERENCE_VIDEO,
        StudioAssetId,
    };

    fn seedance_request() -> GenerationRequest {
        GenerationRequest {
            provider_id: crate::venice::VENICE_PROVIDER_ID.into(),
            model_id: "seedance-1-5-pro-text-to-video-basic".into(),
            operation: MediaOperation::TextToVideo,
            prompt: "a comet".into(),
            negative_prompt: None,
            output_count: 1,
            controls: BTreeMap::from([
                (
                    "duration".into(),
                    ControlValue::DurationSeconds { value: 10.0 },
                ),
                (
                    "resolution".into(),
                    ControlValue::Resolution {
                        value: "1080p".into(),
                    },
                ),
                (
                    "aspect_ratio".into(),
                    ControlValue::AspectRatio {
                        width: 16,
                        height: 9,
                    },
                ),
                ("audio".into(), ControlValue::Boolean { value: true }),
            ]),
            inputs: Vec::new(),
            manifest_version: "v1".into(),
            display_aspect_ratio: (16, 9),
        }
    }

    fn grok_request() -> GenerationRequest {
        GenerationRequest {
            provider_id: crate::venice::VENICE_PROVIDER_ID.into(),
            model_id: "grok-imagine-reference-to-video".into(),
            operation: MediaOperation::ReferenceToVideo,
            prompt: "a comet".into(),
            negative_prompt: None,
            output_count: 1,
            controls: BTreeMap::from([
                (
                    "duration".into(),
                    ControlValue::DurationSeconds { value: 8.0 },
                ),
                (
                    "resolution".into(),
                    ControlValue::Resolution {
                        value: "720p".into(),
                    },
                ),
                (
                    "aspect_ratio".into(),
                    ControlValue::AspectRatio {
                        width: 16,
                        height: 9,
                    },
                ),
            ]),
            inputs: Vec::new(),
            manifest_version: "v1".into(),
            display_aspect_ratio: (16, 9),
        }
    }

    fn grok_v2v_request() -> GenerationRequest {
        let mut request = grok_request();
        request.model_id = "grok-imagine-video-to-video-private".into();
        request.operation = MediaOperation::VideoToVideo;
        request
            .controls
            .insert("audio".into(), ControlValue::Boolean { value: true });
        request
    }

    #[test]
    fn seedance_duration_uses_s_suffix_and_auto() {
        assert_eq!(
            wire_duration(
                AdapterFamily::Seedance,
                &ControlValue::DurationSeconds { value: 10.0 }
            )
            .unwrap(),
            "10s"
        );
        assert_eq!(
            wire_duration(AdapterFamily::Seedance, &ControlValue::DurationAuto).unwrap(),
            "auto"
        );
    }

    #[test]
    fn grok_duration_is_unsuffixed_and_rejects_auto() {
        assert_eq!(
            wire_duration(
                AdapterFamily::Grok,
                &ControlValue::DurationSeconds { value: 8.0 }
            )
            .unwrap(),
            "8"
        );
        assert_eq!(
            wire_duration(AdapterFamily::Grok, &ControlValue::DurationAuto)
                .unwrap_err()
                .kind,
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn seedance_aspect_accepts_auto_and_adaptive() {
        assert_eq!(
            wire_aspect(AdapterFamily::Seedance, &ControlValue::AspectRatioAuto).unwrap(),
            "auto"
        );
        assert_eq!(
            wire_aspect(AdapterFamily::Seedance, &ControlValue::AspectRatioAdaptive).unwrap(),
            "adaptive"
        );
        assert_eq!(
            wire_aspect(
                AdapterFamily::Seedance,
                &ControlValue::AspectRatio {
                    width: 9,
                    height: 16
                }
            )
            .unwrap(),
            "9:16"
        );
    }

    #[test]
    fn grok_aspect_is_width_height_only() {
        assert_eq!(
            wire_aspect(
                AdapterFamily::Grok,
                &ControlValue::AspectRatio {
                    width: 16,
                    height: 9
                }
            )
            .unwrap(),
            "16:9"
        );
        assert_eq!(
            wire_aspect(AdapterFamily::Grok, &ControlValue::AspectRatioAuto)
                .unwrap_err()
                .kind,
            ProviderErrorKind::InvalidRequest
        );
        assert_eq!(
            wire_aspect(AdapterFamily::Grok, &ControlValue::AspectRatioAdaptive)
                .unwrap_err()
                .kind,
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn audio_none_omits_forced_on_sends_true() {
        assert_eq!(wire_audio(AudioCapability::None, None), None);
        assert_eq!(
            wire_audio(
                AudioCapability::None,
                Some(&ControlValue::Boolean { value: true })
            ),
            None
        );
        assert_eq!(wire_audio(AudioCapability::ForcedOn, None), Some(true));
        assert_eq!(
            wire_audio(
                AudioCapability::Configurable { default: true },
                Some(&ControlValue::Boolean { value: false })
            ),
            Some(false)
        );
    }

    #[test]
    fn seedance_quote_encodes_family_fields() {
        let value = video_quote_payload(&seedance_request(), None).unwrap();
        assert_eq!(value["duration"], "10s");
        assert_eq!(value["aspect_ratio"], "16:9");
        assert_eq!(value["audio"], true);
        assert!(value.get("reference_video_total_duration").is_none());
    }

    #[test]
    fn grok_quote_omits_audio_and_uses_unsuffixed_duration() {
        let value = video_quote_payload(&grok_request(), None).unwrap();
        assert_eq!(value["duration"], "8");
        assert_eq!(value["aspect_ratio"], "16:9");
        assert!(value.get("audio").is_none());
    }

    #[test]
    fn seedance_without_audio_control_sends_forced_on() {
        let mut request = seedance_request();
        request.controls.remove(&crate::ControlId::from("audio"));
        let value = video_quote_payload(&request, None).unwrap();
        assert_eq!(value["audio"], true);
    }

    #[test]
    fn seedance_configurable_audio_sends_bool() {
        let mut request = seedance_request();
        request
            .controls
            .insert("audio".into(), ControlValue::Boolean { value: false });
        let value = video_quote_payload(&request, None).unwrap();
        assert_eq!(value["audio"], false);
    }

    #[test]
    fn quote_includes_reference_video_total_duration() {
        let mut request = seedance_request();
        request.model_id = "seedance-2-5-reference-to-video-basic".into();
        request.operation = MediaOperation::ReferenceToVideo;
        request.inputs.push(GenerationInput {
            role: ROLE_REFERENCE_VIDEO.into(),
            ordinal: 0,
            source: GenerationInputSource::Asset {
                asset_id: StudioAssetId::new(),
            },
            content_hash: "clip".into(),
        });
        let value = video_quote_payload(&request, Some(12.5)).unwrap();
        assert_eq!(value["reference_video_total_duration"], 12.5);
        assert_eq!(
            video_quote_payload(&request, None).unwrap_err().kind,
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn hidden_models_are_not_quoted_or_queued() {
        let mut request = seedance_request();
        request.model_id = "kling-o3-pro-reference-to-video".into();
        assert_eq!(
            video_quote_payload(&request, None).unwrap_err().kind,
            ProviderErrorKind::Unsupported
        );
        assert_eq!(
            adapter_family_for("kling-o3-pro-reference-to-video").unwrap(),
            AdapterFamily::Hidden
        );
        assert_eq!(
            require_queueable_family(AdapterFamily::Hidden)
                .unwrap_err()
                .kind,
            ProviderErrorKind::Unsupported
        );
    }

    #[test]
    fn seedance_queue_maps_reference_roles() {
        let png = solid_png(32, 32);
        let (image_path, image) = file_input(ROLE_SOURCE, 0, "image/png", &png);
        let (last_path, last) = file_input(ROLE_LAST_FRAME, 0, "image/png", &png);
        let (ref_path, reference) = file_input(ROLE_REFERENCE, 0, "image/png", &png);
        let (video_path, video) = file_input(ROLE_REFERENCE_VIDEO, 0, "video/mp4", b"mp4");
        let (audio_path, audio) = file_input(ROLE_REFERENCE_AUDIO, 0, "audio/wav", b"wav");
        let mut request = seedance_request();
        request.model_id = "seedance-2-5-reference-to-video-basic".into();
        request.operation = MediaOperation::ReferenceToVideo;
        request
            .controls
            .insert("aspect_ratio".into(), ControlValue::AspectRatioAdaptive);
        request
            .controls
            .insert("duration".into(), ControlValue::DurationAuto);
        let context = SubmitContext {
            idempotency_key: "key".into(),
            inputs: vec![image, last, reference, video, audio],
        };
        let value = video_queue_payload(&request, &context, AdapterFamily::Seedance).unwrap();
        assert_eq!(value["duration"], "auto");
        assert_eq!(value["aspect_ratio"], "adaptive");
        assert!(
            value["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(
            value["end_image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(value["reference_image_urls"].as_array().unwrap().len(), 1);
        assert_eq!(value["reference_video_urls"].as_array().unwrap().len(), 1);
        assert_eq!(value["reference_audio_urls"].as_array().unwrap().len(), 1);
        assert!(value.get("omni_reference_task_type").is_none());
        assert!(value.get("referenceImageUrls").is_none());
        for path in [image_path, last_path, ref_path, video_path, audio_path] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn grok_queue_uses_camel_case_reference_urls() {
        let png = solid_png(32, 32);
        let (path, input) = file_input(ROLE_REFERENCE, 0, "image/png", &png);
        let request = grok_request();
        let context = SubmitContext {
            idempotency_key: "key".into(),
            inputs: vec![input],
        };
        let value = video_queue_payload(&request, &context, AdapterFamily::Grok).unwrap();
        assert_eq!(value["duration"], "8");
        assert!(value.get("audio").is_none());
        assert_eq!(value["referenceImageUrls"].as_array().unwrap().len(), 1);
        assert!(value.get("reference_image_urls").is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn grok_v2v_queue_sends_video_url_and_audio() {
        let (path, input) = file_input(ROLE_SOURCE, 0, "video/mp4", b"mp4");
        let request = grok_v2v_request();
        let context = SubmitContext {
            idempotency_key: "key".into(),
            inputs: vec![input],
        };
        let value = video_queue_payload(&request, &context, AdapterFamily::Grok).unwrap();
        assert_eq!(value["duration"], "8");
        assert_eq!(value["audio"], true);
        assert!(
            value["video_url"]
                .as_str()
                .unwrap()
                .starts_with("data:video/mp4;base64,")
        );
        assert!(value.get("referenceImageUrls").is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn grok_v2v_quote_sends_audio() {
        let value = video_quote_payload(&grok_v2v_request(), None).unwrap();
        assert_eq!(value["duration"], "8");
        assert_eq!(value["audio"], true);
    }

    #[test]
    fn seedance_v2v_queue_sends_video_url() {
        let (path, input) = file_input(ROLE_SOURCE, 0, "video/mp4", b"mp4");
        let mut request = seedance_request();
        request.model_id = "wan-2-7-video-to-video".into();
        request.operation = MediaOperation::VideoToVideo;
        let context = SubmitContext {
            idempotency_key: "key".into(),
            inputs: vec![input],
        };
        let value = video_queue_payload(&request, &context, AdapterFamily::Seedance).unwrap();
        assert!(
            value["video_url"]
                .as_str()
                .unwrap()
                .starts_with("data:video/mp4;base64,")
        );
        assert!(value.get("image_url").is_none());
        let _ = std::fs::remove_file(path);
    }

    fn solid_png(width: u32, height: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb([40, 120, 200]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut raw), image::ImageFormat::Png)
        .unwrap();
        raw
    }

    fn file_input(
        role: &str,
        ordinal: u32,
        mime: &str,
        bytes: &[u8],
    ) -> (std::path::PathBuf, ResolvedInput) {
        let path = std::env::temp_dir().join(format!(
            "zeron-venice-video-{}-{}.bin",
            role,
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, bytes).unwrap();
        (
            path.clone(),
            ResolvedInput {
                role: role.into(),
                ordinal,
                path,
                mime_type: mime.into(),
                content_hash: "hash".into(),
                size_bytes: bytes.len() as u64,
            },
        )
    }
}
