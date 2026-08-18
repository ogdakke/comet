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
use zeron_rpc::methods;
use zeron_studio::{
    AttachmentOrigin, BudgetKind, ComposerAttachment, ComposerEvent, ComposerMediaKind,
    ComposerMode, InputRole, LimitBudget, MediaKind, MediaOperation, ROLE_REFERENCE, ROLE_SOURCE,
    StudioArtifactId, StudioAssetId, TrayAccept,
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
                Ok(Ok(Some(paths))) => paths,
                _ => return,
            };
            this.update(cx, |page, cx| page.attach_tray_paths(paths, &accept, cx))
                .ok();
        }));
    }

    pub(super) fn attach_tray_paths(
        &mut self,
        paths: Vec<PathBuf>,
        accept: &TrayAccept,
        cx: &mut Context<Self>,
    ) {
        let mut first_error = None;
        for path in paths {
            match stage_studio_file(&path, accept) {
                Ok(file) => self.begin_tray_import(file, cx),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if let Some(error) = first_error {
            self.error = Some(error.into());
            cx.notify();
        }
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
                    import_studio_asset(&engine, asset_id, &bytes, &hash, Some(&mime)).await;
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
        self.request_close_artifact(cx);
        if self.selected_conversation.is_none()
            && let Some(conversation_id) = self.artifact_menu_conversation(artifact_id)
        {
            self.open_conversation(conversation_id, cx);
            // Skip last-turn restore so this pin is not overwritten.
            self.composer_seeded_for = Some(conversation_id);
            cx.emit(super::StudioEvent::ShowThread {
                conversation_id,
                focus_artifact: None,
            });
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
        let icon = match attachment.kind {
            ComposerMediaKind::Image => crate::icons::GALLERY_MINIMALISTIC,
            ComposerMediaKind::Video => crate::icons::GALLERY_WIDE,
            ComposerMediaKind::Audio => crate::icons::VOLUME_LOUD,
        };
        div()
            .id(SharedString::from(format!("studio-att-{}", asset_id.0)))
            .group(group.clone())
            .relative()
            .size(px(TRAY_THUMB))
            .flex_none()
            .rounded(px(8.0))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(crate::theme::wash(0.06))
            .flex()
            .items_center()
            .justify_center()
            .when(pending, |chip| chip.opacity(0.7))
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
                        .child(
                            crate::icons::icon(crate::icons::REFRESH)
                                .size(px(12.0))
                                .text_color(theme.text),
                        ),
                )
            })
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
            return self.images.get_thumb(&artifact_id);
        }
        None
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
        let labels = self
            .composer_view
            .budgets
            .iter()
            .filter(|budget| matches!(budget.kind, BudgetKind::Role { .. }))
            .filter_map(budget_label)
            .collect::<Vec<_>>();
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
            role_budget_noun(role.as_str())
        )),
    }
}

fn role_budget_noun(role: &str) -> &'static str {
    match role {
        zeron_studio::ROLE_REFERENCE_VIDEO => "videos",
        zeron_studio::ROLE_REFERENCE_AUDIO | zeron_studio::ROLE_AUDIO => "audios",
        _ => "images",
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

fn stage_studio_file(path: &Path, accept: &TrayAccept) -> Result<StagedStudioFile, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let Some(mime) = mime_for_path(path) else {
        return Err(format!("{name} is not a supported reference."));
    };
    if !accepts_mime(accept, mime) {
        return Err(format!("{name} is not accepted by the selected models."));
    }
    let Some(kind) = kind_for_mime(mime) else {
        return Err(format!("{name} is not a supported reference."));
    };
    let meta = std::fs::metadata(path).map_err(|_| format!("{name} could not be read."))?;
    if meta.len() > MAX_IMPORT_BYTES {
        return Err(format!("{name} is too large (64 MB max)."));
    }
    if meta.len() == 0 {
        return Err(format!("{name} is empty."));
    }
    let bytes = std::fs::read(path).map_err(|_| format!("{name} could not be read."))?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let preview = image_format_for_mime(mime)
        .map(|format| Arc::new(Image::from_bytes(format, bytes.clone())));
    Ok(StagedStudioFile {
        bytes,
        mime: mime.to_owned(),
        kind,
        hash,
        preview,
    })
}

async fn import_studio_asset(
    engine: &EngineHandle,
    asset_id: StudioAssetId,
    bytes: &[u8],
    hash: &str,
    mime_hint: Option<&str>,
) -> Result<ComposerAttachment, String> {
    let mut offset = 0u64;
    loop {
        let start = offset as usize;
        if start > bytes.len() {
            return Err("import offset exceeded the file".into());
        }
        let remaining = bytes.len() - start;
        let last = remaining <= IMPORT_CHUNK_BYTES;
        let take = remaining.min(IMPORT_CHUNK_BYTES);
        let chunk = &bytes[start..start + take];
        let value = call_import(
            engine,
            import_params(asset_id, offset, chunk, last, hash, mime_hint),
        )
        .await?;
        let response = serde_json::from_value::<ImportStudioAssetResponse>(value)
            .map_err(|error| error.to_string())?;
        match response {
            ImportStudioAssetResponse::Complete(attachment) => return Ok(attachment),
            ImportStudioAssetResponse::Continue(continued) => {
                if last {
                    return Err("import finished without a committed attachment".into());
                }
                offset = continued.next_offset;
            }
        }
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
) -> Result<serde_json::Value, String> {
    let mut last_error = String::new();
    for attempt in 0..3 {
        match engine
            .client()
            .call(methods::IMPORT_STUDIO_ASSET, params.clone())
            .await
        {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = error.to_string();
                if attempt == 2 {
                    break;
                }
            }
        }
    }
    Err(last_error)
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
