//! Conflict popup over the studio composer.

use gpui::{AnyElement, Context, SharedString, div, prelude::*, px};
use zeron_studio::{ComposerConflict, ConflictId, ResolveAction};

use crate::theme::Theme;

use super::page::StudioPage;

impl StudioPage {
    pub(super) fn popup_conflict_view(&self) -> Option<&ComposerConflict> {
        let id = self.popup_conflict.as_ref()?;
        self.composer_view
            .conflicts
            .iter()
            .find(|conflict| &conflict.id == id)
    }

    pub(super) fn resolve_composer_conflict(
        &mut self,
        conflict_id: ConflictId,
        action: ResolveAction,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.conflict_more_open = false;
        self.apply_composer_event(
            zeron_studio::ComposerEvent::Resolve {
                conflict_id,
                action,
            },
            Some(window),
            cx,
        );
    }

    pub(super) fn render_conflict_popup(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let conflict = self.popup_conflict_view()?.clone();
        let actions = conflict.actions.clone();
        if actions.is_empty() {
            return None;
        }
        let visible = if actions.len() <= 2 || self.conflict_more_open {
            actions.len()
        } else {
            2
        };
        let show_more = actions.len() > 2 && !self.conflict_more_open;
        let copy = conflict.title.clone();
        let mut buttons = div().flex_none().flex().items_center().gap(px(6.0));
        for (index, offered) in actions.iter().take(visible).enumerate() {
            let conflict_id = conflict.id.clone();
            let action = offered.action.clone();
            let label = offered.label.clone();
            let primary = index == 0;
            buttons = buttons.child(
                conflict_action_chip(
                    SharedString::from(format!("studio-conflict-action-{index}")),
                    label,
                    primary,
                    theme,
                )
                .on_click(cx.listener(move |page, _, window, cx| {
                    page.resolve_composer_conflict(conflict_id.clone(), action.clone(), window, cx);
                })),
            );
        }
        if show_more {
            buttons = buttons.child(
                conflict_action_chip("studio-conflict-more", "More", false, theme).on_click(
                    cx.listener(|page, _, _, cx| {
                        page.conflict_more_open = true;
                        cx.notify();
                    }),
                ),
            );
        }
        Some(
            div()
                .id("studio-conflict-popup")
                .w_full()
                .max_w(px(768.0 - 42.0))
                .h(px(48.0))
                .px(px(12.0))
                .rounded_t(px(18.0))
                .border_1()
                .border_b_0()
                .border_color(theme.border)
                .bg(theme.surface_overlay)
                .when(!theme.is_glass(), |bar| bar.shadow_md())
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child(SharedString::from(copy)),
                )
                .child(buttons)
                .into_any_element(),
        )
    }
}

fn conflict_action_chip(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    primary: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id.into())
        .h(px(26.0))
        .px(px(10.0))
        .flex_none()
        .flex()
        .items_center()
        .rounded(px(8.0))
        .bg(if primary {
            theme.text
        } else {
            crate::theme::wash(0.06)
        })
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if primary { theme.on_solid } else { theme.text })
        .cursor_pointer()
        .hover(|style| style.opacity(0.88))
        .child(label)
}
