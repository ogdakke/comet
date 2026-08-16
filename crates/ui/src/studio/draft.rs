//! Per-model draft settings and control chrome.

use std::collections::BTreeMap;

use gpui::{SharedString, div, prelude::*, px};

use crate::theme::Theme;

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

pub(super) fn boolean_control_chip(
    id: impl Into<SharedString>,
    label: &'static str,
    on: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .h(px(27.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .rounded(px(7.0))
        .bg(crate::theme::wash(if on { 0.12 } else { 0.07 }))
        .cursor_pointer()
        .child(
            div()
                .w(px(18.0))
                .h(px(10.0))
                .rounded_full()
                .bg(if on {
                    theme.text_muted
                } else {
                    theme.border_strong
                })
                .p(px(2.0))
                .child(
                    div()
                        .size(px(6.0))
                        .rounded_full()
                        .bg(theme.surface)
                        .when(on, |dot| dot.ml(px(8.0))),
                ),
        )
        .child(
            div()
                .text_size(px(10.5))
                .text_color(theme.text_muted)
                .child(label),
        )
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
