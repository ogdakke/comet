//! Studio composer card: model picker, per-model controls, and submit.

use std::collections::BTreeSet;

use gpui::{
    AnyElement, Context, Focusable as _, KeyDownEvent, Point, SharedString, Window, div,
    prelude::*, px,
};
use zeron_studio::{MediaModel, ModelFeature, ModelId};

use crate::motion;
use crate::popover;
use crate::theme::Theme;

use super::draft::{DraftRunConfig, control_value_label, draft_aspect};
use super::page::StudioPage;

const COMPACT_ACTIONS_INSET: f32 = 205.0;

/// Catalog rows visible in the Studio model picker after favorites, feature
/// filters, and search. Starred models stay floated to the top.
fn visible_model_indices(
    models: &[MediaModel],
    query: &str,
    favorites_only: bool,
    favorites: &[ModelId],
    filters: &BTreeSet<ModelFeature>,
) -> Vec<usize> {
    let is_favorite = |id: &ModelId| favorites.iter().any(|favorite| favorite == id);
    let candidates = models
        .iter()
        .enumerate()
        .filter(|(_, model)| {
            if favorites_only && !is_favorite(&model.id) {
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
        let Some(draft) = self.draft_runs.get_mut(model_id) else {
            return;
        };
        draft.controls.insert(control_id.clone(), value);
        self.persist_composer_defaults(cx);
        cx.notify();
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
        self.model_picker_scroll.set_offset(Point::default());
        let search_focus = self.model_search.read(cx).focus_handle(cx);
        window.focus(&search_focus, cx);
        cx.notify();
    }

    pub(super) fn filtered_model_indices(&self, cx: &gpui::App) -> Vec<usize> {
        visible_model_indices(
            &self.models,
            self.model_search.read(cx).text(),
            self.model_picker_favorites,
            &self.remembered.favorites,
            &self.model_picker_filters,
        )
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

    pub(super) fn activate_model_picker_row(&mut self, cx: &mut Context<Self>) {
        if !self.model_picker.is_open() {
            return;
        }
        let visible = self.filtered_model_indices(cx);
        let Some(model_index) = visible.get(self.model_picker_active.unwrap_or(0)).copied() else {
            return;
        };
        let id = self.models[model_index].id.clone();
        if !self.selected_models.remove(&id) {
            self.selected_models.insert(id);
        }
        self.persist_composer_defaults(cx);
        cx.notify();
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
        if let Some(draft) = self.draft_runs.get_mut(model_id) {
            draft.output_count =
                (draft.output_count as i32 + delta).clamp(1, maximum as i32) as u32;
        } else {
            return;
        }
        self.persist_composer_defaults(cx);
        cx.notify();
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
        let current = draft.controls.get(&control.id).or(control.default.as_ref());
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

        if let Some(aspect) = model
            .controls
            .iter()
            .find(|control| control.id.as_str() == "aspect_ratio")
        {
            controls =
                controls.child(self.render_model_control(&model_id, aspect, draft, theme, cx));
        }

        let mut resolution_reasoning = div().w_full().flex().items_start().gap(px(12.0));
        let mut has_resolution_reasoning = false;
        for id in ["resolution", "reasoning"] {
            if let Some(control) = model
                .controls
                .iter()
                .find(|control| control.id.as_str() == id)
            {
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

        if let Some(format) = model
            .controls
            .iter()
            .find(|control| control.id.as_str() == "format")
        {
            controls =
                controls.child(self.render_model_control(&model_id, format, draft, theme, cx));
        }

        for control in &model.controls {
            // `steps` is submitted at the catalog default and is not a user knob.
            if matches!(
                control.id.as_str(),
                "aspect_ratio" | "resolution" | "reasoning" | "steps" | "format" | "safe_mode"
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
        let aspect = draft_aspect(&model, &draft);
        let aspect_label = model
            .controls
            .iter()
            .find(|control| control.id.as_str() == "aspect_ratio")
            .and_then(|control| draft.controls.get(&control.id).or(control.default.as_ref()))
            .map(control_value_label)
            .unwrap_or_else(|| format!("{}:{}", aspect.0, aspect.1));
        let resolution_label = model
            .controls
            .iter()
            .find(|control| control.id.as_str() == "resolution")
            .and_then(|control| draft.controls.get(&control.id).or(control.default.as_ref()))
            .map(control_value_label)
            .unwrap_or_else(|| "Auto".into());
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
                    .child(SharedString::from(model.display_name)),
            )
            .child(config_readout(
                SharedString::from(format!("{amount}×")),
                theme,
            ))
            .child(config_readout(SharedString::from(aspect_label), theme))
            .child(config_readout(SharedString::from(resolution_label), theme))
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
                        page.selected_models.remove(&remove_id);
                        page.close_model_config_menu(cx);
                        page.persist_composer_defaults(cx);
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
        let has_model_configs = !model_configs.is_empty();

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
                        if !page.selected_models.remove(&id) {
                            page.selected_models.insert(id.clone());
                        }
                        page.persist_composer_defaults(cx);
                        cx.notify();
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
                .w(px(112.0))
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
            rail = rail.child(
                div()
                    .h(px(1.0))
                    .mx(px(-4.0))
                    .my(px(1.0))
                    .bg(crate::theme::hairline(0.08)),
            );
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
                .w(px(428.0))
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
            || self.selected_models.is_empty()
            || self.prompt.read(cx).text().trim().is_empty();
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
        self.prompt
            .update(cx, |input, _| input.set_soft_wrap(prompt_expanded));

        let prompt_height = (content_height + 12.0).clamp(32.0, 220.0);
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
                    .overflow_hidden()
                    .when(!prompt_expanded, |input| {
                        input.pr(px(COMPACT_ACTIONS_INSET))
                    })
                    .child(self.prompt.clone()),
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
            .px(px(8.0))
            .pt(px(8.0))
            .pb(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .when(has_model_configs, |composer| {
                composer.child(
                    div()
                        .id("studio-model-configs")
                        .flex()
                        .flex_row()
                        .gap(px(7.0))
                        .overflow_x_scroll()
                        .children(model_configs),
                )
            })
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
            .children(self.render_generate_more_pill(theme, cx))
            .child(crate::frost::frosted(26.0, 16.0, composer))
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
            visible_model_indices(&models, "", false, &favorites, &BTreeSet::new()),
            vec![1, 0, 2]
        );
        assert_eq!(
            visible_model_indices(&models, "", true, &favorites, &BTreeSet::new()),
            vec![1]
        );

        let uncensored = BTreeSet::from([ModelFeature::Uncensored]);
        assert_eq!(
            visible_model_indices(&models, "", false, &favorites, &uncensored),
            vec![0, 2]
        );

        let uncensored_anon = BTreeSet::from([ModelFeature::Uncensored, ModelFeature::Anon]);
        assert_eq!(
            visible_model_indices(&models, "", false, &favorites, &uncensored_anon),
            vec![2]
        );

        assert_eq!(
            visible_model_indices(&models, "gpt", false, &favorites, &BTreeSet::new()),
            vec![1]
        );
        assert!(visible_model_indices(&models, "gpt", false, &favorites, &uncensored).is_empty());
    }
}
