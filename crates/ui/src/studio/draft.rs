//! Per-model draft settings and control chrome.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use zeron_proto::StudioTurnView;
use zeron_studio::{
    AttachmentOrigin, ComposerAttachment, ComposerMediaKind, ComposerMode, ComposerSnapshot,
    ControlId, ControlValue, GenerationInputSource, MediaKind, MediaModel, MediaOperation, ModelId,
    SelectedModelRef, StudioAssetId,
};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RememberedDraft {
    pub(super) output_count: u32,
    #[serde(default)]
    pub(super) controls: BTreeMap<ControlId, ControlValue>,
}

#[derive(Clone, Debug)]
pub(super) struct DraftRunConfig {
    pub(super) output_count: u32,
    pub(super) controls: BTreeMap<zeron_studio::ControlId, zeron_studio::ControlValue>,
}

impl DraftRunConfig {
    pub(super) fn from_model(model: &zeron_studio::MediaModel) -> Self {
        Self {
            output_count: 1,
            controls: model
                .controls
                .iter()
                .filter(|control| control.id.as_str() != "duration")
                .filter_map(|control| {
                    control
                        .default
                        .clone()
                        .or_else(|| control.choices.first().map(|choice| choice.value.clone()))
                        .map(|value| (control.id.clone(), value))
                })
                .collect(),
        }
    }
}

/// Overlay last-used values onto a model's current defaults, dropping controls
/// the catalog no longer accepts. Duration stays global — never a per-chip draft.
pub(super) fn overlay_draft(
    model: &MediaModel,
    output_count: u32,
    controls: &BTreeMap<ControlId, ControlValue>,
) -> DraftRunConfig {
    let mut draft = DraftRunConfig::from_model(model);
    draft.output_count = output_count.clamp(1, model.maximum_output_count.max(1));
    for (id, value) in controls {
        if id.as_str() == "duration" {
            continue;
        }
        if let Some(control) = model.controls.iter().find(|control| &control.id == id)
            && control.validate(value).is_ok()
        {
            draft.controls.insert(id.clone(), value.clone());
        }
    }
    draft
}

pub(super) fn drop_global_duration(
    controls: &BTreeMap<ControlId, ControlValue>,
) -> BTreeMap<ControlId, ControlValue> {
    controls
        .iter()
        .filter(|(id, _)| id.as_str() != "duration")
        .map(|(id, value)| (id.clone(), value.clone()))
        .collect()
}

/// Select remembered models that still exist in the catalog. Leaves an already
/// populated selection alone.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn apply_remembered_selection(
    selected: &mut BTreeSet<ModelId>,
    catalog: &[MediaModel],
    remembered_ids: &[ModelId],
) {
    if !selected.is_empty() {
        return;
    }
    for id in remembered_ids {
        if catalog.iter().any(|model| model.id == *id) {
            selected.insert(id.clone());
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn select_first_model(selected: &mut BTreeSet<ModelId>, catalog: &[MediaModel]) {
    if selected.is_empty()
        && let Some(model) = catalog.first()
    {
        selected.insert(model.id.clone());
    }
}

/// Restore the last turn's models and settings. Unknown catalog models are
/// skipped so a vanished model cannot empty the composer.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn apply_turn_models(
    selected: &mut BTreeSet<ModelId>,
    drafts: &mut HashMap<ModelId, DraftRunConfig>,
    catalog: &[MediaModel],
    turn: &StudioTurnView,
) -> bool {
    let Some(snapshot) = snapshot_from_committed_turn(turn, catalog, &[]) else {
        return false;
    };
    let mut next_selected = BTreeSet::new();
    for selected_ref in &snapshot.selected {
        let Some(model) = catalog
            .iter()
            .find(|model| model.id == selected_ref.model_id)
        else {
            continue;
        };
        next_selected.insert(model.id.clone());
        drafts.insert(
            model.id.clone(),
            overlay_draft(model, selected_ref.output_count, &selected_ref.controls),
        );
    }
    if next_selected.is_empty() {
        return false;
    }
    *selected = next_selected;
    true
}

/// Rebuild a live snapshot from a turn that already passed evaluate+send.
/// Vanished / Hidden / wrong-kind chips are omitted so restore cannot empty
/// the composer onto a stale Hidden row.
pub(super) fn snapshot_from_committed_turn(
    turn: &StudioTurnView,
    catalog: &[MediaModel],
    extra_artifacts: &[zeron_proto::StudioArtifactView],
) -> Option<ComposerSnapshot> {
    if turn.runs.is_empty() {
        return None;
    }
    let video = turn
        .runs
        .iter()
        .all(|run| run.model.output_kind == MediaKind::Video);
    let mode = if video {
        ComposerMode::Video
    } else {
        ComposerMode::Image
    };
    let mut selected: Vec<SelectedModelRef> = Vec::new();
    for run in &turn.runs {
        let Some(model) = catalog.iter().find(|model| model.id == run.model.id) else {
            continue;
        };
        if !model.is_picker_visible() || !model_matches_composer_mode(model, mode) {
            continue;
        }
        let draft = overlay_draft(model, run.output_count, &run.controls);
        // Generate-more extends a turn with another run for the same model,
        // but the composer keeps one chip per model. Merge the runs: sum the
        // counts (capped at the model maximum) and keep the latest controls.
        // Duplicate entries would show one run's count while a send
        // regenerates every run, and chip edits would land on an entry the
        // UI does not display.
        if let Some(existing) = selected
            .iter_mut()
            .find(|existing| existing.model_id == model.id)
        {
            let total = existing.output_count.saturating_add(run.output_count);
            existing.output_count = if video {
                1
            } else {
                total.clamp(1, model.maximum_output_count.max(1))
            };
            existing.controls = draft.controls;
        } else {
            selected.push(SelectedModelRef {
                provider_id: model.provider_id.clone(),
                model_id: model.id.clone(),
                output_count: if video { 1 } else { draft.output_count },
                controls: draft.controls,
            });
        }
    }
    if selected.is_empty() {
        return None;
    }
    let duration = video.then(|| duration_from_runs(turn)).flatten();
    Some(ComposerSnapshot {
        mode,
        prompt: turn.prompt.clone(),
        duration,
        attachments: attachments_from_committed_turn(turn, extra_artifacts),
        selected,
        source_turn_id: Some(turn.id),
        ..ComposerSnapshot::default()
    })
}

fn model_matches_composer_mode(model: &MediaModel, mode: ComposerMode) -> bool {
    match mode {
        ComposerMode::Image => {
            model.output_kind == MediaKind::Image
                && !matches!(
                    model.operation,
                    MediaOperation::ImageEdit | MediaOperation::Upscale
                )
        }
        ComposerMode::Video => model.output_kind == MediaKind::Video,
    }
}

fn duration_from_runs(turn: &StudioTurnView) -> Option<ControlValue> {
    turn.runs.iter().find_map(|run| {
        run.controls
            .iter()
            .find(|(id, value)| {
                id.as_str() == "duration"
                    && matches!(
                        value,
                        ControlValue::DurationSeconds { .. } | ControlValue::DurationAuto
                    )
            })
            .map(|(_, value)| value.clone())
    })
}

fn attachments_from_committed_turn(
    turn: &StudioTurnView,
    extra_artifacts: &[zeron_proto::StudioArtifactView],
) -> Vec<ComposerAttachment> {
    let mut inputs: Vec<_> = turn.runs.iter().flat_map(|run| run.inputs.iter()).collect();
    inputs.sort_by_key(|input| (input.role.as_str().to_owned(), input.ordinal));
    let mut seen = HashSet::new();
    let mut attachments = Vec::new();
    for input in inputs {
        let key = match &input.source {
            GenerationInputSource::Asset { asset_id } => ("asset", asset_id.0),
            GenerationInputSource::Artifact { artifact_id } => ("artifact", artifact_id.0),
        };
        if !seen.insert(key) {
            continue;
        }
        let (id, origin) = match &input.source {
            GenerationInputSource::Asset { asset_id } => (*asset_id, AttachmentOrigin::Asset),
            GenerationInputSource::Artifact { artifact_id } => (
                StudioAssetId(artifact_id.0),
                AttachmentOrigin::Artifact {
                    artifact_id: *artifact_id,
                },
            ),
        };
        let artifact = lookup_attachment_artifact(turn, extra_artifacts, input);
        let Some(artifact) = artifact else {
            // No GetStudioAsset RPC yet. Empty MIME/geometry fails map_tray.
            continue;
        };
        if artifact.mime_type.is_empty() {
            continue;
        }
        attachments.push(ComposerAttachment {
            id,
            kind: attachment_kind(input.role.as_str(), Some(artifact.media_kind)),
            pending: false,
            origin,
            mime_type: artifact.mime_type.clone(),
            byte_size: artifact.size_bytes,
            width: artifact.width,
            height: artifact.height,
            duration_seconds: artifact.duration_seconds,
            content_hash: if input.content_hash.is_empty() {
                artifact.content_hash.clone()
            } else {
                input.content_hash.clone()
            },
            role_hint: Some(input.role.clone()),
        });
    }
    attachments
}

fn lookup_attachment_artifact<'a>(
    turn: &'a StudioTurnView,
    extra_artifacts: &'a [zeron_proto::StudioArtifactView],
    input: &zeron_studio::GenerationInput,
) -> Option<&'a zeron_proto::StudioArtifactView> {
    let mut known = turn
        .runs
        .iter()
        .flat_map(|run| run.artifacts.iter())
        .chain(extra_artifacts.iter());
    known.find(|artifact| match &input.source {
        GenerationInputSource::Artifact { artifact_id } => artifact.id == *artifact_id,
        GenerationInputSource::Asset { .. } => {
            !input.content_hash.is_empty() && artifact.content_hash == input.content_hash
        }
    })
}

fn attachment_kind(role: &str, media_kind: Option<MediaKind>) -> ComposerMediaKind {
    match role {
        zeron_studio::ROLE_REFERENCE_VIDEO => ComposerMediaKind::Video,
        zeron_studio::ROLE_REFERENCE_AUDIO | zeron_studio::ROLE_AUDIO => ComposerMediaKind::Audio,
        _ => match media_kind {
            Some(MediaKind::Video) => ComposerMediaKind::Video,
            _ => ComposerMediaKind::Image,
        },
    }
}

pub(super) fn restore_refs(
    ids: &[ModelId],
    catalog: &[MediaModel],
    drafts: &HashMap<ModelId, DraftRunConfig>,
    fallback_provider: Option<&zeron_studio::ProviderId>,
    mode: ComposerMode,
) -> Vec<SelectedModelRef> {
    ids.iter()
        .map(|id| {
            if let Some(model) = catalog.iter().find(|model| model.id == *id) {
                let draft = drafts
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| DraftRunConfig::from_model(model));
                SelectedModelRef {
                    provider_id: model.provider_id.clone(),
                    model_id: model.id.clone(),
                    output_count: match mode {
                        ComposerMode::Video => 1,
                        ComposerMode::Image => draft.output_count,
                    },
                    controls: draft.controls,
                }
            } else {
                SelectedModelRef {
                    provider_id: fallback_provider
                        .cloned()
                        .unwrap_or_else(|| zeron_studio::ProviderId::new("venice")),
                    model_id: id.clone(),
                    output_count: 1,
                    controls: drafts
                        .get(id)
                        .map(|draft| drop_global_duration(&draft.controls))
                        .unwrap_or_default(),
                }
            }
        })
        .collect()
}

pub(super) fn apply_remembered_drafts(
    drafts: &mut HashMap<ModelId, DraftRunConfig>,
    catalog: &[MediaModel],
    remembered: &BTreeMap<ModelId, RememberedDraft>,
) {
    for model in catalog {
        if drafts.contains_key(&model.id) {
            continue;
        }
        if let Some(draft) = remembered.get(&model.id) {
            drafts.insert(
                model.id.clone(),
                overlay_draft(model, draft.output_count, &draft.controls),
            );
        } else {
            drafts.insert(model.id.clone(), DraftRunConfig::from_model(model));
        }
    }
}

pub(super) fn default_aspect(model: &zeron_studio::MediaModel) -> (u32, u32) {
    model
        .controls
        .iter()
        .find(|control| control.id.as_str() == "aspect_ratio")
        .and_then(|control| control.default.as_ref())
        .and_then(zeron_studio::ControlValue::aspect_ratio_dimensions)
        .or_else(|| {
            model
                .controls
                .iter()
                .find(|control| control.id.as_str() == "aspect_ratio")
                .and_then(|control| {
                    control
                        .choices
                        .iter()
                        .find_map(|choice| choice.value.aspect_ratio_dimensions())
                })
        })
        .unwrap_or((1, 1))
}

pub(super) fn draft_aspect(model: &zeron_studio::MediaModel, draft: &DraftRunConfig) -> (u32, u32) {
    draft
        .controls
        .values()
        .find_map(zeron_studio::ControlValue::aspect_ratio_dimensions)
        .unwrap_or_else(|| default_aspect(model))
}

pub(super) fn control_value_label(value: &zeron_studio::ControlValue) -> String {
    use zeron_studio::ControlValue;
    match value {
        ControlValue::Enum { value } | ControlValue::Resolution { value } => value.clone(),
        ControlValue::Integer { value } => value.to_string(),
        ControlValue::Number { value } | ControlValue::DurationSeconds { value } => {
            value.to_string()
        }
        ControlValue::DurationAuto => "Auto".into(),
        ControlValue::Boolean { value } => if *value { "On" } else { "Off" }.into(),
        ControlValue::Dimensions { width, height } => format!("{width}×{height}"),
        ControlValue::AspectRatio { width, height } => format!("{width}:{height}"),
        ControlValue::AspectRatioAuto => "Auto".into(),
        ControlValue::AspectRatioAdaptive => "Adaptive".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use zeron_proto::{StudioArtifactView, StudioRunState, StudioRunView};
    use zeron_studio::{
        ControlChoice, ControlKind, GenerationInput, InputConstraint, MimeConstraint, ModelControl,
        StudioArtifactId, StudioAssetId, evaluate_composer,
    };

    fn test_model(id: &str, controls: Vec<ModelControl>) -> MediaModel {
        test_model_kind(id, MediaOperation::TextToImage, MediaKind::Image, controls)
    }

    fn test_model_kind(
        id: &str,
        operation: MediaOperation,
        output_kind: MediaKind,
        controls: Vec<ModelControl>,
    ) -> MediaModel {
        MediaModel {
            provider_id: "venice".into(),
            id: id.into(),
            display_name: id.into(),
            description: None,
            operation,
            output_kind,
            output_mime_types: vec![if output_kind == MediaKind::Video {
                "video/mp4".into()
            } else {
                "image/png".into()
            }],
            input_constraints: Vec::new(),
            prompt_maximum_chars: None,
            negative_prompt_maximum_chars: None,
            maximum_output_count: 8,
            controls,
            pricing: None,
            features: Vec::new(),
            video: zeron_studio::VideoModelMeta::default(),
            manifest_version: "test".into(),
            fetched_at: Utc::now(),
        }
    }

    fn aspect_control(default: (u32, u32), extra: &[(u32, u32)]) -> ModelControl {
        let mut choices = vec![ControlChoice {
            value: ControlValue::AspectRatio {
                width: default.0,
                height: default.1,
            },
            label: format!("{}:{}", default.0, default.1),
        }];
        for (width, height) in extra {
            choices.push(ControlChoice {
                value: ControlValue::AspectRatio {
                    width: *width,
                    height: *height,
                },
                label: format!("{width}:{height}"),
            });
        }
        ModelControl {
            id: ControlId::new("aspect_ratio"),
            label: "Aspect".into(),
            description: None,
            kind: ControlKind::AspectRatio,
            required: false,
            default: Some(ControlValue::AspectRatio {
                width: default.0,
                height: default.1,
            }),
            minimum: None,
            maximum: None,
            step: None,
            choices,
            visible_when: Vec::new(),
        }
    }

    fn test_turn(runs: Vec<StudioRunView>) -> StudioTurnView {
        StudioTurnView {
            id: zeron_studio::StudioTurnId::new(),
            position: 0,
            prompt: "ignored".into(),
            source_turn_id: None,
            batch_id: zeron_studio::StudioBatchId::new(),
            runs,
            created_at: Utc::now(),
        }
    }

    fn test_run(
        model: MediaModel,
        output_count: u32,
        controls: BTreeMap<ControlId, ControlValue>,
    ) -> StudioRunView {
        StudioRunView {
            id: zeron_studio::StudioRunId::new(),
            position: 0,
            provider_id: model.provider_id.clone(),
            model,
            controls,
            output_count,
            display_aspect_ratio: (1, 1),
            state: StudioRunState::Succeeded,
            progress: None,
            error: None,
            quote: None,
            prompt: None,
            inputs: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    #[test]
    fn opening_a_chat_preselects_last_turn_models_and_settings() {
        let flux = test_model("flux", vec![aspect_control((1, 1), &[(16, 9)])]);
        let kling = test_model("kling", vec![aspect_control((1, 1), &[(3, 2)])]);
        let catalog = vec![
            test_model("default-first", vec![aspect_control((1, 1), &[])]),
            flux.clone(),
            kling.clone(),
        ];
        let turn = test_turn(vec![
            test_run(
                flux,
                4,
                BTreeMap::from([(
                    ControlId::new("aspect_ratio"),
                    ControlValue::AspectRatio {
                        width: 16,
                        height: 9,
                    },
                )]),
            ),
            test_run(kling, 2, BTreeMap::new()),
        ]);

        let mut selected = BTreeSet::from([ModelId::new("default-first")]);
        let mut drafts = HashMap::new();
        assert!(apply_turn_models(
            &mut selected,
            &mut drafts,
            &catalog,
            &turn
        ));
        assert_eq!(
            selected,
            BTreeSet::from([ModelId::new("flux"), ModelId::new("kling")])
        );
        assert_eq!(drafts[&ModelId::new("flux")].output_count, 4);
        assert_eq!(
            drafts[&ModelId::new("flux")].controls[&ControlId::new("aspect_ratio")],
            ControlValue::AspectRatio {
                width: 16,
                height: 9
            }
        );
        assert_eq!(drafts[&ModelId::new("kling")].output_count, 2);
    }

    #[test]
    fn last_turn_skips_models_that_left_the_catalog() {
        let gone = test_model("retired", Vec::new());
        let flux = test_model("flux", Vec::new());
        let catalog = vec![flux.clone()];
        let turn = test_turn(vec![test_run(gone, 2, BTreeMap::new())]);

        let mut selected = BTreeSet::from([ModelId::new("flux")]);
        let mut drafts = HashMap::new();
        assert!(!apply_turn_models(
            &mut selected,
            &mut drafts,
            &catalog,
            &turn
        ));
        assert_eq!(selected, BTreeSet::from([ModelId::new("flux")]));
        assert!(drafts.is_empty());
    }

    #[test]
    fn overlay_and_from_model_drop_global_duration() {
        let model = test_model_kind(
            "seedance-t2v",
            MediaOperation::TextToVideo,
            MediaKind::Video,
            vec![
                ModelControl {
                    id: ControlId::new("duration"),
                    label: "Duration".into(),
                    description: None,
                    kind: ControlKind::Duration,
                    required: true,
                    default: Some(ControlValue::DurationSeconds { value: 6.0 }),
                    minimum: None,
                    maximum: None,
                    step: None,
                    choices: Vec::new(),
                    visible_when: Vec::new(),
                },
                ModelControl {
                    id: ControlId::new("resolution"),
                    label: "Resolution".into(),
                    description: None,
                    kind: ControlKind::Resolution,
                    required: false,
                    default: Some(ControlValue::Resolution {
                        value: "720p".into(),
                    }),
                    minimum: None,
                    maximum: None,
                    step: None,
                    choices: Vec::new(),
                    visible_when: Vec::new(),
                },
            ],
        );
        let from_model = DraftRunConfig::from_model(&model);
        assert!(
            !from_model
                .controls
                .contains_key(&ControlId::new("duration"))
        );
        assert_eq!(
            from_model.controls[&ControlId::new("resolution")],
            ControlValue::Resolution {
                value: "720p".into()
            }
        );
        let overlaid = overlay_draft(
            &model,
            1,
            &BTreeMap::from([
                (
                    ControlId::new("duration"),
                    ControlValue::DurationSeconds { value: 10.0 },
                ),
                (
                    ControlId::new("resolution"),
                    ControlValue::Resolution {
                        value: "1080p".into(),
                    },
                ),
            ]),
        );
        assert!(!overlaid.controls.contains_key(&ControlId::new("duration")));
        assert_eq!(
            overlaid.controls[&ControlId::new("resolution")],
            ControlValue::Resolution {
                value: "1080p".into()
            }
        );
    }

    #[test]
    fn overlay_drops_invalid_or_unknown_controls() {
        let model = test_model("flux", vec![aspect_control((1, 1), &[(16, 9)])]);
        let draft = overlay_draft(
            &model,
            99,
            &BTreeMap::from([
                (
                    ControlId::new("aspect_ratio"),
                    ControlValue::AspectRatio {
                        width: 4,
                        height: 3,
                    },
                ),
                (
                    ControlId::new("mystery"),
                    ControlValue::Boolean { value: true },
                ),
            ]),
        );
        assert_eq!(draft.output_count, 8);
        assert_eq!(
            draft.controls[&ControlId::new("aspect_ratio")],
            ControlValue::AspectRatio {
                width: 1,
                height: 1
            }
        );
        assert!(!draft.controls.contains_key(&ControlId::new("mystery")));
    }

    #[test]
    fn remembered_selection_wins_over_catalog_default() {
        let catalog = vec![
            test_model("default-first", Vec::new()),
            test_model("flux", Vec::new()),
        ];
        let mut selected = BTreeSet::new();
        apply_remembered_selection(
            &mut selected,
            &catalog,
            &[ModelId::new("missing"), ModelId::new("flux")],
        );
        assert_eq!(selected, BTreeSet::from([ModelId::new("flux")]));

        apply_remembered_selection(&mut selected, &catalog, &[ModelId::new("default-first")]);
        assert_eq!(selected, BTreeSet::from([ModelId::new("flux")]));
    }

    #[test]
    fn image_empty_set_still_selects_the_first_catalog_row() {
        let catalog = vec![
            test_model("default-first", Vec::new()),
            test_model("flux", Vec::new()),
        ];
        let mut selected = BTreeSet::new();
        select_first_model(&mut selected, &catalog);
        assert_eq!(selected, BTreeSet::from([ModelId::new("default-first")]));
        select_first_model(&mut selected, &catalog);
        assert_eq!(selected, BTreeSet::from([ModelId::new("default-first")]));
    }

    #[test]
    fn remembered_drafts_seed_uninitialized_models() {
        let flux = test_model("flux", vec![aspect_control((1, 1), &[(16, 9)])]);
        let mut drafts = HashMap::new();
        let remembered = BTreeMap::from([(
            ModelId::new("flux"),
            RememberedDraft {
                output_count: 3,
                controls: BTreeMap::from([(
                    ControlId::new("aspect_ratio"),
                    ControlValue::AspectRatio {
                        width: 16,
                        height: 9,
                    },
                )]),
            },
        )]);
        apply_remembered_drafts(&mut drafts, &[flux], &remembered);
        assert_eq!(drafts[&ModelId::new("flux")].output_count, 3);
        apply_remembered_drafts(
            &mut drafts,
            &[test_model("flux", vec![aspect_control((1, 1), &[(16, 9)])])],
            &BTreeMap::from([(
                ModelId::new("flux"),
                RememberedDraft {
                    output_count: 1,
                    controls: BTreeMap::new(),
                },
            )]),
        );
        assert_eq!(drafts[&ModelId::new("flux")].output_count, 3);
    }

    #[test]
    fn committed_turn_merges_generate_more_runs_into_one_chip() {
        let flux = test_model("flux", vec![aspect_control((1, 1), &[(16, 9)])]);
        let run = |count, width, height| {
            test_run(
                flux.clone(),
                count,
                BTreeMap::from([(
                    ControlId::new("aspect_ratio"),
                    ControlValue::AspectRatio { width, height },
                )]),
            )
        };
        // Generate-more appended a second run for the same model: 6 + 6 images
        // where the model allows at most 8 per batch.
        let turn = test_turn(vec![run(6, 1, 1), run(6, 16, 9)]);
        let snapshot =
            snapshot_from_committed_turn(&turn, std::slice::from_ref(&flux), &[]).unwrap();
        assert_eq!(snapshot.selected.len(), 1);
        // The count is the summed truth of the turn, capped at the model
        // maximum; the latest run's controls win.
        assert_eq!(snapshot.selected[0].output_count, 8);
        assert_eq!(
            snapshot.selected[0].controls[&ControlId::new("aspect_ratio")],
            ControlValue::AspectRatio {
                width: 16,
                height: 9
            }
        );
    }

    #[test]
    fn committed_turn_merged_count_sums_without_clamping_below_the_cap() {
        let flux = test_model("flux", Vec::new());
        let turn = test_turn(vec![
            test_run(flux.clone(), 2, BTreeMap::new()),
            test_run(flux.clone(), 2, BTreeMap::new()),
        ]);
        let snapshot =
            snapshot_from_committed_turn(&turn, std::slice::from_ref(&flux), &[]).unwrap();
        assert_eq!(snapshot.selected.len(), 1);
        assert_eq!(snapshot.selected[0].output_count, 4);
    }

    #[test]
    fn committed_video_turn_restores_mode_duration_and_tray() {
        let asset_id = StudioAssetId::new();
        let artifact_id = StudioArtifactId::new();
        let mut r2v = test_model_kind(
            "seedance-r2v",
            MediaOperation::ReferenceToVideo,
            MediaKind::Video,
            vec![ModelControl {
                id: ControlId::new("duration"),
                label: "Duration".into(),
                description: None,
                kind: ControlKind::Duration,
                required: true,
                default: None,
                minimum: None,
                maximum: None,
                step: None,
                choices: vec![ControlChoice {
                    value: ControlValue::DurationSeconds { value: 8.0 },
                    label: "8s".into(),
                }],
                visible_when: Vec::new(),
            }],
        );
        r2v.video.adapter_family = zeron_studio::AdapterFamily::Seedance;
        r2v.input_constraints = vec![InputConstraint {
            role: zeron_studio::InputRole::new(zeron_studio::ROLE_REFERENCE),
            minimum_count: 0,
            maximum_count: 9,
            mime: MimeConstraint {
                accepted: vec!["image/png".into(), "image/jpeg".into(), "image/webp".into()],
                ..MimeConstraint::default()
            },
        }];
        let mut run = test_run(
            r2v.clone(),
            2,
            BTreeMap::from([(
                ControlId::new("duration"),
                ControlValue::DurationSeconds { value: 8.0 },
            )]),
        );
        run.inputs = vec![
            GenerationInput {
                role: zeron_studio::InputRole::new(zeron_studio::ROLE_REFERENCE),
                ordinal: 0,
                source: GenerationInputSource::Asset { asset_id },
                content_hash: "abc".into(),
            },
            GenerationInput {
                role: zeron_studio::InputRole::new(zeron_studio::ROLE_REFERENCE),
                ordinal: 1,
                source: GenerationInputSource::Artifact { artifact_id },
                content_hash: "def".into(),
            },
        ];
        run.artifacts = vec![StudioArtifactView {
            id: artifact_id,
            output_position: 0,
            media_kind: MediaKind::Image,
            mime_type: "image/png".into(),
            size_bytes: 12,
            width: Some(64),
            height: Some(64),
            duration_seconds: None,
            metadata: serde_json::json!({}),
            created_at: Utc::now(),
            thumbhash: None,
            content_hash: "def".into(),
        }];
        let turn = test_turn(vec![run]);
        let snapshot =
            snapshot_from_committed_turn(&turn, std::slice::from_ref(&r2v), &[]).unwrap();
        assert_eq!(snapshot.mode, ComposerMode::Video);
        assert_eq!(
            snapshot.duration,
            Some(ControlValue::DurationSeconds { value: 8.0 })
        );
        assert_eq!(snapshot.selected.len(), 1);
        assert_eq!(snapshot.selected[0].output_count, 1);
        assert_eq!(snapshot.attachments.len(), 1);
        assert!(
            snapshot
                .attachments
                .iter()
                .all(|attachment| attachment.id != asset_id),
            "unproved asset stubs must not be restored"
        );
        assert!(matches!(
            snapshot.attachments[0].origin,
            AttachmentOrigin::Artifact { artifact_id: id } if id == artifact_id
        ));
        assert_eq!(snapshot.source_turn_id, Some(turn.id));
        let view = evaluate_composer(&snapshot, std::slice::from_ref(&r2v));
        assert!(view.send.enabled, "{:?}", view.conflicts);
        assert!(
            view.conflicts
                .iter()
                .all(|conflict| !conflict.blocks_send()),
            "{:?}",
            view.conflicts
        );
    }
}
