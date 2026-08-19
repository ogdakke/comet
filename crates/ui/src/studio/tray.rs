//! Composer attachment tray: file pick, ImportStudioAsset, budgets, Make video.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use gpui::{
    Context, Image, ImageFormat, ObjectFit, PathPromptOptions, SharedString, StyledImage as _,
    Window, div, img, prelude::*, px,
};
use sha2::{Digest, Sha256};
use zeron_proto::ImportStudioAssetResponse;
use zeron_rpc::{RpcError, methods};
use zeron_studio::{
    AttachmentOrigin, BudgetKind, ComposerAttachment, ComposerEvent, ComposerMediaKind,
    ComposerMode, InputRole, LimitBudget, MediaKind, MediaOperation, ROLE_LAST_FRAME,
    ROLE_REFERENCE, ROLE_REFERENCE_AUDIO, ROLE_REFERENCE_VIDEO, ROLE_SOURCE, StudioArtifactId,
    StudioAssetId, StudioConversationId, TrayAccept, sniff_media_mime,
};

use crate::state::EngineHandle;
use crate::theme::Theme;

use super::page::StudioPage;

/// Local IPC can take larger slices than the chat relay; keep JSON frames modest.
const IMPORT_CHUNK_BYTES: usize = 256 * 1024;
const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;
const TRAY_THUMB: f32 = 44.0;

pub(super) struct StagedStudioFile {
    bytes: Vec<u8>,
    mime: String,
    kind: ComposerMediaKind,
    hash: String,
    preview: Option<Arc<Image>>,
}

#[derive(Clone, Debug)]
pub(super) struct ArtifactReferenceMeta {
    pub artifact_id: StudioArtifactId,
    pub mime_type: String,
    pub byte_size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub content_hash: String,
    pub kind: ComposerMediaKind,
}

impl StudioPage {
    pub(super) fn tray_add_enabled(&self) -> bool {
        self.composer_view.attachments.add_enabled
            && !self.composer_view.attachments.accept.mime_types.is_empty()
    }

    pub(super) fn open_tray_picker(&mut self, cx: &mut Context<Self>) {
        if !self.tray_add_enabled() {
            return;
        }
        let accept = self.composer_view.attachments.accept.clone();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        self.tray_picker_task = Some(cx.spawn(async move |this, cx| {
            let paths = match rx.await {
                Ok(Ok(Some(paths))) if !paths.is_empty() => paths,
                _ => return,
            };
            this.update(cx, |page, cx| page.import_tray_paths(paths, accept, cx))
                .ok();
        }));
    }

    pub(super) fn add_dropped_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if !self.tray_add_enabled() || paths.is_empty() {
            return;
        }
        let accept = self.composer_view.attachments.accept.clone();
        self.import_tray_paths(paths, accept, cx);
    }

    fn import_tray_paths(
        &mut self,
        paths: Vec<PathBuf>,
        accept: TrayAccept,
        cx: &mut Context<Self>,
    ) {
        self.tray_picker_task = Some(cx.spawn(async move |this, cx| {
            let accept = this
                .update(cx, |page, _| page.composer_view.attachments.accept.clone())
                .ok()
                .unwrap_or(accept);
            let staged = cx
                .background_executor()
                .spawn(async move { stage_studio_paths(paths, &accept) })
                .await;
            this.update(cx, |page, cx| {
                let mut first_error = None;
                for result in staged {
                    match result {
                        Ok(file) => page.begin_tray_import(file, cx),
                        Err(error) => {
                            if first_error.is_none() {
                                first_error = Some(error);
                            }
                        }
                    }
                }
                if let Some(error) = first_error {
                    page.error = Some(error.into());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn begin_tray_import(&mut self, file: StagedStudioFile, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let asset_id = StudioAssetId::new();
        if let Some(preview) = file.preview {
            self.tray_previews.insert(asset_id, preview);
        }
        self.apply_composer_event(
            ComposerEvent::Attach {
                attachment: ComposerAttachment {
                    id: asset_id,
                    kind: file.kind,
                    pending: true,
                    origin: AttachmentOrigin::Asset,
                    mime_type: file.mime.clone(),
                    byte_size: file.bytes.len() as u64,
                    width: None,
                    height: None,
                    duration_seconds: None,
                    content_hash: file.hash.clone(),
                    role_hint: None,
                },
            },
            None,
            cx,
        );
        let bytes = file.bytes;
        let hash = file.hash;
        let mime = file.mime;
        self.import_tasks.insert(
            asset_id,
            cx.spawn(async move |this, cx| {
                let result =
                    import_studio_asset(&engine, asset_id, &bytes, &hash, Some(&mime), "This file")
                        .await;
                this.update(cx, |page, cx| {
                    page.import_tasks.remove(&asset_id);
                    let still_present = page
                        .composer
                        .attachments
                        .iter()
                        .any(|attachment| attachment.id == asset_id);
                    if !still_present {
                        page.tray_previews.remove(&asset_id);
                        return;
                    }
                    match result {
                        Ok(attachment) => page.apply_composer_event(
                            ComposerEvent::Attach { attachment },
                            None,
                            cx,
                        ),
                        Err(error) => {
                            page.detach_tray_attachment(asset_id, cx);
                            page.error = Some(error.into());
                            cx.notify();
                        }
                    }
                })
                .ok();
            }),
        );
    }

    pub(super) fn detach_tray_attachment(
        &mut self,
        asset_id: StudioAssetId,
        cx: &mut Context<Self>,
    ) {
        self.import_tasks.remove(&asset_id);
        self.tray_previews.remove(&asset_id);
        self.apply_composer_event(ComposerEvent::Detach { asset_id }, None, cx);
    }

    pub(super) fn make_video_from_artifact(
        &mut self,
        artifact_id: StudioArtifactId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conversation_id = self.artifact_menu_conversation(artifact_id);
        if make_video_already_on_thread(self.selected_conversation, conversation_id) {
            self.request_close_artifact(cx);
        } else if let Some(conversation_id) = conversation_id {
            // ShowThread dismisses the lightbox without bouncing through the
            // gallery nav entry (CloseArtifact would call show_gallery).
            self.open_conversation(conversation_id, cx);
            self.composer_seeded_for = Some(conversation_id);
            cx.emit(super::StudioEvent::ShowThread {
                conversation_id,
                focus_artifact: None,
            });
        } else {
            self.request_close_artifact(cx);
        }
        if self.composer.mode != ComposerMode::Video {
            self.set_composer_mode(ComposerMode::Video, window, cx);
        }
        self.attach_artifact_reference(artifact_id, cx);
    }

    pub(super) fn attach_artifact_reference(
        &mut self,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        let Some(meta) = self.artifact_reference_meta(artifact_id) else {
            self.error = Some("This image is not available as a reference".into());
            cx.notify();
            return;
        };
        let role_hint =
            make_video_role_hint(self.composer_view.models.iter().map(|chip| chip.operation));
        if let Some(existing) = self
            .composer
            .attachments
            .iter()
            .find(|attachment| match attachment.origin {
                AttachmentOrigin::Artifact { artifact_id: id } => id == artifact_id,
                AttachmentOrigin::Asset => false,
            })
        {
            if let Some(role) = role_hint.filter(|role| existing.role_hint.as_ref() != Some(role)) {
                self.apply_composer_event(
                    ComposerEvent::PinRole {
                        asset_id: existing.id,
                        role,
                    },
                    None,
                    cx,
                );
            }
            return;
        }
        self.request_images(vec![artifact_id], false, cx);
        self.apply_composer_event(
            ComposerEvent::Attach {
                attachment: attachment_from_artifact(meta, role_hint),
            },
            None,
            cx,
        );
    }

    pub(super) fn artifact_reference_meta(
        &self,
        artifact_id: StudioArtifactId,
    ) -> Option<ArtifactReferenceMeta> {
        if let Some(artifact) = self.find_conversation_artifact(artifact_id) {
            return Some(ArtifactReferenceMeta {
                artifact_id,
                mime_type: artifact.mime_type.clone(),
                byte_size: artifact.size_bytes,
                width: artifact.width,
                height: artifact.height,
                duration_seconds: artifact.duration_seconds,
                content_hash: artifact.content_hash.clone(),
                kind: kind_from_media(artifact.media_kind, &artifact.mime_type),
            });
        }
        let item = self.gallery_item(artifact_id)?;
        Some(ArtifactReferenceMeta {
            artifact_id,
            mime_type: item.mime_type.clone(),
            byte_size: item.size_bytes,
            width: item.width,
            height: item.height,
            duration_seconds: None,
            content_hash: String::new(),
            kind: kind_from_media(item.media_kind, &item.mime_type),
        })
    }

    fn find_conversation_artifact(
        &self,
        artifact_id: StudioArtifactId,
    ) -> Option<&zeron_proto::StudioArtifactView> {
        self.conversation.as_ref().and_then(|view| {
            view.turns
                .iter()
                .flat_map(|turn| turn.runs.iter().flat_map(|run| run.artifacts.iter()))
                .find(|artifact| artifact.id == artifact_id)
        })
    }

    pub(super) fn render_attachment_tray(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let items = &self.composer_view.attachments.items;
        let add_enabled = self.tray_add_enabled();
        if items.is_empty() && !add_enabled {
            return None;
        }
        let mut row = div()
            .id("studio-attachment-tray")
            .w_full()
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0));
        let mut chips = div()
            .id("studio-attachment-chips")
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .pt(px(6.0))
            .pr(px(6.0))
            .overflow_x_scroll();
        for attachment in items {
            chips = chips.child(self.render_tray_chip(attachment, theme, cx));
        }
        if add_enabled {
            chips = chips.child(self.render_tray_add(theme, cx));
        }
        row = row.child(chips);
        if let Some(budgets) = self.render_role_budgets(theme) {
            row = row.child(budgets);
        }
        Some(row)
    }

    fn render_tray_chip(
        &self,
        attachment: &ComposerAttachment,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let asset_id = attachment.id;
        let pending = attachment.pending;
        let group: SharedString = format!("studio-att-{}", asset_id.0).into();
        let preview = self.tray_chip_image(attachment);
        let lightbox = (!pending)
            .then(|| self.tray_lightbox_preview(attachment))
            .flatten();
        let icon = match attachment.kind {
            ComposerMediaKind::Image => crate::icons::GALLERY_MINIMALISTIC,
            ComposerMediaKind::Video => crate::icons::GALLERY_WIDE,
            ComposerMediaKind::Audio => crate::icons::VOLUME_LOUD,
        };
        div()
            .id(SharedString::from(format!("studio-att-{}", asset_id.0)))
            .group(group.clone())
            .relative()
            .flex_none()
            .child(
                div()
                    .id(SharedString::from(format!(
                        "studio-att-thumb-{}",
                        asset_id.0
                    )))
                    .relative()
                    .size(px(TRAY_THUMB))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::wash(0.06))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(pending, |chip| chip.opacity(0.7))
                    .when_some(lightbox.clone(), |chip, preview| {
                        chip.cursor_pointer()
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.tray_preview = Some(preview.clone());
                                page.tray_preview_focus_pending = true;
                                cx.notify();
                            }))
                    })
                    .child(match preview {
                        Some(image) => img(image)
                            .size_full()
                            .rounded(px(7.0))
                            .object_fit(ObjectFit::Cover)
                            .into_any_element(),
                        None => crate::icons::icon(icon)
                            .size(px(16.0))
                            .text_color(theme.text_muted)
                            .into_any_element(),
                    })
                    .when(pending, |chip| {
                        chip.child(
                            div()
                                .absolute()
                                .inset_0()
                                .bg(theme.bg.opacity(0.45))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(crate::loaders::mini_gradient_spinner(
                                    format!("studio-att-spin-{}", asset_id.0),
                                    2.0,
                                    cx.entity_id(),
                                    cx,
                                )),
                        )
                    }),
            )
            .child(crate::frost::layered(
                div()
                    .id(SharedString::from(format!(
                        "studio-att-remove-{}",
                        asset_id.0
                    )))
                    .absolute()
                    .top(px(-6.0))
                    .right(px(-6.0))
                    .size(px(18.0))
                    .rounded_full()
                    .bg(theme.bg)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .shadow_sm()
                    .opacity(0.0)
                    .group_hover(group, |style| style.opacity(1.0))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        cx.stop_propagation();
                        page.detach_tray_attachment(asset_id, cx);
                    }))
                    .child(
                        crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    ),
            ))
    }

    fn render_tray_add(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id("studio-attach")
            .size(px(TRAY_THUMB))
            .flex_none()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.08)))
            .on_click(cx.listener(|page, _, _, cx| page.open_tray_picker(cx)))
            .child(
                crate::icons::icon(crate::icons::PLUS)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
    }

    fn tray_chip_image(&self, attachment: &ComposerAttachment) -> Option<Arc<Image>> {
        if let Some(preview) = self.tray_previews.get(&attachment.id) {
            return Some(preview.clone());
        }
        if let AttachmentOrigin::Artifact { artifact_id } = attachment.origin {
            return self
                .images
                .get_display(&artifact_id)
                .or_else(|| self.images.get_thumb(&artifact_id));
        }
        None
    }

    fn tray_lightbox_preview(
        &self,
        attachment: &ComposerAttachment,
    ) -> Option<crate::attachments::PreviewImage> {
        if attachment.kind != ComposerMediaKind::Image {
            return None;
        }
        let image = if let Some(preview) = self.tray_previews.get(&attachment.id) {
            preview.clone()
        } else if let AttachmentOrigin::Artifact { artifact_id } = attachment.origin {
            self.images
                .get_full(&artifact_id)
                .or_else(|| self.images.get_display(&artifact_id))
                .or_else(|| self.images.get_thumb(&artifact_id))?
        } else {
            return None;
        };
        Some(crate::attachments::PreviewImage {
            name: SharedString::from("Reference image"),
            image,
        })
    }

    pub(super) fn render_prompt_budget(&self, theme: &Theme) -> Option<gpui::Stateful<gpui::Div>> {
        let budget = self
            .composer_view
            .budgets
            .iter()
            .find(|budget| matches!(budget.kind, BudgetKind::PromptChars))?;
        let label = budget_label(budget)?;
        let overflow = budget.remaining.is_some_and(|remaining| remaining < 0);
        Some(
            div()
                .id("studio-prompt-budget")
                .flex_none()
                .text_size(px(11.0))
                .text_color(if overflow {
                    theme.danger
                } else {
                    theme.text_muted
                })
                .child(SharedString::from(label)),
        )
    }

    fn render_role_budgets(&self, theme: &Theme) -> Option<gpui::Stateful<gpui::Div>> {
        let labels = tray_budget_labels(
            &self.composer_view.budgets,
            &self.composer_view.attachments.items,
        );
        if labels.is_empty() {
            return None;
        }
        Some(
            div()
                .id("studio-role-budgets")
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.0))
                .children(labels.into_iter().map(|label| {
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(label))
                })),
        )
    }

    pub(super) fn render_make_video_action(
        &self,
        artifact_id: StudioArtifactId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("studio-make-video")
            .h(px(34.0))
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(7.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(crate::theme::wash(0.06))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.10)))
            .on_click(cx.listener(move |page, _, window, cx| {
                page.make_video_from_artifact(artifact_id, window, cx);
            }))
            .child(SharedString::from("Make video"))
    }
}

pub(super) fn make_video_already_on_thread(
    selected: Option<StudioConversationId>,
    artifact_conversation: Option<StudioConversationId>,
) -> bool {
    matches!(
        (selected, artifact_conversation),
        (Some(selected), Some(artifact)) if selected == artifact
    )
}

pub(super) fn make_video_role_hint(
    operations: impl IntoIterator<Item = MediaOperation>,
) -> Option<InputRole> {
    let mut has_i2v = false;
    let mut has_r2v = false;
    for operation in operations {
        match operation {
            MediaOperation::ImageToVideo => has_i2v = true,
            MediaOperation::ReferenceToVideo => has_r2v = true,
            _ => {}
        }
    }
    if has_i2v {
        Some(InputRole::new(ROLE_SOURCE))
    } else if has_r2v {
        Some(InputRole::new(ROLE_REFERENCE))
    } else {
        None
    }
}

pub(super) fn attachment_from_artifact(
    meta: ArtifactReferenceMeta,
    role_hint: Option<InputRole>,
) -> ComposerAttachment {
    ComposerAttachment {
        id: StudioAssetId(meta.artifact_id.0),
        kind: meta.kind,
        pending: false,
        origin: AttachmentOrigin::Artifact {
            artifact_id: meta.artifact_id,
        },
        mime_type: meta.mime_type,
        byte_size: meta.byte_size,
        width: meta.width,
        height: meta.height,
        duration_seconds: meta.duration_seconds,
        content_hash: meta.content_hash,
        role_hint,
    }
}

pub(super) fn budget_label(budget: &LimitBudget) -> Option<String> {
    let maximum = budget.maximum?;
    match &budget.kind {
        BudgetKind::PromptChars => Some(format!("{}/{}", budget.used, maximum)),
        BudgetKind::Role { role } => Some(format!(
            "{}/{} {}",
            budget.used,
            maximum,
            role_budget_noun(role.as_str())?
        )),
    }
}

pub(super) fn tray_budget_labels(
    budgets: &[LimitBudget],
    attachments: &[ComposerAttachment],
) -> Vec<String> {
    let mut labels = Vec::new();
    let mut frame_max = 0u32;
    let mut has_frames = false;
    for budget in budgets {
        match &budget.kind {
            BudgetKind::Role { role } if matches!(role.as_str(), ROLE_SOURCE | ROLE_LAST_FRAME) => {
                let Some(maximum) = budget.maximum else {
                    continue;
                };
                has_frames = true;
                frame_max += maximum;
            }
            BudgetKind::Role { .. } => {
                if let Some(label) = budget_label(budget) {
                    labels.push(label);
                }
            }
            BudgetKind::PromptChars => {}
        }
    }
    if has_frames && frame_max > 0 {
        let used = attachments
            .iter()
            .filter(|attachment| !attachment.pending && attachment.kind == ComposerMediaKind::Image)
            .count() as u32;
        labels.insert(0, format!("{used}/{frame_max} frames"));
    }
    labels
}

fn role_budget_noun(role: &str) -> Option<&'static str> {
    match role {
        ROLE_REFERENCE => Some("images"),
        ROLE_REFERENCE_VIDEO => Some("videos"),
        ROLE_REFERENCE_AUDIO => Some("audios"),
        _ => None,
    }
}

pub(super) fn studio_drop_veil_copy(accept: &TrayAccept) -> &'static str {
    let images = accept
        .mime_types
        .iter()
        .any(|mime| mime.starts_with("image/"));
    let videos = accept
        .mime_types
        .iter()
        .any(|mime| mime.starts_with("video/"));
    let audios = accept
        .mime_types
        .iter()
        .any(|mime| mime.starts_with("audio/"));
    match (images, videos, audios) {
        (true, false, false) => "Drop images to attach",
        (false, true, false) => "Drop videos to attach",
        (false, false, true) => "Drop audio to attach",
        (true, true, false) => "Drop images and videos to attach",
        _ => "Drop files to attach",
    }
}

pub(super) fn accepts_mime(accept: &TrayAccept, mime: &str) -> bool {
    let mime = normalize_mime(mime);
    accept
        .mime_types
        .iter()
        .any(|accepted| normalize_mime(accepted) == mime)
}

pub(super) fn mime_for_path(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "webp" => Some("image/webp"),
        "mp4" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        "wav" => Some("audio/wav"),
        "mp3" => Some("audio/mpeg"),
        _ => None,
    }
}

pub(super) fn kind_for_mime(mime: &str) -> Option<ComposerMediaKind> {
    match normalize_mime(mime).as_str() {
        "image/jpeg" | "image/png" | "image/webp" => Some(ComposerMediaKind::Image),
        "video/mp4" | "video/quicktime" => Some(ComposerMediaKind::Video),
        "audio/wav" | "audio/mpeg" => Some(ComposerMediaKind::Audio),
        other if other.starts_with("image/") => Some(ComposerMediaKind::Image),
        other if other.starts_with("video/") => Some(ComposerMediaKind::Video),
        other if other.starts_with("audio/") => Some(ComposerMediaKind::Audio),
        _ => None,
    }
}

fn kind_from_media(media_kind: MediaKind, mime: &str) -> ComposerMediaKind {
    kind_for_mime(mime).unwrap_or(match media_kind {
        MediaKind::Video => ComposerMediaKind::Video,
        MediaKind::Image => ComposerMediaKind::Image,
    })
}

fn normalize_mime(mime: &str) -> String {
    let mime = mime.trim().to_ascii_lowercase();
    if mime == "image/jpg" {
        "image/jpeg".into()
    } else {
        mime
    }
}

fn image_format_for_mime(mime: &str) -> Option<ImageFormat> {
    match normalize_mime(mime).as_str() {
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/png" => Some(ImageFormat::Png),
        "image/webp" => Some(ImageFormat::Webp),
        _ => None,
    }
}

fn stage_studio_paths(
    paths: Vec<PathBuf>,
    accept: &TrayAccept,
) -> Vec<Result<StagedStudioFile, String>> {
    paths
        .iter()
        .map(|path| stage_studio_file(path, accept))
        .collect()
}

fn stage_studio_file(path: &Path, accept: &TrayAccept) -> Result<StagedStudioFile, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let meta = std::fs::metadata(path).map_err(|_| format!("{name} could not be read."))?;
    if meta.len() > MAX_IMPORT_BYTES {
        return Err(too_large_message(&name));
    }
    if meta.len() == 0 {
        return Err(format!("{name} is empty."));
    }
    let bytes = std::fs::read(path).map_err(|_| format!("{name} could not be read."))?;
    if bytes.len() as u64 > MAX_IMPORT_BYTES {
        return Err(too_large_message(&name));
    }
    if bytes.is_empty() {
        return Err(format!("{name} is empty."));
    }
    let mime = sniff_media_mime(&bytes)
        .or_else(|| mime_for_path(path))
        .ok_or_else(|| format!("{name} is not a supported reference."))?;
    if !accepts_mime(accept, mime) {
        return Err(format!("{name} is not accepted by the selected models."));
    }
    let Some(kind) = kind_for_mime(mime) else {
        return Err(format!("{name} is not a supported reference."));
    };
    let hash = format!("{:x}", Sha256::digest(&bytes));
    Ok(StagedStudioFile {
        preview: tray_preview(&bytes, mime),
        bytes,
        mime: mime.to_owned(),
        kind,
        hash,
    })
}

fn too_large_message(name: &str) -> String {
    format!("{name} is too large (64 MiB max).")
}

fn tray_preview(bytes: &[u8], mime: &str) -> Option<Arc<Image>> {
    let format = image_format_for_mime(mime)?;
    Some(Arc::new(Image::from_bytes(format, bytes.to_vec())))
}

async fn import_studio_asset(
    engine: &EngineHandle,
    asset_id: StudioAssetId,
    bytes: &[u8],
    hash: &str,
    mime_hint: Option<&str>,
    name: &str,
) -> Result<ComposerAttachment, String> {
    let mut offset = 0u64;
    loop {
        let start = offset as usize;
        if start > bytes.len() {
            return Err(format!("{name} could not be uploaded."));
        }
        let remaining = bytes.len() - start;
        let last = remaining <= IMPORT_CHUNK_BYTES;
        let take = remaining.min(IMPORT_CHUNK_BYTES);
        let chunk = &bytes[start..start + take];
        let value = call_import(
            engine,
            import_params(asset_id, offset, chunk, last, hash, mime_hint),
        )
        .await
        .map_err(|error| friendly_import_error(&error.to_string(), name))?;
        let response = serde_json::from_value::<ImportStudioAssetResponse>(value)
            .map_err(|error| error.to_string())?;
        match response {
            ImportStudioAssetResponse::Complete(attachment) => return Ok(attachment),
            ImportStudioAssetResponse::Continue(continued) => {
                if last {
                    return Err("import finished without a committed attachment".into());
                }
                offset = import_next_offset(offset, continued.next_offset)?;
            }
        }
    }
}

fn import_next_offset(current: u64, next: u64) -> Result<u64, String> {
    if next <= current {
        Err("import stopped advancing".into())
    } else {
        Ok(next)
    }
}

fn friendly_import_error(message: &str, name: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("64 mib") || lower.contains("too large") || lower.contains("exceeds 64") {
        too_large_message(name)
    } else if lower.contains("nextoffset") || lower.contains("offset") {
        format!("{name} could not be uploaded.")
    } else {
        message.to_string()
    }
}

fn import_params(
    asset_id: StudioAssetId,
    offset: u64,
    chunk: &[u8],
    last: bool,
    hash: &str,
    mime_hint: Option<&str>,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "assetId": asset_id,
        "offset": offset,
        "data": base64::engine::general_purpose::STANDARD.encode(chunk),
        "last": last,
    });
    if last {
        params["expectedHash"] = serde_json::Value::String(hash.to_owned());
    }
    if let Some(mime) = mime_hint {
        params["mimeHint"] = serde_json::Value::String(mime.to_owned());
    }
    params
}

async fn call_import(
    engine: &EngineHandle,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    for attempt in 0..3 {
        match engine
            .client()
            .call(methods::IMPORT_STUDIO_ASSET, params.clone())
            .await
        {
            Ok(value) => return Ok(value),
            Err(error) if import_error_is_retryable(&error) && attempt < 2 => {}
            Err(error) => return Err(error),
        }
    }
    Err(RpcError::Closed)
}

fn import_error_is_retryable(error: &RpcError) -> bool {
    matches!(error, RpcError::Transport(_) | RpcError::Closed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_studio::{BudgetKind, InputRole, LimitBudget, ModelId};

    #[test]
    fn accept_filters_to_union_mimes() {
        let accept = TrayAccept {
            mime_types: vec!["image/png".into(), "video/mp4".into()],
        };
        assert!(accepts_mime(&accept, "image/png"));
        assert!(accepts_mime(&accept, "image/PNG"));
        assert!(!accepts_mime(&accept, "image/jpeg"));
        assert!(accepts_mime(&accept, "video/mp4"));
        assert!(!accepts_mime(&accept, "audio/wav"));
    }

    #[test]
    fn path_mime_covers_r2v_kinds() {
        assert_eq!(mime_for_path(Path::new("ref.JPG")), Some("image/jpeg"));
        assert_eq!(
            mime_for_path(Path::new("clip.mov")),
            Some("video/quicktime")
        );
        assert_eq!(mime_for_path(Path::new("line.mp3")), Some("audio/mpeg"));
        assert_eq!(kind_for_mime("image/jpeg"), Some(ComposerMediaKind::Image));
        assert_eq!(
            kind_for_mime("video/quicktime"),
            Some(ComposerMediaKind::Video)
        );
        assert_eq!(kind_for_mime("audio/wav"), Some(ComposerMediaKind::Audio));
    }

    fn import_chunk_plan(total: usize, chunk: usize) -> Vec<(u64, usize, bool)> {
        if total == 0 {
            return vec![(0, 0, true)];
        }
        let chunk = chunk.max(1);
        let mut offset = 0u64;
        let mut plan = Vec::new();
        while (offset as usize) < total {
            let remaining = total - offset as usize;
            let last = remaining <= chunk;
            let take = if last { remaining } else { chunk };
            plan.push((offset, take, last));
            offset += take as u64;
        }
        plan
    }

    #[test]
    fn import_plan_chunks_and_marks_last() {
        assert_eq!(
            import_chunk_plan(10, 4),
            vec![(0, 4, false), (4, 4, false), (8, 2, true)]
        );
        assert_eq!(import_chunk_plan(4, 4), vec![(0, 4, true)]);
        assert_eq!(import_chunk_plan(0, 4), vec![(0, 0, true)]);
    }

    #[test]
    fn budgets_print_used_over_maximum() {
        assert_eq!(
            budget_label(&LimitBudget {
                kind: BudgetKind::PromptChars,
                used: 812,
                maximum: Some(1000),
                subjects: Vec::new(),
                remaining: Some(188),
            })
            .as_deref(),
            Some("812/1000")
        );
        assert_eq!(
            budget_label(&LimitBudget {
                kind: BudgetKind::Role {
                    role: InputRole::new(ROLE_REFERENCE),
                },
                used: 2,
                maximum: Some(4),
                subjects: vec![ModelId::new("seedance")],
                remaining: Some(2),
            })
            .as_deref(),
            Some("2/4 images")
        );
        assert_eq!(
            budget_label(&LimitBudget {
                kind: BudgetKind::Role {
                    role: InputRole::new(ROLE_REFERENCE_VIDEO),
                },
                used: 1,
                maximum: Some(3),
                subjects: Vec::new(),
                remaining: Some(2),
            })
            .as_deref(),
            Some("1/3 videos")
        );
        assert!(
            budget_label(&LimitBudget {
                kind: BudgetKind::Role {
                    role: InputRole::new(ROLE_SOURCE),
                },
                used: 1,
                maximum: Some(1),
                subjects: Vec::new(),
                remaining: Some(0),
            })
            .is_none(),
            "source/last_frame must not duplicate the images counter"
        );
        assert!(
            budget_label(&LimitBudget {
                kind: BudgetKind::PromptChars,
                used: 10,
                maximum: None,
                subjects: Vec::new(),
                remaining: None,
            })
            .is_none()
        );
    }

    #[test]
    fn i2v_budgets_print_combined_frames() {
        let budgets = [
            LimitBudget {
                kind: BudgetKind::Role {
                    role: InputRole::new(ROLE_SOURCE),
                },
                used: 0,
                maximum: Some(1),
                subjects: Vec::new(),
                remaining: Some(1),
            },
            LimitBudget {
                kind: BudgetKind::Role {
                    role: InputRole::new(ROLE_LAST_FRAME),
                },
                used: 0,
                maximum: Some(1),
                subjects: Vec::new(),
                remaining: Some(1),
            },
            LimitBudget {
                kind: BudgetKind::Role {
                    role: InputRole::new(ROLE_REFERENCE),
                },
                used: 0,
                maximum: Some(30),
                subjects: Vec::new(),
                remaining: Some(30),
            },
        ];
        assert_eq!(
            tray_budget_labels(&budgets, &[]),
            vec!["0/2 frames".to_owned(), "0/30 images".to_owned()]
        );
        let attached = [ComposerAttachment {
            id: StudioAssetId::new(),
            kind: ComposerMediaKind::Image,
            pending: false,
            origin: AttachmentOrigin::Asset,
            mime_type: "image/png".into(),
            byte_size: 12,
            width: None,
            height: None,
            duration_seconds: None,
            content_hash: "a".into(),
            role_hint: None,
        }];
        assert_eq!(
            tray_budget_labels(&budgets, &attached),
            vec!["1/2 frames".to_owned(), "0/30 images".to_owned()]
        );
    }

    #[test]
    fn drop_veil_copy_matches_accepted_kinds() {
        assert_eq!(
            studio_drop_veil_copy(&TrayAccept {
                mime_types: vec!["image/png".into()],
            }),
            "Drop images to attach"
        );
        assert_eq!(
            studio_drop_veil_copy(&TrayAccept {
                mime_types: vec!["image/png".into(), "video/mp4".into()],
            }),
            "Drop images and videos to attach"
        );
        assert_eq!(
            studio_drop_veil_copy(&TrayAccept {
                mime_types: vec!["image/png".into(), "video/mp4".into(), "audio/mpeg".into()],
            }),
            "Drop files to attach"
        );
    }

    #[test]
    fn make_video_closes_only_when_already_on_the_thread() {
        let conversation = StudioConversationId::new();
        assert!(make_video_already_on_thread(
            Some(conversation),
            Some(conversation)
        ));
        assert!(!make_video_already_on_thread(None, Some(conversation)));
        assert!(!make_video_already_on_thread(
            Some(StudioConversationId::new()),
            Some(conversation)
        ));
    }

    #[test]
    fn import_stalls_when_next_offset_does_not_advance() {
        assert!(import_next_offset(12, 12).is_err());
        assert!(import_next_offset(12, 8).is_err());
        assert_eq!(import_next_offset(12, 24).unwrap(), 24);
    }

    #[test]
    fn import_errors_are_friendly_for_size_and_offset() {
        assert_eq!(
            friendly_import_error("bad params: studio asset exceeds 64 MiB", "clip.mp4"),
            "clip.mp4 is too large (64 MiB max)."
        );
        assert_eq!(
            friendly_import_error(
                "bad params: import offset must equal nextOffset",
                "still.png"
            ),
            "still.png could not be uploaded."
        );
        assert!(!import_error_is_retryable(&RpcError::BadParams(
            "studio asset exceeds 64 MiB".into()
        )));
        assert!(import_error_is_retryable(&RpcError::Closed));
        assert!(import_error_is_retryable(&RpcError::Transport(
            "reset".into()
        )));
    }

    #[test]
    fn stage_sniffs_bytes_then_accepts_mime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ref.bin");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let accept = TrayAccept {
            mime_types: vec!["image/png".into()],
        };
        let staged = stage_studio_file(&path, &accept).unwrap();
        assert_eq!(staged.mime, "image/png");
        assert_eq!(staged.kind, ComposerMediaKind::Image);

        let video_only = TrayAccept {
            mime_types: vec!["video/mp4".into()],
        };
        let rejected = stage_studio_file(&path, &video_only).err().expect("reject");
        assert!(rejected.contains("not accepted"), "{rejected}");
    }

    #[test]
    fn stage_falls_back_to_extension_when_sniff_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        std::fs::write(&path, b"not a real container").unwrap();
        let accept = TrayAccept {
            mime_types: vec!["video/mp4".into()],
        };
        let staged = stage_studio_file(&path, &accept).unwrap();
        assert_eq!(staged.mime, "video/mp4");
        assert_eq!(staged.kind, ComposerMediaKind::Video);
    }

    #[test]
    fn make_video_pins_source_when_any_i2v() {
        assert_eq!(
            make_video_role_hint([
                MediaOperation::ImageToVideo,
                MediaOperation::ReferenceToVideo
            ]),
            Some(InputRole::new(ROLE_SOURCE))
        );
        assert_eq!(
            make_video_role_hint([MediaOperation::ReferenceToVideo]),
            Some(InputRole::new(ROLE_REFERENCE))
        );
        assert_eq!(make_video_role_hint([MediaOperation::TextToVideo]), None);
    }

    #[test]
    fn pending_attachment_blocks_send() {
        use chrono::Utc;
        use zeron_studio::{
            ComposerSnapshot, InputConstraint, MediaModel, MimeConstraint, SelectedModelRef,
            evaluate_composer,
        };

        let mut model = MediaModel {
            provider_id: "venice".into(),
            id: "seedance-r2v".into(),
            display_name: "Seedance R2V".into(),
            description: None,
            operation: MediaOperation::ReferenceToVideo,
            output_kind: MediaKind::Video,
            output_mime_types: vec!["video/mp4".into()],
            input_constraints: vec![InputConstraint {
                role: InputRole::new(ROLE_REFERENCE),
                minimum_count: 0,
                maximum_count: 9,
                mime: MimeConstraint {
                    accepted: vec!["image/png".into()],
                    ..MimeConstraint::default()
                },
            }],
            prompt_maximum_chars: Some(1000),
            negative_prompt_maximum_chars: None,
            maximum_output_count: 1,
            controls: Vec::new(),
            pricing: None,
            features: Vec::new(),
            video: zeron_studio::VideoModelMeta {
                adapter_family: zeron_studio::AdapterFamily::Seedance,
                ..zeron_studio::VideoModelMeta::default()
            },
            manifest_version: "test".into(),
            fetched_at: Utc::now(),
        };
        model.video.requires_visual_reference = false;
        let snapshot = ComposerSnapshot {
            mode: ComposerMode::Video,
            prompt: "a comet".into(),
            selected: vec![SelectedModelRef::new("venice", "seedance-r2v")],
            attachments: vec![ComposerAttachment {
                id: StudioAssetId::new(),
                kind: ComposerMediaKind::Image,
                pending: true,
                origin: AttachmentOrigin::Asset,
                mime_type: "image/png".into(),
                byte_size: 12,
                width: Some(64),
                height: Some(64),
                duration_seconds: None,
                content_hash: "abc".into(),
                role_hint: None,
            }],
            ..ComposerSnapshot::default()
        };
        let view = evaluate_composer(&snapshot, std::slice::from_ref(&model));
        assert!(!view.send.enabled);
    }

    #[test]
    fn artifact_attachment_is_committed_and_not_pending() {
        let artifact_id = StudioArtifactId::new();
        let attachment = attachment_from_artifact(
            ArtifactReferenceMeta {
                artifact_id,
                mime_type: "image/png".into(),
                byte_size: 12,
                width: Some(64),
                height: Some(64),
                duration_seconds: None,
                content_hash: "abc".into(),
                kind: ComposerMediaKind::Image,
            },
            Some(InputRole::new(ROLE_REFERENCE)),
        );
        assert!(!attachment.pending);
        assert_eq!(attachment.id.0, artifact_id.0);
        assert!(matches!(
            attachment.origin,
            AttachmentOrigin::Artifact { artifact_id: id } if id == artifact_id
        ));
        assert_eq!(
            attachment.role_hint.as_ref().map(|role| role.as_str()),
            Some(ROLE_REFERENCE)
        );
    }
}
