//! Right-click / two-finger-tap menu shared by every visible Studio image.
//!
//! Filmstrip thumbs are navigation, not an image surface — they stay unbound.

use gpui::{
    AnyElement, Context, InteractiveElement, MouseButton, MouseDownEvent, Pixels, Point,
    SharedString, prelude::*, px,
};
use zeron_studio::{StudioArtifactId, StudioConversationId};

use crate::icons;
use crate::popover;
use crate::theme::Theme;

use super::StudioEvent;
use super::page::StudioPage;

/// Where a generated image is shown. Drives whether the menu exists and
/// which extra items it offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ImageSurface {
    GalleryTile,
    GalleryArtifact,
    ThreadTile,
    ThreadArtifact,
    Filmstrip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ImageMenuAction {
    OpenThread,
    Download,
    Delete,
}

#[derive(Clone, Debug)]
pub(super) struct ImageMenu {
    artifact_id: StudioArtifactId,
    conversation_id: StudioConversationId,
    position: Point<Pixels>,
    open_thread: bool,
}

pub(super) fn image_menu_enabled(surface: ImageSurface) -> bool {
    !matches!(surface, ImageSurface::Filmstrip)
}

pub(super) fn image_menu_open_thread(surface: ImageSurface) -> bool {
    matches!(
        surface,
        ImageSurface::GalleryTile | ImageSurface::GalleryArtifact
    )
}

pub(super) fn image_menu_actions(open_thread: bool) -> Vec<ImageMenuAction> {
    let mut actions = Vec::with_capacity(3);
    if open_thread {
        actions.push(ImageMenuAction::OpenThread);
    }
    actions.push(ImageMenuAction::Download);
    actions.push(ImageMenuAction::Delete);
    actions
}

impl StudioPage {
    /// Bind the shared image menu to any visible image element.
    pub(super) fn bind_image_menu<E>(
        &self,
        element: E,
        artifact_id: StudioArtifactId,
        conversation_id: StudioConversationId,
        surface: ImageSurface,
        cx: &mut Context<Self>,
    ) -> E
    where
        E: InteractiveElement,
    {
        if !image_menu_enabled(surface) {
            return element;
        }
        let open_thread = image_menu_open_thread(surface);
        element.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |page, event: &MouseDownEvent, window, cx| {
                window.prevent_default();
                page.open_image_menu(
                    artifact_id,
                    conversation_id,
                    open_thread,
                    event.position,
                    cx,
                );
                cx.stop_propagation();
            }),
        )
    }

    pub(super) fn open_image_menu(
        &mut self,
        artifact_id: StudioArtifactId,
        conversation_id: StudioConversationId,
        open_thread: bool,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.close_upscale_settings_menu(cx);
        self.close_artifact_actions_menu(cx);
        self.image_menu.open(ImageMenu {
            artifact_id,
            conversation_id,
            position,
            open_thread,
        });
        cx.notify();
    }

    pub(super) fn close_image_menu(&mut self, cx: &mut Context<Self>) {
        if self.image_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.image_menu);
            cx.notify();
        }
    }

    pub(super) fn dismiss_image_menu(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.keystroke.key == "escape" && self.image_menu.is_open() {
            self.close_image_menu(cx);
            true
        } else {
            false
        }
    }

    pub(super) fn artifact_menu_conversation(
        &self,
        artifact_id: StudioArtifactId,
    ) -> Option<StudioConversationId> {
        self.artifact_conversation(artifact_id)
            .or_else(|| {
                self.gallery_item(artifact_id)
                    .map(|item| item.conversation_id)
            })
            .or(self.selected_conversation)
            .or_else(|| self.conversation.as_ref().map(|view| view.conversation.id))
    }

    pub(super) fn artifact_image_surface(&self) -> ImageSurface {
        if self.is_gallery() {
            ImageSurface::GalleryArtifact
        } else {
            ImageSurface::ThreadArtifact
        }
    }

    fn download_image_menu(&mut self, artifact_id: StudioArtifactId, cx: &mut Context<Self>) {
        self.close_image_menu(cx);
        self.download_artifact(artifact_id, cx);
    }

    fn delete_image_menu(&mut self, artifact_id: StudioArtifactId, cx: &mut Context<Self>) {
        self.close_image_menu(cx);
        self.delete_artifact(artifact_id, cx);
    }

    fn open_image_thread(
        &mut self,
        conversation_id: StudioConversationId,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        self.close_image_menu(cx);
        cx.emit(StudioEvent::ShowThread {
            conversation_id,
            focus_artifact: Some(artifact_id),
        });
    }

    pub(super) fn render_image_menu(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.image_menu.get()?.clone();
        let closing = self.image_menu.closing_since();
        let artifact_id = menu.artifact_id;
        let conversation_id = menu.conversation_id;
        let actions = image_menu_actions(menu.open_thread);
        let mut card = popover::popover_card(theme)
            .w(px(188.0))
            .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                page.close_image_menu(cx);
            }))
            .flex()
            .flex_col();
        for (index, action) in actions.iter().copied().enumerate() {
            if action == ImageMenuAction::Delete && index > 0 {
                card = card.child(popover::menu_separator());
            }
            card = card.child(self.render_image_menu_row(
                action,
                artifact_id,
                conversation_id,
                theme,
                cx,
            ));
        }
        Some(popover::menu_at(
            "studio-image-context-menu",
            menu.position,
            card.into_any_element(),
            closing,
        ))
    }

    fn render_image_menu_row(
        &self,
        action: ImageMenuAction,
        artifact_id: StudioArtifactId,
        conversation_id: StudioConversationId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (id, fade, label, icon, danger) = match action {
            ImageMenuAction::OpenThread => (
                "studio-image-menu-open-thread",
                format!("studio-image-menu-open-thread-{}", artifact_id.0),
                "Open in thread",
                icons::CHAT_ROUND_LINE,
                false,
            ),
            ImageMenuAction::Download => (
                "studio-image-menu-download",
                format!("studio-image-menu-download-{}", artifact_id.0),
                "Download",
                icons::ARROW_DOWN,
                false,
            ),
            ImageMenuAction::Delete => (
                "studio-image-menu-delete",
                format!("studio-image-menu-delete-{}", artifact_id.0),
                "Delete",
                icons::TRASH_BIN_MINIMALISTIC,
                true,
            ),
        };
        let color = if danger {
            theme.danger
        } else {
            theme.text_muted
        };
        popover::menu_row(theme, false, fade)
            .id(SharedString::from(id))
            .when(danger, |row| row.text_color(theme.danger))
            .on_click(cx.listener(move |page, _, _, cx| match action {
                ImageMenuAction::OpenThread => {
                    page.open_image_thread(conversation_id, artifact_id, cx)
                }
                ImageMenuAction::Download => page.download_image_menu(artifact_id, cx),
                ImageMenuAction::Delete => page.delete_image_menu(artifact_id, cx),
            }))
            .child(icons::icon(icon).size(px(16.0)).text_color(color))
            .child(SharedString::from(label))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_image_surfaces_have_a_menu() {
        assert!(image_menu_enabled(ImageSurface::GalleryTile));
        assert!(image_menu_enabled(ImageSurface::GalleryArtifact));
        assert!(image_menu_enabled(ImageSurface::ThreadTile));
        assert!(image_menu_enabled(ImageSurface::ThreadArtifact));
    }

    #[test]
    fn filmstrip_thumbs_do_not_have_a_menu() {
        assert!(!image_menu_enabled(ImageSurface::Filmstrip));
        assert!(!image_menu_open_thread(ImageSurface::Filmstrip));
    }

    #[test]
    fn gallery_surfaces_offer_open_thread() {
        assert!(image_menu_open_thread(ImageSurface::GalleryTile));
        assert!(image_menu_open_thread(ImageSurface::GalleryArtifact));
        assert_eq!(
            image_menu_actions(true),
            vec![
                ImageMenuAction::OpenThread,
                ImageMenuAction::Download,
                ImageMenuAction::Delete,
            ]
        );
    }

    #[test]
    fn thread_surfaces_keep_download_and_delete() {
        assert!(!image_menu_open_thread(ImageSurface::ThreadTile));
        assert!(!image_menu_open_thread(ImageSurface::ThreadArtifact));
        assert_eq!(
            image_menu_actions(false),
            vec![ImageMenuAction::Download, ImageMenuAction::Delete]
        );
    }
}
