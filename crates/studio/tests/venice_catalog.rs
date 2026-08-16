use chrono::{TimeZone, Utc};
use zeron_studio::{ControlId, ControlKind, MediaOperation, venice::normalize_model_catalog};

const IMAGE: &[u8] = include_bytes!("fixtures/venice/image-model.json");
const TEXT_TO_VIDEO: &[u8] = include_bytes!("fixtures/venice/text-to-video-model.json");
const IMAGE_TO_VIDEO: &[u8] = include_bytes!("fixtures/venice/image-to-video-model.json");

fn fetched_at() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_777_000_000, 0).unwrap()
}

#[test]
fn real_catalog_fixtures_render_as_provider_neutral_controls() {
    let image = normalize_model_catalog(IMAGE, fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(image.operation, MediaOperation::TextToImage);
    assert_eq!(image.prompt_maximum_chars, Some(10_000));
    assert_control(&image, "aspect_ratio", ControlKind::AspectRatio, 8);
    assert_control(&image, "resolution", ControlKind::Resolution, 3);
    assert_control(&image, "quality", ControlKind::Enum, 3);
    assert_control(&image, "steps", ControlKind::Integer, 0);
    assert_control(&image, "format", ControlKind::Enum, 3);

    let text_video = normalize_model_catalog(TEXT_TO_VIDEO, fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(text_video.operation, MediaOperation::TextToVideo);
    assert!(text_video.input_constraints.is_empty());
    assert_control(&text_video, "duration", ControlKind::Duration, 9);
    assert_control(&text_video, "audio", ControlKind::Boolean, 0);

    let image_video = normalize_model_catalog(IMAGE_TO_VIDEO, fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(image_video.operation, MediaOperation::ImageToVideo);
    assert_eq!(image_video.input_constraints.len(), 1);
    assert_eq!(image_video.input_constraints[0].role.as_str(), "source");
    assert!(
        image_video
            .controls
            .iter()
            .all(|control| control.id.as_str() != "aspect_ratio")
    );
}

#[test]
fn operation_and_controls_do_not_depend_on_model_name() {
    let mut fixture: serde_json::Value = serde_json::from_slice(TEXT_TO_VIDEO).unwrap();
    fixture["data"][0]["id"] = "opaque-provider-id".into();
    fixture["data"][0]["model_spec"]["name"] = "Renamed Tomorrow".into();

    let model = normalize_model_catalog(&serde_json::to_vec(&fixture).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(model.operation, MediaOperation::TextToVideo);
    assert_control(&model, "duration", ControlKind::Duration, 9);
}

#[test]
fn manifest_version_ignores_fetch_time_but_tracks_constraints() {
    let first = normalize_model_catalog(IMAGE, fetched_at())
        .unwrap()
        .remove(0);
    let later = normalize_model_catalog(IMAGE, Utc.timestamp_opt(1_778_000_000, 0).unwrap())
        .unwrap()
        .remove(0);
    assert_eq!(first.manifest_version, later.manifest_version);

    let mut changed: serde_json::Value = serde_json::from_slice(IMAGE).unwrap();
    changed["data"][0]["model_spec"]["constraints"]["steps"]["max"] = 51.into();
    let changed = normalize_model_catalog(&serde_json::to_vec(&changed).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    assert_ne!(first.manifest_version, changed.manifest_version);
}

fn assert_control(
    model: &zeron_studio::MediaModel,
    id: &str,
    kind: ControlKind,
    choice_count: usize,
) {
    let control = model
        .controls
        .iter()
        .find(|control| control.id == ControlId::from(id))
        .unwrap();
    assert_eq!(control.kind, kind);
    assert_eq!(control.choices.len(), choice_count);
}
