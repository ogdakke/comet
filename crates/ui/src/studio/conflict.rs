//! Conflict popup over the studio composer.

use gpui::{AnyElement, Context, SharedString, div, prelude::*, px};
use zeron_studio::{ComposerConflict, ConflictId, ResolveAction};

use crate::popover;
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
        let mut buttons = div()
            .mt(px(16.0))
            .flex()
            .flex_wrap()
            .justify_end()
            .gap(px(8.0));
        for (index, offered) in actions.iter().take(visible).enumerate() {
            let conflict_id = conflict.id.clone();
            let action = offered.action.clone();
            let label = offered.label.clone();
            let primary = index == 0;
            let button = if primary {
                popover::btn_primary(theme, &label)
            } else {
                popover::btn_ghost(
                    theme,
                    &label,
                    SharedString::from(format!("studio-conflict-action-{index}")),
                )
            };
            buttons = buttons.child(
                button
                    .id(SharedString::from(format!(
                        "studio-conflict-action-{}",
                        index
                    )))
                    .on_click(cx.listener(move |page, _, window, cx| {
                        page.resolve_composer_conflict(
                            conflict_id.clone(),
                            action.clone(),
                            window,
                            cx,
                        );
                    })),
            );
        }
        if show_more {
            buttons = buttons.child(
                popover::btn_ghost(theme, "More", "studio-conflict-more")
                    .id("studio-conflict-more")
                    .on_click(cx.listener(|page, _, _, cx| {
                        page.conflict_more_open = true;
                        cx.notify();
                    })),
            );
        }
        Some(
            div()
                .id("studio-conflict-popup")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .px(px(16.0))
                .bg(crate::theme::ink(0.45))
                // Blocking conflicts stay until a typed action or a compensating edit.
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    popover::dialog_card(theme)
                        .id("studio-conflict-card")
                        .child(popover::dialog_title(theme, &conflict.title))
                        .child(
                            popover::dialog_body(theme, conflict.explanation.clone()).mt(px(8.0)),
                        )
                        .child(buttons),
                )
                .into_any_element(),
        )
    }
}
