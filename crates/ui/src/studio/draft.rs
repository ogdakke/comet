//! Per-model draft settings and control chrome.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use zeron_proto::StudioTurnView;
use zeron_studio::{ControlId, ControlValue, MediaModel, ModelId};

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
                .filter_map(|control| {
                    control
                        .default
                        .clone()
                        .map(|value| (control.id.clone(), value))
                })
                .collect(),
        }
    }
}

/// Overlay last-used values onto a model's current defaults, dropping controls
/// the catalog no longer accepts.
pub(super) fn overlay_draft(
    model: &MediaModel,
    output_count: u32,
    controls: &BTreeMap<ControlId, ControlValue>,
) -> DraftRunConfig {
    let mut draft = DraftRunConfig::from_model(model);
    draft.output_count = output_count.clamp(1, model.maximum_output_count.max(1));
    for (id, value) in controls {
        if let Some(control) = model.controls.iter().find(|control| &control.id == id)
            && control.validate(value).is_ok()
        {
            draft.controls.insert(id.clone(), value.clone());
        }
    }
    draft
}

/// Select remembered models that still exist in the catalog. Leaves an already
/// populated selection alone.
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

pub(super) fn select_first_model(selected: &mut BTreeSet<ModelId>, catalog: &[MediaModel]) {
    if selected.is_empty()
        && let Some(model) = catalog.first()
    {
        selected.insert(model.id.clone());
    }
}

/// Restore the last turn's models and settings. Unknown catalog models are
/// skipped so a vanished model cannot empty the composer.
pub(super) fn apply_turn_models(
    selected: &mut BTreeSet<ModelId>,
    drafts: &mut HashMap<ModelId, DraftRunConfig>,
    catalog: &[MediaModel],
    turn: &StudioTurnView,
) -> bool {
    let mut next_selected = BTreeSet::new();
    for run in &turn.runs {
        let Some(model) = catalog.iter().find(|model| model.id == run.model.id) else {
            continue;
        };
        next_selected.insert(model.id.clone());
        drafts.insert(
            model.id.clone(),
            overlay_draft(model, run.output_count, &run.controls),
        );
    }
    if next_selected.is_empty() {
        return false;
    }
    *selected = next_selected;
    true
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
        ControlValue::Boolean { value } => if *value { "On" } else { "Off" }.into(),
        ControlValue::Dimensions { width, height } => format!("{width}×{height}"),
        ControlValue::AspectRatio { width, height } => format!("{width}:{height}"),
        ControlValue::AspectRatioAuto => "Auto".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use zeron_proto::{StudioRunState, StudioRunView};
    use zeron_studio::{ControlChoice, ControlKind, ModelControl};

    fn test_model(id: &str, controls: Vec<ModelControl>) -> MediaModel {
        MediaModel {
            provider_id: "venice".into(),
            id: id.into(),
            display_name: id.into(),
            description: None,
            operation: zeron_studio::MediaOperation::TextToImage,
            output_kind: zeron_studio::MediaKind::Image,
            output_mime_types: vec!["image/png".into()],
            input_constraints: Vec::new(),
            prompt_maximum_chars: None,
            negative_prompt_maximum_chars: None,
            maximum_output_count: 8,
            controls,
            pricing: None,
            features: Vec::new(),
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
}
