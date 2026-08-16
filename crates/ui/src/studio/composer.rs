//! Studio composer card: model picker, per-model controls, and submit.

use gpui::{
    AnyElement, Context, Focusable as _, KeyDownEvent, Point, SharedString, Window, div,
    prelude::*, px,
};

use crate::composer::ComposerInput;
use crate::icons;
use crate::popover;
use crate::theme::Theme;

use super::draft::{DraftRunConfig, boolean_control_chip, control_value_label, draft_aspect};
use super::page::StudioPage;

impl StudioPage {
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
        self.model_picker_scroll.set_offset(Point::default());
        let search_focus = self.model_search.read(cx).focus_handle(cx);
        window.focus(&search_focus, cx);
        cx.notify();
    }

    pub(super) fn filtered_model_indices(&self, cx: &gpui::App) -> Vec<usize> {
        let query = self.model_search.read(cx).text();
        let labels = self
            .models
            .iter()
            .map(|model| model.display_name.as_str())
            .collect::<Vec<_>>();
        popover::filter_indices(query, &labels)
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

    pub(super) fn cycle_control(
        &mut self,
        model_id: &zeron_studio::ModelId,
        control: &zeron_studio::ModelControl,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(draft) = self.draft_runs.get_mut(model_id) else {
                return;
            };
            let current = draft.controls.get(&control.id).cloned();
            let next = if !control.choices.is_empty() {
                let index = control
                    .choices
                    .iter()
                    .position(|choice| Some(&choice.value) == current.as_ref())
                    .map(|index| (index + 1) % control.choices.len())
                    .unwrap_or(0);
                Some(control.choices[index].value.clone())
            } else if control.kind == zeron_studio::ControlKind::Boolean {
                let value = match current {
                    Some(zeron_studio::ControlValue::Boolean { value }) => !value,
                    _ => !matches!(
                        control.default,
                        Some(zeron_studio::ControlValue::Boolean { value: true })
                    ),
                };
                Some(zeron_studio::ControlValue::Boolean { value })
            } else if let Some(zeron_studio::ControlValue::Integer { value }) = current {
                let step = control.step.unwrap_or(1.0).max(1.0) as i64;
                let minimum = control.minimum.unwrap_or(value as f64) as i64;
                let maximum = control.maximum.unwrap_or((value + step) as f64) as i64;
                Some(zeron_studio::ControlValue::Integer {
                    value: if value + step > maximum {
                        minimum
                    } else {
                        value + step
                    },
                })
            } else if let Some(zeron_studio::ControlValue::Number { value }) = current {
                let step = control.step.unwrap_or(1.0);
                let minimum = control.minimum.unwrap_or(value);
                let maximum = control.maximum.unwrap_or(value + step);
                Some(zeron_studio::ControlValue::Number {
                    value: if value + step > maximum {
                        minimum
                    } else {
                        value + step
                    },
                })
            } else {
                None
            };
            if let Some(next) = next {
                draft.controls.insert(control.id.clone(), next);
            } else {
                return;
            }
        }
        self.persist_composer_defaults(cx);
        cx.notify();
    }

    pub(super) fn render_composer(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let model_configs = self
            .models
            .clone()
            .into_iter()
            .filter(|model| self.selected_models.contains(&model.id))
            .map(|model| {
                let remove_id = model.id.clone();
                let output_count = self
                    .draft_runs
                    .get(&model.id)
                    .map(|draft| draft.output_count)
                    .unwrap_or(1);
                let maximum_output_count = model.maximum_output_count.max(1);
                let decrement_id = model.id.clone();
                let increment_id = model.id.clone();
                let aspect_control = model
                    .controls
                    .iter()
                    .find(|control| control.id.as_str() == "aspect_ratio")
                    .cloned();
                let resolution_control = model
                    .controls
                    .iter()
                    .find(|control| control.id.as_str() == "resolution")
                    .cloned();
                let reasoning_control = model
                    .controls
                    .iter()
                    .find(|control| control.id.as_str() == "reasoning")
                    .cloned();
                let draft = self
                    .draft_runs
                    .get(&model.id)
                    .cloned()
                    .unwrap_or_else(|| DraftRunConfig::from_model(&model));
                let aspect = draft_aspect(&model, &draft);
                let aspect_label = aspect_control
                    .as_ref()
                    .and_then(|control| draft.controls.get(&control.id))
                    .map(control_value_label)
                    .unwrap_or_else(|| format!("{}:{}", aspect.0, aspect.1));
                let aspect_ratio = aspect.0 as f32 / aspect.1.max(1) as f32;
                let (indicator_w, indicator_h) = if aspect_ratio >= 1.0 {
                    (18.0, (18.0 / aspect_ratio).clamp(7.0_f32, 18.0))
                } else {
                    ((18.0 * aspect_ratio).clamp(7.0_f32, 18.0), 18.0)
                };
                let resolution_label = resolution_control
                    .as_ref()
                    .and_then(|control| draft.controls.get(&control.id))
                    .map(control_value_label)
                    .unwrap_or_else(|| "Auto".into());
                let reasoning_on = reasoning_control
                    .as_ref()
                    .and_then(|control| draft.controls.get(&control.id))
                    .is_some_and(|value| {
                        matches!(value, zeron_studio::ControlValue::Boolean { value: true })
                    });
                let aspect_model_id = model.id.clone();
                let resolution_model_id = model.id.clone();
                let reasoning_model_id = model.id.clone();
                div()
                    .id(SharedString::from(format!(
                        "studio-model-config-{}",
                        model.id.as_str()
                    )))
                    .w(px(292.0))
                    .flex_none()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::wash(0.025))
                    .px(px(10.0))
                    .py(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(SharedString::from(model.display_name.clone())),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "studio-remove-model-{}",
                                        remove_id.as_str()
                                    )))
                                    .size(px(18.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(crate::theme::wash(0.10)))
                                    .on_click(cx.listener(move |page, _, _, cx| {
                                        page.selected_models.remove(&remove_id);
                                        page.persist_composer_defaults(cx);
                                        cx.notify();
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::CLOSE)
                                            .size(px(11.0))
                                            .text_color(theme.text_muted),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .rounded(px(7.0))
                                    .bg(crate::theme::wash(0.07))
                                    .text_size(px(10.5))
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "studio-output-minus-{}",
                                                decrement_id.as_str()
                                            )))
                                            .cursor_pointer()
                                            .px(px(6.0))
                                            .py(px(5.0))
                                            .on_click(cx.listener(move |page, _, _, cx| {
                                                page.adjust_output_count(
                                                    &decrement_id,
                                                    -1,
                                                    maximum_output_count,
                                                    cx,
                                                );
                                            }))
                                            .child("−"),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(16.0))
                                            .text_center()
                                            .text_color(theme.text_muted)
                                            .child(SharedString::from(output_count.to_string())),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "studio-output-plus-{}",
                                                increment_id.as_str()
                                            )))
                                            .cursor_pointer()
                                            .px(px(6.0))
                                            .py(px(5.0))
                                            .on_click(cx.listener(move |page, _, _, cx| {
                                                page.adjust_output_count(
                                                    &increment_id,
                                                    1,
                                                    maximum_output_count,
                                                    cx,
                                                );
                                            }))
                                            .child("+"),
                                    ),
                            )
                            .when_some(aspect_control, |row, control| {
                                row.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "studio-aspect-{}",
                                            aspect_model_id.as_str()
                                        )))
                                        .h(px(27.0))
                                        .px(px(7.0))
                                        .flex()
                                        .items_center()
                                        .gap(px(5.0))
                                        .rounded(px(7.0))
                                        .bg(crate::theme::wash(0.07))
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            page.cycle_control(&aspect_model_id, &control, cx)
                                        }))
                                        .child(
                                            div()
                                                .w(px(indicator_w))
                                                .h(px(indicator_h))
                                                .rounded(px(2.0))
                                                .border_1()
                                                .border_color(theme.text_muted.opacity(0.75)),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(10.5))
                                                .text_color(theme.text_muted)
                                                .child(SharedString::from(aspect_label)),
                                        ),
                                )
                            })
                            .when_some(resolution_control, |row, control| {
                                row.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "studio-resolution-{}",
                                            resolution_model_id.as_str()
                                        )))
                                        .h(px(27.0))
                                        .px(px(7.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(7.0))
                                        .bg(crate::theme::wash(0.07))
                                        .cursor_pointer()
                                        .text_size(px(10.5))
                                        .text_color(theme.text_muted)
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            page.cycle_control(&resolution_model_id, &control, cx)
                                        }))
                                        .child(SharedString::from(resolution_label)),
                                )
                            })
                            .when_some(reasoning_control, |row, control| {
                                row.child(
                                    boolean_control_chip(
                                        format!("studio-reasoning-{}", reasoning_model_id.as_str()),
                                        "Reasoning",
                                        reasoning_on,
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        move |page, _, _, cx| {
                                            page.cycle_control(&reasoning_model_id, &control, cx)
                                        },
                                    )),
                                )
                            }),
                    )
            })
            .collect::<Vec<_>>();

        let visible_model_indices = self.filtered_model_indices(cx);
        let picker_rows = visible_model_indices
            .iter()
            .map(|model_index| self.models[*model_index].clone())
            .enumerate()
            .map(|(visible_index, model)| {
                let selected = self.selected_models.contains(&model.id);
                let active = self.model_picker_active == Some(visible_index);
                let id = model.id.clone();
                let mut row = div()
                    .id(SharedString::from(format!("studio-model-{}", id.as_str())))
                    .h(px(40.0))
                    .flex_none()
                    .px(px(8.0))
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
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(SharedString::from(model.display_name)),
                    )
                    .when(selected, |row| {
                        row.child(
                            crate::icons::icon(crate::icons::CHECK)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        )
                    });
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
            let card = popover::popover_card_flush(theme)
                .id("studio-model-picker")
                .w(px(320.0))
                .track_focus(&self.model_picker_focus)
                .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_model_picker(cx)))
                .on_key_down(cx.listener(|page, event: &KeyDownEvent, window, cx| {
                    page.on_model_picker_key_down(event, window, cx)
                }))
                .child(search_row)
                .child(
                    div()
                        .id("studio-model-list")
                        .max_h(px(300.0))
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
                                    .child("No models found"),
                            )
                        })
                        .children(picker_rows),
                )
                .into_any_element();
            popover::anchored_menu_above(
                "studio-model-menu",
                card,
                self.model_picker.closing_since(),
            )
        });

        let blocked = self.busy
            || self.selected_models.is_empty()
            || self.prompt.read(cx).text().trim().is_empty();
        let composer = div()
            .w_full()
            .max_w(px(920.0))
            .occlude()
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.input_glass_bg())
            .when(!theme.is_glass(), |composer| composer.shadow_lg())
            .px(px(12.0))
            .pt(px(10.0))
            .pb(px(10.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .id("studio-model-configs")
                    .flex()
                    .flex_row()
                    .gap(px(7.0))
                    .overflow_x_scroll()
                    .children(model_configs),
            )
            .child(
                div()
                    .min_h(px(54.0))
                    .px(px(4.0))
                    .py(px(4.0))
                    .child(self.prompt.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
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
                                        crate::icons::icon(crate::icons::WIDGET)
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
                    .when(self.source_turn.is_some(), |row| {
                        row.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child("Using previous settings"),
                        )
                    })
                    .child(div().flex_1())
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

        div()
            .absolute()
            .left(px(24.0))
            .right(px(24.0))
            .bottom(px(18.0))
            .flex()
            .justify_center()
            .child(crate::frost::frosted(26.0, 16.0, composer))
            .into_any_element()
    }
}
