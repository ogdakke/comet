//! Conflict tray over the studio composer — built on the shared
//! [`crate::tray`] component (also the agent-chat queued-prompt tray).

use gpui::{AnyElement, Context, SharedString};
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
        let mut tray_actions = Vec::new();
        for (index, offered) in actions.iter().take(visible).enumerate() {
            let conflict_id = conflict.id.clone();
            let action = offered.action.clone();
            let label = offered.label.clone();
            let primary = index == 0;
            tray_actions.push(if primary {
                crate::tray::TrayAction::primary(
                    SharedString::from(format!("studio-conflict-action-{index}")),
                    label,
                    cx.listener(move |page, _, window, cx| {
                        page.resolve_composer_conflict(
                            conflict_id.clone(),
                            action.clone(),
                            window,
                            cx,
                        );
                    }),
                )
            } else {
                crate::tray::TrayAction::label(
                    SharedString::from(format!("studio-conflict-action-{index}")),
                    label,
                    cx.listener(move |page, _, window, cx| {
                        page.resolve_composer_conflict(
                            conflict_id.clone(),
                            action.clone(),
                            window,
                            cx,
                        );
                    }),
                )
            });
        }
        if show_more {
            tray_actions.push(crate::tray::TrayAction::label(
                "studio-conflict-more",
                "More",
                cx.listener(|page, _, _, cx| {
                    page.conflict_more_open = true;
                    cx.notify();
                }),
            ));
        }
        crate::tray::render_tray(
            "studio-conflict-tray",
            self.tray_scroll.clone(),
            vec![crate::tray::TrayItem {
                id: "studio-conflict-row".into(),
                label: SharedString::from(copy),
                actions: tray_actions,
            }],
            theme,
        )
    }
}
