use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use zeron_studio::{
    AdapterFamily, AttachmentOrigin, AudioCapability, ComposerAttachment, ComposerEvent,
    ComposerMediaKind, ComposerMode, ComposerPhase, ComposerSnapshot, ConflictCode, ControlChoice,
    ControlId, ControlKind, ControlValue, GenerationInputSource, InputConstraint, InputRole,
    MediaKind, MediaModel, MediaOperation, MimeConstraint, ModelControl, ProviderId,
    QUEUE_BODY_LIMIT_BYTES, ROLE_LAST_FRAME, ROLE_REFERENCE, ROLE_REFERENCE_AUDIO,
    ROLE_REFERENCE_VIDEO, ROLE_SOURCE, ResolveAction, ResolveError, SelectedModelRef,
    StudioAssetId, VideoModelMeta, apply_event, apply_resolve_checked, estimate_queue_body_bytes,
    evaluate_composer, map_tray, picker_models, popup_conflict, venice::normalize_model_catalog,
};

const IMAGE: &[u8] = include_bytes!("fixtures/venice/image-model.json");
const TEXT_TO_VIDEO: &[u8] = include_bytes!("fixtures/venice/text-to-video-model.json");
const IMAGE_TO_VIDEO: &[u8] = include_bytes!("fixtures/venice/image-to-video-model.json");
const SEEDANCE_2_5_R2V: &[u8] =
    include_bytes!("fixtures/venice/seedance-2-5-reference-to-video-model.json");
const UPSCALE: &[u8] = include_bytes!("fixtures/venice/upscale-model.json");

fn fetched_at() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_777_000_000, 0).unwrap()
}

fn fixture(bytes: &[u8]) -> MediaModel {
    normalize_model_catalog(bytes, fetched_at())
        .unwrap()
        .remove(0)
}

fn catalog_models() -> (MediaModel, MediaModel, MediaModel, MediaModel) {
    (
        fixture(SEEDANCE_2_5_R2V),
        fixture(TEXT_TO_VIDEO),
        fixture(IMAGE_TO_VIDEO),
        fixture(IMAGE),
    )
}

fn duration_choices(seconds: &[f64]) -> Vec<ControlChoice> {
    seconds
        .iter()
        .map(|value| ControlChoice {
            value: ControlValue::DurationSeconds { value: *value },
            label: format!("{value}s"),
        })
        .collect()
}

fn duration_control(seconds: &[f64]) -> ModelControl {
    ModelControl {
        id: ControlId::from("duration"),
        label: "Duration".to_owned(),
        description: None,
        kind: ControlKind::Duration,
        required: true,
        default: None,
        minimum: None,
        maximum: None,
        step: None,
        choices: duration_choices(seconds),
        visible_when: Vec::new(),
    }
}

fn aspect_control(include_adaptive: bool) -> ModelControl {
    let mut choices = vec![ControlChoice {
        value: ControlValue::AspectRatio {
            width: 16,
            height: 9,
        },
        label: "16:9".to_owned(),
    }];
    if include_adaptive {
        choices.push(ControlChoice {
            value: ControlValue::AspectRatioAdaptive,
            label: "Adaptive".to_owned(),
        });
    }
    ModelControl {
        id: ControlId::from("aspect_ratio"),
        label: "Aspect".to_owned(),
        description: None,
        kind: ControlKind::AspectRatio,
        required: false,
        default: Some(ControlValue::AspectRatio {
            width: 16,
            height: 9,
        }),
        minimum: None,
        maximum: None,
        step: None,
        choices,
        visible_when: Vec::new(),
    }
}

fn video_model(
    id: &str,
    display_name: &str,
    operation: MediaOperation,
    durations: &[f64],
    constraints: Vec<InputConstraint>,
    video: VideoModelMeta,
) -> MediaModel {
    MediaModel {
        provider_id: ProviderId::from("venice"),
        id: id.into(),
        display_name: display_name.to_owned(),
        description: None,
        operation,
        output_kind: MediaKind::Video,
        output_mime_types: vec!["video/mp4".to_owned()],
        input_constraints: constraints,
        prompt_maximum_chars: Some(2500),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 1,
        controls: vec![duration_control(durations), aspect_control(false)],
        pricing: None,
        features: Vec::new(),
        video,
        manifest_version: "fixture-v1".to_owned(),
        fetched_at: fetched_at(),
    }
}

fn seedance_meta() -> VideoModelMeta {
    VideoModelMeta {
        adapter_family: AdapterFamily::Seedance,
        generate_audio: AudioCapability::Configurable { default: true },
        ..VideoModelMeta::default()
    }
}

fn t2v(id: &str, durations: &[f64]) -> MediaModel {
    video_model(
        id,
        id,
        MediaOperation::TextToVideo,
        durations,
        Vec::new(),
        seedance_meta(),
    )
}

fn r2v(id: &str, image_max: u32, durations: &[f64]) -> MediaModel {
    let mut meta = seedance_meta();
    meta.requires_visual_reference = true;
    meta.reference_audio_requires_visual = true;
    video_model(
        id,
        id,
        MediaOperation::ReferenceToVideo,
        durations,
        vec![
            InputConstraint {
                role: InputRole::from(ROLE_REFERENCE),
                minimum_count: 0,
                maximum_count: image_max,
                mime: MimeConstraint {
                    accepted: vec![
                        "image/jpeg".to_owned(),
                        "image/png".to_owned(),
                        "image/webp".to_owned(),
                    ],
                    maximum_bytes: Some(30 * 1024 * 1024),
                    ..MimeConstraint::default()
                },
            },
            InputConstraint {
                role: InputRole::from(ROLE_REFERENCE_VIDEO),
                minimum_count: 0,
                maximum_count: 3,
                mime: MimeConstraint {
                    accepted: vec!["video/mp4".to_owned(), "video/quicktime".to_owned()],
                    maximum_bytes: Some(50 * 1024 * 1024),
                    minimum_duration_seconds: Some(2.0),
                    maximum_duration_seconds: Some(15.0),
                    maximum_total_duration_seconds: Some(15.0),
                    ..MimeConstraint::default()
                },
            },
            InputConstraint {
                role: InputRole::from(ROLE_REFERENCE_AUDIO),
                minimum_count: 0,
                maximum_count: 3,
                mime: MimeConstraint {
                    accepted: vec!["audio/wav".to_owned(), "audio/mpeg".to_owned()],
                    maximum_bytes: Some(15 * 1024 * 1024),
                    minimum_duration_seconds: Some(2.0),
                    maximum_duration_seconds: Some(15.0),
                    ..MimeConstraint::default()
                },
            },
        ],
        meta,
    )
}

fn i2v(id: &str, last_frame: bool) -> MediaModel {
    let mut constraints = vec![InputConstraint {
        role: InputRole::from(ROLE_SOURCE),
        minimum_count: 1,
        maximum_count: 1,
        mime: MimeConstraint {
            accepted: vec![
                "image/jpeg".to_owned(),
                "image/png".to_owned(),
                "image/webp".to_owned(),
            ],
            minimum_short_side: Some(300),
            minimum_aspect_ratio: Some(0.4),
            maximum_aspect_ratio: Some(2.5),
            ..MimeConstraint::default()
        },
    }];
    if last_frame {
        constraints.push(InputConstraint {
            role: InputRole::from(ROLE_LAST_FRAME),
            minimum_count: 0,
            maximum_count: 1,
            mime: MimeConstraint::accepting(["image/jpeg", "image/png", "image/webp"]),
        });
    }
    video_model(
        id,
        id,
        MediaOperation::ImageToVideo,
        &[4.0, 6.0, 8.0, 10.0],
        constraints,
        seedance_meta(),
    )
}

fn t2i(id: &str) -> MediaModel {
    MediaModel {
        provider_id: ProviderId::from("venice"),
        id: id.into(),
        display_name: id.to_owned(),
        description: None,
        operation: MediaOperation::TextToImage,
        output_kind: MediaKind::Image,
        output_mime_types: vec!["image/png".to_owned()],
        input_constraints: Vec::new(),
        prompt_maximum_chars: Some(10_000),
        negative_prompt_maximum_chars: None,
        maximum_output_count: 4,
        controls: Vec::new(),
        pricing: None,
        features: Vec::new(),
        video: VideoModelMeta::default(),
        manifest_version: "fixture-v1".to_owned(),
        fetched_at: fetched_at(),
    }
}

fn hidden_kling() -> MediaModel {
    let mut model = r2v("kling-o3-pro-reference-to-video", 7, &[5.0, 8.0, 10.0]);
    model.display_name = "Kling O3 Pro".to_owned();
    model.video.adapter_family = AdapterFamily::Hidden;
    model
}

fn selected(model: &MediaModel) -> SelectedModelRef {
    SelectedModelRef::new(model.provider_id.clone(), model.id.clone())
}

fn still(hash: &str) -> ComposerAttachment {
    ComposerAttachment {
        id: StudioAssetId::new(),
        kind: ComposerMediaKind::Image,
        pending: false,
        origin: AttachmentOrigin::Asset,
        mime_type: "image/png".to_owned(),
        byte_size: 80_000,
        width: Some(512),
        height: Some(512),
        duration_seconds: None,
        content_hash: hash.to_owned(),
        role_hint: None,
    }
}

fn clip(hash: &str, bytes: u64, duration: Option<f64>) -> ComposerAttachment {
    ComposerAttachment {
        id: StudioAssetId::new(),
        kind: ComposerMediaKind::Video,
        pending: false,
        origin: AttachmentOrigin::Asset,
        mime_type: "video/mp4".to_owned(),
        byte_size: bytes,
        width: Some(1280),
        height: Some(720),
        duration_seconds: duration,
        content_hash: hash.to_owned(),
        role_hint: None,
    }
}

fn audio(hash: &str) -> ComposerAttachment {
    ComposerAttachment {
        id: StudioAssetId::new(),
        kind: ComposerMediaKind::Audio,
        pending: false,
        origin: AttachmentOrigin::Asset,
        mime_type: "audio/wav".to_owned(),
        byte_size: 200_000,
        width: None,
        height: None,
        duration_seconds: Some(4.0),
        content_hash: hash.to_owned(),
        role_hint: None,
    }
}

fn video_snapshot(
    models: &[&MediaModel],
    attachments: Vec<ComposerAttachment>,
    duration: f64,
) -> ComposerSnapshot {
    ComposerSnapshot {
        mode: ComposerMode::Video,
        prompt: "a comet over a quiet lake".to_owned(),
        duration: Some(ControlValue::DurationSeconds { value: duration }),
        attachments,
        selected: models.iter().copied().map(selected).collect(),
        catalog_fetched_at: Some(fetched_at()),
        ..ComposerSnapshot::default()
    }
}

fn codes(view: &zeron_studio::ComposerView) -> Vec<ConflictCode> {
    view.conflicts
        .iter()
        .map(|conflict| conflict.code)
        .collect()
}

fn conflict(
    view: &zeron_studio::ComposerView,
    code: ConflictCode,
) -> &zeron_studio::ComposerConflict {
    view.conflicts
        .iter()
        .find(|conflict| conflict.code == code)
        .unwrap_or_else(|| panic!("missing {code:?} in {:?}", codes(view)))
}

fn actions(conflict: &zeron_studio::ComposerConflict) -> Vec<ResolveAction> {
    conflict
        .actions
        .iter()
        .map(|action| action.action.clone())
        .collect()
}

fn catalog(models: &[&MediaModel]) -> Vec<MediaModel> {
    models.iter().copied().cloned().collect()
}

#[test]
fn canonical_r2v_plus_t2v_with_stills_is_unsupported_references() {
    let (r2v, t2v, _, _) = catalog_models();
    let images = vec![still("a"), still("b")];
    let snapshot = video_snapshot(&[&r2v], images, 5.0);
    let catalog = catalog(&[&r2v, &t2v]);
    let ready = evaluate_composer(&snapshot, &catalog);
    assert_eq!(ready.phase, ComposerPhase::Editing);
    assert!(ready.send.enabled);
    assert_eq!(map_tray(&snapshot, &r2v).unwrap().len(), 2);
    assert!(
        map_tray(&snapshot, &r2v)
            .unwrap()
            .iter()
            .all(|input| input.role.as_str() == ROLE_REFERENCE)
    );

    let (snapshot, view) = apply_event(
        snapshot,
        &catalog,
        ComposerEvent::SelectModel {
            provider_id: t2v.provider_id.clone(),
            model_id: t2v.id.clone(),
        },
    );
    assert_eq!(view.phase, ComposerPhase::NeedsResolution);
    assert!(!view.send.enabled);
    assert_eq!(codes(&view), vec![ConflictCode::UnsupportedReferences]);
    let blocked = conflict(&view, ConflictCode::UnsupportedReferences);
    assert!(blocked.title.contains("doesn’t accept reference images"));
    assert_eq!(
        popup_conflict(
            &view,
            &ComposerEvent::SelectModel {
                provider_id: t2v.provider_id.clone(),
                model_id: t2v.id.clone(),
            }
        )
        .as_ref(),
        Some(&blocked.id)
    );

    let deselect = ResolveAction::DeselectIncompatibleModels {
        model_ids: vec![t2v.id.clone()],
    };
    let remove = ResolveAction::RemoveUnsupportedReferences {
        asset_ids: snapshot
            .attachments
            .iter()
            .map(|attachment| attachment.id)
            .collect(),
    };
    assert!(actions(blocked).contains(&deselect));
    assert!(actions(blocked).contains(&remove));

    let kept = apply_resolve_checked(snapshot.clone(), &catalog, blocked, &deselect).unwrap();
    let kept = evaluate_composer(&kept, &catalog);
    assert!(kept.send.enabled);
    assert_eq!(kept.models.len(), 1);
    assert_eq!(kept.models[0].model_id, r2v.id);
    assert_eq!(kept.models[0].mapped_inputs.len(), 2);

    let stripped = apply_resolve_checked(snapshot.clone(), &catalog, blocked, &remove).unwrap();
    let stripped = evaluate_composer(&stripped, &catalog);
    assert_eq!(codes(&stripped), vec![ConflictCode::MissingRequiredInput]);
    assert!(stripped.attachments.items.is_empty());

    assert_eq!(
        apply_resolve_checked(snapshot, &catalog, blocked, &ResolveAction::ClearPrompt),
        Err(ResolveError::ActionNotOffered)
    );
}

#[test]
fn i2v_plus_r2v_one_image_is_ready() {
    let (r2v, _, i2v, _) = catalog_models();
    let snapshot = video_snapshot(&[&i2v, &r2v], vec![still("start")], 5.0);
    let catalog = catalog(&[&i2v, &r2v]);
    let view = evaluate_composer(&snapshot, &catalog);
    assert!(view.send.enabled, "{:?}", codes(&view));
    assert_eq!(view.phase, ComposerPhase::Editing);
    assert!(!codes(&view).contains(&ConflictCode::DisjointCapabilities));

    let i2v_inputs = map_tray(&snapshot, &i2v).unwrap();
    assert_eq!(i2v_inputs.len(), 1);
    assert_eq!(i2v_inputs[0].role.as_str(), ROLE_SOURCE);
    let r2v_inputs = map_tray(&snapshot, &r2v).unwrap();
    assert_eq!(r2v_inputs.len(), 1);
    assert_eq!(r2v_inputs[0].role.as_str(), ROLE_REFERENCE);
}

#[test]
fn image_mode_ignores_stills_after_a_video_visit() {
    let (r2v, _, _, t2i) = catalog_models();
    let images = vec![still("keep-a"), still("keep-b")];
    let video = video_snapshot(&[&r2v], images, 5.0);
    let catalog = catalog(&[&r2v, &t2i]);
    let (image, view) = apply_event(
        video.clone(),
        &catalog,
        ComposerEvent::SetMode {
            mode: ComposerMode::Image,
            restore: vec![selected(&t2i)],
        },
    );
    assert_eq!(image.attachments.len(), 2);
    assert_eq!(image.mode, ComposerMode::Image);
    assert!(image.duration.is_none());
    assert_eq!(map_tray(&image, &t2i).unwrap(), Vec::new());
    assert!(view.send.enabled, "{:?}", codes(&view));
    assert_eq!(view.phase, ComposerPhase::Editing);
}

#[test]
fn set_mode_never_drops_attachments() {
    let (r2v, t2v, _, t2i) = catalog_models();
    let attachments = vec![still("stay"), clip("clip", 1_000_000, Some(4.0))];
    let image = ComposerSnapshot {
        mode: ComposerMode::Image,
        prompt: "keep me".to_owned(),
        attachments: attachments.clone(),
        selected: vec![selected(&t2i)],
        ..ComposerSnapshot::default()
    };
    let catalog = catalog(&[&r2v, &t2v, &t2i]);
    let (video, _) = apply_event(
        image,
        &catalog,
        ComposerEvent::SetMode {
            mode: ComposerMode::Video,
            restore: vec![selected(&t2v)],
        },
    );
    assert_eq!(video.attachments.len(), 2);
    assert_eq!(video.prompt, "keep me");
    assert_eq!(
        video.attachments[0].content_hash,
        attachments[0].content_hash
    );
}

#[test]
fn hidden_selected_is_stale_model() {
    let kling = hidden_kling();
    let r2v = r2v("seedance-r2v", 9, &[5.0, 8.0, 10.0]);
    assert!(!kling.is_picker_visible());
    assert!(picker_models(&[kling.clone(), r2v.clone()]).len() == 1);

    let snapshot = video_snapshot(&[&kling], vec![still("ref")], 5.0);
    let view = evaluate_composer(&snapshot, &[kling.clone(), r2v.clone()]);
    assert_eq!(codes(&view), vec![ConflictCode::StaleModel]);
    assert!(!view.send.enabled);
    assert!(!view.open_picker);
    let stale = conflict(&view, ConflictCode::StaleModel);
    assert!(
        actions(stale)
            .iter()
            .any(|action| matches!(action, ResolveAction::DropVanishedModels { .. }))
    );
    assert!(actions(stale).contains(&ResolveAction::RefreshCatalog));
}

#[test]
fn queue_payload_is_estimated_per_run() {
    let left = r2v("r2v-a", 9, &[5.0, 8.0, 10.0]);
    let right = r2v("r2v-b", 9, &[5.0, 8.0, 10.0]);
    let shared = clip("shared-20mb", 20 * 1024 * 1024, Some(4.0));
    let two = video_snapshot(&[&left, &right], vec![shared.clone()], 5.0);
    let catalog = catalog(&[&left, &right]);
    let ok = evaluate_composer(&two, &catalog);
    assert!(
        !codes(&ok).contains(&ConflictCode::QueuePayloadTooLarge),
        "{:?}",
        codes(&ok)
    );
    assert!(ok.send.enabled, "{:?}", codes(&ok));

    let huge = clip("solo-30mb", 30 * 1024 * 1024, Some(4.0));
    let one = video_snapshot(&[&left], vec![huge.clone()], 5.0);
    let blocked = evaluate_composer(&one, &catalog);
    assert_eq!(codes(&blocked), vec![ConflictCode::QueuePayloadTooLarge]);
    let conflict = conflict(&blocked, ConflictCode::QueuePayloadTooLarge);
    assert_eq!(conflict.subjects.model_ids, vec![left.id.clone()]);
    assert_eq!(conflict.subjects.asset_ids, vec![huge.id]);

    let mapped = map_tray(&one, &left).unwrap();
    let estimate = estimate_queue_body_bytes(
        &left,
        &mapped,
        &BTreeMap::new(),
        &one.prompt,
        &BTreeMap::from([(huge.id, huge.byte_size)]),
    );
    assert!(estimate > QUEUE_BODY_LIMIT_BYTES);
}

#[test]
fn duration_clamp_prefers_closer_then_shorter() {
    let short = t2v("short", &[4.0, 6.0, 8.0]);
    let long = t2v("long", &[6.0, 8.0, 12.0]);
    let catalog = catalog(&[&short, &long]);

    let cases = [(7.0, 6.0), (6.5, 6.0), (5.0, 6.0)];
    for (current, expected) in cases {
        let snapshot = ComposerSnapshot {
            duration: Some(ControlValue::DurationSeconds { value: current }),
            ..video_snapshot(&[&short, &long], Vec::new(), current)
        };
        let view = evaluate_composer(&snapshot, &catalog);
        assert_eq!(
            codes(&view),
            vec![ConflictCode::DurationUnsupported],
            "current={current}"
        );
        let clamp = actions(conflict(&view, ConflictCode::DurationUnsupported))
            .into_iter()
            .find_map(|action| match action {
                ResolveAction::ClampDuration { value } => Some(value),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            clamp,
            ControlValue::DurationSeconds { value: expected },
            "current={current}"
        );
    }
}

#[test]
fn disjoint_duration_clique_keeps_current_duration_when_possible() {
    let a = t2v("a", &[4.0, 8.0]);
    let b = t2v("b", &[8.0, 10.0]);
    let c = t2v("c", &[4.0, 10.0]);
    let catalog = catalog(&[&a, &b, &c]);

    let cases: &[(f64, &[&str])] = &[(4.0, &["a", "c"]), (8.0, &["a", "b"]), (10.0, &["b", "c"])];
    for (current, keep) in cases {
        let snapshot = video_snapshot(&[&a, &b, &c], Vec::new(), *current);
        let view = evaluate_composer(&snapshot, &catalog);
        assert_eq!(
            codes(&view),
            vec![ConflictCode::DisjointDurations],
            "current={current}"
        );
        let kept = actions(conflict(&view, ConflictCode::DisjointDurations))
            .into_iter()
            .find_map(|action| match action {
                ResolveAction::KeepModelsDropOthers { model_ids } => Some(model_ids),
                _ => None,
            })
            .unwrap();
        let names: Vec<&str> = kept.iter().map(|id| id.as_str()).collect();
        assert_eq!(names, *keep, "current={current}");
    }
}

#[test]
fn empty_video_set_opens_the_picker() {
    let t2v = t2v("t2v", &[4.0, 6.0, 8.0]);
    let t2i = t2i("flux");
    let start = ComposerSnapshot {
        mode: ComposerMode::Image,
        prompt: "hello".to_owned(),
        selected: vec![selected(&t2i)],
        ..ComposerSnapshot::default()
    };
    let (_, view) = apply_event(
        start,
        &[t2v, t2i],
        ComposerEvent::SetMode {
            mode: ComposerMode::Video,
            restore: Vec::new(),
        },
    );
    assert_eq!(codes(&view), vec![ConflictCode::EmptyModelSet]);
    assert!(view.open_picker);
    assert!(!view.send.enabled);
    assert!(
        actions(conflict(&view, ConflictCode::EmptyModelSet))
            .contains(&ResolveAction::OpenModelPicker)
    );
}

#[test]
fn prompt_too_long_disables_send_without_a_set_prompt_popup() {
    let tight = {
        let mut model = t2v("tight", &[5.0, 8.0, 10.0]);
        model.prompt_maximum_chars = Some(1000);
        model
    };
    let roomy = t2v("roomy", &[5.0, 8.0, 10.0]);
    let catalog = catalog(&[&tight, &roomy]);
    let mut snapshot = video_snapshot(&[&roomy], Vec::new(), 5.0);
    snapshot.prompt = "x".repeat(1400);
    let (snapshot, typed) = apply_event(
        snapshot,
        &catalog,
        ComposerEvent::SetPrompt {
            text: "x".repeat(1400),
        },
    );
    assert!(typed.send.enabled);
    assert!(
        popup_conflict(
            &typed,
            &ComposerEvent::SetPrompt {
                text: snapshot.prompt.clone()
            }
        )
        .is_none()
    );

    let (snapshot, view) = apply_event(
        snapshot,
        &catalog,
        ComposerEvent::SelectModel {
            provider_id: tight.provider_id.clone(),
            model_id: tight.id.clone(),
        },
    );
    assert_eq!(codes(&view), vec![ConflictCode::PromptTooLong]);
    assert!(!view.send.enabled);
    assert_eq!(
        popup_conflict(
            &view,
            &ComposerEvent::SelectModel {
                provider_id: tight.provider_id.clone(),
                model_id: tight.id.clone(),
            }
        )
        .as_ref(),
        Some(&conflict(&view, ConflictCode::PromptTooLong).id)
    );
    assert!(
        popup_conflict(
            &view,
            &ComposerEvent::SetPrompt {
                text: snapshot.prompt.clone(),
            }
        )
        .is_none()
    );
}

#[test]
fn video_output_count_is_forced_to_one() {
    let t2v = t2v("t2v", &[5.0, 8.0]);
    let mut snapshot = video_snapshot(&[&t2v], Vec::new(), 5.0);
    snapshot.selected[0].output_count = 4;
    let (snapshot, view) = apply_event(
        snapshot,
        std::slice::from_ref(&t2v),
        ComposerEvent::SetOutputCount {
            model_id: t2v.id.clone(),
            output_count: 4,
        },
    );
    assert_eq!(snapshot.selected[0].output_count, 1);
    assert_eq!(view.models[0].output_count, 1);
}

#[test]
fn set_duration_ignores_values_outside_the_intersection() {
    let t2v = t2v("t2v", &[4.0, 6.0, 8.0]);
    let snapshot = video_snapshot(&[&t2v], Vec::new(), 6.0);
    let (next, view) = apply_event(
        snapshot,
        std::slice::from_ref(&t2v),
        ComposerEvent::SetDuration {
            value: ControlValue::DurationSeconds { value: 15.0 },
        },
    );
    assert_eq!(
        next.duration,
        Some(ControlValue::DurationSeconds { value: 6.0 })
    );
    assert_eq!(
        view.globals.duration,
        Some(ControlValue::DurationSeconds { value: 6.0 })
    );
    assert!(
        !view
            .globals
            .duration_choices
            .iter()
            .any(|choice| choice.value == ControlValue::DurationSeconds { value: 15.0 })
    );
}

type ClassBuild = (ComposerSnapshot, Vec<MediaModel>, Vec<ConflictCode>, bool);

struct ClassCase {
    name: &'static str,
    build: fn() -> ClassBuild,
}

#[test]
fn conflict_classes_1_through_19() {
    let cases = [
        ClassCase {
            name: "1 image to video with stills on t2v",
            build: || {
                let t2v = t2v("t2v", &[4.0, 6.0, 8.0]);
                let t2i = t2i("flux");
                let snapshot = ComposerSnapshot {
                    mode: ComposerMode::Image,
                    prompt: "stay".to_owned(),
                    attachments: vec![still("one"), still("two")],
                    selected: vec![selected(&t2i)],
                    ..ComposerSnapshot::default()
                };
                let catalog = catalog(&[&t2v, &t2i]);
                let (next, view) = apply_event(
                    snapshot,
                    &catalog,
                    ComposerEvent::SetMode {
                        mode: ComposerMode::Video,
                        restore: vec![selected(&t2v)],
                    },
                );
                assert_eq!(next.attachments.len(), 2);
                (
                    next,
                    catalog,
                    vec![
                        ConflictCode::UnsupportedReferences,
                        ConflictCode::OrphanedAttachments,
                    ],
                    view.open_picker,
                )
            },
        },
        ClassCase {
            name: "2 add t2v beside r2v leftovers",
            build: || {
                let r2v = r2v("r2v", 9, &[5.0, 8.0]);
                let t2v = t2v("t2v", &[5.0, 8.0]);
                (
                    video_snapshot(&[&r2v, &t2v], vec![still("a"), still("b")], 5.0),
                    catalog(&[&r2v, &t2v]),
                    vec![ConflictCode::UnsupportedReferences],
                    false,
                )
            },
        },
        ClassCase {
            name: "3 duration unsupported vs disjoint",
            build: || {
                let short = t2v("short", &[4.0, 6.0, 8.0]);
                let long = t2v("long", &[6.0, 8.0, 12.0]);
                (
                    video_snapshot(&[&short, &long], Vec::new(), 12.0),
                    catalog(&[&short, &long]),
                    vec![ConflictCode::DurationUnsupported],
                    false,
                )
            },
        },
        ClassCase {
            name: "4 duration auto without reference video",
            build: || {
                let mut model = r2v("r2v-auto", 9, &[4.0, 8.0]);
                model
                    .controls
                    .iter_mut()
                    .find(|control| control.id.as_str() == "duration")
                    .unwrap()
                    .choices
                    .push(ControlChoice {
                        value: ControlValue::DurationAuto,
                        label: "Auto".to_owned(),
                    });
                let mut snapshot = video_snapshot(&[&model], vec![still("still")], 8.0);
                snapshot.duration = Some(ControlValue::DurationAuto);
                (
                    snapshot,
                    catalog(&[&model]),
                    vec![ConflictCode::MissingRequiredInput],
                    false,
                )
            },
        },
        ClassCase {
            name: "5 prompt too long",
            build: || {
                let mut model = t2v("tight", &[5.0, 8.0]);
                model.prompt_maximum_chars = Some(8);
                let mut snapshot = video_snapshot(&[&model], Vec::new(), 5.0);
                snapshot.prompt = "123456789".to_owned();
                (
                    snapshot,
                    catalog(&[&model]),
                    vec![ConflictCode::PromptTooLong],
                    false,
                )
            },
        },
        ClassCase {
            name: "6 reference count exceeded",
            build: || {
                let grok = {
                    let mut model = r2v("grok-imagine-reference-to-video", 7, &[5.0, 8.0, 10.0]);
                    model.video.adapter_family = AdapterFamily::Grok;
                    model.video.generate_audio = AudioCapability::None;
                    model
                };
                let seedance = r2v("seedance-r2v", 30, &[5.0, 8.0, 10.0]);
                let images = (0..8).map(|i| still(&format!("img-{i}"))).collect();
                (
                    video_snapshot(&[&grok, &seedance], images, 5.0),
                    catalog(&[&grok, &seedance]),
                    vec![ConflictCode::ReferenceCountExceeded],
                    false,
                )
            },
        },
        ClassCase {
            name: "7 mixed leftover kinds",
            build: || {
                let i2v = i2v("i2v", false);
                (
                    video_snapshot(
                        &[&i2v],
                        vec![still("src"), clip("extra", 800_000, Some(4.0))],
                        6.0,
                    ),
                    catalog(&[&i2v]),
                    vec![
                        ConflictCode::MixedReferenceTypes,
                        ConflictCode::OrphanedAttachments,
                    ],
                    false,
                )
            },
        },
        ClassCase {
            name: "7 audio without visual",
            build: || {
                let r2v = r2v("r2v", 9, &[5.0, 8.0]);
                (
                    video_snapshot(&[&r2v], vec![audio("voice")], 5.0),
                    catalog(&[&r2v]),
                    vec![ConflictCode::AudioWithoutVisual],
                    false,
                )
            },
        },
        ClassCase {
            name: "8 orphaned after dropping the supporting model",
            build: || {
                let r2v = r2v("r2v", 9, &[5.0, 8.0]);
                let t2v = t2v("t2v", &[5.0, 8.0]);
                let snapshot = video_snapshot(&[&r2v, &t2v], vec![still("keep")], 5.0);
                let catalog = catalog(&[&r2v, &t2v]);
                let (next, _) = apply_event(
                    snapshot,
                    &catalog,
                    ComposerEvent::DeselectModel {
                        model_id: r2v.id.clone(),
                    },
                );
                (
                    next,
                    catalog,
                    vec![
                        ConflictCode::UnsupportedReferences,
                        ConflictCode::OrphanedAttachments,
                    ],
                    false,
                )
            },
        },
        ClassCase {
            name: "9 mixed video ops without leftovers are ready",
            build: || {
                let r2v = r2v("r2v", 9, &[5.0, 8.0, 10.0]);
                let i2v = i2v("i2v", false);
                (
                    video_snapshot(&[&i2v, &r2v], vec![still("one")], 8.0),
                    catalog(&[&i2v, &r2v]),
                    Vec::new(),
                    false,
                )
            },
        },
        ClassCase {
            name: "11 empty model set",
            build: || {
                let t2v = t2v("t2v", &[5.0, 8.0]);
                let mut snapshot = video_snapshot(&[&t2v], Vec::new(), 5.0);
                snapshot.selected.clear();
                (
                    snapshot,
                    catalog(&[&t2v]),
                    vec![ConflictCode::EmptyModelSet],
                    true,
                )
            },
        },
        ClassCase {
            name: "12 vanished model",
            build: || {
                let t2v = t2v("t2v", &[5.0, 8.0]);
                let snapshot = ComposerSnapshot {
                    mode: ComposerMode::Video,
                    prompt: "gone".to_owned(),
                    duration: Some(ControlValue::DurationSeconds { value: 5.0 }),
                    selected: vec![SelectedModelRef::new("venice", "vanished-id")],
                    ..ComposerSnapshot::default()
                };
                (
                    snapshot,
                    catalog(&[&t2v]),
                    vec![ConflictCode::StaleModel],
                    false,
                )
            },
        },
        ClassCase {
            name: "12 hidden kling",
            build: || {
                let kling = hidden_kling();
                (
                    video_snapshot(&[&kling], Vec::new(), 5.0),
                    catalog(&[&kling]),
                    vec![ConflictCode::StaleModel],
                    false,
                )
            },
        },
        ClassCase {
            name: "15 mixed image and video output",
            build: || {
                let t2v = t2v("t2v", &[5.0, 8.0]);
                let t2i = t2i("flux");
                (
                    video_snapshot(&[&t2v, &t2i], Vec::new(), 5.0),
                    catalog(&[&t2v, &t2i]),
                    vec![ConflictCode::MixedImageVideoIntent],
                    false,
                )
            },
        },
        ClassCase {
            name: "17 i2v missing source",
            build: || {
                let i2v = i2v("i2v", false);
                (
                    video_snapshot(&[&i2v], Vec::new(), 6.0),
                    catalog(&[&i2v]),
                    vec![ConflictCode::MissingRequiredInput],
                    false,
                )
            },
        },
        ClassCase {
            name: "18 attachment geometry",
            build: || {
                let i2v = i2v("i2v", false);
                let mut tiny = still("tiny");
                tiny.width = Some(64);
                tiny.height = Some(64);
                (
                    video_snapshot(&[&i2v], vec![tiny], 6.0),
                    catalog(&[&i2v]),
                    vec![ConflictCode::AttachmentGeometry],
                    false,
                )
            },
        },
        ClassCase {
            name: "18 attachment too large",
            build: || {
                let r2v = r2v("r2v", 9, &[5.0, 8.0]);
                let mut huge = still("huge");
                huge.byte_size = 40 * 1024 * 1024;
                (
                    video_snapshot(&[&r2v], vec![huge], 5.0),
                    catalog(&[&r2v]),
                    vec![ConflictCode::AttachmentTooLarge],
                    false,
                )
            },
        },
        ClassCase {
            name: "18 attachment duration unproved",
            build: || {
                let r2v = r2v("r2v", 9, &[5.0, 8.0]);
                (
                    video_snapshot(&[&r2v], vec![clip("raw", 800_000, None)], 5.0),
                    catalog(&[&r2v]),
                    vec![ConflictCode::AttachmentDuration],
                    false,
                )
            },
        },
        ClassCase {
            name: "19 queue payload too large",
            build: || {
                let r2v = r2v("r2v", 9, &[5.0, 8.0]);
                (
                    video_snapshot(
                        &[&r2v],
                        vec![clip("30mb", 30 * 1024 * 1024, Some(4.0))],
                        5.0,
                    ),
                    catalog(&[&r2v]),
                    vec![ConflictCode::QueuePayloadTooLarge],
                    false,
                )
            },
        },
    ];

    for case in cases {
        let (snapshot, catalog, expected, open_picker) = (case.build)();
        let view = evaluate_composer(&snapshot, &catalog);
        assert_eq!(codes(&view), expected, "{}", case.name);
        assert_eq!(view.open_picker, open_picker, "{}", case.name);
        if expected.is_empty() {
            assert!(view.send.enabled, "{}", case.name);
            assert!(!codes(&view).contains(&ConflictCode::DisjointCapabilities));
            assert!(!codes(&view).contains(&ConflictCode::ProviderSwitch));
        } else {
            assert!(!view.send.enabled, "{}", case.name);
            assert_eq!(view.phase, ComposerPhase::NeedsResolution, "{}", case.name);
        }
    }
}

#[test]
fn disjoint_durations_has_no_silent_clamp() {
    let a = t2v("a", &[4.0, 6.0]);
    let b = t2v("b", &[10.0, 12.0]);
    let view = evaluate_composer(
        &video_snapshot(&[&a, &b], Vec::new(), 6.0),
        &catalog(&[&a, &b]),
    );
    assert_eq!(codes(&view), vec![ConflictCode::DisjointDurations]);
    assert!(
        actions(conflict(&view, ConflictCode::DisjointDurations))
            .iter()
            .all(|action| !matches!(action, ResolveAction::ClampDuration { .. }))
    );
}

#[test]
fn i2v_last_frame_then_leftover_still() {
    let i2v = i2v("i2v-end", true);
    let snapshot = video_snapshot(
        &[&i2v],
        vec![still("start"), still("end"), still("extra")],
        6.0,
    );
    let mapped_err = map_tray(&snapshot, &i2v).unwrap_err();
    assert_eq!(mapped_err.code, ConflictCode::UnsupportedReferences);
    assert_eq!(mapped_err.subjects.asset_ids.len(), 1);
    assert_eq!(mapped_err.subjects.asset_ids[0], snapshot.attachments[2].id);
}

#[test]
fn i2v_three_stills_without_last_frame_are_leftovers() {
    let i2v = i2v("i2v", false);
    let snapshot = video_snapshot(&[&i2v], vec![still("a"), still("b"), still("c")], 6.0);
    let err = map_tray(&snapshot, &i2v).unwrap_err();
    assert_eq!(err.code, ConflictCode::UnsupportedReferences);
    assert_eq!(err.subjects.asset_ids.len(), 2);
}

#[test]
fn v2v_first_clip_is_source() {
    let v2v = video_model(
        "v2v",
        "v2v",
        MediaOperation::VideoToVideo,
        &[4.0, 6.0, 8.0],
        vec![InputConstraint {
            role: InputRole::from(ROLE_SOURCE),
            minimum_count: 1,
            maximum_count: 1,
            mime: MimeConstraint::accepting(["video/mp4"]),
        }],
        seedance_meta(),
    );
    let snapshot = video_snapshot(&[&v2v], vec![clip("src", 900_000, Some(4.0))], 6.0);
    let inputs = map_tray(&snapshot, &v2v).unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].role.as_str(), ROLE_SOURCE);
    match inputs[0].source {
        GenerationInputSource::Asset { asset_id } => {
            assert_eq!(asset_id, snapshot.attachments[0].id);
        }
        GenerationInputSource::Artifact { .. } => panic!("expected asset source"),
    }
}

#[test]
fn incompatible_upscale_is_not_on_this_composer() {
    let upscale = fixture(UPSCALE);
    let t2i = t2i("flux");
    let snapshot = ComposerSnapshot {
        mode: ComposerMode::Image,
        prompt: "upscale".to_owned(),
        selected: vec![selected(&upscale)],
        ..ComposerSnapshot::default()
    };
    let view = evaluate_composer(&snapshot, &[upscale, t2i]);
    assert_eq!(codes(&view), vec![ConflictCode::IncompatibleModeModels]);
}

#[test]
fn pending_attachment_disables_send_without_a_conflict() {
    let t2v = t2v("t2v", &[5.0, 8.0]);
    let mut pending = still("pending");
    pending.pending = true;
    let view = evaluate_composer(
        &video_snapshot(&[&t2v], vec![pending], 5.0),
        std::slice::from_ref(&t2v),
    );
    assert!(!view.send.enabled);
    assert!(view.conflicts.is_empty());
    assert_eq!(view.phase, ComposerPhase::Editing);
}

#[test]
fn r2v_video_only_satisfies_visual_reference() {
    let r2v = r2v("r2v", 9, &[5.0, 8.0]);
    let view = evaluate_composer(
        &video_snapshot(&[&r2v], vec![clip("ref", 900_000, Some(4.0))], 5.0),
        std::slice::from_ref(&r2v),
    );
    assert!(view.send.enabled, "{:?}", codes(&view));
}

#[test]
fn image_mode_video_attachment_is_unsupported() {
    let t2i = t2i("flux");
    let snapshot = ComposerSnapshot {
        mode: ComposerMode::Image,
        prompt: "still".to_owned(),
        attachments: vec![clip("video", 900_000, Some(4.0))],
        selected: vec![selected(&t2i)],
        ..ComposerSnapshot::default()
    };
    let view = evaluate_composer(&snapshot, std::slice::from_ref(&t2i));
    assert_eq!(codes(&view), vec![ConflictCode::UnsupportedReferences]);
}

#[test]
fn send_event_does_not_mutate_the_snapshot() {
    let t2v = t2v("t2v", &[5.0, 8.0]);
    let snapshot = video_snapshot(&[&t2v], Vec::new(), 5.0);
    let (next, _) = apply_event(
        snapshot.clone(),
        std::slice::from_ref(&t2v),
        ComposerEvent::Send,
    );
    assert_eq!(next, snapshot);
}
