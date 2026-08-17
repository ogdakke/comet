//! Artifact-viewer upscale action, settings, and background completion work.
//!
//! Upscaling is intentionally separate from the normal generation composer:
//! it is an artifact action with a small persisted settings surface, and its
//! result is saved to Downloads rather than added to the current viewer.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui::{AnyElement, Context, IntoElement, SharedString, div, prelude::*, px};
use zeron_proto::{StudioArtifactView, StudioConversationView, StudioRunState, StudioTurnView};
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
            self.upscale_settings_menu.open(artifact_id);
            cx.notify();
        }
    }

    /// The split action requested by the artifact inspector: a wide primary
    /// action and a compact settings trigger that owns its anchored menu.
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
            .h(px(34.0))
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(7.0))
            .rounded(px(8.0))
            .bg(if available {
                theme.text
            } else {
                crate::theme::wash(0.06)
            })
            .text_color(if available {
                theme.on_solid
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
                .hover(|style| style.opacity(0.88))
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
            .size(px(34.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.0))
            .border_1()
            .border_color(if menu_open {
                theme.border_strong
            } else {
                theme.border
            })
            .text_color(theme.text_muted)
            .when(available, |button| {
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::wash(0.09)))
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
        let display_aspect_ratio = self
            .artifact_frame(artifact_id)
            .and_then(|frame| frame.width.zip(frame.height))
            .filter(|(width, height)| *width > 0 && *height > 0)
            .unwrap_or((1, 1));

        self.upscale_jobs.insert(
            artifact_id,
            UpscaleJob {
                conversation_id,
                run_id: None,
            },
        );
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
                    methods::CREATE_STUDIO_TURN,
                    serde_json::json!({
                        "conversationId": conversation_id,
                        "prompt": "",
                        "runs": [{
                            "providerId": provider_id,
                            "modelId": model_id,
                            "operation": operation,
                            "outputCount": 1,
                            "controls": controls,
                            "inputs": [{
                                "role": "source",
                                "ordinal": 0,
                                "source": {
                                    "source": "artifact",
                                    "artifact_id": artifact_id,
                                },
                                "content_hash": "",
                            }],
                            "manifestVersion": manifest_version,
                            "displayAspectRatio": display_aspect_ratio,
                        }],
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
                            page.observe_upscale_view(&view, cx);
                        }
                        Err(error) => {
                            page.upscale_jobs.remove(&artifact_id);
                            page.error =
                                Some(format!("Upscale response was invalid: {error}").into());
                        }
                    },
                    Err(error) => {
                        page.upscale_jobs.remove(&artifact_id);
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
                Some((
                    *source_id,
                    run.state,
                    run.error.clone(),
                    run.artifacts.clone(),
                ))
            })
            .collect::<Vec<_>>();

        let had_completed = !completed.is_empty();
        for (source_id, state, error, artifacts) in completed {
            self.upscale_jobs.remove(&source_id);
            match state {
                StudioRunState::Succeeded => {
                    for artifact in artifacts {
                        self.queue_upscale_download(&artifact, cx);
                    }
                }
                StudioRunState::Failed => {
                    self.error = Some(
                        format!(
                            "Upscale failed{}",
                            error
                                .as_deref()
                                .map(|message| format!(": {message}"))
                                .unwrap_or_default()
                        )
                        .into(),
                    );
                }
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

    fn queue_upscale_download(&mut self, artifact: &StudioArtifactView, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let destination =
            default_download_directory().join(upscale_file_name(artifact.id, &artifact.mime_type));
        let artifact_id = artifact.id;
        let task = cx.spawn(async move |this, cx| {
            let result = match super::artifact::read_artifact_bytes(&engine, artifact_id).await {
                Ok((_, _, bytes)) => {
                    cx.background_executor()
                        .spawn(async move {
                            if let Some(parent) = destination.parent() {
                                std::fs::create_dir_all(parent)
                                    .map_err(|error| error.to_string())?;
                            }
                            super::artifact::write_artifact_file(destination, bytes)
                        })
                        .await
                }
                Err(error) => Err(error),
            };
            this.update(cx, |page, cx| {
                page.upscale_download_tasks.remove(&artifact_id);
                if let Err(error) = result {
                    page.error = Some(format!("Could not save upscaled image: {error}").into());
                }
                cx.notify();
            })
            .ok();
        });
        self.upscale_download_tasks.insert(artifact_id, task);
    }
}

/// Upscale runs use a Studio turn as their durable engine job, but that turn
/// is an internal artifact operation rather than a user-visible message.
pub(super) fn visible_conversation_view(view: &StudioConversationView) -> StudioConversationView {
    let mut visible = view.clone();
    visible
        .turns
        .retain(|turn| !is_background_upscale_turn(turn));
    visible
}

fn is_background_upscale_turn(turn: &StudioTurnView) -> bool {
    !turn.runs.is_empty()
        && turn
            .runs
            .iter()
            .all(|run| run.model.operation == MediaOperation::Upscale)
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

fn default_download_directory() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("Downloads"))
}

fn upscale_file_name(artifact_id: StudioArtifactId, mime_type: &str) -> String {
    let extension = match mime_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    };
    format!("studio-upscale-{}.{extension}", artifact_id.0)
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
        let regular = test_turn(regular_model, "a fox");
        let upscale = test_turn(upscale_model(), "");
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
                inputs: Vec::new(),
                artifacts: Vec::new(),
            }],
        }
    }
}
