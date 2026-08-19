//! Studio composer card: model picker, per-model controls, and submit.

use std::collections::BTreeSet;

use gpui::{
    AnyElement, Bounds, Context, DragMoveEvent, ExternalPaths, Focusable as _, KeyDownEvent,
    MouseButton, Pixels, Point, SharedString, Window, canvas, div, point, prelude::*, px, size,
};
use zeron_rpc::{RpcError, methods};
use zeron_studio::{
    AudioCapability, ChipView, ComposerEvent, ComposerMode, ComposerView, ControlChoice,
    ControlValue, MediaKind, MediaModel, MediaOperation, ModelControl, ModelFeature, ModelId,
    ResolveAction, SelectedModelRef, StudioValidationError, apply_event, popup_conflict,
};

use crate::motion;
use crate::popover;
use crate::theme::Theme;

use super::draft::{DraftRunConfig, control_value_label, restore_refs};
use super::page::StudioPage;

const IMAGE_PROMPT_PLACEHOLDER: &str = "Describe the image you want to create";
const VIDEO_PROMPT_PLACEHOLDER: &str = "Describe the video you want to create";

const COMPACT_ACTIONS_INSET: f32 = 205.0;
const DURATION_DRAG_STEP_PX: f32 = 12.0;
const MODEL_CHIPS_FADE: f32 = 24.0;
const DURATION_CHIP_MIN_GLYPHS: &str = "30s";

/// Catalog rows visible in the Studio model picker after favorites, feature
/// filters, operation filters, and search. Starred models stay floated to the top.
fn visible_model_indices(
    models: &[MediaModel],
    query: &str,
    favorites_only: bool,
    favorites: &[ModelId],
    filters: &BTreeSet<ModelFeature>,
    operations: &BTreeSet<MediaOperation>,
) -> Vec<usize> {
    let is_favorite = |id: &ModelId| favorites.iter().any(|favorite| favorite == id);
    let candidates = models
        .iter()
        .enumerate()
        .filter(|(_, model)| {
            if favorites_only && !is_favorite(&model.id) {
                return false;
            }
            if !operations.is_empty() && !operations.contains(&model.operation) {
                return false;
            }
            filters
                .iter()
                .all(|feature| model.features.contains(feature))
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let labels = candidates
        .iter()
        .map(|&index| models[index].display_name.as_str())
        .collect::<Vec<_>>();
    let mut ranked = popover::filter_indices(query, &labels);
    ranked.sort_by_key(|&visible| {
        let model_index = candidates[visible];
        (!is_favorite(&models[model_index].id), visible)
    });
    ranked
        .into_iter()
        .map(|visible| candidates[visible])
        .collect()
}

impl StudioPage {
    pub(super) fn apply_composer_event(
        &mut self,
        event: ComposerEvent,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        self.sync_prompt_into_composer(cx);
        let (snapshot, view) = apply_event(self.composer.clone(), &self.models, event.clone());
        self.composer = snapshot;
        self.sync_from_composer_view(&view, &event);
        self.honor_composer_flags(&event, window, cx);
        self.persist_composer_defaults(cx);
        self.sync_prompt_placeholder(cx);
        cx.notify();
    }

    pub(super) fn reevaluate_composer(&mut self, last_event: Option<&ComposerEvent>) {
        let event = last_event.cloned().unwrap_or(ComposerEvent::Send);
        let view = zeron_studio::evaluate_composer(&self.composer, &self.models);
        self.sync_from_composer_view(&view, &event);
    }

    pub(super) fn sync_prompt_into_composer(&mut self, cx: &Context<Self>) {
        self.composer.prompt = self.prompt.read(cx).text().to_owned();
        self.composer.conversation_id = self.selected_conversation;
        self.composer.source_turn_id = self.source_turn;
    }

    pub(super) fn on_prompt_edited(&mut self, cx: &mut Context<Self>) {
        let text = self.prompt.read(cx).text().to_owned();
        let (snapshot, view) = apply_event(
            {
                let mut snapshot = self.composer.clone();
                snapshot.conversation_id = self.selected_conversation;
                snapshot.source_turn_id = self.source_turn;
                snapshot
            },
            &self.models,
            ComposerEvent::SetPrompt { text },
        );
        self.composer = snapshot;
        self.sync_from_composer_view(
            &view,
            &ComposerEvent::SetPrompt {
                text: self.composer.prompt.clone(),
            },
        );
        self.refresh_draft_quotes(cx);
        cx.notify();
    }

    pub(super) fn sync_from_composer_view(
        &mut self,
        view: &ComposerView,
        last_event: &ComposerEvent,
    ) {
        self.composer_view = view.clone();
        self.selected_models = self
            .composer
            .selected
            .iter()
            .map(|selected| selected.model_id.clone())
            .collect();
        for selected in &self.composer.selected {
            if let Some(model) = self
                .models
                .iter()
                .find(|model| model.id == selected.model_id)
            {
                self.draft_runs.insert(
                    selected.model_id.clone(),
                    super::draft::overlay_draft(model, selected.output_count, &selected.controls),
                );
            } else {
                self.draft_runs.insert(
                    selected.model_id.clone(),
                    DraftRunConfig {
                        output_count: selected.output_count,
                        controls: super::draft::drop_global_duration(&selected.controls),
                    },
                );
            }
        }
        if let Some(id) = self.popup_conflict.as_ref() {
            if !view
                .conflicts
                .iter()
                .any(|conflict| &conflict.id == id && conflict.blocks_send())
            {
                self.popup_conflict = popup_conflict(view, last_event);
                self.conflict_more_open = false;
            }
        } else {
            self.popup_conflict = popup_conflict(view, last_event);
            if self.popup_conflict.is_some() {
                self.conflict_more_open = false;
            }
        }
    }

    fn honor_composer_flags(
        &mut self,
        event: &ComposerEvent,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let selection_event = matches!(
            event,
            ComposerEvent::SetMode { .. }
                | ComposerEvent::DeselectModel { .. }
                | ComposerEvent::ReplaceModels { .. }
                | ComposerEvent::RestoreDraft { .. }
                | ComposerEvent::Resolve { .. }
                | ComposerEvent::CatalogUpdated { .. }
        );
        if self.composer_view.open_picker && selection_event && !self.model_picker.is_open() {
            let from_resolve = matches!(
                event,
                ComposerEvent::Resolve {
                    action: ResolveAction::OpenModelPicker,
                    ..
                }
            );
            if from_resolve || self.popup_conflict.is_none() {
                if let Some(window) = window {
                    self.toggle_model_picker(window, cx);
                } else {
                    self.model_picker.open(());
                }
            }
        }
        if self.composer_view.refresh_catalog {
            self.refresh_studio_catalog(true, cx);
        }
    }

    pub(super) fn set_composer_mode(
        &mut self,
        mode: ComposerMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.composer.mode == mode {
            return;
        }
        self.close_duration_popup(cx);
        self.persist_composer_defaults(cx);
        if mode == ComposerMode::Video {
            self.composer.duration = self.remembered.video_duration.clone();
        }
        let restore = self.restore_refs_for(mode);
        self.apply_composer_event(ComposerEvent::SetMode { mode, restore }, Some(window), cx);
    }

    pub(super) fn set_composer_duration(&mut self, value: ControlValue, cx: &mut Context<Self>) {
        if !self
            .composer_view
            .globals
            .duration_choices
            .iter()
            .any(|choice| choice.value == value)
        {
            return;
        }
        self.apply_composer_event(ComposerEvent::SetDuration { value }, None, cx);
    }

    fn close_duration_popup(&mut self, cx: &mut Context<Self>) {
        self.duration_dragging = false;
        if self.duration_popup.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.duration_popup);
        }
        cx.notify();
    }

    fn begin_duration_drag(&mut self, x: f32, from_chip: bool, cx: &mut Context<Self>) {
        self.duration_popup.note_trigger_press();
        if !self.duration_popup.is_open() {
            self.duration_popup.open(());
        }
        self.duration_dragging = true;
        self.duration_drag_from_chip = from_chip;
        self.duration_drag_moved = false;
        self.duration_drag_origin_x = x;
        self.duration_drag_start_index = current_duration_index(
            self.composer_view.globals.duration.as_ref(),
            &self.composer_view.globals.duration_choices,
        )
        .unwrap_or(0);
        cx.notify();
    }

    fn apply_duration_drag_x(&mut self, x: f32, cx: &mut Context<Self>) {
        if !self.duration_dragging {
            return;
        }
        let choices = &self.composer_view.globals.duration_choices;
        if choices.is_empty() {
            return;
        }
        if (x - self.duration_drag_origin_x).abs() > 3.0 {
            self.duration_drag_moved = true;
        }
        let next = if !self.duration_drag_from_chip
            && let Some(track) = self.duration_track
        {
            snap_duration_from_track(x, track, choices)
        } else {
            let delta = ((x - self.duration_drag_origin_x) / DURATION_DRAG_STEP_PX).round() as i32;
            let index = (self.duration_drag_start_index as i32 + delta)
                .clamp(0, choices.len().saturating_sub(1) as i32) as usize;
            Some(choices[index].value.clone())
        };
        let Some(next) = next else {
            return;
        };
        if self.composer_view.globals.duration.as_ref() == Some(&next) {
            return;
        }
        self.set_composer_duration(next, cx);
    }

    fn end_duration_drag(&mut self, cx: &mut Context<Self>) {
        if !self.duration_dragging {
            return;
        }
        self.duration_dragging = false;
        if self.duration_drag_from_chip
            && !self.duration_drag_moved
            && self.duration_popup.take_press_was_open()
        {
            self.close_duration_popup(cx);
            return;
        }
        cx.notify();
    }

    pub(super) fn restore_refs_for(&self, mode: ComposerMode) -> Vec<SelectedModelRef> {
        let fallback = self
            .providers
            .iter()
            .find(|provider| provider.configured)
            .map(|provider| provider.provider_id.clone());
        restore_refs(
            self.remembered.selected_ids_for(mode),
            &self.models,
            &self.draft_runs,
            fallback.as_ref(),
            mode,
        )
    }

    pub(super) fn remembered_mode_lists(&self) -> (Vec<ModelId>, Vec<ModelId>) {
        let current = self
            .composer
            .selected
            .iter()
            .map(|selected| selected.model_id.clone())
            .collect();
        match self.composer.mode {
            ComposerMode::Image => (current, self.remembered.selected_video_model_ids.clone()),
            ComposerMode::Video => (self.remembered.selected_image_model_ids.clone(), current),
        }
    }

    pub(super) fn sync_prompt_placeholder(&mut self, cx: &mut Context<Self>) {
        let placeholder = match self.composer.mode {
            ComposerMode::Image => IMAGE_PROMPT_PLACEHOLDER,
            ComposerMode::Video => VIDEO_PROMPT_PLACEHOLDER,
        };
        self.prompt
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
    }

    pub(super) fn apply_studio_rpc_error(&mut self, error: RpcError) {
        if let RpcError::FailedStructured { message, payload } = &error
            && let Ok(validation) = serde_json::from_value::<StudioValidationError>(payload.clone())
            && !validation.conflicts.is_empty()
        {
            self.composer_view.conflicts = validation.conflicts;
            self.popup_conflict = self
                .composer_view
                .conflicts
                .iter()
                .find(|conflict| conflict.blocks_send())
                .or(self.composer_view.conflicts.first())
                .map(|conflict| conflict.id.clone());
            self.conflict_more_open = false;
            if self.popup_conflict.is_none() {
                self.error = Some(message.clone().into());
            }
            return;
        }
        self.error = Some(error.to_string().into());
    }

    pub(super) fn refresh_studio_catalog(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let Some(provider_id) = self
            .providers
            .iter()
            .find(|provider| provider.configured)
            .map(|provider| provider.provider_id.clone())
        else {
            return;
        };
        self.catalog_refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::LIST_STUDIO_MODELS,
                    serde_json::json!({ "providerId": provider_id, "refresh": refresh }),
                )
                .await;
            this.update(cx, |page, cx| {
                if let Ok(value) = result
                    && let Ok(response) =
                        serde_json::from_value::<zeron_proto::ListStudioModelsResponse>(value)
                {
                    page.apply_models(response.models);
                    page.apply_composer_event(
                        ComposerEvent::CatalogUpdated {
                            fetched_at: response.fetched_at,
                        },
                        None,
                        cx,
                    );
                }
            })
            .ok();
        }));
    }

    fn toggle_selected_model(&mut self, id: ModelId, cx: &mut Context<Self>) {
        if self.selected_models.contains(&id) {
            self.apply_composer_event(ComposerEvent::DeselectModel { model_id: id }, None, cx);
            return;
        }
        let provider_id = self
            .models
            .iter()
            .find(|model| model.id == id)
            .map(|model| model.provider_id.clone())
            .or_else(|| {
                self.providers
                    .iter()
                    .find(|provider| provider.configured)
                    .map(|provider| provider.provider_id.clone())
            })
            .unwrap_or_else(|| zeron_studio::ProviderId::new("venice"));
        self.apply_composer_event(
            ComposerEvent::SelectModel {
                provider_id,
                model_id: id,
            },
            None,
            cx,
        );
    }

    fn close_model_config_menu(&mut self, cx: &mut Context<Self>) {
        if self.model_config_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.model_config_menu);
        }
        cx.notify();
    }

    fn toggle_model_config_menu(&mut self, model_id: ModelId, cx: &mut Context<Self>) {
        let pressed_open = self.model_config_menu.take_press_was_open();
        if self.model_config_menu.is_open() && self.model_config_menu.as_open() == Some(&model_id) {
            self.close_model_config_menu(cx);
        } else if !pressed_open {
            self.model_config_menu.open(model_id);
            cx.notify();
        }
    }

    fn set_control(
        &mut self,
        model_id: &ModelId,
        control_id: &zeron_studio::ControlId,
        value: zeron_studio::ControlValue,
        cx: &mut Context<Self>,
    ) {
        self.apply_composer_event(
            ComposerEvent::SetModelControl {
                model_id: model_id.clone(),
                control_id: control_id.clone(),
                value,
            },
            None,
            cx,
        );
    }

    pub(super) fn close_model_picker(&mut self, cx: &mut Context<Self>) {
        if self.model_picker.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.model_picker);
        }
        cx.notify();
    }

    pub(super) fn toggle_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pressed_open = self.model_picker.take_press_was_open();
        if self.model_picker.is_open() || pressed_open {
            self.close_model_picker(cx);
            return;
        }

        self.model_picker.open(());
        self.model_search.update(cx, |input, cx| {
            input.set_placeholder("Search models…", cx);
            if !input.text().is_empty() {
                input.set_text("", cx);
            }
        });
        self.model_picker_active = None;
        self.model_picker_favorites = false;
        self.model_picker_filters.clear();
        self.model_picker_operations.clear();
        self.model_picker_scroll.set_offset(Point::default());
        let search_focus = self.model_search.read(cx).focus_handle(cx);
        window.focus(&search_focus, cx);
        cx.notify();
    }

    pub(super) fn filtered_model_indices(&self, cx: &gpui::App) -> Vec<usize> {
        let kind = match self.composer.mode {
            ComposerMode::Image => MediaKind::Image,
            ComposerMode::Video => MediaKind::Video,
        };
        visible_model_indices(
            &self.models,
            self.model_search.read(cx).text(),
            self.model_picker_favorites,
            &self.remembered.favorites,
            &self.model_picker_filters,
            &self.model_picker_operations,
        )
        .into_iter()
        .filter(|&index| self.models[index].output_kind == kind)
        .collect()
    }

    pub(super) fn toggle_model_favorite(&mut self, id: &ModelId, cx: &mut Context<Self>) {
        self.remembered.toggle_favorite(id);
        if let Some(dir) = self.state.read(cx).data_dir.clone()
            && let Err(err) = self.remembered.save(&dir)
        {
            tracing::warn!(error = %err, "studio-defaults save failed");
        }
        let visible = self.filtered_model_indices(cx).len();
        self.model_picker_active = self.model_picker_active.filter(|&active| active < visible);
        cx.notify();
    }

    pub(super) fn show_favorite_models(&mut self, cx: &mut Context<Self>) {
        self.model_picker_favorites = !self.model_picker_favorites;
        self.model_picker_active = None;
        self.model_picker_scroll.set_offset(Point::default());
        cx.notify();
    }

    pub(super) fn toggle_model_feature_filter(
        &mut self,
        feature: ModelFeature,
        cx: &mut Context<Self>,
    ) {
        if !self.model_picker_filters.remove(&feature) {
            self.model_picker_filters.insert(feature);
        }
        self.model_picker_active = None;
        self.model_picker_scroll.set_offset(Point::default());
        cx.notify();
    }

    pub(super) fn toggle_model_operation_filter(
        &mut self,
        operation: MediaOperation,
        cx: &mut Context<Self>,
    ) {
        if !self.model_picker_operations.remove(&operation) {
            self.model_picker_operations.insert(operation);
        }
        self.model_picker_active = None;
        self.model_picker_scroll.set_offset(Point::default());
        cx.notify();
    }

    pub(super) fn activate_model_picker_row(&mut self, cx: &mut Context<Self>) {
        if !self.model_picker.is_open() {
            return;
        }
        let visible = self.filtered_model_indices(cx);
        let Some(model_index) = visible.get(self.model_picker_active.unwrap_or(0)).copied() else {
            return;
        };
        let id = self.models[model_index].id.clone();
        self.toggle_selected_model(id, cx);
    }

    pub(super) fn on_model_picker_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.model_picker.is_open() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        let search_focus = self.model_search.read(cx).focus_handle(cx);
        let search_focused = search_focus.is_focused(window);
        let list_focused = self.model_picker_focus.is_focused(window);
        match key {
            popover::MenuKey::Escape => {
                self.close_model_picker(cx);
                cx.stop_propagation();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.filtered_model_indices(cx).len();
                if search_focused && key == popover::MenuKey::Down {
                    self.model_picker_active = (count > 0).then_some(0);
                    if count > 0 {
                        window.focus(&self.model_picker_focus, cx);
                    }
                } else if list_focused
                    && key == popover::MenuKey::Up
                    && self.model_picker_active == Some(0)
                {
                    self.model_picker_active = None;
                    window.focus(&search_focus, cx);
                } else if list_focused {
                    let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                    self.model_picker_active =
                        popover::menu_step(self.model_picker_active, count, delta);
                } else {
                    return;
                }
                if let Some(active) = self.model_picker_active {
                    self.model_picker_scroll.scroll_to_item(active);
                }
                cx.notify();
                cx.stop_propagation();
            }
            popover::MenuKey::Enter if list_focused => {
                self.activate_model_picker_row(cx);
                cx.stop_propagation();
            }
            _ if list_focused => {
                let modifiers = &event.keystroke.modifiers;
                if modifiers.platform || modifiers.control || modifiers.alt {
                    return;
                }
                let typed = event
                    .keystroke
                    .key_char
                    .as_deref()
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        let key = event.keystroke.key.as_str();
                        if key == "space" {
                            Some(" ".to_owned())
                        } else if key.chars().count() == 1 {
                            Some(key.to_owned())
                        } else {
                            None
                        }
                    });
                if let Some(typed) = typed {
                    let query = self.model_search.read(cx).text().to_owned();
                    window.focus(&search_focus, cx);
                    self.model_search.update(cx, |input, cx| {
                        input.set_text(format!("{query}{typed}"), cx)
                    });
                    cx.stop_propagation();
                } else if event.keystroke.key == "backspace" {
                    let mut query = self.model_search.read(cx).text().to_owned();
                    query.pop();
                    window.focus(&search_focus, cx);
                    self.model_search
                        .update(cx, |input, cx| input.set_text(query, cx));
                    cx.stop_propagation();
                }
            }
            _ => {}
        }
    }

    pub(super) fn adjust_output_count(
        &mut self,
        model_id: &zeron_studio::ModelId,
        delta: i32,
        maximum: u32,
        cx: &mut Context<Self>,
    ) {
        if self.composer.mode == ComposerMode::Video {
            return;
        }
        let Some(draft) = self.draft_runs.get(model_id) else {
            return;
        };
        let next = (draft.output_count as i32 + delta).clamp(1, maximum as i32) as u32;
        self.apply_composer_event(
            ComposerEvent::SetOutputCount {
                model_id: model_id.clone(),
                output_count: next,
            },
            None,
            cx,
        );
    }

    fn adjust_numeric_control(
        &mut self,
        model_id: &ModelId,
        control: &zeron_studio::ModelControl,
        direction: f64,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.draft_runs.get_mut(model_id) else {
            return;
        };
        let current = draft.controls.get(&control.id).or(control.default.as_ref());
        let step = control.step.unwrap_or(1.0).max(f64::EPSILON) * direction;
        let next = match current {
            Some(zeron_studio::ControlValue::Integer { value }) => {
                let minimum = control.minimum.unwrap_or(*value as f64);
                let maximum = control.maximum.unwrap_or(*value as f64);
                zeron_studio::ControlValue::Integer {
                    value: ((*value as f64 + step).clamp(minimum, maximum)).round() as i64,
                }
            }
            Some(zeron_studio::ControlValue::Number { value }) => {
                zeron_studio::ControlValue::Number {
                    value: (*value + step).clamp(
                        control.minimum.unwrap_or(*value),
                        control.maximum.unwrap_or(*value),
                    ),
                }
            }
            Some(zeron_studio::ControlValue::DurationSeconds { value }) => {
                zeron_studio::ControlValue::DurationSeconds {
                    value: (*value + step).clamp(
                        control.minimum.unwrap_or(*value),
                        control.maximum.unwrap_or(*value),
                    ),
                }
            }
            _ => return,
        };
        draft.controls.insert(control.id.clone(), next);
        self.persist_composer_defaults(cx);
        cx.notify();
    }

    fn render_model_control(
        &self,
        model_id: &ModelId,
        control: &zeron_studio::ModelControl,
        draft: &DraftRunConfig,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let current = draft
            .controls
            .get(&control.id)
            .or(control.default.as_ref())
            .or_else(|| control.choices.first().map(|choice| &choice.value));
        let mut choices = div().flex().flex_wrap().gap(px(5.0));
        if control.kind == zeron_studio::ControlKind::Boolean {
            let on = current.is_some_and(|value| {
                matches!(value, zeron_studio::ControlValue::Boolean { value: true })
            });
            for (label, value) in [("Off", false), ("On", true)] {
                let click_model = model_id.clone();
                let click_control = control.id.clone();
                choices = choices.child(
                    config_choice(
                        format!(
                            "studio-control-{}-{}-{label}",
                            model_id.as_str(),
                            control.id.as_str()
                        ),
                        label,
                        on == value,
                        theme,
                    )
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.set_control(
                            &click_model,
                            &click_control,
                            zeron_studio::ControlValue::Boolean { value },
                            cx,
                        )
                    })),
                );
            }
        } else if !control.choices.is_empty() {
            for choice in &control.choices {
                let active = current == Some(&choice.value);
                let click_model = model_id.clone();
                let click_control = control.id.clone();
                let value = choice.value.clone();
                let id = format!(
                    "studio-control-{}-{}-{}",
                    model_id.as_str(),
                    control.id.as_str(),
                    choice.label
                );
                let button = if control.kind == zeron_studio::ControlKind::AspectRatio {
                    config_aspect_choice(id, choice.label.clone(), &choice.value, active, theme)
                } else {
                    config_choice(id, choice.label.clone(), active, theme)
                };
                choices = choices.child(button.on_click(cx.listener(move |page, _, _, cx| {
                    page.set_control(&click_model, &click_control, value.clone(), cx)
                })));
            }
        } else {
            let minus_model = model_id.clone();
            let plus_model = model_id.clone();
            let minus_control = control.clone();
            let plus_control = control.clone();
            let value = current
                .map(control_value_label)
                .unwrap_or_else(|| "Default".into());
            choices = choices.child(
                div()
                    .h(px(30.0))
                    .flex()
                    .items_center()
                    .rounded(px(7.0))
                    .bg(crate::theme::wash(0.05))
                    .child(config_step_button(
                        format!(
                            "studio-control-minus-{}-{}",
                            model_id.as_str(),
                            control.id.as_str()
                        ),
                        "−",
                        move |page, cx| {
                            page.adjust_numeric_control(&minus_model, &minus_control, -1.0, cx)
                        },
                        cx,
                    ))
                    .child(
                        div()
                            .min_w(px(28.0))
                            .text_center()
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(SharedString::from(value)),
                    )
                    .child(config_step_button(
                        format!(
                            "studio-control-plus-{}-{}",
                            model_id.as_str(),
                            control.id.as_str()
                        ),
                        "+",
                        move |page, cx| {
                            page.adjust_numeric_control(&plus_model, &plus_control, 1.0, cx)
                        },
                        cx,
                    )),
            );
        }
        config_section(SharedString::from(control.label.clone()), theme).child(choices)
    }

    fn render_model_config_menu(
        &self,
        model: &MediaModel,
        draft: &DraftRunConfig,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let model_id = model.id.clone();
        let maximum = model.maximum_output_count.max(1);
        let count = draft.output_count;
        let minus_id = model_id.clone();
        let plus_id = model_id.clone();
        let mut controls = div().flex().flex_col().gap(px(12.0));
        let video_mode = self.composer.mode == ComposerMode::Video;
        let chip = self
            .composer_view
            .models
            .iter()
            .find(|chip| chip.model_id == model.id);
        let advertised = chip_popover_controls(model, chip);

        if !video_mode {
            controls = controls.child(
                config_section("Amount", theme).child(
                    div()
                        .w(px(92.0))
                        .h(px(32.0))
                        .flex()
                        .items_center()
                        .rounded(px(8.0))
                        .bg(crate::theme::wash(0.06))
                        .child(config_step_button(
                            format!("studio-output-minus-{}", model.id.as_str()),
                            "−",
                            move |page, cx| page.adjust_output_count(&minus_id, -1, maximum, cx),
                            cx,
                        ))
                        .child(
                            div()
                                .min_w(px(28.0))
                                .text_center()
                                .text_size(px(11.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(SharedString::from(count.to_string())),
                        )
                        .child(config_step_button(
                            format!("studio-output-plus-{}", model.id.as_str()),
                            "+",
                            move |page, cx| page.adjust_output_count(&plus_id, 1, maximum, cx),
                            cx,
                        )),
                ),
            );
        }

        if let Some(aspect) = advertised
            .iter()
            .find(|control| control.id.as_str() == "aspect_ratio")
        {
            controls =
                controls.child(self.render_model_control(&model_id, aspect, draft, theme, cx));
        }

        let mut resolution_reasoning = div().w_full().flex().items_start().gap(px(12.0));
        let mut has_resolution_reasoning = false;
        for id in ["resolution", "reasoning"] {
            if let Some(control) = advertised.iter().find(|control| control.id.as_str() == id) {
                has_resolution_reasoning = true;
                resolution_reasoning = resolution_reasoning.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(self.render_model_control(&model_id, control, draft, theme, cx)),
                );
            }
        }
        if has_resolution_reasoning {
            controls = controls.child(resolution_reasoning);
        }

        if let Some(format) = advertised
            .iter()
            .find(|control| control.id.as_str() == "format")
        {
            controls =
                controls.child(self.render_model_control(&model_id, format, draft, theme, cx));
        }

        for control in advertised {
            if matches!(
                control.id.as_str(),
                "aspect_ratio" | "resolution" | "reasoning" | "format"
            ) {
                continue;
            }
            controls =
                controls.child(self.render_model_control(&model_id, control, draft, theme, cx));
        }

        popover::popover_card(theme)
            .id(SharedString::from(format!(
                "studio-model-options-{}",
                model.id.as_str()
            )))
            .w(px(320.0))
            .max_h(px(420.0))
            .overflow_y_scroll()
            .p(px(12.0))
            .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_model_config_menu(cx)))
            .child(
                div()
                    .mb(px(12.0))
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(SharedString::from(model.display_name.clone())),
            )
            .child(controls)
            .into_any_element()
    }

    fn render_model_config(
        &mut self,
        model: MediaModel,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let draft = self
            .draft_runs
            .get(&model.id)
            .cloned()
            .unwrap_or_else(|| DraftRunConfig::from_model(&model));
        let amount = draft.output_count;
        let chip = self
            .composer_view
            .models
            .iter()
            .find(|chip| chip.model_id == model.id);
        let aspect_label = chip_control_readout(&model, &draft, chip, "aspect_ratio");
        let resolution_label = chip_control_readout(&model, &draft, chip, "resolution");
        let audio_on = chip_audio_enabled(&model, &draft, chip);
        let audio_toggleable = matches!(
            model.video_capability().map(|cap| cap.generate_audio),
            Some(AudioCapability::Configurable { .. })
        );
        let display_name = model.display_name.clone();
        let menu_here = self.model_config_menu.get() == Some(&model.id);
        let menu = menu_here.then(|| {
            popover::anchored_menu_above(
                SharedString::from(format!("studio-model-options-menu-{}", model.id.as_str())),
                self.render_model_config_menu(&model, &draft, theme, cx),
                self.model_config_menu.closing_since(),
            )
        });
        let trigger_id = model.id.clone();
        let press_id = model.id.clone();
        let remove_id = model.id.clone();
        let badge = self
            .composer_view
            .models
            .iter()
            .find(|chip| chip.model_id == model.id)
            .and_then(|chip| chip.badge.clone());

        div()
            .id(SharedString::from(format!(
                "studio-model-config-{}",
                model.id.as_str()
            )))
            .relative()
            .h(px(34.0))
            .max_w(px(310.0))
            .flex_none()
            .rounded(px(14.0))
            .border_1()
            .border_color(if menu_here {
                theme.border_strong
            } else {
                theme.border
            })
            .flex()
            .items_center()
            .gap(px(7.0))
            .pl(px(12.0))
            .pr(px(7.0))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.075)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |page, _, _, _| {
                    page.model_config_menu
                        .note_trigger_press_matching(|id| id == &press_id)
                }),
            )
            .on_click(cx.listener(move |page, _, _, cx| {
                page.toggle_model_config_menu(trigger_id.clone(), cx)
            }))
            .when_some(menu, |card, menu| card.child(menu))
            .child(
                div()
                    .max_w(px(132.0))
                    .truncate()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(SharedString::from(display_name)),
            )
            .when_some(badge, |chip, badge| {
                chip.child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(theme.warning)
                        .child(SharedString::from(badge)),
                )
            })
            .when(self.composer.mode != ComposerMode::Video, |chip| {
                chip.child(config_readout(
                    SharedString::from(format!("{amount}×")),
                    theme,
                ))
            })
            .when_some(aspect_label, |chip, label| {
                chip.child(config_readout(SharedString::from(label), theme))
            })
            .when_some(resolution_label, |chip, label| {
                chip.child(config_readout(SharedString::from(label), theme))
            })
            .when_some(audio_on, |chip, on| {
                let audio_id = model.id.clone();
                chip.child(
                    div()
                        .id(SharedString::from(format!(
                            "studio-model-audio-{}",
                            audio_id.as_str()
                        )))
                        .size(px(22.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(5.0))
                        .bg(crate::theme::wash(0.065))
                        .when(audio_toggleable, |icon| {
                            icon.cursor_pointer()
                                .hover(|style| style.bg(crate::theme::wash(0.10)))
                                .on_click(cx.listener(move |page, _, _, cx| {
                                    cx.stop_propagation();
                                    page.set_control(
                                        &audio_id,
                                        &zeron_studio::ControlId::from("audio"),
                                        ControlValue::Boolean { value: !on },
                                        cx,
                                    );
                                }))
                        })
                        .child(
                            crate::icons::icon(if on {
                                crate::icons::VOLUME_LOUD
                            } else {
                                crate::icons::VOLUME_CROSS
                            })
                            .size(px(13.0))
                            .text_color(if on {
                                theme.text_muted
                            } else {
                                theme.text_faint
                            }),
                        ),
                )
            })
            .child(
                div()
                    .id(SharedString::from(format!(
                        "studio-remove-model-{}",
                        remove_id.as_str()
                    )))
                    .size(px(22.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_click(cx.listener(move |page, _, _, cx| {
                        cx.stop_propagation();
                        page.close_model_config_menu(cx);
                        page.apply_composer_event(
                            ComposerEvent::DeselectModel {
                                model_id: remove_id.clone(),
                            },
                            None,
                            cx,
                        );
                    }))
                    .child(
                        crate::icons::icon(crate::icons::CLOSE)
                            .size(px(11.0))
                            .text_color(theme.text_muted),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_composer(
        &mut self,
        window: &mut Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self
            .models
            .clone()
            .into_iter()
            .filter(|model| self.selected_models.contains(&model.id))
            .collect::<Vec<_>>();
        let mut model_configs = Vec::with_capacity(selected.len());
        for model in selected {
            model_configs.push(self.render_model_config(model, theme, cx));
        }

        let visible_model_indices = self.filtered_model_indices(cx);
        let favorites_view = self.model_picker_favorites;
        let searching = !self.model_search.read(cx).text().trim().is_empty();
        let picker_rows = visible_model_indices
            .iter()
            .map(|model_index| self.models[*model_index].clone())
            .enumerate()
            .map(|(visible_index, model)| {
                let selected = self.selected_models.contains(&model.id);
                let active = self.model_picker_active == Some(visible_index);
                let starred = self.remembered.is_favorite(&model.id);
                let id = model.id.clone();
                let star_id = model.id.clone();
                let features = model.features.clone();
                let mut row = div()
                    .id(SharedString::from(format!("studio-model-{}", id.as_str())))
                    .flex_none()
                    .px(px(8.0))
                    .py(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .on_hover(cx.listener(move |page, hovered: &bool, _, cx| {
                        if *hovered && page.model_picker_active != Some(visible_index) {
                            page.model_picker_active = Some(visible_index);
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.toggle_selected_model(id.clone(), cx);
                    }))
                    .child({
                        let mut copy = div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .w_full()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(SharedString::from(model.display_name)),
                            );
                        if !features.is_empty() {
                            let mut badges = div().flex().items_center().gap(px(4.0));
                            for feature in features {
                                badges = badges.child(feature_badge(theme, feature));
                            }
                            copy = copy.child(badges);
                        }
                        copy
                    });
                if selected {
                    row = row.child(
                        crate::icons::icon(crate::icons::CHECK)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    );
                }
                row = row.child(
                    div()
                        .id(SharedString::from(format!(
                            "studio-model-star-{}",
                            star_id.as_str()
                        )))
                        .flex_none()
                        .size(px(22.0))
                        .rounded(px(6.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|style| style.bg(crate::theme::ink(0.08)))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            cx.stop_propagation();
                            page.toggle_model_favorite(&star_id, cx);
                        }))
                        .child(
                            crate::icons::icon(if starred {
                                crate::icons::STAR_BOLD
                            } else {
                                crate::icons::STAR
                            })
                            .size(px(13.0))
                            .text_color(if starred {
                                theme.warning
                            } else {
                                theme.text_muted.opacity(0.45)
                            }),
                        ),
                );
                if selected {
                    row = row
                        .bg(crate::theme::card_selected_bg())
                        .shadow(crate::theme::card_selected_shadows());
                } else if active {
                    row = row.bg(crate::theme::ink(0.05));
                } else {
                    row = row.hover(|style| style.bg(crate::theme::ink(0.05)));
                }
                row
            })
            .collect::<Vec<_>>();

        let picker = self.model_picker.get().map(|_| {
            let empty = picker_rows.is_empty();
            let empty_copy = if empty && favorites_view && !searching {
                "No starred models yet — hit a row's star"
            } else {
                "No models found"
            };
            let search_row = div()
                .flex_none()
                .h(px(46.0))
                .px(px(10.0))
                .border_b_1()
                .border_color(crate::theme::hairline(0.08))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    crate::icons::icon(crate::icons::MAGNIFER)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(0.7)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(13.0))
                        .child(self.model_search.clone()),
                );
            let mut rail = div()
                .w(px(140.0))
                .flex_none()
                .p(px(4.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    filter_rail_row(
                        "studio-model-rail-favorites",
                        favorites_view,
                        theme,
                        |row| {
                            row.child(
                                crate::icons::icon(crate::icons::STAR_BOLD)
                                    .size(px(13.0))
                                    .text_color(if favorites_view {
                                        theme.text
                                    } else {
                                        theme.text_muted.opacity(0.75)
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(if favorites_view {
                                        theme.text
                                    } else {
                                        theme.text_muted
                                    })
                                    .child("Favorites"),
                            )
                        },
                    )
                    .on_click(cx.listener(|page, _, _, cx| page.show_favorite_models(cx))),
                );
            if self.composer.mode == ComposerMode::Video {
                rail = rail.child(filter_rail_divider());
                for operation in MediaOperation::VIDEO_PICKER {
                    let selected = self.model_picker_operations.contains(&operation);
                    let label = operation.picker_label().unwrap_or_default();
                    rail = rail.child(
                        filter_rail_row(
                            SharedString::from(format!("studio-model-rail-{}", label)),
                            selected,
                            theme,
                            |row| {
                                row.child(
                                    div()
                                        .text_size(px(11.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(if selected {
                                            theme.text
                                        } else {
                                            theme.text_muted
                                        })
                                        .child(label),
                                )
                            },
                        )
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.toggle_model_operation_filter(operation, cx)
                        })),
                    );
                }
            }
            rail = rail.child(filter_rail_divider());
            for feature in ModelFeature::ALL {
                let selected = self.model_picker_filters.contains(&feature);
                rail = rail.child(
                    filter_rail_row(
                        SharedString::from(format!("studio-model-rail-{}", feature.label())),
                        selected,
                        theme,
                        |row| {
                            row.child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(if selected {
                                        theme.text
                                    } else {
                                        theme.text_muted
                                    })
                                    .child(feature.label()),
                            )
                        },
                    )
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.toggle_model_feature_filter(feature, cx)
                    })),
                );
            }
            let list = div()
                .id("studio-model-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.model_picker_scroll)
                .p(px(6.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .when(empty, |list| {
                    list.child(
                        div()
                            .h(px(72.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(empty_copy),
                    )
                })
                .children(picker_rows);
            let pane = div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .bg(crate::theme::ink(0.02))
                .border_l_1()
                .border_color(crate::theme::hairline(0.07))
                .child(search_row)
                .child(list);
            let card = popover::popover_card_flush(theme)
                .id("studio-model-picker")
                .w(px(456.0))
                .h(px(346.0))
                .track_focus(&self.model_picker_focus)
                .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_model_picker(cx)))
                .on_key_down(cx.listener(|page, event: &KeyDownEvent, window, cx| {
                    page.on_model_picker_key_down(event, window, cx)
                }))
                .flex()
                .flex_row()
                .items_stretch()
                .child(rail)
                .child(pane)
                .into_any_element();
            popover::anchored_menu_above(
                "studio-model-menu",
                card,
                self.model_picker.closing_since(),
            )
        });

        let batch_quote = super::cost::selected_batch_quote(
            &self.models,
            &self.selected_models,
            &self.draft_runs,
            &self.live_quotes,
        );
        let blocked = self.busy
            || self.prompt.read(cx).text().trim().is_empty()
            || !self.composer_view.send.enabled;
        let (content_height, text_width, layout_width, has_newline) = {
            let input = self.prompt.read(cx);
            (
                input.measured_content_height(),
                input.measured_text_width(),
                input.measured_layout_width(),
                input.has_newline(),
            )
        };
        let compact_capacity = if self.prompt_expanded {
            layout_width - COMPACT_ACTIONS_INSET
        } else {
            layout_width
        };
        let prompt_expanded = if layout_width > 0.0 {
            crate::composer::composer_flip(
                self.prompt_expanded,
                text_width,
                compact_capacity,
                has_newline,
                false,
            )
        } else {
            self.prompt_expanded
        };
        self.prompt_expanded = prompt_expanded;
        let prompt_height = (content_height + 12.0).clamp(32.0, 220.0);
        // Inner well after `py(6)` — this is the scroll viewport once the
        // card stops growing. Without it the input sizes to content (or the
        // agent 240px cap) and the card just clips.
        let prompt_viewport = (prompt_height - 12.0).max(0.0);
        self.prompt.update(cx, |input, _| {
            input.set_soft_wrap(prompt_expanded);
            input.set_viewport_max(Some(prompt_viewport));
        });
        let body_target = if prompt_expanded {
            prompt_height + 36.0
        } else {
            32.0
        };
        let now_ms =
            self.prompt_morph_clock.elapsed().as_secs_f32() * 1000.0 / crate::motion::speed_scale();
        if self.prompt_target_height > 0.0 && (body_target - self.prompt_target_height).abs() > 0.5
        {
            self.prompt_morph = if crate::motion::reduced_motion(cx) {
                None
            } else {
                Some(crate::composer::FlipMorph {
                    from: self.prompt_last_height,
                    start_ms: now_ms,
                })
            };
        }
        self.prompt_target_height = body_target;
        let (body_height, morphing) = match self.prompt_morph {
            Some(morph) if !morph.done(now_ms) => (morph.height(body_target, now_ms), true),
            _ => {
                self.prompt_morph = None;
                (body_target, false)
            }
        };
        self.prompt_last_height = body_height;
        if morphing {
            window.request_animation_frame();
        }
        let body = div()
            .relative()
            .w_full()
            .h(px(body_height))
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(prompt_height))
                    .px(px(8.0))
                    .py(px(6.0))
                    .flex()
                    .flex_col()
                    .when(!prompt_expanded, |input| {
                        input.pr(px(COMPACT_ACTIONS_INSET))
                    })
                    .child(
                        div()
                            .id("studio-prompt")
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .overflow_hidden()
                            .child(self.prompt.clone()),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(32.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .when(self.source_turn.is_some() && prompt_expanded, |row| {
                        row.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child("Using previous settings"),
                        )
                    })
                    .child(div().flex_1().min_w_0())
                    .child(
                        div()
                            .relative()
                            .when_some(picker, |button, picker| button.child(picker))
                            .child(
                                div()
                                    .id("studio-model-picker-toggle")
                                    .h(px(28.0))
                                    .px(px(8.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .rounded(px(7.0))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(crate::theme::wash(0.08)))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|page, _, _, _| {
                                            page.model_picker.note_trigger_press()
                                        }),
                                    )
                                    .on_click(cx.listener(|page, _, window, cx| {
                                        page.toggle_model_picker(window, cx)
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::PALETTE)
                                            .size(px(13.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .child(SharedString::from(format!(
                                                "{} model{}",
                                                self.selected_models.len(),
                                                if self.selected_models.len() == 1 {
                                                    ""
                                                } else {
                                                    "s"
                                                }
                                            ))),
                                    ),
                            ),
                    )
                    .when_some(batch_quote.as_ref(), |row, quote| {
                        row.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(super::cost::format_quote(quote))),
                        )
                    })
                    .child(
                        div()
                            .id("studio-generate")
                            .size(px(28.0))
                            .flex_none()
                            .rounded_full()
                            .bg(theme.text)
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(blocked, |button| button.opacity(0.35))
                            .when(!blocked, |button| {
                                button
                                    .cursor_pointer()
                                    .hover(|style| style.opacity(0.85))
                                    .on_click(cx.listener(|page, _, _, cx| page.submit(cx)))
                            })
                            .child(
                                crate::icons::icon(crate::icons::ARROW_UP)
                                    .size(px(14.0))
                                    .text_color(theme.bg),
                            ),
                    ),
            );
        let drop_enabled = self.tray_add_enabled();
        let composer = div()
            .relative()
            .w_full()
            // Match the agent composer column (`max-w-3xl`).
            .max_w(px(768.0))
            .occlude()
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.input_glass_bg())
            .when(!theme.is_glass(), |composer| composer.shadow_lg())
            .when(drop_enabled, |composer| {
                composer
                    .on_drag_move::<ExternalPaths>(cx.listener(
                        |this, e: &DragMoveEvent<ExternalPaths>, _, cx| {
                            let inside = e.bounds.contains(&e.event.position);
                            if this.file_drag_active != inside {
                                this.file_drag_active = inside;
                                cx.notify();
                            }
                        },
                    ))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                        this.file_drag_active = false;
                        this.add_dropped_paths(paths.paths().to_vec(), cx);
                        cx.notify();
                    }))
            })
            .px(px(8.0))
            .pt(px(8.0))
            .pb(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .id("studio-model-configs")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(7.0))
                    .when(
                        self.composer.mode == ComposerMode::Video
                            && !self.composer_view.globals.duration_choices.is_empty(),
                        |row| row.child(self.render_duration_control(theme, cx)),
                    )
                    .child(self.render_mode_segment(theme, cx))
                    .child(
                        div().flex_1().min_w_0().child(
                            crate::edge_fade::edge_faded(
                                MODEL_CHIPS_FADE,
                                false,
                                false,
                                div()
                                    .id("studio-model-chips")
                                    .size_full()
                                    .flex()
                                    .flex_row()
                                    .gap(px(7.0))
                                    .overflow_x_scroll()
                                    .track_scroll(&self.model_chips_scroll)
                                    .children(model_configs),
                            )
                            .fade_left(true)
                            .fade_right(true)
                            .fade_overflow_x(&self.model_chips_scroll),
                        ),
                    )
                    .when_some(self.render_prompt_budget(theme), |row, budget| {
                        row.child(budget)
                    }),
            )
            .children(self.render_attachment_tray(theme, cx))
            .child(body);

        div()
            .absolute()
            .left(px(24.0))
            .right(px(24.0))
            .bottom(px(18.0))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .when(drop_enabled, |stack| {
                stack
                    .on_drag_move::<ExternalPaths>(cx.listener(
                        |this, e: &DragMoveEvent<ExternalPaths>, _, cx| {
                            let inside = e.bounds.contains(&e.event.position);
                            if this.file_drag_active != inside {
                                this.file_drag_active = inside;
                                cx.notify();
                            }
                        },
                    ))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                        this.file_drag_active = false;
                        this.add_dropped_paths(paths.paths().to_vec(), cx);
                        cx.notify();
                    }))
            })
            .children(self.render_conflict_popup(theme, cx))
            .children(
                self.popup_conflict
                    .is_none()
                    .then(|| self.render_generate_more_pill(theme, cx))
                    .flatten(),
            )
            .child(crate::frost::frosted(26.0, 16.0, composer))
            .into_any_element()
    }

    fn render_mode_segment(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("studio-mode-segment")
            .h(px(34.0))
            .flex_none()
            .flex()
            .items_center()
            .rounded(px(14.0))
            .border_1()
            .border_color(theme.border)
            .bg(gpui::transparent_black())
            .p(px(3.0))
            .gap(px(2.0))
            .child(mode_segment_chip(
                "studio-mode-image",
                crate::icons::CAMERA,
                self.composer.mode == ComposerMode::Image,
                theme,
                ComposerMode::Image,
                cx,
            ))
            .child(mode_segment_chip(
                "studio-mode-video",
                crate::icons::VIDEOCAMERA,
                self.composer.mode == ComposerMode::Video,
                theme,
                ComposerMode::Video,
                cx,
            ))
    }

    fn render_duration_control(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let current = self.composer_view.globals.duration.clone();
        let label = current
            .as_ref()
            .map(duration_chip_label)
            .unwrap_or_else(|| "Duration".into());
        let sizer = duration_chip_sizer(&self.composer_view.globals.duration_choices);
        let open = self.duration_popup.get().is_some();
        let slider = open.then(|| {
            popover::anchored_menu_above(
                "studio-duration-menu",
                self.render_duration_slider(theme, cx),
                self.duration_popup.closing_since(),
            )
        });
        div()
            .id("studio-duration")
            .relative()
            .h(px(34.0))
            .px(px(10.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(14.0))
            .border_1()
            .border_color(if open {
                theme.border_strong
            } else {
                theme.border
            })
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.075)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|page, event: &gpui::MouseDownEvent, _, cx| {
                    page.begin_duration_drag(f32::from(event.position.x), true, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|page, event: &gpui::MouseMoveEvent, _, cx| {
                if page.duration_dragging && event.dragging() {
                    page.apply_duration_drag_x(f32::from(event.position.x), cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|page, _, _, cx| page.end_duration_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|page, _, _, cx| page.end_duration_drag(cx)),
            )
            .when_some(slider, |chip, slider| chip.child(slider))
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .opacity(0.0)
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(SharedString::from(sizer)),
                    )
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(SharedString::from(label)),
                    ),
            )
    }

    fn render_duration_slider(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let choices = self.composer_view.globals.duration_choices.clone();
        let current = self.composer_view.globals.duration.clone();
        let t = duration_slider_t(current.as_ref(), &choices);
        let entity = cx.weak_entity();
        let has_seconds = choices
            .iter()
            .any(|choice| matches!(choice.value, ControlValue::DurationSeconds { .. }));
        let has_auto = choices
            .iter()
            .any(|choice| matches!(choice.value, ControlValue::DurationAuto));
        popover::popover_card(theme)
            .id("studio-duration-slider")
            .bg(crate::theme::wash(0.06))
            .w(px(220.0))
            .p(px(2.0))
            .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                if !page.duration_dragging {
                    page.close_duration_popup(cx);
                }
            }))
            .when(has_seconds, |card| {
                card.child(
                    div()
                        .id("studio-duration-track")
                        .relative()
                        .w_full()
                        .h(px(24.0))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|page, event: &gpui::MouseDownEvent, _, cx| {
                                page.begin_duration_drag(f32::from(event.position.x), false, cx);
                                page.apply_duration_drag_x(f32::from(event.position.x), cx);
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            canvas(
                                |_, _, _| {},
                                move |bounds, (), window, cx| {
                                    let _ = entity.update(cx, |page, _| {
                                        page.duration_track = Some(bounds);
                                    });
                                    paint_duration_track(
                                        bounds,
                                        t,
                                        crate::theme::wash(0.6),
                                        window,
                                    );
                                    let entity_move = entity.clone();
                                    window.on_mouse_event(
                                        move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                                            if phase != gpui::DispatchPhase::Bubble
                                                || !event.dragging()
                                            {
                                                return;
                                            }
                                            let _ = entity_move.update(cx, |page, cx| {
                                                page.apply_duration_drag_x(
                                                    f32::from(event.position.x),
                                                    cx,
                                                );
                                            });
                                        },
                                    );
                                    let entity_up = entity.clone();
                                    window.on_mouse_event(
                                        move |event: &gpui::MouseUpEvent, phase, _, cx| {
                                            if phase != gpui::DispatchPhase::Bubble
                                                || event.button != MouseButton::Left
                                            {
                                                return;
                                            }
                                            let _ = entity_up.update(cx, |page, cx| {
                                                page.end_duration_drag(cx);
                                            });
                                        },
                                    );
                                },
                            )
                            .size_full(),
                        ),
                )
            })
            .when(has_auto, |card| {
                let active = matches!(current, Some(ControlValue::DurationAuto));
                card.child(
                    config_choice("studio-duration-auto", "Auto", active, theme)
                        .mt(px(8.0))
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.set_composer_duration(ControlValue::DurationAuto, cx);
                        })),
                )
            })
            .into_any_element()
    }

    /// "Generate more" chip — same chrome as the agent-chat jump-to-bottom
    /// pill, floating just above the composer. Extends the latest turn.
    fn render_generate_more_pill(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.latest_extendable_turn().is_none() {
            return None;
        }
        let busy = self.busy;
        Some(
            motion::dialog_in(
                "studio-generate-more",
                div()
                    .id("studio-generate-more-btn")
                    .h(px(30.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .shadow_md()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(11.0))
                    .pr(px(13.0))
                    .bg(motion::hover_blend(
                        "studio-generate-more-pill",
                        theme.surface_raised,
                        theme.surface_raised_hover,
                    ))
                    .when(busy, |pill| pill.opacity(0.35))
                    .when(!busy, |pill| {
                        pill.cursor_pointer()
                            .on_hover(motion::hover_listener("studio-generate-more-pill"))
                            .on_click(cx.listener(|page, _, _, cx| page.generate_more_latest(cx)))
                    })
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("+")),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text)
                            .child(SharedString::from("Generate more")),
                    ),
            )
            .into_any_element(),
        )
    }
}

fn mode_segment_chip(
    id: impl Into<SharedString>,
    icon: &'static str,
    active: bool,
    theme: &Theme,
    mode: ComposerMode,
    cx: &mut gpui::Context<StudioPage>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .h(px(28.0))
        .w(px(36.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(10.0))
        .bg(if active {
            crate::theme::card_selected_bg()
        } else {
            gpui::transparent_black()
        })
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::wash(0.08)))
        .on_click(cx.listener(move |page, _, window, cx| {
            page.set_composer_mode(mode, window, cx);
        }))
        .child(
            crate::icons::icon(icon)
                .size(px(15.0))
                .text_color(if active { theme.text } else { theme.text_muted }),
        )
}

fn config_section(label: impl Into<SharedString>, theme: &Theme) -> gpui::Div {
    div().flex().flex_col().gap(px(6.0)).child(
        div()
            .text_size(px(10.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_faint)
            .child(label.into()),
    )
}

fn config_choice(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    active: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .h(px(28.0))
        .px(px(9.0))
        .flex()
        .items_center()
        .rounded(px(7.0))
        .border_1()
        .border_color(if active {
            theme.border_strong
        } else {
            theme.border
        })
        .bg(if active {
            crate::theme::card_selected_bg()
        } else {
            crate::theme::wash(0.035)
        })
        .text_size(px(10.5))
        .text_color(if active { theme.text } else { theme.text_muted })
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::wash(0.09)))
        .child(label.into())
}

fn config_aspect_choice(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    value: &zeron_studio::ControlValue,
    active: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let dimensions = value.aspect_ratio_dimensions();
    let (width, height) = dimensions
        .map(|(width, height)| {
            let ratio = width as f32 / height.max(1) as f32;
            if ratio >= 1.0 {
                (22.0, (22.0 / ratio).clamp(7.0, 18.0))
            } else {
                ((18.0 * ratio).clamp(7.0, 18.0), 18.0)
            }
        })
        .unwrap_or((18.0, 18.0));
    let preview = div()
        .w(px(24.0))
        .h(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .w(px(width))
                .h(px(height))
                .rounded(px(2.0))
                .border_1()
                .border_color(if active {
                    theme.text_muted
                } else {
                    theme.text_faint
                })
                .when(dimensions.is_none(), |preview| {
                    preview
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(8.0))
                        .text_color(theme.text_faint)
                        .child("A")
                }),
        );
    div()
        .id(id.into())
        .w(px(67.0))
        .h(px(50.0))
        .flex_none()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(3.0))
        .rounded(px(7.0))
        .border_1()
        .border_color(if active {
            theme.border_strong
        } else {
            theme.border
        })
        .bg(if active {
            crate::theme::card_selected_bg()
        } else {
            crate::theme::wash(0.035)
        })
        .text_size(px(9.5))
        .text_color(if active { theme.text } else { theme.text_muted })
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::wash(0.09)))
        .child(preview)
        .child(label.into())
}

/// Advertised chip knobs. Duration is global. Audio only when Configurable.
fn chip_popover_controls<'a>(
    model: &'a MediaModel,
    chip: Option<&'a ChipView>,
) -> Vec<&'a ModelControl> {
    let advertised = chip
        .map(|chip| chip.controls.as_slice())
        .unwrap_or(model.controls.as_slice());
    advertised
        .iter()
        .filter(|control| popover_shows_control(model, control))
        .collect()
}

fn popover_shows_control(model: &MediaModel, control: &ModelControl) -> bool {
    match control.id.as_str() {
        "duration" | "steps" | "safe_mode" => false,
        "audio" => matches!(
            model.video_capability().map(|cap| cap.generate_audio),
            Some(AudioCapability::Configurable { .. })
        ),
        _ => true,
    }
}

fn chip_control_readout(
    model: &MediaModel,
    draft: &DraftRunConfig,
    chip: Option<&ChipView>,
    control_id: &str,
) -> Option<String> {
    let control = chip_popover_controls(model, chip)
        .into_iter()
        .find(|control| control.id.as_str() == control_id)?;
    draft
        .controls
        .get(&control.id)
        .or(control.default.as_ref())
        .or_else(|| chip.and_then(|chip| chip.values.get(&control.id)))
        .or_else(|| control.choices.first().map(|choice| &choice.value))
        .map(control_value_label)
}

fn chip_audio_enabled(
    model: &MediaModel,
    draft: &DraftRunConfig,
    chip: Option<&ChipView>,
) -> Option<bool> {
    match model.video_capability().map(|cap| cap.generate_audio)? {
        AudioCapability::None => None,
        AudioCapability::ForcedOn => Some(true),
        AudioCapability::Configurable { default } => {
            let value = draft
                .controls
                .get(&zeron_studio::ControlId::from("audio"))
                .or_else(|| {
                    chip.and_then(|chip| chip.values.get(&zeron_studio::ControlId::from("audio")))
                })
                .cloned()
                .or_else(|| Some(ControlValue::Boolean { value: default }));
            Some(matches!(value, Some(ControlValue::Boolean { value: true })))
        }
    }
}

fn duration_chip_label(value: &ControlValue) -> String {
    match value {
        ControlValue::DurationSeconds { value } if value.fract() == 0.0 => {
            format!("{}s", *value as i64)
        }
        ControlValue::DurationSeconds { value } => format!("{value}s"),
        ControlValue::DurationAuto => "Auto".into(),
        _ => "Duration".into(),
    }
}

/// Invisible sizer so `4s` → `30s` (or `Auto`) does not shift the mode bar.
fn duration_chip_sizer(choices: &[ControlChoice]) -> String {
    let mut widest = DURATION_CHIP_MIN_GLYPHS.to_owned();
    for choice in choices {
        let label = duration_chip_label(&choice.value);
        if label.chars().count() > widest.chars().count() {
            widest = label;
        }
    }
    widest
}

fn current_duration_index(
    current: Option<&ControlValue>,
    choices: &[ControlChoice],
) -> Option<usize> {
    let current = current?;
    choices.iter().position(|choice| &choice.value == current)
}

fn duration_seconds_span(choices: &[ControlChoice]) -> Option<(f64, f64)> {
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    for choice in choices {
        if let ControlValue::DurationSeconds { value } = choice.value {
            min = min.min(value);
            max = max.max(value);
        }
    }
    (min.is_finite() && max.is_finite() && max >= min).then_some((min, max))
}

fn snap_duration_from_track(
    x: f32,
    track: Bounds<Pixels>,
    choices: &[ControlChoice],
) -> Option<ControlValue> {
    let (min, max) = duration_seconds_span(choices)?;
    let left = f32::from(track.origin.x);
    let width = f32::from(track.size.width).max(1.0);
    let t = ((x - left) / width).clamp(0.0, 1.0) as f64;
    let seconds = if (max - min).abs() < f64::EPSILON {
        min
    } else {
        min + t * (max - min)
    };
    choices
        .iter()
        .filter_map(|choice| match choice.value {
            ControlValue::DurationSeconds { value } => Some((value, choice.value.clone())),
            _ => None,
        })
        .min_by(|left, right| {
            (left.0 - seconds)
                .abs()
                .partial_cmp(&(right.0 - seconds).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(_, value)| value)
}

fn duration_slider_t(current: Option<&ControlValue>, choices: &[ControlChoice]) -> f32 {
    let Some((min, max)) = duration_seconds_span(choices) else {
        return 0.0;
    };
    let seconds = match current {
        Some(ControlValue::DurationSeconds { value }) => *value,
        _ => min,
    };
    if (max - min).abs() < f64::EPSILON {
        return 0.0;
    }
    ((seconds - min) / (max - min)).clamp(0.0, 1.0) as f32
}

fn paint_duration_track(bounds: Bounds<Pixels>, t: f32, ink: gpui::Hsla, window: &mut Window) {
    let mid_y = bounds.origin.y + bounds.size.height / 2.0;
    let left = bounds.origin.x + px(8.0);
    let right = bounds.origin.x + bounds.size.width - px(8.0);
    let width = right - left;
    window.paint_quad(gpui::quad(
        Bounds {
            origin: point(left, mid_y - px(4.0)),
            size: size(width, px(8.0)),
        },
        px(4.0),
        ink.opacity(0.28),
        px(0.0),
        gpui::transparent_black(),
        gpui::BorderStyle::default(),
    ));
    let fill = width * t.clamp(0.0, 1.0);
    window.paint_quad(gpui::quad(
        Bounds {
            origin: point(left, mid_y - px(4.0)),
            size: size(fill, px(8.0)),
        },
        px(4.0),
        ink,
        px(0.0),
        gpui::transparent_black(),
        gpui::BorderStyle::default(),
    ));
}

fn config_readout(label: SharedString, theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(2.0))
        .rounded(px(5.0))
        .bg(crate::theme::wash(0.065))
        .text_size(px(10.0))
        .text_color(theme.text_muted)
        .child(label)
}

fn config_step_button(
    id: impl Into<SharedString>,
    label: &'static str,
    click: impl Fn(&mut StudioPage, &mut Context<StudioPage>) + 'static,
    cx: &mut Context<StudioPage>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .size(px(32.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.0))
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::wash(0.10)))
        .on_click(cx.listener(move |page, _, _, cx| click(page, cx)))
        .child(label)
}

fn feature_badge(theme: &Theme, feature: ModelFeature) -> gpui::Div {
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(crate::theme::wash(0.08))
        .text_size(px(10.0))
        .text_color(theme.text_muted)
        .child(SharedString::from(feature.label()))
}

fn filter_rail_divider() -> gpui::Div {
    div()
        .h(px(1.0))
        .mx(px(-4.0))
        .my(px(1.0))
        .bg(crate::theme::hairline(0.08))
}

fn filter_rail_row(
    id: impl Into<SharedString>,
    selected: bool,
    theme: &Theme,
    decorate: impl FnOnce(gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div>,
) -> gpui::Stateful<gpui::Div> {
    let mut row = div()
        .id(id.into())
        .relative()
        .h(px(36.0))
        .px(px(8.0))
        .rounded(px(8.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .cursor_pointer();
    if selected {
        row = row.bg(crate::theme::ink(0.06));
    } else {
        row = row.hover(|style| style.bg(crate::theme::ink(0.06)));
    }
    decorate(row).when(selected, |row| row.child(filter_rail_indicator(theme)))
}

fn filter_rail_indicator(theme: &Theme) -> gpui::Div {
    let tint = match theme.appearance {
        crate::theme::Appearance::Dark => crate::theme::oklch(0.702, 0.183, 293.541),
        crate::theme::Appearance::Light => crate::theme::oklch(0.541, 0.281, 293.009),
    };
    div()
        .absolute()
        .right(px(-4.0))
        .top(px(8.0))
        .w(px(3.0))
        .h(px(20.0))
        .rounded_tl(px(3.0))
        .rounded_bl(px(3.0))
        .bg(tint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn model(id: &str, name: &str, features: &[ModelFeature]) -> MediaModel {
        MediaModel {
            provider_id: "venice".into(),
            id: id.into(),
            display_name: name.into(),
            description: None,
            operation: zeron_studio::MediaOperation::TextToImage,
            output_kind: zeron_studio::MediaKind::Image,
            output_mime_types: vec!["image/png".into()],
            input_constraints: Vec::new(),
            prompt_maximum_chars: None,
            negative_prompt_maximum_chars: None,
            maximum_output_count: 1,
            controls: Vec::new(),
            pricing: None,
            features: features.to_vec(),
            video: zeron_studio::VideoModelMeta::default(),
            manifest_version: "test".into(),
            fetched_at: Utc::now(),
        }
    }

    #[test]
    fn missing_features_deserialize_as_empty() {
        let json = serde_json::json!({
            "provider_id": "venice",
            "id": "legacy",
            "display_name": "Legacy",
            "operation": "text_to_image",
            "output_kind": "image",
            "output_mime_types": ["image/png"],
            "input_constraints": [],
            "maximum_output_count": 1,
            "controls": [],
            "manifest_version": "old",
            "fetched_at": "2026-01-01T00:00:00Z"
        });
        let model: MediaModel = serde_json::from_value(json).unwrap();
        assert!(model.features.is_empty());
    }

    #[test]
    fn picker_filters_and_favorites() {
        let models = [
            model(
                "flux",
                "Flux",
                &[ModelFeature::Uncensored, ModelFeature::Private],
            ),
            model("gpt", "GPT Image", &[ModelFeature::Anon]),
            model(
                "seed",
                "Seedream",
                &[ModelFeature::Uncensored, ModelFeature::Anon],
            ),
        ];
        let favorites = [ModelId::new("gpt")];

        assert_eq!(
            visible_model_indices(
                &models,
                "",
                false,
                &favorites,
                &BTreeSet::new(),
                &BTreeSet::new()
            ),
            vec![1, 0, 2]
        );
        assert_eq!(
            visible_model_indices(
                &models,
                "",
                true,
                &favorites,
                &BTreeSet::new(),
                &BTreeSet::new()
            ),
            vec![1]
        );

        let uncensored = BTreeSet::from([ModelFeature::Uncensored]);
        assert_eq!(
            visible_model_indices(
                &models,
                "",
                false,
                &favorites,
                &uncensored,
                &BTreeSet::new()
            ),
            vec![0, 2]
        );

        let uncensored_anon = BTreeSet::from([ModelFeature::Uncensored, ModelFeature::Anon]);
        assert_eq!(
            visible_model_indices(
                &models,
                "",
                false,
                &favorites,
                &uncensored_anon,
                &BTreeSet::new()
            ),
            vec![2]
        );

        assert_eq!(
            visible_model_indices(
                &models,
                "gpt",
                false,
                &favorites,
                &BTreeSet::new(),
                &BTreeSet::new()
            ),
            vec![1]
        );
        assert!(
            visible_model_indices(
                &models,
                "gpt",
                false,
                &favorites,
                &uncensored,
                &BTreeSet::new()
            )
            .is_empty()
        );
    }

    #[test]
    fn picker_filters_video_operations() {
        let mut t2v = model("t2v", "Seedance T2V", &[]);
        t2v.output_kind = zeron_studio::MediaKind::Video;
        t2v.operation = MediaOperation::TextToVideo;
        let mut i2v = model("i2v", "Seedance I2V", &[]);
        i2v.output_kind = zeron_studio::MediaKind::Video;
        i2v.operation = MediaOperation::ImageToVideo;
        let mut r2v = model("r2v", "Seedance R2V", &[]);
        r2v.output_kind = zeron_studio::MediaKind::Video;
        r2v.operation = MediaOperation::ReferenceToVideo;
        let models = [t2v, i2v, r2v];
        let favorites = [];
        let t2v_only = BTreeSet::from([MediaOperation::TextToVideo]);
        assert_eq!(
            visible_model_indices(&models, "", false, &favorites, &BTreeSet::new(), &t2v_only),
            vec![0]
        );
        let t2v_i2v = BTreeSet::from([MediaOperation::TextToVideo, MediaOperation::ImageToVideo]);
        assert_eq!(
            visible_model_indices(&models, "", false, &favorites, &BTreeSet::new(), &t2v_i2v),
            vec![0, 1]
        );
    }

    fn video_model(
        id: &str,
        operation: zeron_studio::MediaOperation,
        audio: AudioCapability,
        controls: Vec<ModelControl>,
    ) -> MediaModel {
        MediaModel {
            provider_id: "venice".into(),
            id: id.into(),
            display_name: id.into(),
            description: None,
            operation,
            output_kind: MediaKind::Video,
            output_mime_types: vec!["video/mp4".into()],
            input_constraints: Vec::new(),
            prompt_maximum_chars: None,
            negative_prompt_maximum_chars: None,
            maximum_output_count: 1,
            controls,
            pricing: None,
            features: Vec::new(),
            video: zeron_studio::VideoModelMeta {
                adapter_family: zeron_studio::AdapterFamily::Seedance,
                generate_audio: audio,
                ..zeron_studio::VideoModelMeta::default()
            },
            manifest_version: "test".into(),
            fetched_at: Utc::now(),
        }
    }

    fn control(id: &str, kind: zeron_studio::ControlKind) -> ModelControl {
        ModelControl {
            id: zeron_studio::ControlId::new(id),
            label: id.into(),
            description: None,
            kind,
            required: false,
            default: None,
            minimum: None,
            maximum: None,
            step: None,
            choices: Vec::new(),
            visible_when: Vec::new(),
        }
    }

    #[test]
    fn video_popover_shows_only_advertised_controls() {
        let i2v = video_model(
            "seedance-i2v",
            zeron_studio::MediaOperation::ImageToVideo,
            AudioCapability::Configurable { default: true },
            vec![
                control("resolution", zeron_studio::ControlKind::Resolution),
                control("duration", zeron_studio::ControlKind::Duration),
                control("audio", zeron_studio::ControlKind::Boolean),
            ],
        );
        let ids: Vec<_> = chip_popover_controls(&i2v, None)
            .into_iter()
            .map(|control| control.id.as_str().to_owned())
            .collect();
        assert_eq!(ids, vec!["resolution", "audio"]);
        assert!(
            chip_control_readout(
                &i2v,
                &DraftRunConfig::from_model(&i2v),
                None,
                "aspect_ratio"
            )
            .is_none()
        );

        let grok = video_model(
            "grok-r2v",
            zeron_studio::MediaOperation::ReferenceToVideo,
            AudioCapability::None,
            vec![
                control("aspect_ratio", zeron_studio::ControlKind::AspectRatio),
                control("resolution", zeron_studio::ControlKind::Resolution),
                control("duration", zeron_studio::ControlKind::Duration),
                control("audio", zeron_studio::ControlKind::Boolean),
            ],
        );
        let grok_ids: Vec<_> = chip_popover_controls(&grok, None)
            .into_iter()
            .map(|control| control.id.as_str().to_owned())
            .collect();
        assert_eq!(grok_ids, vec!["aspect_ratio", "resolution"]);

        let forced = video_model(
            "forced-audio",
            zeron_studio::MediaOperation::TextToVideo,
            AudioCapability::ForcedOn,
            vec![
                control("resolution", zeron_studio::ControlKind::Resolution),
                control("audio", zeron_studio::ControlKind::Boolean),
            ],
        );
        let forced_ids: Vec<_> = chip_popover_controls(&forced, None)
            .into_iter()
            .map(|control| control.id.as_str().to_owned())
            .collect();
        assert_eq!(forced_ids, vec!["resolution"]);
    }

    #[test]
    fn duration_chip_label_is_compact() {
        assert_eq!(
            duration_chip_label(&ControlValue::DurationSeconds { value: 6.0 }),
            "6s"
        );
        assert_eq!(duration_chip_label(&ControlValue::DurationAuto), "Auto");
    }

    #[test]
    fn duration_chip_sizer_holds_three_glyphs() {
        let short = [
            ControlChoice {
                value: ControlValue::DurationSeconds { value: 4.0 },
                label: "4s".into(),
            },
            ControlChoice {
                value: ControlValue::DurationSeconds { value: 8.0 },
                label: "8s".into(),
            },
        ];
        assert_eq!(duration_chip_sizer(&short), "30s");
        let with_auto = [
            ControlChoice {
                value: ControlValue::DurationSeconds { value: 4.0 },
                label: "4s".into(),
            },
            ControlChoice {
                value: ControlValue::DurationAuto,
                label: "Auto".into(),
            },
        ];
        assert_eq!(duration_chip_sizer(&with_auto), "Auto");
    }

    #[test]
    fn duration_slider_snaps_to_nearest_choice() {
        let choices = [
            ControlChoice {
                value: ControlValue::DurationSeconds { value: 4.0 },
                label: "4s".into(),
            },
            ControlChoice {
                value: ControlValue::DurationSeconds { value: 6.0 },
                label: "6s".into(),
            },
            ControlChoice {
                value: ControlValue::DurationSeconds { value: 12.0 },
                label: "12s".into(),
            },
        ];
        let track = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(100.0), px(24.0)),
        };
        assert_eq!(
            snap_duration_from_track(0.0, track, &choices),
            Some(ControlValue::DurationSeconds { value: 4.0 })
        );
        assert_eq!(
            snap_duration_from_track(25.0, track, &choices),
            Some(ControlValue::DurationSeconds { value: 6.0 })
        );
        assert_eq!(
            snap_duration_from_track(100.0, track, &choices),
            Some(ControlValue::DurationSeconds { value: 12.0 })
        );
        assert!((duration_slider_t(Some(&choices[1].value), &choices) - 0.25).abs() < 0.001);
    }
}
