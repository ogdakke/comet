//! Artifact-viewer upscale action, settings, and background completion work.
//!
//! Upscaling is an artifact action with a small persisted settings surface.
//! The result is appended to the source image's turn, next to that image.

use std::collections::BTreeMap;

use gpui::{AnyElement, Context, IntoElement, SharedString, div, prelude::*, px};
#[cfg(test)]
use zeron_proto::StudioTurnView;
use zeron_proto::{StudioConversationView, StudioRunState};
use zeron_rpc::methods;
use zeron_studio::{
    ControlId, ControlValue, GenerationInputSource, MediaModel, MediaOperation, StudioArtifactId,
    StudioConversationId, StudioRunId,
};

use crate::loaders;
use crate::popover;
use crate::theme::Theme;

use super::defaults::UpscaleDefaults;
use super::page::StudioPage;

#[derive(Clone, Debug)]
pub(super) struct UpscaleJob {
    pub(super) conversation_id: StudioConversationId,
    pub(super) run_id: Option<StudioRunId>,
}

impl StudioPage {
    pub(super) fn upscale_is_busy(&self, artifact_id: StudioArtifactId) -> bool {
        self.upscale_jobs.contains_key(&artifact_id)
    }

    pub(super) fn close_upscale_settings_menu(&mut self, cx: &mut Context<Self>) {
        if self.upscale_settings_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.upscale_settings_menu);
            cx.notify();
        }
    }

    pub(super) fn dismiss_upscale_settings_menu(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.keystroke.key == "escape" && self.upscale_settings_menu.is_open() {
            self.close_upscale_settings_menu(cx);
            true
        } else {
            false
        }
    }

    fn toggle_upscale_settings_menu(
        &mut self,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        let pressed_open = self.upscale_settings_menu.take_press_was_open();
        if pressed_open {
            self.close_upscale_settings_menu(cx);
        } else {
            self.close_artifact_actions_menu(cx);
            self.upscale_settings_menu.open(artifact_id);
            cx.notify();
        }
    }

    /// A subdued artifact action with a compact settings trigger that owns its
    /// anchored menu. Upscale intentionally carries no leading icon: it reads
    /// as a utility action alongside the icon-led media transforms.
    pub(super) fn render_artifact_upscale_actions(
        &self,
        artifact_id: StudioArtifactId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.upscale_is_busy(artifact_id);
        let available = self.upscale_models.iter().any(|model| {
            model.operation == MediaOperation::Upscale
                && model.output_kind == zeron_studio::MediaKind::Image
        });
        let menu_open = self.upscale_settings_menu.get() == Some(&artifact_id);
        let menu = menu_open.then(|| self.render_upscale_settings_menu(theme, cx));

        let primary_label = if busy { "Upscaling…" } else { "Upscale" };
        let mut primary = div()
            .id("studio-upscale-artifact")
            .h(px(36.0))
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.0))
            .bg(crate::theme::wash(0.06))
            .text_color(if available {
                theme.text
            } else {
                theme.text_faint
            })
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM);
        if busy {
            primary = primary
                .opacity(0.72)
                .child(loaders::mini_gradient_spinner(
                    format!("studio-upscale-spinner-{}", artifact_id.0),
                    2.0,
                    cx.entity_id(),
                    cx,
                ))
                .child(primary_label);
        } else if available {
            primary = primary
                .cursor_pointer()
                .hover(|style| style.bg(crate::theme::wash(0.10)))
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.start_upscale(artifact_id, cx);
                }))
                .child(primary_label);
        } else {
            primary = primary.opacity(0.58).child(primary_label);
        }

        let settings_id = artifact_id;
        let press_id = artifact_id;
        let mut settings = div()
            .id("studio-upscale-settings")
            .relative()
            .size(px(36.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.0))
            .bg(if menu_open {
                crate::theme::wash(0.10)
            } else {
                crate::theme::wash(0.06)
            })
            .text_color(theme.text_muted)
            .when(available, |button| {
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::wash(0.10)))
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |page, _, _, _| {
                    page.upscale_settings_menu
                        .note_trigger_press_matching(|id| id == &press_id);
                }),
            )
            .when(available, |button| {
                button.on_click(cx.listener(move |page, _, _, cx| {
                    page.toggle_upscale_settings_menu(settings_id, cx);
                }))
            })
            .child(
                crate::icons::icon(crate::icons::TUNING)
                    .size(px(15.0))
                    .text_color(theme.text_muted),
            );
        if let Some(menu) = menu {
            settings = settings.child(menu);
        }

        div()
            .flex()
            .items_center()
            .gap(px(7.0))
            .child(primary)
            .child(settings)
            .into_any_element()
    }

    fn render_upscale_settings_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let model = self.upscale_models.iter().find(|model| {
            model.operation == MediaOperation::Upscale
                && model.output_kind == zeron_studio::MediaKind::Image
        });
        let settings = effective_upscale_defaults(model, &self.remembered.upscale);
        let scale_choices = upscale_scale_choices(model);
        let creativity_control = model.and_then(|model| {
            model
                .controls
                .iter()
                .find(|control| control.id.as_str() == "creativity")
        });
        let creativity_minimum = creativity_control
            .and_then(|control| control.minimum)
            .unwrap_or(0.0);
        let creativity_maximum = creativity_control
            .and_then(|control| control.maximum)
            .unwrap_or(0.02);
        let creativity_step = creativity_control
            .and_then(|control| control.step)
            .unwrap_or(0.001)
            .max(f64::EPSILON);
        let menu_id = self
            .upscale_settings_menu
            .get()
            .copied()
            .unwrap_or_default();

        let mut card = popover::popover_card(theme)
            .w(px(238.0))
            .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                page.close_upscale_settings_menu(cx);
            }))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(popover::menu_heading(theme, "Upscale settings"));

        card = card.child(popover::menu_heading(theme, "Scale"));
        for (value, label) in scale_choices {
            let active = settings.scale == value;
            card = card.child(
                popover::menu_row(theme, active, format!("studio-upscale-scale-{value}"))
                    .id(SharedString::from(format!("studio-upscale-scale-{value}")))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.set_upscale_scale(value, cx);
                    }))
                    .child(SharedString::from(label)),
            );
        }

        card = card.child(popover::menu_separator());
        card = card.child(popover::menu_heading(theme, "Creativity"));
        let minus_id = menu_id;
        let plus_id = menu_id;
        let value = format_creativity(settings.creativity);
        let control_row = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .pb(px(4.0))
            .child(step_button(
                "studio-upscale-creativity-minus",
                "−",
                theme,
                move |page, cx| {
                    page.adjust_upscale_creativity(
                        minus_id,
                        -creativity_step,
                        creativity_minimum,
                        creativity_maximum,
                        creativity_step,
                        cx,
                    );
                },
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .text_center()
                    .text_size(px(11.0))
                    .text_color(theme.text)
                    .child(SharedString::from(value)),
            )
            .child(step_button(
                "studio-upscale-creativity-plus",
                "+",
                theme,
                move |page, cx| {
                    page.adjust_upscale_creativity(
                        plus_id,
                        creativity_step,
                        creativity_minimum,
                        creativity_maximum,
                        creativity_step,
                        cx,
                    );
                },
                cx,
            ));
        card = card.child(control_row);

        popover::anchored_menu_above_end(
            "studio-upscale-settings-menu",
            card.into_any_element(),
            self.upscale_settings_menu.closing_since(),
        )
    }

    fn set_upscale_scale(&mut self, scale: i64, cx: &mut Context<Self>) {
        self.remembered.upscale.scale = scale;
        self.persist_composer_defaults(cx);
        cx.notify();
    }

    fn adjust_upscale_creativity(
        &mut self,
        _artifact_id: StudioArtifactId,
        delta: f64,
        minimum: f64,
        maximum: f64,
        step: f64,
        cx: &mut Context<Self>,
    ) {
        let next = self.remembered.upscale.creativity + delta;
        self.remembered.upscale.creativity = quantize(next, minimum, maximum, step);
        self.persist_composer_defaults(cx);
        cx.notify();
    }

    pub(super) fn start_upscale(&mut self, artifact_id: StudioArtifactId, cx: &mut Context<Self>) {
        if self.upscale_is_busy(artifact_id) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            self.error = Some("Engine not connected".into());
            return;
        };
        let Some(conversation_id) = self.artifact_menu_conversation(artifact_id) else {
            self.error = Some("This image is not attached to a conversation".into());
            return;
        };
        let Some(model) = self
            .upscale_models
            .iter()
            .find(|model| {
                model.operation == MediaOperation::Upscale
                    && model.output_kind == zeron_studio::MediaKind::Image
            })
            .cloned()
        else {
            self.error = Some("No upscale model is available".into());
            return;
        };
        let settings = effective_upscale_defaults(Some(&model), &self.remembered.upscale);
        let controls = upscale_controls(&model, &settings);
        let display_aspect_ratio = self.display_aspect_for(artifact_id);

        self.upscale_jobs.insert(
            artifact_id,
            UpscaleJob {
                conversation_id,
                run_id: None,
            },
        );
        self.pending_edit_source = Some(artifact_id);
        self.ensure_upscale_watch(conversation_id, cx);
        self.close_upscale_settings_menu(cx);
        cx.notify();

        let provider_id = model.provider_id.clone();
        let model_id = model.id.clone();
        let operation = model.operation;
        let manifest_version = model.manifest_version.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::APPEND_STUDIO_DERIVED_RUN,
                    serde_json::json!({
                        "sourceArtifactId": artifact_id,
                        "prompt": "",
                        "run": {
                            "providerId": provider_id,
                            "modelId": model_id,
                            "operation": operation,
                            "outputCount": 1,
                            "controls": controls,
                            "inputs": [],
                            "manifestVersion": manifest_version,
                            "displayAspectRatio": display_aspect_ratio,
                        },
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<StudioConversationView>(value) {
                        Ok(view) => {
                            if let Some(job) = page.upscale_jobs.get_mut(&artifact_id) {
                                job.run_id = find_upscale_run(&view, artifact_id).map(|run| run.id);
                            }
                            page.conversation = Some(view.clone());
                            page.select_pending_derived(&view, artifact_id, cx);
                            page.observe_upscale_view(&view, cx);
                        }
                        Err(error) => {
                            page.upscale_jobs.remove(&artifact_id);
                            page.pending_edit_source = None;
                            page.error =
                                Some(format!("Upscale response was invalid: {error}").into());
                        }
                    },
                    Err(error) => {
                        page.upscale_jobs.remove(&artifact_id);
                        page.pending_edit_source = None;
                        page.error = Some(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn ensure_upscale_watch(
        &mut self,
        conversation_id: StudioConversationId,
        cx: &mut Context<Self>,
    ) {
        if self.upscale_watch_tasks.contains_key(&conversation_id) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let stream = engine
                .client()
                .subscribe(
                    methods::WATCH_STUDIO_CONVERSATION,
                    serde_json::json!({ "conversationId": conversation_id }),
                )
                .await;
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    this.update(cx, |page, cx| {
                        page.upscale_jobs
                            .retain(|_, job| job.conversation_id != conversation_id);
                        page.error = Some(error.to_string().into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            while let Some(value) = stream.recv().await {
                let Ok(view) = serde_json::from_value::<StudioConversationView>(value) else {
                    continue;
                };
                if this
                    .update(cx, |page, cx| page.observe_upscale_view(&view, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.upscale_watch_tasks.insert(conversation_id, task);
    }

    pub(super) fn observe_upscale_view(
        &mut self,
        view: &StudioConversationView,
        cx: &mut Context<Self>,
    ) {
        let completed = self
            .upscale_jobs
            .iter()
            .filter(|(_, job)| job.conversation_id == view.conversation.id)
            .filter_map(|(source_id, job)| {
                let run = job
                    .run_id
                    .and_then(|run_id| find_run(view, run_id))
                    .or_else(|| find_upscale_run(view, *source_id));
                let run = run?;
                if !matches!(
                    run.state,
                    StudioRunState::Succeeded | StudioRunState::Failed | StudioRunState::Cancelled
                ) {
                    return None;
                }
                Some((*source_id, run.state))
            })
            .collect::<Vec<_>>();

        let had_completed = !completed.is_empty();
        for (source_id, state) in completed {
            self.upscale_jobs.remove(&source_id);
            match state {
                StudioRunState::Succeeded | StudioRunState::Failed => {}
                StudioRunState::Cancelled => {
                    self.error = Some("Upscale did not complete".into());
                }
                _ => {}
            }
        }
        if had_completed {
            cx.notify();
        }
    }
}

/// Hide derived-only turns as feed rows. Their images are spliced next to
/// the source by [`super::lineage`].
#[cfg(test)]
pub(super) fn visible_conversation_view(view: &StudioConversationView) -> StudioConversationView {
    let mut visible = view.clone();
    visible.turns = super::lineage::visible_root_turns(view);
    visible
}

fn upscale_controls(
    model: &MediaModel,
    defaults: &UpscaleDefaults,
) -> BTreeMap<ControlId, ControlValue> {
    model
        .controls
        .iter()
        .filter_map(|control| {
            let value = match control.id.as_str() {
                "scale" => ControlValue::Integer {
                    value: defaults.scale,
                },
                "creativity" => ControlValue::Number {
                    value: defaults.creativity,
                },
                _ => control.default.clone()?,
            };
            control
                .validate(&value)
                .ok()
                .map(|_| (control.id.clone(), value))
        })
        .collect()
}

fn effective_upscale_defaults(
    model: Option<&MediaModel>,
    defaults: &UpscaleDefaults,
) -> UpscaleDefaults {
    let mut effective = defaults.clone();
    if let Some(control) = model.and_then(|model| {
        model
            .controls
            .iter()
            .find(|control| control.id.as_str() == "scale")
    }) {
        let value = ControlValue::Integer {
            value: effective.scale,
        };
        if control.validate(&value).is_err() {
            effective.scale = control
                .default
                .as_ref()
                .and_then(|value| match value {
                    ControlValue::Integer { value } => Some(*value),
                    _ => None,
                })
                .or_else(|| {
                    control
                        .choices
                        .iter()
                        .find_map(|choice| match &choice.value {
                            ControlValue::Integer { value } => Some(*value),
                            _ => None,
                        })
                })
                .unwrap_or(2);
        }
    }
    if let Some(control) = model.and_then(|model| {
        model
            .controls
            .iter()
            .find(|control| control.id.as_str() == "creativity")
    }) {
        let value = ControlValue::Number {
            value: effective.creativity,
        };
        if control.validate(&value).is_err() {
            effective.creativity = control
                .default
                .as_ref()
                .and_then(|value| match value {
                    ControlValue::Number { value } => Some(*value),
                    _ => None,
                })
                .or(control.minimum)
                .unwrap_or(0.0);
        }
    }
    effective
}

fn upscale_scale_choices(model: Option<&MediaModel>) -> Vec<(i64, String)> {
    model
        .and_then(|model| {
            model
                .controls
                .iter()
                .find(|control| control.id.as_str() == "scale")
        })
        .map(|control| {
            control
                .choices
                .iter()
                .filter_map(|choice| match &choice.value {
                    ControlValue::Integer { value } => Some((*value, choice.label.clone())),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .filter(|choices| !choices.is_empty())
        .unwrap_or_else(|| vec![(2, "2×".into()), (4, "4×".into())])
}

fn find_upscale_run(
    view: &StudioConversationView,
    source_id: StudioArtifactId,
) -> Option<&zeron_proto::StudioRunView> {
    view.turns
        .iter()
        .flat_map(|turn| turn.runs.iter())
        .find(|run| {
            run.model.operation == MediaOperation::Upscale
                && run.inputs.iter().any(|input| {
                    matches!(
                        input.source,
                        GenerationInputSource::Artifact { artifact_id } if artifact_id == source_id
                    )
                })
        })
}

fn find_run(
    view: &StudioConversationView,
    run_id: StudioRunId,
) -> Option<&zeron_proto::StudioRunView> {
    view.turns
        .iter()
        .flat_map(|turn| turn.runs.iter())
        .find(|run| run.id == run_id)
}

fn quantize(value: f64, minimum: f64, maximum: f64, step: f64) -> f64 {
    let clamped = value.clamp(minimum, maximum);
    let steps = ((clamped - minimum) / step).round();
    let quantized = minimum + steps * step;
    (quantized * 1_000_000.0).round() / 1_000_000.0
}

fn format_creativity(value: f64) -> String {
    if value.abs() < 0.000_5 {
        "0".into()
    } else {
        format!("{value:.3}")
    }
}

fn step_button(
    id: &'static str,
    label: &'static str,
    theme: &Theme,
    click: impl Fn(&mut StudioPage, &mut Context<StudioPage>) + 'static,
    cx: &mut Context<StudioPage>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(26.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .bg(crate::theme::wash(0.05))
        .text_color(theme.text_muted)
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::wash(0.11)))
        .on_click(cx.listener(move |page, _, _, cx| click(page, cx)))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use zeron_studio::{ControlChoice, ControlKind, ModelControl, ModelId, ProviderId};

    fn upscale_model() -> MediaModel {
        MediaModel {
            provider_id: ProviderId::new("venice"),
            id: ModelId::new("upscaler"),
            display_name: "Upscaler".into(),
            description: None,
            operation: MediaOperation::Upscale,
            output_kind: zeron_studio::MediaKind::Image,
            output_mime_types: vec!["image/png".into()],
            input_constraints: Vec::new(),
            prompt_maximum_chars: None,
            negative_prompt_maximum_chars: None,
            maximum_output_count: 1,
            controls: vec![
                ModelControl {
                    id: ControlId::new("scale"),
                    label: "Scale".into(),
                    description: None,
                    kind: ControlKind::Integer,
                    required: true,
                    default: Some(ControlValue::Integer { value: 2 }),
                    minimum: Some(2.0),
                    maximum: Some(4.0),
                    step: Some(2.0),
                    choices: vec![
                        ControlChoice {
                            value: ControlValue::Integer { value: 2 },
                            label: "2×".into(),
                        },
                        ControlChoice {
                            value: ControlValue::Integer { value: 4 },
                            label: "4×".into(),
                        },
                    ],
                    visible_when: Vec::new(),
                },
                ModelControl {
                    id: ControlId::new("creativity"),
                    label: "Creativity".into(),
                    description: None,
                    kind: ControlKind::Number,
                    required: true,
                    default: Some(ControlValue::Number { value: 0.01 }),
                    minimum: Some(0.0),
                    maximum: Some(0.02),
                    step: Some(0.001),
                    choices: Vec::new(),
                    visible_when: Vec::new(),
                },
            ],
            pricing: None,
            features: Vec::new(),
            video: zeron_studio::VideoModelMeta::default(),
            manifest_version: "test-v1".into(),
            fetched_at: Utc::now(),
        }
    }

    #[test]
    fn defaults_are_four_x_with_zero_creativity() {
        let defaults = UpscaleDefaults::default();
        assert_eq!(defaults.scale, 4);
        assert_eq!(defaults.creativity, 0.0);
    }

    #[test]
    fn controls_use_the_persisted_upscale_settings() {
        let settings = UpscaleDefaults {
            scale: 4,
            creativity: 0.007,
        };
        let controls = upscale_controls(&upscale_model(), &settings);
        assert_eq!(
            controls.get(&ControlId::new("scale")),
            Some(&ControlValue::Integer { value: 4 })
        );
        assert_eq!(
            controls.get(&ControlId::new("creativity")),
            Some(&ControlValue::Number { value: 0.007 })
        );
    }

    #[test]
    fn invalid_saved_settings_fall_back_to_model_values() {
        let settings = UpscaleDefaults {
            scale: 8,
            creativity: 0.9,
        };
        let effective = effective_upscale_defaults(Some(&upscale_model()), &settings);
        assert_eq!(effective.scale, 2);
        assert_eq!(effective.creativity, 0.01);
    }

    #[test]
    fn background_upscale_turns_are_removed_from_the_visible_view() {
        let mut regular_model = upscale_model();
        regular_model.operation = MediaOperation::TextToImage;
        let source_id = StudioArtifactId::new();
        let mut regular = test_turn(regular_model, "a fox");
        regular.runs[0]
            .artifacts
            .push(zeron_proto::StudioArtifactView {
                id: source_id,
                output_position: 0,
                media_kind: zeron_studio::MediaKind::Image,
                mime_type: "image/png".into(),
                size_bytes: 1,
                width: Some(1),
                height: Some(1),
                duration_seconds: None,
                metadata: serde_json::Value::Null,
                created_at: Utc::now(),
                thumbhash: None,
                content_hash: String::new(),
            });
        regular.runs[0].state = StudioRunState::Succeeded;
        let mut upscale = test_turn(upscale_model(), "");
        upscale.runs[0].inputs.push(zeron_studio::GenerationInput {
            role: "source".into(),
            ordinal: 0,
            source: GenerationInputSource::Artifact {
                artifact_id: source_id,
            },
            content_hash: String::new(),
        });
        let conversation_id = StudioConversationId::new();
        let view = StudioConversationView {
            conversation: zeron_proto::StudioConversationSummary {
                id: conversation_id,
                title: "Test".into(),
                turn_count: 2,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                archived: false,
                forked_from_turn_id: None,
                creating: true,
                done: false,
            },
            turns: vec![regular, upscale],
        };

        let visible = visible_conversation_view(&view);
        assert_eq!(visible.turns.len(), 1);
        assert_eq!(visible.turns[0].prompt, "a fox");
    }

    #[test]
    fn creativity_steps_stay_on_the_provider_grid() {
        assert_eq!(quantize(0.0006, 0.0, 0.02, 0.001), 0.001);
        assert_eq!(quantize(-1.0, 0.0, 0.02, 0.001), 0.0);
        assert_eq!(quantize(1.0, 0.0, 0.02, 0.001), 0.02);
    }

    fn test_turn(model: MediaModel, prompt: &str) -> StudioTurnView {
        StudioTurnView {
            id: zeron_studio::StudioTurnId::new(),
            position: 0,
            prompt: prompt.into(),
            source_turn_id: None,
            batch_id: zeron_studio::StudioBatchId::new(),
            created_at: Utc::now(),
            runs: vec![zeron_proto::StudioRunView {
                id: zeron_studio::StudioRunId::new(),
                position: 0,
                provider_id: model.provider_id.clone(),
                model,
                controls: BTreeMap::new(),
                output_count: 1,
                display_aspect_ratio: (1, 1),
                state: StudioRunState::Queued,
                progress: None,
                error: None,
                quote: None,
                prompt: None,
                inputs: Vec::new(),
                artifacts: Vec::new(),
            }],
        }
    }
}
