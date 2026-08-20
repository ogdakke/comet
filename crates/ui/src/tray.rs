//! Generic composer tray — a compact stack of actionable one-line rows that
//! sits above a composer. One component, two surfaces:
//!
//! - the agent-chat composer's QUEUED PROMPT tray (a row per prompt with
//!   Edit / Steer / Delete ghost buttons; scrolling past three rows with an
//!   edge fade);
//! - the studio composer's CONFLICT tray (a row per conflict with its resolve
//!   action chips).
//!
//! Rows are one line, truncated with an ellipsis; the scrollbox shows three
//! rows before scrolling (edge-faded top/bottom when overflow exists).

use gpui::{
    AnyElement, ClickEvent, ScrollHandle, ScrollWheelEvent, SharedString, div, prelude::*, px,
};

use crate::theme::Theme;

/// Visible rows before the tray scrolls.
pub const TRAY_MAX_ROWS: usize = 3;
const ROW_H: f32 = 30.0;
/// The sliver of the next row that peeks above the fold when the tray
/// overflows — the "there's more" affordance. The bottom fade stays within
/// this sliver, so a fully-visible row is never dimmed (the old 16px band
/// over exactly-fitted rows ate the last row's text — user report).
const PEEK_H: f32 = 14.0;
/// Keep part of the preview sliver readable instead of fading all 14px to
/// transparent; it still signals more queued rows without hiding their copy.
const BOTTOM_BAND: f32 = 8.0;
/// Top-edge fade when scrolled down (only ever covers the half-scrolled
/// row at the top, never a full one).
const TOP_BAND: f32 = 10.0;
/// Horizontal inset from the composer's edges — the tray is narrower than
/// the composer it sits on (the studio conflict popup's `max-w-3xl − 42`).
const TRAY_INSET: f32 = 21.0;

/// One action button on a tray row. Ghost by default (`label` or `icon`);
/// `primary` renders the filled chip style (a conflict's first resolve
/// action).
pub struct TrayAction {
    pub id: SharedString,
    pub label: Option<SharedString>,
    pub icon: Option<&'static str>,
    pub primary: bool,
    pub on_click: Box<dyn Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static>,
}

impl TrayAction {
    /// A ghost text button ("Edit", "Steer").
    pub fn label(
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        on_click: impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: Some(label.into()),
            icon: None,
            primary: false,
            on_click: Box::new(on_click),
        }
    }

    /// A ghost icon-only button (delete's trash).
    pub fn icon(
        id: impl Into<SharedString>,
        icon: &'static str,
        on_click: impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: None,
            icon: Some(icon),
            primary: false,
            on_click: Box::new(on_click),
        }
    }

    /// The filled primary chip (conflict resolve).
    pub fn primary(
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        on_click: impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    ) -> Self {
        Self {
            primary: true,
            ..Self::label(id, label, on_click)
        }
    }
}

/// One tray row: a one-line truncated label plus trailing action buttons.
pub struct TrayItem {
    pub id: SharedString,
    pub label: SharedString,
    pub actions: Vec<TrayAction>,
}

/// Render the tray. `None` when there is nothing in it (the tray is only
/// visible when it has content). `scroll` is the caller-owned scroll handle
/// for the row list.
pub fn render_tray(
    tray_id: impl Into<SharedString>,
    scroll: ScrollHandle,
    items: Vec<TrayItem>,
    theme: &Theme,
) -> Option<AnyElement> {
    let tray_id = tray_id.into();
    if items.is_empty() {
        return None;
    }
    let visible_rows = items.len().min(TRAY_MAX_ROWS);
    // Overflowing trays show TRAY_MAX_ROWS full rows + a PEEK sliver of the
    // next; an exact fit gets no sliver (and no fade — `fade_overflow_y`
    // gates on real overflow at paint time).
    let max_h = visible_rows as f32 * ROW_H
        + if items.len() > TRAY_MAX_ROWS {
            PEEK_H
        } else {
            0.0
        };
    let mut list = div()
        .id(SharedString::from(format!("{tray_id}-list")))
        .w_full()
        .flex()
        .flex_col()
        .max_h(px(max_h))
        .overflow_y_scroll()
        .track_scroll(&scroll);
    for item in items {
        list = list.child(render_row(item, theme));
    }
    // Keep the tray's *surface* 42px narrower than its composer column.
    // Margins on a `w_full` surface only move it outward in GPUI's flex
    // layout; this wrapper reserves the two 21px insets instead, so the
    // surface is both centered and exactly 42px narrower.
    Some(
        div()
            .w_full()
            .px(px(TRAY_INSET))
            .child(
                div()
                    .id(tray_id)
                    .w_full()
                    // This floats over the transcript. It must own the hitbox
                    // and consume wheel events even when the row list has no
                    // overflow, otherwise the transcript scrolls underneath.
                    .occlude()
                    .on_scroll_wheel(|_: &ScrollWheelEvent, _, cx| cx.stop_propagation())
                    .px(px(10.0))
                    .py(px(4.0))
                    // Hugs the composer's top edge: rounded top corners, square
                    // bottom corners, no bottom border — the pill's own top border
                    // is the seam (the studio conflict popup's shape).
                    .rounded_t(px(18.0))
                    .border_1()
                    .border_b_0()
                    .border_color(theme.border)
                    .bg(theme.surface_raised)
                    .when(!theme.is_glass(), |el| el.shadow_md())
                    .child(
                        crate::edge_fade::edge_faded(PEEK_H, true, true, list)
                            .band_top(TOP_BAND)
                            .band_bottom(BOTTOM_BAND)
                            .fade_overflow_y(&scroll),
                    ),
            )
            .into_any_element(),
    )
}

fn render_row(item: TrayItem, theme: &Theme) -> impl IntoElement {
    let mut actions = div().flex_none().flex().items_center().gap(px(4.0));
    for action in item.actions {
        actions = actions.child(render_action(action, theme));
    }
    div()
        .id(item.id)
        // flex_none: taffy's default shrink would squish every row to fit
        // `max_h` instead of letting the list scroll (30px rows collapsed to
        // 22.5px with four queued prompts — user report).
        .flex_none()
        .h(px(ROW_H))
        .flex()
        .items_center()
        .gap(px(8.0))
        .min_w_0()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(12.0))
                .text_color(theme.text)
                .child(item.label),
        )
        .child(actions)
}

fn render_action(action: TrayAction, theme: &Theme) -> impl IntoElement {
    let TrayAction {
        id,
        label,
        icon,
        primary,
        on_click,
    } = action;
    let label_color = if primary {
        theme.on_solid
    } else {
        theme.text_muted
    };
    div()
        .id(id)
        .h(px(22.0))
        .when_some(label, |el, label| el.px(px(8.0)).child(label))
        .when_some(icon, |el, icon| {
            el.w(px(22.0)).flex().items_center().justify_center().child(
                crate::icons::icon(icon)
                    .size(px(13.0))
                    .text_color(label_color),
            )
        })
        .flex_none()
        .flex()
        .items_center()
        .rounded(px(6.0))
        .bg(if primary {
            theme.text
        } else {
            gpui::transparent_black()
        })
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(label_color)
        .cursor_pointer()
        .hover(|style| {
            if primary {
                style.opacity(0.88)
            } else {
                style.bg(crate::theme::wash(0.08))
            }
        })
        .on_click(move |event, window, cx| on_click(event, window, cx))
}
