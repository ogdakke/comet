//! Routed artifact viewer: full-bleed image strip, filmstrip, and inspector.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, Element, GlobalElementId, Image, ImageFormat,
    InspectorElementId, IntoElement, LayoutId, ObjectFit, PinchEvent, Pixels, Point, Refineable,
    ScrollWheelEvent, SharedString, Style, StyleRefinement, Styled, TouchPhase, Window, canvas,
    div, prelude::*, px,
};
use zeron_proto::{StudioConversationView, StudioGalleryItem};
use zeron_rpc::methods;
use zeron_studio::{MediaKind, StudioArtifactId, StudioConversationId, StudioTurnId};

use crate::state::EngineHandle;
use crate::theme::Theme;

use crate::motion;

use super::StudioEvent;
use super::page::StudioPage;

/// iOS `UIScrollView` paging: settle on a page once the projected rest
/// crosses half a page, or a short flick in that direction.
const ARTIFACT_SWIPE_COMMIT_FRACTION: f32 = 0.5;
/// Horizontal flick (px/s) that turns the page even before halfway.
const ARTIFACT_SWIPE_FLICK: f32 = 500.0;
/// `UIScrollView.DecelerationRate.normal` — per millisecond.
const ARTIFACT_DECEL_RATE: f32 = 0.998;
/// Photos-like settle: ease-out, no overshoot.
const ARTIFACT_SNAP_DURATION: Duration = Duration::from_millis(220);
/// If macOS never sends Ended, snap once the wheel goes idle.
const ARTIFACT_SWIPE_IDLE: Duration = Duration::from_millis(48);
/// Apple rubber-band coefficient (WWDC / UIScrollView).
const ARTIFACT_RUBBER_COEFF: f32 = 0.55;
const ARTIFACT_FILMSTRIP_STEP: f32 = 38.0;
const ARTIFACT_FILMSTRIP_GAP: f32 = 8.0;
const ARTIFACT_FILMSTRIP_SELECTED: f32 = 58.0;
const ARTIFACT_FILMSTRIP_THUMB: f32 = 50.0;
const ARTIFACT_FILMSTRIP_HEIGHT: f32 = 78.0;
const ARTIFACT_FILMSTRIP_WIDTH_FRACTION: f32 = 0.94;
const ARTIFACT_FILMSTRIP_FADE: f32 = 28.0;
const ARTIFACT_ZOOM_MAX: f32 = 24.0;
/// Full-size frames kept around the current filmstrip index.
const LIGHTBOX_PREFETCH: usize = 6;
const INSPECTOR_WIDTH: f32 = 320.0;
const INSPECTOR_PAD_X: f32 = 18.0;
const INSPECTOR_COPY_SIZE: f32 = 24.0;
const INSPECTOR_COPY_GAP: f32 = 8.0;
/// Collapsed inspector prompt: ten rows of 12px sidebar type, then Show more.
const INSPECTOR_PROMPT_COLLAPSED_LINES: usize = 10;
const INSPECTOR_PROMPT_LINE_HEIGHT: f32 = 18.0;
/// Geist 12px Latin advance — scaled from the 14px chat-bubble estimate.
const INSPECTOR_PROMPT_ADVANCE: f32 = super::feed::PROMPT_AVG_CHAR_ADVANCE * (12.0 / 14.0);

/// One frame the artifact viewer can show. Callers build the list;
/// the viewer never asks where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ArtifactFrame {
    pub id: StudioArtifactId,
    pub conversation_id: StudioConversationId,
    pub turn_id: StudioTurnId,
    pub prompt: String,
    pub model_display_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
}

pub(super) fn frames_from_gallery(items: &[StudioGalleryItem]) -> Vec<ArtifactFrame> {
    items
        .iter()
        .map(|item| ArtifactFrame {
            id: item.id,
            conversation_id: item.conversation_id,
            turn_id: item.turn_id,
            prompt: item.prompt.clone(),
            model_display_name: item.model_display_name.clone(),
            mime_type: item.mime_type.clone(),
            size_bytes: item.size_bytes,
        })
        .collect()
}

pub(super) fn frames_from_conversation(view: &StudioConversationView) -> Vec<ArtifactFrame> {
    view.turns
        .iter()
        .flat_map(|turn| {
            turn.runs.iter().flat_map(move |run| {
                run.artifacts
                    .iter()
                    .filter(|artifact| artifact.media_kind == MediaKind::Image)
                    .map(move |artifact| ArtifactFrame {
                        id: artifact.id,
                        conversation_id: view.conversation.id,
                        turn_id: turn.id,
                        prompt: turn.prompt.clone(),
                        model_display_name: run.model.display_name.clone(),
                        mime_type: artifact.mime_type.clone(),
                        size_bytes: artifact.size_bytes,
                    })
            })
        })
        .collect()
}

pub(super) fn lightbox_neighbor_ids(
    frames: &[ArtifactFrame],
    selected: StudioArtifactId,
) -> Vec<StudioArtifactId> {
    let Some(index) = frames.iter().position(|frame| frame.id == selected) else {
        return Vec::new();
    };
    let mut ids = Vec::with_capacity(LIGHTBOX_PREFETCH * 2 + 1);
    ids.push(frames[index].id);
    for step in 1..=LIGHTBOX_PREFETCH {
        if let Some(frame) = frames.get(index + step) {
            ids.push(frame.id);
        }
        if let Some(frame) = index
            .checked_sub(step)
            .and_then(|previous| frames.get(previous))
        {
            ids.push(frame.id);
        }
    }
    ids
}

fn inspector_prompt_inner_width() -> f32 {
    (INSPECTOR_WIDTH - INSPECTOR_PAD_X * 2.0 - INSPECTOR_COPY_SIZE - INSPECTOR_COPY_GAP).max(1.0)
}

fn filmstrip_thumb_size(index: usize, selected: usize) -> f32 {
    if index == selected {
        ARTIFACT_FILMSTRIP_SELECTED
    } else {
        ARTIFACT_FILMSTRIP_THUMB
    }
}

/// Distance from the strip's left edge to the selected thumb's center.
fn filmstrip_selected_center(selected: usize) -> f32 {
    selected as f32 * (ARTIFACT_FILMSTRIP_THUMB + ARTIFACT_FILMSTRIP_GAP)
        + ARTIFACT_FILMSTRIP_SELECTED / 2.0
}

fn filmstrip_content_width(count: usize) -> f32 {
    if count == 0 {
        0.0
    } else {
        ARTIFACT_FILMSTRIP_SELECTED
            + count.saturating_sub(1) as f32 * (ARTIFACT_FILMSTRIP_THUMB + ARTIFACT_FILMSTRIP_GAP)
    }
}

fn filmstrip_viewport_width(stage_width: f32) -> f32 {
    let stage = if stage_width > 1.0 {
        stage_width
    } else {
        1200.0
    };
    stage * ARTIFACT_FILMSTRIP_WIDTH_FRACTION
}

/// Shift that puts the selected thumb on the viewport midline.
fn filmstrip_offset(selected: usize, viewport: f32) -> f32 {
    viewport / 2.0 - filmstrip_selected_center(selected)
}

fn filmstrip_thumb_origin(index: usize, selected: usize) -> f32 {
    index as f32 * (ARTIFACT_FILMSTRIP_THUMB + ARTIFACT_FILMSTRIP_GAP)
        + if index > selected {
            ARTIFACT_FILMSTRIP_SELECTED - ARTIFACT_FILMSTRIP_THUMB
        } else {
            0.0
        }
}

fn filmstrip_visible_range(selected: usize, count: usize, viewport: f32) -> std::ops::Range<usize> {
    if count == 0 {
        return 0..0;
    }
    let slot = ARTIFACT_FILMSTRIP_THUMB + ARTIFACT_FILMSTRIP_GAP;
    let pad = ((viewport / (2.0 * slot)).ceil() as usize).saturating_add(3);
    let start = selected.saturating_sub(pad);
    let end = (selected + pad + 1).min(count);
    start..end
}

/// Neighbor slides only while a swipe is in flight. The resting image fills
/// the live stage so a resize cannot leave it stuck at a stale pixel size.
fn lightbox_uses_paging_slides(
    zoomed: bool,
    page_width: f32,
    swipe_x: f32,
    snapping: bool,
) -> bool {
    !zoomed && page_width > 1.0 && (swipe_x.abs() > 0.5 || snapping)
}

fn lightbox_stage_size_changed(
    current_width: f32,
    current_height: f32,
    width: f32,
    height: f32,
) -> bool {
    (current_width - width).abs() > 0.5 || (current_height - height).abs() > 0.5
}

/// Paint an image into the box we asked for. `img` stamps the photo's aspect
/// ratio onto its layout box, so a portrait grows out of a square thumb and
/// a 4K frame blows past the lightbox. This element never takes an aspect
/// ratio: it lays out as the parent and paints Cover/Contain into those bounds.
pub(super) fn cover_image(image: Arc<Image>) -> FittedImage {
    fitted_image(image, ObjectFit::Cover)
}

pub(super) fn contain_image(image: Arc<Image>) -> FittedImage {
    fitted_image(image, ObjectFit::Contain)
}

fn fitted_image(image: Arc<Image>, fit: ObjectFit) -> FittedImage {
    FittedImage {
        image,
        fit,
        style: StyleRefinement::default(),
    }
}

pub(super) struct FittedImage {
    image: Arc<Image>,
    fit: ObjectFit,
    style: StyleRefinement,
}

impl Styled for FittedImage {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Element for FittedImage {
    type RequestLayoutState = Style;
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Style) {
        let _ = self.image.clone().use_render_image(window, cx);
        let mut style = Style::default();
        style.refine(&self.style);
        let layout_id = window.request_layout(style.clone(), [], cx);
        (layout_id, style)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Style,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        style: &mut Style,
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(data) = self.image.clone().use_render_image(window, cx) else {
            return;
        };
        if data.frame_count() == 0 {
            return;
        }
        let fitted = self.fit.get_bounds(bounds, data.size(0));
        let visible = bounds.intersect(&fitted);
        if visible.size.width <= px(0.0) || visible.size.height <= px(0.0) {
            return;
        }
        let corner_radii = style
            .corner_radii
            .to_pixels(window.rem_size())
            .clamp_radii_for_quad_size(visible.size);
        window
            .paint_image_fitted(visible, fitted, corner_radii, data, 0, false)
            .ok();
    }
}

impl IntoElement for FittedImage {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Ease from `from` to `to` after fingers lift. `to` is 0, +page, or −page.
#[derive(Debug, Clone, Copy)]
pub(super) struct LightboxSnap {
    from: f32,
    to: f32,
    started: Instant,
}

fn stepped_artifact_index(index: usize, len: usize, delta: isize, wraps: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if wraps {
        (index as isize + delta).rem_euclid(len as isize) as usize
    } else {
        (index as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize
    }
}

/// WWDC 2018 "Designing Fluid Interfaces" projection: where a flick would
/// rest if it kept `UIScrollView` deceleration. `velocity` is px/s.
fn project_scroll(velocity: f32, rate: f32) -> f32 {
    if !(0.0..1.0).contains(&rate) {
        return 0.0;
    }
    (velocity / 1000.0) * rate / (1.0 - rate)
}

/// Apple rubber-band: `f(x) = (1 - 1/((x * c / d) + 1)) * d`.
fn rubber_band(offset: f32, dimension: f32) -> f32 {
    let dimension = dimension.max(1.0);
    let x = offset.abs();
    let limited = (1.0 - (1.0 / ((x * ARTIFACT_RUBBER_COEFF / dimension) + 1.0))) * dimension;
    limited.copysign(offset)
}

/// Page to settle on: 0 stays, −width next, +width previous.
fn lightbox_paging_target(
    offset: f32,
    velocity: f32,
    width: f32,
    can_prev: bool,
    can_next: bool,
) -> f32 {
    let width = width.max(1.0);
    let projected = offset + project_scroll(velocity, ARTIFACT_DECEL_RATE);
    let halfway = width * ARTIFACT_SWIPE_COMMIT_FRACTION;
    let mut target = if projected <= -halfway {
        -width
    } else if projected >= halfway {
        width
    } else {
        0.0
    };
    if target == 0.0 {
        if can_next && offset < 0.0 && velocity <= -ARTIFACT_SWIPE_FLICK {
            target = -width;
        } else if can_prev && offset > 0.0 && velocity >= ARTIFACT_SWIPE_FLICK {
            target = width;
        }
    }
    if target < 0.0 && !can_next {
        0.0
    } else if target > 0.0 && !can_prev {
        0.0
    } else {
        target
    }
}

fn apply_lightbox_swipe_delta(
    offset: f32,
    delta: f32,
    width: f32,
    can_prev: bool,
    can_next: bool,
) -> f32 {
    let width = width.max(1.0);
    let proposed = offset + delta;
    if proposed > 0.0 {
        if can_prev {
            proposed.min(width)
        } else if proposed > 0.0 {
            rubber_band(proposed, width)
        } else {
            0.0
        }
    } else if proposed < 0.0 {
        if can_next {
            proposed.max(-width)
        } else {
            rubber_band(proposed, width)
        }
    } else {
        0.0
    }
}

/// How far a zoomed image may travel. `stage * zoom / 2` lets any edge
/// reach the stage center — enough to clear the overlaid titlebar/filmstrip
/// on a 9:16 that fills the stage height.
fn lightbox_pan_range(stage: f32, zoom: f32) -> f32 {
    if zoom <= 1.001 {
        0.0
    } else {
        stage.max(1.0) * zoom / 2.0
    }
}

fn snap_offset_at(from: f32, to: f32, elapsed: f32, duration: f32) -> f32 {
    let t = if duration <= f32::EPSILON {
        1.0
    } else {
        (elapsed / duration).clamp(0.0, 1.0)
    };
    from + (to - from) * motion::EASE_OUT.eval(t)
}

pub(super) fn write_artifact_file(destination: PathBuf, bytes: Vec<u8>) -> Result<(), String> {
    std::fs::write(destination, bytes).map_err(|error| error.to_string())
}

/// Minimum short edge retained for a gallery preview. The old longest-edge
/// thumbnail left only ~288 source pixels across the short edge of a common
/// 16:9 image, then enlarged that crop into a ~250px Retina tile.
const GALLERY_THUMB_SHORT_EDGE: u32 = 640;
/// Bound panoramas and unusually tall images without changing their aspect.
const GALLERY_THUMB_LONG_EDGE: u32 = 1920;

fn gallery_thumb_dimensions(width: u32, height: u32) -> (u32, u32) {
    let short = width.min(height);
    let long = width.max(height);
    if short <= GALLERY_THUMB_SHORT_EDGE && long <= GALLERY_THUMB_LONG_EDGE {
        return (width, height);
    }
    let scale_for_short = GALLERY_THUMB_SHORT_EDGE as f64 / short.max(1) as f64;
    let scale_for_long = GALLERY_THUMB_LONG_EDGE as f64 / long.max(1) as f64;
    let scale = scale_for_short.min(scale_for_long).min(1.0);
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

pub(super) fn downsample_gallery_thumb(bytes: Vec<u8>) -> Result<Arc<Image>, String> {
    let image = image::load_from_memory(&bytes).map_err(|error| error.to_string())?;
    // Preserve aspect because conversation tiles use Contain while gallery
    // and filmstrip tiles use Cover. Sizing from the short edge gives cover
    // crops a 2×+ pixel buffer without retaining the original frame.
    let (width, height) = gallery_thumb_dimensions(image.width(), image.height());
    let thumb = image.resize_exact(width, height, image::imageops::FilterType::Triangle);
    let mut encoded = std::io::Cursor::new(Vec::new());
    thumb
        .write_to(&mut encoded, image::ImageFormat::Jpeg)
        .map_err(|error| error.to_string())?;
    Ok(Arc::new(Image::from_bytes(
        ImageFormat::Jpeg,
        encoded.into_inner(),
    )))
}

pub(super) async fn read_artifact_bytes(
    engine: &EngineHandle,
    artifact_id: StudioArtifactId,
) -> Result<(String, String, Vec<u8>), String> {
    let mut bytes = Vec::new();
    let mut offset = 0u64;
    for _ in 0..4096 {
        let value = engine
            .client()
            .call(
                methods::READ_STUDIO_ARTIFACT_CHUNK,
                serde_json::json!({ "artifactId": artifact_id, "offset": offset }),
            )
            .await
            .map_err(|error| error.to_string())?;
        let chunk: zeron_proto::StudioArtifactChunk =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        let mime = chunk.mime_type;
        bytes.extend(
            base64::engine::general_purpose::STANDARD
                .decode(chunk.data)
                .map_err(|error| error.to_string())?,
        );
        if chunk.done {
            return Ok((chunk.file_name, mime, bytes));
        }
        if chunk.next_offset <= offset {
            return Err("artifact read stopped advancing".into());
        }
        offset = chunk.next_offset;
    }
    Err("artifact exceeded the chunk limit".into())
}

impl StudioPage {
    pub(super) fn delete_artifact(
        &mut self,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        self.delete_artifacts(vec![artifact_id], cx);
    }

    pub(super) fn download_artifact(
        &mut self,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let suggested = self.artifact_file_name(artifact_id);
        let receiver = cx.prompt_for_new_path(&PathBuf::new(), Some(&suggested));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let destination = match receiver.await {
                Ok(Ok(Some(path))) => path,
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
            let result = match read_artifact_bytes(&engine, artifact_id).await {
                Ok((_, _, bytes)) => {
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
            }
        }));
    }

    pub(super) fn artifact_sequence(&self) -> Vec<StudioArtifactId> {
        self.lightbox_frames.iter().map(|frame| frame.id).collect()
    }

    pub(super) fn artifact_frame(&self, artifact_id: StudioArtifactId) -> Option<&ArtifactFrame> {
        self.lightbox_frames
            .iter()
            .find(|frame| frame.id == artifact_id)
    }

    pub(super) fn artifact_conversation(
        &self,
        artifact_id: StudioArtifactId,
    ) -> Option<StudioConversationId> {
        self.artifact_frame(artifact_id)
            .map(|frame| frame.conversation_id)
    }

    /// Open the viewer on `id` inside `frames`. The filmstrip, arrows, and
    /// prefetch all walk this list and nothing else.
    pub(super) fn open_artifact_viewer(
        &mut self,
        id: StudioArtifactId,
        frames: Vec<ArtifactFrame>,
        cx: &mut Context<Self>,
    ) {
        self.close_image_menu(cx);
        self.lightbox_frames = frames;
        if let Some(index) = self.lightbox_frames.iter().position(|frame| frame.id == id) {
            self.select_artifact_index(index, cx);
            return;
        }
        if self.selected_artifact.take().is_some() {
            self.reset_lightbox_viewer();
            cx.emit(StudioEvent::CloseArtifact);
        }
        cx.notify();
    }

    fn surface_artifact_frames(&self) -> Vec<ArtifactFrame> {
        if let Some(view) = &self.conversation {
            frames_from_conversation(view)
        } else {
            frames_from_gallery(&self.gallery)
        }
    }

    pub(super) fn reset_lightbox_swipe(&mut self) {
        self.lightbox_swipe_x = 0.0;
        self.lightbox_swipe_velocity = 0.0;
        self.lightbox_snap = None;
        self.lightbox_swipe_last_tick = None;
    }

    pub(super) fn lightbox_motion_pending(&self) -> bool {
        self.lightbox_snap.is_some()
            || (self.lightbox_swipe_x.abs() > 0.5
                && self
                    .lightbox_swipe_last_tick
                    .is_some_and(|last| last.elapsed() >= ARTIFACT_SWIPE_IDLE))
    }

    pub(super) fn reset_lightbox_viewer(&mut self) {
        self.lightbox_zoom = 1.0;
        self.lightbox_pan = Point::default();
        self.lightbox_drag = None;
        self.reset_lightbox_swipe();
        self.lightbox_swipe_locked = false;
    }

    pub(super) fn adopt_artifact_index(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let artifacts = self.artifact_sequence();
        let Some(artifact_id) = artifacts.get(index).copied() else {
            return false;
        };
        let changed = self.selected_artifact != Some(artifact_id);
        self.selected_artifact = Some(artifact_id);
        self.lightbox_zoom = 1.0;
        self.lightbox_pan = Point::default();
        self.lightbox_drag = None;
        if changed {
            self.inspector_scroll.set_offset(Point::default());
        }
        if let Some(conversation_id) = self.artifact_conversation(artifact_id) {
            cx.emit(StudioEvent::OpenArtifact {
                conversation_id,
                artifact_id,
            });
        }
        if changed {
            self.request_visible_gallery_images(cx);
        }
        cx.notify();
        changed
    }

    pub(super) fn visible_filmstrip_ids(&self) -> Vec<StudioArtifactId> {
        let Some(selected) = self.selected_artifact else {
            return Vec::new();
        };
        let sequence = self.artifact_sequence();
        let Some(index) = sequence.iter().position(|id| *id == selected) else {
            return Vec::new();
        };
        let range = filmstrip_visible_range(
            index,
            sequence.len(),
            filmstrip_viewport_width(self.lightbox_stage_width),
        );
        sequence.get(range).unwrap_or(&[]).to_vec()
    }

    pub(super) fn select_artifact_index(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let changed = self.adopt_artifact_index(index, cx);
        self.reset_lightbox_swipe();
        self.lightbox_swipe_locked = false;
        changed
    }

    pub(super) fn navigate_artifact_with_wrap(
        &mut self,
        delta: isize,
        wraps: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let artifacts = self.artifact_sequence();
        let Some(selected) = self.selected_artifact else {
            return false;
        };
        let Some(index) = artifacts.iter().position(|id| *id == selected) else {
            return false;
        };
        let next = stepped_artifact_index(index, artifacts.len(), delta, wraps);
        if next == index {
            return false;
        }
        self.select_artifact_index(next, cx)
    }

    pub(super) fn navigate_artifact(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.navigate_artifact_with_wrap(delta, true, cx);
    }

    pub(super) fn select_artifact_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        let artifacts = self.artifact_sequence();
        let index = if last {
            artifacts.len().saturating_sub(1)
        } else {
            0
        };
        self.select_artifact_index(index, cx);
    }

    pub fn show_artifact(
        &mut self,
        conversation_id: StudioConversationId,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        if self.artifact_frame(artifact_id).is_some() {
            if self.selected_artifact != Some(artifact_id) {
                if let Some(index) = self
                    .lightbox_frames
                    .iter()
                    .position(|frame| frame.id == artifact_id)
                {
                    self.adopt_artifact_index(index, cx);
                }
            }
            return;
        }
        let frames = self.surface_artifact_frames();
        if frames.iter().any(|frame| frame.id == artifact_id) {
            self.open_artifact_viewer(artifact_id, frames, cx);
            return;
        }
        if self.selected_conversation != Some(conversation_id) {
            self.open_conversation(conversation_id, cx);
        }
    }

    pub fn close_artifact(&mut self, cx: &mut Context<Self>) {
        self.close_image_menu(cx);
        if self.selected_artifact.take().is_some() {
            self.lightbox_frames.clear();
            self.reset_lightbox_viewer();
            self.request_visible_gallery_images(cx);
            cx.notify();
        }
    }

    pub(super) fn lightbox_page_width(&self) -> f32 {
        self.lightbox_stage_width.max(1.0)
    }

    fn lightbox_stage_size(&self) -> (f32, f32) {
        (
            if self.lightbox_stage_width > 1.0 {
                self.lightbox_stage_width
            } else {
                1200.0
            },
            if self.lightbox_stage_height > 1.0 {
                self.lightbox_stage_height
            } else {
                800.0
            },
        )
    }

    fn clamp_lightbox_pan(&mut self) {
        let (limit_x, limit_y) = self.lightbox_pan_limits();
        self.lightbox_pan.x = px(f32::from(self.lightbox_pan.x).clamp(-limit_x, limit_x));
        self.lightbox_pan.y = px(f32::from(self.lightbox_pan.y).clamp(-limit_y, limit_y));
    }

    fn lightbox_pan_limits(&self) -> (f32, f32) {
        let (width, height) = self.lightbox_stage_size();
        (
            lightbox_pan_range(width, self.lightbox_zoom),
            lightbox_pan_range(height, self.lightbox_zoom),
        )
    }

    pub(super) fn adjust_lightbox_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        self.lightbox_zoom = (self.lightbox_zoom * factor).clamp(1.0, ARTIFACT_ZOOM_MAX);
        if self.lightbox_zoom > 1.001 {
            self.reset_lightbox_swipe();
        }
        self.clamp_lightbox_pan();
        cx.notify();
    }

    pub(super) fn fit_lightbox(&mut self, cx: &mut Context<Self>) {
        self.lightbox_zoom = 1.0;
        self.lightbox_pan = Point::default();
        cx.notify();
    }

    pub(super) fn begin_lightbox_pan(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.lightbox_zoom > 1.0 {
            self.lightbox_drag = Some(position);
            cx.notify();
        }
    }

    pub(super) fn update_lightbox_pan(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(previous) = self.lightbox_drag else {
            return;
        };
        self.pan_lightbox(
            f32::from(position.x - previous.x),
            f32::from(position.y - previous.y),
            cx,
        );
        self.lightbox_drag = Some(position);
    }

    pub(super) fn end_lightbox_pan(&mut self, cx: &mut Context<Self>) {
        if self.lightbox_drag.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn finish_lightbox_snap_immediate(&mut self, cx: &mut Context<Self>) {
        let target = self.lightbox_snap.map(|snap| snap.to).unwrap_or(0.0);
        self.commit_lightbox_snap_target(target, cx);
    }

    fn commit_lightbox_snap_target(&mut self, target: f32, cx: &mut Context<Self>) {
        if target.abs() > 1.0 {
            let artifacts = self.artifact_sequence();
            let index = self
                .selected_artifact
                .and_then(|selected| artifacts.iter().position(|id| *id == selected))
                .unwrap_or(0);
            let delta = if target < 0.0 { 1 } else { -1 };
            let next = stepped_artifact_index(index, artifacts.len(), delta, false);
            if next != index {
                self.adopt_artifact_index(next, cx);
            }
        }
        self.reset_lightbox_swipe();
        cx.notify();
    }

    pub(super) fn step_lightbox_motion(&mut self, cx: &mut Context<Self>) {
        if self.lightbox_snap.is_some() {
            let snap = self.lightbox_snap.unwrap();
            let duration = ARTIFACT_SNAP_DURATION.as_secs_f32() * motion::speed_scale();
            let elapsed = snap.started.elapsed().as_secs_f32();
            self.lightbox_swipe_x = snap_offset_at(snap.from, snap.to, elapsed, duration);
            if elapsed >= duration {
                self.commit_lightbox_snap_target(snap.to, cx);
                return;
            }
            cx.notify();
            return;
        }
        if self.lightbox_swipe_x.abs() > 0.5
            && self
                .lightbox_swipe_last_tick
                .is_some_and(|last| last.elapsed() >= ARTIFACT_SWIPE_IDLE)
        {
            self.finish_lightbox_swipe(cx);
        }
    }

    pub(super) fn finish_lightbox_swipe(&mut self, cx: &mut Context<Self>) {
        let width = self.lightbox_page_width();
        let offset = self.lightbox_swipe_x;
        let artifacts = self.artifact_sequence();
        let index = self
            .selected_artifact
            .and_then(|selected| artifacts.iter().position(|id| *id == selected))
            .unwrap_or(0);
        let target = lightbox_paging_target(
            offset,
            self.lightbox_swipe_velocity,
            width,
            index > 0,
            index + 1 < artifacts.len(),
        );
        self.lightbox_swipe_locked = true;
        if crate::motion::reduced_motion(cx) || (target - offset).abs() < 0.5 {
            self.commit_lightbox_snap_target(target, cx);
            return;
        }
        self.lightbox_snap = Some(LightboxSnap {
            from: offset,
            to: target,
            started: Instant::now(),
        });
        self.lightbox_swipe_velocity = 0.0;
        cx.notify();
    }

    pub(super) fn on_lightbox_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(16.0));
        let horizontal = f32::from(delta.x);
        let vertical = f32::from(delta.y);
        if event.touch_phase == TouchPhase::Started {
            self.lightbox_swipe_locked = false;
            self.lightbox_snap = None;
            self.lightbox_swipe_last_tick = None;
        }
        if event.modifiers.platform {
            let movement = if vertical.abs() >= horizontal.abs() {
                vertical
            } else {
                horizontal
            };
            if movement.abs() > f32::EPSILON {
                self.adjust_lightbox_zoom((movement * 0.01).exp(), cx);
                if self.lightbox_zoom <= 1.001 {
                    self.fit_lightbox(cx);
                }
            }
            cx.stop_propagation();
            return;
        }
        if self.lightbox_swipe_locked {
            cx.stop_propagation();
            return;
        }
        if self.lightbox_drag.is_some() {
            cx.stop_propagation();
            return;
        }
        if matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled)
            && self.lightbox_zoom <= 1.001
            && self.lightbox_swipe_x.abs() > f32::EPSILON
        {
            self.finish_lightbox_swipe(cx);
            cx.stop_propagation();
            return;
        }
        if self.lightbox_zoom > 1.001 {
            self.pan_lightbox(horizontal, vertical, cx);
            cx.stop_propagation();
            return;
        }
        if horizontal.abs() < f32::EPSILON {
            cx.stop_propagation();
            return;
        }
        if !event.delta.precise() {
            self.lightbox_swipe_locked = false;
            let direction = if horizontal < 0.0 { 1 } else { -1 };
            self.navigate_artifact_with_wrap(direction, false, cx);
            cx.stop_propagation();
            return;
        }
        let now = Instant::now();
        let dt = self
            .lightbox_swipe_last_tick
            .map(|last| now.duration_since(last).as_secs_f32())
            .unwrap_or(1.0 / 60.0)
            .clamp(1.0 / 240.0, 1.0 / 20.0);
        self.lightbox_swipe_last_tick = Some(now);
        let artifacts = self.artifact_sequence();
        let index = self
            .selected_artifact
            .and_then(|selected| artifacts.iter().position(|id| *id == selected))
            .unwrap_or(0);
        self.lightbox_swipe_x = apply_lightbox_swipe_delta(
            self.lightbox_swipe_x,
            horizontal,
            self.lightbox_page_width(),
            index > 0,
            index + 1 < artifacts.len(),
        );
        let sample = horizontal / dt;
        self.lightbox_swipe_velocity = self.lightbox_swipe_velocity * 0.35 + sample * 0.65;
        if matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.finish_lightbox_swipe(cx);
        } else {
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn pan_lightbox(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        self.lightbox_pan.x = px(f32::from(self.lightbox_pan.x) + dx);
        self.lightbox_pan.y = px(f32::from(self.lightbox_pan.y) + dy);
        self.clamp_lightbox_pan();
        cx.notify();
    }

    pub(super) fn on_lightbox_pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        self.adjust_lightbox_zoom((1.0 + event.delta).max(0.05), cx);
        if matches!(event.phase, TouchPhase::Ended | TouchPhase::Cancelled)
            && self.lightbox_zoom <= 1.01
        {
            self.fit_lightbox(cx);
        }
        cx.stop_propagation();
    }

    pub(super) fn on_filmstrip_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(16.0));
        let movement = if f32::from(delta.x).abs() > f32::EPSILON {
            f32::from(delta.x)
        } else {
            f32::from(delta.y)
        };
        if event.touch_phase == TouchPhase::Started {
            self.filmstrip_scroll_accum = 0.0;
        }
        self.filmstrip_scroll_accum += movement;
        while self.filmstrip_scroll_accum.abs() >= ARTIFACT_FILMSTRIP_STEP {
            let direction = if self.filmstrip_scroll_accum > 0.0 {
                -1
            } else {
                1
            };
            if !self.navigate_artifact_with_wrap(direction, false, cx) {
                self.filmstrip_scroll_accum *= 0.25;
                break;
            }
            self.filmstrip_scroll_accum -=
                self.filmstrip_scroll_accum.signum() * ARTIFACT_FILMSTRIP_STEP;
        }
        if matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.filmstrip_scroll_accum = 0.0;
        }
        cx.stop_propagation();
    }

    /// Thumb (or full, if that is all we have) plus a GPU-ready full overlay.
    /// The base stays mounted so swapping in the full frame cannot remount.
    pub(super) fn display_layers(
        &self,
        id: StudioArtifactId,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (Option<Arc<Image>>, Option<Arc<Image>>) {
        let base = self.images.get_thumb(&id);
        if let Some(base) = base.as_ref() {
            let _ = base.clone().use_render_image(window, cx);
        }
        let overlay = self.images.get_full(&id).and_then(|full| {
            let same_as_base = base.as_ref().is_some_and(|base| Arc::ptr_eq(base, &full));
            if same_as_base {
                return None;
            }
            full.clone().use_render_image(window, cx).map(|_| full)
        });
        (base, overlay)
    }

    fn warm_lightbox_neighbors(&self, window: &mut Window, cx: &mut gpui::App) {
        let Some(selected) = self.selected_artifact else {
            return;
        };
        for id in lightbox_neighbor_ids(&self.lightbox_frames, selected) {
            if let Some(full) = self.images.get_full(&id) {
                let _ = full.use_render_image(window, cx);
            }
            if let Some(thumb) = self.images.get_thumb_only(&id) {
                let _ = thumb.use_render_image(window, cx);
            }
        }
    }

    pub(super) fn render_lightbox_slide(
        &self,
        artifact_id: Option<StudioArtifactId>,
        page: Option<(f32, f32)>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (base, overlay) = artifact_id
            .map(|id| self.display_layers(id, window, cx))
            .unwrap_or((None, None));
        let slide_id = match artifact_id {
            Some(id) => SharedString::from(format!("studio-artifact-slide-{}", id.0)),
            None => SharedString::from("studio-artifact-slide-empty"),
        };
        let frame = if let Some((left, width)) = page {
            div()
                .id(slide_id)
                .absolute()
                .top_0()
                .bottom_0()
                .w(px(width))
                .left(px(left))
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
        } else {
            // Do not combine inset_0 with left/top: that pins the far edges
            // and shrinks the box when you pan down, so the top of a tall
            // image is unreachable.
            div()
                .id(slide_id)
                .absolute()
                .inset_0()
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
        };
        let frame = if let Some(id) = artifact_id
            && let Some(conversation_id) = self.artifact_menu_conversation(id)
        {
            self.bind_image_menu(
                frame,
                id,
                conversation_id,
                self.artifact_image_surface(),
                cx,
            )
        } else {
            frame
        };
        let zoom = if page.is_some() {
            1.0
        } else {
            self.lightbox_zoom
        };
        let measured = self.lightbox_stage_width > 1.0 && self.lightbox_stage_height > 1.0;
        let zoomed = page.is_none() && zoom > 1.001;
        let stack = |base: Arc<Image>, overlay: Option<Arc<Image>>| {
            let mut layer = div().relative().overflow_hidden();
            layer = if !zoomed {
                layer.size_full()
            } else if measured {
                layer
                    .flex_none()
                    .w(px(self.lightbox_stage_width * zoom))
                    .h(px(self.lightbox_stage_height * zoom))
                    .left(self.lightbox_pan.x)
                    .top(self.lightbox_pan.y)
            } else {
                layer
                    .flex_none()
                    .w(gpui::relative(zoom))
                    .h(gpui::relative(zoom))
                    .left(self.lightbox_pan.x)
                    .top(self.lightbox_pan.y)
            };
            layer
                .child(contain_image(base).size_full())
                .when_some(overlay, |layer, overlay| {
                    layer.child(contain_image(overlay).absolute().inset_0())
                })
        };
        match base {
            Some(base) => frame.child(stack(base, overlay)).into_any_element(),
            None => frame
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child("Loading image…"),
                )
                .into_any_element(),
        }
    }

    pub(super) fn render_artifact_page(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let id = self.selected_artifact?;
        self.warm_lightbox_neighbors(window, cx);
        let sequence = self.artifact_sequence();
        let selected_index = sequence
            .iter()
            .position(|candidate| *candidate == id)
            .unwrap_or(0);
        let details = self.artifact_frame(id).map(|frame| {
            (
                frame.turn_id,
                frame.prompt.clone(),
                frame.model_display_name.clone(),
                frame.mime_type.clone(),
                frame.size_bytes,
            )
        });
        let filmstrip_viewport = filmstrip_viewport_width(self.lightbox_stage_width);
        let filmstrip_range =
            filmstrip_visible_range(selected_index, sequence.len(), filmstrip_viewport);
        let thumbnails = filmstrip_range
            .filter_map(|index| {
                sequence
                    .get(index)
                    .copied()
                    .map(|artifact_id| (index, artifact_id))
            })
            .map(|(index, artifact_id)| {
                let thumbnail = self.images.get_thumb(&artifact_id);
                let frame_size = filmstrip_thumb_size(index, selected_index);
                let origin = filmstrip_thumb_origin(index, selected_index)
                    + filmstrip_offset(selected_index, filmstrip_viewport);
                let border = if index == selected_index {
                    theme.text_muted
                } else {
                    theme.border
                };
                let frame = div()
                    .id(SharedString::from(format!("studio-thumbnail-{index}")))
                    .absolute()
                    .left(px(origin))
                    .top(px((ARTIFACT_FILMSTRIP_HEIGHT - frame_size) / 2.0))
                    .size(px(frame_size))
                    .min_w(px(frame_size))
                    .max_w(px(frame_size))
                    .min_h(px(frame_size))
                    .max_h(px(frame_size))
                    .flex_none()
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(border)
                    .bg(crate::theme::wash(0.04))
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.82))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.select_artifact_index(index, cx);
                    }));
                let frame = match self.artifact_menu_conversation(artifact_id) {
                    Some(conversation_id) => self.bind_image_menu(
                        frame,
                        artifact_id,
                        conversation_id,
                        super::image_menu::ImageSurface::Filmstrip,
                        cx,
                    ),
                    None => frame,
                };
                match thumbnail {
                    Some(thumbnail) => frame
                        .child(cover_image(thumbnail).size_full().rounded(px(7.0)))
                        .into_any_element(),
                    None => frame.into_any_element(),
                }
            })
            .collect::<Vec<_>>();
        let filmstrip_x = filmstrip_offset(selected_index, filmstrip_viewport);
        let filmstrip_span = filmstrip_content_width(sequence.len());
        let fade_left = filmstrip_x < -0.5;
        let fade_right = filmstrip_x + filmstrip_span > filmstrip_viewport + 0.5;

        let zoomed = self.lightbox_zoom > 1.001;
        let page_width = self.lightbox_stage_width;
        let mut slides = Vec::new();
        if !lightbox_uses_paging_slides(
            zoomed,
            page_width,
            self.lightbox_swipe_x,
            self.lightbox_snap.is_some(),
        ) {
            slides.push(self.render_lightbox_slide(Some(id), None, theme, window, cx));
        } else {
            if selected_index > 0 {
                slides.push(self.render_lightbox_slide(
                    Some(sequence[selected_index - 1]),
                    Some((self.lightbox_swipe_x - page_width, page_width)),
                    theme,
                    window,
                    cx,
                ));
            }
            slides.push(self.render_lightbox_slide(
                Some(id),
                Some((self.lightbox_swipe_x, page_width)),
                theme,
                window,
                cx,
            ));
            if let Some(next_id) = sequence.get(selected_index + 1).copied() {
                slides.push(self.render_lightbox_slide(
                    Some(next_id),
                    Some((self.lightbox_swipe_x + page_width, page_width)),
                    theme,
                    window,
                    cx,
                ));
            }
        }
        let measure_entity = cx.weak_entity();
        let stage = div()
            .id("studio-artifact-stage")
            .absolute()
            .inset_0()
            .overflow_hidden()
            .cursor_pointer()
            .on_scroll_wheel(cx.listener(|page, event, _, cx| page.on_lightbox_scroll(event, cx)))
            .on_pinch(cx.listener(|page, event, _, cx| page.on_lightbox_pinch(event, cx)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|page, event: &gpui::MouseDownEvent, _, cx| {
                    page.begin_lightbox_pan(event.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|page, event: &gpui::MouseMoveEvent, _, cx| {
                if event.dragging() {
                    page.update_lightbox_pan(event.position, cx);
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|page, _, _, cx| page.end_lightbox_pan(cx)),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|page, _, _, cx| page.end_lightbox_pan(cx)),
            )
            .on_click(cx.listener(|page, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    if page.lightbox_zoom > 1.0 {
                        page.fit_lightbox(cx);
                    } else {
                        page.adjust_lightbox_zoom(2.0, cx);
                    }
                }
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let width = f32::from(bounds.size.width);
                        let height = f32::from(bounds.size.height);
                        let changed = measure_entity
                            .update(cx, |page, _| {
                                if !lightbox_stage_size_changed(
                                    page.lightbox_stage_width,
                                    page.lightbox_stage_height,
                                    width,
                                    height,
                                ) {
                                    return false;
                                }
                                page.lightbox_stage_width = width;
                                page.lightbox_stage_height = height;
                                page.clamp_lightbox_pan();
                                true
                            })
                            .unwrap_or(false);
                        if changed {
                            // `notify` during prepaint does not mark the window
                            // dirty (draw is already in progress), so the new
                            // size would sit unused until the next click/cycle.
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .children(slides);

        let back_button = div()
            .id("studio-artifact-back")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .occlude()
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.14)))
            .on_click(cx.listener(|page, _, _, cx| {
                page.selected_artifact = None;
                cx.emit(StudioEvent::CloseArtifact);
                cx.notify();
            }))
            .child(
                crate::icons::icon(crate::icons::ARROW_LEFT)
                    .size(px(14.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            );

        let previous = div()
            .id("studio-artifact-previous")
            .size(px(32.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(7.0))
            .occlude()
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.11)))
            .on_click(cx.listener(|page, _, _, cx| page.navigate_artifact(-1, cx)))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_LEFT)
                    .size(px(18.0))
                    .text_color(theme.text_muted),
            );
        let next = div()
            .id("studio-artifact-next")
            .size(px(32.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(7.0))
            .occlude()
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.11)))
            .on_click(cx.listener(|page, _, _, cx| page.navigate_artifact(1, cx)))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_RIGHT)
                    .size(px(18.0))
                    .text_color(theme.text_muted),
            );

        let inspector = div()
            .id("studio-artifact-inspector")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.inspector_scroll)
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.glass_overlay())
            .px(px(INSPECTOR_PAD_X))
            .pt(px(Theme::TITLEBAR_HEIGHT + 18.0))
            .pb(px(16.0))
            .when_some(
                details,
                |inspector, (turn_id, prompt, model, mime, size)| {
                    let copy_prompt = prompt.clone();
                    let expanded = self.expanded_inspector_prompts.contains(&turn_id);
                    let clampable = super::feed::prompt_exceeds_lines(
                        &prompt,
                        inspector_prompt_inner_width(),
                        INSPECTOR_PROMPT_ADVANCE,
                        INSPECTOR_PROMPT_COLLAPSED_LINES,
                    );
                    let collapsed = clampable && !expanded;
                    inspector
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .items_start()
                                .gap(px(INSPECTOR_COPY_GAP))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .gap(px(6.0))
                                        .child(
                                            div()
                                                .w_full()
                                                .text_size(px(12.0))
                                                .line_height(px(INSPECTOR_PROMPT_LINE_HEIGHT))
                                                .text_color(theme.text)
                                                .when(collapsed, |box_| {
                                                    box_.max_h(px(INSPECTOR_PROMPT_LINE_HEIGHT
                                                        * INSPECTOR_PROMPT_COLLAPSED_LINES as f32))
                                                        .overflow_hidden()
                                                })
                                                .child(SharedString::from(prompt)),
                                        )
                                        .when(clampable, |col| {
                                            col.child(
                                                super::feed::show_more_action(
                                                    format!(
                                                        "studio-inspector-toggle-prompt-{}",
                                                        turn_id.0
                                                    ),
                                                    expanded,
                                                    theme,
                                                )
                                                .on_click(cx.listener(move |page, _, _, cx| {
                                                    page.toggle_inspector_prompt_expanded(
                                                        turn_id, cx,
                                                    );
                                                })),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .id("studio-copy-prompt")
                                        .size(px(INSPECTOR_COPY_SIZE))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(6.0))
                                        .cursor_pointer()
                                        .hover(|style| style.bg(crate::theme::wash(0.14)))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_prompt.clone(),
                                            ));
                                        })
                                        .child(
                                            crate::icons::icon(crate::icons::COPY)
                                                .size(px(14.0))
                                                .text_color(theme.text_muted.opacity(0.7)),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex_none()
                                .mt(px(14.0))
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(format!(
                                    "{model} · {mime} · {:.1} KB",
                                    size as f64 / 1024.0
                                ))),
                        )
                },
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("studio-download-artifact")
                            .h(px(32.0))
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(px(7.0))
                            .rounded(px(7.0))
                            .border_1()
                            .border_color(theme.border)
                            .cursor_pointer()
                            .hover(|style| style.bg(crate::theme::wash(0.09)))
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.download_artifact(id, cx)),
                            )
                            .child(
                                crate::icons::icon(crate::icons::ARROW_DOWN)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child("Download"),
                    )
                    .child(
                        div()
                            .id("studio-delete-artifact")
                            .h(px(32.0))
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(px(7.0))
                            .rounded(px(7.0))
                            .cursor_pointer()
                            .text_color(theme.danger)
                            .hover(|style| style.bg(theme.danger.opacity(0.08)))
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.delete_artifact(id, cx)),
                            )
                            .child(
                                crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.danger),
                            )
                            .child("Delete"),
                    ),
            );

        Some(
            div()
                .size_full()
                .flex()
                .min_w_0()
                .track_focus(&self.focus)
                .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, _, cx| {
                    if page.dismiss_image_menu(event, cx) {
                        cx.stop_propagation();
                        return;
                    }
                    match event.keystroke.key.as_str() {
                        "escape" => {
                            page.selected_artifact = None;
                            cx.emit(StudioEvent::CloseArtifact);
                            cx.notify();
                        }
                        "left" => page.navigate_artifact(-1, cx),
                        "right" => page.navigate_artifact(1, cx),
                        "home" => page.select_artifact_edge(false, cx),
                        "end" => page.select_artifact_edge(true, cx),
                        _ => {}
                    }
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .relative()
                        .overflow_hidden()
                        .child(stage)
                        .child(
                            div()
                                .absolute()
                                .top(px(Theme::TITLEBAR_TOP_PAD))
                                .left(px(16.0))
                                .h(px(Theme::TITLEBAR_HEIGHT))
                                .flex()
                                .items_center()
                                .child(back_button),
                        )
                        .child(
                            div()
                                .absolute()
                                .left(px(16.0))
                                .top_0()
                                .bottom_0()
                                .flex()
                                .items_center()
                                .child(previous),
                        )
                        .child(
                            div()
                                .absolute()
                                .right(px(16.0))
                                .top_0()
                                .bottom_0()
                                .flex()
                                .items_center()
                                .child(next),
                        )
                        .child(
                            div()
                                .id("studio-artifact-filmstrip")
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .h(px(ARTIFACT_FILMSTRIP_HEIGHT))
                                .flex()
                                .justify_center()
                                .occlude()
                                .on_scroll_wheel(cx.listener(|page, event, _, cx| {
                                    page.on_filmstrip_scroll(event, cx)
                                }))
                                .child(
                                    crate::edge_fade::edge_faded(
                                        ARTIFACT_FILMSTRIP_FADE,
                                        false,
                                        false,
                                        div()
                                            .w(px(filmstrip_viewport))
                                            .h_full()
                                            .flex_none()
                                            .overflow_hidden()
                                            .relative()
                                            .children(thumbnails),
                                    )
                                    .fade_left(fade_left)
                                    .fade_right(fade_right),
                                ),
                        ),
                )
                .child(
                    div()
                        .relative()
                        .w(px(INSPECTOR_WIDTH))
                        .h_full()
                        .flex_none()
                        .child(crate::frost::frosted(
                            0.0,
                            crate::frost::MENU_BLUR,
                            inspector,
                        ))
                        .child(
                            crate::scrollbar::overlay("studio-inspector", &self.inspector_scroll)
                                .inset_top(Theme::TITLEBAR_HEIGHT),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_navigation_wraps_for_arrows_but_clamps_for_swipes() {
        assert_eq!(stepped_artifact_index(0, 4, -1, true), 3);
        assert_eq!(stepped_artifact_index(3, 4, 1, true), 0);
        assert_eq!(stepped_artifact_index(0, 4, -1, false), 0);
        assert_eq!(stepped_artifact_index(3, 4, 1, false), 3);
        assert_eq!(stepped_artifact_index(1, 4, 1, false), 2);
    }

    #[test]
    fn lightbox_paging_uses_halfway_or_flick() {
        assert_eq!(
            lightbox_paging_target(-500.0, 0.0, 800.0, true, true),
            -800.0
        );
        assert_eq!(lightbox_paging_target(500.0, 0.0, 800.0, true, true), 800.0);
        assert_eq!(lightbox_paging_target(-200.0, 0.0, 800.0, true, true), 0.0);
        assert_eq!(
            lightbox_paging_target(-80.0, -2000.0, 800.0, true, true),
            -800.0
        );
        assert_eq!(lightbox_paging_target(-500.0, 0.0, 800.0, true, false), 0.0);
        assert_eq!(lightbox_paging_target(500.0, 0.0, 800.0, false, true), 0.0);
    }

    #[test]
    fn lightbox_swipe_tracks_one_to_one_and_rubber_bands_the_end() {
        assert_eq!(
            apply_lightbox_swipe_delta(0.0, -200.0, 800.0, true, true),
            -200.0
        );
        assert_eq!(
            apply_lightbox_swipe_delta(0.0, -900.0, 800.0, true, true),
            -800.0
        );
        let resisted = apply_lightbox_swipe_delta(0.0, -200.0, 800.0, true, false);
        assert!(resisted < 0.0 && resisted > -200.0, "resisted={resisted}");
    }

    #[test]
    fn lightbox_pan_limits_cover_the_zoomed_stage() {
        assert_eq!(lightbox_pan_range(900.0, 1.0), 0.0);
        assert!((lightbox_pan_range(900.0, 2.0) - 900.0).abs() < 0.01);
        assert!((lightbox_pan_range(1400.0, 3.0) - 2100.0).abs() < 0.01);
    }

    #[test]
    fn lightbox_snap_ease_never_overshoots() {
        let from = -240.0;
        let to = -800.0;
        let mut previous = from;
        for i in 0..=22 {
            let elapsed = i as f32 / 100.0;
            let position = snap_offset_at(from, to, elapsed, 0.22);
            assert!(
                position <= previous + 0.01,
                "went backwards or bounced: {previous} -> {position}"
            );
            assert!(
                position <= from + 0.01 && position >= to - 0.01,
                "left the [to, from] interval: {position}"
            );
            previous = position;
        }
        assert!((snap_offset_at(from, to, 0.22, 0.22) - to).abs() < 0.01);
    }

    #[test]
    fn inspector_prompt_inner_width_leaves_room_for_copy() {
        assert!((inspector_prompt_inner_width() - 252.0).abs() < 0.01);
    }

    #[test]
    fn filmstrip_content_width_counts_one_selected_thumb() {
        assert!((filmstrip_content_width(0) - 0.0).abs() < 0.01);
        assert!((filmstrip_content_width(1) - ARTIFACT_FILMSTRIP_SELECTED).abs() < 0.01);
        assert!(
            (filmstrip_content_width(4)
                - (ARTIFACT_FILMSTRIP_SELECTED
                    + 3.0 * (ARTIFACT_FILMSTRIP_THUMB + ARTIFACT_FILMSTRIP_GAP)))
                .abs()
                < 0.01
        );
    }

    #[test]
    fn filmstrip_keeps_the_selected_thumb_centered() {
        let viewport = 800.0;
        for selected in 0..12 {
            let offset = filmstrip_offset(selected, viewport);
            let center = offset + filmstrip_selected_center(selected);
            assert!(
                (center - viewport / 2.0).abs() < 0.01,
                "selected {selected} landed at {center}, expected {}",
                viewport / 2.0
            );
        }
    }

    #[test]
    fn filmstrip_viewport_uses_most_of_the_stage() {
        assert!((filmstrip_viewport_width(1000.0) - 940.0).abs() < 0.01);
        assert!((filmstrip_viewport_width(0.0) - 1128.0).abs() < 0.01);
    }

    #[test]
    fn gallery_thumbs_shrink_a_large_frame() {
        let mut raw = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(1200, 800))
            .write_to(&mut std::io::Cursor::new(&mut raw), image::ImageFormat::Png)
            .unwrap();
        let thumb = downsample_gallery_thumb(raw).unwrap();
        assert!(thumb.bytes.len() < 1200 * 800);
        assert_eq!(thumb.format, ImageFormat::Jpeg);
        let decoded = image::load_from_memory(&thumb.bytes).unwrap();
        assert_eq!(decoded.width(), 960);
        assert_eq!(decoded.height(), GALLERY_THUMB_SHORT_EDGE);
    }

    #[test]
    fn gallery_thumbs_bound_extreme_aspect_ratios() {
        assert_eq!(gallery_thumb_dimensions(4096, 1024), (1920, 480));
        assert_eq!(gallery_thumb_dimensions(800, 600), (800, 600));
    }

    #[test]
    fn filmstrip_visible_range_stays_bounded() {
        let range = filmstrip_visible_range(500, 10_000, 800.0);
        assert!(range.len() < 40, "range={range:?}");
        assert!(range.contains(&500));
        assert_eq!(filmstrip_visible_range(0, 0, 800.0), 0..0);
    }

    fn test_frame(conversation: StudioConversationId, id: StudioArtifactId) -> ArtifactFrame {
        ArtifactFrame {
            id,
            conversation_id: conversation,
            turn_id: StudioTurnId::new(),
            prompt: "prompt".into(),
            model_display_name: "model".into(),
            mime_type: "image/png".into(),
            size_bytes: 1,
        }
    }

    #[test]
    fn viewer_walks_only_the_frames_it_was_given() {
        let conversation = StudioConversationId::new();
        let ids: Vec<_> = (0..4).map(|_| StudioArtifactId::new()).collect();
        let frames: Vec<_> = ids
            .iter()
            .copied()
            .map(|id| test_frame(conversation, id))
            .collect();
        assert_eq!(frames.iter().map(|frame| frame.id).collect::<Vec<_>>(), ids);
        let neighbors = lightbox_neighbor_ids(&frames, ids[1]);
        assert_eq!(neighbors[0], ids[1]);
        assert!(neighbors.contains(&ids[0]));
        assert!(neighbors.contains(&ids[2]));
        assert!(!neighbors.contains(&StudioArtifactId::new()));
    }

    #[test]
    fn conversation_frames_stay_inside_that_conversation() {
        use chrono::Utc;
        use std::collections::BTreeMap;
        let conversation_id = StudioConversationId::new();
        let other = StudioConversationId::new();
        let artifact = zeron_proto::StudioArtifactView {
            id: StudioArtifactId::new(),
            output_position: 0,
            media_kind: MediaKind::Image,
            mime_type: "image/png".into(),
            size_bytes: 1,
            width: Some(1),
            height: Some(1),
            duration_seconds: None,
            metadata: serde_json::Value::Null,
            created_at: Utc::now(),
        };
        let view = StudioConversationView {
            conversation: zeron_proto::StudioConversationSummary {
                id: conversation_id,
                title: "one".into(),
                turn_count: 1,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                archived: false,
                forked_from_turn_id: None,
            },
            turns: vec![zeron_proto::StudioTurnView {
                id: StudioTurnId::new(),
                position: 0,
                prompt: "a fox".into(),
                source_turn_id: None,
                batch_id: zeron_studio::StudioBatchId::new(),
                created_at: Utc::now(),
                runs: vec![zeron_proto::StudioRunView {
                    id: zeron_studio::StudioRunId::new(),
                    position: 0,
                    provider_id: "venice".into(),
                    model: zeron_studio::MediaModel {
                        provider_id: "venice".into(),
                        id: "flux".into(),
                        display_name: "Flux".into(),
                        description: None,
                        operation: zeron_studio::MediaOperation::TextToImage,
                        output_kind: MediaKind::Image,
                        output_mime_types: vec!["image/png".into()],
                        input_constraints: Vec::new(),
                        prompt_maximum_chars: None,
                        negative_prompt_maximum_chars: None,
                        maximum_output_count: 8,
                        controls: Vec::new(),
                        pricing: None,
                        features: Vec::new(),
                        manifest_version: "test".into(),
                        fetched_at: Utc::now(),
                    },
                    controls: BTreeMap::new(),
                    output_count: 1,
                    display_aspect_ratio: (1, 1),
                    state: zeron_proto::StudioRunState::Succeeded,
                    progress: None,
                    error: None,
                    quote: None,
                    artifacts: vec![artifact.clone()],
                }],
            }],
        };
        let frames = frames_from_conversation(&view);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].conversation_id, conversation_id);
        assert_ne!(frames[0].conversation_id, other);
        assert_eq!(frames[0].id, artifact.id);
    }

    #[test]
    fn lightbox_rests_on_a_live_stage_and_pages_only_while_swiping() {
        assert!(!lightbox_uses_paging_slides(false, 1200.0, 0.0, false));
        assert!(lightbox_uses_paging_slides(false, 1200.0, -80.0, false));
        assert!(lightbox_uses_paging_slides(false, 1200.0, 0.0, true));
        assert!(!lightbox_uses_paging_slides(true, 1200.0, -80.0, false));
        assert!(!lightbox_uses_paging_slides(false, 0.0, -80.0, false));
    }

    #[test]
    fn lightbox_stage_tracks_a_resize() {
        assert!(lightbox_stage_size_changed(1200.0, 800.0, 900.0, 800.0));
        assert!(lightbox_stage_size_changed(1200.0, 800.0, 1200.0, 600.0));
        assert!(!lightbox_stage_size_changed(1200.0, 800.0, 1200.2, 800.1));
    }

    #[test]
    fn artifact_file_write_does_not_require_a_tokio_runtime() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let destination = dir.path().join("download.png");

        write_artifact_file(destination.clone(), b"image bytes".to_vec()).expect("artifact write");

        assert_eq!(
            std::fs::read(destination).expect("saved artifact"),
            b"image bytes"
        );
    }
}
