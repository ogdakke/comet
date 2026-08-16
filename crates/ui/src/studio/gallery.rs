//! Profile-wide image gallery: virtualized grid, multi-select, bulk actions.

use std::ops::Range;

use gpui::{
    AnyElement, Context, ListAlignment, ListOffset, ListScrollEvent, ListState, MouseButton,
    PathPromptOptions, SharedString, Window, canvas, div, list, prelude::*, px,
};
use zeron_proto::StudioGalleryItem;
use zeron_rpc::methods;
use zeron_studio::StudioArtifactId;

use crate::icons;
use crate::theme::{Theme, hairline};

use super::StudioEvent;
use super::artifact::{
    cover_image, downsample_gallery_thumb, read_artifact_bytes, write_artifact_file,
};
use super::page::StudioPage;

const GALLERY_PAD: f32 = 20.0;
const GALLERY_GAP: f32 = 10.0;
const GALLERY_MIN_TILE: f32 = 248.0;
const GALLERY_CHECK: f32 = 20.0;
// Image decoding is CPU-heavy. A large fan-out makes scrolling contend with
// the render thread even though the list itself is virtualized.
const MAX_IN_FLIGHT_IMAGES: usize = 2;
/// Outward cache fill gets one slot; the other remains available to the
/// viewport runway, hover, and lightbox work.
const MAX_BACKGROUND_IMAGES: usize = 1;
/// Synchronous runway maintained on every scroll update. At five columns this
/// is up to 40 previews on either side, while decode concurrency stays bounded.
const PREFETCH_ROWS: usize = 8;

pub(super) fn gallery_columns(width: f32) -> usize {
    let inner = (width - GALLERY_PAD * 2.0).max(1.0);
    let cols = ((inner + GALLERY_GAP) / (GALLERY_MIN_TILE + GALLERY_GAP)).floor() as usize;
    cols.clamp(1, 16)
}

pub(super) fn gallery_tile_size(width: f32, columns: usize) -> f32 {
    let inner = (width - GALLERY_PAD * 2.0).max(1.0);
    let columns = columns.max(1) as f32;
    (inner - GALLERY_GAP * (columns - 1.0)) / columns
}

pub(super) fn gallery_row_height(width: f32) -> f32 {
    gallery_tile_size(width, gallery_columns(width)) + GALLERY_GAP
}

pub(super) fn gallery_row_count(item_count: usize, columns: usize) -> usize {
    if item_count == 0 || columns == 0 {
        0
    } else {
        item_count.div_ceil(columns)
    }
}

pub(super) fn select_index_range(count: usize, from: usize, to: usize) -> Range<usize> {
    let start = from.min(to).min(count.saturating_sub(1));
    let end = from.max(to).min(count.saturating_sub(1));
    if count == 0 { 0..0 } else { start..end + 1 }
}

fn gallery_prefetch_row_order(total_rows: usize, visible: Range<usize>) -> Vec<usize> {
    let start = visible.start.min(total_rows);
    let end = visible.end.min(total_rows).max(start);
    let radius = start.max(total_rows.saturating_sub(end));
    let mut rows = Vec::with_capacity(total_rows.saturating_sub(end - start));
    for distance in 0..radius {
        if let Some(below) = end.checked_add(distance).filter(|row| *row < total_rows) {
            rows.push(below);
        }
        if let Some(above) = start.checked_sub(distance + 1) {
            rows.push(above);
        }
    }
    rows
}

pub(super) fn new_gallery_list(cx: &mut Context<StudioPage>) -> ListState {
    let list = ListState::new(0, ListAlignment::Top, px(1600.0));
    let weak = cx.weak_entity();
    list.set_scroll_handler(move |event: &ListScrollEvent, _, cx| {
        weak.update(cx, |page: &mut StudioPage, cx| {
            page.gallery_visible_rows = event.visible_range.clone();
            page.request_visible_gallery_images(cx);
            cx.notify();
        })
        .ok();
    });
    list
}

impl StudioPage {
    pub fn show_gallery(&mut self, cx: &mut Context<Self>) {
        self.close_image_menu(cx);
        self.scroll_to_artifact = None;
        self.close_artifact(cx);
        self.gallery_selected.clear();
        self.gallery_anchor = None;
        if self.selected_conversation.take().is_some() {
            self.conversation = None;
            self.watch_task = None;
            self.composer_seeded_for = None;
            self.expanded_prompts.clear();
            self.reset_feed_list();
            cx.emit(StudioEvent::SidebarChanged);
        }
        cx.notify();
    }

    pub fn is_gallery(&self) -> bool {
        self.selected_conversation.is_none()
    }

    pub fn gallery_image_count(&self) -> u32 {
        self.gallery.len() as u32
    }

    pub(super) fn gallery_content_width(&self, window: &Window) -> f32 {
        if self.gallery_width > 1.0 {
            self.gallery_width
        } else {
            (f32::from(window.viewport_size().width) - crate::settings::SIDEBAR_DEFAULT).max(320.0)
        }
    }

    pub(super) fn sync_gallery_list(&mut self, width: f32) {
        let columns = gallery_columns(width);
        let rows = gallery_row_count(self.gallery.len(), columns);
        let height = gallery_row_height(width);
        let keep = self
            .gallery_visible_rows
            .start
            .checked_mul(self.gallery_list_columns.max(1))
            .and_then(|index| self.gallery.get(index).map(|item| item.id));
        if self.gallery_list.item_count() != rows
            || self.gallery_list_columns != columns
            || (self.gallery_row_px - height).abs() > 0.5
        {
            self.gallery_list
                .reset_with_uniform_height(rows, px(height.max(1.0)));
            self.gallery_list_columns = columns;
            self.gallery_row_px = height;
            if let Some(id) = keep
                && let Some(index) = self.gallery.iter().position(|item| item.id == id)
            {
                self.gallery_list.scroll_to(ListOffset {
                    item_ix: index / columns.max(1),
                    offset_in_item: px(0.0),
                });
            }
        }
    }

    pub(super) fn apply_gallery(
        &mut self,
        artifacts: Vec<StudioGalleryItem>,
        cx: &mut Context<Self>,
    ) {
        let previous = self
            .gallery_visible_rows
            .start
            .checked_mul(self.gallery_list_columns.max(1))
            .and_then(|index| self.gallery.get(index).map(|item| item.id));
        self.gallery = artifacts;
        self.gallery_selected
            .retain(|id| self.gallery.iter().any(|item| item.id == *id));
        if let Some(anchor) = self.gallery_anchor
            && !self.gallery.iter().any(|item| item.id == anchor)
        {
            self.gallery_anchor = None;
        }
        if let Some(selected) = self.selected_artifact
            && !self.gallery.iter().any(|item| item.id == selected)
        {
            self.selected_artifact = None;
            self.reset_lightbox_viewer();
            cx.emit(StudioEvent::CloseArtifact);
        }
        self.sync_gallery_list(self.gallery_width.max(1.0));
        if let Some(id) = previous
            && let Some(index) = self.gallery.iter().position(|item| item.id == id)
        {
            let columns = self.gallery_list_columns.max(1);
            self.gallery_list.scroll_to(ListOffset {
                item_ix: index / columns,
                offset_in_item: px(0.0),
            });
        }
        self.request_visible_gallery_images(cx);
    }

    pub(super) fn watch_gallery(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.gallery_watch_task = Some(cx.spawn(async move |this, cx| {
            let stream = engine
                .client()
                .subscribe(methods::WATCH_STUDIO_GALLERY, serde_json::json!({}))
                .await;
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    this.update(cx, |page, cx| {
                        page.error = Some(error.to_string().into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            while let Some(value) = stream.recv().await {
                let Ok(list) =
                    serde_json::from_value::<zeron_proto::ListStudioArtifactsResponse>(value)
                else {
                    continue;
                };
                if this
                    .update(cx, |page, cx| {
                        page.apply_gallery(list.artifacts, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    pub(super) fn gallery_item(&self, id: StudioArtifactId) -> Option<&StudioGalleryItem> {
        self.gallery.iter().find(|item| item.id == id)
    }

    fn sync_gallery_visible_rows(&mut self) {
        let rows = gallery_row_count(self.gallery.len(), self.gallery_list_columns.max(1));
        if rows == 0 {
            self.gallery_visible_rows = 0..0;
            return;
        }
        let top = self.gallery_list.logical_scroll_top();
        let viewport_h = f32::from(self.gallery_list.viewport_bounds().size.height);
        let row_h = self.gallery_row_px.max(1.0);
        let span = ((viewport_h + f32::from(top.offset_in_item)) / row_h).ceil() as usize;
        self.gallery_visible_rows = top.item_ix.min(rows)..(top.item_ix + span.max(1)).min(rows);
    }

    fn gallery_ids_around_visible(&self, extra_rows: usize) -> Vec<StudioArtifactId> {
        let columns = self.gallery_list_columns.max(1);
        let rows = if self.gallery_visible_rows.end > self.gallery_visible_rows.start {
            self.gallery_visible_rows.clone()
        } else {
            0..4
        };
        let start = rows
            .start
            .saturating_sub(extra_rows)
            .saturating_mul(columns);
        let end = rows
            .end
            .saturating_add(extra_rows)
            .saturating_mul(columns)
            .min(self.gallery.len());
        self.gallery
            .get(start..end)
            .unwrap_or(&[])
            .iter()
            .map(|item| item.id)
            .collect()
    }

    fn gallery_ids_outward_from_visible(&self) -> Vec<StudioArtifactId> {
        let columns = self.gallery_list_columns.max(1);
        let total_rows = gallery_row_count(self.gallery.len(), columns);
        let visible = if self.gallery_visible_rows.end > self.gallery_visible_rows.start {
            self.gallery_visible_rows.clone()
        } else {
            0..1.min(total_rows)
        };
        gallery_prefetch_row_order(total_rows, visible)
            .into_iter()
            .flat_map(|row| {
                let start = row * columns;
                self.gallery
                    .get(start..(start + columns).min(self.gallery.len()))
                    .unwrap_or(&[])
                    .iter()
                    .map(|item| item.id)
            })
            .collect()
    }

    pub(super) fn request_visible_gallery_images(&mut self, cx: &mut Context<Self>) {
        if let Some(selected) = self.selected_artifact {
            let fulls = super::artifact::lightbox_neighbor_ids(&self.lightbox_frames, selected);
            let mut thumbs = self.visible_filmstrip_ids();
            for id in &fulls {
                if !thumbs.contains(id) {
                    thumbs.push(*id);
                }
            }
            self.image_protect = thumbs.iter().chain(fulls.iter()).copied().collect();
            self.image_protect
                .extend(self.loading_images.iter().copied());
            // Start each lightbox neighbor as a thumb request, then promote
            // that same in-flight read to retain the original. This ensures
            // every paging target has a fallback instead of letting full-only
            // work starve the never-blank path.
            self.request_images(fulls.clone(), false, cx);
            self.request_images(fulls, true, cx);
            self.request_images(thumbs, false, cx);
            return;
        }
        if self.selected_conversation.is_some() {
            self.request_visible_feed_images(cx);
            return;
        }
        let visible = self.gallery_ids_around_visible(0);
        let mut thumbs = Vec::new();
        for id in self.gallery_ids_around_visible(PREFETCH_ROWS) {
            if !visible.contains(&id) {
                thumbs.push(id);
            }
        }
        self.image_protect = visible.iter().chain(thumbs.iter()).copied().collect();
        self.image_protect
            .extend(self.loading_images.iter().copied());
        // A sharp, cover-capable preview is the stable grid resource. Loading
        // originals for every visible tile turns scrolling into a decode and
        // GPU-cache eviction loop; hover/open promotes only the likely target.
        self.request_images(visible, false, cx);
        self.request_images(thumbs, false, cx);
        // Continuously spend one otherwise-free slot filling outward. This is
        // capacity-driven rather than scroll-idle-driven, so casual scrolling
        // still warms the gallery while foreground work keeps priority.
        if self.images.has_thumb_capacity() {
            let outward = self.gallery_ids_outward_from_visible();
            self.request_images_with_limit(outward, false, MAX_BACKGROUND_IMAGES, false, cx);
        }
    }

    pub(super) fn request_images(
        &mut self,
        ids: Vec<StudioArtifactId>,
        full: bool,
        cx: &mut Context<Self>,
    ) {
        self.request_images_with_limit(ids, full, MAX_IN_FLIGHT_IMAGES, true, cx);
    }

    fn request_images_with_limit(
        &mut self,
        ids: Vec<StudioArtifactId>,
        full: bool,
        max_in_flight: usize,
        protect_requests: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        if protect_requests {
            self.images.touch(ids.iter().copied());
            self.image_protect.extend(ids.iter().copied());
        }
        let mut inflight = self.loading_images.len();
        for id in ids {
            let ready = if full {
                self.images.contains_full(&id)
            } else {
                self.images.contains_thumb(&id)
            };
            if ready || self.image_failed.contains(&id) {
                continue;
            }
            if self.loading_images.contains(&id) {
                if full {
                    // Promote an already-running thumb read instead of
                    // fetching the same original a second time.
                    self.loading_full_images.insert(id);
                }
                continue;
            }
            if inflight >= max_in_flight {
                break;
            }
            if full {
                self.loading_full_images.insert(id);
            }
            let need_thumb = !self.images.contains_thumb(&id);
            self.loading_images.insert(id);
            inflight += 1;
            let engine = engine.clone();
            let task = cx.spawn(async move |this, cx| {
                let loaded = read_artifact_bytes(&engine, id).await;
                let keep_full = this
                    .update(cx, |page, _| page.loading_full_images.contains(&id))
                    .unwrap_or(full);
                let decoded = match loaded {
                    Ok((_, mime, bytes)) => {
                        let format = gpui::ImageFormat::from_mime_type(&mime)
                            .unwrap_or(gpui::ImageFormat::Png);
                        let full_image = keep_full.then(|| {
                            std::sync::Arc::new(gpui::Image::from_bytes(format, bytes.clone()))
                        });
                        let thumb = if need_thumb {
                            cx.background_executor()
                                .spawn(async move { downsample_gallery_thumb(bytes) })
                                .await
                                .ok()
                        } else {
                            None
                        };
                        Ok((full_image, thumb))
                    }
                    Err(error) => Err(error),
                };
                this.update(cx, |page, cx| {
                    page.loading_images.remove(&id);
                    page.loading_full_images.remove(&id);
                    match decoded {
                        Ok((full_image, thumb)) => {
                            if let Some(full_image) = full_image {
                                page.images.insert_full(id, full_image);
                            }
                            if let Some(thumb) = thumb {
                                page.images.insert_thumb(id, thumb);
                            }
                        }
                        Err(_) => {
                            page.image_failed.insert(id);
                        }
                    }
                    page.image_tasks.remove(&id);
                    page.images.evict(&page.image_protect);
                    page.request_visible_gallery_images(cx);
                    cx.notify();
                })
                .ok();
            });
            self.image_tasks.insert(id, task);
        }
        self.images.evict(&self.image_protect);
    }

    fn toggle_gallery_selected(&mut self, id: StudioArtifactId, cx: &mut Context<Self>) {
        if !self.gallery_selected.remove(&id) {
            self.gallery_selected.insert(id);
        }
        self.gallery_anchor = Some(id);
        cx.notify();
    }

    fn select_gallery_range(&mut self, id: StudioArtifactId, cx: &mut Context<Self>) {
        let Some(end) = self.gallery.iter().position(|item| item.id == id) else {
            return;
        };
        let start = self
            .gallery_anchor
            .and_then(|anchor| self.gallery.iter().position(|item| item.id == anchor))
            .unwrap_or(end);
        self.gallery_selected.clear();
        for item in &self.gallery[select_index_range(self.gallery.len(), start, end)] {
            self.gallery_selected.insert(item.id);
        }
        if self.gallery_anchor.is_none() {
            self.gallery_anchor = Some(id);
        }
        cx.notify();
    }

    fn clear_gallery_selection(&mut self, cx: &mut Context<Self>) {
        if !self.gallery_selected.is_empty() {
            self.gallery_selected.clear();
            cx.notify();
        }
    }

    fn select_all_gallery(&mut self, cx: &mut Context<Self>) {
        let all = self.gallery.iter().map(|item| item.id).collect();
        if self.gallery_selected != all {
            self.gallery_selected = all;
            cx.notify();
        }
    }

    fn on_gallery_item_click(
        &mut self,
        id: StudioArtifactId,
        event: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.modifiers();
        if modifiers.shift {
            self.select_gallery_range(id, cx);
        } else if modifiers.platform || modifiers.control {
            self.toggle_gallery_selected(id, cx);
        } else {
            let frames = super::artifact::frames_from_gallery(&self.gallery);
            self.open_artifact_viewer(id, frames, cx);
            window.focus(&self.focus, cx);
        }
    }

    pub(super) fn delete_selected_gallery(&mut self, cx: &mut Context<Self>) {
        let ids = self.gallery_selected.iter().copied().collect::<Vec<_>>();
        if ids.is_empty() {
            return;
        }
        self.delete_artifacts(ids, cx);
    }

    pub(super) fn delete_artifacts(&mut self, ids: Vec<StudioArtifactId>, cx: &mut Context<Self>) {
        if ids.is_empty() {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let close = self
            .selected_artifact
            .is_some_and(|selected| ids.contains(&selected));
        self.busy = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let mut first_error = None;
            for id in ids {
                let result = engine
                    .client()
                    .call(
                        methods::DELETE_STUDIO_ARTIFACT,
                        serde_json::json!({ "artifactId": id }),
                    )
                    .await;
                let failed = this
                    .update(cx, |page, _| match result {
                        Ok(_) => {
                            page.forget_artifact(id);
                            false
                        }
                        Err(error) => {
                            first_error = Some(error.to_string());
                            true
                        }
                    })
                    .unwrap_or(true);
                if failed {
                    break;
                }
            }
            this.update(cx, |page, cx| {
                page.busy = false;
                page.gallery_selected.clear();
                if let Some(error) = first_error {
                    page.error = Some(error.into());
                } else if close {
                    cx.emit(StudioEvent::CloseArtifact);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn download_selected_gallery(&mut self, cx: &mut Context<Self>) {
        let ids = self.gallery_selected.iter().copied().collect::<Vec<_>>();
        self.download_artifacts(ids, cx);
    }

    pub(super) fn download_artifacts(
        &mut self,
        ids: Vec<StudioArtifactId>,
        cx: &mut Context<Self>,
    ) {
        if ids.is_empty() {
            return;
        }
        if ids.len() == 1 {
            self.download_artifact(ids[0], cx);
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let names = ids
            .iter()
            .map(|id| (*id, self.artifact_file_name(*id)))
            .collect::<Vec<_>>();
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Save images".into()),
        });
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let folder = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    this.update(cx, |page, cx| {
                        page.error = Some(error.to_string().into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                Err(error) => {
                    this.update(cx, |page, cx| {
                        page.error = Some(error.to_string().into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let Some(folder) = folder else {
                return;
            };
            for (id, name) in names {
                let destination = folder.join(name);
                let result = match read_artifact_bytes(&engine, id).await {
                    Ok((_, _, bytes)) => {
                        let destination = destination.clone();
                        cx.background_executor()
                            .spawn(async move { write_artifact_file(destination, bytes) })
                            .await
                    }
                    Err(error) => Err(error),
                };
                if let Err(error) = result {
                    this.update(cx, |page, cx| {
                        page.error = Some(error.into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            }
        }));
    }

    pub(super) fn artifact_file_name(&self, artifact_id: StudioArtifactId) -> String {
        let mime = self
            .artifact_frame(artifact_id)
            .map(|frame| frame.mime_type.as_str())
            .or_else(|| {
                self.gallery_item(artifact_id)
                    .map(|item| item.mime_type.as_str())
            })
            .or_else(|| {
                self.conversation
                    .iter()
                    .flat_map(|view| &view.turns)
                    .flat_map(|turn| &turn.runs)
                    .flat_map(|run| &run.artifacts)
                    .find(|artifact| artifact.id == artifact_id)
                    .map(|artifact| artifact.mime_type.as_str())
            })
            .unwrap_or("image/png");
        let extension = match mime {
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            _ => "png",
        };
        format!("studio-{}.{extension}", artifact_id.0)
    }

    pub(super) fn render_gallery(
        &mut self,
        window: &mut Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.images.flush(Some(window), cx);
        let width = self.gallery_content_width(window);
        self.sync_gallery_list(width);
        self.request_visible_gallery_images(cx);
        let selected_count = self.gallery_selected.len();
        let empty = self.gallery.is_empty();
        let top = self.gallery_list.logical_scroll_top();
        let fade_top = top.item_ix > 0 || f32::from(top.offset_in_item) > 1.0;
        let fade_bottom = self.gallery_list.is_scrolled_to_end() == Some(false);
        let measure_entity = cx.weak_entity();
        let list_element = list(
            self.gallery_list.clone(),
            cx.processor(Self::render_gallery_row),
        )
        .flex_1()
        .size_full()
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto);
        let body = if empty {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(crate::motion::fade_in(
                    "studio-gallery-empty",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            crate::icons::icon(crate::icons::WIDGET)
                                .w(px(28.0))
                                .h(px(28.0))
                                .text_color(theme.text.opacity(0.2)),
                        )
                        .child(
                            div()
                                .mt(px(12.0))
                                .text_size(px(14.0))
                                .text_color(theme.text_muted.opacity(0.6))
                                .child("No images yet"),
                        )
                        .child(
                            div()
                                .mt(px(4.0))
                                .text_size(px(12.0))
                                .text_color(theme.text_faint)
                                .child("Open a thread and generate to fill the gallery."),
                        ),
                ))
                .into_any_element()
        } else {
            div()
                .id("studio-gallery-scroll")
                .size_full()
                // The list must underlap the fade band; padding it past the
                // band made the existing EdgeFade scope visually inert.
                .pt(px(Theme::TITLEBAR_HEIGHT))
                .child(list_element)
                .into_any_element()
        };
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, _, cx| {
                if page.dismiss_image_menu(event, cx) {
                    cx.stop_propagation();
                    return;
                }
                let key = event.keystroke.key.as_str();
                let chord = event.keystroke.modifiers.platform || event.keystroke.modifiers.control;
                if key == "escape" {
                    page.clear_gallery_selection(cx);
                } else if key == "a" && chord {
                    page.select_all_gallery(cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let width = f32::from(bounds.size.width);
                        let changed = measure_entity
                            .update(cx, |page, _| {
                                if width < 64.0 || (page.gallery_width - width).abs() <= 0.5 {
                                    return false;
                                }
                                page.gallery_width = width;
                                true
                            })
                            .unwrap_or(false);
                        if changed {
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .child(
                div().flex_1().min_h_0().w_full().child(
                    // Same titlebar underlap fade as the chat transcript:
                    // fully gone by the title text, ramping in the band
                    // just below it so tiles dissolve through the glass.
                    crate::edge_fade::edge_faded(
                        Theme::TRANSCRIPT_FADE_BAND,
                        fade_top,
                        fade_bottom,
                        body,
                    )
                    .inset_top(Theme::TITLEBAR_HEIGHT)
                    .band_top(Theme::TRANSCRIPT_FADE_BAND)
                    .band_bottom(Theme::TRANSCRIPT_FADE_BAND),
                ),
            )
            .when(!empty, |el| {
                let scrub = cx.weak_entity();
                el.child(
                    crate::scrollbar::overlay("studio-gallery", &self.gallery_list)
                        .inset_top(Theme::TITLEBAR_HEIGHT)
                        .on_scrub(move |_, cx| {
                            scrub
                                .update(cx, |page: &mut StudioPage, cx| {
                                    page.sync_gallery_visible_rows();
                                    page.request_visible_gallery_images(cx);
                                    cx.notify();
                                })
                                .ok();
                        }),
                )
            })
            .when(selected_count > 0, |el| {
                el.child(self.render_gallery_selection_bar(selected_count, theme, cx))
            })
            .into_any_element()
    }

    fn render_gallery_selection_bar(
        &self,
        count: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = if count == 1 {
            "1 selected".to_string()
        } else {
            format!("{count} selected")
        };
        let card = div()
            .h(px(36.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(hairline(0.10))
            .bg(if theme.is_glass() {
                theme.glass_overlay()
            } else {
                theme.surface_overlay
            })
            .when(!theme.is_glass(), |card| card.shadow_lg())
            .child(
                div()
                    .id("studio-gallery-clear-selection")
                    .h(px(26.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::wash(0.10)))
                    .on_click(cx.listener(|page, _, _, cx| page.clear_gallery_selection(cx)))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(label)),
                    ),
            )
            .child(
                gallery_bar_action(
                    "studio-gallery-download",
                    "Download",
                    icons::ARROW_DOWN,
                    theme,
                    theme.text_muted,
                )
                .on_click(cx.listener(|page, _, _, cx| page.download_selected_gallery(cx))),
            )
            .child(
                gallery_bar_action(
                    "studio-gallery-delete",
                    "Delete",
                    icons::TRASH_BIN_MINIMALISTIC,
                    theme,
                    theme.danger,
                )
                .on_click(cx.listener(|page, _, _, cx| page.delete_selected_gallery(cx))),
            );
        div()
            .absolute()
            .top(px(Theme::TITLEBAR_HEIGHT + 8.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .occlude()
            .child(crate::frost::frosted(12.0, crate::frost::MENU_BLUR, card))
            .into_any_element()
    }

    fn render_gallery_row(
        &mut self,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let columns = self.gallery_list_columns.max(1);
        let tile = gallery_tile_size(self.gallery_width.max(1.0), columns);
        let start = row * columns;
        div()
            .w_full()
            .h(px(tile + GALLERY_GAP))
            .px(px(GALLERY_PAD))
            .flex()
            .flex_row()
            .gap(px(GALLERY_GAP))
            .children((0..columns).filter_map(|column| {
                let item = self.gallery.get(start + column)?;
                Some(self.render_gallery_tile(item, tile, &theme, window, cx))
            }))
            .into_any_element()
    }

    pub(super) fn prefetch_gallery_full(
        &mut self,
        id: StudioArtifactId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(full) = self.images.get_full(&id) {
            let _ = full.use_render_image(window, cx);
        }
        self.request_images(vec![id], true, cx);
    }

    fn render_gallery_tile(
        &self,
        item: &StudioGalleryItem,
        tile: f32,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = item.id;
        let selected = self.gallery_selected.contains(&id);
        let (image, full) = self.display_layers(id, window, cx);
        let checkbox = div()
            .id(SharedString::from(format!("studio-gallery-check-{}", id.0)))
            .absolute()
            .top(px(8.0))
            .right(px(8.0))
            .size(px(GALLERY_CHECK))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .border_1()
            .border_color(if selected {
                theme.text
            } else {
                theme.text.opacity(0.88)
            })
            .when(selected, |check| check.bg(theme.text))
            // BlockMouse (`.occlude()`) ends the hit test, so a wheel over
            // the check never reaches the virtualized list underneath.
            .block_mouse_except_scroll()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |page, _, _, cx| {
                page.toggle_gallery_selected(id, cx);
            }))
            .when(selected, |check| {
                check.child(
                    icons::icon(icons::CHECK)
                        .size(px(11.0))
                        .text_color(theme.bg),
                )
            });
        let conversation_id = item.conversation_id;
        let frame = self.bind_image_menu(
            div()
                .id(SharedString::from(format!("studio-gallery-tile-{}", id.0)))
                .relative()
                .size(px(tile))
                .flex_none()
                .rounded(px(10.0))
                .overflow_hidden()
                .bg(crate::theme::ink(0.045))
                .cursor_pointer()
                .border_1()
                .border_color(if selected {
                    theme.text.opacity(0.7)
                } else {
                    gpui::hsla(0.0, 0.0, 0.0, 0.0)
                })
                .on_hover(cx.listener(move |page, hovered: &bool, window, cx| {
                    if *hovered {
                        page.prefetch_gallery_full(id, window, cx);
                    }
                }))
                .on_click(
                    cx.listener(move |page, event: &gpui::ClickEvent, window, cx| {
                        page.on_gallery_item_click(id, event, window, cx);
                    }),
                ),
            id,
            conversation_id,
            super::image_menu::ImageSurface::GalleryTile,
            cx,
        );
        match image {
            Some(image) => frame
                .child(cover_image(image).size_full().rounded(px(10.0)))
                .when_some(full, |frame, full| {
                    frame.child(cover_image(full).absolute().inset_0().rounded(px(10.0)))
                })
                .child(checkbox)
                .into_any_element(),
            None => frame.child(checkbox).into_any_element(),
        }
    }
}

fn gallery_bar_action(
    id: &'static str,
    label: &'static str,
    icon: &'static str,
    theme: &Theme,
    color: gpui::Hsla,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(26.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded(px(7.0))
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::wash(0.10)))
        .child(icons::icon(icon).size(px(13.0)).text_color(color))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(if label == "Delete" {
                    theme.danger
                } else {
                    theme.text
                })
                .child(label),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gallery_fills_width_with_at_least_one_column() {
        assert_eq!(gallery_columns(200.0), 1);
        assert!(gallery_columns(900.0) >= 3);
        assert!(gallery_columns(1600.0) >= 5);
        assert!(gallery_columns(2400.0) <= 16);
    }

    #[test]
    fn gallery_tiles_consume_the_inner_width() {
        let width = 1000.0;
        let columns = gallery_columns(width);
        let tile = gallery_tile_size(width, columns);
        let used = tile * columns as f32 + GALLERY_GAP * (columns.saturating_sub(1) as f32);
        assert!((used - (width - GALLERY_PAD * 2.0)).abs() < 0.02);
    }

    #[test]
    fn gallery_row_count_packs_the_last_row() {
        assert_eq!(gallery_row_count(0, 4), 0);
        assert_eq!(gallery_row_count(4, 4), 1);
        assert_eq!(gallery_row_count(5, 4), 2);
    }

    #[test]
    fn shift_range_covers_both_directions() {
        assert_eq!(select_index_range(10, 2, 5), 2..6);
        assert_eq!(select_index_range(10, 5, 2), 2..6);
        assert_eq!(select_index_range(10, 0, 0), 0..1);
        assert_eq!(select_index_range(0, 0, 0), 0..0);
    }

    #[test]
    fn background_prefetch_walks_outward_on_both_sides() {
        assert_eq!(
            gallery_prefetch_row_order(10, 4..6),
            vec![6, 3, 7, 2, 8, 1, 9, 0]
        );
        assert_eq!(gallery_prefetch_row_order(4, 0..1), vec![1, 2, 3]);
        assert_eq!(gallery_prefetch_row_order(4, 3..4), vec![2, 1, 0]);
    }
}
